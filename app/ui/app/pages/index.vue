<script setup lang="ts">
import { toMillis, type ModuleSwitch, type PeerView } from "~/composables/useNode"

const { t } = useI18n()

/** Ticks once a minute, for "on Bluetooth for N min". */
const now = ref(Date.now())
const clock = setInterval(() => (now.value = Date.now()), 60_000)
onUnmounted(() => clearInterval(clock))

/**
 * How a peer held over Bluetooth is labelled: with how long, once the node
 * says since when, since a machine on Bluetooth for a minute and one for an
 * hour are different stories — the second has a network that is not coming
 * back by itself.
 */
function overBluetooth(peer: PeerView): string {
  const since = peer.link?.path_since
  if (!since) return t("peer.overBluetooth")
  const minutes = Math.floor((now.value - toMillis(since)) / 60_000)
  return minutes < 1 ? t("peer.overBluetooth") : t("peer.overBluetoothFor", { minutes })
}
const { status, error, setModule, pendingSwitches, restart } = useNode()
const { latency: latencyHistory, uptime } = useHistory()
const { isMobile } = usePlatform()
const toast = useCursioToast()

// The node's reasons are written for a log; on a badge they should say what
// to do. A peer lacking a module it could run is switched off there or older.
function skipHint(peer: string, why: string): string {
  if (why.startsWith("peer does not have it")) return t("peer.skippedLacks", { peer })
  if (why.startsWith("version mismatch")) return t("peer.skippedVersion", { peer })
  return why
}

const onlinePeers = computed(() => status.value?.peers.filter((p) => p.presence === "online") ?? [])
const online = computed(() => onlinePeers.value.length)
const onlineNames = computed(() => onlinePeers.value.map((p) => p.name).join(", ") || t("stats.nobodyOnline"))

const problemsNote = computed(() => {
  const all = status.value?.problems ?? []
  if (!all.length) return t("stats.allClear")
  const count = (severity: string) => all.filter((p) => p.severity === severity).length
  return [
    ["stats.errors", count("Error")],
    ["stats.warnings", count("Warning")],
    ["stats.notes", count("Info")],
  ]
    .filter(([, n]) => n)
    .map(([key, n]) => t(key as string, { n }, n as number))
    .join(" · ")
})

const latency = computed(() => worstLatency(status.value?.latency ?? []))

