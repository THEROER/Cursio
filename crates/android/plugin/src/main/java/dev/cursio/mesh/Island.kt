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
    private const val STOP = "miui.focus.action_stop"

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
    fun extras(ctx: Context, title: String, button: Notification.Action): Bundle {
        val pic = JSONObject().put("type", 1).put("pic", PIC)
        val param = JSONObject()
            .put("protocol", 3)
            .put("business", "find_phone")
            .put("ticker", title)
            .put("aodTitle", title)
            .put("updatable", false)
            .put("enableFloat", true)
            .put("islandFirstFloat", true)
            .put(
                "baseInfo",
                JSONObject().put("type", 1).put("title", "Find my phone").put("content", title),
            )
            .put(
                "param_island",
                JSONObject()
                    .put("islandProperty", 1)
                    // Above a hotspot's or a timer's: a ringing phone is the one
                    // thing the person holding it needs to see.
                    .put("islandPriority", 3)
                    .put(
                        "bigIslandArea",
                        JSONObject().put(
                            "imageTextInfoLeft",
                            JSONObject()
                                .put("type", 1)
                                .put("picInfo", pic)
                                .put("textInfo", JSONObject().put("title", "Ringing")),
                        ),
                    )
                    .put("smallIslandArea", JSONObject().put("picInfo", pic)),
            )
            .put(
                "actions",
                JSONArray().put(JSONObject().put("type", 0).put("action", STOP)),
            )
        return Bundle().apply {
            putString("miui.focus.param", JSONObject().put("param_v2", param).toString())
            putBundle(
                "miui.focus.pics",
                Bundle().apply { putParcelable(PIC, Icon.createWithResource(ctx, R.drawable.ic_notify)) },
            )
            putBundle("miui.focus.actions", Bundle().apply { putParcelable(STOP, button) })
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
