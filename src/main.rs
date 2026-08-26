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

/// Build the window / macOS dock icon from the bundled PNG artwork
/// (`assets/icon.png`), decoded at startup via eframe's built-in `image`
/// support. The same artwork backs the macOS `.app` bundle icon
/// (`Contents/Resources/AppIcon.icns`), so the dock icon matches whether the
/// app is running or at rest in Finder.
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("bundled assets/icon.png should be a valid PNG")
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
