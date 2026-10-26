/**
 * The window's own overlays — the log drawer and the command palette — opened
 * from the title bar, a hotkey, the palette or a peer's menu, so their state is
 * shared rather than owned by whichever of those happened to open them.
 */
const logsOpen = ref(false)
const logsSource = ref<"node" | "app">("node")
const logsFilter = ref("")
const paletteOpen = ref(false)

/** Open the node's log narrowed to what mentions `text`, e.g. a peer's name. */
function showInLogs(text: string) {
  logsSource.value = "node"
  logsFilter.value = text
  logsOpen.value = true
}

/** Where the theme studio remembers its tuning; the startup plugin and the settings page share it. */
export const THEME_STUDIO_KEY = "cursio-theme-studio"

export function useChrome() {
  return { logsOpen, logsSource, logsFilter, paletteOpen, showInLogs }
}
