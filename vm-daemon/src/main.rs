//! `vm-daemon`: headless pipeline orchestrator.
//!
//! Owns the live runtime (`DaemonState`) and serves it over the Control
//! API + global hotkeys. `--live` selects the real PipeWire backend
//! (falls back to the stub with a clear message when unreachable).

mod replay;

use replay::{replay_path, write_wav_pcm16, ReplayRing};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc, Mutex,
};
use vm_audio::{
    AudioBackend, DeviceRole, MonitorSettings, PassthroughSettings, StreamProcessor,
    StreamStopReason, StreamTopology, SAMPLE_RATE,
};
use vm_control::{
    Action, ControlApiServer, ControlEvent, ControlHandler, ControlReply, DuckingConfig,
    HotkeyManager, HotkeyTarget, Keybind, MidiBinding, MidiConfig, MidiEvent, MidiKind,
    MidiTarget, MonitorConfig, ObsConfig, PassthroughConfig, ReplayConfig, ServerConfig,
};
use vm_dsp::{Graph, NodeConstructorContext, NodeFactory, NodeId, ProcessContext};

/// Ducking knobs mirrored to atomics so the RT processor never blocks on
/// a control-plane mutex (F-0.4.C item 14).
#[derive(Default)]
struct DuckingRt {
    enabled: AtomicBool,
    depth_db: AtomicU32,
    threshold_db: AtomicU32,
    attack_ms: AtomicU32,
    release_ms: AtomicU32,
}

/// Live level snapshot for `getLevels` / the OBS VU source: written by the
/// RT processor, read by the control plane.
#[derive(Debug, Clone, Copy)]
struct LevelsSnapshot {
    mic_db: f32,
    out_db: f32,
    speech: bool,
}

impl Default for LevelsSnapshot {
    fn default() -> Self {
        // Silence floor, not 0 dB: a fresh `getLevels` must read as quiet.
        Self { mic_db: -100.0, out_db: -100.0, speech: false }
    }
}

/// obs-websocket bridge runtime status (F-0.4.C item 35).
#[derive(Debug, Default, Clone)]
struct ObsStatus {
    connected: bool,
    scene: String,
    url: String,
}

/// MIDI input runtime state (F-0.4.C item 37).
#[derive(Debug, Default, Clone)]
struct MidiRuntime {
    /// Armed learn target: the next incoming event binds to it.
    learn: Option<MidiTarget>,
    connected: bool,
}

/// RT DSP entry: runs the voice graph, or passes through when the voice
/// changer is bypassed. Graph + flag shared with the control plane.
///
/// F-0.4.C: also mixes the soundboard into the vm-mic (VAD bus) and the
/// hear-myself monitor (monitor-only bus), and applies sidechain ducking
/// driven by the raw mic level.
struct GraphProcessor {
    graph: Arc<Mutex<Graph>>,
    voice_enabled: Arc<AtomicBool>,
    /// Master mute: output goes silent while the DSP state stays live
    /// (reverb tails don't burst on unmute).
    muted: Arc<AtomicBool>,
    /// Lock-contention counter: a quantum that finds the graph locked
    /// (preset swap in flight) passes the previous buffer through and
    /// counts as a graph-side xrun instead of blocking the RT thread.
    graph_xruns: Arc<AtomicU64>,
    /// Soundboard mixer, shared with the control plane (play/stop/volume).
    mixer: Arc<Mutex<vm_soundboard::SoundboardMixer>>,
    /// Ducking knobs (atomics) read every quantum.
    ducking: Arc<DuckingRt>,
    /// Level snapshot shared with `getLevels` / the OBS bridge.
    levels: Arc<Mutex<LevelsSnapshot>>,
    lane_in: Vec<Vec<f32>>,
    lane_out: Vec<Vec<f32>>,
    /// Soundboard buses (reused; `mix_into` zeroes them each call).
    vad_bus: vm_soundboard::StereoBus,
    mon_bus: vm_soundboard::StereoBus,
    /// Interleaved stereo monitor-only bus (`mon − vad`), returned by
    /// `monitor_bus()` for the hear-myself mix.
    mon_extra: Vec<f32>,
    /// Interleaved stereo full soundboard bus (post ducking, no voice)
    /// returned by `inject_bus()` for the passthrough injection (18).
    inj_extra: Vec<f32>,
    /// Instant replay ring (F-0.4.B item 20): the final outputs are
    /// appended under `try_lock` (a contended lock skips the quantum).
    replay: Arc<Mutex<ReplayRing>>,
    /// Smoothed duck gain in dB (0 = unity).
    duck_db: f32,
    speech: bool,
}

impl GraphProcessor {
    // Each store is an independent RT-shared handle; bundling them in a
    // struct would only add a name for the same wiring.
    #[allow(clippy::too_many_arguments)]
    fn new(
        graph: Arc<Mutex<Graph>>,
        voice_enabled: Arc<AtomicBool>,
        muted: Arc<AtomicBool>,
        graph_xruns: Arc<AtomicU64>,
        mixer: Arc<Mutex<vm_soundboard::SoundboardMixer>>,
        ducking: Arc<DuckingRt>,
        levels: Arc<Mutex<LevelsSnapshot>>,
        replay: Arc<Mutex<ReplayRing>>,
    ) -> Self {
        Self {
            graph,
            voice_enabled,
            muted,
            graph_xruns,
            mixer,
            ducking,
            levels,
            lane_in: Vec::new(),
            lane_out: Vec::new(),
            vad_bus: vm_soundboard::StereoBus::default(),
            mon_bus: vm_soundboard::StereoBus::default(),
            mon_extra: Vec::new(),
            inj_extra: Vec::new(),
            replay,
            duck_db: 0.0,
            speech: false,
        }
    }

    /// Build a processor wired to the live state's shared stores (tests).
    #[cfg(test)]
    fn for_state(state: &Arc<DaemonState>) -> Self {
        Self::new(
            state.graph.clone(),
            state.voice_enabled.clone(),
            state.muted.clone(),
            state.graph_xruns.clone(),
            state.mixer.clone(),
            state.ducking_rt.clone(),
            state.levels.clone(),
            state.replay_ring.clone(),
        )
    }

    /// RMS of the pre-graph mic lanes, in dBFS (−100 floor).
    fn mic_level(&self, frames: usize) -> f32 {
        let mut sum = 0.0f64;
        let mut n = 0usize;
        for lane in self.lane_in.iter() {
            for s in lane.iter().take(frames) {
                sum += (*s as f64) * (*s as f64);
                n += 1;
            }
        }
        if n == 0 {
            return -100.0;
        }
        let rms = (sum / n as f64).sqrt() as f32;
        if rms > 1e-9 {
            20.0 * rms.log10()
        } else {
            -100.0
        }
    }

    /// RMS of the final output lanes, in dBFS.
    fn output_level(&self, outputs: &[&mut [f32]], frames: usize) -> f32 {
        let mut sum = 0.0f64;
        let mut n = 0usize;
        for lane in outputs.iter() {
            for s in lane.iter().take(frames) {
                sum += (*s as f64) * (*s as f64);
                n += 1;
            }
        }
        if n == 0 {
            return -100.0;
        }
        let rms = (sum / n as f64).sqrt() as f32;
        if rms > 1e-9 {
            20.0 * rms.log10()
        } else {
            -100.0
        }
    }

    /// Sidechain ducking (F-0.4.C item 14): attenuate the soundboard
    /// buses while the mic sits above the speech threshold. Ramps are in
    /// dB/frame so `attack_ms`/`release_ms` map to the configured times.
    fn apply_ducking(&mut self, mic_db: f32, rate: u32, frames: usize) {
        let rt = &self.ducking;
        let enabled = rt.enabled.load(Ordering::Relaxed);
        let depth = f32::from_bits(rt.depth_db.load(Ordering::Relaxed));
        let thresh = f32::from_bits(rt.threshold_db.load(Ordering::Relaxed));
        let attack = f32::from_bits(rt.attack_ms.load(Ordering::Relaxed)).max(1.0);
        let release = f32::from_bits(rt.release_ms.load(Ordering::Relaxed)).max(1.0);
        self.speech = enabled && mic_db > thresh;
        let target = if self.speech { -depth } else { 0.0 };
        let down = depth / (attack / 1000.0 * rate as f32) * frames as f32;
        let up = depth / (release / 1000.0 * rate as f32) * frames as f32;
        if target < self.duck_db {
            self.duck_db = (self.duck_db - down).max(target);
        } else {
            self.duck_db = (self.duck_db + up).min(target);
        }
        let g = 10f32.powf(self.duck_db / 20.0);
        for s in self.vad_bus.left.iter_mut() {
            *s *= g;
        }
        for s in self.vad_bus.right.iter_mut() {
            *s *= g;
        }
        for s in self.mon_bus.left.iter_mut() {
            *s *= g;
        }
        for s in self.mon_bus.right.iter_mut() {
            *s *= g;
        }
        // Monitor-only bus = monitor − vad: sounds sent to vm-mic already
        // ride the wet path, so they must not be counted twice here.
        self.mon_extra.resize(frames * 2, 0.0);
        for f in 0..frames {
            let l = self.mon_bus.left.get(f).copied().unwrap_or(0.0)
                - self.vad_bus.left.get(f).copied().unwrap_or(0.0);
            let r = self.mon_bus.right.get(f).copied().unwrap_or(0.0)
                - self.vad_bus.right.get(f).copied().unwrap_or(0.0);
            self.mon_extra[f * 2] = l;
            self.mon_extra[f * 2 + 1] = r;
        }
        // Full soundboard bus for the passthrough injection: the monitor
        // bus verbatim (post ducking; no mic voice rides either bus).
        self.inj_extra.resize(frames * 2, 0.0);
        for f in 0..frames {
            self.inj_extra[f * 2] = self.mon_bus.left.get(f).copied().unwrap_or(0.0);
            self.inj_extra[f * 2 + 1] = self.mon_bus.right.get(f).copied().unwrap_or(0.0);
        }
    }
}

impl StreamProcessor for GraphProcessor {
    fn process(&mut self, rate: u32, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let frames = outputs.first().map(|l| l.len()).unwrap_or(0);
        // NOTE: staging allocs only on growth (documented Fase 3b debt:
        // pre-sized arenas land with the RT-hardening work).
        if self.lane_in.len() != inputs.len() {
            self.lane_in = vec![Vec::new(); inputs.len()];
            self.lane_out = vec![Vec::new(); outputs.len()];
        }
        for (lane, src) in self.lane_in.iter_mut().zip(inputs.iter()) {
            if lane.len() < frames {
                lane.resize(frames, 0.0);
            }
            lane[..frames].copy_from_slice(&src[..frames.min(src.len())]);
            // NaN/Inf guard: one poisoned sample latches IIR state (e.g.
            // biquad) into permanent NaN. Sanitize at the DSP boundary —
            // standard practice in every audio engine.
            for s in lane[..frames].iter_mut() {
                if !s.is_finite() {
                    *s = 0.0;
                }
            }
        }
        for lane in self.lane_out.iter_mut() {
            if lane.len() < frames {
                lane.resize(frames, 0.0);
            }
        }
        // Sidechain source: the raw mic, before the voice graph.
        let mic_db = self.mic_level(frames);
        if !self.voice_enabled.load(Ordering::Relaxed) {
            for (dst, src) in self.lane_out.iter_mut().zip(self.lane_in.iter()) {
                dst[..frames].copy_from_slice(&src[..frames]);
            }
        } else if let Ok(mut graph) = self.graph.try_lock() {
            let ctx = ProcessContext { rate, frames };
            graph.process(&ctx, &self.lane_in, &mut self.lane_out);
        } else {
            self.graph_xruns.fetch_add(1, Ordering::Relaxed);
            for (dst, src) in self.lane_out.iter_mut().zip(self.lane_in.iter()) {
                dst[..frames].copy_from_slice(&src[..frames]);
            }
        }
        // Soundboard into the vm-mic (VAD bus) + monitor-only bus, then
        // sidechain ducking. A contended mixer keeps the previous quantum's
        // buses: never block the RT thread for a control-plane action.
        if let Ok(mut mixer) = self.mixer.try_lock() {
            mixer.mix_into(frames, &mut self.vad_bus, &mut self.mon_bus);
        }
        self.apply_ducking(mic_db, rate, frames);
        for (li, lane) in self.lane_out.iter_mut().enumerate() {
            let src = if li == 0 { &self.vad_bus.left } else { &self.vad_bus.right };
            for (slot, v) in lane.iter_mut().zip(src.iter().take(frames)) {
                *slot += *v;
            }
        }
        for (dst, src) in outputs.iter_mut().zip(self.lane_out.iter()) {
            dst.copy_from_slice(&src[..frames.min(dst.len()).min(src.len())]);
        }
        if self.muted.load(Ordering::Relaxed) {
            for dst in outputs.iter_mut() {
                let n = frames.min(dst.len());
                dst[..n].fill(0.0);
            }
        }
        // Instant replay (20) taps the final output — post mute, i.e.
        // exactly what left the vm-mic. `try_lock` keeps a saving thread
        // out of the RT path (a contended lock skips this quantum).
        if outputs.len() >= 2 {
            let n = frames.min(outputs[0].len()).min(outputs[1].len());
            if let Ok(mut ring) = self.replay.try_lock() {
                ring.push_stereo(&outputs[0][..n], &outputs[1][..n]);
            }
        }
        if let Ok(mut lv) = self.levels.try_lock() {
            lv.mic_db = mic_db;
            lv.out_db = self.output_level(outputs, frames);
            lv.speech = self.speech;
        }
    }

    fn monitor_bus(&self) -> &[f32] {
        &self.mon_extra
    }

    fn inject_bus(&self) -> &[f32] {
        &self.inj_extra
    }
}

/// The whole live runtime behind one handle. Every Control API action
/// funnels through here (`ControlHandler`), as do hotkey triggers.
struct DaemonState {
    graph: Arc<Mutex<Graph>>,
    /// Preset node id → graph node (for `setVoiceParameter`).
    node_ids: Mutex<HashMap<String, NodeId>>,
    voice_name: Mutex<String>,
    voice_enabled: Arc<AtomicBool>,
    mixer: Arc<Mutex<vm_soundboard::SoundboardMixer>>,
    /// Sound name → id index (built at boot from the sound dir).
    sound_ids: Mutex<HashMap<String, vm_soundboard::SoundHashId>>,
    audio: Arc<Mutex<Box<dyn AudioBackend>>>,
    preset_dir: PathBuf,
    /// Last successfully opened topology (for runtime input switching).
    topology: Mutex<Option<StreamTopology>>,
    /// Graph-side xrun counter, shared with the RT processor.
    graph_xruns: Arc<AtomicU64>,
    /// Watchdog bookkeeping (restarts, cooldown, last reason).
    watchdog: Mutex<WatchdogState>,
    /// Master mute flag, shared with the RT processor.
    muted: Arc<AtomicBool>,
    /// Live keybind store, shared with the evdev watch threads.
    hotkeys: Arc<Mutex<HotkeyManager>>,
    /// `$VM_CONFIG_DIR/config.json` (bind + device-profile persistence).
    config_path: PathBuf,
    /// Device (uid, uid base, or name) → voice preset. Autoswitches the
    /// voice when the device appears or is selected.
    device_profiles: Mutex<HashMap<String, String>>,
    /// Hear-myself monitor settings (persisted config v3, F-0.4.C 15).
    monitor: Mutex<MonitorConfig>,
    /// Sidechain ducking settings (persisted, F-0.4.C 14).
    ducking: Mutex<DuckingConfig>,
    /// Ducking knobs mirrored to atomics for the RT processor.
    ducking_rt: Arc<DuckingRt>,
    /// obs-websocket settings (persisted, F-0.4.C 35).
    obs: Mutex<ObsConfig>,
    /// obs-websocket bridge runtime status (for `getObsStatus`).
    obs_status: Arc<Mutex<ObsStatus>>,
    /// MIDI learn settings (persisted, F-0.4.C 37).
    midi: Mutex<MidiConfig>,
    /// MIDI input runtime state (learn target + connected flag).
    midi_runtime: Arc<Mutex<MidiRuntime>>,
    /// Instant replay settings (persisted config v4, F-0.4.B 20).
    replay: Mutex<ReplayConfig>,
    /// Instant replay ring shared with the RT processor.
    replay_ring: Arc<Mutex<ReplayRing>>,
    /// Per-application passthrough settings (persisted config v4, 18).
    passthrough: Mutex<PassthroughConfig>,
    /// Live level snapshot (mic/output RMS + speech) for `getLevels`.
    levels: Arc<Mutex<LevelsSnapshot>>,
    /// watch channel notifying the obs-websocket bridge of config changes.
    obs_tx: tokio::sync::watch::Sender<ObsConfig>,
}

/// Strip a PipeWire `#node-id` suffix (`alsa:...#52` → base): node ids
/// churn across restarts, the device part doesn't.
fn strip_node_id(uid: &str) -> &str {
    match uid.rsplit_once('#') {
        Some((base, num)) if !base.is_empty() && num.chars().all(|c| c.is_ascii_digit()) => base,
        _ => uid,
    }
}

/// Does a bound device string match this device? Exact uid, stable base
/// uid, or human device name — in that order of precision.
fn device_matches(bound: &str, uid: &str, name: &str) -> bool {
    bound == uid || bound == strip_node_id(uid) || bound == name
}

/// Watchdog state: restart counter + cooldown bookkeeping. The policy
/// itself is the pure [`decide_watchdog`] below (unit-tested); this
/// struct only carries the inputs it needs across ticks.
#[derive(Debug, Default)]
struct WatchdogState {
    restarts: u64,
    last_reason: String,
    last_restart_unix: u64,
    last_xruns: u64,
    consecutive_failures: u64,
}

/// Minimum seconds between restart attempts (a persistently failing
/// backend must not spin `open()` in a hot loop).
const WATCHDOG_COOLDOWN_SECS: u64 = 10;
/// Backend xruns within one tick (~2 s) that count as a storm.
const WATCHDOG_STORM_XRUNS: u64 = 20;

/// Point-in-time link picture the watchdog policy decides on.
struct LinkSnapshot {
    link: vm_audio::StreamLinkState,
    xruns_total: u64,
    has_topology: bool,
}

