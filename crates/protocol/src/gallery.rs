//! Photos and videos of one machine, browsed from another.
//!
//! Every machine has a gallery: a phone's camera roll and screenshots, a
//! computer's Pictures and Videos. Any of them may look at any other's, so
//! unlike messages the module is symmetric — a phone shows a computer's
//! screenshots as readily as a computer shows a phone's photos.
//!
//! ## Asked for, never pushed
//!
//! A camera roll is gigabytes, and almost every link never opens the gallery.
//! So the exchange is request and response with ids, like file sharing: a page
//! of the list, one thumbnail, one slice of one file. The only thing sent
//! unasked is [`GalleryMsg::Changed`] — "look again" — when a photo is taken or
//! deleted.
//!
//! ## Three sizes of answer, three priorities
//!
//! A page of the list is a few kilobytes that a person is waiting for; a
//! thumbnail is tens of kilobytes and a grid asks for dozens at once; a slice
//! of a full photo is up to [`MAX_SLICE`]. The list goes as metadata, which a
//! running file copy cannot hold up, and thumbnails and slices as bulk, behind
//! input and sound. See [`GalleryMsg::priority`].
//!
//! ## Pages run backwards in time
//!
//! Newest first, paged from the last item shown ([`PageMark`]) — the same
//! reason as messages: a boundary in time stays right while new photos arrive,
//! and an offset would shift under them. Unlike messages the mark carries the
//! item's id too: a folder copied in one go has hundreds of photos with the
//! same time, and a boundary by time alone would drop every one of them that
//! fell on the page edge.

use serde::{Deserialize, Serialize};

use crate::Priority;

/// Most items one page may hold.
pub const MAX_PAGE: u32 = 200;

/// Largest thumbnail edge a peer may ask for, in pixels. A grid cell is a few
/// hundred pixels; more is a full image asked for the wrong way.
pub const MAX_THUMBNAIL_PX: u32 = 1024;

/// Largest slice of a file one answer carries. A whole photo in one message
/// would sit in front of every other bulk frame for seconds; slices let the
/// link interleave.
pub const MAX_SLICE: u32 = 512 * 1024;

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Image,
    Video,
}

/// Which items a page lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MediaFilter {
    #[default]
    All,
    Images,
    Videos,
}

/// One photo or video.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaItem {
    /// The answering machine's own handle for the item, opaque to the asker:
    /// a MediaStore id on a phone, a path relative to a gallery folder on a
    /// computer. Only ever handed back to the machine that issued it.
    pub id: String,
    /// File name, for saving.
    pub name: String,
    pub kind: MediaKind,
    /// When it was taken (or, failing that, last changed), Unix milliseconds.
    pub taken_ms: i64,
    pub size: u64,
    /// Pixel size where the machine knows it without decoding the file.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Length of a video, where known.
    pub duration_ms: Option<u64>,
    /// MIME type, for the viewer and for saving with the right extension.
    pub mime: String,
}

/// The last item a page ended on. Items with the same time are ordered by the
/// answering machine, which alone knows what its ids mean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageMark {
    pub taken_ms: i64,
    pub id: String,
}

/// What one machine asks another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GalleryRequest {
    /// Newest first, starting after `after` when given.
    List {
        limit: u32,
        after: Option<PageMark>,
        filter: MediaFilter,
    },
    /// A JPEG no larger than `max_px` on its longer edge.
    Thumbnail { id: String, max_px: u32 },
    /// `len` bytes of the file from `offset`.
    Read { id: String, offset: u64, len: u32 },
}

/// The answer to one [`GalleryRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GalleryResponse {
    Items(Vec<MediaItem>),
    /// JPEG bytes; `None` when the machine cannot make one for this item (a
    /// video on a computer without a decoder), which the viewer shows as a
    /// placeholder rather than an error.
    Thumbnail(Option<Vec<u8>>),
    /// The bytes read; shorter than asked only at the end of the file.
    Slice(Vec<u8>),
}

/// The gallery's messages: requests either way (every machine both shows its
/// photos and browses its peer's), and `Changed` when something was added or
/// removed.
pub type GalleryMsg = crate::exchange::ExchangeMsg<GalleryRequest, GalleryResponse>;

impl GalleryMsg {
    /// The class this message rides in; see the module docs.
    pub fn priority(&self) -> Priority {
        match self {
            GalleryMsg::Response {
                response: Ok(GalleryResponse::Thumbnail(_) | GalleryResponse::Slice(_)),
                ..
            } => Priority::Bulk,
            _ => Priority::Metadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_of_photos_round_trips() {
        let msg = GalleryMsg::Response {
            id: 3,
            response: Ok(GalleryResponse::Items(vec![MediaItem {
                id: "content://media/external/images/media/42".into(),
                name: "IMG_0042.jpg".into(),
                kind: MediaKind::Image,
                taken_ms: 1_700_000_000_000,
                size: 2_345_678,
                width: Some(4000),
                height: Some(3000),
                duration_ms: None,
                mime: "image/jpeg".into(),
            }])),
        };
        let bytes = bincode::serialize(&msg).unwrap();
        assert_eq!(bincode::deserialize::<GalleryMsg>(&bytes).unwrap(), msg);
    }

    #[test]
    fn pixels_ride_behind_input_and_listings_do_not() {
        let slice = GalleryMsg::Response {
            id: 1,
            response: Ok(GalleryResponse::Slice(vec![0; 16])),
        };
        let list = GalleryMsg::Request {
            id: 2,
            request: GalleryRequest::List {
                limit: 10,
                after: None,
                filter: MediaFilter::All,
            },
        };
        assert_eq!(slice.priority(), Priority::Bulk);
        assert_eq!(list.priority(), Priority::Metadata);
    }
}
