<script setup lang="ts">
import { getCurrentWindow } from "@tauri-apps/api/window"

const { t } = useI18n()
const toast = useCursioToast()

/**
 * The window draws its own title bar (`decorations: false`, as the LeavePulse
 * launcher does), so it has to offer what the system bar did. Close hides: the
 * Rust side turns a close request into a hide, because the tray is the app
 * and the node keeps running either way.
 */
async function act(action: "minimize" | "toggleMaximize" | "close") {
  try {
    await getCurrentWindow()[action]()
  } catch (e) {
    toast.failure(e, { title: t("window.failed") })
  }
}
</script>

<template>
  <div class="flex items-center gap-0.5">
    <LpButton variant="muted" size="sm" square :aria-label="t('window.minimize')" @click="act('minimize')">
      <LpIcon name="lucide:minus" :size="15" />
    </LpButton>
    <LpButton variant="muted" size="sm" square :aria-label="t('window.maximize')" @click="act('toggleMaximize')">
      <LpIcon name="lucide:square" :size="13" />
    </LpButton>
    <LpButton
      variant="muted"
      size="sm"
      square
      class="hover:!bg-danger hover:!text-ink-inverse"
      :aria-label="t('window.close')"
      @click="act('close')"
    >
      <LpIcon name="lucide:x" :size="15" />
    </LpButton>
  </div>
</template>
