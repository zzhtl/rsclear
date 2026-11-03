#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "$0")/.."
if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo '此脚本构建 Linux x86_64 安装包。' >&2
  exit 1
fi
mkdir -p .build/tmp dist
export CARGO_TARGET_DIR="$PWD/.build/target"
cargo build --release --locked --bin rsclear --bin rsclear-helper
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
stage=$(mktemp -d "$PWD/.build/tmp/package-linux.XXXXXX")
chmod 755 "$stage"
glibc_min=$(objdump -T .build/target/release/rsclear .build/target/release/rsclear-helper | sed -n 's/.*(GLIBC_\([0-9.]*\)).*/\1/p' | sort -Vu | tail -1)
install -Dm755 .build/target/release/rsclear "$stage/usr/bin/rsclear"
install -Dm755 .build/target/release/rsclear-helper "$stage/usr/libexec/rsclear-helper"
install -Dm644 packaging/rsclear.desktop "$stage/usr/share/applications/rsclear.desktop"
install -Dm644 packaging/rsclear.svg "$stage/usr/share/icons/hicolor/scalable/apps/rsclear.svg"
install -Dm644 packaging/cn.rsclear.policy "$stage/usr/share/polkit-1/actions/cn.rsclear.policy"
install -Dm644 README.md "$stage/usr/share/doc/rsclear/README.md"
install -Dm644 docs/validation.md "$stage/usr/share/doc/rsclear/docs/validation.md"
install -Dm644 LICENSE "$stage/usr/share/doc/rsclear/copyright"
mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<EOF
Package: rsclear
Version: $version
Section: utils
Priority: optional
Architecture: amd64
Maintainer: rsclear contributors
Depends: libc6 (>= ${glibc_min:-2.36}), libgcc-s1, libfontconfig1, libxkbcommon0, libxkbcommon-x11-0, libx11-6, libx11-xcb1, libxcursor1, libxi6, libxrandr2, libwayland-client0, pkexec, polkitd
Recommends: xdg-desktop-portal, fonts-noto-cjk
Description: Local disk explorer and cache cleaner
 Native Rust desktop tool for large files, caches and Cargo build outputs.
EOF
dpkg-deb --root-owner-group --build "$stage" "dist/rsclear_${version}_amd64.deb"
portable=$(mktemp -d "$PWD/.build/tmp/package-portable.XXXXXX")
chmod 755 "$portable"
install -m755 .build/target/release/rsclear .build/target/release/rsclear-helper "$portable/"
install -m644 README.md LICENSE "$portable/"
mkdir -p "$portable/docs"
install -m644 docs/validation.md "$portable/docs/validation.md"
tar -C "$portable" -czf "dist/rsclear-${version}-linux-x86_64.tar.gz" .
(
  cd dist
  sha256sum "rsclear_${version}_amd64.deb" "rsclear-${version}-linux-x86_64.tar.gz" > SHA256SUMS-linux
)
