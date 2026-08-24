//! cmx-mdm-store-pg 错误助手。
//!
//! 公共错误构造（`api_err`/`api_err_db`）来自 `cmx_biz::error`，此处 re-export
//! 保持本 crate 调用点零改动；`pg_detail` 因入参即 `cmx_database_pg::Error`，
//! 已下沉至 `cmx_database_pg`（归属地更自然）。

// 公共错误助手重导出（向后兼容：本 crate 内 `api_err`/`api_err_db` 调用点零改动）。
pub use cmx_biz::{api_err, api_err_db};

use serde_json::Value;

/// 把 Value 对象里某 String 字段 parse 回 JSON（JSONB 列在 DB 返回 text，需还原）。
///
/// 供 activation_store / match_config_store / doc_accessor 共用（消除 3 份复刻）。
/// 批量还原 JSONB 列（DB 返回 text，统一 parse 回对象；无效/非字符串保持原值）。
///
/// # Arguments
///
/// * `v` - 目标 JSON 行（就地修改）。
/// * `fields` - 待还原的 JSONB 列名列表。
pub(crate) fn parse_jsonb_fields(v: &mut Value, fields: &[&str]) {
    for f in fields {
        parse_jsonb_field(v, f);
    }
}

pub(crate) fn parse_jsonb_field(v: &mut Value, field: &str) {
    if let Some(obj) = v.as_object()
        && let Some(s) = obj.get(field).and_then(|x| x.as_str())
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
        && let Some(obj) = v.as_object_mut() {
            obj.insert(field.to_string(), parsed);
        }
}
