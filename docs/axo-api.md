# Axo CSI API proposal

**Status: client contract implemented and tested with an HTTP double; server
implementation required in Axo. These endpoints are not present in the checked
out Axo repository. They must pass the acceptance conditions below before this
driver can replace ceph-csi.** No Axo or Supernova source is modified by this
repository.

The machine-readable contract is [axo-openapi.yaml](axo-openapi.yaml). The CSI
wire interface is the vendored CSI v1.12.0 protocol in `proto/csi.proto`.

## Ownership and existing conventions

Existing Axo Kubernetes endpoints use
`/v4/kubernetes/organisations/{owner_id}/clusters/{cluster_id}`
(`axo/crates/api/kubernetes/src/controller.rs`, including the existing
`.../csi/ceph` endpoint). Existing storage management uses tenant-scoped
`/v4/tenants/{tenant_id}/...` endpoints, and compute lists VMs under
`/v4/compute/virtual-machines`. Those existing endpoints do not implement the
idempotent CSI image/attachment operations below.

The proposed cluster-scoped facade is:

```text
/v4/kubernetes/organisations/{organisation_id}/clusters/{cluster_id}/csi
```

The facade authorizes the cluster, calls storage for image operations, and calls
compute for attachment routing. Axo remains the sole authority for ownership,
billing/adoption, RBD journal resolution, VM incarnations and fencing. Supernova
is not exposed to the CSI driver. Organisation and Kubernetes cluster IDs are
distinct from Ceph's `clusterID` parameter.

All handles are opaque JSON strings. The client does not parse legacy ceph-csi
UUIDs, reconstruct RBD image names, concatenate a handle into a URL, or trust
`volume_context` as proof of ownership. This also supports statically provisioned
PV handles when Axo has an explicit, verified mapping for them.

## Authentication, transport and timeouts

- HTTPS and certificate verification are mandatory by default. `--allow-insecure-http`
  is an explicit local development setting; it never disables HTTPS certificate
  verification. URL credentials, paths, queries and fragments are rejected.
- A cluster-scoped Biscuit is supplied in `Authorization: Bearer ...`. Read the
  token from an absolute file path for **each HTTP request**, so atomic file
  replacement rotates it without restarting the controller. Neither CSI Secret
  values nor Ceph credentials are forwarded. Token expiry, revocation and scope
  verification belong to Axo, and must fail closed.
- Axo must bind every requested volume, snapshot and VM to the authorized
  organisation and cluster. Caller-supplied Ceph pool/namespace/cluster metadata
  are hints to compare against the authoritative registry, never authority.
- Redirects are disabled; the token must not move to a different origin.
- A configured HTTP timeout (at most 600 seconds), the incoming gRPC timeout,
  and client disconnect/cancellation bound work in this process. Mutations are
  sent once, without an application retry loop. On ambiguous completion, the
  next CSI retry asks Axo to reconcile the durable operation.
- HTTP timeout or gRPC cancellation is **not proof that backend work stopped**.
  Axo must fence superseded jobs and reconcile an already completed mutation
  rather than create another resource. A response must fit within 1 MiB.
- No unfiltered Axo error text is returned to Kubernetes; it may contain an
  upstream secret. Only documented status codes and fixed messages cross this
  boundary.

## Operations

Every mutation carries `Idempotency-Key: csi-<sha256>`. Keys are stable for the
API scope, operation and logical resource. The hash is length-delimited, does not
contain tokens, and distinguishes operation types.

| Method and suffix | Meaning | Successful body |
| --- | --- | --- |
| `GET /capabilities` | Contract and readiness verification | API version, features, format policy |
| `POST /volumes` | Ensure a named volume, clone, or snapshot restore | `VolumeRecord` |
| `POST /volumes/resolve` | Read and validate an existing handle and metadata | `VolumeRecord` |
| `POST /volumes/delete` | Delete an owned, detached volume | No body required |
| `POST /volumes/expand` | Ensure monotonically increasing capacity | `VolumeRecord` |
| `POST /attachments` | Ensure attachment, or verified legacy no-op | `AttachmentRecord` |
| `POST /attachments/delete` | Ensure completed, fenced detach | No body required |
| `POST /snapshots` | Ensure a named snapshot | `SnapshotRecord` |
| `POST /snapshots/delete` | Ensure snapshot deletion | No body required |
| `POST /snapshots/list` | Read a filtered page, including adopted snapshots | `SnapshotPage` |

