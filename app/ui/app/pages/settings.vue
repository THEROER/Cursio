<script setup lang="ts">
import { invoke } from "@tauri-apps/api/core"
import type { CallsConfig, Reply, ServiceState } from "~/composables/useNode"

const { t } = useI18n()
const { status, restart, config, setMicGate, setName, setCalls } = useNode()
const { isMobile, ready } = usePlatform()
const toast = useCursioToast()

/** `identity::MAX_NAME_CHARS`; the node refuses anything longer. */
const NAME_MAX = 40

const nameDraft = ref("")
const nameBusy = ref(false)

// Follows the node's name, so a rename from elsewhere (the CLI, a restart)
// shows; it never changes while the user types, so their draft survives.
watch(
  () => status.value?.node.name,
  (name) => {
    if (name !== undefined) nameDraft.value = name
  },
  { immediate: true },
)

const nameTrimmed = computed(() => nameDraft.value.trim())
/** Counted in characters, as the node counts, not UTF-16 units. */
const nameError = computed(() => {
  if (!nameTrimmed.value) return t("settings.nameEmpty")
  if ([...nameTrimmed.value].length > NAME_MAX) return t("settings.nameTooLong", { max: NAME_MAX })
  return undefined
})
const nameChanged = computed(() => !!status.value && nameTrimmed.value !== status.value.node.name)
const nameWaitsForRestart = computed(() => !isMobile.value && !!status.value?.restart_needed.includes("name"))

async function saveName() {
  if (!nameChanged.value || nameError.value) return
  nameBusy.value = true
  try {
    await setName(nameTrimmed.value)
    if (isMobile.value) {
      // The phone's links greet peers with the name only when they come up,
      // and nothing here is costly to bring back, so it restarts at once
      // rather than leaving peers on the old name until the next reboot.
      await mobileRestart()
      toast.success(t("settings.renamedPhone"))
    } else {
      toast.success(t("settings.renamed"))
    }
  } catch (e) {
    toast.failure(e, { title: t("settings.renameFailed"), retry: saveName })
  } finally {
    nameBusy.value = false
  }
}

type GateMode = "auto" | "fixed" | "off"
const GATE_MIN = -80
const GATE_MAX = -20

/** `null` until the node's config has been read. */
const gateMode = ref<GateMode | null>(null)
const gateLevel = ref(-55)
const gatePinned = ref(false)
const gateBusy = ref(false)

const gateModes = computed(() => [
  { value: "auto", label: t("settings.micGateAuto") },
  { value: "fixed", label: t("settings.micGateFixed") },
  { value: "off", label: t("settings.micGateOff") },
])

/** Reads the effective value, so a flag pinning it shows what is running. */
async function loadMicGate() {
  try {
    const reply = await config()
    if (reply.reply !== "config") return
    const { effective, pinned } = reply as Extract<Reply, { reply: "config" }>
    const value = effective.audio.mic_gate?.trim() ?? "auto"
    gatePinned.value = pinned.includes("audio.mic_gate")
    if (value === "auto" || value === "off") {
      gateMode.value = value
      return
    }
    const db = Number.parseFloat(value)
    gateMode.value = "fixed"
    if (Number.isFinite(db)) gateLevel.value = Math.round(Math.min(GATE_MAX, Math.max(GATE_MIN, db)))
  } catch (e) {
    toast.failure(e, { title: t("settings.micGateFailed") })
  }
}

watch(
  () => !!status.value,
  (up) => {
    if (up && gateMode.value === null) void loadMicGate()
  },
  { immediate: true },
)

async function saveMicGate() {
  const value = gateMode.value === "fixed" ? String(gateLevel.value) : gateMode.value
  if (!value) return
  gateBusy.value = true
  try {
    const reply = await setMicGate(value)
    if (reply.reply === "saved" && (reply.pinned as string[]).includes("audio.mic_gate")) {
      toast.warning(t("settings.micGatePinned"))
    } else {
      toast.success(t("settings.saved"))
    }
  } catch (e) {
    toast.failure(e, { title: t("settings.micGateFailed"), retry: saveMicGate })
  } finally {
    gateBusy.value = false
  }
}

