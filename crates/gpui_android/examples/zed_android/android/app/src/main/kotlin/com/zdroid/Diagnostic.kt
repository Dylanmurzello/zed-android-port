package com.zdroid

import android.content.ContentValues
import android.content.Context
import android.hardware.input.InputManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.MediaStore
import android.util.Log
import android.view.InputDevice
import android.widget.Toast
import java.io.File
import java.text.SimpleDateFormat
import java.util.ArrayDeque
import java.util.Date
import java.util.Locale

/// On-device diagnostic exporter.
///
/// User apps can't read another app's logcat without READ_LOGS — that's a
/// signature-level permission and there's no user-facing grant path. A
/// reporter with no PC + ADB therefore can't dump Zdroid's logs via Termux
/// or any other shell on the device. Instead we maintain a small ring of
/// the log lines worth shipping inside Zdroid itself, and offer a one-tap
/// "Export Diagnostic" that drops a dump file into the user's Downloads
/// folder so they can attach it to a GitHub issue via the Files app or
/// any other browser/upload widget.
///
/// Save path: `Downloads/Zdroid/zdroid-diag-<stamp>.txt`. We write through
/// MediaStore.Downloads (API 29+) so no WRITE_EXTERNAL_STORAGE grant is
/// needed; pre-29 falls back to the app-private cacheDir copy and the
/// toast surfaces that path instead.
object Diagnostic {
    private const val RING_CAPACITY = 500

    private val ring = ArrayDeque<String>()

    @JvmStatic
    fun record(tag: String, level: Char, message: String) {
        val line = "${timestamp()} $level/$tag: $message"
        synchronized(ring) {
            if (ring.size >= RING_CAPACITY) ring.pollFirst()
            ring.addLast(line)
        }
    }

    @JvmStatic
    fun composeDump(context: Context, extras: String?): File {
        val sb = StringBuilder()
        sb.appendLine("# Zdroid diagnostic ${isoTimestamp()}")
        sb.appendLine()
        appendDeviceSection(sb)
        appendAppSection(sb, context)
        appendInputDevicesSection(sb, context)
        if (!extras.isNullOrEmpty()) {
            sb.appendLine("## Runtime state (gpui_android)")
            sb.appendLine(extras.trim())
            sb.appendLine()
        }
        appendRingSection(sb)

        val dir = File(context.cacheDir, "diagnostic").apply { mkdirs() }
        val stamp = SimpleDateFormat("yyyyMMdd-HHmmss", Locale.US).format(Date())
        val file = File(dir, "zdroid-diag-$stamp.txt")
        file.writeText(sb.toString())
        return file
    }

