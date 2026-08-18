#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
server="$root/target/release/atra-web"
port=${ATRA_WEB_TEST_PORT:-32872}
temporary=$(mktemp -d)
log="$temporary/atra-web.log"
server_pid=

cleanup() {
  status=$?
  trap - EXIT
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill "$server_pid"
    wait "$server_pid" 2>/dev/null || true
  fi
  if ((status != 0)) && [[ -s "$log" ]]; then
    printf '%s\n' '--- atra-web server log ---' >&2
    cat "$log" >&2
  fi
  rm -rf "$temporary"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

test -x "$server"
mkdir -m 700 "$temporary/runtime" "$temporary/controller"
export XDG_RUNTIME_DIR="$temporary/runtime"
export ATRA_RUNTIME_DIR="$temporary/controller"

"$server" --port "$port" serve >"$log" 2>&1 &
server_pid=$!

ready=
for _ in {1..200}; do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    wait "$server_pid" || true
    printf 'atra-web exited before becoming ready on port %s\n' "$port" >&2
    exit 1
  fi
  if "$server" --port "$port" status 2>/dev/null | grep -q '^running at '; then
    ready=1
    break
  fi
  sleep 0.05
done

if [[ -z "$ready" ]]; then
  printf 'atra-web did not become ready on port %s\n' "$port" >&2
  exit 1
fi

ATRA_WEB_URL="http://127.0.0.1:$port" pnpm --dir "$root/web" test
