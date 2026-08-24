//! M2 CR 变更请求 handler —— 审批流转 / 列表 / 详情。
//!
//! 新建 CR 走平台标准 `/doc/save`，本模块仅覆盖审批流转与查询。
//!
//! 对应路由（`cmx-mdm-api/src/lib.rs`）：
//! - `POST /mdm/change-requests/submit` → [`mdm_cr_submit`]
//! - `POST /mdm/change-requests/abort` → [`mdm_cr_abort`]
//! （M7.1 决议：approve/reject 旧端点已删除，审批走 review.rs 的业务封装端点）
//! - `GET /mdm/change-requests` → [`mdm_cr_list`]
//! - `GET /mdm/change-requests/detail` → [`mdm_cr_detail`]

use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};

use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_store_pg as store;

use super::{default_page, default_page_size};

/// 提交变更请求（M7 流程版六步：抢占 → 防孤儿 → 发起 → 信封判据 → 代确认 apply）。
///
/// `POST /api/mdm/change-requests/submit` —— draft / rejected → approving（驳回后可直接编辑
/// 重新提交，无需 clone 新 CR），并同步发起流程实例。body：
///
/// ```json
/// { "crId": 123 }
/// ```
///
/// 步骤（V4 方案 D1）：
/// 1. **抢占**：条件 UPDATE（draft/rejected→approving，同语句刷 update_time 作懒同步
///    自愈窗口起点），0 行 = 并发冲突 → 409；
/// 2. 读 CR 头（doc_no / create_by / 业务字段）；
/// 3. **防孤儿实例**：存在 ACTIVE 实例 → 回滚抢占 → 409（不自动 cancel，运维台处理）；
/// 4. 回环发起 `/api/flow/instances`（bizLink 绑 cv_mdm_apply）；
/// 5. **信封判据**：HTTP 2xx 且 code==0 才算成功（flow 业务错误 = 200+code=1）；
///    失败回滚 CR 状态 → 502（msg 带 flow 明细，如「流程定义未发布」）；
/// 6. **代确认 apply 节点**（以发起人身份 complete），失败不回滚（任务留发起人待办，
///    手动 complete 即继续——优雅降级）。
///
/// 返回 `{ crId, status: "approving", instanceId }`。
#[utoipa::path(
    post,
    path = "/api/mdm/change-requests/submit",
    request_body = Value,
    responses(
        (status = 200, description = "{ crId, status, instanceId }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_submit(
    headers: HeaderMap,
    Json(body): Json<CrIdBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    // 1) 抢占（0 行 = 双击/双端并发，他人已处理）。
    let won = store::try_set_cr_status_pub(mm, &db_id, None, body.cr_id, &["draft", "rejected"], "approving")
        .await?;
    if !won {
        return Err(cmx_api_types::Error::business_error(
            "单据状态已变更，请刷新后重试",
        ));
    }
    // 2) 读 CR 头（抢占成功后再读，避免读旧态）。
    let head = store::check_status(mm, &db_id, None, body.cr_id, "approving").await?;
    // 3) 防孤儿实例：存在 ACTIVE 旧实例（仅崩溃窗口会产生）则拒绝，回滚抢占。
    let instances = crate::flow_client::biz_instances(body.cr_id).await;
    match instances {
        Ok(list) if list.iter().any(|i| i.get("state").and_then(|v| v.as_str()) == Some("ACTIVE")) => {
            let _ = store::set_cr_status_pub(mm, &db_id, None, body.cr_id, "draft").await;
            return Err(cmx_api_types::Error::business_error(
                "存在未终结的流程实例，请联系管理员处理（运维台取消后重提）",
            ));
        }
        Ok(_) => {}
        Err(e) => {
            // 查询失败按「无实例」处理——发起步骤自身还有一道校验兜底。
            tracing::warn!(cr_id = body.cr_id, error = %e, "防孤儿实例查询失败（继续提交）");
        }
    }
    // 4)+5) 发起实例（信封判据；失败回滚抢占）。
    // 用户 Bearer 透传（委托令牌）：回环调 flow 以发起人身份执行（T0b 授权要求
    // current_user==任务 assignee/initiator）。current_original_token() 在平台上下文
    // 不保证有值，故直接从本请求头取。
    let user_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(|t| t.to_string()));
    let started =
        crate::flow_client::start_instance(&head, body.cr_id, user_token.as_deref()).await;
    let view = match started {
        Ok(v) => v,
        Err(e) => {
            // 回滚失败仅日志（cr_id 便于人工修复）；接口返回 502 用户可直接重试。
            if let Err(re) =
                store::set_cr_status_pub(mm, &db_id, None, body.cr_id, "draft").await
            {
                tracing::error!(cr_id = body.cr_id, error = %re, "发起失败后回滚 CR 状态失败（人工介入）");
            }
            return Err(cmx_api_types::Error::business_error(format!("发起审批流程失败: {e}")));
        }
    };
    let instance_id = view
        .get("id")
        .or_else(|| view.get("instanceId"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // 6) 代确认 apply 节点（以发起人身份 complete；失败不回滚——任务留待办，手动确认即继续）。
    let initiator = match head.get("create_by") {
        Some(serde_json::Value::String(v)) => v.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    if let Some(task_id) = find_apply_task(&view) {
        if let Err(e) = crate::flow_client::complete_apply_task(
            &task_id,
            &instance_id,
            &initiator,
            user_token.as_deref(),
        )
        .await
        {
            tracing::error!(cr_id = body.cr_id, task = %task_id, error = %e,
                "代确认 apply 节点失败（发起人可在待办手动确认）");
        }
    } else {
        tracing::warn!(cr_id = body.cr_id, "发起响应未找到 apply 任务（跳过代确认）");
    }
    Ok(Json(ApiResp::ok(json!({
        "crId": body.cr_id, "status": "approving", "instanceId": instance_id,
    }))))
}

/// 从发起响应的实例视图里找未办结的 apply 节点任务 id（node 含 "apply" 且未完成）。
fn find_apply_task(view: &Value) -> Option<String> {
    let tasks = view.get("tasks").and_then(|v| v.as_array())?;
    tasks
        .iter()
        .filter(|t| {
            let node = t.get("nodeBpmnId").or_else(|| t.get("node")).and_then(|v| v.as_str()).unwrap_or("");
            node.to_lowercase().contains("apply")
                && !t.get("completed").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .find_map(|t| t.get("id").and_then(|v| v.as_str()).map(String::from))
}

/// 作废变更请求。
///
/// `POST /api/mdm/change-requests/abort` —— draft → aborted。body `{ crId }`：
///
/// ```json
/// { "crId": 123 }
/// ```
///
/// 返回 `{ crId, status: "aborted" }`。
#[utoipa::path(
    post,
    path = "/api/mdm/change-requests/abort",
    request_body = Value,
    responses(
        (status = 200, description = "{ crId, status }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_abort(
    headers: HeaderMap,
    Json(body): Json<CrIdBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    store::abort_cr(mm, &db_id, body.cr_id).await?;
    Ok(Json(ApiResp::ok(json!({ "crId": body.cr_id, "status": "aborted" }))))
}

/// 列变更请求。
///
/// `GET /api/mdm/change-requests` —— 按 `docStatus` / `docType` / `keyword`（单据号·主体名模糊）可选过滤 + 分页，返回全部业务字段。
#[utoipa::path(
    get,
    path = "/api/mdm/change-requests",
    params(CrListQuery),
    responses(
        (status = 200, description = "{ list, total, page, pageSize }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_list(
    headers: HeaderMap,
    Query(q): Query<CrListQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let (list, total) = store::list_cr(
        mm, &db_id, q.doc_status.as_deref(), q.doc_type.as_deref(), q.keyword.as_deref(), q.page,
        q.page_size,
    )
    .await?;
    Ok(Json(ApiResp::ok(json!({
        "list": list, "total": total, "page": q.page, "pageSize": q.page_size,
    }))))
}

/// 取变更请求详情。
///
/// `GET /api/mdm/change-requests/detail` —— 按 `crId` 取 CR 头 + 行。
#[utoipa::path(
    get,
    path = "/api/mdm/change-requests/detail",
    params(CrDetailQuery),
    responses(
        (status = 200, description = "CR 头 + 行详情", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_detail(
    headers: HeaderMap,
    Query(q): Query<CrDetailQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let detail = store::get_cr_detail(mm, &db_id, q.cr_id).await?;
    Ok(Json(ApiResp::ok(detail)))
}

/// 通用 CR id body（submit/approve/reject/abort 复用）。
#[derive(serde::Deserialize)]
pub struct CrIdBody {
    /// CR id。
    #[serde(alias = "crId")]
    pub cr_id: i64,
}

/// CR 列表查询（分页）。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CrListQuery {
    /// 单据状态过滤（可选）。
    #[serde(default, alias = "docStatus")]
    pub doc_status: Option<String>,
    /// 单据类型过滤（可选；= 激活映射 source_doc_type，如 gys/kh）。
    #[serde(default, alias = "docType")]
    pub doc_type: Option<String>,
    /// 关键字过滤（可选；单据号 / 主体名模糊匹配）。
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size", alias = "pageSize")]
    pub page_size: i64,
}

/// CR 详情查询。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CrDetailQuery {
    /// CR id。
    #[serde(alias = "crId")]
    pub cr_id: i64,
}
