//! M7 流程平台客户端 —— 经 `cmx-flow-sdk` 契约 SDK + 服务目录直连 flow。
//!
//! **定位**：`[service_rpc.services].flow`（url 直连或 Nacos 服务发现）——不再回环绕行
//! 门户（旧形态 loopback→8080→反代→8091 两跳），独立部署的 flow-server 直达；
//! 鉴权（`X-API-Key` + `X-Delegated-User-Token` + `X-Request-Id`）、超时
//! （键级 `timeout_ms` ?? 全局 30s）、幂等重试、熔断全部由 `cmx-service-rpc` 基座承担。
//!
//! **成功判据（全方法统一）**：HTTP 2xx **且** 信封 `code == 0`（SDK 内判定）。flow 的
//! 业务错误（定义未发布、状态不符等）返回 HTTP 200 + `{code:1,msg}`——只看 HTTP 状态码
//! 会把失败误判为成功，产生 approving 孤儿单。
//!
//! **路径**：v1（`/api/flow/v1/*`，与 flow 的 openapi 同源，路径常量在 SDK）。
//!
//! 本模块是**薄适配层**：保持既有函数签名（`Value` 进出），内部转 SDK 的类型化契约，
//! 上游 handler 零改动。配置段（toml）：
//! ```toml
//! [mdm.flow]
//! definition_key = "mdm_cr_approval"        # 流程定义 key
//! webhook_secret = "..."                    # = flow-server FLOW_WEBHOOK_SIGNING_KEY
//! manual_override_enabled = false           # M2 手动端点（approve/reject/activate）开关
//!
//! [service_rpc.services.flow]               # 定位与超时（基座目录）
//! url = "http://127.0.0.1:8091"
//! timeout_ms = 10000                        # 保持 mdm 原回环 10s 语义（缺省全局 30s）
//! ```

use serde_json::Value;

use cmx_flow_sdk::{
    self, BizLink, CancelReq, CompleteTaskReq, InstanceView, RejectTaskReq, StartInstanceReq,
};
use cmx_service_rpc::ServiceRpcError;
use cmx_utils::ConfigManager;

/// MDM 单据绑定的流程表名（biz_link 坐标）。
pub const MDM_BIZ_TABLE: &str = "cv_mdm_apply";

/// `[mdm.flow]` 配置快照（进程内缓存，配置热更不敏感——M7 参数均为部署期定值）。
///
/// 定位（旧 `loopback_base`）与超时（旧 `timeout_ms`）已迁至 `[service_rpc.services.flow]`。
#[derive(Debug, Clone)]
pub struct FlowCfg {
    /// 流程定义 key。
    pub definition_key: String,
    /// webhook 验签密钥。
    pub webhook_secret: String,
    /// M2 手动端点开关。
    pub manual_override_enabled: bool,
}

