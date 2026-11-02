<script setup lang="ts">
/**
 * Builds this phone holds and only the person can install: Cursio's own and
 * other applications'. Phone only. The notification offering each one was the
 * only way to its installer, and one swipe or a mis-tap in the installer lost
 * it for good; here the Install button stays until the build is on.
 */
import type { FetchingBuild, WaitingBuild } from "~/composables/useNode"

const { t } = useI18n()
const { status } = useNode()
const toast = useCursioToast()
const busy = ref<string | null>(null)

const waiting = computed(() => status.value?.waiting_builds ?? [])
const fetching = computed(() => status.value?.fetching_builds ?? [])

const percentOf = (build: FetchingBuild) =>
  build.total ? Math.min(100, (build.received / build.total) * 100) : 0

const keyOf = (build: { app: string | null; version: string }) =>
  `${build.app ?? ""}@${build.version}`

async function install(build: WaitingBuild) {
  busy.value = keyOf(build)
  try {
    await mobileInstallBuild(build.app)
  } catch (e) {
    toast.failure(e, { title: t("phoneUpdate.failed") })
  } finally {
    busy.value = null
  }
}
</script>

<template>
  <LpCard v-if="waiting.length || fetching.length" padded class="flex flex-col gap-3">
    <div v-for="build in fetching" :key="`in:${keyOf(build)}`" class="flex flex-col gap-1">
      <p class="truncate text-sm font-medium">
        {{ t("phoneUpdate.receiving", { name: build.app ?? "Cursio", version: build.version }) }}
      </p>
      <p class="truncate text-xs text-muted">{{ t("phoneUpdate.from", { machine: build.from }) }}</p>
      <LpProgress :value="percentOf(build)" caption />
    </div>
    <div v-for="build in waiting" :key="keyOf(build)" class="flex items-center gap-3">
      <img
        v-if="build.icon"
        :src="`data:image/png;base64,${build.icon}`"
        alt=""
        class="size-8 shrink-0 rounded-lg"
      />
      <LpIcon v-else name="lucide:download" :size="18" class="shrink-0 text-brand" />
      <div class="min-w-0 flex-1">
        <p class="truncate text-sm font-medium">
          {{ t("phoneUpdate.ready", { name: build.label ?? build.app ?? "Cursio", version: build.version }) }}
        </p>
        <p v-if="build.from" class="truncate text-xs text-muted">
          {{ t("phoneUpdate.from", { machine: build.from }) }}
        </p>
      </div>
      <LpButton class="shrink-0" :disabled="busy !== null" @click="install(build)">
        {{ t("phoneUpdate.install") }}
      </LpButton>
    </div>
  </LpCard>
</template>
