package dev.cursio.mesh

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Gravity
import android.view.View
import android.view.inputmethod.EditorInfo
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.RadioGroup
import android.widget.SeekBar
import android.widget.TextView
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import org.json.JSONException
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The one screen: this phone, its machines, pairing and audio.
 *
 * Everything shown comes from one `nativeStatusJson` snapshot polled each
 * second while the screen is visible; nothing here keeps its own idea of what
 * the node is doing. Native calls run on [worker], never the main thread, so a
 * node that is busy starting or stopping cannot freeze the UI.
 */
class MainActivity : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor { r -> Thread(r, "cursio-ui-native") }
    private val pollInFlight = AtomicBoolean(false)
    private val streamRows = LinkedHashMap<Int, StreamRow>()
    private var visible = false

    private lateinit var name: EditText
    private lateinit var nodeDot: View
    private lateinit var nodeStatus: TextView
    private lateinit var runButton: Button
    private lateinit var loadError: TextView
    private lateinit var peersList: LinearLayout
    private lateinit var peersEmpty: TextView
    private lateinit var pairCode: EditText
    private lateinit var pairError: TextView
    private lateinit var showCodeButton: Button
    private lateinit var pairingActive: View
    private lateinit var shownCode: TextView
    private lateinit var pairingState: TextView
    private lateinit var lastPairing: TextView
    private lateinit var streamsList: LinearLayout
    private lateinit var streamsEmpty: TextView
    private lateinit var gateGroup: RadioGroup
    private lateinit var gateLevel: SeekBar
    private lateinit var gateValue: TextView
    private lateinit var gateError: TextView

    private val poll = object : Runnable {
        override fun run() {
            if (!visible) return
            requestStatus()
            handler.postDelayed(this, POLL_INTERVAL_MS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        applyInsets()
        bindViews()

        Node.init(this)
        Node.loadError?.let {
            loadError.text = it
            loadError.visibility = View.VISIBLE
        }

        setUpName()
        setUpPairing()
        setUpMicGate()
        setUpSetupCard()
        runButton.setOnClickListener { toggleRunning() }

        if (!Prefs.askedPermissions(this)) {
            Prefs.setAskedPermissions(this)
            requestRuntimePermissions()
        }
    }

    override fun onResume() {
        super.onResume()
        visible = true
        refreshSetupCard()
        // Started again from here even when already running: this is the one
        // place the service may claim the microphone, so a node brought up at
        // boot gains it the first time the user opens the app.
        if (Prefs.wantedRunning(this)) MeshService.start(this, fromForeground = true)
        handler.removeCallbacks(poll)
        handler.post(poll)
    }

    override fun onPause() {
        visible = false
        handler.removeCallbacks(poll)
        commitName()
        super.onPause()
    }

    override fun onDestroy() {
        worker.shutdown()
        super.onDestroy()
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        refreshSetupCard()
        if (Prefs.wantedRunning(this) && MeshService.micGranted(this) && !MeshService.holdsMicrophone()) {
            MeshService.start(this, fromForeground = true)
        }
    }

    /**
     * Since Android 15 an app targeting 35+ draws under the system bars
     * whether it asked to or not, so the bars' space is added as padding.
     */
    private fun applyInsets() {
        val content = findViewById<View>(R.id.content)
        val base = content.paddingBottom
        ViewCompat.setOnApplyWindowInsetsListener(content) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.ime())
            v.setPadding(v.paddingLeft, bars.top, v.paddingRight, base + bars.bottom)
            insets
        }
    }

    private fun bindViews() {
        name = findViewById(R.id.name)
        nodeDot = findViewById(R.id.node_dot)
        nodeStatus = findViewById(R.id.node_status)
        runButton = findViewById(R.id.run_button)
        loadError = findViewById(R.id.load_error)
        peersList = findViewById(R.id.peers_list)
        peersEmpty = findViewById(R.id.peers_empty)
        pairCode = findViewById(R.id.pair_code)
        pairError = findViewById(R.id.pair_error)
        showCodeButton = findViewById(R.id.show_code_button)
        pairingActive = findViewById(R.id.pairing_active)
        shownCode = findViewById(R.id.shown_code)
        pairingState = findViewById(R.id.pairing_state)
        lastPairing = findViewById(R.id.last_pairing)
        streamsList = findViewById(R.id.streams_list)
        streamsEmpty = findViewById(R.id.streams_empty)
        gateGroup = findViewById(R.id.gate_group)
        gateLevel = findViewById(R.id.gate_level)
        gateValue = findViewById(R.id.gate_value)
        gateError = findViewById(R.id.gate_error)
    }

    private fun requestRuntimePermissions() {
        val wanted = buildList {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) add(Manifest.permission.POST_NOTIFICATIONS)
            add(Manifest.permission.RECORD_AUDIO)
        }.filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (wanted.isNotEmpty()) requestPermissions(wanted.toTypedArray(), REQUEST_PERMISSIONS)
    }

    private fun toggleRunning() {
        commitName()
        if (Prefs.wantedRunning(this)) {
            Prefs.setWantedRunning(this, false)
            MeshService.stop(this)
        } else {
            Prefs.setWantedRunning(this, true)
            MeshService.start(this, fromForeground = true)
        }
        renderRunButton()
    }

    private fun setUpName() {
        name.setText(Prefs.displayName(this))
        name.setOnEditorActionListener { v, action, _ ->
            if (action == EditorInfo.IME_ACTION_DONE) {
                commitName()
                v.clearFocus()
            }
            false
        }
        name.setOnFocusChangeListener { _, focused -> if (!focused) commitName() }
    }

    /** Store a changed name and, if the node runs, restart it under that name. */
    private fun commitName() {
        val entered = name.text.toString().trim()
        if (entered.isEmpty()) {
            name.setText(Prefs.displayName(this))
            return
        }
        if (entered == Prefs.displayName(this)) return
        Prefs.setDisplayName(this, entered)
        if (Prefs.wantedRunning(this)) MeshService.start(this, fromForeground = visible)
    }

    private fun renderRunButton() {
        runButton.text = if (Prefs.wantedRunning(this)) "Stop" else "Start"
    }

    private fun requestStatus() {
        if (!pollInFlight.compareAndSet(false, true)) return
        worker.execute {
            val status = try {
                Node.statusJson()?.let { NodeStatus.parse(it) } ?: NodeStatus.STOPPED
            } catch (e: JSONException) {
                android.util.Log.w("Cursio", "unreadable status snapshot", e)
                null
            } finally {
                pollInFlight.set(false)
            }
            if (status != null) handler.post { if (!isDestroyed) render(status) }
        }
    }

    private fun render(status: NodeStatus) {
        renderRunButton()
        val wanted = Prefs.wantedRunning(this)
        nodeStatus.text = when {
            status.running -> listOfNotNull("Running", status.bound).joinToString(" · ")
            wanted && Node.loadError == null -> "Starting…"
            else -> "Stopped"
        }
        setDot(nodeDot, if (status.running) "online" else if (wanted) "connecting" else "offline")

        renderPeers(status.peers)
        renderPairing(status)
        renderStreams(status.streams)
    }

    private fun renderPeers(peers: List<NodeStatus.Peer>) {
        peersList.removeAllViews()
        peersEmpty.visibility = if (peers.isEmpty()) View.VISIBLE else View.GONE
        val dp = resources.displayMetrics.density
        for (peer in peers.sortedWith(compareBy({ presenceRank(it.presence) }, { it.name.lowercase() }))) {
            val dot = View(this)
            setDot(dot, peer.presence)
            val title = TextView(this).apply {
                setTextColor(getColor(R.color.text))
                textSize = 15f
                text = peer.name
            }
            val detail = TextView(this).apply {
                setTextColor(getColor(R.color.text_muted))
                textSize = 13f
                text = listOf(peer.presence).plus(peer.modules).joinToString(" · ")
            }
            val text = LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                addView(title)
                addView(detail)
            }
            val row = LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(0, (6 * dp).toInt(), 0, (6 * dp).toInt())
                addView(dot, LinearLayout.LayoutParams((10 * dp).toInt(), (10 * dp).toInt()).apply {
                    marginEnd = (12 * dp).toInt()
                })
                addView(text)
            }
            peersList.addView(row)
        }
    }

    private fun presenceRank(presence: String) = when (presence) {
        "online" -> 0
        "connecting" -> 1
        else -> 2
    }

    private fun setDot(view: View, presence: String) {
        val color = when (presence) {
            "online" -> R.color.online
            "connecting" -> R.color.connecting
            else -> R.color.offline
        }
        view.background = GradientDrawable().apply {
            shape = GradientDrawable.OVAL
            setColor(getColor(color))
        }
    }

    private fun renderStreams(streams: List<NodeStatus.Stream>) {
        val ids = streams.map { it.id }.toSet()
        val gone = streamRows.keys.filter { it !in ids }
        for (id in gone) streamsList.removeView(streamRows.remove(id)?.view)
        for (stream in streams) {
            val row = streamRows.getOrPut(stream.id) {
                StreamRow(this, ::setStreamVolume).also { streamsList.addView(it.view) }
            }
            row.bind(stream)
        }
        streamsEmpty.visibility = if (streams.isEmpty()) View.VISIBLE else View.GONE
    }

    private fun setStreamVolume(id: Int, volume: Float, muted: Boolean) {
        worker.execute {
            Node.call("nativeSetStreamVolume", false) { CursioNative.nativeSetStreamVolume(id, volume, muted) }
        }
    }

    private fun onMain(block: () -> Unit) {
        handler.post { if (!isDestroyed) block() }
    }

    private fun setUpPairing() {
        pairCode.setOnEditorActionListener { _, action, _ ->
            if (action == EditorInfo.IME_ACTION_GO) submitCode()
            false
        }
        findViewById<Button>(R.id.pair_button).setOnClickListener { submitCode() }
        showCodeButton.setOnClickListener {
            showPairError(null)
            worker.execute {
                val code = Node.call("nativeShowCode", "") { CursioNative.nativeShowCode() }
                onMain {
                    if (code.isEmpty()) {
                        showPairError("Start the node first.")
                    } else {
                        shownCode.text = code
                        pairingActive.visibility = View.VISIBLE
                        showCodeButton.visibility = View.GONE
                    }
                }
            }
        }
        findViewById<Button>(R.id.cancel_pair_button).setOnClickListener {
            worker.execute {
                Node.call("nativeCancelPairing", false) { CursioNative.nativeCancelPairing() }
            }
        }
    }

    private fun submitCode() {
        val code = pairCode.text.toString().trim().uppercase()
        if (code.isEmpty()) return
        if (!Node.isRunning()) {
            showPairError("Start the node first.")
            return
        }
        worker.execute {
            val err = Node.call("nativeEnterCode", "this build of the node cannot pair yet") {
                CursioNative.nativeEnterCode(code)
            }
            onMain {
                showPairError(err.ifEmpty { null })
                if (err.isEmpty()) pairCode.text.clear()
            }
        }
    }

    private fun showPairError(message: String?) {
        pairError.text = message
        pairError.visibility = if (message == null) View.GONE else View.VISIBLE
    }

    private fun renderPairing(status: NodeStatus) {
        val pairing = status.pairing
        pairingActive.visibility = if (pairing != null) View.VISIBLE else View.GONE
        showCodeButton.visibility = if (pairing == null) View.VISIBLE else View.GONE
        if (pairing != null) {
            shownCode.text = pairing.code ?: ""
            val left = pairing.expiresInS?.let { " · %d:%02d".format(it / 60, it % 60) } ?: ""
            pairingState.text = if (pairing.kind == "shown_code") {
                "Enter this code on your computer$left"
            } else {
                "Looking for the computer showing this code$left"
            }
        }

        val last = status.lastPairing
        lastPairing.visibility = if (last != null && pairing == null) View.VISIBLE else View.GONE
        if (last != null) {
            lastPairing.text = when (last.outcome) {
                "paired" -> "Paired with ${last.peer ?: "a new machine"}."
                "expired" -> "The code expired before a machine used it."
                "too_many_attempts" -> "Too many wrong codes; pairing was closed. Start again."
                "cancelled" -> "Pairing was cancelled."
                else -> "Pairing ended: ${last.outcome}."
            }
        }
    }

    private fun setUpMicGate() {
        val stored = Prefs.micGate(this)
        val level = stored.toIntOrNull()
        gateLevel.progress = (level ?: Prefs.GATE_DEFAULT_LEVEL) - GATE_MIN_DB
        gateGroup.check(
            when {
                stored == Prefs.GATE_OFF -> R.id.gate_off
                level != null -> R.id.gate_fixed
                else -> R.id.gate_auto
            },
        )
        renderGateLevel()

        gateGroup.setOnCheckedChangeListener { _, _ ->
            renderGateLevel()
            applyMicGate()
        }
        gateLevel.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(bar: SeekBar, progress: Int, fromUser: Boolean) = renderGateLevel()

            override fun onStartTrackingTouch(bar: SeekBar) = Unit

            override fun onStopTrackingTouch(bar: SeekBar) = applyMicGate()
        })
    }

    private fun renderGateLevel() {
        val fixed = gateGroup.checkedRadioButtonId == R.id.gate_fixed
        gateLevel.isEnabled = fixed
        gateValue.alpha = if (fixed) 1f else 0.4f
        gateValue.text = "${gateLevel.progress + GATE_MIN_DB} dB"
    }

    /** Store the gate and, if the node runs, apply it now; a start applies it too. */
    private fun applyMicGate() {
        val value = when (gateGroup.checkedRadioButtonId) {
            R.id.gate_off -> Prefs.GATE_OFF
            R.id.gate_fixed -> (gateLevel.progress + GATE_MIN_DB).toString()
            else -> Prefs.GATE_AUTO
        }
        Prefs.setMicGate(this, value)
        if (!Node.isRunning()) return
        worker.execute {
            val err = Node.call("nativeSetMicGate", "this build of the node has no mic gate") {
                CursioNative.nativeSetMicGate(value)
            }
            onMain {
                gateError.text = err
                gateError.visibility = if (err.isEmpty()) View.GONE else View.VISIBLE
            }
        }
    }

    private fun setUpSetupCard() {
        findViewById<Button>(R.id.setup_notify_button).setOnClickListener { requestRuntimePermissions() }
        findViewById<Button>(R.id.setup_mic_button).setOnClickListener {
            // The app asks on first launch, so no rationale here means the
            // user denied twice and Android no longer shows the dialog at all;
            // the app's settings page is then the only place left to grant it.
            if (shouldShowRequestPermissionRationale(Manifest.permission.RECORD_AUDIO)) {
                requestRuntimePermissions()
            } else {
                PowerSetup.openAppDetails(this)
            }
        }
        findViewById<Button>(R.id.setup_battery_button).setOnClickListener {
            PowerSetup.requestIgnoreBatteryOptimizations(this)
        }
        findViewById<Button>(R.id.setup_autostart_button).setOnClickListener {
            Prefs.setOpenedAutostart(this)
            PowerSetup.openAutostartSettings(this)
        }
    }

    private fun refreshSetupCard() {
        val notify = Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        val mic = !MeshService.micGranted(this)
        val battery = !PowerSetup.isIgnoringBatteryOptimizations(this)
        val autostart = PowerSetup.hasXiaomiAutostart() && !Prefs.openedAutostart(this)
        findViewById<View>(R.id.setup_notify).visibility = if (notify) View.VISIBLE else View.GONE
        findViewById<View>(R.id.setup_mic).visibility = if (mic) View.VISIBLE else View.GONE
        findViewById<View>(R.id.setup_battery).visibility = if (battery) View.VISIBLE else View.GONE
        findViewById<View>(R.id.setup_autostart).visibility = if (autostart) View.VISIBLE else View.GONE
        findViewById<View>(R.id.setup_card).visibility =
            if (notify || mic || battery || autostart) View.VISIBLE else View.GONE
    }

    companion object {
        private const val GATE_MIN_DB = -80
        private const val POLL_INTERVAL_MS = 1_000L
        private const val REQUEST_PERMISSIONS = 1
    }
}
