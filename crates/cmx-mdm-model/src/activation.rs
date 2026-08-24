//! 激活器纯逻辑:字段搬运规则(配置驱动,无 DB)。
//!
//! 读 CR 头/行(serde_json::Value)→ 按 [`ActivationConfig`] 的映射配置 → 产出 cm_* 头行数据。
//! 新建(create)/变更(update)分支、明细关联列回填、line_action 处理。
//!
//! 纯计算层:不接 DB,可单测。DB 读写由 cmx-mdm-store-pg 的各 accessor 执行。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 激活映射配置(对应 mdm_activation 一行,由 activation_store 反序列化)。
///
/// 顶层字段对齐 DB 列名(snake_case);target_table 是目标物理表名(配置器选字典时一并落库)。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActivationConfig {
    pub activation_code: String,
    pub source_doc_type: String,
    /// 变更类型 create/update(M1);merge/block/flag_delete 后续
    pub cr_type: String,
    pub target_dict: String,
    /// 目标头表物理名(如 cm_supplier)。DDL 已有 target_table 列(governance.up.sql),
    /// 配置器 UI 选字典时从 dct/meta 的 tableName 一并写入;激活器直接用此字段拼 SQL。
    #[serde(default)]
    pub target_table: String,
    /// 头映射 {单据字段: 主数据列}
    #[serde(default)]
    pub header_mapping: Map<String, Value>,
    /// 明细映射数组(JSON 内容用 camelCase 键,见 [`LineMapping`] 的 serde rename)。
    #[serde(default)]
    pub line_mappings: Vec<LineMapping>,
    pub code_rule_code: Option<String>,
    /// 主体名字段来源（payload 内字段名，前端按此填 subject_name）。
    #[serde(default)]
    pub subject_name_field: Option<String>,
    /// 【已废弃】主体编码字段来源。从未接线——激活器不读此字段，字典 code 一律走
    /// dictMeta.codeRule 铸号（与 dct 直存路径统一）。列保留不删（避免迁移风险），
    /// 配置器 UI 已移除，保存时置空以清除旧数据。
    #[serde(default)]
    pub subject_code_field: Option<String>,
    /// 头映射分组（纯 UI 展示用，不影响激活器搬运）。
    /// fields 存 header_mapping 的 key（源字段名），用它把扁平映射行归组展示。
    /// 激活器（find_by_doc_type / plan_create）不读此字段——header_mapping 落库仍扁平。
    /// 外层 snake（对齐 line_mappings 范式 + DB 列名），内层 HeaderGroup 字段 camel。
    #[serde(default)]
    pub header_groups: Vec<HeaderGroup>,
    /// 单据字段铸号规则覆盖 {单据字段: ruleCode}。
    ///
    /// 单据保存铸号时覆盖单据元数据 layers[].code_rule 同名字段（field 匹配）的 ruleCode
    /// ——激活配置优先于单据元数据。空则不覆盖（回退元数据原 ruleCode）。
    /// **激活器自身不读此字段**——由 cr-form 读取后经 saveDocData → /doc/save → saver
    /// 的 codeRuleOverrides 覆盖铸号。外层 snake 对齐 DB 列名（同 header_groups 范式）。
    #[serde(default)]
    pub doc_code_rules: Map<String, Value>,
    /// 关键信息查重字段（cr-form 步骤条 step1「关键信息」表单字段 + `/mdm/check-key` 的
    /// specs/clusterKeys 来源）。数组顺序即簇键优先级（强标识字段排前）。
    /// 空则无步骤①——create 直接进完整表单，不做查重（keyDefs 完全等于配置，不强制补主体名）。
    /// **激活器不读此字段**——由 cr-form 读取渲染查重表单并构造查重请求（后端 check-key
    /// 天生支持多字段：keyValue Map + specs 数组，加权分 ≥80 阻断）。
    #[serde(default)]
    pub key_fields: Vec<KeyField>,
}

