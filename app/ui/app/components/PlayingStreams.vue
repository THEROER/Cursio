<script setup lang="ts">
import { MAX_STREAM_VOLUME, type PlayingStream, type StreamKind } from "~/composables/useNode"
import { gainFromSlider, sliderFromGain } from "~/utils/loudness"

/**
 * `sound` is what plays out of this machine; `microphones` are peers'
 * microphones published here as inputs, whose slider is how loud each comes
 * in. Apart because a microphone row among an app's sound read as something
 * playing that nothing could be heard from.
 */
const props = withDefaults(defineProps<{ show?: "sound" | "microphones" }>(), { show: "sound" })

/**
 * Peers' streams playing on this machine, each with the listener's own volume
 * and mute. This is the listener's say only: the sender's level and every
 * other machine hearing the same stream are untouched.
 *
 * What the slider shows while a change is in flight is the hand's value, not
 * the node's. The node pushes its status on every change, and a push carrying
 * the value from before the drag arrives mid-drag — drawing it would yank the
 * thumb back under the finger. A draft is dropped once the node reports the
 * same value back, or after `SETTLE_MS` if it never does.
 */
const { t } = useI18n()
const { status, setStreamVolume } = useNode()
const { isMobile } = usePlatform()
const toast = useCursioToast()

/** The thumb's travel, in percent of loudness (`utils/loudness.ts`), not of gain. */
const MAX_PERCENT = Math.floor(sliderFromGain(MAX_STREAM_VOLUME))
/** Sends while dragging at most this often; the release always sends at once. */
const DRAG_SEND_MS = 120
const SETTLE_MS = 3000

const streams = computed<PlayingStream[]>(() =>
  (status.value?.streams ?? []).filter((s) => (s.kind === "microphone") === (props.show === "microphones")),
)

const ICONS: Record<StreamKind, string> = {
  app: "lucide:app-window",
  microphone: "lucide:mic",
  device: "lucide:speaker",
  call: "lucide:phone",
  phone_sound: "lucide:smartphone",
}

function iconOf(s: PlayingStream): string {
  return ICONS[s.kind ?? "app"] ?? "lucide:audio-lines"
}

const draftVolume = ref<Record<number, number>>({})
const draftMuted = ref<Record<number, boolean>>({})
const sendTimers = new Map<number, ReturnType<typeof setTimeout>>()
const settleTimers = new Map<number, ReturnType<typeof setTimeout>>()

function percentOf(s: PlayingStream): number {
  return draftVolume.value[s.id] ?? Math.round(sliderFromGain(s.volume))
}

function mutedOf(s: PlayingStream): boolean {
  return draftMuted.value[s.id] ?? s.muted
}

function dropDraft(id: number) {
  const { [id]: _v, ...volumes } = draftVolume.value
  const { [id]: _m, ...mutes } = draftMuted.value
  draftVolume.value = volumes
  draftMuted.value = mutes
  clearTimeout(sendTimers.get(id))
  clearTimeout(settleTimers.get(id))
  sendTimers.delete(id)
  settleTimers.delete(id)
}

async function send(s: PlayingStream) {
  sendTimers.delete(s.id)
  const volume = gainFromSlider(percentOf(s))
  const muted = mutedOf(s)
  try {
    await setStreamVolume(s.id, volume, muted)
  } catch (e) {
    dropDraft(s.id)
    toast.failure(e, { title: t("streams.failed", { name: s.name }) })
  }
}

function settleLater(id: number) {
  clearTimeout(settleTimers.get(id))
  settleTimers.set(
    id,
    setTimeout(() => dropDraft(id), SETTLE_MS),
  )
}

function onDrag(s: PlayingStream, percent: number) {
  draftVolume.value = { ...draftVolume.value, [s.id]: percent }
  settleLater(s.id)
  if (sendTimers.has(s.id)) return
  sendTimers.set(
    s.id,
    setTimeout(() => void send(s), DRAG_SEND_MS),
  )
}

function onRelease(s: PlayingStream, percent: number) {
  draftVolume.value = { ...draftVolume.value, [s.id]: percent }
  clearTimeout(sendTimers.get(s.id))
  settleLater(s.id)
  void send(s)
}

function toggleMute(s: PlayingStream) {
  draftMuted.value = { ...draftMuted.value, [s.id]: !mutedOf(s) }
  settleLater(s.id)
  void send(s)
}

watch(streams, (now) => {
  const live = new Map(now.map((s) => [s.id, s]))
  for (const key of new Set([...Object.keys(draftVolume.value), ...Object.keys(draftMuted.value)])) {
    const id = Number(key)
    const s = live.get(id)
    if (!s) {
      dropDraft(id)
      continue
    }
    if (sendTimers.has(id)) continue
    const volume = draftVolume.value[id]
    const muted = draftMuted.value[id]
    const volumeArrived = volume === undefined || Math.abs(sliderFromGain(s.volume) - volume) < 0.5
    const muteArrived = muted === undefined || s.muted === muted
    if (volumeArrived && muteArrived) dropDraft(id)
  }
})

onBeforeUnmount(() => {
  for (const timer of [...sendTimers.values(), ...settleTimers.values()]) clearTimeout(timer)
})
</script>

<template>
  <LpEmptyState
    v-if="streams.length === 0"
    compact
    :icon="show === 'microphones' ? 'lucide:mic-off' : 'lucide:volume-off'"
    :title="show === 'microphones' ? t('streams.noMicrophones') : t('streams.none')"
    :description="show === 'microphones' ? t('streams.noMicrophonesHint') : t('streams.noneHint')"
  />
  <ul v-else class="flex flex-col divide-y divide-line">
    <li v-for="s in streams" :key="s.id" class="flex flex-col gap-2 py-3 first:pt-0 last:pb-0">
      <div class="flex items-center gap-3">
        <LpIcon :name="iconOf(s)" :size="18" class="shrink-0 text-muted" />
        <div class="min-w-0 flex-1">
          <p class="truncate text-sm font-medium">{{ s.name }}</p>
          <p class="truncate text-xs text-muted">{{ t("streams.from", { peer: s.peer }) }}</p>
        </div>
        <LpTooltip :content="mutedOf(s) ? t('streams.unmute') : t('streams.mute')">
          <LpButton
            :variant="mutedOf(s) ? 'soft' : 'ghost'"
            :size="isMobile ? 'md' : 'sm'"
            square
            :aria-label="mutedOf(s) ? t('streams.unmute') : t('streams.mute')"
            :aria-pressed="mutedOf(s)"
            @click="toggleMute(s)"
          >
            <LpIcon :name="mutedOf(s) ? 'lucide:volume-x' : 'lucide:volume-2'" :size="16" />
          </LpButton>
        </LpTooltip>
      </div>
      <div class="flex items-center gap-4" :class="{ 'opacity-60': mutedOf(s) }">
        <LpSlider
          :model-value="percentOf(s)"
          :min="0"
          :max="MAX_PERCENT"
          :step="1"
          :aria-label="t('streams.volume', { name: s.name })"
          class="flex-1"
          :class="isMobile ? 'py-3' : ''"
          @update:model-value="(v: number) => onDrag(s, v)"
          @change="(v: number) => onRelease(s, v)"
        />
        <span class="w-12 shrink-0 text-right font-mono text-xs tabular-nums">{{ percentOf(s) }}%</span>
      </div>
    </li>
  </ul>
</template>