/// Pure watchdog policy: `Some(reason)` means "restart the pipeline
/// now". Rules, in order: nothing to restart before first `open`;
/// cooldown wins over everything else; a dead link restarts; an xrun
/// storm restarts. A merely `Down` link with no topology is the
/// pre-streaming state, not a failure.
fn decide_watchdog(
    snap: &LinkSnapshot,
    prev_xruns: u64,
    cooldown_ok: bool,
) -> Option<String> {
    if !snap.has_topology {
        return None;
    }
    if !cooldown_ok {
        return None;
    }
    if snap.link == vm_audio::StreamLinkState::NeedsRestart {
        return Some("link needs restart".to_string());
    }
    if snap.xruns_total.saturating_sub(prev_xruns) >= WATCHDOG_STORM_XRUNS {
        return Some("xrun storm".to_string());
    }
    None
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl DaemonState {
    /// Write an already-built config to `config.json` (no locks taken).
    fn write_config_file(&self, cfg: &vm_control::DaemonConfig) -> Result<(), String> {
        let text = vm_control::save_daemon_config(cfg);
        std::fs::write(&self.config_path, text)
            .map_err(|e| format!("write {}: {e}", self.config_path.display()))
    }

    /// Atomically mutate persisted state then save. Both locks always in
    /// the same order (hotkeys, then profiles — never the reverse, so
    /// concurrent API calls can't deadlock); snapshots both stores and
    /// rolls both back when the disk write fails, so memory and disk
    /// can never disagree (a live-but-unpersisted entry would lie to
    /// list calls).
    fn transact_config<T>(
        &self,
        op: impl FnOnce(&mut HotkeyManager, &mut HashMap<String, String>) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut mgr = self.hotkeys.lock().unwrap();
        let mut profiles = self.device_profiles.lock().unwrap();
        let backup_mgr = mgr.export();
        let backup_prof = profiles.clone();
        let out = op(&mut mgr, &mut profiles);
        match out {
            Ok(v) => {
                let cfg = self.build_config_from(&mgr, &profiles);
                match self.write_config_file(&cfg) {
                    Ok(()) => Ok(v),
                    Err(e) => {
                        mgr.import(backup_mgr);
                        *profiles = backup_prof;
                        Err(format!("applied but not saved, rolled back: {e}"))
                    }
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Persist one F-0.4.C section (monitor/ducking/obs/midi) with the
    /// same snapshot/rollback discipline as [`Self::transact_config`].
    /// The section lock is released before the file snapshot is taken (the
    /// snapshot re-locks every section, so holding one would deadlock).
    fn transact_section<T: Clone>(
        &self,
        store: &Mutex<T>,
        op: impl FnOnce(&mut T) -> Result<(), String>,
    ) -> Result<(), String> {
        let backup = store.lock().unwrap().clone();
        {
            let mut sec = store.lock().unwrap();
            op(&mut sec)?;
        }
        let mgr = self.hotkeys.lock().unwrap();
        let profiles = self.device_profiles.lock().unwrap();
        let cfg = self.build_config_from(&mgr, &profiles);
        drop(mgr);
        drop(profiles);
        match self.write_config_file(&cfg) {
            Ok(()) => Ok(()),
            Err(e) => {
                *store.lock().unwrap() = backup;
                Err(format!("applied but not saved, rolled back: {e}"))
            }
        }
    }

    /// Assemble the persisted config from the live stores. `hotkeys` and
    /// `device_profiles` are passed in because the caller may already hold
    /// their locks; the F-0.4.C sections are snapshotted here.
    fn build_config_from(
        &self,
        mgr: &HotkeyManager,
        profiles: &HashMap<String, String>,
    ) -> vm_control::DaemonConfig {
        vm_control::DaemonConfig {
            hotkeys: mgr.export(),
            device_profiles: profiles.clone(),
            monitor: self.monitor.lock().unwrap().clone(),
            ducking: self.ducking.lock().unwrap().clone(),
            obs: self.obs.lock().unwrap().clone(),
            midi: self.midi.lock().unwrap().clone(),
            replay: self.replay.lock().unwrap().clone(),
            passthrough: self.passthrough.lock().unwrap().clone(),
        }
    }

    /// Mirror the persisted ducking config into the RT atomics (called on
    /// boot and on every `setDucking`).
    fn push_ducking_rt(&self, cfg: &DuckingConfig) {
        let rt = &self.ducking_rt;
        rt.enabled.store(cfg.enabled, Ordering::Relaxed);
        rt.depth_db.store(cfg.depth_db.to_bits(), Ordering::Relaxed);
        rt.threshold_db.store(cfg.threshold_db.to_bits(), Ordering::Relaxed);
        rt.attack_ms.store(cfg.attack_ms.to_bits(), Ordering::Relaxed);
        rt.release_ms.store(cfg.release_ms.to_bits(), Ordering::Relaxed);
    }

    /// Apply monitor settings to the backend (validates the device uid)
    /// and return the broadcast event for the new state.
    fn apply_monitor(&self, cfg: &MonitorConfig) -> Result<ControlEvent, String> {
        let settings = MonitorSettings {
            enabled: cfg.enabled,
            device: cfg.device.clone(),
            mix: cfg.mix,
            volume_db: cfg.volume_db,
            muted: cfg.muted,
        };
        let mut audio = self.audio.lock().unwrap();
        audio.set_monitor(&settings).map_err(|e| e.to_string())?;
        Ok(ControlEvent::MonitorChanged {
            enabled: cfg.enabled,
            device: cfg.device.clone(),
            mix: cfg.mix,
            volume_db: cfg.volume_db,
            muted: cfg.muted,
        })
    }

    /// Mirror replay settings into the shared ring (boot + every set).
    fn apply_replay(&self, cfg: &ReplayConfig) {
        let mut ring = self.replay_ring.lock().unwrap();
        ring.configure(cfg.seconds, SAMPLE_RATE);
        ring.set_enabled(cfg.enabled);
    }

    /// Apply passthrough settings to the backend (moves apps, loads the
    /// loopback) and return the broadcast event for the new state.
    fn apply_passthrough(&self, cfg: &PassthroughConfig) -> Result<ControlEvent, String> {
        let settings = PassthroughSettings {
            enabled: cfg.enabled,
            device: cfg.device.clone(),
            app: cfg.app.clone(),
        };
        let mut audio = self.audio.lock().unwrap();
        audio.set_passthrough(&settings).map_err(|e| e.to_string())?;
        Ok(ControlEvent::PassthroughChanged {
            enabled: cfg.enabled,
            device: cfg.device.clone(),
            app: cfg.app.clone(),
        })
    }

    /// Snapshot the replay ring and write `<config>/replays/replay-<unix>.wav`.
    /// The copy happens under the ring lock; the file write outside it.
    fn save_replay(&self) -> Result<(String, ControlEvent), String> {
        let cfg = self.replay.lock().unwrap().clone();
        if !cfg.enabled {
            return Err("instant replay is disabled".into());
        }
        let snap = self.replay_ring.lock().unwrap().snapshot();
        if snap.is_empty() {
            return Err("replay buffer is empty (stream not running?)".into());
        }
        let config_dir = self
            .config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let path = replay_path(&config_dir, unix_now());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        write_wav_pcm16(&path, &snap, SAMPLE_RATE, replay::WAV_CHANNELS)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        let shown = path.display().to_string();
        Ok((shown.clone(), ControlEvent::ReplaySaved { path: shown }))
    }

    fn obs_status_json(&self) -> serde_json::Value {
        let st = self.obs_status.lock().unwrap();
        // Before the bridge connects, report the configured endpoint so
        // UIs can show where it *would* connect.
        let url = if st.url.is_empty() {
            self.obs.lock().unwrap().url.clone()
        } else {
            st.url.clone()
        };
        serde_json::json!({
            "connected": st.connected,
            "scene": st.scene,
            "url": url,
            "enabled": self.obs.lock().unwrap().enabled,
        })
    }

    fn obs_status_event(&self) -> ControlEvent {
        let st = self.obs_status.lock().unwrap();
        ControlEvent::ObsStatusChanged {
            connected: st.connected,
            scene: st.scene.clone(),
            url: st.url.clone(),
        }
    }

    #[cfg_attr(not(feature = "midi"), allow(dead_code))]
    fn set_midi_connected(&self, connected: bool) {
        self.midi_runtime.lock().unwrap().connected = connected;
    }

    fn set_obs_status(&self, connected: bool, scene: String, url: String) {
        let mut st = self.obs_status.lock().unwrap();
        st.connected = connected;
        st.scene = scene;
        st.url = url;
    }

    /// Dispatch one incoming MIDI event (F-0.4.C 37): while learning, the
    /// event becomes a new binding; otherwise it fires the mapped target.
    #[cfg_attr(not(feature = "midi"), allow(dead_code))]
    fn dispatch_midi(
        &self,
        ev: &MidiEvent,
        publisher: &tokio::sync::broadcast::Sender<String>,
    ) {
        let learn_target = self.midi_runtime.lock().unwrap().learn.take();
        if let Some(target) = learn_target {
            let binding = MidiBinding {
                id: 0,
                channel: Some(ev.channel),
                kind: ev.kind,
                number: ev.number,
                target: target.clone(),
                enabled: true,
            };
            let res = self.transact_section(&self.midi, |c| {
                let id = c.next_binding_id();
                let mut b = binding.clone();
                b.id = id;
                c.bindings.push(b);
                Ok(())
            });
            if let Err(e) = res {
                eprintln!("midi learn: {e}");
                return;
            }
            let id = self.midi.lock().unwrap().bindings.last().map(|b| b.id).unwrap_or(0);
            publish(publisher, &ControlEvent::MidiLearned {
                midi_id: id,
                kind: ev.kind,
                number: ev.number,
                channel: Some(ev.channel),
                target,
            });
            publish(publisher, &ControlEvent::MidiBindingsChanged);
            return;
        }
        let binding = {
            let cfg = self.midi.lock().unwrap();
            cfg.find_binding(ev.kind, ev.number, ev.channel).cloned()
        };
        let Some(b) = binding else { return };
        let action = match &b.target {
            MidiTarget::Trigger(t) => hotkey_action(t, ev.value > 0, self),
            MidiTarget::SetVoiceParameter { param, min, max } => {
                let t = (ev.value as f32 / 127.0).clamp(0.0, 1.0);
                Some(Action::SetVoiceParameter {
                    param: param.clone(),
                    value: serde_json::json!(min + t * (max - min)),
                })
            }
        };
        if let Some(a) = action {
            let (_reply, evts) = self.handle(&a);
            for e in &evts {
                publish(publisher, e);
            }
        }
    }

    /// Load a voice preset file by name and apply it. Shared by the
    /// `SelectVoice` action, the boot default, and device autoswitch.
    fn select_voice(&self, voice: &str) -> Result<Vec<ControlEvent>, String> {
        let path = self.preset_dir.join(format!("{voice}.json"));
        let text =
            std::fs::read_to_string(&path).map_err(|_| format!("unknown voice '{voice}'"))?;
        let preset = vm_preset::load(&text).map_err(|e| format!("bad preset: {e}"))?;
        self.apply_preset(&preset)?;
        Ok(vec![ControlEvent::VoiceChanged { voice: voice.to_string() }])
    }

    /// Device-profile autoswitch: if `uid`/`name` has a bound voice that
    /// isn't already active, select it. Returns the events when a switch
    /// happened. Never fights the user (same voice → no-op, so repeated
    /// hotplug polls can't loop).
    fn autoswitch_for_device(&self, uid: &str, name: &str) -> Option<Vec<ControlEvent>> {
        let target = {
            let profiles = self.device_profiles.lock().unwrap();
            profiles
                .iter()
                .find_map(|(bound, voice)| device_matches(bound, uid, name).then(|| voice.clone()))
        }?;
        if *self.voice_name.lock().unwrap() == target {
            return None;
        }
        match self.select_voice(&target) {
            Ok(evts) => {
                println!("device profile: '{uid}' ({name}) → voice '{target}'");
                Some(evts)
            }
            Err(e) => {
                eprintln!("device profile: cannot select '{target}' for '{uid}': {e}");
                None
            }
        }
    }

    /// Write a captured noise floor into the current voice's preset
    /// file as the node's non-public `profile` param. The updated file
    /// is re-validated before it reaches disk; user edits to every
    /// other param survive untouched (only `profile` is inserted).
    fn store_noise_profile(&self, node: &str, profile: &[f64]) -> Result<usize, String> {
        let voice = self.voice_name.lock().unwrap().clone();
        if voice.is_empty() {
            return Err("no voice loaded".to_string());
        }
        let path = self.preset_dir.join(format!("{voice}.json"));
        let text = std::fs::read_to_string(&path)
            .map_err(|_| format!("preset file missing for '{voice}'"))?;
        let mut preset = vm_preset::load(&text).map_err(|e| format!("bad preset: {e}"))?;
        let spec = preset
            .nodes
            .iter_mut()
            .find(|n| n.id == node)
            .ok_or_else(|| format!("node '{node}' not in preset '{voice}'"))?;
        spec.params.insert(
            "profile".to_string(),
            vm_preset::ParamSpec {
                value: vm_preset::JsonParamValue::FloatArray(profile.to_vec()),
                range: None,
                public: false,
                modulation: None,
            },
        );
        let out =
            serde_json::to_string_pretty(&preset).map_err(|e| format!("encode: {e}"))?;
        vm_preset::load(&out).map_err(|e| format!("wrote invalid preset: {e}"))?;
        std::fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(profile.len())
    }

    /// Runtime input switch: stop → reopen on the new input (same virtual
    /// output) → start. Drops in-flight audio; the graph keeps running.
    /// A bound device autoswitches its voice (events bubble to the
    /// caller); anything else keeps the current voice.
    fn switch_input(&self, uid: &str) -> Result<Vec<ControlEvent>, String> {
        let topo = self.topology.lock().map_err(|_| "topology lock poisoned".to_string())?;
        let base = topo.clone().ok_or_else(|| "pipeline never opened".to_string())?;
        drop(topo);
        let mut audio = self.audio.lock().map_err(|_| "audio lock poisoned".to_string())?;
        let devs = audio.enumerate().map_err(|e| format!("enumerate: {e}"))?;
        let dev = devs.iter().find(|d| d.uid.0 == uid).ok_or_else(|| format!("unknown input '{uid}'"))?;
        let name = dev.name.clone();
        let next = StreamTopology {
            input: vm_audio::DeviceUid::new(uid),
            virtual_out: base.virtual_out.clone(),
            monitor: base.monitor.clone(),
        };
        audio.stop(StreamStopReason::Requested).map_err(|e| format!("stop: {e}"))?;
        audio.open(&next).map_err(|e| format!("open: {e}"))?;
        audio.start().map_err(|e| format!("start: {e}"))?;
        drop(audio);
        *self.topology.lock().map_err(|_| "topology lock poisoned".to_string())? = Some(next);
        Ok(self.autoswitch_for_device(uid, &name).unwrap_or_default())
    }

    /// Watchdog restart: stop → reopen the stored topology → start.
    /// Mirrors the reference `TryRestartOnInternalError`: same pipeline,
    /// no state reset (the graph keeps running; only the backend
    /// reopens). Records success/failure for the health readout.
    fn restart_pipeline(&self) -> Result<(), String> {
        let topo = self
            .topology
            .lock()
            .map_err(|_| "topology lock poisoned".to_string())?
            .clone()
            .ok_or_else(|| "pipeline never opened".to_string())?;
        {
            let mut audio = self.audio.lock().map_err(|_| "audio lock poisoned".to_string())?;
            audio.stop(StreamStopReason::InternalError).map_err(|e| format!("stop: {e}"))?;
            audio.open(&topo).map_err(|e| format!("open: {e}"))?;
            audio.start().map_err(|e| format!("start: {e}"))?;
        }
        *self.topology.lock().map_err(|_| "topology lock poisoned".to_string())? = Some(topo);
        Ok(())
    }

    /// One watchdog tick: snapshot the link, run the pure policy, and
    /// restart on decision. Runs on the 2 s housekeeping tick (never on
    /// the RT path). Attempt timestamps feed the cooldown even on
    /// failure, so a dead backend costs one `open()` per cooldown, not
    /// one per tick.
    fn watchdog_tick(&self) {
        use vm_audio::StreamKind as K;
        let (link, xruns) = {
            let audio = self.audio.lock().unwrap();
            let xruns = [K::FullDuplexCapture, K::HalfDuplexPlayback, K::Monitor]
                .iter()
                .map(|k| audio.stats(*k).xruns)
                .sum();
            (audio.link_state(), xruns)
        };
        let has_topology = self.topology.lock().unwrap().is_some();
        let now = unix_now();
        let decision = {
            let wd = self.watchdog.lock().unwrap();
            let cooldown_ok = now.saturating_sub(wd.last_restart_unix) >= WATCHDOG_COOLDOWN_SECS;
            let snap = LinkSnapshot { link, xruns_total: xruns, has_topology };
            decide_watchdog(&snap, wd.last_xruns, cooldown_ok)
        };
        let Some(reason) = decision else {
            self.watchdog.lock().unwrap().last_xruns = xruns;
            return;
        };
        match self.restart_pipeline() {
            Ok(()) => {
                println!("watchdog: restarted pipeline ({reason})");
                let mut wd = self.watchdog.lock().unwrap();
                wd.restarts += 1;
                wd.last_reason = reason;
                wd.last_restart_unix = now;
                wd.last_xruns = xruns;
                wd.consecutive_failures = 0;
            }
            Err(e) => {
                eprintln!("watchdog: restart failed ({reason}): {e}");
                let mut wd = self.watchdog.lock().unwrap();
                wd.last_restart_unix = now;
                wd.last_xruns = xruns;
                wd.consecutive_failures += 1;
            }
        }
    }

    fn json_param(value: &serde_json::Value) -> Result<vm_dsp::ParamValue, String> {        match value {
            serde_json::Value::Number(n) => n
                .as_f64()
                .map(|v| vm_dsp::ParamValue::Float(v as f32))
                .ok_or_else(|| "non-numeric param".to_string()),
            serde_json::Value::Bool(b) => Ok(vm_dsp::ParamValue::Bool(*b)),
            serde_json::Value::String(s) => Ok(vm_dsp::ParamValue::Text(s.clone())),
            _ => Err("unsupported param json type (want number/bool/string)".to_string()),
        }
    }

    /// (Re)build the graph from a preset, swapping it under lock.
    /// The RT side uses `try_lock`: contended quanta pass through and
    /// count as xruns instead of blocking.
    fn apply_preset(&self, preset: &vm_preset::Preset) -> Result<(), String> {
        let nctx = NodeConstructorContext { rate: SAMPLE_RATE, max_channels: 2 };
        let factory: NodeFactory = vm_dsp::factory::mvp_factory();
        let mut graph = Graph::new();
        let mut ids = HashMap::new();
        for spec in &preset.nodes {
            let mut node =
                factory.create(&spec.r#type, &nctx).map_err(|e| format!("factory: {e}"))?;
            for (pname, pspec) in &spec.params {
                let v = match &pspec.value {
                    vm_preset::JsonParamValue::Float(v) => vm_dsp::ParamValue::Float(*v as f32),
                    vm_preset::JsonParamValue::Int(v) => vm_dsp::ParamValue::Int(*v as i32),
                    vm_preset::JsonParamValue::Bool(v) => vm_dsp::ParamValue::Bool(*v),
                    vm_preset::JsonParamValue::Text(v) => vm_dsp::ParamValue::Text(v.clone()),
                    vm_preset::JsonParamValue::FloatArray(v) => {
                        vm_dsp::ParamValue::FloatArray(v.iter().map(|x| *x as f32).collect())
                    }
                };
                node.set_param(pname, v).map_err(|e| format!("param {pname}: {e}"))?;
            }
            node.prepare(nctx.max_channels).map_err(|e| format!("prepare: {e}"))?;
            ids.insert(spec.id.clone(), graph.add(node));
        }
        for edge in &preset.edges {
            let from = ids.get(&edge.from).ok_or_else(|| format!("bad edge from {}", edge.from))?;
            let to = ids.get(&edge.to).ok_or_else(|| format!("bad edge to {}", edge.to))?;
            graph
                .connect(vm_dsp::Edge {
                    from: *from,
                    from_ch: edge.from_ch,
                    to: *to,
                    to_ch: edge.to_ch,
                })
                .map_err(|e| format!("graph: {e}"))?;
        }
        *self.graph.lock().map_err(|_| "graph lock poisoned".to_string())? = graph;
        *self.node_ids.lock().map_err(|_| "node map poisoned".to_string())? = ids;
        *self.voice_name.lock().map_err(|_| "voice lock poisoned".to_string())? =
            preset.name.clone();
        Ok(())
    }
}

impl ControlHandler for DaemonState {
    fn handle(&self, action: &Action) -> (ControlReply, Vec<ControlEvent>) {
        match action {
            Action::SelectVoice { voice } => match self.select_voice(voice) {
                Ok(evts) => (ControlReply::ok(), evts),
                Err(e) => (ControlReply::err(e), Vec::new()),
            },
            Action::SetVoiceParameter { param, value } => {
                let (node, name) = match param.split_once('.') {
                    Some((n, p)) => (n, p),
                    None => {
                        return (
                            ControlReply::err("param must be 'node.param'".to_string()),
                            Vec::new(),
                        )
                    }
                };
                let value = match Self::json_param(value) {
                    Ok(v) => v,
                    Err(e) => return (ControlReply::err(e), Vec::new()),
                };
                // Both are uncontended control-plane locks (never held on RT).
                let ids = self.node_ids.lock().unwrap();
                let Some(id) = ids.get(node).copied() else {
                    return (ControlReply::err(format!("unable to find node '{node}'")), Vec::new());
                };
                drop(ids);
                let mut graph = self.graph.lock().unwrap();
                let Some(n) = graph.node_mut(id) else {
                    return (ControlReply::err(format!("node '{node}' retired")), Vec::new());
                };
                match n.set_param(name, value) {
                    Ok(()) => (
                        ControlReply::ok(),
                        vec![ControlEvent::VoiceParameterChanged {
                            param: param.clone(),
                            value: action_value(action),
                        }],
                    ),
                    Err(e) => (ControlReply::err(format!("set param: {e}")), Vec::new()),
                }
            }
            Action::PlaySound { sound } => {
                let id = {
                    let map = self.sound_ids.lock().unwrap();
                    match map.get(sound).copied() {
                        Some(id) => id,
                        None => {
                            return (
                                ControlReply::err(format!("unknown sound '{sound}'")),
                                Vec::new(),
                            )
                        }
                    }
                };
                let mut mixer = self.mixer.lock().unwrap();
                match mixer.play(id, vm_soundboard::SoundOptions::default()) {
                    Ok(_) => (
                        ControlReply::ok(),
                        vec![ControlEvent::SoundStarted { sound: sound.clone() }],
                    ),
                    Err(_) => (ControlReply::err(format!("cannot play '{sound}'")), Vec::new()),
                }
            }
            Action::StopAllSounds => {
                // Snapshot the playing ids first: SoundStopped per sound
                // keeps physical buttons in sync (the enum existed since
                // Fase 3b but nothing ever emitted it).
                let playing = self.mixer.lock().unwrap().playing();
                self.mixer.lock().unwrap().stop_all();
                let map = self.sound_ids.lock().unwrap();
                let evts = playing
                    .into_iter()
                    .filter_map(|id| map.iter().find_map(|(n, i)| (*i == id).then(|| n.clone())))
                    .map(|sound| ControlEvent::SoundStopped { sound })
                    .collect();
                (ControlReply::ok(), evts)
            }
            Action::FadeSound { sound, fade_out_secs } => {
                let id = {
                    let map = self.sound_ids.lock().unwrap();
                    match map.get(sound).copied() {
                        Some(id) => id,
                        None => {
                            return (
                                ControlReply::err(format!("unknown sound '{sound}'")),
                                Vec::new(),
                            )
                        }
                    }
                };
                let ok = self.mixer.lock().unwrap().fade_out(id, *fade_out_secs);
                if ok {
                    (ControlReply::ok(), Vec::new())
                } else {
                    (ControlReply::err(format!("'{sound}' is not playing")), Vec::new())
                }
            }
            Action::ToggleVoiceChanger { enabled } => {
                self.voice_enabled.store(*enabled, Ordering::Relaxed);
                (
                    ControlReply::ok(),
                    vec![ControlEvent::VoiceChangerToggled { enabled: *enabled }],
                )
            }
            Action::SetMuted { muted } => {
                self.muted.store(*muted, Ordering::Relaxed);
                (
                    ControlReply::ok(),
                    vec![ControlEvent::MutedChanged { muted: *muted }],
                )
            }
            Action::AddHotkey { codes, target, enabled } => {
                let bind =
                    Keybind { codes: codes.clone(), target: target.clone(), enabled: *enabled };
                if let Err(e) = vm_control::validate_bind(&bind) {
                    return (ControlReply::err(e), Vec::new());
                }
                match self.transact_config(|mgr, _| Ok(mgr.add(bind))) {
                    Ok(id) => {
                        (ControlReply::ok_data(serde_json::json!({ "id": id })), Vec::new())
                    }
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::RemoveHotkey { hotkey_id } => {
                match self.transact_config(|mgr, _| {
                    if mgr.remove(*hotkey_id) {
                        Ok(())
                    } else {
                        Err(format!("unknown hotkey id {hotkey_id}"))
                    }
                }) {
                    Ok(()) => (ControlReply::ok(), Vec::new()),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::ListHotkeys => {
                let mgr = self.hotkeys.lock().unwrap();
                let binds: Vec<serde_json::Value> = mgr
                    .list()
                    .into_iter()
                    .map(|(id, b)| {
                        serde_json::json!({
                            "id": id,
                            "codes": b.codes,
                            "target": b.target,
                            "enabled": b.enabled,
                        })
                    })
                    .collect();
                (ControlReply::ok_data(serde_json::json!({ "hotkeys": binds })), Vec::new())
            }
            Action::CaptureNoiseProfile { node } => {
                // Read the live floor off the running node, then store it
                // into the current voice's preset file (round-trips
                // through validation before touching disk).
                let profile: Vec<f64> = {
                    let ids = self.node_ids.lock().unwrap();
                    let Some(id) = ids.get(node).copied() else {
                        return (
                            ControlReply::err(format!("unable to find node '{node}'")),
                            Vec::new(),
                        );
                    };
                    drop(ids);
                    let mut graph = self.graph.lock().unwrap();
                    let Some(n) = graph.node_mut(id) else {
                        return (
                            ControlReply::err(format!("node '{node}' retired")),
                            Vec::new(),
                        );
                    };
                    match n.get_param("profile") {
                        Some(vm_dsp::ParamValue::FloatArray(p)) => {
                            p.iter().map(|x| *x as f64).collect()
                        }
                        _ => {
                            return (
                                ControlReply::err(format!(
                                    "node '{node}' exposes no noise profile (want a denoise node)"
                                )),
                                Vec::new(),
                            )
                        }
                    }
                };
                match self.store_noise_profile(node, &profile) {
                    Ok(bins) => (
                        ControlReply::ok_data(
                            serde_json::json!({ "node": node, "bins": bins }),
                        ),
                        Vec::new(),
                    ),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::SetDeviceProfile { device, voice } => {
                if device.is_empty() || voice.is_empty() {
                    return (
                        ControlReply::err("device and voice must both be non-empty".to_string()),
                        Vec::new(),
                    );
                }
                match self.transact_config(|_, profiles| {
                    profiles.insert(device.clone(), voice.clone());
                    Ok(())
                }) {
                    Ok(()) => (ControlReply::ok(), Vec::new()),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::RemoveDeviceProfile { device } => {
                match self.transact_config(|_, profiles| {
                    profiles.remove(device.as_str()).map(|_| ()).ok_or_else(|| {
                        format!("no profile bound to '{device}'")
                    })
                }) {
                    Ok(()) => (ControlReply::ok(), Vec::new()),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::ListDeviceProfiles => {
                let profiles = self.device_profiles.lock().unwrap();
                let mut entries: Vec<serde_json::Value> = profiles
                    .iter()
                    .map(|(d, v)| serde_json::json!({ "device": d, "voice": v }))
                    .collect();
                entries.sort_by(|a, b| a["device"].as_str().cmp(&b["device"].as_str()));
                (
                    ControlReply::ok_data(serde_json::json!({ "profiles": entries })),
                    Vec::new(),
                )
            }
            Action::StartAudioStreaming => {
                let mut audio = self.audio.lock().unwrap();
                match audio.start() {
                    Ok(()) => (
                        ControlReply::ok(),
                        vec![ControlEvent::StreamStatus { active: true }],
                    ),
                    Err(e) => (ControlReply::err(format!("start: {e}")), Vec::new()),
                }
            }
            Action::GetAudioStreamStatus => {
                let audio = self.audio.lock().unwrap();
                let active = !matches!(
                    audio.link_state(),
                    vm_audio::StreamLinkState::Down
                );
                let cap = audio.stats(vm_audio::StreamKind::FullDuplexCapture);
                let voice = self.voice_name.lock().unwrap().clone();
                let nodes = self.graph.lock().map(|g| g.len()).unwrap_or(usize::MAX);
                (
                    ControlReply::ok_data(serde_json::json!({
                        "active": active,
                        "voice": voice,
                        "nodes": nodes,
                        "link": format!("{:?}", audio.link_state()),
                        "xruns": cap.xruns + audio.stats(vm_audio::StreamKind::HalfDuplexPlayback).xruns,
                        "avgCallbackUs": cap.avg_duration_us,
                    })),
                    Vec::new(),
                )
            }
            Action::GetHealth => {
                let (link, cap, play, mon) = {
                    let audio = self.audio.lock().unwrap();
                    use vm_audio::StreamKind as K;
                    let stat = |k: K| {
                        let s = audio.stats(k);
                        serde_json::json!({
                            "invocations": s.invocations,
                            "avgUs": s.avg_duration_us,
                            "maxUs": s.max_duration_us,
                            "xruns": s.xruns,
                            "watchdogTrips": s.watchdog_trips,
                            "lastFrames": s.last_frames,
                        })
                    };
                    let link = audio.link_state();
                    (link, stat(K::FullDuplexCapture), stat(K::HalfDuplexPlayback), stat(K::Monitor))
                };
                // Join graph telemetry (keyed by NodeId) with preset node
                // ids; unnamed nodes fall back to `node<N>`. Sorted for a
                // deterministic wire order. Each row also carries the
                // node's algorithmic latency in ms (group delay, not CPU
                // cost) — the chain budget the low-latency mode optimises.
                let ids = self.node_ids.lock().unwrap();
                let graph = self.graph.lock().unwrap();
                let tele = graph.telemetry();
                let mut nodes: Vec<serde_json::Value> = tele
                    .into_iter()
                    .map(|(id, t)| {
                        let name = ids
                            .iter()
                            .find_map(|(k, v)| (*v == id).then(|| k.clone()))
                            .unwrap_or_else(|| format!("node{}", id.0));
                        let lat_ms = graph
                            .node(id)
                            .map(|n| n.latency_samples(SAMPLE_RATE) as f64 * 1000.0 / SAMPLE_RATE as f64)
                            .unwrap_or(0.0);
                        serde_json::json!({
                            "node": name,
                            "avgUs": t.avg_us,
                            "maxUs": t.max_us,
                            "calls": t.calls,
                            "latMs": lat_ms,
                        })
                    })
                    .collect();
                nodes.sort_by(|a, b| a["node"].as_str().cmp(&b["node"].as_str()));
                let latency_ms: f64 =
                    nodes.iter().map(|n| n["latMs"].as_f64().unwrap_or(0.0)).sum();
                drop(ids);
                drop(graph);
                let wd = self.watchdog.lock().unwrap();
                (
                    ControlReply::ok_data(serde_json::json!({
                        "active": !matches!(link, vm_audio::StreamLinkState::Down),
                        "voice": self.voice_name.lock().unwrap().clone(),
                        "link": format!("{link:?}"),
                        "graphXruns": self.graph_xruns.load(Ordering::Relaxed),
                        "nodes": nodes,
                        "capture": cap,
                        "playback": play,
                        "monitor": mon,
                        "watchdog": {
                            "restarts": wd.restarts,
                            "lastReason": wd.last_reason,
                            "lastRestartUnix": wd.last_restart_unix,
                            "consecutiveFailures": wd.consecutive_failures,
                        },
                        // Button-init snapshot: a freshly connected client
                        // learns changer + mute state without toggling.
                        "enabled": self.voice_enabled.load(Ordering::Relaxed),
                        "muted": self.muted.load(Ordering::Relaxed),
                        // Chain algorithmic latency budget in ms.
                        "latencyMs": latency_ms,
                    })),
                    Vec::new(),
                )
            }
            Action::GetVoices => {
                let mut voices: Vec<String> = std::fs::read_dir(&self.preset_dir)
                    .map(|entries| {
                        entries
                            .filter_map(|e| e.ok())
                            .map(|e| e.path())
                            .filter(|p| {
                                p.extension().and_then(|x| x.to_str()) == Some("json")
                            })
                            .filter_map(|p| {
                                p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                voices.sort();
                (ControlReply::ok_data(serde_json::json!({ "voices": voices })), Vec::new())
            }
            Action::GetAudioDevices => {
                let mut audio = self.audio.lock().unwrap();
                match audio.enumerate() {
                    Ok(devs) => (
                        ControlReply::ok_data(serde_json::json!({
                            "devices": devs.iter().map(|d| serde_json::json!({
                                "uid": d.uid.0, "name": d.name,
                                "role": format!("{:?}", d.role),
                                "rate": d.sample_rate, "channels": d.channels,
                            })).collect::<Vec<_>>(),
                        })),
                        Vec::new(),
                    ),
                    Err(e) => (ControlReply::err(format!("enumerate: {e}")), Vec::new()),
                }
            }
            Action::ResetVoiceParameters => {
                let voice = self.voice_name.lock().unwrap().clone();
                if voice.is_empty() {
                    return (ControlReply::err("no voice loaded".to_string()), Vec::new());
                }
                let path = self.preset_dir.join(format!("{voice}.json"));
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(_) => {
                        return (
                            ControlReply::err(format!("preset file missing for '{voice}'")),
                            Vec::new(),
                        )
                    }
                };
                match vm_preset::load(&text).map_err(|e| e.to_string()).and_then(|p| {
                    self.apply_preset(&p)
                }) {
                    Ok(()) => (
                        ControlReply::ok(),
                        vec![ControlEvent::VoiceChanged { voice }],
                    ),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::SetDefaultMicrophone { uid } => (
                ControlReply::err(format!(
                    "OS default mic is managed by the desktop, not the daemon ({uid}); \
                     select the daemon input via VM_INPUT_UID or the UI"
                )),
                Vec::new(),
            ),
            Action::GetUserLicense => (
                ControlReply::ok_data(serde_json::json!({
                    "tier": "local", "pro": false, "source": "clean-room",
                })),
                Vec::new(),
            ),
            Action::GetAllSoundboard => {
                let map = self.sound_ids.lock().unwrap();
                let mut names: Vec<&String> = map.keys().collect();
                names.sort();
                (ControlReply::ok_data(serde_json::json!({ "sounds": names })), Vec::new())
            }
            Action::GetBitmap { resource } => (
                ControlReply::err(format!("no icon assets in clean-room build ({resource})")),
                Vec::new(),
            ),
            Action::SetSoundboardMasterVolume { per_mille } => {
                self.mixer.lock().unwrap().set_master_volume(*per_mille);
                (
                    ControlReply::ok(),
                    Vec::new(),
                )
            }
            Action::SelectInput { uid } => match self.switch_input(uid) {
                Ok(evts) => {
                    let mut all = vec![ControlEvent::DevicesChanged];
                    all.extend(evts);
                    (ControlReply::ok(), all)
                }
                Err(e) => (ControlReply::err(e), Vec::new()),
            },
            Action::SetMonitor { enabled, device, mix, volume_db, muted } => {
                let next = {
                    let cfg = self.monitor.lock().unwrap();
                    let mut n = cfg.clone();
                    if let Some(v) = enabled {
                        n.enabled = *v;
                    }
                    if let Some(v) = device {
                        n.device = v.clone();
                    }
                    if let Some(v) = mix {
                        n.mix = *v;
                    }
                    if let Some(v) = volume_db {
                        n.volume_db = *v;
                    }
                    if let Some(v) = muted {
                        n.muted = *v;
                    }
                    n.clamp();
                    n
                };
                // Persist first (rollback-safe), then push to the backend;
                // a bad device rolls the section back so disk and audio
                // can never disagree.
                let prev = self.monitor.lock().unwrap().clone();
                match self.transact_section(&self.monitor, |c| {
                    *c = next.clone();
                    Ok(())
                }) {
                    Ok(()) => match self.apply_monitor(&next) {
                        Ok(evt) => (ControlReply::ok(), vec![evt]),
                        Err(e) => {
                            let _ = self.transact_section(&self.monitor, |c| {
                                *c = prev;
                                Ok(())
                            });
                            (ControlReply::err(format!("monitor: {e}")), Vec::new())
                        }
                    },
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::GetMonitor => {
                let cfg = self.monitor.lock().unwrap();
                let v = serde_json::to_value(&*cfg).unwrap_or(serde_json::Value::Null);
                (ControlReply::ok_data(v), Vec::new())
            }
            Action::SetInstantReplay { enabled, seconds } => {
                let next = {
                    let cfg = self.replay.lock().unwrap();
                    let mut n = cfg.clone();
                    if let Some(v) = enabled {
                        n.enabled = *v;
                    }
                    if let Some(v) = seconds {
                        n.seconds = *v;
                    }
                    n.clamp();
                    n
                };
                match self.transact_section(&self.replay, |c| {
                    *c = next.clone();
                    Ok(())
                }) {
                    Ok(()) => {
                        self.apply_replay(&next);
                        let v = serde_json::json!({
                            "enabled": next.enabled,
                            "seconds": next.seconds,
                        });
                        (ControlReply::ok_data(v), Vec::new())
                    }
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::GetInstantReplay => {
                let cfg = self.replay.lock().unwrap().clone();
                let buffered = self.replay_ring.lock().unwrap().filled_frames() as f64
                    / SAMPLE_RATE as f64;
                let mut v = serde_json::to_value(&cfg).unwrap_or(serde_json::Value::Null);
                if let Some(o) = v.as_object_mut() {
                    o.insert(
                        "buffered_seconds".into(),
                        serde_json::json!((buffered * 100.0).round() / 100.0),
                    );
                }
                (ControlReply::ok_data(v), Vec::new())
            }
            Action::SaveInstantReplay => match self.save_replay() {
                Ok((path, evt)) => (
                    ControlReply::ok_data(serde_json::json!({ "path": path })),
                    vec![evt],
                ),
                Err(e) => (ControlReply::err(e), Vec::new()),
            },
            Action::ListPassthroughApps => {
                // Lock released before serializing: the backend call may
                // block on pactl, the reply building never holds it.
                let res = self.audio.lock().unwrap().output_streams();
                match res {
                    Ok(apps) => (
                        ControlReply::ok_data(serde_json::json!({ "apps": apps })),
                        Vec::new(),
                    ),
                    Err(e) => (ControlReply::err(e.to_string()), Vec::new()),
                }
            }
            Action::GetPassthrough => {
                let cfg = self.passthrough.lock().unwrap();
                let v = serde_json::to_value(&*cfg).unwrap_or(serde_json::Value::Null);
                (ControlReply::ok_data(v), Vec::new())
            }
            Action::SetPassthrough { enabled, device, app } => {
                let next = {
                    let cfg = self.passthrough.lock().unwrap();
                    let mut n = cfg.clone();
                    if let Some(v) = enabled {
                        n.enabled = *v;
                    }
                    if let Some(v) = device {
                        n.device = v.clone();
                    }
                    if let Some(v) = app {
                        n.app = v.clone();
                    }
                    n.clamp();
                    n
                };
                let prev = self.passthrough.lock().unwrap().clone();
                // Same discipline as `setMonitor`: persist, then apply; a
                // failing apply rolls the section back so disk and system
                // can never disagree.
                match self.transact_section(&self.passthrough, |c| {
                    *c = next.clone();
                    Ok(())
                }) {
                    Ok(()) => match self.apply_passthrough(&next) {
                        Ok(evt) => (ControlReply::ok(), vec![evt]),
                        Err(e) => {
                            // Restore disk *and* re-converge the backend:
                            // a mid-apply failure can leave apps moved
                            // (e.g. the picked app just closed).
                            let _ = self.transact_section(&self.passthrough, |c| {
                                *c = prev.clone();
                                Ok(())
                            });
                            let _ = self.apply_passthrough(&prev);
                            (ControlReply::err(format!("passthrough: {e}")), Vec::new())
                        }
                    },
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::SetDucking { enabled, depth_db, threshold_db, attack_ms, release_ms } => {
                let next = {
                    let cfg = self.ducking.lock().unwrap();
                    let mut n = cfg.clone();
                    if let Some(v) = enabled {
                        n.enabled = *v;
                    }
                    if let Some(v) = depth_db {
                        n.depth_db = *v;
                    }
                    if let Some(v) = threshold_db {
                        n.threshold_db = *v;
                    }
                    if let Some(v) = attack_ms {
                        n.attack_ms = *v;
                    }
                    if let Some(v) = release_ms {
                        n.release_ms = *v;
                    }
                    n.clamp();
                    n
                };
                match self.transact_section(&self.ducking, |c| {
                    *c = next.clone();
                    Ok(())
                }) {
                    Ok(()) => {
                        self.push_ducking_rt(&next);
                        (
                            ControlReply::ok(),
                            vec![ControlEvent::DuckingChanged {
                                enabled: next.enabled,
                                depth_db: next.depth_db,
                                threshold_db: next.threshold_db,
                                attack_ms: next.attack_ms,
                                release_ms: next.release_ms,
                            }],
                        )
                    }
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::GetDucking => {
                let cfg = self.ducking.lock().unwrap();
                let v = serde_json::to_value(&*cfg).unwrap_or(serde_json::Value::Null);
                (ControlReply::ok_data(v), Vec::new())
            }
            Action::GetLevels => {
                let lv = self.levels.lock().unwrap();
                (
                    ControlReply::ok_data(serde_json::json!({
                        "mic_db": lv.mic_db,
                        "out_db": lv.out_db,
                        "speech": lv.speech,
                    })),
                    Vec::new(),
                )
            }
            Action::SetObs { enabled, url, password } => {
                let next = {
                    let cfg = self.obs.lock().unwrap();
                    let mut n = cfg.clone();
                    if let Some(v) = enabled {
                        n.enabled = *v;
                    }
                    if let Some(v) = url {
                        n.url = v.clone();
                    }
                    if let Some(v) = password {
                        n.password = v.clone();
                    }
                    n
                };
                match self.transact_section(&self.obs, |c| {
                    *c = next.clone();
                    Ok(())
                }) {
                    Ok(()) => {
                        let _ = self.obs_tx.send(next);
                        (ControlReply::ok(), vec![self.obs_status_event()])
                    }
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::GetObsStatus => (ControlReply::ok_data(self.obs_status_json()), Vec::new()),
            Action::SetObsSceneProfile { scene, voice } => {
                if voice.trim().is_empty() {
                    return (ControlReply::err("voice must not be empty"), Vec::new());
                }
                match self.transact_section(&self.obs, |c| {
                    c.scenes.insert(scene.clone(), voice.clone());
                    Ok(())
                }) {
                    Ok(()) => (ControlReply::ok(), Vec::new()),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::RemoveObsSceneProfile { scene } => {
                match self.transact_section(&self.obs, |c| {
                    c.scenes.remove(scene);
                    Ok(())
                }) {
                    Ok(()) => (ControlReply::ok(), Vec::new()),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::ListObsSceneProfiles => {
                let obs = self.obs.lock().unwrap();
                let mut scenes: Vec<(&String, &String)> = obs.scenes.iter().collect();
                scenes.sort();
                let v = serde_json::json!({
                    "scenes": scenes
                        .iter()
                        .map(|(s, v)| serde_json::json!({ "scene": s, "voice": v }))
                        .collect::<Vec<_>>(),
                });
                (ControlReply::ok_data(v), Vec::new())
            }
            Action::GetMidiStatus => {
                let rt = self.midi_runtime.lock().unwrap();
                let cfg = self.midi.lock().unwrap();
                (
                    ControlReply::ok_data(serde_json::json!({
                        "connected": rt.connected,
                        "learning": rt.learn.is_some(),
                        "port": cfg.port,
                        "bindings": cfg.bindings.len(),
                        "enabled": cfg.enabled,
                    })),
                    Vec::new(),
                )
            }
            Action::SetMidi { enabled, port } => {
                let next = {
                    let cfg = self.midi.lock().unwrap();
                    let mut n = cfg.clone();
                    if let Some(v) = enabled {
                        n.enabled = *v;
                    }
                    if let Some(v) = port {
                        n.port = v.clone();
                    }
                    n
                };
                match self.transact_section(&self.midi, |c| {
                    *c = next;
                    Ok(())
                }) {
                    Ok(()) => (ControlReply::ok(), Vec::new()),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::MidiLearnStart { target } => {
                let probe = MidiBinding {
                    id: 0,
                    channel: None,
                    kind: MidiKind::Cc,
                    number: 0,
                    target: target.clone(),
                    enabled: true,
                };
                if let Err(e) = probe.validate() {
                    return (ControlReply::err(e), Vec::new());
                }
                self.midi_runtime.lock().unwrap().learn = Some(target.clone());
                (ControlReply::ok(), Vec::new())
            }
            Action::MidiLearnCancel => {
                self.midi_runtime.lock().unwrap().learn = None;
                (ControlReply::ok(), Vec::new())
            }
            Action::MidiBindAdd { channel, kind, number, target } => {
                let binding = MidiBinding {
                    id: 0,
                    channel: *channel,
                    kind: *kind,
                    number: *number,
                    target: target.clone(),
                    enabled: true,
                };
                if let Err(e) = binding.validate() {
                    return (ControlReply::err(e), Vec::new());
                }
                match self.transact_section(&self.midi, |c| {
                    let id = c.next_binding_id();
                    let mut b = binding.clone();
                    b.id = id;
                    c.bindings.push(b);
                    Ok(())
                }) {
                    Ok(()) => (ControlReply::ok(), vec![ControlEvent::MidiBindingsChanged]),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::MidiBindRemove { midi_id } => {
                match self.transact_section(&self.midi, |c| {
                    c.bindings.retain(|b| b.id != *midi_id);
                    Ok(())
                }) {
                    Ok(()) => (ControlReply::ok(), vec![ControlEvent::MidiBindingsChanged]),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
            Action::ListMidiBindings => {
                let cfg = self.midi.lock().unwrap();
                let v = serde_json::to_value(&cfg.bindings).unwrap_or(serde_json::Value::Null);
                (ControlReply::ok_data(serde_json::json!({ "bindings": v })), Vec::new())
            }
        }
    }
}

/// Extract the raw JSON value out of a SetVoiceParameter action for echoes.
fn action_value(action: &Action) -> serde_json::Value {
    match action {
        Action::SetVoiceParameter { value, .. } => value.clone(),
        _ => serde_json::Value::Null,
    }
}

fn hotkey_action(target: &HotkeyTarget, pressed: bool, state: &DaemonState) -> Option<Action> {
    match target {
        HotkeyTarget::ToggleVoiceChanger if pressed => {
            let cur = state.voice_enabled.load(Ordering::Relaxed);
            Some(Action::ToggleVoiceChanger { enabled: !cur })
        }
        // ToggleMute is real now (master mute flag); PTT/PTM below are
        // the momentary edges that ride on the same two actions.
        HotkeyTarget::ToggleMute if pressed => {
            let cur = state.muted.load(Ordering::Relaxed);
            Some(Action::SetMuted { muted: !cur })
        }
        HotkeyTarget::PushToTalk => Some(Action::ToggleVoiceChanger { enabled: pressed }),
        HotkeyTarget::PushToMute => Some(Action::SetMuted { muted: pressed }),
        HotkeyTarget::PlaySound(name) if pressed => Some(Action::PlaySound { sound: name.clone() }),
        HotkeyTarget::SelectVoice(name) if pressed => {
            Some(Action::SelectVoice { voice: name.clone() })
        }
        HotkeyTarget::StopAllSounds if pressed => Some(Action::StopAllSounds),
        HotkeyTarget::ToggleMonitor if pressed => {
            let cur = state.monitor.lock().unwrap().enabled;
            Some(Action::SetMonitor {
                enabled: Some(!cur),
                device: None,
                mix: None,
                volume_db: None,
                muted: None,
            })
        }
        HotkeyTarget::SaveReplay if pressed => Some(Action::SaveInstantReplay),
        _ => None,
    }
}

/// Broadcast a control event on the shared bus (hotkeys, MIDI, OBS).
fn publish(publisher: &tokio::sync::broadcast::Sender<String>, event: &ControlEvent) {
    if let Ok(text) = serde_json::to_string(event) {
        let _ = publisher.send(text);
    }
}

/// Write the VU line for OBS's Text source (read from file).
fn write_vu_file(path: &std::path::Path, state: &DaemonState) {
    let lv = state.levels.lock().unwrap();
    let voice = state.voice_name.lock().unwrap().clone();
    const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let idx = (((-lv.mic_db).clamp(0.0, 60.0) / 60.0 * (BARS.len() - 1) as f32).round() as usize)
        .min(BARS.len() - 1);
    let text = format!(
        "[{voice}] mic {:.1}dB out {:.1}dB {}",
        lv.mic_db,
        lv.out_db,
        BARS[idx]
    );
    drop(lv);
    let _ = std::fs::write(path, text);
}

/// obs-websocket bridge: reconnect loop, scene → voice autoswitch and the
/// VU text source (F-0.4.C 35).
async fn obs_bridge(
    state: Arc<DaemonState>,
    publisher: tokio::sync::broadcast::Sender<String>,
    mut cfg_rx: tokio::sync::watch::Receiver<ObsConfig>,
) {
    let vu_path = state
        .preset_dir
        .parent()
        .map(|p| p.join("obs-vu.txt"))
        .unwrap_or_else(|| std::path::PathBuf::from("obs-vu.txt"));
    let mut last_scene = String::new();
    loop {
        let cfg = cfg_rx.borrow().clone();
        if !cfg.enabled {
            if cfg_rx.changed().await.is_err() {
                return;
            }
            last_scene.clear();
            continue;
        }
        match vm_obs::ObsClient::connect(&cfg.url, &cfg.password).await {
            Ok(mut cli) => {
                last_scene.clear();
                state.set_obs_status(true, String::new(), cfg.url.clone());
                publish(&publisher, &state.obs_status_event());
                let mut vu = tokio::time::interval(std::time::Duration::from_millis(100));
                vu.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        changed = cfg_rx.changed() => {
                            if changed.is_err() { return; }
                            break; // config changed: reconnect with the new settings
                        }
                        _ = vu.tick() => write_vu_file(&vu_path, &state),
                        ev = cli.recv() => {
                            let Some((event_type, data)) = ev else { break };
                            if event_type != "CurrentProgramSceneChanged" {
                                continue;
                            }
                            let scene = data
                                .get("sceneName")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if scene == last_scene {
                                continue;
                            }
                            last_scene = scene.clone();
                            state.set_obs_status(true, scene.clone(), cfg.url.clone());
                            if let Some(voice) = cfg.scenes.get(&scene) {
                                let (reply, evts) = state.handle(&Action::SelectVoice { voice: voice.clone() });
                                if !reply.ok {
                                    eprintln!("obs scene '{scene}': {:?}", reply.error);
                                }
                                for e in &evts {
                                    publish(&publisher, e);
                                }
                            }
                            publish(&publisher, &state.obs_status_event());
                        }
                    }
                }
                state.set_obs_status(false, String::new(), cfg.url.clone());
                publish(&publisher, &state.obs_status_event());
            }
            Err(e) => {
                eprintln!("obs: {e}");
                state.set_obs_status(false, String::new(), cfg.url.clone());
                publish(&publisher, &state.obs_status_event());
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    cfg_rx.changed(),
                )
                .await;
            }
        }
    }
}

/// MIDI input loop: open the configured port, dispatch events, reopen when
/// the enable/port settings change (F-0.4.C 37).
#[cfg(feature = "midi")]
fn midi_input_loop(
    state: Arc<DaemonState>,
    publisher: tokio::sync::broadcast::Sender<String>,
) {
    // Repeat open failures every 2 s, but only print when the message
    // changes — a machine with no ALSA ports must not spam the log.
    let mut last_err: Option<String> = None;
    loop {
        let (enabled, port) = {
            let cfg = state.midi.lock().unwrap();
            (cfg.enabled, cfg.port.clone())
        };
        if !enabled {
            std::thread::sleep(std::time::Duration::from_millis(250));
            continue;
        }
        let opened_port = port.clone();
        let input = match vm_midi::MidiInput::open(if port.is_empty() { None } else { Some(port.as_str()) }) {
            Ok(i) => {
                last_err = None;
                i
            }
            Err(e) => {
                let msg = e.to_string();
                if last_err.as_deref() != Some(msg.as_str()) {
                    eprintln!("midi: {msg}");
                    last_err = Some(msg);
                }
                state.set_midi_connected(false);
                std::thread::sleep(std::time::Duration::from_secs(2));
                continue;
            }
        };
        let label = if opened_port.is_empty() {
            "<first port>".to_string()
        } else {
            opened_port.clone()
        };
        eprintln!("midi: listening on {label}");
        state.set_midi_connected(true);
        loop {
            match input.recv_timeout(std::time::Duration::from_millis(200)) {
                vm_midi::MidiRecv::Event(ev) => state.dispatch_midi(&ev, &publisher),
                vm_midi::MidiRecv::Disconnected => break,
                vm_midi::MidiRecv::Timeout => {
                    let (enabled, port) = {
                        let cfg = state.midi.lock().unwrap();
                        (cfg.enabled, cfg.port.clone())
                    };
                    if !enabled || port != opened_port {
                        break; // settings changed: reopen below
                    }
                }
            }
        }
        state.set_midi_connected(false);
    }
}

fn config_dirs() -> (PathBuf, PathBuf) {
    let base = std::env::var("VM_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs_fallback().join("vm")
        });
    (base.join("presets"), base.join("sounds"))
}

fn dirs_fallback() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[tokio::main]
async fn main() {
    let live = std::env::args().any(|a| a == "--live");
    if let Err(e) = run(live).await {
        eprintln!("vm-daemon: {e}");
        std::process::exit(1);
    }
}

/// Copy the built-in presets into `preset_dir` on boot. Never
/// overwrites an existing file: user edits to a seeded preset win.
/// (`example-drive` is seeded separately below because it is generated,
/// not embedded.)
fn seed_builtin_presets(preset_dir: &std::path::Path) -> std::io::Result<()> {
    for (stem, text) in vm_preset::BUILTIN_PRESETS {
        let path = preset_dir.join(format!("{stem}.json"));
        if !path.exists() {
            std::fs::write(path, text)?;
        } else if std::fs::read_to_string(&path).map(|t| t != *text).unwrap_or(false) {
            // Never overwrite — but say so loudly, or factory
            // recalibrations (stronger denoise, retuned chains) would
            // never reach existing installs.
            eprintln!(
                "preset '{stem}' differs from the factory version; \
                 delete {} to receive the update (your edits will be lost)",
                path.display()
            );
        }
    }
    Ok(())
}

/// Load the persisted daemon config. Missing file = first boot (empty
/// config, no error). A corrupt file is quarantined to `config.json.bak`
/// instead of being overwritten — hand edits are never silently eaten.
fn load_daemon_config_file(path: &std::path::Path) -> Result<vm_control::DaemonConfig, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(vm_control::DaemonConfig::default())
        }
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    match vm_control::load_daemon_config(&text) {
        Ok(b) => Ok(b),
        Err(e) => {
            let bak = path.with_extension("json.bak");
            if std::fs::rename(path, &bak).is_ok() {
                Err(format!("{e} (moved to {})", bak.display()))
            } else {
                Err(e)
            }
        }
    }
}

async fn run(live: bool) -> Result<(), Box<dyn std::error::Error>> {
    let (preset_dir, sound_dir) = config_dirs();
    std::fs::create_dir_all(&preset_dir)?;
    std::fs::create_dir_all(&sound_dir)?;
    // Seed the example preset on first boot (dogfoods the format).
    let example = preset_dir.join("example-drive.json");
    if !example.exists() {
        let text = serde_json::to_string_pretty(&vm_preset::example_drive())?;
        std::fs::write(&example, text)?;
    }
    // Ship the ready-made mic presets (Fase 7); user edits always win.
    seed_builtin_presets(&preset_dir)?;

    // Hotkey binds + device profiles (F-B/F-0.4.D): load the persisted
    // config before the state exists so the API, the evdev threads and
    // the autoswitch share one store.
    let config_path = preset_dir
        .parent()
        .map(|p| p.join("config.json"))
        .unwrap_or_else(|| PathBuf::from("/tmp/vm-config.json"));
    let mut hotkeys = HotkeyManager::new();
    let mut device_profiles = HashMap::new();
    let mut monitor = MonitorConfig::default();
    let mut ducking = DuckingConfig::default();
    let mut obs = ObsConfig::default();
    let mut midi = MidiConfig::default();
    let mut replay = ReplayConfig::default();
    let mut passthrough = PassthroughConfig::default();
    match load_daemon_config_file(&config_path) {
        Ok(cfg) => {
            if !cfg.hotkeys.is_empty() {
                println!(
                    "hotkeys: loaded {} binds from {}",
                    cfg.hotkeys.len(),
                    config_path.display()
                );
            }
            hotkeys.import(cfg.hotkeys);
            if !cfg.device_profiles.is_empty() {
                println!(
                    "device profiles: loaded {} from {}",
                    cfg.device_profiles.len(),
                    config_path.display()
                );
            }
            device_profiles = cfg.device_profiles;
            monitor = cfg.monitor;
            ducking = cfg.ducking;
            obs = cfg.obs;
            midi = cfg.midi;
            replay = cfg.replay;
            passthrough = cfg.passthrough;
        }
        Err(e) => eprintln!("config: {e} (starting empty)"),
    }

    let ducking_rt = Arc::new(DuckingRt::default());
    let (obs_tx, obs_rx) = tokio::sync::watch::channel(obs.clone());
    let replay_ring = {
        let mut ring = ReplayRing::new(replay.seconds, SAMPLE_RATE);
        ring.set_enabled(replay.enabled);
        Arc::new(Mutex::new(ring))
    };
    let state = Arc::new(DaemonState {
        graph: Arc::new(Mutex::new(Graph::new())),
        node_ids: Mutex::new(HashMap::new()),
        voice_name: Mutex::new(String::new()),
        voice_enabled: Arc::new(AtomicBool::new(true)),
        mixer: Arc::new(Mutex::new(vm_soundboard::SoundboardMixer::new())),
        sound_ids: Mutex::new(HashMap::new()),
        audio: Arc::new(Mutex::new(build_backend(live)?)),
        preset_dir: preset_dir.clone(),
        topology: Mutex::new(None),
        graph_xruns: Arc::new(AtomicU64::new(0)),
        watchdog: Mutex::new(WatchdogState::default()),
        muted: Arc::new(AtomicBool::new(false)),
        hotkeys: Arc::new(Mutex::new(hotkeys)),
        config_path: config_path.clone(),
        device_profiles: Mutex::new(device_profiles),
        monitor: Mutex::new(monitor),
        ducking: Mutex::new(ducking),
        ducking_rt: ducking_rt.clone(),
        obs: Mutex::new(obs),
        obs_status: Arc::new(Mutex::new(ObsStatus::default())),
        midi: Mutex::new(midi),
        midi_runtime: Arc::new(Mutex::new(MidiRuntime::default())),
        replay: Mutex::new(replay),
        replay_ring: replay_ring.clone(),
        passthrough: Mutex::new(passthrough),
        levels: Arc::new(Mutex::new(LevelsSnapshot::default())),
        obs_tx,
    });
    // Default voice.
    {
        let text = std::fs::read_to_string(preset_dir.join("example-drive.json"))?;
        let preset = vm_preset::load(&text)?;
        state.apply_preset(&preset).map_err(|e| format!("preset: {e}"))?;
    }
    // Graph processor shares the live graph + bypass flag.
    let graph_xruns = state.graph_xruns.clone();
    {
        let cfg = state.ducking.lock().unwrap().clone();
        state.push_ducking_rt(&cfg);
    }
    let processor: Arc<Mutex<dyn StreamProcessor>> = Arc::new(Mutex::new(GraphProcessor::new(
        state.graph.clone(),
        state.voice_enabled.clone(),
        state.muted.clone(),
        graph_xruns,
        state.mixer.clone(),
        state.ducking_rt.clone(),
        state.levels.clone(),
        state.replay_ring.clone(),
    )));
    // Preload sounds (decode+resample now; never on the trigger path).
    {
        let mut mixer = state.mixer.lock().unwrap();
        let mut ids = state.sound_ids.lock().unwrap();
        let mut entries: Vec<_> = std::fs::read_dir(&sound_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
                    Some("wav" | "flac" | "mp3" | "ogg" | "opus" | "oga")
                )
            })
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("sound").to_string();
            let id = vm_soundboard::SoundHashId::from_name(&name);
            match decode_to_loaded(id, &name, &path) {
                Ok(loaded) => {
                    mixer.add_sound(loaded);
                    ids.insert(name.clone(), id);
                    println!("sound loaded: {name}");
                }
                Err(e) => eprintln!("sound skipped {name}: {e}"),
            }
        }
    }

    // Audio backend bring-up (live with stub fallback).
    let topo = {
        let mut audio = state.audio.lock().unwrap();
        audio.set_processor(processor);
        let devices = audio.enumerate()?;
        println!("devices:");
        for d in &devices {
            println!("  {} ({:?})", d.uid.0, d.role);
        }
        let vm_uid = audio.ensure_virtual_source()?;
        println!("virtual source: {}", vm_uid.0);
        // Re-enumerate so the fresh node id is resolvable.
        let devices = audio.enumerate()?;
        let input = pick_input(&devices).ok_or("no input device found")?;
        let virtual_out = devices
            .iter()
            .find(|d| d.uid == vm_uid)
            .or_else(|| devices.iter().find(|d| d.role == DeviceRole::Virtual))
            .ok_or("no virtual output found")?;
        println!("using input: {}", input.uid.0);
        let topo = StreamTopology {
            input: input.uid.clone(),
            virtual_out: virtual_out.uid.clone(),
            monitor: None,
        };
        audio.open(&topo)?;
        audio.start()?;
        println!("streaming. Ctrl-C to stop.");
        topo
    };
    *state.topology.lock().unwrap() = Some(topo.clone());
    // Boot-apply the persisted sections the backend owns (monitor 15,
    // passthrough 18): loading the config into memory is not enough — a
    // reboot must reproduce the last known-good routing.
    {
        let cfg = state.monitor.lock().unwrap().clone();
        if let Err(e) = state.apply_monitor(&cfg) {
            // F-0.4.2: a pinned output that vanished while we were down
            // must not brick the monitor — drop to the system default,
            // persist it and keep the feature alive.
            if cfg.device.is_empty() {
                eprintln!("monitor: boot apply failed: {e}");
            } else {
                eprintln!("monitor: boot apply failed: {e}; falling back to system default");
                let next = {
                    let mut n = cfg.clone();
                    n.device = String::new();
                    n
                };
                if let Err(e) = state.transact_section(&state.monitor, |c| {
                    *c = next.clone();
                    Ok(())
                }) {
                    eprintln!("monitor: persisting boot fallback failed: {e}");
                }
                if let Err(e) = state.apply_monitor(&next) {
                    eprintln!("monitor: boot fallback failed: {e}");
                }
            }
        }
    }
    if state.passthrough.lock().unwrap().enabled {
        let cfg = state.passthrough.lock().unwrap().clone();
        if let Err(e) = state.apply_passthrough(&cfg) {
            eprintln!("passthrough: boot apply failed: {e}");
        }
    }
    // Boot device profile: a bound input starts on its voice instead of
    // the default (new hardware keeps sounding right across restarts).
    {
        let input_uid = topo.input.0.clone();
        let input_name = {
            let mut audio = state.audio.lock().unwrap();
            audio
                .enumerate()
                .unwrap_or_default()
                .into_iter()
                .find(|d| d.uid.0 == input_uid)
                .map(|d| d.name.clone())
                .unwrap_or_default()
        };
        state.autoswitch_for_device(&input_uid, &input_name);
    }
    let _ = topo;

    // Control plane (auth opt-in via VM_CONTROL_TOKEN).
    let mut cfg = ServerConfig::default();
    if std::env::var("VM_CONTROL_TOKEN").map(|t| !t.is_empty()).unwrap_or(false) {
        cfg.require_auth = true;
        println!("control api: token auth enabled");
    } else {
        println!("control api: WITHOUT auth (loopback dev default; set VM_CONTROL_TOKEN to enable)");
    }
    let server = ControlApiServer::new(cfg, state.clone());
    let port = server.serve().await.map_err(|e| format!("control api: {e}"))?;
    println!("control api listening (effective port {port})");

    // Hotkeys (F-B): the API-owned store is shared with the evdev
    // watch threads. Sandboxed (Flatpak) means no /dev/input by design:
    // say so plainly instead of the generic permissions hint.
    if let Some(note) = vm_control::sandbox_note() {
        eprintln!("{note}");
    }
    {
        let manager = state.hotkeys.clone();
        let publisher = server.publisher();
        let keyboards = HotkeyManager::scan_keyboards();
        if keyboards.is_empty() && vm_control::sandbox_note().is_none() {
            eprintln!("hotkeys: no readable keyboards (need input group/udev rule)");
        }
        for path in keyboards {
            let path = path.split(' ').next().unwrap_or("").to_string();
            if path.is_empty() {
                continue;
            }
            let manager = manager.clone();
            let state = state.clone();
            let publisher = publisher.clone();
            std::thread::Builder::new()
                .name("vm-hotkey".into())
                .spawn(move || {
                    vm_control::hotkeys::watch_device(path, manager, move |target, pressed| {
                        if let Some(action) = hotkey_action(&target, pressed, &state) {
                            let (reply, evts) = state.handle(&action);
                            if !reply.ok {
                                eprintln!("hotkey action failed: {:?}", reply.error);
                            }
                            for e in &evts {
                                publish(&publisher, e);
                            }
                        }
                    })
                })
                .ok();
        }
    }

    // obs-websocket bridge (F-0.4.C 35): scene → voice autoswitch + VU
    // text source. Reconnects with backoff until disabled.
    {
        let state = state.clone();
        let publisher = server.publisher();
        tokio::spawn(async move { obs_bridge(state, publisher, obs_rx).await });
    }

    // MIDI learn input (F-0.4.C 37; ALSA sequencer, feature-gated).
    #[cfg(feature = "midi")]
    {
        let state = state.clone();
        let publisher = server.publisher();
        std::thread::Builder::new()
            .name("vm-midi".into())
            .spawn(move || midi_input_loop(state, publisher))
            .ok();
    }

    // Hotplug watch + status.
    let mut tick = 0u32;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                let mut arrivals = Vec::new();
                let mut removed = Vec::new();
                let mut default_changed = false;
                {
                    let mut audio = state.audio.lock().unwrap();
                    for ev in audio.poll_hotplug() {
                        println!("hotplug: {ev:?} (link: {:?})", audio.link_state());
                        match ev {
                            // A (re)plugged input with a bound voice takes
                            // over immediately (F-0.4.D device profiles).
                            vm_audio::HotplugEvent::DeviceAdded(dev) => {
                                if dev.role == DeviceRole::Input {
                                    arrivals.push((dev.uid.0.clone(), dev.name.clone()));
                                }
                            }
                            vm_audio::HotplugEvent::DeviceRemoved(uid) => {
                                removed.push(uid.0);
                            }
                            // The backend re-links its own monitor when it
                            // tracks the default (F-0.4.2); UIs just need a
                            // list refresh.
                            vm_audio::HotplugEvent::DefaultChanged { .. } => {
                                default_changed = true;
                            }
                        }
                    }
                }
                for (uid, name) in arrivals {
                    state.autoswitch_for_device(&uid, &name);
                }
                // Monitor output unplugged (F-0.4.2): fall back to the
                // system default, persist and tell every listener — the
                // config must never keep pointing at a device that is
                // gone. The backend already dropped the dead links.
                let monitor_lost = {
                    let dev = state.monitor.lock().unwrap().device.clone();
                    !dev.is_empty() && removed.contains(&dev)
                };
                if monitor_lost {
                    println!("monitor: output device removed — falling back to system default");
                    let (reply, evts) = state.handle(&Action::SetMonitor {
                        enabled: None,
                        device: Some(String::new()),
                        mix: None,
                        volume_db: None,
                        muted: None,
                    });
                    if !reply.ok {
                        eprintln!("monitor: hotplug fallback failed: {:?}", reply.error);
                    }
                    let publisher = server.publisher();
                    for e in &evts {
                        publish(&publisher, e);
                    }
                }
                if default_changed {
                    publish(&server.publisher(), &ControlEvent::DevicesChanged);
                }
                // Watchdog (F-A): auto-restart a dead link / xrun storm.
                state.watchdog_tick();
                tick += 1;
                {
                    let audio = state.audio.lock().unwrap();
                    if tick.is_multiple_of(5) {
                        use vm_audio::StreamKind as K;
                        let c = audio.stats(K::FullDuplexCapture);
                        let v = audio.stats(K::HalfDuplexPlayback);
                        let m = audio.stats(K::Monitor);
                        let (cr, cc, or, oc) = audio.negotiated();
                        println!(
                            "stats link={:?} fmt=cap{cc}ch@{cr}/out{oc}ch@{or} cap{{n={} avg={:.0}us xr={}}} vad{{n={} avg={:.0}us xr={} cont={} fr={}}} mon{{n={} avg={:.0}us xr={}}}",
                            audio.link_state(),
                            c.invocations, c.avg_duration_us, c.xruns,
                            v.invocations, v.avg_duration_us, v.xruns, v.watchdog_trips, v.last_frames,
                            m.invocations, m.avg_duration_us, m.xruns,
                        );
                    }
                }
            }
        }
    }
    state.audio.lock().unwrap().stop(StreamStopReason::Requested)?;
    println!("stopped.");
    Ok(())
}

fn pick_input(devices: &[vm_audio::AudioDevice]) -> Option<&vm_audio::AudioDevice> {
    if let Ok(want) = std::env::var("VM_INPUT_UID") {
        if let Some(dev) = devices.iter().find(|d| d.uid.0 == want) {
            println!("using override input: {}", dev.uid.0);
            return Some(dev);
        }
        eprintln!("VM_INPUT_UID '{want}' not found, falling back to auto-select");
    }
    devices
        .iter()
        .find(|d| {
            d.role == DeviceRole::Input
                && !d.uid.0.contains("easyeffects")
                && !d.uid.0.contains("monitor")
        })
        .or_else(|| devices.iter().find(|d| d.role == DeviceRole::Input))
}

fn decode_to_loaded(
    id: vm_soundboard::SoundHashId,
    name: &str,
    path: &std::path::Path,
) -> Result<vm_soundboard::LoadedSound, Box<dyn std::error::Error>> {
    // Reuse the soundboard's own decode path, then hand the decoded PCM
    // to the mixer. (This used to end in `Err("unreachable")`, which
    // silently skipped every sound at boot.)
    let decoded = vm_soundboard::decode::decode_file(path)?;
    Ok(vm_soundboard::LoadedSound {
        id,
        name: name.to_string(),
        rate: decoded.rate,
        channels: decoded.channels,
    })
}

fn build_backend(live: bool) -> Result<Box<dyn AudioBackend>, Box<dyn std::error::Error>> {
    if live {
        match vm_audio::backend::live::PipeWireBackend::new() {
            Ok(b) => return Ok(Box::new(b)),
            Err(e) => eprintln!("live backend unavailable ({e}); falling back to stub"),
        }
    }
    Ok(Box::new(vm_audio::backend::stub::StubBackend::new()))
}

#[cfg(test)]
fn test_state() -> Arc<DaemonState> {
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("vm-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("presets")).unwrap();
    std::fs::create_dir_all(dir.join("sounds")).unwrap();
    let state = Arc::new(DaemonState {
        graph: Arc::new(Mutex::new(Graph::new())),
        node_ids: Mutex::new(HashMap::new()),
        voice_name: Mutex::new(String::new()),
        voice_enabled: Arc::new(AtomicBool::new(true)),
        mixer: Arc::new(Mutex::new(vm_soundboard::SoundboardMixer::new())),
        sound_ids: Mutex::new(HashMap::new()),
        audio: Arc::new(Mutex::new(build_backend(false).unwrap())),
        preset_dir: dir.join("presets"),
        topology: Mutex::new(None),
        graph_xruns: Arc::new(AtomicU64::new(0)),
        watchdog: Mutex::new(WatchdogState::default()),
        muted: Arc::new(AtomicBool::new(false)),
        hotkeys: Arc::new(Mutex::new(HotkeyManager::new())),
        config_path: dir.join("config.json"),
        device_profiles: Mutex::new(HashMap::new()),
        monitor: Mutex::new(MonitorConfig::default()),
        ducking: Mutex::new(DuckingConfig::default()),
        ducking_rt: Arc::new(DuckingRt::default()),
        obs: Mutex::new(ObsConfig::default()),
        obs_status: Arc::new(Mutex::new(ObsStatus::default())),
        midi: Mutex::new(MidiConfig::default()),
        midi_runtime: Arc::new(Mutex::new(MidiRuntime::default())),
        replay: Mutex::new(ReplayConfig::default()),
        replay_ring: Arc::new(Mutex::new(ReplayRing::new(30, SAMPLE_RATE))),
        passthrough: Mutex::new(PassthroughConfig::default()),
        levels: Arc::new(Mutex::new(LevelsSnapshot::default())),
        obs_tx: tokio::sync::watch::channel(ObsConfig::default()).0,
    });
    // Seed example preset + one wav sound.
    let text = serde_json::to_string_pretty(&vm_preset::example_drive()).unwrap();
    std::fs::write(dir.join("presets").join("example-drive.json"), &text).unwrap();
    write_test_wav(&dir.join("sounds").join("blip.wav"));
    state
}

#[cfg(test)]
fn write_test_wav(path: &std::path::Path) {
    let s: Vec<i16> = (0..4800)
        .map(|i| (0.5 * 32767.0 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin()) as i16)
        .collect();
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&((36 + s.len() * 2) as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&48_000u32.to_le_bytes());
    v.extend_from_slice(&96_000u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(s.len() as u32 * 2).to_le_bytes());
    for x in s {
        v.extend_from_slice(&x.to_le_bytes());
    }
    std::fs::write(path, v).unwrap();
}

#[cfg(test)]
fn load_test_sound(state: &Arc<DaemonState>, name: &str) {
    let dir = state.preset_dir.parent().unwrap().join("sounds");
    let id = vm_soundboard::SoundHashId::from_name(name);
    let decoded = vm_soundboard::decode::decode_file(&dir.join(format!("{name}.wav"))).unwrap();
    state.mixer.lock().unwrap().add_sound(vm_soundboard::LoadedSound {
        id,
        name: name.to_string(),
        rate: decoded.rate,
        channels: decoded.channels,
    });
    state.sound_ids.lock().unwrap().insert(name.to_string(), id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_control::Action;

    #[test]
    fn select_voice_and_set_param() {
        let state = test_state();
        let (reply, evts) = state.handle(&Action::SelectVoice { voice: "example-drive".into() });
        assert!(reply.ok, "{:?}", reply.error);
        assert!(matches!(evts[0], ControlEvent::VoiceChanged { .. }));
        assert_eq!(*state.voice_name.lock().unwrap(), "example-drive".to_string());

        let (reply, evts) = state.handle(&Action::SetVoiceParameter {
            param: "drive.gain".into(),
            value: serde_json::json!(2.5),
        });
        assert!(reply.ok, "{:?}", reply.error);
        assert!(matches!(evts[0], ControlEvent::VoiceParameterChanged { .. }));

        // Unknown node / bad path / bad type.
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "nope.gain".into(),
            value: serde_json::json!(1.0),
        });
        assert!(!reply.ok);
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "gain".into(),
            value: serde_json::json!(1.0),
        });
        assert!(!reply.ok);
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "missing".into() });
        assert!(!reply.ok);
    }

    #[test]
    fn sound_lifecycle() {
        let state = test_state();
        load_test_sound(&state, "blip");
        let (reply, evts) = state.handle(&Action::PlaySound { sound: "blip".into() });
        assert!(reply.ok, "{:?}", reply.error);
        assert!(matches!(evts[0], ControlEvent::SoundStarted { .. }));
        // Mix a block: routed to both buses by default.
        let (vad, mon) = state.mixer.lock().unwrap().mix(480);
        assert!(vad.left.iter().any(|s| *s != 0.0));
        assert!(mon.left.iter().any(|s| *s != 0.0));
        // Fade-out retires the voice.
        let (reply, _) = state.handle(&Action::FadeSound { sound: "blip".into(), fade_out_secs: 0.001 });
        assert!(reply.ok, "{:?}", reply.error);
        state.mixer.lock().unwrap().mix(4800);
        assert!(state.mixer.lock().unwrap().playing().is_empty());
        // Unknown sound errors.
        let (reply, _) = state.handle(&Action::PlaySound { sound: "nope".into() });
        assert!(!reply.ok);
        // stopAll on empty is fine (and emits nothing).
        let (reply, evts) = state.handle(&Action::StopAllSounds);
        assert!(reply.ok);
        assert!(evts.is_empty());
    }

    #[test]
    fn stop_all_sounds_emits_per_sound_events() {
        let state = test_state();
        load_test_sound(&state, "blip");
        let (reply, _) = state.handle(&Action::PlaySound { sound: "blip".into() });
        assert!(reply.ok);
        let (reply, evts) = state.handle(&Action::StopAllSounds);
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(evts.len(), 1);
        assert!(matches!(&evts[0], ControlEvent::SoundStopped { sound } if sound == "blip"));
        assert!(state.mixer.lock().unwrap().playing().is_empty());
    }

    #[test]
    fn get_voices_lists_preset_dir() {
        let state = test_state();
        // test_state seeds example-drive.json only.
        let (reply, _) = state.handle(&Action::GetVoices);
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(reply.data.unwrap()["voices"], serde_json::json!(["example-drive"]));
        // Sorted stems, json extension only.
        std::fs::write(state.preset_dir.join("mic-npr-voice.json"), "{}").unwrap();
        std::fs::write(state.preset_dir.join("notes.txt"), "x").unwrap();
        let (reply, _) = state.handle(&Action::GetVoices);
        assert_eq!(
            reply.data.unwrap()["voices"],
            serde_json::json!(["example-drive", "mic-npr-voice"])
        );
    }

    #[test]
    fn master_volume_and_select_input() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SetSoundboardMasterVolume { per_mille: 420 });
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(state.mixer.lock().unwrap().master_volume(), 420);
        // Clamped at 1000.
        let (reply, _) = state.handle(&Action::SetSoundboardMasterVolume { per_mille: 9999 });
        assert!(reply.ok);
        assert_eq!(state.mixer.lock().unwrap().master_volume(), 1000);

        // selectInput on the stub backend: needs an opened pipeline first.
        // Open the stub topology directly, then switch inputs.
        let topo = vm_audio::StreamTopology {
            input: vm_audio::DeviceUid::new("stub:input:0"),
            virtual_out: vm_audio::DeviceUid::new("stub:virtual:vm-mic"),
            monitor: None,
        };
        {
            let mut audio = state.audio.lock().unwrap();
            audio.enumerate().unwrap();
            audio.open(&topo).unwrap();
            audio.start().unwrap();
        }
        *state.topology.lock().unwrap() = Some(topo);
        let (reply, evts) = state.handle(&Action::SelectInput { uid: "stub:input:0".into() });
        assert!(reply.ok, "{:?}", reply.error);
        assert!(matches!(evts[0], ControlEvent::DevicesChanged));
        let (reply, _) = state.handle(&Action::SelectInput { uid: "nope".into() });
        assert!(!reply.ok);
    }

    #[test]
    fn toggle_and_status_with_stub() {
        let state = test_state();
        let (reply, evts) = state.handle(&Action::ToggleVoiceChanger { enabled: false });
        assert!(reply.ok);
        assert!(!state.voice_enabled.load(Ordering::Relaxed));
        assert!(matches!(evts[0], ControlEvent::VoiceChangerToggled { enabled: false }));
        // Bypass actually passes audio through.
        let mut proc = GraphProcessor::for_state(&state);
        let input = [vec![0.3f32; 64]];
        let mut out = [vec![0.0f32; 64]];
        let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
        proc.process(48_000, &ins, &mut outs);
        assert!((out[0][0] - 0.3).abs() < 1e-6);

        // Status works against the stub backend (no hardware).
        let (reply, _) = state.handle(&Action::GetAudioStreamStatus);
        assert!(reply.ok, "{:?}", reply.error);
        let (reply, _) = state.handle(&Action::GetAudioDevices);
        assert!(reply.ok, "{:?}", reply.error);
        let data = reply.data.unwrap();
        assert_eq!(data["devices"].as_array().unwrap().len(), 3);
        let (reply, _) = state.handle(&Action::GetUserLicense);
        assert!(reply.ok);
        let (reply, _) = state.handle(&Action::GetAllSoundboard);
        assert!(reply.ok);
        let (reply, _) = state.handle(&Action::GetBitmap { resource: "x".into() });
        assert!(!reply.ok);
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    fn open_stub_topology(state: &Arc<DaemonState>) {
        let topo = vm_audio::StreamTopology {
            input: vm_audio::DeviceUid::new("stub:input:0"),
            virtual_out: vm_audio::DeviceUid::new("stub:virtual:vm-mic"),
            monitor: None,
        };
        {
            let mut audio = state.audio.lock().unwrap();
            audio.enumerate().unwrap();
            audio.open(&topo).unwrap();
            audio.start().unwrap();
        }
        *state.topology.lock().unwrap() = Some(topo);
    }

    #[test]
    fn health_reports_nodes_streams_and_watchdog() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "example-drive".into() });
        assert!(reply.ok, "{:?}", reply.error);
        // Run a few quanta so telemetry has calls.
        let mut proc = GraphProcessor::for_state(&state);
        for _ in 0..4 {
            let input = vec![vec![0.1f32; 64]; 2];
            let mut out = [vec![0.0f32; 64], vec![0.0f32; 64]];
            let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
        }
        let (reply, _) = state.handle(&Action::GetHealth);
        assert!(reply.ok, "{:?}", reply.error);
        let data = reply.data.unwrap();
        assert_eq!(data["voice"], "example-drive");
        assert_eq!(data["graphXruns"], 0);
        let nodes = data["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        // Deterministic wire order + preset ids joined.
        assert_eq!(nodes[0]["node"], "drive");
        assert_eq!(nodes[1]["node"], "smooth");
        for n in nodes {
            assert_eq!(n["calls"], 4, "node {}", n["node"]);
            assert!(n["avgUs"].as_f64().unwrap() >= 0.0);
        }
        // Stream blocks present even on the stub (all zeros).
        assert_eq!(data["capture"]["xruns"], 0);
        assert_eq!(data["watchdog"]["restarts"], 0);
        // Button-init snapshot: changer + mute state without toggling.
        assert_eq!(data["enabled"], true);
        assert_eq!(data["muted"], false);
    }

    #[test]
    fn graph_lock_contention_counts_xrun() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "example-drive".into() });
        assert!(reply.ok);
        let mut proc = GraphProcessor::for_state(&state);
        // Hold the graph lock: the RT path must skip, not block.
        let _held = state.graph.lock().unwrap();
        let input = vec![vec![0.1f32; 64]; 2];
        let mut out = [vec![0.0f32; 64], vec![0.0f32; 64]];
        let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
        proc.process(48_000, &ins, &mut outs);
        drop(_held);
        assert_eq!(state.graph_xruns.load(Ordering::Relaxed), 1);
        let (reply, _) = state.handle(&Action::GetHealth);
        assert_eq!(reply.data.unwrap()["graphXruns"], 1);
    }

    #[test]
    fn watchdog_policy_matrix() {
        let snap = |link, xruns, topo| LinkSnapshot {
            link,
            xruns_total: xruns,
            has_topology: topo,
        };
        use vm_audio::StreamLinkState as L;
        // Never opened: nothing to restart (boot owns bring-up).
        assert!(decide_watchdog(&snap(L::Down, 0, false), 0, true).is_none());
        // Cooldown suppresses even a dead link (no open() hot loop).
        assert!(decide_watchdog(&snap(L::NeedsRestart, 0, true), 0, false).is_none());
        // Dead link restarts.
        let r = decide_watchdog(&snap(L::NeedsRestart, 5, true), 5, true).unwrap();
        assert!(r.contains("needs restart"), "{r}");
        // Xrun storm restarts.
        let r = decide_watchdog(&snap(L::Streaming, 100, true), 80, true).unwrap();
        assert!(r.contains("storm"), "{r}");
        // A few xruns are normal operation, not a storm.
        assert!(decide_watchdog(&snap(L::Streaming, 84, true), 80, true).is_none());
        // Quiet link: nothing to do.
        assert!(decide_watchdog(&snap(L::Streaming, 80, true), 80, true).is_none());
    }

    #[test]
    fn watchdog_tick_is_quiet_on_stub_and_restart_reopens() {
        let state = test_state();
        // No topology yet: tick must be a no-op (boot owns bring-up).
        state.watchdog_tick();
        assert_eq!(state.watchdog.lock().unwrap().restarts, 0);
        // Open the stub pipeline: link stays Down (stub default) with no
        // xruns -> still quiet, no spurious restarts.
        open_stub_topology(&state);
        state.watchdog_tick();
        assert_eq!(state.watchdog.lock().unwrap().restarts, 0);
        // Direct restart path reopens the same topology.
        state.restart_pipeline().expect("stub reopen");
        let topo = state.topology.lock().unwrap().clone().unwrap();
        assert_eq!(topo.input.0, "stub:input:0");
    }
}

#[cfg(test)]
mod hotkey_api_tests {
    use super::*;
    use vm_control::HotkeyTarget;

    fn add_bind(
        state: &Arc<DaemonState>,
        codes: &[u16],
        target: HotkeyTarget,
    ) -> serde_json::Value {
        let (reply, _) = state.handle(&Action::AddHotkey {
            codes: codes.to_vec(),
            target,
            enabled: true,
        });
        assert!(reply.ok, "{:?}", reply.error);
        reply.data.unwrap()
    }

    #[test]
    fn add_list_remove_roundtrip_with_file() {
        let state = test_state();
        assert!(!state.config_path.exists(), "fresh temp dir has no config");
        let data = add_bind(&state, &[29, 42], HotkeyTarget::PushToTalk);
        assert_eq!(data["id"], 0);
        let (reply, _) = state.handle(&Action::ListHotkeys);
        assert!(reply.ok);
        let binds = reply.data.unwrap()["hotkeys"].as_array().unwrap().clone();
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0]["id"], 0);
        assert_eq!(binds[0]["codes"], serde_json::json!([29, 42]));
        assert_eq!(binds[0]["target"], "PushToTalk");
        assert_eq!(binds[0]["enabled"], true);
        // Persisted to config.json with schema version.
        let text = std::fs::read_to_string(&state.config_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], 5);
        assert_eq!(v["hotkeys"].as_array().unwrap().len(), 1);
        // F-0.4.C/B: the v5 schema always carries the integration sections.
        assert!(v["monitor"].is_object());
        assert!(v["ducking"].is_object());
        assert!(v["obs"].is_object());
        assert!(v["midi"].is_object());

        let (reply, _) = state.handle(&Action::RemoveHotkey { hotkey_id: 0 });
        assert!(reply.ok, "{:?}", reply.error);
        let (reply, _) = state.handle(&Action::ListHotkeys);
        assert_eq!(reply.data.unwrap()["hotkeys"].as_array().unwrap().len(), 0);
        // Unknown id errors cleanly.
        let (reply, _) = state.handle(&Action::RemoveHotkey { hotkey_id: 0 });
        assert!(!reply.ok);
    }

    #[test]
    fn add_rejects_invalid_binds() {
        let state = test_state();
        for (codes, target) in [
            (vec![], HotkeyTarget::PushToTalk),
            (vec![0x300], HotkeyTarget::PushToTalk),
            (vec![29], HotkeyTarget::SelectVoice(String::new())),
        ] {
            let (reply, _) = state.handle(&Action::AddHotkey {
                codes,
                target,
                enabled: true,
            });
            assert!(!reply.ok, "invalid bind must be rejected");
        }
        // Nothing persisted from rejected adds.
        assert!(!state.config_path.exists());
        let (reply, _) = state.handle(&Action::ListHotkeys);
        assert_eq!(reply.data.unwrap()["hotkeys"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn persisted_binds_reload_like_a_reboot() {
        let state = test_state();
        add_bind(&state, &[29], HotkeyTarget::PushToTalk);
        add_bind(&state, &[56], HotkeyTarget::ToggleMute);
        // Simulate a reboot: fresh manager from the same file.
        let stored = load_daemon_config_file(&state.config_path).expect("reload");
        assert_eq!(stored.hotkeys.len(), 2);
        let mut mgr = HotkeyManager::new();
        mgr.import(stored.hotkeys);
        let listed = mgr.list();
        assert_eq!(listed[0].0, 0);
        assert_eq!(listed[1].0, 1);
        assert!(matches!(listed[1].1.target, HotkeyTarget::ToggleMute));
        // Ids keep growing past the max (no reuse after reboot).
        let next = mgr.add(vm_control::Keybind {
            codes: vec![57],
            target: HotkeyTarget::StopAllSounds,
            enabled: true,
        });
        assert_eq!(next, 2);
    }

    #[test]
    fn corrupt_config_is_quarantined_not_eaten() {
        let state = test_state();
        std::fs::write(&state.config_path, "{oops").unwrap();
        let err = load_daemon_config_file(&state.config_path).expect_err("must fail");
        assert!(err.contains(".bak"), "{err}");
        assert!(!state.config_path.exists(), "corrupt file moved away");
        assert!(state.config_path.with_extension("json.bak").exists());
    }

    #[test]
    fn set_muted_zeroes_output_and_emits_event() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "example-drive".into() });
        assert!(reply.ok);
        let run_block = |muted: bool| {
            state.muted.store(muted, Ordering::Relaxed);
            let mut proc = GraphProcessor::for_state(&state);
            let input = vec![vec![0.4f32; 64]; 2];
            let mut out = vec![vec![9.9f32; 64]; 2];
            let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
            out
        };
        // Via the API action (event for StreamDeck-style clients).
        let (reply, evts) = state.handle(&Action::SetMuted { muted: true });
        assert!(reply.ok);
        assert!(matches!(evts[0], ControlEvent::MutedChanged { muted: true }));
        let out = run_block(true);
        assert!(out.iter().all(|lane| lane.iter().all(|s| *s == 0.0)), "muted must be bit-exact silence");
        let (reply, evts) = state.handle(&Action::SetMuted { muted: false });
        assert!(reply.ok);
        assert!(matches!(evts[0], ControlEvent::MutedChanged { muted: false }));
        let out = run_block(false);
        assert!(out[0].iter().any(|s| *s != 0.0), "unmuted chain passes signal");
    }

    #[test]
    fn hotkey_edge_mapping() {
        let state = test_state();
        // Momentary: both edges map.
        assert_eq!(
            hotkey_action(&HotkeyTarget::PushToTalk, true, &state),
            Some(Action::ToggleVoiceChanger { enabled: true })
        );
        assert_eq!(
            hotkey_action(&HotkeyTarget::PushToTalk, false, &state),
            Some(Action::ToggleVoiceChanger { enabled: false })
        );
        assert_eq!(
            hotkey_action(&HotkeyTarget::PushToMute, true, &state),
            Some(Action::SetMuted { muted: true })
        );
        assert_eq!(
            hotkey_action(&HotkeyTarget::PushToMute, false, &state),
            Some(Action::SetMuted { muted: false })
        );
        // Toggles fire on press only, flipping live state.
        assert_eq!(
            hotkey_action(&HotkeyTarget::ToggleMute, true, &state),
            Some(Action::SetMuted { muted: true })
        );
        assert_eq!(hotkey_action(&HotkeyTarget::ToggleMute, false, &state), None);
        assert!(hotkey_action(&HotkeyTarget::ToggleVoiceChanger, false, &state).is_none());
        assert!(hotkey_action(
            &HotkeyTarget::PlaySound("x".into()),
            false,
            &state
        )
        .is_none());
        assert_eq!(
            hotkey_action(&HotkeyTarget::StopAllSounds, true, &state),
            Some(Action::StopAllSounds)
        );
        // Monitor toggle (F-0.4.2): flips the live enabled flag.
        assert_eq!(
            hotkey_action(&HotkeyTarget::ToggleMonitor, true, &state),
            Some(Action::SetMonitor {
                enabled: Some(true),
                device: None,
                mix: None,
                volume_db: None,
                muted: None,
            })
        );
        assert_eq!(hotkey_action(&HotkeyTarget::ToggleMonitor, false, &state), None);
        let (reply, _) = state.handle(&Action::SetMonitor {
            enabled: Some(true),
            device: None,
            mix: None,
            volume_db: None,
            muted: None,
        });
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(
            hotkey_action(&HotkeyTarget::ToggleMonitor, true, &state),
            Some(Action::SetMonitor {
                enabled: Some(false),
                device: None,
                mix: None,
                volume_db: None,
                muted: None,
            })
        );
    }
}

#[cfg(test)]
mod osc_chain_tests {
    use super::*;

    fn freq_of(x: &[f32]) -> f32 {
        let mut crossings = 0;
        for w in x.windows(2) {
            if (w[0] <= 0.0) != (w[1] <= 0.0) {
                crossings += 1;
            }
        }
        crossings as f32 * 48_000.0 / (2.0 * x.len() as f32)
    }

    #[test]
    fn osc_preset_through_graph_processor() {
        let state = test_state();
        // Write an osc-test preset into the temp preset dir.
        let preset = vm_preset::Preset {
            name: "osc-test".into(),
            description: None,
            rate: 48_000,
            nodes: vec![
                vm_preset::NodeSpec {
                    id: "gen".into(),
                    r#type: "oscillator".into(),
                    params: [(
                        "freq_hz".to_string(),
                        vm_preset::ParamSpec {
                            value: vm_preset::JsonParamValue::Float(440.0),
                            range: None,
                            public: true,
                            modulation: None,
                        },
                    )]
                    .into_iter()
                    .collect(),
                },
                vm_preset::NodeSpec {
                    id: "out".into(),
                    r#type: "gain".into(),
                    params: [(
                        "gain".to_string(),
                        vm_preset::ParamSpec {
                            value: vm_preset::JsonParamValue::Float(0.5),
                            range: None,
                            public: true,
                            modulation: None,
                        },
                    )]
                    .into_iter()
                    .collect(),
                },
            ],
            edges: vec![vm_preset::EdgeSpec { from: "gen".into(), from_ch: 0, to: "out".into(), to_ch: 0 }],
            public_params: vec![],
        };
        state.apply_preset(&preset).expect("apply");
        let mut proc = GraphProcessor::for_state(&state);
        // Run 1 s through the processor in 960-frame blocks.
        let mut out_all = Vec::new();
        for _ in 0..50 {
            let input = vec![vec![0.0f32; 960]; 2];
            let mut out = vec![vec![0.0f32; 960]; 2];
            let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
            out_all.extend_from_slice(&out[0]);
        }
        let steady = &out_all[48_000 / 2..];
        let f = freq_of(steady);
        let peak = steady.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!((f - 440.0).abs() < 440.0 * 0.05, "got {f:.1} Hz peak {peak:.3}");
        assert!((peak - 0.5).abs() < 0.05, "peak {peak:.3}, want 0.5");
    }

    #[test]
    fn seed_builtin_presets_writes_and_preserves_edits() {
        let dir = std::env::temp_dir().join(format!("vm-seed-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        seed_builtin_presets(&dir).unwrap();
        let path = dir.join("mic-npr-voice.json");
        assert!(path.exists(), "builtin must be seeded on first boot");
        let embedded = vm_preset::BUILTIN_PRESETS
            .iter()
            .find(|(s, _)| *s == "mic-npr-voice")
            .unwrap()
            .1;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), embedded);

        // A second boot must NOT clobber the user's edits.
        std::fs::write(&path, "user edit").unwrap();
        seed_builtin_presets(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user edit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn select_mic_npr_voice_processes_noise_end_to_end() {
        let state = test_state();
        seed_builtin_presets(&state.preset_dir).unwrap();
        let (reply, evts) = state.handle(&Action::SelectVoice { voice: "mic-npr-voice".into() });
        assert!(reply.ok, "{:?}", reply.error);
        assert!(matches!(evts[0], ControlEvent::VoiceChanged { .. }));
        assert_eq!(*state.voice_name.lock().unwrap(), "mic-npr-voice");
        assert_eq!(state.node_ids.lock().unwrap().len(), 10, "full chain must be wired");

        // Deterministic white noise (LCG), ±amp.
        let mut st = 0x2545_f491_4f6c_dd1du64;
        let mut noise = Vec::with_capacity(96_000);
        for _ in 0..96_000 {
            st = st.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            noise.push((((st >> 32) as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.1);
        }
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let mut proc = GraphProcessor::for_state(&state);
        let mut out_all = Vec::new();
        for chunk in noise.as_chunks::<960>().0 {
            let input = [chunk.to_vec(), chunk.to_vec()];
            let mut out = vec![vec![0.0f32; chunk.len()]; 2];
            let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
            out_all.extend_from_slice(&out[0]);
        }
        assert!(out_all.iter().all(|s| s.is_finite()), "chain output must be finite");
        // Steady state (after the denoiser's ~1 s warmup + 21 ms frame):
        // stationary noise must come out quieter than it went in.
        let in_r = rms(&noise[72_000..]);
        let out_r = rms(&out_all[72_000..]);
        eprintln!("stationary: in {in_r:.5}, out {out_r:.5}, ratio {r:.3}", r = out_r / in_r);
        assert!(
            out_r < in_r * 0.5,
            "mic chain must crush stationary noise: in {in_r:.4}, out {out_r:.4}"
        );
        assert!(out_r > in_r * 0.001, "output must not be muted: {out_r:.5}");

        // Nasal breathing: lowpassed-noise puffs (-30 dB peaks) over a
        // -55 dB room bed. Pauses must go near-silent; puffs must come
        // out weaker than they went in.
        let mut st2 = 0x9e37_79b9_7f4a_7c15u64;
        let mut lp = 0.0f32;
        let mut breath = Vec::with_capacity(288_000);
        for i in 0..288_000 {
            st2 = st2.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let w = ((st2 >> 32) as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0;
            lp += 0.04 * (w - lp); // heavy lowpass: breath-like rumble
            let t = (i as f32 / 48_000.0) % 2.5;
            let puff = if t < 0.5 { (t / 0.5 * std::f32::consts::PI).sin() } else { 0.0 };
            breath.push(lp * (0.004 + 0.03 * puff));
        }
        let mut out_b = Vec::with_capacity(288_000);
        for chunk in breath.as_chunks::<960>().0 {
            let input = [chunk.to_vec(), chunk.to_vec()];
            let mut out = vec![vec![0.0f32; chunk.len()]; 2];
            let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
            out_b.extend_from_slice(&out[0]);
        }
        // Pause segment (room bed only, floor learned, gate settled):
        // 3.5–4.5 s. Puff #3 at 5.0–5.5 s (fully warmed floor).
        let bed_in = rms(&breath[168_000..216_000]);
        let bed_out = rms(&out_b[168_000..216_000]);
        let puff_in = rms(&breath[240_000..264_000]);
        let puff_out = rms(&out_b[240_000..264_000]);
        eprintln!(
            "breath: bed in {bed_in:.5} out {bed_out:.5} (x{rb:.3}); puff in {puff_in:.5} out {puff_out:.5} (x{rp:.3})",
            rb = bed_out / bed_in,
            rp = puff_out / puff_in
        );
        assert!(bed_out < bed_in * 0.1, "pauses must go near-silent");
        assert!(puff_out < puff_in * 0.5, "breath puffs must be crushed");

        // Speech preservation: loud low-band complex tone after warmup.
        let speech: Vec<f32> = (0..96_000)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                0.25 * (2.0 * std::f32::consts::PI * 150.0 * t).sin()
                    + 0.15 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
            })
            .collect();
        let mut out_s = Vec::with_capacity(96_000);
        for chunk in speech.as_chunks::<960>().0 {
            let input = [chunk.to_vec(), chunk.to_vec()];
            let mut out = vec![vec![0.0f32; chunk.len()]; 2];
            let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
            out_s.extend_from_slice(&out[0]);
        }
        let sp_in = rms(&speech[48_000..]);
        let sp_out = rms(&out_s[48_000..]);
        eprintln!("speech: in {sp_in:.4}, out {sp_out:.4}, ratio {r:.3}", r = sp_out / sp_in);
        assert!(sp_out > sp_in * 0.5, "speech must survive the chain");
    }
}

#[cfg(test)]
mod widget_matrix_tests {
    use super::*;

    const SHOWCASE: &str = r#"{
        "name": "widget-showcase",
        "description": "demo",
        "rate": 48000,
        "nodes": [
            {"id": "gate", "type": "mute", "params": {"muted": {"value": false, "public": true}}},
            {"id": "echo", "type": "delay", "params": {
                "delay_ms": {"value": 250, "range": [0.0, 2000.0, 250.0], "public": true},
                "feedback": {"value": 0.3, "range": [0.0, 0.95, 0.3], "public": true},
                "mix": {"value": 0.5, "range": [0.0, 1.0, 0.5], "public": true}
            }},
            {"id": "drive", "type": "gain", "params": {
                "gain": {"value": 1.5, "range": [0.0, 4.0, 1.0], "public": true},
                "note": {"value": "hi", "public": true}
            }}
        ],
        "edges": [
            {"from": "gate", "from_ch": 0, "to": "echo", "to_ch": 0},
            {"from": "echo", "from_ch": 0, "to": "drive", "to_ch": 0}
        ],
        "public_params": ["gate.muted", "echo.delay_ms", "drive.gain", "drive.note"]
    }"#;

    #[test]
    fn full_widget_matrix_bool_int_text_float() {
        let state = test_state();
        std::fs::write(state.preset_dir.join("widget-showcase.json"), SHOWCASE).unwrap();
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "widget-showcase".into() });
        assert!(reply.ok, "{:?}", reply.error);
        // Bool.
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "gate.muted".into(),
            value: serde_json::json!(true),
        });
        assert!(reply.ok, "{:?}", reply.error);
        // Int (JSON integer stays Int through the untagged enum).
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "echo.delay_ms".into(),
            value: serde_json::json!(300),
        });
        assert!(reply.ok, "{:?}", reply.error);
        // Text.
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "drive.note".into(),
            value: serde_json::json!("updated"),
        });
        assert!(reply.ok, "{:?}", reply.error);
        // Float.
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "drive.gain".into(),
            value: serde_json::json!(2.0),
        });
        assert!(reply.ok, "{:?}", reply.error);
        // Wrong types still rejected.
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "drive.note".into(),
            value: serde_json::json!(1.5),
        });
        assert!(!reply.ok);
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    fn learned_state() -> Arc<DaemonState> {
        let state = test_state();
        seed_builtin_presets(&state.preset_dir).unwrap();
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "mic-npr-voice".into() });
        assert!(reply.ok, "{:?}", reply.error);
        // 2 s of room noise through the chain: the floor learns.
        let mut st = 0x2545_f491_4f6c_dd1du64;
        let mut proc = GraphProcessor::for_state(&state);
        for _ in 0..100 {
            let mut chunk = vec![0.0f32; 960];
            for s in chunk.iter_mut() {
                st = st.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                *s = (((st >> 32) as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.05;
            }
            let input = [chunk.clone(), chunk];
            let mut out = vec![vec![0.0f32; 960]; 2];
            let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
        }
        state
    }

    #[test]
    fn capture_writes_profile_and_reload_applies_it() {
        let state = learned_state();
        let (reply, _) = state.handle(&Action::CaptureNoiseProfile { node: "denoise".into() });
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(reply.data.unwrap()["bins"], 513);

        // File: profile present, non-public, everything else intact.
        let path = state.preset_dir.join("mic-npr-voice.json");
        let text = std::fs::read_to_string(&path).unwrap();
        let preset = vm_preset::load(&text).expect("captured file must validate");
        let dn = preset.nodes.iter().find(|n| n.id == "denoise").unwrap();
        let prof = &dn.params["profile"];
        assert!(!prof.public);
        assert!(prof.range.is_none());
        match &prof.value {
            vm_preset::JsonParamValue::FloatArray(bins) => assert_eq!(bins.len(), 513),
            other => panic!("want FloatArray, got {other:?}"),
        }
        assert_eq!(
            dn.params["strength"].value,
            vm_preset::BUILTIN_PRESETS
                .iter()
                .find(|(s, _)| *s == "mic-npr-voice")
                .and_then(|(_, t)| vm_preset::load(t).ok())
                .and_then(|p| p.nodes.into_iter().find(|n| n.id == "denoise"))
                .and_then(|n| n.params.get("strength").map(|s| s.value.clone()))
                .expect("embedded preset carries strength"),
            "unrelated params survive capture"
        );

        // Reload (boot path): the installed floor equals the file.
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "mic-npr-voice".into() });
        assert!(reply.ok, "{:?}", reply.error);
        let ids = state.node_ids.lock().unwrap();
        let mut graph = state.graph.lock().unwrap();
        let n = graph.node_mut(ids["denoise"]).unwrap();
        match n.get_param("profile") {
            Some(vm_dsp::ParamValue::FloatArray(floor)) => {
                assert_eq!(floor.len(), 513);
                match &prof.value {
                    vm_preset::JsonParamValue::FloatArray(bins) => {
                        for (a, b) in floor.iter().zip(bins.iter()) {
                            assert!((a - *b as f32).abs() < 1e-6);
                        }
                    }
                    _ => unreachable!(),
                }
            }
            other => panic!("want installed floor, got {other:?}"),
        }
    }

    #[test]
    fn capture_rejects_unknown_and_non_denoise_nodes() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "example-drive".into() });
        assert!(reply.ok);
        let (reply, _) = state.handle(&Action::CaptureNoiseProfile { node: "ghost".into() });
        assert!(!reply.ok);
        let (reply, _) = state.handle(&Action::CaptureNoiseProfile { node: "drive".into() });
        assert!(!reply.ok, "gain node exposes no profile");
        // No voice loaded at all: node lookup fails first, cleanly.
        let blank = test_state();
        let (reply, _) =
            blank.handle(&Action::CaptureNoiseProfile { node: "denoise".into() });
        assert!(!reply.ok);
    }

    #[test]
    fn frame_switch_updates_health_latency() {
        let state = learned_state();
        let lat_of = |state: &Arc<DaemonState>| {
            let (reply, _) = state.handle(&Action::GetHealth);
            assert!(reply.ok);
            reply.data.unwrap()
        };
        let data = lat_of(&state);
        let dn = data["nodes"].as_array().unwrap().iter().find(|n| n["node"] == "denoise").unwrap().clone();
        assert!((dn["latMs"].as_f64().unwrap() - 1024.0 * 1000.0 / 48_000.0).abs() < 1e-9);
        assert!((data["latencyMs"].as_f64().unwrap() - dn["latMs"].as_f64().unwrap()).abs() < 1e-9,
            "mic chain budget is the denoise frame alone");
        // Live switch over the API (JSON numbers arrive as Float).
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "denoise.frame".into(),
            value: serde_json::json!(512),
        });
        assert!(reply.ok, "{:?}", reply.error);
        let data = lat_of(&state);
        let dn = data["nodes"].as_array().unwrap().iter().find(|n| n["node"] == "denoise").unwrap().clone();
        assert!((dn["latMs"].as_f64().unwrap() - 512.0 * 1000.0 / 48_000.0).abs() < 1e-9);
        assert!((data["latencyMs"].as_f64().unwrap() - dn["latMs"].as_f64().unwrap()).abs() < 1e-9);
        // Capture at 512 stores 257 bins; reload applies them.
        let (reply, _) = state.handle(&Action::CaptureNoiseProfile { node: "denoise".into() });
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(reply.data.unwrap()["bins"], 257);
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "mic-npr-voice".into() });
        assert!(reply.ok, "{:?}", reply.error);
        // Middle values stay rejected (never a half-built STFT).
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "denoise.frame".into(),
            value: serde_json::json!(777),
        });
        assert!(!reply.ok);
    }

    #[test]
    fn full_chain_512_attenuates_without_blowing_up() {
        let state = learned_state();
        let (reply, _) = state.handle(&Action::SetVoiceParameter {
            param: "denoise.frame".into(),
            value: serde_json::json!(512),
        });
        assert!(reply.ok);
        // Fresh noise (unseen room) through the 512 chain.
        let mut st = 0x9e37_79b9_7f4a_7c15u64;
        let mut proc = GraphProcessor::for_state(&state);
        let mut out_all = Vec::new();
        let mut noise = Vec::with_capacity(96_000);
        for _ in 0..96_000 {
            st = st.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            noise.push((((st >> 32) as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.1);
        }
        for chunk in noise.as_chunks::<960>().0 {
            let input = [chunk.to_vec(), chunk.to_vec()];
            let mut out = vec![vec![0.0f32; chunk.len()]; 2];
            let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
            out_all.extend_from_slice(&out[0]);
        }
        assert!(out_all.iter().all(|s| s.is_finite()));
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let in_r = rms(&noise[72_000..]);
        let out_r = rms(&out_all[72_000..]);
        assert!(
            out_r < in_r * 0.9,
            "512 chain must still suppress stationary noise: in {in_r:.4}, out {out_r:.4}"
        );
        assert_eq!(state.graph_xruns.load(Ordering::Relaxed), 0, "no lock contention");
    }
}

#[cfg(test)]
mod device_profile_tests {
    use super::*;

    #[test]
    fn match_rules_handle_node_ids() {
        let fifine = "alsa_input.usb-MV-SILICON_fifine_Microphone_20190808-00.mono-fallback#52";
        let base = "alsa_input.usb-MV-SILICON_fifine_Microphone_20190808-00.mono-fallback";
        assert_eq!(strip_node_id(fifine), base);
        assert_eq!(strip_node_id(base), base, "no suffix: untouched");
        assert_eq!(strip_node_id("weird#name"), "weird#name", "non-numeric suffix kept");
        assert!(device_matches(fifine, fifine, "Fifine Microphone"));
        assert!(device_matches(base, fifine, "Fifine Microphone"), "base survives reboot renumbering");
        assert!(device_matches("Fifine Microphone", fifine, "Fifine Microphone"));
        assert!(!device_matches(base, "stub:input:0", "Stub Microphone"));
        assert!(!device_matches("", fifine, "Fifine Microphone"));
    }

    #[test]
    fn profile_crud_persists_v4() {
        let state = test_state();
        // Empty values rejected.
        let (reply, _) = state.handle(&Action::SetDeviceProfile {
            device: "".into(),
            voice: "example-drive".into(),
        });
        assert!(!reply.ok);
        let (reply, _) = state.handle(&Action::SetDeviceProfile {
            device: "stub:input:0".into(),
            voice: "example-drive".into(),
        });
        assert!(reply.ok, "{:?}", reply.error);
        let (reply, _) = state.handle(&Action::ListDeviceProfiles);
        assert!(reply.ok);
        assert_eq!(
            reply.data.unwrap()["profiles"],
            serde_json::json!([{"device": "stub:input:0", "voice": "example-drive"}])
        );
        // File is schema v4 with the profile + integration sections.
        let text = std::fs::read_to_string(&state.config_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], 5);
        assert_eq!(v["device_profiles"]["stub:input:0"], "example-drive");
        assert_eq!(v["ducking"]["enabled"], true, "ducking defaults on");
        assert_eq!(v["replay"]["enabled"], true, "replay defaults on");
        assert_eq!(v["replay"]["seconds"], 30);
        assert_eq!(v["passthrough"]["enabled"], false, "passthrough defaults off");
        // Reload survives a reboot.
        let cfg = load_daemon_config_file(&state.config_path).expect("reload");
        assert_eq!(cfg.device_profiles["stub:input:0"], "example-drive");
        // Remove unknown errors; remove known empties.
        let (reply, _) =
            state.handle(&Action::RemoveDeviceProfile { device: "ghost".into() });
        assert!(!reply.ok);
        let (reply, _) =
            state.handle(&Action::RemoveDeviceProfile { device: "stub:input:0".into() });
        assert!(reply.ok, "{:?}", reply.error);
        let (reply, _) = state.handle(&Action::ListDeviceProfiles);
        assert_eq!(reply.data.unwrap()["profiles"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn autoswitch_selects_once_and_never_fights() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SetDeviceProfile {
            device: "stub:input:0".into(),
            voice: "example-drive".into(),
        });
        assert!(reply.ok);
        // Voice starts empty → switch fires with an event.
        let evts = state.autoswitch_for_device("stub:input:0", "Stub Microphone");
        assert!(matches!(evts, Some(ref e) if matches!(e[0], ControlEvent::VoiceChanged { .. })));
        assert_eq!(*state.voice_name.lock().unwrap(), "example-drive");
        // Same voice again → no-op (hotplug polls can't loop).
        assert!(state.autoswitch_for_device("stub:input:0", "Stub Microphone").is_none());
        // Unbound device → nothing.
        assert!(state.autoswitch_for_device("other:9", "Other").is_none());
        // Bound-but-missing voice file → clean None, voice untouched.
        let (reply, _) = state.handle(&Action::SetDeviceProfile {
            device: "other:9".into(),
            voice: "ghost-voice".into(),
        });
        assert!(reply.ok);
        assert!(state.autoswitch_for_device("other:9", "Other").is_none());
        assert_eq!(*state.voice_name.lock().unwrap(), "example-drive");
    }

    #[test]
    fn select_input_autoswitches_bound_device() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SetDeviceProfile {
            device: "stub:input:0".into(),
            voice: "example-drive".into(),
        });
        assert!(reply.ok);
        let topo = vm_audio::StreamTopology {
            input: vm_audio::DeviceUid::new("stub:input:0"),
            virtual_out: vm_audio::DeviceUid::new("stub:virtual:vm-mic"),
            monitor: None,
        };
        {
            let mut audio = state.audio.lock().unwrap();
            audio.enumerate().unwrap();
            audio.open(&topo).unwrap();
            audio.start().unwrap();
        }
        *state.topology.lock().unwrap() = Some(topo);
        let (reply, evts) = state.handle(&Action::SelectInput { uid: "stub:input:0".into() });
        assert!(reply.ok, "{:?}", reply.error);
        assert!(matches!(evts[0], ControlEvent::DevicesChanged));
        assert!(
            matches!(evts.get(1), Some(ControlEvent::VoiceChanged { .. })),
            "bound input must also switch the voice: {evts:?}"
        );
        assert_eq!(*state.voice_name.lock().unwrap(), "example-drive");
    }
}

