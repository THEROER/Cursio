<script setup lang="ts">
/**
 * Show this phone's screen on the computers, from the phone. Phone only — the
 * command exists only there. A computer can ask for the screen too, which
 * reaches the phone as a notification; both end at Android's consent dialog.
 *
 * The computers are chosen here, the one the user was at last ticked first:
 * sharing with every linked computer put the screen on whichever machines
 * happened to be up rather than the one being looked at.
 */
import type { PhoneScreenTarget } from "~/composables/usePhone"

const { t } = useI18n()
const { setup, refresh } = usePhone()
const toast = useCursioToast()
const busy = ref(false)

const showing = computed(() => setup.value?.showingScreen ?? false)

const targets = ref<PhoneScreenTarget[]>([])
/** Ticked computers; kept across refreshes so a choice is not undone under the user. */
const chosen = ref(new Set<string>())
let touched = false

async function loadTargets() {
  try {
    targets.value = await phoneScreenTargets()
  } catch (e) {
    toast.failure(e, { title: t("phoneScreen.failed") })
    return
  }
  const names = new Set(targets.value.map((target) => target.name))
  const kept = new Set([...chosen.value].filter((name) => names.has(name)))
  if (!touched || kept.size === 0) {
    const last = targets.value.find((target) => target.lastUsed)
    const only = targets.value.length === 1 ? targets.value[0] : undefined
    const preferred = last ?? only
    chosen.value = new Set(preferred ? [preferred.name] : [])
    touched = false
  } else {
    chosen.value = kept
  }
}

function setChosen(name: string, on: boolean) {
  touched = true
  const next = new Set(chosen.value)
  if (on) next.add(name)
  else next.delete(name)
  chosen.value = next
}

async function toggle() {
  busy.value = true
  try {
    await phoneScreenShare(!showing.value, showing.value ? undefined : [...chosen.value])
  } catch (e) {
    toast.failure(e, { title: t("phoneScreen.failed") })
  } finally {
    busy.value = false
    // The consent dialog answers after this resolves; the window coming back
    // to the front reads the state again either way.
    await refresh()
  }
}

function onReturn() {
  if (document.visibilityState === "visible") void loadTargets()
}

onMounted(() => {
  void loadTargets()
  window.addEventListener("focus", onReturn)
  document.addEventListener("visibilitychange", onReturn)
})
onBeforeUnmount(() => {
  window.removeEventListener("focus", onReturn)
  document.removeEventListener("visibilitychange", onReturn)
})
</script>

<template>
  <LpCard padded class="flex flex-col gap-3">
    <div class="flex items-center gap-3">
      <LpIcon name="lucide:cast" :size="18" class="shrink-0 text-muted" />
      <p class="min-w-0 flex-1 text-sm">
        {{ showing ? t("phoneScreen.showing") : t("phoneScreen.consent") }}
      </p>
      <LpButton
        :variant="showing ? 'danger' : 'solid'"
        class="shrink-0"
        :disabled="busy || (!showing && chosen.size === 0)"
        @click="toggle"
      >
        {{ showing ? t("phoneScreen.stop") : t("phoneScreen.show") }}
      </LpButton>
    </div>
    <template v-if="!showing">
      <p v-if="targets.length === 0" class="text-xs text-muted">{{ t("phoneScreen.noComputers") }}</p>
      <div v-else class="flex flex-col gap-2">
        <p class="text-xs text-muted">{{ t("phoneScreen.choose") }}</p>
        <LpCheckbox
          v-for="target in targets"
          :key="target.name"
          :model-value="chosen.has(target.name)"
          :label="target.lastUsed ? t('phoneScreen.lastUsed', { computer: target.name }) : target.name"
          @update:model-value="(on: boolean) => setChosen(target.name, on)"
        />
      </div>
    </template>
    <p v-if="setup && !setup.touch" class="text-xs text-muted">{{ t("phoneScreen.noTouch") }}</p>
  </LpCard>
</template>
