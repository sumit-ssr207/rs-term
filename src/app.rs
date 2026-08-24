//! The application: tabbed infinite canvases of terminal/note nodes.

use eframe::egui;
use egui::{Align2, Color32, FontId, Id, Pos2, Rect, Sense, Stroke, Vec2};

use crate::node::{Node, NodeKind, NoteNode, TerminalNode};
use crate::terminal::PtyTerminal;

// --- visual constants (in world units at zoom 1.0) ---
const BASE_FONT: f32 = 13.0;
const TITLE_H: f32 = 26.0;
const RESIZE_HANDLE: f32 = 16.0;
const MIN_W: f32 = 200.0;
const MIN_H: f32 = 140.0;
const GRID: f32 = 40.0;
const ZOOM_MIN: f32 = 0.2;
const ZOOM_MAX: f32 = 3.0;

const CANVAS_BG: Color32 = Color32::from_rgb(0x14, 0x16, 0x1a);
const NODE_BG: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x1e);
const TERM_BG: Color32 = Color32::from_rgb(0x1a, 0x1b, 0x1f);
const TERM_FG: Color32 = Color32::from_rgb(0xd4, 0xd4, 0xd4);
const ACCENT: Color32 = Color32::from_rgb(0x4c, 0x8b, 0xf5);
const ORANGE: Color32 = Color32::from_rgb(0xd0, 0x86, 0x4a);

/// A single infinite canvas (one tab).
struct Canvas {
    name: String,
    /// Working directory this canvas is bound to; terminals open here.
    workspace: Option<std::path::PathBuf>,
    nodes: Vec<Node>,
    offset: Vec2,
    zoom: f32,
    focused: Option<u64>,
    moving: Option<u64>,
    resizing: Option<u64>,
    menu_world: Pos2,
}

impl Canvas {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            workspace: None,
            nodes: Vec::new(),
            offset: Vec2::new(80.0, 80.0),
            zoom: 1.0,
            focused: None,
            moving: None,
            resizing: None,
            menu_world: Pos2::ZERO,
        }
    }
}

pub struct RsTermApp {
    canvases: Vec<Canvas>,
    active: usize,
    next_id: u64,
    cell: Option<Vec2>,
    viewport_center: Pos2,
    /// Fractional accumulator for smooth terminal scrollback scrolling.
    scroll_accum: f32,
    /// Canvas index awaiting a "close this canvas?" confirmation, if any.
    confirm_close: Option<usize>,
    /// Layout has unsaved changes; flushed by the throttled autosave.
    dirty: bool,
    /// When the layout was last written to disk.
    last_save: std::time::Instant,
}

