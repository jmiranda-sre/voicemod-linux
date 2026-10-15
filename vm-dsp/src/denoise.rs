//! Spectral denoiser: per-bin noise-floor estimation + gated Wiener-ish
//! gain (the RNNoise/DeepFilterNet *slot* in a mic chain — no neural
//! models, textbook stationary-noise suppression instead).
//!
//! How it works, per STFT frame (1024 @ hop 256, Hann/WOLA):
//! 1. A per-bin noise floor tracks the magnitude spectrum with two time
//!    constants: fast fall (noise removed → floor follows down) and a
//!    rise that is fast only while the overall frame is QUIET (below
//!    `-40 dBFS`, i.e. likely background noise) and slow while loud —
//!    so a new fan gets learned within a second, but sustained speech
//!    is never mistaken for noise.
//! 2. Gain per bin: `1 − strength·noise²/mag²` clamped to
//!    `10^(reduction_db/20)` — classic over-subtracted spectral gate.
//! 3. Temporal attack/release smoothing per bin + a 3-tap spectral
//!    smooth kill isolated-bin "musical noise".
//! 4. Phase is untouched (real-valued gain on the complex bin).
//!
//! Latency: one full analysis frame ≈ 1024/48000 ≈ 21 ms (the frame
//! must be buffered before its first hop can be synthesized).
//! Warmup: the first frame seeds the floor, so give it ~1 s of signal
//! before judging noise attenuation.
//!
//! RT contract: every buffer is preallocated; `process` only resizes on
//! channel-count growth (same policy as the graph scratch).

use super::dynamics::db_to_gain;
use super::node::{AudioNode, ParamError, ProcessContext};
use super::param::{ParamValue, PublicParam, RangedParam};
use super::spectral::hann;
use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::collections::VecDeque;
use std::sync::Arc;

const FRAME: usize = 1024;
const HOP: usize = 256;
/// Bins per channel (`FRAME/2 + 1`): the `profile` param length.
const BINS: usize = FRAME / 2 + 1;
const EPS: f32 = 1e-12;
/// Frames below this level are "quiet": upward floor adaptation runs at
/// full speed there (a new background noise gets learned fast). Loud
/// frames (speech, sustained tones) use the slow constant.
const QUIET_GATE_DB: f32 = -40.0;

/// Per-channel streaming state.
struct DenoiseCh {
    pending: VecDeque<f32>,
    /// Synthesized-but-not-yet-emitted samples (latency FIFO).
    out: VecDeque<f32>,
    ola: Vec<f32>,
    norm: Vec<f32>,
    /// Per-bin noise floor (magnitude).
    noise: Vec<f32>,
    /// Per-bin smoothed gain.
    gain: Vec<f32>,
    initialized: bool,
}

impl DenoiseCh {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            out: VecDeque::new(),
            ola: vec![0.0; FRAME],
            norm: vec![0.0; FRAME],
            noise: vec![0.0; FRAME / 2 + 1],
            gain: vec![1.0; FRAME / 2 + 1],
            initialized: false,
        }
    }
}

/// All time constants of one frame pass, precomputed per quantum.
#[derive(Clone, Copy)]
struct Coefs {
    /// Noise floor fall (seconds).
    a_down: f32,
    /// Noise floor rise while the frame is quiet (seconds).
    a_up_quiet: f32,
    /// Noise floor rise while the frame is loud (seconds).
    a_up_loud: f32,
    /// Gain smoothing attack / release (seconds).
    g_atk: f32,
    g_rel: f32,
}

fn frame_coef(tau_secs: f32, rate: u32) -> f32 {
    let hops = (tau_secs * rate as f32 / HOP as f32).max(1.0);
    1.0 - (-1.0 / hops).exp()
}

