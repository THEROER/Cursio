<script setup lang="ts">
import type { ContextMenuItemDef, TopologyEdge, TopologyLane, TopologyNode } from "@leavepulse/ui"
import type { PeerView, Position } from "~/composables/useNode"
import type { PeerRef } from "~/composables/usePeerMenu"

const { t } = useI18n()
const { status, setPosition } = useNode()
const toast = useCursioToast()
const peerMenu = usePeerMenu()

function peerRef(p: PeerView): PeerRef {
  return { id: p.id, name: p.name, presence: p.presence }
}

/**
 * The machine the canvas was right-clicked on. The canvas names the node
 * before the menu opens, so the menu is resolved per click; this machine's own
 * card and the empty grid have no actions and open nothing.
 */
const menuPeer = ref<PeerRef | null>(null)
function onNodeContextMenu({ id }: { id: string }) {
  const peer = status.value?.peers.find((p) => p.id === id)
  menuPeer.value = peer ? peerRef(peer) : null
}
const canvasMenu = computed<ContextMenuItemDef[]>(() =>
  menuPeer.value ? peerMenu.items(menuPeer.value) : [],
)

/**
 * Where this machine is being moved to, before it is applied. Only this
 * machine's card moves: each node writes its own `position`, and a peer's
 * cell is that peer's to change, from its own app.
 */
const draft = ref<Position | null>(null)
const here = computed<Position | null>(() => draft.value ?? status.value?.node.position ?? null)

/** One grid cell on the canvas, and where a card sits inside it. */
const CELL_W = 260
const CELL_H = 170
const PAD_X = 20
const PAD_Y = 40

interface Placed {
  id: string
  name: string
  at: Position
  self: boolean
  online: boolean
  kind?: string
}

const placed = computed<Placed[]>(() => {
  const s = status.value
  if (!s || !here.value) return []
  const out: Placed[] = [{ id: s.node.id, name: s.node.name, at: here.value, self: true, online: true }]
  for (const p of s.peers) {
    if (p.link) {
      out.push({
        id: p.id,
        name: p.name,
        at: p.link.position,
        self: false,
        online: p.presence === "online",
        kind: t(`kind.${p.link.device_kind}`),
      })
    }
  }
  return out
})

/** Peers whose cell is not known: it rides the greeting, so only while linked. */
const unplaced = computed(() => status.value?.peers.filter((p) => !p.link) ?? [])

/** Every placed machine plus a ring of empty cells to move into. */
const bounds = computed(() => {
  const xs = placed.value.map((m) => m.at.x)
  const ys = placed.value.map((m) => m.at.y)
  if (xs.length === 0) return { x0: -1, x1: 1, y0: -1, y1: 1 }
  return { x0: Math.min(...xs) - 1, x1: Math.max(...xs) + 1, y0: Math.min(...ys) - 1, y1: Math.max(...ys) + 1 }
})

const lanes = computed<TopologyLane[]>(() => {
  const out: TopologyLane[] = []
  for (let y = bounds.value.y0; y <= bounds.value.y1; y++) {
    for (let x = bounds.value.x0; x <= bounds.value.x1; x++) {
      out.push({ id: `${x},${y}`, label: `${x}, ${y}`, x: x * CELL_W, y: y * CELL_H, width: CELL_W - 12, height: CELL_H - 12 })
    }
  }
  return out
})

/**
 * Bumped when a drag must be undone — a peer's card, which is not this
 * machine's to move. The canvas keeps a dragged node where it was dropped
 * until it is handed positions it has not seen, so the nodes are rebuilt.
 */
const revision = ref(0)

const nodes = computed<TopologyNode[]>(() => {
  void revision.value
  return placed.value.map((m) => ({
    id: m.id,
    position: { x: m.at.x * CELL_W + PAD_X, y: m.at.y * CELL_H + PAD_Y },
    data: {
      name: m.name,
      // The kit's own role keys, so this machine and its peers take different
      // colours and glyphs; the printed label is `roleLabel`.
      role: m.self ? "host" : "edge",
      roleLabel: m.self ? t("placement.thisMachine") : (m.kind ?? t("placement.peer")),
      online: m.online,
    },
  }))
})

const edges = computed<TopologyEdge[]>(() => {
  const self = placed.value.find((m) => m.self)
  if (!self) return []
  return placed.value
    .filter((m) => !m.self)
    .map((m) => ({
      id: `${self.id}-${m.id}`,
      source: self.id,
      target: m.id,
      kind: t("placement.link"),
      observed: m.online ? ("applied" as const) : ("pending" as const),
    }))
})

