//! 治理表分页查询（md_audit / md_event_log / md_subscription）。
//!
//! 与 [`crate::md_accessor`] 的写入函数对偶：`write_audit` → [`list_audit`]，
//! `write_event` → [`list_events`]，`upsert_subscription` → [`list_subscriptions`]。

use cmx_core::dv;
use cmx_core::model::cell::{DataValue, SqlTypeMarker};
use cmx_database_pg::DatabaseManager;
use serde_json::Value;

use crate::error::api_err_db;

/// 变更历史 / 版本留痕（md_audit，分页）。可选 dictCode / recordId 过滤。
pub async fn list_audit(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: Option<&str>,
    record_id: Option<i64>,
    page: i64,
    page_size: i64,
) -> Result<(Vec<Value>, i64), cmx_api_types::Error> {
    let mut clauses = vec!["1=1".to_string()];
    let mut params: Vec<DataValue> = Vec::new();
    if let Some(d) = dict_code {
        clauses.push(format!("dict_code = ${}", params.len() + 1));
        params.push(DataValue::String(d.into()));
    }
    if let Some(r) = record_id {
        clauses.push(format!("record_id = ${}", params.len() + 1));
        params.push(DataValue::Int(r));
    }
    let where_sql = clauses.join(" AND ");
    let cnt_sql = format!("SELECT COUNT(*) AS c FROM md_audit WHERE {where_sql}");
    let cds = mm
        .query_sql_with_datavalues(db_id, None, &cnt_sql, params.clone(), "mdm_audit_count")
        .await
        .map_err(|e| api_err_db(&format!("查 md_audit 总数失败: {e}")))?;
    let total = cds
        .rows
        .first()
        .and_then(|r| r.get_by_name_as::<i64>(cds.schema.as_ref(), "c"))
        .unwrap_or(0);
    let ps = if page_size > 0 { page_size } else { 20 };
    let pg = if page > 0 { page } else { 1 };
    let off = (pg - 1) * ps;
    let n = params.len() as i64;
    params.push(DataValue::Int(ps));
    params.push(DataValue::Int(off));
    let sql = format!(
        "SELECT id, dict_code, record_id, version, action, source_cr_id, field, old_value, new_value, operated_by, operated_at \
         FROM md_audit WHERE {where_sql} ORDER BY operated_at DESC, id DESC \
         LIMIT ${} OFFSET ${}",
        n + 1,
        n + 2
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_audit_list")
        .await
        .map_err(|e| api_err_db(&format!("列表 md_audit 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    Ok((
        ds.rows.iter().map(|r| r.to_json_value(schema)).collect(),
        total,
    ))
}

/// 事件查询（md_event_log，delta 拉取，分页）。可选 dictCode / since(seq) / order。
///
/// `order`：`Some("desc")` 按 seq 倒序（监控页最新在前）；缺省/其他值按 seq 正序
/// （delta 消费端按序拉取的既有契约，保持不变）。
pub async fn list_events(
    mm: &DatabaseManager,
    db_id: &str,
    dict_code: Option<&str>,
    since: Option<i64>,
    order: Option<&str>,
    page: i64,
    page_size: i64,
) -> Result<(Vec<Value>, i64), cmx_api_types::Error> {
    let mut clauses = vec!["1=1".to_string()];
    let mut params: Vec<DataValue> = Vec::new();
    if let Some(d) = dict_code {
        clauses.push(format!("dict_code = ${}", params.len() + 1));
        params.push(DataValue::String(d.into()));
    }
    if let Some(s) = since {
        clauses.push(format!("seq > ${}", params.len() + 1));
        params.push(DataValue::Int(s));
    }
    let where_sql = clauses.join(" AND ");
    let cnt_sql = format!("SELECT COUNT(*) AS c FROM md_event_log WHERE {where_sql}");
    let cds = mm
        .query_sql_with_datavalues(db_id, None, &cnt_sql, params.clone(), "mdm_event_count")
        .await
        .map_err(|e| api_err_db(&format!("查 md_event_log 总数失败: {e}")))?;
    let total = cds
        .rows
        .first()
        .and_then(|r| r.get_by_name_as::<i64>(cds.schema.as_ref(), "c"))
        .unwrap_or(0);
    let ps = if page_size > 0 { page_size } else { 20 };
    let pg = if page > 0 { page } else { 1 };
    let off = (pg - 1) * ps;
    let n = params.len() as i64;
    params.push(DataValue::Int(ps));
    params.push(DataValue::Int(off));
    let order_sql = if matches!(order, Some(o) if o.eq_ignore_ascii_case("desc")) {
        "seq DESC"
    } else {
        "seq ASC"
    };
    let sql = format!(
        "SELECT id, seq, dict_code, record_id, event_type, payload, emitted_at \
         FROM md_event_log WHERE {where_sql} ORDER BY {order_sql} \
         LIMIT ${} OFFSET ${}",
        n + 1,
        n + 2
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_event_list")
        .await
        .map_err(|e| api_err_db(&format!("列表 md_event_log 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    Ok((
        ds.rows.iter().map(|r| r.to_json_value(schema)).collect(),
        total,
    ))
}

/// 订阅配置列表（md_subscription，分页 + 可选过滤 + 近 24h 投递统计列）。
///
/// 过滤参数（`q`）：`targetSys` / `dictCode` / `channel` / `active`，全部可缺省。
/// 统计列：近 24h 投递总数 / 成功数（监控列表直显成功率）与当前积压数。
/// JSONB 列已 parse 还原为对象/数组（secret 掩码由 handler 层处理）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `q` - 过滤与分页参数（`{targetSys?, dictCode?, channel?, active?, page, pageSize}`）。
///
/// # Returns
///
/// `(list, total)`，按 created_at DESC。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn list_subscriptions(
    mm: &DatabaseManager,
    db_id: &str,
    q: &Value,
) -> Result<(Vec<Value>, i64), cmx_api_types::Error> {
    let mut clauses = vec!["1=1".to_string()];
    let mut params: Vec<DataValue> = Vec::new();
    if let Some(s) = q["targetSys"].as_str() {
        if !s.is_empty() {
            clauses.push(format!("target_sys = ${}", params.len() + 1));
            params.push(DataValue::String(s.into()));
        }
    }
    if let Some(s) = q["dictCode"].as_str() {
        if !s.is_empty() {
            clauses.push(format!("dict_code = ${}", params.len() + 1));
            params.push(DataValue::String(s.into()));
        }
    }
    if let Some(s) = q["channel"].as_str() {
        if !s.is_empty() {
            clauses.push(format!("channel = ${}", params.len() + 1));
            params.push(DataValue::String(s.into()));
        }
    }
    if let Some(b) = q["active"].as_bool() {
        clauses.push(format!("active = ${}", params.len() + 1));
        params.push(DataValue::Bool(b));
    }
    let where_sql = clauses.join(" AND ");
    let cnt_sql = format!("SELECT COUNT(*) AS c FROM md_subscription WHERE {where_sql}");
    let cds = mm
        .query_sql_with_datavalues(db_id, None, &cnt_sql, params.clone(), "mdm_sub_count")
        .await
        .map_err(|e| api_err_db(&format!("查 md_subscription 总数失败: {e}")))?;
    let total = cds
        .rows
        .first()
        .and_then(|r| r.get_by_name_as::<i64>(cds.schema.as_ref(), "c"))
        .unwrap_or(0);
    let ps = q["pageSize"].as_i64().filter(|v| *v > 0).unwrap_or(20);
    let pg = q["page"].as_i64().filter(|v| *v > 0).unwrap_or(1);
    let n = params.len() as i64;
    params.push(DataValue::Int(ps));
    params.push(DataValue::Int((pg - 1) * ps));
    let sql = format!(
        "SELECT s.id, s.target_sys, s.dict_code, s.filter, s.field_map, s.channel, s.active, \
                s.name, s.description, s.channel_config, s.event_types, s.retry_max, s.timeout_ms, \
                s.batch_size, s.created_by, s.created_at, s.updated_at, \
                COALESCE(st.total_24h, 0) AS stat_total_24h, \
                COALESCE(st.ok_24h, 0) AS stat_ok_24h, \
                COALESCE(bk.backlog, 0) AS stat_backlog \
         FROM md_subscription s \
         LEFT JOIN (SELECT subscription_id, COUNT(*) AS total_24h, \
                      COUNT(*) FILTER (WHERE status = 'delivered') AS ok_24h \
                    FROM md_dispatch_log WHERE updated_at > now() - interval '24 hours' \
                    GROUP BY subscription_id) st ON st.subscription_id = s.id \
         LEFT JOIN (SELECT subscription_id, COUNT(*) AS backlog FROM md_dispatch_log \
                    WHERE status IN ('pending','running','failed') GROUP BY subscription_id) bk \
                  ON bk.subscription_id = s.id \
         WHERE {where_sql} ORDER BY s.created_at DESC, s.id DESC LIMIT ${} OFFSET ${}",
        n + 1, n + 2
    );
    let ds = mm
        .query_sql_with_datavalues(db_id, None, &sql, params, "mdm_sub_list")
        .await
        .map_err(|e| api_err_db(&format!("列表 md_subscription 失败: {e}")))?;
    let schema = ds.schema.as_ref();
    let mut list: Vec<Value> = ds.rows.iter().map(|r| r.to_json_value(schema)).collect();
    for sub in list.iter_mut() {
        crate::error::parse_jsonb_fields(sub, &["filter", "field_map", "channel_config", "event_types"]);
    }
    Ok((list, total))
}

/// 订阅配置 upsert（按 id；id 缺省生成）。M5 扩展：name/description/channel_config/
/// event_types/retry_max/timeout_ms/batch_size/created_by/updated_at。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `body` - 订阅字段（channel_config 结构合法性由通道 validate_config 前置校验）。
///
/// # Returns
///
/// 订阅 id（新建为应用层生成值）。
///
/// # Errors
///
/// SQL 失败（含 uk_md_subscription 唯一冲突）时返回数据库错误。
pub async fn upsert_subscription(
    mm: &DatabaseManager,
    db_id: &str,
    body: &Value,
) -> Result<i64, cmx_api_types::Error> {
    let id = body
        .get("id")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(cmx_utils::next_pk_id);
    let target = body.get("target_sys").and_then(|v| v.as_str()).unwrap_or("");
    let dict = body.get("dict_code").and_then(|v| v.as_str()).unwrap_or("");
    let channel = body.get("channel").and_then(|v| v.as_str()).unwrap_or("webhook");
    let active = body.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
    let filter = body.get("filter").map(|v| v.to_string());
    let field_map = body.get("field_map").map(|v| v.to_string());
    let name = body.get("name").and_then(|v| v.as_str());
    let description = body.get("description").and_then(|v| v.as_str());
    let channel_config = body.get("channel_config").map(|v| v.to_string());
    let event_types = body.get("event_types").map(|v| v.to_string());
    let retry_max = body.get("retry_max").and_then(|v| v.as_i64()).unwrap_or(8);
    let timeout_ms = body.get("timeout_ms").and_then(|v| v.as_i64()).unwrap_or(10000);
    let batch_size = body.get("batch_size").and_then(|v| v.as_i64()).unwrap_or(50);
    let created_by = body.get("created_by").and_then(|v| v.as_str());
    let sql = r#"INSERT INTO md_subscription (id, target_sys, dict_code, filter, field_map, channel, active,
        name, description, channel_config, event_types, retry_max, timeout_ms, batch_size, created_by, created_at, updated_at)
      VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,now(),now())
      ON CONFLICT (id) DO UPDATE SET target_sys=EXCLUDED.target_sys, dict_code=EXCLUDED.dict_code,
        filter=EXCLUDED.filter, field_map=EXCLUDED.field_map, channel=EXCLUDED.channel, active=EXCLUDED.active,
        name=EXCLUDED.name, description=EXCLUDED.description, channel_config=EXCLUDED.channel_config,
        event_types=EXCLUDED.event_types, retry_max=EXCLUDED.retry_max, timeout_ms=EXCLUDED.timeout_ms,
        batch_size=EXCLUDED.batch_size, updated_at=now()"#;
    mm.execute_sql_with_datavalues(
        db_id,
        None,
        sql,
        dv![
            DataValue::Int(id),
            DataValue::String(target.into()),
            DataValue::String(dict.into()),
            filter
                .map(DataValue::Json)
                .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Json)),
            field_map
                .map(DataValue::Json)
                .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Json)),
            DataValue::String(channel.into()),
            DataValue::Bool(active),
            name.map(|x| DataValue::String(x.into()))
                .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Text)),
            description
                .map(|x| DataValue::String(x.into()))
                .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Text)),
            channel_config
                .map(DataValue::Json)
                .unwrap_or(DataValue::Json("{}".into())),
            event_types
                .map(DataValue::Json)
                .unwrap_or(DataValue::Json("[]".into())),
            DataValue::Int(retry_max),
            DataValue::Int(timeout_ms),
            DataValue::Int(batch_size),
            created_by
                .map(|x| DataValue::String(x.into()))
                .unwrap_or(DataValue::NullTyped(SqlTypeMarker::Text)),
        ],
    )
    .await
    .inspect_err(|e| {
        tracing::error!(target: "cmx_mdm::store", error = %e, "写 md_subscription 失败（原始错误）");
    })
    .map_err(|e| api_err_db(&format!("写 md_subscription 失败: {e}")))?;
    Ok(id)
}

