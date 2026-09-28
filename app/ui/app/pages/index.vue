<script setup lang="ts">
import { invoke } from "@tauri-apps/api/core"
import {
  LpAlert,
  LpBadge,
  LpButton,
  LpCard,
  LpEmptyState,
  LpSection,
  LpStat,
  LpSwitch,
} from "@leavepulse/ui"
import type { ModuleSwitch, NodeStatus, PeerView } from "~/composables/useNode"

const { t } = useI18n()
const { status, setModule, pendingSwitches, restart } = useNode()

// A phone has no node to show: its face is the touchpad.
onMounted(async () => {
  if ((await invoke<string>("app_platform")) === "mobile") void navigateTo("/touchpad")
})

const online = computed(() => status.value?.peers.filter((p) => p.presence === "online").length ?? 0)

/** The slower of the two measured paths is the one the hand feels. */
const latency = computed(() => {
  const readings = status.value?.latency ?? []
  if (readings.length === 0) return null
  const worst = readings.reduce((a, b) => (b.p99_us > a.p99_us ? b : a))
  return { p50: worst.p50_us / 1000, p99: worst.p99_us / 1000 }
})

const switches: { key: ModuleSwitch; label: string; hint: string }[] = [
  { key: "input", label: "modules.input", hint: "modules.inputHint" },
  { key: "clipboard", label: "modules.clipboard", hint: "modules.clipboardHint" },
  { key: "audio", label: "modules.audio", hint: "modules.audioHint" },
  { key: "files", label: "modules.files", hint: "modules.filesHint" },
  { key: "usb", label: "modules.usb", hint: "modules.usbHint" },
  { key: "remote_screen", label: "modules.remoteScreen", hint: "modules.remoteScreenHint" },
  { key: "screen_host", label: "modules.screenHost", hint: "modules.screenHostHint" },
]

const toggleError = ref("")
async function toggle(key: ModuleSwitch, enabled: boolean) {
  toggleError.value = ""
  try {
    const reply = await setModule(key, enabled)
    if (reply.reply === "saved" && (reply.pinned as string[]).length > 0) {
      toggleError.value = t("modules.pinned")
    }
  } catch (e) {
    toggleError.value = (e as { message?: string }).message ?? String(e)
  }
}

const restarting = ref(false)
async function restartNode() {
  restarting.value = true
  try {
    await restart()
  } catch (e) {
    toggleError.value = (e as { message?: string }).message ?? String(e)
  } finally {
    restarting.value = false
  }
}

function isPinned(key: ModuleSwitch) {
  return status.value?.pinned.includes(`modules.${key}`) ?? false
}

function battery(s: NodeStatus): string | null {
  if (s.battery_percent === null) return null
  const power = s.power === "Charging" ? t("peer.charging") : s.power === "Mains" ? t("peer.plugged") : ""
  return `${s.battery_percent}%${power ? ` · ${power}` : ""}`
}

function network(s: NodeStatus): string | null {
  switch (s.network) {
    case "Wired":
      return t("peer.wired")
    case "Wifi":
      return s.signal_dbm !== null ? `${t("peer.wifi")} ${s.signal_dbm} dBm` : t("peer.wifi")
    case "Cellular":
      return t("peer.cellular")
    case "Unknown":
      return t("peer.unknownNetwork")
    default:
      return null
  }
}

function presenceTone(p: PeerView) {
  return p.presence === "online" ? "success" : p.presence === "connecting" ? "brand" : "neutral"
}

const severityVariant = { Info: "info", Warning: "warning", Error: "danger" } as const
</script>

