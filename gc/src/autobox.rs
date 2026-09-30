// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The ONE implementation of "a non-reference `Value` was stored into a slot
//! the class declares as a reference".
//!
//! # Why this module exists
//!
//! Until 2026-08-12 the tree had **four** answers to that one question, one per
//! field-store implementation, and which one a program got depended on the
//! collector flag:
//!
//! | store | answer |
//! |---|---|
//! | `gen_heap::set_field`, compact arm | box into a 1-field `AUTOBOX_CLASS_ID` wrapper; `get_field` unboxes — the value survives |
//! | `zgc::set_field` / `g1::set_field` / `heap::set_field`, compact arm | delegate straight to `write_compact_field`, whose `FieldStorageKind::Reference` arm maps every non-`Object` value to raw `0` — the write is silently dropped to null |
//! | any of the four, legacy 16-byte `Value` cell | the raw `Value::Int` is stored and reads back — the value survives |
//!
//! Two lanes established the consequences rather than predicting them
//! (W7-75-continuation-forkjoinpool-alias.md §4,
//! W7-77-guarded-slot-maps.md §4): no bogus pointer reaches any collector on
//! any of those arms, so the "a pointer for the collector to mark and move"
//! framing in W7-69-read-side-alias-instrument.md §6 is wrong — but a write
//! that vanishes is its own defect, and on a real `java.time.Month` it nulls
//! `Enum.name` on a *shared enum constant*, after which an `unwrap_or(1)`
//! fallback answers January for every month of the year.
//!
//! # Why boxing, and not null / not a refusal
//!
//! Recorded in full in W7-84-primitive-in-reference-store.md §3. The short
//! form, because a reader here needs the reason and not just the rule:
//!
//! 1. **All four heaps already agree on boxing one granularity up.**
//!    `Heap::set_array_element`, `GenerationalHeap::set_array_element`,
//!    `G1Collector::set_array_element` and `ZgcRealHeap::set_array_element`
//!    every one box a non-`Object` element into an `AUTOBOX_CLASS_ID` wrapper,
//!    and every one unboxes on the read. G1's array path carried the identical
//!    `_ => 0` encoder and the identical defect — `.mapToDouble(..).toArray()`
//!    returned all zeros under `-XX:+UseG1GC` and was correct under the default
//!    collector — and that was adjudicated in favour of boxing (see
//!    `G1Collector::set_array_element`'s own comment). Answering the FIELD
//!    question differently would trade a disagreement between collectors for a
//!    disagreement between fields and array elements.
//! 2. **Boxing is what the legacy layout has always done**, in all four heaps.
//!    `CRATONVM_COMPACT_REF_FIELDS` is a *representation* switch; it must not
//!    change what a program observes. Before this module, flipping it on ZGC
//!    turned `Int(42)` into `null`.
//! 3. **Null is not "visible".** A null in a reference field is
//!    indistinguishable from a legitimately-null reference field: no detector
//!    fires on it and no reader can tell. Visibility has to come from an
//!    instrument, so this module carries one ([`observe_primitive_into_reference_field`])
//!    and it is LOUDER than what it replaces, never quieter.
//! 4. **Refusing is the right diagnostic and the wrong behaviour.** The
//!    population that reaches this is live in Compatible mode on the default
//!    collector (W7-69 §4), so a hard error converts a wrong answer into a
//!    crash on shipped paths, and a `debug_assert!` would red the synthetic-JDK
//!    tests where a fabricated class's slot 0 genuinely IS the value it is
//!    being handed. Take the diagnostic half without the behaviour half.
//!
//! # The latch, and why the read path is cheaper than it was
//!
//! `gen_heap::get_field` used to pay an unconditional `is_object_address` probe
//! plus a header read on EVERY compact reference-field read, just in case the
//! slot held a wrapper. Giving the other three heaps that unconditionally would
//! be a real regression on the default collector.
//!
//! [`wrapper_exists`] is a process-global relaxed `AtomicBool`, set the first
//! time any heap creates a wrapper (field OR array). A process that never boxes
//! — the overwhelming majority — pays one relaxed load of a shared, read-only
//! cache line and a perfectly-predicted branch, and never touches the address
//! validator. So after this change **no arm is slower than `gen_heap` was
//! before it, and in a process that never boxes every arm is faster.**
//!
//! **Correction (gc-common w1-g, 2026-09-23): the premise of the paragraph
//! above does not hold in this VM.** The class-mirror populator boxes a
//! `ClassId` into slot 0 of every `java/lang/Class` object at bootstrap (see
//! [`header_is_wrapper`] for the 2026-09-05 measurement), so the latch is armed
//! before `main` in every process and the latch-screened read half (then
//! `unbox_reference_slot`) paid its `validated_class_id` probe --
//! `is_object_address` plus a header read -- on every non-null compact
//! reference-field read through the `VmHeap` accessors of all three backends.
//! The latch still costs nothing when it is off; it is just never off. Tracked
//! in `common-g-autobox-latch-always-armed` (fixed 2026-09-23, now under
//! `docs/internal/gc-common-round-20260923/`).
//!
//! **Split (gc-common w3-e, 2026-09-23).** The mirror overlays are declared
//! (an [`ExpectedPrimitiveIntoReference`] scope) and all land in one
//! `(class_id, index)` slot, so they no longer arm the latch the READ screens
//! use: [`unexpected_wrapper_exists`] / [`field_read_may_hold_wrapper`] stay
//! off in a program that does not box, and only reads of the overlay slot
//! itself still probe. See the split-latch note below [`note_wrapper_created`].
//! Since gc-common w4-e every field reader (the three collectors and the
//! semispace `Heap`) goes through [`unbox_reference_field`]; the latch-screened
//! `unbox_reference_slot` is gone. [`wrapper_exists`] keeps its meaning ("any
//! wrapper, declared or not") as a diagnostic and for callers outside this
//! crate.
//!
//! The latch is deliberately set by the ARRAY boxing sites too. Its meaning is
//! "an `AUTOBOX_CLASS_ID` wrapper exists in this process", which is exactly the
//! precondition for a read-side check to be able to find one; keying it to
//! field boxing alone would have required an argument about which creation
//! sites can reach which read sites, and `Heap::get_array_element` does not
//! unbox, so a wrapper CAN be laundered out of an array and into a field.
//! Conservative and needing no such argument is worth the odd extra probe.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cratonvm_types::{ClassId, ObjectRef, Value, AUTOBOX_CLASS_ID};

