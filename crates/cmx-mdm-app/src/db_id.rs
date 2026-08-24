//! 请求库路由：从 `db_id` 请求头解析目标数据库 id，缺失时回退业务库。
//!
//! 统一入口 [`resolve_db_id_from_headers`]，供 doc/dct/code/mdm 等所有 API crate 复用，
//! 消除各 handler 重复手写「取 header → trim → 回退 biz」的散落实现。
//!
//! 行为契约：
//!   - `db_id` 头存在且非空（trim 后）→ 用它
//!   - 头缺失 / 空串 / 非 UTF-8 → 回退第一个 `source_type="biz"` 的业务库；无业务库再回退默认库

use axum::http::HeaderMap;
use cmx_database_pg::get_default_pg_db_manager;

/// 从请求头解析 db_id（缺失/空/非法时回退业务库）。
///
/// 供各 API handler 统一调用：`let db_id = resolve_db_id_from_headers(&headers).await;`
pub async fn resolve_db_id_from_headers(headers: &HeaderMap) -> String {
    // 非 UTF-8 字节静默丢弃（转 None），与各 handler 原行为对齐
    let raw = headers
        .get("db_id")
        .and_then(|v| v.to_str().ok());
    resolve_db_id(raw).await
}

/// 解析 db_id：显式值优先，缺失/空串回退业务库。
///
/// `db_id_header` 已是 `Option<&str>`（来自 header 或其他来源），trim 后空串视为缺失。
pub async fn resolve_db_id(db_id_header: Option<&str>) -> String {
    if let Some(v) = db_id_header {
        let s = v.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    get_default_pg_db_manager().get_biz_db_id().await
}
