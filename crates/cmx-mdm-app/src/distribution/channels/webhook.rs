//! Webhook 通道 —— HMAC-SHA256 签名推送（协议与 M7 flow webhook 验签同款 `sha256=<hex>`）。
//!
//! 协议（单事件单请求，Stripe/GitHub 惯例）：
//! ```text
//! POST {channel_config.url}
//! X-CMX-Event-Id:    {envelope.event_id}     （消费端幂等键）
//! X-CMX-Event-Type:  {envelope.event_type}
//! X-CMX-Timestamp:   {unix_ms}               （下游防重放建议 ±5min 窗口）
//! X-CMX-Signature:   sha256={hex(HMAC-SHA256(raw_body, secret))}
//! {channel_config.headers 自定义头}
//! ```
//! 成功判据：HTTP 2xx；可重试：超时/连接失败/408/429/5xx；不可重试：其余 4xx → 引擎置 dead。

use std::time::Duration;

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

use cmx_mdm_model::distribution::{DeliveryResult, DistributionChannel, EventEnvelope};

use super::super::dist_cfg;

/// Webhook 通道实现（无状态，连接复用全局 reqwest 单例）。
pub struct WebhookChannel;

/// reqwest 客户端单例（连接池；集群无状态合规——纯基础设施）。
fn client() -> &'static reqwest::Client {
    static CLI: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLI.get_or_init(reqwest::Client::new)
}

/// 校验 + 提取通道配置。
struct WebhookCfg<'a> {
    url: &'a str,
    secret: &'a str,
    headers: &'a Value,
    timeout_ms: Option<u64>,
}

fn parse_config(config: &Value) -> Result<WebhookCfg<'_>, String> {
    let url = config
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if url.is_empty() {
        return Err("webhook 通道缺 url".into());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("webhook url 必须以 http:// 或 https:// 开头".into());
    }
    let secret = config
        .get("secret")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if secret.is_empty() {
        return Err("webhook 通道缺签名秘钥 secret".into());
    }
    let headers = config.get("headers").filter(|v| v.is_object()).unwrap_or(&Value::Null);
    let timeout_ms = config.get("timeout_ms").and_then(|v| v.as_u64());
    Ok(WebhookCfg { url, secret, headers, timeout_ms })
}

/// HMAC-SHA256 签名（`sha256=<hex>`；与 flow_cb 验签协议互为镜像）。
fn sign_body(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC 接受任意长度密钥");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// SSRF 基础防护：目标 host 为字面私网/回环地址且未放行时拒绝。
fn check_private_address(url: &str) -> Result<(), String> {
    if dist_cfg().allow_private_address {
        return Ok(());
    }
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("url 解析失败: {e}"))?;
    let host = parsed.host_str().unwrap_or("");
    let blocked = ["127.", "10.", "192.168.", "169.254.", "localhost", "[::1]", "::1"]
        .iter()
        .any(|p| host.starts_with(p) || host == *p)
        || host
            .strip_prefix("172.")
            .and_then(|rest| rest.split('.').next())
            .and_then(|o| o.parse::<u8>().ok())
            .is_some_and(|o| (16..=31).contains(&o));
    if blocked {
        return Err(format!("webhook 目标 {host} 为私网/回环地址，当前配置禁止（allow_private_address=false）"));
    }
    Ok(())
}

