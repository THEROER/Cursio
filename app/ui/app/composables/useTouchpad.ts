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
 * Pointer acceleration: how far the pointer travels per pixel of finger
 * movement, by how fast the finger moves — the shape libinput gives a laptop's
 * touchpad.
 *
 * A fixed ratio had to choose between aiming and travelling: high enough to
 * cross a desktop from a phone screen in one stroke, it overshot every small
 * target; low enough to aim, crossing took a handful of strokes. Slow movement
 * gets the low ratio and fast movement the high one, so both work. Speeds are
 * CSS pixels per millisecond.
 */
const GAIN_SLOW = 1.4
const GAIN_PER_SPEED = 2.2
const SPEED_FLOOR = 0.15
const SPEED_CEILING = 2.0

/** The user's own multiplier on top, kept on this phone. */
const SPEED_KEY = "cursio.touchpad.speed"
export const POINTER_SPEED_MIN = 0.4
export const POINTER_SPEED_MAX = 2.5

function loadSpeed(): number {
  try {
    const saved = Number(localStorage.getItem(SPEED_KEY))
    if (saved >= POINTER_SPEED_MIN && saved <= POINTER_SPEED_MAX) return saved
  } catch {
    // Storage refused (private mode, cleared data): the default is right.
  }
  return 1
}

/**
 * Two fingers are a scroll or a pinch, which only their first few pixels of
 * movement tell apart: a pinch changes the distance between them, a scroll
 * moves them together. Deciding on the first event read jitter as intent.
 */
