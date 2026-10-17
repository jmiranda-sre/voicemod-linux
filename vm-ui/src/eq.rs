//! Interactive EQ canvas (F-0.4.B item 26): draws the exact biquad
//! response of the current preset's EQ bands and lets the user drag a
//! band's frequency/gain directly on the curve.
//!
//! All filter math comes from `vm-dsp` ([`vm_dsp::biquad_magnitude_db`]),
//! so the drawn curve *is* the filter the audio path runs. The canvas
//! itself is pure UI: it returns pending parameter updates and the caller
//! (main.rs) sends them through the Control API and updates its slider
//! mirrors.

use eframe::egui;
use vm_dsp::{biquad_magnitude_db, BqType};

/// Frequency axis span (log).
pub const F_MIN: f32 = 20.0;
pub const F_MAX: f32 = 20_000.0;
/// Gain axis span (dB).
pub const G_MIN: f32 = -24.0;
pub const G_MAX: f32 = 24.0;

/// One draggable EQ band backed by a `biquad` node in the current preset.
#[derive(Debug, Clone, PartialEq)]
pub struct EqBand {
    /// Preset node id (e.g. `eq220`).
    pub node: String,
    pub kind: BqType,
    pub freq: f32,
    pub q: f32,
    pub gain_db: f32,
    /// Peaking/shelf bands expose `gain_db`; pass/notch bands move on the
    /// frequency axis only.
    pub has_gain: bool,
}

/// Extract the biquad nodes of a preset (preset order preserved).
pub fn bands_from_preset(preset: &vm_preset::Preset) -> Vec<EqBand> {
    let mut out = Vec::new();
    for node in &preset.nodes {
        if node.r#type != "biquad" {
            continue;
        }
        let kind = node
            .params
            .get("type")
            .and_then(|p| match &p.value {
                vm_preset::JsonParamValue::Text(t) => BqType::parse(t),
                _ => None,
            });
        let Some(kind) = kind else { continue };
        let f = |name: &str, dflt: f32| -> f32 {
            match node.params.get(name).map(|p| &p.value) {
                Some(vm_preset::JsonParamValue::Float(v)) => *v as f32,
                _ => dflt,
            }
        };
        let has_gain = matches!(
            kind,
            BqType::Peaking | BqType::Lowshelf | BqType::Highshelf
        ) && node.params.contains_key("gain_db");
        out.push(EqBand {
            node: node.id.clone(),
            kind,
            freq: f("freq_hz", 1_000.0),
            q: f("q", std::f32::consts::FRAC_1_SQRT_2),
            gain_db: if has_gain { f("gain_db", 0.0) } else { 0.0 },
            has_gain,
        });
    }
    out
}

/// Combined response of `bands` at `at` (dB, summed like the cascade).
pub fn response_db(bands: &[EqBand], rate: f32, at: f32) -> f32 {
    bands
        .iter()
        .map(|b| biquad_magnitude_db(b.kind, rate, b.freq, b.q, b.gain_db, at))
        .sum()
}

// ---------------------------------------------------------------------------
// Axis mappings (pure; unit-tested).
// ---------------------------------------------------------------------------

pub fn freq_to_x(f: f32, rect: egui::Rect) -> f32 {
    let t = (f.clamp(F_MIN, F_MAX) / F_MIN).ln() / (F_MAX / F_MIN).ln();
    rect.left() + t * rect.width()
}

pub fn x_to_freq(x: f32, rect: egui::Rect) -> f32 {
    let t = ((x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
    F_MIN * (F_MAX / F_MIN).powf(t)
}

pub fn gain_to_y(g: f32, rect: egui::Rect) -> f32 {
    let t = ((G_MAX - g.clamp(G_MIN, G_MAX)) / (G_MAX - G_MIN)).clamp(0.0, 1.0);
    rect.top() + t * rect.height()
}

pub fn y_to_gain(y: f32, rect: egui::Rect) -> f32 {
    let t = ((y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0);
    G_MAX - t * (G_MAX - G_MIN)
}

/// Nearest band to a canvas position, when within `radius` px.
pub fn nearest_band(bands: &[EqBand], rect: egui::Rect, pos: egui::Pos2, radius: f32) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, b) in bands.iter().enumerate() {
        let c = egui::pos2(freq_to_x(b.freq, rect), if b.has_gain { gain_to_y(b.gain_db, rect) } else { gain_to_y(0.0, rect) });
        let d = c.distance(pos);
        if d <= radius && best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some((i, d));
        }
    }
    best.map(|(i, _)| i)
}

