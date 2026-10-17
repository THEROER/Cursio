import { invoke } from "@tauri-apps/api/core"

/**
 * What the Rust side reports about the link. Mirrors `touchpad::Status`.
 */
export type TouchpadStatus = { state: "idle" } | { state: "connected"; peer: string }

/** A machine the touchpad can drive: a peer with a live link. Mirrors `touchpad::Target`. */
export interface TouchpadTarget {
  id: string
  name: string
}

export type MouseButtonName = "left" | "middle" | "right"

/**
 * How far the pointer travels on the peer per pixel of finger movement.
 *
 * A phone screen is small and a desktop is not, so 1:1 makes crossing a screen
 * an unpleasant number of swipes.
 */
const SENSITIVITY = 1.8

/**
 * A tap is a press that neither lasted nor moved. Past either threshold the
 * user was dragging or resting a finger, and turning that into a click leaves a
 * stray click somewhere on their desktop.
 */
const TAP_MS = 220
const TAP_SLOP = 12

/**
 * Pixels of two-finger travel per wheel notch — trackpad feel, not mouse wheel.
 * The wire counts scroll in 120ths of a notch, so it goes out as the fraction
 * travelled rather than waiting for a whole notch.
 */
const SCROLL_STEP = 28
const SCROLL_NOTCH = 120

/**
 * Drives a peer's pointer from a touch surface.
 *
 * Motion is accumulated and flushed once per animation frame: a finger produces
 * events far faster than a link should carry them, and the peer only needs the
 * sum. The sub-pixel remainder is kept, or slow deliberate movement — aiming at
 * a small target — drifts to nothing.
 */
