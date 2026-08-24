//! md_xref 状态切换（merge inactive / unmerge active）。
//!
//! md_xref 无时间戳列——update SQL 不 SET 时间戳。

use cmx_core::dv;
use cmx_core::model::cell::DataValue;
use cmx_database_pg::DatabaseManager;

use crate::error::api_err_db;

/// md_xref 置 inactive（merge 后 victim 引用失效）。不 SET 时间戳。
pub async fn deactivate_xref(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    dict_code: &str,
    record_id: i64,
) -> Result<u64, cmx_api_types::Error> {
    set_xref_status(mm, db_id, txn_id, dict_code, record_id, "inactive").await
}

/// md_xref 恢复 active（unmerge）。
pub async fn activate_xref(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    dict_code: &str,
    record_id: i64,
) -> Result<u64, cmx_api_types::Error> {
    set_xref_status(mm, db_id, txn_id, dict_code, record_id, "active").await
}

/// 改 md_xref 的 xref_status（[`deactivate_xref`] / [`activate_xref`] 的共享实现）。
async fn set_xref_status(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    dict_code: &str,
    record_id: i64,
    status: &str,
) -> Result<u64, cmx_api_types::Error> {
    let sql = "UPDATE md_xref SET xref_status = $1 WHERE dict_code = $2 AND record_id = $3";
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            txn_id,
            sql,
            dv![
                DataValue::String(status.into()),
                DataValue::String(dict_code.into()),
                DataValue::Int(record_id),
            ],
        )
        .await
        .map_err(|e| api_err_db(&format!("改 md_xref 状态失败: {e}")))?;
    Ok(n)
}
