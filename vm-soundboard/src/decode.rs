//! Sample decoding: WAV / FLAC / MP3 / Vorbis via Symphonia,
//! Opus via libopus. Ogg-Opus demux + resampling land in Fase 3b.

use super::SoundboardError;
use std::path::Path;
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decoded PCM at its native rate (planar f32).
#[derive(Debug)]
pub struct DecodedPcm {
    pub rate: u32,
    pub channels: Vec<Vec<f32>>,
}

/// Decode a whole file into memory, resampled to 48 kHz stereoflex.
/// Sounds are short; streaming decode lands with the playlist work.
/// Returns planar f32 lanes at 48 kHz regardless of source format/rate.
pub fn decode_file(path: &Path) -> Result<DecodedPcm, SoundboardError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let raw = match ext.as_str() {
        "opus" | "oga" => decode_opus_file(path)?,
        "ogg" => match decode_symphonia(path) {
            Ok(d) => d,
            // Symphonia 0.5 has no Opus codec: fall through to our demux.
            Err(_) => decode_opus_file(path)?,
        },
        _ => decode_symphonia(path)?,
    };
    let channels = super::resample::to_48000(raw.rate, raw.channels)?;
    Ok(DecodedPcm { rate: 48_000, channels })
}

fn decode_opus_file(path: &Path) -> Result<DecodedPcm, SoundboardError> {
    let bytes = std::fs::read(path).map_err(|e| SoundboardError::Io(e.to_string()))?;
    let streams =
        super::ogg::demux(&bytes).map_err(|e| SoundboardError::Decode(format!("ogg: {e}")))?;
    let dec = super::opus::decode_ogg_opus(&streams)?;
    Ok(DecodedPcm { rate: 48_000, channels: dec.samples })
}

fn decode_symphonia(path: &Path) -> Result<DecodedPcm, SoundboardError> {
    let file = std::fs::File::open(path).map_err(|e| SoundboardError::Io(e.to_string()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| SoundboardError::Decode(e.to_string()))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| SoundboardError::Decode("no default audio track".into()))?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate.unwrap_or(48_000);
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count())
        .unwrap_or(2);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| SoundboardError::Decode(e.to_string()))?;

    let mut lanes: Vec<Vec<f32>> = vec![Vec::new(); channels];
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet).map_err(|e| SoundboardError::Decode(e.to_string()))? {
            AudioBufferRef::F32(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| buf.chan(ch)[i]),
            AudioBufferRef::F64(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| buf.chan(ch)[i] as f32),
            AudioBufferRef::S32(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| {
                buf.chan(ch)[i] as f32 / i32::MAX as f32
            }),
            AudioBufferRef::S24(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| {
                buf.chan(ch)[i].0 as f32 / 8_388_607.0
            }),
            AudioBufferRef::S16(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| {
                buf.chan(ch)[i] as f32 / i16::MAX as f32
            }),
            AudioBufferRef::U8(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| {
                (buf.chan(ch)[i] as f32 - 128.0) / 128.0
            }),
            AudioBufferRef::U16(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| {
                (buf.chan(ch)[i] as f32 - 32768.0) / 32768.0
            }),
            AudioBufferRef::U24(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| {
                buf.chan(ch)[i].0 as f32 / 8_388_607.0
            }),
            AudioBufferRef::U32(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| {
                (buf.chan(ch)[i] as f64 - 2_147_483_648.0) as f32 / 2_147_483_648.0
            }),
            AudioBufferRef::S8(buf) => push_planar(&mut lanes, buf.chan(0).len(), |ch, i| {
                buf.chan(ch)[i] as f32 / i8::MAX as f32
            }),
        }
    }
    Ok(DecodedPcm { rate, channels: lanes })
}

fn push_planar(
    lanes: &mut [Vec<f32>],
    frames: usize,
    at: impl Fn(usize, usize) -> f32,
) {
    for (ch, lane) in lanes.iter_mut().enumerate() {
        lane.reserve(frames);
        for i in 0..frames {
            lane.push(at(ch, i));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a minimal PCM16 WAV (no deps).
    fn write_wav(path: &Path, rate: u32, samples: &[i16]) {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&((36 + samples.len() * 2) as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(samples.len() as u32 * 2).to_le_bytes());
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, v).unwrap();
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
    fn decode_wav_44100_resamples_to_48000() {
        let dir = std::env::temp_dir();
        let path = dir.join("vm_sb_test_44100.wav");
        let s: Vec<i16> = (0..44_100)
            .map(|i| (0.5 * 32767.0 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44_100.0).sin()) as i16)
            .collect();
        write_wav(&path, 44_100, &s);
        let dec = decode_file(&path).unwrap();
        assert_eq!(dec.rate, 48_000);
        assert_eq!(dec.channels.len(), 1);
        let steady = &dec.channels[0][4800..dec.channels[0].len() - 4800];
        let f = zero_cross_freq(steady, 48_000);
        assert!((f - 440.0).abs() < 440.0 * 0.01, "got {f:.1} Hz");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn decode_missing_file_errors() {
        assert!(decode_file(Path::new("/nonexistent-xyz/silence.wav")).is_err());
    }
}
