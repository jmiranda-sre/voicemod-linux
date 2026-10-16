//! Minimal Ogg container demuxer (RFC 3533 subset, clean-room).
//!
//! Parses pages, validates CRC32 (IEEE) — corrupt pages are skipped —
//! and reassembles packets, including packets continued across pages.
//! One logical stream per bitstream serial; returns streams in order of
//! first appearance.

use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct RawPacket {
    /// Complete packet payload.
    pub data: Vec<u8>,
    /// Granule position of the page that completed the packet.
    pub granule: u64,
    pub bos: bool,
    pub eos: bool,
}

#[derive(Debug, Clone)]
pub struct OggStream {
    pub serial: u32,
    pub packets: Vec<RawPacket>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OggError {
    TooShort,
    BadCapture,
    BadVersion,
    Truncated,
}

impl std::fmt::Display for OggError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "not an ogg stream"),
            Self::BadCapture => write!(f, "bad ogg capture pattern"),
            Self::BadVersion => write!(f, "unsupported ogg version"),
            Self::Truncated => write!(f, "truncated ogg page"),
        }
    }
}

impl std::error::Error for OggError {}

fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

/// CRC32 over the page with the checksum field zeroed.
fn page_crc(page: &[u8]) -> u32 {
    let table = crc_table();
    let mut crc = 0xFFFF_FFFFu32;
    for (i, b) in page.iter().enumerate() {
        let v = if (22..26).contains(&i) { 0u8 } else { *b };
        crc = table[((crc ^ v as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Demux all logical streams. Corrupt-checksum pages are skipped, not fatal.
pub fn demux(mut data: &[u8]) -> Result<Vec<OggStream>, OggError> {
    if data.len() < 27 {
        return Err(OggError::TooShort);
    }
    let mut order: Vec<u32> = Vec::new();
    let mut streams: HashMap<u32, Vec<RawPacket>> = HashMap::new();
    // In-progress continued packet per serial.
    let mut pending: HashMap<u32, (Vec<u8>, bool)> = HashMap::new();

    while !data.is_empty() {
        if data.len() < 27 {
            break; // trailing garbage tolerated
        }
        if &data[0..4] != b"OggS" {
            return Err(OggError::BadCapture);
        }
        if data[4] != 0 {
            return Err(OggError::BadVersion);
        }
        let header_type = data[5];
        let granule = le_u64(&data[6..14]);
        let serial = le_u32(&data[14..18]);
        let stored_crc = le_u32(&data[22..26]);
        let nsegs = data[26] as usize;
        if data.len() < 27 + nsegs {
            return Err(OggError::Truncated);
        }
        let seg_table = &data[27..27 + nsegs];
        let body_len: usize = seg_table.iter().map(|s| *s as usize).sum();
        let page_len = 27 + nsegs + body_len;
        if data.len() < page_len {
            return Err(OggError::Truncated);
        }
        let page = &data[..page_len];
        data = &data[page_len..];

        if page_crc(page) != stored_crc {
            continue; // corrupt page: skip, keep stream state
        }
        if let std::collections::hash_map::Entry::Vacant(e) = streams.entry(serial) {
            e.insert(Vec::new());
            order.push(serial);
        }
        let bos = header_type & 0x02 != 0;
        let eos = header_type & 0x04 != 0;
        let continued = header_type & 0x01 != 0;
        let mut body = &page[27 + nsegs..];

        // A fresh page without the continued flag drops any stale partial.
        if !continued {
            pending.remove(&serial);
        }
        let (mut packet, mut packet_bos) =
            pending.remove(&serial).unwrap_or_default();
        if packet.is_empty() {
            // Fresh packet: BOS comes from this page's header.
            packet_bos = bos;
        }
        for seg in seg_table {
            let len = *seg as usize;
            packet.extend_from_slice(&body[..len]);
            body = &body[len..];
            if len < 255 {
                // Packet completes here.
                streams.get_mut(&serial).unwrap().push(RawPacket {
                    data: std::mem::take(&mut packet),
                    granule,
                    bos: packet_bos,
                    eos,
                });
                packet_bos = false;
            }
        }
        if !packet.is_empty() {
            // Continues on the next page (last segment was 255).
            pending.insert(serial, (packet, packet_bos));
        }
    }
    Ok(order
        .into_iter()
        .map(|serial| OggStream { serial, packets: streams.remove(&serial).unwrap_or_default() })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one page with a correct checksum (test helper).
    fn page(serial: u32, seq: u32, header_type: u8, granule: u64, payloads: &[&[u8]]) -> Vec<u8> {
        // Split payloads into 255-byte segments the way a muxer would.
        let mut segs: Vec<u8> = Vec::new();
        let mut body: Vec<u8> = Vec::new();
        for p in payloads {
            let mut rest = *p;
            // Zero-length packet = single 0 segment.
            loop {
                let take = rest.len().min(255);
                segs.push(take as u8);
                body.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                if take < 255 {
                    break;
                }
            }
        }
        let mut pg = Vec::new();
        pg.extend_from_slice(b"OggS");
        pg.push(0);
        pg.push(header_type);
        pg.extend_from_slice(&granule.to_le_bytes());
        pg.extend_from_slice(&serial.to_le_bytes());
        pg.extend_from_slice(&seq.to_le_bytes());
        pg.extend_from_slice(&0u32.to_le_bytes()); // checksum placeholder
        pg.push(segs.len() as u8);
        pg.extend_from_slice(&segs);
        pg.extend_from_slice(&body);
        let crc = page_crc(&pg);
        pg[22..26].copy_from_slice(&crc.to_le_bytes());
        pg
    }

    #[test]
    fn single_page_two_packets() {
        let mut stream = page(7, 0, 0x02, 0, &[b"hello", b"world!"]);
        let out = demux(&stream).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].serial, 7);
        assert_eq!(out[0].packets.len(), 2);
        assert_eq!(out[0].packets[0].data, b"hello");
        assert!(out[0].packets[0].bos);
        stream[30] ^= 0xFF; // corrupt checksum
        let out = demux(&stream).unwrap();
        assert!(out.is_empty() || out[0].packets.is_empty());
    }

    #[test]
    fn continued_packet_across_pages() {
        // True continuation: page 0 ends mid-packet (last segment 255),
        // page 1 carries the `continued` flag and completes it.
        fn raw_page(serial: u32, seq: u32, header_type: u8, granule: u64, segs: &[u8], body: &[u8]) -> Vec<u8> {
            let mut pg = Vec::new();
            pg.extend_from_slice(b"OggS");
            pg.push(0);
            pg.push(header_type);
            pg.extend_from_slice(&granule.to_le_bytes());
            pg.extend_from_slice(&serial.to_le_bytes());
            pg.extend_from_slice(&seq.to_le_bytes());
            pg.extend_from_slice(&0u32.to_le_bytes());
            pg.push(segs.len() as u8);
            pg.extend_from_slice(segs);
            pg.extend_from_slice(body);
            let crc = page_crc(&pg);
            pg[22..26].copy_from_slice(&crc.to_le_bytes());
            pg
        }
        let big = vec![0xABu8; 600];
        let p0 = raw_page(9, 0, 0x02, 0, &[255, 255], &big[..510]);
        let p1 = raw_page(9, 1, 0x01, 600, &[90, 4], &[big[510..].to_vec(), b"tail".to_vec()].concat());
        let mut data = p0;
        data.extend_from_slice(&p1);
        let out = demux(&data).unwrap();
        assert_eq!(out[0].packets.len(), 2);
        assert_eq!(out[0].packets[0].data, big);
        assert!(out[0].packets[0].bos);
        assert_eq!(out[0].packets[1].data, b"tail");
        assert!(!out[0].packets[1].bos);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(demux(b"nope").unwrap_err(), OggError::TooShort);
        let mut bad = vec![0u8; 64];
        bad[0..4].copy_from_slice(b"XXXX");
        assert_eq!(demux(&bad).unwrap_err(), OggError::BadCapture);
    }
}
