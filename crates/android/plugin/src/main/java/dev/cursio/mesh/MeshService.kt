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

    /** Keeps the notification's status line current while the service runs. */
    private val refresh = object : Runnable {
        override fun run() {
            notificationManager().notify(NOTIFICATION_ID, buildNotification())
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
        val capturing = intent?.action == ACTION_CAPTURE
        if (!enterForeground(fromForeground, projection = capturing || capture != null)) {
            takePendingCapture()?.invoke("Android refused to keep the Cursio service in the foreground")
            stopSelf()
            return START_NOT_STICKY
        }

        acquireLocks()
        Node.start(this)

        handler.removeCallbacks(refresh)
        handler.postDelayed(refresh, REFRESH_INTERVAL_MS)

        if (capturing) {
            val error = beginCapture(intent!!)
            if (error != null) Log.w(TAG, "playback capture did not start: $error")
            takePendingCapture()?.invoke(error)
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
    private fun enterForeground(fromForeground: Boolean, projection: Boolean = capture != null): Boolean {
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
        if (error != null) enterForeground(fromForeground = true, projection = false)
        return error
    }

    /** Stop capturing playback and give up the projection type it needed. */
    private fun endCapture(fromForeground: Boolean) {
        capture?.stop()
        capture = null
        if (activeTypes and projectionType() != 0) enterForeground(fromForeground, projection = false)
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

    /**
     * Hold the CPU and the Wi-Fi radio awake for as long as the node runs.
     *
     * The foreground service keeps the process; it does not keep the radio
     * listening. With the screen off, Wi-Fi power save batches incoming
     * packets behind the access point's beacon interval and Doze parks the
     * CPU between them, so audio arrives in bursts — heard as stutter — and
     * heartbeats arrive late enough for a peer to call the link dead. Each lock
     * closes one of those gaps:
     * - the partial wake lock keeps the CPU running to decode and play;
     * - the multicast lock lets mDNS through at all — without it Android
     *   filters multicast and the phone announces itself and hears nobody;
     * - the Wi-Fi lock keeps the radio out of power save. Low-latency mode
     *   (API 29+) is honoured only while the app is in front, so on 29–33 the
     *   high-performance lock is held alongside it; from 34 that lock does
     *   nothing and is skipped.
     */
    private fun acquireLocks() {
        if (wakeLock == null) {
            val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "cursio:mesh").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        if (multicastLock == null) {
            multicastLock = wifi.createMulticastLock("cursio:mdns").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
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

    private fun releaseLocks() {
        wifiLocks.forEach { if (it.isHeld) it.release() }
        wifiLocks = emptyList()
        multicastLock?.let { if (it.isHeld) it.release() }
        multicastLock = null
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
    }

    override fun onDestroy() {
        handler.removeCallbacks(refresh)
        deviceStatus.stop()
        capture?.stop()
        capture = null
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

    private fun buildNotification(): Notification {
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
            .setContentText(Node.statusLine())
            .setSmallIcon(R.drawable.ic_notify)
            .setContentIntent(open)
            .setOngoing(true)
            .setShowWhen(false)
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
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
        // makes a sound every time the status line changes.
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

        private const val ACTION_STOP = "dev.cursio.mesh.STOP"
        private const val EXTRA_FROM_FOREGROUND = "dev.cursio.mesh.FROM_FOREGROUND"
        private const val ACTION_CAPTURE = "dev.cursio.mesh.CAPTURE_PLAYBACK"
        private const val EXTRA_RESULT_CODE = "dev.cursio.mesh.RESULT_CODE"
        private const val EXTRA_PROJECTION = "dev.cursio.mesh.PROJECTION"

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
