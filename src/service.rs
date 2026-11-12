//! The user systemd service that runs the emitter at login with the saved settings.
//! It has its own name, so a hand-made vban_emitter.service is left alone.

use crate::config::{config_home, state_home};
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const UNIT: &str = "vban-emitter-gui.service";

pub fn unit_path() -> PathBuf {
    config_home().join("systemd/user").join(UNIT)
}

pub fn log_path() -> PathBuf {
    state_home().join("vban-emitter-gui/service.log")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub installed: bool,
    /// systemctl is-enabled: enabled, disabled, not-found...
    pub enabled: String,
    /// systemctl is-active: active, inactive, failed, activating...
    pub active: String,
}

impl Status {
    pub fn running(&self) -> bool {
        self.active == "active" || self.active == "activating" || self.active == "reloading"
    }
}

fn systemctl(args: &[&str]) -> (bool, String) {
    match Command::new("systemctl").arg("--user").args(args).env("LC_ALL", "C").stdin(Stdio::null()).output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if !err.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            (o.status.success(), text)
        }
        Err(e) => (false, format!("systemctl: {e}")),
    }
}

pub fn status() -> Status {
    let installed = unit_path().exists();
    let first = |s: String| s.lines().next().unwrap_or("").to_string();
    Status {
        installed,
        enabled: first(systemctl(&["is-enabled", UNIT]).1),
        active: first(systemctl(&["is-active", UNIT]).1),
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

fn unit_text(exe: &str, config: &str) -> String {
    format!(
        "# Written by vban-emitter-gui. Settings are in {config}; the service reads them each time it starts.\n\
         [Unit]\n\
         Description=VBAN Emitter GUI: sends the VBAN streams that are switched on\n\
         After=pipewire.service pipewire-pulse.service pulseaudio.service\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} --run --config {}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         StandardOutput=truncate:%h/.local/state/vban-emitter-gui/service.log\n\
         StandardError=inherit\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote(exe),
        quote(config)
    )
}

fn step(log: &mut Vec<String>, args: &[&str]) -> bool {
    let (ok, text) = systemctl(args);
    log.push(format!("$ systemctl --user {}{}", args.join(" "), if ok { "" } else { "  (failed)" }));
    log.extend(text.lines().filter(|l| !l.is_empty()).map(|l| format!("  {l}")));
    ok
}

/// Writes the unit, enables it at login and (re)starts it.
pub fn install(config: &std::path::Path) -> Vec<String> {
    let mut log = Vec::new();
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => return vec![format!("can't find this program's path: {e}")],
    };
    let path = unit_path();
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| log_path().parent().map_or(Ok(()), std::fs::create_dir_all))
        .and_then(|_| std::fs::write(&path, unit_text(&exe, &config.to_string_lossy())));
    match written {
        Ok(()) => log.push(format!("wrote {}", path.display())),
        Err(e) => {
            log.push(format!("can't write {}: {e}", path.display()));
            return log;
        }
    }
    let _ = step(&mut log, &["daemon-reload"]) && step(&mut log, &["enable", UNIT]) && step(&mut log, &["restart", UNIT]);
    log
}

pub fn remove() -> Vec<String> {
    let mut log = Vec::new();
    step(&mut log, &["disable", "--now", UNIT]);
    match std::fs::remove_file(unit_path()) {
        Ok(()) => log.push(format!("removed {}", unit_path().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log.push(format!("can't remove {}: {e}", unit_path().display())),
    }
    step(&mut log, &["daemon-reload"]);
    step(&mut log, &["reset-failed", UNIT]);
    log
}

/// start, stop, restart, enable or disable.
pub fn control(verb: &str) -> Vec<String> {
    let mut log = Vec::new();
    step(&mut log, &[verb, UNIT]);
    log
}

/// Another user service that runs VBAN, such as a hand-made vban_emitter.service.
#[derive(Clone, Debug, PartialEq)]
pub struct OldUnit {
    pub name: String,
    pub enabled: String,
    pub active: String,
}

/// User services other than ours whose unit file mentions vban.
pub fn old_units() -> Vec<OldUnit> {
    let dir = config_home().join("systemd/user");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut found = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == UNIT || !name.ends_with(".service") || !e.path().is_file() {
            continue;
        }
        let text = std::fs::read_to_string(e.path()).unwrap_or_default().to_lowercase();
        if !text.contains("vban") && !name.to_lowercase().contains("vban") {
            continue;
        }
        let first = |s: String| s.lines().next().unwrap_or("").to_string();
        found.push(OldUnit {
            enabled: first(systemctl(&["is-enabled", &name]).1),
            active: first(systemctl(&["is-active", &name]).1),
            name,
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Turns another VBAN service off now and at login. Its files stay, so `systemctl --user enable --now` undoes it.
pub fn retire(name: &str) -> Vec<String> {
    let mut log = Vec::new();
    step(&mut log, &["disable", "--now", name]);
    log
}

/// The last lines the service wrote.
pub fn log_tail(lines: usize) -> Vec<String> {
    let Ok(text) = std::fs::read(log_path()) else { return Vec::new() };
    let text = String::from_utf8_lossy(&text);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].iter().map(|s| s.to_string()).collect()
}
