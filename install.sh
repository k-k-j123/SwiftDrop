#!/usr/bin/env bash
# Install SwiftDrop for the current user: the `swiftdrop` binary, an app-menu
# launcher, and an icon. No root, no system paths.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

cargo install --path "$here/apps/cli"

data="${XDG_DATA_HOME:-$HOME/.local/share}"
mkdir -p "$data/applications" "$data/icons/hicolor/scalable/apps"

# Resolve the binary to an absolute path: desktop launchers do not inherit
# ~/.cargo/bin from your shell, so `Exec=swiftdrop` alone often fails.
bin="$(command -v swiftdrop || true)"
[ -n "$bin" ] || bin="$HOME/.cargo/bin/swiftdrop"
sed "s|^Exec=.*|Exec=$bin serve --open|" \
    "$here/packaging/swiftdrop.desktop" > "$data/applications/swiftdrop.desktop"
chmod 644 "$data/applications/swiftdrop.desktop"

install -m 644 "$here/packaging/swiftdrop.svg" \
    "$data/icons/hicolor/scalable/apps/swiftdrop.svg"

command -v update-desktop-database >/dev/null && update-desktop-database "$data/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$data/icons/hicolor" >/dev/null 2>&1 || true

echo "SwiftDrop installed at $bin"
echo "Launch it from your app menu, or run: swiftdrop serve --open"
