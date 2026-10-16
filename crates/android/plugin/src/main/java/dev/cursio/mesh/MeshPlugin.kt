package dev.cursio.mesh

import android.Manifest
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

/**
 * What the app's window asks of Android: start the service, and the grants
 * the node needs to stay up and carry audio (`app/src/mobile.rs` is the Rust
 * half).
 *
 * Permission dialogs and settings screens go through Tauri's own request and
 * activity-result plumbing rather than the Activity's callbacks, which a
 * plugin does not own: each `request` resolves only once the user is back from
 * the dialog or screen, so the answer it carries is the one they just gave.
 */
@TauriPlugin(
    permissions = [
        Permission(strings = [Manifest.permission.POST_NOTIFICATIONS], alias = MeshPlugin.NOTIFICATIONS),
        Permission(strings = [Manifest.permission.RECORD_AUDIO], alias = MeshPlugin.MICROPHONE),
    ],
)
class MeshPlugin(private val activity: Activity) : Plugin(activity) {

    /**
     * Started again on every return to the window, even when already running:
     * this is the one moment the service may claim the microphone, so a node
     * brought up at boot gains it the first time the user opens the app.
     */
    override fun onResume() {
        if (Prefs.wantedRunning(activity)) startFromForeground()
    }

    @Command
    fun startService(invoke: Invoke) {
        Prefs.setWantedRunning(activity, true)
        if (startFromForeground()) {
            invoke.resolve()
        } else {
            invoke.reject("Android refused to start the Cursio service")
        }
    }

    @Command
    fun setup(invoke: Invoke) {
        invoke.resolve(setupState())
    }

    @Command
    fun request(invoke: Invoke) {
        when (val what = invoke.getArgs().getString("what", "")) {
            NOTIFICATIONS -> {
                // Below Android 13 the notification is not a runtime permission.
                if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU || granted(Manifest.permission.POST_NOTIFICATIONS)) {
                    invoke.resolve(setupState())
                } else {
                    requestPermissionForAlias(NOTIFICATIONS, invoke, "permissionAnswered")
                }
            }
            MICROPHONE -> requestMicrophone(invoke)
            BATTERY -> launchFirst(invoke, PowerSetup.batteryOptimizationIntents(activity))
            AUTOSTART -> {
                Prefs.setOpenedAutostart(activity)
                launchFirst(invoke, PowerSetup.autostartIntents(activity))
            }
            else -> invoke.reject("unknown grant \"$what\"")
        }
    }

    /**
     * After a second refusal Android answers "denied" without showing
     * anything, so asking again would resolve at once with nothing on screen;
     * the app's settings page is the only place the user can still grant it.
     */
    private fun requestMicrophone(invoke: Invoke) {
        val permission = Manifest.permission.RECORD_AUDIO
        when {
            granted(permission) -> invoke.resolve(setupState())
            Prefs.askedMicrophone(activity) && !activity.shouldShowRequestPermissionRationale(permission) ->
                launchFirst(invoke, listOf(PowerSetup.appDetails(activity)))
            else -> {
                Prefs.setAskedMicrophone(activity)
                requestPermissionForAlias(MICROPHONE, invoke, "permissionAnswered")
            }
        }
    }

    @PermissionCallback
    private fun permissionAnswered(invoke: Invoke) {
        afterGrant()
        invoke.resolve(setupState())
    }

    @ActivityCallback
    private fun screenClosed(invoke: Invoke, @Suppress("UNUSED_PARAMETER") result: ActivityResult) {
        afterGrant()
        invoke.resolve(setupState())
    }

    /**
     * A microphone granted while the service runs is not yet one it holds:
     * the type is claimed at `startForeground`, so the service is started
     * again from here, while the window is still in front.
     */
    private fun afterGrant() {
        if (Prefs.wantedRunning(activity) && MeshService.micGranted(activity) && !MeshService.holdsMicrophone()) {
            startFromForeground()
        }
    }

    private fun launchFirst(invoke: Invoke, intents: List<Intent>) {
        for (intent in intents) {
            try {
                startActivityForResult(invoke, intent, "screenClosed")
                return
            } catch (e: ActivityNotFoundException) {
                Log.i(TAG, "no activity for $intent", e)
            } catch (e: SecurityException) {
                Log.i(TAG, "not allowed to open $intent", e)
            }
        }
        invoke.reject("this phone has no settings screen for that")
    }

    private fun startFromForeground(): Boolean = try {
        MeshService.start(activity, fromForeground = true)
        true
    } catch (e: IllegalStateException) {
        // `ForegroundServiceStartNotAllowedException`: the window was not in
        // front after all, e.g. a resume racing the screen turning off.
        Log.w(TAG, "starting the Cursio service was refused", e)
        false
    }

    private fun granted(permission: String): Boolean =
        activity.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED

    private fun setupState(): JSObject {
        val state = JSObject()
        state.put(
            "notifications",
            Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU || granted(Manifest.permission.POST_NOTIFICATIONS),
        )
        state.put("microphone", MeshService.micGranted(activity))
        state.put("battery", PowerSetup.isIgnoringBatteryOptimizations(activity))
        // No API reads the HyperOS grant, so once the user has been sent to
        // the screen it is no longer offered.
        state.put("autostartSetting", PowerSetup.hasXiaomiAutostart() && !Prefs.openedAutostart(activity))
        return state
    }

    companion object {
        private const val TAG = "Cursio"
        const val NOTIFICATIONS = "notifications"
        const val MICROPHONE = "microphone"
        const val BATTERY = "battery"
        const val AUTOSTART = "autostart"
    }
}
