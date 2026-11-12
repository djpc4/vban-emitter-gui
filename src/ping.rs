//! Checks that the receivers are there: VBAN's service "ping", which Voicemeeter and other VB-Audio apps answer with
//! their name. Sending audio gets no answer, so this is the only sign that anything is listening.
//! Voicemeeter answers to the port the ping went to, on this computer, so we listen on each destination port.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Answer {
    pub at: Instant,
    /// Such as "Voicemeeter Potato on Windows PC".
    pub who: String,
}

#[derive(Default)]
pub struct Shared {
    /// Where to ping: (ip, port).
    pub targets: Vec<(Ipv4Addr, u16)>,
    /// The last answer from each receiver.
    pub answers: HashMap<Ipv4Addr, Answer>,
    /// Ports we couldn't listen on (something else holds them).
    pub busy_ports: HashSet<u16>,
}

fn text(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).trim().to_string()
}

/// A PING0 request: the VBAN header for the service sub-protocol, then a description of ourselves.
fn request(counter: u32) -> Vec<u8> {
    let mut p = b"VBAN".to_vec();
    p.extend_from_slice(&[0x60, 0x00, 0x00, 0x00]);
    let mut name = b"VBAN Service".to_vec();
    name.resize(16, 0);
    p.extend_from_slice(&name);
    p.extend_from_slice(&counter.to_le_bytes());
    let fixed = |s: &str, n: usize| {
        let mut v = s.as_bytes().to_vec();
        v.resize(n, 0);
        v
    };
    for v in [1u32, 1, 0, 48000, 8000, 96000, 0x3fc1d9] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    p.extend_from_slice(&[0, 1, 0, 0]);
    p.extend_from_slice(&[0u8; 16]);
    p.extend_from_slice(&fixed("en", 8));
    p.extend_from_slice(&[0u8; 8 + 64 + 32 + 4]);
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    for s in ["Linux", "vban-emitter-gui", "vban-emitter-gui", host.trim()] {
        p.extend_from_slice(&fixed(s, 64));
    }
    p.extend_from_slice(&[0u8; 256]);
    p
}

/// Pings every target every 3 s and records who answers. Runs for the life of the window.
pub fn start(shared: Arc<Mutex<Shared>>) {
    std::thread::spawn(move || {
        let mut sockets: HashMap<u16, UdpSocket> = HashMap::new();
        let mut counter = 0u32;
        let mut last_ping = Instant::now() - Duration::from_secs(10);
        loop {
            let targets = shared.lock().unwrap().targets.clone();
            for (_, port) in &targets {
                if sockets.contains_key(port) || shared.lock().unwrap().busy_ports.contains(port) {
                    continue;
                }
                match UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, *port)) {
                    Ok(s) => {
                        let _ = s.set_nonblocking(true);
                        sockets.insert(*port, s);
                    }
                    Err(_) => {
                        shared.lock().unwrap().busy_ports.insert(*port);
                    }
                }
            }
            if last_ping.elapsed() >= Duration::from_secs(3) {
                last_ping = Instant::now();
                for (ip, port) in &targets {
                    if let Some(s) = sockets.get(port) {
                        counter = counter.wrapping_add(1);
                        let _ = s.send_to(&request(counter), SocketAddrV4::new(*ip, *port));
                    }
                }
            }
            let mut buf = [0u8; 2048];
            for s in sockets.values() {
                while let Ok((n, from)) = s.recv_from(&mut buf) {
                    let d = &buf[..n];
                    // A PING0 reply: service sub-protocol, reply bit set, then the 676-byte description.
                    if n >= 28 + 420 && &d[..4] == b"VBAN" && d[4] == 0x60 && d[5] & 0x80 != 0 {
                        let p = &d[28..];
                        let device = text(&p[164..228]);
                        let app = text(&p[292..356]);
                        let who = match (app.is_empty(), device.is_empty()) {
                            (false, false) => format!("{app} on {device}"),
                            (false, true) => app,
                            (true, false) => device,
                            _ => "a VBAN receiver".into(),
                        };
                        if let std::net::SocketAddr::V4(v4) = from {
                            shared.lock().unwrap().answers.insert(*v4.ip(), Answer { at: Instant::now(), who });
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
}