/// Has any heap in this process created an auto-box wrapper?
///
/// Relaxed on both sides: this is a monotone one-way latch and the only
/// consequence of a stale `false` is that one read skips an unbox it did not
/// need to skip — and it CANNOT be stale for the thread that boxed, because
/// [`note_wrapper_created`] runs on that thread before the wrapper reference is
/// published into the slot. A reader on another thread that can observe the
/// slot has, by the same publication, an ordering edge to the store.
static WRAPPER_CREATED: AtomicBool = AtomicBool::new(false);

/// Whether a wrapper could exist anywhere in this process.
///
/// The read-side fast-path screen. See the module note on cost.
#[inline(always)]
pub fn wrapper_exists() -> bool {
    WRAPPER_CREATED.load(Ordering::Relaxed)
}

/// Should a reference-ARRAY read consult [`wrapper_exists`] before probing for
/// a wrapper? Default yes; `CRATONVM_GC_NO_ARRAY_AUTOBOX_LATCH=1`
/// (`CRATONVM_GC=-array-autobox-latch`) restores the unconditional probe.
///
/// # Why this existed to be fixed
///
/// The module note above explains the latch by saying `gen_heap::get_field`
/// "used to pay an unconditional `is_object_address` probe plus a header read
/// on EVERY compact reference-field read, just in case the slot held a
/// wrapper", and that giving the other heaps that unconditionally "would be a
/// real regression".
///
/// The FIELD read paths took that advice — `get_field` goes through
/// the latched read half (today [`unbox_reference_field`], screened per
/// slot). The reference-**array** read
/// paths did not: `ZgcRealHeap::get_array_element` (both its barrier arm and
/// its plain arm) and `G1Collector::get_array_element` called
/// [`super::GarbageCollector::autobox_payload`] directly, whose first act is
/// `is_object_address` — the exact probe this latch exists to avoid — on every
/// non-null `aaload`. Measured 2026-09-05 at +47 ns for an `aaload` over an
/// `iaload` + `ifne` doing the same work, against HotSpot's −0.8 ns.
///
/// # Why the screen cannot change an answer
///
/// `autobox_payload` returns `Some` only when the loaded object's header says
/// `class_id == AUTOBOX_CLASS_ID`, and such an object can only exist if some
/// site created one — which is exactly what sets the latch, array sites
/// included (see the module note on why it is deliberately set by those too).
/// So `wrapper_exists() == false` implies `autobox_payload` would have
/// returned `None`. The ordering argument for a cross-thread reader is the one
/// [`wrapper_exists`] already makes.
#[inline(always)]
pub fn array_unbox_latch_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GC_NO_ARRAY_AUTOBOX_LATCH").is_none()
    })
}

/// Is this object an auto-box wrapper? The per-object test, for a reader that
/// already holds a validated header.
///
/// # Why the process-wide latch is not enough
///
/// [`wrapper_exists`] was designed as a cheap screen on the premise, stated in
/// the module note above, that "a process that never boxes — the overwhelming
/// majority — pays one relaxed load ... and never touches the address
/// validator". **In this VM there is no such process.** The class-mirror
/// populator puns a `ClassId` (and `Int(-1)` for primitive mirrors) into slot 0
/// of an object stamped `java/lang/Class`, that store goes through
/// [`box_for_reference_slot`], and it arms the latch unconditionally at
/// bootstrap. Measured 2026-09-05 with a per-cause census on
/// `probes/ArrBurn.java`: `aaload: hit=0 miss_barrier=0 miss_wrapper=2000146`.
/// Every screen keyed on the latch is therefore permanently open, and every
/// fast path guarded by one is dead code.
///
/// A reader that has already loaded the reference can ask the precise question
/// instead, for one header compare. That is the same discriminator
/// [`super::GarbageCollector::autobox_payload`] applies — minus the
/// `is_object_address` probe, which
/// `interpreter::field_fast::registry_probe_restored` establishes is not
/// needed for parity with the handlers these paths replace.
#[inline(always)]
pub fn header_is_wrapper(header: &cratonvm_types::ObjectHeader) -> bool {
    header.class_id == AUTOBOX_CLASS_ID
}

/// The screen itself: `true` when a reference-array read must go on and probe
/// for a wrapper. Reads as "the latch is off, or it says a wrapper exists".
///
/// gc-common w3-e (2026-09-23): keyed on [`unexpected_wrapper_exists`], not on
/// [`wrapper_exists`]. The only wrappers the narrower latch leaves out are the
/// declared class-mirror overlays, which live in ONE field slot and reach an
/// array only if a raw (non-unboxing) reader launders one out of that slot --
/// see the split-latch note below [`note_wrapper_created`]. Every array boxing
/// site arms the narrower latch too, so an element wrapper is never missed.
/// Before this, the boot-time mirror overlays held this screen open in every
/// process, exactly as [`header_is_wrapper`] records.
#[inline(always)]
pub fn array_read_may_hold_wrapper() -> bool {
    !array_unbox_latch_enabled() || unexpected_wrapper_exists()
}

/// Arm [`wrapper_exists`] AND [`unexpected_wrapper_exists`]. Called from every
/// site that allocates a wrapper outside [`box_for_reference_slot`] -- the
/// `set_array_element`s -- which is to say from every site that is not a
/// declared field overlay.
#[inline]
pub(crate) fn note_wrapper_created() {
    // Store, not swap: a redundant store to an already-`true` flag is cheaper
    // than an RMW, and this is on an allocating path either way.
    WRAPPER_CREATED.store(true, Ordering::Relaxed);
    LATCHES.note_undeclared();
}

