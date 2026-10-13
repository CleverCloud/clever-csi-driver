//! CSI controller: validates Kubernetes requests and delegates storage ownership
//! and fencing to the proposed Axo API. Handles are opaque and never decoded.

use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use reqwest::Method;
use tonic::{Request, Response, Status};

pub use crate::axo::ControllerConfig;
use crate::{axo, csi};

#[derive(Clone)]
pub struct CsiController {
    axo: axo::AxoClient,
}

impl CsiController {
    pub fn new(config: ControllerConfig) -> Result<Self, Status> {
        Ok(Self {
            axo: axo::AxoClient::new(config)?,
        })
    }

    pub async fn ready(&self) -> Result<(), Status> {
        self.axo.ready().await
    }
}

#[tonic::async_trait]
impl csi::controller_server::Controller for CsiController {
    async fn create_volume(
        &self,
        request: Request<csi::CreateVolumeRequest>,
    ) -> Result<Response<csi::CreateVolumeResponse>, Status> {
        let deadline = deadline(&request)?;
        let request = request.into_inner();
        object_name(&request.name)?;
        if !request.mutable_parameters.is_empty() {
            return Err(Status::unimplemented(
                "mutable volume parameters are not supported",
            ));
        }
        if request
            .accessibility_requirements
            .as_ref()
            .is_some_and(|r| !r.requisite.is_empty() || !r.preferred.is_empty())
        {
            return Err(Status::unimplemented(
                "topology constraints are not supported",
            ));
        }
        let parameters = storage_parameters(request.parameters)?;
        let shape = capabilities(&request.volume_capabilities)?;
        validate_parameter_filesystem(&parameters, &shape)?;
        let (required_bytes, limit_bytes) = capacity(request.capacity_range.as_ref(), false)?;
        let source = request
            .volume_content_source
            .map(content_source)
            .transpose()?;
        let body = axo::EnsureVolume {
            name: request.name,
            required_bytes,
            limit_bytes,
            shape: shape.clone(),
            parameters,
            source: source.clone(),
        };
        let record: axo::VolumeRecord = self
            .axo
            .json(
                Method::POST,
                "volumes",
                &body,
                Some(&[&body.name]),
                deadline,
            )
            .await?;
        validate_record(&record)?;
        if record.shape != shape || record.source != source {
            return Err(Status::data_loss(
                "Axo returned incompatible volume metadata",
            ));
        }
        validate_capacity(record.capacity_bytes, required_bytes, limit_bytes)?;
        Ok(Response::new(csi::CreateVolumeResponse {
            volume: Some(volume(record)),
        }))
    }

    async fn delete_volume(
        &self,
        request: Request<csi::DeleteVolumeRequest>,
    ) -> Result<Response<csi::DeleteVolumeResponse>, Status> {
        let deadline = deadline(&request)?;
        let request = request.into_inner();
        identifier(&request.volume_id, "volume_id")?;
        let body = axo::VolumeId {
            volume_id: request.volume_id,
        };
        self.axo
            .delete("volumes/delete", &body, &[&body.volume_id], deadline)
            .await?;
        Ok(Response::new(csi::DeleteVolumeResponse {}))
    }

