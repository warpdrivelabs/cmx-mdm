//! md_match_config 查重规则配置读写（治理表，md_ 前缀 + BIGINT 主键，对齐 md_merge_record）。
//!
//! - [`list_match_config`]：查重界面按字典列规则（下拉「已有规则」用）。
//! - [`get_match_config`]：按 id 取单条。
//! - [`upsert_match_config`]：新建/编辑规则（id 为 0 则新建，非 0 则按 (dict_code, rule_name) upsert）。
//! - [`delete_match_config`]：按 id 软删（is_active=FALSE）。
//!
//! 规则维护发生在查重界面内（选字典→选已有规则/新建/编辑弹框），无独立管理页。
//! JSONB 列（specs/cluster_keys/survive_fields/thresholds）在 DB 以 text 返回，需 parse_jsonb_field。

use cmx_core::dv;
use cmx_core::model::cell::{DataValue, SqlTypeMarker};
use cmx_database_pg::DatabaseManager;
use cmx_utils::next_pk_id;
use serde_json::Value;

use crate::error::{api_err, api_err_db, parse_jsonb_field};

/// 按字典码列规则（查重界面下拉用）。dict_code 为 None 时列全部。
pub async fn list_match_config(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: Option<&str>,
) -> Result<Vec<Value>, cmx_api_types::Error> {
    let mut where_clauses = vec!["is_active = TRUE".to_string()];
    let mut params: Vec<DataValue> = Vec::new();
    if let Some(dc) = dict_code {
        where_clauses.push("dict_code = $1".to_string());
        params.push(DataValue::String(dc.into()));
    }
    let sql = format!(
        r#"SELECT id, rule_name, dict_code, target_table, specs, cluster_keys, survive_fields,
                  thresholds, is_active, created_at
           FROM md_match_config WHERE {} ORDER BY rule_name"#,
        where_clauses.join(" AND ")
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_mcfg_list")
        .await
        .map_err(|e| api_err_db(&format!("列查重规则失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = Vec::with_capacity(ds.rows.len());
    for row in ds.rows.iter() {
        let mut v = row.to_json_value(schema);
        parse_jsonb_field(&mut v, "specs");
        parse_jsonb_field(&mut v, "cluster_keys");
        parse_jsonb_field(&mut v, "survive_fields");
        parse_jsonb_field(&mut v, "thresholds");
        out.push(v);
    }
    Ok(out)
}

/// 按 id 取单条规则。
pub async fn get_match_config(
    mm: &DatabaseManager,
    db_id: &str,
    config_id: i64,
) -> Result<Option<Value>, cmx_api_types::Error> {
    let sql = r#"SELECT id, rule_name, dict_code, target_table, specs, cluster_keys, survive_fields,
                        thresholds, is_active, created_at
                 FROM md_match_config WHERE id = $1"#;
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![DataValue::Int(config_id)],
            "mdm_mcfg_get",
        )
        .await
        .map_err(|e| api_err_db(&format!("取查重规则失败: {e}")))?;
    let Some(row) = ds.rows.first() else {
        return Ok(None);
    };
    let mut v = row.to_json_value(ds.schema.as_ref());
    parse_jsonb_field(&mut v, "specs");
    parse_jsonb_field(&mut v, "cluster_keys");
    parse_jsonb_field(&mut v, "survive_fields");
    parse_jsonb_field(&mut v, "thresholds");
    Ok(Some(v))
}

/// upsert 配置项（由 handler 反序列化好的 Value 透传）。
///
/// - `id` 为 0 或缺失：新建（next_pk_id 生成 id）。
/// - `id` 非零：按 (dict_code, rule_name) upsert（保留原 id）。
///
/// `cfg` 须含 rule_name/dict_code/target_table/specs/cluster_keys/survive_fields/thresholds(可选)。
/// 返回规则 id（i64）。
pub async fn upsert_match_config(
    mm: &DatabaseManager,
    db_id: &str,
    cfg: &Value,
) -> Result<i64, cmx_api_types::Error> {
    // 兼容驼峰（前端契约）与下划线（DB 风格）两种 key
    let pick_str = |k1: &str, k2: &str| cfg.get(k1).and_then(|v| v.as_str()).or_else(|| cfg.get(k2).and_then(|v| v.as_str()));
    let rule_name = pick_str("ruleName", "rule_name")
        .ok_or_else(|| api_err("rule_name 必填"))?;
    let dict_code = pick_str("dictCode", "dict_code")
        .ok_or_else(|| api_err("dict_code 必填"))?;
    let target_table = pick_str("targetTable", "target_table")
        .ok_or_else(|| api_err("target_table 必填"))?;
    let empty_arr = Value::Array(vec![]);
    let specs = cfg.get("specs").unwrap_or(&empty_arr);
    let cluster_keys = cfg.get("clusterKeys").or_else(|| cfg.get("cluster_keys")).unwrap_or(&empty_arr);
    let survive_fields = cfg.get("surviveFields").or_else(|| cfg.get("survive_fields")).unwrap_or(&empty_arr);
    let thresholds = cfg.get("thresholds"); // 可空

    // id：0/缺失=新建(next_pk_id)；非零=按 (dict_code,rule_name) upsert 保留原 id
    let existing_id = cfg.get("id").and_then(|v| v.as_i64()).filter(|i| *i > 0);
    let id = existing_id.unwrap_or_else(next_pk_id);

    let specs_json = serde_json::to_string(specs).map_err(|e| api_err(&format!("specs 序列化失败: {e}")))?;
    let ck_json = serde_json::to_string(cluster_keys).map_err(|e| api_err(&format!("cluster_keys 序列化失败: {e}")))?;
    let sf_json = serde_json::to_string(survive_fields).map_err(|e| api_err(&format!("survive_fields 序列化失败: {e}")))?;
    let thr_dv = match thresholds {
        Some(t) if !t.is_null() => {
            let s = serde_json::to_string(t).map_err(|e| api_err(&format!("thresholds 序列化失败: {e}")))?;
            DataValue::Json(s)
        }
        // thresholds 可空（JSONB 列）：用 NullTyped(Json) 绑定类型化 NULL，避免裸 Null 绑成 VARCHAR NULL 被 JSONB 拒
        _ => DataValue::NullTyped(SqlTypeMarker::Json),
    };

    let sql = r#"INSERT INTO md_match_config
                   (id, rule_name, dict_code, target_table, specs, cluster_keys, survive_fields,
                    thresholds, is_active)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,TRUE)
                 ON CONFLICT (dict_code, rule_name) DO UPDATE SET
                   target_table   = EXCLUDED.target_table,
                   specs          = EXCLUDED.specs,
                   cluster_keys   = EXCLUDED.cluster_keys,
                   survive_fields = EXCLUDED.survive_fields,
                   thresholds     = EXCLUDED.thresholds,
                   is_active      = TRUE"#;
    mm.execute_sql_with_datavalues(
        db_id,
        None,
        sql,
        dv![
            DataValue::Int(id),
            DataValue::String(rule_name.into()),
            DataValue::String(dict_code.into()),
            DataValue::String(target_table.into()),
            DataValue::Json(specs_json),
            DataValue::Json(ck_json),
            DataValue::Json(sf_json),
            thr_dv,
        ],
    )
    .await
    .map_err(|e| api_err_db(&format!("保存查重规则失败: {e}")))?;
    // ON CONFLICT 更新分支不返回 EXCLUDED.id，需反查确保返回正确 id
    if existing_id.is_none() {
        let real = resolve_id(mm, db_id, dict_code, rule_name).await?;
        return Ok(real);
    }
    Ok(id)
}

/// upsert 后反查规则 id（`RETURNING id` 缺失时的兜底，按 (dict_code, rule_name) 唯一键定位）。
///
/// # Errors
///
/// 保存后反查为空（数据异常）时返回错误。
async fn resolve_id(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: &str,
    rule_name: &str,
) -> Result<i64, cmx_api_types::Error> {
    let sql = "SELECT id FROM md_match_config WHERE dict_code = $1 AND rule_name = $2";
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![DataValue::String(dict_code.into()), DataValue::String(rule_name.into())],
            "mdm_mcfg_rid",
        )
        .await
        .map_err(|e| api_err_db(&format!("反查规则 id 失败: {e}")))?;
    ds.rows
        .first()
        .and_then(|r| r.get_by_name_as::<i64>(ds.schema.as_ref(), "id"))
        .ok_or_else(|| api_err("保存后反查规则 id 为空"))
}

/// 按 id 删除规则（软删：is_active=FALSE）。
pub async fn delete_match_config(
    mm: &DatabaseManager,
    db_id: &str,
    config_id: i64,
) -> Result<u64, cmx_api_types::Error> {
    let sql = "UPDATE md_match_config SET is_active = FALSE WHERE id = $1";
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![DataValue::Int(config_id)],
        )
        .await
        .map_err(|e| api_err_db(&format!("删除查重规则失败: {e}")))?;
    Ok(n)
}
