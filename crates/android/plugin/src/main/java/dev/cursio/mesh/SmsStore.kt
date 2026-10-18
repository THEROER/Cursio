package dev.cursio.mesh

import android.Manifest
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.database.ContentObserver
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.ContactsContract
import android.provider.Telephony
import android.telephony.SmsManager
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/**
 * This phone's text messages, read and sent for a desktop
 * (`crates/core/src/messages_session.rs` is the other half).
 *
 * Read straight from the telephony provider rather than kept in a copy:
 * Android already stores every SMS and the text of every MMS, and a copy would
 * be one more thing to keep in step with the messaging app the user actually
 * uses. An app that is not the default SMS app may read all of it with
 * `READ_SMS` and send with `SEND_SMS` — the system then records what it sent
 * in the provider itself — but may not write to it, which is why marking a
 * conversation read can only be refused.
 *
 * A change is noticed by observing the provider, not by `SMS_RECEIVED`: the
 * broadcast covers only incoming SMS, while the observer also sees a sent
 * message turn from queued to sent or failed, and MMS. HyperOS also delivers
 * the observer to an app it would not wake for the broadcast.
 *
 * Requests arrive from the node's threads as one JSON object and answer as
 * one; the node turns both into the wire vocabulary. One entry point keeps
 * the JNI surface to a single method R8 must keep.
 */
object SmsStore {
    private const val TAG = "Cursio"
    private const val SENT_ACTION = "dev.cursio.mesh.SMS_SENT"

    /** What a desktop is told when it asks to mark a conversation read. */
    private const val NOT_DEFAULT_APP =
        "Android lets only the phone's default SMS app mark messages read"

    @Volatile
    var context: Context? = null

    private var observing = false
    private val main = Handler(Looper.getMainLooper())

    /** Collapses a burst of provider changes — a multipart SMS is several — into one. */
    private val tell = Runnable {
        try {
            CursioNative.nativeSmsChanged()
        } catch (e: UnsatisfiedLinkError) {
            Log.w(TAG, "the node is not loaded; a message change goes untold", e)
        }
    }

    /** Start telling the node when the provider changes. Safe to call again. */
    fun watch(context: Context) {
        main.post {
            if (observing || !granted(context, Manifest.permission.READ_SMS)) return@post
            val app = context.applicationContext
            app.contentResolver.registerContentObserver(
                Uri.parse("content://mms-sms/"),
                true,
                object : ContentObserver(main) {
                    override fun onChange(selfChange: Boolean) {
                        main.removeCallbacks(tell)
                        main.postDelayed(tell, 500)
                    }
                },
            )
            val sent = object : BroadcastReceiver() {
                override fun onReceive(context: Context, intent: Intent) {
                    if (resultCode != android.app.Activity.RESULT_OK) {
                        Log.w(TAG, "the phone could not send an SMS (result $resultCode)")
                    }
                }
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                app.registerReceiver(sent, IntentFilter(SENT_ACTION), Context.RECEIVER_NOT_EXPORTED)
            } else {
                app.registerReceiver(sent, IntentFilter(SENT_ACTION))
            }
            observing = true
        }
    }

    /** Answer one request from a desktop; see the class comment for the shape. */
    fun handle(request: String): String {
        val ctx = context ?: return error("the app is not running")
        return try {
            val req = JSONObject(request)
            when (req.getString("op")) {
                "conversations" -> conversations(ctx, req.getInt("limit"), before(req))
                "messages" -> messages(ctx, req.getLong("thread"), req.getInt("limit"), before(req))
                "send" -> send(ctx, req.getJSONArray("addresses"), req.getString("body"))
                "mark_read" -> markRead(ctx, req.getLong("thread"))
                else -> error("unknown request")
            }
        } catch (e: SecurityException) {
            error("the phone refused: ${e.message ?: "permission denied"}")
        } catch (e: Exception) {
            Log.w(TAG, "answering a messages request failed", e)
            error(e.message ?: e.javaClass.simpleName)
        }
    }

    private fun before(req: JSONObject): Long? = if (req.isNull("before")) null else req.getLong("before")

