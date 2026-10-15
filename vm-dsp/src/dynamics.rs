//! Dynamics processors: gate, compressor, limiter, de-esser.
//!
//! Clean-room textbook feed-forward designs: RMS level detection with
//! one-pole ballistics, soft-knee gain computer, smoothing in the dB
//! domain. Every detector links across channels — one sidechain stream
//! drives one gain applied to all lanes, exactly like a mono sidechain
//! on a stereo bus (mic chains are mono anyway).
//!
//! Timing model: `attack_ms`/`release_ms` are one-pole time constants
//! (63% of the step in one tau), computed from the sample rate once per
//! quantum, never per sample.

use super::node::{AudioNode, ParamError, ProcessContext};
use super::param::{ParamValue, PublicParam, RangedParam};

/// Linear amplitude from decibels: `10^(dB/20)`, written as `exp2`
/// with the log2(10) factor folded into the multiplier (exp2 is a
/// single instruction; the raw `exp2(db/20)` would be 2^(dB/20) — an
/// order-of-magnitude-wrong constant).
pub(crate) fn db_to_gain(db: f32) -> f32 {
    (db * (std::f32::consts::LOG2_10 / 20.0)).exp2()
}

/// Mean-square → dBFS (10·log10(ms) written as a log2 multiply).
fn ms_to_db(ms: f32) -> f32 {
    10.0 * core::f32::consts::LOG10_2 * ms.max(1e-15).log2()
}

/// Peak → dBFS (20·log10(x)).
fn peak_to_db(x: f32) -> f32 {
    20.0 * core::f32::consts::LOG10_2 * x.max(1e-15).log2()
}

/// One-pole coefficient for a time constant in milliseconds.
fn tau_coef(tau_ms: f32, rate: u32) -> f32 {
    let samples = (tau_ms * 1e-3 * rate as f32).max(1.0);
    1.0 - (-1.0 / samples).exp()
}

fn clamp_to(v: f32, r: RangedParam) -> f32 {
    r.clamp(v)
}

fn float_param(name: &str, value: ParamValue) -> Result<f32, ParamError> {
    value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })
}

/// Shared RMS detector: symmetric one-pole mean-square (reactivity =
/// pole time constant). Deliberately NOT asymmetric (fast attack /
/// slow release): switching coefficients per sample biases the estimate
/// upward on oscillating signals, which would shift every threshold
/// by ~1 dB. Ballistics belong to the gain stage, not the detector.
#[derive(Debug)]
struct RmsDetector {
    ms: f32,
    react_ms: f32,
    rate: u32,
    coef: f32,
}

impl RmsDetector {
    fn new(react_ms: f32) -> Self {
        Self { ms: 0.0, react_ms, rate: 0, coef: 0.0 }
    }

    fn ensure_rate(&mut self, rate: u32) {
        if rate != self.rate {
            self.coef = tau_coef(self.react_ms, rate);
            self.rate = rate;
        }
    }

    #[inline]
    fn push(&mut self, x: f32) {
        let x2 = x * x;
        self.ms += self.coef * (x2 - self.ms);
    }
}

/// Downward compressor static curve with soft knee, in dB.
/// Returns the gain (≤ 0) for a detector level.
fn compressor_gain_db(
    level_db: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
) -> f32 {
    let over = level_db - threshold_db;
    let slope = 1.0 / ratio - 1.0; // ≤ 0 for ratio ≥ 1
    if knee_db <= 0.0 {
        if over > 0.0 {
            slope * over
        } else {
            0.0
        }
    } else if over <= -knee_db * 0.5 {
        0.0
    } else if over >= knee_db * 0.5 {
        slope * over
    } else {
        // Quadratic knee: continuous with both branches at ±k/2.
        slope * (over + knee_db * 0.5).powi(2) / (2.0 * knee_db)
    }
}

