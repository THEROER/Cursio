# Gallery

Photos and videos of one machine, browsed from another: a phone's camera roll
on a computer, a computer's screenshots on a phone. The Phone Link "Photos"
page, for every pair of machines in the mesh rather than one phone and one PC.

## A module, symmetric

`ModuleId::GALLERY` (14; 13 is the media module's). Every machine both answers
for its own photos and asks for its peer's, so the module advertises `Both`
wherever a machine has a gallery and a window to show one — unlike messages,
where only a phone has the messages and only a desktop the window. A machine
whose platform names no Pictures folder still browses: it advertises `Sink`.

Code: `protocol/src/gallery.rs` (wire), `core/src/gallery_session.rs` (the
module on a link), `core/src/gallery_hub.rs` (where a window's requests leave
and this machine's changes are counted), `core/src/gallery_folder.rs` (a
computer's gallery), `crates/android/src/gallery.rs` + `GalleryStore.kt` (a
phone's).

## Asked for, never pushed

A camera roll is gigabytes and almost no link ever opens the page, so nothing
crosses until a window asks: request and response with ids, the shape of the
messages module. The one unasked message is `Changed` — a photo taken,
deleted — after which an open window asks for its first page again.

| Request | Answer | Priority |
|---|---|---|
| `List { limit, after, filter }` | items, newest first | Metadata |
| `Thumbnail { id, max_px }` | JPEG, or none | Bulk |
| `Read { id, offset, len }` | up to 512 KiB of the file | Bulk |

A page is metadata: a person is waiting for it, and a file copy running on the
same link must not stand in front of it. Thumbnails and slices are bulk — a grid
asks for dozens at once, and they must never get between the cursor and the
link. The answering side works on at most four requests at a time, so a phone
is not asked to decode fifty photos together because a grid scrolled.

## Pages end on an item, not a time

`after` is a `PageMark` — the time *and id* of the last item shown — not a
`before_ms` like messages. A folder copied in one go, or a burst of photos in
one second (a phone's dates are in seconds), has many items with one time; a
boundary by time alone dropped every one of them that fell on a page edge.
Ids are compared only by the machine that issued them, which alone knows what
they mean:

- **Computer**: `<folder>/<path in it>`, ordered as strings.
- **Phone**: `i:` or `v:` and the MediaStore row id, zero-padded to 19 digits,
  so comparing them as strings orders them as SQL sorts the numbers.

## An id is untrusted

It comes back off the wire as a string. A computer resolves one only as a
plain relative path below one of its gallery folders, refuses `..`, a root or
a prefix, and checks the result is still inside the folder after links are
followed (`gallery_folder::resolve`) — a gallery is not a way to read
`~/.ssh`. A phone resolves one only as a row of its own two MediaStore tables,
read through the content resolver, so the phone's scoped storage decides what
may be read. Sizes are bounded in `gallery_session::check` before either sees
a request.

## A computer's gallery

The user's Pictures and Videos, as the platform names them (`directories`: the
XDG user dirs on Linux, the Known Folders on Windows). Screenshots live inside
Pictures, so they are part of it. A walk, not an index: hidden entries and
links are skipped, depth is bounded, and the walk is redone when a window asks
for a first page (at most every 10 s) and kept for the pages after it.

Thumbnails are decoded with `image` and cached on disk under the cache dir,
keyed by path, size and modification time, because a grid asks again every
time it opens. A video has none on a computer: decoding one needs FFmpeg,
which only the window module links, and the gallery is not worth tying to it.
The viewer shows a film icon with the video's length.

## A phone's gallery

MediaStore, not a walk of storage: it already indexes the camera roll,
screenshots and every app's saved pictures, and reading it needs only
`READ_MEDIA_IMAGES`/`READ_MEDIA_VIDEO` (Android 13+) or `READ_EXTERNAL_STORAGE`
(older) — not access to all files. The two permissions are two Tauri aliases,
one per Android generation, since an alias naming a permission the manifest
leaves out is refused. The date is `DATE_MODIFIED`: `DATE_TAKEN` is empty for
many screenshots and downloads, and an expression choosing between them is not
a column MediaProvider accepts in a projection since Android 10.

Thumbnails come from `ContentResolver.loadThumbnail` (API 29+, the system's own
cache) or the older thumbnail tables. A list crosses JNI as JSON, as messages
do; a thumbnail or slice crosses as a Java `byte[]`. A ContentObserver on both
tables tells the node of a change, a second after the burst ends.

## The window

`app/ui/app/pages/gallery.vue`, in the desktop rail while any machine shares
its gallery and as a tab on the phone. A grid asks for each thumbnail as its
cell comes near the viewport and for the next page as the end does. A photo
opens in the kit's lightbox, showing its thumbnail at once and the whole photo
once read; a video is read whole and played in a dialog. **Save** (desktop)
copies into `Pictures/Cursio/<machine>` through the node, slice by slice into
a partial file renamed at the end, under a free name beside one already there;
**Copy** puts the photo on this machine's clipboard as PNG.

From GNOME the machine's menu has **Gallery**, which opens
`cursio-app --page gallery --peer <machine>`.

Over the control socket: `gallery_list`, `gallery_thumbnail`, `gallery_read`,
`gallery_save` and `gallery` in the status (control v15). Bytes cross the
socket's JSON as base64.

## Not here

- Dragging a photo out of the window into another app.
- Video thumbnails on a computer, and saving on a phone (a phone's app has no
  Pictures the node can write to without MediaStore).
- Deleting or editing on the other machine: the gallery is read-only.
