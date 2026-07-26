#!/bin/sh
set -eu

output=$1
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/blobs"
objects='[]'
entries='[]'

add_object() {
    name=$1
    executable=$2
    description=$(file "$executable")
    printf '%s\n' "$description"
    printf '%s\n' "$description" | grep -Eq 'statically linked|static-pie linked'
    digest=$(
        {
            printf 'atra-object\000\001'
            cat "$executable"
        } | sha256sum | cut -d' ' -f1
    )
    compressed="$work/$name.zst"
    zstd -q -19 -T0 "$executable" -o "$compressed"
    compressed_digest=$(sha256sum "$compressed" | cut -d' ' -f1)
    blob="blobs/$compressed_digest.zst"
    mv "$compressed" "$work/$blob"
    objects=$(jq \
        --arg digest "$digest" \
        --arg blob "$blob" \
        '. + [{digest: $digest, executable: true, blob: $blob}]' \
        <<EOF
$objects
EOF
    )
}

add_object atra-runner /opt/atra/bin/atra-runner
runner_digest=$digest

for name in bash fd jq rg tmux; do
    add_object "$name" "/opt/atra/bin/$name"
    entries=$(jq \
        --arg path "bin/$name" \
        --arg object "$digest" \
        '. + [{type: "file", path: $path, object: $object}]' \
        <<EOF
$entries
EOF
    )
done

jq -n \
    --arg platform "$(uname -m)-linux-static" \
    --arg runner "$runner_digest" \
    --argjson entries "$entries" \
    --argjson objects "$objects" \
    '{
        platform: $platform,
        runner: $runner,
        tools: {entries: $entries},
        objects: $objects
    }' >"$work/manifest.json"

mkdir -p "$(dirname "$output")"
(cd "$work" && zip -q -0 -r "$output" manifest.json blobs)
