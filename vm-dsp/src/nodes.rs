//! MVP node set. Original implementations throughout.
//! Names match the factory registry (`mvp_factory`).

use super::node::{AudioNode, ParamError, ProcessContext};
use super::param::{ParamValue, PublicParam, RangedParam};

/// Pass-through / placeholder (mirrors the Null role).
#[derive(Debug, Default)]
pub struct Null;

impl AudioNode for Null {
    fn name(&self) -> &'static str {
        "null"
    }
    fn process(&mut self, _ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
            o.copy_from_slice(i);
        }
    }
    fn set_param(&mut self, name: &str, _v: ParamValue) -> Result<(), ParamError> {
        Err(ParamError::Unknown(name.into()))
    }
    fn params(&self) -> Vec<PublicParam> {
        Vec::new()
    }
}

/// Scalar gain (the Volume role).
#[derive(Debug)]
pub struct Gain {
    pub gain: f32,
    /// Free-form annotation (preset documentation channel).
    pub note: String,
}

impl Default for Gain {
    fn default() -> Self {
        Self { gain: 1.0, note: String::new() }
    }
}

impl AudioNode for Gain {
    fn name(&self) -> &'static str {
        "gain"
    }
    fn process(&mut self, _ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
            for (s, d) in i.iter().zip(o.iter_mut()) {
                *d = *s * self.gain;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match (name, value) {
            ("gain", v) => {
                let v = value_as_f32(name, v)?;
                self.gain = RangedParam { min: 0.0, max: 4.0, default: 1.0 }.clamp(v);
                Ok(())
            }
            ("note", ParamValue::Text(s)) => {
                self.note = s;
                Ok(())
            }
            ("note", _) => Err(ParamError::TypeMismatch { name: name.into() }),
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "note",
                value: ParamValue::Text(self.note.clone()),
                range: None,
                public: true,
            },
            PublicParam {
            name: "gain",
            value: ParamValue::Float(self.gain),
            range: Some(RangedParam { min: 0.0, max: 4.0, default: 1.0 }),
            public: true,
        }]
    }
}

fn value_as_f32(name: &str, v: ParamValue) -> Result<f32, ParamError> {
    v.as_float()
        .ok_or_else(|| ParamError::TypeMismatch { name: name.into() })
}

/// Hard mute.
#[derive(Debug, Default)]
pub struct Mute {
    pub muted: bool,
}