/// Noise gate: closes below threshold (attenuating by `reduction_db`),
/// opens above it, with hysteresis so the close point sits below the
/// open point (no chattering on borderline noise).
#[derive(Debug)]
pub struct Gate {
    threshold_db: f32,
    reduction_db: f32,
    attack_ms: f32,
    release_ms: f32,
    hysteresis_db: f32,
    open: bool,
    gain: f32,
    closed_gain: f32,
    det: RmsDetector,
    rate: u32,
    atk: f32,
    rel: f32,
}

impl Default for Gate {
    fn default() -> Self {
        let mut g = Self {
            threshold_db: -50.0,
            reduction_db: -12.0,
            attack_ms: 5.0,
            release_ms: 250.0,
            hysteresis_db: 6.0,
            open: false,
            gain: 1.0,
            closed_gain: 1.0,
            det: RmsDetector::new(10.0),
            rate: 0,
            atk: 1.0,
            rel: 1.0,
        };
        g.retune();
        g
    }
}

impl Gate {
    fn retune(&mut self) {
        self.closed_gain = db_to_gain(self.reduction_db);
        if self.rate != 0 {
            self.atk = tau_coef(self.attack_ms, self.rate);
            self.rel = tau_coef(self.release_ms, self.rate);
        }
    }
}

impl AudioNode for Gate {
    fn name(&self) -> &'static str {
        "gate"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        if ctx.rate != self.rate {
            self.rate = ctx.rate;
            self.atk = tau_coef(self.attack_ms, ctx.rate);
            self.rel = tau_coef(self.release_ms, ctx.rate);
        }
        self.det.ensure_rate(ctx.rate);
        for s in 0..ctx.frames {
            // Linked detection: the loudest lane decides for all.
            let mut peak = 0.0f32;
            for i in inputs {
                peak = peak.max(i[s].abs());
            }
            self.det.push(peak);
            let level_db = ms_to_db(self.det.ms);
            if self.open {
                if level_db <= self.threshold_db - self.hysteresis_db {
                    self.open = false;
                }
            } else if level_db >= self.threshold_db {
                self.open = true;
            }
            let target = if self.open { 1.0 } else { self.closed_gain };
            let c = if target > self.gain { self.atk } else { self.rel };
            self.gain += c * (target - self.gain);
            let g = self.gain;
            for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
                o[s] = i[s] * g;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "threshold_db" => {
                self.threshold_db = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: -80.0, max: 0.0, default: -50.0 },
                );
                Ok(())
            }
            "reduction_db" => {
                self.reduction_db = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: -60.0, max: 0.0, default: -12.0 },
                );
                self.retune();
                Ok(())
            }
            "attack_ms" => {
                self.attack_ms = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: 0.1, max: 500.0, default: 5.0 },
                );
                if self.rate != 0 {
                    self.atk = tau_coef(self.attack_ms, self.rate);
                }
                Ok(())
            }
            "release_ms" => {
                self.release_ms = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: 1.0, max: 5000.0, default: 250.0 },
                );
                if self.rate != 0 {
                    self.rel = tau_coef(self.release_ms, self.rate);
                }
                Ok(())
            }
            "hysteresis_db" => {
                self.hysteresis_db = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: 0.0, max: 24.0, default: 6.0 },
                );
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "threshold_db",
                value: ParamValue::Float(self.threshold_db),
                range: Some(RangedParam { min: -80.0, max: 0.0, default: -50.0 }),
                public: true,
            },
            PublicParam {
                name: "reduction_db",
                value: ParamValue::Float(self.reduction_db),
                range: Some(RangedParam { min: -60.0, max: 0.0, default: -12.0 }),
                public: true,
            },
            PublicParam {
                name: "attack_ms",
                value: ParamValue::Float(self.attack_ms),
                range: Some(RangedParam { min: 0.1, max: 500.0, default: 5.0 }),
                public: true,
            },
            PublicParam {
                name: "release_ms",
                value: ParamValue::Float(self.release_ms),
                range: Some(RangedParam { min: 1.0, max: 5000.0, default: 250.0 }),
                public: true,
            },
            PublicParam {
                name: "hysteresis_db",
                value: ParamValue::Float(self.hysteresis_db),
                range: Some(RangedParam { min: 0.0, max: 24.0, default: 6.0 }),
                public: true,
            },
        ]
    }
}