function setGateMode(mode: string) {
  gateMode.value = mode as GateMode
  void saveMicGate()
}

/** The `[calls]` section as the file has it; `null` until read. */
const calls = ref<CallsConfig | null>(null)
const callsBusy = ref(false)
const callSetup = computed(() => status.value?.call_setup ?? null)

/** "No device chosen", as a value the select can hold: it takes no empty string. */
const DEFAULT_DEVICE = "cursio:default"

const voiceModes = computed(() => [
  { value: "follow", label: t("settings.callsVoiceFollow") },
  { value: "here", label: t("settings.callsVoiceHere") },
])

function deviceOptions(devices: { id: string; name: string }[], chosen: string | null) {
  const options = [{ value: DEFAULT_DEVICE, label: t("settings.callsDefault") }]
  for (const d of devices) options.push({ value: d.id, label: d.name })
  // A device chosen earlier that is not plugged in now is still the setting.
  if (chosen && !devices.some((d) => d.id === chosen)) options.push({ value: chosen, label: chosen })
  return options
}

const outputOptions = computed(() => deviceOptions(callSetup.value?.outputs ?? [], calls.value?.output ?? null))
const inputOptions = computed(() => deviceOptions(callSetup.value?.inputs ?? [], calls.value?.input ?? null))

function serviceLabel(state: ServiceState) {
  if (state.state === "running") return t("settings.callsRunning")
  if (state.state === "failed") return t("settings.callsFailed")
  return t("settings.callsStarting")
}

function serviceTone(state: ServiceState) {
  if (state.state === "running") return "success"
  if (state.state === "failed") return "danger"
  return "neutral"
}

async function loadCalls() {
  try {
    const reply = await config()
    if (reply.reply !== "config") return
    const { file } = reply as Extract<Reply, { reply: "config" }>
    calls.value = file.calls ?? { answer_moves_voice: true, voice: "follow", output: null, input: null }
  } catch (e) {
    toast.failure(e, { title: t("settings.callsFailedSave") })
  }
}

watch(
  () => !!callSetup.value,
  (up) => {
    if (up && calls.value === null) void loadCalls()
  },
  { immediate: true },
)

async function saveCalls(change: Partial<CallsConfig>) {
  if (!calls.value) return
  const next = { ...calls.value, ...change }
  callsBusy.value = true
  try {
    await setCalls(next)
    calls.value = next
    toast.success(t("settings.saved"))
  } catch (e) {
    toast.failure(e, { title: t("settings.callsFailedSave"), retry: () => saveCalls(change) })
  } finally {
    callsBusy.value = false
  }
}

interface Autostart {
  node: boolean
  tray: boolean
}

const autostart = ref<Autostart | null>(null)
const busy = ref(false)

onMounted(async () => {
  // A phone's node is started by its foreground service, at boot or by the
  // app; there is no login item to read, and the command does not exist there.
  if ((await ready()) === "mobile") return
  try {
    autostart.value = await invoke<Autostart>("autostart_get")
  } catch (e) {
    toast.failure(e, { title: t("settings.autostartFailed") })
  }
})

async function setAutostart(next: Autostart) {
  busy.value = true
  try {
    autostart.value = await invoke<Autostart>("autostart_set", { node: next.node, tray: next.tray })
    toast.success(t("settings.saved"))
  } catch (e) {
    toast.failure(e, { title: t("settings.autostartFailed"), retry: () => setAutostart(next) })
  } finally {
    busy.value = false
  }
}

const confirmRestart = ref(false)
const restarting = ref(false)
async function restartNode() {
  restarting.value = true
  try {
    await restart()
    toast.success(t("settings.restarting"))
    confirmRestart.value = false
  } catch (e) {
    toast.failure(e, { title: t("settings.restartFailed") })
  } finally {
    restarting.value = false
  }
}
</script>

