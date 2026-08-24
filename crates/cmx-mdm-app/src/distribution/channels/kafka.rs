//! Kafka 通道骨架（`channel-kafka` feature）。
//!
//! M5.3 启用时引入 rdkafka 并补全实现（方案 §6.3 要点）：
//! - producer：brokers 来自 channel_config；`partition_key = record_id`（同记录事件同分区有序，
//!   延续方案 §7.6 顺序语义的 MQ 形态）；`acks=all`；
//! - 发送回调异步确认映射 [`DeliveryResult`]（broker ack = 投递成功，非下游消费确认）；
//! - health_check：metadata fetch 探活。
//!
//! 当前骨架：配置结构校验可用，deliver 返回明确的"未启用"错误（不产生错误投递）。

use serde_json::Value;

use cmx_mdm_model::distribution::{DeliveryResult, DistributionChannel, EventEnvelope};

/// Kafka 通道骨架。
pub struct KafkaChannel;

#[async_trait::async_trait]
impl DistributionChannel for KafkaChannel {
    fn channel_type(&self) -> &'static str {
        "kafka"
    }

    async fn validate_config(&self, config: &Value) -> Result<(), String> {
        let brokers = config.get("brokers").and_then(|v| v.as_str()).map(str::trim).unwrap_or("");
        let topic = config.get("topic").and_then(|v| v.as_str()).map(str::trim).unwrap_or("");
        if brokers.is_empty() {
            return Err("kafka 通道缺 brokers".into());
        }
        if topic.is_empty() {
            return Err("kafka 通道缺 topic".into());
        }
        Ok(())
    }

    async fn deliver(&self, config: &Value, envelopes: &[EventEnvelope]) -> Vec<DeliveryResult> {
        // 骨架：M5.3 引入 rdkafka 后实现（partition_key = record_id）
        let _ = config;
        envelopes
            .iter()
            .map(|env| {
                DeliveryResult::fail(
                    &env.event_id,
                    false,
                    None,
                    "kafka 通道未实现（M5.3 启用 channel-kafka feature 并引入 rdkafka）",
                )
            })
            .collect()
    }

    async fn health_check(&self, _config: &Value) -> Result<(), String> {
        Err("kafka 通道未实现（M5.3 启用）".into())
    }
}
