//! The tray icon, and the one-copy-at-a-time lock. Closing the window hides it to the tray; the service (and a run
//! started from the window) keeps sending either way. Built the same way as est-gateway's tray.

use eframe::egui;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

pub const TITLE: &str = "VBAN Emitter GUI";
const ICON_SIZE: u32 = 32;

/// The app's icon as RGBA pixels: a dark rounded square with a cyan dot and an amber ring, drawn here so the app
/// needs no image files. Used for the tray.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let n = size as f32;
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded corners: radius 1/8 of the size.
            let r = n / 8.0;
            let cx = fx.clamp(r, n - r);
            let cy = fy.clamp(r, n - r);
            let in_shape = (fx - cx).powi(2) + (fy - cy).powi(2) <= r * r;
            let d = ((fx - n / 2.0).powi(2) + (fy - n / 2.0).powi(2)).sqrt() / n;
            let pixel = if !in_shape {
                [0, 0, 0, 0]
            } else if d <= 0.16 {
                [63, 193, 217, 255]
            } else if (0.297..=0.375).contains(&d) {
                [224, 164, 58, 255]
            } else {
                [33, 39, 58, 255]
            };
            pixels.extend_from_slice(&pixel);
        }
    }
    pixels
}

/// The window's (and taskbar's) icon, the same picture as packaging/vban-emitter-gui.svg: a speaker with two sound
/// bars. Drawn at 4x4 samples per pixel for smooth edges.
pub fn window_icon_rgba(size: u32) -> Vec<u8> {
    let scale = 64.0 / size as f32;
    let blend = |under: [f32; 4], over: [f32; 3], a: f32| -> [f32; 4] {
        [under[0] * (1.0 - a) + over[0] * a, under[1] * (1.0 - a) + over[1] * a, under[2] * (1.0 - a) + over[2] * a, under[3] + (1.0 - under[3]) * a]
    };
    let (bg, edge, cyan, amber) = ([33.0, 39.0, 58.0], [58.0, 69.0, 87.0], [63.0, 193.0, 217.0], [224.0, 164.0, 58.0]);
    // Distance from a point to a rounded rectangle's outline (negative inside).
    let rounded = |x: f32, y: f32| {
        let (qx, qy) = ((x - 32.0).abs() - 18.0, (y - 32.0).abs() - 18.0);
        (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - 10.0
    };
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut sum = [0f32; 4];
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = (px as f32 + (sx as f32 + 0.5) / 4.0) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / 4.0) * scale;
                    let mut c = [0f32; 4];
                    let d = rounded(x, y);
                    if d <= 1.0 {
                        c = [bg[0], bg[1], bg[2], 1.0];
                        if d > -1.0 {
                            c = [edge[0], edge[1], edge[2], 1.0];
                        }
                    }
                    // The speaker: a box and a cone, as one outline.
                    const SPEAKER: [(f32, f32); 6] = [(12.0, 26.0), (20.0, 26.0), (30.0, 16.0), (30.0, 48.0), (20.0, 38.0), (12.0, 38.0)];
                    let mut inside = false;
                    for k in 0..SPEAKER.len() {
                        let (a, b) = (SPEAKER[k], SPEAKER[(k + 1) % SPEAKER.len()]);
                        if (a.1 > y) != (b.1 > y) && x < a.0 + (y - a.1) / (b.1 - a.1) * (b.0 - a.0) {
                            inside = !inside;
                        }
                    }
                    if inside {
                        c = blend(c, cyan, 1.0);
                    }
                    // The two sound bars: arcs around (30, 32) on the right-hand side, 4.5 wide, round ends.
                    for (r, color) in [(9.0f32, cyan), (17.0, amber)] {
                        let (dx, dy) = (x - 30.0, y - 32.0);
                        let dist = (dx * dx + dy * dy).sqrt();
                        let half = (r * r - (r * 0.55) * (r * 0.55)).sqrt();
                        let on_arc = dx >= r * 0.55 && (dist - r).abs() <= 2.25;
                        let near_end = [(30.0 + r * 0.55, 32.0 - half), (30.0 + r * 0.55, 32.0 + half)]
                            .iter()
                            .any(|(ex, ey)| (x - ex).powi(2) + (y - ey).powi(2) <= 2.25 * 2.25);
                        if on_arc || near_end {
                            c = blend(c, color, 1.0);
                        }
                    }
                    for k in 0..4 {
                        sum[k] += if k < 3 { c[k] * c[3] } else { c[3] };
                    }
                }
            }
            let a = sum[3] / 16.0;
            let rgb = |k: usize| if sum[3] > 0.0 { (sum[k] / sum[3]).round().clamp(0.0, 255.0) as u8 } else { 0 };
            pixels.extend_from_slice(&[rgb(0), rgb(1), rgb(2), (a * 255.0).round() as u8]);
        }
    }
    pixels
}

