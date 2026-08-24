//! M5 分发引擎存储层 —— md_dispatch_log（投递实例队列）+ md_dist_watermark（扇出水位）
//! + md_consumer_offset（pull 游标）读写。
//!
//! 与 [`crate::match_store::governance`]（md_audit/md_event_log/md_subscription 分页查询）
//! 对偶：本模块服务分发引擎（fanout / claim / mark / retry）与监控端点（流水 / 统计）。
//!
//! 集群安全：
//! - fanout：watermark 行 `FOR UPDATE` 独占窗口（多节点排队）+ dispatch uk 幂等；
//! - claim：`FOR UPDATE SKIP LOCKED` 抢占 + 严格顺序守卫（同订阅只放行"最小未完成 seq"）。

use cmx_core::dv;
use cmx_core::model::cell::{DataValue, SqlTypeMarker};
use cmx_database_pg::DatabaseManager;
use serde_json::Value;

use crate::error::api_err_db;

/// 扇出水位键（当前仅一种）。
const WM_FANOUT: &str = "fanout";

/// 单轮扇出：水位窗口内读事件 → 匹配订阅 → 幂等插入投递实例 → 推进水位。
///
/// 事务内执行（watermark `FOR UPDATE` 独占窗口，多节点排队；dispatch uk 冲突忽略，
/// 重复扇出无害）。`matches(event, sub)` 为同步纯函数（filter 求值，DB-free）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `batch` - 单轮最大事件数。
/// * `matches` - 事件×订阅匹配函数（返回 true 才生成投递实例）。
///
/// # Returns
///
/// 本轮新生成的投递实例数（uk 冲突不计）。
///
/// # Errors
///
/// 任一 SQL 失败时事务回滚（水位不推进，下一轮重扫，幂等安全）。
pub async fn fanout_tick(
    mm: &DatabaseManager,
    db_id: &str,
    batch: i64,
    matches: &(dyn Fn(&Value, &Value) -> bool + Send + Sync),
) -> Result<u64, cmx_api_types::Error> {
    let txn_ctx = mm.get_transaction_context();
    let guard = txn_ctx
        .begin_with_guard(db_id)
        .await
        .map_err(|e| api_err_db(&format!("扇出开事务失败: {e}")))?;
    let txn_id = guard.txn_id().to_string();

    let inner = async {
        // 1. 水位窗口独占（多节点在此排队，窗口内互斥）
        let wm_sql = "SELECT last_seq FROM md_dist_watermark WHERE key = $1 FOR UPDATE";
        let wm = mm
            .query_sql_with_datavalues(
                db_id,
                Some(&txn_id),
                wm_sql,
                dv![DataValue::String(WM_FANOUT.into())],
                "mdm_wm_lock",
            )
            .await
            .map_err(|e| {
                tracing::error!(target: "cmx_mdm::distribution", error = %e, "锁扇出水位失败（原始）");
                api_err_db(&format!("锁扇出水位失败: {e}"))
            })?;
        let last_seq = wm
            .rows
            .first()
            .and_then(|r| r.get_by_name_as::<i64>(wm.schema.as_ref(), "last_seq"))
            .unwrap_or(0);

        // 2. 读水位之后的事件批
        let ev_sql = "SELECT id, seq, dict_code, record_id, event_type, payload \
                      FROM md_event_log WHERE seq > $1 ORDER BY seq ASC LIMIT $2";
        let ev = mm
            .query_sql_with_datavalues(
                db_id,
                Some(&txn_id),
                ev_sql,
                dv![DataValue::Int(last_seq), DataValue::Int(batch)],
                "mdm_fanout_events",
            )
            .await
            .map_err(|e| {
                tracing::error!(target: "cmx_mdm::distribution", error = %e, "扇出读事件失败（原始）");
                api_err_db(&format!("扇出读事件失败: {e}"))
            })?;
        let schema = ev.schema.as_ref();
        let mut events: Vec<Value> = ev.rows.iter().map(|r| r.to_json_value(schema)).collect();
        for e in events.iter_mut() {
            crate::error::parse_jsonb_field(e, "payload");
        }
        if events.is_empty() {
            return Ok(0_u64);
        }

        // 3. 读 active 推送型订阅（rest_pull 只登记不投递）
        let sub_sql = "SELECT id, dict_code, channel, event_types, filter \
                       FROM md_subscription WHERE active = TRUE AND channel <> 'rest_pull'";
        let subs = mm
            .query_sql_with_datavalues(db_id, Some(&txn_id), sub_sql, vec![], "mdm_fanout_subs")
            .await
            .map_err(|e| {
                tracing::error!(target: "cmx_mdm::distribution", error = %e, "扇出读订阅失败（原始）");
                api_err_db(&format!("扇出读订阅失败: {e}"))
            })?;
        let mut sub_list: Vec<Value> =
            subs.rows.iter().map(|r| r.to_json_value(subs.schema.as_ref())).collect();
        for sub in sub_list.iter_mut() {
            crate::error::parse_jsonb_fields(sub, &["event_types", "filter"]);
        }

        // 4. 匹配 + 幂等插入（分批防参数超限：每批 500 行 × 6 列）
        let mut created: u64 = 0;
        let mut rows: Vec<[DataValue; 6]> = Vec::new();
        for ev in &events {
            for sub in &sub_list {
                if !matches(ev, sub) {
                    continue;
                }
                rows.push([
                    DataValue::Int(cmx_utils::next_pk_id()),
                    DataValue::Int(sub["id"].as_i64().unwrap_or(0)),
                    DataValue::String(ev["id"].as_str().unwrap_or("").into()),
                    DataValue::Int(ev["seq"].as_i64().unwrap_or(0)),
                    DataValue::String(ev["dict_code"].as_str().unwrap_or("").into()),
                    DataValue::Int(ev["record_id"].as_i64().unwrap_or(0)),
                ]);
            }
        }
        for chunk in rows.chunks(500) {
            if chunk.is_empty() {
                continue;
            }
            let mut params: Vec<DataValue> = Vec::with_capacity(chunk.len() * 6);
            let mut values = String::new();
            for (i, row) in chunk.iter().enumerate() {
                if i > 0 {
                    values.push(',');
                }
                let base = (i * 6) as i64;
                values.push_str(&format!(
                    "(${},${},${},${},${},${},'pending')",
                    base + 1,
                    base + 2,
                    base + 3,
                    base + 4,
                    base + 5,
                    base + 6
                ));
                params.extend(row.iter().cloned());
            }
            let sql = format!(
                "INSERT INTO md_dispatch_log (id, subscription_id, event_id, event_seq, dict_code, record_id, status) \
                 VALUES {values} ON CONFLICT (subscription_id, event_id) DO NOTHING"
            );
            let n = mm
                .execute_sql_with_datavalues(db_id, Some(&txn_id), &sql, params)
                .await
                .map_err(|e| {
                    tracing::error!(target: "cmx_mdm::distribution", error = %e, "扇出插投递实例失败（原始）");
                    api_err_db(&format!("扇出插投递实例失败: {e}"))
                })?;
            created += n as u64;
        }

        // 5. 推进水位（无论是否命中订阅——水位表示"已扇出处理"）
        let max_seq = events.last().and_then(|e| e["seq"].as_i64()).unwrap_or(last_seq);
        if max_seq > last_seq {
            mm.execute_sql_with_datavalues(
                db_id,
                Some(&txn_id),
                "UPDATE md_dist_watermark SET last_seq = $1, updated_at = now() WHERE key = $2",
                dv![DataValue::Int(max_seq), DataValue::String(WM_FANOUT.into())]
            )
            .await
            .map_err(|e| {
                tracing::error!(target: "cmx_mdm::distribution", error = %e, "推进扇出水位失败（原始）");
                api_err_db(&format!("推进扇出水位失败: {e}"))
            })?;
        }
        Ok(created)
    }
    .await;

    match inner {
        Ok(n) => {
            guard
                .commit()
                .await
                .map_err(|e| api_err_db(&format!("扇出提交事务失败: {e}")))?;
            Ok(n)
        }
        Err(e) => Err(e), // guard drop 自动回滚
    }
}

