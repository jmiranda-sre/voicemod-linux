//! Spectral engine: phase-vocoder time-stretch + formant envelope.
//!
//! Textbook DSP throughout (Dolson/Laroche phase vocoder, cepstral
//! liftering for the spectral envelope). Pitch shift = stretch(β) +
//! linear resample(1/β), duration-preserving, exactly the classic recipe.
//!
//! Envelope correction (channel-vocoder idea): with input envelope E,
//! resample ratio r and warp k, synthesis magnitudes are scaled by
//! `G(u) = E(u·k)/E(u)` so formants either follow the pitch (`k = 1`)
//! or hold/warp independently (`k = r/f`).

use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::collections::VecDeque;
use std::sync::Arc;

pub const FRAME: usize = 2048;
pub const HOP_A: usize = 512;
const CEPS_LIFTER: usize = 40;
const EPS: f32 = 1e-9;

pub fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
        .collect()
}

fn wrap_pi(x: f32) -> f32 {
    let two_pi = 2.0 * std::f32::consts::PI;
    ((x + std::f32::consts::PI) % two_pi + two_pi) % two_pi - std::f32::consts::PI
}

pub struct PitchEngine {
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    spec: Vec<Complex<f32>>,
    cep: Vec<Complex<f32>>,
    /// Pitch ratio β (>1 = up). Resample ratio derives as 1/β.
    pub pitch: f32,
    /// Envelope warp selector k (see module docs).
    pub warp_k: f32,
    ch: Vec<PitchCh>,
}

struct PitchCh {
    pending: VecDeque<f32>,
    stretched: VecDeque<f32>,
    ola: Vec<f32>,
    norm: Vec<f32>,
    prev_phase: Vec<f32>,
    out_phase: Vec<f32>,
    initialized: bool,
}

impl PitchEngine {
    pub fn new() -> Self {
        let mut planner = FftPlanner::new();
        Self {
            window: hann(FRAME),
            fft: planner.plan_fft_forward(FRAME),
            ifft: planner.plan_fft_inverse(FRAME),
            spec: vec![Complex::default(); FRAME],
            cep: vec![Complex::default(); FRAME],
            pitch: 1.0,
            warp_k: 1.0,
            ch: Vec::new(),
        }
    }

    fn ensure_channels(&mut self, n: usize) {
        while self.ch.len() < n {
            let bins = FRAME / 2 + 1;
            let mut c = PitchCh {
                pending: VecDeque::new(),
                stretched: VecDeque::new(),
                ola: vec![0.0; FRAME],
                norm: vec![0.0; FRAME],
                prev_phase: vec![0.0; bins],
                out_phase: vec![0.0; bins],
                initialized: false,
            };
            c.pending.reserve(FRAME * 2);
            c.stretched.reserve(FRAME * 4);
            self.ch.push(c);
        }
    }

    /// Push `frames` input samples per channel, pull the same count out.
    /// Exact-hop analysis; startup pads the first frames with zeros.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        self.ensure_channels(inputs.len());
        // Stretch longer by β, then resample back by 1/β: net duration ×1,
        // pitch ×β. (hop_s = Ha·β is the correct direction.)
        let beta = self.pitch.max(0.5).min(2.0);
        let hop_s = ((HOP_A as f32) * beta).round().max(64.0) as usize;
        // Consume the stretched stream β× faster than it is produced:
        // duration back to ×1, pitch ×β.
        let r = beta;
        let k = self.warp_k;
        for ((input, output), ch) in inputs.iter().zip(outputs.iter_mut()).zip(self.ch.iter_mut()) {
            ch.pending.extend(input.iter());
            while ch.pending.len() >= FRAME {
                let frame: Vec<f32> = ch.pending.iter().take(FRAME).copied().collect();
                analyze_into(
                    &self.window,
                    &self.fft,
                    &self.ifft,
                    &mut self.spec,
                    &mut self.cep,
                    &frame,
                    ch,
                    hop_s,
                    k,
                );
                for _ in 0..HOP_A {
                    ch.pending.pop_front();
                }
            }
            // Resample the stretched stream back to the input sample count.
            let mut pos = 0.0f32;
            // Keep fractional position across calls via the stretched FIFO:
            // emit exactly `input.len()` samples.
            let mut emitted = 0usize;
            while emitted < input.len() {
                let need = (pos + 1.0).ceil() as usize + 1;
                if ch.stretched.len() < need {
                    output[emitted] = 0.0;
                } else {
                    let i = pos as usize;
                    let frac = pos - i as f32;
                    let a = ch.stretched[i];
                    let b = ch.stretched[i + 1];
                    output[emitted] = a + (b - a) * frac;
                }
                pos += r;
                emitted += 1;
            }
            let drain = (pos as usize).min(ch.stretched.len());
            for _ in 0..drain {
                ch.stretched.pop_front();
            }
        }
    }
} // end impl PitchEngine

