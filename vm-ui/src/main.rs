//! `vm-ui`: native egui frontend — a WebSocket client of the Control API.
//!
//! The UI owns no audio: presets come from `~/.config/vm/presets` (parsed
//! with `vm-preset` for ranges), everything else flows through actions.
//!
//! The window is disposable: closing it (WM "X" or the tray's Hide) destroys
//! it while the process — WS link, tray, child daemon — keeps running, and
//! the tray's Show builds a fresh one. This is also the only true "hide" on
//! Wayland, where winit's `Window::set_visible` is a documented no-op.

mod eq;
mod link;
mod tray;
mod wizard;

use eframe::egui;
use link::{Snapshot, UiLink};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tray::{Tray, TrayAction};

/// Where the WS link thread wakes the UI: the current window's context.
/// Windows are disposable, so `VmApp::new` points this slot at the fresh
/// context on every run and `main` clears it when the run ends.
type Waker = Arc<Mutex<Option<egui::Context>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Voices,
    Soundboard,
    Devices,
    Integrations,
    Health,
}

struct VmApp<'a> {
    /// The process-wide WS link (owned by `main`; windows are disposable).
    link: &'a UiLink,
    snap: Arc<Mutex<Snapshot>>,
    tab: Tab,
    presets: Vec<String>,
    preset: Option<vm_preset::Preset>,
    /// Local param mirrors for immediate-mode widgets.
    sliders: HashMap<(String, String), f32>,
    toggles: HashMap<(String, String), bool>,
    ints: HashMap<(String, String), i32>,
    texts: HashMap<(String, String), String>,
    master: f32,
    input_sel: String,
    /// Daemon launched from the UI, if any. `main` owns it: a re-opened
    /// window still finds the handle for the Stop button.
    daemon_child: &'a mut Option<std::process::Child>,
    status_msg: String,
    /// System tray, owned by `main` and borrowed while a window exists
    /// (None = window-only: sandbox or no SNI host).
    tray: &'a Option<Tray>,
    /// Last synced (visible, muted) — menu updates only on change. This
    /// struct only exists while its window does, so visible is always true.
    tray_sync: (bool, bool),
    /// EQ canvas bands (rebuilt on preset load; values refreshed from the
    /// slider mirrors every frame).
    eq_bands: Vec<eq::EqBand>,
    eq_selected: Option<usize>,
    /// First-run setup wizard state machine.
    wizard: wizard::Wizard,
    /// Fast level polling currently requested (mirrors `wizard.open`).
    wizard_fast: bool,
    /// Preferences dialog (F-0.4.1.B): XDG autostart toggle + daemon
    /// unit state, backed by `vm_control::autostart` — the same code
    /// `vm autostart` runs on the CLI.
    prefs_open: bool,
    prefs_status: Option<vm_control::autostart::Status>,
    /// Last enable/disable outcome: (is_error, text).
    prefs_note: Option<(bool, String)>,
    /// About dialog (F-0.4.1.C): identity, clean-room note, license.
    about_open: bool,
    /// F-0.4.2.B: signature of the last rendered monitor state. When the
    /// daemon reports new values (hotplug fallback, ToggleMonitor hotkey,
    /// `vm call`), the monitor mirrors are dropped so the Hear Myself
    /// card follows the truth instead of stale local edits.
    mon_seen: Option<(bool, String, u32, u32, bool)>,
}

impl<'a> VmApp<'a> {
    fn new(
        cc: &eframe::CreationContext<'_>,
        link: &'a UiLink,
        tray: &'a Option<Tray>,
        daemon_child: &'a mut Option<std::process::Child>,
        waker: &Waker,
    ) -> Self {
        // Point the link's repaint hook at this window's context. `main`
        // clears it when the run ends: between windows there is nothing to
        // repaint and the tray wait loop polls on its own.
        *waker.lock().unwrap() = Some(cc.egui_ctx.clone());
        let snap = link.snapshot.clone();
        let first = snap.lock().map(|s| s.clone()).unwrap_or_default();
        let mut app = Self {
            link,
            snap,
            tab: Tab::Voices,
            presets: Vec::new(),
            preset: None,
            sliders: HashMap::new(),
            toggles: HashMap::new(),
            ints: HashMap::new(),
            texts: HashMap::new(),
            master: 1000.0,
            input_sel: String::new(),
            daemon_child,
            // A re-opened window usually lands on a live link: don't show
            // "connecting…" when we already are.
            status_msg: if first.connected {
                "connected".into()
            } else {
                "connecting…".into()
            },
            tray,
            tray_sync: (true, false),
            eq_bands: Vec::new(),
            eq_selected: None,
            wizard: wizard::Wizard::first_run(&Self::config_dir()),
            wizard_fast: false,
            prefs_open: false,
            prefs_status: None,
            prefs_note: None,
            about_open: false,
            mon_seen: None,
        };
        app.refresh_presets();
        app
    }

