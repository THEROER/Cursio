package dev.cursio.mesh

import android.annotation.SuppressLint
import android.bluetooth.BluetoothClass
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.companion.CompanionDeviceManager
import android.content.Context
import android.os.Build
import android.util.Log

/**
 * Cursio as the companion app of the computers this phone is paired with.
 *
 * HyperOS kills background apps that are not on its own Autostart list, and
 * resets that grant on its own (the node lost it twice in a day); the battery
 * exemption only helps against Android's Doze. A companion association is the
 * one mechanism Android offers an ordinary app for "this app serves that
 * device": the system grants it background running and data, lets it start a
 * foreground service from the background, and — from Android 12 — binds
 * [CompanionService] while the computer is near, which both wakes a killed
 * node and keeps its process ranked as in use.
 *
 * The association is a plain Bluetooth one. Android has a "computer" profile
 * (what Phone Link holds) and self-managed associations, but both are
 * `signature|privileged` — checked on the phones, not assumed — so an app
 * from outside the system image cannot request either. A Bluetooth filter
 * over the computers already paired here needs no extra permission, and those
 * are exactly the machines a mesh link by Bluetooth reaches.
 */
object ComputerCompanion {
    private const val TAG = "Cursio"

    private fun manager(context: Context): CompanionDeviceManager? =
        context.getSystemService(CompanionDeviceManager::class.java)

    /** MAC addresses this app is associated with, lower case as CDM reports them. */
    fun associatedAddresses(context: Context): List<String> {
        val cdm = manager(context) ?: return emptyList()
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            cdm.myAssociations.mapNotNull { it.deviceMacAddress?.toString()?.lowercase() }
        } else {
            @Suppress("DEPRECATION")
            cdm.associations.map { it.lowercase() }
        }
    }

    fun associated(context: Context): Boolean = associatedAddresses(context).isNotEmpty()

    /**
     * The computers paired over Bluetooth that are not associated yet. Empty
     * without the Nearby devices grant on Android 12+, which reading bonded
     * devices needs.
     */
    @SuppressLint("MissingPermission")
    fun unassociatedComputers(context: Context): List<BluetoothDevice> {
        val adapter = context.getSystemService(BluetoothManager::class.java)?.adapter ?: return emptyList()
        val known = associatedAddresses(context).toSet()
        return try {
            adapter.bondedDevices
                .filter { it.bluetoothClass?.majorDeviceClass == BluetoothClass.Device.Major.COMPUTER }
                .filter { it.address.lowercase() !in known }
        } catch (e: SecurityException) {
            Log.i(TAG, "cannot list paired computers without the Nearby devices grant", e)
            emptyList()
        }
    }

    /**
     * Ask the system to tell [CompanionService] when an associated computer
     * comes near. Repeated on every service start: it is idempotent, and an
     * association made by an older build never asked.
     */
    @SuppressLint("MissingPermission")
    fun observe(context: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return
        val cdm = manager(context) ?: return
        for (address in associatedAddresses(context)) {
            try {
                @Suppress("DEPRECATION")
                cdm.startObservingDevicePresence(address.uppercase())
            } catch (e: RuntimeException) {
                // DeviceNotAssociatedException, or a skin refusing presence for
                // a classic Bluetooth device: the association still lifts the
                // background limits, only the wake-up is lost.
                Log.w(TAG, "cannot observe $address coming near", e)
            }
        }
    }
}
