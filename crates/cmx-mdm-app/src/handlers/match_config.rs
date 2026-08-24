//! 查重规则配置 handler。
//!
//! 规则维护内嵌查重界面，无独立管理页。
//!
//! 对应路由（`cmx-mdm-api/src/lib.rs`）：
//! - `GET /mdm/match-configs` → [`mdm_match_configs_list`]
//! - `POST /mdm/match-configs` → [`mdm_match_configs_save`]
//! - `POST /mdm/match-configs/delete` → [`mdm_match_configs_delete`]

use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};

use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_store_pg as store;

/// 列查重规则。
///
/// `GET /api/mdm/match-configs` —— 按 `dictCode` 可选过滤（空则列全部）。
#[utoipa::path(
    get,
    path = "/api/mdm/match-configs",
    params(MatchConfigQuery),
    responses(
        (status = 200, description = "查重规则列表数组", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_match_configs_list(
    headers: HeaderMap,
    Query(q): Query<MatchConfigQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let list = store::list_match_config(mm, &db_id, q.dict_code.as_deref()).await?;
    Ok(Json(ApiResp::ok(json!(list))))
}

/// 保存查重规则。
///
/// `POST /api/mdm/match-configs` —— upsert（id 缺省 / 0 = 新建；非零或 (dictCode, ruleName) 已存在 = 更新）。body：
///
/// ```json
/// { "id": 0, "ruleName": "默认", "dictCode": "supplier", "targetTable": "cm_supplier",
///   "specs": [{ "field": "name", "weight": 100, "kind": "EditDistance" }],
///   "clusterKeys": ["tax_no"], "surviveFields": ["code", "name"], "thresholds": {} }
/// ```
///
/// 返回 `{ id }`。
#[utoipa::path(
    post,
    path = "/api/mdm/match-configs",
    request_body = Value,
    responses(
        (status = 200, description = "{ id }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_match_configs_save(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let id = store::upsert_match_config(mm, &db_id, &body).await?;
    Ok(Json(ApiResp::ok(json!({ "id": id }))))
}

/// 删除查重规则。
///
/// `POST /api/mdm/match-configs/delete` —— 软删（is_active=FALSE）。body：
///
/// ```json
/// { "configId": 1 }
/// ```
///
/// 返回 `{ configId, affected }`。
#[utoipa::path(
    post,
    path = "/api/mdm/match-configs/delete",
    request_body = Value,
    responses(
        (status = 200, description = "{ configId, affected }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_match_configs_delete(
    headers: HeaderMap,
    Json(body): Json<MatchConfigDeleteBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let n = store::delete_match_config(mm, &db_id, body.config_id).await?;
    Ok(Json(ApiResp::ok(
        json!({ "configId": body.config_id, "affected": n }),
    )))
}

/// 查重规则列表查询。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MatchConfigQuery {
    /// 字典代码（可选过滤，空则列全部）。
    #[serde(default, alias = "dictCode")]
    pub dict_code: Option<String>,
}

/// 查重规则删除请求体。
#[derive(serde::Deserialize)]
pub struct MatchConfigDeleteBody {
    /// 待删除的规则 id。
    #[serde(alias = "configId")]
    pub config_id: i64,
}
