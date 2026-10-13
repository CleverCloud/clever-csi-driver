//! Contract tests against a local HTTP Axo double; no Ceph, VM or cluster mutation.
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request as HttpRequest, State},
    http::StatusCode,
};
use clever_csi_driver::{
    axo::REQUIRED_FEATURES,
    controller::{ControllerConfig, CsiController},
    csi,
};
use csi::controller_server::Controller;
use serde_json::{Value, json};
use tonic::{Code, Request};

#[derive(Clone, Debug)]
struct Call {
    path: String,
    authorization: String,
    key: Option<String>,
    body: Value,
}
struct Reply {
    status: StatusCode,
    body: Value,
    delay: Duration,
}
#[derive(Default)]
struct MockState {
    calls: Vec<Call>,
    replies: VecDeque<Reply>,
}

struct Fixture {
    controller: CsiController,
    state: Arc<Mutex<MockState>>,
    task: tokio::task::JoinHandle<()>,
    temp: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixture {
    async fn new() -> Self {
        let state = Arc::new(Mutex::new(MockState::default()));
        let router = Router::new().fallback(mock).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let token_file = temp.path().join("token");
        std::fs::write(&token_file, "biscuit-first\n").unwrap();
        let controller = CsiController::new(ControllerConfig {
            axo_url: format!("http://{addr}"),
            organisation_id: "orga-test".into(),
            cluster_id: "kubernetes-test".into(),
            token_file,
            allow_http: true,
            timeout: Duration::from_secs(2),
        })
        .unwrap();
        Self {
            controller,
            state,
            task,
            temp,
        }
    }
    fn reply(&self, status: StatusCode, body: Value) {
        self.state.lock().unwrap().replies.push_back(Reply {
            status,
            body,
            delay: Duration::ZERO,
        });
    }
    fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }
}

async fn mock(
    State(state): State<Arc<Mutex<MockState>>>,
    request: HttpRequest,
) -> axum::response::Response {
    let path = request.uri().path().to_owned();
    let authorization = request
        .headers()
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let key = request
        .headers()
        .get("idempotency-key")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    let reply = {
        let mut state = state.lock().unwrap();
        state.calls.push(Call {
            path,
            authorization,
            key,
            body,
        });
        state.replies.pop_front().unwrap_or(Reply {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: json!({}),
            delay: Duration::ZERO,
        })
    };
    tokio::time::sleep(reply.delay).await;
    axum::response::Response::builder()
        .status(reply.status)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&reply.body).unwrap()))
        .unwrap()
}

fn cap(fs: Option<&str>) -> csi::VolumeCapability {
    csi::VolumeCapability {
        access_mode: Some(csi::volume_capability::AccessMode {
            mode: csi::volume_capability::access_mode::Mode::SingleNodeWriter as i32,
        }),
        access_type: Some(match fs {
            Some(fs) => {
                csi::volume_capability::AccessType::Mount(csi::volume_capability::MountVolume {
                    fs_type: fs.into(),
                    ..Default::default()
                })
            }
            None => {
                csi::volume_capability::AccessType::Block(csi::volume_capability::BlockVolume {})
            }
        }),
    }
}

fn create() -> csi::CreateVolumeRequest {
    csi::CreateVolumeRequest {
        name: "pvc-same-name".into(),
        volume_capabilities: vec![cap(Some("xfs"))],
        capacity_range: Some(csi::CapacityRange {
            required_bytes: 4096,
            limit_bytes: 8192,
        }),
        ..Default::default()
    }
}

fn volume_record() -> Value {
    json!({"volume_id":"opaque/legacy:handle", "capacity_bytes":4096,"volume_mode":"filesystem", "fs_type":"xfs", "format_policy":"preformatted", "source":null})
}

fn snapshot_record() -> Value {
    json!({"snapshot_id":"snap/opaque:id", "source_volume_id":"opaque/legacy:handle", "size_bytes":4096,
        "creation_time_seconds":1700000000, "creation_time_nanos":42, "ready_to_use":true})
}

fn publish() -> csi::ControllerPublishVolumeRequest {
    csi::ControllerPublishVolumeRequest {
        volume_id: "0001-not-decoded/../../opaque".into(),
        node_id: "vm-incarnation-0123".into(),
        volume_capability: Some(cap(Some("xfs"))),
        volume_context: HashMap::from([("clusterID".into(), "legacy-ceph-id".into())]),
        ..Default::default()
    }
}

