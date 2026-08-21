//! Welcome screen - a fork local customization, not part of upstream Helix.
//!
//! Everything lives here so upstream merges stay cheap. The rest of the tree is
//! touched in exactly two places. both marked with "fork" sentinel comments:
//! 1. helix-term/src/lib.rs            -- `pub mod welcome;`.
//! 2. helix-term/src/application.rs    -- `welcome::layer()` at startup.
//!
//! `layer` is the only entry point, so growing this feature never widens that
//! footprint. See FORK.md at the repository root.

use helix_core::{unicode::width::UnicodeWidthStr, Position};
use helix_view::{
    editor::Action as EditorAction,
    graphics::{CursorKind, Modifier, Rect},
    input::KeyEvent,
    keyboard::{KeyCode, KeyModifiers},
    Editor,
};
use tui::buffer::Buffer as Surface;

mod config;

pub use config::Config;

use crate::{
    args::Args,
    compositor::{Component, Compositor, Context, Event, EventResult},
    ctrl, key,
    ui::{self, overlay::overlaid},
};

use std::io::{stdin, IsTerminal};
use std::path::PathBuf;

/// Drawn above the menu unless `welcome.toml` overrides it. Lines are centered
/// individually, so they don't have to be the same length.
const BANNER: &[&str] = &[
    "██╗  ██╗███████╗██╗     ██╗██╗  ██╗",
    "██║  ██║██╔════╝██║     ██║╚██╗██╔╝",
    "███████║█████╗  ██║     ██║ ╚███╔╝ ",
    "██╔══██║██╔══╝  ██║     ██║ ██╔██╗ ",
    "██║  ██║███████╗███████╗██║██╔╝ ██╗",
    "╚═╝  ╚═╝╚══════╝╚══════╝╚═╝╚═╝  ╚═╝",
];

const ITEMS: &[Item] = &[
    Item {
        key: 'f',
        label: "Find file",
        action: Action::FindFile,
    },
    Item {
        key: 'e',
        label: "File explorer",
        action: Action::FileExplorer,
    },
    Item {
        key: 'n',
        label: "New file",
        action: Action::NewFile,
    },
    Item {
        key: 'c',
        label: "Open config",
        action: Action::OpenConfig,
    },
    Item {
        key: 't',
        label: "Tutor",
        action: Action::Tutor,
    },
    Item {
        key: 'q',
        label: "Quit",
        action: Action::Quit,
    },
];

/// Marker on the selected row, two columns wide including the trailing space.
const MARKER: &str = "▸ ";
/// Columns from the left edge of the menu to the label: marker (2), hotkey (1),
/// gap (2).
const LABEL_OFFSET: u16 = 5;
/// Blank rows between banner, menu and footer.
const GAP: u16 = 2;
/// Rows taken by the footer.
const FOOTER_HEIGHT: u16 = 1;

struct Item {
    key: char,
    label: &'static str,
    action: Action,
}

/// What a menu entry does once chosen. Every variant runs as a compositor
/// callback, after the welcome layer has removed itself.
#[derive(Clone, Copy)]
enum Action {
    FindFile,
    FileExplorer,
    NewFile,
    OpenConfig,
    Tutor,
    Quit,
}

impl Action {
    /// Pickers are pushed *over* the welcome screen so that dismissing one
    /// comes back here instead of dropping onto the scratch buffer. Everything
    /// else replaces the screen outright.
    fn keeps_screen(self) -> bool {
        matches!(self, Self::FindFile | Self::FileExplorer)
    }

