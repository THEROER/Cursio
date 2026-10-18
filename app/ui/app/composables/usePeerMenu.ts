import type { ContextMenuItemDef } from "@leavepulse/ui"
import { invoke } from "@tauri-apps/api/core"

/** What a peer's menu needs to know about the machine it is for. */
export interface PeerRef {
  id: string
  name: string
  presence?: "online" | "connecting" | "offline"
  /** A phone linked with the window module, whose screen can open here. */
  screen?: boolean
}

// One removal at a time, whichever card, node or list asked for it: the
// confirmation dialog is mounted once, in the shell, and reads this.
const removing = ref<PeerRef | null>(null)
const confirmOpen = ref(false)
const busy = ref(false)

/**
 * The actions on a peer, as the LeavePulse launcher builds its menus: one item
 * list, handed to a right-click `LpContextMenu` and to the ⋯ `LpDropdownMenu`
 * alike, so the two never disagree about what a machine offers.
 */
export function usePeerMenu() {
  const { t } = useI18n()
  const toast = useCursioToast()
  const { showInLogs } = useChrome()
  const { forget } = useNode()
  const { isMobile } = usePlatform()

  function items(peer: PeerRef): ContextMenuItemDef[] {
    const list: ContextMenuItemDef[] = []
    // A phone has no log drawer: the node's log is logcat's there.
    if (!isMobile.value) {
      list.push({
        label: t("peerMenu.showInLogs"),
        icon: "lucide:scroll-text",
        onSelect: () => showInLogs(peer.name),
      })
    }
    if (peer.screen && !isMobile.value) {
      list.push({
        label: t("peerMenu.showScreen"),
        icon: "lucide:cast",
        // The phone asks its user first: the window follows once allowed.
        onSelect: async () => {
          try {
            await invoke("node_phone_screen", { phone: peer.name, show: true })
            toast.success(t("peerMenu.screenAsked", { name: peer.name }))
          } catch (e) {
            toast.failure(e, { title: t("peerMenu.screenFailed", { name: peer.name }) })
          }
        },
      })
    }
    list.push(
      {
        label: t("peerMenu.copyId"),
        icon: "lucide:copy",
        onSelect: async () => {
          await navigator.clipboard.writeText(peer.id)
          toast.success(t("peerMenu.idCopied", { name: peer.name }))
        },
      },
    )
    // A machine that never comes online may be one the other side removed;
    // pairing again is then the way back, not waiting.
    if (peer.presence === "offline") {
      list.push({
        label: t("peerMenu.pairAgain"),
        icon: "lucide:link",
        onSelect: () => void navigateTo("/pair"),
      })
    }
    list.push({
      label: t("peerMenu.remove"),
      icon: "lucide:unlink",
      danger: true,
      separatorBefore: true,
      onSelect: () => {
        removing.value = peer
        confirmOpen.value = true
      },
    })
    return list
  }

  async function confirmRemove() {
    const peer = removing.value
    if (!peer) return
    busy.value = true
    try {
      await forget(peer.id)
      confirmOpen.value = false
      toast.success(t("peerMenu.removedHint"), {
        title: t("peerMenu.removed", { name: peer.name }),
        duration: 8000,
        actions: [
          { label: t("peerMenu.pairAgain"), icon: "lucide:link", onClick: () => void navigateTo("/pair") },
        ],
      })
    } catch (e) {
      toast.failure(e, { title: t("peerMenu.removeFailed", { name: peer.name }), retry: confirmRemove })
    } finally {
      busy.value = false
    }
  }

  return { items, removing, confirmOpen, busy, confirmRemove }
}
