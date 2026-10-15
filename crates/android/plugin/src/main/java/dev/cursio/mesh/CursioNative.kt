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
 * same `cursio-core` every desktop peer runs. Kotlin starts it, stops it, and
 * asks what it can see. Nothing here holds a socket or parses a message —
 * putting the session loop on the JVM would expose a handshake to garbage
 * collection pauses and would mean reimplementing what `mesh.rs` already does.
 */
object CursioNative {
    init {
        System.loadLibrary("cursio_android")
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
     *   a meaningless `localhost` on most devices, so the user's name for it is
     *   passed in instead.
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

    /** Whether a node is running right now. */
    external fun nativeIsRunning(): Boolean

    /** This node's stable peer id, or an empty string if it cannot be read. */
    external fun nativePeerId(): String

    /** One line for the service notification: what this node can see. */
    external fun nativeStatusLine(): String

    /**
     * Open the pairing window with a code shown on another machine
     * (e.g. "ABCD-EFGH"). Returns "" on success, else a human-readable error.
     */
    external fun nativeEnterCode(code: String): String

    /**
     * Open the pairing window with a fresh code for this phone to show.
     * Returns the code, or "" if the node is not running.
     */
    external fun nativeShowCode(): String

    external fun nativeCancelPairing(): Boolean

    /** A JSON snapshot of the node; [NodeStatus.parse] reads it. */
    external fun nativeStatusJson(): String

    /**
     * Local playback volume (0.0–1.5) and mute of one stream.
     * Returns false if the stream is unknown.
     */
    external fun nativeSetStreamVolume(id: Int, volume: Float, muted: Boolean): Boolean

    /**
     * Gate for this phone's microphone: "auto", "off" or a dBFS level such as
     * "-55". Returns "" on success, else an error.
     */
    external fun nativeSetMicGate(value: String): String
}
