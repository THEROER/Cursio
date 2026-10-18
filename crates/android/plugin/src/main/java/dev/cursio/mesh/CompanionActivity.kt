package dev.cursio.mesh

import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.companion.AssociationInfo
import android.companion.AssociationRequest
import android.companion.BluetoothDeviceFilter
import android.companion.CompanionDeviceManager
import android.content.Intent
import android.content.IntentSender
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log

/**
 * Shows the system's "Allow Cursio to manage <computer>?" dialog, once per
 * paired computer not yet associated, and finishes — with [EXTRA_ERROR] set
 * when none came of it.
 *
 * A screen of its own because the dialog comes back as an [IntentSender] whose
 * answer reaches only the Activity that launched it, and the Tauri Activity's
 * results are not the plugin's to read. [MeshPlugin] starts this for a result,
 * so its request resolves when the user is back, as every other grant does.
 *
 * One computer per request, by address, with `setSingleDevice(true)`: only
 * then does the system's discovery offer a device that is already *bonded*
 * (read from Android 10's CompanionDeviceManager, the same in later releases).
 * Otherwise it lists only what an inquiry finds, and a computer is not
 * discoverable — so the dialog never appeared, and since Android never gives
 * up on its own the transparent screen stayed until the user pressed Back.
 * [TIMEOUT_MS] still ends a request the system leaves unanswered.
 */
class CompanionActivity : Activity() {
    private val main = Handler(Looper.getMainLooper())
    private var cdm: CompanionDeviceManager? = null
    private val pending = ArrayDeque<BluetoothDevice>()
    private var lastError: String? = null
    private var finished = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Recreated after all (process death): the callback that would have
        // shown the dialog went with the old instance, so nothing would come.
        if (savedInstanceState != null) {
            finishWith("the window was recreated before Android answered; try again")
            return
        }
        cdm = getSystemService(CompanionDeviceManager::class.java)
        if (cdm == null) {
            finishWith("this phone has no companion device support")
            return
        }
        pending.addAll(ComputerCompanion.unassociatedComputers(this))
        if (pending.isEmpty()) {
            finishWith("pair this phone with your computer over Bluetooth first")
            return
        }
        next()
    }

    private fun next() {
        main.removeCallbacksAndMessages(null)
        val computer = pending.removeFirstOrNull()
        if (computer == null) {
            finishWith(if (ComputerCompanion.associated(this)) null else lastError ?: "not allowed")
            return
        }
        val request = AssociationRequest.Builder()
            .addDeviceFilter(BluetoothDeviceFilter.Builder().setAddress(computer.address).build())
            .setSingleDevice(true)
            .build()
        val callback = object : CompanionDeviceManager.Callback() {
            @Deprecated("Android 13 calls onAssociationPending instead")
            override fun onDeviceFound(chooserLauncher: IntentSender) = launch(chooserLauncher)

            override fun onAssociationPending(intentSender: IntentSender) = launch(intentSender)

            override fun onAssociationCreated(associationInfo: AssociationInfo) = Unit

            override fun onFailure(error: CharSequence?) {
                Log.w(TAG, "the companion association with ${computer.address} failed: $error")
                lastError = "Android refused: $error"
                next()
            }
        }
        main.postDelayed({
            Log.w(TAG, "Android offered no dialog for ${computer.address}")
            lastError = "Android did not offer your computer; switch its Bluetooth on and keep it near"
            next()
        }, TIMEOUT_MS)
        val manager = cdm ?: return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            manager.associate(request, mainExecutor, callback)
        } else {
            @Suppress("DEPRECATION")
            manager.associate(request, callback, null)
        }
    }

    private fun launch(sender: IntentSender) {
        // The dialog is up: from here the user decides, however long it takes.
        main.removeCallbacksAndMessages(null)
        try {
            startIntentSenderForResult(sender, REQUEST, null, 0, 0, 0)
        } catch (e: IntentSender.SendIntentException) {
            Log.w(TAG, "cannot show the companion dialog", e)
            lastError = "cannot show Android's dialog"
            next()
        }
    }

    /**
     * The dialog's answer for one computer. Below Android 13 it is the only
     * one (the chosen device in `EXTRA_DEVICE`, no callback); the association
     * itself is read afterwards, so the extra is not needed.
     */
    @Deprecated("the platform callback this overrides is deprecated")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST) return
        if (resultCode != RESULT_OK) lastError = "not allowed"
        next()
    }

    override fun onDestroy() {
        main.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    private fun finishWith(error: String?) {
        if (finished) return
        finished = true
        main.removeCallbacksAndMessages(null)
        ComputerCompanion.observe(this)
        setResult(RESULT_OK, Intent().apply { if (error != null) putExtra(EXTRA_ERROR, error) })
        if (!isFinishing) finish()
    }

    companion object {
        private const val TAG = "Cursio"
        private const val REQUEST = 1
        private const val TIMEOUT_MS = 20_000L
        const val EXTRA_ERROR = "dev.cursio.mesh.COMPANION_ERROR"
    }
}
