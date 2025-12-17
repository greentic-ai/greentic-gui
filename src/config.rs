use greentic_config::{ConfigLayer, ConfigResolver};
use greentic_config_types::{GreenticConfig, PackSourceConfig};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Runtime configuration for the GUI server resolved from greentic-config.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub public_base_url: Option<String>,
    pub pack_root: PathBuf,
    pub default_tenant: String,
    pub enable_cors: bool,
    pub pack_cache_ttl: Duration,
    pub session_ttl: Duration,
    pub env_id: String,
    pub default_team: String,
    pub distributor: Option<DistributorConfig>,
    pub oauth_broker_url: Option<String>,
    pub oauth_issuer: Option<String>,
    pub oauth_audience: Option<String>,
    pub oauth_jwks_url: Option<String>,
    pub oauth_required_scopes: Vec<String>,
    pub resolved: GreenticConfig,
}

#[derive(Debug, Clone)]
pub struct DistributorConfig {
    pub base_url: String,
    pub environment_id: String,
    /// Reference to a secrets entry for the token (not the token itself).
    pub auth_token_ref: Option<String>,
    /// JSON string mapping pack kind to {pack_id, component_id, version}
    pub packs_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub app: AppConfig,
    pub provenance: greentic_config::ProvenanceMap,
    pub warnings: Vec<String>,
}

impl AppConfig {
    pub fn tenant_for_domain<'a>(&'a self, _domain: &'a str) -> &'a str {
        // No domain map yet; default to configured tenant.
        &self.default_tenant
    }
}

pub fn load_config(
    cli: &crate::CliArgs,
    cli_layer: Option<ConfigLayer>,
) -> anyhow::Result<LoadedConfig> {
    let mut resolver = ConfigResolver::new().allow_dev(cli.allow_dev);
    if let Some(root) = cli.project_root.clone() {
        resolver = resolver.with_project_root(root);
    } else if let Some(root) = project_root_from_cwd()? {
        resolver = resolver.with_project_root(root);
    }

    if let Some(layer) = cli_layer {
        resolver = resolver.with_cli_overrides(layer);
    }

    let resolved = resolver.load()?;
    let app = map_to_app_config(resolved.config.clone(), cli);
    Ok(LoadedConfig {
        app,
        provenance: resolved.provenance,
        warnings: resolved.warnings,
    })
}

fn map_to_app_config(resolved: GreenticConfig, cli: &crate::CliArgs) -> AppConfig {
    let bind_addr = cli
        .bind_addr
        .unwrap_or_else(|| "0.0.0.0:8080".parse().unwrap());
    let public_base_url = cli.public_base_url.clone();
    let env_id = resolved.environment.env_id.to_string();
    let default_tenant = resolved
        .dev
        .as_ref()
        .map(|d| d.default_tenant.clone())
        .unwrap_or_else(|| "tenant-default".to_string());
    let default_team = resolved
        .dev
        .as_ref()
        .and_then(|d| d.default_team.clone())
        .unwrap_or_else(|| "gui".to_string());

    // Place packs under the state dir by default to avoid scattering artifacts.
    let pack_root = resolved.paths.state_dir.join("packs");

    let distributor = distributor_from_config(&resolved);

    AppConfig {
        bind_addr,
        public_base_url,
        pack_root,
        default_tenant,
        enable_cors: false,
        pack_cache_ttl: Duration::from_secs(0),
        session_ttl: Duration::from_secs(0),
        env_id,
        default_team,
        distributor,
        oauth_broker_url: std::env::var("OAUTH_BROKER_URL").ok(),
        oauth_issuer: std::env::var("OAUTH_ISSUER").ok(),
        oauth_audience: std::env::var("OAUTH_AUDIENCE").ok(),
        oauth_jwks_url: std::env::var("OAUTH_JWKS_URL").ok(),
        oauth_required_scopes: oauth_required_scopes(),
        resolved,
    }
}

fn distributor_from_config(resolved: &GreenticConfig) -> Option<DistributorConfig> {
    resolved
        .packs
        .as_ref()
        .and_then(|packs| match &packs.source {
            PackSourceConfig::HttpIndex { url } => Some(DistributorConfig {
                base_url: url.clone(),
                environment_id: resolved.environment.env_id.to_string(),
                auth_token_ref: None,
                packs_json: None,
            }),
            PackSourceConfig::OciRegistry { reference } => Some(DistributorConfig {
                base_url: reference.clone(),
                environment_id: resolved.environment.env_id.to_string(),
                auth_token_ref: None,
                packs_json: None,
            }),
            PackSourceConfig::LocalIndex { .. } => None,
        })
}

fn oauth_required_scopes() -> Vec<String> {
    std::env::var("OAUTH_REQUIRED_SCOPES")
        .ok()
        .map(|s| {
            s.split(',')
                .filter(|v| !v.is_empty())
                .map(|v| v.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn project_root_from_cwd() -> anyhow::Result<Option<PathBuf>> {
    let cwd = std::env::current_dir()?;
    Ok(greentic_config::discover_project_root(&cwd))
}
