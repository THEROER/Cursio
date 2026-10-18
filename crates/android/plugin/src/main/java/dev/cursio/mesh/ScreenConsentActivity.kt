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
 * computer's request arrives with no window of Cursio's open. The question
 * is asked once, in whichever place the user is ([ask]):
 * - Cursio's own window, when it is in front: a sheet with Allow, Decline and
 *   "don't ask again" for that computer;
 * - otherwise a notification with Allow and Always allow, and — with no
 *   consent kept, the screen on, and Android letting the app start an
 *   activity from the background (a CompanionDeviceManager association, or
 *   drawing over other apps) — the system's dialog at once, since MIUI does
 *   not pop a notification up unless the user turned that on per app.
 *
 * An answer opens this. Before Android 14 one system consent may start any
 * number of captures, so it is kept for the process's life and an allowed
 * request starts at once; a computer always allowed is not asked at all.
 * From 14 every capture needs the system's own dialog, so an allowed request
 * goes straight to it.
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
        // Opened only as an answer — a tap on the notification, "Allow" in
        // the window, or a computer always allowed — so a consent kept from
        // earlier is used without asking the system again.
        val computer = intent.getStringExtra(EXTRA_COMPUTER)
        if (intent.getBooleanExtra(EXTRA_ALWAYS, false) && computer != null) trust(this, computer)
        val keptConsent = kept
        if (keptConsent != null && computer != null) {
            getSystemService(NotificationManager::class.java).cancel(PROMPT_ID)
            startKept(this, keptConsent, computer)
            finish()
            return
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
            if (trusted(context, computer)) {
                if (consent != null && startKept(context, consent, computer)) return
                // Android wants its own dialog for this capture: straight to it.
                prompt(context, computer)
                if (mayOpenNow(context)) open(context, answer(context, computer))
                return
            }
            if (MeshPlugin.offerScreen(computer, kept = consent != null)) return
            prompt(context, computer)
            // A kept consent would start the capture with no question at all,
            // so with one the notification is the question.
            if (consent == null && mayOpenNow(context)) open(context, answer(context, computer))
        }

        private fun open(context: Context, intent: Intent) {
            try {
                context.startActivity(intent)
            } catch (e: RuntimeException) {
                Log.i(TAG, "the screen consent could not open by itself; the notification stays", e)
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
                answer(context, computer),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            val always = PendingIntent.getActivity(
                context,
                1,
                answer(context, computer, always = true),
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
                .addAction(Notification.Action.Builder(null, "Always allow", always).build())
                .build()
            manager.notify(PROMPT_ID, notification)
        }
    }
}
