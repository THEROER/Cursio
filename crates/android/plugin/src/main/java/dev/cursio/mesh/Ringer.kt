package dev.cursio.mesh

import android.app.KeyguardManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.graphics.drawable.Icon
import android.media.AudioAttributes
import android.media.AudioManager
import android.media.MediaPlayer
import android.media.RingtoneManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.VibrationEffect
import android.os.Vibrator
import android.util.Log

/**
 * Rings this phone loudly so it can be found, at a desktop's request
 * (`NotifyMsg::Ring`, through `CursioNative.onRing`).
 *
 * Loud on purpose, and past silent mode: it plays as an *alarm*
 * (`USAGE_ALARM`), which the ringer switch does not mute and Do Not Disturb
 * lets through unless set to total silence, with the alarm stream raised to
 * its maximum for as long as it rings and put back afterwards.
 *
 * It stops the way a phone that has been found is handled:
 *  - unlocking it (`ACTION_USER_PRESENT`) — the hand that picked it up. Some
 *    firmwares never send that without a secure lock screen, so there the
 *    screen coming on (`ACTION_SCREEN_ON`) counts instead. A phone already
 *    unlocked when the ring arrives is not stopped by being so: only the
 *    *next* unlock counts, which is what these broadcasts report anyway;
 *  - the notification: its "Found it" button, a tap, or swiping it away;
 *  - a volume key. A background app cannot see key presses, so this watches
 *    the alarm volume change instead (`VOLUME_CHANGED_ACTION`, which every
 *    Android sends but the SDK does not name);
 *  - by itself after [TIMEOUT_MS], because a phone that is truly lost may
 *    never be reached again to be told to stop.
 *
 * Where it shows: everywhere a heads-up does (a high-importance channel), and
 * on a phone with an island in the island too, so a phone someone is already
 * holding says what the noise is and offers the button without pulling the
 * shade down. Two ways in, because neither covers both cases: HyperOS 3.0
 * draws only its own Focus notifications there ([Island]), while Android 16's
 * Live Updates (`setRequestPromotedOngoing`) are what HyperOS 3.1 and other
 * skins put in theirs. Both are plain notification extras; a system that knows
 * neither shows the ordinary heads-up.
 */
object Ringer {
    private const val TAG = "Cursio"
    private const val CHANNEL_ID = "cursio-ring"
    private const val NOTIFICATION_ID = 0x52494e47
    private const val ACTION_STOP = "dev.cursio.mesh.RING_STOP"
    private const val TIMEOUT_MS = 30_000L
    private const val VOLUME_CHANGED = "android.media.VOLUME_CHANGED_ACTION"
    private const val EXTRA_STREAM = "android.media.EXTRA_VOLUME_STREAM_TYPE"
    private const val EXTRA_VALUE = "android.media.EXTRA_VOLUME_STREAM_VALUE"

    /** The application context, set before the node can deliver anything. */
    @Volatile
    var context: Context? = null

    private val main = Handler(Looper.getMainLooper())

    // Everything below is touched on the main thread only.
    private var player: MediaPlayer? = null
    private var vibrator: Vibrator? = null
    private var receiver: BroadcastReceiver? = null
    private var restoreVolume: Int? = null
    private var raisedTo: Int = -1
    private val timeout = Runnable { stop("nobody found it in time") }

    /** Called by the node on its own thread. */
    fun ring(on: Boolean) {
        main.post { if (on) start() else stop("the desktop stopped it") }
    }

