// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The layout-alias census: **one** detector, sitting on the allocation every
//! native object in the workspace actually reaches.
//!
//! # The species
//!
//! `docs/architecture/natives-over-real-jdk-classes.md` §5: a Rust native
//! writes a field of a real JDK object by slot index computed from CratonVM's
//! own idea of the layout. When the loaded class has a different layout the
//! write lands on the wrong field, or past the end of the object — which §5
//! calls heap corruption rather than a wrong answer. The one place that
//! intention is machine-readable is the **allocation**: a caller asking for `N`
//! slots on a class that declares `M` is stating, in an integer, that it holds
//! a slot map of a different width from the class.
//!
//! # Why this module exists, and where it came from
//!
//! Until 2026-08-12 the whole detector was `report_layout_alias` inside
//! `native-builtins/src/util_concurrent_ext.rs`, called from exactly one place:
//! `try_alloc_concurrent_synthetic`, the crate's fabrication funnel. That funnel
//! is busy (~1,800 call sites) but it is **not the only allocator**.
//! W7-49-slot-index-recensus.md measured the gap: `native-builtins`,
//! `native-io` and `native-collections` also hold direct
//! `alloc_object` / `try_alloc_object_gc_safe` call sites that never pass
//! through it, so no request-versus-declared comparison was made for any of
//! them and `CRATONVM_DBG_LAYOUT_ALIAS=1` could not name one.
//!
//! That was not a hypothetical hole. `native-io/src/async_socket.rs` allocates
//! `java/nio/channels/AsynchronousSocketChannel` four slots wide against a class
//! declaring one, and `native-io` is the live owner of that class — so the
//! widest live over-allocation in the workspace was invisible to the instrument
//! built to find over-allocations. An instrument that reports clean because it
//! cannot see is the failure mode this campaign keeps re-buying; the lanes
//! downstream then read that clean report as coverage.
//!
//! # One detector, two observation points, and why that is still one funnel
//!
//! The counting, the flag, the dedup key and the output channel all live
//! **here**, once. Two places call in:
//!
//! 1. `NativeContextImpl::alloc_object` in `vm/src/vm/vm_exec.rs` — the terminal
//!    every native object allocation in every native crate reaches, including
//!    the fabrication funnel's own. It already computes the loaded class's
//!    `num_total_fields` (it has clamped the requested count up to it since the
//!    Kafka `HashSet` failure), so the comparison this census needs is two
//!    integers that are *already in registers* at that point. Nothing is looked
//!    up for the detector's sake.
//! 2. `try_alloc_concurrent_synthetic` in `native-builtins` — kept, and
//!    load-bearing, because that funnel **clamps before it allocates**:
//!    `n = requested.max(real)`. By the time an under-request reaches (1) it has
//!    become `n == real` and there is nothing left to see. Removing this call
//!    would make the detector quieter in the `under` direction, which is the
//!    direction it has had since it was written. Louder is always allowed;
//!    quieter never is.
//!
//! Two callers into one implementation is not two detectors. The rule this
//! project paid for — *one sick collector ⇒ diff the two impls of the same
//! primitive* — is about two implementations of one primitive drifting apart.
//! There is one implementation here, and the funnel's copy has been deleted
//! rather than left beside it.
//!
//! # Observation-only, and free when off
//!
//! [`enabled()`] is a `OnceLock<bool>` read that every caller checks **before**
//! it assembles anything. With the flag off the added cost on the allocation
//! path is one relaxed load and one predictable branch; no class name is
//! resolved, no frame is formatted, no lock is taken. Nothing about the
//! allocation changes in either mode, with the flag on or off: the clamp, the
//! slot count, the TLAB path and the returned object are byte-for-byte what
//! they were. Compatible mode is untouched by construction — this module can
//! only print.
//!
//! # `declared == 0` — the short-object case, reported since 2026-08-12
//!
//! Until W7-73-short-object-blind-spot.md this module returned `None` for
//! `declared == 0` and said so in a header paragraph titled *"what it still
//! cannot see"*. That paragraph was accurate and the behaviour was still wrong,
//! because a `None` and a clean run are the same absence to every consumer.
//!
//! The reasoning that forced the change is structural, not statistical.
//! `NativeContextImpl::alloc_object` clamps `slots = num_fields.max(real_fields)`
//! **two lines after** it calls this module. So:
//!
//! * whenever `declared > 0`, a [`Direction::Under`] row describes an object
//!   the clamp has already widened to its full declared width — a *mis-request*,
//!   never a short object;
//! * whenever `declared == 0` the clamp is the identity (`n.max(0) == n`), the
//!   object really is exactly `requested` wide, and the old `None` meant the one
//!   case that CAN be short was the one case reported as nothing.
//!
//! [`Direction::Undeclared`] is that case, and it is deliberately not folded
//! into `Over`. `declared == 0` is overloaded three ways and the instrument
//! cannot tell them apart from here:
//!
//! 1. **Genuinely field-less** — an interface, `java/lang/Object`, a marker
//!    class. A non-zero request is intended fabrication and the object is not
//!    short by any definition. Common and benign.
//! 2. **Not registered** — `get_class(class_id)` answered `None`. A stale id, or
//!    a class observed mid-registration (which is exactly why the base
//!    allocator refuses to cache a zero).
//! 3. **A fabricated stub, or the `ClassId::new(0)` fallback arm** — the class
//!    the caller *named* has a real layout wider than the request, and nothing
//!    clamped. **This object is short.**
//!
//! Reporting all three as `undeclared` is the honest answer: the row says "an
//! allocation this instrument cannot adjudicate happened here", which is what a
//! reader can act on. Silence said "clean", which is what a reader believed.
//! A consumer that only wants the pre-2026-08-12 census filters on
//! `direction in (under, over)`; the field names and channel are unchanged.
//!
//! # And the `ClassId::new(0)` sentinel does NOT reach this module
//!
//! Worth stating because it is the natural next guess and it is wrong.
//! `alloc_object` intercepts `class_id == ClassId::new(0) && num_fields > 0`
//! *before* it resolves any field count and substitutes
//! `cratonvm/synthetic/AnonymousObject$N`, which declares exactly `N`. So the
//! sentinel arrives at [`classify`] as `classify(n, n)` — agreement — not as
//! `declared == 0`. The width species genuinely has nothing to say about it, and
//! that is why the base allocator observes the sentinel **before** the
//! substitution, with [`UNRESOLVED_CLASS`] as the class name and `declared = 0`.
//! Without that the busiest blind-spot population in the workspace — 30
//! production sites, 16 of them requesting fewer slots than the class they name
//! really declares — is invisible in both directions at once.