#[tokio::test]
async fn create_retries_keep_identity_and_rotate_token_without_forwarding_csi_secrets() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK, volume_record());
    fixture.reply(StatusCode::OK, volume_record());
    let mut request = create();
    request
        .secrets
        .insert("userKey".into(), "must-never-reach-Axo".into());
    request.parameters.insert("pool".into(), "rbd".into());
    let first = fixture
        .controller
        .create_volume(Request::new(request.clone()))
        .await
        .unwrap()
        .into_inner();
    std::fs::write(fixture.temp.path().join("token"), "biscuit-rotated").unwrap();
    let second = fixture
        .controller
        .create_volume(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first, second);
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].path,
        "/v4/kubernetes/organisations/orga-test/clusters/kubernetes-test/csi/volumes"
    );
    assert_eq!(calls[0].key, calls[1].key);
    assert!(calls[0].key.as_ref().unwrap().starts_with("csi-"));
    assert_eq!(calls[0].authorization, "Bearer biscuit-first");
    assert_eq!(calls[1].authorization, "Bearer biscuit-rotated");
    assert!(!calls[0].body.to_string().contains("must-never"));
    assert_eq!(
        first.volume.unwrap().volume_context["format_policy"],
        "preformatted"
    );
}

#[tokio::test]
async fn clone_and_restore_preserve_opaque_source_and_idempotent_create_name() {
    for source in [
        csi::volume_content_source::Type::Volume(csi::volume_content_source::VolumeSource {
            volume_id: "opaque/source:volume".into(),
        }),
        csi::volume_content_source::Type::Snapshot(csi::volume_content_source::SnapshotSource {
            snapshot_id: "opaque/source:snapshot".into(),
        }),
    ] {
        let fixture = Fixture::new().await;
        let source_json = match &source {
            csi::volume_content_source::Type::Volume(s) => {
                json!({"type":"volume", "id":s.volume_id})
            }
            csi::volume_content_source::Type::Snapshot(s) => {
                json!({"type":"snapshot", "id":s.snapshot_id})
            }
        };
        let mut record = volume_record();
        record["source"] = source_json.clone();
        fixture.reply(StatusCode::OK, record.clone());
        fixture.reply(StatusCode::OK, record);
        let mut request = create();
        request.volume_content_source = Some(csi::VolumeContentSource {
            r#type: Some(source),
        });
        for _ in 0..2 {
            let response = fixture
                .controller
                .create_volume(Request::new(request.clone()))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(
                response.volume.unwrap().content_source,
                request.volume_content_source
            );
        }
        let calls = fixture.calls();
        assert_eq!(calls[0].body["source"], source_json);
        assert_eq!(calls[0].key, calls[1].key);
    }
}

#[tokio::test]
async fn attach_requires_completed_preformatted_identity_and_preserves_opaque_handles() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK, json!({"transport":"qemu", "volume_id":"0001-not-decoded/../../opaque", "node_id":"vm-incarnation-0123", "readonly":false, "device_serial":"csi-serial-42", "volume_mode":"filesystem", "fs_type":"xfs", "format_policy":"preformatted"}));
    let response = fixture
        .controller
        .controller_publish_volume(Request::new(publish()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        response.publish_context,
        HashMap::from([
            ("device_serial".into(), "csi-serial-42".into()),
            ("volume_mode".into(), "filesystem".into()),
            ("fs_type".into(), "xfs".into()),
            ("format_policy".into(), "preformatted".into()),
            ("readonly".into(), "false".into()),
        ])
    );
    let call = &fixture.calls()[0];
    assert_eq!(call.body["volume_id"], "0001-not-decoded/../../opaque");
    assert_eq!(call.body["node_id"], "vm-incarnation-0123");
    assert!(!call.path.contains("opaque"));
    assert_eq!(call.body["volume_context"]["clusterID"], "legacy-ceph-id");
}

#[tokio::test]
async fn historical_node_names_only_use_the_authoritative_legacy_response() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK, json!({"transport":"legacy-krbd"}));
    let mut request = publish();
    request.node_id = "manual-nodegroup-node0".into();
    let response = fixture
        .controller
        .controller_publish_volume(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert!(response.publish_context.is_empty());
    assert_eq!(fixture.calls()[0].body["node_id"], "manual-nodegroup-node0");
    assert_eq!(fixture.calls().len(), 1); // no second local attach path or name->VM resolution
}