The action routes intentionally keep opaque identities in JSON request bodies,
including delete and list operations. `resolve` and `snapshots/list` are read-only
and have no idempotency header. `resolve` must not start billing or adoption.

Successful mutations return only after their postconditions have been verified.
For example, an attachment is not ready merely because its AMQP instruction was
queued. HTTP 202 means in progress and maps to CSI `ABORTED`, never success.

### Readiness

`GET /capabilities` must validate its serving dependencies and return:

```json
{
  "api_version": "clever-csi-v1",
  "format_policy": "preformatted",
  "features": [
    "volume.ensure", "volume.resolve", "volume.delete", "volume.expand",
    "volume.clone", "volume.restore", "attachment.ensure", "attachment.delete",
    "snapshot.ensure", "snapshot.delete", "snapshot.list",
    "legacy-pool-routing", "attachment.readonly"
  ]
}
```

Missing routes or missing features make the controller unready. Advertising a
feature is a backend guarantee, not permission to implement it as a stub. The
controller advertises only its implemented CSI RPCs: volume create/delete,
publish/unpublish including read-only, expansion, snapshot create/delete/list,
and clone. It does not advertise GetCapacity, ListVolumes, ControllerGetVolume,
ControllerModifyVolume, GetSnapshot, topology, RWX or SINGLE_NODE_MULTI_WRITER.
Single-node writer and single-node reader-only access modes are accepted.

### Provisioning, clone, restore and adoption

`POST /volumes` receives `name`, `required_bytes`, optional `limit_bytes`,
`volume_mode` (`filesystem` or `block`), optional `fs_type` (`xfs` or `ext4`),
`parameters`, and optional `source`:

```json
{ "type": "snapshot", "id": "opaque-original-snapshot-handle" }
```

The other source type is `volume`. Its ID remains opaque. Creation keys are
scoped by cluster and CSI name. Same name and compatible source/capability/range
returns the same volume; incompatible requests return `ALREADY_EXISTS`. Validate
source ownership before any copy, ensure it is ready, preserve its data, honor
the requested size range, and return the exact source in `VolumeRecord.source`.
Sources smaller than the requested volume may be grown; requests that would
truncate source data must fail. Clones must be independent or retain tracked
dependencies so source deletion cannot corrupt them.

CSI creation names preserve Unicode and ordinary whitespace (including tab,
line feed and carriage return). Forbidden control ranges are U+0000–U+0008,
U+000B–U+000C, U+000E–U+001F and U+007F–U+009F; a name must be nonempty and at
most 1024 UTF-8 bytes. Resource handles remain nonempty opaque values without
control characters. Neither names nor handles are interpolated into URLs.
An omitted CSI capacity range uses the 1 GiB default. If a range is supplied,
at least one positive bound is mandatory; a present `{0,0}` range is invalid.
`ValidateVolumeCapabilities` rejects malformed required capability fields,
and returns an unconfirmed response for unsupported mutable parameters.

A `VolumeRecord` contains `volume_id`, positive `capacity_bytes`, `volume_mode`,
optional `fs_type`, `format_policy: preformatted`, and optional `source`. Its
immutable CSI context contains only mode, filesystem and format policy, never
credentials. Existing ceph-csi handles remain unchanged throughout adoption.

**The Node plugin never runs mkfs.** Axo's provisioning workflow must create and
format new filesystem images in a controlled platform environment, under a
durable exclusive allocation record, before the image is eligible for attach.
Record the allocated RBD image identity and initialization attempt before any
destructive operation. After an interrupted formatting step, inspect the same
allocated image and reconcile its recorded filesystem identity; do not blindly
reformat on a retry. Publish success only after initialization is durably ready.
Block images remain raw; `preformatted` means this initialization policy has
completed, not that raw Block contains a filesystem.

