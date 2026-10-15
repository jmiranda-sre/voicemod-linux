#!/bin/bash
# Runs INSIDE the Flatpak build sandbox (org.freedesktop.Sdk 26.08 +
# rust-stable + llvm22 extensions are mounted at /usr/lib/sdk/*).
#
# Kept as a script (not inline build-commands) so the logic can be run and
# audited directly, and so the manifest stays readable.
set -euo pipefail

# libspa-sys drives bindgen, which needs libclang. The llvm22 extension
# provides it, but its clang cannot locate the SDK's gcc fixed headers
# (they live at a non-multiarch path), so point libclang at them explicitly.
# This affects build-time codegen only — nothing is linked from here.
GCC_INC="$(find /usr/lib/gcc -name stdbool.h 2>/dev/null | head -n1 | xargs -r dirname || true)"
if [ -n "${GCC_INC:-}" ]; then
  export BINDGEN_EXTRA_CLANG_ARGS="-I$GCC_INC"
  echo "build-inside: bindgen include: $GCC_INC"
fi

cargo build --release --locked --workspace

install -Dm755 target/release/vm /app/bin/vm
install -Dm755 target/release/vm-daemon /app/bin/vm-daemon
install -Dm755 target/release/vm-ui /app/bin/vm-ui

install -Dm644 packaging/io.github.voicemodlinux.vm.desktop \
  /app/share/applications/io.github.voicemodlinux.vm.desktop
install -Dm644 packaging/io.github.voicemodlinux.vm.metainfo.xml \
  /app/share/metainfo/io.github.voicemodlinux.vm.metainfo.xml
install -Dm644 packaging/io.github.voicemodlinux.vm.svg \
  /app/share/icons/hicolor/scalable/apps/io.github.voicemodlinux.vm.svg

echo "build-inside: OK"
