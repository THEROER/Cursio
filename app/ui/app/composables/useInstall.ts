import { invoke } from "@tauri-apps/api/core"

/** Mirrors `install::InstallView` on the Rust side. */
export interface InstallView {
  root: string | null
  current: string | null
  previous: string | null
  installed: string[]
}

const install = ref<InstallView | null>(null)

async function refreshInstall() {
  try {
    install.value = await invoke<InstallView>("install_status")
  } catch {
    install.value = null
  }
}

/**
 * The versions on disk: read when first asked and again after a rollback, not
 * followed — they change only when an update or a rollback runs.
 */
export function useInstall() {
  if (!install.value) void refreshInstall()
  return {
    install,
    refreshInstall,
    rollBack: (nodePid: number) => invoke<void>("install_rollback", { nodePid }),
  }
}
