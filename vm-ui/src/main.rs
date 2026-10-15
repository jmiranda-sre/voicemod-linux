//! `vm-ui`: native egui frontend — a WebSocket client of the Control API.
//!
//! The UI owns no audio: presets come from `~/.config/vm/presets` (parsed
//! with `vm-preset` for ranges), everything else flows through actions.

mod link;

use eframe::egui;
use link::{Snapshot, UiLink};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Voices,
    Soundboard,
    Devices,
}

struct VmApp {
    link: UiLink,
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
    daemon_child: Option<std::process::Child>,
    status_msg: String,
}

impl VmApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let link = UiLink::new(move || ctx.request_repaint());
        let snap = link.snapshot.clone();
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
            daemon_child: None,
            status_msg: "connecting…".to_string(),
        };
        app.refresh_presets();
        app
    }

    fn preset_dir() -> std::path::PathBuf {
        std::env::var("VM_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("XDG_CONFIG_HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
                    .join("vm")
            })
            .join("presets")
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
                        }
                    }
                }
                self.preset = Some(p);
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

impl eframe::App for VmApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self.snap.lock().map(|s| s.clone()).unwrap_or_default();

        // Keep local preset selection in sync with the daemon voice.
        if !snap.selected_voice.is_empty()
            && self.preset.as_ref().map(|p| p.name.as_str()) != Some(snap.selected_voice.as_str())
            && self.presets.contains(&snap.selected_voice)
        {
            self.load_preset(&snap.selected_voice);
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
                for (tab, name) in [(Tab::Voices, "Voices"), (Tab::Soundboard, "Soundboard"), (Tab::Devices, "Devices")] {
                    ui.selectable_value(&mut self.tab, tab, name);
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
                                self.daemon_child = Some(child);
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
        });
    }
}

impl VmApp {
    fn voices_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
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
                            ui.label("→ VAD+mon");
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
        ui.heading("Virtual + monitor");
        for d in snap.devices.iter().filter(|d| d.role != "Input") {
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
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 640.0])
            .with_position([80.0, 80.0]),
        ..Default::default()
    };
    eframe::run_native(
        "vm-ui — Voicemod Linux (clean-room)",
        options,
        Box::new(|cc| Ok(Box::new(VmApp::new(cc)))),
    )
}
