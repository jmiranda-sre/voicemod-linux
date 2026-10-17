//! System tray for `vm-ui` (F-13): StatusNotifier/AppIndicator icon +
//! menu so the window can close while the daemon link stays alive.
//!
//! The tray is owned by `main` and outlives every window: [`Tray::poll`]
//! drains both event channels without blocking, called from the app's
//! `logic` hook while a window exists and from the tray wait loop in
//! `main` when none does. The channels never wake egui, so with a window
//! a 500 ms self-tick keeps the menu responsive, and the wait loop polls
//! on its own. "Hide"/"Show" map to closing and reopening the window —
//! the only mechanism that actually hides on Wayland, where winit's
//! `Window::set_visible` is a documented no-op. No tray in the Flatpak
//! sandbox (`/dev/input`-style: the SNI host is unreachable there by
//! design).

use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};

/// Actions the tray raises for the app to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    ToggleVisibility,
    ToggleMute,
    Quit,
}

/// 32 px mic glyph, drawn in code (no asset pipeline needed for the
/// tray: capsule + cradle arc + stand, light on transparent).
pub fn draw_mic_icon() -> (Vec<u8>, u32, u32) {
    const S: usize = 32;
    let mut px = vec![0u8; S * S * 4];
    let white = [235, 235, 240, 255];
    let green = [80, 200, 120, 255];
    // Capsule: bar x∈[13,18], y∈[5,15] with round caps.
    for y in 5..=15 {
        for x in 13..=18 {
            put(&mut px, S, x, y, white);
        }
    }
    disc(&mut px, S, 15, 5, 3, white);
    disc(&mut px, S, 16, 5, 3, white);
    disc(&mut px, S, 15, 15, 3, white);
    disc(&mut px, S, 16, 15, 3, white);
    // Cradle arc under the capsule + stand.
    for a in 0..=20 {
        let t = std::f32::consts::PI * (0.15 + 0.7 * a as f32 / 20.0);
        put(
            &mut px,
            S,
            (16.0 + 8.0 * t.cos()).round() as i32,
            (13.0 + 8.0 * t.sin()).round() as i32,
            white,
        );
    }
    for y in 22..=26 {
        put(&mut px, S, 16, y, white);
    }
    for x in 12..=20 {
        put(&mut px, S, x, 27, white);
    }
    // Status dot (green = live).
    disc(&mut px, S, 25, 25, 3, green);
    (px, S as u32, S as u32)
}

fn put(px: &mut [u8], size: usize, x: i32, y: i32, c: [u8; 4]) {
    if (0..size as i32).contains(&x) && (0..size as i32).contains(&y) {
        let o = (y as usize * size + x as usize) * 4;
        px[o..o + 4].copy_from_slice(&c);
    }
}

fn disc(px: &mut [u8], size: usize, cx: i32, cy: i32, r: i32, c: [u8; 4]) {
    for y in (cy - r)..=(cy + r) {
        for x in (cx - r)..=(cx + r) {
            let dx = x - cx;
            let dy = y - cy;
            if dx * dx + dy * dy <= r * r {
                put(px, size, x, y, c);
            }
        }
    }
}

pub struct Tray {
    _icon: tray_icon::TrayIcon,
    show: MenuItem,
    mute: CheckMenuItem,
    quit: MenuItem,
}

impl Tray {
    /// Build the icon + menu. `Err` (logged by the caller) means
    /// window-only mode: sandbox, headless D-Bus, or no SNI host.
    /// Everything else keeps working — the tray is decoration, never
    /// a dependency.
    pub fn new() -> Result<Self, String> {
        if vm_control::sandbox_note().is_some() {
            return Err("Flatpak sandbox: no tray (SNI host unreachable)".into());
        }
        let (rgba, w, h) = draw_mic_icon();
        let icon = tray_icon::Icon::from_rgba(rgba, w, h).map_err(|e| format!("tray icon: {e}"))?;
        let menu = Menu::new();
        let show = MenuItem::new("Hide vm-ui", true, None);
        let mute = CheckMenuItem::new("Mute microphone", true, false, None);
        let quit = MenuItem::new("Quit", true, None);
        menu.append_items(&[&show, &mute, &PredefinedMenuItem::separator(), &quit])
            .map_err(|e| format!("tray menu: {e}"))?;
        let icon = tray_icon::TrayIconBuilder::new()
            .with_menu_on_left_click(false)
            .with_menu(Box::new(menu.clone()))
            .with_icon(icon)
            .with_tooltip("Voicemod Linux")
            .with_title("Voicemod Linux")
            .build()
            .map_err(|e| format!("tray build: {e}"))?;
        Ok(Self { _icon: icon, show, mute, quit })
    }

    /// Reflect window + mute state in the menu (labels/checkmark).
    pub fn sync(&self, visible: bool, muted: bool) {
        self.show.set_text(if visible { "Hide vm-ui" } else { "Show vm-ui" });
        self.mute.set_checked(muted);
    }

    /// Drain menu + click events without blocking. Left-click toggles
    /// the window (the menu stays on right-click).
    pub fn poll(&self) -> Vec<TrayAction> {
        let mut out = Vec::new();
        while let Ok(ev) = muda::MenuEvent::receiver().try_recv() {
            if ev.id == self.quit.id() {
                out.push(TrayAction::Quit);
            } else if ev.id == self.show.id() {
                out.push(TrayAction::ToggleVisibility);
            } else if ev.id == self.mute.id() {
                out.push(TrayAction::ToggleMute);
            }
        }
        while let Ok(ev) = tray_icon::TrayIconEvent::receiver().try_recv() {
            if matches!(
                ev,
                tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                }
            ) {
                out.push(TrayAction::ToggleVisibility);
            }
        }
        out
    }
}

#[cfg(test)]
mod tray_tests {
    use super::*;

    #[test]
    fn mic_icon_is_32px_rgba_with_opaque_pixels() {
        let (rgba, w, h) = draw_mic_icon();
        assert_eq!((w, h), (32, 32));
        assert_eq!(rgba.len(), 32 * 32 * 4);
        let opaque = rgba.as_chunks::<4>().0.iter().filter(|p| p[3] == 255).count();
        // Capsule + cradle + stand + dot, on a transparent background.
        assert!(opaque > 100 && opaque < 32 * 32, "opaque pixels: {opaque}");
        // Status dot is green-ish at the corner.
        let o = (25 * 32 + 25) * 4;
        assert!(rgba[o + 1] > 150 && rgba[o] < 120, "status dot must be green");
    }
}
