//! DeepFilterNet runtime (F-0.4.A, `neural` feature).
//!
//! Faithful runtime for DeepFilterNet3 ONNX bundles (`enc`, `erb_dec`,
//! `df_dec` and `config.ini`, as a directory or `.tar.gz`): ERB
//! masking (stage 1) plus complex deep filtering (stage 2) over a
//! 960/480 STFT, driven frame-by-frame through pulsed tract sessions.
//! Hyperparameters come from the model's own `config.ini` — never
//! hardcoded — so any DF3 bundle with the same layout runs.
//!
//! Model files are user-supplied (`model_path`): download a DF3 bundle
//! (e.g. the `deepfilternet3-onnx` release) and point the node at the
//! directory (or tarball). Without a model the node passes through and
//! `model_path` fails loud on missing/corrupt files.

use super::node::{AudioNode, ParamError, ProcessContext};
use super::param::{ParamValue, PublicParam, RangedParam};
use std::collections::VecDeque;
use std::io::Read as _;
use std::path::Path;
use tract_data::prelude::*;
use tract_data::tvec;
use tract_hir::infer::InferenceFact;
use tract_hir::internal::*;
use tract_pulse::model::PulsedModelExt;

type C32 = rustfft::num_complex::Complex<f32>;

// ---------------------------------------------------------------------------
// Model bundle loading (directory or .tar.gz with enc/erb_dec/df_dec + ini).
// ---------------------------------------------------------------------------

struct DfBundle {
    enc: Vec<u8>,
    erb_dec: Vec<u8>,
    df_dec: Vec<u8>,
    ini: ini::Ini,
}

fn load_bundle(path: &str) -> Result<DfBundle, String> {
    let p = Path::new(path);
    if p.is_dir() {
        let read = |n: &str| {
            std::fs::read(p.join(n)).map_err(|e| format!("{path}: missing {n} ({e})"))
        };
        let ini_text =
            std::fs::read_to_string(p.join("config.ini")).map_err(|e| format!("{path}: {e}"))?;
        let ini = ini::Ini::load_from_str(&ini_text).map_err(|e| format!("{path}: bad ini ({e})"))?;
        Ok(DfBundle {
            enc: read("enc.onnx")?,
            erb_dec: read("erb_dec.onnx")?,
            df_dec: read("df_dec.onnx")?,
            ini,
        })
    } else if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        let file = std::fs::File::open(p).map_err(|e| format!("read {path}: {e}"))?;
        let gz = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(gz);
        let mut enc = Vec::new();
        let mut erb_dec = Vec::new();
        let mut df_dec = Vec::new();
        let mut ini_text = String::new();
        for entry in archive.entries().map_err(|e| format!("{path}: bad tar ({e})"))? {
            let mut f = entry.map_err(|e| format!("{path}: bad entry ({e})"))?;
            let name = f.path().map_err(|e| format!("{path}: bad name ({e})"))?.to_string_lossy().into_owned();
            if name.ends_with("enc.onnx") {
                f.read_to_end(&mut enc).map_err(|e| format!("{path}: {e}"))?;
            } else if name.ends_with("erb_dec.onnx") {
                f.read_to_end(&mut erb_dec).map_err(|e| format!("{path}: {e}"))?;
            } else if name.ends_with("df_dec.onnx") {
                f.read_to_end(&mut df_dec).map_err(|e| format!("{path}: {e}"))?;
            } else if name.ends_with("config.ini") {
                f.read_to_string(&mut ini_text).map_err(|e| format!("{path}: {e}"))?;
            }
        }
        if enc.is_empty() || erb_dec.is_empty() || df_dec.is_empty() || ini_text.is_empty() {
            return Err(format!("{path}: tarball lacks enc/erb_dec/df_dec/config.ini"));
        }
        let ini = ini::Ini::load_from_str(&ini_text).map_err(|e| format!("{path}: bad ini ({e})"))?;
        Ok(DfBundle { enc, erb_dec, df_dec, ini })
    } else {
        Err(format!("{path}: want a model directory or .tar.gz bundle"))
    }
}

// ---------------------------------------------------------------------------
// DF DSP state (STFT, ERB features, norms, masking — the DF3 recipe).
// ---------------------------------------------------------------------------

const MEAN_NORM_INIT: [f32; 2] = [-60., -90.];
const UNIT_NORM_INIT: [f32; 2] = [0.001, 0.0001];

fn erb_fb(sr: usize, fft_size: usize, nb_bands: usize, min_nb_freqs: usize) -> Vec<usize> {
    let erb_of = |f: f32| 9.265 * (f / (24.7 * 9.265)).ln_1p();
    let freq_of = |e: f32| 24.7 * 9.265 * ((e / 9.265).exp() - 1.);
    let freq_width = sr as f32 / fft_size as f32;
    let (lo, hi) = (erb_of(0.), erb_of(sr as f32 / 2.));
    let step = (hi - lo) / nb_bands as f32;
    let mut erb = vec![0; nb_bands];
    let mut prev = 0i32;
    let mut over = 0i32;
    for i in 1..=nb_bands {
        let fb = (freq_of(lo + i as f32 * step) / freq_width).round() as usize;
        let mut n = fb as i32 - prev - over;
        if n < min_nb_freqs as i32 {
            over = min_nb_freqs as i32 - n;
            n = min_nb_freqs as i32;
        } else {
            over = 0;
        }
        erb[i - 1] = n as usize;
        prev = fb as i32;
    }
    erb[nb_bands - 1] += 1;
    let too_large = erb.iter().sum::<usize>() as i64 - (fft_size / 2 + 1) as i64;
    if too_large > 0 {
        erb[nb_bands - 1] -= too_large as usize;
    }
    erb
}

struct DfDsp {
    hop: usize,
    window: Vec<f32>,
    wnorm: f32,
    erb: Vec<usize>,
    fwd: std::sync::Arc<dyn rustfft::Fft<f32>>,
    inv: std::sync::Arc<dyn rustfft::Fft<f32>>,
    ana_mem: Vec<f32>,
    syn_mem: Vec<f32>,
    mean_norm: Vec<f32>,
    unit_norm: Vec<f32>,
}

