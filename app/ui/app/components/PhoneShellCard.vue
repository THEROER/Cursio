<script setup lang="ts">
/**
 * This phone's own shell, through its wireless debugging
 * (`docs/phone-shell-design.md`). Pairing opens the developer settings at
 * wireless debugging and ends in a notification: the system's pairing dialog
 * has to stay open while its code is typed, so the code is asked for there,
 * not here.
 */
const { t } = useI18n()
const { setup, request } = usePhone()
const toast = useCursioToast()
const busy = ref(false)

async function pair() {
  busy.value = true
  try {
    await request("shell")
  } catch (e) {
    toast.failure(e, { title: t("phone.requestFailed") })
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <LpCard v-if="setup" padded class="flex flex-col gap-3">
    <div class="flex items-center gap-3">
      <LpIcon name="lucide:terminal" :size="18" class="shrink-0 text-muted" />
      <div class="min-w-0 flex-1">
        <p class="text-sm font-medium">{{ t("phoneShell.title") }}</p>
        <p class="text-xs text-muted">
          {{ setup.shell ? (setup.shellOn ? t("phoneShell.ready") : t("phoneShell.off")) : t("phoneShell.hint") }}
        </p>
      </div>
      <LpBadge v-if="setup.shell && setup.shellOn" tone="success" dot size="sm" class="shrink-0">
        {{ t("phoneShell.on") }}
      </LpBadge>
      <LpButton v-else variant="solid" class="shrink-0" :disabled="busy" @click="pair">
        {{ setup.shell ? t("phoneShell.turnOn") : t("phoneShell.pair") }}
      </LpButton>
    </div>
    <p v-if="!setup.shell" class="text-xs text-muted">{{ t("phoneShell.how") }}</p>
  </LpCard>
</template>
