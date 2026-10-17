//! Global hotkeys via Linux `evdev` (replaces RawInput).
//!
//! Model mirrors `KeyBindsService`: per-target binds, press/release
//! trigger + edit capture, persisted by the caller.
//! NOTE: reading `/dev/input/event*` needs the `input` group or an
//! equivalent udev rule — document at install time (Fase 3b).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// What a hotkey triggers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HotkeyTarget {
    ToggleVoiceChanger,
    ToggleMute,
    /// Momentary: voice changer on while held, off on release.
    PushToTalk,
    /// Momentary: master mute on while held, off on release.
    PushToMute,
    PlaySound(String),
    SelectVoice(String),
    StopAllSounds,
    /// Edge: write the instant-replay window to disk (F-0.4.B item 20).
    SaveReplay,
}

/// A user keybind: evdev key codes + target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keybind {
    /// Linux evdev `KEY_*` codes that must be held together.
    pub codes: Vec<u16>,
    pub target: HotkeyTarget,
    pub enabled: bool,
}

/// In-memory keybind store with JSON persistence (`config.json`, owned
/// by the daemon; see [`save_daemon_config`]/[`load_daemon_config`]).
#[derive(Debug, Default)]
pub struct HotkeyManager {
    binds: HashMap<u32, Keybind>,
    next_id: u32,
}

impl HotkeyManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, bind: Keybind) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.binds.insert(id, bind);
        id
    }

    pub fn remove(&mut self, id: u32) -> bool {
        self.binds.remove(&id).is_some()
    }

    pub fn set_enabled(&mut self, id: u32, enabled: bool) -> bool {
        match self.binds.get_mut(&id) {
            Some(b) => {
                b.enabled = enabled;
                true
            }
            None => false,
        }
    }

    pub fn list(&self) -> Vec<(u32, Keybind)> {
        let mut out: Vec<(u32, Keybind)> =
            self.binds.iter().map(|(id, b)| (*id, b.clone())).collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    /// Match currently-held codes against enabled binds.
    pub fn match_held(&self, held: &[u16]) -> Vec<HotkeyTarget> {
        self.binds
            .values()
            .filter(|b| b.enabled && !b.codes.is_empty() && b.codes.iter().all(|c| held.contains(c)))
            .map(|b| b.target.clone())
            .collect()
    }

    /// Export the full store (ids included) for persistence.
    pub fn export(&self) -> Vec<StoredBind> {
        let mut out: Vec<StoredBind> = self
            .binds
            .iter()
            .map(|(id, bind)| StoredBind { id: *id, bind: bind.clone() })
            .collect();
        out.sort_by_key(|s| s.id);
        out
    }

    /// Replace the store with persisted binds; `next_id` resumes past
    /// the highest stored id so ids stay stable across restarts
    /// (StreamDeck-style clients can reference them).
    pub fn import(&mut self, binds: Vec<StoredBind>) {
        self.binds.clear();
        let mut next = 0u32;
        for s in binds {
            next = next.max(s.id.saturating_add(1));
            self.binds.insert(s.id, s.bind);
        }
        self.next_id = next;
    }
}

/// A persisted bind: id included so references stay stable across
/// restarts (see [`HotkeyManager::import`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredBind {
    pub id: u32,
    pub bind: Keybind,
}

/// Whole daemon config (`$VM_CONFIG_DIR/config.json`): hotkey binds +
/// device→voice profiles + the F-0.4.C integration settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub hotkeys: Vec<StoredBind>,
    /// Device (uid, uid base, or device name) → voice preset name.
    #[serde(default)]
    pub device_profiles: HashMap<String, String>,
    /// Hear-myself monitor (F-0.4.C 15).
    #[serde(default)]
    pub monitor: super::settings::MonitorConfig,
    /// Sidechain ducking (F-0.4.C 14).
    #[serde(default)]
    pub ducking: super::settings::DuckingConfig,
    /// obs-websocket integration (F-0.4.C 35).
    #[serde(default)]
    pub obs: super::settings::ObsConfig,
    /// MIDI learn (F-0.4.C 37).
    #[serde(default)]
    pub midi: super::settings::MidiConfig,
    /// Instant replay ring (F-0.4.B 20).
    #[serde(default)]
    pub replay: super::settings::ReplayConfig,
    /// Per-application passthrough (F-0.4.B 18).
    #[serde(default)]
    pub passthrough: super::settings::PassthroughConfig,
}

