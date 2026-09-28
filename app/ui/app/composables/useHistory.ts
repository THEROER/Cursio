import type { UptimeSegment } from "@leavepulse/ui"
import type { NodeStatusReply } from "~/composables/useNode"

/**
 * Short histories the node does not keep, sampled here from what it pushes.
 *
 * The node reports latency as the percentiles of the last window and a peer's
 * presence as it is now; a trend and an uptime bar need the past. Kept by the
 * window since it opened — which is what the labels say — rather than asking
 * the node to hold history for a view that is usually closed.
 */
const LATENCY_POINTS = 40
const PRESENCE_POINTS = 40
/** One presence sample per slice, so a 40-slice bar spans ten minutes. */
const PRESENCE_EVERY_MS = 15_000

const latency = ref<number[]>([])
const presence = ref<Record<string, UptimeSegment[]>>({})
let lastLatencyAt = 0
let timer: ReturnType<typeof setInterval> | null = null
let users = 0

function worstP99(status: NodeStatusReply): number | null {
  if (status.latency.length === 0) return null
  const newest = Math.max(...status.latency.map((l) => l.at.secs_since_epoch))
  if (newest === lastLatencyAt) return null
  lastLatencyAt = newest
  return Math.max(...status.latency.map((l) => l.p99_us)) / 1000
}

function samplePresence(status: NodeStatusReply | null) {
  if (!status) return
  const next: Record<string, UptimeSegment[]> = {}
  for (const peer of status.peers) {
    const past = presence.value[peer.id] ?? []
    const now: UptimeSegment = {
      status:
        peer.presence === "online" ? "operational" : peer.presence === "connecting" ? "degraded" : "down",
    }
    next[peer.id] = [...past, now].slice(-PRESENCE_POINTS)
  }
  presence.value = next
}

export function useHistory() {
  const { status } = useNode()

  watch(status, (s) => {
    if (!s) return
    const p99 = worstP99(s)
    if (p99 !== null) latency.value = [...latency.value, p99].slice(-LATENCY_POINTS)
  })

  onMounted(() => {
    users += 1
    if (users === 1) {
      samplePresence(status.value)
      timer = setInterval(() => samplePresence(status.value), PRESENCE_EVERY_MS)
    }
  })
  onBeforeUnmount(() => {
    users -= 1
    if (users === 0 && timer) {
      clearInterval(timer)
      timer = null
    }
  })

  /** A bar always as wide as the full window, the unsampled part shown empty. */
  function uptime(peerId: string): UptimeSegment[] {
    const seen = presence.value[peerId] ?? []
    const empty: UptimeSegment[] = Array.from({ length: PRESENCE_POINTS - seen.length }, () => ({
      status: "empty",
    }))
    return [...empty, ...seen]
  }

  return { latency, uptime }
}
