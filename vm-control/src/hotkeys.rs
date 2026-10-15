//! Global hotkeys via Linux `evdev` (replaces RawInput).
//!
//! Model mirrors `KeyBindsService`: per-target binds, press/release
//! trigger + edit capture, persisted by the caller.
//! NOTE: reading `/dev/input/event*` needs the `input` group or an
//! equivalent udev rule — document at install time (Fase 3b).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// What a hotkey triggers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HotkeyTarget {
    ToggleVoiceChanger,
    ToggleMute,
    PlaySound(String),
    SelectVoice(String),
    StopAllSounds,
}

/// A user keybind: evdev key codes + target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keybind {
    /// Linux evdev `KEY_*` codes that must be held together.
    pub codes: Vec<u16>,
    pub target: HotkeyTarget,
    pub enabled: bool,
}

/// In-memory keybind store. Persistence (JSON next to presets) lands
/// with the settings work in Fase 3b.
#[derive(Debug, Default)]
pub struct HotkeyManager {
    binds: HashMap<u32, Keybind>,
    next_id: u32,
}

impl HotkeyManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, bind: Keybind) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.binds.insert(id, bind);
        id
    }

    pub fn remove(&mut self, id: u32) -> bool {
        self.binds.remove(&id).is_some()
    }

    pub fn set_enabled(&mut self, id: u32, enabled: bool) -> bool {
        match self.binds.get_mut(&id) {
            Some(b) => {
                b.enabled = enabled;
                true
            }
            None => false,
        }
    }

    pub fn list(&self) -> Vec<(u32, Keybind)> {
        self.binds.iter().map(|(id, b)| (*id, b.clone())).collect()
    }

    /// Match currently-held codes against enabled binds.
    pub fn match_held(&self, held: &[u16]) -> Vec<HotkeyTarget> {
        self.binds
            .values()
            .filter(|b| b.enabled && !b.codes.is_empty() && b.codes.iter().all(|c| held.contains(c)))
            .map(|b| b.target.clone())
            .collect()
    }

    /// List readable evdev keyboard devices (best-effort; empty without
    /// input permissions — see module docs).
    pub fn scan_keyboards() -> Vec<String> {
        let mut out = Vec::new();
        for n in 0..32 {
            let path = format!("/dev/input/event{n}");
            match evdev::Device::open(&path) {
                Ok(dev) => {
                    let name = dev.name().unwrap_or("?").to_string();
                    // Heuristic: keyboards expose KEY_A.
                    if dev.supported_keys().map(|k| k.contains(evdev::Key::KEY_A)).unwrap_or(false) {
                        out.push(format!("{path} ({name})"));
                    }
                }
                Err(_) => continue,
            }
        }
        out
    }
}

/// Blocking evdev read loop for one keyboard device. Runs on its own
/// thread; calls `on_trigger` on press edges whose held set matches an
/// enabled bind. Best-effort: returns if the device can't be opened
/// (permissions) or disappears.
pub fn watch_device(path: String, manager: std::sync::Arc<std::sync::Mutex<HotkeyManager>>, on_trigger: impl Fn(HotkeyTarget) + Send + 'static) {
    use std::collections::HashSet;
    let mut dev = match evdev::Device::open(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hotkeys: cannot open {path}: {e}");
            return;
        }
    };
    let mut held: HashSet<u16> = HashSet::new();
    loop {
        let batch = match dev.fetch_events() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("hotkeys: {path} read error: {e}");
                return;
            }
        };
        for ev in batch {
            if ev.event_type() != evdev::EventType::KEY {
                continue;
            }
            let code = ev.code();
            match ev.value() {
                1 => {
                    held.insert(code);
                    let held_vec: Vec<u16> = held.iter().copied().collect();
                    if let Ok(mgr) = manager.lock() {
                        for target in mgr.match_held(&held_vec) {
                            on_trigger(target);
                        }
                    }
                }
                0 => {
                    held.remove(&code);
                }
                _ => {}
            }
        }
    }
}
