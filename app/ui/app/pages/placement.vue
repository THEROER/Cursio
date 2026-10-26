<script setup lang="ts">
import type { ContextMenuItemDef } from "@leavepulse/ui"
import type { DeskMachine, DeskSeam, PeerView } from "~/composables/useNode"
import type { PeerRef } from "~/composables/usePeerMenu"
import type { Positioned } from "~/components/DeskArrangement.vue"
import { bounds } from "~/utils/deskSnap"

/**
 * The shared desk: every connected machine's screens at their real size,
 * dragged into the places they stand in. The cursor crosses only where two
 * machines' screens meet here, so a laptop under the right-hand monitor is
 * reached from that monitor and not the left one; clicking a seam closes it.
 *
 * Each machine writes its own place. Dropping another machine asks it over
 * its link, and the move shows once it has taken it — the status then
 * reports it from that machine itself.
 */
const { t } = useI18n()
const { status, arrange, setBarrier } = useNode()
const toast = useCursioToast()
const peerMenu = usePeerMenu()

const desk = computed(() => status.value?.desk ?? null)
const self = computed(() => status.value?.node.id ?? "")

/** Where a machine was dropped, until the status reports it standing there. */
const pending = ref<Record<string, { x: number; y: number }>>({})

watch(desk, (d) => {
  if (!d) return
  const left = { ...pending.value }
  for (const m of d.machines) {
    const want = left[m.id]
    if (want && m.place && Math.abs(m.place.x_mm - want.x) < 1 && Math.abs(m.place.y_mm - want.y) < 1) {
      delete left[m.id]
    }
  }
  pending.value = left
})

function rects(m: DeskMachine, x: number, y: number) {
  return m.screens.map((s) => ({ x: x + s.x_mm, y: y + s.y_mm, w: s.w_mm, h: s.h_mm }))
}

/**
 * Every machine where it is drawn. One not yet arranged waits to the right
 * of those that are, so it can be seen and dragged in; with none arranged,
 * this machine stands at the origin.
 */
const positioned = computed<Positioned[]>(() => {
  const machines = desk.value?.machines ?? []
  const out: Positioned[] = []
  for (const m of machines) {
    const at = pending.value[m.id] ?? (m.place ? { x: m.place.x_mm, y: m.place.y_mm } : null)
    if (at) out.push({ machine: m, x: at.x, y: at.y, unplaced: false })
  }
  for (const m of machines) {
    if (out.some((p) => p.machine.id === m.id)) continue
    const box = bounds(out.flatMap((p) => rects(p.machine, p.x, p.y)))
    out.push({ machine: m, x: box ? box.x + box.w + 120 : 0, y: box ? box.y : 0, unplaced: true })
  }
  return out
})

const selected = ref<string | null>(null)
const selectedMachine = computed(
  () => positioned.value.find((p) => p.machine.id === selected.value) ?? positioned.value.find((p) => p.machine.this),
)

async function onMove(id: string, x: number, y: number) {
  pending.value = { ...pending.value, [id]: { x, y } }
  const machine = desk.value?.machines.find((m) => m.id === id)
  try {
    await arrange(machine?.this ? null : id, x, y)
    // The cursor crosses only between machines both on the desk, so the
    // first machine dragged in brings this one with it where it is drawn.
    const me = positioned.value.find((p) => p.machine.this)
    if (me?.unplaced && !machine?.this) {
      pending.value = { ...pending.value, [me.machine.id]: { x: me.x, y: me.y } }
      await arrange(null, me.x, me.y)
    }
  } catch (e) {
    const left = { ...pending.value }
    delete left[id]
    pending.value = left
    toast.failure(e, { title: t("placement.failed") })
  }
}

async function onToggleSeam(s: DeskSeam) {
  // Always told from this machine's side: its screen, the peer's screen.
  const mine = s.machine === self.value
  const screen = mine ? s.screen : s.peer_screen
  const peer = mine ? s.peer : s.machine
  const peerScreen = mine ? s.peer_screen : s.screen
  try {
    await setBarrier(screen, peer, peerScreen, !s.closed)
    toast.success(s.closed ? t("placement.seamOpened") : t("placement.seamClosed"))
  } catch (e) {
    toast.failure(e, { title: t("placement.seamFailed") })
  }
}

function peerRef(p: PeerView): PeerRef {
  return peerRefOf(p)
}

