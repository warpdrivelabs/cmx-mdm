//! cm_* 主数据 published 行装载（查重 / 合并事务内读主数据）。
//!
//! 表名 / 列名经 [`validate_ident`] 白名单校验防注入。

use cmx_core::model::cell::DataValue;
use cmx_database_pg::DatabaseManager;
use cmx_mdm_model::match_algo::MatchRecord;

use crate::dct_accessor::validate_ident;
use crate::error::api_err_db;

/// 读字典全量 published 行（cm_*）。
///
/// `columns` = id + 比较 / 存活字段 + update_time。表名 / 列名经 [`validate_ident`] 白名单校验防注入。
///
/// ⚠ **大表慎用**：本函数回传全量 published 行（O(N)），几十万行会爆内存。
/// 全库查重场景应改用 [`load_suspects`]（分块下推 SQL，只回传嫌疑记录）。
/// 本函数仅适合小表、调试，或已按 id 范围预过滤后的装载。
pub async fn load_published(
    mm: &DatabaseManager,
    db_id: &str,
    table: &str,
    columns: &[&str],
) -> Result<Vec<MatchRecord>, cmx_api_types::Error> {
    validate_ident(table)?;
    for c in columns {
        validate_ident(c)?;
    }
    // cm_* 治理列无 delete_flag（DCT 字典表），仅按 lifecycle_status 过滤
    let sql = format!(
        "SELECT {} FROM {table} WHERE lifecycle_status = 'published'",
        columns.join(", ")
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, vec![], "mdm_load_published")
        .await
        .map_err(|e| api_err_db(&format!("装载 {table} published 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = Vec::with_capacity(ds.rows.len());
    for row in ds.rows.iter() {
        let v = row.to_json_value(schema);
        let obj = v.as_object().cloned().unwrap_or_default();
        let id = obj.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
        out.push(MatchRecord { id, fields: obj });
    }
    Ok(out)
}

/// 嫌疑记录回传总量上限（兜底防脏数据爆内存）。
///
/// 正常数据永远触不到；触发说明簇键区分度太低（如 name 大量重名），需数据治理。
/// 触顶时 SQL 截断 + `tracing::warn`，被截记录未参与比较。
const SUSPECT_CAP: i64 = 50_000;

/// 按 cluster_keys 下推 SQL 分块，只拉「嫌疑记录」（块大小>1 的簇键对应行）。
///
/// 与 [`load_published`] 的区别：`load_published` 拉全量 published 行（O(N)），
/// 本函数把分块过滤下推到 SQL（`GROUP BY HAVING COUNT(*)>1` 找出"有重复"的簇键值），
/// 只回传这些嫌疑键对应的记录（O(嫌疑块)，通常 ≪ N），供应用层做字段级精细比较。
///
/// ## SQL 原理：GROUP BY HAVING 怎么找重复
///
/// `GROUP BY credit_code HAVING COUNT(*) > 1` 的含义：按 credit_code 分组，
/// 只保留「组内行数 > 1」的组——即该信用代码对应了多条记录，潜在重复。
/// 把这些「有重复的簇键值」收集起来，再用 `WHERE credit_code IN (...)`
/// 把对应记录全部拉出，交给应用层做字段级精细比较。整个过滤在 DB 内完成，
/// 应用层只收到「嫌疑记录」（通常几十~几百条），避免全量回传。
///
/// SQL 结构（每个 cluster_key 一段 CTE + 一段 OR 分支）：
///
/// ```sql
/// WITH s_<key1> AS (
///   SELECT <key1> AS k FROM {table}
///   WHERE lifecycle_status='published' AND <key1> IS NOT NULL AND <key1> <> ''
///   GROUP BY <key1> HAVING COUNT(*) > 1
/// ), s_<key2> AS ( ... )
/// SELECT DISTINCT {columns} FROM {table} t
/// WHERE lifecycle_status='published'
///   AND (t.<key1> IN (SELECT k FROM s_<key1>) OR t.<key2> IN (SELECT k FROM s_<key2>) OR ...)
/// ```
///
/// 语义等价于 `load_published` 后应用层
/// [`blocking`](cmx_mdm_model::match_algo::blocking) 再保留"块大小>1"的记录，
/// 但避免全量回传。簇键为精确等值分块（模糊簇键 soundex 未实现，见方案缺口）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据库标识。
/// * `table` - cm_* 主数据物理表名（经 [`validate_ident`]）。
/// * `columns` - 回传列（经 [`validate_ident`]）；须含 `id`。
/// * `cluster_keys` - 分块簇键列表（经 [`validate_ident`]）；空则返回空 Vec。
///
/// ## LIMIT 兜底
///
/// SQL 末尾带 `LIMIT` [`SUSPECT_CAP`] = 50000：正常数据永远触不到；
/// 触顶（回传数 ≥ 上限）说明数据过脏（大量簇键重复值），会 `tracing::warn` 提示，
/// 被截记录未参与比较——建议先做数据治理或分批扫描。
///
/// # Errors
///
/// 表名/列名/簇键名非法，或 SQL 执行失败时返回错误。
pub async fn load_suspects(
    mm: &DatabaseManager,
    db_id: &str,
    table: &str,
    columns: &[&str],
    cluster_keys: &[&str],
) -> Result<Vec<MatchRecord>, cmx_api_types::Error> {
    validate_ident(table)?;
    for c in columns {
        validate_ident(c)?;
    }
    for k in cluster_keys {
        validate_ident(k)?;
    }
    if cluster_keys.is_empty() {
        return Ok(Vec::new());
    }
    // 每个 cluster_key 一段 CTE：找该键有重复的值（COUNT>1）
    let ctes: Vec<String> = cluster_keys
        .iter()
        .map(|k| {
            format!(
                "s_{k} AS (SELECT {k} AS k FROM {table} \
                 WHERE lifecycle_status='published' AND {k} IS NOT NULL AND {k} <> '' \
                 GROUP BY {k} HAVING COUNT(*) > 1)"
            )
        })
        .collect();
    // 每个 cluster_key 一段 OR：命中任一嫌疑键集合的记录都拉出
    let ors: Vec<String> = cluster_keys
        .iter()
        .map(|k| format!("t.{k} IN (SELECT k FROM s_{k})"))
        .collect();
    let sql = format!(
        "WITH {ctes} \
         SELECT DISTINCT {cols} FROM {table} t \
         WHERE lifecycle_status='published' AND ({ors}) \
         LIMIT {cap}",
        ctes = ctes.join(", "),
        cols = columns.join(", "),
        table = table,
        ors = ors.join(" OR "),
        cap = SUSPECT_CAP
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, vec![], "mdm_load_suspects")
        .await
        .map_err(|e| api_err_db(&format!("装载 {table} 嫌疑记录失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = Vec::with_capacity(ds.rows.len());
    for row in ds.rows.iter() {
        let v = row.to_json_value(schema);
        let obj = v.as_object().cloned().unwrap_or_default();
        let id = obj.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
        out.push(MatchRecord { id, fields: obj });
    }
    // 触顶兜底：回传数达上限说明数据过脏（大量簇键重复），warn 提示管家治理
    if out.len() as i64 >= SUSPECT_CAP {
        tracing::warn!(
            target: "cmx_mdm::match",
            table = table,
            returned = out.len(),
            cap = SUSPECT_CAP,
            "嫌疑记录触顶截断（数据过脏：大量簇键重复值）。被截记录未参与比较，建议数据治理或分批扫描"
        );
    }
    Ok(out)
}

/// 按 id 批量读 cm_* 行（merge 事务内读 master/victims）。columns 同 [`load_published`]。
pub async fn load_by_ids(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    table: &str,
    columns: &[&str],
    ids: &[i64],
) -> Result<Vec<MatchRecord>, cmx_api_types::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    validate_ident(table)?;
    for c in columns {
        validate_ident(c)?;
    }
    let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "SELECT {} FROM {table} WHERE id IN ({})",
        columns.join(", "),
        placeholders.join(", ")
    );
    let params: Vec<DataValue> = ids.iter().map(|i| DataValue::Int(*i)).collect();
    let ds = mm
        .query_sql_with_datavalues(db_id, txn_id, &sql, params, "mdm_load_by_ids")
        .await
        .map_err(|e| api_err_db(&format!("按 id 读 {table} 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = Vec::with_capacity(ds.rows.len());
    for row in ds.rows.iter() {
        let v = row.to_json_value(schema);
        let obj = v.as_object().cloned().unwrap_or_default();
        let id = obj.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
        out.push(MatchRecord { id, fields: obj });
    }
    Ok(out)
}
