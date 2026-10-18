package dev.cursio.mesh

import android.Manifest
import android.content.ContentUris
import android.content.Context
import android.content.pm.PackageManager
import android.database.ContentObserver
import android.graphics.Bitmap
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.MediaStore
import android.util.Log
import android.util.Size
import org.json.JSONArray
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.FileInputStream

/**
 * This phone's photos and videos, browsed from a peer
 * (`crates/core/src/gallery_session.rs` is the other half).
 *
 * Read from MediaStore rather than by walking storage: it already indexes the
 * camera roll, screenshots and every app's saved pictures, with dates and
 * sizes, and reading it needs only READ_MEDIA_IMAGES/VIDEO (13+) or
 * READ_EXTERNAL_STORAGE (older) — not access to all files.
 *
 * Images and videos are two tables with their own ids, so an id here is
 * `i:<id>` or `v:<id>`, and a page merges the two newest-first. Items with
 * the same date are ordered by that id string, which is what a page mark
 * carries to say where the last page ended. The date is when the file last
 * changed, which for a photo is almost always when it was taken.
 *
 * Thumbnails come from `loadThumbnail` (API 29+), which uses the system's own
 * cache; older Android decodes a downsampled bitmap itself. A slice of a file
 * is read through a file descriptor from the content resolver, so the phone's
 * scoped storage rules are the ones that decide what may be read.
 */
object GalleryStore {
    private const val TAG = "Cursio"

    @Volatile
    var context: Context? = null

    private var observing = false
    private val main = Handler(Looper.getMainLooper())

    /** A burst of changes — a camera saving a photo is several — told once. */
    private val tell = Runnable {
        try {
            CursioNative.nativeGalleryChanged()
        } catch (e: UnsatisfiedLinkError) {
            Log.w(TAG, "the node is not loaded; a gallery change goes untold", e)
        }
    }

