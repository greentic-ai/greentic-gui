use crate::api;
use crate::auth;
use crate::config::AppConfig;
use crate::fragments::{FragmentError, FragmentRenderer, inject_fragments};
use crate::integration::{SessionManager, TelemetryEvent, TelemetrySink};
use crate::packs::PackProvider;
use crate::routing::{RouteDecision, resolve_route};
use crate::tenant::TenantGuiConfig;
use crate::worker::WorkerHost;
use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::fs;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::warn;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub pack_provider: Arc<dyn PackProvider>,
    pub fragment_renderer: Arc<dyn FragmentRenderer>,
    pub session_manager: Arc<dyn SessionManager>,
    pub telemetry: Arc<dyn TelemetrySink>,
    pub worker_host: Arc<WorkerHost>,
    tenant_cache: Arc<RwLock<HashMap<String, CachedTenant>>>,
    cache_hits: Arc<AtomicU64>,
    cache_misses: Arc<AtomicU64>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        pack_provider: Arc<dyn PackProvider>,
        fragment_renderer: Arc<dyn FragmentRenderer>,
        session_manager: Arc<dyn SessionManager>,
        telemetry: Arc<dyn TelemetrySink>,
        worker_host: Arc<WorkerHost>,
    ) -> Self {
        Self {
            config,
            pack_provider,
            fragment_renderer,
            session_manager,
            telemetry,
            worker_host,
            tenant_cache: Arc::new(RwLock::new(HashMap::new())),
            cache_hits: Arc::new(AtomicU64::new(0)),
            cache_misses: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn load_tenant(&self, domain: &str) -> anyhow::Result<TenantGuiConfig> {
        let tenant = self.config.tenant_for_domain(domain).to_string();
        if let Some(cfg) = self.cached_tenant(&tenant).await {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(cfg);
        }

        let cfg = TenantGuiConfig::load(&tenant, domain, self.pack_provider.clone()).await?;
        self.insert_cache(tenant, cfg.clone()).await;
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
        Ok(cfg)
    }

    async fn cached_tenant(&self, tenant: &str) -> Option<TenantGuiConfig> {
        let ttl = self.config.pack_cache_ttl;
        if ttl.is_zero() {
            return None;
        }
        let cache = self.tenant_cache.read().await;
        if let Some(entry) = cache.get(tenant)
            && entry.created.elapsed() <= ttl
        {
            return Some(entry.config.clone());
        }
        None
    }

    async fn insert_cache(&self, tenant: String, cfg: TenantGuiConfig) {
        if self.config.pack_cache_ttl.is_zero() {
            return;
        }
        let mut cache = self.tenant_cache.write().await;
        cache.insert(
            tenant,
            CachedTenant {
                config: cfg,
                created: Instant::now(),
            },
        );
    }

    pub async fn clear_cache(&self) {
        {
            let mut cache = self.tenant_cache.write().await;
            cache.clear();
        }
        self.pack_provider.clear_cache().await;
    }

    pub fn cache_stats(&self) -> (u64, u64) {
        (
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
        )
    }
}

#[derive(Clone)]
struct CachedTenant {
    config: TenantGuiConfig,
    created: Instant,
}

pub async fn run(addr: SocketAddr, state: AppState) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")
}

pub fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/greentic/gui-sdk.js", get(api::serve_sdk))
        .route("/api/gui/config", get(api::get_gui_config))
        .route("/api/gui/worker/message", post(api::post_worker_message))
        .route("/api/gui/events", post(api::post_events))
        .route("/api/gui/cache/clear", post(api::clear_cache))
        .route("/api/gui/packs/reload", post(reload_packs))
        .route("/api/gui/session", post(api::issue_session))
        .route("/auth/:provider/start", get(auth::start_auth))
        .route("/auth/:provider/callback", get(auth::auth_callback))
        .route("/auth/logout", get(auth::logout))
        .route("/tests/sdk-harness", get(serve_sdk_harness))
        .route("/*path", get(serve_route));

    if state.config.enable_cors {
        use tower_http::cors::{Any, CorsLayer};
        let cors = CorsLayer::new()
            .allow_methods(Any)
            .allow_headers(Any)
            .allow_origin(Any);
        router = router.layer(cors);
    }

    let mut router = router
        .with_state(state.clone())
        .layer(TraceLayer::new_for_http());

    if let Some(base) = &state.config.public_base_url {
        let layer = tower_http::set_header::SetResponseHeaderLayer::if_not_present(
            header::HeaderName::from_static("x-greentic-public-base"),
            http::HeaderValue::from_str(base)
                .unwrap_or_else(|_| http::HeaderValue::from_static("")),
        );
        router = router.layer(layer);
    }

    router
}

