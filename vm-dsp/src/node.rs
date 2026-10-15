//! Node trait + processing context.

use super::param::{ParamValue, PublicParam};

/// Real-time processing context for one quantum.
///
/// `frames` varies per callback (device quantum), exactly like the
/// reference PortAudio callback's `frameCount`.
#[derive(Debug, Clone, Copy)]
pub struct ProcessContext {
    pub rate: u32,
    pub frames: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamError {
    Unknown(String),
    TypeMismatch { name: String },
    OutOfRange { name: String },
    /// Well-typed but outside the param's domain (e.g. an unknown
    /// filter `type` name).
    InvalidValue(String),
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(n) => write!(f, "unable to find param {n}"),
            Self::TypeMismatch { name } => write!(f, "invalid variant type for param {name}"),
            Self::OutOfRange { name } => write!(f, "param {name} out of range"),
            Self::InvalidValue(v) => write!(f, "invalid value for param: {v}"),
        }
    }
}

impl std::error::Error for ParamError {}

/// A DSP node: named, parameterised, N-channel native.
///
/// Contract: `inputs.len() == outputs.len()` channel count; each slice has
/// exactly `ctx.frames` samples. Nodes must be RT-safe after construction
/// (no allocation in `process`).
pub trait AudioNode: Send {
    fn name(&self) -> &'static str;
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]);
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError>;
    /// Read back a live value (calibration capture, e.g. a denoise
    /// noise profile). Default is "no readable params" — nodes opt in.
    fn get_param(&self, _name: &str) -> Option<ParamValue> {
        None
    }
    /// Algorithmic latency in samples at `rate` (STFT frame, delay
    /// line…). Default 0 (stateless gain/mix/EQ/dynamics). The graph
    /// sums it as the chain's latency budget — CPU cost is separate
    /// (see telemetry).
    fn latency_samples(&self, _rate: u32) -> usize {
        0
    }
    fn params(&self) -> Vec<PublicParam>;
}
