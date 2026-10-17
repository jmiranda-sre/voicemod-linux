//! Control actions. Same *coverage* as the reference Control API
//! (StreamDeck-compatible surface), own schema:
//! `{"action": "<name>", ...fields}` → `{"ok": true|false, ...}`.

use serde::{Deserialize, Serialize};

use super::hotkeys::HotkeyTarget;
use super::settings::{MidiKind, MidiTarget};

/// Full action catalogue (Fase 2 ③ inventory, extended in Fase 3b).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Action {
    SelectVoice { voice: String },
    ToggleVoiceChanger { enabled: bool },
    StopAllSounds,
    PlaySound { sound: String },
    GetAllSoundboard,
    SetVoiceParameter { param: String, value: serde_json::Value },
    ResetVoiceParameters,
    SetDefaultMicrophone { uid: String },
    GetAudioDevices,
    GetUserLicense,
    FadeSound { sound: String, fade_out_secs: f32 },
    StartAudioStreaming,
    GetAudioStreamStatus,
    GetHealth,
    /// Register a hotkey bind. `target` uses the [`HotkeyTarget`] shape,
    /// e.g. `"ToggleMute"` or `{"SelectVoice": "mic-npr-voice"}`.
    AddHotkey { codes: Vec<u16>, target: HotkeyTarget, enabled: bool },
    RemoveHotkey { hotkey_id: u32 },
    ListHotkeys,
    /// Master mute (momentary PTM rides on this; output goes silent
    /// while the DSP state stays live).
    SetMuted { muted: bool },
    /// List loadable voice preset names (sorted stems of
    /// `$VM_CONFIG_DIR/presets/*.json`) — feeds Companion dropdowns.
    GetVoices,
    /// Snapshot a denoise node's live noise floor into the current
    /// voice's preset file (`profile` param): the next load skips the
    /// ~1 s warmup. The "learn room" button for users and Companion.
    CaptureNoiseProfile { node: String },
    /// Bind a device (uid, uid base without `#id`, or device name) to
    /// a voice preset: plugging/selecting it auto-selects the voice.
    SetDeviceProfile { device: String, voice: String },
    RemoveDeviceProfile { device: String },
    ListDeviceProfiles,
    GetBitmap { resource: String },
    SetSoundboardMasterVolume { per_mille: u16 },
    SelectInput { uid: String },
    /// Hear-myself monitor bus (F-0.4.C, item 15). Partial update: only
    /// the provided fields change. `mix` blends dry (raw mic, zero added
    /// latency) ↔ wet (processed voice); `device` empty = default sink.
    SetMonitor {
        enabled: Option<bool>,
        device: Option<String>,
        mix: Option<f32>,
        volume_db: Option<f32>,
        muted: Option<bool>,
    },
    GetMonitor,
    /// Sidechain ducking (F-0.4.C, item 14). Partial update, same rules.
    SetDucking {
        enabled: Option<bool>,
        depth_db: Option<f32>,
        threshold_db: Option<f32>,
        attack_ms: Option<f32>,
        release_ms: Option<f32>,
    },
    GetDucking,
    /// Live mic/output level snapshot (dBFS RMS) + speech flag — feeds
    /// VU meters and OBS-scene widgets.
    GetLevels,
    /// obs-websocket v5 integration (F-0.4.C, item 35). Partial update.
    SetObs { enabled: Option<bool>, url: Option<String>, password: Option<String> },
    GetObsStatus,
    /// Map an OBS scene to a voice preset (auto-switch on scene change).
    SetObsSceneProfile { scene: String, voice: String },
    RemoveObsSceneProfile { scene: String },
    ListObsSceneProfiles,
    /// MIDI learn (F-0.4.C, item 37).
    GetMidiStatus,
    /// MIDI input settings (partial update).
    SetMidi { enabled: Option<bool>, port: Option<String> },
    /// Arm learning: the next CC/Note event binds to `target`.
    MidiLearnStart { target: MidiTarget },
    MidiLearnCancel,
    /// Manual mapping (learn is the ergonomic path; this scripts it).
    MidiBindAdd {
        channel: Option<u8>,
        kind: MidiKind,
        number: u8,
        target: MidiTarget,
    },
    MidiBindRemove { midi_id: u32 },
    ListMidiBindings,
}

