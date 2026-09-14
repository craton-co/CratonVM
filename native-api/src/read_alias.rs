// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The **read-side** half of the slot-index census: a native reading slot `k`
//! of a real JDK object it did not allocate, where slot `k` on the loaded class
//! means a different field.
//!
//! # The species, and why [`crate::layout_alias`] cannot express it
//!
//! W7-59-layout-detector-coverage.md §6 states the split, and it is the reason
//! this module is a separate instrument rather than a wider one:
//!
//! * **Allocation width** — a native asking for `N` slots on a class declaring
//!   `M`. One integer against another. That is [`crate::layout_alias`], and
//!   since 2026-08-12 it sits on the base allocator, so it sees every native
//!   object allocation in the workspace.
//! * **Read-side wrong field** — `ctx.get_field(this, 0)` on a receiver this
//!   native never allocated. Three independent reasons the allocation
//!   instrument misses it, each sufficient: it is an *allocation* instrument
//!   and no allocation happens on that path; its whole vocabulary is a slot
//!   *count*, so it cannot say "slot 0 is `mark`, not `hb`"; and its documented
//!   fallback discriminator — intersect with the `cratonvm::gc::guard`
//!   out-of-bounds reads — misses too, because **slot 0 exists**. An in-bounds
//!   read of the wrong field is invisible to both halves of that intersection.
//!
//! The worked example is `bb_state` in `native-io`, repaired on 2026-08-12 by
//! W7-58-bytebuffer-direct-arm.md: it read slot 0 of a real
//! `java.nio.DirectByteBuffer` expecting the backing array `hb`, got
//! `java.nio.Buffer.mark = -1`, and reported "missing backing array". The real
//! JDK 25 layout — `javap -p` against Eclipse Adoptium 25.0.3.9, superclass
//! first, declaration order within a class, `static` excluded — is
//!
//! ```text
//!   0 mark   1 position   2 limit   3 capacity   4 address   5 segment    (java.nio.Buffer)
//!   6 hb     7 offset     8 isReadOnly   9 bigEndian   10 nativeByteOrder (java.nio.ByteBuffer)
//! ```
//!
//! so the in-place comment claiming "`hb` @ 5" was wrong twice over: 5 is
//! `segment` and `hb` is 6. That is the vocabulary this module adds — a slot
//! index checked against the **named** field at that index in the loaded class.
//!
//! # What is checkable, and when — the design decision
//!
//! W7-59 §6 sketched "checked once at **registration** against
//! `declared_fields` for the real class". That shape was evaluated and
//! **rejected as the primary check** for three reasons, all of which are
//! properties of this tree rather than matters of taste:
//!
//! 1. **At registration time the class is usually not loaded, and "not loaded"
//!    is indistinguishable from "no fields".** `vm_init` loads 323 named
//!    bootstrap classes and *then* registers natives, so a registration-time
//!    check can see `java/nio/Buffer` and `java/nio/ByteBuffer` — but
//!    `java.nio.DirectByteBuffer`, the receiver in the calibration case, is
//!    package-private, is **not** in that list, and is loaded on demand. Its
//!    `declared_fields` would come back empty and the check would report
//!    nothing. That is the same `declared == 0` overload
//!    [`crate::layout_alias`] already records as *unmeasured, not cleared* —
//!    inherited and made worse, because at registration time it covers most of
//!    the population instead of a corner of it.
//! 2. **The registered class is not the receiver's class.** A native
//!    registered on `java/nio/ByteBuffer` is entered with a `HeapByteBuffer`,
//!    a `DirectByteBuffer`, or a `ByteBufferAsIntBufferL`. Inherited slots are
//!    stable, so checking the registered class is *sound but partial*: it
//!    cannot see any slot past the registered class's own width, which is
//!    exactly where `DirectByteBuffer.cleaner` (11) and `att` (12) live. And a
//!    native registered on an interface has no fields to check against at all.
//! 3. **Registration is last-write-wins, so a registration-time check reports
//!    dead code as a defect.** A triple registered and then overwritten never
//!    runs; W7-59 spent most of §5.3 on exactly that distinction and settled it
//!    with "did the path run", not "does the site exist". A receiver-keyed
//!    runtime observation answers the LIVE question directly, which is the same
//!    argument [`crate::layout_alias::AllocSite::Java`] makes for reporting
//!    Java frames instead of a `#[track_caller]` Rust location.
//!
//! **The trade-off, stated rather than hidden.** Registration-time is free at
//! steady state and needs no workload; the runtime check costs a branch on a
//! read path that is genuinely hot (the per-element buffer accessors call it
//! once per element moved), and it reports only what a given run executed, so
//! its census is a **lower bound** that depends on the workload. This module
//! takes the runtime check as primary and pays for the workload dependence with
//! a second, free entry point:
//!
//! * [`observe_read`] — receiver-keyed, at the read, deduped, gated on the
//!   existing `CRATONVM_DBG_LAYOUT_ALIAS` flag. Answers "this path ran and read
//!   the wrong field".
//! * [`declare_slot_map`] + [`verify_declared_slot_maps`] — a native publishes
//!   its `const F_x: usize = k` table as a machine-readable `(slot, field
//!   name)` list, and the whole table is checked against the loaded class in
//!   one sweep. W7-59 §6's complaint was that those constants have "no
//!   machine-readable link to a field name"; [`SlotMap`] is that link. The
//!   sweep is where the registration-time idea survives, moved to a point where
//!   the classes are actually loaded and with `Unknown` kept distinct from
//!   clean. [`sweep_declared_slot_maps_at`] is the wrapper the trigger points
//!   call; W7-90-slot-map-sweep-caller.md says where they are and why. Until
//!   that lane the sweep had **no caller at all**, which is indistinguishable
//!   from a detector reporting all-clear — this campaign's dominant species,
//!   sitting inside the instrument built to detect it.
//!
//! Both funnel into one classifier and one emitter, for the same reason
//! [`crate::layout_alias`] has two observation points and one implementation:
//! two implementations of one primitive drift and then disagree.
//!
//! # Observation-only, and free when off
//!
//! Every public entry point checks [`crate::layout_alias::enabled()`] — a
//! `OnceLock<bool>` — **first**, and returns before touching a class, a name or
//! a lock. With the flag off the cost is one relaxed load and one predictable
//! branch. Nothing here returns a value a caller could act on except an
//! `Option<ReadFinding>` that exists so a test can tell "clean" from "not
//! looking"; the inserted blocks have no `else`. Compatible mode (`--real-jdk`)
//! is untouched by construction: this module can only print.
//!
//! **No new flag.** `CRATONVM_DBG_LAYOUT_ALIAS` is reused deliberately — the
//! two censuses are two halves of one species and a reader turning on "the slot
//! census" should get both. A new `CRATONVM_*` name would also need four files
//! (`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
//! `docs/flag-tokens.md`, `docs/config/flag-inventory.md`) or
//! `cargo test -p cratonvm-types` goes red.
//!
//! # What this cannot see
//!
//! * A slot read on a class whose width is 0 — "not loaded" and "genuinely no
//!   instance fields" are still one answer, and both are [`SlotAnswer::Unknown`]:
//!   **unmeasured, not clean**. Inherited from the allocation detector.
//! * A read whose expected field name is not stated anywhere. A bare
//!   `ctx.get_field(this, 4)` with no `SlotMap` and no [`observe_read`] carries
//!   no intent, and no instrument can infer one. That population is what
//!   `native-api/tests/read_alias_coverage.rs`'s census **prints**, so it is a
//!   visible remainder rather than a silent one.
//! * Whether a wrong field is *harmful*. Slot 6 read as an `Int` when it holds
//!   an array reference is caught by the value tag on most paths; the census is
//!   a risk register until a row is confirmed against a workload.
//! * **A wrong-KIND read of the RIGHT field** — one layer further out again,
//!   and the first instance is recorded rather than theoretical.
//!   W7-83-segment-as-backing-array.md found `bb_resolve_heap_array`
//!   (`native-io`) and `s2_bb_arr` (`native-builtins`) reading
//!   `java.nio.Buffer.segment` at slot 5 and returning it as the backing
//!   `byte[]`. The slot index is right, the field name the site declares is
//!   right, the read is in bounds, and the value is a `MemorySegment` where an
//!   array is required. [`classify_read`] compares a name against a name, so it
//!   answers **clean** — correctly, and that clean row is a deliberate
//!   non-firing control (W7-69 §3), which is why the repair was a screen on the
//!   VALUE after the read rather than a change here. Nothing in this module can
//!   be widened to catch the species: it would need the field's DESCRIPTOR and
//!   the value's runtime kind, i.e. a third instrument. The screen that does
//!   catch it is `ctx.heap_kind_of(a) == ObjectKind::Array` at each of those two
//!   sites; `native-api/tests/read_alias_coverage.rs
//!   ::the_calibration_site_is_still_observed_before_its_own_read` is what pins
//!   the observation to stay above it.

