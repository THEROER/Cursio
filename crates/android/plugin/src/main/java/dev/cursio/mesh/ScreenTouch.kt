package dev.cursio.mesh

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.ComponentName
import android.content.Context
import android.graphics.Path
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo

/**
 * A computer's mouse, as a finger on this phone's screen.
 *
 * An accessibility service because that is the one public way an app may touch
 * the screen for the user: `dispatchGesture`. scrcpy injects events with the
 * shell's privileges over ADB; nothing here needs ADB, only that the user
 * turns this service on once in the system's accessibility settings, which
 * Android insists be done by hand.
 *
 * A held button is one continuous touch: the press starts a stroke that will
 * continue, each move continues it, the release ends it. Android takes a
 * continuation only once the previous piece has been dispatched, and the mouse
 * moves faster than that, so moves arriving in between are folded into the
 * next piece rather than queued — the finger goes where the pointer is now,
 * not where it was.
 *
 * Typing goes into whichever field has focus, by setting its text: the
 * service sees text fields but holds no keyboard, so a character is appended
 * at the end of what is there rather than at the caret, and keys that are
 * not characters, Backspace and Enter excepted, do nothing.
 *
 * Gestures arrive from the node's thread and are carried out on the main one.
 */
class ScreenTouch : AccessibilityService() {
    override fun onServiceConnected() {
        instance = this
        Log.i(TAG, "touch from computers is available")
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        if (instance === this) instance = null
        return super.onUnbind(intent)
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

    override fun onInterrupt() {}

    // Everything below runs on the main thread.

    /** Where each finger is, in screen pixels, while one is down. */
    private var fingers: List<Pair<Float, Float>> = emptyList()
    private var strokes: List<GestureDescription.StrokeDescription> = emptyList()
    /** A piece is out and not yet dispatched. */
    private var dispatching = false
    /** Where the fingers should be next, folded from moves that came meanwhile. */
    private var target: List<Pair<Float, Float>>? = null
    private var lifting = false

    private fun pixels(x: Float, y: Float): Pair<Float, Float> {
        val metrics = resources.displayMetrics
        val bounds = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            getSystemService(android.view.WindowManager::class.java).maximumWindowMetrics.bounds
        } else {
            null
        }
        val width = bounds?.width() ?: metrics.widthPixels
        val height = bounds?.height() ?: metrics.heightPixels
        return (x * (width - 1)).coerceAtLeast(0f) to (y * (height - 1)).coerceAtLeast(0f)
    }

    private fun down(points: List<Pair<Float, Float>>) {
        // A press while one is still out (a lost release) starts afresh.
        fingers = points
        target = null
        lifting = false
        strokes = points.map { (x, y) ->
            GestureDescription.StrokeDescription(Path().apply { moveTo(x, y) }, 0, TAP_MS, true)
        }
        send()
    }

    private fun move(points: List<Pair<Float, Float>>) {
        if (strokes.isEmpty()) return
        target = points
        if (!dispatching) next()
    }

    private fun up(points: List<Pair<Float, Float>>) {
        if (strokes.isEmpty()) return
        target = points
        lifting = true
        if (!dispatching) next()
    }

    /** Continue the touch toward [target], ending it if it is lifting. */
    private fun next() {
        val to = target ?: if (lifting) fingers else return
        target = null
        val end = lifting
        strokes = strokes.mapIndexed { i, stroke ->
            val (fx, fy) = fingers.getOrElse(i) { fingers.last() }
            val (tx, ty) = to.getOrElse(i) { to.last() }
            val path = Path().apply {
                moveTo(fx, fy)
                // A continuation must move; a still finger moves by nothing
                // a screen can draw.
                lineTo(if (tx == fx && ty == fy) tx + 0.01f else tx, ty)
            }
            stroke.continueStroke(path, 0, MOVE_MS, !end)
        }
        fingers = to
        if (end) lifting = false
        send(finished = end)
    }

    private fun send(finished: Boolean = false) {
        val gesture = GestureDescription.Builder().apply { strokes.forEach { addStroke(it) } }.build()
        dispatching = true
        val callback = object : GestureResultCallback() {
            override fun onCompleted(gestureDescription: GestureDescription?) = settled()
            override fun onCancelled(gestureDescription: GestureDescription?) {
                // Something on the phone took the touch away (a system gesture,
                // the screen turning off); the next press starts clean.
                strokes = emptyList()
                target = null
                lifting = false
                dispatching = false
            }
        }
        if (!dispatchGesture(gesture, callback, null)) {
            Log.w(TAG, "the phone refused a touch")
            strokes = emptyList()
            dispatching = false
            return
        }
        if (finished) strokes = emptyList()
    }

