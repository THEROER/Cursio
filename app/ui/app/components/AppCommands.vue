<script setup lang="ts">
/*
 * The command palette (Ctrl K): pages, machines, modules, themes, problems and
 * the node's log in one box, each with a preview pane beside the list.
 *
 * Rows are looked up for the pane by id prefix (`peer:`, `module:` …) rather
 * than by object: the palette copies some rows (a fallback, a recent pick), so
 * identity does not survive, the id does. A row the slot draws nothing for
 * falls back to the kit's own text pane, which is what log lines use.
 */
import { invoke } from "@tauri-apps/api/core"
import { presets, useHotkeys, useTheme, type Command, type PaletteFact, type TokenSet } from "@leavepulse/ui"
import { toMillis, type ModuleSwitch, type PeerView, type Problem } from "~/composables/useNode"

/** The switcher's own list, in its order, so both offer the same themes. */
const THEMES: TokenSet[] = Object.values(presets)

const { t } = useI18n()
const { status, notRunning, start, restart, setModule, applyUpdate } = useNode()
const { logsOpen, paletteOpen, showInLogs } = useChrome()
const { items: peerMenu } = usePeerMenu()
const { battery, network } = useMachineText()
const theme = useTheme()
const toast = useCursioToast()
// A phone's build registers none of the daemon's commands (logs, restart,
// updates, placement), so the palette there must not offer them at all.
const { isMobile } = usePlatform()

const confirmRestart = ref(false)
const confirmUpdate = ref(false)

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