use std::collections::HashSet;
use std::sync::OnceLock;

use parking_lot::Mutex;

use cratonvm_types::{ClassId, ObjectRef};

use crate::layout_alias;
use crate::registry::{FieldMetadata, NativeContext};

/// How a slot read disagrees with the loaded class.
///
/// Deliberately **not** the same enum as [`layout_alias::Direction`]. That one
/// is a direction on a count; this one names a field. Folding them together is
/// how a census ends up reporting a count where a name was needed, which is the
/// exact failure W7-59 §6 records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadFinding {
    /// The slot exists on the loaded class and holds a **different** field.
    /// This is the `bb_state` shape: in bounds, so the `gc::guard`
    /// out-of-bounds reads never fire, and silent.
    WrongField {
        /// The field the loaded class actually has at that index.
        actual: String,
    },
    /// The slot is past the loaded class's field block. The read is refused by
    /// `NativeContextImpl::get_field`'s bounds check (M4a), so this shows up as
    /// a wrong *answer* rather than corruption — but it is still a slot map
    /// that disagrees with the class.
    SlotAbsent {
        /// `class_num_total_fields` for the receiver's class.
        declared_width: usize,
    },
}

/// What the loaded class says lives at a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotAnswer {
    /// The class declares this instance field at that absolute index.
    Named(String),
    /// The class is loaded and the index is past its field block.
    Absent {
        /// `class_num_total_fields` for the class.
        width: usize,
    },
    /// **Unmeasured, not clean.** The class has no field metadata at all, which
    /// means either "not loaded yet" or "genuinely declares no instance fields"
    /// — one answer for two questions, the same overload
    /// [`layout_alias`] documents for `declared == 0`. Never reported.
    Unknown,
}