use std::collections::HashSet;
use std::panic::Location;
use std::sync::OnceLock;

/// Which way the request disagrees with the loaded class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The caller asked for FEWER slots than the class declares. The allocator
    /// clamps up, so these writes alias the real class's own fields, and any
    /// native reading a wider layout for this class reads past the object.
    Under,
    /// The caller asked for MORE slots than the class declares. The object
    /// carries more slots than its class has fields, so its header disagrees
    /// with `num_total_fields` (what `CRATONVM_DBG_VALIDATE_NEW` prints as
    /// `BAD`), and the caller's slot map has entries the class does not —
    /// applied to any instance this site did not allocate (real bytecode `new`,
    /// or the JIT) those indices write past the object.
    Over,
    /// The caller asked for slots against a class that declares **none**, so
    /// there was nothing to compare and nothing to clamp against.
    ///
    /// This is the only direction in which a **short object** can exist, and it
    /// is reported precisely because it cannot be adjudicated here. See the
    /// module header: `declared == 0` means "interface / field-less" (benign),
    /// "not registered" (a stale or mid-registration id), or "a fabricated stub
    /// standing in for a class with a wider real layout" (short). The row says
    /// which allocation, not which of the three — that is the reader's next
    /// step, and before 2026-08-12 there was no row to take it from.
    ///
    /// It is deliberately NOT `Over`. An `over` row asserts the object has more
    /// slots than its class has fields, which is a claim about a known layout;
    /// here there is no known layout to make a claim about.
    Undeclared,
}

/// The class-name placeholder for an allocation whose `ClassId` did not resolve.
///
/// `alloc_object` substitutes `cratonvm/synthetic/AnonymousObject$N` for
/// `ClassId::new(0)` before it looks anything up, so by the time a name is
/// available it is the *substitute's* name and the row would read as a clean
/// exact-width allocation of a class nobody asked for. Reporting the sentinel
/// under this literal keeps the row greppable and keeps it from being mistaken
/// for a real `java/lang/Object` allocation (which is a legitimate, field-less,
/// zero-slot thing that happens constantly).
pub const UNRESOLVED_CLASS: &str = "<unresolved:ClassId(0)>";

