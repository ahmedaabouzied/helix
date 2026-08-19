//! Where a list draws its preview — shared by the picker and the file tree.
//!
//! Upstream always puts the preview beside the list, which halves the width a
//! second time when Helix is itself already in a vertical split — the list and
//! the preview both end up a quarter of the screen wide. Stacking the preview
//! under the list instead keeps the full width for both.

use crate::graphics::Rect;
use serde::{Deserialize, Serialize};

/// Narrower than this, a side-by-side preview leaves neither pane usable.
/// Mirrors `helix_term::ui::picker::MIN_AREA_WIDTH_FOR_PREVIEW`, which cannot be
/// imported here: `helix-view` does not depend on `helix-term`.
const MIN_WIDTH: u16 = 72;
/// Shorter than this, a stacked preview leaves neither pane usable.
const MIN_HEIGHT: u16 = 20;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewLayout {
    /// Beside the list. Upstream's layout for the picker.
    #[default]
    Right,
    /// Under the list.
    Bottom,
}

impl PreviewLayout {
    /// Divides `area` into the list area and, where there is room for one, the
    /// preview area.
    pub fn split(self, area: Rect) -> (Rect, Option<Rect>) {
        match self {
            Self::Right if area.width > MIN_WIDTH => {
                let width = area.width / 2;
                (area.with_width(width), Some(area.clip_left(width)))
            }
            Self::Bottom if area.height > MIN_HEIGHT => {
                let height = area.height / 2;
                (area.with_height(height), Some(area.clip_top(height)))
            }
            // Too cramped to divide: the list gets everything and the caller
            // skips the preview, exactly as upstream does when too narrow.
            _ => (area, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    };

    #[test]
    fn right_splits_the_width() {
        let (list, preview) = PreviewLayout::Right.split(AREA);
        let preview = preview.unwrap();
        assert_eq!((list.width, list.height), (50, 40));
        assert_eq!((preview.x, preview.width, preview.height), (50, 50, 40));
    }

    #[test]
    fn bottom_splits_the_height() {
        let (list, preview) = PreviewLayout::Bottom.split(AREA);
        let preview = preview.unwrap();
        assert_eq!((list.width, list.height), (100, 20));
        assert_eq!((preview.y, preview.width, preview.height), (20, 100, 20));
    }

    #[test]
    fn no_preview_when_there_is_no_room() {
        let narrow = Rect {
            width: MIN_WIDTH,
            ..AREA
        };
        assert_eq!(PreviewLayout::Right.split(narrow), (narrow, None));

        let short = Rect {
            height: MIN_HEIGHT,
            ..AREA
        };
        assert_eq!(PreviewLayout::Bottom.split(short), (short, None));
    }

    #[test]
    fn each_layout_only_minds_its_own_dimension() {
        // A short-but-wide area still gets a side-by-side preview, and a
        // narrow-but-tall one still gets a stacked preview.
        let short = Rect { height: 4, ..AREA };
        assert!(PreviewLayout::Right.split(short).1.is_some());

        let narrow = Rect { width: 20, ..AREA };
        assert!(PreviewLayout::Bottom.split(narrow).1.is_some());
    }
}
