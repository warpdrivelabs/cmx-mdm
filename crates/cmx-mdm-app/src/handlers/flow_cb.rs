//! M7 流程回调与同步 handler —— webhook 接收 + 回写状态机 + 懒同步 + 撤回 + 流程查询。
//!
//! 回写统一入口 [`sync_flow_result`] 的五条防御规则（V4 方案 §D2，覆盖 webhook 的
//! 重复/迟到/错配/幽灵投递）：
//! 1. biz 归属：实例绑定的 bizTable 必须 = cv_mdm_apply（webhook 载荷自带 definitionKey
//!    可先过滤非本模块流程）；
//! 2. 当前实例：biz_link 倒序第一条的 instanceId 必须 == 事件实例（旧实例迟到事件拦截）；
//! 3. completed：lastDecision=='reject' → rejected；否则抢占 activating → 激活器；
//! 4. terminated：载荷 state 须为 TERMINATED（拦 cancel 幂等幽灵事件）且 CR=approving → aborted；
//! 5. 全部写动作走 try_set 抢占（webhook/懒同步/手动三方并发单赢家）。
//!
//! 懒同步（读时自愈）：cr-todo 列表 / cr-form 详情触发 [`lazy_sync_cr`]，核对实例终态与
//! CR 状态的矛盾并就地修复——webhook 丢失的兜底通道。

use axum::Json;
use axum::body::Bytes;
use axum::extract::Query;
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::db_id::resolve_db_id;
use cmx_api_types::{ApiResp, Result};
use cmx_api_types::Error;
use cmx_database_pg::{DatabaseManager, get_default_pg_db_manager};
use cmx_mdm_model::codegen::RandomCodeGenerator;
use cmx_mdm_store_pg as store;

use crate::flow_client::{self, FlowCfg, MDM_BIZ_TABLE, flow_cfg};

use super::cr::CrIdBody;

/// 懒同步自愈窗口（秒）：approving 且无实例超过该时长才回退 draft——submit 的发起往返
/// 只占秒级（timeout 上界 10s），5 分钟足够区分「进行中」与「崩溃残留」。
const LAZY_SYNC_STALE_SECS: i64 = 300;

/// callback 事件预检结果（纯函数可测）。
#[derive(Debug, PartialEq, Eq)]
enum EventAction {
    /// 忽略（非终态事件 / 非本模块流程 / 幽灵 terminated）。
    Ignore,
    /// 处理 instance.completed（规则 3）。
    SyncCompleted,
    /// 处理 instance.terminated（规则 4，载荷 state 已验）。
    SyncTerminated,
}

/// callback 事件分类（V4 五规则的纯判定部分：规则 1 定义过滤 + 规则 4 幽灵拦截）。
fn classify_callback_event(
    kind: &str,
    payload_state: &str,
    definition_key: &str,
    cfg_definition_key: &str,
) -> EventAction {
    // 规则 1（前置）：definitionKey 非空且不匹配 → 忽略（载荷缺定义时放行，进 sync 后再查 biz 兜底）。
    if !definition_key.is_empty() && definition_key != cfg_definition_key {
        return EventAction::Ignore;
    }
    match kind {
        "instance.completed" => EventAction::SyncCompleted,
        // 规则 4：cancel 幂等幽灵事件——对已终态实例 cancel 仍 emit terminated，载荷 state=快照态。
        "instance.terminated" if payload_state == "TERMINATED" => EventAction::SyncTerminated,
        _ => EventAction::Ignore,
    }
}