    private fun conversations(ctx: Context, limit: Int, before: Long?): String {
        if (!granted(ctx, Manifest.permission.READ_SMS)) return error(askForMessages())
        val addresses = canonicalAddresses(ctx)
        val names = Names(ctx)
        val out = JSONArray()
        ctx.contentResolver.query(
            Uri.parse("content://mms-sms/conversations?simple=true"),
            arrayOf("_id", "date", "recipient_ids", "snippet", "read"),
            before?.let { "date < ?" },
            before?.let { arrayOf(it.toString()) },
            "date DESC",
        )?.use { c ->
            while (c.moveToNext() && out.length() < limit) {
                val recipients = (c.getString(2) ?: "").split(' ').mapNotNull { addresses[it] }
                out.put(
                    JSONObject()
                        .put("thread", c.getLong(0))
                        .put("addresses", JSONArray(recipients))
                        .put("names", JSONArray(recipients.map { names.of(it) }))
                        .put("snippet", c.getString(3) ?: "")
                        .put("date", c.getLong(1))
                        .put("unread", c.getInt(4) == 0),
                )
            }
        }
        return JSONObject().put("conversations", out).toString()
    }

    private fun canonicalAddresses(ctx: Context): Map<String, String> {
        val map = HashMap<String, String>()
        ctx.contentResolver.query(
            Uri.parse("content://mms-sms/canonical-addresses"),
            arrayOf("_id", "address"),
            null,
            null,
            null,
        )?.use { c ->
            while (c.moveToNext()) map[c.getLong(0).toString()] = c.getString(1) ?: ""
        }
        return map
    }

    private fun messages(ctx: Context, thread: Long, limit: Int, before: Long?): String {
        if (!granted(ctx, Manifest.permission.READ_SMS)) return error(askForMessages())
        val all = ArrayList<JSONObject>()
        val dateFilter = if (before != null) " AND date < ?" else ""
        ctx.contentResolver.query(
            Telephony.Sms.CONTENT_URI,
            arrayOf("_id", "address", "body", "date", "type", "read"),
            "thread_id = ?$dateFilter",
            listOfNotNull(thread.toString(), before?.toString()).toTypedArray(),
            "date DESC",
        )?.use { c ->
            while (c.moveToNext() && all.size < limit) {
                all.add(
                    JSONObject()
                        .put("id", c.getLong(0))
                        .put("mms", false)
                        .put("thread", thread)
                        .put("address", c.getString(1) ?: "")
                        .put("body", c.getString(2) ?: "")
                        .put("date", c.getLong(3))
                        .put("kind", smsKind(c.getInt(4)))
                        .put("read", c.getInt(5) != 0)
                        .put("attachment", false),
                )
            }
        }
        // MMS dates are seconds, SMS dates milliseconds.
        ctx.contentResolver.query(
            Telephony.Mms.CONTENT_URI,
            arrayOf("_id", "date", "msg_box", "read"),
            "thread_id = ?$dateFilter",
            listOfNotNull(thread.toString(), before?.let { (it / 1000).toString() }).toTypedArray(),
            "date DESC",
        )?.use { c ->
            var taken = 0
            while (c.moveToNext() && taken < limit) {
                val id = c.getLong(0)
                val box = c.getInt(2)
                val (text, attachment) = mmsParts(ctx, id)
                all.add(
                    JSONObject()
                        .put("id", id)
                        .put("mms", true)
                        .put("thread", thread)
                        .put("address", mmsAddress(ctx, id, box == Telephony.Mms.MESSAGE_BOX_INBOX))
                        .put("body", text)
                        .put("date", c.getLong(1) * 1000)
                        .put("kind", mmsKind(box))
                        .put("read", c.getInt(3) != 0)
                        .put("attachment", attachment),
                )
                taken++
            }
        }
        all.sortByDescending { it.getLong("date") }
        return JSONObject().put("messages", JSONArray(all.take(limit))).toString()
    }

    /** The text of an MMS, and whether it carried anything else. */
    private fun mmsParts(ctx: Context, id: Long): Pair<String, Boolean> {
        val text = StringBuilder()
        var attachment = false
        ctx.contentResolver.query(
            Uri.parse("content://mms/part"),
            arrayOf("_id", "ct", "text", "_data"),
            "mid = ?",
            arrayOf(id.toString()),
            null,
        )?.use { c ->
            while (c.moveToNext()) {
                when (c.getString(1)) {
                    "text/plain" -> {
                        val inline = c.getString(2)
                        val body = if (inline != null || c.getString(3) == null) {
                            inline ?: ""
                        } else {
                            ctx.contentResolver.openInputStream(Uri.parse("content://mms/part/${c.getLong(0)}"))
                                ?.use { it.readBytes().toString(Charsets.UTF_8) } ?: ""
                        }
                        if (text.isNotEmpty() && body.isNotEmpty()) text.append('\n')
                        text.append(body)
                    }
                    "application/smil" -> {}
                    else -> attachment = true
                }
            }
        }
        return text.toString() to attachment
    }

