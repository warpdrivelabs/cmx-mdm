//! MDM 路由表（对任意 axum state 泛型 `S` 成立）。宿主 `merge` 或 `nest("/api", …)` 之。
//!
//! 端点路径与迁移前 cmx-mdm-api 的 MdmModule::routes() 完全一致（`/mdm/*`，`/api` 前缀由宿主 nest
//! 加）。八组路由与 handlers/ 子模块业务域一一对应，各组路径互不重叠。
//! **API 约定**（AGENTS.md §四 第 5 条）：禁用 Path Variable，资源标识/参数走 query（GET）或 body（POST）。

use axum::Router;
use axum::routing::{get, post};

use crate::handlers as mdm;

/// MDM 全部路由（八组合并）。
pub fn mdm_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    health_routes()
        .merge(activation_routes())
        .merge(cr_routes())
        .merge(flow_routes())
        .merge(dedup_routes())
        .merge(merge_routes())
        .merge(subscription_routes())
        .merge(distribution_routes())
}

/// 健康检查。
fn health_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/mdm/health", get(mdm::mdm_health))
}

/// 激活映射配置（M1 配置器 UI）+ 手动激活。
fn activation_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/mdm/activations",
            get(mdm::mdm_activations_list).post(mdm::mdm_activations_save),
        )
        .route("/mdm/activations/delete", post(mdm::mdm_activations_delete))
        .route("/mdm/change-requests/activate", post(mdm::mdm_cr_activate))
}

/// CR 变更请求（M2 审批流转 / 列表 / 详情；新建走标准 /doc/save）。
fn cr_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/mdm/change-requests/submit", post(mdm::mdm_cr_submit))
        .route("/mdm/change-requests/abort", post(mdm::mdm_cr_abort))
        .route("/mdm/change-requests", get(mdm::mdm_cr_list))
        .route("/mdm/change-requests/detail", get(mdm::mdm_cr_detail))
}

/// 流程平台对接（M7 webhook 回调 + 回写状态机）+ M7.1 审批动作业务封装。
fn flow_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/mdm/flow/callback", post(mdm::flow_cb::mdm_flow_callback))
        .route("/mdm/change-requests/withdraw", post(mdm::flow_cb::mdm_cr_withdraw))
        .route("/mdm/change-requests/flow-status", get(mdm::flow_cb::mdm_cr_flow_status))
        .route("/mdm/change-requests/flow-history", get(mdm::flow_cb::mdm_cr_flow_history))
        .route("/mdm/change-requests/review", post(mdm::review::mdm_cr_review))
        .route("/mdm/change-requests/return", post(mdm::review::mdm_cr_return))
        .route("/mdm/change-requests/confirm-apply", post(mdm::review::mdm_cr_confirm_apply))
        .route("/mdm/change-requests/review-context", get(mdm::review::mdm_cr_review_context))
}

/// 查重（M3 实时查重 + V3.2 关键信息查重）+ 查重规则配置维护。
fn dedup_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/mdm/records/find-duplicates", post(mdm::mdm_find_duplicates))
        .route("/mdm/check-key", post(mdm::mdm_check_key))
        .route(
            "/mdm/match-configs",
            get(mdm::mdm_match_configs_list).post(mdm::mdm_match_configs_save),
        )
        .route("/mdm/match-configs/delete", post(mdm::mdm_match_configs_delete))
}

/// 合并请求（M3 确认/还原 + M4 管家工作台详情/驳回）+ M3.5 全库扫描查重 + 汇总计数。
fn merge_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/mdm/merge-requests",
            get(mdm::mdm_merge_requests_list).post(mdm::mdm_merge_requests_create),
        )
        .route("/mdm/merge-requests/undo", post(mdm::mdm_merge_requests_undo))
        .route("/mdm/merge-requests/detail", get(mdm::mdm_merge_request_detail))
        .route("/mdm/merge-requests/reject", post(mdm::mdm_merge_request_reject))
        .route("/mdm/match-scan", get(mdm::mdm_match_scan_list))
        .route("/mdm/match-scan/run", post(mdm::mdm_match_scan_run))
        .route("/mdm/match-scan/detail", get(mdm::mdm_match_scan_detail))
        .route("/mdm/match-scan/ignore", post(mdm::mdm_match_scan_ignore))
        .route("/mdm/workbench/summary", get(mdm::mdm_workbench_summary))
}

/// 订阅与治理（M5 订阅 CRUD / 启停 / 测试 + 审计 / 事件日志 / 手动补发）。
fn subscription_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/mdm/audit", get(mdm::mdm_audit_list))
        .route("/mdm/events", get(mdm::mdm_events_list))
        .route(
            "/mdm/subscriptions",
            get(mdm::mdm_subscriptions_list).post(mdm::mdm_subscriptions_save),
        )
        .route("/mdm/subscriptions/delete", post(mdm::mdm_subscriptions_delete))
        .route("/mdm/subscriptions/set-active", post(mdm::mdm_subscriptions_set_active))
        .route("/mdm/subscriptions/test", post(mdm::mdm_subscriptions_test))
        .route("/mdm/subscriptions/channels", get(mdm::mdm_subscriptions_channels))
        .route("/mdm/publish", post(mdm::mdm_publish))
}

/// 分发投递治理（M5 投递流水 / 统计 / 重发 / 跳过 + pull 游标 + 全量快照）。
fn distribution_routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/mdm/dispatches/query", post(mdm::mdm_dispatches_query))
        .route("/mdm/dispatches/detail", get(mdm::mdm_dispatches_detail))
        .route("/mdm/dispatches/retry", post(mdm::mdm_dispatches_retry))
        .route("/mdm/dispatches/skip", post(mdm::mdm_dispatches_skip))
        .route("/mdm/dispatches/stats", get(mdm::mdm_dispatches_stats))
        .route("/mdm/events/ack", post(mdm::mdm_events_ack))
        .route("/mdm/events/offsets", get(mdm::mdm_events_offsets))
        .route("/mdm/records/snapshot", post(mdm::mdm_records_snapshot))
}
