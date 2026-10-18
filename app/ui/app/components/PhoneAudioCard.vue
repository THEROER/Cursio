<script setup lang="ts">
/**
 * Where the phone's own sound plays: one of the peers' output devices, or
 * nowhere but the phone. Phone only — the commands exist only there.
 */
import type { PhoneAudioState, PhoneAudioTarget } from "~/composables/usePhone"

const OFF = "off"

const { t } = useI18n()
const { status } = useNode()
const toast = useCursioToast()

/** `null` until the first answer, so the card does not flash "no devices". */
const targets = ref<PhoneAudioTarget[] | null>(null)
const state = ref<PhoneAudioState | null>(null)
const busy = ref(false)

/**
 * Whether this window already asked to resume the device chosen before a
 * restart. Android's consent cannot outlive the process, so the choice comes
 * back as one prompt — once, so declining it is not answered with another.
 */
let resumeAsked = false

async function refresh() {
  try {
    ;[targets.value, state.value] = await Promise.all([phoneAudioTargets(), phoneAudioState()])
  } catch (e) {
    toast.failure(e, { title: t("phoneAudio.listFailed") })
    return
  }
  const resume = state.value?.resume
  if (resume != null && !resumeAsked && !busy.value) {
    resumeAsked = true
    await pick(String(resume))
  }
}

const percent = computed(() => Math.round((state.value?.volume ?? 1) * 100))

/** Applied while dragging: the point is to hear the level change, not guess it. */
async function setVolume(value: number) {
  if (state.value) state.value.volume = value / 100
  try {
    await phoneAudioVolume(value / 100)
  } catch (e) {
    toast.failure(e, { title: t("phoneAudio.volumeFailed") })
  }
}

const active = computed(() => targets.value?.find((target) => target.routed) ?? null)
const options = computed(() => [
  { value: OFF, label: t("phoneAudio.off") },
  ...(targets.value ?? []).map((target) => ({ value: String(target.key), label: target.name })),
])

async function pick(value: string | string[] | undefined) {
  if (typeof value !== "string") return
  const target = value === OFF ? null : Number(value)
  // Choosing "off" while a pre-restart device is remembered is how the user
  // forgets it, so it still goes through.
  if (target === (active.value?.key ?? null) && !(target === null && state.value?.remembered)) return
  busy.value = true
  try {
    await phoneAudioRoute(target)
    toast.success(target === null ? t("phoneAudio.stopped") : t("phoneAudio.routed"))
  } catch (e) {
    toast.failure(e, { title: t("phoneAudio.routeFailed") })
  } finally {
    busy.value = false
    await refresh()
  }
}

// Devices are the peers' and come and go with their links, which is what the
// node pushes its status on; returning from Android's consent screen or from
// another app is the other moment the list may have moved.
watch(
  () => status.value?.peers.map((p) => `${p.id}:${p.presence}`).join(),
  () => void refresh(),
)

function onReturn() {
  if (document.visibilityState === "visible") void refresh()
}

onMounted(() => {
  void refresh()
  window.addEventListener("focus", onReturn)
  document.addEventListener("visibilitychange", onReturn)
})
onBeforeUnmount(() => {
  window.removeEventListener("focus", onReturn)
  document.removeEventListener("visibilitychange", onReturn)
})
</script>

<template>
  <LpSkeleton v-if="!targets" rounded="card" class="h-24" />
  <LpCard v-else padded class="flex flex-col gap-3">
    <LpFormField :label="t('phoneAudio.device')">
      <LpSelect
        :model-value="active ? String(active.key) : OFF"
        :options="options"
        :disabled="busy"
        @update:model-value="pick"
      />
    </LpFormField>
    <p v-if="active" class="text-xs">{{ t("phoneAudio.playingOn", { device: active.name }) }}</p>
    <LpAlert v-else-if="state?.awaited" variant="warning">
      {{ t("phoneAudio.awaited", { device: state.awaited }) }}
    </LpAlert>
    <LpAlert v-else-if="state?.remembered" variant="warning">
      <div class="flex flex-col gap-2">
        <span>{{ t("phoneAudio.remembered", { device: state.remembered }) }}</span>
        <LpButton
          v-if="state.resume != null"
          variant="soft"
          size="sm"
          :disabled="busy"
          @click="pick(String(state.resume))"
        >
          {{ t("phoneAudio.resume") }}
        </LpButton>
      </div>
    </LpAlert>
    <p v-else-if="targets.length === 0" class="text-xs text-muted">{{ t("phoneAudio.none") }}</p>
    <LpFormField :label="t('phoneAudio.volume')">
      <div class="flex items-center gap-4">
        <LpSlider
          :model-value="percent"
          :min="0"
          :max="100"
          :step="1"
          :aria-label="t('phoneAudio.volume')"
          class="flex-1 py-3"
          @update:model-value="setVolume"
        />
        <span class="w-12 shrink-0 text-right font-mono text-xs tabular-nums">{{ percent }}%</span>
      </div>
    </LpFormField>
    <p class="text-xs text-muted">{{ t("phoneAudio.limits") }}</p>
  </LpCard>
</template>
