//! The app's icons: small stroked SVGs from the design, compiled in.
//!
//! GPUI draws an SVG as a mask filled with the element's text color, so the
//! stroke color inside each document does not matter; only its shape does.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

/// An icon, by the asset path GPUI's `svg()` loads it from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// The app's mark: a double chevron pointing back.
    Mark,
    GoToStart,
    ChevronLeft,
    ChevronRight,
    Fork,
    Close,
}

impl Icon {
    const ALL: [Icon; 6] = [
        Icon::Mark,
        Icon::GoToStart,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::Fork,
        Icon::Close,
    ];

    pub fn path(self) -> &'static str {
        match self {
            Icon::Mark => "icons/mark.svg",
            Icon::GoToStart => "icons/go-to-start.svg",
            Icon::ChevronLeft => "icons/chevron-left.svg",
            Icon::ChevronRight => "icons/chevron-right.svg",
            Icon::Fork => "icons/fork.svg",
            Icon::Close => "icons/close.svg",
        }
    }

    /// The icon's stroke width and shapes, on the design's 24 pixel grid.
    fn shapes(self) -> (&'static str, &'static str) {
        match self {
            Icon::Mark => (
                "2.2",
                r#"<path d="M11 19l-7-7 7-7"/><path d="M20 19l-7-7 7-7"/>"#,
            ),
            Icon::GoToStart => ("2", r#"<path d="M6 5v14"/><path d="M18 5l-9 7 9 7z"/>"#),
            Icon::ChevronLeft => ("2.2", r#"<path d="M15 18l-6-6 6-6"/>"#),
            Icon::ChevronRight => ("2.2", r#"<path d="M9 18l6-6-6-6"/>"#),
            Icon::Fork => (
                "2.2",
                r#"<circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="8" r="2"/><path d="M6 7v10"/><path d="M18 10c0 4-6 3-12 7"/>"#,
            ),
            Icon::Close => ("2", r#"<path d="M6 6l12 12"/><path d="M18 6L6 18"/>"#),
        }
    }

    /// The icon as a complete SVG document.
    fn document(self) -> String {
        let (stroke, shapes) = self.shapes();
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="{stroke}" stroke-linecap="round" stroke-linejoin="round">{shapes}</svg>"##
        )
    }
}

/// The icons as GPUI assets.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let icon = Icon::ALL.into_iter().find(|i| i.path() == path);
        Ok(icon.map(|i| Cow::Owned(i.document().into_bytes())))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Icon::ALL
            .into_iter()
            .map(Icon::path)
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
