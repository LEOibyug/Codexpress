#!/bin/bash
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$project_root"

run_rust() {
  codexpress_use_local_rust
  if [ "$#" -eq 0 ]; then
    cargo test --locked --manifest-path gateway/Cargo.toml
    cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
    return
  fi
  local targets=("$@")
  local test_flags=()
  local target
  for target in "${targets[@]}"; do
    test_flags+=(--test "$target")
  done
  cargo test --locked --manifest-path gateway/Cargo.toml --lib
  for target in "${targets[@]}"; do
    cargo test --locked --manifest-path gateway/Cargo.toml --test "$target"
  done
  cargo clippy --locked --manifest-path gateway/Cargo.toml --lib --bins "${test_flags[@]}" -- -D warnings
}

mode="${1:-changed}"
case "$mode" in
  --all)
    run_rust
    codexpress_swift test
    python3 scripts/check-image-skill.py
    ;;
  --gateway|--fast)
    shift
    if [ "$#" -eq 0 ]; then
      codexpress_use_local_rust
      cargo test --locked --manifest-path gateway/Cargo.toml --lib
      cargo clippy --locked --manifest-path gateway/Cargo.toml --lib --bins -- -D warnings
    else
      run_rust "$@"
    fi
    ;;
  --swift)
    codexpress_swift test
    ;;
  changed|--changed)
    changed_files="$(
      {
        git diff --name-only HEAD 2>/dev/null || true
        git ls-files --others --exclude-standard 2>/dev/null || true
      } | sort -u
    )"
    if [ -z "$changed_files" ]; then
      printf '未检测到未提交改动；需要完整回归请运行 scripts/check.sh --all。\n'
      exit 0
    fi
    need_rust=false
    need_swift=false
    need_skill=false
    rust_tests=()
    while IFS= read -r file; do
      [ -z "$file" ] && continue
      case "$file" in
        gateway/tests/*.rs)
          need_rust=true
          base="$(basename "$file" .rs)"
          if [ "$base" != "common" ]; then
            rust_tests+=("$base")
          fi
          ;;
        gateway/*)
          need_rust=true
          ;;
        Sources/*|Tests/*|Package.swift|Resources/*)
          need_swift=true
          ;;
      esac
      case "$file" in
        Resources/skills/codexpress-image/*|scripts/check-image-skill.py)
          need_skill=true
          ;;
      esac
    done <<< "$changed_files"

    if [ "$need_rust" = true ]; then
      if [ "${#rust_tests[@]}" -gt 0 ]; then
        run_rust "${rust_tests[@]}"
      else
        codexpress_use_local_rust
        cargo test --locked --manifest-path gateway/Cargo.toml --lib
        cargo clippy --locked --manifest-path gateway/Cargo.toml --lib --bins -- -D warnings
      fi
    fi
    if [ "$need_swift" = true ]; then
      codexpress_swift test
    fi
    if [ "$need_skill" = true ]; then
      python3 scripts/check-image-skill.py
    fi
    if [ "$need_rust" = false ] && [ "$need_swift" = false ] && [ "$need_skill" = false ]; then
      printf '当前改动不涉及 Rust/Swift/技能检查。\n'
    fi
    ;;
  *)
    printf '用法: scripts/check.sh [--changed|--gateway [集成测试名...]|--swift|--all]\n' >&2
    exit 2
    ;;
esac

