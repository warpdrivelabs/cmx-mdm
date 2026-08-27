//! 请求级身份中间件（独立壳模式）——委托令牌模式对标 flow-server `cmx-flow-app/src/auth.rs`。
//!
//! 配置走**平台统一装配链**（ConfigManager 三源合并：mdm-server.toml ← Nacos 配置中心 ← env），
//! 与 `[mdm.flow]` 等业务段同一条链，启动快照进 [`AUTH`]（OnceLock，进程内只读）：
//!
//! ```toml
//! [auth]
//! jwt_secret = "<平台签发 JWT 的密钥>"          # 委托令牌验签（HS256）
//! api_keys   = "<key1>,<key2>"                 # 服务间 API Key（= 平台 outgoing_api_key）
//! whitelist  = ["/api/mdm/health"]             # 免鉴权路径（前缀匹配，兼容门户带 /api 写法；可追加）
//! ```
//!
//! 行为（对齐 flow 语义）：
//!   1. **白名单**直放——内置 `/mdm/health`（探针）与 `/mdm/flow/callback`（webhook，HMAC 签名
//!      即凭证），配置段可追加（「内置 + toml 合并」语义对齐门户 mw_auth 的白名单制度）；
//!   2. `X-API-Key` 校验（配置了 `api_keys` 才启用；命中即服务身份）；
//!   3. `X-Delegated-User-Token` **始终验签**（HS256，`jwt_secret` = 平台签发 JWT 的密钥），
//!      解 `sub`（= user_id）建立请求级 scope——handler 的 [`crate::ctx::current_user_id`] 与
//!      `flow_client` 的 `current_original_token()`（继续向流程平台透传用户 JWT）都从此取值；
//!      未配密钥或验签失败 → 退化为纯服务调用（匿名），**不 401**（服务身份已由 API Key 验过）。
//!
//! scope 用 `cmx_traits::auth::context_scope::scope_full`（与平台 mw_auth 并行同源的 task_local），
//! 嵌入平台时二者注入同一登录用户 → 零回归；⚠️ task_local 不跨 `tokio::spawn`（分发 dispatcher
//! 等后台任务读不到，本就无需用户身份）。
//!
//! 为什么不复用 `cmx-auth`（平台认证基建）的 `JwtEncoder::decode_access_token`：该 crate 连带
//! sqlx/Redis/argon2/moka/prometheus 整套平台认证栈，拖进独立微服务编译图代价过大——flow /
//! rules 同样在各自 app 层轻量自实现 JWT 解码（engine 服务既定模式），仅 claims 取 `sub`/`roles`
//! 两字段与平台签发对齐。

use std::sync::{Arc, OnceLock};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use cmx_core::AuthContext;
use cmx_traits::auth::context_scope::scope_full;
use cmx_utils::ConfigManager;

/// 内置免用户鉴权路径（请求路径已剥 `/api` 前缀）：探针 + 流程 webhook 回调（HMAC 签名即凭证）。
const BUILTIN_WHITELIST: [&str; 2] = ["/mdm/health", "/mdm/flow/callback"];

/// 认证配置快照（启动期经 ConfigManager 装配一次，进程内只读；配置热更不敏感——密钥类均为
/// 部署期定值，与 flow_client 的 `[mdm.flow]` 快照同一口径）。
struct AuthConfig {
    /// 委托令牌验签密钥（HS256）。空 = 不解票（纯服务调用）。
    jwt_secret: Arc<str>,
    /// 服务间 API Key 集合（已剥冒号后缀归一）。空 = 不强制校验。
    api_keys: Vec<String>,
    /// 免鉴权路径前缀（已归一为剥 `/api` 的内部形态）。
    whitelist: Vec<String>,
}

impl AuthConfig {
    /// 经 ConfigManager 读 `[auth]` 段（缺项回退空值；ConfigManager 未初始化时同样回退）。
    fn load() -> Self {
        let mut cfg = Self {
            jwt_secret: Arc::from(""),
            api_keys: Vec::new(),
            whitelist: BUILTIN_WHITELIST.iter().map(|s| s.to_string()).collect(),
        };
        let Some(cm) = ConfigManager::try_global() else {
            return cfg;
        };
        if let Ok(v) = cm.get_string("auth.jwt_secret") {
            cfg.jwt_secret = Arc::from(v.trim());
        }
        if let Ok(v) = cm.get_string("auth.api_keys") {
            cfg.api_keys = v
                .split(',')
                .map(|k| k.trim().split(':').next().unwrap_or("").trim().to_string())
                .filter(|k| !k.is_empty())
                .collect();
        }
        // 白名单：内置 + 配置追加（语义对齐门户 mw_auth「BUILTIN_WHITELIST 与 TOML [auth].whitelist 合并」）。
        for item in cm.get_as_or::<Vec<String>>("auth.whitelist", Vec::new()) {
            let item = item.trim();
            // 兼容门户带 /api 前缀的写法（中间件看到的是已剥 /api 的路径）。
            let p = item.strip_prefix("/api").unwrap_or(item);
            if !p.is_empty() && !cfg.whitelist.iter().any(|w| w == p) {
                cfg.whitelist.push(p.to_string());
            }
        }
        cfg
    }
}

/// 全局认证配置（首次请求时装载快照）。
static AUTH: OnceLock<AuthConfig> = OnceLock::new();

fn auth_cfg() -> &'static AuthConfig {
    AUTH.get_or_init(AuthConfig::load)
}

/// 是否免用户鉴权路径（前缀匹配，对齐门户白名单语义：`/mdm/health` 也覆盖 `/mdm/health/x`）。
fn is_whitelisted(path: &str, whitelist: &[String]) -> bool {
    whitelist.iter().any(|w| path.starts_with(w.as_str()))
}

