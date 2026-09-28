<script setup lang="ts">
import type { PeerRef } from "~/composables/usePeerMenu"

/**
 * The same actions as the right-click menu, behind a ⋯ button: a touch screen
 * or a keyboard has no right click, and a menu nobody can see is there is one
 * nobody finds.
 */
const props = defineProps<{ peer: PeerRef }>()
const { t } = useI18n()
const { items } = usePeerMenu()
</script>

<template>
  <LpDropdownMenu :items="items(props.peer)">
    <template #trigger>
      <LpButton
        variant="muted"
        size="sm"
        square
        :aria-label="t('peerMenu.open', { name: props.peer.name })"
        @click.stop
      >
        <LpIcon name="lucide:ellipsis" :size="16" />
      </LpButton>
    </template>
  </LpDropdownMenu>
</template>
