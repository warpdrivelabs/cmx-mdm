//! M3 合并请求 handler —— 确认 / 详情 / 驳回 / 还原。
//!
//! 对应路由（`cmx-mdm-api/src/lib.rs`）：
//! - `GET /mdm/merge-requests` → [`mdm_merge_requests_list`]
//! - `POST /mdm/merge-requests` → [`mdm_merge_requests_create`]
//! - `GET /mdm/merge-requests/detail` → [`mdm_merge_request_detail`]
//! - `POST /mdm/merge-requests/reject` → [`mdm_merge_request_reject`]
//! - `POST /mdm/merge-requests/undo` → [`mdm_merge_requests_undo`]

use std::collections::HashMap;

use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};

use cmx_database_pg::{get_default_pg_db_manager, DatabaseManager};
use cmx_mdm_model::survivorship::SurvivorRule;
use cmx_mdm_store_pg as store;

use super::dedup::{line_tables, load_match_config_defaults, resolve_dict_meta};
use super::{default_page, default_page_size};

/// 取当前请求认证上下文的操作人 id（i64）；无认证 scope / 空 / 非数字 → 0。
/// 复刻原 cmx_api_core::actor::actor_id_i64 语义，改走 cmx-traits context_scope。
fn mdm_operated_by() -> i64 {
    crate::ctx::current_actor_id()
}

/// 列合并请求。
///
/// `GET /api/mdm/merge-requests` —— 分页查询合并请求，默认排除 pending。`kw` 名称搜索需配合 `dictCode`。
#[utoipa::path(
    get,
    path = "/api/mdm/merge-requests",
    params(MergeListQuery),
    responses(
        (status = 200, description = "{ list, total, page, pageSize }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_merge_requests_list(
    headers: HeaderMap,
    Query(q): Query<MergeListQuery>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    // excludePending 默认 true（"1"/"true"）；显式传 false 关闭
    let exclude_pending = !matches!(
        q.exclude_pending.as_deref(),
        Some("0") | Some("false") | Some("False")
    );
    let exclude_statuses: Option<&[&str]> = if exclude_pending {
        Some(&["pending"])
    } else {
        None
    };
    // 名称搜索（D-05）：kw 非空且选了 dict 时，先在 cm_*.name ILIKE 查命中 id，
    // 再交给 store 过滤 master_id/member_ids。kw 无 dict（"全部字典"，无法解析目标表）
    // 或 dict 未注册时忽略 kw，按原条件列出。
    let mut name_match_ids: Option<Vec<i64>> = None;
    if let Some(kw) = q.kw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(dict_code) = q.dict_code.as_deref() {
            if let Ok(meta) = resolve_dict_meta(dict_code).await {
                let ids =
                    store::find_ids_by_name_like(mm, &db_id, &meta.table_name, kw).await?;
                if ids.is_empty() {
                    // 名称无命中：直接返回空，避免 store 收到空命中集语义歧义
                    return Ok(Json(ApiResp::ok(json!({
                        "list": Vec::<Value>::new(),
                        "total": 0,
                        "page": q.page, "pageSize": q.page_size,
                    }))));
                }
                name_match_ids = Some(ids);
            }
        }
    }
    let (list, total) = store::list_match_groups(
        mm,
        &db_id,
        q.dict_code.as_deref(),
        q.status.as_deref(),
        exclude_statuses,
        name_match_ids.as_deref(),
        q.page,
        q.page_size,
    )
    .await?;

    // 回填可读名称：按 group 的 master_id / member_ids 联查目标表 name/code
    let list = enrich_group_names(mm, &db_id, list).await;

    Ok(Json(ApiResp::ok(json!({
        "list": list, "total": total, "page": q.page, "pageSize": q.page_size,
    }))))
}

