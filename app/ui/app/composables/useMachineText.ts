import type { LatencyReading, LinkView, ModuleSwitch, NodeStatus } from "~/composables/useNode"

export interface ModuleMeta {
  key: ModuleSwitch
  label: string
  hint: string
  icon: string
}

/** Every switchable module, in the order the status page lists them. */
export const MODULES: ModuleMeta[] = [
  { key: "input", label: "modules.input", hint: "modules.inputHint", icon: "lucide:mouse-pointer-2" },
  { key: "clipboard", label: "modules.clipboard", hint: "modules.clipboardHint", icon: "lucide:clipboard" },
  { key: "audio", label: "modules.audio", hint: "modules.audioHint", icon: "lucide:volume-2" },
  { key: "files", label: "modules.files", hint: "modules.filesHint", icon: "lucide:hard-drive" },
  { key: "usb", label: "modules.usb", hint: "modules.usbHint", icon: "lucide:usb" },
  { key: "remote_screen", label: "modules.remoteScreen", hint: "modules.remoteScreenHint", icon: "lucide:cast" },
  { key: "screen_host", label: "modules.screenHost", hint: "modules.screenHostHint", icon: "lucide:monitor-up" },
]

export const KIND_ICON: Record<LinkView["device_kind"], string> = {
  Desktop: "lucide:monitor",
  Laptop: "lucide:laptop",
  Phone: "lucide:smartphone",
  Tablet: "lucide:tablet",
  Headless: "lucide:server",
}

/** A peer seen in no link this session has no known kind. */
export const UNKNOWN_KIND_ICON = "lucide:monitor-smartphone"

/** The slower of the measured paths, in ms: that is the one the hand feels. */
export function worstLatency(readings: LatencyReading[]): { p50: number; p99: number } | null {
  if (readings.length === 0) return null
  const worst = readings.reduce((a, b) => (b.p99_us > a.p99_us ? b : a))
  return { p50: worst.p50_us / 1000, p99: worst.p99_us / 1000 }
}

/**
 * A machine's battery and network as words, shared by the status page and the
 * command palette so the two never describe one machine differently. `null`
 * is "cannot say", which each caller words for its own place.
 */
export function useMachineText() {
  const { t } = useI18n()

  function battery(s: NodeStatus): string | null {
    if (s.battery_percent === null) return null
    const power = s.power === "Charging" ? t("peer.charging") : s.power === "Mains" ? t("peer.plugged") : ""
    return `${s.battery_percent}%${power ? ` · ${power}` : ""}`
  }

  function network(s: NodeStatus): string | null {
    switch (s.network) {
      case "Wired":
        return t("peer.wired")
      case "Wifi":
        return s.signal_dbm !== null ? `${t("peer.wifi")} ${s.signal_dbm} dBm` : t("peer.wifi")
      case "Cellular":
        return t("peer.cellular")
      case "Unknown":
        return t("peer.unknownNetwork")
      default:
        return null
    }
  }

  return { battery, network }
}