/** The machine the desk was right-clicked on; this machine has no menu. */
const menuPeer = ref<PeerRef | null>(null)
function onMenu(id: string) {
  const peer = status.value?.peers.find((p) => p.id === id)
  menuPeer.value = peer ? peerRef(peer) : null
}
const canvasMenu = computed<ContextMenuItemDef[]>(() => (menuPeer.value ? peerMenu.items(menuPeer.value) : []))

/** Peers with no screen on the desk: offline ones, and phones. */
const offDesk = computed(() =>
  (status.value?.peers ?? []).filter((p) => !desk.value?.machines.some((m) => m.id === p.id)),
)

const seamCounts = computed(() => {
  const seams = desk.value?.seams ?? []
  return { open: seams.filter((s) => !s.closed).length, closed: seams.filter((s) => s.closed).length }
})

/** A panel's diagonal in inches, as monitors are sold. */
function inches(w: number, h: number): string {
  return (Math.hypot(w, h) / 25.4).toFixed(1)
}

const labels = computed(() => ({
  unplaced: t("placement.unplaced"),
  open: t("placement.seamOpen"),
  closed: t("placement.seamClosedHint"),
  elsewhere: t("placement.seamElsewhere"),
}))
</script>

<template>
  <DesktopShell>
    <section class="flex flex-col gap-4">
      <LpSection :title="t('placement.title')" :description="t('placement.description')" icon="lucide:monitor" />

      <LpSkeleton v-if="!status" rounded="card" class="h-[28rem]" />
      <LpAlert v-else-if="!desk || !desk.machines.length" variant="info">
        {{ t("placement.noScreens") }}
      </LpAlert>
      <div v-else class="grid gap-4 lg:grid-cols-[1fr_18rem]">
        <LpCard class="overflow-hidden">
          <LpContextMenu always :items="canvasMenu">
            <div class="h-[28rem] bg-surface-soft" @contextmenu.capture="menuPeer = null">
              <DeskArrangement
                :machines="positioned"
                :seams="desk.seams"
                :selected="selectedMachine?.machine.id ?? null"
                :self="self"
                :labels="labels"
                @move="onMove"
                @select="(id) => (selected = id)"
                @toggle-seam="onToggleSeam"
                @menu="onMenu"
              />
            </div>
          </LpContextMenu>
          <div class="flex flex-wrap items-center gap-x-4 gap-y-1 border-t border-line px-4 py-2 text-xs text-muted">
            <span class="flex items-center gap-1.5">
              <span class="inline-block h-0.5 w-5 rounded bg-accent" />
              {{ t("placement.legendOpen", { n: seamCounts.open }) }}
            </span>
            <span class="flex items-center gap-1.5">
              <span class="inline-block h-0.5 w-5 rounded border-t-2 border-dashed border-danger" />
              {{ t("placement.legendClosed", { n: seamCounts.closed }) }}
            </span>
            <span class="ml-auto">{{ t("placement.hint") }}</span>
          </div>
        </LpCard>

        <LpCard v-if="selectedMachine" class="flex flex-col gap-3 p-4">
          <div class="flex items-center gap-2">
            <LpIcon :name="selectedMachine.machine.this ? 'lucide:monitor-check' : 'lucide:monitor'" :size="18" />
            <span class="font-semibold">{{ selectedMachine.machine.name }}</span>
            <LpBadge v-if="selectedMachine.machine.this" tone="brand">{{ t("placement.thisMachine") }}</LpBadge>
            <LpBadge v-else-if="selectedMachine.unplaced" tone="warning">{{ t("placement.unplaced") }}</LpBadge>
          </div>
          <ul class="flex flex-col gap-2">
            <li
              v-for="s in selectedMachine.machine.screens"
              :key="s.id"
              class="rounded-control border border-line px-3 py-2 text-sm"
            >
              <div class="truncate font-medium">{{ s.name }}</div>
              <div class="text-xs text-muted">
                {{ t("placement.screenSize", { inches: inches(s.w_mm, s.h_mm), w: Math.round(s.w_mm), h: Math.round(s.h_mm) }) }}
                · {{ s.width }}×{{ s.height }}
              </div>
              <div v-if="!s.measured" class="mt-1 text-xs text-muted">{{ t("placement.estimated") }}</div>
            </li>
          </ul>
          <p v-if="!selectedMachine.machine.this" class="text-xs text-muted">{{ t("placement.peerMoves") }}</p>
        </LpCard>
      </div>

      <div v-if="offDesk.length" class="flex flex-col gap-2">
        <p class="text-xs text-muted">{{ t("placement.offDeskTitle") }}</p>
        <div class="flex flex-wrap gap-2">
          <PeerMenu v-for="p in offDesk" :key="p.id" :peer="peerRef(p)">
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
