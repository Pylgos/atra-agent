#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
ui="$root/crates/atra-web-ui"
dist="$root/target/dx/atra-web-ui/release/web/public"

command -v tailwindcss >/dev/null
rm -rf "$dist"

(
  cd "$ui"
  dx build --release
)

test -f "$dist/index.html"
ATRA_WEB_ASSETS_DIR="$dist" cargo build --release -p atra-web
