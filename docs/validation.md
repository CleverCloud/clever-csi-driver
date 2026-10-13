# Validation

The runtime checks exercise the [current `clever-csi-v1` HTTP contract](axo-api-v1.md),
which originated at baseline `339e7f0` and now requires the guarded attachment
transport feature. The
[REST proposal dated 2026-10-13](axo-api.md) requires the
[client migration](axo-api.md#client-migration) and an Axo implementation;
the existing controller tests do not exercise that proposal.

## Proposed REST contract

Run `python3 -m pip install -r deploy/requirements.txt` followed by
`python3 deploy/test_api_contract.py` to reproduce the checked-in
[`axo-contract.json`](../deploy/fixtures/axo-contract.json) fixtures. CI executes
this check alongside deployment validation.

On 2026-10-13, the replacement document passed OpenAPI 3.1 validation and 169
positive/negative schema cases. These covered conditional attachment fields,
filesystem/Block shape, serials and identifiers, UTF-8 byte bounds, snapshot
failure/readiness, bounded public diagnostics, deletion outcomes and pagination
cursors. The byte-bound checks use the documented `x-maxUtf8Bytes` and
`x-maxSerializedBytes` rules in addition to JSON Schema;
ordinary schema tooling does not enforce that extension automatically.

The document's 16 operations and 31 machine codes were also checked for matching
HTTP responses, error catalogue references, version headers, authentication and
operation-specific success bodies. These are local document checks, not
runtime contract tests or evidence of an implemented Axo backend. The required
client migration and platform acceptance tests remain outstanding.

## Rust and real Linux storage tests

Final local checks on 2026-10-13 passed: `cargo fmt --all -- --check`,
`cargo clippy --all-targets --locked -- -D warnings` and
`cargo test --all-targets --locked`. The ordinary test run passed **96 tests**
(55 library, 40 controller contract and one gRPC transport), with the privileged
Linux test explicitly excluded and run separately as described below. The
restored and updated `clever-csi-v1` OpenAPI document also passed
`openapi-spec-validator` validation. The separate REST proposal is covered by
the document checks above, not these runtime tests.

The Rust unit and contract tests exercise persistent node-state recovery,
device identity, mount ownership, read-only access, malformed requests,
controller idempotency keys, Biscuit rotation, response validation, timeouts
and sanitized backend errors. These targeted tests are not exhaustive mutation
coverage: an assertion passing does not prove every guard is independently
exercised. The fake host cannot reproduce all Linux mount/error behavior.
A separate test uses a real Unix-domain socket
and generated CSI gRPC clients. Formatting and Clippy with warnings denied are
part of the required CI checks.

Recovery tests cover re-enumeration, permutations of two and three disks with
publication between stages, and refusal to rebind devices with old, foreign or
nested mounts. Serial tests cover unrelated malformed VPD pages, transport
padding, conflicting identities and the exact 36-character attachment limit.
Unpublish tests verify owned-target removal and retained recovery intent when
cleanup fails. Read-only tests cover filesystem constraints and rejection of a
read-only Block request that would affect an existing writable publication.

`bash deploy/test_linux.sh` successfully ran the privileged integration test
against real Linux loop block devices in a disposable Docker container. It
exercised XFS, ext4 and raw Block: stage, publish, write/read checksum, statistics,
driver restart with persisted state, online growth, unpublish and unstage,
including repeated cleanup calls. A blank adopted disk was rejected without
changing its bytes. Additional cases verify filesystem read-only constraints,
rejection of foreign raw Block binds, removal of published targets and retention
of staging directories. A read-only Block publication requires both the backend
read-only indication and Linux block protection: attempted writes to a real
read-only loop device failed and its checksum remained unchanged. Requesting a
read-only publication on an existing writable attachment was rejected without
changing the writable consumer. The current suite also covers a private copy of
a filesystem mount, unmount blocked in the kernel after its mountinfo entry
disappears, exclusive open refusal, a child retaining the driver lock, and XFS shutdown/EIO followed
by restart and cleanup. Release is also refused after restart when a private
mount survives missing serial discovery, or a device path disappears while
sysfs still reports the device. A custom device directory on ext4 verifies
detection of foreign raw Block binds without scanning unrelated filesystem
targets. The aggregate Linux test passed nine scenarios.

The node now retains pending effects across RPC cancellation and keeps recovery
state until mount commands finish. Its inherited process lock and exclusive-open
check were exercised in the blocked-unmount test. This does not prove absence
of all ordinary raw Block descriptors; backend fencing remains required.

The backing files and mounts were isolated to the test container. The runner
identifies its loop backing files by filesystem device/inode, not a path that
can change when the allocating container disappears. A separate recovery check
removed a freshly allocated loop after destruction of that container. The runner
cleaned its owned loop devices, container and temporary Docker volumes; absence
of remaining test resources was checked. The test is ignored in an ordinary
`cargo test` run and executed explicitly by the `linux-mounts` CI job before
image publication. SCSI VPD serial discovery used a sysfs fixture backed by
real loop devices; this does **not** validate a QEMU virtio-scsi attachment.

Controller contract tests use a mock Axo HTTP server implementing the historical
routes and response shapes. The replacement API documents describe a future
contract for both client and backend; neither those mocks nor the runtime
implement it. There has been no end-to-end Ceph provisioning, real VM fencing,
legacy volume migration or billing validation against Axo and Supernova.

## CSI conformance fixture

`bash deploy/test_linux.sh sanity` builds pinned `csi-sanity` v5.6.0 and the
test-only `examples/csi-sanity-server.rs` in a disposable privileged container.
The combined test server exposes the production Controller and Node services on
one socket; the production binary's separate roles are unchanged. A local Python
Axo fixture checks routes/authentication and the allowed-transport guard, creates
new sparse image files, formats them, and attaches only owned loop devices.
Device transport/serial discovery is synthetic; filesystem and block operations
run on Linux. This is neither the proposed REST backend nor real QEMU/Ceph.

On 2026-10-13, **72 specs passed, zero failed, 30 were skipped and one was marked
pending upstream**. Skips follow capabilities/options not advertised by this
driver, including GetCapacity, ListVolumes, GetSnapshot and mutable attributes;
the runner does not filter individual failures. CI runs the same command before
image publication and propagates its exit code. Its loops, container and scratch
volumes were removed after completion. The local log is
`test-results/csi-sanity.log`; CI retains command output in the job log.

## Packaging and isolated cluster smoke

Validated locally on 2026-10-13:

- Docker multi-stage image built successfully with Rust 1.90 / Debian Bookworm.
  The runtime contains mount/umount/blkid/blockdev and XFS/ext4 resize tools. Its
  CLI rejects missing controller settings and an empty node identity. The image
  was rebuilt and smoke-tested after the final node changes.
- With a read-only root filesystem, node mode returned HTTP 200 for `/healthz`
  and `/readyz`. Controller mode as UID/GID 65532 returned health 200 and readiness
  503 when the Axo endpoint was unavailable. `deploy/test_image.py` repeats these
  checks; CI runs them before publishing the image.
- Helm default and final-phase lint, chart packaging, eight deployment-contract
  tests and deterministic render comparison passed. Unsupported resource kinds
  are rejected explicitly before ordering the generated bundles.
- Actionlint 1.7.12 accepted the GitHub Actions workflow. Its pinned action SHAs
  were resolved from the official repositories' release tags. The image job
  depends on the separate Linux storage test job; publication is a final
  push-only job with `packages: write`. Pull-request jobs have read-only token
  permissions and do not authenticate to the registry.
- A local image export/load check preserved the tested image ID. Corrupted
  archive checksums and mismatched image IDs were rejected. The publication job
  loads this checked artifact without rebuilding; automatic cancellation is
  limited to pull requests, and automatic `latest` tagging is disabled.
- Sidecar versions were verified against their upstream release tags. Their
  explicit feature flags match the supported contracts: no topology/capacity
  publication, VolumeAttributesClass or group snapshots. Provisioner v5.3.0
  accepts `Topology=false` with a GA feature-gate warning; its gate is not
  locked to the default. These checks do not replace a full sidecar reconciliation
  test against the future Axo volume API.
  Runtime flag checks also accepted `--default-fstype=xfs` and
  `--handle-volume-inuse-error=false`. Packaging tests check these flags, the
  controller preflight without sidecars, digest/private-image settings,
  rejection of misspelled values and renderer inheritance of chart defaults.
- All three generated manifest phases (`stage`, `attach`, `final`) passed
  **server-side dry-run** on `csi-migration-tests`. None of those bundles was
  applied to the cluster.

During initial implementation validation, a real Docker-built driver and the
compiled `examples/csi-probe.rs` ran in a
throwaway Debian Bookworm pod on `csi-migration-tests`, in a temporary namespace.
The pod had no service-account token and only emptyDir
volumes. Device discovery, state and kubelet paths pointed to isolated test
directories. No hostPath, registrar, CSIDriver, StorageClass, PV or PVC was created.

Observed responses:

```text
GET /healthz: HTTP/1.1 200 OK
GET /readyz: HTTP/1.1 200 OK
driver=smoke.csi.clever-cloud.com version=0.1.0 ready=true
node_id=smoke-vm-incarnation
SIGTERM removed CSI socket
```

The gRPC probe exercised GetPluginInfo, Probe and NodeGetInfo through the Unix
socket. The test namespace was deleted and `kubectl wait --for=delete` confirmed
completion. This validates runtime startup, transport, identity and shutdown on
the cluster; it does not validate a real Axo attachment or a QEMU/Ceph migration.
Local diagnostic notes are in the ignored `test-results/packaging-live-smoke.txt`.

A further isolated test used the final driver with a real Kubernetes 1.36.3
kubelet, external-attacher 4.10.0 and node-driver-registrar 2.15.0, under a unique
CSI name and namespace. An ext4 volume was writable by UID 1000 with fsGroup
2000; its root was `0:2000` mode `2775`, and its file was `1000:2000`. A checksum
survived detach/reattach. A raw Block volume preserved an 18-byte marker and its
device alias was a real bind mount, not a symlink. Kubelet statistics reported
108974080 bytes for the filesystem and 134217728 bytes for Block. The first cycle
also restarted the node plugin with both publications active and preserved their
data. The final cycle repeated the filesystem, block, reattachment and statistics
checks after the last source changes. Cleanup left no owned loop, test mount,
backing file or node registry entry. Test PVs, PVCs, pods, VolumeAttachments,
CSIDriver and RBAC objects were deleted, and the test entry was removed from
CSINode while preserving existing drivers. The test namespace was also removed;
a final API lookup confirmed its absence.

This exercises kubelet and sidecar reconciliation against the real driver, but
still uses a local Axo fixture, fresh loop devices and synthetic sysfs transport
identity. It does not validate real Axo, Supernova, QEMU, Ceph or migration fencing.

The separate `clever-priv` kernel variant follows the existing r100/r200
wrappers and uses their shared configuration and installation hooks. Its three
extra symbols were checked against the existing configuration. A full Exherbo
Linux build, effective Kconfig validation, installation and KVM guest
hotplug/resize remain release checks; these have not been run here.