/// The classification, with no I/O and no global state.
///
/// Split out from [`observe`] so the rule itself is testable without a heap, a
/// class manager or a `tracing` subscriber, and so the two observation points
/// provably apply the *same* rule rather than each open-coding `!=`.
///
/// `None` is "nothing to say", and after 2026-08-12 it means exactly two
/// things, both of which really are nothing:
///
/// * `requested == 0` — the caller is not asserting a layout at all, so there
///   is no slot map to be wrong about;
/// * `requested == declared` — agreement.
///
/// `declared == 0` used to be a third `None` and is now
/// [`Direction::Undeclared`]. That is the whole of W7-73-short-object-blind-spot.md:
/// it is the ONLY case in which the base allocator's
/// `slots = num_fields.max(real_fields)` clamp does nothing, hence the only case
/// in which the allocated object can be narrower than the class it is handed out
/// as. Reporting it as `None` made the instrument silent about the one thing it
/// was named after.
#[must_use]
#[inline]
pub fn classify(requested: usize, declared: usize) -> Option<Direction> {
    if requested == 0 {
        return None;
    }
    if declared == 0 {
        // Deliberately BEFORE the `requested == declared` test, which would
        // otherwise swallow `classify(0, 0)` — already handled above — and,
        // more to the point, keeps this arm from ever being reachable-by-
        // accident-only. It is the reported case, not the leftover one.
        return Some(Direction::Undeclared);
    }
    if requested == declared {
        return None;
    }
    Some(if requested < declared {
        Direction::Under
    } else {
        Direction::Over
    })
}

/// Is the census switched on?
///
/// OFF by default, and deliberately so. Measured over a 30-class random sample
/// of the real Tomcat suite the funnel half alone fired for 49 distinct JDK
/// classes from 75 call sites, and the SAME runs produced zero out-of-bounds
/// field reads — so the shape is pervasive and, on that corpus, harmless: the
/// clamp keeps every access in bounds, and no second native reads a wider layout
/// for any of those classes. An always-on warning would be boot noise for a risk
/// register, not a bug list.
///
/// It becomes a BUG when a class has two layouts and someone reads the wider
/// one. That is `java.lang.Process`, and the discriminator is cheap: a class
/// appearing in BOTH this census and the `cratonvm::gc::guard` out-of-bounds
/// reads has a live defect. Turn this on, run the failing workload, and
/// intersect the two lists.
///
/// **An intersection is only as complete as its smallest list.** That sentence
/// is why this module exists: while the census covered one funnel, a class
/// allocated wide by a direct `alloc_object` appeared in the guard list with no
/// census row to intersect it against, which reads as "not this species" when it
/// is exactly this species.
///
/// Making it fatal, or even default-on, needs a count the `over` direction still
/// does not have: run the real suites with this flag on, and intersect the
/// distinct `(class, site)` pairs against `CRATONVM_DBG_VALIDATE_NEW=1`'s
/// `[young-validate] BAD` lines and the `cratonvm::gc::guard` out-of-bounds
/// reads. A pair in this census with no guard hit is wide-but-unused and can be
/// narrowed at the call site; a pair in both is a live defect. Turning it fatal
/// before that count exists would convert an unknown number of working call
/// sites into `NoClassDefFoundError` at boot.
#[must_use]
#[inline]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LAYOUT_ALIAS").is_some()
    })
}

/// Where the request came from, for the census row.
///
/// The two observation points can afford different answers and neither is
/// forced to pay for the other's.
pub enum AllocSite<'a> {
    /// A Rust source location, exact. Used by the fabrication funnel, which is
    /// already `#[track_caller]` for the class-origin census, so the location is
    /// in hand and costs nothing new.
    Rust(&'static Location<'static>),
    /// The Java frames that entered the native, formatted by the caller.
    ///
    /// Used by the base allocator, which is hot enough that adding
    /// `#[track_caller]` to `NativeContext::alloc_object` — a hidden argument on
    /// every native allocation plus a reify shim on the `dyn` vtable — is a real
    /// cost paid in every run to serve a flag that is off in almost all of them.
    /// The Java frame is already a `Vec` on the thread and is formatted only
    /// when a row is about to be printed.
    ///
    /// It is also the better answer for the question the census actually asks.
    /// A Rust source location says a site exists; a Java frame says the path
    /// **ran**, which is the LIVE-versus-dead distinction the whole census turns
    /// on, and which registration being last-write-wins makes impossible to
    /// settle from source alone.
    Java(&'a str),
}