    async fn controller_publish_volume(
        &self,
        request: Request<csi::ControllerPublishVolumeRequest>,
    ) -> Result<Response<csi::ControllerPublishVolumeResponse>, Status> {
        let deadline = deadline(&request)?;
        let request = request.into_inner();
        identifier(&request.volume_id, "volume_id")?;
        node_id(&request.node_id)?;
        let cap = request
            .volume_capability
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("volume_capability is required"))?;
        let shape = capability(cap)?;
        if cap.access_mode.as_ref().is_some_and(|mode| {
            mode.mode == csi::volume_capability::access_mode::Mode::SingleNodeReaderOnly as i32
        }) && !request.readonly
        {
            return Err(Status::invalid_argument(
                "SINGLE_NODE_READER_ONLY requires readonly attachment",
            ));
        }
        let volume_context = storage_parameters(request.volume_context)?;
        validate_parameter_filesystem(&volume_context, &shape)?;
        let body = axo::EnsureAttachment {
            volume_id: request.volume_id,
            node_id: request.node_id,
            readonly: request.readonly,
            shape: shape.clone(),
            volume_context,
        };
        let attachment: axo::AttachmentRecord = self
            .axo
            .json(
                Method::POST,
                "attachments",
                &body,
                Some(&[&body.volume_id, &body.node_id]),
                deadline,
            )
            .await?;
        let publish_context = match attachment {
            axo::AttachmentRecord::LegacyKrbd => HashMap::new(),
            axo::AttachmentRecord::Qemu {
                volume_id,
                node_id,
                device_serial,
                readonly,
                shape: actual,
                format_policy,
            } => {
                if actual != shape
                    || format_policy != "preformatted"
                    || readonly != body.readonly
                    || volume_id != body.volume_id
                    || node_id != body.node_id
                {
                    return Err(Status::data_loss(
                        "Axo attachment does not match the requested volume capability",
                    ));
                }
                if device_serial.is_empty()
                    || device_serial.len() > 256
                    || device_serial
                        .chars()
                        .any(|c| c.is_control() || c.is_whitespace())
                {
                    return Err(Status::data_loss("Axo returned an invalid device serial"));
                }
                let mut context = shape_context(&shape);
                context.insert("device_serial".into(), device_serial);
                context.insert("readonly".into(), readonly.to_string());
                context
            }
        };
        Ok(Response::new(csi::ControllerPublishVolumeResponse {
            publish_context,
        }))
    }

    async fn controller_unpublish_volume(
        &self,
        request: Request<csi::ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<csi::ControllerUnpublishVolumeResponse>, Status> {
        let deadline = deadline(&request)?;
        let request = request.into_inner();
        identifier(&request.volume_id, "volume_id")?;
        let target = if request.node_id.is_empty() {
            None
        } else {
            node_id(&request.node_id)?;
            Some(request.node_id)
        };
        let body = axo::DeleteAttachment {
            volume_id: request.volume_id,
            node_id: target,
        };
        self.axo
            .delete(
                "attachments/delete",
                &body,
                &[&body.volume_id, body.node_id.as_deref().unwrap_or("*")],
                deadline,
            )
            .await?;
        Ok(Response::new(csi::ControllerUnpublishVolumeResponse {}))
    }

    async fn validate_volume_capabilities(
        &self,
        request: Request<csi::ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<csi::ValidateVolumeCapabilitiesResponse>, Status> {
        let deadline = deadline(&request)?;
        let request = request.into_inner();
        identifier(&request.volume_id, "volume_id")?;
        if request.volume_capabilities.is_empty() {
            return Err(Status::invalid_argument("volume_capabilities is required"));
        }
        for cap in &request.volume_capabilities {
            required_capability_fields(cap)?;
        }
        if !request.mutable_parameters.is_empty() {
            return Ok(Response::new(csi::ValidateVolumeCapabilitiesResponse {
                confirmed: None,
                message: "mutable volume parameters are not supported".into(),
            }));
        }
        let shape = match capabilities(&request.volume_capabilities) {
            Ok(shape) => shape,
            Err(error) => {
                return Ok(Response::new(csi::ValidateVolumeCapabilitiesResponse {
                    confirmed: None,
                    message: error.message().into(),
                }));
            }
        };
        let context = storage_parameters(request.volume_context.clone())?;
        let parameters = storage_parameters(request.parameters.clone())?;
        validate_parameter_filesystem(&context, &shape)?;
        validate_parameter_filesystem(&parameters, &shape)?;
        let body = axo::ResolveVolume {
            volume_id: request.volume_id,
            volume_context: context,
            parameters,
        };
        let record: axo::VolumeRecord = self
            .axo
            .json(Method::POST, "volumes/resolve", &body, None, deadline)
            .await?;
        validate_record(&record)?;
        if record.volume_id != body.volume_id {
            return Err(Status::data_loss("Axo changed an opaque volume identity"));
        }
        if record.shape != shape {
            return Ok(Response::new(csi::ValidateVolumeCapabilitiesResponse {
                confirmed: None,
                message: "requested capability does not match the existing volume".into(),
            }));
        }
        Ok(Response::new(csi::ValidateVolumeCapabilitiesResponse {
            confirmed: Some(csi::validate_volume_capabilities_response::Confirmed {
                volume_context: request.volume_context,
                volume_capabilities: request.volume_capabilities,
                parameters: request.parameters,
                mutable_parameters: HashMap::new(),
            }),
            message: String::new(),
        }))
    }

    async fn controller_expand_volume(
        &self,
        request: Request<csi::ControllerExpandVolumeRequest>,
    ) -> Result<Response<csi::ControllerExpandVolumeResponse>, Status> {
        let deadline = deadline(&request)?;
        let request = request.into_inner();
        identifier(&request.volume_id, "volume_id")?;
        let requested_shape = request
            .volume_capability
            .as_ref()
            .map(capability)
            .transpose()?;
        let (required_bytes, limit_bytes) = capacity(request.capacity_range.as_ref(), true)?;
        let body = axo::ExpandVolume {
            volume_id: request.volume_id,
            required_bytes,
            limit_bytes,
            expected_shape: requested_shape.clone(),
        };
        let record: axo::VolumeRecord = self
            .axo
            .json(
                Method::POST,
                "volumes/expand",
                &body,
                Some(&[
                    &body.volume_id,
                    &required_bytes.to_string(),
                    &limit_bytes.unwrap_or(0).to_string(),
                ]),
                deadline,
            )
            .await?;
        validate_record(&record)?;
        if record.volume_id != body.volume_id
            || requested_shape
                .as_ref()
                .is_some_and(|shape| shape != &record.shape)
        {
            return Err(Status::data_loss(
                "Axo expansion changed volume identity or capability",
            ));
        }
        validate_capacity(record.capacity_bytes, required_bytes, limit_bytes)?;
        Ok(Response::new(csi::ControllerExpandVolumeResponse {
            capacity_bytes: record.capacity_bytes,
            // Raw Block also needs the guest's SCSI rescan before its larger
            // capacity is visible to applications.
            node_expansion_required: true,
        }))
    }

    async fn create_snapshot(
        &self,
        request: Request<csi::CreateSnapshotRequest>,
    ) -> Result<Response<csi::CreateSnapshotResponse>, Status> {
        let deadline = deadline(&request)?;
        let request = request.into_inner();
        object_name(&request.name)?;
        identifier(&request.source_volume_id, "source_volume_id")?;
        let body = axo::EnsureSnapshot {
            name: request.name,
            source_volume_id: request.source_volume_id,
            parameters: storage_parameters(request.parameters)?,
        };
        let record: axo::SnapshotRecord = self
            .axo
            .json(
                Method::POST,
                "snapshots",
                &body,
                Some(&[&body.name]),
                deadline,
            )
            .await?;
        if record.source_volume_id != body.source_volume_id {
            return Err(Status::data_loss(
                "Axo snapshot source differs from request",
            ));
        }
        Ok(Response::new(csi::CreateSnapshotResponse {
            snapshot: Some(snapshot(record)?),
        }))
    }

    async fn delete_snapshot(
        &self,
        request: Request<csi::DeleteSnapshotRequest>,
    ) -> Result<Response<csi::DeleteSnapshotResponse>, Status> {
        let deadline = deadline(&request)?;
        let request = request.into_inner();
        identifier(&request.snapshot_id, "snapshot_id")?;
        let body = axo::SnapshotId {
            snapshot_id: request.snapshot_id,
        };
        self.axo
            .delete("snapshots/delete", &body, &[&body.snapshot_id], deadline)
            .await?;
        Ok(Response::new(csi::DeleteSnapshotResponse {}))
    }

    async fn list_snapshots(
        &self,
        request: Request<csi::ListSnapshotsRequest>,
    ) -> Result<Response<csi::ListSnapshotsResponse>, Status> {
        let deadline = deadline(&request)?;
        let request = request.into_inner();
        if request.max_entries < 0 {
            return Err(Status::invalid_argument("max_entries must not be negative"));
        }
        for (value, field) in [
            (&request.snapshot_id, "snapshot_id"),
            (&request.source_volume_id, "source_volume_id"),
        ] {
            if !value.is_empty() {
                identifier(value, field)?;
            }
        }
        if request.starting_token.len() > 8192 {
            return Err(Status::aborted("invalid snapshot pagination token"));
        }
        let body = axo::ListSnapshots {
            snapshot_id: nonempty(request.snapshot_id),
            source_volume_id: nonempty(request.source_volume_id),
            starting_token: nonempty(request.starting_token),
            max_entries: request.max_entries,
        };
        let page: axo::SnapshotPage = self
            .axo
            .json(Method::POST, "snapshots/list", &body, None, deadline)
            .await?;
        if (body.max_entries > 0 && page.entries.len() > body.max_entries as usize)
            || page
                .next_token
                .as_ref()
                .is_some_and(|token| token.len() > 8192)
        {
            return Err(Status::data_loss("Axo returned an invalid snapshot page"));
        }
        let mut entries = Vec::with_capacity(page.entries.len());
        for record in page.entries {
            if body
                .snapshot_id
                .as_ref()
                .is_some_and(|id| id != &record.snapshot_id)
                || body
                    .source_volume_id
                    .as_ref()
                    .is_some_and(|id| id != &record.source_volume_id)
            {
                return Err(Status::data_loss(
                    "Axo snapshot page does not match requested filters",
                ));
            }
            entries.push(csi::list_snapshots_response::Entry {
                snapshot: Some(snapshot(record)?),
            });
        }
        Ok(Response::new(csi::ListSnapshotsResponse {
            entries,
            next_token: page.next_token.unwrap_or_default(),
        }))
    }

    async fn controller_get_capabilities(
        &self,
        _: Request<csi::ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<csi::ControllerGetCapabilitiesResponse>, Status> {
        use csi::controller_service_capability::{Rpc, Type, rpc::Type as Kind};
        let kinds = [
            Kind::CreateDeleteVolume,
            Kind::PublishUnpublishVolume,
            Kind::ExpandVolume,
            Kind::CreateDeleteSnapshot,
            Kind::ListSnapshots,
            Kind::CloneVolume,
            Kind::PublishReadonly,
        ];
        let capabilities = kinds
            .into_iter()
            .map(|kind| csi::ControllerServiceCapability {
                r#type: Some(Type::Rpc(Rpc {
                    r#type: kind as i32,
                })),
            })
            .collect();
        Ok(Response::new(csi::ControllerGetCapabilitiesResponse {
            capabilities,
        }))
    }

    async fn list_volumes(
        &self,
        _: Request<csi::ListVolumesRequest>,
    ) -> Result<Response<csi::ListVolumesResponse>, Status> {
        Err(Status::unimplemented("ListVolumes is not supported"))
    }
    async fn get_capacity(
        &self,
        _: Request<csi::GetCapacityRequest>,
    ) -> Result<Response<csi::GetCapacityResponse>, Status> {
        Err(Status::unimplemented("GetCapacity is not supported"))
    }
    async fn controller_get_volume(
        &self,
        _: Request<csi::ControllerGetVolumeRequest>,
    ) -> Result<Response<csi::ControllerGetVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "ControllerGetVolume is not supported",
        ))
    }
    async fn controller_modify_volume(
        &self,
        _: Request<csi::ControllerModifyVolumeRequest>,
    ) -> Result<Response<csi::ControllerModifyVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "ControllerModifyVolume is not supported",
        ))
    }
    async fn get_snapshot(
        &self,
        _: Request<csi::GetSnapshotRequest>,
    ) -> Result<Response<csi::GetSnapshotResponse>, Status> {
        Err(Status::unimplemented(
            "GetSnapshot is not supported; use ListSnapshots with snapshot_id",
        ))
    }
}

