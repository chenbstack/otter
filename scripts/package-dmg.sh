#!/bin/sh
# 打 release 版 dmg:构建 → 组装 Otter.app → 签名 → 生成 target/Otter-<版本>-<架构>.dmg(含「应用程序」快捷方式)。
# 用法:scripts/package-dmg.sh
# 签名证书取本机第一个 Apple Development 证书(OTTER_SIGN_ID 可指定)。没有 Developer ID 证书和公证,
# 所以只适合自己或小范围试用;换一台 Mac 打开时要右键 → 打开,或执行 xattr -cr /Applications/Otter.app。
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VER="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
ARCH="$(uname -m)"
ID="${OTTER_SIGN_ID:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)}"
[ -n "$ID" ] || { echo "没找到代码签名证书" >&2; exit 1; }

cargo build --release
APP="$ROOT/target/dmg/Otter.app"
STAGE="$ROOT/target/dmg/stage"
OUT="$ROOT/target/Otter-$VER-$ARCH.dmg"
rm -rf "$ROOT/target/dmg" "$OUT"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$STAGE"
cp target/release/otter "$APP/Contents/MacOS/otter"
cp assets/logo/otter.icns "$APP/Contents/Resources/otter.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Otter</string>
  <key>CFBundleDisplayName</key><string>Otter</string>
  <key>CFBundleIdentifier</key><string>dev.otter.app</string>
  <key>CFBundleExecutable</key><string>otter</string>
  <key>CFBundleIconFile</key><string>otter</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VER</string>
  <key>CFBundleShortVersionString</key><string>$VER</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign -f -s "$ID" -i dev.otter.app "$APP"
codesign --verify --strict "$APP"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Otter" -srcfolder "$STAGE" -ov -format UDZO "$OUT" >/dev/null
rm -rf "$ROOT/target/dmg"
echo "已生成: $OUT"
ls -lh "$OUT"