/// Feed-forward downward compressor: RMS detector → soft-knee static
/// curve → dB-domain attack/release smoothing → makeup → output.
#[derive(Debug)]
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    knee_db: f32,
    makeup_db: f32,
    gain_db: f32,
    det: RmsDetector,
    rate: u32,
    atk: f32,
    rel: f32,
}

impl Default for Compressor {
    fn default() -> Self {
        Self {
            threshold_db: -18.0,
            ratio: 3.0,
            attack_ms: 15.0,
            release_ms: 200.0,
            knee_db: 6.0,
            makeup_db: 0.0,
            gain_db: 0.0,
            det: RmsDetector::new(10.0),
            rate: 0,
            atk: 1.0,
            rel: 1.0,
        }
    }
}

impl AudioNode for Compressor {
    fn name(&self) -> &'static str {
        "compressor"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        if ctx.rate != self.rate {
            self.rate = ctx.rate;
            self.atk = tau_coef(self.attack_ms, ctx.rate);
            self.rel = tau_coef(self.release_ms, ctx.rate);
        }
        self.det.ensure_rate(ctx.rate);
        for s in 0..ctx.frames {
            let mut peak = 0.0f32;
            for i in inputs {
                peak = peak.max(i[s].abs());
            }
            self.det.push(peak);
            let level_db = ms_to_db(self.det.ms);
            let target = compressor_gain_db(level_db, self.threshold_db, self.ratio, self.knee_db)
                + self.makeup_db;
            // Attack when MORE reduction is required (gain must fall),
            // release when it can back off.
            let c = if target < self.gain_db { self.atk } else { self.rel };
            self.gain_db += c * (target - self.gain_db);
            let g = db_to_gain(self.gain_db);
            for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
                o[s] = i[s] * g;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "threshold_db" => {
                self.threshold_db = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: -60.0, max: 0.0, default: -18.0 },
                );
                Ok(())
            }
            "ratio" => {
                self.ratio =
                    clamp_to(float_param(name, value)?, RangedParam { min: 1.0, max: 20.0, default: 3.0 });
                Ok(())
            }
            "attack_ms" => {
                self.attack_ms = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: 0.1, max: 500.0, default: 15.0 },
                );
                if self.rate != 0 {
                    self.atk = tau_coef(self.attack_ms, self.rate);
                }
                Ok(())
            }
            "release_ms" => {
                self.release_ms = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: 5.0, max: 5000.0, default: 200.0 },
                );
                if self.rate != 0 {
                    self.rel = tau_coef(self.release_ms, self.rate);
                }
                Ok(())
            }
            "knee_db" => {
                self.knee_db =
                    clamp_to(float_param(name, value)?, RangedParam { min: 0.0, max: 24.0, default: 6.0 });
                Ok(())
            }
            "makeup_db" => {
                self.makeup_db = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: -12.0, max: 24.0, default: 0.0 },
                );
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "threshold_db",
                value: ParamValue::Float(self.threshold_db),
                range: Some(RangedParam { min: -60.0, max: 0.0, default: -18.0 }),
                public: true,
            },
            PublicParam {
                name: "ratio",
                value: ParamValue::Float(self.ratio),
                range: Some(RangedParam { min: 1.0, max: 20.0, default: 3.0 }),
                public: true,
            },
            PublicParam {
                name: "attack_ms",
                value: ParamValue::Float(self.attack_ms),
                range: Some(RangedParam { min: 0.1, max: 500.0, default: 15.0 }),
                public: true,
            },
            PublicParam {
                name: "release_ms",
                value: ParamValue::Float(self.release_ms),
                range: Some(RangedParam { min: 5.0, max: 5000.0, default: 200.0 }),
                public: true,
            },
            PublicParam {
                name: "knee_db",
                value: ParamValue::Float(self.knee_db),
                range: Some(RangedParam { min: 0.0, max: 24.0, default: 6.0 }),
                public: true,
            },
            PublicParam {
                name: "makeup_db",
                value: ParamValue::Float(self.makeup_db),
                range: Some(RangedParam { min: -12.0, max: 24.0, default: 0.0 }),
                public: true,
            },
        ]
    }
}

