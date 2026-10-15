//! Parameter model: static values + modulation sources.
//!
//! Mirrors the reference `FAudioParam*` / `VAudioParameter*` families with
//! an original design: every parameter is either a static value or a
//! modulation source evaluated per quantum.

/// Variant-typed parameter value (mirrors the loader's variant dispatch).
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    Float(f32),
    Int(i32),
    Bool(bool),
    Text(String),
    /// Multi-lane value (e.g. a denoise noise profile: one magnitude
    /// per FFT bin). Nodes that don't understand it reject with
    /// `TypeMismatch` via the accessors below.
    FloatArray(Vec<f32>),
}

impl ParamValue {
    pub fn as_float(&self) -> Option<f32> {
        match self {
            Self::Float(v) => Some(*v),
            Self::Int(v) => Some(*v as f32),
            _ => None,
        }
    }

    pub fn as_float_array(&self) -> Option<&[f32]> {
        match self {
            Self::FloatArray(v) => Some(v),
            _ => None,
        }
    }
}

/// A modulation source evaluated per quantum. Sampled at control rate
/// (once per `process` call) unless a node documents otherwise.
#[derive(Debug, Clone, PartialEq)]
pub enum Modulation {
    /// Fixed value.
    Static(ParamValue),
    /// Sine LFO: `offset + depth * sin(2π·rate·t + phase)`.
    Lfo {
        rate_hz: f32,
        depth: f32,
        offset: f32,
    },
    /// Random walk / sample-and-hold.
    Random {
        min: f32,
        max: f32,
        hold_quanta: u32,
    },
    /// Linear transition from current to target over N quanta.
    Transition { target: f32, quanta: u32 },
    /// Gate/trigger: emits `high` while active, else `low`.
    Trigger { high: f32, low: f32 },
    /// Step sequence looped per quantum.
    Sequence(Vec<f32>),
}

/// A UI-exposed parameter with its valid range (Voicelab-slider equivalent).
#[derive(Debug, Clone, PartialEq)]
pub struct PublicParam {
    pub name: &'static str,
    pub value: ParamValue,
    pub range: Option<RangedParam>,
    /// `false` = internal/calibration param, hidden from UI
    /// (mirrors the reference "non public param" skip).
    pub public: bool,
}

/// Numeric range for continuous parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RangedParam {
    pub min: f32,
    pub max: f32,
    pub default: f32,
}

impl RangedParam {
    pub fn clamp(&self, v: f32) -> f32 {
        v.clamp(self.min, self.max)
    }
}
