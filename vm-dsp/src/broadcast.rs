//! Broadcast-style processors (F-0.4.A): automatic loudness leveling.
//!
//! `autogain`: EBU R128-inspired gain rider. K-weighted measurement
//! (the exact libebur128 48 kHz pre-filter + RLB coefficients), 400 ms
//! momentary blocks at 100 ms hops, absolute (−70 LUFS) + relative
//! (−10 LU) gating, and a dB-domain follower driving the output toward
//! `target_lufs` within `max_gain_db` of lift. Whispers come up, shouts
//! come down, silence never explodes: below-silence blocks are gated
//! out of the measurement and the follower freezes.

use super::node::{AudioNode, ParamError, ProcessContext};
use super::param::{ParamValue, PublicParam, RangedParam};

/// libebur128 48 kHz K-weighting, stage 1 (pre-filter shelf). The f64
/// reference printout is kept verbatim (f32 rounds it at compile time);
/// truncating the literals would obscure the provenance.
#[allow(clippy::excessive_precision)]
const PRE_B: [f32; 3] = [1.53512485958697, -2.69169618940638, 1.19839281085285];
#[allow(clippy::excessive_precision)]
const PRE_A: [f32; 3] = [1.0, -1.69065929318241, 0.73248077421585];
/// Stage 2 (RLB high-pass).
#[allow(clippy::excessive_precision)]
const RLB_B: [f32; 3] = [1.0, -2.0, 1.0];
#[allow(clippy::excessive_precision)]
const RLB_A: [f32; 3] = [1.0, -1.99004745483398, 0.99007225036621];

/// Momentary block (400 ms) and hop (100 ms) at 48 kHz.
const BLOCK: usize = 19_200;
const HOP: usize = 4_800;
/// -70 LUFS absolute gate in mean-square.
const ABS_GATE_MS: f32 = 1e-7;
/// -10 LU relative gate below the ungated integrated level.
const REL_GATE_DB: f32 = -10.0;

/// Direct-form I biquad step (measurement path only).
fn biquad_step(x: f32, b: &[f32; 3], a: &[f32; 3], z1: &mut f32, z2: &mut f32) -> f32 {
    let y = b[0] * x + *z1;
    *z1 = b[1] * x - a[1] * y + *z2;
    *z2 = b[2] * x - a[2] * y;
    y
}

#[derive(Debug)]
pub struct Autogain {
    target_lufs: f32,
    max_gain_db: f32,
    adapt_ms: f32,
    /// K-filter state (shared across channels: one measurement).
    pre_z: (f32, f32),
    rlb_z: (f32, f32),
    /// Sliding 400 ms window of squared K-weighted samples + its sum.
    ring: std::collections::VecDeque<f32>,
    ring_sum: f64,
    /// Recent block levels for the integrated estimate (1 s window).
    history: Vec<f32>,
    /// Follower target in dB (written by the measurement, read by the slew).
    target_db: f32,
    /// Currently applied gain in dB (slewed output state, persists
    /// across quanta — never overwritten by the target).
    gain_db: f32,
    /// Samples since the last hop (block advance cadence).
    since_hop: usize,
}

impl Default for Autogain {
    fn default() -> Self {
        Self {
            target_lufs: -23.0,
            max_gain_db: 20.0,
            adapt_ms: 500.0,
            pre_z: (0.0, 0.0),
            rlb_z: (0.0, 0.0),
            ring: std::collections::VecDeque::with_capacity(BLOCK + HOP),
            ring_sum: 0.0,
            history: Vec::new(),
            target_db: 0.0,
            gain_db: 0.0,
            since_hop: 0,
        }
    }
}

