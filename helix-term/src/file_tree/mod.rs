//! A modal file tree — a fork-local customization, not part of upstream Helix.
//!
//! Opened with `:file-tree`, it takes over the screen the way a picker does and
//! closes on `Esc`. Modal rather than a sidebar on purpose: a permanent pane
//! would cost width in every split forever, and it would need the editor's
//! render path to reserve space for it. This needs neither.
//!
//! Everything lives under `helix-term/src/file_tree/`; see FORK.md at the
//! repository root for the upstream files this touches.

pub mod model;

use std::path::PathBuf;

use helix_core::{command_line::Args, Position};
use helix_view::{
    graphics::{CursorKind, Rect},
    Editor,
};
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, Widget},
};

use crate::{
    compositor::{Component, Compositor, Context, Event, EventResult},
    ctrl, job, key, shift,
    ui::{overlay::overlaid, PromptEvent},
};

use model::Tree;

/// Columns of indentation per nesting level.
const INDENT: usize = 2;
/// Width of the open/shut marker in front of every row, blank for files.
const MARKER_WIDTH: usize = 2;

pub struct FileTree {
    tree: Tree,
    /// First visible row. Kept in the component rather than the model because
    /// it only means anything against a viewport height, which is known at
    /// render time.
    offset: usize,
    /// Rows the last render could fit, for page movement.
    height: usize,
}

impl FileTree {
    pub const ID: &'static str = "file-tree";

    pub fn new(root: PathBuf, editor: &Editor) -> std::io::Result<Self> {
        // `file_explorer.hidden` reads as "hide hidden files", so it inverts.
        let show_hidden = !editor.config().file_explorer.hidden;
        let children = model::read_dir(&root, show_hidden)?;

        Ok(Self {
            tree: Tree::new(root, children),
            offset: 0,
            height: 0,
        })
    }
}

impl FileTree {
    /// Rows a page jump covers: half a screen, matching Helix's `C-d`/`C-u`.
    fn page(&self) -> isize {
        (self.height / 2).max(1) as isize
    }

    /// Drags the viewport just far enough to keep the cursor on screen.
    fn scroll_to_selection(&mut self) {
        if self.height == 0 {
            return;
        }

        // Clamp first, so a tree that shrank cannot leave the viewport parked
        // past the end, then correct for the cursor.
        self.offset = self.offset.min(self.tree.len().saturating_sub(self.height));

        let selected = self.tree.selected();
        if selected < self.offset {
            self.offset = selected;
        } else if selected >= self.offset + self.height {
            self.offset = selected + 1 - self.height;
        }
    }
}

impl Component for FileTree {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;
        let text_style = theme.get("ui.text");
        let directory_style = theme.get("ui.text.directory");

        let selected_style = theme.get("ui.menu.selected");

        surface.clear_with(area, theme.get("ui.background"));

        let title = helix_stdx::path::fold_home_dir(self.tree.root())
            .to_string_lossy()
            .into_owned();
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        block.render(area, surface);

        self.height = inner.height as usize;
        self.scroll_to_selection();

        for (index, entry) in self
            .tree
            .entries()
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(self.height)
        {
            let row = index - self.offset;
            let indent = entry.depth * INDENT;
            let marker = match (entry.is_dir, entry.expanded) {
                (true, true) => "▾ ",
                (true, false) => "▸ ",
                (false, _) => "  ",
            };
            let name = entry.name();
            let (name, mut style) = if entry.is_dir {
                (format!("{name}/"), directory_style)
            } else {
                (name.into_owned(), text_style)
            };

            let x = inner.x + indent as u16;
            let y = inner.y + row as u16;
            let width = inner.width.saturating_sub(indent as u16) as usize;

            if index == self.tree.selected() {
                // Paint the whole row first so the highlight reads as a bar,
                // then let the text inherit that background.
                surface.set_style(
                    Rect {
                        x: inner.x,
                        y,
                        width: inner.width,
                        height: 1,
                    },
                    selected_style,
                );
                style = selected_style;
            }

            surface.set_stringn(x, y, marker, width, style);
            surface.set_stringn(
                x + MARKER_WIDTH as u16,
                y,
                &name,
                width.saturating_sub(MARKER_WIDTH),
                style,
            );
        }
    }

    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        match *key {
            key!(Esc) | ctrl!('c') => {
                return EventResult::Consumed(Some(Box::new(|compositor, _| {
                    compositor.remove(FileTree::ID);
                })))
            }
            key!('j') | key!(Down) | ctrl!('n') => self.tree.select_next(),
            key!('k') | key!(Up) | ctrl!('p') => self.tree.select_prev(),
            key!('g') => self.tree.select_first(),
            shift!('G') => self.tree.select_last(),
            key!(PageDown) | ctrl!('d') => self.tree.select_by(self.page()),
            key!(PageUp) | ctrl!('u') => self.tree.select_by(-self.page()),
            // Modal: swallow everything else rather than let it reach the
            // buffer underneath.
            _ => {}
        }

        EventResult::Consumed(None)
    }

    /// Answering here keeps the editor's cursor from showing through; the
    /// compositor stops at the first layer that returns `Some`.
    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<Position>, CursorKind) {
        (Some(Position::default()), CursorKind::Hidden)
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}

/// `:file-tree` — the entry point registered in `TYPABLE_COMMAND_LIST`.
///
/// Typable commands get no compositor, so pushing a layer goes through a job
/// callback, the same way `:lsp-workspace-command` does.
pub fn open(cx: &mut Context, _args: Args, event: PromptEvent) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    let root = helix_loader::find_workspace().0;
    if !root.exists() {
        anyhow::bail!("workspace directory does not exist");
    }

    let tree = FileTree::new(root, cx.editor)?;

    cx.jobs.callback(async move {
        let call: job::Callback = job::Callback::EditorCompositor(Box::new(
            move |_editor, compositor: &mut Compositor| {
                compositor.push(Box::new(overlaid(tree)));
            },
        ));
        Ok(call)
    });

    Ok(())
}
