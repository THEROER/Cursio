<script setup lang="ts">
import { toMillis, type PairingOutcome, type PairingState, type Reply } from "~/composables/useNode"

const { t } = useI18n()
const { pairShow, pairEnter, pairCancel, pairing } = useNode()

const state = ref<PairingState | null>(null)
const outcome = ref<PairingOutcome | null>(null)
const error = ref("")
const code = ref("")
const now = ref(Date.now())

function take(reply: Reply) {
  if (reply.reply !== "pairing") return
  state.value = (reply.state as PairingState | null) ?? null
  outcome.value = (reply.last as PairingOutcome | null) ?? null
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
      error.value = (e as { message?: string }).message ?? String(e)
    }
  }, 1000)
})
onBeforeUnmount(() => clearInterval(timer))

async function run(action: () => Promise<Reply>) {
  error.value = ""
  try {
    take(await action())
  } catch (e) {
    error.value = (e as { message?: string }).message ?? String(e)
  }
}

const remaining = computed(() => {
  if (!state.value) return ""
  const left = Math.max(0, Math.round((toMillis(state.value.expires_at) - now.value) / 1000))
  return `${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")}`
})

const outcomeText = computed(() => {
  const o = outcome.value
  if (!o || state.value) return ""
  switch (o.outcome) {
    case "paired":
      return t("pair.paired", { peer: o.peer })
    case "expired":
      return t("pair.expired")
    case "too_many_attempts":
      return t("pair.tooMany")
    default:
      return t("pair.cancelled")
  }
})
</script>

<template>
  <DesktopShell>
    <section class="flex flex-col gap-3">
      <LpSection :title="t('pair.title')" :description="t('pair.description')" icon="lucide:link" />
      <div v-if="state" class="flex flex-col gap-4">
        <LpCard padded class="flex flex-col items-center gap-3 text-center">
          <template v-if="state.code">
            <p class="text-sm text-muted">{{ t("pair.showThis") }}</p>
            <p class="font-mono text-4xl font-semibold tracking-[0.2em] select-all">{{ state.code }}</p>
          </template>
          <p v-else class="text-sm">{{ t("pair.waiting") }}</p>
          <p class="text-xs text-muted">{{ t("pair.expiresIn", { time: remaining }) }}</p>
          <p v-if="state.failed_attempts" class="text-xs text-danger">
            {{ t("pair.failedAttempts", { n: state.failed_attempts }) }}
          </p>
          <LpButton variant="ghost" size="sm" @click="run(pairCancel)">{{ t("pair.cancel") }}</LpButton>
        </LpCard>
      </div>

      <div v-else class="grid grid-cols-1 gap-4 md:grid-cols-2">
        <LpCard padded class="flex flex-col gap-3">
          <h3 class="text-sm font-semibold">{{ t("pair.showTitle") }}</h3>
          <p class="text-xs text-muted">{{ t("pair.showHint") }}</p>
          <LpButton variant="solid" class="mt-auto" @click="run(pairShow)">{{ t("pair.show") }}</LpButton>
        </LpCard>
        <LpCard padded class="flex flex-col gap-3">
          <h3 class="text-sm font-semibold">{{ t("pair.enterTitle") }}</h3>
          <p class="text-xs text-muted">{{ t("pair.enterHint") }}</p>
          <form class="flex flex-col gap-3" @submit.prevent="run(() => pairEnter(code))">
            <LpOtpInput v-model="code" :length="8" alphanumeric />
            <LpButton type="submit" variant="solid" :disabled="code.length < 8">{{ t("pair.enter") }}</LpButton>
          </form>
        </LpCard>
      </div>

      <LpAlert v-if="outcomeText" :variant="outcome?.outcome === 'paired' ? 'success' : 'warning'" class="mt-4">
        {{ outcomeText }}
      </LpAlert>
      <LpAlert v-if="error" variant="danger" class="mt-4">{{ error }}</LpAlert>
      <p class="mt-4 text-xs text-muted">{{ t("pair.cli") }}</p>
    </section>
  </DesktopShell>
</template>