/// 删除订阅（仅停用态可删——handler 侧校验；dispatch_log 保留审计）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `id` - 订阅 id。
///
/// # Returns
///
/// 删除行数（0 = 不存在）。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn delete_subscription(
    mm: &DatabaseManager,
    db_id: &str,
    id: i64,
) -> Result<u64, cmx_api_types::Error> {
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            None,
            "DELETE FROM md_subscription WHERE id = $1",
            dv![DataValue::Int(id)]
        )
        .await
        .map_err(|e| api_err_db(&format!("删 md_subscription 失败: {e}")))?;
    Ok(n.max(0) as u64)
}

/// 订阅启停（active 翻转）。
///
/// 当前 dispatcher 扇出每 tick 直查 DB，停用即刻生效（强一致，无缓存延迟）。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `id` - 订阅 id。
/// * `active` - 目标状态。
///
/// # Returns
///
/// 更新行数（0 = 不存在）。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn set_subscription_active(
    mm: &DatabaseManager,
    db_id: &str,
    id: i64,
    active: bool,
) -> Result<u64, cmx_api_types::Error> {
    let n = mm
        .execute_sql_with_datavalues(
            db_id,
            None,
            "UPDATE md_subscription SET active = $1, updated_at = now() WHERE id = $2",
            dv![DataValue::Bool(active), DataValue::Int(id)]
        )
        .await
        .map_err(|e| api_err_db(&format!("启停 md_subscription 失败: {e}")))?;
    Ok(n.max(0) as u64)
}

/// 取单条订阅（删除前校验 active 态 / 测试通道读配置用）。
///
/// JSONB 列（filter/field_map/channel_config/event_types）已 parse 还原为对象/数组。
///
/// # Arguments
///
/// * `mm` - 数据库管理器。
/// * `db_id` - 数据源 id。
/// * `id` - 订阅 id。
///
/// # Returns
///
/// `Some(row)` 订阅全列；不存在返回 `None`。
///
/// # Errors
///
/// SQL 失败时返回数据库错误。
pub async fn get_subscription(
    mm: &DatabaseManager,
    db_id: &str,
    id: i64,
) -> Result<Option<Value>, cmx_api_types::Error> {
    let ds = mm
        .query_sql_with_datavalues(
            db_id,
            None,
            "SELECT * FROM md_subscription WHERE id = $1",
            dv![DataValue::Int(id)],
            "mdm_sub_get",
        )
        .await
        .map_err(|e| api_err_db(&format!("查 md_subscription 失败: {e}")))?;
    Ok(ds
        .rows
        .first()
        .map(|r| {
            let mut v = r.to_json_value(ds.schema.as_ref());
            crate::error::parse_jsonb_fields(&mut v, &["filter", "field_map", "channel_config", "event_types"]);
            v
        }))
}
