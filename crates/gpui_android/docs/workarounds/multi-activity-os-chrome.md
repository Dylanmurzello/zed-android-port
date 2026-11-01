# Multi-Activity OS-chromed extra windows

**Status:** Active
**Phase / Commit:** L7 (multi-Activity transition)
**Files:**
- `crates/gpui_android/src/multi_window.rs`
- `crates/gpui_android/src/platform.rs`
- `crates/gpui_android/src/window.rs`
- `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/com/zdroid/MainActivity.kt`
- `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/com/zdroid/ExtraWindowActivity.kt`
- `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/com/zdroid/NativeBridge.kt`
- `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/com/zdroid/ZedApplication.kt`
- `crates/gpui_android/examples/zed_android/android/app/src/main/AndroidManifest.xml`

## Problem

`cx.open_window` on Android originally maxed out at one window per process —
GameActivity exposes exactly one `ANativeWindow` so any second
`VkSurfaceKHR` against it gets `ERROR_NATIVE_WINDOW_IN_USE_KHR`.

L6 worked around it by stuffing extra `SurfaceView`s into MainActivity's
`FrameLayout` (each carrying its own `ANativeWindow`), but those overlays are
borderless rectangles floating inside the primary Activity. **No chrome, no
close X, no drag bar, no resize handles.** Settings opens and the user has
no way to dismiss it.

## Constraint

Modern Android (Samsung DeX, Pixel desktop windowing, Android 15+ Desktop
Mode, ChromeOS) provides native freeform windowing chrome — close X, drag
bar, resize handles, minimize, maximize — for free, **but only when each
window is a separate Activity task with `resizeableActivity="true"`**. The
OS treats one Activity = one task = one freeform window. There's no API
to ask the OS to "wrap a `SurfaceView` in chrome" inside an existing
Activity's content view.

So getting chrome means going multi-Activity, multi-task. This is the same
pattern Chrome browser, Edge, and Files-by-Google use for "open in new
window" affordances. Universal Android API — not Samsung-private. API floor
is 24 (`resizeableActivity`); the app's `minSdk` is above it.

## Solution

Each `cx.open_window` past the first launches a separate
`ExtraWindowActivity` (a thin AppCompatActivity host) via JNI Intent.
ExtraWindowActivity inflates a `SurfaceView` whose `ANativeWindow` is the
gpui Window's render target. The OS wraps the Activity in freeform chrome
on devices that support it.

```
APK (single process, single JVM, single Rust runtime, single gpui App)
├── MainActivity : GameActivity                  ← primary Workspace surface
│   └── android_main runs once, drives gpui App + game thread
└── ExtraWindowActivity : AppCompatActivity      ← N instances, one per cx.open_window
    ├── pure SurfaceView host (no game loop)
    ├── reads window_id from Intent extras
    ├── onCreate    → JNI nativeOnExtraActivityCreated(id, GlobalRef)
    ├── surfaceCreated/Changed/Destroyed → existing JNI bridge
    ├── OnTouchListener → existing JNI bridge
    └── onDestroy   → JNI nativeOnExtraActivityDestroyed(id)
```

Same single Linux process, same JVM, same Rust runtime, same gpui App.
Critical: **no `android:process` attribute** on either Activity — that would
fork a separate JVM and break shared state.

### Manifest

```xml
<application android:name=".ZedApplication"
             android:resizeableActivity="true"
             android:appCategory="productivity">
  <activity android:name=".MainActivity"
            android:exported="true"
            android:configChanges="<exhaustive list>">
    <meta-data android:name="android.app.lib_name" android:value="zed_android"/>
    ...
  </activity>
  <activity android:name=".ExtraWindowActivity"
            android:exported="false"
            android:resizeableActivity="true"
            android:documentLaunchMode="always"
            android:taskAffinity="com.zdroid.extra"
            android:configChanges="<same exhaustive list>"/>
</application>
```

`taskAffinity` differentiates the extra window's task from MainActivity's.
`documentLaunchMode="always"` makes each launch a fresh task.

### Rust → JVM Intent dispatch

`multi_window::launch_extra_activity` builds the Intent through JNI:

```rust
// Resolve ExtraWindowActivity via MainActivity's ClassLoader
let main_class = env.get_object_class(&main_activity)?;
let class_loader = env
    .call_method(&main_class, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
    .l()?;
let class_name = env.new_string("com.zdroid.ExtraWindowActivity")?;
let extra_class = env
    .call_method(&class_loader, "loadClass", ..., &[JValue::Object(&class_name)])?
    .l()?;

// Build Intent + putExtra(window_id) + startActivity
let intent = env.new_object(&intent_class,
    "(Landroid/content/Context;Ljava/lang/Class;)V",
    &[JValue::Object(&main_activity), JValue::Object(&extra_class)])?;
env.call_method(&intent, "putExtra",
    "(Ljava/lang/String;J)Landroid/content/Intent;",
    &[JValue::Object(&extra_key), JValue::Long(window_id as i64)])?;
env.call_method(&main_activity, "startActivity",
    "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])?;
```