/// The metadata [`field_name_at`] needs, split out of [`NativeContext`] so the
/// slot→name walk is testable against a real superclass **chain**.
///
/// This is not decoration. [`NativeContext::declared_fields`] returns only the
/// fields a class declares *itself* (with absolute `slot_index`), so an
/// implementation that forgets to walk `superclass_of` answers
/// [`SlotAnswer::Absent`] for every inherited field — which on the calibration
/// case turns "slot 0 is `mark`, not `hb`" into "slot 0 does not exist", a
/// quieter and wrong verdict. `MockNativeContext::superclass_of` returns `None`
/// unconditionally, so a mock-only test cannot catch that; this trait lets one.
pub trait SlotOracle {
    /// Fields declared by this class itself, with absolute slot indices.
    fn declared_at(&self, class_id: ClassId) -> Vec<FieldMetadata>;
    /// The superclass, or `None` at `java/lang/Object`.
    fn super_of(&self, class_id: ClassId) -> Option<ClassId>;
    /// Total instance-field width including inherited fields; 0 when unknown.
    fn total_width(&self, class_id: ClassId) -> usize;
    /// Internal class name, for the census row.
    fn name_of(&self, class_id: ClassId) -> Option<String>;
}

impl<'ctx> SlotOracle for dyn NativeContext + 'ctx {
    fn declared_at(&self, class_id: ClassId) -> Vec<FieldMetadata> {
        self.declared_fields(class_id)
    }
    fn super_of(&self, class_id: ClassId) -> Option<ClassId> {
        self.superclass_of(class_id)
    }
    fn total_width(&self, class_id: ClassId) -> usize {
        self.class_num_total_fields(class_id)
    }
    fn name_of(&self, class_id: ClassId) -> Option<String> {
        self.class_name_of_id(class_id)
    }
}

/// Which field the loaded class has at absolute slot `slot`.
///
/// Walks `class_id` and every superclass, because `declared_fields` is
/// declared-only while `slot_index` is absolute — an inherited field is
/// reachable only from the class that declared it. The walk is depth-bounded so
/// a cyclic or self-referential hierarchy in a synthetic image cannot hang a
/// diagnostic.
///
/// Only ever called with the flag on.
///
/// Generic over the oracle rather than taking `&dyn SlotOracle`: `&dyn
/// NativeContext` does **not** coerce to `&dyn SlotOracle`, because
/// trait-object-to-trait-object is not an unsizing coercion (`Unsize<dyn Trait>`
/// needs a `Sized` source). `O: ?Sized` lets `dyn NativeContext` itself be the
/// type parameter, so the VM passes its context straight through and a test
/// passes a concrete chain oracle.
#[must_use]
pub fn field_name_at<O: SlotOracle + ?Sized>(
    oracle: &O,
    class_id: ClassId,
    slot: usize,
) -> SlotAnswer {
    let mut cursor = Some(class_id);
    let mut saw_any_field = false;
    // 256 is far past any real Java hierarchy; the bound is against a broken
    // synthetic image, not against Java.
    for _ in 0..256 {
        let Some(cid) = cursor else { break };
        for field in oracle.declared_at(cid) {
            if field.is_static {
                continue;
            }
            saw_any_field = true;
            if field.slot_index == slot {
                return SlotAnswer::Named(field.name);
            }
        }
        cursor = oracle.super_of(cid);
    }
    let width = oracle.total_width(class_id);
    if width == 0 && !saw_any_field {
        return SlotAnswer::Unknown;
    }
    SlotAnswer::Absent { width }
}

