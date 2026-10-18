package dev.cursio.mesh

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjectionManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import androidx.core.app.ServiceCompat
import androidx.core.content.IntentCompat

/**
 * What keeps the node alive on a phone.
 *
 * Android does not run daemons. A process with no visible component is a
 * process the system may reclaim at any time, and Doze will freeze its network
 * even before that. A **foreground service** is the one arrangement the
 * platform treats as legitimately long-running: it survives Doze, keeps its
 * sockets, and costs a permanent notification the user can see — which is the
 * honest trade, since this is software holding an open connection to their
 * other machines.
 *
 * ## Types, and where each may be claimed
 *
 * `connectedDevice` is what this is, and it is always claimed. `dataSync` (the
 * tempting alternative) carries a timeout on newer releases that would stop a
 * mesh link mid-session.
 *
 * `mediaPlayback` covers the audio arriving from a peer and `microphone` the
 * audio leaving for one, and the platform does not let either be claimed from
 * everywhere. A `microphone` service started while the app is not in the
 * foreground throws (Android 14), and since Android 15 a `BOOT_COMPLETED`
 * receiver may not start `mediaPlayback` either. So a start from boot or a
 * sticky restart claims what it may, and the app's window, once visible, starts
 * the service again from the foreground (`MeshPlugin`); `startForeground` with the wider mask
 * then upgrades the running service in place. Claimed while visible, the
 * microphone stays usable after the user leaves the app.
 *
 * `mediaProjection` is held only while the phone's playback is being captured
 * for a peer (`PlaybackCapture`), and dropped when that stops: Android shows a
 * "sharing your screen" indicator for as long as it is held, and its consent
 * is single-use, so a sticky restart could never reclaim it anyway.
 *
 * ## Battery optimisation is a separate ask
 *
 * The foreground service is what prevents Doze from freezing this process.
 * Asking the user to exempt Cursio from battery optimisation on top of it helps
 * on manufacturer skins that kill background apps regardless (Xiaomi, Samsung,
 * Honor), but it is not the mechanism — a build that only asked for the
 * exemption and skipped the service would be killed anyway. The exemption also
 * lifts the Android 12 ban on starting a foreground service from the
 * background, which is what lets a sticky restart come back at all.
 */
class MeshService : Service() {
    private val handler = Handler(Looper.getMainLooper())
    private var wakeLock: PowerManager.WakeLock? = null
    private var multicastLock: WifiManager.MulticastLock? = null
    private var wifiLocks: List<WifiManager.WifiLock> = emptyList()
    private lateinit var deviceStatus: DeviceStatus
    private var capture: PlaybackCapture? = null
    private var screen: ScreenShare? = null

    /** The status line last posted, so an unchanged one is not posted again. */
    private var shownStatus: String? = null

    /**
     * Keeps the notification's status line current while the service runs.
     *
     * Posted only when the line changes: reposting every tick was ~4700 posts a
     * day, which the system counts against the app and which brings a dismissed
     * notification straight back.
     */
    private val refresh = object : Runnable {
        override fun run() {
            val status = Node.statusLine()
            if (status != shownStatus) {
                notificationManager().notify(NOTIFICATION_ID, buildNotification(status))
            }
            handler.postDelayed(this, REFRESH_INTERVAL_MS)
        }
    }

    override fun onCreate() {
        super.onCreate()
        createChannel()
        // Before the node: identity and the trust store are read relative to
        // this directory, and reading them first would resolve them against a
        // path that does not exist.
        Node.init(this)
        deviceStatus = DeviceStatus(this).also { it.start() }
        instance = this
        CursioNotificationListener.rebindIfLost(this)
        CallMonitor.start(this)
        ComputerCompanion.observe(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            Prefs.setWantedRunning(this, false)
            ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }

        // Foreground first, node second. Android gives a service a few seconds
        // to call `startForeground` and kills it for missing that deadline, so
        // nothing that can block belongs before this line. A null intent is a
        // sticky restart, which happens in the background by definition.
        val fromForeground = intent?.getBooleanExtra(EXTRA_FROM_FOREGROUND, false) ?: false
        // Stopping the screen is the notification's button; what follows is
        // an ordinary start of a service that is already running.
        if (intent?.action == ACTION_SCREEN_STOP) endScreen(fromForeground)
        val capturing = intent?.action == ACTION_CAPTURE
        val screening = intent?.action == ACTION_SCREEN
        val projecting = capturing || screening || capture != null || screen != null
        if (!enterForeground(fromForeground, projection = projecting)) {
            takePendingCapture()?.invoke("Android refused to keep the Cursio service in the foreground")
            stopSelf()
            return START_NOT_STICKY
        }

        Node.start(this)

        handler.removeCallbacks(refresh)
        handler.postDelayed(refresh, REFRESH_INTERVAL_MS)
        // Busy from the start: a node coming up is dialling and handshaking.
        lastBusyAt = android.os.SystemClock.elapsedRealtime()
        handler.removeCallbacks(power)
        handler.post(power)

        if (capturing) {
            val error = beginCapture(intent!!)
            if (error != null) Log.w(TAG, "playback capture did not start: $error")
            takePendingCapture()?.invoke(error)
        }
        if (screening) {
            val error = beginScreen(intent!!)
            if (error != null) Log.w(TAG, "showing the screen did not start: $error")
            // A kept consent the system no longer honours: ask for a new one.
            val computer = intent.getStringExtra(ScreenConsentActivity.EXTRA_REUSED_FOR)
            if (error != null && computer != null) ScreenConsentActivity.consentRefused(this, computer)
        }

        // START_STICKY: if the system reclaims this process under memory
        // pressure, bring it back. A mesh node that stays gone until the user
        // notices is the failure people already blame KDE Connect for.
        return START_STICKY
    }

