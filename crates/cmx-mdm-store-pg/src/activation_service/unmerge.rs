//! 还原主流程：victim merged→published、明细指回、xref active、group=unmerged。
//!
//! master 存活值不回退（仅留痕），避免数据抖动。双 unmerge 第二笔 CAS n=0 报错。

use cmx_database_pg::DatabaseManager;
use serde_json::{json, Value};

use crate::{
    activation_store::LineTableInfo, dct_accessor, error::api_err, match_store, md_accessor,
};

/// unmerge：反向还原（victim merged→published、明细指回、xref active、group=unmerged）。
///
/// master 存活值不回退（仅留痕），避免数据抖动。双 unmerge 第二笔 CAS n=0 报错。
///
/// # Arguments
///
/// * `mm` / `db_id` - 数据库管理器与数据源 id。
/// * `dict_code` - 字典代码。
/// * `head_table` - 目标头物理表。
/// * `master_id` - 存活记录 id（审计留痕用）。
/// * `victim_id` - 待还原的 victim 记录 id。
/// * `line_tables` - 明细表清单（[`LineTableInfo`]），用于还原 reparent + 去重软删。
/// * `operated_by` - 操作人 id。
/// * `match_group_id` - 合并请求 group id（→ unmerged）。
///
/// # Errors
///
/// victim 非 merged（双 unmerge 拦截）、group 不存在、审计写入失败等任一步出错时返回错误。
#[allow(clippy::too_many_arguments)]
pub async fn unmerge(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: &str,
    head_table: &str,
    master_id: i64,
    victim_id: i64,
    line_tables: &[LineTableInfo],
    operated_by: i64,
    match_group_id: i64,
) -> Result<(), cmx_api_types::Error> {
    let txn_ctx = mm.get_transaction_context();
    let guard = txn_ctx
        .begin_with_guard(db_id)
        .await
        .map_err(|e| api_err(&format!("开事务失败: {e}")))?;
    let txn_id = guard.txn_id().to_string();

    let result = unmerge_inner(
        mm, db_id, &txn_id, dict_code, head_table, master_id, victim_id,
        line_tables, operated_by, match_group_id,
    )
    .await;

    match result {
        Ok(()) => {
            guard
                .commit()
                .await
                .map_err(|e| api_err(&format!("提交事务失败: {e}")))?;
            Ok(())
        }
        Err(e) => {
            tracing::error!(target: "cmx_mdm::merge", master_id, victim_id, error = %e, "还原失败,事务已回滚");
            Err(e)
        }
    }
}

/// 还原编排（事务内执行，由 [`unmerge`] 开事务后调用）。
#[allow(clippy::too_many_arguments)]
async fn unmerge_inner(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    dict_code: &str,
    head_table: &str,
    master_id: i64,
    victim_id: i64,
    line_tables: &[LineTableInfo],
    operated_by: i64,
    match_group_id: i64,
) -> Result<(), cmx_api_types::Error> {
    // victim merged→published（CAS，双 unmerge 拦截）
    let n = dct_accessor::set_lifecycle(mm, db_id, txn_id, head_table, victim_id, "merged", "published")
        .await?;
    if n == 0 {
        return Err(api_err(&format!("victim {victim_id} 非 merged（双 unmerge 拦截）")));
    }

    // 读 group 的 survivorship_log.reparented，按 id 指回 victim
    // （JSONB 列 to_json_value 为转义字符串，需 parse）
    let group = match_store::get_match_group(mm, db_id, match_group_id).await?;
    let slog_raw = group.as_ref().and_then(|g| g.get("survivorship_log")).cloned();
    let slog = match slog_raw {
        Some(Value::String(s)) => serde_json::from_str::<Value>(&s).unwrap_or(Value::Null),
        Some(v) => v,
        None => Value::Null,
    };
    let reparented = slog.get("reparented").cloned().unwrap_or(json!({}));
    let deduped = slog.get("deduped").cloned().unwrap_or(json!({}));
    for lt in line_tables {
        let table = lt.table.as_str();
        let pf = lt.parent_field.as_str();
        // 还原 reparent：迁移到 master 的行指回 victim
        let ids: Vec<i64> = reparented
            .get(table)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if !ids.is_empty() {
            dct_accessor::reparent_lines_by_ids(mm, db_id, txn_id, table, pf, &ids, victim_id)
                .await?;
        }
        // 还原 dedup：去重时软删(merged)的行 CAS 回 published（parent 仍是 victim，无需 reparent）
        let dids: Vec<i64> = deduped
            .get(table)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if !dids.is_empty() {
            dct_accessor::set_lifecycle_by_ids(mm, db_id, txn_id, table, &dids, "merged", "published")
                .await?;
        }
    }

    // xref active
    match_store::activate_xref(mm, db_id, Some(txn_id), dict_code, victim_id).await?;

    // 审计 + group=unmerged
    md_accessor::write_audit(
        mm, db_id, txn_id, dict_code, master_id, 0, "unmerge",
        Some(victim_id), None, None, None, operated_by,
    )
    .await?;
    match_store::update_match_group(mm, db_id, Some(txn_id), match_group_id, "unmerged", None, None)
        .await?;

    Ok(())
}
