<script setup lang="ts">
import type { SegmentedOption } from "@leavepulse/ui"
import type { Conversation, TextMessage } from "~/composables/useMessages"
import { PAGE } from "~/composables/useMessages"

/**
 * A phone's SMS conversations, read and answered from this desktop.
 *
 * Nothing is kept here: the phone holds the messages and answers each page
 * when asked. When it says something changed (the status's `revision` for it
 * moves) the list and the open conversation are asked for again, which is how
 * a reply that just arrived, or a sent message turning from queued to sent,
 * shows up without a refresh button.
 *
 * Laid out like a messenger rather than a settings page: the two panes fill
 * the window, and below `md` only one of them shows at a time, list first,
 * because a 360 px window cannot hold both and stacking them put the open
 * conversation below the fold.
 */
const { t } = useI18n()
const { status } = useNode()
const messages = useMessages()
const toast = useCursioToast()

const phones = computed(() => status.value?.messages ?? [])
const phoneId = ref<string>("")
watchEffect(() => {
  if (!phones.value.some((p) => p.peer === phoneId.value)) phoneId.value = phones.value[0]?.peer ?? ""
})
const phoneOptions = computed<SegmentedOption[]>(() => phones.value.map((p) => ({ value: p.peer, label: p.name })))
const phoneName = computed(() => phones.value.find((p) => p.peer === phoneId.value)?.name ?? "")
const revision = computed(() => phones.value.find((p) => p.peer === phoneId.value)?.revision ?? 0)

const list = ref<Conversation[]>([])
const listEnd = ref(false)
const loadingList = ref(false)
const listLoaded = ref(false)
const search = ref("")

const open = ref<Conversation | null>(null)
/** A new conversation, before it has a thread: just who it goes to. */
const draftTo = ref("")
const composingNew = ref(false)
const thread = ref<TextMessage[]>([])
const threadEnd = ref(false)
const loadingThread = ref(false)
const body = ref("")
const sending = ref(false)
const scroller = ref<HTMLElement | null>(null)
const composer = ref<{ $el?: HTMLElement } | null>(null)

async function loadList(more = false) {
  if (!phoneId.value) return
  loadingList.value = true
  try {
    const before = more ? list.value.at(-1)?.date_ms : undefined
    const page = await messages.conversations(phoneId.value, before)
    list.value = more ? [...list.value, ...page] : page
    listEnd.value = page.length < PAGE
    listLoaded.value = true
    if (open.value) open.value = list.value.find((c) => c.thread_id === open.value?.thread_id) ?? open.value
  } catch (e) {
    toast.failure(e, { title: t("messages.loadFailed"), retry: () => loadList(more) })
  } finally {
    loadingList.value = false
  }
}

async function loadThread(more = false) {
  if (!phoneId.value || !open.value) return
  loadingThread.value = true
  try {
    const oldest = more ? thread.value[0]?.date_ms : undefined
    const page = await messages.thread(phoneId.value, open.value.thread_id, oldest)
    const ordered = [...page].reverse()
    thread.value = more ? [...ordered, ...thread.value] : ordered
    threadEnd.value = page.length < PAGE
    if (!more) {
      await nextTick()
      scroller.value?.scrollTo({ top: scroller.value.scrollHeight })
    }
  } catch (e) {
    toast.failure(e, { title: t("messages.loadFailed"), retry: () => loadThread(more) })
  } finally {
    loadingThread.value = false
  }
}

function focusComposer() {
  void nextTick(() => composer.value?.$el?.querySelector("textarea")?.focus())
}

function choose(c: Conversation) {
  composingNew.value = false
  open.value = c
  thread.value = []
  void loadThread()
  focusComposer()
  if (c.unread) {
    // Refused unless Cursio is the phone's default SMS app; the phone's own
    // app marks it read when the user next opens it there, so this stays quiet.
    messages.markRead(phoneId.value, c.thread_id).catch(() => undefined)
  }
}

