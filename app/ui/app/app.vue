<script setup lang="ts">
import { listen, type UnlistenFn } from "@tauri-apps/api/event"

// The tray opens the window on a page ("Arrange machines…" lands on the
// placement editor), so the Rust side names the route and the router follows.
let unlisten: UnlistenFn | undefined
onMounted(async () => {
  unlisten = await listen<string>("navigate", (event) => {
    void navigateTo(event.payload)
  })
})
onBeforeUnmount(() => unlisten?.())
</script>

<template>
  <NuxtPage />
  <LpToaster />
</template>
