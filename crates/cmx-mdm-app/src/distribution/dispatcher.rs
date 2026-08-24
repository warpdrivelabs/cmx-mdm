//! Dispatcher 常驻循环 —— 扇出 / 回收 / 抢占 / 投递 / 落结果 / 告警。
//!
//! 每 tick（默认 2s，`GlobalEventBus("mdm.event.appended")` 进程内即时唤醒，跨节点靠 tick 兜底）：
//! ① fanout：水位窗口内读事件 → 匹配订阅 → 幂等插 md_dispatch_log(pending)；
//! ② reclaim：回收超时 running 残留（attempts+1 防崩溃循环）；
//! ③ claim：SKIP LOCKED + 严格顺序守卫抢占（同订阅只放行最小未完成 seq，方案 §7.6）；
//! ④ deliver：按订阅分组、组内按 event_seq 串行调通道（保序），跨组并发（semaphore）；
//! ⑤ mark：delivered / failed（指数退避 next_retry_at）/ dead（重试耗尽或不可重试）；
//! ⑥ notify：dead / 连续失败 → 门户通知（订阅创建人 + admin）。
//!
//! 目标库：默认业务库（`get_biz_db_id`，与 flow_cb 回调同模式——多库路由为已知边界 D-09 同源）。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};

use cmx_mdm_model::distribution::EventEnvelope;
use cmx_mdm_store_pg as store;

use super::registry::ChannelRegistry;
use super::transform;
use super::DistCfg;

/// 拉起常驻循环（由 `start_distribution` spawn；循环内吞错误只记日志，永不退出）。
///
/// 触发：`scan_interval_ms` 定时 tick + `GlobalEventBus("mdm.event.appended")` 进程内
/// 即时唤醒（跨节点靠 tick 兜底，最终一致）。
///
/// # Arguments
///
/// * `cfg` - 引擎配置快照（周期/批量/退避参数）。
pub async fn run(cfg: DistCfg) {
    // 进程内即时唤醒：订阅 GlobalEventBus 的激活事件通知（同步闭包 → Notify）
    let notify = Arc::new(tokio::sync::Notify::new());
    if cmx_traits::event_bus::GlobalEventBus::is_initialized() {
        let n = notify.clone();
        cmx_traits::event_bus::GlobalEventBus::get()
            .subscribe("mdm.event.appended", Arc::new(move |_topic, _payload| {
                n.notify_one();
            }))
            .await;
    }

    let mut interval =
        tokio::time::interval(std::time::Duration::from_millis(cfg.scan_interval_ms.max(200)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = notify.notified() => {}
        }
        if let Err(e) = tick(&cfg).await {
            tracing::warn!(target: "cmx_mdm::distribution", error = %e, "分发 tick 失败（下一轮重试）");
        }
    }
}

/// 单轮：扇出 → 回收 → 抢占 → 投递。
///
/// 任一步失败记日志后由外层循环下一轮重试（幂等安全）。
async fn tick(cfg: &DistCfg) -> Result<(), cmx_api_types::Error> {
    let mm = cmx_database_pg::get_default_pg_db_manager();
    let db_id = resolve_biz_db_id().await;

    // ① 扇出（订阅匹配纯函数闭包）
    let created = store::fanout_tick(mm, &db_id, cfg.fanout_batch, &|ev, sub| {
        transform::event_matches_sub(ev, sub)
    })
    .await?;
    if created > 0 {
        tracing::info!(target: "cmx_mdm::distribution", created, "扇出生成投递实例");
    }

    // ② 回收超时 running
    let reclaimed = store::reclaim_running(mm, &db_id, cfg.running_reclaim_minutes).await?;
    if reclaimed > 0 {
        tracing::warn!(target: "cmx_mdm::distribution", reclaimed, "回收超时 running 残留");
    }

    // ③ 抢占 + ④ 投递
    deliver_round(mm, &db_id, cfg.clone()).await?;
    Ok(())
}

