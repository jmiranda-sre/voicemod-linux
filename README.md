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
  `vm actions`, `vm keyboards`, …). `vm call '<action-json>'` sends any
  Control API action and prints the reply (exit 1 when `ok` is false),
  so the whole API is scriptable from shell, hyprland binds and cron:
  `vm call '{"action":"selectVoice","voice":"demon"}'`.
* **`vm-control`** is the shared protocol crate (actions, events, server).
  Any WebSocket client — Stream Deck plugins, scripts, Home Assistant — drives
  the daemon with the same protocol.

### Device profiles (per-microphone voices)

Bind a device (uid, uid base without the `#id` suffix, or human name)
to a voice — plugging it in (or `selectInput` to it) auto-selects the
voice, and the boot input starts on its voice too:

```bash
vm call '{"action":"setDeviceProfile","device":"Fifine Microphone","voice":"mic-npr-voice"}'
vm call '{"action":"listDeviceProfiles"}'
```

Bindings persist in `config.json`; re-plugging never fights an explicit
`selectVoice` (same voice → no-op).

## Building from source

Dependencies (Debian/Ubuntu names):

```
rust (≥1.74)  cargo  clang (libclang, needed by bindgen)  pkg-config
libpipewire-0.3-dev  libopus-dev
```

The optional `midi` feature (see *MIDI learn* above) additionally needs
`libasound2-dev`. Everything else builds without ALSA.

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

## UI or CLI — your call

**GUI-first, CLI intact.** Everyday use happens in the window — no
terminal needed — and everything the window does maps to a CLI command
against the same daemon, so scripts and power users keep every option.

| Task | GUI (`vm-ui`) | CLI (`vm`) |
| --- | --- | --- |
| Load a voice | click a preset in **Voices** | `vm call '{"action":"selectVoice","voice":"robot"}'` |
| Play a sound | click a pad in **Soundboard** | `vm call '{"action":"playSound","sound":"sting"}'` |
| Measure the room | **Setup wizard** | `vm call '{"action":"captureNoiseProfile","node":"denoise"}'` |
| Autostart at login | **⚙ Preferences** checkbox | `vm autostart status\|enable\|disable` |
| Health and levels | **Health** tab | `vm call '{"action":"getHealth"}'` |
| Whole API | — | `vm actions` lists every action for scripting |

Where it matters the sharing goes deeper than the daemon: the autostart
toggle is one `vm-control` module used by both front-ends (the CLI
output stays byte-identical to what it always was), so GUI and CLI can
never disagree. Existing scripts, hotkey daemons and `vm call` pipelines
keep working exactly as before.

### Virtual microphone

The daemon creates a null-sink named **`vm-mic`** (plus `vm-monitor`) and links
`capture → DSP graph → vm-mic` explicitly, so WirePlumber-independent updates
keep working. In Discord/OBS/etc. pick the input usually shown as **`vm-mic`**
(Monitor of `vm-mic`). Use the UI's *Devices* tab to pick the physical input
(the environment variable `VM_INPUT_UID` pins one at startup).

### Hear yourself (monitor mix)

The daemon always creates a **`vm-monitor`** sink next to `vm-mic`, but
it stays silent until you turn the monitor on — `vm-mic` is the clean
signal apps record, the monitor is only for your own ears:

```bash
vm call '{"action":"setMonitor","enabled":true,"mix":0.5,"volume_db":-6}'
vm call '{"action":"getMonitor"}'
vm call '{"action":"setMonitor","muted":true}'     # quick mute, settings kept
```

`mix` blends the two legs: `0.0` is the **raw input** (dry — zero added
latency, no DSP), `1.0` is the fully processed voice. `0.5` lets you
hear both, which is the classic "do I sound like that?" check without
running your voice through a second output path. The monitor is
independent of `vm-mic`: muting it (or disabling it) never touches what
apps receive. A device retarget (`"device":"..."`, same names as
`getAudioDevices`) rebuilds only the monitor link, so changing outputs
never interrupts the main chain; a rejected device rolls the setting
back and says why.

### Sidechain ducking

When the mic crosses `threshold_db` the daemon attenuates the
soundboard/music bus by up to `depth_db` — music dips under your voice
and comes back up ~`release_ms` after you stop:

