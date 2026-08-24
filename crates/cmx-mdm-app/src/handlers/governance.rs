//! MDM 治理端点 handler —— 审计 / 事件 / 订阅 / 发布。
//!
//! 对应路由（`cmx-mdm-api/src/lib.rs`）：
//! - `GET /mdm/audit` → [`mdm_audit_list`]
//! - `GET /mdm/events` → [`mdm_events_list`]
//! - `GET /mdm/subscriptions` → [`mdm_subscriptions_list`]（M5：过滤 + 统计 + secret 掩码）
//! - `POST /mdm/subscriptions` → [`mdm_subscriptions_save`]（M5：通道配置校验）
//! - `POST /mdm/subscriptions/delete` → [`mdm_subscriptions_delete`]
//! - `POST /mdm/subscriptions/set-active` → [`mdm_subscriptions_set_active`]
//! - `POST /mdm/subscriptions/test` → [`mdm_subscriptions_test`]
//! - `GET /mdm/subscriptions/channels` → [`mdm_subscriptions_channels`]
//! - `POST /mdm/publish` → [`mdm_publish`]（M5 重定义：手动补发）

use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};

use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_store_pg as store;

use super::{default_page, default_page_size};

/// 列审计记录。
///
/// `GET /api/mdm/audit` —— 变更历史 / 版本留痕，按 `dictCode` / `recordId` 可选过滤 + 分页。
#[utoipa::path(
    get,
    path = "/api/mdm/audit",
    params(GovListQuery),
    responses(
        (status = 200, description = "{ list, total, page, pageSize }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_audit_list(
    headers: HeaderMap,
    Query(q): Query<GovListQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let (list, total) = store::list_audit(
        mm,
        &db_id,
        q.dict_code.as_deref(),
        q.record_id,
        q.page,
        q.page_size,
    )
    .await?;
    Ok(Json(ApiResp::ok(
        json!({ "list": list, "total": total, "page": q.page, "pageSize": q.page_size }),
    )))
}

/// 列变更事件。
///
/// `GET /api/mdm/events` —— 事件 delta 查询，`since` 为序列起点（增量拉取）+ 分页；
/// `order=desc` 按 seq 倒序（监控页最新在前），缺省正序保持消费端 delta 契约。
#[utoipa::path(
    get,
    path = "/api/mdm/events",
    params(GovListQuery),
    responses(
        (status = 200, description = "{ list, total, page, pageSize }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_events_list(
    headers: HeaderMap,
    Query(q): Query<GovListQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let (list, total) = store::list_events(
        mm,
        &db_id,
        q.dict_code.as_deref(),
        q.since,
        q.order.as_deref(),
        q.page,
        q.page_size,
    )
    .await?;
    // delta 游标推进提示：本页最大 seq；hasMore 支持翻页判断（M5 消费端约定）
    let next_since = list
        .iter()
        .map(|e| e.get("seq").and_then(|v| v.as_i64()).unwrap_or(0))
        .max()
        .unwrap_or(q.since.unwrap_or(0));
    let has_more = total > q.page * q.page_size;
    Ok(Json(ApiResp::ok(json!({
        "list": list, "total": total, "page": q.page, "pageSize": q.page_size,
        "nextSince": next_since, "hasMore": has_more,
    }))))
}

/// 列订阅配置（M5 增强：targetSys/dictCode/channel/active 过滤 + 近 24h 投递统计 + secret 掩码）。
///
/// `GET /api/mdm/subscriptions` —— 订阅配置分页列表（存量 GET 接口增强，简单标量过滤）。
#[utoipa::path(
    get,
    path = "/api/mdm/subscriptions",
    params(GovListQuery),
    responses(
        (status = 200, description = "{ list, total, page, pageSize }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_subscriptions_list(
    headers: HeaderMap,
    Query(q): Query<GovListQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let qv = json!({
        "targetSys": q.target_sys, "dictCode": q.dict_code, "channel": q.channel,
        "active": q.active, "page": q.page, "pageSize": q.page_size,
    });
    let (mut list, total) = store::list_subscriptions(mm, &db_id, &qv).await?;
    for sub in list.iter_mut() {
        mask_secret(sub);
    }
    Ok(Json(ApiResp::ok(
        json!({ "list": list, "total": total, "page": q.page, "pageSize": q.page_size }),
    )))
}

/// secret 掩码：channel_config.secret 非空时替换为 "***"（回显不泄密；保存时未变更不覆盖）。
fn mask_secret(sub: &mut Value) {
    if let Some(cfg) = sub.get_mut("channel_config").and_then(|c| c.as_object_mut()) {
        if let Some(secret) = cfg.get("secret").and_then(|v| v.as_str()) {
            if !secret.is_empty() && secret != "***" {
                cfg.insert("secret".into(), json!("***"));
            }
        }
    }
}

/// 保存时保留原 secret：前端回传 "***" 表示未变更，回读库内原值覆盖。
async fn keep_secret_if_masked(
    mm: &cmx_database_pg::DatabaseManager,
    db_id: &str,
    body: &mut Value,
) -> Result<()> {
    let masked = body
        .get("channel_config")
        .and_then(|c| c.get("secret"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| s == "***");
    if !masked {
        return Ok(());
    }
    if let Some(id) = body.get("id").and_then(|v| v.as_i64()) {
        if let Some(old) = store::get_subscription(mm, db_id, id).await? {
            if let (Some(target), Some(origin)) = (
                body.get_mut("channel_config").and_then(|c| c.as_object_mut()),
                old.get("channel_config").and_then(|c| c.get("secret")).cloned(),
            ) {
                target.insert("secret".into(), origin);
            }
        }
    }
    Ok(())
}

/// 保存订阅配置。
///
/// `POST /api/mdm/subscriptions` —— upsert 订阅（id 缺省新建，非零更新）。body：
///
/// ```json
/// { "id": 1, "target_sys": "wms", "dict_code": "supplier",
///   "channel": "webhook", "active": true, "filter": {}, "field_map": {} }
/// ```
///
/// 返回 `{ id }`。
#[utoipa::path(
    post,
    path = "/api/mdm/subscriptions",
    request_body = Value,
    responses(
        (status = 200, description = "{ id }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_subscriptions_save(
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    // 基础校验：目标系统/字典必填；channel 默认 webhook
    let target = body.get("target_sys").and_then(|v| v.as_str()).unwrap_or("");
    let dict = body.get("dict_code").and_then(|v| v.as_str()).unwrap_or("");
    if target.trim().is_empty() || dict.trim().is_empty() {
        return Err(store::api_err("target_sys / dict_code 不能为空"));
    }
    let channel = body
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("webhook")
        .to_string();
    // secret 掩码回传先还原（前端未变更则沿用库内原值）
    keep_secret_if_masked(mm, &db_id, &mut body).await?;
    // 通道配置校验（未注册的通道在此拦截；rest_pull 只登记不投递）
    let reg = crate::distribution::registry::ChannelRegistry::global();
    if let Some(ch) = reg.get(&channel) {
        let cfg_val = body.get("channel_config").cloned().unwrap_or(json!({}));
        ch.validate_config(&cfg_val).await.map_err(|e| store::api_err(&e))?;
    } else if channel != "rest_pull" {
        return Err(store::api_err(&format!("通道 {channel} 未注册或未启用")));
    }
    // 创建人（新建时补，字符串口径与列类型 VARCHAR(64) / 门户通知 user_id 一致）
    if body.get("id").and_then(|v| v.as_i64()).is_none() {
        let user = super::flow_cb::current_user_id();
        body["created_by"] = json!(user);
    }
    let id = store::upsert_subscription(mm, &db_id, &body).await?;
    Ok(Json(ApiResp::ok(json!({ "id": id }))))
}

/// 删除订阅（仅停用态可删；dispatch_log 保留审计）。
///
/// `POST /api/mdm/subscriptions/delete` —— body：`{ "id": 1 }`。
#[utoipa::path(
    post,
    path = "/api/mdm/subscriptions/delete",
    request_body = Value,
    responses((status = 200, description = "{ deleted: n }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_subscriptions_delete(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let Some(id) = body.get("id").and_then(|v| v.as_i64()) else {
        return Err(store::api_err("id 不能为空"));
    };
    let sub = store::get_subscription(mm, &db_id, id)
        .await?
        .ok_or_else(|| store::api_err(&format!("订阅 {id} 不存在")))?;
    if sub.get("active").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(store::api_err("订阅启用中，请先停用再删除（投递流水将保留审计）"));
    }
    let n = store::delete_subscription(mm, &db_id, id).await?;
    Ok(Json(ApiResp::ok(json!({ "deleted": n }))))
}

/// 订阅启停。
///
/// `POST /api/mdm/subscriptions/set-active` —— body：`{ "id": 1, "active": false }`。
/// 停用准即时生效（dispatcher 订阅读取每 tick 直查 DB）。
#[utoipa::path(
    post,
    path = "/api/mdm/subscriptions/set-active",
    request_body = Value,
    responses((status = 200, description = "{ updated: n }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_subscriptions_set_active(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let Some(id) = body.get("id").and_then(|v| v.as_i64()) else {
        return Err(store::api_err("id 不能为空"));
    };
    let active = body.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
    let n = store::set_subscription_active(mm, &db_id, id, active).await?;
    if n == 0 {
        return Err(store::api_err(&format!("订阅 {id} 不存在")));
    }
    Ok(Json(ApiResp::ok(json!({ "updated": n }))))
}

/// 通道连通性测试（发送 test 信封，不落事件/投递实例）。
///
/// `POST /api/mdm/subscriptions/test` —— body 二选一：
/// `{ "id": 1 }`（已保存订阅）或 `{ "channel": "webhook", "channel_config": {...} }`（未保存直测）。
#[utoipa::path(
    post,
    path = "/api/mdm/subscriptions/test",
    request_body = Value,
    responses((status = 200, description = "{ ok, detail, latencyMs }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_subscriptions_test(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let (channel_type, config) = if let Some(id) = body.get("id").and_then(|v| v.as_i64()) {
        let sub = store::get_subscription(mm, &db_id, id)
            .await?
            .ok_or_else(|| store::api_err(&format!("订阅 {id} 不存在")))?;
        let ch = sub.get("channel").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut cfg = sub.get("channel_config").cloned().unwrap_or(json!({}));
        if let Some(t) = sub.get("timeout_ms").and_then(|v| v.as_u64()) {
            cfg.as_object_mut().map(|o| o.insert("timeout_ms".into(), json!(t)));
        }
        (ch, cfg)
    } else {
        (
            body.get("channel").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            body.get("channel_config").cloned().unwrap_or(json!({})),
        )
    };
    let reg = crate::distribution::registry::ChannelRegistry::global();
    let Some(ch) = reg.get(&channel_type) else {
        return Err(store::api_err(&format!("通道 {channel_type} 未注册或未启用")));
    };
    if config.get("secret").and_then(|v| v.as_str()) == Some("***") {
        return Err(store::api_err("secret 为掩码值，请重新填写或改用已保存订阅测试"));
    }
    let started = std::time::Instant::now();
    let result = ch.health_check(&config).await;
    let latency = started.elapsed().as_millis() as i64;
    Ok(Json(ApiResp::ok(json!({
        "ok": result.is_ok(),
        "detail": result.err().unwrap_or_else(|| "连接成功".into()),
        "latencyMs": latency,
    }))))
}

/// 已启用通道枚举（前端通道下拉数据源；feature 未开的类型天然不出现）。
///
/// `GET /api/mdm/subscriptions/channels` —— 无参数只读。
#[utoipa::path(
    get,
    path = "/api/mdm/subscriptions/channels",
    responses((status = 200, description = "{ list: [{type,label}] }", body = ApiResp<Value>)),
    tag = "MDM主数据接口"
)]
pub async fn mdm_subscriptions_channels(
) -> Result<Json<ApiResp<Value>>> {
    let reg = crate::distribution::registry::ChannelRegistry::global();
    let mut list: Vec<Value> = reg
        .types()
        .into_iter()
        .map(|t| json!({ "type": t, "label": t }))
        .collect();
    // rest_pull 型订阅（仅登记 pull 消费者监控，不投递）恒可选
    list.push(json!({ "type": "rest_pull", "label": "rest_pull" }));
    Ok(Json(ApiResp::ok(json!({ "list": list }))))
}

/// 手动补发（D-01 桩消灭，方案 §8.4 重定义）。
///
/// `POST /api/mdm/publish` —— 按订阅/字典/seq 范围重建 pending 投递实例（运维补推入口）。body：
///
/// ```json
/// { "subscriptionId": 1, "dictCode": "supplier", "fromSeq": 10, "toSeq": 99, "force": false }
/// ```
///
/// `force=false`：已投递（delivered）的不重发（uk 冲突忽略）；`force=true` 全部重置 pending。
/// 上限 5000 行防误操作全量重推风暴。返回 `{ created: n }`。
#[utoipa::path(
    post,
    path = "/api/mdm/publish",
    request_body = Value,
    responses(
        (status = 200, description = "{ created }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_publish(
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let created = store::publish_rebuild(mm, &db_id, &body).await?;
    tracing::info!(
        target: "cmx_mdm::distribution",
        body = %body, created,
        "手动补发重建投递实例"
    );
    Ok(Json(ApiResp::ok(json!({ "created": created }))))
}

/// 审计 / 事件 / 订阅 列表查询（分页，无 path variable）。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GovListQuery {
    /// 字典代码（可选过滤）。
    #[serde(default, alias = "dictCode")]
    pub dict_code: Option<String>,
    /// 记录 id（审计列表按记录过滤用）。
    #[serde(default, alias = "recordId")]
    pub record_id: Option<i64>,
    /// 事件序列起点（事件 delta 拉取用）。
    #[serde(default)]
    pub since: Option<i64>,
    /// 排序方向（事件列表专用：`desc` 最新在前，监控页用；缺省正序保持 delta 消费契约）。
    #[serde(default)]
    pub order: Option<String>,
    /// 目标系统（订阅列表过滤）。
    #[serde(default, alias = "targetSys")]
    pub target_sys: Option<String>,
    /// 通道（订阅列表过滤）。
    #[serde(default)]
    pub channel: Option<String>,
    /// 启用态（订阅列表过滤）。
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size", alias = "pageSize")]
    pub page_size: i64,
}
