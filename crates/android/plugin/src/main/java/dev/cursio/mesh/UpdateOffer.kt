package dev.cursio.mesh

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.content.FileProvider
import java.io.File

/**
 * A build of this app that came over the mesh, offered to the system installer.
 *
 * The node fetched and verified the APK (signed statement, size, sha256) and
 * put it under `files/data/updates/versions/<version>/`. Android replaces an
 * app only through its own installer, which asks the person, so the last step
 * is a notification: its tap opens the installer on that APK. The first time,
 * Android also asks to let Cursio install apps, and sends the person to the
 * setting; after that each update is one tap and one confirmation.
 */
object UpdateOffer {
    lateinit var context: Context

    private const val CHANNEL_ID = "cursio_updates"
    private const val NOTIFICATION_ID = 0x0c0de

    fun offer(path: String, version: String) {
        val ctx = context
        val apk = File(path)
        if (!apk.isFile) return
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.updates", apk)
        val install = Intent(Intent.ACTION_VIEW)
            .setDataAndType(uri, "application/vnd.android.package-archive")
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        val manager = ctx.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Cursio updates", NotificationManager.IMPORTANCE_DEFAULT),
        )
        val notification = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setContentTitle("Cursio update ready")
            .setContentText("Tap to install $version")
            .setAutoCancel(true)
            .setContentIntent(
                PendingIntent.getActivity(
                    ctx,
                    NOTIFICATION_ID,
                    install,
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                ),
            )
            .build()
        // One notification however many builds arrive: the newest replaces it.
        manager.notify(NOTIFICATION_ID, notification)
    }
}

/**
 * Serves the downloaded APK to the installer, and nothing else: the app's own
 * provider (Tauri's) covers external storage and the cache, and a second
 * declaration of `FileProvider` itself would collide with it in the merged
 * manifest.
 */
class UpdateFileProvider : FileProvider()
