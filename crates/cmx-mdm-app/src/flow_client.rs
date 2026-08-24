//! M7 流程平台客户端 —— MDM 后端回环调本进程 `/api/flow/*`。
//!
//! **为什么回环 HTTP 而非进程内直调引擎**：流程引擎是「一芯双壳」（内嵌 = FlowModule 同进程
//! handlers；反代 = FlowProxyModule 转发独立 flow-server）。回环走本进程统一入口，两种部署
//! 模式对 MDM 完全透明（`[center_client.services].flow` 配置切换零改代码）。
//!
//! **成功判据（全方法统一）**：HTTP 2xx **且** 信封 `code == 0`。flow 的业务错误（定义未发布、
//! 状态不符等）返回 HTTP 200 + `{code:1,msg}`——只看 HTTP 状态码会把失败误判为成功，产生
//! approving 孤儿单。
//!
//! **鉴权**：`X-API-Key`（`[service_auth].outgoing_api_key`，服务身份）+ 可选
//! `X-Delegated-User-Token`（透传当前用户原始 JWT，flow 侧解出真实办理人——代确认 apply
//! 节点、撤回等以用户身份执行的动作用它）。
//!
//! 配置段（门户 toml）：
//! ```toml
//! [mdm.flow]
//! loopback_base = "http://127.0.0.1:8080"   # 本进程基址（集群指 LB/VIP），不含 /api
//! definition_key = "mdm_cr_approval"        # 流程定义 key
//! webhook_secret = "..."                    # = flow-server FLOW_WEBHOOK_SIGNING_KEY
//! timeout_ms = 10000
//! manual_override_enabled = false           # M2 手动端点（approve/reject/activate）开关
//! ```

use std::sync::OnceLock;

use serde_json::{Value, json};

use cmx_utils::ConfigManager;

/// MDM 单据绑定的流程表名（biz_link 坐标）。
pub const MDM_BIZ_TABLE: &str = "cv_mdm_apply";

/// `[mdm.flow]` 配置快照（进程内缓存，配置热更不敏感——M7 参数均为部署期定值）。
#[derive(Debug, Clone)]
pub struct FlowCfg {
    /// 本进程回环基址（不含 `/api`）。
    pub loopback_base: String,
    /// 流程定义 key。
    pub definition_key: String,
    /// webhook 验签密钥。
    pub webhook_secret: String,
    /// 回环调用超时（毫秒）。
    pub timeout_ms: u64,
    /// M2 手动端点开关。
    pub manual_override_enabled: bool,
}

impl Default for FlowCfg {
    fn default() -> Self {
        Self {
            loopback_base: "http://127.0.0.1:8080".to_string(),
            definition_key: "mdm_cr_approval".to_string(),
            webhook_secret: String::new(),
            timeout_ms: 10_000,
            manual_override_enabled: false,
        }
    }
}

/// 读 `[mdm.flow]` 段（缺项回退默认值）。
pub fn flow_cfg() -> FlowCfg {
    let mut cfg = FlowCfg::default();
    let Some(cm) = ConfigManager::try_global() else {
        return cfg;
    };
    if let Ok(v) = cm.get_string("mdm.flow.loopback_base")
        && !v.trim().is_empty()
    {
        cfg.loopback_base = v.trim().trim_end_matches('/').to_string();
    }
    if let Ok(v) = cm.get_string("mdm.flow.definition_key")
        && !v.trim().is_empty()
    {
        cfg.definition_key = v.trim().to_string();
    }
    if let Ok(v) = cm.get_string("mdm.flow.webhook_secret") {
        cfg.webhook_secret = v;
    }
    if let Ok(v) = cm.get_string("mdm.flow.timeout_ms")
        && let Ok(ms) = v.trim().parse::<u64>()
        && ms > 0
    {
        cfg.timeout_ms = ms;
    }
    if let Ok(v) = cm.get_string("mdm.flow.manual_override_enabled") {
        cfg.manual_override_enabled =
            v.trim().eq_ignore_ascii_case("true") || v.trim() == "1";
    }
    cfg
}

/// reqwest 客户端单例（连接池复用；集群无状态合规——纯连接池无业务状态）。
fn client() -> &'static reqwest::Client {
    static CLI: OnceLock<reqwest::Client> = OnceLock::new();
    CLI.get_or_init(reqwest::Client::new)
}

/// 服务间 API Key（`[service_auth].outgoing_api_key`）。未配置返回 None（回环将走匿名，
/// 内嵌模式由 mw_auth 兜底，反代模式会被 flow-server 401）。
fn outgoing_api_key() -> Option<String> {
    let cm = ConfigManager::try_global()?;
    let key = cm.get_string("service_auth.outgoing_api_key").ok()?;
    if key.trim().is_empty() { None } else { Some(key) }
}

