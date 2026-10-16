//! Neural denoisers (F-0.4.A).
//!
//! `rnnoise`: Xiph RNNoise via the pure-Rust `nnnoiseless` port (model
//! weights embedded, no files, no runtime download) — the same
//! algorithm behind EasyEffects' "Noise reduction". Runs on exact
//! 10 ms frames (480 samples @ 48 kHz, the model's native rate) with a
//! per-channel FIFO, so any graph quantum works and the added latency
//! is exactly one frame.

use super::node::{AudioNode, ParamError, ProcessContext};
use super::param::{ParamValue, PublicParam, RangedParam};
use std::collections::VecDeque;

/// RNNoise native frame (10 ms @ 48 kHz).
const FRAME: usize = nnnoiseless::DenoiseState::FRAME_SIZE;

struct RnCh {
    pending: VecDeque<f32>,
    out: VecDeque<f32>,
    state: Box<nnnoiseless::DenoiseState<'static>>,
}

impl RnCh {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            out: VecDeque::new(),
            state: nnnoiseless::DenoiseState::new(),
        }
    }
}

#[derive(Default)]
pub struct Rnnoise {
    mix: f32,
    ch: Vec<RnCh>,
}

impl Rnnoise {
    pub(crate) fn new() -> Self {
        Self { mix: 1.0, ch: Vec::new() }
    }
}

