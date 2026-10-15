# Voicemod Linux

Native, open-source **clean-room port** of the Voicemod concept for Linux.
Real-time voice effects, a multi-format soundboard and a **real PipeWire
virtual microphone** — no Windows drivers, no proprietary code.

The engine was reverse-engineered **only for behaviour** (signal flow, node
topology, protocol semantics). Every line of code here is an original
implementation; no Voicemod code, assets or data are shipped.

License: **MIT OR Apache-2.0** (see `LICENSE-MIT` / `LICENSE-APACHE`).

---

## Architecture

```
┌────────────┐   WebSocket    ┌──────────────┐   PipeWire   ┌──────────────┐
│   vm-ui    │  Control API   │   vm-daemon  │  links/nodes │  mic + apps  │
│  (egui)    ├───────────────►│  headless RT │─────────────►│ (any app see │
└────────────┘   127.0.0.1    └──────────────┘   vm-mic sink│  "vm-mic")   │
┌────────────┐                ┌──────────────┐              └──────────────┘
│  vm (CLI)  │  / Stream Deck │  vm-control  │
└────────────┘                └──────────────┘
```

* **`vm-daemon`** owns the real-time audio pipeline (PipeWire backend: capture,
  DSP graph, virtual-mic null-sink + monitor, soundboard mixer). Also serves
  the WebSocket **Control API** (default port `8765`) and global hotkeys.
  `--live` selects the real PipeWire backend; without it the daemon runs with
  a stub backend (CI/dev, no hardware).
* **`vm-ui`** is an `egui` desktop client. It is **only** a Control API client —
  it can also spawn/lifecycle-manage a local daemon (`Launch daemon`).
* **`vm`** is a headless CLI (`vm devices --live`, `vm check-preset file.json`,
  `vm actions`, `vm keyboards`, …).
* **`vm-control`** is the shared protocol crate (actions, events, server).
  Any WebSocket client — Stream Deck plugins, scripts, Home Assistant — drives
  the daemon with the same protocol.

## Building from source

Dependencies (Debian/Ubuntu names):

```
rust (≥1.74)  cargo  clang (libclang, needed by bindgen)  pkg-config
libpipewire-0.3-dev  libopus-dev
```

```bash
cargo build --release --workspace
# binaries: target/release/{vm,vm-daemon,vm-ui}
```

Optional but recommended — real-time audio without xruns:

```bash
sudo usermod -aG audio "$USER"     # rtprio 99 + memlock for the audio group
sudo usermod -aG input "$USER"     # global hotkeys via evdev, without root
sudo udevadm control --reload-rules && sudo udevadm trigger
```

## Running

```bash
# 1. the daemon (real PipeWire backend)
./target/release/vm-daemon --live

# 2. the UI (in another terminal, or from the desktop file)
./target/release/vm-ui
```

The UI shows `connected :8765` once linked. If no daemon is running, the
top bar offers **Launch daemon** (starts a local instance); when a daemon is
already answering (e.g. a `systemd --user` instance), the UI shows
`daemon running` and never spawns a second one.

### Virtual microphone

The daemon creates a null-sink named **`vm-mic`** (plus `vm-monitor`) and links
`capture → DSP graph → vm-mic` explicitly, so WirePlumber-independent updates
keep working. In Discord/OBS/etc. pick the input usually shown as **`vm-mic`**
(Monitor of `vm-mic`). Use the UI's *Devices* tab to pick the physical input
(the environment variable `VM_INPUT_UID` pins one at startup).

### Global hotkeys (evdev)

The daemon watches readable evdev keyboards (`vm keyboards` lists them) and
funnels matched binds through the same action path as the UI. Two pieces are
**infrastructure only in 0.1.0**: there are no default bindings yet and there
is no API action to add them (binding persistence is a post-release item —
see `STATUS.md`). What works today is the permission plumbing:

```bash
sudo usermod -aG input "$USER"
sudo udevadm control --reload-rules && sudo udevadm trigger   # 99-vm-hotkeys.rules
```

Without those the daemon logs `hotkeys: no readable keyboards` and everything
else keeps working.

### Presets and soundboard

