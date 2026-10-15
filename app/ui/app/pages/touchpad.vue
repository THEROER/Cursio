<script setup lang="ts">
const { t } = useI18n()
const { status, linkLost, isConnected, isBusy, connect, click, surface } = useTouchpad()

const password = ref("")

const statusTone = computed(() => {
  switch (status.value.state) {
    case "connected":
      return "success" as const
    case "failed":
      return "danger" as const
    case "searching":
    case "connecting":
      return "brand" as const
    default:
      return "neutral" as const
  }
})

const statusLabel = computed(() => {
  const value = status.value
  switch (value.state) {
    case "searching":
      return t("connection.searching")
    case "connecting":
      return t("connection.connectingTo", { peer: value.peer })
    case "connected":
      return t("connection.connectedTo", { peer: value.peer })
    case "failed":
      return t("connection.failed")
    default:
      return t("connection.disconnected")
  }
})

const padMessage = computed(() => {
  if (linkLost.value) return t("pad.lost")
  return isConnected.value ? t("pad.ready") : t("pad.idle")
})

const buttons: { name: "left" | "middle" | "right"; label: string }[] = [
  { name: "left", label: "buttons.left" },
  { name: "middle", label: "buttons.middle" },
  { name: "right", label: "buttons.right" },
]
</script>

<template>
  <DesktopShell full-bleed>
    <div class="flex h-full min-h-0 flex-1 flex-col gap-4 p-4">
      <header class="flex items-center gap-3">
        <p class="min-w-0 truncate text-xs text-muted">{{ t("app.subtitle") }}</p>
        <LpBadge :tone="statusTone" dot class="ml-auto shrink-0">{{ statusLabel }}</LpBadge>
      </header>

      <LpCard v-if="!isConnected" padded>
        <form class="flex flex-col gap-3" @submit.prevent="connect(password)">
          <LpFormField :label="t('connection.password')" :hint="t('connection.passwordHint')">
            <LpPasswordInput v-model="password" :disabled="isBusy" />
          </LpFormField>
          <LpButton type="submit" variant="solid" block :disabled="isBusy">
            {{ isBusy ? t("connection.connecting") : t("connection.connect") }}
          </LpButton>
        </form>
      </LpCard>

      <LpAlert v-if="status.state === 'failed'" variant="danger">
        {{ status.reason }}
      </LpAlert>

      <!-- The surface itself. Not a kit component: nothing in a form kit models a
           raw gesture target, and every pixel of it is the control. -->
      <LpCard
        class="cursio-pad flex min-h-40 flex-1 select-none items-center justify-center text-center"
        :variant="isConnected ? 'raised' : 'flat'"
        v-on="surface"
      >
        <div class="pointer-events-none flex flex-col gap-1 px-6">
          <p class="text-sm" :class="isConnected ? 'text-ink' : 'text-muted'">{{ padMessage }}</p>
          <p class="text-xs text-muted">{{ t("pad.hints") }}</p>
        </div>
      </LpCard>

      <div class="grid grid-cols-3 gap-2">
        <LpButton
          v-for="button in buttons"
          :key="button.name"
          variant="soft"
          size="lg"
          :disabled="!isConnected"
          @click="click(button.name)"
        >
          {{ t(button.label) }}
        </LpButton>
      </div>
    </div>
  </DesktopShell>
</template>
