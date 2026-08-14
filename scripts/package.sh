#!/bin/sh
set -eu

if [ "$#" -ne 0 ]; then
    echo "Usage: ./scripts/package.sh" >&2
    exit 2
fi

script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
project_dir=$(dirname "$script_dir")
manifest="$project_dir/Cargo.toml"
target_dir="$project_dir/target"
dist_dir="$project_dir/dist"
source_binary="$target_dir/release/btrtty"
output_binary="$dist_dir/btrtty"
temporary_binary="$dist_dir/.btrtty.$$.tmp"
cargo_bin=${CARGO:-cargo}
identity=${CODESIGN_IDENTITY:--}

cleanup() {
    if [ -n "$temporary_binary" ]; then
        rm -f "$temporary_binary"
    fi
}
trap cleanup 0
trap 'exit 1' 1 2 15

"$cargo_bin" build \
    --manifest-path "$manifest" \
    --release \
    --package btrtty \
    --target-dir "$target_dir"

mkdir -p "$dist_dir"
cp "$source_binary" "$temporary_binary"

if [ "$identity" = "-" ]; then
    codesign \
        --force \
        --sign - \
        --identifier dev.btrtty.terminal \
        --timestamp=none \
        "$temporary_binary"
else
    codesign \
        --force \
        --sign "$identity" \
        --identifier dev.btrtty.terminal \
        --options runtime \
        --timestamp \
        "$temporary_binary"
fi

plutil -p "$temporary_binary" >/dev/null
codesign --verify --strict --verbose=2 "$temporary_binary"
mv -f "$temporary_binary" "$output_binary"
temporary_binary=

echo "$output_binary"