impl RsTermApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        let mut app = Self {
            canvases: Vec::new(),
            active: 0,
            next_id: 1,
            cell: None,
            viewport_center: Pos2::new(640.0, 400.0),
            scroll_accum: 0.0,
            confirm_close: None,
            dirty: false,
            last_save: std::time::Instant::now(),
        };
        app.load_layout();
        if app.canvases.is_empty() {
            // Start with an empty, unbound canvas showing the "Open Folder" prompt.
            app.canvases.push(Canvas::new("untitled"));
            app.active = 0;
        }
        app
    }

    // --- persistence ---

    fn layout_path() -> std::path::PathBuf {
        let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        std::path::Path::new(&base)
            .join(".rs-term")
            .join("layout.json")
    }

    fn save_layout(&self) {
        let canvases = self
            .canvases
            .iter()
            .map(|c| crate::node::SavedCanvas {
                name: c.name.clone(),
                workspace: c.workspace.as_ref().map(|p| p.to_string_lossy().into_owned()),
                offset: [c.offset.x, c.offset.y],
                zoom: c.zoom,
                nodes: c.nodes.iter().map(|n| n.to_saved()).collect(),
            })
            .collect();
        let saved = crate::node::SavedApp {
            active: self.active,
            next_id: self.next_id,
            canvases,
        };
        if let Ok(json) = serde_json::to_string_pretty(&saved) {
            let path = Self::layout_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, json);
        }
    }

    /// Mark the layout as needing a save; the actual write is throttled.
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Write the layout now and reset the autosave throttle.
    fn flush_layout(&mut self) {
        self.save_layout();
        self.dirty = false;
        self.last_save = std::time::Instant::now();
    }

    fn load_layout(&mut self) {
        let Ok(data) = std::fs::read_to_string(Self::layout_path()) else {
            return;
        };
        let Ok(saved) = serde_json::from_str::<crate::node::SavedApp>(&data) else {
            return;
        };
        self.next_id = saved.next_id.max(1);
        self.canvases = saved
            .canvases
            .into_iter()
            .map(|c| Canvas {
                name: c.name,
                workspace: c.workspace.map(std::path::PathBuf::from),
                offset: Vec2::new(c.offset[0], c.offset[1]),
                zoom: c.zoom.clamp(ZOOM_MIN, ZOOM_MAX),
                nodes: c.nodes.into_iter().map(Node::from_saved).collect(),
                focused: None,
                moving: None,
                resizing: None,
                menu_world: Pos2::ZERO,
            })
            .collect();
        self.active = saved.active.min(self.canvases.len().saturating_sub(1));
    }

    // --- coordinate transforms (active canvas) ---

    fn cv(&self) -> &Canvas {
        &self.canvases[self.active]
    }

    fn w2s(&self, p: Pos2) -> Pos2 {
        let c = self.cv();
        (p.to_vec2() * c.zoom + c.offset).to_pos2()
    }

    fn s2w(&self, p: Pos2) -> Pos2 {
        let c = self.cv();
        ((p.to_vec2() - c.offset) / c.zoom).to_pos2()
    }

    fn node_rect(&self, node: &Node) -> Rect {
        Rect::from_min_size(self.w2s(node.pos), node.size * self.cv().zoom)
    }

    // --- zoom helpers (around the viewport center) ---

    fn zoom_around(&mut self, anchor: Pos2, new_zoom: f32) {
        let c = &mut self.canvases[self.active];
        let nz = new_zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        let world = (anchor.to_vec2() - c.offset) / c.zoom;
        c.zoom = nz;
        c.offset = anchor.to_vec2() - world * nz;
    }

    fn zoom_by(&mut self, factor: f32) {
        let z = self.cv().zoom;
        let center = self.viewport_center;
        self.zoom_around(center, z * factor);
    }

    fn set_zoom(&mut self, z: f32) {
        let center = self.viewport_center;
        self.zoom_around(center, z);
    }

    /// Frame all nodes of the active canvas into the viewport.
    fn fit(&mut self, view: Rect) {
        let c = &self.canvases[self.active];
        if c.nodes.is_empty() {
            return;
        }
        let mut min = Pos2::new(f32::MAX, f32::MAX);
        let mut max = Pos2::new(f32::MIN, f32::MIN);
        for n in &c.nodes {
            min.x = min.x.min(n.pos.x);
            min.y = min.y.min(n.pos.y);
            max.x = max.x.max(n.pos.x + n.size.x);
            max.y = max.y.max(n.pos.y + n.size.y);
        }
        let pad = 60.0;
        let w = (max.x - min.x + pad * 2.0).max(1.0);
        let h = (max.y - min.y + pad * 2.0).max(1.0);
        let zoom = (view.width() / w)
            .min(view.height() / h)
            .clamp(ZOOM_MIN, ZOOM_MAX);
        let center_world = Pos2::new((min.x + max.x) / 2.0, (min.y + max.y) / 2.0);
        let c = &mut self.canvases[self.active];
        c.zoom = zoom;
        c.offset = view.center().to_vec2() - center_world.to_vec2() * zoom;
    }

    // --- node creation ---

    fn default_cwd() -> Option<std::path::PathBuf> {
        std::env::current_dir().ok()
    }

    fn add_terminal(&mut self, world_pos: Pos2) {
        let id = self.next_id;
        self.next_id += 1;
        // Open in the canvas's workspace folder if it has one.
        let cwd = self.canvases[self.active]
            .workspace
            .clone()
            .or_else(Self::default_cwd);
        let c = &mut self.canvases[self.active];
        c.nodes.push(Node {
            id,
            title: format!("terminal {id}"),
            pos: world_pos,
            size: Vec2::new(560.0, 360.0),
            kind: NodeKind::Terminal(TerminalNode {
                cwd,
                agent: false,
                term: None,
            }),
        });
        c.focused = Some(id);
        self.mark_dirty();
    }

    fn add_claude(&mut self, world_pos: Pos2) {
        let id = self.next_id;
        self.next_id += 1;
        let cwd = self.canvases[self.active]
            .workspace
            .clone()
            .or_else(Self::default_cwd);
        let c = &mut self.canvases[self.active];
        c.nodes.push(Node {
            id,
            title: format!("claude {id}"),
            pos: world_pos,
            size: Vec2::new(620.0, 420.0),
            kind: NodeKind::Terminal(TerminalNode {
                cwd,
                agent: true,
                term: None,
            }),
        });
        c.focused = Some(id);
        self.mark_dirty();
    }

    fn add_note(&mut self, world_pos: Pos2) {
        let id = self.next_id;
        self.next_id += 1;
        let c = &mut self.canvases[self.active];
        c.nodes.push(Node {
            id,
            title: format!("note {id}"),
            pos: world_pos,
            size: Vec2::new(280.0, 200.0),
            kind: NodeKind::Note(NoteNode {
                text: String::new(),
            }),
        });
        c.focused = Some(id);
        self.mark_dirty();
    }

    fn add_canvas(&mut self) {
        // New canvases start empty and unbound; the "Open Folder" prompt lets the
        // user bind a workspace, which names the tab and opens a terminal there.
        self.canvases.push(Canvas::new("untitled"));
        self.active = self.canvases.len() - 1;
    }

    /// Prompt for a folder (native macOS dialog) and bind it to the active
    /// canvas: name the tab after it and open a terminal there.
    fn open_workspace(&mut self) {
        let Some(path) = pick_folder() else {
            return;
        };
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let a = self.active;
        self.canvases[a].workspace = Some(path);
        self.canvases[a].name = name;
        if self.canvases[a].nodes.is_empty() {
            self.add_terminal(Pos2::new(60.0, 40.0));
        }
        self.mark_dirty();
    }

    fn close_canvas(&mut self, idx: usize) {
        if self.canvases.len() <= 1 || idx >= self.canvases.len() {
            return;
        }
        // Destroy the tmux sessions belonging to this canvas.
        for node in &self.canvases[idx].nodes {
            if let NodeKind::Terminal(t) = &node.kind {
                if let Some(term) = &t.term {
                    term.kill_session();
                }
            }
        }
        self.canvases.remove(idx);
        if self.active >= self.canvases.len() {
            self.active = self.canvases.len() - 1;
        }
    }

    fn close_node(&mut self, id: u64) {
        let c = &mut self.canvases[self.active];
        if let Some(pos) = c.nodes.iter().position(|n| n.id == id) {
            if let NodeKind::Terminal(t) = &c.nodes[pos].kind {
                if let Some(term) = &t.term {
                    term.kill_session();
                }
            }
            c.nodes.remove(pos);
        }
        if c.focused == Some(id) {
            c.focused = None;
        }
    }

    /// Index of the top-most terminal node whose rect contains `pos`.
    fn terminal_index_at(&self, pos: Pos2) -> Option<usize> {
        let c = &self.canvases[self.active];
        let mut found = None;
        for (i, n) in c.nodes.iter().enumerate() {
            if matches!(n.kind, NodeKind::Terminal(_)) && self.node_rect(n).contains(pos) {
                found = Some(i);
            }
        }
        found
    }

    fn scroll_terminal(&mut self, i: usize, lines: isize) {
        let a = self.active;
        if let NodeKind::Terminal(t) = &mut self.canvases[a].nodes[i].kind {
            if let Some(term) = &mut t.term {
                term.scroll_lines(lines);
            }
        }
    }

    fn bring_to_front(&mut self, id: u64) {
        let c = &mut self.canvases[self.active];
        if let Some(pos) = c.nodes.iter().position(|n| n.id == id) {
            let node = c.nodes.remove(pos);
            c.nodes.push(node);
        }
    }

    /// The live title for a node ("what it's doing").
    fn display_title(&self, node: &Node) -> String {
        match &node.kind {
            NodeKind::Terminal(t) => t
                .term
                .as_ref()
                .and_then(|tm| tm.summary.lock().ok().map(|s| s.clone()))
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| format!("terminal {}", node.id)),
            NodeKind::Note(n) => {
                let line = n
                    .text
                    .lines()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                if line.is_empty() {
                    "note".to_string()
                } else if line.chars().count() > 30 {
                    let s: String = line.chars().take(30).collect();
                    format!("{s}…")
                } else {
                    line.to_string()
                }
            }
        }
    }
}

