# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
semantic versioning.

## [0.4.3] — 2026-10-17

### Fixed

- **Hear Myself never auto-arms at boot**: a monitor left enabled in the
  previous session used to come back on when the daemon started (sound
  at login, acoustic-feedback risk with speakers). The runtime now
  always boots with the monitor off; the card, the `ToggleMonitor`
  hotkey and `vm call` still enable it on demand, and the switch keeps
  persisting within the session.
- **Boot input pick follows the system default source** (`pactl
  get-default-source`): "first input in the enumeration list" is not
  stable across boots and could capture the onboard analog jack instead
  of the USB microphone — extremely hot/distorted capture and physical
  mic knobs with no effect. `VM_INPUT_UID` still wins when set, and the
  startup log now says why the device was chosen.

## [0.4.2] — 2026-10-17

### Added (Hear Myself, F-0.4.2)

- **Hear Myself** card at the top of the *Voices* tab: a prominent
  ON/OFF toggle with live status, a quick mute, the output picker
  (physical outputs plus *system default*), the return-volume slider
  and an *Advanced* drawer with the dry/wet blend and a headphones tip
  against acoustic feedback. The duplicated monitor block was removed
  from the *Integrations* tab, and the card follows the daemon:
  state changes made outside the window (hotplug fallback, hotkey,
  `vm call`) always win over stale local edits.
- `ToggleMonitor` as a global-hotkey target (same API action the card
  and the CLI fire).
- `packaging/smoke/monitor-lifecycle.sh` — real-PipeWire release gate:
  fresh wet default, enable → volume/blend edits, retarget to a
  temporary null-sink, back to the default, unplug fallback, disable;
  asserts wire state, disk persistence (schema v5), the WebSocket
  `monitorChanged` frames and FL↔FL / FR↔FR port pairing.

### Changed

- Monitor defaults now favor **hearing the processed voice**: `mix`
  defaults to `1.0` (wet, what callers hear) instead of `0.0`; configs
  that never touched the blend migrate to wet while an explicit `0.0`
  is preserved. Config schema bumped to v5.
- `"device":""` now means *the system default, followed live* — the
  daemon re-links when the default sink changes, and a pinned output
  that disappears falls back to the default (persisted as `""`) while
  the monitor stays on, re-linking by itself once a device is back.
- Monitor targets are validated: only physical outputs (role `Sink`,
  outside the `vm-*` namespace) are accepted; a rejected device rolls
  back without disturbing the active route and reports why.

### Fixed

- Stereo links are paired by channel tag (`FL↔FL`, `FR↔FR`) instead of
  PipeWire port-id order, which differs between nodes (streams vs ALSA
  devices) — the old pairing shipped swapped channels nondeterministically.

## [0.4.1] — 2026-10-17

### Changed (desktop integration, F-0.4.1.A)

- The window is now a first-class desktop citizen: the Wayland window
  announces the canonical `io.github.voicemodlinux.vm` app-id (the
  AppStream/desktop ID) instead of its full title, so compositors pair
  the window with the installed `.desktop` entry — and therefore its
  icon — in menus, taskbars and stores; the title is now simply
  "Voicemod Linux". The desktop entry gained `StartupNotify=true` and a
  "Start minimized" action (`vm-ui --minimized`, boots straight to the
  tray) in the icon's context menu, and the app icon now ships as the
  SVG plus 256/512 px PNGs under `packaging/icons/hicolor/` for every
  packaging path (AUR, Flatpak, AppImage).

### Added (autostart from the UI, F-0.4.1.B)

- "Start with the system" is now a checkbox in a new "⚙ Preferences"
  dialog in vm-ui (top bar): it installs/removes the XDG autostart
  entry (`vm-ui --minimized` at login), reports enable/disable warnings
  inline, and shows the live daemon unit state. In the Flatpak sandbox
  the switch gives way to an explanation (autostart belongs to the
  permission store there). The dialog and `vm autostart
  enable|disable|status` are two front-ends of the SAME code: the
  autostart module moved from vm-cli into vm-control (tests came
  along), the CLI output stays byte-identical, and both can never
  disagree again.

