//! M5 分发治理端点 handler —— 投递流水 / 统计 / 重发 / 跳过 + pull 游标 + 全量快照。
//!
//! 对应路由（`cmx-mdm-api/src/lib.rs`）：
//! - `POST /mdm/dispatches/query` → [`mdm_dispatches_query`]（多过滤+分页，POST+body 规范）
//! - `GET  /mdm/dispatches/detail` → [`mdm_dispatches_detail`]
//! - `POST /mdm/dispatches/retry` → [`mdm_dispatches_retry`]
//! - `POST /mdm/dispatches/skip` → [`mdm_dispatches_skip`]
//! - `GET  /mdm/dispatches/stats` → [`mdm_dispatches_stats`]（无参数只读）
//! - `POST /mdm/events/ack` → [`mdm_events_ack`]
//! - `GET  /mdm/events/offsets` → [`mdm_events_offsets`]
//! - `POST /mdm/records/snapshot` → [`mdm_records_snapshot`]

use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};

use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_store_pg as store;

/// 投递流水查询（多过滤 + 分页；POST+body，承接 AGENTS.md §四.6 列表规范）。
///
/// body：`{ subscriptionId?, status?, dictCode?, eventId?, timeFrom?, timeTo?, page, pageSize }`。
#[utoipa::path(
    post,
    path = "/api/mdm/dispatches/query",
    request_body = Value,
    responses(
        (status = 200, description = "{ list, total, page, pageSize }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_dispatches_query(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let (list, total) = store::list_dispatches(mm, &db_id, &body).await?;
    let page = body.get("page").and_then(|v| v.as_i64()).unwrap_or(1);
    let page_size = body.get("pageSize").and_then(|v| v.as_i64()).unwrap_or(20);
    Ok(Json(ApiResp::ok(
        json!({ "list": list, "total": total, "page": page, "pageSize": page_size }),
    )))
}

/// 单条投递详情（按 id 取一条，GET 合规场景）。
#[utoipa::path(
    get,
    path = "/api/mdm/dispatches/detail",
    params(DispatchDetailQuery),
    responses((status = 200, description = "投递行 + 事件类型/payload/订阅名", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_dispatches_detail(
    headers: HeaderMap,
    Query(q): Query<DispatchDetailQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let row = store::get_dispatch(mm, &db_id, q.id)
        .await?
        .ok_or_else(|| store::api_err(&format!("投递实例 {} 不存在", q.id)))?;
    Ok(Json(ApiResp::ok(row)))
}

/// 手动重发（ids 列表或订阅+状态批量；dead/failed → pending）。
#[utoipa::path(
    post,
    path = "/api/mdm/dispatches/retry",
    request_body = Value,
    responses((status = 200, description = "{ retried: n }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_dispatches_retry(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let n = store::retry_dispatches(mm, &db_id, &body).await?;
    Ok(Json(ApiResp::ok(json!({ "retried": n }))))
}

/// 人工跳过死信（终态 skipped，放行决策留痕）。
#[utoipa::path(
    post,
    path = "/api/mdm/dispatches/skip",
    request_body = Value,
    responses((status = 200, description = "{ skipped: n }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_dispatches_skip(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let ids: Vec<i64> = body
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return Err(store::api_err("ids 不能为空"));
    }
    let n = store::skip_dispatches(mm, &db_id, &ids).await?;
    Ok(Json(ApiResp::ok(json!({ "skipped": n }))))
}

/// 监控 KPI 统计（无参数全局只读）。
#[utoipa::path(
    get,
    path = "/api/mdm/dispatches/stats",
    responses((status = 200, description = "{ todayTotal, todayOk, backlog, failed, dead, avgLatencyMs, fanoutLag }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_dispatches_stats(
    headers: HeaderMap,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let stats = store::dispatch_stats(mm, &db_id).await?;
    Ok(Json(ApiResp::ok(stats)))
}

/// pull 游标登记（单调递增：仅接受更大 seq）。
///
/// body：`{ "consumerId": "wms", "dictCode": "supplier", "seq": 123 }`。
#[utoipa::path(
    post,
    path = "/api/mdm/events/ack",
    request_body = Value,
    responses((status = 200, description = "{ ok: true }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_events_ack(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let consumer = body.get("consumerId").and_then(|v| v.as_str()).unwrap_or("");
    let dict = body.get("dictCode").and_then(|v| v.as_str()).unwrap_or("");
    let Some(seq) = body.get("seq").and_then(|v| v.as_i64()) else {
        return Err(store::api_err("seq 不能为空"));
    };
    if consumer.trim().is_empty() || dict.trim().is_empty() {
        return Err(store::api_err("consumerId / dictCode 不能为空"));
    }
    store::upsert_consumer_offset(mm, &db_id, consumer, dict, seq).await?;
    Ok(Json(ApiResp::ok(json!({ "ok": true }))))
}

/// pull 消费者游标列表（监控页消费进度表：consumer/dict/acked_seq/lag）。
#[utoipa::path(
    get,
    path = "/api/mdm/events/offsets",
    responses((status = 200, description = "{ list: [{consumerId, dictCode, ackedSeq, ackedAt, lag}] }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_events_offsets(
    headers: HeaderMap,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let list = store::list_consumer_offsets(mm, &db_id).await?;
    Ok(Json(ApiResp::ok(json!({ "list": list }))))
}

/// 全量快照分页拉取（首接 / 对账修复；支持全量与按日期段增量）。
///
/// body：`{ "dictCode": "supplier", "updatedFrom?": "2026-08-01", "updatedTo?": ..., "page", "pageSize" }`
/// —— 不传时间参数 = 全量；updatedFrom/updatedTo（闭区间）按 `cm_*.update_time` 过滤
/// （如"本月有变更"= updatedFrom = 当月 1 号）。表名经 DCT 元数据解析（dict_meta.table_name）。
#[utoipa::path(
    post,
    path = "/api/mdm/records/snapshot",
    request_body = Value,
    responses((status = 200, description = "{ list, total, page, pageSize, maxSeq }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_records_snapshot(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let dict = body.get("dictCode").and_then(|v| v.as_str()).unwrap_or("");
    if dict.trim().is_empty() {
        return Err(store::api_err("dictCode 不能为空"));
    }
    // 表名解析：DCT 元数据（无元数据的字典明确报错，不猜表名）
    let meta = cmx_dct_store_pg::dict_meta(&cmx_dct_store_pg::DctQuery::by_code(dict)).await;
    let table = match meta {
        Ok(m) => m.table_name,
        Err(_) => return Err(store::api_err(&format!("字典 {dict} 的 DCT 元数据不存在"))),
    };

    let mut clauses = vec!["lifecycle_status = 'published'".to_string()];
    let mut params: Vec<cmx_core::model::cell::DataValue> = Vec::new();
    if let Some(v) = body.get("updatedFrom").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
        clauses.push(format!("update_time >= ${}::text::timestamptz", params.len() + 1));
        params.push(cmx_core::model::cell::DataValue::String(v.into()));
    }
    if let Some(v) = body.get("updatedTo").and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
        clauses.push(format!("update_time < (${}::text::timestamptz) + interval '1 day'", params.len() + 1));
        params.push(cmx_core::model::cell::DataValue::String(v.into()));
    }
    let where_sql = clauses.join(" AND ");
    let cnt_sql = format!("SELECT COUNT(*) AS c FROM {table} WHERE {where_sql}");
    let cds = mm
        .query_sql_with_datavalues(&db_id, None, &cnt_sql, params.clone(), "mdm_snapshot_count")
        .await
        .map_err(|e| store::api_err_db(&format!("查 {table} 总数失败: {e}")))?;
    let total = cds
        .rows
        .first()
        .and_then(|r| r.get_by_name_as::<i64>(cds.schema.as_ref(), "c"))
        .unwrap_or(0);
    let ps = body.get("pageSize").and_then(|v| v.as_i64()).filter(|v| *v > 0).unwrap_or(20);
    let pg = body.get("page").and_then(|v| v.as_i64()).filter(|v| *v > 0).unwrap_or(1);
    let n = params.len() as i64;
    params.push(cmx_core::model::cell::DataValue::Int(ps));
    params.push(cmx_core::model::cell::DataValue::Int((pg - 1) * ps));
    let sql = format!(
        "SELECT * FROM {table} WHERE {where_sql} ORDER BY update_time DESC, id DESC LIMIT ${} OFFSET ${}",
        n + 1,
        n + 2
    );
    let ds = mm
        .query_sql_with_datavalues(&db_id, None, &sql, params, "mdm_snapshot_list")
        .await
        .map_err(|e| store::api_err_db(&format!("快照查 {table} 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let list: Vec<Value> = ds.rows.iter().map(|r| r.to_json_value(schema)).collect();

    // maxSeq 提示（消费端可从此处转 events 增量）
    let max_seq = mm
        .query_sql_with_datavalues(
            &db_id,
            None,
            "SELECT COALESCE(MAX(seq), 0) AS m FROM md_event_log WHERE dict_code = $1",
            cmx_core::dv![cmx_core::model::cell::DataValue::String(dict.into())],
            "mdm_snapshot_maxseq",
        )
        .await
        .ok()
        .and_then(|d| {
            d.rows
                .first()
                .and_then(|r| r.get_by_name_as::<i64>(d.schema.as_ref(), "m"))
        })
        .unwrap_or(0);
    Ok(Json(ApiResp::ok(json!({
        "list": list, "total": total, "page": pg, "pageSize": ps, "maxSeq": max_seq,
    }))))
}

/// 投递详情查询（无 path variable）。
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DispatchDetailQuery {
    /// 投递实例 id。
    pub id: i64,
}
