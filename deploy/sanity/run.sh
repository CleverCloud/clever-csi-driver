#!/usr/bin/env bash
# Runs only in the disposable privileged test container created by test_linux.sh.
set -euo pipefail
export CLEVER_CSI_PRIVILEGED_TESTS=1
root=$(mktemp -d /scratch/clever-csi-owned-loops-sanity-XXXXXXXX)
mkdir -p "$root/data" "$root/sys" "$root/dev" "$root/state" "$root/kubelet"
printf 'sanity-biscuit\n' > "$root/token"
chmod 600 "$root/token"
backend_pid=
driver_pid=
cleanup() {
  local status=$?
  [[ -z "$driver_pid" ]] || kill "$driver_pid" 2>/dev/null || true
  [[ -z "$backend_pid" ]] || kill "$backend_pid" 2>/dev/null || true
  wait 2>/dev/null || true
  if (( status != 0 )); then
    cat "$root/driver.log" "$root/axo.log" >&2 || true
  fi
  # The outer runner removes this mount namespace, then recovers any owned loops
  # through their durable inode markers before deleting the scratch volume.
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
cargo build --locked --example csi-sanity-server
python3 /source/deploy/sanity/fake_axo.py --data "$root/data" --sys "$root/sys" \
  --dev "$root/dev" --nodes sanity-vm --log "$root/axo-calls.jsonl" > "$root/axo.log" 2>&1 &
backend_pid=$!
for attempt in {1..100}; do
  if python3 -c 'import urllib.request; urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:18080/v4/kubernetes/organisations/orga-test/clusters/kubernetes-test/csi/capabilities", headers={"Authorization":"Bearer sanity-biscuit"}), timeout=1)' 2>/dev/null; then
    break
  fi
  kill -0 "$backend_pid"
  sleep 0.1
done
"$CARGO_TARGET_DIR/debug/examples/csi-sanity-server" --endpoint "unix://$root/csi.sock" \
  --axo-url http://127.0.0.1:18080 --token-file "$root/token" --state-dir "$root/state" \
  --device-root "$root/dev" --sys-block-dir "$root/sys" --kubelet-root "$root/kubelet" \
  > "$root/driver.log" 2>&1 &
driver_pid=$!
for attempt in {1..100}; do
  [[ ! -S "$root/csi.sock" ]] || break
  kill -0 "$driver_pid"
  sleep 0.1
done
test -S "$root/csi.sock"
csi-sanity --csi.endpoint="$root/csi.sock" \
  --csi.stagingdir="$root/kubelet/staging" --csi.mountdir="$root/kubelet/mount" \
  --ginkgo.no-color --ginkgo.v