### Fixed (tray / close lifecycle, vm-ui)

- The window "X" now hides for real: the window is a disposable
  viewport — closing it (X or the tray's "Hide") destroys it while the
  process, daemon WebSocket link and tray keep running, and "Show"
  builds a fresh window (UI state reloads from disk/daemon). The old
  minimize-to-tray used `ViewportCommand::Visible(false)`, which winit
  documents as a no-op on Wayland (`Window::set_visible`): the window
  never actually hid there, and an idle window could sit without ticks
  (the tray channels never wake egui), freezing the menu. `--minimized`
  (autostart) now boots into the tray wait loop without ever creating a
  window — nothing can flash on screen (proof at the Wayland protocol
  level: zero buffer attaches / toplevels). Real quitting stays
  exclusive to the tray's "Quit" item; without a tray (sandbox /
  headless) closing quits normally, so the app can never become
  unreachable. New e2e smoke `packaging/smoke/ui-lifecycle.sh` (real
  session) pins close→tray→quit, `--minimized` boot→Show→close→quit
  and the windowless-boot proof.

### Added (wizard/About polish & store assets, F-0.4.1.C)

- The wizard is now visibly a wizard: four painter-drawn progress pips
  (`● ○ ○ ○` — vector circles, no font glyphs), step titles without
  the `n/4 · ` prefix and at a slightly larger size, and a flow line
  that matches the real sequence (`mic check » 3 s of silence » read
  one sentence » apply the suggestions`). The measurement flow and its
  state machine are untouched.
- New "About" dialog (top-bar button, next to Preferences): app name,
  version (`CARGO_PKG_VERSION`), the clean-room note, the license
  (MIT OR Apache-2.0) and a clickable repository link (egui-winit
  forwards `OpenUrl` to the system browser).
- AppStream/Catalog metadata: `<url type="bugtracker">`, `<keywords>`
  and three real screenshots (voices, soundboard, first-run wizard;
  1068×777) served from `packaging/screenshots/` via raw URLs — ready
  for Flathub, GNOME Software and KDE Discover listings.

### Fixed (vm-ui boot state and glyphs, F-0.4.1.C)

- A freshly connected vm-ui no longer shows "select a voice preset"
  while a voice is live: when the event-driven `selected_voice` has not
  arrived yet, the tab falls back to `Health.voice` from the status
  poll and mirrors the active preset locally (`load_preset` only fills
  widgets — it sends nothing to the daemon).
- Empty-state pass on the tabs: the preset list says "no presets found
  — click ↻ presets" and the Devices tab says "no devices yet — is the
  daemon running?" instead of rendering silent blanks.
- Glyph audit (screen probe): the bundled fonts lack `→` and `✓` (both
  rendered as tofu); every user-visible occurrence was replaced with
  plain text or `»` (guillemets, verified to render), while
  `↻ ≈ µ · “ ” … − ⚙` were confirmed to render and stay. `∈` existed
  only in a code comment.
- The soundboard passthrough row reads in order now — "App [(none)] to
  [(system default)]" (explicit labels + `from_id_salt`; `from_label`
  draws the text after the combo).

## [0.4.0] — 2026-10-17

### Added (F-0.4.C: routing & integrations)

- Monitor mix (item 15): a hear-yourself path independent of `vm-mic`.
  The dry leg is the raw pre-graph input (zero added latency) blended
  with the processed wet signal via `mix`; `setMonitor` changes
  enable/device/mix/volume/mute live — a device retarget rebuilds only
  the monitor links, the DSP chain never restarts (rejected devices
  roll the setting back). Mute or disable pushes zeros instead of
  leaving a dangling link (no xruns). `getMonitor` + `monitorChanged`
  report the live state; `getLevels` exposes mic/out dBFS for scripts.
