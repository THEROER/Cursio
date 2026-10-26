<script setup lang="ts">
import type { DeskMachine, DeskSeam } from "~/composables/useNode"
import { bounds, snap, type MmRect } from "~/utils/deskSnap"

/**
 * The shared desk drawn to scale, as the system's display settings draw
 * monitors: each machine's screens in millimetres, dragged as one, pulled
 * flush against a neighbour when an edge comes near. Where two machines'
 * screens meet is drawn as a seam the cursor crosses — or, closed, a
 * barrier it does not.
 *
 * The view box is in millimetres, so the browser turns a pointer into desk
 * coordinates itself (`getScreenCTM`). It is held still while a machine is
 * dragged; refitting under the pointer would move the target away from it.
 */
export interface Positioned {
  machine: DeskMachine
  x: number
  y: number
  /** Not yet arranged: drawn where it waits to be dragged in. */
  unplaced: boolean
}

const props = defineProps<{
  machines: Positioned[]
  seams: DeskSeam[]
  selected: string | null
  /** This node's id: only seams it is part of can be closed from here. */
  self: string
  labels: { unplaced: string; open: string; closed: string; elsewhere: string }
}>()

const emit = defineEmits<{
  move: [id: string, x: number, y: number]
  select: [id: string]
  toggleSeam: [seam: DeskSeam]
  menu: [id: string]
}>()

/** How close, in screen pixels, an edge must come to be pulled flush. */
const SNAP_PX = 14

const svg = ref<SVGSVGElement | null>(null)

function rectsAt(m: DeskMachine, x: number, y: number): MmRect[] {
  return m.screens.map((s) => ({ x: x + s.x_mm, y: y + s.y_mm, w: s.w_mm, h: s.h_mm }))
}

const drag = ref<{ id: string; from: DOMPoint; origin: { x: number; y: number }; at: { x: number; y: number } } | null>(null)

/** Where each machine is drawn now: where it is, or where it is being dragged. */
const drawn = computed(() =>
  props.machines.map((p) => {
    const at = drag.value?.id === p.machine.id ? drag.value.at : { x: p.x, y: p.y }
    return { ...p, ...at, rects: rectsAt(p.machine, at.x, at.y) }
  }),
)

const fitted = computed(() => {
  const box = bounds(props.machines.flatMap((p) => rectsAt(p.machine, p.x, p.y))) ?? { x: 0, y: 0, w: 1000, h: 600 }
  const pad = Math.max(box.w, box.h) * 0.12 + 40
  return { x: box.x - pad, y: box.y - pad, w: box.w + pad * 2, h: box.h + pad * 2 }
})
const frozen = ref<typeof fitted.value | null>(null)
const view = computed(() => frozen.value ?? fitted.value)
/** Text and strokes are sized against the desk shown, not fixed millimetres. */
const unit = computed(() => Math.max(view.value.w, view.value.h) / 100)

function toDesk(e: PointerEvent): DOMPoint | null {
  const el = svg.value
  const ctm = el?.getScreenCTM()
  if (!el || !ctm) return null
  return new DOMPoint(e.clientX, e.clientY).matrixTransform(ctm.inverse())
}

function onDown(e: PointerEvent, p: Positioned) {
  if (e.button !== 0) return
  const from = toDesk(e)
  if (!from) return
  emit("select", p.machine.id)
  frozen.value = fitted.value
  ;(e.currentTarget as Element).setPointerCapture(e.pointerId)
  drag.value = { id: p.machine.id, from, origin: { x: p.x, y: p.y }, at: { x: p.x, y: p.y } }
}

function onMove(e: PointerEvent) {
  const d = drag.value
  const p = d && toDesk(e)
  const el = svg.value
  if (!d || !p || !el) return
  const moving = props.machines.find((m) => m.machine.id === d.id)
  if (!moving) return
  const x = d.origin.x + (p.x - d.from.x)
  const y = d.origin.y + (p.y - d.from.y)
  const others = props.machines
    .filter((m) => m.machine.id !== d.id)
    .flatMap((m) => rectsAt(m.machine, m.x, m.y))
  const mmPerPx = view.value.w / Math.max(el.clientWidth, 1)
  const pull = snap(rectsAt(moving.machine, x, y), others, SNAP_PX * mmPerPx)
  d.at = { x: x + pull.dx, y: y + pull.dy }
}