fn nonempty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn identifier(value: &str, field: &str) -> Result<(), Status> {
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        Err(Status::invalid_argument(format!(
            "{field} must be a nonempty opaque identifier of at most 1024 bytes without control characters"
        )))
    } else {
        Ok(())
    }
}

fn object_name(value: &str) -> Result<(), Status> {
    // CSI names permit ordinary whitespace, including tab, LF and CR. They
    // remain opaque JSON data; neither shell nor path construction uses them.
    if value.is_empty()
        || value.len() > 1024
        || value
            .chars()
            .any(|c| matches!(c as u32, 0..=8 | 11..=12 | 14..=31 | 127..=159))
    {
        Err(Status::invalid_argument(
            "name must be a nonempty CSI name of at most 1024 bytes without forbidden control characters",
        ))
    } else {
        Ok(())
    }
}

fn node_id(value: &str) -> Result<(), Status> {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        Err(Status::invalid_argument(
            "node_id must identify a VM incarnation without whitespace",
        ))
    } else {
        Ok(())
    }
}

fn capability(cap: &csi::VolumeCapability) -> Result<axo::VolumeShape, Status> {
    use csi::volume_capability::{AccessType, access_mode::Mode};
    required_capability_fields(cap)?;
    if !cap.access_mode.as_ref().is_some_and(|mode| {
        [
            Mode::SingleNodeWriter as i32,
            Mode::SingleNodeReaderOnly as i32,
        ]
        .contains(&mode.mode)
    }) {
        return Err(Status::invalid_argument(
            "only SINGLE_NODE_WRITER and SINGLE_NODE_READER_ONLY are supported",
        ));
    }
    match &cap.access_type {
        Some(AccessType::Block(_)) => Ok(axo::VolumeShape {
            volume_mode: axo::VolumeMode::Block,
            fs_type: None,
        }),
        Some(AccessType::Mount(mount)) => {
            let fs_type = if mount.fs_type.is_empty() {
                "xfs"
            } else {
                &mount.fs_type
            };
            if !["xfs", "ext4"].contains(&fs_type) {
                return Err(Status::invalid_argument(
                    "only XFS and ext4 filesystems are supported",
                ));
            }
            if !mount.volume_mount_group.is_empty() {
                return Err(Status::invalid_argument(
                    "delegated volume_mount_group is not supported",
                ));
            }
            validate_mount_flags(&mount.mount_flags, fs_type)?;
            Ok(axo::VolumeShape {
                volume_mode: axo::VolumeMode::Filesystem,
                fs_type: Some(fs_type.into()),
            })
        }
        None => Err(Status::invalid_argument(
            "volume capability access_type is required",
        )),
    }
}

