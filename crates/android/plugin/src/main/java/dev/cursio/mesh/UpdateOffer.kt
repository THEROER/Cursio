package dev.cursio.mesh

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import androidx.core.app.NotificationCompat
import androidx.core.content.FileProvider
import java.io.File

/**
 * A build of this app that came over the mesh, offered to the system installer.
 *
 * The node fetched and verified the APK (signed statement, size, sha256) and
 * put it under `files/data/updates/versions/<version>/`. Android replaces an
 * app only through its own installer, which asks the person, so the last step
 * is theirs: a notification whose tap opens the installer on that APK, and the
 * same offer in the app's window ([MeshPlugin.installBuild]). The notification
 * alone was not enough — swiped away, or the installer backed out of by a
 * mis-tap, it left nothing to press. The first time, Android also asks to let
 * Cursio install apps; after that each update is one tap and one confirmation.
 */
object UpdateOffer {
    lateinit var context: Context

    private const val CHANNEL_ID = "cursio_updates"
    private const val NOTIFICATION_ID = 0x0c0de

    /**
     * APKs already announced by this process. The window posts each waiting
     * build's notification again whenever it comes back, and that must not
     * ring every time the person switches to the app.
     */
    private val announced = mutableSetOf<String>()

    fun offer(path: String, version: String) {
        notify(File(path), "Cursio update ready", "Tap to install $version", NOTIFICATION_ID)
    }

    /**
     * Another application's build (`docs/app-sdk-design.md`, step 4), under
     * `files/data/apps/<app>/versions/<version>/`. Answers why it cannot be
     * offered, or `""`.
     */
    fun offerApp(path: String, app: String, version: String): String {
        val apk = File(path)
        refusal(apk)?.let { return it }
        // One notification per application: the newest of each replaces it.
        notify(apk, "$app update ready", "Tap to install $version", NOTIFICATION_ID + 1 + (app.hashCode() and 0xffff))
        return ""
    }

    /**
     * Why [apk] cannot be handed to the installer, or null. An APK signed by
     * another key than the installed package is refused by the installer only
     * after the person taps, with an error that names nothing, so that is
     * checked here first.
     */
    fun refusal(apk: File): String? {
        if (!apk.isFile) return "the build's APK is missing"
        return signerMismatch(apk)
    }

    /** Whether the package [apk] holds is installed at its version or later. */
    fun installed(apk: File): Boolean {
        val pm = context.packageManager
        val archive = pm.getPackageArchiveInfo(apk.path, 0) ?: return false
        val present = try {
            pm.getPackageInfo(archive.packageName, 0)
        } catch (_: PackageManager.NameNotFoundException) {
            return false
        }
        return present.longVersionCode >= archive.longVersionCode
    }

    /** The installer's screen for [apk], from any context. */
    fun installIntent(ctx: Context, apk: File): Intent {
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.updates", apk)
        return Intent(Intent.ACTION_VIEW)
            .setDataAndType(uri, "application/vnd.android.package-archive")
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
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
        val manager = ctx.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Cursio updates", NotificationManager.IMPORTANCE_DEFAULT),
        )
        val again = synchronized(announced) { !announced.add(apk.path) }
        val notification = NotificationCompat.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setContentTitle(title)
            .setContentText(text)
            .setAutoCancel(true)
            .setOnlyAlertOnce(true)
            .setSilent(again)
            .setContentIntent(
                PendingIntent.getActivity(
                    ctx,
                    id,
                    installIntent(ctx, apk),
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
