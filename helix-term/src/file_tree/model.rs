//! The tree's data model: which rows exist, in what order, at what depth.
//!
//! Deliberately free of rendering and of the editor. The one exception is
//! [`read_dir`], which is the only piece that touches the filesystem, so the
//! rest can be tested as plain data.
//!
//! Rows are kept **flattened in display order** rather than as a nested tree.
//! Expanding splices a directory's children in behind their parent; collapsing
//! removes the run of deeper rows that follows it. Rendering and cursor
//! movement then need no traversal at all — they index straight into the list.

use std::io;
use std::path::{Path, PathBuf};

/// One visible row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub is_dir: bool,
    /// Nesting level; the root's children are at 0.
    pub depth: usize,
    /// Always false for files.
    pub expanded: bool,
}

impl Entry {
    fn new(path: PathBuf, is_dir: bool, depth: usize) -> Self {
        Self {
            path,
            is_dir,
            depth,
            expanded: false,
        }
    }

    /// The text shown for this row, without indentation.
    pub fn name(&self) -> std::borrow::Cow<'_, str> {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
    }
}

#[derive(Debug)]
pub struct Tree {
    root: PathBuf,
    entries: Vec<Entry>,
}

impl Tree {
    /// Builds a tree whose visible rows are `children`, the contents of `root`.
    pub fn new(root: PathBuf, children: Vec<(PathBuf, bool)>) -> Self {
        let entries = children
            .into_iter()
            .map(|(path, is_dir)| Entry::new(path, is_dir, 0))
            .collect();

        Self { root, entries }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Splices `children` in behind the directory at `index`.
    ///
    /// A no-op for files and for directories that are already open, so callers
    /// can expand without checking first. Reading the directory is the caller's
    /// job — see [`read_dir`] — which keeps the failure path out of the model.
    pub fn expand(&mut self, index: usize, children: Vec<(PathBuf, bool)>) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if !entry.is_dir || entry.expanded {
            return;
        }

        entry.expanded = true;
        let depth = entry.depth + 1;

        self.entries.splice(
            index + 1..index + 1,
            children
                .into_iter()
                .map(|(path, is_dir)| Entry::new(path, is_dir, depth)),
        );
    }

    /// Removes the rows nested under the directory at `index`.
    ///
    /// The whole subtree goes, however deep, so the children of a directory
    /// collapsed while its own children were open come back collapsed.
    pub fn collapse(&mut self, index: usize) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if !entry.expanded {
            return;
        }

        entry.expanded = false;
        let depth = entry.depth;

        let end = self.entries[index + 1..]
            .iter()
            .position(|entry| entry.depth <= depth)
            .map(|offset| index + 1 + offset)
            .unwrap_or(self.entries.len());

        self.entries.drain(index + 1..end);
    }

    /// The index of the row holding `path`, if it is currently visible.
    pub fn position(&self, path: &Path) -> Option<usize> {
        self.entries.iter().position(|entry| entry.path == path)
    }
}