    private fun start() {
        val ctx = context ?: run {
            Log.w(TAG, "asked to ring before the app was initialised")
            return
        }
        main.removeCallbacks(timeout)
        main.postDelayed(timeout, TIMEOUT_MS)
        if (player != null) return

        val audio = ctx.getSystemService(AudioManager::class.java)
        restoreVolume = audio.getStreamVolume(AudioManager.STREAM_ALARM)
        raisedTo = audio.getStreamMaxVolume(AudioManager.STREAM_ALARM)
        try {
            audio.setStreamVolume(AudioManager.STREAM_ALARM, raisedTo, 0)
        } catch (e: SecurityException) {
            // Raising a stream is refused under Do Not Disturb on some
            // versions; it still rings at the volume the user left.
            Log.w(TAG, "could not raise the alarm volume: ${e.message}")
        }

        player = try {
            MediaPlayer().apply {
                setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_ALARM)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                        .build(),
                )
                setDataSource(ctx, sound(ctx))
                isLooping = true
                prepare()
                start()
            }
        } catch (e: Exception) {
            Log.w(TAG, "could not play the ring: ${e.message}")
            null
        }

        vibrator = ctx.getSystemService(Vibrator::class.java)?.also {
            it.vibrate(VibrationEffect.createWaveform(longArrayOf(0, 800, 400), 0))
        }

        listen(ctx)
        show(ctx)
        Log.i(TAG, "ringing until found")
    }

    private fun stop(why: String) {
        main.removeCallbacks(timeout)
        val ctx = context
        val wasRinging = player != null || vibrator != null || receiver != null
        player?.run {
            runCatching { stop() }
            release()
        }
        player = null
        vibrator?.cancel()
        vibrator = null
        if (ctx != null) {
            receiver?.let { runCatching { ctx.unregisterReceiver(it) } }
            restoreVolume?.let { volume ->
                runCatching {
                    ctx.getSystemService(AudioManager::class.java)
                        .setStreamVolume(AudioManager.STREAM_ALARM, volume, 0)
                }
            }
            ctx.getSystemService(NotificationManager::class.java).cancel(NOTIFICATION_ID)
        }
        receiver = null
        restoreVolume = null
        if (wasRinging) Log.i(TAG, "stopped ringing: $why")
    }

    /** The alarm sound the user chose, else the ringtone, else any sound. */
    private fun sound(ctx: Context): Uri =
        listOf(RingtoneManager.TYPE_ALARM, RingtoneManager.TYPE_RINGTONE, RingtoneManager.TYPE_NOTIFICATION)
            .firstNotNullOfOrNull { RingtoneManager.getActualDefaultRingtoneUri(ctx, it) }
            ?: RingtoneManager.getDefaultUri(RingtoneManager.TYPE_ALARM)

    private fun listen(ctx: Context) {
        val filter = IntentFilter().apply {
            addAction(ACTION_STOP)
            addAction(Intent.ACTION_USER_PRESENT)
            addAction(VOLUME_CHANGED)
            // Without a secure lock screen some firmwares never report the
            // unlock; the screen coming on is then the closest sign of a hand.
            val secure = ctx.getSystemService(KeyguardManager::class.java).isKeyguardSecure
            if (!secure) addAction(Intent.ACTION_SCREEN_ON)
        }
        receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                when (intent.action) {
                    ACTION_STOP -> stop("found it")
                    Intent.ACTION_USER_PRESENT -> stop("the phone was unlocked")
                    Intent.ACTION_SCREEN_ON -> stop("the screen was turned on")
                    VOLUME_CHANGED -> {
                        val stream = intent.getIntExtra(EXTRA_STREAM, -1)
                        val value = intent.getIntExtra(EXTRA_VALUE, raisedTo)
                        // Our own raise arrives here too, at exactly the value
                        // we set; anything else is a hand on the volume key.
                        if (stream == AudioManager.STREAM_ALARM && value != raisedTo) {
                            stop("a volume key was pressed")
                        }
                    }
                }
            }
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            ctx.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            ctx.registerReceiver(receiver, filter)
        }
    }

    private fun show(ctx: Context) {
        val manager = ctx.getSystemService(NotificationManager::class.java)
        // High importance so it is the heads-up a person looking for the phone
        // sees first; silent itself, because the ring is the sound.
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Find my phone", NotificationManager.IMPORTANCE_HIGH).apply {
                setSound(null, null)
                enableVibration(false)
            },
        )
        val stopIntent = PendingIntent.getBroadcast(
            ctx,
            0,
            Intent(ACTION_STOP).setPackage(ctx.packageName),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val found = Notification.Action.Builder(
            Icon.createWithResource(ctx, R.drawable.ic_notify),
            "Found it",
            stopIntent,
        ).build()
        val builder = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notify)
            .setContentTitle(TITLE)
            .setContentText("Unlock it or tap here to stop the ringing")
            .setStyle(Notification.BigTextStyle().bigText("Unlock it or tap here to stop the ringing"))
            .setCategory(Notification.CATEGORY_ALARM)
            // Ongoing so it stays in the island and on top for as long as it
            // rings; "Found it", a tap and the timeout end it instead of a swipe.
            .setOngoing(true)
            .setTimeoutAfter(TIMEOUT_MS)
            .setContentIntent(stopIntent)
            .setDeleteIntent(stopIntent)
            .addAction(found)
        val island = Island.supported(ctx)
        if (!island) {
            // MIUI floats no third-party heads-up unless the user found the
            // per-app switch; a full-screen intent is the one it floats anyway,
            // and on an unlocked screen Android shows it as a heads-up rather
            // than opening anything. Not on the island path: that one already
            // has the place a heads-up would take.
            ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.let { launch ->
                val open = PendingIntent.getActivity(
                    ctx,
                    1,
                    launch,
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                )
                builder.setFullScreenIntent(open, true)
            }
        }
        if (island) {
            // Not also a Live Update: HyperOS renders one of those itself at
            // the lowest island priority, and a hotspot's island then hides it.
            builder.addExtras(Island.extras(ctx, TITLE, found))
        } else if (Build.VERSION.SDK_INT >= 36) {
            // Android 16's Live Update: the status-bar chip, and the island on
            // skins that draw these there. The extra is what
            // `setRequestPromotedOngoing` sets, which compileSdk 36 lacks.
            builder.addExtras(Bundle().apply { putBoolean("android.requestPromotedOngoing", true) })
        }
        manager.notify(NOTIFICATION_ID, builder.build())
    }

    private const val TITLE = "Your computer is looking for this phone"
}