/// 回收 running 残留（节点崩溃兜底）：超时未更新的 running 重置 pending 且 attempts+1
/// （attempts 递增防节点反复崩溃形成无限重试循环）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `minutes` - 超时阈值（分钟），running 超过该时长未更新即回收。
///
/// # Returns
///
/// 回收行数。
pub async fn reclaim_running(
    mm: &DatabaseManager,
    db_id: &str,
    minutes: i64,
) -> Result<u64, cmx_api_types::Error> {
    let sql = "UPDATE md_dispatch_log SET status = 'pending', attempts = attempts + 1, \
               updated_at = now() WHERE status = 'running' AND updated_at < now() - ($1 || ' minutes')::interval";
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![DataValue::String(minutes.to_string())]
        )
        .await
        .map_err(|e| {
        tracing::error!(target: "cmx_mdm::distribution", error = %e, "回收 running 残留失败（原始）");
        api_err_db(&format!("回收 running 残留失败: {e}"))
    })?;
    Ok(n as u64)
}

/// 抢占可投递行（`FOR UPDATE SKIP LOCKED` + 严格顺序守卫）并置 running。
///
/// 顺序守卫（方案 §7.6 ②）：某事件存在同订阅更小 seq 的未完成行
/// （pending/running/failed 任一）即阻塞——重试退避等待期间后续事件不超车；
/// delivered/dead/skipped 终态不阻塞（死信人工决策是显式顺序让渡）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `limit` - 单轮抢占上限。
///
/// # Returns
///
/// 抢占到的投递行（含 subscription_id/event_id/event_seq/dict_code/record_id/attempts）。
pub async fn claim_dispatches(
    mm: &DatabaseManager,
    db_id: &str,
    limit: i64,
) -> Result<Vec<Value>, cmx_api_types::Error> {
    let sql = "WITH candidates AS ( \
        SELECT d.id FROM md_dispatch_log d \
        WHERE (d.status = 'pending' OR (d.status = 'failed' AND d.next_retry_at <= now())) \
          AND NOT EXISTS ( \
            SELECT 1 FROM md_dispatch_log d2 \
            WHERE d2.subscription_id = d.subscription_id AND d2.event_seq < d.event_seq \
              AND d2.status IN ('pending','running','failed')) \
        ORDER BY d.subscription_id, d.event_seq \
        FOR UPDATE SKIP LOCKED LIMIT $1) \
      UPDATE md_dispatch_log SET status = 'running', updated_at = now() \
      WHERE id IN (SELECT id FROM candidates) \
      RETURNING id, subscription_id, event_id, event_seq, dict_code, record_id, attempts";
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![DataValue::Int(limit)],
            "mdm_claim_dispatches",
        )
        .await
        .map_err(|e| {
        tracing::error!(target: "cmx_mdm::distribution", error = %e, "抢占投递行失败（原始）");
        api_err_db(&format!("抢占投递行失败: {e}"))
    })?;
    let schema = ds.schema.as_ref();
    Ok(ds.rows.iter().map(|r| r.to_json_value(schema)).collect())
}