/// The rule, with no I/O and no global state.
///
/// Split out of [`observe_read`] for the same reason
/// [`layout_alias::classify`] is: so the rule is testable without a heap and so
/// both entry points provably apply the *same* rule instead of each open-coding
/// a comparison.
///
/// `None` is "nothing to say", and it covers [`SlotAnswer::Unknown`] — which is
/// unmeasured, **not** clean.
#[must_use]
pub fn classify_read(expected_field: &str, answer: &SlotAnswer) -> Option<ReadFinding> {
    match answer {
        SlotAnswer::Named(actual) if actual == expected_field => None,
        SlotAnswer::Named(actual) => Some(ReadFinding::WrongField {
            actual: actual.clone(),
        }),
        SlotAnswer::Absent { width } => Some(ReadFinding::SlotAbsent {
            declared_width: *width,
        }),
        SlotAnswer::Unknown => None,
    }
}

/// Global dedup, keyed on `(class, slot, expected, site)`.
///
/// The site is in the key for the same reason it is in the allocation
/// detector's: two natives making the same mistake on the same class are two
/// findings, and collapsing them would be the detector choosing to be quieter.
fn already_reported(key: (String, usize, String, String)) -> bool {
    static SEEN: OnceLock<Mutex<HashSet<(String, usize, String, String)>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    // The guard lives for exactly one `insert` and nothing is taken while it is
    // held, so `emit`'s `tracing::warn!` — which re-enters the VM through the
    // subscriber — runs with no lock held. Same discipline as `layout_alias`.
    let mut guard = seen.lock();
    !guard.insert(key)
}

/// One census row. Nothing is taken while the dedup lock is held, so the
/// `tracing::warn!` — which re-enters the VM through the subscriber — runs
/// unlocked.
fn emit(class: &str, slot: usize, expected: &str, finding: &ReadFinding, site: &str) {
    match finding {
        ReadFinding::WrongField { actual } => tracing::warn!(
            class = class,
            slot = slot,
            expected_field = expected,
            actual_field = %actual,
            direction = "wrong-field",
            site = site,
            "native read a slot of a real JDK object under its OWN slot map; the slot \
             exists on the loaded class but holds a different field, so the read is \
             perfectly in bounds, the gc::guard out-of-bounds reads never fire, and \
             the wrong value is returned silently (this is the bb_state shape: slot 0 \
             of a real java.nio.DirectByteBuffer is Buffer.mark = -1, not the backing \
             array hb, which is 6)"
        ),
        ReadFinding::SlotAbsent { declared_width } => tracing::warn!(
            class = class,
            slot = slot,
            expected_field = expected,
            real_fields = declared_width,
            direction = "absent-slot",
            site = site,
            "native read a slot PAST the loaded class's field block; get_field's \
             bounds check (M4a) refuses it, so this surfaces as a defaulted answer \
             rather than corruption, but the native's slot map is wider than the class"
        ),
    }
}

/// Record one **read** of `slot` on `obj` that the caller believes is the field
/// named `expected_field`.
///
/// Callers **must** gate on [`layout_alias::enabled()`] first when assembling
/// `site` costs anything; this re-checks, so a caller that forgets is merely
/// slow, not wrong.
///
/// Returns the finding, or `None` when there is nothing to say — the flag is
/// off, the slot holds the expected field, the class is unmeasured, or this
/// exact `(class, slot, expected, site)` has been reported before. Returned
/// rather than discarded so a caller (and a test) can tell "clean" from "not
/// looking", which is the distinction this whole species is about.
///
/// **Observation only.** The return value must not steer the read: a diagnostic
/// that changes behaviour is a behaviour change in Compatible mode, which is
/// contractually frozen.
///
/// **A WRITE through an aliased slot is the same finding and uses this same
/// call.** The question is what slot `k` MEANS on the loaded class, not which
/// direction the access goes; `site` is where the caller says which it was.
/// `native-io`'s `buf_set_mark` is the worked example — it stamps an `Int`
/// mark onto slot 4, which on a real `java.nio.Buffer` is `address`, and the
/// save/restore either side of it exists precisely because of that.
pub fn observe_read(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    slot: usize,
    expected_field: &str,
    site: &str,
) -> Option<ReadFinding> {
    if !layout_alias::enabled() {
        return None;
    }
    let class_id = ctx.class_id_of_object(obj);
    observe_read_on_class(ctx, class_id, slot, expected_field, site)
}