- Sidechain ducking (item 14): mic-over-threshold attenuation of the
  soundboard/music bus (defaults: on, 12 dB at −40 dBFS, 50/400 ms
  attack/release). RT-safe by construction — knobs mirror into atomics
  and the gain ramp scales with the quantum, the RT thread never
  blocks. `setDucking`/`getDucking` + `duckingChanged`. The soundboard
  bus now mixes into `vm-mic` output (all paths incl. bypass), so
  ducking is audible exactly where music and voice share one stream.
- OBS integration (item 35): new `vm-obs` crate (pure Rust —
  tokio-tungstenite + sha2/base64) speaking obs-websocket v5: Hello /
  Identify handshake with the salted-SHA256 auth challenge (vector
  tested), event subscriptions General|Scenes. Scene → voice profiles
  auto-switch the voice on scene change
  (`setObsSceneProfile`/`removeObsSceneProfile`/`listObsSceneProfiles`);
  `obsStatusChanged` publishes connect state + current scene. Live VU
  for OBS: `$VM_CONFIG_DIR/obs-vu.txt` is rewritten every 100 ms
  (`[voice] mic -58.3dB out -11.7dB ▇`) — point an OBS Text source at
  the file. `setObs` configures url/password/enabled; inert unless
  enabled (no runtime cost when off).
- MIDI learn (item 37): new `vm-midi` crate (midir/ALSA) behind the
  optional `midi` cargo feature — the default build stays free of ALSA
  deps and answers the MIDI actions without ever opening a port. CC /
  Note bindings drive any hotkey target (ToggleMute, PushToTalk,
  PlaySound, SelectVoice…), or a voice parameter with min/max range
  mapping (`SetVoiceParameter`); triggers fire on the press edge
  (Note-off never double-fires toggles). `midiLearnStart` captures the
  next event into a persisted binding (`midiLearned`,
  `midiBindingsChanged`); `midiBindAdd`/`midiBindRemove`/
  `listMidiBindings`/`setMidi`/`getMidiStatus` complete the surface.
- Config schema v3 (`monitor`, `ducking`, `obs`, `midi` sections);
  partial updates keep every other field, persist-then-apply so a
  rejected change never sticks.
- `vm actions` now prints the catalogue from `vm_control::ALL_ACTIONS`
  (43 actions) — one source of truth, guarded by a test that
  serializes every `Action` variant and compares names in order.

### Fixed (F-0.4.C)

- Soundboard files never loaded at runtime: `decode_to_loaded`
  returned `Err("unreachable")` on every call, so `playSound` replied
  OK but ran silent. Now it decodes through `decode::decode_file`;
  the live smoke shows the output level jump to −11.7 dB during
  playback (previously flat at the mic noise floor).
- MIDI open failures are logged once per distinct error instead of
  every 2 s retry.

### Added (F-0.4.B: soundboard pro + UX)

- First-run setup wizard (item 29): guided modal (microphone test →
  3 s room-noise measurement → gate → speech level → makeup/limiter
  trim) that opens automatically on the first UI run (`wizard.json`
  marker in the config dir; "Setup wizard" in the top bar reopens it).
  It samples distinct level readings, takes the median, captures the
  denoise noise profile (`captureNoiseProfile`) and suggests a gate
  (floor + 6 dB, clamped to the node range) plus a makeup gain that
  brings speech to −18 dBFS — each applied with one click. Levels
  poll at 100 ms only while the wizard is open.
- Graphical EQ (item 26): interactive response curve in the *Voices*
  tab. Drag a handle (horizontal = frequency, vertical = gain),
  double-click resets the gain, sliders fine-tune frequency/Q/gain of
  the selected band. The curve is drawn from the exact DSP math
  (`vm_dsp::biquad_magnitude_db` — analytic response cross-checked
  against the measured biquad in the test suite), so canvas and audio
  cannot drift.
