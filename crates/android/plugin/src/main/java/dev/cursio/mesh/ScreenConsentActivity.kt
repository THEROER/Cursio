package dev.cursio.mesh

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import android.util.Log

/**
 * Android's "start recording or casting?" for showing this screen on a
 * computer, from wherever the user was when it was asked for.
 *
 * Its own invisible activity because the consent is an activity result, and a
 * computer's request arrives with no window of Cursio's open. How much of it
 * the user sees depends on what Android allows ([ask]):
 * - before Android 14 one consent may start any number of projections, so
 *   after the first "Start now" a computer asking again is simply shown the
 *   screen, until the app's process ends;
 * - from 14 each projection needs its own consent, and the system dialog
 *   opens on the phone at once when the screen is on and the app may start
 *   an activity from the background — which an app associated with a
 *   computer through CompanionDeviceManager may, as may one allowed to draw
 *   over other apps;
 * - otherwise, and always alongside, a notification whose button opens it:
 *   a request that arrived while the phone lay in a pocket is still there
 *   when it comes out, and a start Android silently refused leaves the user
 *   something to tap.
 */
class ScreenConsentActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val manager = getSystemService(MediaProjectionManager::class.java)
        // The whole display: a computer showing one app would lose it the
        // moment the user switched.
        val consent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            manager.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            manager.createScreenCaptureIntent()
        }
        try {
            @Suppress("DEPRECATION")
            startActivityForResult(consent, REQUEST)
        } catch (e: ActivityNotFoundException) {
            Log.w(TAG, "no screen-capture consent dialog", e)
            finish()
        }
    }

    @Deprecated("the platform's own result path; this activity has no fragments")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        getSystemService(NotificationManager::class.java).cancel(PROMPT_ID)
        if (requestCode == REQUEST && resultCode == RESULT_OK && data != null) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) kept = Intent(data)
            val everyone = intent.getBooleanExtra(EXTRA_EVERYONE, false)
            try {
                MeshService.startScreen(this, resultCode, data, everyone)
            } catch (e: IllegalStateException) {
                Log.w(TAG, "Android refused to start showing the screen", e)
            }
        } else {
            Log.i(TAG, "showing this screen on a computer was declined")
        }
        finish()
    }

    companion object {
        private const val TAG = "Cursio"
        private const val REQUEST = 0x5343
        private const val CHANNEL_ID = "cursio-screen-ask"
        private const val PROMPT_ID = 0x53435245
        const val EXTRA_EVERYONE = "dev.cursio.mesh.SCREEN_EVERYONE"

        /** On a start from a kept consent: the computer to ask again for if it fails. */
        const val EXTRA_REUSED_FOR = "dev.cursio.mesh.SCREEN_REUSED_FOR"

        /** The last consent given, before Android 14, for the next request. */
        @Volatile
        private var kept: Intent? = null

        /** The application context, set before the node can ask anything. */
        @Volatile
        var context: Context? = null

        /** Open the consent from a window, to show the screen on every computer. */
        fun open(context: Context) {
            context.startActivity(
                Intent(context, ScreenConsentActivity::class.java)
                    .putExtra(EXTRA_EVERYONE, true)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }

        /** A computer asked to see this screen; see the class for what follows. */
        fun ask(context: Context, computer: String) {
            val consent = kept
            if (consent != null) {
                try {
                    MeshService.startScreen(context, RESULT_OK, consent, everyone = false, reusedFor = computer)
                    return
                } catch (e: IllegalStateException) {
                    Log.w(TAG, "cannot start showing the screen from the kept consent", e)
                }
            }
            prompt(context, computer)
            if (mayOpenNow(context)) {
                try {
                    context.startActivity(
                        Intent(context, ScreenConsentActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                    )
                } catch (e: RuntimeException) {
                    Log.i(TAG, "the screen consent could not open by itself; the notification stays", e)
                }
            }
        }

        /** The kept consent was refused when used: forget it and ask anew. */
        fun consentRefused(context: Context, computer: String) {
            kept = null
            ask(context, computer)
        }

        /**
         * Only with the screen on: a dialog that pops up on a phone nobody is
         * looking at is one nobody answers, and it would still be waiting,
         * unexplained, when they next did.
         */
        private fun mayOpenNow(context: Context): Boolean {
            val power = context.getSystemService(PowerManager::class.java)
            if (power?.isInteractive != true) return false
            return ComputerCompanion.associated(context) || Settings.canDrawOverlays(context)
        }

        private fun prompt(context: Context, computer: String) {
            val manager = context.getSystemService(NotificationManager::class.java)
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL_ID, "Screen requests", NotificationManager.IMPORTANCE_HIGH).apply {
                    description = "A computer asking to show this phone's screen"
                },
            )
            val open = PendingIntent.getActivity(
                context,
                0,
                Intent(context, ScreenConsentActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            val notification = Notification.Builder(context, CHANNEL_ID)
                .setSmallIcon(R.drawable.ic_notify)
                .setContentTitle("$computer wants to show this screen")
                .setContentText("Tap to allow; you can control the phone from $computer")
                .setCategory(Notification.CATEGORY_RECOMMENDATION)
                .setContentIntent(open)
                .setAutoCancel(true)
                .addAction(Notification.Action.Builder(null, "Allow", open).build())
                .build()
            manager.notify(PROMPT_ID, notification)
        }
    }
}
