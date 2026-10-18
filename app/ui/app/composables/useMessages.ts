import { invoke } from "@tauri-apps/api/core"
import type { Reply } from "~/composables/useNode"

/** Mirrors `cursio_protocol::messages::Conversation`. */
export interface Conversation {
  thread_id: number
  addresses: string[]
  /** The contact's name per address, or the address where the phone has none. */
  names: string[]
  snippet: string
  date_ms: number
  unread: boolean
}

/** Mirrors `cursio_protocol::messages::TextMessage`. */
export interface TextMessage {
  id: number
  mms: boolean
  thread_id: number
  address: string
  body: string
  date_ms: number
  kind: "Inbox" | "Sent" | "Outbox" | "Failed" | "Other"
  read: boolean
  /** An MMS that carried a picture or the like, which is not shown here. */
  attachment: boolean
}

/** Mirrors `cursio_core::messages_hub::MessagesPeer`. */
export interface MessagesPeer {
  peer: string
  name: string
  /** Moves each time the phone says something changed. */
  revision: number
}

/** How many items one page asks the phone for. */
export const PAGE = 40

/**
 * A phone's text messages, asked of it through the node. Every call waits on
 * the phone, so the node answers it off its own loop and may take seconds.
 */
export function useMessages() {
  async function conversations(peer: string, beforeMs?: number): Promise<Conversation[]> {
    const reply = await invoke<Reply>("messages_conversations", { peer, limit: PAGE, beforeMs: beforeMs ?? null })
    return reply.reply === "conversations" ? (reply.conversations as Conversation[]) : []
  }

  async function thread(peer: string, threadId: number, beforeMs?: number): Promise<TextMessage[]> {
    const reply = await invoke<Reply>("messages_thread", { peer, threadId, limit: PAGE, beforeMs: beforeMs ?? null })
    return reply.reply === "messages" ? (reply.messages as TextMessage[]) : []
  }

  const send = (peer: string, addresses: string[], body: string) =>
    invoke<Reply>("messages_send", { peer, addresses, body })

  const markRead = (peer: string, threadId: number) => invoke<Reply>("messages_mark_read", { peer, threadId })

  return { conversations, thread, send, markRead }
}
