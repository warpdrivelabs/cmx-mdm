//! cm_* 主数据写入闸口（激活器唯一入口，强制 lifecycle_status='published'）。
//!
//! 自己拼 SQL + DatabaseManager 事务执行（不复用 cmx-dct-store-pg：要纳入激活器单事务 + 强制 published）。
//! INSERT/UPDATE SQL 由 [`crate::sql_builder`] 的 `build_insert_sql` / `build_update_sql` 构造；
//! 列值经 `to_dv` 转 DataValue。

use cmx_core::model::cell::{DataValue, SqlTypeMarker};
use cmx_database_pg::DatabaseManager;
use cmx_utils::next_pk_id;
use serde_json::{Map, Value};

use crate::error::{api_err, api_err_db};
use crate::sql_builder::{build_insert_sql, build_update_line_sql, build_update_sql};

/// 新建主数据行（INSERT，头表/明细表共用）。返回新 id。
///
/// row 已含 lifecycle_status='published'（由 plan_create/plan_lines 强制）。
/// 补 id（next_pk_id）+ backfill 公共 NOT NULL 列（sort_no/status/create_time/...，
/// 对齐 cmx-dct-store-pg 的 backfill 语义，避免 NOT NULL 列缺失导致插入失败）。
pub async fn insert_header(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    table: &str,
    row: &Map<String, Value>,
    operated_by: i64,
) -> Result<i64, cmx_api_types::Error> {
    validate_ident(table)?;
    let id = next_pk_id();
    // backfill:对缺失的公共列补默认值(不覆盖 row 已有值)。先查目标表实际列,只补目标表
    // 拥有的列——避免给无 code/name 的明细表补这两列导致 INSERT「column does not exist」(D-07)。
    // 注:create_time/update_time 由 build_insert_sql 用 SQL now() 填充,这里只占位(若列存在)。
    let cols = load_table_columns(mm, db_id, Some(txn_id), table).await?;
    let mut full = row.clone();
    let id_str = id.to_string();
    // code/name 属 dictionaryCommonFields:头表已由 plan_create/dict_upsert 填;明细表若有此两列且 CR 未提供则补占位
    backfill_col(&cols, &mut full, "code", serde_json::json!(format!("MDM-{id_str}")));
    backfill_col(&cols, &mut full, "name", serde_json::json!(format!("MDM-{id_str}")));
    backfill_col(&cols, &mut full, "published_version", serde_json::json!(1));
    backfill_col(&cols, &mut full, "sort_no", serde_json::json!(0));
    backfill_col(&cols, &mut full, "status", serde_json::json!(1));
    backfill_col(&cols, &mut full, "create_by", serde_json::json!(operated_by));
    backfill_col(&cols, &mut full, "update_by", serde_json::json!(operated_by));
    backfill_col(&cols, &mut full, "create_time", serde_json::Value::Null);
    backfill_col(&cols, &mut full, "update_time", serde_json::Value::Null);
    let (sql, params) = build_insert_sql(table, &full, id);
    mm.execute_sql_with_datavalues(db_id, Some(txn_id), &sql, params)
        .await
        .map_err(|e| {
            tracing::error!(target: "cmx_mdm::db", table=table, sql=%sql, error=%e, "INSERT 失败");
            // 原始错误文本必须带给 api_err_db：classify_db_error 靠它识别唯一约束冲突等
            // （吞掉会退化成「数据保存失败：请检查数据后重试」，如明细账号撞 uk_ 约束时不可排查）。
            api_err_db(&format!("INSERT {table} 失败: {e}"))
        })?;
    Ok(id)
}

/// 若 row 无该列**且目标表实际拥有此列**，则补默认值（不覆盖已有，不补目标表没有的列）。
fn backfill_col(
    cols: &std::collections::HashSet<String>,
    row: &mut Map<String, Value>,
    col: &str,
    default: Value,
) {
    if cols.contains(col) && !row.contains_key(col) {
        row.insert(col.to_string(), default);
    }
}

