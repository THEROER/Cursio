<script setup lang="ts">
import { LpAlert, LpButton, LpCard, LpIcon, LpSection } from "@leavepulse/ui"
import type { Position } from "~/composables/useNode"

const { t } = useI18n()
const { status, setPosition } = useNode()

/**
 * Where this machine is being moved to, before it is applied. Only this
 * machine's tile moves: each node writes its own `position`, and a peer's
 * cell is that peer's to change, from its own app.
 */
const draft = ref<Position | null>(null)
const here = computed<Position | null>(() => draft.value ?? status.value?.node.position ?? null)

interface Tile {
  key: string
  name: string
  at: Position
  self: boolean
}

const placed = computed<Tile[]>(() => {
  const s = status.value
  if (!s || !here.value) return []
  const tiles: Tile[] = [{ key: s.node.id, name: s.node.name, at: here.value, self: true }]
  for (const p of s.peers) {
    if (p.link) tiles.push({ key: p.id, name: p.name, at: p.link.position, self: false })
  }
  return tiles
})

/** Peers whose cell is not known: it rides the greeting, so only while linked. */
const unplaced = computed(() => status.value?.peers.filter((p) => !p.link) ?? [])

/** The grid shown: every placed tile, plus a ring of empty cells to move into. */
const bounds = computed(() => {
  const xs = placed.value.map((t) => t.at.x)
  const ys = placed.value.map((t) => t.at.y)
  if (xs.length === 0) return { x0: -1, x1: 1, y0: -1, y1: 1 }
  return {
    x0: Math.min(...xs) - 1,
    x1: Math.max(...xs) + 1,
    y0: Math.min(...ys) - 1,
    y1: Math.max(...ys) + 1,
  }
})

const cells = computed(() => {
  const out: Position[] = []
  for (let y = bounds.value.y0; y <= bounds.value.y1; y++)
    for (let x = bounds.value.x0; x <= bounds.value.x1; x++) out.push({ x, y })
  return out
})

function tilesAt(p: Position) {
  return placed.value.filter((t) => t.at.x === p.x && t.at.y === p.y)
}

const sharing = computed(() => {
  const me = here.value
  if (!me) return []
  return placed.value.filter((t) => !t.self && t.at.x === me.x && t.at.y === me.y)
})

const changed = computed(() => {
  const s = status.value
  return !!draft.value && !!s && (draft.value.x !== s.node.position.x || draft.value.y !== s.node.position.y)
})

function moveTo(p: Position) {
  draft.value = { x: p.x, y: p.y }
}

function nudge(dx: number, dy: number) {
  if (here.value) moveTo({ x: here.value.x + dx, y: here.value.y + dy })
}

function onDragStart(event: DragEvent, tile: Tile) {
  if (!tile.self || !event.dataTransfer) return
  event.dataTransfer.effectAllowed = "move"
  event.dataTransfer.setData("text/plain", "cursio-self")
}

function onDrop(event: DragEvent, p: Position) {
  if (event.dataTransfer?.getData("text/plain") === "cursio-self") moveTo(p)
}

const applying = ref(false)
const message = ref("")
async function apply() {
  if (!draft.value) return
  applying.value = true
  message.value = ""
  try {
    const reply = await setPosition(draft.value)
    message.value =
      reply.reply === "saved" && (reply.pinned as string[]).includes("position")
        ? t("placement.pinned")
        : t("placement.applied")
    draft.value = null
  } catch (e) {
    message.value = (e as { message?: string }).message ?? String(e)
  } finally {
    applying.value = false
  }
}
</script>

<template>
  <DesktopShell>
    <section v-if="status" class="flex flex-col gap-3">
      <LpSection
        :title="t('placement.title')"
        :description="t('placement.description')"
        icon="lucide:layout-grid"
      />
      <LpCard padded>
        <div
          class="grid gap-2"
          :style="{ gridTemplateColumns: `repeat(${bounds.x1 - bounds.x0 + 1}, minmax(0, 7rem))` }"
        >
          <div
            v-for="cell in cells"
            :key="`${cell.x},${cell.y}`"
            class="flex aspect-[4/3] flex-col items-stretch justify-center gap-1 rounded-control border border-dashed border-line p-1"
            @dragover.prevent
            @drop.prevent="onDrop($event, cell)"
            @dblclick="moveTo(cell)"
          >
            <div
              v-for="tile in tilesAt(cell)"
              :key="tile.key"
              :draggable="tile.self"
              class="flex min-w-0 items-center gap-1 rounded-control px-2 py-1 text-xs"
              :class="tile.self ? 'cursor-grab bg-brand text-ink-inverse' : 'bg-surface-soft text-ink'"
              @dragstart="onDragStart($event, tile)"
            >
              <LpIcon :name="tile.self ? 'lucide:monitor' : 'lucide:monitor-smartphone'" :size="14" />
              <span class="truncate">{{ tile.name }}</span>
            </div>
            <span v-if="tilesAt(cell).length === 0" class="text-center text-[10px] text-muted">
              {{ cell.x }},{{ cell.y }}
            </span>
          </div>
        </div>

        <div class="mt-4 flex flex-wrap items-center gap-2">
          <LpButton variant="soft" size="sm" square :aria-label="t('placement.left')" @click="nudge(-1, 0)">
            <LpIcon name="lucide:arrow-left" :size="16" />
          </LpButton>
          <LpButton variant="soft" size="sm" square :aria-label="t('placement.up')" @click="nudge(0, -1)">
            <LpIcon name="lucide:arrow-up" :size="16" />
          </LpButton>
          <LpButton variant="soft" size="sm" square :aria-label="t('placement.down')" @click="nudge(0, 1)">
            <LpIcon name="lucide:arrow-down" :size="16" />
          </LpButton>
          <LpButton variant="soft" size="sm" square :aria-label="t('placement.right')" @click="nudge(1, 0)">
            <LpIcon name="lucide:arrow-right" :size="16" />
          </LpButton>
          <span class="text-xs text-muted">{{ t("placement.here", { x: here?.x, y: here?.y }) }}</span>
          <div class="ml-auto flex gap-2">
            <LpButton v-if="changed" variant="ghost" size="sm" @click="draft = null">{{ t("placement.reset") }}</LpButton>
            <LpButton variant="solid" size="sm" :disabled="!changed || applying" @click="apply">
              {{ t("placement.apply") }}
            </LpButton>
          </div>
        </div>
        <p v-if="message" class="mt-3 text-xs text-muted">{{ message }}</p>
      </LpCard>

      <LpAlert v-if="sharing.length" variant="warning" class="mt-4">
        {{ t("placement.sameCell", { names: sharing.map((s) => s.name).join(", ") }) }}
      </LpAlert>
      <p v-if="unplaced.length" class="mt-4 text-xs text-muted">
        {{ t("placement.unplaced", { names: unplaced.map((p) => p.name).join(", ") }) }}
      </p>
    </section>
  </DesktopShell>
</template>
