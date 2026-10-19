<script setup lang="ts">
/**
 * This window is an older build than the node it talks to. The node switches
 * versions by itself; a tray window is rarely closed, and an old one cannot
 * read what a newer node sends — the settings page sat empty that way. One
 * button brings the window back as the version the node runs.
 */
const { t } = useI18n()
const { status } = useNode()
const { install, refreshInstall, relaunch } = useInstall()
const toast = useCursioToast()

const stale = computed(() => {
  const mine = install.value?.app
  const node = status.value?.node.installed_version
  return !!mine && !!node && mine !== node
})

// The node switching is the moment this can become true; the versions on
// disk are read again then rather than followed.
watch(
  () => status.value?.node.installed_version,
  () => void refreshInstall(),
)

const restarting = ref(false)
async function restart() {
  restarting.value = true
  try {
    await relaunch()
  } catch (e) {
    toast.failure(e, { title: t("window.relaunchFailed"), retry: restart })
    restarting.value = false
  }
}
</script>

<template>
  <LpAlert v-if="stale" variant="warning" :title="t('window.staleTitle')" class="mb-4">
    <p class="text-sm">
      {{ t("window.staleBody", { window: install?.app, node: status?.node.installed_version }) }}
    </p>
    <div class="mt-3">
      <LpButton variant="solid" size="sm" :disabled="restarting" @click="restart">
        <LpIcon name="lucide:refresh-cw" :size="14" />
        {{ t("window.relaunch") }}
      </LpButton>
    </div>
  </LpAlert>
</template>
