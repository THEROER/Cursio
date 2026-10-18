/*
 * Tell the phone which theme the page is painted in, so the status and
 * navigation bar icons stay readable over it.
 *
 * The window draws under both bars, and Android colours their icons from the
 * system's day/night setting rather than from this page. The kit's theme is
 * the user's own choice and is dark by default, so a phone in day mode drew
 * dark icons over a dark page. Only the phone build has the command; a desktop
 * window has no system bars of its own to colour.
 */
import { invoke } from "@tauri-apps/api/core"
import { useTheme } from "@leavepulse/ui"
import { defineNuxtPlugin } from "#imports"

export default defineNuxtPlugin(() => {
  const { ready } = usePlatform()
  const theme = useTheme()
  void ready().then((platform) => {
    if (platform !== "mobile") return
    watch(
      () => theme.current.value?.mode,
      (mode) => {
        if (!mode) return
        invoke("mobile_system_bars", { dark: mode === "dark" }).catch((e) =>
          console.warn("could not colour the system bars", e),
        )
      },
      { immediate: true },
    )
  })
})
