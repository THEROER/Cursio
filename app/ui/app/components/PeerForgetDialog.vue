<script setup lang="ts">
/**
 * The one confirmation for removing a machine, whichever of its menus asked.
 * Mounted once in the shell; `usePeerMenu` says which machine.
 */
const { t } = useI18n()
const { removing, confirmOpen, busy, confirmRemove } = usePeerMenu()
</script>

<template>
  <LpConfirmDialog
    v-model:open="confirmOpen"
    danger
    :title="t('peerMenu.removeTitle', { name: removing?.name ?? '' })"
    :description="t('peerMenu.removeBody', { name: removing?.name ?? '' })"
    :confirm-label="t('peerMenu.removeConfirm')"
    :cancel-label="t('common.cancel')"
    :loading="busy"
    :loading-label="t('common.working')"
    @confirm="confirmRemove"
  />
</template>
