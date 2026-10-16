package dev.cursio.mesh

import android.annotation.SuppressLint
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings

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
 *
 * Each screen comes as a list of intents, best first: the screens are private
 * to the system or a vendor app and move between releases, so failing to
 * launch one is expected, not an error, and the next is tried.
 */
object PowerSetup {
    fun isIgnoringBatteryOptimizations(context: Context): Boolean {
        val pm = context.getSystemService(Context.POWER_SERVICE) as PowerManager
        return pm.isIgnoringBatteryOptimizations(context.packageName)
    }

    /**
     * The system's own one-tap dialog, then the list it lives in.
     *
     * Play policy restricts the dialog to apps that need it; this one holds a
     * LAN link that must outlive the screen, which is the case it exists for.
     */
    @SuppressLint("BatteryLife")
    fun batteryOptimizationIntents(context: Context): List<Intent> = listOf(
        Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${context.packageName}")),
        Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS),
    )

    /** Xiaomi, Redmi and POCO all ship the same skin and the same Autostart gate. */
    fun hasXiaomiAutostart(): Boolean {
        val maker = Build.MANUFACTURER.lowercase()
        return maker == "xiaomi" || maker == "redmi" || maker == "poco"
    }

    /**
     * The Autostart list, or the app's own settings page where the Security
     * app is absent or does not export the screen on this build.
     */
    fun autostartIntents(context: Context): List<Intent> = listOf(
        Intent().setComponent(
            ComponentName(
                "com.miui.securitycenter",
                "com.miui.permcenter.autostart.AutoStartManagementActivity",
            ),
        ),
        appDetails(context),
    )

    fun appDetails(context: Context): Intent =
        Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${context.packageName}"))
}
