//! 订阅匹配 + 字段投影 + 信封组装（纯函数，DB-free，可单测）。
//!
//! 语义（方案 §5.4 / §6.1）：
//! - 匹配三要素：dict_code 相等 + event_types 包含（空数组=全部）+ filter 对快照求值；
//! - filter v1 操作符 `eq / ne / in / like`，logic 仅 `and`；字段缺失按不命中计；
//! - field_map 三键：`include`（裁剪）/ `rename`（重命名）/ `mask`（脱敏 `***`）；
//! - 薄事件（无 snapshot 的存量事件）跳过 filter 求值（视为命中）。

use cmx_mdm_model::distribution::EventEnvelope;
use serde_json::{Map, Value};

/// 事件×订阅匹配（扇出闭包；纯函数）。
///
/// 匹配三要素：dict_code 相等 + event_types 包含（空数组=全部）+ filter 对快照求值。
///
/// # Arguments
///
/// * `event` - md_event_log 行（payload 需已 parse 为对象；含 snapshot 的 fat event）。
/// * `sub` - md_subscription 行（event_types/filter 需已 parse）。
///
/// # Returns
///
/// 命中返回 `true`（扇出将生成投递实例）；存量薄事件（无 snapshot）跳过 filter 求值视为命中。
pub fn event_matches_sub(event: &Value, sub: &Value) -> bool {
    let ev_dict = event.get("dict_code").and_then(|v| v.as_str()).unwrap_or("");
    let sub_dict = sub.get("dict_code").and_then(|v| v.as_str()).unwrap_or("");
    if ev_dict.is_empty() || ev_dict != sub_dict {
        return false;
    }
    // 事件类型过滤：空数组 = 全部
    let ev_type = event.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(types) = sub.get("event_types").and_then(|v| v.as_array()) {
        let hit = types
            .iter()
            .any(|t| t.as_str() == Some(ev_type) || t.as_str() == Some("*"));
        if !types.is_empty() && !hit {
            return false;
        }
    }
    // 行级过滤：对事件快照求值；快照缺失（存量薄事件）跳过滤（视为命中，方案 §5.5）
    let snapshot = event
        .get("payload")
        .and_then(|p| p.get("snapshot"))
        .filter(|s| s.is_object());
    if let Some(filter) = sub.get("filter").filter(|f| f.is_object()) {
        if let Some(snap) = snapshot {
            return eval_filter(filter, snap);
        }
        tracing::warn!(
            target: "cmx_mdm::distribution",
            dict = ev_dict, event_type = ev_type,
            "存量薄事件无 snapshot，跳过 filter 求值（视为命中）"
        );
    }
    true
}

/// filter 求值：`{conditions:[{field,op,value}], logic:"and"}`。
fn eval_filter(filter: &Value, snapshot: &Value) -> bool {
    let Some(conds) = filter.get("conditions").and_then(|v| v.as_array()) else {
        return true;
    };
    if conds.is_empty() {
        return true;
    }
    conds.iter().all(|c| eval_condition(c, snapshot))
}

/// 单条件求值：字段缺失 / 类型不匹配按不命中计（保守）。
fn eval_condition(cond: &Value, snapshot: &Value) -> bool {
    let Some(field) = cond.get("field").and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(op) = cond.get("op").and_then(|v| v.as_str()) else {
        return false;
    };
    let want = cond.get("value").cloned().unwrap_or(Value::Null);
    let got = snapshot.get(field).unwrap_or(&Value::Null);
    match op {
        "eq" => json_eq(got, &want),
        "ne" => !json_eq(got, &want),
        "in" => want
            .as_array()
            .map(|a| a.iter().any(|x| json_eq(got, x)))
            .unwrap_or(false),
        "like" => {
            let (Some(g), Some(w)) = (got.as_str(), want.as_str()) else {
                return false;
            };
            let w = w.trim_end_matches('%');
            g.contains(w.trim_start_matches('%')) && (w.starts_with('%') || g.starts_with(w))
        }
        _ => false,
    }
}

/// JSON 值宽松相等（数字统一 f64 比较，字符串精确比较）。
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            x.as_f64() == y.as_f64()
                || x.as_i64().zip(y.as_i64()).map(|(p, q)| p == q).unwrap_or(false)
        }
        _ => a == b,
    }
}