    fn preset_dir() -> std::path::PathBuf {
        std::env::var("VM_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("XDG_CONFIG_HOME")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config"))
                    })
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                    .join("vm")
            })
            .join("presets")
    }

    /// Config dir (`<cfg>/vm`) — wizard marker + replays live here.
    fn config_dir() -> std::path::PathBuf {
        Self::preset_dir()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/vm"))
    }

    // -- Mirror accessors (widget-local values shadow the snapshot) -------

    fn m_bool(&self, node: &str, param: &str, d: bool) -> bool {
        *self
            .toggles
            .get(&(node.to_string(), param.to_string()))
            .unwrap_or(&d)
    }

    fn m_f32(&self, node: &str, param: &str, d: f32) -> f32 {
        *self
            .sliders
            .get(&(node.to_string(), param.to_string()))
            .unwrap_or(&d)
    }

    fn m_text(&self, node: &str, param: &str, d: &str) -> String {
        self.texts
            .get(&(node.to_string(), param.to_string()))
            .cloned()
            .unwrap_or_else(|| d.to_string())
    }

    fn set_f32(&mut self, node: &str, param: &str, v: f32) {
        self.sliders.insert((node.to_string(), param.to_string()), v);
    }

    fn set_bool(&mut self, node: &str, param: &str, v: bool) {
        self.toggles.insert((node.to_string(), param.to_string()), v);
    }

    // -- Integration action senders (full state from mirrors + snapshot) --

    fn send_monitor(&self, snap: &Snapshot) {
        self.act(serde_json::json!({
            "action": "setMonitor",
            "enabled": self.m_bool("monitor", "enabled", snap.monitor.enabled),
            "device": self.m_text("monitor", "device", &snap.monitor.device),
            "mix": self.m_f32("monitor", "mix", snap.monitor.mix),
            "volume_db": self.m_f32("monitor", "volume_db", snap.monitor.volume_db),
            "muted": self.m_bool("monitor", "muted", snap.monitor.muted),
        }));
    }

    fn send_ducking(&self, snap: &Snapshot) {
        self.act(serde_json::json!({
            "action": "setDucking",
            "enabled": self.m_bool("ducking", "enabled", snap.ducking.enabled),
            "depth_db": self.m_f32("ducking", "depth_db", snap.ducking.depth_db),
            "threshold_db": self.m_f32("ducking", "threshold_db", snap.ducking.threshold_db),
            "attack_ms": self.m_f32("ducking", "attack_ms", snap.ducking.attack_ms),
            "release_ms": self.m_f32("ducking", "release_ms", snap.ducking.release_ms),
        }));
    }

    fn send_replay(&self, snap: &Snapshot) {
        let seconds = *self
            .ints
            .get(&("replay".to_string(), "seconds".to_string()))
            .unwrap_or(&(snap.replay.seconds as i32));
        self.act(serde_json::json!({
            "action": "setInstantReplay",
            "enabled": self.m_bool("replay", "enabled", snap.replay.enabled),
            "seconds": seconds.max(5) as u32,
        }));
    }

    fn send_passthrough(&self, snap: &Snapshot) {
        self.act(serde_json::json!({
            "action": "setPassthrough",
            "enabled": self.m_bool("passthrough", "enabled", snap.passthrough.enabled),
            "device": self.m_text("passthrough", "device", &snap.passthrough.device),
            "app": self.m_text("passthrough", "app", &snap.passthrough.app),
        }));
    }

    fn send_obs(&self, snap: &Snapshot, password: Option<String>) {
        let mut v = serde_json::json!({
            "action": "setObs",
            "enabled": self.m_bool("obs", "enabled", false),
            "url": self.m_text("obs", "url", &snap.obs.url),
        });
        if let (Some(p), Some(o)) = (password, v.as_object_mut()) {
            o.insert("password".into(), serde_json::Value::String(p));
        }
        self.act(v);
    }

    fn send_midi(&self, snap: &Snapshot) {
        self.act(serde_json::json!({
            "action": "setMidi",
            "enabled": self.m_bool("midi", "enabled", !snap.midi.port.is_empty()),
            "port": self.m_text("midi", "port", &snap.midi.port),
        }));
    }

    // -- Current-preset lookups (wizard + EQ target the live preset) ------

    /// First node carrying `param` and none of `exclude`.
    fn node_with_param(&self, param: &str, exclude: &[&str]) -> Option<String> {
        let p = self.preset.as_ref()?;
        p.nodes
            .iter()
            .find(|n| {
                n.params.contains_key(param)
                    && !exclude.iter().any(|e| n.params.contains_key(*e))
            })
            .map(|n| n.id.clone())
    }

    fn param_range(&self, node: &str, param: &str) -> Option<(f32, f32)> {
        let p = self.preset.as_ref()?;
        let n = p.nodes.iter().find(|n| n.id == node)?;
        n.params.get(param)?.range.map(|r| (r[0] as f32, r[1] as f32))
    }

    fn gate_node(&self) -> Option<String> {
        self.node_with_param("hysteresis_db", &[])
    }

    fn comp_node(&self) -> Option<String> {
        self.node_with_param("makeup_db", &[])
    }

    fn limit_node(&self) -> Option<String> {
        self.node_with_param("release_ms", &["hysteresis_db", "ratio", "freq_hz"])
    }

    fn denoise_node(&self) -> Option<String> {
        self.node_with_param("reduction_db", &[])
    }

    /// Apply one live voice parameter (mirror + Control API).
    fn apply_param(&mut self, node: &str, param: &str, value: f32) {
        self.set_f32(node, param, value);
        self.act(serde_json::json!({
            "action": "setVoiceParameter",
            "param": format!("{node}.{param}"),
            "value": value,
        }));
    }

    fn refresh_presets(&mut self) {
        self.presets.clear();
        if let Ok(entries) = std::fs::read_dir(Self::preset_dir()) {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()))
                .collect();
            names.sort();
            self.presets = names;
        }
    }

    fn load_preset(&mut self, name: &str) {
        let path = Self::preset_dir().join(format!("{name}.json"));
        match std::fs::read_to_string(&path).ok().and_then(|t| vm_preset::load(&t).ok()) {
            Some(p) => {
                self.sliders.clear();
                self.toggles.clear();
                self.ints.clear();
                self.texts.clear();
                for node in &p.nodes {
                    for (pname, pspec) in &node.params {
                        let key = (node.id.clone(), pname.clone());
                        match &pspec.value {
                            vm_preset::JsonParamValue::Float(v) => {
                                self.sliders.insert(key, *v as f32);
                            }
                            vm_preset::JsonParamValue::Bool(v) => {
                                self.toggles.insert(key, *v);
                            }
                            vm_preset::JsonParamValue::Int(v) => {
                                self.ints.insert(key, *v as i32);
                            }
                            vm_preset::JsonParamValue::Text(v) => {
                                self.texts.insert(key, v.clone());
                            }
                            // Saved noise profile: machine data, not
                            // hand-editable (capture via Control API).
                            vm_preset::JsonParamValue::FloatArray(_) => {}
                        }
                    }
                }
                self.preset = Some(p);
                self.eq_bands = self
                    .preset
                    .as_ref()
                    .map(eq::bands_from_preset)
                    .unwrap_or_default();
                self.eq_selected = None;
                self.status_msg.clear();
            }
            None => self.status_msg = format!("cannot load preset '{name}'"),
        }
    }

    fn act(&self, action: serde_json::Value) {
        self.link.send_action(action);
    }

    fn daemon_path() -> Option<std::path::PathBuf> {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let cand = dir.join("vm-daemon");
                if cand.exists() {
                    return Some(cand);
                }
            }
        }
        None
    }
}

impl eframe::App for VmApp<'_> {
    /// Runs before every `ui` pass. While this window exists, the tray
    /// actions (hide / mute / quit) and the smoke hooks land here. Closing
    /// the window — WM "X" or the tray's Hide — needs no interception: the
    /// run just ends and `main` picks up with the link and tray still alive.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = std::time::Instant::now();
        if smoke().close_due(now) {
            // Same path as the WM "X": close; `main` decides the fate.
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
        if smoke().quit_due(now) {
            QUIT.store(true, Ordering::Relaxed);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }

        // Tray: drain menu/click actions, then reflect state in the menu
        // (only on change — each sync is an SNI round-trip).
        let snap = self.snap.lock().map(|s| s.clone()).unwrap_or_default();
        if let Some(tray) = self.tray.as_ref() {
            for action in tray.poll() {
                match action {
                    // "Hide" closes the window; link + tray stay up in
                    // `main`, and "Show" opens a fresh window.
                    TrayAction::ToggleVisibility => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    TrayAction::ToggleMute => {
                        self.act(serde_json::json!({
                            "action": "setMuted", "muted": !snap.muted,
                        }));
                    }
                    TrayAction::Quit => {
                        QUIT.store(true, Ordering::Relaxed);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
            if (true, snap.muted) != self.tray_sync {
                tray.sync(true, snap.muted);
                self.tray_sync = (true, snap.muted);
            }
        }

        // Tray channels never wake egui and an idle window gets no input
        // events either, so keep a slow self-tick while a tray is live:
        // 500 ms keeps menu clicks (hide / mute / quit) prompt. With no
        // window there is no loop to tick — the wait in `main` polls itself.
        if self.tray.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self.snap.lock().map(|s| s.clone()).unwrap_or_default();

        // F-0.4.2.B: the monitor is mutable from outside this window
        // (hotplug fallback, ToggleMonitor hotkey, `vm call`). When the
        // daemon reports values different from the last frame, drop the
        // widget mirrors so the card shows the daemon's truth.
        let mon_sig = (
            snap.monitor.enabled,
            snap.monitor.device.clone(),
            snap.monitor.mix.to_bits(),
            snap.monitor.volume_db.to_bits(),
            snap.monitor.muted,
        );
        if self.mon_seen.as_ref() != Some(&mon_sig) {
            if self.mon_seen.is_some() {
                for param in ["enabled", "device", "mix", "volume_db", "muted"] {
                    let key = ("monitor".to_string(), param.to_string());
                    self.sliders.remove(&key);
                    self.toggles.remove(&key);
                    self.texts.remove(&key);
                }
            }
            self.mon_seen = Some(mon_sig);
        }

        // Keep local preset selection in sync with the daemon voice. On a
        // fresh connect `selected_voice` (event-driven) is still empty
        // until the first voiceChanged, but `Health.voice` (status poll)
        // is already populated — fall back to it so the tab never shows
        // "select a voice preset" while a voice is live.
        let daemon_voice: &str = if snap.selected_voice.is_empty() {
            snap.health.voice.as_str()
        } else {
            snap.selected_voice.as_str()
        };
        if !daemon_voice.is_empty()
            && self.preset.as_ref().map(|p| p.name.as_str()) != Some(daemon_voice)
            && self.presets.iter().any(|v| v == daemon_voice)
        {
            self.load_preset(daemon_voice);
        }

        egui::Panel::top("top").show(ui, |ui| {
            if snap.connected && self.status_msg == "connecting…" {
                self.status_msg.clear();
            }
            ui.horizontal(|ui| {
                if snap.connected {
                    ui.colored_label(egui::Color32::GREEN, "connected");
                } else {
                    ui.colored_label(egui::Color32::RED, "offline");
                }
                ui.label(format!(":{}", snap.port));
                ui.separator();
                for (tab, name) in [(Tab::Voices, "Voices"), (Tab::Soundboard, "Soundboard"), (Tab::Devices, "Devices"), (Tab::Integrations, "Integrations"), (Tab::Health, "Health")] {
                    ui.selectable_value(&mut self.tab, tab, name);
                }
                ui.separator();
                if ui.button("Setup wizard").clicked() {
                    self.wizard.open = true;
                }
                if ui.button("⚙ Preferences").clicked() {
                    // Load fresh state right when the dialog opens (the
                    // CLI may have changed things since boot).
                    self.prefs_open = true;
                    self.prefs_status = Some(vm_control::autostart::status());
                    self.prefs_note = None;
                }
                if ui.button("About").clicked() {
                    self.about_open = true;
                }
                ui.separator();
                if self.daemon_child.is_some() {
                    if ui.button("Stop daemon").clicked() {
                        if let Some(mut child) = self.daemon_child.take() {
                            let _ = child.kill();
                        }
                    }
                } else if snap.connected {
                    // A daemon answers on the Control API — it may be managed
                    // externally (systemd --user). Never spawn a second one.
                    ui.colored_label(egui::Color32::GRAY, "daemon running");
                } else if ui.button("Launch daemon").clicked() {
                    match Self::daemon_path() {
                        Some(path) => match std::process::Command::new(path).arg("--live").spawn() {
                            Ok(child) => {
                                *self.daemon_child = Some(child);
                                self.status_msg = "daemon launched".to_string();
                            }
                            Err(e) => self.status_msg = format!("launch failed: {e}"),
                        },
                        None => self.status_msg = "vm-daemon not found next to vm-ui".to_string(),
                    }
                }
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "link={} xruns={} avg={:.0}µs",
                    snap.status.link, snap.status.xruns, snap.status.avg_us
                ));
                ui.separator();
                if !snap.last_error.is_empty() {
                    ui.colored_label(egui::Color32::RED, &snap.last_error);
                } else if !self.status_msg.is_empty() {
                    ui.label(&self.status_msg);
                } else {
                    ui.label(format!("voice: {} ({} tier)", snap.selected_voice, snap.tier));
                }
            });
        });

        ui.spacing_mut().slider_width = 220.0;
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Voices => self.voices_tab(ui, &snap),
            Tab::Soundboard => self.soundboard_tab(ui, &snap),
            Tab::Devices => self.devices_tab(ui, &snap),
            Tab::Integrations => self.integrations_tab(ui, &snap),
            Tab::Health => self.health_tab(ui, &snap),
        });

        // First-run wizard: modal-ish floating window; fast level polling
        // only while it is open (the meters need ~100 ms updates).
        if self.wizard.open != self.wizard_fast {
            self.link.set_fast_levels(self.wizard.open);
            self.wizard_fast = self.wizard.open;
        }
        if self.wizard.open {
            let dt = ui.ctx().input(|i| i.stable_dt).min(0.5);
            self.wizard.feed(snap.levels.mic_db, dt);
            let ctx = ui.ctx().clone();
            self.wizard_ui(&ctx, &snap);
        }

        // Preferences dialog (F-0.4.1.B) — independent of the daemon
        // link: autostart is a local file + systemctl --user operation.
        if self.prefs_open {
            let ctx = ui.ctx().clone();
            self.prefs_ui(&ctx);
        }

        // About dialog (F-0.4.1.C): identity + clean-room note; static.
        if self.about_open {
            let ctx = ui.ctx().clone();
            self.about_ui(&ctx);
        }
    }
}

