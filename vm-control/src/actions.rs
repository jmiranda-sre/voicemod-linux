//! Control actions. Same *coverage* as the reference Control API
//! (StreamDeck-compatible surface), own schema:
//! `{"action": "<name>", ...fields}` → `{"ok": true|false, ...}`.

use serde::{Deserialize, Serialize};

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
    StreamStatus { active: bool },
    DevicesChanged,
}

/// Implemented by whoever owns the live pipeline (the daemon).
/// Must be lock-free-friendly: called from WS connection tasks;
/// never block on the RT path (use `try_lock`, report contention).
pub trait ControlHandler: Send + Sync {
    fn handle(&self, action: &Action) -> (ControlReply, Vec<ControlEvent>);
}
