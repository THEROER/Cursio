<script setup lang="ts">
import type { Edge, EdgeSpan, Reply } from "~/composables/useNode"

/**
 * Which part of each edge sends the cursor to the machine beyond it
 * (`[input.crossing]`). The rest of the edge stays the desktop's: GNOME shows
 * its dock when the pointer presses the bottom edge, and a peer below took
 * that press for a crossing. Applied live by the node.
 *
 * Offered as halves and thirds rather than two free numbers: a hand aims at
 * "the right third", and a file holding something else still shows, as its
 * own entry, rather than being rounded onto a preset by looking at it.
 */
const { t } = useI18n()
const { config, applyConfigEdit } = useNode()
const toast = useCursioToast()

type Side = "left" | "right" | "top" | "bottom"

const EDGES: { edge: Edge; side: Side; along: "horizontal" | "vertical" }[] = [
  { edge: "Left", side: "left", along: "vertical" },
  { edge: "Right", side: "right", along: "vertical" },
  { edge: "Top", side: "top", along: "horizontal" },
  { edge: "Bottom", side: "bottom", along: "horizontal" },
]

/** Spans by name; `start` is the left or top end of an edge, `end` the other. */
const PRESETS: Record<string, EdgeSpan> = {
  whole: [0, 100],
  startHalf: [0, 50],
  endHalf: [50, 100],
  startThird: [0, 33],
  middleThird: [33, 67],
  endThird: [67, 100],
}

const spans = ref<Partial<Record<Side, EdgeSpan>> | null>(null)
const busy = ref(false)

function presetOf(span: EdgeSpan | undefined): string {
  const [from, to] = span ?? PRESETS.whole!
  const found = Object.entries(PRESETS).find(([, [f, t]]) => f === from && t === to)
  return found ? found[0] : `custom:${from}-${to}`
}

function optionsFor(along: "horizontal" | "vertical", current: string) {
  const ends = along === "horizontal" ? ["Left", "Right"] : ["Top", "Bottom"]
  const options = [
    { value: "whole", label: t("crossing.whole") },
    { value: "startHalf", label: t(`crossing.half${ends[0]}`) },
    { value: "endHalf", label: t(`crossing.half${ends[1]}`) },
    { value: "startThird", label: t(`crossing.third${ends[0]}`) },
    { value: "middleThird", label: t("crossing.thirdMiddle") },
    { value: "endThird", label: t(`crossing.third${ends[1]}`) },
  ]
  if (current.startsWith("custom:")) {
    const [from, to] = current.slice("custom:".length).split("-")
    options.push({ value: current, label: t("crossing.custom", { from: from ?? "", to: to ?? "" }) })
  }
  return options
}

async function load() {
  try {
    const reply = await config()
    if (reply.reply !== "config") return
    const { file } = reply as Extract<Reply, { reply: "config" }>
    spans.value = { ...(file.input?.crossing ?? {}) }
  } catch (e) {
    toast.failure(e, { title: t("crossing.failed") })
  }
}

onMounted(load)

async function pick(edge: Edge, side: Side, value: string) {
  const span = PRESETS[value]
  if (!span) return
  busy.value = true
  try {
    await applyConfigEdit({ crossing: { edge, span: value === "whole" ? null : span } })
    spans.value = { ...spans.value, [side]: value === "whole" ? undefined : span }
    toast.success(t("settings.saved"))
  } catch (e) {
    toast.failure(e, { title: t("crossing.failed"), retry: () => pick(edge, side, value) })
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <LpSkeleton v-if="spans === null" rounded="card" class="h-24" />
  <LpCard v-else padded class="flex flex-col gap-3">
    <div v-for="e in EDGES" :key="e.edge" class="flex flex-wrap items-center gap-4">
      <p class="min-w-0 flex-1 text-sm font-medium">{{ t(`crossing.edge${e.edge}`) }}</p>
      <LpSelect
        :model-value="presetOf(spans[e.side])"
        :options="optionsFor(e.along, presetOf(spans[e.side]))"
        :disabled="busy"
        :aria-label="t(`crossing.edge${e.edge}`)"
        class="w-56"
        @update:model-value="(v: string | string[] | undefined) => typeof v === 'string' && pick(e.edge, e.side, v)"
      />
    </div>
    <p class="text-xs text-muted">{{ t("crossing.hint") }}</p>
  </LpCard>
</template>