/// 单事件单请求投递（分类结果：可重试 / 不可重试）。
async fn post_once(url: &str, secret: &str, headers: &Value, timeout_ms: u64, env: &EventEnvelope) -> DeliveryResult {
    if let Err(e) = check_private_address(url) {
        return DeliveryResult::fail(&env.event_id, false, None, e);
    }
    let body = match serde_json::to_vec(env) {
        Ok(b) => b,
        Err(e) => return DeliveryResult::fail(&env.event_id, false, None, format!("信封序列化失败: {e}")),
    };
    let ts = chrono_now_ms();
    let mut req = client()
        .post(url)
        .timeout(Duration::from_millis(timeout_ms))
        .header("Content-Type", "application/json")
        .header("X-CMX-Event-Id", &env.event_id)
        .header("X-CMX-Event-Type", &env.event_type)
        .header("X-CMX-Timestamp", ts)
        .header("X-CMX-Signature", sign_body(secret, &body))
        .body(body);
    if let Some(hs) = headers.as_object() {
        for (k, v) in hs {
            if let Some(val) = v.as_str() {
                req = req.header(k.as_str(), val);
            }
        }
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16() as i64;
            let snippet = resp.text().await.unwrap_or_default();
            let snippet = if snippet.chars().count() > 200 {
                snippet.chars().take(200).collect::<String>()
            } else {
                snippet
            };
            if (200..300).contains(&status) {
                DeliveryResult::ok(&env.event_id, Some(status), Some(snippet))
            } else if status == 408 || status == 429 || status >= 500 {
                DeliveryResult::fail(&env.event_id, true, Some(status), format!("HTTP {status}: {snippet}"))
            } else {
                DeliveryResult::fail(
                    &env.event_id,
                    false,
                    Some(status),
                    format!("HTTP {status}（不可重试类错误，请检查订阅配置）: {snippet}"),
                )
            }
        }
        Err(e) => DeliveryResult::fail(&env.event_id, true, None, format!("请求失败: {e}")),
    }
}

/// 当前 unix 毫秒（时间戳头）。
fn chrono_now_ms() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl DistributionChannel for WebhookChannel {
    fn channel_type(&self) -> &'static str {
        "webhook"
    }

    async fn validate_config(&self, config: &Value) -> Result<(), String> {
        let cfg = parse_config(config)?;
        check_private_address(cfg.url)
    }

    async fn deliver(&self, config: &Value, envelopes: &[EventEnvelope]) -> Vec<DeliveryResult> {
        let cfg = match parse_config(config) {
            Ok(c) => c,
            Err(e) => {
                return envelopes
                    .iter()
                    .map(|env| DeliveryResult::fail(&env.event_id, false, None, e.clone()))
                    .collect();
            }
        };
        let timeout = cfg.timeout_ms.unwrap_or(10_000);
        let mut out = Vec::with_capacity(envelopes.len());
        for env in envelopes {
            out.push(post_once(cfg.url, cfg.secret, cfg.headers, timeout, env).await);
        }
        out
    }

    async fn health_check(&self, config: &Value) -> Result<(), String> {
        let cfg = parse_config(config)?;
        let test = EventEnvelope {
            event_id: format!("test-{}", cmx_utils::next_pk_id()),
            seq: 0,
            event_type: "test".into(),
            dict_code: "health-check".into(),
            record_id: 0,
            record_code: String::new(),
            version: 0,
            source: "cmx-mdm",
            occurred_at: chrono_now_ms(),
            data: serde_json::json!({ "message": "cmx-mdm webhook 连通性测试" }),
            meta: serde_json::json!({}),
        };
        let r = post_once(cfg.url, cfg.secret, cfg.headers, cfg.timeout_ms.unwrap_or(10_000), &test).await;
        if r.ok {
            Ok(())
        } else {
            Err(r.detail.unwrap_or_else(|| "未知错误".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_flow_protocol() {
        let sig = sign_body("secret", b"body");
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), 7 + 64);
        // 确定性
        assert_eq!(sig, sign_body("secret", b"body"));
    }

    #[test]
    fn config_validation_rejects_missing_fields() {
        assert!(parse_config(&serde_json::json!({})).is_err());
        assert!(parse_config(&serde_json::json!({"url": "http://a"})).is_err());
        assert!(parse_config(&serde_json::json!({"url": "ftp://a", "secret": "s"})).is_err());
        assert!(parse_config(&serde_json::json!({"url": "http://a", "secret": "s"})).is_ok());
    }
}