    /**
     * Claim the widest set of types this start may claim, falling back to
     * `connectedDevice` alone when the platform refuses the rest. The refusal
     * is a `SecurityException` for a missing while-in-use permission and an
     * `IllegalStateException` (`ForegroundServiceStartNotAllowedException`) for
     * a background start; both mean "less", not "nothing".
     */
    private fun enterForeground(
        fromForeground: Boolean,
        projection: Boolean = capture != null || screen != null,
    ): Boolean {
        // What the service already holds is asked for again, so a start that
        // may claim less (a sticky restart) does not quietly drop the
        // microphone the window claimed earlier. The projection type is the
        // exception: it is held exactly while a capture needs it, and the
        // fallbacks without it leave the caller to see it missing.
        val extra = if (projection) projectionType() else 0
        val held = activeTypes and projectionType().inv()
        val allowed = wantedTypes(fromForeground)
        val candidates = listOf(
            allowed or held or extra,
            allowed or extra,
            baseType() or extra,
            allowed or held,
            allowed,
            baseType(),
        )
        for (types in candidates.distinct()) {
            try {
                ServiceCompat.startForeground(this, NOTIFICATION_ID, buildNotification(), types)
                activeTypes = types
                return true
            } catch (e: SecurityException) {
                Log.w(TAG, "foreground types $types refused", e)
            } catch (e: IllegalStateException) {
                Log.w(TAG, "foreground types $types refused", e)
            }
        }
        activeTypes = 0
        return false
    }

    private fun wantedTypes(fromForeground: Boolean): Int {
        var types = baseType()
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return types
        if (fromForeground || Build.VERSION.SDK_INT < Build.VERSION_CODES.VANILLA_ICE_CREAM) {
            types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
        }
        if (fromForeground && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && micGranted(this)) {
            types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
        }
        return types
    }

    /**
     * Start capturing what the phone plays, from the consent the window
     * obtained, or say why not. The service must already hold the projection
     * type: Android 14 refuses `getMediaProjection` to a service without it.
     */
    private fun beginCapture(intent: Intent): String? {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return "capturing playback needs Android 10 or later"
        if (!micGranted(this)) return "Android captures playback only for an app allowed to use the microphone"
        if (activeTypes and projectionType() == 0) return "Android refused the screen-capture service type"
        val data = IntentCompat.getParcelableExtra(intent, EXTRA_PROJECTION, Intent::class.java)
            ?: return "the screen-capture consent was missing"
        val code = intent.getIntExtra(EXTRA_RESULT_CODE, 0)

        // A consent is good for one projection, so a capture already running
        // gives way to the one the user just approved.
        capture?.stop()
        capture = null

        val manager = getSystemService(MediaProjectionManager::class.java)
        val error = try {
            val projection = manager.getMediaProjection(code, data)
            if (projection == null) {
                "Android did not accept the screen-capture consent"
            } else {
                var started: PlaybackCapture? = null
                started = PlaybackCapture.start(projection, handler) {
                    if (capture != null && capture === started) endCapture(fromForeground = false)
                }
                capture = started
                null
            }
        } catch (e: SecurityException) {
            "Android refused the projection: ${e.message}"
        } catch (e: RuntimeException) {
            "capturing playback failed: ${e.message ?: e.javaClass.simpleName}"
        }
        if (error != null) enterForeground(fromForeground = true, projection = screen != null)
        return error
    }