fn icon() -> Result<Icon, String> {
    Icon::from_rgba(icon_rgba(ICON_SIZE), ICON_SIZE, ICON_SIZE).map_err(|e| e.to_string())
}

/// Set when something asks for the window, so the app knows it's back on screen (a hidden window can't tell).
static SHOWN: AtomicBool = AtomicBool::new(false);

pub fn take_shown() -> bool {
    SHOWN.swap(false, Ordering::SeqCst)
}

pub fn show_window(ctx: &egui::Context) {
    SHOWN.store(true, Ordering::SeqCst);
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

pub fn hide_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
}

fn handle_events(ctx: egui::Context, show: MenuId, quit: MenuId, exit_requested: Arc<AtomicBool>) {
    loop {
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == show {
                show_window(&ctx);
            } else if event.id == quit {
                exit_requested.store(true, Ordering::SeqCst);
                show_window(&ctx);
            }
        }
        if let Ok(TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }) =
            TrayIconEvent::receiver().try_recv()
        {
            show_window(&ctx);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The icon needs GTK's own loop, on a thread of its own. Fails when there's no tray to put it in.
pub fn start(ctx: &egui::Context, exit_requested: Arc<AtomicBool>) -> Result<(), String> {
    let ctx = ctx.clone();
    let (result_sender, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Err(e) = gtk::init() {
            let _ = result_sender.send(Err(e.to_string()));
            return;
        }
        let show = MenuItem::new("Show", true, None);
        let quit = MenuItem::new("Exit", true, None);
        let menu = Menu::new();
        let built = menu
            .append(&show)
            .and_then(|_| menu.append(&quit))
            .map_err(|e| e.to_string())
            .and_then(|_| icon())
            .and_then(|icon| {
                TrayIconBuilder::new()
                    .with_menu(Box::new(menu))
                    .with_tooltip(TITLE)
                    .with_icon(icon)
                    .build()
                    .map_err(|e| e.to_string())
            });
        match built {
            Ok(tray) => {
                let _ = result_sender.send(Ok(()));
                let (show, quit) = (show.id().clone(), quit.id().clone());
                std::thread::spawn(move || handle_events(ctx, show, quit, exit_requested));
                let _keep = tray;
                gtk::main();
            }
            Err(e) => {
                let _ = result_sender.send(Err(e));
            }
        }
    });
    result.recv().map_err(|e| e.to_string())?
}

fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let user = unsafe { libc::getuid() };
    dir.join(format!("vban-emitter-gui-{user}.sock"))
}

/// Becomes the only running copy for this user, or returns None after asking the running one to show its window.
/// A copy started with --tray doesn't ask, as it was meant to stay out of sight.
pub fn claim_single_instance(quiet: bool) -> Option<UnixListener> {
    let path = socket_path();
    match UnixListener::bind(&path) {
        Ok(l) => Some(l),
        Err(_) => {
            if let Ok(mut s) = UnixStream::connect(&path) {
                if !quiet {
                    use std::io::Write;
                    let _ = s.write_all(b"show");
                }
                return None;
            }
            // Nobody answers: left over from a copy that crashed.
            let _ = std::fs::remove_file(&path);
            UnixListener::bind(&path).ok()
        }
    }
}

/// Each connection that says "show" is another copy asking for the window.
pub fn show_on_request(listener: UnixListener, ctx: egui::Context) {
    for conn in listener.incoming().flatten() {
        use std::io::Read;
        let mut buf = [0u8; 8];
        let mut conn = conn;
        let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
        if matches!(conn.read(&mut buf), Ok(n) if n > 0) {
            show_window(&ctx);
        }
    }
}

pub fn release_single_instance() {
    let _ = std::fs::remove_file(socket_path());
}
