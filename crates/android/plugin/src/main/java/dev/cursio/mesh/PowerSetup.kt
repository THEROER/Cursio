package dev.cursio.mesh

import android.annotation.SuppressLint
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import android.util.Log

/**
 * The grants a phone needs before a node on it can stay up.
 *
 * Stock Android keeps a foreground service alive. HyperOS (and MIUI before
 * it) does not take that as the final word: it kills background apps the user
 * has not exempted, ignores `BOOT_COMPLETED` for apps without its own
 * "Autostart" grant, and does both silently. A node that works until the
 * screen goes off, or never comes back after a reboot, is the result. Neither
 * grant can be requested through an API — only by sending the user to the
 * right settings screen — so this object knows where those screens are.
 */
object PowerSetup {
    private const val TAG = "Cursio"

    fun isIgnoringBatteryOptimizations(context: Context): Boolean {
        val pm = context.getSystemService(Context.POWER_SERVICE) as PowerManager
        return pm.isIgnoringBatteryOptimizations(context.packageName)
    }

    /**
     * Ask for the battery-optimisation exemption in the system's own dialog.
     *
     * Play policy restricts this intent to apps that need it; this one holds
     * a LAN link that must outlive the screen, which is the case it exists for.
     */
    @SuppressLint("BatteryLife")
    fun requestIgnoreBatteryOptimizations(activity: Activity) {
        val direct = Intent(
            Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
            Uri.parse("package:${activity.packageName}"),
        )
        if (!launch(activity, direct)) {
            launch(activity, Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
        }
    }

    /** Xiaomi, Redmi and POCO all ship the same skin and the same Autostart gate. */
    fun hasXiaomiAutostart(): Boolean {
        val maker = Build.MANUFACTURER.lowercase()
        return maker == "xiaomi" || maker == "redmi" || maker == "poco"
    }

    /**
     * Open the Autostart list, or the app's own settings page where the
     * Security app is absent or has moved the screen. The component is
     * private to the Security app and has changed name across releases, so
     * failing to launch it is expected, not an error.
     */
    fun openAutostartSettings(activity: Activity) {
        val autostart = Intent().setComponent(
            ComponentName(
                "com.miui.securitycenter",
                "com.miui.permcenter.autostart.AutoStartManagementActivity",
            ),
        )
        if (!launch(activity, autostart)) openAppDetails(activity)
    }

    fun openAppDetails(activity: Activity) {
        launch(
            activity,
            Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${activity.packageName}")),
        )
    }

    private fun launch(activity: Activity, intent: Intent): Boolean = try {
        activity.startActivity(intent)
        true
    } catch (e: ActivityNotFoundException) {
        Log.i(TAG, "no activity for $intent", e)
        false
    } catch (e: SecurityException) {
        // The Security app exports the screen on some builds and not others.
        Log.i(TAG, "not allowed to open $intent", e)
        false
    }
}
