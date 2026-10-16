package dev.cursio.mesh

import android.content.Context
import android.content.SharedPreferences

/** What the user chose, kept across restarts of the app and the phone. */
object Prefs {
    private const val FILE = "cursio"
    private const val KEY_WANTED = "wanted_running"
    private const val KEY_ASKED_MICROPHONE = "asked_microphone"
    private const val KEY_OPENED_AUTOSTART = "opened_autostart"

    private fun prefs(context: Context): SharedPreferences =
        context.applicationContext.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    /**
     * Whether the user last left the node running. This, not whether the
     * process happens to be alive, decides what happens at boot: a node the
     * user stopped stays stopped across a reboot.
     */
    fun wantedRunning(context: Context): Boolean = prefs(context).getBoolean(KEY_WANTED, false)

    fun setWantedRunning(context: Context, wanted: Boolean) =
        prefs(context).edit().putBoolean(KEY_WANTED, wanted).apply()

    /**
     * Whether the microphone has been asked for before. After a second refusal
     * Android stops showing the dialog and answers "denied" at once, and
     * `shouldShowRequestPermissionRationale` is false both then and before the
     * first ask — this is what tells the two apart.
     */
    fun askedMicrophone(context: Context): Boolean = prefs(context).getBoolean(KEY_ASKED_MICROPHONE, false)

    fun setAskedMicrophone(context: Context) =
        prefs(context).edit().putBoolean(KEY_ASKED_MICROPHONE, true).apply()

    /**
     * Whether the user has been to the HyperOS Autostart screen. Nothing tells
     * an app whether it holds that grant, so having opened it once is the only
     * signal there is for no longer asking.
     */
    fun openedAutostart(context: Context): Boolean = prefs(context).getBoolean(KEY_OPENED_AUTOSTART, false)

    fun setOpenedAutostart(context: Context) =
        prefs(context).edit().putBoolean(KEY_OPENED_AUTOSTART, true).apply()
}
