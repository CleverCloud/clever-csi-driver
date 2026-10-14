//! CSI identity and initialization status. Neither endpoint exposes credentials.
use crate::csi::{self, identity_server::Identity};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub struct IdentityService {
    name: String,
    controller: bool,
    ready: Arc<AtomicBool>,
}

impl IdentityService {
    pub fn new(name: String, controller: bool, ready: Arc<AtomicBool>) -> Result<Self, String> {
        if name.is_empty()
            || name.len() > 63
            || !name.as_bytes()[0].is_ascii_alphanumeric()
            || !name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
        {
            return Err("driver name must be a CSI domain-style name of 1..63 characters".into());
        }
        Ok(Self {
            name,
            controller,
            ready,
        })
    }
}

#[tonic::async_trait]
impl Identity for IdentityService {
    async fn get_plugin_info(
        &self,
        _: Request<csi::GetPluginInfoRequest>,
    ) -> Result<Response<csi::GetPluginInfoResponse>, Status> {
        Ok(Response::new(csi::GetPluginInfoResponse {
            name: self.name.clone(),
            vendor_version: env!("CARGO_PKG_VERSION").into(),
            manifest: Default::default(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _: Request<csi::GetPluginCapabilitiesRequest>,
    ) -> Result<Response<csi::GetPluginCapabilitiesResponse>, Status> {
        use csi::plugin_capability::{Service, Type, VolumeExpansion, service, volume_expansion};
        let mut capabilities = vec![csi::PluginCapability {
            r#type: Some(Type::VolumeExpansion(VolumeExpansion {
                r#type: volume_expansion::Type::Online as i32,
            })),
        }];
        if self.controller {
            capabilities.push(csi::PluginCapability {
                r#type: Some(Type::Service(Service {
                    r#type: service::Type::ControllerService as i32,
                })),
            });
        }
        Ok(Response::new(csi::GetPluginCapabilitiesResponse {
            capabilities,
        }))
    }

    async fn probe(
        &self,
        _: Request<csi::ProbeRequest>,
    ) -> Result<Response<csi::ProbeResponse>, Status> {
        Ok(Response::new(csi::ProbeResponse {
            ready: Some(self.ready.load(Ordering::Acquire)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn probe_distinguishes_startup_from_readiness() {
        let ready = Arc::new(AtomicBool::new(false));
        let svc = IdentityService::new("rbd.csi.ceph.com".into(), true, ready.clone()).unwrap();
        assert_eq!(
            svc.probe(Request::new(csi::ProbeRequest {}))
                .await
                .unwrap()
                .into_inner()
                .ready,
            Some(false)
        );
        ready.store(true, Ordering::Release);
        assert_eq!(
            svc.probe(Request::new(csi::ProbeRequest {}))
                .await
                .unwrap()
                .into_inner()
                .ready,
            Some(true)
        );
        assert_eq!(
            svc.get_plugin_info(Request::new(csi::GetPluginInfoRequest {}))
                .await
                .unwrap()
                .into_inner()
                .name,
            "rbd.csi.ceph.com"
        );
    }
    #[test]
    fn rejects_invalid_driver_names() {
        for name in ["", "-driver", "bad/name", "a b", "trailing."] {
            assert!(
                IdentityService::new(name.into(), false, Arc::new(AtomicBool::new(false))).is_err()
            );
        }
    }
}
