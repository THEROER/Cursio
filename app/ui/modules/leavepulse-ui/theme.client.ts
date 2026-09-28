/*
 * Paint the saved theme before the first frame.
 *
 * The kit's own plugin also reads a cookie so a server render can inline the
 * theme; this app is a static SPA with no server, so what is left is the
 * client half: `init` restores the theme saved in localStorage (the full token
 * set, including any the user tweaked) or falls back to the product's dark one.
 */
import { leavepulse, useTheme } from "@leavepulse/ui"
import { defineNuxtPlugin } from "#imports"

export default defineNuxtPlugin(() => {
  useTheme().init(leavepulse)
})
