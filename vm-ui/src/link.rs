//! Background WebSocket link: dedicated thread + Tokio runtime.
//!
//! The egui thread never touches the network: it reads an `Arc<Mutex>`
//! snapshot and posts actions through an mpsc queue. Replies and
//! broadcasts both funnel back into the snapshot (+ repaint).

use futures_util::{SinkExt, StreamExt};
use std::sync::{mpsc, Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub uid: String,
    pub name: String,
    pub role: String,
    pub rate: u32,
    pub channels: u32,
}

#[derive(Debug, Clone, Default)]
pub struct StatusInfo {
    pub active: bool,
    pub link: String,
    pub xruns: u64,
    pub avg_us: f64,
}

#[derive(Debug, Clone, Default)]
pub struct NodeHealth {
    pub node: String,
    pub avg_us: f64,
    pub max_us: u64,
    pub calls: u64,
    /// Algorithmic latency in ms (group delay, not CPU cost).
    pub lat_ms: f64,
}

#[derive(Debug, Clone, Default)]
pub struct StreamHealth {
    pub invocations: u64,
    pub avg_us: f64,
    pub max_us: u64,
    pub xruns: u64,
    pub trips: u64,
    pub last_frames: usize,
}

#[derive(Debug, Clone, Default)]
pub struct HealthInfo {
    pub voice: String,
    pub link: String,
    pub active: bool,
    pub graph_xruns: u64,
    pub nodes: Vec<NodeHealth>,
    pub capture: StreamHealth,
    pub playback: StreamHealth,
    pub monitor: StreamHealth,
    pub restarts: u64,
    pub last_reason: String,
    pub last_restart_unix: u64,
    /// Chain algorithmic latency budget in ms.
    pub latency_ms: f64,
}

/// Hear-myself monitor (F-0.4.C 15).
#[derive(Debug, Clone, Default)]
pub struct MonitorState {
    pub enabled: bool,
    pub device: String,
    pub mix: f32,
    pub volume_db: f32,
    pub muted: bool,
}

