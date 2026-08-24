//! 匹配组 / 交叉引用 / 治理查询 store（M3）。
//!
//! 模块划分：
//! - [`loader`]：cm_* published 装载（查重 / 合并事务内读主数据）。
//! - [`match_group`]：md_merge_record 读写（合并请求生命周期）。
//! - [`governance`]：治理表分页查询（md_audit / md_event_log / md_subscription）。
//! - [`xref`]：md_xref 状态切换（merge inactive / unmerge active）。
//!
//! 绑定口径（审查重要-2）：可空 BIGINT 用 `DataValue::from(Option<i64>)`（NullTyped(Int)），
//! 可空 JSONB 用 `NullTyped(SqlTypeMarker::Json)`——裸 `DataValue::Null` 绑成 VARCHAR NULL 会被
//! BIGINT/JSONB 列拒收（executor/mod.rs:280）。
//! 时间戳：md_merge_record 仅 created_at（DEFAULT now()）、md_xref 无时间戳列——update SQL **不 SET 时间戳**（审查建议-1）。

/// 治理表分页查询（md_audit / md_event_log / md_subscription）。
mod governance;
/// cm_* published 装载（查重 / 合并事务内读主数据）。
mod loader;
/// md_merge_record 读写（合并请求生命周期）。
mod match_group;
/// md_xref 状态切换（merge inactive / unmerge active）。
mod xref;

pub use governance::{
    delete_subscription, get_subscription, list_audit, list_events, list_subscriptions,
    set_subscription_active, upsert_subscription,
};
pub use loader::{load_by_ids, load_published, load_suspects};
pub use match_group::{
    count_merge_by_status, get_match_group, insert_match_group, list_match_groups,
    transition_match_group, update_match_group,
};
pub use xref::{activate_xref, deactivate_xref};
