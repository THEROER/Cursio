<script setup lang="ts">
import type { SettingView } from "~/composables/useNode"

/**
 * One setting the node listed, with the one control its kind calls for.
 *
 * The node describes the setting — its kind, its default, what a change
 * costs — so nothing here knows which setting it is showing beyond looking
 * its key up for a translated title. A control shows the change at once and
 * goes back to the value it had when `save` throws; `save` itself reports the
 * failure, so the row only has its own value to put right.
 */
const props = defineProps<{
  setting: SettingView
  /** This machine's own setting, rather than a peer's read through the mesh. */
  local: boolean
  save: (value: unknown) => Promise<void>
  reset: () => Promise<void>
}>()

const emit = defineEmits<{
  /** The user asked to restart this machine's node. */
  (e: "restart"): void
}>()

const { t, te } = useI18n()
const { isMobile } = usePlatform()

/** "Unset, so the module decides", as a value a select can hold: it takes no `null`. */
const DEFAULT = "cursio:default"
/** A level's "a number" choice, beside its words. */
const LEVEL = "cursio:level"
/** Choices past this many no longer fit a segmented control on a phone. */
const SEGMENTED_MAX = 4

function parse(json: string): unknown {
  try {
    return JSON.parse(json)
  } catch {
    return null
  }
}

const value = ref<unknown>(parse(props.setting.value_json))
const busy = ref(false)
watch(
  () => props.setting.value_json,
  (json) => {
    value.value = parse(json)
  },
)

const kind = computed(() => props.setting.kind)
const changed = computed(() => props.setting.value_json !== props.setting.default_json)

function lookup(suffix: string, fallback: string): string {
  const path = `settingsRegistry.${props.setting.key}.${suffix}`
  return te(path) ? t(path) : fallback
}

const title = computed(() => lookup("title", props.setting.title))
const hint = computed(() => lookup("hint", props.setting.hint))

/** A word the node offers, translated when the app has a name for it. */
function word(option: string): string {
  // A path segment the i18n parser would split or misread is left as it is.
  if (!/^[A-Za-z0-9_-]+$/.test(option)) return option
  return lookup(`options.${option}`, option)
}

async function change(next: unknown) {
  const before = value.value
  value.value = next
  busy.value = true
  try {
    await props.save(next)
  } catch {
    value.value = before
  } finally {
    busy.value = false
  }
}

async function resetToDefault() {
  busy.value = true
  try {
    await props.reset()
  } catch {
    // Reported by `reset`; the value shown never moved.
  } finally {
    busy.value = false
  }
}

/** Saved by the switch's own `action`, which flips back on a throw. */
function saveToggle(on: boolean) {
  return props.save(on)
}

function asOption(v: unknown): string {
  return v === null || v === undefined ? DEFAULT : String(v)
}

function fromOption(v: string): unknown {
  return v === DEFAULT ? null : v
}

const choiceOptions = computed(() => {
  if (kind.value.type !== "choice") return []
  const options = props.setting.optional ? [{ value: DEFAULT, label: t("settingsPage.default") }] : []
  for (const o of kind.value.options) options.push({ value: o, label: word(o) })
  const current = asOption(value.value)
  // A value a newer build wrote, or a hand-edited file, is still what runs.
  if (!options.some((o) => o.value === current)) options.push({ value: current, label: current })
  return options
})

/** The level a slider shows: the stored one, else the middle of the range. */
const level = ref(0)
const levelMode = computed(() => {
  if (kind.value.type !== "level") return DEFAULT
  const v = value.value
  if (v === null || v === undefined) return DEFAULT
  const text = String(v).trim()
  return kind.value.words.includes(text) ? text : LEVEL
})

watch(
  value,
  (v) => {
    if (kind.value.type !== "level") return
    const n = Number.parseInt(String(v ?? ""), 10)
    const { min, max } = kind.value
    if (Number.isFinite(n)) level.value = Math.min(max, Math.max(min, n))
    else if (level.value < min || level.value > max) level.value = Math.round((min + max) / 2)
  },
  { immediate: true },
)

