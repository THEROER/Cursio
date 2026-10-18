<script setup lang="ts">
import type { LightboxItem, SegmentedOption } from "@leavepulse/ui"
import type { ComponentPublicInstance } from "vue"
import type { MediaFilter, MediaItem } from "~/composables/useGallery"
import { GALLERY_PAGE } from "~/composables/useGallery"

/**
 * Photos and videos of another machine — a phone's camera roll, a computer's
 * Pictures — browsed from this one.
 *
 * Nothing is copied until asked: the grid holds thumbnails the other machine
 * made, the viewer reads a photo whole only when it is opened, and "save"
 * copies it into Pictures/Cursio here. When the other machine says its
 * gallery changed (its `revision` moves) the first page is asked for again,
 * which is how a photo just taken appears.
 */
const { t } = useI18n()
const route = useRoute()
const { status } = useNode()
const { isMobile } = usePlatform()
const gallery = useGallery()
const toast = useCursioToast()

const peers = computed(() => status.value?.gallery ?? [])
const peerId = ref("")
watchEffect(() => {
  if (peers.value.some((p) => p.peer === peerId.value)) return
  // `cursio-app --page gallery --peer <name>` opens on that machine.
  const asked = typeof route.query.peer === "string" ? route.query.peer.toLowerCase() : ""
  const wanted = peers.value.find((p) => p.peer === asked || p.name.toLowerCase() === asked)
  peerId.value = (wanted ?? peers.value[0])?.peer ?? ""
})
const peerOptions = computed<SegmentedOption[]>(() => peers.value.map((p) => ({ value: p.peer, label: p.name })))
const revision = computed(() => peers.value.find((p) => p.peer === peerId.value)?.revision ?? 0)

const filter = ref<MediaFilter>("All")
const filterOptions = computed<SegmentedOption[]>(() => [
  { value: "All", label: t("gallery.all") },
  { value: "Images", label: t("gallery.images") },
  { value: "Videos", label: t("gallery.videos") },
])

const items = ref<MediaItem[]>([])
const ended = ref(false)
const loading = ref(false)
/** Thumbnail object URLs by item id, filled as cells come into view. */
const thumbs = reactive(new Map<string, string | null>())

async function load(more = false) {
  if (!peerId.value || (more && (ended.value || loading.value))) return
  const peer = peerId.value
  loading.value = true
  try {
    const page = await gallery.list(peer, more ? items.value.at(-1) : undefined, filter.value)
    if (peer !== peerId.value) return
    items.value = more ? [...items.value, ...page] : page
    ended.value = page.length < GALLERY_PAGE
  } catch (e) {
    toast.failure(e, { title: t("gallery.loadFailed"), retry: () => load(more) })
  } finally {
    loading.value = false
  }
}

function reset() {
  gallery.forget()
  thumbs.clear()
  items.value = []
  ended.value = false
  void load()
}
watch([peerId, filter], reset)
watch(revision, () => void load())
onMounted(() => void load())
onBeforeUnmount(() => {
  gallery.forget()
  for (const url of full.values()) URL.revokeObjectURL(url)
})

/** Ask for a cell's thumbnail once it scrolls into view, not before. */
const seen = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue
      const id = (entry.target as HTMLElement).dataset.id
      const item = id ? items.value.find((i) => i.id === id) : undefined
      seen.unobserve(entry.target)
      if (!item || thumbs.has(item.id)) continue
      const peer = peerId.value
      gallery
        .thumbnail(peer, item)
        .then((url) => peer === peerId.value && thumbs.set(item.id, url))
        .catch(() => undefined)
    }
  },
  { rootMargin: "400px" },
)
const watchCell = (el: Element | ComponentPublicInstance | null) => {
  if (el instanceof HTMLElement) seen.observe(el)
}