/// 统一回环请求：`{loopback_base}/api/flow{path}`，成功返回信封 `data` 部分。
///
/// 失败 = HTTP 非 2xx / 信封 code != 0 / 网络错误 / JSON 解析失败，错误信息带 flow 的 msg。
async fn call_flow(
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
    user_token: Option<&str>,
) -> Result<Value, String> {
    let cfg = flow_cfg();
    let url = format!("{}/api/flow{}", cfg.loopback_base, path);
    let mut rb = client()
        .request(method, &url)
        .timeout(std::time::Duration::from_millis(cfg.timeout_ms));
    if let Some(key) = outgoing_api_key() {
        rb = rb.header("X-API-Key", key);
    }
    // 委托令牌：显式传入优先，缺省取当前请求用户原始 JWT（请求作用域内）。
    let token = user_token
        .map(|s| s.to_string())
        .or_else(cmx_traits::auth::context_scope::current_original_token);
    if let Some(t) = token {
        rb = rb.header("X-Delegated-User-Token", format!("Bearer {t}"));
    }
    let body = body.unwrap_or_else(|| json!({}));
    let resp = rb
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("流程服务不可达: {e}"))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取流程响应失败: {e}"))?;
    let envelop: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("流程响应非 JSON（HTTP {status}）: {e}"))?;
    let code = envelop.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if !status.is_success() || code != 0 {
        let msg = envelop
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误")
            .to_string();
        return Err(format!("流程服务返回失败（HTTP {status}, code {code}）: {msg}"));
    }
    Ok(envelop.get("data").cloned().unwrap_or(Value::Null))
}