/// Sidechain ducking (F-0.4.C 14).
#[derive(Debug, Clone, Default)]
pub struct DuckingState {
    pub enabled: bool,
    pub depth_db: f32,
    pub threshold_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

/// Instant replay (F-0.4.B 20).
#[derive(Debug, Clone, Default)]
pub struct ReplayState {
    pub enabled: bool,
    pub seconds: u32,
    pub buffered_seconds: f32,
}

/// Per-application passthrough (F-0.4.B 18).
#[derive(Debug, Clone, Default)]
pub struct PassthroughState {
    pub enabled: bool,
    pub device: String,
    pub app: String,
}

/// One application output stream (passthrough picker).
#[derive(Debug, Clone, Default)]
pub struct AppEntry {
    pub app: String,
    pub name: String,
    pub sink: String,
}

/// obs-websocket bridge status (F-0.4.C 35).
#[derive(Debug, Clone, Default)]
pub struct ObsState {
    pub connected: bool,
    pub scene: String,
    pub url: String,
    pub enabled: bool,
}

/// MIDI learn status (F-0.4.C 37).
#[derive(Debug, Clone, Default)]
pub struct MidiState {
    pub connected: bool,
    pub learning: bool,
    pub port: String,
    pub bindings: u32,
    pub enabled: bool,
}

/// Live level snapshot (wizard meters + VU).
#[derive(Debug, Clone, Default)]
pub struct LevelsState {
    pub mic_db: f32,
    pub out_db: f32,
    pub speech: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub port: u16,
    pub devices: Vec<DeviceInfo>,
    pub sounds: Vec<String>,
    pub status: StatusInfo,
    pub health: HealthInfo,
    pub tier: String,
    pub voice_enabled: bool,
    /// Master mute (tray menu checkmark; fed by `mutedChanged` + health).
    pub muted: bool,
    pub selected_voice: String,
    pub last_error: String,
    pub monitor: MonitorState,
    pub ducking: DuckingState,
    pub replay: ReplayState,
    pub passthrough: PassthroughState,
    pub passthrough_apps: Vec<AppEntry>,
    pub obs: ObsState,
    pub midi: MidiState,
    pub levels: LevelsState,
    /// Last saved replay path (`replaySaved` event).
    pub replay_saved_path: String,
}

impl Snapshot {
    fn set_error(&mut self, e: impl Into<String>) {
        self.last_error = e.into();
    }
}

pub enum ToLink {
    /// Raw action object, e.g. `{"action":"playSound","sound":"x"}`.
    Action(serde_json::Value),
    /// Force a status refresh now.
    Refresh,
    /// Poll interval flag changed (wake the loop to apply it).
    FastLevels,
}

pub struct UiLink {
    tx: mpsc::Sender<ToLink>,
    pub snapshot: Arc<Mutex<Snapshot>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    /// Fast `getLevels` polling (wizard meters); 100 ms instead of the
    /// regular 2 s status cadence.
    fast: Arc<std::sync::atomic::AtomicBool>,
}

impl UiLink {
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<ToLink>();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let snap = snapshot.clone();
        let next_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let fast = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fast_thread = fast.clone();
        std::thread::Builder::new()
            .name("vm-ws".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("link runtime");
                rt.block_on(link_main(rx, snap, Arc::new(repaint), fast_thread));
            })
            .ok();
        Self { tx, snapshot, next_id, fast }
    }

    pub fn send_action(&self, mut action: serde_json::Value) {
        // Attach a request id for strict reply correlation; servers that
        // predate ids simply ignore it (backward compatible).
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(obj) = action.as_object_mut() {
            obj.insert("id".to_string(), serde_json::Value::from(id));
        }
        let _ = self.tx.send(ToLink::Action(action));
    }

    pub fn refresh(&self) {
        let _ = self.tx.send(ToLink::Refresh);
    }

    /// Fast level polling for the wizard meters (and to wake the loop now).
    pub fn set_fast_levels(&self, on: bool) {
        self.fast.store(on, std::sync::atomic::Ordering::Relaxed);
        let _ = self.tx.send(ToLink::FastLevels);
    }
}

fn update_snap<F: FnOnce(&mut Snapshot)>(snap: &Arc<Mutex<Snapshot>>, repaint: &Arc<dyn Fn()>, f: F) {
    if let Ok(mut s) = snap.lock() {
        f(&mut s);
    }
    repaint();
}

