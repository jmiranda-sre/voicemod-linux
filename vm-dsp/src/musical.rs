//! Musical processors (F-0.4.A): real-time pitch correction.
//!
//! `autotune`: YIN-lite pitch detection per quantum, quantization to a
//! key/scale, and correction through the phase-vocoder [`PitchEngine`]
//! (fractional transpose, formants preserved via `warp_k = β`). Fast
//! `speed_ms` = hard T-Pain snap; slow = gentle nudge. Silence and
//! unvoiced frames hold the last correction instead of jumping.

use super::node::{AudioNode, ParamError, ProcessContext};
use super::param::{ParamValue, PublicParam, RangedParam};
use super::spectral::PitchEngine;
use std::collections::VecDeque;

/// Detection window (43 ms @ 48 kHz) and voice range.
const DET_N: usize = 2048;
const FMIN: f32 = 55.0;
const FMAX: f32 = 1200.0;
/// RMS below this (-80 dBFS) counts as silence: hold, don't detect.
const SILENCE_RMS: f32 = 1e-4;
/// YIN absolute threshold on the normalized difference.
const YIN_THRESH: f32 = 0.15;

const MAJOR: [i32; 7] = [0, 2, 4, 5, 7, 9, 11];
const MINOR: [i32; 7] = [0, 2, 3, 5, 7, 8, 10];

/// YIN-lite fundamental estimate. Returns `None` for silence/unvoiced.
fn estimate_f0(x: &[f32], rate: u32) -> Option<f32> {
    if x.len() < DET_N {
        return None;
    }
    let frame = &x[x.len() - DET_N..];
    let mean_sq: f32 = frame.iter().map(|s| s * s).sum::<f32>() / DET_N as f32;
    if mean_sq.sqrt() < SILENCE_RMS {
        return None;
    }
    let min_lag = (rate as f32 / FMAX).floor() as usize;
    let max_lag = ((rate as f32 / FMIN).ceil() as usize).min(DET_N / 2);
    // Difference function.
    let mut df = vec![0.0f32; max_lag + 1];
    for lag in min_lag..=max_lag {
        let mut acc = 0.0f32;
        for i in 0..DET_N - max_lag {
            let d = frame[i] - frame[i + lag];
            acc += d * d;
        }
        df[lag] = acc;
    }
    // Cumulative-mean-normalized difference (the YIN curve).
    let mut cmnd = vec![1.0f32; max_lag + 1];
    let mut run = 0.0f32;
    for lag in min_lag..=max_lag {
        run += df[lag];
        cmnd[lag] = if run > 1e-12 { df[lag] * lag as f32 / run } else { 1.0 };
    }
    // First dip below threshold, then walk to the local minimum.
    let mut best = None;
    for lag in min_lag..=max_lag {
        if cmnd[lag] < YIN_THRESH {
            let mut b = lag;
            while b < max_lag && cmnd[b + 1] < cmnd[b] {
                b += 1;
            }
            best = Some(b);
            break;
        }
    }
    let lag = best?;
    // Parabolic interpolation around the minimum (on the YIN curve).
    let y0 = cmnd[lag.saturating_sub(1).max(min_lag)];
    let y1 = cmnd[lag];
    let y2 = cmnd[(lag + 1).min(max_lag)];
    let denom = (y0 - 2.0 * y1 + y2).abs().max(1e-12);
    let shift = 0.5 * (y0 - y2) / denom;
    let period = (lag as f32 + shift.clamp(-1.0, 1.0)).max(1.0);
    Some(rate as f32 / period)
}

/// Nearest scale degree to `midi` (fractional) with `key` root.
/// Returns the target MIDI note (fractional degrees snap to integers).
fn quantize(midi: f32, key: i32, scale: &[i32]) -> f32 {
    let base_oct = ((midi - key as f32) / 12.0).round() as i32;
    let mut best = midi;
    let mut best_dist = f32::INFINITY;
    for oct in [base_oct - 1, base_oct, base_oct + 1] {
        for d in scale {
            let t = key as f32 + oct as f32 * 12.0 + *d as f32;
            let dist = (t - midi).abs();
            if dist < best_dist {
                best_dist = dist;
                best = t;
            }
        }
    }
    best
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scale {
    Major,
    Minor,
    Chromatic,
}

impl Scale {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "major" => Some(Self::Major),
            "minor" => Some(Self::Minor),
            "chromatic" => Some(Self::Chromatic),
            _ => None,
        }
    }

    fn degrees(&self) -> &[i32] {
        static CHROMATIC: [i32; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        match self {
            Self::Major => &MAJOR,
            Self::Minor => &MINOR,
            Self::Chromatic => &CHROMATIC,
        }
    }
}

