//! The document behind the preview pane: reading it, keeping it, and getting
//! it highlighted without stalling the editor.
//!
//! The picker does all of this already, and none of it can be borrowed: its
//! cache is a field of `Picker<T, D>`, and the handler that fills in syntax
//! highlighting finds its way back to that cache through
//! `compositor.find::<Overlay<Picker<T, D>>>()` — the picker is named in the
//! type. What *is* shared is everything underneath: `Document::open`, the size
//! limit, and the binary sniff.

use std::{
    collections::HashMap,
    io::{self, Read},
    path::Path,
    sync::Arc,
    time::Duration,
};

use helix_event::AsyncHook;
use helix_view::{Document, Editor};
use tokio::{sync::mpsc::Sender, time::Instant};

use crate::{job, ui::overlay::Overlay, ui::picker::MAX_FILE_SIZE_FOR_PREVIEW};

use super::FileTree;

/// How long the cursor has to rest on a row before its file is parsed. Holding
/// `j` down should not queue a parse per row; the picker debounces by the same
/// amount for the same reason.
const HIGHLIGHT_DEBOUNCE: Duration = Duration::from_millis(150);

/// Bytes read to decide whether a file is binary, as the picker does.
const SNIFF_BYTES: u64 = 1024;

/// What a path turned out to hold, once the cache has looked.
pub enum Cached {
    Document(Box<Document>),
    /// Contents are shown by expanding the row, so the pane stays empty.
    Directory,
    Binary,
    TooLarge,
    Unreadable,
}

/// A preview ready to be drawn.
///
/// Kept apart from [`Cached`] because a file open in the editor is borrowed
/// from there rather than cached — a preview of a buffer with unsaved edits
/// should show the edits, not what is still on disk.
pub enum Preview<'cache, 'editor> {
    Cached(&'cache Cached),
    Open(&'editor Document),
}

impl Preview<'_, '_> {
    pub fn document(&self) -> Option<&Document> {
        match self {
            Self::Open(doc) => Some(doc),
            Self::Cached(Cached::Document(doc)) => Some(doc),
            _ => None,
        }
    }

    /// What to say in place of the file's text, or `None` where the pane is
    /// better left blank.
    pub fn placeholder(&self) -> Option<&'static str> {
        match self {
            Self::Open(_) | Self::Cached(Cached::Document(_)) | Self::Cached(Cached::Directory) => {
                None
            }
            Self::Cached(Cached::Binary) => Some("<binary file>"),
            Self::Cached(Cached::TooLarge) => Some("<file too large to preview>"),
            Self::Cached(Cached::Unreadable) => Some("<cannot be read>"),
        }
    }
}

/// Every file the cursor has rested on, so that walking back up the tree costs
/// nothing. Dropped with the tree, which is what bounds its size.
pub struct Previews {
    entries: HashMap<Arc<Path>, Cached>,
    /// Reused by the binary sniff, so scrolling allocates nothing.
    buffer: Vec<u8>,
    highlight: Sender<Arc<Path>>,
}

impl Default for Previews {
    fn default() -> Self {
        Self::new()
    }
}

impl Previews {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            buffer: Vec::with_capacity(SNIFF_BYTES as usize),
            highlight: HighlightHandler::default().spawn(),
        }
    }

    /// The preview for `path`, read from disk the first time it is asked for.
    ///
    /// Highlighting is not waited for: the text is returned unhighlighted and a
    /// parse is queued, which arrives on a later frame.
    pub fn get<'cache, 'editor>(
        &'cache mut self,
        path: &Path,
        editor: &'editor Editor,
    ) -> Preview<'cache, 'editor> {
        if let Some(doc) = editor.document_by_path(path) {
            return Preview::Open(doc);
        }

        if !self.entries.contains_key(path) {
            let cached = self.read(path, editor);
            self.entries.insert(path.into(), cached);
        }

        // Looked up by key rather than indexed, to get hold of the `Arc<Path>`
        // the handler needs to find this entry again.
        let (path, cached) = self
            .entries
            .get_key_value(path)
            .expect("the entry was just inserted");

        if matches!(cached, Cached::Document(doc) if doc.syntax().is_none()) {
            helix_event::send_blocking(&self.highlight, path.clone());
        }

        Preview::Cached(cached)
    }

    /// Forgets what was read for `path`, so the next look goes back to disk.
    /// The tree's own create, rename and delete all invalidate a preview.
    pub fn forget(&mut self, path: &Path) {
        self.entries.remove(path);
    }

    fn read(&mut self, path: &Path, editor: &Editor) -> Cached {
        match classify(path, &mut self.buffer) {
            Ok(Kind::Directory) => Cached::Directory,
            Ok(Kind::Binary) => Cached::Binary,
            Ok(Kind::TooLarge) => Cached::TooLarge,
            Ok(Kind::Text) => {
                // `detect_language` is left off and the language set by hand:
                // it is the language config that says whether there is anything
                // to highlight at all, and the parse itself waits for the
                // handler.
                let Ok(mut doc) = Document::open(
                    path,
                    None,
                    false,
                    editor.config.clone(),
                    editor.syn_loader.clone(),
                ) else {
                    return Cached::Unreadable;
                };

                let loader = editor.syn_loader.load();
                doc.language = doc.detect_language_config(&loader);

                Cached::Document(Box::new(doc))
            }
            Err(_) => Cached::Unreadable,
        }
    }

    /// The cached document for `path`, if it is still there and still unparsed.
    fn awaiting_highlight(&mut self, path: &Path) -> Option<&mut Document> {
        match self.entries.get_mut(path) {
            Some(Cached::Document(doc)) if doc.syntax().is_none() => Some(doc),
            _ => None,
        }
    }
}

