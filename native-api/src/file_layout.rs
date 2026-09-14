// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! What a `java.io.File` this VM builds has to contain for the JDK's OWN
//! `File` bytecode to agree with it.
//!
//! # The defect this closes
//!
//! Every `File` this VM constructs kept its path in **slot 0** and wrote
//! nothing else. That is self-consistent for exactly as long as this VM's own
//! natives are the only readers -- which is §1.4's whole story -- and it ends
//! the moment a retirement lets real `File` bytecode run.
//! `UnixFileSystem.resolve` reads `prefixLength`, which was never written and
//! so is `0`, and concludes the path is **not absolute**.
//!
//! Measured 2026-09-11 on the lane-4 wave-1 retirement binary, against a real
//! HotSpot 25 on the same host:
//!
//! ```text
//!   temp.getPath          abs=true len=32 slashes=2      (correct)
//!   temp.isAbsolute       false                          (HotSpot: true)
//!   temp.getAbsolutePath  abs=true len=45 slashes=5      (the cwd, prepended)
//! ```
//!
//! and downstream of it, `RJdkSecurity`: a `javax.net.ssl.trustStore` property
//! naming a path that no longer resolved, so the JDK fell back to `cacerts`,
//! reported **122 trust anchors where the vector expects 1**, and ACCEPTED a
//! certificate HotSpot REJECTS. A widened trust set, out of a missing `int`
//! field, with nothing thrown anywhere. Thirteen lane-4 probes over 4 416 rows
//! scored 0 in both modes on that same binary.
//!
//! # Why this lives in `native-api`
//!
//! `java.io.File` has **six producing call sites across two crates** --
//! `native-io`'s three constructors and `Path.toFile`, and
//! `native-builtins`'s `file_alloc`, `file_alloc_units` and two more
//! `Path.toFile` registrations. A layout helper living in one of them would be
//! a second, drifting copy in the other: the failure family
//! `two-producers-of-one-carrier-class` is named for exactly this. Same
//! reasoning, and the same home, as [`crate::path_layout`] and
//! [`crate::appended_slots`].
//!
//! # Why it is ADDITIVE and does not move the path out of slot 0
//!
//! Those six producers are read back through `native-io`'s `read_file_path` at
//! twenty-two call sites plus a long tail of direct slot-0 reads. Relocating
//! the path would be the same failure family from the other side. So slot 0
//! keeps the String and every existing reader is untouched; [`write`] adds the
//! real fields beside it, and skips any name the class does not declare or any
//! index the object is too narrow to hold.
//!
//! On a real image `File`, slot 0 is `pathStatus` -- see `native_file_to_path`
//! in `native-io` -- whose only consumer is `isInvalid()` asking
//! `s == PathStatus.INVALID`. A String there answers that `false`, which is the
//! answer a correctly `CHECKED` path gives.

use crate::registry::{NativeClassAccess, NativeContext};
use cratonvm_types::{ObjectRef, Value};

/// The prefix length real `java.io.File` bytecode computes for a path.
///
/// `UnixFileSystem.prefixLength` is exactly `path.startsWith("/") ? 1 : 0`.
///
/// **Windows is deliberately absent.** `WinNTFileSystem`'s rule is drive
/// letters and UNC roots, this campaign's two shell gates are keyed `25/linux`
/// and refuse on Windows, and an unmeasured value written into a field the JDK
/// trusts is worse than the pre-2026-09-11 behaviour of not writing it --
/// which is a known quantity. [`write`] therefore writes `path` on every
/// platform and `prefixLength` only on POSIX.
#[cfg(not(windows))]
#[must_use]
pub fn prefix_length(path: &str) -> i32 {
    i32::from(path.starts_with('/'))
}

/// How wide to allocate a fabricated `java.io.File`: the real class's own
/// instance-field count, so `path`, `prefixLength` and `pathStatus` all have
/// somewhere to land.
///
/// Every producer allocated **one** slot before 2026-09-11, which was enough
/// while slot 0 was the only slot anyone wrote — and is exactly the shape that
/// takes the path and then silently keeps `prefixLength = 0`, because [`write`]
/// skips an index past the object's own width rather than writing out of
/// bounds.
pub fn alloc_width(ctx: &mut dyn NativeContext) -> usize {
    match ctx.ensure_class_initialized("java/io/File") {
        Ok(cid) => ctx.class_num_total_fields(cid).max(1),
        Err(_) => 1,
    }
}

/// Write the fields real `java.io.File` bytecode reads, **beside** the slot-0
/// string the caller has already written.
///
/// `path_obj` must be the same `String` the caller stored in slot 0, and
/// `path` its text. Resolving BY NAME against the object's own class and
/// writing BY INDEX is the route that works and the route the real `getfield`
/// takes; see [`crate::path_layout`] for the two attempts that measured inert
/// by resolving through a receiver whose stamp did not declare the name.
pub fn write(ctx: &mut dyn NativeContext, file: ObjectRef, path_obj: ObjectRef, path: &str) {
    let cid = ctx.class_id_of_object(file);
    let width = ctx.object_num_fields(file);
    if let Some(i) = ctx.resolve_field_index_by_class_id(cid, "path") {
        if i < width {
            ctx.set_field(file, i, Value::Object(Some(path_obj)));
        }
    }
    #[cfg(not(windows))]
    if let Some(i) = ctx.resolve_field_index_by_class_id(cid, "prefixLength") {
        if i < width {
            ctx.set_field(file, i, Value::Int(prefix_length(path)));
        }
    }
    #[cfg(windows)]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn prefix_length_is_the_unixfilesystem_rule() {
        assert_eq!(prefix_length("/tmp/x"), 1);
        assert_eq!(prefix_length("/"), 1);
        assert_eq!(prefix_length("rel.txt"), 0);
        assert_eq!(prefix_length(""), 0);
    }
}