/// field_map 投影：include 裁剪 → rename 重命名 → mask 脱敏（顺序固定）。
///
/// # Arguments
///
/// * `snapshot` - 事件快照（对象；非对象原样返回）。
/// * `field_map` - 订阅级转换规则 `{include:[], rename:{}, mask:[]}`；空/缺省键跳过对应步骤。
///
/// # Returns
///
/// 投影后的新 JSON（不改入参）；mask 字段值替换为 `"***"`。
pub fn apply_field_map(snapshot: &Value, field_map: &Value) -> Value {
    let Some(obj) = snapshot.as_object() else {
        return snapshot.clone();
    };
    let Some(fm) = field_map.as_object() else {
        return snapshot.clone();
    };
    // 1. include（空/缺省 = 全字段）
    let mut out: Map<String, Value> = if let Some(inc) = fm.get("include").and_then(|v| v.as_array())
    {
        let names: Vec<&str> = inc.iter().filter_map(|v| v.as_str()).collect();
        if names.is_empty() {
            obj.clone()
        } else {
            obj.iter()
                .filter(|(k, _)| names.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }
    } else {
        obj.clone()
    };
    // 2. rename（old → new；冲突时新名覆盖）
    if let Some(ren) = fm.get("rename").and_then(|v| v.as_object()) {
        for (old, new) in ren {
            if let (Some(new_key), Some(val)) = (new.as_str(), out.remove(old)) {
                out.insert(new_key.to_string(), val);
            }
        }
    }
    // 3. mask（值替换 "***"，保留字段存在性）
    if let Some(mask) = fm.get("mask").and_then(|v| v.as_array()) {
        for f in mask.iter().filter_map(|v| v.as_str()) {
            if out.contains_key(f) {
                out.insert(f.to_string(), Value::String("***".into()));
            }
        }
    }
    Value::Object(out)
}

/// 组装投递信封（投递时由 dispatcher 调用）。
///
/// # Arguments
///
/// * `event` - md_event_log 行（id/seq/event_type/dict_code/record_id/emitted_at/payload）。
/// * `data` - field_map 投影后的快照（信封 `data` 字段）。
/// * `meta` - 溯源信息（crId/dispatchId 等，信封 `meta` 字段）。
///
/// # Returns
///
/// 通道无关的标准投递信封（record_code/version 取自 payload）。
pub fn build_envelope(event: &Value, data: Value, meta: Value) -> EventEnvelope {
    let payload = event.get("payload").cloned().unwrap_or(Value::Null);
    let snapshot = payload.get("snapshot").cloned().unwrap_or(Value::Null);
    EventEnvelope {
        event_id: event.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        seq: event.get("seq").and_then(|v| v.as_i64()).unwrap_or(0),
        event_type: event
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        dict_code: event
            .get("dict_code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        record_id: event.get("record_id").and_then(|v| v.as_i64()).unwrap_or(0),
        record_code: snapshot
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        version: payload.get("version").and_then(|v| v.as_i64()).unwrap_or(0),
        source: "cmx-mdm",
        occurred_at: event
            .get("emitted_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        data,
        meta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(dict: &str, ev_type: &str, snapshot: Value) -> Value {
        json!({
            "id": "evt-1", "seq": 1, "dict_code": dict, "event_type": ev_type,
            "record_id": 42, "emitted_at": "2026-08-18T08:00:00Z",
            "payload": { "version": 3, "snapshot": snapshot }
        })
    }

    fn sub(dict: &str, types: Value, filter: Value) -> Value {
        json!({ "dict_code": dict, "event_types": types, "filter": filter })
    }

    #[test]
    fn matches_dict_type_and_filter() {
        let ev = event("supplier", "created", json!({"status": "A", "group": "g1", "name": "华东钢铁"}));
        // 字典不匹配
        assert!(!event_matches_sub(&ev, &sub("customer", json!([]), json!({}))));
        // 类型过滤
        assert!(!event_matches_sub(&ev, &sub("supplier", json!(["updated"]), json!({}))));
        assert!(event_matches_sub(&ev, &sub("supplier", json!([]), json!({}))));
        // filter eq / in / like
        assert!(event_matches_sub(&ev, &sub("supplier", json!([]), json!({"conditions":[{"field":"status","op":"eq","value":"A"}]}))));
        assert!(!event_matches_sub(&ev, &sub("supplier", json!([]), json!({"conditions":[{"field":"status","op":"ne","value":"A"}]}))));
        assert!(event_matches_sub(&ev, &sub("supplier", json!([]), json!({"conditions":[{"field":"group","op":"in","value":["g1","g2"]}]}))));
        assert!(event_matches_sub(&ev, &sub("supplier", json!([]), json!({"conditions":[{"field":"name","op":"like","value":"华东%"}]}))));
        assert!(!event_matches_sub(&ev, &sub("supplier", json!([]), json!({"conditions":[{"field":"name","op":"like","value":"钢铁%"}]}))));
        // 字段缺失 → 不命中
        assert!(!event_matches_sub(&ev, &sub("supplier", json!([]), json!({"conditions":[{"field":"missing","op":"eq","value":1}]}))));
        // 多条件 and
        assert!(event_matches_sub(&ev, &sub("supplier", json!([]), json!({"conditions":[{"field":"status","op":"eq","value":"A"},{"field":"group","op":"eq","value":"g1"}]}))));
    }

    #[test]
    fn thin_event_skips_filter() {
        let ev = json!({
            "id": "evt-2", "seq": 2, "dict_code": "supplier", "event_type": "updated",
            "record_id": 1, "payload": { "version": 2 }
        });
        // 无 snapshot + 带 filter 订阅 → 视为命中（跳过求值）
        assert!(event_matches_sub(
            &ev,
            &sub("supplier", json!([]), json!({"conditions":[{"field":"status","op":"eq","value":"A"}]}))
        ));
    }

    #[test]
    fn field_map_project_in_order() {
        let snap = json!({"code": "GYS1", "name": "华东", "tax_no": "91310000", "status": "A"});
        let fm = json!({
            "include": ["code", "name", "tax_no"],
            "rename": { "code": "supplierCode" },
            "mask": ["tax_no"]
        });
        let out = apply_field_map(&snap, &fm);
        assert_eq!(out["supplierCode"], "GYS1");
        assert_eq!(out["tax_no"], "***");
        assert!(out.get("status").is_none());
        // 空 field_map → 原样
        assert_eq!(apply_field_map(&snap, &json!({})), snap);
    }

    #[test]
    fn envelope_carries_version_and_code() {
        let ev = event("supplier", "updated", json!({"code": "GYS1", "name": "华东"}));
        let env = build_envelope(&ev, json!({"code": "GYS1"}), json!({"crId": 9}));
        assert_eq!(env.version, 3);
        assert_eq!(env.record_code, "GYS1");
        assert_eq!(env.seq, 1);
    }
}