/// [`observe_read`] for a caller that holds the `ClassId` rather than an
/// instance — the sweep, and any native that resolved its receiver's class
/// already.
pub fn observe_read_on_class(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    slot: usize,
    expected_field: &str,
    site: &str,
) -> Option<ReadFinding> {
    if !layout_alias::enabled() {
        return None;
    }
    let answer = field_name_at(ctx, class_id, slot);
    let finding = classify_read(expected_field, &answer)?;
    // `ctx.class_name_of_id`, not `SlotOracle::name_of`: the two answer the
    // same thing (the blanket impl forwards), and calling the inherent
    // `NativeContext` method leaves no room for the two traits' methods to be
    // ambiguous at this call site.
    let class = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| format!("<class#{}>", class_id.as_u32()));
    if already_reported((
        class.clone(),
        slot,
        expected_field.to_string(),
        site.to_string(),
    )) {
        return None;
    }
    emit(&class, slot, expected_field, &finding, site);
    Some(finding)
}

/// A native's slot map, published as `(slot, field name)` pairs.
///
/// This is the machine-readable link W7-59 §6 says the
/// `const BB_FIELD_ARRAY: usize = 0` style does not have. Declaring one costs
/// nothing at runtime — it is `const` data — and buys two things a comment does
/// not: [`verify_declared_slot_maps`] can check the whole table against the
/// loaded class in one sweep, and
/// `native-api/tests/read_alias_coverage.rs` can count the slot maps that exist
/// against the literal-slot reads that do not have one.
pub struct SlotMap {
    /// Internal name of the class the map is written for, e.g.
    /// `java/nio/ByteBuffer`.
    pub class: &'static str,
    /// `(absolute slot index, the field the native believes is there)`, in the
    /// native's own numbering. A slot the native never touches is simply
    /// absent; the map does not have to be total.
    pub slots: &'static [(usize, &'static str)],
    /// Where the map is written, for the census row — e.g.
    /// `native-io/src/lib.rs BB_FIELD_*`.
    pub origin: &'static str,
}

fn declarations() -> &'static Mutex<Vec<&'static SlotMap>> {
    static MAPS: OnceLock<Mutex<Vec<&'static SlotMap>>> = OnceLock::new();
    MAPS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Publish a slot map. Idempotent by pointer, so a registrar called twice does
/// not double the sweep.
///
/// Unconditional — **not** gated on the flag. The list is `&'static` pointers
/// and is pushed once per registrar per process; gating it would mean a run
/// that turns the flag on later has nothing to sweep.
pub fn declare_slot_map(map: &'static SlotMap) {
    let mut guard = declarations().lock();
    if guard.iter().any(|m| std::ptr::eq(*m, map)) {
        return;
    }
    guard.push(map);
}

/// Every slot map published so far. For the sweep and for tests.
#[must_use]
pub fn declared_slot_maps() -> Vec<&'static SlotMap> {
    declarations().lock().clone()
}

/// What one run of [`verify_declared_slot_maps`] actually looked at.
///
/// A bare row count is the shape this campaign keeps buying: `0` reads as
/// "clean" when it usually means "swept nothing". Every field here exists to
/// keep one of the three zeroes distinguishable from the others —
///
/// * `ran == false` — the flag is off and **the sweep did not happen**;
/// * `maps == 0` — nothing was ever handed to [`declare_slot_map`], so the
///   sweep had no population at all (the vacuous green
///   `read_alias_coverage.rs`'s link 6 exists to catch);
/// * `unresolved > 0` — a published map names a class no loader has, so those
///   slots are **unmeasured**, not clean. That is the run-time twin of
///   [`SlotAnswer::Unknown`], and it is why the summary prints it beside
///   `rows`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// The sweep actually ran. `false` means the flag was off — every other
    /// field is then a structural zero and must not be read as a result.
    pub ran: bool,
    /// Slot maps published so far, i.e. the population the sweep had.
    pub maps: usize,
    /// Of those, the ones whose class the loader could name.
    pub resolved: usize,
    /// Of those, the ones whose class no loader has (or several do,
    /// ambiguously). **Unmeasured, not clean.**
    pub unresolved: usize,
    /// Slots actually checked, across the resolved maps.
    pub slots: usize,
    /// Census rows emitted. Suppressed duplicates are not counted, so a second
    /// sweep in the same process reports `0` — see `already_reported`.
    pub rows: usize,
}

