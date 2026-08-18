//! Welcome screen - a fork local customization, not part of upstream Helix.
//!
//! Everything lives here so upstream merges stay cheap. The rest of the tree is
//! touched in exactly two places. both marked with "fork" sentinel comments:
//! 1. helix-term/src/lib.rs            -- `pub mod welcome;`.
//! 2. helix-term/src/application.rs    -- push the layer at startup.

use helix_core::Position;
use helix_view::{
    graphics::{CursorKind, Rect},
    Editor,
};
use tui::buffer::Buffer as Surface;

use crate::{
    args::Args,
    compositor::{Component, Context, Event, EventResult},
};

use std::io::{stdin, IsTerminal};

pub fn should_show(args: &Args) -> bool {
    !cfg!(feature = "integration")
        && args.files.is_empty()
        && !args.load_tutor
        && stdin().is_terminal()
}

#[derive(Default)]
pub struct Welcome;

impl Welcome {
    pub const ID: &'static str = "welcome";
    pub fn new() -> Self {
        Self
    }
}

impl Component for Welcome {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // Leave the bottom row alone so the status line sill shows through
        let area = area.clip_bottom(1);

        let text = "welcome";
        let x = area.x + area.width.saturating_sub(text.len() as u16) / 2;
        let y = area.y + area.height / 2;
        surface.set_string(x, y, text, cx.editor.theme.get("ui.text"));
    }

    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        if !matches!(event, Event::Key(_)) {
            return EventResult::Ignored(None);
        }

        // `Ignored` with a callback is the popup auto-close pattern: the key
        // still reaches the editor underneath, and we get removed afterwards.
        EventResult::Ignored(Some(Box::new(|compositor, _| {
            compositor.remove(Welcome::ID);
        })))
    }

    /// The compositor walks layers front-to-back and stops at the first `Some`.
    /// Answering here is what suppresses the editor's own cursor.
    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<Position>, CursorKind) {
        (Some(Position::default()), CursorKind::Hidden)
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}
