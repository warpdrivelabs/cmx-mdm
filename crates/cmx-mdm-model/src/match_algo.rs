//! 匹配算法（M3，纯逻辑 DB-free，可单测）—— 主数据查重与聚类的核心。
//!
//! ## 为什么需要这套算法
//!
//! 多源汇入（ERP / CRM / SRM / 旧系统）导致同一实体被反复录入，形成「一物多码」。
//! 查重要找出这些重复，但**全量两两比较不可行**：N 条记录要 N×(N-1)/2 次比对，
//! 10 万条就是约 50 亿次——内存和算力都扛不住。
//!
//! 所以采用**四阶段流水线**把代价压到可控：
//!
//! 1. **分块**（[`blocking`]）：按簇键（如 credit_code）把记录分到同值桶，只比同桶。
//! 2. **比较**（[`compare`]）：块内两两算加权得分 0-100（字段级加权平均）。
//! 3. **裁决**（[`decide`]）：双阈值把分数映射成决策——≥95 AutoMerge / 80–94 Review / <80 NoMatch。
//! 4. **聚类**（[`scan_clusters`]）：全库扫描时把块内高分记录聚成重复簇，落 md_match_scan。
//!
//! 分块把 O(N²) 的比较数降到 Σ(桶大小²)，桶都小时接近线性。
//! 护栏：NULL 巨簇防护（空簇键不进共享桶）、桶上限 [`BLOCK_CAP`] = 500（超限截断+warn）。
//! 精确等值分块——模糊簇键（soundex/前缀）留作后续增强。
//!
//! 详见 `docs` / `.trae/documents/MDM主数据管理平台/` 下的算法原理文档。

use serde_json::{Map, Value};

/// 候选记录（cm_* published 行的精简投影）。
#[derive(Debug, Clone)]
pub struct MatchRecord {
    pub id: i64,
    pub fields: Map<String, Value>,
}

/// 比较字段种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// 全等得分（相等满分，不等 0；两侧均空 0 分——空值不构成匹配证据）
    Exact,
    /// 归一化编辑距离得分（两侧均空视为相等满分）
    EditDistance,
}

/// 比较字段配置（weight 为权重，u32 防溢出）。
#[derive(Debug, Clone)]
pub struct MatchFieldSpec {
    pub field: String,
    pub weight: u32,
    pub kind: FieldKind,
}

/// 双阈值裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// ≥95 自动合并
    AutoMerge,
    /// 80–94 人工评审
    Review,
    /// <80 不匹配
    NoMatch,
}

/// 匹配候选。
#[derive(Debug, Clone)]
pub struct MatchCandidate {
    pub record_id: i64,
    pub score: u8,
    pub decision: Decision,
}

/// 簇成员（scan 模式返回）。
#[derive(Debug, Clone)]
pub struct MatchClusterMember {
    pub record_id: i64,
    /// 簇内与其他成员的最高配对分。
    pub score: u8,
    pub fields: Map<String, Value>,
}

/// 重复簇（scan 模式返回结构）。
#[derive(Debug, Clone)]
pub struct DupCluster {
    /// `"credit_code:C1"` 形式，块内第一个非空簇键。
    pub cluster_key: String,
    pub members: Vec<MatchClusterMember>,
}

/// 块上限护栏（防 N×N 爆炸）。
pub const BLOCK_CAP: usize = 500;

