//! Opus-in-Ogg decoding via libopus (`audiopus`).
//!
//! Parses `OpusHead`/`OpusTags`, decodes audio packets at 48 kHz, trims
//! the pre-skip. End-trim follows the last page granule when present.
//! Corrupt packets are skipped (counted); only a total decode failure
//! is an error — a soundboard must never die on one bad file.

use super::ogg::OggStream;
use super::SoundboardError;
use audiopus::{Channels, SampleRate};

#[derive(Debug)]
pub struct DecodedOpus {
    pub channels: usize,
    pub samples: Vec<Vec<f32>>,
    pub skipped_packets: usize,
}

fn parse_head(packet: &[u8]) -> Result<(usize, usize), SoundboardError> {
    if packet.len() < 19 || &packet[0..8] != b"OpusHead" {
        return Err(SoundboardError::Decode("missing OpusHead".into()));
    }
    if packet[8] != 1 {
        return Err(SoundboardError::Decode(format!("unsupported OpusHead version {}", packet[8])));
    }
    let channels = packet[9] as usize;
    if channels != 1 && channels != 2 {
        return Err(SoundboardError::Decode(format!("unsupported opus channels {channels}")));
    }
    let preskip = u16::from_le_bytes([packet[10], packet[11]]) as usize;
    let mapping_family = packet[18];
    if mapping_family != 0 && mapping_family != 1 {
        return Err(SoundboardError::Decode(format!("unsupported mapping family {mapping_family}")));
    }
    if mapping_family == 1 && packet.len() < 21 + channels {
        return Err(SoundboardError::Decode("truncated channel mapping".into()));
    }
    Ok((channels, preskip))
}

/// Decode the first Opus logical stream found in `streams`.
pub fn decode_ogg_opus(streams: &[OggStream]) -> Result<DecodedOpus, SoundboardError> {
    let stream = streams
        .iter()
        .find(|s| s.packets.first().map(|p| p.data.starts_with(b"OpusHead")).unwrap_or(false))
        .ok_or_else(|| SoundboardError::Decode("no OpusHead stream found".into()))?;
    if stream.packets.len() < 2 || !stream.packets[1].data.starts_with(b"OpusTags") {
        return Err(SoundboardError::Decode("missing OpusTags".into()));
    }
    let (channels, preskip) = parse_head(&stream.packets[0].data)?;
    let ch = if channels == 1 { Channels::Mono } else { Channels::Stereo };
    let mut dec = audiopus::coder::Decoder::new(SampleRate::Hz48000, ch)
        .map_err(|e| SoundboardError::Decode(format!("opus decoder: {e:?}")))?;

    // 120 ms max Opus frame @48 kHz, interleaved scratch.
    let mut frame = vec![0.0f32; 5760 * channels];
    let mut lanes: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut skipped = 0usize;
    for packet in stream.packets.iter().skip(2) {
        if packet.data.is_empty() {
            continue;
        }
        match dec.decode_float(Some(&packet.data), &mut frame, false) {
            Ok(n) => {
                for (c, lane) in lanes.iter_mut().enumerate() {
                    lane.extend(frame.chunks_exact(channels).take(n).map(|s| s[c.min(channels - 1)]));
                }
            }
            Err(_) => skipped += 1,
        }
    }
    if lanes.iter().all(|l| l.is_empty()) {
        return Err(SoundboardError::Decode(format!(
            "no decodable opus audio ({skipped} bad packets)"
        )));
    }
    // Pre-skip comes from the encoder delay.
    for lane in lanes.iter_mut() {
        let cut = preskip.min(lane.len());
        lane.drain(..cut);
    }
    // End-trim from the last granule when sane.
    if let Some(last) = stream.packets.last() {
        if last.granule != u64::MAX && last.granule > preskip as u64 {
            let want = (last.granule as usize - preskip).min(lanes[0].len());
            for lane in lanes.iter_mut() {
                lane.truncate(want);
            }
        }
    }
    Ok(DecodedOpus { channels, samples: lanes, skipped_packets: skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_mono(freq: f32, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.5)
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
    fn opus_encode_decode_roundtrip() {
        use audiopus::{Application, Channels, SampleRate};
        let enc = audiopus::coder::Encoder::new(
            SampleRate::Hz48000,
            Channels::Mono,
            Application::Audio,
        )
        .unwrap();
        let pcm = sine_mono(440.0, 48_000, 48_000); // 1 s
        let mut packets: Vec<Vec<u8>> = Vec::new();
        // OpusHead + OpusTags headers (synthetic, valid).
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 1]); // version, channels
        head.extend_from_slice(&312u16.to_le_bytes()); // preskip (typical)
        head.extend_from_slice(&48_000u32.to_le_bytes()); // input rate
        head.extend_from_slice(&0i16.to_le_bytes()); // gain
        head.push(0); // mapping family
        packets.push(head);
        packets.push(b"OpusTags".to_vec());
        let mut out = vec![0u8; 4000];
        for chunk in pcm.chunks(960) {
            // Encoder works on i16; convert.
            let s16: Vec<i16> = chunk.iter().map(|s| (s * 32767.0) as i16).collect();
            let n = enc.encode(&s16, &mut out).unwrap();
            packets.push(out[..n].to_vec());
        }
        let streams = vec![OggStream { serial: 1, packets: packets.into_iter().enumerate().map(|(i, data)| {
            crate::ogg::RawPacket { data, granule: if i < 2 { 0 } else { (i - 2) as u64 * 960 }, bos: i == 0, eos: false }
        }).collect() }];
        let dec = decode_ogg_opus(&streams).unwrap();
        assert_eq!(dec.channels, 1);
        // Skip codec startup transient + preskip edge, measure steady state.
        let steady = &dec.samples[0][48_000 / 4..];
        let f = zero_cross_freq(steady, 48_000);
        assert!((f - 440.0).abs() < 440.0 * 0.03, "got {f:.1} Hz");
        assert_eq!(dec.skipped_packets, 0);
    }

    #[test]
    fn rejects_non_opus() {
        let streams = vec![OggStream { serial: 1, packets: vec![] }];
        assert!(decode_ogg_opus(&streams).is_err());
    }
}
