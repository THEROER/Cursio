package dev.cursio.mesh

import android.app.Notification
import android.content.Context
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
     * The extras that put a computer's player in the island: its cover (or
     * the logo), the track, and play/pause. HyperOS draws its own music
     * island only for an app playing sound on the phone itself, which a
     * remote control is not — so without these a computer's player sat in
     * the shade and never reached the island.
     */
    fun mediaExtras(
        ctx: Context,
        title: String,
        artist: String,
        source: String,
        cover: Icon?,
        button: Notification.Action,
    ): Bundle =
        focus(
            business = "music",
            heading = title,
            title = title,
            content = listOf(artist, source).filter { it.isNotEmpty() }.joinToString(" · "),
            islandText = title,
            // Below a ringing phone, a call or a timer.
            priority = 1,
            updatable = true,
            pic = cover ?: Icon.createWithResource(ctx, R.drawable.ic_notify),
            button = button,
        )

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
