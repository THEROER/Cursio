package dev.cursio.mesh

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.PackageManager
import android.util.Log

/**
 * Opening another app's launcher screen for a computer of the mesh.
 *
 * Android 10 and later refuse to start an activity from an app in the
 * background, with a few exemptions; one is an app whose accessibility
 * service the system has bound — Cursio's [ScreenTouch], when the person
 * turned it on. Then the screen is started directly. Otherwise the person is
 * asked with a notification whose tap opens it, and the computer is told so.
 *
 * MIUI/HyperOS add their own gate, "Display pop-up windows while running in
 * the background", which drops such a start without an error. Nothing here
 * changes that setting; the computer's answer names it so a start that did
 * not appear can be explained.
 */
object AppOpener {
    private const val TAG = "Cursio"
    private const val CHANNEL_ID = "cursio_open"
    private const val NOTIFICATION_ID = 0x0be4

    fun open(pkg: String): String {
        val ctx = UpdateOffer.context
        val pm = ctx.packageManager
        val info = try {
            pm.getPackageInfo(pkg, 0)
        } catch (_: PackageManager.NameNotFoundException) {
            return "error\t$pkg is not installed on this phone"
        }
        val versionName = info.versionName ?: ""
        val launch = pm.getLaunchIntentForPackage(pkg)
            ?: return "error\t$pkg has no screen to open"
        launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED)
        val label = info.applicationInfo?.let { pm.getApplicationLabel(it).toString() } ?: pkg
        if (ScreenTouch.connected()) {
            try {
                ctx.startActivity(launch)
                return "direct\t$versionName"
            } catch (e: Exception) {
                Log.w(TAG, "could not open $pkg directly; asking with a notification", e)
            }
        }
        ask(launch, label, pkg)
        return "asked\t$versionName"
    }

    /** A notification whose tap opens [launch]: what a person can always do. */
    private fun ask(launch: Intent, label: String, pkg: String) {
        val ctx = UpdateOffer.context
        val manager = ctx.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, "Open apps for a computer", NotificationManager.IMPORTANCE_HIGH),
        )
        val id = NOTIFICATION_ID + (pkg.hashCode() and 0xffff)
        val notification = Notification.Builder(ctx, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.ic_menu_view)
            .setContentTitle("Open $label")
            .setContentText("A computer of your mesh asks to open $label")
            .setAutoCancel(true)
            .setContentIntent(
                PendingIntent.getActivity(
                    ctx,
                    id,
                    launch,
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                ),
            )
            .build()
        manager.notify(id, notification)
    }
}
