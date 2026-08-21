#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
ui="$root/crates/atra-web-ui"
dist="$root/target/dx/atra-web-ui/release/web/public"
vite_dist="$root/target/web-assets/vite"

rm -rf "$dist" "$vite_dist"
pnpm --dir "$root/web" build
(
  cd "$ui"
  dx build --release --debug-symbols false --keep-names
)

test -f "$dist/index.html"
test -f "$vite_dist/assets/shiki/index.mjs"
cp -R "$vite_dist/." "$dist/"
cp "$root/crates/atra-web/assets/service-worker.js" "$dist/service-worker.js"
test -f "$dist/service-worker.js"
