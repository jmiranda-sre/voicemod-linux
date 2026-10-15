//! Offline stub backend: deterministic, no hardware. Used for
//! headless CI (golden-render tests) until the PipeWire backend lands.

use super::super::{
    AudioBackend, AudioDevice, AudioError, CallbackStats, DeviceRole, DeviceUid, HotplugEvent,
    StreamKind, StreamStopReason, StreamTopology, MAX_CHANNELS, SAMPLE_RATE,
};

/// In-memory loopback: what is "captured" is handed to DSP and fanned out
/// to the virtual + monitor endpoints without touching hardware.
#[derive(Debug, Default)]
pub struct StubBackend {
    devices: Vec<AudioDevice>,
    open: Option<StreamTopology>,
    running: bool,
}

impl StubBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AudioBackend for StubBackend {
    fn enumerate(&mut self) -> Result<Vec<AudioDevice>, AudioError> {
        self.devices = vec![
            AudioDevice {
                uid: DeviceUid::new("stub:input:0"),
                name: "Stub Microphone".into(),
                role: DeviceRole::Input,
                sample_rate: SAMPLE_RATE,
                channels: 1,
                latency_secs: 0.010,
                is_bluetooth: false,
            },
            AudioDevice {
                uid: DeviceUid::new("stub:virtual:vm-mic"),
                name: super::super::VIRTUAL_SOURCE_NAME.into(),
                role: DeviceRole::Virtual,
                sample_rate: SAMPLE_RATE,
                channels: MAX_CHANNELS,
                latency_secs: 0.005,
                is_bluetooth: false,
            },
            AudioDevice {
                uid: DeviceUid::new("stub:monitor:0"),
                name: super::super::MONITOR_SINK_NAME.into(),
                role: DeviceRole::Monitor,
                sample_rate: SAMPLE_RATE,
                channels: MAX_CHANNELS,
                latency_secs: 0.010,
                is_bluetooth: false,
            },
        ];
        Ok(self.devices.clone())
    }

    fn open(&mut self, topology: &StreamTopology) -> Result<(), AudioError> {
        for uid in [&topology.input, &topology.virtual_out]
            .into_iter()
            .chain(topology.monitor.as_ref())
        {
            if !self.devices.iter().any(|d| &d.uid == uid) {
                return Err(AudioError::DeviceNotFound(uid.clone()));
            }
        }
        self.open = Some(topology.clone());
        Ok(())
    }

    fn start(&mut self) -> Result<(), AudioError> {
        if self.open.is_none() {
            return Err(AudioError::NoStream);
        }
        if self.running {
            return Err(AudioError::StreamAlreadyRunning);
        }
        self.running = true;
        Ok(())
    }

    fn stop(&mut self, _reason: StreamStopReason) -> Result<(), AudioError> {
        self.running = false;
        Ok(())
    }

    fn stats(&self, _kind: StreamKind) -> CallbackStats {
        CallbackStats::default()
    }

    fn poll_hotplug(&mut self) -> Vec<HotplugEvent> {
        Vec::new()
    }

    fn ensure_virtual_source(&mut self) -> Result<DeviceUid, AudioError> {
        Ok(DeviceUid::new("stub:virtual:vm-mic"))
    }
}
