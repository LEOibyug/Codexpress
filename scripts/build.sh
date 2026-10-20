#!/bin/bash
set -euo pipefail
project_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_root"
if [ -x "$project_root/.tools/cargo/bin/cargo" ]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$project_root/.tools/cargo/bin:$PATH"
fi
cargo build --release --locked --manifest-path gateway/Cargo.toml
swift build -c release --arch arm64
app="$project_root/dist/Codexpress.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp .build/arm64-apple-macosx/release/Codexpress "$app/Contents/MacOS/Codexpress"
cp gateway/target/release/codexpress-gateway "$app/Contents/MacOS/codexpress-gateway"
cp Resources/Info.plist "$app/Contents/Info.plist"
swift scripts/make-icon.swift "$project_root/dist/AppIcon.iconset"
iconutil --convert icns "$project_root/dist/AppIcon.iconset" --output "$app/Contents/Resources/AppIcon.icns"
codesign --force --sign - "$app/Contents/MacOS/codexpress-gateway"
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
printf 'Built %s\n' "$app"

