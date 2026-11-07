<script setup lang="ts">
/**
 * The shell, once for every page.
 *
 * Each page used to draw its own `DesktopShell`, so moving between pages tore
 * the rail, the title bar and the alerts down and built them again, and
 * nothing could animate the move without animating the shell with it. Here
 * the shell stays and only the page inside it changes (see `app.vue`). A page
 * that owns the whole content area says so with `definePageMeta({ fullBleed:
 * true })`.
 */
const route = useRoute()
</script>

<template>
  <DesktopShell :full-bleed="!!route.meta.fullBleed">
    <slot />
  </DesktopShell>
</template>
