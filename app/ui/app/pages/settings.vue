<script setup lang="ts">
import type { TocLink } from "@leavepulse/ui"
import { invoke } from "@tauri-apps/api/core"
import type { ServiceState } from "~/composables/useNode"

const { t } = useI18n()
const { status, restart, setName } = useNode()
const { isMobile, ready } = usePlatform()
const toast = useCursioToast()

/** `identity::MAX_NAME_CHARS`; the node refuses anything longer. */
const NAME_MAX = 40

// The palette links to a section (`/settings#calls`). Most sections wait for
// the node's status, so the router's own jump finds nothing to land on;
// this one waits for the section to exist, then scrolls to it. The node's own
// settings arrive later still, so a section not there yet is scrolled to by
// `collectToc` when it appears.
const route = useRoute()
let awaitedSection: string | null = null
watch(
  [() => route.hash, () => !!status.value],
  async ([hash]) => {
    if (!hash) return
    await nextTick()
    const target = document.getElementById(hash.slice(1))
    awaitedSection = target ? null : hash.slice(1)
    target?.scrollIntoView({ behavior: "smooth", block: "start" })
  },
  { immediate: true },
)

const nameDraft = ref("")
const nameBusy = ref(false)

// Follows the node's name, so a rename from elsewhere (the CLI, a restart)
// shows; it never changes while the user types, so their draft survives.
watch(
  () => status.value?.node.name,
  (name) => {
    if (name !== undefined) nameDraft.value = name
  },
  { immediate: true },
)

const nameTrimmed = computed(() => nameDraft.value.trim())
/** Counted in characters, as the node counts, not UTF-16 units. */
const nameError = computed(() => {
  if (!nameTrimmed.value) return t("settings.nameEmpty")
  if ([...nameTrimmed.value].length > NAME_MAX) return t("settings.nameTooLong", { max: NAME_MAX })
  return undefined
})
const nameChanged = computed(() => !!status.value && nameTrimmed.value !== status.value.node.name)
const nameWaitsForRestart = computed(() => !isMobile.value && !!status.value?.restart_needed.includes("name"))

async function saveName() {
  if (!nameChanged.value || nameError.value) return
  nameBusy.value = true
  try {
    await setName(nameTrimmed.value)
    if (isMobile.value) {
      // The phone's links greet peers with the name only when they come up,
      // and nothing here is costly to bring back, so it restarts at once
      // rather than leaving peers on the old name until the next reboot.
      await mobileRestart()
      toast.success(t("settings.renamedPhone"))
    } else {
      toast.success(t("settings.renamed"))
    }
  } catch (e) {
    toast.failure(e, { title: t("settings.renameFailed"), retry: saveName })
  } finally {
    nameBusy.value = false
  }
}

const callSetup = computed(() => status.value?.call_setup ?? null)
/**
 * Phones linked over the mesh that this computer has no Bluetooth pairing
 * with: their calls ring here, but their voice cannot come through, and
 * nothing else on the page says why they are missing from the list above.
 * Matched by name, as `calls_hub` does, since Android hides its address.
 */
const unpairedPhones = computed(() => {
  const paired = new Set((callSetup.value?.phones ?? []).map((phone) => phone.name.toLowerCase()))
  return (status.value?.peers ?? [])
    .filter((peer) => peer.link?.device_kind === "Phone" && !paired.has(peer.name.toLowerCase()))
    .map((peer) => peer.name)
})

function serviceLabel(state: ServiceState) {
  if (state.state === "running") return t("settings.callsRunning")
  if (state.state === "failed") return t("settings.callsFailed")
  return t("settings.callsStarting")
}

function serviceStatus(state: ServiceState) {
  if (state.state === "running") return "online"
  if (state.state === "failed") return "down"
  return "starting"
}

const nodeFacts = computed(() =>
  status.value
    ? [
        { key: "build", label: t("settings.build"), value: status.value.node.build, mono: true, copyable: true },
        ...(isMobile.value
          ? []
          : [
              {
                key: "installed",
                label: t("settings.installedVersion"),
                value: status.value.node.installed_version ?? t("settings.notInstalled"),
                mono: true,
              },
              { key: "pid", label: t("settings.pid"), value: status.value.node.pid || null, mono: true },
            ]),
      ]
    : [],
)

interface Autostart {
  node: boolean
  tray: boolean
}

const autostart = ref<Autostart | null>(null)

onMounted(async () => {
  // A phone's node is started by its foreground service, at boot or by the
  // app; there is no login item to read, and the command does not exist there.
  if ((await ready()) === "mobile") return
  try {
    autostart.value = await invoke<Autostart>("autostart_get")
  } catch (e) {
    toast.failure(e, { title: t("settings.autostartFailed") })
  }
})

