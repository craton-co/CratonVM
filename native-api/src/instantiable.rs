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

/// One recorded JVMS §6.5 violation: the offending class, whether it was an
/// interface or abstract, and the native that asked for the shape.
///
/// The requester is kept as an owned `String` rather than the
/// `&'static Location` it came from because the row outlives the call: the
/// census is read once, at exit, on another stack entirely.
type UninstantiableRow = (String, &'static str, String);

/// Every distinct offending class this process has seen, in first-seen order.
///
/// A plain `parking_lot::Mutex`, which is this crate's convention
/// (`capability.rs`, `fd_table.rs`); the ordered wrappers are the other crate's
/// ratchet and do not apply here. First-seen order rather than sorted, because
/// the order the boot produced them in is the order a reader retracing that
/// boot needs.
static SEEN: std::sync::OnceLock<parking_lot::Mutex<Vec<UninstantiableRow>>> =
    std::sync::OnceLock::new();

/// `CRATONVM_DBG_LAYOUT_ALIAS=1` — restore the per-class row, with its
/// `requester=`, at the moment the violation happens rather than at exit.
///
/// No flag of its own, deliberately. This is the same species the alias census
/// covers — the VM's own model of a class disagreeing with what the image
/// declares — and `layout_alias.rs` already reads exactly this name for it.
/// Having to arm two flags to see two halves of one story is how half of it
/// gets missed.
fn uninstantiable_verbose() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LAYOUT_ALIAS").is_some())
}

/// A native is about to hand back an instance of a class `new` could not have
/// produced. Record it ONCE per class, naming the native that asked.
///
/// Returns whether this call was the one that recorded — i.e. whether this
/// class was new to the census.
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
/// # 2026-09-01: it counts here and reports at exit
///
/// This used to `warn!` per class as the violation happened, and a stock
/// `cratonvm Hello` — a one-line hello-world — paid three of them
/// (`java/util/stream/IntStream` interface, `java/lang/invoke/MethodHandle`
/// and `java/lang/invoke/VarHandle` abstract), each carrying the same
/// four-line paragraph. HotSpot prints nothing on that program. Three copies
/// of one paragraph during boot is the background a real warning then has to
/// be noticed against, and every tool that reads this VM's stderr sees it.
///
/// **Nothing is silenced.** The species stays a default-run report; it moved
/// from three lines during boot to one line at exit that names every offending
/// class, its kind and its requester — strictly more than any single line
/// carried before, since a reader no longer has to correlate three records to
/// learn how many distinct classes were involved. This is the shape
/// `cratonvm_types::compact_value::coercion_census::exit_summary` established
/// on the same day for the same measurement, and matching it matters: two
/// censuses of VM-internal type damage that print in two different styles are
/// two things to learn instead of one.
///
/// The violations themselves are UNCHANGED and still open. A previous
/// investigation established, with a site census, that each of the three
/// classes is minted from several files, that the `IntStream` interface name is
/// load-bearing (two exact-match tables in `native-collections` key on the
/// literal string), and that re-minting `MethodHandle`/`VarHandle` as concrete
/// classes without a matching arm in
/// `vm/src/runtime/interpreter/typecheck.rs` would turn a harmless wrong
/// `getClass()` into a wrong `checkcast` on every lambda call — strictly worse.
/// So this changes when and how the census is printed, and nothing about what
/// it is counting.
///
/// # Why a census line and not a `--jdk-only` report row
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
/// workload pays one census row per carrier rather than thousands.
///
/// `#[track_caller]` all the way up the funnel, so the location recorded is the
/// NATIVE that asked for the shape, not this line and not the forwarder.
#[track_caller]
pub fn observe_uninstantiable_receiver(class_name: &str, flags: u16) -> bool {
    if flags & (ACC_INTERFACE | ACC_ABSTRACT) == 0 {
        return false;
    }
    let kind = if flags & ACC_INTERFACE != 0 {
        "interface"
    } else {
        "abstract"
    };
    let site = core::panic::Location::caller();
    let requester = format!("{}:{}", site.file(), site.line());
    {
        let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(Vec::new()));
        let mut guard = seen.lock();
        if guard.iter().any(|(name, _, _)| name.as_str() == class_name) {
            return false;
        }
        guard.push((class_name.to_string(), kind, requester.clone()));
    }
    // The guard above lives for exactly one push and is dropped before this:
    // the `tracing` subscriber re-enters the VM, so emitting under it would be
    // the lock inversion the move to this crate exists to avoid.
    if uninstantiable_verbose() {
        tracing::warn!(
            class = %class_name,
            requester = %requester,
            kind = kind,
            "a native allocated an instance of a class `new` could not produce (JVMS 6.5). \
             One row per class; the whole census also prints once at exit."
        );
    }
    true
}

/// One line on the exit path naming every class this process handed back that
/// `new` could not have produced, or nothing at all when there were none.
///
/// Not behind a debug flag, and not printed on an empty census — the two
/// decisions answer different questions. Unconditional when non-empty, because
/// a violation you have to know to ask for is how a run gets read without one.
/// Silent when empty, because the whole point of the change is that a
/// hello-world's stderr is empty, and because this census cannot certify a
/// clean run anyway: it sees the natives that route through
/// [`observe_uninstantiable_receiver`], so its ABSENCE is not a proof that
/// nothing else minted an abstract receiver.
///
/// Idempotent — `Once`-guarded — because two exit arms call it: the
/// `System.exit` shutdown trailer in `native-builtins::lang_system`, and
/// `vm-cli`'s normal-return arm, which is the one a program returning from
/// `main` takes instead.
///
/// `eprintln!` rather than `tracing::warn!`, matching the sibling censuses: a
/// report that only appears when the subscriber's filter allows it is a report
/// that a `RUST_LOG=error` run silently loses, and this one is the only place
/// the species is stated on a default run.
pub fn exit_summary() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let Some(seen) = SEEN.get() else {
        return;
    };
    let rows: Vec<UninstantiableRow> = seen.lock().clone();
    if rows.is_empty() {
        return;
    }
    ONCE.call_once(|| {
        let mut listed = String::new();
        for (name, kind, requester) in &rows {
            if !listed.is_empty() {
                listed.push_str("; ");
            }
            listed.push_str(name);
            listed.push_str(" (");
            listed.push_str(kind);
            listed.push_str(", requester=");
            listed.push_str(requester);
            listed.push(')');
        }
        eprintln!(
            "[cratonvm] JVMS 6.5 uninstantiable-receiver census: {} class(es): {listed} \
             -- a native handed back an instance of a class `new` could not have produced \
             (JVMS 6.5 makes `new` on an INTERFACE or an ABSTRACT class an \
             InstantiationError, so no bytecode in any image could have produced these \
             receivers). The definition-of-done screen's compatibility_classes counts \
             classes MINTED and cannot see this species: the allocation is against a class \
             that is perfectly real. One row per class, first-seen order; \
             CRATONVM_DBG_LAYOUT_ALIAS=1 reports each row as it happens instead. THIS \
             COUNTS THE NATIVES THAT ROUTE THROUGH observe_uninstantiable_receiver: the \
             absence of this line is not a clean run.",
            rows.len()
        );
    });
}
