package dev.cursio.mesh

import android.content.Context
import android.graphics.Typeface
import android.os.SystemClock
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.SeekBar
import android.widget.TextView

/**
 * One audio stream with its own volume and mute.
 *
 * Kept by id and updated in place, because the screen is redrawn every second
 * and rebuilding the row would yank the slider out from under a finger. For
 * the same reason the node's figure is ignored while the user is dragging and
 * briefly after: the poll that follows a change may predate it, and showing it
 * would snap the slider back.
 */
class StreamRow(
    context: Context,
    private val onChange: (id: Int, volume: Float, muted: Boolean) -> Unit,
) {
    private var id = 0
    private var muted = false
    private var dragging = false
    private var lastLocalChange = 0L

    private val icon = ImageView(context)
    private val title = TextView(context).apply {
        setTextColor(context.getColor(R.color.text))
        textSize = 15f
        setTypeface(typeface, Typeface.BOLD)
        maxLines = 1
    }
    private val subtitle = TextView(context).apply {
        setTextColor(context.getColor(R.color.text_muted))
        textSize = 13f
        maxLines = 1
    }
    private val mute = Button(context, null, 0, android.R.style.Widget_Material_Button_Borderless_Colored)
    private val slider = SeekBar(context).apply { max = MAX_PERCENT }
    private val percent = TextView(context).apply {
        setTextColor(context.getColor(R.color.text_muted))
        textSize = 13f
        typeface = Typeface.MONOSPACE
        gravity = Gravity.END
    }

    val view: View

    init {
        val dp = context.resources.displayMetrics.density
        val header = LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            addView(icon, LinearLayout.LayoutParams((24 * dp).toInt(), (24 * dp).toInt()).apply {
                marginEnd = (12 * dp).toInt()
            })
            val text = LinearLayout(context).apply {
                orientation = LinearLayout.VERTICAL
                addView(title)
                addView(subtitle)
            }
            addView(text, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
            addView(mute)
        }
        val volume = LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            addView(slider, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
            addView(percent, LinearLayout.LayoutParams((56 * dp).toInt(), LinearLayout.LayoutParams.WRAP_CONTENT))
        }
        view = LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(0, (8 * dp).toInt(), 0, (8 * dp).toInt())
            addView(header)
            addView(volume)
        }

        mute.setOnClickListener {
            muted = !muted
            lastLocalChange = SystemClock.uptimeMillis()
            showMute()
            onChange(id, slider.progress / 100f, muted)
        }
        slider.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(bar: SeekBar, progress: Int, fromUser: Boolean) {
                percent.text = "$progress%"
                if (!fromUser) return
                lastLocalChange = SystemClock.uptimeMillis()
                onChange(id, progress / 100f, muted)
            }

            override fun onStartTrackingTouch(bar: SeekBar) {
                dragging = true
            }

            override fun onStopTrackingTouch(bar: SeekBar) {
                dragging = false
                lastLocalChange = SystemClock.uptimeMillis()
            }
        })
    }

    fun bind(stream: NodeStatus.Stream) {
        id = stream.id
        icon.setImageResource(if (stream.incoming) R.drawable.ic_in else R.drawable.ic_out)
        icon.contentDescription = if (stream.incoming) "Playing here" else "Sent from this phone"
        title.text = stream.name.ifEmpty { "Stream ${stream.id}" }
        val direction = if (stream.incoming) "from ${stream.peer}" else "to ${stream.peer}"
        subtitle.text = if (stream.playing) direction else "$direction · idle"

        if (dragging || SystemClock.uptimeMillis() - lastLocalChange < LOCAL_CHANGE_HOLD_MS) return
        muted = stream.muted
        showMute()
        val progress = (stream.volume * 100).toInt().coerceIn(0, MAX_PERCENT)
        slider.progress = progress
        percent.text = "$progress%"
    }

    private fun showMute() {
        mute.text = if (muted) "Unmute" else "Mute"
        slider.alpha = if (muted) 0.4f else 1f
    }

    companion object {
        private const val MAX_PERCENT = 150
        private const val LOCAL_CHANGE_HOLD_MS = 1_500L
    }
}