/// One analysis→gain→synthesis frame. The frame sits at the front of
/// `ch.pending`; on return exactly `HOP` samples were consumed and `HOP`
/// synthesized samples pushed to `ch.out`. Free function so the channel
/// state and the shared scratch buffers borrow disjointly.
#[allow(clippy::too_many_arguments)]
fn process_frame(
    window: &[f32],
    fft: &Arc<dyn Fft<f32>>,
    ifft: &Arc<dyn Fft<f32>>,
    spec: &mut [Complex<f32>],
    g_scratch: &mut [f32],
    ch: &mut DenoiseCh,
    coefs: Coefs,
    strength: f32,
    reduction_db: f32,
) {
    // Windowed analysis frame.
    for (i, s) in spec.iter_mut().enumerate() {
        let x = ch.pending[i];
        *s = Complex::new(x * window[i], 0.0);
    }
    fft.process(spec);

    let bins = FRAME / 2 + 1;
    let floor_g = db_to_gain(reduction_db);
    if !ch.initialized {
        // Seed the floor with the very first frame; gains pass through
        // untouched (the floor takes over from frame 2).
        for (i, s) in spec.iter().take(bins).enumerate() {
            ch.noise[i] = s.norm();
        }
        ch.gain.fill(1.0);
        ch.initialized = true;
    } else {
        // Frame level (time domain, pre-window) picks the rise speed:
        // quiet frames learn a new noise fast, loud frames don't.
        let mut acc = 0.0f32;
        for i in 0..FRAME {
            let x = ch.pending[i];
            acc += x * x;
        }
        let frame_db = 3.010_299_6 * (acc / FRAME as f32).max(1e-15).log2();
        let a_up = if frame_db < QUIET_GATE_DB {
            coefs.a_up_quiet
        } else {
            coefs.a_up_loud
        };
        for (i, s) in spec.iter().take(bins).enumerate() {
            let m = s.norm();
            let n = &mut ch.noise[i];
            let adapt = if m < *n { coefs.a_down } else { a_up };
            *n += adapt * (m - *n);
            // Over-subtracted power gain with a hard floor.
            let mut g = 1.0 - strength * (*n * *n) / (m * m + EPS);
            g = g.max(floor_g).min(1.0);
            // Temporal smoothing: fast to suppress, slow to reopen
            // (musical-noise control).
            let co = if g < ch.gain[i] { coefs.g_atk } else { coefs.g_rel };
            ch.gain[i] += co * (g - ch.gain[i]);
        }
    }
    // 3-tap spectral smooth of the gains (isolated-bin flicker).
    let gain = &ch.gain;
    for (i, gs) in g_scratch.iter_mut().enumerate().take(bins) {
        let l = gain[i.saturating_sub(1)];
        let r = gain[(i + 1).min(bins - 1)];
        *gs = (l + 2.0 * gain[i] + r) * 0.25;
    }
    // Apply (real gain keeps Hermitian symmetry → real output).
    for (i, s) in spec.iter_mut().enumerate() {
        let b = if i <= FRAME / 2 { i } else { FRAME - i };
        *s *= g_scratch[b];
    }
    ifft.process(spec);
    let scale = 1.0 / FRAME as f32;
    for i in 0..FRAME {
        let v = spec[i].re * scale;
        ch.ola[i] += v * window[i];
        ch.norm[i] += window[i] * window[i];
    }
    for i in 0..HOP {
        let o = if ch.norm[i] > 1e-9 { ch.ola[i] / ch.norm[i] } else { 0.0 };
        ch.out.push_back(o);
    }
    // Shift the tail down and zero the vacated tail (no reallocation —
    // the Vecs are permanently FRAME long).
    ch.ola.copy_within(HOP..FRAME, 0);
    ch.ola[FRAME - HOP..].fill(0.0);
    ch.norm.copy_within(HOP..FRAME, 0);
    ch.norm[FRAME - HOP..].fill(0.0);
    for _ in 0..HOP {
        ch.pending.pop_front();
    }
}

