import { invoke } from "@tauri-apps/api/core"
import type { Reply } from "~/composables/useNode"

/** Mirrors `cursio_protocol::gallery::MediaItem`. */
export interface MediaItem {
  id: string
  name: string
  kind: "Image" | "Video"
  taken_ms: number
  size: number
  width: number | null
  height: number | null
  duration_ms: number | null
  mime: string
}

/** Mirrors `cursio_core::gallery_hub::GalleryPeer`. */
export interface GalleryPeer {
  peer: string
  name: string
  /** Moves each time the machine says its gallery changed. */
  revision: number
}

export type MediaFilter = "All" | "Images" | "Videos"

/** Items asked for per page; a grid row is four to eight of them. */
export const GALLERY_PAGE = 60

/** Longer edge of a grid thumbnail, in pixels: a cell at 2x scale. */
export const THUMB_PX = 320

/** Bytes asked for per read; `cursio_protocol::gallery::MAX_SLICE`. */
const SLICE = 512 * 1024

function bytesOf(base64: string): Uint8Array {
  const raw = atob(base64)
  const out = new Uint8Array(raw.length)
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i)
  return out
}

/**
 * A machine's photos and videos, asked of it through the node. Thumbnails are
 * kept as object URLs for as long as the page lives, since a grid scrolled
 * back up asks for the same ones again; `forget` lets them go.
 */
export function useGallery() {
  const thumbs = new Map<string, Promise<string | null>>()

  async function list(peer: string, after: MediaItem | undefined, filter: MediaFilter): Promise<MediaItem[]> {
    const reply = await invoke<Reply>("gallery_list", {
      peer,
      limit: GALLERY_PAGE,
      after: after ? { taken_ms: after.taken_ms, id: after.id } : null,
      filter,
    })
    return reply.reply === "media" ? (reply.items as MediaItem[]) : []
  }

  /** An object URL for the item's thumbnail, or null where the machine has none. */
  function thumbnail(peer: string, item: MediaItem): Promise<string | null> {
    const key = `${peer}\u0000${item.id}`
    let pending = thumbs.get(key)
    if (!pending) {
      pending = invoke<Reply>("gallery_thumbnail", { peer, item: item.id, maxPx: THUMB_PX }).then((reply) => {
        const jpeg = reply.reply === "thumbnail" ? (reply.jpeg as string | null) : null
        return jpeg ? URL.createObjectURL(new Blob([bytesOf(jpeg)], { type: "image/jpeg" })) : null
      })
      // A failure is not kept: scrolling back asks again.
      pending.catch(() => thumbs.delete(key))
      thumbs.set(key, pending)
    }
    return pending
  }

  /** The whole item, read slice by slice, as a Blob of its own type. */
  async function whole(peer: string, item: MediaItem, progress?: (done: number) => void): Promise<Blob> {
    const parts: Uint8Array[] = []
    let offset = 0
    for (;;) {
      const reply = await invoke<Reply>("gallery_read", { peer, item: item.id, offset, len: SLICE })
      const bytes = reply.reply === "slice" ? bytesOf(reply.data as string) : new Uint8Array()
      if (!bytes.length) break
      parts.push(bytes)
      offset += bytes.length
      progress?.(offset)
      if (bytes.length < SLICE) break
    }
    return new Blob(parts, { type: item.mime })
  }

  /** Copy into this computer's Pictures/Cursio/<machine>; answers the path. */
  async function save(peer: string, item: MediaItem): Promise<string> {
    const reply = await invoke<Reply>("gallery_save", { peer, item: item.id, name: item.name })
    return reply.reply === "saved_file" ? (reply.path as string) : ""
  }

  function forget() {
    for (const pending of thumbs.values()) {
      void pending.then((url) => url && URL.revokeObjectURL(url)).catch(() => undefined)
    }
    thumbs.clear()
  }

  return { list, thumbnail, whole, save, forget }
}
