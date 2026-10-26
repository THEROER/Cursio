/**
 * Where a machine being dragged on the desk settles: flush against another
 * machine's screen when one of its edges comes within reach of one, the way
 * the system's own display settings pull a monitor into place.
 *
 * Flush matters beyond looks. The node counts two screens as meeting only
 * when their edges lie within a few millimetres (`desk::TOUCH_MM`), so a
 * machine dropped by eye a centimetre away would border nothing and the
 * cursor would never cross. Each axis snaps on its own: edge to edge (side
 * by side, stacked), and edge to the same edge (tops lined up).
 */

export interface MmRect {
  x: number
  y: number
  w: number
  h: number
}

/** The best shift along one axis that lines `moving` up with `fixed`. */
function nearest(moving: [number, number][], fixed: [number, number][], reach: number): number | null {
  let best: number | null = null
  for (const [a0, a1] of moving) {
    for (const [b0, b1] of fixed) {
      for (const delta of [b1 - a0, b0 - a1, b0 - a0, b1 - a1]) {
        if (Math.abs(delta) <= reach && (best === null || Math.abs(delta) < Math.abs(best))) best = delta
      }
    }
  }
  return best
}

/**
 * Shift `moving` (rectangles already at the candidate spot) so its edges meet
 * `fixed`'s where they come within `reach` millimetres; answers the shift.
 */
export function snap(moving: MmRect[], fixed: MmRect[], reach: number): { dx: number; dy: number } {
  const span = (r: MmRect, horizontal: boolean): [number, number] => (horizontal ? [r.x, r.x + r.w] : [r.y, r.y + r.h])
  const dx = nearest(
    moving.map((r) => span(r, true)),
    fixed.map((r) => span(r, true)),
    reach,
  )
  const dy = nearest(
    moving.map((r) => span(r, false)),
    fixed.map((r) => span(r, false)),
    reach,
  )
  return { dx: dx ?? 0, dy: dy ?? 0 }
}

/** The box around `rects`, or null for none. */
export function bounds(rects: MmRect[]): MmRect | null {
  if (!rects.length) return null
  const x0 = Math.min(...rects.map((r) => r.x))
  const y0 = Math.min(...rects.map((r) => r.y))
  const x1 = Math.max(...rects.map((r) => r.x + r.w))
  const y1 = Math.max(...rects.map((r) => r.y + r.h))
  return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}
