import { invoke } from "@tauri-apps/api/core"

/** Mirrors `mobile::Setup`. Each grant is `true` once given. */
export interface PhoneSetup {
  notifications: boolean
  microphone: boolean
  battery: boolean
  /**
   * The maker kills background apps unless a setting of its own allows this
   * one (HyperOS "Autostart"). No API reads that setting, so this is never
   * "granted": it only says the phone has one worth visiting.
   */
  autostartSetting: boolean
  /** The phone's notifications may be shown on the machine the user is at. */
  notificationAccess: boolean
  /** The phone's calls may be shown, answered and placed from a computer. */
  calls: boolean
  /** The phone's SMS may be read and sent from a desktop. */
  messages: boolean
}
export type PhoneGrant =
  | "notifications"
  | "microphone"
  | "battery"
  | "autostart"
  | "notificationAccess"
  | "calls"
  | "bluetooth"
  | "messages"

export const mobileSetup = () => invoke<PhoneSetup>("mobile_setup")

/** Resolves once the system dialog or settings screen has been shown. */
export const mobileRequest = (what: PhoneGrant) => invoke<PhoneSetup>("mobile_request", { what })

/**
 * Bring the phone's node down and up again. Links greet peers only when they
 * come up, so a change such as a new name reaches them through this.
 */
export const mobileRestart = () => invoke<void>("mobile_restart")

/** A peer's output device the phone's sound can play on. Mirrors the app's audio target. */
export interface PhoneAudioTarget {
  key: number
  name: string
  /** The phone's sound goes here now. */
  routed: boolean
}

export const phoneAudioTargets = () => invoke<PhoneAudioTarget[]>("phone_audio_targets")

/**
 * Send everything the phone plays to `target`, or stop with `null`. Resolves
 * only after Android's capture consent; rejects with its reason when declined.
 */
export const phoneAudioRoute = (target: number | null) => invoke<void>("phone_audio_route", { target })

/** Mirrors the app's `PhoneAudioState`. */
export interface PhoneAudioState {
  /** How loud the phone's sound reaches the computer, 0 to 1. */
  volume: number
  /** The chosen device, while its machine is away. */
  awaited: string | null
  /** The device chosen before Cursio restarted, while nothing is routed. */
  remembered: string | null
  /** That device's key, when it is connected and can be resumed now. */
  resume: number | null
}

export const phoneAudioState = () => invoke<PhoneAudioState>("phone_audio_state")
export const phoneAudioVolume = (volume: number) => invoke<void>("phone_audio_volume", { volume })

/**
 * The user's own word that autostart is allowed, kept because the phone
 * cannot report it: without this the hint would stay on the home page forever.
 */
const AUTOSTART_DONE_KEY = "cursio.phone.autostartDone"

function readAutostartDone(): boolean {
  try {
    return localStorage.getItem(AUTOSTART_DONE_KEY) === "1"
  } catch {
    return false
  }
}

const setup = ref<PhoneSetup | null>(null)
const autostartDone = ref(readAutostartDone())
const failure = ref<unknown>(null)
let users = 0

async function refresh() {
  try {
    setup.value = await mobileSetup()
    failure.value = null
  } catch (e) {
    failure.value = e
  }
}

// A grant is given in a system screen outside this window, and the app learns
// nothing when the user comes back from it; returning is the moment to ask.
function onReturn() {
  if (document.visibilityState === "visible") void refresh()
}

/** The phone's own permissions for keeping the node alive and carrying sound. */
export function usePhone() {
  onMounted(() => {
    users += 1
    if (users !== 1) return
    void refresh()
    window.addEventListener("focus", onReturn)
    document.addEventListener("visibilitychange", onReturn)
  })
  onBeforeUnmount(() => {
    users -= 1
    if (users !== 0) return
    window.removeEventListener("focus", onReturn)
    document.removeEventListener("visibilitychange", onReturn)
  })

  async function request(what: PhoneGrant): Promise<void> {
    setup.value = await mobileRequest(what)
  }

  function markAutostartDone(done: boolean) {
    autostartDone.value = done
    try {
      if (done) localStorage.setItem(AUTOSTART_DONE_KEY, "1")
      else localStorage.removeItem(AUTOSTART_DONE_KEY)
    } catch {
      // Storage refused: the hint shows again at the next launch, nothing worse.
    }
  }

  /** Something the node needs that the user has not given yet. */
  const missing = computed(() => {
    const s = setup.value
    if (!s) return false
    return (
      !s.notifications ||
      !s.microphone ||
      !s.battery ||
      !s.notificationAccess ||
      !s.calls ||
      !s.messages ||
      (s.autostartSetting && !autostartDone.value)
    )
  })

  return { setup, failure, missing, autostartDone, refresh, request, markAutostartDone }
}
