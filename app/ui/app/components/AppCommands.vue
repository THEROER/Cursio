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
import { presets, useHotkeys, useTheme, type Command, type PaletteFact, type RecentEntry, type TokenSet } from "@leavepulse/ui"
import { PAGE, type Conversation, type MessagesPeer } from "~/composables/useMessages"
import { toMillis, type CallAction, type ModuleSwitch, type PeerView, type Problem } from "~/composables/useNode"

/** The switcher's own list, in its order, so both offer the same themes. */
const THEMES: TokenSet[] = Object.values(presets)

const { t } = useI18n()
const { status, notRunning, start, restart, setModule, applyUpdate, callAction } = useNode()
const messages = useMessages()
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
  /**
   * Words people type for it, Russian and Ukrainian among them: the UI is in
   * English, the person at it often thinks in another language, and a
   * palette that finds "сообщения" nowhere reads as one that cannot search.
   */
  keywords: string[]
  only?: "desktop" | "mobile"
  /** Listed only while this holds, as the sidebar lists it. */
  when?: () => boolean
}

const ALL_PAGES: Page[] = [
  { path: "/", label: "nav.status", icon: "lucide:activity", lead: "palette.pageStatus", keywords: ["peers", "latency", "machines", "статус", "состояние", "стан", "машины", "пристрої"] },
  { path: "/placement", label: "nav.placement", icon: "lucide:layout-grid", lead: "placement.description", keywords: ["position", "grid", "layout", "monitors", "расположение", "сетка", "розташування", "сітка"], only: "desktop" },
  { path: "/pair", label: "nav.pair", icon: "lucide:link", lead: "pair.description", keywords: ["code", "pairing", "add machine", "сопряжение", "пара", "код", "додати", "підключити"] },
  { path: "/touchpad", label: "nav.touchpad", icon: "lucide:mouse-pointer-click", lead: "app.subtitle", keywords: ["mouse", "pointer", "мышь", "миша", "тачпад"], only: "mobile" },
  { path: "/messages", label: "nav.messages", icon: "lucide:message-square", lead: "messages.description", keywords: ["sms", "texts", "chat", "conversations", "сообщения", "смс", "переписка", "повідомлення", "листування"], only: "desktop", when: () => !!status.value?.messages?.length },
  { path: "/gallery", label: "nav.gallery", icon: "lucide:images", lead: "gallery.description", keywords: ["photos", "pictures", "videos", "camera", "фото", "галерея", "картинки", "видео", "відео", "світлини"], when: () => !!status.value?.gallery?.length },
  { path: "/settings", label: "nav.settings", icon: "lucide:settings", lead: "palette.pageSettings", keywords: ["autostart", "version", "volume", "preferences", "настройки", "параметры", "налаштування"] },
]

const PAGES = computed(() => {
  const here = isMobile.value ? "mobile" : "desktop"
  return ALL_PAGES.filter((p) => (!p.only || p.only === here) && (!p.when || p.when()))
})

/** A section of the settings page, reached by its anchor. */
interface SettingsSection {
  anchor: string
  label: string
  icon: string
  keywords: string[]
  only?: "desktop" | "mobile"
}

