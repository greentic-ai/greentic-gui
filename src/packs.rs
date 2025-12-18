use anyhow::{Context, anyhow};
use async_trait::async_trait;
use greentic_distributor_client::{
    DistributorClient, HttpDistributorClient,
    types::{ArtifactLocation, DistributorEnvironmentId, ResolveComponentRequest},
};
use greentic_types::{PackManifest, SecretRequirement};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::fs as tokio_fs;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};
use uuid::Uuid;
use zip::CompressionMethod;
use zip::write::{ExtendedFileOptions, FileOptions};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
#[allow(clippy::enum_variant_names)]
pub enum PackKind {
    GuiLayout,
    GuiAuth,
    GuiFeature,
    GuiSkin,
    GuiTelemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributorPackRef {
    pub pack_id: String,
    pub component_id: String,
    pub version: String,
}

#[derive(Debug, Clone)]
struct ResolvedPack {
    root: PathBuf,
    secret_requirements: Vec<SecretRequirement>,
    pack_hint: Option<String>,
}

pub struct DistributorPackProvider {
    client: HttpDistributorClient,
    env_id: DistributorEnvironmentId,
    /// mapping from kind -> pack ref
    packs: HashMap<PackKind, DistributorPackRef>,
    cache: tokio::sync::Mutex<HashMap<String, ResolvedPack>>,
}

impl DistributorPackProvider {
    pub fn new(
        client: HttpDistributorClient,
        env_id: DistributorEnvironmentId,
        packs: HashMap<PackKind, DistributorPackRef>,
    ) -> Self {
        Self {
            client,
            env_id,
            packs,
            cache: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn resolve(&self, tenant: &str, kind: PackKind) -> anyhow::Result<Option<ResolvedPack>> {
        let Some(pack_ref) = self.packs.get(&kind) else {
            return Ok(None);
        };
        let cache_key = format!("{}::{:?}", tenant, kind);
        if let Some(pack) = self.cache.lock().await.get(&cache_key).cloned() {
            return Ok(Some(pack));
        }

        let tenant_ctx = greentic_types::TenantCtx::new(
            greentic_types::EnvId::new(self.env_id.as_str())?,
            greentic_types::TenantId::new(tenant)?,
        );

        let req = ResolveComponentRequest {
            tenant: tenant_ctx,
            environment_id: self.env_id.clone(),
            pack_id: pack_ref.pack_id.clone(),
            component_id: pack_ref.component_id.clone(),
            version: pack_ref.version.clone(),
            extra: serde_json::Value::Null,
        };

        let resp = self.client.resolve_component(req).await?;
        let mut secret_requirements = resp.secret_requirements.unwrap_or_default();
        let (path, pack_hint) = match resp.artifact {
            ArtifactLocation::FilePath { path } => {
                let path_buf = PathBuf::from(&path);
                (path_buf.clone(), Some(path))
            }
            ArtifactLocation::OciReference { reference } => self.download_oci(&reference).await?,
            ArtifactLocation::DistributorInternal { handle } => {
                let root = self.materialize_internal(&handle).await?;
                (root, None)
            }
        };
        // Merge in requirements from the manifest if present.
        secret_requirements =
            load_secret_requirements_from_pack_root(&path, secret_requirements, false);

        let resolved = ResolvedPack {
            pack_hint: pack_hint.or_else(|| pack_hint_from_root(&path)),
            root: path.clone(),
            secret_requirements,
        };
        self.cache.lock().await.insert(cache_key, resolved.clone());
        Ok(Some(resolved))
    }

    async fn load_manifest_from_path(&self, root: PathBuf) -> anyhow::Result<serde_json::Value> {
        let manifest_path = root.join("gui").join("manifest.json");
        let mut file = tokio_fs::File::open(&manifest_path)
            .await
            .with_context(|| format!("opening manifest {:?}", manifest_path))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await?;
        let json: serde_json::Value = serde_json::from_slice(&buf)?;
        Ok(json)
    }

    async fn load_pack(&self, tenant: &str, kind: PackKind) -> anyhow::Result<Option<GuiPack>> {
        let Some(resolved) = self.resolve(tenant, kind.clone()).await? else {
            return Ok(None);
        };
        let manifest_json = self.load_manifest_from_path(resolved.root.clone()).await?;
        let gui_pack = match manifest_json.get("kind").and_then(|v| v.as_str()) {
            Some("gui-layout") if kind == PackKind::GuiLayout => {
                let manifest: LayoutManifest = serde_json::from_value(manifest_json)?;
                Some(GuiPack::Layout {
                    manifest,
                    root: resolved.root,
                    secret_requirements: resolved.secret_requirements,
                    pack_hint: resolved.pack_hint,
                })
            }
            Some("gui-auth") if kind == PackKind::GuiAuth => {
                let manifest: AuthManifest = serde_json::from_value(manifest_json)?;
                Some(GuiPack::Auth {
                    manifest,
                    root: resolved.root,
                    secret_requirements: resolved.secret_requirements,
                    pack_hint: resolved.pack_hint,
                })
            }
            Some("gui-feature") if kind == PackKind::GuiFeature => {
                let manifest: FeatureManifest = serde_json::from_value(manifest_json)?;
                Some(GuiPack::Feature {
                    manifest,
                    root: resolved.root,
                    secret_requirements: resolved.secret_requirements,
                    pack_hint: resolved.pack_hint,
                })
            }
            Some("gui-skin") if kind == PackKind::GuiSkin => Some(GuiPack::Skin {
                manifest: manifest_json,
                root: resolved.root,
                secret_requirements: resolved.secret_requirements,
                pack_hint: resolved.pack_hint,
            }),
            Some("gui-telemetry") if kind == PackKind::GuiTelemetry => Some(GuiPack::Telemetry {
                manifest: manifest_json,
                root: resolved.root,
                secret_requirements: resolved.secret_requirements,
                pack_hint: resolved.pack_hint,
            }),
            _ => None,
        };
        Ok(gui_pack)
    }

    async fn materialize_internal(&self, handle: &str) -> anyhow::Result<PathBuf> {
        // Temporary workaround: treat handle as a local path if it exists; otherwise error.
        let path = PathBuf::from(handle);
        if path.exists() {
            return Ok(path);
        }
        Err(anyhow!(
            "unsupported distributor internal artifact handle (not a local path): {handle}"
        ))
    }

    async fn download_oci(&self, reference: &str) -> anyhow::Result<(PathBuf, Option<String>)> {
        let tmp_dir = std::env::temp_dir().join(format!("greentic_gui_{}", Uuid::new_v4()));
        tokio_fs::create_dir_all(&tmp_dir).await?;
        let archive_path = tmp_dir.join("artifact.tar");
        let client = reqwest::Client::builder()
            .user_agent("greentic-gui/0.1")
            .build()?;
        let mut req = client.get(reference);
        let bearer = std::env::var("GREENTIC_OCI_BEARER").ok();
        let user = std::env::var("GREENTIC_OCI_USERNAME").ok();
        let pass = std::env::var("GREENTIC_OCI_PASSWORD").ok();
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        } else if let (Some(user), Some(pass)) = (user.clone(), pass.clone()) {
            req = req.basic_auth(user, Some(pass));
        } else if user.is_some() || pass.is_some() {
            warn!(
                "GREENTIC_OCI_USERNAME or GREENTIC_OCI_PASSWORD set without both values; continuing unauthenticated"
            );
        }
        let mut resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "failed to download OCI reference {}: status {}",
                reference,
                resp.status()
            ));
        }
        let mut file = tokio_fs::File::create(&archive_path).await?;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk).await?;
        }
        // naive extraction: assume tar.gz or tar; attempt both
        if let Err(err) = tokio::task::spawn_blocking({
            let archive_path = archive_path.clone();
            let tmp_dir = tmp_dir.clone();
            move || -> anyhow::Result<()> {
                let file = std::fs::File::open(&archive_path)?;
                let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
                archive.unpack(&tmp_dir)?;
                Ok(())
            }
        })
        .await?
        {
            warn!(?err, "tar.gz extraction failed; trying plain tar");
            let _ = tokio::task::spawn_blocking({
                let archive_path = archive_path.clone();
                let tmp_dir = tmp_dir.clone();
                move || -> anyhow::Result<()> {
                    let file = std::fs::File::open(&archive_path)?;
                    let mut archive = tar::Archive::new(file);
                    archive.unpack(&tmp_dir)?;
                    Ok(())
                }
            })
            .await?;
        }
        let pack_hint = find_gtpack_in_dir(&tmp_dir).or_else(|| create_local_gtpack(&tmp_dir));
        Ok((tmp_dir, pack_hint))
    }

    pub async fn reset_cache(&self) {
        let mut cache = self.cache.lock().await;
        cache.clear();
        tracing::info!("pack cache cleared");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutManifest {
    pub kind: String,
    pub layout: LayoutConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub slots: Vec<String>,
    pub entrypoint_html: String,
    pub spa: bool,
    pub slot_selectors: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthManifest {
    pub kind: String,
    pub routes: Vec<AuthRoute>,
    pub oauth: serde_json::Value,
    pub ui_bindings: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRoute {
    pub path: String,
    #[serde(default)]
    pub public: bool,
    pub html: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureManifest {
    pub kind: String,
    pub routes: Vec<FeatureRoute>,
    #[serde(default)]
    pub digital_workers: Vec<DigitalWorker>,
    #[serde(default)]
    pub fragments: Vec<FragmentBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureRoute {
    pub path: String,
    #[serde(default)]
    pub authenticated: bool,
    pub html: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigitalWorker {
    pub id: String,
    pub worker_id: String,
    pub attach: WorkerAttach,
    pub routes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAttach {
    pub mode: String,
    pub selector: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FragmentBinding {
    pub id: String,
    pub selector: String,
    #[serde(rename = "component_world")]
    pub component_world: String,
    #[serde(rename = "component_name")]
    pub component_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum GuiPack {
    #[serde(rename = "gui-layout")]
    Layout {
        manifest: LayoutManifest,
        root: PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        secret_requirements: Vec<SecretRequirement>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pack_hint: Option<String>,
    },
    #[serde(rename = "gui-auth")]
    Auth {
        manifest: AuthManifest,
        root: PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        secret_requirements: Vec<SecretRequirement>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pack_hint: Option<String>,
    },
    #[serde(rename = "gui-feature")]
    Feature {
        manifest: FeatureManifest,
        root: PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        secret_requirements: Vec<SecretRequirement>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pack_hint: Option<String>,
    },
    #[serde(rename = "gui-skin")]
    Skin {
        manifest: serde_json::Value,
        root: PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        secret_requirements: Vec<SecretRequirement>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pack_hint: Option<String>,
    },
    #[serde(rename = "gui-telemetry")]
    Telemetry {
        manifest: serde_json::Value,
        root: PathBuf,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        secret_requirements: Vec<SecretRequirement>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pack_hint: Option<String>,
    },
}

impl GuiPack {
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        match self {
            GuiPack::Layout { root, .. }
            | GuiPack::Auth { root, .. }
            | GuiPack::Feature { root, .. }
            | GuiPack::Skin { root, .. }
            | GuiPack::Telemetry { root, .. } => root.as_path(),
        }
    }

    #[allow(dead_code)]
    pub fn assets_root(&self) -> PathBuf {
        self.root().join("gui").join("assets")
    }
}

#[async_trait]
pub trait PackProvider: Send + Sync {
    async fn load_layout(&self, tenant: &str) -> anyhow::Result<GuiPack>;
    async fn load_auth(&self, tenant: &str) -> anyhow::Result<Option<GuiPack>>;
    async fn load_skin(&self, tenant: &str) -> anyhow::Result<Option<GuiPack>>;
    async fn load_telemetry(&self, tenant: &str) -> anyhow::Result<Option<GuiPack>>;
    async fn load_features(&self, tenant: &str) -> anyhow::Result<Vec<GuiPack>>;
    async fn clear_cache(&self);
}

/// File-system backed pack provider for development and tests.
pub struct FsPackProvider {
    root: PathBuf,
}

impl FsPackProvider {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    async fn load_manifest(&self, path: &Path) -> anyhow::Result<serde_json::Value> {
        let manifest_path = path.join("gui").join("manifest.json");
        let mut file = tokio_fs::File::open(&manifest_path)
            .await
            .with_context(|| format!("opening manifest {:?}", manifest_path))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await?;
        let json: serde_json::Value = serde_json::from_slice(&buf)?;
        Ok(json)
    }

    fn tenant_pack_root(&self, tenant: &str, pack_name: &str) -> PathBuf {
        self.root.join(tenant).join(pack_name)
    }

    fn discover_packs(&self, tenant: &str) -> anyhow::Result<Vec<String>> {
        let root = self.root.join(tenant);
        if !root.exists() {
            return Ok(vec![]);
        }
        let entries = fs::read_dir(root)?;
        let mut packs = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                packs.push(name.to_string());
            }
        }
        Ok(packs)
    }
}

#[async_trait]
impl PackProvider for FsPackProvider {
    async fn load_layout(&self, tenant: &str) -> anyhow::Result<GuiPack> {
        let candidates = self.discover_packs(tenant)?;
        for name in candidates {
            let pack_root = self.tenant_pack_root(tenant, &name);
            let manifest_json = self.load_manifest(&pack_root).await?;
            if manifest_json.get("kind").and_then(|v| v.as_str()) == Some("gui-layout") {
                let manifest: LayoutManifest = serde_json::from_value(manifest_json.clone())
                    .context("parse layout manifest")?;
                let secret_requirements =
                    load_secret_requirements_from_pack_root(&pack_root, vec![], true);
                return Ok(GuiPack::Layout {
                    manifest,
                    root: pack_root.clone(),
                    secret_requirements,
                    pack_hint: pack_hint_from_root(&pack_root),
                });
            }
        }
        Err(anyhow!("no layout pack found for tenant {}", tenant))
    }

    async fn load_auth(&self, tenant: &str) -> anyhow::Result<Option<GuiPack>> {
        let candidates = self.discover_packs(tenant)?;
        for name in candidates {
            let pack_root = self.tenant_pack_root(tenant, &name);
            let manifest_json = self.load_manifest(&pack_root).await?;
            if manifest_json.get("kind").and_then(|v| v.as_str()) == Some("gui-auth") {
                let manifest: AuthManifest =
                    serde_json::from_value(manifest_json.clone()).context("parse auth manifest")?;
                let secret_requirements =
                    load_secret_requirements_from_pack_root(&pack_root, vec![], true);
                return Ok(Some(GuiPack::Auth {
                    manifest,
                    root: pack_root.clone(),
                    secret_requirements,
                    pack_hint: pack_hint_from_root(&pack_root),
                }));
            }
        }
        Ok(None)
    }

    async fn load_skin(&self, tenant: &str) -> anyhow::Result<Option<GuiPack>> {
        let candidates = self.discover_packs(tenant)?;
        for name in candidates {
            let pack_root = self.tenant_pack_root(tenant, &name);
            let manifest_json = self.load_manifest(&pack_root).await?;
            if manifest_json.get("kind").and_then(|v| v.as_str()) == Some("gui-skin") {
                let secret_requirements =
                    load_secret_requirements_from_pack_root(&pack_root, vec![], true);
                return Ok(Some(GuiPack::Skin {
                    manifest: manifest_json,
                    root: pack_root.clone(),
                    secret_requirements,
                    pack_hint: pack_hint_from_root(&pack_root),
                }));
            }
        }
        Ok(None)
    }

    async fn load_telemetry(&self, tenant: &str) -> anyhow::Result<Option<GuiPack>> {
        let candidates = self.discover_packs(tenant)?;
        for name in candidates {
            let pack_root = self.tenant_pack_root(tenant, &name);
            let manifest_json = self.load_manifest(&pack_root).await?;
            if manifest_json.get("kind").and_then(|v| v.as_str()) == Some("gui-telemetry") {
                let secret_requirements =
                    load_secret_requirements_from_pack_root(&pack_root, vec![], true);
                return Ok(Some(GuiPack::Telemetry {
                    manifest: manifest_json,
                    root: pack_root.clone(),
                    secret_requirements,
                    pack_hint: pack_hint_from_root(&pack_root),
                }));
            }
        }
        Ok(None)
    }

    async fn load_features(&self, tenant: &str) -> anyhow::Result<Vec<GuiPack>> {
        let candidates = self.discover_packs(tenant)?;
        let mut features = Vec::new();
        for name in candidates {
            let pack_root = self.tenant_pack_root(tenant, &name);
            let manifest_json = self.load_manifest(&pack_root).await?;
            if manifest_json.get("kind").and_then(|v| v.as_str()) == Some("gui-feature") {
                let manifest: FeatureManifest = serde_json::from_value(manifest_json.clone())
                    .context("parse feature manifest")?;
                let secret_requirements =
                    load_secret_requirements_from_pack_root(&pack_root, vec![], true);
                features.push(GuiPack::Feature {
                    manifest,
                    root: pack_root.clone(),
                    secret_requirements,
                    pack_hint: pack_hint_from_root(&pack_root),
                });
            }
        }
        Ok(features)
    }

    async fn clear_cache(&self) {}
}

#[async_trait]
impl PackProvider for DistributorPackProvider {
    async fn load_layout(&self, tenant: &str) -> anyhow::Result<GuiPack> {
        let Some(pack) = self.load_pack(tenant, PackKind::GuiLayout).await? else {
            return Err(anyhow!("no layout pack configured for tenant {}", tenant));
        };
        Ok(pack)
    }

    async fn load_auth(&self, tenant: &str) -> anyhow::Result<Option<GuiPack>> {
        self.load_pack(tenant, PackKind::GuiAuth).await
    }

    async fn load_skin(&self, tenant: &str) -> anyhow::Result<Option<GuiPack>> {
        self.load_pack(tenant, PackKind::GuiSkin).await
    }

    async fn load_telemetry(&self, tenant: &str) -> anyhow::Result<Option<GuiPack>> {
        self.load_pack(tenant, PackKind::GuiTelemetry).await
    }

    async fn load_features(&self, tenant: &str) -> anyhow::Result<Vec<GuiPack>> {
        match self.load_pack(tenant, PackKind::GuiFeature).await? {
            Some(pack) => Ok(vec![pack]),
            None => Ok(vec![]),
        }
    }

    async fn clear_cache(&self) {
        self.reset_cache().await;
    }
}

pub fn normalize_route(path: &str) -> String {
    let re = Regex::new(r"/+").unwrap();
    let normalized = re.replace_all(path, "/");
    let mut s = normalized.trim().to_string();
    if !s.starts_with('/') {
        s = format!("/{}", s);
    }
    s
}

fn load_secret_requirements_from_pack_root(
    root: &Path,
    seed: Vec<SecretRequirement>,
    try_manifest_json: bool,
) -> Vec<SecretRequirement> {
    let mut reqs = seed;
    if let Some(mut manifest_reqs) = read_secret_requirements_from_manifest_cbor(root) {
        reqs.append(&mut manifest_reqs);
    }
    if try_manifest_json
        && let Some(mut manifest_reqs) = read_secret_requirements_from_manifest_json(root)
    {
        reqs.append(&mut manifest_reqs);
    }
    dedup_requirements(reqs)
}

fn read_secret_requirements_from_manifest_cbor(root: &Path) -> Option<Vec<SecretRequirement>> {
    let manifest_path = root.join("manifest.cbor");
    let file = std::fs::File::open(&manifest_path).ok()?;
    let manifest: PackManifest = match ciborium::de::from_reader(file) {
        Ok(m) => m,
        Err(err) => {
            debug!(?err, path = ?manifest_path, "failed to parse manifest.cbor for secrets");
            return None;
        }
    };
    Some(manifest.secret_requirements)
}

fn read_secret_requirements_from_manifest_json(root: &Path) -> Option<Vec<SecretRequirement>> {
    let manifest_path = root.join("manifest.json");
    if !manifest_path.exists() {
        return None;
    }
    let file = std::fs::File::open(&manifest_path).ok()?;
    let manifest: PackManifest = match serde_json::from_reader(file) {
        Ok(m) => m,
        Err(err) => {
            debug!(?err, path = ?manifest_path, "failed to parse manifest.json for secrets");
            return None;
        }
    };
    Some(manifest.secret_requirements)
}

fn pack_hint_from_root(root: &Path) -> Option<String> {
    if root.extension().is_some_and(|ext| ext == "gtpack") {
        return Some(root.to_string_lossy().to_string());
    }
    if let Some(path) = find_gtpack_in_dir(root) {
        return Some(path);
    }
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("gtpack"))
            {
                return Some(path.to_string_lossy().to_string());
            }
        }
    }
    let manifest_cbor = root.join("manifest.cbor");
    if manifest_cbor.exists() {
        return Some(manifest_cbor.to_string_lossy().to_string());
    }
    None
}

fn dedup_requirements(reqs: Vec<SecretRequirement>) -> Vec<SecretRequirement> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for req in reqs {
        let key = requirement_key(&req);
        if seen.insert(key) {
            out.push(req);
        }
    }
    out
}

fn requirement_key(req: &SecretRequirement) -> String {
    let scope = req
        .scope
        .as_ref()
        .map(|s| {
            format!(
                "{}/{}/{}",
                s.env,
                s.tenant,
                s.team.as_deref().unwrap_or("_")
            )
        })
        .unwrap_or_else(|| "_/_/_".to_string());
    format!("{}::{}", scope, req.key.as_str())
}

fn find_gtpack_in_dir(root: &Path) -> Option<String> {
    if root.is_file()
        && root
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("gtpack"))
    {
        return Some(root.to_string_lossy().to_string());
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("gtpack"))
                {
                    return Some(path.to_string_lossy().to_string());
                }
            }
        }
    }
    None
}

