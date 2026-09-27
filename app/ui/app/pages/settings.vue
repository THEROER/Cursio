<script setup lang="ts">
import { invoke } from "@tauri-apps/api/core"
import { LpButton, LpCard, LpSection, LpSwitch } from "@leavepulse/ui"

const { t } = useI18n()
const { status, restart } = useNode()

interface Autostart {
  node: boolean
  tray: boolean
}

const autostart = ref<Autostart | null>(null)
const busy = ref(false)
const message = ref("")

onMounted(async () => {
  autostart.value = await invoke<Autostart>("autostart_get")
})

async function setAutostart(next: Autostart) {
  busy.value = true
  message.value = ""
  try {
    autostart.value = await invoke<Autostart>("autostart_set", { node: next.node, tray: next.tray })
  } catch (e) {
    message.value = String(e)
  } finally {
    busy.value = false
  }
}

async function restartNode() {
  message.value = ""
  try {
    await restart()
    message.value = t("settings.restarting")
  } catch (e) {
    message.value = (e as { message?: string }).message ?? String(e)
  }
}
</script>

<template>
  <DesktopShell>
    <div class="flex flex-col gap-6">
      <section class="flex flex-col gap-3">
        <LpSection
          :title="t('settings.autostartTitle')"
          :description="t('settings.autostartDescription')"
          icon="lucide:power"
        />
        <LpCard v-if="autostart" padded>
          <ul class="flex flex-col divide-y divide-line">
            <li class="flex items-center gap-4 pb-3">
              <div class="min-w-0 flex-1">
                <p class="text-sm font-medium">{{ t("settings.autostartNode") }}</p>
                <p class="text-xs text-muted">{{ t("settings.autostartNodeHint") }}</p>
              </div>
              <LpSwitch
                :model-value="autostart.node"
                :disabled="busy"
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
                @update:model-value="(v: boolean) => setAutostart({ ...autostart!, tray: v })"
              />
            </li>
          </ul>
        </LpCard>
      </section>

      <section class="flex flex-col gap-3">
        <LpSection :title="t('settings.nodeTitle')" icon="lucide:server" />
        <LpCard padded class="flex flex-col gap-3">
          <p v-if="status" class="text-xs text-muted">{{ t("settings.configAt", { path: status.config_path }) }}</p>
          <p v-if="status?.pinned.length" class="text-xs text-muted">
            {{ t("settings.pinned", { fields: status.pinned.join(", ") }) }}
          </p>
          <div>
            <LpButton variant="soft" size="sm" :disabled="!status" @click="restartNode">
              {{ t("settings.restart") }}
            </LpButton>
          </div>
        </LpCard>
      </section>

      <p v-if="message" class="text-xs text-muted">{{ message }}</p>
    </div>
  </DesktopShell>
</template>
