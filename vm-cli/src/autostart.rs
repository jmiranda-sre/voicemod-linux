//! Autostart management (F-13): XDG desktop entry for the UI plus the
//! `systemd --user` unit for the daemon ("install and forget").
//!
//! Split for testability: path rendering + file ops are pure over
//! injected dirs (unit-tested); `systemctl` is a thin best-effort
//! runner. In the Flatpak sandbox both degrade to portal guidance
//! (autostart is owned by the Flatpak permission store there).

use std::path::{Path, PathBuf};

pub const DESKTOP_ID: &str = "io.github.voicemodlinux.vm.desktop";
pub const UNIT_NAME: &str = "vm-daemon.service";

/// `$XDG_CONFIG_HOME/autostart/<app-id>` (XDG Autostart spec).
pub fn autostart_desktop_path(config_home: &Path) -> PathBuf {
    config_home.join("autostart").join(DESKTOP_ID)
}

/// Desktop entry that boots the UI straight into the tray. The daemon
/// itself autostarts via the systemd unit (separate mechanism below).
pub fn render_autostart_desktop(ui_exec: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.5\n\
         Name=Voicemod Linux\n\
         Comment=Voice changer UI (starts minimized to the tray)\n\
         Exec={ui_exec} --minimized\n\
         Icon=io.github.voicemodlinux.vm\n\
         Terminal=false\n\
         Categories=AudioVideo;Audio;\n\
         X-GNOME-Autostart-enabled=true\n\
         Hidden=false\n"
    )
}

/// Where a user/system installed unit would live, in lookup order.
pub fn unit_search_paths(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".config/systemd/user").join(UNIT_NAME),
        PathBuf::from("/usr/lib/systemd/user").join(UNIT_NAME),
        PathBuf::from("/etc/systemd/user").join(UNIT_NAME),
    ]
}

pub fn find_unit(home: &Path) -> Option<PathBuf> {
    unit_search_paths(home).into_iter().find(|p| p.exists())
}

fn systemctl_user(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("systemctl unavailable: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn sandbox_guidance() -> String {
    "sandboxed (Flatpak): autostart is owned by the Flatpak permission \
     store, not by systemd/XDG files here. Enable Background permission \
     for the app (software center or Flatseal) and launch the UI from \
     the app grid — the tray is unavailable in the sandbox, the window \
     stays a normal window."
        .to_string()
}

/// Pure status assembly (systemctl outputs injected → unit-tested).
pub fn status_report(
    desktop_present: bool,
    desktop_path: &Path,
    unit: Option<&Path>,
    enabled: &str,
    active: &str,
) -> String {
    format!(
        "ui autostart desktop: {} ({})\ndaemon unit: {}\nunit enabled: {enabled}\nunit active: {active}",
        if desktop_present { "installed" } else { "absent" },
        desktop_path.display(),
        unit.map(|p| p.display().to_string()).as_deref().unwrap_or("not installed"),
    )
}

pub fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")).unwrap_or_else(|_| {
                PathBuf::from("/tmp")
            })
        })
}

fn ui_exec() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("vm-ui");
            if cand.exists() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    "vm-ui".to_string()
}

/// Write (or overwrite) the XDG autostart entry. Returns its path.
pub fn install_desktop(config_home: &Path, exec: &str) -> Result<PathBuf, String> {
    let path = autostart_desktop_path(config_home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, render_autostart_desktop(exec))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Remove the XDG autostart entry. `Ok(true)` = removed, `Ok(false)` =
/// it wasn't there.
pub fn remove_desktop(config_home: &Path) -> Result<bool, String> {
    let path = autostart_desktop_path(config_home);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

pub fn cmd_status() -> String {
    if std::env::var("FLATPAK_ID").is_ok() {
        return sandbox_guidance();
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let ch = config_home();
    let desktop = autostart_desktop_path(&ch);
    let unit = find_unit(&home);
    let enabled = systemctl_user(&["is-enabled", UNIT_NAME]).unwrap_or_else(|e| e);
    let active = systemctl_user(&["is-active", UNIT_NAME]).unwrap_or_else(|e| e);
    status_report(desktop.exists(), &desktop, unit.as_deref(), &enabled, &active)
}

pub fn cmd_enable() -> String {
    if std::env::var("FLATPAK_ID").is_ok() {
        return sandbox_guidance();
    }
    let mut notes = Vec::new();
    let ch = config_home();
    match install_desktop(&ch, &ui_exec()) {
        Ok(p) => notes.push(format!("ui autostart installed: {}", p.display())),
        Err(e) => return format!("enable failed: {e}"),
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    match find_unit(&home) {
        Some(p) => match systemctl_user(&["enable", "--now", UNIT_NAME]) {
            Ok(_) => notes.push(format!("daemon unit enabled+started (from {})", p.display())),
            Err(e) => notes.push(format!("WARNING: unit found but systemctl failed: {e}")),
        },
        None => notes.push(
            "WARNING: vm-daemon.service not installed (native package missing?) — \
             UI autostart is on, daemon autostart unavailable"
                .to_string(),
        ),
    }
    notes.join("\n")
}

pub fn cmd_disable() -> String {
    if std::env::var("FLATPAK_ID").is_ok() {
        return sandbox_guidance();
    }
    let mut notes = Vec::new();
    match remove_desktop(&config_home()) {
        Ok(true) => notes.push("ui autostart removed".to_string()),
        Ok(false) => notes.push("ui autostart was not installed".to_string()),
        Err(e) => notes.push(format!("WARNING: {e}")),
    }
    match systemctl_user(&["disable", "--now", UNIT_NAME]) {
        Ok(_) => notes.push("daemon unit disabled+stopped".to_string()),
        Err(e) => notes.push(format!("daemon unit untouched ({e})")),
    }
    notes.join("\n")
}

#[cfg(test)]
mod autostart_tests {
    use super::*;

    fn tmp(n: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vm-autostart-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn desktop_entry_boots_minimized() {
        let text = render_autostart_desktop("/usr/bin/vm-ui");
        assert!(text.contains("Exec=/usr/bin/vm-ui --minimized"));
        assert!(text.contains("X-GNOME-Autostart-enabled=true"));
        assert!(text.contains("Hidden=false"));
    }

    #[test]
    fn install_remove_roundtrip() {
        let dir = tmp("roundtrip");
        let home = dir.join("home");
        let path = install_desktop(&home.join(".config"), "/usr/bin/vm-ui").unwrap();
        assert_eq!(path, home.join(".config/autostart").join(DESKTOP_ID));
        assert!(path.exists());
        assert!(remove_desktop(&home.join(".config")).unwrap());
        assert!(!remove_desktop(&home.join(".config")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unit_lookup_prefers_user_dir() {
        let dir = tmp("lookup");
        assert!(find_unit(&dir).is_none());
        let user_unit = dir.join(".config/systemd/user");
        std::fs::create_dir_all(&user_unit).unwrap();
        std::fs::write(user_unit.join(UNIT_NAME), "[Unit]").unwrap();
        assert_eq!(find_unit(&dir).unwrap(), user_unit.join(UNIT_NAME));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_report_shape() {
        let r = status_report(true, Path::new("/x/y.desktop"), Some(Path::new("/u.service")), "enabled", "active");
        assert!(r.contains("installed"));
        assert!(r.contains("unit enabled: enabled"));
        assert!(r.contains("unit active: active"));
        let r = status_report(false, Path::new("/x/y.desktop"), None, "disabled", "inactive");
        assert!(r.contains("absent") && r.contains("not installed"));
    }
}
