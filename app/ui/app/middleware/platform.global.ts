import type { Platform } from "~/composables/usePlatform"

/**
 * Pages that exist only on one face of the app. Arranging machines places a
 * desktop's screen edges for its cursor; a phone has no edge a cursor crosses,
 * and the command that saves a position is not registered there. The touchpad
 * drives a peer through the phone's own links, and a desktop build registers
 * none of its commands. A stale route (a remembered palette pick, a typed
 * URL) lands on the home page instead of on a page whose every action fails.
 */
const ONLY_ON: Record<string, Platform> = {
  "/placement": "desktop",
  "/touchpad": "mobile",
}

export default defineNuxtRouteMiddleware(async (to) => {
  const only = Object.entries(ONLY_ON).find(([path]) => to.path.startsWith(path))?.[1]
  if (!only) return
  if ((await usePlatform().ready()) !== only) return navigateTo("/")
})
