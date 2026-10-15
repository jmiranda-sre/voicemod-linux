//! `vm-preset`: clean-room voice preset format (JSON).
//!
//! Mirrors the *role* of the reference voice graph + `[{name, value,
//! public}]` param arrays with an original schema:
//!
//! ```json
//! {
//!   "name": "example",
//!   "nodes": [
//!     {"id": "drive", "type": "gain",
//!      "params": {"gain": {"value": 1.5, "range": [0.0, 4.0], "public": true}}}
//!   ],
//!   "edges": [],
//!   "public_params": ["drive.gain"]
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A parameter value in a preset file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// NOTE: `Int` comes first: serde untagged tries variants in order, so
/// plain `250` lands on `Int` while `1.5` falls through to `Float`.
/// `FloatArray` is last: JSON arrays only match it (e.g. a saved
/// denoise noise profile — 513 magnitudes, non-public, no range).
#[serde(untagged)]
pub enum JsonParamValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    FloatArray(Vec<f64>),
}

/// One parameter entry: value + optional range + visibility.
/// Mirrors the reference "non public param" skip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamSpec {
    pub value: JsonParamValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[f64; 3]>,
    #[serde(default = "default_true")]
    pub public: bool,
    /// Modulation source, e.g. `{"lfo": {"rate_hz": 5.0, "depth": 0.2}}`.
    /// Shape is validated, semantics live in `vm-dsp::param::Modulation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modulation: Option<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

/// One node instance in a preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub id: String,
    pub r#type: String,
    #[serde(default)]
    pub params: HashMap<String, ParamSpec>,
}

/// A directed edge between node output and node input channels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeSpec {
    pub from: String,
    pub from_ch: usize,
    pub to: String,
    pub to_ch: usize,
}

/// A whole voice preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "default_rate")]
    pub rate: u32,
    #[serde(default)]
    pub nodes: Vec<NodeSpec>,
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,
    /// Dotted `node.param` paths exposed to UI/automation.
    #[serde(default)]
    pub public_params: Vec<String>,
}

fn default_rate() -> u32 {
    48_000
}

#[derive(Debug, PartialEq, Eq)]
pub enum PresetError {
    Json(String),
    UnknownNodeType(String),
    UnknownNode(String),
    DuplicateNode(String),
    BadRange(String),
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "preset json: {e}"),
            Self::UnknownNodeType(t) => write!(f, "unknown node type: {t}"),
            Self::UnknownNode(n) => write!(f, "edge references unknown node: {n}"),
            Self::DuplicateNode(n) => write!(f, "duplicate node id: {n}"),
            Self::BadRange(p) => write!(f, "bad range for param: {p}"),
        }
    }
}

impl std::error::Error for PresetError {}

/// Node types the MVP factory understands.
pub const KNOWN_NODE_TYPES: &[&str] = &[
    "null",
    "gain",
    "mute",
    "mix",
    "biquad",
    "pitch-fft",
    "delay",
    "formant-pitch-shifter",
    "reverb",
    "tube",
    "oscillator",
    "gate",
    "compressor",
    "limiter",
    "de-esser",
    "denoise",
];

/// Parse + validate a preset. Validation mirrors loader strictness:
/// known node types, unique ids, edges resolve, ranges sane.
pub fn load(text: &str) -> Result<Preset, PresetError> {
    let preset: Preset = serde_json::from_str(text).map_err(|e| PresetError::Json(e.to_string()))?;
    validate(&preset)?;
    Ok(preset)
}