const levelOptions = computed(() => {
  if (kind.value.type !== "level") return []
  const options = props.setting.optional ? [{ value: DEFAULT, label: t("settingsPage.default") }] : []
  for (const w of kind.value.words) options.push({ value: w, label: word(w) })
  options.push({ value: LEVEL, label: t("settingsPage.level") })
  return options
})

function pickLevelMode(mode: string) {
  void change(mode === LEVEL ? String(level.value) : fromOption(mode))
}

const deviceOptions = computed(() => {
  if (kind.value.type !== "device") return []
  const options = props.setting.optional ? [{ value: DEFAULT, label: t("settingsPage.defaultDevice") }] : []
  for (const name of kind.value.options) options.push({ value: name, label: name })
  const current = value.value
  // A headset unplugged is still the chosen one, and comes back with it.
  if (typeof current === "string" && !kind.value.options.includes(current)) {
    options.push({ value: current, label: t("settingsPage.notPresent", { device: current }) })
  }
  return options
})

/**
 * What a typed field holds until it is left or submitted: saving on every
 * keystroke would send half a port number to a running node.
 */
const draft = ref("")
const numberDraft = ref<number | null>(null)
watch(
  value,
  (v) => {
    draft.value = v === null || v === undefined ? "" : String(v)
    numberDraft.value = typeof v === "number" ? v : null
  },
  { immediate: true },
)

function commitText(text: string = draft.value) {
  const trimmed = text.trim()
  const current = value.value === null || value.value === undefined ? "" : String(value.value)
  if (trimmed === current) return
  if (!trimmed) {
    if (props.setting.optional) void change(null)
    else draft.value = current
    return
  }
  void change(trimmed)
}

function commitNumber() {
  const current = typeof value.value === "number" ? value.value : null
  if (numberDraft.value === current) return
  if (numberDraft.value === null) {
    if (props.setting.optional) void change(null)
    else numberDraft.value = current
    return
  }
  void change(numberDraft.value)
}

const structured = computed(() => {
  const v = value.value
  if (v === null || v === undefined) return t("settingsPage.notSet")
  const text = JSON.stringify(v)
  return text.length > 120 ? `${text.slice(0, 117)}…` : text
})

/** Where a structured setting is edited, since this page only shows it. */
const editedAt = computed<{ text: string; to?: string } | null>(() => {
  if (kind.value.type !== "structured") return null
  const key = props.setting.key
  if (key === "position" || key === "layout") return { text: t("settingsPage.editedOnPlacement"), to: "/placement" }
  if (key === "input.crossing" && props.local && !isMobile.value) {
    return { text: t("settingsPage.editedInCrossing"), to: "#crossing" }
  }
  return { text: props.local ? t("settingsPage.editedInFile") : t("settingsPage.editedInPeerFile") }
})
</script>

