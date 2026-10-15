package dev.cursio.mesh

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

/**
 * Brings the node back after a reboot or an app update, if the user left it
 * running.
 *
 * An update matters as much as a boot: installing a new APK kills the process,
 * and a phone that silently left the mesh because a build was pushed to it
 * looks like a broken link on every peer.
 *
 * The start is a background one, so the service claims only what a boot
 * receiver may (no `microphone`, and on Android 15+ no `mediaPlayback`); the
 * Activity widens it the next time the user opens the app.
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED -> Unit
            else -> return
        }
        if (!Prefs.wantedRunning(context)) return
        try {
            MeshService.start(context, fromForeground = false)
        } catch (e: IllegalStateException) {
            // `ForegroundServiceStartNotAllowedException`: these broadcasts are
            // exempt from the background-start ban, so reaching this means the
            // OEM skin overrode that — HyperOS does without the Autostart grant.
            Log.w("Cursio", "starting the mesh after ${intent.action} was refused", e)
        }
    }
}