impl AllocSite<'_> {
    fn as_key(&self) -> String {
        match self {
            AllocSite::Rust(loc) => format!("{loc}"),
            AllocSite::Java(frames) => (*frames).to_string(),
        }
    }
}

/// Record one allocation whose requested width disagrees with its loaded class.
///
/// Callers **must** gate on [`enabled()`] first; this re-checks (so a future
/// caller that forgets is merely slow, not wrong) but the `&str` arguments are
/// the expensive part and only the caller can avoid building them.
///
/// Returns the direction reported, or `None` when the row was suppressed —
/// either the flag is off, the counts agree, `declared == 0`, or this exact
/// `(class, requested, declared, site)` has been reported before. Returned
/// rather than discarded so a caller (and a test) can tell "clean" from "not
/// looking", which is the distinction this whole lane is about.
///
/// # Deduplication
///
/// Keyed on `(class, requested, declared, site)`. The site is **in** the key on
/// purpose: two different natives asking for the same wrong width on the same
/// class are two findings, and collapsing them to one would be the detector
/// choosing to be quieter. A hot allocation loop still reports once, because a
/// loop is one site.
pub fn observe(
    class_name: &str,
    requested: usize,
    declared: usize,
    site: AllocSite<'_>,
) -> Option<Direction> {
    if !enabled() {
        return None;
    }
    let direction = classify(requested, declared)?;

    // A plain `parking_lot::Mutex`, matching this crate (`capability.rs`,
    // `fd_table.rs`); the ordered wrappers are `native-builtins`' ratchet and do
    // not apply here. The guard lives for exactly one `insert` and nothing is
    // taken while it is held, so the `tracing::warn!` below — which re-enters
    // the VM through the subscriber — runs with no lock held.
    static SEEN: OnceLock<parking_lot::Mutex<HashSet<(String, usize, usize, String)>>> =
        OnceLock::new();
    let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(HashSet::new()));
    let key = (class_name.to_string(), requested, declared, site.as_key());
    let site_text = key.3.clone();
    {
        let mut guard = seen.lock();
        if !guard.insert(key) {
            return None;
        }
    }

    // One channel, one flag, one dedup key, two directions. The `direction`
    // field is what makes the census sortable — a consumer that only wants the
    // corruption-shaped half filters on `over`, and the `under` rows keep the
    // meaning they have had since the funnel-only days, unchanged.
    match direction {
        Direction::Under => tracing::warn!(
            class = class_name,
            requested_fields = requested,
            real_fields = declared,
            direction = "under",
            site = %site_text,
            "native allocated a class under its own SMALLER field layout; the slot \
             count is clamped up to the real one, so these writes alias the real \
             class's own fields and any native reading a wider layout for this \
             class reads past the object"
        ),
        Direction::Over => tracing::warn!(
            class = class_name,
            requested_fields = requested,
            real_fields = declared,
            direction = "over",
            site = %site_text,
            "native allocated a class under its own WIDER field layout; the object \
             carries more slots than its class declares fields, so its header \
             disagrees with num_total_fields (what CRATONVM_DBG_VALIDATE_NEW calls \
             BAD), and the caller's slot map has entries the class does not -- \
             applied to any instance this site did not allocate (real bytecode new, \
             or the JIT) those indices write past the object"
        ),
        Direction::Undeclared => tracing::warn!(
            class = class_name,
            requested_fields = requested,
            real_fields = declared,
            direction = "undeclared",
            site = %site_text,
            "native allocated slots against a class declaring NONE, so the slot-count \
             clamp did nothing and this object is exactly requested_fields wide -- \
             this instrument cannot tell whether that is correct. real_fields=0 means \
             one of: the class is genuinely field-less (an interface, java/lang/Object \
             -- benign); its ClassId is not registered (stale, or observed \
             mid-registration); or it is a fabricated stub / the ClassId::new(0) \
             fallback arm standing in for a class whose real layout is WIDER, in which \
             case this object is SHORT and every real-bytecode read of a field past \
             requested_fields is out of bounds. Compare requested_fields against \
             `javap -p` for the class this site names. See \
             W7-73-short-object-blind-spot.md"
        ),
    }
    Some(direction)
}