impl Default for FlowCfg {
    fn default() -> Self {
        Self {
            definition_key: "mdm_cr_approval".to_string(),
            webhook_secret: String::new(),
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
    if let Ok(v) = cm.get_string("mdm.flow.definition_key")
        && !v.trim().is_empty()
    {
        cfg.definition_key = v.trim().to_string();
    }
    if let Ok(v) = cm.get_string("mdm.flow.webhook_secret") {
        cfg.webhook_secret = v;
    }
    if let Ok(v) = cm.get_string("mdm.flow.manual_override_enabled") {
        cfg.manual_override_enabled =
            v.trim().eq_ignore_ascii_case("true") || v.trim() == "1";
    }
    cfg
}

/// 取 flow 客户端（目录键 "flow"；未初始化 / 未配键返回错误字符串）。
fn flow() -> Result<std::sync::Arc<dyn cmx_flow_sdk::FlowClient>, String> {
    cmx_flow_sdk::client().map_err(|e| e.to_string())
}

/// 基座错误 → 字符串（保持旧错误文案风格：含 HTTP 状态 / code / msg）。
fn err_str(e: ServiceRpcError) -> String {
    e.to_string()
}

/// 实例视图 → `Value`（消费方按 JSON 导航 tasks/state 等字段）。
fn view_to_value(v: InstanceView) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// 发起 MDM CR 审批实例：`POST /instances`（bizLink 绑单据坐标）。
///
/// `initiator_name` 为发起人姓名快照（随实例 variables 落 `initiatorName`）——列表/待办中心
/// 展示发起人直接取值，无需回查用户表；人员改名/删号后历史实例仍显示发起时点姓名。
/// 传 None 时变量缺省（兼容：展示侧仍可按 initiator id 回查）。
///
/// 返回实例视图（含 `tasks` 数组——apply 节点任务从这里取）。
pub async fn start_instance(
    cr_head: &serde_json::Map<String, Value>,
    cr_id: i64,
    initiator_name: Option<&str>,
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
    let mut variables = serde_json::json!({
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
    });
    if let Some(name) = initiator_name.map(str::trim).filter(|s| !s.is_empty()) {
        variables["initiatorName"] = serde_json::json!(name);
    }
    let req = StartInstanceReq {
        definition_key: cfg.definition_key,
        business_key: Some(s("doc_no")),
        variables: Some(variables),
        biz_link: Some(BizLink {
            biz_table: MDM_BIZ_TABLE.to_string(),
            biz_id: cr_id.to_string(),
            biz_key: None,
            role: "approval".to_string(),
        }),
        ..Default::default()
    };
    Ok(view_to_value(
        flow()?
            .start_instance(req, user_token)
            .await
            .map_err(err_str)?,
    ))
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
    let req = CompleteTaskReq {
        instance_id: instance_id.to_string(),
        operator: Some(initiator.to_string()),
        ..Default::default()
    };
    match flow()?.complete_task(task_id, req, user_token).await {
        Ok(_) => Ok(()),
        Err(e) if is_already_done(&e) => {
            tracing::debug!(task_id, instance = instance_id, "apply 任务已办结（幂等成功）");
            Ok(())
        }
        Err(e) => Err(err_str(e)),
    }
}

/// 单据 → 实例列表（倒序，第一条 = 当前实例）：`GET /biz/cv_mdm_apply/{crId}/instances`。
pub async fn biz_instances(cr_id: i64) -> Result<Vec<Value>, String> {
    let list = flow()?
        .biz_instances(MDM_BIZ_TABLE, &cr_id.to_string())
        .await
        .map_err(err_str)?;
    Ok(list
        .into_iter()
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect())
}

/// 实例详情（state / tasks / openTasks）：`GET /instances/{id}`。
pub async fn instance_detail(instance_id: &str) -> Result<Value, String> {
    Ok(view_to_value(
        flow()?.instance_detail(instance_id).await.map_err(err_str)?,
    ))
}

/// 实例变量（含 lastDecision）：`GET /instances/{id}/variables`。
pub async fn instance_variables(instance_id: &str) -> Result<Value, String> {
    flow()?
        .instance_variables(instance_id)
        .await
        .map_err(err_str)
}

/// 实例审批意见（cmx_flow_task_comment 投影）：`GET /instances/{id}/comments`。
pub async fn instance_comments(instance_id: &str) -> Result<Vec<Value>, String> {
    flow()?
        .instance_comments(instance_id)
        .await
        .map_err(err_str)
}

/// 实例 → 绑定的单据坐标：`GET /instances/{id}/biz`。
pub async fn biz_of_instance(instance_id: &str) -> Result<Vec<Value>, String> {
    flow()?
        .biz_of_instance(instance_id)
        .await
        .map_err(err_str)
}

/// 取消实例（撤回申请 = 终止本轮审批）：`POST /instances/{id}/cancel`。
///
/// 以发起人身份执行（T0b 授权下须 current_user==initiator）。
pub async fn cancel_instance(
    instance_id: &str,
    reason: &str,
    user_token: Option<&str>,
) -> Result<(), String> {
    flow()?
        .cancel_instance(
            instance_id,
            CancelReq {
                reason: Some(reason.to_string()),
            },
            user_token,
        )
        .await
        .map_err(err_str)
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
    let req = CompleteTaskReq {
        instance_id: instance_id.to_string(),
        decision: Some(decision.to_string()),
        comment: comment.map(str::to_string),
        operator: Some(operator.to_string()),
        ..Default::default()
    };
    flow()?
        .complete_task(task_id, req, user_token)
        .await
        .map_err(err_str)
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
    let req = RejectTaskReq {
        instance_id: instance_id.to_string(),
        from_user: Some(from_user.to_string()),
        target_bpmn_id: target_bpmn_id.map(str::to_string),
        reason: reason.map(str::to_string),
        ..Default::default()
    };
    Ok(view_to_value(
        flow()?
            .reject_task(task_id, req, user_token)
            .await
            .map_err(err_str)?,
    ))
}

/// 当前用户可认领的任务列表（`GET /tasks/my?kind=claimable`）——review-context 的
/// canReview 判定数据源之一（任务无 assignee 时看候选池是否含当前用户）。
pub async fn my_claimable_tasks(user: &str) -> Result<Vec<String>, String> {
    flow()?
        .my_claimable_tasks(user)
        .await
        .map_err(err_str)
}

/// 任务已办结（幂等成功）判定：flow 业务错误码 TaskNotActionable / 「不可办」文案。
fn is_already_done(e: &ServiceRpcError) -> bool {
    match e {
        ServiceRpcError::Remote { msg, .. } => {
            msg.contains("TaskNotActionable") || msg.contains("不可办")
        }
        _ => false,
    }
}
