package com.zdroid

import android.app.Application
import java.io.File

/// Ensures `~` (the root `ZedDocumentsProvider` exposes) exists before the system
/// serves Files app queries. Providers attach before any Activity: when another
/// app browses Zdroid while it isn't running, Android starts the process, runs
/// this and the provider's `onCreate`, then serves queries without ever reaching
/// `MainActivity` or `android_main`, where the home folder's directories are
/// otherwise made.
class ZedApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        File(filesDir, "home/projects").mkdirs()
    }
}
