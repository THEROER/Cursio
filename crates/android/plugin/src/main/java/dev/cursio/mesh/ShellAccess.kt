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
 * (`docs/phone-shell-design.md`): pairing with the code its screen shows,
 * and where its services are.
 *
 * The code is typed into a notification rather than into Cursio's window:
 * leaving the system's pairing dialog closes the pairing service, so the
 * person answers from the shade while the dialog stays open. The dialog
 * shows the port beside the code, and the notification takes both: Android's
 * mDNS does not show an app the services its own phone advertises (HyperOS 3 /
 * Android 16 — discovery ran and found nothing), so a pairing port found by
 * mDNS is a convenience, not something to wait for. The debugging port is
 * found by the node itself, by trying ports (`crates/android/src/shell.rs`).
 *
 * What happens here is said in the node's log too, where a computer reads it.
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

    /** The debugging port as mDNS showed it, `host:port`, on a phone where it does. */
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
            say(false, "mDNS shows this phone's debugging port: ${addr ?: "gone"}")
            connectAddr = addr
        }
    }

    /** Whether wireless debugging is on now. */
    fun on(context: Context): Boolean =
        Settings.Global.getInt(context.contentResolver, "adb_wifi_enabled", 0) == 1

    /**
     * Ask for the pairing dialog's port and code in a notification at once,
     * and look for the pairing service meanwhile; if mDNS shows it, the code
     * alone will do.
     */
    @Synchronized
    fun startPairing(context: Context) {
        val ctx = context.applicationContext
        say(false, "pairing this phone's shell: waiting for the dialog's port and code")
        askForCode(ctx, null, null)
        pairing?.let { stop(ctx, it) }
        pairing = discover(ctx, PAIRING) { addr ->
            if (addr != null) {
                say(false, "mDNS shows this phone's pairing service at $addr")
                askForCode(ctx, addr, null)
            }
        }
    }

    /** The settings screen pairing starts from, with wireless debugging picked out. */
    fun settingsIntent(): Intent =
        Intent(Settings.ACTION_APPLICATION_DEVELOPMENT_SETTINGS)
            .putExtra(":settings:fragment_args_key", "toggle_adb_wireless")
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)

    private fun askForCode(ctx: Context, addr: String?, failed: String?) {
        val manager = ctx.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Pairing this phone's shell", NotificationManager.IMPORTANCE_HIGH),
        )
        val input = RemoteInput.Builder(KEY_CODE)
            .setLabel(if (addr == null) "Port and code, e.g. 37421 123456" else "Pairing code")
            .build()
        val intent = Intent(ctx, ShellPairReceiver::class.java).setAction(ACTION_CODE)
        if (addr != null) intent.putExtra(EXTRA_ADDR, addr)
        val reply = PendingIntent.getBroadcast(
            ctx,
            NOTIFICATION_ID,
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE,
        )
        val action = Notification.Action.Builder(null, "Enter", reply).addRemoteInput(input).build()
        val asked = if (addr == null) {
            "In wireless debugging tap “Pair device with pairing code”, keep that dialog open, " +
                "and type the port after its address and the six digits here."
        } else {
            "Type the six digits the pairing dialog shows. Keep the dialog open."
        }
        val text = failed?.let { "That did not pair: $it. $asked" } ?: asked
        val notification = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_lock_lock)
            .setContentTitle("Pair Cursio with this phone's debugging")
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .addAction(action)
            .setOnlyAlertOnce(failed == null && addr != null)
            .build()
        manager.notify(NOTIFICATION_ID, notification)
    }

    /**
     * What the person typed: the six-digit code, after the pairing port when
     * mDNS did not show it — `37421 123456`, or the dialog's whole
     * `192.168.1.24:37421` and the code.
     */
    fun answer(context: Context, found: String?, typed: String) {
        val ctx = context.applicationContext
        val numbers = Regex("\\d+").findAll(typed).map { it.value }.toList()
        val code = numbers.lastOrNull()
        val port = numbers.getOrNull(numbers.size - 2)
        val addr = when {
            port != null && port.length in 4..5 -> wifiAddress()?.let { "$it:$port" }
            else -> found
        }
        if (code == null || addr == null) {
            askForCode(ctx, found, "type the port shown after the dialog's address, then the code")
            return
        }
        Thread {
            val why = CursioNative.nativeShellPair(addr, code)
            if (why.isEmpty()) {
                say(false, "this phone's shell is paired through $addr")
                synchronized(this) { pairing?.let { stop(ctx, it) }; pairing = null }
                done(ctx)
            } else {
                say(true, "pairing this phone's shell through $addr failed: $why")
                askForCode(ctx, found, why)
            }
        }.start()
    }

    private fun done(ctx: Context) {
        val notification = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_lock_lock)
            .setContentTitle("This phone's shell is paired")
            .setContentText("Cursio can now use it. You can close the pairing dialog.")
            .setTimeoutAfter(10_000)
            .build()
        ctx.getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification)
    }

    /** This phone's Wi-Fi address, IPv4: where its wireless debugging listens. */
    fun wifiAddress(): String? {
        val nics = NetworkInterface.getNetworkInterfaces()?.toList().orEmpty()
            .filter { it.isUp && !it.isLoopback }
            .sortedBy { if (it.name.startsWith("wlan")) 0 else 1 }
        return nics.firstNotNullOfOrNull { nic ->
            nic.inetAddresses.toList().firstOrNull { it is Inet4Address && it.isSiteLocalAddress }?.hostAddress
        }
    }

    private fun say(warn: Boolean, message: String) {
        if (warn) Log.w(TAG, message) else Log.i(TAG, message)
        try {
            CursioNative.nativeShellLog(warn, message)
        } catch (e: UnsatisfiedLinkError) {
            Log.w(TAG, "the node's log is not reachable", e)
        }
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
                say(true, "cannot look for $serviceType: error $errorCode")
            }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}

            override fun onServiceFound(info: NsdServiceInfo) {
                nsd.resolveService(info, object : NsdManager.ResolveListener {
                    override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) {
                        say(true, "cannot resolve ${info.serviceName}: error $errorCode")
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

/** What was typed into the pairing notification. */
class ShellPairReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ShellAccess.ACTION_CODE) return
        val typed = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(ShellAccess.KEY_CODE)?.toString() ?: return
        ShellAccess.answer(context, intent.getStringExtra(ShellAccess.EXTRA_ADDR), typed)
    }
}