/// 明细映射(一条 = 一类明细行,如 bank_account)。
///
/// JSON 内容键用 camelCase(对齐 DDL line_mappings 注释
/// `{lineType,targetDict,targetTable,parentIdField,fields}`),Rust 字段用 snake_case +
/// `#[serde(rename)]` 桥接。target_table 加 default 兼容历史数据。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LineMapping {
    #[serde(rename = "lineType")]
    pub line_type: String,
    #[serde(rename = "targetDict")]
    pub target_dict: String,
    #[serde(default, rename = "targetTable")]
    pub target_table: String,
    #[serde(rename = "parentIdField")]
    pub parent_field: String,
    #[serde(default)]
    pub fields: Map<String, Value>,
    /// 明细字段展示顺序（纯 UI 用：fields 经 serde Map 字母序 + jsonb 无序落库后 key 序必丢，
    /// 配置器把用户排的字段顺序存此保序数组；激活器 plan_lines 不读——遍历 fields 与顺序无关）。
    #[serde(default, rename = "fieldOrder")]
    pub field_order: Vec<String>,
}

/// 关键信息查重字段（一条 = 一个关键信息维度，如 name / tax_no）。
///
/// field 是**目标字典列名**（如 cm_supplier.name → "name"），与 header_mapping 的
/// value 同空间。cr-form 按它反查 header_mapping 得 CR 侧源字段渲染步骤①表单。
/// weight/kind 语义与 `/mdm/check-key` 的 SpecDto 一致（加权分 + Exact/EditDistance）。
/// dedup=false 的字段仅进步骤①表单采集（提前录入），不进查重请求
/// （specs/clusterKeys/keyValue 均不含）——关键信息 ≠ 全部查重。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeyField {
    /// 目标字典列名（如 "name" / "tax_no"）
    pub field: String,
    /// 查重权重（score = Σ(字段分 × weight) / Σweight），默认 100
    #[serde(default = "default_key_weight")]
    pub weight: u32,
    /// 比较方式：Exact（全等）/ EditDistance（编辑距离），默认 EditDistance
    #[serde(default = "default_key_kind")]
    pub kind: String,
    /// 是否参与查重（false = 仅步骤①展示采集，不进查重请求），默认 true
    #[serde(default = "default_key_dedup")]
    pub dedup: bool,
}
fn default_key_weight() -> u32 {
    100
}
fn default_key_kind() -> String {
    "EditDistance".into()
}
fn default_key_dedup() -> bool {
    true
}

/// 头映射分组（一条 = 一个展示分组，如「基础信息」「工商资质」）。
///
/// 纯 UI 组织用：fields 列出归入本组的 header_mapping key（源字段名），
/// 渲染时按此把扁平映射行分区展示。激活器不读此结构。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HeaderGroup {
    #[serde(rename = "groupCode")]
    pub group_code: String,
    #[serde(rename = "groupName")]
    pub group_name: String,
    #[serde(default)]
    pub fields: Vec<String>,
}

/// 激活器产出:要写入 cm_* 的头行数据(供 dct_accessor 执行)。
#[derive(Debug, Clone)]
pub struct ActivationPlan {
    /// 头:目标列 → 值(已按 header_mapping 搬运 + 补 lifecycle_status='published')
    pub header_row: Map<String, Value>,
    /// 明细:每条 = (目标明细表, 关联列名, 行数据)
    pub line_rows: Vec<(String, String, Map<String, Value>)>,
}

/// 明细处理产出（diff 方案）：inserts（新增行）+ updates（改已有行）。
///
/// - 无 `line_target_id` 的 CR 行 → [`LinesPlan::inserts`]（新增 cm_* 明细）
/// - 有 `line_target_id` 的 CR 行 → [`LinesPlan::updates`]（UPDATE 该 cm_* 明细，id 稳定）
/// - cm_* 现有但 CR 未提及的 → 由激活器 diff `select_line_keys` 算出 to_delete 软删（不在本结构）
#[derive(Debug, Clone, Default)]
pub struct LinesPlan {
    /// 新增明细：(目标明细物理表, 关联列名, 行数据)
    pub inserts: Vec<(String, String, Map<String, Value>)>,
    /// 改已有明细：(目标明细物理表, cm_* 明细 id, 行数据)
    pub updates: Vec<(String, i64, Map<String, Value>)>,
}

