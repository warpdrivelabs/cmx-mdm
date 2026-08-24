//! cmx-mdm-store-pg —— 主数据（MDM）模块的 PostgreSQL 持久化/服务层。
//!
//! 模块结构：
//! - [`doc_accessor`]：读 CR 单据（cv_mdm_apply 头 + cv_mdm_apply_line 行）。
//! - [`activation_store`]：mdm_activation 激活映射配置读写（激活器 + UI 配置器）。
//! - [`dct_accessor`]：cm_* 主数据写入闸口（强制 lifecycle_status='published'，唯一入口）。
//! - [`sql_builder`]：cm_* 写入的 SQL 构造与列值转换工具（dct_accessor 内部用）。
//! - [`md_accessor`]：md_audit / md_event_log 治理表写入 + CR 状态归档。
//! - [`activation_service`]：激活器 / 合并 / 还原三套主流程的单事务编排。
//! - [`cr_service`]：CR 变更请求服务（状态校验 / 列表 / 详情 / 作废）。
//! - [`match_store`]：匹配组 / 交叉引用 / 治理查询 store。
//! - [`match_config_store`]：查重规则配置 store。
//! - [`scan_store`]：查重发现项 store（md_match_scan，全库扫描结果载体）。
//! - [`error`]：错误助手（api_err / api_err_db / parse_jsonb_field）。
//!
//! 惯例（对齐 cmx-dct-store-pg）：store 是模块级自由 async 函数，DB 连接走
//! `cmx_database_pg::get_default_pg_db_manager()` 全局单例，不经 HTTP / State 注入。

/// 激活器 / 合并 / 还原三套主流程的单事务编排。
mod activation_service;
/// mdm_activation 激活映射配置读写（激活器 + UI 配置器）。
mod activation_store;
/// CR 变更请求服务（状态校验 / 列表 / 详情 / 作废）。
mod cr_service;
/// cm_* 主数据写入闸口（强制 lifecycle_status='published'，唯一入口）。
mod dct_accessor;
/// 读 CR 单据（cv_mdm_apply 头 + cv_mdm_apply_line 行）。
mod doc_accessor;
/// 错误助手（api_err / api_err_db / parse_jsonb_field）。
mod error;
/// md_match_config 查重规则配置读写。
mod match_config_store;
/// 匹配组 / 交叉引用 / 治理查询 store。
mod match_store;
/// md_match_scan 查重发现项 store（全库扫描结果载体，管家评审）。
mod scan_store;
/// md_audit / md_event_log 治理表写入 + CR 状态归档。
mod md_accessor;
/// cm_* 写入的 SQL 构造与列值转换工具（dct_accessor 内部用）。
mod sql_builder;
/// M5 分发引擎存储（投递实例队列 / 扇出水位 / pull 游标）。
mod dispatch_store;

pub use activation_store::{find_by_doc_type, line_tables_for_dict, list, upsert, delete_by_code, LineTableInfo};
pub use cr_service::{abort_cr, check_status, check_status_in, get_cr_detail, list_cr};
pub use error::{api_err, api_err_db};
// cm_* 按名称模糊查 id（合并历史名称搜索 D-05 用，复用 dct_accessor 列判断）
pub use dct_accessor::find_ids_by_name_like;
pub use dct_accessor::select_row_json;
// 激活器主流程对 api 层暴露（M1 activate + M3 merge/unmerge）
pub use activation_service::{activate, merge, unmerge, MergeStats};
// M3 匹配/合并 store 对 api 层暴露
pub use match_store::{
    count_merge_by_status, get_match_group, insert_match_group, list_audit, list_events,
    list_match_groups, list_subscriptions, load_by_ids, load_published, load_suspects,
    transition_match_group, update_match_group, upsert_subscription,
    delete_subscription, set_subscription_active, get_subscription,
};
// M5 分发引擎存储对 api 层暴露
pub use dispatch_store::{
    claim_dispatches, dispatch_stats, fanout_tick, get_dispatch, list_consumer_offsets,
    list_dispatches, load_events_by_ids, load_subscriptions_by_ids, mark_dispatch,
    publish_rebuild, reclaim_running, retry_dispatches, skip_dispatches, upsert_consumer_offset,
};
// M3.5 查重发现项 store（全库扫描 / 评审队列，cluster_hash 去重）
pub use scan_store::{
    count_scan_by_status, get_scan, insert_findings, list_scans, transition_scan_status,
    InsertStats, PreparedCluster,
};
// 查重规则配置 store 对 api 层暴露（查重界面内维护）
pub use match_config_store::{
    delete_match_config, get_match_config, list_match_config, upsert_match_config,
};
// set_cr_status 供 api 层改 CR 状态(submit/reject,自动提交);激活器内部直接用 md_accessor
pub use md_accessor::set_cr_status as set_cr_status_pub;
// M7 抢占式状态迁移（流程回写三方并发收敛原语）
pub use md_accessor::try_set_cr_status as try_set_cr_status_pub;
// M7 懒同步自愈窗口判定（update_time 时效）
pub use md_accessor::cr_updated_before as cr_updated_before_pub;
