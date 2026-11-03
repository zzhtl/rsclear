#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "$0")/.."
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'macOS 打包需要在安装 Xcode Command Line Tools 的 Mac 上运行。' >&2
  exit 1
fi
mkdir -p .build/tmp dist
export CARGO_TARGET_DIR="$PWD/.build/target"
export MACOSX_DEPLOYMENT_TARGET=13.0
cargo build --release --locked --target aarch64-apple-darwin --bin rsclear --bin rsclear-helper
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
stage=$(mktemp -d "$PWD/.build/tmp/package-macos.XXXXXX")
chmod 755 "$stage"
app="$stage/rsclear.app"
cp README.md LICENSE "$stage/"
mkdir -p "$stage/docs"
cp docs/validation.md "$stage/docs/"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp .build/target/aarch64-apple-darwin/release/rsclear .build/target/aarch64-apple-darwin/release/rsclear-helper "$app/Contents/MacOS/"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>rsclear</string>
<key>CFBundleIdentifier</key><string>cn.rsclear.desktop</string>
<key>CFBundleName</key><string>rsclear</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$version</string>
<key>CFBundleVersion</key><string>$version</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF
identity="${RSCLEAR_SIGN_IDENTITY:--}"
codesign --force --options runtime --sign "$identity" "$app/Contents/MacOS/rsclear-helper"
codesign --force --options runtime --sign "$identity" "$app"
codesign --verify --deep --strict "$app"
ditto -c -k --sequesterRsrc --keepParent "$app" "dist/rsclear-${version}-macos-arm64.zip"
hdiutil create -ov -volname rsclear -srcfolder "$stage" "dist/rsclear-${version}-macos-arm64.dmg"
if [[ -n "${RSCLEAR_NOTARY_PROFILE:-}" ]]; then
  xcrun notarytool submit "dist/rsclear-${version}-macos-arm64.dmg" --keychain-profile "$RSCLEAR_NOTARY_PROFILE" --wait
  xcrun stapler staple "dist/rsclear-${version}-macos-arm64.dmg"
fi
(
  cd dist
  shasum -a 256 "rsclear-${version}-macos-arm64.zip" "rsclear-${version}-macos-arm64.dmg" > SHA256SUMS-macos
)
