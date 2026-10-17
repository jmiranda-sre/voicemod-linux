//! Instant Replay (F-0.4.B item 20): a fixed-capacity ring of the final
//! vm-mic output, snapshotted to a WAV file on demand.
//!
//! The RT processor pushes one stereo quantum per callback inside a
//! `try_lock`; a contended lock just skips that quantum (a hole of ≤10 ms
//! in the history, never a stall). Saving copies the ring under the lock
//! and writes the file outside it.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Folder (under the config dir) where saved replays land.
pub const REPLAY_DIR: &str = "replays";
/// Replay files are always stereo (the vm-mic contract).
pub const WAV_CHANNELS: u16 = 2;

/// Fixed-capacity stereo ring of the final vm-mic output.
pub struct ReplayRing {
    /// Capacity in frames.
    capacity: usize,
    /// Interleaved stereo samples (`capacity * 2`).
    buf: Vec<f32>,
    /// Next frame slot to overwrite.
    write: usize,
    /// Valid frames (`<= capacity`).
    filled: usize,
    enabled: bool,
}

impl ReplayRing {
    /// `seconds` clamps to at least 1; `rate` is frames/second.
    pub fn new(seconds: u32, rate: u32) -> Self {
        let capacity = Self::frames_for(seconds, rate);
        Self {
            capacity,
            buf: vec![0.0; capacity * 2],
            write: 0,
            filled: 0,
            enabled: false,
        }
    }

    fn frames_for(seconds: u32, rate: u32) -> usize {
        seconds.max(1) as usize * rate.max(1) as usize
    }

    /// Resize the window for a new `seconds`/`rate`. A capacity change
    /// resets the history (the old buffer can't be re-cut meaningfully);
    /// an unchanged capacity keeps it.
    pub fn configure(&mut self, seconds: u32, rate: u32) {
        let capacity = Self::frames_for(seconds, rate);
        if capacity != self.capacity {
            self.capacity = capacity;
            self.buf = vec![0.0; capacity * 2];
            self.clear();
        }
    }

    /// Toggling off drops the history so a re-enable never replays stale
    /// audio.
    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled && !on {
            self.clear();
        }
        self.enabled = on;
    }

    /// Valid frames currently buffered (≤ capacity).
    pub fn filled_frames(&self) -> usize {
        self.filled
    }

    pub fn clear(&mut self) {
        self.write = 0;
        self.filled = 0;
    }

    /// RT-safe append of one quantum: writes the last `min(len)` frames.
    /// No-op while disabled.
    pub fn push_stereo(&mut self, left: &[f32], right: &[f32]) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let p = self.write;
            self.buf[p * 2] = left[i];
            self.buf[p * 2 + 1] = right[i];
            self.write = if p + 1 == self.capacity { 0 } else { p + 1 };
        }
        self.filled = (self.filled + n).min(self.capacity);
    }

    /// Chronological copy (oldest → newest), interleaved stereo.
    pub fn snapshot(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.filled * 2);
        let start = if self.filled < self.capacity {
            0
        } else {
            self.write
        };
        for i in 0..self.filled {
            let p = if start + i >= self.capacity {
                start + i - self.capacity
            } else {
                start + i
            };
            out.push(self.buf[p * 2]);
            out.push(self.buf[p * 2 + 1]);
        }
        out
    }
}

/// `<config_dir>/replays/replay-<unix>.wav`.
pub fn replay_path(config_dir: &Path, unix: u64) -> PathBuf {
    config_dir.join(REPLAY_DIR).join(format!("replay-{unix}.wav"))
}

/// Minimal RIFF/WAVE PCM16 writer (the format is a 44-byte header plus
/// little-endian samples; no deps needed).
pub fn write_wav_pcm16(
    path: &Path,
    samples: &[f32],
    rate: u32,
    channels: u16,
) -> std::io::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes()); // byte rate
    bytes.extend_from_slice(&(channels * 2).to_le_bytes()); // block align
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(&bytes)?;
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize) -> (Vec<f32>, Vec<f32>) {
        let l: Vec<f32> = (0..n).map(|i| i as f32 * 0.001).collect();
        let r: Vec<f32> = (0..n).map(|i| 0.5 + i as f32 * 0.001).collect();
        (l, r)
    }

    #[test]
    fn ring_wraps_and_keeps_chronological_order() {
        let mut ring = ReplayRing::new(1, 100); // 100 frames
        ring.set_enabled(true);
        // 150 frames: frames 50..150 must survive, in order.
        let (l, r) = ramp(150);
        ring.push_stereo(&l, &r);
        assert_eq!(ring.filled_frames(), 100);
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 200);
        assert!((snap[0] - l[50]).abs() < 1e-6);
        assert!((snap[1] - r[50]).abs() < 1e-6);
        assert!((snap[198] - l[149]).abs() < 1e-6);
        assert!((snap[199] - r[149]).abs() < 1e-6);
    }

    #[test]
    fn disabled_ring_ignores_pushes_and_disable_clears() {
        let mut ring = ReplayRing::new(1, 100);
        let (l, r) = ramp(10);
        ring.push_stereo(&l, &r);
        assert_eq!(ring.filled_frames(), 0);
        ring.set_enabled(true);
        ring.push_stereo(&l, &r);
        assert_eq!(ring.filled_frames(), 10);
        ring.set_enabled(false);
        assert_eq!(ring.filled_frames(), 0);
        assert!(ring.snapshot().is_empty());
    }

    #[test]
    fn configure_resizes_and_resets_only_on_change() {
        let mut ring = ReplayRing::new(1, 100);
        ring.set_enabled(true);
        let (l, r) = ramp(50);
        ring.push_stereo(&l, &r);
        ring.configure(1, 100); // same window: history kept
        assert_eq!(ring.filled_frames(), 50);
        ring.configure(2, 100); // new window: reset
        assert_eq!(ring.filled_frames(), 0);
        ring.push_stereo(&l, &r);
        assert_eq!(ring.snapshot().len(), 100);
    }

    #[test]
    fn wav_round_trips_header_and_samples() {
        let dir = std::env::temp_dir().join(format!("vm-replay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        let samples = [0.0f32, 0.5, -0.5, 1.0, -1.0, 2.0];
        write_wav_pcm16(&path, &samples, 48_000, 2).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len() - 8);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2); // channels
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
        assert_eq!(&bytes[36..40], b"data");
        let data = &bytes[44..];
        assert_eq!(data.len(), samples.len() * 2);
        let got: Vec<i16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|w| i16::from_le_bytes(*w))
            .collect();
        // 32767 scaling with clamp at ±1.0.
        assert_eq!(got, [0, 16384, -16384, 32767, -32767, 32767]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_path_lands_under_replay_dir() {
        let p = replay_path(Path::new("/cfg"), 1700);
        assert!(p.ends_with("replays/replay-1700.wav"));
    }
}