#[tokio::test]
async fn block_attachment_omits_filesystem_metadata() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK, json!({"transport":"qemu", "volume_id":"0001-not-decoded/../../opaque", "node_id":"vm-incarnation-0123", "readonly":false, "device_serial":"block-serial", "volume_mode":"block", "format_policy":"preformatted"}));
    let mut request = publish();
    request.volume_capability = Some(cap(None));
    let response = fixture
        .controller
        .controller_publish_volume(Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.publish_context["volume_mode"], "block");
    assert!(!response.publish_context.contains_key("fs_type"));
}

#[tokio::test]
async fn mismatched_filesystem_or_unformatted_attach_is_not_published() {
    for (fs, policy) in [("ext4", "preformatted"), ("xfs", "blank")] {
        let fixture = Fixture::new().await;
        fixture.reply(StatusCode::OK, json!({"transport":"qemu", "volume_id":"0001-not-decoded/../../opaque", "node_id":"vm-incarnation-0123", "readonly":false, "device_serial":"serial", "volume_mode":"filesystem", "fs_type":fs, "format_policy":policy}));
        assert_eq!(
            fixture
                .controller
                .controller_publish_volume(Request::new(publish()))
                .await
                .unwrap_err()
                .code(),
            Code::DataLoss
        );
    }
}

#[tokio::test]
async fn stale_attachment_response_for_another_incarnation_is_rejected() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK, json!({
        "transport":"qemu", "volume_id":"0001-not-decoded/../../opaque", "node_id":"old-destroyed-incarnation",
        "readonly":false, "device_serial":"serial", "volume_mode":"filesystem", "fs_type":"xfs", "format_policy":"preformatted"
    }));
    assert_eq!(
        fixture
            .controller
            .controller_publish_volume(Request::new(publish()))
            .await
            .unwrap_err()
            .code(),
        Code::DataLoss
    );
}

