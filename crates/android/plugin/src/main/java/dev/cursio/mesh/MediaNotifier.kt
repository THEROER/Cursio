package dev.cursio.mesh

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.drawable.Icon
import android.media.MediaMetadata
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/**
 * The computers' players on this phone (`crates/android/src/media.rs`).
 *
 * Each becomes a media session of this app's and a media-style notification
 * tied to it. That pairing — not a custom layout — is what Android's media
 * controls and HyperOS's island pick up: they show any app's active session
 * with its notification, the way KDE Connect's players appear there. Buttons
 * on it come back through the session's callback and go to the computer;
 * "Disconnect" hides that player until it plays something else.
 */
object MediaNotifier {
    private const val TAG = "Cursio"
    private const val CHANNEL_ID = "cursio_media"
    private const val ACTION = "dev.cursio.mesh.MEDIA"

    @Volatile
    var context: Context? = null

    private val main = Handler(Looper.getMainLooper())
    private val shown = HashMap<String, Shown>()
    private var receiverRegistered = false

    private class Shown(
        val session: MediaSession,
        val id: Int,
        val peer: String,
        val player: String,
        var artKey: String? = null,
    ) {
        var peerName = ""
        var last: JSONObject? = null
        var playing = false
        /** When it last went from not playing to playing; see [present]. */
        var startedAt = 0L
    }

    /**
     * When each player was last seen playing, by [key]. A browser keeps a
     * paused player for as long as the tab with the video is open — hours —
     * and it sat in the shade the whole time, so a player paused longer than
     * [STALE_MS] is taken down until it plays again.
     */
    private val lastPlaying = HashMap<String, Long>()
    private const val STALE_MS = 5 * 60_000L

    /** Each computer's last list, re-applied once a pause has gone stale. */
    private val lastLists = HashMap<String, Pair<String, String>>()

    /** A computer's players changed; [json] is the full list, empty to clear. */
    fun update(peer: String, peerName: String, json: String) {
        val ctx = context ?: return
        main.post {
            try {
                lastLists[peer] = peerName to json
                apply(ctx, peer, peerName, JSONArray(json))
            } catch (e: Exception) {
                Log.w(TAG, "could not show $peerName's players", e)
            }
        }
    }

    private fun key(peer: String, player: String) = "$peer\u0000$player"

    private fun apply(ctx: Context, peer: String, peerName: String, list: JSONArray) {
        val manager = ctx.getSystemService(NotificationManager::class.java)
        ensureChannel(manager)
        ensureReceiver(ctx)
        val present = HashSet<String>()
        val now = SystemClock.elapsedRealtime()
        var nextStale = Long.MAX_VALUE
        for (i in 0 until list.length()) {
            val p = list.getJSONObject(i)
            val player = p.getString("id")
            val k = key(peer, player)
            if (p.optBoolean("playing")) lastPlaying[k] = now
            val since = lastPlaying.getOrPut(k) { now }
            if (now - since >= STALE_MS) continue
            nextStale = minOf(nextStale, since + STALE_MS - now)
            present.add(k)
            val entry = shown.getOrPut(k) {
                val session = MediaSession(ctx, "cursio-$peerName-$player")
                session.setCallback(Buttons(peer, player), main)
                Shown(session, k.hashCode(), peer, player)
            }
            val playing = p.optBoolean("playing")
            if (playing && !entry.playing) entry.startedAt = now
            entry.playing = playing
            entry.peerName = peerName
            entry.last = p
            fill(entry, p, peerName)
        }
        if (nextStale != Long.MAX_VALUE) {
            main.postDelayed({
                val (name, json) = lastLists[peer] ?: return@postDelayed
                try {
                    apply(ctx, peer, name, JSONArray(json))
                } catch (e: Exception) {
                    Log.w(TAG, "could not refresh $name's players", e)
                }
            }, nextStale + 1_000L)
        }
        // A stale player stays listed, and stays down; one the computer
        // dropped is forgotten.
        val listed = listedKeys(peer, list)
        lastPlaying.keys.removeAll { it.startsWith("$peer\u0000") && it !in listed }
        val gone = shown.keys.filter { it.startsWith("$peer\u0000") && it !in present }
        for (k in gone) {
            val entry = shown.remove(k) ?: continue
            manager.cancel(entry.id)
            entry.session.isActive = false
            entry.session.release()
        }
        present(manager, ctx)
    }

