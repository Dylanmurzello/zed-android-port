package com.zdroid

import android.app.Application
import java.io.File

/// Loads the native library before any Activity starts. Required because
/// `ExtraWindowActivity` extends `AppCompatActivity` and does not trigger
/// the GameActivity meta-data path that loads `libzed_android.so`. With the
/// load centralized here, both activities can call into JNI on first touch
/// without per-Activity init blocks racing against Android's class-loader
/// when an Activity is recreated.
///
/// Also ensures `~` (the SAF root exposed by `ZedDocumentsProvider`)
/// exists before the system serves DocumentsUI queries. ContentProviders
/// attach earlier in the process lifecycle than Activities — when another
/// app opens DocumentsUI and Zed isn't running, Android forks our process,
/// runs `Application.onCreate`, then `ContentProvider.onCreate`, then
/// services queries — without ever instantiating `MainActivity` or
/// running `android_main`. The runtime setup and the home folder's
/// directories live in `android_main`, so without this `mkdirs` the SAF
/// picker would surface "Zed" with an empty root the first time another
/// app queries it.
class ZedApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        System.loadLibrary("zed_android")
        // Pre-create ~ AND ~/projects so:
        //   1. ZedDocumentsProvider's SAF root is non-empty before
        //      android_main runs (cold provider queries succeed with at
        //      least one visible subfolder).
        //   2. The noexec-banner Copy-to-projects dialog has a real
        //      destination dir from the moment the user can interact —
        //      no race with `storage::setup_user_symlinks` which mkdirs
        //      the same path on the Rust side at android_main entry.
        File(filesDir, "home/projects").mkdirs()
    }
}