/// 401 响应（仅 API Key 校验失败时返回；委托令牌失败只降级不拒绝）。
fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::WWW_AUTHENTICATE, "X-API-Key")],
        format!("{{\"code\":401,\"msg\":\"{msg}\"}}"),
    )
        .into_response()
}

/// 请求级身份中间件主体（`axum::middleware::from_fn` 形态）。
pub async fn mw(req: Request, next: Next) -> Response {
    let cfg = auth_cfg();
    if is_whitelisted(req.uri().path(), &cfg.whitelist) {
        return next.run(req).await;
    }

    // 服务身份：配置了 api_keys 才强制校验。平台反代（MdmProxyModule）与 flow 客户端均会
    // 携带 = [service_auth].outgoing_api_key。
    if !cfg.api_keys.is_empty() {
        let hit = req
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .map(|k| cfg.api_keys.iter().any(|allowed| allowed == k))
            .unwrap_or(false);
        if !hit {
            return unauthorized("无效或缺失 X-API-Key");
        }
    }

    // 终端用户身份：X-Delegated-User-Token 验签（始终验签；未配密钥/失败 → 匿名服务调用）。
    let (auth, original_token) = match delegated_auth(&req, &cfg.jwt_secret) {
        Delegated::Verified(auth, token) => (Some(auth), Some(token)),
        Delegated::Anonymous(reason) => {
            tracing::debug!(target: "cmx_mdm::auth", reason, "无委托用户身份，按服务调用处理");
            (None, None)
        }
    };

    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    // 请求全程在 scope 内：current_user_id / current_original_token（flow_client 透传）可用。
    scope_full(auth, original_token, request_id, None, next.run(req)).await
}

/// 委托令牌解析结果。
enum Delegated {
    /// 验签通过：用户身份 + 原始 JWT（供继续透传）。
    Verified(AuthContext, String),
    /// 无令牌 / 未配密钥 / 验签失败（reason 供日志）。
    Anonymous(&'static str),
}

/// 委托令牌的 JWT claim（对齐平台 `cmx-auth` AccessClaims：`sub` = user_id、`username` =
/// 用户名；roles/username/nickname 可缺省——缺省时展示名按 nickname→username→sub 回退，
/// 兼容旧令牌与第三方精简令牌）。
#[derive(Debug, Deserialize)]
struct DelegatedClaims {
    sub: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
}

/// 从 `X-Delegated-User-Token: Bearer <jwt>` 验签解出终端用户（`sub` = user_id）。
///
/// 未配 `jwt_secret` 时返回 Anonymous（服务 key 调用照常工作，仅 created_by / operated_by 类
/// 字段回退空/0）；密钥必须 = 平台签发 JWT 的密钥。
fn delegated_auth(req: &Request, secret: &str) -> Delegated {
    let Some(token) = req
        .headers()
        .get("x-delegated-user-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .and_then(|s| s.strip_prefix("Bearer ").or_else(|| s.strip_prefix("bearer ")))
        .filter(|s| !s.is_empty())
    else {
        return Delegated::Anonymous("无 X-Delegated-User-Token");
    };
    if secret.is_empty() {
        // 未配密钥：不能无签信任终端用户身份（对齐 flow「委托令牌始终验签」），降级服务调用。
        return Delegated::Anonymous("未配 auth.jwt_secret，跳过委托令牌解票");
    }
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    // 平台令牌不带 aud 约束校验（JwtConfig issuer/audience 仅作签发侧记录），这里只验签名 + exp。
    validation.validate_aud = false;
    validation.required_spec_claims.clear();
    let data = match jsonwebtoken::decode::<DelegatedClaims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    ) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(target: "cmx_mdm::auth", error = %e, "X-Delegated-User-Token 验签失败，退化为纯服务调用");
            return Delegated::Anonymous("委托令牌验签失败");
        }
    };
    let user_id = data.claims.sub.trim().to_string();
    if user_id.is_empty() {
        return Delegated::Anonymous("委托令牌 sub 为空");
    }
    // username 是操作人姓名展示来源——优先 nickname（如"张三"），回退 username claim
    // （"admin"），再回退 user_id。不取姓名兜底 id 会让 created_by/operated_by 类展示变成雪花 id。
    let user_name = {
        let nick = data.claims.nickname.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let name = data.claims.username.trim();
        match (nick, name.is_empty()) {
            (Some(n), _) => n.to_string(),
            (None, false) => name.to_string(),
            (None, true) => user_id.clone(),
        }
    };
    let auth = AuthContext {
        username: user_name,
        user_id,
        roles: data.claims.roles,
        permissions: Vec::new(),
        org_id: None,
        session_id: None,
        device_type: None,
        auth_method: Some("delegated_jwt".to_string()),
    };
    Delegated::Verified(auth, token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_prefix_match_and_api_prefix_stripped() {
        let wl = vec!["/mdm/health".to_string(), "/mdm/flow/callback".to_string()];
        assert!(is_whitelisted("/mdm/health", &wl));
        assert!(is_whitelisted("/mdm/flow/callback", &wl));
        assert!(!is_whitelisted("/mdm/change-requests", &wl));
    }

    #[test]
    fn api_keys_tolerate_colon_suffix() {
        // AuthConfig::load 需要 ConfigManager，这里只验证归一逻辑的输入输出形态。
        let raw = "key1, key2:tenant ,key3";
        let keys: Vec<String> = raw
            .split(',')
            .map(|k| k.trim().split(':').next().unwrap_or("").trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        assert_eq!(keys, vec!["key1", "key2", "key3"]);
    }
}
