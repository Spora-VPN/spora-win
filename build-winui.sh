#!/usr/bin/env bash
# Build both executables for Windows (release) and package them into the NSIS
# installer, stamped with the workspace version (Cargo.toml).
#
#   ./build-winui.sh [--locked] [--out PATH]
#
# --out     installer path (default ~/winshare/spora-installer.exe, which
#           `wt install` picks up)
# --locked  build exactly the committed Cargo.lock (scripts/release.sh)
set -euo pipefail

OUTFILE="$HOME/winshare/spora-installer.exe"
CARGO_LOCKED=()
while [ $# -gt 0 ]; do
    case $1 in
        --locked) CARGO_LOCKED=(--locked) ;;
        --out) OUTFILE="${2:?--out needs a path}"; shift ;;
        *) echo "usage: $0 [--locked] [--out PATH]" >&2; exit 2 ;;
    esac
    shift
done
case $OUTFILE in /*) ;; *) OUTFILE="$PWD/$OUTFILE" ;; esac
cd "$(dirname "$0")"

TARGET="x86_64-pc-windows-gnu"
WINTUN_URL="https://www.wintun.net/builds/wintun-0.14.1.zip"
# SHA-256 of wintun-0.14.1.zip. wintun.dll is a privileged network driver loaded
# by the shipped client, so the download MUST be verified against this pin before
# it is bundled (a poisoned mirror, hijacked redirect, or TLS-strip would
# otherwise substitute an attacker DLL into the installer). Cross-check
# this value against the hashes published at https://www.wintun.net before bumping
# the version.
WINTUN_SHA256="07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51"
# SHA-256 of bin/amd64/wintun.dll inside that zip: the staged copy is reused
# across builds, so it is re-checked against this every time before bundling.
WINTUN_DLL_SHA256="e5da8447dc2c320edc0fc52fa01885c103de8c118481f683643cacc3220dafce"
STAGING="target/installer-staging"

# ── Build ─────────────────────────────────────────────────────────────

# The workspace version, via Cargo (pkgid ends in `#0.2.1` or `#name@0.2.1`).
VERSION="$(cargo pkgid "${CARGO_LOCKED[@]}" -p spora-winui)"
VERSION="${VERSION##*[#@]}"
# NSIS's VIProductVersion takes exactly four numeric fields: drop any
# pre-release/build suffix and append a zero.
VERSION_NUMERIC="${VERSION%%[-+]*}.0"

echo ":: Building spora-wincore + spora-winui $VERSION for $TARGET (release)..."
cargo build "${CARGO_LOCKED[@]}" --release --target "$TARGET" -p spora-wincore -p spora-winui

# ── Stage files ───────────────────────────────────────────────────────

echo ":: Staging files..."
mkdir -p "$STAGING"

WINUI_EXE="$(pwd)/target/$TARGET/release/spora-winui.exe"
WINCORE_EXE="$(pwd)/target/$TARGET/release/spora-wincore.exe"
WINTUN_DLL="$(pwd)/$STAGING/wintun.dll"
# Wintun's licence, from the same verified zip; it ships next to the DLL.
WINTUN_LICENSE="$(pwd)/$STAGING/wintun-LICENSE.txt"

# Download wintun.dll (and its licence) if not already staged
if [ ! -f "$WINTUN_DLL" ] || [ ! -f "$WINTUN_LICENSE" ]; then
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
    cp "$TMPDIR/wintun/wintun/LICENSE.txt" "$WINTUN_LICENSE"
    rm -rf "$TMPDIR"
else
    echo ":: wintun.dll already staged, skipping download"
fi
if ! echo "$WINTUN_DLL_SHA256  $WINTUN_DLL" | sha256sum -c - >/dev/null 2>&1; then
    echo ":: ERROR: staged wintun.dll does not match the pinned SHA-256 — delete it and rebuild" >&2
    echo "::   $WINTUN_DLL" >&2
    exit 1
fi

# ── Generate icon ─────────────────────────────────────────────────────

# assets/spora.ico is the committed application icon: the spore mark on a
# square canvas centered on its circle (assets/logo-square.svg), rendered
# at 16-256px. Regenerate with: for n in 16 20 24 32 40 48 64 128 256; do
#   rsvg-convert -w $n -h $n assets/logo-square.svg -o /tmp/ico-$n.png; done
#   convert /tmp/ico-{16,20,24,32,40,48,64,128,256}.png assets/spora.ico
ICON="$(pwd)/assets/spora.ico"
[ -f "$ICON" ] || { echo ":: ERROR: $ICON missing" >&2; exit 1; }

# ── Licences ──────────────────────────────────────────────────────────

# Everything in this directory lands in $INSTDIR\licenses; rebuilt every time
# so nothing stale ships.
LICENSES="$(pwd)/$STAGING/licenses"
rm -rf "$LICENSES"
mkdir -p "$LICENSES"
cp LICENSE "$LICENSES/LICENSE.txt"
cp NOTICE "$LICENSES/NOTICE.txt"
cp spora-winui/ui/fonts/OFL.txt "$LICENSES/IBM-Plex-OFL.txt"
cp "$WINTUN_LICENSE" "$LICENSES/wintun-LICENSE.txt"

# ── Build installer ───────────────────────────────────────────────────

mkdir -p "$(dirname "$OUTFILE")"
echo ":: Building NSIS installer..."
makensis -V2 \
    -DVERSION="$VERSION" \
    -DVERSION_NUMERIC="$VERSION_NUMERIC" \
    -DWINUI_EXE="$WINUI_EXE" \
    -DWINCORE_EXE="$WINCORE_EXE" \
    -DWINTUN_DLL="$WINTUN_DLL" \
    -DICON_FILE="$ICON" \
    -DLICENSES_DIR="$LICENSES" \
    -DOUTFILE_PATH="$OUTFILE" \
    installer/spora.nsi

# ── Done ──────────────────────────────────────────────────────────────

echo ""
echo "Installer ready: $OUTFILE (Spora $VERSION)"
ls -lh "$OUTFILE"
