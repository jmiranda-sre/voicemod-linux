# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
semantic versioning.

## [Unreleased]

### Added (F-0.4.D: full CLI + device profiles)

- `vm call '<action-json>'`: 100% of the Control API from shell
  (port scan, broadcast-skipping replies, exit 1 on `ok:false`) —
  scripts, hyprland binds and cron drive everything.
- Device→voice profiles (`set/remove/listDeviceProfiles`, persisted
  in `config.json` schema v2 with v1 migration): hotplug arrival,
  `selectInput` and boot all autoswitch bound inputs (uid, uid base
  without `#id`, or device name); same voice → no-op, missing files
  fail loud without touching state.
- Config save path hardened: one `transact_config` (fixed lock order,
  dual rollback) replaces the per-caller save that could self-deadlock.

### Fixed (mic-npr-voice recalibration)

- Denoise retuned to replace the *two* stacked neural denoisers it
  stands in for: `strength` 2.0 → 3.0, floor −35 → −45 dB. Pauses go
  near-silent (×0.003), breath puffs crushed (×0.042), stationary
  noise −17.5 dB, speech preserved (×0.85) — locked by test bars.
- Hi-pass corrected to a single 12 dB/oct stage per the EasyEffects
  `Slope x2 (12 dB/oct)` spec (was two cascaded = 24 dB/oct).
  Gate/compressor/EQ/de-esser/limiter values were already exact.
- Boot now logs when a seeded file differs from the factory version
  (seed never overwrites): delete the file to receive recalibrations.

## [0.3.0] — 2026-10-16

### Added (F-13: autostart + tray)

- `vm autostart status|enable|disable`: XDG desktop entry
  (`vm-ui --minimized`) + `systemctl --user` wiring for the existing
  `vm-daemon.service`; honest partial-failure reports, sandbox portal
  guidance instead of acting.
- UI system tray (StatusNotifier SNI via `ksni`, icon drawn in code):
  close-to-hide with the WS link untouched, live mute checkmark, Quit;
  `--minimized` boots into the tray. Deliberately GTK-free (the default
  AppIndicator backend dragged 32 MB of ICU/GTK into the AppImage:
  45.6 MB → 17.2 MB); legacy XEmbed trays unsupported, SNI hosts
  (Plasma, GNOME + extension) covered.
- UI snapshot learns `muted` (`mutedChanged` + health seed) for the
  tray checkmark.
- Sandbox: no tray, plain window; autostart delegated to the Flatpak
  Background permission with an explanatory message.

### Added (F-6: factory persona voices)

- Five built-in personas, shipped DSP nodes only: `robot`, `chipmunk`
  (+9 st), `demon` (−7 st + cave), `radio` (band + grit + small room),
  `telephone` (narrow dry band). Seeded on first boot, never overwrite.
- Golden audio tests per persona (pitch targets ±5–7%, band-shape
  directionals, finite output); `vm check-preset` validates all five.

### Added (F-5: low-latency mode + per-node latency)

- `AudioNode::latency_samples` (default 0) with real group delays:
  denoise STFT frame, pitch/formant 2048, delay line; `Graph`
  sums the chain budget.
- Denoise `frame` param (512/1024, default 1024): low-latency opt-in
  halves the window (≈21.3 ms → ≈10.7 ms). FFT plans + window rebuild
  on the control plane; channels reset with the *live* floor carried
  across (resampled) so a switch costs no re-learn; profiles
  resample both ways, so a switch can never brick a preset load.
- `GetHealth` rows carry `latMs` plus a chain `latencyMs` total; the
  UI *Health* tab shows per-node ms, the chain budget and the
  bass-resolution tradeoff note (93.8 Hz bins at 512).
- `mic-npr-voice` ships an explicit `frame: 1024` (editable in the UI).

### Added (F-3: saved noise profiles)

- Preset values grow a `FloatArray` kind (JSON arrays; serde-untagged,
  last variant) and DSP `ParamValue` mirrors it — scalar paths
  (`as_float`, UI sliders, `setVoiceParameter`) reject arrays as
  `TypeMismatch`, never silently coerce.
- `denoise` learns a `profile` param: installs the 513-bin floor on
  all channels (+ a stash for channels created later, so the boot
  install before the first quantum is never a silent no-op) and reads
  it back via the new `AudioNode::get_param` (default: none).