    /** Stop capturing playback and give up the projection type it needed. */
    private fun endCapture(fromForeground: Boolean) {
        capture?.stop()
        capture = null
        if (activeTypes and projectionType() != 0 && screen == null) {
            enterForeground(fromForeground, projection = false)
        }
    }

    /**
     * Start showing the screen on computers, from the consent the user just
     * gave, or say why not. Android starts one projection per consent and
     * stops an app's earlier one when it starts the next, so sharing the
     * screen ends a playback capture that was running.
     */
    private fun beginScreen(intent: Intent): String? {
        if (activeTypes and projectionType() == 0) return "Android refused the screen-capture service type"
        val data = IntentCompat.getParcelableExtra(intent, EXTRA_PROJECTION, Intent::class.java)
            ?: return "the screen-capture consent was missing"
        val code = intent.getIntExtra(EXTRA_RESULT_CODE, 0)
        screen?.stop()
        screen = null
        val manager = getSystemService(MediaProjectionManager::class.java)
        val error = try {
            val projection = manager.getMediaProjection(code, data)
            if (projection == null) {
                "Android did not accept the screen-capture consent"
            } else {
                var started: ScreenShare? = null
                started = ScreenShare.start(this, projection) {
                    handler.post { if (screen != null && screen === started) endScreen(fromForeground = false) }
                }
                screen = started
                if (intent.getBooleanExtra(ScreenConsentActivity.EXTRA_EVERYONE, false)) {
                    val computers = CursioNative.nativeScreenShareWithLinked()
                    Log.i(TAG, "showing the screen on $computers computers")
                }
                notificationManager().notify(NOTIFICATION_ID, buildNotification(Node.statusLine()))
                null
            }
        } catch (e: SecurityException) {
            "Android refused the projection: ${e.message}"
        } catch (e: RuntimeException) {
            "showing the screen failed: ${e.message ?: e.javaClass.simpleName}"
        }
        if (error != null) enterForeground(fromForeground = true, projection = capture != null)
        return error
    }

    /** Stop showing the screen and give up the projection type if nothing else needs it. */
    private fun endScreen(fromForeground: Boolean) {
        val share = screen ?: return
        screen = null
        share.stop()
        if (activeTypes and projectionType() != 0 && capture == null) {
            enterForeground(fromForeground, projection = false)
        }
        notificationManager().notify(NOTIFICATION_ID, buildNotification(Node.statusLine()))
    }