Runtime data lives in `$VM_CONFIG_DIR` (default `~/.config/vm`):

```
~/.config/vm/presets/*.json     # voice presets (nodes: gain, biquad, delay,
                                # pitch-fft, formant-pitch-shifter, reverb,
                                # tube, oscillator, mute, mix…)
~/.config/vm/sounds/            # .ogg (Opus), .wav — used by the soundboard
```

A demo preset with every UI widget (bool/int/text/slider) is included:
`vm-preset/widget-showcase.json` — validate it with `vm check-preset`.

Parameters support `float`, `int`, `bool` and `text`; edit them live in the
UI's *Voices* tab or over the Control API (`setVoiceParameter`).

## Control API (WebSocket)

Connect to `ws://127.0.0.1:8765`, send JSON, receive a reply plus broadcast
events. Replies echo an optional request `id` for strict correlation:

```json
→ {"action":"selectVoice","voice":"widget-showcase","id":1}
← {"ok":true,"data":{...},"id":1}
← {"event":"voiceChanged","voice":"widget-showcase"}
```

Actions: `selectVoice`, `toggleVoiceChanger`, `stopAllSounds`, `playSound`,
`getAllSoundboard`, `setVoiceParameter`, `resetVoiceParameters`,
`setDefaultMicrophone`, `getAudioDevices`, `getUserLicense`, `fadeSound`,
`startAudioStreaming`, `getAudioStreamStatus`, `getBitmap`,
`setSoundboardMasterVolume`, `selectInput`.

Events: `voiceChanged`, `voiceParameterChanged`, `soundStarted`,
`soundStopped`, `voiceChangerToggled`, `streamStatus`.

Auth is opt-in: set `VM_CONTROL_TOKEN` before starting the daemon and send
`"token"` alongside each message. Without it the API trusts the loopback
interface (fine for a single-user desktop, set the token when other machines
can reach the port). For a Stream Deck / second device, run the daemon with a
token and give the plugin the same value.

Minimal Python client (stdlib only) — change a voice, then a parameter:

```python
import json, socket, base64, struct

def send(sock, obj):
    msg = json.dumps(obj).encode()
    mask = b"\x01\x02\x03\x04"
    sock.sendall(bytes([0x81, 0x80 | len(msg)]) + mask +
                 bytes(b ^ mask[i % 4] for i, b in enumerate(msg)))

s = socket.create_connection(("127.0.0.1", 8765))
s.sendall((f"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n"
           f"Connection: Upgrade\r\nSec-WebSocket-Key: {base64.b64encode(b'abcdefghijklmnop').decode()}\r\n"
           f"Sec-WebSocket-Version: 13\r\n\r\n").encode())
while b"\r\n\r\n" not in (resp := s.recv(4096)):
    pass
send(s, {"action": "selectVoice", "voice": "widget-showcase", "id": 1})
send(s, {"action": "setVoiceParameter", "param": "drive.gain", "value": 1.8, "id": 2})
print(s.recv(4096))
print(s.recv(4096))
```

Broadcast events (voice changes, sounds starting/stopping) arrive unsolicited
on the same socket — clients should not assume one request maps to one frame.

## Packaging

Everything lives under [`packaging/`](packaging/). All recipes install the
three binaries, the `.desktop` entry, the icon, the AppStream metadata and the
systemd user unit; the AUR recipe additionally ships the udev rule and
licenses.

### Flatpak

**To install** (once published to Flathub):

```bash
flatpak install flathub io.github.voicemodlinux.vm
flatpak run io.github.voicemodlinux.vm
```

**To build locally from this checkout:**

```bash
flatpak-builder --user --share=network --force-clean build-dir \
  packaging/flatpak/io.github.voicemodlinux.vm.json
flatpak-builder --run build-dir packaging/flatpak/io.github.voicemodlinux.vm.json vm-ui
```

* Built with `org.freedesktop.Sdk.Extension.rust-stable` + `llvm22`
  (libclang for bindgen); PipeWire/Opus come from the runtime. The build logic
  lives in `packaging/flatpak/build-inside.sh` (points libclang at the SDK's
  gcc headers — a build-time-only codegen step).
