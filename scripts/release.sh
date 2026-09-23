#!/bin/bash
# Build a release installer — the Windows client's release ceremony (the
# counterpart of spora-android's scripts/release.sh).
#
#   ./scripts/release.sh
#
# The build is reproducible from the tag it offers:
#   1. the sibling core checkout (../spora — where spora-wincore's path
#      dependencies point) must sit exactly at the ref pinned in
#      .github/core-ref, the ref CI builds against;
#   2. shows the workspace version (Cargo.toml) and asks for confirmation;
#   3. builds via build-winui.sh with --locked (exactly the committed
#      Cargo.lock) into ~/winshare/spora-<version>-setup.exe, then checks
#      that both executables and the installer carry that version;
#   4. on clean trees, offers an annotated v<version> tag recording the core
#      commit, the toolchain and the installer's SHA-256.
#
# No code signing yet: the installer and executables are unsigned, so
# SmartScreen warns on first run.

set -euo pipefail
cd "$(dirname "$0")/.."

CORE_DIR=../spora
TARGET=x86_64-pc-windows-gnu

die() { echo "error: $*" >&2; exit 1; }

# --- core: exactly the pinned ref, or the tag would not describe the build.
[ -d "$CORE_DIR/.git" ] || die "no core checkout at $CORE_DIR"
PIN=$(grep -v '^#' .github/core-ref | head -1 | tr -d '[:space:]')
PIN_SHA=$(git -C "$CORE_DIR" rev-parse -q --verify "$PIN^{commit}") \
  || die "core ref '$PIN' (.github/core-ref) not found in $CORE_DIR — git -C $CORE_DIR fetch --tags"
CORE_SHA=$(git -C "$CORE_DIR" rev-parse HEAD)
if [ "$CORE_SHA" != "$PIN_SHA" ]; then
  echo "error: $CORE_DIR is at $(git -C "$CORE_DIR" describe --tags --always HEAD)," >&2
  echo "       but .github/core-ref pins $PIN ($(git -C "$CORE_DIR" rev-parse --short "$PIN_SHA")):" >&2
  echo "         git -C $CORE_DIR checkout $PIN" >&2
  echo "       (or bump .github/core-ref, commit, and re-run)" >&2
  exit 1
fi
CORE_PROVENANCE="$PIN ($CORE_SHA)"
# Dirty the way the core's own build stamp counts it (tracked files only):
# that stamp is what every connection record from this build will carry.
if [ -n "$(git -C "$CORE_DIR" status --porcelain --untracked-files=no)" ]; then
  echo "warning: $CORE_DIR has uncommitted changes — the build will not match $PIN:"
  git -C "$CORE_DIR" status --short --untracked-files=no | head -10
  echo
  CORE_PROVENANCE="$CORE_PROVENANCE-dirty"
fi
echo "core: $CORE_PROVENANCE"

if [ -n "$(git status --porcelain)" ]; then
  echo "warning: working tree is not clean — this build will not be reproducible from a commit:"
  git status --short | head -10
  echo
fi

# --locked: a stale Cargo.lock fails here rather than being rewritten.
VERSION=$(cargo pkgid --locked -p spora-winui)
VERSION=${VERSION##*[#@]}
TAG="v$VERSION"
echo "Version (Cargo.toml [workspace.package]): $VERSION"
if TAG_SHA=$(git rev-parse -q --verify "refs/tags/$TAG^{commit}"); then
  if [ "$TAG_SHA" = "$(git rev-parse HEAD)" ]; then
    echo "note: $TAG already tags HEAD — this rebuilds that release"
  else
    echo "warning: $TAG already exists on another commit ($(git rev-parse --short "$TAG_SHA")) — bump the version"
  fi
fi
read -rp "Build the release with version $VERSION? [y/N] " answer
case "$answer" in
  y|Y|yes|YES) ;;
  *) echo "aborted — edit [workspace.package] version in Cargo.toml and re-run"; exit 1 ;;
esac

OUT="$HOME/winshare/spora-$VERSION-setup.exe"
./build-winui.sh --locked --out "$OUT"

# Both executables are x86-64; the installer is NSIS's own 32-bit stub.
for exe in spora-winui spora-wincore; do
  file -b "target/$TARGET/release/$exe.exe" | grep -q 'PE32+ executable.*x86-64' \
    || die "$exe.exe is not an x86-64 Windows executable"
done
file -b "$OUT" | grep -q 'Nullsoft Installer' || die "$OUT is not an NSIS installer"
# Every shipped PE file must carry the version as its ProductVersion (the
# VS_VERSIONINFO strings are UTF-16LE, hence strings -el).
for pe in "target/$TARGET/release/spora-winui.exe" "target/$TARGET/release/spora-wincore.exe" "$OUT"; do
  strings -el "$pe" | grep -xF -A1 ProductVersion | grep -qxF "$VERSION" \
    || die "$pe does not carry version $VERSION"
done
echo "version stamp verified: $VERSION"

SHA256=$(sha256sum "$OUT" | cut -d' ' -f1)
echo
echo "built: $OUT"
echo "sha256: $SHA256"

# Offer an annotated tag for the exact source state of this build — only on
# clean trees (a tag on a dirty build would name a commit that is not what
# shipped), and never moving an existing tag (rebuild-after-fix case).
if [ -n "$(git status --porcelain)" ]; then
  echo "note: working tree dirty — not offering tag $TAG (it would not describe this build)"
elif [[ $CORE_PROVENANCE == *-dirty ]]; then
  echo "note: core checkout dirty — not offering tag $TAG (it would not describe this build)"
elif git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "note: tag $TAG already exists — not re-tagging (delete it first if this build supersedes it)"
else
  read -rp "Tag HEAD as $TAG (annotated)? [y/N] " tagit
  case "$tagit" in
    y|Y|yes|YES)
      git tag -a "$TAG" -m "spora-win $VERSION

core: $CORE_PROVENANCE
toolchain: $(rustc --version), $TARGET; makensis $(makensis -VERSION)
installer: spora-$VERSION-setup.exe sha256 $SHA256"
      echo "tagged $TAG — push with: git push origin $TAG"
      ;;
    *)
      echo "skipped — tag later with: git tag -a $TAG && git push origin $TAG"
      ;;
  esac
fi

echo
echo "next:  install it on the test laptop and sanity-check (share, connect, a spora: link):"
echo "         tools/wintest/wt install $OUT"
echo "       then upload via the website admin (Windows, app)."