/// 「未填」判断：`null` 或空字符串视作未提供（与 `cmx-biz` NOT NULL 校验「空串=missing」语义一致）。
///
/// 激活器搬运时跳过这些值——让目标表 DEFAULT / 服务端 backfill（如 status=1、sort_no=0）生效，
/// 避免空串写入 INT/DATE 等强类型列触发「类型不匹配」校验失败或 DB 绑定错误。
/// （前端表单数值框留空时回传空串，是业务最常见的「未填」形态。）
fn is_unfilled(v: &Value) -> bool {
    v.is_null() || v.as_str().is_some_and(str::is_empty)
}

/// 仅空字符串判断（update 场景：null 是显式清空意图，须保留给落库层 SET col=NULL）。
fn is_empty_str(v: &Value) -> bool {
    v.as_str().is_some_and(str::is_empty)
}

/// 按 mapping 把 CR 头字段搬运成 cm_* 头行(create 分支)。
///
/// - `cfg`:激活映射配置
/// - `cr_head`:cv_mdm_apply 头记录(字段名 → 值)
/// - `new_code`:新建时由 [`crate::codegen::CodeGenerator`] 产出的 code
pub fn plan_create(cfg: &ActivationConfig, cr_head: &Map<String, Value>, new_code: &str) -> ActivationPlan {
    let mut header_row = Map::new();
    // 通用回退:先查 payload 内(业务字段),再查 cr_head 顶层(公共搜索列)
    let payload_obj = cr_head.get("payload").and_then(|v| v.as_object());
    for (src_field, tgt_col) in &cfg.header_mapping {
        let val = payload_obj
            .and_then(|p| p.get(src_field))
            .or_else(|| cr_head.get(src_field));
        // null/空串跳过：未填字段不搬运，让目标表 DEFAULT / backfill（status=1、sort_no=0）兜底，
        // 避免空串落 INT/DATE 列触发「类型不匹配」（见 build_upsert_sql_dv 的 backfill 仅对未提供列生效）。
        if let Some(tgt) = tgt_col.as_str()
            && let Some(v) = val
            && !is_unfilled(v)
        {
            header_row.insert(tgt.to_string(), v.clone());
        }
    }
    // subject_name 是 CR 的专用主体名称列：cr-form 的 buildHead 把 nameFieldKey（即
    // subject_name_field）的值存到 subject_name，并故意排除出 payload（避免重复）。因此
    // header_mapping 按 payload[src]/cr_head[src] 查找时取不到它（name 既不在 payload 也不在
    // 顶层）。这里按 cfg.subject_name_field（目标字典的名称列名，如 "name"）把 subject_name
    // 单独搬运，补齐映射——否则目标表 name(NOT NULL) 落空报「供应商名称不能为空」。
    if let Some(name_col) = cfg.subject_name_field.as_deref()
        && !name_col.is_empty()
    {
        if let Some(v) = cr_head.get("subject_name")
            && !is_unfilled(v)
        {
            header_row.insert(name_col.to_string(), v.clone());
        }
    }
    header_row.insert("code".into(), Value::String(new_code.to_string()));
    // 闸口:强制 published(V3.1 dct_accessor 唯一写入入口约束)
    header_row.insert("lifecycle_status".into(), Value::String("published".to_string()));
    header_row.insert("published_version".into(), Value::Number(1.into()));
    ActivationPlan { header_row, line_rows: vec![] }
}

