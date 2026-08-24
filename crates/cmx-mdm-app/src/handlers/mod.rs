//! MDM handlers —— HTTP handler 集合，按业务域分文件组织。
//!
//! 模块划分：
//! - [`activation`]：激活映射配置 CRUD + 手动激活。
//! - [`cr`]：M2 CR 变更请求（审批流转 / 列表 / 详情）。
//! - [`dedup`]：实时查重 + 关键信息查重（V3.2 步骤条预校验）。
//! - [`merge`]：M3 合并请求（确认 / 详情 / 驳回 / 还原）。
//! - [`governance`]：MDM 治理端点（审计 / 事件 / 订阅 / 发布）。
//! - [`distribution`]：M5 分发治理端点（投递流水 / 统计 / pull 游标 / 全量快照）。
//! - [`match_config`]：查重规则配置。
//! - [`scan`]：M3.5 全库扫描查重（扫描 / 列表 / 详情 / 忽略）。
//! - [`workbench`]：M4 管家工作台聚合（summary 计数）。
//!
//! 提取器惯例（对齐 cmx-dct-api/src/handlers.rs:14-27）：
//!   - `State(_s): State<CmxAppState>`：状态（DB 走全局单例，常忽略为 `_s`）
//!   - `CmxSvrContext(_ctx)`：cmx 上下文
//!   - `headers: HeaderMap`：取 db_id（与 dct/doc 同库路由一致）
//!   - `Query<T>` / `Json<T>`：参数（**禁用 `Path`**，承接 AGENTS.md §四 第 5 条）

/// 激活映射配置 CRUD + 手动激活 handler。
mod activation;
/// M2 CR 变更请求 handler（审批流转 / 列表 / 详情）。
mod cr;
/// M7 流程平台对接 handler（webhook 回调 + 回写状态机 + 懒同步 + 撤回 + 流程查询）。
pub mod flow_cb;
/// M7.1 审批动作业务封装 handler（同意/驳回/退回 + 详情页按钮数据源）。
pub mod review;
/// 实时查重 + 关键信息查重 handler。
mod dedup;
/// MDM 治理端点 handler（审计 / 事件 / 订阅 / 发布）。
mod governance;
/// M5 分发治理端点 handler（投递流水 / 统计 / 重发 / 跳过 / pull 游标 / 全量快照）。
pub mod distribution;
/// 查重规则配置 handler。
mod match_config;
/// M3 合并请求 handler（确认 / 详情 / 驳回 / 还原）。
mod merge;
/// M3.5 全库扫描查重 handler（扫描 / 列表 / 详情 / 忽略）。
mod scan;
/// M4 管家工作台聚合 handler（summary 计数）。
mod workbench;

use axum::Json;
use serde_json::{json, Value};

use cmx_api_types::{ApiResp, Result};
use cmx_mdm_model::match_algo::{FieldKind, MatchFieldSpec};
use cmx_mdm_store_pg as store;

// 共享 DTO 与辅助函数（被多个子模块引用）放此处的 pub(crate) / pub(super)。

/// 比较字段 DTO（`kind`: `"Exact"` | `"EditDistance"`）。
///
/// 序列化形态示例：`{ "field": "name", "weight": 100, "kind": "EditDistance" }`。
/// 经 [`SpecDto::to_match_spec`] 转成 [`MatchFieldSpec`] 供匹配算法使用。
#[derive(serde::Deserialize, Debug, Clone)]
pub struct SpecDto {
    pub field: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default = "default_kind")]
    pub kind: String,
}

impl SpecDto {
    /// 转成匹配算法所需的 [`MatchFieldSpec`]，校验 `kind` 合法。
    ///
    /// # Errors
    ///
    /// `kind` 非 `Exact` / `EditDistance`（大小写或下划线变体均可）时返回错误。
    pub(crate) fn to_match_spec(&self) -> Result<MatchFieldSpec> {
        let kind = match self.kind.as_str() {
            "Exact" | "exact" => FieldKind::Exact,
            "EditDistance" | "edit_distance" | "editDistance" => FieldKind::EditDistance,
            other => {
                return Err(store::api_err(&format!(
                    "字段 {field} 的比较方式 {other:?} 不合法（Exact / EditDistance）",
                    field = self.field
                )))
            }
        };
        Ok(MatchFieldSpec {
            field: self.field.clone(),
            weight: self.weight,
            kind,
        })
    }
}

/// 分页默认页号（第 1 页起）。
pub(crate) fn default_page() -> i64 {
    1
}

/// 分页默认页大小（每页 20 条）。
pub(crate) fn default_page_size() -> i64 {
    20
}

fn default_weight() -> u32 {
    0
}

fn default_kind() -> String {
    "Exact".into()
}

/// 健康检查。
///
/// `GET /api/mdm/health` —— 探测 MDM 模块是否就绪，返回固定 `{ module, status }`。
#[utoipa::path(
    get,
    path = "/api/mdm/health",
    responses(
        (status = 200, description = "{ module: \"mdm\", status: \"ok\" }", body = ApiResp<Value>)
    ),
    tag = "MDM主数据接口"
)]
pub async fn mdm_health(
) -> Result<Json<ApiResp<Value>>> {
    Ok(Json(ApiResp::ok(json!({ "module": "mdm", "status": "ok" }))))
}

pub use activation::*;
pub use cr::*;
pub use dedup::*;
pub use governance::*;
pub use match_config::*;
pub use merge::*;
pub use scan::*;
pub use workbench::*;
pub use distribution::*;
