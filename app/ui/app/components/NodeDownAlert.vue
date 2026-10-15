<script setup lang="ts">
/**
 * What every page shows above itself while the node does not answer: on a
 * desktop the daemon is not running, on a phone the foreground service that
 * carries the node has stopped. Either way the way back is one button.
 */
const { t } = useI18n()
const { error, notRunning, start } = useNode()
const { isMobile } = usePlatform()
const toast = useCursioToast()

const starting = ref(false)
async function startNode() {
  starting.value = true
  try {
    await start()
    toast.success(t("node.startRequested"))
  } catch (e) {
    toast.failure(e, { title: t("node.startFailed"), retry: startNode })
  } finally {
    // The node takes a moment to open its socket; the pushed status says when.
    setTimeout(() => (starting.value = false), 3000)
  }
}
</script>

<template>
  <LpAlert
    v-if="notRunning"
    variant="warning"
    :title="isMobile ? t('node.notRunningTitlePhone') : t('node.notRunningTitle')"
    class="mb-4"
  >
    <p class="text-sm">{{ isMobile ? t("node.notRunningBodyPhone") : t("node.notRunningBody") }}</p>
    <div class="mt-3">
      <LpButton variant="solid" :size="isMobile ? 'md' : 'sm'" :disabled="starting" @click="startNode">
        <LpIcon name="lucide:play" :size="14" />
        {{ starting ? t("node.starting") : t("node.start") }}
      </LpButton>
    </div>
  </LpAlert>
  <LpAlert v-else-if="error" variant="danger" class="mb-4">{{ error.message }}</LpAlert>
</template>