/// Reads the immediate children of `path`, ordered for display: directories
/// first, then files, each case-insensitively alphabetical.
///
/// Entries that cannot be stat'd are skipped rather than failing the read — a
/// broken symlink in a directory should not stop the directory from opening.
pub fn read_dir(path: &Path, show_hidden: bool) -> io::Result<Vec<(PathBuf, bool)>> {
    let mut entries: Vec<(PathBuf, bool)> = std::fs::read_dir(path)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();

            if !show_hidden && is_hidden(&path) {
                return None;
            }

            Some((path, entry.file_type().ok()?.is_dir()))
        })
        .collect();

    entries.sort_by(|(a, a_is_dir), (b, b_is_dir)| {
        b_is_dir.cmp(a_is_dir).then_with(|| {
            let (a, b) = (sort_key(a), sort_key(b));
            a.cmp(&b)
        })
    });

    Ok(entries)
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn sort_key(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> (PathBuf, bool) {
        (PathBuf::from(name), true)
    }

    fn file(name: &str) -> (PathBuf, bool) {
        (PathBuf::from(name), false)
    }

    fn shape(tree: &Tree) -> Vec<(String, usize)> {
        tree.entries()
            .iter()
            .map(|entry| (entry.name().into_owned(), entry.depth))
            .collect()
    }

    #[test]
    fn new_lists_children_at_depth_zero() {
        let tree = Tree::new(PathBuf::from("/root"), vec![dir("src"), file("a.rs")]);

        assert_eq!(
            shape(&tree),
            [("src".into(), 0), ("a.rs".into(), 0)],
            "root children sit at depth 0 in the order given"
        );
        assert_eq!(tree.root(), Path::new("/root"));
    }

    #[test]
    fn expand_splices_children_behind_their_parent() {
        let mut tree = Tree::new(PathBuf::from("/root"), vec![dir("src"), file("a.rs")]);
        tree.expand(0, vec![file("main.rs")]);

        assert_eq!(
            shape(&tree),
            [("src".into(), 0), ("main.rs".into(), 1), ("a.rs".into(), 0)],
            "children land between the parent and its next sibling"
        );
        assert!(tree.get(0).unwrap().expanded);
    }

    #[test]
    fn collapse_removes_the_whole_subtree() {
        let mut tree = Tree::new(PathBuf::from("/root"), vec![dir("src"), file("a.rs")]);
        tree.expand(0, vec![dir("ui"), file("main.rs")]);
        tree.expand(1, vec![file("menu.rs")]);
        assert_eq!(tree.len(), 5);

        tree.collapse(0);

        assert_eq!(
            shape(&tree),
            [("src".into(), 0), ("a.rs".into(), 0)],
            "nested rows go too, not just the immediate children"
        );
        assert!(!tree.get(0).unwrap().expanded);
    }

    #[test]
    fn collapsing_forgets_that_children_were_open() {
        let mut tree = Tree::new(PathBuf::from("/root"), vec![dir("src")]);
        tree.expand(0, vec![dir("ui")]);
        tree.expand(1, vec![file("menu.rs")]);

        tree.collapse(0);
        tree.expand(0, vec![dir("ui")]);

        assert_eq!(
            shape(&tree),
            [("src".into(), 0), ("ui".into(), 1)],
            "re-expanding shows `ui` shut, not restored to its old state"
        );
    }

    #[test]
    fn expanding_a_file_or_an_open_directory_does_nothing() {
        let mut tree = Tree::new(PathBuf::from("/root"), vec![dir("src"), file("a.rs")]);

        tree.expand(1, vec![file("nope.rs")]);
        assert_eq!(tree.len(), 2, "files have no children to splice in");

        tree.expand(0, vec![file("main.rs")]);
        tree.expand(0, vec![file("again.rs")]);
        assert_eq!(tree.len(), 3, "the second expand of `src` is ignored");
    }

    #[test]
    fn collapsing_a_leaf_or_a_shut_directory_does_nothing() {
        let mut tree = Tree::new(PathBuf::from("/root"), vec![dir("src"), file("a.rs")]);

        tree.collapse(0);
        tree.collapse(1);
        tree.collapse(99);

        assert_eq!(shape(&tree), [("src".into(), 0), ("a.rs".into(), 0)]);
    }

    #[test]
    fn collapse_stops_at_the_next_sibling() {
        let mut tree = Tree::new(PathBuf::from("/root"), vec![dir("a"), dir("b")]);
        tree.expand(1, vec![file("b1.rs")]);
        tree.expand(0, vec![file("a1.rs")]);

        tree.collapse(0);

        assert_eq!(
            shape(&tree),
            [("a".into(), 0), ("b".into(), 0), ("b1.rs".into(), 1)],
            "`b`'s open children survive `a` collapsing"
        );
    }

    #[test]
    fn position_finds_visible_rows_only() {
        let mut tree = Tree::new(PathBuf::from("/root"), vec![dir("src")]);
        tree.expand(0, vec![file("main.rs")]);

        assert_eq!(tree.position(Path::new("main.rs")), Some(1));

        tree.collapse(0);
        assert_eq!(tree.position(Path::new("main.rs")), None);
    }

    #[test]
    fn read_dir_puts_directories_first_then_sorts_by_name() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("zebra")).unwrap();
        std::fs::create_dir(root.path().join("Alpha")).unwrap();
        std::fs::File::create(root.path().join("b.rs")).unwrap();
        std::fs::File::create(root.path().join("A.rs")).unwrap();

        let names: Vec<_> = read_dir(root.path(), false)
            .unwrap()
            .into_iter()
            .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            names,
            ["Alpha", "zebra", "A.rs", "b.rs"],
            "directories first, and the sort ignores case"
        );
    }

    #[test]
    fn read_dir_hides_dotfiles_unless_asked() {
        let root = tempfile::tempdir().unwrap();
        std::fs::File::create(root.path().join(".env")).unwrap();
        std::fs::File::create(root.path().join("visible.rs")).unwrap();

        let shown = read_dir(root.path(), false).unwrap();
        assert_eq!(shown.len(), 1);

        let all = read_dir(root.path(), true).unwrap();
        assert_eq!(all.len(), 2);
    }
}
