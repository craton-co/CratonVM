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

/// A native is about to hand back an instance of a class `new` could not have
/// produced. Report it ONCE per class, naming the native that asked.
///
/// Returns whether this call was the one that reported.
///
/// # Why this lives here rather than at the funnel that calls it
///
/// It was written inline in `native-builtins`' fabrication funnel, and it cost
/// that crate its 429th raw lock construction. `lock_discipline_ratchet` holds
/// `native-builtins` to a baseline for a reason it states plainly: that crate
/// RE-ENTERS the VM — a native callback calls back into Java, which takes the
/// heap and the L10 class-manager locks — so a new global lock there with no
/// `LockLevel` is a deadlock the order checker cannot see. A diagnostic census
/// has no business being the thing that raises that ceiling, and the predicate
/// it needs (`ACC_INTERFACE` / `ACC_ABSTRACT`) is already here.
///
/// # The shape is `layout_alias::observe`'s, deliberately
///
/// A plain `parking_lot::Mutex`, which is this crate's convention
/// (`capability.rs`, `fd_table.rs`); the ordered wrappers are the other crate's
/// ratchet and do not apply here. The guard lives for exactly one `insert` and
/// is dropped before the `tracing::warn!` — the subscriber re-enters the VM, so
/// warning under the guard would be the inversion this move exists to avoid.
///
/// # Why a `warn!` and not a `--jdk-only` report row
///
/// A new `JdkOnlyViolation` variant is a wire-format change: `kind()` is the
/// documented `"kind"` field of every report row, `difftest`'s census tallies by
/// exactly that string, and `jfr`'s `jdk_only` holds a CLOSED label vocabulary
/// with its own schema version. That is the right eventual home and it is four
/// crates and a schema bump; a half-added kind — recorded but not tallied, or
/// tallied but not labelled — is worse than none.
///
/// # Why it is not behind a flag
///
/// `layout_alias` is, and that is exactly why the definition-of-done screen
/// could not see this species: the screen reads the REPORT, not a debug flag
/// that is off in almost every run. MEASURED 2026-08-29
/// (`probes/AbstractReceiverSweep`): seven `java.lang.foreign` sites answer an
/// interface under `--jdk-only`, and one — `Arena.ofConfined()` — does so in
/// compatible mode too, on runs whose report said `compatibility_classes: 0`.
/// The predicate counts classes MINTED; this species is an allocation against a
/// class that is perfectly real. Deduped by class name, so a segment-heavy
/// workload pays one line per carrier rather than thousands.
///
/// `#[track_caller]` all the way up the funnel, so the location reported is the
/// NATIVE that asked for the shape, not this line and not the forwarder.
#[track_caller]
pub fn observe_uninstantiable_receiver(class_name: &str, flags: u16) -> bool {
    if flags & (ACC_INTERFACE | ACC_ABSTRACT) == 0 {
        return false;
    }
    static SEEN: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()));
    {
        let mut guard = seen.lock();
        if !guard.insert(class_name.to_string()) {
            return false;
        }
    }
    let site = core::panic::Location::caller();
    tracing::warn!(
        class = %class_name,
        requester = %format!("{}:{}", site.file(), site.line()),
        kind = if flags & ACC_INTERFACE != 0 { "interface" } else { "abstract" },
        "a native allocated an instance of a class `new` could not produce (JVMS 6.5);          the definition-of-done screen's compatibility_classes counts classes MINTED and          cannot see this. Reported once per class."
    );
    true
}