impl AudioNode for Mute {
    fn name(&self) -> &'static str {
        "mute"
    }
    fn process(&mut self, _ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
            if self.muted {
                o.fill(0.0);
            } else {
                o.copy_from_slice(i);
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match (name, value) {
            ("muted", ParamValue::Bool(b)) => {
                self.muted = b;
                Ok(())
            }
            ("muted", _) => Err(ParamError::TypeMismatch { name: name.into() }),
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![PublicParam {
            name: "muted",
            value: ParamValue::Bool(self.muted),
            range: None,
            public: true,
        }]
    }
}

/// N→1 mixer with per-input gains (the Mix role).
/// Inputs: all lanes concatenated; output: single lane per call channel
/// group. MVP: sums all input lanes into each output lane with one gain.
#[derive(Debug)]
pub struct Mix {
    pub gain: f32,
}

impl Default for Mix {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl AudioNode for Mix {
    fn name(&self) -> &'static str {
        "mix"
    }
    fn process(&mut self, _ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        for o in outputs.iter_mut() {
            o.fill(0.0);
            for i in inputs.iter() {
                for (s, d) in i.iter().zip(o.iter_mut()) {
                    *d += *s * self.gain;
                }
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match (name, value) {
            ("gain", v) => {
                let v = value_as_f32(name, v)?;
                self.gain = RangedParam { min: 0.0, max: 4.0, default: 1.0 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
            name: "gain",
            value: ParamValue::Float(self.gain),
            range: Some(RangedParam { min: 0.0, max: 4.0, default: 1.0 }),
            public: true,
        }]
    }
}

/// Pitch shifter (phase-vocoder + resample, formants follow pitch —
/// the classic "transpose" character). `transpose_semitones` in [-12, 12].
#[derive(Debug)]
pub struct PitchFFT {
    engine: super::spectral::PitchEngine,
    transpose: f32,
    mix: f32,
}

impl Default for PitchFFT {
    fn default() -> Self {
        let mut engine = super::spectral::PitchEngine::new();
        engine.pitch = 1.0;
        engine.warp_k = 1.0;
        Self { engine, transpose: 0.0, mix: 1.0 }
    }
}

impl PitchFFT {
    fn retune(&mut self) {
        let p = 2.0f32.powf(self.transpose / 12.0);
        self.engine.pitch = p;
        // Formants follow the pitch automatically through the resample
        // stage, so no pre-correction: warp k = 1 (G = E(u)/E(u) = 1).
        self.engine.warp_k = 1.0;
    }
}

impl AudioNode for PitchFFT {
    fn name(&self) -> &'static str {
        "pitch-fft"
    }
    fn process(&mut self, _ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        // Wet path through the engine, then dry/wet mix.
        let mut wet: Vec<Vec<f32>> = inputs.iter().map(|l| vec![0.0; l.len()]).collect();
        {
            let ins: Vec<&[f32]> = inputs.iter().map(|l| l.as_ref()).collect();
            let mut outs: Vec<&mut [f32]> = wet.iter_mut().map(|l| l.as_mut_slice()).collect();
            self.engine.process(&ins, &mut outs);
        }
        for ((i, w), o) in inputs.iter().zip(wet.iter()).zip(outputs.iter_mut()) {
            for ((s, wv), d) in i.iter().zip(w.iter()).zip(o.iter_mut()) {
                *d = *s * (1.0 - self.mix) + *wv * self.mix;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "transpose_semitones" => {
                let v = value_as_f32(name, value)?;
                self.transpose = RangedParam { min: -12.0, max: 12.0, default: 0.0 }.clamp(v);
                self.retune();
                Ok(())
            }
            "mix" => {
                let v = value_as_f32(name, value)?;
                self.mix = RangedParam { min: 0.0, max: 1.0, default: 1.0 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "transpose_semitones",
                value: ParamValue::Float(self.transpose),
                range: Some(RangedParam { min: -12.0, max: 12.0, default: 0.0 }),
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

/// Formant-aware pitch shifter: independent transpose + formant warp.
/// `formant_shift_semitones = 0` preserves the original formants.
#[derive(Debug)]
pub struct FormantPitchShifter {
    engine: super::spectral::PitchEngine,
    transpose: f32,
    formant: f32,
    mix: f32,
}

impl Default for FormantPitchShifter {
    fn default() -> Self {
        let mut s = Self {
            engine: super::spectral::PitchEngine::new(),
            transpose: 0.0,
            formant: 0.0,
            mix: 1.0,
        };
        s.retune();
        s
    }
}

impl FormantPitchShifter {
    fn retune(&mut self) {
        let p = 2.0f32.powf(self.transpose / 12.0);
        let f = 2.0f32.powf(self.formant / 12.0);
        self.engine.pitch = p;
        // G(u) = E(u·k)/E(u) with k = r/f (r = β): target envelope E(w/f).
        self.engine.warp_k = p / f;
    }
}

impl AudioNode for FormantPitchShifter {
    fn name(&self) -> &'static str {
        "formant-pitch-shifter"
    }
    fn process(&mut self, _ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let mut wet: Vec<Vec<f32>> = inputs.iter().map(|l| vec![0.0; l.len()]).collect();
        {
            let ins: Vec<&[f32]> = inputs.iter().map(|l| l.as_ref()).collect();
            let mut outs: Vec<&mut [f32]> = wet.iter_mut().map(|l| l.as_mut_slice()).collect();
            self.engine.process(&ins, &mut outs);
        }
        for ((i, w), o) in inputs.iter().zip(wet.iter()).zip(outputs.iter_mut()) {
            for ((s, wv), d) in i.iter().zip(w.iter()).zip(o.iter_mut()) {
                *d = *s * (1.0 - self.mix) + *wv * self.mix;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "transpose_semitones" => {
                let v = value_as_f32(name, value)?;
                self.transpose = RangedParam { min: -12.0, max: 12.0, default: 0.0 }.clamp(v);
                self.retune();
                Ok(())
            }
            "formant_shift_semitones" => {
                let v = value_as_f32(name, value)?;
                self.formant = RangedParam { min: -12.0, max: 12.0, default: 0.0 }.clamp(v);
                self.retune();
                Ok(())
            }
            "mix" => {
                let v = value_as_f32(name, value)?;
                self.mix = RangedParam { min: 0.0, max: 1.0, default: 1.0 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "transpose_semitones",
                value: ParamValue::Float(self.transpose),
                range: Some(RangedParam { min: -12.0, max: 12.0, default: 0.0 }),
                public: true,
            },
            PublicParam {
                name: "formant_shift_semitones",
                value: ParamValue::Float(self.formant),
                range: Some(RangedParam { min: -12.0, max: 12.0, default: 0.0 }),
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

// ---- Schroeder/Freeverb-structure reverb + tanh tube ----

const COMB_TUNINGS: [usize; 4] = [1557, 1617, 1491, 1422];
const ALLPASS_TUNINGS: [usize; 2] = [556, 441];
const STEREO_SPREAD: usize = 23;

#[derive(Debug)]
struct Comb {
    buf: Vec<f32>,
    idx: usize,
    feedback: f32,
    damp: f32,
    store: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)], idx: 0, feedback: 0.5, damp: 0.5, store: 0.0 }
    }
    fn process(&mut self, input: f32) -> f32 {
        let out = self.buf[self.idx];
        self.store = out * (1.0 - self.damp) + self.store * self.damp;
        self.buf[self.idx] = input + self.store * self.feedback;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
}

#[derive(Debug)]
struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)], idx: 0 }
    }
    fn process(&mut self, input: f32) -> f32 {
        let bufout = self.buf[self.idx];
        let out = -input + bufout;
        const FB: f32 = 0.5;
        self.buf[self.idx] = input + bufout * FB;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
}

/// Algorithmic reverb (Schroeder comb+allpass network, Freeverb structure,
/// tunings scaled 44.1k→48k). Original code, textbook structure.
#[derive(Debug)]
pub struct Reverb {
    combs_l: Vec<Comb>,
    combs_r: Vec<Comb>,
    all_l: Vec<Allpass>,
    all_r: Vec<Allpass>,
    room: f32,
    damping: f32,
    wet: f32,
}

impl Default for Reverb {
    fn default() -> Self {
        let scale = |t: usize| ((t as f32) * 48_000.0 / 44_100.0) as usize;
        let mut r = Self {
            combs_l: COMB_TUNINGS.iter().map(|t| Comb::new(scale(*t))).collect(),
            combs_r: COMB_TUNINGS.iter().map(|t| Comb::new(scale(*t) + STEREO_SPREAD)).collect(),
            all_l: ALLPASS_TUNINGS.iter().map(|t| Allpass::new(scale(*t))).collect(),
            all_r: ALLPASS_TUNINGS
                .iter()
                .map(|t| Allpass::new(scale(*t) + STEREO_SPREAD))
                .collect(),
            room: 0.5,
            damping: 0.5,
            wet: 0.33,
        };
        r.retune();
        r
    }
}

impl Reverb {
    fn retune(&mut self) {
        for c in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) {
            c.feedback = 0.28 + self.room * 0.7;
            c.damp = self.damping;
        }
    }
}

impl AudioNode for Reverb {
    fn name(&self) -> &'static str {
        "reverb"
    }
    fn process(&mut self, _ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let mono_in: Vec<f32> = if inputs.len() == 1 {
            inputs[0].to_vec()
        } else {
            inputs[0]
                .iter()
                .zip(inputs[1].iter())
                .map(|(a, b)| (a + b) * 0.5)
                .collect()
        };
        let mut rev_l = vec![0.0f32; mono_in.len()];
        let mut rev_r = vec![0.0f32; mono_in.len()];
        for (i, s) in mono_in.iter().enumerate() {
            let mut acc_l = 0.0;
            let mut acc_r = 0.0;
            for (cl, cr) in self.combs_l.iter_mut().zip(self.combs_r.iter_mut()) {
                acc_l += cl.process(*s);
                acc_r += cr.process(*s);
            }
            for (al, ar) in self.all_l.iter_mut().zip(self.all_r.iter_mut()) {
                acc_l = al.process(acc_l);
                acc_r = ar.process(acc_r);
            }
            rev_l[i] = acc_l;
            rev_r[i] = acc_r;
        }
        for (ch, o) in outputs.iter_mut().enumerate() {
            let rev = if ch % 2 == 0 { &rev_l } else { &rev_r };
            let dry_ch = inputs.get(ch % inputs.len()).unwrap();
            for ((s, r), d) in dry_ch.iter().zip(rev.iter()).zip(o.iter_mut()) {
                *d = *s * (1.0 - self.wet) + *r * self.wet;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "room_size" => {
                let v = value_as_f32(name, value)?;
                self.room = RangedParam { min: 0.0, max: 1.0, default: 0.5 }.clamp(v);
                self.retune();
                Ok(())
            }
            "damping" => {
                let v = value_as_f32(name, value)?;
                self.damping = RangedParam { min: 0.0, max: 1.0, default: 0.5 }.clamp(v);
                self.retune();
                Ok(())
            }
            "wet" => {
                let v = value_as_f32(name, value)?;
                self.wet = RangedParam { min: 0.0, max: 1.0, default: 0.33 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "room_size",
                value: ParamValue::Float(self.room),
                range: Some(RangedParam { min: 0.0, max: 1.0, default: 0.5 }),
                public: true,
            },
            PublicParam {
                name: "damping",
                value: ParamValue::Float(self.damping),
                range: Some(RangedParam { min: 0.0, max: 1.0, default: 0.5 }),
                public: true,
            },
            PublicParam {
                name: "wet",
                value: ParamValue::Float(self.wet),
                range: Some(RangedParam { min: 0.0, max: 1.0, default: 0.33 }),
                public: true,
            },
        ]
    }
}

/// Tube-style saturation: tanh drive + tone low-pass + dry/wet mix.
#[derive(Debug)]
pub struct Tube {
    drive: f32,
    tone_hz: f32,
    mix: f32,
    lp: Vec<f32>,
}

impl Default for Tube {
    fn default() -> Self {
        Self { drive: 2.0, tone_hz: 8000.0, mix: 1.0, lp: Vec::new() }
    }
}

impl AudioNode for Tube {
    fn name(&self) -> &'static str {
        "tube"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        if self.lp.len() < inputs.len() {
            self.lp.resize(inputs.len(), 0.0);
        }
        // One-pole tone control coefficient from cutoff.
        let rc = 1.0 / (2.0 * std::f32::consts::PI * self.tone_hz.max(20.0));
        let dt = 1.0 / ctx.rate as f32;
        let alpha = dt / (rc + dt);
        let norm = (self.drive.max(0.001)).tanh();
        for ((i, o), z) in inputs.iter().zip(outputs.iter_mut()).zip(self.lp.iter_mut()) {
            for (s, d) in i.iter().zip(o.iter_mut()) {
                let shaped = ((self.drive * *s).tanh()) / norm;
                *z += alpha * (shaped - *z);
                *d = *s * (1.0 - self.mix) + *z * self.mix;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "drive" => {
                let v = value_as_f32(name, value)?;
                self.drive = RangedParam { min: 0.0, max: 10.0, default: 2.0 }.clamp(v);
                Ok(())
            }
            "tone_hz" => {
                let v = value_as_f32(name, value)?;
                self.tone_hz = RangedParam { min: 500.0, max: 18_000.0, default: 8_000.0 }.clamp(v);
                Ok(())
            }
            "mix" => {
                let v = value_as_f32(name, value)?;
                self.mix = RangedParam { min: 0.0, max: 1.0, default: 1.0 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "drive",
                value: ParamValue::Float(self.drive),
                range: Some(RangedParam { min: 0.0, max: 10.0, default: 2.0 }),
                public: true,
            },
            PublicParam {
                name: "tone_hz",
                value: ParamValue::Float(self.tone_hz),
                range: Some(RangedParam { min: 500.0, max: 18_000.0, default: 8_000.0 }),
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
/// Coefficients follow the standard RBJ cookbook (clean-room
/// re-derivation of textbook formulas).
/// Test oscillator: pure sine generator (measurement fixture, not an effect).
#[derive(Debug)]
pub struct Oscillator {
    freq: f32,
    phase: f32,
}

impl Default for Oscillator {
    fn default() -> Self {
        Self { freq: 440.0, phase: 0.0 }
    }
}

impl AudioNode for Oscillator {
    fn name(&self) -> &'static str {
        "oscillator"
    }
    fn process(&mut self, ctx: &ProcessContext, _inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let step = 2.0 * std::f32::consts::PI * self.freq / ctx.rate as f32;
        for o in outputs.iter_mut() {
            for d in o.iter_mut() {
                *d = self.phase.sin();
                self.phase += step;
                if self.phase >= 2.0 * std::f32::consts::PI {
                    self.phase -= 2.0 * std::f32::consts::PI;
                }
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "freq_hz" => {
                let v = value_as_f32(name, value)?;
                self.freq = RangedParam { min: 20.0, max: 20_000.0, default: 440.0 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![PublicParam {
            name: "freq_hz",
            value: ParamValue::Float(self.freq),
            range: Some(RangedParam { min: 20.0, max: 20_000.0, default: 440.0 }),
            public: true,
        }]
    }
}

/// Delay line with feedback (slapback/echo). Original implementation.
#[derive(Debug)]
pub struct Delay {
    buf: Vec<f32>,
    pos: usize,
    delay_ms: f32,
    feedback: f32,
    mix: f32,
    rate: f32,
}

impl Default for Delay {
    fn default() -> Self {
        Self { buf: vec![0.0; 96_000], pos: 0, delay_ms: 250.0, feedback: 0.3, mix: 0.5, rate: 48_000.0 }
    }
}

impl Delay {
    fn delay_samples(&self) -> usize {
        ((self.delay_ms.max(0.0) / 1000.0 * self.rate) as usize).clamp(1, self.buf.len() - 1)
    }
}

impl AudioNode for Delay {
    fn name(&self) -> &'static str {
        "delay"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        self.rate = ctx.rate as f32;
        let d = self.delay_samples();
        for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
            for (s, dst) in i.iter().zip(o.iter_mut()) {
                let read = (self.pos + self.buf.len() - d) % self.buf.len();
                let delayed = self.buf[read];
                self.buf[self.pos] = *s + delayed * self.feedback;
                self.pos = (self.pos + 1) % self.buf.len();
                *dst = *s * (1.0 - self.mix) + delayed * self.mix;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "delay_ms" => {
                let v = value_as_f32(name, value)?;
                self.delay_ms = RangedParam { min: 0.0, max: 2000.0, default: 250.0 }.clamp(v);
                Ok(())
            }
            "feedback" => {
                let v = value_as_f32(name, value)?;
                self.feedback = RangedParam { min: 0.0, max: 0.95, default: 0.3 }.clamp(v);
                Ok(())
            }
            "mix" => {
                let v = value_as_f32(name, value)?;
                self.mix = RangedParam { min: 0.0, max: 1.0, default: 0.5 }.clamp(v);
                Ok(())
            }
            _ => Err(ParamError::Unknown(name.into())),
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "delay_ms",
                value: ParamValue::Float(self.delay_ms),
                range: Some(RangedParam { min: 0.0, max: 2000.0, default: 250.0 }),
                public: true,
            },
            PublicParam {
                name: "feedback",
                value: ParamValue::Float(self.feedback),
                range: Some(RangedParam { min: 0.0, max: 0.95, default: 0.3 }),
                public: true,
            },
            PublicParam {
                name: "mix",
                value: ParamValue::Float(self.mix),
                range: Some(RangedParam { min: 0.0, max: 1.0, default: 0.5 }),
                public: true,
            },
        ]
    }
}

#[derive(Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: Vec<f32>,
    z2: Vec<f32>,
}

impl Default for Biquad {
    fn default() -> Self {
        let mut b = Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: Vec::new(),
            z2: Vec::new(),
        };
        b.set_lowpass(48_000.0, 1_000.0, 0.7071);
        b
    }
}

impl Biquad {
    /// Low-pass; `q` is the resonance (0.5 = Butterworth-ish for 2nd order
    /// cascades; 0.7071 = maximally flat single stage).
    pub fn set_lowpass(&mut self, rate: f32, freq: f32, q: f32) {
        let w0 = 2.0 * std::f32::consts::PI * freq / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let b0 = (1.0 - cos) / 2.0;
        let b1 = 1.0 - cos;
        let b2 = (1.0 - cos) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }
}

impl AudioNode for Biquad {
    fn name(&self) -> &'static str {
        "biquad"
    }
    fn process(&mut self, _ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        if self.z1.len() < inputs.len() {
            self.z1.resize(inputs.len(), 0.0);
            self.z2.resize(inputs.len(), 0.0);
        }
        for ((i, o), (z1, z2)) in inputs
            .iter()
            .zip(outputs.iter_mut())
            .zip(self.z1.iter_mut().zip(self.z2.iter_mut()))
        {
            for (s, d) in i.iter().zip(o.iter_mut()) {
                // Transposed DF-II, denormal-guarded by voice-gate upstream.
                let out = self.b0 * *s + *z1;
                *z1 = self.b1 * *s - self.a1 * out + *z2;
                *z2 = self.b2 * *s - self.a2 * out;
                *d = out;
            }
        }
    }
    fn set_param(&mut self, name: &str, _value: ParamValue) -> Result<(), ParamError> {
        // Frequency/q are construction-time in MVP; runtime retune lands
        // in Fase 3b with the EQ node work.
        Err(ParamError::Unknown(name.into()))
    }
    fn params(&self) -> Vec<PublicParam> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessContext;

    const RATE: u32 = 48_000;

    fn ctx(frames: usize) -> ProcessContext {
        ProcessContext { rate: RATE, frames }
    }

    fn sine(freq: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin())
            .collect()
    }

    /// Dominant frequency via zero-crossing rate (robust for near-sine).
    fn zero_cross_freq(x: &[f32]) -> f32 {
        let mut crossings = 0;
        for w in x.windows(2) {
            if (w[0] <= 0.0) != (w[1] <= 0.0) {
                crossings += 1;
            }
        }
        crossings as f32 * RATE as f32 / (2.0 * x.len() as f32)
    }

    fn run_node(node: &mut dyn AudioNode, input: &[f32], chunk: usize) -> Vec<f32> {
        let c = ctx(chunk);
        let mut out = vec![0.0; input.len()];
        for (ib, ob) in input.chunks(chunk).zip(out.chunks_mut(chunk)) {
            // Pad final partial chunk.
            let mut ib_pad = ib.to_vec();
            ib_pad.resize(chunk, 0.0);
            let mut ob_pad = vec![0.0; chunk];
            let ins = [ib_pad.as_slice()];
            let mut outs = [ob_pad.as_mut_slice()];
            node.process(&c, &ins, &mut outs);
            ob.copy_from_slice(&outs[0][..ob.len()]);
        }
        out
    }

    fn settle_skipped(x: &[f32], skip_secs: f32) -> &[f32] {
        let skip = (skip_secs * RATE as f32) as usize;
        &x[skip.min(x.len())..]
    }

    #[test]
    fn pitch_fft_octave_up() {
        let mut n = PitchFFT::default();
        n.set_param("transpose_semitones", ParamValue::Float(12.0)).unwrap();
        let input = sine(440.0, RATE as usize); // 1 s
        let out = run_node(&mut n, &input, 960);
        // Skip STFT latency + startup (~0.3 s), measure the steady state.
        let steady = settle_skipped(&out, 0.4);
        let f = zero_cross_freq(steady);
        assert!((f - 880.0).abs() < 880.0 * 0.05, "got {f:.1} Hz, want ~880");
        assert!(steady.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn pitch_fft_bypass_at_zero() {
        let mut n = PitchFFT::default();
        let input = sine(440.0, RATE as usize);
        let out = run_node(&mut n, &input, 960);
        let steady = settle_skipped(&out, 0.4);
        let f = zero_cross_freq(steady);
        assert!((f - 440.0).abs() < 440.0 * 0.05, "got {f:.1} Hz, want ~440");
    }

    #[test]
    fn formant_shifter_tracks_and_clamps() {
        let mut n = FormantPitchShifter::default();
        n.set_param("transpose_semitones", ParamValue::Float(-12.0)).unwrap();
        n.set_param("formant_shift_semitones", ParamValue::Float(0.0)).unwrap();
        // Out-of-range clamps, unknown names error.
        n.set_param("transpose_semitones", ParamValue::Float(99.0)).unwrap();
        assert!(n.transpose <= 12.0);
        assert!(n.set_param("nope", ParamValue::Float(1.0)).is_err());
        let input = sine(440.0, RATE as usize);
        let out = run_node(&mut n, &input, 960);
        let steady = settle_skipped(&out, 0.4);
        let f = zero_cross_freq(steady);
        assert!((f - 880.0).abs() < 880.0 * 0.06, "got {f:.1} Hz, want ~880");
        assert!(steady.iter().all(|s| s.is_finite()));
        let peak = steady.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak > 0.05, "output too quiet: {peak}");
    }

    #[test]
    fn reverb_tail_decays_and_dry_passthrough() {
        // Dry passthrough exactness.
        let mut dry = Reverb::default();
        dry.set_param("wet", ParamValue::Float(0.0)).unwrap();
        let input = sine(440.0, 960 * 2);
        let out = run_node(&mut dry, &input, 960);
        for (a, b) in input.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
        // Impulse → audible tail that decays.
        let mut wet = Reverb::default();
        wet.set_param("room_size", ParamValue::Float(0.9)).unwrap();
        wet.set_param("wet", ParamValue::Float(1.0)).unwrap();
        let mut impulse = vec![0.0; 960 * 40];
        impulse[0] = 1.0;
        let out = run_node(&mut wet, &impulse, 960);
        let early: f32 = out[100..2000].iter().map(|s| s * s).sum();
        let late: f32 = out[out.len() - 2000..].iter().map(|s| s * s).sum();
        assert!(late > 0.0, "no reverb tail at all");
        assert!(late < early, "tail did not decay (early={early}, late={late})");
        assert!(out.iter().all(|s| s.is_finite() && s.abs() < 10.0));
    }

    #[test]
    fn tube_saturates_and_bounds() {
        let mut t = Tube::default();
        t.set_param("drive", ParamValue::Float(8.0)).unwrap();
        let input = sine(440.0, 960 * 4);
        let out = run_node(&mut t, &input, 960);
        assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0 + 1e-6));
        // Saturated output differs from a clean sine (harmonics added).
        let diff: f32 = input.iter().zip(out.iter()).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 1.0, "tube did not saturate");
    }
}

#[cfg(test)]
mod osc_tests {
    use super::*;
    use crate::ProcessContext;

    #[test]
    fn oscillator_emits_configured_sine() {
        let mut o = Oscillator::default();
        o.set_param("freq_hz", ParamValue::Float(440.0)).unwrap();
        let ctx = ProcessContext { rate: 48_000, frames: 960 };
        let mut out = vec![vec![0.0f32; 960]];
        let ins: Vec<&[f32]> = Vec::new();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
        o.process(&ctx, &ins, &mut outs);
        let mut crossings = 0;
        for w in out[0].windows(2) {
            if (w[0] <= 0.0) != (w[1] <= 0.0) {
                crossings += 1;
            }
        }
        let f = crossings as f32 * 48_000.0 / (2.0 * 960.0);
        assert!((f - 440.0).abs() < 440.0 * 0.05, "got {f:.1} Hz");
    }
}

#[cfg(test)]
mod delay_tests {
    use super::*;
    use crate::ProcessContext;

    #[test]
    fn delay_echoes_impulse() {
        let mut d = Delay::default();
        d.set_param("delay_ms", ParamValue::Float(10.0)).unwrap(); // 480 samples @48k
        d.set_param("feedback", ParamValue::Float(0.0)).unwrap();
        d.set_param("mix", ParamValue::Float(1.0)).unwrap();
        let ctx = ProcessContext { rate: 48_000, frames: 960 };
        // Impulse then silence, over two quanta.
        let mut input = vec![0.0f32; 1920];
        input[0] = 1.0;
        let mut out = vec![vec![0.0f32; 1920]];
        let ins = [input.as_slice()];
        let mut outs = [out[0].as_mut_slice()];
        // Feed in two 960-frame calls to exercise cross-call state.
        for k in 0..2 {
            let ins_k = [&ins[0][k * 960..(k + 1) * 960]];
            let mut outs_k = [&mut outs[0][k * 960..(k + 1) * 960]];
            d.process(&ctx, &ins_k, &mut outs_k);
        }
        // Dry at 0, single wet echo at sample 480.
        assert!(out[0][0].abs() < 1e-6, "dry must be silent with mix=1");
        assert!((out[0][480] - 1.0).abs() < 1e-5, "echo at delay, got {}", out[0][480]);
        assert!(out[0][479].abs() < 1e-6 && out[0][481].abs() < 1e-6);
        // Int values coerce (UI sends ints for int-typed params).
        d.set_param("delay_ms", ParamValue::Int(10)).unwrap();
    }
}
