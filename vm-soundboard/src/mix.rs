//! Soundboard mixer: per-sound routing (`sendToVAD`), master volume,
//! fades, loop and stop semantics → stereo Vad + Monitor buses.
//!
//! All stored sounds are 48 kHz (resampled at load). The mixer is
//! sample-exact and allocation-light per call (only the output buses).

use super::{LoadedSound, SoundHashId, SoundOptions};
use std::collections::HashMap;

/// One active voice: playhead into a loaded sound.
#[derive(Debug)]
pub struct PlayingSound {
    pub id: SoundHashId,
    pub options: SoundOptions,
    /// Playhead in frames.
    pub position: usize,
    /// Frames remaining in a triggered fade-out (`None` = not fading).
    pub fade_out_left: Option<usize>,
    /// Total fade-out length for ramp computation.
    pub fade_out_total: usize,
}

/// Stereo output buses (planar).
#[derive(Debug, Default)]
pub struct StereoBus {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
}

#[derive(Debug, Default)]
pub struct SoundboardMixer {
    sounds: HashMap<SoundHashId, LoadedSound>,
    playing: Vec<PlayingSound>,
    master_per_mille: u16,
}

impl SoundboardMixer {
    pub fn new() -> Self {
        Self { master_per_mille: 1000, ..Self::default() }
    }

    pub fn add_sound(&mut self, sound: LoadedSound) {
        self.sounds.insert(sound.id, sound);
    }

    pub fn remove_sound(&mut self, id: SoundHashId) {
        self.sounds.remove(&id);
        self.playing.retain(|p| p.id != id);
    }

    /// Start playback. `stop_other_sounds` clears the playlist first,
    /// mirroring `stopAllSounds` semantics on play.
    pub fn play(&mut self, id: SoundHashId, options: SoundOptions) -> Result<(), SoundHashId> {
        if !self.sounds.contains_key(&id) {
            return Err(id);
        }
        if options.stop_other_sounds {
            self.playing.clear();
        }
        // Restart if already playing.
        if let Some(p) = self.playing.iter_mut().find(|p| p.id == id) {
            p.position = 0;
            p.options = options;
            p.fade_out_left = None;
            p.fade_out_total = 0;
        } else {
            self.playing.push(PlayingSound {
                id,
                options,
                position: 0,
                fade_out_left: None,
                fade_out_total: 0,
            });
        }
        Ok(())
    }

    pub fn stop(&mut self, id: SoundHashId) {
        self.playing.retain(|p| p.id != id);
    }

