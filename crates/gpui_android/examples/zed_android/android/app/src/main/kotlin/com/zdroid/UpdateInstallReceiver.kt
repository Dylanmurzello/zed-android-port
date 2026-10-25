package com.zdroid

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentSender
import android.content.pm.PackageInstaller
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationCompat

/**
 * Receives the result of the update session [MainActivity.installUpdate]
 * commits, and `MY_PACKAGE_REPLACED` once an update has installed.
 *
 * Android asks the user to confirm the session, then kills the app to
 * replace it. Unlike an intent install, nothing offers to open the new
 * version afterwards, so a notification does, in the role of desktop
 * Zed's "Restart to Update".
 */
class UpdateInstallReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == Intent.ACTION_MY_PACKAGE_REPLACED) {
            if (takeUpdating(context)) {
                showUpdatedNotification(context)
            }
            return
        }

        val status = intent.getIntExtra(
            PackageInstaller.EXTRA_STATUS,
            PackageInstaller.STATUS_FAILURE,
        )
        when (status) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                @Suppress("DEPRECATION")
                val confirm = intent.getParcelableExtra<Intent>(Intent.EXTRA_INTENT)
                if (confirm != null) {
                    context.startActivity(confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                    return
                }
            }
            // The app is being replaced; MY_PACKAGE_REPLACED follows.
            PackageInstaller.STATUS_SUCCESS -> return
        }

        takeUpdating(context)
        val message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)
        try {
            NativeBridge.nativeOnUpdateInstallResult(status, message)
        } catch (e: UnsatisfiedLinkError) {
            // The process restarted since the session began, so no updater is waiting.
            Log.w(TAG, "Update session ended with status $status ($message)")
        }
    }

    companion object {
        private const val TAG = "zed_android_update"
        private const val PREFS_NAME = "zdroid-updater"
        private const val KEY_UPDATING = "updating"
        private const val CHANNEL_ID = "zdroid-updates"
        private const val NOTIFICATION_ID = 0x5a55

        fun statusSender(context: Context, sessionId: Int): IntentSender {
            // Android adds the status extras, so the intent must stay mutable.
            val mutable = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                PendingIntent.FLAG_MUTABLE
            } else {
                0
            }
            return PendingIntent.getBroadcast(
                context,
                sessionId,
                Intent(context, UpdateInstallReceiver::class.java),
                PendingIntent.FLAG_UPDATE_CURRENT or mutable,
            ).intentSender
        }

        /** Marks the next `MY_PACKAGE_REPLACED` as an in-app update. */
        fun setUpdating(context: Context, updating: Boolean) {
            // `commit`, not `apply`: Android kills the app soon after.
            context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .edit()
                .putBoolean(KEY_UPDATING, updating)
                .commit()
        }

        private fun takeUpdating(context: Context): Boolean {
            val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            val updating = prefs.getBoolean(KEY_UPDATING, false)
            if (updating) {
                prefs.edit().remove(KEY_UPDATING).apply()
            }
            return updating
        }

        private fun showUpdatedNotification(context: Context) {
            val manager = context.getSystemService(NotificationManager::class.java)
            manager.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    "Updates",
                    NotificationManager.IMPORTANCE_DEFAULT,
                ).apply {
                    description = "Shown after Zdroid updates itself"
                }
            )
            val version = try {
                context.packageManager.getPackageInfo(context.packageName, 0).versionName
            } catch (t: Throwable) {
                null
            }
            val open = PendingIntent.getActivity(
                context,
                0,
                Intent(context, MainActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            val notification = NotificationCompat.Builder(context, CHANNEL_ID)
                .setSmallIcon(R.drawable.ic_launcher_foreground)
                .setContentTitle(
                    if (version != null) "Updated to Zdroid $version" else "Zdroid updated"
                )
                .setContentText("Tap to open Zdroid.")
                .setContentIntent(open)
                .setAutoCancel(true)
                .build()
            manager.notify(NOTIFICATION_ID, notification)
        }
    }
}
