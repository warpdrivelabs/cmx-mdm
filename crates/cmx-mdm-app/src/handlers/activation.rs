//! 激活映射配置 CRUD + 手动激活 handler。
//!
//! 对应路由（`cmx-mdm-api/src/lib.rs`）：
//! - `GET /mdm/activations` → [`mdm_activations_list`]
//! - `POST /mdm/activations` → [`mdm_activations_save`]
//! - `POST /mdm/change-requests/activate` → [`mdm_cr_activate`]

use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};

use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_model::activation::ActivationConfig;
use cmx_mdm_model::codegen::RandomCodeGenerator;
use cmx_mdm_store_pg as store;

/// 列激活映射配置。
///
/// `GET /api/mdm/activations` —— 配置器 UI 用，按 `sourceDocType` / `crType` / `targetDict` 可选过滤，返回全部激活映射。
#[utoipa::path(
    get,
    path = "/api/mdm/activations",
    params(ActivationListQuery),
    responses(
        (status = 200, description = "激活映射列表数组", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_activations_list(
    headers: HeaderMap,
    Query(q): Query<ActivationListQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let list = store::list(
        mm,
        &db_id,
        q.source_doc_type.as_deref(),
        q.cr_type.as_deref(),
        q.target_dict.as_deref(),
    )
    .await?;
    Ok(Json(ApiResp::ok(json!(list))))
}

/// 保存激活映射。
///
/// `POST /api/mdm/activations` —— upsert（按 `activationCode` 唯一），配置器 UI 用。body 为
/// `ActivationConfig` JSON（顶层 snake_case，明细 `lineMappings` 内层 camelCase）：
///
/// ```json
/// { "activation_code": "supplier_create", "source_doc_type": "PO",
///   "cr_type": "create", "target_dict": "supplier", "target_table": "cm_supplier",
///   "header_mapping": { "code": "code", "name": "name" },
///   "line_mappings": [{ "lineType": "bank", "targetDict": "bank_account", "fields": {} }],
///   "code_rule_code": "SUP_SEQ" }
/// ```
///
/// 返回 `{ activationCode }`。
#[utoipa::path(
    post,
    path = "/api/mdm/activations",
    request_body = Value,
    responses(
        (status = 200, description = "{ activationCode }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_activations_save(
    headers: HeaderMap,
    Json(body): Json<ActivationConfig>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let code = store::upsert(mm, &db_id, &body).await?;
    Ok(Json(ApiResp::ok(json!({ "activationCode": code }))))
}

/// 删除激活映射。
///
/// `POST /api/mdm/activations/delete` —— 硬删除（禁用 Path Variable，承接 AGENTS.md §四 第 5 条）。
/// body：
///
/// ```json
/// { "activationCode": "supplier_create" }
/// ```
///
/// 返回 `{ activationCode, affected }`。
#[utoipa::path(
    post,
    path = "/api/mdm/activations/delete",
    request_body = Value,
    responses(
        (status = 200, description = "{ activationCode, affected }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_activations_delete(
    headers: HeaderMap,
    Json(body): Json<ActivationDeleteBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let n = store::delete_by_code(mm, &db_id, &body.activation_code).await?;
    Ok(Json(ApiResp::ok(
        json!({ "activationCode": body.activation_code, "affected": n }),
    )))
}

/// 删除激活映射请求体。
#[derive(serde::Deserialize)]
pub struct ActivationDeleteBody {
    /// 待删除的激活编码（mdm_activation 唯一键）。
    #[serde(alias = "activationCode")]
    pub activation_code: String,
}

/// 手动触发激活（**运维兜底端点**，默认关闭）。
///
/// `POST /api/mdm/change-requests/activate` —— 审批型 CR 兜底入口 / 内部 CR 直接调激活器。
/// M7 起受 `[mdm.flow].manual_override_enabled` 开关保护（默认 403）——webhook 丢失且
/// 懒同步失效时的终极兜底。body `{ crId }`，返回激活后的主数据记录 id：
///
/// ```json
/// { "crId": 123 }
/// ```
#[utoipa::path(
    post,
    path = "/api/mdm/change-requests/activate",
    request_body = Value,
    responses(
        (status = 200, description = "{ recordId }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_cr_activate(
    headers: HeaderMap,
    Json(body): Json<ActivateBody>,
) -> Result<Json<ApiResp<Value>>> {
    super::flow_cb::manual_override_guard()?;
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let operated_by = super::flow_cb::current_actor_id();
    let codegen = RandomCodeGenerator;
    let record_id = store::activate(mm, &db_id, body.cr_id, operated_by, &codegen).await?;
    Ok(Json(ApiResp::ok(json!({ "recordId": record_id }))))
}

/// 激活映射列表查询参数。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ActivationListQuery {
    /// 源单据类型（可选过滤）。
    #[serde(default, alias = "sourceDocType")]
    pub source_doc_type: Option<String>,
    /// CR 类型（可选过滤）。
    #[serde(default, alias = "crType")]
    pub cr_type: Option<String>,
    /// 目标主数据字典码（可选过滤；通用详情页按 targetDict 反查激活映射以发现子表）。
    #[serde(default, alias = "targetDict")]
    pub target_dict: Option<String>,
}

/// 手动激活请求体。
#[derive(serde::Deserialize)]
pub struct ActivateBody {
    /// 待激活的 CR id。
    #[serde(alias = "crId")]
    pub cr_id: i64,
}