#[cfg(test)]
mod f04a_node_tests {
    use super::*;

    /// Guard against catalogue drift: every node type the preset
    /// validator accepts must be constructible by the daemon's factory
    /// (deepfilter fails exactly when the `neural` feature is off).
    #[test]
    fn every_known_node_type_maps_to_a_factory_constructor() {
        let f = vm_dsp::factory::mvp_factory();
        let ctx = NodeConstructorContext { rate: 48_000, max_channels: 2 };
        for name in vm_preset::KNOWN_NODE_TYPES {
            match f.create(name, &ctx) {
                Ok(_) => {}
                Err(e) if *name == "deepfilter" => {
                    assert!(
                        e.contains("'neural' cargo feature"),
                        "deepfilter must fail only when the feature is missing: {e}"
                    );
                }
                Err(e) => {
                    panic!("preset accepts '{name}' but the factory cannot build it: {e}")
                }
            }
        }
    }

    /// Minimal mono PCM16 48 kHz WAV writer (test-local).
    fn write_wav16(path: &std::path::Path, samples: &[f32]) {
        let data_len = samples.len() * 2;
        let mut b = Vec::with_capacity(44 + data_len);
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&1u16.to_le_bytes()); // mono
        b.extend_from_slice(&48_000u32.to_le_bytes());
        b.extend_from_slice(&(48_000u32 * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data_len as u32).to_le_bytes());
        for s in samples {
            b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    /// Deterministic white noise (LCG), same generator as the other E2E tests.
    fn noise(len: usize, amp: f32) -> Vec<f32> {
        let mut st = 0x2545_f491_4f6c_dd1du64;
        (0..len)
            .map(|_| {
                st = st
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (((st >> 32) as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0) * amp
            })
            .collect()
    }

    fn sine(freq: f32, len: usize, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn process(state: &Arc<DaemonState>, input: &[f32]) -> Vec<f32> {
        let mut proc = GraphProcessor::for_state(state);
        let mut out_all = Vec::new();
        for chunk in input.as_chunks::<960>().0 {
            let cin = [chunk.to_vec(), chunk.to_vec()];
            let mut out = vec![vec![0.0f32; chunk.len()]; 2];
            let ins: Vec<&[f32]> = cin.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
            out_all.extend_from_slice(&out[0]);
        }
        out_all
    }

    /// F-0.4.A end-to-end: presets with the new node types (Text/Int
    /// params included) load through the daemon, wire one node, and
    /// process audio with the expected gross behavior.
    #[test]
    fn f04a_presets_load_and_process() {
        let state = test_state();
        let write = |name: &str, json: &str| {
            std::fs::write(state.preset_dir.join(format!("{name}.json")), json).unwrap();
        };
        let select = |name: &str| {
            let (reply, _) = state.handle(&Action::SelectVoice { voice: name.into() });
            assert!(reply.ok, "{name}: {:?}", reply.error);
            assert_eq!(state.node_ids.lock().unwrap().len(), 1, "{name} wires one node");
        };

        // autogain alone: -40 dBFS tone must come out audibly lifted.
        write(
            "f04a-ag",
            r#"{"name":"f04a-ag","nodes":[{"id":"ag","type":"autogain","params":{
               "target_lufs":{"value":-23.0},"max_gain_db":{"value":20.0},
               "adapt_ms":{"value":400.0}}}],"edges":[]}"#,
        );
        select("f04a-ag");
        let quiet = sine(440.0, 96_000, 0.01);
        let out = process(&state, &quiet);
        assert!(out.iter().all(|s| s.is_finite()));
        let (in_r, out_r) = (rms(&quiet[72_000..]), rms(&out[72_000..]));
        assert!(out_r > in_r * 2.0, "autogain must lift: in {in_r:.4}, out {out_r:.4}");
        assert!(out_r < in_r * 12.0, "lift stays capped (max_gain_db)");

