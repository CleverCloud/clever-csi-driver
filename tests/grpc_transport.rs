//! Exercise the actual CSI protobuf and Unix gRPC transport, without privileged mounts.
use clever_csi_driver::{
    csi::{
        self, identity_client::IdentityClient, identity_server::IdentityServer,
        node_client::NodeClient, node_server::NodeServer,
    },
    identity::IdentityService,
    node::{NodeConfig, NodeService},
    server::bind_socket,
};
use hyper_util::rt::TokioIo;
use std::sync::{Arc, atomic::AtomicBool};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Endpoint, Server};
use tower::service_fn;

#[tokio::test]
async fn unix_grpc_exposes_identity_vm_incarnation_and_safe_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("csi.sock");
    let (listener, _guard) = bind_socket(&path).unwrap();
    let identity = IdentityService::new(
        "rbd.csi.ceph.com".into(),
        false,
        Arc::new(AtomicBool::new(true)),
    )
    .unwrap();
    let node = NodeService::new(NodeConfig {
        node_id: "vm-incarnation-from-platform".into(),
        state_dir: dir.path().join("state"),
        device_root: "/dev".into(),
        sys_block_dir: "/sys/block".into(),
        kubelet_root: dir.path().join("kubelet"),
    })
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(
        Server::builder()
            .add_service(IdentityServer::new(identity))
            .add_service(NodeServer::new(node))
            .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async {
                let _ = stopped.await;
            }),
    );
    let channel = Endpoint::try_from("http://localhost")
        .unwrap()
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                tokio::net::UnixStream::connect(path)
                    .await
                    .map(TokioIo::new)
            }
        }))
        .await
        .unwrap();
    let mut identity = IdentityClient::new(channel.clone());
    assert_eq!(
        identity
            .get_plugin_info(csi::GetPluginInfoRequest {})
            .await
            .unwrap()
            .into_inner()
            .name,
        "rbd.csi.ceph.com"
    );
    assert_eq!(
        identity
            .probe(csi::ProbeRequest {})
            .await
            .unwrap()
            .into_inner()
            .ready,
        Some(true)
    );
    let mut node = NodeClient::new(channel);
    assert_eq!(
        node.node_get_info(csi::NodeGetInfoRequest {})
            .await
            .unwrap()
            .into_inner()
            .node_id,
        "vm-incarnation-from-platform"
    );
    let error = node
        .node_unpublish_volume(csi::NodeUnpublishVolumeRequest {
            volume_id: "volume".into(),
            target_path: "/etc".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    let error = node
        .node_stage_volume(csi::NodeStageVolumeRequest::default())
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    drop(node);
    drop(identity);
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
