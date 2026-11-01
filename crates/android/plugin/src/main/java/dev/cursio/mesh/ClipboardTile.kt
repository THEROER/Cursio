package dev.cursio.mesh

import android.annotation.SuppressLint
import android.app.PendingIntent
import android.os.Build
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import org.json.JSONObject

/**
 * "Send clipboard" in Quick Settings. The tile cannot read the clipboard
 * itself — a tile is a service, and Android reads the clipboard only to a
 * focused window — so a tap opens [ClipboardSendActivity] over whatever the
 * user is in, which does.
 *
 * While the shade is open the tile names the computer a tap would reach, so
 * the user sees where it goes before it goes.
 */
class ClipboardTile : TileService() {
    override fun onStartListening() {
        super.onStartListening()
        val tile = qsTile ?: return
        val where = if (Node.init(this) && Node.isRunning()) {
            JSONObject(CursioNative.nativeClipboardDestination())
        } else {
            JSONObject().put("nowhere", "Cursio is not running")
        }
        tile.label = "Send clipboard"
        when {
            where.has("to") -> {
                tile.state = Tile.STATE_INACTIVE
                setSubtitle(tile, "to ${where.getJSONArray("to").getString(1)}")
            }
            where.has("pick") -> {
                tile.state = Tile.STATE_INACTIVE
                setSubtitle(tile, "choose a computer")
            }
            else -> {
                tile.state = Tile.STATE_UNAVAILABLE
                setSubtitle(tile, "no computer")
            }
        }
        tile.updateTile()
    }

    private fun setSubtitle(tile: Tile, text: String) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) tile.subtitle = text
    }

    @SuppressLint("StartActivityAndCollapseDeprecated")
    override fun onClick() {
        super.onClick()
        val intent = ClipboardSendActivity.intent(this)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startActivityAndCollapse(
                PendingIntent.getActivity(this, 0, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT),
            )
        } else {
            @Suppress("DEPRECATION")
            startActivityAndCollapse(intent)
        }
    }
}
