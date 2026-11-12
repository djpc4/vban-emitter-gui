//! The settings: one shared input (backend and program) and any number of outgoing streams, the config file they
//! live in, and the vban_emitter command line for each stream.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

/// The backends vban_emitter knows. JACK is listed by `--help` but the emitter refuses it, so it isn't offered.
pub const BACKENDS: &[&str] = &["pulseaudio", "alsa", "pipe", "file"];
pub const FORMATS: &[&str] = &["16I", "24I", "32I", "32F", "8I", "64F", "12I", "10I"];
/// The sample rates the VBAN protocol has a code for; others can't be sent.
pub const RATES: &[u32] = &[
    6000, 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000, 128000, 176400,
    192000, 256000, 352800, 384000, 512000, 705600,
];
/// Capture whatever the default output plays. PulseAudio and PipeWire both understand this name.
pub const DEFAULT_MONITOR: &str = "@DEFAULT_MONITOR@";

/// One outgoing VBAN stream: one vban_emitter.
#[derive(Clone, PartialEq, Debug)]
pub struct Stream {
    /// Sent by the window's Start and by the service.
    pub on: bool,
    pub name: String,
    pub ip: String,
    pub port: u16,
    /// The PulseAudio/PipeWire source to capture: a source name, DEFAULT_MONITOR, or empty for the default input.
    /// The pulseaudio backend opens it directly; with alsa the emitter's stream is moved to it after it starts.
    pub source: String,
    /// `-d` for the alsa, pipe and file backends.
    pub device: String,
    pub rate: u32,
    pub channels: u32,
    pub format: String,
    /// `-c`: which device channels to send, such as "1,2". Empty sends them all.
    pub channel_map: String,
    pub log_level: u8,
    /// `-x`: the audio device buffer size (vban 2.2.0 and later). 0 leaves the emitter's default (1024).
    pub buffer_size: u32,
}

