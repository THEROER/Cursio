package dev.cursio.mesh

import android.companion.AssociationInfo
import android.companion.CompanionDeviceService
import android.companion.DevicePresenceEvent
import android.util.Log
import androidx.annotation.RequiresApi

/**
 * Bound by the system while an associated computer is near (see [ComputerCompanion]).
 *
 * That binding is the point: it brings back a node HyperOS killed, without the
 * Autostart grant, and while it lasts the process counts as serving a device.
 * The start is a background one, allowed because the association carries
 * `REQUEST_COMPANION_START_FOREGROUND_SERVICES_FROM_BACKGROUND`.
 */
@RequiresApi(31)
class CompanionService : CompanionDeviceService() {
    @Deprecated("Android 13 calls the AssociationInfo overload instead")
    override fun onDeviceAppeared(address: String) = wake(address)

    @Deprecated("Android 16 calls onDevicePresenceEvent instead")
    override fun onDeviceAppeared(associationInfo: AssociationInfo) =
        wake(associationInfo.deviceMacAddress?.toString() ?: "association ${associationInfo.id}")

    /** A classic Bluetooth computer reports `BT_CONNECTED`, never `BLE_APPEARED`. */
    @RequiresApi(36)
    override fun onDevicePresenceEvent(event: DevicePresenceEvent) {
        when (event.event) {
            DevicePresenceEvent.EVENT_BT_CONNECTED, DevicePresenceEvent.EVENT_BLE_APPEARED ->
                wake("association ${event.associationId}")
            else -> Unit
        }
    }

    private fun wake(what: String) {
        if (!Prefs.wantedRunning(this)) return
        Log.i(TAG, "$what is near; making sure the mesh runs")
        try {
            MeshService.start(this, fromForeground = false)
        } catch (e: IllegalStateException) {
            Log.w(TAG, "starting the mesh when $what came near was refused", e)
        }
    }

    private companion object {
        const val TAG = "Cursio"
    }
}
