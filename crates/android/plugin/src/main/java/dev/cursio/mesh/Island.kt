package dev.cursio.mesh

import android.app.Notification
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.PorterDuff
import android.graphics.PorterDuffXfermode
import android.graphics.RectF
import android.graphics.drawable.Icon
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/**
 * HyperOS's Super Island, reached through its Focus notification extras.
 *
 * HyperOS 3.0 puts only Focus notifications in the island: an ordinary
 * notification carrying a JSON description under `miui.focus.param`, with the
 * pictures and buttons it names in the `miui.focus.pics` and
 * `miui.focus.actions` bundles. Xiaomi documents the shape only partly (its
 * push partners' pages); the field names here follow those pages and the
 * open-source HyperIsland ToolKit, which is what third-party apps use.
 *
 * Asked before use rather than always attached: a phone without the island
 * ignores the extras, but the system's own answer to "may this app show a
 * Focus notification" is what decides whether HyperOS draws it at all, and a
 * refusal there is worth a log line rather than an island that silently never
 * appears.
 */
object Island {
    private const val TAG = "Cursio"
    private const val PIC = "miui.focus.pic_cursio"
    private const val ACTION = "miui.focus.action_cursio"

    /** Whether this phone draws Focus notifications in an island for us. */
    fun supported(ctx: Context): Boolean {
        if (!Build.MANUFACTURER.equals("Xiaomi", ignoreCase = true)) return false
        if (!systemFlag("persist.sys.feature.island")) return false
        val allowed = try {
            ctx.contentResolver
                .call(
                    Uri.parse("content://miui.statusbar.notification.public"),
                    "canShowFocus",
                    null,
                    Bundle().apply { putString("package", ctx.packageName) },
                )
                ?.getBoolean("canShowFocus", false) ?: false
        } catch (e: Exception) {
            Log.w(TAG, "could not ask HyperOS about Focus notifications: ${e.message}")
            false
        }
        Log.i(TAG, "HyperOS Focus notifications for Cursio: ${if (allowed) "allowed" else "not allowed"}")
        return allowed
    }

    /** The extras that put [title] and [button] in the island. */
    fun extras(ctx: Context, title: String, button: Notification.Action): Bundle =
        focus(
            business = "find_phone",
            heading = "Find my phone",
            title = title,
            content = title,
            islandText = "Ringing",
            // Above a hotspot's or a timer's: a ringing phone is the one thing
            // the person holding it needs to see.
            priority = 3,
            updatable = false,
            pic = Icon.createWithResource(ctx, R.drawable.ic_notify),
            button = button,
        )

    /**
     * The extras that put a computer's player in the island, drawn as close
     * to HyperOS's own music island as its Focus templates allow.
     *
     * HyperOS gives its native music island only to an app playing sound on
     * the phone itself, which a remote control is not; there is no API that
     * asks for it. The Focus "chat" template is the richest one left, and it
     * is what HyperBridge, the island bridge for other apps, settles on too:
     * the rounded cover, the track, a line saying where it plays, the card
     * and the island's glow in the cover's own colour, the track's progress,
     * and previous / play-pause / next.
     */
    fun mediaExtras(
        title: String,
        subtitle: String,
        cover: Bitmap?,
        fallback: Icon,
        positionMs: Long?,
        durationMs: Long?,
        buttons: List<Notification.Action>,
    ): Bundle {
        val tint = cover?.let(::dominantColor) ?: 0xFF2D2D2D.toInt()
        val (textColor, subColor) =
            if (luminance(tint) > 0.55) "#111111" to "#333333" else "#FFFFFF" to "#E6E6E6"
        val tintHex = String.format("#%06X", 0xFFFFFF and tint)
        val coverIcon = cover?.let { Icon.createWithBitmap(rounded(it)) } ?: fallback
        val picInfo = JSONObject().put("type", 1).put("pic", PIC)
        val actionRefs = JSONArray()
        buttons.forEachIndexed { i, _ -> actionRefs.put(JSONObject().put("type", 0).put("action", "$ACTION$i")) }
        val param = JSONObject()
            .put("protocol", 3)
            .put("business", "music")
            .put("ticker", title)
            .put("aodTitle", title)
            .put("updatable", true)
            // A new track is not news worth taking over the screen for.
            .put("enableFloat", false)
            .put("islandFirstFloat", false)
            .put("hideDeco", true)
            .put("reopen", true)
            .put("showSmallIcon", true)
            .put(
                "chatInfo",
                JSONObject()
                    .put("type", 1)
                    .put("title", title)
                    .put("content", subtitle)
                    .put("picProfile", PIC)
                    .put("picProfileDark", PIC)
                    .put("colorTitle", textColor)
                    .put("colorTitleDark", textColor)
                    .put("colorContent", subColor)
                    .put("colorContentDark", subColor)
                    .put("actions", actionRefs),
            )
            .put("bgInfo", JSONObject().put("type", 1).put("colorBg", tintHex))
            .put(
                "param_island",
                JSONObject()
                    .put("islandProperty", 1)
                    // Below a ringing phone, a call or a timer.
                    .put("islandPriority", 1)
                    .put("highlightColor", tintHex)
                    .put(
                        "bigIslandArea",
                        JSONObject().put(
                            "imageTextInfoLeft",
                            JSONObject()
                                .put("type", 1)
                                .put("picInfo", picInfo)
                                .put("textInfo", JSONObject().put("title", title)),
                        ),
                    )
                    .put("smallIslandArea", JSONObject().put("picInfo", picInfo)),
            )
        if (positionMs != null && durationMs != null && durationMs > 0) {
            val progress = (positionMs * 100 / durationMs).toInt().coerceIn(0, 100)
            param.put("progressInfo", JSONObject().put("progress", progress).put("colorProgress", textColor))
        }
        return Bundle().apply {
            putString("miui.focus.param", JSONObject().put("param_v2", param).toString())
            putBundle("miui.focus.pics", Bundle().apply { putParcelable(PIC, coverIcon) })
            putBundle(
                "miui.focus.actions",
                Bundle().apply { buttons.forEachIndexed { i, b -> putParcelable("$ACTION$i", b) } },
            )
        }
    }

