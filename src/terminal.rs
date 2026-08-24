//! tmux-backed terminal: each node attaches a persistent tmux session inside a
//! PTY. Sessions live on a private tmux server (`-L rs-term`) so they survive
//! app restarts. A background poller asks tmux what the pane is currently doing
//! and exposes a short summary used as the node's title.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// Location of a usable tmux binary + our dedicated config/socket.
struct TmuxEnv {
    bin: String,
    conf: PathBuf,
    socket: String,
}

const TMUX_CONF: &str = "\
set -g status off
set -g mouse on
set -g history-limit 20000
set -g escape-time 10
set -g focus-events on
set -g default-terminal \"xterm-256color\"
setw -g aggressive-resize on
set -g bell-action any
set -g visual-bell off
setw -g monitor-bell on
";

fn config_dir() -> PathBuf {
    let base = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&base).join(".rs-term")
}

/// Detect tmux once. Returns None if no tmux is available (callers fall back
/// to a plain shell).
fn tmux_env() -> Option<&'static TmuxEnv> {
    static TMUX: OnceLock<Option<TmuxEnv>> = OnceLock::new();
    TMUX.get_or_init(|| {
        let candidates = [
            "/opt/homebrew/bin/tmux",
            "/usr/local/bin/tmux",
            "/usr/bin/tmux",
            "/opt/local/bin/tmux",
        ];
        let bin = candidates
            .iter()
            .find(|p| Path::new(p).exists())
            .map(|s| s.to_string())
            .or_else(|| {
                // Fall back to PATH lookup.
                Command::new("tmux")
                    .arg("-V")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|_| "tmux".to_string())
            })?;
        let dir = config_dir();
        let _ = std::fs::create_dir_all(&dir);
        let conf = dir.join("tmux.conf");
        let _ = std::fs::write(&conf, TMUX_CONF);
        Some(TmuxEnv {
            bin,
            conf,
            socket: "rs-term".to_string(),
        })
    })
    .as_ref()
}

/// Stable tmux session name for a node id.
pub fn session_name(id: u64) -> String {
    format!("rsterm_{id}")
}

pub struct PtyTerminal {
    pub parser: Arc<Mutex<vt100::Parser>>,
    /// Short "what is this doing" summary, updated by the poller thread.
    pub summary: Arc<Mutex<String>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    rows: u16,
    cols: u16,
    session: String,
    running: Arc<AtomicBool>,
    /// Whether this terminal is backed by a tmux session.
    tmux_backed: bool,
    /// Whether the tmux pane is currently in copy-mode (scrolled back).
    /// Kept accurate by the poller thread reading `pane_in_mode`.
    in_copy: Arc<AtomicBool>,
    /// Whether the pane's app has mouse reporting on (a TUI like Claude Code).
    /// Kept accurate by the poller thread reading `mouse_any_flag`.
    mouse_on: Arc<AtomicBool>,
    /// Set true by the reader thread when a genuine attention bell (BEL, 0x07)
    /// arrives from the pane — e.g. Claude Code finishing a turn. Consumed
    /// (swapped back to false) by the UI via `take_bell`.
    bell: Arc<AtomicBool>,
}

