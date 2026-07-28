#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
output=${1:-"$repository/dist"}
architecture=$(uname -m)
case "$architecture" in
    x86_64 | aarch64) ;;
    *)
        printf 'unsupported architecture: %s\n' "$architecture" >&2
        exit 1
        ;;
esac

mkdir -p "$output"
podman build \
    --file "$repository/tools/platform-bundle/Containerfile" \
    --output "$output" \
    "$repository"
