# cmx-mdm —— 主数据治理独立微服务

从 `cmx-container` 抽出的主数据（MDM）微服务 workspace，对标 `cmx-flowengine` / `cmx-report` /
`cmx-rulesengine`（方案承接 standalone-microservice-design）。**构建本仓需 `../cmx-container/`
并排存在**（基础设施 crate 经跨 workspace path 复用）。

## 一芯多壳

| 壳 | crate | 位置 | 职责 |
| --- | --- | --- | --- |
| **中立核** | `cmx-mdm-app` | 本仓 | 全部 axum handler + `mdm_routes::<S>()` + M5 分发引擎 + M7 流程客户端 + 请求级身份中间件 + native 页自持投递。信封直用 `cmx-api-types`，**零 cmx-api** |
| **独立壳** | `cmx-mdm-server` | 本仓 | chassis bin（:8095）：merge `mdm_routes::<()>()` + 数据源钩子 + 分发引擎拉起 |
| **平台壳** | `cmx-mdm-api` | cmx-container | 纯反代：`MdmProxyModule` 把门户 `/api/mdm/*` + `portal.mdm.*` 页取页请求转发到本服务（`[service_rpc.services].mdm` per-key 定位），前端零改 |

域内库：`cmx-mdm-model`（语义中立层）、`cmx-mdm-store-pg`（PG 持久化/服务层）——自 container
物理迁入，除 workspace 归属外零改。

## 启动

```bash
./mdm.sh                # 开发模式（读 .env / mdm-server.toml）
./mdm.sh --release      # 发布模式
curl http://127.0.0.1:8095/api/mdm/health
```

配置（`mdm-server.toml`，`CONFIG_FILE` 指定；全部走**平台统一装配链**——ConfigManager 三源合并：
本地 toml ← Nacos 配置中心 ← env）：

- `[[databases]]`：**标准数据源段**（与门户 `dev.toml` 逐字段同构），经
  `cmx-service-base::BaseConfig::from_config_manager()` 读取、`register_pg_datasources` 注册
  ——db_id / 连接池 / 健康检查全部配置驱动。要求至少一个 `default` 库 + 一个
  `source_type="biz"` 业务库（`md_*` / `cv_mdm_apply`）；handler 的 `db_id` 请求头路由语义不变。
- `[auth]`：`jwt_secret`（**必须 = 平台签发 JWT 的密钥**，验 `X-Delegated-User-Token` 解真实
  办理人）+ `api_keys`（服务间 API Key = 平台 `[service_auth].outgoing_api_key`）+
  `whitelist`（免鉴权路径，内置 `/mdm/health` 与 `/mdm/flow/callback`，可追加，语义对齐门户
  mw_auth「内置 + toml 合并」）。认证中间件在 `cmx-mdm-app/src/auth.rs`（轻量自实现 JWT 解码，
  与 flow/rules 同模式——不复用 `cmx-auth` 以免把 sqlx/Redis/argon2 整套平台认证栈拖进编译图）。
- `[service_auth]`：出站服务身份（回环调门户时携带的 `X-API-Key`）。
- `[mdm.flow]`：CR 送审流程对接（`definition_key` 流程定义键、`webhook_secret` 回调验签密钥、
  `manual_override_enabled` 人工改派开关）；flow 定位走统一调用目录 `[service_rpc.services].flow`
  （`cmx-flow-sdk` 直连起实例，不再回环门户）。
- `[mdm.distribution]`：M5 分发引擎开关与节流参数。
- 死信门户通知：走统一调用目录 `[service_rpc.services].portal`（`POST /api/notifications/publish`，
  出站凭证由 `cmx-service-rpc` 基座注入；原 `[mdm.notify]` 段已删）。

## 与嵌入时代（原 cmx-mdm-api）的行为对齐

- **URL 零改**：对外仍 `/api/mdm/*`（门户反代恒等映射）。
- **信封零改**：`cmx-api-types` 与平台同源。
- **身份**：原 `CmxSvrContext` 提取器 → `cmx-traits::auth::context_scope` task_local；门户代理
  注入 `X-Delegated-User-Token`，本服务验签建 scope（`created_by` / `operated_by` 同源零回归）。
- **库路由**：`db_id` 请求头优先、缺失回退 biz 库（语义字节对齐原 `cmx-api-core::db_id`）。
- **native 页**：`portal.mdm.*` 共 10 页自持于 `web/ui-native/`（rev = xxhash64，字节对齐门户），
  门户按 id 归属反代取页。
- **死信通知**：原进程内调 `cmx-portal` 通知存储 → HTTP 回环门户统一端点（避免把门户业务 crate
  拖进本服务编译图）。

## 前端页面归属

`web/ui-native/`：`portal.mdm.*`（activation-mapper / master-list / cr-todo / cr-form /
master-detail / duplicate-check / steward / health / subscription-manager / dispatch-monitor）。
菜单节点仍由门户种子（`basic.dataplatform.mdm.mdm-menu`）承载，取页经门户反代。
