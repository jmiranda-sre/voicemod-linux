//! Live PipeWire backend (`pipewire` 0.10 against system PipeWire 1.6).
//!
//! Architecture (all PipeWire objects are `!Send`, so everything lives
//! on ONE worker thread that pumps `Loop::iterate`):
//!
//! ```text
//! mic ──▶ capture stream ──▶ raw ring ──╮
//!                                        ├─▶ DSP graph (try_lock, RT-safe)
//!                    ┌─▶ vm-mic sink ◀───┘
//!                    └─▶ monitor ring ──▶ monitor stream ──▶ default sink
//! ```
//!
//! - Sample-granular SPSC rings absorb ANY graph quantum (our 960 is a
//!   `node.latency` request, not a guarantee).
//! - The graph always runs stereo: mono capture is duplicated, the
//!   negotiated format is read back from `param_changed`.
//! - Underflow → silence + xrun counter (mirrors the reference xrun
//!   detector); overflow → drop + xrun counter.
//! - Hotplug comes from registry `global`/`global_remove`.

use super::super::{
    AudioBackend, AudioDevice, AudioError, CallbackStats, DeviceRole, DeviceUid, HotplugEvent,
    StreamKind, StreamLinkState, StreamProcessor, StreamStopReason, StreamTopology, MAX_CHANNELS,
    MONITOR_SINK_NAME, QUANTUM_FRAMES, SAMPLE_RATE, VIRTUAL_SOURCE_NAME,
};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Ring capacity in samples (8 quanta × stereo × headroom).
const RING_SAMPLES: usize = QUANTUM_FRAMES as usize * MAX_CHANNELS * 32;
/// Requested stream latency: 960/48000 = 20 ms.
const LATENCY_PROP: &str = "960/48000";

#[derive(Debug, Clone)]
struct NodeSummary {
    id: u32,
    name: String,
    media_class: String,
    description: String,
}

#[derive(Debug, Clone)]
struct PortSummary {
    id: u32,
    node_id: u32,
    name: String,
    /// "in" or "out" (port.direction).
    direction: String,
}

/// Negotiated sample format, normalized to a small code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SampleFormat {
    F32,
    S32,
    /// Packed 3-byte S24.
    S24,
    /// S24 in a 32-bit container.
    S24_32,
    S16,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default)]
struct FormatState {
    rate: u32,
    channels: usize,
    format: SampleFormat,
}

fn classify_format(f: spa::param::audio::AudioFormat) -> SampleFormat {
    use spa::param::audio::AudioFormat as A;
    match f {
        A::F32LE | A::F32BE => SampleFormat::F32,
        A::S32LE | A::S32BE => SampleFormat::S32,
        A::S24LE | A::S24BE => SampleFormat::S24,
        A::S24_32LE | A::S24_32BE => SampleFormat::S24_32,
        A::S16LE | A::S16BE => SampleFormat::S16,
        _ => SampleFormat::Unknown,
    }
}

struct Shared {
    stats_cap: Mutex<CallbackStats>,
    stats_vad: Mutex<CallbackStats>,
    stats_mon: Mutex<CallbackStats>,
    fmt_cap: Mutex<FormatState>,
    fmt_out: Mutex<FormatState>,
    link: Mutex<StreamLinkState>,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stats_cap: Mutex::new(CallbackStats::default()),
            stats_vad: Mutex::new(CallbackStats::default()),
            stats_mon: Mutex::new(CallbackStats::default()),
            fmt_cap: Mutex::new(FormatState { rate: SAMPLE_RATE, channels: 2, format: SampleFormat::F32 }),
            fmt_out: Mutex::new(FormatState { rate: SAMPLE_RATE, channels: 2, format: SampleFormat::F32 }),
            link: Mutex::new(StreamLinkState::Down),
        })
    }
}

fn record(stats: &Mutex<CallbackStats>, start: Instant, xrun: bool) {
    let us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;
    if let Ok(mut s) = stats.lock() {
        s.record(us, xrun);
    }
}

// ---------------------------------------------------------------------------
// Stream user-data (owned by listeners; only Arcs cross threads).
// ---------------------------------------------------------------------------

struct CaptureData {
    shared: Arc<Shared>,
    tx: rtrb::Producer<f32>,
    scratch: Vec<f32>,
}

struct PlaybackData {
    shared: Arc<Shared>,
    rx: rtrb::Consumer<f32>,
    processor: Arc<Mutex<dyn StreamProcessor>>,
    /// Tee into the monitor ring (Some on the VAD playback path).
    mon_tx: Option<rtrb::Producer<f32>>,
    scratch_in: Vec<Vec<f32>>,
    scratch_out: Vec<Vec<f32>>,
    scratch_raw: Vec<f32>,
}

struct MonitorData {
    shared: Arc<Shared>,
    rx: rtrb::Consumer<f32>,
    scratch: Vec<f32>,
}

/// Build one EnumFormat pod: F32LE @ 48 kHz, stereo.
/// The returned pod borrows intentionally leaked bytes ('static) owned
/// for the whole stream lifetime by the worker (documented leak, bounded).
fn format_pod(channels: u32) -> Result<&'static Pod, AudioError> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(channels);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| AudioError::Backend(format!("pod serialize: {e:?}")))?
    .0
    .into_inner();
    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    Pod::from_bytes(leaked).ok_or_else(|| AudioError::Backend("pod parse failed".into()))
}

// ---------------------------------------------------------------------------
// Worker commands.
// ---------------------------------------------------------------------------

enum Cmd {
    Enumerate(mpsc::Sender<Result<Vec<AudioDevice>, AudioError>>),
    EnsureVmMic(mpsc::Sender<Result<u32, AudioError>>),
    Open(OpenReq, mpsc::Sender<Result<(), AudioError>>),
    Start(mpsc::Sender<Result<(), AudioError>>),
    Stop(StreamStopReason, mpsc::Sender<Result<(), AudioError>>),
    DestroyVmMic(mpsc::Sender<Result<(), AudioError>>),
    Quit,
}

struct OpenReq {
    input_id: u32,
    vm_id: u32,
    processor: Arc<Mutex<dyn StreamProcessor>>,
}

// ---------------------------------------------------------------------------
// Public handle.
// ---------------------------------------------------------------------------

