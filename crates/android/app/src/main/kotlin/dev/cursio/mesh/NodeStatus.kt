package dev.cursio.mesh

import org.json.JSONArray
import org.json.JSONObject

/**
 * The node as `nativeStatusJson` describes it.
 *
 * Parsed leniently: every field has a default, because the Rust side and this
 * app are built separately and one gaining a field before the other must not
 * turn the whole screen blank.
 */
data class NodeStatus(
    val running: Boolean,
    val name: String?,
    val peerId: String?,
    val bound: String?,
    val peers: List<Peer>,
    val pairing: Pairing?,
    val lastPairing: LastPairing?,
    val micGate: String?,
    val streams: List<Stream>,
) {
    data class Peer(val id: String, val name: String, val presence: String, val modules: List<String>)

    data class Pairing(val kind: String, val code: String?, val expiresInS: Int?)

    data class LastPairing(val outcome: String, val peer: String?)

    data class Stream(
        val id: Int,
        val peer: String,
        val name: String,
        val incoming: Boolean,
        val volume: Float,
        val muted: Boolean,
        val playing: Boolean,
    )

    companion object {
        val STOPPED = NodeStatus(false, null, null, null, emptyList(), null, null, null, emptyList())

        fun parse(json: String): NodeStatus {
            val o = JSONObject(json)
            val audio = o.optJSONObject("audio")
            return NodeStatus(
                running = o.optBoolean("running", false),
                name = o.str("name"),
                peerId = o.str("peer_id"),
                bound = o.str("bound"),
                peers = o.optJSONArray("peers").objects().map {
                    Peer(
                        id = it.str("id") ?: "",
                        name = it.str("name") ?: it.str("id") ?: "?",
                        presence = it.str("presence") ?: "offline",
                        modules = it.optJSONArray("modules").strings(),
                    )
                },
                pairing = o.optJSONObject("pairing")?.let {
                    Pairing(
                        kind = it.str("kind") ?: "",
                        code = it.str("code"),
                        expiresInS = if (it.has("expires_in_s") && !it.isNull("expires_in_s")) {
                            it.optInt("expires_in_s")
                        } else {
                            null
                        },
                    )
                },
                lastPairing = o.optJSONObject("last_pairing")?.let {
                    LastPairing(outcome = it.str("outcome") ?: "", peer = it.str("peer"))
                },
                micGate = audio?.str("mic_gate"),
                streams = audio?.optJSONArray("streams").objects().map {
                    Stream(
                        id = it.optInt("id"),
                        peer = it.str("peer") ?: "",
                        name = it.str("name") ?: "",
                        incoming = it.str("direction") != "out",
                        volume = it.optDouble("volume", 1.0).toFloat(),
                        muted = it.optBoolean("muted", false),
                        playing = it.optBoolean("playing", false),
                    )
                },
            )
        }

        /** `optString` turns JSON null into the string "null"; this does not. */
        private fun JSONObject.str(key: String): String? =
            if (!has(key) || isNull(key)) null else optString(key).takeIf { it.isNotEmpty() }

        private fun JSONArray?.objects(): List<JSONObject> =
            if (this == null) emptyList() else (0 until length()).mapNotNull { optJSONObject(it) }

        private fun JSONArray?.strings(): List<String> =
            if (this == null) emptyList() else (0 until length()).mapNotNull { i ->
                optString(i).takeIf { it.isNotEmpty() && !isNull(i) }
            }
    }
}
