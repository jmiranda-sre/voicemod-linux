#!/usr/bin/env bash
# Build a portable AppImage for Voicemod Linux.
#
# Usage:
#   packaging/appimage/build.sh [--assemble-only] [--out DIR]
#
# What it does:
#   1. Builds the release binaries (cargo build --release --workspace).
#   2. Assembles an AppDir: binaries + bundled shared libraries (libpipewire,
#      libopus and everything they need that is not in the base system
#      allowlist), desktop file, icon and an AppRun launcher.
#   3. Packages the AppDir with appimagetool (downloaded on demand). With
#      --assemble-only the AppDir is left in place and the final packaging
#      step is skipped (useful on hosts without FUSE).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | head -n1 | sed 's/.*"\(.*\)".*/\1/')"
ARCH="x86_64"
OUT="$(mktemp -d /tmp/voicemod-appimage.XXXXXX)"
APPDIR="$OUT/VoicemodLinux.AppDir"
ASSEMBLE_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --assemble-only) ASSEMBLE_ONLY=1 ;;
    --out) shift; OUT="$1" ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

echo "== building release binaries"
cargo build --release --workspace

# libs that the host always provides; anything NOT listed here gets bundled.
BASE_LIBS="^(ld-linux|libc|libm|libgcc_s|libdl|libpthread|librt|libstdc\+\+|libresolv|libnss_|libutil)\.so"

echo "== assembling AppDir at $APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps" \
         "$APPDIR/usr/share/icons/hicolor/512x512/apps"

for bin in vm vm-daemon vm-ui; do
  install -Dm755 "target/release/$bin" "$APPDIR/usr/bin/$bin"
done

# Copy each binary's non-base shared libraries, transitively, resolving
# symlinks so the runtime never depends on -devel-only .so names.
copied=""
copy_libs() {
  local target="$1"
  local dep resolved real
  for dep in $(ldd "$target" 2>/dev/null | awk '/=> \//{print $3} /^\//{print $1}' | sort -u); do
    real="$(readlink -f "$dep")"
    case "$copied" in *"|$real|"*) continue ;; esac
    copied="$copied|$real|"
    install -Dm644 "$real" "$APPDIR/usr/lib/$(basename "$real")"
    copy_libs "$real"
  done
}
for bin in vm vm-daemon vm-ui; do
  copy_libs "target/release/$bin"
done
# Drop anything that matched the base allowlist by accident.
find "$APPDIR/usr/lib" -type f | while read -r f; do
  b="$(basename "$f")"
  if [[ "$b" =~ $BASE_LIBS ]] || [[ "$b" == ld-*.so* ]]; then
    rm -f "$f"
  fi
done

# The dynamic loader looks libraries up by SONAME, not by their
# versioned file name: without these symlinks LD_LIBRARY_PATH never
# matches and the bundled libs are silently ignored (host lib wins).
find "$APPDIR/usr/lib" -type f | while read -r f; do
  soname="$(objdump -p "$f" 2>/dev/null | awk '/SONAME/{print $2; exit}')"
  if [ -n "$soname" ] && [ "$soname" != "$(basename "$f")" ]; then
    ln -sf "$(basename "$f")" "$APPDIR/usr/lib/$soname"
  fi
done

echo "== bundled libraries:"
(cd "$APPDIR/usr/lib" && ls -1 || true)

ICONS="packaging/icons/hicolor"
install -Dm644 "$ICONS/scalable/apps/io.github.voicemodlinux.vm.svg" \
  "$APPDIR/usr/share/icons/hicolor/scalable/apps/io.github.voicemodlinux.vm.svg"
install -Dm644 "$ICONS/256x256/apps/io.github.voicemodlinux.vm.png" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps/io.github.voicemodlinux.vm.png"
install -Dm644 "$ICONS/512x512/apps/io.github.voicemodlinux.vm.png" \
  "$APPDIR/usr/share/icons/hicolor/512x512/apps/io.github.voicemodlinux.vm.png"

# Desktop entry: plain Exec (the AppRun wrapper handles the daemon). Narrow the
# sed to the main section only, so Desktop Actions keep their own Exec lines.
sed "0,/^Exec=.*/s|^Exec=.*|Exec=vm-ui %U|" \
  packaging/io.github.voicemodlinux.vm.desktop > "$APPDIR/vm-ui.desktop"
install -Dm644 "$ICONS/scalable/apps/io.github.voicemodlinux.vm.svg" \
  "$APPDIR/io.github.voicemodlinux.vm.svg"

cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/bash
HERE="$(dirname "$(readlink -f "$0")")"
export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export PATH="$HERE/usr/bin:$PATH"

# Bring up the daemon if the Control API port is not answering yet.
port_opens() {
  (exec 3<>"/dev/tcp/127.0.0.1/8765") 2>/dev/null
}
if ! port_opens; then
  "$HERE/usr/bin/vm-daemon" --live >/dev/null 2>&1 &
  for _ in $(seq 1 50); do
    port_opens && break
    sleep 0.1
  done
fi
exec "$HERE/usr/bin/vm-ui" "$@"
EOF
chmod +x "$APPDIR/AppRun"

if [ "$ASSEMBLE_ONLY" = "1" ]; then
  echo "== assemble-only: AppDir ready at $APPDIR"
  echo "   package it with: appimagetool $APPDIR"
  exit 0
fi

echo "== fetching appimagetool if needed"
TOOL="$OUT/appimagetool"
if ! command -v appimagetool >/dev/null 2>&1; then
  if [ ! -x "$TOOL" ]; then
    curl -fsSL -o "$TOOL" \
      "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
    chmod +x "$TOOL"
  fi
else
  TOOL="appimagetool"
fi

IMG="$OUT/VoicemodLinux-$VERSION-$ARCH.AppImage"
"$TOOL" --no-appstream "$APPDIR" "$IMG" >/dev/null
echo "== AppImage: $IMG"
ls -la "$IMG"