    private fun projectionType(): Int =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
        } else {
            0
        }

    private fun baseType(): Int =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
        } else {
            0
        }

    /** When the node last had work, for [IDLE_GRACE_MS]. */
    private var lastBusyAt = 0L

    /** Re-reads what the node is doing and holds the locks that calls for. */
    private val power = object : Runnable {
        override fun run() {
            updateLocks()
            handler.postDelayed(this, POWER_CHECK_MS)
        }
    }

    /**
     * Hold the CPU and the Wi-Fi radio awake while the node has work, and the
     * multicast filter open while it may need to find a machine.
     *
     * The foreground service keeps the process; it does not keep the radio
     * listening. With the screen off, Wi-Fi power save batches incoming
     * packets behind the access point's beacon interval and the CPU sleeps
     * between them, so a stream arrives in bursts — heard as stutter. Each
     * lock closes one of those gaps:
     * - the partial wake lock keeps the CPU running to decode and play;
     * - the Wi-Fi lock keeps the radio out of power save. Low-latency mode
     *   (API 29+) is honoured only while the app is in front, so on 29–33 the
     *   high-performance lock is held alongside it; from 34 that lock does
     *   nothing and is skipped;
     * - the multicast lock lets mDNS through at all — without it Android
     *   filters multicast and the phone announces itself and hears nobody.
     *
     * They used to be held for as long as the node ran, and that was most of
     * what Cursio cost a phone's battery (a quarter of it, on a day of
     * nothing streaming): a held wake lock let every idle timer of every link
     * keep the CPU up. Idle, nothing needs them. A peer's packet wakes a
     * sleeping phone by itself, and its heartbeats every few seconds are what
     * keep the link from looking dead; the node answers in that wake. So the
     * CPU and radio locks follow the node's own "moving data" signal (any
     * module at its fast pace, a capture or a shown screen), held a few
     * seconds past the last of it so a stream with gaps does not flap them.
     * Multicast is open while the screen is on, while busy, and for a short
     * window every few minutes otherwise, which is enough to hear a machine
     * that changed address; machines already known are dialled directly.
     */
    private fun updateLocks() {
        val now = android.os.SystemClock.elapsedRealtime()
        val busy = Node.busy() || capture != null || screen != null
        if (busy) lastBusyAt = now
        val working = busy || now - lastBusyAt < IDLE_GRACE_MS
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        val discovering = working || pm.isInteractive || now % DISCOVERY_EVERY_MS < DISCOVERY_FOR_MS
        holdWork(working)
        holdMulticast(discovering)
    }

    private fun holdWork(held: Boolean) {
        if (!held) {
            wifiLocks.forEach { if (it.isHeld) it.release() }
            wifiLocks = emptyList()
            wakeLock?.let { if (it.isHeld) it.release() }
            wakeLock = null
            return
        }
        if (wakeLock == null) {
            val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "cursio:mesh").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        if (wifiLocks.isEmpty()) {
            val modes = buildList {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                    add(WifiManager.WIFI_MODE_FULL_LOW_LATENCY)
                }
                if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                    @Suppress("DEPRECATION")
                    add(WifiManager.WIFI_MODE_FULL_HIGH_PERF)
                }
            }
            wifiLocks = modes.map { mode ->
                wifi.createWifiLock(mode, "cursio:link").apply {
                    setReferenceCounted(false)
                    acquire()
                }
            }
        }
    }

    private fun holdMulticast(held: Boolean) {
        if (!held) {
            multicastLock?.let { if (it.isHeld) it.release() }
            multicastLock = null
            return
        }
        if (multicastLock == null) {
            val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            multicastLock = wifi.createMulticastLock("cursio:mdns").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
    }

    private fun releaseLocks() {
        wifiLocks.forEach { if (it.isHeld) it.release() }
        wifiLocks = emptyList()
        multicastLock?.let { if (it.isHeld) it.release() }
        multicastLock = null
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
    }

    override fun onDestroy() {
        CallMonitor.stop()
        handler.removeCallbacks(refresh)
        handler.removeCallbacks(power)
        deviceStatus.stop()
        capture?.stop()
        capture = null
        screen?.stop()
        screen = null
        takePendingCapture()?.invoke("the Cursio service stopped")
        instance = null
        activeTypes = 0
        // Queued on the lifecycle thread, which blocks until the listener is
        // closed; a start that follows is queued behind it and so binds a
        // free port.
        Node.stop()
        releaseLocks()
        super.onDestroy()
    }

    /** Not a bound service: the node's lifetime is the service's, not a caller's. */
    override fun onBind(intent: Intent?): IBinder? = null

    private fun buildNotification(status: String = Node.statusLine()): Notification {
        shownStatus = status
        // The Activity is Tauri's, generated into the app under the app's own
        // package, which this library cannot name; the launcher entry finds it.
        val launch = packageManager.getLaunchIntentForPackage(packageName)
            ?.addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
        val open = launch?.let {
            PendingIntent.getActivity(
                this,
                0,
                it,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
        }
        val stop = PendingIntent.getService(
            this,
            1,
            Intent(this, MeshService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val builder = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Cursio")
            .setContentText(status)
            .setSmallIcon(R.drawable.ic_notify)
            .setContentIntent(open)
            // Not ongoing: from Android 14 that lets the user swipe it away,
            // and the service stays in the foreground without it showing.
            .setShowWhen(false)
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
        if (screen != null) {
            val stopScreen = PendingIntent.getService(
                this,
                2,
                Intent(this, MeshService::class.java).setAction(ACTION_SCREEN_STOP),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            builder.setSubText("Showing this screen on computers")
                .addAction(Notification.Action.Builder(null, "Stop showing screen", stopScreen).build())
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // Without this Android 12+ may hold the notification back for ten
            // seconds, and a phone that just booted shows no sign the node is up.
            builder.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
        }
        return builder.build()
    }

    private fun createChannel() {
        // Low importance: this notification exists because the platform
        // requires one, not because the user needs telling. Anything higher
        // makes a sound every time the status line changes, and lower is not
        // available — the system raises a foreground service's channel to low.
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Cursio mesh",
            NotificationManager.IMPORTANCE_LOW,
        )
        channel.description = "Shows that this device is connected to your other machines."
        notificationManager().createNotificationChannel(channel)
    }

    private fun notificationManager(): NotificationManager =
        getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

    companion object {
        private const val TAG = "Cursio"
        private const val CHANNEL_ID = "cursio-mesh"
        private const val NOTIFICATION_ID = 1
        private const val REFRESH_INTERVAL_MS = 5_000L

        /** How often the locks are fitted to what the node is doing. */
        private const val POWER_CHECK_MS = 1_000L

        /** How long past the last work the CPU and radio stay held. */
        private const val IDLE_GRACE_MS = 10_000L

        /** With the screen off and nothing to do: a discovery window this
         *  long every [DISCOVERY_EVERY_MS]. */
        private const val DISCOVERY_FOR_MS = 30_000L
        private const val DISCOVERY_EVERY_MS = 5 * 60_000L

        private const val ACTION_STOP = "dev.cursio.mesh.STOP"
        private const val EXTRA_FROM_FOREGROUND = "dev.cursio.mesh.FROM_FOREGROUND"
        private const val ACTION_CAPTURE = "dev.cursio.mesh.CAPTURE_PLAYBACK"
        private const val EXTRA_RESULT_CODE = "dev.cursio.mesh.RESULT_CODE"
        private const val EXTRA_PROJECTION = "dev.cursio.mesh.PROJECTION"
        private const val ACTION_SCREEN = "dev.cursio.mesh.SCREEN"
        private const val ACTION_SCREEN_STOP = "dev.cursio.mesh.SCREEN_STOP"

        /**
         * Show this screen on computers, from a consent just given; with
         * [everyone], on every computer linked now, otherwise on the ones
         * that asked. Must run from the visible window or the consent
         * activity: the projection type is claimed by this start.
         *
         * @throws IllegalStateException when Android refuses the start.
         */
        fun startScreen(context: Context, resultCode: Int, data: Intent, everyone: Boolean, reusedFor: String? = null) {
            val intent = Intent(context, MeshService::class.java)
                .setAction(ACTION_SCREEN)
                .putExtra(EXTRA_FROM_FOREGROUND, true)
                .putExtra(EXTRA_RESULT_CODE, resultCode)
                .putExtra(EXTRA_PROJECTION, data)
                .putExtra(ScreenConsentActivity.EXTRA_EVERYONE, everyone)
                .putExtra(ScreenConsentActivity.EXTRA_REUSED_FOR, reusedFor)
            context.startForegroundService(intent)
        }

        /** Stop showing the screen; nobody is watching any more. */
        fun stopScreen() {
            main.post { instance?.endScreen(fromForeground = false) }
        }

        fun screenKeyframe() {
            main.post { instance?.screen?.keyframe() }
        }

        fun screenBitrate(bitsPerSecond: Int) {
            main.post { instance?.screen?.setBitrate(bitsPerSecond) }
        }

        /** Whether the screen is being shown now. */
        fun showingScreen(): Boolean = instance?.screen != null

        /** The foreground types the running service holds; 0 when it is not running. */
        @Volatile
        var activeTypes: Int = 0
            private set

        /** Whether the running service holds the microphone type. */
        fun holdsMicrophone(): Boolean =
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
                activeTypes and ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE != 0

        fun micGranted(context: Context): Boolean =
            context.checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

        /**
         * Start the node, or widen a running one to the types this caller
         * may claim.
         *
         * @param fromForeground true only from the visible window: it is what
         *   allows the microphone type, and claiming it from anywhere else is
         *   an exception, not a no-op.
         */
        fun start(context: Context, fromForeground: Boolean) {
            val intent = Intent(context, MeshService::class.java)
                .putExtra(EXTRA_FROM_FOREGROUND, fromForeground)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, MeshService::class.java))
        }

        private val main = Handler(Looper.getMainLooper())

        /** Touched only on the main thread, like every service callback. */
        private var instance: MeshService? = null

        private var pendingCapture: ((String?) -> Unit)? = null

        private fun takePendingCapture(): ((String?) -> Unit)? = pendingCapture.also { pendingCapture = null }

        /**
         * Capture what the phone plays, from a projection consent the window
         * just obtained; [done] gets null once audio is flowing, or the reason
         * it is not. Must run on the main thread and from the visible window:
         * the projection type is claimed by this start.
         *
         * @throws IllegalStateException when Android refuses the start.
         */
        fun startPlaybackCapture(context: Context, resultCode: Int, data: Intent, done: (String?) -> Unit) {
            takePendingCapture()?.invoke("a newer request to share playback replaced this one")
            pendingCapture = done
            val intent = Intent(context, MeshService::class.java)
                .setAction(ACTION_CAPTURE)
                .putExtra(EXTRA_FROM_FOREGROUND, true)
                .putExtra(EXTRA_RESULT_CODE, resultCode)
                .putExtra(EXTRA_PROJECTION, data)
            try {
                context.startForegroundService(intent)
            } catch (e: IllegalStateException) {
                pendingCapture = null
                throw e
            }
        }

        /** Stop capturing playback, then call [done] on the main thread. */
        fun stopPlaybackCapture(done: () -> Unit) {
            main.post {
                instance?.endCapture(fromForeground = true)
                done()
            }
        }
    }
}
