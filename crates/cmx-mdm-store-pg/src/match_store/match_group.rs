//! md_merge_record 读写（合并请求生命周期）。
//!
//! 时间戳：md_merge_record 仅 created_at（DEFAULT now()），update SQL **不 SET 时间戳**。

use cmx_core::dv;
use cmx_core::model::cell::{DataValue, SqlTypeMarker};
use cmx_database_pg::DatabaseManager;
use cmx_utils::next_pk_id;
use serde_json::Value;

use crate::error::api_err_db;

/// 写 md_merge_record 一条。返回新建 id。
#[allow(clippy::too_many_arguments)]
pub async fn insert_match_group(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    dict_code: &str,
    group_key: &str,
    member_ids: &Value,
    master_id: Option<i64>,
    score: i64,
    decision: &str,
    status: &str,
) -> Result<i64, cmx_api_types::Error> {
    let id = next_pk_id();
    // score 列 SMALLINT：DataValue::Int 走 PgInt 宽度自适应 INT2/4/8，可直绑
    let sql = r#"INSERT INTO md_merge_record
        (id, dict_code, group_key, member_ids, master_id, score, decision, survivorship_log, status, created_at)
      VALUES ($1,$2,$3,$4,$5,$6,$7,NULL,$8,now())"#;
    mm.execute_sql_with_datavalues(
        db_id,
        txn_id,
        sql,
        dv![
            DataValue::Int(id),
            DataValue::String(dict_code.into()),
            DataValue::String(group_key.into()),
            DataValue::Json(member_ids.to_string()),
            DataValue::from(master_id),
            DataValue::Int(score),
            DataValue::String(decision.into()),
            DataValue::String(status.into()),
        ],
    )
    .await
    .map_err(|e| api_err_db(&format!("写 md_merge_record 失败: {e}")))?;
    Ok(id)
}

/// 更新 md_merge_record（status / survivorship_log / master_id）。不 SET 时间戳。
pub async fn update_match_group(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    id: i64,
    status: &str,
    survivorship_log: Option<&Value>,
    master_id: Option<i64>,
) -> Result<u64, cmx_api_types::Error> {
    let sql = "UPDATE md_merge_record SET status = $1, survivorship_log = $2, master_id = $3 WHERE id = $4";
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            txn_id,
            sql,
            dv![
                DataValue::String(status.into()),
                survivorship_log
                    .map(|v| DataValue::Json(v.to_string()))
                    .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Json)),
                DataValue::from(master_id),
                DataValue::Int(id),
            ],
        )
        .await
        .map_err(|e| api_err_db(&format!("更新 md_merge_record 失败: {e}")))?;
    Ok(n)
}

/// 状态 CAS 转换（M4 审查 C3/C6）：仅当当前 status=from 才改 to。返回行数（0=冲突）。
///
/// 不 SET 时间戳。用于 reject(pending→rejected) / merge 占位(pending→reviewed)。
pub async fn transition_match_group(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    id: i64,
    from: &str,
    to: &str,
) -> Result<u64, cmx_api_types::Error> {
    let sql = "UPDATE md_merge_record SET status = $1 WHERE id = $2 AND status = $3";
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            txn_id,
            sql,
            dv![
                DataValue::String(to.into()),
                DataValue::Int(id),
                DataValue::String(from.into()),
            ],
        )
        .await
        .map_err(|e| api_err_db(&format!("转换 md_merge_record 状态失败: {e}")))?;
    Ok(n)
}

