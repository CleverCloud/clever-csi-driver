#!/usr/bin/env python3
"""Validate deployment boundaries that protect a cluster during CSI takeover."""
import subprocess
import unittest
from render import ROOT, bundle, render


class DeploymentContract(unittest.TestCase):
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
                for doc in docs:
                    for rule in doc.get("rules", []):
                        self.assertNotIn("*", rule["verbs"])
                        self.assertNotIn("*", rule["resources"])
                role = next(d for d in docs if d["kind"] == "Role")
                secrets = next(r for r in role["rules"] if r["resources"] == ["secrets"])
                self.assertEqual(secrets["verbs"], ["get"])
                self.assertEqual(secrets["resourceNames"], ["csi-rbd-secret"])

    def test_vendor_output_is_deterministic_and_ownership_neutral(self):
        for phase in ["stage", "attach", "final"]:
            first = bundle(phase)
            self.assertEqual(first, bundle(phase))
            self.assertEqual(set(first["manifest.txt"].splitlines()[1:]), set(first) - {"manifest.txt"})
            self.assertFalse(any("app.kubernetes.io/managed-by:" in data or "helm.sh/chart:" in data for data in first.values()))


if __name__ == "__main__":
    unittest.main()
