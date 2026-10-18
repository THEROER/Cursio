import { invoke } from "@tauri-apps/api/core"
import { listen, type UnlistenFn } from "@tauri-apps/api/event"

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
  /** Modules this node offers that could run on this link but do not, and why. */
  skipped: [string, string][]
  /** Modules with no meaning between these two machines; never shown as off. */
  inapplicable?: string[]
  since: SystemTime
  /** What carries the link; absent from a node older than control v12. */
  path?: "network" | "bluetooth"
}

/**
 * Whether a machine of this kind has a screen the shared cursor can enter.
 * Mirrors `DeviceKind::takes_cursor`: a phone only ever drives a cursor, so
 * its grid cell borders nothing and showing one invites placing it.
 */
export function takesCursor(kind: LinkView["device_kind"]): boolean {
  return kind === "Desktop" || kind === "Laptop"
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

/** A machine the node heard announce itself on the LAN. Mirrors `control::NearbyMachine`. */
export interface NearbyMachine {
  id: string
  name: string
  addr: string
  /** Already in this node's mesh. */
  paired: boolean
  last_seen: SystemTime
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
    /** 0 from a node older than control v4. */
    pid: number
    build: string
    installed_version: string | null
  }
  peers: PeerView[]
  modules: Modules
  /**
   * The switches this build can act on. Empty from a node older than control
   * v8, which said nothing about it.
   */
  available: ModuleSwitch[]
  pinned: string[]
  /** Settings changed since the node started that only a restart applies. */
  restart_needed: string[]
  config_path: string
  latency: LatencyReading[]
  problems: Problem[]
  pairing: PairingState | null
  /** Empty (all defaults) from a node older than control v5. */
  update: UpdateStatus
  /** Peer streams playing on this machine. The app's own core defaults it from an older node. */
  streams: PlayingStream[]
  /** Phones whose messages can be read here; absent from a node older than control v10. */
  messages?: import("~/composables/useMessages").MessagesPeer[]
  /** Phones whose calls this machine sees; absent from a node without the calls module. */
  calls?: PhoneCalls[]
  /** Machines whose galleries can be browsed; absent from a node older than control v15. */
  gallery?: import("~/composables/useGallery").GalleryPeer[]
  /** This computer as a phone's Bluetooth headset; absent where it cannot be one, and from a node older than control v16. */
  call_setup?: CallSetup | null
}

/** Mirrors `cursio_core::calls_hub::ServiceState`. */
export type ServiceState = { state: "starting" } | { state: "running" } | { state: "failed"; reason: string }

/** Mirrors `cursio_core::calls_hub::CallSetup`. */
export interface CallSetup {
  /** `cursio bluetooth install` handed the phone roles from PipeWire to Cursio. */
  roles_taken: boolean
  hands_free: ServiceState
  speaker: ServiceState
  phones: { name: string; address: string; connected: boolean }[]
  outputs: AudioDevice[]
  inputs: AudioDevice[]
}

export interface AudioDevice {
  id: string
  name: string
}

/** Mirrors `cursio_core::config::CallsConfig`. */
export interface CallsConfig {
  answer_moves_voice: boolean
  voice: "here" | "follow"
  output: string | null
  input: string | null
}

/** Mirrors `cursio_protocol::calls::Call`. */
export interface PhoneCall {
  id: number
  direction: "Incoming" | "Outgoing"
  state: "Ringing" | "Waiting" | "Dialing" | "Alerting" | "Active" | "Held"
  /** `null` when the caller withheld it. */
  number: string | null
  /** The phone's contacts' name for the number, when they have one. */
  name: string | null
}

/** Mirrors `cursio_core::calls_hub::PhoneCalls`. */
export interface PhoneCalls {
  name: string
  calls: PhoneCall[]
  /** The voice of the call that is up plays on this computer. */
  audio_here: boolean
  /** The machine this computer sends the voice on to, when it plays elsewhere. */
  voice_on?: string | null
  /** Reached over Bluetooth hands-free, so the voice can come here. */
  hands_free: boolean
  unavailable: string | null
}

/** Mirrors `cursio_core::calls_hub::CallAction`. */
export type CallAction =
  | { action: "answer" }
  | { action: "decline" }
  | { action: "hang_up" }
  | { action: "dial"; number: string }
  | { action: "mute"; muted: boolean }
  | { action: "audio_here"; here: boolean }

/** `cursio_core::mixer::StreamKind`; absent from a node before control v18. */
export type StreamKind = "app" | "microphone" | "device" | "call" | "phone_sound"