fn required_capability_fields(cap: &csi::VolumeCapability) -> Result<(), Status> {
    use csi::volume_capability::access_mode::Mode;
    if cap.access_type.is_none() {
        return Err(Status::invalid_argument(
            "volume capability access_type is required",
        ));
    }
    let mode = cap
        .access_mode
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("volume capability access_mode is required"))?;
    if Mode::try_from(mode.mode).is_err() || mode.mode == Mode::Unknown as i32 {
        return Err(Status::invalid_argument(
            "volume capability access_mode must be specified",
        ));
    }
    Ok(())
}

fn validate_mount_flags(flags: &[String], fs_type: &str) -> Result<(), Status> {
    const ALLOWED: &[&str] = &[
        "ro",
        "rw",
        "nosuid",
        "nodev",
        "noexec",
        "noatime",
        "relatime",
        "strictatime",
        "nodiratime",
        "sync",
        "dirsync",
        "discard",
        "nodiscard",
    ];
    if flags
        .iter()
        .any(|flag| !(ALLOWED.contains(&flag.as_str()) || flag == "nouuid" && fs_type == "xfs"))
    {
        return Err(Status::invalid_argument("unsupported mount flag"));
    }
    let contains = |flag: &str| flags.iter().any(|value| value == flag);
    if (contains("ro") && contains("rw"))
        || (contains("discard") && contains("nodiscard"))
        || ["noatime", "relatime", "strictatime"]
            .into_iter()
            .filter(|flag| contains(flag))
            .count()
            > 1
    {
        return Err(Status::invalid_argument("conflicting mount flags"));
    }
    Ok(())
}