/// Lookahead-free peak limiter: instant gain reduction when the peak
/// crosses the threshold, release-timed recovery. Output peak can never
/// exceed the threshold (per-sample instantaneous gain).
#[derive(Debug)]
pub struct Limiter {
    threshold_db: f32,
    release_ms: f32,
    /// Current required reduction (dB, ≥ 0).
    reduction_db: f32,
    rate: u32,
    rel: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self { threshold_db: -1.5, release_ms: 50.0, reduction_db: 0.0, rate: 0, rel: 1.0 }
    }
}

impl AudioNode for Limiter {
    fn name(&self) -> &'static str {
        "limiter"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        if ctx.rate != self.rate {
            self.rate = ctx.rate;
            self.rel = tau_coef(self.release_ms, ctx.rate);
        }
        for s in 0..ctx.frames {
            let mut peak = 0.0f32;
            for i in inputs {
                peak = peak.max(i[s].abs());
            }
            let needed = (peak_to_db(peak) - self.threshold_db).max(0.0);
            // Instant attack (needed jumps up immediately), exponential
            // release toward zero, never below what's currently needed.
            let released = self.reduction_db + self.rel * (0.0 - self.reduction_db);
            self.reduction_db = needed.max(released);
            let g = db_to_gain(-self.reduction_db);
            for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
                o[s] = i[s] * g;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "threshold_db" => {
                self.threshold_db = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: -24.0, max: 0.0, default: -1.5 },
                );
                Ok(())
            }
            "release_ms" => {
                self.release_ms = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: 0.5, max: 1000.0, default: 50.0 },
                );
                if self.rate != 0 {
                    self.rel = tau_coef(self.release_ms, self.rate);
                }
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "threshold_db",
                value: ParamValue::Float(self.threshold_db),
                range: Some(RangedParam { min: -24.0, max: 0.0, default: -1.5 }),
                public: true,
            },
            PublicParam {
                name: "release_ms",
                value: ParamValue::Float(self.release_ms),
                range: Some(RangedParam { min: 0.5, max: 1000.0, default: 50.0 }),
                public: true,
            },
        ]
    }
}

/// De-esser: split-band compressor. The LOW band comes from an RBJ
/// lowpass at `freq_hz`; the HIGH band is the exact complement
/// `x − low` (deriving it from a separate highpass would leave the two
/// branches out of phase and the recombination would smear the
/// reduction). The high band alone is detected and gain-reduced when
/// sibilance crosses the threshold; below `freq_hz` the transfer is
/// `out = x` exactly whenever the gain sits at unity.
#[derive(Debug)]
pub struct DeEsser {
    freq_hz: f32,
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    gain_db: f32,
    /// Lowpass coefficients (RBJ, computed on param/rate change).
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    coeff_rate: u32,
    /// Per-channel transposed DF-II state for the low band.
    z: Vec<(f32, f32)>,
    /// Per-quantum low-band scratch (one slot per channel).
    low: Vec<f32>,
    det: RmsDetector,
    rate: u32,
    atk: f32,
    rel: f32,
}

impl Default for DeEsser {
    fn default() -> Self {
        Self {
            freq_hz: 4000.0,
            threshold_db: -22.0,
            ratio: 3.0,
            attack_ms: 2.0,
            release_ms: 50.0,
            gain_db: 0.0,
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            coeff_rate: 0,
            z: Vec::new(),
            low: Vec::new(),
            det: RmsDetector::new(5.0),
            rate: 0,
            atk: 1.0,
            rel: 1.0,
        }
    }
}

