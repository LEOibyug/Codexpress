#!/bin/bash
set -euo pipefail
source "$(dirname "$0")/common.sh"
cd "$project_root"
app="$project_root/dist/Codexpress.app"
version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf '错误：应用版本号必须为 major.minor.patch。\n' >&2
  exit 1
fi
codexpress_verify_app "$app"

staging="$(mktemp -d "${TMPDIR:-/tmp}/codexpress-package.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
ditto "$app" "$staging/Codexpress.app"
ln -s /Applications "$staging/Applications"
name="Codexpress-${version}-arm64"
ditto -c -k --sequesterRsrc --keepParent "$app" "$project_root/dist/$name.zip"
hdiutil create -volname Codexpress -srcfolder "$staging" -fs HFS+ -format UDZO -ov "$project_root/dist/$name.dmg"
hdiutil verify "$project_root/dist/$name.dmg"
unzip -tq "$project_root/dist/$name.zip"
(
  cd "$project_root/dist"
  shasum -a 256 "$name.dmg" "$name.zip" > SHA256SUMS.txt
)
printf '打包完成：dist/%s.dmg、dist/%s.zip\n' "$name" "$name"