fn capabilities(caps: &[csi::VolumeCapability]) -> Result<axo::VolumeShape, Status> {
    let first = caps
        .first()
        .ok_or_else(|| Status::invalid_argument("volume_capabilities is required"))?;
    let shape = capability(first)?;
    for cap in &caps[1..] {
        if capability(cap)? != shape {
            return Err(Status::invalid_argument(
                "mixed block/filesystem or filesystem types are not supported",
            ));
        }
    }
    Ok(shape)
}

fn capacity(
    range: Option<&csi::CapacityRange>,
    required: bool,
) -> Result<(i64, Option<i64>), Status> {
    if required && range.is_none() {
        return Err(Status::invalid_argument("capacity_range is required"));
    }
    let lower = range.map(|range| range.required_bytes).unwrap_or(0);
    let upper = range.map(|range| range.limit_bytes).unwrap_or(0);
    if lower < 0
        || upper < 0
        || (upper > 0 && upper < lower)
        || (range.is_some() && lower == 0 && upper == 0)
    {
        return Err(Status::invalid_argument("invalid capacity range"));
    }
    // 1 GiB when unconstrained; if a caller supplies only a smaller upper bound,
    // allow Axo to select a fitting positive capacity instead of violating it.
    let lower = if lower == 0 {
        if upper > 0 {
            upper.min(1 << 30)
        } else {
            1 << 30
        }
    } else {
        lower
    };
    Ok((lower, if upper == 0 { None } else { Some(upper) }))
}