impl DeEsser {
    fn retune(&mut self, rate: u32) {
        let [b0, b1, b2, a1, a2] =
            super::nodes::rbj_lowpass(rate as f32, self.freq_hz, std::f32::consts::FRAC_1_SQRT_2);
        self.b0 = b0;
        self.b1 = b1;
        self.b2 = b2;
        self.a1 = a1;
        self.a2 = a2;
        self.coeff_rate = rate;
    }
}

impl AudioNode for DeEsser {
    fn name(&self) -> &'static str {
        "de-esser"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        if ctx.rate != self.rate {
            self.rate = ctx.rate;
            self.atk = tau_coef(self.attack_ms, ctx.rate);
            self.rel = tau_coef(self.release_ms, ctx.rate);
        }
        if self.coeff_rate != ctx.rate {
            self.retune(ctx.rate);
        }
        self.det.ensure_rate(ctx.rate);
        if self.z.len() < inputs.len() {
            self.z.resize(inputs.len(), (0.0, 0.0));
        }
        if self.low.len() < inputs.len() {
            self.low.resize(inputs.len(), 0.0);
        }
        for s in 0..ctx.frames {
            // Split: LOW through the lowpass (per-channel state),
            // HIGH := dry − low (phase-exact by construction).
            let mut high_peak = 0.0f32;
            for (ch, i) in inputs.iter().enumerate() {
                let (z1, z2) = &mut self.z[ch];
                let l = self.b0 * i[s] + *z1;
                *z1 = self.b1 * i[s] - self.a1 * l + *z2;
                *z2 = self.b2 * i[s] - self.a2 * l;
                self.low[ch] = l;
                high_peak = high_peak.max((i[s] - l).abs());
            }
            // Detect on the high band (linked), compute the reduction.
            self.det.push(high_peak);
            let level_db = ms_to_db(self.det.ms);
            let target = compressor_gain_db(level_db, self.threshold_db, self.ratio, 0.0);
            let c = if target < self.gain_db { self.atk } else { self.rel };
            self.gain_db += c * (target - self.gain_db);
            let g = db_to_gain(self.gain_db);
            // Recombine: out = low + g·high = g·dry + (1 − g)·low.
            for (ch, (i, o)) in inputs.iter().zip(outputs.iter_mut()).enumerate() {
                o[s] = g * i[s] + (1.0 - g) * self.low[ch];
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "freq_hz" => {
                self.freq_hz = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: 1000.0, max: 12_000.0, default: 4000.0 },
                );
                if self.coeff_rate != 0 {
                    let rate = self.coeff_rate;
                    self.retune(rate);
                }
                Ok(())
            }
            "threshold_db" => {
                self.threshold_db = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: -60.0, max: 0.0, default: -22.0 },
                );
                Ok(())
            }
            "ratio" => {
                self.ratio =
                    clamp_to(float_param(name, value)?, RangedParam { min: 1.0, max: 20.0, default: 3.0 });
                Ok(())
            }
            "attack_ms" => {
                self.attack_ms = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: 0.1, max: 100.0, default: 2.0 },
                );
                if self.rate != 0 {
                    self.atk = tau_coef(self.attack_ms, self.rate);
                }
                Ok(())
            }
            "release_ms" => {
                self.release_ms = clamp_to(
                    float_param(name, value)?,
                    RangedParam { min: 5.0, max: 1000.0, default: 50.0 },
                );
                if self.rate != 0 {
                    self.rel = tau_coef(self.release_ms, self.rate);
                }
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "freq_hz",
                value: ParamValue::Float(self.freq_hz),
                range: Some(RangedParam { min: 1000.0, max: 12_000.0, default: 4000.0 }),
                public: true,
            },
            PublicParam {
                name: "threshold_db",
                value: ParamValue::Float(self.threshold_db),
                range: Some(RangedParam { min: -60.0, max: 0.0, default: -22.0 }),
                public: true,
            },
            PublicParam {
                name: "ratio",
                value: ParamValue::Float(self.ratio),
                range: Some(RangedParam { min: 1.0, max: 20.0, default: 3.0 }),
                public: true,
            },
            PublicParam {
                name: "attack_ms",
                value: ParamValue::Float(self.attack_ms),
                range: Some(RangedParam { min: 0.1, max: 100.0, default: 2.0 }),
                public: true,
            },
            PublicParam {
                name: "release_ms",
                value: ParamValue::Float(self.release_ms),
                range: Some(RangedParam { min: 5.0, max: 1000.0, default: 50.0 }),
                public: true,
            },
        ]
    }
}