/// 落投递结果（单条）。`status` 仅 delivered / failed / dead。
///
/// # Arguments
///
/// * `dispatch_id` - 投递实例 id。
/// * `status` - 终态或待重试态。
/// * `attempts` - 累计尝试次数（引擎按读出值 +1 传入）。
/// * `next_retry_at_epoch` - failed 的下次可抢占时间（unix 秒）；其余状态忽略。
/// * `http_status` - webhook 响应码。
/// * `error` - 错误信息（last_error）。
/// * `snippet` - 响应摘要（response_snippet，截断 512）。
pub async fn mark_dispatch(
    mm: &DatabaseManager,
    db_id: &str,
    dispatch_id: i64,
    status: &str,
    attempts: i64,
    next_retry_at_epoch: Option<i64>,
    http_status: Option<i64>,
    error: Option<&str>,
    snippet: Option<&str>,
) -> Result<(), cmx_api_types::Error> {
    let delivered_at = if status == "delivered" { "now()" } else { "NULL" };
    let retry = if status == "failed" {
        match next_retry_at_epoch {
            Some(secs) => format!("to_timestamp({secs})"),
            None => "NULL".to_string(),
        }
    } else {
        "NULL".to_string()
    };
    let sql = format!(
        "UPDATE md_dispatch_log SET status = $1, attempts = $2, next_retry_at = {retry}, \
         http_status = $3, last_error = $4, response_snippet = $5, delivered_at = {delivered_at}, \
         updated_at = now() WHERE id = $6"
    );
    mm.execute_sql_with_datavalues(
        db_id,
        None,
        &sql,
        dv![
            DataValue::String(status.into()),
            DataValue::Int(attempts),
            http_status.map(DataValue::Int).unwrap_or(DataValue::NullTyped(SqlTypeMarker::Int)),
            error
                .map(|x| DataValue::String(x.into()))
                .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Text)),
            snippet
                .map(|s| DataValue::String(truncate(s, 512)))
                .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Text)),
            DataValue::Int(dispatch_id),
        ]
    )
    .await
    .map_err(|e| api_err_db(&format!("落投递结果失败: {e}")))?;
    Ok(())
}

