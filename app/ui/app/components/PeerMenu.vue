<script setup lang="ts">
import type { PeerRef } from "~/composables/usePeerMenu"

/** Right-click anywhere in the slot for the peer's actions. */
const props = defineProps<{ peer: PeerRef }>()
const { items } = usePeerMenu()
</script>

<template>
  <LpContextMenu :items="items(props.peer)">
    <slot />
  </LpContextMenu>
</template>