impl AudioNode for Rnnoise {
    fn name(&self) -> &'static str {
        "rnnoise"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let _ = ctx.rate; // RNNoise is natively 48 kHz (the pipeline rate).
        while self.ch.len() < inputs.len() {
            self.ch.push(RnCh::new());
        }
        let mix = self.mix;
        for ((input, output), ch) in inputs.iter().zip(outputs.iter_mut()).zip(self.ch.iter_mut()) {
            ch.pending.extend(input.iter());
            while ch.pending.len() >= FRAME {
                let mut frame = [0.0f32; FRAME];
                for s in frame.iter_mut() {
                    *s = ch.pending.pop_front().unwrap_or(0.0);
                }
                // nnnoiseless speaks 16-bit-scale PCM (±32768), not ±1.0
                // float: scale at the boundary, clamp for hot makeup.
                for s in frame.iter_mut() {
                    *s = (*s * 32768.0).clamp(-32768.0, 32768.0);
                }
                let mut wet = [0.0f32; FRAME];
                ch.state.process_frame(&mut wet, &frame);
                for s in wet.iter_mut() {
                    *s /= 32768.0;
                }
                for (i, w) in wet.iter().enumerate() {
                    ch.out.push_back(frame[i] / 32768.0 * (1.0 - mix) + w * mix);
                }
            }
            // Emit exactly input.len(): the FIFO covers the one-frame
            // startup gap, then streams steadily.
            for o in output.iter_mut() {
                *o = ch.out.pop_front().unwrap_or(0.0);
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "mix" => {
                let v = value
                    .as_float()
                    .ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                self.mix = RangedParam { min: 0.0, max: 1.0, default: 1.0 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    /// One exact frame of group delay (no overlap, no lookahead).
    fn latency_samples(&self, _rate: u32) -> usize {
        FRAME
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![PublicParam {
            name: "mix",
            value: ParamValue::Float(self.mix),
            range: Some(RangedParam { min: 0.0, max: 1.0, default: 1.0 }),
            public: true,
        }]
    }
}

#[cfg(test)]
mod neural_tests {
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

    /// Deterministic uniform white noise in ±amp (xorshift64).
    fn noise(len: usize, amp: f32) -> Vec<f32> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let u = (state >> 32) as u32 as f32 / u32::MAX as f32;
                (u * 2.0 - 1.0) * amp
            })
            .collect()
    }

    #[test]
    fn rnnoise_suppresses_room_like_noise() {
        let mut n = Rnnoise::new();
        assert_eq!(n.latency_samples(RATE), 480);
        // Pink bed (room-like spectrum — the model's trained regime):
        // deep suppression expected. (Pure white noise is
        // off-distribution for RNNoise's VAD-gated gains and only
        // partially suppresses; pair with a gate for pauses, as every
        // real chain does.)
        let mut lp = 0.0f32;
        let pink: Vec<f32> = noise(96_000, 0.06)
            .into_iter()
            .map(|w| {
                lp += 0.08 * (w - lp);
                lp
            })
            .collect();
        let out = run(&mut n, &pink, 960);
        assert!(out.iter().all(|s| s.is_finite()));
        let in_r = rms(&pink[48_000..]);
        let out_r = rms(&out[48_000..]);
        assert!(
            out_r < in_r * 0.1,
            "RNNoise must crush room noise: in {in_r:.4}, out {out_r:.4}"
        );
    }

    #[test]
    fn rnnoise_keeps_speech_drops_the_bed() {
        // Harmonic complex (speech-like) buried in white noise: the tone
        // must survive near-intact while the total drops to ~tone level.
        let mut n = Rnnoise::new();
        let nz = noise(96_000, 0.1);
        let tone: Vec<f32> = (0..96_000)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                0.20 * (2.0 * std::f32::consts::PI * 150.0 * t).sin()
                    + 0.12 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
                    + 0.08 * (2.0 * std::f32::consts::PI * 450.0 * t).sin()
            })
            .collect();
        let mix: Vec<f32> = tone.iter().zip(nz.iter()).map(|(s, n)| s + n).collect();
        let out = run(&mut n, &mix, 960);
        assert!(out.iter().all(|s| s.is_finite()));
        let tone_r = rms(&tone[48_000..]);
        let out_r = rms(&out[48_000..]);
        assert!(out_r > tone_r * 0.85, "speech must survive: tone {tone_r:.4}, out {out_r:.4}");
        assert!(out_r < rms(&mix[48_000..]), "total must drop toward tone-only level");
    }

    #[test]
    fn rnnoise_mix_zero_is_passthrough() {
        let mut n = Rnnoise::new();
        n.set_param("mix", ParamValue::Float(0.0)).unwrap();
        let input = noise(9_600, 0.3);
        let out = run(&mut n, &input, 960);
        // No lookahead: the first quantum already yields processed
        // audio (dry here), so the whole output matches sample-exact.
        assert_eq!(out.len(), input.len());
        for (a, b) in out.iter().zip(input.iter()) {
            assert!((a - b).abs() < 1e-6, "mix=0 must pass through");
        }
    }

    #[test]
    fn crate_direct_matches_node_on_pink() {
        // Regression guard for the FIFO/scaling plumbing: the node must
        // reproduce raw nnnoiseless sample-exact on identical input.
        let mut lp = 0.0f32;
        let pink: Vec<f32> = noise(48_000, 0.06)
            .into_iter()
            .map(|w| {
                lp += 0.08 * (w - lp);
                lp
            })
            .collect();
        let mut dns = nnnoiseless::DenoiseState::new();
        let mut out = vec![0.0f32; pink.len()];
        for (ib, ob) in pink
            .as_chunks::<480>()
            .0
            .iter()
            .zip(out.as_chunks_mut::<480>().0.iter_mut())
        {
            let inp: Vec<f32> = ib.iter().map(|s| (s * 32768.0).clamp(-32768.0, 32768.0)).collect();
            let mut wet = [0.0f32; 480];
            dns.process_frame(&mut wet, &inp);
            for (o, w) in ob.iter_mut().zip(wet.iter()) {
                *o = w / 32768.0;
            }
        }
        // Same pink bed through the node: must match.
        let mut nn = Rnnoise::new();
        let on = run(&mut nn, &pink, 960);
        assert_eq!(on.len(), out.len());
        for (a, b) in on.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-6, "node must match the crate sample-exact");
        }
    }

    #[test]
    fn rnnoise_rejects_bad_params() {
        let mut n = Rnnoise::new();
        assert!(matches!(
            n.set_param("mix", ParamValue::Text("x".into())),
            Err(ParamError::TypeMismatch { .. })
        ));
        assert!(matches!(
            n.set_param("nope", ParamValue::Float(1.0)),
            Err(ParamError::Unknown(_))
        ));
    }
}
