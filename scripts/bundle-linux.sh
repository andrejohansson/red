#!/usr/bin/env bash
# Assemble build/RED-<version>-<arch>.AppImage from a release build.
#
#   ./scripts/bundle-linux.sh [version]
#
# A first-cut AppImage: it bundles the RED binary, icon, and .desktop entry and
# relies on the host's system libraries (glibc, X11/Wayland, Vulkan, fontconfig)
# — present on any Linux desktop. Hardening to a fully self-contained image
# (bundling libxkbcommon etc. via linuxdeploy) is a follow-up; see
# docs/plans/future/cross-platform.md.
#
# Needs: rsvg-convert (librsvg) for the icon; appimagetool (auto-downloaded to
# build/ if not on PATH). Run on Linux.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:-$(git -C "$ROOT" describe --tags --always | sed 's/^v//')}"
ARCH="$(uname -m)"
BUILD="$ROOT/build"
APPDIR="$BUILD/RED.AppDir"

echo "▸ cargo build --release"
cargo build -p red --release
BIN="$ROOT/target/release/RED"

echo "▸ assembling AppDir (v$VERSION, $ARCH)"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps"
cp "$BIN" "$APPDIR/usr/bin/RED"

# Icon: a scalable SVG plus rasterized PNGs at the standard hicolor sizes.
cp "$ROOT/assets/red.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/red.svg"
for size in 32 64 128 256 512; do
  dir="$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps"
  mkdir -p "$dir"
  rsvg-convert -w "$size" -h "$size" "$ROOT/assets/red.svg" -o "$dir/red.png"
done
# AppImage expects a top-level icon + .desktop next to AppRun.
cp "$APPDIR/usr/share/icons/hicolor/512x512/apps/red.png" "$APPDIR/red.png"
cp "$ROOT/crates/red/resources/red.desktop" "$APPDIR/usr/share/applications/red.desktop"
cp "$ROOT/crates/red/resources/red.desktop" "$APPDIR/red.desktop"

# AppRun: launch the bundled binary. exec so signals reach RED directly.
cat > "$APPDIR/AppRun" <<'EOF'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "${0}")")"
exec "${HERE}/usr/bin/RED" "$@"
EOF
chmod +x "$APPDIR/AppRun" "$APPDIR/usr/bin/RED"

# appimagetool: prefer one on PATH, else fetch the official AppImage into build/.
#
# The download is pinned by digest, not just by URL. `continuous` is a *moving*
# tag - the same URL serves different bytes on different days - so an unpinned
# fetch means the tool that assembles a signed-adjacent release artifact is
# whatever that repository published most recently. Bump `APPIMAGETOOL_SHA256`
# deliberately when bumping the tool, the same way a lockfile is bumped.
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"
APPIMAGETOOL_SHA256="${APPIMAGETOOL_SHA256:-}"
TOOL="$(command -v appimagetool || true)"
if [[ -z "$TOOL" ]]; then
  TOOL="$BUILD/appimagetool-x86_64.AppImage"
  if [[ ! -x "$TOOL" ]]; then
    echo "▸ downloading appimagetool"
    curl -fsSL --proto '=https' --proto-redir '=https' -o "$TOOL" "$APPIMAGETOOL_URL"
    if [[ -n "$APPIMAGETOOL_SHA256" ]]; then
      echo "$APPIMAGETOOL_SHA256  $TOOL" | sha256sum --check --status || {
        echo "appimagetool digest mismatch; refusing to package with it" >&2
        echo "  expected $APPIMAGETOOL_SHA256" >&2
        echo "  got      $(sha256sum "$TOOL" | cut -d" " -f1)" >&2
        rm -f "$TOOL"
        exit 1
      }
    else
      # Not fatal for a local build, where the operator can see what they fetched;
      # the release workflow sets the variable, so a release never packages with
      # an unverified tool.
      echo "⚠ APPIMAGETOOL_SHA256 is unset; packaging with an unverified appimagetool" >&2
      echo "  digest of what was downloaded: $(sha256sum "$TOOL" | cut -d" " -f1)" >&2
    fi
    chmod +x "$TOOL"
  fi
fi

OUT="$BUILD/RED-$VERSION-$ARCH.AppImage"
echo "▸ packaging $OUT"
# ARCH guides appimagetool's runtime selection; --no-appstream skips the metainfo
# validation we don't ship yet. Falls back to extract-and-run where FUSE is absent
# (CI containers), so it works without /dev/fuse.
ARCH="$ARCH" "$TOOL" --no-appstream "$APPDIR" "$OUT" \
  || ARCH="$ARCH" "$TOOL" --appimage-extract-and-run --no-appstream "$APPDIR" "$OUT"

# Sidecar checksum: the Linux self-updater verifies the downloaded AppImage against
# this (AppImages aren't OS-notarized). Written with a bare filename so the digest
# is the first token, which the updater parses. Keep it next to the artifact.
( cd "$BUILD" && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256" )
echo "▸ done: $OUT (+ .sha256)"
