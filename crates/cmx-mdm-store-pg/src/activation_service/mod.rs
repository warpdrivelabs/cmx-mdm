//! 激活器 / 合并 / 还原三套主流程的单事务编排。
//!
//! 模块划分：
//! - [`activate`]：激活器主流程（七步单事务：读 CR → 读映射 → 头处理 → 明细 → 审计 → 事件 → 归档）。
//! - [`merge`]：合并主流程（十步单事务：锁 master → survive → victim→merged → reparent → 更新 master）。
//! - [`unmerge`]：还原主流程（反向：victim merged→published、明细指回、xref active）。
//!
//! 全程在一个 DB 事务内，任一步失败 guard drop 自动回滚，无中间态。

/// 激活器主流程（七步单事务）。
mod activate;
/// 合并主流程（十步单事务）。
mod merge;
/// 还原主流程（victim merged→published）。
mod unmerge;

use cmx_mdm_model::match_algo::MatchRecord;
use serde_json::{Map, Value};

pub use activate::activate;
pub use merge::{merge, MergeStats};
pub use unmerge::unmerge;

/// 取记录 `lifecycle_status` 字符串（缺省返回空串）。
///
/// 供 [`merge`] / [`unmerge`] 校验 master/victim 状态用。
fn lifecycle_of(r: &MatchRecord) -> &str {
    r.fields
        .get("lifecycle_status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// 用累积 row 构造临时 [`MatchRecord`]（供下一轮 survive 作 master）。
fn master_record(row: &Map<String, Value>) -> MatchRecord {
    let id = row.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
    MatchRecord {
        id,
        fields: row.clone(),
    }
}
