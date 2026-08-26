package dev.cursio.mesh

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper

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
 * The type is `connectedDevice`, which is what this actually is. It is not a
 * cosmetic label: the system matches the declared type against what the service
 * does, and `dataSync` (the tempting alternative) carries a timeout on newer
 * releases that would stop a mesh link mid-session.
 *
 * ## Battery optimisation is a separate ask
 *
 * The foreground service is what prevents Doze from freezing this process.
 * Asking the user to exempt Cursio from battery optimisation on top of it helps
 * on manufacturer skins that kill background apps regardless (Xiaomi, Samsung,
 * Honor), but it is not the mechanism — a build that only asked for the
 * exemption and skipped the service would be killed anyway.
 */
class MeshService : Service() {
    private val handler = Handler(Looper.getMainLooper())

    /** Keeps the notification's status line current while the node runs. */
    private val refresh = object : Runnable {
        override fun run() {
            if (!CursioNative.nativeIsRunning()) return
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
        CursioNative.nativeInit(filesDir.absolutePath)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Foreground first, node second. Android gives a service a few seconds
        // to call `startForeground` and kills it for missing that deadline, so
        // nothing that can block belongs before this line.
        startForeground(NOTIFICATION_ID, buildNotification())

        val name = intent?.getStringExtra(EXTRA_DISPLAY_NAME) ?: Build.MODEL ?: "Android"
        val port = intent?.getIntExtra(EXTRA_PORT, 0) ?: 0
        CursioNative.nativeStart(name, port)

        handler.postDelayed(refresh, REFRESH_INTERVAL_MS)

        // START_STICKY: if the system reclaims this process under memory
        // pressure, bring it back. A mesh node that stays gone until the user
        // notices is the failure people already blame KDE Connect for.
        return START_STICKY
    }

    override fun onDestroy() {
        handler.removeCallbacks(refresh)
        // Blocks until the listener is closed. A restart that arrives before
        // the port is free binds nothing and looks like a node that will not
        // start.
        CursioNative.nativeStop()
        super.onDestroy()
    }

    /** Not a bound service: the node's lifetime is the service's, not a caller's. */
    override fun onBind(intent: Intent?): IBinder? = null

    private fun buildNotification(): Notification {
        val status = runCatching { CursioNative.nativeStatusLine() }.getOrDefault("starting")
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Cursio")
            .setContentText(status)
            .setSmallIcon(android.R.drawable.stat_sys_upload)
            .setOngoing(true)
            .build()
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
        private const val CHANNEL_ID = "cursio-mesh"
        private const val NOTIFICATION_ID = 1
        private const val REFRESH_INTERVAL_MS = 5_000L

        const val EXTRA_DISPLAY_NAME = "dev.cursio.mesh.DISPLAY_NAME"
        const val EXTRA_PORT = "dev.cursio.mesh.PORT"

        /** Start the mesh, naming this phone as peers will see it. */
        fun start(context: Context, displayName: String, port: Int = 0) {
            val intent = Intent(context, MeshService::class.java)
                .putExtra(EXTRA_DISPLAY_NAME, displayName)
                .putExtra(EXTRA_PORT, port)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, MeshService::class.java))
        }
    }
}
