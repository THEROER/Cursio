<script setup lang="ts">
/**
 * Show this phone's screen on the computers, from the phone. Phone only — the
 * command exists only there. A computer can ask for the screen too, which
 * reaches the phone as a notification; both end at Android's consent dialog.
 */
const { t } = useI18n()
const { setup, refresh } = usePhone()
const toast = useCursioToast()
const busy = ref(false)

const showing = computed(() => setup.value?.showingScreen ?? false)

async function toggle() {
  busy.value = true
  try {
    await phoneScreenShare(!showing.value)
  } catch (e) {
    toast.failure(e, { title: t("phoneScreen.failed") })
  } finally {
    busy.value = false
    // The consent dialog answers after this resolves; the window coming back
    // to the front reads the state again either way.
    await refresh()
  }
}
</script>

<template>
  <LpCard padded class="flex flex-col gap-3">
    <div class="flex items-center gap-3">
      <LpIcon name="lucide:cast" :size="18" class="shrink-0 text-muted" />
      <p class="min-w-0 flex-1 text-sm">
        {{ showing ? t("phoneScreen.showing") : t("phoneScreen.consent") }}
      </p>
      <LpButton :variant="showing ? 'danger' : 'solid'" class="shrink-0" :disabled="busy" @click="toggle">
        {{ showing ? t("phoneScreen.stop") : t("phoneScreen.show") }}
      </LpButton>
    </div>
    <p v-if="setup && !setup.touch" class="text-xs text-muted">{{ t("phoneScreen.noTouch") }}</p>
  </LpCard>
</template>
