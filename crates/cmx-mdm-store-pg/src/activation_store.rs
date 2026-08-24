//! mdm_activation 激活映射配置读写。
//!
//! - [`find_by_doc_type`]：激活器主用（按 source_doc_type + cr_type 取映射）。
//! - [`list`] / [`upsert`]：映射配置器 UI 用。

use cmx_core::dv;
use cmx_core::model::cell::DataValue;
use cmx_database_pg::DatabaseManager;
use cmx_mdm_model::activation::ActivationConfig;
use cmx_utils::snowflake_id_str;
use serde_json::Value;

use crate::error::{api_err, api_err_db, parse_jsonb_field};

/// 按来源单据类型 + cr_type 查激活映射（激活器主用）。
pub async fn find_by_doc_type(
    mm: &DatabaseManager,
    db_id: &str,
    txn_id: Option<&str>,
    source_doc_type: &str,
    cr_type: &str,
) -> Result<Option<ActivationConfig>, cmx_api_types::Error> {
    let sql = r#"SELECT activation_code, source_doc_type, cr_type, target_dict, target_table,
                        header_mapping, line_mappings, code_rule_code, subject_name_field, subject_code_field,
                        doc_code_rules, key_fields
                 FROM mdm_activation
                 WHERE source_doc_type = $1 AND cr_type = $2 AND is_active = TRUE
                 LIMIT 1"#;
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            txn_id,
            sql,
            dv![
                DataValue::String(source_doc_type.into()),
                DataValue::String(cr_type.into()),
            ],
            "mdm_act_find",
        )
        .await
        .map_err(|e| api_err(&format!("查激活映射失败: {e}")))?;
    let Some(row) = ds.rows.first() else {
        return Ok(None);
    };
    let mut v = row.to_json_value(ds.schema.as_ref());
    // header_mapping / line_mappings 是 JSONB，DB 里是 text，需 parse
    parse_jsonb_field(&mut v, "header_mapping");
    parse_jsonb_field(&mut v, "line_mappings");
    parse_jsonb_field(&mut v, "doc_code_rules");
    parse_jsonb_field(&mut v, "key_fields");
    let cfg = serde_json::from_value::<ActivationConfig>(v)
        .map_err(|e| api_err(&format!("激活映射反序列化失败: {e}")))?;
    Ok(Some(cfg))
}

