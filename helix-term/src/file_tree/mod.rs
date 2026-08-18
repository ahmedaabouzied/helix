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

use std::path::{Path, PathBuf};

use helix_core::{command_line::Args, Position};
use helix_view::{
    editor::Action,
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
    ui::{completers, overlay::overlaid, Prompt, PromptEvent},
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
    /// Read once at open: directories opened later use the same rule as the
    /// root listing, so the tree stays internally consistent even if the
    /// setting changes mid-session.
    show_hidden: bool,
    /// An in-progress prompt and what it will do once submitted. Owned by the
    /// tree rather than pushed as its own layer, so that acting on the answer
    /// needs no reaching across the compositor to find this component again.
    prompt: Option<(Pending, Prompt)>,
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

        Ok(Self {
            tree: Tree::new(root, children),
            offset: 0,
            height: 0,
            show_hidden,
            prompt: None,
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

        // A name may carry directories of its own — `ui/menu.rs` creates `ui`
        // on the way. `create_new` then refuses to clobber an existing file.
        let created = if directory_wanted {
            std::fs::create_dir_all(&path)
        } else {
            path.parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::File::create_new(&path).map(|_| ()))
        };

        if let Err(err) = created {
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

        let list = if self.prompt.is_some() {
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

            surface.set_stringn(x, y, marker, width, style);
            surface.set_stringn(
                x + MARKER_WIDTH as u16,
                y,
                &name,
                width.saturating_sub(MARKER_WIDTH),
                style,
            );
        }

        if let Some((_, prompt)) = &mut self.prompt {
            prompt.render(prompt_row(inner), surface, cx);
        }
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
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
            key!('l') | key!(Enter) | key!(Right) => return self.activate(cx),
            key!('h') | key!(Left) => self.collapse_or_leave(),
            key!('a') => self.ask(Pending::CreateFile, cx.editor),
            shift!('A') => self.ask(Pending::CreateDirectory, cx.editor),
            key!('r') => self.ask_rename(cx.editor),
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