    /** The sender of a received MMS, or the first recipient of a sent one. */
    private fun mmsAddress(ctx: Context, id: Long, received: Boolean): String {
        // PduHeaders.FROM and PduHeaders.TO.
        val type = if (received) 137 else 151
        ctx.contentResolver.query(
            Uri.parse("content://mms/$id/addr"),
            arrayOf("address"),
            "type = ?",
            arrayOf(type.toString()),
            null,
        )?.use { c ->
            if (c.moveToFirst()) return c.getString(0) ?: ""
        }
        return ""
    }

    private fun smsKind(type: Int): String = when (type) {
        Telephony.Sms.MESSAGE_TYPE_INBOX -> "inbox"
        Telephony.Sms.MESSAGE_TYPE_SENT -> "sent"
        Telephony.Sms.MESSAGE_TYPE_OUTBOX, Telephony.Sms.MESSAGE_TYPE_QUEUED -> "outbox"
        Telephony.Sms.MESSAGE_TYPE_FAILED -> "failed"
        else -> "other"
    }

    private fun mmsKind(box: Int): String = when (box) {
        Telephony.Mms.MESSAGE_BOX_INBOX -> "inbox"
        Telephony.Mms.MESSAGE_BOX_SENT -> "sent"
        Telephony.Mms.MESSAGE_BOX_OUTBOX -> "outbox"
        else -> "other"
    }

    private fun send(ctx: Context, addresses: JSONArray, body: String): String {
        if (!granted(ctx, Manifest.permission.SEND_SMS)) return error(askForMessages())
        val manager = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            ctx.getSystemService(SmsManager::class.java)
        } else {
            @Suppress("DEPRECATION")
            SmsManager.getDefault()
        } ?: return error("this phone cannot send SMS")
        val parts = manager.divideMessage(body)
        for (i in 0 until addresses.length()) {
            val sent = ArrayList<PendingIntent>()
            repeat(parts.size) {
                sent.add(
                    PendingIntent.getBroadcast(
                        ctx,
                        0,
                        Intent(SENT_ACTION).setPackage(ctx.packageName),
                        PendingIntent.FLAG_IMMUTABLE,
                    ),
                )
            }
            manager.sendMultipartTextMessage(addresses.getString(i), null, parts, sent, null)
        }
        return JSONObject().put("sent", true).toString()
    }

    private fun markRead(ctx: Context, thread: Long): String {
        if (!granted(ctx, Manifest.permission.READ_SMS)) return error(askForMessages())
        val unread = ctx.contentResolver.query(
            Telephony.Sms.CONTENT_URI,
            arrayOf("_id"),
            "thread_id = ? AND read = 0",
            arrayOf(thread.toString()),
            null,
        )?.use { it.count } ?: 0
        if (unread == 0) return JSONObject().put("done", true).toString()
        val changed = try {
            ctx.contentResolver.update(
                Telephony.Sms.CONTENT_URI,
                ContentValues().apply { put("read", 1) },
                "thread_id = ? AND read = 0",
                arrayOf(thread.toString()),
            )
        } catch (e: SecurityException) {
            0
        }
        return if (changed > 0) JSONObject().put("done", true).toString() else error(NOT_DEFAULT_APP)
    }

    private fun askForMessages(): String =
        "allow Cursio to read and send SMS on the phone: open Cursio there and grant Messages"

    private fun error(message: String): String = JSONObject().put("error", message).toString()

    fun granted(ctx: Context, permission: String): Boolean =
        ctx.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED

    /** Contact names by address, looked up once per request. */
    private class Names(private val ctx: Context) {
        private val known = HashMap<String, String>()
        private val allowed = granted(ctx, Manifest.permission.READ_CONTACTS)

        fun of(address: String): String {
            if (!allowed || address.isEmpty()) return address
            return known.getOrPut(address) {
                val uri = Uri.withAppendedPath(ContactsContract.PhoneLookup.CONTENT_FILTER_URI, Uri.encode(address))
                ctx.contentResolver.query(uri, arrayOf(ContactsContract.PhoneLookup.DISPLAY_NAME), null, null, null)
                    ?.use { c -> if (c.moveToFirst()) c.getString(0) else null }
                    ?: address
            }
        }
    }
}