/// 分块：按簇键优先级（cluster_keys 序）把记录分到同值桶，只比同桶。
///
/// ## 为什么分块
///
/// 全量两两比较是 O(N²)：10 万条 → 约 50 亿次比对，不可行。
/// 分块把「同簇键值」的记录归到一只桶，只在桶内两两比较——
/// 若每个桶大小都 ≤k，总比较数 = 桶数 × k(k-1)/2，接近线性。
///
/// ## 簇键选择
///
/// `cluster_keys` 按优先级序（如 `["credit_code", "tax_no", "name"]`），
/// 每条记录取**第一个非空簇键值**作桶键（`"{key}:{value}"`）。
/// 高优先级键（credit_code）强标识，同值几乎必为同一主体；低优先级键兜底。
///
/// ## 护栏
///
/// - **NULL 巨簇防护**：簇键全空的记录不进共享桶（否则所有空值挤一只桶 → 巨簇 → 退化成 N²）。
/// - **桶上限** [`BLOCK_CAP`] = 500：超限截断并 `tracing::warn`（被截记录本桶内不参与比较）。
///   M4 计划降级到次簇键再分桶，把巨桶拆小。
///
/// 精确等值分块——簇键值必须完全相等才同桶。EditDistance 字段（如 name）作簇键时，
/// 仅完全相同的名称才同桶；拼写差异由 [`compare`] 阶段的编辑距离评分兜底。
pub fn blocking<'a>(
    records: &'a [MatchRecord],
    cluster_keys: &[&str],
) -> Vec<Vec<&'a MatchRecord>> {
    use std::collections::BTreeMap;
    let mut blocks: BTreeMap<String, Vec<&'a MatchRecord>> = BTreeMap::new();

    for r in records {
        // 取第一个非空簇键（名+值）作块键；全空 → 孤儿，不进共享块（防 NULL 巨簇）
        if let Some((kname, kval)) = cluster_keys.iter().find_map(|k| {
            field_str(&r.fields, k)
                .filter(|s| !s.is_empty())
                .map(|v| (*k, v))
        }) {
            blocks.entry(format!("{kname}:{kval}")).or_default().push(r);
        }
    }

    let mut out = Vec::new();
    for (key, mut blk) in blocks {
        if blk.len() > BLOCK_CAP {
            tracing::warn!(
                target: "cmx_mdm::match", block = %key, size = blk.len(), cap = BLOCK_CAP,
                "块超上限截断（被截记录本块内不参与比较，M4 降级次簇键）"
            );
            blk.truncate(BLOCK_CAP);
        }
        // 单元素块无比较意义，不输出
        if blk.len() > 1 {
            out.push(blk);
        }
    }
    out
}

/// 比较：target vs other 的加权相似度得分（0-100，越高越像同一主体）。
///
/// ## 为什么加权平均
///
/// 不同字段对「同一主体」的证据强度不同：credit_code（统一社会信用代码）全等
/// 几乎能断定同主体；name 相似只是弱证据（重名常见）。所以每个字段配一个 weight，
/// 最终得分 = 各字段得分按权重加权平均。强标识字段权重高，兜底字段权重低。
///
/// supplier 默认权重：credit_code(40) + tax_no(30) + name(30)——强标识占主导。
///
/// ## 公式
///
/// `score = Σ(field_score × weight) / Σweight`（0-100 归一化，中间量 u32 防溢出）。
///
/// ## 空值处理（关键）
///
/// - **两侧均空的字段跳过**：空值不是「相同」的证据。两条记录都没填 tax_no，
///   不能因此加分。跳过后该字段的 weight 不计入分母（避免空字段稀释得分）。
/// - 跳过后若 Σweight == 0（所有可比较字段都空）→ 返回 0（NoMatch，显式不除零）。
///
/// ## 字段得分
///
/// - [`FieldKind::Exact`]：相等=100，不等=0（强标识字段用，非黑即白）。
/// - [`FieldKind::EditDistance`]：`100 × (1 - 编辑距离/max_len)`，
///   差 1 字扣相应比例；max_len=0 显式返回 100（防除零）。
pub fn compare(target: &MatchRecord, other: &MatchRecord, specs: &[MatchFieldSpec]) -> u8 {
    let mut acc: u32 = 0;
    let mut total_w: u32 = 0;
    for s in specs {
        let a = field_str(&target.fields, &s.field);
        let b = field_str(&other.fields, &s.field);
        let a_empty = a.as_deref().map(|s| s.is_empty()).unwrap_or(true);
        let b_empty = b.as_deref().map(|s| s.is_empty()).unwrap_or(true);
        // 两侧均空 → 跳过（不参与评分）
        if a_empty && b_empty {
            continue;
        }
        let field_score: u32 = match s.kind {
            FieldKind::Exact => match (a.as_deref(), b.as_deref()) {
                (Some(x), Some(y)) if !x.is_empty() && x == y => 100,
                _ => 0,
            },
            FieldKind::EditDistance => match (a.as_deref(), b.as_deref()) {
                (Some(x), Some(y)) => {
                    let max_len = x.chars().count().max(y.chars().count()) as u32;
                    if max_len == 0 {
                        100
                    } else {
                        let dist = levenshtein(x, y).min(max_len);
                        (100 * (max_len - dist) / max_len).min(100)
                    }
                }
                _ => 0,
            },
        };
        acc += field_score * s.weight;
        total_w += s.weight;
    }
    if total_w == 0 {
        return 0;
    }
    (acc / total_w) as u8
}