/// 发起 MDM CR 审批实例：`POST /instances`（bizLink 绑单据坐标）。
///
/// 返回 data（实例视图，含 `tasks` 数组——apply 节点任务从这里取）。
pub async fn start_instance(
    cr_head: &serde_json::Map<String, Value>,
    cr_id: i64,
    user_token: Option<&str>,
) -> Result<Value, String> {
    let cfg = flow_cfg();
    // 兼容字符串/数字两种序列化形态（create_by 等列由元数据驱动，列型可能演进）。
    let s = |k: &str| -> String {
        match cr_head.get(k) {
            Some(Value::String(v)) => v.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    };
    let body = json!({
        "definitionKey": cfg.definition_key,
        "businessKey": s("doc_no"),
        "variables": {
            // initiator 必须显式带：撤回护栏 / 「我发起的」过滤按 variables.initiator 判定。
            "initiator": s("create_by"),
            "docNo": s("doc_no"),
            "docType": s("doc_type"),
            "targetDictCode": s("target_dict_code"),
            "crType": s("cr_type"),
            "subjectName": s("subject_name"),
            // biz 坐标进变量：待办中心 tasks/my 的 bizTable/bizId 投影取自实例变量
            // （bizLink 表只供反查），不塞则任务打开时业务表单定位不到单据。
            "bizTable": MDM_BIZ_TABLE,
            "bizId": cr_id.to_string(),
        },
        "bizLink": {
            "bizTable": MDM_BIZ_TABLE,
            "bizId": cr_id.to_string(),
            "role": "approval",
        },
    });
    call_flow(reqwest::Method::POST, "/instances", Some(body), user_token).await
}

/// 办结 apply（发起人确认）节点任务：`POST /tasks/{taskId}/complete`。
///
/// 以发起人身份执行（透传其 JWT——T0b 授权下 apply assignee=initiator 合法）。
/// 幂等归类：任务已办结时 flow 返回 code=1（TaskNotActionable）→ 视为成功（AlreadyDone），
/// 防重试误判故障。
pub async fn complete_apply_task(
    task_id: &str,
    instance_id: &str,
    initiator: &str,
    user_token: Option<&str>,
) -> Result<(), String> {
    let body = json!({
        "instanceId": instance_id,
        "operator": initiator,
    });
    match call_flow(
        reqwest::Method::POST,
        &format!("/tasks/{task_id}/complete"),
        Some(body),
        user_token,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.contains("TaskNotActionable") || e.contains("不可办") => {
            tracing::debug!(task_id, instance = instance_id, "apply 任务已办结（幂等成功）");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// 单据 → 实例列表（倒序，第一条 = 当前实例）：`GET /biz/cv_mdm_apply/{crId}/instances`。
pub async fn biz_instances(cr_id: i64) -> Result<Vec<Value>, String> {
    let data = call_flow(
        reqwest::Method::GET,
        &format!("/biz/{MDM_BIZ_TABLE}/{cr_id}/instances"),
        None,
        None,
    )
    .await?;
    Ok(data
        .get("instances")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// 实例详情（state / tasks / openTasks）：`GET /instances/{id}`。
pub async fn instance_detail(instance_id: &str) -> Result<Value, String> {
    call_flow(
        reqwest::Method::GET,
        &format!("/instances/{instance_id}"),
        None,
        None,
    )
    .await
}

/// 实例变量（含 lastDecision）：`GET /instances/{id}/variables`。
pub async fn instance_variables(instance_id: &str) -> Result<Value, String> {
    call_flow(
        reqwest::Method::GET,
        &format!("/instances/{instance_id}/variables"),
        None,
        None,
    )
    .await
}

/// 实例审批意见（cmx_flow_task_comment 投影）：`GET /instances/{id}/comments`。
pub async fn instance_comments(instance_id: &str) -> Result<Vec<Value>, String> {
    let data = call_flow(
        reqwest::Method::GET,
        &format!("/instances/{instance_id}/comments"),
        None,
        None,
    )
    .await?;
    Ok(data
        .get("comments")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// 实例 → 绑定的单据坐标：`GET /instances/{id}/biz`。
pub async fn biz_of_instance(instance_id: &str) -> Result<Vec<Value>, String> {
    let data = call_flow(
        reqwest::Method::GET,
        &format!("/instances/{instance_id}/biz"),
        None,
        None,
    )
    .await?;
    Ok(data
        .get("links")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// 取消实例（撤回申请 = 终止本轮审批）：`POST /instances/{id}/cancel`。
///
/// 以发起人身份执行（T0b 授权下须 current_user==initiator）。
pub async fn cancel_instance(
    instance_id: &str,
    reason: &str,
    user_token: Option<&str>,
) -> Result<(), String> {
    let body = json!({ "reason": reason });
    call_flow(
        reqwest::Method::POST,
        &format!("/instances/{instance_id}/cancel"),
        Some(body),
        user_token,
    )
    .await
    .map(|_| ())
}

/// 办结 review 审批任务（M7.1 业务封装入口）：`POST /tasks/{taskId}/complete`。
///
/// 以**当前审批人**身份执行（透传其 JWT——T0b 授权要求 current_user∈assignee∪候选）；
/// operator 显式传入作留痕（与 T0 兜底同值）。
pub async fn complete_review_task(
    task_id: &str,
    instance_id: &str,
    decision: &str,
    comment: Option<&str>,
    operator: &str,
    user_token: Option<&str>,
) -> Result<(), String> {
    let body = json!({
        "instanceId": instance_id,
        "decision": decision,
        "comment": comment,
        "operator": operator,
    });
    call_flow(
        reqwest::Method::POST,
        &format!("/tasks/{task_id}/complete"),
        Some(body),
        user_token,
    )
    .await
    .map(|_| ())
}

/// 退回（打回上节点重办，实例仍 ACTIVE）：`POST /tasks/{taskId}/reject`。
///
/// 以当前用户身份执行（fromUser 留痕；引擎侧无 T0b 校验，业务层负责权限前置）。
pub async fn return_review_task(
    task_id: &str,
    instance_id: &str,
    from_user: &str,
    target_bpmn_id: Option<&str>,
    reason: Option<&str>,
    user_token: Option<&str>,
) -> Result<Value, String> {
    let body = json!({
        "instanceId": instance_id,
        "fromUser": from_user,
        "targetBpmnId": target_bpmn_id,
        "reason": reason,
    });
    call_flow(
        reqwest::Method::POST,
        &format!("/tasks/{task_id}/reject"),
        Some(body),
        user_token,
    )
    .await
}

/// 当前用户可认领的任务列表（`GET /tasks/my?kind=claimable`）——review-context 的
/// canReview 判定数据源之一（任务无 assignee 时看候选池是否含当前用户）。
pub async fn my_claimable_tasks(user: &str) -> Result<Vec<String>, String> {
    let data = call_flow(
        reqwest::Method::GET,
        &format!("/tasks/my?kind=claimable&assignee={}", urlencode(user)),
        None,
        None,
    )
    .await?;
    let arr = data
        .get("tasks")
        .or_else(|| data.get("items"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|t| t.get("taskId").and_then(|v| v.as_str()).map(String::from))
        .collect())
}

/// URL query 编码（user id 拼进 query 用）。
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
