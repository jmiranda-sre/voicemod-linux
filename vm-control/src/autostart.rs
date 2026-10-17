//! Autostart management (F-13): XDG desktop entry for the UI plus the
//! `systemd --user` unit for the daemon ("install and forget").
//!
//! Shared by `vm autostart` (CLI) and the vm-ui Preferences dialog —
//! one implementation, two front-ends, so they can never disagree.
//!
//! Split for testability: path rendering + file ops are pure over
//! injected dirs, and `systemctl` sits behind an injected runner
//! (unit-tested with mocks — tests never touch the live session). In
//! the Flatpak sandbox everything degrades to portal guidance
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

/// Inside the Flatpak sandbox, autostart is owned by the permission
/// store (same detection as the hotkeys sandbox note).
pub fn sandboxed() -> bool {
    std::env::var("FLATPAK_ID").is_ok() || Path::new("/.flatpak-info").exists()
}

pub fn sandbox_guidance() -> String {
    "sandboxed (Flatpak): autostart is owned by the Flatpak permission \
     store, not by systemd/XDG files here. Enable Background permission \
     for the app (software center or Flatseal) and launch the UI from \
     the app grid — the tray is unavailable in the sandbox, the window \
     stays a normal window."
        .to_string()
}

pub fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
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

/// Live autostart state as data (not text): the CLI prints it, the UI
/// renders it. `unit_*` fields carry the raw `systemctl` output — or
/// the error text when the query fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub sandboxed: bool,
    /// `$XDG_CONFIG_HOME/autostart/<id>` present → the UI starts at login.
    pub desktop_installed: bool,
    pub desktop_path: PathBuf,
    /// Where the daemon unit was found, if the package provides it.
    pub unit_path: Option<PathBuf>,
    pub unit_enabled: String,
    pub unit_active: String,
}

impl Status {
    /// One-line daemon-autostart summary for the UI (pure over fields).
    pub fn daemon_summary(&self) -> String {
        if self.unit_path.is_none() {
            return "daemon autostart: not installed".to_string();
        }
        let enabled = matches!(self.unit_enabled.as_str(), "enabled" | "enabled-runtime");
        let running = matches!(self.unit_active.as_str(), "active" | "activating");
        match (enabled, running) {
            (true, true) => "daemon autostart: enabled, running now".to_string(),
            (true, false) => "daemon autostart: enabled (stopped now)".to_string(),
            (false, true) => "daemon autostart: running (not enabled at login)".to_string(),
            (false, false) => "daemon autostart: not enabled".to_string(),
        }
    }
}

/// Query the live autostart state (file probes + two systemctl reads).
pub fn status() -> Status {
    let home = home_dir();
    status_with(sandboxed(), &config_home(), find_unit(&home), &systemctl_user)
}

fn status_with(
    sandboxed: bool,
    config_home: &Path,
    unit: Option<PathBuf>,
    systemctl: &dyn Fn(&[&str]) -> Result<String, String>,
) -> Status {
    let desktop_path = autostart_desktop_path(config_home);
    let desktop_installed = desktop_path.exists();
    if sandboxed {
        // The records live in the permission store; systemctl (and the
        // unit files) are meaningless here — don't shell out.
        return Status {
            sandboxed,
            desktop_installed,
            desktop_path,
            unit_path: unit,
            unit_enabled: String::new(),
            unit_active: String::new(),
        };
    }
    let unit_enabled = systemctl(&["is-enabled", UNIT_NAME]).unwrap_or_else(|e| e);
    let unit_active = systemctl(&["is-active", UNIT_NAME]).unwrap_or_else(|e| e);
    Status {
        sandboxed,
        desktop_installed,
        desktop_path,
        unit_path: unit,
        unit_enabled,
        unit_active,
    }
}

/// Install the XDG entry and enable+start the daemon unit. `Ok(notes)`
/// carries what happened (WARNING notes included); `Err` means the
/// desktop entry could not be written.
pub fn enable() -> Result<Vec<String>, String> {
    if sandboxed() {
        return Err(sandbox_guidance());
    }
    let home = home_dir();
    enable_with(&config_home(), find_unit(&home).as_deref(), &ui_exec(), &systemctl_user)
}

fn enable_with(
    config_home: &Path,
    unit: Option<&Path>,
    exec: &str,
    systemctl: &dyn Fn(&[&str]) -> Result<String, String>,
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    let p = install_desktop(config_home, exec)?;
    notes.push(format!("ui autostart installed: {}", p.display()));
    match unit {
        Some(p) => match systemctl(&["enable", "--now", UNIT_NAME]) {
            Ok(_) => notes.push(format!("daemon unit enabled+started (from {})", p.display())),
            Err(e) => notes.push(format!("WARNING: unit found but systemctl failed: {e}")),
        },
        None => notes.push(
            "WARNING: vm-daemon.service not installed (native package missing?) — \
             UI autostart is on, daemon autostart unavailable"
                .to_string(),
        ),
    }
    Ok(notes)
}

/// Remove the XDG entry and disable+stop the daemon unit. Best-effort:
/// always returns notes (warnings included), never fails as a whole.
pub fn disable() -> Vec<String> {
    if sandboxed() {
        return vec![sandbox_guidance()];
    }
    disable_with(&config_home(), &systemctl_user)
}