impl Autogain {
    /// Feed one sample into the K-weighted measurement; every HOP
    /// samples the finished 400 ms sliding block updates the integrated
    /// level and the follower gain. Pure measurement — no audio touched.
    fn observe(&mut self, x: f32) {
        let y = biquad_step(x, &PRE_B, &PRE_A, &mut self.pre_z.0, &mut self.pre_z.1);
        let y = biquad_step(y, &RLB_B, &RLB_A, &mut self.rlb_z.0, &mut self.rlb_z.1);
        self.ring.push_back(y * y);
        self.ring_sum += (y as f64) * (y as f64);
        while self.ring.len() > BLOCK {
            if let Some(old) = self.ring.pop_front() {
                self.ring_sum -= old as f64;
            }
        }
        self.since_hop += 1;
        if self.since_hop >= HOP {
            self.since_hop = 0;
            let ms = (self.ring_sum / self.ring.len().max(1) as f64) as f32;
            if ms >= ABS_GATE_MS {
                self.history.push(ms);
                if self.history.len() > 10 {
                    self.history.remove(0);
                }
            }
            self.update_follower();
        }
    }

    /// Recompute the follower target from the gated integrated level.
    fn update_follower(&mut self) {
        if self.history.is_empty() {
            return; // silence: freeze, never wind up
        }
        let mean = self.history.iter().sum::<f32>() / self.history.len() as f32;
        // Relative gate: blocks 10+ LU below the mean don't count.
        let floor = mean * 10f32.powf(REL_GATE_DB / 10.0);
        let gated: Vec<f32> =
            self.history.iter().copied().filter(|m| *m >= floor).collect();
        if gated.is_empty() {
            return;
        }
        let integrated = gated.iter().sum::<f32>() / gated.len() as f32;
        let lufs = 10.0 * integrated.max(1e-12).log10() - 0.691;
        let mut want = self.target_lufs - lufs;
        if want > self.max_gain_db {
            want = self.max_gain_db;
        }
        // Follower slew is applied per-sample in process(); store target.
        self.target_db = want;
    }

    /// Smoothing coefficient for `adapt_ms` at 48 kHz.
    fn coef(&self) -> f32 {
        let n = (self.adapt_ms.max(1.0) * 48.0).max(1.0);
        1.0 - (-1.0 / n).exp()
    }
}

impl AudioNode for Autogain {
    fn name(&self) -> &'static str {
        "autogain"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let _ = ctx.rate; // K coefficients are calibrated for 48 kHz (the pipeline rate).
        // Mono measurement: average across channels (EBU sums channels;
        // averaging keeps the calibration for any channel count).
        let frames = outputs.first().map(|l| l.len()).unwrap_or(0);
        let coef = self.coef();
        let mut applied = self.gain_db;
        for s in 0..frames {
            let mut m = 0.0f32;
            for i in inputs {
                m += i[s];
            }
            self.observe(m / inputs.len().max(1) as f32);
            // Slew the persistent output state toward the follower
            // target (never the reverse: the target must survive).
            applied += coef * (self.target_db - applied);
            let g = 10f32.powf(applied / 20.0);
            for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
                o[s] = i[s] * g;
            }
        }
        self.gain_db = applied;
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        let v = value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
        match name {
            "target_lufs" => {
                self.target_lufs =
                    RangedParam { min: -40.0, max: -10.0, default: -23.0 }.clamp(v);
                Ok(())
            }
            "max_gain_db" => {
                self.max_gain_db =
                    RangedParam { min: 0.0, max: 30.0, default: 20.0 }.clamp(v);
                Ok(())
            }
            "adapt_ms" => {
                self.adapt_ms =
                    RangedParam { min: 10.0, max: 5000.0, default: 500.0 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    /// Sample-synchronous gain riding: no lookahead, no added latency.
    fn latency_samples(&self, _rate: u32) -> usize {
        0
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "target_lufs",
                value: ParamValue::Float(self.target_lufs),
                range: Some(RangedParam { min: -40.0, max: -10.0, default: -23.0 }),
                public: true,
            },
            PublicParam {
                name: "max_gain_db",
                value: ParamValue::Float(self.max_gain_db),
                range: Some(RangedParam { min: 0.0, max: 30.0, default: 20.0 }),
                public: true,
            },
            PublicParam {
                name: "adapt_ms",
                value: ParamValue::Float(self.adapt_ms),
                range: Some(RangedParam { min: 10.0, max: 5000.0, default: 500.0 }),
                public: true,
            },
        ]
    }
}