// ---------------------------------------------------------------------------
// The split latch (gc-common w3-e, 2026-09-23)
// ---------------------------------------------------------------------------
//
// `common-g-autobox-latch-always-armed`: [`WRAPPER_CREATED`] is armed before
// `main` in every process, because the class-mirror populator boxes a
// `ClassId` / `Int(-1)` into slot 0 of every `java/lang/Class` object. Every
// screen keyed on it is permanently open, so every non-null compact
// reference-field read through `get_field`, on every backend, paid an address
// validation plus a dependent header load of the REFERENT -- usually a cold
// line -- to learn that it was not a wrapper.
//
// The mirror populator is the one producer that DECLARES its stores (an
// [`ExpectedPrimitiveIntoReference`] scope), and every one of its stores lands
// in the same `(class_id, field index)` slot. So the latch is split:
//
// * the UNEXPECTED latch ([`unexpected_wrapper_exists`]) -- armed by every
//   wrapper that is NOT a declared field overlay: any array wrapper, any field
//   wrapper made outside an expected scope, and any expected-scope wrapper in
//   a second, different slot (see below). This is the latch the old one was
//   supposed to be: off in an ordinary program.
// * the OVERLAY SLOT -- the ONE `(class_id, index)` pair whose wrappers were
//   all declared. A read of that exact slot must still unbox
//   (`mirror_class_id`'s slot-0 fallback depends on it: see
//   `vm/src/vm/vm_object.rs`), every other slot need not.
//
// WHY THIS CANNOT HIDE A WRAPPER A CORRECT PROGRAM RELIES ON. A wrapper in a
// field slot S of an object of class C got there by one of:
//
//   1. `box_for_reference_slot(.., C, S, ..)` -- recorded: S is the overlay
//      slot, or the unexpected latch is armed;
//   2. an ARRAY boxing site -- arms the unexpected latch (`note_wrapper_created`);
//   3. a plain REFERENCE store of a wrapper object some reader handed out. The
//      unboxing readers (`get_field`, `get_array_element*`) never hand one out;
//      only a RAW reader of a slot holding one can, and the only such slot
//      without the unexpected latch armed is the overlay slot itself --
//      `java.lang.Class.cachedConstructor`, where a raw reader (the JIT's
//      compact field helper, `vm_object.rs`'s documented "JIT hazard") already
//      sees a wrapper where a `Constructor` belongs. That program is wrong
//      before the wrapper moves anywhere, and it was wrong in the same way
//      before this change (it then read back an `Int` in a reference slot).
//
// One overlay slot, not a set: a second distinct declared slot arms the
// unexpected latch instead, which is always correct (it is the old behaviour).

/// The two halves of the split latch, as one value so the classification can
/// be unit-tested on a private instance (the process-wide one, [`LATCHES`], is
/// monotone and shared by every test in the binary).
struct WrapperLatches {
    /// Has any wrapper been created that is NOT a declared field overlay?
    unexpected: AtomicBool,
    /// The single declared overlay slot, packed by [`overlay_slot_key`], or
    /// [`NO_OVERLAY_SLOT`].
    overlay_slot: AtomicU64,
}

/// "No declared overlay slot recorded yet". Never a valid key: see
/// [`overlay_slot_key`].
const NO_OVERLAY_SLOT: u64 = u64::MAX;

/// `(class_id << 32) | index`, or `None` for an index that does not fit below
/// `u32::MAX` -- such a slot is simply never treated as the overlay. The low
/// half of a key is therefore never all-ones, so no key equals
/// [`NO_OVERLAY_SLOT`].
#[inline(always)]
fn overlay_slot_key(class_id: ClassId, index: usize) -> Option<u64> {
    if index >= u32::MAX as usize {
        return None;
    }
    Some(((class_id.as_u32() as u64) << 32) | index as u64)
}

impl WrapperLatches {
    const fn new() -> Self {
        Self {
            unexpected: AtomicBool::new(false),
            overlay_slot: AtomicU64::new(NO_OVERLAY_SLOT),
        }
    }

    #[inline(always)]
    fn unexpected(&self) -> bool {
        self.unexpected.load(Ordering::Relaxed)
    }

    #[inline(always)]
    fn note_undeclared(&self) {
        self.unexpected.store(true, Ordering::Relaxed);
    }

    #[inline(always)]
    fn field_may_hold(&self, owner: ClassId, index: usize) -> bool {
        if self.unexpected() {
            return true;
        }
        match overlay_slot_key(owner, index) {
            Some(key) => self.overlay_slot.load(Ordering::Relaxed) == key,
            None => false,
        }
    }

    /// Classify a FIELD wrapper for field `index` of an object of class
    /// `class_id`; `expected` is whether the store was made inside an
    /// [`ExpectedPrimitiveIntoReference`] scope.
    fn note_field(&self, class_id: ClassId, index: usize, expected: bool) {
        if expected {
            if let Some(key) = overlay_slot_key(class_id, index) {
                // Hot only at boot (~33 mirrors); a load first keeps the
                // steady state off the RMW.
                let cur = self.overlay_slot.load(Ordering::Relaxed);
                if cur == key {
                    return;
                }
                if cur == NO_OVERLAY_SLOT {
                    match self.overlay_slot.compare_exchange(
                        NO_OVERLAY_SLOT,
                        key,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return,
                        Err(now) if now == key => return,
                        Err(_) => {}
                    }
                }
            }
        }
        // Undeclared, unpackable, or a SECOND declared slot: the conservative
        // answer, which is exactly the pre-split behaviour for every slot.
        self.note_undeclared();
    }
}

/// The process-wide split latch. See the note above [`WrapperLatches`].
static LATCHES: WrapperLatches = WrapperLatches::new();

/// Has a wrapper been created anywhere EXCEPT the one declared overlay slot?
///
/// The screen for readers that cannot name the slot they read (reference-array
/// elements). See the split-latch note above.
#[inline(always)]
pub fn unexpected_wrapper_exists() -> bool {
    LATCHES.unexpected()
}

/// Could field `index` of an object of class `owner` hold a wrapper?
///
/// Two relaxed loads, no heap access. `true` when an undeclared wrapper exists
/// anywhere in the process, or when `(owner, index)` is the declared overlay
/// slot. Same cross-thread ordering argument as [`wrapper_exists`]: the latch
/// store precedes, on the creating thread, the store that publishes the
/// wrapper into the slot.
#[inline(always)]
pub fn field_read_may_hold_wrapper(owner: ClassId, index: usize) -> bool {
    LATCHES.field_may_hold(owner, index)
}