        // autotune alone: detuned tone survives the vocoder (pitch
        // correctness is unit-tested; here the daemon path is on trial).
        write(
            "f04a-at",
            r#"{"name":"f04a-at","nodes":[{"id":"at","type":"autotune","params":{
               "key":{"value":9},"scale":{"value":"chromatic"},"mix":{"value":1.0},
               "speed_ms":{"value":40.0}}}],"edges":[]}"#,
        );
        select("f04a-at");
        let detuned = sine(216.2, 96_000, 0.4); // ~30 cents flat of A3
        let out = process(&state, &detuned);
        assert!(out.iter().all(|s| s.is_finite()));
        let (in_r, out_r) = (rms(&detuned[48_000..]), rms(&out[48_000..]));
        assert!(out_r > in_r * 0.2, "autotune must not destroy the voice: out {out_r:.4}");

        // rnnoise alone: pink room noise crushed (weights are embedded,
        // no files, no feature — this is the always-available neural).
        write(
            "f04a-rn",
            r#"{"name":"f04a-rn","nodes":[{"id":"rn","type":"rnnoise",
               "params":{"mix":{"value":1.0}}}],"edges":[]}"#,
        );
        select("f04a-rn");
        let mut lp = 0.0f32;
        let pink: Vec<f32> = noise(96_000, 0.06)
            .into_iter()
            .map(|w| {
                lp += 0.08 * (w - lp);
                lp
            })
            .collect();
        let out = process(&state, &pink);
        assert!(out.iter().all(|s| s.is_finite()));
        let (in_r, out_r) = (rms(&pink[48_000..]), rms(&out[48_000..]));
        assert!(out_r < in_r * 0.3, "rnnoise must crush room noise: in {in_r:.4}, out {out_r:.4}");

        // convolver with a real WAV IR (peak at 0, taps at 100 ms and
        // 1.0 s): a 300 ms noise burst must reappear at the 1 s tap.
        let mut ir = vec![0.0f32; 52_800];
        ir[0] = 1.0;
        ir[4_800] = 0.6;
        ir[48_000] = 0.5;
        let ir_path = state.preset_dir.join("f04a-ir.wav");
        write_wav16(&ir_path, &ir);
        write(
            "f04a-cv",
            &format!(
                r#"{{"name":"f04a-cv","nodes":[{{"id":"cv","type":"convolver","params":{{
                   "ir_path":{{"value":"{}"}},"mix":{{"value":1.0}}}}}}],"edges":[]}}"#,
                ir_path.display()
            ),
        );
        select("f04a-cv");
        let mut burst = noise(14_400, 0.3);
        burst.resize(96_000, 0.0);
        let out = process(&state, &burst);
        assert!(out.iter().all(|s| s.is_finite()));
        let echo = rms(&out[49_000..62_000]);
        // All taps of the 100 ms echo (burst end + 4.8k tap + partition
        // latency) land by sample ~20_224; from 24_000 up to the 1 s tap
        // the convolution is exactly zero.
        let gap = rms(&out[24_000..40_000]);
        assert!(echo > 0.01, "IR tap at 1 s must echo the burst: {echo:.5}");
        assert!(gap < echo * 0.05, "no content where the IR has no taps: {gap:.5}");

        // deepfilter preset without the feature (or without a usable
        // model): selectVoice must fail loud — never a silent mic.
        write(
            "f04a-df",
            r#"{"name":"f04a-df","nodes":[{"id":"df","type":"deepfilter","params":{
               "model_path":{"value":"/nonexistent-df3"}}}],"edges":[]}"#,
        );
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "f04a-df".into() });
        assert!(!reply.ok, "deepfilter without model/feature must fail");
        let err = reply.error.unwrap_or_default();
        assert!(
            err.contains("'neural' cargo feature") || err.contains("model_path"),
            "clear failure wanted, got: {err}"
        );
        // The failed select must not have clobbered the live voice.
        assert_eq!(*state.voice_name.lock().unwrap(), "f04a-cv");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};

    /// Full bidirectional loop over a real socket with the real daemon
    /// handler: action in, reply + broadcast event out. This is the exact
    /// flow a Stream Deck / Companion module drives.
    #[tokio::test]
    async fn websocket_control_and_feedback_loop() {
        let state = test_state();
        let server = ControlApiServer::new(
            ServerConfig {
                bind: "127.0.0.1".parse().unwrap(),
                ports: vec![18780, 18781, 18782],
                require_auth: false,
                expected_token: None,
            },
            state.clone(),
        );
        let port = server.serve().await.expect("serve");
        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
                .await
                .expect("connect");

        async fn roundtrip(
            ws: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            text: &str,
            id: u64,
        ) -> (serde_json::Value, Vec<serde_json::Value>) {
            ws.send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                .await
                .expect("send");
            let mut evts = Vec::new();
            let reply = loop {
                let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
                    .await
                    .expect("reply timeout")
                    .expect("stream")
                    .expect("msg");
                let v: serde_json::Value =
                    serde_json::from_str(msg.to_text().unwrap()).unwrap();
                if v.get("event").is_some() {
                    evts.push(v);
                    continue;
                }
                assert_eq!(v["id"], id, "reply echoes the request id");
                break v;
            };
            // Broadcasts fan out through a channel, so they land after
            // the direct reply: drain everything still in flight.
            while let Ok(Some(Ok(msg))) =
                tokio::time::timeout(std::time::Duration::from_millis(500), ws.next()).await
            {
                let v: serde_json::Value =
                    serde_json::from_str(msg.to_text().unwrap()).unwrap();
                if v.get("event").is_some() {
                    evts.push(v);
                }
            }
            (reply, evts)
        }

        // selectVoice -> ok + voiceChanged broadcast.
        let (reply, evts) = roundtrip(
            &mut ws,
            r#"{"action":"selectVoice","voice":"example-drive","id":1}"#,
            1,
        )
        .await;
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0]["event"], "voiceChanged");
        assert_eq!(evts[0]["voice"], "example-drive");

        // setMuted -> ok + mutedChanged broadcast (button lights up).
        let (reply, evts) = roundtrip(&mut ws, r#"{"action":"setMuted","muted":true,"id":2}"#, 2).await;
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0]["event"], "mutedChanged");
        assert_eq!(evts[0]["muted"], true);

        // toggleVoiceChanger -> ok + voiceChangerToggled broadcast.
        let (reply, evts) = roundtrip(
            &mut ws,
            r#"{"action":"toggleVoiceChanger","enabled":false,"id":3}"#,
            3,
        )
        .await;
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0]["event"], "voiceChangerToggled");

        // getHealth reflects the muted/toggled state (button init).
        let (reply, _) = roundtrip(&mut ws, r#"{"action":"getHealth","id":4}"#, 4).await;
        assert_eq!(reply["data"]["muted"], true);
        assert_eq!(reply["data"]["enabled"], false);
        assert_eq!(reply["data"]["voice"], "example-drive");

        // getVoices lists the seeded preset.
        let (reply, _) = roundtrip(&mut ws, r#"{"action":"getVoices","id":5}"#, 5).await;
        assert_eq!(reply["data"]["voices"], serde_json::json!(["example-drive"]));

        ws.close(None).await.ok();
    }
}

