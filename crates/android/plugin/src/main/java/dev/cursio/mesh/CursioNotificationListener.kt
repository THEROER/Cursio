package dev.cursio.mesh

import android.app.ActivityOptions
import android.app.Notification
import android.app.NotificationManager
import android.app.RemoteInput
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.drawable.Drawable
import android.os.Build
import android.os.Bundle
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.util.Log
import androidx.core.app.NotificationManagerCompat
import java.io.ByteArrayOutputStream

/**
 * This phone's notifications, handed to the node for the desktop the user is
 * at (`crates/android/src/notify.rs`).
 *
 * Android binds this once the user grants notification access, and calls it
 * for every notification posted and removed. Not everything is passed on:
 *
 * - group summaries, which repeat the notifications they group;
 * - ongoing ones — a download, a running timer, music — except a call, which
 *   is ongoing for as long as it rings;
 * - silent ones (low importance), which the app itself thought not worth an
 *   interruption;
 * - Cursio's own, which would echo back to where they came from.
 *
 * What a desktop's user does comes back through [invoke] and [dismiss].
 */
class CursioNotificationListener : NotificationListenerService() {

    override fun onListenerConnected() {
        instance = this
    }

    override fun onListenerDisconnected() {
        if (instance === this) instance = null
    }

    override fun onNotificationPosted(sbn: StatusBarNotification) {
        if (!worthSending(sbn)) return
        try {
            send(sbn)
        } catch (e: Exception) {
            Log.w(TAG, "could not pass on a notification from ${sbn.packageName}", e)
        }
    }

    override fun onNotificationRemoved(sbn: StatusBarNotification) {
        if (sbn.packageName == packageName) return
        try {
            CursioNative.nativeNotificationRemoved(sbn.key)
        } catch (e: UnsatisfiedLinkError) {
            Log.w(TAG, "the node library is not loaded", e)
        }
    }

    private fun worthSending(sbn: StatusBarNotification): Boolean {
        val n = sbn.notification
        if (sbn.packageName == packageName) return false
        if (n.flags and Notification.FLAG_GROUP_SUMMARY != 0) return false
        val call = n.category == Notification.CATEGORY_CALL
        if (sbn.isOngoing && !call) return false
        val ranking = Ranking()
        if (currentRanking.getRanking(sbn.key, ranking) &&
            ranking.importance <= NotificationManager.IMPORTANCE_LOW && !call
        ) return false
        val extras = n.extras
        return !extras.getCharSequence(Notification.EXTRA_TITLE).isNullOrBlank() ||
            !extras.getCharSequence(Notification.EXTRA_TEXT).isNullOrBlank()
    }

    private fun send(sbn: StatusBarNotification) {
        val n = sbn.notification
        val extras = n.extras
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString().orEmpty()
        val text = (extras.getCharSequence(Notification.EXTRA_BIG_TEXT)
            ?: extras.getCharSequence(Notification.EXTRA_TEXT))?.toString().orEmpty()
        val kind = when {
            n.category == Notification.CATEGORY_CALL -> "call"
            n.category == Notification.CATEGORY_MESSAGE ||
                extras.containsKey(Notification.EXTRA_MESSAGES) -> "message"
            else -> "other"
        }
        val actions = n.actions.orEmpty()
        CursioNative.nativeNotificationPosted(
            sbn.key,
            sbn.packageName,
            appName(sbn.packageName),
            title,
            text,
            icon(sbn),
            n.`when`,
            kind,
            actions.map { it.title?.toString().orEmpty() }.toTypedArray(),
            actions.map { a -> a.remoteInputs?.any { it.allowFreeFormInput } == true }.toBooleanArray(),
        )
    }

    private fun appName(pkg: String): String = try {
        packageManager.getApplicationLabel(packageManager.getApplicationInfo(pkg, 0)).toString()
    } catch (e: Exception) {
        pkg
    }

    /**
     * The picture a desktop shows beside it: the sender's avatar when the app
     * gives one (a chat's large icon), otherwise the app's icon.
     */
    private fun icon(sbn: StatusBarNotification): ByteArray? = try {
        val drawable = sbn.notification.getLargeIcon()?.loadDrawable(this)
            ?: packageManager.getApplicationIcon(sbn.packageName)
        png(drawable)
    } catch (e: Exception) {
        null
    }

    private fun png(drawable: Drawable): ByteArray {
        val bitmap = Bitmap.createBitmap(ICON_PX, ICON_PX, Bitmap.Config.ARGB_8888)
        drawable.setBounds(0, 0, ICON_PX, ICON_PX)
        drawable.draw(Canvas(bitmap))
        return ByteArrayOutputStream().use { out ->
            bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
            bitmap.recycle()
            out.toByteArray()
        }
    }

    companion object {
        private const val TAG = "cursio"

        /** Desktop notifications draw an icon at 48–64 px; twice that stays sharp. */
        private const val ICON_PX = 96

        @Volatile
        private var instance: CursioNotificationListener? = null

        /** Press button [action] of [key], with [reply] typed into it if given. */
        fun invoke(key: String, action: Int, reply: String?) {
            val listener = instance ?: run {
                Log.w(TAG, "notification access is off; cannot press a button")
                return
            }
            val sbn = listener.activeNotifications.firstOrNull { it.key == key } ?: return
            val pressed = sbn.notification.actions?.getOrNull(action) ?: return
            val fill = Intent()
            val inputs = pressed.remoteInputs
            if (reply != null && inputs != null) {
                val results = Bundle()
                for (input in inputs) results.putCharSequence(input.resultKey, reply)
                RemoteInput.addResultsToIntent(inputs, fill, results)
            }
            // A button may open the app; Android 14 lets a notification
            // listener's PendingIntent do that only when it says so.
            val options = if (Build.VERSION.SDK_INT >= 34) {
                ActivityOptions.makeBasic()
                    .setPendingIntentBackgroundActivityStartMode(
                        ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED,
                    )
                    .toBundle()
            } else {
                null
            }
            pressed.actionIntent.send(listener, 0, fill, null, null, null, options)
        }

        /**
         * Ask Android to bind the listener again when access is granted but it
         * is not bound. A listener whose process the system killed (HyperOS
         * does, and an app update does) stays granted and unbound: Settings
         * shows it on, nothing arrives, and nothing tells the user to act.
         *
         * `requestRebind` does not help: it only undoes `requestUnbind`, and
         * is a no-op for a listener the system dropped. Turning the component
         * off and on is a package change, which makes Android rebind every
         * listener of the package.
         */
        fun rebindIfLost(context: Context) {
            if (instance != null) return
            if (!NotificationManagerCompat.getEnabledListenerPackages(context).contains(context.packageName)) return
            Log.i(TAG, "notification access is on but the listener is unbound; toggling it to rebind")
            val component = ComponentName(context, CursioNotificationListener::class.java)
            val pm = context.packageManager
            pm.setComponentEnabledSetting(
                component,
                PackageManager.COMPONENT_ENABLED_STATE_DISABLED,
                PackageManager.DONT_KILL_APP,
            )
            pm.setComponentEnabledSetting(
                component,
                PackageManager.COMPONENT_ENABLED_STATE_DEFAULT,
                PackageManager.DONT_KILL_APP,
            )
        }

        /** Dismiss [key], as if swiped away here. */
        fun dismiss(key: String) {
            instance?.cancelNotification(key)
        }
    }
}