    fun permissions(): Array<String> =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            arrayOf(Manifest.permission.READ_MEDIA_IMAGES, Manifest.permission.READ_MEDIA_VIDEO)
        } else {
            arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE)
        }

    fun granted(context: Context): Boolean =
        permissions().all { context.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }

    /** Start telling the node when MediaStore changes. Safe to call again. */
    fun watch(context: Context) {
        main.post {
            if (observing || !granted(context)) return@post
            val observer = object : ContentObserver(main) {
                override fun onChange(selfChange: Boolean) {
                    main.removeCallbacks(tell)
                    main.postDelayed(tell, 1000)
                }
            }
            val resolver = context.applicationContext.contentResolver
            resolver.registerContentObserver(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, true, observer)
            resolver.registerContentObserver(MediaStore.Video.Media.EXTERNAL_CONTENT_URI, true, observer)
            observing = true
        }
    }

    private const val REFUSED = "allow Cursio to see photos and videos on the phone"

    /** A page of the list, as the JSON `crates/android/src/gallery.rs` reads. */
    fun list(request: String): String {
        val ctx = context ?: return JSONObject().put("error", "the app is not running").toString()
        if (!granted(ctx)) return JSONObject().put("error", REFUSED).toString()
        return try {
            val req = JSONObject(request)
            val limit = req.getInt("limit")
            val afterMs = if (req.isNull("after_ms")) null else req.getLong("after_ms")
            val afterId = if (req.isNull("after_id")) null else req.getString("after_id")
            val filter = req.optString("filter", "all")
            val items = ArrayList<JSONObject>()
            if (filter != "videos") items += query(ctx, false, limit, afterMs, afterId)
            if (filter != "images") items += query(ctx, true, limit, afterMs, afterId)
            items.sortWith(compareByDescending<JSONObject> { it.getLong("taken") }.thenBy { it.getString("id") })
            val page = JSONArray()
            items.take(limit).forEach { page.put(it) }
            JSONObject().put("items", page).toString()
        } catch (e: SecurityException) {
            JSONObject().put("error", REFUSED).toString()
        } catch (e: Exception) {
            Log.w(TAG, "listing the gallery failed", e)
            JSONObject().put("error", e.message ?: e.javaClass.simpleName).toString()
        }
    }

    /**
     * The peer's id for row [number]: zero-padded, so comparing ids as strings
     * — what a page mark does — orders them as the numbers SQL sorts by.
     */
    private fun idOf(video: Boolean, number: Long) = (if (video) "v:" else "i:") + "%019d".format(number)

    private fun query(ctx: Context, video: Boolean, limit: Int, afterMs: Long?, afterId: String?): List<JSONObject> {
        val uri = if (video) MediaStore.Video.Media.EXTERNAL_CONTENT_URI else MediaStore.Images.Media.EXTERNAL_CONTENT_URI
        // DATE_MODIFIED, not DATE_TAKEN: taken is empty for many screenshots
        // and downloads, and an expression choosing between the two is not a
        // column MediaProvider accepts in a projection since Android 10.
        val date = MediaStore.MediaColumns.DATE_MODIFIED
        val columns = mutableListOf(
            MediaStore.MediaColumns._ID,
            MediaStore.MediaColumns.DISPLAY_NAME,
            MediaStore.MediaColumns.SIZE,
            MediaStore.MediaColumns.WIDTH,
            MediaStore.MediaColumns.HEIGHT,
            MediaStore.MediaColumns.MIME_TYPE,
            date,
        )
        if (video) columns += MediaStore.Video.VideoColumns.DURATION
        // The page edge's own second is fetched again and the rows up to the
        // mark's id skipped here; dates are in seconds, so a second shared by
        // a burst of photos is the usual case, not a rare one.
        val selection = afterMs?.let { "$date <= ?" }
        val args = afterMs?.let { arrayOf((it / 1000).toString()) }
        val order = "$date DESC, ${MediaStore.MediaColumns._ID} ASC"
        val out = ArrayList<JSONObject>()
        ctx.contentResolver.query(uri, columns.toTypedArray(), selection, args, order)?.use { c ->
            val idCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns._ID)
            val nameCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns.DISPLAY_NAME)
            val sizeCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns.SIZE)
            val wCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns.WIDTH)
            val hCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns.HEIGHT)
            val mimeCol = c.getColumnIndexOrThrow(MediaStore.MediaColumns.MIME_TYPE)
            val dateCol = c.getColumnIndexOrThrow(date)
            val durCol = if (video) c.getColumnIndexOrThrow(MediaStore.Video.VideoColumns.DURATION) else -1
            while (c.moveToNext() && out.size < limit) {
                val id = idOf(video, c.getLong(idCol))
                val at = c.getLong(dateCol) * 1000
                if (afterMs != null && at == afterMs && afterId != null && id <= afterId) continue
                val item = JSONObject()
                    .put("id", id)
                    .put("name", c.getString(nameCol) ?: id)
                    .put("video", video)
                    .put("taken", at)
                    .put("size", c.getLong(sizeCol))
                    .put("width", c.getInt(wCol))
                    .put("height", c.getInt(hCol))
                    .put("mime", c.getString(mimeCol) ?: if (video) "video/mp4" else "image/jpeg")
                if (video) item.put("duration", c.getLong(durCol))
                out += item
            }
        }
        return out
    }

    private fun uriOf(id: String): Uri {
        val number = id.drop(2).toLongOrNull() ?: throw IllegalArgumentException("not an item of this gallery")
        return when {
            id.startsWith("i:") -> ContentUris.withAppendedId(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, number)
            id.startsWith("v:") -> ContentUris.withAppendedId(MediaStore.Video.Media.EXTERNAL_CONTENT_URI, number)
            else -> throw IllegalArgumentException("not an item of this gallery")
        }
    }

    /**
     * A JPEG thumbnail when [thumbPx] > 0 (null if the system has none), else
     * [len] bytes of the file from [offset]. Refusals are thrown with the
     * reason as the message; the node passes the message on.
     */
    fun bytes(id: String, thumbPx: Int, offset: Long, len: Int): ByteArray? {
        val ctx = context ?: throw IllegalStateException("the app is not running")
        if (!granted(ctx)) throw SecurityException(REFUSED)
        val uri = uriOf(id)
        return if (thumbPx > 0) thumbnail(ctx, uri, id.startsWith("v:"), thumbPx) else read(ctx, uri, offset, len)
    }

    private fun thumbnail(ctx: Context, uri: Uri, video: Boolean, px: Int): ByteArray? {
        val bitmap: Bitmap = try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                ctx.contentResolver.loadThumbnail(uri, Size(px, px), null)
            } else if (video) {
                @Suppress("DEPRECATION")
                MediaStore.Video.Thumbnails.getThumbnail(
                    ctx.contentResolver, ContentUris.parseId(uri), MediaStore.Video.Thumbnails.MINI_KIND, null,
                ) ?: return null
            } else {
                @Suppress("DEPRECATION")
                MediaStore.Images.Thumbnails.getThumbnail(
                    ctx.contentResolver, ContentUris.parseId(uri), MediaStore.Images.Thumbnails.MINI_KIND, null,
                ) ?: return null
            }
        } catch (e: java.io.IOException) {
            // A file MediaStore lists but cannot decode: a placeholder, not an error.
            Log.d(TAG, "no thumbnail for $uri: ${e.message}")
            return null
        }
        val scaled = if (bitmap.width > px || bitmap.height > px) {
            val scale = px.toFloat() / maxOf(bitmap.width, bitmap.height)
            Bitmap.createScaledBitmap(bitmap, (bitmap.width * scale).toInt().coerceAtLeast(1), (bitmap.height * scale).toInt().coerceAtLeast(1), true)
        } else bitmap
        val out = ByteArrayOutputStream()
        scaled.compress(Bitmap.CompressFormat.JPEG, 80, out)
        return out.toByteArray()
    }

    private fun read(ctx: Context, uri: Uri, offset: Long, len: Int): ByteArray {
        val fd = ctx.contentResolver.openFileDescriptor(uri, "r") ?: throw IllegalStateException("the phone could not open the file")
        fd.use {
            FileInputStream(it.fileDescriptor).use { stream ->
                stream.channel.position(offset)
                val buffer = ByteArray(len)
                var filled = 0
                while (filled < len) {
                    val n = stream.read(buffer, filled, len - filled)
                    if (n < 0) break
                    filled += n
                }
                return if (filled == len) buffer else buffer.copyOf(filled)
            }
        }
    }
}