- `captureNoiseProfile{node}` action ("learn room" button): snapshots
  the live floor into the current voice's preset file as a non-public,
  range-less `profile` (re-validated before touching disk; every other
  param untouched). Next load skips the ~1 s warmup.
- UI renders a stored profile as a read-only "N bins stored" row.
- Backcompat: presets without `profile` learn dynamically exactly as
  before (covered by an explicit test).
- `vm actions` lists `captureNoiseProfile` (23 total).

### Added (F-8: Stream Deck / Companion surface)

- `GetVoices` action (sorted preset stems for Companion dropdowns).
- `GetHealth` now also returns `enabled` + `muted`: a freshly connected
  client initialises all button states without toggling anything.
- `StopAllSounds` emits `SoundStopped` per playing sound (the event
  existed since Fase 3b but nothing ever emitted it).
- `docs/streamdeck-companion.md`: action map, event→button-state table,
  payload examples, Companion module sketch, reconnect guidance.
- E2E test replays the Companion loop over a real socket
  (select → voiceChanged, mute → mutedChanged, toggle → event, health
  snapshot, voices list) with strict id correlation.
- Anti-collision test now covers all 22 actions (envelope `id`/`token`
  can never be shadowed by an action field again).

### Added (F-B: hotkeys, PTT/mute, persistence)

- Control API: `addHotkey{codes,target,enabled}` (validated: 1–8 codes
  in evdev range, non-empty target names), `removeHotkey{id}`,
  `listHotkeys`, `setMuted{muted}` + `mutedChanged` event; `vm actions`
  lists all 21 actions.
- Momentary targets `PushToTalk` (changer on while held) and
  `PushToMute` (silence while held); evdev watcher now delivers
  press/release edges, releasing only chords that stop matching.
  `ToggleMute` is real (master mute zeroes output, DSP stays live).
- Binds persist in `$VM_CONFIG_DIR/config.json` (schema version 1,
  ids stable across restarts); mutate-then-save is atomic with
  in-memory rollback, corrupt files are quarantined to `.bak`.
- Flatpak sandbox detected (`FLATPAK_ID`/`/.flatpak-info`) with a plain
  message instead of the generic permissions hint.

### Added (F-A: RT health + watchdog)

- Per-node DSP telemetry: `Graph` times every node per quantum
  (vDSO clock, EMA α=1/16 + latched peak + call count, pre-sized —
  zero RT allocation) and exposes it via `telemetry()`.
- Graph-side xrun counter: a quantum that finds the graph locked
  (preset swap in flight) passes audio through and counts instead of
  blocking the RT thread.
- New Control API action `getHealth`: voice, link state, graph xruns,
  per-node µs table, per-stream stats (capture/playback/monitor) and
  watchdog state (`restarts`, `lastReason`, `lastRestartUnix`).
- Watchdog: 2 s tick runs a pure, unit-tested policy — restart the
  pipeline (stop → reopen stored topology → start) on `NeedsRestart`
  link or xrun storm (≥20 xruns/tick), with a 10 s attempt cooldown so
  a dead backend costs one `open()` per cooldown, never a hot loop.
- UI *Health* tab: node µs table, stream rows with red xrun
  highlighting, watchdog restarts + last reason.
- Control server logs each busy candidate port before falling through
  (second instance no longer dies silently — backlog item closed).

## [0.2.0] — 2026-10-15

### Added

**Mic-processing section (`vm-dsp`)**
- `denoise`: spectral noise suppressor — per-bin noise floor with two
  time constants (fast fall; rise fast only when the frame is quiet,
  −40 dBFS, so sustained speech is never learned as noise), over-subtracted
  Wiener-style gain with a hard `reduction_db` floor, per-bin attack /
  release smoothing plus a 3-tap spectral smooth against musical noise.
  STFT 1024/hop 256 Hann+WOLA → ~21 ms latency; phase untouched.
- `gate`: noise gate with RMS detector, hysteresis (close point below the
  open point), attack/release ballistics, `reduction_db` floor.
- `compressor`: feed-forward downward compressor — symmetric RMS
  detector, soft-knee static curve, dB-domain attack/release smoothing,
  static makeup. Ratio 1 is a bit-exact no-op.