<template>
  <DesktopShell>
    <div v-if="status" class="flex flex-col gap-6">
      <div class="grid grid-cols-1 gap-3 sm:grid-cols-3">
        <LpStat
          :label="t('stats.peers')"
          :value="`${online} / ${status.peers.length}`"
          icon="lucide:monitor-smartphone"
          :online="online > 0"
        />
        <LpStat
          :label="t('stats.latency')"
          :value="latency ? `${latency.p50.toFixed(1)} / ${latency.p99.toFixed(1)} ms` : '—'"
          :hint="latency ? t('stats.latencyHint') : t('stats.noLatency')"
          icon="lucide:gauge"
        />
        <LpStat
          :label="t('stats.problems')"
          :value="status.problems.length"
          icon="lucide:triangle-alert"
        />
      </div>

      <section v-if="status.problems.length" class="flex flex-col gap-3">
        <LpSection :title="t('problems.title')" icon="lucide:triangle-alert" />
        <div class="flex flex-col gap-2">
          <LpAlert
            v-for="problem in status.problems"
            :key="problem.id"
            :variant="severityVariant[problem.severity]"
            :title="problem.summary"
          >
            <p v-if="problem.hint" class="text-sm">{{ problem.hint }}</p>
          </LpAlert>
        </div>
      </section>

      <section class="flex flex-col gap-3">
        <LpSection :title="t('modules.title')" :description="t('modules.description')" icon="lucide:blocks" />
        <LpCard padded>
          <ul class="flex flex-col divide-y divide-line">
            <li v-for="s in switches" :key="s.key" class="flex items-center gap-4 py-3 first:pt-0 last:pb-0">
              <div class="min-w-0 flex-1">
                <p class="flex items-center gap-2 text-sm font-medium">
                  {{ t(s.label) }}
                  <LpBadge v-if="isPinned(s.key)" size="sm" tone="warning">{{ t("modules.flag") }}</LpBadge>
                </p>
                <p class="text-xs text-muted">{{ t(s.hint) }}</p>
              </div>
              <LpSwitch
                :model-value="status.modules[s.key]"
                :disabled="pendingSwitches.has(s.key) || isPinned(s.key)"
                @update:model-value="(v: boolean) => toggle(s.key, v)"
              />
            </li>
          </ul>
          <p v-if="toggleError" class="mt-3 text-xs text-danger">{{ toggleError }}</p>
          <p class="mt-3 text-xs text-muted">{{ t("modules.appliedNote", { path: status.config_path }) }}</p>
        </LpCard>
        <LpAlert
          v-if="status.restart_needed.length"
          variant="warning"
          :title="t('modules.restartNeeded', { fields: status.restart_needed.join(', ') })"
        >
          <LpButton variant="soft" size="sm" :disabled="restarting" @click="restartNode">
            {{ t("settings.restart") }}
          </LpButton>
        </LpAlert>
      </section>

      <section class="flex flex-col gap-3">
        <LpSection :title="t('peers.title')" icon="lucide:network" />
        <LpEmptyState
          v-if="status.peers.length === 0"
          compact
          icon="lucide:link"
          :title="t('peers.none')"
          :description="t('peers.noneHint')"
        />
        <div v-else class="grid grid-cols-1 gap-3 md:grid-cols-2">
          <LpCard v-for="peer in status.peers" :key="peer.id" padded>
            <div class="flex items-center gap-2">
              <p class="min-w-0 flex-1 truncate font-medium">{{ peer.name }}</p>
              <LpBadge :tone="presenceTone(peer)" dot size="sm">{{ t(`presence.${peer.presence}`) }}</LpBadge>
            </div>
            <template v-if="peer.link">
              <dl class="mt-3 grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
                <dt class="text-muted">{{ t("peer.position") }}</dt>
                <dd>{{ peer.link.position.x }}, {{ peer.link.position.y }}</dd>
                <dt class="text-muted">{{ t("peer.kind") }}</dt>
                <dd>{{ t(`kind.${peer.link.device_kind}`) }}</dd>
                <dt class="text-muted">{{ t("peer.battery") }}</dt>
                <dd>{{ battery(peer.link.status) ?? t("peer.cannotSay") }}</dd>
                <dt class="text-muted">{{ t("peer.network") }}</dt>
                <dd>{{ network(peer.link.status) ?? t("peer.cannotSay") }}</dd>
              </dl>
              <div class="mt-3 flex flex-wrap gap-1">
                <LpBadge v-for="m in peer.link.modules" :key="m" size="sm" tone="brand">{{ m }}</LpBadge>
              </div>
              <ul v-if="peer.link.skipped.length" class="mt-2 flex flex-col gap-0.5 text-xs text-muted">
                <li v-for="[m, why] in peer.link.skipped" :key="m">{{ t("peer.skipped", { module: m, why }) }}</li>
              </ul>
            </template>
            <p v-else class="mt-2 text-xs text-muted">{{ t("peer.noLink") }}</p>
          </LpCard>
        </div>
      </section>
    </div>
  </DesktopShell>
</template>
