//! The vocabulary every request-and-answer module shares.
//!
//! Messages and the gallery have the same three things to say: a numbered
//! request, the answer to one (or why there is none), and an unasked "look
//! again" when the answering side's store changed. Only what is asked and
//! answered differs, so the shape is written once and each module names its
//! own instance ([`MessagesMsg`](crate::messages::MessagesMsg),
//! [`GalleryMsg`](crate::gallery::GalleryMsg)). The variants keep the order and
//! fields the two separate enums had, so the bytes on the wire are the same.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExchangeMsg<Req, Resp> {
    /// The asking side's request, numbered so its answer can be matched.
    Request { id: u32, request: Req },
    /// The answer to request `id`, or why there is none.
    Response {
        id: u32,
        response: Result<Resp, String>,
    },
    /// Something in the answering side's store changed; whatever is shown
    /// should be asked for again.
    Changed,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The enum a module had of its own before this one existed.
    #[derive(Serialize)]
    enum Separate {
        Request {
            id: u32,
            request: String,
        },
        Response {
            id: u32,
            response: Result<u64, String>,
        },
        Changed,
    }

    #[test]
    fn the_shared_shape_puts_the_same_bytes_on_the_wire_as_a_module_of_its_own() {
        type Shared = ExchangeMsg<String, u64>;
        let pairs = [
            (
                bincode::serialize(&Shared::Request {
                    id: 7,
                    request: "page".into(),
                }),
                bincode::serialize(&Separate::Request {
                    id: 7,
                    request: "page".into(),
                }),
            ),
            (
                bincode::serialize(&Shared::Response {
                    id: 7,
                    response: Err("no".into()),
                }),
                bincode::serialize(&Separate::Response {
                    id: 7,
                    response: Err("no".into()),
                }),
            ),
            (
                bincode::serialize(&Shared::Response {
                    id: 8,
                    response: Ok(42),
                }),
                bincode::serialize(&Separate::Response {
                    id: 8,
                    response: Ok(42),
                }),
            ),
            (
                bincode::serialize(&Shared::Changed),
                bincode::serialize(&Separate::Changed),
            ),
        ];
        for (shared, separate) in pairs {
            assert_eq!(shared.unwrap(), separate.unwrap());
        }
    }
}
