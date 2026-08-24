//! Node data model and its on-disk (serde) representation.

use serde::{Deserialize, Serialize};

use crate::terminal::PtyTerminal;

/// A draggable card on the canvas.
pub struct Node {
    pub id: u64,
    pub title: String,
    /// Top-left position in world coordinates (canvas space at zoom 1.0).
    pub pos: egui::Pos2,
    /// Size in world units.
    pub size: egui::Vec2,
    pub kind: NodeKind,
}

pub enum NodeKind {
    Terminal(TerminalNode),
    Note(NoteNode),
}

pub struct TerminalNode {
    pub cwd: Option<std::path::PathBuf>,
    /// A Claude Code terminal (launches `claude`, themed orange).
    pub agent: bool,
    /// Lazily created on first render (needs an egui Context).
    pub term: Option<PtyTerminal>,
}

pub struct NoteNode {
    pub text: String,
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// The whole app: a set of tabbed canvases plus a shared id counter.
#[derive(Serialize, Deserialize)]
pub struct SavedApp {
    pub active: usize,
    pub next_id: u64,
    pub canvases: Vec<SavedCanvas>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedCanvas {
    pub name: String,
    #[serde(default)]
    pub workspace: Option<String>,
    pub offset: [f32; 2],
    pub zoom: f32,
    pub nodes: Vec<SavedNode>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedNode {
    pub id: u64,
    pub title: String,
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub kind: SavedKind,
}

#[derive(Serialize, Deserialize)]
pub enum SavedKind {
    Terminal {
        cwd: Option<String>,
        #[serde(default)]
        agent: bool,
    },
    Note {
        text: String,
    },
}

impl Node {
    pub fn to_saved(&self) -> SavedNode {
        let kind = match &self.kind {
            NodeKind::Terminal(t) => SavedKind::Terminal {
                cwd: t.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                agent: t.agent,
            },
            NodeKind::Note(n) => SavedKind::Note {
                text: n.text.clone(),
            },
        };
        SavedNode {
            id: self.id,
            title: self.title.clone(),
            pos: [self.pos.x, self.pos.y],
            size: [self.size.x, self.size.y],
            kind,
        }
    }

    pub fn from_saved(s: SavedNode) -> Self {
        let kind = match s.kind {
            SavedKind::Terminal { cwd, agent } => NodeKind::Terminal(TerminalNode {
                cwd: cwd.map(std::path::PathBuf::from),
                agent,
                term: None,
            }),
            SavedKind::Note { text } => NodeKind::Note(NoteNode { text }),
        };
        Node {
            id: s.id,
            title: s.title,
            pos: egui::pos2(s.pos[0], s.pos[1]),
            size: egui::vec2(s.size[0], s.size[1]),
            kind,
        }
    }
}
