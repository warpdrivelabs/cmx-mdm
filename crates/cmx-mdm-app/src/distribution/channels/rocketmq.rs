//! RocketMQ 通道骨架（`channel-rocketmq` feature）。
//!
//! M5.3 启用时选型客户端（5.x gRPC 协议优先）并补全实现（方案 §6.3 / R2 要点）：
//! - 消息 Key = record_id（同记录有序）；broker ack = 投递成功；
//! - health_check：NameServer/Proxy 探活。
//!
//! 当前骨架与 KafkaChannel 同策略：配置校验可用，投递返回明确"未启用"错误。

use serde_json::Value;

use cmx_mdm_model::distribution::{DeliveryResult, DistributionChannel, EventEnvelope};

/// RocketMQ 通道骨架。
pub struct RocketMqChannel;

#[async_trait::async_trait]
impl DistributionChannel for RocketMqChannel {
    fn channel_type(&self) -> &'static str {
        "rocketmq"
    }

    async fn validate_config(&self, config: &Value) -> Result<(), String> {
        let endpoints = config
            .get("endpoints")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        let topic = config.get("topic").and_then(|v| v.as_str()).map(str::trim).unwrap_or("");
        if endpoints.is_empty() {
            return Err("rocketmq 通道缺 endpoints（NameServer/Proxy 地址）".into());
        }
        if topic.is_empty() {
            return Err("rocketmq 通道缺 topic".into());
        }
        Ok(())
    }

    async fn deliver(&self, config: &Value, envelopes: &[EventEnvelope]) -> Vec<DeliveryResult> {
        let _ = config;
        envelopes
            .iter()
            .map(|env| {
                DeliveryResult::fail(
                    &env.event_id,
                    false,
                    None,
                    "rocketmq 通道未实现（M5.3 启用 channel-rocketmq feature）",
                )
            })
            .collect()
    }

    async fn health_check(&self, _config: &Value) -> Result<(), String> {
        Err("rocketmq 通道未实现（M5.3 启用）".into())
    }
}
