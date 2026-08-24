//! 主数据微服务监控大盘（对标 cmx-model-app / cmx-rpt-app 的 dashboard + stats）。
//!
//!   - [`dashboard`]：`GET /` 返回自包含单页 HTML（include_str!，light/dark，轮询 stats）。
//!   - [`mdm_stats`]：`GET /api/mdm/stats` 返回服务标识 + 能力清单 + 存活探针结果。
//!     只读、无 DB 强耦合（MDM 数据全在业务库，表结构随部署而异），保证大盘总能出盘。
//! MDM 的真实业务界面是 10 个 native 页（portal.mdm.*，经前端联邦投递）。

use axum::Json;
use axum::response::Html;
use serde_json::{Value, json};

use cmx_api_types::ApiResp;

/// 大盘页（自包含 HTML，轮询 `/api/mdm/stats`）。
pub async fn dashboard() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

/// 服务标识 + 能力清单 + 存活。绝不 500。
pub async fn mdm_stats() -> Json<ApiResp<Value>> {
    Json(ApiResp::ok(json!({
        "service": "cmx-mdm 主数据中心",
        "status": "up",
        "capabilities": [
            {"key": "activation", "label": "激活映射 / 手动激活"},
            {"key": "cr", "label": "变更请求(CR)审批流转"},
            {"key": "dedup", "label": "实时查重 / 规则配置"},
            {"key": "merge", "label": "合并 golden record / 全库扫描"},
            {"key": "subscription", "label": "订阅分发(webhook/kafka/rocketmq)"},
            {"key": "distribution", "label": "投递治理 / 死信重发 / pull 游标"},
            {"key": "governance", "label": "审计事件 / 数据治理"},
            {"key": "flow", "label": "流程平台对接(审批)"},
        ],
        "pages": [
            "portal.mdm.master-list", "portal.mdm.master-detail", "portal.mdm.cr-form",
            "portal.mdm.cr-todo", "portal.mdm.steward", "portal.mdm.duplicate-check",
            "portal.mdm.activation-mapper", "portal.mdm.subscription-manager",
            "portal.mdm.dispatch-monitor", "portal.mdm.health",
        ],
    })))
}
