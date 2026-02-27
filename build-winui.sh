#!/usr/bin/env bash
set -euo pipefail

TARGET="x86_64-pc-windows-gnu"
WINTUN_URL="https://www.wintun.net/builds/wintun-0.14.1.zip"
STAGING="target/installer-staging"

# ── Build ─────────────────────────────────────────────────────────────

echo ":: Building spora-wincore + spora-winui for $TARGET (release)..."
cargo build --release --target "$TARGET" -p spora-wincore -p spora-winui

# ── Stage files ───────────────────────────────────────────────────────

echo ":: Staging files..."
mkdir -p "$STAGING"

WINUI_EXE="$(pwd)/target/$TARGET/release/spora-winui.exe"
WINCORE_EXE="$(pwd)/target/$TARGET/release/spora-wincore.exe"
WINTUN_DLL="$(pwd)/$STAGING/wintun.dll"

# Download wintun.dll if not already staged
if [ ! -f "$WINTUN_DLL" ]; then
    echo ":: Downloading wintun.dll..."
    TMPDIR=$(mktemp -d)
    curl -sL "$WINTUN_URL" -o "$TMPDIR/wintun.zip"
    unzip -qo "$TMPDIR/wintun.zip" -d "$TMPDIR/wintun"
    cp "$TMPDIR/wintun/wintun/bin/amd64/wintun.dll" "$WINTUN_DLL"
    rm -rf "$TMPDIR"
else
    echo ":: wintun.dll already staged, skipping download"
fi

# ── Generate icon ─────────────────────────────────────────────────────

ICON="$(pwd)/$STAGING/spora.ico"

if [ ! -f "$ICON" ]; then
    echo ":: Converting logo.svg to spora.ico..."
    convert -background none "assets/logo.svg" \
        \( -clone 0 -resize 16x16 \) \
        \( -clone 0 -resize 32x32 \) \
        \( -clone 0 -resize 48x48 \) \
        \( -clone 0 -resize 64x64 \) \
        \( -clone 0 -resize 128x128 \) \
        \( -clone 0 -resize 256x256 \) \
        -delete 0 "$ICON"
else
    echo ":: spora.ico already staged, skipping conversion"
fi

# ── Build installer ───────────────────────────────────────────────────

OUTFILE="$HOME/winshare/spora-installer.exe"

mkdir -p "$HOME/winshare"
echo ":: Building NSIS installer..."
makensis -V2 \
    -DWINUI_EXE="$WINUI_EXE" \
    -DWINCORE_EXE="$WINCORE_EXE" \
    -DWINTUN_DLL="$WINTUN_DLL" \
    -DICON_FILE="$ICON" \
    -DOUTFILE_PATH="$OUTFILE" \
    installer/spora.nsi

# ── Done ──────────────────────────────────────────────────────────────

echo ""
echo "Installer ready: $OUTFILE"
ls -lh "$OUTFILE"
