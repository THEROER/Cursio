//! A PipeWire param object as the bytes a stream is handed.

use pipewire::spa::pod::{serialize::PodSerializer, Object, Value};

/// Serialize `obj`, a param built here from fixed fields, which cannot fail
/// to serialize; `what` names it if that ever stops being true.
pub(crate) fn pod_bytes(obj: Object, what: &str) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .unwrap_or_else(|e| panic!("serializing a static {what} pod cannot fail: {e:?}"))
        .0
        .into_inner()
}