/// 查目标表的列名集合（information_schema.columns）。
///
/// 供 [`insert_header`] 判断哪些 backfill 列目标表实际拥有——避免给无 `code`/`name`
/// 的明细表补这两列导致 INSERT「column does not exist」失败（D-07）。
async fn load_table_columns(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    table: &str,
) -> Result<std::collections::HashSet<String>, cmx_api_types::Error> {
    let sql = "SELECT column_name FROM information_schema.columns WHERE table_name = $1";
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            txn_id,
            sql,
            vec![DataValue::String(table.into())],
            "mdm_table_cols",
        )
        .await
        .map_err(|e| api_err_db(&format!("查 {table} 列失败: {e}")))?;
    let schema = ds.schema.as_ref();
    Ok(ds
        .rows
        .iter()
        .filter_map(|r| r.get_by_name_as::<String>(schema, "column_name"))
        .collect())
}

/// 按名称模糊匹配主数据 id（合并历史名称搜索用，D-05）。
///
/// 在 `{table}.name` 上做 `ILIKE '%kw%'`，返回命中 id。目标表无 `name` 列时返回空 Vec
/// （防御性：合并的 cm_* 主数据表通常都有 name，但仍避免无 name 列的表 SQL 报错）。
///
/// `kw` 作为绑定参数 `%kw%` 传入，无 SQL 注入风险；kw 中的 `%`/`_` 按 ILIKE 通配语义
/// 处理（搜索词罕见此类字符，先不做 ESCAPE 转义）。
pub async fn find_ids_by_name_like(
    mm: &DatabaseManager,
    db_id: &str,
    table: &str,
    kw: &str,
) -> Result<Vec<i64>, cmx_api_types::Error> {
    validate_ident(table)?;
    let cols = load_table_columns(mm, db_id, None, table).await?;
    if !cols.contains("name") {
        return Ok(Vec::new());
    }
    let sql = format!("SELECT id FROM {table} WHERE name ILIKE $1");
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            None,
            &sql,
            vec![DataValue::String(format!("%{kw}%").into())],
            "mdm_find_ids_by_name",
        )
        .await
        .map_err(|e| api_err_db(&format!("按名称查 {table} 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut ids = Vec::with_capacity(ds.rows.len());
    for row in ds.rows.iter() {
        if let Some(id) = row.get_by_name_as::<i64>(schema, "id") {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// 变更主数据头（UPDATE by id + 乐观锁 CAS）。返回受影响行数（0=版本冲突）。
pub async fn update_header(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    table: &str,
    record_id: i64,
    row: &Map<String, Value>,
    expected_version: i64,
) -> Result<u64, cmx_api_types::Error> {
    validate_ident(table)?;
    let (sql, params) = build_update_sql(table, record_id, row, expected_version);
    let n = mm
        .execute_sql_with_datavalues(db_id, Some(txn_id), &sql, params)
        .await
        .map_err(|e| api_err_db(&format!("UPDATE {table} 失败: {e}")))?;
    Ok(n)
}

/// 明细 update（diff 方案）：按 id UPDATE 业务字段 + `update_time=now()` + `published_version+1`，
/// `WHERE id=$ AND lifecycle_status='published'`（不 CAS——明细在激活器单事务内，CR 互斥已保护头）。
/// 返回受影响行数（0 = 明细已非 published 状态）。
pub async fn update_line(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    table: &str,
    line_id: i64,
    row: &Map<String, Value>,
) -> Result<u64, cmx_api_types::Error> {
    validate_ident(table)?;
    let (sql, params) = build_update_line_sql(table, line_id, row);
    let n = mm
        .execute_sql_with_datavalues(db_id, Some(txn_id), &sql, params)
        .await
        .map_err(|e| api_err_db(&format!("UPDATE {table} 明细失败: {e}")))?;
    Ok(n)
}

/// 查当前 published_version（乐观锁快照用）。无记录返回 None。
pub async fn get_version(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    table: &str,
    record_id: i64,
) -> Result<Option<i64>, cmx_api_types::Error> {
    validate_ident(table)?;
    let sql = format!("SELECT published_version FROM {table} WHERE id = $1");
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            txn_id,
            &sql,
            vec![DataValue::Int(record_id)],
            "mdm_get_version",
        )
        .await
        .map_err(|e| api_err_db(&format!("查 {table} 版本失败: {e}")))?;
    let Some(row) = ds.rows.first() else {
        return Ok(None);
    };
    Ok(row.get_by_name_as::<i64>(ds.schema.as_ref(), "published_version"))
}

/// 读头表单行的 BIGINT 列值（激活器 update 分支树形补偿重算取旧 parent_id 用：
/// 节点移父后旧父可能变回叶子，is_leaf 修正需要旧父进重算集合）。
/// 列名与表名均走 `validate_ident` 白名单校验。
pub async fn select_bigint_col(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    table: &str,
    col: &str,
    record_id: i64,
) -> Result<Option<i64>, cmx_api_types::Error> {
    validate_ident(table)?;
    validate_ident(col)?;
    let sql = format!("SELECT {col} FROM {table} WHERE id = $1");
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            txn_id,
            &sql,
            vec![DataValue::Int(record_id)],
            "mdm_select_bigint_col",
        )
        .await
        .map_err(|e| api_err_db(&format!("查 {table}.{col} 失败: {e}")))?;
    let Some(row) = ds.rows.first() else {
        return Ok(None);
    };
    Ok(row.get_by_name_as::<i64>(ds.schema.as_ref(), col))
}

/// 改 lifecycle_status（merge→merged / unmerge→published / freeze 等，M3）。
///
/// **双保险**（审查重要-5）：① CAS `expected→next` 防双 merge / 双 unmerge；
/// ② SET 同时 `published_version+1`，使并发持旧版本的 update-CR 写入者 CAS 失配回滚。
/// 返回受影响行数（0 = 状态冲突）。
pub async fn set_lifecycle(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    table: &str,
    record_id: i64,
    expected: &str,
    next: &str,
) -> Result<u64, cmx_api_types::Error> {
    validate_ident(table)?;
    let sql = format!(
        "UPDATE {table} SET lifecycle_status = $1, published_version = published_version + 1, \
         update_time = now() WHERE id = $2 AND lifecycle_status = $3"
    );
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            Some(txn_id),
            &sql,
            vec![
                DataValue::String(next.into()),
                DataValue::Int(record_id),
                DataValue::String(expected.into()),
            ],
        )
        .await
        .map_err(|e| api_err_db(&format!("改 {table} 生命周期失败: {e}")))?;
    Ok(n)
}

/// 明细 re-parent（M3 merge）：detail 表里 parent_field=from_id 的行改指 to_id。返回行数。
pub async fn reparent_lines(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    detail_table: &str,
    parent_field: &str,
    from_id: i64,
    to_id: i64,
) -> Result<u64, cmx_api_types::Error> {
    validate_ident(detail_table)?;
    validate_ident(parent_field)?;
    let sql = format!(
        "UPDATE {detail_table} SET {parent_field} = $1, update_time = now() \
         WHERE {parent_field} = $2"
    );
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            Some(txn_id),
            &sql,
            vec![DataValue::Int(to_id), DataValue::Int(from_id)],
        )
        .await
        .map_err(|e| api_err_db(&format!("re-parent {detail_table} 失败: {e}")))?;
    Ok(n)
}

/// 查明细行 id 清单（M3）：detail 表里 parent_field=parent_id 的行 id（merge 前快照，供 unmerge 逆操作）。
pub async fn select_line_ids(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    detail_table: &str,
    parent_field: &str,
    parent_id: i64,
) -> Result<Vec<i64>, cmx_api_types::Error> {
    validate_ident(detail_table)?;
    validate_ident(parent_field)?;
    let sql = format!("SELECT id FROM {detail_table} WHERE {parent_field} = $1 ORDER BY id");
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            Some(txn_id),
            &sql,
            vec![DataValue::Int(parent_id)],
            "mdm_line_ids",
        )
        .await
        .map_err(|e| api_err_db(&format!("查 {detail_table} 行 id 失败: {e}")))?;
    Ok(ds
        .rows
        .iter()
        .filter_map(|r| r.get_by_name_as::<i64>(ds.schema.as_ref(), "id"))
        .collect())
}