    @JvmStatic
    fun save(context: Context, file: File) {
        val publicPath = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            copyToDownloads(context, file)
        } else {
            null
        }
        val message = if (publicPath != null) {
            "Diagnostic saved: $publicPath"
        } else {
            // Pre-API-29 path or MediaStore failure — surface the
            // private path so the user can fish it out via `adb pull`
            // or via Files-app "show app private data" if their OEM
            // ships that. Better than silent failure.
            "Diagnostic saved: ${file.absolutePath}"
        }
        Log.i("zed_android_diag", message)
        // Toast must be created on a thread with a Looper. The JNI
        // dispatch from Rust hits us on gpui's executor worker which
        // has none, so post the toast to the main thread regardless of
        // who called us — cheap, makes the call site context-agnostic.
        Handler(Looper.getMainLooper()).post {
            Toast.makeText(context, message, Toast.LENGTH_LONG).show()
        }
    }

    private fun copyToDownloads(context: Context, file: File): String? {
        return try {
            val resolver = context.contentResolver
            val values = ContentValues().apply {
                put(MediaStore.Downloads.DISPLAY_NAME, file.name)
                put(MediaStore.Downloads.MIME_TYPE, "text/plain")
                // RELATIVE_PATH puts it under /storage/emulated/0/Download/Zdroid/
                // so it's segregated from random user downloads.
                put(MediaStore.Downloads.RELATIVE_PATH, "Download/Zdroid")
            }
            val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                ?: return null
            resolver.openOutputStream(uri)?.use { out ->
                file.inputStream().use { it.copyTo(out) }
            } ?: return null
            "Downloads/Zdroid/${file.name}"
        } catch (t: Throwable) {
            Log.w("zed_android_diag", "copyToDownloads failed", t)
            null
        }
    }

    private fun appendDeviceSection(sb: StringBuilder) {
        sb.appendLine("## Device")
        sb.appendLine("manufacturer: ${Build.MANUFACTURER}")
        sb.appendLine("model:        ${Build.MODEL}")
        sb.appendLine("device:       ${Build.DEVICE}")
        sb.appendLine("product:      ${Build.PRODUCT}")
        sb.appendLine("brand:        ${Build.BRAND}")
        sb.appendLine("release:      Android ${Build.VERSION.RELEASE}")
        sb.appendLine("sdk:          ${Build.VERSION.SDK_INT}")
        sb.appendLine("fingerprint:  ${Build.FINGERPRINT}")
        sb.appendLine()
    }

    private fun appendAppSection(sb: StringBuilder, context: Context) {
        sb.appendLine("## App")
        sb.appendLine("package:      ${context.packageName}")
        val pkg = try {
            context.packageManager.getPackageInfo(context.packageName, 0)
        } catch (t: Throwable) {
            sb.appendLine("packageInfo failed: ${t.message}")
            null
        }
        if (pkg != null) {
            sb.appendLine("versionName:  ${pkg.versionName}")
            sb.appendLine("versionCode:  ${pkg.longVersionCode}")
        }
        sb.appendLine()
    }

    private fun appendInputDevicesSection(sb: StringBuilder, context: Context) {
        sb.appendLine("## Input devices")
        try {
            val im = context.getSystemService(InputManager::class.java)
            if (im == null) {
                sb.appendLine("(InputManager unavailable)")
            } else {
                for (id in im.inputDeviceIds) {
                    val d = im.getInputDevice(id) ?: continue
                    sb.appendLine("- id=$id name=\"${d.name}\"")
                    sb.appendLine("    vendorId=0x${d.vendorId.toString(16)} productId=0x${d.productId.toString(16)}")
                    sb.appendLine("    sources=0x${d.sources.toString(16)} (${sourcesToNames(d.sources)})")
                    sb.appendLine("    keyboardType=${keyboardTypeName(d.keyboardType)} isVirtual=${d.isVirtual} isExternal=${d.isExternal}")
                }
            }
        } catch (t: Throwable) {
            sb.appendLine("(enumeration failed: ${t.message})")
        }
        sb.appendLine()
    }

    private fun appendRingSection(sb: StringBuilder) {
        val snapshot: List<String>
        synchronized(ring) { snapshot = ring.toList() }
        sb.appendLine("## Ring buffer (${snapshot.size} lines)")
        for (line in snapshot) sb.appendLine(line)
    }

    private fun timestamp(): String =
        SimpleDateFormat("HH:mm:ss.SSS", Locale.US).format(Date())

    private fun isoTimestamp(): String =
        SimpleDateFormat("yyyy-MM-dd HH:mm:ss.SSS", Locale.US).format(Date())

    private fun sourcesToNames(sources: Int): String {
        val names = mutableListOf<String>()
        // Each Android InputDevice.SOURCE_* constant is a bitfield; AND
        // each against the device's sources mask and label the ones that
        // light up. Order matters only for readability.
        fun check(mask: Int, name: String) {
            if (sources and mask == mask) names.add(name)
        }
        check(InputDevice.SOURCE_KEYBOARD, "KEYBOARD")
        check(InputDevice.SOURCE_DPAD, "DPAD")
        check(InputDevice.SOURCE_GAMEPAD, "GAMEPAD")
        check(InputDevice.SOURCE_TOUCHSCREEN, "TOUCHSCREEN")
        check(InputDevice.SOURCE_MOUSE, "MOUSE")
        check(InputDevice.SOURCE_MOUSE_RELATIVE, "MOUSE_RELATIVE")
        check(InputDevice.SOURCE_STYLUS, "STYLUS")
        check(InputDevice.SOURCE_TRACKBALL, "TRACKBALL")
        check(InputDevice.SOURCE_TOUCHPAD, "TOUCHPAD")
        check(InputDevice.SOURCE_JOYSTICK, "JOYSTICK")
        return if (names.isEmpty()) "NONE" else names.joinToString("|")
    }

    private fun keyboardTypeName(kt: Int): String = when (kt) {
        InputDevice.KEYBOARD_TYPE_NONE -> "NONE"
        InputDevice.KEYBOARD_TYPE_NON_ALPHABETIC -> "NON_ALPHABETIC"
        InputDevice.KEYBOARD_TYPE_ALPHABETIC -> "ALPHABETIC"
        else -> "type=$kt"
    }
}
