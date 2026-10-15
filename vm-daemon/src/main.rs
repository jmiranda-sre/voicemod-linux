//! `vm-daemon`: headless pipeline orchestrator.
//!
//! Owns the live runtime (`DaemonState`) and serves it over the Control
//! API + global hotkeys. `--live` selects the real PipeWire backend
//! (falls back to the stub with a clear message when unreachable).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use vm_audio::{AudioBackend, DeviceRole, StreamProcessor, StreamStopReason, StreamTopology, SAMPLE_RATE};
use vm_control::{
    Action, ControlApiServer, ControlEvent, ControlHandler, ControlReply, HotkeyManager,
    HotkeyTarget, ServerConfig,
};
use vm_dsp::{Graph, NodeConstructorContext, NodeFactory, NodeId, ProcessContext};

/// RT DSP entry: runs the voice graph, or passes through when the voice
/// changer is bypassed. Graph + flag shared with the control plane.
struct GraphProcessor {
    graph: Arc<Mutex<Graph>>,
    voice_enabled: Arc<AtomicBool>,
    /// Lock-contention counter: a quantum that finds the graph locked
    /// (preset swap in flight) passes the previous buffer through and
    /// counts as a graph-side xrun instead of blocking the RT thread.
    graph_xruns: Arc<AtomicU64>,
    lane_in: Vec<Vec<f32>>,
    lane_out: Vec<Vec<f32>>,
}

impl GraphProcessor {
    fn new(
        graph: Arc<Mutex<Graph>>,
        voice_enabled: Arc<AtomicBool>,
        graph_xruns: Arc<AtomicU64>,
    ) -> Self {
        Self { graph, voice_enabled, graph_xruns, lane_in: Vec::new(), lane_out: Vec::new() }
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
        if !self.voice_enabled.load(Ordering::Relaxed) {
            for (dst, src) in outputs.iter_mut().zip(self.lane_in.iter()) {
                dst.copy_from_slice(&src[..frames.min(dst.len()).min(src.len())]);
            }
            return;
        }
        let ctx = ProcessContext { rate, frames };
        if let Ok(mut graph) = self.graph.try_lock() {
            graph.process(&ctx, &self.lane_in, &mut self.lane_out);
        } else {
            self.graph_xruns.fetch_add(1, Ordering::Relaxed);
        }
        for (dst, src) in outputs.iter_mut().zip(self.lane_out.iter()) {
            dst.copy_from_slice(&src[..frames.min(dst.len()).min(src.len())]);
        }
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
    mixer: Mutex<vm_soundboard::SoundboardMixer>,
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
    /// Runtime input switch: stop → reopen on the new input (same virtual
    /// output) → start. Drops in-flight audio; the graph keeps running.
    fn switch_input(&self, uid: &str) -> Result<(), String> {
        let topo = self.topology.lock().map_err(|_| "topology lock poisoned".to_string())?;
        let base = topo.clone().ok_or_else(|| "pipeline never opened".to_string())?;
        drop(topo);
        let mut audio = self.audio.lock().map_err(|_| "audio lock poisoned".to_string())?;
        let devs = audio.enumerate().map_err(|e| format!("enumerate: {e}"))?;
        if !devs.iter().any(|d| d.uid.0 == uid) {
            return Err(format!("unknown input '{uid}'"));
        }
        let next = StreamTopology {
            input: vm_audio::DeviceUid::new(uid),
            virtual_out: base.virtual_out.clone(),
            monitor: base.monitor.clone(),
        };
        audio.stop(StreamStopReason::Requested).map_err(|e| format!("stop: {e}"))?;
        audio.open(&next).map_err(|e| format!("open: {e}"))?;
        audio.start().map_err(|e| format!("start: {e}"))?;
        *self.topology.lock().map_err(|_| "topology lock poisoned".to_string())? = Some(next);
        Ok(())
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
                };
                node.set_param(pname, v).map_err(|e| format!("param {pname}: {e}"))?;
            }
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
            Action::SelectVoice { voice } => {
                let path = self.preset_dir.join(format!("{voice}.json"));
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(_) => {
                        return (
                            ControlReply::err(format!("unknown voice '{voice}'")),
                            Vec::new(),
                        )
                    }
                };
                let preset = match vm_preset::load(&text) {
                    Ok(p) => p,
                    Err(e) => return (ControlReply::err(format!("bad preset: {e}")), Vec::new()),
                };
                match self.apply_preset(&preset) {
                    Ok(()) => (
                        ControlReply::ok(),
                        vec![ControlEvent::VoiceChanged { voice: voice.clone() }],
                    ),
                    Err(e) => (ControlReply::err(e), Vec::new()),
                }
            }
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
                self.mixer.lock().unwrap().stop_all();
                (ControlReply::ok(), Vec::new())
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
                // deterministic wire order.
                let ids = self.node_ids.lock().unwrap();
                let tele = self.graph.lock().map(|g| g.telemetry()).unwrap_or_default();
                let mut nodes: Vec<serde_json::Value> = tele
                    .into_iter()
                    .map(|(id, t)| {
                        let name = ids
                            .iter()
                            .find_map(|(k, v)| (*v == id).then(|| k.clone()))
                            .unwrap_or_else(|| format!("node{}", id.0));
                        serde_json::json!({
                            "node": name,
                            "avgUs": t.avg_us,
                            "maxUs": t.max_us,
                            "calls": t.calls,
                        })
                    })
                    .collect();
                nodes.sort_by(|a, b| a["node"].as_str().cmp(&b["node"].as_str()));
                drop(ids);
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
                    })),
                    Vec::new(),
                )
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
                Ok(()) => (
                    ControlReply::ok(),
                    vec![ControlEvent::DevicesChanged],
                ),
                Err(e) => (ControlReply::err(e), Vec::new()),
            },
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

