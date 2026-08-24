//! 实时查重 + 关键信息查重 handler。
//!
//! 对应路由（`cmx-mdm-api/src/lib.rs`）：
//! - `POST /mdm/records/find-duplicates` → [`mdm_find_duplicates`]
//! - `POST /mdm/check-key` → [`mdm_check_key`]
//!
//! 本模块还提供：
//! - [`resolve_dict_meta`]：按 dict_code 调 DCT `dict_meta` 拿 DictMeta（头表名 + 列清单），
//!   供 [`super::merge`] 的 detail/undo 取头表名、详情取列清单（替代硬编码 load_columns）。
//! - [`line_tables`]：明细表清单（merge/undo 的明细 reparent 用，从 mdm_activation.line_mappings 按 target_dict 聚合）。

use axum::Json;
use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::db_id::resolve_db_id_from_headers;
use cmx_api_types::{ApiResp, Result};

use cmx_database_pg::{get_default_pg_db_manager, DatabaseManager};
use cmx_dct_store_pg::{DctQuery, DictMeta, dict_meta};
use cmx_mdm_model::match_algo::{find_candidates, MatchRecord};
use cmx_mdm_store_pg as store;

use super::SpecDto;

/// 按 dict_code 调 DCT `dict_meta` 拿 [`DictMeta`]（头表名 + 全量列清单）。
///
/// [`DctQuery::by_code`] 构造定位器——dict_meta 内部按 dict 全局反查补全坐标
/// （`coord::resolve_dam_by_code`），MDM 侧无需感知字典定义文件所在模块。
///
/// # Errors
///
/// dict 未注册、定义文件缺失或 tableName 缺失时返回错误。
pub(crate) async fn resolve_dict_meta(dict_code: &str) -> Result<DictMeta> {
    dict_meta(&DctQuery::by_code(dict_code)).await
}

/// dict → 明细表清单（含去重键，merge/undo 的明细 reparent + 去重用）。
///
/// 流程：从 `mdm_activation.line_mappings` 按 `target_dict` 聚合明细表
/// `(table, parent_field, target_dict)`；再对每个 `target_dict` 调 [`resolve_dict_meta`] 读
/// DCT `uniqueKeys`，去掉外键列（`parent_field`）后剩余字段即该表的去重业务键。
/// 未注册字典或无 uniqueKeys 的表 `dedup_keys` 为空（合并不去重，全量 reparent）。
pub(crate) async fn line_tables(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: &str,
) -> Result<Vec<store::LineTableInfo>> {
    let raw = store::line_tables_for_dict(mm, db_id, dict_code).await?;
    let mut out = Vec::with_capacity(raw.len());
    for (table, parent_field, target_dict) in raw {
        // 从 DCT uniqueKeys 推导去重键；target_dict 缺失或未注册则不去重
        let dedup_keys = if target_dict.is_empty() {
            Vec::new()
        } else {
            resolve_dict_meta(&target_dict)
                .await
                .ok()
                .map(|meta| dedup_keys_from(&meta.unique_keys, &parent_field))
                .unwrap_or_default()
        };
        out.push(store::LineTableInfo { table, parent_field, dedup_keys });
    }
    Ok(out)
}

/// 从 uniqueKeys 推导明细去重键：取第一组唯一键，去掉外键列（parent_field）后剩余字段。
///
/// 如 cm_bank_account 的 `[["supplier_id","account_no"]]` 去掉 `supplier_id` → `["account_no"]`。
/// 无 uniqueKeys 或只剩外键列时返回空 Vec（该表不去重）。
fn dedup_keys_from(unique_keys: &[Vec<String>], parent_field: &str) -> Vec<String> {
    unique_keys
        .iter()
        .next()
        .map(|grp| grp.iter().filter(|k| *k != parent_field).cloned().collect())
        .unwrap_or_default()
}

/// 查重规则默认值（从 md_match_config 按 dictCode 读）。
///
/// [`find_duplicates`](mdm_find_duplicates) / [`check_key`](mdm_check_key) /
/// match-scan/run 在 body 字段缺失时用它兜底。
#[derive(Debug, Clone)]
pub(crate) struct MatchConfigDefaults {
    /// 目标头物理表。
    pub target_table: String,
    /// 比较字段规则。
    pub specs: Vec<SpecDto>,
    /// 分块簇键。
    pub cluster_keys: Vec<String>,
    /// 存活字段。
    pub survive_fields: Vec<String>,
}

