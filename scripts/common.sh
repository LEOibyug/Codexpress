#!/bin/bash
# Shared build helpers; entry points choose their own commands and shell options.
project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
runtime_root="$project_root/.runtime"
export TMPDIR="$runtime_root/tmp"
export TMP="$TMPDIR" TEMP="$TMPDIR"
export XDG_CACHE_HOME="$runtime_root/cache"
export CLANG_MODULE_CACHE_PATH="$runtime_root/cache/modules"
export SWIFTPM_MODULECACHE_OVERRIDE="$CLANG_MODULE_CACHE_PATH"
export PYTHONDONTWRITEBYTECODE=1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$runtime_root/build/rust}"
export CODEXPRESS_SWIFT_SCRATCH_PATH="${CODEXPRESS_SWIFT_SCRATCH_PATH:-$runtime_root/build/swift}"
mkdir -p "$TMPDIR" "$CLANG_MODULE_CACHE_PATH"

codexpress_use_local_rust() {
  if [ -x "$project_root/.tools/cargo/bin/cargo" ]; then
    export RUSTUP_HOME="$project_root/.tools/rustup"
    export CARGO_HOME="$project_root/.tools/cargo"
    export PATH="$project_root/.tools/cargo/bin:$PATH"
  fi
}

codexpress_swift() {
  swift "$@" --scratch-path "$CODEXPRESS_SWIFT_SCRATCH_PATH" --cache-path "$runtime_root/cache/swiftpm"
}

codexpress_verify_app() {
  local app_path="$1" binary
  codesign --verify --deep --strict "$app_path"
  for binary in Codexpress codexpress-gateway; do
    if [ "$(lipo -archs "$app_path/Contents/MacOS/$binary")" != arm64 ]; then
      printf '错误：%s 不是纯 arm64 构建。\n' "$binary" >&2
      return 1
    fi
  done
}
