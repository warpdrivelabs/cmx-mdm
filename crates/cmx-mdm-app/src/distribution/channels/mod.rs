//! 通道实现集合 —— Webhook（本期）+ Kafka / RocketMQ（feature 骨架，M5.3 启用）。

/// Webhook 通道（本期核心实现：HMAC-SHA256 签名 + 单事件单请求）。
mod webhook;
pub use webhook::WebhookChannel;

/// Kafka 通道骨架（`channel-kafka` feature 开启后编译；引入 rdkafka 前仅占位）。
#[cfg(feature = "channel-kafka")]
mod kafka;
#[cfg(feature = "channel-kafka")]
pub use kafka::KafkaChannel;

/// RocketMQ 通道骨架（`channel-rocketmq` feature 开启后编译）。
#[cfg(feature = "channel-rocketmq")]
mod rocketmq;
#[cfg(feature = "channel-rocketmq")]
pub use rocketmq::RocketMqChannel;
