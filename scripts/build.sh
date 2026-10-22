#!/bin/bash
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$project_root"
codexpress_use_local_rust
export MACOSX_DEPLOYMENT_TARGET=14.0
cargo build --release --locked --target aarch64-apple-darwin --manifest-path gateway/Cargo.toml
cargo_target_dir="$(cargo metadata --locked --no-deps --format-version 1 --manifest-path gateway/Cargo.toml | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
codexpress_swift build -c release --arch arm64
swift_bin_dir="$(codexpress_swift build -c release --arch arm64 --show-bin-path)"
app="$project_root/dist/Codexpress.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$swift_bin_dir/Codexpress" "$app/Contents/MacOS/Codexpress"
cp "$cargo_target_dir/aarch64-apple-darwin/release/codexpress-gateway" "$app/Contents/MacOS/codexpress-gateway"
cp Resources/Info.plist "$app/Contents/Info.plist"
cp gateway/resources/CODEX-LICENSE "$app/Contents/Resources/CODEX-LICENSE"
cp gateway/resources/CODEX-NOTICE "$app/Contents/Resources/CODEX-NOTICE"
mkdir -p "$app/Contents/Resources/skills"
cp -R Resources/skills/codexpress-image "$app/Contents/Resources/skills/"
icon_directory="$(mktemp -d "${TMPDIR:-/tmp}/codexpress-icon.XXXXXX")"
trap 'rm -rf "$icon_directory"' EXIT
swift scripts/make-icon.swift "$icon_directory/AppIcon.iconset"
iconutil --convert icns "$icon_directory/AppIcon.iconset" --output "$app/Contents/Resources/AppIcon.icns"
codesign --force --sign - "$app/Contents/MacOS/codexpress-gateway"
codesign --force --sign - "$app"
codexpress_verify_app "$app"
printf 'Built %s\n' "$app"