impl VmApp<'_> {
    /// F-0.4.2.B: "Hear Myself" — the first thing on the Voices tab.
    /// Hearing your own voice with the effects live is the moment the
    /// voice changer stops being abstract; everything else can wait.
    fn hear_myself_card(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        // Physical outputs only, mirroring the backend policy: the vm-*
        // namespace is ours (monitoring into it would double-deliver the
        // voice into the graph). Filtered on uid (raw node name) since
        // `name` is the friendly description.
        let sinks: Vec<&link::DeviceInfo> = snap
            .devices
            .iter()
            .filter(|d| d.role == "Sink" && !d.uid.starts_with("vm-"))
            .collect();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            let mut en = self.m_bool("monitor", "enabled", snap.monitor.enabled);
            ui.horizontal(|ui| {
                let label = if en { "Hear Myself — ON" } else { "Hear Myself — OFF" };
                if ui
                    .toggle_value(&mut en, egui::RichText::new(label).size(16.0).strong())
                    .changed()
                {
                    self.set_bool("monitor", "enabled", en);
                    self.send_monitor(snap);
                }
                if en {
                    let muted = self.m_bool("monitor", "muted", snap.monitor.muted);
                    let mlabel = if muted { "unmute" } else { "mute" };
                    if ui.button(mlabel).clicked() {
                        self.set_bool("monitor", "muted", !muted);
                        self.send_monitor(snap);
                    }
                }
                ui.weak(if en {
                    "live — you hear your voice with the current effects"
                } else {
                    "hear your own voice through your headphones or speakers"
                });
            });
            let cur = self.m_text("monitor", "device", &snap.monitor.device);
            let shown = if cur.is_empty() {
                "(system default)".to_string()
            } else {
                sinks
                    .iter()
                    .find(|d| d.uid == cur)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "(device not connected)".to_string())
            };
            let mut chosen: Option<String> = None;
            ui.horizontal(|ui| {
                ui.label("Output:");
                egui::ComboBox::from_id_salt("monitor_device")
                    .selected_text(shown)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(cur.is_empty(), "(system default)")
                            .clicked()
                        {
                            chosen = Some(String::new());
                        }
                        for d in &sinks {
                            if ui.selectable_label(cur == d.uid, &d.name).clicked() {
                                chosen = Some(d.uid.clone());
                            }
                        }
                    });
            });
            if let Some(c) = chosen {
                self.texts
                    .insert(("monitor".to_string(), "device".to_string()), c);
                self.send_monitor(snap);
            }
            let mut vol = self.m_f32("monitor", "volume_db", snap.monitor.volume_db);
            let r = ui.add(egui::Slider::new(&mut vol, -40.0..=12.0).text("return volume (dB)"));
            self.set_f32("monitor", "volume_db", vol);
            if r.drag_stopped() {
                self.send_monitor(snap);
            }
            let mix_custom = self.m_f32("monitor", "mix", snap.monitor.mix) < 1.0;
            egui::CollapsingHeader::new("Advanced")
                .default_open(mix_custom)
                .show(ui, |ui| {
                    let mut mix = self.m_f32("monitor", "mix", snap.monitor.mix);
                    let r =
                        ui.add(egui::Slider::new(&mut mix, 0.0..=1.0).text("dry « blend » wet"));
                    self.set_f32("monitor", "mix", mix);
                    if r.drag_stopped() {
                        self.send_monitor(snap);
                    }
                    ui.weak("0.0 = raw mic (dry, lowest latency) · 1.0 = processed voice (wet)");
                    ui.weak(
                        "Tip: use headphones — with speakers, the mic can pick the \
                         monitor up again and echo (acoustic feedback).",
                    );
                });
        });
    }

    fn voices_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        self.hear_myself_card(ui, snap);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Voice changer:");
            let mut enabled = snap.voice_enabled;
            if ui.checkbox(&mut enabled, "enabled").changed() {
                self.act(serde_json::json!({"action": "toggleVoiceChanger", "enabled": enabled}));
            }
            if ui.button("↻ presets").clicked() {
                self.refresh_presets();
            }
        });
        ui.separator();
        egui::Panel::left("voices").show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if self.presets.is_empty() {
                    ui.label("no presets found — click ↻ presets");
                }
                for name in self.presets.clone() {
                    let selected = self.preset.as_ref().map(|p| p.name.as_str()) == Some(name.as_str());
                    if ui.selectable_label(selected, &name).clicked() {
                        self.act(serde_json::json!({"action": "selectVoice", "voice": name}));
                        self.load_preset(&name);
                    }
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.eq_section(ui);
                let mut pending: Vec<(String, String, serde_json::Value)> = Vec::new();
                if let Some(preset) = &self.preset {
                    if let Some(desc) = preset.description.as_deref() {
                        ui.label(desc);
                        ui.separator();
                    }
                    for node in &preset.nodes {
                        egui::CollapsingHeader::new(&node.id).default_open(true).show(ui, |ui| {
                            for (pname, pspec) in &node.params {
                                let vm_preset::ParamSpec { value, range, public, .. } = pspec;
                                if !public {
                                    continue;
                                }
                                match (value, range) {
                                    (vm_preset::JsonParamValue::Float(_), Some([min, max, _])) => {
                                        let key = (node.id.clone(), pname.clone());
                                        let mut v = *self.sliders.get(&key).unwrap_or(&0.0);
                                        let resp = ui.add(
                                            egui::Slider::new(&mut v, *min as f32..=*max as f32)
                                                .text(pname.as_str()),
                                        );
                                        if resp.changed() {
                                            self.sliders.insert(key.clone(), v);
                                            pending.push((
                                                node.id.clone(),
                                                pname.clone(),
                                                serde_json::Value::from(v),
                                            ));
                                        }
                                    }
                                    (vm_preset::JsonParamValue::Float(_), None) => {
                                        let key = (node.id.clone(), pname.clone());
                                        let mut v = *self.sliders.get(&key).unwrap_or(&0.0);
                                        let resp = ui.add(
                                            egui::DragValue::new(&mut v)
                                                .prefix(format!("{pname}: "))
                                                .speed(0.01),
                                        );
                                        if resp.changed() {
                                            self.sliders.insert(key.clone(), v);
                                            pending.push((
                                                node.id.clone(),
                                                pname.clone(),
                                                serde_json::Value::from(v),
                                            ));
                                        }
                                    }
                                    (vm_preset::JsonParamValue::Bool(_), _) => {
                                        let key = (node.id.clone(), pname.clone());
                                        let mut v = *self.toggles.get(&key).unwrap_or(&false);
                                        if ui.checkbox(&mut v, pname.as_str()).changed() {
                                            self.toggles.insert(key.clone(), v);
                                            pending.push((
                                                node.id.clone(),
                                                pname.clone(),
                                                serde_json::Value::Bool(v),
                                            ));
                                        }
                                    }
                                    (vm_preset::JsonParamValue::Int(_), _) => {
                                        let key = (node.id.clone(), pname.clone());
                                        let mut v = *self.ints.get(&key).unwrap_or(&0);
                                        if ui
                                            .add(egui::DragValue::new(&mut v).prefix(format!("{pname}: ")))
                                            .changed()
                                        {
                                            self.ints.insert(key.clone(), v);
                                            pending.push((
                                                node.id.clone(),
                                                pname.clone(),
                                                serde_json::Value::from(v),
                                            ));
                                        }
                                    }
                                    (vm_preset::JsonParamValue::Text(_), _) => {
                                        let key = (node.id.clone(), pname.clone());
                                        let mut v =
                                            self.texts.get(&key).cloned().unwrap_or_default();
                                        let resp = ui.add(
                                            egui::TextEdit::singleline(&mut v).hint_text(pname.as_str()),
                                        );
                                        if resp.lost_focus()
                                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                        {
                                            self.texts.insert(key.clone(), v.clone());
                                            pending.push((
                                                node.id.clone(),
                                                pname.clone(),
                                                serde_json::Value::String(v),
                                            ));
                                        } else {
                                            self.texts.insert(key, v);
                                        }
                                    }
                                    (vm_preset::JsonParamValue::FloatArray(bins), _) => {
                                        ui.label(format!(
                                            "{pname}: {} bins stored (noise profile)",
                                            bins.len()
                                        ));
                                    }
                                }
                            }
                        });
                    }
                } else {
                    ui.label("select a voice preset");
                }
                for (node, param, v) in pending {
                    self.act(serde_json::json!({
                        "action": "setVoiceParameter",
                        "param": format!("{node}.{param}"),
                        "value": v,
                    }));
                }
            });
        });
    }

    fn soundboard_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.heading("Soundboard");
        ui.horizontal(|ui| {
            ui.label("Master:");
            let mut master = self.master;
            let resp = ui.add(egui::Slider::new(&mut master, 0.0..=1000.0));
            if resp.drag_stopped() {
                self.master = master;
                self.act(serde_json::json!({
                    "action": "setSoundboardMasterVolume",
                    "per_mille": master as u16,
                }));
            } else {
                self.master = master;
            }
            if ui.button("Stop all").clicked() {
                self.act(serde_json::json!({"action": "stopAllSounds"}));
            }
        });
        ui.separator();
        // Instant replay (F-0.4.B 20): rolling window of the final output.
        ui.horizontal(|ui| {
            let mut en = self.m_bool("replay", "enabled", snap.replay.enabled);
            if ui.checkbox(&mut en, "Instant replay").changed() {
                self.set_bool("replay", "enabled", en);
                self.send_replay(snap);
            }
            let key = ("replay".to_string(), "seconds".to_string());
            let mut secs = *self
                .ints
                .get(&key)
                .unwrap_or(&(snap.replay.seconds as i32));
            if ui
                .add(
                    egui::DragValue::new(&mut secs)
                        .range(5..=120)
                        .suffix(" s")
                        .speed(1.0),
                )
                .changed()
            {
                self.ints.insert(key, secs);
                self.send_replay(snap);
            }
            ui.label(format!("buffered {:.1}s", snap.replay.buffered_seconds));
            if ui.button("Save now").clicked() {
                self.act(serde_json::json!({"action": "saveInstantReplay"}));
            }
            if !snap.replay_saved_path.is_empty() {
                ui.label(format!("saved to {}", snap.replay_saved_path));
            }
        });
        // Per-application passthrough (F-0.4.B 18).
        ui.horizontal(|ui| {
            let mut en = self.m_bool("passthrough", "enabled", snap.passthrough.enabled);
            if ui.checkbox(&mut en, "Route app audio").changed() {
                self.set_bool("passthrough", "enabled", en);
                if en {
                    self.act(serde_json::json!({"action": "listPassthroughApps"}));
                }
                self.send_passthrough(snap);
            }
            if ui.button("↻ apps").clicked() {
                self.act(serde_json::json!({"action": "listPassthroughApps"}));
            }
            let mut apps: Vec<link::AppEntry> = snap.passthrough_apps.clone();
            apps.sort_by(|a, b| a.app.cmp(&b.app));
            apps.dedup_by(|a, b| a.app == b.app);
            let cur = self.m_text("passthrough", "app", &snap.passthrough.app);
            let mut chosen: Option<String> = None;
            ui.label("App");
            egui::ComboBox::from_id_salt("passthrough_app")
                .selected_text(if cur.is_empty() {
                    "(none)".to_string()
                } else {
                    cur.clone()
                })
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(cur.is_empty(), "(none — manual device)")
                        .clicked()
                    {
                        chosen = Some(String::new());
                    }
                    for a in &apps {
                        let label = if a.name.is_empty() {
                            format!("{} ({})", a.app, a.sink)
                        } else {
                            format!("{} ({})", a.app, a.name)
                        };
                        if ui.selectable_label(cur == a.app, &label).clicked() {
                            chosen = Some(a.app.clone());
                        }
                    }
                });
            if let Some(c) = chosen {
                self.texts
                    .insert(("passthrough".to_string(), "app".to_string()), c);
                self.send_passthrough(snap);
            }
            let cur_dev = self.m_text("passthrough", "device", &snap.passthrough.device);
            let sinks: Vec<&link::DeviceInfo> =
                snap.devices.iter().filter(|d| d.role == "Sink").collect();
            let shown_dev = sinks
                .iter()
                .find(|d| d.uid == cur_dev)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "(system default)".to_string());
            let mut chosen_dev: Option<String> = None;
            ui.label("to");
            egui::ComboBox::from_id_salt("passthrough_device")
                .selected_text(shown_dev)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(cur_dev.is_empty(), "(system default)")
                        .clicked()
                    {
                        chosen_dev = Some(String::new());
                    }
                    for d in &sinks {
                        if ui.selectable_label(cur_dev == d.uid, &d.name).clicked() {
                            chosen_dev = Some(d.uid.clone());
                        }
                    }
                });
            if let Some(c) = chosen_dev {
                self.texts
                    .insert(("passthrough".to_string(), "device".to_string()), c);
                self.send_passthrough(snap);
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("sounds").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
                for (i, name) in snap.sounds.iter().enumerate() {
                    ui.vertical(|ui| {
                        if ui.button(name).clicked() {
                            self.act(serde_json::json!({"action": "playSound", "sound": name}));
                        }
                        ui.horizontal(|ui| {
                            if ui.small_button("fade").clicked() {
                                self.act(serde_json::json!({
                                    "action": "fadeSound", "sound": name, "fade_out_secs": 0.5,
                                }));
                            }
                            ui.label("to VAD+mon");
                        });
                    });
                    if i % 3 == 2 {
                        ui.end_row();
                    }
                }
            });
            if snap.sounds.is_empty() {
                ui.label("no sounds — drop .wav/.mp3/.ogg/.opus into ~/.config/vm/sounds");
            }
        });
    }

    fn devices_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        if snap.devices.is_empty() {
            ui.label("no devices yet — is the daemon running? (see the status bar below)");
            ui.separator();
        }
        ui.heading("Input");
        ui.add_space(4.0);
        let inputs: Vec<&link::DeviceInfo> =
            snap.devices.iter().filter(|d| d.role == "Input").collect();
        if self.input_sel.is_empty() {
            if let Some(first) = inputs.first() {
                self.input_sel = first.uid.clone();
            }
        }
        egui::ComboBox::from_label("Microphone").selected_text(
            inputs
                .iter()
                .find(|d| d.uid == self.input_sel)
                .map(|d| d.name.as_str())
                .unwrap_or("—"),
        )
        .show_ui(ui, |ui| {
            for d in &inputs {
                if ui.selectable_value(&mut self.input_sel, d.uid.clone(), &d.name).clicked() {
                    self.act(serde_json::json!({"action": "selectInput", "uid": d.uid}));
                }
            }
        });
        ui.separator();
        ui.heading("Output (sinks)");
        ui.label("Selectable targets for monitor + passthrough.");
        for d in snap.devices.iter().filter(|d| d.role == "Sink") {
            ui.label(format!("{} — {} ({}Hz {}ch)", d.name, d.uid, d.rate, d.channels));
        }
        ui.separator();
        ui.heading("Virtual + monitor");
        for d in snap
            .devices
            .iter()
            .filter(|d| matches!(d.role.as_str(), "Virtual" | "Monitor"))
        {
            ui.label(format!("{} — {} ({}Hz {}ch)", d.name, d.role, d.rate, d.channels));
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!(
                "streaming={} link={} xruns={}",
                snap.status.active, snap.status.link, snap.status.xruns
            ));
            if ui.button("Start").clicked() {
                self.act(serde_json::json!({"action": "startAudioStreaming"}));
            }
            if ui.button("Refresh").clicked() {
                self.link.refresh();
            }
        });
    }

    /// Interactive EQ (F-0.4.B item 26): drag bands on the canvas; the
    /// drawn curve is `vm-dsp`'s own biquad math.
    fn eq_section(&mut self, ui: &mut egui::Ui) {
        if self.eq_bands.is_empty() {
            return;
        }
        // Refresh band values from the slider mirrors (sliders may be the
        // writer, e.g. the generic param list below the canvas).
        for i in 0..self.eq_bands.len() {
            let (node, freq, q, gain, has_gain) = {
                let b = &self.eq_bands[i];
                (b.node.clone(), b.freq, b.q, b.gain_db, b.has_gain)
            };
            let nf = self.m_f32(&node, "freq_hz", freq);
            let nq = self.m_f32(&node, "q", q);
            let ng = if has_gain {
                self.m_f32(&node, "gain_db", gain)
            } else {
                gain
            };
            let b = &mut self.eq_bands[i];
            b.freq = nf;
            b.q = nq;
            b.gain_db = ng;
        }
        ui.label("EQ — drag a handle: horizontal = frequency, vertical = gain, double-click resets gain");
        let pending = eq::canvas(ui, &mut self.eq_bands, 48_000.0, &mut self.eq_selected);
        for (node, param, value) in pending {
            self.apply_param(&node, param, value);
        }
        if let Some(i) = self.eq_selected {
            let info = self
                .eq_bands
                .get(i)
                .map(|b| (b.node.clone(), b.freq, b.q, b.gain_db, b.has_gain));
            if let Some((node, freq, q, gain, has_gain)) = info {
                let (flo, fhi) = self
                    .param_range(&node, "freq_hz")
                    .unwrap_or((20.0, 20_000.0));
                let mut f = freq;
                if ui
                    .add(egui::Slider::new(&mut f, flo..=fhi).logarithmic(true).text("freq (Hz)"))
                    .drag_stopped()
                {
                    self.apply_param(&node, "freq_hz", f);
                }
                {
                    let (qlo, qhi) = self.param_range(&node, "q").unwrap_or((0.1, 18.0));
                    let mut qv = q;
                    if ui
                        .add(egui::Slider::new(&mut qv, qlo..=qhi).text("Q"))
                        .drag_stopped()
                    {
                        self.apply_param(&node, "q", qv);
                    }
                }
                if has_gain {
                    let (glo, ghi) = self.param_range(&node, "gain_db").unwrap_or((-24.0, 24.0));
                    let mut g = gain;
                    if ui
                        .add(egui::Slider::new(&mut g, glo..=ghi).text("gain (dB)"))
                        .drag_stopped()
                    {
                        self.apply_param(&node, "gain_db", g);
                    }
                }
            }
        }
        ui.separator();
    }

    fn integrations_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        // The monitor card lives on the Voices tab since F-0.4.2.B (top
        // of the tab, where it is actually discovered); this tab keeps
        // the routing/bridge extras.
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Ducking — lower sounds while you speak");
            ui.label("Detects your voice on the raw mic and dips the soundboard for the effect.");
            let mut en = self.m_bool("ducking", "enabled", snap.ducking.enabled);
            if ui.checkbox(&mut en, "enabled").changed() {
                self.set_bool("ducking", "enabled", en);
                self.send_ducking(snap);
            }
            for (param, label, lo, hi, def) in [
                ("depth_db", "depth (dB)", 0.0f32, 30.0, snap.ducking.depth_db),
                ("threshold_db", "threshold (dBFS)", -60.0, 0.0, snap.ducking.threshold_db),
                ("attack_ms", "attack (ms)", 1.0, 500.0, snap.ducking.attack_ms),
                ("release_ms", "release (ms)", 10.0, 2000.0, snap.ducking.release_ms),
            ] {
                let mut v = self.m_f32("ducking", param, def);
                let r = ui.add(egui::Slider::new(&mut v, lo..=hi).text(label));
                self.set_f32("ducking", param, v);
                if r.drag_stopped() {
                    self.send_ducking(snap);
                }
            }

            ui.separator();
            ui.heading("OBS bridge (obs-websocket)");
            ui.label("Switches voice on OBS scene changes and exposes a VU text source.");
            let mut en = self.m_bool("obs", "enabled", snap.obs.enabled);
            if ui.checkbox(&mut en, "enabled").changed() {
                self.set_bool("obs", "enabled", en);
                self.send_obs(snap, None);
            }
            ui.horizontal(|ui| {
                let mut url = self.m_text("obs", "url", &snap.obs.url);
                let r = ui.add(
                    egui::TextEdit::singleline(&mut url)
                        .hint_text("ws://127.0.0.1:4455")
                        .desired_width(220.0),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.texts
                        .insert(("obs".to_string(), "url".to_string()), url.clone());
                    self.send_obs(snap, None);
                } else {
                    self.texts
                        .insert(("obs".to_string(), "url".to_string()), url);
                }
                let mut pass = self.m_text("obs", "password", "");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut pass)
                        .password(true)
                        .hint_text("password")
                        .desired_width(140.0),
                );
                if r.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !pass.is_empty()
                {
                    self.send_obs(snap, Some(pass));
                }
            });
            if snap.obs.connected {
                ui.colored_label(
                    egui::Color32::GREEN,
                    format!("connected — scene '{}'", snap.obs.scene),
                );
            } else {
                ui.colored_label(egui::Color32::GRAY, "not connected");
            }

            ui.separator();
            ui.heading("MIDI learn");
            ui.label("Binds MIDI knobs/pads to hotkey targets; captured via the Control API.");
            let mut en = self.m_bool("midi", "enabled", snap.midi.enabled);
            if ui.checkbox(&mut en, "enabled").changed() {
                self.set_bool("midi", "enabled", en);
                self.send_midi(snap);
            }
            ui.horizontal(|ui| {
                let mut port = self.m_text("midi", "port", &snap.midi.port);
                let r = ui.add(
                    egui::TextEdit::singleline(&mut port)
                        .hint_text("ALSA port (empty = first available)")
                        .desired_width(240.0),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.texts
                        .insert(("midi".to_string(), "port".to_string()), port.clone());
                    self.send_midi(snap);
                } else {
                    self.texts
                        .insert(("midi".to_string(), "port".to_string()), port);
                }
                if ui.button("↻ status").clicked() {
                    self.act(serde_json::json!({"action": "getMidiStatus"}));
                }
            });
            ui.label(format!(
                "{} bindings · {}{}",
                snap.midi.bindings,
                if snap.midi.connected { "connected" } else { "not connected" },
                if snap.midi.learning { " · capturing…" } else { "" }
            ));
        });
    }

    /// Preferences dialog (F-0.4.1.B): "Start with the system" toggle
    /// for the XDG autostart entry plus the daemon unit state.
    /// All the work happens in `vm_control::autostart` — the same code
    /// `vm autostart` runs on the CLI, so both fronts always agree.
    fn prefs_ui(&mut self, ctx: &egui::Context) {
        let status = self
            .prefs_status
            .clone()
            .unwrap_or_else(vm_control::autostart::status);
        let mut open = self.prefs_open;
        let mut toggle: Option<bool> = None;
        let mut refresh = false;
        egui::Window::new("Preferences")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.strong("Startup");
                ui.add_space(2.0);
                if status.sandboxed {
                    ui.label(vm_control::autostart::sandbox_guidance());
                } else {
                    let mut on = status.desktop_installed;
                    if ui
                        .checkbox(
                            &mut on,
                            "Start with the system (starts minimized to the tray)",
                        )
                        .on_hover_text(
                            "Install an XDG autostart entry running `vm-ui --minimized` at login",
                        )
                        .changed()
                    {
                        toggle = Some(on);
                    }
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label(status.daemon_summary());
                        if ui.small_button("refresh").clicked() {
                            refresh = true;
                        }
                    });
                }
                if let Some((is_err, text)) = &self.prefs_note {
                    ui.add_space(6.0);
                    let color = if *is_err {
                        egui::Color32::RED
                    } else {
                        egui::Color32::GRAY
                    };
                    ui.colored_label(color, text);
                }
            });
        self.prefs_open = open;
        if refresh {
            self.prefs_status = Some(vm_control::autostart::status());
        }
        if let Some(on) = toggle {
            self.prefs_note = Some(if on {
                Self::autostart_enable()
            } else {
                Self::autostart_disable()
            });
            // Reflect the real outcome immediately (install may warn).
            self.prefs_status = Some(vm_control::autostart::status());
        }
    }

    /// Shapes `autostart::enable()`'s outcome for the dialog.
    fn autostart_enable() -> (bool, String) {
        match vm_control::autostart::enable() {
            Ok(notes) => (
                notes.iter().any(|n| n.starts_with("WARNING")),
                notes.join("\n"),
            ),
            Err(e) => (true, format!("enable failed: {e}")),
        }
    }

    /// Shapes `autostart::disable()`'s outcome for the dialog.
    fn autostart_disable() -> (bool, String) {
        let notes = vm_control::autostart::disable();
        (
            notes.iter().any(|n| n.starts_with("WARNING")),
            notes.join("\n"),
        )
    }

    /// About dialog (F-0.4.1.C): identity, clean-room note, license and
    /// repo link. The URL opens in the browser (egui-winit open_url).
    fn about_ui(&mut self, ctx: &egui::Context) {
        let mut open = self.about_open;
        egui::Window::new("About")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(400.0)
            .show(ctx, |ui| {
                ui.add_space(2.0);
                ui.heading("Voicemod Linux");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.add_space(6.0);
                ui.label(
                    "A clean-room, from-scratch voice changer for Linux: real-time DSP \
                     effects, a soundboard and a real PipeWire virtual microphone. \
                     Original code and permissive-only dependencies.",
                );
                ui.add_space(6.0);
                ui.hyperlink_to(
                    "github.com/jmiranda-sre/voicemod-linux",
                    "https://github.com/jmiranda-sre/voicemod-linux",
                );
                ui.add_space(2.0);
                ui.label("License: MIT OR Apache-2.0");
            });
        self.about_open = open;
    }

    /// Setup wizard modal (F-0.4.B item 29).
    fn wizard_ui(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        // Auto-advance the measurement steps when their clock elapses.
        match self.wizard.step {
            wizard::Step::NoiseFloor if self.wizard.measurement_done() => {
                let range = self
                    .gate_node()
                    .and_then(|g| self.param_range(&g, "threshold_db"));
                self.wizard.finish_noise(range);
                if let Some(dn) = self.denoise_node() {
                    self.act(serde_json::json!({"action": "captureNoiseProfile", "node": dn}));
                    self.wizard.note = "noise profile captured".to_string();
                }
                self.wizard.begin_step(wizard::Step::Gate);
            }
            wizard::Step::SpeechLevel if self.wizard.measurement_done() => {
                self.wizard.finish_speech();
                self.wizard.begin_step(wizard::Step::Limiter);
            }
            _ => {}
        }
        let step = self.wizard.step;
        let mut open = self.wizard.open;
        egui::Window::new("Setup wizard — perfect audio in a minute")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .show(ctx, |ui| match step {
                wizard::Step::Welcome => {
                    ui.label("This 1-minute check finds your microphone level, measures your room, and suggests a noise gate and a safety limiter.");
                    ui.add_space(6.0);
                    ui.label("Flow: mic check » 3 s of silence » read one sentence » apply the suggestions.");
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Start").clicked() {
                            self.wizard.begin_step(wizard::Step::MicCheck);
                        }
                        if ui.button("Skip").clicked() {
                            self.wizard.open = false;
                        }
                    });
                }
                wizard::Step::MicCheck => {
                    wizard_pips(ui, 1);
                    ui.label(egui::RichText::new("Test your microphone").size(15.0).strong());
                    ui.label("Speak normally — the bars must move.");
                    ui.add_space(4.0);
                    level_db_bar(ui, "mic", snap.levels.mic_db);
                    level_db_bar(ui, "out", snap.levels.out_db);
                    ui.add_space(6.0);
                    if snap.levels.speech {
                        ui.colored_label(egui::Color32::GREEN, "speech detected");
                    } else if snap.levels.mic_db < -55.0 {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            "No signal yet — check the microphone on the Devices tab.",
                        );
                    }
                    if ui.button("I see the level moving »").clicked() {
                        self.wizard.begin_step(wizard::Step::NoiseFloor);
                    }
                }
                wizard::Step::NoiseFloor => {
                    wizard_pips(ui, 2);
                    ui.label(
                        egui::RichText::new("Measuring the noise floor").size(15.0).strong(),
                    );
                    ui.label("Stay silent for 3 seconds…");
                    ui.add(egui::ProgressBar::new(self.wizard.progress()).desired_height(10.0));
                    ui.add_space(4.0);
                    level_db_bar(ui, "mic", snap.levels.mic_db);
                    if snap.levels.speech {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            "voice detected — the measurement restarts best if you stay quiet",
                        );
                    }
                }
                wizard::Step::Gate => {
                    wizard_pips(ui, 3);
                    ui.label(egui::RichText::new("Noise gate").size(15.0).strong());
                    ui.label(format!(
                        "Room floor ≈ {:.0} dBFS — a gate {:.0} dBFS keeps it out.",
                        self.wizard.floor_db,
                        self.wizard.gate_db
                    ));
                    let (lo, hi) = self
                        .gate_node()
                        .and_then(|g| self.param_range(&g, "threshold_db"))
                        .unwrap_or((-70.0, -20.0));
                    let mut v = self.m_f32("wizard", "gate_db", self.wizard.gate_db);
                    if ui
                        .add(egui::Slider::new(&mut v, lo..=hi).text("gate threshold (dBFS)"))
                        .changed()
                    {
                        self.set_f32("wizard", "gate_db", v);
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("Apply to voice").clicked() {
                            let v = self.m_f32("wizard", "gate_db", self.wizard.gate_db);
                            if let Some(g) = self.gate_node() {
                                self.apply_param(&g, "threshold_db", v);
                                self.wizard.note = format!("gate set to {v:.0} dBFS");
                            } else {
                                self.wizard.note =
                                    "current preset has no gate node".to_string();
                            }
                        }
                        if ui.button("Next »").clicked() {
                            self.wizard.note.clear();
                            self.wizard.begin_step(wizard::Step::SpeechLevel);
                        }
                    });
                    if !self.wizard.note.is_empty() {
                        ui.label(&self.wizard.note);
                    }
                }
                wizard::Step::SpeechLevel => {
                    wizard_pips(ui, 4);
                    ui.label(egui::RichText::new("Speech level").size(15.0).strong());
                    ui.label("Read anything out loud: “The quick brown fox jumps over the lazy dog.”");
                    ui.add(egui::ProgressBar::new(self.wizard.progress()).desired_height(10.0));
                    ui.add_space(4.0);
                    level_db_bar(ui, "mic", snap.levels.mic_db);
                }
                wizard::Step::Limiter => {
                    wizard_pips(ui, 4);
                    ui.label(egui::RichText::new("Finishing touches").size(15.0).strong());
                    ui.label(format!(
                        "Speech ≈ {:.0} dBFS (target {:.0} dBFS).",
                        self.wizard.speech_db,
                        wizard::TARGET_SPEECH_DB
                    ));
                    let mut applied_any = false;
                    if let Some(comp) = self.comp_node() {
                        let range = self.param_range(&comp, "makeup_db");
                        let suggestion = wizard::suggest_makeup(self.wizard.speech_db, range);
                        let mut v = self.m_f32("wizard", "makeup_db", suggestion);
                        ui.add(egui::Slider::new(&mut v, -12.0..=24.0).text("makeup gain (dB)"));
                        self.set_f32("wizard", "makeup_db", v);
                        if ui.button("Apply makeup").clicked() {
                            self.apply_param(&comp, "makeup_db", v);
                            self.wizard.note = format!("makeup {v:+.1} dB applied");
                            applied_any = true;
                        }
                    }
                    if let Some(lim) = self.limit_node() {
                        let (lo, hi) = self
                            .param_range(&lim, "threshold_db")
                            .unwrap_or((-12.0, 0.0));
                        let suggestion = (-1.0f32).clamp(lo, hi);
                        let mut v = self.m_f32("wizard", "limiter_db", suggestion);
                        ui.add(egui::Slider::new(&mut v, lo..=hi).text("limiter threshold (dBFS)"));
                        self.set_f32("wizard", "limiter_db", v);
                        if ui.button("Apply limiter").clicked() {
                            self.apply_param(&lim, "threshold_db", v);
                            self.wizard.note = format!("limiter at {v:.1} dBFS applied");
                            applied_any = true;
                        }
                    }
                    let _ = applied_any;
                    if !self.wizard.note.is_empty() {
                        ui.label(&self.wizard.note);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Finish").clicked() {
                            self.wizard.mark_done(&Self::config_dir());
                            self.wizard.open = false;
                        }
                        if ui.button("Replay setup later").clicked() {
                            self.wizard.open = false;
                        }
                    });
                }
            });
        self.wizard.open = open;
    }

    fn health_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let h = &snap.health;
        ui.heading(format!("Pipeline — voice '{}'", h.voice));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(format!("link={} active={}", h.link, h.active));
            ui.separator();
            if h.graph_xruns > 0 {
                ui.colored_label(
                    egui::Color32::RED,
                    format!("graph xruns={}", h.graph_xruns),
                );
            } else {
                ui.label("graph xruns=0");
            }
            ui.separator();
            ui.label(format!(
                "watchdog restarts={}{}",
                h.restarts,
                if h.last_reason.is_empty() {
                    String::new()
                } else {
                    let age = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        .saturating_sub(h.last_restart_unix);
                    format!(" (last: {} {}s ago)", h.last_reason, age)
                }
            ));
            if ui.button("Refresh").clicked() {
                self.link.refresh();
            }
        });
        ui.separator();
        ui.heading("DSP nodes (µs per quantum)");
        ui.label(format!(
            "chain algorithmic latency ≈ {:.1} ms (group delay, not CPU cost)",
            h.latency_ms
        ));
        egui::Grid::new("node_health").num_columns(5).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.strong("node");
            ui.strong("avg µs");
            ui.strong("max µs");
            ui.strong("lat ms");
            ui.strong("calls");
            ui.end_row();
            for n in &h.nodes {
                ui.label(&n.node);
                ui.label(format!("{:.1}", n.avg_us));
                ui.label(format!("{}", n.max_us));
                ui.label(format!("{:.1}", n.lat_ms));
                ui.label(format!("{}", n.calls));
                ui.end_row();
            }
        });
        if h.nodes.is_empty() {
            ui.label("no telemetry yet — is a voice loaded and streaming?");
        }
        ui.label("low-latency note: denoise frame 512 halves the STFT window (≈21 ms » ≈11 ms) \
            but also halves frequency resolution (93.8 Hz bins — bass selectivity suffers). \
            Opt in via the denoise frame param; re-capture the noise profile after switching.");
        ui.separator();
        ui.heading("Streams");
        egui::Grid::new("stream_health").num_columns(7).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.strong("stream");
            ui.strong("calls");
            ui.strong("avg µs");
            ui.strong("max µs");
            ui.strong("xruns");
            ui.strong("trips");
            ui.strong("frames");
            ui.end_row();
            for (name, s) in [("capture", &h.capture), ("playback", &h.playback), ("monitor", &h.monitor)] {
                ui.label(name);
                ui.label(format!("{}", s.invocations));
                ui.label(format!("{:.1}", s.avg_us));
                ui.label(format!("{}", s.max_us));
                if s.xruns > 0 {
                    ui.colored_label(egui::Color32::RED, format!("{}", s.xruns));
                } else {
                    ui.label("0");
                }
                ui.label(format!("{}", s.trips));
                ui.label(format!("{}", s.last_frames));
                ui.end_row();
            }
        });
    }
}

