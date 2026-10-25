package com.zdroid

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat

/**
 * Opt-in background mode. While it runs, the process holds foreground
 * service priority, so Android is far less likely to freeze or kill Zdroid
 * while it sits in the background, and SSH or agent sessions keep running
 * when the user switches apps. It holds no wake lock, so with the screen
 * off battery optimization can still cut the network; setting the app's
 * battery usage to Unrestricted avoids that. The notification's Stop
 * action turns the mode off.
 *
 * The service lives in the app process, so quitting Zdroid ends it too.
 * The user's choice is persisted and re-applied on the next launch.
 */
class KeepAliveService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            setEnabled(this, false)
            return START_NOT_STICKY
        }
        startForeground(NOTIFICATION_ID, buildNotification())
        // The app re-applies the persisted choice on launch, so the system
        // shouldn't restart the service on its own after the process dies.
        return START_NOT_STICKY
    }

    private fun buildNotification(): android.app.Notification {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                "Background mode",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Shown while Zdroid keeps running in the background"
            }
        )
        val openApp = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val stop = PendingIntent.getService(
            this,
            0,
            Intent(this, KeepAliveService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_launcher_foreground)
            .setContentTitle("Zdroid is running in the background")
            .setContentText("Terminal, SSH and agent sessions stay connected.")
            .setContentIntent(openApp)
            .addAction(0, "Stop", stop)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
    }

    companion object {
        private const val CHANNEL_ID = "zdroid-background-mode"
        private const val NOTIFICATION_ID = 0x5a44
        private const val ACTION_STOP = "com.zdroid.action.STOP_BACKGROUND_MODE"
        private const val PREFS_NAME = "zdroid-background-mode"
        private const val KEY_ENABLED = "enabled"

        fun isEnabled(context: Context): Boolean =
            context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .getBoolean(KEY_ENABLED, false)

        fun setEnabled(context: Context, enabled: Boolean) {
            context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .edit()
                .putBoolean(KEY_ENABLED, enabled)
                .apply()
            val intent = Intent(context, KeepAliveService::class.java)
            if (enabled) {
                ContextCompat.startForegroundService(context, intent)
            } else {
                context.stopService(intent)
            }
        }

        fun startIfEnabled(context: Context) {
            if (isEnabled(context)) {
                ContextCompat.startForegroundService(
                    context,
                    Intent(context, KeepAliveService::class.java),
                )
            }
        }
    }
}
