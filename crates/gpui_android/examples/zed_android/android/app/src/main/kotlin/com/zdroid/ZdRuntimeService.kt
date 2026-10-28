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

/// Foreground service that holds the app process out of Android's cached
/// state while the editor has child processes (language servers, terminal
/// shells, debug adapters). An app in the cached state has its children
/// stopped by the phantom process killer and is itself an early pick for
/// the low-memory killer; a foreground service is the one signal that
/// says "this app is doing work the user asked for".
///
/// The service has no logic of its own. Rust reads the process tree and
/// pushes start/stop through `MainActivity.setRuntimeServiceActive`
/// (`gpui_android::runtime_service`). `keepAwake` adds a partial wake
/// lock for the service's lifetime so builds continue with the screen
/// off; it mirrors the `android_runtime.keep_awake` setting.
class ZdRuntimeService : Service() {
    private var wakeLock: PowerManager.WakeLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        ensureChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            releaseWakeLock()
            ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }
        val notification = buildNotification()
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
        // Not sticky: the Rust side restarts the service from the process
        // tree after a process death; a system restart with no editor
        // process behind it would only show an orphan notification.
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        releaseWakeLock()
        super.onDestroy()
    }

    private fun ensureChannel() {
        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (manager.getNotificationChannel(CHANNEL_ID) != null) return
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Runtime",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "Shown while language servers or terminals keep running in the background"
            setShowBadge(false)
        }
        manager.createNotificationChannel(channel)
    }

    private fun buildNotification(): Notification {
        val launch = packageManager.getLaunchIntentForPackage(packageName)
        val tap = launch?.let {
            PendingIntent.getActivity(this, 0, it, PendingIntent.FLAG_IMMUTABLE)
        }
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_runtime)
            .setContentTitle("Zdroid runtime is running")
            .setContentText("Language servers and terminals stay alive in the background")
            .setOngoing(true)
            .setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .apply { if (tap != null) setContentIntent(tap) }
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
            // No timeout: the lock lives exactly as long as the service,
            // which Rust stops once the last child is gone.
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
        const val ACTION_START = "com.zdroid.runtime.START"
        const val ACTION_STOP = "com.zdroid.runtime.STOP"
        const val EXTRA_KEEP_AWAKE = "keep_awake"
        private const val CHANNEL_ID = "runtime"
        private const val NOTIFICATION_ID = 1
        private const val WAKE_LOCK_TAG = "zdroid:runtime"
        private const val TAG = "zed_android_runtime"
    }
}
