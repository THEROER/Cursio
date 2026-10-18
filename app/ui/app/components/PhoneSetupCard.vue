<script setup lang="ts">
import type { PhoneGrant } from "~/composables/usePhone"

/**
 * What the phone still has to allow for the node to stay up and carry sound.
 * Shown only while something is missing; each row opens the system's own
 * dialog or screen, and the list is read again when the user comes back.
 *
 * Autostart is not a permission: a HyperOS phone kills the service unless a
 * setting of the maker's own allows it, and nothing can read that setting. So
 * it is a hint the user closes by saying it is done, never a granted state.
 */
const { t } = useI18n()
const { setup, missing, autostartDone, request, markAutostartDone } = usePhone()
const toast = useCursioToast()

interface Row {
  what: Exclude<PhoneGrant, "autostart">
  icon: string
}

const ROWS: Row[] = [
  { what: "notifications", icon: "lucide:bell" },
  { what: "microphone", icon: "lucide:mic" },
  { what: "battery", icon: "lucide:battery-charging" },
  { what: "notificationAccess", icon: "lucide:bell-ring" },
  { what: "calls", icon: "lucide:phone" },
  { what: "messages", icon: "lucide:message-square" },
  { what: "gallery", icon: "lucide:images" },
  { what: "nearby", icon: "lucide:bluetooth" },
  { what: "storage", icon: "lucide:folder-open" },
  { what: "companion", icon: "lucide:laptop" },
]

const busy = ref<PhoneGrant | null>(null)

async function ask(what: PhoneGrant) {
  busy.value = what
  try {
    await request(what)
  } catch (e) {
    toast.failure(e, { title: t("phone.requestFailed") })
  } finally {
    busy.value = null
  }
}
</script>

<template>
  <LpCard v-if="setup && missing" padded class="flex flex-col gap-3">
    <div class="flex items-start gap-3">
      <LpIcon name="lucide:smartphone" :size="20" class="mt-0.5 shrink-0 text-brand" />
      <div class="min-w-0">
        <p class="text-sm font-semibold">{{ t("phone.title") }}</p>
        <p class="text-xs text-muted">{{ t("phone.description") }}</p>
      </div>
    </div>
    <ul class="flex flex-col divide-y divide-line">
      <li v-for="row in ROWS" :key="row.what" class="flex items-center gap-3 py-3 last:pb-0">
        <LpIcon :name="row.icon" :size="18" class="shrink-0 text-muted" />
        <div class="min-w-0 flex-1">
          <p class="text-sm font-medium">{{ t(`phone.${row.what}`) }}</p>
          <p class="text-xs text-muted">{{ t(`phone.${row.what}Hint`) }}</p>
        </div>
        <LpBadge v-if="setup[row.what]" tone="success" dot size="sm" class="shrink-0">{{ t("phone.granted") }}</LpBadge>
        <LpButton
          v-else
          variant="solid"
          class="shrink-0"
          :disabled="busy !== null"
          @click="ask(row.what)"
        >
          {{ t("phone.allow") }}
        </LpButton>
      </li>
      <li v-if="setup.autostartSetting && !autostartDone" class="flex flex-col gap-3 py-3 last:pb-0">
        <div class="flex items-center gap-3">
          <LpIcon name="lucide:rocket" :size="18" class="shrink-0 text-muted" />
          <div class="min-w-0 flex-1">
            <p class="text-sm font-medium">{{ t("phone.autostart") }}</p>
            <p class="text-xs text-muted">{{ t("phone.autostartHint") }}</p>
          </div>
        </div>
        <div class="flex flex-wrap justify-end gap-2">
          <LpButton variant="ghost" @click="markAutostartDone(true)">{{ t("phone.autostartDone") }}</LpButton>
          <LpButton variant="soft" :disabled="busy !== null" @click="ask('autostart')">
            <LpIcon name="lucide:external-link" :size="14" />
            {{ t("phone.openSettings") }}
          </LpButton>
        </div>
      </li>
    </ul>
  </LpCard>
</template>