#[cfg(test)]
mod persona_tests {
    use super::*;

    fn render(state: &Arc<DaemonState>, input: &[f32]) -> Vec<f32> {
        let mut proc = GraphProcessor::for_state(state);
        let mut out_all = Vec::with_capacity(input.len());
        for chunk in input.chunks(960) {
            let ch = chunk.to_vec();
            let input = [ch.clone(), ch];
            let mut out = vec![vec![0.0f32; chunk.len()]; 2];
            let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
            proc.process(48_000, &ins, &mut outs);
            out_all.extend_from_slice(&out[0]);
        }
        out_all
    }

    fn sine(freq: f32, len: usize, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin())
            .collect()
    }

    fn freq_of(x: &[f32]) -> f32 {
        let mut crossings = 0;
        for w in x.windows(2) {
            if (w[0] <= 0.0) != (w[1] <= 0.0) {
                crossings += 1;
            }
        }
        crossings as f32 * 48_000.0 / (2.0 * x.len() as f32)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn select_persona(state: &Arc<DaemonState>, voice: &str) {
        seed_builtin_presets(&state.preset_dir).unwrap();
        let (reply, _) = state.handle(&Action::SelectVoice { voice: voice.into() });
        assert!(reply.ok, "{voice}: {:?}", reply.error);
    }

    #[test]
    fn chipmunk_shifts_up_nine_semitones() {
        let state = test_state();
        select_persona(&state, "chipmunk");
        let out = render(&state, &sine(440.0, 144_000, 0.5));
        assert!(out.iter().all(|s| s.is_finite()));
        // Past the 2048-sample STFT + startup: 440 * 2^(9/12) ≈ 740 Hz.
        let f = freq_of(&out[96_000..]);
        assert!((f - 740.0).abs() < 740.0 * 0.05, "chipmunk f = {f:.1} Hz");
    }

    #[test]
    fn demon_shifts_down_seven_semitones() {
        let state = test_state();
        select_persona(&state, "demon");
        let out = render(&state, &sine(440.0, 144_000, 0.5));
        assert!(out.iter().all(|s| s.is_finite()));
        // 440 * 2^(-7/12) ≈ 293.7 Hz (reverb tail keeps it finite).
        let f = freq_of(&out[96_000..]);
        assert!((f - 293.7).abs() < 293.7 * 0.07, "demon f = {f:.1} Hz");
    }

    #[test]
    fn robot_colors_without_silencing() {
        let state = test_state();
        select_persona(&state, "robot");
        let input = sine(440.0, 96_000, 0.5);
        let out = render(&state, &input);
        assert!(out.iter().all(|s| s.is_finite()));
        let steady = &out[48_000..];
        let in_r = rms(&input[48_000..]);
        let out_r = rms(steady);
        assert!(out_r > in_r * 0.2 && out_r < in_r * 3.0, "robot level {out_r:.3} vs {in_r:.3}");
        let diff = steady
            .iter()
            .zip(input[48_000..].iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, |a, b| a.max(b));
        assert!(diff > 0.05, "robot must audibly color the signal");
    }

    #[test]
    fn radio_and_telephone_are_bands_around_1k() {
        for (voice, stop_lo) in [("radio", 100.0), ("telephone", 100.0)] {
            let state = test_state();
            select_persona(&state, voice);
            // In-band passes, an octave below the HP corner does not.
            let hi = render(&state, &sine(1000.0, 48_000, 0.5));
            let lo = render(&state, &sine(stop_lo, 48_000, 0.5));
            let r_hi = rms(&hi[24_000..]) / 0.5;
            let r_lo = rms(&lo[24_000..]) / 0.5;
            assert!(hi.iter().all(|s| s.is_finite()) && lo.iter().all(|s| s.is_finite()));
            // Tube harmonics of the bass probe leak back in-band, so the
            // bar is directional (1 kHz clearly wins) plus absolute
            // rejection (bass cut to well under half).
            assert!(
                r_hi > r_lo * 1.5,
                "{voice}: 1 kHz ({r_hi:.3}) must pass better than 100 Hz ({r_lo:.3})"
            );
            assert!(r_lo < 0.5, "{voice}: bass must be cut ({r_lo:.3})");
        }
        // Telephone is the narrower of the two: less lows leak through.
        let radio_lo = {
            let state = test_state();
            select_persona(&state, "radio");
            rms(&render(&state, &sine(100.0, 48_000, 0.5))[24_000..])
        };
        let tele_lo = {
            let state = test_state();
            select_persona(&state, "telephone");
            rms(&render(&state, &sine(100.0, 48_000, 0.5))[24_000..])
        };
        assert!(
            tele_lo < radio_lo,
            "telephone ({tele_lo:.4}) must leak less bass than radio ({radio_lo:.4})"
        );
    }
}

