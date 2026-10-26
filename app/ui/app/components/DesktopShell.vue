<script setup lang="ts">
import type { SidebarItem } from "@leavepulse/ui"

/**
 * The desktop window's frame: the kit's app shell with its rail, the window's
 * own title bar, and the overlays every page shares (log drawer, command
 * palette, confirmations). On a phone every page gets `MobileShell` instead,
 * so a page names one shell and never asks which platform it runs on.
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
const { status, notRunning } = useNode()
const { logsOpen, paletteOpen } = useChrome()
const { isMobile } = usePlatform()

const items = computed<SidebarItem[]>(() => [
  { id: "/", label: t("nav.status"), icon: "lucide:activity", badge: status.value?.problems.length || undefined },
  { id: "/placement", label: t("nav.placement"), icon: "lucide:layout-grid" },
  { id: "/pair", label: t("nav.pair"), icon: "lucide:link", dot: !!status.value?.pairing },
  // Only while a phone shares its messages, or the page is already open
  // (`cursio-app --page messages` from the Shell menu).
  ...(status.value?.messages?.length || route.path === "/messages"
    ? [{ id: "/messages", label: t("nav.messages"), icon: "lucide:message-square" }]
    : []),
  ...(status.value?.gallery?.length || route.path === "/gallery"
    ? [{ id: "/gallery", label: t("nav.gallery"), icon: "lucide:images" }]
    : []),
  { id: "/settings", label: t("nav.settings"), icon: "lucide:settings" },
])

const nodeState = computed(() =>
  status.value
    ? { tone: "success" as const, dot: "bg-action", label: t("node.running") }
    : notRunning.value
      ? { tone: "danger" as const, dot: "bg-danger", label: t("node.stopped") }
      : { tone: "neutral" as const, dot: "bg-muted", label: t("node.connecting") },
)

const title = computed(() => {
  const match = [...items.value].sort((a, b) => b.id.length - a.id.length).find((i) =>
    i.id === "/" ? route.path === "/" : route.path.startsWith(i.id),
  )
  return match?.label ?? t("app.title")
})
</script>

<template>
  <MobileShell v-if="isMobile" :full-bleed="fullBleed">
    <slot />
  </MobileShell>
  <LpAppShell
    v-else
    :items="items"
    :path="route.path"
    :full-bleed="fullBleed"
    content-class="mx-auto w-full max-w-5xl"
    resizable
    pinnable
    persist="cursio"
    @select="(item: SidebarItem) => navigateTo(item.id)"
  >
    <template #logo="{ compact }">
      <div class="flex items-center gap-2" :class="compact ? '' : 'px-1'" data-tauri-drag-region>
        <CursioLogo :size="20" class="shrink-0 text-brand" />
        <div v-if="!compact" class="min-w-0" data-tauri-drag-region>
          <p class="truncate text-sm font-semibold" data-tauri-drag-region>{{ t("app.title") }}</p>
          <p class="truncate text-xs text-muted" data-tauri-drag-region>{{ status?.node.name ?? t("app.desktopSubtitle") }}</p>
        </div>
      </div>
    </template>

    <template #sidebar-footer="{ compact }">
      <LpTooltip v-if="compact" :content="nodeState.label" side="right">
        <span class="grid size-10 place-items-center" role="status" :aria-label="nodeState.label">
          <span class="size-2.5 rounded-full" :class="nodeState.dot" />
        </span>
      </LpTooltip>
      <div v-else class="flex flex-col gap-2 px-1">
        <LpBadge :tone="nodeState.tone" dot>{{ nodeState.label }}</LpBadge>
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

    <NodeDownAlert />
    <WindowStaleAlert />
    <slot />

    <template #overlays>
      <LogsDrawer />
      <AppCommands />
      <PeerForgetDialog />
    </template>
  </LpAppShell>
</template>