    private fun settled() {
        dispatching = false
        if (strokes.isNotEmpty() && (target != null || lifting)) next()
    }

    /** A wheel turned: a short swipe, the content following the wheel. */
    private fun scroll(at: Pair<Float, Float>, dx: Int, dy: Int) {
        val metrics = resources.displayMetrics
        val stepX = metrics.widthPixels * SCROLL_STEP
        val stepY = metrics.heightPixels * SCROLL_STEP
        // Wheel away from the user (dy > 0) shows what is above: the finger
        // moves down.
        val toX = (at.first - dx * stepX).coerceIn(0f, metrics.widthPixels - 1f)
        val toY = (at.second + dy * stepY).coerceIn(0f, metrics.heightPixels - 1f)
        val path = Path().apply {
            moveTo(at.first, at.second)
            lineTo(toX, toY)
        }
        val stroke = GestureDescription.StrokeDescription(path, 0, SCROLL_MS)
        dispatchGesture(GestureDescription.Builder().addStroke(stroke).build(), null, null)
    }

    private fun type(edit: (CharSequence) -> CharSequence?) {
        val field = rootInActiveWindow?.findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
        if (field == null || !field.isEditable) {
            Log.i(TAG, "typed from a computer with no text field focused on the phone")
            return
        }
        // A field showing its hint reports the hint as its text.
        val current = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && field.isShowingHintText) "" else field.text ?: ""
        val next = edit(current) ?: return
        val args = Bundle().apply {
            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, next)
        }
        field.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
    }

    private fun enter() {
        val field = rootInActiveWindow?.findFocus(AccessibilityNodeInfo.FOCUS_INPUT) ?: return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            field.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)
        } else {
            type { "$it\n" }
        }
    }

    fun gesture(kind: Int, x1: Float, y1: Float, x2: Float, y2: Float, dx: Int, dy: Int, text: String?) {
        val a = pixels(x1, y1)
        val b = pixels(x2, y2)
        when (kind) {
            0 -> down(listOf(a))
            1 -> move(listOf(a))
            2 -> up(listOf(a))
            3 -> down(listOf(a, b))
            4 -> move(listOf(a, b))
            5 -> up(listOf(a, b))
            6 -> scroll(a, dx, dy)
            7 -> performGlobalAction(GLOBAL_ACTION_BACK)
            8 -> performGlobalAction(GLOBAL_ACTION_HOME)
            9 -> if (text != null) type { "$it$text" }
            10 -> type { if (it.isEmpty()) null else it.subSequence(0, it.length - 1) }
            11 -> enter()
            else -> Log.w(TAG, "a touch of kind $kind is not known")
        }
    }

    companion object {
        private const val TAG = "Cursio"
        private const val TAP_MS = 1L
        private const val MOVE_MS = 8L
        private const val SCROLL_MS = 120L
        /** One wheel notch swipes this share of the screen. */
        private const val SCROLL_STEP = 0.12f

        private val main = Handler(Looper.getMainLooper())

        @Volatile
        private var instance: ScreenTouch? = null

        /** Carry out a touch from a computer, or say once why it cannot be. */
        @Volatile
        private var warned = false

        fun gesture(kind: Int, x1: Float, y1: Float, x2: Float, y2: Float, dx: Int, dy: Int, text: String?) {
            main.post {
                val service = instance
                if (service == null) {
                    if (!warned) {
                        warned = true
                        Log.w(TAG, "a computer touched this screen, but touch from computers is off in the accessibility settings")
                    }
                    return@post
                }
                warned = false
                service.gesture(kind, x1, y1, x2, y2, dx, dy, text)
            }
        }

        /** Whether the user turned this service on in the accessibility settings. */
        fun enabled(context: Context): Boolean {
            val wanted = ComponentName(context, ScreenTouch::class.java).flattenToString()
            val on = Settings.Secure.getString(
                context.contentResolver,
                Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
            ) ?: return false
            return on.split(':').any { it.equals(wanted, ignoreCase = true) }
        }
    }
}