/// 按 mapping 把 CR 头字段搬运成 update delta(update 分支)。
///
/// 变更:只搬 field_deltas 里的新值(不覆盖整行);version+1。
///
/// - `field_deltas`:`{field: {old, new}}`,取 new 按 header_mapping 落到目标列
/// - `current_version`:目标记录当前 published_version(乐观锁快照)
pub fn plan_update(
    cfg: &ActivationConfig,
    _cr_head: &Map<String, Value>,
    field_deltas: &Value,
    current_version: i64,
) -> ActivationPlan {
    let mut header_row = Map::new();
    if let Some(deltas) = field_deltas.as_object() {
        for (src_field, tgt_col) in &cfg.header_mapping {
            if let Some(tgt) = tgt_col.as_str()
                && let Some(delta) = deltas.get(src_field)
                && let Some(new_val) = delta.get("new")
                && !is_empty_str(new_val)
            {
                // 空串跳过（前端表单未改的空串）；null 保留——update 时是显式清空意图，落库 SET col=NULL。
                header_row.insert(tgt.to_string(), new_val.clone());
            }
        }
        // 同 plan_create：subject_name 是 CR 专用列，update 时名称变更存在 deltas['subject_name']
        // （cr-form buildHead 的 deltas key 是 'subject_name'），header_mapping 按 src_field
        // ('name') 查 deltas 查不到。按 cfg.subject_name_field 把 deltas['subject_name'].new
        // 搬到目标名称列。
        if let Some(name_col) = cfg.subject_name_field.as_deref()
            && !name_col.is_empty()
        {
            if let Some(delta) = deltas.get("subject_name")
                && let Some(new_val) = delta.get("new")
                && !is_empty_str(new_val)
            {
                header_row.insert(name_col.to_string(), new_val.clone());
            }
        }
    }
    header_row.insert("published_version".into(), Value::Number((current_version + 1).into()));
    // 变更不改 lifecycle_status(保持 published)
    ActivationPlan { header_row, line_rows: vec![] }
}

