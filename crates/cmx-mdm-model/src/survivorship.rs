//! 字段级存活（M3，纯逻辑 DB-free）：合并时逐字段从 master/victim 取真值。
//!
//! 策略：MasterFirst（master 非空优先）/ Fullest（非空优先，都非空 master）/
//! Latest（按 update_time 新者）。M4 加管家逐字段人工裁决。

use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::match_algo::MatchRecord;

/// 逐字段存活规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurvivorRule {
    /// master 非空用 master，否则 victim
    MasterFirst,
    /// 非空优先；都非空 master
    Fullest,
    /// 按 update_time 新者取值（fields 需含 update_time）
    Latest,
}

/// 存活留痕（每字段来源）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SurvivorLogEntry {
    pub field: String,
    /// master | victim
    pub from: String,
    pub value: Value,
}

/// 逐字段存活取值。
///
/// - `survive_fields`：参与存活的字段清单（不含 code/id 等键列）
/// - `rules`：逐字段规则；未配置默认 [`SurvivorRule::MasterFirst`]
///
/// 返回 (存活行 row, survivorship_log)。
pub fn survive(
    master: &MatchRecord,
    victim: &MatchRecord,
    survive_fields: &[String],
    rules: &HashMap<String, SurvivorRule>,
) -> (Map<String, Value>, Vec<SurvivorLogEntry>) {
    let mut row = Map::new();
    let mut log = Vec::new();
    for f in survive_fields {
        let rule = rules.get(f).copied().unwrap_or(SurvivorRule::MasterFirst);
        let m = master.fields.get(f).cloned().unwrap_or(Value::Null);
        let v = victim.fields.get(f).cloned().unwrap_or(Value::Null);
        let (from, value) = pick(rule, &m, &v, master, victim);
        row.insert(f.clone(), value.clone());
        log.push(SurvivorLogEntry { field: f.clone(), from: from.to_string(), value });
    }
    (row, log)
}

/// 按规则在 master/victim 值间选择，返回 (来源, 值)。
fn pick(
    rule: SurvivorRule,
    m: &Value,
    v: &Value,
    master: &MatchRecord,
    victim: &MatchRecord,
) -> (&'static str, Value) {
    let m_empty = is_empty(m);
    let v_empty = is_empty(v);
    match rule {
        SurvivorRule::MasterFirst | SurvivorRule::Fullest => {
            if !m_empty {
                ("master", m.clone())
            } else if !v_empty {
                ("victim", v.clone())
            } else {
                ("master", m.clone())
            }
        }
        SurvivorRule::Latest => {
            let mt = master.fields.get("update_time").and_then(|t| t.as_str()).unwrap_or("");
            let vt = victim.fields.get("update_time").and_then(|t| t.as_str()).unwrap_or("");
            if vt > mt {
                ("victim", v.clone())
            } else {
                ("master", m.clone())
            }
        }
    }
}

/// 空值判定：Null / 空串 / 缺失语义。
fn is_empty(v: &Value) -> bool {
    matches!(v, Value::Null) || v.as_str().map(|s| s.is_empty()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(id: i64, name: &str, phone: &str) -> MatchRecord {
        let mut fields = Map::new();
        fields.insert("name".into(), json!(name));
        fields.insert("phone".into(), json!(phone));
        MatchRecord { id, fields }
    }

    #[test]
    fn master_empty_victim_wins() {
        let m = rec(1, "", "111");
        let v = rec(2, "乙", "");
        let (row, log) = survive(
            &m,
            &v,
            &["name".to_string(), "phone".to_string()],
            &HashMap::new(),
        );
        assert_eq!(row.get("name").and_then(|x| x.as_str()), Some("乙"));
        assert_eq!(log.iter().find(|e| e.field == "name").unwrap().from, "victim");
        assert_eq!(row.get("phone").and_then(|x| x.as_str()), Some("111"));
    }

    #[test]
    fn both_nonempty_master_wins() {
        let m = rec(1, "甲", "");
        let v = rec(2, "乙", "");
        let (row, log) = survive(&m, &v, &["name".to_string()], &HashMap::new());
        assert_eq!(row.get("name").and_then(|x| x.as_str()), Some("甲"));
        assert_eq!(log[0].from, "master");
    }
}
