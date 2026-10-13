//! Two deployment roles from the same image. Axo credentials exist only in controller mode.
use clap::{Parser, ValueEnum};
use clever_csi_driver::{
    controller::{ControllerConfig, CsiController},
    csi::{
        controller_server::ControllerServer, identity_server::IdentityServer,
        node_server::NodeServer,
    },
    identity::IdentityService,
    node::{NodeConfig, NodeService},
    server::{bind_socket, endpoint_path},
};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    Controller,
    Node,
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Clever Cloud CSI driver (Axo controller or Linux node plugin)"
)]
struct Args {
    #[arg(long, env = "CSI_MODE")]
    mode: Mode,
    #[arg(long, env = "CSI_ENDPOINT", default_value = "unix:///csi/csi.sock")]
    endpoint: String,
    #[arg(long, env = "CSI_DRIVER_NAME", default_value = "rbd.csi.ceph.com")]
    driver_name: String,
    #[arg(long, env = "CSI_HEALTH_ADDRESS", default_value = "0.0.0.0:9808")]
    health_address: SocketAddr,
    #[arg(long, env = "CSI_AXO_URL")]
    axo_url: Option<String>,
    #[arg(long, env = "CSI_ORGANISATION_ID")]
    organisation_id: Option<String>,
    #[arg(long, env = "CSI_CLUSTER_ID")]
    cluster_id: Option<String>,
    #[arg(long, env = "CSI_AXO_TOKEN_FILE")]
    axo_token_file: Option<PathBuf>,
    /// Test environments only. TLS certificate verification is never disabled.
    #[arg(long, env = "CSI_ALLOW_INSECURE_HTTP", default_value_t = false)]
    allow_insecure_http: bool,
    #[arg(long, env = "CSI_AXO_TIMEOUT_SECONDS", default_value_t = 120)]
    axo_timeout_seconds: u64,
    /// Platform VM incarnation identifier, not a Kubernetes Node name or IP.
    #[arg(long, env = "CSI_NODE_ID", conflicts_with = "node_id_file")]
    node_id: Option<String>,
    #[arg(long, env = "CSI_NODE_ID_FILE", conflicts_with = "node_id")]
    node_id_file: Option<PathBuf>,
    #[arg(
        long,
        env = "CSI_NODE_STATE_DIR",
        default_value = "/var/lib/clever-csi"
    )]
    node_state_dir: PathBuf,
    #[arg(long, env = "CSI_DEVICE_ROOT", default_value = "/dev")]
    device_root: PathBuf,
    #[arg(long, env = "CSI_SYS_BLOCK_DIR", default_value = "/sys/block")]
    sys_block_dir: PathBuf,
    #[arg(long, env = "CSI_KUBELET_ROOT", default_value = "/var/lib/kubelet")]
    kubelet_root: PathBuf,
}

fn required<T>(value: Option<T>, flag: &str) -> Result<T, Box<dyn std::error::Error>> {
    value.ok_or_else(|| format!("--{flag} is required in controller mode").into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let readiness = Arc::new(AtomicBool::new(false));
    let identity = IdentityService::new(
        args.driver_name.clone(),
        args.mode == Mode::Controller,
        readiness.clone(),
    )?;
    let (controller, node) = match args.mode {
        Mode::Controller => {
            if args.axo_timeout_seconds == 0 || args.axo_timeout_seconds > 600 {
                return Err("--axo-timeout-seconds must be between 1 and 600".into());
            }
            let controller = CsiController::new(ControllerConfig {
                axo_url: required(args.axo_url, "axo-url")?,
                organisation_id: required(args.organisation_id, "organisation-id")?,
                cluster_id: required(args.cluster_id, "cluster-id")?,
                token_file: required(args.axo_token_file, "axo-token-file")?,
                allow_http: args.allow_insecure_http,
                timeout: Duration::from_secs(args.axo_timeout_seconds),
            })?;
            (Some(controller), None)
        }
        Mode::Node => {
            let node_id = match (args.node_id, args.node_id_file) {
                (Some(id), None) => id,
                (None, path) => std::fs::read_to_string(
                    path.unwrap_or_else(|| PathBuf::from("/etc/clever-csi/node-id")),
                )?
                .trim()
                .to_string(),
                _ => return Err("use either --node-id or --node-id-file".into()),
            };
            let node = NodeService::new(NodeConfig {
                node_id,
                state_dir: args.node_state_dir,
                device_root: args.device_root,
                sys_block_dir: args.sys_block_dir,
                kubelet_root: args.kubelet_root,
            })?;
            readiness.store(true, Ordering::Release);
            (None, Some(node))
        }
    };
    let path = endpoint_path(&args.endpoint)?;
    let (listener, _socket_guard) = bind_socket(&path)?;
    let http_listener = tokio::net::TcpListener::bind(args.health_address).await?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let stopping_readiness = readiness.clone();
    let signal = tokio::spawn(async move {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
        let _ = shutdown_tx.send(true);
        stopping_readiness.store(false, Ordering::Release);
    });
    let checker = controller.clone().map(|backend| {
        let ready = readiness.clone();
        let mut stop = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                let result = tokio::time::timeout(Duration::from_secs(5), backend.ready()).await;
                if *stop.borrow() {
                    ready.store(false, Ordering::Release);
                    break;
                }
                ready.store(matches!(result, Ok(Ok(()))), Ordering::Release);
                tokio::select! { _ = tokio::time::sleep(Duration::from_secs(5)) => {}, _ = stop.changed() => break }
            }
        })
    });
    let router = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok\n" }))
        .route(
            "/readyz",
            axum::routing::get({
                let ready = readiness.clone();
                move || {
                    let ready = ready.clone();
                    async move {
                        if ready.load(Ordering::Acquire) {
                            (axum::http::StatusCode::OK, "ready\n")
                        } else {
                            (
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                "initializing\n",
                            )
                        }
                    }
                }
            }),
        );
    let mut grpc_stop = shutdown_rx.clone();
    let mut http_stop = shutdown_rx;
    tracing::info!(role = ?args.mode, driver = %args.driver_name, endpoint = %path.display(), "CSI service starting");
    let grpc = Server::builder()
        .add_service(IdentityServer::new(identity))
        .add_optional_service(controller.map(ControllerServer::new))
        .add_optional_service(node.map(NodeServer::new))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async move {
            let _ = grpc_stop.changed().await;
        });
    let http = axum::serve(http_listener, router).with_graceful_shutdown(async move {
        let _ = http_stop.changed().await;
    });
    // Drain both servers: completion of HTTP shutdown must not cancel an
    // in-flight gRPC storage operation.
    let result = tokio::try_join!(
        async { grpc.await.map_err(Box::<dyn std::error::Error>::from) },
        async { http.await.map_err(Box::<dyn std::error::Error>::from) },
    );
    readiness.store(false, Ordering::Release);
    if let Some(checker) = checker {
        checker.abort();
    }
    signal.abort();
    result?;
    Ok(())
}