/// Setup wizard progress pips: 4 dots, `filled` of them done. Drawn
/// with the painter (no font glyphs involved), purely cosmetic — the
/// flow itself still lives in `wizard::Step`.
fn wizard_pips(ui: &mut egui::Ui, filled: usize) {
    let total = 4;
    let gap = 12.0;
    let r = 3.5;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(gap * total as f32, r * 2.0 + 2.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    for i in 0..total {
        let center = rect.min + egui::vec2(gap * (i as f32 + 0.5), rect.height() / 2.0);
        if i < filled {
            painter.circle_filled(center, r, egui::Color32::from_rgb(0x7d, 0xd3, 0xfc));
        } else {
            painter.circle_stroke(center, r, egui::Stroke::new(1.0, egui::Color32::GRAY));
        }
    }
}

/// Setup wizard VU: dBFS (−60…0) into a progress bar.
fn level_db_bar(ui: &mut egui::Ui, label: &str, db: f32) {
    let t = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(
            egui::ProgressBar::new(t)
                .desired_width(260.0)
                .text(format!("{db:.0} dBFS")),
        );
    });
}

/// What `main` does after a window closes; kept pure for the unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseOutcome {
    /// Link + tray live on in `main`; a later tray "Show" opens a new window.
    WaitTray,
    /// Quit (tray menu / smoke hook), or window-only mode: with no tray
    /// there is no way back to a closed window, and a stranded process
    /// would need a kill — so closing really quits.
    Exit,
}

