<script setup lang="ts">
import { toMillis, type NearbyMachine, type PairingOutcome, type PairingState, type Reply } from "~/composables/useNode"

const { t } = useI18n()
const { pairShow, pairEnter, pairCancel, pairing } = useNode()
const { isMobile } = usePlatform()
const toast = useCursioToast()

const state = ref<PairingState | null>(null)
const outcome = ref<PairingOutcome | null>(null)
// Who a code could reach. Known before a window opens, so the page can say
// "nobody is visible" before the user shows a code that cannot be used.
const nearby = ref<NearbyMachine[] | null>(null)
const code = ref("")
const now = ref(Date.now())
const confirmCancel = ref(false)

function take(reply: Reply) {
  if (reply.reply !== "pairing") return
  const was = state.value
  state.value = (reply.state as PairingState | null) ?? null
  const last = (reply.last as PairingOutcome | null) ?? null
  // Say how the window closed once, when it closes, not on every poll after.
  if (was && !state.value && last) announce(last)
  outcome.value = last
  // A node older than the list sends none, which is not the same as nobody.
  nearby.value = (reply.nearby as NearbyMachine[] | undefined) ?? null
}

const strangers = computed(() => nearby.value?.filter((m) => !m.paired).length ?? 0)

function announce(o: PairingOutcome) {
  switch (o.outcome) {
    case "paired":
      toast.success(t("pair.paired", { peer: o.peer }))
      break
    case "expired":
      toast.warning(t("pair.expired"))
      break
    case "too_many_attempts":
      toast.error(t("pair.tooMany"))
      break
    default:
      toast.info(t("pair.cancelled"))
  }
}

// The window closes on its own — used, expired, or too many wrong codes — so
// it is watched for as long as this page is open.
let timer: ReturnType<typeof setInterval> | undefined
onMounted(async () => {
  try {
    take(await pairing())
  } catch {
    // The shell already says the node is not running.
  }
  // Polled with no window open too, slower: the list of who is visible
  // changes as machines come and go, and it is what tells the user a code
  // would be wasted.
  let ticks = 0
  timer = setInterval(async () => {
    now.value = Date.now()
    ticks += 1
    if (!state.value && ticks % 3 !== 0) return
    try {
      take(await pairing())
    } catch (e) {
      if (state.value) toast.failure(e, { title: t("pair.failed") })
    }
  }, 1000)
})
onBeforeUnmount(() => clearInterval(timer))

async function run(action: () => Promise<Reply>) {
  try {
    take(await action())
  } catch (e) {
    toast.failure(e, { title: t("pair.failed"), retry: () => run(action) })
  }
}

async function cancel() {
  await run(pairCancel)
  confirmCancel.value = false
}

const remaining = computed(() => {
  if (!state.value) return ""
  const left = Math.max(0, Math.round((toMillis(state.value.expires_at) - now.value) / 1000))
  return `${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")}`
})
</script>

<template>
  <DesktopShell>
    <section class="flex flex-col gap-4">
      <LpSection :title="t('pair.title')" :description="t('pair.description')" icon="lucide:link" />

      <LpCard v-if="state" padded class="flex flex-col items-center gap-3 text-center">
        <template v-if="state.code">
          <p class="text-sm text-muted">{{ t("pair.showThis") }}</p>
          <LpTooltip :content="t('pair.copyHint')">
            <p class="font-mono text-4xl font-semibold tracking-[0.2em] select-all">{{ state.code }}</p>
          </LpTooltip>
        </template>
        <p v-else class="text-sm">{{ t("pair.waiting") }}</p>
        <LpBadge tone="brand">{{ t("pair.expiresIn", { time: remaining }) }}</LpBadge>
        <LpBadge v-if="state.failed_attempts" tone="danger">
          {{ t("pair.failedAttempts", { n: state.failed_attempts }) }}
        </LpBadge>
        <LpButton variant="ghost" size="sm" @click="confirmCancel = true">{{ t("pair.cancel") }}</LpButton>
      </LpCard>

      <div v-else class="grid grid-cols-1 gap-4 md:grid-cols-2">
        <LpCard padded class="flex flex-col gap-3">
          <h3 class="flex items-center gap-2 text-sm font-semibold">
            <LpIcon name="lucide:qr-code" :size="16" class="text-brand" />{{ t("pair.showTitle") }}
          </h3>
          <p class="text-xs text-muted">{{ t("pair.showHint") }}</p>
          <LpButton variant="solid" class="mt-auto" @click="run(pairShow)">{{ t("pair.show") }}</LpButton>
        </LpCard>
        <LpCard padded class="flex flex-col gap-3">
          <h3 class="flex items-center gap-2 text-sm font-semibold">
            <LpIcon name="lucide:keyboard" :size="16" class="text-brand" />{{ t("pair.enterTitle") }}
          </h3>
          <p class="text-xs text-muted">{{ t("pair.enterHint") }}</p>
          <form class="flex flex-col gap-3" @submit.prevent="run(() => pairEnter(code))">
            <!-- Eight medium cells are wider than a phone held upright. -->
            <LpOtpInput
              v-model="code"
              :length="8"
              alphanumeric
              :size="isMobile ? 'sm' : 'md'"
              :class="isMobile ? 'gap-1' : ''"
            />
            <LpButton type="submit" variant="solid" :disabled="code.length < 8">{{ t("pair.enter") }}</LpButton>
          </form>
        </LpCard>
      </div>

      <LpAlert v-if="nearby && strangers === 0" variant="warning" :title="t('pair.nobodyTitle')">
        <p class="text-sm">{{ t("pair.nobodyBody") }}</p>
        <p v-if="!isMobile" class="mt-2 font-mono text-xs text-muted">{{ t("pair.nobodyCli") }}</p>
      </LpAlert>

      <LpCard v-if="nearby && nearby.length" padded class="flex flex-col gap-3">
        <h3 class="flex items-center gap-2 text-sm font-semibold">
          <LpIcon name="lucide:radar" :size="16" class="text-brand" />{{ t("pair.nearbyTitle") }}
        </h3>
        <p class="text-xs text-muted">{{ t("pair.nearbyHint") }}</p>
        <ul class="flex flex-col divide-y divide-line">
          <li v-for="m in nearby" :key="m.id" class="flex items-center justify-between gap-3 py-2">
            <div class="min-w-0">
              <p class="truncate text-sm font-medium">{{ m.name }}</p>
              <p class="truncate font-mono text-xs text-muted">{{ m.addr }}</p>
            </div>
            <LpBadge :tone="m.paired ? 'neutral' : 'success'">
              {{ m.paired ? t("pair.nearbyPaired") : t("pair.nearbyNew") }}
            </LpBadge>
          </li>
        </ul>
      </LpCard>

      <LpCodeBlock v-if="!isMobile" :model-value="t('pair.cliCommands')" lang="bash" :title="t('pair.cli')" />
    </section>

    <LpConfirmDialog
      v-model:open="confirmCancel"
      danger
      icon="lucide:link-2-off"
      :title="t('pair.cancelTitle')"
      :description="t('pair.cancelBody')"
      :confirm-label="t('pair.cancel')"
      :cancel-label="t('pair.keepOpen')"
      @confirm="cancel"
    />
  </DesktopShell>
</template>