/// 一轮投递：抢占 → 组装信封 → 分组串行投递 → 落结果。
async fn deliver_round(
    mm: &'static cmx_database_pg::DatabaseManager,
    db_id: &str,
    cfg: DistCfg,
) -> Result<(), cmx_api_types::Error> {
    let claimed = store::claim_dispatches(mm, db_id, cfg.deliver_batch).await?;
    if claimed.is_empty() {
        return Ok(());
    }

    // 反查事件详情（快照/时间）与订阅配置（通道/重试策略）
    let event_ids: Vec<String> = claimed
        .iter()
        .filter_map(|d| d["event_id"].as_str().map(|s| s.to_string()))
        .collect();
    let sub_ids: Vec<i64> = claimed
        .iter()
        .filter_map(|d| d["subscription_id"].as_i64())
        .collect();
    let (events, subs) = tokio::join!(
        store::load_events_by_ids(mm, db_id, &event_ids),
        store::load_subscriptions_by_ids(mm, db_id, &sub_ids),
    );
    let events: Arc<HashMap<String, Value>> = Arc::new(
        events?
            .into_iter()
            .map(|e| {
                let id = e["id"].as_str().unwrap_or("").to_string();
                (id, e)
            })
            .collect(),
    );
    let subs: HashMap<i64, Value> = subs?
        .into_iter()
        .map(|s| {
            let id = s["id"].as_i64().unwrap_or(0);
            (id, s)
        })
        .collect();

    // 按订阅分组（组内已按 event_seq 升序——claim 的 ORDER BY 保证）；owned 值供 spawn
    let mut groups: HashMap<i64, Vec<Value>> = HashMap::new();
    for d in claimed {
        if let Some(sid) = d["subscription_id"].as_i64() {
            groups.entry(sid).or_default().push(d);
        }
    }

    // 跨订阅并发（semaphore 上限），组内串行（保序）
    let sem = Arc::new(tokio::sync::Semaphore::new(cfg.deliver_concurrency.max(1)));
    let mut handles = Vec::with_capacity(groups.len());
    for (sid, rows) in groups {
        let Some(sub) = subs.get(&sid).cloned() else {
            // 订阅被删（active=false 才可删，此处防御）：行直接落 failed 提示
            for d in rows {
                mark_simple(mm, db_id, &d, "failed", None, "订阅已不存在").await;
            }
            continue;
        };
        let permit = sem.clone().acquire_owned().await;
        let db_id = db_id.to_string();
        let cfg = cfg.clone();
        let events = events.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            // 全局单例 &'static Arc<DatabaseManager>（spawn 需 'static）
            let mm = cmx_database_pg::get_default_pg_db_manager();
            deliver_group(mm, &db_id, &cfg, rows, &sub, &events).await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

/// 单订阅组投递：组内按 event_seq 串行（严格保序），逐条落结果。
async fn deliver_group(
    mm: &'static cmx_database_pg::DatabaseManager,
    db_id: &str,
    cfg: &DistCfg,
    rows: Vec<Value>,
    sub: &Value,
    events: &HashMap<String, Value>,
) {
    let sid = sub["id"].as_i64().unwrap_or(0);
    let channel_type = sub["channel"].as_str().unwrap_or("");
    let Some(channel) = ChannelRegistry::global().get(channel_type) else {
        for d in rows {
            mark_simple(mm, db_id, &d, "failed", None, "通道未注册/未启用").await;
        }
        return;
    };
    let retry_max = sub["retry_max"].as_i64().unwrap_or(8);
    let batch_size = sub["batch_size"].as_i64().unwrap_or(50).max(1) as usize;
    let timeout_ms = sub["timeout_ms"].as_u64();
    let field_map = sub.get("field_map").cloned().unwrap_or(Value::Null);
    let mut channel_config = sub.get("channel_config").cloned().unwrap_or(json!({}));
    if let Some(t) = timeout_ms {
        channel_config
            .as_object_mut()
            .map(|o| o.insert("timeout_ms".into(), json!(t)));
    }

    // 组内分小批（batch_size）逐批调 deliver；批内顺序 = event_seq 顺序
    for chunk in rows.chunks(batch_size) {
        let envelopes: Vec<EventEnvelope> = chunk
            .iter()
            .filter_map(|d| {
                let eid = d["event_id"].as_str()?;
                events.get(eid).map(|ev| {
                    let snapshot = ev
                        .get("payload")
                        .and_then(|p| p.get("snapshot"))
                        .cloned()
                        .unwrap_or_else(|| ev.get("payload").cloned().unwrap_or(Value::Null));
                    let data = transform::apply_field_map(&snapshot, &field_map);
                    let meta = json!({
                        "crId": ev.get("payload").and_then(|p| p.get("cr_id")).cloned().unwrap_or(Value::Null),
                        "dispatchId": d["id"].clone(),
                    });
                    transform::build_envelope(ev, data, meta)
                })
            })
            .collect();
        let results = channel.deliver(&channel_config, &envelopes).await;
        let mut by_event: HashMap<String, &cmx_mdm_model::distribution::DeliveryResult> =
            HashMap::with_capacity(results.len());
        for r in &results {
            by_event.insert(r.event_id.clone(), r);
        }
        for d in chunk {
            let eid = d["event_id"].as_str().unwrap_or("");
            let attempts = d["attempts"].as_i64().unwrap_or(0) + 1;
            match by_event.get(eid) {
                Some(r) if r.ok => {
                    let _ = store::mark_dispatch(
                        mm,
                        db_id,
                        d["id"].as_i64().unwrap_or(0),
                        "delivered",
                        attempts,
                        None,
                        r.http_status,
                        None,
                        r.detail.as_deref(),
                    )
                    .await;
                }
                Some(r) => {
                    let dead = !r.retryable || attempts >= retry_max;
                    let status = if dead { "dead" } else { "failed" };
                    let next_retry = if dead {
                        None
                    } else {
                        Some(backoff_epoch(cfg, attempts))
                    };
                    let err = r.detail.as_deref().unwrap_or("未知错误");
                    let _ = store::mark_dispatch(
                        mm,
                        db_id,
                        d["id"].as_i64().unwrap_or(0),
                        status,
                        attempts,
                        next_retry,
                        r.http_status,
                        Some(err),
                        r.detail.as_deref(),
                    )
                    .await;
                    if dead {
                        tracing::error!(
                            target: "cmx_mdm::distribution",
                            subscription = sid, event = eid, attempts,
                            error = err, "投递进入死信"
                        );
                        notify_dead(sub, eid, err).await;
                    }
                }
                None => {
                    // 事件详情缺失（事件被清理？）——不可重试落 dead
                    mark_simple(mm, db_id, d, "dead", None, "事件详情缺失（可能已被归档清理）").await;
                }
            }
        }
    }
}

/// 指数退避：base * 2^(attempts-1)，封顶 max（attempts 从 1 起算——首发失败退避 base）。
///
/// # Arguments
///
/// * `cfg` - 退避参数来源。
/// * `attempts` - 即将计入的尝试次数（本次失败后累计值）。
///
/// # Returns
///
/// 下次可抢占的 unix 秒。
fn backoff_epoch(cfg: &DistCfg, attempts: i64) -> i64 {
    let exp = (attempts.max(1) - 1) as u32;
    let base = cfg.backoff_base_ms as f64;
    let delay = (base * 2_f64.powi(exp as i32)).min(cfg.backoff_max_ms as f64) as u64;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 + (delay / 1000) as i64 + 1)
        .unwrap_or(0)
}

/// 简易落结果（无退避；failed 带 next_retry 由调用方语义决定——此处默认 60s 后可重试）。
async fn mark_simple(
    mm: &cmx_database_pg::DatabaseManager,
    db_id: &str,
    d: &Value,
    status: &str,
    http_status: Option<i64>,
    err: &str,
) {
    let attempts = d["attempts"].as_i64().unwrap_or(0) + 1;
    let next = if status == "failed" {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|x| x.as_secs() as i64 + 60)
            .ok()
    } else {
        None
    };
    let _ = store::mark_dispatch(
        mm,
        db_id,
        d["id"].as_i64().unwrap_or(0),
        status,
        attempts,
        next,
        http_status,
        Some(err),
        None,
    )
    .await;
}

