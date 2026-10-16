<script setup lang="ts">
/**
 * Where the phone's own sound plays: one of the peers' output devices, or
 * nowhere but the phone. Phone only — the commands exist only there.
 */
import type { PhoneAudioTarget } from "~/composables/usePhone"

const OFF = "off"

const { t } = useI18n()
const { status } = useNode()
const toast = useCursioToast()

/** `null` until the first answer, so the card does not flash "no devices". */
const targets = ref<PhoneAudioTarget[] | null>(null)
const busy = ref(false)

async function refresh() {
  try {
    targets.value = await phoneAudioTargets()
  } catch (e) {
    toast.failure(e, { title: t("phoneAudio.listFailed") })
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
  if (target === (active.value?.key ?? null)) return
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
    <p v-else-if="targets.length === 0" class="text-xs text-muted">{{ t("phoneAudio.none") }}</p>
    <p class="text-xs text-muted">{{ t("phoneAudio.limits") }}</p>
  </LpCard>
</template>