pub fn validate(preset: &Preset) -> Result<(), PresetError> {
    let mut ids = std::collections::HashSet::new();
    for node in &preset.nodes {
        if !ids.insert(node.id.clone()) {
            return Err(PresetError::DuplicateNode(node.id.clone()));
        }
        if !KNOWN_NODE_TYPES.contains(&node.r#type.as_str()) {
            return Err(PresetError::UnknownNodeType(node.r#type.clone()));
        }
        for (pname, pspec) in &node.params {
            if let Some([min, max, default]) = pspec.range {
                if !(min <= default && default <= max) {
                    return Err(PresetError::BadRange(format!("{}.{}", node.id, pname)));
                }
            }
        }
    }
    for edge in &preset.edges {
        for end in [&edge.from, &edge.to] {
            if !ids.contains(end) {
                return Err(PresetError::UnknownNode(end.clone()));
            }
        }
    }
    Ok(())
}

/// Presets embedded in the binary and seeded into
/// `$VM_CONFIG_DIR/presets` (default `~/.config/vm/presets`) on first
/// boot: `(file stem, JSON text)`. Seeding never overwrites — edits the
/// user makes to a seeded file always win.
pub const BUILTIN_PRESETS: &[(&str, &str)] =
    &[("mic-npr-voice", include_str!("../builtin/mic-npr-voice.json"))];

/// A neutral built-in example (own design, not a vendor pack):
/// gentle drive → low-pass → output.
pub fn example_drive() -> Preset {
    let mut drive_params = HashMap::new();
    drive_params.insert(
        "gain".into(),
        ParamSpec {
            value: JsonParamValue::Float(1.5),
            range: Some([0.0, 4.0, 1.0]),
            public: true,
            modulation: None,
        },
    );
    Preset {
        name: "example-drive".into(),
        description: None,
        rate: 48_000,
        nodes: vec![
            NodeSpec { id: "drive".into(), r#type: "gain".into(), params: drive_params },
            NodeSpec { id: "smooth".into(), r#type: "biquad".into(), params: HashMap::new() },
        ],
        edges: vec![EdgeSpec { from: "drive".into(), from_ch: 0, to: "smooth".into(), to_ch: 0 }],
        public_params: vec!["drive.gain".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_drive_loads() {
        let text = serde_json::to_string(&example_drive()).unwrap();
        load(&text).unwrap();
    }

    #[test]
    fn builtin_presets_embed_and_validate() {
        assert!(!BUILTIN_PRESETS.is_empty());
        for (stem, text) in BUILTIN_PRESETS {
            let preset = load(text)
                .unwrap_or_else(|e| panic!("builtin '{stem}' failed validation: {e}"));
            assert_eq!(&preset.name, stem, "builtin file stem must match preset name");
            assert!(!preset.nodes.is_empty());
        }
    }

    #[test]
    fn mic_npr_voice_covers_the_full_mic_chain() {
        let (_, text) = BUILTIN_PRESETS
            .iter()
            .find(|(s, _)| *s == "mic-npr-voice")
            .expect("mic-npr-voice builtin present");
        let preset = load(text).unwrap();
        let types: Vec<&str> = preset.nodes.iter().map(|n| n.r#type.as_str()).collect();
        // The EasyEffects chain maps 1:1 (rnnoise+deepfilternet →
        // denoise; two Hi-pass stages mirror slope x2).
        for want in ["denoise", "gate", "biquad", "compressor", "de-esser", "limiter"] {
            assert!(types.contains(&want), "chain missing node type {want}");
        }
        // Sequential edges: 10 links for 11 nodes.
        assert_eq!(preset.edges.len(), preset.nodes.len() - 1);
        assert!(preset.public_params.contains(&"comp.threshold_db".to_string()));
    }

    #[test]
    fn rejects_unknown_node_type_and_bad_edge() {
        let text = r#"{"name":"x","nodes":[{"id":"a","type":"not-a-node"}],"edges":[]}"#;
        assert!(matches!(load(text), Err(PresetError::UnknownNodeType(_))));
        let text = r#"{"name":"x","nodes":[{"id":"a","type":"gain"}],
                       "edges":[{"from":"a","from_ch":0,"to":"ghost","to_ch":0}]}"#;
        assert!(matches!(load(text), Err(PresetError::UnknownNode(_))));
    }

    #[test]
    fn float_array_values_roundtrip() {
        // A saved noise profile: 513 magnitudes, non-public, no range.
        let text = r#"{"name":"mic","rate":48000,
            "nodes":[{"id":"dn","type":"denoise","params":{
                "strength":{"value":2.0,"range":[1.0,4.0,2.0],"public":true},
                "profile":{"value":[0.0,0.001,0.002],"public":false}}}],
            "edges":[]}"#;
        let preset = load(text).expect("array param validates");
        let prof = &preset.nodes[0].params["profile"];
        assert!(!prof.public);
        assert!(prof.range.is_none());
        assert_eq!(
            prof.value,
            JsonParamValue::FloatArray(vec![0.0, 0.001, 0.002])
        );
        // Serde roundtrip preserves the array shape (stays an array,
        // never collapses into scalars).
        let back: Preset =
            serde_json::from_str(&serde_json::to_string(&preset).unwrap()).unwrap();
        assert_eq!(back.nodes[0].params["profile"].value, prof.value);
    }
}
