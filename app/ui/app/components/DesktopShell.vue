<script setup lang="ts">
import { LpAlert, LpBadge, LpButton, LpTabs } from "@leavepulse/ui"

const { t } = useI18n()
const route = useRoute()
const { status, error, notRunning, start } = useNode()

const tabs = computed(() => [
  { value: "/", label: t("nav.status"), icon: "lucide:activity" },
  { value: "/placement", label: t("nav.placement"), icon: "lucide:layout-grid" },
  { value: "/pair", label: t("nav.pair"), icon: "lucide:link" },
  { value: "/settings", label: t("nav.settings"), icon: "lucide:settings" },
])

const current = computed({
  get: () => route.path,
  set: (path: string) => void navigateTo(path),
})

const starting = ref(false)
const startError = ref("")
async function startNode() {
  starting.value = true
  startError.value = ""
  try {
    await start()
  } catch (e) {
    startError.value = (e as { message?: string }).message ?? String(e)
  } finally {
    // The node takes a moment to open its socket; the poll picks it up.
    setTimeout(() => (starting.value = false), 3000)
  }
}
</script>

<template>
  <div class="flex h-dvh flex-col bg-surface text-ink">
    <header class="flex items-center gap-3 border-b border-line px-4 py-3">
      <div class="min-w-0">
        <h1 class="truncate text-base font-semibold">{{ status?.node.name ?? t("app.title") }}</h1>
        <p class="truncate text-xs text-muted">{{ t("app.desktopSubtitle") }}</p>
      </div>
      <LpBadge v-if="status" tone="success" dot class="ml-auto shrink-0">{{ t("node.running") }}</LpBadge>
      <LpBadge v-else-if="notRunning" tone="danger" dot class="ml-auto shrink-0">
        {{ t("node.stopped") }}
      </LpBadge>
      <LpBadge v-else tone="neutral" dot class="ml-auto shrink-0">{{ t("node.connecting") }}</LpBadge>
    </header>

    <nav class="px-4 pt-3">
      <LpTabs v-model="current" :items="tabs" variant="underline" />
    </nav>

    <main class="min-h-0 flex-1 overflow-y-auto px-4 py-4">
      <LpAlert v-if="notRunning" variant="warning" :title="t('node.notRunningTitle')" class="mb-4">
        <p class="text-sm">{{ t("node.notRunningBody") }}</p>
        <div class="mt-3 flex items-center gap-3">
          <LpButton variant="solid" size="sm" :disabled="starting" @click="startNode">
            {{ starting ? t("node.starting") : t("node.start") }}
          </LpButton>
          <span v-if="startError" class="text-xs text-danger">{{ startError }}</span>
        </div>
      </LpAlert>
      <LpAlert v-else-if="error" variant="danger" class="mb-4">{{ error.message }}</LpAlert>
      <slot />
    </main>
  </div>
</template>