/// 死信通知：门户通知订阅创建人 + admin（不阻塞主循环；失败仅记日志）。
async fn notify_dead(sub: &Value, event_id: &str, err: &str) {
    // GlobalEventBus 广播（进程内消费者：审计/扩展钩子）
    if cmx_traits::event_bus::GlobalEventBus::is_initialized() {
        cmx_traits::event_bus::GlobalEventBus::get()
            .publish(
                "mdm.dispatch.dead",
                json!({
                    "subscriptionId": sub["id"],
                    "targetSys": sub["target_sys"],
                    "dictCode": sub["dict_code"],
                    "eventId": event_id,
                    "error": err,
                }),
            )
            .await;
    }
    // 门户通知：创建人 + admin（通知中心 center=message，level=error）
    let title = format!(
        "主数据分发死信：{} → {}",
        sub["dict_code"].as_str().unwrap_or("?"),
        sub["target_sys"].as_str().unwrap_or("?")
    );
    let body = format!("事件 {event_id} 投递耗尽重试进入死信：{err}");
    let mut targets: Vec<String> = vec!["admin".into()];
    if let Some(creator) = sub["created_by"].as_str().filter(|s| !s.is_empty()) {
        targets.push(creator.to_string());
    }
    for uid in targets {
        let input = cmx_portal::notify::store::NotifyInput {
            user_id: Some(uid.clone()),
            center: "message".into(),
            title: title.clone(),
            body: Some(body.clone()),
            level: Some("error".into()),
            link: None,
        };
        if let Err(e) = cmx_portal::notify::store::publish(input).await {
            tracing::warn!(target: "cmx_mdm::distribution", user = %uid, error = %e, "死信门户通知发送失败");
        }
    }
}

/// 解析默认业务库 id（与 flow_cb 回调同模式；多库路由为已知边界，方案 §十五 Q&D-09 同源）。
async fn resolve_biz_db_id() -> String {
    cmx_database_pg::get_default_pg_db_manager()
        .get_biz_db_id()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_exponential_and_capped() {
        let cfg = DistCfg {
            backoff_base_ms: 5_000,
            backoff_max_ms: 1_800_000,
            ..DistCfg::default()
        };
        assert_eq!(backoff_epoch(&cfg, 1), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 + 5 + 1);
        // 封顶：attempts 足够大时不再增长
        let a = backoff_epoch(&cfg, 20);
        let b = backoff_epoch(&cfg, 30);
        assert!((a as i64 - b as i64).abs() <= 2, "退避应封顶 30min（a={a} b={b}）");
    }
}
