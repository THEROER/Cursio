<script setup lang="ts">
import { invoke } from "@tauri-apps/api/core"
import type { LogLine } from "@leavepulse/ui"

const { t } = useI18n()
// Shared, so a peer's "show in logs" can open this already narrowed to it.
const { logsOpen, logsSource: source, logsFilter: filter } = useChrome()

const lines = ref<LogLine[]>([])
const error = ref("")

/** Read while the drawer is open, at the pace a person reads a tailing log. */
const REFRESH_MS = 2000
const LINES = 800

async function load() {
  try {
    const raw = await invoke<string[]>("log_tail", { source: source.value, lines: LINES })
    lines.value = raw.map((message) => ({ message }))
    error.value = ""
  } catch (e) {
    error.value = errorText(e)
    lines.value = []
  }
}

let timer: ReturnType<typeof setInterval> | null = null
function stop() {
  if (timer) clearInterval(timer)
  timer = null
}
watch(
  logsOpen,
  (open) => {
    stop()
    if (!open) return
    void load()
    timer = setInterval(load, REFRESH_MS)
  },
  { immediate: true },
)
watch(source, () => void load())
onBeforeUnmount(stop)

const sources = computed(() => [
  { value: "node", label: t("logs.node"), icon: "lucide:server" },
  { value: "app", label: t("logs.app"), icon: "lucide:app-window" },
])
</script>

<template>
  <LpDrawer v-model:open="logsOpen" side="right" size="xl" resizable :title="t('logs.title')">
    <div class="flex h-full min-h-0 flex-col gap-3">
      <div class="flex flex-wrap items-center gap-2">
        <LpSegmented v-model="source" :options="sources" size="sm" />
        <LpInput v-model="filter" size="sm" :placeholder="t('logs.filter')" class="min-w-40 flex-1" />
      </div>
      <LpAlert v-if="error" variant="warning">{{ error }}</LpAlert>
      <LpLogViewer
        :lines="lines"
        parse
        :highlight="filter"
        filter-matches
        height="100%"
        class="min-h-0 flex-1"
        :empty-label="t('logs.empty')"
      />
    </div>
  </LpDrawer>
</template>
