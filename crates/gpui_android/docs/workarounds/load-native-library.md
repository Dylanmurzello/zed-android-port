# Loading the native library

**Status:** Active
**Files:** `NativeBridge.kt`, `ZedApplication.kt`

## Problem

`ExtraWindowActivity` extends `AppCompatActivity`, which doesn't load a native
library. `MainActivity` extends `GameActivity`, which loads the one named by
`android.app.lib_name` in the manifest. So an extra window's first JNI call
could fail with `UnsatisfiedLinkError`.

## Constraint

`System.loadLibrary` must run before any JNI call. Loading from
`Application.onCreate` satisfies that, but it runs for every process start,
including when only `ZedDocumentsProvider` or a broadcast started the process.
The library holds the whole editor, so mapping and relocating it there would
slow every query another app makes while Zdroid isn't running.

## Solution

`NativeBridge`, which every JNI declaration goes through, loads the library in
its `init` block. Loading twice is harmless, so `GameActivity` doing it as well
changes nothing.

## Failure mode if regressed

An `external fun` declared outside `NativeBridge` can run before the library is
loaded and throw `UnsatisfiedLinkError`.
