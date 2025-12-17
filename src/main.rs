mod api;
mod auth;
mod config;
mod fragments;
mod integration;
mod packs;
mod routing;
mod sdk;
mod server;
mod tenant;
mod worker;

use crate::config::LoadedConfig;
use crate::fragments::{CompositeFragmentRenderer, NoopFragmentInvoker, WasmtimeFragmentInvoker};
use crate::integration::{GreenticTelemetrySink, RealSessionManager};
use crate::packs::FsPackProvider;
use crate::server::AppState;
use crate::worker::{WorkerHost, worker_backend_from_env};
use clap::Parser;
use greentic_config::explain;
use greentic_distributor_client::{
    HttpDistributorClient, config::DistributorClientConfig, types::DistributorEnvironmentId,
};
use greentic_session::{SessionBackendConfig, create_session_store};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(author, version, about = "Greentic GUI server")]
pub struct CliArgs {
    /// Path to a config file (toml/json). Highest precedence.
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,

    /// Override bind address (e.g., 0.0.0.0:8080).
    #[arg(long)]
    pub bind_addr: Option<SocketAddr>,

    /// Override public base URL (for reverse proxy setups).
    #[arg(long)]
    pub public_base_url: Option<String>,

    /// Allow dev-only fields outside dev envs.
    #[arg(long, default_value_t = false)]
    pub allow_dev: bool,

    /// Override project root (defaults to cwd discovery).
    #[arg(long)]
    pub project_root: Option<std::path::PathBuf>,

    /// Print resolved config explain and exit.
    #[arg(long, default_value_t = false)]
    pub explain_config: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    GreenticTelemetrySink::init();
    let cli = CliArgs::parse();
    let cli_layer = load_cli_config_layer(cli.config.clone())?;
    let LoadedConfig {
        app: config,
        provenance,
        warnings,
    } = crate::config::load_config(&cli, cli_layer)?;

    if cli.explain_config {
        let report = explain(&config.resolved, &provenance, &warnings);
        println!("{}", report.text);
        return Ok(());
    }

    if !warnings.is_empty() {
        for warning in warnings {
            tracing::warn!(%warning, "config warning");
        }
    }

    let pack_provider: Arc<dyn crate::packs::PackProvider> = if let Some(dist) = &config.distributor
    {
        if let Some(auth_ref) = &dist.auth_token_ref {
            tracing::info!(%auth_ref, "using distributor auth token reference");
        }
        let cfg = DistributorClientConfig {
            base_url: Some(dist.base_url.clone()),
            environment_id: DistributorEnvironmentId::from(dist.environment_id.clone()),
            tenant: greentic_types::TenantCtx::new(
                greentic_types::EnvId::new(&config.env_id)?,
                greentic_types::TenantId::new(&config.default_tenant)?,
            ),
            auth_token: None,
            extra_headers: None,
            request_timeout: None,
        };
        let client = HttpDistributorClient::new(cfg)?;
        if let Some(packs_json) = &dist.packs_json {
            Arc::new(crate::packs::distributor_provider_from_json(
                client,
                DistributorEnvironmentId::from(dist.environment_id.clone()),
                packs_json,
            )?)
        } else {
            Arc::new(FsPackProvider::new(config.pack_root.clone()))
        }
    } else {
        Arc::new(FsPackProvider::new(config.pack_root.clone()))
    };
    let wit_invoker: Arc<dyn crate::fragments::FragmentInvoker> =
        match WasmtimeFragmentInvoker::new() {
            Ok(inv) => Arc::new(inv),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "failed to init wasmtime fragment invoker; falling back to noop"
                );
                Arc::new(NoopFragmentInvoker)
            }
        };
    let fragment_renderer = Arc::new(CompositeFragmentRenderer::with_wit(wit_invoker));
    let session_backend = if let Ok(redis_url) = std::env::var("REDIS_URL") {
        tracing::info!("using Redis session store");
        SessionBackendConfig::RedisUrl(redis_url)
    } else {
        SessionBackendConfig::InMemory
    };
    let session_store: Arc<dyn greentic_session::store::SessionStore> =
        match create_session_store(session_backend) {
            Ok(store) => Arc::from(store),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "failed to init configured session store; falling back to in-memory"
                );
                Arc::from(
                    create_session_store(SessionBackendConfig::InMemory)
                        .expect("in-memory session store should be available"),
                )
            }
        };
    let session_manager: Arc<dyn crate::integration::SessionManager> =
        Arc::new(RealSessionManager::new(session_store));
    let telemetry: Arc<dyn crate::integration::TelemetrySink> = Arc::new(GreenticTelemetrySink);
    let worker_backend = worker_backend_from_env();
    let worker_host = Arc::new(WorkerHost::new(worker_backend));

    let state = AppState::new(
        config.clone(),
        pack_provider,
        fragment_renderer,
        session_manager,
        telemetry,
        worker_host,
    );

    let addr: SocketAddr = config.bind_addr;
    if let Some(url) = &config.public_base_url {
        tracing::info!(%url, "public base url configured");
    }
    tracing::info!(%addr, "starting greentic-gui server");
    server::run(addr, state).await?;
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();
}

fn load_cli_config_layer(
    path: Option<std::path::PathBuf>,
) -> anyhow::Result<Option<greentic_config::ConfigLayer>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let contents = std::fs::read_to_string(&path)?;
    let layer = match path.extension().and_then(|s| s.to_str()) {
        Some("json") => serde_json::from_str(&contents)?,
        _ => toml::from_str(&contents)?,
    };
    Ok(Some(layer))
}