impl eframe::App for RsTermApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.cell.is_none() {
            let font = FontId::monospace(BASE_FONT);
            let (w, h) = ctx.fonts(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
            self.cell = Some(Vec2::new(w.max(1.0), h.max(1.0)));
        }

        self.top_bar(ctx);
        self.bottom_bar(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(CANVAS_BG))
            .show(ctx, |ui| {
                self.canvas(ui, ctx);
            });

        self.confirm_close_dialog(ctx);

        // Always flush immediately when the window is closing.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.dirty {
                self.flush_layout();
            }
        } else if self.dirty && self.last_save.elapsed().as_secs() >= 10 {
            // Throttled autosave: at most once every 10s while there are changes.
            self.flush_layout();
        }

        // Keep node titles and terminal cursors live even while idle/unfocused,
        // so the tmux-polled summaries refresh promptly.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

impl RsTermApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            // Tab row. Tab names are derived from each canvas's workspace folder
            // (or "untitled" until one is opened); there is no manual renaming.
            let mut switch: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            let mut add = false;
            ui.horizontal(|ui| {
                let n = self.canvases.len();
                for idx in 0..n {
                    let selected = idx == self.active;
                    let name = self.canvases[idx].name.clone();
                    // Workspaced tabs show the folder name plainly; untitled tabs
                    // get a placeholder glyph.
                    let label = if self.canvases[idx].workspace.is_some() {
                        name
                    } else {
                        format!("▦ {name}")
                    };
                    if ui.selectable_label(selected, label).clicked() {
                        switch = Some(idx);
                    }
                    if n > 1 {
                        if ui
                            .small_button("×")
                            .on_hover_text("Close this canvas (kills its sessions)")
                            .clicked()
                        {
                            close_idx = Some(idx);
                        }
                    }
                    ui.separator();
                }
                if ui.button("+").on_hover_text("New canvas").clicked() {
                    add = true;
                }
            });
            if let Some(idx) = switch {
                self.active = idx;
            }
            if let Some(idx) = close_idx {
                // Don't close straight away — ask for confirmation first.
                self.confirm_close = Some(idx);
            }
            if add {
                self.add_canvas();
                self.mark_dirty();
            }
        });
    }

    /// Modal "Are you sure?" shown before a canvas (and its tmux sessions) is
    /// destroyed. Confirm closes it; Cancel / Esc backs out.
    fn confirm_close_dialog(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.confirm_close else {
            return;
        };
        // The tab list may have shifted since the request; bail if stale.
        if idx >= self.canvases.len() {
            self.confirm_close = None;
            return;
        }
        let name = self.canvases[idx].name.clone();

        // Dim + input-block everything behind the dialog.
        egui::Area::new(Id::new("confirm_close_overlay"))
            .order(egui::Order::Middle)
            .fixed_pos(Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ctx.screen_rect();
                ui.painter()
                    .rect_filled(screen, 0.0, Color32::from_black_alpha(140));
                ui.allocate_rect(screen, Sense::click());
            });

        let mut confirmed = false;
        let mut cancelled = false;

        egui::Window::new("Close canvas")
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(300.0);
                ui.add_space(4.0);
                ui.label(format!(
                    "Close “{name}” and kill its terminal sessions?"
                ));
                ui.add_space(6.0);
                ui.strong("Are you sure?");
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let close = ui.add(
                            egui::Button::new(
                                egui::RichText::new("Close canvas").color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgb(0xc0, 0x3a, 0x3a)),
                        );
                        if close.clicked() {
                            confirmed = true;
                        }
                    });
                });
            });

        // Keyboard: Enter confirms, Esc cancels.
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            confirmed = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancelled = true;
        }

        if confirmed {
            self.confirm_close = None;
            self.close_canvas(idx);
            self.mark_dirty();
        } else if cancelled {
            self.confirm_close = None;
        }
    }

    fn bottom_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("zoombar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button(" - ").on_hover_text("Zoom out").clicked() {
                    self.zoom_by(1.0 / 1.2);
                }
                let z = self.cv().zoom;
                ui.label(
                    egui::RichText::new(format!("{:>4.0}%", z * 100.0))
                        .monospace(),
                );
                if ui.button(" + ").on_hover_text("Zoom in").clicked() {
                    self.zoom_by(1.2);
                }
                if ui.button("100%").clicked() {
                    self.set_zoom(1.0);
                }
                if ui.button("Fit").on_hover_text("Fit all nodes").clicked() {
                    let view = Rect::from_center_size(self.viewport_center, Vec2::new(1000.0, 600.0));
                    self.fit(view);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(
                            "scroll: pan   ·   ⌥+scroll: zoom   ·   ⌥+scroll on selected terminal: scroll text",
                        )
                        .weak()
                        .small(),
                    );
                });
            });
        });
    }

    fn canvas(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let panel = ui.max_rect();
        self.viewport_center = panel.center();
        let cell = self.cell.unwrap_or(Vec2::new(8.0, 16.0));
        let a = self.active;

        let pointer = ui.input(|i| i.pointer.hover_pos());

        // --- app-level keyboard shortcuts ---
        let (new_term, new_note, do_save, close_focused) = ui.input(|i| {
            let c = i.modifiers.command;
            (
                c && i.key_pressed(egui::Key::T),
                c && i.key_pressed(egui::Key::N),
                c && i.key_pressed(egui::Key::S),
                c && i.key_pressed(egui::Key::W),
            )
        });
        if close_focused {
            if let Some(id) = self.canvases[a].focused {
                self.close_node(id);
                self.mark_dirty();
            }
        }
        if new_term {
            let p = self.s2w(self.viewport_center);
            self.add_terminal(p);
        }
        if new_note {
            let p = self.s2w(self.viewport_center);
            self.add_note(p);
        }
        if do_save {
            // ⌘S saves immediately.
            self.flush_layout();
        }

        // --- scroll behavior ---
        //   plain scroll            -> pan the canvas
        //   Option + scroll         -> zoom the canvas
        //   Option + scroll, with a terminal selected -> scroll that terminal
        //   trackpad pinch          -> always zoom
        if let Some(ptr) = pointer {
            if panel.contains(ptr) {
                let (scroll, pinch, alt) =
                    ui.input(|i| (i.raw_scroll_delta, i.zoom_delta(), i.modifiers.alt));

                // Pinch gesture always zooms.
                if (pinch - 1.0).abs() > f32::EPSILON {
                    let z = self.canvases[a].zoom;
                    self.zoom_around(ptr, z * pinch);
                }

                if scroll != Vec2::ZERO {
                    if alt {
                        if let Some(ti) = self.terminal_index_at(ptr) {
                            // Scroll the hovered terminal's history (no click
                            // needed). Accumulate fractional lines so slow
                            // trackpad scrolls still register.
                            self.scroll_accum += scroll.y / 12.0;
                            let lines = self.scroll_accum.trunc() as isize;
                            self.scroll_accum -= lines as f32;
                            if lines != 0 {
                                self.scroll_terminal(ti, lines);
                            }
                        } else {
                            // Option + scroll zooms the canvas.
                            let factor = (scroll.y * 0.0015).exp();
                            let z = self.canvases[a].zoom;
                            self.zoom_around(ptr, z * factor);
                        }
                    } else {
                        // Plain scroll pans the canvas.
                        self.canvases[a].offset += scroll;
                    }
                }
            }
        }

        let painter = ui.painter_at(panel);
        self.draw_grid(&painter, panel);

        // Top-most node under the pointer. While a mouse button is held, hit-test
        // against the press origin instead of the live pointer, so the node you
        // grabbed stays the drag target even once the pointer moves off it (e.g.
        // dragging the title bar upward past the top edge).
        let (button_down, press_origin) =
            ui.input(|i| (i.pointer.primary_down(), i.pointer.press_origin()));
        let hit_pos = if button_down {
            press_origin.or(pointer)
        } else {
            pointer
        };
        let mut topmost_hover: Option<u64> = None;
        if let Some(hp) = hit_pos {
            for node in &self.canvases[a].nodes {
                if self.node_rect(node).contains(hp) {
                    topmost_hover = Some(node.id);
                }
            }
        }
        let active_node = self.canvases[a]
            .moving
            .or(self.canvases[a].resizing)
            .or(topmost_hover);

        let zoom = self.canvases[a].zoom;
        let offset = self.canvases[a].offset;
        let w2s = |p: Pos2| (p.to_vec2() * zoom + offset).to_pos2();

        let mut bring_to_front: Option<u64> = None;
        let mut to_close: Option<u64> = None;

        let count = self.canvases[a].nodes.len();
        for i in 0..count {
            let (id, pos, size, is_terminal, is_agent) = {
                let n = &self.canvases[a].nodes[i];
                let is_agent = matches!(&n.kind, NodeKind::Terminal(t) if t.agent);
                (
                    n.id,
                    n.pos,
                    n.size,
                    matches!(n.kind, NodeKind::Terminal(_)),
                    is_agent,
                )
            };
            let is_active = active_node == Some(id);
            let is_focused = self.canvases[a].focused == Some(id);

            // Keep the stored title in sync with the live summary.
            let title = self.display_title(&self.canvases[a].nodes[i]);
            self.canvases[a].nodes[i].title = title.clone();

            let tl = w2s(pos);
            let rect = Rect::from_min_size(tl, size * zoom);
            let title_h = TITLE_H * zoom;
            let title_rect = Rect::from_min_size(tl, Vec2::new(size.x * zoom, title_h));
            let body_rect =
                Rect::from_min_max(Pos2::new(rect.min.x, rect.min.y + title_h), rect.max);

            let painter = ui.painter_at(panel);
            painter.rect_filled(rect, 6.0, NODE_BG);
            // Border highlights only when selected; Claude nodes use orange.
            let border = if !is_focused {
                Color32::from_rgb(0x3a, 0x3d, 0x44)
            } else if is_agent {
                ORANGE
            } else {
                ACCENT
            };
            painter.rect_stroke(rect, 6.0, Stroke::new(1.5f32, border));

            let title_fill = if is_agent && is_focused {
                Color32::from_rgb(0x40, 0x2c, 0x18)
            } else if is_focused {
                Color32::from_rgb(0x2b, 0x3a, 0x55)
            } else {
                Color32::from_rgb(0x26, 0x28, 0x2e)
            };
            painter.rect_filled(title_rect, 6.0, title_fill);

            // status dot + title text
            let dot = if is_agent {
                ORANGE
            } else if is_terminal {
                Color32::from_rgb(0x35, 0xc0, 0x7a)
            } else {
                Color32::from_rgb(0xe0, 0xc0, 0x50)
            };
            let dot_c = Pos2::new(title_rect.min.x + 10.0 * zoom, title_rect.center().y);
            painter.circle_filled(dot_c, 3.5 * zoom, dot);

            let cs = 16.0 * zoom;
            let close_rect = Rect::from_min_size(
                Pos2::new(
                    title_rect.max.x - cs - 6.0 * zoom,
                    title_rect.center().y - cs / 2.0,
                ),
                Vec2::splat(cs),
            );

            let text_left = title_rect.min.x + 20.0 * zoom;
            let text_max = close_rect.min.x - 4.0 * zoom;
            painter.text(
                Pos2::new(text_left, title_rect.center().y),
                Align2::LEFT_CENTER,
                elide(&title, ((text_max - text_left) / (7.0 * zoom)).max(3.0) as usize),
                FontId::proportional((12.5 * zoom).clamp(8.0, 24.0)),
                Color32::from_rgb(0xcf, 0xd2, 0xd8),
            );

            if is_active {
                let close = ui.interact(close_rect, Id::new(("close", id)), Sense::click());
                if close.clicked() {
                    to_close = Some(id);
                }
                let cc = close_rect.center();
                let r = cs * 0.26;
                let col = if close.hovered() {
                    Color32::from_rgb(0xff, 0x6b, 0x6b)
                } else {
                    Color32::from_gray(0x88)
                };
                painter.line_segment([cc + Vec2::new(-r, -r), cc + Vec2::new(r, r)], Stroke::new(1.6f32, col));
                painter.line_segment([cc + Vec2::new(-r, r), cc + Vec2::new(r, -r)], Stroke::new(1.6f32, col));

                let drag_rect =
                    Rect::from_min_max(title_rect.min, Pos2::new(close_rect.min.x, title_rect.max.y));
                let td = ui.interact(drag_rect, Id::new(("title", id)), Sense::click_and_drag());
                if td.drag_started() {
                    self.canvases[a].moving = Some(id);
                    self.canvases[a].focused = Some(id);
                    bring_to_front = Some(id);
                }
                if self.canvases[a].moving == Some(id) {
                    self.canvases[a].nodes[i].pos += td.drag_delta() / zoom;
                }
                if td.drag_stopped() {
                    self.canvases[a].moving = None;
                    self.mark_dirty();
                }
                if td.clicked() {
                    self.canvases[a].focused = Some(id);
                    bring_to_front = Some(id);
                }
                if td.hovered() {
                    ctx.set_cursor_icon(egui::CursorIcon::Grab);
                }

                let hs = RESIZE_HANDLE * zoom;
                let handle_rect = Rect::from_min_size(rect.max - Vec2::splat(hs), Vec2::splat(hs));
                let rz = ui.interact(handle_rect, Id::new(("resize", id)), Sense::drag());
                if rz.drag_started() {
                    self.canvases[a].resizing = Some(id);
                    self.canvases[a].focused = Some(id);
                    bring_to_front = Some(id);
                }
                if self.canvases[a].resizing == Some(id) {
                    let d = rz.drag_delta() / zoom;
                    let ns = self.canvases[a].nodes[i].size + d;
                    self.canvases[a].nodes[i].size = Vec2::new(ns.x.max(MIN_W), ns.y.max(MIN_H));
                }
                if rz.drag_stopped() {
                    self.canvases[a].resizing = None;
                    self.mark_dirty();
                }
                if rz.hovered() {
                    ctx.set_cursor_icon(egui::CursorIcon::ResizeNwSe);
                }
                for k in 1..=3 {
                    let o = k as f32 * 3.5 * zoom;
                    painter.line_segment(
                        [
                            Pos2::new(handle_rect.max.x - o, handle_rect.max.y),
                            Pos2::new(handle_rect.max.x, handle_rect.max.y - o),
                        ],
                        Stroke::new(1.0f32, Color32::from_gray(0x77)),
                    );
                }

                let body = ui.interact(body_rect, Id::new(("body", id)), Sense::click());
                if body.clicked() {
                    self.canvases[a].focused = Some(id);
                    bring_to_front = Some(id);
                }
            }

            match &self.canvases[a].nodes[i].kind {
                NodeKind::Terminal(_) => self.render_terminal(i, ui, body_rect, cell, ctx, is_focused),
                NodeKind::Note(_) => self.render_note(i, ui, body_rect, zoom),
            }
        }

        if let Some(id) = bring_to_front {
            self.bring_to_front(id);
        }
        if let Some(id) = to_close {
            self.close_node(id);
            self.mark_dirty();
        }

        // Minimap (bottom-right). Panning is suppressed while over it.
        let mm_rect = Self::minimap_rect(panel);
        let over_mm = pointer.map_or(false, |p| mm_rect.contains(p));

        // "Open Folder" prompt for a new, unbound canvas.
        let show_card =
            self.canvases[a].workspace.is_none() && self.canvases[a].nodes.is_empty();
        let card_rect = Rect::from_center_size(panel.center(), Vec2::new(380.0, 156.0));
        let btn_rect = Rect::from_center_size(
            Pos2::new(panel.center().x, panel.center().y + 30.0),
            Vec2::new(190.0, 42.0),
        );
        let over_card = show_card && pointer.map_or(false, |p| card_rect.contains(p));
        let mut open_ws = false;
        if show_card {
            let p = ui.painter_at(panel);
            p.rect_filled(card_rect, 12.0, Color32::from_rgb(0x22, 0x25, 0x2c));
            p.rect_stroke(card_rect, 12.0, Stroke::new(1.0f32, Color32::from_gray(0x44)));
            p.text(
                Pos2::new(panel.center().x, card_rect.min.y + 36.0),
                Align2::CENTER_CENTER,
                "This canvas has no workspace",
                FontId::proportional(16.0),
                Color32::from_rgb(0xd0, 0xd4, 0xda),
            );
            p.text(
                Pos2::new(panel.center().x, card_rect.min.y + 62.0),
                Align2::CENTER_CENTER,
                "Open a folder to name this tab and start a terminal there",
                FontId::proportional(12.0),
                Color32::from_gray(0x99),
            );
            let btn = ui.interact(btn_rect, Id::new(("open_ws", a)), Sense::click());
            let fill = if btn.hovered() {
                ACCENT
            } else {
                Color32::from_rgb(0x3a, 0x6a, 0xc0)
            };
            p.rect_filled(btn_rect, 8.0, fill);
            p.text(
                btn_rect.center(),
                Align2::CENTER_CENTER,
                "Open Folder…",
                FontId::proportional(14.0),
                Color32::WHITE,
            );
            if btn.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if btn.clicked() {
                open_ws = true;
            }
        }

        // Background: pan + deselect + right-click menu (empty space only).
        if topmost_hover.is_none()
            && !over_mm
            && !over_card
            && self.canvases[a].moving.is_none()
            && self.canvases[a].resizing.is_none()
        {
            let bg = ui.interact(panel, Id::new(("canvas-bg", a)), Sense::click_and_drag());
            if bg.dragged() {
                self.canvases[a].offset += bg.drag_delta();
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            if bg.clicked() {
                self.canvases[a].focused = None;
            }
            if bg.secondary_clicked() {
                if let Some(p) = pointer {
                    self.canvases[a].menu_world = self.s2w(p);
                }
            }
            let menu_world = self.canvases[a].menu_world;
            bg.context_menu(|ui| {
                if ui.button("New Terminal here").clicked() {
                    self.add_terminal(menu_world);
                    ui.close_menu();
                }
                if ui.button("New Claude Code here").clicked() {
                    self.add_claude(menu_world);
                    ui.close_menu();
                }
                if ui.button("New Note here").clicked() {
                    self.add_note(menu_world);
                    ui.close_menu();
                }
            });
        }

        self.draw_minimap(ui, panel, mm_rect);

        if open_ws {
            self.open_workspace();
        }
    }

    fn minimap_rect(panel: Rect) -> Rect {
        let size = Vec2::new(210.0, 140.0);
        let margin = 14.0;
        Rect::from_min_size(
            Pos2::new(panel.max.x - size.x - margin, panel.max.y - size.y - margin),
            size,
        )
    }

    fn draw_minimap(&mut self, ui: &mut egui::Ui, panel: Rect, mm: Rect) {
        let a = self.active;
        let painter = ui.painter_at(panel);

        // Panel background + border.
        painter.rect_filled(mm, 6.0, Color32::from_rgba_unmultiplied(0x0e, 0x10, 0x14, 0xD8));
        painter.rect_stroke(mm, 6.0, Stroke::new(1.0f32, Color32::from_gray(0x44)));

        // World bounds = union of all nodes and the current viewport.
        let view_tl = self.s2w(panel.min);
        let view_br = self.s2w(panel.max);
        let mut min = Pos2::new(view_tl.x.min(view_br.x), view_tl.y.min(view_br.y));
        let mut max = Pos2::new(view_tl.x.max(view_br.x), view_tl.y.max(view_br.y));
        for n in &self.canvases[a].nodes {
            min.x = min.x.min(n.pos.x);
            min.y = min.y.min(n.pos.y);
            max.x = max.x.max(n.pos.x + n.size.x);
            max.y = max.y.max(n.pos.y + n.size.y);
        }
        // Pad a little so nothing hugs the edge.
        let padw = (max.x - min.x) * 0.06 + 20.0;
        let padh = (max.y - min.y) * 0.06 + 20.0;
        min -= Vec2::new(padw, padh);
        max += Vec2::new(padw, padh);

        let draw = mm.shrink(8.0);
        let bw = (max.x - min.x).max(1.0);
        let bh = (max.y - min.y).max(1.0);
        let scale = (draw.width() / bw).min(draw.height() / bh);
        let origin = draw.min
            + Vec2::new(
                (draw.width() - bw * scale) * 0.5,
                (draw.height() - bh * scale) * 0.5,
            );
        let to_mm = |p: Pos2| origin + (p - min) * scale;

        // Nodes.
        for n in &self.canvases[a].nodes {
            let r = Rect::from_min_size(to_mm(n.pos), (n.size * scale).max(Vec2::splat(2.0)));
            let col = match &n.kind {
                NodeKind::Terminal(t) if t.agent => ORANGE,
                NodeKind::Terminal(_) => Color32::from_rgb(0x35, 0x6a, 0x9a),
                NodeKind::Note(_) => Color32::from_rgb(0x9a, 0x86, 0x40),
            };
            painter.rect_filled(r, 1.0, col);
            if self.canvases[a].focused == Some(n.id) {
                painter.rect_stroke(r, 1.0, Stroke::new(1.0f32, ACCENT));
            }
        }

        // Current viewport indicator.
        let vr = Rect::from_min_max(to_mm(view_tl), to_mm(view_br)).intersect(draw);
        painter.rect_stroke(vr, 0.0, Stroke::new(1.0f32, Color32::from_rgb(0xcf, 0xd6, 0xe6)));

        // Click / drag to recenter the view on that world point.
        let resp = ui.interact(mm, Id::new(("minimap", a)), Sense::click_and_drag());
        if resp.dragged() || resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let world = min + (p - origin) / scale;
                let z = self.canvases[a].zoom;
                self.canvases[a].offset = panel.center().to_vec2() - world.to_vec2() * z;
            }
        }
    }

    fn draw_grid(&self, painter: &egui::Painter, rect: Rect) {
        let zoom = self.cv().zoom;
        let step = GRID * zoom;
        if step < 6.0 {
            return;
        }
        // Dot grid, 50% lighter than the previous lines (alpha 8 -> 4).
        let col = Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 4);
        let radius = (1.3 * zoom).clamp(0.8, 2.0);
        let start_w = self.s2w(rect.min);

        // Precompute the screen x positions of each grid column in view.
        let mut xs: Vec<f32> = Vec::new();
        let mut wx = (start_w.x / GRID).floor() * GRID;
        loop {
            let sx = self.w2s(Pos2::new(wx, 0.0)).x;
            if sx > rect.max.x {
                break;
            }
            if sx >= rect.min.x {
                xs.push(sx);
            }
            wx += GRID;
        }

        // Draw a dot at every (column, row) intersection.
        let mut wy = (start_w.y / GRID).floor() * GRID;
        loop {
            let sy = self.w2s(Pos2::new(0.0, wy)).y;
            if sy > rect.max.y {
                break;
            }
            if sy >= rect.min.y {
                for &sx in &xs {
                    painter.circle_filled(Pos2::new(sx, sy), radius, col);
                }
            }
            wy += GRID;
        }
    }

    fn render_note(&mut self, i: usize, ui: &mut egui::Ui, body_rect: Rect, zoom: f32) {
        let a = self.active;
        let painter = ui.painter_at(body_rect);
        painter.rect_filled(body_rect, 0.0, Color32::from_rgb(0x2a, 0x27, 0x1c));
        let id = self.canvases[a].nodes[i].id;
        let mut save_on_blur = false;
        if let NodeKind::Note(note) = &mut self.canvases[a].nodes[i].kind {
            let mut text = note.text.clone();
            let resp = ui.put(
                body_rect.shrink(8.0 * zoom),
                egui::TextEdit::multiline(&mut text)
                    .frame(false)
                    .font(FontId::proportional((13.0 * zoom).clamp(8.0, 26.0)))
                    .text_color(Color32::from_rgb(0xf0, 0xe6, 0xc8))
                    .hint_text("note…"),
            );
            if resp.changed() {
                note.text = text;
            }
            if resp.has_focus() {
                self.canvases[a].focused = Some(id);
            }
            save_on_blur = resp.lost_focus();
        }
        if save_on_blur {
            self.mark_dirty();
        }
    }

    fn render_terminal(
        &mut self,
        i: usize,
        ui: &mut egui::Ui,
        body_rect: Rect,
        cell: Vec2,
        ctx: &egui::Context,
        is_focused: bool,
    ) {
        let a = self.active;
        let zoom = self.canvases[a].zoom;
        let inner = body_rect.shrink(4.0 * zoom);
        let cw = cell.x * zoom;
        let ch = cell.y * zoom;
        let cols = ((inner.width() / cw).floor().max(1.0)) as u16;
        let rows = ((inner.height() / ch).floor().max(1.0)) as u16;

        let painter = ui.painter_at(body_rect);
        painter.rect_filled(body_rect, 0.0, TERM_BG);

        let (cwd, id, agent) = {
            let n = &self.canvases[a].nodes[i];
            let (cwd, agent) = if let NodeKind::Terminal(t) = &n.kind {
                (t.cwd.clone(), t.agent)
            } else {
                (None, false)
            };
            (cwd, n.id, agent)
        };
        let startup = if agent { Some("claude") } else { None };

        let (parser, dead, scrolled) = {
            let NodeKind::Terminal(t) = &mut self.canvases[a].nodes[i].kind else {
                return;
            };
            if t.term.is_none() {
                match PtyTerminal::spawn(rows, cols, cwd.as_deref(), ctx.clone(), id, startup) {
                    Ok(term) => t.term = Some(term),
                    Err(e) => {
                        painter.text(
                            inner.min,
                            Align2::LEFT_TOP,
                            format!("failed to start terminal: {e}"),
                            FontId::monospace(13.0),
                            Color32::from_rgb(0xff, 0x80, 0x80),
                        );
                        return;
                    }
                }
            }
            let term = t.term.as_mut().unwrap();
            term.resize(rows, cols);
            (term.parser.clone(), term.is_dead(), term.is_scrolled())
        };

        if is_focused && !ctx.wants_keyboard_input() {
            self.send_input(i, ui);
        }

        let guard = match parser.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let screen = guard.screen();
        let font = FontId::monospace(BASE_FONT * zoom);

        for row in 0..rows {
            let y = inner.min.y + row as f32 * ch;
            let mut col = 0u16;
            while col < cols {
                let (fg, bg, inv) = cell_style(screen, row, col);
                let start = col;
                let mut text = String::new();
                loop {
                    let (cfg, cbg, cinv) = cell_style(screen, row, col);
                    if cfg != fg || cbg != bg || cinv != inv {
                        break;
                    }
                    match screen.cell(row, col) {
                        Some(c) if !c.contents().is_empty() => text.push_str(&c.contents()),
                        _ => text.push(' '),
                    }
                    col += 1;
                    if col >= cols {
                        break;
                    }
                }
                let x = inner.min.x + start as f32 * cw;
                let run_w = (col - start) as f32 * cw;

                let raw_fg = to_color(fg, TERM_FG);
                let raw_bg = to_color(bg, TERM_BG);
                let (eff_fg, eff_bg) = if inv { (raw_bg, raw_fg) } else { (raw_fg, raw_bg) };

                if inv || bg != vt100::Color::Default {
                    painter.rect_filled(
                        Rect::from_min_size(Pos2::new(x, y), Vec2::new(run_w, ch)),
                        0.0,
                        eff_bg,
                    );
                }
                if !text.trim().is_empty() {
                    painter.text(Pos2::new(x, y), Align2::LEFT_TOP, text, font.clone(), eff_fg);
                }
            }
        }

        // Only draw the cursor when viewing the live bottom.
        if !scrolled && !dead && !screen.hide_cursor() {
            let (cr, cc) = screen.cursor_position();
            if cr < rows && cc < cols {
                let x = inner.min.x + cc as f32 * cw;
                let y = inner.min.y + cr as f32 * ch;
                let cur = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cw, ch));
                if is_focused {
                    // Solid block cursor with the glyph re-drawn on top in the
                    // background color, so the character stays readable.
                    painter.rect_filled(cur, 0.0, TERM_FG);
                    let glyph = screen
                        .cell(cr, cc)
                        .map(|c| c.contents())
                        .unwrap_or_default();
                    if !glyph.is_empty() {
                        painter.text(
                            Pos2::new(x, y),
                            Align2::LEFT_TOP,
                            glyph,
                            font.clone(),
                            TERM_BG,
                        );
                    }
                } else {
                    // Hollow box when the terminal isn't focused.
                    painter.rect_stroke(cur, 0.0, Stroke::new(1.0f32, Color32::from_gray(0x88)));
                }
            }
        }

        // Scrollback indicator while viewing history.
        if scrolled {
            let label = "▲ history — ⌥scroll down or type to return to live".to_string();
            let fid = FontId::proportional((11.0 * zoom).clamp(8.0, 20.0));
            let galley = painter.layout_no_wrap(label.clone(), fid.clone(), Color32::WHITE);
            let pad = 5.0;
            let box_rect = Rect::from_min_size(
                Pos2::new(inner.max.x - galley.size().x - pad * 2.0, inner.min.y),
                Vec2::new(galley.size().x + pad * 2.0, galley.size().y + pad),
            );
            painter.rect_filled(box_rect, 3.0, Color32::from_rgba_unmultiplied(0x30, 0x34, 0x40, 0xE0));
            painter.text(
                box_rect.min + Vec2::new(pad, pad * 0.5),
                Align2::LEFT_TOP,
                label,
                fid,
                Color32::from_rgb(0xcf, 0xd6, 0xe6),
            );
        }

        if dead {
            painter.text(
                Pos2::new(inner.min.x + 4.0, inner.max.y - 4.0),
                Align2::LEFT_BOTTOM,
                "● process exited — ⌘W to close",
                FontId::proportional((11.0 * zoom).clamp(8.0, 20.0)),
                Color32::from_rgb(0xd6, 0x70, 0x70),
            );
        }
    }

    fn send_input(&mut self, i: usize, ui: &egui::Ui) {
        let a = self.active;
        let events = ui.input(|inp| inp.events.clone());
        let mut out: Vec<u8> = Vec::new();
        for e in &events {
            match e {
                egui::Event::Text(t) => {
                    let s: String = t.chars().filter(|c| !c.is_control()).collect();
                    if !s.is_empty() {
                        out.extend_from_slice(s.as_bytes());
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if modifiers.command || modifiers.mac_cmd {
                        continue;
                    }
                    if let Some(bytes) = key_to_bytes(*key, modifiers) {
                        out.extend_from_slice(&bytes);
                    }
                }
                egui::Event::Paste(s) => out.extend_from_slice(s.as_bytes()),
                _ => {}
            }
        }
        if out.is_empty() {
            return;
        }
        if let NodeKind::Terminal(t) = &mut self.canvases[a].nodes[i].kind {
            if let Some(term) = &mut t.term {
                term.send(&out);
            }
        }
    }
}

