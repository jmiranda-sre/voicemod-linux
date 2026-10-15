//! `vm-soundboard`: sample playback pipeline (clean-room).
//!
//! Mirrors the ledger (Fase 2, item ③): per-sound options
//! (loop / stop-others / mute-others / mute-voice / per-mille volume /
//! `sendToVAD` routing / fades), hash-id addressing, master volume,
//! `stopAllSounds`. Codecs: WAV / FLAC / MP3 / Vorbis via Symphonia,
//! Opus via libopus (`audiopus`); Ogg-Opus demux lands in Fase 3b.

use std::collections::{HashMap, HashSet};
use std::path::Path;

pub mod decode;
pub mod mix;
pub mod ogg;
pub mod opus;
pub mod resample;

pub use mix::{SoundboardMixer, StereoBus};

/// Sound identifier: content hash (mirrors `playSoundByHashId`).
/// Well-known sounds can also be addressed by stable name hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SoundHashId(pub u64);

impl SoundHashId {
    /// Deterministic FNV-1a 64 name hash (stable across runs/platforms).
    pub fn from_name(name: &str) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in name.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        Self(h)
    }
}

/// Where a playing sound is routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputRoute {
    /// To the virtual microphone only.
    Vad,
    /// To the local monitor only.
    Monitor,
    /// To both (voice + soundboard mix).
    Both,
}

/// Per-sound play options. Field-for-field equivalent of the reference
/// play-sound struct, clean-room re-expressed.
#[derive(Debug, Clone)]
pub struct SoundOptions {
    pub loop_: bool,
    pub stop_other_sounds: bool,
    pub mute_other_sounds: bool,
    pub mute_voice: bool,
    /// Per-mille volume (0–1000), same unit as the reference.
    pub volume_per_mille: u16,
    pub send_to_vad: bool,
    pub fade_in_secs: f32,
    pub fade_out_secs: f32,
}

impl Default for SoundOptions {
    fn default() -> Self {
        Self {
            loop_: false,
            stop_other_sounds: false,
            mute_other_sounds: false,
            mute_voice: false,
            volume_per_mille: 1000,
            send_to_vad: true,
            fade_in_secs: 0.0,
            fade_out_secs: 0.0,
        }
    }
}

impl SoundOptions {
    pub fn route(&self) -> OutputRoute {
        if self.send_to_vad {
            OutputRoute::Both
        } else {
            OutputRoute::Monitor
        }
    }

    pub fn gain(&self, master_per_mille: u16) -> f32 {
        (self.volume_per_mille as f32 / 1000.0) * (master_per_mille as f32 / 1000.0)
    }
}

#[derive(Debug, Clone)]
pub struct LoadedSound {
    pub id: SoundHashId,
    pub name: String,
    pub rate: u32,
    /// Planar f32 lanes at [`LoadedSound::rate`].
    pub channels: Vec<Vec<f32>>,
}

/// Linear fade envelope applied in place. `fade_in/out` in seconds at
/// `rate`; non-positive values disable that side.
pub fn apply_fade(samples: &mut [f32], rate: u32, fade_in_secs: f32, fade_out_secs: f32) {
    let n = samples.len();
    if n == 0 {
        return;
    }
    let over = |secs: f32| ((secs.max(0.0) * rate as f32) as usize).min(n);
    let fi = over(fade_in_secs);
    for (i, s) in samples.iter_mut().enumerate().take(fi) {
        *s *= i as f32 / fi.max(1) as f32;
    }
    let fo = over(fade_out_secs);
    for (i, s) in samples.iter_mut().enumerate().skip(n - fo) {
        let k = n - 1 - i;
        *s *= k as f32 / fo.max(1) as f32;
    }
}

#[derive(Debug)]
pub enum SoundboardError {
    Io(String),
    Decode(String),
    UnknownSound(SoundHashId),
    UnsupportedRate(u32),
}

impl std::fmt::Display for SoundboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "soundboard i/o: {e}"),
            Self::Decode(e) => write!(f, "decode failed: {e}"),
            Self::UnknownSound(id) => write!(f, "inexistent sound hash id {}", id.0),
            Self::UnsupportedRate(r) => write!(f, "unsupported sample rate: {r}"),
        }
    }
}

impl std::error::Error for SoundboardError {}

/// Playlist + mixer state: loaded sounds, playing set, master volume.
#[derive(Debug, Default)]
pub struct SoundboardController {
    sounds: HashMap<SoundHashId, LoadedSound>,
    playing: HashSet<SoundHashId>,
    master_volume_per_mille: u16,
}

impl SoundboardController {
    pub fn new() -> Self {
        Self {
            master_volume_per_mille: 1000,
            ..Self::default()
        }
    }

    pub fn load_file(&mut self, id: SoundHashId, name: String, path: &Path) -> Result<(), SoundboardError> {
        // decode_file always returns 48 kHz planar f32.
        let decoded = decode::decode_file(path)?;
        self.sounds.insert(id, LoadedSound {
            id,
            name,
            rate: decoded.rate,
            channels: decoded.channels,
        });
        Ok(())
    }

    pub fn play(&mut self, id: SoundHashId, opts: &SoundOptions) -> Result<OutputRoute, SoundboardError> {
        if !self.sounds.contains_key(&id) {
            return Err(SoundboardError::UnknownSound(id));
        }
        if opts.stop_other_sounds {
            self.playing.clear();
        }
        self.playing.insert(id);
        Ok(opts.route())
    }

    pub fn stop_all(&mut self) {
        self.playing.clear();
    }

    pub fn set_master_volume(&mut self, per_mille: u16) {
        self.master_volume_per_mille = per_mille.min(1000);
    }

    pub fn master_volume(&self) -> u16 {
        self.master_volume_per_mille
    }

    pub fn playing(&self) -> Vec<SoundHashId> {
        self.playing.iter().copied().collect()
    }
}
