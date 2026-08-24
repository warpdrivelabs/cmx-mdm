//! M4 管家工作台聚合 handler —— summary（两表各状态计数）。
//!
//! 对应路由（`cmx-mdm-api/src/lib.rs`）：
//! - `GET /mdm/workbench/summary` → [`mdm_workbench_summary`]
//!
//! 一次请求返回 md_match_scan（发现项）+ md_merge_record（合并历史）各 status 计数，
//! 供前端 zone 计数展示，替代"全量拉取后前端 filter 计数"。

use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};

use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_store_pg as store;

/// 工作台汇总计数。
///
/// `GET /api/mdm/workbench/summary` —— 一次请求返回 `md_match_scan`（发现项）+
/// `md_merge_record`（合并历史）各 status 计数，供前端 zone 展示。query `?dictCode=` 可选（缺省全表聚合）。
#[utoipa::path(
    get,
    path = "/api/mdm/workbench/summary",
    params(WorkbenchSummaryQuery),
    responses(
        (status = 200, description = "{ dictCode, findings{pending,resolved,ignored}, merges{pending,reviewed,rejected,unmerged} }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_workbench_summary(
    headers: HeaderMap,
    Query(q): Query<WorkbenchSummaryQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let findings = store::count_scan_by_status(mm, &db_id, q.dict_code.as_deref()).await?;
    let merges = store::count_merge_by_status(mm, &db_id, q.dict_code.as_deref()).await?;
    Ok(Json(ApiResp::ok(json!({
        "dictCode": q.dict_code,
        "findings": findings,
        "merges": merges,
    }))))
}

/// summary 查询参数。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct WorkbenchSummaryQuery {
    /// 字典码（可选，限定聚合域；缺省全表）。
    #[serde(default, alias = "dictCode")]
    pub dict_code: Option<String>,
}