fn close_outcome(quit: bool, tray: bool) -> CloseOutcome {
    if quit || !tray {
        CloseOutcome::Exit
    } else {
        CloseOutcome::WaitTray
    }
}

/// Set by tray Quit (or the quit smoke hook); read by `main` after every
/// window closes: set = the process exits, clear = back to the tray.
static QUIT: AtomicBool = AtomicBool::new(false);

/// What the tray wait loop got: open a window, or exit the process.
enum WaitAction {
    Show,
    Exit,
}

/// E2E smoke hooks, armed by env: `VM_UI_SMOKE_CLOSE_SECS` closes the
/// window like the WM "X" and `VM_UI_SMOKE_QUIT_SECS` quits like the tray
/// Quit item (both once, counting from process start), while
/// `VM_UI_SMOKE_SHOW_SECS` re-opens one like a tray "Show" click, N seconds
/// into every stay in the tray. Together they drive the whole lifecycle
/// with no human clicking; unset in normal runs.
struct Smoke {
    start: std::time::Instant,
    close_after: Option<std::time::Duration>,
    show_after: Option<std::time::Duration>,
    quit_after: Option<std::time::Duration>,
    close_fired: AtomicBool,
}

impl Smoke {
    fn from_env() -> Self {
        Self {
            start: std::time::Instant::now(),
            close_after: smoke_secs("VM_UI_SMOKE_CLOSE_SECS"),
            show_after: smoke_secs("VM_UI_SMOKE_SHOW_SECS"),
            quit_after: smoke_secs("VM_UI_SMOKE_QUIT_SECS"),
            close_fired: AtomicBool::new(false),
        }
    }

