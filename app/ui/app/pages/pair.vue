<script setup lang="ts">
import { toMillis, type PairingOutcome, type PairingState, type Reply } from "~/composables/useNode"

const { t } = useI18n()
const { pairShow, pairEnter, pairCancel, pairing } = useNode()
const { isMobile } = usePlatform()
const toast = useCursioToast()

const state = ref<PairingState | null>(null)
const outcome = ref<PairingOutcome | null>(null)
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
}

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
  timer = setInterval(async () => {
    now.value = Date.now()
    if (!state.value) return
    try {
      take(await pairing())
    } catch (e) {
      toast.failure(e, { title: t("pair.failed") })
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
