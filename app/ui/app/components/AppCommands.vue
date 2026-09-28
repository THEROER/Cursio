<script setup lang="ts">
import { useHotkeys, type Command } from "@leavepulse/ui"
import type { ModuleSwitch } from "~/composables/useNode"

const { t } = useI18n()
const { status, notRunning, start, restart, setModule } = useNode()
const { logsOpen, paletteOpen } = useChrome()
const toast = useCursioToast()

const confirmRestart = ref(false)

async function startNode() {
  try {
    await start()
    toast.success(t("node.startRequested"))
  } catch (e) {
    toast.failure(e, { title: t("node.startFailed"), retry: startNode })
  }
}

async function restartNode() {
  try {
    await restart()
    toast.success(t("settings.restarting"))
  } catch (e) {
    toast.failure(e, { title: t("settings.restartFailed") })
  } finally {
    confirmRestart.value = false
  }
}

async function toggle(module: ModuleSwitch) {
  const on = status.value?.modules[module]
  if (on === undefined) return
  try {
    await setModule(module, !on)
  } catch (e) {
    toast.failure(e, { title: t("modules.toggleFailed") })
  }
}

const MODULES: ModuleSwitch[] = ["input", "clipboard", "audio", "files", "usb", "remote_screen", "screen_host"]
const MODULE_LABEL: Record<ModuleSwitch, string> = {
  input: "modules.input",
  clipboard: "modules.clipboard",
  audio: "modules.audio",
  files: "modules.files",
  usb: "modules.usb",
  remote_screen: "modules.remoteScreen",
  screen_host: "modules.screenHost",
}

const commands = computed<Command[]>(() => {
  const go = t("palette.go")
  const node = t("palette.node")
  const list: Command[] = [
    { id: "go-status", group: go, label: t("nav.status"), icon: "lucide:activity", onSelect: () => navigateTo("/") },
    { id: "go-placement", group: go, label: t("nav.placement"), icon: "lucide:layout-grid", onSelect: () => navigateTo("/placement") },
    { id: "go-pair", group: go, label: t("nav.pair"), icon: "lucide:link", keywords: ["code"], onSelect: () => navigateTo("/pair") },
    { id: "go-settings", group: go, label: t("nav.settings"), icon: "lucide:settings", keywords: ["theme", "autostart"], onSelect: () => navigateTo("/settings") },
    { id: "logs", group: node, label: t("logs.title"), icon: "lucide:scroll-text", shortcut: "Ctrl L", onSelect: () => { logsOpen.value = true } },
  ]
  if (notRunning.value) {
    list.push({ id: "start", group: node, label: t("node.start"), icon: "lucide:play", onSelect: () => void startNode() })
  } else if (status.value) {
    list.push({
      id: "restart",
      group: node,
      label: t("settings.restart"),
      icon: "lucide:rotate-cw",
      hidden: true,
      onSelect: () => { confirmRestart.value = true },
    })
    list.push({
      id: "modules",
      group: node,
      label: t("palette.toggleModule"),
      icon: "lucide:blocks",
      childPlaceholder: t("palette.moduleSearch"),
      children: MODULES.map((m) => ({
        id: `module-${m}`,
        label: t(MODULE_LABEL[m]),
        description: status.value?.modules[m] ? t("palette.on") : t("palette.off"),
        icon: status.value?.modules[m] ? "lucide:toggle-right" : "lucide:toggle-left",
        disabled: status.value?.pinned.includes(`modules.${m}`),
        keepOpen: true,
        onSelect: () => void toggle(m),
      })),
    })
  }
  return list
})

useHotkeys(() => [
  { key: "l", code: "KeyL", ctrl: true, handler: () => { logsOpen.value = !logsOpen.value } },
])

const labels = computed(() => ({
  navigate: t("palette.navigate"),
  select: t("palette.select"),
  close: t("palette.close"),
  back: t("palette.back"),
  searching: t("palette.searching"),
}))
</script>

<template>
  <LpCommandPalette
    v-model:open="paletteOpen"
    :commands="commands"
    :placeholder="t('palette.placeholder')"
    :empty-text="t('palette.empty')"
    :recent-label="t('palette.recent')"
    :results-label="t('palette.results')"
    :labels="labels"
  />
  <LpConfirmDialog
    v-model:open="confirmRestart"
    icon="lucide:rotate-cw"
    :title="t('settings.restartTitle')"
    :description="t('settings.restartBody')"
    :confirm-label="t('settings.restart')"
    :cancel-label="t('common.cancel')"
    @confirm="restartNode"
  />
</template>
