# cmx-mdm-store-pg

> 主数据管理（MDM）模块的 PostgreSQL 持久化/服务层：激活器 / 合并 / 还原三套单事务主流程编排，`cm_*` 主数据写入闸口，CR 单据、治理表、匹配组、查重配置、扫描发现项与分发引擎的 store 集合。

[![Version](https://img.shields.io/badge/version-0.1.12-blue.svg)]()
[![Edition](https://img.shields.io/badge/rust--edition-2024-orange.svg)]()

---

## 项目简介

`cmx-mdm-store-pg` 是 MDM 域三件套中的**持久化层**。它把 `cmx-mdm-model` 的纯算法
落到 PostgreSQL：激活器读 CR 单据 + 映射配置 → 调 model 的 `plan_*` 搬运 → 经
`dct_accessor` 闸口写入 `cm_*`（强制 `lifecycle_status='published'`）→ 记审计、发事件、
归档 CR——全程**一个 DB 事务**，任一步失败 guard drop 自动回滚，无中间态。

**惯例**（对齐 `cmx-dct-store-pg`）：store 是**模块级自由 async 函数**，DB 连接走
`cmx_database_pg::get_default_pg_db_manager()` 全局单例，不经 HTTP / State 注入；
错误统一映射为 `cmx_api_types::Error`（与 cmx-api 同一错误类型）。

### 涉及的表

| 表 | 归属 | 说明 |
|----|------|------|
| `cv_mdm_apply` / `cv_mdm_apply_line` | CR 单据（模型中心 cm_*/cv_*） | 变更请求头/行（新建走标准 /doc/save） |
| `cm_*`（cm_supplier 等） | 主数据（模型中心） | 只存 published；写入闸口在本 crate `dct_accessor` |
| `mdm_activation` | 激活映射（迁移） | 「源单据字段 → 主数据列」搬运配置 |
| `md_audit` / `md_event_log` | 治理（迁移 md_*） | 字段级审计留痕 / 分发事件（seq 自增） |
| `md_merge_record` / `md_xref` | 匹配治理（迁移） | 合并请求生命周期 / 记录交叉引用 |
| `md_match_config` | 查重规则（迁移） | 字段权重 + 双阈值（thresholds）配置 |
| `md_match_scan` | 扫描发现项（迁移） | 全库查重聚类结果，cluster_hash 去重 |
| `md_subscription` / `md_dispatch_log` / `md_consumer_offset` | 分发（迁移） | 订阅 / 投递实例 / pull 游标 |

---

## 与其他 crate 的关系

### 上游依赖（本 crate 依赖）

| 依赖 | 用途 |
|------|------|
| `cmx-mdm-model` | 纯逻辑层：`plan_create`/`plan_update`/`plan_lines`（激活器）、`survive`（合并）、`MatchRecord`、`LifecycleStatus`、`ActivationConfig` |
| `cmx-database-pg` | tokio-postgres `DatabaseManager` 全局单例：`query_sql_with_datavalues` / `execute_sql_with_datavalues` / 事务上下文（RAII guard） |
| `cmx-dct-store-pg` | 字典元数据与事务化写入：激活器 create 分支复用 `dict_upsert` 纳入主事务、`dict_meta` 查 codeRule、`recompute_dict_hierarchy` 分级补偿 |
| `cmx-traits` | `code::GlobalCodeMinter`（激活器铸主数据 code 的注入点） |
| `cmx-utils` | `next_pk_id` / `snowflake_id_str`（主键铸号） |
| `cmx-core` | `DataValue`（SQL 参数强类型绑定） |
| `cmx-biz` | `api_err` / `api_err_db` 错误助手（本 crate error.rs re-export） |
| `cmx-api-types` | `Error` / `Result` 信封（与 cmx-api 保持同一错误类型） |
| `serde` / `serde_json` / `tokio` / `tracing` / `sha2` | 序列化 / 异步运行时 / 日志 / cluster_hash 去重（member_ids 升序稳定 hash，跨重启一致） |

### 下游使用方（谁依赖本 crate）

| 使用方 | 引用方式 | 实际用途 |
|--------|---------|---------|
| `cmx-mdm-api` | `cmx-mdm-store-pg = { workspace = true }` | 唯一直接依赖者：全部 handler 经 `use cmx_mdm_store_pg as store;` 调用（submit/activate/merge/查重/治理/分发的所有 DB 操作） |
| `cmx-platform-app` / `cmx-portalservice` | 经 `cmx-mdm-api` 传递依赖 | 门户进程承载 MDM HTTP 端点时间接受益 |

无反向依赖：本 crate 不依赖 `cmx-mdm-api`（分层无环）。

```text
cmx-mdm-api（HTTP handler）
        │  use cmx_mdm_store_pg as store;
        ▼
┌─────────────────────────────────────────────────────┐
│ cmx-mdm-store-pg（本 crate）                         │
│  activation_service ── 调 ──► cmx-mdm-model 纯算法   │
│  （activate 七步 / merge 十步 / unmerge，单事务）    │
│  各 store / accessor ──────► cmx-database-pg（SQL）  │
└─────────────────────────────────────────────────────┘
```

---

## 核心功能与特性

| 功能 | 说明 |
|------|------|
| 激活器主流程 `activate` | 七步单事务：读 CR 头行 → 读激活映射 → 头处理（create 铸号/upsert 或 update CAS）→ 明细处理 → 记审计 → 发事件 → CR 归档（approved/approving → activated） |
| 合并主流程 `merge` | 十步单事务：lock_record(master, FOR UPDATE) 串行化交叉 merge → 读双方（须 published）→ survive 逐字段（多 victim 顺序累积）→ victim CAS→merged → 明细迁移+按业务键去重软删 → master CAS 更新 → xref 失活 → 审计/事件 → match_group CAS 收敛 |
| 还原主流程 `unmerge` | 合并的反向：victim merged→published、明细指回原头表、xref 重新激活 |
| `cm_*` 写入闸口 | `dct_accessor` 是写入 `cm_*` 的**唯一入口**（强制 lifecycle_status='published'）；create 分支复用 `cmx-dct-store-pg::dict_upsert` 纳入激活主事务 |
| 乐观锁并发控制 | `get_version` 快照 + `published_version` 条件更新（CAS），n=0 即冲突报错 |
| 抢占式状态迁移 | `try_set_cr_status`：条件 UPDATE（from 集合命中才迁移），webhook 回调 / 列表懒同步 / 手动兜底三方并发收敛原语，同语句刷 update_time 作懒同步自愈窗口计时起点 |
| 铸号集成 | create 分支优先 `GlobalCodeMinter`（字典自身 dictMeta.codeRule），未注入/失败回退调用方传入的 `CodeGenerator` 占位码（保证 NOT NULL） |
| 审计与事件 | `write_audit`（字段级新旧值留痕）/ `write_event`（分发事件，seq 由 DB 自增）；事件在激活事务内写入，与主数据原子 |
| 匹配组 / 交叉引用 | `md_merge_record` 生命周期（pending→reviewed/rejected/undone）+ `md_xref` 状态切换（merge 失活 / unmerge 激活） |
| 查重规则配置 | `md_match_config` 读写（字段权重 + 双阈值 thresholds） |
| 全库扫描发现项 | `insert_findings` 批量落 `md_match_scan`（`PreparedCluster` 输入 + `InsertStats` 去重统计，cluster_hash 幂等） |
| 分发引擎存储 | `fanout_tick`（事件扇出为投递实例）→ `claim_dispatches`（认领投递）→ `mark_dispatch`（结果回写 + 退避）；`reclaim_running` 回收 running 残留；`retry/skip_dispatches` 治理动作；pull 模式 `load_events_by_ids` + `upsert_consumer_offset` 游标 |
| JSONB 还原工具 | `parse_jsonb_field(s)`：JSONB 列 DB 返回 text 统一 parse 回对象（activation_store / match_config_store / doc_accessor 共用） |

---

## 模块结构

```text
cmx-mdm-store-pg
├── src
│   ├── lib.rs                    # 模块声明 + pub use 对 api 层的扁平导出清单
│   ├── error.rs                  # api_err/api_err_db 重导出（来自 cmx-biz）+ parse_jsonb_field(s)
│   ├── activation_service/       # 三套单事务主流程
│   │   ├── mod.rs                #   公共工具（lifecycle_of / master_record）+ activate/merge/unmerge 导出
│   │   ├── activate.rs           #   激活七步（含 mint_dict_code 铸号集成）
│   │   ├── merge.rs              #   合并十步（MergeStats 统计）
│   │   └── unmerge.rs            #   还原（victim merged→published）
│   ├── activation_store.rs       # mdm_activation 读写：find_by_doc_type/list/upsert/delete_by_code/line_tables_for_dict
│   ├── cr_service.rs             # CR 服务：check_status(_in)/list_cr/get_cr_detail/abort_cr
│   ├── doc_accessor.rs           # 读 CR 单据：load_cr_head（SELECT * 元数据驱动）/ load_cr_lines
│   ├── dct_accessor.rs           # cm_* 写入闸口：insert_header/update_header/update_line/set_lifecycle/reparent_lines/lock_record…
│   ├── sql_builder.rs            # cm_* 写入的 SQL 构造与列值转换（dct_accessor 内部用）
│   ├── md_accessor.rs            # 治理表写入：write_audit/write_event/set_cr_status/try_set_cr_status/cr_updated_before
│   ├── match_store/              # 匹配组 / 交叉引用 / 治理查询
│   │   ├── mod.rs                #   导出 + 可空列绑定口径说明（NullTyped）
│   │   ├── loader.rs             #   cm_* published 装载：load_published/load_suspects/load_by_ids
│   │   ├── match_group.rs        #   md_merge_record：insert/update/transition/list/count/get
│   │   ├── governance.rs         #   md_audit/md_event_log/md_subscription 分页与订阅 CRUD
│   │   └── xref.rs               #   md_xref：activate_xref/deactivate_xref
│   ├── match_config_store.rs     # md_match_config 查重规则读写
│   ├── scan_store.rs             # md_match_scan 发现项：insert_findings/list/get/transition/count
│   └── dispatch_store.rs         # 分发引擎存储：fanout_tick/claim_dispatches/mark_dispatch/retry/skip/stats/offsets/rebuild
└── Cargo.toml
```

---

## 关键类型 / API（lib.rs 扁平导出）

### 三套主流程（activation_service）

```rust
/// 激活一份 CR（审批通过后调用；两条触发路径统一入口）。
/// 返回新建/变更的主数据记录 id。任一步出错事务回滚，cm_* 无中间态。
pub async fn activate(
    mm: &DatabaseManager, db_id: &str,
    cr_id: i64, operated_by: i64, codegen: &dyn CodeGenerator,
) -> Result<i64, cmx_api_types::Error>;

/// 合并：master + victims → 十步单事务。返回迁移/去重统计。
pub async fn merge(
    mm: &DatabaseManager, db_id: &str, dict_code: &str, head_table: &str,
    master_id: i64, victim_ids: &[i64],
    survive_fields: &[String],
    rules: &HashMap<String, SurvivorRule>,
    overrides: &serde_json::Map<String, Value>,   // 人工裁决显式真值
    line_tables: &[LineTableInfo],
    operated_by: i64, match_group_id: i64,
) -> Result<MergeStats, cmx_api_types::Error>;

pub struct MergeStats { pub master_id: i64, pub reparented_total: u64, pub deduped_total: u64 }

/// 还原：victim merged→published、明细指回、xref 重新激活（单 victim）。
pub async fn unmerge(
    mm: &DatabaseManager, db_id: &str, dict_code: &str, head_table: &str,
    master_id: i64, victim_id: i64,
    line_tables: &[LineTableInfo], operated_by: i64, match_group_id: i64,
) -> Result<(), cmx_api_types::Error>;
```

### 激活映射 / CR 服务

```rust
// activation_store.rs
pub async fn find_by_doc_type(mm, db_id, txn_id, source_doc_type: &str, cr_type: &str)
    -> Result<Option<ActivationConfig>, cmx_api_types::Error>;
pub async fn upsert(mm, db_id, cfg: &ActivationConfig) -> Result<String, Error>;  // 返回 activation_code
pub async fn line_tables_for_dict(mm, db_id, dict_code) -> Result<Vec<(String, String, String)>, Error>;
pub struct LineTableInfo { pub table: String, pub parent_field: String, pub dedup_keys: Vec<String> }

// cr_service.rs
pub async fn check_status(mm, db_id, txn_id, cr_id, expect: &str) -> Result<Map<String, Value>, Error>;
pub async fn list_cr(…) / get_cr_detail(…) / abort_cr(…);
```

### 写入闸口与治理（dct_accessor / md_accessor）

```rust
// dct_accessor.rs（cm_* 唯一写入入口，部分）
pub async fn insert_header(mm, db_id, txn_id, table, row: &Map<String,Value>) -> Result<i64, Error>;
pub async fn update_header(mm, db_id, txn_id, table, id, row, expect_version: i64) -> Result<(), Error>; // CAS
pub async fn set_lifecycle(mm, db_id, txn_id, table, id, from: &str, to: &str) -> Result<u64, Error>;
pub async fn reparent_lines(mm, db_id, txn_id, table, parent_field, old_id, new_id) -> Result<u64, Error>;
pub async fn lock_record(mm, db_id, txn_id, table, id) -> Result<Map<String, Value>, Error>;  // FOR UPDATE
pub async fn select_row_json(mm, db_id, txn_id, table, id) -> Result<Value, Error>;
pub async fn find_ids_by_name_like(mm, db_id, table, col, keyword) -> Result<Vec<i64>, Error>;

// md_accessor.rs
pub async fn write_audit(mm, db_id, txn_id, dict_code, record_id, version, action,
    source_cr_id: Option<i64>, field: Option<&str>,
    old_value: Option<Value>, new_value: Option<Value>, operated_by) -> Result<i64, Error>;
pub async fn write_event(mm, db_id, txn_id, dict_code, record_id, event_type, payload) -> Result<String, Error>;
pub async fn try_set_cr_status(mm, db_id, txn_id, cr_id, from: &[&str], to: &str) -> Result<bool, Error>;
pub async fn cr_updated_before(mm, db_id, cr_id, minutes) -> Result<bool, Error>; // 懒同步自愈窗口
```

### 匹配 / 扫描 / 分发（match_store / scan_store / dispatch_store，部分）

```rust
pub use match_store::{ load_published, load_suspects, load_by_ids,
    insert_match_group, transition_match_group, list_match_groups, count_merge_by_status,
    list_audit, list_events, upsert_subscription, /* … */ };
pub use scan_store::{ insert_findings, list_scans, transition_scan_status,
    count_scan_by_status, PreparedCluster, InsertStats };
pub use dispatch_store::{ fanout_tick, claim_dispatches, mark_dispatch,
    retry_dispatches, skip_dispatches, dispatch_stats, reclaim_running,
    list_dispatches, get_dispatch, publish_rebuild,
    load_events_by_ids, upsert_consumer_offset, list_consumer_offsets, /* … */ };
```

---

## 使用示例

### 一、激活一份 CR（api 层手动激活端点的真实调用）

```rust
use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_model::codegen::RandomCodeGenerator;
use cmx_mdm_store_pg as store;

async fn run_activate(cr_id: i64, operated_by: i64) -> Result<i64, cmx_api_types::Error> {
    let mm = get_default_pg_db_manager();
    let db_id = "default";

    // 激活器七步单事务（内部优先走 GlobalCodeMinter 铸号，未注入时用占位码兜底）：
    // 读 CR → 读映射 → 头处理 → 明细 → 审计 → 事件 → CR 归档（approving→activated）
    let record_id = store::activate(mm, db_id, cr_id, operated_by, &RandomCodeGenerator).await?;
    Ok(record_id)
}
```

### 二、合并两条主数据（管家确认合并请求）

```rust
use std::collections::HashMap;
use cmx_mdm_model::survivorship::SurvivorRule;
use cmx_mdm_store_pg::{merge, LineTableInfo};

async fn run_merge() -> Result<(), cmx_api_types::Error> {
    let mm = cmx_database_pg::get_default_pg_db_manager();
    let db_id = "default";

    // 字段级存活策略：tax_no 按时间新者，其余默认 MasterFirst
    let mut rules = HashMap::new();
    rules.insert("tax_no".to_string(), SurvivorRule::Latest);

    // 明细表清单：从激活映射聚合而来（去重键由 DCT uniqueKeys 推导；空=不去重全量迁移）
    let line_tables = vec![LineTableInfo {
        table: "cm_bank_account".into(),
        parent_field: "supplier_id".into(),
        dedup_keys: vec!["account_no".into()],
    }];

    // 十步单事务：master=1001 存活，victim=1002 → merged；返回「迁移 X 条 / 去重 Y 条」
    let stats = merge(
        mm, db_id, "supplier", "cm_supplier",
        1001, &[1002],
        &["name".into(), "tax_no".into()],
        &rules,
        &Default::default(),      // 无人工裁决覆盖
        &line_tables,
        42,                       // operated_by
        5001,                     // match_group_id（CAS pending→reviewed）
    ).await?;

    println!("合并完成：master={} 迁移 {} 条明细，去重 {} 条",
             stats.master_id, stats.reparented_total, stats.deduped_total);
    Ok(())
}
```

### 三、抢占式改 CR 状态（三方并发收敛）

```rust
use cmx_database_pg::get_default_pg_db_manager;
use cmx_mdm_store_pg as store;

async fn submit_cr(cr_id: i64) -> Result<(), cmx_api_types::Error> {
    let mm = get_default_pg_db_manager();
    // 条件 UPDATE：仅 draft/rejected 才迁移到 approving。
    // 双击/双端并发时只有一次成功（返回 true），其余 false → 上层返回 409。
    // 同语句刷 update_time —— 它是懒同步「approving 且无实例超 5 分钟回退 draft」的计时起点。
    let won = store::try_set_cr_status_pub(
        mm, "default", None, cr_id,
        &["draft", "rejected"], "approving",
    ).await?;

    if !won {
        return Err(cmx_api_types::Error::business_error("单据状态已变更，请刷新后重试"));
    }
    // … 后续：读 CR 头、防孤儿实例检查、发起流程实例 …
    Ok(())
}
```

### 四、分发扇出与投递认领（dispatcher 循环每轮的真实序列）

```rust
use cmx_mdm_store_pg as store;

async fn dispatch_tick(db_id: &str) -> Result<(), cmx_api_types::Error> {
    let mm = cmx_database_pg::get_default_pg_db_manager();

    // ① 扇出：把 md_event_log 新事件按活跃订阅展开为 md_dispatch_log 投递实例。
    //    第 4 参是订阅过滤谓词（生产环境传 cmx-mdm-api 的 transform::event_matches_sub，
    //    按 dictCode/eventType 过滤），水位窗口 FOR UPDATE 互斥，失败回滚下一轮重扫。
    let n = store::fanout_tick(
        mm, db_id, 500 /*batch*/,
        &|event, sub| {
            event["dictCode"] == sub["dictCode"] || sub["dictCode"].is_null()
        },
    ).await?;

    // ② 回收：running 状态超过阈值（分钟）的投递重置回 pending（进程崩溃残留自愈）
    store::reclaim_running(mm, db_id, 10).await?;

    // ③ 认领：取一批 pending/到期 failed 投递（FOR UPDATE SKIP LOCKED 抢占，
    //    同订阅按 event_seq 保序，阻塞式投递不乱序）
    let dispatches = store::claim_dispatches(mm, db_id, 100).await?;

    // ④ 回写：按通道投递结果标记状态（delivered / failed / dead）。
    //    failed 需给 next_retry_at_epoch（退避后约 unix 秒）；retryable=false 直接置 dead。
    // for d in &dispatches {
    //     store::mark_dispatch(mm, db_id, d["id"].as_i64().unwrap(), "delivered",
    //         d["attempts"].as_i64().unwrap() + 1,
    //         None, Some(200), None, None).await?;
    // }
    let _ = n;
    Ok(())
}
```

---

## 常见问题

### Q1: 为什么 store 是自由函数而不是 struct + 方法？

对齐 `cmx-dct-store-pg` 的既有惯例：DB 连接从 `get_default_pg_db_manager()` 全局单例取，
不持有 per-instance 状态，自由函数最简洁且与 handler 调用形态（`store::xxx(mm, …)`）一致。

### Q2: 可空 BIGINT / JSONB 列绑定有什么坑？

裸 `DataValue::Null` 会被绑成 VARCHAR NULL，BIGINT/JSONB 列拒收。统一口径：可空
BIGINT 用 `DataValue::from(Option<i64>)`（NullTyped(Int)），可空 JSONB 用
`NullTyped(SqlTypeMarker::Json)`（见 `match_store/mod.rs` 模块文档）。

### Q3: 激活器 create 分支为什么复用 `cmx-dct-store-pg::dict_upsert`？

字典表的行级校验（NOT NULL、列类型、唯一键）与分级字典的 hierarchy 维护已沉淀在
dct-store-pg；复用可保证「激活落库」与「字典直存」两条路径行为一致，且 upsert 可携带
txn_id 纳入激活主事务（要么同成要么同败）。
