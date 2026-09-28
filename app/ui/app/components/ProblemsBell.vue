<script setup lang="ts">
import type { NotificationItem } from "@leavepulse/ui"
import { toMillis } from "~/composables/useNode"

const { t } = useI18n()
const { status } = useNode()

/**
 * Problems are the node's, and they clear only when the node sees them fixed —
 * so "read" here is the window's own note that they were looked at, kept for
 * this session, never a way to dismiss one.
 */
const seen = ref(new Set<string>())

const tone = { Info: "brand", Warning: "accent", Error: "danger" } as const
const icon = { Info: "lucide:info", Warning: "lucide:triangle-alert", Error: "lucide:octagon-alert" } as const

const items = computed<NotificationItem[]>(() =>
  (status.value?.problems ?? []).map((p) => ({
    id: p.id,
    title: p.summary,
    body: p.hint ?? undefined,
    icon: icon[p.severity],
    tone: tone[p.severity],
    createdAt: new Date(toMillis(p.since)).toISOString(),
    read: seen.value.has(p.id),
  })),
)

function mark(id: string, read: boolean) {
  const next = new Set(seen.value)
  if (read) next.add(id)
  else next.delete(id)
  seen.value = next
}

const labels = computed(() => ({
  title: t("problems.title"),
  markAllRead: t("problems.markAllRead"),
  markAsRead: t("problems.markRead"),
  markAsUnread: t("problems.markUnread"),
  open: t("problems.open"),
  empty: t("problems.none"),
  ariaLabel: t("problems.title"),
}))
</script>

<template>
  <LpNotificationBell
    :items="items"
    :labels="labels"
    @mark-read="(id: string) => mark(id, true)"
    @mark-unread="(id: string) => mark(id, false)"
    @mark-all-read="seen = new Set(items.map((i) => i.id))"
    @select="(item: NotificationItem) => { mark(item.id, true); void navigateTo('/') }"
  />
</template>