async fn link_main(
    rx: mpsc::Receiver<ToLink>,
    snap: Arc<Mutex<Snapshot>>,
    repaint: Arc<dyn Fn()>,
    fast: Arc<std::sync::atomic::AtomicBool>,
) {
    let token = std::env::var("VM_CONTROL_TOKEN").unwrap_or_default();
    loop {
        match connect(&snap, &repaint).await {
            Ok((mut ws, port)) => {
                update_snap(&snap, &repaint, |s| {
                    s.connected = true;
                    s.port = port;
                    s.last_error.clear();
                });
                // Bootstrap.
                for boot in [
                    serde_json::json!({"action": "getAudioDevices"}),
                    serde_json::json!({"action": "getAllSoundboard"}),
                    serde_json::json!({"action": "getAudioStreamStatus"}),
                    serde_json::json!({"action": "getHealth"}),
                    serde_json::json!({"action": "getUserLicense"}),
                    serde_json::json!({"action": "getMonitor"}),
                    serde_json::json!({"action": "getDucking"}),
                    serde_json::json!({"action": "getInstantReplay"}),
                    serde_json::json!({"action": "getPassthrough"}),
                    serde_json::json!({"action": "getObsStatus"}),
                    serde_json::json!({"action": "getMidiStatus"}),
                ] {
                    if !request(&mut ws, &snap, &repaint, &token, boot).await {
                        break;
                    }
                }
                // Serve the queue + periodic refresh until the socket dies.
                loop {
                    let fast_now = fast.load(std::sync::atomic::Ordering::Relaxed);
                    let poll = if fast_now {
                        std::time::Duration::from_millis(100)
                    } else {
                        std::time::Duration::from_secs(2)
                    };
                    let alive = match rx.recv_timeout(poll) {
                        Ok(ToLink::Action(a)) => {
                            request(&mut ws, &snap, &repaint, &token, a).await
                        }
                        Ok(ToLink::FastLevels) => true,
                        Ok(ToLink::Refresh) | Err(mpsc::RecvTimeoutError::Timeout) => {
                            // Wizard open (fast): cheap level polls only;
                            // otherwise the regular status + health cadence.
                            if fast.load(std::sync::atomic::Ordering::Relaxed) {
                                request(
                                    &mut ws,
                                    &snap,
                                    &repaint,
                                    &token,
                                    serde_json::json!({"action": "getLevels"}),
                                )
                                .await
                            } else if request(
                                &mut ws,
                                &snap,
                                &repaint,
                                &token,
                                serde_json::json!({"action": "getAudioStreamStatus"}),
                            )
                            .await
                            {
                                request(
                                    &mut ws,
                                    &snap,
                                    &repaint,
                                    &token,
                                    serde_json::json!({"action": "getHealth"}),
                                )
                                .await
                            } else {
                                false
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    };
                    if !alive {
                        break; // socket dead -> outer loop reconnects
                    }
                }
            }
            Err(e) => {
                update_snap(&snap, &repaint, |s| {
                    s.connected = false;
                    s.set_error(format!("ws: {e} (is vm-daemon running?)"));
                });
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    _snap: &Arc<Mutex<Snapshot>>,
    _repaint: &Arc<dyn Fn()>,
) -> Result<(Ws, u16), String> {
    for port in [8765u16, 8766, 8767, 8768, 8769] {
        match tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}")).await {
            Ok((ws, _)) => return Ok((ws, port)),
            Err(_) => continue,
        }
    }
    Err("no Control API port reachable".to_string())
}

/// Send one action; consume messages until the reply (`"ok"` key) arrives.
/// Broadcasts (`"event"` key) update the snapshot along the way.
async fn request(
    ws: &mut Ws,
    snap: &Arc<Mutex<Snapshot>>,
    repaint: &Arc<dyn Fn()>,
    token: &str,
    mut action: serde_json::Value,
) -> bool {
    if !token.is_empty() {
        if let Some(obj) = action.as_object_mut() {
            obj.insert("token".to_string(), serde_json::Value::String(token.to_string()));
        }
    }
    let text = action.to_string();
    if ws.send(Message::Text(text.into())).await.is_err() {
        update_snap(snap, repaint, |s| s.connected = false);
        return false;
    }
    let want_id = action.get("id").cloned();
    loop {
        let msg = match tokio::time::timeout(std::time::Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => {
                update_snap(snap, repaint, |s| s.connected = false);
                return false;
            }
        };
        if !msg.is_text() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(msg.to_text().unwrap_or("")) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("event").is_some() {
            apply_event(snap, repaint, &v);
            continue;
        }
        if v.get("ok").is_some() {
            // Strict correlation when both sides speak ids; accept
            // id-less replies for backward compatibility.
            let got_id = v.get("id").cloned();
            if want_id.is_some() && got_id.is_some() && want_id != got_id {
                continue;
            }
            apply_reply(snap, repaint, &action, &v);
            return true;
        }
    }
}

fn apply_event(snap: &Arc<Mutex<Snapshot>>, repaint: &Arc<dyn Fn()>, v: &serde_json::Value) {
    update_snap(snap, repaint, |s| {
        match v.get("event").and_then(|e| e.as_str()) {
            Some("voiceChanged") => {
                s.selected_voice = v.get("voice").and_then(|x| x.as_str()).unwrap_or("").to_string();
            }
            Some("voiceChangerToggled") => {
                s.voice_enabled = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true);
            }
            Some("mutedChanged") => {
                s.muted = v.get("muted").and_then(|x| x.as_bool()).unwrap_or(false);
            }
            Some("monitorChanged") => {
                s.monitor = MonitorState {
                    enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
                    device: v.get("device").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    mix: v.get("mix").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32,
                    volume_db: v.get("volume_db").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32,
                    muted: v.get("muted").and_then(|x| x.as_bool()).unwrap_or(false),
                };
            }
            Some("duckingChanged") => {
                s.ducking = DuckingState {
                    enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
                    depth_db: v.get("depth_db").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32,
                    threshold_db: v.get("threshold_db").and_then(|x| x.as_f64()).unwrap_or(0.0)
                        as f32,
                    attack_ms: v.get("attack_ms").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32,
                    release_ms: v.get("release_ms").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32,
                };
            }
            Some("replaySaved") => {
                s.replay_saved_path = v.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string();
            }
            Some("passthroughChanged") => {
                s.passthrough = PassthroughState {
                    enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
                    device: v.get("device").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    app: v.get("app").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                };
            }
            Some("obsStatusChanged") => {
                s.obs.connected = v.get("connected").and_then(|x| x.as_bool()).unwrap_or(false);
                s.obs.scene = v.get("scene").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if let Some(u) = v.get("url").and_then(|x| x.as_str()) {
                    if !u.is_empty() {
                        s.obs.url = u.to_string();
                    }
                }
            }
            Some("midiBindingsChanged") => {
                // Count is stale until the next getMidiStatus; leave it.
            }
            _ => {}
        }
    });
}

fn apply_reply(
    snap: &Arc<Mutex<Snapshot>>,
    repaint: &Arc<dyn Fn()>,
    action: &serde_json::Value,
    reply: &serde_json::Value,
) {
    update_snap(snap, repaint, |s| {
        if reply.get("ok").and_then(|x| x.as_bool()) != Some(true) {
            s.set_error(
                reply.get("error").and_then(|x| x.as_str()).unwrap_or("action failed").to_string(),
            );
            return;
        }
        s.last_error.clear();
        let name = action.get("action").and_then(|x| x.as_str()).unwrap_or("");
        let data = reply.get("data");
        match name {
            "getAudioDevices" => {
                s.devices = data
                    .and_then(|d| d.get("devices"))
                    .and_then(|d| d.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|d| DeviceInfo {
                                uid: d.get("uid").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                name: d.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                role: d.get("role").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                rate: d.get("rate").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                                channels: d.get("channels").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "getAllSoundboard" => {
                s.sounds = data
                    .and_then(|d| d.get("sounds"))
                    .and_then(|d| d.as_array())
                    .map(|arr| {
                        arr.iter().filter_map(|x| x.as_str().map(|x| x.to_string())).collect()
                    })
                    .unwrap_or_default();
            }
            "getUserLicense" => {
                s.tier = data
                    .and_then(|d| d.get("tier"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
            }
            "getAudioStreamStatus" => {
                s.status = StatusInfo {
                    active: data.and_then(|d| d.get("active")).and_then(|x| x.as_bool()).unwrap_or(false),
                    link: data.and_then(|d| d.get("link")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    xruns: data.and_then(|d| d.get("xruns")).and_then(|x| x.as_u64()).unwrap_or(0),
                    avg_us: data.and_then(|d| d.get("avgCallbackUs")).and_then(|x| x.as_f64()).unwrap_or(0.0),
                };
            }
            "getHealth" => {
                s.health = parse_health(data);
                s.muted = data.and_then(|d| d.get("muted")).and_then(|x| x.as_bool()).unwrap_or(false);
                s.voice_enabled =
                    data.and_then(|d| d.get("enabled")).and_then(|x| x.as_bool()).unwrap_or(true);
            }
            "getMonitor" => {
                s.monitor = MonitorState {
                    enabled: data.and_then(|d| d.get("enabled")).and_then(|x| x.as_bool()).unwrap_or(false),
                    device: str_of(data, "device"),
                    mix: fnum(data.and_then(|d| d.get("mix"))) as f32,
                    volume_db: fnum(data.and_then(|d| d.get("volume_db"))) as f32,
                    muted: data.and_then(|d| d.get("muted")).and_then(|x| x.as_bool()).unwrap_or(false),
                };
            }
            "getDucking" => {
                s.ducking = DuckingState {
                    enabled: data.and_then(|d| d.get("enabled")).and_then(|x| x.as_bool()).unwrap_or(false),
                    depth_db: fnum(data.and_then(|d| d.get("depth_db"))) as f32,
                    threshold_db: fnum(data.and_then(|d| d.get("threshold_db"))) as f32,
                    attack_ms: fnum(data.and_then(|d| d.get("attack_ms"))) as f32,
                    release_ms: fnum(data.and_then(|d| d.get("release_ms"))) as f32,
                };
            }
            "getInstantReplay" | "setInstantReplay" => {
                s.replay = ReplayState {
                    enabled: data.and_then(|d| d.get("enabled")).and_then(|x| x.as_bool()).unwrap_or(false),
                    seconds: data.and_then(|d| d.get("seconds")).and_then(|x| x.as_u64()).unwrap_or(30) as u32,
                    buffered_seconds: fnum(data.and_then(|d| d.get("buffered_seconds"))) as f32,
                };
            }
            "saveInstantReplay" => {
                s.replay_saved_path = str_of(data, "path");
            }
            "getPassthrough" => {
                s.passthrough = PassthroughState {
                    enabled: data.and_then(|d| d.get("enabled")).and_then(|x| x.as_bool()).unwrap_or(false),
                    device: str_of(data, "device"),
                    app: str_of(data, "app"),
                };
            }
            "listPassthroughApps" => {
                s.passthrough_apps = data
                    .and_then(|d| d.get("apps"))
                    .and_then(|d| d.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|a| AppEntry {
                                app: str_of(Some(a), "app"),
                                name: str_of(Some(a), "name"),
                                sink: str_of(Some(a), "sink"),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "getObsStatus" => {
                s.obs = ObsState {
                    connected: data.and_then(|d| d.get("connected")).and_then(|x| x.as_bool()).unwrap_or(false),
                    scene: str_of(data, "scene"),
                    url: str_of(data, "url"),
                    enabled: data.and_then(|d| d.get("enabled")).and_then(|x| x.as_bool()).unwrap_or(false),
                };
            }
            "getMidiStatus" => {
                s.midi = MidiState {
                    connected: data.and_then(|d| d.get("connected")).and_then(|x| x.as_bool()).unwrap_or(false),
                    learning: data.and_then(|d| d.get("learning")).and_then(|x| x.as_bool()).unwrap_or(false),
                    port: str_of(data, "port"),
                    bindings: data.and_then(|d| d.get("bindings")).and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                    enabled: data.and_then(|d| d.get("enabled")).and_then(|x| x.as_bool()).unwrap_or(false),
                };
            }
            "getLevels" => {
                s.levels = LevelsState {
                    mic_db: fnum(data.and_then(|d| d.get("mic_db"))) as f32,
                    out_db: fnum(data.and_then(|d| d.get("out_db"))) as f32,
                    speech: data.and_then(|d| d.get("speech")).and_then(|x| x.as_bool()).unwrap_or(false),
                };
            }
            _ => {}
        }
    });
}

fn num(v: Option<&serde_json::Value>) -> u64 {
    v.and_then(|x| x.as_u64()).unwrap_or(0)
}

fn fnum(v: Option<&serde_json::Value>) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn str_of(data: Option<&serde_json::Value>, key: &str) -> String {
    data.and_then(|d| d.get(key)).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn parse_stream(data: Option<&serde_json::Value>) -> StreamHealth {
    let g = |key: &str| data.and_then(|d| d.get(key));
    StreamHealth {
        invocations: num(g("invocations")),
        avg_us: fnum(g("avgUs")),
        max_us: num(g("maxUs")),
        xruns: num(g("xruns")),
        trips: num(g("watchdogTrips")),
        last_frames: num(g("lastFrames")) as usize,
    }
}

/// Map a `getHealth` data payload into snapshot state. Missing keys
/// degrade to zeros (older daemons simply have no `getHealth`).
fn parse_health(data: Option<&serde_json::Value>) -> HealthInfo {
    let nodes = data
        .and_then(|d| d.get("nodes"))
        .and_then(|n| n.as_array())
        .map(|arr| {
            arr.iter()
                .map(|n| NodeHealth {
                    node: n.get("node").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
                    avg_us: fnum(n.get("avgUs")),
                    max_us: num(n.get("maxUs")),
                    calls: num(n.get("calls")),
                    lat_ms: fnum(n.get("latMs")),
                })
                .collect()
        })
        .unwrap_or_default();
    let wd = data.and_then(|d| d.get("watchdog"));
    HealthInfo {
        voice: str_of(data, "voice"),
        link: str_of(data, "link"),
        active: data.and_then(|d| d.get("active")).and_then(|x| x.as_bool()).unwrap_or(false),
        graph_xruns: num(data.and_then(|d| d.get("graphXruns"))),
        nodes,
        capture: parse_stream(data.and_then(|d| d.get("capture"))),
        playback: parse_stream(data.and_then(|d| d.get("playback"))),
        monitor: parse_stream(data.and_then(|d| d.get("monitor"))),
        restarts: num(wd.and_then(|w| w.get("restarts"))),
        last_reason: wd
            .and_then(|w| w.get("lastReason"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        last_restart_unix: num(wd.and_then(|w| w.get("lastRestartUnix"))),
        latency_ms: fnum(data.and_then(|d| d.get("latencyMs"))),
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    fn snap_link() -> Arc<Mutex<Snapshot>> {
        Arc::new(Mutex::new(Snapshot::default()))
    }
    fn snap_repaint() -> Arc<dyn Fn()> {
        Arc::new(|| {})
    }

    #[test]
    fn health_payload_maps_into_snapshot() {
        let snap = snap_link();
        let action = serde_json::json!({"action": "getHealth"});
        let reply = serde_json::json!({
            "ok": true,
            "data": {
                "active": true,
                "voice": "mic-npr-voice",
                "link": "Streaming",
                "graphXruns": 3,
                "latencyMs": 10.666,
                "muted": true,
                "enabled": false,
                "nodes": [
                    {"node": "denoise", "avgUs": 410.5, "maxUs": 900, "calls": 50, "latMs": 10.666}
                ],
                "capture": {"invocations": 50, "avgUs": 6.0, "maxUs": 20, "xruns": 0, "watchdogTrips": 0, "lastFrames": 960},
                "watchdog": {"restarts": 2, "lastReason": "xrun storm", "lastRestartUnix": 42, "consecutiveFailures": 0}
            }
        });
        apply_reply(&snap, &snap_repaint(), &action, &reply);
        let s = snap.lock().unwrap();
        assert_eq!(s.health.voice, "mic-npr-voice");
        assert_eq!(s.health.link, "Streaming");
        assert!(s.health.active);
        assert_eq!(s.health.graph_xruns, 3);
        assert!((s.health.latency_ms - 10.666).abs() < 1e-9);
        assert!(s.muted, "health seeds the tray mute checkmark");
        assert!(!s.voice_enabled);
        assert_eq!(s.health.nodes.len(), 1);
        assert_eq!(s.health.nodes[0].node, "denoise");
        assert!((s.health.nodes[0].avg_us - 410.5).abs() < 1e-9);
        assert_eq!(s.health.nodes[0].max_us, 900);
        assert!((s.health.nodes[0].lat_ms - 10.666).abs() < 1e-9);
        assert_eq!(s.health.capture.invocations, 50);
        assert_eq!(s.health.playback.xruns, 0); // missing block degrades to zero
        assert_eq!(s.health.restarts, 2);
        assert_eq!(s.health.last_reason, "xrun storm");
        assert_eq!(s.health.last_restart_unix, 42);
    }

    #[test]
    fn failed_reply_sets_error_and_keeps_health() {
        let snap = snap_link();
        let action = serde_json::json!({"action": "getHealth"});
        let reply = serde_json::json!({"ok": false, "error": "boom"});
        apply_reply(&snap, &snap_repaint(), &action, &reply);
        let s = snap.lock().unwrap();
        assert_eq!(s.last_error, "boom");
        assert!(s.health.nodes.is_empty());
    }

    #[test]
    fn integration_replies_map_into_snapshot() {
        let snap = snap_link();
        let rp = snap_repaint();
        for (action, reply) in [
            (
                serde_json::json!({"action": "getMonitor"}),
                serde_json::json!({"ok": true, "data": {"enabled": true, "device": "a#1", "mix": 0.5, "volume_db": -6.0, "muted": false}}),
            ),
            (
                serde_json::json!({"action": "getDucking"}),
                serde_json::json!({"ok": true, "data": {"enabled": true, "depth_db": 12.0, "threshold_db": -40.0, "attack_ms": 50.0, "release_ms": 400.0}}),
            ),
            (
                serde_json::json!({"action": "getInstantReplay"}),
                serde_json::json!({"ok": true, "data": {"enabled": true, "seconds": 30, "buffered_seconds": 12.5}}),
            ),
            (
                serde_json::json!({"action": "getPassthrough"}),
                serde_json::json!({"ok": true, "data": {"enabled": true, "device": "", "app": "Discord"}}),
            ),
            (
                serde_json::json!({"action": "listPassthroughApps"}),
                serde_json::json!({"ok": true, "data": {"apps": [{"id": 7, "app": "Discord", "name": "playback", "sink": "vm-passthrough"}]}}),
            ),
            (
                serde_json::json!({"action": "getMidiStatus"}),
                serde_json::json!({"ok": true, "data": {"connected": true, "learning": false, "port": "mpk", "bindings": 2}}),
            ),
            (
                serde_json::json!({"action": "getLevels"}),
                serde_json::json!({"ok": true, "data": {"mic_db": -22.5, "out_db": -20.0, "speech": true}}),
            ),
        ] {
            apply_reply(&snap, &rp, &action, &reply);
        }
        let s = snap.lock().unwrap();
        assert!(s.monitor.enabled);
        assert_eq!(s.monitor.device, "a#1");
        assert!((s.monitor.mix - 0.5).abs() < 1e-6);
        assert!(s.ducking.enabled);
        assert!((s.ducking.depth_db - 12.0).abs() < 1e-6);
        assert!(s.replay.enabled);
        assert_eq!(s.replay.seconds, 30);
        assert!((s.replay.buffered_seconds - 12.5).abs() < 1e-6);
        assert!(s.passthrough.enabled);
        assert_eq!(s.passthrough.app, "Discord");
        assert_eq!(s.passthrough_apps.len(), 1);
        assert_eq!(s.passthrough_apps[0].sink, "vm-passthrough");
        assert_eq!(s.midi.bindings, 2);
        assert!((s.levels.mic_db + 22.5).abs() < 1e-6);
        assert!(s.levels.speech);
    }

    #[test]
    fn events_update_integration_state() {
        let snap = snap_link();
        let rp = snap_repaint();
        apply_event(&snap, &rp, &serde_json::json!({"event": "replaySaved", "path": "/tmp/r.wav"}));
        apply_event(
            &snap,
            &rp,
            &serde_json::json!({"event": "passthroughChanged", "enabled": true, "device": "d", "app": "Firefox"}),
        );
        apply_event(
            &snap,
            &rp,
            &serde_json::json!({"event": "monitorChanged", "enabled": true, "device": "", "mix": 1.0, "volume_db": 0.0, "muted": true}),
        );
        let s = snap.lock().unwrap();
        assert_eq!(s.replay_saved_path, "/tmp/r.wav");
        assert!(s.passthrough.enabled);
        assert_eq!(s.passthrough.app, "Firefox");
        assert!(s.monitor.muted);
    }
}
