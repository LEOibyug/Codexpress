#!/bin/bash
set -euo pipefail
project_root="$(cd "$(dirname "$0")/.." && pwd)"
if [ ! -d "$project_root/dist/Codexpress.app" ]; then
  bash "$project_root/scripts/build.sh"
fi
open "$project_root/dist/Codexpress.app"

