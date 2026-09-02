//! M7.1 审批动作业务封装 handler —— 同意/驳回/退回 + 详情页按钮数据源。
//!
//! 分层（方案 20260818 V2）：前端仅传 `crId + action + comment`；本层做业务校验与封装，
//! 流程调用全部经 [`crate::flow_client`] 回环（不直连引擎）；状态落位保持「回写统一入口」
//! ——approve/reject 办结后**主动同步** `sync_flow_result`（与 webhook 幂等竞争由 try_set
//! 抢占收敛），API 返回时 CR 即为终态。
//!
//! 五步 Service（对齐经典「业务 ↔ 工作流」集成范式）：
//! ① CR 状态校验（approving，防过期/重复审批）
//! ② 业务权限校验（当前用户 ∈ review 任务 assignee∪候选；与引擎侧 T0b 纵深防御）
//! ③ 业务动作：不直接写状态（回写链统一负责）
//! ④ 调工作流（complete/reject，以当前用户身份透传 JWT）
//! ⑤ complete 成功 → 同步回写（approve→抢占 activating→激活器；reject→rejected）

use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};
use cmx_api_types::Error;
use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_store_pg as store;

use crate::flow_client;

use super::flow_cb::current_user_id;

/// 当前请求的用户 Bearer（回环透传作委托令牌；审批动作以本人身份执行）。
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(|t| t.to_string()))
}

/// 定位当前实例 + 未办结的 review 任务（①②共用），并返回当前用户是否可办。
///
/// 返回 `(instance_id, task_id, can_review)`；无可办任务返回 None + 业务错误说明。
async fn locate_review_task(
    cr_id: i64,
    user: &str,
) -> std::result::Result<(String, String, bool), Error> {
    locate_open_task(cr_id, user, "review", "审批任务不存在或已办结").await
}