impl PtyTerminal {
    /// Attach (or create) the tmux session for `id`, sized `rows` x `cols`.
    ///
    /// `startup`, if set, is a command run in a login shell when the session is
    /// first created (e.g. "claude"); after it exits the shell stays open.
    pub fn spawn(
        rows: u16,
        cols: u16,
        cwd: Option<&Path>,
        ctx: egui::Context,
        id: u64,
        startup: Option<&str>,
    ) -> anyhow::Result<Self> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let session = session_name(id);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        // A login shell that runs the startup command then drops to an interactive
        // shell, so the node stays usable after the agent exits.
        let startup_args = startup.map(|cmd| {
            vec![
                "-l".to_string(),
                "-c".to_string(),
                format!("{cmd}; exec ${{SHELL:-/bin/zsh}}"),
            ]
        });

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let env = tmux_env();
        let mut cmd = match env {
            Some(t) => {
                // tmux -L rs-term -f conf new-session -A -s <name> -x C -y R [-c dir] [cmd...]
                let mut c = CommandBuilder::new(&t.bin);
                c.arg("-L");
                c.arg(&t.socket);
                c.arg("-f");
                c.arg(&t.conf);
                c.arg("new-session");
                c.arg("-A");
                c.arg("-s");
                c.arg(&session);
                c.arg("-x");
                c.arg(cols.to_string());
                c.arg("-y");
                c.arg(rows.to_string());
                if let Some(dir) = cwd {
                    c.arg("-c");
                    c.arg(dir);
                }
                // Trailing command that tmux runs on session creation.
                if let Some(args) = &startup_args {
                    c.arg(&shell);
                    for a in args {
                        c.arg(a);
                    }
                }
                c
            }
            None => {
                // No tmux: plain shell (optionally running the startup command).
                let mut c = CommandBuilder::new(&shell);
                if let Some(args) = &startup_args {
                    for a in args {
                        c.arg(a);
                    }
                }
                match cwd {
                    Some(dir) => c.cwd(dir),
                    None => {
                        if let Ok(home) = std::env::var("HOME") {
                            c.cwd(home);
                        }
                    }
                }
                c
            }
        };
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 20_000)));
        let summary = Arc::new(Mutex::new(String::new()));
        let running = Arc::new(AtomicBool::new(true));
        let in_copy = Arc::new(AtomicBool::new(false));
        let mouse_on = Arc::new(AtomicBool::new(false));
        let bell = Arc::new(AtomicBool::new(false));
        let tmux_backed = env.is_some();

        // Reader thread: shell output -> vt100 grid.
        //
        // The bytes pass through `AcsFilter` first, which translates DEC Special
        // Graphics (line-drawing) runs into real Unicode. vt100 0.15.2 ignores
        // the `ESC ( 0` / `ESC ( B` charset-designation escapes, so without this
        // tmux's box-drawing output would land in the grid as literal `q x l k`…
        let parser_thread = parser.clone();
        let ctx_reader = ctx.clone();
        let bell_reader = bell.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut acs = AcsFilter::default();
            let mut out = Vec::with_capacity(8192);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        out.clear();
                        acs.process(&buf[..n], &mut out);
                        if acs.bell {
                            bell_reader.store(true, Ordering::Relaxed);
                        }
                        if let Ok(mut p) = parser_thread.lock() {
                            p.process(&out);
                        }
                        ctx_reader.request_repaint();
                    }
                    Err(_) => break,
                }
            }
            ctx_reader.request_repaint();
        });

        // Poller thread: ask tmux what the pane is doing -> title summary.
        if let Some(t) = env {
            let bin = t.bin.clone();
            let sock = t.socket.clone();
            let sname = session.clone();
            let sum = summary.clone();
            let run = running.clone();
            let in_copy_poll = in_copy.clone();
            let mouse_on_poll = mouse_on.clone();
            let ctx_poll = ctx.clone();
            thread::spawn(move || {
                // Belt-and-suspenders: status bar off and mouse routing on (the
                // latter lets wheel events reach mouse-aware apps like Claude Code).
                for _ in 0..20 {
                    if !run.load(Ordering::Relaxed) {
                        return;
                    }
                    let up = Command::new(&bin)
                        .args(["-L", &sock, "has-session", "-t", &sname])
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                    if up {
                        let _ = Command::new(&bin)
                            .args(["-L", &sock, "set-option", "-t", &sname, "status", "off"])
                            .output();
                        let _ = Command::new(&bin)
                            .args(["-L", &sock, "set-option", "-g", "mouse", "on"])
                            .output();
                        // Forward pane bells to us as a literal BEL (rather than
                        // tmux drawing its own visual bell), so the reader thread
                        // can detect "Claude finished" and glow the node.
                        let _ = Command::new(&bin)
                            .args(["-L", &sock, "set-option", "-g", "bell-action", "any"])
                            .output();
                        let _ = Command::new(&bin)
                            .args(["-L", &sock, "set-option", "-g", "visual-bell", "off"])
                            .output();
                        break;
                    }
                    thread::sleep(Duration::from_millis(150));
                }

                while run.load(Ordering::Relaxed) {
                    if let Ok(o) = Command::new(&bin)
                        .args([
                            "-L",
                            &sock,
                            "display-message",
                            "-p",
                            "-t",
                            &sname,
                            "#{pane_current_command}\t#{pane_current_path}\t#{pane_in_mode}\t#{mouse_any_flag}",
                        ])
                        .output()
                    {
                        if o.status.success() {
                            let raw = String::from_utf8_lossy(&o.stdout);
                            let line = raw.trim_end_matches('\n');
                            let mut it = line.split('\t');
                            let cmd = it.next().unwrap_or("");
                            let path = it.next().unwrap_or("");
                            let mode = it.next().unwrap_or("0").trim();
                            let mouse = it.next().unwrap_or("0").trim();

                            in_copy_poll.store(mode == "1", Ordering::Relaxed);
                            mouse_on_poll.store(mouse == "1", Ordering::Relaxed);

                            let title = summarize(cmd, path);
                            if let Ok(mut g) = sum.lock() {
                                if *g != title {
                                    *g = title;
                                    ctx_poll.request_repaint();
                                }
                            }
                        }
                    }
                    // Sleep ~1.5s, but wake promptly if we're shutting down.
                    for _ in 0..10 {
                        if !run.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(150));
                    }
                }
            });
        }

        Ok(Self {
            parser,
            summary,
            writer,
            master: pair.master,
            child,
            rows,
            cols,
            session,
            running,
            tmux_backed,
            in_copy,
            mouse_on,
            bell,
        })
    }

    /// Consume a pending attention bell, returning whether one had rung since
    /// the last call. Used by the UI to start the node's glow + play a sound.
    pub fn take_bell(&self) -> bool {
        self.bell.swap(false, Ordering::Relaxed)
    }

    pub fn send(&mut self, bytes: &[u8]) {
        // Typing returns to the live bottom of the buffer.
        if self.tmux_backed {
            // If we're scrolled back (copy-mode), cancel it so keystrokes reach
            // the shell rather than the copy-mode command table.
            if self.in_copy.load(Ordering::Relaxed) {
                if let Some(t) = tmux_env() {
                    let _ = Command::new(&t.bin)
                        .args(["-L", &t.socket, "send-keys", "-t", &self.session, "-X", "cancel"])
                        .output();
                }
                self.in_copy.store(false, Ordering::Relaxed);
            }
        } else if let Ok(mut p) = self.parser.lock() {
            p.set_scrollback(0);
        }
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Scroll the view. Positive `delta` moves toward older output; negative
    /// moves back toward the live bottom.
    ///
    /// For tmux terminals this drives tmux copy-mode (tmux keeps the history,
    /// not our vt100 parser). For plain shells it uses the vt100 scrollback.
    pub fn scroll_lines(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        // Full-screen apps that request mouse (e.g. Claude Code) manage their own
        // scrolling; tmux copy-mode has no history for the alternate screen. Send
        // wheel events straight to the app instead (tmux `mouse on` routes them).
        if self.tmux_backed && self.mouse_on.load(Ordering::Relaxed) {
            let cx = (self.cols / 2).max(1);
            let cy = (self.rows / 2).max(1);
            let btn = if delta > 0 { 64 } else { 65 }; // SGR wheel up / down
            let one = format!("\x1b[<{btn};{cx};{cy}M");
            let mut seq = String::new();
            for _ in 0..delta.unsigned_abs().min(8) {
                seq.push_str(&one);
            }
            let _ = self.writer.write_all(seq.as_bytes());
            let _ = self.writer.flush();
            return;
        }
        if self.tmux_backed {
            let Some(t) = tmux_env() else { return };
            let count = delta.unsigned_abs().to_string();
            if delta > 0 {
                // Enter copy-mode (no-op preserving position if already in it),
                // then scroll up. `-e` auto-exits when scrolled back to bottom.
                let _ = Command::new(&t.bin)
                    .args(["-L", &t.socket, "copy-mode", "-e", "-t", &self.session])
                    .output();
                let _ = Command::new(&t.bin)
                    .args([
                        "-L", &t.socket, "send-keys", "-t", &self.session, "-X", "-N", &count,
                        "scroll-up",
                    ])
                    .output();
                self.in_copy.store(true, Ordering::Relaxed);
            } else if self.in_copy.load(Ordering::Relaxed) {
                let _ = Command::new(&t.bin)
                    .args([
                        "-L", &t.socket, "send-keys", "-t", &self.session, "-X", "-N", &count,
                        "scroll-down",
                    ])
                    .output();
            }
            return;
        }
        if let Ok(mut p) = self.parser.lock() {
            let cur = p.screen().scrollback() as isize;
            let target = (cur + delta).max(0) as usize;
            p.set_scrollback(target);
        }
    }

    /// Whether the view is currently scrolled back (viewing history).
    pub fn is_scrolled(&self) -> bool {
        if self.tmux_backed {
            self.in_copy.load(Ordering::Relaxed)
        } else {
            self.parser
                .lock()
                .map(|p| p.screen().scrollback() > 0)
                .unwrap_or(false)
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.lock() {
            p.set_size(rows, cols);
        }
    }

    pub fn is_dead(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Permanently destroy the underlying tmux session (used on explicit close).
    pub fn kill_session(&self) {
        if let Some(t) = tmux_env() {
            let _ = Command::new(&t.bin)
                .args(["-L", &t.socket, "kill-session", "-t", &self.session])
                .output();
        }
    }
}

impl Drop for PtyTerminal {
    fn drop(&mut self) {
        // Stop the poller and detach the client, but leave the tmux session
        // alive so it survives app restarts. Sessions are only destroyed via
        // an explicit kill_session() on node/canvas close.
        self.running.store(false, Ordering::Relaxed);
        let _ = self.child.kill();
    }
}

/// Turn a pane's current command + path into a short "what it's doing" title.
fn summarize(cmd: &str, path: &str) -> String {
    let cmd = cmd.trim();
    let path = path.trim();

    let base = path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(path);
    let base = if base.is_empty() { "~" } else { base };

    let shells = [
        "zsh", "bash", "sh", "fish", "-zsh", "-bash", "-fish", "login", "tmux",
    ];
    if cmd.is_empty() {
        base.to_string()
    } else if shells.contains(&cmd) {
        base.to_string()
    } else {
        format!("{cmd} · {base}")
    }
}

/// A small VT-aware byte filter that translates DEC Special Graphics
/// (line-drawing) runs into Unicode before they reach the vt100 parser.
///
/// vt100 0.15.2 does not implement the G0/G1 charset-designation escapes
/// (`ESC ( 0` selects line-drawing, `ESC ( B` restores ASCII), so line bytes
/// like `q` `x` `l` `k` would otherwise be stored verbatim and shown as literal
/// letters. tmux emits exactly these ACS runs when it repaints box-drawing for
/// a client whose terminfo advertises `smacs`/`rmacs` (xterm-256color does).
///
/// The filter is a minimal state machine so it can pass ordinary escape/CSI/OSC
/// sequences through untouched, only rewriting printable bytes while a
/// line-drawing charset is active. State is retained across reads, so sequences
/// split over buffer boundaries are handled.
#[derive(Default)]
struct AcsFilter {
    state: AcsState,
    /// Whether G0 / G1 currently designate the line-drawing charset.
    g0_line: bool,
    g1_line: bool,
    /// Active charset: false = G0 (SI / 0x0f), true = G1 (SO / 0x0e).
    shift_out: bool,
    /// Bytes of an in-progress escape sequence, buffered until we know whether
    /// to forward them (ordinary escape) or drop them (charset designation).
    pending: Vec<u8>,
    /// Set when a genuine attention bell (a ground-state BEL) was seen during
    /// the current `process` call. Reset at the start of each call. BELs that
    /// merely terminate an OSC string (window-title updates) are handled in the
    /// `Osc` state and never set this.
    bell: bool,
}

#[derive(Clone, Copy)]
enum AcsState {
    Ground,
    Esc,     // saw ESC (0x1b)
    Scs(u8), // saw ESC + one of ( ) * + ; the intermediate is stored
    Csi,     // saw ESC [ … consuming until a final byte
    Osc,     // saw ESC ] … consuming until BEL or ST
    OscEsc,  // inside OSC, saw ESC (expecting the \ of ST)
}

impl Default for AcsState {
    fn default() -> Self {
        AcsState::Ground
    }
}

impl AcsFilter {
    fn process(&mut self, input: &[u8], out: &mut Vec<u8>) {
        self.bell = false;
        for &b in input {
            match self.state {
                AcsState::Ground => self.ground(b, out),
                AcsState::Esc => self.esc(b, out),
                AcsState::Scs(which) => self.scs(which, b),
                AcsState::Csi => self.csi(b, out),
                AcsState::Osc => self.osc(b, out),
                AcsState::OscEsc => self.osc_esc(b, out),
            }
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<u8>) {
        match b {
            0x1b => {
                // Start buffering a possible charset designation.
                self.pending.clear();
                self.pending.push(b);
                self.state = AcsState::Esc;
            }
            0x0e => self.shift_out = true,  // SO -> G1
            0x0f => self.shift_out = false, // SI -> G0
            0x07 => self.bell = true,       // BEL — attention bell, consumed here
            _ => {
                let line = if self.shift_out { self.g1_line } else { self.g0_line };
                if line && (0x5f..=0x7e).contains(&b) {
                    push_acs(b, out);
                } else {
                    out.push(b);
                }
            }
        }
    }

    fn esc(&mut self, b: u8, out: &mut Vec<u8>) {
        match b {
            b'(' | b')' | b'*' | b'+' => {
                // Charset designation — keep buffering, we'll drop the whole run.
                self.pending.push(b);
                self.state = AcsState::Scs(b);
            }
            b'[' => {
                self.flush_pending(out);
                out.push(b);
                self.state = AcsState::Csi;
            }
            b']' => {
                self.flush_pending(out);
                out.push(b);
                self.state = AcsState::Osc;
            }
            _ => {
                // Any other ESC x (ESC 7, ESC M, ESC =, …) — forward verbatim.
                self.flush_pending(out);
                out.push(b);
                self.state = AcsState::Ground;
            }
        }
    }

    fn scs(&mut self, which: u8, b: u8) {
        // Intermediate bytes (e.g. `%` in a multi-byte designator) — keep going.
        if (0x20..=0x2f).contains(&b) {
            return;
        }
        // `0` selects DEC Special Graphics (line-drawing); anything else (B, A, …)
        // restores a text charset.
        let line = b == b'0';
        match which {
            b'(' => self.g0_line = line,
            b')' => self.g1_line = line,
            _ => {} // G2/G3 (`*`/`+`) are only reachable via SS2/SS3; ignore.
        }
        // Drop the whole `ESC ( X` run — vt100 ignores it anyway.
        self.pending.clear();
        self.state = AcsState::Ground;
    }

    fn csi(&mut self, b: u8, out: &mut Vec<u8>) {
        out.push(b);
        if (0x40..=0x7e).contains(&b) {
            self.state = AcsState::Ground;
        }
    }

    fn osc(&mut self, b: u8, out: &mut Vec<u8>) {
        out.push(b);
        match b {
            0x07 => self.state = AcsState::Ground, // BEL terminator
            0x1b => self.state = AcsState::OscEsc, // maybe ST (ESC \)
            _ => {}
        }
    }

    fn osc_esc(&mut self, b: u8, out: &mut Vec<u8>) {
        out.push(b);
        self.state = if b == b'\\' {
            AcsState::Ground
        } else {
            AcsState::Osc
        };
    }

    fn flush_pending(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pending);
        self.pending.clear();
    }
}

/// Map a DEC Special Graphics byte (0x5f..=0x7e) to its Unicode glyph and append
/// the UTF-8 to `out`. Bytes outside the table are emitted unchanged.
fn push_acs(b: u8, out: &mut Vec<u8>) {
    let s: &str = match b {
        0x5f => " ",
        // 0x60..=0x7e below
        0x60 => "◆",
        0x61 => "▒",
        0x62 => "␉",
        0x63 => "␌",
        0x64 => "␍",
        0x65 => "␊",
        0x66 => "°",
        0x67 => "±",
        0x68 => "␤",
        0x69 => "␋",
        0x6a => "┘",
        0x6b => "┐",
        0x6c => "┌",
        0x6d => "└",
        0x6e => "┼",
        0x6f => "⎺",
        0x70 => "⎻",
        0x71 => "─",
        0x72 => "⎼",
        0x73 => "⎽",
        0x74 => "├",
        0x75 => "┤",
        0x76 => "┴",
        0x77 => "┬",
        0x78 => "│",
        0x79 => "≤",
        0x7a => "≥",
        0x7b => "π",
        0x7c => "≠",
        0x7d => "£",
        0x7e => "·",
        _ => {
            out.push(b);
            return;
        }
    };
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> String {
        let mut f = AcsFilter::default();
        let mut out = Vec::new();
        for c in chunks {
            f.process(c, &mut out);
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn translates_line_drawing_run() {
        // ESC ( 0  qqq  ESC ( B  -> three horizontal lines.
        assert_eq!(run(&[b"\x1b(0qqq\x1b(B"]), "───");
    }

    #[test]
    fn corners_and_verticals() {
        // lqk / x x / mqj  -> a tiny box's characters.
        assert_eq!(run(&[b"\x1b(0lqkxxmqj\x1b(B"]), "┌─┐││└─┘");
    }

    #[test]
    fn plain_text_untouched() {
        assert_eq!(run(&[b"hello qqq world"]), "hello qqq world");
    }

    #[test]
    fn csi_passes_through() {
        // A colour SGR must survive verbatim, and `q` outside a run stays `q`.
        assert_eq!(run(&[b"\x1b[1;31mq\x1b[0m"]), "\x1b[1;31mq\x1b[0m");
    }

    #[test]
    fn charset_switch_split_across_reads() {
        // Sequence and run fragmented over several buffers.
        assert_eq!(run(&[b"\x1b", b"(0q", b"q", b"q\x1b(", b"B"]), "───");
    }

    #[test]
    fn restore_stops_translation() {
        // After ESC ( B, `q` is a literal `q` again.
        assert_eq!(run(&[b"\x1b(0q\x1b(Bq"]), "─q");
    }

    /// Return whether processing `chunks` reported an attention bell.
    fn bell(chunks: &[&[u8]]) -> bool {
        let mut f = AcsFilter::default();
        let mut out = Vec::new();
        let mut rang = false;
        for c in chunks {
            f.process(c, &mut out);
            rang |= f.bell;
        }
        rang
    }

    #[test]
    fn ground_bel_is_an_attention_bell() {
        assert!(bell(&[b"done\x07"]));
    }

    #[test]
    fn osc_title_terminator_is_not_a_bell() {
        // Shells set the window title with `ESC ] 0 ; title BEL` on every prompt.
        // That terminating BEL must NOT be treated as an attention bell.
        assert!(!bell(&[b"\x1b]0;my title\x07"]));
        // …and it still isn't when split across reads.
        assert!(!bell(&[b"\x1b]2;dir", b" name\x07"]));
    }

    #[test]
    fn bell_flag_resets_each_process_call() {
        let mut f = AcsFilter::default();
        let mut out = Vec::new();
        f.process(b"\x07", &mut out);
        assert!(f.bell);
        f.process(b"plain", &mut out);
        assert!(!f.bell);
    }
}