fn validate_capacity(actual: i64, lower: i64, upper: Option<i64>) -> Result<(), Status> {
    if actual < lower || upper.is_some_and(|upper| actual > upper) {
        Err(Status::out_of_range(
            "Axo volume capacity does not satisfy requested range",
        ))
    } else {
        Ok(())
    }
}

fn content_source(source: csi::VolumeContentSource) -> Result<axo::ContentSource, Status> {
    use csi::volume_content_source::Type;
    match source.r#type {
        Some(Type::Snapshot(source)) => {
            identifier(&source.snapshot_id, "snapshot_id")?;
            Ok(axo::ContentSource::Snapshot(source.snapshot_id))
        }
        Some(Type::Volume(source)) => {
            identifier(&source.volume_id, "source volume_id")?;
            Ok(axo::ContentSource::Volume(source.volume_id))
        }
        None => Err(Status::invalid_argument(
            "volume_content_source type is required",
        )),
    }
}

fn shape_context(shape: &axo::VolumeShape) -> HashMap<String, String> {
    let mode = match shape.volume_mode {
        axo::VolumeMode::Block => "block",
        axo::VolumeMode::Filesystem => "filesystem",
    };
    let mut context = HashMap::from([
        ("volume_mode".into(), mode.into()),
        ("format_policy".into(), "preformatted".into()),
    ]);
    if let Some(fs_type) = &shape.fs_type {
        context.insert("fs_type".into(), fs_type.clone());
    }
    context
}

fn validate_record(record: &axo::VolumeRecord) -> Result<(), Status> {
    identifier(&record.volume_id, "Axo volume_id")
        .map_err(|_| Status::data_loss("invalid Axo volume identity"))?;
    if record.capacity_bytes <= 0
        || record.format_policy != "preformatted"
        || match record.shape.volume_mode {
            axo::VolumeMode::Block => record.shape.fs_type.is_some(),
            axo::VolumeMode::Filesystem => {
                !matches!(record.shape.fs_type.as_deref(), Some("xfs" | "ext4"))
            }
        }
    {
        return Err(Status::data_loss("invalid Axo volume metadata"));
    }
    Ok(())
}

fn volume(record: axo::VolumeRecord) -> csi::Volume {
    use csi::volume_content_source::{SnapshotSource, Type, VolumeSource};
    let content_source = record.source.map(|source| csi::VolumeContentSource {
        r#type: Some(match source {
            axo::ContentSource::Snapshot(snapshot_id) => {
                Type::Snapshot(SnapshotSource { snapshot_id })
            }
            axo::ContentSource::Volume(volume_id) => Type::Volume(VolumeSource { volume_id }),
        }),
    });
    csi::Volume {
        capacity_bytes: record.capacity_bytes,
        volume_id: record.volume_id,
        volume_context: shape_context(&record.shape),
        content_source,
        accessible_topology: Vec::new(),
    }
}

