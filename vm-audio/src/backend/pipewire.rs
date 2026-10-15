//! PipeWire backend wiring point.
//!
//! Spec-first: this module pins the *verified* property table the live
//! implementation must use (ledger Fase 2 ①③). The `pipewire` crate is
//! deliberately unwired — its `libspa` sys crate currently fails against
//! the system's PipeWire 1.6.9 headers. Fase 3b aligns versions, then this
//! module grows the `MainLoop`/`Context`/`Stream` connection.

use super::super::{MONITOR_SINK_NAME, QUANTUM_FRAMES, SAMPLE_RATE, VIRTUAL_SOURCE_NAME};

/// Exact PipeWire node properties for our endpoints.
#[derive(Debug, Clone)]
pub struct PwNodeProps {
    pub node_name: &'static str,
    pub media_class: &'static str,
    pub is_virtual: bool,
    pub rate: u32,
    pub channels: u32,
    pub quantum: u32,
}

/// Virtual microphone source: what other apps capture.
/// Replaces the WDM loopback driver (render endpoint + capture endpoint).
pub fn virtual_source_props() -> PwNodeProps {
    PwNodeProps {
        node_name: VIRTUAL_SOURCE_NAME,
        media_class: "Audio/Source/Virtual",
        is_virtual: true,
        rate: SAMPLE_RATE,
        channels: 2,
        quantum: QUANTUM_FRAMES,
    }
}

/// Local monitor sink (hear-myself path, never exposed as a mic).
pub fn monitor_sink_props() -> PwNodeProps {
    PwNodeProps {
        node_name: MONITOR_SINK_NAME,
        media_class: "Audio/Sink",
        is_virtual: true,
        rate: SAMPLE_RATE,
        channels: 2,
        quantum: QUANTUM_FRAMES,
    }
}

impl PwNodeProps {
    /// Property keys, in the order they must be applied.
    /// (`node.virtual=true` is what makes `vm-mic` appear as a microphone.)
    pub fn as_pairs(&self) -> [(&'static str, String); 6] {
        [
            ("node.name", self.node_name.to_string()),
            ("media.class", self.media_class.to_string()),
            ("node.virtual", self.is_virtual.to_string()),
            ("audio.rate", self.rate.to_string()),
            ("audio.channels", self.channels.to_string()),
            ("node.latency", format!("{}/{}", self.quantum, self.rate)),
        ]
    }
}
