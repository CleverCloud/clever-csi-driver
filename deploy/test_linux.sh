#!/usr/bin/env bash
# Run real Linux storage integration tests in a disposable privileged container.
# Only fresh loop devices backed by files inside that container are used.
set -euo pipefail

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
image=${CLEVER_CSI_TEST_IMAGE:-clever-csi-driver:linux-tests}
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
          { IFS= read -r device; IFS= read -r backing; } < "$marker"
          [[ "$device" =~ ^/dev/loop[0-9]+$ ]] || exit 1
          [[ "$backing" == /scratch/clever-csi-owned-loops-* ]] || exit 1
          [[ -b "$device" ]] || mknod "$device" b 7 "${device#/dev/loop}"
          current=$(losetup --noheadings --raw --output BACK-FILE "$device" 2>/dev/null || true)
          if [[ -z "$current" ]]; then rm -f -- "$marker"; continue; fi
          if [[ "$current" != "$backing" ]]; then
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

docker build --build-arg "BASE_IMAGE=$base_image" --tag "$image" "$build_context"
docker volume create "$scratch_volume" >/dev/null
docker volume create "$target_volume" >/dev/null
docker run --name "$run_name" --privileged \
  --mount "type=bind,src=$repo_root,dst=/source,readonly" \
  --mount "type=volume,src=$scratch_volume,dst=/scratch" \
  --mount "type=volume,src=$target_volume,dst=/target-tests" \
  "$image"