fn create_local_gtpack(root: &Path) -> Option<String> {
    let manifest_path = root.join("manifest.cbor");
    if !manifest_path.exists() {
        return None;
    }
    let out_path = root.join("cached.gtpack");
    let file = std::fs::File::create(&out_path).ok()?;
    let mut writer = zip::ZipWriter::new(file);
    let options: FileOptions<ExtendedFileOptions> = FileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o644);

    let manifest_bytes = fs::read(&manifest_path).ok()?;
    if writer.start_file("manifest.cbor", options.clone()).is_err() {
        return None;
    }
    if writer.write_all(&manifest_bytes).is_err() {
        return None;
    }

    for (_prefix, dir) in [
        ("components", root.join("components")),
        ("assets", root.join("assets")),
    ] {
        if dir.exists() {
            for entry in list_files_recursively(&dir) {
                let rel = entry.strip_prefix(root).ok()?;
                let logical = rel.to_string_lossy().replace('\\', "/");
                if writer.start_file(logical, options.clone()).is_err() {
                    return None;
                }
                let bytes = fs::read(&entry).ok()?;
                if writer.write_all(&bytes).is_err() {
                    return None;
                }
            }
        }
    }

    if writer.finish().is_err() {
        return None;
    }
    Some(out_path.to_string_lossy().to_string())
}

