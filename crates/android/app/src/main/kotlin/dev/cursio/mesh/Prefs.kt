package dev.cursio.mesh

import android.content.Context
import android.content.SharedPreferences

/** What the user chose, kept across restarts of the app and the phone. */
object Prefs {
    private const val FILE = "cursio"
    private const val KEY_NAME = "display_name"
    private const val KEY_WANTED = "wanted_running"
    private const val KEY_MIC_GATE = "mic_gate"
    private const val KEY_ASKED_PERMISSIONS = "asked_permissions"
    private const val KEY_OPENED_AUTOSTART = "opened_autostart"

    const val GATE_AUTO = "auto"
    const val GATE_OFF = "off"
    const val GATE_DEFAULT_LEVEL = -55

    private fun prefs(context: Context): SharedPreferences =
        context.applicationContext.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    fun displayName(context: Context): String =
        prefs(context).getString(KEY_NAME, null)?.takeIf { it.isNotBlank() } ?: Node.defaultName()

    fun setDisplayName(context: Context, name: String) =
        prefs(context).edit().putString(KEY_NAME, name).apply()

    /**
     * Whether the user last left the node running. This, not whether the
     * process happens to be alive, decides what happens at boot and launch:
     * a node the user stopped stays stopped across a reboot.
     */
    fun wantedRunning(context: Context): Boolean = prefs(context).getBoolean(KEY_WANTED, false)

    fun setWantedRunning(context: Context, wanted: Boolean) =
        prefs(context).edit().putBoolean(KEY_WANTED, wanted).apply()

    /** "auto", "off" or a dBFS level such as "-55", exactly as the node takes it. */
    fun micGate(context: Context): String = prefs(context).getString(KEY_MIC_GATE, GATE_AUTO) ?: GATE_AUTO

    fun setMicGate(context: Context, value: String) =
        prefs(context).edit().putString(KEY_MIC_GATE, value).apply()

    /**
     * Whether the user has been to the HyperOS Autostart screen. Nothing tells
     * an app whether it holds that grant, so having opened it once is the only
     * signal there is for no longer asking.
     */
    fun openedAutostart(context: Context): Boolean = prefs(context).getBoolean(KEY_OPENED_AUTOSTART, false)

    fun setOpenedAutostart(context: Context) =
        prefs(context).edit().putBoolean(KEY_OPENED_AUTOSTART, true).apply()

    fun askedPermissions(context: Context): Boolean = prefs(context).getBoolean(KEY_ASKED_PERMISSIONS, false)

    fun setAskedPermissions(context: Context) =
        prefs(context).edit().putBoolean(KEY_ASKED_PERMISSIONS, true).apply()
}
