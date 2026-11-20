<script setup lang="ts">
import type { SettingView } from "~/composables/useNode"

/**
 * Every setting a node lists, this machine's or a peer's, grouped as the node
 * groups them.
 *
 * The page names no setting itself: the node says which exist on its build
 * and platform, what each takes and what changing it costs, so a setting a
 * module gains shows here without a change to the app — and a peer's list is
 * its own, not this machine's guessed at. Rendered as sibling `section`s so
 * the settings page's contents pick each group up.
 */
const emit = defineEmits<{
  /** Restart this machine's node, for a setting that waits for one. */
  (e: "restart"): void
}>()

const { t, te } = useI18n()
const { status, settings, setSetting, resetSetting } = useNode()
const toast = useCursioToast()

/** This machine, as a value the select can hold: a peer may be called anything else. */
const HERE = "cursio:here"

const machine = ref(HERE)
const list = ref<SettingView[] | null>(null)
const failure = ref<string | null>(null)
/** Only the newest request's answer is shown: a slow peer must not overwrite the machine picked after it. */
let asked = 0

const peer = computed(() => (machine.value === HERE ? null : machine.value))

const machines = computed(() => {
  const options = [{ value: HERE, label: t("settingsPage.thisMachine", { name: status.value?.node.name ?? "" }) }]
  for (const p of status.value?.peers ?? []) {
    if (p.presence === "online" && p.link) options.push({ value: p.name, label: p.name })
  }
  // A peer that went away stays picked, so the select does not go blank under
  // the error that says it cannot answer.
  if (!options.some((o) => o.value === machine.value)) options.push({ value: machine.value, label: machine.value })
  return options
})

async function load() {
  const mine = ++asked
  const target = peer.value
  try {
    const next = await settings(target)
    if (mine !== asked) return
    list.value = next
    failure.value = null
  } catch (e) {
    if (mine !== asked) return
    list.value = null
    failure.value = errorText(e)
  }
}

watch(machine, () => {
  list.value = null
  failure.value = null
  void load()
})

// Also after the node comes back from a restart, when what waited has applied.
watch(
  () => !!status.value,
  (up) => {
    if (up) void load()
  },
  { immediate: true },
)

function titleOf(setting: SettingView): string {
  const path = `settingsRegistry.${setting.key}.title`
  return te(path) ? t(path) : setting.title
}

function sectionTitle(section: string): string {
  const path = `settingsPage.sections.${section}`
  if (te(path)) return t(path)
  return section.charAt(0).toUpperCase() + section.slice(1)
}

const ICONS: Record<string, string> = {
  modules: "lucide:blocks",
  desk: "lucide:layout-grid",
  input: "lucide:mouse-pointer-2",
  audio: "lucide:audio-lines",
  calls: "lucide:phone",
  files: "lucide:folder",
  network: "lucide:network",
  bluetooth: "lucide:bluetooth",
  updates: "lucide:download",
  airplay: "lucide:airplay",
  apps: "lucide:app-window",
}

const groups = computed(() => {
  const bySection = new Map<string, SettingView[]>()
  for (const s of list.value ?? []) {
    const group = bySection.get(s.section)
    if (group) group.push(s)
    else bySection.set(s.section, [s])
  }
  return [...bySection].map(([section, items]) => ({ section, items }))
})

/** After a change, what the node now says about the setting decides the toast. */
async function settle(key: string) {
  const target = machine.value
  await load()
  if (machine.value !== target) return
  const now = list.value?.find((s) => s.key === key)
  if (now?.pinned) toast.warning(t("settingsPage.savedPinned"))
  else if (now?.waits_for_restart) toast.success(t("settingsPage.savedAfterRestart"))
  else toast.success(t("settings.saved"))
}

function saver(setting: SettingView) {
  const target = peer.value
  const save = async (value: unknown): Promise<void> => {
    try {
      await setSetting(setting.key, value, target)
    } catch (e) {
      toast.failure(e, {
        title: t("settingsPage.failed", { setting: titleOf(setting) }),
        retry: () => save(value).catch(() => {}),
      })
      throw e
    }
    await settle(setting.key)
  }
  return save
}

function resetter(setting: SettingView) {
  const target = peer.value
  const reset = async (): Promise<void> => {
    try {
      await resetSetting(setting.key, target)
    } catch (e) {
      toast.failure(e, {
        title: t("settingsPage.resetFailed", { setting: titleOf(setting) }),
        retry: () => reset().catch(() => {}),
      })
      throw e
    }
    await settle(setting.key)
  }
  return reset
}
</script>

<template>
  <section v-if="status" id="machine-settings" class="flex scroll-mt-4 flex-col gap-3">
    <LpSection :title="t('settingsPage.title')" :description="t('settingsPage.description')" icon="lucide:sliders-horizontal">
      <template v-if="machines.length > 1" #actions>
        <LpSelect v-model="machine" :options="machines" :aria-label="t('settingsPage.machine')" class="w-56" />
      </template>
    </LpSection>
    <LpAlert
      v-if="failure"
      variant="danger"
      :title="peer ? t('settingsPage.peerFailed', { machine: peer }) : t('settingsPage.loadFailed')"
    >
      <p class="text-sm">{{ failure }}</p>
      <LpButton variant="soft" size="sm" class="mt-2" @click="load">{{ t("toast.retry") }}</LpButton>
    </LpAlert>
    <LpSkeleton v-else-if="!list" rounded="card" class="h-48" />
  </section>

  <section
    v-for="group in groups"
    :id="`settings-${group.section}`"
    :key="`${machine}:${group.section}`"
    class="flex scroll-mt-4 flex-col gap-3"
  >
    <LpSection :title="sectionTitle(group.section)" :icon="ICONS[group.section] ?? 'lucide:puzzle'" />
    <LpCard padded>
      <ul class="flex flex-col divide-y divide-line">
        <SettingControl
          v-for="setting in group.items"
          :key="setting.key"
          :setting="setting"
          :local="!peer"
          :save="saver(setting)"
          :reset="resetter(setting)"
          @restart="emit('restart')"
        />
      </ul>
    </LpCard>
  </section>
</template>