/// 截断字符串到指定字节上限（CHARACTER_MAXIMUM_LENGTH 防御）。
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

/// 投递流水分页查询（POST body 过滤：subscriptionId/status/dictCode/eventId/时间范围）。
///
/// LEFT JOIN md_subscription 连出订阅名（`sub_name`）/目标系统（`sub_target_sys`）供监控页
/// 直显名称而非数字 id；订阅已删除时为 NULL（LEFT JOIN 不丢投递行）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `q` - 过滤与分页参数（`{subscriptionId?, status?, dictCode?, eventId?, timeFrom?, timeTo?, page, pageSize}`）。
///
/// # Returns
///
/// `(list, total)`，按 created_at DESC；行含 `sub_name` / `sub_target_sys`。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn list_dispatches(
    mm: &DatabaseManager,
    db_id: &str,
    q: &Value,
) -> Result<(Vec<Value>, i64), cmx_api_types::Error> {
    let mut clauses = vec!["1=1".to_string()];
    let mut params: Vec<DataValue> = Vec::new();
    if let Some(v) = q["subscriptionId"].as_i64() {
        clauses.push(format!("d.subscription_id = ${}", params.len() + 1));
        params.push(DataValue::Int(v));
    }
    if let Some(s) = q["status"].as_str().filter(|s| !s.is_empty()) {
        clauses.push(format!("d.status = ${}", params.len() + 1));
        params.push(DataValue::String(s.into()));
    }
    if let Some(s) = q["dictCode"].as_str().filter(|s| !s.is_empty()) {
        clauses.push(format!("d.dict_code = ${}", params.len() + 1));
        params.push(DataValue::String(s.into()));
    }
    if let Some(s) = q["eventId"].as_str().filter(|s| !s.is_empty()) {
        clauses.push(format!("d.event_id = ${}", params.len() + 1));
        params.push(DataValue::String(s.into()));
    }
    if let Some(s) = q["timeFrom"].as_str().filter(|s| !s.is_empty()) {
        clauses.push(format!("d.created_at >= ${}::text::timestamptz", params.len() + 1));
        params.push(DataValue::String(s.into()));
    }
    if let Some(s) = q["timeTo"].as_str().filter(|s| !s.is_empty()) {
        clauses.push(format!("d.created_at <= ${}::text::timestamptz", params.len() + 1));
        params.push(DataValue::String(s.into()));
    }
    query_page(
        mm,
        db_id,
        "md_dispatch_log d LEFT JOIN md_subscription s ON s.id = d.subscription_id",
        "d.*, s.name AS sub_name, s.target_sys AS sub_target_sys",
        &clauses,
        params,
        "d.created_at DESC, d.id DESC",
        q,
    )
    .await
}

