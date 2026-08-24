//! M3.5 全库扫描查重 handler —— 扫描 / 列表 / 详情 / 忽略。
//!
//! 对应路由（`cmx-mdm-api/src/lib.rs`）：
//! - `POST /mdm/match-scan/run` → [`mdm_match_scan_run`]
//! - `GET /mdm/match-scan` → [`mdm_match_scan_list`]
//! - `GET /mdm/match-scan/detail` → [`mdm_match_scan_detail`]
//! - `POST /mdm/match-scan/ignore` → [`mdm_match_scan_ignore`]
//!
//! 与 [`dedup`](super::dedup) 的 [`find_duplicates`](super::dedup::mdm_find_duplicates) 区别：
//! find_duplicates 是锚点查重（必传 recordId，找它的同伙）；match-scan 是全库普查
//! （无 recordId，扫描整个 cm_* 主动发现重复簇，落 md_match_scan 供管家评审）。

use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};

use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_model::match_algo::scan_clusters;
use cmx_mdm_store_pg as store;

use super::SpecDto;
use super::dedup::{load_match_config_defaults, resolve_dict_meta};
use super::{default_page, default_page_size};

/// 取当前请求认证上下文的操作人 id（i64）；无认证 scope / 空 / 非数字 → 0。
/// 复刻原 cmx_api_core::actor::actor_id_i64 语义，改走 cmx-traits context_scope。
fn mdm_operated_by() -> i64 {
    crate::ctx::current_actor_id()
}