/// Append a broad-coverage symbol font as a fallback so UI glyphs
/// (● ✕ ▦ ⌘ ⌥ · … and box-drawing) render instead of showing tofu boxes.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "/System/Library/Fonts/Apple Symbols.ttf",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("symbols".to_owned(), egui::FontData::from_owned(bytes));
            // Only add the symbol fallback to the proportional (UI) family. The
            // monospace family must stay pure so terminal cell metrics (advance
            // width and row height) match the glyphs exactly.
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("symbols".to_owned());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

/// Show the native macOS folder chooser and return the selected path.
/// Returns None if the user cancels.
fn pick_folder() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("osascript")
        .args([
            "-e",
            "POSIX path of (choose folder with prompt \"Select a workspace folder\")",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // cancelled
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(path))
    }
}

fn elide(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else if max_chars <= 1 {
        "…".to_string()
    } else {
        let t: String = s.chars().take(max_chars - 1).collect();
        format!("{t}…")
    }
}

fn cell_style(screen: &vt100::Screen, row: u16, col: u16) -> (vt100::Color, vt100::Color, bool) {
    match screen.cell(row, col) {
        Some(c) => (c.fgcolor(), c.bgcolor(), c.inverse()),
        None => (vt100::Color::Default, vt100::Color::Default, false),
    }
}