impl SweepReport {
    /// One line, for the trigger points to print.
    ///
    /// Deliberately carries the denominator. `rows=0 unresolved=7` and
    /// `rows=0 unresolved=0 slots=29` are opposite findings, and a line that
    /// printed only the first number would render them identical.
    #[must_use]
    pub fn summary_line(&self) -> String {
        if !self.ran {
            return "did not run (the layout-alias debug flag is off)".to_string();
        }
        format!(
            "maps={} resolved={} unresolved={} slots={} rows={} \
             (unresolved maps name a class no loader has: UNMEASURED, not clean)",
            self.maps, self.resolved, self.unresolved, self.slots, self.rows
        )
    }
}

/// Sweep every published [`SlotMap`] against the loaded class, and report the
/// slots that disagree.
///
/// This is where W7-59 §6's registration-time idea survives — moved off
/// registration, where the class is usually not loaded, to any point the caller
/// chooses. Call it after a workload has touched the classes, or from a
/// debug-only VM hook; a class that is still unloaded answers
/// [`SlotAnswer::Unknown`] and is skipped as **unmeasured**, not counted clean.
///
/// Returns a [`SweepReport`] rather than a bare count, so a caller can tell
/// "clean" from "not looking" from "the flag is off" — three states a `usize`
/// return collapses into the same `0`. [`sweep_declared_slot_maps_at`] is the
/// wrapper the trigger points use; it prints the summary.
pub fn verify_declared_slot_maps(ctx: &dyn NativeContext) -> SweepReport {
    if !layout_alias::enabled() {
        // `ran: false`, and every other field a structural zero. The caller
        // must not read that as "seven maps agree with their classes".
        return SweepReport::default();
    }
    let mut report = SweepReport {
        ran: true,
        ..SweepReport::default()
    };
    for map in declared_slot_maps() {
        report.maps += 1;
        // `class_id_by_name` returning `None` is two answers — "no loader has
        // it" and "several do, ambiguously". The sweep gives up on both, which
        // is what that method's own doc says a caller that will not act on a
        // miss should do; a diagnostic must never load or fabricate a class.
        let Some(class_id) = ctx.class_id_by_name(map.class) else {
            report.unresolved += 1;
            continue;
        };
        report.resolved += 1;
        for (slot, field) in map.slots {
            report.slots += 1;
            if observe_read_on_class(ctx, class_id, *slot, field, map.origin).is_some() {
                report.rows += 1;
            }
        }
    }
    report
}