function startNew() {
  composingNew.value = true
  open.value = null
  thread.value = []
  draftTo.value = ""
}

function back() {
  open.value = null
  composingNew.value = false
}

const recipients = computed(() =>
  composingNew.value
    ? draftTo.value.split(",").map((a) => a.trim()).filter(Boolean)
    : (open.value?.addresses ?? []),
)

async function send(text = body.value.trim()) {
  if (!text || !recipients.value.length || sending.value) return
  sending.value = true
  try {
    await messages.send(phoneId.value, recipients.value, text)
    if (text === body.value.trim()) body.value = ""
    toast.success(t("messages.handedOver"))
  } catch (e) {
    toast.failure(e, { title: t("messages.sendFailed") })
  } finally {
    sending.value = false
  }
}

function onKey(event: KeyboardEvent) {
  if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
    event.preventDefault()
    void send()
  }
}

// Immediate: arriving from another page, the shared status is already there,
// so the phone is picked above before this watch exists and it never fires.
watch(
  phoneId,
  () => {
    open.value = null
    composingNew.value = false
    thread.value = []
    listLoaded.value = false
    void loadList()
  },
  { immediate: true },
)
watch(revision, () => {
  void loadList()
  if (open.value) void loadThread()
})

function who(c: Conversation): string {
  return (c.names.length ? c.names : c.addresses).join(", ") || t("messages.unknown")
}

const filtered = computed(() => {
  const q = search.value.trim().toLowerCase()
  if (!q) return list.value
  return list.value.filter((c) =>
    [...c.names, ...c.addresses, c.snippet].some((s) => s.toLowerCase().includes(q)),
  )
})

/** Whether an address is a number rather than a sender name ("NovaPoshta"). */
const isNumber = (s: string) => /^[+\d][\d\s()-]*$/.test(s)

function initials(c: Conversation): string {
  const name = c.names[0] ?? c.addresses[0] ?? ""
  // A group shows its icon, and a bare number has no letters worth showing.
  if (c.addresses.length > 1 || isNumber(name)) return ""
  return name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((w) => [...w][0]?.toUpperCase() ?? "")
    .join("")
}

function avatarIcon(c: Conversation): string | undefined {
  if (c.addresses.length > 1) return "lucide:users"
  return initials(c) ? undefined : "lucide:user"
}

/** The contact's name for one address of the open conversation. */
function nameOf(address: string): string {
  const c = open.value
  if (!c) return address
  const i = c.addresses.indexOf(address)
  return (i >= 0 ? c.names[i] : undefined) ?? address
}

const group = computed(() => (open.value?.addresses.length ?? 0) > 1)
const outgoing = (m: TextMessage) => m.kind !== "Inbox"

const time = (ms: number) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })

function when(ms: number): string {
  const d = new Date(ms)
  const now = new Date()
  if (d.toDateString() === now.toDateString()) return time(ms)
  const days = Math.floor((startOfDay(now) - startOfDay(d)) / 86_400_000)
  if (days === 1) return t("messages.yesterday")
  if (days < 7) return d.toLocaleDateString([], { weekday: "short" })
  return d.toLocaleDateString([], { day: "numeric", month: "short", year: d.getFullYear() === now.getFullYear() ? undefined : "numeric" })
}

function startOfDay(d: Date): number {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()
}

function dayLabel(ms: number): string {
  const d = new Date(ms)
  const now = new Date()
  const days = Math.floor((startOfDay(now) - startOfDay(d)) / 86_400_000)
  if (days === 0) return t("messages.today")
  if (days === 1) return t("messages.yesterday")
  return d.toLocaleDateString([], { weekday: "long", day: "numeric", month: "long", year: d.getFullYear() === now.getFullYear() ? undefined : "numeric" })
}

/** Messages this close together from the same side read as one run. */
const RUN_GAP_MS = 5 * 60_000