/// Every action name the daemon understands, in catalogue order — the
/// single source of truth for `vm actions` and docs. Guarded by
/// `no_action_field_collides_with_envelope`, which serializes one
/// instance of every variant and compares names in this exact order.
pub const ALL_ACTIONS: &[&str] = &[
    "selectVoice",
    "toggleVoiceChanger",
    "stopAllSounds",
    "playSound",
    "getAllSoundboard",
    "setVoiceParameter",
    "resetVoiceParameters",
    "setDefaultMicrophone",
    "getAudioDevices",
    "getUserLicense",
    "fadeSound",
    "startAudioStreaming",
    "getAudioStreamStatus",
    "getHealth",
    "getBitmap",
    "setSoundboardMasterVolume",
    "selectInput",
    "addHotkey",
    "removeHotkey",
    "listHotkeys",
    "setMuted",
    "getVoices",
    "captureNoiseProfile",
    "setDeviceProfile",
    "removeDeviceProfile",
    "listDeviceProfiles",
    "setMonitor",
    "getMonitor",
    "setDucking",
    "getDucking",
    "getLevels",
    "setObs",
    "getObsStatus",
    "setObsSceneProfile",
    "removeObsSceneProfile",
    "listObsSceneProfiles",
    "getMidiStatus",
    "setMidi",
    "midiLearnStart",
    "midiLearnCancel",
    "midiBindAdd",
    "midiBindRemove",
    "listMidiBindings",
];

/// Inbound control message: action + auth token + optional request id.
/// The server echoes a client-provided `id` back in the reply so clients
/// can correlate responses under interleaved broadcasts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlMessage {
    #[serde(flatten)]
    pub action: Action,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
}

/// Outbound reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ControlReply {
    pub fn ok() -> Self {
        Self { ok: true, error: None, data: None }
    }

    pub fn ok_data(data: serde_json::Value) -> Self {
        Self { ok: true, error: None, data: Some(data) }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: Some(msg.into()), data: None }
    }
}

/// State-change broadcast (StreamDeck-style clients subscribe).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum ControlEvent {
    VoiceChanged { voice: String },
    VoiceParameterChanged { param: String, value: serde_json::Value },
    SoundStarted { sound: String },
    SoundStopped { sound: String },
    VoiceChangerToggled { enabled: bool },
    MutedChanged { muted: bool },
    StreamStatus { active: bool },
    DevicesChanged,
    /// Monitor bus settings changed (full new state, F-0.4.C).
    MonitorChanged { enabled: bool, device: String, mix: f32, volume_db: f32, muted: bool },
    /// Ducking settings changed (full new state).
    DuckingChanged {
        enabled: bool,
        depth_db: f32,
        threshold_db: f32,
        attack_ms: f32,
        release_ms: f32,
    },
    /// OBS connection/scene changed (also pushed on connect/disconnect).
    ObsStatusChanged { connected: bool, scene: String, url: String },
    /// MIDI learn captured an event and stored the binding.
    MidiLearned { midi_id: u32, kind: MidiKind, number: u8, channel: Option<u8>, target: MidiTarget },
    /// MIDI mapping was added or removed.
    MidiBindingsChanged,
}

/// Implemented by whoever owns the live pipeline (the daemon).
/// Must be lock-free-friendly: called from WS connection tasks;
/// never block on the RT path (use `try_lock`, report contention).
pub trait ControlHandler: Send + Sync {
    fn handle(&self, action: &Action) -> (ControlReply, Vec<ControlEvent>);
}

#[cfg(test)]
mod action_shape_tests {
    use super::*;
    use crate::hotkeys::HotkeyTarget;