    fn due(
        &self,
        deadline: Option<std::time::Duration>,
        fired: &AtomicBool,
        now: std::time::Instant,
    ) -> bool {
        deadline.is_some_and(|d| now.saturating_duration_since(self.start) >= d)
            && !fired.swap(true, Ordering::Relaxed)
    }

    fn close_due(&self, now: std::time::Instant) -> bool {
        self.due(self.close_after, &self.close_fired, now)
    }

    /// Quit is idempotent (the `QUIT` flag latches), so it stays "due".
    fn quit_due(&self, now: std::time::Instant) -> bool {
        self.quit_after
            .is_some_and(|d| now.saturating_duration_since(self.start) >= d)
    }
}

/// Parse a smoke-hook env var (seconds; clamped to a small minimum so the
/// app always finishes booting first).
fn smoke_secs(var: &str) -> Option<std::time::Duration> {
    let secs: f32 = std::env::var(var).ok()?.parse().ok()?;
    Some(std::time::Duration::from_secs_f32(secs.max(0.05)))
}

/// One process-wide smoke instance, anchored at first use (`main` arms it
/// before doing anything else).
fn smoke() -> &'static Smoke {
    static SMOKE: OnceLock<Smoke> = OnceLock::new();
    SMOKE.get_or_init(Smoke::from_env)
}

