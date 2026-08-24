//! 请求级身份助手（task_local 版，语义对齐 cmx-api-core/src/actor.rs）。
//!
//! 中立核不依赖 cmx-api（原 handler 经 `CmxSvrContext` 获取操作者身份），此处提供**语义字节
//! 对齐**的本地实现：从 `cmx-traits::auth::context_scope` task_local 取（平台 mw_auth 与本
//! crate [`crate::auth`] 中间件并行同源注入；独立壳由 auth 中间件从 `X-Delegated-User-Token`
//! 验签建立）。库路由（db_id 头）见 [`crate::db_id`]（对方既有模块，语义同 cmx-api-core::db_id）。

/// 当前登录用户 id（字符串口径，与 CR create_by / flow initiator 对齐；未登录返回 None）。
///
/// 原 `flow_cb::current_user_id` 读 `svr_ctx.0.auth_context.user_id`；这里改读请求级
/// task_local（`cmx_traits::auth::context_scope::current_auth()`），平台 mw_auth 与本 crate
/// [`crate::auth`] 中间件注入同一快照，取值同源零回归。
pub fn current_user_id() -> Option<String> {
    cmx_traits::auth::context_scope::current_auth()
        .map(|a| a.user_id.trim().to_string())
        .filter(|u| !u.is_empty())
}

/// 当前操作人 id（i64 口径，审计列 operated_by 用）；无认证/空/非数字 → 0。
/// 复刻原 `cmx_api_core::actor::actor_id_i64` 语义（约定 0=系统，保存**永不因身份缺失失败**），
/// 改走 cmx-traits context_scope。
pub fn current_actor_id() -> i64 {
    current_user_id()
        .and_then(|u| u.parse::<i64>().ok())
        .unwrap_or(0)
}