async function toggle(key: ModuleSwitch, enabled: boolean) {
  try {
    const reply = await setModule(key, enabled)
    if (reply.reply === "saved" && (reply.pinned as string[]).length > 0) toast.warning(t("modules.pinned"))
  } catch (e) {
    toast.failure(e, { title: t("modules.toggleFailed"), retry: () => toggle(key, enabled) })
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

function isPinned(key: ModuleSwitch) {
  return status.value?.pinned.includes(`modules.${key}`) ?? false
}

const { battery, network, place } = useMachineText()

const MODULE_META = Object.fromEntries(MODULES.map((m) => [m.key, m]))

const peerRef = peerRefOf

function presenceTone(p: PeerView) {
  return p.presence === "online" ? "success" : p.presence === "connecting" ? "brand" : "neutral"
}
</script>

<template>
  <DesktopShell>
    <!-- Loading: the first push has not arrived and nothing has failed yet. -->
    <div v-if="!status && !error" class="flex flex-col gap-6">
      <div class="grid grid-cols-1 gap-3 sm:grid-cols-3">
        <LpSkeleton v-for="i in 3" :key="i" rounded="card" class="h-24" />
      </div>
      <LpSkeleton rounded="card" class="h-64" />
    </div>

    <div v-if="isMobile" class="mb-6 empty:hidden"><PhoneSetupCard /></div>

    <div v-if="status" class="flex flex-col gap-6">
      <LpCard v-if="isMobile" padded class="flex flex-col gap-3">
        <div class="flex items-center gap-2">
          <LpIcon name="lucide:smartphone" :size="18" class="shrink-0 text-brand" />
          <p class="min-w-0 flex-1 truncate font-medium">{{ status.node.name }}</p>
          <LpBadge tone="brand" size="sm">{{ t("phone.thisPhone") }}</LpBadge>
        </div>
        <dl class="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
          <dt class="text-muted">{{ t("peer.battery") }}</dt>
          <dd>{{ battery(status.node.status) ?? t("peer.cannotSay") }}</dd>
          <dt class="text-muted">{{ t("peer.network") }}</dt>
          <dd>{{ network(status.node.status) ?? t("peer.cannotSay") }}</dd>
          <dt class="text-muted">{{ t("stats.peers") }}</dt>
          <dd>{{ online }} / {{ status.peers.length }}</dd>
        </dl>
      </LpCard>

      <div v-else class="grid grid-cols-1 gap-3 sm:grid-cols-3">
        <LpTooltip :content="t('stats.peersHint')">
          <LpCard padded><LpStat
            :label="t('stats.peers')"
            :value="`${online} / ${status.peers.length}`"
            icon="lucide:monitor-smartphone"
            :online="online > 0"
            :hint="onlineNames"
          /></LpCard>
        </LpTooltip>
        <LpTooltip :content="latency ? t('stats.latencyHint') : t('stats.noLatency')">
          <LpCard padded><LpStat
            :label="t('stats.latency')"
            :value="latency ? latency.p99 : '—'"
            :decimals="1"
            value-suffix=" ms"
            :hint="latency ? t('stats.latencyP50', { p50: latency.p50.toFixed(1) }) : t('stats.noLatency')"
            icon="lucide:gauge"
          >
            <template #trend>
              <LpSparkline
                v-if="latencyHistory.length > 1"
                :data="latencyHistory"
                :min="0"
                tone="brand"
                area
                show-last
                :aria-label="t('stats.latencyTrend')"
              />
            </template>
          </LpStat></LpCard>
        </LpTooltip>
        <LpTooltip :content="t('stats.problemsHint')">
          <LpCard padded><LpStat :label="t('stats.problems')" :value="status.problems.length" :hint="problemsNote" icon="lucide:triangle-alert" /></LpCard>
        </LpTooltip>
      </div>

      <section class="flex flex-col gap-3">
        <LpSection :title="t('problems.title')" icon="lucide:triangle-alert" />
        <ProblemList :problems="status.problems" />
      </section>

      <section class="flex flex-col gap-3">
        <LpSection :title="t('modules.title')" :description="t('modules.description')" icon="lucide:blocks" />
        <LpCard padded>
          <ul class="flex flex-col divide-y divide-line">
            <li v-for="s in offeredModules(status.available)" :key="s.key" class="flex items-center gap-4 py-3 first:pt-0 last:pb-0">
              <LpIcon :name="s.icon" :size="18" class="shrink-0 text-muted" />
              <div class="min-w-0 flex-1">
                <p class="flex items-center gap-2 text-sm font-medium">
                  {{ t(s.label) }}
                  <LpTooltip v-if="isPinned(s.key)" :content="t('modules.pinned')">
                    <LpBadge size="sm" tone="warning">{{ t("modules.flag") }}</LpBadge>
                  </LpTooltip>
                </p>
                <p class="text-xs text-muted">{{ t(s.hint) }}</p>
              </div>
              <LpSwitch
                :model-value="status.modules[s.key]"
                :disabled="pendingSwitches.has(s.key) || isPinned(s.key)"
                :aria-label="t(s.label)"
                @update:model-value="(v: boolean) => toggle(s.key, v)"
              />
            </li>
          </ul>
          <p class="mt-3 text-xs text-muted">{{ t("modules.appliedNote", { path: status.config_path }) }}</p>
        </LpCard>
        <LpAlert
          v-if="status.restart_needed.length"
          variant="warning"
          :title="t('modules.restartNeeded', { fields: status.restart_needed.join(', ') })"
        >
          <LpButton v-if="!isMobile" variant="soft" size="sm" @click="confirmRestart = true">{{ t("settings.restart") }}</LpButton>
          <p v-else class="text-sm">{{ t("modules.restartPhone") }}</p>
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
          <PeerMenu
            v-for="peer in status.peers"
            :key="peer.id"
            :peer="peerRef(peer)"
          >
            <LpCard padded class="flex flex-col gap-3">
              <div class="flex items-center gap-2">
                <LpIcon
                  :name="peer.link ? KIND_ICON[peer.link.device_kind] : UNKNOWN_KIND_ICON"
                  :size="18"
                  class="shrink-0 text-muted"
                />
                <p class="min-w-0 flex-1 truncate font-medium">{{ peer.name }}</p>
                <LpBadge :tone="presenceTone(peer)" dot size="sm">{{ t(`presence.${peer.presence}`) }}</LpBadge>
                <LpTooltip v-if="peer.link?.path === 'bluetooth'" :content="t('peer.overBluetoothHint')">
                  <LpBadge size="sm" tone="outline">{{ overBluetooth(peer) }}</LpBadge>
                </LpTooltip>
                <PeerMenuButton :peer="peerRef(peer)" />
              </div>
              <LpUptimeBar
                :segments="uptime(peer.id)"
                :title="t('peer.uptime')"
                :start-label="t('peer.uptimeStart')"
                :end-label="t('peer.uptimeEnd')"
              />
              <template v-if="peer.link">
                <dl class="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
                  <template v-if="takesCursor(peer.link.device_kind)">
                    <dt class="text-muted">{{ t("peer.position") }}</dt>
                    <dd>{{ place(peer.link.position) }}</dd>
                  </template>
                  <dt class="text-muted">{{ t("peer.kind") }}</dt>
                  <dd>{{ t(`kind.${peer.link.device_kind}`) }}</dd>
                  <dt class="text-muted">{{ t("peer.battery") }}</dt>
                  <dd>{{ battery(peer.link.status) ?? t("peer.cannotSay") }}</dd>
                  <dt class="text-muted">{{ t("peer.network") }}</dt>
                  <dd>{{ network(peer.link.status) ?? t("peer.cannotSay") }}</dd>
                </dl>
                <div class="flex flex-wrap gap-1">
                  <LpBadge v-for="m in peer.link.modules" :key="m" size="sm" tone="brand">
                    <LpIcon v-if="MODULE_META[m]" :name="MODULE_META[m]!.icon" :size="12" />
                    {{ MODULE_META[m] ? t(MODULE_META[m]!.label) : m }}
                  </LpBadge>
                  <LpTooltip v-for="[m, why] in peer.link.skipped" :key="m" :content="skipHint(peer.name, why)">
                    <LpBadge size="sm" tone="outline" class="text-muted">
                      <LpIcon v-if="MODULE_META[m]" :name="MODULE_META[m]!.icon" :size="12" />
                      {{ MODULE_META[m] ? t(MODULE_META[m]!.label) : m }}
                    </LpBadge>
                  </LpTooltip>
                </div>
              </template>
              <p v-else class="text-xs text-muted">{{ t("peer.noLink") }}</p>
            </LpCard>
          </PeerMenu>
        </div>
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
