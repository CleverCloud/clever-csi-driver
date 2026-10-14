#!/usr/bin/env python3
"""Stateful test-only Axo implementing the current clever-csi-v1 client contract.

Volumes are sparse files formatted by "Axo" (preformatted policy). An attachment
creates a loop device + a synthetic sysfs entry (device/vpd_pg80 carrying the
serial) + a block node in a fake device root, emulating a QEMU scsi-hd hotplug.
Detach removes them (hot-unplug).
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FEATURES = [
    "volume.ensure", "volume.resolve", "volume.delete", "volume.expand",
    "volume.clone", "volume.restore", "attachment.ensure", "attachment.delete",
    "snapshot.ensure", "snapshot.delete", "snapshot.list",
    "legacy-pool-routing", "attachment.readonly", "attachment.allowed-transports",
]

LOCK = threading.Lock()
ARGS = None
STATE = {"volumes": {}, "names": {}, "snapshots": {}, "snap_names": {}, "slot": 0}
LOG = None


def log(entry):
    LOG.write(json.dumps(entry) + "\n")
    LOG.flush()


class ApiError(Exception):
    def __init__(self, status, code, message=""):
        self.status, self.code, self.message = status, code, message


def run(*cmd):
    out = subprocess.run(cmd, capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError(f"{cmd}: {out.returncode} {out.stderr}")
    return out.stdout.strip()


def not_found():
    return ApiError(404, "clever.csi.resource-not-found", "resource not found")


def record(vol):
    r = {
        "volume_id": vol["id"],
        "capacity_bytes": vol["capacity"],
        "volume_mode": vol["shape"]["volume_mode"],
        "format_policy": "preformatted",
        "source": vol["source"],
    }
    if vol["shape"].get("fs_type"):
        r["fs_type"] = vol["shape"]["fs_type"]
    return r


def snap_record(s):
    return {
        "snapshot_id": s["id"],
        "source_volume_id": s["source"],
        "size_bytes": s["size"],
        "creation_time_seconds": s["ctime_s"],
        "creation_time_nanos": s["ctime_ns"],
        "ready_to_use": True,
    }


def shape_of(body):
    shape = {"volume_mode": body.get("volume_mode")}
    if body.get("fs_type") is not None:
        shape["fs_type"] = body["fs_type"]
    return shape


def backing(vid):
    return os.path.join(ARGS.data, vid.replace("/", "_") + ".img")


def ensure_volume(body):
    name = body["name"]
    req = body["required_bytes"]
    lim = body.get("limit_bytes")
    shape = shape_of(body)
    source = body.get("source")
    params = body.get("parameters") or {}
    if name in STATE["names"]:
        vol = STATE["volumes"][STATE["names"][name]]
        if (vol["shape"] != shape or vol["source"] != source or vol["params"] != params
                or vol["capacity"] < req or (lim is not None and vol["capacity"] > lim)):
            raise ApiError(409, "clever.csi.incompatible-resource")
        return record(vol)
    src_file = None
    if source is not None:
        if source["type"] == "snapshot":
            snap = STATE["snapshots"].get(source["id"])
            if snap is None:
                raise not_found()
            src_file = snap["file"]
            src_size = snap["size"]
            src_shape = snap["shape"]
        else:
            src = STATE["volumes"].get(source["id"])
            if src is None:
                raise not_found()
            src_file = backing(src["id"])
            src_size = src["capacity"]
            src_shape = src["shape"]
        if src_shape != shape:
            raise ApiError(400, "clever.csi.invalid-argument")
        if lim is not None and src_size > lim:
            raise ApiError(422, "clever.csi.out-of-range")
    capacity = req
    if lim is not None and capacity > lim:
        raise ApiError(422, "clever.csi.out-of-range")
    vid = "vol-" + uuid.uuid4().hex
    path = backing(vid)
    if src_file:
        run("cp", "--sparse=always", src_file, path)
        capacity = max(capacity, src_size)
        os.truncate(path, capacity)
    else:
        with open(path, "wb") as f:
            f.truncate(capacity)
        fs = shape.get("fs_type")
        if shape["volume_mode"] == "filesystem":
            if fs == "xfs":
                run("mkfs.xfs", "-q", "-f", path)
            elif fs == "ext4":
                run("mkfs.ext4", "-q", "-F", path)
            else:
                raise ApiError(400, "clever.csi.invalid-argument")
    vol = {"id": vid, "name": name, "capacity": capacity, "shape": shape, "source": source,
           "params": params, "attach": None}
    STATE["volumes"][vid] = vol
    STATE["names"][name] = vid
    return record(vol)


def resolve(body):
    vol = STATE["volumes"].get(body["volume_id"])
    if vol is None:
        raise not_found()
    return record(vol)


def delete_volume(body):
    vol = STATE["volumes"].get(body["volume_id"])
    if vol is None:
        raise not_found()
    if vol["attach"] is not None:
        raise ApiError(409, "clever.csi.volume-in-use")
    os.unlink(backing(vol["id"]))
    del STATE["volumes"][vol["id"]]
    del STATE["names"][vol["name"]]
    return {}


def expand(body):
    vol = STATE["volumes"].get(body["volume_id"])
    if vol is None:
        raise not_found()
    exp = body.get("expected_shape")
    if exp is not None and (exp["volume_mode"] != vol["shape"]["volume_mode"]
            or exp.get("fs_type") is not None and exp["fs_type"] != vol["shape"].get("fs_type")):
        raise ApiError(409, "clever.csi.incompatible-resource")
    req = body["required_bytes"]
    lim = body.get("limit_bytes")
    if lim is not None and max(req, vol["capacity"]) > lim:
        raise ApiError(422, "clever.csi.out-of-range")
    if vol["capacity"] < req:
        os.truncate(backing(vol["id"]), req)
        vol["capacity"] = req
    # Backing growth can complete before the attached device refresh fails.
    # Every retry must reconcile and observe the device, even if the recorded
    # backing capacity already satisfies this request.
    if vol["attach"] is not None:
        loop = vol["attach"]["loop"]
        run("losetup", "--set-capacity", loop)
        if int(run("blockdev", "--getsize64", loop)) < vol["capacity"]:
            raise RuntimeError("attached device has not observed the expanded capacity")
    return record(vol)


def plug(vol, readonly):
    STATE["slot"] += 1
    name = f"sdz{STATE['slot']}"
    serial = f"cc-{vol['id'][4:]}"
    args = ["losetup", "--find", "--show"]
    if readonly:
        args.append("--read-only")
    # Docker's /dev may lack freshly allocated loop nodes.
    cand = run("losetup", "--find")
    if not os.path.exists(cand):
        os.mknod(cand, 0o600 | 0o060000, os.makedev(7, int(cand[len("/dev/loop"):])))
    loop = run(*args, backing(vol["id"]))
    metadata = os.stat(backing(vol["id"]))
    marker = backing(vol["id"]) + ".loop-owner"
    with open(marker, "w") as handle:
        handle.write(f"{loop}\n{backing(vol['id'])}\n{os.major(metadata.st_dev)}:{os.minor(metadata.st_dev)}\n{metadata.st_ino}\n")
        handle.flush()
        os.fsync(handle.fileno())
    st = os.stat(loop)
    major, minor = os.major(st.st_rdev), os.minor(st.st_rdev)
    root = os.path.join(ARGS.sys, name)
    topology = Path(ARGS.sys).parent / "synthetic-topology"
    transport = topology / "virtio0"
    device = transport / "host0/target0:0:0" / name
    for directory in [Path(root), device, topology / "buses/scsi", topology / "buses/virtio", topology / "drivers/virtio_scsi"]:
        directory.mkdir(parents=True, exist_ok=True)
    Path(root, "device").symlink_to(device)
    (device / "subsystem").symlink_to(topology / "buses/scsi")
    if not (transport / "driver").exists():
        (transport / "driver").symlink_to(topology / "drivers/virtio_scsi")
        (transport / "subsystem").symlink_to(topology / "buses/virtio")
    (device / "type").write_text("0\n")
    with open(os.path.join(root, "dev"), "w") as f:
        f.write(f"{major}:{minor}\n")
    page = bytes([0, 0x80, 0, len(serial)]) + serial.encode()
    with open(os.path.join(root, "device", "vpd_pg80"), "wb") as f:
        f.write(page)
    with open(os.path.join(root, "device", "rescan"), "w") as f:
        f.write("")
    node = os.path.join(ARGS.dev, name)
    os.mknod(node, 0o600 | 0o060000, os.makedev(major, minor))
    return {"loop": loop, "name": name, "serial": serial, "marker": marker}


def unplug(att):
    shutil.rmtree(os.path.join(ARGS.sys, att["name"]))
    os.unlink(os.path.join(ARGS.dev, att["name"]))
    run("losetup", "--detach", att["loop"])
    os.unlink(att["marker"])


def attach(body):
    if "qemu" not in body.get("allowed_transports", []):
        raise ApiError(412, "clever.csi.unsupported-transport")
    vol = STATE["volumes"].get(body["volume_id"])
    if vol is None:
        raise not_found()
    node = body["node_id"]
    if node not in ARGS.nodes:
        raise not_found()
    if shape_of(body) != vol["shape"]:
        raise ApiError(409, "clever.csi.incompatible-resource")
    att = vol["attach"]
    if att is not None:
        if att["node"] != node:
            raise ApiError(409, "clever.csi.attached-elsewhere")
        if att["readonly"] != body["readonly"]:
            raise ApiError(409, "clever.csi.incompatible-resource")
    else:
        dev = plug(vol, body["readonly"])
        att = {"node": node, "readonly": body["readonly"], **dev}
        vol["attach"] = att
    r = {"transport": "qemu", "volume_id": vol["id"], "node_id": node,
         "device_serial": att["serial"], "readonly": att["readonly"],
         "volume_mode": vol["shape"]["volume_mode"], "format_policy": "preformatted"}
    if vol["shape"].get("fs_type"):
        r["fs_type"] = vol["shape"]["fs_type"]
    return r


def detach(body):
    vol = STATE["volumes"].get(body["volume_id"])
    if vol is None:
        raise not_found()
    node = body.get("node_id")
    if node is not None and node not in ARGS.nodes:
        raise not_found()
    att = vol["attach"]
    if att is None or (node is not None and att["node"] != node):
        return {}
    unplug(att)
    vol["attach"] = None
    return {}


def ensure_snapshot(body):
    name = body["name"]
    if name in STATE["snap_names"]:
        s = STATE["snapshots"][STATE["snap_names"][name]]
        if s["source"] != body["source_volume_id"] or s["params"] != (body.get("parameters") or {}):
            raise ApiError(409, "clever.csi.incompatible-resource")
        return snap_record(s)
    vol = STATE["volumes"].get(body["source_volume_id"])
    if vol is None:
        raise not_found()
    sid = "snap-" + uuid.uuid4().hex
    f = os.path.join(ARGS.data, sid + ".snap")
    run("cp", "--sparse=always", backing(vol["id"]), f)
    now = time.time_ns()
    s = {"id": sid, "name": name, "source": vol["id"], "size": vol["capacity"],
         "shape": vol["shape"], "file": f, "params": body.get("parameters") or {},
         "ctime_s": now // 1_000_000_000, "ctime_ns": now % 1_000_000_000}
    STATE["snapshots"][sid] = s
    STATE["snap_names"][name] = sid
    return snap_record(s)


def delete_snapshot(body):
    s = STATE["snapshots"].get(body["snapshot_id"])
    if s is None:
        raise not_found()
    os.unlink(s["file"])
    del STATE["snapshots"][s["id"]]
    del STATE["snap_names"][s["name"]]
    return {}


def list_snapshots(body):
    entries = sorted(STATE["snapshots"].values(), key=lambda s: (s["ctime_s"], s["ctime_ns"], s["id"]))
    if body.get("snapshot_id") is not None:
        entries = [s for s in entries if s["id"] == body["snapshot_id"]]
    if body.get("source_volume_id") is not None:
        entries = [s for s in entries if s["source"] == body["source_volume_id"]]
    start = 0
    tok = body.get("starting_token")
    if tok is not None:
        try:
            start = int(tok)
            if start < 0 or start > len(entries):
                raise ValueError
        except ValueError:
            raise ApiError(400, "clever.csi.invalid-pagination-token")
    maxe = body.get("max_entries") or 0
    end = len(entries) if maxe <= 0 else min(len(entries), start + maxe)
    page = {"entries": [snap_record(s) for s in entries[start:end]]}
    if end < len(entries):
        page["next_token"] = str(end)
    return page


ROUTES = {
    "volumes": ensure_volume,
    "volumes/resolve": resolve,
    "volumes/delete": delete_volume,
    "volumes/expand": expand,
    "attachments": attach,
    "attachments/delete": detach,
    "snapshots": ensure_snapshot,
    "snapshots/delete": delete_snapshot,
    "snapshots/list": list_snapshots,
}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def handle_any(self, method):
        prefix = f"/v4/kubernetes/organisations/{ARGS.org}/clusters/{ARGS.cluster}/csi/"
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        entry = {"method": method, "path": self.path, "key": self.headers.get("Idempotency-Key")}
        try:
            body = json.loads(raw) if raw else None
        except ValueError:
            body = None
        entry["body"] = body
        if self.headers.get("Authorization") != f"Bearer {ARGS.token}":
            entry["status"] = 401
            log(entry)
            return self.reply(401, {"code": "clever.unauthenticated"})
        if not self.path.startswith(prefix):
            entry["status"] = 404
            log(entry)
            return self.reply(404, {"code": "clever.core.not-found"})
        route = self.path[len(prefix):]
        try:
            with LOCK:
                if method == "GET" and route == "capabilities":
                    result = {"api_version": "clever-csi-v1", "format_policy": "preformatted",
                              "features": FEATURES}
                elif method == "POST" and route in ROUTES:
                    result = ROUTES[route](body)
                else:
                    raise ApiError(404, "clever.core.not-found")
            entry["status"] = 200
            entry["reply"] = result
            log(entry)
            return self.reply(200, result)
        except ApiError as e:
            entry["status"] = e.status
            entry["code"] = e.code
            log(entry)
            return self.reply(e.status, {"code": e.code, "message": e.message})
        except Exception as e:  # noqa
            entry["status"] = 500
            entry["error"] = repr(e)
            log(entry)
            return self.reply(500, {"code": "clever.internal", "message": repr(e)})

    def do_GET(self):
        self.handle_any("GET")

    def do_POST(self):
        self.handle_any("POST")


def main():
    global ARGS, LOG
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=18080)
    p.add_argument("--listen", default="127.0.0.1")
    p.add_argument("--data", required=True)
    p.add_argument("--sys", required=True)
    p.add_argument("--dev", required=True)
    p.add_argument("--nodes", required=True)
    p.add_argument("--org", default="orga-test")
    p.add_argument("--cluster", default="kubernetes-test")
    p.add_argument("--token", default="sanity-biscuit")
    p.add_argument("--log", required=True)
    ARGS = p.parse_args()
    ARGS.nodes = set(ARGS.nodes.split(","))
    for d in (ARGS.data, ARGS.sys, ARGS.dev):
        os.makedirs(d, exist_ok=True)
    LOG = open(ARGS.log, "a")
    srv = ThreadingHTTPServer((ARGS.listen, ARGS.port), Handler)
    print("fake axo listening", ARGS.port, file=sys.stderr, flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
