//! md_match_scan 查重发现项 store：全库扫描结果载体（管家评审队列）。
//!
//! 与 md_merge_record 职责分离：
//! - md_match_scan：**发现项**（系统扫描出的重复簇，pending→resolved/ignored）；
//! - md_merge_record：**合并事务载体**（确认合并后承载 survivorship_log）。
//!
//! 去重靠 cluster_hash（member_ids 升序后 SHA256 前 32 hex 字符）：
//! 相同成员集合的 pending 记录不重复插入；resolved/ignored 后再次发现会重新产生 pending。
//!
//! 绑定口径（对齐 match_store）：可空 BIGINT 用 `DataValue::from(Option<i64>)`，
//! JSONB 用 `DataValue::Json(String)`；时间戳列由 DB DEFAULT/now() 算，应用层不 SET。

use std::collections::{HashMap, HashSet};

use cmx_core::dv;
use cmx_core::model::cell::DataValue;
use cmx_database_pg::DatabaseManager;
use cmx_utils::next_pk_id;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::{api_err_db, parse_jsonb_field};

/// 待入库的簇（handler 从 [`DupCluster`](cmx_mdm_model::match_algo::DupCluster) 转换而来）。
#[derive(Debug, Clone)]
pub struct PreparedCluster {
    /// 簇键标识，如 `"credit_code:C1"`。
    pub cluster_key: String,
    /// 簇内记录 id（无需排序，[`cluster_hash`] 内部会升序）。
    pub member_ids: Vec<i64>,
    /// 簇内最高配对分（0-100）。
    pub max_score: u8,
}

/// 批量插入结果。
#[derive(Debug, Clone, Copy, Default)]
pub struct InsertStats {
    /// 新插入数。
    pub inserted: u64,
    /// 去重跳过数（已存在相同 cluster_hash 的 pending）。
    pub skipped: u64,
}

/// 插入发现项（cluster_hash 去重）。
///
/// 去重策略：按 dict_code 拉所有 pending 的 cluster_hash 集合，新簇 hash 命中则跳过。
/// 已 resolved/ignored 的簇不参与去重——数据若再次重复会重新产生 pending。
///
/// # Arguments
///
/// * `dict_code` - 字典码（限定去重域）。
/// * `clusters` - 待入库的簇列表。
///
/// # Returns
///
/// 插入统计 [`InsertStats`]（inserted + skipped）。
pub async fn insert_findings(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: &str,
    clusters: &[PreparedCluster],
) -> Result<InsertStats, cmx_api_types::Error> {
    if clusters.is_empty() {
        return Ok(InsertStats::default());
    }
    // 拉当前 dict_code 下所有 pending 的 cluster_hash（去重比对集）
    let existing = load_pending_hashes(mm, db_id, dict_code).await?;
    let mut stats = InsertStats::default();
    for c in clusters {
        let hash = cluster_hash(&c.member_ids);
        if existing.contains(&hash) {
            stats.skipped += 1;
            continue;
        }
        let id = next_pk_id();
        let member_ids_json = json!(c.member_ids);
        let sql = r#"INSERT INTO md_match_scan
            (id, dict_code, cluster_key, cluster_hash, member_ids, member_count, max_score, status)
          VALUES ($1,$2,$3,$4,$5,$6,$7,'pending')"#;
        mm.execute_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![
                DataValue::Int(id),
                DataValue::String(dict_code.into()),
                DataValue::String(c.cluster_key.clone()),
                DataValue::String(hash),
                DataValue::Json(member_ids_json.to_string()),
                DataValue::Int(c.member_ids.len() as i64),
                DataValue::Int(c.max_score as i64),
            ],
        )
        .await
        .map_err(|e| api_err_db(&format!("写 md_match_scan 失败: {e}")))?;
        stats.inserted += 1;
    }
    Ok(stats)
}