fn list_files_recursively(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    files.push(path);
                }
            }
        }
    }
    files
}

/// Build a distributor-backed pack provider using config JSON mapping.
pub fn distributor_provider_from_json(
    client: HttpDistributorClient,
    env_id: DistributorEnvironmentId,
    packs_json: &str,
) -> anyhow::Result<DistributorPackProvider> {
    let map: HashMap<String, DistributorPackRef> = serde_json::from_str(packs_json)?;
    let mut packs = HashMap::new();
    for (k, v) in map {
        let kind = match k.as_str() {
            "layout" | "gui-layout" => PackKind::GuiLayout,
            "auth" | "gui-auth" => PackKind::GuiAuth,
            "feature" | "gui-feature" => PackKind::GuiFeature,
            "skin" | "gui-skin" => PackKind::GuiSkin,
            "telemetry" | "gui-telemetry" => PackKind::GuiTelemetry,
            _ => continue,
        };
        packs.insert(kind, v);
    }
    Ok(DistributorPackProvider::new(client, env_id, packs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_types::{PackId, PackKind, PackManifest, PackSignatures, SecretKey, SecretScope};
    use semver::Version;

    #[test]
    fn normalizes_routes() {
        assert_eq!(normalize_route("foo/bar"), "/foo/bar");
        assert_eq!(normalize_route("/foo/bar"), "/foo/bar");
        assert_eq!(normalize_route("/foo//bar"), "/foo/bar");
        assert_eq!(normalize_route("foo///bar"), "/foo/bar");
    }

    #[test]
    fn loads_secret_requirements_from_manifest_cbor() {
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("manifest.cbor");
        let mut req = SecretRequirement::default();
        req.key = SecretKey::new("api/token").unwrap();
        req.description = Some("token".into());
        req.scope = Some(SecretScope {
            env: "dev".into(),
            tenant: "tenant".into(),
            team: Some("team".into()),
        });
        let manifest = PackManifest {
            schema_version: "1".into(),
            pack_id: PackId::new("demo.pack").unwrap(),
            version: Version::new(0, 1, 0),
            kind: PackKind::Application,
            publisher: "demo".into(),
            components: vec![],
            flows: vec![],
            dependencies: vec![],
            capabilities: vec![],
            secret_requirements: vec![req.clone()],
            signatures: PackSignatures::default(),
        };
        let file = std::fs::File::create(manifest_path).unwrap();
        ciborium::ser::into_writer(&manifest, file).unwrap();

        let reqs = load_secret_requirements_from_pack_root(temp.path(), vec![], true);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].key.as_str(), req.key.as_str());
        assert_eq!(reqs[0].description, req.description);
    }

    #[test]
    fn creates_gtpack_hint_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("manifest.cbor");
        let manifest = PackManifest {
            schema_version: "1".into(),
            pack_id: PackId::new("demo.pack").unwrap(),
            version: Version::new(0, 1, 0),
            kind: PackKind::Application,
            publisher: "demo".into(),
            components: vec![],
            flows: vec![],
            dependencies: vec![],
            capabilities: vec![],
            secret_requirements: vec![],
            signatures: PackSignatures::default(),
        };
        let file = std::fs::File::create(manifest_path).unwrap();
        ciborium::ser::into_writer(&manifest, file).unwrap();

        let gtpack = create_local_gtpack(temp.path()).expect("gtpack path");
        assert!(
            gtpack.ends_with(".gtpack"),
            "gtpack hint should point to a .gtpack file"
        );
        assert!(PathBuf::from(gtpack).exists());
    }
}
