package dev.cursio.mesh

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
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
        notify(File(path), "Cursio update ready", "Tap to install $version", NOTIFICATION_ID)
    }

    /**
     * Another application's build (`docs/app-sdk-design.md`, step 4), under
     * `files/data/apps/<app>/versions/<version>/`. Answers why it cannot be
     * offered, or `""`: an APK signed by another key than the installed
     * package is refused by the installer only after the person taps, with an
     * error that names nothing, so that is checked here first.
     */
    fun offerApp(path: String, app: String, version: String): String {
        val apk = File(path)
        if (!apk.isFile) return "the build's APK is missing"
        signerMismatch(apk)?.let { return it }
        // One notification per application: the newest of each replaces it.
        notify(apk, "$app update ready", "Tap to install $version", NOTIFICATION_ID + 1 + (app.hashCode() and 0xffff))
        return ""
    }

    /** Why `apk` cannot replace the installed package of its name, or null. */
    private fun signerMismatch(apk: File): String? {
        val pm = context.packageManager
        val archive = pm.getPackageArchiveInfo(apk.path, PackageManager.GET_SIGNING_CERTIFICATES)
            ?: return "the APK cannot be read"
        val installed = try {
            pm.getPackageInfo(archive.packageName, PackageManager.GET_SIGNING_CERTIFICATES)
        } catch (_: PackageManager.NameNotFoundException) {
            return null
        }
        val theirs = archive.signingInfo?.apkContentsSigners?.toSet() ?: return null
        val ours = installed.signingInfo?.apkContentsSigners?.toSet() ?: return null
        return if (theirs.intersect(ours).isEmpty()) {
            "${archive.packageName} is installed signed by another key"
        } else {
            null
        }
    }

    private fun notify(apk: File, title: String, text: String, id: Int) {
        val ctx = context
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
            .setContentTitle(title)
            .setContentText(text)
            .setAutoCancel(true)
            .setContentIntent(
                PendingIntent.getActivity(
                    ctx,
                    id,
                    install,
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                ),
            )
            .build()
        manager.notify(id, notification)
    }
}

/**
 * Serves the downloaded APK to the installer, and nothing else: the app's own
 * provider (Tauri's) covers external storage and the cache, and a second
 * declaration of `FileProvider` itself would collide with it in the merged
 * manifest.
 */
class UpdateFileProvider : FileProvider()