    /**
     * Post every computer's players, with one of them the main one: the one
     * that last started playing, anywhere. Only its session is active and its
     * notification held, and it is posted last so it sits first — which is
     * what the system's media controls and HyperOS's island follow, so the
     * island stays on the music just started instead of hopping to whichever
     * player last changed a title. The others stay in the shade, each with
     * its own buttons. A pause does not hand the place on, or a second press
     * would start the player beside it.
     */
    private fun present(manager: NotificationManager, ctx: Context) {
        val lead = shown.entries.maxByOrNull { it.value.startedAt }?.key
        for ((k, entry) in shown.entries.sortedBy { it.key == lead }) {
            val p = entry.last ?: continue
            entry.session.isActive = k == lead
            manager.notify(entry.id, notification(ctx, entry, p, entry.peer, entry.player, entry.peerName, k == lead))
        }
    }

    private fun fill(entry: Shown, p: JSONObject, peerName: String) {
        val meta = MediaMetadata.Builder()
            .putString(MediaMetadata.METADATA_KEY_TITLE, p.optString("title"))
            .putString(MediaMetadata.METADATA_KEY_ARTIST, p.optString("artist").ifEmpty { "${p.optString("app")} · $peerName" })
            .putString(MediaMetadata.METADATA_KEY_ALBUM, p.optString("album"))
        if (p.has("durationMs")) meta.putLong(MediaMetadata.METADATA_KEY_DURATION, p.getLong("durationMs"))
        val art = p.optString("art", "").ifEmpty { null }
        if (art != null) {
            val bytes = try {
                CursioNative.nativeMediaArt(art)
            } catch (e: UnsatisfiedLinkError) {
                null
            }
            if (bytes != null && bytes.isNotEmpty()) {
                BitmapFactory.decodeByteArray(bytes, 0, bytes.size)?.let {
                    meta.putBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART, it)
                }
            }
        }
        entry.artKey = art
        entry.session.setMetadata(meta.build())
        var actions = 0L
        if (p.optBoolean("canPlay")) actions = actions or PlaybackState.ACTION_PLAY or PlaybackState.ACTION_PLAY_PAUSE
        if (p.optBoolean("canPause")) actions = actions or PlaybackState.ACTION_PAUSE or PlaybackState.ACTION_PLAY_PAUSE
        if (p.optBoolean("canNext")) actions = actions or PlaybackState.ACTION_SKIP_TO_NEXT
        if (p.optBoolean("canPrevious")) actions = actions or PlaybackState.ACTION_SKIP_TO_PREVIOUS
        if (p.optBoolean("canSeek")) actions = actions or PlaybackState.ACTION_SEEK_TO
        val playing = p.optBoolean("playing")
        entry.session.setPlaybackState(
            PlaybackState.Builder()
                .setActions(actions)
                .setState(
                    if (playing) PlaybackState.STATE_PLAYING else PlaybackState.STATE_PAUSED,
                    p.optLong("positionMs", PlaybackState.PLAYBACK_POSITION_UNKNOWN),
                    if (playing) 1f else 0f,
                    SystemClock.elapsedRealtime(),
                )
                .build(),
        )
    }

    private fun button(ctx: Context, icon: Int, title: String, peer: String, player: String, command: String): Notification.Action {
        val intent = Intent(ACTION).setPackage(ctx.packageName)
            .putExtra("peer", peer).putExtra("player", player).putExtra("command", command)
        val pending = PendingIntent.getBroadcast(
            ctx,
            (peer + player + command).hashCode(),
            intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return Notification.Action.Builder(Icon.createWithResource(ctx, icon), title, pending).build()
    }

    private fun notification(
        ctx: Context,
        entry: Shown,
        p: JSONObject,
        peer: String,
        player: String,
        peerName: String,
        main: Boolean,
    ): Notification {
        val playing = p.optBoolean("playing")
        val actions = listOf(
            button(ctx, android.R.drawable.ic_media_previous, "Previous", peer, player, "previous"),
            if (playing) {
                button(ctx, android.R.drawable.ic_media_pause, "Pause", peer, player, "pause")
            } else {
                button(ctx, android.R.drawable.ic_media_play, "Play", peer, player, "play")
            },
            button(ctx, android.R.drawable.ic_media_next, "Next", peer, player, "next"),
            button(ctx, android.R.drawable.ic_menu_close_clear_cancel, "Disconnect", peer, player, "dismiss"),
        )
        val builder = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notify)
            .setContentTitle(p.optString("title"))
            .setContentText(p.optString("artist"))
            .setSubText(listOf(p.optString("app"), peerName).filter { it.isNotEmpty() }.joinToString(" · "))
            .setOngoing(playing && main)
            .setShowWhen(false)
            .setVisibility(Notification.VISIBILITY_PUBLIC)
            .setStyle(Notification.MediaStyle().setMediaSession(entry.session.sessionToken).setShowActionsInCompactView(0, 1, 2))
        for (a in actions) builder.addAction(a)
        // Without a cover, the app's own icon: HyperOS puts it there by
        // itself, MIUI 12 left an empty grey square.
        val art = entry.session.controller.metadata?.getBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART)
        (art ?: appIcon(ctx))?.let { builder.setLargeIcon(it) }
        if (main && islandHere(ctx)) {
            builder.addExtras(
                Island.mediaExtras(
                    ctx,
                    p.optString("title"),
                    p.optString("artist"),
                    listOf(p.optString("app"), entry.peerName).filter { it.isNotEmpty() }.joinToString(" · "),
                    art?.let { Icon.createWithBitmap(it) },
                    actions[1],
                ),
            )
        }
        return builder.build()
    }

    private var icon: Bitmap? = null

    /** Asked once: HyperOS's answer does not change while the app runs. */
    private var island: Boolean? = null

    private fun islandHere(ctx: Context): Boolean = island ?: Island.supported(ctx).also { island = it }

    private fun appIcon(ctx: Context): Bitmap? {
        icon?.let { return it }
        return try {
            val drawable = ctx.packageManager.getApplicationIcon(ctx.packageName)
            val size = 192
            Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888).also {
                drawable.setBounds(0, 0, size, size)
                drawable.draw(Canvas(it))
                icon = it
            }
        } catch (e: Exception) {
            Log.w(TAG, "no app icon for a player without a cover", e)
            null
        }
    }

    private fun listedKeys(peer: String, list: JSONArray): Set<String> =
        (0 until list.length()).map { key(peer, list.getJSONObject(it).getString("id")) }.toSet()

    private fun ensureChannel(manager: NotificationManager) {
        if (manager.getNotificationChannel(CHANNEL_ID) != null) return
        // Low importance: a player is something to reach, not an interruption.
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Your computers' players", NotificationManager.IMPORTANCE_LOW).apply {
                setShowBadge(false)
            },
        )
    }

    private fun ensureReceiver(ctx: Context) {
        if (receiverRegistered) return
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(c: Context, intent: Intent) {
                val peer = intent.getStringExtra("peer") ?: return
                val player = intent.getStringExtra("player") ?: return
                val command = intent.getStringExtra("command") ?: return
                if (command == "dismiss") {
                    try {
                        CursioNative.nativeMediaDismiss(peer, player)
                    } catch (e: UnsatisfiedLinkError) {
                        Log.w(TAG, "the node library is not loaded", e)
                    }
                } else {
                    press(peer, player, command, 0)
                }
            }
        }
        val filter = IntentFilter(ACTION)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            ctx.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            ctx.registerReceiver(receiver, filter)
        }
        receiverRegistered = true
    }

    private fun press(peer: String, player: String, command: String, arg: Long) {
        // Off the main thread: the node answers once the command is queued, but
        // a JNI call is still no place for the UI thread to wait.
        Thread {
            val why = try {
                CursioNative.nativeMediaRemoteCommand(peer, player, command, arg)
            } catch (e: UnsatisfiedLinkError) {
                "the node library is not loaded"
            }
            if (why.isNotEmpty()) Log.i(TAG, "a player button went nowhere: $why")
        }.start()
    }

    private class Buttons(val peer: String, val player: String) : MediaSession.Callback() {
        override fun onPlay() = press(peer, player, "play", 0)
        override fun onPause() = press(peer, player, "pause", 0)
        override fun onSkipToNext() = press(peer, player, "next", 0)
        override fun onSkipToPrevious() = press(peer, player, "previous", 0)
        override fun onSeekTo(pos: Long) = press(peer, player, "seek", pos)
        override fun onStop() = press(peer, player, "pause", 0)
    }
}