```bash
vm call '{"action":"setDucking","enabled":true,"depth_db":12,"threshold_db":-40}'
vm call '{"action":"getDucking"}'
```

Defaults: enabled, 12 dB at −40 dBFS, 50 ms attack / 400 ms release.
The detector runs on atomics only (the real-time thread never blocks),
so ducking cannot cause xruns. The bus is mixed into `vm-mic`, so the
duck applies exactly where callers hear music and voice together.

### OBS integration (scene → voice + live VU)

OBS scenes can switch your voice automatically (e.g. *Game* scene →
`robot`, *Just Chatting* → your normal mic chain), and OBS can show a
live input meter from the daemon:

1. In OBS: **Tools → WebSocket Server Settings** → enable, note the
   port (default `4455`) and the password (or disable auth).
2. Point the daemon at it, and bind scenes to voices:

```bash
vm call '{"action":"setObs","enabled":true,"url":"ws://127.0.0.1:4455","password":"..."}'
vm call '{"action":"setObsSceneProfile","scene":"Game","voice":"robot"}'
vm call '{"action":"listObsSceneProfiles"}'
vm call '{"action":"getObsStatus"}'   # connected, current scene, url
```

3. For the VU: add a **Text source** in OBS, enable *Read from file* and
   select `~/.config/vm/obs-vu.txt` (inside the Flatpak:
   `~/.var/app/io.github.voicemodlinux.vm/config/vm/obs-vu.txt`). The
   daemon rewrites it ~10×/s with one line:
   `[robot] mic -58.3dB out -11.7dB ▇` — current voice, mic level,
   output level and a level bar.

The client speaks plain obs-websocket v5 (auth handshake included) and
subscribes to events only — it never issues OBS requests, so it cannot
fight the OBS UI. Disabled by default; when off it is dormant.

### MIDI learn (CC / Note)

Any MIDI controller can drive the daemon — faders to parameters, pads
to sounds and mutes. MIDI support is an optional build (ALSA via
`libasound2-dev`); the default binaries answer the MIDI actions but
never open a port:

```bash
sudo apt install libasound2-dev                 # Debian/Ubuntu
cargo build --release -p vm-daemon --features midi
```

Bind by hand or capture the next control you touch:

```bash
# Map CC 7 (channel 1) to the drive amount, 0→4.0
vm call '{"action":"midiBindAdd","channel":0,"kind":"cc","number":7,
          "target":{"SetVoiceParameter":{"param":"drive.gain","min":0.0,"max":4.0}}}'
# Arm learn, then wiggle: the next CC/Note becomes bound to ToggleMute
vm call '{"action":"midiLearnStart","target":{"Trigger":"ToggleMute"}}'
vm call '{"action":"listMidiBindings"}'
vm call '{"action":"getMidiStatus"}'            # connected, port, bindings
```

Targets are the hotkey targets (`ToggleMute`, `ToggleVoiceChanger`,
`PushToTalk`, `{"PlaySound": name}`, `{"SelectVoice": name}`,
`StopAllSounds`) or `SetVoiceParameter` with a min/max range map.
Triggers fire on the press edge only. `setMidi` picks the port
(`{"action":"setMidi","port":"..."}`, empty = first); `midi: listening
on ...` appears in the daemon log once connected, and the learn result
broadcasts as `midiLearned`.

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

The UI lives in the system tray (StatusNotifier/AppIndicator): the
window "X" hides it — the process keeps running with the daemon link
(WebSocket) alive, and the tray menu stays responsive (show/hide, live
mute checkmark, Quit). Closing the window really destroys it (the only
hide that works on Wayland) and "Show" builds a fresh one, so the UI
state reloads from disk/daemon. Quitting for real is exclusive to the
tray's "Quit" item; `vm-ui --minimized` boots straight into the tray
without ever creating a window. Without a tray (Flatpak sandbox — the
SNI host is unreachable there by design — or a headless session) the
window stays a normal window and X closes the app; autostart is owned
by the Flatpak Background permission instead. Tabs: *Voices* (presets,
parameters, EQ), *Soundboard* (sounds, instant replay, passthrough),
*Devices* (input / output / virtual+monitor), *Integrations* (monitor
mix, ducking, OBS, MIDI) and *Health*.

### Presets and soundboard

