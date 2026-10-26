<script setup lang="ts">
/**
 * The phone's frame: a title bar, one scrolling column and a tab bar under it.
 *
 * Not the kit's app shell squeezed narrow: that turns its rail into a drawer
 * behind a burger, which hides four destinations behind a fifth tap, and its
 * bar sits under the status bar because nothing in it knows about insets. A
 * phone's web view runs edge to edge (`viewport-fit=cover`), so every edge
 * here pads by its safe-area inset — the top clears the status bar and a
 * camera cutout, the tab bar clears the gesture strip.
 *
 * There is no window to move, minimise or close, no tray, no log to tail and
 * no Ctrl K to press, so none of the desktop bar's actions are drawn.
 */
defineProps<{
  /** The page owns the whole content area (a touch surface). */
  fullBleed?: boolean
}>()

const { t } = useI18n()
const route = useRoute()
const { status, notRunning } = useNode()

const items = computed(() => [
  { path: "/", label: t("nav.status"), icon: "lucide:activity", dot: (status.value?.problems.length ?? 0) > 0 },
  { path: "/pair", label: t("nav.pair"), icon: "lucide:link", dot: !!status.value?.pairing },
  { path: "/touchpad", label: t("nav.touchpad"), icon: "lucide:mouse-pointer-click", dot: false },
  { path: "/gallery", label: t("nav.gallery"), icon: "lucide:images", dot: false },
  { path: "/settings", label: t("nav.settings"), icon: "lucide:settings", dot: false },
])

function isActive(path: string) {
  return path === "/" ? route.path === "/" : route.path.startsWith(path)
}

const title = computed(() => items.value.find((i) => isActive(i.path))?.label ?? t("app.title"))

// The tab bar is the kit's, so its marker slides between destinations; the
// route stays the single source of which one is open.
const tabs = computed(() => items.value.map((i) => ({ value: i.path, label: i.label, icon: i.icon, dot: i.dot })))
const activeTab = computed({
  get: () => items.value.find((i) => isActive(i.path))?.path ?? "/",
  set: (path: string) => void navigateTo(path),
})

// The phone's sound comes back to its computer from whatever page opens
// first, as soon as that computer is linked: the node can start capture only
// with the consent a visible window asks for.
const toast = useCursioToast()
async function resumeSound() {
  try {
    if (await resumePhoneAudio()) toast.success(t("phoneAudio.routed"))
  } catch (e) {
    toast.failure(e, { title: t("phoneAudio.routeFailed") })
  }
}
watch(
  () => status.value?.peers.map((p) => `${p.id}:${p.presence}`).join(),
  () => void resumeSound(),
)
onMounted(() => void resumeSound())
</script>

<template>
  <div class="cursio-mobile flex h-dvh flex-col overflow-hidden bg-surface text-ink">
    <header class="cursio-mobile__top shrink-0 border-b border-line bg-surface">
      <div class="flex h-14 items-center gap-3 px-4">
        <CursioLogo :size="20" class="shrink-0 text-brand" />
        <div class="min-w-0 flex-1">
          <h1 class="truncate text-base font-semibold leading-tight">{{ title }}</h1>
          <p class="truncate text-xs text-muted">{{ status?.node.name ?? t("app.title") }}</p>
        </div>
        <LpBadge v-if="status" tone="success" dot size="sm" class="shrink-0">{{ t("node.runningShort") }}</LpBadge>
        <LpBadge v-else-if="notRunning" tone="danger" dot size="sm" class="shrink-0">{{ t("node.stoppedShort") }}</LpBadge>
        <LpBadge v-else tone="neutral" dot size="sm" class="shrink-0">{{ t("node.connecting") }}</LpBadge>
        <ProblemsBell />
      </div>
    </header>

    <main v-if="fullBleed" class="cursio-mobile__sides flex min-h-0 flex-1 flex-col overflow-hidden">
      <slot />
    </main>
    <main v-else class="cursio-mobile__sides min-h-0 flex-1">
      <LpScrollArea class="h-full" content-class="px-4 py-4">
        <NodeDownAlert />
        <slot />
      </LpScrollArea>
    </main>

    <LpTabs
      v-model="activeTab"
      :items="tabs"
      variant="bottom"
      accent
      :label="t('nav.label')"
      class="shrink-0"
    />

    <AppCommands />
    <PeerForgetDialog />
    <ScreenAskSheet />
  </div>
</template>

<style scoped>
.cursio-mobile__top {
  padding-top: env(safe-area-inset-top);
  padding-left: env(safe-area-inset-left);
  padding-right: env(safe-area-inset-right);
}

.cursio-mobile__sides {
  padding-left: env(safe-area-inset-left);
  padding-right: env(safe-area-inset-right);
}
</style>