    /// No action field may be named `id` or `token`: `ControlMessage`
    /// flattens the action alongside its own correlation `id` / auth
    /// `token`, and serde gives the outer fields priority — an action
    /// field with the same name would silently vanish on parse
    /// (removeHotkey did exactly this; now covered forever).
    #[test]
    fn no_action_field_collides_with_envelope() {
        let all = vec![
            Action::SelectVoice { voice: String::new() },
            Action::ToggleVoiceChanger { enabled: true },
            Action::StopAllSounds,
            Action::PlaySound { sound: String::new() },
            Action::GetAllSoundboard,
            Action::SetVoiceParameter { param: String::new(), value: serde_json::Value::Null },
            Action::ResetVoiceParameters,
            Action::SetDefaultMicrophone { uid: String::new() },
            Action::GetAudioDevices,
            Action::GetUserLicense,
            Action::FadeSound { sound: String::new(), fade_out_secs: 0.0 },
            Action::StartAudioStreaming,
            Action::GetAudioStreamStatus,
            Action::GetHealth,
            Action::GetBitmap { resource: String::new() },
            Action::SetSoundboardMasterVolume { per_mille: 0 },
            Action::SelectInput { uid: String::new() },
            Action::AddHotkey {
                codes: vec![],
                target: HotkeyTarget::PushToTalk,
                enabled: true,
            },
            Action::RemoveHotkey { hotkey_id: 0 },
            Action::ListHotkeys,
            Action::SetMuted { muted: false },
            Action::GetVoices,
            Action::CaptureNoiseProfile { node: String::new() },
            Action::SetDeviceProfile { device: String::new(), voice: String::new() },
            Action::RemoveDeviceProfile { device: String::new() },
            Action::ListDeviceProfiles,
            Action::SetMonitor {
                enabled: None,
                device: None,
                mix: None,
                volume_db: None,
                muted: None,
            },
            Action::GetMonitor,
            Action::SetDucking {
                enabled: None,
                depth_db: None,
                threshold_db: None,
                attack_ms: None,
                release_ms: None,
            },
            Action::GetDucking,
            Action::GetLevels,
            Action::SetObs { enabled: None, url: None, password: None },
            Action::GetObsStatus,
            Action::SetObsSceneProfile { scene: String::new(), voice: String::new() },
            Action::RemoveObsSceneProfile { scene: String::new() },
            Action::ListObsSceneProfiles,
            Action::GetMidiStatus,
            Action::SetMidi { enabled: None, port: None },
            Action::MidiLearnStart {
                target: MidiTarget::Trigger(HotkeyTarget::ToggleMute),
            },
            Action::MidiLearnCancel,
            Action::MidiBindAdd {
                channel: None,
                kind: MidiKind::Cc,
                number: 0,
                target: MidiTarget::Trigger(HotkeyTarget::ToggleMute),
            },
            Action::MidiBindRemove { midi_id: 0 },
            Action::ListMidiBindings,
        ];
        assert_eq!(all.len(), 43, "keep this list in sync with the catalogue");
        let names: Vec<String> = all
            .iter()
            .map(|a| {
                let o = serde_json::to_value(a).unwrap();
                let o = o.as_object().unwrap();
                assert!(
                    !o.contains_key("id") && !o.contains_key("token"),
                    "envelope collision in {a:?}"
                );
                o["action"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            names,
            ALL_ACTIONS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "ALL_ACTIONS is stale — update vm-control::ALL_ACTIONS in variant order"
        );
    }

    #[test]
    fn set_monitor_partial_update_deserializes() {
        // Only the provided fields change; the rest default to None.
        let a: Action = serde_json::from_str(r#"{"action":"setMonitor","enabled":true}"#).unwrap();
        assert_eq!(
            a,
            Action::SetMonitor { enabled: Some(true), device: None, mix: None, volume_db: None, muted: None }
        );
        let a: Action =
            serde_json::from_str(r#"{"action":"setDucking","depth_db":18.0}"#).unwrap();
        assert_eq!(
            a,
            Action::SetDucking {
                enabled: None,
                depth_db: Some(18.0),
                threshold_db: None,
                attack_ms: None,
                release_ms: None,
            }
        );
        // Wire shape: snake_case fields, camelCase actions.
        let v = serde_json::to_value(Action::SetObsSceneProfile {
            scene: "Game".into(),
            voice: "robot".into(),
        })
        .unwrap();
        assert_eq!(v["action"], "setObsSceneProfile");
        let v = serde_json::to_value(Action::SetMonitor {
            enabled: Some(true),
            device: None,
            mix: Some(0.5),
            volume_db: None,
            muted: None,
        })
        .unwrap();
        assert_eq!(v["volume_db"], serde_json::Value::Null);
        assert_eq!(v["mix"], 0.5);
        // Remove-by-id must never shadow the envelope `id`.
        let v = serde_json::to_value(Action::MidiBindRemove { midi_id: 7 }).unwrap();
        assert_eq!(v["midi_id"], 7);
    }
}
