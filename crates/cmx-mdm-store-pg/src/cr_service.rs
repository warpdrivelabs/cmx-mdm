//! CR 变更请求:状态校验 + 列表/详情/作废。
//!
//! 状态转移强校验(封禁非法跳转)。approve 由 api 层直接调激活器(M2-0 方案 A:
//! 激活器接受 approving,单事务 approving→activated)。
//!
//! 状态机:
//!   draft ──submit──→ approving ──approve(激活器)──→ activated(归档)
//!                          └──reject──→ rejected(归档)
//!   rejected ──submit──→ approving（驳回后可直接编辑重新提交）
//!   draft ──abort──→ aborted(作废)

use cmx_core::model::cell::DataValue;
use cmx_database_pg::DatabaseManager;
use serde_json::{json, Value};

use crate::error::{api_err, parse_jsonb_field};
use crate::md_accessor::set_cr_status;

/// 校验 CR 当前状态,返回头 Map。状态不符报错。
pub async fn check_status(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    cr_id: i64,
    expect: &str,
) -> Result<serde_json::Map<String, Value>, cmx_api_types::Error> {
    let head = crate::doc_accessor::load_cr_head(mm, db_id, txn_id, cr_id).await?;
    let cur = head.get("doc_status").and_then(|v| v.as_str()).unwrap_or("");
    if cur != expect {
        return Err(api_err(&format!(
            "CR {cr_id} 状态「{cur}」不符(须 {expect})"
        )));
    }
    Ok(head)
}

/// 校验 CR 当前状态在允许集合内，返回头 Map。状态不符报错。
/// 用于跨状态操作（如 submit 允许 draft / rejected）。
pub async fn check_status_in(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    cr_id: i64,
    expect: &[&str],
) -> Result<serde_json::Map<String, Value>, cmx_api_types::Error> {
    let head = crate::doc_accessor::load_cr_head(mm, db_id, txn_id, cr_id).await?;
    let cur = head.get("doc_status").and_then(|v| v.as_str()).unwrap_or("");
    if !expect.contains(&cur) {
        return Err(api_err(&format!(
            "CR {cr_id} 状态「{cur}」不符(须 {})",
            expect.join(" 或 ")
        )));
    }
    Ok(head)
}

/// CR 列表（分页，返回 total）。可选过滤 docStatus / docType / keyword（单据号·主体名模糊）。
/// 返回 (list, total)。page 从 1 起；page_size<=0 时默认 20。
pub async fn list_cr(
    mm: &DatabaseManager,
    db_id: &str,
    doc_status: Option<&str>,
    doc_type: Option<&str>,
    keyword: Option<&str>,
    page: i64,
    page_size: i64,
) -> Result<(Vec<Value>, i64), cmx_api_types::Error> {
    // 动态过滤：doc_status / doc_type / keyword 均可选，占位符序号随入参递增
    let mut conds = vec!["delete_flag = 0".to_string()];
    let mut params: Vec<DataValue> = Vec::new();
    if let Some(st) = doc_status {
        params.push(DataValue::String(st.into()));
        conds.push(format!("doc_status = ${}", params.len()));
    }
    if let Some(dt) = doc_type {
        params.push(DataValue::String(dt.into()));
        conds.push(format!("doc_type = ${}", params.len()));
    }
    // keyword：单据号 / 主体名模糊匹配（与列表页搜索框 placeholder 语义一致）
    if let Some(kw) = keyword.map(str::trim).filter(|k| !k.is_empty()) {
        params.push(DataValue::String(format!("%{kw}%")));
        let n = params.len();
        conds.push(format!("(doc_no ILIKE ${n} OR subject_name ILIKE ${n})"));
    }
    let where_sql = conds.join(" AND ");
    // 总数
    let cnt_sql = format!("SELECT COUNT(*) AS c FROM cv_mdm_apply WHERE {where_sql}");
    let cds = mm
        .query_sql_with_datavalues(db_id, None, &cnt_sql, params.clone(), "mdm_cr_count")
        .await
        .map_err(|e| api_err(&format!("查 CR 总数失败: {e}")))?;
    let total = cds.rows.first()
        .and_then(|r| r.get_by_name_as::<i64>(cds.schema.as_ref(), "c"))
        .unwrap_or(0);
    // 分页
    let ps = if page_size > 0 { page_size } else { 20 };
    let pg = if page > 0 { page } else { 1 };
    let off = (pg - 1) * ps;
    let n = params.len() as i64;
    params.push(DataValue::Int(ps));
    params.push(DataValue::Int(off));
    // SELECT *：元数据驱动加业务字段时列表自动带上，无需改 SQL；JSONB 列 parse 成对象
    let sql = format!(
        "SELECT * FROM cv_mdm_apply WHERE {where_sql} ORDER BY create_time DESC \
         LIMIT ${} OFFSET ${}", n + 1, n + 2);
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_cr_list")
        .await
        .map_err(|e| api_err(&format!("查 CR 列表失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let list = ds
        .rows
        .iter()
        .map(|r| {
            let mut v = r.to_json_value(schema);
            parse_jsonb_field(&mut v, "payload");
            parse_jsonb_field(&mut v, "field_deltas");
            v
        })
        .collect();
    Ok((list, total))
}

/// CR 详情(头+行)。
pub async fn get_cr_detail(
    mm: &DatabaseManager,
    db_id: &str,
    cr_id: i64,
) -> Result<Value, cmx_api_types::Error> {
    let head = crate::doc_accessor::load_cr_head(mm, db_id, None, cr_id).await?;
    let lines = crate::doc_accessor::load_cr_lines(mm, db_id, None, cr_id).await?;
    Ok(json!({ "head": head, "lines": lines }))
}

/// 作废 draft CR。draft → aborted。
pub async fn abort_cr(
    mm: &DatabaseManager,
    db_id: &str,
    cr_id: i64,
) -> Result<u64, cmx_api_types::Error> {
    check_status(mm, db_id, None, cr_id, "draft").await?;
    set_cr_status(mm, db_id, None, cr_id, "aborted").await
}
