#!/bin/bash
# Собрать .app из release-сборки: бинарник + Info.plist + иконка icns
# (packaging/icons, ужатая системным sips до размеров iconset). Локальная сборка
# запускается без нотаризации; ad-hoc подпись нужна, чтобы приложение охотнее
# запускалось у соседей по сети.
#
# Использование: tools/make_app.sh [версия]   (по умолчанию — из Cargo.toml)
#   MACOS_TARGET=<triple> — сборка под конкретную архитектуру (кросс-сборка для CI);
#   бандл кладётся в target/<triple>/dist/, чтобы две архитектуры не конфликтовали.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_NAME="Яндекс Музыка RS"
EXE="yamusic-rs"
VERSION="${1:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"

if [ -n "${MACOS_TARGET:-}" ]; then
  cargo build --release --target "$MACOS_TARGET"
  BIN="target/$MACOS_TARGET/release/$EXE"
  DIST="target/$MACOS_TARGET/dist"
else
  cargo build --release
  BIN="target/release/$EXE"
  DIST="target/dist"
fi
BUNDLE="$DIST/${APP_NAME}.app"

rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
cp "$BIN" "$BUNDLE/Contents/MacOS/$EXE"

# --- иконка: 256.png → .iconset (размеры правит sips) → icns ---
ICONSET="$DIST/${EXE}.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for name in 16 32 128 256; do
  cp "packaging/icons/${name}.png" "$ICONSET/icon_${name}x${name}.png"
done
cp packaging/icons/32.png "$ICONSET/icon_16x16@2x.png"
cp packaging/icons/64.png "$ICONSET/icon_32x32@2x.png"
cp packaging/icons/256.png "$ICONSET/icon_128x128@2x.png"
iconutil -c icns "$ICONSET" -o "$BUNDLE/Contents/Resources/app.icns"

# --- Info.plist ---
cat > "$BUNDLE/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>${APP_NAME}</string>
  <key>CFBundleDisplayName</key><string>${APP_NAME}</string>
  <key>CFBundleExecutable</key><string>${EXE}</string>
  <key>CFBundleIdentifier</key><string>rs.yamusic.mac</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>app</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>GPL-3.0-only; форк SpolzerMod/yamusic-rs</string>
</dict>
</plist>
EOF

codesign --force -s - "$BUNDLE" 2>/dev/null || true
echo "Собрано: $BUNDLE"