/// [`locate_review_task`] 的泛化：按节点名关键字（contains）找当前实例里未办结的任务。
/// review 环节传 "review"，退回后的发起人重办环节传 "apply"。
async fn locate_open_task(
    cr_id: i64,
    user: &str,
    node_contains: &str,
    not_found_msg: &str,
) -> std::result::Result<(String, String, bool), Error> {
    // 当前实例 = biz_link 倒序第一条且须 ACTIVE。
    let instances = flow_client::biz_instances(cr_id)
        .await
        .map_err(Error::business_error)?;
    let current = instances
        .iter()
        .find(|i| i.get("state").and_then(|v| v.as_str()) == Some("ACTIVE"))
        .ok_or_else(|| Error::business_error("该单据当前没有进行中的审批流程"))?;
    let instance_id = current
        .get("instanceId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // 目标任务：实例视图里未办结、节点名含关键字的环节（双节点流程 apply/review）。
    let detail = flow_client::instance_detail(&instance_id)
        .await
        .map_err(Error::business_error)?;
    let tasks = detail.get("tasks").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let task = tasks
        .iter()
        .find(|t| {
            let node = t
                .get("nodeBpmnId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_lowercase();
            node.contains(node_contains) && !t.get("completed").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .ok_or_else(|| Error::business_error(not_found_msg))?;
    let task_id = task
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // 权限：assignee==me，或 me 在候选池（claimable 列表含该任务）。
    let assignee = task.get("assignee").and_then(|v| v.as_str()).unwrap_or("");
    let can = if !assignee.is_empty() {
        assignee == user
    } else if user.is_empty() {
        false
    } else {
        flow_client::my_claimable_tasks(user)
            .await
            .map(|ids| ids.contains(&task_id))
            .unwrap_or(false)
    };
    Ok((instance_id, task_id, can))
}

/// 审批动作统一入口：`POST /api/mdm/change-requests/review`。
///
/// body：`{ "crId": i64, "action": "approve" | "reject", "comment": "..." }`
/// 返回：`{ crId, action, status /* activated / rejected */, instanceId }`。
#[utoipa::path(
    post,
    path = "/api/mdm/change-requests/review",
    request_body = Value,
    responses(
        (status = 200, description = "{ crId, action, status, instanceId }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_review(
    headers: HeaderMap,
    Json(body): Json<ReviewBody>,
) -> Result<Json<ApiResp<Value>>> {
    if !matches!(body.action.as_str(), "approve" | "reject") {
        return Err(Error::business_error("action 仅支持 approve / reject"));
    }
    // ① 状态校验。
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    store::check_status(mm, &db_id, None, body.cr_id, "approving").await?;
    // ② 定位任务 + 业务权限校验（错误文案面向业务用户；引擎侧 T0b 为纵深兜底）。
    let user = current_user_id().unwrap_or_default();
    let (instance_id, task_id, can_review) = locate_review_task(body.cr_id, &user).await?;
    if !can_review {
        return Err(Error::business_error(
            "您不是该单据当前环节的审批人（既非办理人也不在候选池）",
        ));
    }
    // ④ 调工作流办结（以审批人身份；comment 进流程意见留痕 + lastDecision 驱动网关）。
    let token = bearer_token(&headers);
    flow_client::complete_review_task(
        &task_id,
        &instance_id,
        &body.action,
        body.comment.as_deref(),
        &user,
        token.as_deref(),
    )
    .await
    .map_err(Error::business_error)?;
    // ⑤ 同步回写（approve→抢占 activating→激活器；reject→rejected）；与 webhook 并发
    //    由 try_set 抢占收敛——此处调用保证 API 返回时状态已终态。
    let _ = super::flow_cb::sync_flow_result_with(mm, &db_id, &instance_id, "completed", None, "").await;
    // 激活失败会回置 approving——approve 分支回写后如实读终态返回。
    let final_status = if body.action == "reject" {
        "rejected".to_string()
    } else {
        read_cr_status(mm, &db_id, body.cr_id).await
    };
    Ok(Json(ApiResp::ok(json!({
        "crId": body.cr_id, "action": body.action, "status": final_status, "instanceId": instance_id,
    }))))
}

/// 退回（打回上节点重办，实例仍 ACTIVE）：`POST /api/mdm/change-requests/return`。
///
/// body：`{ "crId": i64, "reason": "...", "targetBpmnId": "apply" /* 可选，缺省直接前驱 */ }`
/// 退回后任务落回 apply 节点（发起人待办重确认），CR 保持 approving（无状态回写）。
#[utoipa::path(
    post,
    path = "/api/mdm/change-requests/return",
    request_body = Value,
    responses(
        (status = 200, description = "{ crId, status: \"approving\", instanceId }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_return(
    headers: HeaderMap,
    Json(body): Json<ReturnBody>,
) -> Result<Json<ApiResp<Value>>> {
    // ① 状态校验。
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    store::check_status(mm, &db_id, None, body.cr_id, "approving").await?;
    // ② 定位 + 权限。
    let user = current_user_id().unwrap_or_default();
    let (instance_id, task_id, can_review) = locate_review_task(body.cr_id, &user).await?;
    if !can_review {
        return Err(Error::business_error(
            "您不是该单据当前环节的审批人（既非办理人也不在候选池）",
        ));
    }
    // ④ 调工作流退回（fromUser 留痕）。
    let token = bearer_token(&headers);
    flow_client::return_review_task(
        &task_id,
        &instance_id,
        &user,
        body.target_bpmn_id.as_deref(),
        body.reason.as_deref(),
        token.as_deref(),
    )
    .await
    .map_err(Error::business_error)?;
    Ok(Json(ApiResp::ok(json!({
        "crId": body.cr_id, "status": "approving", "instanceId": instance_id,
    }))))
}

/// 退回重办确认：`POST /api/mdm/change-requests/confirm-apply`。
///
/// body：`{ "crId": i64 }`
/// 退回后 apply 任务落回发起人待办（CR 保持 approving）；本端点以发起人身份办结该任务，
/// 流程继续走 review。无状态回写（办结后仍在审批流中，终态由回写链统一收口）。
#[utoipa::path(
    post,
    path = "/api/mdm/change-requests/confirm-apply",
    request_body = Value,
    responses(
        (status = 200, description = "{ crId, status: \"approving\", instanceId }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_confirm_apply(
    headers: HeaderMap,
    Json(body): Json<ConfirmApplyBody>,
) -> Result<Json<ApiResp<Value>>> {
    // ① 状态校验。
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    store::check_status(mm, &db_id, None, body.cr_id, "approving").await?;
    // ② 定位 + 权限（apply 节点：assignee=initiator，候选=initiator）。
    let user = current_user_id().unwrap_or_default();
    let (instance_id, task_id, can_apply) =
        locate_open_task(body.cr_id, &user, "apply", "重办任务不存在或已办结").await?;
    if !can_apply {
        return Err(Error::business_error(
            "您不是该单据当前重办环节的办理人（发起人）",
        ));
    }
    // ④ 办结 apply 任务（幂等：已办结视为成功）。
    let token = bearer_token(&headers);
    flow_client::complete_apply_task(&task_id, &instance_id, &user, token.as_deref())
        .await
        .map_err(Error::business_error)?;
    Ok(Json(ApiResp::ok(json!({
        "crId": body.cr_id, "status": "approving", "instanceId": instance_id,
    }))))
}

/// 详情页流程按钮数据源：`GET /api/mdm/change-requests/review-context?crId=`。
///
/// 返回 `{ crId, instanceId, taskId, canReview, canWithdraw, state }`——
/// cr-form 装载时调用，按 canReview/canWithdraw 渲染审批/撤回按钮组。
#[utoipa::path(
    get,
    path = "/api/mdm/change-requests/review-context",
    params(ReviewContextQuery),
    responses(
        (status = 200, description = "{ crId, instanceId, taskId, canReview, canWithdraw, state }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_review_context(
    headers: HeaderMap,
    Query(q): Query<ReviewContextQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    // 非 approving/activating（终态或草稿）→ 无流程操作。
    let head = match store::check_status_in(mm, &db_id, None, q.cr_id, &["approving", "activating"]).await {
        Ok(h) => h,
        Err(_) => {
            return Ok(Json(ApiResp::ok(json!({
                "crId": q.cr_id, "instanceId": null, "taskId": null,
                "canReview": false, "canWithdraw": false, "state": null,
            }))))
        }
    };
    let user = current_user_id().unwrap_or_default();
    let create_by = match head.get("create_by") {
        Some(Value::String(v)) => v.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let can_withdraw = !user.is_empty() && !create_by.is_empty() && user == create_by;
    // activating（激活中转瞬态）不提供审批操作。
    let doc_status = head
        .get("doc_status")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if doc_status == "activating" {
        return Ok(Json(ApiResp::ok(json!({
            "crId": q.cr_id, "instanceId": null, "taskId": null,
            "canReview": false, "canWithdraw": false, "state": "ACTIVATING",
        }))));
    }
    let (instance_id, task_id, can_review) = locate_review_task(q.cr_id, &user)
        .await
        .unwrap_or_default();
    // 退回后的发起人重办环节（apply 任务开放且当前用户可办）→ 前端渲染「确认并继续」。
    // 退回态下 review 任务已办结、locate_review_task 拿不到实例 → instanceId 用 apply 侧兜底
    // （同一实例；列表详情工作台的轨迹视图靠它组装上下文）。
    let (apply_instance, apply_task_id, can_apply) =
        locate_open_task(q.cr_id, &user, "apply", "重办任务不存在或已办结")
            .await
            .unwrap_or_default();
    let out_instance = if !instance_id.is_empty() { instance_id.clone() } else { apply_instance };
    Ok(Json(ApiResp::ok(json!({
        "crId": q.cr_id,
        "instanceId": if out_instance.is_empty() { Value::Null } else { json!(out_instance) },
        "taskId": if task_id.is_empty() { Value::Null } else { json!(task_id) },
        "canReview": can_review,
        "canApply": can_apply,
        "applyTaskId": if apply_task_id.is_empty() { Value::Null } else { json!(apply_task_id) },
        "canWithdraw": can_withdraw,
        "state": if out_instance.is_empty() { Value::Null } else { json!("ACTIVE") },
    }))))
}

/// 回写后读 CR 终态（review 返回用）。
async fn read_cr_status(mm: &cmx_database_pg::DatabaseManager, db_id: &str, cr_id: i64) -> String {
    store::check_status_in(mm, db_id, None, cr_id, &["approving", "activating", "activated", "rejected", "aborted"])
        .await
        .ok()
        .and_then(|h| {
            h.get("doc_status")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_default()
}

/// review 请求体。
#[derive(serde::Deserialize)]
pub struct ReviewBody {
    /// CR id。
    #[serde(alias = "crId")]
    pub cr_id: i64,
    /// 审批动作：approve（通过）/ reject（驳回）。
    pub action: String,
    /// 审批意见（落流程意见留痕）。
    #[serde(default)]
    pub comment: Option<String>,
}

/// return 请求体。
#[derive(serde::Deserialize)]
pub struct ReturnBody {
    /// CR id。
    #[serde(alias = "crId")]
    pub cr_id: i64,
    /// 退回意见。
    #[serde(default)]
    pub reason: Option<String>,
    /// 退回目标节点 bpmn id（可选，缺省=直接前驱用户任务）。
    #[serde(default, alias = "targetBpmnId")]
    pub target_bpmn_id: Option<String>,
}

/// confirm-apply 请求体。
#[derive(serde::Deserialize)]
pub struct ConfirmApplyBody {
    /// CR id。
    #[serde(alias = "crId")]
    pub cr_id: i64,
}

/// review-context 查询。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ReviewContextQuery {
    /// CR id。
    #[serde(alias = "crId")]
    pub cr_id: i64,
}