fn to_color(c: vt100::Color, default: Color32) -> Color32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => ansi_256(i),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

fn ansi_256(idx: u8) -> Color32 {
    const BASE: [(u8, u8, u8); 16] = [
        (0x1e, 0x1e, 0x1e),
        (0xcd, 0x31, 0x31),
        (0x0d, 0xbc, 0x79),
        (0xe5, 0xe5, 0x10),
        (0x24, 0x72, 0xc8),
        (0xbc, 0x3f, 0xbc),
        (0x11, 0xa8, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x66, 0x66, 0x66),
        (0xf1, 0x4c, 0x4c),
        (0x23, 0xd1, 0x8b),
        (0xf5, 0xf5, 0x43),
        (0x3b, 0x8e, 0xea),
        (0xd6, 0x70, 0xd6),
        (0x29, 0xb8, 0xdb),
        (0xff, 0xff, 0xff),
    ];
    if idx < 16 {
        let (r, g, b) = BASE[idx as usize];
        Color32::from_rgb(r, g, b)
    } else if idx < 232 {
        let i = idx - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let conv = |v: u8| if v == 0 { 0u8 } else { 55 + v * 40 };
        Color32::from_rgb(conv(r), conv(g), conv(b))
    } else {
        let v = 8 + (idx - 232) * 10;
        Color32::from_rgb(v, v, v)
    }
}

