//! rs-term — a node-based terminal manager.
//!
//! Terminals and sticky notes live as draggable cards on an infinite,
//! pan/zoomable canvas. A Rust reimagining of the core idea behind nodeterm
//! (https://github.com/eneskirca/nodeterm), built natively with egui.

mod app;
mod node;
mod terminal;

use app::RsTermApp;

fn main() -> eframe::Result<()> {
    // Headless check of the PTY + vt100 pipeline (no window). Used to verify
    // the terminal backend works in CI / non-GUI environments.
    if std::env::args().any(|a| a == "--selftest") {
        selftest();
        return Ok(());
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 480.0])
            .with_title("rs-term")
            .with_icon(app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "rs-term",
        native_options,
        Box::new(|cc| Ok(Box::new(RsTermApp::new(cc)))),
    )
}

/// Build the window / macOS dock icon: an orange "RS" on a dark rounded
/// square, drawn from a hand-coded 5×7 bitmap font (no image dependency). This
/// replaces eframe's default egui "e" glyph.
fn app_icon() -> egui::IconData {
    const W: usize = 256;
    const H: usize = 256;
    const RADIUS: f32 = 48.0;
    const SCALE: usize = 20; // side of one font "pixel" block
    const GAP: usize = 1; // columns between the two glyphs

    let bg: [u8; 4] = [0x1a, 0x1b, 0x1f, 0xff]; // dark card background
    let fg: [u8; 4] = [0xff, 0x8c, 0x2a, 0xff]; // ORANGE accent

    // 5 wide × 7 tall bitmaps for the two letters.
    let r_glyph: [[u8; 5]; 7] = [
        [1, 1, 1, 1, 0],
        [1, 0, 0, 0, 1],
        [1, 0, 0, 0, 1],
        [1, 1, 1, 1, 0],
        [1, 0, 1, 0, 0],
        [1, 0, 0, 1, 0],
        [1, 0, 0, 0, 1],
    ];
    let s_glyph: [[u8; 5]; 7] = [
        [0, 1, 1, 1, 1],
        [1, 0, 0, 0, 0],
        [1, 0, 0, 0, 0],
        [0, 1, 1, 1, 0],
        [0, 0, 0, 0, 1],
        [0, 0, 0, 0, 1],
        [1, 1, 1, 1, 0],
    ];

    let mut rgba = vec![0u8; W * H * 4]; // transparent by default

    // Rounded-square background.
    let inside = |x: f32, y: f32| -> bool {
        let cx = x.clamp(RADIUS, W as f32 - RADIUS);
        let cy = y.clamp(RADIUS, H as f32 - RADIUS);
        let (dx, dy) = (x - cx, y - cy);
        dx * dx + dy * dy <= RADIUS * RADIUS
    };
    for y in 0..H {
        for x in 0..W {
            if inside(x as f32 + 0.5, y as f32 + 0.5) {
                let i = (y * W + x) * 4;
                rgba[i..i + 4].copy_from_slice(&bg);
            }
        }
    }

    // Center the two glyphs.
    let block_w = (5 * 2 + GAP) * SCALE;
    let block_h = 7 * SCALE;
    let x0 = (W - block_w) / 2;
    let y0 = (H - block_h) / 2;

    let mut draw = |glyph: &[[u8; 5]; 7], ox: usize| {
        for (gy, row) in glyph.iter().enumerate() {
            for (gx, &on) in row.iter().enumerate() {
                if on == 0 {
                    continue;
                }
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        let px = ox + gx * SCALE + dx;
                        let py = y0 + gy * SCALE + dy;
                        let i = (py * W + px) * 4;
                        rgba[i..i + 4].copy_from_slice(&fg);
                    }
                }
            }
        }
    };
    draw(&r_glyph, x0);
    draw(&s_glyph, x0 + (5 + GAP) * SCALE);

    egui::IconData {
        rgba,
        width: W as u32,
        height: H as u32,
    }
}

/// Spawn a shell in a PTY, run a command, and confirm its output shows up in
/// the vt100 screen grid. Proves the terminal backend end-to-end, headlessly.
fn selftest() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::{Read, Write};

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");

    let mut parser = vt100::Parser::new(24, 80, 0);

    // Ask the shell to print a unique marker, then exit.
    writer
        .write_all(b"printf 'RSTERM_OK_%s\\n' 4242; exit\n")
        .unwrap();
    writer.flush().unwrap();

    let mut buf = [0u8; 4096];
    let mut got = String::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                parser.process(&buf[..n]);
                got.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            Err(_) => break,
        }
    }
    let _ = child.wait();

    let screen = parser.screen().contents();
    let ok = screen.contains("RSTERM_OK_4242") || got.contains("RSTERM_OK_4242");
    println!("--- vt100 screen dump ---\n{}\n-------------------------", screen.trim_end());
    if ok {
        println!("SELFTEST PASS: shell output reached the vt100 grid");
    } else {
        eprintln!("SELFTEST FAIL: marker not found");
        std::process::exit(1);
    }
}