/// On-disk config file shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConfigFile {
    version: u32,
    #[serde(default)]
    hotkeys: Vec<StoredBind>,
    #[serde(default)]
    device_profiles: HashMap<String, String>,
    #[serde(default)]
    monitor: super::settings::MonitorConfig,
    #[serde(default)]
    ducking: super::settings::DuckingConfig,
    #[serde(default)]
    obs: super::settings::ObsConfig,
    #[serde(default)]
    midi: super::settings::MidiConfig,
    #[serde(default)]
    replay: super::settings::ReplayConfig,
    #[serde(default)]
    passthrough: super::settings::PassthroughConfig,
}

/// Current config schema version. v1 (hotkeys only), v2 (device
/// profiles) and v3 (F-0.4.C integrations) files migrate forward;
/// unknown versions fail closed with a clear error.
pub const CONFIG_VERSION: u32 = 4;

/// Serialize the daemon config to file text.
pub fn save_daemon_config(cfg: &DaemonConfig) -> String {
    serde_json::to_string_pretty(&ConfigFile {
        version: CONFIG_VERSION,
        hotkeys: cfg.hotkeys.clone(),
        device_profiles: cfg.device_profiles.clone(),
        monitor: cfg.monitor.clone(),
        ducking: cfg.ducking.clone(),
        obs: cfg.obs.clone(),
        midi: cfg.midi.clone(),
        replay: cfg.replay.clone(),
        passthrough: cfg.passthrough.clone(),
    })
    .unwrap_or_else(|_| {
        String::from(
            "{\"version\":4,\"hotkeys\":[],\"device_profiles\":{},\"monitor\":{},\
             \"ducking\":{},\"obs\":{},\"midi\":{},\"replay\":{},\"passthrough\":{}}",
        )
    })
}

/// Parse config file text. Unknown versions fail closed (never silently
/// reinterpreted); unknown *targets* also fail (a typo'd bind must not
/// boot as a different action).
pub fn load_daemon_config(text: &str) -> Result<DaemonConfig, String> {
    let cfg: ConfigFile =
        serde_json::from_str(text).map_err(|e| format!("bad daemon config: {e}"))?;
    match cfg.version {
        1 => Ok(DaemonConfig { hotkeys: cfg.hotkeys, ..DaemonConfig::default() }),
        2 => Ok(DaemonConfig {
            hotkeys: cfg.hotkeys,
            device_profiles: cfg.device_profiles,
            ..DaemonConfig::default()
        }),
        CONFIG_VERSION => Ok(DaemonConfig {
            hotkeys: cfg.hotkeys,
            device_profiles: cfg.device_profiles,
            monitor: cfg.monitor,
            ducking: cfg.ducking,
            obs: cfg.obs,
            midi: cfg.midi,
            replay: cfg.replay,
            passthrough: cfg.passthrough,
        }),
        v => Err(format!("unsupported daemon config version {v} (want {CONFIG_VERSION})")),
    }
}

/// Validate an API-supplied bind before it enters the store.
pub fn validate_bind(bind: &Keybind) -> Result<(), String> {
    if bind.codes.is_empty() || bind.codes.len() > 8 {
        return Err("codes must hold 1..=8 evdev key codes".to_string());
    }
    if bind.codes.iter().any(|c| *c > 0x2ff) {
        return Err("code out of evdev range (0..=0x2ff)".to_string());
    }
    match &bind.target {
        HotkeyTarget::PlaySound(n) | HotkeyTarget::SelectVoice(n) if n.is_empty() => {
            Err("target name must not be empty".to_string())
        }
        _ => Ok(()),
    }
}