/// [`verify_declared_slot_maps`] plus the one-line summary — the entry point
/// the trigger points call. `trigger` names where the sweep fired, e.g.
/// `main-returned`.
///
/// The summary goes to stderr rather than through `tracing`, and that is not a
/// style choice: the `System.exit` trigger runs microseconds before
/// `std::process::exit`, which does not unwind and does not flush a `tracing`
/// subscriber — `lang_system.rs`'s own `System.exit(N) called` line records the
/// same reason for the same placement. The per-slot rows still go through the
/// module's one emitter, so a run that exits hard can print the summary and
/// lose the rows; that limit is stated in W7-90-slot-map-sweep-caller.md rather
/// than papered over with a second emitter, because two detectors on one
/// primitive drift, then disagree, and then the reader has to pick.
///
/// **Observation only.** Nothing consumes the return value on any production
/// path; it exists so a caller and a test can tell "clean" from "not looking".
pub fn sweep_declared_slot_maps_at(ctx: &dyn NativeContext, trigger: &str) -> SweepReport {
    let report = verify_declared_slot_maps(ctx);
    if report.ran {
        eprintln!(
            "[read-alias] declared slot-map sweep at {trigger}: {}",
            report.summary_line()
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot oracle with a real superclass chain — which
    /// `MockNativeContext` cannot provide, because its `superclass_of` returns
    /// `None` unconditionally.
    struct ChainOracle {
        /// `(class name, superclass index or usize::MAX, own fields as
        /// (absolute slot, name))`
        classes: Vec<(&'static str, usize, Vec<(usize, &'static str)>)>,
    }

    impl SlotOracle for ChainOracle {
        fn declared_at(&self, class_id: ClassId) -> Vec<FieldMetadata> {
            let idx = class_id.as_u32() as usize;
            self.classes
                .get(idx)
                .map(|(_, _, fields)| {
                    fields
                        .iter()
                        .map(|(slot, name)| FieldMetadata {
                            name: (*name).to_string(),
                            descriptor: "I".to_string(),
                            access_flags: 0,
                            slot_index: *slot,
                            declaring_class_id: class_id,
                            is_static: false,
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        fn super_of(&self, class_id: ClassId) -> Option<ClassId> {
            let idx = class_id.as_u32() as usize;
            match self.classes.get(idx) {
                Some((_, s, _)) if *s != usize::MAX => Some(ClassId::new(*s as u32)),
                _ => None,
            }
        }
        fn total_width(&self, class_id: ClassId) -> usize {
            let mut cursor = Some(class_id);
            let mut width = 0usize;
            while let Some(cid) = cursor {
                let idx = cid.as_u32() as usize;
                let Some((_, _, fields)) = self.classes.get(idx) else {
                    break;
                };
                width += fields.len();
                cursor = self.super_of(cid);
            }
            width
        }
        fn name_of(&self, class_id: ClassId) -> Option<String> {
            self.classes
                .get(class_id.as_u32() as usize)
                .map(|(n, _, _)| (*n).to_string())
        }
    }

    /// The real JDK 25 `java.nio.DirectByteBuffer` chain, read with `javap -p`
    /// against Eclipse Adoptium 25.0.3.9 on this host: superclass first,
    /// declaration order within a class, `static` excluded.
    ///
    /// `Buffer` — mark, position, limit, capacity, address, segment.
    /// `ByteBuffer` — hb, offset, isReadOnly, bigEndian, nativeByteOrder.
    /// `MappedByteBuffer` — fd, isSync. `DirectByteBuffer` — cleaner, att.
    fn direct_byte_buffer_chain() -> (ChainOracle, ClassId) {
        let oracle = ChainOracle {
            classes: vec![
                ("java/lang/Object", usize::MAX, vec![]),
                (
                    "java/nio/Buffer",
                    0,
                    vec![
                        (0, "mark"),
                        (1, "position"),
                        (2, "limit"),
                        (3, "capacity"),
                        (4, "address"),
                        (5, "segment"),
                    ],
                ),
                (
                    "java/nio/ByteBuffer",
                    1,
                    vec![
                        (6, "hb"),
                        (7, "offset"),
                        (8, "isReadOnly"),
                        (9, "bigEndian"),
                        (10, "nativeByteOrder"),
                    ],
                ),
                (
                    "java/nio/MappedByteBuffer",
                    2,
                    vec![(11, "fd"), (12, "isSync")],
                ),
                (
                    "java/nio/DirectByteBuffer",
                    3,
                    vec![(13, "cleaner"), (14, "att")],
                ),
            ],
        };
        (oracle, ClassId::new(4))
    }

    /// THE CALIBRATION CASE. `bb_state`'s pre-2026-08-12 body read slot 0 of a
    /// real `java.nio.DirectByteBuffer` and called it `hb`. If the instrument
    /// does not name `mark` here, the widening did not work and every census
    /// this module produces is unsupported.
    ///
    /// The finding must be `WrongField`, **not** `SlotAbsent`: `mark` is
    /// declared four classes up the chain, so an oracle that fails to walk
    /// `superclass_of` answers "slot 0 does not exist" — quieter, and wrong.
    #[test]
    fn bb_state_reading_slot_0_of_a_real_direct_byte_buffer_as_hb_is_flagged() {
        let (oracle, dbb) = direct_byte_buffer_chain();
        let answer = field_name_at(&oracle, dbb, 0);
        assert_eq!(
            answer,
            SlotAnswer::Named("mark".to_string()),
            "slot 0 of a real DirectByteBuffer is java.nio.Buffer.mark; an oracle that \
             does not walk the superclass chain answers Absent here and the whole \
             instrument goes quiet on exactly the defect it was built for"
        );
        assert_eq!(
            classify_read("hb", &answer),
            Some(ReadFinding::WrongField {
                actual: "mark".to_string()
            })
        );
    }

    /// The other half of the historical comment: it said the fallback slot was
    /// "the real-JDK HeapByteBuffer slot (`hb` @ 5)". On JDK 25, 5 is
    /// `Buffer.segment` and `hb` is 6 — wrong twice over.
    #[test]
    fn the_historical_hb_at_5_comment_is_flagged_too() {
        let (oracle, dbb) = direct_byte_buffer_chain();
        assert_eq!(
            classify_read("hb", &field_name_at(&oracle, dbb, 5)),
            Some(ReadFinding::WrongField {
                actual: "segment".to_string()
            })
        );
        // And the corrected index is clean, so the instrument is not simply
        // "always fires" — the vacuous shape this project keeps re-buying.
        assert_eq!(classify_read("hb", &field_name_at(&oracle, dbb, 6)), None);
    }

    /// CratonVM's own `java/nio/ByteBuffer` slot map, from
    /// `native-io/src/lib.rs`'s `BB_FIELD_*` constants, checked slot by slot
    /// against the real layout. This is the first census row and it is
    /// derived, not asserted by hand.
    ///
    /// The name says three and the assertion says two, and both are right: the
    /// third disagreeing slot is 6, which `bb_resolve_heap_offset` reads and
    /// which the PUBLISHED map did not carry at all until W7-76 added it. It
    /// has its own test immediately below. The six replicated here are the six
    /// the map held when this was written; the name is left alone because it
    /// is the trail to W7-76-bytebuffer-alias-residuals.md, where the count is
    /// reconciled and the published map is made complete.
    #[test]
    fn the_cratonvm_byte_buffer_slot_map_disagrees_at_three_of_six_slots() {
        let (oracle, dbb) = direct_byte_buffer_chain();
        // (slot, what native-io believes is there)
        let craton: &[(usize, &str)] = &[
            (0, "hb"),       // BB_FIELD_ARRAY
            (1, "position"), // BB_FIELD_POS
            (2, "limit"),    // BB_FIELD_LIMIT
            (3, "capacity"), // BB_FIELD_CAPACITY
            (4, "mark"),     // BB_FIELD_MARK
            (5, "segment"),  // BB_SEGMENT_SLOT — correct, and for the stated reason
        ];
        let wrong: Vec<(usize, ReadFinding)> = craton
            .iter()
            .filter_map(|(slot, field)| {
                classify_read(field, &field_name_at(&oracle, dbb, *slot)).map(|f| (*slot, f))
            })
            .collect();
        assert_eq!(
            wrong,
            vec![
                (
                    0,
                    ReadFinding::WrongField {
                        actual: "mark".to_string()
                    }
                ),
                (
                    4,
                    ReadFinding::WrongField {
                        actual: "address".to_string()
                    }
                ),
            ],
            "slots 1, 2, 3 agree by accident of declaration order and slot 5 agrees on \
             purpose; 0 and 4 are the two the ByteBuffer lane found by hand"
        );
    }

    /// `bb_resolve_heap_offset`'s slot-6 fallback expects `offset`. On the real
    /// layout 6 is `hb` — an array reference where an `Int` is read.
    #[test]
    fn the_heap_offset_fallback_slot_names_hb_not_offset() {
        let (oracle, dbb) = direct_byte_buffer_chain();
        assert_eq!(
            classify_read("offset", &field_name_at(&oracle, dbb, 6)),
            Some(ReadFinding::WrongField {
                actual: "hb".to_string()
            })
        );
    }

    /// Past the end of the class is a different finding from the wrong field,
    /// and must stay different: the out-of-range one is the half the
    /// `gc::guard` out-of-bounds reads can already see.
    #[test]
    fn a_slot_past_the_class_is_absent_not_wrong() {
        let (oracle, dbb) = direct_byte_buffer_chain();
        assert_eq!(
            field_name_at(&oracle, dbb, 99),
            SlotAnswer::Absent { width: 15 }
        );
        assert_eq!(
            classify_read("whatever", &SlotAnswer::Absent { width: 15 }),
            Some(ReadFinding::SlotAbsent { declared_width: 15 })
        );
    }

    /// A class with no field metadata is UNMEASURED, not clean — the same
    /// discipline `layout_alias` applies to `declared == 0`. Reporting it would
    /// make every not-yet-loaded class a finding, which is precisely how the
    /// registration-time design fails.
    #[test]
    fn an_unloaded_class_is_unknown_and_never_reported() {
        let oracle = ChainOracle {
            classes: vec![("java/nio/DirectByteBuffer", usize::MAX, vec![])],
        };
        assert_eq!(
            field_name_at(&oracle, ClassId::new(0), 0),
            SlotAnswer::Unknown
        );
        assert_eq!(classify_read("hb", &SlotAnswer::Unknown), None);
    }

    /// The agreement case answers `None`, so a clean slot map is silent.
    #[test]
    fn agreement_is_not_a_finding() {
        let (oracle, dbb) = direct_byte_buffer_chain();
        for (slot, name) in [
            (1, "position"),
            (2, "limit"),
            (3, "capacity"),
            (13, "cleaner"),
        ] {
            assert_eq!(
                classify_read(name, &field_name_at(&oracle, dbb, slot)),
                None,
                "slot {slot} really is {name}"
            );
        }
    }

    /// A published map is idempotent by pointer and readable back, so the sweep
    /// cannot double-count a registrar that runs twice.
    #[test]
    fn declaring_a_slot_map_twice_publishes_it_once() {
        static MAP: SlotMap = SlotMap {
            class: "java/nio/ByteBuffer",
            slots: &[(0, "hb")],
            origin: "read_alias unit test",
        };
        declare_slot_map(&MAP);
        declare_slot_map(&MAP);
        let count = declared_slot_maps()
            .iter()
            .filter(|m| std::ptr::eq(**m, &MAP))
            .count();
        assert_eq!(count, 1);
    }
}