/** The next page is asked for as the end of the grid comes near. */
const sentinel = ref<HTMLElement | null>(null)
const nearEnd = new IntersectionObserver((entries) => {
  if (entries.some((e) => e.isIntersecting)) void load(true)
}, { rootMargin: "600px" })
watch(sentinel, (el, old) => {
  if (old) nearEnd.unobserve(old)
  if (el) nearEnd.observe(el)
})
onBeforeUnmount(() => {
  seen.disconnect()
  nearEnd.disconnect()
})

// The viewer pages through photos only: a video plays in its own dialog.
const photos = computed(() => items.value.filter((i) => i.kind === "Image"))
/** Whole photos read so far, as object URLs, by item id. */
const full = reactive(new Map<string, string>())
const viewing = ref(false)
const index = ref(0)

const lightbox = computed<LightboxItem[]>(() =>
  photos.value.map((p) => ({
    src: full.get(p.id) ?? thumbs.get(p.id) ?? "",
    thumb: thumbs.get(p.id) ?? undefined,
    title: p.name,
    description: describe(p),
    filename: p.name,
  })),
)

async function readWhole(item: MediaItem) {
  if (full.has(item.id)) return
  const peer = peerId.value
  try {
    const blob = await gallery.whole(peer, item)
    if (peer === peerId.value) full.set(item.id, URL.createObjectURL(blob))
  } catch (e) {
    toast.failure(e, { title: t("gallery.openFailed") })
  }
}

watch([viewing, index], ([open, at]) => {
  const photo = open ? photos.value[at] : undefined
  if (photo) void readWhole(photo)
})

const video = ref<MediaItem | null>(null)
const videoUrl = ref("")
const videoDone = ref(0)

async function openItem(item: MediaItem) {
  if (item.kind === "Image") {
    index.value = photos.value.findIndex((p) => p.id === item.id)
    viewing.value = true
    return
  }
  video.value = item
  videoUrl.value = full.get(item.id) ?? ""
  videoDone.value = 0
  if (videoUrl.value) return
  const peer = peerId.value
  try {
    const blob = await gallery.whole(peer, item, (done) => (videoDone.value = done))
    const url = URL.createObjectURL(blob)
    full.set(item.id, url)
    if (video.value?.id === item.id) videoUrl.value = url
  } catch (e) {
    toast.failure(e, { title: t("gallery.openFailed") })
    video.value = null
  }
}

const saving = ref<string | null>(null)
async function save(item: MediaItem) {
  saving.value = item.id
  try {
    const path = await gallery.save(peerId.value, item)
    toast.success(t("gallery.saved", { path }))
  } catch (e) {
    toast.failure(e, { title: t("gallery.saveFailed") })
  } finally {
    saving.value = null
  }
}

function onCopy(_item: LightboxItem, ok: boolean) {
  if (ok) toast.success(t("gallery.copied"))
  else toast.failure(new Error(t("gallery.copyFailed")))
}

function size(bytes: number): string {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(1)} GB`
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(1)} MB`
  return `${Math.max(1, Math.round(bytes / 1024))} KB`
}

