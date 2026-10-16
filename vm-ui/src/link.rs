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
    pub master_per_mille: u16,
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
}

pub struct UiLink {
    tx: mpsc::Sender<ToLink>,
    pub snapshot: Arc<Mutex<Snapshot>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl UiLink {
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<ToLink>();
        let snapshot = Arc::new(Mutex::new(Snapshot { master_per_mille: 1000, ..Default::default() }));
        let snap = snapshot.clone();
        let next_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
        std::thread::Builder::new()
            .name("vm-ws".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("link runtime");
                rt.block_on(link_main(rx, snap, Arc::new(repaint)));
            })
            .ok();
        Self { tx, snapshot, next_id }
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
                ] {
                    if !request(&mut ws, &snap, &repaint, &token, boot).await {
                        break;
                    }
                }
                // Serve the queue + periodic refresh until the socket dies.
                loop {
                    let alive = match rx.recv_timeout(std::time::Duration::from_secs(2)) {
                        Ok(ToLink::Action(a)) => {
                            request(&mut ws, &snap, &repaint, &token, a).await
                        }
                        Ok(ToLink::Refresh) | Err(mpsc::RecvTimeoutError::Timeout) => {
                            let ok = request(
                                &mut ws,
                                &snap,
                                &repaint,
                                &token,
                                serde_json::json!({"action": "getAudioStreamStatus"}),
                            )
                            .await;
                            if ok {
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

async fn ws_is_closed(ws: &mut Ws) -> bool {
    use futures_util::StreamExt as _;
    // Peek via a zero-timeout poll is complex; instead detect on next op.
    // True closure surfaces as an error inside `request`.
    let _ = ws;
    false
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
        update_snap(&snap, repaint, |s| s.connected = false);
        return false;
    }
    let want_id = action.get("id").cloned();
    loop {
        let msg = match tokio::time::timeout(std::time::Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => {
                update_snap(&snap, repaint, |s| s.connected = false);
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
    update_snap(&snap, &repaint, |s| {
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
    update_snap(&snap, &repaint, |s| {
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
}
