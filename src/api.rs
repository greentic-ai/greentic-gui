use crate::integration::{TelemetryEvent, build_tenant_ctx};
use crate::server::AppState;
use crate::tenant::TenantGuiConfig;
use crate::worker::MissingSecretsError;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

pub async fn serve_sdk(State(_state): State<AppState>) -> impl IntoResponse {
    match std::fs::read_to_string("assets/gui-sdk.js") {
        Ok(script) => {
            let mut resp = Response::new(script);
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/javascript"),
            );
            resp
        }
        Err(_) => {
            let script = crate::sdk::sdk_script();
            let mut resp = Response::new(script);
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/javascript"),
            );
            resp
        }
    }
}

pub async fn get_gui_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let domain = super::server::host_from_headers(&headers)
        .unwrap_or_else(|| state.config.default_tenant.clone());
    match state.load_tenant(&domain).await {
        Ok(cfg) => {
            let routes: Vec<serde_json::Value> = cfg
                .features
                .iter()
                .flat_map(|f| {
                    f.manifest.routes.iter().map(|r| {
                        json!({
                            "path": r.path,
                            "authenticated": r.authenticated
                        })
                    })
                })
                .collect();
            let workers: Vec<serde_json::Value> = cfg
                .features
                .iter()
                .flat_map(|f| f.manifest.digital_workers.iter().map(|w| json!(w)))
                .collect();
            let pack_init_hint = pick_pack_hint(&cfg);
            let body = json!({
                "tenant": cfg.tenant_did,
                "domain": cfg.domain,
                "routes": routes,
                "workers": workers,
                "skin": cfg.skin.as_ref().map(|s| s.assets.to_string_lossy()),
                "secret_requirements": cfg.secret_requirements,
                "pack_init_hint": pack_init_hint,
            });
            Json(body).into_response()
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::fragments::{FragmentContext, FragmentRenderer};
    use crate::integration::{
        SessionError, SessionInfo, SessionManager, TelemetryEvent, TelemetrySink,
    };
    use crate::packs::{GuiPack, LayoutConfig, LayoutManifest, PackProvider};
    use crate::worker::{WorkerBackend, WorkerHost};
    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use greentic_types::{
        FlowId, SecretFormat, SecretKey, SecretRequirement, SecretScope, TenantCtx,
    };
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn missing_secrets_response_includes_remediation_hint() {
        let mut req = SecretRequirement::default();
        req.key = SecretKey::new("api/token").unwrap();
        req.description = Some("API token".into());
        req.scope = Some(SecretScope {
            env: "dev".into(),
            tenant: "tenant".into(),
            team: Some("team".into()),
        });
        req.format = Some(SecretFormat::Text);
        let err = MissingSecretsError {
            missing_secrets: vec![req],
            pack_hint: Some("/tmp/demo.gtpack".into()),
            message: None,
        };

        let resp = missing_secrets_response(&err, None);
        assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);

        let bytes = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(json["error"], "missing_secrets");
        assert_eq!(json["pack_hint"], "/tmp/demo.gtpack");
        let remediation = json["remediation"].as_str().expect("remediation string");
        assert!(
            remediation.contains("greentic-secrets init --pack /tmp/demo.gtpack"),
            "remediation hint should include command"
        );
    }

    #[tokio::test]
    async fn gui_config_includes_requirements_and_pack_hint() {
        let req = sample_req();
        let pack_hint = "/tmp/demo.gtpack".to_string();
        let state = test_state(
            vec![req.clone()],
            Some(pack_hint.clone()),
            Arc::new(StubWorkerBackend),
        );
        let headers = HeaderMap::new();
        let resp = get_gui_config(State(state), headers).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["secret_requirements"].as_array().unwrap().len(), 1);
        assert_eq!(
            json["secret_requirements"][0]["key"].as_str().unwrap(),
            req.key.as_str()
        );
        assert_eq!(json["pack_init_hint"].as_str().unwrap(), pack_hint);
    }

    #[tokio::test]
    async fn worker_missing_secrets_returns_428_with_hint() {
        let req = sample_req();
        let pack_hint = "/tmp/demo.gtpack".to_string();
        let backend: Arc<dyn WorkerBackend> = Arc::new(FailingMissingSecretsBackend {
            missing: req.clone(),
            hint: pack_hint.clone(),
        });
        let state = test_state(vec![req.clone()], Some(pack_hint.clone()), backend);
        let body = WorkerMessageRequest {
            worker_id: "worker.missing".into(),
            payload: serde_json::json!({}),
            context: WorkerRequestContext::default(),
        };
        let resp = post_worker_message(State(state), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
        let bytes = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body bytes");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["error"], "missing_secrets");
        assert_eq!(
            json["missing_secrets"][0]["key"].as_str().unwrap(),
            req.key.as_str()
        );
        assert_eq!(json["pack_hint"].as_str().unwrap(), pack_hint);
        assert!(
            json["remediation"]
                .as_str()
                .unwrap()
                .contains("greentic-secrets init --pack")
        );
    }

    fn sample_req() -> SecretRequirement {
        let mut req = SecretRequirement::default();
        req.key = SecretKey::new("api/token").unwrap();
        req.description = Some("API token".into());
        req.scope = Some(SecretScope {
            env: "dev".into(),
            tenant: "tenant".into(),
            team: Some("team".into()),
        });
        req.format = Some(SecretFormat::Text);
        req
    }

    fn test_state(
        requirements: Vec<SecretRequirement>,
        pack_hint: Option<String>,
        worker_backend: Arc<dyn WorkerBackend>,
    ) -> AppState {
        let cfg = AppConfig {
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            public_base_url: None,
            pack_root: std::path::PathBuf::from("./packs"),
            default_tenant: "tenant".into(),
            enable_cors: false,
            pack_cache_ttl: Duration::from_secs(0),
            session_ttl: Duration::from_secs(0),
            env_id: "dev".into(),
            default_team: "team".into(),
            distributor: None,
            oauth_broker_url: None,
            oauth_issuer: None,
            oauth_audience: None,
            oauth_jwks_url: None,
            oauth_required_scopes: vec![],
            resolved: greentic_config_types::GreenticConfig {
                schema_version: greentic_config_types::ConfigVersion::v1(),
                environment: greentic_config_types::EnvironmentConfig {
                    env_id: greentic_types::EnvId::new("dev").unwrap(),
                    deployment: None,
                    connection: None,
                    region: None,
                },
                paths: greentic_config_types::PathsConfig {
                    greentic_root: std::path::PathBuf::from("."),
                    state_dir: std::path::PathBuf::from("."),
                    cache_dir: std::path::PathBuf::from("."),
                    logs_dir: std::path::PathBuf::from("."),
                },
                packs: None,
                services: None,
                events: None,
                runtime: greentic_config_types::RuntimeConfig::default(),
                telemetry: greentic_config_types::TelemetryConfig::default(),
                network: greentic_config_types::NetworkConfig::default(),
                deployer: None,
                secrets: greentic_config_types::SecretsBackendRefConfig::default(),
                dev: None,
            },
        };
        let pack_provider: Arc<dyn PackProvider> = Arc::new(MockPackProvider {
            requirements,
            pack_hint,
        });
        let fragment_renderer: Arc<dyn FragmentRenderer> = Arc::new(NullFragmentRenderer);
        let session_manager: Arc<dyn SessionManager> = Arc::new(NullSessionManager);
        let telemetry: Arc<dyn TelemetrySink> = Arc::new(NullTelemetrySink);
        let worker_host = Arc::new(WorkerHost::new(worker_backend));
        AppState::new(
            cfg,
            pack_provider,
            fragment_renderer,
            session_manager,
            telemetry,
            worker_host,
        )
    }

    struct MockPackProvider {
        requirements: Vec<SecretRequirement>,
        pack_hint: Option<String>,
    }

    #[async_trait]
    impl PackProvider for MockPackProvider {
        async fn load_layout(&self, _tenant: &str) -> anyhow::Result<GuiPack> {
            let manifest = LayoutManifest {
                kind: "gui-layout".into(),
                layout: LayoutConfig {
                    slots: vec!["root".into()],
                    entrypoint_html: "index.html".into(),
                    spa: true,
                    slot_selectors: HashMap::new(),
                },
            };
            Ok(GuiPack::Layout {
                manifest,
                root: std::path::PathBuf::from("/tmp/layout"),
                secret_requirements: self.requirements.clone(),
                pack_hint: self.pack_hint.clone(),
            })
        }

        async fn load_auth(&self, _tenant: &str) -> anyhow::Result<Option<GuiPack>> {
            Ok(None)
        }

        async fn load_skin(&self, _tenant: &str) -> anyhow::Result<Option<GuiPack>> {
            Ok(None)
        }

        async fn load_telemetry(&self, _tenant: &str) -> anyhow::Result<Option<GuiPack>> {
            Ok(None)
        }

        async fn load_features(&self, _tenant: &str) -> anyhow::Result<Vec<GuiPack>> {
            Ok(vec![])
        }

        async fn clear_cache(&self) {}
    }

    struct NullFragmentRenderer;

    #[async_trait]
    impl FragmentRenderer for NullFragmentRenderer {
        async fn render_fragment(
            &self,
            _binding: &crate::packs::FragmentBinding,
            _assets_root: &Path,
            _ctx: FragmentContext,
        ) -> Result<Option<String>, crate::fragments::FragmentError> {
            Ok(None)
        }
    }

    struct NullSessionManager;

    #[async_trait]
    impl SessionManager for NullSessionManager {
        async fn validate(
            &self,
            _token: Option<String>,
        ) -> Result<Option<SessionInfo>, SessionError> {
            Ok(None)
        }

        async fn issue(
            &self,
            ctx: TenantCtx,
            _flow_id: FlowId,
        ) -> Result<SessionInfo, SessionError> {
            Ok(SessionInfo {
                session_id: "session".into(),
                tenant_ctx: ctx,
                user_id: None,
            })
        }
    }

    struct NullTelemetrySink;

    #[async_trait]
    impl TelemetrySink for NullTelemetrySink {
        async fn record_event(&self, _event: TelemetryEvent) {}
    }

    #[derive(Clone)]
    struct StubWorkerBackend;

    #[async_trait]
    impl WorkerBackend for StubWorkerBackend {
        async fn invoke(
            &self,
            _req: greentic_interfaces_host::worker::HostWorkerRequest,
        ) -> anyhow::Result<greentic_interfaces_host::worker::HostWorkerResponse> {
            Err(anyhow::anyhow!("not implemented"))
        }
    }

    struct FailingMissingSecretsBackend {
        missing: SecretRequirement,
        hint: String,
    }

    #[async_trait]
    impl WorkerBackend for FailingMissingSecretsBackend {
        async fn invoke(
            &self,
            _req: greentic_interfaces_host::worker::HostWorkerRequest,
        ) -> anyhow::Result<greentic_interfaces_host::worker::HostWorkerResponse> {
            Err(crate::worker::MissingSecretsError {
                missing_secrets: vec![self.missing.clone()],
                pack_hint: Some(self.hint.clone()),
                message: Some("missing secrets".into()),
            }
            .into())
        }
    }
}