async function switchVersion() {
  const version = status.value?.update.ready?.version ?? ""
  try {
    await applyUpdate()
    toast.success(t("update.switching", { version }))
  } catch (e) {
    toast.failure(e, { title: t("update.applyFailed") })
  } finally {
    confirmUpdate.value = false
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

interface Page {
  path: string
  label: string
  icon: string
  lead: string
  keywords: string[]
  only?: "desktop" | "mobile"
}

const ALL_PAGES: Page[] = [
  { path: "/", label: "nav.status", icon: "lucide:activity", lead: "palette.pageStatus", keywords: ["peers", "latency"] },
  { path: "/placement", label: "nav.placement", icon: "lucide:layout-grid", lead: "placement.description", keywords: ["position", "grid"], only: "desktop" },
  { path: "/pair", label: "nav.pair", icon: "lucide:link", lead: "pair.description", keywords: ["code"] },
  { path: "/touchpad", label: "nav.touchpad", icon: "lucide:mouse-pointer-click", lead: "app.subtitle", keywords: ["mouse", "pointer"], only: "mobile" },
  { path: "/settings", label: "nav.settings", icon: "lucide:settings", lead: "palette.pageSettings", keywords: ["autostart", "version", "volume"] },
]

const PAGES = computed(() => {
  const here = isMobile.value ? "mobile" : "desktop"
  return ALL_PAGES.filter((p) => !p.only || p.only === here)
})

const SEVERITY_ICON = { Info: "lucide:info", Warning: "lucide:triangle-alert", Error: "lucide:octagon-alert" } as const

function peerIcon(p: PeerView): string {
  return p.link ? KIND_ICON[p.link.device_kind] : UNKNOWN_KIND_ICON
}

function peerCommand(p: PeerView): Command {
  return {
    id: `peer:${p.id}`,
    group: t("palette.peers"),
    label: p.name,
    description: t(`presence.${p.presence}`),
    icon: peerIcon(p),
    keywords: [p.id, ...(p.link ? [p.link.device_kind] : [])],
    onSelect: () => void navigateTo("/"),
    // Tab on the row: the same actions as the peer's right-click menu.
    actions: peerMenu(p).map((item, i) => ({
      id: `peer:${p.id}:action-${i}`,
      label: item.label,
      icon: item.icon,
      disabled: item.disabled,
      onSelect: item.onSelect,
    })),
  }
}

function themeCommand(set: TokenSet): Command {
  const current = theme.currentName.value === set.name
  return {
    id: `theme:${set.name}`,
    label: set.name,
    description: current ? t("palette.current") : undefined,
    icon: current ? "lucide:check" : "lucide:palette",
    keywords: [set.mode],
    keepOpen: true,
    remember: false,
    onSelect: () => theme.setTheme(set),
  }
}

/** Context around a log line for the pane: the line a reader opens it for. */
const LOG_CONTEXT = 6
const LOG_LINES = 2000
const LOG_RESULTS = 60

async function searchLog(query: string, _signal: AbortSignal, parsed: { include: string[] }): Promise<Command[]> {
  const lines = await invoke<string[]>("log_tail", { source: "node", lines: LOG_LINES })
  const hits: Command[] = []
  for (let i = lines.length - 1; i >= 0 && hits.length < LOG_RESULTS; i--) {
    const line = lines[i]!
    const lower = line.toLowerCase()
    if (!parsed.include.every((word) => lower.includes(word))) continue
    hits.push({
      id: `log:${i}`,
      label: line,
      icon: / ERROR /.test(line) ? SEVERITY_ICON.Error : / WARN /.test(line) ? SEVERITY_ICON.Warning : "lucide:scroll-text",
      preview: lines.slice(Math.max(0, i - LOG_CONTEXT), i + LOG_CONTEXT + 1).join("\n"),
      remember: false,
      onSelect: () => showInLogs(query),
    })
  }
  return hits
}

const commands = computed<Command[]>(() => {
  const go = t("palette.go")
  const node = t("palette.node")
  const s = status.value
  const list: Command[] = PAGES.value.map((p) => ({
    id: `page:${p.path}`,
    group: go,
    label: t(p.label),
    icon: p.icon,
    keywords: p.keywords,
    onSelect: () => void navigateTo(p.path),
  }))

  const peers = (s?.peers ?? []).map(peerCommand)
  list.push(...peers)
  if (peers.length > 0) {
    list.push({
      id: "peers",
      group: t("palette.peers"),
      label: t("palette.findPeer"),
      icon: "lucide:search",
      prefix: "@",
      modeLabel: t("palette.peersMode"),
      childPlaceholder: t("palette.peerSearch"),
      noPreview: true,
      children: peers,
    })
  }

  for (const p of s?.problems ?? []) {
    list.push({
      id: `problem:${p.id}`,
      group: t("palette.problems"),
      label: p.summary,
      description: t(`severity.${p.severity}`),
      icon: SEVERITY_ICON[p.severity],
      onSelect: () => void navigateTo("/"),
    })
  }

  const desktop = !isMobile.value

  if (desktop) {
    list.push({
      id: "logs",
      group: node,
      label: t("logs.title"),
      icon: "lucide:scroll-text",
      shortcut: "Ctrl L",
      onSelect: () => { logsOpen.value = true },
    })
    list.push({
      id: "log-search",
      group: node,
      label: t("palette.searchLogs", { query: "{query}" }),
      modeLabel: t("palette.logsMode"),
      icon: "lucide:text-search",
      prefix: "/",
      fallback: true,
      carryQuery: true,
      remember: false,
      noPreview: true,
      childPlaceholder: t("palette.logSearch"),
      children: searchLog,
    })
  }

  if (notRunning.value) {
    list.push({ id: "start", group: node, label: t("node.start"), icon: "lucide:play", onSelect: () => void startNode() })
  } else if (s) {
    if (desktop && s.update.ready) {
      list.push({
        id: "update",
        group: node,
        label: t("palette.updateApply", { version: s.update.ready.version }),
        icon: "lucide:download",
        keywords: ["update", "version"],
        onSelect: () => { confirmUpdate.value = true },
      })
    }
    list.push({
      id: "restart",
      group: node,
      label: t("settings.restart"),
      icon: "lucide:rotate-cw",
      hidden: !desktop || s.restart_needed.length === 0,
      onSelect: () => { confirmRestart.value = true },
    })
    list.push({
      id: "modules",
      group: node,
      label: t("palette.toggleModule"),
      icon: "lucide:blocks",
      prefix: "m ",
      carryQuery: true,
      modeLabel: t("palette.modulesMode"),
      childPlaceholder: t("palette.moduleSearch"),
      noPreview: true,
      children: MODULES.map((m) => ({
        id: `module:${m.key}`,
        label: t(m.label),
        description: s.modules[m.key] ? t("palette.on") : t("palette.off"),
        icon: s.modules[m.key] ? "lucide:toggle-right" : "lucide:toggle-left",
        disabled: s.pinned.includes(`modules.${m.key}`),
        keepOpen: true,
        onSelect: () => void toggle(m.key),
      })),
    })
  }

  list.push({
    id: "theme",
    group: node,
    label: t("palette.theme"),
    icon: "lucide:palette",
    prefix: "t ",
    modeLabel: t("palette.themeMode"),
    childPlaceholder: t("palette.themeSearch"),
    noPreview: true,
    children: THEMES.map(themeCommand),
  })
  return list
})

/** What the pane shows for a row, found by the row's id. */
type Preview =
  | { kind: "page"; path: string; lead: string }
  | { kind: "peer"; peer: PeerView }
  | { kind: "module"; key: ModuleSwitch }
  | { kind: "theme"; set: TokenSet }
  | { kind: "problem"; problem: Problem }
  | { kind: "logs" | "restart" | "start" | "update" }

function previewOf(command: Command): Preview | null {
  const [kind, ...rest] = command.id.split(":")
  const key = rest.join(":")
  const s = status.value
  switch (kind) {
    case "page": {
      const page = PAGES.value.find((p) => p.path === key)
      return page ? { kind, path: page.path, lead: page.lead } : null
    }
    case "peer": {
      const peer = s?.peers.find((p) => p.id === key)
      return peer && !key.includes(":") ? { kind, peer } : null
    }
    case "module":
      return MODULES.some((m) => m.key === key) ? { kind, key: key as ModuleSwitch } : null
    case "theme": {
      const set = THEMES.find((th) => th.name === key)
      return set ? { kind, set } : null
    }
    case "problem": {
      const problem = s?.problems.find((p) => p.id === key)
      return problem ? { kind, problem } : null
    }
    case "logs":
    case "restart":
    case "start":
    case "update":
      return { kind }
    default:
      return null
  }
}

const cannotSay = computed(() => t("peer.cannotSay"))

function peerFacts(p: PeerView): PaletteFact[] {
  const facts: PaletteFact[] = [{ icon: "lucide:radio", label: t("palette.presence"), value: t(`presence.${p.presence}`) }]
  const link = p.link
  if (!link) return facts
  facts.push(
    { icon: "lucide:layout-grid", label: t("peer.position"), value: `${link.position.x}, ${link.position.y}`, mono: true },
    { icon: "lucide:battery", label: t("peer.battery"), value: battery(link.status) ?? cannotSay.value },
    { icon: "lucide:wifi", label: t("peer.network"), value: network(link.status) ?? cannotSay.value },
    {
      icon: "lucide:blocks",
      label: t("palette.sharedOnLink"),
      hint: link.modules.join(", ") || t("palette.nothingShared"),
    },
  )
  for (const [module, why] of link.skipped) {
    facts.push({ icon: "lucide:circle-off", label: t("peer.skipped", { module, why }) })
  }
  return facts
}

function moduleFacts(key: ModuleSwitch): PaletteFact[] {
  const on = status.value?.modules[key] ?? false
  const facts: PaletteFact[] = [
    { icon: on ? "lucide:toggle-right" : "lucide:toggle-left", label: t("palette.onHere"), value: on ? t("palette.on") : t("palette.off"), current: on },
  ]
  if (status.value?.pinned.includes(`modules.${key}`)) {
    facts.push({ icon: "lucide:flag", label: t("modules.flag"), hint: t("modules.pinned") })
  }
  return facts
}

function pageFacts(path: string): PaletteFact[] {
  const s = status.value
  if (!s) return []
  switch (path) {
    case "/placement":
      return [
        { icon: "lucide:locate", label: t("palette.thisMachine"), value: `${s.node.position.x}, ${s.node.position.y}`, mono: true },
        { icon: "lucide:layout-grid", label: t("palette.placedPeers"), value: s.peers.filter((p) => p.link).length },
      ]
    case "/pair":
      return s.pairing?.code ? [{ icon: "lucide:key-round", label: t("palette.pairingCode"), value: s.pairing.code, mono: true }] : []
    case "/settings":
      return [
        { icon: "lucide:package", label: t("settings.installedVersion"), value: s.node.installed_version ?? t("settings.notInstalled") },
        { icon: "lucide:hammer", label: t("settings.build"), value: s.node.build || "?", mono: true },
        { icon: "lucide:file-cog", label: t("settings.configFile"), hint: s.config_path },
      ]
    default:
      return []
  }
}

const statusStats = computed(() => {
  const s = status.value
  if (!s) return []
  const latency = worstLatency(s.latency)
  return [
    { label: t("stats.peers"), value: s.peers.filter((p) => p.presence === "online").length, suffix: ` / ${s.peers.length}` },
    { label: t("stats.problems"), value: s.problems.length },
    latency
      ? { label: t("stats.latency"), value: latency.p99, suffix: " ms", format: (v: number) => v.toFixed(1) }
      : { label: t("stats.latency"), value: "—" },
  ]
})

function swatches(set: TokenSet): string[] {
  const c = set.colors
  return [c.ink, c.brand, c.action, c.accent, c.danger]
}

useHotkeys(() =>
  isMobile.value ? [] : [{ key: "l", code: "KeyL", ctrl: true, handler: () => { logsOpen.value = !logsOpen.value } }],
)

const labels = computed(() => ({
  navigate: t("palette.navigate"),
  select: t("palette.select"),
  close: t("palette.close"),
  back: t("palette.back"),
  searching: t("palette.searching"),
  searchFailed: t("palette.searchFailed"),
  retry: t("palette.retry"),
  showMore: t("palette.showMore", { count: "{count}" }),
  actions: t("palette.actions"),
  title: t("palette.title"),
  description: t("palette.description"),
  esc: t("palette.esc"),
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
    recent-key="cursio.palette.recent"
    :group-limit="6"
    preview-pane
  >
    <template #preview="{ command, highlight }">
      <template v-for="p in [previewOf(command)]" :key="command.id">
        <LpPalettePreview v-if="p?.kind === 'page'">
          <LpPaletteHeader :eyebrow="t('palette.page')" :icon="command.icon" :title="highlight(command.label)" :lead="t(p.lead)" />
          <LpPaletteStats v-if="p.path === '/'" :items="statusStats" :columns="3" />
          <LpPaletteFacts v-else-if="pageFacts(p.path).length" :items="pageFacts(p.path)" />
        </LpPalettePreview>

        <LpPalettePreview v-else-if="p?.kind === 'peer'">
          <LpPaletteHeader
            :eyebrow="p.peer.link ? t(`kind.${p.peer.link.device_kind}`) : t('palette.peers')"
            :icon="peerIcon(p.peer)"
            :title="highlight(p.peer.name)"
            :subtitle="p.peer.id"
            :lead="p.peer.link ? undefined : t('peer.noLink')"
          />
          <LpPaletteFacts :items="peerFacts(p.peer)" />
        </LpPalettePreview>

        <LpPalettePreview v-else-if="p?.kind === 'module'">
          <LpPaletteHeader
            :eyebrow="t('palette.module')"
            :icon="MODULES.find((m) => m.key === p.key)?.icon"
            :title="highlight(command.label)"
            :lead="t(MODULES.find((m) => m.key === p.key)?.hint ?? '')"
          />
          <LpPaletteFacts :items="moduleFacts(p.key)" />
        </LpPalettePreview>

        <LpPalettePreview v-else-if="p?.kind === 'theme'">
          <LpPaletteHeader :eyebrow="t('settings.themeTitle')" icon="lucide:palette" :title="highlight(p.set.name)" />
          <LpPaletteSwatches :background="p.set.colors.surface" :colors="swatches(p.set)" :label="p.set.name" />
          <LpPaletteFacts
            :items="[
              { icon: 'lucide:sun-moon', label: t('palette.mode'), value: p.set.mode === 'dark' ? t('palette.dark') : t('palette.light') },
              { icon: 'lucide:check', label: t('palette.current'), value: theme.currentName.value === p.set.name ? t('palette.on') : t('palette.off'), current: theme.currentName.value === p.set.name },
            ]"
          />
        </LpPalettePreview>

        <LpPalettePreview v-else-if="p?.kind === 'problem'">
          <LpPaletteHeader
            :eyebrow="t(`severity.${p.problem.severity}`)"
            :icon="SEVERITY_ICON[p.problem.severity]"
            :title="highlight(p.problem.summary)"
            :lead="p.problem.hint ? highlight(p.problem.hint) : undefined"
          />
          <LpPaletteFacts
            :items="[{ icon: 'lucide:clock', label: t('palette.since'), value: new Date(toMillis(p.problem.since)).toLocaleString() }]"
          />
        </LpPalettePreview>

        <LpPalettePreview v-else-if="p?.kind === 'logs'">
          <LpPaletteHeader :eyebrow="t('palette.node')" icon="lucide:scroll-text" :title="highlight(command.label)" />
          <LpKeys combo="Ctrl+L" :label="t('palette.toggleLogs')" />
          <LpKeys combo="/" :label="t('palette.logSearch')" />
        </LpPalettePreview>

        <LpPalettePreview v-else-if="p?.kind === 'restart' && status">
          <LpPaletteHeader :eyebrow="t('palette.node')" icon="lucide:rotate-cw" :title="highlight(command.label)" :lead="t('settings.restartBody')" />
          <LpPaletteFacts
            v-if="status.restart_needed.length"
            :items="status.restart_needed.map((field) => ({ icon: 'lucide:hourglass', label: field, mono: true }))"
          />
        </LpPalettePreview>

        <LpPalettePreview v-else-if="p?.kind === 'start'">
          <LpPaletteHeader :eyebrow="t('palette.node')" icon="lucide:play" :title="highlight(command.label)" :lead="t('node.notRunningBody')" />
        </LpPalettePreview>

        <LpPalettePreview v-else-if="p?.kind === 'update' && status?.update.ready">
          <LpPaletteHeader
            :eyebrow="t('update.title')"
            icon="lucide:download"
            :title="highlight(command.label)"
            :lead="t('update.applyBody')"
          />
          <LpPaletteFacts
            :items="[
              { icon: 'lucide:package', label: t('update.title'), value: status.update.ready.version, mono: true },
              { icon: 'lucide:server', label: t('palette.updateFrom'), value: status.update.ready.from },
            ]"
          />
        </LpPalettePreview>
      </template>
    </template>
  </LpCommandPalette>
  <LpConfirmDialog
    v-model:open="confirmRestart"
    icon="lucide:rotate-cw"
    :title="t('settings.restartTitle')"
    :description="t('settings.restartBody')"
    :confirm-label="t('settings.restart')"
    :cancel-label="t('common.cancel')"
    @confirm="restartNode"
  />
  <LpConfirmDialog
    v-model:open="confirmUpdate"
    icon="lucide:download"
    :title="t('update.applyTitle', { version: status?.update.ready?.version ?? '' })"
    :description="t('update.applyBody')"
    :confirm-label="t('update.apply')"
    :cancel-label="t('common.cancel')"
    @confirm="switchVersion"
  />
</template>