#[cfg(test)]
mod dynamics_tests {
    use super::*;
    use crate::ProcessContext;

    const RATE: u32 = 48_000;

    fn sine(freq: f32, len: usize, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        if x.is_empty() {
            return 0.0;
        }
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn run(node: &mut dyn AudioNode, input: &[f32], chunk: usize) -> Vec<f32> {
        let c = ProcessContext { rate: RATE, frames: chunk };
        let mut out = vec![0.0; input.len()];
        for (ib, ob) in input.chunks(chunk).zip(out.chunks_mut(chunk)) {
            let mut ibp = ib.to_vec();
            ibp.resize(chunk, 0.0);
            let mut obp = vec![0.0; chunk];
            {
                let ins = [ibp.as_slice()];
                let mut outs = [obp.as_mut_slice()];
                node.process(&c, &ins, &mut outs);
            }
            ob.copy_from_slice(&obp[..ob.len()]);
        }
        out
    }

    #[test]
    fn gate_attenuates_quiet_and_passes_loud() {
        let mut g = Gate::default();
        let quiet = sine(220.0, 72_000, 0.001); // 1.5 s @ -60 dBFS peak
        let loud = sine(220.0, 48_000, 0.5); // 1 s @ -6 dBFS
        let mut input = quiet.clone();
        input.extend_from_slice(&loud);
        let out = run(&mut g, &input, 960);

        let q_out = rms(&out[quiet.len() - 14_400..quiet.len()]);
        let q_in = rms(&quiet[quiet.len() - 14_400..]);
        let ratio = q_out / q_in;
        let expect = 10f32.powf(-12.0 / 20.0); // default reduction, -12 dB
        assert!(
            (ratio - expect).abs() < expect * 0.25,
            "closed gain {ratio:.3}, want ~{expect:.3}"
        );

        let l_in = rms(&loud[24_000..]);
        let l_out = rms(&out[quiet.len() + 24_000..]);
        let open_ratio = l_out / l_in;
        assert!((open_ratio - 1.0).abs() < 0.05, "open gate must pass, got {open_ratio:.3}");
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn compressor_matches_static_curve() {
        let mut c = Compressor::default(); // -18 dB, 3:1, knee 6, no makeup
        let input = sine(300.0, 96_000, 0.5); // 2 s, level ≈ -9 dBFS
        let out = run(&mut c, &input, 960);
        let in_r = rms(&input[48_000..]);
        let out_r = rms(&out[48_000..]);
        // Independent expectation: static curve evaluated on the
        // *measured* input level (well above the knee → linear region).
        let level_db = 20.0 * in_r.log10();
        let expect_db = (level_db - (-18.0)) * (1.0 / 3.0 - 1.0);
        let expect = 10f32.powf(expect_db / 20.0); // independent oracle
        let ratio = out_r / in_r;
        assert!(
            (ratio - expect).abs() < expect * 0.12,
            "gain {ratio:.3}, curve says {expect:.3}"
        );
    }

    #[test]
    fn compressor_ratio_one_is_unity() {
        let mut c = Compressor::default();
        c.set_param("ratio", ParamValue::Float(1.0)).unwrap();
        let input = sine(300.0, 48_000, 0.5);
        let out = run(&mut c, &input, 960);
        for (a, b) in input.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-4, "ratio 1 must be a no-op");
        }
    }

