#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
dist="$root/target/dx/atra-web-ui/release/web/public"

"$root/tools/build-atra-web-assets.sh"
ATRA_WEB_ASSETS_DIR="$dist" cargo build --release -p atra-web
