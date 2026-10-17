//! `vm-control`: local Control API (WebSocket) + global hotkeys.
//!
//! Mirrors the ledger (Fase 2, item ③) with original code:
//! - JSON action-string dispatch (same *shape* as the reference bridge).
//! - Bind on all interfaces with a runtime port scan + client auth hook.
//! - Hotkeys via Linux `evdev` (replaces RawInput).

pub mod actions;
pub mod autostart;
pub mod hotkeys;
pub mod server;
pub mod settings;

pub use actions::{Action, ControlEvent, ControlHandler, ControlMessage, ControlReply, ALL_ACTIONS};
pub use hotkeys::{
    HotkeyManager, HotkeyTarget, Keybind, StoredBind, DaemonConfig, CONFIG_VERSION, is_momentary,
    load_daemon_config, save_daemon_config, sandbox_note, validate_bind,
};
pub use server::{dispatch, ControlApiServer, ServerConfig};
pub use settings::{
    DuckingConfig, MidiBinding, MidiConfig, MidiEvent, MidiKind, MidiTarget, MonitorConfig,
    ObsConfig, PassthroughConfig, ReplayConfig,
};
