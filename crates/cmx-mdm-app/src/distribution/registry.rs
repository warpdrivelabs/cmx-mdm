//! 通道注册表 —— channel_type → [`DistributionChannel`] 实现的进程内只读注册表。
//!
//! 集群合规说明：这是基础设施注册表（代码装配，非业务数据缓存），与
//! `flow_client` 的 OnceLock 客户端单例同性质；注册发生在启动期，运行期只读。

use std::sync::Arc;

use dashmap::DashMap;

use cmx_mdm_model::distribution::DistributionChannel;

/// 全局通道注册表（`channel_type` 唯一，后注册覆盖先注册）。
pub struct ChannelRegistry {
    channels: DashMap<&'static str, Arc<dyn DistributionChannel>>,
}

impl ChannelRegistry {
    /// 取全局单例（进程内唯一注册表，启动期写入、运行期只读）。
    pub fn global() -> &'static Self {
        static REG: std::sync::OnceLock<ChannelRegistry> = std::sync::OnceLock::new();
        REG.get_or_init(|| Self { channels: DashMap::new() })
    }

    /// 登记通道实现（启动期调用；同 type 后注册覆盖）。
    ///
    /// # Arguments
    ///
    /// * `channel` - 通道实现（trait 对象，无业务状态）。
    pub fn register(&self, channel: Arc<dyn DistributionChannel>) {
        self.channels.insert(channel.channel_type(), channel);
    }

    /// 按类型取通道实现。
    ///
    /// # Arguments
    ///
    /// * `channel_type` - 通道类型标识（与 md_subscription.channel 对应）。
    ///
    /// # Returns
    ///
    /// 已登记返回实现；未登记（含 feature 未启用）返回 `None`。
    pub fn get(&self, channel_type: &str) -> Option<Arc<dyn DistributionChannel>> {
        self.channels.get(channel_type).map(|e| e.value().clone())
    }

    /// 列出全部已启用通道类型（前端通道下拉数据源；feature 未开启的类型天然不出现）。
    ///
    /// # Returns
    ///
    /// 类型标识排序数组。
    pub fn types(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = self.channels.iter().map(|e| *e.key()).collect();
        v.sort_unstable();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    struct StubChannel(&'static str);

    #[async_trait::async_trait]
    impl DistributionChannel for StubChannel {
        fn channel_type(&self) -> &'static str {
            self.0
        }
        async fn validate_config(&self, _config: &Value) -> Result<(), String> {
            Ok(())
        }
        async fn deliver(
            &self,
            _config: &Value,
            _envelopes: &[cmx_mdm_model::distribution::EventEnvelope],
        ) -> Vec<cmx_mdm_model::distribution::DeliveryResult> {
            vec![]
        }
        async fn health_check(&self, _config: &Value) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn register_and_lookup_roundtrip() {
        let reg = ChannelRegistry { channels: DashMap::new() };
        reg.register(Arc::new(StubChannel("stub-a")));
        assert!(reg.get("stub-a").is_some());
        assert!(reg.get("stub-b").is_none());
        assert_eq!(reg.types(), vec!["stub-a"]);
    }
}