Game thread blocks on a `oneshot::Receiver` until `surfaceCreated` JNIs back
with the new `ANativeWindow`. Hard 4-second timeout protects against
ANR-class freezes from a stalled cold Activity launch (typical cold start
in DeX freeform: 2-2.1s; warm reopen ~50ms).

### Activity ref tracking

`finishAndRemoveTask` (gpui-initiated close) must target a specific Activity
instance. We track them in a process-global registry keyed by `window_id`:

```rust
static EXTRA_ACTIVITY_REFS: Mutex<Option<HashMap<u64, GlobalRef>>> = Mutex::new(None);
```

- `nativeOnExtraActivityCreated` (called from `ExtraWindowActivity.onCreate`)
  wraps the Activity in a `GlobalRef` and inserts.
- `finish_extra_activity` reads from the registry, calls
  `Activity.finishAndRemoveTask` on the stored ref.
- `nativeOnExtraActivityDestroyed` removes the ref AND posts `OsClosed` so
  the gpui-side window can be reaped.

**Thread constraint** (also in module docs): only mutate the map from the
gpui main thread. The `jni` crate calls `DeleteGlobalRef` via whatever
`JNIEnv` the dropping thread can attach via `JavaVM::attach_current_thread`.
Drops on a non-attachable thread (e.g. tokio worker after its `AttachGuard`
released) silently leak.

### Bidirectional close

Two paths:

- **Path A (gpui-initiated):** `Window::remove_window()` → drops
  `Box<PlatformWindow>` → `AndroidWindow::Drop` reads `os_closed` flag (false)
  → JNI `finish_extra_activity` → Activity destroys → JNI fires `OsClosed`
  → drain handler removes from `extra_windows` map (already empty no-op).
- **Path B (OS-initiated):** user clicks chrome X → `Activity.finish()` →
  `onDestroy` → JNI fires `OsClosed` → drain handler sets `os_closed=true`
  on state, takes the registered `on_close` callback (gpui wires it at
  `gpui/src/window.rs:1327`), invokes it. Callback drives
  `remove_window()` via captured `cx.to_async()`. Window drops, AndroidWindow
  drops, Drop reads `os_closed=true` → skips JNI finish (Activity already
  gone). Idempotent.

Critical: `os_closed: AtomicBool` lives on `AndroidWindowState` (inside the
`Rc<RefCell>`), NOT on `AndroidWindow` itself. The state survives the Box
drop and is reachable from the drain handler via `extra_windows`.

## Why this works

- Same-process multi-Activity is the textbook Chrome/Edge pattern. AOSP
  blesses it.
- gpui's `PlatformWindow` is opaque to where the Activity lives. The
  trait sees a thing that draws and dispatches input; doesn't care if it's
  in a separate task with chrome or an overlay.
- `ANativeWindow_fromSurface` works the same regardless of which Activity
  hosts the SurfaceView.
- gpui's Window has zero opinion about what an "extra window" looks like —
  it just holds a `Box<PlatformWindow>` and calls `draw(scene)`.

## Failure mode if regressed

- **No chrome** in DeX/desktop windowing → user can't close. (L6 baseline
  state.)
- **Same-process broken** (`android:process` set somewhere): JVM forks, app
  crashes on JNI access to gpui state from the wrong process.
- **`taskAffinity` collision**: ExtraWindowActivity ends up in MainActivity's
  task → no separate freeform window, no separate Recents card.
- **`os_closed` flag on AndroidWindow** (not state): gpui drops the wrapper
  on OS X click before drain handler reads the flag → drain misses it.

## See also

- [`appCategory="productivity"`](android16-app-category-productivity.md) — defangs the games carve-out
- [Android 16 freeform configChanges](android16-config-changes-resize.md) — drag-resize destroys Activity by default
- [`documentLaunchMode` implies Intent flags](document-launch-mode-implies-flags.md)
- [Cold Activity launch timeout (4s)](activity-launch-cold-timeout.md)
- [Loading the native library](load-native-library.md)
- [JNI exception clear after error](jni-exception-clear-after-error.md)
- [`futures::oneshot::Receiver::try_recv` semantics](futures-oneshot-tryrecv-semantics.md)
- [Process-death recovery for extra windows](process-death-recovery-extra-windows.md)
- [ActivityOptions launch bounds](activity-options-launch-bounds.md)
