<script setup lang="ts">
withDefaults(defineProps<{ size?: number }>(), { size: 20 })
</script>

<template>
  <svg :width="size" :height="size" viewBox="0 0 24 24" fill="currentColor" role="img" aria-label="Cursio">
    <polygon points="3.80,6.06 20.20,6.06 20.20,3.59 3.80,3.59" /><polygon points="2.73,5.44 10.93,19.79 13.07,18.56 4.87,4.21" /><polygon points="19.13,4.21 10.93,18.56 13.07,19.79 21.27,5.44" /><circle cx="3.80" cy="4.82" r="3.80" /><circle cx="20.20" cy="4.82" r="3.80" /><circle cx="12.00" cy="19.18" r="3.80" />
  </svg>
</template>
