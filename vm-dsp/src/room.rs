//! Room processors (F-0.4.A): convolution reverb.
//!
//! `convolver`: uniform partitioned convolution (1024-sample partitions)
//! against a WAV impulse response loaded from disk. Real rooms and
//! cabinets instead of algorithmic approximations. The partition FFTs
//! of the IR are precomputed on the control plane (`ir_path` set); the
//! RT path only transforms input blocks, multiplies and accumulates.
//! Added latency is exactly one partition (1024 samples ≈ 21 ms).

use super::node::{AudioNode, ParamError, ProcessContext};
use super::param::{ParamValue, PublicParam, RangedParam};
use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::collections::VecDeque;
use std::sync::Arc;

/// Partition size (and added latency) in samples.
const PART: usize = 1024;
/// FFT size for linear convolution of two partitions.
const FFT_N: usize = PART * 2;
/// IR length cap (4 s @ 48 kHz — longer tails truncate).
const MAX_IR: usize = 192_000;

/// Minimal WAV reader (PCM 16/24/32-int + 32-float, any rate/channels).
/// Returns (mono-or-stereo planar f32, rate). MP3/OGG/FLAC are rejected
/// with a clear error (convert to WAV first).
fn read_wav(path: &str) -> Result<(Vec<Vec<f32>>, u32), String> {
    let b = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let err = |m: &str| format!("{path}: {m} (want RIFF/WAVE PCM16-32 or FLOAT32)");
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(err("not a RIFF/WAVE file"));
    }
    let u16le = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
    let u32le = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    // Walk chunks (fmt/data can sit anywhere).
    let mut fmt: Option<(usize, usize, usize, usize)> = None; // audio, ch, rate, bits
    let mut data: Option<(usize, usize)> = None;
    let mut o = 12;
    while o + 8 <= b.len() {
        let id = &b[o..o + 4];
        let size = u32le(o + 4) as usize;
        if id == b"fmt " {
            if size < 16 || o + 24 > b.len() {
                return Err(err("bad fmt chunk"));
            }
            fmt = Some((u16le(o + 8), u16le(o + 10), u32le(o + 12) as usize, u16le(o + 22)));
        } else if id == b"data" {
            data = Some((o + 8, size));
        }
        o += 8 + size + (size & 1);
    }
    let (audio, ch, rate, bits) = fmt.ok_or_else(|| err("no fmt chunk"))?;
    let (doff, dlen) = data.ok_or_else(|| err("no data chunk"))?;
    if audio != 1 && audio != 3 {
        return Err(err("only PCM integer and IEEE-float WAV"));
    }
    if ch == 0 || ch > 32 || !(bits == 16 || bits == 24 || bits == 32) {
        return Err(err("channels 1..=32, bits 16/24/32"));
    }
    let bytes = bits / 8;
    let frames = dlen / bytes / ch;
    if frames == 0 || doff + frames * bytes * ch > b.len() {
        return Err(err("truncated data"));
    }
    let mut planar = vec![Vec::with_capacity(frames); ch];
    for f in 0..frames {
        for (c, lane) in planar.iter_mut().enumerate() {
            let p = doff + (f * ch + c) * bytes;
            let s = match (audio, bits) {
                (1, 16) => i16::from_le_bytes([b[p], b[p + 1]]) as f32 / 32768.0,
                (1, 24) => {
                    let v = (b[p] as i32) | ((b[p + 1] as i32) << 8) | ((b[p + 2] as i32) << 16);
                    let v = if v & 0x800000 != 0 { v - 0x1000000 } else { v };
                    v as f32 / 8388608.0
                }
                (_, 32) => {
                    if audio == 3 {
                        f32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]])
                    } else {
                        i32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]) as f32 / 2147483648.0
                    }
                }
                _ => return Err(err("unsupported sample format")),
            };
            lane.push(if s.is_finite() { s } else { 0.0 });
        }
    }
    Ok((planar, rate as u32))
}