impl DfDsp {
    fn new(sr: usize, fft_size: usize, hop: usize, nb_erb: usize, min_nb: usize, nb_df: usize) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        // Vorbis window: sin(π/2·sin²(π·n/N)), exactly as trained.
        let mut window = vec![0.0; fft_size];
        for (i, w) in window.iter_mut().enumerate() {
            let s = (0.5 * std::f64::consts::PI * (i as f64 + 0.5) / (fft_size / 2) as f64).sin();
            *w = (0.5 * std::f64::consts::PI * s * s).sin() as f32;
        }
        let wnorm = 1. / (fft_size.pow(2) as f32 / (2 * hop) as f32);
        let erb = erb_fb(sr, fft_size, nb_erb, min_nb);
        let mean_norm: Vec<f32> = (0..nb_erb)
            .map(|i| MEAN_NORM_INIT[0] + i as f32 * (MEAN_NORM_INIT[1] - MEAN_NORM_INIT[0]) / (nb_erb - 1) as f32)
            .collect();
        let unit_norm: Vec<f32> = (0..nb_df)
            .map(|i| UNIT_NORM_INIT[0] + i as f32 * (UNIT_NORM_INIT[1] - UNIT_NORM_INIT[0]) / (nb_df - 1) as f32)
            .collect();
        Self {
            hop,
            window,
            wnorm,
            erb,
            fwd: planner.plan_fft_forward(fft_size),
            inv: planner.plan_fft_inverse(fft_size),
            ana_mem: vec![0.; fft_size - hop],
            syn_mem: vec![0.; fft_size - hop],
            mean_norm,
            unit_norm,
        }
    }

    fn analysis(&mut self, input: &[f32], output: &mut [C32]) {
        use rustfft::num_complex::Complex as RC;
        let n = self.window.len();
        let mut buf: Vec<RC<f32>> = Vec::with_capacity(n);
        let mem = self.ana_mem.len();
        for i in 0..n - self.hop {
            let (y, w) = if i < mem {
                (self.ana_mem[i], self.window[i])
            } else {
                (input[i - mem], self.window[i])
            };
            buf.push(RC::new(y * w, 0.0));
        }
        for (i, y) in input.iter().enumerate().take(self.hop) {
            buf.push(RC::new(y * self.window[n - self.hop + i], 0.0));
        }
        self.ana_mem.rotate_left(self.hop);
        self.ana_mem[n - 2 * self.hop..].copy_from_slice(&input[..self.hop.min(input.len())]);
        let mut spec = buf;
        self.fwd.process(&mut spec);
        for (o, s) in output.iter_mut().zip(spec.iter()) {
            *o = C32::new(s.re * self.wnorm, s.im * self.wnorm);
        }
    }

    fn synthesis(&mut self, input: &mut [C32], output: &mut [f32]) {
        use rustfft::num_complex::Complex as RC;
        let n = self.window.len();
        let mut buf: Vec<RC<f32>> =
            input.iter().map(|c| RC::new(c.re, c.im)).collect();
        buf.resize(n, RC::new(0.0, 0.0));
        // Full-spectrum inverse via conjugate symmetry is handled by
        // processing the packed half through a mirrored buffer.
        let mut full = vec![RC::new(0.0, 0.0); n];
        for (i, c) in input.iter().enumerate() {
            full[i] = RC::new(c.re, c.im);
        }
        for i in 1..n / 2 {
            full[n - i] = RC::new(input[i].re, -input[i].im);
        }
        self.inv.process(&mut full);
        // No 1/N here: the analysis-side wnorm already carries the
        // roundtrip normalization (2F/N² = 1/N at 50% overlap), exactly
        // like the reference realfft pipeline. An extra 1/N would
        // silence the output by ~60 dB (found the hard way).
        for (i, s) in full.iter().enumerate().take(n) {
            let v = s.re * self.window[i];
            if i < self.hop {
                output[i] = v + self.syn_mem[i];
            }
        }
        // Shift synthesis memory and store the new tail.
        self.syn_mem.rotate_left(self.hop);
        let m = self.syn_mem.len();
        // True tail: windowed IFFT samples [hop..] land in syn_mem.
        for (i, s) in full.iter().enumerate().skip(self.hop).take(m) {
            self.syn_mem[i - self.hop] = s.re * self.window[i];
        }
    }

    /// ERB log-energies + exponential mean norm (÷40).
    fn feat_erb(&mut self, spec: &[C32], alpha: f32, out: &mut [f32]) {
        let mut bcsum = 0;
        for (b, o) in self.erb.iter().zip(out.iter_mut()) {
            let mut e = 0.0f32;
            for j in 0..*b {
                let c = spec[bcsum + j];
                e += (c.re * c.re + c.im * c.im) / *b as f32;
            }
            bcsum += b;
            *o = (e + 1e-10).log10() * 10.;
        }
        for (x, s) in out.iter_mut().zip(self.mean_norm.iter_mut()) {
            *s = *x * (1. - alpha) + *s * alpha;
            *x -= *s;
            *x /= 40.;
        }
    }

    /// First `nb_df` bins, unit-normalized in place.
    fn feat_cplx(&mut self, spec: &[C32], alpha: f32, out: &mut [C32]) {
        out.clone_from_slice(&spec[..out.len()]);
        for (x, s) in out.iter_mut().zip(self.unit_norm.iter_mut()) {
            *s = x.norm() * (1. - alpha) + *s * alpha;
            let n = s.sqrt().max(1e-12);
            x.re /= n;
            x.im /= n;
        }
    }

    /// Multiply the full spectrum by ERB-interpolated gains.
    fn apply_mask(&self, spec: &mut [C32], gains: &[f32]) {
        let mut bcsum = 0;
        for (b, g) in self.erb.iter().zip(gains.iter()) {
            for j in 0..*b {
                let c = &mut spec[bcsum + j];
                c.re *= *g;
                c.im *= *g;
            }
            bcsum += b;
        }
    }
}

fn post_filter(noisy: &[C32], enh: &mut [C32], beta: f32) {
    let beta_p1 = beta + 1.;
    let eps = 1e-12f32;
    let pi = std::f32::consts::PI;
    for (n, e) in noisy
        .as_chunks::<4>()
        .0
        .iter()
        .zip(enh.as_chunks_mut::<4>().0.iter_mut())
    {
        for (nn, ee) in n.iter().zip(e.iter_mut()) {
            let g = (ee.norm() / (nn.norm() + eps)).min(1.).max(eps);
            let gs = g * (g * pi / 2.0).sin();
            let pf = (beta_p1 * g / (1. + beta * (g / gs).powi(2))) / g;
            ee.re *= pf;
            ee.im *= pf;
        }
    }
}

fn atten_lim_of(atten_db: f32) -> Option<f32> {
    let lim = atten_db.abs();
    if lim >= 100. {
        None
    } else if lim < 0.01 {
        Some(1.)
    } else {
        Some(10f32.powf(-lim / 20.))
    }
}

fn calc_norm_alpha(sr: usize, hop: usize, tau: f32) -> f32 {
    let dt = hop as f32 / sr as f32;
    let alpha = f32::exp(-dt / tau);
    let mut a = 1.0;
    let mut precision = 3;
    while a >= 1.0 {
        a = (alpha * 10i32.pow(precision) as f32).round() / 10i32.pow(precision) as f32;
        precision += 1;
    }
    a
}

