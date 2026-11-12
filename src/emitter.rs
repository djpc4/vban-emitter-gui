//! Running vban_emitter: the PulseAudio/PipeWire sources it can capture, moving its stream to one, finding emitters
//! already running, and the headless mode the user service runs (`vban-emitter-gui --run`).

use crate::config::{Config, Stream, DEFAULT_MONITOR};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Source {
    pub name: String,
    pub description: String,
    pub monitor: bool,
}

fn pactl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("pactl: {e} (install pulseaudio-utils)"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("pactl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The sources PulseAudio/PipeWire offer, monitors (what an output plays) first.
pub fn list_sources() -> Result<Vec<Source>, String> {
    let text = pactl(&["list", "sources"])?;
    let mut list: Vec<Source> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if line.starts_with("Source #") {
            list.push(Source { name: String::new(), description: String::new(), monitor: false });
        } else if let Some(s) = list.last_mut() {
            if let Some(v) = t.strip_prefix("Name: ") {
                s.name = v.into();
            } else if let Some(v) = t.strip_prefix("Description: ") {
                s.description = v.into();
            } else if let Some(v) = t.strip_prefix("Monitor of Sink: ") {
                s.monitor = v != "n/a";
            }
        }
    }
    list.retain(|s| !s.name.is_empty());
    list.sort_by_key(|s| !s.monitor);
    Ok(list)
}

/// Outputs (sinks) as (name, description), for the test tone.
pub fn list_sinks() -> Result<Vec<(String, String)>, String> {
    let text = pactl(&["list", "sinks"])?;
    let mut list: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if line.starts_with("Sink #") {
            list.push(Default::default());
        } else if let Some(s) = list.last_mut() {
            if let Some(v) = t.strip_prefix("Name: ") {
                s.0 = v.into();
            } else if let Some(v) = t.strip_prefix("Description: ") {
                s.1 = v.into();
            }
        }
    }
    list.retain(|s| !s.0.is_empty());
    Ok(list)
}

/// The monitor of the default output, which is what DEFAULT_MONITOR means.
pub fn default_monitor() -> Result<String, String> {
    let sink = pactl(&["get-default-sink"])?;
    Ok(format!("{}.monitor", sink.trim()))
}

/// The source-output (recording stream) of a process, and the index of the source it records from.
fn source_output_of(pid: u32) -> Result<Option<(String, String)>, String> {
    let text = pactl(&["list", "source-outputs"])?;
    let want = format!("application.process.id = \"{pid}\"");
    let (mut id, mut source) = (String::new(), String::new());
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = line.strip_prefix("Source Output #") {
            id = v.trim().into();
            source.clear();
        } else if let Some(v) = t.strip_prefix("Source: ") {
            source = v.into();
        } else if t == want && !id.is_empty() {
            return Ok(Some((id, source)));
        }
    }
    Ok(None)
}

/// The name of the source a running emitter records from, if PulseAudio/PipeWire sees its stream.
pub fn current_source_of(pid: u32) -> Option<String> {
    let (_, index) = source_output_of(pid).ok()??;
    let short = pactl(&["list", "short", "sources"]).ok()?;
    short.lines().find_map(|l| {
        let mut f = l.split('\t');
        (f.next()? == index).then(|| f.next().map(String::from)).flatten()
    })
}

/// Waits for the emitter's recording stream to appear, then moves it to `target`. What happened goes to `log`.
pub fn move_stream(pid: u32, target: &str, log: &dyn Fn(String)) -> bool {
    let resolved = match target {
        DEFAULT_MONITOR => default_monitor(),
        "" => pactl(&["get-default-source"]).map(|s| s.trim().to_string()),
        t => Ok(t.to_string()),
    };
    let target = match resolved {
        Ok(t) => t,
        Err(e) => {
            log(format!("[gui] can't find the default device: {e}"));
            return false;
        }
    };
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return false;
        }
        match source_output_of(pid) {
            Ok(Some((id, _))) => {
                return match pactl(&["move-source-output", &id, &target]) {
                    Ok(_) => {
                        log(format!("[gui] capturing from {target} (moved stream #{id})"));
                        true
                    }
                    Err(e) => {
                        log(format!("[gui] couldn't move the stream to {target}: {e}"));
                        false
                    }
                };
            }
            Ok(None) => {}
            Err(e) => {
                log(format!("[gui] {e}"));
                return false;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    log("[gui] the emitter's recording stream never showed up in PulseAudio/PipeWire, so it wasn't moved".into());
    false
}

#[derive(Clone, Debug, PartialEq)]
pub enum Owner {
    /// Started by this window.
    Here,
    /// Run by the vban-emitter-gui user service.
    Service,
    /// Anything else, such as a script.
    Other,
}

#[derive(Clone, Debug)]
pub struct Running {
    pub pid: u32,
    pub args: Vec<String>,
    pub owner: Owner,
}

fn cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
}

fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

