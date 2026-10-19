package dev.cursio.mesh

import android.content.Context
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Display
import android.view.Surface
import android.view.WindowManager

/**
 * This phone's screen, encoded for a computer to show.
 *
 * The projection draws the display into a virtual display whose surface is the
 * hardware H.264 encoder's input, so no pixel passes through this process: the
 * encoder's output units go down to the node as they come (`nativeScreenUnit`,
 * read in place from the codec's buffer) and out to the computer untouched,
 * the way an iPhone's AirPlay stream does. The computer's viewer already
 * decodes that codec.
 *
 * Kept small on purpose: the long side is held to [MAX_LONG_SIDE] because the
 * picture crosses Wi-Fi beside the mouse, and the encoder repeats the last
 * frame on a still screen ([REPEAT_AFTER_US]) so a viewer joining late still
 * gets pictures to decode. A keyframe is otherwise only sent when one is
 * asked for, which is what a viewer that just opened or fell behind does.
 *
 * Turning the phone changes the display's size, and an encoder's size is fixed
 * when it is configured, so a turn tears the encoder down and builds one at the
 * new size; the node is told first ([CursioNative.nativeScreenStarted]), which
 * reopens the computer's window at the new shape.
 *
 * Everything here runs on [thread]; the node's requests are posted to it.
 */