pub struct Autotune {
    engine: PitchEngine,
    /// -1 = chromatic (snap to semitones), else 0..11 = key root (C..B).
    key: i32,
    scale: Scale,
    mix: f32,
    speed_ms: f32,
    /// Current correction in semitones (slewed state).
    correction_st: f32,
    /// Detection history (mono average).
    hist: VecDeque<f32>,
}

impl Default for Autotune {
    fn default() -> Self {
        Self {
            engine: PitchEngine::new(),
            key: 0,
            scale: Scale::Major,
            mix: 1.0,
            speed_ms: 40.0,
            correction_st: 0.0,
            hist: VecDeque::with_capacity(DET_N * 2),
        }
    }
}

impl Autotune {
    fn retune_engine(&mut self) {
        let beta = 2.0f32.powf(self.correction_st / 12.0).clamp(0.5, 2.0);
        self.engine.pitch = beta;
        // Formants preserved: warp k = β (see FormantPitchShifter).
        self.engine.warp_k = beta;
    }
}

impl AudioNode for Autotune {
    fn name(&self) -> &'static str {
        "autotune"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        // Mono detection: average channels (one decision, no image shift).
        for s in 0..ctx.frames {
            let mut m = 0.0f32;
            for i in inputs {
                m += i[s];
            }
            self.hist.push_back(m / inputs.len().max(1) as f32);
            while self.hist.len() > DET_N {
                self.hist.pop_front();
            }
        }
        // Detect → quantize → slew the correction (per quantum is smooth
        // enough: the vocoder interpolates ratio changes internally).
        if self.hist.len() >= DET_N {
            let flat: Vec<f32> = self.hist.iter().copied().collect();
            if let Some(f0) = estimate_f0(&flat, ctx.rate) {
                let midi = 69.0 + 12.0 * (f0 / 440.0).log2();
                let key = if self.scale == Scale::Chromatic { 0 } else { self.key };
                let degrees: &[i32] = if self.scale == Scale::Chromatic {
                    Scale::Chromatic.degrees()
                } else {
                    self.scale.degrees()
                };
                let target = quantize(midi, key, degrees);
                let want = target - midi;
                // Slew in semitones (speed_ms time constant per quantum).
                let quanta_ms = ctx.frames as f32 * 1000.0 / ctx.rate.max(1) as f32;
                let c = 1.0 - (-quanta_ms / self.speed_ms.max(1.0)).exp();
                self.correction_st += c * (want - self.correction_st);
                self.correction_st = self.correction_st.clamp(-12.0, 12.0);
                self.retune_engine();
            }
            // Unvoiced/silent: hold (never jump).
        }
        // Wet through the engine, then dry/wet mix.
        let mut wet: Vec<Vec<f32>> = inputs.iter().map(|l| vec![0.0; l.len()]).collect();
        {
            let ins: Vec<&[f32]> = inputs.iter().map(|l| l.as_ref()).collect();
            let mut outs: Vec<&mut [f32]> = wet.iter_mut().map(|l| l.as_mut_slice()).collect();
            self.engine.process(&ins, &mut outs);
        }
        for ((i, w), o) in inputs.iter().zip(wet.iter()).zip(outputs.iter_mut()) {
            for ((s, wv), d) in i.iter().zip(w.iter()).zip(o.iter_mut()) {
                *d = s * (1.0 - self.mix) + wv * self.mix;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "key" => {
                let v = value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                let k = v.round() as i32;
                if !(-1..=11).contains(&k) {
                    return Err(ParamError::InvalidValue(format!("key {v}, want -1..=11")));
                }
                self.key = k;
                Ok(())
            }
            "scale" => {
                let s = match value {
                    ParamValue::Text(s) => s,
                    _ => return Err(ParamError::TypeMismatch { name: name.into() }),
                };
                self.scale =
                    Scale::parse(&s).ok_or_else(|| ParamError::InvalidValue(format!("scale '{s}'")))?;
                Ok(())
            }
            "mix" => {
                let v = value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                self.mix = RangedParam { min: 0.0, max: 1.0, default: 1.0 }.clamp(v);
                Ok(())
            }
            "speed_ms" => {
                let v = value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                self.speed_ms = RangedParam { min: 1.0, max: 500.0, default: 40.0 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    /// Same phase-vocoder analysis frame as pitch-fft.
    fn latency_samples(&self, _rate: u32) -> usize {
        super::spectral::FRAME
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "key",
                value: ParamValue::Int(self.key),
                range: Some(RangedParam { min: -1.0, max: 11.0, default: 0.0 }),
                public: true,
            },
            PublicParam {
                name: "scale",
                value: ParamValue::Text(
                    match self.scale {
                        Scale::Major => "major",
                        Scale::Minor => "minor",
                        Scale::Chromatic => "chromatic",
                    }
                    .into(),
                ),
                range: None,
                public: true,
            },
            PublicParam {
                name: "mix",
                value: ParamValue::Float(self.mix),
                range: Some(RangedParam { min: 0.0, max: 1.0, default: 1.0 }),
                public: true,
            },
            PublicParam {
                name: "speed_ms",
                value: ParamValue::Float(self.speed_ms),
                range: Some(RangedParam { min: 1.0, max: 500.0, default: 40.0 }),
                public: true,
            },
        ]
    }
}

#[cfg(test)]
mod musical_tests {
    use super::*;
    use crate::ProcessContext;

    const RATE: u32 = 48_000;

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

    fn sine(freq: f32, len: usize, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin())
            .collect()
    }

    fn freq_of(x: &[f32]) -> f32 {
        let mut crossings = 0;
        for w in x.windows(2) {
            if (w[0] <= 0.0) != (w[1] <= 0.0) {
                crossings += 1;
            }
        }
        crossings as f32 * RATE as f32 / (2.0 * x.len() as f32)
    }

    #[test]
    fn yin_detects_sine_and_rejects_silence() {
        let s = sine(440.0, DET_N, 0.5);
        let f0 = estimate_f0(&s, RATE).expect("voiced sine must detect");
        assert!((f0 - 440.0).abs() < 5.0, "f0 = {f0:.1}");
        assert!(estimate_f0(&vec![0.0; DET_N], RATE).is_none());
        assert!(estimate_f0(&s[..DET_N / 2], RATE).is_none(), "short window: no detect");
    }

    #[test]
    fn quantize_snaps_to_scale() {
        // 450 Hz ≈ A+39ct: C major has no A#, so target is A 440.
        let midi = 69.0 + 12.0 * (450.0f32 / 440.0).log2();
        let t = quantize(midi, 0, Scale::Major.degrees());
        assert!((t - 69.0).abs() < 1e-6, "target = {t}");
        // Chromatic snaps to the nearest semitone instead.
        let t = quantize(midi, 0, Scale::Chromatic.degrees());
        assert!((t - 69.0).abs() < 1e-6);
        // D minor: F4 (349 Hz) is degree 3 — holds.
        let midi_f = 69.0 + 12.0 * (349.23f32 / 440.0).log2();
        let t = quantize(midi_f, 2, Scale::Minor.degrees());
        assert!((t - midi_f).abs() < 0.02, "in-scale note holds");
    }

    #[test]
    fn autotune_pulls_sharp_a_down_to_scale() {
        let mut a = Autotune::default();
        let out = run(&mut a, &sine(450.0, 144_000, 0.5), 960);
        assert!(out.iter().all(|s| s.is_finite()));
        // Steady state (detector + vocoder warmed): pulled toward 440.
        let f = freq_of(&out[96_000..]);
        assert!(f < 445.0 && f > 425.0, "corrected f = {f:.1} Hz (want ≈440)");
    }

    #[test]
    fn autotune_leaves_in_scale_notes_alone() {
        let mut a = Autotune::default();
        let out = run(&mut a, &sine(440.0, 144_000, 0.5), 960);
        let f = freq_of(&out[96_000..]);
        assert!((f - 440.0).abs() < 440.0 * 0.03, "in-scale f = {f:.1} Hz");
    }

    #[test]
    fn autotune_params_validate() {
        let mut a = Autotune::default();
        a.set_param("key", ParamValue::Int(7)).unwrap();
        assert!(matches!(
            a.set_param("key", ParamValue::Float(99.0)),
            Err(ParamError::InvalidValue(_))
        ));
        a.set_param("scale", ParamValue::Text("minor".into())).unwrap();
        assert!(matches!(
            a.set_param("scale", ParamValue::Text("dorian".into())),
            Err(ParamError::InvalidValue(_))
        ));
        assert!(matches!(
            a.set_param("scale", ParamValue::Float(1.0)),
            Err(ParamError::TypeMismatch { .. })
        ));
        assert!(matches!(
            a.set_param("nope", ParamValue::Float(1.0)),
            Err(ParamError::Unknown(_))
        ));
    }
}
