package dev.cursio.mesh

import android.app.KeyguardManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.wifi.WifiInfo
import android.net.wifi.WifiManager
import android.os.BatteryManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import androidx.core.content.ContextCompat

/**
 * This phone's battery and network, read here and handed to the node.
 *
 * The desktop nodes read the same facts themselves, but on Android SELinux
 * keeps an app out of `/sys/class/power_supply`, so Rust has nothing to read;
 * the platform's broadcasts and callbacks are the only source. Both arrive on the main thread, so the latest reading
 * is kept in one place without locking, and the node hears it only when it
 * actually differs — a capabilities callback fires for many changes nobody
 * reports.
 */
class DeviceStatus(context: Context) {
    private data class Reading(
        val battery: Int = UNKNOWN_BATTERY,
        val power: String = "",
        val network: String = "",
        val signalDbm: Int = UNKNOWN_SIGNAL,
    )

    private val context = context.applicationContext
    private val main = Handler(Looper.getMainLooper())
    private val connectivity = this.context.getSystemService(ConnectivityManager::class.java)
    private var reading = Reading()
    private var reported: Reading? = null
    private var started = false

    private val battery = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) = onBattery(intent)
    }

    /**
     * Whether someone is using the phone: screen on and unlocked. The screen
     * times out without touches, so this is also "touched recently" without
     * reading anyone's input. Unlock arrives as USER_PRESENT; a phone with no
     * lock screen sends only SCREEN_ON, which is why the keyguard is asked.
     */
    private val screen = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) = onScreen()
    }
    private var inUse: Boolean? = null

    private val network = object : ConnectivityManager.NetworkCallback() {
        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) = onNetwork(caps)

        override fun onLost(network: Network) {
            update(reading.copy(network = "", signalDbm = UNKNOWN_SIGNAL))
            // Sessions leave the network path now rather than after its
            // silence, so a Bluetooth standby takes over at once.
            CursioNative.nativeNetworkLost()
        }
    }

    /**
     * Begin listening. Both registrations deliver the current state at once —
     * the battery broadcast is sticky and the default-network callback opens
     * with the network in use — so the node has a reading straight away.
     */
    fun start() {
        if (started) return
        started = true
        val sticky = ContextCompat.registerReceiver(
            context,
            battery,
            IntentFilter(Intent.ACTION_BATTERY_CHANGED),
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        sticky?.let(::onBattery)
        ContextCompat.registerReceiver(
            context,
            screen,
            IntentFilter().apply {
                addAction(Intent.ACTION_SCREEN_ON)
                addAction(Intent.ACTION_SCREEN_OFF)
                addAction(Intent.ACTION_USER_PRESENT)
            },
            ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        onScreen()
        try {
            connectivity.registerDefaultNetworkCallback(network, main)
        } catch (e: RuntimeException) {
            // Android caps an app's network callbacks; past the cap this throws
            // rather than failing quietly. The node then reports no network.
            Log.w(TAG, "watching the default network was refused", e)
        }
    }

    fun stop() {
        if (!started) return
        started = false
        context.unregisterReceiver(battery)
        context.unregisterReceiver(screen)
        inUse = null
        try {
            connectivity.unregisterNetworkCallback(network)
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "the network callback was not registered", e)
        }
    }

    private fun onBattery(intent: Intent) {
        val level = intent.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
        val scale = intent.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
        val percent = if (level >= 0 && scale > 0) level * 100 / scale else UNKNOWN_BATTERY
        val plugged = intent.getIntExtra(BatteryManager.EXTRA_PLUGGED, -1)
        val status = intent.getIntExtra(BatteryManager.EXTRA_STATUS, -1)
        val power = when {
            plugged < 0 -> ""
            plugged == 0 -> "battery"
            status == BatteryManager.BATTERY_STATUS_FULL -> "mains"
            else -> "charging"
        }
        update(reading.copy(battery = percent, power = power))
    }

    private fun onNetwork(caps: NetworkCapabilities) {
        val kind = when {
            caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> "wifi"
            caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> "cellular"
            caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> "wired"
            else -> "unknown"
        }
        val signal = if (kind == "wifi") wifiRssi(caps) else UNKNOWN_SIGNAL
        update(reading.copy(network = kind, signalDbm = signal))
    }

    /**
     * From Android 12 the capabilities carry the `WifiInfo` themselves, and
     * the callback fires again as the signal moves. Before that they carry
     * nothing, and `WifiManager` is the only place left to ask.
     */
    private fun wifiRssi(caps: NetworkCapabilities): Int {
        val info = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            caps.transportInfo as? WifiInfo
        } else {
            val wifi = context.getSystemService(WifiManager::class.java)
            @Suppress("DEPRECATION")
            wifi?.connectionInfo
        }
        val rssi = info?.rssi ?: return UNKNOWN_SIGNAL
        return if (rssi == INVALID_RSSI) UNKNOWN_SIGNAL else rssi
    }

    private fun onScreen() {
        val power = context.getSystemService(PowerManager::class.java)
        val keyguard = context.getSystemService(KeyguardManager::class.java)
        val now = power?.isInteractive == true && keyguard?.isKeyguardLocked != true
        if (now == inUse) return
        inUse = now
        Node.reportInUse(now)
    }

    private fun update(next: Reading) {
        reading = next
        if (next == reported) return
        reported = next
        Node.reportStatus(next.battery, next.power, next.network, next.signalDbm)
    }

    private companion object {
        const val TAG = "Cursio"
        const val UNKNOWN_BATTERY = -1
        const val UNKNOWN_SIGNAL = Int.MIN_VALUE

        // `WifiInfo.INVALID_RSSI` is hidden API; this is what it reads while
        // the radio has no measurement yet.
        const val INVALID_RSSI = -127
    }
}
