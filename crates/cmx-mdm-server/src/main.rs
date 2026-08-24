/*
 * cmx-mdm 独立主数据微服务 HTTP 服务器（对标 cmx-model-server / cmx-rpt-server）。
 *
 * chassis 装配：mdm_routes 路由 + 监控大盘 + 前端联邦 + 请求级身份中间件 + 数据源钩子 + banner。
 * 零 cmx-api 依赖。
 *
 * 配置全部走**平台统一装配链**（mdm-server.toml，路径由 CONFIG_FILE 指定）：
 *   - 框架级：[server] 段（env 覆盖 SERVER__HOST / SERVER__PORT 默认 0.0.0.0:8095 /
 *     SERVER__LOG_DIR / SERVER__LOG_LEVEL，与 ConfigManager `__` 约定同名）；
 *   - 数据源：标准 [[databases]] 段（与门户 dev.toml 同构）→ cmx-service-base::BaseConfig
 *     （ConfigManager 三源合并读取），注册走共享原语 register_pg_datasources，零硬编码；
 *   - 认证：[auth] 段 → cmx-mdm-app 认证中间件（X-API-Key 校验 + X-Delegated-User-Token 验签）；
 *   - 业务：[mdm.flow] / [mdm.distribution] / [mdm.notify] / [service_auth]。
 *
 * 用法：./mdm.sh  →  curl http://127.0.0.1:8095/api/mdm/health
 */

use axum::Router;
use axum::routing::get;
use cmx_mdm_app::{dashboard, mdm_routes};
use cmx_web_chassis::{BannerSpec, ChassisConfig, ServiceSpec, run};

/// mdm 专属字符画（MEGA MDM）。
const MDM_ART: &str = r#"
███╗   ███╗███████╗ ██████╗  █████╗     ███╗   ███╗██████╗ ███╗   ███╗
████╗ ████║██╔════╝██╔════╝ ██╔══██╗    ████╗ ████║██╔══██╗████╗ ████║
██╔████╔██║█████╗  ██║  ███╗███████╗    ██╔████╔██║██║  ██║██╔████╔██║
██║╚██╔╝██║██╔══╝  ██║   ██║██╔══██║    ██║╚██╔╝██║██║  ██║██║╚██╔╝██║
██║ ╚═╝ ██║███████╗╚██████╔╝██║  ██║    ██║ ╚═╝ ██║██████╔╝██║ ╚═╝ ██║
╚═╝     ╚═╝╚══════╝ ╚═════╝ ╚═╝  ╚═╝    ╚═╝     ╚═╝╚═════╝ ╚═╝     ╚═╝
"#;