fn disable_with(
    config_home: &Path,
    systemctl: &dyn Fn(&[&str]) -> Result<String, String>,
) -> Vec<String> {
    let mut notes = Vec::new();
    match remove_desktop(config_home) {
        Ok(true) => notes.push("ui autostart removed".to_string()),
        Ok(false) => notes.push("ui autostart was not installed".to_string()),
        Err(e) => notes.push(format!("WARNING: {e}")),
    }
    match systemctl(&["disable", "--now", UNIT_NAME]) {
        Ok(_) => notes.push("daemon unit disabled+stopped".to_string()),
        Err(e) => notes.push(format!("daemon unit untouched ({e})")),
    }
    notes
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

pub fn cmd_status() -> String {
    let st = status();
    if st.sandboxed {
        return sandbox_guidance();
    }
    status_report(
        st.desktop_installed,
        &st.desktop_path,
        st.unit_path.as_deref(),
        &st.unit_enabled,
        &st.unit_active,
    )
}

pub fn cmd_enable() -> String {
    if sandboxed() {
        return sandbox_guidance();
    }
    match enable() {
        Ok(notes) => notes.join("\n"),
        Err(e) => format!("enable failed: {e}"),
    }
}

pub fn cmd_disable() -> String {
    if sandboxed() {
        return sandbox_guidance();
    }
    disable().join("\n")
}

#[cfg(test)]
mod autostart_tests {
    use super::*;
    use std::cell::RefCell;

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

    #[test]
    fn enable_installs_entry_and_warns_without_unit() {
        // With no unit found, enable must not call systemctl at all.
        let dir = tmp("enable-no-unit");
        let ch = dir.join("config");
        let runner = |_: &[&str]| -> Result<String, String> {
            panic!("systemctl must not be called when the unit is absent")
        };
        let notes = enable_with(&ch, None, "/usr/bin/vm-ui", &runner).unwrap();
        assert!(notes[0].starts_with("ui autostart installed:"));
        assert!(notes[1].starts_with("WARNING: vm-daemon.service not installed"));
        assert!(autostart_desktop_path(&ch).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enable_enables_unit_when_found() {
        let dir = tmp("enable-unit");
        let calls = RefCell::new(Vec::new());
        let runner = |args: &[&str]| -> Result<String, String> {
            calls.borrow_mut().push(args.join(" "));
            Ok(String::new())
        };
        let unit = dir.join(UNIT_NAME);
        let notes = enable_with(&dir.join("config"), Some(&unit), "/usr/bin/vm-ui", &runner).unwrap();
        assert!(notes.iter().any(|n| n.contains("daemon unit enabled+started")));
        assert_eq!(calls.borrow().as_slice(), ["enable --now vm-daemon.service"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disable_reports_notes_for_all_branches() {
        let dir = tmp("disable");
        let ch = dir.join("config");
        let ok = |_: &[&str]| -> Result<String, String> { Ok(String::new()) };
        let notes = disable_with(&ch, &ok);
        assert_eq!(notes, ["ui autostart was not installed", "daemon unit disabled+stopped"]);
        install_desktop(&ch, "/usr/bin/vm-ui").unwrap();
        let notes = disable_with(&ch, &ok);
        assert_eq!(notes[0], "ui autostart removed");
        let err = |_: &[&str]| -> Result<String, String> { Err("no session bus".into()) };
        let notes = disable_with(&ch, &err);
        assert_eq!(notes[1], "daemon unit untouched (no session bus)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_maps_unit_state_and_skips_systemctl_in_sandbox() {
        let dir = tmp("status");
        let ch = dir.join("config");
        let runner = |args: &[&str]| -> Result<String, String> {
            match args[0] {
                "is-enabled" => Ok("enabled".into()),
                _ => Ok("active".into()),
            }
        };
        let st = status_with(false, &ch, Some(dir.join(UNIT_NAME)), &runner);
        assert!(!st.desktop_installed && st.unit_path.is_some());
        assert_eq!(st.daemon_summary(), "daemon autostart: enabled, running now");
        // Sandbox: never shells out.
        let runner = |_: &[&str]| -> Result<String, String> {
            panic!("systemctl must not be called in the sandbox branch")
        };
        let st = status_with(true, &ch, None, &runner);
        assert!(st.sandboxed && st.unit_active.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_summary_variants() {
        let sum = |unit: Option<&str>, enabled: &str, active: &str| Status {
            sandboxed: false,
            desktop_installed: false,
            desktop_path: PathBuf::from("/x"),
            unit_path: unit.map(PathBuf::from),
            unit_enabled: enabled.to_string(),
            unit_active: active.to_string(),
        }
        .daemon_summary();
        assert_eq!(sum(None, "", ""), "daemon autostart: not installed");
        assert_eq!(sum(Some("/u"), "enabled", "active"), "daemon autostart: enabled, running now");
        assert_eq!(sum(Some("/u"), "enabled", "inactive"), "daemon autostart: enabled (stopped now)");
        assert_eq!(sum(Some("/u"), "disabled", "active"), "daemon autostart: running (not enabled at login)");
        assert_eq!(sum(Some("/u"), "disabled", "inactive"), "daemon autostart: not enabled");
        assert_eq!(sum(Some("/u"), "static", "inactive"), "daemon autostart: not enabled");
    }
}