/// 通用分页查询（FROM 子句可含 JOIN + SELECT 列清单 + WHERE + ORDER + LIMIT/OFFSET）。
async fn query_page(
    mm: &DatabaseManager,
    db_id: &str,
    from_clause: &str,
    select_cols: &str,
    clauses: &[String],
    params: Vec<DataValue>,
    order: &str,
    q: &Value,
) -> Result<(Vec<Value>, i64), cmx_api_types::Error> {
    let where_sql = clauses.join(" AND ");
    let cnt_sql = format!("SELECT COUNT(*) AS c FROM {from_clause} WHERE {where_sql}");
    let cds = mm
        .query_sql_with_datavalues(db_id, None, &cnt_sql, params.clone(), "mdm_page_count")
        .await
        .map_err(|e| api_err_db(&format!("查 {from_clause} 总数失败: {e}")))?;
    let total = cds
        .rows
        .first()
        .and_then(|r| r.get_by_name_as::<i64>(cds.schema.as_ref(), "c"))
        .unwrap_or(0);
    let ps = q["pageSize"].as_i64().filter(|v| *v > 0).unwrap_or(20);
    let pg = q["page"].as_i64().filter(|v| *v > 0).unwrap_or(1);
    let n = params.len() as i64;
    let mut p2 = params.clone();
    p2.push(DataValue::Int(ps));
    p2.push(DataValue::Int((pg - 1) * ps));
    let sql = format!(
        "SELECT {select_cols} FROM {from_clause} WHERE {where_sql} ORDER BY {order} LIMIT ${} OFFSET ${}",
        n + 1,
        n + 2
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, p2, "mdm_page_list")
        .await
        .map_err(|e| api_err_db(&format!("分页查 {from_clause} 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    Ok((ds.rows.iter().map(|r| r.to_json_value(schema)).collect(), total))
}

/// 取单条投递详情（LEFT JOIN md_event_log 补事件类型/payload、md_subscription 补订阅名）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `id` - 投递实例 id。
///
/// # Returns
///
/// `Some(row)` 行含投递全列 + event_type/event_payload/emitted_at/sub_name/target_sys；不存在返回 `None`。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn get_dispatch(
    mm: &DatabaseManager,
    db_id: &str,
    id: i64,
) -> Result<Option<Value>, cmx_api_types::Error> {
    let sql = "SELECT d.*, e.event_type, e.payload AS event_payload, e.emitted_at, s.name AS sub_name, s.target_sys \
               FROM md_dispatch_log d \
               LEFT JOIN md_event_log e ON e.id = d.event_id \
               LEFT JOIN md_subscription s ON s.id = d.subscription_id \
               WHERE d.id = $1";
    let ds = mm
        .query_sql_with_datavalues(db_id, None, sql, dv![DataValue::Int(id)], "mdm_dispatch_get")
        .await
        .map_err(|e| api_err_db(&format!("查投递详情失败: {e}")))?;
    Ok(ds
        .rows
        .first()
        .map(|r| r.to_json_value(ds.schema.as_ref())))
}

/// 手动重发：按 id 列表（或订阅+状态批量）将 failed/dead 重置 pending。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `q` - `{ids:[...]}` 或 `{subscriptionId?, status:"failed"|"dead"}`（批量）。
///
/// # Returns
///
/// 重置行数。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn retry_dispatches(
    mm: &DatabaseManager,
    db_id: &str,
    q: &Value,
) -> Result<u64, cmx_api_types::Error> {
    let ids: Vec<i64> = q["ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();
    let sql = if !ids.is_empty() {
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("${i}")).collect();
        format!(
            "UPDATE md_dispatch_log SET status = 'pending', next_retry_at = NULL, updated_at = now() \
             WHERE id IN ({}) AND status IN ('failed','dead')",
            placeholders.join(",")
        )
    } else {
        let sub = q.get("subscriptionId").and_then(|v| v.as_i64());
        let status = q["status"].as_str().unwrap_or("dead");
        let mut clauses = vec!["status = $1".to_string()];
        let mut params = vec![DataValue::String(status.into())];
        if let Some(s) = sub {
            clauses.push(format!("subscription_id = ${}", params.len() + 1));
            params.push(DataValue::Int(s));
        }
        format!(
            "UPDATE md_dispatch_log SET status = 'pending', next_retry_at = NULL, updated_at = now() \
             WHERE {}",
            clauses.join(" AND ")
        )
    };
    let params: Vec<DataValue> = if !ids.is_empty() {
        ids.into_iter().map(DataValue::Int).collect()
    } else if let Some(s) = q.get("subscriptionId").and_then(|v| v.as_i64()) {
        vec![DataValue::String(q["status"].as_str().unwrap_or("dead").into()), DataValue::Int(s)]
    } else {
        vec![DataValue::String(q["status"].as_str().unwrap_or("dead").into())]
    };
    let n = mm
        .execute_sql_with_datavalues(db_id, None, &sql, params)
        .await
        .map_err(|e| api_err_db(&format!("重发投递失败: {e}")))?;
    Ok(n as u64)
}

/// 人工跳过死信（终态 skipped，放行决策留痕；仅 dead 态可跳过）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `ids` - 投递实例 id 列表。
///
/// # Returns
///
/// 跳过行数。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn skip_dispatches(
    mm: &DatabaseManager,
    db_id: &str,
    ids: &[i64],
) -> Result<u64, cmx_api_types::Error> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "UPDATE md_dispatch_log SET status = 'skipped', next_retry_at = NULL, updated_at = now() \
         WHERE id IN ({}) AND status = 'dead'",
        placeholders.join(",")
    );
    let params: Vec<DataValue> = ids.iter().cloned().map(DataValue::Int).collect();
    let n = mm
        .execute_sql_with_datavalues(db_id, None, &sql, params)
        .await
        .map_err(|e| api_err_db(&format!("跳过死信失败: {e}")))?;
    Ok(n as u64)
}