/// 按 dict_code 拉所有 pending 的 cluster_hash 集合（去重用）。
async fn load_pending_hashes(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: &str,
) -> Result<HashSet<String>, cmx_api_types::Error> {
    let sql = "SELECT cluster_hash FROM md_match_scan WHERE dict_code = $1 AND status = 'pending'";
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![DataValue::String(dict_code.into())],
            "mdm_scan_pending_hashes",
        )
        .await
        .map_err(|e| api_err_db(&format!("读 md_match_scan pending hash 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = HashSet::new();
    for row in ds.rows.iter() {
        if let Some(h) = row.get_by_name_as::<String>(schema, "cluster_hash") {
            out.insert(h);
        }
    }
    Ok(out)
}

/// 发现项列表（分页，dictCode + status 过滤）。
///
/// 排序：max_score DESC（高分优先）→ created_at DESC → id DESC。
/// 按 status 聚合计数（管家工作台 summary 用）。
///
/// `dict_code` 为 `Some` 时按字典过滤；`None` 全表。吃 `(dict_code, status)` 索引。
/// 返回 `status → 数量`；未出现的 status 调用方默认 0。
pub async fn count_scan_by_status(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: Option<&str>,
) -> Result<HashMap<String, i64>, cmx_api_types::Error> {
    let sql = if dict_code.is_some() {
        "SELECT status, COUNT(*)::bigint AS c FROM md_match_scan \
         WHERE dict_code = $1 GROUP BY status"
    } else {
        "SELECT status, COUNT(*)::bigint AS c FROM md_match_scan GROUP BY status"
    };
    let params: Vec<DataValue> = match dict_code {
        Some(d) => vec![DataValue::String(d.into())],
        None => vec![],
    };
    let ds = mm
        .query_sql_with_datavalues(db_id, None, sql, params, "mdm_scan_count_by_status")
        .await
        .map_err(|e| api_err_db(&format!("聚合 md_match_scan 计数失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = HashMap::new();
    for row in ds.rows.iter() {
        if let (Some(s), Some(c)) = (
            row.get_by_name_as::<String>(schema, "status"),
            row.get_by_name_as::<i64>(schema, "c"),
        ) {
            out.insert(s, c);
        }
    }
    Ok(out)
}

/// member_ids JSONB 列经 [`parse_jsonb_field`] 转对象。
pub async fn list_scans(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: Option<&str>,
    status: Option<&str>,
    page: i64,
    page_size: i64,
) -> Result<(Vec<Value>, i64), cmx_api_types::Error> {
    let mut clauses = vec!["1=1".to_string()];
    let mut params: Vec<DataValue> = Vec::new();
    if let Some(d) = dict_code {
        clauses.push(format!("dict_code = ${}", params.len() + 1));
        params.push(DataValue::String(d.into()));
    }
    if let Some(s) = status {
        clauses.push(format!("status = ${}", params.len() + 1));
        params.push(DataValue::String(s.into()));
    }
    let where_sql = clauses.join(" AND ");
    let cnt_sql = format!("SELECT COUNT(*) AS c FROM md_match_scan WHERE {where_sql}");
    let cds = mm
        .query_sql_with_datavalues(db_id, None, &cnt_sql, params.clone(), "mdm_scan_count")
        .await
        .map_err(|e| api_err_db(&format!("查 md_match_scan 总数失败: {e}")))?;
    let total = cds
        .rows
        .first()
        .and_then(|r| r.get_by_name_as::<i64>(cds.schema.as_ref(), "c"))
        .unwrap_or(0);
    let ps = if page_size > 0 { page_size } else { 20 };
    let pg = if page > 0 { page } else { 1 };
    let off = (pg - 1) * ps;
    let n = params.len() as i64;
    params.push(DataValue::Int(ps));
    params.push(DataValue::Int(off));
    let sql = format!(
        "SELECT id, dict_code, cluster_key, member_ids, member_count, max_score, status, \
         scaned_at, resolved_at, resolved_by, created_at \
         FROM md_match_scan WHERE {where_sql} ORDER BY max_score DESC, created_at DESC, id DESC \
         LIMIT ${} OFFSET ${}",
        n + 1,
        n + 2
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_scan_list")
        .await
        .map_err(|e| api_err_db(&format!("列表 md_match_scan 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = Vec::with_capacity(ds.rows.len());
    for row in ds.rows.iter() {
        let mut v = row.to_json_value(schema);
        parse_jsonb_field(&mut v, "member_ids");
        out.push(v);
    }
    Ok((out, total))
}

/// 单条详情。member_ids JSONB 列经 [`parse_jsonb_field`] 转对象。
pub async fn get_scan(
    mm: &DatabaseManager,
    db_id: &str,
    scan_id: i64,
) -> Result<Option<Value>, cmx_api_types::Error> {
    let sql = "SELECT id, dict_code, cluster_key, member_ids, member_count, max_score, status, \
               scaned_at, resolved_at, resolved_by, created_at \
               FROM md_match_scan WHERE id = $1";
    let ds = mm
        .query_sql_with_datavalues(db_id, None, sql, dv![DataValue::Int(scan_id)], "mdm_scan_get")
        .await
        .map_err(|e| api_err_db(&format!("查 md_match_scan 失败: {e}")))?;
    let Some(row) = ds.rows.first() else {
        return Ok(None);
    };
    let mut v = row.to_json_value(ds.schema.as_ref());
    parse_jsonb_field(&mut v, "member_ids");
    Ok(Some(v))
}

/// CAS 状态流转：pending→resolved/ignored。返回行数（0=状态冲突）。
///
/// resolved/ignored 均视为"处理完毕"，统一记 `resolved_at = now()` + `resolved_by`。
///
/// # Arguments
///
/// * `txn_id` - 可选外部事务（合并事务内联动 resolved 时传入）。
/// * `scan_id` - 发现项 id。
/// * `from` - 期望的当前状态（CAS 期望值，通常 `"pending"`）。
/// * `to` - 目标状态（`"resolved"` / `"ignored"`）。
/// * `operated_by` - 操作人 id（写入 resolved_by）。
pub async fn transition_scan_status(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    scan_id: i64,
    from: &str,
    to: &str,
    operated_by: i64,
) -> Result<u64, cmx_api_types::Error> {
    // pending→resolved/ignored 均视为"处理完毕"：记 resolved_at + resolved_by
    let sql = "UPDATE md_match_scan SET status = $1, resolved_at = now(), resolved_by = $4 \
               WHERE id = $2 AND status = $3";
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            txn_id,
            sql,
            dv![
                DataValue::String(to.into()),
                DataValue::Int(scan_id),
                DataValue::String(from.into()),
                DataValue::Int(operated_by),
            ],
        )
        .await
        .map_err(|e| api_err_db(&format!("转换 md_match_scan 状态失败: {e}")))?;
    Ok(n)
}

/// cluster_hash：member_ids 升序后 SHA256 前 32 hex 字符。
///
/// 升序保证不同顺序的相同成员集合产生相同 hash（`[3,1,2]` 与 `[1,2,3]` 同 hash）。
/// SHA256 跨重启稳定（不依赖进程），保证去重跨重启有效。
fn cluster_hash(member_ids: &[i64]) -> String {
    let mut sorted: Vec<i64> = member_ids.to_vec();
    sorted.sort_unstable();
    let mut hasher = Sha256::new();
    for id in &sorted {
        hasher.update(id.to_le_bytes());
    }
    let result = hasher.finalize();
    // 取前 16 字节（32 hex 字符）
    hex_encode(&result[..16])
}

/// 简易 hex 编码（避免引入 hex crate）。
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_hash_deterministic_unordered() {
        // 不同顺序的相同成员集合应产生相同 hash
        let a = cluster_hash(&[3, 1, 2]);
        let b = cluster_hash(&[1, 2, 3]);
        let c = cluster_hash(&[3, 2, 1]);
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn cluster_hash_differs_for_different_members() {
        let a = cluster_hash(&[1, 2, 3]);
        let b = cluster_hash(&[1, 2, 4]);
        assert_ne!(a, b);
    }

    #[test]
    fn cluster_hash_length_32() {
        let h = cluster_hash(&[1, 2]);
        assert_eq!(h.len(), 32, "hash 应为 32 hex 字符，实际 {}", h.len());
    }
}
