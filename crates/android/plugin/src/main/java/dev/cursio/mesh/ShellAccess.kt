package dev.cursio.mesh

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.provider.Settings
import android.util.Log
import java.net.Inet4Address
import java.net.InetAddress
import java.net.NetworkInterface

/**
 * This phone's own wireless debugging, as the node reaches it
 * (`docs/phone-shell-design.md`): finding its two services by mDNS, and
 * pairing with the code its screen shows.
 *
 * Both services move: the pairing one exists only while "Pair device with
 * pairing code" is open, and the debugging port changes every time debugging
 * is turned on. Android's own NsdManager sees them on this phone; only those
 * whose address is one of this phone's are taken, so a computer's or another
 * phone's debugging on the same Wi-Fi is never mistaken for ours.
 *
 * The code is typed into a notification rather than into Cursio's window:
 * leaving the system's pairing dialog closes the pairing service, so the
 * person must answer from the shade while the dialog stays open.
 */
object ShellAccess {
    private const val TAG = "Cursio"
    private const val PAIRING = "_adb-tls-pairing._tcp"
    private const val CONNECT = "_adb-tls-connect._tcp"
    private const val CHANNEL_ID = "cursio_shell"
    private const val NOTIFICATION_ID = 0x0c5e11
    const val ACTION_CODE = "dev.cursio.mesh.SHELL_PAIRING_CODE"
    const val EXTRA_ADDR = "addr"
    const val KEY_CODE = "code"

    /** The debugging port of this phone, `host:port`, while debugging is on. */
    @Volatile
    var connectAddr: String? = null
        private set

    private var watching: NsdManager.DiscoveryListener? = null
    private var pairing: NsdManager.DiscoveryListener? = null

    /** Keep [connectAddr] current for as long as the node runs; the first call starts it. */
    @Synchronized
    fun watch(context: Context) {
        if (watching != null) return
        watching = discover(context.applicationContext, CONNECT) { addr ->
            Log.i(TAG, "this phone's debugging port: ${addr ?: "gone"}")
            connectAddr = addr
        }
    }

    /**
     * Open the developer settings at wireless debugging and wait for its
     * pairing service; when it appears, ask for the code in a notification.
     */
    @Synchronized
    fun startPairing(context: Context) {
        val ctx = context.applicationContext
        pairing?.let { stop(ctx, it) }
        pairing = discover(ctx, PAIRING) { addr ->
            if (addr != null) askForCode(ctx, addr, null)
        }
    }

    /** The settings screen pairing starts from, with wireless debugging picked out. */
    fun settingsIntent(): Intent =
        Intent(Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS)
            .putExtra(":settings:fragment_args_key", "toggle_adb_wireless")
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)

    private fun askForCode(ctx: Context, addr: String, failed: String?) {
        val manager = ctx.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Pairing this phone's shell", NotificationManager.IMPORTANCE_HIGH),
        )
        val input = RemoteInput.Builder(KEY_CODE).setLabel("Pairing code").build()
        val reply = PendingIntent.getBroadcast(
            ctx,
            NOTIFICATION_ID,
            Intent(ctx, ShellPairReceiver::class.java).setAction(ACTION_CODE).putExtra(EXTRA_ADDR, addr),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE,
        )
        val action = Notification.Action.Builder(null, "Enter code", reply).addRemoteInput(input).build()
        val text = failed?.let { "That did not pair: $it. Type the code again." }
            ?: "Type the six digits the pairing dialog shows. Keep the dialog open."
        val notification = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_lock_lock)
            .setContentTitle("Pair Cursio with this phone's debugging")
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .addAction(action)
            .setOnlyAlertOnce(failed != null)
            .build()
        manager.notify(NOTIFICATION_ID, notification)
    }

    /** The code the person typed for the pairing service at [addr]. */
    fun answer(context: Context, addr: String, code: String) {
        val ctx = context.applicationContext
        Thread {
            val why = CursioNative.nativeShellPair(addr, code.trim())
            if (why.isEmpty()) {
                synchronized(this) { pairing?.let { stop(ctx, it) }; pairing = null }
                done(ctx, "Cursio can now use this phone's shell. You can close the dialog.")
            } else {
                Log.w(TAG, "pairing this phone's shell failed: $why")
                askForCode(ctx, addr, why)
            }
        }.start()
    }

    private fun done(ctx: Context, text: String) {
        val notification = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_lock_lock)
            .setContentTitle("This phone's shell is paired")
            .setContentText(text)
            .setTimeoutAfter(10_000)
            .build()
        ctx.getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification)
    }

    /**
     * Discover [type] and report this phone's own instance as `host:port`,
     * and `null` when it goes away.
     */
    @Suppress("DEPRECATION")
    private fun discover(ctx: Context, type: String, found: (String?) -> Unit): NsdManager.DiscoveryListener {
        val nsd = ctx.getSystemService(NsdManager::class.java)
        val ours = HashSet<String>()
        val listener = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String) {}
            override fun onDiscoveryStopped(serviceType: String) {}
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "cannot look for $serviceType: $errorCode")
            }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}

            override fun onServiceFound(info: NsdServiceInfo) {
                nsd.resolveService(info, object : NsdManager.ResolveListener {
                    override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) {
                        Log.w(TAG, "cannot resolve ${info.serviceName}: $errorCode")
                    }

                    override fun onServiceResolved(info: NsdServiceInfo) {
                        val host = info.host ?: return
                        if (!isThisPhone(host)) return
                        synchronized(ours) { ours.add(info.serviceName) }
                        val address = if (host is Inet4Address) host.hostAddress else "[${host.hostAddress}]"
                        found("$address:${info.port}")
                    }
                })
            }

            override fun onServiceLost(info: NsdServiceInfo) {
                val was = synchronized(ours) { ours.remove(info.serviceName) }
                if (was) found(null)
            }
        }
        nsd.discoverServices(type, NsdManager.PROTOCOL_DNS_SD, listener)
        return listener
    }

    private fun stop(ctx: Context, listener: NsdManager.DiscoveryListener) {
        try {
            ctx.getSystemService(NsdManager::class.java).stopServiceDiscovery(listener)
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "the discovery was not running", e)
        }
    }

    private fun isThisPhone(host: InetAddress): Boolean =
        NetworkInterface.getNetworkInterfaces()?.toList().orEmpty().any { nic ->
            nic.inetAddresses.toList().any { it == host }
        }
}

/** The code typed into the pairing notification. */
class ShellPairReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ShellAccess.ACTION_CODE) return
        val addr = intent.getStringExtra(ShellAccess.EXTRA_ADDR) ?: return
        val code = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(ShellAccess.KEY_CODE)?.toString() ?: return
        ShellAccess.answer(context, addr, code)
    }
}