fn hotkey_action(target: &HotkeyTarget, state: &DaemonState) -> Option<Action> {
    match target {
        HotkeyTarget::ToggleVoiceChanger => {
            let cur = state.voice_enabled.load(Ordering::Relaxed);
            Some(Action::ToggleVoiceChanger { enabled: !cur })
        }
        HotkeyTarget::ToggleMute => None, // master mute lands with the settings work
        HotkeyTarget::PlaySound(name) => Some(Action::PlaySound { sound: name.clone() }),
        HotkeyTarget::SelectVoice(name) => Some(Action::SelectVoice { voice: name.clone() }),
        HotkeyTarget::StopAllSounds => Some(Action::StopAllSounds),
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
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
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
        }
    }
    Ok(())
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

    let state = Arc::new(DaemonState {
        graph: Arc::new(Mutex::new(Graph::new())),
        node_ids: Mutex::new(HashMap::new()),
        voice_name: Mutex::new(String::new()),
        voice_enabled: Arc::new(AtomicBool::new(true)),
        mixer: Mutex::new(vm_soundboard::SoundboardMixer::new()),
        sound_ids: Mutex::new(HashMap::new()),
        audio: Arc::new(Mutex::new(build_backend(live)?)),
        preset_dir: preset_dir.clone(),
        topology: Mutex::new(None),
        graph_xruns: Arc::new(AtomicU64::new(0)),
        watchdog: Mutex::new(WatchdogState::default()),
    });
    // Default voice.
    {
        let text = std::fs::read_to_string(preset_dir.join("example-drive.json"))?;
        let preset = vm_preset::load(&text)?;
        state.apply_preset(&preset).map_err(|e| format!("preset: {e}"))?;
    }
    // Graph processor shares the live graph + bypass flag.
    let graph_xruns = state.graph_xruns.clone();
    let processor: Arc<Mutex<dyn StreamProcessor>> = Arc::new(Mutex::new(GraphProcessor::new(
        state.graph.clone(),
        state.voice_enabled.clone(),
        graph_xruns,
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

    // Hotkeys (best-effort: empty without input permissions).
    {
        let manager = Arc::new(Mutex::new(HotkeyManager::new()));
        let keyboards = HotkeyManager::scan_keyboards();
        if keyboards.is_empty() {
            eprintln!("hotkeys: no readable keyboards (need input group/udev rule)");
        }
        for path in keyboards {
            let path = path.split(' ').next().unwrap_or("").to_string();
            if path.is_empty() {
                continue;
            }
            let manager = manager.clone();
            let state = state.clone();
            std::thread::Builder::new()
                .name("vm-hotkey".into())
                .spawn(move || {
                    vm_control::hotkeys::watch_device(path, manager, move |target| {
                        if let Some(action) = hotkey_action(&target, &state) {
                            let (reply, _evts) = state.handle(&action);
                            if !reply.ok {
                                eprintln!("hotkey action failed: {:?}", reply.error);
                            }
                        }
                    })
                })
                .ok();
        }
    }

    // Hotplug watch + status.
    let mut tick = 0u32;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                {
                    let mut audio = state.audio.lock().unwrap();
                    for ev in audio.poll_hotplug() {
                        println!("hotplug: {ev:?} (link: {:?})", audio.link_state());
                    }
                }
                // Watchdog (F-A): auto-restart a dead link / xrun storm.
                state.watchdog_tick();
                tick += 1;
                {
                    let audio = state.audio.lock().unwrap();
                    if tick % 5 == 0 {
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
    // Reuse the soundboard's own decode path through a temp controller.
    let mut ctl = vm_soundboard::SoundboardController::new();
    ctl.load_file(id, name.to_string(), path)?;
    Err("unreachable".into())
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
        mixer: Mutex::new(vm_soundboard::SoundboardMixer::new()),
        sound_ids: Mutex::new(HashMap::new()),
        audio: Arc::new(Mutex::new(build_backend(false).unwrap())),
        preset_dir: dir.join("presets"),
        topology: Mutex::new(None),
        graph_xruns: Arc::new(AtomicU64::new(0)),
        watchdog: Mutex::new(WatchdogState::default()),
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
        // stopAll on empty is fine.
        let (reply, _) = state.handle(&Action::StopAllSounds);
        assert!(reply.ok);
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
        let mut proc = GraphProcessor::new(
            state.graph.clone(),
            state.voice_enabled.clone(),
            state.graph_xruns.clone(),
        );
        let input = vec![vec![0.3f32; 64]];
        let mut out = vec![vec![0.0f32; 64]];
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
        let mut proc = GraphProcessor::new(
            state.graph.clone(),
            state.voice_enabled.clone(),
            state.graph_xruns.clone(),
        );
        for _ in 0..4 {
            let input = vec![vec![0.1f32; 64]; 2];
            let mut out = vec![vec![0.0f32; 64]; 2];
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
    }

    #[test]
    fn graph_lock_contention_counts_xrun() {
        let state = test_state();
        let (reply, _) = state.handle(&Action::SelectVoice { voice: "example-drive".into() });
        assert!(reply.ok);
        let mut proc = GraphProcessor::new(
            state.graph.clone(),
            state.voice_enabled.clone(),
            state.graph_xruns.clone(),
        );
        // Hold the graph lock: the RT path must skip, not block.
        let _held = state.graph.lock().unwrap();
        let input = vec![vec![0.1f32; 64]; 2];
        let mut out = vec![vec![0.0f32; 64]; 2];
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
        let mut proc = GraphProcessor::new(
            state.graph.clone(),
            state.voice_enabled.clone(),
            state.graph_xruns.clone(),
        );
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
        assert_eq!(state.node_ids.lock().unwrap().len(), 11, "full chain must be wired");

        // Deterministic white noise (LCG), ±amp.
        let mut st = 0x2545_f491_4f6c_dd1du64;
        let mut noise = Vec::with_capacity(96_000);
        for _ in 0..96_000 {
            st = st.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            noise.push((((st >> 32) as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.1);
        }
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let mut proc = GraphProcessor::new(
            state.graph.clone(),
            state.voice_enabled.clone(),
            state.graph_xruns.clone(),
        );
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
        assert!(
            out_r < in_r * 0.9,
            "mic chain must attenuate stationary noise: in {in_r:.4}, out {out_r:.4}"
        );
        assert!(out_r > in_r * 0.001, "output must not be muted: {out_r:.5}");
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