// ---------------------------------------------------------------------------
// Canvas widget.
// ---------------------------------------------------------------------------

/// Draw the EQ canvas. Returns pending `(node, param, value)` updates —
/// the caller forwards them as `setVoiceParameter` actions.
pub fn canvas(
    ui: &mut egui::Ui,
    bands: &mut [EqBand],
    rate: f32,
    selected: &mut Option<usize>,
) -> Vec<(String, &'static str, f32)> {
    let mut pending: Vec<(String, &'static str, f32)> = Vec::new();
    if bands.is_empty() {
        return pending;
    }
    let width = ui.available_width().clamp(260.0, 720.0);
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(width, 190.0), egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(24, 26, 32));

    // Grid: decades + gain lines.
    let faint = egui::Stroke::new(1.0, egui::Color32::from_rgb(48, 52, 62));
    for f in [50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0] {
        let x = freq_to_x(f, rect);
        painter.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], faint);
    }
    for g in [-12.0, 0.0, 12.0] {
        let y = gain_to_y(g, rect);
        painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], faint);
    }
    for f in [100.0, 1_000.0, 10_000.0] {
        let x = freq_to_x(f, rect);
        let label = if f >= 1_000.0 {
            format!("{:.0}k", f / 1_000.0)
        } else {
            format!("{f:.0}")
        };
        painter.text(
            egui::pos2(x, rect.top() + 2.0),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(10.0),
            egui::Color32::from_rgb(140, 146, 160),
        );
    }

    // Combined response curve — the exact DSP math.
    let steps = (width as usize / 2).max(64);
    let mut pts = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let x = rect.left() + rect.width() * i as f32 / steps as f32;
        let db = response_db(bands, rate, x_to_freq(x, rect)).clamp(G_MIN, G_MAX);
        pts.push(egui::pos2(x, gain_to_y(db, rect)));
    }
    painter.add(egui::Shape::line(
        pts,
        egui::Stroke::new(1.8, egui::Color32::from_rgb(120, 200, 255)),
    ));

    // Interaction: drag starts near a handle (36 px), then moves it.
    if resp.drag_started() {
        if let Some(pos) = resp.interact_pointer_pos() {
            if let Some(i) = nearest_band(bands, rect, pos, 36.0) {
                *selected = Some(i);
            }
        }
    }
    if resp.dragged() {
        if let (Some(i), Some(pos)) = (*selected, resp.interact_pointer_pos()) {
            if let Some(b) = bands.get_mut(i) {
                let freq = x_to_freq(pos.x, rect).clamp(F_MIN, F_MAX);
                b.freq = freq;
                pending.push((b.node.clone(), "freq_hz", freq));
                if b.has_gain {
                    let g = y_to_gain(pos.y, rect);
                    b.gain_db = g;
                    pending.push((b.node.clone(), "gain_db", g));
                }
            }
        }
    }
    if resp.double_clicked() {
        if let Some(i) = *selected {
            if let Some(b) = bands.get_mut(i) {
                if b.has_gain && b.gain_db != 0.0 {
                    b.gain_db = 0.0;
                    pending.push((b.node.clone(), "gain_db", 0.0));
                }
            }
        }
    }

    // Handles.
    for (i, b) in bands.iter().enumerate() {
        let c = egui::pos2(
            freq_to_x(b.freq, rect),
            if b.has_gain { gain_to_y(b.gain_db, rect) } else { gain_to_y(0.0, rect) },
        );
        let is_sel = *selected == Some(i);
        let color = if is_sel {
            egui::Color32::from_rgb(255, 214, 120)
        } else {
            egui::Color32::from_rgb(200, 208, 224)
        };
        if is_sel {
            painter.circle_stroke(c, 8.0, egui::Stroke::new(1.5, color));
        }
        painter.circle_filled(c, 5.0, color);
        if is_sel {
            let label = if b.has_gain {
                format!("{}: {:.0} Hz  {:+.1} dB  Q {:.2}", b.node, b.freq, b.gain_db, b.q)
            } else {
                format!("{}: {:.0} Hz  Q {:.2}", b.node, b.freq, b.q)
            };
            painter.text(
                egui::pos2(c.x, (c.y - 10.0).max(rect.top() + 12.0)),
                egui::Align2::CENTER_BOTTOM,
                label,
                egui::FontId::proportional(11.0),
                color,
            );
        }
    }
    resp.on_hover_text("Drag a handle: horizontal = frequency, vertical = gain (double-click resets)");
    pending
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(600.0, 190.0))
    }

    fn preset_json() -> String {
        r#"{
            "name": "t", "version": 4,
            "nodes": [
                {"id": "hp", "type": "biquad", "params": {
                    "type": {"value": "highpass"},
                    "freq_hz": {"value": 90.0, "range": [20.0, 2000.0, 90.0], "public": true},
                    "q": {"value": 0.707}
                }},
                {"id": "eq350", "type": "biquad", "params": {
                    "type": {"value": "peaking"},
                    "freq_hz": {"value": 350.0},
                    "q": {"value": 1.2},
                    "gain_db": {"value": -3.0}
                }},
                {"id": "gate", "type": "gate", "params": {
                    "threshold_db": {"value": -55.0}
                }}
            ],
            "edges": []
        }"#
        .to_string()
    }

    #[test]
    fn bands_from_preset_picks_biquads_only() {
        let p = vm_preset::load(&preset_json()).unwrap();
        let bands = bands_from_preset(&p);
        assert_eq!(bands.len(), 2);
        assert_eq!(bands[0].node, "hp");
        assert_eq!(bands[0].kind, BqType::Highpass);
        assert!(!bands[0].has_gain, "highpass has no gain handle");
        assert!((bands[0].freq - 90.0).abs() < 1e-6);
        assert_eq!(bands[1].node, "eq350");
        assert!(bands[1].has_gain);
        assert!((bands[1].gain_db + 3.0).abs() < 1e-6);
        assert!((bands[1].q - 1.2).abs() < 1e-6);
    }

    #[test]
    fn axis_mappings_round_trip() {
        let r = rect();
        for f in [20.0f32, 100.0, 440.0, 1_000.0, 8_000.0, 20_000.0] {
            let x = freq_to_x(f, r);
            let back = x_to_freq(x, r);
            assert!((back - f).abs() <= f * 1e-4, "{f} -> {back}");
        }
        for g in [-24.0f32, -6.0, 0.0, 3.5, 24.0] {
            let y = gain_to_y(g, r);
            let back = y_to_gain(y, r);
            assert!((back - g).abs() < 1e-3, "{g} -> {back}");
        }
        // Axis direction: 20 Hz at the left edge, 20 kHz at the right.
        assert!((freq_to_x(20.0, r) - r.left()).abs() < 0.01);
        assert!((freq_to_x(20_000.0, r) - r.right()).abs() < 0.01);
        assert!((gain_to_y(0.0, r) - r.center().y).abs() < 0.01);
    }

    #[test]
    fn response_sums_bands_like_a_cascade() {
        let p = vm_preset::load(&preset_json()).unwrap();
        let bands = bands_from_preset(&p);
        // Sum of the singles equals the combined curve at any probe.
        let at = 350.0;
        let single: f32 = bands
            .iter()
            .map(|b| biquad_magnitude_db(b.kind, 48_000.0, b.freq, b.q, b.gain_db, at))
            .sum();
        assert!((response_db(&bands, 48_000.0, at) - single).abs() < 1e-4);
        // The bell still peaks near its own center in the combined curve.
        assert!(response_db(&bands, 48_000.0, 350.0) < response_db(&bands, 48_000.0, 5_000.0) + 2.0);
    }

    #[test]
    fn nearest_band_picks_within_radius_only() {
        let p = vm_preset::load(&preset_json()).unwrap();
        let bands = bands_from_preset(&p);
        let r = rect();
        let c = egui::pos2(freq_to_x(350.0, r), gain_to_y(-3.0, r));
        assert_eq!(nearest_band(&bands, r, c, 36.0), Some(1));
        assert_eq!(nearest_band(&bands, r, egui::pos2(r.left() + 2.0, r.top() + 2.0), 12.0), None);
        // Highpass handle sits on the 0 dB line.
        let hc = egui::pos2(freq_to_x(90.0, r), gain_to_y(0.0, r));
        assert_eq!(nearest_band(&bands, r, hc, 10.0), Some(0));
    }
}