<template>
  <li class="flex flex-col gap-2 py-3 first:pt-0 last:pb-0">
    <div class="flex flex-wrap items-center gap-x-4 gap-y-2">
      <div class="min-w-0 flex-1 basis-56">
        <div class="flex flex-wrap items-center gap-2">
          <p class="text-sm font-medium">{{ title }}</p>
          <LpBadge v-if="setting.waits_for_restart" tone="warning" size="sm" dot>
            {{ t("settingsPage.waitsForRestart") }}
          </LpBadge>
          <LpBadge v-else-if="setting.apply === 'restart'" tone="outline" size="sm">
            {{ t("settingsPage.appliesAfterRestart") }}
          </LpBadge>
          <LpBadge v-if="setting.pinned" tone="info" size="sm">{{ t("settingsPage.pinned") }}</LpBadge>
        </div>
        <p v-if="hint" class="text-xs text-muted">{{ hint }}</p>
      </div>

      <div class="flex shrink-0 items-center gap-2">
        <LpSwitch
          v-if="kind.type === 'toggle'"
          :model-value="value === true"
          :action="saveToggle"
          :aria-label="title"
          @update:model-value="(on: boolean) => (value = on)"
          @error="() => {}"
        />

        <template v-else-if="kind.type === 'choice'">
          <LpSegmented
            v-if="choiceOptions.length <= SEGMENTED_MAX"
            :model-value="asOption(value)"
            :options="choiceOptions"
            :disabled="busy"
            :aria-label="title"
            size="sm"
            @update:model-value="(v: string) => change(fromOption(v))"
          />
          <LpSelect
            v-else
            :model-value="asOption(value)"
            :options="choiceOptions"
            :disabled="busy"
            :aria-label="title"
            class="w-56"
            @update:model-value="(v: string | string[] | undefined) => typeof v === 'string' && change(fromOption(v))"
          />
        </template>

        <LpSegmented
          v-else-if="kind.type === 'level'"
          :model-value="levelMode"
          :options="levelOptions"
          :disabled="busy"
          :aria-label="title"
          size="sm"
          @update:model-value="pickLevelMode"
        />

        <form
          v-else-if="kind.type === 'number'"
          class="w-44"
          @submit.prevent="commitNumber"
          @focusout="commitNumber"
        >
          <LpNumberField
            v-model="numberDraft"
            :min="kind.min"
            :max="kind.max"
            :step="1"
            :format-options="{ maximumFractionDigits: 0, useGrouping: false }"
            :disabled="busy"
            :aria-label="title"
            size="sm"
          />
        </form>

        <form
          v-else-if="kind.type === 'text'"
          class="w-56"
          @submit.prevent="commitText()"
          @focusout="commitText()"
        >
          <LpAutocomplete
            v-model="draft"
            :options="kind.suggestions"
            :min-chars="0"
            :disabled="busy"
            :aria-label="title"
            size="sm"
            @select="(v: string) => commitText(v)"
          />
        </form>

        <LpSelect
          v-else-if="kind.type === 'device'"
          :model-value="asOption(value)"
          :options="deviceOptions"
          :searchable="deviceOptions.length > 8"
          :disabled="busy"
          :aria-label="title"
          class="w-64"
          @update:model-value="(v: string | string[] | undefined) => typeof v === 'string' && change(fromOption(v))"
        />

        <form
          v-else-if="kind.type === 'address'"
          class="w-56"
          @submit.prevent="commitText()"
          @focusout="commitText()"
        >
          <LpInput
            v-model="draft"
            :placeholder="setting.optional ? t('settingsPage.anyAddress') : undefined"
            :disabled="busy"
            :aria-label="title"
            size="sm"
            class="font-mono"
          />
        </form>

        <LpButton
          v-if="changed && kind.type !== 'structured'"
          variant="ghost"
          size="sm"
          :disabled="busy"
          :aria-label="t('settingsPage.reset')"
          :title="t('settingsPage.reset')"
          @click="resetToDefault"
        >
          <LpIcon name="lucide:rotate-ccw" :size="14" />
        </LpButton>
      </div>
    </div>

    <div v-if="kind.type === 'level' && levelMode === LEVEL" class="flex items-center gap-4">
      <LpSlider
        v-model="level"
        :min="kind.min"
        :max="kind.max"
        :step="1"
        :disabled="busy"
        :aria-label="title"
        class="flex-1"
        @change="(n: number) => change(String(n))"
      />
      <span class="w-20 text-right font-mono text-xs tabular-nums">{{ level }} {{ kind.unit }}</span>
    </div>

    <div v-if="editedAt" class="flex flex-col gap-1">
      <code class="truncate rounded-control bg-surface-soft px-2 py-1 font-mono text-xs text-muted">{{ structured }}</code>
      <p class="text-xs text-muted">
        <NuxtLink v-if="editedAt.to" :to="editedAt.to" class="underline underline-offset-2 hover:text-ink">
          {{ editedAt.text }}
        </NuxtLink>
        <template v-else>{{ editedAt.text }}</template>
      </p>
    </div>

    <div v-if="setting.waits_for_restart && local && !isMobile">
      <LpButton variant="soft" size="sm" @click="emit('restart')">
        <LpIcon name="lucide:rotate-cw" :size="14" />
        {{ t("settings.restart") }}
      </LpButton>
    </div>
  </li>
</template>
