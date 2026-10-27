# Ahead of upstream

Zdroid implements these because upstream Zed doesn't yet; at each Zed sync, check whether upstream now covers one, and if so take upstream's and delete ours.

Everything outside `crates/gpui_android` and the app is gated with `cfg(target_os = "android")`. Items (a) to (d) build on gpui's touch gesture recognizer (`crates/gpui/src/gestures.rs`), which gets all finger input.

## (a) Two-finger tap and two-finger pan

A two-finger tap becomes a secondary click when the fingers lift, and a two-finger drag becomes one touch at the fingers' centroid, so it scrolls.

- Ours: `crates/gpui_android/src/touch.rs`, `TouchFeed` (`pointer_down`, `pointer_up`, `start_two_finger_pan`, `TwoFingerGesture`). Writeup: [two-finger-rightclick.md](workarounds/two-finger-rightclick.md).
- Builds on: `PlatformInput::Touch` and gpui's `TouchGestureRecognizer`, which follows one touch at a time.
- Landed upstream when: `crates/gpui/src/gestures.rs` recognizes more than one touch (its "additional touches are ignored" note is gone, or a two-finger or secondary-tap gesture appears).

## (b) Touch-drag claims

A touch drag that starts on a scrollbar thumb, an editor scrollbar, a pane splitter or a dock edge drags it instead of scrolling the content.

- Ours: `crates/ui/src/components/scrollbar.rs` (`ScrollbarElement::paint`), `crates/editor/src/element.rs` (`paint_scrollbars`), `crates/workspace/src/pane_group.rs` (`PaneAxisElement::paint`), `crates/workspace/src/dock.rs` (`touch_resize_listener`) with `Workspace::drag_dock_edge` in `crates/workspace/src/workspace.rs`.
- Builds on: `TouchDragEvent` and `Window::prevent_default` on its `Started` phase.
- Landed upstream when: `grep -rn TouchDragEvent crates/ui crates/editor crates/workspace` finds ungated listeners.

## (c) Long press

A long press in the editor selects the word (the line, in the gutter) and moving the finger extends the selection. In the terminal it does the same and opens the context menu on release.

- Ours: `crates/editor/src/element/mouse.rs` (`paint_mouse_listeners`), `crates/terminal_view/src/terminal_element.rs` (`register_mouse_listeners`, `deploy_long_press_context_menu`) and `TerminalView::deploy_long_press_context_menu` in `crates/terminal_view/src/terminal_view.rs`.
- Builds on: `LongPressEvent`, `Window::capture_long_press`, `Window::has_long_press_capture`.
- Landed upstream when: `grep -rn LongPressEvent crates/editor crates/terminal_view` finds ungated listeners.

## (d) Taps carry modifier keys

Taps carry the modifier keys held on a keyboard cover, so Ctrl+tap on a link opens it.

- Ours: the `cfg(target_os = "android")` block in the `RecognizedTouchGesture::Tap` arm of `Window::dispatch_recognized_touch_gesture`, `crates/gpui/src/window.rs`.
- Builds on: gpui's recognizer, which builds taps with `Modifiers::default()`.
- Landed upstream when: the recognizer's `Tap` in `crates/gpui/src/gestures.rs` no longer uses `Modifiers::default()`, or that arm in `window.rs` sets the window's modifiers itself.

## (e) Keyboard requests

The on-screen keyboard shows when the user clicks or taps a text field, including after the user hid it, and when a single-line editor (a picker, a search bar) gains focus. Focus moving any other way doesn't show it, as on Android.

- Ours: the left mouse down in `crates/editor/src/element/mouse.rs` (`paint_mouse_listeners`) and `crates/terminal_view/src/terminal_element.rs` (`register_mouse_listeners`), and `Editor::handle_focus` with `handle_blur` in `crates/editor/src/editor.rs` for single-line editors. On the platform side, `AndroidWindow::request_soft_keyboard` in `crates/gpui_android/src/window.rs`, and `reconcile_ime_for_window` in `crates/gpui_android/src/platform.rs`, which only hides the keyboard when focus leaves a text field.
- Builds on: `Window::request_virtual_keyboard`, as `EntityInputHandler`'s docs in `crates/gpui/src/input.rs` ask of text fields.
- Landed upstream when: `grep -rn request_virtual_keyboard crates/editor crates/terminal_view` finds ungated calls.

## (f) Tabs always show their close button

Zed shows a tab's close button on hover, which a finger can't do, so the app's defaults set `tabs.show_close_button` to `"always"`.

- Ours: `set_default_settings` in `crates/gpui_android/examples/zed_android/src/android_input.rs`.
- Builds on: the `tabs.show_close_button` setting (`ShowCloseButton`).
- Landed upstream when: `crates/workspace/src/pane.rs` shows the close button for touch input, for example by checking `Window::last_input_was_touch`.

## Upstream APIs Zdroid doesn't use yet

Upstream gpui has these, but nothing in Zed drives them yet, so Zdroid keeps its own behavior. At each Zed sync, run the grep; once it finds something, adopt the API.

- `PlatformWindow::set_text_input_configuration`: gpui forwards each text field's `TextInputConfiguration` (autocorrect, autocapitalization, suggestions, the enter key's action), but Zed's editor doesn't supply one, so every field gets gpui's default of no autocorrect and no suggestions. Zdroid keeps choosing the `EditorInfo` per field kind (terminal or code editor) in `ImeHostView.onCreateInputConnection`. Adopt when `grep -rn "fn text_input_configuration" crates/editor` finds an implementation.
- `PlatformWindow::set_back_handler` and `set_back_enabled`: gpui's hooks for the Android back button and back gesture. Nothing in Zed calls them. Adopt when `grep -rn "set_back_handler\|set_back_enabled" crates --include='*.rs' | grep -v "^crates/gpui"` finds a caller.
- The `ime` part of `PlatformWindow::insets`: `AndroidWindow` reports it as zero, consumed the way `adjustResize` consumes it, because Zdroid shrinks the layout area above the keyboard itself (`AndroidWindowState::layout_bounds` in `crates/gpui_android/src/window.rs`). If Zed's workspace ever lays out with `Window::fully_visible_bounds` or `Window::insets` (`grep -rn "fully_visible_bounds\|\.insets()" crates/workspace crates/zed`), report the real `ime` inset and stop shrinking.