/// 按 line_mappings 把 CR 行搬运成明细变更（diff 方案：insert / update）。
///
/// 遍历 cr_lines，按 line_type 匹配 mapping，按 `line_target_id` 区分：
/// - **无 `line_target_id`** → [`LinesPlan::inserts`]（新增明细：回填关联列 + 强制 published）
/// - **有 `line_target_id`** → [`LinesPlan::updates`]（改已有 cm_* 明细：只搬业务字段，
///   不改关联列/lifecycle；id 稳定）
///
/// `line_target_id` 由前端 cr-form 在 update 模式预填 cm_* 明细时写入（指向主数据明细 id）。
/// cm_* 现有但 CR 未提及的明细（= 用户删除的）不在此结构——由激活器 diff `select_line_keys` 算出 to_delete。
pub fn plan_lines(
    cfg: &ActivationConfig,
    cr_lines: &[Value],
    header_id: i64,
) -> LinesPlan {
    let mut plan = LinesPlan::default();
    for line in cr_lines {
        let Some(line_obj) = line.as_object() else { continue };
        let Some(line_type) = line_obj.get("line_type").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(lm) = cfg.line_mappings.iter().find(|m| m.line_type == line_type) else {
            continue;
        };
        // 搬运业务字段（line_payload → 目标列），null/空串跳过
        let mut row = Map::new();
        if let Some(payload) = line_obj.get("line_payload").and_then(|v| v.as_object()) {
            for (src, tgt) in &lm.fields {
                // null/空串跳过（同 plan_create）：让明细表 DEFAULT / backfill 兜底，避免空串落强类型列。
                if let Some(t) = tgt.as_str()
                    && let Some(v) = payload.get(src)
                    && !is_unfilled(v)
                {
                    row.insert(t.to_string(), v.clone());
                }
            }
        }
        // diff：有 line_target_id = 改已有明细；无 = 新增明细
        match line_obj.get("line_target_id").and_then(|v| v.as_i64()) {
            Some(tid) => {
                // update：只搬业务字段（不改关联列/lifecycle，id 稳定）
                plan.updates.push((lm.target_table.clone(), tid, row));
            }
            None => {
                // insert：回填关联列指向头表 + 强制 published
                row.insert(lm.parent_field.clone(), Value::Number(header_id.into()));
                row.insert("lifecycle_status".into(), Value::String("published".to_string()));
                plan.inserts.push((lm.target_table.clone(), lm.parent_field.clone(), row));
            }
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_cfg() -> ActivationConfig {
        // 模拟 mdm_activation 一行(经 find_by_doc_type 反序列化)
        serde_json::from_value(json!({
            "activation_code": "supplier_apply",
            "source_doc_type": "mdm_supplier_apply",
            "cr_type": "create",
            "target_dict": "supplier",
            "target_table": "cm_supplier",
            "header_mapping": { "name": "name", "tax_no": "tax_no" },
            "line_mappings": [{
                "lineType": "bank_account",
                "targetDict": "supplier_bank",
                "targetTable": "cm_bank_account",
                "parentIdField": "supplier_id",
                "fields": { "account_no": "account_no" }
            }],
            "code_rule_code": null,
            "subject_name_field": "name"
        }))
        .unwrap()
    }

    #[test]
    fn key_fields_default_empty_and_defaults_fill_weight_kind() {
        // 旧数据无 key_fields 列 → serde default 空 Vec（回退单字段查重，兼容）
        let cfg = sample_cfg();
        assert!(cfg.key_fields.is_empty());
        // 配置了 key_fields 但漏 weight/kind/dedup → 填默认（100 / EditDistance / true）
        let v = serde_json::from_value::<ActivationConfig>(json!({
            "activation_code": "x", "source_doc_type": "d", "cr_type": "create",
            "target_dict": "supplier", "target_table": "cm_supplier",
            "key_fields": [
                { "field": "tax_no", "weight": 40, "kind": "Exact" },
                { "field": "name" },
                { "field": "supplier_type", "dedup": false }
            ]
        }))
        .unwrap();
        assert_eq!(v.key_fields.len(), 3);
        assert_eq!(v.key_fields[0].field, "tax_no");
        assert_eq!(v.key_fields[0].weight, 40);
        assert_eq!(v.key_fields[0].kind, "Exact");
        assert!(v.key_fields[0].dedup);
        assert_eq!(v.key_fields[1].field, "name");
        assert_eq!(v.key_fields[1].weight, 100);
        assert_eq!(v.key_fields[1].kind, "EditDistance");
        assert!(v.key_fields[1].dedup);
        // dedup=false：仅展示采集，不进查重
        assert_eq!(v.key_fields[2].field, "supplier_type");
        assert!(!v.key_fields[2].dedup);
        // 序列化往返无损（配置器 collectForm 发的就是这套 camel 键）
        let back = serde_json::to_value(&v).unwrap();
        assert_eq!(
            back.get("key_fields").unwrap(),
            &json!([
                { "field": "tax_no", "weight": 40, "kind": "Exact", "dedup": true },
                { "field": "name", "weight": 100, "kind": "EditDistance", "dedup": true },
                { "field": "supplier_type", "weight": 100, "kind": "EditDistance", "dedup": false }
            ])
        );
    }

    #[test]
    fn plan_create_carries_mapped_fields_and_forces_published() {
        let cfg = sample_cfg();
        // 业务字段（name/tax_no）走 payload；公共搜索列 subject_name 留顶层
        let cr_head = serde_json::from_value(json!({
            "subject_name": "B公司",
            "payload": { "name": "B公司", "tax_no": "911", "extra": "忽略" }
        })).unwrap();
        let plan = plan_create(&cfg, &cr_head, "SUPPLI-abc");
        assert_eq!(plan.header_row.get("name").and_then(|v| v.as_str()), Some("B公司"));
        assert_eq!(plan.header_row.get("tax_no").and_then(|v| v.as_str()), Some("911"));
        assert_eq!(plan.header_row.get("code").and_then(|v| v.as_str()), Some("SUPPLI-abc"));
        // extra 未在 header_mapping,不搬运
        assert!(plan.header_row.get("extra").is_none());
        // 闸口:强制 published
        assert_eq!(plan.header_row.get("lifecycle_status").and_then(|v| v.as_str()), Some("published"));
        assert_eq!(plan.header_row.get("published_version").and_then(|v| v.as_i64()), Some(1));
    }

    #[test]
    fn plan_create_skips_empty_and_null_fields() {
        // 前端表单数值框留空时回传空串（业务最常见的「未填」形态）。
        // 激活器搬运时须跳过空串/null，让目标表 DEFAULT / 服务端 backfill（status=1、sort_no=0）生效，
        // 否则空串落 INT 列会触发「类型不匹配」校验失败。
        let mut cfg = sample_cfg();
        cfg.header_mapping = serde_json::from_value(json!({
            "name": "name", "tax_no": "tax_no", "status": "status", "sort_no": "sort_no"
        })).unwrap();
        let cr_head = serde_json::from_value(json!({
            "payload": { "name": "A公司", "tax_no": "911", "status": "", "sort_no": null }
        })).unwrap();
        let plan = plan_create(&cfg, &cr_head, "SUPPLI-x");
        // 有值字段正常搬运
        assert_eq!(plan.header_row.get("name").and_then(|v| v.as_str()), Some("A公司"));
        assert_eq!(plan.header_row.get("tax_no").and_then(|v| v.as_str()), Some("911"));
        // 空串 / null 字段不搬运（交给 backfill）
        assert!(plan.header_row.get("status").is_none(), "空串 status 应跳过");
        assert!(plan.header_row.get("sort_no").is_none(), "null sort_no 应跳过");
    }

    #[test]
    fn plan_create_maps_subject_name_when_payload_misses_it() {
        // 真实 cr-form 场景：供应商名称只存在 cr_head.subject_name（buildHead 把 nameFieldKey 值
        // 存到 subject_name 并排除出 payload）。header_mapping 的 name→name 按 payload[src]/
        // cr_head[src] 查不到，须由 cfg.subject_name_field 把 subject_name 搬到目标列。
        let cfg = sample_cfg(); // subject_name_field = "name"
        let cr_head = serde_json::from_value(json!({
            "subject_name": "张三供应商",
            "payload": { "tax_no": "911" }   // payload 故意不含 name（对齐 cr-form buildHead）
        })).unwrap();
        let plan = plan_create(&cfg, &cr_head, "GYS-001");
        // subject_name 经 subject_name_field 映射到目标 name 列（不再为空）
        assert_eq!(plan.header_row.get("name").and_then(|v| v.as_str()), Some("张三供应商"));
        assert_eq!(plan.header_row.get("tax_no").and_then(|v| v.as_str()), Some("911"));
    }

    #[test]
    fn plan_update_skips_empty_str_but_keeps_null() {
        // update 场景：空串=前端未改（跳过）；null=显式清空（保留落库 SET col=NULL）。
        let mut cfg = sample_cfg();
        cfg.header_mapping = serde_json::from_value(json!({
            "tax_no": "tax_no", "status": "status"
        })).unwrap();
        let cr_head = Map::new();
        let deltas = json!({
            "tax_no": { "old": "911", "new": "" },
            "status": { "old": 1, "new": null }
        });
        let plan = plan_update(&cfg, &cr_head, &deltas, 1);
        // 空串跳过（不更新）
        assert!(plan.header_row.get("tax_no").is_none(), "空串 new 应跳过不更新");
        // null 保留（显式清空）
        assert!(plan.header_row.get("status").and_then(|v| v.as_null()).is_some(), "null new 应保留");
    }

    #[test]
    fn plan_update_takes_new_value_from_deltas_and_bumps_version() {
        let mut cfg = sample_cfg();
        cfg.cr_type = "update".into();
        let cr_head = Map::new();
        let deltas = json!({ "tax_no": { "old": "911", "new": "922" }, "name": { "old": "B", "new": "B公司" } });
        let plan = plan_update(&cfg, &cr_head, &deltas, 3);
        assert_eq!(plan.header_row.get("tax_no").and_then(|v| v.as_str()), Some("922"));
        assert_eq!(plan.header_row.get("name").and_then(|v| v.as_str()), Some("B公司"));
        assert_eq!(plan.header_row.get("published_version").and_then(|v| v.as_i64()), Some(4));
        // 变更不改 lifecycle_status
        assert!(plan.header_row.get("lifecycle_status").is_none());
    }

    #[test]
    fn plan_update_maps_subject_name_rename() {
        // 真实 cr-form 场景：供应商改名时 cr-form 把变更存到 deltas['subject_name']
        // （key 是 subject_name，非 header_mapping 的 'name'）。plan_update 须按
        // subject_name_field 把 new 搬到目标 name 列。
        let cfg = sample_cfg(); // subject_name_field = "name"
        let deltas = json!({
            "subject_name": { "old": "旧名", "new": "新名" },
            "tax_no": { "old": "911", "new": "922" }
        });
        let plan = plan_update(&cfg, &Map::new(), &deltas, 1);
        // subject_name 改名 → 目标 name 列
        assert_eq!(plan.header_row.get("name").and_then(|v| v.as_str()), Some("新名"));
        // tax_no 正常经 header_mapping 搬运
        assert_eq!(plan.header_row.get("tax_no").and_then(|v| v.as_str()), Some("922"));
        assert_eq!(plan.header_row.get("published_version").and_then(|v| v.as_i64()), Some(2));
    }

    #[test]
    fn plan_lines_diffs_insert_and_update() {
        let cfg = sample_cfg();
        let cr_lines = vec![
            // 新增明细：无 line_target_id → inserts
            json!({ "line_type": "bank_account", "line_action": "insert",
                    "line_payload": { "account_no": "工行6222" } }),
            // 改已有明细：有 line_target_id（指向 cm_bank_account id）→ updates
            json!({ "line_type": "bank_account", "line_action": "insert",
                    "line_target_id": 9001,
                    "line_payload": { "account_no": "工行9999" } }),
        ];
        let plan = plan_lines(&cfg, &cr_lines, 8001);
        // 新增行进 inserts（回填关联列 + 强制 published）
        assert_eq!(plan.inserts.len(), 1);
        let (table, parent_col, row) = &plan.inserts[0];
        assert_eq!(table, "cm_bank_account");
        assert_eq!(parent_col, "supplier_id");
        assert_eq!(row.get("account_no").and_then(|v| v.as_str()), Some("工行6222"));
        assert_eq!(row.get("supplier_id").and_then(|v| v.as_i64()), Some(8001));
        assert_eq!(row.get("lifecycle_status").and_then(|v| v.as_str()), Some("published"));
        // 改已有行进 updates（只业务字段，id 稳定，无关联列/lifecycle）
        assert_eq!(plan.updates.len(), 1);
        let (utable, uid, urow) = &plan.updates[0];
        assert_eq!(utable, "cm_bank_account");
        assert_eq!(*uid, 9001);
        assert_eq!(urow.get("account_no").and_then(|v| v.as_str()), Some("工行9999"));
        assert!(urow.get("supplier_id").is_none(), "update 行不应回填关联列");
        assert!(urow.get("lifecycle_status").is_none(), "update 行不改 lifecycle");
    }

    #[test]
    fn activation_config_deserializes_camel_case_line_mappings() {
        // 验证 LineMapping 的 serde rename 生效(从 DB JSON 反序列化)
        let cfg = sample_cfg();
        assert_eq!(cfg.target_table, "cm_supplier");
        assert_eq!(cfg.line_mappings.len(), 1);
        let lm = &cfg.line_mappings[0];
        assert_eq!(lm.line_type, "bank_account");
        assert_eq!(lm.target_table, "cm_bank_account");
        assert_eq!(lm.parent_field, "supplier_id");
    }
}
