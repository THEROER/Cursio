package dev.cursio.mesh

import android.content.ClipData
import android.content.ClipboardManager
import android.os.Handler
import android.os.Looper
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * A computer's copy onto this phone's clipboard. Called from the node's own
 * thread through `CursioNative.putClipboard`, which `crates/android/src/clipboard.rs`
 * calls whenever a computer offers a copy (see `clipboard_session/phone.rs`).
 *
 * Writing from the background is allowed; reading is not, which is why the
 * other direction is [ClipboardSendActivity] and never anything here.
 */
object PhoneClipboard {
    /** The label Android shows as the clip's source: these copies are Cursio's. */
    const val LABEL = "Cursio"

    private val main = Handler(Looper.getMainLooper())

    /**
     * Put [text] on the clipboard; answers `""` or why not. On the main
     * thread, where the clipboard service's callbacks land, waiting a moment
     * for it so a refusal reaches the node's log instead of vanishing.
     */
    fun put(text: String): String {
        val ctx = ShareReceiver.context ?: return "the app is not running"
        // Read only after the latch, which orders it after the write.
        var error = ""
        val done = CountDownLatch(1)
        main.post {
            try {
                val clipboard = ctx.getSystemService(ClipboardManager::class.java)
                clipboard.setPrimaryClip(ClipData.newPlainText(LABEL, text))
            } catch (e: Exception) {
                error = e.message ?: e.javaClass.simpleName
            }
            done.countDown()
        }
        // A main thread busy elsewhere still runs the post; only the answer
        // is given up on.
        return if (done.await(2, TimeUnit.SECONDS)) error else ""
    }
}
