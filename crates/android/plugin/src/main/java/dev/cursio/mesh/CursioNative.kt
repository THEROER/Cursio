package dev.cursio.mesh

/**
 * The mesh core, as the JVM sees it.
 *
 * Every method here is implemented in `crates/android/src/lib.rs`; this file is
 * only the declaration the JVM links against. The names are load-bearing — JNI
 * resolves `Java_dev_cursio_mesh_CursioNative_nativeStart` by the package,
 * class and method spelled here, so renaming any of the three without changing
 * the Rust export breaks the link at *call* time rather than at build time.
 *
 * The node itself lives in Rust: its own thread, its own Tokio runtime, the
 * same `cursio-core` every desktop peer runs. Kotlin only starts it, stops it,
 * and asks for a status line; the window reaches the node through Tauri
 * commands, not through here.
 */
object CursioNative {
    init {
        // The Tauri app's own library, not a separate one: `cursio-android` is
        // linked into it so the window and the node share one process image
        // and one copy of the mesh core. The service may run with no Activity
        // at all (after boot), so it loads the library itself; a second
        // `loadLibrary` from the Activity is a no-op.
        System.loadLibrary("cursio_app_lib")
    }

    /**
     * Point Cursio at this app's private directory and start logging.
     *
     * Must be called before anything else: identity, the trust store and the
     * log directory are all resolved against it, and on Android none of them
     * can be derived — the writable path is chosen by the system per install
     * and reaches the app only through `Context.getFilesDir()`.
     *
     * Safe to call again with the same directory; a different one is refused,
     * because state split across two directories is a node that forgets who it
     * paired with.
     */
    external fun nativeInit(filesDir: String): Boolean

    /**
     * Start listening and announcing on the LAN.
     *
     * Returns false when a node is already running — the ordinary case when
     * Android restarts a service that was never really gone.
     *
     * @param displayName what peers show for this phone. Android's hostname is
     *   a meaningless `localhost` on most devices, so a name is passed in.
     * @param port 0 to let the OS choose.
     */
    external fun nativeStart(displayName: String, port: Int): Boolean

    /**
     * Stop the node, blocking until its listener is closed.
     *
     * Blocking is deliberate: Android restarts services eagerly, and returning
     * early would let the next instance fail to bind a port this one still
     * holds.
     */
    external fun nativeStop(): Boolean

    external fun nativeIsRunning(): Boolean

    /** One line for the service notification: what this node can see. */
    external fun nativeStatusLine(): String
}