#[axum::debug_handler]
async fn serve_route(
    State(state): State<AppState>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let path = if path.starts_with('/') {
        path
    } else {
        format!("/{}", path)
    };
    let domain = host_from_headers(&headers).unwrap_or_else(|| state.config.default_tenant.clone());
    let tenant_cfg = match state.load_tenant(&domain).await {
        Ok(cfg) => cfg,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    };

    let span = tracing::info_span!(
        "gui_request",
        tenant = %tenant_cfg.tenant_did,
        domain = %tenant_cfg.domain,
        path = %path
    );
    let _enter = span.enter();

    let session_token = session_cookie(&headers);
    if let Some(ref token) = session_token {
        crate::integration::set_request_telemetry_ctx(
            &tenant_cfg.tenant_did,
            Some(token.as_str()),
            Some("gui"),
        );
    } else {
        crate::integration::set_request_telemetry_ctx(&tenant_cfg.tenant_did, None, Some("gui"));
    }
    let decision = match resolve_route(
        &tenant_cfg,
        &path,
        session_token,
        state.session_manager.as_ref(),
    )
    .await
    {
        Ok(decision) => decision,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    };

    match decision {
        RouteDecision::Serve(content) => {
            let content = *content;
            let base_html = content.html.clone();
            let html = match inject_fragments(
                base_html.clone(),
                &content.fragments,
                content.session.as_ref(),
                &tenant_cfg.tenant_did,
                &path,
                state.fragment_renderer.clone(),
            )
            .await
            {
                Ok(html) => html,
                Err(err) => {
                    if matches!(err, FragmentError::MissingSecrets(_)) {
                        let pack_hint = tenant_cfg.layout.location.pack_hint.clone();
                        let body = serde_json::json!({
                            "error": "missing_secrets",
                            "pack_hint": pack_hint,
                            "remediation": pack_hint.as_ref().map(|hint| format!("greentic-secrets init --pack {hint}")),
                        });
                        return (StatusCode::PRECONDITION_REQUIRED, Json(body)).into_response();
                    }
                    warn!(?err, "failed to inject fragments");
                    base_html
                }
            };
            Html(html).into_response()
        }
        RouteDecision::Redirect(target) => (
            StatusCode::FOUND,
            [(header::LOCATION, target.as_str())],
            "redirecting",
        )
            .into_response(),
        RouteDecision::NotFound => match path.as_str() {
            "/login" => match fs::read_to_string("assets/login.html").await {
                Ok(html) => Html(html).into_response(),
                Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
            },
            "/logout" => Redirect::to("/auth/logout").into_response(),
            "/unauthorized" => match fs::read_to_string("assets/unauthorized.html").await {
                Ok(html) => Html(html).into_response(),
                Err(_) => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
            },
            _ => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
}

async fn serve_sdk_harness() -> impl IntoResponse {
    match fs::read_to_string("assets/sdk-harness.html").await {
        Ok(html) => Html(html).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Force reload of tenant packs by clearing cache and reloading default tenant.
async fn reload_packs(
    State(state): State<AppState>,
    Json(body): Json<HashMap<String, String>>,
) -> impl IntoResponse {
    state.clear_cache().await;
    let tenant = body
        .get("tenant")
        .map(|s| s.as_str())
        .unwrap_or(&state.config.default_tenant);
    let result = state.load_tenant(tenant).await;
    let (status, err) = match result {
        Ok(_) => (StatusCode::NO_CONTENT, None),
        Err(err) => {
            tracing::warn!(?err, "pack reload encountered errors");
            (StatusCode::PARTIAL_CONTENT, Some(err.to_string()))
        }
    };
    let (hits, misses) = state.cache_stats();
    let event = TelemetryEvent {
        event_type: "cache.reload".into(),
        path: "/api/gui/packs/reload".into(),
        timestamp_ms: chrono::Utc::now().timestamp_millis(),
        metadata: json!({
            "tenant": tenant,
            "cache_hits": hits,
            "cache_misses": misses,
            "status": status.as_u16(),
            "error": err,
        }),
    };
    state.telemetry.record_event(event).await;
    status
}

pub fn host_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers.get(header::COOKIE).and_then(|cookie_hdr| {
        cookie_hdr.to_str().ok().and_then(|raw| {
            raw.split(';').find_map(|kv| {
                let trimmed = kv.trim();
                trimmed
                    .strip_prefix("greentic_session_id=")
                    .map(|rest| rest.to_string())
            })
        })
    })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
