package dev.cursio.mesh

import android.content.ComponentName
import android.content.Context
import android.graphics.Bitmap
import android.media.MediaMetadata
import android.media.session.MediaController
import android.media.session.MediaSessionManager
import android.media.session.PlaybackState
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.ByteArrayOutputStream

/**
 * This phone's players, for the computers' media widgets
 * (`crates/android/src/media.rs`).
 *
 * Android lists other apps' media sessions only to the holder of notification
 * access, named by its listener component — which is why this needs
 * [CursioNotificationListener] granted and does nothing until it is. Each
 * controller's callback pushes the list down again, but only when something a
 * remote control shows changed: playback state callbacks also fire as the
 * position ticks, and resending the list for those would make a playing track
 * a stream. A position is "this far, at this moment"; the computer advances it.
 */
object MediaBridge {
    private const val TAG = "Cursio"
    /** A cover sent is at most this many pixels on its long side. */
    private const val ART_EDGE = 512

    private val main = Handler(Looper.getMainLooper())
    @Volatile
    private var controllers: List<MediaController> = emptyList()
    private val callbacks = HashMap<MediaController, MediaController.Callback>()
    private var lastKey = ""
    private var lastPositions = emptyMap<String, Pair<Long, Long>>()
    private var watching = false

    /** Start listening for sessions; safe to call again once access is granted. */
    fun watch(ctx: Context) {
        main.post { start(ctx.applicationContext) }
    }

    private fun start(ctx: Context) {
        if (watching) return
        val manager = ctx.getSystemService(MediaSessionManager::class.java) ?: return
        val listener = ComponentName(ctx, CursioNotificationListener::class.java)
        try {
            manager.addOnActiveSessionsChangedListener({ list -> follow(ctx, list.orEmpty()) }, listener, main)
            follow(ctx, manager.getActiveSessions(listener))
            watching = true
        } catch (e: SecurityException) {
            Log.i(TAG, "players are not shared until notification access is granted")
        }
    }

    private fun follow(ctx: Context, list: List<MediaController>) {
        for ((controller, callback) in callbacks) controller.unregisterCallback(callback)
        callbacks.clear()
        // Our own sessions are the computers' players; sent back, they would
        // appear on the computer they came from.
        val others = list.filter { it.packageName != ctx.packageName }
        for (controller in others) {
            val callback = object : MediaController.Callback() {
                override fun onPlaybackStateChanged(state: PlaybackState?) = push(ctx)
                override fun onMetadataChanged(metadata: MediaMetadata?) = push(ctx)
                override fun onSessionDestroyed() = push(ctx)
            }
            controller.registerCallback(callback, main)
            callbacks[controller] = callback
        }
        controllers = others
        push(ctx, force = true)
    }

    private fun id(c: MediaController) = "${c.packageName}#${Integer.toHexString(c.sessionToken.hashCode())}"