#[cfg(test)]
mod f04c_tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        if x.is_empty() {
            return 0.0;
        }
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn run_block(proc: &mut GraphProcessor, input: &[Vec<f32>; 2], out: &mut [Vec<f32>; 2]) {
        let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
        proc.process(48_000, &ins, &mut outs);
    }

    fn add_tone(state: &DaemonState, id: u64, sample: f32, send_to_vad: bool) {
        let mut m = state.mixer.lock().unwrap();
        m.add_sound(vm_soundboard::LoadedSound {
            id: vm_soundboard::SoundHashId(id),
            name: format!("tone{id}"),
            rate: 48_000,
            channels: vec![vec![sample; 256]],
        });
        let opts = vm_soundboard::SoundOptions { send_to_vad, ..Default::default() };
        m.play(vm_soundboard::SoundHashId(id), opts).unwrap();
    }

    #[test]
    fn monitor_set_get_and_persisted() {
        let state = test_state();
        let (reply, evts) = state.handle(&Action::SetMonitor {
            enabled: Some(true),
            device: None,
            mix: Some(0.5),
            volume_db: Some(-6.0),
            muted: None,
        });
        assert!(reply.ok, "{:?}", reply.error);
        match &evts[0] {
            ControlEvent::MonitorChanged { enabled, mix, volume_db, muted, .. } => {
                assert!(*enabled);
                assert!((*mix - 0.5).abs() < 1e-6);
                assert!((*volume_db + 6.0).abs() < 1e-6);
                assert!(!*muted);
            }
            other => panic!("expected MonitorChanged, got {other:?}"),
        }
        let (reply, _) = state.handle(&Action::GetMonitor);
        let d = reply.data.unwrap();
        assert_eq!(d["enabled"], true);
        assert_eq!(d["mix"], 0.5);
        assert_eq!(d["volume_db"], -6.0);
        assert_eq!(d["muted"], false);
        let cfg = load_daemon_config_file(&state.config_path).expect("reload");
        assert!(cfg.monitor.enabled);
        assert_eq!(cfg.monitor.mix, 0.5);
    }

    #[test]
    fn ducking_set_get_and_persisted() {
        let state = test_state();
        let (reply, evts) = state.handle(&Action::SetDucking {
            enabled: Some(false),
            depth_db: Some(18.0),
            threshold_db: None,
            attack_ms: None,
            release_ms: None,
        });
        assert!(reply.ok, "{:?}", reply.error);
        assert!(matches!(evts[0], ControlEvent::DuckingChanged { enabled: false, depth_db: 18.0, .. }));
        let (reply, _) = state.handle(&Action::GetDucking);
        let d = reply.data.unwrap();
        assert_eq!(d["enabled"], false);
        assert_eq!(d["depth_db"], 18.0);
        assert_eq!(d["threshold_db"], -40.0, "untouched field keeps its default");
        let cfg = load_daemon_config_file(&state.config_path).expect("reload");
        assert!(!cfg.ducking.enabled);
        assert_eq!(cfg.ducking.depth_db, 18.0);
    }

    #[test]
    fn monitor_only_bus_carries_non_vad_sounds() {
        let state = test_state();
        state.voice_enabled.store(false, Ordering::Relaxed);
        add_tone(&state, 7, 0.4, false);
        let mut proc = GraphProcessor::for_state(&state);
        let silent = [vec![0.0f32; 64], vec![0.0f32; 64]];
        let mut out = [vec![0.0f32; 64], vec![0.0f32; 64]];
        run_block(&mut proc, &silent, &mut out);
        // Never reaches vm-mic...
        assert!(out[0].iter().all(|s| s.abs() < 1e-6), "monitor-only sound leaked to vm-mic");
        // ...but it is on the hear-myself monitor bus.
        let bus = proc.monitor_bus();
        assert!(bus.len() >= 128, "interleaved stereo bus");
        assert!((bus[0] - 0.4).abs() < 1e-6, "monitor bus: {}", bus[0]);
    }

    #[test]
    fn ducking_attenuates_soundboard_while_speech() {
        let state = test_state();
        state.voice_enabled.store(false, Ordering::Relaxed);
        add_tone(&state, 9, 0.5, false);
        let (reply, _) = state.handle(&Action::SetDucking {
            enabled: Some(true),
            depth_db: Some(12.0),
            threshold_db: Some(-40.0),
            attack_ms: Some(1.0),
            release_ms: Some(1.0),
        });
        assert!(reply.ok, "{:?}", reply.error);
        let mut proc = GraphProcessor::for_state(&state);
        let silent = [vec![0.0f32; 64], vec![0.0f32; 64]];
        let loud = [vec![0.5f32; 64], vec![0.5f32; 64]];
        let mut out = [vec![0.0f32; 64], vec![0.0f32; 64]];
        run_block(&mut proc, &silent, &mut out);
        let unity = rms(&proc.monitor_bus()[..128]);
        assert!(unity > 0.1, "sound must reach the monitor bus un-ducked: {unity}");
        run_block(&mut proc, &loud, &mut out);
        let ducked = rms(&proc.monitor_bus()[..128]);
        assert!(ducked < unity * 0.35, "12 dB duck expected: unity {unity:.4} ducked {ducked:.4}");
        assert!(ducked > unity * 0.1, "not silenced either: {ducked:.4}");
    }

    #[test]
    fn levels_reported_from_rt_processor() {
        let state = test_state();
        let mut proc = GraphProcessor::for_state(&state);
        let input = [vec![0.25f32; 64], vec![0.25f32; 64]];
        let mut out = [vec![0.0f32; 64], vec![0.0f32; 64]];
        run_block(&mut proc, &input, &mut out);
        let (reply, _) = state.handle(&Action::GetLevels);
        assert!(reply.ok, "{:?}", reply.error);
        let d = reply.data.unwrap();
        let mic = d["mic_db"].as_f64().unwrap();
        assert!((mic - (-12.04)).abs() < 0.5, "0.25 amplitude ≈ −12 dBFS, got {mic}");
        assert!(d["out_db"].as_f64().is_some());
        assert!(d["speech"].is_boolean());
    }

    #[test]
    fn obs_scene_profiles_and_status() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SetObs {
            enabled: Some(true),
            url: Some("ws://127.0.0.1:4455".into()),
            password: None,
        });
        assert!(reply.ok, "{:?}", reply.error);
        let (reply, _) = state.handle(&Action::SetObsSceneProfile {
            scene: "Game".into(),
            voice: "robot".into(),
        });
        assert!(reply.ok);
        let (reply, _) = state.handle(&Action::ListObsSceneProfiles);
        let scenes = reply.data.unwrap()["scenes"].as_array().unwrap().clone();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0]["scene"], "Game");
        assert_eq!(scenes[0]["voice"], "robot");
        let (reply, _) = state.handle(&Action::GetObsStatus);
        assert_eq!(reply.data.unwrap()["url"], "ws://127.0.0.1:4455");
        let cfg = load_daemon_config_file(&state.config_path).expect("reload");
        assert!(cfg.obs.enabled);
        assert_eq!(cfg.obs.scenes["Game"], "robot");
        let (reply, _) = state.handle(&Action::RemoveObsSceneProfile { scene: "Game".into() });
        assert!(reply.ok);
        let (reply, _) = state.handle(&Action::ListObsSceneProfiles);
        assert_eq!(reply.data.unwrap()["scenes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn midi_bindings_learn_and_dispatch() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "example-drive".into() });
        assert!(reply.ok, "{:?}", reply.error);
        let (reply, _) = state.handle(&Action::MidiBindAdd {
            channel: Some(0),
            kind: MidiKind::Cc,
            number: 7,
            target: MidiTarget::SetVoiceParameter { param: "drive.gain".into(), min: 0.0, max: 4.0 },
        });
        assert!(reply.ok, "{:?}", reply.error);
        // Invalid number fails closed.
        let (reply, _) = state.handle(&Action::MidiBindAdd {
            channel: None,
            kind: MidiKind::Cc,
            number: 200,
            target: MidiTarget::Trigger(HotkeyTarget::ToggleMute),
        });
        assert!(!reply.ok);
        // Learn: the next event becomes a binding and is broadcast.
        let (reply, _) = state.handle(&Action::MidiLearnStart {
            target: MidiTarget::Trigger(HotkeyTarget::ToggleMute),
        });
        assert!(reply.ok);
        let (publisher, mut sub) = tokio::sync::broadcast::channel(16);
        state.dispatch_midi(
            &MidiEvent { channel: 3, kind: MidiKind::Note, number: 36, value: 100 },
            &publisher,
        );
        let learned = sub.try_recv().expect("MidiLearned broadcast");
        assert!(learned.contains("midiLearned"), "{learned}");
        let (reply, _) = state.handle(&Action::ListMidiBindings);
        let binds = reply.data.unwrap()["bindings"].as_array().unwrap().clone();
        assert_eq!(binds.len(), 2, "manual + learned");
        assert_eq!(binds[1]["number"], 36);
        assert_eq!(binds[1]["channel"], 3);
        // Dispatching the learned note toggles mute.
        state.dispatch_midi(
            &MidiEvent { channel: 3, kind: MidiKind::Note, number: 36, value: 100 },
            &publisher,
        );
        assert!(state.muted.load(Ordering::Relaxed), "note bound to ToggleMute");
        // The CC binding maps 64/127 onto [0, 4] → drive.gain ≈ 2.016.
        state.dispatch_midi(
            &MidiEvent { channel: 0, kind: MidiKind::Cc, number: 7, value: 64 },
            &publisher,
        );
        let ids = state.node_ids.lock().unwrap();
        let id = ids.get("drive").copied().expect("example-drive has a drive node");
        drop(ids);
        let mut graph = state.graph.lock().unwrap();
        let node = graph.node_mut(id).expect("node");
        match node.get_param("gain") {
            Some(vm_dsp::ParamValue::Float(v)) => {
                assert!((v - (64.0 / 127.0 * 4.0)).abs() < 0.02, "gain={v}");
            }
            other => panic!("expected a float gain param, got {other:?}"),
        }
        drop(graph);
        // Remove by id.
        let (reply, _) = state.handle(&Action::MidiBindRemove { midi_id: 0 });
        assert!(reply.ok);
        let (reply, _) = state.handle(&Action::ListMidiBindings);
        assert_eq!(reply.data.unwrap()["bindings"].as_array().unwrap().len(), 1);
        let (reply, _) = state.handle(&Action::GetMidiStatus);
        assert!(reply.ok);
        assert_eq!(reply.data.unwrap()["bindings"], 1);
    }
}

