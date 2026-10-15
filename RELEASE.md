# Release v0.1.0 — publication checklist

The repository is prepared and tagged locally (`v0.1.0`). It is published at
**`https://github.com/jmiranda-sre/voicemod-linux`** — that URL is used
consistently in the AppStream metadata, systemd unit, PKGBUILD, Flatpak
manifest, CHANGELOG and this documentation. (The Flatpak app ID
`io.github.voicemodlinux.vm` stays as-is; if the project later moves to a
`voicemodlinux` organization, a repository transfer keeps the old links
working and only these references need updating.)

## 1. Publish the repository

```bash
# from the project root (already a git repo with the v0.1.0 tag)
git remote add origin https://github.com/jmiranda-sre/voicemod-linux.git
git push -u origin main
git push origin v0.1.0
```

## 2. Create the GitHub release

```bash
gh release create v0.1.0 \
  --title "Voicemod Linux 0.1.0" \
  --notes-file CHANGELOG.md \
  target/release/vm target/release/vm-daemon target/release/vm-ui
```

Optionally attach the AppImage built with `packaging/appimage/build.sh`.
After the release exists, the Flatpak manifest's `tag: v0.1.0` source and the
AppStream `url` resolve, and `appstreamcli validate` stops warning about the
unreachable URL.

## 3. Distribution channels

| Channel | Status | Action |
| --- | --- | --- |
| Flathub | manifest ready | open a PR to `flathub/flathub` adding the repo (manifest + `flathub.json` under the app ID) |
| AUR | PKGBUILD ready | `git clone ssh://aur@aur.archlinux.org/voicemod-linux.git`, copy `packaging/aur/{PKGBUILD,voicemod-linux.install}`, generate `.SRCINFO` (`MAKEPKG_LINT_PKGBUILD=0 makepkg --printsrcinfo > .SRCINFO` on pacman 7.1), push |
| AppImage | build script ready | attach the artifact to the GitHub release (step 2) |
| crates.io | n/a | workspace crates are not independently published |

## 4. Post-release

- Bump `version` in the workspace `Cargo.toml` for the next cycle.
- Move remaining `STATUS.md` backlog items into issues.