// ---------------------------------------------------------------------------
// Pulsed tract sessions (encoder + ERB/DF decoders).
// ---------------------------------------------------------------------------

type TractSession = tract_onnx::tract_core::model::typed::TypedSimpleState;

fn load_session(
    bytes: &[u8],
    inputs: &[(&str, Vec<DimDesc>)],
    outputs: &[&str],
) -> Result<TractSession, String> {
    let mut m = tract_onnx::onnx()
        .model_for_read(&mut std::io::Cursor::new(bytes))
        .map_err(|e| format!("onnx parse: {e}"))?;
    let s = m.sym("S");
    for (i, (name, shape)) in inputs.iter().enumerate() {
        if shape.is_empty() || shape.len() > 4 {
            return Err(format!("input {name}: want 1..=4 dims"));
        }
        let dims: Vec<_> = shape.iter().map(|d| dim_of(d, &s)).collect();
        let fact = InferenceFact::from(f32::fact(dims));
        m = m.with_input_fact(i, fact).map_err(|e| format!("input {name}: {e}"))?;
    }
    m = m
        .with_input_names(inputs.iter().map(|(n, _)| *n))
        .map_err(|e| format!("input names: {e}"))?;
    m = m.with_outputs_by_name(outputs).map_err(|e| format!("output names: {e}"))?;
    m.analyse(true).map_err(|e| format!("analyse: {e}"))?;
    let mut m = m.into_typed().map_err(|e| format!("typed: {e}"))?;
    m.declutter().map_err(|e| format!("declutter: {e}"))?;
    let pulsed = tract_pulse::model::PulsedModel::new(&m, s, &1.to_dim())
        .map_err(|e| format!("pulse: {e}"))?;
    let m = pulsed.into_typed().map_err(|e| format!("pulse typed: {e}"))?;
    let m = m.into_optimized().map_err(|e| format!("optimize: {e}"))?;
    let plan = m.into_runnable().map_err(|e| format!("runnable: {e}"))?;
    plan.spawn().map_err(|e| format!("state: {e}"))
}

/// One input dimension: fixed size or the streaming axis.
#[derive(Clone, Copy)]
enum DimDesc {
    Fixed(usize),
    Stream,
}

fn dim_of(d: &DimDesc, s: &Symbol) -> TDim {
    match d {
        DimDesc::Fixed(v) => TDim::from(*v),
        DimDesc::Stream => TDim::from(s.clone()),
    }
}

// ---------------------------------------------------------------------------
// The streaming DF3 runtime: one hop in, one hop out.
// ---------------------------------------------------------------------------

pub struct DfRunner {
    hop: usize,
    n_freqs: usize,
    nb_erb: usize,
    nb_df: usize,
    df_order: usize,
    lookahead: usize,
    n_ch: usize,
    alpha: f32,
    atten_lim: Option<f32>,
    postfilter_beta: f32,
    erb_thresh: f32,
    df_thresh: f32,
    min_thresh: f32,
    dsp: Vec<DfDsp>,
    enc: TractSession,
    erb_dec: TractSession,
    df_dec: TractSession,
    spec_buf: Vec<C32>,
    erb_buf: Vec<f32>,
    cplx_buf: Vec<C32>,
    roll_y: VecDeque<Vec<C32>>,
    roll_x: VecDeque<Vec<C32>>,
    skips: usize,
}

