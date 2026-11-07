# Trackpads and mice go through Android's input stack

**Status:** Active
**Files:** `crates/gpui_android/src/events.rs`, `crates/gpui_android/src/events/mouse.rs`, `MotionEventExt.kt`, `MainActivity.kt` (`dispatchTouchEvent`)

## What Android does

The Book Cover trackpad (`sec_touchpad_pogo`, `Classes: TOUCH | TOUCH_MT | TOUCHPAD`) goes through AOSP's `TouchpadInputMapper` and `GestureConverter`, which run the Gestures library. `dumpsys input` shows both, with their properties. Android draws the pointer, applies its pointer speed, runs the library's palm handling (its `Palm` and `Keyboard Palm Prevent Timeout` properties), and reports this to the window under the pointer (Android 16, One UI 8, tablet mode, 2026-11-01, logged by a throwaway app):

| Gesture | Events |
|---|---|
| Move | `HOVER_ENTER`, `HOVER_MOVE`, `HOVER_EXIT`, source `MOUSE`, tool `FINGER` |
| Tap | `DOWN` with `BUTTON_PRIMARY`, `BUTTON_PRESS`, `BUTTON_RELEASE`, `UP` |
| Two-finger tap, or a press with two fingers | The same with `BUTTON_SECONDARY` |
| Press until it clicks, and slide | `DOWN`, `BUTTON_PRESS`, `MOVE` with the button held, `BUTTON_RELEASE`, `UP` |
| Tap, touch again and slide, with tap dragging on | The same, as one left-button drag |
| Two-finger swipe | `DOWN`, `MOVE`, `UP` of one fake finger with `CLASSIFICATION_TWO_FINGER_SWIPE`, whose `x` and `y` move with the scroll |
| Finger landing during a fling | That fake finger's `DOWN` and `CANCEL` |

Android's SDK also has `CLASSIFICATION_PINCH` (not logged). Both classifications arrived in Android 14. Android leaves the fling to the app (`handleFling` in `GestureConverter.cpp`).

## What gpui_android does with them

A pointer's events are a mouse's, in `events/mouse.rs`. A pointer's event with a classification is a gesture, whose fake fingers aren't presses, and GameActivity's events don't carry the classification, so `MainActivity.dispatchTouchEvent` sends these over JNI before a view can take them for touches.

A two-finger swipe is a pointer scroll, which is what `ScrollSource::Pointer` documents and what `gpui_ios` sends, with gpui's `PointerFling` for the glide. It isn't a touch: a touch could tap, long press, or grab a scrollbar or pane splitter under the pointer, and would switch gpui to touch input. A pinch does nothing yet.

## The gesture areas

Zed's window fills the screen, including what Android reports as `WindowInsets.Type.mandatorySystemGestures()`, areas where system gestures have priority and "may consume some or all touch input". With the system bars hidden (One UI 8, tablet mode, 2026-11-01), these are 46 px at the top and 38 px at the bottom. Zed's status bar lies inside the bottom one, so its buttons are hard to press. A finger's press in that area reaches the window but presses nothing, which leaves the one pixel row above it. A mouse's or trackpad's press works down to about 28 px from the bottom, the buttons' top 10 px or so, and below that goes to One UI's `TaskbarWindow`. The top area doesn't get in the way: the menu bar and the title bar's button inside it took presses from all three.

Android's guidance is to keep interactive content out of these areas. A test build gave the surface margins of that size from `WindowInsetsReporter`, and the owner preferred a window that fills the screen to the strips that left (2026-11-01).

## One UI's differences

- A press with the pointer at the screen's edge reports `DOWN` with no button and private flags, and one mid-screen press and slide did too. `events/mouse.rs` reads a `DOWN` with no button as the left one.
- The pad's firmware reports `BTN_RIGHT` when a press has two contacts on the pad and `BTN_MOUSE` with one, so a finger resting on the pad turns a press into a right-click (`getevent`, 2026-11-01).

## Settings

These are Android's, not Zdroid's: pointer speed, tap to click, and scrolling direction (`touchpad_scrolling_direction` on One UI). Tap dragging is off by default, and `adb shell settings put system touchpad_tap_dragging 1` turns it on. An app can't change it.

## Why not pointer capture

Zdroid used to capture the pointer, recognize trackpad gestures itself and draw its own cursor. Capture came from a May 2026 reading that the trackpad "reports as a single-finger `SOURCE_MOUSE` pointer with no multi-finger gesture recognition". That single finger is the fake finger above, whose classification nothing read. Android's docs describe capture for apps such as games and remote desktops, and on the tablet it also kept the pointer from a window's title bar.
