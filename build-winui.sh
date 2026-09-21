#!/usr/bin/env bash
set -euo pipefail

TARGET="x86_64-pc-windows-gnu"
WINTUN_URL="https://www.wintun.net/builds/wintun-0.14.1.zip"
# SHA-256 of wintun-0.14.1.zip. wintun.dll is a privileged network driver loaded
# by the shipped client, so the download MUST be verified against this pin before
# it is bundled (a poisoned mirror, hijacked redirect, or TLS-strip would
# otherwise substitute an attacker DLL into the signed installer). Cross-check
# this value against the hashes published at https://www.wintun.net before bumping
# the version.
WINTUN_SHA256="07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51"
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
    curl -fsSL "$WINTUN_URL" -o "$TMPDIR/wintun.zip"
    if ! echo "$WINTUN_SHA256  $TMPDIR/wintun.zip" | sha256sum -c - >/dev/null 2>&1; then
        echo ":: ERROR: wintun.zip SHA-256 mismatch — refusing to bundle an unverified driver" >&2
        echo "::   expected $WINTUN_SHA256" >&2
        echo "::   got      $(sha256sum "$TMPDIR/wintun.zip" | cut -d' ' -f1)" >&2
        rm -rf "$TMPDIR"
        exit 1
    fi
    unzip -qo "$TMPDIR/wintun.zip" -d "$TMPDIR/wintun"
    cp "$TMPDIR/wintun/wintun/bin/amd64/wintun.dll" "$WINTUN_DLL"
    rm -rf "$TMPDIR"
else
    echo ":: wintun.dll already staged, skipping download"
fi

# ── Generate icon ─────────────────────────────────────────────────────

# assets/spora.ico is the committed application icon: the spore mark on a
# square canvas centered on its circle (assets/logo-square.svg), rendered
# at 16-256px. Regenerate with: for n in 16 20 24 32 40 48 64 128 256; do
#   rsvg-convert -w $n -h $n assets/logo-square.svg -o /tmp/ico-$n.png; done
#   convert /tmp/ico-{16,20,24,32,40,48,64,128,256}.png assets/spora.ico
ICON="$(pwd)/assets/spora.ico"
[ -f "$ICON" ] || { echo ":: ERROR: $ICON missing" >&2; exit 1; }

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
