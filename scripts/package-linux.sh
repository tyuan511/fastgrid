#!/usr/bin/env bash
# Build the Linux release artifacts: a .deb and an AppImage.
#
# Usage: scripts/package-linux.sh [version]
#   version defaults to the workspace version in Cargo.toml.
# Outputs (under dist/):
#   fastgrid-<version>-amd64.deb
#   fastgrid-<version>-x86_64.AppImage
#
# The AppImage ships the binaries, the desktop entry, and the icon, and
# relies on the host for the GUI system libraries (same set as the .deb
# Depends below). It is a portable launcher, not a fully bundled build;
# see docs/packaging.md.
#
# appimagetool is downloaded on first use (continuous release) because it
# is not in distribution repositories. It runs with
# --appimage-extract-and-run: CI runners and minimal hosts have no FUSE.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist"
VERSION="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)}"
ARCH="amd64"
APPIMAGE_ARCH="x86_64"

if [ -z "$VERSION" ]; then
    echo "could not read the workspace version from Cargo.toml" >&2
    exit 1
fi

export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p fg-app -p fg-cli

BIN="$ROOT/target/release/fastgrid"
CLI="$ROOT/target/release/fastgrid-cli"
for f in "$BIN" "$CLI"; do
    [ -x "$f" ] || { echo "missing binary: $f" >&2; exit 1; }
done

mkdir -p "$DIST"
cat > "$DIST/fastgrid.desktop" <<'EOF'
[Desktop Entry]
Name=Fastgrid
Comment=Database client for SQLite, PostgreSQL and MySQL
Exec=fastgrid
Icon=fastgrid
Terminal=false
Type=Application
Categories=Development;Database;
EOF

# --- .deb -----------------------------------------------------------------
DEBROOT="$DIST/debroot"
rm -rf "$DEBROOT"
mkdir -p \
    "$DEBROOT/DEBIAN" \
    "$DEBROOT/usr/bin" \
    "$DEBROOT/usr/share/applications" \
    "$DEBROOT/usr/share/icons/hicolor/256x256/apps"
cat > "$DEBROOT/DEBIAN/control" <<EOF
Package: fastgrid
Version: $VERSION
Section: database
Priority: optional
Architecture: $ARCH
Maintainer: tyuan511 <tangge777@proton.me>
Depends: libxkbcommon-x11-0, libxcb1, libwayland-client0, libasound2t64 | libasound2t, libfontconfig1, libssl3, libvulkan1
Description: Fastgrid database client
 A TablePlus-style database client for SQLite, PostgreSQL and
 MySQL/MariaDB, with a built-in MCP server.
EOF
cp "$BIN" "$DEBROOT/usr/bin/fastgrid"
cp "$CLI" "$DEBROOT/usr/bin/fastgrid-cli"
cp "$DIST/fastgrid.desktop" "$DEBROOT/usr/share/applications/fastgrid.desktop"
cp "$ROOT/assets/app-icon-256.png" \
    "$DEBROOT/usr/share/icons/hicolor/256x256/apps/fastgrid.png"
# Normalize permissions: parent directories here may carry a setgid bit
# (inherited from the home directory), which dpkg-deb rejects on the
# control directory (it requires modes between 0755 and 0775).
find "$DEBROOT" -type d -exec chmod 0755 {} +
find "$DEBROOT" -type f -exec chmod 0644 {} +
chmod 0755 "$DEBROOT/usr/bin/fastgrid" "$DEBROOT/usr/bin/fastgrid-cli"
dpkg-deb --build "$DEBROOT" "$DIST/fastgrid-$VERSION-$ARCH.deb"

# --- AppImage -------------------------------------------------------------
APPDIR="$DIST/Fastgrid.AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin"
cp "$BIN" "$APPDIR/usr/bin/fastgrid"
cp "$CLI" "$APPDIR/usr/bin/fastgrid-cli"
cp "$DIST/fastgrid.desktop" "$APPDIR/fastgrid.desktop"
cp "$ROOT/assets/app-icon-256.png" "$APPDIR/fastgrid.png"
cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
exec "$(dirname "$0")/usr/bin/fastgrid" "$@"
EOF
# Same normalization as the .deb tree, so the AppImage never embeds
# setgid directories or non-executable binaries from a strict umask.
find "$APPDIR" -type d -exec chmod 0755 {} +
find "$APPDIR" -type f -exec chmod 0644 {} +
chmod 0755 "$APPDIR/AppRun" "$APPDIR/usr/bin/fastgrid" "$APPDIR/usr/bin/fastgrid-cli"

TOOLS="$DIST/.tools"
mkdir -p "$TOOLS"
APPIMAGETOOL="$TOOLS/appimagetool-$APPIMAGE_ARCH.AppImage"
if [ ! -x "$APPIMAGETOOL" ]; then
    curl -L -o "$APPIMAGETOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$APPIMAGE_ARCH.AppImage"
    chmod +x "$APPIMAGETOOL"
fi
ARCH="$APPIMAGE_ARCH" "$APPIMAGETOOL" --appimage-extract-and-run \
    "$APPDIR" "$DIST/fastgrid-$VERSION-$APPIMAGE_ARCH.AppImage"

echo "built:"
ls -l "$DIST/fastgrid-$VERSION-$ARCH.deb" "$DIST/fastgrid-$VERSION-$APPIMAGE_ARCH.AppImage"