/// Record a FIELD wrapper just created by [`box_for_reference_slot`].
#[inline]
fn note_field_wrapper_created(class_id: ClassId, index: usize, expected: bool) {
    WRAPPER_CREATED.store(true, Ordering::Relaxed);
    LATCHES.note_field(class_id, index, expected);
}

/// Is this thread inside an [`ExpectedPrimitiveIntoReference`] scope?
#[inline]
fn in_expected_scope() -> bool {
    EXPECTED_DEPTH.with(|d| d.get()) > 0
}

// ---------------------------------------------------------------------------
// The expected-overlay scope
// ---------------------------------------------------------------------------
//
// Two populations reach [`observe_primitive_into_reference_field`] and they are
// not the same news:
//
//   * the VM's OWN class-mirror populator, which type-puns a `ClassId` (and
//     `Int(-1)` for primitive mirrors) into slot 0 of an object stamped
//     `java/lang/Class` -- deliberate, self-inflicted, and load-bearing;
//   * a genuine third-party store of a primitive into a slot the class declares
//     as a reference -- precisely what this guard exists to catch.
//
// Until 2026-09-01 the guard could not tell them apart, and the first
// population is large enough to hide the second: a stock `cratonvm Hello` mints
// ~33 mirrors, which the `n < 8 || n.is_power_of_two()` limiter turns into ~12
// WARN lines, and by the time a real third-party store arrives the budget is
// spent. Measured, with the corroboration across five other programs, in
// `.agent-requests/A12-gc-guard.txt` and at both write sites in
// `vm/src/vm/vm_object.rs`.
//
// So the ONE known producer marks its own stores expected, at the call site,
// and the guard stays fully armed for everybody else. A scope and not a flag,
// and emphatically not a filter on `class_id == 12 && index == 0`: a class-id
// filter would also swallow a THIRD-PARTY store into that same slot, which is
// the single most interesting store this guard could ever see.

