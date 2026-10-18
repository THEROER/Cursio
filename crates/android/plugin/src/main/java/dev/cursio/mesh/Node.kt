package dev.cursio.mesh

import android.content.Context
import android.os.Build
import android.provider.Settings
import android.util.Log
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/**
 * The one way the service reaches the native node.
 *
 * Starting and stopping run on a single process-wide thread. `nativeStop`
 * blocks until the listener is closed, and on the main thread that is an ANR
 * waiting to happen; on a pool, a start queued behind a stop could overtake it
 * and fail to bind the port the stop is still releasing. One thread gives both:
 * the main thread never waits, and every lifecycle call lands in the order it
 * was made.
 */
object Node {
    private const val TAG = "Cursio"

    private val lifecycle: ExecutorService = Executors.newSingleThreadExecutor { r ->
        Thread(r, "cursio-node-lifecycle")
    }

    /**
     * Why the native library could not be used, or null if it can.
     *
     * Kept rather than rethrown so the notification can say what is wrong.
     * Without it, a missing or mismatched `.so` is a crash with nothing on
     * screen naming the cause.
     */
    @Volatile
    var loadError: String? = null
        private set

    @Volatile
    private var initialized = false

    @Volatile
    private var reportUnavailable = false

    /** Load the library and point it at this install's private directory. */
    @Synchronized
    fun init(context: Context): Boolean {
        if (initialized) return true
        if (loadError != null) return false
        return try {
            ShareReceiver.context = context.applicationContext
            Ringer.context = context.applicationContext
            SmsStore.context = context.applicationContext
            BtLink.context = context.applicationContext
            MediaNotifier.context = context.applicationContext
            initialized = CursioNative.nativeInit(context.filesDir.absolutePath)
            if (!initialized) loadError = "the native node refused its data directory"
            if (initialized) {
                SmsStore.watch(context)
                MediaBridge.watch(context)
            }
            initialized
        } catch (e: LinkageError) {
            loadError = "native library unusable: ${e.message ?: e.javaClass.simpleName}"
            Log.e(TAG, "loading libcursio_app_lib failed", e)
            false
        }
    }

    fun isRunning(): Boolean = initialized && CursioNative.nativeIsRunning()

    fun statusLine(): String = if (isRunning()) CursioNative.nativeStatusLine() else loadError ?: "starting"

    /** Start the node unless it is already running. */
    fun start(context: Context) {
        val app = context.applicationContext
        lifecycle.execute {
            if (!init(app) || CursioNative.nativeIsRunning()) return@execute
            startNow(app)
        }
    }

    /**
     * Stop a running node and start it again, then call [done] with null or
     * the reason it failed. Peers learn the name in `Hello`, so a rename only
     * reaches them through new links. A node that is not running stays down:
     * starting one here would put it outside the foreground service.
     */
    fun restart(context: Context, done: (String?) -> Unit) {
        val app = context.applicationContext
        lifecycle.execute {
            val error = when {
                !init(app) -> loadError ?: "the native node is not available"
                !CursioNative.nativeIsRunning() -> null
                else -> {
                    CursioNative.nativeStop()
                    if (startNow(app)) null else "the node did not start again"
                }
            }
            done(error)
        }
    }

    fun stop() {
        lifecycle.execute {
            if (!initialized) return@execute
            CursioNative.nativeStop()
        }
    }

    /**
     * Pass a reading on to the node. Ignored until [init] has succeeded:
     * before that the library may not load at all, and [init] is where that
     * failure is caught and named.
     */
    /** The screen is on and unlocked ([inUse]), or no longer; see [DeviceStatus]. */
    fun reportInUse(inUse: Boolean) {
        if (!initialized) return
        try {
            CursioNative.nativePhoneInUse(inUse)
        } catch (e: UnsatisfiedLinkError) {
            Log.w(TAG, "this native library cannot take whether the phone is in use", e)
        }
    }

    fun reportStatus(battery: Int, power: String, network: String, signalDbm: Int) {
        if (!initialized || reportUnavailable) return
        try {
            CursioNative.nativeReportStatus(battery, power, network, signalDbm)
        } catch (e: UnsatisfiedLinkError) {
            // A library older than this Kotlin: the node runs without a
            // reading, which is what it shows peers anyway, so say it once.
            reportUnavailable = true
            Log.w(TAG, "this native library cannot take a device status", e)
        } catch (e: RuntimeException) {
            Log.w(TAG, "the node refused a device status", e)
        }
    }

    private fun startNow(app: Context): Boolean {
        val name = deviceName(app)
        return try {
            CursioNative.nativeStart(name, 0).also { if (it) Log.i(TAG, "node started (default name \"$name\")") }
        } catch (e: RuntimeException) {
            Log.e(TAG, "starting the node failed", e)
            false
        }
    }

    /**
     * The name the user gave this phone in its own settings, which is what
     * they will look for on a peer; the model is all that is left without one.
     */
    private fun deviceName(context: Context): String =
        Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)?.takeIf { it.isNotBlank() }
            ?: Build.MODEL?.takeIf { it.isNotBlank() }
            ?: "Android"
}