/** Mirrors `cursio_core::mixer::MixerStream`: the listener's own say on one stream. */
export interface PlayingStream {
  id: number
  /** The machine it comes from, by name. */
  peer: string
  name: string
  kind?: StreamKind
  /** Linear, 1 is as sent, up to `MAX_STREAM_VOLUME`. */
  volume: number
  muted: boolean
}

/** `cursio_core::mixer::MAX_LOCAL_VOLUME`: the node refuses anything louder. */
export const MAX_STREAM_VOLUME = 1.5

/** Mirrors `cursio_core::update::agent::UpdateStatus`. */
export interface UpdateStatus {
  policy: string
  channel: string
  running: string
  progress: { version: string; from: string; received: number; total: number } | null
  ready: { version: string; from: string; published_at: number } | null
  waiting_for: string | null
  last: string | null
}

/** The part of `cursio_core::config::Config` the app reads. */
export interface NodeConfig {
  audio: { render_device: string | null; mic_gate: string | null }
  /** Absent from a node older than control v16. */
  calls?: CallsConfig
  [key: string]: unknown
}

export type Reply =
  | { reply: "saved"; pinned: string[]; restart_needed: string[] }
  | { reply: "config"; path: string; file: NodeConfig; effective: NodeConfig; pinned: string[] }
  | { reply: "pairing"; state: PairingState | null; last: PairingOutcome | null; nearby?: NearbyMachine[] }
  | { reply: "done" }
  | { reply: "forgotten"; name: string; was_builder: boolean }
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

/**
 * The node as the window sees it: pushed, never polled.
 *
 * The Rust side keeps one subscribed connection and re-emits every change as
 * an event, so a toggle's result shows the moment the node applies it and no
 * request of the window's ever waits behind a poll. Shared by every page, so
 * switching pages neither subscribes twice nor flashes an empty state.
 */
const status = ref<NodeStatusReply | null>(null)
const error = ref<NodeError | null>(null)
let unlisten: UnlistenFn[] = []
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

async function follow() {
  unlisten = await Promise.all([
    listen<NodeStatusReply>("node://status", (event) => {
      status.value = event.payload
      error.value = null
    }),
    listen<NodeError>("node://down", (event) => {
      error.value = event.payload
      status.value = null
    }),
  ])
  // Whatever was pushed before this page was listening.
  await refresh()
}

/** Switches whose change the node has not answered yet. */
const pendingSwitches = ref(new Set<ModuleSwitch>())

export function useNode() {
  onMounted(() => {
    users += 1
    if (users === 1) void follow()
  })
  onBeforeUnmount(() => {
    users -= 1
    if (users === 0) {
      for (const stop of unlisten) stop()
      unlisten = []
    }
  })

  const notRunning = computed(() => error.value?.kind === "notRunning")

  async function call<T = Reply>(cmd: string, args?: Record<string, unknown>): Promise<T> {
    try {
      return await invoke<T>(cmd, args)
    } catch (e) {
      throw asNodeError(e)
    }
  }

  /** Change one module; only that switch waits for the answer. */
  async function setModule(module: ModuleSwitch, enabled: boolean): Promise<Reply> {
    pendingSwitches.value = new Set(pendingSwitches.value).add(module)
    try {
      return await call<Reply>("node_set_module", { module, enabled })
    } finally {
      const next = new Set(pendingSwitches.value)
      next.delete(module)
      pendingSwitches.value = next
    }
  }

  return {
    status,
    error,
    notRunning,
    refresh,
    pendingSwitches,
    setModule,
    setPosition: (p: Position) => call<Reply>("node_set_position", { x: p.x, y: p.y }),
    setName: (name: string) => call<Reply>("node_set_name", { name }),
    config: () => call<Reply>("node_config"),
    setMicGate: (value: string) => call<Reply>("node_set_mic_gate", { value }),
    setCalls: (calls: CallsConfig) => call<Reply>("node_set_calls", { calls }),
    setStreamVolume: (stream: number, volume: number, muted: boolean) =>
      call<Reply>("node_set_stream_volume", { stream, volume, muted }),
    pairShow: () => call<Reply>("node_pair_show"),
    pairEnter: (code: string) => call<Reply>("node_pair_enter", { code }),
    pairCancel: () => call<Reply>("node_pair_cancel"),
    pairing: () => call<Reply>("node_pairing"),
    restart: () => call<Reply>("node_restart"),
    applyUpdate: () => call<Reply>("node_update_apply"),
    forget: (peer: string) => call<Reply>("node_forget", { peer }),
    /** Act on a phone's call; `phone` is its name in `status.calls`. */
    callAction: (phone: string, action: CallAction) => call<Reply>("node_call", { phone, action }),
    start: () => call<void>("node_start"),
  }
}
