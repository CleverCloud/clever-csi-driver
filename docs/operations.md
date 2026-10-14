# Deploying the Clever CSI driver

The chart and generated bundles package the driver; they do not implement or
activate Axo/Supernova's required volume API. Deploy to a disposable validation
cluster first. The default image tag is `v0.1.0`, which must be built and published
before installation; an unbuilt release is not made available by these files.

This guide describes the current binary and chart, whose controller uses the
[current `clever-csi-v1` HTTP contract](axo-api-v1.md) (implementation baseline `339e7f0`).
The [REST proposal dated 2026-10-13](axo-api.md) is not implemented by this
client or Axo. Its dated version header, resource routes, response schemas and
capability negotiation require the [client migration](axo-api.md#client-migration)
before these deployment phases can target a server implementing that proposal.
Changing documentation or chart values does not switch the client's protocol.

## Images and local verification

```sh
cargo test --locked --all-targets
python3 -m pip install -r deploy/requirements.txt
helm lint charts/clever-csi-driver
python3 deploy/test_packaging.py
python3 deploy/render.py --check
python3 deploy/test_api_contract.py
docker build --tag clever-csi-driver:test .
python3 deploy/test_image.py clever-csi-driver:test
# Requires a Docker daemon allowed to run disposable privileged containers:
bash deploy/test_linux.sh
# CSI conformance with a test-only combined server and a local Axo fixture:
bash deploy/test_linux.sh sanity
```

The multi-stage image compiles with Rust 1.90 on Debian Bookworm and includes
`mount`, `umount`, `blkid`, `blockdev`, XFS and ext4 resize tools in its runtime.
The node service runs privileged; the controller runs as UID/GID 65532 with a read-only
root filesystem. The node mounts `/sys` writable solely to rescan the verified
SCSI device during expansion. Device identity is validated before that sysfs
write. The node never uses Ceph clients or Ceph credentials. Kubelet still sends
credentials from existing PV `node-stage-secret` references; the driver ignores
these request fields. Keep referenced Secrets until their consumers are retired. It
refuses to format unknown disks: Axo must supply a preformatted XFS/ext4 image
for Filesystem volumes, while raw Block volumes are never formatted.

GitHub Actions runs formatting, Clippy, Rust tests, Helm lint and deployment
contract tests, checks the deterministic bundles, runs the real Linux storage
tests in a disposable privileged Docker container, then builds `linux/amd64`
and smoke-tests the real runtime before publication.
The tested image is transferred to the publication job as an artifact, checked
against its recorded image identity and published without a second build.
The Linux test source is read-only and its scratch/target directories are private
Docker volumes. It creates only new file-backed loop devices for XFS/ext4/raw
Block tests; the test runner cleans its container and temporary volumes.
Pushes to `main` and `v*` tags publish
`ghcr.io/clevercloud/clever-csi-driver` using `GITHUB_TOKEN` with `packages: write`.
Pull requests build without registry login or publication. A `v0.1.0` release
produces both `:v0.1.0` and `:0.1.0`; main publishes `:main`, plus a commit tag.
No automatic `latest` tag is generated. Runs on the same release reference are
serialized; automatic cancellation is limited to pull requests, so another push
does not interrupt an active publication.
The repository must have permission to create/write this package in the
CleverCloud organization. This implementation does not push a release itself.
Make the package readable by the cluster before installation: either publish it
publicly or configure `imagePullSecrets` for both workloads. Tags, including
commit tags, are mutable across rebuilt workflow runs. For rollout, record and
set `image.digest=sha256:<registry-digest>`; it overrides `image.tag`. Use the
registry digest of the tested image, not its local Docker image ID.

## Mount and recovery policy

In the driver's mount namespace, filesystem mounts include `nosuid,nodev`; raw Block binds include
`nosuid` without `nodev`. `noexec` is applied only when requested. Mount options
that weaken these defaults, including `suid` and `dev`, are rejected. Workloads
that depend on setuid executables or device files within a filesystem volume
must be reviewed before moving from the legacy plugin. These are driver-side
mount checks, not a guarantee of the final flags inside every pod: the container
runtime can set different flags on its own bind mount. Inspect the actual pod
mount and enforce workload security through the cluster's admission/runtime
policy. Retaining a volume's
data and handle does not imply identical mount options to the legacy deployment.

For filesystems, effective read-only access combines the attachment, volume
capability, mount options and individual publication request. A false
`NodePublishVolume.readonly` value never overrides an existing read-only
constraint. Read-only Block publication additionally requires an attachment
marked read-only by Axo and a guest block device reporting read-only. A request
for a read-only Block publication on a writable attachment is rejected: a bind
mount flag alone does not prevent device writes. The plugin does not set the
whole device read-only as a side effect of one pod's request.

Before device discovery or mounting, Stage and Publish require the attachment's
`node_id` to match the node's exact VM incarnation ID. A publish context from a
previous incarnation is rejected even when its serial matches a current disk.
The serial identifies a disk within that incarnation; it is not sufficient
authorization on its own. An older publish context without `node_id` is also
rejected. When upgrading an earlier test deployment, stop the affected consumers
and complete a normal detach/reattach with the updated controller before
restaging; do not manufacture a context or edit a VolumeAttachment to bypass
the check. After device re-enumeration, the plugin can update
its persisted path and device number only after checking that the
old staging and published mounts are gone and the new device has no conflicting
mounts. Conflicting identities and mounted devices still cause refusal. Raw
Block checks inspect the block inode's device number as well as mountinfo.
Do not delete the state registry to bypass an ownership failure.
If recovery refuses a volume, stop its consumers, preserve the registry and
collect device serials, mount tables and Axo attachment state. Resolve the
ownership discrepancy or replace the node through a fenced handoff; there is no
supported manual registry-editing command. Even an exclusive-open check cannot
prove that every raw Block file descriptor in another process is closed. Backend
writer fencing remains required before detaching or transferring ownership.

Unpublish removes the driver's target file or empty directory after unmounting
and verifying ownership. It does not recursively remove data or unrelated
targets. The staging directory remains owned by kubelet after Unstage.

The QEMU path supports XFS/ext4 and single-node writer/reader access only. Its
mount-option allowlist is `ro`, `rw`, `nosuid`, `nodev`, `noexec`, `noatime`,
`relatime`, `strictatime`, `nodiratime`, `sync`, `dirsync`, `discard`, `nodiscard`,
plus `nouuid` for XFS. Conflicting options, encryption, `rbd-nbd`, other
filesystems and shared multi-node access must not be migrated to this path.
A read-only filesystem with an unreplayed journal also requires preparation
under exclusive writable ownership; `norecovery`/`noload` are not supported.

Changes to non-security mount options are deferred until a complete Unstage
and a new Stage. Retries, restarts and device re-enumeration retain the options
actually applied to the existing stage. Changes to read-only or `noexec` policy
require proving that no old staging or published mounts remain. A persisted
publication intent may change its read-only mode only when both its target and
all descendant mounts are absent.

A writable filesystem Stage grows an existing XFS/ext4 filesystem to the
available device capacity, including on retry after an interrupted growth. It
never formats the disk. Read-only and Block stages do not grow a filesystem;
Axo must prepare a larger filesystem clone/restore before a read-only attachment
can become eligible. See the [backend contract](axo-api-v1.md).

## Inputs owned by the platform

- Install `clever/linux-vm:kube` on the relevant hypervisors and select
  `/linux-vm-kube` only for new CKE node generations. The guest needs virtio-scsi,
  hotplug and mounted block-device write support. The `linux-vm` r300 variant in
  `clever-priv` follows the existing r100/r200 package layout and enables
  `SCSI_VIRTIO`, `HOTPLUG_PCI_ACPI` and `BLK_DEV_WRITE_MOUNTED` through
  `extra_config`. It inherits the shared configuration and installation hooks;
  a valid existing `/kernel` symlink is preserved, while other cases follow the
  standard kernel hook and require checking before installation. Check the
  effective build configuration and boot the selected image in a test guest
  before rollout. The package alone does not
  change the kernel selected by Axo for a node generation.
- Create `/etc/clever-csi/node-id` in each guest, containing the exact VM incarnation identifier
  accepted by Axo/Supernova. The chart mounts that existing file with
  `hostPath.type: File`; a missing file prevents startup. A Kubernetes Node name,
  `cluster-vm-id` label or unverified DMI value is not a substitute. Axo must
  check VM incarnation and cluster ownership on every attach.
- Supply the existing `clever-csi-axo` Secret in `kube-system`, key `token`, using
  a short-lived Biscuit scoped to the organisation and cluster. It is mounted
  only into the controller. Axo remains responsible for authorization, durable
  attachment intents, fencing, confirmed shutdown, and token rotation/revocation.
  Secret rotation uses a projected directory, not a `subPath` mount.
- Keep existing `CSIDriver rbd.csi.ceph.com`, StorageClasses, PVs/PVCs, referenced
  Secrets, snapshot CRDs and snapshot-controller under their existing ownership.
  The chart creates or deletes none of these. Standalone clusters need these
  objects supplied explicitly before using the driver.

## Three explicit phases

| Phase | New controller roles | New node plugin |
| --- | --- | --- |
| `stage` / chart defaults | Disabled | Only on `clever-cloud.com/csi=supernova` nodes |
| `attach` | Attacher only | Same new pool |
| `final` | Attacher, provisioner, resizer, snapshotter | Same new pool |

The release name in the generated bundles is `clever-csi`, in `kube-system`.
Keep the namespace aligned with the old sidecars: a leader lease is identified
by both name and namespace. `controller.ownershipConfirmed=true` is a deliberate
operator declaration; Helm cannot determine that the previous controller is
stopped or that a volume is safe to transfer.

These phases are requirements for a future Axo integration, not an executable
cluster upgrade on the current catalog. Axo must persist a per-cluster phase and
per-generation node profile, and render the matching legacy/new controller roles
and node affinities on every reconciliation. Its current shared `CEPH_CSI`
corpus and forced server-side apply will restore a manually removed attacher.
Changing that global corpus alone would affect other clusters. Do not start a
canary until phase selection and reconciliation persistence are implemented.

Inventory PV/PVC modes, access modes, filesystems, mount options, encryption,
mounters, Secret references, volume/snapshot handles and group snapshots before
cutover. Separate workloads that the QEMU path cannot serve and keep them on
legacy nodes. Group snapshots have no replacement handler in this driver;
their outstanding resources must be resolved before removing that legacy role.

Before `stage`, update the legacy ceph-csi DaemonSet in Axo's catalog to require
`clever-cloud.com/csi NotIn [supernova]`. The new DaemonSet requires `In
[supernova]`; the absence of the label matches only the legacy plugin. Ensure
these requirements survive Axo reconciliation. Never relabel a live node to
switch implementations: the socket and registrar identity are shared. A fresh
node receives its label and profile at creation. The new DaemonSet uses
`OnDelete` to defer pod replacement for image and other pod-template updates.
**It does not protect a pod whose node stops matching the DaemonSet's required
affinity**: the DaemonSet controller can delete that pod immediately, including
when an affinity change makes its node ineligible.
The platform must create that label and the incarnation file together; neither
the chart nor the current catalog produces the node profile. Once node plugins
have been deployed, treat `node.poolLabel`, `node.poolValue`, the complementary
legacy exclusion and the corresponding node labels as immutable on active
nodes. Enforce this in Axo's persisted profile and reconciliation/admission
policy. An offline Helm render cannot compare against live node eligibility.
Changing these settings requires a separately planned, drained and fenced node
generation transition; updating both affinities in place is not a safe migration.
During `stage`, keep new nodes cordoned and prohibit storage workloads on them.
The old attacher can mark a VolumeAttachment attached without QEMU metadata;
such an attachment is not automatically republished by installing the new driver.

Existing ceph-csi CSINodes can report a Kubernetes Node name as NodeID. Axo must
explicitly route those legacy attachments as `legacy-krbd` while routing new VM
incarnation identities as `qemu`. Never resolve a name or IP to a guessed QEMU
instance. The backend must advertise and implement this mixed-pool contract
before the new attacher takes over the cluster.

Before `attach`, start the new controller with every sidecar disabled and verify
its `/readyz`, CSI Probe and the backend's complete required capability set.
The current client requires its full feature set even for an attacher-only phase;
do not discover an unsupported backend after stopping the old attacher.
For this preflight, set `controller.enabled=true` and all four
`controller.sidecars.*` values to `false`; `ownershipConfirmed` can remain false
because no controller role is being taken over. Supply the normal cluster,
organisation and token settings. `attachment.allowed-transports` must be present:
the backend must enforce the controller's allowed route before mutating a disk.
Then transition this cluster's persisted catalog phase, stop/remove the **legacy
attacher**, verify its process is stopped and its lease has expired, enable the
new attacher, and confirm it acquires the expected lease before any node roll. Do
not depend on leader election to coordinate two different controller
implementations: a legacy sidecar must not reacquire the role later. Other
legacy roles remain active until their explicit handoff. All gRPC sidecar
operation timeouts are 180 seconds; Axo's operation budget must be shorter and
its timeout/cancellation contract must prevent a late attach after detach.
Both pod roles default to a 210-second termination grace period, exceeding the
180-second sidecar and 120-second Axo budgets. Node commands have no independent
kill timeout: an operation survives RPC cancellation until its process exits,
and shutdown waits for those effects. The process lock is inherited by child
commands so a replacement driver cannot clear recovery state while an old
command still runs. A kernel-blocked command can outlive the pod's grace period;
210 seconds, SIGKILL and absence from mountinfo are not proof that writes ended.
Resolve the device/kernel failure or positively fence the old incarnation before
allowing ownership to move.

In `attach`, the legacy provisioner still creates blank images for ordinary new
Filesystem PVCs; ceph-csi normally formats them at its first NodeStage. A newly
allocated but never-staged legacy image therefore cannot be sent to the QEMU
pool. Keep such PVC consumers on legacy nodes until initialization is positively
verified, or hand provisioning to a backend that owns and journals safe initial
formatting before enabling new-pool scheduling. Never infer permission to format
an adopted image from an empty filesystem probe. Raw Block and already formatted
clones have different initialization requirements. This gate must be enforced by
the platform's admission/scheduling flow, not left to a human watching roll logs.

Example for a lab only, once those conditions hold:

```sh
helm upgrade --install clever-csi charts/clever-csi-driver \
  --namespace kube-system -f deploy/values-attach.yaml \
  --set-string controller.clusterId=kubernetes_example \
  --set-string controller.organisationId=orga_example
```

Roll via the platform's persistent per-cluster/per-generation profile. A single
incoming/outgoing node with drain, CSI barriers and pause/resume is a **required
Axo feature**, not an assumption about the currently implemented roll. Transfer
only after the old volume is unmounted and Axo proves release or fences the old
client. An absent Ceph watcher/lock alone does not establish that a partitioned
writer cannot return. Stop on alternating Node boot IDs, duplicate plugins,
uncertain attach results, orphan attachments, or lost ownership proof.

Coexistence needs a defined resize contract: legacy ceph-csi can resize RBD
without telling QEMU. Do not permit resize of already migrated volumes until
Axo updates QEMU capacity as part of that path, or deliberately transfers the
resizer role with a controller able to serve both pools. The platform must reject
size-changing PVC updates for affected volumes while this gate is active, through
admission or an equivalent enforced policy. Neither the chart nor Helm provides
that coordination. The resizer disables its pod-cache/in-use workaround because
the backend contract requires online growth; a backend requiring offline resize
does not satisfy this deployment contract.

Before `final`, the platform must enforce a persistent scheduling/admission gate:
**no workload that can consume a native Clever CSI volume may be scheduled on a
legacy node**. This applies to new PVCs, restores, clones, existing native PVCs,
replacement pods and future node generations. Apply the policy to every such
consumer, including workload controllers, and keep it effective across Axo
reconciliation. A label on the CSI DaemonSet, the identity of the active
provisioner, or `WaitForFirstConsumer` alone does not provide this guarantee:
this driver publishes no CSI topology contract. Use an enforced scheduler or
admission policy that restricts these consumers to the new pool (or finish
retiring the legacy pool before native provisioning). Verify that a disposable
native PVC consumer explicitly targeting a legacy node is rejected, and that its
new-pool counterpart can attach and mount. Axo must also reject a native-volume
attachment routed to `legacy-krbd` before changing any backend state.
**Do not enable the new provisioner or enter `final` until this gate is enforced
and verified.** The gate must remain in force for as long as legacy nodes remain;
an operator declaration or a successful manifest render is not enforcement.

Then adopt existing images, outstanding reservations and snapshot/
clone journals, validate create/delete/expand/snapshot/restore and rollback,
then stop the remaining legacy sidecars and the `csi-rbdplugin-controller`
reconciler, and wait for each prior lease to expire before enabling
its new counterpart. `deploy/values-final.yaml` is only for this completed
handoff; it does not install snapshot CRDs or snapshot-controller. Revoke/rotate
old cephx credentials in Ceph after all legacy users are gone; emptying a Secret
does not revoke copied credentials. Preserve Secrets still referenced by PVs
and kubelet even when their sensitive contents are no longer needed.

Readiness `/readyz` on port 9808 checks driver readiness; liveness `/healthz`
checks the service. These are not proof that kubelet has registered the plugin
or that a disk was safely handed over. Check CSINode identity, registrar logs,
VolumeAttachments, Axo state and a real disposable PVC/pod separately.
The current controller's readiness check validates `clever-csi-v1`; it does not
negotiate or verify the proposed dated REST contract.

Rollback requires a new node generation and a cold handoff to the legacy path,
with consistent credentials, journals and fencing. Reversing a label or rolling
back a Helm release cannot safely move an attached disk.

For a node-plugin image upgrade, `OnDelete` leaves existing pods on their old
image. Cordon and drain one node, verify unpublish/unstage and backend release,
then delete that node's DaemonSet pod. Check its replacement image digest,
registrar, CSINode and disposable volume before uncordoning and moving to the
next node. Never delete all node-plugin pods together to force an update.

## Axo catalog bundles

`python3 deploy/render.py` writes deterministic `deploy/rendered/{stage,attach,final}`
bundles. Each contains numbered YAML files and a `manifest.txt` in apply order.
The renderer removes Helm ownership labels and leaves Axo to stamp its manager
and plugin labels, following the existing Karpenter catalog convention. No
Namespace, Secret, StorageClass, CSIDriver or CRD is included; reverse-order
teardown therefore cannot delete those shared objects.

These outputs are integration inputs, **not a change to Axo's catalog**. A future
Axo PR must register the plugin and its owned GVKs, wire its phase/state gates,
reject adoption of a foreign install, and provide `${OVD_CLUSTER_ID}` plus the
new `${OVD_CSI_ORGANISATION_ID}` substitution. The latter is not registered in
Axo today and must be added to its explicit placeholder allowlist. The API base
URL defaults to `https://api.clever-cloud.com`; override it at render time for a
different deployment. Axo must provision the token Secret independently and
preserve existing shared objects during legacy teardown.

Resource budgets must be stored per cluster in Axo's persisted configuration and
passed into every render/reconciliation; do not vendor one fixed set of limits
for every cluster. `sidecarResources` applies separately to each sidecar
container. `sidecarResourcesByRole` accepts `registrar`, `attacher`, `provisioner`,
`resizer` and `snapshotter`; each entry is merged over the common resources, so a
memory-only override preserves the common CPU request and other settings. The
common default is a 128 MiB request and a 512 MiB limit per sidecar. These are
starting values, **not a supported object-count or memory guarantee**. The
controller sidecars maintain different informer caches, so size their requests
and limits from measurements at the cluster's expected PV/PVC/VolumeAttachment/
snapshot count, including startup/list bursts and recovery. Account for the sum
of all container requests when placing the controller pod. Monitor OOM kills
and memory pressure; a larger limit alone does not establish an adequate budget.

For example, a cluster-specific values file can lower the registrar's allocation
independently of a provisioner measured to require more memory:

```yaml
# cluster-values.yaml: example inputs, to be sized against this cluster's load.
sidecarResourcesByRole:
  registrar:
    requests: {memory: 32Mi}
    limits: {memory: 128Mi}
  provisioner:
    requests: {memory: 256Mi}
    limits: {memory: 1Gi}
```

To render fully concrete bundles for a test cluster without placeholders:

```sh
python3 deploy/render.py --output ./test-results/manifests \
  --cluster-id kubernetes_example --organisation-id orga_example \
  --axo-url https://axo.example.test --image-tag lab \
  --phase attach --values cluster-values.yaml
```

Repeat `--values` to layer files in order; later files override earlier ones and
all override the phase's defaults. Explicit image, API URL and identity flags
take precedence. Without `--phase`, all three phase directories are generated;
`--phase` limits rendering and `--check` to the selected phase(s).

Review and vendor the selected phase into Axo only after implementing those
integration hooks; do not manually apply all three phases together. Changing
roles is a controlled state transition, not an automatic manifest replacement.

## Pinned sidecar sources

The versions match the current ceph-csi catalog and were checked against their
upstream releases:

- [external-attacher v4.10.0](https://github.com/kubernetes-csi/external-attacher/releases/tag/v4.10.0)
- [external-provisioner v5.3.0](https://github.com/kubernetes-csi/external-provisioner/releases/tag/v5.3.0)
- [external-resizer v1.14.0](https://github.com/kubernetes-csi/external-resizer/releases/tag/v1.14.0)
- [external-snapshotter v8.4.0](https://github.com/kubernetes-csi/external-snapshotter/releases/tag/v8.4.0)
- [node-driver-registrar v2.15.0](https://github.com/kubernetes-csi/node-driver-registrar/releases/tag/v2.15.0)

The chart disables provisioner topology and storage-capacity publishing because
the driver advertises no topology contract. With provisioner v5.3.0,
`Topology=false` can emit a GA feature-gate warning but is accepted (the gate is
not locked to its default in the [upstream feature definitions](https://github.com/kubernetes-csi/external-provisioner/blob/v5.3.0/pkg/features/features.go)). VolumeAttributesClass and group snapshots are also
disabled rather than implying support for capabilities the driver does not
implement. Revisit these explicit gates when upgrading sidecars.

The node service account has no API token or RBAC. Controller sidecars have the
permissions required by the enabled roles; Secret reads are confined to named
referenced CSI Secrets in the release namespace. If legacy PVs reference
Secrets elsewhere, define separately reviewed namespace-specific RoleBindings
rather than granting cluster-wide Secret access. The Biscuit itself is mounted
by kubelet and does not need an API Secret-read grant.

Sidecar HTTP metrics endpoints are not enabled by this chart. Add a reviewed
metrics configuration and monitoring integration before relying on sidecar
metrics for an operational acceptance criterion.
