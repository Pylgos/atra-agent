#!/bin/sh
set -eu

output=$1
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/blobs"
tools_json='[]'

executable=/opt/atra/bin/atra-runner
description=$(file "$executable")
printf '%s\n' "$description"
printf '%s\n' "$description" | grep -Eq 'statically linked|static-pie linked'
runner_digest=$(sha256sum "$executable" | cut -d' ' -f1)
compressed="$work/atra-runner.zst"
zstd -q -19 -T0 "$executable" -o "$compressed"
compressed_digest=$(sha256sum "$compressed" | cut -d' ' -f1)
runner_blob="blobs/$compressed_digest.zst"
mv "$compressed" "$work/$runner_blob"

for name in bash rg fd jq tmux; do
    executable="/opt/atra/bin/$name"
    description=$(file "$executable")
    printf '%s\n' "$description"
    printf '%s\n' "$description" | grep -Eq 'statically linked|static-pie linked'
    digest=$(sha256sum "$executable" | cut -d' ' -f1)
    compressed="$work/$name.zst"
    zstd -q -19 -T0 "$executable" -o "$compressed"
    compressed_digest=$(sha256sum "$compressed" | cut -d' ' -f1)
    blob="blobs/$compressed_digest.zst"
    mv "$compressed" "$work/$blob"
    tools_json=$(jq \
        --arg name "$name" \
        --arg digest "$digest" \
        --arg blob "$blob" \
        '. + [{name: $name, digest: $digest, blob: $blob}]' \
        <<EOF
$tools_json
EOF
    )
done

jq -n \
    --arg platform "$(uname -m)-linux-musl" \
    --arg runner_digest "$runner_digest" \
    --arg runner_blob "$runner_blob" \
    --argjson tools "$tools_json" \
    '{
        platform: $platform,
        runner: {digest: $runner_digest, blob: $runner_blob},
        tools: $tools
    }' >"$work/manifest.json"

mkdir -p "$(dirname "$output")"
(cd "$work" && zip -q -0 -r "$output" manifest.json blobs)