impl DfRunner {
    fn new_bundled(
        bundle: &DfBundle,
        n_ch: usize,
        atten_db: f32,
        erb_thresh: f32,
        df_thresh: f32,
        postfilter_beta: f32,
    ) -> Result<Self, String> {
        let get = |sec: &str, key: &str| {
            bundle
                .ini
                .get_from(Some(sec), key)
                .ok_or_else(|| format!("config.ini lacks [{sec}] {key}"))
                .map(|s| s.to_string())
        };
        let num = |sec: &str, key: &str| {
            get(sec, key)?.parse::<usize>().map_err(|_| format!("config.ini: [{sec}] {key} not a number"))
        };
        let sr = num("df", "sr")?;
        if sr != 48_000 {
            return Err(format!("model sample rate is {sr}, this node runs at 48000"));
        }
        let fft_size = num("df", "fft_size")?;
        let hop = num("df", "hop_size")?;
        let nb_erb = num("df", "nb_erb")?;
        let nb_df = num("df", "nb_df")?;
        let min_nb = num("df", "min_nb_erb_freqs")?;
        let df_order = get("df", "df_order")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .or_else(|| {
                bundle.ini.get_from(Some("deepfilternet"), "df_order")?.parse::<usize>().ok()
            })
            .ok_or_else(|| "config.ini lacks df_order".to_string())?;
        let df_look = get("df", "df_lookahead")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .or_else(|| {
                bundle.ini.get_from(Some("deepfilternet"), "df_lookahead")?.parse::<usize>().ok()
            })
            .ok_or_else(|| "config.ini lacks df_lookahead".to_string())?;
        let conv_look: usize = bundle
            .ini
            .get_from(Some("deepfilternet"), "conv_lookahead")
            .ok_or_else(|| "config.ini lacks conv_lookahead".to_string())?
            .parse()
            .map_err(|_| "config.ini: conv_lookahead not a number".to_string())?;
        let model = bundle
            .ini
            .get_from(Some("train"), "model")
            .ok_or_else(|| "config.ini lacks [train] model".to_string())?;
        if model != "deepfilternet3" {
            return Err(format!("unsupported model type '{model}' (want deepfilternet3)"));
        }
        let lookahead = conv_look.max(df_look);
        let n_freqs = fft_size / 2 + 1;
        let alpha = match bundle.ini.get_from(Some("df"), "norm_alpha") {
            Some(a) => a.parse::<f32>().map_err(|_| "config.ini: norm_alpha".to_string())?,
            None => {
                let tau: f32 = bundle
                    .ini
                    .get_from(Some("df"), "norm_tau")
                    .ok_or_else(|| "config.ini lacks norm_tau".to_string())?
                    .parse()
                    .map_err(|_| "config.ini: norm_tau".to_string())?;
                calc_norm_alpha(sr, hop, tau)
            }
        };
        // Encoder: [n_ch,1,S,nb_erb] + [n_ch,2,S,nb_df] → e0..e3, emb, c0, lsnr.
        // Shapes use 0 for the streaming axis S (pulsed to 1).
        let enc = load_session(
            &bundle.enc,
            &[
                (
                    "feat_erb",
                    vec![
                        DimDesc::Fixed(n_ch),
                        DimDesc::Fixed(1),
                        DimDesc::Stream,
                        DimDesc::Fixed(nb_erb),
                    ],
                ),
                (
                    "feat_spec",
                    vec![
                        DimDesc::Fixed(n_ch),
                        DimDesc::Fixed(2),
                        DimDesc::Stream,
                        DimDesc::Fixed(nb_df),
                    ],
                ),
            ],
            &["e0", "e1", "e2", "e3", "emb", "c0", "lsnr"],
        )?;
        // conv_ch from the net section: hidden width factor.
        let conv_ch: usize = bundle
            .ini
            .get_from(Some("deepfilternet"), "conv_ch")
            .ok_or_else(|| "config.ini lacks conv_ch".to_string())?
            .parse()
            .map_err(|_| "config.ini: conv_ch".to_string())?;
        let n_hidden = conv_ch * nb_erb / 4;
        let e3f = nb_erb / 4;
        let e1f = nb_erb / 2;
        let erb_dec = load_session(
            &bundle.erb_dec,
            &[
                (
                    "emb",
                    vec![DimDesc::Fixed(n_ch), DimDesc::Stream, DimDesc::Fixed(n_hidden)],
                ),
                (
                    "e3",
                    vec![
                        DimDesc::Fixed(n_ch),
                        DimDesc::Fixed(conv_ch),
                        DimDesc::Stream,
                        DimDesc::Fixed(e3f),
                    ],
                ),
                (
                    "e2",
                    vec![
                        DimDesc::Fixed(n_ch),
                        DimDesc::Fixed(conv_ch),
                        DimDesc::Stream,
                        DimDesc::Fixed(e3f),
                    ],
                ),
                (
                    "e1",
                    vec![
                        DimDesc::Fixed(n_ch),
                        DimDesc::Fixed(conv_ch),
                        DimDesc::Stream,
                        DimDesc::Fixed(e1f),
                    ],
                ),
                (
                    "e0",
                    vec![
                        DimDesc::Fixed(n_ch),
                        DimDesc::Fixed(conv_ch),
                        DimDesc::Stream,
                        DimDesc::Fixed(nb_erb),
                    ],
                ),
            ],
            &["m"],
        )?;
        let df_dec = load_session(
            &bundle.df_dec,
            &[
                (
                    "emb",
                    vec![DimDesc::Fixed(n_ch), DimDesc::Stream, DimDesc::Fixed(n_hidden)],
                ),
                (
                    "c0",
                    vec![
                        DimDesc::Fixed(n_ch),
                        DimDesc::Fixed(conv_ch),
                        DimDesc::Stream,
                        DimDesc::Fixed(nb_df),
                    ],
                ),
            ],
            &["coefs"],
        )?;
        let atten_lim = atten_lim_of(atten_db);
        let n = df_order + lookahead;
        let mut roll_y = VecDeque::with_capacity(n + 1);
        let mut roll_x = VecDeque::with_capacity(lookahead.max(df_order) + 1);
        for _ in 0..n {
            roll_y.push_back(vec![C32::new(0., 0.); n_ch * n_freqs]);
        }
        for _ in 0..lookahead.max(df_order) {
            roll_x.push_back(vec![C32::new(0., 0.); n_ch * n_freqs]);
        }
        let mut dsp = Vec::with_capacity(n_ch);
        for _ in 0..n_ch {
            dsp.push(DfDsp::new(sr, fft_size, hop, nb_erb, min_nb, nb_df));
        }
        let mut runner = Self {
            hop,
            n_freqs,
            nb_erb,
            nb_df,
            df_order,
            lookahead,
            n_ch,
            alpha,
            atten_lim,
            postfilter_beta,
            erb_thresh,
            df_thresh,
            min_thresh: -10.0,
            dsp,
            enc,
            erb_dec,
            df_dec,
            spec_buf: vec![C32::new(0., 0.); n_ch * n_freqs],
            erb_buf: vec![0.; n_ch * nb_erb],
            cplx_buf: vec![C32::new(0., 0.); n_ch * nb_df],
            roll_y,
            roll_x,
            skips: 0,
        };
        // Warm the tract plans on whatever thread built the runner (the
        // control plane in the daemon): the first `run` on a spawned
        // state compiles/allocates lazily — measured live as an ~81 ms
        // stall on the RT thread's first hop. Two silent hops force that
        // work here; the STFT/normalization state is then rebuilt so the
        // warm-up never leaks into audio (the session GRU just sees two
        // silent frames, far below any audible effect — locked smoke
        // bars re-verified after this change).
        let silence = vec![vec![0.0f32; hop]; n_ch];
        let mut sink = vec![vec![0.0f32; hop]; n_ch];
        for _ in 0..2 {
            let ins: Vec<&[f32]> = silence.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = sink.iter_mut().map(Vec::as_mut_slice).collect();
            let _ = runner.process_hop(&ins, &mut outs);
        }
        for d in runner.dsp.iter_mut() {
            *d = DfDsp::new(sr, fft_size, hop, nb_erb, min_nb, nb_df);
        }
        for b in runner.roll_x.iter_mut().chain(runner.roll_y.iter_mut()) {
            for s in b.iter_mut() {
                *s = C32::new(0., 0.);
            }
        }
        runner.skips = 0;
        Ok(runner)
    }

    pub fn hop(&self) -> usize {
        self.hop
    }

    pub fn lookahead(&self) -> usize {
        self.lookahead
    }

    fn t_erb(&self) -> TValue {
        let shape = [self.n_ch, 1, 1, self.nb_erb];
        let mut t = Tensor::zero::<f32>(&shape).unwrap();
        let mut view = t.view_mut();
        view.as_slice_mut::<f32>().unwrap().copy_from_slice(&self.erb_buf);
        t.into()
    }

    fn t_cplx(&self) -> TValue {
        // [n_ch, 2, 1, nb_df]: real/imag split along axis 1.
        let shape = [self.n_ch, 2, 1, self.nb_df];
        let mut t = Tensor::zero::<f32>(&shape).unwrap();
        let mut view = t.view_mut();
        let s = view.as_slice_mut::<f32>().unwrap();
        for ch in 0..self.n_ch {
            for f in 0..self.nb_df {
                let c = self.cplx_buf[ch * self.nb_df + f];
                s[ch * 2 * self.nb_df + f] = c.re;
                s[ch * 2 * self.nb_df + self.nb_df + f] = c.im;
            }
        }
        t.into()
    }

