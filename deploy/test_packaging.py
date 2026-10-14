#!/usr/bin/env python3
"""Validate deployment boundaries that protect a cluster during CSI takeover."""
import subprocess
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import yaml
from render import ROOT, bundle, render


class DeploymentContract(unittest.TestCase):
    def test_bundles_reject_shared_or_unknown_resources_before_sorting(self):
        for kind in ["Secret", "StorageClass", "CSIDriver", "Namespace", "CustomResourceDefinition", "UnexpectedKind"]:
            with self.subTest(kind=kind), patch("render.render", return_value=[{"kind": kind, "metadata": {"name": "shared"}}]):
                with self.assertRaisesRegex(ValueError, f"unsupported plugin-owned resource {kind}/shared"):
                    bundle("stage")

    def test_stage_cannot_take_over_legacy_controller_or_shared_objects(self):
        docs = render("stage")
        self.assertEqual({d["kind"] for d in docs}, {"ServiceAccount", "DaemonSet"})
        node = next(d for d in docs if d["kind"] == "DaemonSet")["spec"]["template"]["spec"]
        self.assertFalse(node["automountServiceAccountToken"])
        self.assertEqual(node["terminationGracePeriodSeconds"], 210)
        expression = node["affinity"]["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"][0]["matchExpressions"][0]
        self.assertEqual(expression, {"key": "clever-cloud.com/csi", "operator": "In", "values": ["supernova"]})
        self.assertFalse(any("secret" in volume for volume in node["volumes"]))
        identity = next(v for v in node["volumes"] if v["name"] == "node-id")
        self.assertEqual(identity["hostPath"], {"path": "/etc/clever-csi/node-id", "type": "File"})
        driver = node["containers"][0]
        self.assertIn("--node-id-file=/etc/clever-csi/node-id", driver["args"])
        self.assertIn("--kubelet-root=/var/lib/kubelet", driver["args"])
        self.assertEqual(driver["readinessProbe"]["httpGet"]["path"], "/readyz")
        self.assertNotIn("fieldRef", str(node))

    def test_controller_requires_explicit_ownership_handoff(self):
        result = subprocess.run(["helm", "template", "test", str(ROOT / "charts/clever-csi-driver"), "--set", "controller.enabled=true"], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ownershipConfirmed", result.stderr)

    def test_controller_preflight_has_no_mutating_sidecar(self):
        output = subprocess.check_output(["helm", "template", "test", str(ROOT / "charts/clever-csi-driver"),
            "--set", "controller.enabled=true,controller.sidecars.attacher=false",
            "--set-string", "controller.clusterId=kubernetes_test",
            "--set-string", "controller.organisationId=orga_test"], text=True)
        deployment = next(doc for doc in yaml.safe_load_all(output) if doc and doc["kind"] == "Deployment")
        self.assertEqual([container["name"] for container in deployment["spec"]["template"]["spec"]["containers"]], ["driver"])

    def test_each_phase_has_only_the_requested_sidecars(self):
        for phase, expected in [("attach", {"driver", "csi-attacher"}), ("final", {"driver", "csi-attacher", "csi-provisioner", "csi-resizer", "csi-snapshotter"})]:
            with self.subTest(phase=phase):
                docs = render(phase)
                self.assertTrue(all(d["kind"] not in {"Namespace", "Secret", "CSIDriver", "StorageClass", "CustomResourceDefinition"} for d in docs))
                deployment = next(d for d in docs if d["kind"] == "Deployment")
                self.assertEqual(deployment["spec"]["template"]["spec"]["terminationGracePeriodSeconds"], 210)
                containers = deployment["spec"]["template"]["spec"]["containers"]
                self.assertEqual({c["name"] for c in containers}, expected)
                for container in containers[1:]:
                    self.assertIn("--leader-election-namespace=kube-system", container["args"])
                    if container["name"] == "csi-provisioner":
                        self.assertIn("--default-fstype=xfs", container["args"])
                    if container["name"] == "csi-resizer":
                        self.assertIn("--handle-volume-inuse-error=false", container["args"])
                for doc in docs:
                    for rule in doc.get("rules", []):
                        self.assertNotIn("*", rule["verbs"])
                        self.assertNotIn("*", rule["resources"])
                role = next(d for d in docs if d["kind"] == "Role")
                secrets = next(r for r in role["rules"] if r["resources"] == ["secrets"])
                self.assertEqual(secrets["verbs"], ["get"])
                self.assertEqual(secrets["resourceNames"], ["csi-rbd-secret"])

    def test_renderer_inherits_chart_defaults_and_preserves_explicit_overrides(self):
        results = ROOT / "test-results"
        results.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=results) as directory:
            temporary = Path(directory)
            shutil.copytree(ROOT / "charts", temporary / "charts")
            (temporary / "deploy").mkdir()
            shutil.copy(ROOT / "deploy/values-attach.yaml", temporary / "deploy")
            values_path = temporary / "charts/clever-csi-driver/values.yaml"
            values = yaml.safe_load(values_path.read_text())
            values["image"]["tag"] = "changed-default"
            values["controller"]["axoUrl"] = "https://changed.example.test"
            values_path.write_text(yaml.safe_dump(values))
            with patch("render.ROOT", temporary):
                for settings, tag, url in [({}, "changed-default", "https://changed.example.test"),
                        ({"image_tag": "explicit", "axo_url": "https://explicit.example.test"}, "explicit", "https://explicit.example.test")]:
                    docs = render("attach", **settings)
                    deployment = next(doc for doc in docs if doc["kind"] == "Deployment")
                    driver = deployment["spec"]["template"]["spec"]["containers"][0]
                    self.assertTrue(driver["image"].endswith(":" + tag))
                    self.assertIn("--axo-url=" + url, driver["args"])

    def test_common_resources_remain_compatible_for_every_sidecar(self):
        common = {"requests": {"cpu": "75m", "memory": "192Mi"}, "limits": {"memory": "768Mi"}}
        with tempfile.TemporaryDirectory() as directory:
            values_file = Path(directory) / "cluster.yaml"
            values_file.write_text(yaml.safe_dump({"sidecarResources": common}))
            docs = render("final", values_files=[values_file])
        sidecars = [container for doc in docs if doc["kind"] in {"Deployment", "DaemonSet"}
                    for container in doc["spec"]["template"]["spec"]["containers"]
                    if container["name"] != "driver"]
        self.assertEqual(len(sidecars), 5)
        for container in sidecars:
            with self.subTest(container=container["name"]):
                self.assertEqual(container["resources"], common)

    def test_role_overrides_are_partial_independent_and_layered(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory) / "base.yaml"
            cluster = Path(directory) / "cluster.yaml"
            base.write_text(yaml.safe_dump({
                "sidecarResources": {"requests": {"cpu": "25m", "memory": "128Mi"}, "limits": {"memory": "512Mi"}},
                "sidecarResourcesByRole": {
                    "attacher": {"limits": {"memory": "768Mi"}},
                    "provisioner": {"requests": {"memory": "256Mi"}},
                    "resizer": {"limits": {"memory": "640Mi"}},
                    "snapshotter": {"requests": {"cpu": "40m"}},
                    "registrar": {"requests": {"memory": "32Mi"}},
                },
            }))
            cluster.write_text(yaml.safe_dump({
                "sidecarResourcesByRole": {
                    "attacher": {"limits": {"memory": "1Gi"}},
                    "registrar": {"limits": {"memory": "128Mi"}},
                },
            }))
            docs = render("final", values_files=[base, cluster])
        resources = {container["name"]: container["resources"]
                     for doc in docs if doc["kind"] in {"Deployment", "DaemonSet"}
                     for container in doc["spec"]["template"]["spec"]["containers"]
                     if container["name"] != "driver"}
        self.assertEqual(resources, {
            "csi-attacher": {"requests": {"cpu": "25m", "memory": "128Mi"}, "limits": {"memory": "1Gi"}},
            "csi-provisioner": {"requests": {"cpu": "25m", "memory": "256Mi"}, "limits": {"memory": "512Mi"}},
            "csi-resizer": {"requests": {"cpu": "25m", "memory": "128Mi"}, "limits": {"memory": "640Mi"}},
            "csi-snapshotter": {"requests": {"cpu": "40m", "memory": "128Mi"}, "limits": {"memory": "512Mi"}},
            "registrar": {"requests": {"cpu": "25m", "memory": "32Mi"}, "limits": {"memory": "128Mi"}},
        })

    def test_renderer_cli_applies_cluster_values_and_checks_selected_phase(self):
        with tempfile.TemporaryDirectory() as directory:
            values_file = Path(directory) / "cluster.yaml"
            output = Path(directory) / "rendered"
            values_file.write_text(yaml.safe_dump({
                "image": {"tag": "values-tag"},
                "controller": {"axoUrl": "https://values.example.test"},
                "sidecarResourcesByRole": {"attacher": {"limits": {"memory": "1Gi"}}},
            }))
            command = ["python3", str(ROOT / "deploy/render.py"), "--output", str(output),
                       "--phase", "attach", "--values", str(values_file),
                       "--image-tag", "flag-tag", "--axo-url", "https://flag.example.test"]
            subprocess.run(command, check=True, capture_output=True, text=True)
            self.assertEqual({path.name for path in output.iterdir()}, {"attach"})
            docs = [yaml.safe_load(path.read_text()) for path in (output / "attach").glob("*.yaml")]
            containers = next(doc for doc in docs if doc["kind"] == "Deployment")["spec"]["template"]["spec"]["containers"]
            self.assertTrue(containers[0]["image"].endswith(":flag-tag"))
            self.assertIn("--axo-url=https://flag.example.test", containers[0]["args"])
            self.assertEqual(containers[1]["resources"]["limits"]["memory"], "1Gi")
            subprocess.run(command + ["--check"], check=True, capture_output=True, text=True)

    def test_values_reject_mistakes_and_allow_pinned_private_images(self):
        base = ["helm", "template", "test", str(ROOT / "charts/clever-csi-driver")]
        for value in ["controller.sidecars.provisoner=true", "sidecarImages.attacher=12", "node.resources=bad", "imagePullSecrets=secret",
                      "sidecarResourcesByRole.attacher=bad", "sidecarResourcesByRole.provisoner.limits.memory=1Gi",
                      "sidecarResourcesByRole.registrar.limits.memory=-1"]:
            with self.subTest(value=value):
                result = subprocess.run(base + ["--set", value], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
        digest = "sha256:" + "a" * 64
        output = subprocess.check_output(base + ["--set-string", "image.digest=" + digest,
            "--set-string", "imagePullSecrets[0].name=registry-credential"], text=True)
        node = next(doc for doc in yaml.safe_load_all(output) if doc and doc["kind"] == "DaemonSet")["spec"]["template"]["spec"]
        self.assertEqual(node["imagePullSecrets"], [{"name": "registry-credential"}])
        self.assertTrue(node["containers"][0]["image"].endswith("@" + digest))

    def test_vendor_output_is_deterministic_and_ownership_neutral(self):
        for phase in ["stage", "attach", "final"]:
            first = bundle(phase)
            self.assertEqual(first, bundle(phase))
            self.assertEqual(set(first["manifest.txt"].splitlines()[1:]), set(first) - {"manifest.txt"})
            self.assertFalse(any("app.kubernetes.io/managed-by:" in data or "helm.sh/chart:" in data for data in first.values()))


if __name__ == "__main__":
    unittest.main()
