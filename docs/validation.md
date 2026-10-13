# Validation

## Rust and real Linux storage tests

Final local checks on 2026-10-13 passed: `cargo fmt --all -- --check`,
`cargo clippy --all-targets --locked -- -D warnings` and
`cargo test --all-targets --locked`. The ordinary test run passed **62 tests**
(35 library, 26 controller contract and one gRPC transport), with the privileged
Linux test explicitly excluded and run separately as described below. The final
OpenAPI 3.1 document also passed `openapi-spec-validator` validation.

The Rust unit and contract tests exercise persistent node-state recovery,
device identity, mount ownership, read-only access, malformed requests,
controller idempotency keys, Biscuit rotation, response validation, timeouts
and sanitized backend errors. A separate test uses a real Unix-domain socket
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
changing the writable consumer. This suite was rerun on the final node sources.

The backing files and mounts were isolated to the test container. The runner
cleaned its owned loop devices, container and temporary Docker volumes; absence
of remaining test resources was checked. The test is ignored in an ordinary
`cargo test` run and executed explicitly by the `linux-mounts` CI job before
image publication. SCSI VPD serial discovery used a sysfs fixture backed by
real loop devices; this does **not** validate a QEMU virtio-scsi attachment.

Controller contract tests use a mock Axo HTTP server. The proposed routes and
OpenAPI document describe backend work still required in Axo and Supernova;
there has been no end-to-end Ceph provisioning, real VM fencing, legacy volume
migration or billing validation against those services.

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
- Helm default and final-phase lint, chart packaging, five deployment-contract
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

The separate `clever-priv` kernel variant follows the existing r100/r200
wrappers and uses their shared configuration and installation hooks. Its three
extra symbols were checked against the existing configuration. A full Exherbo
Linux build, effective Kconfig validation, installation and KVM guest
hotplug/resize remain release checks; these have not been run here.