/// 列表（配置器 UI / 通用详情页）。可选过滤 sourceDocType/crType/targetDict。
pub async fn list(
    mm: &DatabaseManager,
    db_id: &str,
    source_doc_type: Option<&str>,
    cr_type: Option<&str>,
    target_dict: Option<&str>,
) -> Result<Vec<Value>, cmx_api_types::Error> {
    // 动态拼 WHERE（参数化，防注入）：占位符序号取 push 后的 params.len()，
    // 天然与参数个数一致，避免手工计数在多过滤条件同传时撞 $n。
    let mut where_clauses = vec!["is_active = TRUE".to_string()];
    let mut params: Vec<DataValue> = Vec::new();
    if let Some(sdt) = source_doc_type {
        params.push(DataValue::String(sdt.into()));
        where_clauses.push(format!("source_doc_type = ${}", params.len()));
    }
    if let Some(ct) = cr_type {
        params.push(DataValue::String(ct.into()));
        where_clauses.push(format!("cr_type = ${}", params.len()));
    }
    if let Some(td) = target_dict {
        params.push(DataValue::String(td.into()));
        where_clauses.push(format!("target_dict = ${}", params.len()));
    }
    let sql = format!(
        r#"SELECT id, activation_code, source_doc_type, cr_type, target_dict, target_table,
                  header_mapping, line_mappings, code_rule_code, subject_name_field, subject_code_field,
                  header_groups, doc_code_rules, key_fields, is_active
           FROM mdm_activation WHERE {} ORDER BY sort_order_of_none(), activation_code"#,
        where_clauses.join(" AND ")
    );
    // 上面 ORDER BY sort_order_of_none() 是占位——mdm_activation 无 sort_order 列，改用 activation_code
    let sql = sql.replace("ORDER BY sort_order_of_none(), ", "ORDER BY ");
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_act_list")
        .await
        .map_err(|e| api_err_db(&format!("列表激活映射失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut out = Vec::with_capacity(ds.rows.len());
    for row in ds.rows.iter() {
        let mut v = row.to_json_value(schema);
        parse_jsonb_field(&mut v, "header_mapping");
        parse_jsonb_field(&mut v, "line_mappings");
        parse_jsonb_field(&mut v, "header_groups");
        parse_jsonb_field(&mut v, "doc_code_rules");
        parse_jsonb_field(&mut v, "key_fields");
        out.push(v);
    }
    Ok(out)
}

/// 保存（upsert by activation_code）。id 用 snowflake_id_str()。返回 activation_code。
pub async fn upsert(
    mm: &DatabaseManager,
    db_id: &str,
    cfg: &ActivationConfig,
) -> Result<String, cmx_api_types::Error> {
    let id = snowflake_id_str();
    let header_json = serde_json::to_string(&cfg.header_mapping)
        .map_err(|e| api_err(&format!("header_mapping 序列化失败: {e}")))?;
    let line_json = serde_json::to_string(&cfg.line_mappings)
        .map_err(|e| api_err(&format!("line_mappings 序列化失败: {e}")))?;
    let groups_json = serde_json::to_string(&cfg.header_groups)
        .map_err(|e| api_err(&format!("header_groups 序列化失败: {e}")))?;
    let doc_rules_json = serde_json::to_string(&cfg.doc_code_rules)
        .map_err(|e| api_err(&format!("doc_code_rules 序列化失败: {e}")))?;
    let key_fields_json = serde_json::to_string(&cfg.key_fields)
        .map_err(|e| api_err(&format!("key_fields 序列化失败: {e}")))?;
    let sql = r#"INSERT INTO mdm_activation
                   (id, activation_code, source_doc_type, cr_type, target_dict, target_table,
                    header_mapping, line_mappings, code_rule_code, subject_name_field, subject_code_field,
                    header_groups, doc_code_rules, key_fields, is_active)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,TRUE)
                 ON CONFLICT (activation_code) DO UPDATE SET
                   source_doc_type     = EXCLUDED.source_doc_type,
                   cr_type             = EXCLUDED.cr_type,
                   target_dict         = EXCLUDED.target_dict,
                   target_table        = EXCLUDED.target_table,
                   header_mapping      = EXCLUDED.header_mapping,
                   line_mappings       = EXCLUDED.line_mappings,
                   code_rule_code      = EXCLUDED.code_rule_code,
                   subject_name_field  = EXCLUDED.subject_name_field,
                   subject_code_field  = EXCLUDED.subject_code_field,
                   header_groups       = EXCLUDED.header_groups,
                   doc_code_rules      = EXCLUDED.doc_code_rules,
                   key_fields          = EXCLUDED.key_fields,
                   is_active           = TRUE,
                   updated_at          = now()"#;
    let params = dv![
        DataValue::String(id),
        DataValue::String(cfg.activation_code.clone()),
        DataValue::String(cfg.source_doc_type.clone()),
        DataValue::String(cfg.cr_type.clone()),
        DataValue::String(cfg.target_dict.clone()),
        DataValue::String(cfg.target_table.clone()),
        DataValue::Json(header_json),
        DataValue::Json(line_json),
        cfg.code_rule_code.clone().map(DataValue::String).unwrap_or(DataValue::Null),
        cfg.subject_name_field.clone().map(DataValue::String).unwrap_or(DataValue::Null),
        cfg.subject_code_field.clone().map(DataValue::String).unwrap_or(DataValue::Null),
        DataValue::Json(groups_json),
        DataValue::Json(doc_rules_json),
        DataValue::Json(key_fields_json),
    ];
    mm.execute_sql_with_datavalues(db_id, None, sql, params)
        .await
        .map_err(|e| api_err_db(&format!("保存激活映射失败: {e}")))?;
    Ok(cfg.activation_code.clone())
}

/// 删除（按 activation_code，硬删除）。返回影响行数。
///
/// 注意：`list` 只返回 `is_active=TRUE` 的行，故「停用」（开关置 FALSE）也会让映射从列表消失；
/// 本函数是**彻底删除**（DELETE），与停用语义不同——配置器「删除」按钮用此。
pub async fn delete_by_code(
    mm: &DatabaseManager,
    db_id: &str,
    activation_code: &str,
) -> Result<u64, cmx_api_types::Error> {
    let sql = "DELETE FROM mdm_activation WHERE activation_code = $1";
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![DataValue::String(activation_code.into())],
        )
        .await
        .map_err(|e| api_err_db(&format!("删除激活映射失败: {e}")))?;
    Ok(n)
}

/// 明细表清单条目（合并去重用，api 层组装后传给合并/还原服务）。
///
/// `dedup_keys` 由 api 层从 DCT `uniqueKeys` 去掉 `parent_field` 后推导；为空表示该表
/// 不去重（合并时全部 reparent）。
#[derive(Debug, Clone)]
pub struct LineTableInfo {
    /// 明细物理表名（如 cm_bank_account）。
    pub table: String,
    /// 外键列名（如 supplier_id）。
    pub parent_field: String,
    /// 去重业务键列名（如 ["account_no"]）；空 Vec=不去重。
    pub dedup_keys: Vec<String>,
}

/// 按 target_dict 聚合所有激活映射的明细表清单（合并/还原 reparent 用）。
///
/// 告诉合并引擎「这个主数据有哪些子表、子表通过哪个外键列挂在头表上」，以便 victim 的
/// 明细行 reparent 到 master。数据源是 `mdm_activation.line_mappings`（JSONB 数组，
/// 元素含 `{targetTable, parentIdField, targetDict}`），按 `target_dict` 过滤所有激活配置聚合。
///
/// 返回 `(明细表名, 外键列名, 明细字典码)` 三元组：target_dict 供调用方查 DCT uniqueKeys
/// 推导去重键。一个 target_dict 可能被多个 activation 引用（create/update/block 各一条），
/// 故按 `(table, field)` 去重（target_dict 取首次出现值，同表同列归属应一致）。
/// 未配置明细或 line_mappings 非数组时返回空 Vec（合并不 reparent 明细）。
pub async fn line_tables_for_dict(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: &str,
) -> Result<Vec<(String, String, String)>, cmx_api_types::Error> {
    let sql = "SELECT line_mappings FROM mdm_activation WHERE target_dict = $1 AND is_active = TRUE";
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            None,
            sql,
            dv![DataValue::String(dict_code.into())],
            "mdm_act_line_tables",
        )
        .await
        .map_err(|e| api_err_db(&format!("查 {dict_code} 明细表清单失败: {e}")))?;
    let schema = ds.schema.as_ref();
    // (table, parent_field, target_dict)；按 (table, parent_field) 去重
    let mut out: Vec<(String, String, String)> = Vec::new();
    for row in &ds.rows {
        let mut v = row.to_json_value(schema);
        parse_jsonb_field(&mut v, "line_mappings");
        let Some(items) = v.get("line_mappings").and_then(|x| x.as_array()) else {
            continue;
        };
        for item in items {
            let table = item.get("targetTable").and_then(|x| x.as_str()).unwrap_or("");
            let field = item.get("parentIdField").and_then(|x| x.as_str()).unwrap_or("");
            let tdict = item.get("targetDict").and_then(|x| x.as_str()).unwrap_or("");
            if !table.is_empty() && !field.is_empty()
                && !out.iter().any(|(t, f, _)| t == table && f == field)
            {
                // 按 (table, field) 去重（同一明细表字典码应一致，取首次）
                out.push((table.to_string(), field.to_string(), tdict.to_string()));
            }
        }
    }
    Ok(out)
}
