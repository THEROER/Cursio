//! Listing a directory for Windows: the peer's entries, cached, matched
//! against the query's pattern and handed out in batches.

use crate::winmount::convert::to_ntstatus;
use crate::winmount::fs::info::fill_info_at;
use crate::winmount::fs::info::set_entry_name;
use crate::winmount::fs::info::synthetic_dir;
use crate::winmount::fs::pattern::matches_pattern;
use crate::winmount::fs::FileCtx;
use crate::winmount::fs::RemoteFs;
use cursio_protocol::FileError;
use widestring::U16CStr;
use winfsp::filesystem::DirInfo;
use winfsp::filesystem::DirMarker;
use winfsp::filesystem::WideNameInfo;

impl RemoteFs {
    /// [`FileSystemContext::read_directory`](winfsp::filesystem::FileSystemContext::read_directory): one batch of the listing,
    /// matched against the pattern Windows sent and resumed after its marker.
    pub(super) fn list_into(
        &self,
        context: &FileCtx,
        pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let _call = crate::inflight::shared().enter("read_directory");
        if !context.is_dir {
            return Err(to_ntstatus(&FileError::WrongKind));
        }
        // Once per enumeration: a continuation carries a marker and lists the
        // same directory again.
        if marker.inner_as_cstr().is_none() {
            crate::walk::observe(
                &self.peer_name,
                &self.bound,
                context.opener,
                crate::walk::dir_key(&context.volume, &context.path),
            );
        }

        // Fill the caller's buffer directly, the way WinFsp's own MEMFS sample
        // does, rather than going through a `DirBuffer`.
        //
        // The buffered route ended enumeration by *not* finding the marker: its
        // continuation is a binary search over records WinFsp sorted itself, and
        // once the marker reached the last name the search kept resolving to
        // that same record. Asked for what follows `var`, it answered `var`,
        // forever — 2.5 million entries in a directory of 24.
        //
        // Writing straight into `buffer` removes the search entirely: we skip
        // past the marker ourselves and append until the buffer is full, which
        // is exactly what the sample implementation does.
        let mut cursor = 0u32;

        // `.` and `..` come first, and only at the start of an enumeration —
        // the sample emits `.` when there is no marker and `..` when the marker
        // is `.`, then clears the marker so the real entries follow.
        let pattern = pattern.map(|p| p.to_string_lossy());

        let mut skip_until = marker.inner_as_cstr().map(|m| m.to_string_lossy());
        if skip_until.is_none() || skip_until.as_deref() == Some(".") {
            let starting = skip_until.is_none();
            for dot in [".", ".."] {
                if starting || dot == ".." {
                    let mut info: DirInfo = DirInfo::default();
                    let dot_path = if dot == "." {
                        context.path.clone()
                    } else {
                        // The parent of the volume root is the mount root.
                        context
                            .path
                            .rsplit_once('/')
                            .map(|(p, _)| p.to_string())
                            .unwrap_or_default()
                    };
                    fill_info_at(
                        info.file_info_mut(),
                        &synthetic_dir(dot),
                        &context.volume,
                        &dot_path,
                    );
                    set_entry_name(&mut info, dot)?;
                    if !info.append_to_buffer(buffer, &mut cursor) {
                        return Ok(cursor);
                    }
                }
            }
            // Past the dot entries: everything after this is a real name.
            skip_until = None;
        }

        // Windows may ask for a subset. Answering with the whole directory
        // regardless is not merely wasteful — the caller takes the mismatch as a
        // malformed reply, which is how `cargo` clearing a `.temp-archive`
        // directory got `ERROR_INVALID_NAME` on a name that was perfectly valid.
        let entries = self.entries_of(context).map_err(|e| to_ntstatus(&e))?;
        let mut past_marker = skip_until.is_none();
        for meta in entries {
            if let Some(pat) = pattern.as_deref() {
                if !matches_pattern(pat, &meta.name) {
                    continue;
                }
            }
            // Resume *after* the marker, never at it — returning the marker
            // again is what made the listing repeat.
            if !past_marker {
                if Some(meta.name.as_str()) == skip_until.as_deref() {
                    past_marker = true;
                }
                continue;
            }
            let mut info: DirInfo = DirInfo::default();
            let child = if context.path.is_empty() {
                meta.name.clone()
            } else {
                format!("{}/{}", context.path, meta.name)
            };
            fill_info_at(info.file_info_mut(), &meta, &context.volume, &child);
            set_entry_name(&mut info, &meta.name)?;
            if !info.append_to_buffer(buffer, &mut cursor) {
                // Buffer full: what is left comes on the next call.
                return Ok(cursor);
            }
        }

        // Everything fit, so this is the end of the directory. The terminating
        // call is what tells the caller to stop asking.
        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }
}
