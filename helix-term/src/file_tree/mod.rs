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
pub mod preview;

use std::path::{Path, PathBuf};

use helix_core::{command_line::Args, text_annotations::TextAnnotations, Position};
use helix_view::{
    editor::Action,
    fork::{icons, preview_layout::PreviewLayout},
    graphics::{CursorKind, Margin, Rect},
    view::ViewPosition,
    Editor,
};
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, Widget},
};

use crate::{
    compositor::{Component, Compositor, Context, Event, EventResult},
    ctrl, job, key, shift,
    ui::{
        completers, document::render_document, overlay::overlaid,
        text_decorations::DecorationManager, EditorView, Prompt, PromptEvent,
    },
};

use model::Tree;
use preview::Previews;

/// Columns of indentation per nesting level.
const INDENT: usize = 2;
/// Width of the open/shut marker in front of every row, blank for files.
const MARKER_WIDTH: usize = 2;
/// Width of the icon column: one glyph and the space after it. Nerd Font
/// glyphs measure as one cell; a terminal that renders them double-width needs
/// this at 3.
const ICON_WIDTH: usize = 2;

pub struct FileTree {
    tree: Tree,
    /// First visible row. Kept in the component rather than the model because
    /// it only means anything against a viewport height, which is known at
    /// render time.
    offset: usize,
    /// Rows the last render could fit, for page movement.
    height: usize,
    /// Read once at open: directories opened later use the same rule as the
    /// root listing, so the tree stays internally consistent even if the
    /// setting changes mid-session.
    show_hidden: bool,
    /// An in-progress prompt and what it will do once submitted. Owned by the
    /// tree rather than pushed as its own layer, so that acting on the answer
    /// needs no reaching across the compositor to find this component again.
    prompt: Option<(Pending, Prompt)>,
    /// A destructive action waiting on a yes.
    confirm: Option<Confirm>,
    /// Files already read for the preview pane. Filled as the cursor moves,
    /// and thrown away with the tree.
    previews: Previews,
    /// Where the preview pane sits, or `None` while it is hidden — which is how
    /// the tree opens, so a reader who never asks for a preview keeps every row
    /// the screen can hold.
    preview: Option<PreviewLayout>,
}

/// A pending deletion. Held rather than acted on immediately so the question
/// can be shown and answered.
#[derive(Debug)]
struct Confirm {
    message: String,
    path: PathBuf,
    /// Directories are removed with their contents; files never need this.
    recursive: bool,
}

/// What the open prompt is collecting a name for.
#[derive(Debug, Clone)]
enum Pending {
    CreateFile,
    CreateDirectory,
    /// Renaming the entry at this path, which is remembered because the
    /// selection may have moved by the time the name is submitted.
    Rename(PathBuf),
}

impl FileTree {
    pub const ID: &'static str = "file-tree";

    pub fn new(root: PathBuf, editor: &Editor) -> std::io::Result<Self> {
        // `file_explorer.hidden` reads as "hide hidden files", so it inverts.
        let show_hidden = !editor.config().file_explorer.hidden;
        let children = model::read_dir(&root, show_hidden)?;
        let mut tree = Tree::new(root, children);

        // Open onto the file being edited rather than at the root, so the tree
        // starts where the reader already is. A scratch buffer, or a file from
        // outside the workspace, leaves it shut.
        if let Some(path) = doc!(editor).path() {
            tree.reveal(path, |directory| {
                model::read_dir(directory, show_hidden).ok()
            });
        }

        Ok(Self {
            tree,
            offset: 0,
            height: 0,
            show_hidden,
            prompt: None,
            confirm: None,
            previews: Previews::new(),
            preview: None,
        })
    }
}

impl FileTree {
    /// Rows a page jump covers: half a screen, matching Helix's `C-d`/`C-u`.
    fn page(&self) -> isize {
        (self.height / 2).max(1) as isize
    }

