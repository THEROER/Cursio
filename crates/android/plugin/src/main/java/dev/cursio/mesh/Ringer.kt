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
import android.media.AudioAttributes
import android.media.AudioManager
import android.media.MediaPlayer
import android.media.RingtoneManager
import android.net.Uri
import android.os.Build
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
        val notification = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notify)
            .setContentTitle("Your computer is looking for this phone")
            .setContentText("Unlock it or tap here to stop the ringing")
            .setCategory(Notification.CATEGORY_ALARM)
            .setContentIntent(stopIntent)
            .setDeleteIntent(stopIntent)
            .addAction(Notification.Action.Builder(null, "Found it", stopIntent).build())
            .build()
        manager.notify(NOTIFICATION_ID, notification)
    }
}