* Runtime permissions: wayland/x11, dri, `pulseaudio` socket, **native
  PipeWire socket** (`--filesystem=xdg-run/pipewire-0`), `share=ipc`,
  `share=network` (so host-side Control API clients can reach the daemon).
* **Verified end-to-end**: full sandbox build, then `vm-daemon --live` inside
  the sandbox enumerated devices, created `vm-mic` and served the Control API.
* Limitations, by design of the sandbox: global hotkeys are unavailable
  (`/dev/input` is not exposed — the daemon degrades gracefully). Set
  `VM_CONTROL_TOKEN` before running if the API port is exposed.
* Config lives inside the sandbox at
  `~/.var/app/io.github.voicemodlinux.vm/config/vm` (copy your presets and
  sounds there).

### AppImage

**To use**:

```bash
chmod +x VoicemodLinux-0.1.0-x86_64.AppImage
./VoicemodLinux-0.1.0-x86_64.AppImage
```

Optionally move it to `~/.local/bin` or register it with AppImageLauncher for
desktop integration.

**To build**:

```bash
packaging/appimage/build.sh                        # binaries + AppImage
packaging/appimage/build.sh --assemble-only        # leaves the AppDir only
```

`libpipewire`/`libopus` are bundled (allowlist logic keeps base system libs
out), and the `AppRun` launcher starts the daemon when port 8765 is closed —
a double-click "just works". Requires `appimagetool` (downloaded if absent)
or FUSE for the downloaded tool.

### AUR (Arch)

**To install** (once submitted to the AUR):

```bash
yay -S voicemod-linux     # or: paru -S voicemod-linux
```

**To build locally from this checkout:**

```bash
cd packaging/aur && makepkg -si
```

* `depends=('pipewire' 'opus')`, `makedepends=('cargo' 'rust' 'clang' 'pkgconf')`
  (`cargo` is provided by `rust`; `clang` supplies libclang for bindgen).
* Ships `/usr/lib/systemd/user/vm-daemon.service`:
  `systemctl --user enable --now vm-daemon`.
* The post-install script prints the `audio`/`input` group hints.
* Verified with a real end-to-end `makepkg` build (all package files audited).
  Note: pacman 7.1.0 regressed function-style `pkgver` updates — a local build
  there yields an empty version string (cosmetic; AUR infrastructure is
  unaffected) and `--printsrcinfo` needs
  `MAKEPKG_LINT_PKGBUILD=0 makepkg --printsrcinfo > .SRCINFO`.

### systemd (manual / non-Arch)

```bash
install -Dm644 packaging/systemd/vm-daemon.service \
  ~/.config/systemd/user/vm-daemon.service
systemctl --user enable --now vm-daemon
```

## Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| xruns in UI status | add yourself to the `audio` group and re-login; RT threads then get SCHED_FIFO |
| no `vm-mic` in apps | run `vm-daemon --live`; check *Devices* tab links are ACTIVE |
| UI shows `offline` | start the daemon (or click *Launch daemon*) |
| hotkeys never fire | 0.1.0 ships the evdev watcher but no bindings yet (infrastructure only); also check the `input` group + udev rule — everything else works regardless |
| control port busy | another daemon instance runs — stop it (`Stop daemon` in the UI, or the systemd unit) |
| Flatpak: no sound | ensure `pipewire` is your session manager; the native socket permission must be granted (see manifest) |

## Repository layout

```
vm-audio/      PipeWire backend (null-sink, links, formats), stub backend
vm-dsp/        real-time DSP nodes + graphs (gain, biquad, delay, pitch
               (phase-vocoder + cepstrum), formant shifter, reverb, tube…)
vm-soundboard/ ogg/Opus demux + decode, resampler, mixer
vm-preset/     preset JSON format, factory, example fixtures
vm-control/    Control API protocol + WebSocket server
vm-daemon/     headless daemon (owns the graph + audio + API)
vm-cli/        `vm` CLI
vm-ui/         egui desktop client
packaging/     flatpak, appimage, aur, desktop/systemd/udev/metainfo/icon
STATUS.md      engineering ledger (phases, measurements, decisions)
```
