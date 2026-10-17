package dev.cursio.mesh

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.MediaStore
import java.io.File

/**
 * What the phone does with a share a machine sent it. Called from the node's
 * own thread through `CursioNative.onShare`, and answers in the words
 * `crates/android/src/share.rs` turns into the sender's outcome.
 *
 * A link is never opened directly: Android does not let an app in the
 * background start an activity, so it becomes a notification that opens it on
 * a tap. Text and copies go on the clipboard, which an app may write from the
 * background (only reading it is restricted). A file goes into Downloads
 * through MediaStore, the only way an app reaches that folder since Android 10.
 */
object ShareReceiver {
    private const val CHANNEL_ID = "cursio-share"

    /** The application context, set before the node can deliver anything. */
    @Volatile
    var context: Context? = null

    fun act(from: String, kind: String, text: String, path: String, name: String, mime: String, action: String): String {
        val ctx = context ?: return "failed:the app is not running"
        return try {
            when {
                kind == "file" -> keep(ctx, from, File(path), name, mime.ifEmpty { null }, action == "open")
                action == "copy" || kind == "text" -> {
                    copy(ctx, text)
                    notify(ctx, "Copied from $from", text.take(80), null)
                    "copied"
                }
                else -> {
                    val open = Intent(Intent.ACTION_VIEW, Uri.parse(text)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    notify(ctx, "Open link from $from", text, open)
                    "offered"
                }
            }
        } catch (e: Exception) {
            "failed:${e.message ?: e.javaClass.simpleName}"
        }
    }

    private fun copy(ctx: Context, text: String) {
        val clipboard = ctx.getSystemService(ClipboardManager::class.java)
        clipboard.setPrimaryClip(ClipData.newPlainText("Cursio", text))
    }

    private fun keep(ctx: Context, from: String, file: File, name: String, mime: String?, open: Boolean): String {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            return "refused:keeping files needs Android 10 or later"
        }
        val resolver = ctx.contentResolver
        val values = ContentValues().apply {
            put(MediaStore.Downloads.DISPLAY_NAME, name)
            if (mime != null) put(MediaStore.Downloads.MIME_TYPE, mime)
            put(MediaStore.Downloads.IS_PENDING, 1)
        }
        val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
            ?: return "failed:Downloads refused the file"
        resolver.openOutputStream(uri).use { out ->
            requireNotNull(out) { "Downloads gave nothing to write to" }
            file.inputStream().use { it.copyTo(out) }
        }
        resolver.update(uri, ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }, null, null)
        // MediaStore renames on a clash ("photo (1).jpg"); the sender is told
        // the name the file really has.
        val saved = resolver.query(uri, arrayOf(MediaStore.Downloads.DISPLAY_NAME), null, null, null)?.use {
            if (it.moveToFirst()) it.getString(0) else null
        } ?: name
        val view = Intent(Intent.ACTION_VIEW)
            .setDataAndType(uri, mime ?: resolver.getType(uri))
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        notify(ctx, if (open) "Open $saved from $from" else "Saved $saved from $from", "In Downloads", view)
        return if (open) "offered" else "saved:$saved"
    }

    private fun notify(ctx: Context, title: String, text: String, tap: Intent?) {
        val manager = ctx.getSystemService(NotificationManager::class.java)
        // High importance: a link someone just sent is wanted now, as a
        // heads-up, not found later in the shade.
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Shared from your machines", NotificationManager.IMPORTANCE_HIGH),
        )
        val id = (System.nanoTime() and 0x7fffffff).toInt()
        val builder = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setContentTitle(title)
            .setContentText(text)
            .setAutoCancel(true)
        if (tap != null) {
            builder.setContentIntent(
                PendingIntent.getActivity(ctx, id, tap, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT),
            )
        }
        manager.notify(id, builder.build())
    }
}
