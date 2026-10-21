#!/bin/bash
set -euo pipefail
project_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_root"
if [ -x "$project_root/.tools/cargo/bin/cargo" ]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$project_root/.tools/cargo/bin:$PATH"
fi
export MACOSX_DEPLOYMENT_TARGET=14.0
cargo build --release --locked --target aarch64-apple-darwin --manifest-path gateway/Cargo.toml
cargo_target_dir="$(cargo metadata --locked --no-deps --format-version 1 --manifest-path gateway/Cargo.toml | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
swift_options=(-c release --arch arm64)
if [ -n "${CODEXPRESS_SWIFT_SCRATCH_PATH:-}" ]; then
  swift_options+=(--scratch-path "$CODEXPRESS_SWIFT_SCRATCH_PATH")
fi
swift build "${swift_options[@]}"
swift_bin_dir="$(swift build "${swift_options[@]}" --show-bin-path)"
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
for binary in Codexpress codexpress-gateway; do
  if [ "$(lipo -archs "$app/Contents/MacOS/$binary")" != arm64 ]; then
    printf '错误：%s 不是纯 arm64 构建。\n' "$binary" >&2
    exit 1
  fi
done
codesign --force --sign - "$app/Contents/MacOS/codexpress-gateway"
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
printf 'Built %s\n' "$app"