/// [`observe`] for a caller that wants its own source location in the row.
///
/// `#[track_caller]` chains, so a `#[track_caller]` forwarding funnel reports
/// the native that asked for the shape rather than the funnel.
#[track_caller]
pub fn observe_from_rust(class_name: &str, requested: usize, declared: usize) -> Option<Direction> {
    observe(
        class_name,
        requested,
        declared,
        AllocSite::Rust(Location::caller()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule, stated against the case that motivated widening the detector.
    ///
    /// `java/nio/channels/AsynchronousSocketChannel` declares exactly one
    /// instance field (`provider`, transitively — its superclass is
    /// `java/lang/Object`, checked with `javap -p` against JDK 25.0.3.9), and
    /// `native-io/src/async_socket.rs` allocates it with `N_FIELDS = 4`. That is
    /// the widest live over-allocation W7-49-slot-index-recensus.md found, and
    /// the pre-2026-08-12 detector could not see it because that site is a
    /// direct `alloc_object` rather than the fabrication funnel.
    ///
    /// This asserts the *classification*, which is the only half a unit test can
    /// reach. That the site now arrives here at all is what
    /// `native-api/tests/layout_alias_coverage.rs` proves, and the two together
    /// are the claim — this one alone would be a probe that cannot fail.
    /// (The path was `vm/tests/layout_alias_detector_coverage.rs` here until
    /// 2026-08-12; no such file was ever committed. A pointer to a gate that
    /// does not exist reads exactly like a gate that does — corrected while
    /// reading this module for W7-68-live-under-allocations.md.)
    #[test]
    fn async_socket_channel_shape_classifies_as_over() {
        assert_eq!(classify(4, 1), Some(Direction::Over));
    }

    #[test]
    fn under_and_exact_and_unmeasured() {
        assert_eq!(
            classify(1, 3),
            Some(Direction::Under),
            "Kafka HashSet shape"
        );
        assert_eq!(classify(3, 3), None, "agreement is not a finding");
        assert_eq!(classify(0, 4), None, "caller asserts no layout");
    }

    /// The blind spot W7-73-short-object-blind-spot.md closed.
    ///
    /// This assertion was `assert_eq!(classify(4, 0), None, "class not loaded,
    /// or an interface")` until 2026-08-12, with a comment calling it
    /// "unmeasured, NOT clean". The comment was right and the return value was
    /// still read as clean by every consumer, because a suppressed row and an
    /// absent defect are the same bytes on the wire.
    ///
    /// `declared == 0` is the ONLY case the base allocator's
    /// `slots = num_fields.max(real_fields)` clamp leaves alone, so it is the
    /// only case in which the object can be narrower than the class it is handed
    /// out as. The three shapes that produce it are in the module header; this
    /// test asserts the classification, and
    /// `native-api/tests/layout_alias_coverage.rs` asserts that the sites
    /// producing it actually reach here.
    #[test]
    fn a_class_declaring_nothing_is_reported_not_swallowed() {
        assert_eq!(
            classify(6, 0),
            Some(Direction::Undeclared),
            "java/util/zip/ZipEntry's ClassId::new(0) fallback arm: 6 slots, no \
             declared layout to clamp against, real class declares 14"
        );
        assert_eq!(
            classify(5, 0),
            Some(Direction::Undeclared),
            "the java/lang/Thread mirror in vertx_eventloop.rs / xnio_io_thread.rs: \
             5 slots against a class declaring 19, and NOT on a fallback arm"
        );
        // Still nothing to say: the caller asserted no layout at all.
        assert_eq!(classify(0, 0), None, "no request, no claim");
    }

    /// The dedup key carries the site, so two natives making the same mistake on
    /// the same class are two rows. Collapsing them would be the detector
    /// choosing to be quieter, which this lane is not allowed to do.
    #[test]
    fn site_is_part_of_the_dedup_key() {
        let a = AllocSite::Java("Foo.bar").as_key();
        let b = AllocSite::Java("Baz.qux").as_key();
        assert_ne!(a, b);
    }
}
