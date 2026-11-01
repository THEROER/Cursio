package dev.cursio.mesh

import android.app.Activity
import android.app.AlertDialog
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.widget.Toast
import org.json.JSONObject

/**
 * This phone's clipboard to a computer, in one tap: from [ClipboardTile] or
 * the service notification's "Send clipboard".
 *
 * An activity, invisible, only because Android 10+ lets an app read the
 * clipboard while one of its windows has focus and at no other time. So it
 * reads once focus arrives — in `onCreate` the window has none yet and the
 * read answers null — hands the text to the node and finishes. Nothing else
 * in the app reads the clipboard.
 *
 * The node says where it goes (`clipboard_session::phone`): the computer the
 * user was at last, else the only one, else the user picks here.
 */
class ClipboardSendActivity : Activity() {
    private var read = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (!Node.init(this)) {
            done(Node.loadError ?: "Cursio could not start")
            return
        }
        if (!Node.isRunning()) {
            done("Cursio is not running")
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus || read || isFinishing) return
        read = true
        val text = clipText()
        if (text.isNullOrEmpty()) {
            done("The clipboard holds no text")
            return
        }
        val destination = JSONObject(CursioNative.nativeClipboardDestination())
        when {
            destination.has("to") -> {
                val to = destination.getJSONArray("to")
                send(to.getString(0), text)
            }
            destination.has("pick") -> pick(destination.getJSONArray("pick"), text)
            else -> done(destination.optString("nowhere", "No computer can take it"))
        }
    }

    private fun clipText(): String? {
        val clipboard = getSystemService(ClipboardManager::class.java)
        val clip = clipboard.primaryClip ?: return null
        if (clip.itemCount == 0) return null
        return clip.getItemAt(0).coerceToText(this)?.toString()
    }

    private fun pick(computers: org.json.JSONArray, text: String) {
        val ids = (0 until computers.length()).map { computers.getJSONArray(it).getString(0) }
        val names = (0 until computers.length()).map { computers.getJSONArray(it).getString(1) }
        AlertDialog.Builder(this, android.R.style.Theme_DeviceDefault_Dialog_Alert)
            .setTitle("Send clipboard to")
            .setItems(names.toTypedArray()) { _, which -> send(ids[which], text) }
            .setNegativeButton("Cancel") { _, _ -> finish() }
            .setOnCancelListener { finish() }
            .show()
    }

    private fun send(peer: String, text: String) {
        val answer = CursioNative.nativeSendClipboard(peer, text)
        done(if (answer.startsWith("sent:")) "Clipboard sent to ${answer.removePrefix("sent:")}" else answer)
    }

    private fun done(message: String) {
        Toast.makeText(this, message, Toast.LENGTH_SHORT).show()
        finish()
    }

    companion object {
        fun intent(context: Context): Intent =
            Intent(context, ClipboardSendActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_NO_ANIMATION)
    }
}
