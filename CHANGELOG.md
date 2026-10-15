# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
semantic versioning.

## [Unreleased]

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