Runtime data lives in `$VM_CONFIG_DIR` (default `~/.config/vm`):

```
~/.config/vm/presets/*.json     # voice presets (nodes: gain, biquad, delay,
                                # pitch-fft, formant-pitch-shifter, reverb,
                                # tube, oscillator, mute, mix…, plus the mic
                                # section: denoise, rnnoise, deepfilter,
                                # gate, compressor, de-esser, limiter,
                                # autogain, autotune, convolver)
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

### Route an app through the soundboard bus (passthrough)

Feed any application — a call, a game, music — through the same
pipeline the soundboard uses, and out through the output you choose:

```bash
vm call '{"action":"listPassthroughApps"}'         # pickable streams
vm call '{"action":"setPassthrough","enabled":true,"app":"Firefox"}'
vm call '{"action":"setPassthrough","device":""}'  # "" = system default
vm call '{"action":"getPassthrough"}'
```

The daemon creates a `vm-passthrough` sink and a 20 ms loopback to the
target output; exactly the selected app is moved onto the sink and
every other stream is restored to where it was. The action is
declarative — re-running it converges — and an unknown app name fails
loudly, with both the config and the backend rolled back. While it is
on, the soundboard bus is injected into the same sink (`vm-inject`),
so the chosen output gets your sounds alongside the app's audio —
independent of `vm-mic`. A daemon restart re-applies the saved state;
stale sinks from a crashed session are adopted and cleaned.

EasyEffects note: its *Process all output streams* option re-routes
every stream to `easyeffects_sink` and will fight any third-party move
(pavucontrol, Soundux, this feature) — disable that option or
blocklist the app to use passthrough.

### Instant replay (30 s)

Every output frame lands in a rolling 30-second ring (tapped after the
master mute, so muted audio never sneaks into the file). Save the last
window — a quote from the call, a clutch play — whenever you want:

```bash
vm call '{"action":"setInstantReplay","enabled":true,"seconds":30}'
vm call '{"action":"getInstantReplay"}'   # enabled, seconds, buffered_seconds
vm call '{"action":"saveInstantReplay"}'  # → ~/.config/vm/replays/replay-<unix>.wav
```

Or bind the `SaveReplay` hotkey and never touch the terminal. Files
are PCM16 stereo 48 kHz written with no extra dependencies; the
real-time thread only snapshots the ring under `try_lock` and the WAV
write happens off the audio path. Disabling the feature clears the
buffer.

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
| Rumble filter | Hi-pass 80 Hz, 12 dB/oct | one `biquad` highpass |
| Tone shaping | Bell 220 −2, Bell 350 −2, Bell 3.5k +2, Hi-shelf 10k +2 | four `biquad` (peaking/shelf) |
| Leveling | Compressor −18 dB, 3:1, knee 6, +3 makeup | `compressor` |
| Sibilance | De-esser 4 kHz, −22 dB, 3:1 | `de-esser` |
| Safety ceiling | Limiter −1.5 dB | `limiter` |

Differences worth knowing: the shipped chain keeps the spectral
denoiser (portable, zero model files) — the neural `rnnoise` and
`deepfilter` nodes are now drop-in alternatives (see *Studio-grade DSP
nodes* below). The spectral denoiser costs ~21 ms of latency and needs
about a second to learn the room; the EasyEffects `wet`/`dry`/`lookahead`
options are not modeled (our limiter is lookahead-free with instant
attack).

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

### First-run setup wizard

The first UI launch opens a guided calibration ("perfect audio in a
minute"): a visual microphone test, a 3-second room-noise measurement,
a suggested gate threshold (measured floor + 6 dB, clamped to the
preset's declared range), a speech-level pass that sizes the makeup
gain toward −18 dBFS, and a limiter trim — every suggestion applies
with one click, and measuring the noise floor also captures the
denoiser's noise profile into the active preset. A `wizard.json`
marker in the config dir remembers the run; *Setup wizard* in the top
bar reopens it any time. Levels refresh at 100 ms while it is open and
return to the normal pace afterwards.

### Graphic EQ

Every biquad in the active voice appears as a draggable handle on the
response curve in the *Voices* tab: horizontal = frequency, vertical =
gain (double-click resets it), and the sliders under the canvas
fine-tune frequency, Q and gain of the selected band. The curve is
computed with the same biquad math the DSP runs
(`biquad_magnitude_db`), so what you see is what you hear — the
analytic response is cross-checked against the measured filter in the
test suite.

### Studio-grade DSP nodes

v0.4.0 adds five new-generation nodes to the factory — write them into
any preset (`vm check-preset` validates first):

| Node | Role | Params | Latency |
| --- | --- | --- | --- |
| `rnnoise` | Xiph RNNoise neural denoiser (pure-Rust `nnnoiseless`; weights embedded — no files, no downloads) | `mix` | 10 ms |
| `deepfilter` | DeepFilterNet3 (ONNX via tract) over user-supplied model bundles | `model_path`, `atten_db`, `erb_thresh`, `df_thresh`, `postfilter_beta`, `mix` | 20 ms |
| `autogain` | EBU R128 loudness rider — whispers come up, shouts come down, silence stays frozen | `target_lufs` (−40..−10), `max_gain_db`, `adapt_ms` | 0 |
| `autotune` | Real-time pitch correction (YIN + phase-vocoder): hard T-Pain snap or gentle nudge | `key`, `scale` (major/minor/chromatic), `mix`, `speed_ms` | 43 ms |
| `convolver` | True convolution reverb from any WAV impulse response (rooms, cabinets) | `ir_path`, `mix` | 21 ms |

Swap RNNoise into your copy of `mic-npr-voice.json` (replace the
`denoise` node with
`{"id":"rn","type":"rnnoise","params":{"mix":{"value":1.0}}}` and drop
`strength`/`reduction_db`/`profile`) for a file-free neural chain.

`deepfilter` is opt-in twice over: build the binaries with the feature,
and point the node at a DeepFilterNet3 bundle (a directory or `.tar.gz`
with `enc.onnx`, `erb_dec.onnx`, `df_dec.onnx` and `config.ini` — e.g.
from the `deepfilternet3-onnx` release). Hyperparameters are read from
the bundle's own `config.ini`; a missing or corrupt model fails at
`selectVoice` time (fail loud, never a silent mic); without
`model_path` the node passes audio through.

```bash
cargo build --release -p vm-daemon --features vm-dsp/neural
```

`convolver` reads mono/stereo WAV IRs (PCM16/24/32-int or float32, any
sample rate — resampled and peak-normalized) up to 4 s long; the wet
path is delayed exactly one 1024-sample partition (21 ms), so latency
stays quantum-independent and the dry path (via `mix`) stays
sample-exact.

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

Actions (43 total — `vm actions` prints the catalogue):
`selectVoice`, `toggleVoiceChanger`, `stopAllSounds`, `playSound`,
`getAllSoundboard`, `setVoiceParameter`, `resetVoiceParameters`,
`setDefaultMicrophone`, `getAudioDevices`, `getUserLicense`, `fadeSound`,
`startAudioStreaming`, `getAudioStreamStatus`, `getHealth`, `getBitmap`,
`setSoundboardMasterVolume`, `selectInput`, `addHotkey`, `removeHotkey`,
`listHotkeys`, `setMuted`, `getVoices`, `captureNoiseProfile`,
`setDeviceProfile`, `removeDeviceProfile`, `listDeviceProfiles`,
`setMonitor`, `getMonitor`, `setDucking`, `getDucking`, `getLevels`,
`setObs`, `getObsStatus`, `setObsSceneProfile`, `removeObsSceneProfile`,
`listObsSceneProfiles`, `getMidiStatus`, `setMidi`, `midiLearnStart`,
`midiLearnCancel`, `midiBindAdd`, `midiBindRemove`, `listMidiBindings`.

Events: `voiceChanged`, `voiceParameterChanged`, `soundStarted`,
`soundStopped`, `voiceChangerToggled`, `mutedChanged`, `streamStatus`,
`devicesChanged`, `monitorChanged`, `duckingChanged`, `obsStatusChanged`,
`midiLearned`, `midiBindingsChanged`.

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
| MIDI bindings never fire | the default build has no MIDI input — build `vm-daemon --features midi` (needs `libasound2-dev`); check `getMidiStatus.connected` |
| OBS never switches voice | obs-websocket disabled or wrong password — `getObsStatus` shows `connected` + live scene; scene names must match `setObsSceneProfile` exactly |
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