#[tokio::test]
async fn deleted_resources_and_detached_incarnations_are_idempotent() {
    let fixture = Fixture::new().await;
    for _ in 0..3 {
        fixture.reply(
            StatusCode::NOT_FOUND,
            json!({"code":"clever.csi.resource-not-found"}),
        );
    }
    fixture
        .controller
        .delete_volume(Request::new(csi::DeleteVolumeRequest {
            volume_id: "opaque".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    fixture
        .controller
        .controller_unpublish_volume(Request::new(csi::ControllerUnpublishVolumeRequest {
            volume_id: "opaque".into(),
            node_id: String::new(),
            ..Default::default()
        }))
        .await
        .unwrap();
    fixture
        .controller
        .delete_snapshot(Request::new(csi::DeleteSnapshotRequest {
            snapshot_id: "opaque-snapshot".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    assert_eq!(fixture.calls()[1].body["node_id"], Value::Null);
    assert!(fixture.calls().iter().all(|c| c.key.is_some()));
}

#[tokio::test]
async fn status_mapping_never_forwards_upstream_secrets_or_retries_mutations() {
    for (http, code, grpc) in [
        (StatusCode::FORBIDDEN, "irrelevant", Code::PermissionDenied),
        (
            StatusCode::UNAUTHORIZED,
            "irrelevant",
            Code::Unauthenticated,
        ),
        (
            StatusCode::CONFLICT,
            "clever.csi.incompatible-resource",
            Code::AlreadyExists,
        ),
        (
            StatusCode::CONFLICT,
            "clever.csi.operation-in-progress",
            Code::Aborted,
        ),
        (
            StatusCode::CONFLICT,
            "clever.csi.fencing-required",
            Code::FailedPrecondition,
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "irrelevant",
            Code::Unavailable,
        ),
        (
            StatusCode::TOO_MANY_REQUESTS,
            "irrelevant",
            Code::ResourceExhausted,
        ),
        (StatusCode::ACCEPTED, "irrelevant", Code::Aborted),
    ] {
        let fixture = Fixture::new().await;
        fixture.reply(
            http,
            json!({"code":code,"message":"secret-supernova-token"}),
        );
        let error = fixture
            .controller
            .create_volume(Request::new(create()))
            .await
            .unwrap_err();
        assert_eq!(error.code(), grpc);
        assert!(!error.message().contains("secret"));
        assert_eq!(fixture.calls().len(), 1);
    }
}

#[tokio::test]
async fn call_deadline_bounds_http_work_without_blind_retry() {
    let fixture = Fixture::new().await;
    fixture.state.lock().unwrap().replies.push_back(Reply {
        status: StatusCode::OK,
        body: volume_record(),
        delay: Duration::from_secs(1),
    });
    let mut request = Request::new(create());
    request.set_timeout(Duration::from_millis(30));
    let start = std::time::Instant::now();
    let error = fixture.controller.create_volume(request).await.unwrap_err();
    assert_eq!(error.code(), Code::DeadlineExceeded);
    assert!(start.elapsed() < Duration::from_millis(500));
    assert_eq!(fixture.calls().len(), 1);
}

#[tokio::test]
async fn unsupported_capabilities_and_encryption_fail_before_http() {
    let fixture = Fixture::new().await;
    let mut unsupported = create();
    unsupported.volume_capabilities[0]
        .access_mode
        .as_mut()
        .unwrap()
        .mode = 5; // MULTI_NODE_MULTI_WRITER
    assert_eq!(
        fixture
            .controller
            .create_volume(Request::new(unsupported))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let mut encrypted = create();
    encrypted
        .parameters
        .insert("encrypted".into(), "true".into());
    assert_eq!(
        fixture
            .controller
            .create_volume(Request::new(encrypted))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let mut flags = create();
    if let Some(csi::volume_capability::AccessType::Mount(mount)) =
        &mut flags.volume_capabilities[0].access_type
    {
        mount.mount_flags.push("bind".into());
    }
    assert_eq!(
        fixture
            .controller
            .create_volume(Request::new(flags))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn volume_resolution_validates_adopted_metadata_without_rewriting_handle() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK, volume_record());
    let response = fixture
        .controller
        .validate_volume_capabilities(Request::new(csi::ValidateVolumeCapabilitiesRequest {
            volume_id: "opaque/legacy:handle".into(),
            volume_capabilities: vec![cap(Some("ext4"))],
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.confirmed.is_none());
    assert_eq!(fixture.calls()[0].body["volume_id"], "opaque/legacy:handle");
    assert!(fixture.calls()[0].key.is_none()); // lookup is not adoption/facturation
}

#[tokio::test]
async fn expansion_preserves_identity_and_requests_node_filesystem_growth() {
    let fixture = Fixture::new().await;
    let mut record = volume_record();
    record["capacity_bytes"] = json!(8192);
    fixture.reply(StatusCode::OK, record);
    let response = fixture
        .controller
        .controller_expand_volume(Request::new(csi::ControllerExpandVolumeRequest {
            volume_id: "opaque/legacy:handle".into(),
            capacity_range: Some(csi::CapacityRange {
                required_bytes: 8192,
                limit_bytes: 8192,
            }),
            volume_capability: Some(cap(Some("xfs"))),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.node_expansion_required);
    assert_eq!(response.capacity_bytes, 8192);
    assert_eq!(fixture.calls()[0].body["limit_bytes"], 8192);
}

#[tokio::test]
async fn create_and_list_snapshots_preserve_source_time_and_pagination() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK, snapshot_record());
    fixture.reply(
        StatusCode::OK,
        json!({"entries":[snapshot_record()], "next_token":"opaque-pagination-token"}),
    );
    let created = fixture
        .controller
        .create_snapshot(Request::new(csi::CreateSnapshotRequest {
            name: "snapshot-name".into(),
            source_volume_id: "opaque/legacy:handle".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .snapshot
        .unwrap();
    assert_eq!(created.creation_time.unwrap().nanos, 42);
    let page = fixture
        .controller
        .list_snapshots(Request::new(csi::ListSnapshotsRequest {
            max_entries: 1,
            source_volume_id: "opaque/legacy:handle".into(),
            starting_token: "prior-token".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.next_token, "opaque-pagination-token");
    assert_eq!(fixture.calls()[1].body["starting_token"], "prior-token");
    assert!(fixture.calls()[1].key.is_none());
}

#[tokio::test]
async fn invalid_snapshot_pages_and_tokens_fail_closed() {
    let fixture = Fixture::new().await;
    fixture.reply(
        StatusCode::OK,
        json!({"entries":[snapshot_record()],"next_token":null}),
    );
    let error = fixture
        .controller
        .list_snapshots(Request::new(csi::ListSnapshotsRequest {
            snapshot_id: "different-id".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::DataLoss);
    fixture.reply(
        StatusCode::BAD_REQUEST,
        json!({"code":"clever.csi.invalid-pagination-token"}),
    );
    let error = fixture
        .controller
        .list_snapshots(Request::new(csi::ListSnapshotsRequest {
            starting_token: "expired".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::Aborted);
}

#[tokio::test]
async fn readiness_requires_the_backend_contract_including_legacy_fencing_and_clone() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK,json!({"api_version":"clever-csi-v1", "format_policy":"preformatted", "features":REQUIRED_FEATURES}));
    fixture.controller.ready().await.unwrap();
    fixture.reply(
        StatusCode::OK,
        json!({"api_version":"clever-csi-v1", "format_policy":"preformatted", "features":[]}),
    );
    assert_eq!(
        fixture.controller.ready().await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    fixture.reply(StatusCode::NOT_FOUND, json!({}));
    assert_eq!(
        fixture.controller.ready().await.unwrap_err().code(),
        Code::Unimplemented
    );
}

#[tokio::test]
async fn missing_api_route_is_not_reported_as_successful_deletion() {
    let fixture = Fixture::new().await;
    fixture.reply(
        StatusCode::NOT_FOUND,
        json!({"code":"clever.core.not-found"}),
    );
    let error = fixture
        .controller
        .delete_volume(Request::new(csi::DeleteVolumeRequest {
            volume_id: "opaque/real-resource".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::Unimplemented);
}

#[tokio::test]
async fn attach_detach_reattach_always_reconciles_current_state_even_with_the_same_key() {
    let fixture = Fixture::new().await;
    let attached = json!({"transport":"qemu", "volume_id":"0001-not-decoded/../../opaque", "node_id":"vm-incarnation-0123", "readonly":false, "device_serial":"serial", "volume_mode":"filesystem", "fs_type":"xfs", "format_policy":"preformatted"});
    fixture.reply(StatusCode::OK, attached.clone());
    fixture.reply(StatusCode::OK, json!({}));
    fixture.reply(StatusCode::OK, attached);
    let request = publish();
    fixture
        .controller
        .controller_publish_volume(Request::new(request.clone()))
        .await
        .unwrap();
    fixture
        .controller
        .controller_unpublish_volume(Request::new(csi::ControllerUnpublishVolumeRequest {
            volume_id: request.volume_id.clone(),
            node_id: request.node_id.clone(),
            ..Default::default()
        }))
        .await
        .unwrap();
    fixture
        .controller
        .controller_publish_volume(Request::new(request))
        .await
        .unwrap();
    let calls = fixture.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].key, calls[2].key);
    assert_ne!(calls[0].key, calls[1].key);
    assert!(calls[2].path.ends_with("/attachments"));
}

#[tokio::test]
async fn readonly_attachment_is_enforced_by_backend_and_publish_context() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK, json!({"transport":"qemu", "volume_id":"0001-not-decoded/../../opaque", "node_id":"vm-incarnation-0123", "readonly":true, "device_serial":"serial", "volume_mode":"filesystem", "fs_type":"xfs", "format_policy":"preformatted"}));
    let mut request = publish();
    request.readonly = true;
    request
        .volume_capability
        .as_mut()
        .unwrap()
        .access_mode
        .as_mut()
        .unwrap()
        .mode = 2;
    let response = fixture
        .controller
        .controller_publish_volume(Request::new(request.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.publish_context["readonly"], "true");
    assert_eq!(fixture.calls()[0].body["readonly"], true);
    request.readonly = false;
    assert_eq!(
        fixture
            .controller
            .controller_publish_volume(Request::new(request))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(fixture.calls().len(), 1);
}

#[tokio::test]
async fn raw_block_expansion_requires_guest_rescan() {
    let fixture = Fixture::new().await;
    fixture.reply(StatusCode::OK,json!({"volume_id":"block-handle", "capacity_bytes":8192,"volume_mode":"block", "format_policy":"preformatted", "source":null}));
    let response = fixture
        .controller
        .controller_expand_volume(Request::new(csi::ControllerExpandVolumeRequest {
            volume_id: "block-handle".into(),
            capacity_range: Some(csi::CapacityRange {
                required_bytes: 8192,
                limit_bytes: 8192,
            }),
            volume_capability: Some(cap(None)),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.node_expansion_required);
}

#[test]
fn tls_and_credential_configuration_fail_closed() {
    let mut config = ControllerConfig {
        axo_url: "http://example.com".into(),
        organisation_id: "orga-test".into(),
        cluster_id: "cluster".into(),
        token_file: "/absolute/token".into(),
        allow_http: false,
        timeout: Duration::from_secs(2),
    };
    assert!(CsiController::new(config.clone()).is_err());
    config.axo_url = "https://secret@example.com".into();
    assert!(CsiController::new(config.clone()).is_err());
    config.axo_url = "https://example.com".into();
    config.cluster_id = "../../outside".into();
    assert!(CsiController::new(config.clone()).is_err());
    config.cluster_id = "cluster".into();
    config.token_file = "relative".into();
    assert!(CsiController::new(config).is_err());
}

#[tokio::test]
async fn explicitly_empty_capacity_range_is_invalid_but_omitted_create_range_uses_default() {
    let fixture = Fixture::new().await;
    let mut request = create();
    request.capacity_range = Some(csi::CapacityRange::default());
    assert_eq!(
        fixture
            .controller
            .create_volume(Request::new(request.clone()))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        fixture
            .controller
            .controller_expand_volume(Request::new(csi::ControllerExpandVolumeRequest {
                volume_id: "opaque-handle".into(),
                capacity_range: Some(csi::CapacityRange::default()),
                ..Default::default()
            }))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert!(fixture.calls().is_empty());
    let mut record = volume_record();
    record["capacity_bytes"] = json!(1_i64 << 30);
    fixture.reply(StatusCode::OK, record);
    request.capacity_range = None;
    fixture
        .controller
        .create_volume(Request::new(request))
        .await
        .unwrap();
    assert_eq!(fixture.calls()[0].body["required_bytes"], 1_i64 << 30);
}

#[tokio::test]
async fn validation_distinguishes_malformed_required_fields_from_unsupported_features() {
    let fixture = Fixture::new().await;
    let mut no_mode = cap(Some("xfs"));
    no_mode.access_mode = None;
    let mut unknown_mode = cap(Some("xfs"));
    unknown_mode.access_mode.as_mut().unwrap().mode = 0;
    let mut no_type = cap(Some("xfs"));
    no_type.access_type = None;
    for malformed in [no_mode, unknown_mode, no_type] {
        let error = fixture
            .controller
            .validate_volume_capabilities(Request::new(csi::ValidateVolumeCapabilitiesRequest {
                volume_id: "opaque-handle".into(),
                volume_capabilities: vec![malformed],
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::InvalidArgument);
    }
    let response = fixture
        .controller
        .validate_volume_capabilities(Request::new(csi::ValidateVolumeCapabilitiesRequest {
            volume_id: "opaque-handle".into(),
            volume_capabilities: vec![cap(Some("xfs"))],
            mutable_parameters: HashMap::from([("iops".into(), "1000".into())]),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.confirmed.is_none());
    assert!(response.message.contains("mutable"));
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn csi_creation_names_preserve_permitted_unicode_whitespace_and_reject_forbidden_controls() {
    let fixture = Fixture::new().await;
    let name = "volume\tétude\nligne\rsuivante";
    let mut request = create();
    request.name = name.into();
    fixture.reply(StatusCode::OK, volume_record());
    fixture
        .controller
        .create_volume(Request::new(request))
        .await
        .unwrap();
    fixture.reply(StatusCode::OK, snapshot_record());
    fixture
        .controller
        .create_snapshot(Request::new(csi::CreateSnapshotRequest {
            name: name.into(),
            source_volume_id: "opaque/legacy:handle".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    assert!(fixture.calls().iter().all(|call| call.body["name"] == name));
    for name in ["bad\0name", "bad\u{000b}name", "bad\u{0085}name"] {
        let mut request = create();
        request.name = name.into();
        assert_eq!(
            fixture
                .controller
                .create_volume(Request::new(request))
                .await
                .unwrap_err()
                .code(),
            Code::InvalidArgument
        );
    }
    assert_eq!(fixture.calls().len(), 2);
}