/// What a path can show, decided before any of it is read into memory.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Directory,
    Binary,
    TooLarge,
}

/// Classifies `path` cheaply: a stat, and at most [`SNIFF_BYTES`] of reading.
///
/// `buffer` is scratch space for the sniff, left empty on return.
fn classify(path: &Path, buffer: &mut Vec<u8>) -> io::Result<Kind> {
    let metadata = std::fs::metadata(path)?;

    if metadata.is_dir() {
        return Ok(Kind::Directory);
    }

    // A fifo or a device: reading it could block until the end of time.
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }

    if metadata.len() > MAX_FILE_SIZE_FOR_PREVIEW {
        return Ok(Kind::TooLarge);
    }

    let read = std::fs::File::open(path)?
        .take(SNIFF_BYTES)
        .read_to_end(buffer)?;
    let binary = crate::is_binary(&buffer[..read]);
    buffer.clear();

    Ok(if binary { Kind::Binary } else { Kind::Text })
}

/// Parses a cached preview off the main thread, then hands the syntax tree
/// back.
///
/// A copy of the picker's handler rather than a use of it: that one reaches its
/// cache through the picker's own type, so it cannot be pointed at anything
/// else.
#[derive(Default)]
pub struct HighlightHandler {
    trigger: Option<Arc<Path>>,
}

impl AsyncHook for HighlightHandler {
    type Event = Arc<Path>;

    fn handle_event(&mut self, path: Self::Event, timeout: Option<Instant>) -> Option<Instant> {
        if self.trigger.as_ref().is_some_and(|trigger| *trigger == path) {
            // Still the same row: let the wait run down rather than restarting
            // it, or a repeating render would hold the parse off forever.
            timeout
        } else {
            self.trigger = Some(path);
            Some(Instant::now() + HIGHLIGHT_DEBOUNCE)
        }
    }

    fn finish_debounce(&mut self) {
        let Some(path) = self.trigger.take() else {
            return;
        };

        job::dispatch_blocking(move |editor, compositor| {
            let Some(Overlay { content: tree, .. }) = compositor.find::<Overlay<FileTree>>() else {
                return;
            };
            let Some(doc) = tree.previews.awaiting_highlight(&path) else {
                return;
            };
            let Some(language) = doc.language_config().map(|config| config.language()) else {
                return;
            };

            let loader = editor.syn_loader.load();
            let text = doc.text().clone();

            tokio::task::spawn_blocking(move || {
                let syntax = match helix_core::Syntax::new(text.slice(..), language, &loader) {
                    Ok(syntax) => syntax,
                    Err(err) => {
                        log::info!("highlighting file tree preview failed: {err}");
                        return;
                    }
                };

                job::dispatch_blocking(move |editor, compositor| {
                    let Some(Overlay { content: tree, .. }) =
                        compositor.find::<Overlay<FileTree>>()
                    else {
                        log::info!("file tree closed before syntax highlighting finished");
                        return;
                    };
                    let Some(doc) = tree.previews.awaiting_highlight(&path) else {
                        return;
                    };

                    let diagnostics = helix_view::Editor::doc_diagnostics(
                        &editor.language_servers,
                        &editor.diagnostics,
                        doc,
                    );
                    doc.replace_diagnostics(diagnostics, &[], None);
                    doc.syntax = Some(syntax);
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_path(path: &Path) -> io::Result<Kind> {
        let mut buffer = Vec::new();
        let kind = classify(path, &mut buffer);
        assert!(buffer.is_empty(), "the scratch buffer is handed back empty");
        kind
    }

    #[test]
    fn a_text_file_is_previewable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();

        assert_eq!(classify_path(&path).unwrap(), Kind::Text);
    }

    #[test]
    fn an_empty_file_is_previewable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("empty");
        std::fs::File::create(&path).unwrap();

        assert_eq!(
            classify_path(&path).unwrap(),
            Kind::Text,
            "nothing to sniff is not the same as binary"
        );
    }

    #[test]
    fn a_file_with_nul_bytes_is_binary() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.out");
        std::fs::write(&path, [0x7f, b'E', b'L', b'F', 0x00, 0x01]).unwrap();

        assert_eq!(classify_path(&path).unwrap(), Kind::Binary);
    }

    #[test]
    fn a_huge_file_is_refused_without_reading_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("huge.log");
        let file = std::fs::File::create(&path).unwrap();
        // Sparse, so the test costs no disk: only the length is looked at.
        file.set_len(MAX_FILE_SIZE_FOR_PREVIEW + 1).unwrap();

        assert_eq!(classify_path(&path).unwrap(), Kind::TooLarge);
    }

    #[test]
    fn a_directory_is_left_to_the_tree() {
        let root = tempfile::tempdir().unwrap();

        assert_eq!(classify_path(root.path()).unwrap(), Kind::Directory);
    }

    #[test]
    fn a_missing_path_is_an_error() {
        let root = tempfile::tempdir().unwrap();

        assert!(classify_path(&root.path().join("gone.rs")).is_err());
    }
}