/// Nesting depth of [`expect_primitive_into_reference`] scopes on **this
/// thread**.
///
/// Thread-local rather than a process-global flag, and the difference is the
/// whole point. The store and the guard run on the same thread one frame apart,
/// so a thread-local is sufficient; a global would additionally mute a
/// concurrent third-party store happening on another thread while the mirror
/// populator runs -- and the boot is exactly when other threads are starting.
/// A guard that goes blind under concurrency is worse than a noisy one.
///
/// A `u32` depth rather than a `bool` so nesting is safe by construction: an
/// inner `bool` scope would clear the outer one when it dropped.
///
/// `const`-initialised and holding a non-`Drop` `Cell<u32>`, so this
/// thread-local has neither a destructor nor a lazy-init flag -- `with`
/// therefore cannot observe a destroyed slot and cannot panic. That matters
/// because the reader below sits on a heap store path that can run on a thread
/// already tearing down.
thread_local! {
    static EXPECTED_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Boxing stores made inside an expected scope.
///
/// Counted, never printed per event. The bar this change had to clear is that a
/// quiet boot means "nothing unexpected happened" and never "we stopped
/// looking", so the expected population has to stay *measurable* -- otherwise
/// silencing it is indistinguishable from deleting the guard. Read it with
/// [`expected_primitive_into_reference_count`]; it also rides along as the
/// `expected_overlay` field on every WARN the guard does emit, which is the
/// moment the number is most worth having, because it says how much known
/// traffic ran before this unknown store.
static EXPECTED_STORES: AtomicU64 = AtomicU64::new(0);

/// A store made while this guard is alive is a KNOWN VM-internal overlay: it is
/// counted into [`expected_primitive_into_reference_count`] and not warned
/// about.
///
/// The one caller is `vm/src/vm/vm_object.rs`'s class-mirror populator
/// (`get_or_create_class_mirror` and `get_or_create_primitive_mirror`), which
/// deliberately writes a `ClassId` / `Int(-1)` into slot 0 of a
/// `java/lang/Class` object whose real JDK 25 slot 0 is
/// `Constructor<T> cachedConstructor`. It cannot stop, and the three
/// cheaper-looking answers are all unsound: skipping the store was MEASURED to
/// fail `RJdkHello` at `System.out instanceof PrintStream`; hand-boxing there
/// would hand `mirror_class_id`'s slot-0 fallback the WRAPPER instead of the
/// `Int`, because [`unbox_reference_field`] is gated on the latch / overlay
/// slot that only [`box_for_reference_slot`] records; and
/// `set_field_as(.., b'L')` coerces `Int(_)` to `Value::Object(None)` and loses
/// the tag entirely. The long form of all three, with the dates, is at the
/// write site.
///
/// **Hold it across as little as possible.** The scope is a suppression, so
/// every instruction inside it is an instruction the guard is not watching.
/// Both call sites `drop` it explicitly on the line after the store rather than
/// letting it live to the end of the function: `set_field`'s boxing closure
/// allocates, an allocation can collect, and a collection's own field writes
/// must not inherit the scope.
///
/// RAII rather than a `set`/`clear` pair so the expectation cannot leak past a
/// `?` or a panic -- and the allocation just named is exactly a thing that can
/// unwind.
#[must_use]
pub struct ExpectedPrimitiveIntoReference(());

impl Drop for ExpectedPrimitiveIntoReference {
    fn drop(&mut self) {
        // `saturating_sub`: a depth that somehow reached 0 early must not wrap
        // to `u32::MAX` and mute this thread's guard for the rest of the run.
        // Failing towards "the guard is armed" is the only acceptable direction
        // for a bug inside a suppression.
        EXPECTED_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Open an expected-overlay scope on this thread. See
/// [`ExpectedPrimitiveIntoReference`] for what belongs inside one, and why the
/// scope has to be kept to a single statement.
#[must_use]
pub fn expect_primitive_into_reference() -> ExpectedPrimitiveIntoReference {
    EXPECTED_DEPTH.with(|d| d.set(d.get() + 1));
    ExpectedPrimitiveIntoReference(())
}

/// How many boxing stores this process made inside an expected scope.
///
/// Process-wide and monotone. The intended long-term reader is the shutdown
/// census line that already prints the sibling counters (see
/// `.agent-requests/A12-gc-guard.txt` item 4, which is owned by another file);
/// until it is wired there the number is reachable from any test or debugger,
/// and from the guard's own WARN as `expected_overlay`.
#[must_use]
pub fn expected_primitive_into_reference_count() -> u64 {
    EXPECTED_STORES.load(Ordering::Relaxed)
}

/// Does `value` need boxing before it can occupy a REFERENCE slot?
///
/// `Value::Object(None)` does NOT — a native clearing a real reference field
/// legitimately writes it, and boxing it would turn a null field into a
/// non-null wrapper, which is the one way this whole scheme can make a program
/// worse (a `field == null` check that used to pass). Only genuinely
/// non-reference tags are boxed.
#[inline(always)]
pub(crate) fn needs_reference_box(value: Value) -> bool {
    !matches!(value, Value::Object(_))
}

/// The instrument: a primitive is about to be boxed into a declared-reference
/// field slot, i.e. a type-punning store has been caught in the act.
///
/// **The store is NOT always a native, and on the boot path it is not one at
/// all.** Measured 2026-08-12 on the current binary: the very first record of
/// this warning in every run, and the overwhelming majority of them, is
/// `vm/src/vm/vm_object.rs`'s class-mirror populator —
/// `get_or_create_class_mirror` writes `Value::Int(class_id)` and
/// `get_or_create_primitive_mirror` writes `Value::Int(-1)` into slot 0 of an
/// object stamped `java/lang/Class`, whose real JDK 25 slot 0 is
/// `Constructor<T> cachedConstructor`, a reference. That is a VM-internal
/// overlay (`internal/audits/jdk-only-object-layout-audit.md` rank 6), it
/// goes through `shared.mem.heap.set_field` rather than through any
/// `NativeContext`, and so **no native-side census can ever name it**. Pointing
/// a reader at the read-side alias census for it — as this message used to —
/// sends them to an instrument that is structurally unable to answer.
///
/// To turn a `class_id=ClassId(N)` here into a class NAME, run with
/// **`CRATONVM_DBG_LAYOUT=1`**, which prints `[layout] <name> cid=<N> ...` once
/// per class in add order — e.g. `[layout] java/lang/Class cid=12 body=136
/// refs=16 fields=19`, which is the id this guard reports on the boot path.
///
/// This previously advertised `CRATONVM_DBG_TOARRAY=1`. That variable exists
/// but prints nothing at this site: measured 2026-08-12 by two independent
/// readers who each followed the advice and got an empty transcript, then had
/// to find the working flag themselves. **A diagnostic that names the wrong
/// instrument costs more than no diagnostic**, because it is trusted.
///
/// For stores that genuinely do come
/// from a native, `native-api`'s read-side alias census
/// (`CRATONVM_DBG=layout-alias`) is the right instrument — but note it observes
/// READS, so it names the alias, not this write.
///
/// Rate-limited to the first few plus powers of two, the shape the sibling
/// `cratonvm::gc::guard` records in `gen_heap.rs` / `g1.rs` / `zgc.rs` already
/// use — a per-store record would turn a Tomcat boot into minutes of stderr.
/// No `CRATONVM_*` flag of its own: `tracing`'s own level filter is the gate,
/// and adding a name would need four files or `cargo test -p cratonvm-types`
/// goes red (W7-77 §5.5).
///
/// **Not every store reaches the message.** A store made inside an
/// [`ExpectedPrimitiveIntoReference`] scope is a declared VM-internal overlay:
/// it is counted into [`expected_primitive_into_reference_count`] and returns
/// BEFORE the rate limiter, so it neither prints nor spends budget. Exactly one
/// producer opens such a scope — `vm/src/vm/vm_object.rs`'s class-mirror
/// populator, which G30 §1 established was every single warning a default run
/// emitted. The screen is thread-local and lasts one statement; every other
/// producer, on this thread or any other, still gets the full message below
/// with the same escalating dedup.
///
/// This is ADDITIVE to the two detectors that already cover this species —
/// `vm_exec.rs`'s `overlay_access_is_cross_type` (the interpreter/native
/// boundary) and `native-api`'s read-side alias census. Neither is quietened by
/// the convergence: they fire on the store attempt, which still happens.
#[cold]
pub(crate) fn observe_primitive_into_reference_field(
    class_id: ClassId,
    index: usize,
    value: Value,
) {
    // The expected-overlay screen, and it is deliberately the FIRST statement
    // rather than a condition on the `warn!` below. If an expected store fell
    // through to `SEEN.fetch_add` it would still consume the
    // `n < 8 || n.is_power_of_two()` budget, so the boot's ~33 known mirror
    // stores would push the first genuine third-party store past occurrence 33
    // -- where the next printable occurrence is 64 -- and it would never be
    // printed at all. Quietening the known producer WITHOUT also giving it back
    // its budget would have left the guard strictly worse than the noise it
    // replaced. See `ExpectedPrimitiveIntoReference`.
    if EXPECTED_DEPTH.with(|d| d.get()) > 0 {
        EXPECTED_STORES.fetch_add(1, Ordering::Relaxed);
        return;
    }

    static SEEN: AtomicU64 = AtomicU64::new(0);
    let n = SEEN.fetch_add(1, Ordering::Relaxed);
    if n < 8 || n.is_power_of_two() {
        tracing::warn!(
            target: "cratonvm::gc::guard",
            class_id = ?class_id,
            index,
            value = ?value,
            occurrence = n,
            expected_overlay = EXPECTED_STORES.load(Ordering::Relaxed),
            "a non-reference value was stored into a slot the class declares as \
             a REFERENCE — boxing it into an AUTOBOX_CLASS_ID wrapper so the \
             value survives and every collector agrees (W7-84). The store is a \
             type-punning one and it is NOT necessarily a native. Since \
             2026-09-01 it is also NOT the VM's own class-mirror populator: \
             that producer (a ClassId, or Int(-1), written over \
             java.lang.Class.cachedConstructor by vm/src/vm/vm_object.rs) \
             declares its two stores expected and is tallied into \
             expected_overlay instead of being reported here — it used to be \
             every single one of these records, so a reader who chases it now \
             is chasing the wrong file. Run with CRATONVM_DBG_LAYOUT=1 to \
             resolve class_id to a name.",
        );
    }
}

/// The write half, shared by all four field-store implementations.
///
/// Returns the value that should actually be written into the reference slot:
/// `value` unchanged when it is already a reference, or a fresh 1-field
/// `AUTOBOX_CLASS_ID` wrapper carrying it when it is not.
///
/// `alloc_wrapper` must allocate a 1-field object of [`AUTOBOX_CLASS_ID`] on
/// the calling heap and store `value` in its slot 0. It is a closure rather
/// than a trait method because the four heaps do not share an allocation trait
/// at this layer, and because two of them (G1, and any humongous path) must
/// allocate BEFORE taking their region lock.
///
/// The wrapper can never itself take this path: `register_class_layout`
/// refuses a `class_id` at or above `MAX_DENSE_CLASS_LAYOUTS` and
/// `AUTOBOX_CLASS_ID` is `u32::MAX`, so a wrapper is provably a legacy
/// 16-byte-cell object and its slot 0 is provably not a compact reference
/// slot. No recursion.
#[inline]
pub(crate) fn box_for_reference_slot(
    value: Value,
    class_id: ClassId,
    index: usize,
    alloc_wrapper: impl FnOnce(Value) -> ObjectRef,
) -> Value {
    if !needs_reference_box(value) {
        return value;
    }
    // Read BEFORE the allocation: `alloc_wrapper` can collect, and whatever a
    // collection does on this thread must not decide how this store is
    // classified. (The scope is thread-local and still open here either way;
    // this just keeps the classification a property of the store.)
    let expected = in_expected_scope();
    observe_primitive_into_reference_field(class_id, index, value);
    // gen r5w1/oom5: inside a `FallibleWrapperScope` the wrapper comes from
    // the scope's non-aborting allocator instead of `alloc_wrapper`. One-shot
    // (`take`), so nothing the allocation itself does on this thread can
    // inherit it.
    if let Some((try_alloc, heap)) = FALLIBLE_WRAPPER.with(|c| c.take()) {
        return match try_alloc(heap, value) {
            Some(wrapper) => {
                note_field_wrapper_created(class_id, index, expected);
                Value::Object(Some(wrapper))
            }
            None => {
                // Declined: store the slot's default (null) and let the
                // arming caller see it. No wrapper exists, so no latch moves.
                FALLIBLE_WRAPPER_DECLINED.with(|d| d.set(true));
                Value::Object(None)
            }
        };
    }
    let wrapper = alloc_wrapper(value);
    note_field_wrapper_created(class_id, index, expected);
    Value::Object(Some(wrapper))
}

// ---------------------------------------------------------------------------
// The fallible wrapper scope (gen r5w1/oom5, 2026-09-26)
// ---------------------------------------------------------------------------
//
// `docs/internal/gc/gengc-r4w6-review6-class-mirror-creation-aborts-on-a-full-heap-outside-ldc-FIXED-20260927.md`
// item 2: the class-mirror populator's slot-0 store boxes through each
// backend's PANICKING `alloc_object(AUTOBOX_CLASS_ID, 1)`, so a mirror that
// just fit in the last bytes of a full heap still ended the process on its
// 24-byte wrapper. The four backends pass their own infallible closure to
// `box_for_reference_slot`; rather than give every backend a fallible twin of
// `set_field` (G1 and ZGC are other owners' files), the boxing half consults a
// thread-local, scoped, non-aborting allocator that `VmHeap::try_set_field`
// arms around ONE descriptor-less store. Outside such a scope nothing changes.

/// A non-aborting wrapper allocator: allocate a 1-field [`AUTOBOX_CLASS_ID`]
/// object on the heap `heap` points at, store `value` in its slot 0, and
/// return it -- or `None`, having stored nothing, when the heap cannot hold it
/// without a collection. `heap` is the opaque pointer the arming caller
/// passed; the function is the arming caller's own and knows its type.
pub(crate) type TryAllocWrapper = fn(heap: *const (), value: Value) -> Option<ObjectRef>;

thread_local! {
    /// The armed allocator, if any. `const`-initialised and `Copy`: no
    /// destructor and no lazy-init flag, same reasoning as `EXPECTED_DEPTH`.
    static FALLIBLE_WRAPPER: std::cell::Cell<Option<(TryAllocWrapper, *const ())>> =
        const { std::cell::Cell::new(None) };
    /// Set when the armed allocator declined inside the current scope.
    static FALLIBLE_WRAPPER_DECLINED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII scope returned by [`arm_fallible_wrapper`]. Restores the enclosing
/// scope's state on drop, so nesting (and a panic out of the store) cannot leak
/// the hook past its statement.
#[must_use]
pub(crate) struct FallibleWrapperScope {
    prev: Option<(TryAllocWrapper, *const ())>,
    prev_declined: bool,
}

impl FallibleWrapperScope {
    /// Did a boxing store inside this scope decline (and store null instead)?
    pub(crate) fn declined(&self) -> bool {
        FALLIBLE_WRAPPER_DECLINED.with(|d| d.get())
    }
}

impl Drop for FallibleWrapperScope {
    fn drop(&mut self) {
        FALLIBLE_WRAPPER.with(|c| c.set(self.prev));
        FALLIBLE_WRAPPER_DECLINED.with(|d| d.set(self.prev_declined));
    }
}

/// Arm a non-aborting wrapper allocator on this thread until the returned
/// scope drops: the next boxing store made through [`box_for_reference_slot`]
/// allocates its wrapper with `alloc(heap, value)` instead of the backend's
/// panicking closure, and on `None` stores null and marks the scope
/// [`declined`](FallibleWrapperScope::declined).
///
/// # Safety
///
/// `heap` must stay valid, for whatever `alloc` reads through it, until the
/// returned scope is dropped.
pub(crate) unsafe fn arm_fallible_wrapper(
    alloc: TryAllocWrapper,
    heap: *const (),
) -> FallibleWrapperScope {
    let prev = FALLIBLE_WRAPPER.with(|c| c.replace(Some((alloc, heap))));
    let prev_declined = FALLIBLE_WRAPPER_DECLINED.with(|d| d.replace(false));
    FallibleWrapperScope {
        prev,
        prev_declined,
    }
}

/// The read half, shared by all four field-read implementations: `value` was
/// just read from field `index` of an object of class `owner`.
///
/// `owner` must be the receiver's header `class_id` -- the same id its
/// `set_field` hands [`box_for_reference_slot`] -- because the screen is keyed
/// on the `(owner, index)` slot.
///
/// `validated_class_id` must return the header `ClassId` of `r` **only after**
/// proving `r` is a live object of the calling heap; a reference slot holds a
/// raw word and a stale one could point anywhere, so an unchecked header read
/// here would be wild. Each heap already owns that primitive
/// (`GenerationalHeap::is_object_address`, `ZgcRealHeap::is_object_address`,
/// `G1Collector::is_object_address`, `Heap::is_valid_heap_object`).
///
/// Ordered so the expensive validation is unreachable until the screen says a
/// wrapper could be in this slot at all. The screen is
/// [`field_read_may_hold_wrapper`], not [`wrapper_exists`]: in a process whose
/// only wrappers are the declared class-mirror overlays (every process that
/// does not type-pun a primitive into a reference field itself) a compact
/// reference-field read no longer validates and loads the header of every
/// non-null referent. Reads of the overlay slot itself still unbox, which is
/// what `mirror_class_id`'s slot-0 fallback needs.
/// `common-g-autobox-latch-always-armed`: split latch gc-common w3-e; the
/// three collector `get_field`s moved onto this in w4-e, and the old
/// latch-screened `unbox_reference_slot` was deleted with its last caller.
#[inline]
pub(crate) fn unbox_reference_field(
    value: Value,
    owner: ClassId,
    index: usize,
    validated_class_id: impl FnOnce(ObjectRef) -> Option<ClassId>,
    read_wrapper_payload: impl FnOnce(ObjectRef) -> Value,
) -> Value {
    if !field_read_may_hold_wrapper(owner, index) {
        return value;
    }
    let Value::Object(Some(r)) = value else {
        return value;
    };
    if validated_class_id(r) != Some(AUTOBOX_CLASS_ID) {
        return value;
    }
    read_wrapper_payload(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Object(None)` must NOT be boxed. A native clearing a real reference
    /// field writes exactly that, and boxing it would replace a null field
    /// with a non-null wrapper — the one regression this scheme can cause
    /// (`vm/src/vm/vm_init.rs`'s `System.out` slot-0 guard is the site that
    /// already had to work around a non-null wrapper defeating a null check).
    #[test]
    fn a_null_reference_is_not_boxed() {
        assert!(!needs_reference_box(Value::Object(None)));
        // SAFETY: never dereferenced — `needs_reference_box` only reads the tag.
        let some = unsafe { ObjectRef::from_raw(8usize as *mut u8) };
        assert!(!needs_reference_box(Value::Object(Some(some))));
    }

    /// The other side, so the predicate is not vacuously false: every
    /// non-reference tag IS boxed. `Uninitialized` is included deliberately —
    /// it is what a freshly-allocated legacy slot decodes as, and letting it
    /// through unboxed would write a raw 0 into the slot under exactly the
    /// encoder this change exists to stop trusting.
    #[test]
    fn every_non_reference_tag_is_boxed() {
        for v in [
            Value::Int(16),
            Value::Long(-1),
            Value::Float(0.75),
            Value::Double(-17.5),
            Value::Uninitialized,
        ] {
            assert!(needs_reference_box(v), "{v:?} must be boxed");
        }
    }

    /// The latch gates the read half, and the read half is otherwise a
    /// faithful un-wrap. Both directions, so neither "always unbox" nor
    /// "never unbox" passes.
    #[test]
    fn the_latch_gates_the_read_half_in_both_directions() {
        // SAFETY: never dereferenced — the closures below are the only things
        // that would, and they answer from the test's own tables.
        let r = unsafe { ObjectRef::from_raw(16usize as *mut u8) };
        let boxed = Value::Object(Some(r));
        let owner = ClassId::new(9);

        // Latch armed (this test's own store; the flag is monotone and other
        // tests in this binary may have armed it already, which is fine). An
        // array wrapper arms the UNEXPECTED latch, so every slot is screened
        // open from here on.
        note_wrapper_created();
        assert!(field_read_may_hold_wrapper(owner, 3));
        assert_eq!(
            unbox_reference_field(
                boxed,
                owner,
                3,
                |_| Some(AUTOBOX_CLASS_ID),
                |_| { Value::Int(42) }
            ),
            Value::Int(42),
            "an armed latch plus a wrapper class id must unbox",
        );
        assert_eq!(
            unbox_reference_field(
                boxed,
                owner,
                3,
                |_| Some(ClassId::new(3)),
                |_| { Value::Int(42) }
            ),
            boxed,
            "an ordinary object must come back as itself, not as its slot 0",
        );
        assert_eq!(
            unbox_reference_field(boxed, owner, 3, |_| None, |_| Value::Int(42)),
            boxed,
            "an address the heap cannot validate must come back untouched",
        );
        assert_eq!(
            unbox_reference_field(
                Value::Object(None),
                owner,
                3,
                |_| Some(AUTOBOX_CLASS_ID),
                |_| { Value::Int(42) }
            ),
            Value::Object(None),
            "null is not a wrapper",
        );
    }

    /// `box_for_reference_slot` arms the latch, and returns the wrapper the
    /// allocator handed it rather than the primitive.
    #[test]
    fn boxing_arms_the_latch_and_returns_the_wrapper() {
        // SAFETY: never dereferenced.
        let wrapper = unsafe { ObjectRef::from_raw(24usize as *mut u8) };
        let mut boxed_payload = None;
        let out = box_for_reference_slot(Value::Int(0x5EED), ClassId::new(9), 0, |v| {
            boxed_payload = Some(v);
            wrapper
        });
        assert_eq!(out, Value::Object(Some(wrapper)));
        assert_eq!(boxed_payload, Some(Value::Int(0x5EED)));
        assert!(wrapper_exists());
    }

    /// gc-common w3-e: the split latch. A declared overlay store opens the
    /// field screen for ITS slot only and leaves the unexpected latch off; an
    /// undeclared field store, an array wrapper, or a second declared slot
    /// arms the unexpected latch, after which every slot is screened open.
    /// Private instances, because the process-wide one is monotone and
    /// shared with every other test in this binary.
    #[test]
    fn the_split_latch_keeps_the_mirror_overlay_to_its_own_slot() {
        let mirror = ClassId::new(12);
        let other = ClassId::new(40);

        let l = WrapperLatches::new();
        assert!(!l.unexpected());
        assert!(!l.field_may_hold(mirror, 0));

        // The mirror populator, ~33 times at boot.
        for _ in 0..33 {
            l.note_field(mirror, 0, true);
        }
        assert!(!l.unexpected(), "declared overlays must not arm the latch");
        assert!(
            l.field_may_hold(mirror, 0),
            "the overlay slot must still unbox"
        );
        assert!(
            !l.field_may_hold(mirror, 1),
            "a neighbouring field is not the overlay"
        );
        assert!(
            !l.field_may_hold(other, 0),
            "another class's slot 0 is not the overlay"
        );

        // A second DISTINCT declared slot: conservative.
        let l2 = WrapperLatches::new();
        l2.note_field(mirror, 0, true);
        l2.note_field(other, 3, true);
        assert!(l2.unexpected());
        assert!(l2.field_may_hold(other, 7));

        // An undeclared field store.
        let l3 = WrapperLatches::new();
        l3.note_field(other, 2, false);
        assert!(l3.unexpected());
        assert!(l3.field_may_hold(mirror, 5));

        // An array wrapper.
        let l4 = WrapperLatches::new();
        l4.note_undeclared();
        assert!(l4.field_may_hold(other, 0));

        // An index that cannot be packed is never the overlay.
        let l5 = WrapperLatches::new();
        l5.note_field(mirror, u32::MAX as usize, true);
        assert!(l5.unexpected());
        assert_eq!(
            overlay_slot_key(ClassId::new(u32::MAX), u32::MAX as usize),
            None
        );
        assert_ne!(
            overlay_slot_key(ClassId::new(u32::MAX), (u32::MAX - 1) as usize),
            Some(NO_OVERLAY_SLOT),
        );
    }

    /// `unbox_reference_field` screens on the SLOT: with a wrapper-looking
    /// referent, the overlay slot unboxes, any other slot comes back untouched
    /// (while the process-wide unexpected latch is off -- which another test in
    /// this binary may already have armed, in which case both unbox and the
    /// second half is skipped).
    #[test]
    fn unbox_reference_field_follows_the_field_screen() {
        // SAFETY: never dereferenced -- the closures answer from the test.
        let r = unsafe { ObjectRef::from_raw(40usize as *mut u8) };
        let boxed = Value::Object(Some(r));
        let mirror = ClassId::new(12);
        {
            let _scope = expect_primitive_into_reference();
            let _ = box_for_reference_slot(Value::Int(12), mirror, 0, |_| r);
        }
        assert_eq!(
            unbox_reference_field(
                boxed,
                mirror,
                0,
                |_| Some(AUTOBOX_CLASS_ID),
                |_| Value::Int(12)
            ),
            Value::Int(12),
            "the declared overlay slot must unbox",
        );
        if !unexpected_wrapper_exists() {
            assert_eq!(
                unbox_reference_field(
                    boxed,
                    ClassId::new(41),
                    0,
                    |_| panic!("validated a referent behind a closed screen"),
                    |_| Value::Int(0)
                ),
                boxed,
            );
        }
    }

    /// gen r5w1/oom5: inside an armed fallible scope the boxing store uses
    /// the scope's allocator, never the backend's panicking closure; a decline
    /// stores null and is visible on the scope; the hook is one-shot and is
    /// gone once the scope drops.
    #[test]
    fn a_fallible_wrapper_scope_declines_instead_of_calling_the_panicking_allocator() {
        fn decline(_heap: *const (), _v: Value) -> Option<ObjectRef> {
            None
        }
        fn accept(heap: *const (), _v: Value) -> Option<ObjectRef> {
            // SAFETY: never dereferenced; the "heap" pointer is the wrapper.
            Some(unsafe { ObjectRef::from_raw(heap as *mut u8) })
        }
        {
            // SAFETY: `decline` reads nothing through the pointer.
            let scope = unsafe { arm_fallible_wrapper(decline, std::ptr::null()) };
            assert!(!scope.declined());
            let out = box_for_reference_slot(Value::Int(7), ClassId::new(9), 0, |_| {
                panic!("the panicking allocator ran inside a fallible scope")
            });
            assert_eq!(out, Value::Object(None));
            assert!(scope.declined());
        }
        {
            // SAFETY: `accept` never dereferences the pointer.
            let scope = unsafe { arm_fallible_wrapper(accept, 48usize as *const ()) };
            let out = box_for_reference_slot(Value::Int(7), ClassId::new(9), 0, |_| {
                panic!("the panicking allocator ran inside a fallible scope")
            });
            // SAFETY: never dereferenced.
            let expect = unsafe { ObjectRef::from_raw(48usize as *mut u8) };
            assert_eq!(out, Value::Object(Some(expect)));
            assert!(!scope.declined());
            // One-shot: a second boxing store in the same scope takes the
            // ordinary path.
            let mut ran = false;
            let _ = box_for_reference_slot(Value::Int(8), ClassId::new(9), 0, |_| {
                ran = true;
                expect
            });
            assert!(ran);
        }
        // Dropped: the ordinary path, and no stale decline.
        let mut ran = false;
        let _ = box_for_reference_slot(Value::Int(9), ClassId::new(9), 0, |_| {
            ran = true;
            // SAFETY: never dereferenced.
            unsafe { ObjectRef::from_raw(56usize as *mut u8) }
        });
        assert!(ran);
        assert!(!FALLIBLE_WRAPPER_DECLINED.with(|d| d.get()));
    }

    /// A reference store must not reach the allocator at all — the hot path
    /// pays no allocation, and this is what pins that.
    #[test]
    fn a_reference_store_never_allocates() {
        // SAFETY: never dereferenced.
        let r = unsafe { ObjectRef::from_raw(32usize as *mut u8) };
        for v in [Value::Object(None), Value::Object(Some(r))] {
            let out = box_for_reference_slot(v, ClassId::new(9), 0, |_| {
                panic!("allocated a wrapper for a reference store: {v:?}")
            });
            assert_eq!(out, v);
        }
    }
}