pub struct PipeWireBackend {
    cmd: mpsc::Sender<Cmd>,
    hotplug_rx: mpsc::Receiver<HotplugEvent>,
    shared: Arc<Shared>,
    devices: Vec<AudioDevice>,
    node_ids: HashMap<String, u32>,
    topology: Option<StreamTopology>,
    processor: Option<Arc<Mutex<dyn StreamProcessor>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl PipeWireBackend {
    pub fn new() -> Result<Self, AudioError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (hp_tx, hp_rx) = mpsc::channel::<HotplugEvent>();
        let shared = Shared::new();
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("vm-pw".into())
            .spawn(move || worker_main(cmd_rx, hp_tx, worker_shared))
            .map_err(|e| AudioError::Backend(format!("spawn pw thread: {e}")))?;
        Ok(Self {
            cmd: cmd_tx,
            hotplug_rx: hp_rx,
            shared,
            devices: Vec::new(),
            node_ids: HashMap::new(),
            topology: None,
            processor: None,
            worker: Some(worker),
        })
    }

    fn roundtrip<T>(
        &self,
        send: impl FnOnce(mpsc::Sender<T>) -> Cmd,
        what: &str,
    ) -> Result<T, AudioError> {
        let (tx, rx) = mpsc::channel::<T>();
        self.cmd
            .send(send(tx))
            .map_err(|_| AudioError::Backend(format!("pw thread gone ({what})")))?;
        rx.recv_timeout(Duration::from_secs(90))
            .map_err(|_| AudioError::Backend(format!("pw thread timeout ({what})")))
    }

    /// Create `vm-mic` if missing; returns its node id either way.
    pub fn ensure_virtual_source_id(&mut self) -> Result<u32, AudioError> {
        flatten(self.roundtrip(Cmd::EnsureVmMic, "ensure vm-mic"))
    }

    /// Best-effort teardown of the `vm-mic` node created by `open`.
    pub fn destroy_virtual_source(&mut self) -> Result<(), AudioError> {
        flatten(self.roundtrip(Cmd::DestroyVmMic, "destroy vm-mic"))
    }
}

impl Drop for PipeWireBackend {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Quit);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn flatten<T>(r: Result<Result<T, AudioError>, AudioError>) -> Result<T, AudioError> {
    r?
}

/// Default sink node id via `pactl get-default-sink` (pipewire-pulse).
/// Returns None when pactl is missing/fails — callers fall back to
/// AUTOCONNECT routing.
fn default_sink_id(nodes: &Arc<Mutex<HashMap<u32, NodeSummary>>>) -> Option<u32> {
    let out = std::process::Command::new("pactl").arg("get-default-sink").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    nodes
        .lock()
        .ok()?
        .iter()
        .find(|(_, n)| n.name == name)
        .map(|(id, _)| *id)
}

fn device_role(class: &str, name: &str) -> Option<DeviceRole> {
    if name == VIRTUAL_SOURCE_NAME || name.starts_with(&format!("{VIRTUAL_SOURCE_NAME}.")) {
        return Some(DeviceRole::Virtual);
    }
    if class.contains("Audio/Source") {
        Some(DeviceRole::Input)
    } else {
        None
    }
}

impl AudioBackend for PipeWireBackend {
    fn enumerate(&mut self) -> Result<Vec<AudioDevice>, AudioError> {
        let devs: Vec<AudioDevice> = flatten(self.roundtrip(Cmd::Enumerate, "enumerate"))?;
        self.node_ids = devs
            .iter()
            .filter_map(|d| {
                // node id travels inside the uid as "name#id" (see worker).
                d.uid
                    .0
                    .rsplit('#')
                    .next()
                    .and_then(|id| id.parse::<u32>().ok())
                    .map(|id| (d.uid.0.clone(), id))
            })
            .collect();
        self.devices = devs.clone();
        Ok(devs)
    }

    fn open(&mut self, topology: &StreamTopology) -> Result<(), AudioError> {
        let proc = self.processor.clone().ok_or_else(|| {
            AudioError::Backend("no StreamProcessor installed (call set_processor first)".into())
        })?;
        let find = |uid: &DeviceUid| {
            self.node_ids
                .get(&uid.0)
                .copied()
                .ok_or_else(|| AudioError::DeviceNotFound(uid.clone()))
        };
        let req = OpenReq {
            input_id: find(&topology.input)?,
            vm_id: find(&topology.virtual_out)?,
            processor: proc,
        };
        flatten(self.roundtrip(|tx| Cmd::Open(req, tx), "open"))?;
        self.topology = Some(topology.clone());
        Ok(())
    }

    fn start(&mut self) -> Result<(), AudioError> {
        if self.topology.is_none() {
            return Err(AudioError::NoStream);
        }
        flatten(self.roundtrip(Cmd::Start, "start"))
    }

    fn stop(&mut self, reason: StreamStopReason) -> Result<(), AudioError> {
        flatten(self.roundtrip(|tx| Cmd::Stop(reason, tx), "stop"))
    }

    fn stats(&self, kind: StreamKind) -> CallbackStats {
        let guard = match kind {
            StreamKind::FullDuplexCapture => self.shared.stats_cap.lock(),
            StreamKind::HalfDuplexPlayback => self.shared.stats_vad.lock(),
            StreamKind::Monitor => self.shared.stats_mon.lock(),
        };
        guard
            .map(|g| CallbackStats {
                invocations: g.invocations,
                avg_duration_us: g.avg_duration_us,
                max_duration_us: g.max_duration_us,
                xruns: g.xruns,
                watchdog_trips: g.watchdog_trips,
                last_frames: g.last_frames,
            })
            .unwrap_or_default()
    }

    fn poll_hotplug(&mut self) -> Vec<HotplugEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.hotplug_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn ensure_virtual_source(&mut self) -> Result<DeviceUid, AudioError> {
        let id = self.ensure_virtual_source_id()?;
        Ok(DeviceUid::new(format!("{VIRTUAL_SOURCE_NAME}#{id}")))
    }

    fn link_state(&self) -> StreamLinkState {
        self.shared.link.lock().map(|g| *g).unwrap_or(StreamLinkState::Down)
    }

