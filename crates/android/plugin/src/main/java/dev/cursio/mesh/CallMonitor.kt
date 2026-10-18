package dev.cursio.mesh

import android.Manifest
import android.annotation.SuppressLint
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.media.AudioManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.ContactsContract
import android.provider.Settings
import android.telecom.TelecomManager
import android.telephony.TelephonyManager
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/**
 * This phone's calls, reported to the node, and a computer's requests carried
 * out on them.
 *
 * An app that is not the default dialer sees calls only through the
 * `PHONE_STATE` broadcast: idle, ringing (with the caller's number, given
 * `READ_CALL_LOG`) or off-hook. That is aggregate — "a call is up", never
 * which — so at most two calls are modelled, the one up and one waiting,
 * which is every case a person handles from a computer. Becoming the default
 * dialer would give every call by name, and take the phone's own dialer away
 * from the user; that trade is not made.
 *
 * The voice is not here at all. Android reserves call audio to system apps;
 * a computer gets it by being this phone's Bluetooth hands-free unit, which
 * needs nothing from this class.
 */
@SuppressLint("StaticFieldLeak")
object CallMonitor {
    private const val TAG = "Cursio"

    /** Everything a computer's requests need, granted by the "calls" row. */
    val PERMISSIONS = arrayOf(
        Manifest.permission.READ_PHONE_STATE,
        Manifest.permission.READ_CALL_LOG,
        Manifest.permission.ANSWER_PHONE_CALLS,
        Manifest.permission.CALL_PHONE,
        Manifest.permission.READ_CONTACTS,
    )

    private var context: Context? = null
    private var receiver: BroadcastReceiver? = null

    private var lastState = TelephonyManager.EXTRA_STATE_IDLE
    private var ringingNumber: String? = null
    private var activeNumber: String? = null
    private var activeIncoming = true
    private var waitingNumber: String? = null
    /** The number a computer asked to dial, for the off-hook that follows. */
    private var dialed: String? = null

    fun granted(context: Context): Boolean = PERMISSIONS.all {
        context.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED
    }

