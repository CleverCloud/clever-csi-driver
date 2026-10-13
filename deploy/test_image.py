#!/usr/bin/env python3
"""Smoke-test the real runtime image; creates only disposable local containers."""
import argparse
from pathlib import Path
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def docker(*args, check=True):
    return subprocess.run(["docker", *args], capture_output=True, text=True,
                          check=check, timeout=30)


def status(port, endpoint):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/{endpoint}", timeout=1) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


def smoke(image):
    results = Path(__file__).resolve().parents[1] / "test-results"
    results.mkdir(exist_ok=True)
    help_text = docker("run", "--rm", "--network=none", image, "--help").stdout
    assert "--node-id-file" in help_text and "--organisation-id" in help_text
    for arguments in [["--mode=controller"], ["--mode=node", "--node-id="]]:
        result = docker("run", "--rm", "--network=none", image, *arguments, check=False)
        assert result.returncode != 0, f"invalid settings accepted: {arguments}"
    docker("run", "--rm", "--network=none", "--entrypoint", "sh", image, "-c",
           'for tool in mount umount blkid blockdev xfs_growfs resize2fs; do command -v "$tool" || exit 1; done')
    for mode in ["node", "controller"]:
        with tempfile.TemporaryDirectory(dir=results) as directory:
            token = Path(directory) / "token"
            token.write_text("fixture-token-not-a-credential")
            token.chmod(0o444)
            args = ["run", "--detach", "--rm", "--read-only", "--publish", "127.0.0.1::9808",
                    "--tmpfs", "/csi:uid=65532,gid=65532,mode=0777", "--tmpfs", "/var/lib/clever-csi",
                    "--tmpfs", "/run", "--tmpfs", "/tmp"]
            cli = ["--mode=" + mode]
            if mode == "node":
                cli += ["--node-id=11111111-1111-4111-8111-111111111111"]
            else:
                args += ["--user", "65532:65532", "--mount", f"type=bind,src={token},dst=/token,readonly"]
                cli += ["--organisation-id=orga_test", "--cluster-id=kubernetes_test",
                        "--axo-url=https://127.0.0.1:1", "--axo-token-file=/token"]
            container = docker(*args, image, *cli).stdout.strip()
            try:
                port = docker("port", container, "9808/tcp").stdout.strip().rsplit(":", 1)[1]
                for _ in range(50):
                    try:
                        if status(port, "healthz") == 200:
                            break
                    except (OSError, urllib.error.URLError):
                        pass
                    time.sleep(0.1)
                assert status(port, "healthz") == 200
                expected = 200 if mode == "node" else 503
                assert status(port, "readyz") == expected
                print(f"{mode}: read-only rootfs, health=200, ready={expected}")
            finally:
                docker("stop", "--timeout", "2", container, check=False)
    print("runtime image smoke checks passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", nargs="?", default="clever-csi-driver:dev")
    smoke(parser.parse_args().image)
