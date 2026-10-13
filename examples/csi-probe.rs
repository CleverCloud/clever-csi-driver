//! Read-only deployment probe: cargo run --example csi-probe -- unix:///csi/csi.sock node
use clever_csi_driver::{
    csi::{self, identity_client::IdentityClient, node_client::NodeClient},
    server::endpoint_path,
};
use hyper_util::rt::TokioIo;
use tonic::transport::Endpoint;
use tower::service_fn;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = std::env::args()
        .nth(1)
        .ok_or("usage: csi-probe unix:///path/to/csi.sock [node]")?;
    let path = endpoint_path(&endpoint)?;
    let channel = Endpoint::try_from("http://localhost")?
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(5))
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                tokio::net::UnixStream::connect(path)
                    .await
                    .map(TokioIo::new)
            }
        }))
        .await?;
    let mut identity = IdentityClient::new(channel.clone());
    let info = identity
        .get_plugin_info(csi::GetPluginInfoRequest {})
        .await?
        .into_inner();
    let ready = identity
        .probe(csi::ProbeRequest {})
        .await?
        .into_inner()
        .ready
        .unwrap_or(true);
    println!(
        "driver={} version={} ready={ready}",
        info.name, info.vendor_version
    );
    if std::env::args().nth(2).as_deref() == Some("node") {
        let info = NodeClient::new(channel)
            .node_get_info(csi::NodeGetInfoRequest {})
            .await?
            .into_inner();
        println!("node_id={}", info.node_id);
    }
    if !ready {
        return Err("CSI driver is not ready".into());
    }
    Ok(())
}
