//! Clipboard sharing on the mesh: the clipboard module on one link, over
//! `clipboard-core`'s platform layer. See `docs/clipboard-design.md`.
//!
//! Split out of `cursio-core`, which re-exports it as
//! `cursio_core::clipboard_session`. With `files`, a copied file list is
//! rewritten to the same files through the peer's mounted volume.

pub mod clipboard_session;

use cursio_core_base::module;
#[cfg(test)]
use cursio_core_base::test_support;
#[cfg(feature = "files")]
use cursio_files_session::files_session;
