<script setup lang="ts">
const { t } = useI18n()
const { status, applyUpdate } = useNode()
const { install, refreshInstall, rollBack } = useInstall()
const toast = useCursioToast()

const open = ref(false)
const confirming = ref(false)
const rolling = ref(false)
const applying = ref(false)
const confirmingApply = ref(false)

/** Builds arriving over the mesh, as the node reports them. */
const mesh = computed(() => status.value?.update ?? null)
const progress = computed(() => {
  const p = mesh.value?.progress
  if (!p || p.total === 0) return null
  return { ...p, percent: Math.min(100, Math.round((p.received / p.total) * 100)) }
})
const ready = computed(() => mesh.value?.ready ?? null)

async function confirmApply() {
  applying.value = true
  try {
    await applyUpdate()
    toast.success(t("update.switching", { version: ready.value?.version ?? "" }))
    confirmingApply.value = false
    open.value = false
  } catch (e) {
    toast.failure(e, { title: t("update.applyFailed") })
  } finally {
    applying.value = false
  }
}

/** What runs, named the way the install names it; a checkout build says so. */
const running = computed(() => {
  const node = status.value?.node
  if (!node) return install.value?.current ?? null
  return node.installed_version ?? t("update.unversioned", { build: node.build || "?" })
})

/**
 * A running node that is not the install's current version is a switch still
 * to be made (a restart), or one done by hand; either way worth saying.
 */
const drift = computed(() => {
  const node = status.value?.node
  const current = install.value?.current
  return !!node?.installed_version && !!current && node.installed_version !== current
})

async function confirmRollback() {
  rolling.value = true
  try {
    await rollBack(status.value?.node.pid ?? 0)
    toast.success(t("update.rollingBack", { version: install.value?.previous ?? "" }))
    confirming.value = false
    open.value = false
    setTimeout(() => void refreshInstall(), 5000)
  } catch (e) {
    toast.failure(e, { title: t("update.rollbackFailed") })
  } finally {
    rolling.value = false
  }
}

/**
 * Close the popover before asking. Both are floating layers, and the popover
 * stayed open above the confirmation it had raised, covering the buttons that
 * answer it.
 */
function ask(what: "apply" | "rollback") {
  open.value = false
  if (what === "apply") confirmingApply.value = true
  else confirming.value = true
}

watch(open, (o) => {
  if (o) void refreshInstall()
})
</script>

<template>
  <LpPopover v-model:open="open" align="end" panel-class="w-80 p-3">
    <template #trigger>
      <LpButton variant="ghost" size="sm" :aria-label="t('update.title')">
        <LpIcon name="lucide:package" :size="15" />
        <span class="max-w-40 truncate font-mono text-xs">{{ running ?? "—" }}</span>
        <span v-if="ready" class="size-1.5 rounded-full bg-action" />
        <span v-else-if="drift" class="size-1.5 rounded-full bg-accent" />
      </LpButton>
    </template>

    <div class="flex flex-col gap-3">
      <div>
        <p class="text-sm font-semibold">{{ t("update.title") }}</p>
        <p v-if="mesh?.policy" class="text-xs text-muted">
          {{ t("update.policy", { policy: mesh.policy, channel: mesh.channel }) }}
        </p>
      </div>
      <div v-if="progress" class="flex flex-col gap-1">
        <p class="text-xs">
          {{ t("update.fetching", { version: progress.version, from: progress.from }) }}
        </p>
        <LpProgress :value="progress.percent" caption />
      </div>
      <LpAlert v-if="ready" variant="success">
        <p class="text-sm">{{ t("update.ready", { version: ready.version, from: ready.from }) }}</p>
        <p v-if="mesh?.waiting_for" class="text-xs text-muted">
          {{ t("update.waitingFor", { reason: mesh.waiting_for }) }}
        </p>
        <LpButton class="mt-2" variant="solid" size="sm" @click="ask('apply')">
          <LpIcon name="lucide:refresh-cw" :size="14" />
          {{ t("update.apply") }}
        </LpButton>
      </LpAlert>
      <p v-if="mesh?.last" class="text-xs text-muted">{{ mesh.last }}</p>
      <LpAlert v-if="drift" variant="warning">
        {{ t("update.drift", { running: status?.node.installed_version, current: install?.current }) }}
      </LpAlert>
      <LpEmptyState
        v-if="!install || install.installed.length === 0"
        compact
        icon="lucide:package-open"
        :title="t('update.noInstall')"
      >
        <template #description><CodeText :text="t('update.noInstallHint')" /></template>
      </LpEmptyState>
      <ul v-else class="flex flex-col gap-1">
        <li
          v-for="version in [...install.installed].reverse()"
          :key="version"
          class="flex items-center gap-2 rounded-control px-2 py-1 text-xs"
          :class="version === install.current ? 'bg-brand-soft' : ''"
        >
          <span class="min-w-0 flex-1 truncate font-mono">{{ version }}</span>
          <LpBadge v-if="version === install.current" size="sm" tone="brand">{{ t("update.current") }}</LpBadge>
          <LpBadge v-else-if="version === install.previous" size="sm">{{ t("update.previous") }}</LpBadge>
        </li>
      </ul>
      <LpButton
        variant="soft"
        size="sm"
        :disabled="!install?.previous"
        @click="ask('rollback')"
      >
        <LpIcon name="lucide:history" :size="14" />
        {{ t("update.rollback") }}
      </LpButton>
    </div>
  </LpPopover>

  <LpConfirmDialog
    v-model:open="confirming"
    danger
    icon="lucide:history"
    :title="t('update.rollbackTitle', { version: install?.previous ?? '' })"
    :description="t('update.rollbackBody')"
    :confirm-label="t('update.rollback')"
    :cancel-label="t('common.cancel')"
    :loading="rolling"
    :loading-label="t('common.working')"
    @confirm="confirmRollback"
  />

  <LpConfirmDialog
    v-model:open="confirmingApply"
    icon="lucide:refresh-cw"
    :title="t('update.applyTitle', { version: ready?.version ?? '' })"
    :description="t('update.applyBody')"
    :confirm-label="t('update.apply')"
    :cancel-label="t('common.cancel')"
    :loading="applying"
    :loading-label="t('common.working')"
    @confirm="confirmApply"
  />
</template>