/// Flatpak-sandbox explanation, or `None` when not sandboxed. Hotkeys
/// need `/dev/input`, which the sandbox never grants — say so plainly
/// instead of the generic "no readable keyboards".
pub fn sandbox_note() -> Option<&'static str> {
    if std::env::var("FLATPAK_ID").is_ok() || std::path::Path::new("/.flatpak-info").exists() {
        Some(
            "hotkeys: Flatpak sandbox has no /dev/input access — hotkeys are unavailable \
             here (use the native AppImage/AUR build, or drive the Control API instead)",
        )
    } else {
        None
    }
}

impl HotkeyManager {
    /// List readable evdev keyboard devices (best-effort; empty without
    /// input permissions — see module docs).
    pub fn scan_keyboards() -> Vec<String> {
        let mut out = Vec::new();
        for n in 0..32 {
            let path = format!("/dev/input/event{n}");
            match evdev::Device::open(&path) {
                Ok(dev) => {
                    let name = dev.name().unwrap_or("?").to_string();
                    // Heuristic: keyboards expose KEY_A.
                    if dev.supported_keys().map(|k| k.contains(evdev::Key::KEY_A)).unwrap_or(false) {
                        out.push(format!("{path} ({name})"));
                    }
                }
                Err(_) => continue,
            }
        }
        out
    }
}

/// Momentary targets (PTT/PTM) follow press AND release; everything
/// else fires on press only.
pub fn is_momentary(t: &HotkeyTarget) -> bool {
    matches!(t, HotkeyTarget::PushToTalk | HotkeyTarget::PushToMute)
}

