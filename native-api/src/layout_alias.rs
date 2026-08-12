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
//! # What it still cannot see
//!
//! `declared == 0` is excluded from both directions and that exclusion is
//! inherited, not new. Zero is overloaded: it means "class not loaded yet" (the
//! reason the clamp's `max` exists at all) and it also means "genuinely no
//! instance fields" — every interface, and `java/lang/Object`. Natives are
//! routinely asked for interface names, where a non-zero request is the
//! intended fabrication and not an alias. Those allocations are **unmeasured by
//! this census, not cleared by it**. Closing that needs a `class_is_loaded`
//! predicate `NativeContext` does not have.

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
}

/// The classification, with no I/O and no global state.
///
/// Split out from [`observe`] so the rule itself is testable without a heap, a
/// class manager or a `tracing` subscriber, and so the two observation points
/// provably apply the *same* rule rather than each open-coding `!=`.
///
/// `None` is "nothing to say", and it covers two cases that must not be
/// conflated with "clean": a request of 0 (the caller is not asserting a
/// layout), and a declared count of 0 (see the module header — unmeasured).
#[must_use]
#[inline]
pub fn classify(requested: usize, declared: usize) -> Option<Direction> {
    if requested == 0 || declared == 0 || requested == declared {
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
    *ENABLED.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LAYOUT_ALIAS").is_some())
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
        assert_eq!(classify(1, 3), Some(Direction::Under), "Kafka HashSet shape");
        assert_eq!(classify(3, 3), None, "agreement is not a finding");
        // Both zero cases are "unmeasured", NOT "clean" — see the module header.
        assert_eq!(classify(4, 0), None, "class not loaded, or an interface");
        assert_eq!(classify(0, 4), None, "caller asserts no layout");
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
