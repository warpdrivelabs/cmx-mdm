//! M5 分发订阅引擎 —— 通道注册表 + Webhook 通道 + Dispatcher 常驻循环。
//!
//! 架构（方案 20260818 §三/§七）：
//! - 生产侧：激活器单事务写 md_event_log（事务性 Outbox，本模块不生产事件）；
//! - 引擎侧：Dispatcher 每 tick 扇出（watermark 窗口 + uk 幂等）→ 抢占
//!   （SKIP LOCKED + 严格顺序守卫）→ 按订阅分组串行投递（保序）→ 落结果
//!   （delivered / failed 指数退避 / dead）；
//! - 通道侧：[`registry::ChannelRegistry`] 登记 [`WebhookChannel`] 等实现，
//!   Kafka / RocketMQ 骨架 feature-gated（M5.3 启用）。
//!
//! 集群无状态合规：全部队列状态在 DB；进程内仅通道注册表/连接池/Notify 唤醒等基础设施。

/// 通道实现集合（webhook 本期 + kafka/rocketmq 骨架）。
pub mod channels;
/// Dispatcher 常驻循环（扇出 / 抢占 / 投递 / 落结果 / 告警）。
pub mod dispatcher;
/// 通道注册表（channel_type → 实现）。
pub mod registry;
/// 订阅匹配 + 字段投影 + 信封组装（纯函数，可单测）。
pub mod transform;

use std::sync::OnceLock;

use cmx_utils::ConfigManager;

/// `[mdm.distribution]` 配置快照（部署期定值，模式同 `flow_client::flow_cfg`）。
#[derive(Debug, Clone)]
pub struct DistCfg {
    /// 引擎总开关（false：不 spawn 循环，HTTP 端点仍可用）。
    pub enabled: bool,
    /// dispatcher tick 周期（毫秒）。
    pub scan_interval_ms: u64,
    /// 单轮扇出最大事件数。
    pub fanout_batch: i64,
    /// 单轮投递最大 dispatch 数。
    pub deliver_batch: i64,
    /// 跨订阅并发投递上限。
    pub deliver_concurrency: usize,
    /// 重试退避基数（毫秒）。
    pub backoff_base_ms: u64,
    /// 重试退避上限（毫秒）。
    pub backoff_max_ms: u64,
    /// running 残留回收阈值（分钟）。
    pub running_reclaim_minutes: i64,
    /// webhook 目标是否允许私网/回环地址（默认 true：MDM 下游多为内网；外网部署置 false）。
    pub allow_private_address: bool,
}

impl Default for DistCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_interval_ms: 2_000,
            fanout_batch: 500,
            deliver_batch: 100,
            deliver_concurrency: 8,
            backoff_base_ms: 5_000,
            backoff_max_ms: 1_800_000,
            running_reclaim_minutes: 10,
            allow_private_address: true,
        }
    }
}

/// 读 `[mdm.distribution]` 段（缺项回退默认值；模式同 `flow_client::flow_cfg`）。
///
/// # Returns
///
/// 配置快照（逐键读取，进程内不缓存）。
pub fn dist_cfg() -> DistCfg {
    let mut cfg = DistCfg::default();
    let Some(cm) = ConfigManager::try_global() else {
        return cfg;
    };
    if let Ok(v) = cm.get_string("mdm.distribution.enabled") {
        cfg.enabled = v.trim().eq_ignore_ascii_case("true") || v.trim() == "1";
    }
    if let Ok(v) = cm.get_string("mdm.distribution.scan_interval_ms")
        && let Ok(n) = v.trim().parse::<u64>()
        && n > 0
    {
        cfg.scan_interval_ms = n;
    }
    if let Ok(v) = cm.get_string("mdm.distribution.fanout_batch")
        && let Ok(n) = v.trim().parse::<i64>()
        && n > 0
    {
        cfg.fanout_batch = n;
    }
    if let Ok(v) = cm.get_string("mdm.distribution.deliver_batch")
        && let Ok(n) = v.trim().parse::<i64>()
        && n > 0
    {
        cfg.deliver_batch = n;
    }
    if let Ok(v) = cm.get_string("mdm.distribution.deliver_concurrency")
        && let Ok(n) = v.trim().parse::<usize>()
        && n > 0
    {
        cfg.deliver_concurrency = n;
    }
    if let Ok(v) = cm.get_string("mdm.distribution.backoff_base_ms")
        && let Ok(n) = v.trim().parse::<u64>()
        && n > 0
    {
        cfg.backoff_base_ms = n;
    }
    if let Ok(v) = cm.get_string("mdm.distribution.backoff_max_ms")
        && let Ok(n) = v.trim().parse::<u64>()
        && n > 0
    {
        cfg.backoff_max_ms = n;
    }
    if let Ok(v) = cm.get_string("mdm.distribution.running_reclaim_minutes")
        && let Ok(n) = v.trim().parse::<i64>()
        && n > 0
    {
        cfg.running_reclaim_minutes = n;
    }
    if let Ok(v) = cm.get_string("mdm.distribution.allow_private_address") {
        cfg.allow_private_address = v.trim().eq_ignore_ascii_case("true") || v.trim() == "1";
    }
    cfg
}

/// 分发引擎入口：注册通道 + 按配置拉起 Dispatcher 循环。
///
/// 由 `cmx-platform-app` 启动链调用（幂等：重复调用只注册一次循环）。
/// `enabled=false` 时仅注册通道实现（`/subscriptions/test` 等端点仍可用），不 spawn 循环。
///
/// # Errors
///
/// 当前恒为 `Ok(())`（注册与 spawn 均不失败）；保留 `Result` 签名以符合
/// AGENTS.md §十七 全局初始化约束，便于后续启动项扩展。
pub fn start_distribution() -> Result<(), String> {
    use std::sync::Arc;

    let cfg = dist_cfg();
    let reg = registry::ChannelRegistry::global();
    reg.register(Arc::new(channels::WebhookChannel));
    #[cfg(feature = "channel-kafka")]
    reg.register(Arc::new(channels::KafkaChannel));
    #[cfg(feature = "channel-rocketmq")]
    reg.register(Arc::new(channels::RocketMqChannel));

    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_ok() && cfg.enabled {
        tokio::spawn(async move {
            dispatcher::run(cfg).await;
        });
        tracing::info!(target: "cmx_mdm::distribution", "分发引擎已启动");
    }
    Ok(())
}
