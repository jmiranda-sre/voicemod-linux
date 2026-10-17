//! First-run setup wizard (F-0.4.B item 29): guided mic test + calibration.
//!
//! UI-side state machine — no daemon changes. It samples `getLevels`
//! readings (the caller feeds them every frame) and computes suggestions
//! that the UI applies through the ordinary `setVoiceParameter` /
//! `captureNoiseProfile` actions. A `wizard.json` marker in the config
//! dir records completion so it only auto-opens once.

use std::path::Path;

pub const WIZARD_FILE: &str = "wizard.json";
/// Seconds of audio observed per measurement step.
pub const MEASURE_SECS: f32 = 3.0;
/// Target speech level (dBFS) the speech step aims at.
pub const TARGET_SPEECH_DB: f32 = -18.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    MicCheck,
    NoiseFloor,
    Gate,
    SpeechLevel,
    Limiter,
}

pub struct Wizard {
    pub open: bool,
    pub step: Step,
    floor_samples: Vec<f32>,
    speech_samples: Vec<f32>,
    elapsed: f32,
    last_mic: f32,
    /// Measured room noise floor (dBFS).
    pub floor_db: f32,
    /// Measured speech level (dBFS).
    pub speech_db: f32,
    /// Suggested gate threshold (dBFS).
    pub gate_db: f32,
    /// Human-readable result of the last applied suggestion.
    pub note: String,
}

impl Default for Wizard {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Wizard {
    /// `force_open` mirrors first-run detection (marker file).
    pub fn new(force_open: bool) -> Self {
        Self {
            open: force_open,
            step: Step::Welcome,
            floor_samples: Vec::new(),
            speech_samples: Vec::new(),
            elapsed: 0.0,
            last_mic: f32::NAN,
            floor_db: -60.0,
            speech_db: -30.0,
            gate_db: -54.0,
            note: String::new(),
        }
    }

    /// First-run wizard: auto-open unless `wizard.json` marks completion.
    pub fn first_run(config_dir: &Path) -> Self {
        Self::new(!config_dir.join(WIZARD_FILE).exists())
    }

    /// Advance the current measurement step; sampling is wall-time driven
    /// (progress) but only distinct level updates are recorded (the poll
    /// is coarse, repeating the same reading would skew the median).
    pub fn feed(&mut self, mic_db: f32, dt: f32) {
        self.elapsed += dt;
        let changed = self.last_mic.is_nan() || (mic_db - self.last_mic).abs() > 0.05;
        self.last_mic = mic_db;
        if !changed {
            return;
        }
        match self.step {
            Step::NoiseFloor => self.floor_samples.push(mic_db),
            Step::SpeechLevel => self.speech_samples.push(mic_db),
            _ => {}
        }
    }

    /// Switch step, resetting the measurement clock + sampler.
    pub fn begin_step(&mut self, step: Step) {
        self.step = step;
        self.elapsed = 0.0;
        self.last_mic = f32::NAN;
        match step {
            Step::NoiseFloor => self.floor_samples.clear(),
            Step::SpeechLevel => self.speech_samples.clear(),
            _ => {}
        }
    }

    pub fn progress(&self) -> f32 {
        (self.elapsed / MEASURE_SECS).clamp(0.0, 1.0)
    }

    pub fn measurement_done(&self) -> bool {
        self.elapsed >= MEASURE_SECS
    }

    /// Close the noise-floor measurement: median floor, gate suggestion.
    /// Returns the suggested threshold (clamped to the preset's range).
    pub fn finish_noise(&mut self, range: Option<(f32, f32)>) -> f32 {
        self.floor_db = median(&self.floor_samples).unwrap_or(-60.0);
        self.gate_db = suggest_gate(self.floor_db, range);
        self.gate_db
    }

    /// Close the speech measurement: median speech level.
    pub fn finish_speech(&mut self) -> f32 {
        self.speech_db = median(&self.speech_samples).unwrap_or(-30.0);
        self.speech_db
    }