/// Pump the tray while no window exists: show / mute / quit and the smoke
/// hooks, with the WS link still running on its own thread. Returns when a
/// window should open or the process should exit.
fn tray_wait(tray: &Option<Tray>, link: &UiLink) -> WaitAction {
    let Some(tray) = tray.as_ref() else {
        // Unreachable (window-only mode exits instead of waiting), but a
        // wait with nothing to wait on must never become an infinite loop.
        return WaitAction::Exit;
    };
    let show_after = smoke().show_after;
    let entered = std::time::Instant::now();
    let mut synced: Option<bool> = None;
    loop {
        let now = std::time::Instant::now();
        if smoke().quit_due(now) {
            QUIT.store(true, Ordering::Relaxed);
        }
        if QUIT.load(Ordering::Relaxed) {
            return WaitAction::Exit;
        }
        if show_after.is_some_and(|d| now.saturating_duration_since(entered) >= d) {
            return WaitAction::Show;
        }
        for action in tray.poll() {
            match action {
                TrayAction::ToggleVisibility => return WaitAction::Show,
                TrayAction::ToggleMute => {
                    let muted = link.snapshot.lock().map(|s| s.muted).unwrap_or(false);
                    link.send_action(serde_json::json!({
                        "action": "setMuted", "muted": !muted,
                    }));
                }
                TrayAction::Quit => {
                    QUIT.store(true, Ordering::Relaxed);
                    return WaitAction::Exit;
                }
            }
        }
        // Menu reflects "no window": label says Show, checkmark stays live.
        let muted = link.snapshot.lock().map(|s| s.muted).unwrap_or(false);
        if synced != Some(muted) {
            tray.sync(false, muted);
            synced = Some(muted);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn main() -> eframe::Result<()> {
    let _ = smoke(); // anchor the smoke deadlines at process start
    let hidden = start_hidden_from(std::env::args());

    let waker: Waker = Arc::new(Mutex::new(None));
    let mut daemon_child: Option<std::process::Child> = None;
    let link = UiLink::new({
        let waker = waker.clone();
        move || {
            if let Ok(waker) = waker.lock() {
                if let Some(ctx) = waker.as_ref() {
                    ctx.request_repaint();
                }
            }
        }
    });
    let tray = match Tray::new() {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("tray: {e} (window-only mode: closing the window quits)");
            None
        }
    };
    // A hidden boot without a tray would leave nothing on screen to click,
    // so window-only mode always shows the window (it stays reachable).
    // With a tray, `--minimized` starts directly in the wait loop and no
    // window is ever created — nothing can flash on screen.
    if hidden && tray.is_some() {
        if let WaitAction::Exit = tray_wait(&tray, &link) {
            return Ok(());
        }
    }

    loop {
        let res = eframe::run_native(
            "Voicemod Linux",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_inner_size([980.0, 640.0])
                    // Must equal the AppStream/desktop ID: Wayland compositors
                    // pair the window with io.github.voicemodlinux.vm.desktop
                    // (and its icon) through this app_id.
                    .with_app_id("io.github.voicemodlinux.vm"),
                // Closing the window must return here — the process keeps
                // living in the tray — instead of calling `process::exit`.
                run_and_return: true,
                ..Default::default()
            },
            Box::new(|cc| Ok(Box::new(VmApp::new(cc, &link, &tray, &mut daemon_child, &waker)))),
        );
        if let Ok(mut waker) = waker.lock() {
            *waker = None;
        }
        link.set_fast_levels(false); // wizard meters stop polling between windows
        res?;
        if close_outcome(QUIT.load(Ordering::Relaxed), tray.is_some()) == CloseOutcome::Exit {
            break;
        }
        eprintln!("window closed; running in the tray (Quit lives there)");
        if let WaitAction::Exit = tray_wait(&tray, &link) {
            break;
        }
    }
    Ok(())
}

/// `--minimized` anywhere in argv boots straight into the tray.
fn start_hidden_from(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|a| a == "--minimized")
}

