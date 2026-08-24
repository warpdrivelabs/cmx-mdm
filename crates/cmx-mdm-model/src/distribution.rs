//! M5 分发订阅 —— 通道无关的契约层（信封 / 投递结果 / 通道 trait）。
//!
//! 归属模型层的原因与 `codegen::CodeGenerator` 同构：分发引擎（cmx-mdm-api）按
//! [`DistributionChannel`] 抽象驱动，新增通道（Kafka / RocketMQ / ...）= 实现本 trait
//! + 在引擎的通道注册表登记，引擎与 store 零改动。
//!
//! DB-free：本模块只定义数据结构与行为契约，不触碰存储。

use serde_json::Value;

/// 分发事件信封（通道无关的标准投递单元，webhook body / 未来 MQ 消息体同构）。
///
/// 下游接入契约：
/// - `event_id`：消费端幂等键（at-least-once 投递语义下按它去重）；
/// - `seq`：全局单调（delta token），可校验连续性发现缺口；
/// - `version`：记录级单调（published_version），可丢弃旧版本事件兜底。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    /// 事件 id（md_event_log.id，snowflake，消费端幂等键）。
    pub event_id: String,
    /// 事件全局序号（md_event_log.seq）。
    pub seq: i64,
    /// 事件类型：created / updated / merged（test 通道探测用 "test"）。
    pub event_type: String,
    /// 字典代码。
    pub dict_code: String,
    /// 主数据记录 id。
    pub record_id: i64,
    /// 主数据记录 code（快照 code，快速定位）。
    pub record_code: String,
    /// 记录版本（published_version，记录级单调）。
    pub version: i64,
    /// 事件来源标识（固定 "cmx-mdm"）。
    pub source: &'static str,
    /// 事件发生时间（RFC3339）。
    pub occurred_at: String,
    /// field_map 转换后的快照投影（订阅级裁剪/重命名/脱敏后）。
    pub data: Value,
    /// 溯源信息（crId / victim_ids 等）。
    pub meta: Value,
}

/// 单事件投递结果（[`DistributionChannel::deliver`] 逐条返回）。
#[derive(Debug, Clone)]
pub struct DeliveryResult {
    /// 对应事件 id。
    pub event_id: String,
    /// 是否投递成功。
    pub ok: bool,
    /// 失败是否可重试：`false` = 配置/协议类错误（4xx），引擎直接置 dead 不再重试。
    pub retryable: bool,
    /// HTTP 响应码（webhook 通道；其余通道 None）。
    pub http_status: Option<i64>,
    /// 错误或响应摘要（≤512 字符，落 md_dispatch_log.last_error / response_snippet）。
    pub detail: Option<String>,
}

impl DeliveryResult {
    /// 构造成功结果。
    ///
    /// # Arguments
    ///
    /// * `event_id` - 事件 id。
    /// * `http_status` - HTTP 响应码。
    /// * `detail` - 响应摘要。
    pub fn ok(event_id: impl Into<String>, http_status: Option<i64>, detail: Option<String>) -> Self {
        Self { event_id: event_id.into(), ok: true, retryable: true, http_status, detail }
    }

    /// 构造失败结果。
    ///
    /// # Arguments
    ///
    /// * `event_id` - 事件 id。
    /// * `retryable` - 是否可重试（超时/408/429/5xx/网络 = true；4xx = false）。
    /// * `http_status` - HTTP 响应码。
    /// * `detail` - 错误信息。
    pub fn fail(
        event_id: impl Into<String>,
        retryable: bool,
        http_status: Option<i64>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            ok: false,
            retryable,
            http_status,
            detail: Some(detail.into()),
        }
    }
}

/// 分发通道抽象——一种推送型通道（webhook / kafka / rocketmq / ...）的行为契约。
///
/// 新增通道 = 实现本 trait + 在引擎的 `ChannelRegistry` 登记，分发引擎零改动。
/// 实现须无业务状态（连接池/客户端单例等基础设施除外，集群无状态合规）。
#[async_trait::async_trait]
pub trait DistributionChannel: Send + Sync {
    /// 通道类型标识（对应 md_subscription.channel，如 "webhook" / "kafka"）。
    fn channel_type(&self) -> &'static str;

    /// 校验订阅的 channel_config 结构（保存订阅时前置调用）。
    ///
    /// # Errors
    ///
    /// 配置结构不合法（缺 url / secret、类型错误等）时返回可读错误信息（直接回显给前端）。
    async fn validate_config(&self, config: &Value) -> Result<(), String>;

    /// 投递一批事件信封（引擎已完成订阅级过滤与字段转换）。
    ///
    /// 实现约束：逐条返回结果、不得因单条失败中断整批；单事件单请求（不聚合）。
    async fn deliver(&self, config: &Value, envelopes: &[EventEnvelope]) -> Vec<DeliveryResult>;

    /// 连通性测试（订阅「测试」按钮）：向目标通道发送一条 test 信封。
    ///
    /// test 信封不落 md_event_log / md_dispatch_log，仅验证通道可达与配置正确。
    ///
    /// # Errors
    ///
    /// 通道不可达 / 配置错误 / 响应非 2xx 时返回可读错误信息。
    async fn health_check(&self, config: &Value) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_result_helpers_build_expected_shapes() {
        let ok = DeliveryResult::ok("e1", Some(200), Some("ok".into()));
        assert!(ok.ok && ok.retryable && ok.http_status == Some(200));

        let fail = DeliveryResult::fail("e2", false, Some(404), "not found");
        assert!(!fail.ok && !fail.retryable && fail.detail.as_deref() == Some("not found"));
    }

    #[test]
    fn envelope_serializes_all_fields() {
        let env = EventEnvelope {
            event_id: "evt-1".into(),
            seq: 7,
            event_type: "created".into(),
            dict_code: "supplier".into(),
            record_id: 42,
            record_code: "GYS0001".into(),
            version: 1,
            source: "cmx-mdm",
            occurred_at: "2026-08-18T08:00:00Z".into(),
            data: serde_json::json!({"code": "GYS0001"}),
            meta: serde_json::json!({"crId": 9}),
        };
        let s = serde_json::to_value(&env).expect("序列化信封失败");
        assert_eq!(s["eventId"], "evt-1");
        assert_eq!(s["seq"], 7);
        assert_eq!(s["data"]["code"], "GYS0001");
    }
}