const SETTINGS: SettingsSection[] = [
  { anchor: "name", label: "settings.nameTitle", icon: "lucide:tag", keywords: ["rename", "hostname", "имя", "название", "ім'я", "назва"] },
  { anchor: "theme", label: "settings.themeTitle", icon: "lucide:palette", keywords: ["dark", "light", "colors", "тема", "тёмная", "светлая", "темна", "світла"] },
  { anchor: "audio", label: "settings.micGate", icon: "lucide:mic", keywords: ["microphone", "gate", "noise", "quiet", "voice", "микрофон", "шум", "тихо", "голос", "мікрофон"] },
  { anchor: "calls", label: "settings.callsTitle", icon: "lucide:phone", keywords: ["bluetooth", "headset", "hands-free", "voice", "звонки", "блютуз", "гарнитура", "дзвінки", "гарнітура"], only: "desktop" },
  { anchor: "phone-audio", label: "phoneAudio.title", icon: "lucide:speaker", keywords: ["sound", "speaker", "звук", "колонка"], only: "mobile" },
  { anchor: "phone-screen", label: "phoneScreen.title", icon: "lucide:cast", keywords: ["screen", "mirror", "cast", "экран", "трансляция", "екран"], only: "mobile" },
  { anchor: "standby", label: "settings.standbyTitle", icon: "lucide:bluetooth", keywords: ["bluetooth", "wifi", "backup", "резерв", "блютуз", "вайфай"], only: "mobile" },
  { anchor: "playing", label: "streams.title", icon: "lucide:volume-2", keywords: ["volume", "sound", "громкость", "звук", "гучність"] },
  { anchor: "microphones", label: "streams.microphones", icon: "lucide:mic", keywords: ["микрофоны", "мікрофони"] },
  { anchor: "autostart", label: "settings.autostartTitle", icon: "lucide:power", keywords: ["login", "boot", "tray", "startup", "автозапуск", "автозавантаження"], only: "desktop" },
  { anchor: "node", label: "settings.nodeTitle", icon: "lucide:server", keywords: ["config", "version", "build", "узел", "конфиг", "версия", "вузол", "версія"] },
]

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
    actions: peerMenu(peerRefOf(p)).map((item, i) => ({
      id: `peer:${p.id}:action-${i}`,
      label: item.label,
      icon: item.icon,
      disabled: item.disabled,
      onSelect: item.onSelect,
    })),
  }
}

/**
 * The same actions as rows of their own, found by what they do: "ring",
 * "screen" or the phone's name reaches them without first finding the
 * machine and pressing Tab, which nobody guesses. Hidden until typed for, so
 * the empty palette is not a wall of every machine's every action.
 */
function peerActionCommands(p: PeerView): Command[] {
  return peerMenu(peerRefOf(p))
    .filter((item) => !item.danger)
    .map((item, i) => ({
      id: `peeract:${p.id}:${i}`,
      group: t("palette.machineActions"),
      label: `${item.label} — ${p.name}`,
      icon: item.icon,
      keywords: [p.name, ...ACTION_WORDS[item.icon ?? ""] ?? []],
      disabled: item.disabled,
      hidden: true,
      noPreview: true,
      onSelect: item.onSelect,
    }))
}

/** Other words for each module's switch. */
const MODULE_WORDS: Partial<Record<ModuleSwitch, string[]>> = {
  input: ["mouse", "keyboard", "cursor", "мышь", "клавиатура", "курсор", "миша", "клавіатура"],
  clipboard: ["copy", "paste", "буфер", "копировать", "вставить", "копіювати"],
  audio: ["sound", "microphone", "звук", "аудио", "микрофон", "аудіо", "мікрофон"],
  files: ["disks", "drives", "mount", "файлы", "диски", "файли"],
  usb: ["device", "adb", "устройство", "пристрій"],
  remote_screen: ["discord", "obs", "stream", "экран", "стрим", "екран"],
  screen_host: ["screen", "share", "экран", "показ", "екран"],
}

