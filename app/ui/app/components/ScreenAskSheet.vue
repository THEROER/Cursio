<script setup lang="ts">
import { addPluginListener, invoke, type PluginListener } from "@tauri-apps/api/core"

/**
 * A computer asking to see this phone's screen, asked here while the window
 * is in front instead of in a notification (`ScreenConsentActivity.ask`).
 *
 * "Allow" still ends in Android's own dialog wherever Android insists on one
 * per capture; where it does not (before 14, once a consent was given), it
 * starts the capture at once, and "don't ask again" skips this sheet too.
 */
const { t } = useI18n()
const toast = useCursioToast()

const asking = useState<{ computer: string; kept: boolean } | null>("screen-ask", () => null)
const always = ref(false)
const open = computed({
  get: () => !!asking.value,
  set: (value: boolean) => {
    if (!value) void answer(false)
  },
})

let listener: PluginListener | undefined
onMounted(async () => {
  listener = await addPluginListener<{ computer: string; kept: boolean }>("cursio-mesh", "screenAsk", (ask) => {
    always.value = false
    asking.value = ask
  }).catch(() => undefined)
})
onBeforeUnmount(() => void listener?.unregister())

async function answer(allow: boolean) {
  const ask = asking.value
  if (!ask) return
  asking.value = null
  try {
    await invoke("phone_screen_answer", { computer: ask.computer, allow, always: allow && always.value })
  } catch (e) {
    toast.failure(e, { title: t("screenAsk.failed") })
  }
}
</script>

<template>
  <LpDrawer
    v-model:open="open"
    direction="bottom"
    :title="t('screenAsk.title', { computer: asking?.computer ?? '' })"
    :description="t('screenAsk.description', { computer: asking?.computer ?? '' })"
  >
    <div class="flex flex-col gap-4 pb-[env(safe-area-inset-bottom)]">
      <LpCheckbox v-model="always" :label="t('screenAsk.always', { computer: asking?.computer ?? '' })" />
      <div class="flex gap-2">
        <LpButton variant="outline" class="flex-1" @click="answer(false)">{{ t("screenAsk.decline") }}</LpButton>
        <LpButton variant="action" class="flex-1" @click="answer(true)">
          <LpIcon name="lucide:cast" :size="16" />{{ t("screenAsk.allow") }}
        </LpButton>
      </div>
    </div>
  </LpDrawer>
</template>
