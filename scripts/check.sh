#!/bin/bash
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$project_root"
codexpress_use_local_rust
cargo test --locked --manifest-path gateway/Cargo.toml
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
codexpress_swift test
python3 scripts/check-image-skill.py

