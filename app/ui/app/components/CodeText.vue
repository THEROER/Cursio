<script setup lang="ts">
/**
 * A line of prose whose `backticked` parts are commands to type. Translations
 * keep the backticks, since a translator sees where a command starts and ends,
 * and this draws those parts as code instead of showing the marks.
 */
const props = defineProps<{ text: string }>()

const parts = computed(() => props.text.split("`").map((part, i) => ({ part, code: i % 2 === 1 })))
</script>

<template>
  <span>
    <template v-for="(p, i) in parts" :key="i">
      <code v-if="p.code" class="rounded-xs bg-surface-soft px-1 py-px font-mono text-[0.92em] text-ink">{{ p.part }}</code>
      <template v-else>{{ p.part }}</template>
    </template>
  </span>
</template>