/// 按 id 精确 re-parent（M3 unmerge 逆操作）：把 ids 行改指 to_id。返回行数。
pub async fn reparent_lines_by_ids(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    detail_table: &str,
    parent_field: &str,
    ids: &[i64],
    to_id: i64,
) -> Result<u64, cmx_api_types::Error> {
    if ids.is_empty() {
        return Ok(0);
    }
    validate_ident(detail_table)?;
    validate_ident(parent_field)?;
    // IN 列表用 $2..$N+1 参数化（防注入）
    let placeholders: Vec<String> = (2..=ids.len() + 1).map(|i| format!("${i}")).collect();
    let sql = format!(
        "UPDATE {detail_table} SET {parent_field} = $1, update_time = now() \
         WHERE id IN ({})",
        placeholders.join(", ")
    );
    let mut params = vec![DataValue::Int(to_id)];
    params.extend(ids.iter().map(|i| DataValue::Int(*i)));
    let n = mm
        .execute_sql_with_datavalues(db_id, Some(txn_id), &sql, params)
        .await
        .map_err(|e| api_err_db(&format!("按 id re-parent {detail_table} 失败: {e}")))?;
    Ok(n)
}

/// 查某 parent 名下全部 published 明细的 id + 指定业务键列值（合并去重用，一次查询）。
///
/// 合并去重需比对 master 与 victim 的明细业务键是否相同：本函数一次查出某 parent 名下
/// 全部 `published` 明细行的 id 及其业务键列值，供应用层内存比对（避免逐行查库）。
/// `key_cols` 为业务键列名（如 `["account_no"]`），全部过 [`validate_ident`] 防注入。
/// 返回 `(行 id, 业务键值向量)`，值向量顺序与 `key_cols` 一致；`key_cols` 为空时只查 id。
pub async fn select_line_keys(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    detail_table: &str,
    parent_field: &str,
    parent_id: i64,
    key_cols: &[&str],
) -> Result<Vec<(i64, Vec<Value>)>, cmx_api_types::Error> {
    validate_ident(detail_table)?;
    validate_ident(parent_field)?;
    for k in key_cols {
        validate_ident(k)?;
    }
    // SELECT id, k1, k2, ... FROM {table} WHERE {parent_field}=$1 AND lifecycle_status='published'
    let cols = std::iter::once("id".to_string())
        .chain(key_cols.iter().map(|s| s.to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {cols} FROM {detail_table} \
         WHERE {parent_field} = $1 AND lifecycle_status = 'published' ORDER BY id"
    );
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            Some(txn_id),
            &sql,
            vec![DataValue::Int(parent_id)],
            "mdm_line_keys",
        )
        .await
        .map_err(|e| api_err_db(&format!("查 {detail_table} 业务键失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = Vec::with_capacity(ds.rows.len());
    for row in &ds.rows {
        let v = row.to_json_value(schema);
        let Some(id) = v.get("id").and_then(|x| x.as_i64()) else { continue };
        let vals = key_cols
            .iter()
            .map(|k| v.get(*k).cloned().unwrap_or(Value::Null))
            .collect();
        out.push((id, vals));
    }
    Ok(out)
}

/// 按 id 批量改 lifecycle（合并去重软删 victim 重复明细用）。
///
/// 语义对齐单条 [`set_lifecycle`]：CAS `expected→next` + `published_version+1`，只是作用于一组
/// id（IN 列表参数化，参考 [`reparent_lines_by_ids`]）。去重时把 victim 与 master 业务键
/// 冲突的明细行批量置 `merged`（软删，parent 仍是 victim，unmerge 时 CAS 回 published 即恢复）。
/// 返回受影响行数。
pub async fn set_lifecycle_by_ids(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    table: &str,
    ids: &[i64],
    expected: &str,
    next: &str,
) -> Result<u64, cmx_api_types::Error> {
    if ids.is_empty() {
        return Ok(0);
    }
    validate_ident(table)?;
    // $1=next, $2=expected, $3..$N+2=ids
    let placeholders: Vec<String> = (3..=ids.len() + 2).map(|i| format!("${i}")).collect();
    let sql = format!(
        "UPDATE {table} SET lifecycle_status = $1, published_version = published_version + 1, \
         update_time = now() WHERE lifecycle_status = $2 AND id IN ({})",
        placeholders.join(", ")
    );
    let mut params = vec![DataValue::String(next.into()), DataValue::String(expected.into())];
    params.extend(ids.iter().map(|i| DataValue::Int(*i)));
    let n = mm
        .execute_sql_with_datavalues(db_id, Some(txn_id), &sql, params)
        .await
        .map_err(|e| api_err_db(&format!("批量改 {table} lifecycle 失败: {e}")))?;
    Ok(n)
}

/// 锁行（M3 merge，审查重要-4）：事务内 `SELECT ... FOR UPDATE` 占行锁，
/// 串行化交叉 merge（X⇄Y 互并）。返回行是否存在。
pub async fn lock_record(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: &str,
    table: &str,
    record_id: i64,
) -> Result<bool, cmx_api_types::Error> {
    validate_ident(table)?;
    let sql = format!("SELECT id FROM {table} WHERE id = $1 FOR UPDATE");
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            Some(txn_id),
            &sql,
            vec![DataValue::Int(record_id)],
            "mdm_lock_record",
        )
        .await
        .map_err(|e| api_err_db(&format!("锁 {table} 行失败: {e}")))?;
    Ok(!ds.rows.is_empty())
}

/// 标识符（表名/列名）白名单校验：仅允许 [a-zA-Z0-9_]，防 SQL 注入。
/// pub(crate)：match_store 复用（审查建议-6，不复制第二份）。
pub(crate) fn validate_ident(name: &str) -> Result<(), cmx_api_types::Error> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(api_err(&format!("非法标识符: {name}")));
    }
    Ok(())
}

