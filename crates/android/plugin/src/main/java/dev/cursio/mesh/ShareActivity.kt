package dev.cursio.mesh

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.OpenableColumns
import android.widget.Toast
import org.json.JSONArray
import java.io.File
import java.util.UUID

/**
 * "Share → Cursio" from any app: pick a machine and what it should do — open
 * the link or file there, put it on its clipboard, or keep the file.
 *
 * A dialog over whatever app the user is in, not the Cursio window: sharing
 * is a two-tap errand, and opening the full app for it would take the user
 * out of what they were doing.
 *
 * A shared file arrives as a content URI that is readable only while this
 * activity runs, and the transfer outlives it — so it is copied into the
 * app's cache first and that copy is what the node sends.
 */
class ShareActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (!Node.init(this)) {
            done(Node.loadError ?: "Cursio could not start")
            return
        }
        // Only when it is not running: a restart recomputes the service's
        // types, and from here that would drop the microphone a running one
        // was given from the window.
        if (!Node.isRunning()) MeshService.start(this, false)

        val targets = JSONArray(CursioNative.nativeShareTargets())
        if (targets.length() == 0) {
            done("No computer is connected to this phone")
            return
        }
        val ids = (0 until targets.length()).map { targets.getJSONArray(it).getString(0) }
        val names = (0 until targets.length()).map { targets.getJSONArray(it).getString(1) }

        val files = streams()
        val text = intent.getStringExtra(Intent.EXTRA_TEXT) ?: intent.getStringExtra(Intent.EXTRA_SUBJECT) ?: ""
        if (files.isEmpty() && text.isBlank()) {
            done("Nothing to send")
            return
        }

        var chosen = 0
        val dialog = AlertDialog.Builder(this, android.R.style.Theme_DeviceDefault_Dialog_Alert)
            .setTitle("Send to")
            .setSingleChoiceItems(names.toTypedArray(), 0) { _, which -> chosen = which }
            .setPositiveButton("Open there") { _, _ -> send(ids[chosen], names[chosen], files, text, "open") }
            .setNeutralButton(if (files.isEmpty()) "Copy there" else "Save there") { _, _ ->
                send(ids[chosen], names[chosen], files, text, if (files.isEmpty()) "copy" else "save")
            }
            .setNegativeButton("Cancel") { _, _ -> finish() }
            .setOnCancelListener { finish() }
            .create()
        dialog.show()
    }

    private fun streams(): List<Uri> {
        @Suppress("DEPRECATION")
        return when (intent.action) {
            Intent.ACTION_SEND -> listOfNotNull(
                if (Build.VERSION.SDK_INT >= 33) intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
                else intent.getParcelableExtra(Intent.EXTRA_STREAM),
            )
            Intent.ACTION_SEND_MULTIPLE ->
                (if (Build.VERSION.SDK_INT >= 33) intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
                else intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM)) ?: emptyList()
            else -> emptyList()
        }
    }

    private fun send(peer: String, name: String, files: List<Uri>, text: String, action: String) {
        Thread({
            val error = if (files.isEmpty()) {
                CursioNative.nativeShare(peer, text, "", "", "", action)
            } else {
                files.map { sendFile(peer, it, action) }.firstOrNull { it.isNotEmpty() } ?: ""
            }
            runOnUiThread { done(error.ifEmpty { "Sent to $name" }) }
        }, "cursio-share-send").start()
    }

    private fun sendFile(peer: String, uri: Uri, action: String): String {
        val dir = File(cacheDir, "share-out").apply { mkdirs() }
        // The node reads each copy after this activity is gone; a day is far
        // longer than any transfer, and the folder never grows without bound.
        val stale = System.currentTimeMillis() - 24 * 60 * 60 * 1000
        dir.listFiles()?.filter { it.lastModified() < stale }?.forEach { it.delete() }

        val name = contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
            if (it.moveToFirst()) it.getString(0) else null
        } ?: uri.lastPathSegment ?: "shared file"
        val copy = File(dir, UUID.randomUUID().toString())
        return try {
            contentResolver.openInputStream(uri).use { input ->
                requireNotNull(input) { "the app that shared it gave nothing to read" }
                copy.outputStream().use { input.copyTo(it) }
            }
            CursioNative.nativeShare(peer, "", copy.absolutePath, name, contentResolver.getType(uri) ?: "", action)
        } catch (e: Exception) {
            copy.delete()
            "Could not read $name: ${e.message}"
        }
    }

    private fun done(message: String) {
        Toast.makeText(this, message, Toast.LENGTH_SHORT).show()
        finish()
    }
}