    fn run(self, compositor: &mut Compositor, cx: &mut Context) {
        match self {
            Self::FindFile => {
                let Some(root) = workspace_root(cx) else {
                    return;
                };
                compositor.push(Box::new(overlaid(ui::file_picker(cx.editor, root))));
            }
            Self::FileExplorer => {
                let Some(root) = workspace_root(cx) else {
                    return;
                };
                match ui::file_explorer(root, cx.editor) {
                    Ok(explorer) => compositor.push(Box::new(overlaid(explorer))),
                    Err(err) => cx
                        .editor
                        .set_error(format!("Failed to open file explorer: {err}")),
                }
            }
            // The scratch buffer the editor started with is already sitting
            // underneath us, so dismissing the layer is the whole of "new file".
            Self::NewFile => (),
            Self::OpenConfig => {
                if let Err(err) = cx
                    .editor
                    .open(&helix_loader::config_file(), EditorAction::Replace)
                {
                    cx.editor.set_error(format!("Failed to open config: {err}"));
                }
            }
            Self::Tutor => match cx
                .editor
                .open(&helix_loader::runtime_file("tutor"), EditorAction::Replace)
            {
                // Unset the path to prevent accidentally saving to the original
                // tutor file, the same way `Application::new` does.
                Ok(_) => doc_mut!(cx.editor).set_path(None),
                Err(err) => cx.editor.set_error(format!("Failed to open tutor: {err}")),
            },
            // Emptying the view tree is what `Editor::should_close` polls for.
            // Nothing can be unsaved at this point: any key that could edit a
            // buffer dismisses this layer before the editor ever sees it.
            Self::Quit => {
                let views: Vec<_> = cx.editor.tree.views().map(|(view, _)| view.id).collect();
                for view in views {
                    cx.editor.close(view);
                }
            }
        }
    }
}

fn workspace_root(cx: &mut Context) -> Option<PathBuf> {
    let root = helix_loader::find_workspace().0;
    if !root.exists() {
        cx.editor.set_error("Workspace directory does not exist");
        return None;
    }
    Some(root)
}

/// The welcome screen layer for this invocation, if there should be one.
///
/// A bare `hx` qualifies, and so does a lone directory like `hx .`: naming a
/// directory asks for a place to start, which is what this screen is for. Named
/// files, `--tutor`, or piped stdin all mean the user already said what they
/// want to look at. Integration tests drive the compositor directly, so keep the
/// layer out of their way too.
///
/// A directory argument is dropped once we decide to show the screen. `main`
/// has already made it the working directory by this point, and dropping it
/// stops `Application::new` from stacking its own file picker on top of us --
/// the menu offers "Find file" already.
pub fn layer(args: &mut Args) -> Option<Box<dyn Component>> {
    // Paths are canonicalized during parsing, so the `.` the user typed is long
    // gone by now. Ask the filesystem what the argument turned out to be.
    let lone_directory =
        args.files.len() == 1 && args.files.keys().next().is_some_and(|path| path.is_dir());

    let wanted = !cfg!(feature = "integration")
        && (args.files.is_empty() || lone_directory)
        && !args.load_tutor
        && stdin().is_terminal();

    if !wanted {
        return None;
    }

    let config = Config::load();
    if !config.enable {
        return None;
    }

    args.files.clear();

    Some(Box::new(Welcome::new(config)))
}

pub struct Welcome {
    selected: usize,
    banner: Vec<String>,
    footer: String,
}

impl Welcome {
    pub const ID: &'static str = "welcome";

    fn new(config: Config) -> Self {
        Self {
            selected: 0,
            banner: config
                .banner
                .unwrap_or_else(|| BANNER.iter().map(|line| line.to_string()).collect()),
            footer: config
                .footer
                .unwrap_or_else(|| format!("helix {}", helix_loader::VERSION_AND_GIT_HASH)),
        }
    }

    /// Runs `action`, removing the layer first unless the action wants to be
    /// layered on top of it.
    fn activate(action: Action) -> EventResult {
        EventResult::Consumed(Some(Box::new(move |compositor, cx| {
            if !action.keeps_screen() {
                compositor.remove(Welcome::ID);
            }
            action.run(compositor, cx);
        })))
    }

    /// Whether the screen has outlived its purpose.
    ///
    /// A picker left on top of us can open a document at any time, and we get
    /// no say in it. Once the editor has a real file to show, this layer stops
    /// drawing and retires itself on the next event.
    fn is_stale(editor: &Editor) -> bool {
        doc!(editor).path().is_some()
    }

    /// Removes the layer but lets the key through to the editor underneath, so
    /// typing straight into the scratch buffer just works. `Ignored` with a
    /// callback is the same auto-close trick `ui/popup.rs` uses.
    fn dismiss() -> EventResult {
        EventResult::Ignored(Some(Box::new(|compositor, _| {
            compositor.remove(Welcome::ID);
        })))
    }