fn pick_pack_hint(cfg: &TenantGuiConfig) -> Option<String> {
    cfg.layout
        .location
        .pack_hint
        .clone()
        .or_else(|| cfg.auth.as_ref().and_then(|a| a.location.pack_hint.clone()))
        .or_else(|| {
            cfg.features
                .iter()
                .find_map(|f| f.location.pack_hint.clone())
        })
        .or_else(|| cfg.skin.as_ref().and_then(|s| s.pack_hint.clone()))
        .or_else(|| cfg.telemetry.as_ref().and_then(|t| t.pack_hint.clone()))
}

#[derive(Debug, Deserialize)]
pub struct WorkerMessageRequest {
    pub worker_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default)]
    pub context: WorkerRequestContext,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
pub struct WorkerRequestContext {
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub route: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

pub async fn post_worker_message(
    State(state): State<AppState>,
    Json(body): Json<WorkerMessageRequest>,
) -> impl IntoResponse {
    let tenant_ctx = build_tenant_ctx(
        &state.config.env_id,
        &state.config.default_tenant,
        Some(&state.config.default_team),
        body.context.user_id.as_deref(),
    );
    match state
        .worker_host
        .invoke_worker(tenant_ctx, &body.worker_id, body.payload)
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(err) => {
            if let Some(missing) = err.downcast_ref::<MissingSecretsError>() {
                let pack_hint = missing.pack_hint.clone();
                return missing_secrets_response(missing, pack_hint);
            }
            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
        }
    }
}