/// 监控 KPI 统计（今日投递 / 成功率 / 平均耗时 / 积压 / 死信 / 扇出滞后）。
///
/// 今日窗口按 `updated_at >= current_date`（参数化规避查询通道对无参聚合 SQL 的
/// 兼容问题）；扇出滞后 = md_event_log 最大 seq - watermark。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
///
/// # Returns
///
/// 单行统计 JSON：`{today_total, today_ok, backlog, failed, dead, avg_latency_ms, fanout_lag}`；无数据时各计数为 0。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn dispatch_stats(mm: &DatabaseManager, db_id: &str) -> Result<Value, cmx_api_types::Error> {
    // 注：FILTER 聚合子句在本库查询通道下报 "column updated_at does not exist"
    // （psql 直跑同款正常——查询层与 FILTER 组合的兼容问题），统一改写 SUM(CASE WHEN)。
    let sql = "SELECT \
        SUM(CASE WHEN updated_at >= current_date THEN 1 ELSE 0 END)::bigint AS today_total, \
        SUM(CASE WHEN updated_at >= current_date AND status = 'delivered' THEN 1 ELSE 0 END)::bigint AS today_ok, \
        SUM(CASE WHEN status IN ('pending','running') THEN 1 ELSE 0 END)::bigint AS backlog, \
        SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END)::bigint AS failed, \
        SUM(CASE WHEN status = 'dead' THEN 1 ELSE 0 END)::bigint AS dead, \
        COALESCE(AVG(CASE WHEN status = 'delivered' AND delivered_at > now() - interval '24 hours' \
          THEN EXTRACT(EPOCH FROM (delivered_at - created_at)) * 1000 END), 0) AS avg_latency_ms, \
        GREATEST(COALESCE((SELECT MAX(seq) FROM md_event_log), 0) \
          - COALESCE((SELECT last_seq FROM md_dist_watermark WHERE key = 'fanout'), 0), 0) AS fanout_lag \
      FROM md_dispatch_log WHERE $1::int = 1";
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            None,
            sql,
            cmx_core::dv![cmx_core::model::cell::DataValue::Int(1)],
            "mdm_dispatch_stats",
        )
        .await
        .map_err(|e| api_err_db(&format!("统计投递失败: {e}")))?;
    Ok(ds
        .rows
        .first()
        .map(|r| r.to_json_value(ds.schema.as_ref()))
        .unwrap_or(Value::Null))
}

/// pull 游标登记（单调递增：仅接受比当前更大的 seq）。
///
/// 先按 `(consumer_id, dict_code)` 单调 UPDATE；0 行再 INSERT（`ON CONFLICT DO NOTHING`
/// 容忍并发首插，唯一赢家生效）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `consumer_id` - 下游消费者标识（建议 = target_sys）。
/// * `dict_code` - 字典代码。
/// * `seq` - 已确认消费到的 seq。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn upsert_consumer_offset(
    mm: &DatabaseManager,
    db_id: &str,
    consumer_id: &str,
    dict_code: &str,
    seq: i64,
) -> Result<(), cmx_api_types::Error> {
    // 先按 (consumer_id, dict_code) 单调更新；0 行再插入（uk 冲突容忍——并发首插唯一赢家）
    let upd = "UPDATE md_consumer_offset SET acked_seq = $1, acked_at = now() \
               WHERE consumer_id = $2 AND dict_code = $3 AND acked_seq < $1";
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            None,
            upd,
            dv![
                DataValue::Int(seq),
                DataValue::String(consumer_id.into()),
                DataValue::String(dict_code.into()),
            ]
        )
        .await
        .map_err(|e| api_err_db(&format!("更新消费游标失败: {e}")))?;
    if n > 0 {
        return Ok(());
    }
    let ins = "INSERT INTO md_consumer_offset (id, consumer_id, dict_code, acked_seq) \
               VALUES ($1, $2, $3, $4) ON CONFLICT (consumer_id, dict_code) DO NOTHING";
    mm.execute_sql_with_datavalues(
        db_id,
        None,
        ins,
        dv![
            DataValue::Int(cmx_utils::next_pk_id()),
            DataValue::String(consumer_id.into()),
            DataValue::String(dict_code.into()),
            DataValue::Int(seq),
        ]
    )
    .await
    .map_err(|e| api_err_db(&format!("登记消费游标失败: {e}")))?;
    Ok(())
}