/// 按 dictCode 从 md_match_config 读第一条 active 规则作默认。
///
/// 返回 `None` 表示无配置，或配置缺 target_table（视为无效）。
/// specs/cluster_keys/survive_fields 是 JSONB 数组，已由 store 层 parse 成对象。
///
/// # Errors
///
/// DB 查询失败时返回错误。
pub(crate) async fn load_match_config_defaults(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: &str,
) -> Result<Option<MatchConfigDefaults>> {
    let list = store::list_match_config(mm, db_id, Some(dict_code)).await?;
    let cfg = match list.into_iter().next() {
        Some(c) => c,
        None => return Ok(None),
    };
    let target_table = cfg
        .get("target_table")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if target_table.is_empty() {
        return Ok(None);
    }
    let specs = parse_array_field::<SpecDto>(&cfg, "specs");
    let cluster_keys = parse_array_field::<String>(&cfg, "cluster_keys");
    let survive_fields = parse_array_field::<String>(&cfg, "survive_fields");
    Ok(Some(MatchConfigDefaults {
        target_table,
        specs,
        cluster_keys,
        survive_fields,
    }))
}

/// 从 JSON 对象的指定字段反序列化数组（缺失或类型不符返回空 Vec）。
fn parse_array_field<T: serde::de::DeserializeOwned>(v: &Value, field: &str) -> Vec<T> {
    v.get(field)
        .filter(|v| v.is_array())
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// 实时查重。
///
/// `POST /api/mdm/records/find-duplicates` —— 锚点查重（纯查询不落库），按 `recordId` 找同伙。
/// `targetTable`/`specs`/`clusterKeys`/`surviveFields` 任一缺失时从 `md_match_config` 按 `dictCode`
/// 读默认。body：
///
/// ```json
/// { "dictCode": "supplier", "recordId": 101, "targetTable": "cm_supplier",
///   "specs": [{ "field": "name", "weight": 100, "kind": "EditDistance" }],
///   "clusterKeys": ["tax_no"], "surviveFields": ["code", "name"],
///   "displayFields": ["label"] }
/// ```
///
/// 候选裁决：≥95 自动合并 / 80-94 待评审 / <80 不匹配（双阈值）。返回目标字段 + 每个候选的字段值（供前端对比表）。
#[utoipa::path(
    post,
    path = "/api/mdm/records/find-duplicates",
    request_body = Value,
    responses(
        (status = 200, description = "{ targetId, targetFields, candidates[{recordId,score,decision,fields}], thresholds }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_find_duplicates(
    headers: HeaderMap,
    Json(mut body): Json<FindDupBody>,
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

    // 把 DTO specs 转成 MatchFieldSpec（校验 kind 合法）
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

    // 装载列 = id ∪ specs 字段 ∪ surviveFields ∪ displayFields ∪ {update_time}
    // （防注入经 load_published validate_ident；displayFields 仅展示用，如 label/code）
    let mut col_set: Vec<String> = vec!["id".into(), "update_time".into()];
    for s in &body.specs {
        col_set.push(s.field.clone());
    }
    for f in &body.survive_fields {
        col_set.push(f.clone());
    }
    for f in &body.display_fields {
        col_set.push(f.clone());
    }
    col_set.sort();
    col_set.dedup();
    let columns: Vec<&str> = col_set.iter().map(|s| s.as_str()).collect();

    let all = store::load_suspects(mm, &db_id, &body.target_table, &columns, &cluster_keys).await?;
    let target = all
        .iter()
        .find(|r| r.id == body.record_id)
        .cloned()
        .ok_or_else(|| store::api_err(&format!("记录 {} 不存在或非 published", body.record_id)))?;

    let candidates = find_candidates(&target, &all, &specs, &cluster_keys);

    // 不落库（查重预览）。落库收敛到 mdm_merge_requests_create 一处。
    Ok(Json(ApiResp::ok(json!({
        "targetId": target.id,
        "targetFields": target.fields,
        "candidates": candidates.iter().map(|c| {
            // 回填候选的字段值（供前端对比表）
            let rec = all.iter().find(|r| r.id == c.record_id);
            json!({
                "recordId": c.record_id,
                "score": c.score,
                "decision": format!("{:?}", c.decision),
                "fields": rec.map(|r| r.fields.clone()).unwrap_or_default(),
            })
        }).collect::<Vec<_>>(),
        "thresholds": { "auto_merge": 95, "review": 80 },
    }))))
}

/// 关键信息查重。
///
/// `POST /api/mdm/check-key` —— V3.2 步骤条预校验（新建场景，无 recordId）。用前端提交的关键信息
/// 构造虚拟 target（id=0）与激活区已发布记录比对；命中（score ≥ 80）即阻断。body：
///
/// ```json
/// { "dictCode": "supplier", "targetTable": "cm_supplier",
///   "keyValue": { "name": "A公司", "tax_no": "911..." },
///   "specs": [{ "field": "name", "weight": 100, "kind": "EditDistance" }],
///   "clusterKeys": ["tax_no"] }
/// ```
///
/// 返回 `{ exists: false }` 或 `{ exists: true, id, code, message }`。
#[utoipa::path(
    post,
    path = "/api/mdm/check-key",
    request_body = Value,
    responses(
        (status = 200, description = "{ exists: false } 或 { exists: true, id, code, message }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_check_key(
    headers: HeaderMap,
    Json(mut body): Json<CheckKeyBody>,
) -> Result<Json<ApiResp<Value>>> {
    let mm = get_default_pg_db_manager();
    let db_id = resolve_db_id_from_headers(&headers).await;

    // 回填：target_table/specs/cluster_keys 任一缺失，从 match_config 读默认
    if (body.target_table.is_empty() || body.specs.is_empty() || body.cluster_keys.is_empty())
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
    }

    // specs → MatchFieldSpec（校验 kind 合法）
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

    // 装载列 = id ∪ specs 字段 ∪ {code, name, update_time}（code/name 用于返回给前端展示）
    let mut col_set: Vec<String> = vec!["id".into(), "code".into(), "name".into(), "update_time".into()];
    for s in &body.specs {
        col_set.push(s.field.clone());
    }
    col_set.sort();
    col_set.dedup();
    let columns: Vec<&str> = col_set.iter().map(|s| s.as_str()).collect();

    // 拉嫌疑记录（DB 内分块下推，避免全量装载）
    let all = store::load_suspects(mm, &db_id, &body.target_table, &columns, &cluster_keys).await?;

    // 构造虚拟 target：id=0（表示未落库），fields = keyValue
    let target = MatchRecord {
        id: 0,
        fields: body.key_value.clone(),
    };

    let candidates = find_candidates(&target, &all, &specs, &cluster_keys);

    // 命中即阻断（score ≥ 80 = Review 阈值）
    if let Some(first) = candidates.first() {
        // 找到匹配记录，取 id/code 用于返回
        let rec = all.iter().find(|r| r.id == first.record_id);
        let code = rec
            .and_then(|r| r.fields.get("code"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let name = rec
            .and_then(|r| r.fields.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // 拼展示消息：已存在相同记录：SUP001（A公司）
        let display = match (code, name) {
            (c, n) if !c.is_empty() && !n.is_empty() => format!("{}（{}）", c, n),
            (c, "") if !c.is_empty() => c.to_string(),
            ("", n) if !n.is_empty() => n.to_string(),
            _ => format!("id={}", first.record_id),
        };
        return Ok(Json(ApiResp::ok(json!({
            "exists": true,
            "id": first.record_id,
            "code": code,
            "message": format!("已存在相同记录：{}", display),
        }))));
    }

    Ok(Json(ApiResp::ok(json!({ "exists": false }))))
}

/// find-duplicates 请求体。
#[derive(serde::Deserialize)]
pub struct FindDupBody {
    #[serde(alias = "dictCode")]
    pub dict_code: String,
    #[serde(alias = "recordId")]
    pub record_id: i64,
    /// 目标头物理表（从 dct/meta tableName 或 match_config 带入，替代硬编码 dict_tables）。
    #[serde(alias = "targetTable")]
    pub target_table: String,
    /// 比较字段规则（替代硬编码 default_specs）。
    #[serde(default)]
    pub specs: Vec<SpecDto>,
    /// 分块簇键（替代硬编码 default_cluster_keys）。
    #[serde(default, alias = "clusterKeys")]
    pub cluster_keys: Vec<String>,
    /// 存活字段（供前端做字段对比展示用；查重本身只需 specs）。
    #[serde(default, alias = "surviveFields")]
    pub survive_fields: Vec<String>,
    /// 仅用于展示的附加列（如 labelField/codeField），不参与匹配/存活，只随候选字段返回。
    #[serde(default, alias = "displayFields")]
    pub display_fields: Vec<String>,
}

/// check-key 请求体（V3.2 步骤条预校验）。
#[derive(serde::Deserialize)]
pub struct CheckKeyBody {
    #[serde(alias = "dictCode")]
    pub dict_code: String,
    #[serde(alias = "targetTable")]
    pub target_table: String,
    /// 关键信息字段值（虚拟 target 的 fields），如 `{ "name": "A公司", "tax_no": "911..." }`。
    #[serde(alias = "keyValue")]
    pub key_value: serde_json::Map<String, Value>,
    /// 比较字段规则（同 [`FindDupBody::specs`]）。
    #[serde(default)]
    pub specs: Vec<SpecDto>,
    /// 分块簇键（同 [`FindDupBody::cluster_keys`]）。
    #[serde(default, alias = "clusterKeys")]
    pub cluster_keys: Vec<String>,
}