#[cfg(test)]
mod ui_flag_tests {
    use super::{close_outcome, start_hidden_from, CloseOutcome, Smoke};
    use std::sync::atomic::AtomicBool;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn minimized_flag_detected() {
        assert!(start_hidden_from(argv(&["vm-ui", "--minimized"]).into_iter()));
        assert!(start_hidden_from(argv(&["vm-ui", "--foo", "--minimized", "--bar"]).into_iter()));
        assert!(!start_hidden_from(argv(&["vm-ui"]).into_iter()));
        assert!(!start_hidden_from(argv(&["vm-ui", "--minimize"]).into_iter()));
    }

    #[test]
    fn close_outcome_quits_only_on_quit_or_no_tray() {
        // "X" with a live tray: back to the tray, process + link stay up.
        assert_eq!(close_outcome(false, true), CloseOutcome::WaitTray);
        // Tray Quit: exit for real.
        assert_eq!(close_outcome(true, true), CloseOutcome::Exit);
        // Window-only mode: close behaves like a normal app close.
        assert_eq!(close_outcome(false, false), CloseOutcome::Exit);
        assert_eq!(close_outcome(true, false), CloseOutcome::Exit);
    }

    #[test]
    fn smoke_deadlines_fire_once() {
        let start = std::time::Instant::now();
        let s = Smoke {
            start,
            close_after: Some(std::time::Duration::from_secs(2)),
            show_after: None,
            quit_after: Some(std::time::Duration::from_secs(4)),
            close_fired: AtomicBool::new(false),
        };
        let at = |ms| start + std::time::Duration::from_millis(ms);
        assert!(!s.close_due(at(1999)));
        assert!(s.close_due(at(2000)));
        assert!(!s.close_due(at(3000)), "close fires at most once");
        assert!(!s.quit_due(at(3999)));
        assert!(s.quit_due(at(4000)));
        assert!(s.quit_due(at(5000)), "quit stays due (idempotent)");
    }
}
