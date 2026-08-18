//! A modal file tree — a fork-local customization, not part of upstream Helix.
//!
//! Opened with `:file-tree`, it takes over the screen the way a picker does,
//! and closes on `Esc`. Everything lives under `helix-term/src/file_tree/` so
//! that merging upstream stays cheap; see FORK.md at the repository root.

pub mod model;