    #[test]
    fn compressor_makeup_applies_static_gain() {
        let mut c = Compressor::default();
        c.set_param("ratio", ParamValue::Float(1.0)).unwrap();
        c.set_param("makeup_db", ParamValue::Float(3.0)).unwrap();
        let input = sine(300.0, 48_000, 0.3);
        let out = run(&mut c, &input, 960);
        let ratio = rms(&out[24_000..]) / rms(&input[24_000..]);
        let expect = 10f32.powf(3.0 / 20.0); // +3 dB = 1.413
        assert!((ratio - expect).abs() < expect * 0.05, "makeup ratio {ratio:.3} != {expect:.3}");
    }

    #[test]
    fn limiter_caps_peak_and_passes_below() {
        let mut l = Limiter::default(); // -1.5 dBFS
        let hot = sine(440.0, 48_000, 1.0);
        let out = run(&mut l, &hot, 960);
        let peak = out.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        let cap = 10f32.powf(-1.5 / 20.0); // 0.8414
        assert!(peak <= cap * 1.002, "peak {peak:.4} exceeds cap {cap:.4}");
        assert!(peak > cap * 0.95, "limiter never reached the cap: {peak:.4}");

        let mut l2 = Limiter::default();
        let cool = sine(440.0, 24_000, 0.1); // -20 dBFS: below threshold
        let out2 = run(&mut l2, &cool, 960);
        for (a, b) in cool.iter().zip(out2.iter()) {
            assert!((a - b).abs() < 1e-6, "below threshold must be exact");
        }
    }

    #[test]
    fn deesser_reduces_sibilance_spares_lows() {
        let mut d = DeEsser::default();
        let hf = sine(6_300.0, 96_000, 0.8);
        let out = run(&mut d, &hf, 960);
        let ratio = rms(&out[48_000..]) / rms(&hf[48_000..]);
        assert!(ratio < 0.6, "sibilance not reduced: ratio {ratio:.3}");

        let mut d2 = DeEsser::default();
        let lf = sine(500.0, 48_000, 0.8);
        let out2 = run(&mut d2, &lf, 960);
        let mean_diff: f32 =
            lf.iter().zip(out2.iter()).map(|(a, b)| (a - b).abs()).sum::<f32>() / lf.len() as f32;
        assert!(mean_diff < 0.8 * 0.03, "LF path altered: mean diff {mean_diff:.4}");
        assert!(out2.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn dyn_param_validation_and_clamping() {
        let mut g = Gate::default();
        assert!(matches!(
            g.set_param("nope", ParamValue::Float(1.0)),
            Err(ParamError::Unknown(_))
        ));
        assert!(matches!(
            g.set_param("threshold_db", ParamValue::Text("x".into())),
            Err(ParamError::TypeMismatch { .. })
        ));
        g.set_param("threshold_db", ParamValue::Float(-999.0)).unwrap(); // clamps to -80
        let p = g.params().into_iter().find(|p| p.name == "threshold_db").unwrap();
        assert_eq!(p.value, ParamValue::Float(-80.0));

        // Int coercion (the UI sends ints for int-ish slider steps).
        let mut c = Compressor::default();
        c.set_param("attack_ms", ParamValue::Int(25)).unwrap();
        let p = c.params().into_iter().find(|p| p.name == "attack_ms").unwrap();
        assert_eq!(p.value, ParamValue::Float(25.0));

        let mut l = Limiter::default();
        assert!(l.set_param("ratio", ParamValue::Float(3.0)).is_err()); // not a limiter param
    }

    #[test]
    fn factory_creates_dynamics_and_denoise() {
        let f = crate::factory::mvp_factory();
        let ctx = crate::NodeConstructorContext { rate: 48_000, max_channels: 2 };
        for name in ["gate", "compressor", "limiter", "de-esser", "denoise"] {
            let n = f.create(name, &ctx).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(n.name(), name);
        }
        assert!(f.create("nope", &ctx).is_err());
    }
}
