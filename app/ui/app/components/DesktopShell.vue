<script setup lang="ts">
import type { SidebarItem } from "@leavepulse/ui"

/**
 * The desktop window's frame: the kit's app shell with its rail, the window's
 * own title bar, and the overlays every page shares (log drawer, command
 * palette, confirmations).
 *
 * The page title and the empty stretch of the bar carry
 * `data-tauri-drag-region`: with the system title bar gone, that is where the
 * window is picked up — Tauri moves it only from an element that carries the
 * attribute itself, not from anything inside one.
 */
defineProps<{
  /** The page owns the whole content area (a canvas that pans on its own). */
  fullBleed?: boolean
}>()

const { t } = useI18n()
const route = useRoute()
const { status, error, notRunning, start } = useNode()
const { logsOpen, paletteOpen } = useChrome()
const toast = useCursioToast()

const items = computed<SidebarItem[]>(() => [
  { id: "/", label: t("nav.status"), icon: "lucide:activity", badge: status.value?.problems.length || undefined },
  { id: "/placement", label: t("nav.placement"), icon: "lucide:layout-grid" },
  { id: "/pair", label: t("nav.pair"), icon: "lucide:link", badge: status.value?.pairing ? "•" : undefined },
  { id: "/settings", label: t("nav.settings"), icon: "lucide:settings" },
])

const title = computed(() => {
  const match = [...items.value].sort((a, b) => b.id.length - a.id.length).find((i) =>
    i.id === "/" ? route.path === "/" : route.path.startsWith(i.id),
  )
  return match?.label ?? t("app.title")
})

const starting = ref(false)
async function startNode() {
  starting.value = true
  try {
    await start()
    toast.success(t("node.startRequested"))
  } catch (e) {
    toast.failure(e, { title: t("node.startFailed"), retry: startNode })
  } finally {
    // The node takes a moment to open its socket; the pushed status says when.
    setTimeout(() => (starting.value = false), 3000)
  }
}
</script>

<template>
  <LpAppShell
    :items="items"
    :path="route.path"
    :full-bleed="fullBleed"
    content-class="mx-auto w-full max-w-5xl"
    @select="(item: SidebarItem) => navigateTo(item.id)"
  >
    <template #logo>
      <div class="flex items-center gap-2 px-1" data-tauri-drag-region>
        <LpIcon name="lucide:mouse-pointer-2" :size="20" class="text-brand" />
        <div class="min-w-0" data-tauri-drag-region>
          <p class="truncate text-sm font-semibold" data-tauri-drag-region>{{ t("app.title") }}</p>
          <p class="truncate text-xs text-muted" data-tauri-drag-region>{{ status?.node.name ?? t("app.desktopSubtitle") }}</p>
        </div>
      </div>
    </template>

    <template #sidebar-footer>
      <div class="flex flex-col gap-2 px-1">
        <LpBadge v-if="status" tone="success" dot>{{ t("node.running") }}</LpBadge>
        <LpBadge v-else-if="notRunning" tone="danger" dot>{{ t("node.stopped") }}</LpBadge>
        <LpBadge v-else tone="neutral" dot>{{ t("node.connecting") }}</LpBadge>
      </div>
    </template>

    <template #header-title>
      <div class="flex h-full min-w-0 flex-1 items-center" data-tauri-drag-region>
        <h1 class="truncate text-lg font-semibold" data-tauri-drag-region>{{ title }}</h1>
      </div>
    </template>

    <template #header-actions>
      <LpTooltip :content="t('palette.open')">
        <LpButton variant="soft" size="sm" :aria-label="t('palette.open')" @click="paletteOpen = true">
          <LpIcon name="lucide:search" :size="14" />
          <kbd class="font-mono text-[11px] text-muted">Ctrl K</kbd>
        </LpButton>
      </LpTooltip>
      <UpdateButton />
      <ProblemsBell />
      <LpTooltip :content="t('logs.toggle')">
        <LpButton
          :variant="logsOpen ? 'soft' : 'ghost'"
          size="sm"
          square
          :aria-label="t('logs.toggle')"
          :aria-pressed="logsOpen"
          @click="logsOpen = !logsOpen"
        >
          <LpIcon name="lucide:scroll-text" :size="16" />
        </LpButton>
      </LpTooltip>
      <WindowControls />
    </template>

    <LpAlert v-if="notRunning" variant="warning" :title="t('node.notRunningTitle')" class="mb-4">
      <p class="text-sm">{{ t("node.notRunningBody") }}</p>
      <div class="mt-3">
        <LpButton variant="solid" size="sm" :disabled="starting" @click="startNode">
          {{ starting ? t("node.starting") : t("node.start") }}
        </LpButton>
      </div>
    </LpAlert>
    <LpAlert v-else-if="error" variant="danger" class="mb-4">{{ error.message }}</LpAlert>
    <slot />

    <template #overlays>
      <LogsDrawer />
      <AppCommands />
      <PeerForgetDialog />
    </template>
  </LpAppShell>
</template>