    /// One hop in (n_ch × hop samples), one hop out. Returns the local
    /// SNR estimate (diagnostic).
    pub fn process_hop(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) -> Result<f32, String> {
        // Silence fast-path (matches upstream skip logic).
        let mut max_a = 0f32;
        let mut e = 0f64;
        for ch in input {
            for s in ch.iter().take(self.hop) {
                max_a = max_a.max(s.abs());
                e += (*s as f64) * (*s as f64);
            }
        }
        let rms = e / (input.len().max(1) * self.hop) as f64;
        if rms < 1e-7 {
            self.skips += 1;
        } else {
            self.skips = 0;
        }
        if self.skips > 5 {
            for o in output.iter_mut() {
                for s in o.iter_mut().take(self.hop) {
                    *s = 0.0;
                }
            }
            return Ok(-15.);
        }
        let _ = max_a;
        // Analysis per channel.
        let mut spec: Vec<C32> = vec![C32::new(0., 0.); self.n_ch * self.n_freqs];
        for (ch_i, state) in self.dsp.iter_mut().enumerate() {
            let mut frame = vec![C32::new(0., 0.); self.n_freqs];
            state.analysis(&input[ch_i][..self.hop.min(input[ch_i].len())], &mut frame);
            spec[ch_i * self.n_freqs..(ch_i + 1) * self.n_freqs].copy_from_slice(&frame);
        }
        self.roll_y.pop_front();
        self.roll_x.pop_front();
        // The current noisy frame joins both rolls BEFORE enhancement
        // (upstream order): the mask later hits index df_order-1 and
        // the DF sees [..., current], so everything aligns on the
        // lookahead-delayed frame.
        self.roll_y.push_back(spec.clone());
        self.roll_x.push_back(spec.clone());
        // Features.
        for ch_i in 0..self.n_ch {
            let base = ch_i * self.n_freqs;
            let nsy = &spec[base..base + self.n_freqs];
            let eb = &mut self.erb_buf[ch_i * self.nb_erb..(ch_i + 1) * self.nb_erb];
            self.dsp[ch_i].feat_erb(nsy, self.alpha, eb);
            let cb = &mut self.cplx_buf[ch_i * self.nb_df..(ch_i + 1) * self.nb_df];
            let tmp: Vec<C32> = nsy[..self.nb_df].to_vec();
            self.dsp[ch_i].feat_cplx(&tmp, self.alpha, cb);
        }
        self.spec_buf.copy_from_slice(&spec);
        // Encoder.
        let mut enc_out = self
            .enc
            .run(tvec!(self.t_erb(), self.t_cplx()))
            .map_err(|e| format!("encoder: {e}"))?;
        let lsnr: f32 = {
            let t = enc_out.pop().ok_or("encoder: no lsnr")?.into_tensor();
            *t.view().as_slice::<f32>().map_err(|e| format!("lsnr: {e}"))?.first().ok_or("encoder: empty lsnr")?
        };
        let c0 = enc_out.pop().ok_or("encoder: no c0")?;
        let emb = enc_out.pop().ok_or("encoder: no emb")?;
        let e3 = enc_out.pop().ok_or("encoder: no e3")?;
        let e2 = enc_out.pop().ok_or("encoder: no e2")?;
        let e1 = enc_out.pop().ok_or("encoder: no e1")?;
        let e0 = enc_out.pop().ok_or("encoder: no e0")?;
        // Stage gates by local SNR (matches upstream thresholds).
        let apply_gains = lsnr >= self.min_thresh && lsnr <= self.erb_thresh;
        let apply_gain_zeros = lsnr < self.min_thresh;
        let apply_df = lsnr >= self.min_thresh && lsnr <= self.df_thresh;
        // Stage 1: ERB mask.
        let mut gains: Option<Vec<f32>> = None;
        if apply_gains {
            let mut m = self
                .erb_dec
                .run(tvec!(emb.clone(), e3, e2, e1, e0))
                .map_err(|e| format!("erb decoder: {e}"))?;
            let t = m.pop().ok_or("erb decoder: no mask")?.into_tensor();
            let arr = t.view().as_slice::<f32>().map_err(|e| format!("mask shape: {e}"))?;
            gains = Some(arr.to_vec());
        } else if apply_gain_zeros {
            gains = Some(vec![0.; self.n_ch * self.nb_erb]);
        }
        // Stage 2: deep-filter coefficients.
        let mut coefs: Option<Vec<f32>> = None;
        if apply_df {
            let mut c = self
                .df_dec
                .run(tvec!(emb, c0))
                .map_err(|e| format!("df decoder: {e}"))?;
            let t = c.pop().ok_or("df decoder: no coefs")?.into_tensor();
            let arr = t.view().as_slice::<f32>().map_err(|e| format!("coefs shape: {e}"))?;
            coefs = Some(arr.to_vec());
        }
        // Apply the ERB mask to the delayed spectrum.
        let tgt = self.df_order - 1;
        while self.roll_y.len() <= tgt {
            self.roll_y.push_back(vec![C32::new(0., 0.); self.n_ch * self.n_freqs]);
        }
        if let Some(g) = &gains {
            for ch_i in 0..self.n_ch {
                let base = ch_i * self.n_freqs;
                // Mask layout [n_ch, nb_erb] (or broadcast ch0).
                let gb = if g.len() >= (ch_i + 1) * self.nb_erb {
                    &g[ch_i * self.nb_erb..(ch_i + 1) * self.nb_erb]
                } else {
                    &g[0..self.nb_erb]
                };
                let slot = &mut self.roll_y[tgt];
                self.dsp[ch_i].apply_mask(&mut slot[base..base + self.n_freqs], gb);
            }
            self.skips = 0;
        } else {
            self.skips += 1;
        }
        // Deep filtering over the rolling noisy spectra. Coef order o
        // pairs oldest-first with the last df_order frames (the trained
        // convention: coefs index time-ascending frames).
        let mut enh = self.roll_y[tgt].clone();
        if let Some(c) = &coefs {
            // coefs [n_ch, nb_df, df_order, 2] real-interleaved.
            let base0 = self.roll_x.len().saturating_sub(self.df_order);
            for ch_i in 0..self.n_ch {
                for f in 0..self.nb_df {
                    let mut acc = C32::new(0., 0.);
                    for o in 0..self.df_order {
                        let idx = ((ch_i * self.nb_df + f) * self.df_order + o) * 2;
                        let cc = C32::new(c[idx], c[idx + 1]);
                        let frame = &self.roll_x[(base0 + o).min(self.roll_x.len() - 1)];
                        let s = frame[ch_i * self.n_freqs + f];
                        acc += s * cc;
                    }
                    enh[ch_i * self.n_freqs + f] = acc;
                }
            }
        }
        // Post-filter + attenuation limit against the delayed noisy ref.
        let ref_idx =
            self.roll_x.len().saturating_sub(1 + self.lookahead.min(self.roll_x.len().saturating_sub(1)));
        let noisy_ref = self.roll_x[ref_idx].clone();
        if apply_gains && self.postfilter_beta > 0. {
            post_filter(&noisy_ref, &mut enh, self.postfilter_beta);
        }
        if let Some(lim) = self.atten_lim {
            for (e, n) in enh.iter_mut().zip(noisy_ref.iter()) {
                *e = *e * (1. - lim) + *n * lim;
            }
        }
        // Synthesis per channel straight from the enhanced spectrum
        // (rolls already hold the current frame; lengths stay constant
        // by the pop/push balance above).
        for (ch_i, (state, o)) in self.dsp.iter_mut().zip(output.iter_mut()).enumerate() {
            let base = ch_i * self.n_freqs;
            let mut frame: Vec<C32> = enh[base..base + self.n_freqs].to_vec();
            let mut hop_out = vec![0.0f32; self.hop];
            state.synthesis(&mut frame, &mut hop_out);
            let n = self.hop.min(o.len());
            o[..n].copy_from_slice(&hop_out[..n]);
        }
        Ok(lsnr)
    }
}