/** Other words for a machine action, keyed by the action's icon. */
const ACTION_WORDS: Record<string, string[]> = {
  "lucide:bell-ring": ["find", "lost", "найти", "потерял", "звонить", "знайти", "загубив"],
  "lucide:cast": ["mirror", "scrcpy", "экран", "трансляция", "показать", "екран"],
  "lucide:scroll-text": ["log", "логи", "журнал"],
  "lucide:copy": ["id", "copy", "копировать"],
  "lucide:link": ["pair", "сопрячь", "підключити"],
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

/** Phones that place calls for this computer now. */
const callingPhones = computed(() => (status.value?.calls ?? []).filter((p) => !p.unavailable).map((p) => p.name))

async function dial(phone: string, number: string) {
  try {
    await callAction(phone, { action: "dial", number })
  } catch (e) {
    toast.failure(e, { title: t("messages.callFailed") })
  }
}

/** A typed phone number: digits with the usual separators, at least three of them. */
function asNumber(query: string): string | null {
  const trimmed = query.trim()
  if (!/^\+?[\d\s\-().]+$/.test(trimmed)) return null
  return (trimmed.match(/\d/g)?.length ?? 0) >= 3 ? trimmed.replace(/[\s\-().]/g, "") : null
}

/** Leading words that say what to do with whoever follows: "call mom". */
const CALL_WORDS = ["call", "dial", "позвонить", "позвони", "набрать", "звонок", "подзвонити", "зателефонувати", "дзвінок"]
const TEXT_WORDS = ["text", "sms", "message", "write", "написать", "напиши", "смс", "сообщение", "написати", "повідомлення"]

function splitVerb(words: string[]): { verb: "call" | "text" | null; rest: string[] } {
  const first = words[0]?.toLowerCase() ?? ""
  if (CALL_WORDS.includes(first)) return { verb: "call", rest: words.slice(1) }
  if (TEXT_WORDS.includes(first)) return { verb: "text", rest: words.slice(1) }
  return { verb: null, rest: words }
}

/**
 * Each phone's conversations, the first pages of them, kept until the phone
 * says something changed: typing asks on every keystroke, and the phone
 * should be asked once per change rather than once per letter.
 */
const CONVERSATION_PAGES = 3
const conversationCache = new Map<string, { revision: number; list: Promise<Conversation[]> }>()

function conversationsOf(peer: MessagesPeer): Promise<Conversation[]> {
  const cached = conversationCache.get(peer.peer)
  if (cached && cached.revision === peer.revision) return cached.list
  const list = (async () => {
    const all: Conversation[] = []
    for (let page = 0; page < CONVERSATION_PAGES; page++) {
      const next = await messages.conversations(peer.peer, all.at(-1)?.date_ms)
      all.push(...next)
      if (next.length < PAGE) break
    }
    return all
  })()
  // A failed read is not kept: the next keystroke asks again.
  list.catch(() => conversationCache.delete(peer.peer))
  conversationCache.set(peer.peer, { revision: peer.revision, list })
  return list
}

function conversationName(c: Conversation): string {
  return (c.names.length ? c.names : c.addresses).join(", ") || t("messages.unknown")
}

/** What a remembered conversation or number needs to be rebuilt. */
type Recalled =
  | { kind: "conversation"; peer: string; phone: string; thread: number; address: string; name: string }
  | { kind: "number"; peer: string | null; phone: string | null; number: string }

function openConversation(r: Extract<Recalled, { kind: "conversation" }>) {
  void navigateTo({ path: "/messages", query: { phone: r.peer, thread: String(r.thread), address: r.address, name: r.name } })
}

function conversationCommand(r: Extract<Recalled, { kind: "conversation" }>, c?: Conversation, verb?: "call" | "text" | null): Command {
  const canCall = !!r.address && callingPhones.value.includes(r.phone) && (c?.addresses.length ?? 1) === 1
  const call = () => void dial(r.phone, r.address)
  const actions: Command[] = [
    { id: `sms:${r.peer}:${r.thread}:open`, label: t("palette.openConversation"), icon: "lucide:message-square", onSelect: () => openConversation(r) },
  ]
  if (canCall) actions.unshift({ id: `sms:${r.peer}:${r.thread}:call`, label: t("messages.call"), icon: "lucide:phone", onSelect: call })
  const calling = verb === "call" && canCall
  return {
    id: `sms:${r.peer}:${r.thread}`,
    group: t("palette.conversations"),
    label: calling ? t("palette.callWho", { who: r.name }) : r.name,
    description: c && c.names.length && c.addresses.join() !== c.names.join() ? `${c.addresses.join(", ")} · ${r.phone}` : r.phone,
    excerpt: c?.snippet || undefined,
    icon: calling ? "lucide:phone" : (c?.addresses.length ?? 1) > 1 ? "lucide:users" : "lucide:user",
    keywords: c ? [...c.names, ...c.addresses] : [r.address],
    recentData: r,
    noPreview: true,
    actions,
    onSelect: calling ? call : () => openConversation(r),
  }
}

function numberCommands(number: string, verb: "call" | "text" | null): Command[] {
  const list: Command[] = []
  if (verb !== "text") {
    for (const phone of callingPhones.value) {
      list.push({
        id: `dial:${phone}:${number}`,
        group: t("palette.calls"),
        label: t("palette.callNumber", { number, phone }),
        icon: "lucide:phone-outgoing",
        recentData: { kind: "number", peer: null, phone, number } satisfies Recalled,
        noPreview: true,
        onSelect: () => void dial(phone, number),
      })
    }
  }
  if (verb !== "call") {
    for (const p of status.value?.messages ?? []) {
      list.push({
        id: `text:${p.peer}:${number}`,
        group: t("palette.calls"),
        label: t("palette.textNumber", { number, phone: p.name }),
        icon: "lucide:message-square-plus",
        recentData: { kind: "number", peer: p.peer, phone: p.name, number } satisfies Recalled,
        noPreview: true,
        onSelect: () => void navigateTo({ path: "/messages", query: { phone: p.peer, to: number } }),
      })
    }
  }
  return list
}

/**
 * Everything that lives on a phone rather than in the status: a typed number
 * to call or text, and the conversations — by contact, number or what was
 * said — each opening its thread, or calling with "call …" in front.
 */
async function searchPhones(query: string, _signal: AbortSignal, parsed: { include: string[] }): Promise<Command[]> {
  const { verb, rest } = splitVerb(query.trim().split(/\s+/))
  const number = asNumber(rest.join(" "))
  const found: Command[] = number ? numberCommands(number, verb) : []
  const words = parsed.include.filter((w) => !CALL_WORDS.includes(w) && !TEXT_WORDS.includes(w))
  const digits = number ?? ""
  if (!words.length) return found
  const peers = status.value?.messages ?? []
  const lists = await Promise.allSettled(peers.map(conversationsOf))
  lists.forEach((result, i) => {
    if (result.status !== "fulfilled") return
    const peer = peers[i]!
    let hits = 0
    for (const c of result.value) {
      const haystack = [...c.names, ...c.addresses, c.snippet].join(" ").toLowerCase()
      const byNumber = digits.length >= 3 && c.addresses.some((a) => a.replace(/\D/g, "").includes(digits.replace(/\D/g, "")))
      if (!byNumber && !words.every((w) => haystack.includes(w))) continue
      found.push(
        conversationCommand(
          { kind: "conversation", peer: peer.peer, phone: peer.name, thread: c.thread_id, address: c.addresses[0] ?? "", name: conversationName(c) },
          c,
          verb,
        ),
      )
      if (++hits >= CONVERSATION_RESULTS) break
    }
  })
  return found
}

const CONVERSATION_RESULTS = 8

/** Rebuilds a remembered conversation or number, if the phone is still here. */
function resolveRecent(entry: RecentEntry): Command | null {
  const r = entry.data as Recalled | undefined
  if (!r) return null
  if (r.kind === "conversation") {
    return (status.value?.messages ?? []).some((p) => p.peer === r.peer) ? conversationCommand(r) : null
  }
  if (r.peer) {
    return (status.value?.messages ?? []).some((p) => p.peer === r.peer) ? numberCommands(r.number, "text").find((c) => c.id === `text:${r.peer}:${r.number}`) ?? null : null
  }
  return r.phone && callingPhones.value.includes(r.phone) ? numberCommands(r.number, "call").find((c) => c.id === `dial:${r.phone}:${r.number}`) ?? null : null
}

/** What can be done with the call that is up, wherever it was answered. */
function callCommands(): Command[] {
  const list: Command[] = []
  for (const phone of status.value?.calls ?? []) {
    const call = phone.calls.find((c) => c.state === "Ringing" || c.state === "Waiting") ?? phone.calls[0]
    if (!call) continue
    const who = call.name ?? call.number ?? t("call.unknown")
    const act = (action: CallAction) => () => void callAction(phone.name, action).catch((e) => toast.failure(e, { title: t("call.failed") }))
    const row = (id: string, label: string, icon: string, action: CallAction, keywords: string[]): Command => ({
      id: `call:${phone.name}:${id}`,
      group: t("palette.call", { who }),
      label,
      icon,
      keywords: [who, phone.name, ...keywords],
      remember: false,
      noPreview: true,
      onSelect: act(action),
    })
    if (call.state === "Ringing" || call.state === "Waiting") {
      list.push(row("answer", t("palette.answer"), "lucide:phone-incoming", { action: "answer" }, ["answer", "принять", "ответить", "відповісти"]))
      list.push(row("decline", t("palette.decline"), "lucide:phone-off", { action: "decline" }, ["decline", "reject", "отклонить", "сбросить", "відхилити"]))
    } else {
      list.push(row("hangup", t("palette.hangUp"), "lucide:phone-off", { action: "hang_up" }, ["hang up", "end", "положить", "завершить", "покласти"]))
      list.push(row("mute", t("palette.mute"), "lucide:mic-off", { action: "mute", muted: true }, ["mute", "микрофон", "мікрофон"]))
      list.push(row("unmute", t("palette.unmute"), "lucide:mic", { action: "mute", muted: false }, ["unmute", "микрофон", "мікрофон"]))
      if (phone.hands_free) {
        list.push(
          phone.audio_here
            ? row("phone", t("call.usePhone"), "lucide:smartphone", { action: "audio_here", here: false }, ["voice", "голос", "телефон"])
            : row("here", t("call.useComputer"), "lucide:monitor-speaker", { action: "audio_here", here: true }, ["voice", "голос", "компьютер", "комп'ютер"]),
        )
      }
    }
  }
  return list
}

const commands = computed<Command[]>(() => {
  const go = t("palette.go")
  const node = t("palette.node")
  const s = status.value
  // A call that is up comes first: it is the one thing that cannot wait.
  const list: Command[] = callCommands()
  list.push(
    ...PAGES.value.map((p) => ({
      id: `page:${p.path}`,
      group: go,
      label: t(p.label),
      icon: p.icon,
      keywords: p.keywords,
      onSelect: () => void navigateTo(p.path),
    })),
  )
  const here = isMobile.value ? "mobile" : "desktop"
  for (const section of SETTINGS.filter((x) => !x.only || x.only === here)) {
    list.push({
      id: `setting:${section.anchor}`,
      group: t("palette.settings"),
      label: t(section.label),
      icon: section.icon,
      keywords: [t("nav.settings"), ...section.keywords],
      hidden: true,
      noPreview: true,
      onSelect: () => void navigateTo({ path: "/settings", hash: `#${section.anchor}` }),
    })
  }

  const peers = (s?.peers ?? []).map(peerCommand)
  list.push(...peers)
  for (const p of s?.peers ?? []) list.push(...peerActionCommands(p))
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
      children: offeredModules(s.available).map((m) => ({
        id: `module:${m.key}`,
        label: t(m.label),
        description: s.modules[m.key] ? t("palette.on") : t("palette.off"),
        icon: s.modules[m.key] ? "lucide:toggle-right" : "lucide:toggle-left",
        disabled: s.pinned.includes(`modules.${m.key}`),
        keepOpen: true,
        onSelect: () => void toggle(m.key),
      })),
    })
    // Each module as a row of its own too, said as what picking it does:
    // "audio" should find "Turn sound off" without knowing the "m " mode.
    for (const m of offeredModules(s.available)) {
      const on = s.modules[m.key]
      list.push({
        id: `moduletop:${m.key}`,
        group: t("palette.modules"),
        label: t(on ? "palette.turnOff" : "palette.turnOn", { module: t(m.label) }),
        icon: on ? "lucide:toggle-right" : "lucide:toggle-left",
        keywords: [m.key, ...(MODULE_WORDS[m.key] ?? [])],
        disabled: s.pinned.includes(`modules.${m.key}`),
        hidden: true,
        keepOpen: true,
        onSelect: () => void toggle(m.key),
      })
    }
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
    case "moduletop":
      return MODULES.some((m) => m.key === key) ? { kind: "module", key: key as ModuleSwitch } : null
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
  if (takesCursor(link.device_kind)) {
    facts.push({ icon: "lucide:layout-grid", label: t("peer.position"), value: `${link.position.x}, ${link.position.y}`, mono: true })
  }
  facts.push(
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
        {
          icon: "lucide:layout-grid",
          label: t("palette.placedPeers"),
          value: s.peers.filter((p) => p.link && takesCursor(p.link.device_kind)).length,
        },
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
    :search="searchPhones"
    :resolve-recent="resolveRecent"
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
