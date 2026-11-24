#!/bin/sh
# 开发期:把 release 二进制打成 target/Otter.app(带图标),签名后启动。
# 用法:cargo build --release && scripts/dev-app.sh [--no-open]
# 签名证书同 dev-sign.sh(OTTER_SIGN_ID 可指定)。
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/target/Otter.app"
ID="${OTTER_SIGN_ID:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)}"
[ -n "$ID" ] || { echo "没找到代码签名证书" >&2; exit 1; }
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/otter" "$APP/Contents/MacOS/otter"
cp "$ROOT/assets/logo/otter.icns" "$APP/Contents/Resources/otter.icns"
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
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign -f -s "$ID" -i dev.otter.app "$APP"
echo "已生成并签名: $APP"
[ "$1" = "--no-open" ] || open "$APP"