// ---------------------------------------------------------------------------
// The `deepfilter` audio node: FIFO per channel + lazy runtime.
// ---------------------------------------------------------------------------

pub struct DeepFilter {
    model_path: String,
    atten_db: f32,
    erb_thresh: f32,
    df_thresh: f32,
    postfilter_beta: f32,
    mix: f32,
    runner: Option<DfRunner>,
    run_ch: usize,
    pending: Vec<VecDeque<f32>>,
    out: Vec<VecDeque<f32>>,
    dry: Vec<VecDeque<f32>>,
    last_lsnr: f32,
}

impl DeepFilter {
    pub(crate) fn new() -> Self {
        Self {
            model_path: String::new(),
            atten_db: 100.0,
            erb_thresh: 30.0,
            df_thresh: 20.0,
            postfilter_beta: 0.02,
            mix: 1.0,
            runner: None,
            run_ch: 0,
            pending: Vec::new(),
            out: Vec::new(),
            dry: Vec::new(),
            last_lsnr: 0.0,
        }
    }

    fn ensure_runner(&mut self, n_ch: usize) -> Result<(), String> {
        let stale = match &self.runner {
            Some(r) => r.n_ch != n_ch,
            None => true,
        };
        if stale {
            if self.model_path.is_empty() {
                return Err("deepfilter: no model loaded (set model_path)".into());
            }
            let bundle = load_bundle(&self.model_path)?;
            let runner = DfRunner::new_bundled(
                &bundle,
                n_ch,
                self.atten_db,
                self.erb_thresh,
                self.df_thresh,
                self.postfilter_beta,
            )?;
            let ahead = runner.lookahead() * runner.hop();
            self.runner = Some(runner);
            self.run_ch = n_ch;
            self.pending = vec![VecDeque::new(); n_ch];
            self.out = vec![VecDeque::new(); n_ch];
            // Dry delay matches the enhancement lookahead for mix blends.
            self.dry = vec![VecDeque::from(vec![0.0; ahead]); n_ch];
        }
        Ok(())
    }
}