/// 回填每条 match_group 的 master/member 可读名称。
///
/// `member_ids` 是 JSONB（DB 返回转义字符串），parse 后联查目标表。
async fn enrich_group_names(
    mm: &DatabaseManager,
    db_id: &str,
    mut groups: Vec<Value>,
) -> Vec<Value> {
    // 按 dict_code 分组批量查（每字典一次 load_by_ids）
    let mut by_dict: HashMap<String, Vec<i64>> = HashMap::new();
    for g in &groups {
        let dict_code = g
            .get("dict_code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if dict_code.is_empty() {
            continue;
        }
        let master_id = g.get("master_id").and_then(|v| v.as_i64()).unwrap_or(0);
        let members_raw = g.get("member_ids").cloned().unwrap_or(Value::Null);
        let members = match members_raw {
            Value::String(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::Null),
            v => v,
        };
        let member_ids: Vec<i64> = members
            .as_array()
            .map(|a| a.iter().filter_map(|m| m.as_i64()).collect())
            .unwrap_or_default();
        let entry = by_dict.entry(dict_code).or_default();
        if master_id > 0 {
            entry.push(master_id);
        }
        for id in member_ids {
            entry.push(id);
        }
    }

    // 每字典查一次：头表名走 DCT dict_meta（替代硬编码 dict→table 映射）
    let mut name_cache: HashMap<(String, i64), (String, String)> = HashMap::new(); // (dict,id) -> (name,code)
    for (dict_code, ids) in &by_dict {
        let table = match resolve_dict_meta(dict_code).await {
            Ok(meta) => meta.table_name.clone(),
            Err(_) => continue, // dict 未注册，跳过（名称留空）
        };
        let cols = ["id", "name", "code"];
        if let Ok(rows) = store::load_by_ids(mm, db_id, None, &table, &cols, ids).await {
            for r in rows {
                let get = |k: &str| {
                    r.fields
                        .get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                name_cache.insert((dict_code.clone(), r.id), (get("name"), get("code")));
            }
        }
    }

    for g in groups.iter_mut() {
        let dict_code = g
            .get("dict_code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let master_id = g.get("master_id").and_then(|v| v.as_i64()).unwrap_or(0);
        if let Some((n, c)) = name_cache.get(&(dict_code.clone(), master_id)) {
            g["masterName"] = json!(n);
            g["masterCode"] = json!(c);
        }
        let members_raw = g.get("member_ids").cloned().unwrap_or(Value::Null);
        let members = match members_raw {
            Value::String(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::Null),
            v => v,
        };
        let member_names: Vec<Value> = members
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.as_i64())
                    .map(|id| {
                        let (n, c) = name_cache
                            .get(&(dict_code.clone(), id))
                            .cloned()
                            .unwrap_or_default();
                        json!({ "id": id, "name": n, "code": c })
                    })
                    .collect()
            })
            .unwrap_or_default();
        g["memberNames"] = json!(member_names);
    }
    groups
}

