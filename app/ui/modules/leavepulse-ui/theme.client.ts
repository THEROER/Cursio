/*
 * Paint the saved theme before the first frame.
 *
 * The kit's own plugin also reads a cookie so a server render can inline the
 * theme; this app is a static SPA with no server, so what is left is the
 * client half: `init` restores the theme saved in localStorage or falls back
 * to the kit's dark preset.
 *
 * What the kit saves is the whole token set, not a name, so a choice made
 * under an older kit keeps painting that kit's colours: a "Dark" saved while
 * the kit's dark was the navy LeavePulse theme stays navy, and a theme the kit
 * has since dropped ("LeavePulse", "Onyx") stays applied with no entry in the
 * switcher to mark. The app offers nothing but the kit's presets, so the saved
 * choice is resolved by name against them and re-saved when it differs; a
 * name that no preset carries any more becomes the dark one.
 *
 * On WebKitGTK — the webview on Linux — the View Transitions API is hidden
 * first. The engine declares `document.startViewTransition`, so the kit's
 * theme switch takes the animated path, but animating
 * `::view-transition-old(root)` crashes the whole app (a SIGSEGV inside
 * libwebkit2gtk the moment the theme changed). The LeavePulse launcher met the
 * same crash and bypasses the transition; hiding the API here makes every kit
 * component fall back to an instant switch without each needing to know.
 */
import { dark, isBrokenBackdropEngine, presets, themeToCssVars, useTheme, type TokenSet } from "@leavepulse/ui"
import { defineNuxtPlugin } from "#imports"

function preset(saved: TokenSet): TokenSet {
  return Object.values(presets).find((p) => p.name === saved.name) ?? dark
}

export default defineNuxtPlugin(() => {
  if (isBrokenBackdropEngine()) {
    Object.defineProperty(document, "startViewTransition", {
      value: undefined,
      configurable: true,
    })
  }
  const theme = useTheme()
  const saved = theme.init(dark)
  if (theme.system.value) return
  const wanted = preset(saved)
  if (saved.name !== wanted.name || themeToCssVars(saved) !== themeToCssVars(wanted)) theme.apply(wanted)
})