function length(ms: number): string {
  const s = Math.round(ms / 1000)
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`
}

function describe(item: MediaItem): string {
  const parts = [new Date(item.taken_ms).toLocaleString([], { dateStyle: "medium", timeStyle: "short" })]
  if (item.width && item.height) parts.push(`${item.width}×${item.height}`)
  parts.push(size(item.size))
  return parts.join(" · ")
}
</script>

<template>
  <DesktopShell>
    <section class="flex flex-col gap-4">
      <div class="flex flex-wrap items-center justify-between gap-3">
        <LpSection :title="t('gallery.title')" :description="t('gallery.description')" icon="lucide:images" />
        <div class="flex flex-wrap items-center gap-2">
          <LpSegmented v-if="peers.length > 1" v-model="peerId" :options="peerOptions" size="sm" />
          <LpSegmented v-model="filter" :options="filterOptions" size="sm" />
        </div>
      </div>

      <LpCard v-if="!peers.length" padded>
        <LpEmptyState icon="lucide:images" :title="t('gallery.noPeer')" :description="t('gallery.noPeerHint')" />
      </LpCard>

      <template v-else>
        <div class="grid grid-cols-3 gap-1 sm:grid-cols-4 md:grid-cols-5 lg:grid-cols-6">
          <button
            v-for="item in items"
            :key="item.id"
            :ref="watchCell"
            :data-id="item.id"
            type="button"
            class="group relative aspect-square overflow-hidden rounded-control bg-surface-soft outline-none focus-visible:ring-2 focus-visible:ring-ring"
            :title="`${item.name} · ${describe(item)}`"
            @click="openItem(item)"
          >
            <img
              v-if="thumbs.get(item.id)"
              :src="thumbs.get(item.id)!"
              :alt="item.name"
              class="size-full object-cover transition-transform duration-[var(--duration-fast)] group-hover:scale-105"
              draggable="false"
            />
            <LpSkeleton v-else-if="!thumbs.has(item.id)" class="size-full" />
            <span v-else class="flex size-full items-center justify-center text-muted">
              <LpIcon :name="item.kind === 'Video' ? 'lucide:film' : 'lucide:image-off'" :size="22" />
            </span>
            <span
              v-if="item.kind === 'Video'"
              class="absolute bottom-1 right-1 flex items-center gap-1 rounded bg-black/60 px-1.5 py-0.5 text-[10px] text-white"
            >
              <LpIcon name="lucide:play" :size="10" />
              <template v-if="item.duration_ms">{{ length(item.duration_ms) }}</template>
            </span>
          </button>
        </div>
        <LpEmptyState v-if="!items.length && !loading" compact icon="lucide:image" :title="t('gallery.none')" />
        <div ref="sentinel" class="flex justify-center py-2">
          <LpSkeleton v-if="loading" class="h-4 w-24" />
        </div>
      </template>
    </section>

    <LpLightbox
      v-model:open="viewing"
      v-model:index="index"
      :items="lightbox"
      :downloadable="false"
      copyable
      :filmstrip="false"
      :loop="false"
      :labels="{ close: t('gallery.close'), previous: t('gallery.previous'), next: t('gallery.next'), copy: t('gallery.copy') }"
      @copy="onCopy"
    >
      <template v-if="!isMobile" #actions="{ index: at }">
        <LpTooltip :content="t('gallery.save')">
          <button
            type="button"
            class="inline-flex size-9 items-center justify-center rounded-control text-white/80 hover:bg-white/10 hover:text-white disabled:opacity-50"
            :aria-label="t('gallery.save')"
            :disabled="saving === photos[at]?.id"
            @click="photos[at] && save(photos[at])"
          >
            <LpIcon name="lucide:download" :size="18" />
          </button>
        </LpTooltip>
      </template>
    </LpLightbox>

    <LpModal :open="!!video" :title="video?.name ?? ''" size="lg" @update:open="(open: boolean) => !open && (video = null)">
      <div v-if="video" class="flex flex-col gap-3">
        <video v-if="videoUrl" :src="videoUrl" controls autoplay class="max-h-[70vh] w-full rounded-control bg-black" />
        <div v-else class="flex flex-col gap-2 py-6">
          <LpProgress :value="video.size ? (videoDone / video.size) * 100 : 0" />
          <p class="text-center text-xs text-muted">
            {{ t("gallery.loading", { done: size(videoDone), total: size(video.size) }) }}
          </p>
        </div>
        <div class="flex items-center justify-between gap-2">
          <span class="text-xs text-muted">{{ describe(video) }}</span>
          <LpButton v-if="!isMobile" variant="soft" size="sm" :loading="saving === video.id" @click="save(video)">
            <LpIcon name="lucide:download" :size="14" />{{ t("gallery.save") }}
          </LpButton>
        </div>
      </div>
    </LpModal>
  </DesktopShell>
</template>