/// Blocking evdev read loop for one keyboard device. Runs on its own
/// thread; calls `on_edge(target, pressed)` for press edges whose held
/// set matches an enabled bind, plus release edges for momentary binds
/// (PTT/PTM) whose chord stops matching. Best-effort: returns if the
/// device can't be opened (permissions) or disappears.
pub fn watch_device(path: String, manager: std::sync::Arc<std::sync::Mutex<HotkeyManager>>, on_edge: impl Fn(HotkeyTarget, bool) + Send + 'static) {
    use std::collections::HashSet;
    let mut dev = match evdev::Device::open(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hotkeys: cannot open {path}: {e}");
            return;
        }
    };
    let mut held: HashSet<u16> = HashSet::new();    loop {
        let batch = match dev.fetch_events() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("hotkeys: {path} read error: {e}");
                return;
            }
        };
        for ev in batch {
            if ev.event_type() != evdev::EventType::KEY {
                continue;
            }
            let code = ev.code();
            match ev.value() {
                1 => {
                    held.insert(code);
                    let held_vec: Vec<u16> = held.iter().copied().collect();
                    if let Ok(mgr) = manager.lock() {
                        for target in mgr.match_held(&held_vec) {
                            on_edge(target, true);
                        }
                    }
                }
                0 => {
                    // Release edge: a momentary chord that stops matching
                    // flips off (toggles/sounds are press-only by design).
                    let held_vec: Vec<u16> = held.iter().copied().collect();
                    let before: Vec<HotkeyTarget> = manager
                        .lock()
                        .map(|mgr| mgr.match_held(&held_vec))
                        .unwrap_or_default();
                    held.remove(&code);
                    let held_vec: Vec<u16> = held.iter().copied().collect();
                    if let Ok(mgr) = manager.lock() {
                        let after = mgr.match_held(&held_vec);
                        for target in before {
                            if is_momentary(&target) && !after.contains(&target) {
                                on_edge(target, false);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod hotkey_tests {
    use super::*;

    fn bind(codes: &[u16], target: HotkeyTarget) -> Keybind {
        Keybind { codes: codes.to_vec(), target, enabled: true }
    }

    #[test]
    fn momentary_classification() {
        assert!(is_momentary(&HotkeyTarget::PushToTalk));
        assert!(is_momentary(&HotkeyTarget::PushToMute));
        assert!(!is_momentary(&HotkeyTarget::ToggleMute));
        assert!(!is_momentary(&HotkeyTarget::StopAllSounds));
        assert!(!is_momentary(&HotkeyTarget::SaveReplay));
        // New edge target round-trips on the wire.
        let t: HotkeyTarget = serde_json::from_str(r#""SaveReplay""#).unwrap();
        assert_eq!(t, HotkeyTarget::SaveReplay);
    }

    #[test]
    fn match_held_needs_full_chord_and_enabled() {
        let mut mgr = HotkeyManager::new();
        mgr.add(bind(&[29, 42], HotkeyTarget::PushToTalk));
        let mut off = bind(&[56], HotkeyTarget::ToggleMute);
        off.enabled = false;
        mgr.add(off);
        assert_eq!(mgr.match_held(&[29, 42]), vec![HotkeyTarget::PushToTalk]);
        assert_eq!(mgr.match_held(&[29]), Vec::new(), "partial chord must not fire");
        assert_eq!(mgr.match_held(&[56]), Vec::new(), "disabled bind must not fire");
        // Extra held keys don't break the chord.
        assert_eq!(mgr.match_held(&[29, 42, 56]), vec![HotkeyTarget::PushToTalk]);
    }

    #[test]
    fn export_import_keeps_ids_stable() {
        let mut mgr = HotkeyManager::new();
        let a = mgr.add(bind(&[29], HotkeyTarget::PushToTalk));
        let b = mgr.add(bind(&[56], HotkeyTarget::ToggleMute));
        mgr.remove(a);
        let exported = mgr.export();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].id, b);
        // Fresh manager resumes past the highest id (no reuse).
        let mut mgr2 = HotkeyManager::new();
        mgr2.import(exported);
        let c = mgr2.add(bind(&[57], HotkeyTarget::StopAllSounds));
        assert!(c > b, "ids must not be reused after import");
        assert_eq!(mgr2.list().len(), 2);
    }

    #[test]
    fn config_roundtrip_and_version_gate() {
        let mut mgr = HotkeyManager::new();
        mgr.add(bind(&[29, 42], HotkeyTarget::PushToTalk));
        mgr.add(bind(&[56], HotkeyTarget::SelectVoice("mic-npr-voice".into())));
        let cfg = DaemonConfig {
            hotkeys: mgr.export(),
            device_profiles: [("Fifine Microphone".to_string(), "mic-npr-voice".to_string())]
                .into_iter()
                .collect(),
            monitor: crate::settings::MonitorConfig {
                enabled: true,
                device: "out#3".into(),
                mix: 0.25,
                ..Default::default()
            },
            ducking: crate::settings::DuckingConfig { depth_db: 18.0, ..Default::default() },
            obs: crate::settings::ObsConfig { enabled: true, password: "pw".into(), ..Default::default() },
            midi: crate::settings::MidiConfig {
                bindings: vec![crate::settings::MidiBinding {
                    id: 0,
                    channel: None,
                    kind: crate::settings::MidiKind::Cc,
                    number: 7,
                    target: crate::settings::MidiTarget::Trigger(HotkeyTarget::ToggleMute),
                    enabled: true,
                }],
                ..Default::default()
            },
            replay: crate::settings::ReplayConfig { enabled: false, seconds: 45 },
            passthrough: crate::settings::PassthroughConfig {
                enabled: true,
                device: "alsa_output.pci-0000".into(),
                app: "Discord".into(),
            },
        };
        let text = save_daemon_config(&cfg);
        let back = load_daemon_config(&text).expect("roundtrip");
        assert_eq!(back.hotkeys.len(), 2);
        assert!(matches!(back.hotkeys[0].bind.target, HotkeyTarget::PushToTalk));
        assert_eq!(back.device_profiles["Fifine Microphone"], "mic-npr-voice");
        assert!(back.monitor.enabled && back.monitor.mix == 0.25);
        assert_eq!(back.ducking.depth_db, 18.0);
        assert!(back.obs.enabled && back.obs.password == "pw");
        assert_eq!(back.midi.bindings.len(), 1);
        assert_eq!(back.replay, crate::settings::ReplayConfig { enabled: false, seconds: 45 });
        assert!(back.passthrough.enabled && back.passthrough.app == "Discord");
        // v1/v2 files migrate forward; untouched sections keep defaults.
        let v1 = r#"{"version":1,"hotkeys":[]}"#;
        let migrated = load_daemon_config(v1).expect("v1 migrates");
        assert!(migrated.hotkeys.is_empty() && migrated.device_profiles.is_empty());
        assert!(migrated.ducking.enabled, "settings get v3 defaults");
        assert!(migrated.replay.enabled, "replay defaults on (v4)");
        let v2 = r#"{"version":2,"hotkeys":[],"device_profiles":{"Fifine":"mic-npr-voice"}}"#;
        let migrated = load_daemon_config(v2).expect("v2 migrates");
        assert_eq!(migrated.device_profiles["Fifine"], "mic-npr-voice");
        assert!(!migrated.monitor.enabled, "monitor stays off unless configured");
        assert!(!migrated.passthrough.enabled, "passthrough stays off unless configured");
        // Wrong version fails closed.
        let bad = text.replace("\"version\": 4", "\"version\": 99");
        assert!(load_daemon_config(&bad).is_err());
        // Garbage fails with a clear error, never an empty store.
        assert!(load_daemon_config("not json").is_err());
        // Unknown target fails (typo must not boot as another action).
        assert!(load_daemon_config(r#"{"version":1,"hotkeys":[{"id":0,"bind":{"codes":[29],"target":"Frobnicator","enabled":true}}]}"#).is_err());
    }

    #[test]
    fn validate_bind_rejects_nonsense() {
        assert!(validate_bind(&bind(&[29], HotkeyTarget::PushToTalk)).is_ok());
        assert!(validate_bind(&bind(&[], HotkeyTarget::PushToTalk)).is_err());
        assert!(validate_bind(&bind(&[0x300], HotkeyTarget::PushToTalk)).is_err());
        assert!(validate_bind(&bind(&[29], HotkeyTarget::SelectVoice(String::new()))).is_err());
        assert!(validate_bind(&bind(&[29; 9], HotkeyTarget::PushToTalk)).is_err());
    }

    #[test]
    fn sandbox_note_absent_outside_flatpak() {
        // CI/dev machines are not sandboxed; the Flatpak branch is
        // covered by inspection (FLATPAK_ID env / /.flatpak-info check).
        if std::env::var("FLATPAK_ID").is_err()
            && !std::path::Path::new("/.flatpak-info").exists()
        {
            assert!(sandbox_note().is_none());
        }
    }

    #[test]
    fn addhotkey_wire_shape_parses() {
        use crate::actions::{Action, ControlMessage};
        // Unit target: bare string.
        let msg: ControlMessage = serde_json::from_str(
            r#"{"action":"addHotkey","codes":[29,42],"target":"PushToTalk","enabled":true,"id":7}"#,
        )
        .expect("unit target parses");
        assert_eq!(
            msg.action,
            Action::AddHotkey {
                codes: vec![29, 42],
                target: HotkeyTarget::PushToTalk,
                enabled: true,
            }
        );
        assert_eq!(msg.id, Some(serde_json::json!(7)));
        // Data target: externally-tagged map.
        let msg: ControlMessage = serde_json::from_str(
            r#"{"action":"addHotkey","codes":[56],"target":{"SelectVoice":"mic-npr-voice"},"enabled":true}"#,
        )
        .expect("data target parses");
        assert!(matches!(
            msg.action,
            Action::AddHotkey { target: HotkeyTarget::SelectVoice(_), .. }
        ));
        // Unknown target fails at parse (never boots as another action).
        assert!(serde_json::from_str::<ControlMessage>(
            r#"{"action":"addHotkey","codes":[29],"target":"Frobnicator","enabled":true}"#
        )
        .is_err());
        // removeHotkey's bind id must NOT collide with the request
        // correlation id (a strict client always sends both).
        let msg: ControlMessage = serde_json::from_str(
            r#"{"action":"removeHotkey","hotkey_id":0,"id":6}"#,
        )
        .expect("bind id and request id coexist");
        assert_eq!(msg.action, Action::RemoveHotkey { hotkey_id: 0 });
        assert_eq!(msg.id, Some(serde_json::json!(6)));
    }
}