    /// Write the completion marker (idempotent).
    pub fn mark_done(&self, config_dir: &Path) {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let v = serde_json::json!({
            "completed": true,
            "completed_unix": unix,
            "floor_db": self.floor_db,
            "gate_db": self.gate_db,
            "speech_db": self.speech_db,
        });
        let _ = std::fs::write(config_dir.join(WIZARD_FILE), v.to_string());
    }
}

/// Median of the samples (even counts average the two middles).
pub fn median(samples: &[f32]) -> Option<f32> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// Gate suggestion: 6 dB above the measured floor, clamped to the gate's
/// parameter range when known.
pub fn suggest_gate(floor_db: f32, range: Option<(f32, f32)>) -> f32 {
    let (lo, hi) = range.unwrap_or((-70.0, -20.0));
    (floor_db + 6.0).clamp(lo, hi)
}

/// Makeup-gain suggestion: bring speech to `target_db` (±), clamped.
pub fn suggest_makeup(speech_db: f32, range: Option<(f32, f32)>) -> f32 {
    let delta = TARGET_SPEECH_DB - speech_db;
    let (lo, hi) = range.unwrap_or((-12.0, 12.0));
    delta.clamp(lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_opens_only_without_marker() {
        let dir = std::env::temp_dir().join(format!("vm-wiz-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let w = Wizard::first_run(&dir);
        assert!(w.open, "first run must auto-open");
        let mut w = Wizard::first_run(&dir);
        w.floor_db = -61.5;
        w.mark_done(&dir);
        let again = Wizard::first_run(&dir);
        assert!(!again.open, "completion marker suppresses auto-open");
        let text = std::fs::read_to_string(dir.join(WIZARD_FILE)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["completed"], true);
        assert_eq!(v["floor_db"], -61.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn feed_samples_distinct_readings_only() {
        let mut w = Wizard::default();
        w.begin_step(Step::NoiseFloor);
        w.feed(-70.0, 0.1);
        w.feed(-70.0, 0.1); // repeat: not sampled
        w.feed(-69.9, 0.1);
        assert_eq!(w.floor_samples.len(), 2);
        assert!((w.progress() - 0.1).abs() < 1e-3);
        assert!(!w.measurement_done());
        w.feed(-69.8, MEASURE_SECS);
        assert!(w.measurement_done());
    }

    #[test]
    fn median_odd_even_and_empty() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn gate_and_makeup_suggestions_clamp() {
        // 6 dB over the floor...
        assert!((suggest_gate(-60.0, None) + 54.0).abs() < 1e-6);
        // ...clamped to the preset range.
        assert!((suggest_gate(-20.0, Some((-70.0, -30.0))) + 30.0).abs() < 1e-6);
        assert!((suggest_gate(-80.0, Some((-70.0, -30.0))) + 70.0).abs() < 1e-6);
        // Makeup brings speech to the target, clamped to ±12 by default.
        assert!((suggest_makeup(-30.0, None) - 12.0).abs() < 1e-6);
        assert!((suggest_makeup(-20.0, None) - 2.0).abs() < 1e-6);
        assert!((suggest_makeup(-17.0, Some((0.0, 24.0)))).abs() < 1e-6);
    }

    #[test]
    fn finish_noise_uses_median_and_moves_on() {
        let mut w = Wizard::default();
        w.begin_step(Step::NoiseFloor);
        for db in [-66.0, -64.0, -70.0, -65.0] {
            w.feed(db, 0.05);
        }
        let g = w.finish_noise(None);
        assert!((w.floor_db + 65.5).abs() < 1e-6, "median of samples");
        assert!((g + 59.5).abs() < 1e-6, "floor + 6");
        w.begin_step(Step::SpeechLevel);
        assert_eq!(w.step, Step::SpeechLevel);
        assert!(!w.measurement_done(), "clock reset on step change");
    }
}