/// 全库扫描查重。
///
/// `POST /api/mdm/match-scan/run` —— 管家工作台「发现未知重复」入口，全库普查主动发现重复簇并落库。
/// `targetTable`/`specs`/`clusterKeys`/`surviveFields` 缺失时从 `md_match_config` 按 `dictCode` 回填。
/// body：
///
/// ```json
/// { "dictCode": "supplier", "targetTable": "cm_supplier",
///   "specs": [{ "field": "name", "weight": 100, "kind": "EditDistance" }],
///   "clusterKeys": ["tax_no"], "surviveFields": ["code", "name"], "minScore": 80 }
/// ```
///
/// 返回 `{ newFindings, skipped, pendingTotal }`（相同成员集合的 pending 不重复入库）。
#[utoipa::path(
    post,
    path = "/api/mdm/match-scan/run",
    request_body = Value,
    responses(
        (status = 200, description = "{ newFindings, skipped, pendingTotal }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_match_scan_run(
    headers: HeaderMap,
    Json(mut body): Json<ScanRunBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;

    // 回填：target_table/specs/cluster_keys/survive_fields 任一缺失，从 match_config 读默认
    if (body.target_table.is_empty()
        || body.specs.is_empty()
        || body.cluster_keys.is_empty()
        || body.survive_fields.is_empty())
        && let Some(d) = load_match_config_defaults(mm, &db_id, &body.dict_code).await?
    {
        if body.target_table.is_empty() {
            body.target_table = d.target_table;
        }
        if body.specs.is_empty() {
            body.specs = d.specs;
        }
        if body.cluster_keys.is_empty() {
            body.cluster_keys = d.cluster_keys;
        }
        if body.survive_fields.is_empty() {
            body.survive_fields = d.survive_fields;
        }
    }

    let specs: Vec<_> = body
        .specs
        .iter()
        .map(|s| s.to_match_spec())
        .collect::<Result<Vec<_>>>()?;
    if specs.is_empty() {
        return Err(store::api_err("查重字段（specs）不能为空（body 未传且 match_config 无配置）"));
    }
    if body.target_table.is_empty() {
        return Err(store::api_err("target_table 不能为空（body 未传且 match_config 无配置）"));
    }
    let cluster_keys: Vec<&str> = body.cluster_keys.iter().map(|s| s.as_str()).collect();
    if cluster_keys.is_empty() {
        return Err(store::api_err("cluster_keys 不能为空（body 未传且 match_config 无配置）"));
    }
    let min_score = body.min_score.unwrap_or(80);

    // 装载列 = id ∪ specs 字段 ∪ surviveFields ∪ {update_time}
    let mut col_set: Vec<String> = vec!["id".into(), "update_time".into()];
    for s in &body.specs {
        col_set.push(s.field.clone());
    }
    for f in &body.survive_fields {
        col_set.push(f.clone());
    }
    col_set.sort();
    col_set.dedup();
    let columns: Vec<&str> = col_set.iter().map(|s| s.as_str()).collect();

    // 拉嫌疑记录（DB 内分块下推）+ 扫描聚类
    let suspects = store::load_suspects(mm, &db_id, &body.target_table, &columns, &cluster_keys).await?;
    let clusters = scan_clusters(&suspects, &specs, &cluster_keys, min_score);

    // 转 PreparedCluster（member_ids 抽出，max_score 取簇内最高）
    let prepared: Vec<store::PreparedCluster> = clusters
        .iter()
        .map(|c| {
            let member_ids: Vec<i64> = c.members.iter().map(|m| m.record_id).collect();
            let max_score = c.members.first().map(|m| m.score).unwrap_or(0);
            store::PreparedCluster {
                cluster_key: c.cluster_key.clone(),
                member_ids,
                max_score,
            }
        })
        .collect();

    // 落库（cluster_hash 去重）
    let stats = store::insert_findings(mm, &db_id, &body.dict_code, &prepared).await?;

    // 当前 dictCode 下 pending 总数（供前端展示评审队列规模）
    let (_, pending_total) = store::list_scans(
        mm,
        &db_id,
        Some(&body.dict_code),
        Some("pending"),
        1,
        1,
    )
    .await?;

    Ok(Json(ApiResp::ok(json!({
        "newFindings": stats.inserted,
        "skipped": stats.skipped,
        "pendingTotal": pending_total,
    }))))
}

/// 列扫描发现项。
///
/// `GET /api/mdm/match-scan` —— 管家工作台评审队列，按 `dictCode` / `status` 可选过滤 + 分页
/// （排序：max_score DESC → created_at DESC）。
#[utoipa::path(
    get,
    path = "/api/mdm/match-scan",
    params(ScanListQuery),
    responses(
        (status = 200, description = "{ list, total, page, pageSize }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_match_scan_list(
    headers: HeaderMap,
    Query(q): Query<ScanListQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let (list, total) = store::list_scans(
        mm,
        &db_id,
        q.dict_code.as_deref(),
        q.status.as_deref(),
        q.page,
        q.page_size,
    )
    .await?;
    Ok(Json(ApiResp::ok(json!({
        "list": list,
        "total": total,
        "page": q.page,
        "pageSize": q.page_size,
    }))))
}

/// 取扫描簇详情。
///
/// `GET /api/mdm/match-scan/detail` —— 按 `scanId` 取 scan 记录 + 簇内成员全字段（经 DCT meta
/// 解析头表名 + 列清单，供前端字段对比表）。
#[utoipa::path(
    get,
    path = "/api/mdm/match-scan/detail",
    params(ScanDetailQuery),
    responses(
        (status = 200, description = "{ scan, members }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_match_scan_detail(
    headers: HeaderMap,
    Query(q): Query<ScanDetailQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let scan = store::get_scan(mm, &db_id, q.scan_id)
        .await?
        .ok_or_else(|| store::api_err(&format!("发现项 {} 不存在", q.scan_id)))?;
    // 解析 member_ids，按 DCT meta 拉成员全字段
    let dict_code = scan
        .get("dict_code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let member_ids: Vec<i64> = scan
        .get("member_ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let members = if member_ids.is_empty() {
        Vec::new()
    } else {
        let meta = resolve_dict_meta(&dict_code).await?;
        let head_table = meta.table_name.clone();
        let cols: Vec<String> = meta.column_names().into_iter().map(String::from).collect();
        let cols_ref: Vec<&str> = cols.iter().map(|s| s.as_str()).collect();
        store::load_by_ids(mm, &db_id, None, &head_table, &cols_ref, &member_ids)
            .await?
            .into_iter()
            .map(|r| r.fields)
            .collect()
    };
    Ok(Json(ApiResp::ok(json!({ "scan": scan, "members": members }))))
}

/// 忽略扫描发现项。
///
/// `POST /api/mdm/match-scan/ignore` —— CAS pending→ignored（已 resolved/ignored 的不可再忽略）。
/// body：
///
/// ```json
/// { "scanId": 5 }
/// ```
///
/// 返回 `{ scanId, status: "ignored" }`。
#[utoipa::path(
    post,
    path = "/api/mdm/match-scan/ignore",
    request_body = Value,
    responses(
        (status = 200, description = "{ scanId, status }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_match_scan_ignore(
    headers: HeaderMap,
    Json(body): Json<ScanIgnoreBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let operated_by = mdm_operated_by();
    let txn_ctx = mm.get_transaction_context();
    let guard = txn_ctx
        .begin_with_guard(&db_id)
        .await
        .map_err(|e| store::api_err(&format!("开事务失败: {e}")))?;
    let txn_id = guard.txn_id().to_string();
    let n = store::transition_scan_status(
        mm,
        &db_id,
        Some(&txn_id),
        body.scan_id,
        "pending",
        "ignored",
        operated_by,
    )
    .await?;
    if n == 0 {
        return Err(store::api_err(&format!(
            "发现项 {} 非 pending，不可忽略",
            body.scan_id
        )));
    }
    guard
        .commit()
        .await
        .map_err(|e| store::api_err(&format!("提交失败: {e}")))?;
    Ok(Json(ApiResp::ok(
        json!({ "scanId": body.scan_id, "status": "ignored" }),
    )))
}

/// 扫描请求体（run 端点）。
#[derive(serde::Deserialize)]
pub struct ScanRunBody {
    #[serde(alias = "dictCode")]
    pub dict_code: String,
    /// 目标头物理表（缺失时从 match_config 读）。
    #[serde(default, alias = "targetTable")]
    pub target_table: String,
    /// 比较字段规则（缺失时从 match_config 读）。
    #[serde(default)]
    pub specs: Vec<SpecDto>,
    /// 分块簇键（缺失时从 match_config 读）。
    #[serde(default, alias = "clusterKeys")]
    pub cluster_keys: Vec<String>,
    /// 存活字段（缺失时从 match_config 读；用于成员字段对比展示）。
    #[serde(default, alias = "surviveFields")]
    pub survive_fields: Vec<String>,
    /// 入簇最低分（默认 80 = Review 阈值）。
    #[serde(default, alias = "minScore")]
    pub min_score: Option<u8>,
}

/// 列表查询（分页 + 过滤）。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ScanListQuery {
    #[serde(default, alias = "dictCode")]
    pub dict_code: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size", alias = "pageSize")]
    pub page_size: i64,
}

/// 详情查询。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ScanDetailQuery {
    /// 发现项 id。
    #[serde(alias = "scanId")]
    pub scan_id: i64,
}

/// 忽略请求体。
#[derive(serde::Deserialize)]
pub struct ScanIgnoreBody {
    /// 发现项 id。
    #[serde(alias = "scanId")]
    pub scan_id: i64,
}
