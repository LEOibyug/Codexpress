#!/bin/bash
# Shared build helpers; entry points choose their own commands and shell options.
project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

codexpress_use_local_rust() {
  if [ -x "$project_root/.tools/cargo/bin/cargo" ]; then
    export RUSTUP_HOME="$project_root/.tools/rustup"
    export CARGO_HOME="$project_root/.tools/cargo"
    export PATH="$project_root/.tools/cargo/bin:$PATH"
  fi
}

codexpress_swift() {
  if [ -n "${CODEXPRESS_SWIFT_SCRATCH_PATH:-}" ]; then
    swift "$@" --scratch-path "$CODEXPRESS_SWIFT_SCRATCH_PATH"
  else
    swift "$@"
  fi
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
