//! Small building blocks shared by the scrubber's parts: buttons, pills,
//! panel titles and icons, styled after the design.

use gpui::{
    AnyElement, Div, ElementId, FontWeight, Role, SharedString, Stateful, Svg, div, prelude::*, px,
    rgb, svg,
};

use crate::theme::{self, layout, size};
use crate::ui::icons::Icon;

/// The fonts the app found on this machine.
#[derive(Clone, Debug)]
pub struct Fonts {
    pub ui: SharedString,
    pub mono: SharedString,
}

impl Fonts {
    /// The first family of each preference list the system has. A machine
    /// with none of them gets the first name anyway, and GPUI's own
    /// fallback picks a font for it.
    pub fn resolve(available: &[String]) -> Fonts {
        let pick = |wanted: &[&str]| -> SharedString {
            let found = wanted
                .iter()
                .find(|w| available.iter().any(|a| a == *w))
                .unwrap_or(&wanted[0]);
            SharedString::from(found.to_string())
        };
        Fonts {
            ui: pick(theme::UI_FONTS),
            mono: pick(theme::MONO_FONTS),
        }
    }
}

/// How a button looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonStyle {
    /// Grey, for moving around.
    Neutral,
    /// Blue, for the divergence.
    Divergence,
    /// Red, for the failure.
    Failure,
    /// Amber and filled, for the one action that matters most.
    Primary,
}

/// Whether a button can be pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Enabled,
    Disabled,
}

/// A button's frame: height, padding, border and colors for its style.
/// Callers add the content and the click handler.
pub fn button(
    id: impl Into<ElementId>,
    style: ButtonStyle,
    availability: Availability,
) -> Stateful<Div> {
    let (bg, fg, border, hover) = match style {
        ButtonStyle::Neutral => (
            theme::RAISED,
            theme::TEXT,
            theme::LINE_2,
            theme::RAISED_HOVER,
        ),
        ButtonStyle::Divergence => (
            theme::BLUE_CARD,
            theme::BLUE_SOFT,
            theme::BLUE_BORDER,
            theme::BLUE_PILL,
        ),
        ButtonStyle::Failure => (
            theme::RED_CARD,
            theme::RED_SOFT,
            theme::RED_BORDER,
            theme::RED_PILL,
        ),
        ButtonStyle::Primary => (
            theme::AMBER,
            theme::AMBER_INK,
            theme::AMBER,
            theme::AMBER_HI,
        ),
    };

    let base = div()
        .id(id)
        .role(Role::Button)
        .h(px(size::BUTTON_HEIGHT))
        .px(px(size::BUTTON_PAD_X))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(size::BUTTON_GAP))
        .rounded(px(size::RADIUS_BUTTON))
        .border_1()
        .border_color(rgb(border))
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .text_size(px(size::TEXT_UI))
        .whitespace_nowrap();

    // Primary buttons are set in the semibold weight, as in the design.
    let base = if style == ButtonStyle::Primary {
        base.font_weight(FontWeight::SEMIBOLD)
    } else {
        base
    };

    match availability {
        Availability::Enabled => base.cursor_pointer().hover(move |s| s.bg(rgb(hover))),
        Availability::Disabled => base.opacity(layout::DISABLED_OPACITY),
    }
}

/// An icon at a size and color. An SVG takes its color from its own style,
/// never from its parent, so every icon names one.
pub fn icon(icon: Icon, size_px: f32, color: u32) -> Svg {
    svg()
        .path(icon.path())
        .size(px(size_px))
        .flex_none()
        .text_color(rgb(color))
}

/// How a pill is colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillTone {
    Failed,
    Passed,
    /// The run on screen is compared with this one.
    Compared,
    /// A label with no verdict, like "Unregistered".
    Quiet,
}

/// A rounded label in the header.
pub fn pill(text: impl Into<SharedString>, tone: PillTone, fonts: &Fonts) -> Div {
    let (bg, fg, border) = match tone {
        PillTone::Failed => (theme::RED_PILL, theme::RED_SOFT, theme::RED_BORDER),
        PillTone::Passed => (theme::GREEN_PILL, theme::GREEN_SOFT, theme::GREEN_BORDER),
        PillTone::Compared => (theme::BLUE_PILL, theme::BLUE_SOFT, theme::BLUE_BORDER),
        PillTone::Quiet => (theme::PANEL, theme::MUTED, theme::LINE_2),
    };
    div()
        .flex_none()
        .px(px(size::PILL_PAD_X))
        .py(px(size::PILL_PAD_Y))
        .rounded_full()
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .border_1()
        .border_color(rgb(border))
        .font_family(fonts.mono.clone())
        .text_size(px(size::TEXT_SMALL))
        .whitespace_nowrap()
        .child(text.into())
}

/// A panel's title bar: small caps text over a rule, with optional
/// content on the right.
pub fn panel_title(title: &str, right: Option<AnyElement>) -> Div {
    let bar = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .px(px(size::PANEL_PAD_X))
        .py(px(size::PANEL_TITLE_PAD_Y))
        .text_size(px(size::TEXT_SMALL))
        .text_color(rgb(theme::MUTED))
        .border_b_1()
        .border_color(rgb(theme::LINE_SOFT))
        .child(title.to_uppercase());
    match right {
        Some(right) => bar.child(right),
        None => bar,
    }
}

/// A caption and value pair for the readouts: "step 1,204 / 11,760".
pub fn readout(
    caption: &str,
    value: impl Into<SharedString>,
    value_color: u32,
    weight: FontWeight,
) -> Div {
    div()
        .flex()
        .flex_none()
        .gap(px(size::READOUT_INNER_GAP))
        .whitespace_nowrap()
        .child(
            div()
                .text_color(rgb(theme::MUTED))
                .child(caption.to_string()),
        )
        .child(
            div()
                .text_color(rgb(value_color))
                .font_weight(weight)
                .child(value.into()),
        )
}
