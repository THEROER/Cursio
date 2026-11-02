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
        val name = label(apk) ?: app
        notify(apk, "$name update ready", "Tap to install $version", NOTIFICATION_ID + 1 + (app.hashCode() and 0xffff))
        return ""
    }

    /**
     * The application name [apk] carries, or null. An archive's label is
     * read only once its source paths point at the archive itself.
     */
    fun label(apk: File): String? {
        val pm = context.packageManager
        val info = archived(apk) ?: return null
        return pm.getApplicationLabel(info).toString().takeIf { it.isNotBlank() }
    }

    /**
     * The icon [apk] carries, as base64 PNG of [ICON_PX] square, or null.
     * Kept per file: the window asks with every status, and one build's APK
     * never changes under its path.
     */
    fun icon(apk: File): String? {
        synchronized(icons) { icons[apk.path] }?.let { return it }
        val info = archived(apk) ?: return null
        val drawable = context.packageManager.getApplicationIcon(info)
        val bitmap = android.graphics.Bitmap.createBitmap(ICON_PX, ICON_PX, android.graphics.Bitmap.Config.ARGB_8888)
        val canvas = android.graphics.Canvas(bitmap)
        drawable.setBounds(0, 0, ICON_PX, ICON_PX)
        drawable.draw(canvas)
        val png = java.io.ByteArrayOutputStream()
        bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG, 100, png)
        bitmap.recycle()
        val encoded = android.util.Base64.encodeToString(png.toByteArray(), android.util.Base64.NO_WRAP)
        synchronized(icons) { icons[apk.path] = encoded }
        return encoded
    }

    /**
     * A build coming in over the mesh, as an ongoing silent notification
     * with a bar: sent from a computer, it was otherwise invisible here until
     * the installer opened.
     */
    fun progress(key: String, title: String, text: String, percent: Int) {
        val ctx = context
        val manager = ctx.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(PROGRESS_CHANNEL_ID, "Builds coming in", NotificationManager.IMPORTANCE_LOW),
        )
        val notification = NotificationCompat.Builder(ctx, PROGRESS_CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_download)
            .setContentTitle(title)
            .setContentText("$text · $percent%")
            .setProgress(100, percent, false)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .build()
        manager.notify(progressId(key), notification)
    }

    fun progressDone(key: String) {
        context.getSystemService(NotificationManager::class.java).cancel(progressId(key))
    }

    private fun progressId(key: String) = PROGRESS_ID + (key.hashCode() and 0xffff)

    private const val PROGRESS_CHANNEL_ID = "cursio_build_progress"
    private const val PROGRESS_ID = 0x0c1000

    private val icons = HashMap<String, String>()
    private const val ICON_PX = 96

    /**
     * [apk]'s application info, readable for its label and icon: an
     * archive's resources load only once its source paths name the archive.
     */
    private fun archived(apk: File): android.content.pm.ApplicationInfo? {
        val info = context.packageManager.getPackageArchiveInfo(apk.path, 0)?.applicationInfo ?: return null
        info.sourceDir = apk.path
        info.publicSourceDir = apk.path
        return info
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