    fn negotiated(&self) -> (u32, usize, u32, usize) {
        let cap = self.shared.fmt_cap.lock().map(|f| (f.rate, f.channels)).unwrap_or((SAMPLE_RATE, 2));
        let out = self.shared.fmt_out.lock().map(|f| (f.rate, f.channels)).unwrap_or((SAMPLE_RATE, 2));
        (cap.0, cap.1, out.0, out.1)
    }

    fn set_processor(&mut self, processor: Arc<Mutex<dyn StreamProcessor>>) {
        self.processor = Some(processor);
    }
}

// ---------------------------------------------------------------------------
// Worker (owns every PipeWire object; pumps Loop::iterate).
// ---------------------------------------------------------------------------

fn reply_err(cmd: Cmd, msg: &str) {
    let err = AudioError::Backend(msg.into());
    match cmd {
        Cmd::Enumerate(tx) => {
            let _ = tx.send(Err(err));
        }
        Cmd::EnsureVmMic(tx) => {
            let _ = tx.send(Err(err));
        }
        Cmd::Open(_, tx) => {
            let _ = tx.send(Err(err));
        }
        Cmd::Start(tx) => {
            let _ = tx.send(Err(err));
        }
        Cmd::Stop(_, tx) => {
            let _ = tx.send(Err(err));
        }
        Cmd::DestroyVmMic(tx) => {
            let _ = tx.send(Err(err));
        }
        Cmd::Quit => {}
    }
}

fn worker_main(
    cmd_rx: mpsc::Receiver<Cmd>,
    hp_tx: mpsc::Sender<HotplugEvent>,
    shared: Arc<Shared>,
) {
    pw::init();
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("vm-audio: mainloop failed: {e}");
            drain_with_err(cmd_rx, "mainloop init failed");
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vm-audio: context failed: {e}");
            drain_with_err(cmd_rx, "context init failed");
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vm-audio: connect failed: {e}");
            drain_with_err(cmd_rx, "connect failed");
            return;
        }
    };
    let nodes: Arc<Mutex<HashMap<u32, NodeSummary>>> = Arc::new(Mutex::new(HashMap::new()));
    // Persistent registry listener: feeds the node table (enumerate/open)
    // and hotplug events. Owns only Arcs, so RT control callbacks stay safe.
    let nodes_g = nodes.clone();
    let nodes_r = nodes.clone();
    let hp_g = hp_tx.clone();
    let ports: Arc<Mutex<HashMap<u32, PortSummary>>> = Arc::new(Mutex::new(HashMap::new()));
    let ports_g = ports.clone();
    let ports_r = ports.clone();
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vm-audio: registry failed: {e}");
            return;
        }
    };
    // NOTE: both the registry proxy AND the listener guard must stay
    // alive (dropping the proxy kills the server-side subscription).
    let _reg_listener = registry
        .add_listener_local()
        .global(move |obj| {
            if obj.type_ == pw::types::ObjectType::Port {
                if let Some(props) = obj.props.as_ref() {
                    let node_id: u32 = props.get("node.id").and_then(|s| s.parse().ok()).unwrap_or(0);
                    let name = props.get("port.name").unwrap_or("").to_string();
                    let direction = props.get("port.direction").unwrap_or("").to_string();
                    if node_id != 0 && !name.is_empty() {
                        if let Ok(mut map) = ports_g.lock() {
                            map.insert(obj.id, PortSummary { id: obj.id, node_id, name, direction });
                        }
                    }
                }
                return;
            }
            if obj.type_ != pw::types::ObjectType::Node {
                return;
            }
            let (name, class, desc) = match obj.props.as_ref() {
                Some(props) => (
                    props.get("node.name").unwrap_or("").to_string(),
                    props.get("media.class").unwrap_or("").to_string(),
                    props.get("node.description").unwrap_or("").to_string(),
                ),
                None => return,
            };
            if name.is_empty() {
                return;
            }
            let summary =
                NodeSummary { id: obj.id, name: name.clone(), media_class: class.clone(), description: desc };
            let is_audio = class.contains("Audio/");
            if let Ok(mut map) = nodes_g.lock() {
                map.insert(obj.id, summary.clone());
            }
            if is_audio {
                if let Some(role) = device_role(&class, &name) {
                    let dev = AudioDevice {
                        uid: DeviceUid::new(format!("{name}#{}", obj.id)),
                        name: if summary.description.is_empty() {
                            name
                        } else {
                            summary.description
                        },
                        role,
                        sample_rate: SAMPLE_RATE,
                        channels: MAX_CHANNELS,
                        latency_secs: QUANTUM_FRAMES as f64 / SAMPLE_RATE as f64,
                        is_bluetooth: summary.name.contains("bluez"),
                    };
                    let _ = hp_g.send(HotplugEvent::DeviceAdded(dev));
                }
            }
        })
        .global_remove(move |id| {
            if let Ok(mut pmap) = ports_r.lock() {
                pmap.remove(&id);
            }
            let removed = nodes_r.lock().ok().and_then(|mut map| map.remove(&id));
            if let Some(n) = removed {
                if device_role(&n.media_class, &n.name).is_some() {
                    let _ = hp_tx.send(HotplugEvent::DeviceRemoved(DeviceUid::new(format!(
                        "{}#{id}",
                        n.name
                    ))));
                }
            }
        })
        .register();

    let mut worker = Worker {
        cmd_rx,
        shared,
        nodes,
        ports,
        _registry: registry,
        _reg_listener,
        vm_proxy: None,
        vm_id: None,
        vm_module: None,
        streams: None,
        links: Vec::new(),
    };
    worker.run(&mainloop, &core);
}

fn drain_with_err(cmd_rx: mpsc::Receiver<Cmd>, msg: &str) {
    while let Ok(cmd) = cmd_rx.recv() {
        if matches!(cmd, Cmd::Quit) {
            break;
        }
        reply_err(cmd, msg);
    }
}

struct LiveStreams {
    _capture: pw::stream::StreamRc,
    _vad: pw::stream::StreamRc,
    _monitor: pw::stream::StreamRc,
    _cap_l: pw::stream::StreamListener<CaptureData>,
    _vad_l: pw::stream::StreamListener<PlaybackData>,
    _mon_l: pw::stream::StreamListener<MonitorData>,
    _pods: Vec<&'static Pod>,
}

