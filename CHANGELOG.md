# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
semantic versioning.

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