export function useTouchpad() {
  const status = ref<TouchpadStatus>({ state: "idle" })
  /** Set when an event had nowhere to go, so the UI can stop pretending. */
  const linkLost = ref(false)
  const targets = ref<TouchpadTarget[]>([])
  /** The id last pointed at; the status names the peer only by name. */
  const drivenId = ref<string | null>(null)
  const connecting = ref(false)
  /** Why the last pick did not take, in the node's words. */
  const connectError = ref<string | null>(null)

  const isConnected = computed(() => status.value.state === "connected")
  const driven = computed(() => {
    const s = status.value
    if (s.state !== "connected") return null
    return targets.value.find((t) => t.id === drivenId.value) ?? targets.value.find((t) => t.name === s.peer) ?? null
  })

  let pendingX = 0
  let pendingY = 0
  let flushQueued = false
  let scrollX = 0
  let scrollY = 0

  /** Active pointers, so a second finger switches the gesture to scrolling. */
  const touches = new Map<number, { x: number; y: number }>()
  let pressStarted = 0
  let pressTravel = 0
  /** Most fingers down at once in this gesture: one tapping is a left click, two a right. */
  let gestureFingers = 0

  async function call<T>(command: string, args?: Record<string, unknown>): Promise<T | null> {
    try {
      return await invoke<T>(command, args)
    } catch (error) {
      // Running in a plain browser (`nuxt dev` without the shell) or during
      // teardown: there is no backend to talk to, which is not the user's
      // problem to hear about on every frame.
      console.debug("touchpad command failed", command, error)
      return null
    }
  }

  function delivered(result: boolean | null) {
    if (result === false) linkLost.value = true
  }

  function queueFlush() {
    if (flushQueued) return
    flushQueued = true
    requestAnimationFrame(async () => {
      flushQueued = false
      const dx = Math.trunc(pendingX)
      const dy = Math.trunc(pendingY)
      const sx = Math.trunc(scrollX)
      const sy = Math.trunc(scrollY)
      pendingX -= dx
      pendingY -= dy
      scrollX -= sx
      scrollY -= sy
      if (dx !== 0 || dy !== 0) delivered(await call<boolean>("touchpad_motion", { dx, dy }))
      if (sx !== 0 || sy !== 0) delivered(await call<boolean>("touchpad_scroll", { dx: sx, dy: sy }))
    })
  }

  function onPointerDown(event: PointerEvent) {
    ;(event.currentTarget as HTMLElement).setPointerCapture(event.pointerId)
    touches.set(event.pointerId, { x: event.clientX, y: event.clientY })
    if (touches.size === 1) {
      pressStarted = performance.now()
      pressTravel = 0
      gestureFingers = 0
    }
    gestureFingers = Math.max(gestureFingers, touches.size)
  }

  function onPointerMove(event: PointerEvent) {
    const previous = touches.get(event.pointerId)
    if (!previous) return
    const dx = event.clientX - previous.x
    const dy = event.clientY - previous.y
    touches.set(event.pointerId, { x: event.clientX, y: event.clientY })
    pressTravel += Math.abs(dx) + Math.abs(dy)

    if (touches.size >= 2) {
      // Only the first pointer drives the scroll, or two fingers contribute the
      // same movement twice and the page flies past.
      if (event.pointerId !== touches.keys().next().value) return
      // Content follows the fingers, as on a phone and a laptop's touchpad:
      // fingers moving down scroll up (positive on the wire), and fingers
      // moving right scroll left.
      scrollX -= (dx / SCROLL_STEP) * SCROLL_NOTCH
      scrollY += (dy / SCROLL_STEP) * SCROLL_NOTCH
      queueFlush()
      return
    }

    pendingX += dx * SENSITIVITY
    pendingY += dy * SENSITIVITY
    queueFlush()
  }

  async function onPointerUp(event: PointerEvent) {
    if (!touches.delete(event.pointerId) || touches.size > 0) return
    // Judged when the last finger lifts, so a two-finger tap whose fingers
    // leave a few milliseconds apart is still one right click, not a scroll.
    const quick = performance.now() - pressStarted < TAP_MS
    if (!quick || pressTravel >= TAP_SLOP * gestureFingers) return
    if (gestureFingers === 1) await click("left")
    else if (gestureFingers === 2) await click("right")
  }

  async function press(button: MouseButtonName, pressed: boolean) {
    delivered(await call<boolean>("touchpad_button", { button, pressed }))
  }

  async function click(button: MouseButtonName) {
    await press(button, true)
    await press(button, false)
  }

  /**
   * A clickpad's button area: held, not tapped. Pressing there and moving a
   * second finger on the surface is a drag, which a tap-to-click cannot do.
   */
  function zone(button: MouseButtonName) {
    let down: number | null = null
    const release = (event: PointerEvent) => {
      if (event.pointerId !== down) return
      down = null
      void press(button, false)
    }
    return {
      pointerdown: (event: PointerEvent) => {
        if (down !== null) return
        ;(event.currentTarget as HTMLElement).setPointerCapture(event.pointerId)
        down = event.pointerId
        void press(button, true)
      },
      pointerup: release,
      pointercancel: release,
    }
  }

  /** Point the touchpad at `peer` (an id), or at the only linked machine. */
  async function connect(peer?: string) {
    connecting.value = true
    try {
      await invoke("touchpad_connect", peer ? { peer } : {})
      drivenId.value = peer ?? (targets.value.length === 1 ? targets.value[0]!.id : null)
      connectError.value = null
      linkLost.value = false
    } catch (error) {
      connectError.value = String(error)
    } finally {
      connecting.value = false
      await refreshStatus()
    }
  }

  async function refreshTargets() {
    const next = await call<TouchpadTarget[]>("touchpad_targets")
    if (!next) return
    targets.value = next
    // One machine leaves nothing to choose, so asking would only add a tap
    // before every use. With several, guessing would drive whichever the user
    // was not looking at.
    if (status.value.state === "idle" && next.length === 1 && !connecting.value) await connect()
  }

  async function refreshStatus() {
    const next = await call<TouchpadStatus>("touchpad_status")
    if (!next) return
    const was = status.value.state
    status.value = next
    if (next.state === "connected") linkLost.value = false
    else drivenId.value = null
    if (was !== next.state) void refreshTargets()
  }

  // A link coming or going is what changes the targets, and the node pushes
  // its status on exactly that.
  const { status: node } = useNode()
  watch(
    () => node.value?.peers.map((p) => `${p.id}:${p.presence}:${p.name}`).join(),
    () => void refreshTargets(),
  )

  // Polled rather than pushed: the status changes a handful of times per link,
  // and an event channel for that is more machinery than it earns.
  let timer: ReturnType<typeof setInterval> | undefined
  onMounted(() => {
    void refreshStatus().then(refreshTargets)
    timer = setInterval(refreshStatus, 500)
  })
  onBeforeUnmount(() => clearInterval(timer))

  return {
    status,
    linkLost,
    isConnected,
    targets,
    driven,
    connecting,
    connectError,
    connect,
    refreshTargets,
    click,
    zone,
    // Bare event names: `v-on="object"` adds the `on` itself, so `onPointerdown`
    // here would listen for an event called "onPointerdown", which never fires.
    surface: {
      pointerdown: onPointerDown,
      pointermove: onPointerMove,
      pointerup: onPointerUp,
      pointercancel: onPointerUp,
    },
  }
}