/// 双阈值裁决：把相似度分数映射成合并决策。
///
/// - **≥95 → [`Decision::AutoMerge`]**：几乎确定同一主体
///   （如 credit_code + tax_no 全等 + name 差一字）。可自动合并。
/// - **80–94 → [`Decision::Review`]**：疑似重复，证据不够强（如仅 name 相似）。
///   进管家工作台人工评审，确认后才合并。
/// - **<80 → [`Decision::NoMatch`]**：字段差异大，不视为重复。新记录正常入库。
///
/// 阈值 95/80 是经验值，可按字典场景调整（md_match_config.thresholds）。
pub fn decide(score: u8) -> Decision {
    if score >= 95 {
        Decision::AutoMerge
    } else if score >= 80 {
        Decision::Review
    } else {
        Decision::NoMatch
    }
}

/// 查重主流程：target vs all（先分块再块内比较）。排除自身。
///
/// 返回候选（score≥80 的 Review/AutoMerge，NoMatch 不返回）。
pub fn find_candidates(
    target: &MatchRecord,
    all: &[MatchRecord],
    specs: &[MatchFieldSpec],
    cluster_keys: &[&str],
) -> Vec<MatchCandidate> {
    // target 自身所在块：把 target 也放进分块输入，保证同块记录可比
    let mut input: Vec<MatchRecord> = Vec::with_capacity(all.len() + 1);
    input.push(target.clone());
    input.extend_from_slice(all);
    let blocks = blocking(&input, cluster_keys);
    let mut out = Vec::new();
    for blk in blocks {
        if !blk.iter().any(|r| r.id == target.id) {
            continue;
        }
        for r in blk {
            if r.id == target.id {
                continue;
            }
            let score = compare(target, r, specs);
            let decision = decide(score);
            if decision != Decision::NoMatch {
                out.push(MatchCandidate { record_id: r.id, score, decision });
            }
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.score));
    out
}

/// 全库扫描聚类：对嫌疑记录集分块 → 块内两两比较 → 聚成重复簇。
///
/// 与 [`find_candidates`] 区别：
/// - [`find_candidates`]：锚点模式（target vs all），返回 target 的候选；
/// - `scan_clusters`：普查模式（suspects 内部互相比较），返回所有重复簇。
///
/// 簇成员判定：块内两两 [`compare`]，记录每条记录的 max score；
/// 仅 max ≥ `min_score` 的成员入簇（同块但字段差异大的记录会被排除）。
/// 簇最少 2 成员才返回。
///
/// # Arguments
///
/// * `suspects` - 嫌疑记录集（通常由 DB 下推分块预过滤后传入，非全表）。
/// * `specs` - 比较字段规则。
/// * `cluster_keys` - 分块簇键（按优先级序）。
/// * `min_score` - 入簇最低分（默认场景传 80 = Review 阈值）。
///
/// # Returns
///
/// 所有重复簇（每簇 ≥2 成员），簇间按最高分降序、簇内按分数降序。
pub fn scan_clusters(
    suspects: &[MatchRecord],
    specs: &[MatchFieldSpec],
    cluster_keys: &[&str],
    min_score: u8,
) -> Vec<DupCluster> {
    use std::collections::HashMap;
    let blocks = blocking(suspects, cluster_keys);
    let mut out = Vec::new();
    for blk in blocks {
        // 块内两两比较，记录每条记录的 max score（仅 ≥min_score 的配对计入）
        let mut member_max: HashMap<i64, u8> = HashMap::new();
        for i in 0..blk.len() {
            for j in (i + 1)..blk.len() {
                let s = compare(blk[i], blk[j], specs);
                if s >= min_score {
                    member_max
                        .entry(blk[i].id)
                        .and_modify(|m| *m = (*m).max(s))
                        .or_insert(s);
                    member_max
                        .entry(blk[j].id)
                        .and_modify(|m| *m = (*m).max(s))
                        .or_insert(s);
                }
            }
        }
        // 至少 2 条命中才成簇
        if member_max.len() >= 2 {
            // cluster_key 取块内第一条记录的第一个非空簇键
            let cluster_key = blk
                .first()
                .and_then(|r| {
                    cluster_keys.iter().find_map(|k| {
                        field_str(&r.fields, k)
                            .filter(|s| !s.is_empty())
                            .map(|v| format!("{}:{}", k, v))
                    })
                })
                .unwrap_or_default();
            let mut members: Vec<MatchClusterMember> = blk
                .iter()
                .filter(|r| member_max.contains_key(&r.id))
                .map(|r| MatchClusterMember {
                    record_id: r.id,
                    score: member_max[&r.id],
                    fields: r.fields.clone(),
                })
                .collect();
            members.sort_by_key(|m| std::cmp::Reverse(m.score));
            out.push(DupCluster { cluster_key, members });
        }
    }
    // 簇间按簇内最高分降序
    out.sort_by_key(|c| std::cmp::Reverse(c.members.first().map(|m| m.score).unwrap_or(0)));
    out
}

