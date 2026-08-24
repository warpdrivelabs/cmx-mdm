/*
 * cmx-mdm 独立主数据微服务 HTTP 服务器（对标 cmx-model-server / cmx-rpt-server）。
 *
 * chassis 装配：mdm_routes 路由 + 监控大盘 + 前端联邦 + 数据源钩子 + banner。零 cmx-api 依赖。
 *
 * ★ 单 DB 栈（tokio-pg）：注册两个源——
 *   - MDM_PG_URL      业务库 fico（biz；cm_* md_* mdm_activation cv_mdm_apply，store resolve_db_id 寻址）
 *   - MDM_MAIN_PG_URL 平台主库 cmx（default；分发死信 cmx_portal notify 发门户通知走默认库）
 *
 * 配置：MDM_HOST / MDM_PORT（默认 0.0.0.0:8095）/ MDM_LOG_DIR / MDM_LOG_LEVEL / MDM_CONFIG(toml)。
 * 用法：./mdm.sh  →  curl http://127.0.0.1:8095/api/mdm/health
 */

use axum::Router;
use axum::routing::get;
use cmx_database_pg::{DbConfig, DbType};
use cmx_mdm_app::{dashboard, mdm_routes};
use cmx_web_chassis::{BannerSpec, ChassisConfig, ServiceSpec, run};

/// mdm 专属字符画（MEGA MDM）。
const MDM_ART: &str = r#"
███╗   ███╗███████╗ ██████╗  █████╗     ███╗   ███╗██████╗ ███╗   ███╗
████╗ ████║██╔════╝██╔════╝ ██╔══██╗    ████╗ ████║██╔══██╗████╗ ████║
██╔████╔██║█████╗  ██║  ███╗███████║    ██╔████╔██║██║  ██║██╔████╔██║
██║╚██╔╝██║██╔══╝  ██║   ██║██╔══██║    ██║╚██╔╝██║██║  ██║██║╚██╔╝██║
██║ ╚═╝ ██║███████╗╚██████╔╝██║  ██║    ██║ ╚═╝ ██║██████╔╝██║ ╚═╝ ██║
╚═╝     ╚═╝╚══════╝ ╚═════╝ ╚═╝  ╚═╝    ╚═╝     ╚═╝╚═════╝ ╚═╝     ╚═╝
"#;

#[derive(serde::Deserialize, Default)]
struct MdmFileConfig {
    #[serde(default)]
    datasource: DatasourceSection,
}
#[derive(serde::Deserialize, Default)]
struct DatasourceSection {
    mdm_pg_url: Option<String>,  // → MDM_PG_URL（业务库）
    main_pg_url: Option<String>, // → MDM_MAIN_PG_URL（平台主库）
}

/// 读 mdm-server.toml 的 [datasource] 段，注入 MDM_PG_URL / MDM_MAIN_PG_URL（env 未设时）。
fn apply_toml_env() {
    let path = std::env::var("CONFIG_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("MDM_CONFIG").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "mdm-server.toml".to_string());
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return,
    };
    let file: MdmFileConfig = match toml::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "mdm-server.toml 解析失败，数据源段忽略，回退环境变量");
            return;
        }
    };
    if let Some(v) = file.datasource.mdm_pg_url {
        if !v.trim().is_empty() && std::env::var("MDM_PG_URL").is_err() {
            unsafe { std::env::set_var("MDM_PG_URL", v) }
        }
    }
    if let Some(v) = file.datasource.main_pg_url {
        if !v.trim().is_empty() && std::env::var("MDM_MAIN_PG_URL").is_err() {
            unsafe { std::env::set_var("MDM_MAIN_PG_URL", v) }
        }
    }
}

/// 业务库 db_id（cm_*/md_* 所在；store resolve_db_id 兜底到此 biz 源）。
const BIZ_DB_ID: &str = "fico-db";
/// 平台主库 db_id（默认源；分发死信门户通知）。
const MAIN_DB_ID: &str = "cmx-db";

#[tokio::main]
async fn main() -> cmx_web_chassis::Result<()> {
    dotenvy::dotenv().ok();
    if let Err(e) = cmx_service_base::init_config_manager() {
        tracing::warn!(error = %e, "全局 ConfigManager 初始化失败，回退 env/默认兜底");
    }

    let mut cfg = ChassisConfig::load("mdm", "MDM", "mdm-server.toml");
    apply_toml_env();
    if std::env::var("MDM_PORT").is_err() && cfg.port == 8080 {
        cfg.port = 8095; // 避开 8080/8091/8092/8093/8094。
    }

    let banner = BannerSpec::defaults("mdm")
        .art(MDM_ART)
        .tagline("  MEGA MDM · 主数据中心微服务 · cmx-web-chassis ")
        .stops(vec![(40, 208, 154), (45, 160, 220), (59, 130, 255)]);

    let api_router = mdm_routes::<()>()
        .route("/mdm/stats", get(dashboard::mdm_stats))
        // F2：主数据自持前端页只读投递（native），字节对齐门户信封，供门户 F3 反代。
        .merge(cmx_mdm_app::native_pages::frontend_pages_routes::<()>())
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
        // 钩子：注册两个 tokio-pg 源（业务库 biz + 平台主库 default）。
        .init("datasources", |_meta| {
            Box::pin(async {
                let biz_url = std::env::var("MDM_PG_URL").unwrap_or_else(|_| {
                    "postgres://postgres:postgres@127.0.0.1:5432/fico".to_string()
                });
                let main_url = std::env::var("MDM_MAIN_PG_URL").unwrap_or_else(|_| {
                    "postgres://postgres:postgres@127.0.0.1:5432/cmx".to_string()
                });
                let configs = vec![
                    mdm_db_config(MAIN_DB_ID, &main_url, /*default*/ true, /*biz*/ false),
                    mdm_db_config(BIZ_DB_ID, &biz_url, false, /*biz*/ true),
                ];
                cmx_service_base::register_pg_datasources(&configs)
                    .await
                    .map_err(|e| anyhow::anyhow!("注册数据源失败: {e}"))?;
                tracing::info!(
                    main_db = MAIN_DB_ID, biz_db = BIZ_DB_ID,
                    "✅ 主数据 tokio-pg 数据源已注册（默认=主库 + biz=业务库）"
                );
                Ok(())
            })
        });

    run(spec).await
}

fn mdm_db_config(db_id: &str, url: &str, default: bool, biz: bool) -> DbConfig {
    DbConfig {
        db_type: DbType::Postgres,
        db_url: url.to_string(),
        db_id: db_id.to_string(),
        db_name: None,
        db_schema: Some("public".to_string()),
        default,
        pool_config: Default::default(),
        health_check_interval: 60,
        health_check_timeout: 5,
        domain_code: None,
        application_code: None,
        module_code: None,
        source_type: Some(if biz { "biz".to_string() } else { "default".to_string() }),
    }
}