    /// Starts collecting a name for `pending`.
    fn ask(&mut self, pending: Pending, editor: &Editor) {
        let (label, line) = match &pending {
            Pending::CreateFile => ("create file: ", String::new()),
            Pending::CreateDirectory => ("create directory: ", String::new()),
            // Pre-filled so a rename is an edit of the current name rather than
            // typing it out again.
            Pending::Rename(path) => ("rename to: ", file_name(path)),
        };

        let mut prompt = Prompt::new(label.into(), None, completers::none, |_, _, _| {});
        if !line.is_empty() {
            prompt.set_line(line, editor);
        }

        self.prompt = Some((pending, prompt));
    }

    /// Asks before deleting the selected row.
    fn ask_delete(&mut self) {
        let Some(entry) = self.tree.selected_entry() else {
            return;
        };

        let name = file_name(&entry.path);
        let message = if entry.is_dir {
            // Say plainly that this takes the contents too — the row on screen
            // may well be collapsed, hiding everything about to be destroyed.
            format!("delete {name}/ and everything in it? (y/N) ")
        } else {
            format!("delete {name}? (y/N) ")
        };

        self.confirm = Some(Confirm {
            message,
            path: entry.path.clone(),
            recursive: entry.is_dir,
        });
    }

    /// Only `y` goes through. Every other key cancels, rather than only `Esc`:
    /// a keystroke landing here by accident must not be able to delete a file.
    fn handle_confirm_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Consumed(None);
        };

        match *key {
            key!('y') => {
                if let Some(confirm) = self.confirm.take() {
                    self.delete(&confirm.path, confirm.recursive, cx);
                }
            }
            _ => self.confirm = None,
        }

        EventResult::Consumed(None)
    }

    /// Opens a rename prompt for the selected row.
    fn ask_rename(&mut self, editor: &Editor) {
        if let Some(entry) = self.tree.selected_entry() {
            self.ask(Pending::Rename(entry.path.clone()), editor);
        }
    }

    /// While a prompt is open it owns the keyboard, so the tree's own bindings
    /// stay out of the way of typing a filename.
    fn handle_prompt_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let Event::Key(key) = event else {
            return EventResult::Consumed(None);
        };

        match *key {
            key!(Enter) => {
                if let Some((pending, prompt)) = self.prompt.take() {
                    let name = prompt.line().trim().to_string();
                    if !name.is_empty() {
                        match pending {
                            Pending::CreateFile => self.create(&name, false, cx),
                            Pending::CreateDirectory => self.create(&name, true, cx),
                            Pending::Rename(from) => self.rename(&from, &name, cx),
                        }
                    }
                }
            }
            key!(Esc) | ctrl!('c') => self.prompt = None,
            _ => {
                if let Some((_, prompt)) = &mut self.prompt {
                    prompt.handle_event(event, cx);
                }
            }
        }

        EventResult::Consumed(None)
    }

    /// The directory new entries land in, and the row to re-read afterwards.
    /// `None` means the root, which has no row of its own.
    fn target_directory(&self) -> (PathBuf, Option<usize>) {
        let index = self.tree.selected();

        match self.tree.selected_entry() {
            Some(entry) if entry.is_dir => (entry.path.clone(), Some(index)),
            Some(_) => self.containing_directory(index),
            None => (self.tree.root().to_path_buf(), None),
        }
    }

    /// The directory the row at `index` lives in, and the row to re-read after
    /// changing it. `None` means the root, which has no row of its own.
    fn containing_directory(&self, index: usize) -> (PathBuf, Option<usize>) {
        match self.tree.parent_of(index) {
            Some(parent) => (self.tree.get(parent).unwrap().path.clone(), Some(parent)),
            None => (self.tree.root().to_path_buf(), None),
        }
    }

    fn create(&mut self, name: &str, directory_wanted: bool, cx: &mut Context) {
        let (directory, index) = self.target_directory();
        let path = directory.join(name);

        // `Editor::create_path` writes an empty file with `fs::write`, which
        // truncates whatever is already there. Refuse first: typing a name that
        // happens to exist must never silently empty it.
        if path.exists() {
            cx.editor
                .set_error(format!("{} already exists", path.display()));
            return;
        }

        // A name may carry directories of its own — `ui/menu.rs` creates `ui`
        // on the way, which `create_path` handles.
        if let Err(err) = cx.editor.create_path(&path, directory_wanted) {
            cx.editor
                .set_error(format!("Failed to create {}: {err}", path.display()));
            return;
        }

        self.refresh(index, &directory, cx);

        if let Some(index) = self.tree.position(&path) {
            self.tree.select(index);
        }
    }

    /// Renames the entry at `from`, which may also move it: a name containing
    /// separators is joined onto the containing directory.
    ///
    /// Goes through `Editor::move_path` rather than `fs::rename` so that open
    /// buffers follow the file and language servers get `willRename` — which is
    /// what lets a server fix up imports across the workspace.
    fn rename(&mut self, from: &Path, name: &str, cx: &mut Context) {
        let Some(parent) = from.parent() else {
            cx.editor.set_error("Cannot rename the filesystem root");
            return;
        };

        let to = parent.join(name);
        if to == from {
            return;
        }
        if to.exists() {
            cx.editor
                .set_error(format!("{} already exists", to.display()));
            return;
        }

        if let Err(err) = cx.editor.move_path(from, &to) {
            cx.editor
                .set_error(format!("Failed to rename {}: {err}", from.display()));
            return;
        }

        // Re-read from the row the entry lived under, which is unchanged: a
        // rename cannot move something out of its own directory unless the new
        // name says so, and that case re-reads the same parent anyway.
        let index = self
            .tree
            .position(from)
            .map(|index| self.containing_directory(index))
            .unwrap_or_else(|| self.target_directory());

        self.refresh(index.1, &index.0, cx);

        if let Some(index) = self.tree.position(&to) {
            self.tree.select(index);
        }
    }

    /// Removes an entry, then re-reads the directory it lived in.
    ///
    /// Goes through `Editor::delete_path` for the same reason rename goes
    /// through `move_path`: language servers get `willDelete`/`didDelete`, so a
    /// server can react to a file leaving the workspace.
    fn delete(&mut self, path: &Path, recursive: bool, cx: &mut Context) {
        // Work out where to re-read *before* the row disappears.
        let (directory, index) = self
            .tree
            .position(path)
            .map(|index| self.containing_directory(index))
            .unwrap_or_else(|| self.target_directory());

        if let Err(err) = cx.editor.delete_path(path, recursive) {
            cx.editor
                .set_error(format!("Failed to delete {}: {err}", path.display()));
            return;
        }

        self.refresh(index, &directory, cx);
    }

    /// Re-reads a directory from disk after its contents changed.
    fn refresh(&mut self, index: Option<usize>, directory: &Path, cx: &mut Context) {
        let children = match model::read_dir(directory, self.show_hidden) {
            Ok(children) => children,
            Err(err) => {
                cx.editor
                    .set_error(format!("Failed to read {}: {err}", directory.display()));
                return;
            }
        };

        match index {
            Some(index) => {
                self.tree.collapse(index);
                self.tree.expand(index, children);
            }
            None => self.tree.reset(children),
        }
    }

    /// Opens the selected row: directories toggle, files load into the editor
    /// and close the tree.
    fn activate(&mut self, cx: &mut Context) -> EventResult {
        let Some(entry) = self.tree.selected_entry() else {
            return EventResult::Consumed(None);
        };
        let (path, is_dir, expanded) = (entry.path.clone(), entry.is_dir, entry.expanded);
        let index = self.tree.selected();

        if !is_dir {
            return EventResult::Consumed(Some(Box::new(move |compositor, cx: &mut Context| {
                compositor.remove(FileTree::ID);
                if let Err(err) = cx.editor.open(&path, Action::Replace) {
                    cx.editor
                        .set_error(format!("Failed to open {}: {err}", path.display()));
                }
            })));
        }

        if expanded {
            self.tree.collapse(index);
        } else {
            match model::read_dir(&path, self.show_hidden) {
                Ok(children) => self.tree.expand(index, children),
                Err(err) => cx
                    .editor
                    .set_error(format!("Failed to read {}: {err}", path.display())),
            }
        }

        EventResult::Consumed(None)
    }

    /// Shuts the selected directory, or steps out to the one containing it.
    /// Repeated presses walk back up the tree, which is what makes `h` useful
    /// on a file as well as on a directory.
    fn collapse_or_leave(&mut self) {
        let index = self.tree.selected();
        let Some(entry) = self.tree.get(index) else {
            return;
        };

        if entry.is_dir && entry.expanded {
            self.tree.collapse(index);
        } else if let Some(parent) = self.tree.parent_of(index) {
            self.tree.select(parent);
        }
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

impl FileTree {
    /// Draws the rows, the border, and whichever of the prompt or the
    /// confirmation is open.
    fn render_list(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;
        let text_style = theme.get("ui.text");
        let directory_style = theme.get("ui.text.directory");

        let selected_style = theme.get("ui.menu.selected");
        let warning_style = theme.get("warning");

        surface.clear_with(area, theme.get("ui.background"));

        let title = helix_stdx::path::fold_home_dir(self.tree.root())
            .to_string_lossy()
            .into_owned();
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        block.render(area, surface);

        // The prompt and the confirmation share the bottom row; only one can
        // be open at a time.
        let list = if self.prompt.is_some() || self.confirm.is_some() {
            inner.clip_bottom(1)
        } else {
            inner
        };

        self.height = list.height as usize;
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

            let x = list.x + indent as u16;
            let y = list.y + row as u16;
            let width = list.width.saturating_sub(indent as u16) as usize;

            if index == self.tree.selected() {
                // Paint the whole row first so the highlight reads as a bar,
                // then let the text inherit that background.
                surface.set_style(
                    Rect {
                        x: list.x,
                        y,
                        width: list.width,
                        height: 1,
                    },
                    selected_style,
                );
                style = selected_style;
            }

            let icon = if entry.is_dir {
                icons::for_directory(entry.expanded)
            } else {
                icons::for_file(&entry.path)
            };

            surface.set_stringn(x, y, marker, width, style);
            // Only the foreground is overridden, so a selected row keeps its
            // highlight behind the glyph — and the glyph keeps its own colour,
            // the way nvim-tree leaves icons coloured under the cursor.
            surface.set_stringn(
                x + MARKER_WIDTH as u16,
                y,
                icon.glyph,
                ICON_WIDTH,
                style.fg(icon.color),
            );
            surface.set_stringn(
                x + (MARKER_WIDTH + ICON_WIDTH) as u16,
                y,
                &name,
                width.saturating_sub(MARKER_WIDTH + ICON_WIDTH),
                style,
            );
        }

        if let Some(confirm) = &self.confirm {
            let row = prompt_row(inner);
            surface.set_stringn(
                row.x,
                row.y,
                &confirm.message,
                row.width as usize,
                warning_style,
            );
        }

        if let Some((_, prompt)) = &mut self.prompt {
            prompt.render(prompt_row(inner), surface, cx);
        }
    }

    /// Draws the selected file's contents beside or under the list.
    ///
    /// Highlighting arrives a frame or two late — the parse is debounced onto a
    /// background task — so the first look at a file is plain text.
    fn render_preview(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        surface.clear_with(area, cx.editor.theme.get("ui.background"));

        let block = Block::bordered();
        // A column of air on each side, so text never touches the border.
        let inner = block.inner(area).inner(Margin::horizontal(1));
        block.render(area, surface);

        let Some(entry) = self.tree.selected_entry() else {
            return;
        };
        // Cloned to let go of the tree before the cache is borrowed.
        let path = entry.path.clone();

        let preview = self.previews.get(&path, cx.editor);
        let Some(doc) = preview.document() else {
            if let Some(message) = preview.placeholder() {
                let x = inner.x + inner.width.saturating_sub(message.len() as u16) / 2;
                let y = inner.y + inner.height / 2;
                surface.set_stringn(
                    x,
                    y,
                    message,
                    inner.width as usize,
                    cx.editor.theme.get("ui.text"),
                );
            }
            return;
        };

        // Always from the top of the file: the tree picks a file, not a line,
        // so there is nothing to scroll to.
        let offset = ViewPosition::default();
        let loader = cx.editor.syn_loader.load();
        let config = cx.editor.config();

        let syntax_highlighter =
            EditorView::doc_syntax_highlighter(doc, offset.anchor, area.height, &loader);

        let mut overlay_highlights = Vec::new();
        if doc
            .language_config()
            .and_then(|config| config.rainbow_brackets)
            .unwrap_or(config.rainbow_brackets)
        {
            if let Some(overlay) = EditorView::doc_rainbow_highlights(
                doc,
                offset.anchor,
                area.height,
                &cx.editor.theme,
                &loader,
            ) {
                overlay_highlights.push(overlay);
            }
        }
        EditorView::doc_diagnostics_highlights_into(doc, &cx.editor.theme, &mut overlay_highlights);

        render_document(
            surface,
            inner,
            doc,
            offset,
            &TextAnnotations::default(),
            syntax_highlighter,
            overlay_highlights,
            &cx.editor.theme,
            DecorationManager::default(),
        );
    }
}

