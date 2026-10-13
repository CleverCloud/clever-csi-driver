# Axo Kubernetes CSI API proposal

**Status: proposed resource API, revision `2026-10-13`. Neither the current
driver client nor Axo implements this revision.** The driver currently uses
the earlier action API, `clever-csi-v1`; its HTTP contract tests validate that
earlier interface. The [previous contract at commit 339e7f0](https://github.com/CleverCloud/clever-csi-driver/blob/339e7f0/docs/axo-api.md)
records the implemented client behavior. Updating this document does not change
the driver, deploy an API or enable a migration.

[axo-openapi.yaml](axo-openapi.yaml) defines the proposed routes, JSON schemas,
per-operation responses and machine-readable error catalogue. This document
defines their lifecycle and safety requirements. Both must agree before the
client and backend are implemented. CSI remains the vendored v1.12.0 protocol.

## Scope and ownership

The API is a cluster-scoped facade under:

```text
/v4/kubernetes/organisations/{ownerId}/clusters/{clusterId}/csi
```

Axo authorizes the owner and cluster, resolves storage identities, and routes
attachment work to compute/Supernova. It remains the authority for adoption,
billing, RBD journals, VM incarnations and fencing. The driver receives neither
Ceph credentials nor direct hypervisor access. Caller-supplied pool, namespace,
Ceph `clusterID`, node labels and `volumeContext` are hints, never authority.

All routes resolve identities against the authorized Axo registry and, where
applicable, the legacy ceph-csi journal. Failed lookup is an error, not proof
of absence. GET, listings and `resolve` are pure reads: no alias registration,
adoption, formatting or billing. Listings include verified legacy resources
before adoption, with their provenance and adoption state. Adoption is an
explicit backend step of attachment on the new path.

## Authentication and audience

- The driver uses a cluster-scoped Biscuit in `Authorization: Bearer ...`.
  Axo checks path scope and permitted operation on every request. The controller
  rereads its projected token file for each HTTP request.
- Resource reads may also use OAuth1, subject to organization membership,
  Kubernetes admission and the platform allow-list. The capability endpoint
  is for the driver, not a tenant health dashboard.
- Mutations and the driver's POST `resolve` endpoint use the Biscuit. Console
  mutations are outside this revision; operator workflows must respect PV
  ownership and attachment fencing.
- A token scope mismatch is rejected before resource lookup. Foreign resources
  are excluded from listings and indistinguishable from absent item reads.
  Delete may return `ALREADY_ABSENT` only after verifying that no target belongs
  to this authorized scope; it must not touch a foreign resource.
- Production requires HTTPS with certificate verification and no redirects.
  Tokens and CSI Secret values never enter resource records or node context.

Authentication/path middleware can emit its existing error format before the
handler runs. Clients retain status-based handling for those responses; OpenAPI
response declarations do not change middleware behavior.

## Identity and compatibility

A volume or snapshot has one immutable resource identifier, also its CSI handle.
Names are separate idempotency keys. Identifiers are never reused, even after
deletion or reuse of a creation name. The driver does not parse legacy handles,
derive RBD image names or normalize identity strings.

New resource identifiers are path-safe ASCII, at most **128 bytes**. Resource
paths accept existing path-safe legacy handles up to 1024 bytes; that larger
acceptance bound is a migration tolerance, not a new CSI emission limit.
Existing handles remain byte-for-byte unchanged. CSI's general string bound is
128 bytes unless overridden; node identity has an explicit 256-byte bound.
Any longer historical handle needs compatibility assessment with the actual CO.

**Inventory every existing PV and snapshot handle before adopting this route
format.** Handles outside `[A-Za-z0-9][A-Za-z0-9._-]*` are not addressable by
this revision. Never rewrite them, declare them absent or report local delete
success. The future driver returns `FAILED_PRECONDITION`, leaving Kubernetes
objects and storage intact. Resolve compatibility through an operator workflow
or add a reviewed opaque-handle transport before migrating those resources.
An inventory finding is not permission to edit a bound PV's handle.

For new nodes, `nodeId` is the exact VM incarnation from
`/etc/clever-csi/node-id`; a replacement has a different identity. A historical
Node name is accepted only through Axo's authoritative legacy-pool registry.
Neither names nor IPs are translated into guessed QEMU targets. This contract
introduces no lowercase-UUID restriction inconsistent with NodeGetInfo.
Node IDs remain nonempty tokens of at most 256 UTF-8 bytes without whitespace
or control characters. Encode one path segment and preserve its exact value
through the HTTP front. Unsupported encodings or dot segments cause explicit
refusal, never local detach success. Validate actual platform identities and
front decoding before release.

Creation names preserve Unicode and ordinary whitespace, excluding CSI's
forbidden controls. New names follow the 128-byte CSI limit; up to 1024 UTF-8
bytes may be accepted only for verified legacy names or existing-resource
retries. No trimming or case folding. JSON Schema `maxLength` counts characters;
UTF-8 byte bounds need separate validation, marked by `x-maxUtf8Bytes` where
applicable in the OpenAPI document.

## Resource routes

Suffixes are relative to the cluster CSI prefix. Mutation success requires
observed and durable postconditions. Snapshot creation completes the
point-in-time cut; processing readiness is a separate state.

| Method and suffix | Contract |
| --- | --- |
| `GET /capabilities` | Selected revision, features, limits and cached readiness |
| `GET /volumes` | Authorized inventory, including verified legacy volumes |
| `POST /volumes` | Ensure volume, clone or restore by name; 201 new, 200 compatible existing |
| `GET /volumes/{volumeId}` | Read a volume |
| `POST /volumes/{volumeId}/resolve` | Verify shape and hints without adopting or billing |
| `DELETE /volumes/{volumeId}` | Delete a detached volume with an explicit outcome |
| `POST /volumes/{volumeId}/expand` | Ensure monotonic image and applicable QEMU capacity |
| `GET /volumes/{volumeId}/attachments` | Read attachments |
| `POST /volumes/{volumeId}/attachments` | Ensure for body nodeId; 201 new, 200 compatible existing |
| `GET /volumes/{volumeId}/attachments/{nodeId}` | Read an attachment |
| `DELETE /volumes/{volumeId}/attachments/{nodeId}` | Verify detach from one incarnation |
| `DELETE /volumes/{volumeId}/attachments` | Verify detach from all authoritative incarnations |
| `GET /snapshots` | Filtered, paginated inventory |
| `POST /snapshots` | Ensure a named point-in-time snapshot; 201 new, 200 existing |
| `GET /snapshots/{snapshotId}` | Read readiness or durable failure |
| `DELETE /snapshots/{snapshotId}` | Delete while preserving dependent clones |

JSON uses camelCase, uppercase business enums and RFC 3339 UTC timestamps
preserving nanoseconds. CSI maps `parameters` and `volumeContext` retain original
keys. Shared error-context discriminators retain Axo's snake_case convention.

## Idempotence, operations and deletion

There is no `Idempotency-Key` header. Logical identity comes from authorized
scope, resource path and operation-specific fields:

| Operation | Identity and replay rule |
| --- | --- |
| Ensure volume | Cluster and CSI name; compatible shape, source, policy and capacity return the same resource |
| Ensure snapshot | Cluster and CSI name; different source is incompatible |
| Ensure attachment | Volume and exact incarnation; reconcile current state and requested attributes |
| Expand | Immutable volume and requested range; finish pending growth, never shrink |
| Delete/detach | Immutable resource and addressed incarnation or all-incarnations scope |

A retry joins an in-flight intent or reconciles current state. Incompatible
requests return `incompatible-resource`, never allocate a second resource.
`attach(A) -> detach(A) -> attach(A)` must reattach on the last call rather than
replay old success. After a snapshot cut, replay preserves its identity and
original timestamp; it never silently takes a later snapshot under that name.

Persist intents, claims, initialization records and billing outbox events before
their effects. Serialize competitors and reject stale workers with a monotonic
backend fencing epoch. This is distinct from public attachment generation:
backend bookkeeping must not invalidate unchanged cached CSI context. Timeout
does not cancel backend work or prove failure. A superseding detach must prevent
an old attach worker from subsequently restoring a writer.

Mutations accept `Prefer: wait=<seconds>` and return `Preference-Applied`.
Proposed default/max wait values are 30/110 seconds, **subject to HTTP-front
validation**. If still running, return `409 clever.kubernetes.operation-in-progress`,
`Retry-After` and structured operation context. These endpoints never emit 202.
The driver budgets waiting within both the remaining gRPC deadline and HTTP
timeout, reserving a bounded proportional response margin. A short deadline
must not acquire an extra fixed ten seconds of work. `Retry-After` does not
justify blind mutation retries inside an existing CSI call.

The front must tolerate maximum wait plus response overhead. Bound server
waiters and concurrent intents too. `X-Request-Id` is only bounded correlation
metadata; Axo retains its separately generated `apiRequestId`.

Every DELETE requires **200 with a validated outcome**:

- Volume/snapshot deletion: `DELETED` or `ALREADY_ABSENT`.
- Detachment: `DETACHED`, `ALREADY_ABSENT` or verified `LEGACY_NOOP`.
- Echo `resourceId`, nullable `nodeId`, `detachedNodeIds` and nullable
  `operationId`. Check identities and the outcomes allowed for that operation.
  Detach-all covers every authoritative incarnation; a legacy no-op cannot hide
  a remaining QEMU or krbd writer.

`ALREADY_ABSENT` requires successful authoritative lookup, including legacy
journals where relevant. Missing routes/clusters, unavailable registries and
malformed identities remain errors. **No 404 or local syntax rejection becomes
successful deletion/detachment.** Deleting an attached volume never implicitly
detaches it. Logical deletion may defer physical reclamation while preserving
snapshots and clones; the handle becomes unusable and billing changes durably.

## Provisioning, validation and growth

Ensure-volume compatibility includes source, shape, policy and capacity range.
Validate source ownership/readiness before copying, never truncate source data,
and preserve clone dependencies. Returned capacity fits the requested range
after backend rounding. For new filesystem clones/restores, initialization and
necessary filesystem growth finish before attachment eligibility.

**The Node plugin never runs mkfs.** Axo initializes only newly allocated images
under exclusive durable ownership. Interrupted initialization reconciles the
recorded image and filesystem; retries must not blindly reformat. Adopted images,
including blank or ambiguous images, are never formatted. Unsupported filesystems,
encryption and uncertain identity cause refusal. `PREFORMATTED` means the
initialization policy completed; Block volumes remain raw.

`resolve` returns verified actual shape whenever it is determinable, even if it
differs from `expectedShape`. For `ValidateVolumeCapabilities`, a known mismatch
returns CSI success without `confirmed`. An API `412 shape-mismatch` for a known
mismatch receives that same RPC-specific mapping. Inspection failures, missing
ownership proof and unknown resources are not capability mismatches. For
`ControllerExpandVolume`, incompatible shape maps to `INVALID_ARGUMENT` and
must be rejected before mutation.

Growth is monotonic and serialized with attachment. An image already at the
requested size does not prove completion after a partial failure: finish QEMU
resize and re-observe capacity before success. Coordinate legacy krbd and QEMU
resize paths. CSI requests node expansion for both Filesystem and Block volumes,
including guest rescans and filesystem growth where applicable.

## Attachment identity and fencing

The flat record includes `volumeId`, `nodeId`, `transport`, `device`,
`generation`, `readonly`, shape and initialization policy. Validate combinations:

- `QEMU` requires `device.bus: VIRTIO_SCSI` and an exact ASCII serial matching
  `[A-Za-z0-9._-]{1,36}`: QEMU scsi-hd over virtio-scsi, not virtio-blk.
- `LEGACY_KRBD` requires `device: null` and an authoritative legacy node. It
  is reserved for a verified legacy image not yet adopted on the new path,
  with no QEMU attachment or pending claim and no recorded fence forbidding
  return to legacy access. Native or adopted volumes cannot take this shortcut.
  It authorizes no QEMU attachment; the old plugin receives empty publish context.
- Returned identities, shape and read-only policy match the request. QEMU
  success requires observed attachment and durable completion, not AMQP enqueue.

An existing attachment generation and serial survive reboot, stop/start and live
migration of the **same incarnation**. A repeated ensure returns that compatible
record. Completed detach/fence followed by attachment creates a new generation.
Retain enough history to distinguish retired/fenced incarnations from unknown
or ambiguous identities during cleanup.

The serial remains stable for the volume. **Never reassign it to a different
volume on the same incarnation while an old publish context could be replayed.**
Current uniqueness alone is insufficient: Node discovers by serial and cannot
use an API-only generation to detect reuse. Never truncate, case-fold or derive
the serial by shortening the volume handle. Node may remove transport padding
before exact comparison; conflicting attributes or duplicate matches are errors.

Before committing an attachment, Axo must:

1. Resolve the image and verify owner, cluster and exact incarnation.
2. Acquire the durable per-image claim and fence superseded work.
3. Positively exclude old writers through observed device removal or verified
   isolation/destruction and Ceph session revocation/blocklisting as applicable.
4. Inspect remaining clients as additional evidence, refusing on uncertainty.
5. Observe the expected image, serial and read-only policy and commit durably.

An empty watcher/lock sample, grace period, Kubernetes deletion or AMQP ack is
not a fence. Account for exported legacy Ceph credentials. VM twins, unknown old
mappings and unreachable hypervisors cannot become successful detaches. A legacy
no-op permits no new writer until the old path is released or positively fenced.
Attachment to another active incarnation returns `attached-elsewhere`; an attach
request must not evict a valid owner or implicitly detach it. Fencing during
handoff addresses superseded work or an incarnation authoritatively retired by
the platform. It is not permission to destroy a live writer to satisfy a request.

Backend enforcement is mandatory for read-only attachment. Filesystem publication
cannot relax a read-only constraint. Raw Block read-only publication also needs
Linux block protection (`blockdev --getro` equals `1`); bind flags alone are
insufficient. Never toggle a shared writable device globally read-only for one
pod. Node publish-context keys stay `device_serial`, `readonly`, `volume_mode`,
`fs_type`, `format_policy`; API camelCase does not rename this cached CSI interface.

## Snapshots and pagination

POST returns a snapshot only after the point-in-time cut, with immutable
`id` (the path's `snapshotId`), `sourceVolumeId`, `creationDate`. Success may be
`PROCESSING` with `readyToUse: false`, or `READY` with `readyToUse: true`. Uncut creation intents
are not CSI snapshot records and are excluded from listings. Snapshots are
block-level crash-consistent; application quiescing is a separate responsibility.

A post-cut failure remains durable `FAILED`, preserving identity, timestamp and
a diagnostic failure record. Retry recoverable processing, but do not promise
eventual success after irrecoverable loss. Replayed POST returns `snapshot-failed`,
never a new cut or fabricated 404. GET and HTTP listings expose failure for
diagnosis. The future driver maps a failed snapshot selected by a CSI call to
`INTERNAL`, instead of returning `ready_to_use: false` indefinitely. This includes
an unfiltered CSI page selecting a failed item; it cannot silently omit it.
Recovery and explicit deletion preserve dependent resources.
Failure records visible to organization members must be bounded and sanitized;
they expose no storage credentials or private hypervisor diagnostics.

See the [CSI snapshot error contract](https://github.com/container-storage-interface/spec/blob/v1.12.0/spec.md#the-ready_to_use-parameter)
and [snapshotter v8.4.0](https://github.com/kubernetes-csi/external-snapshotter/blob/v8.4.0/pkg/sidecar-controller/snapshot_controller.go#L237):
ordinary dynamic snapshots repeat CreateSnapshot; they do not universally switch
to ListSnapshots after receiving a handle. Test dynamic and preprovisioned paths.

Lists use `cursor`, `limit` and `{items, nextCursor}`. Opaque cursors are bound to
scope/filters and emitted as at most 128 ASCII bytes; accepted cursors may reach
2048 bytes for compatibility, subject to published limits. Invalid cursors map
to CSI `ABORTED`. No matches produce an empty page. Only targeted
`snapshot-not-found` becomes an empty CSI list, never an absent route or cluster.

Positive CSI `max_entries` caps page size; zero selects backend defaults. A page
may be shorter and still have a continuation. Enforce the **serialized response
byte limit**, including JSON escaping and diagnostics; a count limit alone is
insufficient. Filters must not leak foreign resources. Concurrent creates/deletes
may affect subsequent pages; do not claim global snapshot isolation.

## Version, capabilities and errors

The driver always pins `Clever-API-Version: 2026-10-13`. Absence may mean latest
for other clients, but not for a deployed CSI controller. Axo selects supported
semantics and echoes the effective revision, never silently substitutes an
incompatible latest shape. Retain Axo's `/v4` prefix without an extra CSI version
segment. Version dispatch needs integration in these handlers; it is not automatic.

Capabilities publish effective `contractVersion`, `minContractVersion`,
`formatPolicy`, `features.core`, `features.optional` and limits. Core covers
ensure/resolve/delete volume and ensure/delete/read-only attachment. Expansion,
snapshots, cloning, restoration, legacy routing and additional inventories are
advertised separately. Snapshot restoration depends on snapshot support.
Features gate operations, not alternative JSON meanings or new enum values
under the same revision.

Advertised CSI capabilities must reflect implemented, negotiated features.
Missing optional snapshots need not disable core volume operations. **Mixed-pool
rollout still requires `LEGACY_POOL_ROUTING`**, plus features needed by existing
workloads. Readiness cannot silently discard rollout requirements as optional.
Freeze a coherent feature set per controller instance; incompatible changes need
controlled reconfiguration or restart.

Static contract data can be cached. `ready`, `checkedAt` and `unreadyReasons`
come from a backend health cache at most ten seconds old, without synchronous
deep probing. Do not serve them indefinitely from HTTP caches. Size request
limits for replicas and rolling upgrades; 429 must not extend old healthy state
indefinitely. Readiness remains distinct from process liveness.

The complete machine-code catalogue is `x-csi-error-codes` in OpenAPI. Each
handler code has one HTTP status, a default CSI mapping and any RPC-specific
exception. Codes use `clever.kubernetes.*`. Handler errors follow Axo's
`HTTPErrorBody` (`apiRequestId`, `code`, `context`, `error`). Middleware errors
may keep existing envelopes and require status-based handling. Unknown future
codes never imply success. HTTP 401/403 retain their authentication/authorization
meaning. A known code with an incompatible HTTP status is `DATA_LOSS`, not a
negative successful validation or empty list. Do not forward human server
messages or arbitrary context into Kubernetes errors.

## Client migration

The current controller and mocks still speak `clever-csi-v1`. Implementing this
proposal requires a separate coordinated client/backend change:

- Change paths, methods, camelCase records, enums and pagination. Preserve raw
  identities and safely construct path segments.
- Replace hash headers and 404-to-success handling with reconciliation and
  validated deletion outcomes. Retain token rotation and no blind retries.
- Parse precise RFC 3339 timestamps and durable snapshot failures without
  rounding nanoseconds, recreating a cut or hiding failure behind readiness.
- Add version selection, negotiated features/limits and bounded Prefer waiting
  within the existing gRPC deadline.
- Apply error mappings in RPC context, including negative successful capability
  validation and expansion's incompatible-shape error.
- Rewrite HTTP fixtures; exercise revision compatibility, frontend encoding,
  cancellation, mixed pools and failure paths.

API camelCase alone does not change Node/chart wire formats. That holds only
while node identities and publish context retain their semantics. Any later
restriction needs joint platform/controller/NodeGetInfo validation. Readiness,
roles and feature configuration may require deployment changes at implementation.

## Acceptance before rollout

Schema validation proves neither backend behavior nor current client compatibility.
Release acceptance additionally requires:

- PV/snapshot/node identity inventory and frontend path round trips; no false
  success for incompatible handles or unknown incarnations.
- Crash/retry convergence, competing-operation serialization, stale-worker
  rejection and positive writer fencing with real Ceph/QEMU.
- Serial/generation persistence through reboot and migration, with no reassignment
  capable of misdirecting cached publish context.
- Safe initialization recovery, lossless adoption, exactly-once billing
  transitions, clone/restore integrity and coordinated growth.
- Snapshot failures before/after cut, dynamic/preprovisioned polling, explicit
  cleanup and preservation of dependents.
- Version and feature combinations, middleware errors, bounded waiting,
  response-size limits and representative concurrent load.

Confirm frontend idle/URL limits, handle inventory, authorization integration and
rollout features with the Axo team before freezing the first revision. Error
namespace and wait limits are decisions in this draft, not deployed guarantees.
