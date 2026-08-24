# cmx-mdm-model

> 主数据管理（MDM）模块的语义中立层：纯逻辑、DB-free——承载主数据生命周期状态、激活器字段搬运规则、匹配/聚类算法、字段级存活策略与分发通道契约，可独立单测。

[![Version](https://img.shields.io/badge/version-0.1.12-blue.svg)]()
[![Edition](https://img.shields.io/badge/rust--edition-2024-orange.svg)]()

---

## 项目简介

`cmx-mdm-model` 是 MDM 域三件套中的**领域模型层**。它不依赖任何数据库或 Web 框架，
只包含数据结构与纯算法，是 api 层与 store-pg 层共享的「语言」：激活器用它把 CR 单据
搬运成主数据行，查重引擎用它分块/比较/聚类，合并引擎用它逐字段裁决存活值，分发引擎
用它定义通道行为契约。

**V3 铁律**：`cm_*` 主数据只存 [`LifecycleStatus::Published`]。草稿一律走 CR 单据
（`cv_mdm_apply`），审批通过由激活器（store-pg）落字典为 `published`；冻结/归档/合并是
published 之后的终态流转。激活器是写入 `cm_*` published 的唯一入口。

### 五块纯逻辑

| 模块 | 一句话定位 |
|------|-----------|
| `activation` | 配置驱动的字段搬运：CR 头/行（`serde_json::Value`）→ 按 `ActivationConfig` 映射 → 产出 `cm_*` 头行数据 |
| `match_algo` | 查重四阶段流水线：分块 → 加权比较 → 双阈值裁决 → 聚类（把 O(N²) 压到近线性） |
| `survivorship` | 合并时逐字段从 master/victim 取真值（三种存活策略 + 留痕日志） |
| `distribution` | 分发契约层：事件信封 `EventEnvelope` / 投递结果 `DeliveryResult` / 通道 trait `DistributionChannel` |
| `codegen` | 编码生成抽象：`CodeGenerator` trait + `RandomCodeGenerator` 兜底实现（`<DICT>-<雪花36进制>`） |

---

## 与其他 crate 的关系

### 上游依赖（本 crate 依赖）

| 依赖 | 用途 |
|------|------|
| `cmx-core` | 核心类型（`DataValue` 等） |
| `cmx-utils` | `next_pk_id()` 雪花 id（RandomCodeGenerator 随机段） |
| `cmx-biz` | 业务错误 + 落库前列级校验（DOC/DCT 共享，留在 cmx-biz） |
| `serde` / `serde_json` | DTO 反序列化 + 枚举序列化 / 弱类型 Value（扩展属性·提议值） |
| `chrono` | 生效日判断（effective_date） |
| `tracing` | match_algo 块超上限护栏 warn |
| `async-trait` | `DistributionChannel` trait 的异步方法 |

### 下游使用方（谁依赖本 crate）

| 使用方 | 引用方式 | 实际用途 |
|--------|---------|---------|
| `cmx-mdm-store-pg` | `cmx-mdm-model = { workspace = true }` | 激活器调 `plan_create`/`plan_update`/`plan_lines` 搬运数据；合并调 `survive`；`ActivationConfig` 反序列化自 `mdm_activation` 表 |
| `cmx-mdm-api` | `cmx-mdm-model = { workspace = true }` | handler 用 `ActivationConfig` 接收配置器保存请求、`RandomCodeGenerator` 作手动激活的兜底铸号、`DistributionChannel` trait 驱动分发引擎 |
| `cmx-portalservice`（跨 workspace） | 经 `cmx-platform-app` 间接依赖 | 门户进程承载 MDM 功能时的传递依赖 |

无反向依赖：本 crate 不依赖 `cmx-mdm-api` / `cmx-mdm-store-pg`（分层无环）。

---

## 核心功能与特性

| 功能 | 说明 |
|------|------|
| 生命周期状态枚举 | `LifecycleStatus`：published / frozen / archived / merged（只 4 值，无 draft），`as_str()` 落库、`default_for_new()` 返回 Published |
| 激活映射配置模型 | `ActivationConfig`（对齐 `mdm_activation` 列名）+ `LineMapping`（明细映射，camelCase JSON 桥接）+ `HeaderGroup`（UI 分组）+ `KeyField`（关键信息查重字段） |
| 头搬运（create） | `plan_create`：payload 优先/顶层回退取值，跳过 null/空串（让目标表 DEFAULT 兜底），强制 `lifecycle_status='published'` + `published_version=1` |
| 头搬运（update） | `plan_update`：只搬 `field_deltas` 的 new 值；空串跳过（未改）、null 保留（显式清空）；`published_version` +1，不改 lifecycle |
| 明细搬运（diff） | `plan_lines`：按 `line_target_id` 有无区分 inserts（回填关联列 + 强制 published）/ updates（id 稳定，只搬业务字段） |
| subject_name 专列搬运 | CR 的 `subject_name` 被前端排除出 payload，`plan_create`/`plan_update` 按 `subject_name_field` 单独补齐映射，避免目标表 NOT NULL 落空 |
| 分块 blocking | 按簇键优先级取第一个非空值分桶；NULL 巨簇防护 + 桶上限 `BLOCK_CAP=500` 截断告警 |
| 加权比较 compare | `score = Σ(field_score × weight) / Σweight`（0-100）；Exact 全等 / EditDistance 归一化编辑距离；双侧均空字段跳过（不计入分母） |
| 双阈值裁决 decide | ≥95 AutoMerge / 80–94 Review / <80 NoMatch |
| 锚点查重 find_candidates | target vs all（先分块再块内比较），排除自身，返回 score≥80 候选（按分降序） |
| 全库聚类 scan_clusters | 普查模式：块内两两比较取 max score，≥min_score 入簇，≥2 成员成簇 |
| 字段级存活 survive | MasterFirst（master 非空优先）/ Fullest（非空优先，都非空 master）/ Latest（按 update_time 新者）；每字段留 `SurvivorLogEntry` |
| 分发契约 | `EventEnvelope`（camelCase 信封：eventId 幂等键 / seq 全局单调 / version 记录级单调）+ `DeliveryResult::ok/fail` + `DistributionChannel` trait（validate_config / deliver / health_check） |
| 编码生成抽象 | `CodeGenerator` trait（`generate(dict_code, rule_code)`）+ `RandomCodeGenerator`：前 6 字符大写前缀 + 雪花 id 36 进制，UNIQUE 约束兜底 |

---

## 模块结构

```text
cmx-mdm-model
├── src
│   ├── lib.rs            # LifecycleStatus / MdmQuery + 模块声明与文档
│   ├── activation.rs     # ActivationConfig/LineMapping/KeyField/HeaderGroup + plan_create/plan_update/plan_lines
│   ├── match_algo.rs     # blocking/compare/decide/find_candidates/scan_clusters + Levenshtein（两行 DP）
│   ├── survivorship.rs   # SurvivorRule/SurvivorLogEntry/survive（逐字段取真值 + 留痕）
│   ├── distribution.rs   # EventEnvelope/DeliveryResult/DistributionChannel trait（通道无关契约）
│   └── codegen.rs        # CodeGenerator trait + RandomCodeGenerator + format_radix
└── Cargo.toml
```

---

## 关键类型 / API

```rust
// lib.rs —— 主数据生命周期（cm_*.lifecycle_status 列值域）
pub enum LifecycleStatus { Published, Frozen, Archived, Merged }
impl LifecycleStatus {
    pub fn as_str(&self) -> &'static str;      // "published" / "frozen" / "archived" / "merged"
    pub fn default_for_new() -> Self;          // Published
}

// activation.rs —— 激活映射（对应 mdm_activation 一行）与三个纯函数
pub struct ActivationConfig {
    pub activation_code: String,
    pub source_doc_type: String,
    pub cr_type: String,                       // create / update（merge/block 后续）
    pub target_dict: String,
    pub target_table: String,                  // 目标头表物理名（如 cm_supplier）
    pub header_mapping: Map<String, Value>,    // {单据字段: 主数据列}
    pub line_mappings: Vec<LineMapping>,       // 明细映射数组（内层 camelCase）
    pub code_rule_code: Option<String>,
    pub subject_name_field: Option<String>,    // 主体名字段来源（payload 内字段名）
    pub header_groups: Vec<HeaderGroup>,       // 纯 UI 分组（激活器不读）
    pub doc_code_rules: Map<String, Value>,    // 单据字段铸号规则覆盖（saver 层读，激活器不读）
    pub key_fields: Vec<KeyField>,             // 关键信息查重字段（cr-form/check-key 用）
}
pub struct LineMapping { /* lineType/targetDict/targetTable/parentIdField/fields/fieldOrder */ }
pub struct KeyField { pub field: String, pub weight: u32 /*默认100*/,
                      pub kind: String /*默认EditDistance*/, pub dedup: bool /*默认true*/ }

pub struct ActivationPlan { pub header_row: Map<String, Value>,
                            pub line_rows: Vec<(String, String, Map<String, Value>)> }
pub struct LinesPlan { pub inserts: Vec<(String, String, Map<String, Value>)>,
                       pub updates: Vec<(String, i64, Map<String, Value>)> }

pub fn plan_create(cfg: &ActivationConfig, cr_head: &Map<String, Value>,
                   new_code: &str) -> ActivationPlan;
pub fn plan_update(cfg: &ActivationConfig, _cr_head: &Map<String, Value>,
                   field_deltas: &Value, current_version: i64) -> ActivationPlan;
pub fn plan_lines(cfg: &ActivationConfig, cr_lines: &[Value],
                  header_id: i64) -> LinesPlan;

// match_algo.rs —— 查重四阶段
pub struct MatchRecord { pub id: i64, pub fields: Map<String, Value> }   // cm_* published 精简投影
pub enum FieldKind { Exact, EditDistance }
pub struct MatchFieldSpec { pub field: String, pub weight: u32, pub kind: FieldKind }
pub enum Decision { AutoMerge /*≥95*/, Review /*80–94*/, NoMatch /*<80*/ }
pub struct MatchCandidate { pub record_id: i64, pub score: u8, pub decision: Decision }
pub struct DupCluster { pub cluster_key: String, pub members: Vec<MatchClusterMember> }
pub const BLOCK_CAP: usize = 500;              // 桶上限护栏（防 N×N 爆炸）

pub fn blocking<'a>(records: &'a [MatchRecord], cluster_keys: &[&str]) -> Vec<Vec<&'a MatchRecord>>;
pub fn compare(target: &MatchRecord, other: &MatchRecord, specs: &[MatchFieldSpec]) -> u8;
pub fn decide(score: u8) -> Decision;
pub fn find_candidates(target: &MatchRecord, all: &[MatchRecord],
                       specs: &[MatchFieldSpec], cluster_keys: &[&str]) -> Vec<MatchCandidate>;
pub fn scan_clusters(suspects: &[MatchRecord], specs: &[MatchFieldSpec],
                     cluster_keys: &[&str], min_score: u8) -> Vec<DupCluster>;

// survivorship.rs —— 字段级存活
pub enum SurvivorRule { MasterFirst, Fullest, Latest }
pub struct SurvivorLogEntry { pub field: String, pub from: String /*master|victim*/, pub value: Value }
pub fn survive(master: &MatchRecord, victim: &MatchRecord, survive_fields: &[String],
               rules: &HashMap<String, SurvivorRule>) -> (Map<String, Value>, Vec<SurvivorLogEntry>);

// distribution.rs —— 分发契约（实现方在 cmx-mdm-api/channels）
#[async_trait]
pub trait DistributionChannel: Send + Sync {
    fn channel_type(&self) -> &'static str;
    async fn validate_config(&self, config: &Value) -> Result<(), String>;
    async fn deliver(&self, config: &Value, envelopes: &[EventEnvelope]) -> Vec<DeliveryResult>;
    async fn health_check(&self, config: &Value) -> Result<(), String>;
}

// codegen.rs —— 编码生成抽象
pub trait CodeGenerator: Send + Sync {
    fn generate(&self, dict_code: &str, rule_code: Option<&str>) -> String;
}
pub struct RandomCodeGenerator;   // 格式：SUPPLI-<雪花id36进制>（如 SUPPLI-LS3KQ7A2）
```

---

## 使用示例

### 一、激活器头搬运（create 分支，摘自模块测试场景）

```rust
use cmx_mdm_model::activation::{ActivationConfig, plan_create};
use serde_json::json;

// 模拟 mdm_activation 一行（经 store-pg find_by_doc_type 反序列化）
let cfg: ActivationConfig = serde_json::from_value(json!({
    "activation_code": "supplier_apply",
    "source_doc_type": "mdm_supplier_apply",
    "cr_type": "create",
    "target_dict": "supplier",
    "target_table": "cm_supplier",
    "header_mapping": { "name": "name", "tax_no": "tax_no" },
    "line_mappings": [{ "lineType": "bank_account", "targetDict": "supplier_bank",
                        "targetTable": "cm_bank_account", "parentIdField": "supplier_id",
                        "fields": { "account_no": "account_no" } }],
    "subject_name_field": "name"
})).unwrap();

// CR 头：业务字段在 payload，公共搜索列 subject_name 在顶层
let cr_head = json!({ "subject_name": "B公司", "payload": { "tax_no": "911", "status": "" } });
let plan = plan_create(&cfg, cr_head.as_object().unwrap(), "GYS-001");

// 闸口断言：强制 published；空串 status 不搬运（交给目标表 DEFAULT/backfill）
assert_eq!(plan.header_row.get("lifecycle_status").unwrap(), "published");
assert_eq!(plan.header_row.get("code").unwrap(), "GYS-001");
assert!(plan.header_row.get("status").is_none());
// name 列由 subject_name_field 从 cr_head.subject_name 补齐（payload 不含 name）
assert_eq!(plan.header_row.get("name").unwrap(), "B公司");
```

### 二、查重：分块 + 加权比较 + 双阈值裁决

```rust
use cmx_mdm_model::match_algo::*;

fn rec(id: i64, credit: &str, tax: &str, name: &str) -> MatchRecord {
    MatchRecord { id, fields: serde_json::json!({
        "credit_code": credit, "tax_no": tax, "name": name
    }).as_object().unwrap().clone() }
}

// supplier 默认权重：credit_code(40) + tax_no(30) + name(30)——强标识占主导
let specs = vec![
    MatchFieldSpec { field: "credit_code".into(), weight: 40, kind: FieldKind::Exact },
    MatchFieldSpec { field: "tax_no".into(),     weight: 30, kind: FieldKind::Exact },
    MatchFieldSpec { field: "name".into(),       weight: 30, kind: FieldKind::EditDistance },
];

// 锚点查重：新录入 target vs 库内 all（分块后只比同桶，排除 O(N²)）
let target = rec(9, "C1", "T1", "华东钢铁集团有限公司");
let all = vec![rec(1, "C1", "T1", "华东钢铁集团有限公"),   // 强标识全等 + name 差 1 字
               rec(2, "C2", "T2", "完全不同的公司")];
let candidates = find_candidates(&target, &all, &specs, &["credit_code", "tax_no", "name"]);

// credit+tax 全等 + name 10 字差 1 → 加权分 ≥95 → AutoMerge（可自动合并）
assert_eq!(candidates[0].decision, Decision::AutoMerge);
```

### 三、合并存活：逐字段取真值并留痕

```rust
use cmx_mdm_model::survivorship::{survive, SurvivorRule};
use std::collections::HashMap;

let master = MatchRecord { id: 1, fields: json!({"name": "甲", "phone": ""}).as_object().unwrap().clone() };
let victim = MatchRecord { id: 2, fields: json!({"name": "乙", "phone": "111"}).as_object().unwrap().clone() };

// 未配置规则的字段默认 MasterFirst：master 非空用 master，master 空才用 victim
let (row, log) = survive(&master, &victim,
                         &["name".to_string(), "phone".to_string()], &HashMap::new());

assert_eq!(row.get("name").unwrap(), "甲");            // 双非空 → master
assert_eq!(row.get("phone").unwrap(), "111");          // master 空 → victim 兜底
assert_eq!(log.iter().find(|e| e.field == "phone").unwrap().from, "victim"); // 来源留痕
```

### 四、构造分发信封（通道实现方使用）

```rust
use cmx_mdm_model::distribution::{EventEnvelope, DeliveryResult};

let envelope = EventEnvelope {
    event_id: "evt-1".into(),          // 消费端幂等键（at-least-once 下按它去重）
    seq: 7,                            // 全局单调（delta token，可校验缺口）
    event_type: "created".into(),      // created / updated / merged
    dict_code: "supplier".into(),
    record_id: 42,
    record_code: "GYS0001".into(),
    version: 1,                        // 记录级单调（可丢弃旧版本事件兜底）
    source: "cmx-mdm",
    occurred_at: "2026-08-18T08:00:00Z".into(),
    data: serde_json::json!({"code": "GYS0001"}),  // field_map 投影后的快照
    meta: serde_json::json!({"crId": 9}),          // 溯源（crId / victim_ids 等）
};
// 序列化为 camelCase JSON（serde rename_all）作为 webhook body / 未来 MQ 消息体
let body = serde_json::to_value(&envelope).unwrap();
assert_eq!(body["eventId"], "evt-1");
```

---

## 常见问题

### Q1: 为什么匹配算法要「分块」而不是全量两两比较？

全量两两比较是 O(N²)：10 万条记录约 50 亿次比对，不可行。分块按「同簇键值」归桶后
只在桶内比较，桶都小时接近线性。两条护栏防退化：簇键全空的记录不进共享桶（NULL 巨簇
防护）；桶超过 `BLOCK_CAP=500` 截断并 warn（被截记录本桶内不参与比较）。

### Q2: 为什么 `plan_create` 跳过空串而 `plan_update` 保留 null？

create 时空串/null 都是「未填」，跳过后让目标表 DEFAULT / 服务端 backfill 生效，
避免空串写入 INT/DATE 强类型列触发「类型不匹配」；update 时空串表示「前端未改」
（跳过），null 表示「显式清空」（保留，落库 `SET col=NULL`）——语义不同，处理不同。

### Q3: `subject_code_field` 为什么标记为已废弃？

从未接线——激活器不读此字段，字典 code 一律走 `dictMeta.codeRule` 铸号（与 dct 直存
路径统一）。列保留不删（避免迁移风险），配置器 UI 已移除，保存时置空清除旧数据。