<template>
  <DesktopShell>
    <div class="flex flex-col gap-6">
      <section v-if="status" class="flex flex-col gap-3">
        <LpSection :title="t('settings.nameTitle')" :description="t('settings.nameDescription')" icon="lucide:tag" />
        <LpCard padded class="flex flex-col gap-3">
          <form class="flex flex-wrap items-end gap-3" @submit.prevent="saveName">
            <LpInput
              v-model="nameDraft"
              :label="t('settings.nameLabel')"
              :error="nameChanged ? nameError : undefined"
              :disabled="nameBusy"
              class="min-w-0 flex-1 basis-56"
            />
            <LpButton type="submit" variant="solid" :disabled="!nameChanged || !!nameError || nameBusy">
              {{ nameBusy ? t("common.working") : t("settings.rename") }}
            </LpButton>
          </form>
          <p v-if="isMobile" class="text-xs text-muted">{{ t("settings.namePhoneHint") }}</p>
          <LpAlert v-else-if="nameWaitsForRestart" variant="warning" :title="t('settings.nameWaits')">
            <LpButton variant="soft" size="sm" @click="confirmRestart = true">{{ t("settings.restart") }}</LpButton>
          </LpAlert>
        </LpCard>
      </section>

      <section class="flex flex-col gap-3">
        <LpSection :title="t('settings.themeTitle')" :description="t('settings.themeDescription')" icon="lucide:palette" />
        <LpCard padded>
          <LpThemeSwitcher variant="swatch" show-label :aria-label="t('settings.themeTitle')" />
        </LpCard>
      </section>

      <section v-if="status" class="flex flex-col gap-3">
        <LpSection :title="t('settings.audioTitle')" icon="lucide:mic" />
        <LpSkeleton v-if="!gateMode" rounded="card" class="h-24" />
        <LpCard v-else padded class="flex flex-col gap-3">
          <div class="flex flex-wrap items-center gap-4">
            <div class="min-w-0 flex-1">
              <p class="text-sm font-medium">{{ t("settings.micGate") }}</p>
              <p class="text-xs text-muted">{{ t("settings.micGateHint") }}</p>
            </div>
            <LpSegmented
              :model-value="gateMode"
              :options="gateModes"
              :disabled="gateBusy"
              size="sm"
              @update:model-value="setGateMode"
            />
          </div>
          <div v-if="gateMode === 'fixed'" class="flex items-center gap-4">
            <LpSlider
              v-model="gateLevel"
              :min="GATE_MIN"
              :max="GATE_MAX"
              :step="1"
              :disabled="gateBusy"
              :aria-label="t('settings.micGateLevel')"
              class="flex-1"
              @change="saveMicGate"
            />
            <span class="w-20 text-right font-mono text-xs tabular-nums">
              {{ t("settings.micGateDb", { db: gateLevel }) }}
            </span>
          </div>
          <p v-if="gatePinned" class="text-xs text-muted">{{ t("settings.micGatePinned") }}</p>
        </LpCard>
      </section>

      <section v-if="status && !isMobile" class="flex flex-col gap-3">
        <LpSection :title="t('settings.callsTitle')" :description="t('settings.callsDescription')" icon="lucide:phone" />
        <LpCard v-if="!callSetup" padded>
          <p class="text-sm text-muted">{{ t("settings.callsUnsupported") }}</p>
        </LpCard>
        <LpSkeleton v-else-if="!calls" rounded="card" class="h-48" />
        <LpCard v-else padded class="flex flex-col gap-4">
          <ul class="flex flex-col divide-y divide-line">
            <li class="flex flex-col gap-2 pb-3">
              <div class="flex items-center gap-4">
                <p class="min-w-0 flex-1 text-sm font-medium">{{ t("settings.callsRoles") }}</p>
                <LpBadge :tone="callSetup.roles_taken ? 'success' : 'warning'" dot size="sm">
                  {{ callSetup.roles_taken ? t("settings.callsRolesTaken") : t("settings.callsFailed") }}
                </LpBadge>
              </div>
              <template v-if="!callSetup.roles_taken">
                <p class="text-xs text-muted">{{ t("settings.callsRolesKept") }}</p>
                <LpCodeBlock model-value="cursio bluetooth install" />
              </template>
            </li>
            <li
              v-for="row in [
                { label: t('settings.callsHandsFree'), state: callSetup.hands_free },
                { label: t('settings.callsSpeaker'), state: callSetup.speaker },
              ]"
              :key="row.label"
              class="flex flex-col gap-1 py-3"
            >
              <div class="flex items-center gap-4">
                <p class="min-w-0 flex-1 text-sm font-medium">{{ row.label }}</p>
                <LpBadge :tone="serviceTone(row.state)" dot size="sm">{{ serviceLabel(row.state) }}</LpBadge>
              </div>
              <p v-if="row.state.state === 'failed'" class="text-xs text-muted">{{ row.state.reason }}</p>
            </li>
            <li class="flex flex-col gap-2 pt-3">
              <p class="text-sm font-medium">{{ t("settings.callsPhones") }}</p>
              <p v-if="!callSetup.phones.length" class="text-xs text-muted">{{ t("settings.callsNoPhones") }}</p>
              <div v-for="phone in callSetup.phones" :key="phone.address" class="flex items-center gap-3">
                <LpIcon name="lucide:smartphone" :size="16" class="shrink-0 text-muted" />
                <span class="min-w-0 flex-1 truncate text-sm">{{ phone.name }}</span>
                <span class="font-mono text-xs text-muted">{{ phone.address }}</span>
                <LpBadge v-if="phone.connected" tone="success" dot size="sm">{{ t("settings.callsConnected") }}</LpBadge>
              </div>
            </li>
          </ul>

          <div class="flex items-center gap-4">
            <div class="min-w-0 flex-1">
              <p class="text-sm font-medium">{{ t("settings.callsAnswerMoves") }}</p>
              <p class="text-xs text-muted">{{ t("settings.callsAnswerMovesHint") }}</p>
            </div>
            <LpSwitch
              :model-value="calls.answer_moves_voice"
              :disabled="callsBusy"
              :aria-label="t('settings.callsAnswerMoves')"
              @update:model-value="(v: boolean) => saveCalls({ answer_moves_voice: v })"
            />
          </div>
          <div class="flex flex-wrap items-center gap-4">
            <div class="min-w-0 flex-1">
              <p class="text-sm font-medium">{{ t("settings.callsVoice") }}</p>
              <p class="text-xs text-muted">{{ t("settings.callsVoiceHint") }}</p>
            </div>
            <LpSegmented
              :model-value="calls.voice"
              :options="voiceModes"
              :disabled="callsBusy"
              size="sm"
              @update:model-value="(v: string) => saveCalls({ voice: v as CallsConfig['voice'] })"
            />
          </div>
          <div class="grid gap-3 sm:grid-cols-2">
            <LpFormField :label="t('settings.callsOutput')">
              <LpSelect
                :model-value="calls.output ?? DEFAULT_DEVICE"
                :options="outputOptions"
                :disabled="callsBusy"
                @update:model-value="(v: string) => saveCalls({ output: v === DEFAULT_DEVICE ? null : v })"
              />
            </LpFormField>
            <LpFormField :label="t('settings.callsInput')">
              <LpSelect
                :model-value="calls.input ?? DEFAULT_DEVICE"
                :options="inputOptions"
                :disabled="callsBusy"
                @update:model-value="(v: string) => saveCalls({ input: v === DEFAULT_DEVICE ? null : v })"
              />
            </LpFormField>
          </div>
        </LpCard>
      </section>

      <section v-if="status && isMobile" class="flex flex-col gap-3">
        <LpSection :title="t('phoneAudio.title')" :description="t('phoneAudio.description')" icon="lucide:speaker" />
        <PhoneAudioCard />
      </section>

      <section v-if="status && isMobile" class="flex flex-col gap-3">
        <LpSection :title="t('phoneScreen.title')" :description="t('phoneScreen.description')" icon="lucide:cast" />
        <PhoneScreenCard />
      </section>

      <section v-if="status" class="flex flex-col gap-3">
        <LpSection :title="t('streams.title')" :description="t('streams.description')" icon="lucide:volume-2" />
        <LpCard padded>
          <PlayingStreams show="sound" />
        </LpCard>
      </section>

      <section v-if="status?.streams?.some((s) => s.kind === 'microphone')" class="flex flex-col gap-3">
        <LpSection :title="t('streams.microphones')" :description="t('streams.microphonesDescription')" icon="lucide:mic" />
        <LpCard padded>
          <PlayingStreams show="microphones" />
        </LpCard>
      </section>

      <section v-if="!isMobile" class="flex flex-col gap-3">
        <LpSection
          :title="t('settings.autostartTitle')"
          :description="t('settings.autostartDescription')"
          icon="lucide:power"
        />
        <LpSkeleton v-if="!autostart" rounded="card" class="h-32" />
        <LpCard v-else padded>
          <ul class="flex flex-col divide-y divide-line">
            <li class="flex items-center gap-4 pb-3">
              <div class="min-w-0 flex-1">
                <p class="text-sm font-medium">{{ t("settings.autostartNode") }}</p>
                <p class="text-xs text-muted">{{ t("settings.autostartNodeHint") }}</p>
              </div>
              <LpSwitch
                :model-value="autostart.node"
                :disabled="busy"
                :aria-label="t('settings.autostartNode')"
                @update:model-value="(v: boolean) => setAutostart({ ...autostart!, node: v })"
              />
            </li>
            <li class="flex items-center gap-4 pt-3">
              <div class="min-w-0 flex-1">
                <p class="text-sm font-medium">{{ t("settings.autostartTray") }}</p>
                <p class="text-xs text-muted">{{ t("settings.autostartTrayHint") }}</p>
              </div>
              <LpSwitch
                :model-value="autostart.tray"
                :disabled="busy"
                :aria-label="t('settings.autostartTray')"
                @update:model-value="(v: boolean) => setAutostart({ ...autostart!, tray: v })"
              />
            </li>
          </ul>
        </LpCard>
      </section>

      <section class="flex flex-col gap-3">
        <LpSection :title="t('settings.nodeTitle')" icon="lucide:server" />
        <LpCard padded class="flex flex-col gap-3">
          <template v-if="status">
            <LpCodeBlock :model-value="status.config_path" :title="t('settings.configFile')" />
            <p v-if="status.pinned.length" class="text-xs text-muted">
              {{ t("settings.pinned", { fields: status.pinned.join(", ") }) }}
            </p>
            <dl class="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
              <dt class="text-muted">{{ t("settings.build") }}</dt>
              <dd class="font-mono">{{ status.node.build || "—" }}</dd>
              <template v-if="!isMobile">
                <dt class="text-muted">{{ t("settings.installedVersion") }}</dt>
                <dd class="font-mono">{{ status.node.installed_version ?? t("settings.notInstalled") }}</dd>
                <dt class="text-muted">{{ t("settings.pid") }}</dt>
                <dd class="font-mono">{{ status.node.pid || "—" }}</dd>
              </template>
            </dl>
          </template>
          <div v-if="!isMobile">
            <LpButton variant="soft" size="sm" :disabled="!status" @click="confirmRestart = true">
              <LpIcon name="lucide:rotate-cw" :size="14" />
              {{ t("settings.restart") }}
            </LpButton>
          </div>
        </LpCard>
      </section>
    </div>

    <LpConfirmDialog
      v-model:open="confirmRestart"
      icon="lucide:rotate-cw"
      :title="t('settings.restartTitle')"
      :description="t('settings.restartBody')"
      :confirm-label="t('settings.restart')"
      :cancel-label="t('common.cancel')"
      :loading="restarting"
      :loading-label="t('common.working')"
      @confirm="restartNode"
    />
  </DesktopShell>
</template>