- `limiter`: lookahead-free peak limiter — instant attack, release-timed
  recovery; output peak never crosses `threshold_db`.
- `de-esser`: split-band compressor — low band through an RBJ lowpass,
  high band as the exact complement `x − low` (keeps the split
  phase-exact), reduction applied to the high band only.
- `biquad` now selects its type at runtime (`lowpass`, `highpass`,
  `peaking`/`bell`, `lowshelf`, `highshelf`, `bandpass`, `notch`) with
  `freq_hz`/`q`/`gain_db` params — the EQ work deferred from Fase 3b.
  Coefficients are the RBJ cookbook, recomputed on param/rate change.

**Ready-made presets (`vm-preset`, `vm-daemon`)**
- Built-in `mic-npr-voice`: a faithful port of the EasyEffects *Masc NPR
  Voice + Noise Reduction* chain (denoise → gate → 5-band EQ → 3:1
  compressor → 4 kHz de-esser → −1.5 dB limiter), embedded in the binary
  via `BUILTIN_PRESETS` and seeded into `~/.config/vm/presets/` on first
  boot (never overwrites user edits).

### Verified

- 55/55 workspace tests (+22): dynamics/denoise/EQ units (static-curve
  accuracy, gate open/close, limiter ceiling, split-band complement),
  builtin validation, seed-preserves-edits, and an end-to-end run of the
  full mic chain over noise through the graph processor (finite output,
  stationary noise attenuated).

## [0.1.0] — 2026-10-15

First public release: a from-scratch, open-source clean-room port of the
Voicemod *concept* for Linux (no proprietary code, assets or data).

### Added

**Engine (`vm-daemon`, headless)**
- Real-time PipeWire backend: capture → DSP graph → virtual-mic null-sink
  (`vm-mic` + `vm-monitor`) with explicit links (survives WirePlumber
  changes), formats S16/S24/S32/F32, chunk-disciplined buffers.
- DSP nodes: gain, mute, mix, biquad, delay (echo/slapback), pitch shifter
  (phase-vocoder + resample, cepstrum), formant pitch shifter, reverb, tube
  saturation, oscillator.
- Soundboard: Ogg/Opus demux + decode, resampler, per-sound and master
  mixing with fade-out.
- Preset system (JSON) with float/int/bool/text parameters and node graphs.

**Control plane (`vm-control`)**
- WebSocket Control API (default port 8765) with 16 actions, 6 broadcast
  events and opt-in token auth (`VM_CONTROL_TOKEN`).
- Request/reply correlation via optional request `id` (echoed flat in the
  reply).
- evdev hotkey watcher infrastructure (Keybind model, per-target binds,
  device scanning). *Bindings themselves are not user-configurable yet.*

**Interface (`vm-ui`)**
- Native egui client: Voices / Soundboard / Devices tabs, live sliders,
  bool/int/text widgets, soundboard triggers, device selection, streaming
  status (xruns, latency, link state).
- Daemon lifecycle: connects to a running daemon, launches one on demand,
  never duplicates an existing instance.

**Packaging**
- Flatpak manifest (`io.github.voicemodlinux.vm`), AppImage recipe with
  bundled PipeWire/Opus, AUR PKGBUILD + .install.
- systemd `--user` unit, udev rule for rootless hotkeys, `.desktop` entry,
  icon and AppStream metadata.
- MIT OR Apache-2.0 dual licensing.

### Verified

- 33/33 workspace tests.
- End-to-end audio: 440 Hz tone through the live graph (~50:1 SNR vs
  background).
- Flatpak: full sandbox build + daemon run inside the sandbox (virtual mic
  created, Control API served).
- AppImage: bundled artifact smoke-tested.
- AUR: full `makepkg` build, package contents audited.

### Known limitations (post-MVP backlog)

- Voice bindings for global hotkeys are not yet configurable (API action +
  persistence).
- AI/ML voices (ONNX) are out of scope for 0.1.0.
- Absolute latency with spectral voices (STFT 2048) not yet re-measured;
  adaptive FFT sizing (2048→1024/512) is planned.

[0.1.0]: https://github.com/jmiranda-sre/voicemod-linux/releases/tag/v0.1.0
