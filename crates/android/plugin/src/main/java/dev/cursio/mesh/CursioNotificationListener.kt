package dev.cursio.mesh

import android.app.ActivityOptions
import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
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
import android.telecom.TelecomManager
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
        val skip = skipReason(sbn)
        if (skip != null) {
            // Package and reason only: what the notification says stays here.
            Log.d(TAG, "skipping a notification from ${sbn.packageName}: $skip")
            return
        }
        Log.i(TAG, "forwarding a notification from ${sbn.packageName}${if (isCall(sbn.notification)) " (a call)" else ""}")
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

    /** Why [sbn] is not passed on, or null when it is. */
    private fun skipReason(sbn: StatusBarNotification): String? {
        val n = sbn.notification
        if (sbn.packageName == packageName) return "it is Cursio's own"
        if (n.flags and Notification.FLAG_GROUP_SUMMARY != 0) return "it is a group summary"
        val call = isCall(n)
        if (sbn.isOngoing && !call) return "it is ongoing"
        val ranking = Ranking()
        if (currentRanking.getRanking(sbn.key, ranking) &&
            ranking.importance <= NotificationManager.IMPORTANCE_LOW && !call
        ) return "it is silent"
        val extras = n.extras
        if (extras.getCharSequence(Notification.EXTRA_TITLE).isNullOrBlank() &&
            extras.getCharSequence(Notification.EXTRA_TEXT).isNullOrBlank()
        ) return "it has no text"
        return null
    }

    /**
     * Whether [n] is a call. Telegram's incoming call carries no call
     * category, only `CallStyle` and a full-screen intent on a channel named
     * for calls; reading the category alone sent it nowhere.
     */
    private fun isCall(n: Notification): Boolean {
        if (n.category == Notification.CATEGORY_CALL) return true
        if (n.extras.getString(Notification.EXTRA_TEMPLATE) == CALL_STYLE) return true
        return n.fullScreenIntent != null && n.channelId?.contains("call", ignoreCase = true) == true
    }

    /**
     * The phone's own dialer, whose calls the calls module already shows. Any
     * other app's call is a messenger's, which only this listener sees.
     */
    private fun isTelephony(pkg: String): Boolean {
        val telecom = getSystemService(TelecomManager::class.java)
        return pkg == telecom?.defaultDialerPackage ||
            pkg == telecom?.systemDialerPackage ||
            pkg in IN_CALL_UIS
    }

    /**
     * Positions among [Notification.actions] of the buttons that answer,
     * decline and end a call, -1 for none. `CallStyle` names its buttons'
     * intents (Android 12+), which is the only language-free way to tell them
     * apart; an action whose intent is the same is that button.
     */
    private fun callRoles(n: Notification): IntArray {
        val roles = intArrayOf(-1, -1, -1)
        if (Build.VERSION.SDK_INT < 31) return roles
        val extras = n.extras
        val intents = listOf(
            Notification.EXTRA_ANSWER_INTENT,
            Notification.EXTRA_DECLINE_INTENT,
            Notification.EXTRA_HANG_UP_INTENT,
        ).map { name ->
            if (Build.VERSION.SDK_INT >= 33) {
                extras.getParcelable(name, PendingIntent::class.java)
            } else {
                @Suppress("DEPRECATION")
                extras.getParcelable<PendingIntent>(name)
            }
        }
        n.actions.orEmpty().forEachIndexed { index, action ->
            intents.forEachIndexed { role, intent ->
                if (intent != null && intent == action.actionIntent) roles[role] = index
            }
        }
        return roles
    }

    private fun send(sbn: StatusBarNotification) {
        val n = sbn.notification
        val extras = n.extras
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString().orEmpty()
        val text = (extras.getCharSequence(Notification.EXTRA_BIG_TEXT)
            ?: extras.getCharSequence(Notification.EXTRA_TEXT))?.toString().orEmpty()
        val kind = when {
            isCall(n) && isTelephony(sbn.packageName) -> "dialer_call"
            isCall(n) -> "call"
            n.category == Notification.CATEGORY_MESSAGE ||
                extras.containsKey(Notification.EXTRA_MESSAGES) -> "message"
            else -> "other"
        }
        val actions = n.actions.orEmpty()
        val roles = if (kind == "call") callRoles(n) else intArrayOf(-1, -1, -1)
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
            roles[0],
            roles[1],
            roles[2],
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

        private const val CALL_STYLE = "android.app.Notification\$CallStyle"

        /**
         * In-call screens that post the telephony call's notification under
         * their own package rather than the dialer's (Xiaomi, Samsung, AOSP).
         */
        private val IN_CALL_UIS = setOf(
            "com.android.incallui",
            "com.xiaomi.incallui",
            "com.samsung.android.incallui",
            "com.android.server.telecom",
            "com.android.phone",
        )

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