class ScreenShare private constructor(
    private val context: Context,
    private val projection: MediaProjection,
    private val ended: () -> Unit,
) {
    private val thread = HandlerThread("cursio-screen").apply { start() }
    private val handler = Handler(thread.looper)
    private val displays = context.getSystemService(DisplayManager::class.java)

    private var display: VirtualDisplay? = null
    private var codec: MediaCodec? = null
    private var size: Size? = null
    private var bitrate = START_BITRATE
    private var stopped = false

    private data class Size(val width: Int, val height: Int, val dpi: Int)

    private val projectionStopped = object : MediaProjection.Callback() {
        override fun onStop() {
            // The system took the projection: the user tapped its own stop
            // button, or another app's consent replaced ours.
            handler.post { stop() }
        }
    }

    private val turned = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) {}
        override fun onDisplayRemoved(displayId: Int) {}
        override fun onDisplayChanged(displayId: Int) {
            if (displayId == Display.DEFAULT_DISPLAY) resizeIfTurned()
        }
    }

    private fun start() {
        // Android 14 refuses a virtual display to a projection with no
        // callback registered first.
        projection.registerCallback(projectionStopped, handler)
        val (size, surface) = configureFitted()
        display = projection.createVirtualDisplay(
            "cursio-screen",
            size.width,
            size.height,
            size.dpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
            surface,
            null,
            handler,
        )
        displays.registerDisplayListener(turned, handler)
    }

    /**
     * The display's size now, scaled so its long side fits [MAX_LONG_SIDE],
     * each side the nearest multiple of [step].
     *
     * Nearest, not rounded down: a 1220×2712 screen rounded down to 16 was
     * encoded at 848×1920, 1.8% narrower than the screen, so Android
     * letterboxed the picture into it — black bands on the computer, and a
     * tap there landing that band's height away from where it was aimed.
     * Nearest 8 gives 864×1920, what scrcpy picks.
     */
    private fun measure(step: Int): Size {
        val windows = context.getSystemService(WindowManager::class.java)
        val (width, height) = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val bounds = windows.maximumWindowMetrics.bounds
            bounds.width() to bounds.height()
        } else {
            val metrics = android.util.DisplayMetrics()
            @Suppress("DEPRECATION")
            windows.defaultDisplay.getRealMetrics(metrics)
            metrics.widthPixels to metrics.heightPixels
        }
        val scale = minOf(1.0, MAX_LONG_SIDE.toDouble() / maxOf(width, height))
        fun fit(side: Int) = maxOf(step, Math.round(side * scale / step).toInt() * step)
        return Size(fit(width), fit(height), context.resources.displayMetrics.densityDpi)
    }

    /**
     * Measure and build the encoder, at multiples of 8 and, if the encoder
     * refuses that, of 16 — whole macroblocks, which every encoder takes, where
     * some refuse a size merely even.
     */
    private fun configureFitted(): Pair<Size, Surface> {
        val fine = measure(8)
        return try {
            fine to configure(fine)
        } catch (e: RuntimeException) {
            Log.w(TAG, "the encoder refused ${fine.width}x${fine.height}; using whole macroblocks", e)
            val coarse = measure(16)
            coarse to configure(coarse)
        }
    }

    /** Build an encoder for [size], start it, and answer its input surface. */
    private fun configure(size: Size): Surface {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, size.width, size.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, FRAME_RATE)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, KEYFRAME_INTERVAL_S)
            setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, REPEAT_AFTER_US)
            // Realtime: a frame late is a frame the computer should not see.
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) setInteger(MediaFormat.KEY_LATENCY, 1)
        }
        val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {}

            override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                if (codec !== this@ScreenShare.codec) return
                try {
                    val buffer = codec.getOutputBuffer(index)
                    if (buffer != null && info.size > 0) {
                        CursioNative.nativeScreenUnit(
                            buffer,
                            info.offset,
                            info.size,
                            info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0,
                            info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0,
                        )
                    }
                    codec.releaseOutputBuffer(index, false)
                } catch (e: IllegalStateException) {
                    Log.w(TAG, "the screen encoder went away mid-frame", e)
                }
            }

            override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                Log.w(TAG, "the screen encoder failed", e)
                if (codec === this@ScreenShare.codec) stop()
            }

            override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {}
        }, handler)
        val surface = try {
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            codec.createInputSurface()
        } catch (e: RuntimeException) {
            codec.release()
            throw e
        }
        this.codec = codec
        this.size = size
        // Before the first unit: the node opens windows at this size.
        CursioNative.nativeScreenStarted(size.width, size.height)
        codec.start()
        return surface
    }

    private fun resizeIfTurned() {
        if (stopped) return
        val was = size ?: return
        // Either step: an encoder that refused multiples of 8 runs at 16.
        val same = listOf(measure(8), measure(16)).any { it.width == was.width && it.height == was.height }
        if (same) return
        val old = codec
        codec = null
        releaseCodec(old)
        val (now, surface) = configureFitted()
        Log.i(TAG, "the phone turned; screen ${now.width}x${now.height}")
        display?.resize(now.width, now.height, now.dpi)
        display?.surface = surface
    }

    private fun releaseCodec(codec: MediaCodec?) {
        codec ?: return
        try {
            codec.stop()
        } catch (e: IllegalStateException) {
            Log.w(TAG, "the screen encoder would not stop", e)
        }
        codec.release()
    }

    fun keyframe() = handler.post {
        codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
    }

    fun setBitrate(bitsPerSecond: Int) = handler.post {
        bitrate = bitsPerSecond.coerceIn(MIN_BITRATE, MAX_BITRATE)
        codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, bitrate) })
    }

    /** End the share; safe to call twice and from any thread. */
    fun stop() {
        handler.post {
            if (stopped) return@post
            stopped = true
            displays.unregisterDisplayListener(turned)
            display?.release()
            display = null
            val old = codec
            codec = null
            releaseCodec(old)
            projection.unregisterCallback(projectionStopped)
            projection.stop()
            CursioNative.nativeScreenStopped()
            ended()
            thread.quitSafely()
        }
    }

    companion object {
        private const val TAG = "Cursio"
        private const val MAX_LONG_SIDE = 1920
        private const val FRAME_RATE = 60
        private const val KEYFRAME_INTERVAL_S = 10
        private const val REPEAT_AFTER_US = 200_000L
        private const val START_BITRATE = 6_000_000
        private const val MIN_BITRATE = 1_000_000
        private const val MAX_BITRATE = 12_000_000

        /**
         * Start sharing through [projection]; [ended] runs once, on the
         * share's own thread, however it ends.
         *
         * @throws RuntimeException when the encoder or the display cannot be made.
         */
        fun start(context: Context, projection: MediaProjection, ended: () -> Unit): ScreenShare {
            val share = ScreenShare(context.applicationContext, projection, ended)
            try {
                share.start()
            } catch (e: RuntimeException) {
                share.stop()
                throw e
            }
            return share
        }
    }
}
