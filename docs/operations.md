# Deploying the Clever CSI driver

The chart and generated bundles package the driver; they do not implement or
activate Axo/Supernova's required volume API. Deploy to a disposable validation
cluster first. The default image tag is `v0.1.0`, which must be built and published
before installation; an unbuilt release is not made available by these files.

## Images and local verification

```sh
cargo test --locked --all-targets
python3 -m pip install -r deploy/requirements.txt
helm lint charts/clever-csi-driver
python3 deploy/test_packaging.py
python3 deploy/render.py --check
docker build --tag clever-csi-driver:test .
python3 deploy/test_image.py clever-csi-driver:test
# Requires a Docker daemon allowed to run disposable privileged containers:
bash deploy/test_linux.sh
```

The multi-stage image compiles with Rust 1.90 on Debian Bookworm and includes
`mount`, `umount`, `blkid`, XFS and ext4 resize tools in its runtime. The node
service runs privileged; the controller runs as UID/GID 65532 with a read-only
root filesystem. The node mounts `/sys` writable solely to rescan the verified
SCSI device during expansion. Device identity is validated before that sysfs
write. The node never needs Ceph clients or Ceph credentials. It
refuses to format unknown disks: Axo must supply a preformatted XFS/ext4 image
for Filesystem volumes, while raw Block volumes are never formatted.

GitHub Actions runs formatting, Clippy, Rust tests, Helm lint and deployment
contract tests, checks the deterministic bundles, runs the real Linux storage tests in a disposable privileged Docker container,
then builds `linux/amd64` and smoke-tests the real runtime before publication.
The Linux test source is read-only and its scratch/target directories are private
Docker volumes. It creates only new file-backed loop devices for XFS/ext4/raw
Block tests; the test runner cleans its container and temporary volumes.
Pushes to `main` and `v*` tags publish
`ghcr.io/clevercloud/clever-csi-driver` using `GITHUB_TOKEN` with `packages: write`.
Pull requests build without registry login or publication. A `v0.1.0` release
produces both `:v0.1.0` and `:0.1.0`; main publishes `:main`, plus a commit tag.
The repository must have permission to create/write this package in the
CleverCloud organization. This implementation does not push a release itself.

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

Before `stage`, update the legacy ceph-csi DaemonSet in Axo's catalog to require
`clever-cloud.com/csi NotIn [supernova]`. The new DaemonSet requires `In
[supernova]`; the absence of the label matches only the legacy plugin. Ensure
these requirements survive Axo reconciliation. Never relabel a live node to
switch implementations: the socket and registrar identity are shared. A fresh
node receives its label and profile at creation. The new DaemonSet uses
`OnDelete` so a chart update does not silently restart all mounted node plugins.

Existing ceph-csi CSINodes can report a Kubernetes Node name as NodeID. Axo must
explicitly route those legacy attachments as `legacy-krbd` while routing new VM
incarnation identities as `qemu`. Never resolve a name or IP to a guessed QEMU
instance. The backend must advertise and implement this mixed-pool contract
before the new attacher takes over the cluster.

Before `attach`, stop/remove the **legacy attacher** in the catalog, verify its
process is stopped and its lease released, deploy the new controller, and
confirm the new attacher acquires the expected lease before any node roll. Do
not depend on leader election to coordinate two different controller
implementations: a legacy sidecar must not reacquire the role later. Other
legacy roles remain active until their explicit handoff. All gRPC sidecar
operation timeouts are 180 seconds; Axo's operation budget must be shorter and
its timeout/cancellation contract must prevent a late attach after detach.
Both pod roles default to a 210-second termination grace period, exceeding the
180-second sidecar and 120-second Axo/node-command budgets. Keep the grace period
above the longest operation deadline when adjusting those budgets.

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
resizer role with a controller able to serve both pools. Neither the chart nor
Helm provides that coordination.

Before `final`, adopt existing images, outstanding reservations and snapshot/
clone journals, validate create/delete/expand/snapshot/restore and rollback,
then stop the remaining legacy sidecars and verify each lease before enabling
its new counterpart. `deploy/values-final.yaml` is only for this completed
handoff; it does not install snapshot CRDs or snapshot-controller. Revoke/rotate
old cephx credentials in Ceph after all legacy users are gone; emptying a Secret
does not revoke copied credentials. Preserve Secrets still referenced by PVs
and kubelet even when their sensitive contents are no longer needed.

Readiness `/readyz` on port 9808 checks driver readiness; liveness `/healthz`
checks the service. These are not proof that kubelet has registered the plugin
or that a disk was safely handed over. Check CSINode identity, registrar logs,
VolumeAttachments, Axo state and a real disposable PVC/pod separately.

Rollback requires a new node generation and a cold handoff to the legacy path,
with consistent credentials, journals and fencing. Reversing a label or rolling
back a Helm release cannot safely move an attached disk.

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

To render fully concrete bundles for a test cluster without placeholders:

```sh
python3 deploy/render.py --output ./test-results/manifests \
  --cluster-id kubernetes_example --organisation-id orga_example \
  --axo-url https://axo.example.test --image-tag lab
```

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
