//! Test mode: live level meters for every PulseAudio/PipeWire source (each one a small `parec` recording), and a
//! test tone played on a chosen output, so it's easy to see which source carries what.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

const RATE: u32 = 16000;
/// 40 ms of 2-channel 16-bit audio.
const CHUNK: usize = (RATE as usize / 25) * 4;

fn dies_with_us(c: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    unsafe {
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        })
    }
}

/// Peak levels (0..1, left and right) by source name.
pub type Levels = Arc<Mutex<HashMap<String, [f32; 2]>>>;

#[derive(Default)]
pub struct Meters {
    procs: Vec<Child>,
    pub levels: Levels,
    pub error: Arc<Mutex<Option<String>>>,
}

impl Meters {
    pub fn start(&mut self, sources: &[String], repaint: eframe::egui::Context) {
        self.stop();
        *self.error.lock().unwrap() = None;
        for name in sources {
            let spawned = dies_with_us(Command::new("parec").args([
                "-d",
                name,
                "--raw",
                "--format=s16le",
                &format!("--rate={RATE}"),
                "--channels=2",
                "--latency-msec=40",
                "--client-name=vban-emitter-gui meter",
                "--stream-name=level meter",
            ]))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
            let mut child = match spawned {
                Ok(c) => c,
                Err(e) => {
                    *self.error.lock().unwrap() = Some(format!("can't run parec for the meters: {e} (install pulseaudio-utils)"));
                    return;
                }
            };
            let mut out = child.stdout.take().unwrap();
            let (levels, name, ctx) = (self.levels.clone(), name.clone(), repaint.clone());
            std::thread::spawn(move || {
                let mut buf = vec![0u8; CHUNK];
                let mut shown = [0f32; 2];
                while out.read_exact(&mut buf).is_ok() {
                    let mut peak = [0f32; 2];
                    for frame in buf.chunks_exact(4) {
                        for ch in 0..2 {
                            let s = i16::from_le_bytes([frame[ch * 2], frame[ch * 2 + 1]]) as f32 / 32768.0;
                            peak[ch] = peak[ch].max(s.abs());
                        }
                    }
                    for ch in 0..2 {
                        // Rises at once, falls back gently.
                        shown[ch] = peak[ch].max(shown[ch] * 0.75);
                    }
                    levels.lock().unwrap().insert(name.clone(), shown);
                    ctx.request_repaint();
                }
                levels.lock().unwrap().remove(&name);
            });
            self.procs.push(child);
        }
    }

    pub fn stop(&mut self) {
        for mut c in self.procs.drain(..) {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.levels.lock().unwrap().clear();
    }
}

impl Drop for Meters {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Plays a short three-beep tone on an output (empty `sink` = the default output). Errors go to `done`.
pub fn play_tone(sink: &str, done: impl FnOnce(Result<(), String>) + Send + 'static) {
    let sink = sink.to_string();
    std::thread::spawn(move || {
        let mut cmd = Command::new("pacat");
        cmd.args(["--playback", "--raw", "--format=s16le", "--rate=48000", "--channels=2", "--client-name=vban-emitter-gui", "--stream-name=test tone"]);
        if !sink.is_empty() {
            cmd.args(["-d", &sink]);
        }
        let result = dies_with_us(&mut cmd).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn();
        let mut child = match result {
            Ok(c) => c,
            Err(e) => return done(Err(format!("can't run pacat: {e} (install pulseaudio-utils)"))),
        };
        let mut pcm = Vec::with_capacity(48000 * 4 * 2);
        for beep in 0..3 {
            let freq = [660.0f32, 880.0, 660.0][beep];
            let len = 48000 * 35 / 100;
            for i in 0..len {
                let t = i as f32 / 48000.0;
                let fade = (i.min(len - i) as f32 / 480.0).min(1.0);
                let s = ((t * freq * std::f32::consts::TAU).sin() * 0.3 * fade * 32767.0) as i16;
                pcm.extend_from_slice(&s.to_le_bytes());
                pcm.extend_from_slice(&s.to_le_bytes());
            }
            pcm.extend(std::iter::repeat(0u8).take(48000 * 15 / 100 * 4));
        }
        let wrote = child.stdin.take().map(|mut i| i.write_all(&pcm));
        let status = child.wait_with_output();
        done(match (wrote, status) {
            (Some(Err(e)), _) => Err(format!("pacat: {e}")),
            (_, Ok(o)) if !o.status.success() => Err(format!("pacat: {}", String::from_utf8_lossy(&o.stderr).trim())),
            (_, Err(e)) => Err(format!("pacat: {e}")),
            _ => Ok(()),
        });
    });
}
