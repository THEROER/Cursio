//! This phone's photos and videos, answered for a peer.
//!
//! `GalleryStore` in Kotlin reads MediaStore. A page of the list crosses JNI
//! as JSON, like messages; a thumbnail or a slice of a file crosses as a Java
//! `byte[]`, because half a megabyte of photo as base64 inside a string is
//! three copies of it for nothing.
//!
//! Kotlin refuses by throwing, with the reason in the message; the exception's
//! class name is taken off before the reason travels, since a peer's window
//! shows it to a person.

use std::sync::Arc;

use cursio_core::gallery_hub::MediaStore;
use cursio_protocol::gallery::{
    GalleryRequest, GalleryResponse, MediaFilter, MediaItem, MediaKind,
};
use jni::objects::{JByteArray, JObject, JString, JValue};
use serde::Deserialize;
use serde_json::json;

pub struct AndroidGallery;

impl AndroidGallery {
    pub fn shared() -> Arc<dyn MediaStore> {
        Arc::new(AndroidGallery)
    }
}

impl MediaStore for AndroidGallery {
    fn handle(&self, request: &GalleryRequest) -> Result<GalleryResponse, String> {
        match request {
            GalleryRequest::List { .. } => {
                let question = request_json(request);
                let answer = crate::jvm::with_native(|env, class| {
                    let question = JObject::from(env.new_string(&question)?);
                    let result = env.call_static_method(
                        class,
                        "onGalleryList",
                        "(Ljava/lang/String;)Ljava/lang/String;",
                        &[JValue::Object(&question)],
                    )?;
                    let answer: JString = result.l()?.into();
                    let text: String = env.get_string(&answer)?.into();
                    Ok(text)
                })
                .map_err(reason)?;
                parse_items(&answer).map(GalleryResponse::Items)
            }
            GalleryRequest::Thumbnail { id, max_px } => {
                bytes(id, *max_px as i32, 0, 0).map(GalleryResponse::Thumbnail)
            }
            GalleryRequest::Read { id, offset, len } => bytes(id, 0, *offset as i64, *len as i32)
                .map(|b| GalleryResponse::Slice(b.unwrap_or_default())),
        }
    }
}

/// `thumb_px > 0` asks for a thumbnail; otherwise `len` bytes from `offset`.
/// `None` is Kotlin's `null`: no thumbnail for this item.
fn bytes(id: &str, thumb_px: i32, offset: i64, len: i32) -> Result<Option<Vec<u8>>, String> {
    crate::jvm::with_native(|env, class| {
        let id = JObject::from(env.new_string(id)?);
        let result = env.call_static_method(
            class,
            "onGalleryBytes",
            "(Ljava/lang/String;IJI)[B",
            &[
                JValue::Object(&id),
                JValue::Int(thumb_px),
                JValue::Long(offset),
                JValue::Int(len),
            ],
        )?;
        let array = result.l()?;
        if array.is_null() {
            return Ok(None);
        }
        let array = JByteArray::from(array);
        Ok(Some(env.convert_byte_array(&array)?))
    })
    .map_err(reason)
}

/// `java.lang.SecurityException: allow …` → `allow …`.
fn reason(described: String) -> String {
    match described.split_once(": ") {
        Some((class, rest)) if class.contains("Exception") || class.contains("Error") => {
            rest.to_string()
        }
        _ => described,
    }
}

fn request_json(request: &GalleryRequest) -> String {
    match request {
        GalleryRequest::List {
            limit,
            after,
            filter,
        } => json!({
            "limit": limit,
            "after_ms": after.as_ref().map(|m| m.taken_ms),
            "after_id": after.as_ref().map(|m| m.id.as_str()),
            "filter": match filter {
                MediaFilter::All => "all",
                MediaFilter::Images => "images",
                MediaFilter::Videos => "videos",
            },
        })
        .to_string(),
        _ => String::new(),
    }
}

#[derive(Deserialize)]
struct JsonItem {
    id: String,
    name: String,
    video: bool,
    taken: i64,
    size: u64,
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<u64>,
    mime: String,
}

#[derive(Deserialize)]
struct Answer {
    error: Option<String>,
    #[serde(default)]
    items: Vec<JsonItem>,
}

fn parse_items(answer: &str) -> Result<Vec<MediaItem>, String> {
    let answer: Answer = serde_json::from_str(answer)
        .map_err(|e| format!("the phone's answer could not be read: {e}"))?;
    if let Some(error) = answer.error {
        return Err(error);
    }
    Ok(answer
        .items
        .into_iter()
        .map(|i| MediaItem {
            id: i.id,
            name: i.name,
            kind: if i.video {
                MediaKind::Video
            } else {
                MediaKind::Image
            },
            taken_ms: i.taken,
            size: i.size,
            // MediaStore reports 0 for a size it has not measured yet.
            width: i.width.filter(|w| *w > 0),
            height: i.height.filter(|h| *h > 0),
            duration_ms: i.duration.filter(|d| *d > 0),
            mime: i.mime,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursio_protocol::gallery::PageMark;

    #[test]
    fn a_page_from_mediastore_becomes_the_wire_vocabulary() {
        let answer = r#"{"items":[{"id":"v:12","name":"VID.mp4","video":true,"taken":1700000000000,"size":99,"width":0,"height":0,"duration":4500,"mime":"video/mp4"}]}"#;
        let items = parse_items(answer).unwrap();
        assert_eq!(items[0].kind, MediaKind::Video);
        assert_eq!(items[0].width, None);
        assert_eq!(items[0].duration_ms, Some(4500));
    }

    #[test]
    fn the_phones_refusal_reaches_the_peer_without_the_exception_class() {
        assert_eq!(
            reason("java.lang.SecurityException: allow Cursio to see photos".into()),
            "allow Cursio to see photos"
        );
        assert_eq!(
            parse_items(r#"{"error":"allow Cursio to see photos"}"#),
            Err("allow Cursio to see photos".into())
        );
    }

    #[test]
    fn a_later_page_carries_where_the_last_one_ended() {
        let json = request_json(&GalleryRequest::List {
            limit: 30,
            after: Some(PageMark {
                taken_ms: 5,
                id: "i:7".into(),
            }),
            filter: MediaFilter::Images,
        });
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["after_ms"], 5);
        assert_eq!(value["after_id"], "i:7");
        assert_eq!(value["filter"], "images");
    }
}
