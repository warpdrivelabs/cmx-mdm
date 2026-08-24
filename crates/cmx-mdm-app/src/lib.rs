//! cmx-mdm-app —— 主数据（MDM）的**平台中立应用层**（对标 cmx-model-app / cmx-rpt-app「一芯多壳」）。
//!
//! 暴露 [`mdm_routes`]`::<S>()`：一张对任意 axum state 泛型 `S` 成立的 MDM 路由表（八业务组：健康/
//! 激活/CR/流程/查重/合并/订阅/分发）。全部 handler 只带 Query/Json/HeaderMap 提取器，不取 state；
//! DB 走 tokio-pg 全局管理器；DCT 数据经跨 ws 的 cmx-dct-store-pg；分发死信经 HTTP 回环门户通知。
//! 外加监控大盘（[`dashboard`]）+ 前端联邦（[`native_pages`]，portal.mdm.* 只读投递）。
//!
//! 两壳复用同一核：独立壳 `cmx-mdm-server`（本 ws）merge 路由 + 大盘 + 单 DB 栈数据源钩子；平台壳
//! `cmx-mdm-proxy`（留 cmx-container，M5）proxy-only 反代到独立壳。端点路径与迁移前一致，前端零改。
//! 信封类型来自 cmx-api-types；身份来自 cmx-traits context_scope（scan/merge 审计操作人）。不依赖
//! cmx-api-core（不认 CmxAppState）。

// 内联 db_id 工具（resolve_db_id_from_headers；<40 行，只依赖 axum+cmx-database-pg，不牵 CmxAppState）。
pub mod db_id;

// 请求级身份助手（current_user_id / current_actor_id，task_local 实现；flow_cb re-export）。
pub mod ctx;

// 请求级身份中间件（独立壳模式）：X-API-Key 校验 + X-Delegated-User-Token 验签建 scope
//（ConfigManager 读 [auth]，白名单内置+追加）。没有它，ctx/current_user_id 恒为匿名。
pub mod auth;

// OpenApi 切片（MdmApiDoc），供独立壳 swagger 挂载。
pub mod openapi;

// handler 聚合（八业务域）+ 分发引擎 + 流程客户端。
pub mod handlers;
pub mod distribution;
pub mod flow_client;

// 路由表（泛型 <S>）。
pub mod mdm_routes;

// 监控大盘 + 前端联邦。
pub mod dashboard;

pub use mdm_routes::mdm_routes;
