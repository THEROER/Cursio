import { invoke } from "@tauri-apps/api/core"

/** `std::time::SystemTime` as serde writes it. */
export interface SystemTime {
  secs_since_epoch: number
  nanos_since_epoch: number
}

export interface Position {
  x: number
  y: number
}

/** Mirrors `cursio_protocol::NodeStatus`. `null` means "cannot say", never zero. */
export interface NodeStatus {
  battery_percent: number | null
  power: "Mains" | "Battery" | "Charging" | null
  network: "Wired" | "Wifi" | "Cellular" | "Unknown" | null
  signal_dbm: number | null
}

export interface LinkView {
  position: Position
  device_kind: "Desktop" | "Laptop" | "Phone" | "Tablet" | "Headless"
  status: NodeStatus
  modules: string[]
  skipped: [string, string][]
  since: SystemTime
}

export interface PeerView {
  id: string
  name: string
  presence: "online" | "connecting" | "offline"
  link: LinkView | null
}

export interface Modules {
  input: boolean
  clipboard: boolean
  audio: boolean
  files: boolean
  usb: boolean
  remote_screen: boolean
  screen_host: boolean
}

export type ModuleSwitch = keyof Modules

export interface LatencyReading {
  path: string
  p50_us: number
  p99_us: number
  max_us: number
  events: number
  at: SystemTime
}

export interface Problem {
  id: string
  severity: "Info" | "Warning" | "Error"
  summary: string
  hint: string | null
  since: SystemTime
}

export interface PairingState {
  kind: "shown_code" | "entered_code" | "password"
  code: string | null
  expires_at: SystemTime
  failed_attempts: number
}

export type PairingOutcome =
  | { outcome: "paired"; peer: string }
  | { outcome: "expired" }
  | { outcome: "too_many_attempts" }
  | { outcome: "cancelled" }

/** Mirrors `cursio_core::control::Status`. */
export interface NodeStatusReply {
  version: number
  node: {
    id: string
    name: string
    position: Position
    status: NodeStatus
    started_at: SystemTime
  }
  peers: PeerView[]
  modules: Modules
  pinned: string[]
  config_path: string
  latency: LatencyReading[]
  problems: Problem[]
  pairing: PairingState | null
}

export type Reply =
  | { reply: "saved"; pinned: string[] }
  | { reply: "pairing"; state: PairingState | null; last: PairingOutcome | null }
  | { reply: "done" }
  | { reply: string; [key: string]: unknown }

/** What the Rust side reports when a request fails. Mirrors `node::NodeError`. */
export type NodeError = { kind: "notRunning"; message: string } | { kind: "failed"; message: string }

export function toMillis(t: SystemTime): number {
  return t.secs_since_epoch * 1000 + Math.floor(t.nanos_since_epoch / 1e6)
}

function asNodeError(e: unknown): NodeError {
  if (e && typeof e === "object" && "kind" in e) return e as NodeError
  return { kind: "failed", message: String(e) }
}

/** How often the window asks the node. The same pace the tray keeps. */
const POLL_MS = 1500

/**
 * The node as the status window sees it: polled, never pushed.
 *
 * Shared by every page, so switching pages does not start a second poll or
 * flash an empty state while the first answer arrives.
 */
const status = ref<NodeStatusReply | null>(null)
const error = ref<NodeError | null>(null)
let timer: ReturnType<typeof setInterval> | null = null
let users = 0

async function refresh() {
  try {
    status.value = await invoke<NodeStatusReply>("node_status")
    error.value = null
  } catch (e) {
    error.value = asNodeError(e)
    status.value = null
  }
}

export function useNode() {
  onMounted(() => {
    users += 1
    if (!timer) {
      void refresh()
      timer = setInterval(refresh, POLL_MS)
    }
  })
  onBeforeUnmount(() => {
    users -= 1
    if (users === 0 && timer) {
      clearInterval(timer)
      timer = null
    }
  })

  const notRunning = computed(() => error.value?.kind === "notRunning")

  async function call<T = Reply>(cmd: string, args?: Record<string, unknown>): Promise<T> {
    try {
      const reply = await invoke<T>(cmd, args)
      void refresh()
      return reply
    } catch (e) {
      throw asNodeError(e)
    }
  }

  return {
    status,
    error,
    notRunning,
    refresh,
    setModule: (module: ModuleSwitch, enabled: boolean) =>
      call<Reply>("node_set_module", { module, enabled }),
    setPosition: (p: Position) => call<Reply>("node_set_position", { x: p.x, y: p.y }),
    pairShow: () => call<Reply>("node_pair_show"),
    pairEnter: (code: string) => call<Reply>("node_pair_enter", { code }),
    pairCancel: () => call<Reply>("node_pair_cancel"),
    pairing: () => call<Reply>("node_pairing"),
    restart: () => call<Reply>("node_restart"),
    start: () => call<void>("node_start"),
  }
}
