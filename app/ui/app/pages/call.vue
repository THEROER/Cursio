<script setup lang="ts">
import { getCurrentWindow } from "@tauri-apps/api/window"
import type { CallAction, PhoneCall, PhoneCalls } from "~/composables/useNode"

// The window is transparent so the card's rounded corners show; the page
// behind the card must be too, or the theme's background fills them in.
useHead({
  htmlAttrs: { style: "background: transparent" },
  bodyAttrs: { style: "background: transparent" },
})

/**
 * A phone's call, alone in the small call window (`call_window.rs`).
 *
 * Everything shown comes from the node's status, so the window follows the
 * call wherever it was acted on — answered on the phone, ended by the other
 * side — and hides itself once no call is left. The buttons that move the
 * voice exist only for a phone reached over Bluetooth hands-free: over the
 * mesh the voice never leaves the phone, and a button that cannot move it
 * would only mislead.
 */
const { t } = useI18n()
const { status, callAction } = useNode()
const toast = useCursioToast()

const phones = computed<PhoneCalls[]>(() => (status.value?.calls ?? []).filter((p) => p.calls.length > 0))
/** A ringing call is the one that needs an answer, whichever phone it is on. */
const current = computed<{ phone: PhoneCalls; call: PhoneCall } | null>(() => {
  for (const phone of phones.value) {
    const ringing = phone.calls.find((c) => c.state === "Ringing" || c.state === "Waiting")
    if (ringing) return { phone, call: ringing }
  }
  const phone = phones.value[0]
  return phone ? { phone, call: phone.calls[0]! } : null
})

const incoming = computed(() => current.value?.call.state === "Ringing" || current.value?.call.state === "Waiting")
const who = computed(() => current.value?.call.name ?? current.value?.call.number ?? t("call.unknown"))
const subtitle = computed(() => (current.value?.call.name && current.value.call.number ? current.value.call.number : null))

/** When each call was first seen up, for the timer; the node does not say. */
const since = new Map<number, number>()
const now = ref(Date.now())
let tick: ReturnType<typeof setInterval> | undefined
onMounted(() => (tick = setInterval(() => (now.value = Date.now()), 1000)))
onBeforeUnmount(() => clearInterval(tick))

const elapsed = computed(() => {
  const call = current.value?.call
  if (!call || call.state !== "Active") return null
  if (!since.has(call.id)) since.set(call.id, Date.now())
  const seconds = Math.max(0, Math.floor((now.value - since.get(call.id)!) / 1000))
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`
})

const stateText = computed(() => {
  switch (current.value?.call.state) {
    case "Ringing":
      return t("call.incoming")
    case "Waiting":
      return t("call.waiting")
    case "Dialing":
    case "Alerting":
      return t("call.calling")
    case "Held":
      return t("call.held")
    default:
      return elapsed.value ?? t("call.active")
  }
})

const voiceText = computed(() => {
  const phone = current.value?.phone
  if (!phone?.hands_free || incoming.value) return null
  return phone.audio_here ? t("call.voiceHere") : t("call.voicePhone")
})

/** Mute is the phone's state, but the phone does not report it back. */
const muted = ref(false)
const busy = ref(false)

async function act(action: CallAction) {
  const phone = current.value?.phone.name
  if (!phone || busy.value) return
  busy.value = true
  try {
    await callAction(phone, action)
    if (action.action === "mute") muted.value = action.muted
  } catch (e) {
    toast.failure(e, { title: t("call.failed") })
  } finally {
    busy.value = false
  }
}

async function hide() {
  await getCurrentWindow()
    .hide()
    .catch(() => {})
}

// No call left: the window goes with it, after a moment so "call ended" is
// not a flash. A call coming back in the meantime keeps it.
let closing: ReturnType<typeof setTimeout> | undefined
watch(
  current,
  (value) => {
    clearTimeout(closing)
    if (value) return
    muted.value = false
    since.clear()
    closing = setTimeout(() => void hide(), 1500)
  },
  { immediate: false },
)
</script>

<template>
  <div class="flex h-screen flex-col overflow-hidden rounded-card border border-line bg-surface text-ink select-none">
    <header data-tauri-drag-region class="flex items-center gap-2 px-3 pt-2.5 pb-1">
      <CursioLogo :size="16" class="pointer-events-none shrink-0 text-brand" />
      <span data-tauri-drag-region class="min-w-0 flex-1 truncate text-xs text-ink-muted">
        {{ current?.phone.name ?? t("call.title") }}
      </span>
      <LpButton variant="muted" size="xs" square :aria-label="t('window.close')" @click="hide">
        <LpIcon name="lucide:x" :size="14" />
      </LpButton>
    </header>

    <main v-if="current" data-tauri-drag-region class="flex flex-1 flex-col justify-center gap-0.5 px-4">
      <p class="truncate text-lg font-semibold">{{ who }}</p>
      <p v-if="subtitle" class="truncate text-xs text-ink-muted">{{ subtitle }}</p>
      <p class="flex items-center gap-1.5 text-sm text-ink-muted">
        <LpIcon :name="incoming ? 'lucide:phone-incoming' : 'lucide:phone'" :size="14" />
        <span class="tabular-nums">{{ stateText }}</span>
        <span v-if="voiceText">· {{ voiceText }}</span>
      </p>
    </main>
    <main v-else data-tauri-drag-region class="flex flex-1 items-center px-4 text-sm text-ink-muted">
      {{ t("call.ended") }}
    </main>

    <footer v-if="current" class="flex gap-2 px-3 pb-3">
      <template v-if="incoming">
        <LpButton variant="danger" class="flex-1" :disabled="busy" @click="act({ action: 'decline' })">
          <LpIcon name="lucide:phone-off" :size="16" />
          {{ t("call.decline") }}
        </LpButton>
        <LpButton variant="action" class="flex-1" :disabled="busy" @click="act({ action: 'answer' })">
          <LpIcon name="lucide:phone" :size="16" />
          {{ t("call.answer") }}
        </LpButton>
      </template>
      <template v-else>
        <LpButton
          variant="soft"
          square
          :disabled="busy"
          :aria-label="muted ? t('call.unmute') : t('call.mute')"
          @click="act({ action: 'mute', muted: !muted })"
        >
          <LpIcon :name="muted ? 'lucide:mic-off' : 'lucide:mic'" :size="16" />
        </LpButton>
        <LpButton
          v-if="current.phone.hands_free"
          variant="outline"
          class="flex-1"
          :disabled="busy"
          @click="act({ action: 'audio_here', here: !current.phone.audio_here })"
        >
          <LpIcon :name="current.phone.audio_here ? 'lucide:smartphone' : 'lucide:monitor-speaker'" :size="16" />
          {{ current.phone.audio_here ? t("call.usePhone") : t("call.useComputer") }}
        </LpButton>
        <LpButton variant="danger" :class="current.phone.hands_free ? '' : 'flex-1'" :disabled="busy" @click="act({ action: 'hang_up' })">
          <LpIcon name="lucide:phone-off" :size="16" />
          {{ t("call.hangUp") }}
        </LpButton>
      </template>
    </footer>
  </div>
</template>
