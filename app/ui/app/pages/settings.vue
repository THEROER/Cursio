<script setup lang="ts">
import { invoke } from "@tauri-apps/api/core"

const { t } = useI18n()
const { status, restart } = useNode()
const toast = useCursioToast()

interface Autostart {
  node: boolean
  tray: boolean
}

const autostart = ref<Autostart | null>(null)
const busy = ref(false)

onMounted(async () => {
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
      <section class="flex flex-col gap-3">
        <LpSection :title="t('settings.themeTitle')" :description="t('settings.themeDescription')" icon="lucide:palette" />
        <LpCard padded>
          <LpThemeSwitcher variant="swatch" show-label :aria-label="t('settings.themeTitle')" />
        </LpCard>
      </section>

      <section class="flex flex-col gap-3">
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
              <dt class="text-muted">{{ t("settings.installedVersion") }}</dt>
              <dd class="font-mono">{{ status.node.installed_version ?? t("settings.notInstalled") }}</dd>
              <dt class="text-muted">{{ t("settings.pid") }}</dt>
              <dd class="font-mono">{{ status.node.pid || "—" }}</dd>
            </dl>
          </template>
          <div>
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
