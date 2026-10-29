package com.zdroid

import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat

/// Foreground service that holds the app process out of Android's cached
/// state. An app in the cached state has its children stopped by the
/// phantom process killer and is itself an early pick for the low-memory
/// killer; a foreground service is the one signal that says "this app is
/// doing work the user asked for".
///
/// It runs while the editor has child processes (language servers,
/// terminal shells, debug adapters), which Rust reads from the process
/// tree and reports through `MainActivity.setRuntimeServiceActive`
/// (`gpui_android::runtime_service`), and while the Zdroid menu's Keep
/// Running in Background is on, for work without children such as Zed's
/// own agent. `keepAwake` adds a partial wake lock while children exist,
/// so builds continue with the screen off; it mirrors the
/// `android_runtime.keep_awake` setting.
class ZdRuntimeService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        ensureChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP_BACKGROUND_MODE) {
            setBackgroundModeEnabled(this, false)
            return START_NOT_STICKY
        }
        val backgroundMode = intent?.getBooleanExtra(EXTRA_BACKGROUND_MODE, false) ?: false
        val notification = buildNotification(backgroundMode)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
        val keepAwake = intent?.getBooleanExtra(EXTRA_KEEP_AWAKE, false) ?: false
        if (keepAwake) acquireWakeLock() else releaseWakeLock()
        // Not sticky: the app restarts the service from its own state after
        // a process death; a system restart with no editor process behind
        // it would only show an orphan notification.
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        releaseWakeLock()
        super.onDestroy()
    }

    private fun ensureChannel() {
        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        // Releases before 0.10 posted Keep Running in Background's
        // notification on a channel of its own.
        manager.deleteNotificationChannel(LEGACY_BACKGROUND_MODE_CHANNEL_ID)
        if (manager.getNotificationChannel(CHANNEL_ID) != null) return
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Runtime",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "Shown while Zdroid keeps running in the background"
            setShowBadge(false)
        }
        manager.createNotificationChannel(channel)
    }

    private fun buildNotification(backgroundMode: Boolean): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_runtime)
            .setContentTitle("Zdroid is running in the background")
            .setContentText("Language servers, terminals and agents keep running.")
            .setContentIntent(open)
            .setOngoing(true)
            .setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .apply {
                if (backgroundMode) {
                    val stop = PendingIntent.getService(
                        this@ZdRuntimeService,
                        0,
                        Intent(this@ZdRuntimeService, ZdRuntimeService::class.java)
                            .setAction(ACTION_STOP_BACKGROUND_MODE),
                        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                    )
                    addAction(0, "Stop", stop)
                }
            }
            .build()
    }

    @SuppressLint("WakelockTimeout")
    private fun acquireWakeLock() {
        if (wakeLock?.isHeld == true) return
        val power = getSystemService(Context.POWER_SERVICE) as PowerManager
        val lock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKE_LOCK_TAG).apply {
            setReferenceCounted(false)
        }
        try {
            // No timeout: the lock lives until the last child is gone, which
            // the app reports by starting the service again without it.
            lock.acquire()
            wakeLock = lock
            Log.i(TAG, "wake lock acquired")
        } catch (t: Throwable) {
            Log.w(TAG, "wake lock acquire failed", t)
        }
    }

    private fun releaseWakeLock() {
        val lock = wakeLock ?: return
        wakeLock = null
        try {
            if (lock.isHeld) lock.release()
            Log.i(TAG, "wake lock released")
        } catch (t: Throwable) {
            Log.w(TAG, "wake lock release failed", t)
        }
    }

    companion object {
        private const val ACTION_START = "com.zdroid.runtime.START"
        private const val ACTION_STOP_BACKGROUND_MODE = "com.zdroid.runtime.STOP_BACKGROUND_MODE"
        private const val EXTRA_KEEP_AWAKE = "keep_awake"
        private const val EXTRA_BACKGROUND_MODE = "background_mode"
        private const val CHANNEL_ID = "runtime"
        private const val LEGACY_BACKGROUND_MODE_CHANNEL_ID = "zdroid-background-mode"
        private const val NOTIFICATION_ID = 1
        private const val WAKE_LOCK_TAG = "zdroid:runtime"
        private const val TAG = "zed_android_runtime"
        // The preference Keep Running in Background used before 0.10, so the
        // choice carries over.
        private const val PREFS_NAME = "zdroid-background-mode"
        private const val KEY_ENABLED = "enabled"

        @Volatile private var childrenActive = false
        @Volatile private var keepAwake = false

        fun isBackgroundModeEnabled(context: Context): Boolean =
            context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .getBoolean(KEY_ENABLED, false)

        fun setBackgroundModeEnabled(context: Context, enabled: Boolean) {
            context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .edit()
                .putBoolean(KEY_ENABLED, enabled)
                .apply()
            if (!update(context)) {
                Log.w(TAG, "background mode change didn't reach the service")
            }
        }

        /// Records whether the editor has child processes and applies it.
        /// Returns false when Android refused to start the service, as it
        /// does for an app in the background on Android 12+.
        fun setChildrenActive(context: Context, active: Boolean, keepAwake: Boolean): Boolean {
            childrenActive = active
            this.keepAwake = keepAwake
            return update(context)
        }

        /// Starts the service when Keep Running in Background is on; called
        /// at launch.
        fun startIfBackgroundMode(context: Context) {
            if (isBackgroundModeEnabled(context) && !update(context)) {
                Log.w(TAG, "background mode service didn't start at launch")
            }
        }

        private fun update(context: Context): Boolean {
            val backgroundMode = isBackgroundModeEnabled(context)
            val intent = Intent(context, ZdRuntimeService::class.java)
            return try {
                if (childrenActive || backgroundMode) {
                    intent.action = ACTION_START
                    intent.putExtra(EXTRA_KEEP_AWAKE, childrenActive && keepAwake)
                    intent.putExtra(EXTRA_BACKGROUND_MODE, backgroundMode)
                    ContextCompat.startForegroundService(context, intent)
                } else {
                    context.stopService(intent)
                }
                true
            } catch (t: Throwable) {
                Log.w(TAG, "runtime service update failed", t)
                false
            }
        }
    }
}
