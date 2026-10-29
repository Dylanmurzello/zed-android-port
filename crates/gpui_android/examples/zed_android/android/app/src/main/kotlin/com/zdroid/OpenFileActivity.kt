package com.zdroid

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.os.Handler
import android.os.Looper
import android.provider.OpenableColumns
import android.system.ErrnoException
import android.system.Os
import android.util.Log
import android.widget.Toast
import java.io.File
import java.io.IOException
import java.util.concurrent.Executors

/// Receives "Open with" and "Edit with" from other apps, such as the Files
/// app, and passes the file on to [MainActivity]. MainActivity can't take
/// these itself: an activity another app starts joins that app's task, so
/// MainActivity would get a second instance there, and gpui runs in one.
/// CLEAR_TOP with SINGLE_TOP hands the file to the running MainActivity
/// through `onNewIntent`, or starts it when Zdroid isn't running.
class OpenFileActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val uri = intent?.data
        if (uri != null) {
            val grants = Intent.FLAG_GRANT_READ_URI_PERMISSION or
                Intent.FLAG_GRANT_WRITE_URI_PERMISSION
            startActivity(
                Intent(this, MainActivity::class.java).apply {
                    action = OpenedFiles.ACTION_OPEN_FILE
                    setDataAndType(uri, intent.type)
                    // Passes the sender's grant on to MainActivity, which
                    // reads the file after this activity has finished.
                    addFlags(intent.flags and grants)
                    addFlags(
                        Intent.FLAG_ACTIVITY_NEW_TASK or
                            Intent.FLAG_ACTIVITY_CLEAR_TOP or
                            Intent.FLAG_ACTIVITY_SINGLE_TOP
                    )
                }
            )
        }
        // Theme.NoDisplay requires finishing before onResume.
        finish()
    }
}

/// Turns a file another app opened with Zdroid into one the editor can open
/// by path, and hands it to the editor (`gpui_android::open_files`).
///
/// Zed edits files by path, so a file on this device's storage opens where
/// it is when Zdroid can reach that path: on shared storage with All files
/// access, or in Zdroid's own folder. Any other file, such as one a cloud
/// app serves, opens as a copy in `~/received`.
internal object OpenedFiles {
    const val ACTION_OPEN_FILE = "com.zdroid.action.OPEN_FILE"
    private const val TAG = "zdroid_open_file"

    /// Off the UI thread: a provider may have to download the file first.
    private val executor = Executors.newSingleThreadExecutor()

    private val sharedStorageAliases = listOf(
        Regex("^/(?:data/media|mnt/user/\\d+/emulated|mnt/pass_through/\\d+/emulated)/(\\d+)/(.+)$") to
            "/storage/emulated/$1/$2",
        Regex("^/mnt/(?:media_rw|user/\\d+|pass_through/\\d+)/([0-9A-Fa-f]{4}-[0-9A-Fa-f]{4})/(.+)$") to
            "/storage/$1/$2",
    )

    fun handle(context: Context, intent: Intent?) {
        if (intent?.action != ACTION_OPEN_FILE) return
        val uri = intent.data ?: return
        val appContext = context.applicationContext
        executor.execute {
            val direct = directPath(appContext, uri)
            val path = direct ?: copyIntoHome(appContext, uri)
            Log.i(TAG, "$uri -> $path (copy=${direct == null})")
            if (path == null) {
                toast(appContext, "Zdroid couldn't open the file")
                return@execute
            }
            if (direct == null) {
                toast(appContext, copyMessage())
            }
            NativeBridge.nativeOpenUrl(Uri.fromFile(File(path)).toString())
        }
    }

    /// The file itself, when Zdroid can read it where it is. The provider's
    /// file descriptor names the file it opened, in the spelling of the
    /// provider's storage view; that's mapped to the path apps see, and the
    /// sizes must agree.
    private fun directPath(context: Context, uri: Uri): String? {
        val candidate = if (uri.scheme == "file") {
            uri.path
        } else {
            try {
                context.contentResolver.openFileDescriptor(uri, "r")?.use { descriptor ->
                    val target = Os.readlink("/proc/self/fd/${descriptor.fd}")
                    appVisiblePath(context, target)
                        ?.takeIf { File(it).length() == descriptor.statSize }
                }
            } catch (error: IOException) {
                Log.w(TAG, "open $uri", error)
                null
            } catch (error: SecurityException) {
                Log.w(TAG, "open $uri", error)
                null
            } catch (error: IllegalArgumentException) {
                Log.w(TAG, "open $uri", error)
                null
            } catch (error: ErrnoException) {
                Log.w(TAG, "read the path of $uri", error)
                null
            }
        } ?: return null
        val file = File(candidate)
        return candidate.takeIf { file.isFile && file.canRead() }
    }

    /// The path apps use for a file the kernel names by a lower mount, and
    /// the editor's spelling of Zdroid's own files dir.
    private fun appVisiblePath(context: Context, target: String): String? {
        if (target.startsWith("/storage/")) return target
        val filesDir = context.filesDir.path
        val kernelFilesDir = context.filesDir.canonicalPath
        if (target.startsWith("$filesDir/")) return target
        if (target.startsWith("$kernelFilesDir/")) {
            return filesDir + target.removePrefix(kernelFilesDir)
        }
        for ((alias, replacement) in sharedStorageAliases) {
            if (alias.matches(target)) return alias.replace(target, replacement)
        }
        return null
    }

    /// A copy in `~/received`, under the name the sender gave the file.
    private fun copyIntoHome(context: Context, uri: Uri): String? {
        val directory = File(context.filesDir, "home/received")
        if (!directory.isDirectory && !directory.mkdirs()) {
            Log.w(TAG, "can't create $directory")
            return null
        }
        val target = uniqueFile(directory, displayName(context, uri))
        return try {
            val input = context.contentResolver.openInputStream(uri) ?: return null
            input.use { source -> target.outputStream().use { source.copyTo(it) } }
            target.path
        } catch (error: Exception) {
            Log.w(TAG, "copy $uri", error)
            if (target.exists() && !target.delete()) {
                Log.w(TAG, "can't remove the partial copy $target")
            }
            null
        }
    }

    private fun displayName(context: Context, uri: Uri): String {
        val name = try {
            context.contentResolver
                .query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
                ?.use { cursor -> if (cursor.moveToFirst()) cursor.getString(0) else null }
        } catch (error: Exception) {
            Log.w(TAG, "read the name of $uri", error)
            null
        } ?: uri.lastPathSegment
        val segment = name?.substringAfterLast('/')
        return segment?.takeIf { it.isNotBlank() && it != "." && it != ".." } ?: "file"
    }

    /// `name`, or `stem (n).extension` when that's taken.
    private fun uniqueFile(directory: File, name: String): File {
        val dot = name.lastIndexOf('.').takeIf { it > 0 } ?: name.length
        val stem = name.substring(0, dot)
        val extension = name.substring(dot)
        var file = File(directory, name)
        var number = 1
        while (file.exists()) {
            file = File(directory, "$stem ($number)$extension")
            number++
        }
        return file
    }

    private fun copyMessage(): String {
        val sharedStorageBlocked = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
            !Environment.isExternalStorageManager()
        return if (sharedStorageBlocked) {
            "Opened a copy in ~/received. To edit files where they are, turn on " +
                "Settings → Android → Allow Access to /sdcard."
        } else {
            "Opened a copy in ~/received: saving it doesn't change the original."
        }
    }

    private fun toast(context: Context, message: String) {
        Handler(Looper.getMainLooper()).post {
            Toast.makeText(context, message, Toast.LENGTH_LONG).show()
        }
    }
}