#[cfg(test)]
mod broadcast_tests {
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

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn sine(freq: f32, len: usize, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin())
            .collect()
    }

    #[test]
    fn k_weighting_passes_440hz_near_unity() {
        // Direct filter check: a 440 Hz sine must come out at ~0 dB
        // (Python-verified textbook K curve: +0.01 dB).
        let input = sine(440.0, 48_000, 0.5);
        let mut z = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut acc = 0.0f64;
        for s in &input {
            let y = biquad_step(*s, &PRE_B, &PRE_A, &mut z.0, &mut z.1);
            let y = biquad_step(y, &RLB_B, &RLB_A, &mut z.2, &mut z.3);
            acc += (y as f64) * (y as f64);
        }
        let ms = (acc / input.len() as f64) as f32;
        let in_ms = rms(&input).powi(2);
        let ratio_db = 10.0 * (ms / in_ms).log10();
        assert!(
            ratio_db.abs() < 0.5,
            "K filters must pass 440 Hz near unity, got {ratio_db:.2} dB"
        );
    }

    #[test]
    fn autogain_drives_loud_toward_target() {
        let mut g = Autogain::default();
        // -6 dBFS sine: must come down toward -23 LUFS.
        let input = sine(440.0, 96_000, 0.5);
        let out = run(&mut g, &input, 960);
        assert!(out.iter().all(|s| s.is_finite()));
        let in_r = rms(&input[72_000..]);
        let out_r = rms(&out[72_000..]);
        assert!(out_r < in_r * 0.5, "loud input must be turned down");
    }

    #[test]
    fn autogain_lifts_quiet_within_cap() {
        let mut g = Autogain::default();
        // -40 dBFS sine: lifted, but never past +20 dB of gain.
        let input = sine(440.0, 96_000, 0.01);
        let out = run(&mut g, &input, 960);
        let in_r = rms(&input[72_000..]);
        let out_r = rms(&out[72_000..]);
        assert!(out_r > in_r * 2.0, "quiet input must be lifted");
        assert!(out_r < in_r * 12.0, "lift capped at +20 dB (10x)");
    }

    #[test]
    fn autogain_freezes_on_silence() {
        let mut g = Autogain::default();
        let out = run(&mut g, &vec![0.0; 48_000], 960);
        assert!(out.iter().all(|s| *s == 0.0), "silence in, silence out");
        // Gain must not have wound up: feed a nominal tone right after,
        // it must not blast.
        let out2 = run(&mut g, &sine(440.0, 9_600, 0.1), 960);
        assert!(rms(&out2).max(0.0) < 1.0, "no windup blast after silence");
    }

    #[test]
    fn autogain_applies_equal_gain_stereo() {
        let mut g = Autogain::default();
        let c = ProcessContext { rate: RATE, frames: 960 };
        let l = sine(440.0, 9_600, 0.4);
        let r: Vec<f32> = l.iter().map(|s| s * 0.5).collect();
        let mut ol = vec![0.0; 9_600];
        let mut or = vec![0.0; 9_600];
        for ((il, ir), (olane, orlane)) in
            l.chunks(960).zip(r.chunks(960)).zip(ol.chunks_mut(960).zip(or.chunks_mut(960)))
        {
            let ins = [il, ir];
            let mut outs = [olane, orlane];
            g.process(&c, &ins, &mut outs);
        }
        // Same gain both lanes: the 2:1 input ratio survives exactly.
        for (a, b) in ol.iter().zip(or.iter()) {
            if a.abs() > 1e-6 {
                assert!((a / b - 2.0).abs() < 1e-4, "stereo image must not shift");
            }
        }
    }
}
