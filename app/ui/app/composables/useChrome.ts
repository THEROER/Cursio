/**
 * The window's own overlays — the log drawer and the command palette — opened
 * from the title bar, a hotkey or the palette itself, so their state is shared
 * rather than owned by whichever of those happened to open them.
 */
const logsOpen = ref(false)
const paletteOpen = ref(false)

export function useChrome() {
  return { logsOpen, paletteOpen }
}