impl Component for FileTree {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +------------+ +------------+      +---------------------+
        // |tree        | |preview     |      |tree                 |
        // |            | |            |      +---------------------+
        // |            | |            |      |preview              |
        // +------------+ +------------+      +---------------------+
        //   editor.file-tree-preview            editor.file-tree-preview
        //          = "right"                            = "bottom"
        let (list, preview) = match self.preview {
            Some(layout) => layout.split(area),
            None => (area, None),
        };

        self.render_list(list, surface, cx);

        if let Some(preview) = preview {
            self.render_preview(preview, surface, cx);
        }
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if self.confirm.is_some() {
            return self.handle_confirm_event(event, cx);
        }

        if self.prompt.is_some() {
            return self.handle_prompt_event(event, cx);
        }

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
            key!('o') | key!('l') | key!(Enter) | key!(Right) => return self.activate(cx),
            key!('h') | key!(Left) => self.collapse_or_leave(),
            key!('a') => self.ask(Pending::CreateFile, cx.editor),
            shift!('A') => self.ask(Pending::CreateDirectory, cx.editor),
            key!('r') => self.ask_rename(cx.editor),
            key!('d') => self.ask_delete(),
            // Modal: swallow everything else rather than let it reach the
            // buffer underneath.
            _ => {}
        }

        EventResult::Consumed(None)
    }

    /// Answering here keeps the editor's cursor from showing through; the
    /// compositor stops at the first layer that returns `Some`.
    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        if let Some((_, prompt)) = &self.prompt {
            // Show where typing lands, rather than the hidden cursor below.
            return prompt.cursor(prompt_row(Block::bordered().inner(area)), editor);
        }

        (Some(Position::default()), CursorKind::Hidden)
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}

/// The final component of a path, for display and for pre-filling a rename.
fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// The bottom row of the tree's inner area, where the prompt is drawn.
fn prompt_row(inner: Rect) -> Rect {
    Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
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

    // The tree is built inside the callback rather than out here: it owns a
    // `Prompt`, which is not `Send`, so it cannot cross the job boundary. The
    // upside is that the directory is read at the moment the layer opens.
    cx.jobs.callback(async move {
        let call: job::Callback = job::Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| match FileTree::new(
                root, editor,
            ) {
                Ok(tree) => compositor.push(Box::new(overlaid(tree))),
                Err(err) => editor.set_error(format!("Failed to open file tree: {err}")),
            },
        ));
        Ok(call)
    });

    Ok(())
}
