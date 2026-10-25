/**
 * Where a volume slider's thumb sits for a gain, and back.
 *
 * The node takes a linear gain, and a slider handing it the thumb's position
 * as that gain is linear in *amplitude*, which the ear is not: loudness goes
 * roughly with the logarithm of it. On such a slider everything from silent to
 * comfortable sat in its first few percent — a phone app held at 2% moved
 * audibly with each step between 0 and 6, and the upper two thirds of the
 * travel all sounded alike.
 *
 * The thumb is the cube root of the gain, the mapping PulseAudio, PipeWire and
 * GNOME's own volume sliders use. A cube spans about 60 dB over the travel,
 * close to evenly in loudness, and unlike a dB scale it still reaches true
 * silence at zero. The percent shown is the thumb's, as those sliders show it:
 * 100% is the gain the sound was sent at, and the curve continues past it for
 * a node that allows amplifying.
 */

/** The thumb, as a percent of the travel, for a linear gain. */
export function sliderFromGain(gain: number): number {
  return Math.cbrt(Math.max(0, gain)) * 100
}

/** The linear gain for a thumb at `percent` of the travel. */
export function gainFromSlider(percent: number): number {
  const position = Math.max(0, percent) / 100
  return position * position * position
}