/// 取字段字符串值（非字符串类型转字符串表示；Null/缺失 → None）。
fn field_str(fields: &Map<String, Value>, key: &str) -> Option<String> {
    match fields.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => Some(v.to_string()),
    }
}

/// Levenshtein 编辑距离（字符级，两行 DP 空间 O(n)）。
fn levenshtein(a: &str, b: &str) -> u32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len() as u32;
    }
    if b.is_empty() {
        return a.len() as u32;
    }
    let mut prev: Vec<u32> = (0..=b.len() as u32).collect();
    let mut curr = vec![0u32; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i as u32 + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(id: i64, credit: &str, tax: &str, name: &str) -> MatchRecord {
        let mut fields = Map::new();
        fields.insert("credit_code".into(), json!(credit));
        fields.insert("tax_no".into(), json!(tax));
        fields.insert("name".into(), json!(name));
        MatchRecord { id, fields }
    }

    fn specs() -> Vec<MatchFieldSpec> {
        vec![
            MatchFieldSpec { field: "credit_code".into(), weight: 40, kind: FieldKind::Exact },
            MatchFieldSpec { field: "tax_no".into(), weight: 30, kind: FieldKind::Exact },
            MatchFieldSpec { field: "name".into(), weight: 30, kind: FieldKind::EditDistance },
        ]
    }

    #[test]
    fn blocking_same_credit_code_same_block() {
        let rs = vec![rec(1, "C1", "", "甲"), rec(2, "C1", "", "甲乙"), rec(3, "C2", "", "丙")];
        let blocks = blocking(&rs, &["credit_code", "tax_no", "name"]);
        assert!(blocks.iter().any(|b| b.len() == 2));
    }

    #[test]
    fn blocking_null_key_orphan() {
        let rs = vec![rec(1, "", "", "甲"), rec(2, "", "", "甲"), rec(3, "C1", "", "丙")];
        let blocks = blocking(&rs, &["credit_code", "tax_no", "name"]);
        // 空 credit/tax 走 name 簇键 → 甲甲同块
        assert!(blocks.iter().any(|b| b.len() == 2));
    }

    #[test]
    fn compare_identical_100() {
        let a = rec(1, "C1", "T1", "华东钢铁");
        let b = rec(2, "C1", "T1", "华东钢铁");
        assert_eq!(compare(&a, &b, &specs()), 100);
        assert_eq!(decide(100), Decision::AutoMerge);
    }

    #[test]
    fn compare_name_block_one_char_diff_review() {
        // name 簇键块场景（credit/tax 空被跳过）：name 7 字差 1 → 85 → Review
        let a = rec(1, "", "", "华东钢铁集团");
        let b = rec(2, "", "", "华东钢铁集团公");
        let s = compare(&a, &b, &specs());
        assert!((80..=94).contains(&s), "score={s}");
        assert_eq!(decide(s), Decision::Review);
    }

    #[test]
    fn compare_credit_tax_eq_name_near_automerge() {
        // credit+tax 相等 + name 10 字差 1 → 97 → AutoMerge
        let a = rec(1, "C1", "T1", "华东钢铁集团有限公司");
        let b = rec(2, "C1", "T1", "华东钢铁集团有限公");
        let s = compare(&a, &b, &specs());
        assert!(s >= 95, "score={s}");
        assert_eq!(decide(s), Decision::AutoMerge);
    }

    #[test]
    fn compare_zero_weight_no_panic() {
        let a = rec(1, "C1", "", "甲");
        let b = rec(2, "C1", "", "甲");
        let empty: Vec<MatchFieldSpec> = vec![];
        assert_eq!(compare(&a, &b, &empty), 0);
    }

    #[test]
    fn compare_both_empty_name_no_div_zero() {
        // credit 相等、tax/name 双空被跳过 → 100（同信用代码=同主体）
        let a = rec(1, "C1", "", "");
        let b = rec(2, "C1", "", "");
        let s = compare(&a, &b, &specs());
        assert_eq!(s, 100);
    }

    #[test]
    fn scan_clusters_identical_one_cluster() {
        // C1 同块且全等 → 1 簇（2 成员，100 分）；C2 单条不成块
        let rs = vec![
            rec(1, "C1", "T1", "甲"),
            rec(2, "C1", "T1", "甲"),
            rec(3, "C2", "", "乙"),
        ];
        let clusters = scan_clusters(&rs, &specs(), &["credit_code", "tax_no", "name"], 80);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 2);
        assert_eq!(clusters[0].members[0].score, 100);
        assert_eq!(clusters[0].cluster_key, "credit_code:C1");
    }

    #[test]
    fn scan_clusters_name_key_exact_match() {
        // credit/tax 空 → 走 name 簇键；name 完全相同 → 同块 → 100 分簇。
        // 注：blocking 精确等值分块，name 差一字不会同块（模糊簇键 soundex 未实现，见方案缺口§六.1）
        let rs = vec![
            rec(1, "", "", "华东钢铁集团"),
            rec(2, "", "", "华东钢铁集团"),
        ];
        let clusters = scan_clusters(&rs, &specs(), &["credit_code", "tax_no", "name"], 80);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 2);
        assert_eq!(clusters[0].members[0].score, 100);
        assert_eq!(clusters[0].cluster_key, "name:华东钢铁集团");
    }

    #[test]
    fn scan_clusters_below_threshold_empty() {
        // 同 name 簇键但字段差异大 → score<80 → 不成簇
        let rs = vec![rec(1, "", "", "甲公司"), rec(2, "", "", "乙公司")];
        let clusters = scan_clusters(&rs, &specs(), &["credit_code", "tax_no", "name"], 80);
        assert!(clusters.is_empty(), "应有 0 簇，实际 {}", clusters.len());
    }

    #[test]
    fn scan_clusters_multi_blocks() {
        // 两个独立簇：C1 块 + C2 块
        let rs = vec![
            rec(1, "C1", "T1", "甲"),
            rec(2, "C1", "T1", "甲"),
            rec(3, "C2", "T2", "乙"),
            rec(4, "C2", "T2", "乙"),
        ];
        let clusters = scan_clusters(&rs, &specs(), &["credit_code", "tax_no", "name"], 80);
        assert_eq!(clusters.len(), 2);
        // 都应是 100 分簇
        assert!(clusters.iter().all(|c| c.members.len() == 2 && c.members[0].score == 100));
    }

    #[test]
    fn scan_clusters_partial_member_excluded() {
        // 块内 3 条：A-B 全等(100)，C 同块但与 A/B 字段差异大(<80)
        // → member_max 只含 A、B → C 被排除，簇 2 成员
        let rs = vec![
            rec(1, "C1", "T1", "甲公司"),
            rec(2, "C1", "T1", "甲公司"),
            rec(3, "C1", "T1", "丙公司完全不同"),
        ];
        let clusters = scan_clusters(&rs, &specs(), &["credit_code", "tax_no", "name"], 80);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 2);
        // 簇内不应有 id=3
        assert!(!clusters[0].members.iter().any(|m| m.record_id == 3));
    }
}