Adoption is a separate backend workflow used during attachment. It must verify
the legacy journal, exact image identity, organisation/cluster membership,
filesystem and encryption state; assign its durable registry entry and billing
start exactly once; and preserve existing data, handles and snapshots. Adoption
must **never format an image**, including a blank or ambiguous image. Unknown
static images require an explicit trusted import. Encryption/KMS metadata,
unsupported filesystems or ambiguous ownership fail closed.

Clone/restore preserves filesystem content. XFS mounts use `nouuid` in the Node
plugin so restoring multiple copies of the same filesystem does not conflict.
Snapshots are crash-consistent at the block layer; application quiescing is a
separate platform/workload responsibility, not an implicit CSI guarantee.

### Attachment, coexistence and fencing

`POST /attachments` receives `volume_id`, `node_id`, `readonly`, volume mode,
filesystem and untrusted `volume_context`. Node IDs are supplied by Kubernetes:

- Historical ceph-csi nodes may identify themselves by a reusable Node name
  (observed `manual-nodegroup-node0`). Axo must recognize them from its
  authoritative legacy pool registry and return `{"transport":"legacy-krbd"}`.
  This is a verified no-op: the existing ceph-csi plugin maps the volume. The
  controller returns an empty `publish_context`; it must not create a QEMU
  attachment for a historical Node name.
- New nodes identify the **exact VM incarnation**. Axo must match that immutable
  identity to the cluster, pool and hypervisor. It must never convert a name or
  IP address into a QEMU attachment target. Missing, stale, duplicated or
  ambiguous VM identity is rejected. A replacement VM is a different identity.
- Pool labels and caller hints alone are not authority. Axo must know whether a
  VM is using the legacy path, the new path or is being retired.

For the new path, return a QEMU record only after the VM has the expected image
attached and the requested read-only policy is effective:

```json
{
  "transport": "qemu",
  "volume_id": "opaque-original-volume-handle",
  "node_id": "exact-vm-incarnation",
  "device_serial": "stable-disk-serial",
  "readonly": false,
  "volume_mode": "filesystem",
  "fs_type": "xfs",
  "format_policy": "preformatted"
}
```

The node receives `device_serial`, `readonly` (`"true"` or `"false"`),
`volume_mode`, `fs_type` for filesystems, and `format_policy`. It searches the
actual device identity; no caller-provided `/dev/...` path is accepted. The
controller rejects mismatched volume/VM identity, filesystem/mode/read-only policy or malformed
serials. Read-only requests must also be enforced by QEMU/Axo, not merely by a
guest bind mount. `SINGLE_NODE_READER_ONLY` requires `readonly: true`.

The minimum safe attachment transaction requires:

1. Authoritative handle resolution and cluster ownership verification.
2. A durable per-image ownership claim and monotonic fencing epoch, serialized
   against attach, detach, delete, clone and expand operations.
3. Positive fencing of previous writers and stale work: verified device removal
   from the exact old VM, or confirmed isolation/destruction and appropriate
   Ceph session revocation/blocklisting. **An empty watcher/lock sample, a grace
   period, a Kubernetes object deletion or an AMQP acknowledgement is not a
   fence.** Old writers must be unable to reacquire the image. Existing exported
   Ceph credentials are part of this threat and must be accounted for before
   claiming isolation.
4. Backend checks for active Ceph users/locks as additional evidence, with all
   inspection failures treated as refusal. Never break a lock merely because a
   timeout elapsed; a verified old VM incarnation and its fence are required.
5. Attachment of the exact RBD image to the authorized VM, with matching serial,
   read-only policy and observed QEMU completion, then durable registry commit.

An old completion carrying a revoked epoch must not resurrect an attachment.
VM twins and uncertain old mappings must block attachment instead of risking
concurrent writers. The controller does not implement these platform primitives.

