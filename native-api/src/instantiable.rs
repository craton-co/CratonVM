// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Can `new` legally produce an instance of this class?
//!
//! # The species this closes
//!
//! JVMS §6.5 makes `new` on an ABSTRACT class or an INTERFACE an
//! `InstantiationError`. So a receiver whose runtime class is either is one no
//! bytecode in any image could have produced — and this VM produced them, in
//! both modes, at thirteen measured sites across `java.nio.channels`,
//! `java.nio.file` and `java.nio.fs`:
//!
//! ```text
//! Pipe.open()                      -> java.nio.channels.Pipe            abstract   (H21-1)
//! DatagramChannel.open()           -> java.nio.channels.DatagramChannel abstract
//! FileSystems.getDefault()         -> sun.nio.fs.UnixFileSystem         abstract
//! FileSystems.getDefault()
//!   .newWatchService()             -> java.nio.file.WatchService        INTERFACE
//! ```
//!
//! `probes/W4Abstract.java` is the assertion that finds them, and it needs no
//! oracle: any `ABSTRACT` or `INTERFACE` line in its output is a defect on its
//! own terms (`H21-1` N3, the "free universal assertion nobody makes").
//!
//! # Why the predicate lives here, and why every fix needs it
//!
//! The fix for each site is to name the concrete class the JDK itself would
//! build — and that class is PLATFORM-SPECIFIC and gets renamed between JDK
//! releases (`sun.nio.fs.LinuxFileSystem` on Linux,
//! `sun.nio.fs.WindowsFileSystem` on Windows, and `sun.nio.fs.UnixFileSystem`
//! — the ABSTRACT parent — is what an out-of-date candidate list resolves to).
//! Trusting the NAME therefore reproduces the very defect the fix was written
//! to close, silently, on whichever platform the author did not run.
//! [`first_instantiable`] checks the CLASS instead.
//!
//! Two crates need it: `native-io`'s `concrete_receiver` (which mints) and
//! `native-builtins`' `getClass()` alias table (which reports). Neither depends
//! on the other, and a second copy of a predicate is how the fifteen copies
//! `crate::appended_slots` was written to collapse got there in the first
//! place.

use crate::registry::{NativeClassAccess, NativeContext};
use cratonvm_types::ClassId;

/// `ACC_INTERFACE`, JVMS table 4.1-B.
pub const ACC_INTERFACE: u16 = 0x0200;
/// `ACC_ABSTRACT`, JVMS table 4.1-B.
pub const ACC_ABSTRACT: u16 = 0x0400;

/// Whether `new` could legally produce an instance of `class_id`.
#[must_use]
pub fn class_is_instantiable(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    let flags = ctx.class_access_flags(class_id);
    flags & (ACC_INTERFACE | ACC_ABSTRACT) == 0
}

/// The first of `candidates` the image has AND that `new` could legally
/// produce, with its `ClassId`.
///
/// The name is returned as well as the id because callers need it to ask
/// [`crate::appended_slots::base_for_class`], and a caller that re-derived the
/// name from the id could disagree with the one that was resolved.
///
/// Resolution order per candidate: `ensure_class_initialized`, then a plain
/// `load_class` + by-name lookup. The second try matters — several of these
/// `sun.nio.*` implementations have a `<clinit>` that calls a JNI entry point
/// this VM does not provide (`sun.nio.fs.LinuxWatchService`'s calls
/// `eventSize()`), and a class whose initialiser threw is still a perfectly
/// good ANSWER to "what class would the JDK have used here": the object a
/// native mints is filled in by natives, not by that `<clinit>`.
#[must_use]
pub fn first_instantiable<'a>(
    ctx: &mut dyn NativeContext,
    candidates: &'a [&'a str],
) -> Option<(&'a str, ClassId)> {
    for name in candidates {
        let resolved = match ctx.ensure_class_initialized(name) {
            Ok(cid) => Some(cid),
            Err(_) => {
                // The return value is discarded on purpose: `class_id_by_name`
                // is the answer either way, and a load that failed leaves it
                // `None`.
                let _ = ctx.load_class(name);
                ctx.class_id_by_name(name)
            }
        };
        if let Some(cid) = resolved {
            if class_is_instantiable(ctx, cid) {
                return Some((name, cid));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant the callers rely on: whatever comes back, `new` could
    /// have produced it. Written as an invariant rather than as "an
    /// unresolvable name answers None" because a mock — and the real VM in
    /// synthetic-JDK mode — is allowed to FABRICATE a class for an unknown
    /// name, and a fabricated class is concrete and therefore a legitimate
    /// answer here.
    #[test]
    fn whatever_is_returned_is_something_new_could_produce() {
        let mut ctx = crate::test_mock::MockNativeContext::new();
        if let Some((_, cid)) = first_instantiable(&mut ctx, &["does/not/Exist"]) {
            assert!(class_is_instantiable(&ctx, cid));
        }
    }
}
