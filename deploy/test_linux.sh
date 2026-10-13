#!/usr/bin/env bash
# Run real Linux storage integration tests in a disposable privileged container.
# Only fresh loop devices backed by files inside that container are used.
set -euo pipefail
mode=${1:-mounts}
case "$mode" in
  mounts|sanity) ;;
  *) echo 'Usage: deploy/test_linux.sh [mounts|sanity]' >&2; exit 2 ;;
esac

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cache_root=${CLEVER_CSI_TEST_CACHE:-${XDG_CACHE_HOME:-${HOME}/.cache}/clever-csi-linux-tests}
case "$cache_root" in
  /*) ;;
  *) echo 'CLEVER_CSI_TEST_CACHE must be an absolute path' >&2; exit 2 ;;
esac
case "$cache_root" in
  /tmp|/tmp/*) echo 'Test cache must be outside /tmp' >&2; exit 2 ;;
esac
mkdir -p -- "$cache_root"
build_context=$(mktemp -d "$cache_root/image.XXXXXXXX")
run_name="clever-csi-linux-test-$$-$(date +%s)"
scratch_volume="${run_name}-scratch"
target_volume="${run_name}-target"
image=${CLEVER_CSI_TEST_IMAGE:-clever-csi-driver:linux-tests-$mode}
base_image=${CLEVER_CSI_TEST_BASE_IMAGE:-rust:1.90-bookworm}

cleanup() {
  local status=$?
  # Stop the test first, releasing its private mounts. Durable markers on the
  # scratch volume let a helper clean up even after SIGKILL bypasses Rust Drop.
  docker rm -f "$run_name" >/dev/null 2>&1 || true
  if docker volume inspect "$scratch_volume" >/dev/null 2>&1; then
    if docker run --rm --privileged --entrypoint bash \
      --mount "type=volume,src=$scratch_volume,dst=/scratch" "$image" -c '
        set -euo pipefail
        while IFS= read -r -d "" marker; do
          { IFS= read -r device; IFS= read -r backing; IFS= read -r backing_device; IFS= read -r backing_inode; } < "$marker"
          [[ "$device" =~ ^/dev/loop[0-9]+$ ]] || exit 1
          [[ "$backing" == /scratch/clever-csi-owned-loops-* ]] || exit 1
          [[ "$backing_device" =~ ^[0-9]+:[0-9]+$ && "$backing_inode" =~ ^[0-9]+$ ]] || exit 1
          [[ -b "$device" ]] || mknod "$device" b 7 "${device#/dev/loop}"
          if [[ ! -d "/sys/class/block/${device#/dev/}/loop" ]]; then rm -f -- "$marker"; continue; fi
          # BACK-FILE is namespace-relative and may change when the original
          # container disappears. The kernel keeps the backing inode pinned.
          read -r current_device current_inode < <(losetup --list --noheadings --output BACK-MAJ:MIN,BACK-INO "$device")
          if [[ "$current_device" != "$backing_device" || "$current_inode" != "$backing_inode" ]]; then
            echo "Loop ownership changed: $device; leaving it untouched" >&2
            exit 1
          fi
          losetup --detach "$device"
          rm -f -- "$marker"
        done < <(find /scratch -type f -name "*.loop-owner" -print0)
      '; then
      docker volume rm "$scratch_volume" >/dev/null 2>&1 || true
    else
      echo "Loop cleanup incomplete; retained volume $scratch_volume for recovery" >&2
      status=1
    fi
  fi
  docker volume rm "$target_volume" >/dev/null 2>&1 || true
  rm -rf -- "$build_context"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

cat >"$build_context/Dockerfile" <<'DOCKERFILE'
ARG BASE_IMAGE=rust:1.90-bookworm
FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends util-linux xfsprogs e2fsprogs \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /source
ENV CARGO_TARGET_DIR=/target-tests TMPDIR=/scratch CLEVER_CSI_PRIVILEGED_TESTS=1
CMD ["cargo", "test", "--locked", "--test", "linux_mounts", "--", "--ignored", "--nocapture", "--test-threads=1"]
DOCKERFILE

if [[ "$mode" == sanity ]]; then
  cat >"$build_context/Dockerfile" <<'DOCKERFILE'
ARG BASE_IMAGE=rust:1.90-bookworm
FROM golang:1.25-bookworm AS sanity
RUN GOBIN=/tools go install github.com/kubernetes-csi/csi-test/v5/cmd/csi-sanity@v5.6.0
FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends util-linux xfsprogs e2fsprogs python3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=sanity /tools/csi-sanity /usr/local/bin/csi-sanity
WORKDIR /source
ENV CARGO_TARGET_DIR=/target-tests TMPDIR=/scratch
CMD ["bash", "/source/deploy/sanity/run.sh"]
DOCKERFILE
fi

docker build --build-arg "BASE_IMAGE=$base_image" --tag "$image" "$build_context"
docker volume create "$scratch_volume" >/dev/null
docker volume create "$target_volume" >/dev/null
docker run --name "$run_name" --privileged \
  --mount "type=bind,src=$repo_root,dst=/source,readonly" \
  --mount "type=volume,src=$scratch_volume,dst=/scratch" \
  --mount "type=volume,src=$target_volume,dst=/target-tests" \
  "$image"
