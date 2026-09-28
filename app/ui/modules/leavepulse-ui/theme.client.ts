/*
 * Paint the saved theme before the first frame.
 *
 * The kit's own plugin also reads a cookie so a server render can inline the
 * theme; this app is a static SPA with no server, so what is left is the
 * client half: `init` restores the theme saved in localStorage (the full token
 * set, including any the user tweaked) or falls back to the product's dark one.
 *
 * On WebKitGTK — the webview on Linux — the View Transitions API is hidden
 * first. The engine declares `document.startViewTransition`, so the kit's
 * theme switch takes the animated path, but animating
 * `::view-transition-old(root)` crashes the whole app (a SIGSEGV inside
 * libwebkit2gtk the moment the theme changed). The LeavePulse launcher met the
 * same crash and bypasses the transition; hiding the API here makes every kit
 * component fall back to an instant switch without each needing to know.
 */
import { isBrokenBackdropEngine, leavepulse, useTheme } from "@leavepulse/ui"
import { defineNuxtPlugin } from "#imports"

export default defineNuxtPlugin(() => {
  if (isBrokenBackdropEngine()) {
    Object.defineProperty(document, "startViewTransition", {
      value: undefined,
      configurable: true,
    })
  }
  useTheme().init(leavepulse)
})