/// 匹配组列表（dictCode + status 双过滤，吃 `(dict_code, status)` 索引）。
pub async fn list_match_groups(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: Option<&str>,
    status: Option<&str>,
    exclude_statuses: Option<&[&str]>,
    // 名称搜索命中的主数据 id（D-05）：非空时仅返回 master_id 或 member_ids 命中其一的记录
    name_match_ids: Option<&[i64]>,
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
    if let Some(excl) = exclude_statuses
        && !excl.is_empty()
    {
        let ph: Vec<String> = excl
            .iter()
            .map(|s| {
                let p = format!("${}", params.len() + 1);
                params.push(DataValue::String((*s).into()));
                p
            })
            .collect();
        clauses.push(format!("status NOT IN ({})", ph.join(", ")));
    }
    // 名称搜索命中 id（D-05）：master_id 命中其一 或 member_ids(JSONB 数组) 含任一命中 id。
    // id 集合通常很小（名称模糊匹配命中数），绑定两遍（master IN + member EXISTS IN）。
    if let Some(ids) = name_match_ids
        && !ids.is_empty()
    {
        let m_ph: Vec<String> = ids
            .iter()
            .map(|i| {
                let p = format!("${}", params.len() + 1);
                params.push(DataValue::Int(*i));
                p
            })
            .collect();
        let l_ph: Vec<String> = ids
            .iter()
            .map(|i| {
                let p = format!("${}", params.len() + 1);
                params.push(DataValue::Int(*i));
                p
            })
            .collect();
        clauses.push(format!(
            "(master_id IN ({}) OR EXISTS (SELECT 1 FROM jsonb_array_elements_text(member_ids) AS mid WHERE mid::bigint IN ({})))",
            m_ph.join(", "),
            l_ph.join(", ")
        ));
    }
    let where_sql = clauses.join(" AND ");
    // 总数
    let cnt_sql = format!("SELECT COUNT(*) AS c FROM md_merge_record WHERE {where_sql}");
    let cds = mm
        .query_sql_with_datavalues(db_id, None, &cnt_sql, params.clone(), "mdm_match_count")
        .await
        .map_err(|e| api_err_db(&format!("查 md_merge_record 总数失败: {e}")))?;
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
        "SELECT id, dict_code, group_key, member_ids, master_id, score, decision, status, created_at \
         FROM md_merge_record WHERE {where_sql} ORDER BY created_at DESC, id DESC \
         LIMIT ${} OFFSET ${}",
        n + 1,
        n + 2
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_match_list")
        .await
        .map_err(|e| api_err_db(&format!("列表 md_merge_record 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    Ok((
        ds.rows.iter().map(|r| r.to_json_value(schema)).collect(),
        total,
    ))
}

/// 按 status 聚合计数（管家工作台 summary 用）。
///
/// `dict_code` 为 `Some` 时按字典过滤；`None` 全表。吃 `(dict_code, status)` 索引。
/// 返回 `status → 数量`；未出现的 status 调用方默认 0。
pub async fn count_merge_by_status(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: Option<&str>,
) -> Result<std::collections::HashMap<String, i64>, cmx_api_types::Error> {
    let sql = if dict_code.is_some() {
        "SELECT status, COUNT(*)::bigint AS c FROM md_merge_record \
         WHERE dict_code = $1 GROUP BY status"
    } else {
        "SELECT status, COUNT(*)::bigint AS c FROM md_merge_record GROUP BY status"
    };
    let params: Vec<DataValue> = match dict_code {
        Some(d) => vec![DataValue::String(d.into())],
        None => vec![],
    };
    let ds = mm
        .query_sql_with_datavalues(db_id, None, sql, params, "mdm_merge_count_by_status")
        .await
        .map_err(|e| api_err_db(&format!("聚合 md_merge_record 计数失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = std::collections::HashMap::new();
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

/// 按 id 查 md_merge_record。
pub async fn get_match_group(
    mm: &DatabaseManager,
    db_id: &str,
    id: i64,
) -> Result<Option<Value>, cmx_api_types::Error> {
    let sql = "SELECT id, dict_code, group_key, member_ids, master_id, score, decision, status, survivorship_log \
               FROM md_merge_record WHERE id = $1";
    let ds = mm
        .query_sql_with_datavalues(db_id, None, sql, dv![DataValue::Int(id)], "mdm_match_get")
        .await
        .map_err(|e| api_err_db(&format!("查 md_merge_record 失败: {e}")))?;
    let Some(row) = ds.rows.first() else {
        return Ok(None);
    };
    Ok(Some(row.to_json_value(ds.schema.as_ref())))
}