impl Default for Stream {
    fn default() -> Self {
        Stream {
            on: true,
            name: "Stream1".into(),
            ip: String::new(),
            port: 6980,
            source: DEFAULT_MONITOR.into(),
            device: String::new(),
            rate: 48000,
            channels: 2,
            format: "16I".into(),
            channel_map: String::new(),
            log_level: 1,
            buffer_size: 0,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Config {
    /// The vban_emitter program.
    pub program: String,
    pub backend: String,
    pub streams: Vec<Stream>,
}

/// The vban_emitter the package ships.
pub const BUNDLED: &str = "/usr/lib/vban-emitter-gui/vban_emitter";

impl Default for Config {
    fn default() -> Self {
        let program = if Path::new(BUNDLED).exists() {
            BUNDLED
        } else if Path::new("/usr/local/bin/vban_emitter").exists() {
            "/usr/local/bin/vban_emitter"
        } else {
            "vban_emitter"
        };
        Config { program: program.into(), backend: "pulseaudio".into(), streams: vec![Stream::default()] }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn config_home() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home().join(".config"),
    }
}

pub fn state_home() -> PathBuf {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home().join(".local/state"),
    }
}

pub fn default_path() -> PathBuf {
    config_home().join("vban-emitter-gui/emitter.conf")
}

impl Config {
    /// Reads the settings. A file from before streams existed (no `[stream]` lines) becomes one stream.
    pub fn load(path: &Path) -> Config {
        let mut c = Config::default();
        let Ok(text) = std::fs::read_to_string(path) else { return c };
        c.streams.clear();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if line.eq_ignore_ascii_case("[stream]") {
                c.streams.push(Stream::default());
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim().to_string());
            match k {
                "program" => c.program = v,
                "backend" => c.backend = v,
                _ => {
                    if c.streams.is_empty() {
                        c.streams.push(Stream::default());
                    }
                    let s = c.streams.last_mut().unwrap();
                    match k {
                        "on" => s.on = matches!(v.as_str(), "true" | "yes" | "1"),
                        "name" | "stream" => s.name = v,
                        "ip" => s.ip = v,
                        "port" => s.port = v.parse().unwrap_or(s.port),
                        "source" => s.source = v,
                        "device" => s.device = v,
                        "rate" => s.rate = v.parse().unwrap_or(s.rate),
                        "channels" => s.channels = v.parse().unwrap_or(s.channels),
                        "format" => s.format = v,
                        "channel_map" => s.channel_map = v,
                        "log_level" => s.log_level = v.parse().unwrap_or(s.log_level),
                        "buffer_size" => s.buffer_size = v.parse().unwrap_or(s.buffer_size),
                        _ => {}
                    }
                }
            }
        }
        if c.streams.is_empty() {
            c.streams.push(Stream::default());
        }
        c
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = format!(
            "# vban-emitter-gui settings. The user service reads this file each time it starts.\n\
             program = {}\nbackend = {}\n",
            self.program, self.backend
        );
        for s in &self.streams {
            text.push_str(&format!(
                "\n[stream]\non = {}\nname = {}\nip = {}\nport = {}\nsource = {}\ndevice = {}\nrate = {}\nchannels = {}\n\
                 format = {}\nchannel_map = {}\nlog_level = {}\nbuffer_size = {}\n",
                s.on, s.name, s.ip, s.port, s.source, s.device, s.rate, s.channels, s.format, s.channel_map,
                s.log_level, s.buffer_size
            ));
        }
        let tmp = path.with_extension("conf.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    /// What's wrong with the shared settings and the streams that are on; empty when they can all be started.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.program.trim().is_empty() {
            p.push("The vban_emitter program path is empty.".into());
        }
        if !BACKENDS.contains(&self.backend.as_str()) {
            p.push(format!("Unknown backend {}.", self.backend));
        }
        let on: Vec<&Stream> = self.streams.iter().filter(|s| s.on).collect();
        if on.is_empty() {
            p.push("No stream is switched on.".into());
        }
        for s in &on {
            p.extend(s.problems(&self.backend).into_iter().map(|e| format!("{}: {e}", s.label())));
        }
        for (i, a) in on.iter().enumerate() {
            if on[..i].iter().any(|b| b.name.trim() == a.name.trim() && b.ip.trim() == a.ip.trim() && b.port == a.port) {
                p.push(format!("Two streams named {} both go to {}:{}.", a.label(), a.ip.trim(), a.port));
            }
        }
        p
    }

    pub fn command_line(&self, s: &Stream) -> String {
        std::iter::once(self.program.clone())
            .chain(s.args(&self.backend))
            .map(|a| if a.is_empty() || a.contains([' ', '"', '\'', '$']) { format!("'{a}'") } else { a })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A name not used by any stream yet: Stream1, Stream2...
    pub fn free_name(&self) -> String {
        (1..).map(|n| format!("Stream{n}")).find(|n| !self.streams.iter().any(|s| &s.name == n)).unwrap()
    }
}

impl Stream {
    /// The name, or a stand-in when it's empty.
    pub fn label(&self) -> String {
        if self.name.trim().is_empty() { "(unnamed)".into() } else { self.name.trim().into() }
    }

    pub fn problems(&self, backend: &str) -> Vec<String> {
        let mut p = Vec::new();
        if self.ip.trim().parse::<Ipv4Addr>().is_err() {
            p.push("the destination IP must be an IPv4 address, such as 192.168.1.50".into());
        }
        if self.port == 0 {
            p.push("the port can't be 0".into());
        }
        let n = self.name.trim();
        if n.is_empty() || n.len() > 16 || !n.is_ascii() {
            p.push("the stream name must be 1 to 16 plain (ASCII) characters".into());
        }
        if !RATES.contains(&self.rate) {
            p.push(format!("{} Hz is not a VBAN sample rate", self.rate));
        }
        if !(1..=256).contains(&self.channels) {
            p.push("channels must be 1 to 256".into());
        }
        if !FORMATS.contains(&self.format.as_str()) {
            p.push(format!("unknown sample format {}", self.format));
        } else if backend == "pulseaudio" && matches!(self.format.as_str(), "64F" | "12I" | "10I") {
            p.push(format!("the pulseaudio backend can't capture {}; use 16, 24 or 32-bit", self.format));
        }
        let map = self.channel_map.trim();
        if !map.is_empty() && !map.split(',').all(|x| x.trim().parse::<u32>().map_or(false, |n| n >= 1)) {
            p.push("the channel map must be channel numbers separated by commas, such as 1,2".into());
        }
        if matches!(backend, "pipe" | "file") && self.device.trim().is_empty() {
            p.push(format!("the {backend} backend needs a file or pipe path (⚙)"));
        }
        p
    }

    /// vban_emitter's arguments, without the program name.
    pub fn args(&self, backend: &str) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-i".into(), self.ip.trim().into(),
            "-p".into(), self.port.to_string(),
            "-s".into(), self.name.trim().into(),
            "-b".into(), backend.into(),
        ];
        let device = if backend == "pulseaudio" { self.source.trim() } else { self.device.trim() };
        if !device.is_empty() {
            a.push("-d".into());
            a.push(device.into());
        }
        a.extend([
            "-r".into(), self.rate.to_string(),
            "-n".into(), self.channels.to_string(),
            "-f".into(), self.format.clone(),
        ]);
        let map: String = self.channel_map.chars().filter(|c| !c.is_whitespace()).collect();
        if !map.is_empty() {
            a.push("-c".into());
            a.push(map);
        }
        if self.buffer_size > 0 {
            a.push("-x".into());
            a.push(self.buffer_size.to_string());
        }
        a.push("-l".into());
        a.push(self.log_level.to_string());
        a
    }