const TWO_FINGER_DECIDE = 12
/** Share of the travel that must be spreading or closing to call it a pinch. */
const PINCH_SHARE = 0.6

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

  const speed = ref(loadSpeed())
  watch(speed, (value) => {
    try {
      localStorage.setItem(SPEED_KEY, String(value))
    } catch {
      // Not kept; it still applies until the app closes.
    }
  })

  let pendingX = 0
  let pendingY = 0
  let flushQueued = false
  let scrollX = 0
  let scrollY = 0

  /**
   * What the fingers on the surface are doing. Decided as fingers land and
   * move, and held until they all lift — a swipe whose fingers leave one by one
   * must not turn into pointer motion on the way out.
   */
  type Mode = "idle" | "point" | "two" | "scroll" | "pinch" | "swipe" | "done"
  let mode: Mode = "idle"

  /** Active pointers, with when each last moved, for its speed. */
  const touches = new Map<number, { x: number; y: number; t: number }>()
  let pressStarted = 0
  let pressTravel = 0
  /** Most fingers down at once in this gesture: one tapping is a left click, two a right. */
  let gestureFingers = 0

  /** Two fingers: where they started, and the travel so far, to tell scroll from pinch. */
  let twoStart = { distance: 0, angle: 0, travel: 0 }
  let lastAngle = 0
  /** A gesture's update not yet sent, flushed once per frame like motion. */
  let gesture: { dx: number; dy: number; scale: number; rotation: number } | null = null
  let swipeFingers = 0
  let swipeBegun = false

  /**
   * Gesture commands in the order they were made. Each is its own IPC call,
   * and an End overtaking the last Update would leave the peer's virtual
   * fingers a step behind where they lifted.
   */
  let chain: Promise<unknown> = Promise.resolve()
  function inOrder(command: string, args: Record<string, unknown>) {
    chain = chain.then(() => call<boolean>(command, args).then(delivered))
  }

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
      flushGesture()
    })
  }

  function flushGesture() {
    if (!gesture || (mode !== "pinch" && mode !== "swipe")) return
    const { dx, dy, scale, rotation } = gesture
    const x = Math.trunc(dx)
    const y = Math.trunc(dy)
    const turn = Math.trunc(rotation)
    gesture.dx -= x
    gesture.dy -= y
    gesture.rotation -= turn
    if (mode === "swipe") {
      if (x === 0 && y === 0) return
      inOrder("touchpad_gesture", { kind: "swipe", phase: "update", fingers: swipeFingers, dx: x, dy: y, scale: 1, rotation: 0 })
    } else {
      inOrder("touchpad_gesture", { kind: "pinch", phase: "update", fingers: 2, dx: x, dy: y, scale, rotation: turn })
    }
  }

  /** End the gesture in progress — `cancel` when it was abandoned rather than finished. */
  function endGesture(cancel: boolean) {
    const phase = cancel ? "cancel" : "end"
    if (mode === "swipe" && swipeBegun) {
      flushGesture()
      inOrder("touchpad_gesture", { kind: "swipe", phase, fingers: swipeFingers, dx: 0, dy: 0, scale: 1, rotation: 0 })
    } else if (mode === "pinch") {
      flushGesture()
      inOrder("touchpad_gesture", { kind: "pinch", phase, fingers: 2, dx: 0, dy: 0, scale: gesture?.scale ?? 1, rotation: 0 })
    }
    gesture = null
    swipeBegun = false
  }

  /** The first two fingers, which a two-finger gesture is measured between. */
  function pair() {
    const [a, b] = [...touches.values()]
    if (!a || !b) return null
    return { distance: Math.hypot(b.x - a.x, b.y - a.y), angle: Math.atan2(b.y - a.y, b.x - a.x) }
  }

  function onPointerDown(event: PointerEvent) {
    ;(event.currentTarget as HTMLElement).setPointerCapture(event.pointerId)
    touches.set(event.pointerId, { x: event.clientX, y: event.clientY, t: event.timeStamp })
    if (touches.size === 1) {
      pressStarted = performance.now()
      pressTravel = 0
      gestureFingers = 0
      mode = "point"
    }
    gestureFingers = Math.max(gestureFingers, touches.size)
    if (touches.size === 2 && (mode === "point" || mode === "idle")) {
      const p = pair()!
      twoStart = { distance: p.distance, angle: p.angle, travel: 0 }
      lastAngle = p.angle
      mode = "two"
    } else if (touches.size >= 3 && mode !== "swipe" && mode !== "done") {
      // A third finger turns whatever two were doing into a swipe.
      endGesture(true)
      mode = "swipe"
      swipeFingers = touches.size
      swipeBegun = false
      gesture = { dx: 0, dy: 0, scale: 1, rotation: 0 }
    }
  }

  function onPointerMove(event: PointerEvent) {
    const previous = touches.get(event.pointerId)
    if (!previous) return
    const dx = event.clientX - previous.x
    const dy = event.clientY - previous.y
    const dt = Math.max(event.timeStamp - previous.t, 1)
    touches.set(event.pointerId, { x: event.clientX, y: event.clientY, t: event.timeStamp })
    pressTravel += Math.abs(dx) + Math.abs(dy)
    // One finger of n moving shifts their centroid by a nth of it.
    const share = 1 / Math.max(touches.size, 1)

    switch (mode) {
      case "point": {
        const velocity = Math.hypot(dx, dy) / dt
        const accel = Math.min(Math.max(velocity - SPEED_FLOOR, 0), SPEED_CEILING)
        const gain = speed.value * (GAIN_SLOW + GAIN_PER_SPEED * accel)
        pendingX += dx * gain
        pendingY += dy * gain
        queueFlush()
        return
      }
      case "two": {
        twoStart.travel += Math.hypot(dx, dy)
        if (twoStart.travel < TWO_FINGER_DECIDE) return
        const p = pair()
        if (!p) return
        if (Math.abs(p.distance - twoStart.distance) > twoStart.travel * PINCH_SHARE) {
          mode = "pinch"
          gesture = { dx: 0, dy: 0, scale: 1, rotation: 0 }
          inOrder("touchpad_gesture", { kind: "pinch", phase: "begin", fingers: 2, dx: 0, dy: 0, scale: 1, rotation: 0 })
        } else {
          mode = "scroll"
        }
        return
      }
      case "scroll":
        // Only the first pointer drives the scroll, or two fingers contribute
        // the same movement twice and the page flies past. Content follows the
        // fingers, as on a phone and a laptop's touchpad: fingers moving down
        // scroll up (positive on the wire), and fingers moving right scroll left.
        if (event.pointerId !== touches.keys().next().value) return
        scrollX -= (dx / SCROLL_STEP) * SCROLL_NOTCH
        scrollY += (dy / SCROLL_STEP) * SCROLL_NOTCH
        queueFlush()
        return
      case "pinch": {
        const p = pair()
        if (!p || !gesture || twoStart.distance === 0) return
        gesture.dx += dx * share
        gesture.dy += dy * share
        gesture.scale = p.distance / twoStart.distance
        // Hundredths of a degree, unwrapped across ±180°.
        let turn = p.angle - lastAngle
        if (turn > Math.PI) turn -= 2 * Math.PI
        if (turn < -Math.PI) turn += 2 * Math.PI
        gesture.rotation += (turn * 180 * 100) / Math.PI
        lastAngle = p.angle
        queueFlush()
        return
      }
      case "swipe":
        if (!gesture) return
        if (!swipeBegun) {
          swipeBegun = true
          inOrder("touchpad_gesture", { kind: "swipe", phase: "begin", fingers: swipeFingers, dx: 0, dy: 0, scale: 1, rotation: 0 })
        }
        gesture.dx += dx * share
        gesture.dy += dy * share
        queueFlush()
        return
      default:
        return
    }
  }

  async function lift(event: PointerEvent, cancelled: boolean) {
    if (!touches.delete(event.pointerId)) return
    const below = (mode === "swipe" && touches.size < 3) || (mode === "pinch" && touches.size < 2)
    if (below || cancelled) {
      endGesture(cancelled)
      if (mode === "swipe" || mode === "pinch") mode = "done"
    }
    // One finger left of a scroll is not a pointer: it would jump the cursor as
    // the hand comes off the glass.
    if (mode === "scroll" && touches.size < 2) mode = "done"
    if (touches.size > 0) return
    mode = "idle"
    if (cancelled) return
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
    speed,
    // Bare event names: `v-on="object"` adds the `on` itself, so `onPointerdown`
    // here would listen for an event called "onPointerdown", which never fires.
    surface: {
      pointerdown: onPointerDown,
      pointermove: onPointerMove,
      pointerup: (event: PointerEvent) => void lift(event, false),
      pointercancel: (event: PointerEvent) => void lift(event, true),
    },
  }
}