#[tokio::main]
async fn main() -> cmx_web_chassis::Result<()> {
    // 统一启动契约（与门户/flow/report 一致）：自动读 cwd 的 .env（CONFIG_FILE 指向本仓 toml）。
    // 必须在 ChassisConfig::load / init_infra（都读 env）之前，故置于 main 首行。
    dotenvy::dotenv().ok();

    // 基础设施装配（与门户 run_platform 同一制度）：本地 toml ← Nacos 远程配置中心 ← env
    // 三源 ConfigManager + 注册中心客户端（自注册 + 实例缓存 + 30s 服务列表同步）。开关默认
    // 全关（未开 NACOS_ENABLED 时走 Mock，纯本地 toml+env，行为与接入前一致）；开启后
    // create 阶段强依赖 Nacos 可达，失败即中止启动（register 阶段失败仅 warn）。
    cmx_service_base::init_infra()
        .await
        .map_err(|e| cmx_web_chassis::ChassisError::Config(format!("基础设施初始化失败: {e}")))?;

    // 框架级配置：[server] 段 + SERVER__* env 覆盖（与 ConfigManager `__` 约定同名）+ mdm-server.toml，默认端口 8095。
    let mut cfg = ChassisConfig::load("mdm", "mdm-server.toml");
    if std::env::var("SERVER__PORT").is_err() && cfg.port == 8080 {
        cfg.port = 8095; // 避开 8080/8091/8092/8093/8094。
    }

    let banner = BannerSpec::defaults("mdm")
        .art(MDM_ART)
        .tagline("  MEGA MDM · 主数据中心微服务 · cmx-web-chassis ")
        .stops(vec![(40, 208, 154), (45, 160, 220), (59, 130, 255)]);

    // 路由（对任意 state 泛型成立，这里 state = ()）：
    //   - /api/mdm/*（治理端点，URL 与迁移前完全一致）+ /api/mdm/stats（大盘数据源）；
    //   - /api/native-pages*（主数据自持前端页只读投递，字节对齐门户信封，供门户 F3 反代）。
    // 中间件：请求级身份（X-API-Key 校验 + X-Delegated-User-Token 验签建 scope，白名单放行探针/
    //   webhook 回调）→ 可观测遥测。没有身份层，ctx::current_user_id 恒为匿名（created_by 落空）。
    let api_router = mdm_routes::<()>()
        .route("/mdm/stats", get(dashboard::mdm_stats))
        // 资产目录遵循规范 v2（relPath 相对 index.json）；信封直用 cmx-api-types。
        .merge(cmx_form::serve::frontend_pages_routes::<(), cmx_api_types::Error>(
            cmx_form::serve::PageServeConfig::from_assets(),
        ))
        .layer(axum::middleware::from_fn(cmx_mdm_app::auth::mw))
        // 可观测中间件：采集每请求 method/path/协议/状态/耗时，喂 /_mon 请求遥测面板。
        .layer(axum::middleware::from_fn(cmx_web_monitor::observe));
    let app_router = Router::new()
        .route("/", get(dashboard::dashboard))
        .nest("/api", api_router);

    cmx_web_monitor::set_service_name("cmx-mdm 主数据中心");
    cmx_web_monitor::set_topology_provider(|| {
        vec![cmx_web_monitor::ServiceDep {
            key: "mdm".into(),
            label: "主数据中心".into(),
            mode: "embedded".into(),
            target: None,
            proxiable: false,
        }]
    });

    let spec = ServiceSpec::<()>::new("mdm", cfg)
        .banner(banner)
        .nest_api(false)
        .router(app_router)
        .state(())
        // 钩子：注册数据源——平台封装：BaseConfig（标准 [[databases]] 段，ConfigManager 三源
        // 合并）+ 共享注册原语 register_pg_datasources。要求至少配一个 default 库与一个
        // source_type="biz" 业务库（handler 的 db_id 头路由 / 回退语义依赖后者）。
        .init("datasources", |_meta| {
            Box::pin(async {
                let base = cmx_service_base::BaseConfig::from_config_manager()
                    .map_err(|e| anyhow::anyhow!("读取 [[databases]] 配置失败: {e}"))?;
                if base.databases.is_empty() {
                    return Err(anyhow::anyhow!(
                        "mdm-server.toml 未配置 [[databases]]（需至少一个 default 库 + 一个 source_type=\"biz\" 业务库）"
                    ));
                }
                let has_biz = base
                    .databases
                    .iter()
                    .any(|d| d.source_type.as_deref() == Some("biz"));
                if !has_biz {
                    return Err(anyhow::anyhow!(
                        "[[databases]] 缺少 source_type=\"biz\" 业务库（db_id 请求头缺失时的回退目标）"
                    ));
                }
                let ids: Vec<&str> = base.databases.iter().map(|d| d.db_id.as_str()).collect();
                cmx_service_base::register_pg_datasources(&base.databases)
                    .await
                    .map_err(|e| anyhow::anyhow!("注册数据源失败: {e}"))?;
                tracing::info!(databases = ?ids, "✅ 主数据 tokio-pg 数据源已注册（[[databases]] 配置驱动）");
                Ok(())
            })
        })
        // 钩子：拉起 M5 分发引擎（通道注册 + Dispatcher 常驻循环；[mdm.distribution].enabled
        // =false 时仅注册通道不 spawn，端点仍可用——与门户内嵌时代同一开关语义）。
        .init("distribution", |_meta| {
            Box::pin(async {
                cmx_mdm_app::distribution::start_distribution()
                    .map_err(|e| anyhow::anyhow!("分发引擎启动失败: {e}"))?;
                Ok(())
            })
        });

    let result = run(spec).await;
    // serve 结束（收到关闭信号或自然退出）：注销注册中心实例后再返回——不用 `?` 提前返回，
    // 否则 Err 路径会跳过注销（实例要等 Nacos 心跳超时才摘除）。
    cmx_service_base::shutdown_infra().await;
    result
}
