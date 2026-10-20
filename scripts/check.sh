#!/bin/bash
set -euo pipefail
project_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_root"
if [ -x "$project_root/.tools/cargo/bin/cargo" ]; then
  export RUSTUP_HOME="$project_root/.tools/rustup"
  export CARGO_HOME="$project_root/.tools/cargo"
  export PATH="$project_root/.tools/cargo/bin:$PATH"
fi
cargo test --locked --manifest-path gateway/Cargo.toml
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
swift build