- Per-application passthrough (item 18): route one app's playback into
  a `vm-passthrough` null-sink, forward it to any output through a
  20 ms loopback (`setPassthrough`/`getPassthrough`; empty device =
  system default; `listPassthroughApps` feeds the picker). Exactly the
  chosen app sits on the sink and strays are restored; re-running the
  action converges; an unknown app fails loudly and the daemon rolls
  back config and backend. Soundboard playback injects into the same
  sink (`vm-inject`, post-ducking, independent of the master mute).
  Boot re-applies the saved state, adopting and cleaning leftovers
  from crashed sessions.
- Instant Replay (item 20): always-on ring of the final output
  (post-mute), 30 s by default. `saveInstantReplay` (or the
  `SaveReplay` hotkey) writes the window as PCM16 stereo 48 kHz WAV to
  `$VM_CONFIG_DIR/replays/replay-<unix>.wav` — hand-rolled RIFF
  writer, zero new dependencies; the RT thread only `try_lock`s
  (contention = skip). `setInstantReplay`/`getInstantReplay`
  (5..=120 s, `buffered_seconds`); disabling clears the history.
- Config schema v4 (`replay`, `passthrough` sections; v1–v3 configs
  migrate forward).
- UI: new *Integrations* tab surfaces monitor mix, ducking, OBS and
  MIDI; the *Soundboard* tab gains instant-replay and passthrough
  rows; *Devices* splits Input / Output (sinks) / Virtual+monitor.

### Fixed (F-0.4.B)

- Live passthrough teardown bug (caught by the live smoke): adopting
  a `vm-passthrough` sink left over from a previous session recorded
  no module id, and the next loopback rebuild purged the adopted
  null-sink itself — boot-apply then moved apps toward a sink name
  that no longer existed (`No such entity`). Sink and loopback purges
  are now separate: the adopted sink survives and boot-apply re-routes
  the app as intended.

### Added (F-0.4.A: new-generation DSP)

- `rnnoise` node: Xiph RNNoise denoiser via the pure-Rust
  `nnnoiseless` port — weights embedded (no files, no downloads),
  exact 10 ms frames, `mix` param. The neural alternative to the
  spectral `denoise`; the shipped `mic-npr-voice` keeps the spectral
  node (faithful to its EasyEffects reference).
- `deepfilter` node (opt-in `neural` cargo feature): DeepFilterNet3
  runtime over ONNX bundles via tract — ERB masking + complex deep
  filtering, hyperparameters read from the bundle's own `config.ini`,
  fail-loud on missing/corrupt models, pass-through without one
  (20 ms latency).
- `autogain` node: EBU R128 loudness rider (libebur128 K-weighting,
  400 ms gated blocks, dB follower) — `target_lufs`, `max_gain_db`,
  `adapt_ms`; zero latency; freezes on silence instead of winding up.
- `autotune` node: YIN pitch detection + phase-vocoder correction —
  `key`, `scale` (major/minor/chromatic), `mix`, `speed_ms`; hard
  T-Pain snap or gentle nudge through the same engine as `pitch-fft`.
- `convolver` node: uniform partitioned convolution (1024-sample
  partitions, exact 21 ms wet delay) against WAV IRs (PCM16/24/32,
  float32; any rate; 4 s cap) — `ir_path`, `mix`.
- Control-plane node preparation (`AudioNode::prepare`): the graph
  builder reports the real lane count at selectVoice, so the
  DeepFilter sessions (ONNX parse + tract plan + warm-up) are built
  off the RT thread — live-measured first-hop worst case drops from
  ~81 ms to ~5 ms (avg ~3.4 ms per 20 ms quantum).
- Preset schema knows all five types; daemon E2E covers
  preset→selectVoice→audio for each, plus a `KNOWN_NODE_TYPES` ↔
  factory drift guard. Clippy clean on both feature sets.

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

### Fixed (packaging)

- AppImage bundling now creates SONAME symlinks (`libpipewire-0.3.so.0`
  → `libpipewire-0.3.so.0.1609.0`): the loader looks libraries up by
  SONAME, so without them `LD_LIBRARY_PATH` never matched and the
  bundled libs were silently ignored in favor of host ones.

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
