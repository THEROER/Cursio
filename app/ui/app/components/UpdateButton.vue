<script setup lang="ts">
const { t } = useI18n()
const { status } = useNode()
const { install, refreshInstall, rollBack } = useInstall()
const toast = useCursioToast()

const open = ref(false)
const confirming = ref(false)
const rolling = ref(false)

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
        <span v-if="drift" class="size-1.5 rounded-full bg-accent" />
      </LpButton>
    </template>

    <div class="flex flex-col gap-3">
      <div>
        <p class="text-sm font-semibold">{{ t("update.title") }}</p>
        <p class="text-xs text-muted">{{ t("update.meshLater") }}</p>
      </div>
      <LpAlert v-if="drift" variant="warning">
        {{ t("update.drift", { running: status?.node.installed_version, current: install?.current }) }}
      </LpAlert>
      <LpEmptyState
        v-if="!install || install.installed.length === 0"
        compact
        icon="lucide:package-open"
        :title="t('update.noInstall')"
        :description="t('update.noInstallHint')"
      />
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
        @click="confirming = true"
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
</template>
