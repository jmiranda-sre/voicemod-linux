//! `vm-audio`: PipeWire audio backend abstraction (headless-first).
//!
//! Mirrors the ledger (Fase 2, item ①): 48 kHz, 20 ms quantum (960 frames),
//! three device roles (input / virtual / monitor), RT threads at a *standard*
//! pro-audio level (never max — same rationale as the reference's
//! `AVRT_PRIORITY_NORMAL` choice), hotplug events, xrun-aware restart.

pub mod backend;

/// Canonical sample rate (Hz). Matches the reference engine anchor.
pub const SAMPLE_RATE: u32 = 48_000;
/// Canonical processing quantum (frames). 960 @ 48 kHz = 20 ms,
/// the same frame size the reference codec path uses downstream.
pub const QUANTUM_FRAMES: u32 = 960;
/// Allowed quantum range for tuning (low-latency ↔ stability).
pub const QUANTUM_MIN: u32 = 256;
pub const QUANTUM_MAX: u32 = 2048;
/// Mono or stereo only — same constraint as the reference virtual device.
pub const MAX_CHANNELS: usize = 2;
/// RT priority for audio threads. Deliberately below max (see module docs).
pub const RT_PRIORITY: i32 = 70;
/// Name of our PipeWire virtual source (replaces the kernel WDM driver).
pub const VIRTUAL_SOURCE_NAME: &str = "vm-mic";
/// Name of the local monitor sink (hear-myself path).
pub const MONITOR_SINK_NAME: &str = "vm-monitor";

/// Device role in the streaming topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceRole {
    /// Physical microphone (capture).
    Input,
    /// Virtual microphone exposed to other apps (loopback target).
    Virtual,
    /// Local monitor / hear-myself output.
    Monitor,
}

/// Opaque device identifier (UID string, compared by exact match).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceUid(pub String);

impl DeviceUid {
    pub fn new(uid: impl Into<String>) -> Self {
        Self(uid.into())
    }
}

/// A discovered audio device.
#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub uid: DeviceUid,
    pub name: String,
    pub role: DeviceRole,
    /// Sample rate in Hz (devices report kHz-style doubles; stored as int).
    pub sample_rate: u32,
    pub channels: usize,
    /// Device default low latency, seconds (informational; quantum wins).
    pub latency_secs: f64,
    pub is_bluetooth: bool,
}

/// The three endpoints a streaming session binds together.
#[derive(Debug, Clone)]
pub struct StreamTopology {
    pub input: DeviceUid,
    pub virtual_out: DeviceUid,
    pub monitor: Option<DeviceUid>,
}

/// Stream direction/shape. Own taxonomy (clean-room).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Mic capture → DSP (full-duplex capture side).
    FullDuplexCapture,
    /// DSP → virtual source (half-duplex playback side).
    HalfDuplexPlayback,
    /// DSP → local monitor.
    Monitor,
}

/// Why a stream stopped / needs restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStopReason {
    Requested,
    DeviceDisconnected,
    DeviceInvalidated,
    XrunStorm,
    InternalError,
}

/// Per-callback telemetry. Mirrors the reference's
/// `callback_average_duration_us` + xrun/watchdog counters.
#[derive(Debug, Default)]
pub struct CallbackStats {
    pub invocations: u64,
    pub avg_duration_us: f64,
    pub max_duration_us: u64,
    pub xruns: u64,
    pub watchdog_trips: u64,
    /// Frames processed in the most recent callback (diagnostic).
    pub last_frames: usize,
}

impl CallbackStats {
    pub fn record(&mut self, duration_us: u64, xrun: bool) {
        // Exponential moving average: cheap, lock-free friendly.
        let alpha = 1.0 / 16.0;
        self.avg_duration_us += alpha * (duration_us as f64 - self.avg_duration_us);
        self.max_duration_us = self.max_duration_us.max(duration_us);
        self.invocations += 1;
        if xrun {
            self.xruns += 1;
        }
    }
}

