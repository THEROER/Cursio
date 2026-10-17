<script setup lang="ts">
const { t } = useI18n()
const { status, linkLost, isConnected, targets, driven, connecting, connectError, connect, click, zone, surface } =
  useTouchpad()

/**
 * The whole screen as a buttonless clickpad: every pixel above the button
 * strip moves the pointer, and the strip is split into a left and a right
 * button the way a laptop's pad is. Teleported out of the shell so neither its
 * title bar nor its tab bar takes a stroke meant for the pad.
 */
const full = ref(false)
const leftZone = zone("left")
const rightZone = zone("right")

const statusLabel = computed(() => {
  const value = status.value
  return value.state === "connected" ? t("touchpad.driving", { peer: value.peer }) : t("touchpad.notDriving")
})

const targetOptions = computed(() => targets.value.map((target) => ({ value: target.id, label: target.name })))

function pick(id: string | string[] | undefined) {
  if (typeof id === "string" && id !== driven.value?.id) void connect(id)
}

const padMessage = computed(() => {
  if (linkLost.value) return t("pad.lost")
  if (isConnected.value) return t("pad.ready")
  return targets.value.length === 0 ? t("pad.noTargets") : t("pad.idle")
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
        <LpBadge :tone="isConnected ? 'success' : 'neutral'" dot class="ml-auto shrink-0">{{ statusLabel }}</LpBadge>
      </header>

      <!-- With one machine there is nothing to pick: it is driven as soon as
           its link is up, and the badge already names it. -->
      <LpFormField v-if="targets.length > 1" :label="t('touchpad.target')">
        <LpSelect
          :model-value="driven?.id"
          :options="targetOptions"
          :placeholder="t('touchpad.choose')"
          :disabled="connecting"
          @update:model-value="pick"
        />
      </LpFormField>

      <LpAlert v-if="connectError" variant="danger">{{ connectError }}</LpAlert>

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

      <LpButton variant="outline" block :disabled="!isConnected" @click="full = true">
        <LpIcon name="lucide:maximize" :size="16" />
        {{ t("pad.fullScreen") }}
      </LpButton>
    </div>

    <Teleport to="body">
      <div v-if="full" class="cursio-pad-full fixed inset-0 z-50 flex flex-col bg-surface text-ink">
        <div class="cursio-pad relative flex min-h-0 flex-1 items-center justify-center text-center" v-on="surface">
          <div class="pointer-events-none flex flex-col gap-1 px-6">
            <p class="text-sm" :class="isConnected ? 'text-ink' : 'text-muted'">{{ padMessage }}</p>
            <p class="text-xs text-muted">{{ t("pad.fullHints") }}</p>
          </div>
        </div>
        <!-- Outside the surface, so pressing it is not also a tap on the pad. -->
        <LpButton
          variant="soft"
          size="sm"
          square
          class="absolute right-3 top-[calc(env(safe-area-inset-top)+0.75rem)]"
          :aria-label="t('pad.exitFullScreen')"
          @click="full = false"
        >
          <LpIcon name="lucide:minimize" :size="16" />
        </LpButton>
        <div class="grid h-24 shrink-0 grid-cols-2 border-t border-line">
          <div
            class="cursio-pad flex items-center justify-center border-r border-line text-xs text-muted active:bg-line"
            v-on="leftZone"
          >
            {{ t("buttons.left") }}
          </div>
          <div class="cursio-pad flex items-center justify-center text-xs text-muted active:bg-line" v-on="rightZone">
            {{ t("buttons.right") }}
          </div>
        </div>
      </div>
    </Teleport>
  </DesktopShell>
</template>