Unpublish accepts `node_id: null` to mean all attachments for the volume, as CSI
requires when node_id is omitted. Axo must enumerate authoritative incarnations
and verify completed detaches; it must not choose an arbitrary live VM by name.
Legacy unpublish may be a no-op only after backend verification; it does not
authorize a new QEMU writer until the old path is positively fenced.

### Idempotency is state reconciliation

Keys are **not an eternal response cache**. The sequence
`attach(A) -> detach(A) -> attach(A)` repeats the attach key and must reattach on
the third call. The client sends all three HTTP calls. Axo checks current desired
state and the current fenced generation rather than replaying old success.
Similarly, a replayed delete must be scoped to the immutable image/snapshot
identity and must not delete a new object created with a reused display name.

Persist operation intents/results across process and hypervisor restarts.
Concurrent calls for one image must converge or return `operation-in-progress`.
Do not blindly retry AMQP mutations after a transport error. Detect their actual
postcondition and compare the active fencing generation first. This also
applies to idempotent adoption and billing outbox events.

### Expansion and snapshots

Expansion receives a capacity range and optional `expected_shape`. Validate its
shape **before mutation**. Never shrink a volume. An existing size satisfying
the range returns success. First grow the RBD image, then every applicable QEMU
block device, verify the observed capacity and persist completion. The CSI
response requests Node expansion for both filesystems and raw Block: the guest
may need a SCSI rescan even when no filesystem resize is needed.

Snapshot ensure is scoped by CSI name and source handle. Same name with a
different source returns `ALREADY_EXISTS`; same source returns the existing
snapshot. Snapshot records include size, original source handle, precise UTC
timestamp (`creation_time_seconds` and `creation_time_nanos`), and readiness.
List returns `entries` and an optional opaque `next_token`. Honor both filters,
return no more than positive `max_entries`, and use a backend default when zero.
No matches produce an empty page. Pagination must be stable across requests and
scoped to the same authorised filters. Unknown/expired tokens return the special
invalid-pagination-token error. Adopted legacy snapshots must retain their
identities and source relationships.

## Error contract

Errors have an opaque human message and stable `code`; the driver ignores the
message. The codes below are contractual, including the difference between a
missing API route and a missing resource.

| HTTP | Machine code when required | CSI status |
| --- | --- | --- |
| 400 | `clever.csi.invalid-pagination-token` | `ABORTED` |
| 400 | other | `INVALID_ARGUMENT` |
| 401 / 403 | any | `UNAUTHENTICATED` / `PERMISSION_DENIED` |
| 404 | `clever.csi.resource-not-found` | `NOT_FOUND`; delete/unpublish succeeds idempotently |
| 404 | absent or other code | `UNIMPLEMENTED`; an absent API must not simulate deletion |
| 409 | `clever.csi.incompatible-resource` | `ALREADY_EXISTS` |
| 409 | `clever.csi.operation-in-progress` | `ABORTED` |
| 409 / 412 | other, including fencing/in-use | `FAILED_PRECONDITION` |
| 408 / 504 | any | `DEADLINE_EXCEEDED` |
| 422 | any | `OUT_OF_RANGE` |
| 429 / 507 | any | `RESOURCE_EXHAUSTED` |
| 501 | any | `UNIMPLEMENTED` |
| other 5xx / transport unavailable | any | `UNAVAILABLE` |
| 202 | any | `ABORTED` (not completed) |
| 3xx | any | `FAILED_PRECONDITION` (redirect prohibited) |

## Backend acceptance before rollout

Contract tests in this repository prove client behavior, not backend fencing.
The Axo implementation must additionally demonstrate crash/retry convergence,
simultaneous attach/delete/expand serialization, stale-epoch rejection, VM twins
refusal, old-node legacy routing, real QEMU attach/detach acknowledgement,
Ceph credential isolation, preformat crash recovery without data loss, preserved
legacy image/snapshot identity, exact-once adoption billing, snapshot restore
and clone integrity, and monotonic online expansion including guest rescans.
Run those tests with real Supernova and Ceph before an attachment takeover.