/** One login item switched; the other is sent as it stands. */
async function setAutostart(which: keyof Autostart, on: boolean) {
  const next = { ...autostart.value!, [which]: on }
  autostart.value = await invoke<Autostart>("autostart_set", { node: next.node, tray: next.tray })
  toast.success(t("settings.saved"))
}

function autostartFailed(which: keyof Autostart) {
  return (e: unknown) => {
    const on = !autostart.value![which]
    toast.failure(e, { title: t("settings.autostartFailed"), retry: () => setAutostart(which, on) })
  }
}

const confirmRestart = ref(false)
const restarting = ref(false)
// The sections shown depend on the platform and on what the node reports, so
// the contents are read off the page rather than kept as a second list of the
// same conditions.
const sectionsBox = ref<HTMLElement | null>(null)
const toc = ref<TocLink[]>([])
function collectToc() {
  toc.value = [...(sectionsBox.value?.querySelectorAll<HTMLElement>(":scope > section[id]") ?? [])].map((s) => ({
    id: s.id,
    text: s.querySelector("h1, h2, h3, h4")?.textContent?.trim() || s.id,
  }))
  const awaited = awaitedSection && document.getElementById(awaitedSection)
  if (awaited) {
    awaitedSection = null
    awaited.scrollIntoView({ behavior: "smooth", block: "start" })
  }
}
let watcher: MutationObserver | undefined
onMounted(() => {
  collectToc()
  if (!sectionsBox.value) return
  watcher = new MutationObserver(collectToc)
  watcher.observe(sectionsBox.value, { childList: true })
})
onBeforeUnmount(() => watcher?.disconnect())

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
  <div>
    <div class="xl:grid xl:grid-cols-[minmax(0,1fr)_11rem] xl:items-start xl:gap-8">
      <div ref="sectionsBox" class="flex flex-col gap-6">
        <section v-if="status" id="name" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('settings.nameTitle')" :description="t('settings.nameDescription')" icon="lucide:tag" />
          <LpCard padded class="flex flex-col gap-3">
            <form class="flex flex-wrap items-end gap-3" @submit.prevent="saveName">
              <LpInput
                v-model="nameDraft"
                :label="t('settings.nameLabel')"
                :error="nameChanged ? nameError : undefined"
                :disabled="nameBusy"
                class="min-w-0 flex-1 basis-56"
              />
              <LpButton
                type="submit"
                variant="solid"
                :loading="nameBusy"
                :loading-label="t('common.working')"
                :disabled="!nameChanged || !!nameError"
              >
                {{ t("settings.rename") }}
              </LpButton>
            </form>
            <p v-if="isMobile" class="text-xs text-muted">{{ t("settings.namePhoneHint") }}</p>
            <LpAlert v-else-if="nameWaitsForRestart" variant="warning" :title="t('settings.nameWaits')">
              <LpButton variant="soft" size="sm" @click="confirmRestart = true">{{ t("settings.restart") }}</LpButton>
            </LpAlert>
          </LpCard>
        </section>

        <section id="theme" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('settings.themeTitle')" :description="t('settings.themeDescription')" icon="lucide:palette" />
          <LpCard padded>
            <LpThemeStudio :storage-key="THEME_STUDIO_KEY" :hide="['export']" :labels="{ title: '' }" />
          </LpCard>
        </section>

        <SettingsRegistry @restart="confirmRestart = true" />

        <section v-if="status && !isMobile" id="calls" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('settings.callsTitle')" :description="t('settings.callsDescription')" icon="lucide:phone" />
          <LpCard v-if="!callSetup" padded>
            <p class="text-sm text-muted">{{ t("settings.callsUnsupported") }}</p>
          </LpCard>
          <LpCard v-else padded class="flex flex-col gap-4">
            <p v-if="callSetup.headset === false" class="text-sm text-muted">{{ t("settings.callsUnsupported") }}</p>
            <ul v-else class="flex flex-col divide-y divide-line">
              <li class="flex flex-col gap-2 pb-3">
                <div class="flex items-center gap-4">
                  <p class="min-w-0 flex-1 text-sm font-medium">{{ t("settings.callsRoles") }}</p>
                  <LpBadge :tone="callSetup.roles_taken ? 'success' : 'warning'" dot size="sm">
                    {{ callSetup.roles_taken ? t("settings.callsRolesTaken") : t("settings.callsFailed") }}
                  </LpBadge>
                </div>
                <template v-if="!callSetup.roles_taken">
                  <p class="text-xs text-muted">{{ t("settings.callsRolesKept") }}</p>
                  <LpCodeBlock model-value="cursio bluetooth install" />
                </template>
              </li>
              <li
                v-for="row in [
                  { label: t('settings.callsHandsFree'), state: callSetup.hands_free },
                  { label: t('settings.callsSpeaker'), state: callSetup.speaker },
                ]"
                :key="row.label"
                class="flex flex-col gap-1 py-3"
              >
                <div class="flex items-center gap-4">
                  <p class="min-w-0 flex-1 text-sm font-medium">{{ row.label }}</p>
                  <LpStatus :state="serviceStatus(row.state)" :label="serviceLabel(row.state)" variant="pill" size="sm" />
                </div>
                <p v-if="row.state.state === 'failed'" class="text-xs text-muted">{{ row.state.reason }}</p>
              </li>
              <li class="flex flex-col gap-2 pt-3">
                <p class="text-sm font-medium">{{ t("settings.callsPhones") }}</p>
                <p v-if="!callSetup.phones.length" class="text-xs text-muted">{{ t("settings.callsNoPhones") }}</p>
                <div v-for="phone in callSetup.phones" :key="phone.address" class="flex items-center gap-3">
                  <LpIcon name="lucide:smartphone" :size="16" class="shrink-0 text-muted" />
                  <span class="min-w-0 flex-1 truncate text-sm">{{ phone.name }}</span>
                  <span class="font-mono text-xs text-muted">{{ phone.address }}</span>
                  <LpStatus
                    v-if="phone.connected"
                    state="online"
                    :label="t('settings.callsConnected')"
                    variant="pill"
                    size="sm"
                  />
                </div>
                <p v-if="unpairedPhones.length" class="text-xs text-muted">
                  {{ t("settings.callsUnpaired", { phones: unpairedPhones.join(", ") }) }}
                </p>
              </li>
            </ul>
          </LpCard>
        </section>

        <section v-if="status && isMobile" id="phone-audio" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('phoneAudio.title')" :description="t('phoneAudio.description')" icon="lucide:speaker" />
          <PhoneAudioCard />
        </section>

        <section v-if="status && isMobile" id="phone-screen" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('phoneScreen.title')" :description="t('phoneScreen.description')" icon="lucide:cast" />
          <PhoneScreenCard />
        </section>

        <section v-if="status && isMobile" id="phone-shell" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('phoneShell.section')" :description="t('phoneShell.description')" icon="lucide:terminal" />
          <PhoneShellCard />
        </section>

        <section v-if="status && !isMobile" id="crossing" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('crossing.title')" :description="t('crossing.description')" icon="lucide:panel-bottom" />
          <EdgeCrossingCard />
        </section>

        <section v-if="status" id="playing" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('streams.title')" :description="t('streams.description')" icon="lucide:volume-2" />
          <LpCard padded>
            <PlayingStreams show="sound" />
          </LpCard>
        </section>

        <section v-if="status?.streams?.some((s) => s.kind === 'microphone')" id="microphones" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('streams.microphones')" :description="t('streams.microphonesDescription')" icon="lucide:mic" />
          <LpCard padded>
            <PlayingStreams show="microphones" />
          </LpCard>
        </section>

        <section v-if="!isMobile" id="autostart" class="flex scroll-mt-4 flex-col gap-3">
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
                  v-model="autostart.node"
                  :action="(on: boolean) => setAutostart('node', on)"
                  :aria-label="t('settings.autostartNode')"
                  @error="autostartFailed('node')"
                />
              </li>
              <li class="flex items-center gap-4 pt-3">
                <div class="min-w-0 flex-1">
                  <p class="text-sm font-medium">{{ t("settings.autostartTray") }}</p>
                  <p class="text-xs text-muted">{{ t("settings.autostartTrayHint") }}</p>
                </div>
                <LpSwitch
                  v-model="autostart.tray"
                  :action="(on: boolean) => setAutostart('tray', on)"
                  :aria-label="t('settings.autostartTray')"
                  @error="autostartFailed('tray')"
                />
              </li>
            </ul>
          </LpCard>
        </section>

        <section id="node" class="flex scroll-mt-4 flex-col gap-3">
          <LpSection :title="t('settings.nodeTitle')" icon="lucide:server" />
          <LpCard padded class="flex flex-col gap-3">
            <template v-if="status">
              <LpCodeBlock :model-value="status.config_path" :title="t('settings.configFile')" />
              <p v-if="status.pinned.length" class="text-xs text-muted">
                {{ t("settings.pinned", { fields: status.pinned.join(", ") }) }}
              </p>
              <LpDataList :items="nodeFacts" layout="horizontal" size="sm" :labels="{ empty: '—' }" />
            </template>
            <div v-if="!isMobile">
              <LpButton variant="soft" size="sm" :disabled="!status" @click="confirmRestart = true">
                <LpIcon name="lucide:rotate-cw" :size="14" />
                {{ t("settings.restart") }}
              </LpButton>
            </div>
          </LpCard>
        </section>
      </div>
      <aside v-if="!isMobile" class="sticky top-[var(--lp-shell-top)] hidden self-start xl:block">
        <LpTableOfContents :links="toc" :hover-follow="false" :title="t('settings.onThisPage')" />
      </aside>
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
  </div>
</template>
