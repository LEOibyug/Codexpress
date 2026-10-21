#!/bin/bash
set -euo pipefail
project_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_root"
app="$project_root/dist/Codexpress.app"
version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf '错误：应用版本号必须为 major.minor.patch。\n' >&2
  exit 1
fi
codesign --verify --deep --strict "$app"
for binary in Codexpress codexpress-gateway; do
  if [ "$(lipo -archs "$app/Contents/MacOS/$binary")" != arm64 ]; then
    printf '错误：%s 不是纯 arm64 构建。\n' "$binary" >&2
    exit 1
  fi
done

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