function onUp() {
  const d = drag.value
  drag.value = null
  frozen.value = null
  if (!d) return
  const moved = Math.abs(d.at.x - d.origin.x) > 0.5 || Math.abs(d.at.y - d.origin.y) > 0.5
  const was = props.machines.find((m) => m.machine.id === d.id)
  if (moved || was?.unplaced) emit("move", d.id, d.at.x, d.at.y)
}

function seamTouchesSelf(s: DeskSeam): boolean {
  return s.machine === props.self || s.peer === props.self
}

function seamLabel(s: DeskSeam): string {
  if (!seamTouchesSelf(s)) return props.labels.elsewhere
  return s.closed ? props.labels.closed : props.labels.open
}
</script>

<template>
  <svg
    ref="svg"
    class="h-full w-full touch-none select-none"
    :viewBox="`${view.x} ${view.y} ${view.w} ${view.h}`"
    preserveAspectRatio="xMidYMid meet"
    @pointermove="onMove"
    @pointerup="onUp"
    @pointercancel="onUp"
  >
    <g
      v-for="p in drawn"
      :key="p.machine.id"
      class="cursor-grab"
      :class="{ 'cursor-grabbing': drag?.id === p.machine.id }"
      @pointerdown="onDown($event, p)"
      @contextmenu.prevent="emit('menu', p.machine.id)"
    >
      <rect
        v-for="(r, i) in p.rects"
        :key="p.machine.screens[i]!.id"
        :x="r.x"
        :y="r.y"
        :width="r.w"
        :height="r.h"
        :rx="unit * 0.6"
        :fill="p.machine.this ? 'var(--color-brand-soft)' : 'var(--color-surface-raised)'"
        :stroke="selected === p.machine.id ? 'var(--color-brand)' : 'var(--color-line-strong)'"
        :stroke-width="unit * (selected === p.machine.id ? 0.45 : 0.25)"
        :stroke-dasharray="p.unplaced ? `${unit * 1.5} ${unit}` : undefined"
      />
      <text
        v-for="(r, i) in p.rects"
        :key="`${p.machine.screens[i]!.id}-name`"
        :x="r.x + r.w / 2"
        :y="r.y + r.h / 2 + unit * 2.2"
        text-anchor="middle"
        :font-size="unit * 1.6"
        fill="var(--color-muted)"
        class="pointer-events-none"
      >
        {{ p.machine.screens[i]!.name }}
      </text>
      <text
        :x="p.rects[0]!.x + p.rects[0]!.w / 2"
        :y="p.rects[0]!.y + p.rects[0]!.h / 2"
        text-anchor="middle"
        :font-size="unit * 2.6"
        font-weight="600"
        fill="var(--color-ink)"
        class="pointer-events-none"
      >
        {{ p.machine.name }}
      </text>
      <text
        v-if="p.unplaced"
        :x="p.rects[0]!.x + p.rects[0]!.w / 2"
        :y="p.rects[0]!.y - unit * 1.2"
        text-anchor="middle"
        :font-size="unit * 1.6"
        fill="var(--color-muted)"
        class="pointer-events-none"
      >
        {{ labels.unplaced }}
      </text>
    </g>

    <g v-if="!drag">
      <g v-for="s in seams" :key="`${s.machine}:${s.screen}:${s.peer}:${s.peer_screen}:${s.x1}:${s.y1}`">
        <line
          :x1="s.x1"
          :y1="s.y1"
          :x2="s.x2"
          :y2="s.y2"
          :stroke="s.closed ? 'var(--color-danger)' : 'var(--color-accent)'"
          :stroke-width="unit * 0.9"
          :stroke-dasharray="s.closed ? `${unit * 1.4} ${unit * 1}` : undefined"
          stroke-linecap="round"
          class="pointer-events-none"
        />
        <!-- A wide, invisible line takes the click: the drawn one is thin. -->
        <line
          :x1="s.x1"
          :y1="s.y1"
          :x2="s.x2"
          :y2="s.y2"
          stroke="transparent"
          :stroke-width="unit * 4"
          :class="seamTouchesSelf(s) ? 'cursor-pointer' : 'cursor-default'"
          @click="seamTouchesSelf(s) && emit('toggleSeam', s)"
        >
          <title>{{ seamLabel(s) }}</title>
        </line>
      </g>
    </g>
  </svg>
</template>