/// One analysis/synthesis step: exact-hop STFT in, `hop_s` stretched
/// samples out. Free function so channel state and scratch buffers can
/// borrow disjointly.
#[allow(clippy::too_many_arguments)]
fn analyze_into(
    window: &[f32],
    fft: &Arc<dyn Fft<f32>>,
    ifft: &Arc<dyn Fft<f32>>,
    spec: &mut [Complex<f32>],
    cep: &mut [Complex<f32>],
    frame: &[f32],
    ch: &mut PitchCh,
    hop_s: usize,
    k: f32,
) {
    let n = FRAME;
        let bins = n / 2 + 1;
        for (s, x) in spec.iter_mut().enumerate() {
            *x = Complex::new(frame[s] * window[s], 0.0);
        }
        fft.process(spec);
        let mut mag = vec![0.0f32; bins];
        let mut phase = vec![0.0f32; bins];
        for i in 0..bins {
            mag[i] = spec[i].norm();
            phase[i] = spec[i].arg();
        }
        let env = envelope(fft, ifft, cep, &mag);
        if !ch.initialized {
            ch.prev_phase.copy_from_slice(&phase);
            ch.out_phase.copy_from_slice(&phase);
            ch.initialized = true;
        }
        let omega: Vec<f32> = (0..bins)
            .map(|i| 2.0 * std::f32::consts::PI * i as f32 / n as f32)
            .collect();
        // Synthesis spectrum (same-bin oscillator bank) with envelope fix.
        let mut synth = vec![Complex::default(); n];
        for i in 0..bins {
            let delta = wrap_pi(phase[i] - ch.prev_phase[i] - omega[i] * HOP_A as f32);
            let true_freq = omega[i] + delta / HOP_A as f32;
            ch.out_phase[i] += true_freq * hop_s as f32;
            let src = ((i as f32) * k).round() as usize;
            let src = src.min(bins - 1);
            let g = env[src] / env[i].max(EPS);
            let m = mag[i] * g.min(4.0);
            let (s, c) = ch.out_phase[i].sin_cos();
            synth[i] = Complex::new(m * c, m * s);
            if i > 0 && i < n / 2 {
                synth[n - i] = synth[i].conj();
            }
        }
        ch.prev_phase.copy_from_slice(&phase);
        ifft.process(&mut synth);
        let scale = 1.0 / n as f32;
        // WOLA: accumulate windowed output + window energy, emit hop_s.
        for i in 0..n {
            let v = synth[i].re * scale;
            ch.ola[i] += v * window[i];
            ch.norm[i] += window[i] * window[i];
        }
        for i in 0..hop_s.min(n) {
            let o = if ch.norm[i] > EPS { ch.ola[i] / ch.norm[i] } else { 0.0 };
            ch.stretched.push_back(o);
        }
        // Shift OLA/norm buffers.
        ch.ola.drain(..hop_s.min(n));
        ch.ola.extend(std::iter::repeat(0.0).take(hop_s.min(n)));
        ch.norm.drain(..hop_s.min(n));
        ch.norm.extend(std::iter::repeat(0.0).take(hop_s.min(n)));
    }

/// Spectral envelope via real cepstrum with low-quefrency liftering.
fn envelope(
    fft: &Arc<dyn Fft<f32>>,
    ifft: &Arc<dyn Fft<f32>>,
    cep: &mut [Complex<f32>],
    mag: &[f32],
) -> Vec<f32> {
    let n = FRAME;
    let bins = n / 2 + 1;
    for i in 0..n {
        let m = if i < bins { mag[i] } else { mag[n - i] };
        cep[i] = Complex::new((m + EPS).ln(), 0.0);
    }
    ifft.process(cep);
    for (i, c) in cep.iter_mut().enumerate() {
        let keep = i < CEPS_LIFTER || i + CEPS_LIFTER >= n;
        let v = c.re / n as f32;
        *c = Complex::new(if keep { v } else { 0.0 }, 0.0);
    }
    fft.process(cep);
    (0..bins).map(|i| cep[i].re.exp()).collect()
}

impl Default for PitchEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PitchEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PitchEngine")
            .field("pitch", &self.pitch)
            .field("warp_k", &self.warp_k)
            .field("channels", &self.ch.len())
            .finish()
    }
}
