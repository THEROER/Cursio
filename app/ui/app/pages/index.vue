<script setup lang="ts">
import type { ModuleSwitch, PeerView } from "~/composables/useNode"

const { t } = useI18n()
const { status, error, setModule, pendingSwitches, restart } = useNode()
const { latency: latencyHistory, uptime } = useHistory()
const { isMobile } = usePlatform()
const toast = useCursioToast()

const online = computed(() => status.value?.peers.filter((p) => p.presence === "online").length ?? 0)

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

const { battery, network } = useMachineText()

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
          <LpStat
            :label="t('stats.peers')"
            :value="`${online} / ${status.peers.length}`"
            icon="lucide:monitor-smartphone"
            :online="online > 0"
          />
        </LpTooltip>
        <LpTooltip :content="latency ? t('stats.latencyHint') : t('stats.noLatency')">
          <LpStat
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
          </LpStat>
        </LpTooltip>
        <LpTooltip :content="t('stats.problemsHint')">
          <LpStat :label="t('stats.problems')" :value="status.problems.length" icon="lucide:triangle-alert" />
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
            :peer="{ id: peer.id, name: peer.name, presence: peer.presence }"
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
                <PeerMenuButton :peer="{ id: peer.id, name: peer.name, presence: peer.presence }" />
              </div>
              <LpUptimeBar
                :segments="uptime(peer.id)"
                :title="t('peer.uptime')"
                :start-label="t('peer.uptimeStart')"
                :end-label="t('peer.uptimeEnd')"
              />
              <template v-if="peer.link">
                <dl class="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
                  <dt class="text-muted">{{ t("peer.position") }}</dt>
                  <dd>{{ peer.link.position.x }}, {{ peer.link.position.y }}</dd>
                  <dt class="text-muted">{{ t("peer.kind") }}</dt>
                  <dd>{{ t(`kind.${peer.link.device_kind}`) }}</dd>
                  <dt class="text-muted">{{ t("peer.battery") }}</dt>
                  <dd>{{ battery(peer.link.status) ?? t("peer.cannotSay") }}</dd>
                  <dt class="text-muted">{{ t("peer.network") }}</dt>
                  <dd>{{ network(peer.link.status) ?? t("peer.cannotSay") }}</dd>
                </dl>
                <div class="flex flex-wrap gap-1">
                  <LpBadge v-for="m in peer.link.modules" :key="m" size="sm" tone="brand">{{ m }}</LpBadge>
                  <LpTooltip v-for="[m, why] in peer.link.skipped" :key="m" :content="why">
                    <LpBadge size="sm" tone="outline" class="line-through">{{ m }}</LpBadge>
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