pub struct Denoise {
    strength: f32,
    reduction_db: f32,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    spec: Vec<Complex<f32>>,
    /// Spectral smoothing scratch (bins).
    g_scratch: Vec<f32>,
    ch: Vec<DenoiseCh>,
    /// Saved room floor waiting for channel creation: `process` builds
    /// channels lazily, but `apply_preset` sets `profile` before the
    /// first quantum — without the stash the boot install would be a
    /// silent no-op on an empty `ch`.
    pending_profile: Option<Vec<f32>>,
    rate: u32,
    coefs: Coefs,
}

impl Default for Denoise {
    fn default() -> Self {
        let mut planner = FftPlanner::new();
        Self {
            strength: 2.0,
            reduction_db: -30.0,
            window: hann(FRAME),
            fft: planner.plan_fft_forward(FRAME),
            ifft: planner.plan_fft_inverse(FRAME),
            spec: vec![Complex::default(); FRAME],
            g_scratch: vec![1.0; FRAME / 2 + 1],
            ch: Vec::new(),
            pending_profile: None,
            rate: 0,
            coefs: Coefs {
                a_down: 0.0,
                a_up_quiet: 0.0,
                a_up_loud: 0.0,
                g_atk: 0.0,
                g_rel: 0.0,
            },
        }
    }
}

impl std::fmt::Debug for Denoise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoise")
            .field("strength", &self.strength)
            .field("reduction_db", &self.reduction_db)
            .field("channels", &self.ch.len())
            .finish()
    }
}

impl Denoise {
    fn ensure_rate(&mut self, rate: u32) {
        if rate != self.rate {
            self.coefs = Coefs {
                a_down: frame_coef(0.25, rate),
                a_up_quiet: frame_coef(0.4, rate),
                a_up_loud: frame_coef(8.0, rate),
                g_atk: frame_coef(0.005, rate),
                g_rel: frame_coef(0.12, rate),
            };
            self.rate = rate;
        }
    }
}