struct Worker {
    cmd_rx: mpsc::Receiver<Cmd>,
    shared: Arc<Shared>,
    nodes: Arc<Mutex<HashMap<u32, NodeSummary>>>,
    ports: Arc<Mutex<HashMap<u32, PortSummary>>>,
    _registry: pw::registry::RegistryRc,
    _reg_listener: pw::registry::Listener,
    vm_proxy: Option<pw::node::Node>,
    vm_id: Option<u32>,
    /// pactl module index when created via module-null-sink (for unload).
    vm_module: Option<u32>,
    streams: Option<LiveStreams>,
    links: Vec<pw::link::Link>,
}

impl Worker {
    fn run(&mut self, mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) {
        let loop_ = mainloop.loop_();
        // Wait for the initial registry burst before serving enumerate.
        self.pump_until(loop_, Duration::from_secs(3), || false);
        loop {
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                if matches!(cmd, Cmd::Quit) {
                    self.teardown();
                    return;
                }
                self.handle(cmd, core, loop_);
            }
            // Pump control traffic (params/state/registry). Process
            // callbacks arrive on RT threads independently.
            let _ = loop_.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
        }
    }

    /// Pump the loop until `done` or timeout.
    fn pump_until(
        &self,
        loop_: &pw::loop_::Loop,
        timeout: Duration,
        mut done: impl FnMut() -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done() {
                break;
            }
            let _ = loop_.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
        }
    }

    fn teardown(&mut self) {
        self.streams = None;
        *self.shared.link.lock().unwrap() = StreamLinkState::Down;
    }

    fn handle(
        &mut self,
        cmd: Cmd,
        core: &pw::core::CoreRc,
        loop_: &pw::loop_::Loop,
    ) {
        match cmd {
            Cmd::Enumerate(tx) => {
                let _ = tx.send(self.snapshot_devices());
            }
            Cmd::EnsureVmMic(tx) => {
                let _ = tx.send(self.ensure_vm_mic(core, loop_));
            }
            Cmd::Open(req, tx) => {
                let _ = tx.send(self.open_streams(core, loop_, req));
            }
            Cmd::Start(tx) => {
                let _ = tx.send(self.start_streams());
            }
            Cmd::Stop(reason, tx) => {
                self.stop_streams(reason);
                let _ = tx.send(Ok(()));
            }
            Cmd::DestroyVmMic(tx) => {
                let _ = tx.send(self.destroy_vm_mic(core, loop_));
            }
            Cmd::Quit => {}
        }
    }

    fn snapshot_devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        let map = self.nodes.lock().map_err(|_| AudioError::Backend("node map poisoned".into()))?;
        Ok(map
            .values()
            .filter_map(|n| {
                device_role(&n.media_class, &n.name).map(|role| AudioDevice {
                    uid: DeviceUid::new(format!("{}#{}", n.name, n.id)),
                    name: if n.description.is_empty() {
                        n.name.clone()
                    } else {
                        n.description.clone()
                    },
                    role,
                    sample_rate: SAMPLE_RATE,
                    channels: MAX_CHANNELS,
                    latency_secs: QUANTUM_FRAMES as f64 / SAMPLE_RATE as f64,
                    is_bluetooth: n.name.contains("bluez"),
                })
            })
            .collect())
    }

    fn ensure_vm_mic(
        &mut self,
        core: &pw::core::CoreRc,
        loop_: &pw::loop_::Loop,
    ) -> Result<u32, AudioError> {
        if let Some(id) = self.find_vm_id() {
            self.vm_id = Some(id);
            return Ok(id);
        }
        // Drop any stale proxy before recreating.
        self.vm_proxy = None;
        // Prefer module-null-sink via pactl: its monitor provably forwards
        // input (verified live). The adapter factory node is kept as a
        // fallback when pactl is unavailable.
        match self.ensure_vm_mic_pactl(loop_) {
            Ok(id) => return Ok(id),
            Err(e) => eprintln!("vm-audio: pactl vm-mic failed ({e}), trying adapter factory"),
        }
        let props = properties! {
            *pw::keys::FACTORY_NAME => "support.null-audio-sink",
            *pw::keys::NODE_NAME => VIRTUAL_SOURCE_NAME,
            *pw::keys::NODE_DESCRIPTION => "vm-mic virtual microphone",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::AUDIO_RATE => format!("{SAMPLE_RATE}"),
            *pw::keys::AUDIO_CHANNELS => format!("{MAX_CHANNELS}"),
            *pw::keys::NODE_VIRTUAL => "true",
        };
        let node: pw::node::Node = core
            .create_object("adapter", &props)
            .map_err(|e| AudioError::Backend(format!("create vm-mic: {e:?}")))?;
        self.vm_proxy = Some(node);
        // The object appears asynchronously; pump until visible.
        let mut found = None;
        self.pump_until(loop_, Duration::from_secs(5), || {
            found = self.find_vm_id();
            found.is_some()
        });
        match found {
            Some(id) => {
                self.vm_id = Some(id);
                Ok(id)
            }
            None => Err(AudioError::Backend("vm-mic node did not appear".into())),
        }
    }

    /// Create vm-mic through PipeWire's own module-null-sink (pactl).
    /// Returns the node id once visible. Records the module index so
    /// `destroy_vm_mic` can unload exactly what we created.
    fn ensure_vm_mic_pactl(&mut self, loop_: &pw::loop_::Loop) -> Result<u32, AudioError> {
        let out = std::process::Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                "sink_name=vm-mic",
                "sink_properties=device.description='vm-mic virtual microphone'",
            ])
            .output()
            .map_err(|e| AudioError::Backend(format!("pactl missing/failed: {e}")))?;
        if !out.status.success() {
            return Err(AudioError::Backend(format!(
                "pactl load-module failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let module: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .map_err(|_| AudioError::Backend("pactl returned no module id".into()))?;
        let mut found = None;
        self.pump_until(loop_, Duration::from_secs(5), || {
            found = self.find_vm_id();
            found.is_some()
        });
        match found {
            Some(id) => {
                self.vm_module = Some(module);
                self.vm_id = Some(id);
                Ok(id)
            }
            None => {
                let _ = std::process::Command::new("pactl")
                    .args(["unload-module", &module.to_string()])
                    .output();
                Err(AudioError::Backend("pactl vm-mic node did not appear".into()))
            }
        }
    }

    fn find_vm_id(&self) -> Option<u32> {
        self.nodes
            .lock()
            .ok()?
            .iter()
            .find(|(_, n)| n.name == VIRTUAL_SOURCE_NAME)
            .map(|(id, _)| *id)
    }

    fn destroy_vm_mic(
        &mut self,
        core: &pw::core::CoreRc,
        loop_: &pw::loop_::Loop,
    ) -> Result<(), AudioError> {
        if let Some(proxy) = self.vm_proxy.take() {
            core.destroy_object(proxy)
                .map_err(|e| AudioError::Backend(format!("destroy vm-mic: {e:?}")))?;
            self.pump_until(loop_, Duration::from_secs(3), || self.find_vm_id().is_none());
        }
        self.vm_id = None;
        Ok(())
    }

    fn open_streams(
        &mut self,
        core: &pw::core::CoreRc,
        loop_: &pw::loop_::Loop,
        req: OpenReq,
    ) -> Result<(), AudioError> {
        if self.streams.is_some() {
            return Err(AudioError::StreamAlreadyRunning);
        }
        let (raw_tx, raw_rx) = rtrb::RingBuffer::new(RING_SAMPLES);
        let (mon_tx, mon_rx) = rtrb::RingBuffer::new(RING_SAMPLES);

        let cap_data = CaptureData { shared: self.shared.clone(), tx: raw_tx, scratch: Vec::new() };
        let vad_data = PlaybackData {
            shared: self.shared.clone(),
            rx: raw_rx,
            processor: req.processor,
            mon_tx: Some(mon_tx),
            scratch_in: Vec::new(),
            scratch_out: Vec::new(),
            scratch_raw: Vec::new(),
        };
        let mon_data = MonitorData { shared: self.shared.clone(), rx: mon_rx, scratch: Vec::new() };

        // Resolve the mic's output ports first so the capture format
        // matches the device width (mono mics expose a single port).
        let mic_ch = self.mic_channel_count(loop_, req.input_id)?;

        let cap_props = properties! {
            *pw::keys::NODE_NAME => "vm-capture",
            *pw::keys::NODE_DESCRIPTION => "vm voice input",
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_LATENCY => LATENCY_PROP,
        };
        let vad_props = properties! {
            *pw::keys::NODE_NAME => "vm-playback-vad",
            *pw::keys::NODE_DESCRIPTION => "vm processed output to vm-mic",
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_LATENCY => LATENCY_PROP,
        };
        // Monitor routing: explicit link to the default sink when pactl
        // resolves it (deterministic, no doubles); AUTOCONNECT flag only
        // as a fallback when resolution fails.
        let mon_sink = default_sink_id(&self.nodes);
        if mon_sink.is_none() {
            eprintln!("vm-audio: default sink unknown, monitor falls back to autoconnect");
        }
        let mon_props = properties! {
            *pw::keys::NODE_NAME => MONITOR_SINK_NAME,
            *pw::keys::NODE_DESCRIPTION => "vm hear-myself monitor",
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_LATENCY => LATENCY_PROP,
        };

        let cap_stream = pw::stream::StreamRc::new(pw::core::CoreRc::clone(core), "vm-capture", cap_props)
            .map_err(|e| AudioError::Backend(format!("capture stream: {e:?}")))?;
        let vad_stream = pw::stream::StreamRc::new(pw::core::CoreRc::clone(core), "vm-playback-vad", vad_props)
            .map_err(|e| AudioError::Backend(format!("vad stream: {e:?}")))?;
        let mon_stream = pw::stream::StreamRc::new(pw::core::CoreRc::clone(core), MONITOR_SINK_NAME, mon_props)
            .map_err(|e| AudioError::Backend(format!("monitor stream: {e:?}")))?;

        let cap_l = cap_stream
            .add_local_listener_with_user_data(cap_data)
            .state_changed(|_, data, _old, new| {
                *data.shared.link.lock().unwrap() = match new {
                    pw::stream::StreamState::Streaming => StreamLinkState::Streaming,
                    pw::stream::StreamState::Error(_) => StreamLinkState::NeedsRestart,
                    _ => StreamLinkState::Down,
                };
            })
            .param_changed(|_, data, id, param| {
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else { return };
                let mut info = spa::param::audio::AudioInfoRaw::new();
                if info.parse(param).is_ok() {
                    let fmt = classify_format(info.format());
                    if fmt != SampleFormat::F32 {
                        eprintln!("vm-audio: capture negotiated {fmt:?} (converting to f32)");
                    }
                    *data.shared.fmt_cap.lock().unwrap() = FormatState {
                        rate: info.rate(),
                        channels: info.channels() as usize,
                        format: fmt,
                    };
                }
            })
            .process(|stream, data| {
                let t0 = Instant::now();
                let mut xrun = false;
                if let Some(mut buffer) = stream.dequeue_buffer() {
                    let ch = data.shared.fmt_cap.lock().map(|f| f.channels.max(1)).unwrap_or(2);
                    let cap_fmt = data
                        .shared
                        .fmt_cap
                        .lock()
                        .map(|f| f.format)
                        .unwrap_or(SampleFormat::F32);
                    for data_chunk in buffer.datas_mut() {
                        // Valid frames come from the chunk, NOT the buffer
                        // size (buffers are preallocated for max quantum;
                        // reading past chunk.size() yields stale memory).
                        let valid = data_chunk.chunk().size() as usize;
                        if let Some(bytes) = data_chunk.data() {
                            let bytes = &bytes[..valid.min(bytes.len())];
                            let bps = match cap_fmt {
                                SampleFormat::F32 | SampleFormat::S32 | SampleFormat::S24_32 => 4,
                                SampleFormat::S24 => 3,
                                SampleFormat::S16 => 2,
                                SampleFormat::Unknown => {
                                    xrun = true;
                                    continue;
                                }
                            };
                            let frames = bytes.len() / (bps * ch);
                            data.scratch.resize(frames * ch, 0.0);
                            for f in 0..frames {
                                for c in 0..ch {
                                    let off = (f * ch + c) * bps;
                                    data.scratch[f * ch + c] = match cap_fmt {
                                        SampleFormat::F32 => f32::from_le_bytes([
                                            bytes[off],
                                            bytes[off + 1],
                                            bytes[off + 2],
                                            bytes[off + 3],
                                        ]),
                                        SampleFormat::S32 => {
                                            i32::from_le_bytes([
                                                bytes[off],
                                                bytes[off + 1],
                                                bytes[off + 2],
                                                bytes[off + 3],
                                            ]) as f32
                                                / i32::MAX as f32
                                        }
                                        SampleFormat::S24 => {
                                            let raw = (bytes[off] as i32)
                                                | ((bytes[off + 1] as i32) << 8)
                                                | ((bytes[off + 2] as i32) << 16);
                                            let v = (raw << 8) >> 8;
                                            v as f32 / 8_388_608.0
                                        }
                                        SampleFormat::S24_32 => {
                                            let v = i32::from_le_bytes([
                                                bytes[off],
                                                bytes[off + 1],
                                                bytes[off + 2],
                                                bytes[off + 3],
                                            ]);
                                            v as f32 / i32::MAX as f32
                                        }
                                        SampleFormat::S16 => {
                                            i16::from_le_bytes([bytes[off], bytes[off + 1]]) as f32
                                                / i16::MAX as f32
                                        }
                                        SampleFormat::Unknown => 0.0,
                                    };
                                }
                            }
                            for s in data.scratch.iter().take(frames * ch) {
                                if data.tx.push(*s).is_err() {
                                    xrun = true;
                                    break;
                                }
                            }
                        }
                    }
                    // Buffer returns to the pool on drop (no explicit queue).
                } else {
                    xrun = true;
                }
                record(&data.shared.stats_cap, t0, xrun);
            })
            .register()
            .map_err(|e| AudioError::Backend(format!("capture listener: {e:?}")))?;

        let vad_l = vad_stream
            .add_local_listener_with_user_data(vad_data)
            .param_changed(|_, data, id, param| {
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else { return };
                let mut info = spa::param::audio::AudioInfoRaw::new();
                if info.parse(param).is_ok() {
                    let f32ok = info.format() == spa::param::audio::AudioFormat::F32LE;
                    if !f32ok {
                        eprintln!(
                            "vm-audio WARN: vad negotiated non-F32 format {:?} (conversion missing!)",
                            info.format()
                        );
                    }
                    *data.shared.fmt_out.lock().unwrap() = FormatState {
                        rate: info.rate(),
                        channels: info.channels() as usize,
                        format: classify_format(info.format()),
                    };
                }
            })
            .state_changed(|_, data, _old, new| {
                if matches!(new, pw::stream::StreamState::Error(_)) {
                    *data.shared.link.lock().unwrap() = StreamLinkState::NeedsRestart;
                }
            })
            .process(|stream, data| {
                let t0 = Instant::now();
                let mut xrun = false;
                if let Some(mut buffer) = stream.dequeue_buffer() {
                    {
                        static BLEN2: std::sync::atomic::AtomicUsize =
                            std::sync::atomic::AtomicUsize::new(0);
                        if BLEN2.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 200 == 0 {
                            let nd = buffer.datas_mut().len();
                            let lens: Vec<usize> = buffer
                                .datas_mut()
                                .iter_mut()
                                .map(|d| d.data().map(|b| b.len()).unwrap_or(usize::MAX))
                                .collect();
                            eprintln!("VADBUF2 datas={nd} lens={lens:?}");
                        }
                    }
                    let (ch, rate) = data
                        .shared
                        .fmt_out
                        .lock()
                        .map(|f| (f.channels.max(1), f.rate))
                        .unwrap_or((2, SAMPLE_RATE));
                    let cap_ch = data
                        .shared
                        .fmt_cap
                        .lock()
                        .map(|f| f.channels.max(1))
                        .unwrap_or(ch);
                    for data_chunk in buffer.datas_mut() {
                        if let Some(bytes) = data_chunk.data() {
                            let capacity = bytes.len() / (4 * ch);
                            // One canonical quantum per callback. Writing the
                            // whole preallocated max-buffer overshoots the
                            // stream quantum and builds ever-growing backlog
                            // downstream (monitor then plays the past).
                            let frames = (QUANTUM_FRAMES as usize).min(capacity).max(1);
                            data.scratch_raw.resize(frames * cap_ch, 0.0);
                            for s in data.scratch_raw.iter_mut() {
                                match data.rx.pop() {
                                    Ok(v) => *s = v,
                                    Err(_) => {
                                        *s = 0.0;
                                        xrun = true;
                                    }
                                }
                            }
                            // Planar staging for the graph (stereo-fixed lanes):
                            // duplicate mono up, mix wider captures down.
                            data.scratch_in.resize(MAX_CHANNELS, Vec::new());
                            data.scratch_out.resize(MAX_CHANNELS, Vec::new());
                            for stage in data.scratch_in.iter_mut() {
                                stage.resize(frames, 0.0);
                                if cap_ch == 1 {
                                    stage.copy_from_slice(&data.scratch_raw[..frames]);
                                } else {
                                    for f in 0..frames {
                                        let mut acc = 0.0;
                                        for c in 0..cap_ch {
                                            acc += data.scratch_raw[f * cap_ch + c];
                                        }
                                        stage[f] = acc / cap_ch as f32;
                                    }
                                }
                            }
                            for stage in data.scratch_out.iter_mut() {
                                stage.resize(frames, 0.0);
                            }
                            match data.processor.try_lock() {
                                Ok(mut proc) => {
                                    let ins: Vec<&[f32]> =
                                        data.scratch_in.iter().map(Vec::as_slice).collect();
                                    let mut outs: Vec<&mut [f32]> = data
                                        .scratch_out
                                        .iter_mut()
                                        .map(Vec::as_mut_slice)
                                        .collect();
                                    proc.process(rate, &ins, &mut outs);
                                }
                                Err(_) => {
                                    // Contended: pass through (never block RT).
                                    // Counted as watchdog trip (diagnostic).
                                    if let Ok(mut s) = data.shared.stats_vad.lock() {
                                        s.watchdog_trips += 1;
                                    }
                                    for (o, i) in
                                        data.scratch_out.iter_mut().zip(data.scratch_in.iter())
                                    {
                                        o.copy_from_slice(i);
                                    }
                                    xrun = true;
                                }
                            }
                            {
                                static OPEAK: std::sync::atomic::AtomicUsize =
                                    std::sync::atomic::AtomicUsize::new(0);
                                if OPEAK.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 300 == 0 {
                                    let mx = data
                                        .scratch_out
                                        .iter()
                                        .flat_map(|l| l.iter())
                                        .fold(0.0f32, |m, s| m.max(s.abs()));
                                    eprintln!("OPEAK graph-out peak={mx:.4} rate={rate} frames={frames}");
                                }
                            }
                            for f in 0..frames {
                                for c in 0..ch {
                                    let v = data.scratch_out[c % MAX_CHANNELS][f];
                                    let off = (f * ch + c) * 4;
                                    bytes[off..off + 4].copy_from_slice(&v.to_le_bytes());
                                }
                            }
                            {
                                static IPEAK: std::sync::atomic::AtomicUsize =
                                    std::sync::atomic::AtomicUsize::new(0);
                                if IPEAK.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 300 == 0 {
                                    let mx = bytes
                                        .chunks_exact(4)
                                        .map(|w| f32::from_le_bytes([w[0], w[1], w[2], w[3]]).abs())
                                        .fold(0.0f32, f32::max);
                                    eprintln!("IPEAK interleaved peak={mx:.4} frames={frames}");
                                }
                            }
                            // Publish exactly what we wrote; the rest of the
                            // preallocated buffer is stale memory, never signal.
                            let chunk = data_chunk.chunk_mut();
                            *chunk.offset_mut() = 0;
                            *chunk.stride_mut() = (ch * 4) as _;
                            *chunk.size_mut() = (frames * ch * 4) as _;
                            if let Some(mon) = data.mon_tx.as_mut() {
                                for f in 0..frames {
                                    for c in 0..ch {
                                        if mon.push(data.scratch_out[c % MAX_CHANNELS][f]).is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Buffer returns to the pool on drop (no explicit queue).
                } else {
                    xrun = true;
                }
                record(&data.shared.stats_vad, t0, xrun);
            })
            .register()
            .map_err(|e| AudioError::Backend(format!("vad listener: {e:?}")))?;

        let mon_l = mon_stream
            .add_local_listener_with_user_data(mon_data)
            .process(|stream, data| {
                let t0 = Instant::now();
                let mut xrun = false;
                if let Some(mut buffer) = stream.dequeue_buffer() {
                    let ch =
                        data.shared.fmt_out.lock().map(|f| f.channels.max(1)).unwrap_or(2);
                    for data_chunk in buffer.datas_mut() {
                        if let Some(bytes) = data_chunk.data() {
                            let capacity = bytes.len() / (4 * ch);
                            // One canonical quantum per callback (see vad).
                            let frames = (QUANTUM_FRAMES as usize).min(capacity).max(1);
                            data.scratch.resize(frames * ch, 0.0);
                            for s in data.scratch.iter_mut() {
                                *s = data.rx.pop().unwrap_or_else(|_| {
                                    xrun = true;
                                    0.0
                                });
                            }
                            for (i, word) in bytes.chunks_exact_mut(4).enumerate().take(frames * ch) {
                                word.copy_from_slice(&data.scratch[i].to_le_bytes());
                            }
                            let chunk = data_chunk.chunk_mut();
                            *chunk.offset_mut() = 0;
                            *chunk.stride_mut() = (ch * 4) as _;
                            *chunk.size_mut() = (frames * ch * 4) as _;
                        }
                    }
                    // Buffer returns to the pool on drop (no explicit queue).
                } else {
                    xrun = true;
                }
                record(&data.shared.stats_mon, t0, xrun);
            })
            .register()
            .map_err(|e| AudioError::Backend(format!("monitor listener: {e:?}")))?;

        // Format pods must outlive negotiation: worker owns them.
        let pod_cap = format_pod(mic_ch)?;
        let pod_out = format_pod(MAX_CHANNELS as u32)?;
        let pod_mon = format_pod(MAX_CHANNELS as u32)?;
        let mut params_cap = [pod_cap; 1];
        let mut params_out = [pod_out; 1];
        let mut params_mon = [pod_mon; 1];
        use pw::stream::StreamFlags as F;
        // No AUTOCONNECT: mic→capture links are created explicitly below.
        // (Leaving it on makes WirePlumber double-link extra sources.)
        cap_stream
            .connect(
                spa::utils::Direction::Input,
                None,
                F::MAP_BUFFERS | F::RT_PROCESS,
                &mut params_cap,
            )
            .map_err(|e| AudioError::Backend(format!("capture connect: {e:?}")))?;
        // Explicit targets: no AUTOCONNECT on routed streams.
        vad_stream
            .connect(
                spa::utils::Direction::Output,
                None,
                F::MAP_BUFFERS | F::RT_PROCESS,
                &mut params_out,
            )
            .map_err(|e| AudioError::Backend(format!("vad connect: {e:?}")))?;
        let mon_flags = if mon_sink.is_some() {
            F::MAP_BUFFERS | F::RT_PROCESS
        } else {
            F::AUTOCONNECT | F::MAP_BUFFERS | F::RT_PROCESS
        };
        mon_stream
            .connect(
                spa::utils::Direction::Output,
                None,
                mon_flags,
                &mut params_mon,
            )
            .map_err(|e| AudioError::Backend(format!("monitor connect: {e:?}")))?;

        // Node ids are assigned asynchronously by the server: pump until
        // valid (INVALID = 0xFFFFFFFF) before reading ports.
        // Read before the streams move into storage.
        let mut cap_node_id = cap_stream.node_id();
        let mut mon_node_id = mon_stream.node_id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while cap_node_id == u32::MAX && Instant::now() < deadline {
            let _ = loop_.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
            cap_node_id = cap_stream.node_id();
        }
        if cap_node_id == u32::MAX {
            return Err(AudioError::Backend("capture stream node id never assigned".into()));
        }
        let mut vad_node_id = vad_stream.node_id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while (vad_node_id == u32::MAX || mon_node_id == u32::MAX || cap_node_id == u32::MAX)
            && Instant::now() < deadline
        {
            let _ = loop_.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
            vad_node_id = vad_stream.node_id();
            mon_node_id = mon_stream.node_id();
            cap_node_id = cap_stream.node_id();
        }
        if mon_node_id == u32::MAX {
            return Err(AudioError::Backend("monitor stream node id never assigned".into()));
        }
        if vad_node_id == u32::MAX {
            return Err(AudioError::Backend("vad stream node id never assigned".into()));
        }
        self.streams = Some(LiveStreams {
            _capture: cap_stream,
            _vad: vad_stream,
            _monitor: mon_stream,
            _cap_l: cap_l,
            _vad_l: vad_l,
            _mon_l: mon_l,
            _pods: vec![pod_cap, pod_out, pod_mon],
        });
        // Explicit links, both directions (deterministic; no dependence on
        // session-manager timing; TARGET_OBJECT dropped from props so the
        // session manager never doubles them).
        self.link_ports(core, loop_, req.input_id, cap_node_id)?;
        self.link_vad_to_vmic(core, loop_, req.vm_id, vad_node_id)?;
        if let Some(sink_id) = mon_sink {
            if self.link_ports(core, loop_, mon_node_id, sink_id).is_err() {
                eprintln!("vm-audio: explicit monitor link failed");
            }
        }
        // Manually-linked streams don't self-activate: kick all three.
        // (Autoconnected streams start on their own; this is a no-op for them.)
        if let Some(s) = self.streams.as_ref() {
            let _ = s._capture.set_active(true);
            let _ = s._vad.set_active(true);
            let _ = s._monitor.set_active(true);
        }
        *self.shared.link.lock().unwrap() = StreamLinkState::Streaming;
        Ok(())
    }

    /// Count the mic's output ports (1 = mono, 2 = stereo), waiting for
    /// them to appear. Caps at 2 (wider devices mix down in the callback).
    fn mic_channel_count(
        &mut self,
        loop_: &pw::loop_::Loop,
        mic_id: u32,
    ) -> Result<u32, AudioError> {
        let mut count = 0u32;
        self.pump_until(loop_, Duration::from_secs(10), || {
            let map = match self.ports.lock() {
                Ok(m) => m,
                Err(_) => return false,
            };
            count = map.values().filter(|p| p.node_id == mic_id && p.direction == "out").count() as u32;
            count >= 1
        });
        if count < 1 {
            return Err(AudioError::Backend(format!("mic {mic_id} exposes no output ports")));
        }
        Ok(count.min(MAX_CHANNELS as u32))
    }

    /// Explicit links between two nodes, matched by channel index:
    /// `from` outputs → `to` inputs.
    fn link_ports(
        &mut self,
        core: &pw::core::CoreRc,
        loop_: &pw::loop_::Loop,
        from_node: u32,
        to_node: u32,
    ) -> Result<(), AudioError> {
        let mut out_ports: Vec<u32> = Vec::new();
        let mut in_ports: Vec<u32> = Vec::new();
        self.pump_until(loop_, Duration::from_secs(10), || {
            let map = match self.ports.lock() {
                Ok(m) => m,
                Err(_) => return false,
            };
            let mut o: Vec<u32> = map
                .values()
                .filter(|p| p.node_id == from_node && p.direction == "out")
                .map(|p| p.id)
                .collect();
            o.sort_unstable();
            out_ports = o;
            let mut i: Vec<u32> = map
                .values()
                .filter(|p| p.node_id == to_node && p.direction == "in")
                .map(|p| p.id)
                .collect();
            i.sort_unstable();
            in_ports = i;
            out_ports.len() >= 2 && in_ports.len() >= 2
        });
        let n = out_ports.len().min(in_ports.len());
        if n < 1 {
            return Err(AudioError::Backend(format!(
                "ports not visible (from={from_node}:{}, to={to_node}:{})",
                out_ports.len(),
                in_ports.len()
            )));
        }
        for (out_port, in_port) in out_ports.iter().zip(in_ports.iter()).take(n) {
            let link: pw::link::Link = core
                .create_object(
                    "link-factory",
                    &properties! {
                        *pw::keys::LINK_OUTPUT_NODE => from_node.to_string(),
                        *pw::keys::LINK_OUTPUT_PORT => out_port.to_string(),
                        *pw::keys::LINK_INPUT_NODE => to_node.to_string(),
                        *pw::keys::LINK_INPUT_PORT => in_port.to_string(),
                    },
                )
                .map_err(|e| AudioError::Backend(format!("create link: {e:?}")))?;
            self.links.push(link);
        }
        Ok(())
    }

    /// Create output→input links from our vad stream ports to the vm-mic
    /// sink ports, matched by channel index (FL→playback_1, FR→playback_2).
    fn link_vad_to_vmic(
        &mut self,
        core: &pw::core::CoreRc,
        loop_: &pw::loop_::Loop,
        vm_id: u32,
        vad_node_id: u32,
    ) -> Result<(), AudioError> {
        self.link_ports(core, loop_, vad_node_id, vm_id)
    }

    fn start_streams(&mut self) -> Result<(), AudioError> {
        // PipeWire streams flow once connected; `open` already connected.
        if self.streams.is_none() {
            return Err(AudioError::NoStream);
        }
        Ok(())
    }

    fn stop_streams(&mut self, _reason: StreamStopReason) {
        // Dropping streams AND link proxies disconnects everything; the
        // worker keeps the loop alive for re-open (mirrors the reference
        // restart path).
        self.links.clear();
        self.streams = None;
        *self.shared.link.lock().unwrap() = StreamLinkState::Down;
    }
}

#[cfg(test)]
mod pod_tests {
    use super::format_pod;

    #[test]
    fn format_pod_parses_back_to_f32_stereo_48k() {
        // The exact consumer the server drives: AudioInfoRaw::parse.
        for ch in [1u32, 2u32] {
            let pod = format_pod(ch).expect("build pod");
            let mut info = pipewire::spa::param::audio::AudioInfoRaw::new();
            info.parse(&pod).expect("parse pod");
            assert_eq!(info.rate(), 48_000, "rate for {ch}ch");
            assert_eq!(info.channels(), ch, "channels");
            assert_eq!(
                info.format(),
                pipewire::spa::param::audio::AudioFormat::F32LE,
                "format"
            );
        }
    }
}