    fun start(context: Context) {
        val app = context.applicationContext
        this.context = app
        if (receiver == null) {
            val r = object : BroadcastReceiver() {
                override fun onReceive(context: Context, intent: Intent) = onPhoneState(intent)
            }
            // Not exported: PHONE_STATE is a protected system broadcast, which
            // reaches a non-exported receiver, and no other app should.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                app.registerReceiver(r, IntentFilter(TelephonyManager.ACTION_PHONE_STATE_CHANGED), Context.RECEIVER_NOT_EXPORTED)
            } else {
                app.registerReceiver(r, IntentFilter(TelephonyManager.ACTION_PHONE_STATE_CHANGED))
            }
            receiver = r
        }
        report()
    }

    fun stop() {
        val app = context ?: return
        receiver?.let { runCatching { app.unregisterReceiver(it) } }
        receiver = null
    }

    /** Permissions may have changed: say so to the node. */
    fun refresh() = report()

    private fun onPhoneState(intent: Intent) {
        val state = intent.getStringExtra(TelephonyManager.EXTRA_STATE) ?: return
        // Without READ_CALL_LOG the broadcast arrives twice, once without the
        // number; the copy with it is the one that counts.
        @Suppress("DEPRECATION")
        val number = intent.getStringExtra(TelephonyManager.EXTRA_INCOMING_NUMBER)
        when (state) {
            TelephonyManager.EXTRA_STATE_RINGING -> {
                if (lastState == TelephonyManager.EXTRA_STATE_OFFHOOK) {
                    if (number != null) waitingNumber = number
                } else if (number != null || ringingNumber == null) {
                    ringingNumber = number ?: ringingNumber
                }
            }
            TelephonyManager.EXTRA_STATE_OFFHOOK -> when {
                lastState == TelephonyManager.EXTRA_STATE_RINGING && waitingNumber == null -> {
                    activeNumber = ringingNumber
                    activeIncoming = true
                    ringingNumber = null
                }
                lastState == TelephonyManager.EXTRA_STATE_IDLE -> {
                    activeNumber = dialed
                    activeIncoming = false
                    dialed = null
                }
                else -> {
                    // A waiting call was answered or refused; the aggregate
                    // state cannot tell which, so the waiting one is dropped.
                    waitingNumber = null
                }
            }
            TelephonyManager.EXTRA_STATE_IDLE -> {
                ringingNumber = null
                activeNumber = null
                waitingNumber = null
            }
        }
        lastState = state
        report()
    }

    private fun calls(): JSONArray {
        val calls = JSONArray()
        fun add(id: Int, incoming: Boolean, state: String, number: String?) {
            calls.put(
                JSONObject()
                    .put("id", id)
                    .put("incoming", incoming)
                    .put("state", state)
                    .put("number", number ?: JSONObject.NULL)
                    .put("name", number?.let { contactName(it) } ?: JSONObject.NULL),
            )
        }
        when (lastState) {
            TelephonyManager.EXTRA_STATE_RINGING -> add(1, true, "ringing", ringingNumber)
            TelephonyManager.EXTRA_STATE_OFFHOOK -> {
                add(1, activeIncoming, "active", activeNumber)
                if (waitingNumber != null) add(2, true, "waiting", waitingNumber)
            }
        }
        return calls
    }

    private fun report() {
        val ctx = context ?: return
        val missing = PERMISSIONS.filter { ctx.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        val reason = if (missing.isEmpty()) null else "allow Cursio phone calls and contacts on the phone"
        try {
            CursioNative.nativeCallsChanged(missing.isEmpty(), reason, bluetoothName(ctx), calls().toString())
        } catch (e: UnsatisfiedLinkError) {
            Log.w(TAG, "the node is not loaded; calls not reported", e)
        }
    }

    /**
     * What a paired computer calls this phone over Bluetooth. The adapter's
     * own name needs BLUETOOTH_CONNECT; the device name it is set from does
     * not, and on every phone tried they match.
     */
    private fun bluetoothName(ctx: Context): String? =
        Settings.Global.getString(ctx.contentResolver, Settings.Global.DEVICE_NAME)
            ?: Build.MODEL

    private fun contactName(number: String): String? {
        val ctx = context ?: return null
        if (ctx.checkSelfPermission(Manifest.permission.READ_CONTACTS) != PackageManager.PERMISSION_GRANTED) return null
        val uri = Uri.withAppendedPath(ContactsContract.PhoneLookup.CONTENT_FILTER_URI, Uri.encode(number))
        return try {
            ctx.contentResolver.query(uri, arrayOf(ContactsContract.PhoneLookup.DISPLAY_NAME), null, null, null)?.use {
                if (it.moveToFirst()) it.getString(0) else null
            }
        } catch (e: SecurityException) {
            null
        }
    }

    /**
     * Carry out a computer's request. Returns why it could not be done, or
     * null when it was.
     */
    @SuppressLint("MissingPermission")
    fun act(kind: String, number: String?, muted: Boolean): String? {
        val ctx = context ?: return "the phone's call monitor is not running"
        val telecom = ctx.getSystemService(Context.TELECOM_SERVICE) as TelecomManager
        return try {
            when (kind) {
                "answer" -> {
                    if (!has(ctx, Manifest.permission.ANSWER_PHONE_CALLS)) return "allow Cursio to answer calls on the phone"
                    telecom.acceptRingingCall()
                    null
                }
                "decline", "hangup" -> {
                    if (!has(ctx, Manifest.permission.ANSWER_PHONE_CALLS)) return "allow Cursio to end calls on the phone"
                    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.P) return "ending a call needs Android 9"
                    @Suppress("DEPRECATION")
                    if (telecom.endCall()) null else "the phone had no call to end"
                }
                "dial" -> {
                    val n = number?.takeIf { it.isNotBlank() } ?: return "no number to call"
                    if (!has(ctx, Manifest.permission.CALL_PHONE)) return "allow Cursio to place calls on the phone"
                    dialed = n
                    telecom.placeCall(Uri.fromParts("tel", n, null), Bundle())
                    null
                }
                "mute" -> {
                    val audio = ctx.getSystemService(Context.AUDIO_SERVICE) as AudioManager
                    audio.isMicrophoneMute = muted
                    null
                }
                else -> "unknown request \"$kind\""
            }
        } catch (e: SecurityException) {
            Log.w(TAG, "call request $kind refused", e)
            "the phone refused: ${e.message}"
        }
    }

    private fun has(ctx: Context, permission: String) =
        ctx.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED
}
