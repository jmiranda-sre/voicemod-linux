//! Settings schemas for the F-0.4.C integrations: hear-myself monitor,
//! sidechain ducking, obs-websocket and MIDI learn.
//!
//! These types are at once the wire shape of their control actions and
//! (config schema v3) sections of `config.json`. Field names stay
//! snake_case on the wire, matching the rest of the catalogue.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::hotkeys::HotkeyTarget;

/// Clamp a float, substituting `def` for NaN/inf (a poisoned value must
/// never reach the RT path).
fn sane(v: f32, lo: f32, hi: f32, def: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        def
    }
}

/// F-0.4.C item 15 — hear-myself monitor: a local listening bus
/// independent from the `vm-mic` channel, blending the raw mic (dry)
/// with the processed voice (wet). Empty `device` = system default sink.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub enabled: bool,
    /// Output device uid (`name#id`); empty = system default sink.
    pub device: String,
    /// Dry↔wet blend: 0.0 = raw mic (zero added latency), 1.0 = processed.
    pub mix: f32,
    pub volume_db: f32,
    pub muted: bool,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self { enabled: false, device: String::new(), mix: 0.0, volume_db: 0.0, muted: false }
    }
}

impl MonitorConfig {
    /// Sanitize API-supplied values (NaN-safe).
    pub fn clamp(&mut self) {
        self.mix = sane(self.mix, 0.0, 1.0, 0.0);
        self.volume_db = sane(self.volume_db, -60.0, 12.0, 0.0);
    }
}

/// F-0.4.C item 14 — sidechain ducking: attenuate the soundboard/music
/// bus while speech is detected on the mic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DuckingConfig {
    pub enabled: bool,
    /// Attenuation applied while speech is active (dB, positive = lower).
    pub depth_db: f32,
    /// Input level (dBFS, RMS) above which the mic counts as speech.
    pub threshold_db: f32,
    /// Ramp into the ducked level (ms).
    pub attack_ms: f32,
    /// Hold + ramp back to unity after speech stops (ms).
    pub release_ms: f32,
}

impl Default for DuckingConfig {
    fn default() -> Self {
        Self { enabled: true, depth_db: 12.0, threshold_db: -40.0, attack_ms: 50.0, release_ms: 400.0 }
    }
}

impl DuckingConfig {
    /// Sanitize API-supplied values (NaN-safe).
    pub fn clamp(&mut self) {
        self.depth_db = sane(self.depth_db, 0.0, 48.0, 12.0);
        self.threshold_db = sane(self.threshold_db, -80.0, 0.0, -40.0);
        self.attack_ms = sane(self.attack_ms, 1.0, 500.0, 50.0);
        self.release_ms = sane(self.release_ms, 10.0, 3000.0, 400.0);
    }
}

/// F-0.4.C item 35 — obs-websocket (v5) integration: auto-switch voice
/// per OBS scene and report OBS state to the UI/Companion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObsConfig {
    pub enabled: bool,
    /// obs-websocket endpoint; v5 protocol (OBS 28+).
    pub url: String,
    /// Server password (plain text in config.json, like the hotkeys —
    /// the file is user-owned; document at install time).
    pub password: String,
    /// OBS scene name → voice preset name.
    pub scenes: HashMap<String, String>,
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "ws://127.0.0.1:4455".to_string(),
            password: String::new(),
            scenes: HashMap::new(),
        }
    }
}

/// MIDI message kind the binding listens to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MidiKind {
    /// Control Change (`0xB0`).
    Cc,
    /// Note On/Off (`0x90`/`0x80`; velocity 0 = release edge).
    Note,
}

/// What a MIDI mapping does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MidiTarget {
    /// Edge trigger reusing the hotkey target set (pads, mute, voices).
    Trigger(HotkeyTarget),
    /// `value/127` mapped linearly onto `[min, max]` and applied as
    /// `setVoiceParameter` on `node.param`.
    SetVoiceParameter { param: String, min: f32, max: f32 },
}

