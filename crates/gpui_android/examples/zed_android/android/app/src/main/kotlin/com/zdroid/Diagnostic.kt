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
import android.view.MotionEvent
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
    // Sized to hold ~20s of 100Hz captured-pointer motion plus the
    // transition/lifecycle lines around it. Tab S11 reporters move the
    // mouse for ~10s then export, so a single repro fits comfortably.
    private const val RING_CAPACITY = 2000

    private val ring = ArrayDeque<String>()

    private fun addLine(line: String) {
        synchronized(ring) {
            if (ring.size >= RING_CAPACITY) ring.pollFirst()
            ring.addLast(line)
        }
    }

    @JvmStatic
    fun record(tag: String, level: Char, message: String) {
        addLine("${timestamp()} $level/$tag: $message")
    }

    /// Structured arrival probe. Captures every shape-bit we'd need to
    /// distinguish "framework didn't route", "framework routed but
    /// source mask mismatched", "captured but historical samples
    /// dropped", and similar failure modes. `stage` is a short tag
    /// like "act.gen" (Activity.onGenericMotionEvent) or "act.touch"
    /// (Activity.dispatchTouchEvent) so we can see which dispatch hook
    /// fired. `accepted` reflects whether this event made it past our
    /// source-bit gate into the captured-pointer JNI bridge.
    @JvmStatic
    fun recordMotion(stage: String, event: MotionEvent, hasCapture: Boolean, accepted: Boolean) {
        val sb = StringBuilder(160)
        sb.append(timestamp()).append(" M/").append(stage)
            .append(" act=0x").append(event.actionMasked.toString(16))
            .append(" actBtn=0x").append(event.actionButton.toString(16))
            .append(" btn=0x").append(event.buttonState.toString(16))
            .append(" src=0x").append(event.source.toString(16))
            .append(" dev=").append(event.deviceId)
            .append(" n=").append(event.pointerCount)
            .append(" cap=").append(if (hasCapture) "1" else "0")
            .append(" acc=").append(if (accepted) "1" else "0")
            .append(" hist=").append(event.historySize)
        if (event.pointerCount > 0) {
            sb.append(" x=").append("%.1f".format(event.x))
                .append(" y=").append("%.1f".format(event.y))
                .append(" rx=").append("%.2f".format(event.getAxisValue(MotionEvent.AXIS_RELATIVE_X)))
                .append(" ry=").append("%.2f".format(event.getAxisValue(MotionEvent.AXIS_RELATIVE_Y)))
        }
        addLine(sb.toString())
    }

    /// Binary state transition (capture/focus/lifecycle). One line per
    /// edge change; do not call on every event.
    @JvmStatic
    fun recordTransition(kind: String, value: String) {
        addLine("${timestamp()} T/$kind=$value")
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
        sb.appendLine("fingerprint:  ${redactFingerprint(Build.FINGERPRINT)}")
        sb.appendLine()
    }

    /// Samsung / AOSP fingerprint shape:
    ///   <brand>/<product>/<device>:<release>/<buildid>/<incremental>:<type>/<tags>
    /// The buildid + incremental segments encode the carrier / region
    /// variant of the OEM ROM (e.g. `gts11wifi` vs `gts11vzw`), so
    /// they leak geography. Brand/product/device/release identify the
    /// hardware class — what we actually need — so keep the first
    /// `release:` segment and drop everything past the second `/`.
    private fun redactFingerprint(fp: String): String {
        val firstSlash = fp.indexOf('/')
        if (firstSlash < 0) return "<redacted>"
        val secondSlash = fp.indexOf('/', firstSlash + 1)
        if (secondSlash < 0) return "<redacted>"
        val thirdSlash = fp.indexOf('/', secondSlash + 1)
        if (thirdSlash < 0) return fp
        return fp.substring(0, thirdSlash) + "/<redacted>"
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
                    val displayName = if (d.vendorId == 0 && d.productId == 0) {
                        // Internal kernel devices (sec_touchpad, gpio-keys,
                        // hall_*, etc.) report vendor=0 product=0; the
                        // driver name is the only handle and isn't user-
                        // settable, so keep verbatim.
                        "\"${d.name}\""
                    } else {
                        // External paired devices: vendor/product hex
                        // already identifies the model via the public
                        // USB ID database, so the literal name is
                        // redundant info that adds PII risk (users
                        // sometimes name BT devices after themselves).
                        "<redacted:${redactDeviceName(d.name)}>"
                    }
                    sb.appendLine("- id=$id name=$displayName")
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

    /// Hashes a user-paired device name while keeping the first/last
    /// two characters so reporters can still cross-reference reports
    /// of the same gear ("Lo***se" matches across two Logi M650 dumps
    /// without leaking the literal string). 8-hex-char stable digest
    /// of the full name lets us tell apart "Logi M650 Mouse" from
    /// "Logi M650 Keyboard" if a reporter has both.
    private fun redactDeviceName(name: String): String {
        if (name.isEmpty()) return "[empty]"
        val prefix = name.take(2)
        val suffix = name.takeLast(2)
        val hash = name.hashCode().toUInt().toString(16).padStart(8, '0')
        return "$prefix***$suffix[$hash]"
    }
}
