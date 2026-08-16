#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
pnpm --dir web install --frozen-lockfile
pnpm --dir web run css
source_css=crates/atra-web-ui/assets/app.css
backup=$(mktemp)
cp "$source_css" "$backup"
restore() { cp "$backup" "$source_css"; rm -f "$backup"; }
trap restore EXIT
cp target/web-assets/app.css "$source_css"
(
  cd crates/atra-web-ui
  dx build --release
)
restore
trap - EXIT
dist="$root/target/dx/atra-web-ui/release/web/public"
test -f "$dist/index.html"
ATRA_WEB_ASSETS_DIR="$dist" cargo build --release -p atra-web
