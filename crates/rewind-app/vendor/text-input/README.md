# rewind-text-input

A one-line text field for GPUI: typing, input methods, the mouse and
Shift+arrows to select, Home and End, and copy, cut and paste. Used by
rewind-app's Open link dialog and its step readout.

Adapted from `examples/input.rs` in the `gpui-pre` 0.3.7 crate (Zed's
GPUI at zed@1a28cff), Copyright Zed Industries, Inc., under the Apache
License 2.0 in LICENSE-APACHE. The changes:

- The field is a library type, `TextInput`, with its key bindings in
  `bindings()`, rather than an example program.
- It scrolls sideways to keep the caret in view, and clips what does not
  fit, where the example drew a long line past its edge.
- Colors and the placeholder come from the caller, `TextInputStyle`; the
  font and its size come from the element around it.
- Pasted text keeps its first line only.
- `select_everything` selects the whole text from outside the field, so
  a field opened on a value is replaced by what is typed next.
