/**
 * Pages that exist only on one face of the app. Arranging machines places a
 * desktop's screen edges for its cursor; a phone has no edge a cursor crosses,
 * and the command that saves a position is not registered there. A stale
 * route (a remembered palette pick, a typed URL) lands on the home page
 * instead of on a page whose every action fails.
 */
const DESKTOP_ONLY = ["/placement"]

export default defineNuxtRouteMiddleware(async (to) => {
  if (!DESKTOP_ONLY.some((path) => to.path.startsWith(path))) return
  if ((await usePlatform().ready()) === "mobile") return navigateTo("/")
})
