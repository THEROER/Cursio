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
const revision = computed(() => phones.value.find((p) => p.peer === phoneId.value)?.revision ?? 0)

const list = ref<Conversation[]>([])
const listEnd = ref(false)
const loadingList = ref(false)

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

async function loadList(more = false) {
  if (!phoneId.value) return
  loadingList.value = true
  try {
    const before = more ? list.value.at(-1)?.date_ms : undefined
    const page = await messages.conversations(phoneId.value, before)
    list.value = more ? [...list.value, ...page] : page
    listEnd.value = page.length < PAGE
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

function choose(c: Conversation) {
  composingNew.value = false
  open.value = c
  thread.value = []
  void loadThread()
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

const recipients = computed(() =>
  composingNew.value
    ? draftTo.value.split(",").map((a) => a.trim()).filter(Boolean)
    : (open.value?.addresses ?? []),
)

async function send() {
  const text = body.value.trim()
  if (!text || !recipients.value.length || sending.value) return
  sending.value = true
  try {
    await messages.send(phoneId.value, recipients.value, text)
    body.value = ""
    toast.success(t("messages.handedOver"))
  } catch (e) {
    toast.failure(e, { title: t("messages.sendFailed") })
  } finally {
    sending.value = false
  }
}

function onKey(event: KeyboardEvent) {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault()
    void send()
  }
}

watch(phoneId, () => {
  open.value = null
  composingNew.value = false
  thread.value = []
  void loadList()
})
watch(revision, () => {
  void loadList()
  if (open.value) void loadThread()
})

function who(c: Conversation): string {
  return (c.names.length ? c.names : c.addresses).join(", ") || t("messages.unknown")
}

function when(ms: number): string {
  const d = new Date(ms)
  const today = new Date()
  return d.toDateString() === today.toDateString()
    ? d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : d.toLocaleDateString([], { day: "numeric", month: "short" })
}

const bubble: Record<TextMessage["kind"], string> = {
  Inbox: "self-start bg-surface-soft",
  Sent: "self-end bg-brand text-ink-inverse",
  Outbox: "self-end bg-brand-soft",
  Failed: "self-end bg-danger-soft",
  Other: "self-end bg-surface-soft",
}
</script>

<template>
  <DesktopShell>
    <section class="flex h-full flex-col gap-4">
      <div class="flex flex-wrap items-center justify-between gap-3">
        <LpSection :title="t('messages.title')" :description="t('messages.description')" icon="lucide:message-square" />
        <LpSegmented v-if="phones.length > 1" v-model="phoneId" :options="phoneOptions" size="sm" />
      </div>

      <LpCard v-if="!phones.length" padded>
        <LpEmptyState icon="lucide:smartphone" :title="t('messages.noPhone')" :description="t('messages.noPhoneHint')" />
      </LpCard>

      <div v-else class="grid min-h-[28rem] grid-cols-1 gap-4 md:grid-cols-[18rem_1fr]">
        <LpCard class="flex max-h-[70vh] flex-col overflow-hidden">
          <div class="flex items-center justify-between gap-2 border-b border-line p-2">
            <span class="px-1 text-sm font-semibold">{{ t("messages.conversations") }}</span>
            <LpButton size="sm" variant="soft" @click="startNew">
              <LpIcon name="lucide:pen-square" :size="14" />{{ t("messages.new") }}
            </LpButton>
          </div>
          <ul class="flex-1 divide-y divide-line overflow-y-auto">
            <li v-for="c in list" :key="c.thread_id">
              <button
                class="flex w-full flex-col gap-0.5 px-3 py-2 text-left hover:bg-surface-soft"
                :class="open?.thread_id === c.thread_id ? 'bg-surface-soft' : ''"
                @click="choose(c)"
              >
                <span class="flex items-center gap-2">
                  <span class="min-w-0 flex-1 truncate text-sm" :class="c.unread ? 'font-semibold' : 'font-medium'">{{ who(c) }}</span>
                  <span class="shrink-0 text-xs text-muted">{{ when(c.date_ms) }}</span>
                </span>
                <span class="truncate text-xs text-muted">{{ c.snippet }}</span>
              </button>
            </li>
            <li v-if="!list.length && !loadingList" class="p-3 text-xs text-muted">{{ t("messages.none") }}</li>
          </ul>
          <LpButton v-if="list.length && !listEnd" variant="ghost" size="sm" :loading="loadingList" @click="loadList(true)">
            {{ t("messages.older") }}
          </LpButton>
        </LpCard>

        <LpCard class="flex max-h-[70vh] flex-col overflow-hidden">
          <template v-if="open || composingNew">
            <div class="border-b border-line p-3">
              <LpInput v-if="composingNew" v-model="draftTo" size="sm" icon="lucide:phone" :placeholder="t('messages.to')" />
              <p v-else class="truncate text-sm font-semibold">
                {{ who(open!) }}
                <span v-if="open!.names.join() !== open!.addresses.join()" class="font-normal text-muted">· {{ open!.addresses.join(", ") }}</span>
              </p>
            </div>
            <div ref="scroller" class="flex flex-1 flex-col gap-2 overflow-y-auto p-3">
              <LpButton v-if="open && thread.length && !threadEnd" variant="ghost" size="sm" class="self-center" :loading="loadingThread" @click="loadThread(true)">
                {{ t("messages.older") }}
              </LpButton>
              <div
                v-for="m in thread"
                :key="`${m.mms ? 'mms' : 'sms'}-${m.id}`"
                class="max-w-[75%] rounded-xl px-3 py-2 text-sm"
                :class="bubble[m.kind]"
              >
                <p class="whitespace-pre-wrap break-words">{{ m.body }}</p>
                <p v-if="m.attachment" class="text-xs italic opacity-80">{{ t("messages.attachment") }}</p>
                <p class="mt-1 text-right text-[10px] opacity-70">
                  {{ when(m.date_ms) }}
                  <template v-if="m.kind === 'Outbox'"> · {{ t("messages.sending") }}</template>
                  <template v-else-if="m.kind === 'Failed'"> · {{ t("messages.failed") }}</template>
                </p>
              </div>
            </div>
            <form class="flex items-end gap-2 border-t border-line p-2" @submit.prevent="send">
              <LpTextarea v-model="body" :rows="2" class="flex-1" :placeholder="t('messages.write')" @keydown="onKey" />
              <LpButton type="submit" variant="solid" :loading="sending" :disabled="!body.trim() || !recipients.length">
                <LpIcon name="lucide:send" :size="14" />{{ t("messages.send") }}
              </LpButton>
            </form>
          </template>
          <LpEmptyState v-else compact icon="lucide:messages-square" :title="t('messages.pick')" />
        </LpCard>
      </div>
    </section>
  </DesktopShell>
</template>