/// 验证 flow webhook 签名（HTTP 契约两端各自实现——发送方见 cmx-flowengine 的
/// cmx-flow-adapters `webhook.rs`，契约文档 flowengine `docs/usage/08` §8.5）：
/// HMAC-SHA256(body, secret) 常量时间比较；密钥为空 / 头缺失 / 前缀不符均拒绝。
fn verify_signature(secret: &str, body: &[u8], sig_header: Option<&str>) -> bool {
    if secret.is_empty() {
        // 未配置密钥：拒绝接收（签名即凭证，无密钥等于裸奔）。
        return false;
    }
    let Some(raw) = sig_header else { return false };
    let Some(hex_sig) = raw.trim().strip_prefix("sha256=") else {
        return false;
    };
    // verify_slice 比较的是二进制 MAC（常量时间），先做 hex 解码。
    let Ok(expected) = hex::decode(hex_sig.trim()) else {
        return false;
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// webhook 回调端点：`POST /api/mdm/flow/callback`。
///
/// 免用户鉴权路径（`[auth].whitelist` 放行）——签名即凭证。仅处理
/// `instance.completed` / `instance.terminated`，其余事件 debug 日志。
#[utoipa::path(
    post,
    path = "/api/mdm/flow/callback",
    request_body = Value,
    responses(
        (status = 200, description = "已接收（处理失败仅记日志，靠重投递/懒同步兜底）", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_flow_callback(
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ApiResp<Value>>> {
    let cfg = flow_cfg();
    let sig = headers
        .get("x-cmx-flow-signature")
        .and_then(|v| v.to_str().ok());
    if !verify_signature(&cfg.webhook_secret, &body, sig) {
        return Err(Error::business_error("webhook 签名校验失败"));
    }
    let event: Value = serde_json::from_slice(&body)
        .map_err(|e| Error::business_error(format!("webhook 载荷非 JSON: {e}")))?;
    let kind = event.get("event").and_then(|v| v.as_str()).unwrap_or("");
    let instance_id = event
        .get("instanceId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // 载荷自带 definitionKey / state：先做零成本过滤（规则 1 前置 + 规则 4 判据）。
    let definition_key = event
        .get("definitionKey")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let payload_state = event
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    match classify_callback_event(kind, &payload_state, &definition_key, &cfg.definition_key) {
        EventAction::SyncCompleted => {
            if let Err(e) = sync_flow_result(&instance_id, "completed", None, &definition_key).await {
                tracing::error!(instance = %instance_id, error = %e, "流程完成回写失败（等待重投递/懒同步）");
            }
        }
        EventAction::SyncTerminated => {
            if let Err(e) =
                sync_flow_result(&instance_id, "terminated", Some(&payload_state), &definition_key).await
            {
                tracing::error!(instance = %instance_id, error = %e, "流程终止回写失败（等待重投递/懒同步）");
            }
        }
        EventAction::Ignore => {
            tracing::debug!(event = %kind, state = %payload_state, instance = %instance_id, "忽略流程事件");
        }
    }
    Ok(Json(ApiResp::ok(json!({ "received": true }))))
}

/// 回写统一入口（webhook 与懒同步共用；五规则见模块头注释）。
///
/// `payload_state`：terminated 事件的载荷状态（completed 恒为 COMPLETED，传 None）。
/// 返回 Err 仅表示「本次尝试失败」（激活器报错等），幂等语义由 try_set + activate 状态校验保证。
pub async fn sync_flow_result(
    instance_id: &str,
    event: &str,
    payload_state: Option<&str>,
    definition_key_hint: &str,
) -> std::result::Result<(), String> {
    let mm = get_default_pg_db_manager();
    // callback（webhook）无用户头，CR 定位用默认业务库；懒同步路径同样收敛到这里（多库限制 R2）。
    let db_id = resolve_db_id(None).await;
    sync_flow_result_with(mm, &db_id, instance_id, event, payload_state, definition_key_hint).await
}

/// [`sync_flow_result`] 的带上下文版本（懒同步在用户请求作用域内复用，db_id 一致）。
pub async fn sync_flow_result_with(
    mm: &DatabaseManager,
    db_id: &str,
    instance_id: &str,
    event: &str,
    payload_state: Option<&str>,
    definition_key_hint: &str,
) -> std::result::Result<(), String> {
    let cfg = flow_cfg();
    // —— 规则 1：biz 归属 + 定义过滤 ——
    if !definition_key_hint.is_empty() && definition_key_hint != cfg.definition_key {
        tracing::debug!(instance = instance_id, def = %definition_key_hint, "非 MDM 流程事件，忽略");
        return Ok(());
    }
    let links = flow_client::biz_of_instance(instance_id).await?;
    let Some(link) = links
        .iter()
        .find(|l| l.get("bizTable").and_then(|v| v.as_str()) == Some(MDM_BIZ_TABLE))
    else {
        tracing::debug!(instance = instance_id, "实例未绑定 MDM 单据，忽略");
        return Ok(());
    };
    let Some(cr_id) = link
        .get("bizId")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Err(format!("bizId 非法: {:?}", link.get("bizId")));
    };
    // —— 规则 2：当前实例校验（biz_link 倒序第一条）——
    let instances = flow_client::biz_instances(cr_id).await?;
    let is_current = instances
        .first()
        .and_then(|i| i.get("instanceId").and_then(|v| v.as_str()))
        == Some(instance_id);
    if !is_current {
        tracing::info!(instance = instance_id, cr_id, "非当前实例事件（旧轮次迟到/重复），忽略");
        return Ok(());
    }
    match event {
        "completed" => {
            let vars = flow_client::instance_variables(instance_id).await?;
            let decision = vars
                .get("lastDecision")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if decision == "reject" {
                // 规则 3a：驳回 → rejected（仅当 approving）。
                let won = store::try_set_cr_status_pub(mm, db_id, None, cr_id, &["approving"], "rejected")
                    .await
                    .map_err(|e| e.to_string())?;
                if won {
                    tracing::info!(cr_id, instance = instance_id, "流程驳回 → CR rejected");
                }
                Ok(())
            } else {
                // 规则 3b：通过 → 抢占 activating → 激活器（失败回置 approving 可重试）。
                activate_cr(mm, db_id, cr_id, instance_id).await
            }
        }
        "terminated" => {
            // 规则 4：载荷 state==TERMINATED（callback 入口已校验；懒同步路径传入实例终态）。
            if payload_state != Some("TERMINATED") {
                return Ok(());
            }
            let won = store::try_set_cr_status_pub(mm, db_id, None, cr_id, &["approving"], "aborted")
                .await
                .map_err(|e| e.to_string())?;
            if won {
                tracing::info!(cr_id, instance = instance_id, "流程取消 → CR aborted");
            }
            Ok(())
        }
        other => Err(format!("不支持的事件类型: {other}")),
    }
}

/// 抢占 activating 并执行激活器（approve 分支共用；手动 activate 兜底端点同走此处）。
///
/// operated_by=0 表示流程系统触发（webhook/懒同步无登录用户；md_audit 留痕口径）。
async fn activate_cr(
    mm: &DatabaseManager,
    db_id: &str,
    cr_id: i64,
    instance_id: &str,
) -> std::result::Result<(), String> {
    let won = store::try_set_cr_status_pub(mm, db_id, None, cr_id, &["approving"], "activating")
        .await
        .map_err(|e| e.to_string())?;
    if !won {
        // 他人已处理（activated / activating 中 / 已 reject）——幂等跳过。
        tracing::debug!(cr_id, "抢占 activating 失败（他人已处理），跳过激活");
        return Ok(());
    }
    let codegen = RandomCodeGenerator;
    match store::activate(mm, db_id, cr_id, 0, &codegen).await {
        Ok(_) => {
            tracing::info!(cr_id, instance = instance_id, "流程通过 → 激活完成");
            Ok(())
        }
        Err(e) => {
            // 激活七步失败：回置 approving 保持可重试（webhook 重投 / 懒同步 / 手动兜底）。
            let msg = e.to_string();
            let _ = store::set_cr_status_pub(mm, db_id, None, cr_id, "approving").await;
            tracing::error!(cr_id, error = %msg, "激活失败，已回置 approving");
            Err(format!("激活失败: {msg}"))
        }
    }
}

/// 撤回申请：`POST /api/mdm/change-requests/withdraw`。
///
/// 发起人专属：流程 cancel（终止本轮审批）+ CR 回 draft（改后重提发新实例）。
/// 当前实例非 ACTIVE 时先同步流程终态再按 CR 状态拒绝——防对已通过审批的静默丢弃。
#[utoipa::path(
    post,
    path = "/api/mdm/change-requests/withdraw",
    request_body = Value,
    responses(
        (status = 200, description = "{ crId, status: \"draft\" }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_withdraw(
    headers: HeaderMap,
    Json(body): Json<CrIdBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = crate::db_id::resolve_db_id_from_headers(&headers).await;
    let head = store::check_status(mm, &db_id, None, body.cr_id, "approving").await?;
    // 发起人校验：CR create_by == 当前登录用户（兼容数字/字符串序列化形态）。
    let create_by = match head.get("create_by") {
        Some(Value::String(v)) => v.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let current = current_user_id();
    if !create_by.is_empty() && current.as_deref() != Some(create_by.as_str()) {
        return Err(Error::business_error("仅申请人可撤回"));
    }
    // 当前实例非 ACTIVE → 先同步终态（对齐 CR 状态），再按结果拒绝。
    let instances = flow_client::biz_instances(body.cr_id)
        .await
        .map_err(Error::business_error)?;
    let Some(current_inst) = instances.first() else {
        return Err(Error::business_error("无流程实例，无需撤回"));
    };
    let state = current_inst
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let instance_id = current_inst
        .get("instanceId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if state != "ACTIVE" {
        let _ = sync_flow_result_with(
            mm,
            &db_id,
            &instance_id,
            if state == "COMPLETED" { "completed" } else { "terminated" },
            Some(state),
            "",
        )
        .await;
        return Err(Error::business_error("流程已办结/已终止，不可撤回"));
    }
    // cancel（以发起人身份）+ CR 回 draft。terminated webhook 迟到由规则 4（CR 非 approving
    // 跳过）与「先 abort 后 draft 覆盖」双向收敛。
    // 撤回以发起人身份执行（T0b cancel 授权：current_user==initiator）。
    let user_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(|t| t.to_string()));
    flow_client::cancel_instance(&instance_id, "申请人撤回", user_token.as_deref())
        .await
        .map_err(Error::business_error)?;
    store::set_cr_status_pub(mm, &db_id, None, body.cr_id, "draft").await?;
    Ok(Json(ApiResp::ok(json!({ "crId": body.cr_id, "status": "draft" }))))
}

/// 流程状态批量查询 + 懒同步：`GET /api/mdm/change-requests/flow-status?crIds=1,2`。
///
/// 返回 `{ items: [{ crId, instanceId, state, businessKey }] }`；**读时自愈**：发现
/// 「CR=approving/activating 而实例已终态」就地执行 [`sync_flow_result_with`]，
/// 「approving 且无实例超 5 分钟」回退 draft（submit 崩溃残留）。
#[utoipa::path(
    get,
    path = "/api/mdm/change-requests/flow-status",
    params(FlowStatusQuery),
    responses(
        (status = 200, description = "{ items: [{ crId, instanceId, state, businessKey }] }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_flow_status(
    headers: HeaderMap,
    Query(q): Query<FlowStatusQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = crate::db_id::resolve_db_id_from_headers(&headers).await;
    let ids: Vec<i64> = q
        .cr_ids
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    let mut items = Vec::with_capacity(ids.len());
    for cr_id in ids {
        let head = match store::check_status_in(
            mm,
            &db_id,
            None,
            cr_id,
            &["approving", "activating"],
        )
        .await
        {
            Ok(h) => h,
            Err(_) => {
                // 非 approving/activating（终态/草稿）：不查不修，返回空态。
                items.push(json!({ "crId": cr_id, "instanceId": null, "state": null }));
                continue;
            }
        };
        let doc_status = head
            .get("doc_status")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let instances = flow_client::biz_instances(cr_id)
            .await
            .map_err(Error::business_error)?;
        let Some(inst) = instances.first() else {
            // approving 且无实例：5 分钟外 → 崩溃残留回退 draft；5 分钟内 → submit 可能进行中。
            if doc_status == "approving"
                && store::cr_updated_before_pub(mm, &db_id, cr_id, LAZY_SYNC_STALE_SECS)
                    .await?
            {
                let _ = store::try_set_cr_status_pub(
                    mm, &db_id, None, cr_id, &["approving"], "draft",
                )
                .await;
                tracing::info!(cr_id, "懒同步：approving 无实例超时 → 回退 draft");
            }
            items.push(json!({ "crId": cr_id, "instanceId": null, "state": null }));
            continue;
        };
        let state = inst
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let instance_id = inst
            .get("instanceId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // 自愈：实例终态而 CR 仍 approving/activating。
        match state.as_str() {
            "COMPLETED" => {
                let _ = sync_flow_result_with(mm, &db_id, &instance_id, "completed", None, "").await;
            }
            "TERMINATED" => {
                let _ = sync_flow_result_with(mm, &db_id, &instance_id, "terminated", Some("TERMINATED"), "").await;
            }
            _ => {}
        }
        items.push(json!({
            "crId": cr_id,
            "instanceId": instance_id,
            "state": state,
            "businessKey": inst.get("businessKey").cloned().unwrap_or(Value::Null),
        }));
    }
    Ok(Json(ApiResp::ok(json!({ "items": items }))))
}

/// 流程审批历史（cr-form 流程卡数据源）：`GET /api/mdm/change-requests/flow-history?crId=`。
///
/// 聚合该 CR 的全部实例（倒序分段）+ 各实例审批意见。
#[utoipa::path(
    get,
    path = "/api/mdm/change-requests/flow-history",
    params(FlowHistoryQuery),
    responses(
        (status = 200, description = "{ instances: [{ instanceId, state, businessKey, comments }] }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_flow_history(
    Query(q): Query<FlowHistoryQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let instances = flow_client::biz_instances(q.cr_id)
        .await
        .map_err(Error::business_error)?;
    let mut out = Vec::with_capacity(instances.len());
    for inst in &instances {
        let instance_id = inst
            .get("instanceId")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let comments = if instance_id.is_empty() {
            Vec::new()
        } else {
            flow_client::instance_comments(instance_id)
                .await
                .unwrap_or_default()
        };
        out.push(json!({
            "instanceId": instance_id,
            "state": inst.get("state").cloned().unwrap_or(Value::Null),
            "businessKey": inst.get("businessKey").cloned().unwrap_or(Value::Null),
            "createdAt": Value::Null,
            "comments": comments,
        }));
    }
    Ok(Json(ApiResp::ok(json!({ "instances": out }))))
}

/// 手动端点开关拦截（approve/reject/activate 共用）：默认关闭，流程故障/数据修复时开启。
pub fn manual_override_guard() -> Result<()> {
    let cfg: FlowCfg = flow_cfg();
    if cfg.manual_override_enabled {
        Ok(())
    } else {
        Err(Error::business_error(
            "流程审批模式已启用，手动端点已关闭（[mdm.flow].manual_override_enabled）",
        ))
    }
}

// 身份助手统一收口 crate::ctx（task_local 实现）；沿用 super::flow_cb::current_user_id /
// current_actor_id 调用路径，re-export 保持既有引用零改。
pub(crate) use crate::ctx::{current_actor_id, current_user_id};

/// flow-status 查询参数。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FlowStatusQuery {
    /// CR id 列表（逗号分隔）。
    #[serde(alias = "crIds")]
    pub cr_ids: String,
}

/// flow-history 查询参数。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FlowHistoryQuery {
    /// CR id。
    #[serde(alias = "crId")]
    pub cr_id: i64,
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 发送方同款签名（HMAC-SHA256 → `sha256=<hex>`，与 flow 侧 `sign_body` 同式对拍）。
    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn verify_signature_accepts_correct() {
        let body = br#"{"event":"instance.completed"}"#;
        let sig = sign("s3cret", body);
        assert!(verify_signature("s3cret", body, Some(&sig)));
    }

    #[test]
    fn verify_signature_rejects_wrong() {
        let body = br#"{"event":"instance.completed"}"#;
        let sig = sign("WRONG", body);
        assert!(!verify_signature("s3cret", body, Some(&sig)));
    }

    #[test]
    fn verify_signature_rejects_missing_prefix_or_header_or_secret() {
        let body = br#"{}"#;
        // 缺 sha256= 前缀
        assert!(!verify_signature("s", body, Some("deadbeef")));
        // 缺头
        assert!(!verify_signature("s", body, None));
        // 空 secret（未配置 = 拒收）
        assert!(!verify_signature("", body, Some(&sign("x", body))));
    }

    #[test]
    fn classify_covers_all_rules() {
        let cfg = "mdm_cr_approval";
        // 正常 completed / terminated（载荷 state=TERMINATED）→ 处理
        assert_eq!(
            classify_callback_event("instance.completed", "COMPLETED", cfg, cfg),
            EventAction::SyncCompleted
        );
        assert_eq!(
            classify_callback_event("instance.terminated", "TERMINATED", cfg, cfg),
            EventAction::SyncTerminated
        );
        // 规则 4：幽灵 terminated（对已 COMPLETED 实例 cancel 的幂等事件，载荷 state=COMPLETED）→ 忽略
        assert_eq!(
            classify_callback_event("instance.terminated", "COMPLETED", cfg, cfg),
            EventAction::Ignore
        );
        // 规则 1：其它业务流程（pay/expense demo）的事件 → 忽略
        assert_eq!(
            classify_callback_event("instance.completed", "COMPLETED", "pay_review", cfg),
            EventAction::Ignore
        );
        // 非终态事件 → 忽略
        assert_eq!(
            classify_callback_event("task.created", "", cfg, cfg),
            EventAction::Ignore
        );
        // 载荷缺 definitionKey（放行进 sync，由 biz 反查兜底）
        assert_eq!(
            classify_callback_event("instance.completed", "COMPLETED", "", cfg),
            EventAction::SyncCompleted
        );
    }
}