/// 把 Option<DataValue> 的 None 转成带类型的 Null（供外部用）。
#[allow(dead_code)]
fn typed_null(marker: SqlTypeMarker) -> DataValue {
    DataValue::NullTyped(marker)
}

/// 事务内单行回读（SELECT * → JSON；激活器发事件前取全量快照用）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `txn_id` - 事务 id（事务内可见本事务未提交的写入）。
/// * `table` - 目标表名。
/// * `id` - 记录 id。
///
/// # Returns
///
/// 行 JSON（列名 → 值）；记录不存在返回 None。
///
/// # Errors
///
/// SQL 失败时返回错误。
pub async fn select_row_json(
    mm: &cmx_database_pg::DatabaseManager,
    db_id: &str,
    txn_id: &str,
    table: &str,
    id: i64,
) -> Result<Option<serde_json::Value>, cmx_api_types::Error> {
    use cmx_core::dv;
    use cmx_core::model::cell::DataValue;
    let sql = format!("SELECT * FROM {table} WHERE id = $1");
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            Some(txn_id),
            &sql,
            dv![DataValue::Int(id)],
            "mdm_select_row_json",
        )
        .await
        .map_err(|e| crate::error::api_err_db(&format!("回读 {table}#{id} 失败: {e}")))?;
    Ok(ds.rows.first().map(|r| r.to_json_value(ds.schema.as_ref())))
}
