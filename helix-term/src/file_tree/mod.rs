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
    ctrl, job, key,
    ui::{overlay::overlaid, PromptEvent},
};

use model::Tree;

/// Columns of indentation per nesting level.
const INDENT: usize = 2;
/// Width of the open/shut marker in front of every row, blank for files.
const MARKER_WIDTH: usize = 2;

pub struct FileTree {
    tree: Tree,
}

impl FileTree {
    pub const ID: &'static str = "file-tree";

    pub fn new(root: PathBuf, editor: &Editor) -> std::io::Result<Self> {
        // `file_explorer.hidden` reads as "hide hidden files", so it inverts.
        let show_hidden = !editor.config().file_explorer.hidden;
        let children = model::read_dir(&root, show_hidden)?;

        Ok(Self {
            tree: Tree::new(root, children),
        })
    }
}

impl Component for FileTree {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;
        let text_style = theme.get("ui.text");
        let directory_style = theme.get("ui.text.directory");

        surface.clear_with(area, theme.get("ui.background"));

        let title = helix_stdx::path::fold_home_dir(self.tree.root())
            .to_string_lossy()
            .into_owned();
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        block.render(area, surface);

        for (row, entry) in self
            .tree
            .entries()
            .iter()
            .take(inner.height as usize)
            .enumerate()
        {
            let indent = entry.depth * INDENT;
            let marker = match (entry.is_dir, entry.expanded) {
                (true, true) => "▾ ",
                (true, false) => "▸ ",
                (false, _) => "  ",
            };
            let name = entry.name();
            let (name, style) = if entry.is_dir {
                (format!("{name}/"), directory_style)
            } else {
                (name.into_owned(), text_style)
            };

            let x = inner.x + indent as u16;
            let y = inner.y + row as u16;
            let width = inner.width.saturating_sub(indent as u16) as usize;

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
            key!(Esc) | ctrl!('c') => EventResult::Consumed(Some(Box::new(|compositor, _| {
                compositor.remove(FileTree::ID);
            }))),
            // Modal: swallow everything else rather than let it reach the
            // buffer underneath.
            _ => EventResult::Consumed(None),
        }
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