fn key_to_bytes(key: egui::Key, m: &egui::Modifiers) -> Option<Vec<u8>> {
    if m.ctrl {
        if let Some(b) = ctrl_letter(key) {
            return Some(vec![b & 0x1f]);
        }
    }
    let seq: &[u8] = match key {
        egui::Key::Enter => b"\r",
        egui::Key::Tab => b"\t",
        egui::Key::Backspace => b"\x7f",
        egui::Key::Escape => b"\x1b",
        egui::Key::ArrowUp => b"\x1b[A",
        egui::Key::ArrowDown => b"\x1b[B",
        egui::Key::ArrowRight => b"\x1b[C",
        egui::Key::ArrowLeft => b"\x1b[D",
        egui::Key::Home => b"\x1b[H",
        egui::Key::End => b"\x1b[F",
        egui::Key::Delete => b"\x1b[3~",
        egui::Key::Insert => b"\x1b[2~",
        egui::Key::PageUp => b"\x1b[5~",
        egui::Key::PageDown => b"\x1b[6~",
        _ => return None,
    };
    Some(seq.to_vec())
}

fn ctrl_letter(key: egui::Key) -> Option<u8> {
    use egui::Key::*;
    Some(match key {
        A => b'a', B => b'b', C => b'c', D => b'd', E => b'e', F => b'f',
        G => b'g', H => b'h', I => b'i', J => b'j', K => b'k', L => b'l',
        M => b'm', N => b'n', O => b'o', P => b'p', Q => b'q', R => b'r',
        S => b's', T => b't', U => b'u', V => b'v', W => b'w', X => b'x',
        Y => b'y', Z => b'z',
        _ => return None,
    })
}
