#!/usr/bin/env bash
set -euo pipefail

atra="${ATRA_BINARY:-atra}"
workspace="$(pwd -P)"
activation="$(mktemp)"
trap 'rm -f "$activation"' EXIT

command -v nix >/dev/null
command -v podman >/dev/null

"$atra" runner launch \
  --name host \
  --description "Host environment with no restriction." \
  --approval ask

nix print-dev-env . >"$activation"
rootfs="$(nix build --no-link --print-out-paths .#dev-rootfs)"

container=(
  podman run
  --rm
  --interactive
  --read-only
  --tmpfs /run:rw,nosuid,nodev,noexec,mode=755
  --tmpfs /tmp:rw,nosuid,nodev,exec,mode=1777
  --tmpfs /var/tmp:rw,nosuid,nodev,exec,mode=1777
  --cap-drop=all
  --security-opt label=disable
  --security-opt no-new-privileges
  --volume /nix/store:/nix/store:ro
  --volume "$activation:/activation/dev-env.bash:ro"
  --volume "$workspace:/workspace:rw"
  --volume atra-agent-cargo:/cargo:U
  --volume atra-agent-runner:/atra:U
  --env HOME=/tmp/home
  --env CARGO_HOME=/cargo
  --env CARGO_TARGET_DIR=/workspace/target
  --workdir /workspace
)

runner="$(
  "$atra" runner upload -- \
    "${container[@]}" \
    --env TMPDIR=/atra \
    --rootfs "$rootfs" \
    /bin/bash -c \
    'source /activation/dev-env.bash
     export HOME=/tmp/home TMPDIR=/atra
     exec /bin/bash "$@"' \
    atra-runner-upload
)"

"$atra" runner launch \
  --name container \
  --description "Container environment" \
  --approval allow \
  -- \
  "${container[@]}" \
  --rootfs "$rootfs" \
  /bin/bash -c \
  'source /activation/dev-env.bash
   export HOME=/tmp/home CARGO_HOME=/cargo CARGO_TARGET_DIR=/workspace/target
   mkdir -p "$HOME"
   exec "$@"' \
  atra-runner \
  "$runner" --stdio
