#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
ui="$root/crates/atra-web-ui"
dist="$root/target/dx/atra-web-ui/release/web/public"

rm -rf "$dist"
(
  cd "$ui"
  dx build --release --debug-symbols false --keep-names
)

test -f "$dist/index.html"
cp "$root/crates/atra-web/assets/service-worker.js" "$dist/service-worker.js"
test -f "$dist/service-worker.js"