/// One MIDI mapping: an event address plus its target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MidiBinding {
    pub id: u32,
    /// MIDI channel 0..=15; omitted = any channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    pub kind: MidiKind,
    /// CC number or note number (0..=127).
    pub number: u8,
    pub target: MidiTarget,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl MidiBinding {
    pub fn validate(&self) -> Result<(), String> {
        if self.number > 127 {
            return Err("number must be 0..=127".to_string());
        }
        if let Some(c) = self.channel {
            if c > 15 {
                return Err("channel must be 0..=15".to_string());
            }
        }
        match &self.target {
            MidiTarget::Trigger(HotkeyTarget::PlaySound(n))
            | MidiTarget::Trigger(HotkeyTarget::SelectVoice(n))
                if n.is_empty() =>
            {
                Err("target name must not be empty".to_string())
            }
            MidiTarget::SetVoiceParameter { param, min, max } => {
                if param.split_once('.').map(|(n, p)| n.is_empty() || p.is_empty()).unwrap_or(true) {
                    return Err("param must be 'node.param'".to_string());
                }
                if !min.is_finite() || !max.is_finite() {
                    return Err("min/max must be finite".to_string());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// One incoming MIDI message (F-0.4.C 37): channel + kind + number/value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiEvent {
    /// MIDI channel 0..=15.
    pub channel: u8,
    pub kind: MidiKind,
    /// CC number or note number (0..=127).
    pub number: u8,
    /// CC value / note velocity (0..=127). `0` on a note is a release.
    pub value: u8,
}

/// F-0.4.C item 37 — MIDI learn: map CC/Note events to params, pads,
/// mutes and voices from physical hardware.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiConfig {
    pub enabled: bool,
    /// Input port name to open; empty = every available port.
    pub port: String,
    pub bindings: Vec<MidiBinding>,
}

impl Default for MidiConfig {
    fn default() -> Self {
        Self { enabled: true, port: String::new(), bindings: Vec::new() }
    }
}

impl MidiConfig {
    /// Next stable binding id (ids never repeat, like hotkey ids).
    pub fn next_binding_id(&self) -> u32 {
        self.bindings.iter().map(|b| b.id.saturating_add(1)).max().unwrap_or(0)
    }

    /// Find the binding matching an incoming event (exact number/kind;
    /// channel `None` matches any).
    pub fn find_binding(&self, kind: MidiKind, number: u8, channel: u8) -> Option<&MidiBinding> {
        self.bindings.iter().find(|b| {
            b.enabled
                && b.kind == kind
                && b.number == number
                && b.channel.map(|c| c == channel).unwrap_or(true)
        })
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_wire_shape() {
        let m = MonitorConfig::default();
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "enabled": false, "device": "", "mix": 0.0, "volume_db": 0.0, "muted": false,
            })
        );
        // Partial payloads keep the remaining defaults.
        let m: MonitorConfig = serde_json::from_str(r#"{"enabled":true,"device":"out#7"}"#).unwrap();
        assert!(m.enabled);
        assert_eq!(m.device, "out#7");
        assert_eq!(m.mix, 0.0);
    }

    #[test]
    fn ducking_defaults_and_clamp() {
        let d = DuckingConfig::default();
        assert!(d.enabled, "ducking is on by default with sane depth");
        assert_eq!(d.depth_db, 12.0);
        let mut d = DuckingConfig { depth_db: 999.0, threshold_db: f32::NAN, ..Default::default() };
        d.clamp();
        assert_eq!(d.depth_db, 48.0);
        assert_eq!(d.threshold_db, -40.0, "NaN falls back to the default");
    }

    #[test]
    fn obs_scenes_roundtrip() {
        let mut o = ObsConfig { enabled: true, password: "pw".into(), ..Default::default() };
        o.scenes.insert("Game".into(), "robot".into());
        let text = serde_json::to_string(&o).unwrap();
        let back: ObsConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back, o);
        assert_eq!(back.scenes["Game"], "robot");
    }

    #[test]
    fn midi_binding_validation_and_match() {
        let b = MidiBinding {
            id: 0,
            channel: None,
            kind: MidiKind::Cc,
            number: 7,
            target: MidiTarget::SetVoiceParameter {
                param: "gate.threshold".into(),
                min: 0.0,
                max: 1.0,
            },
            enabled: true,
        };
        assert!(b.validate().is_ok());
        let mut cfg = MidiConfig::default();
        cfg.bindings.push(b);
        // Any channel matches a channel-less binding; wrong number does not.
        assert!(cfg.find_binding(MidiKind::Cc, 7, 3).is_some());
        assert!(cfg.find_binding(MidiKind::Cc, 8, 3).is_none());
        assert!(cfg.find_binding(MidiKind::Note, 7, 3).is_none());
        assert_eq!(cfg.next_binding_id(), 1);
        // Bad numbers/targets fail closed.
        let bad = MidiBinding { number: 200, ..cfg.bindings[0].clone() };
        assert!(bad.validate().is_err());
        let bad = MidiBinding {
            target: MidiTarget::SetVoiceParameter { param: "nogate".into(), min: 0.0, max: 1.0 },
            ..cfg.bindings[0].clone()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn midi_trigger_reuses_hotkey_targets() {
        let b = MidiBinding {
            id: 3,
            channel: Some(9),
            kind: MidiKind::Note,
            number: 36,
            target: MidiTarget::Trigger(HotkeyTarget::PushToTalk),
            enabled: true,
        };
        let text = serde_json::to_string(&b).unwrap();
        assert!(text.contains(r#""Trigger":"PushToTalk""#), "{text}");
        let back: MidiBinding = serde_json::from_str(&text).unwrap();
        assert_eq!(back, b);
        assert!(back.channel == Some(9));
    }
}