fn missing_secrets_response(missing: &MissingSecretsError, pack_hint: Option<String>) -> Response {
    let resolved_hint = missing.pack_hint.clone().or(pack_hint);
    let remediation = resolved_hint
        .as_ref()
        .map(|hint| format!("greentic-secrets init --pack {hint}"));
    let body = json!({
        "error": "missing_secrets",
        "missing_secrets": missing.missing_secrets,
        "pack_hint": resolved_hint,
        "message": missing.message,
        "remediation": remediation,
    });
    (StatusCode::PRECONDITION_REQUIRED, Json(body)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct TelemetryRequest {
    pub event_type: String,
    pub path: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

pub async fn post_events(
    State(state): State<AppState>,
    Json(body): Json<TelemetryRequest>,
) -> impl IntoResponse {
    crate::integration::set_request_telemetry_ctx(&state.config.default_tenant, None, Some("gui"));
    let event = TelemetryEvent {
        event_type: body.event_type,
        path: body.path,
        timestamp_ms: chrono::Utc::now().timestamp_millis(),
        metadata: body.metadata,
    };
    state.telemetry.record_event(event).await;
    StatusCode::ACCEPTED
}

pub async fn clear_cache(State(state): State<AppState>) -> impl IntoResponse {
    state.clear_cache().await;
    StatusCode::NO_CONTENT
}

#[derive(Debug, Deserialize)]
pub struct SessionIssueRequest {
    pub user_id: String,
    #[serde(default)]
    pub team: Option<String>,
}

pub async fn issue_session(
    State(state): State<AppState>,
    Json(body): Json<SessionIssueRequest>,
) -> impl IntoResponse {
    let tenant_ctx = build_tenant_ctx(
        &state.config.env_id,
        &state.config.default_tenant,
        body.team
            .as_deref()
            .or(Some(state.config.default_team.as_str())),
        Some(body.user_id.as_str()),
    );
    let flow_id = greentic_types::FlowId::new("gui").expect("valid flow id");
    match state.session_manager.issue(tenant_ctx, flow_id).await {
        Ok(session) => (
            StatusCode::CREATED,
            [(
                header::SET_COOKIE,
                format!(
                    "greentic_session_id={}; Path=/; HttpOnly",
                    session.session_id
                ),
            )],
            Json(serde_json::json!({
                "session_id": session.session_id,
                "user_id": session.user_id,
            })),
        )
            .into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}