    fn hotkey(c: char) -> Option<Action> {
        ITEMS
            .iter()
            .find(|item| item.key == c)
            .map(|item| item.action)
    }

    fn width() -> u16 {
        let label = ITEMS
            .iter()
            .map(|item| item.label.width())
            .max()
            .unwrap_or(0) as u16;
        LABEL_OFFSET + label
    }
}

impl Component for Welcome {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        if Self::is_stale(cx.editor) {
            return;
        }

        let theme = &cx.editor.theme;

        // Leave the bottom row alone so the statusline still shows through.
        let area = area.clip_bottom(1);
        if area.width == 0 || area.height == 0 {
            return;
        }
        surface.clear_with(area, theme.get("ui.background"));

        let banner_style = theme.get("keyword");
        let key_style = theme.get("constant");
        let text_style = theme.get("ui.text");
        // Bold as well as coloured: `ui.text.focus` falls back to `ui.text` in
        // themes that don't define it, which would leave the selection invisible.
        let selected_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
        let footer_style = theme.get("comment");

        let banner_width = self
            .banner
            .iter()
            .map(|line| line.width())
            .max()
            .unwrap_or(0) as u16;
        let footer_width = self.footer.width() as u16;
        let menu_width = Self::width();

        // Shed decoration rather than overflow a small terminal: the banner goes
        // first, then the footer. The menu is the point, so it always renders.
        let mut height = ITEMS.len() as u16;
        let show_banner = !self.banner.is_empty()
            && area.width >= banner_width
            && area.height >= height + self.banner.len() as u16 + GAP;
        if show_banner {
            height += self.banner.len() as u16 + GAP;
        }
        let show_footer = !self.footer.is_empty()
            && area.width >= footer_width
            && area.height >= height + GAP + FOOTER_HEIGHT;
        if show_footer {
            height += GAP + FOOTER_HEIGHT;
        }

        let center = |width: u16| area.x + area.width.saturating_sub(width) / 2;
        let mut y = area.y + area.height.saturating_sub(height) / 2;

        if show_banner {
            for line in &self.banner {
                surface.set_stringn(
                    center(line.width() as u16),
                    y,
                    line,
                    area.width as usize,
                    banner_style,
                );
                y += 1;
            }
            y += GAP;
        }

        let x = center(menu_width);
        for (i, item) in ITEMS.iter().enumerate() {
            let selected = i == self.selected;
            let label_style = if selected { selected_style } else { text_style };
            let marker = if selected { MARKER } else { "  " };
            let mut buf = [0; 4];

            surface.set_stringn(x, y, marker, 2, label_style);
            surface.set_stringn(x + 2, y, item.key.encode_utf8(&mut buf), 1, key_style);
            surface.set_stringn(
                x + LABEL_OFFSET,
                y,
                item.label,
                item.label.width(),
                label_style,
            );
            y += 1;
        }

        if show_footer {
            y += GAP;
            surface.set_stringn(
                center(footer_width),
                y,
                &self.footer,
                area.width as usize,
                footer_style,
            );
        }
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if Self::is_stale(cx.editor) {
            return Self::dismiss();
        }

        let Event::Key(key) = event else {
            return EventResult::Ignored(None);
        };

        match *key {
            key!('j') | key!(Down) | ctrl!('n') => {
                self.selected = (self.selected + 1) % ITEMS.len();
                EventResult::Consumed(None)
            }
            key!('k') | key!(Up) | ctrl!('p') => {
                self.selected = self.selected.checked_sub(1).unwrap_or(ITEMS.len() - 1);
                EventResult::Consumed(None)
            }
            key!(Enter) => Self::activate(ITEMS[self.selected].action),
            // A menu hotkey runs its entry; every other bare key dismisses.
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
            } => match Self::hotkey(c) {
                Some(action) => Self::activate(action),
                None => Self::dismiss(),
            },
            _ => Self::dismiss(),
        }
    }

    /// The compositor walks layers front-to-back and stops at the first `Some`.
    /// Answering here is what suppresses the editor's own cursor.
    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        if Self::is_stale(editor) {
            return (None, CursorKind::Hidden);
        }

        (Some(Position::default()), CursorKind::Hidden)
    }

    fn id(&self) -> Option<&'static str> {
        Some(Self::ID)
    }
}
