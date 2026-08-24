//! cm_* 主数据写入的 SQL 构造与列值转换工具。
//!
//! 从 [`crate::dct_accessor`] 抽出的纯函数集，供 INSERT/UPDATE 构造参数化 SQL，
//! 避免在 dct_accessor 的写入函数体内混合 SQL 拼接逻辑。

use cmx_core::model::cell::DataValue;
use serde_json::{Map, Value};

/// 拼 INSERT SQL：列含 id + row 的所有列。VALUES 用 `$N` 占位；时间戳列用 `now()`。
///
/// # Arguments
///
/// * `table` - 目标表名（已由调用方经 [`crate::dct_accessor::validate_ident`] 校验）。
/// * `row` - 行字段（key = 列名，value = 列值）。
/// * `id` - 新行主键（占用 `$1`）。
///
/// # Returns
///
/// `(sql, params)`：sql 含 `$N` 占位符，params 为对应 DataValue 列表。
pub(crate) fn build_insert_sql(
    table: &str,
    row: &Map<String, Value>,
    id: i64,
) -> (String, Vec<DataValue>) {
    let mut cols = vec!["id".to_string()];
    let mut params: Vec<DataValue> = vec![DataValue::Int(id)];
    let mut vals = vec!["$1".to_string()]; // id
    let mut idx = 2;
    for (col, val) in row {
        // 时间戳列用 SQL now() 字面量（避免 String→TIMESTAMP 序列化失败）
        if col == "create_time" || col == "update_time" {
            cols.push(col.clone());
            vals.push("now()".to_string());
            continue;
        }
        cols.push(col.clone());
        params.push(to_dv(val));
        vals.push(format!("${idx}"));
        idx += 1;
    }
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({})",
        cols.join(", "),
        vals.join(", ")
    );
    (sql, params)
}

/// 拼 UPDATE CAS SQL：SET row 列，`WHERE id=$ AND published_version=$expected AND lifecycle_status='published'`。
///
/// M3 补强（审查重要-5）：lifecycle 条件使 merged/frozen 行拒绝任何 CAS 写入——
/// merge 把 victim 置 merged 后，并发的 update CR 激活在此处失败回滚。
///
/// # Arguments
///
/// * `table` - 目标表名。
/// * `record_id` - 待更新行 id。
/// * `row` - SET 的列值（key = 列名）。
/// * `expected_version` - 乐观锁期望版本（CAS 条件）。
///
/// # Returns
///
/// `(sql, params)`：sql 含 `$N` 占位符，params 顺序为 SET 列值 + id + expected_version。
pub(crate) fn build_update_sql(
    table: &str,
    record_id: i64,
    row: &Map<String, Value>,
    expected_version: i64,
) -> (String, Vec<DataValue>) {
    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<DataValue> = Vec::new();
    let mut idx = 1;
    for (col, val) in row {
        sets.push(format!("{col} = ${idx}"));
        params.push(to_dv(val));
        idx += 1;
    }
    // WHERE 条件三个：id + published_version（CAS）+ lifecycle_status（M3 补强）
    let id_idx = idx;
    let ver_idx = idx + 1;
    params.push(DataValue::Int(record_id));
    params.push(DataValue::Int(expected_version));
    let sql = format!(
        "UPDATE {table} SET {} WHERE id = ${id_idx} AND published_version = ${ver_idx} \
         AND lifecycle_status = 'published'",
        sets.join(", ")
    );
    (sql, params)
}

/// 拼 UPDATE 明细 SQL（diff 方案）：SET row 业务列 + `update_time=now()` + `published_version+1`，
/// `WHERE id=$ AND lifecycle_status='published'`。
///
/// 与 [`build_update_sql`] 区别：**不带 CAS**（明细在激活器单事务内，CR 互斥已保护头），
/// `published_version` 由 SQL 自增（保持乐观锁语义）。`create_time` 跳过（创建时间不变）。
pub(crate) fn build_update_line_sql(
    table: &str,
    line_id: i64,
    row: &Map<String, Value>,
) -> (String, Vec<DataValue>) {
    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<DataValue> = Vec::new();
    let mut idx = 1;
    for (col, val) in row {
        if col == "create_time" || col == "update_time" {
            continue; // 时间戳统一处理：update_time=now()，create_time 不改
        }
        sets.push(format!("{col} = ${idx}"));
        params.push(to_dv(val));
        idx += 1;
    }
    sets.push("update_time = now()".to_string());
    sets.push("published_version = published_version + 1".to_string());
    let id_idx = idx;
    let sql = format!(
        "UPDATE {table} SET {} WHERE id = ${id_idx} AND lifecycle_status = 'published'",
        sets.join(", ")
    );
    params.push(DataValue::Int(line_id));
    (sql, params)
}

/// Value → DataValue（覆盖 MDM 用到的类型）。
///
/// - String → String
/// - Number（i64/f64）→ Int/Float
/// - Bool → Bool
/// - Null → Null
/// - Object/Array → Json（序列化字符串）
/// - 其它（罕见）fallback String
pub(crate) fn to_dv(v: &Value) -> DataValue {
    match v {
        Value::Null => DataValue::Null,
        Value::Bool(b) => DataValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                DataValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                DataValue::Float(f)
            } else {
                DataValue::String(n.to_string())
            }
        }
        Value::String(s) => DataValue::String(s.clone()),
        Value::Object(_) | Value::Array(_) => DataValue::Json(v.to_string()),
    }
}
