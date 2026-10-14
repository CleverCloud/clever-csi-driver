"""Fault-injection checks for the local Axo fixture; no block devices required."""
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import call, patch

import fake_axo


class ExpansionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.volume = {
            "id": "vol-test", "capacity": 4096,
            "shape": {"volume_mode": "block"}, "source": None,
            "attach": {"loop": "/dev/test-owned-loop"},
        }
        self.enterContext(patch.object(fake_axo, "ARGS", SimpleNamespace(data=self.directory.name)))
        self.enterContext(patch.object(fake_axo, "STATE", {"volumes": {"vol-test": self.volume}}))
        self.path = Path(fake_axo.backing("vol-test"))
        self.path.write_bytes(b"preserved payload" + bytes(4096 - 17))
        self.request = {"volume_id": "vol-test", "required_bytes": 8192, "limit_bytes": 8192}

    def test_retry_refreshes_attached_device_after_backing_growth(self):
        with patch.object(fake_axo, "run", side_effect=RuntimeError("refresh interrupted")):
            with self.assertRaisesRegex(RuntimeError, "refresh interrupted"):
                fake_axo.expand(self.request)
        self.assertEqual(self.path.stat().st_size, 8192)
        self.assertEqual(self.volume["capacity"], 8192)
        with patch.object(fake_axo, "run", side_effect=["", "8192"]) as run:
            result = fake_axo.expand(self.request)
        self.assertEqual(run.call_args_list, [
            call("losetup", "--set-capacity", "/dev/test-owned-loop"),
            call("blockdev", "--getsize64", "/dev/test-owned-loop"),
        ])
        self.assertEqual(result["capacity_bytes"], 8192)
        self.assertEqual(self.path.read_bytes()[:17], b"preserved payload")

    def test_success_waits_for_observed_device_capacity(self):
        with patch.object(fake_axo, "run", side_effect=["", "4096"]):
            with self.assertRaisesRegex(RuntimeError, "has not observed"):
                fake_axo.expand(self.request)
        with patch.object(fake_axo, "run", side_effect=["", "8192"]):
            self.assertEqual(fake_axo.expand(self.request)["capacity_bytes"], 8192)

    def test_limit_is_enforced_before_any_side_effect(self):
        for required, limit in [(8192, 6144), (2048, 2048)]:
            with self.subTest(required=required, limit=limit):
                with patch.object(fake_axo, "run") as run:
                    with self.assertRaises(fake_axo.ApiError) as error:
                        fake_axo.expand({**self.request, "required_bytes": required, "limit_bytes": limit})
                self.assertEqual(error.exception.status, 422)
                run.assert_not_called()
                self.assertEqual(self.volume["capacity"], 4096)
                self.assertEqual(self.path.stat().st_size, 4096)

    def test_unattached_growth_and_retry_preserve_backing_contents(self):
        self.volume["attach"] = None
        with patch.object(fake_axo, "run") as run:
            first = fake_axo.expand(self.request)
            second = fake_axo.expand(self.request)
        run.assert_not_called()
        self.assertEqual(first, second)
        self.assertEqual(first["capacity_bytes"], 8192)
        self.assertEqual(os.path.getsize(self.path), 8192)
        self.assertEqual(self.path.read_bytes()[:17], b"preserved payload")


if __name__ == "__main__":
    unittest.main()
