package dev.cursio.mesh

import android.Manifest
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.activity.result.ActivityResult
import androidx.core.app.NotificationManagerCompat
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
        Permission(
            strings = [
                Manifest.permission.READ_PHONE_STATE,
                Manifest.permission.READ_CALL_LOG,
                Manifest.permission.ANSWER_PHONE_CALLS,
                Manifest.permission.CALL_PHONE,
                Manifest.permission.READ_CONTACTS,
            ],
            alias = MeshPlugin.CALLS,
        ),
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

    /** Restart a running node so its links greet peers with what changed, a new name most often. */
    @Command
    fun restartService(invoke: Invoke) {
        Node.restart(activity) { error ->
            if (error == null) invoke.resolve() else invoke.reject(error)
        }
    }

    /**
     * Ask the user to let Cursio capture what the phone plays, and resolve
     * once audio is flowing. Asked every time: a consent starts exactly one
     * projection.
     */
    @Command
    fun startPlaybackCapture(invoke: Invoke) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            invoke.reject("sharing this phone's playback needs Android 10 or later")
            return
        }
        if (!MeshService.micGranted(activity)) {
            invoke.reject("allow Cursio the microphone first: Android captures playback only under that permission")
            return
        }
        if (!Prefs.wantedRunning(activity)) {
            invoke.reject("start Cursio before sharing this phone's playback")
            return
        }
        val manager = activity.getSystemService(MediaProjectionManager::class.java)
        // Without a config Android 14+ also offers sharing a single app; what
        // a peer should hear is everything the phone plays, so the whole
        // display is the only choice offered.
        val consent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            manager.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            manager.createScreenCaptureIntent()
        }
        try {
            startActivityForResult(invoke, consent, "projectionAnswered")
        } catch (e: ActivityNotFoundException) {
            Log.w(TAG, "no screen-capture consent dialog", e)
            invoke.reject("this phone has no screen-capture consent dialog")
        }
    }

    @ActivityCallback
    private fun projectionAnswered(invoke: Invoke, result: ActivityResult) {
        val data = result.data
        if (result.resultCode != Activity.RESULT_OK || data == null) {
            invoke.reject("sharing this phone's playback was declined")
            return
        }
        try {
            MeshService.startPlaybackCapture(activity, result.resultCode, data) { error ->
                if (error == null) invoke.resolve() else invoke.reject(error)
            }
        } catch (e: IllegalStateException) {
            Log.w(TAG, "starting playback capture was refused", e)
            invoke.reject("Android refused to start capturing playback")
        }
    }

    @Command
    fun stopPlaybackCapture(invoke: Invoke) {
        MeshService.stopPlaybackCapture { invoke.resolve() }
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
            NOTIFICATION_ACCESS -> launchFirst(invoke, notificationAccessIntents())
            BLUETOOTH -> launchFirst(invoke, listOf(Intent(Settings.ACTION_BLUETOOTH_SETTINGS)))
            CALLS -> if (CallMonitor.granted(activity)) {
                invoke.resolve(setupState())
            } else {
                requestPermissionForAlias(CALLS, invoke, "permissionAnswered")
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
        CallMonitor.refresh()
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
        CursioNotificationListener.rebindIfLost(activity)
        state.put(
            "notificationAccess",
            NotificationManagerCompat.getEnabledListenerPackages(activity).contains(activity.packageName),
        )
        state.put("calls", CallMonitor.granted(activity))
        return state
    }

    /**
     * Cursio's own switch where Android 11 has one, the list of every app's
     * otherwise. HyperOS also refuses to bind the listener until Autostart is
     * allowed, which is the row above this one.
     */
    private fun notificationAccessIntents(): List<Intent> {
        val listener = ComponentName(activity, CursioNotificationListener::class.java)
        val detail = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            listOf(
                Intent(Settings.ACTION_NOTIFICATION_LISTENER_DETAIL_SETTINGS)
                    .putExtra(Settings.EXTRA_NOTIFICATION_LISTENER_COMPONENT_NAME, listener.flattenToString()),
            )
        } else {
            emptyList()
        }
        return detail + Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
    }

    companion object {
        private const val TAG = "Cursio"
        const val NOTIFICATIONS = "notifications"
        const val MICROPHONE = "microphone"
        const val BATTERY = "battery"
        const val AUTOSTART = "autostart"
        const val NOTIFICATION_ACCESS = "notificationAccess"
        const val CALLS = "calls"
        const val BLUETOOTH = "bluetooth"
    }
}
