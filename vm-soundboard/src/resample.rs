//! Transparent resampling to the 48 kHz engine rate (rubato, FFT).
//!
//! Whole-file offline conversion at load time: any input rate becomes
//! 48 kHz with no pitch change. Phase-linear, delay-trimmed output.

use super::SoundboardError;
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};

/// Resample planar lanes to 48 kHz. Passthrough when already 48 kHz.
pub fn to_48000(rate: u32, channels: Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>, SoundboardError> {
    if rate == 0 {
        return Err(SoundboardError::Decode("zero sample rate".into()));
    }
    if rate == 48_000 {
        return Ok(channels);
    }
    if channels.is_empty() || channels[0].is_empty() {
        return Err(SoundboardError::Decode("no audio to resample".into()));
    }
    let nch = channels.len();
    let frames = channels[0].len();
    let mut resampler =
        Fft::<f32>::new(rate as usize, 48_000, 4096, nch, FixedSync::Input)
            .map_err(|e| SoundboardError::Decode(format!("resampler setup: {e:?}")))?;
    let adapter = SequentialSliceOfVecs::new(&channels, nch, frames)
        .map_err(|e| SoundboardError::Decode(format!("resampler input: {e:?}")))?;
    let out = resampler
        .process_all(&adapter, frames, None)
        .map_err(|e| SoundboardError::Decode(format!("resample: {e:?}")))?;
    let flat = out.take_data();
    let nout = flat.len() / nch;
    let mut planar = vec![Vec::with_capacity(nout); nch];
    for (i, s) in flat.into_iter().enumerate() {
        planar[i % nch].push(s);
    }
    Ok(planar)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn zero_cross_freq(x: &[f32], rate: u32) -> f32 {
        let mut crossings = 0;
        for w in x.windows(2) {
            if (w[0] <= 0.0) != (w[1] <= 0.0) {
                crossings += 1;
            }
        }
        crossings as f32 * rate as f32 / (2.0 * x.len() as f32)
    }

    #[test]
    fn resample_44100_to_48000_preserves_pitch() {
        let input = sine(440.0, 44_100, 44_100); // 1 s
        let out = to_48000(44_100, vec![input]).unwrap();
        assert_eq!(out.len(), 1);
        // Steady state: skip filter startup + end effects.
        let steady = &out[0][4800..out[0].len() - 4800];
        let f = zero_cross_freq(steady, 48_000);
        assert!((f - 440.0).abs() < 440.0 * 0.01, "got {f:.1} Hz");
        assert!((out[0].len() as f32 - 48_000.0).abs() < 64.0);
    }

    #[test]
    fn passthrough_48000() {
        let input = vec![vec![0.25f32; 100]];
        let out = to_48000(48_000, input.clone()).unwrap();
        assert_eq!(out, input);
    }
}