interface Row {
  m: TextMessage
  /** First of its day: a date separator goes above it. */
  day: string | null
  /** First of a run: gets the sender's name in a group, and the full gap. */
  first: boolean
  /** Last of a run: carries the time and the tail of the bubble. */
  last: boolean
}

const rows = computed<Row[]>(() =>
  thread.value.map((m, i, all) => {
    const prev = all[i - 1]
    const next = all[i + 1]
    const sameDay = (a?: TextMessage, b?: TextMessage) => !!a && !!b && new Date(a.date_ms).toDateString() === new Date(b.date_ms).toDateString()
    const sameRun = (a?: TextMessage, b?: TextMessage) =>
      !!a && !!b && sameDay(a, b) && outgoing(a) === outgoing(b) && a.address === b.address && Math.abs(b.date_ms - a.date_ms) < RUN_GAP_MS
    return {
      m,
      day: sameDay(prev, m) ? null : dayLabel(m.date_ms),
      first: !sameRun(prev, m),
      last: !sameRun(m, next),
    }
  }),
)

function bubbleClass(r: Row): string {
  const out = outgoing(r.m)
  const tail = r.last ? (out ? "rounded-br-md" : "rounded-bl-md") : ""
  const tone: Record<TextMessage["kind"], string> = {
    Inbox: "bg-surface-soft text-ink",
    Sent: "bg-brand text-ink-inverse",
    Outbox: "bg-brand/60 text-ink-inverse",
    Failed: "border border-danger bg-danger-soft text-ink",
    Other: "bg-surface-soft text-ink",
  }
  return `${tone[r.m.kind]} ${tail}`
}

/**
 * How many SMS the draft becomes. GSM-7 fits 160 per message and 153 when
 * split; anything outside it (Cyrillic, emoji) sends as UCS-2, 70 and 67.
 */