    /// Trigger a fade-out over `secs`, auto-stopping at the end.
    /// Returns false when the id isn't playing.
    pub fn fade_out(&mut self, id: SoundHashId, secs: f32) -> bool {
        match self.playing.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                let total = (secs.max(0.0) * 48_000.0) as usize;
                p.fade_out_total = total.max(1);
                p.fade_out_left = Some(p.fade_out_total);
                true
            }
            None => false,
        }
    }

    pub fn stop_all(&mut self) {
        self.playing.clear();
    }

    pub fn set_master_volume(&mut self, per_mille: u16) {
        self.master_per_mille = per_mille.min(1000);
    }

    pub fn master_volume(&self) -> u16 {
        self.master_per_mille
    }

    pub fn playing(&self) -> Vec<SoundHashId> {
        self.playing.iter().map(|p| p.id).collect()
    }

    /// Render `frames` samples into fresh Vad + Monitor buses.
    /// Fades were baked at load; per-call gain = sound × master.
    /// Muted-voice / mute-others flags are honored by the caller dropping
    /// the respective bus (documented contract).
    pub fn mix(&mut self, frames: usize) -> (StereoBus, StereoBus) {
        let mut vad = StereoBus {
            left: vec![0.0; frames],
            right: vec![0.0; frames],
        };
        let mut mon = StereoBus {
            left: vec![0.0; frames],
            right: vec![0.0; frames],
        };
        let master = self.master_per_mille as f32 / 1000.0;
        let mut finished: Vec<SoundHashId> = Vec::new();
        for p in self.playing.iter_mut() {
            let Some(sound) = self.sounds.get(&p.id) else { continue };
            if sound.channels.is_empty() {
                continue;
            }
            let len = sound.channels[0].len();
            if len == 0 {
                continue;
            }
            let g = (p.options.volume_per_mille as f32 / 1000.0) * master;
            let fade_in = (p.options.fade_in_secs.max(0.0) * 48_000.0) as usize;
            let fade_out = (p.options.fade_out_secs.max(0.0) * 48_000.0) as usize;
            let to_vad = p.options.send_to_vad;
            for f in 0..frames {
                let idx = p.position + f;
                // Triggered fade-out overrides the play envelope.
                let mut fade_m = 1.0;
                if let Some(left) = p.fade_out_left.as_mut() {
                    if *left > 0 {
                        fade_m = *left as f32 / p.fade_out_total as f32;
                        *left -= 1;
                    } else {
                        finished.push(p.id);
                        break;
                    }
                }
                if idx >= len {
                    if p.options.loop_ {
                        // Wrap within this block (may wrap multiple times).
                        let at = idx % len;
                        Self::accumulate(&mut vad, &mut mon, sound, at, g, 1.0, to_vad, f);
                    } else {
                        break;
                    }
                } else {
                    // Play-time fade envelope (linear ramps).
                    let mut m = fade_m;
                    if fade_in > 0 && idx < fade_in {
                        m *= idx as f32 / fade_in as f32;
                    }
                    if fade_out > 0 && !p.options.loop_ && idx + fade_out >= len {
                        m *= (len - idx) as f32 / fade_out as f32;
                    }
                    Self::accumulate(&mut vad, &mut mon, sound, idx, g, m, to_vad, f);
                }
            }
            p.position += frames;
            if !p.options.loop_ && p.position >= len {
                finished.push(p.id);
            } else if p.options.loop_ {
                p.position %= len.max(1);
            }
        }
        for id in finished {
            self.playing.retain(|p| p.id != id);
        }
        (vad, mon)
    }

    fn accumulate(
        vad: &mut StereoBus,
        mon: &mut StereoBus,
        sound: &LoadedSound,
        at: usize,
        gain: f32,
        fade: f32,
        to_vad: bool,
        out: usize,
    ) {
        let nch = sound.channels.len();
        let l = sound.channels[0][at] * gain * fade;
        let r = if nch > 1 { sound.channels[1][at] * gain } else { l };
        if to_vad {
            vad.left[out] += l;
            vad.right[out] += r;
        }
        // Monitor always receives (hear-what-plays); mute_others/mute_voice
        // are applied by the caller dropping a whole bus.
        mon.left[out] += l;
        mon.right[out] += r;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound(id: u64, ch: Vec<Vec<f32>>) -> LoadedSound {
        LoadedSound { id: SoundHashId(id), name: format!("s{id}"), rate: 48_000, channels: ch }
    }

    fn opts() -> SoundOptions {
        SoundOptions::default()
    }

    #[test]
    fn routing_matrix() {
        let mut m = SoundboardMixer::new();
        m.add_sound(sound(1, vec![vec![1.0; 64], vec![1.0; 64]]));
        // to VAD+monitor by default (send_to_vad = true).
        m.play(SoundHashId(1), opts()).unwrap();
        let (vad, mon) = m.mix(64);
        assert!(vad.left.iter().all(|s| *s == 1.0));
        assert!(mon.left.iter().all(|s| *s == 1.0));
        // Monitor-only routing.
        m.stop_all();
        let mut o = opts();
        o.send_to_vad = false;
        m.play(SoundHashId(1), o).unwrap();
        let (vad, mon) = m.mix(64);
        assert!(vad.left.iter().all(|s| *s == 0.0));
        assert!(mon.left.iter().all(|s| *s == 1.0));
    }

    #[test]
    fn master_volume_and_stop() {
        let mut m = SoundboardMixer::new();
        m.add_sound(sound(2, vec![vec![1.0; 32]]));
        m.set_master_volume(500);
        m.play(SoundHashId(2), opts()).unwrap();
        let (vad, _) = m.mix(32);
        assert!((vad.left[0] - 0.5).abs() < 1e-6);
        // stop_all silences everything and clears the playlist.
        m.stop_all();
        assert!(m.playing().is_empty());
        let (vad, mon) = m.mix(32);
        assert!(vad.left.iter().all(|s| *s == 0.0));
        assert!(mon.left.iter().all(|s| *s == 0.0));
        // Non-looping sounds retire at the end.
        m.play(SoundHashId(2), opts()).unwrap();
        m.mix(40);
        assert!(m.playing().is_empty());
    }

    #[test]
    fn loop_wraps() {
        let mut m = SoundboardMixer::new();
        m.add_sound(sound(3, vec![vec![0.5; 8]]));
        let mut o = opts();
        o.loop_ = true;
        m.play(SoundHashId(3), o).unwrap();
        let (vad, _) = m.mix(20);
        assert!(vad.left.iter().all(|s| (*s - 0.5).abs() < 1e-6));
        assert!(!m.playing().is_empty());
    }

    #[test]
    fn unknown_sound_rejected() {
        let mut m = SoundboardMixer::new();
        assert!(m.play(SoundHashId(999), opts()).is_err());
    }
}

#[cfg(test)]
mod fade_tests {
    use super::*;

    fn sound(id: u64, ch: Vec<Vec<f32>>) -> LoadedSound {
        LoadedSound { id: SoundHashId(id), name: format!("s{id}"), rate: 48_000, channels: ch }
    }

    #[test]
    fn play_time_fades_shape() {
        let mut m = SoundboardMixer::new();
        // 1 s of DC at 48 kHz.
        m.add_sound(sound(7, vec![vec![1.0; 48_000]]));
        let mut o = SoundOptions::default();
        o.fade_in_secs = 0.1; // 4800 frames ramp
        o.fade_out_secs = 0.1;
        m.play(SoundHashId(7), o).unwrap();
        let (vad, _) = m.mix(48_000);
        assert!(vad.left[0] < 0.01, "fade-in starts near silence");
        assert!((vad.left[24_000] - 1.0).abs() < 0.01, "middle is full level");
        assert!(vad.left[47_999] < 0.01, "fade-out ends near silence");
    }
}
