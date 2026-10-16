package dev.cursio.mesh

import android.content.Context
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
    private var runningName: String? = null

    /** Load the library and point it at this install's private directory. */
    @Synchronized
    fun init(context: Context): Boolean {
        if (initialized) return true
        if (loadError != null) return false
        return try {
            initialized = CursioNative.nativeInit(context.filesDir.absolutePath)
            if (!initialized) loadError = "the native node refused its data directory"
            initialized
        } catch (e: LinkageError) {
            loadError = "native library unusable: ${e.message ?: e.javaClass.simpleName}"
            Log.e(TAG, "loading libcursio_app_lib failed", e)
            false
        }
    }

    fun isRunning(): Boolean = initialized && CursioNative.nativeIsRunning()

    fun statusLine(): String = if (isRunning()) CursioNative.nativeStatusLine() else loadError ?: "starting"

    /**
     * Start the node as [name], or restart it if it is running under another
     * name. Peers learn the name in `Hello`, so a rename only reaches them
     * through a new link.
     */
    fun startOrRename(context: Context, name: String) {
        val app = context.applicationContext
        lifecycle.execute {
            if (!init(app)) return@execute
            if (CursioNative.nativeIsRunning()) {
                if (runningName == name) return@execute
                CursioNative.nativeStop()
            }
            if (CursioNative.nativeStart(name, 0)) {
                runningName = name
                Log.i(TAG, "node started as \"$name\"")
            }
        }
    }

    fun stop() {
        lifecycle.execute {
            if (!initialized) return@execute
            CursioNative.nativeStop()
            runningName = null
        }
    }
}