const GSM = /^[\n\r @£$¥èéùìòÇØøÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ!"#¤%&'()*+,\-./0-9:;<=>?¡A-ZÄÖÑÜ§¿a-zäöñüà^{}\\[~\]|€]*$/
const segments = computed(() => {
  const text = body.value
  if (!text) return null
  const gsm = GSM.test(text)
  const length = gsm ? text.length : [...text].reduce((n, ch) => n + (ch.length > 1 ? 2 : 1), 0)
  const [single, part] = gsm ? [160, 153] : [70, 67]
  return { length, count: length <= single ? 1 : Math.ceil(length / part) }
})

const composerRows = computed(() => Math.min(6, Math.max(1, body.value.split("\n").length)))

const showList = computed(() => !(open.value || composingNew.value))
</script>

<template>
  <DesktopShell full-bleed>
    <div v-if="!phones.length" class="flex h-full items-center justify-center p-6">
      <LpEmptyState icon="lucide:smartphone" :title="t('messages.noPhone')" :description="t('messages.noPhoneHint')" />
    </div>

    <div v-else class="grid h-full min-h-0 grid-cols-1 md:grid-cols-[20rem_1fr]">
      <!-- Conversations -->
      <aside
        class="min-h-0 flex-col border-line md:flex md:border-r"
        :class="showList ? 'flex' : 'hidden'"
        :aria-label="t('messages.conversations')"
      >
        <div class="flex flex-col gap-2 border-b border-line p-3">
          <div class="flex items-center gap-2">
            <LpSegmented v-if="phones.length > 1" v-model="phoneId" :options="phoneOptions" size="sm" class="min-w-0 flex-1" />
            <p v-else class="flex min-w-0 flex-1 items-center gap-1.5 truncate text-sm font-semibold">
              <LpIcon name="lucide:smartphone" :size="14" class="shrink-0 text-muted" />{{ phoneName }}
            </p>
            <LpTooltip :content="t('messages.new')">
              <LpButton size="sm" variant="soft" :aria-label="t('messages.new')" @click="startNew">
                <LpIcon name="lucide:square-pen" :size="16" />
              </LpButton>
            </LpTooltip>
          </div>
          <LpInput v-model="search" size="sm" icon="lucide:search" :placeholder="t('messages.search')" />
        </div>

        <ul class="min-h-0 flex-1 overflow-y-auto p-1.5" role="list">
          <template v-if="loadingList && !listLoaded">
            <li v-for="i in 7" :key="i" class="flex items-center gap-3 px-2.5 py-2.5">
              <LpSkeleton rounded="pill" class="size-10 shrink-0" />
              <div class="flex flex-1 flex-col gap-1.5">
                <LpSkeleton class="h-3.5 w-2/3" />
                <LpSkeleton class="h-3 w-full" />
              </div>
            </li>
          </template>
          <li v-for="c in filtered" :key="c.thread_id">
            <button
              data-conversation
              class="group flex w-full items-center gap-3 rounded-lg px-2.5 py-2 text-left outline-none transition-colors hover:bg-surface-soft focus-visible:ring-2 focus-visible:ring-brand"
              :class="open?.thread_id === c.thread_id ? 'bg-brand-soft hover:bg-brand-soft' : ''"
              :aria-current="open?.thread_id === c.thread_id ? 'true' : undefined"
              @click="choose(c)"
            >
              <LpAvatar :fallback="initials(c)" :icon="avatarIcon(c)" size="md" />
              <span class="flex min-w-0 flex-1 flex-col">
                <span class="flex items-baseline gap-2">
                  <span class="min-w-0 flex-1 truncate text-sm" :class="c.unread ? 'font-semibold text-ink' : 'font-medium'">{{ who(c) }}</span>
                  <span class="shrink-0 text-xs" :class="c.unread ? 'font-medium text-brand' : 'text-muted'">{{ when(c.date_ms) }}</span>
                </span>
                <span class="flex items-center gap-2">
                  <span class="min-w-0 flex-1 truncate text-xs" :class="c.unread ? 'text-ink' : 'text-muted'">{{ c.snippet }}</span>
                  <span v-if="c.unread" class="size-2 shrink-0 rounded-full bg-brand" :aria-label="t('messages.unread')" />
                </span>
              </span>
            </button>
          </li>
          <li v-if="listLoaded && !filtered.length" class="py-8">
            <LpEmptyState
              compact
              :icon="search ? 'lucide:search-x' : 'lucide:message-square-dashed'"
              :title="search ? t('messages.noMatch') : t('messages.none')"
            />
          </li>
          <li v-if="list.length && !listEnd && !search" class="flex justify-center py-2">
            <LpButton variant="ghost" size="sm" :loading="loadingList" @click="loadList(true)">{{ t("messages.older") }}</LpButton>
          </li>
        </ul>
      </aside>

      <!-- Conversation -->
      <section class="min-h-0 flex-col md:flex" :class="showList ? 'hidden' : 'flex'">
        <template v-if="open || composingNew">
          <header class="flex items-center gap-3 border-b border-line px-3 py-2.5">
            <LpButton variant="ghost" size="sm" class="md:hidden" :aria-label="t('messages.back')" @click="back">
              <LpIcon name="lucide:arrow-left" :size="16" />
            </LpButton>
            <template v-if="composingNew">
              <LpInput v-model="draftTo" size="sm" icon="lucide:phone" class="flex-1" :placeholder="t('messages.to')" autofocus />
            </template>
            <template v-else>
              <LpAvatar :fallback="initials(open!)" :icon="avatarIcon(open!)" size="sm" />
              <div class="min-w-0 flex-1">
                <p class="truncate text-sm font-semibold">{{ who(open!) }}</p>
                <p v-if="open!.names.join() !== open!.addresses.join() || group" class="truncate text-xs text-muted">
                  {{ open!.addresses.join(", ") }}
                </p>
              </div>
            </template>
          </header>

          <div ref="scroller" class="min-h-0 flex-1 overflow-y-auto px-4 py-3" aria-live="polite">
            <div v-if="open && thread.length && !threadEnd" class="flex justify-center pb-2">
              <LpButton variant="ghost" size="sm" :loading="loadingThread" @click="loadThread(true)">{{ t("messages.older") }}</LpButton>
            </div>
            <div v-if="loadingThread && !thread.length" class="flex flex-col gap-2">
              <LpSkeleton v-for="i in 5" :key="i" rounded="card" class="h-9" :class="i % 2 ? 'w-1/2 self-start' : 'w-2/5 self-end'" />
            </div>
            <LpEmptyState v-else-if="composingNew || (open && !thread.length)" compact icon="lucide:message-circle" :title="t('messages.startHere')" class="py-10" />

            <template v-for="r in rows" :key="`${r.m.mms ? 'mms' : 'sms'}-${r.m.id}`">
              <div v-if="r.day" class="my-3 flex items-center gap-3 text-xs text-muted" role="separator">
                <span class="h-px flex-1 bg-line" />{{ r.day }}<span class="h-px flex-1 bg-line" />
              </div>
              <div class="flex flex-col" :class="[outgoing(r.m) ? 'items-end' : 'items-start', r.first ? 'mt-2' : 'mt-0.5']">
                <span v-if="group && r.first && !outgoing(r.m)" class="mb-0.5 px-3 text-xs font-medium text-muted">{{ nameOf(r.m.address) }}</span>
                <div class="max-w-[min(32rem,80%)] rounded-2xl px-3.5 py-2 text-sm leading-relaxed" :class="bubbleClass(r)">
                  <p v-if="r.m.body" class="whitespace-pre-wrap break-words">{{ r.m.body }}</p>
                  <p v-if="r.m.attachment" class="flex items-center gap-1.5 text-xs opacity-80">
                    <LpIcon name="lucide:image-off" :size="14" />{{ t("messages.attachment") }}
                  </p>
                </div>
                <span v-if="r.last || r.m.kind === 'Failed' || r.m.kind === 'Outbox'" class="mt-1 flex items-center gap-1.5 px-1 text-[11px] text-muted">
                  <template v-if="r.m.kind === 'Outbox'"><LpIcon name="lucide:clock" :size="11" />{{ t("messages.sending") }}</template>
                  <template v-else-if="r.m.kind === 'Failed'">
                    <LpIcon name="lucide:circle-alert" :size="12" class="text-danger" />
                    <span class="text-danger">{{ t("messages.failed") }}</span>
                    <button class="font-medium text-brand underline-offset-2 hover:underline" :disabled="sending" @click="send(r.m.body)">{{ t("messages.retry") }}</button>
                  </template>
                  <template v-else>{{ time(r.m.date_ms) }}</template>
                </span>
              </div>
            </template>
          </div>

          <form class="flex items-end gap-2 border-t border-line p-3" @submit.prevent="send()">
            <div class="flex min-w-0 flex-1 flex-col gap-1">
              <LpTextarea ref="composer" v-model="body" :rows="composerRows" :placeholder="t('messages.write')" :aria-label="t('messages.write')" @keydown="onKey" />
              <span class="flex justify-between px-1 text-[11px] text-muted">
                <span>{{ t("messages.enterHint") }}</span>
                <span v-if="segments">{{ t("messages.segments", { length: segments.length, count: segments.count }, segments.count) }}</span>
              </span>
            </div>
            <LpButton type="submit" variant="solid" class="mb-5" :loading="sending" :disabled="!body.trim() || !recipients.length" :aria-label="t('messages.send')">
              <LpIcon name="lucide:send-horizontal" :size="16" />
            </LpButton>
          </form>
        </template>
        <div v-else class="flex h-full items-center justify-center">
          <LpEmptyState icon="lucide:messages-square" :title="t('messages.pick')" :description="t('messages.pickHint')" />
        </div>
      </section>
    </div>
  </DesktopShell>
</template>
