<script setup lang="ts">
import { invoke } from "@tauri-apps/api/core"
import { listen, type UnlistenFn } from "@tauri-apps/api/event"

// The tray opens the window on a page ("Arrange machines…" lands on the
// placement editor), so the Rust side names the route and the router follows.
// A page named at launch (`cursio-app --page messages`) was asked for before
// anything listened, so it is taken once the listener is up.
let unlisten: UnlistenFn | undefined
onMounted(async () => {
  unlisten = await listen<string>("navigate", (event) => {
    void navigateTo(event.payload)
  })
  // A phone has no such command; the refusal is the answer "none".
  const start = await invoke<string | null>("app_start_page").catch(() => null)
  if (start) void navigateTo(start)
})
onBeforeUnmount(() => unlisten?.())
</script>

<template>
  <NuxtPage />
  <LpToaster />
</template>
