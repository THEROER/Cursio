package dev.cursio.mesh

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.projection.MediaProjection
import android.os.Handler
import android.os.Process
import android.util.Log
import androidx.annotation.RequiresApi

/**
 * What this phone is playing, read through AudioPlaybackCapture and handed to
 * the node for the peer it is routed to.
 *
 * Android gives an app no loopback of the output mix; a `MediaProjection` the
 * user consented to is the only key to other apps' audio, and the platform
 * still limits it to the usages matched here and to apps that allow capture.
 *
 * Reading runs on a thread of its own because `READ_BLOCKING` waits for each
 * block, and the node takes ~10 ms at a time so a peer hears it with no more
 * delay than the link adds.
 */
@RequiresApi(29)
class PlaybackCapture private constructor(
    private val projection: MediaProjection,
    private val record: AudioRecord,
    private val ended: () -> Unit,
) {
    @Volatile
    private var running = true
    private var stopped = false
    private val reader = Thread(::pump, "cursio-playback-capture")

    private fun pump() {
        val block = FloatArray(BLOCK_SAMPLES)
        var refused = false
        while (running) {
            val n = record.read(block, 0, block.size, AudioRecord.READ_BLOCKING)
            if (!running) break
            if (n < 0) {
                Log.w(TAG, "reading playback capture failed with $n")
                ended()
                return
            }
            if (n == 0) continue
            try {
                CursioNative.nativePushPlayback(block, n)
            } catch (e: UnsatisfiedLinkError) {
                Log.w(TAG, "this native library cannot take the phone's playback", e)
                ended()
                return
            } catch (e: RuntimeException) {
                // Said once: one block refused is the next one refused too,
                // and logging each would be a hundred lines a second.
                if (!refused) Log.w(TAG, "the node refused the phone's playback", e)
                refused = true
            }
        }
    }

    /** Stop reading and give up the projection. Safe to call more than once. */
    @Synchronized
    fun stop() {
        if (stopped) return
        stopped = true
        running = false
        // Stopping the record returns a read blocked inside it, so the join
        // does not wait out a block that may never fill.
        record.stop()
        reader.join(JOIN_TIMEOUT_MS)
        record.release()
        projection.stop()
    }

    companion object {
        private const val TAG = "Cursio"
        private const val SAMPLE_RATE = 48_000
        private const val CHANNELS = 2
        private const val BLOCK_SAMPLES = SAMPLE_RATE / 100 * CHANNELS
        private const val JOIN_TIMEOUT_MS = 1_000L

        /**
         * Begin capturing through [projection], or throw naming why not.
         *
         * [ended] runs, on [handler]'s thread, when the capture ends by itself
         * (the user revoked the projection, or the record failed) and also
         * after [stop], since giving up the projection reports it stopped.
         * It must therefore tolerate a capture that is already stopped.
         *
         * The caller must hold `RECORD_AUDIO`; checked there, where a refusal
         * can be shown.
         */
        @SuppressLint("MissingPermission")
        fun start(projection: MediaProjection, handler: Handler, ended: () -> Unit): PlaybackCapture {
            val onEnded = { handler.post(ended); Unit }
            // Android 14 refuses to use a projection with no callback
            // registered, and this is also how "stop sharing" reaches us.
            projection.registerCallback(
                object : MediaProjection.Callback() {
                    override fun onStop() {
                        onEnded()
                    }
                },
                handler,
            )
            val record = try {
                buildRecord(projection)
            } catch (e: RuntimeException) {
                projection.stop()
                throw e
            }
            val capture = PlaybackCapture(projection, record, onEnded)
            try {
                record.startRecording()
                check(record.recordingState == AudioRecord.RECORDSTATE_RECORDING) {
                    "the playback capture did not start recording"
                }
            } catch (e: RuntimeException) {
                record.release()
                projection.stop()
                throw e
            }
            capture.reader.start()
            return capture
        }

        @SuppressLint("MissingPermission")
        private fun buildRecord(projection: MediaProjection): AudioRecord {
            val config = AudioPlaybackCaptureConfiguration.Builder(projection)
                .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
                .addMatchingUsage(AudioAttributes.USAGE_GAME)
                .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
                // What peers send this phone plays as media too; captured, it
                // would go straight back to them as an echo.
                .excludeUid(Process.myUid())
                .build()
            val format = AudioFormat.Builder()
                .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
                .setSampleRate(SAMPLE_RATE)
                .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                .build()
            val minimum = AudioRecord.getMinBufferSize(
                SAMPLE_RATE,
                AudioFormat.CHANNEL_IN_STEREO,
                AudioFormat.ENCODING_PCM_FLOAT,
            )
            val record = AudioRecord.Builder()
                .setAudioFormat(format)
                .setBufferSizeInBytes(maxOf(minimum, BLOCK_SAMPLES * Float.SIZE_BYTES * 4))
                .setAudioPlaybackCaptureConfig(config)
                .build()
            if (record.state != AudioRecord.STATE_INITIALIZED) {
                record.release()
                throw IllegalStateException("Android could not open a playback capture")
            }
            return record
        }
    }
}