fn snapshot(record: axo::SnapshotRecord) -> Result<csi::Snapshot, Status> {
    identifier(&record.snapshot_id, "snapshot_id")
        .map_err(|_| Status::data_loss("invalid Axo snapshot identity"))?;
    identifier(&record.source_volume_id, "source_volume_id")
        .map_err(|_| Status::data_loss("invalid Axo snapshot source"))?;
    if record.size_bytes < 0
        || !(0..1_000_000_000).contains(&record.creation_time_nanos)
        || !(-62_135_596_800..=253_402_300_799).contains(&record.creation_time_seconds)
    {
        return Err(Status::data_loss("invalid Axo snapshot metadata"));
    }
    Ok(csi::Snapshot {
        size_bytes: record.size_bytes,
        snapshot_id: record.snapshot_id,
        source_volume_id: record.source_volume_id,
        creation_time: Some(prost_types::Timestamp {
            seconds: record.creation_time_seconds,
            nanos: record.creation_time_nanos,
        }),
        ready_to_use: record.ready_to_use,
        ..Default::default()
    })
}

/// Existing Ceph fields are routing hints only; Axo MUST compare them with the
/// cluster's authoritative journal and storage policy. Never forward CSI secrets.
fn storage_parameters(values: HashMap<String, String>) -> Result<BTreeMap<String, String>, Status> {
    if values.len() > 128
        || values
            .iter()
            .any(|(key, value)| key.len() > 256 || value.len() > 4096)
    {
        return Err(Status::invalid_argument("storage parameters exceed limits"));
    }
    for (key, value) in &values {
        match key.as_str() {
            "encrypted" if value != "false" => {
                return Err(Status::invalid_argument(
                    "encrypted volumes are not supported",
                ));
            }
            "encryptionKMSID" if !value.is_empty() => {
                return Err(Status::invalid_argument(
                    "encrypted volumes are not supported",
                ));
            }
            "mounter" if value != "rbd" => {
                return Err(Status::invalid_argument("only RBD volumes are supported"));
            }
            // Secret references are compatible metadata; secret values are never accepted.
            "userKey" | "adminKey" | "key" | "token" | "password" => {
                return Err(Status::invalid_argument(
                    "credentials must not be supplied in volume parameters",
                ));
            }
            _ => {}
        }
    }
    Ok(values.into_iter().collect())
}

fn validate_parameter_filesystem(
    values: &BTreeMap<String, String>,
    shape: &axo::VolumeShape,
) -> Result<(), Status> {
    for key in ["fs_type", "fsType", "fstype", "csi.storage.k8s.io/fstype"] {
        if let Some(fs_type) = values.get(key).filter(|value| !value.is_empty())
            && shape.fs_type.as_ref() != Some(fs_type)
        {
            return Err(Status::invalid_argument(
                "filesystem metadata conflicts with volume capability",
            ));
        }
    }
    if values.get("volume_mode").is_some_and(|value| {
        value
            != if shape.volume_mode == axo::VolumeMode::Block {
                "block"
            } else {
                "filesystem"
            }
    }) {
        return Err(Status::invalid_argument(
            "volume_mode conflicts with volume capability",
        ));
    }
    if values
        .get("format_policy")
        .is_some_and(|value| value != "preformatted")
    {
        return Err(Status::invalid_argument(
            "only preformatted volumes are supported",
        ));
    }
    Ok(())
}

fn deadline<T>(request: &Request<T>) -> Result<Option<Duration>, Status> {
    let Some(value) = request.metadata().get("grpc-timeout") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| Status::invalid_argument("invalid grpc-timeout"))?;
    if value.len() < 2 || value.len() > 9 {
        return Err(Status::invalid_argument("invalid grpc-timeout"));
    }
    let (number, unit) = value.split_at(value.len() - 1);
    if !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Status::invalid_argument("invalid grpc-timeout"));
    }
    let number: u64 = number
        .parse()
        .map_err(|_| Status::invalid_argument("invalid grpc-timeout"))?;
    let nanos = match unit {
        "H" => 3_600_000_000_000,
        "M" => 60_000_000_000,
        "S" => 1_000_000_000,
        "m" => 1_000_000,
        "u" => 1000,
        "n" => 1,
        _ => return Err(Status::invalid_argument("invalid grpc-timeout")),
    };
    Ok(Some(Duration::from_nanos(number.saturating_mul(nanos))))
}