#[derive(Debug)]
pub enum AudioError {
    Backend(String),
    DeviceNotFound(DeviceUid),
    UnsupportedRate { want: u32, got: u32 },
    UnsupportedChannels(usize),
    StreamAlreadyRunning,
    NoStream,
    Realtime(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "audio backend error: {e}"),
            Self::DeviceNotFound(uid) => write!(f, "device not found: {}", uid.0),
            Self::UnsupportedRate { want, got } => {
                write!(f, "unsupported rate: want {want} Hz, device runs at {got} Hz")
            }
            Self::UnsupportedChannels(n) => write!(f, "unsupported channel count: {n} (want 1 or 2)"),
            Self::StreamAlreadyRunning => write!(f, "stream already running"),
            Self::NoStream => write!(f, "no stream open"),
            Self::Realtime(e) => write!(f, "realtime setup failed: {e}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// Hotplug / device-state change (replaces `IMMNotificationClient`).
#[derive(Debug, Clone)]
pub enum HotplugEvent {
    DeviceAdded(AudioDevice),
    DeviceRemoved(DeviceUid),
    DefaultChanged { role: DeviceRole, uid: DeviceUid },
}

/// Real-time thread policy. Applied once at thread spawn (never on the
/// audio callback itself).
#[derive(Debug, Clone, Copy)]
pub struct RtPolicy {
    /// `SCHED_FIFO` priority (1–99). Default [`RT_PRIORITY`].
    pub priority: i32,
    /// Bytes to `mlockall`. Covers quantum buffers + graph state.
    pub memlock_bytes: usize,
}

impl Default for RtPolicy {
    fn default() -> Self {
        Self {
            priority: RT_PRIORITY,
            // 8 MiB default; raise for large graphs / long IRs.
            memlock_bytes: 8 << 20,
        }
    }
}

impl RtPolicy {
    /// Elevate the *current* thread. Call once per audio thread at startup.
    /// Mirrors `MmcssPriorityChanger`: pro-audio class, non-max priority.
    pub fn apply_current_thread(&self) -> Result<(), AudioError> {
        // SAFETY: trivial libc wrappers with valid args; no RT allocation here.
        unsafe {
            // Lock current + future mappings so the callback never page-faults.
            if libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) != 0 {
                return Err(AudioError::Realtime("mlockall failed (raise memlock limit)".into()));
            }
            let param = libc::sched_param {
                sched_priority: self.priority,
            };
            if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) != 0 {
                return Err(AudioError::Realtime(
                    "sched_setscheduler(SCHED_FIFO) failed (need rtprio or root)".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Link state of a live streaming session (hotplug/xrun driven).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamLinkState {
    #[default]
    Down,
    Streaming,
    /// A bound device vanished or a stream errored: reopen required.
    NeedsRestart,
}

/// Real-time-safe DSP entry called from the audio callback thread.
///
/// Constraints (same as the reference callback): no blocking, no
/// allocation, no syscalls. Contended locks must be skipped, never waited.
pub trait StreamProcessor: Send {
    fn process(&mut self, rate: u32, inputs: &[&[f32]], outputs: &mut [&mut [f32]]);
}

/// Audio backend: enumerate → open(topology) → start/stop, with hotplug.
pub trait AudioBackend: Send {    fn enumerate(&mut self) -> Result<Vec<AudioDevice>, AudioError>;
    fn open(&mut self, topology: &StreamTopology) -> Result<(), AudioError>;
    fn start(&mut self) -> Result<(), AudioError>;
    fn stop(&mut self, reason: StreamStopReason) -> Result<(), AudioError>;
    fn stats(&self, kind: StreamKind) -> CallbackStats;
    /// Non-blocking hotplug poll. Backends overlaying async runtimes
    /// should forward events here.
    fn poll_hotplug(&mut self) -> Vec<HotplugEvent>;

    /// Current link state. Defaults to `Down` (stub behaviour).
    fn link_state(&self) -> StreamLinkState {
        StreamLinkState::Down
    }

    /// Install the RT DSP entry (used by live backends on `open`).
    /// Default: ignored (stub behaviour).
    fn set_processor(&mut self, _processor: std::sync::Arc<std::sync::Mutex<dyn StreamProcessor>>) {
    }

    /// Create the virtual source if missing; returns its uid either way.
    /// Default: backends without a creatable virtual device error out
    /// (the stub reports its built-in one instead).
    fn ensure_virtual_source(&mut self) -> Result<DeviceUid, AudioError> {
        Err(AudioError::Backend("no virtual source support".into()))
    }

    /// Negotiated formats: (capture rate/ch, output rate/ch).
    /// Defaults to the canonical spec (stub behaviour).
    fn negotiated(&self) -> (u32, usize, u32, usize) {
        (
            crate::SAMPLE_RATE,
            crate::MAX_CHANNELS,
            crate::SAMPLE_RATE,
            crate::MAX_CHANNELS,
        )
    }
}
