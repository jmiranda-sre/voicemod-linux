//! `vm-midi`: ALSA MIDI input wrapper (F-0.4.C item 37).
//!
//! Thin layer over `midir` (ALSA sequencer) that turns raw 3-byte MIDI
//! messages into [`MidiEvent`]s on a plain channel — the daemon's MIDI
//! thread stays synchronous and easy to test without hardware.

use std::time::Duration;

use vm_control::{MidiEvent, MidiKind};

/// Result of a non-blocking read.
pub enum MidiRecv {
    Event(MidiEvent),
    Timeout,
    Disconnected,
}

/// An open MIDI input. The connection is kept alive for the struct's
/// lifetime; dropping it closes the port.
pub struct MidiInput {
    rx: std::sync::mpsc::Receiver<MidiEvent>,
    _conn: Option<midir::MidiInputConnection<()>>,
}

impl MidiInput {
    /// Open `port` (by name) or the first available port when `None`.
    /// Empty `port` means "any port" for the daemon's default.
    pub fn open(port: Option<&str>) -> Result<Self, String> {
        let input = midir::MidiInput::new("vm-midi").map_err(|e| e.to_string())?;
        let ports = input.ports();
        let idx = match port {
            Some(name) => ports
                .iter()
                .position(|p| input.port_name(p).map(|n| n == name).unwrap_or(false)),
            None => {
                if ports.is_empty() {
                    None
                } else {
                    Some(0)
                }
            }
        };
        let idx = idx.ok_or_else(|| match port {
            Some(name) => format!("MIDI port '{name}' not found"),
            None => "no MIDI input ports available".to_string(),
        })?;
        let (tx, rx) = std::sync::mpsc::channel::<MidiEvent>();
        let conn = input
            .connect(&ports[idx], "vm-midi-in", move |_ts, bytes: &[u8], _| {
                if bytes.len() < 3 {
                    return;
                }
                let status = bytes[0];
                let kind = match status & 0xF0 {
                    0xB0 => MidiKind::Cc,
                    0x90 | 0x80 => MidiKind::Note,
                    _ => return,
                };
                let _ = tx.send(MidiEvent {
                    channel: status & 0x0F,
                    kind,
                    number: bytes[1] & 0x7F,
                    value: bytes[2] & 0x7F,
                });
            }, ())
            .map_err(|e| e.to_string())?;
        Ok(Self { rx, _conn: Some(conn) })
    }

    /// Block up to `dur` for the next event.
    pub fn recv_timeout(&self, dur: Duration) -> MidiRecv {
        match self.rx.recv_timeout(dur) {
            Ok(ev) => MidiRecv::Event(ev),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => MidiRecv::Timeout,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => MidiRecv::Disconnected,
        }
    }

    /// Names of the currently available input ports.
    pub fn ports() -> Result<Vec<String>, String> {
        let input = midir::MidiInput::new("vm-midi").map_err(|e| e.to_string())?;
        Ok(input
            .ports()
            .iter()
            .filter_map(|p| input.port_name(p).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_deterministic_for_known_bytes() {
        // CC 0xB0 | channel 3, controller 7, value 64.
        let ev = parse(&[0xB3, 0x07, 0x40]);
        assert_eq!(ev.channel, 3);
        assert_eq!(ev.kind, MidiKind::Cc);
        assert_eq!(ev.number, 7);
        assert_eq!(ev.value, 64);
        // Note On 0x90 | channel 0, note 36, velocity 100.
        let ev = parse(&[0x90, 0x24, 0x64]);
        assert_eq!(ev.kind, MidiKind::Note);
        assert_eq!(ev.number, 36);
        assert_eq!(ev.value, 100);
        // Note Off (0x80) is a note event with a release edge.
        let ev = parse(&[0x80, 0x24, 0x00]);
        assert_eq!(ev.kind, MidiKind::Note);
        assert_eq!(ev.value, 0);
        // Program Change (0xC0) is ignored.
        assert!(parse_opt(&[0xC0, 0x01, 0x00]).is_none());
    }

    /// Mirror of the closure above, kept in sync for testing.
    fn parse(bytes: &[u8]) -> MidiEvent {
        parse_opt(bytes).expect("event")
    }

    fn parse_opt(bytes: &[u8]) -> Option<MidiEvent> {
        if bytes.len() < 3 {
            return None;
        }
        let status = bytes[0];
        let kind = match status & 0xF0 {
            0xB0 => MidiKind::Cc,
            0x90 | 0x80 => MidiKind::Note,
            _ => return None,
        };
        Some(MidiEvent {
            channel: status & 0x0F,
            kind,
            number: bytes[1] & 0x7F,
            value: bytes[2] & 0x7F,
        })
    }
}
