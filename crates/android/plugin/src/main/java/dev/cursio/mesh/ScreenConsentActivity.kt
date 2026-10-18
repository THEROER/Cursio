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
import android.util.Log

/**
 * Android's "start recording or casting?" for showing this screen on a
 * computer, from wherever the user was when it was asked for.
 *
 * Its own invisible activity because the consent is an activity result, and a
 * computer's request arrives with no window of Cursio's open: the request
 * becomes a notification ([ask]), and tapping its button opens this, which
 * shows the system dialog over whatever the user is doing and goes away.
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

        /**
         * A computer asked to see this screen: say so, with a button that
         * opens the consent. Never the consent itself — an app in the
         * background may not start an activity, and a dialog appearing
         * unasked would be the wrong thing to answer by reflex anyway.
         */
        fun ask(context: Context, computer: String) {
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
