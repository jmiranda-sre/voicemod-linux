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
funnels matched binds through the same action path as the UI. Binds are
managed over the Control API and persisted in `$VM_CONFIG_DIR/config.json`
(schema version 1), so they survive daemon restarts:

```bash
# Push-To-Talk on Right-Ctrl (evdev KEY_RIGHTCTRL = 97)
{"action":"addHotkey","codes":[97],"target":"PushToTalk","enabled":true}
# Momentary mic mute on F9 (evdev KEY_F9 = 67) ... or toggle mute instead
{"action":"addHotkey","codes":[67],"target":"PushToMute","enabled":true}
{"action":"listHotkeys"}
{"action":"removeHotkey","hotkey_id":0}
```

Targets: `ToggleVoiceChanger`, `ToggleMute`, `PushToTalk` (changer on
while held), `PushToMute` (silence while held, DSP stays live),
`{"PlaySound": name}`, `{"SelectVoice": name}`, `StopAllSounds`.
`setMuted` + the `mutedChanged` event expose the same state for
StreamDeck-style clients.

What still needs the permission plumbing:

```bash
sudo usermod -aG input "$USER"
sudo udevadm control --reload-rules && sudo udevadm trigger   # 99-vm-hotkeys.rules
```

Without those the daemon logs `hotkeys: no readable keyboards` and everything
else keeps working. Inside the Flatpak sandbox (`/.flatpak-info` present)
hotkeys are unavailable by design — the daemon says so plainly and keeps
serving the Control API.

### Autostart and system tray

`vm autostart status|enable|disable` manages login autostart: an XDG
desktop entry booting `vm-ui --minimized` plus the `systemd --user`
unit (`vm-daemon.service`, installed by the native package) for the
headless pipeline:

```bash
vm autostart enable    # UI entry + systemctl --user enable --now vm-daemon
vm autostart status    # entry presence + unit enabled/active
```

The UI lives in the system tray (StatusNotifier/AppIndicator): closing
the window hides it instead of quitting — the daemon link thread is
unaffected. Left-click (or the menu) shows it again; the menu carries
a live mute checkmark and Quit. `vm-ui --minimized` boots straight
into the tray. No tray in the Flatpak sandbox (no SNI host there by
design): the window stays a normal window and autostart is owned by
the Flatpak Background permission instead.

### Presets and soundboard

Runtime data lives in `$VM_CONFIG_DIR` (default `~/.config/vm`):

```
~/.config/vm/presets/*.json     # voice presets (nodes: gain, biquad, delay,
                                # pitch-fft, formant-pitch-shifter, reverb,
                                # tube, oscillator, mute, mix…, plus the mic
                                # section: denoise, gate, compressor,
                                # de-esser, limiter)
~/.config/vm/sounds/            # .ogg (Opus), .wav — used by the soundboard
```

A demo preset with every UI widget (bool/int/text/slider) is included:
`vm-preset/widget-showcase.json` — validate it with `vm check-preset`.

Parameters support `float`, `int`, `bool` and `text`; edit them live in the
UI's *Voices* tab or over the Control API (`setVoiceParameter`).

The UI's *Health* tab shows per-node DSP cost (µs per quantum), stream
xruns and watchdog restarts — the same data `getHealth` returns over
the Control API. If the link dies or xruns storm, the daemon's watchdog
reopens the pipeline automatically (10 s attempt cooldown) and records
the reason there.

### Ready-made mic preset

On first boot the daemon seeds `mic-npr-voice` into
`~/.config/vm/presets/` — a complete "plug in and talk" chain. Seeding
never overwrites: edits you make to the seeded file always win. Select it
with `selectVoice mic-npr-voice` (Control API), in the UI's *Voices* tab,
or over a hotkey.

The chain is a clean-room port of the popular EasyEffects *Masc NPR Voice
+ Noise Reduction* preset:

| Stage | EasyEffects | Our node |
| --- | --- | --- |
| Noise removal | rnnoise + DeepFilterNet (neural) | `denoise` (spectral, per-bin noise floor) |
| Between-speech cut | gate −50 dB / −12 dB | `gate` (hysteresis 3 dB) |
| Rumble filter | Hi-pass 80 Hz, slope ×2 | two cascaded `biquad` highpasses |
| Tone shaping | Bell 220 −2, Bell 350 −2, Bell 3.5k +2, Hi-shelf 10k +2 | four `biquad` (peaking/shelf) |
| Leveling | Compressor −18 dB, 3:1, knee 6, +3 makeup | `compressor` |
| Sibilance | De-esser 4 kHz, −22 dB, 3:1 | `de-esser` |
| Safety ceiling | Limiter −1.5 dB | `limiter` |

Differences worth knowing: our denoiser is spectral (not neural), so it
costs ~21 ms of latency and needs about a second to learn the room; the
EasyEffects `wet`/`dry`/`lookahead` options are not modeled (our limiter
is lookahead-free with instant attack).

Once the room is learned, snapshot it into the preset with
`{"action":"captureNoiseProfile","node":"denoise"}` — the `profile`
lands in the preset file (non-public, machine data) and the next load
skips the warmup. Presets without a profile keep learning dynamically.

Low-latency mode (opt-in): set the denoise `frame` to 512
(`setVoiceParameter denoise.frame 512`, or the *Voices* tab) to halve
the STFT window (≈21.3 ms → ≈10.7 ms chain latency). The tradeoff is
frequency resolution — 93.8 Hz bins, so bass selectivity suffers — and
a switch drops live channel state while carrying the learned floor
across (re-capture the profile afterwards anyway for the exact room).
The *Health* tab proves the gain: per-node `lat ms` plus the chain
total, next to the µs CPU cost.

### Factory persona voices

The first boot also seeds five character voices (same mechanism, same
never-overwrite rule): `robot` (tube grit + raised formants + resonant
echo), `chipmunk` (+9 semitones pitch and formants), `demon` (−7 pitch,
dark formants, cave reverb), `radio` (telephone band + speaker grit),
`telephone` (narrow dry handset). All built from shipped DSP nodes only,
all validated by `vm check-preset` and golden-tested (pitch targets,
band shape, finite output).

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
`startAudioStreaming`, `getAudioStreamStatus`, `getHealth`, `addHotkey`,
`removeHotkey`, `listHotkeys`, `setMuted`, `getVoices`, `getBitmap`,
`setSoundboardMasterVolume`, `selectInput`, `getVoices`,
`captureNoiseProfile` (23 total — `vm actions`).

Events: `voiceChanged`, `voiceParameterChanged`, `soundStarted`,
`soundStopped`, `voiceChangerToggled`, `mutedChanged`, `streamStatus`,
`devicesChanged`.

Hardware buttons (Stream Deck / Bitfocus Companion) speak this API
directly — action map, payload examples and a module sketch live in
[`docs/streamdeck-companion.md`](docs/streamdeck-companion.md).

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
