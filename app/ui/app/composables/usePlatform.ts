import { invoke } from "@tauri-apps/api/core"

export type Platform = "desktop" | "mobile"

/**
 * Which face the app shows: a window in front of a desktop daemon, or the
 * phone's node itself.
 *
 * The Rust side is the authority (`app_platform`), because the two builds
 * register different commands and only it knows which. Asking takes a round
 * trip, though, and the first frame cannot wait for it without flashing the
 * desktop rail on a phone — so the user agent seeds the answer and the command
 * corrects it. A narrow desktop window is not a phone: width decides layout,
 * this decides which commands exist.
 */
function guess(): Platform {
  if (typeof navigator === "undefined") return "desktop"
  return /Android|iPhone|iPad|iPod/i.test(navigator.userAgent) ? "mobile" : "desktop"
}

const platform = ref<Platform>(guess())
let asked: Promise<Platform> | null = null

function ask(): Promise<Platform> {
  asked ??= invoke<string>("app_platform")
    .then((answer) => {
      platform.value = answer === "mobile" ? "mobile" : "desktop"
      return platform.value
    })
    .catch(() => platform.value)
  return asked
}

export function usePlatform() {
  void ask()
  const isMobile = computed(() => platform.value === "mobile")
  return { platform, isMobile, ready: ask }
}
