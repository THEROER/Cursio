<script setup lang="ts">
import type { Problem } from "~/composables/useNode"

defineProps<{ problems: Problem[] }>()

const { t } = useI18n()

/**
 * A hint is prose that often ends in the command that fixes it ("…in an
 * administrator shell: netsh advfirewall …"). The command goes in a code block
 * with a copy button, because retyping a firewall rule by hand is how it gets
 * mistyped; the prose before it stays prose.
 */
const COMMAND = /^(sudo|netsh|winget|cursio|systemctl|modprobe|usbipd|adb|pkexec|gsettings|gnome-extensions|powershell)\b/

function split(hint: string): { prose: string; command: string | null } {
  const at = hint.lastIndexOf(": ")
  if (at >= 0) {
    const tail = hint.slice(at + 2).trim()
    if (COMMAND.test(tail)) return { prose: hint.slice(0, at), command: tail }
  }
  if (COMMAND.test(hint.trim())) return { prose: "", command: hint.trim() }
  return { prose: hint, command: null }
}

const icon = { Info: "lucide:info", Warning: "lucide:triangle-alert", Error: "lucide:octagon-alert" } as const
const tone = { Info: "text-brand", Warning: "text-accent", Error: "text-danger" } as const
const badge = { Info: "brand", Warning: "warning", Error: "danger" } as const
</script>

<template>
  <LpEmptyState
    v-if="problems.length === 0"
    compact
    icon="lucide:circle-check"
    :title="t('problems.none')"
    :description="t('problems.noneHint')"
  />
  <ul v-else class="flex flex-col gap-2">
    <li v-for="problem in problems" :key="problem.id">
      <LpCard padded class="flex gap-3">
        <LpIcon :name="icon[problem.severity]" :size="18" class="mt-0.5 shrink-0" :class="tone[problem.severity]" />
        <div class="flex min-w-0 flex-1 flex-col gap-2">
          <p class="text-sm font-medium">{{ problem.summary }}</p>
          <template v-if="problem.hint">
            <p v-if="split(problem.hint).prose" class="text-xs text-muted">{{ split(problem.hint).prose }}</p>
            <LpCodeBlock
              v-if="split(problem.hint).command"
              :model-value="split(problem.hint).command!"
              lang="bash"
              wrap
            />
          </template>
        </div>
        <LpTooltip :content="problem.id">
          <LpBadge size="sm" :tone="badge[problem.severity]" class="shrink-0 self-start">
            {{ t(`severity.${problem.severity}`) }}
          </LpBadge>
        </LpTooltip>
      </LpCard>
    </li>
  </ul>
</template>