function onDragStop({ id, position }: { id: string; position: { x: number; y: number } }) {
  const self = placed.value.find((m) => m.self)
  if (!self || id !== self.id) {
    revision.value += 1
    toast.info(t("placement.peerNotMine"))
    return
  }
  draft.value = {
    x: Math.round((position.x - PAD_X) / CELL_W),
    y: Math.round((position.y - PAD_Y) / CELL_H),
  }
  revision.value += 1
}

const sharing = computed(() => {
  const me = here.value
  if (!me) return []
  return placed.value.filter((m) => !m.self && m.at.x === me.x && m.at.y === me.y)
})

const changed = computed(() => {
  const s = status.value
  return !!draft.value && !!s && (draft.value.x !== s.node.position.x || draft.value.y !== s.node.position.y)
})

function nudge(dx: number, dy: number) {
  if (here.value) draft.value = { x: here.value.x + dx, y: here.value.y + dy }
}

const applying = ref(false)
async function apply() {
  if (!draft.value) return
  applying.value = true
  try {
    const reply = await setPosition(draft.value)
    if (reply.reply === "saved" && (reply.pinned as string[]).includes("position")) toast.warning(t("placement.pinned"))
    else toast.success(t("placement.applied"))
    draft.value = null
  } catch (e) {
    toast.failure(e, { title: t("placement.failed"), retry: apply })
  } finally {
    applying.value = false
  }
}

const legendLabels = computed(() => ({
  network: t("placement.legendLinked"),
  pending: t("placement.legendPending"),
}))
</script>

<template>
  <DesktopShell>
    <section class="flex flex-col gap-4">
      <LpSection :title="t('placement.title')" :description="t('placement.description')" icon="lucide:layout-grid" />

      <LpSkeleton v-if="!status" rounded="card" class="h-[26rem]" />
      <LpCard v-else class="overflow-hidden">
        <LpContextMenu always :items="canvasMenu">
          <div class="h-[26rem]" @contextmenu.capture="menuPeer = null">
            <LpTopologyCanvas
              :nodes="nodes"
              :edges="edges"
              :lanes="lanes"
              :minimap="false"
              :legend-labels="legendLabels"
              @node-drag-stop="onDragStop"
              @node-contextmenu="onNodeContextMenu"
            />
          </div>
        </LpContextMenu>
        <div class="flex flex-wrap items-center gap-2 border-t border-line p-3">
          <LpTooltip :content="t('placement.left')">
            <LpButton variant="soft" size="sm" square :aria-label="t('placement.left')" @click="nudge(-1, 0)">
              <LpIcon name="lucide:arrow-left" :size="16" />
            </LpButton>
          </LpTooltip>
          <LpTooltip :content="t('placement.up')">
            <LpButton variant="soft" size="sm" square :aria-label="t('placement.up')" @click="nudge(0, -1)">
              <LpIcon name="lucide:arrow-up" :size="16" />
            </LpButton>
          </LpTooltip>
          <LpTooltip :content="t('placement.down')">
            <LpButton variant="soft" size="sm" square :aria-label="t('placement.down')" @click="nudge(0, 1)">
              <LpIcon name="lucide:arrow-down" :size="16" />
            </LpButton>
          </LpTooltip>
          <LpTooltip :content="t('placement.right')">
            <LpButton variant="soft" size="sm" square :aria-label="t('placement.right')" @click="nudge(1, 0)">
              <LpIcon name="lucide:arrow-right" :size="16" />
            </LpButton>
          </LpTooltip>
          <span class="text-xs text-muted">{{ t("placement.here", { x: here?.x, y: here?.y }) }}</span>
          <div class="ml-auto flex gap-2">
            <LpButton v-if="changed" variant="ghost" size="sm" @click="draft = null">{{ t("placement.reset") }}</LpButton>
            <LpButton variant="solid" size="sm" :disabled="!changed || applying" @click="apply">
              {{ t("placement.apply") }}
            </LpButton>
          </div>
        </div>
      </LpCard>

      <LpAlert v-if="sharing.length" variant="warning">
        {{ t("placement.sameCell", { names: sharing.map((s) => s.name).join(", ") }) }}
      </LpAlert>
      <div v-if="unplaced.length" class="flex flex-col gap-2">
        <p class="text-xs text-muted">{{ t("placement.unplacedTitle") }}</p>
        <div class="flex flex-wrap gap-2">
          <PeerMenu v-for="p in unplaced" :key="p.id" :peer="peerRef(p)">
            <div class="flex items-center gap-1 rounded-control border border-line py-0.5 pl-3 pr-1 text-sm">
              <span>{{ p.name }}</span>
              <PeerMenuButton :peer="peerRef(p)" />
            </div>
          </PeerMenu>
        </div>
      </div>
    </section>
  </DesktopShell>
</template>