impl AudioNode for DeepFilter {
    fn name(&self) -> &'static str {
        "deepfilter"
    }
    fn process(&mut self, ctx: &ProcessContext, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let _ = ctx.rate; // DF3 bundles are natively 48 kHz (checked at load).
        if self.model_path.is_empty() {
            // Unloaded: honest passthrough (a preset should always set
            // model_path; missing files fail loud at selectVoice time).
            for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
                let n = i.len().min(o.len());
                o[..n].copy_from_slice(&i[..n]);
            }
            return;
        }
        if self.ensure_runner(inputs.len()).is_err() {
            // Load failure mid-stream (deleted file?): pass through
            // rather than silencing the mic. The error already fired
            // at set_param/selectVoice time.
            for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
                let n = i.len().min(o.len());
                o[..n].copy_from_slice(&i[..n]);
            }
            return;
        }
        let runner = self.runner.as_mut().unwrap();
        let hop = runner.hop();
        let mix = self.mix;
        // Stage input + dry delay per lane.
        for (input, (ch_p, ch_d)) in
            inputs.iter().zip(self.pending.iter_mut().zip(self.dry.iter_mut()))
        {
            ch_p.extend(input.iter());
            ch_d.extend(input.iter());
        }
        // Batched inference: while every lane holds a full hop, run one
        // hop across all lanes through the shared runner (tract sessions
        // are n_ch-wide by construction).
        loop {
            if self.pending.iter().any(|p| p.len() < hop) {
                break;
            }
            let ib: Vec<Vec<f32>> = self
                .pending
                .iter_mut()
                .map(|p| p.drain(..hop).collect())
                .collect();
            let mut ob: Vec<Vec<f32>> = (0..ib.len()).map(|_| vec![0.0f32; hop]).collect();
            let ins: Vec<&[f32]> = ib.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [f32]> = ob.iter_mut().map(Vec::as_mut_slice).collect();
            match self.runner.as_mut().unwrap().process_hop(&ins, &mut outs) {
                Ok(lsnr) => self.last_lsnr = lsnr,
                Err(e) => {
                    eprintln!("deepfilter inference failed ({e}); passing through");
                    break;
                }
            }
            for (lane_out, block) in self.out.iter_mut().zip(ob.iter()) {
                lane_out.extend(block.iter());
            }
        }
        // Emit exactly input.len() (dry delayed to match wet).
        for (output, (ch_o, ch_d)) in
            outputs.iter_mut().zip(self.out.iter_mut().zip(self.dry.iter_mut()))
        {
            for o in output.iter_mut() {
                let w = ch_o.pop_front().unwrap_or(0.0);
                let d = ch_d.pop_front().unwrap_or(0.0);
                *o = d * (1.0 - mix) + w * mix;
            }
        }
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<(), ParamError> {
        match name {
            "model_path" => {
                let p = match value {
                    ParamValue::Text(s) => s,
                    _ => return Err(ParamError::TypeMismatch { name: name.into() }),
                };
                // Validate eagerly (control plane): missing/corrupt
                // models fail at selectVoice, never silently at runtime.
                let bundle = load_bundle(&p).map_err(ParamError::InvalidValue)?;
                // The real lane count is only known once the graph
                // builder calls `prepare` (fresh select) — building here
                // for a guessed count forces a full model rebuild
                // (parse + plan warm-up, tens of ms) on the first RT
                // hop. A live swap on a running node rebuilds at once,
                // still on the control plane.
                let rebuilt = if self.run_ch > 0 {
                    Some(
                        DfRunner::new_bundled(
                            &bundle,
                            self.run_ch,
                            self.atten_db,
                            self.erb_thresh,
                            self.df_thresh,
                            self.postfilter_beta,
                        )
                        .map_err(ParamError::InvalidValue)?,
                    )
                } else {
                    None
                };
                self.model_path = p;
                self.runner = rebuilt;
                if let Some(r) = self.runner.as_ref() {
                    let ahead = r.lookahead() * r.hop();
                    self.pending = vec![VecDeque::new(); self.run_ch];
                    self.out = vec![VecDeque::new(); self.run_ch];
                    self.dry = vec![VecDeque::from(vec![0.0; ahead]); self.run_ch];
                }
                Ok(())
            }
            "atten_db" => {
                let v = value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                self.atten_db = RangedParam { min: 0.0, max: 100.0, default: 100.0 }.clamp(v);
                // Live update, no session rebuild (LSTM state survives).
                if let Some(r) = self.runner.as_mut() {
                    r.atten_lim = atten_lim_of(self.atten_db);
                }
                Ok(())
            }
            "erb_thresh" => {
                let v = value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                self.erb_thresh = RangedParam { min: -15.0, max: 35.0, default: 30.0 }.clamp(v);
                if let Some(r) = self.runner.as_mut() {
                    r.erb_thresh = self.erb_thresh;
                }
                Ok(())
            }
            "df_thresh" => {
                let v = value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                self.df_thresh = RangedParam { min: -15.0, max: 35.0, default: 20.0 }.clamp(v);
                if let Some(r) = self.runner.as_mut() {
                    r.df_thresh = self.df_thresh;
                }
                Ok(())
            }
            "postfilter_beta" => {
                let v = value.as_float().ok_or_else(|| ParamError::TypeMismatch { name: name.into() })?;
                self.postfilter_beta =
                    RangedParam { min: 0.0, max: 1.0, default: 0.02 }.clamp(v);
                if let Some(r) = self.runner.as_mut() {
                    r.postfilter_beta = self.postfilter_beta;
                }
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
    /// The graph builder reports the real lane count here (control
    /// plane): parse + optimize + warm the tract sessions now so the
    /// first RT hop is warm and correctly shaped. Without a model this
    /// node is a documented pass-through.
    fn prepare(&mut self, channels: usize) -> Result<(), ParamError> {
        if channels == 0 || self.model_path.is_empty() {
            return Ok(());
        }
        self.ensure_runner(channels).map_err(ParamError::InvalidValue)
    }
    /// Enhancement lookahead in samples (0 until a model loads).
    fn latency_samples(&self, _rate: u32) -> usize {
        self.runner.as_ref().map(|r| r.lookahead() * r.hop()).unwrap_or(0)
    }
    /// Live local-SNR estimate in dB (voice-activity meter for UIs).
    fn get_param(&self, name: &str) -> Option<ParamValue> {
        match name {
            "lsnr" => Some(ParamValue::Float(self.last_lsnr)),
            _ => None,
        }
    }
    fn params(&self) -> Vec<PublicParam> {
        vec![
            PublicParam {
                name: "model_path",
                value: ParamValue::Text(self.model_path.clone()),
                range: None,
                public: true,
            },
            PublicParam {
                name: "atten_db",
                value: ParamValue::Float(self.atten_db),
                range: Some(RangedParam { min: 0.0, max: 100.0, default: 100.0 }),
                public: true,
            },
            PublicParam {
                name: "erb_thresh",
                value: ParamValue::Float(self.erb_thresh),
                range: Some(RangedParam { min: -15.0, max: 35.0, default: 30.0 }),
                public: true,
            },
            PublicParam {
                name: "df_thresh",
                value: ParamValue::Float(self.df_thresh),
                range: Some(RangedParam { min: -15.0, max: 35.0, default: 20.0 }),
                public: true,
            },
            PublicParam {
                name: "postfilter_beta",
                value: ParamValue::Float(self.postfilter_beta),
                range: Some(RangedParam { min: 0.0, max: 1.0, default: 0.02 }),
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
mod deepfilter_tests {
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
    fn unloaded_passthrough_and_loud_errors() {
        // Mask-ones roundtrip WITHOUT any model: analysis → unity mask
        // → synthesis must reproduce the input delayed by the pipeline.
        // (Isolates our DSP from the neural stages entirely.)
        {
            let mut dsp = DfDsp::new(48000, 960, 480, 32, 2, 96);
            let input = noise(9600, 0.2);
            let mut out = vec![0.0f32; input.len()];
            for (ib, ob) in input
                .as_chunks::<480>()
                .0
                .iter()
                .zip(out.as_chunks_mut::<480>().0.iter_mut())
            {
                let mut spec = vec![C32::new(0., 0.); 481];
                dsp.analysis(ib, &mut spec);
                let ones = vec![1.0f32; 32];
                dsp.apply_mask(&mut spec, &ones);
                let mut hop = vec![0.0f32; 480];
                dsp.synthesis(&mut spec, &mut hop);
                ob.copy_from_slice(&hop);
            }
            let tail_in = rms(&input[4800..8600]);
            let tail_out = rms(&out[4800..8600]);
            assert!(
                (tail_out - tail_in).abs() < tail_in * 0.3,
                "unity mask must preserve the signal (in {tail_in:.4}, out {tail_out:.4})"
            );
        }
        // Differential synthesis: identical spectra through both
        // round-trips must match sample-near.
        {
            let mut mine = DfDsp::new(48000, 960, 480, 32, 2, 96);
            let mut refs = df::DFState::new(48000, 960, 480, 32, 2);
            // A speech-like spectrum: strong low bins, weak highs.
            let base: Vec<C32> =
                (0..481).map(|i| C32::new(0.5 / (1.0 + i as f32 / 8.0), 0.01)).collect();
            let mut worst = 0.0f32;
            for _ in 0..6 {
                let mut a = base.clone();
                let mut b: Vec<df::Complex32> =
                    base.iter().map(|c| df::Complex32::new(c.re, c.im)).collect();
                let mut oa = vec![0.0f32; 480];
                let mut ob = vec![0.0f32; 480];
                mine.synthesis(&mut a, &mut oa);
                refs.synthesis(&mut b, &mut ob);
                for (x, y) in oa.iter().zip(ob.iter()) {
                    worst = worst.max((x - y).abs());
                }
            }
            assert!(worst < 1e-2, "synthesis must match upstream, diff {worst:.2e}");
        }
        // Differential: our STFT must match the published primitives
        // (same window/norm/mem discipline) or every feature downstream
        // shifts and the model answers weakly.
        let input = noise(4800, 0.2);
        let mut mine = DfDsp::new(48000, 960, 480, 32, 2, 96);
        let mut refs = df::DFState::new(48000, 960, 480, 32, 2);
        let mut worst = 0.0f32;
        for chunk in input.as_chunks::<480>().0 {
            let mut a = vec![C32::new(0., 0.); 481];
            let mut b = vec![df::Complex32::new(0., 0.); 481];
            mine.analysis(chunk, &mut a);
            refs.analysis(chunk, &mut b);
            for (x, y) in a.iter().zip(b.iter()) {
                worst = worst.max((x.re - y.re).abs()).max((x.im - y.im).abs());
            }
        }
        assert!(worst < 1e-3, "STFT must match upstream primitives, diff {worst:.2e}");
        let mut n = DeepFilter::new();
        assert_eq!(n.latency_samples(RATE), 0);
        // No model: honest passthrough.
        let input = noise(9_600, 0.2);
        let out = run(&mut n, &input, 960);
        for (a, b) in out.iter().zip(input.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
        // Missing dir / file / garbage all fail loud with guidance.
        assert!(n.set_param("model_path", ParamValue::Text("/nonexistent/dir".into())).is_err());
        let bad = std::env::temp_dir().join(format!("vm-df-bad-{}", std::process::id()));
        std::fs::create_dir_all(&bad).unwrap();
        assert!(n.set_param("model_path", ParamValue::Text(bad.to_string_lossy().into_owned())).is_err());
        let _ = std::fs::remove_dir_all(&bad);
        assert!(matches!(
            n.set_param("model_path", ParamValue::Float(1.0)),
            Err(ParamError::TypeMismatch { .. })
        ));
        assert!(matches!(
            n.set_param("nope", ParamValue::Float(1.0)),
            Err(ParamError::Unknown(_))
        ));
        // Thresholds clamp; lsnr reads back (0 with no model).
        n.set_param("atten_db", ParamValue::Float(999.0)).unwrap();
        match n.get_param("lsnr") {
            Some(ParamValue::Float(_)) => {}
            other => panic!("want lsnr float, got {other:?}"),
        }
        assert!(n.get_param("nope").is_none());
    }

    #[test]
    fn erb_stage_alone_passes_speech() {
        // Isolate stage 1: `df_thresh` below the lsnr floor makes the
        // DF-decoder window [min_thresh, df_thresh] empty, so only the
        // ERB mask shapes the output. Measured behavior (DF3 on the
        // pink-warmed mix below): ratio ≈ 0.42 of the input — audible
        // (the ERB mask is broadband suppression, not a mute) and yet
        // suppressed. A fresh node classifies the stationary mix as
        // noise (lsnr < -10 → gains zeroed by design), which is why the
        // pink warm-up (the node's normal operating regime) is required
        // for lsnr to stay in the ERB window at all.
        let Some(dir) = model_dir() else {
            eprintln!("skipping erb-only test (no DF3 bundle; set VM_DF_MODEL)");
            return;
        };
        let mut n = DeepFilter::new();
        n.set_param("model_path", ParamValue::Text(dir.clone())).unwrap();
        n.set_param("df_thresh", ParamValue::Float(-15.0)).unwrap();
        // Warm up on stationary room noise, exactly like real use (the
        // daemon never starts a mic inside a fully synthetic silence).
        let mut lp = 0.0f32;
        let pink: Vec<f32> = noise(144_000, 0.06)
            .into_iter()
            .map(|w| {
                lp += 0.08 * (w - lp);
                lp
            })
            .collect();
        let _ = run(&mut n, &pink, 960);
        let nz = noise(48_000, 0.08);
        let tone: Vec<f32> = (0..48_000)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                0.22 * (2.0 * std::f32::consts::PI * 150.0 * t).sin()
                    + 0.12 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
            })
            .collect();
        let mix: Vec<f32> = tone.iter().zip(nz.iter()).map(|(s, n)| s + n).collect();
        let out = run(&mut n, &mix, 960);
        assert!(out.iter().all(|s| s.is_finite()));
        let lsnr = match n.get_param("lsnr") {
            Some(ParamValue::Float(v)) => v,
            _ => f32::NAN,
        };
        let in_r = rms(&mix[24_000..]);
        let out_r = rms(&out[24_000..]);
        assert!(
            lsnr > -10.0,
            "warm-up must park lsnr in the ERB window (got {lsnr:.1})"
        );
        assert!(
            out_r > in_r * 0.1,
            "ERB-only path must stay audible: in {in_r:.4}, out {out_r:.4}"
        );
        assert!(
            out_r < in_r * 0.8,
            "ERB mask must still suppress: in {in_r:.4}, out {out_r:.4}"
        );
    }

    /// Model dir for the live smoke: `VM_DF_MODEL` or the dev default.
    /// Skips gracefully when no bundle is present (models are ~10 MB
    /// downloads, never vendored).
    fn model_dir() -> Option<String> {
        let mut cands = Vec::new();
        if let Ok(v) = std::env::var("VM_DF_MODEL") {
            cands.push(v);
        }
        cands.push("/tmp/opencode/df3".to_string());
        cands.into_iter().find(|cand| {
            let p = std::path::Path::new(cand);
            p.join("config.ini").exists() && p.join("enc.onnx").exists()
        })
    }

    #[test]
    fn deepfilter_smoke_with_real_model() {
        let Some(dir) = model_dir() else {
            eprintln!("skipping deepfilter smoke (no DF3 bundle; set VM_DF_MODEL)");
            return;
        };
        let mut n = DeepFilter::new();
        n.set_param("model_path", ParamValue::Text(dir)).expect("model must load");
        n.prepare(1).expect("prepare must build the sessions");
        assert_eq!(n.latency_samples(RATE), 2 * 480, "DF3 lookahead is 2 hops");
        // Stationary room noise: must come out much quieter.
        let mut lp = 0.0f32;
        let pink: Vec<f32> = noise(144_000, 0.06)
            .into_iter()
            .map(|w| {
                lp += 0.08 * (w - lp);
                lp
            })
            .collect();
        let out = run(&mut n, &pink, 960);
        assert!(out.iter().all(|s| s.is_finite()), "DF output must be finite");
        let in_r = rms(&pink[96_000..]);
        let out_r = rms(&out[96_000..]);
        assert!(out_r < in_r * 0.3, "DF3 must suppress room noise strongly");
        // Speech-like tone buried in noise: tone survives.
        let nz = noise(96_000, 0.08);
        let tone: Vec<f32> = (0..96_000)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                0.22 * (2.0 * std::f32::consts::PI * 150.0 * t).sin()
                    + 0.12 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
            })
            .collect();
        let mix: Vec<f32> = tone.iter().zip(nz.iter()).map(|(s, n)| s + n).collect();
        let out2 = run(&mut n, &mix, 960);
        let tone_r = rms(&tone[48_000..]);
        let out2_r = rms(&out2[48_000..]);
        assert!(out2_r > tone_r * 0.7, "speech must survive DeepFilterNet");
    }
}
