//! Control actions. Same *coverage* as the reference Control API
//! (StreamDeck-compatible surface), own schema:
//! `{"action": "<name>", ...fields}` → `{"ok": true|false, ...}`.

use serde::{Deserialize, Serialize};

use super::hotkeys::HotkeyTarget;

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
}

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
        ];
        assert_eq!(all.len(), 26, "keep this list in sync with the catalogue");
        for a in &all {
            let o = serde_json::to_value(a).unwrap();
            let o = o.as_object().unwrap();
            assert!(
                !o.contains_key("id") && !o.contains_key("token"),
                "envelope collision in {a:?}"
            );
        }
    }
}