    private fun appName(ctx: Context, pkg: String): String = try {
        val pm = ctx.packageManager
        pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)).toString()
    } catch (e: Exception) {
        pkg
    }

    private fun position(state: PlaybackState?): Long? {
        state ?: return null
        if (state.position < 0) return null
        if (state.state != PlaybackState.STATE_PLAYING) return state.position
        val elapsed = SystemClock.elapsedRealtime() - state.lastPositionUpdateTime
        return state.position + (elapsed * state.playbackSpeed).toLong()
    }

    private fun artKey(meta: MediaMetadata?): String? {
        val bitmap = cover(meta) ?: return null
        val seed = "${meta?.getString(MediaMetadata.METADATA_KEY_ALBUM)}|${meta?.getString(MediaMetadata.METADATA_KEY_TITLE)}|${bitmap.width}x${bitmap.height}"
        return java.lang.Long.toUnsignedString(seed.hashCode().toLong() and 0xffffffffL)
    }

    private fun cover(meta: MediaMetadata?): Bitmap? =
        meta?.getBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART)
            ?: meta?.getBitmap(MediaMetadata.METADATA_KEY_ART)
            ?: meta?.getBitmap(MediaMetadata.METADATA_KEY_DISPLAY_ICON)

    private fun push(ctx: Context, force: Boolean = false) {
        val list = JSONArray()
        val keyParts = StringBuilder()
        val positions = HashMap<String, Pair<Long, Long>>()
        var jumped = false
        val now = SystemClock.elapsedRealtime()
        for (c in controllers) {
            val meta = c.metadata
            val state = c.playbackState
            val actions = state?.actions ?: 0L
            val playing = state?.state == PlaybackState.STATE_PLAYING
            val stateName = when (state?.state) {
                PlaybackState.STATE_PLAYING, PlaybackState.STATE_BUFFERING -> "playing"
                PlaybackState.STATE_PAUSED -> "paused"
                else -> "stopped"
            }
            val title = meta?.getString(MediaMetadata.METADATA_KEY_TITLE).orEmpty()
            if (title.isEmpty() && stateName == "stopped") continue
            val id = id(c)
            val obj = JSONObject()
                .put("id", id)
                .put("app", appName(ctx, c.packageName))
                .put("title", title)
                .put("artist", meta?.getString(MediaMetadata.METADATA_KEY_ARTIST).orEmpty())
                .put("album", meta?.getString(MediaMetadata.METADATA_KEY_ALBUM).orEmpty())
                .put("state", stateName)
                .put("canPlay", actions and (PlaybackState.ACTION_PLAY or PlaybackState.ACTION_PLAY_PAUSE) != 0L)
                .put("canPause", actions and (PlaybackState.ACTION_PAUSE or PlaybackState.ACTION_PLAY_PAUSE) != 0L)
                .put("canNext", actions and PlaybackState.ACTION_SKIP_TO_NEXT != 0L)
                .put("canPrevious", actions and PlaybackState.ACTION_SKIP_TO_PREVIOUS != 0L)
                .put("canSeek", actions and PlaybackState.ACTION_SEEK_TO != 0L)
            meta?.getLong(MediaMetadata.METADATA_KEY_DURATION)?.takeIf { it > 0 }?.let { obj.put("durationMs", it) }
            val pos = position(state)
            pos?.let { obj.put("positionMs", it) }
            artKey(meta)?.let { obj.put("art", it) }
            list.put(obj)
            keyParts.append(obj.toString().replace(Regex("\"positionMs\":\\d+,?"), ""))
            if (pos != null) {
                positions[id] = pos to now
                val before = lastPositions[id]
                if (before != null) {
                    val expected = before.first + if (playing) now - before.second else 0
                    if (kotlin.math.abs(expected - pos) > 3_000) jumped = true
                }
            }
        }
        val key = keyParts.toString()
        if (!force && key == lastKey && !jumped) return
        lastKey = key
        lastPositions = positions
        try {
            CursioNative.nativeMediaChanged(list.toString())
        } catch (e: UnsatisfiedLinkError) {
            Log.w(TAG, "the node library is not loaded", e)
        }
    }

    /** A computer pressed [command] on [player]; answers why not, or null. */
    fun command(player: String, command: String, arg: Long): String? {
        val c = controllers.firstOrNull { id(it) == player } ?: return "that player is gone"
        val t = c.transportControls
        when (command) {
            "play" -> t.play()
            "pause" -> t.pause()
            "playPause" ->
                if (c.playbackState?.state == PlaybackState.STATE_PLAYING) t.pause() else t.play()
            "next" -> t.skipToNext()
            "previous" -> t.skipToPrevious()
            "seek" -> t.seekTo(arg)
            "volume" -> {
                val max = c.playbackInfo?.maxVolume ?: return "the player has no volume"
                c.setVolumeTo((arg * max / 1000).toInt(), 0)
            }
            else -> return "$command is not a player button"
        }
        return null
    }

    /** [player]'s cover as a JPEG small enough to send, or null. */
    fun art(player: String): ByteArray? {
        val c = controllers.firstOrNull { id(it) == player } ?: return null
        val bitmap = cover(c.metadata) ?: return null
        val scale = ART_EDGE.toFloat() / maxOf(bitmap.width, bitmap.height)
        val sized = if (scale < 1f) {
            Bitmap.createScaledBitmap(bitmap, (bitmap.width * scale).toInt(), (bitmap.height * scale).toInt(), true)
        } else {
            bitmap
        }
        val out = ByteArrayOutputStream()
        sized.compress(Bitmap.CompressFormat.JPEG, 80, out)
        return out.toByteArray()
    }
}
