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
            if (activeSessionId(context) != null) {
                end(context)
                showUpdatedNotification(context)
            }
            return
        }

        val status = intent.getIntExtra(
            PackageInstaller.EXTRA_STATUS,
            PackageInstaller.STATUS_FAILURE,
        )
        // Abandoning an earlier session reports it as aborted, possibly
        // after the next session has started.
        val sessionId = intent.getIntExtra(EXTRA_UPDATE_SESSION, -1)
        if (sessionId != activeSessionId(context)) {
            Log.i(TAG, "Ignoring status $status of earlier update session $sessionId")
            return
        }
        when (status) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                @Suppress("DEPRECATION")
                val confirm = intent.getParcelableExtra<Intent>(Intent.EXTRA_INTENT)
                if (confirm != null) {
                    confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    // Kept so the title bar can show the prompt again if it
                    // ends up behind other apps or Android blocks it because
                    // Zdroid went to the background.
                    confirmation = confirm
                    context.startActivity(confirm)
                    return
                }
            }
            // The app is being replaced; MY_PACKAGE_REPLACED follows.
            PackageInstaller.STATUS_SUCCESS -> return
        }

        end(context)
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
        private const val EXTRA_UPDATE_SESSION = "com.zdroid.extra.UPDATE_SESSION"
        private const val PREFS_NAME = "zdroid-updater"
        private const val KEY_SESSION = "session"
        private const val CHANNEL_ID = "zdroid-updates"
        private const val NOTIFICATION_ID = 0x5a55

        @Volatile
        private var confirmation: Intent? = null

        /**
         * Makes [sessionId] the session whose results count, which also
         * marks the next `MY_PACKAGE_REPLACED` as an in-app update, and
         * returns where Android should report the session's status.
         */
        fun begin(context: Context, sessionId: Int): IntentSender {
            setActiveSessionId(context, sessionId)
            confirmation = null
            // Android adds the status extras, so the intent must stay mutable.
            val mutable = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                PendingIntent.FLAG_MUTABLE
            } else {
                0
            }
            return PendingIntent.getBroadcast(
                context,
                sessionId,
                Intent(context, UpdateInstallReceiver::class.java)
                    .putExtra(EXTRA_UPDATE_SESSION, sessionId),
                PendingIntent.FLAG_UPDATE_CURRENT or mutable,
            ).intentSender
        }

        /** Forgets the session in flight, so its results no longer count. */
        fun end(context: Context) {
            setActiveSessionId(context, null)
            confirmation = null
        }

        /** Android's prompt for the session in flight, once it has asked. */
        fun confirmation(): Intent? = confirmation

        // Stored rather than kept in memory: the session's result, and
        // MY_PACKAGE_REPLACED, can arrive in a new process.
        private fun activeSessionId(context: Context): Int? {
            val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            return if (prefs.contains(KEY_SESSION)) prefs.getInt(KEY_SESSION, -1) else null
        }

        private fun setActiveSessionId(context: Context, sessionId: Int?) {
            val editor = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE).edit()
            if (sessionId == null) {
                editor.remove(KEY_SESSION)
            } else {
                editor.putInt(KEY_SESSION, sessionId)
            }
            // `commit`, not `apply`: Android kills the app soon after.
            editor.commit()
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
