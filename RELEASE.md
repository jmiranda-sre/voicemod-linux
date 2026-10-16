# Release checklist (living document)

The repository is published at
**`https://github.com/jmiranda-sre/voicemod-linux`** — that URL is used
consistently in the AppStream metadata, systemd unit, PKGBUILD, Flatpak
manifest, CHANGELOG and this documentation. (The Flatpak app ID
`io.github.voicemodlinux.vm` stays as-is; if the project later moves to a
`voicemodlinux` organization, a repository transfer keeps the old links
working and only these references need updating.)

## Per-release procedure (followed for v0.3.0)

```bash
# 0. Everything green first:
cargo test --workspace            # 104/104 on v0.3.0
cargo build --release
appstreamcli validate packaging/io.github.voicemodlinux.vm.metainfo.xml

# Optional: neural DeepFilterNet variant (opt-in feature; sharpens the
# AppImage only if shipped — the default build keeps `deepfilter` off
# with a clear error at preset load):
cargo build --release -p vm-daemon --features vm-dsp/neural


# 1. Version bump (workspace Cargo.toml + Flatpak tag + metainfo entry;
#    PKGBUILD derives pkgver from `git describe`, no edit needed):
#    - Cargo.toml: version = "X.Y.Z"
#    - packaging/flatpak/io.github.voicemodlinux.vm.json: "tag": "vX.Y.Z"
#    - packaging/io.github.voicemodlinux.vm.metainfo.xml: new <release>
#    - CHANGELOG.md: [Unreleased] -> [X.Y.Z] + date

# 2. Build the AppImage:
packaging/appimage/build.sh
# -> VoicemodLinux-X.Y.Z-x86_64.AppImage

# 3. Smoke the release binaries (fresh VM_CONFIG_DIR, stub backend):
VM_CONFIG_DIR=/tmp/vm-rel target/release/vm-daemon &
# selectVoice / getHealth / hotkey + profile actions over ws://127.0.0.1:8765
target/release/vm actions   # must list the full catalogue

# 4. Commit, tag, push:
git add -A && git commit -m "Release vX.Y.Z — ..."
git tag -a vX.Y.Z -m "Voicemod Linux X.Y.Z — ..."
git push origin main
git push origin vX.Y.Z

# 5. GitHub release (notes = the CHANGELOG section for this version):
gh release create vX.Y.Z \
  --title "Voicemod Linux X.Y.Z" \
  --notes-file <notes-X.Y.Z.md> \
  VoicemodLinux-X.Y.Z-x86_64.AppImage

# 6. Record the publication in STATUS.md (PUBLICADO section) + push.
```

## Distribution channels

| Channel | Status | Action |
| --- | --- | --- |
| Flathub | manifest ready (`tag` tracks the latest release) | open a PR to `flathub/flathub` adding the repo (manifest + `flathub.json` under the app ID) |
| AUR | PKGBUILD ready | `git clone ssh://aur@aur.archlinux.org/voicemod-linux.git`, copy `packaging/aur/{PKGBUILD,voicemod-linux.install}`, generate `.SRCINFO` (`MAKEPKG_LINT_PKGBUILD=0 makepkg --printsrcinfo > .SRCINFO` on pacman 7.1), push |
| AppImage | build script ready | attach the artifact to the GitHub release (step 5) |
| crates.io | n/a | workspace crates are not independently published |

## Post-release

- Bump `version` in the workspace `Cargo.toml` for the next cycle
  (or open `[Unreleased]` in the CHANGELOG).
- Move remaining `STATUS.md` backlog items into issues.
