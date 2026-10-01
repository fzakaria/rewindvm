# gpui-pre-linux 0.3.7, patched for rewind-app

A copy of the `gpui-pre-linux` 0.3.7 crate from crates.io (Zed's
`gpui_linux` at zed@1a28cff), used through `[patch.crates-io]` in
`crates/rewind-app/Cargo.toml`. One change, in `src/linux/x11/window.rs`,
marked "rewind-app patch":

GPUI's X11 backend always creates its window with a 32-bit ARGB visual
when the screen offers one. Transparency only works when a compositing
manager runs, and without one some drivers present nothing into such a
window: under Xvfb with Mesa's lavapipe on Ubuntu 22.04 (Mesa 23.2) and
Debian 12 (Mesa 22.3) the window stays black. The patch takes the ARGB
visual only when a compositing manager owns the `_NET_WM_CM_S<screen>`
selection, and the screen's default visual otherwise.

Drop this copy, and the `[patch.crates-io]` entry, when moving to a
gpui-pre release that chooses the visual this way.