    /**
     * The colour a cover reads as: the average of its saturated pixels, so a
     * red rose on black gives red rather than the near-black most of it is.
     */
    private fun dominantColor(cover: Bitmap): Int {
        val small = Bitmap.createScaledBitmap(cover, 24, 24, true)
        var r = 0L
        var g = 0L
        var b = 0L
        var weight = 0L
        val hsv = FloatArray(3)
        for (y in 0 until small.height) for (x in 0 until small.width) {
            val c = small.getPixel(x, y)
            Color.colorToHSV(c, hsv)
            val w = (1 + hsv[1] * hsv[2] * 20).toLong()
            r += Color.red(c) * w
            g += Color.green(c) * w
            b += Color.blue(c) * w
            weight += w
        }
        // Darkened a little: white text stays readable on all but the palest.
        val f = 0.8
        return Color.rgb((r / weight * f).toInt(), (g / weight * f).toInt(), (b / weight * f).toInt())
    }

    private fun luminance(c: Int): Double =
        (0.299 * Color.red(c) + 0.587 * Color.green(c) + 0.114 * Color.blue(c)) / 255

    private fun rounded(src: Bitmap): Bitmap {
        val out = Bitmap.createBitmap(src.width, src.height, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(out)
        val paint = Paint(Paint.ANTI_ALIAS_FLAG)
        val radius = src.width * 0.18f
        canvas.drawRoundRect(RectF(0f, 0f, src.width.toFloat(), src.height.toFloat()), radius, radius, paint)
        paint.xfermode = PorterDuffXfermode(PorterDuff.Mode.SRC_IN)
        canvas.drawBitmap(src, 0f, 0f, paint)
        return out
    }

    private fun focus(
        business: String,
        heading: String,
        title: String,
        content: String,
        islandText: String,
        priority: Int,
        updatable: Boolean,
        pic: Icon,
        button: Notification.Action,
    ): Bundle {
        val picInfo = JSONObject().put("type", 1).put("pic", PIC)
        val param = JSONObject()
            .put("protocol", 3)
            .put("business", business)
            .put("ticker", title)
            .put("aodTitle", title)
            .put("updatable", updatable)
            .put("enableFloat", !updatable)
            .put("islandFirstFloat", !updatable)
            .put(
                "baseInfo",
                JSONObject().put("type", 1).put("title", heading).put("content", content),
            )
            // The picture beside the text in the expanded card, not only in
            // the island itself: without it a player's card was two lines of
            // text on black.
            .put("picInfo", picInfo)
            .put(
                "param_island",
                JSONObject()
                    .put("islandProperty", 1)
                    .put("islandPriority", priority)
                    .put(
                        "bigIslandArea",
                        JSONObject().put(
                            "imageTextInfoLeft",
                            JSONObject()
                                .put("type", 1)
                                .put("picInfo", picInfo)
                                .put("textInfo", JSONObject().put("title", islandText)),
                        ),
                    )
                    .put("smallIslandArea", JSONObject().put("picInfo", picInfo)),
            )
            .put(
                "actions",
                JSONArray().put(JSONObject().put("type", 0).put("action", ACTION)),
            )
        return Bundle().apply {
            putString("miui.focus.param", JSONObject().put("param_v2", param).toString())
            putBundle("miui.focus.pics", Bundle().apply { putParcelable(PIC, pic) })
            putBundle("miui.focus.actions", Bundle().apply { putParcelable(ACTION, button) })
        }
    }

    /** `SystemProperties.getBoolean`, which the SDK does not expose. */
    private fun systemFlag(name: String): Boolean =
        try {
            Class.forName("android.os.SystemProperties")
                .getMethod("getBoolean", String::class.java, Boolean::class.javaPrimitiveType)
                .invoke(null, name, false) as Boolean
        } catch (e: Exception) {
            false
        }
}