    /// With the alsa backend the capture source is chosen by moving the emitter's stream after it starts.
    pub fn needs_move(&self, backend: &str) -> bool {
        backend == "alsa" && !self.source.trim().is_empty()
    }

    /// Settings read from a running vban_emitter's command line, on top of these ones, and the backend it used.
    pub fn from_args(&self, args: &[String]) -> (Stream, String) {
        let mut c = self.clone();
        let (mut device, mut backend) = (None, "alsa".to_string());
        c.channel_map.clear();
        c.buffer_size = 0;
        let mut i = 0;
        while i < args.len() {
            let (flag, val, step) = match args[i].split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string()), 1),
                _ => (args[i].clone(), args.get(i + 1).cloned(), 2),
            };
            let Some(v) = val else { break };
            match flag.as_str() {
                "-i" | "--ipaddress" => c.ip = v,
                "-p" | "--port" => c.port = v.parse().unwrap_or(c.port),
                "-s" | "--streamname" => c.name = v,
                "-b" | "--backend" => backend = v,
                "-d" | "--device" => device = Some(v),
                "-r" | "--rate" => c.rate = v.parse().unwrap_or(c.rate),
                "-n" | "--nbchannels" => c.channels = v.parse().unwrap_or(c.channels),
                "-f" | "--format" => c.format = v,
                "-c" | "--channels" => c.channel_map = v,
                "-l" | "--loglevel" => c.log_level = v.parse().unwrap_or(c.log_level),
                "-x" | "--bufsize" => c.buffer_size = v.parse().unwrap_or(c.buffer_size),
                _ => {
                    i += 1;
                    continue;
                }
            }
            i += step;
        }
        if backend == "pulseaudio" {
            c.source = device.unwrap_or_default();
        } else {
            c.device = device.unwrap_or_default();
        }
        (c, backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_old_files() {
        let dir = std::env::temp_dir().join(format!("vban-gui-test-{}", std::process::id()));
        let path = dir.join("emitter.conf");
        let mut c = Config::default();
        c.streams[0].ip = "192.168.1.50".into();
        c.streams[0].name = "Desktop".into();
        c.streams[0].channel_map = "1,2".into();
        c.streams.push(Stream { on: false, name: "Mic".into(), source: "alsa_input.x".into(), ..Stream::default() });
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path), c);
        // A settings file from the one-stream version.
        std::fs::write(&path, "program = /x\nip = 192.168.1.50\nport = 6980\nstream = Stream2\nbackend = pulseaudio\nformat = 32I\n").unwrap();
        let old = Config::load(&path);
        assert_eq!((old.program.as_str(), old.backend.as_str(), old.streams.len()), ("/x", "pulseaudio", 1));
        assert_eq!((old.streams[0].name.as_str(), old.streams[0].format.as_str()), ("Stream2", "32I"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn args_and_import() {
        let s = Stream { ip: "192.168.1.50".into(), name: "Desktop".into(), format: "32I".into(), source: String::new(), ..Stream::default() };
        assert_eq!(s.args("alsa").join(" "), "-i 192.168.1.50 -p 6980 -s Desktop -b alsa -r 48000 -n 2 -f 32I -l 1");
        let line: Vec<String> = "-b alsa -i 192.168.1.50 -p 6980 -s Desktop -r 48000 -n 2 -f 32I".split(' ').map(String::from).collect();
        let (d, backend) = Stream::default().from_args(&line);
        assert_eq!((d.ip.as_str(), d.port, d.name.as_str(), backend.as_str(), d.format.as_str()), ("192.168.1.50", 6980, "Desktop", "alsa", "32I"));
        assert!(d.problems(&backend).is_empty());
        let bad = Stream { format: "64F".into(), ..d.clone() };
        assert_eq!(bad.problems("pulseaudio").len(), 1);
        let mut c = Config::default();
        c.streams = vec![d.clone(), d];
        assert!(c.problems().iter().any(|p| p.starts_with("Two streams")));
        c.streams[1].on = false;
        assert!(c.problems().is_empty());
        assert_eq!(c.free_name(), "Stream1");
    }
}
