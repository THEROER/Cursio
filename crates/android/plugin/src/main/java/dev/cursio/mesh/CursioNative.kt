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
     * @param displayName the name a node with none of its own starts under.
     *   Android's hostname is a meaningless `localhost` on most devices, so
     *   one is passed in; once the identity file holds a name the user chose,
     *   that name wins and this one is ignored.
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

    /**
     * Hand the node this phone's power and network reading.
     *
     * Every argument has an "cannot say" value, and it must be used rather
     * than zero: a 0% battery or a 0 dBm signal is a reading, and peers would
     * treat the phone as about to vanish.
     *
     * @param battery percent, or -1 when unknown.
     * @param power `"battery"`, `"charging"`, `"mains"`, or `""` when unknown.
     * @param network `"wifi"`, `"cellular"`, `"wired"`, `"unknown"` (connected
     *   over something else), or `""` when there is no connection.
     * @param signalDbm Wi-Fi RSSI, or [Int.MIN_VALUE] when unknown.
     */
    external fun nativeReportStatus(battery: Int, power: String, network: String, signalDbm: Int): Boolean

    /**
     * Hand the node the first [len] samples of [samples]: what this phone is
     * playing, interleaved stereo f32 at 48 kHz, for the peer it is routed to.
     */
    external fun nativePushPlayback(samples: FloatArray, len: Int)

    /** The machines a share can go to, as `[[id, name], …]` JSON. */
    external fun nativeShareTargets(): String

    /**
     * Send to [peer]: [text] when [path] is empty, otherwise the file at [path]
     * (a private copy) under [name]. [action] is `open`, `copy` or `save`.
     * Answers `""` once it is on its way, otherwise why it is not.
     */
    external fun nativeShare(peer: String, text: String, path: String, name: String, mime: String, action: String): String

    /**
     * A machine shared something to this phone. Called by the node, on its
     * own thread; answers what was done, see [ShareReceiver].
     */
    @JvmStatic
    fun onShare(from: String, kind: String, text: String, path: String, name: String, mime: String, action: String): String =
        ShareReceiver.act(from, kind, text, path, name, mime, action)

    /**
     * A notification was posted on this phone; see [CursioNotificationListener].
     * [kind] is `call`, `message` or `other`; [actionReplies] says which of
     * [actionTitles] take text.
     */
    external fun nativeNotificationPosted(
        key: String,
        appId: String,
        appName: String,
        title: String,
        text: String,
        icon: ByteArray?,
        whenMs: Long,
        kind: String,
        actionTitles: Array<String>,
        actionReplies: BooleanArray,
    )

    /** A notification on this phone went away. */
    external fun nativeNotificationRemoved(key: String)

    /**
     * A desktop's user pressed button [action] of notification [key], typing
     * [reply] into it if it takes text. Called by the node, on its own thread.
     */
    @JvmStatic
    fun onNotificationInvoke(key: String, action: Int, reply: String?) =
        CursioNotificationListener.invoke(key, action, reply)

    /** The phone's messages changed: a desktop showing them should look again. */
    external fun nativeSmsChanged()

    /**
     * A desktop asked for this phone's messages. Called by the node, on its
     * own thread, with one JSON request; answers one JSON object, see [SmsStore].
     */
    @JvmStatic
    fun onSmsRequest(request: String): String = SmsStore.handle(request)

    /** A desktop's user closed notification [key]: dismiss it here too. */
    @JvmStatic
    fun onNotificationDismiss(key: String) = CursioNotificationListener.dismiss(key)

    /** A desktop wants this phone found: ring until it is, or stop. */
    @JvmStatic
    fun onRing(on: Boolean) = Ringer.ring(on)

    /**
     * This phone's calls changed, or what it may do with them; see
     * [CallMonitor]. [calls] is a JSON array of
     * `{id, incoming, state, number, name}`.
     */
    external fun nativeCallsChanged(available: Boolean, reason: String?, bluetoothName: String?, calls: String)

    /**
     * A computer asked for [kind] (`answer`, `decline`, `hangup`, `dial`,
     * `mute`). Called by the node, on its own thread; returns why it could
     * not be done, or null.
     */
    @JvmStatic
    fun onCallRequest(kind: String, number: String?, muted: Boolean): String? =
        CallMonitor.act(kind, number, muted)
}