impl AudioNode for Denoise {
    fn name(&self) -> &'static str {
        "denoise"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        self.ensure_rate(ctx.rate);
        while self.ch.len() < inputs.len() {
            let mut ch = DenoiseCh::new();
            // A profile set before the first quantum (preset boot)
            // lands here instead of vanishing into an empty vec.
            if let Some(floor) = &self.pending_profile {
                ch.noise.copy_from_slice(floor);
                ch.initialized = true;
            }
            self.ch.push(ch);
        }
        for ((input, output), ch) in inputs.iter().zip(outputs.iter_mut()).zip(self.ch.iter_mut()) {
            ch.pending.extend(input.iter());
            while ch.pending.len() >= FRAME {
                process_frame(
                    &self.window,
                    &self.fft,
                    &self.ifft,
                    &mut self.spec,
                    &mut self.g_scratch,
                    ch,
                    self.coefs,
                    self.strength,
                    self.reduction_db,
                );
            }
            // Emit exactly input.len(): the FIFO empties into the
            // ~FRAME-sample startup gap, then streams steadily.
            for o in output.iter_mut() {
                *o = ch.out.pop_front().unwrap_or(0.0);
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        let float = || value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() });
        match name {
            "strength" => {
                self.strength = clamp(float()?, RangedParam { min: 1.0, max: 4.0, default: 2.0 });
                Ok(())
            }
            "reduction_db" => {
                self.reduction_db =
                    clamp(float()?, RangedParam { min: -60.0, max: 0.0, default: -30.0 });
                Ok(())
            }
            // Saved room profile: installs the learned floor on every
            // channel and skips the first-frame seeding (instant warmup).
            // Boot never bricks on a hand-edited file: wrong length is
            // an error, but non-finite/negative bins sanitize to 0
            // (digital-silence floor, re-learns upward within a frame).
            "profile" => {
                let a = value
                    .as_float_array()
                    .ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                if a.len() != BINS {
                    return Err(ParamError::InvalidValue(format!(
                        "profile has {} bins, want {BINS}",
                        a.len()
                    )));
                }
                let floor: Vec<f32> =
                    a.iter().map(|v| if v.is_finite() && *v >= 0.0 { *v } else { 0.0 }).collect();
                for ch in &mut self.ch {
                    ch.noise.copy_from_slice(&floor);
                    ch.initialized = true;
                }
                self.pending_profile = Some(floor);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn get_param(&self, name: &str) -> Option<ParamValue> {
        match name {
            // Channel 0's floor (mono mics: identical everywhere), or
            // the stashed boot profile before channels exist.
            "profile" => Some(ParamValue::FloatArray(
                self.ch
                    .first()
                    .map(|c| c.noise.clone())
                    .or_else(|| self.pending_profile.clone())?,
            )),
            _ => None,
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "strength",
                value: ParamValue::Float(self.strength),
                range: Some(RangedParam { min: 1.0, max: 4.0, default: 2.0 }),
                public: true,
            },
            PublicParam {
                name: "reduction_db",
                value: ParamValue::Float(self.reduction_db),
                range: Some(RangedParam { min: -60.0, max: 0.0, default: -30.0 }),
                public: true,
            },
        ]
    }
}

fn clamp(v: f32, r: RangedParam) -> f32 {
    r.clamp(v)
}

#[cfg(test)]
mod denoise_tests {
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
        if x.is_empty() {
            return 0.0;
        }
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
                let u = (state >> 32) as u32 as f32 / u32::MAX as f32; // [0, 1]
                (u * 2.0 - 1.0) * amp
            })
            .collect()
    }

    #[test]
    fn denoise_attenuates_stationary_noise() {
        let mut n = Denoise::default();
        let input = noise(192_000, 0.1); // 4 s, seed = frame 1 = the noise itself
        let out = run(&mut n, &input, 960);
        let in_r = rms(&input[144_000..]);
        let out_r = rms(&out[144_000..]);
        let atten_db = 20.0 * (out_r / in_r).log10();
        assert!(
            out_r < in_r * 0.5,
            "noise attenuation only {atten_db:.1} dB (in {in_r:.4}, out {out_r:.4})"
        );
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn denoise_keeps_loud_signal_after_warmup() {
        let mut n = Denoise::default();
        let mut input = noise(48_000, 0.002); // 1 s near-silence: floor settles tiny
        let tail_noise = noise(96_000, 0.002);
        let tone: Vec<f32> = tail_noise
            .iter()
            .enumerate()
            .map(|(i, nz)| {
                0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin() + nz
            })
            .collect();
        input.extend_from_slice(&tone);
        let out = run(&mut n, &input, 960);
        let in_r = rms(&input[120_000..]); // last 0.5 s: tone segment
        let out_r = rms(&out[120_000..]);
        let ratio = out_r / in_r;
        assert!(ratio > 0.65, "sustained tone eroded: ratio {ratio:.3}");
        assert!(ratio < 1.35, "tone boosted unexpectedly: ratio {ratio:.3}");
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn denoise_learns_new_noise_while_quiet() {
        let mut n = Denoise::default();
        let mut input = noise(48_000, 0.0005); // 1 s near-silence
        input.extend_from_slice(&noise(144_000, 0.01)); // 3 s of NEW background noise
        let out = run(&mut n, &input, 960);
        // Last 0.5 s of the noise segment (floor must have learned it).
        let in_r = rms(&input[168_000..]);
        let out_r = rms(&out[168_000..]);
        assert!(
            out_r < in_r * 0.5,
            "new noise not learned: in {in_r:.5}, out {out_r:.5}"
        );
    }

    #[test]
    fn denoise_latency_shape_and_params() {
        let mut n = Denoise::default();
        let input: Vec<f32> = (0..24_000)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin())
            .collect();
        let out = run(&mut n, &input, 960);
        assert_eq!(out.len(), input.len(), "length must be preserved");
        // ~FRAME-sample startup silence (the analysis buffer filling).
        assert!(out[..512].iter().all(|s| s.abs() < 1e-6), "startup must be silent");
        assert!(out[1_536..].iter().any(|s| s.abs() > 0.1), "signal never came through");
        assert!(out.iter().all(|s| s.is_finite()));

        // Params: clamp, coerce, reject.
        n.set_param("strength", ParamValue::Float(99.0)).unwrap();
        let p = n.params().into_iter().find(|p| p.name == "strength").unwrap();
        assert_eq!(p.value, ParamValue::Float(4.0));
        assert!(matches!(
            n.set_param("strength", ParamValue::Text("x".into())),
            Err(ParamError::TypeMismatch { .. })
        ));
        assert!(matches!(
            n.set_param("nope", ParamValue::Float(1.0)),
            Err(ParamError::Unknown(_))
        ));
    }

    #[test]
    fn profile_installs_reads_back_and_validates() {
        let mut n = Denoise::default();
        // Unknown readback is None (trait default contract).
        assert!(n.get_param("nope").is_none());
        let floor = vec![0.01f32; BINS];
        n.set_param("profile", ParamValue::FloatArray(floor.clone())).unwrap();
        match n.get_param("profile") {
            Some(ParamValue::FloatArray(back)) => assert_eq!(back, floor),
            other => panic!("want FloatArray readback, got {other:?}"),
        }
        // Wrong length errors (never silently truncates/pads).
        assert!(matches!(
            n.set_param("profile", ParamValue::FloatArray(vec![0.0; 512])),
            Err(ParamError::InvalidValue(_))
        ));
        // Scalar is a type error, not a silent no-op.
        assert!(matches!(
            n.set_param("profile", ParamValue::Float(1.0)),
            Err(ParamError::TypeMismatch { .. })
        ));
        // Non-finite/negative bins sanitize to digital silence.
        let mut dirty = vec![0.02f32; BINS];
        dirty[0] = f32::NAN;
        dirty[1] = f32::INFINITY;
        dirty[2] = -1.0;
        n.set_param("profile", ParamValue::FloatArray(dirty)).unwrap();
        match n.get_param("profile") {
            Some(ParamValue::FloatArray(back)) => {
                assert_eq!(&back[..3], &[0.0, 0.0, 0.0]);
                assert!(back[3..].iter().all(|v| *v == 0.02));
            }
            other => panic!("want sanitized readback, got {other:?}"),
        }
    }

    #[test]
    fn profile_skips_first_frame_seeding() {
        // Same loud stationary noise into a fresh node (seeds the floor
        // from frame 1 → suppresses) and a node with a silence floor
        // installed (nothing learned, everything is signal → passes).
        let input = noise(24_000, 0.1);
        let mut fresh = Denoise::default();
        let out_fresh = run(&mut fresh, &input, 960);
        let mut primed = Denoise::default();
        primed
            .set_param("profile", ParamValue::FloatArray(vec![0.0; BINS]))
            .unwrap();
        let out_primed = run(&mut primed, &input, 960);
        // Early window: past the ~FRAME startup gap, before any slow
        // adaptation could matter.
        let win = 2_048..6_144;
        let r_fresh = rms(&out_fresh[win.clone()]);
        let r_primed = rms(&out_primed[win]);
        assert!(
            r_primed > r_fresh * 5.0,
            "primed node must pass from the start (primed {r_primed:.4}, fresh {r_fresh:.4})"
        );
    }

    #[test]
    fn no_profile_learns_floor_from_first_frame() {
        // Backcompat: without a profile the floor still learns
        // dynamically (this is what presets without `profile` do).
        let mut n = Denoise::default();
        let input = noise(96_000, 0.002); // 2 s near-silence
        let _ = run(&mut n, &input, 960);
        match n.get_param("profile") {
            Some(ParamValue::FloatArray(floor)) => {
                assert_eq!(floor.len(), BINS);
                assert!(floor.iter().all(|v| v.is_finite() && *v >= 0.0));
                let mean = floor.iter().sum::<f32>() / floor.len() as f32;
                assert!(
                    mean > 0.0 && mean < 0.05,
                    "learned floor must reflect the quiet room, got mean {mean:.5}"
                );
            }
            other => panic!("want learned floor readback, got {other:?}"),
        }
    }
}