impl Running {
    /// The `-s` stream name it sends.
    pub fn stream_name(&self) -> Option<&str> {
        self.args.iter().position(|a| a == "-s").and_then(|i| self.args.get(i + 1)).map(String::as_str)
    }
}

/// Every vban_emitter on this computer. `here` is the runner the window started, if any.
pub fn running(here: Option<u32>) -> Vec<Running> {
    let mut found = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return found };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Some(args) = cmdline(pid) else { continue };
        let Some(first) = args.first() else { continue };
        if first.rsplit('/').next() != Some("vban_emitter") {
            continue;
        }
        let ppid = parent(pid);
        let owner = if here.is_some() && ppid == here {
            Owner::Here
        } else if ppid
            .and_then(cmdline)
            .map_or(false, |p| p.iter().any(|a| a == "--run") && p[0].contains("vban-emitter-gui"))
        {
            Owner::Service
        } else {
            Owner::Other
        };
        found.push(Running { pid, args: args[1..].to_vec(), owner });
    }
    found.sort_by_key(|r| r.pid);
    found
}

/// Whether this vban_emitter has `-x/--bufsize` (vban 2.2.0 and later).
pub fn supports_bufsize(program: &str) -> bool {
    Command::new(program)
        .arg("--help")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_or(false, |o| String::from_utf8_lossy(&o.stdout).contains("--bufsize"))
}

/// Makes a child get SIGTERM if this program dies, so an emitter is never left running alone.
pub fn dies_with_us(c: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    unsafe {
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        })
    }
}

/// One stream's vban_emitter, its output piped. The buffer size is left out when the emitter is too old to take it.
fn spawn_stream(config: &Config, s: &Stream, bufsize_ok: bool) -> std::io::Result<Child> {
    let mut s = s.clone();
    if s.buffer_size > 0 && !bufsize_ok {
        say(&s, &format!("{} has no -x (buffer size) option, so the buffer size is skipped; vban 2.2.0 has it", config.program));
        s.buffer_size = 0;
    }
    say(&s, &format!("starting: {}", config.command_line(&s)));
    let mut cmd = Command::new(&config.program);
    cmd.args(s.args(&config.backend)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = dies_with_us(&mut cmd).spawn()?;
    // Each line of the emitter's output, marked with its stream's name.
    let label = s.label();
    for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)]
    .into_iter()
    .flatten()
    {
        let label = label.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                if !line.trim().is_empty() && !line.contains("vban_emitter version") {
                    println!("{label} | {line}");
                }
            }
        });
    }
    if s.needs_move(&config.backend) {
        let (pid, s2) = (child.id(), s.clone());
        std::thread::spawn(move || {
            move_stream(pid, &s2.source, &|m| say(&s2, m.trim_start_matches("[gui] ")));
        });
    }
    Ok(child)
}

fn say(s: &Stream, what: &str) {
    println!("[gui] {}: {what}", s.label());
}

/// What the user service (and the window's Start) runs: every stream that's on, each restarted on its own if it
/// stops, until this program is stopped. Returns the exit code.
pub fn run_service(path: &std::path::Path) -> i32 {
    let config = Config::load(path);
    println!("[gui] settings from {}", path.display());
    let problems = config.problems();
    if !problems.is_empty() {
        for p in problems {
            println!("[gui] {p}");
        }
        return 2;
    }
    let bufsize_ok = supports_bufsize(&config.program);
    struct Slot {
        stream: Stream,
        child: Option<Child>,
        retry_at: Instant,
        failures: u32,
    }
    let mut slots: Vec<Slot> = config
        .streams
        .iter()
        .filter(|s| s.on)
        .map(|s| Slot { stream: s.clone(), child: None, retry_at: Instant::now(), failures: 0 })
        .collect();
    println!("[gui] sending {} stream(s)", slots.len());
    loop {
        for slot in &mut slots {
            if let Some(child) = &mut slot.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        slot.failures += 1;
                        let wait = (3u64 << (slot.failures - 1).min(4)).min(30);
                        say(&slot.stream, &format!("vban_emitter stopped ({status}); starting it again in {wait} s"));
                        slot.child = None;
                        slot.retry_at = Instant::now() + Duration::from_secs(wait);
                    }
                    Ok(None) => {
                        // A stream that has kept going for a minute counts as healthy again.
                        if slot.failures > 0 && slot.retry_at.elapsed() > Duration::from_secs(60) {
                            slot.failures = 0;
                        }
                    }
                    Err(e) => say(&slot.stream, &e.to_string()),
                }
            } else if Instant::now() >= slot.retry_at {
                match spawn_stream(&config, &slot.stream, bufsize_ok) {
                    Ok(c) => slot.child = Some(c),
                    Err(e) => {
                        say(&slot.stream, &format!("can't start {}: {e}", config.program));
                        slot.failures += 1;
                        slot.retry_at = Instant::now() + Duration::from_secs(10);
                    }
                }
                slot.retry_at = slot.retry_at.max(Instant::now());
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}
