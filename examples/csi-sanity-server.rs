//! Combined endpoint for the isolated csi-sanity fixture, never the deployed binary.
use clap::Parser;
use clever_csi_driver::{
    axo::ControllerConfig,
    controller::CsiController,
    csi::{
        controller_server::ControllerServer, identity_server::IdentityServer,
        node_server::NodeServer,
    },
    identity::IdentityService,
    node::{NodeConfig, NodeService},
    server::{bind_socket, endpoint_path},
};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "rbd.csi.ceph.com")]
    driver_name: String,
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    axo_url: String,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long)]
    device_root: PathBuf,
    #[arg(long)]
    sys_block_dir: PathBuf,
    #[arg(long)]
    kubelet_root: PathBuf,
    #[arg(long, default_value = "sanity-vm")]
    node_id: String,
    #[arg(long, default_value = "orga-test")]
    organisation_id: String,
    #[arg(long, default_value = "kubernetes-test")]
    cluster_id: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if std::env::var("CLEVER_CSI_PRIVILEGED_TESTS").as_deref() != Ok("1") {
        return Err("run this fixture only in the disposable privileged test container".into());
    }
    let controller = CsiController::new(ControllerConfig {
        axo_url: args.axo_url,
        organisation_id: args.organisation_id,
        cluster_id: args.cluster_id,
        token_file: args.token_file,
        allow_http: true,
        timeout: Duration::from_secs(60),
    })?;
    controller.ready().await?;
    let node = NodeService::new(NodeConfig {
        node_id: args.node_id,
        state_dir: args.state_dir,
        device_root: args.device_root,
        sys_block_dir: args.sys_block_dir,
        kubelet_root: args.kubelet_root,
    })?;
    let identity = IdentityService::new(args.driver_name, true, Arc::new(AtomicBool::new(true)))?;
    let (listener, _guard) = bind_socket(&endpoint_path(&args.endpoint)?)?;
    Server::builder()
        .add_service(IdentityServer::new(identity))
        .add_service(ControllerServer::new(controller))
        .add_service(NodeServer::new(node.clone()))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
        })
        .await?;
    node.drain().await;
    Ok(())
}