/// Linear resample (control plane only; IR tails forgive the method).
fn resample_linear(x: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || x.is_empty() {
        return x.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let n = ((x.len() as f64 / ratio).ceil() as usize).max(1);
    (0..n)
        .map(|i| {
            let pos = (i as f64 * ratio).min((x.len() - 1) as f64);
            let lo = pos as usize;
            let hi = (lo + 1).min(x.len() - 1);
            x[lo] + (x[hi] - x[lo]) * (pos - lo as f64) as f32
        })
        .collect()
}

struct ConvCh {
    pending: VecDeque<f32>,
    /// Input-block spectra ring ( newest at back ).
    ring: VecDeque<Vec<Complex<f32>>>,
    /// Overlap-add tail + ready FIFO.
    ola: Vec<f32>,
    out: VecDeque<f32>,
    /// Partitioned IR spectra (shared clone per channel).
    ir: Vec<Vec<Complex<f32>>>,
}

impl ConvCh {
    fn new(ir: Vec<Vec<Complex<f32>>>) -> Self {
        let nparts = ir.len().max(1);
        // Wet path carries exactly one partition of delay (pre-filled
        // zeros): deterministic group delay, quantum-independent. Dry
        // passes undelayed, so mix=0 stays bit-exact.
        let mut out = VecDeque::with_capacity(PART * 2);
        out.extend(std::iter::repeat_n(0.0, PART));
        Self {
            pending: VecDeque::new(),
            ring: VecDeque::from(vec![vec![Complex::new(0.0, 0.0); FFT_N]; nparts]),
            ola: vec![0.0; PART],
            out,
            ir,
        }
    }
}

pub struct Convolver {
    mix: f32,
    ir_path: String,
    /// Partitioned spectra per channel (empty = no IR loaded).
    ir_parts: Vec<Vec<Vec<Complex<f32>>>>,
    ir_rate: u32,
    ch: Vec<ConvCh>,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    spec: Vec<Complex<f32>>,
    acc: Vec<Complex<f32>>,
}

impl Default for Convolver {
    fn default() -> Self {
        let mut planner = FftPlanner::new();
        Self {
            mix: 1.0,
            ir_path: String::new(),
            ir_parts: Vec::new(),
            ir_rate: 48_000,
            ch: Vec::new(),
            fft: planner.plan_fft_forward(FFT_N),
            ifft: planner.plan_fft_inverse(FFT_N),
            spec: vec![Complex::default(); FFT_N],
            acc: vec![Complex::default(); FFT_N],
        }
    }
}

impl Convolver {
    /// Partition one channel's IR into spectra (control plane).
    fn partition_ir(&self, ir: &[f32]) -> Vec<Vec<Complex<f32>>> {
        let mut parts = Vec::new();
        let mut buf = vec![Complex::new(0.0, 0.0); FFT_N];
        for block in ir.chunks(PART) {
            for s in buf.iter_mut() {
                *s = Complex::new(0.0, 0.0);
            }
            for (i, s) in block.iter().enumerate() {
                buf[i].re = *s;
            }
            self.fft.process(&mut buf);
            parts.push(buf.clone());
        }
        if parts.is_empty() {
            parts.push(vec![Complex::new(0.0, 0.0); FFT_N]);
        }
        parts
    }

    fn load_ir(&mut self, path: &str) -> Result<(), String> {
        let (planar, rate) = read_wav(path)?;
        // Resample voices to the pipeline rate, normalize peak, truncate.
        let mono: Vec<Vec<f32>> = planar
            .into_iter()
            .map(|ch| {
                let mut r = resample_linear(&ch, rate, 48_000);
                r.truncate(MAX_IR);
                let peak = r.iter().map(|s| s.abs()).fold(0.0f32, f32::max).max(1e-9);
                for s in r.iter_mut() {
                    *s /= peak;
                }
                r
            })
            .collect();
        if mono.iter().all(|ch| ch.iter().all(|s| *s == 0.0)) {
            return Err(format!("{path}: impulse is digital silence"));
        }
        // Spectra per channel (mono IR is shared at lane mapping time).
        self.ir_parts = mono.iter().map(|ch| self.partition_ir(ch)).collect();
        self.ir_rate = 48_000;
        self.ir_path = path.to_string();
        // New IR drops live state (fresh tails, no crossfade artifacts).
        self.ch.clear();
        Ok(())
    }

    fn ir_for_lane(&self, lane: usize, lanes: usize) -> Vec<Vec<Complex<f32>>> {
        if self.ir_parts.is_empty() {
            return vec![vec![Complex::new(0.0, 0.0); FFT_N]];
        }
        if self.ir_parts.len() == 1 || lanes == 1 {
            return self.ir_parts[0].clone();
        }
        self.ir_parts[lane % self.ir_parts.len()].clone()
    }
}

impl AudioNode for Convolver {
    fn name(&self) -> &'static str {
        "convolver"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let _ = ctx.rate;
        while self.ch.len() < inputs.len() {
            let lane = self.ch.len();
            self.ch.push(ConvCh::new(self.ir_for_lane(lane, inputs.len())));
        }
        for ((input, output), ch) in inputs.iter().zip(outputs.iter_mut()).zip(self.ch.iter_mut()) {
            ch.pending.extend(input.iter());
            while ch.pending.len() >= PART {
                // FFT the input block.
                for s in self.spec.iter_mut() {
                    *s = Complex::new(0.0, 0.0);
                }
                for i in 0..PART {
                    self.spec[i].re = ch.pending.pop_front().unwrap_or(0.0);
                }
                self.fft.process(&mut self.spec);
                ch.ring.push_back(self.spec.clone());
                while ch.ring.len() > ch.ir.len() {
                    ch.ring.pop_front();
                }
                // Accumulate IR[k] * X[n-k].
                for s in self.acc.iter_mut() {
                    *s = Complex::new(0.0, 0.0);
                }
                for (h, x) in ch.ir.iter().zip(ch.ring.iter().rev()) {
                    for (a, (hh, xx)) in self.acc.iter_mut().zip(h.iter().zip(x.iter())) {
                        *a += *hh * *xx;
                    }
                }
                self.ifft.process(&mut self.acc);
                let scale = 1.0 / FFT_N as f32;
                // Overlap-add the first partition; tail into ola.
                for i in 0..PART {
                    let v = self.acc[i].re * scale + ch.ola[i];
                    ch.out.push_back(v);
                }
                for i in 0..PART {
                    ch.ola[i] = self.acc[PART + i].re * scale;
                }
            }
            // Emit; dry passes undelayed, wet carries one partition.
            for (i, o) in input.iter().zip(output.iter_mut()) {
                let w = ch.out.pop_front().unwrap_or(0.0);
                *o = i * (1.0 - self.mix) + w * self.mix;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "ir_path" => {
                let p = match value {
                    ParamValue::Text(s) => s,
                    _ => return Err(ParamError::TypeMismatch { name: name.into() }),
                };
                self.load_ir(&p).map_err(ParamError::InvalidValue)?;
                Ok(())
            }
            "mix" => {
                let v = value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                self.mix = RangedParam { min: 0.0, max: 1.0, default: 1.0 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    /// One partition of group delay (uniform partitioned scheme; the
    /// wet FIFO is pre-filled so the delay is exact and
    /// quantum-independent, dry passes undelayed).
    fn latency_samples(&self, _rate: u32) -> usize {
        PART
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "ir_path",
                value: ParamValue::Text(self.ir_path.clone()),
                range: None,
                public: true,
            },
            PublicParam {
                name: "mix",
                value: ParamValue::Float(self.mix),
                range: Some(RangedParam { min: 0.0, max: 1.0, default: 1.0 }),
                public: true,
            },
        ]
    }
}

#[cfg(test)]
mod room_tests {
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

    /// Write a minimal PCM16 mono WAV for loader tests.
    fn write_wav(path: &std::path::Path, rate: u32, samples: &[f32]) {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + samples.len() as u32 * 2).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(samples.len() as u32 * 2).to_le_bytes());
        for s in samples {
            v.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        std::fs::write(path, v).unwrap();
    }

    fn tmp_ir(name: &str, rate: u32, samples: &[f32]) -> String {
        let p = std::env::temp_dir().join(format!("vm-ir-{name}-{}.wav", std::process::id()));
        write_wav(&p, rate, samples);
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn dirac_ir_is_delayed_identity() {
        let mut c = Convolver::default();
        assert_eq!(c.latency_samples(RATE), PART);
        let mut ir = vec![0.0f32; 4096];
        ir[0] = 1.0;
        let path = tmp_ir("dirac", 48_000, &ir);
        c.set_param("ir_path", ParamValue::Text(path.clone())).unwrap();
        let _ = std::fs::remove_file(&path);
        // Impulse in: the same impulse out, exactly one partition later
        // (FFT roundtrip within float tolerance).
        let mut input = vec![0.0f32; 8192];
        input[0] = 1.0;
        let out = run(&mut c, &input, 960);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(out[..PART].iter().all(|s| s.abs() < 1e-5), "one partition of latency");
        assert!((out[PART] - 1.0).abs() < 1e-3, "dirac preserved, got {}", out[PART]);
        assert!(out[PART + 1..].iter().all(|s| s.abs() < 1e-3), "no tail for a dirac");
    }

    #[test]
    fn decaying_ir_rings_and_mix_blends() {
        let mut c = Convolver::default();
        // Slow decay: clearly nonzero several partitions out.
        let ir: Vec<f32> = (0..8192).map(|i| 0.95f32.powi(i / 64)).collect();
        let path = tmp_ir("decay", 48_000, &ir);
        c.set_param("ir_path", ParamValue::Text(path.clone())).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut input = vec![0.0f32; 12_288];
        input[0] = 0.5;
        let out = run(&mut c, &input, 960);
        // Tail energy well past the direct sound: real reverberation.
        let tail: f32 = out[4096..].iter().map(|s| s * s).sum::<f32>();
        assert!(tail > 1e-6, "IR tail must ring");
        // Mix = 0: dry passthrough, no added latency.
        c.set_param("mix", ParamValue::Float(0.0)).unwrap();
        let out2 = run(&mut c, &input, 960);
        for (a, b) in out2.iter().zip(input.iter()) {
            assert!((a - b).abs() < 1e-6, "mix=0 is exact passthrough");
        }
    }

    #[test]
    fn loader_rejects_garbage_and_resamples() {
        let mut c = Convolver::default();
        assert!(c.set_param("ir_path", ParamValue::Text("/nonexistent/x.wav".into())).is_err());
        let bad = std::env::temp_dir().join(format!("vm-ir-bad-{}.wav", std::process::id()));
        std::fs::write(&bad, b"definitely not a wav file at all............").unwrap();
        assert!(c.set_param("ir_path", ParamValue::Text(bad.to_string_lossy().into_owned())).is_err());
        let _ = std::fs::remove_file(&bad);
        assert!(matches!(
            c.set_param("ir_path", ParamValue::Float(1.0)),
            Err(ParamError::TypeMismatch { .. })
        ));
        // 44.1 kHz IR resamples onto the pipeline rate (length scales).
        let ir44: Vec<f32> = (0..4410).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
        let path = tmp_ir("44k", 44_100, &ir44);
        c.set_param("ir_path", ParamValue::Text(path.clone())).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut input = vec![0.0f32; 8192];
        input[0] = 1.0;
        let out = run(&mut c, &input, 960);
        assert!((out[PART] - 1.0).abs() < 0.05, "resampled dirac preserved");
    }

    #[test]
    fn convolver_params_validate() {
        let mut c = Convolver::default();
        assert!(matches!(
            c.set_param("nope", ParamValue::Float(1.0)),
            Err(ParamError::Unknown(_))
        ));
    }
}
