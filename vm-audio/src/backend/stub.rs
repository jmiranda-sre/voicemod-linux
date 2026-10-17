//! Offline stub backend: deterministic, no hardware. Used for
//! headless CI (golden-render tests) until the PipeWire backend lands.

use super::super::{
    AppStream, AudioBackend, AudioDevice, AudioError, CallbackStats, DeviceRole, DeviceUid,
    HotplugEvent, PassthroughSettings, StreamKind, StreamStopReason, StreamTopology, MAX_CHANNELS,
    SAMPLE_RATE,
};

/// In-memory loopback: what is "captured" is handed to DSP and fanned out
/// to the virtual + monitor endpoints without touching hardware.
#[derive(Debug, Default)]
pub struct StubBackend {
    devices: Vec<AudioDevice>,
    open: Option<StreamTopology>,
    running: bool,
    /// Passthrough model (F-0.4.B 18): canned app list + move tracking.
    apps: Vec<AppStream>,
    moved: std::collections::HashMap<u32, String>,
    passthrough: PassthroughSettings,
}

impl StubBackend {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_stub_apps(&mut self) {
        if self.apps.is_empty() {
            self.apps = vec![
                AppStream {
                    id: 1,
                    app: "FakePlayer".into(),
                    name: "FakePlayer: stream".into(),
                    sink: "stub-sink".into(),
                },
                AppStream {
                    id: 2,
                    app: "Discord".into(),
                    name: "Discord: playback".into(),
                    sink: "stub-sink".into(),
                },
            ];
        }
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

    fn output_streams(&mut self) -> Result<Vec<AppStream>, AudioError> {
        self.ensure_stub_apps();
        Ok(self.apps.clone())
    }

    /// In-memory passthrough model: the `app` field is declarative — when
    /// enabled, exactly the matching app (if any) sits on `vm-passthrough`;
    /// disabling restores every moved app to its original sink.
    fn set_passthrough(&mut self, settings: &PassthroughSettings) -> Result<(), AudioError> {
        self.ensure_stub_apps();
        self.passthrough = settings.clone();
        // Restore anything that must no longer be on the passthrough sink.
        let moved = &mut self.moved;
        for app in self.apps.iter_mut() {
            let keep = settings.enabled
                && !settings.app.is_empty()
                && app.app.eq_ignore_ascii_case(&settings.app);
            if app.sink == "vm-passthrough" && !keep {
                app.sink = moved.remove(&app.id).unwrap_or_else(|| "stub-sink".into());
            }
        }
        if settings.enabled && !settings.app.is_empty() {
            let app = self
                .apps
                .iter_mut()
                .find(|a| a.app.eq_ignore_ascii_case(&settings.app))
                .ok_or_else(|| {
                    AudioError::Backend(format!("application '{}' not found", settings.app))
                })?;
            if app.sink != "vm-passthrough" {
                self.moved.entry(app.id).or_insert_with(|| app.sink.clone());
                app.sink = "vm-passthrough".into();
            }
        }
        Ok(())
    }
}
