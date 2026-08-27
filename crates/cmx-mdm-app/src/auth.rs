//! 请求级身份中间件——已收编至 `cmx-engine-kit::auth::delegated`（唯一真源）。
//!
//! 本模块保留 `mw` 薄包装（内嵌本仓内置白名单），main.rs 的 `from_fn(auth::mw)` 挂载零改动。
//! 配置键（`auth.jwt_secret` / `auth.api_keys` / `auth.whitelist`）、行为契约（白名单 →
//! API Key → 委托令牌 → `AuthContext` + `scope_full`，委托失败降级匿名不 401）与展示名
//! 回退链（nickname → username → user_id）见真源：
//! `../cmx-container/crates/libs/cmx-engine-kit/src/auth/delegated.rs`。
//!
//! 为什么不复用 `cmx-auth`（平台认证基建）：该 crate 连带 sqlx/Redis/argon2 整套平台认证栈，
//! 拖进独立微服务编译图代价过大——引擎既定模式是轻量自实现（现统一收编 cmx-engine-kit）。

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

use cmx_engine_kit::auth::delegated::{self, DelegatedSpec};

/// 本仓专属参数：内置白名单 = 探针（`/mdm/health`）+ 流程 webhook 回调
/// （`/mdm/flow/callback`，HMAC 签名即凭证）；toml `[auth].whitelist` 可追加。
static SPEC: DelegatedSpec = DelegatedSpec::new(&["/mdm/health", "/mdm/flow/callback"], "mdm");

/// 请求级身份中间件（`axum::middleware::from_fn` 形态；签名不变）。
///
/// handler 的 [`crate::ctx::current_user_id`] 与 `flow_client` 的
/// `current_original_token()`（继续向流程平台透传用户 JWT）从本中间件建立的 scope 取值。
pub async fn mw(req: Request, next: Next) -> Response {
    delegated::mw(req, next, &SPEC).await
}
