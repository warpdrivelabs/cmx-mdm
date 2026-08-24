//! 治理表写入：md_audit（版本留痕）+ md_event_log（分发事件）+ CR 状态归档。

use cmx_core::dv;
use cmx_core::model::cell::{DataValue, SqlTypeMarker};
use cmx_database_pg::DatabaseManager;
use cmx_utils::{next_pk_id, snowflake_id_str};
use serde_json::Value;

use crate::error::api_err_db;

/// CR 的 `update_time` 是否早于 `secs` 秒前（懒同步自愈窗口判定：
/// approving 且无实例超过 N 分钟 → 判定 submit 崩溃残留，回退 draft）。
pub async fn cr_updated_before(
    mm: &DatabaseManager,
    db_id: &str,
    cr_id: i64,
    secs: i64,
) -> Result<bool, cmx_api_types::Error> {
    // 间隔用 ($2::int8 * interval '1 second')：裸 $2 会被 PG 推断为 float8（interval 乘法
    // 的 double 重载优先），驱动无法把 i64 绑到 float8 参数；显式 int8 走 int8*interval 重载。
    // make_interval(secs=>) 同理要求 double，也不可用。
    let sql = "SELECT COUNT(*) AS c FROM cv_mdm_apply \
               WHERE id = $1 AND update_time < now() - ($2::int8 * interval '1 second')";
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![DataValue::Int(cr_id), DataValue::Int(secs)],
            "mdm_cr_stale",
        )
        .await
        .map_err(|e| {
            // api_err_db 会把 detail 脱敏成通用短语（brief_db_detail），先落 error 日志保真便于排查。
            tracing::error!(cr_id, secs, error = %e, "cr_updated_before 查询失败（原始错误）");
            api_err_db(&format!("查 CR {cr_id} update_time 时效失败"))
        })?;
    let c = ds
        .rows
        .first()
        .and_then(|r| r.get_by_name_as::<i64>(ds.schema.as_ref(), "c"))
        .unwrap_or(0);
    Ok(c > 0)
}

/// 写 md_audit 一条（create/update 留痕）。返回审计 id。
///
/// 参数多但语义清晰（审计字段：字典/记录/版本/动作/来源CR/字段/新旧值/操作人），不拆。
#[allow(clippy::too_many_arguments)]
pub async fn write_audit(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    dict_code: &str,
    record_id: i64,
    version: i64,
    action: &str,
    source_cr_id: Option<i64>,
    field: Option<&str>,
    old_value: Option<Value>,
    new_value: Option<Value>,
    operated_by: i64,
) -> Result<i64, cmx_api_types::Error> {
    let id = next_pk_id();
    let sql = r#"INSERT INTO md_audit (id, dict_code, record_id, version, action, source_cr_id,
                                       field, old_value, new_value, operated_by, operated_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,now())"#;
    // 可选列走 NullTyped（B5 口径）：Option<i64>→NullTyped(Int)；JSONB 的 None→NullTyped(Json)
    let params = dv![
        DataValue::Int(id),
        DataValue::String(dict_code.into()),
        DataValue::Int(record_id),
        DataValue::Int(version),
        DataValue::String(action.into()),
        // Option<i64> → NullTyped(Int)，BIGINT 列安全（显式 DataValue::from 消除推断歧义）
        DataValue::from(source_cr_id),
        // Option<String> → Null，VARCHAR 列安全
        DataValue::from(field.map(|s| s.to_string())),
        old_value
            .map(|v| DataValue::Json(v.to_string()))
            .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Json)),
        new_value
            .map(|v| DataValue::Json(v.to_string()))
            .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Json)),
        DataValue::Int(operated_by),
    ];
    mm.execute_sql_with_datavalues(db_id, Some(txn_id), sql, params)
        .await
        .map_err(|e| api_err_db(&format!("写 md_audit 失败: {e}")))?;
    Ok(id)
}

/// 写 md_event_log 一条（分发事件）。seq 由 DB 自增，不填。返回事件 id。
pub async fn write_event(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    dict_code: &str,
    record_id: i64,
    event_type: &str,
    payload: Value,
) -> Result<String, cmx_api_types::Error> {
    let id = snowflake_id_str();
    let sql = r#"INSERT INTO md_event_log (id, dict_code, record_id, event_type, payload, emitted_at)
                 VALUES ($1,$2,$3,$4,$5,now())"#;
    let params = dv![
        DataValue::String(id.clone()),
        DataValue::String(dict_code.into()),
        DataValue::Int(record_id),
        DataValue::String(event_type.into()),
        DataValue::Json(payload.to_string()),
    ];
    mm.execute_sql_with_datavalues(db_id, Some(txn_id), sql, params)
        .await
        .map_err(|e| api_err_db(&format!("写 md_event_log 失败: {e}")))?;
    Ok(id)
}

/// 改 CR 状态（归档：activated / approved / aborted 等）。返回受影响行数。
///
/// `txn_id`:激活器传 Some(走事务);CR 审批服务传 None(自动提交)。
pub async fn set_cr_status(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    cr_id: i64,
    status: &str,
) -> Result<u64, cmx_api_types::Error> {
    let sql = "UPDATE cv_mdm_apply SET doc_status = $1 WHERE id = $2";
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            txn_id,
            sql,
            dv![DataValue::String(status.into()), DataValue::Int(cr_id)],
        )
        .await
        .map_err(|e| api_err_db(&format!("改 CR {cr_id} 状态失败: {e}")))?;
    Ok(n)
}

/// **抢占式**改 CR 状态：仅当当前状态在 `from` 集合内才更新为 `to`，返回是否抢占成功
/// （0 行受影响 = 他人已处理 / 状态已变，调用方据此跳过后续动作）。
///
/// M7 流程回写的并发收敛原语：webhook 回调、列表懒同步、手动兜底三方并发时，
/// 同一状态迁移只有一次 `try_set` 成功，无需行锁。同语句刷 `update_time`——
/// 它是懒同步「approving 且无实例超 5 分钟回退 draft」自愈窗口的计时起点（=submit 时刻，
/// 而非上次编辑时间；doc/save 链路只维护后者）。
pub async fn try_set_cr_status(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    cr_id: i64,
    from: &[&str],
    to: &str,
) -> Result<bool, cmx_api_types::Error> {
    if from.is_empty() {
        return Err(api_err_db("try_set_cr_status 的 from 状态集合不能为空"));
    }
    // 动态拼 IN 占位符：$1..$n 为 from 集合，$n+1 为目标状态，$n+2 为 id。
    let placeholders: Vec<String> = (1..=from.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "UPDATE cv_mdm_apply SET doc_status = ${}, update_time = now() \
         WHERE id = ${} AND doc_status IN ({})",
        from.len() + 1,
        from.len() + 2,
        placeholders.join(","),
    );
    let mut params: Vec<DataValue> = from.iter().map(|s| DataValue::String((*s).into())).collect();
    params.push(DataValue::String(to.into()));
    params.push(DataValue::Int(cr_id));
    let n = mm
        .execute_sql_with_datavalues(db_id, txn_id, &sql, params)
        .await
        .map_err(|e| api_err_db(&format!("抢占改 CR {cr_id} 状态失败: {e}")))?;
    Ok(n > 0)
}