/// pull 游标列表（带 lag = 同字典全局 max(seq) - acked_seq，监控页消费进度表数据源）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
///
/// # Returns
///
/// 游标行数组（consumer_id/dict_code/acked_seq/acked_at/lag），按消费者与字典排序。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn list_consumer_offsets(
    mm: &DatabaseManager,
    db_id: &str,
) -> Result<Vec<Value>, cmx_api_types::Error> {
    let sql = "SELECT o.consumer_id, o.dict_code, o.acked_seq, o.acked_at, \
               GREATEST(COALESCE((SELECT MAX(e.seq) FROM md_event_log e WHERE e.dict_code = o.dict_code), 0) - o.acked_seq, 0) AS lag \
               FROM md_consumer_offset o ORDER BY o.consumer_id, o.dict_code";
    let ds = mm
        .query_sql_with_datavalues(db_id, None, sql, vec![], "mdm_offset_list")
        .await
        .map_err(|e| api_err_db(&format!("查消费游标失败: {e}")))?;
    let schema = ds.schema.as_ref();
    Ok(ds.rows.iter().map(|r| r.to_json_value(schema)).collect())
}

/// 手动补发（/mdm/publish 重定义）：按订阅/字典/seq 范围重建投递实例。
///
/// `force=true` 时已 delivered 的行也重置 pending；否则 uk 冲突忽略（已投递不重发）。
/// 先 SELECT 匹配组合，应用层逐行生成 snowflake id 后批量幂等插入（与 fanout 同构，
/// 避免 INSERT..SELECT 行内无法安全生成唯一 snowflake 的问题）。
///
/// # Arguments
///
/// * `q` - body：`{subscriptionId?, dictCode?, fromSeq?, toSeq?, force?}`。
///
/// # Returns
///
/// 新建/重置的行数。
pub async fn publish_rebuild(
    mm: &DatabaseManager,
    db_id: &str,
    q: &Value,
) -> Result<u64, cmx_api_types::Error> {
    let mut sub_clause = String::new();
    let mut params: Vec<DataValue> = Vec::new();
    if let Some(s) = q["subscriptionId"].as_i64() {
        params.push(DataValue::Int(s));
        sub_clause = format!(" AND s.id = ${}", params.len());
    }
    if let Some(s) = q["dictCode"].as_str().filter(|s| !s.is_empty()) {
        params.push(DataValue::String(s.into()));
        sub_clause.push_str(&format!(" AND s.dict_code = ${}", params.len()));
    }
    let mut ev_clause = String::new();
    if let Some(v) = q["fromSeq"].as_i64() {
        params.push(DataValue::Int(v));
        ev_clause.push_str(&format!(" AND e.seq >= ${}", params.len()));
    }
    if let Some(v) = q["toSeq"].as_i64() {
        params.push(DataValue::Int(v));
        ev_clause.push_str(&format!(" AND e.seq <= ${}", params.len()));
    }
    // 1. 选出匹配的事件×订阅组合（上限 5000 防误操作全量重推风暴）
    let sel = format!(
        "SELECT s.id AS sub_id, e.id AS event_id, e.seq, e.dict_code, e.record_id \
         FROM md_event_log e JOIN md_subscription s ON s.dict_code = e.dict_code \
         WHERE s.channel <> 'rest_pull'{sub_clause}{ev_clause} \
         ORDER BY e.seq ASC LIMIT 5000"
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sel, params, "mdm_publish_select")
        .await
        .map_err(|e| api_err_db(&format!("补发选事件失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let combos: Vec<Value> = ds.rows.iter().map(|r| r.to_json_value(schema)).collect();
    if combos.is_empty() {
        return Ok(0);
    }
    // 2. 应用层逐行生成 id + 批量幂等插入（分批 500 行防参数超限）
    let conflict_action = if q["force"].as_bool().unwrap_or(false) {
        "DO UPDATE SET status = 'pending', next_retry_at = NULL, updated_at = now()"
    } else {
        "DO NOTHING"
    };
    let mut affected: u64 = 0;
    for chunk in combos.chunks(500) {
        let mut params: Vec<DataValue> = Vec::with_capacity(chunk.len() * 6);
        let mut values = String::new();
        for (i, c) in chunk.iter().enumerate() {
            if i > 0 {
                values.push(',');
            }
            let base = (i * 6) as i64;
            values.push_str(&format!(
                "(${},${},${},${},${},${},'pending')",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6
            ));
            params.extend([
                DataValue::Int(cmx_utils::next_pk_id()),
                DataValue::Int(c["sub_id"].as_i64().unwrap_or(0)),
                DataValue::String(c["event_id"].as_str().unwrap_or("").into()),
                DataValue::Int(c["seq"].as_i64().unwrap_or(0)),
                DataValue::String(c["dict_code"].as_str().unwrap_or("").into()),
                DataValue::Int(c["record_id"].as_i64().unwrap_or(0)),
            ]);
        }
        let sql = format!(
            "INSERT INTO md_dispatch_log (id, subscription_id, event_id, event_seq, dict_code, record_id, status) \
             VALUES {values} ON CONFLICT (subscription_id, event_id) {conflict_action}"
        );
        let n = mm
            .execute_sql_with_datavalues(db_id, None, &sql, params)
            .await
            .map_err(|e| api_err_db(&format!("补发插投递实例失败: {e}")))?;
        affected += n as u64;
    }
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate("abcdef", 3), "abc");
        // 中文字节边界：'中' 是 3 字节，max=4 应回退到 3
        assert_eq!(truncate("中国", 4), "中");
        assert_eq!(truncate("短", 10), "短");
    }
}

/// 批量取订阅配置（投递时按 claim 结果反查；含通道/过滤/重试策略全列）。
///
/// JSONB 列（filter/field_map/channel_config/event_types）已 parse 还原为对象/数组。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `ids` - 订阅 id 列表（空列表直接返回空数组）。
///
/// # Returns
///
/// 订阅配置行数组。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn load_subscriptions_by_ids(
    mm: &DatabaseManager,
    db_id: &str,
    ids: &[i64],
) -> Result<Vec<Value>, cmx_api_types::Error> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "SELECT * FROM md_subscription WHERE id IN ({})",
        placeholders.join(",")
    );
    let params: Vec<DataValue> = ids.iter().cloned().map(DataValue::Int).collect();
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_subs_by_ids")
        .await
        .map_err(|e| api_err_db(&format!("批量取订阅失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut list: Vec<Value> = ds.rows.iter().map(|r| r.to_json_value(schema)).collect();
    for sub in list.iter_mut() {
        crate::error::parse_jsonb_fields(sub, &["filter", "field_map", "channel_config", "event_types"]);
    }
    Ok(list)
}

/// 批量取事件详情（投递时组装信封：payload 快照 + emitted_at）。
///
/// payload（JSONB）已 parse 还原为对象。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `ids` - 事件 id 列表（空列表直接返回空数组）。
///
/// # Returns
///
/// 事件行数组（id/seq/dict_code/record_id/event_type/payload/emitted_at）。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn load_events_by_ids(
    mm: &DatabaseManager,
    db_id: &str,
    ids: &[String],
) -> Result<Vec<Value>, cmx_api_types::Error> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "SELECT id, seq, dict_code, record_id, event_type, payload, emitted_at FROM md_event_log WHERE id IN ({})",
        placeholders.join(",")
    );
    let params: Vec<DataValue> = ids.iter().map(|i| DataValue::String(i.clone().into())).collect();
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_events_by_ids")
        .await
        .map_err(|e| api_err_db(&format!("批量取事件失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut list: Vec<Value> = ds.rows.iter().map(|r| r.to_json_value(schema)).collect();
    for e in list.iter_mut() {
        crate::error::parse_jsonb_field(e, "payload");
    }
    Ok(list)
}