#[cfg(test)]
mod f04b_tests {
    use super::*;

    fn run_block(proc: &mut GraphProcessor, input: &[Vec<f32>; 2], out: &mut [Vec<f32>; 2]) {
        let ins: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
        proc.process(48_000, &ins, &mut outs);
    }

    #[test]
    fn replay_set_get_clamps_and_persists() {
        let state = test_state();
        // Default: always-on, 30 s (the feature "just works").
        let (reply, _) = state.handle(&Action::GetInstantReplay);
        let d = reply.data.unwrap();
        assert_eq!(d["enabled"], true);
        assert_eq!(d["seconds"], 30);
        assert_eq!(d["buffered_seconds"], 0.0, "fresh ring is empty");

        let (reply, _) = state.handle(&Action::SetInstantReplay { enabled: None, seconds: Some(1) });
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(reply.data.unwrap()["seconds"], 5, "clamped to 5..=120");

        let cfg = load_daemon_config_file(&state.config_path).expect("reload");
        assert!(cfg.replay.enabled);
        assert_eq!(cfg.replay.seconds, 5);
    }

    #[test]
    fn replay_save_writes_wav_when_fed() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SetInstantReplay { enabled: Some(true), seconds: Some(5) });
        assert!(reply.ok);
        // Streaming path: dry passthrough makes the output equal the input.
        state.voice_enabled.store(false, Ordering::Relaxed);
        let mut proc = GraphProcessor::for_state(&state);
        let input = [vec![0.25f32; 960], vec![-0.25f32; 960]];
        let mut out = [vec![0.0f32; 960], vec![0.0f32; 960]];
        run_block(&mut proc, &input, &mut out);

        let (reply, _) = state.handle(&Action::GetInstantReplay);
        let buffered = reply.data.unwrap()["buffered_seconds"].as_f64().unwrap();
        assert!(buffered > 0.0, "ring must report buffered audio");

        // Mute is part of the final output: the tap sits post-mute.
        state.muted.store(true, Ordering::Relaxed);
        run_block(&mut proc, &input, &mut out);
        state.muted.store(false, Ordering::Relaxed);
        let snap = state.replay_ring.lock().unwrap().snapshot();
        assert!((snap[snap.len() - 2] - 0.0).abs() < 1e-9, "muted tail must be silence");
        assert!((snap[0] - 0.25).abs() < 1e-6, "unmuted head keeps the signal");

        let (reply, evts) = state.handle(&Action::SaveInstantReplay);
        assert!(reply.ok, "{:?}", reply.error);
        let path = reply.data.unwrap()["path"].as_str().unwrap().to_string();
        assert!(path.ends_with(".wav"), "{path}");
        match &evts[0] {
            ControlEvent::ReplaySaved { path: p } => assert_eq!(p, &path),
            other => panic!("expected ReplaySaved, got {other:?}"),
        }
        let bytes = std::fs::read(&path).expect("replay file");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        // Two fed blocks of 960 frames each: 1920 stereo frames of PCM16.
        assert_eq!(bytes.len(), 44 + 1920 * 2 * 2);
        assert_eq!(
            i16::from_le_bytes([bytes[44], bytes[45]]),
            (0.25f32 * 32767.0).round() as i16
        );
        assert_eq!(
            i16::from_le_bytes([bytes[46], bytes[47]]),
            (-0.25f32 * 32767.0).round() as i16
        );
    }

    #[test]
    fn replay_save_errors_are_clear() {
        let state = test_state();
        // Enabled but nothing fed yet: empty buffer.
        let (reply, _) = state.handle(&Action::SaveInstantReplay);
        assert!(!reply.ok);
        assert!(reply.error.unwrap().contains("empty"));
        // Disabled: says so instead of writing a silent file.
        let (reply, _) = state.handle(&Action::SetInstantReplay { enabled: Some(false), seconds: None });
        assert!(reply.ok);
        let (reply, _) = state.handle(&Action::SaveInstantReplay);
        assert!(!reply.ok);
        assert!(reply.error.unwrap().contains("disabled"));
    }

    #[test]
    fn replay_hotkey_maps_to_save_action() {
        let state = test_state();
        assert_eq!(
            hotkey_action(&HotkeyTarget::SaveReplay, true, &state),
            Some(Action::SaveInstantReplay)
        );
        assert_eq!(hotkey_action(&HotkeyTarget::SaveReplay, false, &state), None);
    }

    #[test]
    fn inject_bus_carries_full_soundboard_mix() {
        let state = test_state();
        state.voice_enabled.store(false, Ordering::Relaxed);
        {
            // A VAD-routed tone: it reaches vm-mic *and* the inject bus.
            let mut m = state.mixer.lock().unwrap();
            m.add_sound(vm_soundboard::LoadedSound {
                id: vm_soundboard::SoundHashId(21),
                name: "vad-tone".into(),
                rate: 48_000,
                channels: vec![vec![0.4; 256]],
            });
            let opts = vm_soundboard::SoundOptions { send_to_vad: true, ..Default::default() };
            m.play(vm_soundboard::SoundHashId(21), opts).unwrap();
        }
        let mut proc = GraphProcessor::for_state(&state);
        let silent = [vec![0.0f32; 64], vec![0.0f32; 64]];
        let mut out = [vec![0.0f32; 64], vec![0.0f32; 64]];
        run_block(&mut proc, &silent, &mut out);
        let inj = proc.inject_bus();
        assert_eq!(inj.len(), 128, "interleaved stereo");
        assert!((inj[0] - 0.4).abs() < 1e-6, "vad sound on inject bus: {}", inj[0]);
        assert!((inj[1] - 0.4).abs() < 1e-6, "right channel too");
    }

    #[test]
    fn passthrough_flow_moves_restores_and_persists() {
        let state = test_state();
        // Stub app model advertises Discord.
        let (reply, _) = state.handle(&Action::ListPassthroughApps);
        assert!(reply.ok, "{:?}", reply.error);
        let apps = reply.data.unwrap()["apps"].as_array().unwrap().clone();
        assert!(apps.iter().any(|a| a["app"] == "Discord"));

        // Enable, targeting Discord on the stub sink.
        let (reply, evts) = state.handle(&Action::SetPassthrough {
            enabled: Some(true),
            device: Some("stub-sink".into()),
            app: Some("Discord".into()),
        });
        assert!(reply.ok, "{:?}", reply.error);
        match &evts[0] {
            ControlEvent::PassthroughChanged { enabled, device, app } => {
                assert!(*enabled);
                assert_eq!(device, "stub-sink");
                assert_eq!(app, "Discord");
            }
            other => panic!("expected PassthroughChanged, got {other:?}"),
        }
        let moved = state.audio.lock().unwrap().output_streams().unwrap();
        assert_eq!(
            moved.iter().find(|a| a.app == "Discord").unwrap().sink,
            "vm-passthrough"
        );
        assert_eq!(
            moved.iter().find(|a| a.app == "FakePlayer").unwrap().sink,
            "stub-sink",
            "untargeted apps stay put"
        );

        // Unknown app: error, config rolled back, backend re-converged.
        let (reply, _) = state.handle(&Action::SetPassthrough {
            enabled: None,
            device: None,
            app: Some("Ghost".into()),
        });
        assert!(!reply.ok);
        assert!(reply.error.unwrap().contains("Ghost"));
        let cfg = load_daemon_config_file(&state.config_path).expect("reload");
        assert_eq!(cfg.passthrough.app, "Discord", "failed set rolled back");
        assert!(cfg.passthrough.enabled);
        let after = state.audio.lock().unwrap().output_streams().unwrap();
        assert_eq!(
            after.iter().find(|a| a.app == "Discord").unwrap().sink,
            "vm-passthrough",
            "backend re-converged to the previous settings"
        );

        // Disable: app restored, config persisted.
        let (reply, _) = state.handle(&Action::SetPassthrough {
            enabled: Some(false),
            device: None,
            app: None,
        });
        assert!(reply.ok, "{:?}", reply.error);
        let restored = state.audio.lock().unwrap().output_streams().unwrap();
        assert_eq!(
            restored.iter().find(|a| a.app == "Discord").unwrap().sink,
            "stub-sink"
        );
        let (reply, _) = state.handle(&Action::GetPassthrough);
        let d = reply.data.unwrap();
        assert_eq!(d["enabled"], false);
        assert_eq!(d["app"], "Discord", "target kept for next enable");
        let cfg = load_daemon_config_file(&state.config_path).expect("reload");
        assert!(!cfg.passthrough.enabled);
    }
}
