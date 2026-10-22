#!/bin/bash
set -euo pipefail
source "$(dirname "$0")/common.sh"
if [ ! -d "$project_root/dist/Codexpress.app" ]; then
  bash "$project_root/scripts/build.sh"
fi
open "$project_root/dist/Codexpress.app"

