//! Client for the proposed, cluster-scoped Axo CSI API documented in docs/axo-api.md.
//! The API is not present in the existing Axo repository. No Ceph or hypervisor
//! credential, journal interpretation, lock breaking, or fencing belongs here.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use reqwest::{Method, StatusCode, Url, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tonic::{Code, Status};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TOKEN_BYTES: u64 = 64 * 1024;
pub const API_VERSION: &str = "clever-csi-v1";
pub const REQUIRED_FEATURES: &[&str] = &[
    "volume.ensure",
    "volume.resolve",
    "volume.delete",
    "volume.expand",
    "volume.clone",
    "volume.restore",
    "attachment.ensure",
    "attachment.delete",
    "snapshot.ensure",
    "snapshot.delete",
    "snapshot.list",
    "legacy-pool-routing",
    "attachment.readonly",
];

#[derive(Clone)]
pub struct ControllerConfig {
    pub axo_url: String,
    pub organisation_id: String,
    pub cluster_id: String,
    pub token_file: PathBuf,
    /// Explicit development-only escape hatch. Certificate verification is never disabled.
    pub allow_http: bool,
    pub timeout: Duration,
}

#[derive(Clone)]
pub struct AxoClient {
    client: reqwest::Client,
    root: Url,
    token_file: PathBuf,
    timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeMode {
    Filesystem,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeShape {
    pub volume_mode: VolumeMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum ContentSource {
    Snapshot(String),
    Volume(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EnsureVolume {
    pub name: String,
    pub required_bytes: i64,
    pub limit_bytes: Option<i64>,
    #[serde(flatten)]
    pub shape: VolumeShape,
    pub parameters: BTreeMap<String, String>,
    pub source: Option<ContentSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeRecord {
    pub volume_id: String,
    pub capacity_bytes: i64,
    #[serde(flatten)]
    pub shape: VolumeShape,
    pub format_policy: String,
    pub source: Option<ContentSource>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResolveVolume {
    pub volume_id: String,
    pub volume_context: BTreeMap<String, String>,
    pub parameters: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VolumeId {
    pub volume_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EnsureAttachment {
    pub volume_id: String,
    /// Exact VM incarnation on the new path. A historical Kubernetes node name
    /// is permitted only when Axo authoritatively classifies it as legacy-krbd;
    /// it must never be resolved into a QEMU attachment target.
    pub node_id: String,
    pub readonly: bool,
    #[serde(flatten)]
    pub shape: VolumeShape,
    pub volume_context: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteAttachment {
    pub volume_id: String,
    /// None means detach this volume from all incarnations, as required by CSI.
    pub node_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum AttachmentRecord {
    /// Only for a platform-verified legacy node during coexistence.
    LegacyKrbd,
    Qemu {
        volume_id: String,
        node_id: String,
        /// Exact scsi-hd/virtio-scsi serial, ASCII [A-Za-z0-9._-]{1,36}.
        device_serial: String,
        readonly: bool,
        #[serde(flatten)]
        shape: VolumeShape,
        format_policy: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExpandVolume {
    pub volume_id: String,
    pub required_bytes: i64,
    pub limit_bytes: Option<i64>,
    pub expected_shape: Option<VolumeShape>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EnsureSnapshot {
    pub name: String,
    pub source_volume_id: String,
    pub parameters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub snapshot_id: String,
    pub source_volume_id: String,
    pub size_bytes: i64,
    pub creation_time_seconds: i64,
    pub creation_time_nanos: i32,
    pub ready_to_use: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotId {
    pub snapshot_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListSnapshots {
    pub snapshot_id: Option<String>,
    pub source_volume_id: Option<String>,
    pub starting_token: Option<String>,
    pub max_entries: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotPage {
    pub entries: Vec<SnapshotRecord>,
    pub next_token: Option<String>,
}

#[derive(Deserialize)]
struct Capabilities {
    api_version: String,
    format_policy: String,
    features: Vec<String>,
}

impl AxoClient {
    pub fn new(config: ControllerConfig) -> Result<Self, Status> {
        if config.timeout.is_zero() || config.timeout > Duration::from_secs(600) {
            return Err(Status::invalid_argument(
                "Axo timeout must be between 1ns and 600s",
            ));
        }
        for value in [&config.organisation_id, &config.cluster_id] {
            if value.is_empty()
                || value.len() > 256
                || value == "."
                || value == ".."
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                return Err(Status::invalid_argument(
                    "invalid organisation or cluster identifier",
                ));
            }
        }
        if !config.token_file.is_absolute() {
            return Err(Status::invalid_argument(
                "token_file must be an absolute path",
            ));
        }
        let mut root =
            Url::parse(&config.axo_url).map_err(|_| Status::invalid_argument("invalid Axo URL"))?;
        if !(root.scheme() == "https" || (config.allow_http && root.scheme() == "http")) {
            return Err(Status::invalid_argument(
                "Axo requires HTTPS; HTTP requires explicit development opt-in",
            ));
        }
        if root.host_str().is_none()
            || !root.username().is_empty()
            || root.password().is_some()
            || root.query().is_some()
            || root.fragment().is_some()
            || root.path() != "/"
        {
            return Err(Status::invalid_argument(
                "Axo URL must be an origin without credentials, path, query or fragment",
            ));
        }
        root.set_path(&format!(
            "/v4/kubernetes/organisations/{}/clusters/{}/csi/",
            config.organisation_id, config.cluster_id
        ));
        let client = reqwest::Client::builder()
            .https_only(!config.allow_http)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.timeout.min(Duration::from_secs(10)))
            .timeout(config.timeout)
            .build()
            .map_err(|_| Status::internal("could not initialize Axo HTTP client"))?;
        Ok(Self {
            client,
            root,
            token_file: config.token_file,
            timeout: config.timeout,
        })
    }

    pub async fn ready(&self) -> Result<(), Status> {
        let caps: Capabilities = self
            .json(Method::GET, "capabilities", &(), None, None)
            .await?;
        if caps.api_version != API_VERSION
            || caps.format_policy != "preformatted"
            || REQUIRED_FEATURES
                .iter()
                .any(|feature| !caps.features.iter().any(|f| f == feature))
        {
            return Err(Status::failed_precondition(
                "Axo does not implement the required CSI contract",
            ));
        }
        Ok(())
    }

    /// A mutation is sent once. On an ambiguous timeout the CO retries its CSI
    /// operation; Axo reconciles durable resource state using this same key.
    pub async fn json<B: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
        identity: Option<&[&str]>,
        deadline: Option<Duration>,
    ) -> Result<R, Status> {
        let bytes = self.send(method, path, body, identity, deadline).await?;
        serde_json::from_slice(&bytes).map_err(|_| Status::data_loss("invalid response from Axo"))
    }

    pub async fn delete<B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
        identity: &[&str],
        deadline: Option<Duration>,
    ) -> Result<(), Status> {
        match self
            .send(Method::POST, path, body, Some(identity), deadline)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if error.code() == Code::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn send<B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: &B,
        identity: Option<&[&str]>,
        deadline: Option<Duration>,
    ) -> Result<Vec<u8>, Status> {
        let timeout = deadline.unwrap_or(self.timeout).min(self.timeout);
        if timeout.is_zero() {
            return Err(Status::deadline_exceeded("CSI request deadline expired"));
        }
        tokio::time::timeout(timeout, async {
            // Read on every request: rotating an atomically replaced token file
            // does not require restarting the controller. Never display its contents.
            let metadata = tokio::fs::metadata(&self.token_file)
                .await
                .map_err(|_| Status::unauthenticated("cannot read Axo token file"))?;
            if !metadata.is_file() || metadata.len() > MAX_TOKEN_BYTES {
                return Err(Status::unauthenticated("invalid Axo token file"));
            }
            let token = tokio::fs::read_to_string(&self.token_file)
                .await
                .map_err(|_| Status::unauthenticated("cannot read Axo token file"))?;
            let token = token.trim();
            if token.is_empty()
                || token.len() as u64 > MAX_TOKEN_BYTES
                || token
                    .bytes()
                    .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
            {
                return Err(Status::unauthenticated("invalid Axo token file"));
            }
            let mut authorization = header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| Status::unauthenticated("invalid Axo token file"))?;
            authorization.set_sensitive(true);
            let url = self
                .root
                .join(path)
                .map_err(|_| Status::internal("invalid internal API route"))?;
            let mut request = self
                .client
                .request(method.clone(), url)
                .header(header::AUTHORIZATION, authorization)
                .header(header::ACCEPT, "application/json");
            if method != Method::GET {
                request = request.json(body);
            }
            if let Some(identity) = identity {
                request = request.header(
                    "Idempotency-Key",
                    idempotency_key(self.root.as_str(), path, identity),
                );
            }
            let mut response = request.send().await.map_err(http_error)?;
            let status = response.status();
            if response
                .content_length()
                .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
            {
                return Err(Status::data_loss("Axo response exceeds limit"));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(http_error)? {
                if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(Status::data_loss("Axo response exceeds limit"));
                }
                bytes.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                return Err(api_error(status, &bytes));
            }
            // Asynchronous acceptance is not success: CSI must not report an
            // attachment ready before Axo observes its completed postconditions.
            if status == StatusCode::ACCEPTED {
                return Err(Status::aborted("Axo operation is still in progress"));
            }
            Ok(bytes)
        })
        .await
        .map_err(|_| Status::deadline_exceeded("Axo request deadline exceeded"))?
    }
}

fn idempotency_key(scope: &str, operation: &str, identity: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in [scope, operation]
        .into_iter()
        .chain(identity.iter().copied())
    {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("csi-{:x}", digest.finalize())
}

fn http_error(error: reqwest::Error) -> Status {
    if error.is_timeout() {
        Status::deadline_exceeded("Axo request deadline exceeded")
    } else {
        Status::unavailable("Axo transport unavailable")
    }
}

fn api_error(status: StatusCode, bytes: &[u8]) -> Status {
    // Server messages may include upstream credentials. Only fixed messages and
    // a small, documented machine-code allowlist may leave this boundary.
    let value: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();
    let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("");
    match status {
        StatusCode::UNAUTHORIZED => Status::unauthenticated("Axo authentication failed"),
        StatusCode::FORBIDDEN => {
            Status::permission_denied("Axo operation not permitted for this cluster")
        }
        StatusCode::NOT_FOUND if code == "clever.csi.resource-not-found" => {
            Status::not_found("Axo resource not found")
        }
        // A missing route must never be mistaken for successful idempotent
        // deletion on an Axo deployment that has not implemented this contract.
        StatusCode::NOT_FOUND => Status::unimplemented("Axo CSI endpoint is unavailable"),
        StatusCode::CONFLICT => match code {
            "clever.csi.incompatible-resource" => {
                Status::already_exists("resource exists with incompatible attributes")
            }
            "clever.csi.operation-in-progress" => {
                Status::aborted("Axo operation is already in progress")
            }
            _ => Status::failed_precondition("Axo resource is in use or cannot be fenced"),
        },
        StatusCode::BAD_REQUEST => match code {
            "clever.csi.invalid-pagination-token" => {
                Status::aborted("invalid snapshot pagination token")
            }
            _ => Status::invalid_argument("Axo rejected request parameters"),
        },
        StatusCode::PRECONDITION_FAILED => {
            Status::failed_precondition("Axo resource precondition failed")
        }
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => {
            Status::deadline_exceeded("Axo request deadline exceeded")
        }
        StatusCode::UNPROCESSABLE_ENTITY => {
            Status::out_of_range("requested capacity is outside backend limits")
        }
        StatusCode::TOO_MANY_REQUESTS | StatusCode::INSUFFICIENT_STORAGE => {
            Status::resource_exhausted("Axo capacity or request limit reached")
        }
        StatusCode::NOT_IMPLEMENTED => {
            Status::unimplemented("Axo does not implement this operation")
        }
        status if status.is_server_error() => Status::unavailable("Axo backend unavailable"),
        status if status.is_redirection() => {
            Status::failed_precondition("Axo redirects are not allowed")
        }
        _ => Status::internal("unexpected Axo response status"),
    }
}
