//! `vm-control`: local Control API (WebSocket) + global hotkeys.
//!
//! Mirrors the ledger (Fase 2, item ③) with original code:
//! - JSON action-string dispatch (same *shape* as the reference bridge).
//! - Bind on all interfaces with a runtime port scan + client auth hook.
//! - Hotkeys via Linux `evdev` (replaces RawInput).

pub mod actions;
pub mod hotkeys;
pub mod server;

pub use actions::{Action, ControlEvent, ControlHandler, ControlMessage, ControlReply};
pub use hotkeys::{HotkeyManager, HotkeyTarget, Keybind};
pub use server::{dispatch, ControlApiServer, ServerConfig};