/// 确认合并。
///
/// `POST /api/mdm/merge-requests` —— 执行主从合并（master 吸收 victims，明细 reparent + 去重）。
/// `targetTable`/`surviveFields` 缺失时从 `md_match_config` 回填；`mergeId` 非空复用既有 group。
/// body：
///
/// ```json
/// { "dictCode": "supplier", "masterId": 1, "victimIds": [2, 3],
///   "mergeId": 10, "targetTable": "cm_supplier", "surviveFields": ["code", "name"],
///   "survivorship": { "name": "master" }, "overrides": { "tax_no": "911..." },
///   "scanId": 5 }
/// ```
///
/// 返回 `{ masterId, matchGroupId, reparentedTotal, dedupedTotal }`。
#[utoipa::path(
    post,
    path = "/api/mdm/merge-requests",
    request_body = Value,
    responses(
        (status = 200, description = "{ masterId, matchGroupId, reparentedTotal, dedupedTotal }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_merge_requests_create(
    headers: HeaderMap,
    Json(mut body): Json<MergeBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    // 回填：target_table/survive_fields 缺失时从 match_config 读默认
    // （steward 工作台/发现项合并可能不传，由后端兜底）
    if (body.target_table.is_empty() || body.survive_fields.is_empty())
        && let Some(d) = load_match_config_defaults(mm, &db_id, &body.dict_code).await?
    {
        if body.target_table.is_empty() {
            body.target_table = d.target_table;
        }
        if body.survive_fields.is_empty() {
            body.survive_fields = d.survive_fields;
        }
    }
    // 头表名由 body.targetTable 传入（或 match_config 回填）；
    // line_tables（明细表 reparent）从 mdm_activation.line_mappings 按 target_dict 聚合。
    let head_table = body.target_table.clone();
    if head_table.is_empty() {
        return Err(store::api_err(
            "target_table 不能为空（body 未传且 match_config 无配置）",
        ));
    }
    let line_tables = line_tables(mm, &db_id, &body.dict_code).await?;
    let operated_by = mdm_operated_by();

    // 审查 C1：管家路径带 mergeId 复用 group（不新插）；否则新插 pending
    let member_ids: Vec<i64> = std::iter::once(body.master_id)
        .chain(body.victim_ids.clone())
        .collect();
    let group_id = match body.merge_id {
        Some(g) => g,
        None => {
            store::insert_match_group(
                mm,
                &db_id,
                None,
                &body.dict_code,
                &format!("merge:{}", body.master_id),
                &json!(member_ids),
                Some(body.master_id),
                100,
                "automerge",
                "pending",
            )
            .await?
        }
    };

    // 审查 A1：未知 survivorship 规则报错（禁止静默兜底）；选 victim/手填走 overrides
    let mut rules: HashMap<String, SurvivorRule> = HashMap::new();
    if let Some(m) = body.survivorship.as_ref() {
        for (k, v) in m {
            let r = match v.as_str() {
                Some("master") => SurvivorRule::MasterFirst,
                Some("fullest") => SurvivorRule::Fullest,
                Some("latest") => SurvivorRule::Latest,
                other => {
                    return Err(store::api_err(&format!(
                        "字段 {k} 的 survivorship 规则 {other:?} 不合法（master/fullest/latest；选 victim/手填请走 overrides）"
                    )))
                }
            };
            rules.insert(k.clone(), r);
        }
    }
    let overrides = body.overrides.clone().unwrap_or_default();

    // 存活字段由 body.survive_fields 传入（来自查重规则）；空则 master 原值全保留
    let survive_fields: Vec<String> = body.survive_fields.clone();
    let stats = store::merge(
        mm,
        &db_id,
        &body.dict_code,
        &head_table,
        body.master_id,
        &body.victim_ids,
        &survive_fields,
        &rules,
        &overrides,
        &line_tables,
        operated_by,
        group_id,
    )
    .await?;

    // 联动 scan 发现项 resolved（merge 已 commit；此处失败仅 log warn 不阻断，
    // scan 仍 pending 管家可再次处理——数据已合并不会重复入库）
    if let Some(scan_id) = body.scan_id {
        match store::transition_scan_status(
            mm,
            &db_id,
            None,
            scan_id,
            "pending",
            "resolved",
            operated_by,
        )
        .await
        {
            Ok(n) if n > 0 => {}
            Ok(_) => tracing::warn!(
                target: "cmx_mdm::scan",
                scan_id,
                "scan 发现项非 pending，未联动 resolved（可能已被处理）"
            ),
            Err(e) => tracing::warn!(
                target: "cmx_mdm::scan",
                scan_id,
                error = %e,
                "联动 scan resolved 失败（merge 已成功，scan 仍 pending）"
            ),
        }
    }

    Ok(Json(ApiResp::ok(json!({
        "masterId": stats.master_id,
        "matchGroupId": group_id,
        "reparentedTotal": stats.reparented_total,
        "dedupedTotal": stats.deduped_total,
    }))))
}

/// 取合并请求详情。
///
/// `GET /api/mdm/merge-requests/detail` —— 红线 diff 用，按 `mergeId` 返回 group + master + victims 全字段。
#[utoipa::path(
    get,
    path = "/api/mdm/merge-requests/detail",
    params(UndoBody),
    responses(
        (status = 200, description = "{ group, master, victims }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_merge_request_detail(
    headers: HeaderMap,
    Query(q): Query<UndoBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let mut group = store::get_match_group(mm, &db_id, q.merge_id)
        .await?
        .ok_or_else(|| store::api_err(&format!("合并请求 {} 不存在", q.merge_id)))?;
    // 审查 B2：group 的 JSONB 列 parse 成对象再吐
    for f in ["member_ids", "survivorship_log"] {
        if let Some(Value::String(s)) = group.get(f).cloned()
            && let Ok(p) = serde_json::from_str::<Value>(&s) {
                group[f] = p;
            }
    }
    let dict_code = group
        .get("dict_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| store::api_err("合并记录 dict_code 缺失"))?
        .to_string();
    let master_id = group.get("master_id").and_then(|v| v.as_i64()).unwrap_or(0);
    // 头表名 + 列清单走 DCT dict_meta（替代硬编码 dict_tables/load_columns）
    let meta = resolve_dict_meta(&dict_code).await?;
    let head_table = meta.table_name.clone();
    let victim_ids: Vec<i64> = group
        .get("member_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.as_i64().filter(|id| *id != master_id))
                .collect()
        })
        .unwrap_or_default();
    // 列清单取 DictMeta.column_names() 全量字段名（替代硬编码 load_columns）
    let cols: Vec<String> = meta.column_names().into_iter().map(String::from).collect();
    let cols_ref: Vec<&str> = cols.iter().map(|s| s.as_str()).collect();
    let master = store::load_by_ids(mm, &db_id, None, &head_table, &cols_ref, &[master_id])
        .await?
        .pop()
        .map(|r| r.fields)
        .unwrap_or_default();
    let victims = store::load_by_ids(mm, &db_id, None, &head_table, &cols_ref, &victim_ids)
        .await?
        .into_iter()
        .map(|r| r.fields)
        .collect::<Vec<_>>();
    Ok(Json(ApiResp::ok(json!({ "group": group, "master": master, "victims": victims }))))
}

/// 驳回合并请求。
///
/// `POST /api/mdm/merge-requests/reject` —— CAS pending→rejected + 审计留痕。body：
///
/// ```json
/// { "mergeId": 10, "reason": "误判" }
/// ```
///
/// 返回 `{ mergeId, status: "rejected" }`。
#[utoipa::path(
    post,
    path = "/api/mdm/merge-requests/reject",
    request_body = Value,
    responses(
        (status = 200, description = "{ mergeId, status }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_merge_request_reject(
    headers: HeaderMap,
    Json(body): Json<RejectMergeBody>,
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
    let n = store::transition_match_group(
        mm,
        &db_id,
        Some(&txn_id),
        body.merge_id,
        "pending",
        "rejected",
    )
    .await?;
    if n == 0 {
        return Err(store::api_err(&format!(
            "group {} 非 pending，不可驳回",
            body.merge_id
        )));
    }
    // 留痕：驳回人 + 原因存 group.survivorship_log（pending 时为 NULL，不覆盖既有 slog）
    let log = json!({ "rejected_by": operated_by, "reason": body.reason });
    store::update_match_group(mm, &db_id, Some(&txn_id), body.merge_id, "rejected", Some(&log), None)
        .await?;
    guard
        .commit()
        .await
        .map_err(|e| store::api_err(&format!("提交失败: {e}")))?;
    Ok(Json(ApiResp::ok(
        json!({ "mergeId": body.merge_id, "status": "rejected" }),
    )))
}

/// 还原合并。
///
/// `POST /api/mdm/merge-requests/undo` —— unmerge（明细重新 reparent 回 victim，恢复两条独立记录）。
/// body `{ mergeId }`：
///
/// ```json
/// { "mergeId": 10 }
/// ```
///
/// 返回 `{ masterId, victimId, status: "unmerged" }`。
#[utoipa::path(
    post,
    path = "/api/mdm/merge-requests/undo",
    request_body = Value,
    responses(
        (status = 200, description = "{ masterId, victimId, status }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_merge_requests_undo(
    headers: HeaderMap,
    Json(body): Json<UndoBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;
    let group = store::get_match_group(mm, &db_id, body.merge_id)
        .await?
        .ok_or_else(|| store::api_err(&format!("合并请求 {} 不存在", body.merge_id)))?;
    let dict_code = group
        .get("dict_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| store::api_err("合并记录 dict_code 缺失"))?
        .to_string();
    let master_id = group.get("master_id").and_then(|v| v.as_i64()).unwrap_or(0);
    // 头表名走 DCT dict_meta（替代硬编码 dict_tables 头表）；明细表清单从 line_mappings 聚合
    let meta = resolve_dict_meta(&dict_code).await?;
    let head_table = meta.table_name.clone();
    let line_tables = line_tables(mm, &db_id, &dict_code).await?;
    // victim = member_ids 中非 master 的第一个（JSONB 列 to_json_value 为转义字符串，需 parse）
    let members_raw = group.get("member_ids").cloned().unwrap_or(Value::Null);
    let members = match members_raw {
        Value::String(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::Null),
        v => v,
    };
    let victim_id = members
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find_map(|m| m.as_i64().filter(|id| *id != master_id))
        })
        .unwrap_or(0);
    let operated_by = mdm_operated_by();

    store::unmerge(
        mm,
        &db_id,
        &dict_code,
        &head_table,
        master_id,
        victim_id,
        &line_tables,
        operated_by,
        body.merge_id,
    )
    .await?;
    Ok(Json(ApiResp::ok(
        json!({ "masterId": master_id, "victimId": victim_id, "status": "unmerged" }),
    )))
}

/// 合并请求列表查询（分页）。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MergeListQuery {
    #[serde(default, alias = "dictCode")]
    pub dict_code: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// 默认排除 pending（查重预览不再落 pending；历史区只看真正合并过的）。
    /// `"1"`/`"true"` 或缺省 = 排除；`"0"`/`"false"` = 不排除。
    #[serde(default, alias = "excludePending")]
    pub exclude_pending: Option<String>,
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page_size", alias = "pageSize")]
    pub page_size: i64,
    /// 名称搜索关键字（D-05）：在目标 `cm_*.name` 上 `ILIKE %kw%`。
    /// 需配合 dictCode（"全部字典"时无法解析目标表，后端忽略 kw）。
    #[serde(default)]
    pub kw: Option<String>,
}

/// 确认合并请求体。
#[derive(serde::Deserialize)]
pub struct MergeBody {
    #[serde(alias = "dictCode")]
    pub dict_code: String,
    #[serde(alias = "masterId")]
    pub master_id: i64,
    #[serde(default, alias = "victimIds")]
    pub victim_ids: Vec<i64>,
    /// 管家路径复用 group（审查 C1）；不传则新插。
    #[serde(default, alias = "mergeId")]
    pub merge_id: Option<i64>,
    /// 目标头物理表（来自查重规则，替代硬编码 dict_tables 头表）。
    #[serde(default, alias = "targetTable")]
    pub target_table: String,
    /// 存活字段（来自查重规则，替代硬编码 default_survive_fields）。
    #[serde(default, alias = "surviveFields")]
    pub survive_fields: Vec<String>,
    /// 字段级存活策略（`master`/`fullest`/`latest`）。
    #[serde(default)]
    pub survivorship: Option<serde_json::Map<String, Value>>,
    /// 人工裁决显式真值（选 victim/手填，审查 A1/A2）；键 ⊆ survive_fields。
    #[serde(default)]
    pub overrides: Option<serde_json::Map<String, Value>>,
    /// 联动的查重发现项 id（match-scan 工作台确认合并时传入；合并成功后 CAS pending→resolved）。
    #[serde(default, alias = "scanId")]
    pub scan_id: Option<i64>,
}

/// undo / detail 查询体。
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UndoBody {
    /// 合并请求 id。
    #[serde(alias = "mergeId")]
    pub merge_id: i64,
}

/// 驳回合并请求体。
#[derive(serde::Deserialize)]
pub struct RejectMergeBody {
    /// 合并请求 id。
    #[serde(alias = "mergeId")]
    pub merge_id: i64,
    /// 驳回原因（可选）。
    #[serde(default)]
    pub reason: Option<String>,
}
