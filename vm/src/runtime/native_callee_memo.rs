// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The resolved-handle form of a native->Java callback.
//!
//! # The question this answers
//!
//! `NativeContext::invoke_virtual(receiver, "someMethod", "()V", &[])` is how a
//! registered native calls back into Java, and ~2 500 stubs use it. Every one
//! of those calls resolves the callee BY NAME, on every call:
//!
//!  * `NativeContextImpl::invoke_virtual` takes the class-manager read lock and
//!    `to_string()`s the receiver's class name — a lock and a heap allocation;
//!  * `invoke_or_native` hashes the `(class, method, descriptor)` triple
//!    against the native slot table (`find_with_kind`), and on a miss runs the
//!    cold descriptor-quirk rewrite;
//!  * it then takes the class-manager read lock twice more (the JVMTI-redefine
//!    probe, which itself runs `find_method_recursive`, and the receiver /
//!    `class_store` lookup at the tail);
//!  * `invoke_on_class_shared_inner` resolves the method by name AGAIN,
//!    `to_string()`s the class name AGAIN, and walks a long `check_override`
//!    chain before reaching `interpreter::execute`.
//!
//! `completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`
//! §3 measured exactly ONE instance of this. `CompletableFuture.complete`'s
//! `postComplete()` callback produced 100 008 missed registry lookups in 40 000
//! chains — 76 % of every missed lookup on the workload — and routing that one
//! call through `invoke_virtual_bytecode_only` was **1.154x of the whole
//! benchmark**. That fix works because a human knows `postComplete` is
//! bytecode. This module is the general form: the VM DISCOVERS it, once, per
//! `(receiver class, method, descriptor)`.
//!
//! # Why a witness, and not a classifier
//!
//! The obvious implementation is a predicate — "is this triple plain
//! bytecode?" — evaluated ahead of the dispatch. It is also unimplementable:
//! `invoke_or_native` and `invoke_on_class_shared_inner` between them carry
//! more than thirty special arms (signature-polymorphic names, dynamic and
//! annotation proxies, the `ClassLoader` bridges, the Spring Boot loader
//! classes, the `SyntheticStub`/real-bytecode yield, the JVMTI-redefine shadow
//! drop, the FFM interface overrides, …), and a predicate that has to agree
//! with all of them is a second copy of the resolver that will drift from the
//! first. `AGENTS.md` names that shape directly: "do not add another
//! hard-coded class-name allow-list. Several already exist, in disagreeing
//! copies."
//!
//! So nothing here predicts. [`arm`] marks the dispatch as a candidate,
//! [`note_plain_bytecode_tail`] fires at the ONE site that is the ordinary
//! tail — the `interpreter::execute` call at the bottom of
//! `invoke_on_class_shared_inner` — and a memo is filled only if the dispatch
//! actually arrived there. Every special arm returns before it, so every
//! special arm is refused by construction, including ones added later that
//! this file has never heard of.
//!
//! # What invalidates a memo
//!
//! The fast path replays `interpreter::execute(declaring_class_id, name, desc)`,
//! so a memo is wrong exactly when the ordinary tail would no longer be
//! reached, or would be reached with a different declaring class. Every input
//! to that decision is covered:
//!
//! | input | guard |
//! |---|---|
//! | a native registered for the triple after the fill | `NativeMethodRegistry::generation()` |
//! | a second loader defining the receiver's class name | the VM's class store's `class_definition_epoch` (its `StoreEpochs` slot; process-wide before wave 21) |
//! | a synthetic stub promoted to real bytecode in place | `class_origin_epoch()` |
//! | an agent redefining a class in place | `class_redefinition_count()` (until wave 18: refused outright once [`any_class_redefined`](crate::classloading::any_class_redefined); `SURVIVES_REDEFINITION`) |
//! | the receiver being a different class | the `ClassId` is part of the key |
//! | a second VM in the same process | `SharedVm::vm_identity` (never reused) is part of the key |
//!
//! `ClassId`s are dense and never reused, and lambda-proxy ids come from a
//! reserved high range (`LAMBDA_PROXY_ID_BASE`), so an id that was an ordinary
//! class when the memo was filled cannot become a proxy afterwards.
//!
//! # Why the table is thread-local
//!
//! Natives run on the mutator thread that entered them, so a per-thread table
//! needs no synchronisation at all — which matters, because the thing being
//! removed IS lock traffic. A shared table would reintroduce, on the same hot
//! path, the contention the memo exists to delete. The cost is that each
//! thread warms its own entries; on any workload where this matters that is a
//! handful of fills against millions of hits.
//!
//! # Why the key is verified by CONTENT
//!
//! The slot index is computed from the receiver id and a few bytes of the two
//! names — cheap, and deliberately NOT a hash of all three strings, which is
//! the cost being removed. The entry then compares the full `method_name` and
//! `descriptor`. Pointer identity would be cheaper still and is not sound: a
//! caller may pass a `&String` whose buffer was freed and reallocated at the
//! same address with the same length, and a wrong hit here runs a method on
//! the wrong declaring class. `native_id.rs`'s module header records the
//! `class_manager` defect of exactly that shape ("Round 4 audit fix (CRIT)");
//! this module does not repeat it.
//!
//! # Switch
//!
//! `CRATONVM_NATIVE_CALLBACK_MEMO=0` disables it, so the mechanism can be
//! priced in ONE binary. `CRATONVM_DBG_CALLBACK_MEMO=1` prints the engagement
//! census at exit — probes, hits, fills and the distinct triples memoised.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::classloading::ClassId;

/// Direct-mapped slots per thread. 256 short entries is ~16 KiB of `Box<str>`
/// in the worst case and covers every distinct callback triple any workload
/// measured here produces; a collision evicts, which degrades to the
/// pre-memo behaviour rather than to a wrong answer.
const SLOTS: usize = 256;

/// Kill switch (interpreter round i1 wave 18, lane L4). `true`: a redefinition
/// retires the memo through [`Stamp`]'s `redefinitions`. `false`: the
/// historical latch — `NativeContextImpl::invoke_virtual` neither consults nor
/// fills the memo for the rest of the process after the first redefinition of
/// any class.
pub(crate) const SURVIVES_REDEFINITION: bool = true;

/// The revalidation stamp. See the table in the module header: every entry is
/// one thing that can make a filled memo describe a dispatch that would no
/// longer happen.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    /// `NativeMethodRegistry::generation()` — a new native slot was appended.
    native_generation: u32,
    /// The VM's class store's `class_definition_epoch` (its `StoreEpochs`
    /// slot) — some name now resolves to another id. The store's own copy,
    /// so another VM's class definitions do not retire this VM's entries
    /// (interpreter round i1 wave 21, lane L5); every fill and probe of an
    /// entry reads it for the VM that `vm` below names.
    class_definition: u64,
    /// `class_origin_epoch()` — some class's provenance changed in place.
    class_origin: u64,
    /// The `SharedVm` this memo was taken against (`SharedVm::vm_identity`). A
    /// test binary that builds two VMs on one thread must not redeem the first
    /// one's `ClassId`s against the second's class store. Not the `SharedVm`
    /// ADDRESS: a VM dropped and the next one built in the same place (the
    /// next unit test on a harness thread, numbering its classes from 0 again)
    /// shared it, and redeemed the first VM's answers.
    vm: usize,
    /// `class_redefinition_count()` — a class was redefined in place. A
    /// redefinition only ever makes a registered native YIELD to bytecode (the
    /// redefine-shadow suppression is monotone per class, and a memo only
    /// ever records a bytecode tail), so an answer stays right across one;
    /// that is also what covers a fill whose dispatch straddled a
    /// redefinition (the stamp is taken at the fill). The stamp retires the
    /// entries filled before it regardless (interpreter round i1 wave 18,
    /// lane L4).
    redefinitions: u64,
}

struct Entry {
    receiver_class: u32,
    /// `Some(first-argument key)` for a STATIC entry ([`fill_static`]):
    /// `receiver_class` is then the OWNER class. Never redeemed by [`lookup`].
    static_key: Option<u32>,
    /// Owned copies, compared in full on every probe — see the module header.
    method_name: Box<str>,
    descriptor: Box<str>,
    stamp: Stamp,
    /// The class whose bytecode the ordinary tail actually ran.
    declaring_class: u32,
}

/// What [`arm`] recorded about the dispatch in flight, and what the tail wrote
/// back into it.
#[derive(Clone, Copy)]
enum Witness {
    /// No dispatch in flight is a fill candidate.
    Idle,
    /// A candidate is in flight. The three fields identify it so that a
    /// NESTED arrival at the tail — a `<clinit>` driven by
    /// `ensure_class_initialized_shared`, say — cannot claim this witness.
    Armed {
        receiver_class: u32,
        /// `Some(first-argument key)` for a STATIC candidate ([`arm_static`]):
        /// `receiver_class` is then the OWNER class, and only
        /// [`note_plain_static_tail`] may claim the witness.
        static_key: Option<u32>,
        name_ptr: usize,
        name_len: usize,
        desc_ptr: usize,
        desc_len: usize,
        /// [`NATIVE_DEPTH`] when the candidate was armed. The candidate's own
        /// tail is reached at the same depth — no registered native runs
        /// between `invoke_or_native` and the ordinary tail — so a tail
        /// arriving deeper is a nested callback's, however identical its
        /// receiver and string literals are.
        native_depth: u32,
        /// The Java frame depth (`thread.frames.len()`) when the candidate was
        /// armed. Nothing between the arm and the candidate's own tail leaves a
        /// frame pushed, so a tail reached from inside Java code that ran in
        /// between (a `<clinit>`, a callback body the dispatch ran, the
        /// interpreter's exotic-invoke fallback) arrives deeper — even when no
        /// native separates it from the arm and it passes the same interned
        /// name `Arc`s the outer call did.
        frame_depth: usize,
    },
    /// The candidate reached the ordinary tail, on this declaring class.
    Reached { declaring_class: u32 },
}

thread_local! {
    static TABLE: RefCell<Vec<Option<Entry>>> = RefCell::new(Vec::new());
    static WITNESS: Cell<Witness> = const { Cell::new(Witness::Idle) };
    /// How many registered natives are running on this thread right now —
    /// maintained by [`enter_native`] around `safe_native_call`'s funnel.
    static NATIVE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// One registered native running on this thread; see [`enter_native`].
pub struct NativeFrameGuard(());

impl Drop for NativeFrameGuard {
    #[inline]
    fn drop(&mut self) {
        NATIVE_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Count a registered native as running until the guard drops. Called from
/// the one native-dispatch funnel (`vm_exec::safe_native_call_impl`).
///
/// The count is what lets [`note_plain_bytecode_tail`] tell the armed
/// dispatch's own tail from a NESTED one with the same receiver and the same
/// string literals: a special arm that answers the outer call with a native,
/// whose body calls back on the same receiver by a route that does not arm
/// its own witness (`invoke_on_class_shared` straight from a native), used to
/// reach the ordinary tail and claim the outer candidate — filling a memo for
/// a dispatch that never ran bytecode. That callback runs one native deeper.
#[inline]
#[must_use = "the depth is decremented when the guard drops"]
pub fn enter_native() -> NativeFrameGuard {
    NATIVE_DEPTH.with(|d| d.set(d.get().wrapping_add(1)));
    NativeFrameGuard(())
}

/// Would a virtual thread that yields now discard a Rust frame's post-call
/// work? (Interpreter round i1 wave 26, lane L4; page
/// `docs/internal/fixed-bugs/interpreter-L4-a-virtual-thread-that-unmounts-under-a-native-loses-the-natives-return-conversion-FIXED-20260928.md`.)
///
/// A continuation is the thread's Java frames and nothing else: a yield
/// unwinds every Rust frame between the yielding native and the carrier, and
/// the remount runs the Java frames only. So a native that re-entered Java --
/// `MethodHandle.invoke` / `invokeExact`, a reflection native, anything that
/// reaches Java through `NativeContext::invoke_*` -- and is below the yielding
/// native on this stack would lose whatever it does after the call (the
/// method-handle natives' return conversion: a `void` target answered no
/// value into a frame promised an `Object`). HotSpot pins such a continuation:
/// the virtual thread blocks its carrier instead of unmounting.
///
/// The yielding native itself is one registered native ([`NATIVE_DEPTH`] 1,
/// counted by the funnel it was called through); a second one is a native
/// that re-entered Java. A mount starts at depth 0 (both mount paths --
/// `vm_exec::resume_virtual_continuation` on a carrier and a virtual thread's
/// first run on its spawned OS thread -- are Rust, not a native), so the
/// depth needs no per-mount baseline. Two counts kept in
/// `cratonvm_native_api::continuation_pin` refine it: a pin held by a
/// non-native Rust door with post-call work of its own ([`pin_continuation`])
/// pins, and a method-handle native that declared itself TRANSPARENT for its
/// nested call (its only post-call work is handing a reference back
/// unchanged; HotSpot has no native frame there) is not counted. Read by the
/// yield decisions (`vt_park_for`, `vt_sleep_for`, `vt_wait_on_key`,
/// `vt_pin_count`); never on a per-call path.
pub fn continuation_pinned_by_native() -> bool {
    use cratonvm_native_api::continuation_pin;
    NATIVE_DEPTH
        .with(Cell::get)
        .saturating_sub(continuation_pin::transparent_natives())
        > 1
        || continuation_pin::pins() > 0
}

/// Pin this thread's continuation until the guard drops: a virtual thread
/// that parks or sleeps meanwhile blocks its carrier instead of unmounting
/// ([`continuation_pinned_by_native`]). For a Rust door that runs Java in a
/// nested call and still has work to do when it returns -- the lambda door's
/// constructor reference, whose `<init>` returns `void` while the door owes
/// its caller the new object. The guard drops on every exit, a yield's
/// unwinding included.
#[inline]
pub fn pin_continuation() -> cratonvm_native_api::continuation_pin::PinGuard {
    cratonvm_native_api::continuation_pin::pin()
}

/// Engagement counters. Indices are [`PROBE`], [`HIT`], [`FILL`], [`EVICT`].
static COUNTS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
const PROBE: usize = 0;
const HIT: usize = 1;
const FILL: usize = 2;
const EVICT: usize = 3;

#[inline]
fn bump(kind: usize) {
    if crate::runtime::interp_census::callback_memo_enabled() {
        COUNTS[kind].fetch_add(1, Ordering::Relaxed);
    }
}

/// Slot index. Cheap on purpose: the receiver id, the two lengths and four
/// sampled bytes. Hashing all three strings is the cost this module removes,
/// and `completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`
/// §3 already priced "shave the hash instead of removing it" at 0.995x.
#[inline]
fn slot(receiver_class: u32, method_name: &str, descriptor: &str) -> usize {
    let n = method_name.as_bytes();
    let d = descriptor.as_bytes();
    let mut h = (receiver_class as usize).wrapping_mul(0x9E37_79B9);
    h ^= n.len().wrapping_shl(3) ^ d.len().wrapping_shl(11);
    if let Some(b) = n.first() {
        h ^= (*b as usize) << 5;
    }
    if let Some(b) = n.last() {
        h ^= (*b as usize) << 13;
    }
    if let Some(b) = d.last() {
        h ^= (*b as usize) << 19;
    }
    (h ^ (h >> 9)) & (SLOTS - 1)
}

#[inline]
fn stamp(shared: &crate::vm::SharedVm) -> Stamp {
    Stamp {
        native_generation: shared.natives.native_methods.generation(),
        class_definition: cratonvm_classloading::store_epochs(shared.jit.class_layout_domain)
            .class_definition_epoch(),
        class_origin: cratonvm_classloading::class_origin_epoch(),
        vm: shared.vm_identity,
        redefinitions: crate::classloading::class_redefinition_count(),
    }
}

/// The memoised declaring class for this callback, if one is valid.
///
/// A `Some` answer means: the last time this exact `(VM, receiver class,
/// method, descriptor)` was dispatched through `NativeContextImpl::invoke_virtual`
/// it reached the ordinary bytecode tail on this declaring class, and nothing
/// that could change that has happened since.
#[inline]
pub fn lookup(
    shared: &crate::vm::SharedVm,
    receiver_class: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Option<ClassId> {
    bump(PROBE);
    let receiver_class = receiver_class.as_u32();
    let want = stamp(shared);
    let idx = slot(receiver_class, method_name, descriptor);
    let hit = TABLE.with(|t| {
        let table = t.borrow();
        let entry = table.get(idx)?.as_ref()?;
        (entry.receiver_class == receiver_class
            && entry.static_key.is_none()
            && entry.stamp == want
            && &*entry.method_name == method_name
            && &*entry.descriptor == descriptor)
            .then_some(entry.declaring_class)
    })?;
    bump(HIT);
    Some(ClassId::new(hit))
}

/// Mark the dispatch about to run as a fill candidate, returning the previous
/// witness for the caller to restore.
///
/// Saved and restored rather than simply overwritten because the dispatch can
/// re-enter: a class initialiser, a lambda body, or a nested native callback
/// runs between this call and the tail. The restore is what keeps the outer
/// candidate alive across them.
///
/// `frame_depth` is the calling thread's `frames.len()`; the tail must report
/// the same depth to claim the witness (see `Witness::Armed::frame_depth`).
#[inline]
#[must_use = "the previous witness must be restored by `disarm`"]
pub fn arm(
    receiver_class: ClassId,
    method_name: &str,
    descriptor: &str,
    frame_depth: usize,
) -> WitnessSave {
    let previous = WITNESS.with(|w| {
        w.replace(Witness::Armed {
            receiver_class: receiver_class.as_u32(),
            static_key: None,
            name_ptr: method_name.as_ptr() as usize,
            name_len: method_name.len(),
            desc_ptr: descriptor.as_ptr() as usize,
            desc_len: descriptor.len(),
            native_depth: NATIVE_DEPTH.with(Cell::get),
            frame_depth,
        })
    });
    WitnessSave(previous)
}

/// The witness [`arm`] displaced. Opaque so it cannot be forged.
pub struct WitnessSave(Witness);

/// Restore the displaced witness and report whether the armed dispatch reached
/// the ordinary tail.
#[inline]
pub fn disarm(save: WitnessSave) -> Option<ClassId> {
    let outcome = WITNESS.with(|w| w.replace(save.0));
    match outcome {
        Witness::Reached { declaring_class } => Some(ClassId::new(declaring_class)),
        _ => None,
    }
}

/// Called from the ONE ordinary-tail site — the `interpreter::execute` at the
/// bottom of `invoke_on_class_shared_inner`, reached only when no override,
/// no proxy and no special arm applied.
///
/// `is_plain` is the tail's own two facts about the callee: it is neither
/// `ACC_NATIVE` nor `ACC_SYNCHRONIZED`. A synchronized callee's monitor is
/// taken by that function's `_sync_guard`, which the fast path does not have,
/// so those are never memoised.
///
/// The identity check is why a nested arrival cannot steal the witness: a
/// `<clinit>` driven from inside this dispatch reaches the same tail with a
/// different name, and a nested native callback armed its own witness before
/// getting here.
///
/// # The receiver is part of the identity
///
/// The name and descriptor are compared BY ADDRESS, and a native's string
/// literals are deduplicated: `native_hs_equals` calls `other.size()` and the
/// `size` native it lands on calls `backing.size()`, with the SAME `"size"` /
/// `"()I"` constants at the SAME addresses. When that inner call reaches the
/// tail by a route that does not arm its own witness, it matched the OUTER
/// candidate's strings and claimed it, and the outer dispatch filled
/// `(LinkedHashSet, size, ()I) -> HashMap` -- a memo that then ran
/// `HashMap.size` bytecode on a one-slot `LinkedHashSet` for every later call
/// (`zgc real: field index OOB index=4 num_slots=1`), so a `HashSet`'s
/// `equals` answered false. MEASURED 2026-09-18 under `--jdk-only`
/// (`ProfilesTests.sensibleEquals`, and a two-line reproducer: a class whose
/// `equals` is `o instanceof PP that && ex.equals(that.ex)` over two
/// `LinkedHashSet`s answers true the first time and false ever after). The
/// tail's receiver has to be the armed dispatch's receiver, so `receiver_class`
/// is the class of the tail's own receiver (`None` for a static callee or a
/// null receiver, which never matches).
///
/// # So is the Java frame depth
///
/// `frame_depth` is the tail thread's `frames.len()`. The native depth above
/// separates a nested callback made from inside a native; it cannot separate
/// one made from inside JAVA code that the dispatch ran on the same native
/// level — a `<clinit>`, a body a special arm executed, whose own invoke
/// reaches this tail through the interpreter's exotic fallback with the same
/// receiver class and, since method names are shared `Arc<str>`s, possibly the
/// same name and descriptor addresses. That arrival is at least one frame
/// deeper than the arm; the candidate's own tail is at the arm's depth.
#[inline]
pub fn note_plain_bytecode_tail(
    declaring_class: ClassId,
    receiver_class: Option<ClassId>,
    method_name: &str,
    descriptor: &str,
    is_plain: bool,
    frame_depth: usize,
) {
    if !is_plain {
        return;
    }
    WITNESS.with(|w| {
        if let Witness::Armed {
            receiver_class: armed_receiver,
            static_key: None,
            name_ptr,
            name_len,
            desc_ptr,
            desc_len,
            native_depth,
            frame_depth: armed_frame_depth,
        } = w.get()
        {
            if native_depth == NATIVE_DEPTH.with(Cell::get)
                && armed_frame_depth == frame_depth
                && receiver_class.map(ClassId::as_u32) == Some(armed_receiver)
                && name_ptr == method_name.as_ptr() as usize
                && name_len == method_name.len()
                && desc_ptr == descriptor.as_ptr() as usize
                && desc_len == descriptor.len()
            {
                w.set(Witness::Reached {
                    declaring_class: declaring_class.as_u32(),
                });
            }
        }
    });
}

/// Record a verified outcome. Only ever called with a `declaring_class` that
/// [`disarm`] reported, i.e. one the resolver actually reached.
pub fn fill(
    shared: &crate::vm::SharedVm,
    receiver_class: ClassId,
    method_name: &str,
    descriptor: &str,
    declaring_class: ClassId,
) {
    let receiver_class = receiver_class.as_u32();
    let idx = slot(receiver_class, method_name, descriptor);
    let entry = Entry {
        receiver_class,
        static_key: None,
        method_name: method_name.into(),
        descriptor: descriptor.into(),
        stamp: stamp(shared),
        declaring_class: declaring_class.as_u32(),
    };
    TABLE.with(|t| {
        let mut table = t.borrow_mut();
        if table.is_empty() {
            table.resize_with(SLOTS, || None);
        }
        if table[idx].is_some() {
            bump(EVICT);
        }
        table[idx] = Some(entry);
    });
    bump(FILL);
}

/// A static candidate's first argument when it is not an object (a primitive,
/// `null`, or no argument at all).
pub const STATIC_FIRST_NOT_AN_OBJECT: u32 = u32::MAX;

/// The first-argument half of a STATIC key, or `None` when the call may not be
/// memoised (round 13 wave 11, lane callcost5; the static half of this memo,
/// served to a settled `REF_invokeStatic` handle by
/// `vm_exec::invoke_static_settling`). `invoke_on_class_shared_inner`'s
/// argument-keyed arms all read `args[0]`, so its class is part of the key; an
/// ARRAY there carries its component's class id (the `KC26 array.clone()`
/// aliasing) and a LAMBDA PROXY is offered to `try_lambda_dispatch` whatever
/// the callee, so both decline.
pub fn static_first_arg_key(
    shared: &crate::vm::SharedVm,
    args: &[cratonvm_types::Value],
) -> Option<u32> {
    match args.first() {
        Some(cratonvm_types::Value::Object(Some(o))) => {
            if shared.mem.heap.kind_of(*o) == cratonvm_types::ObjectKind::Array {
                return None;
            }
            let cid = shared.mem.heap.class_id_of(*o);
            if shared.classes.is_lambda_proxy_class(cid) {
                return None;
            }
            Some(cid.as_u32())
        }
        _ => Some(STATIC_FIRST_NOT_AN_OBJECT),
    }
}

#[inline]
fn static_slot(owner: u32, first_key: u32, method_name: &str, descriptor: &str) -> usize {
    slot(owner ^ first_key.rotate_left(16) ^ 0x5A5A_0000, method_name, descriptor)
}

/// [`arm`] for a STATIC callee dispatched on `owner` (the class
/// `invoke_on_class_shared` is handed), `first_key` from
/// [`static_first_arg_key`].
#[inline]
#[must_use = "the previous witness must be restored by `disarm`"]
pub fn arm_static(
    owner: ClassId,
    first_key: u32,
    method_name: &str,
    descriptor: &str,
    frame_depth: usize,
) -> WitnessSave {
    let previous = WITNESS.with(|w| {
        w.replace(Witness::Armed {
            receiver_class: owner.as_u32(),
            static_key: Some(first_key),
            name_ptr: method_name.as_ptr() as usize,
            name_len: method_name.len(),
            desc_ptr: descriptor.as_ptr() as usize,
            desc_len: descriptor.len(),
            native_depth: NATIVE_DEPTH.with(Cell::get),
            frame_depth,
        })
    });
    WitnessSave(previous)
}

/// [`note_plain_bytecode_tail`] for a STATIC callee: claims only a static
/// candidate armed for this owner, this first-argument key, these strings (by
/// address), at this native and frame depth.
#[inline]
pub fn note_plain_static_tail(
    declaring_class: ClassId,
    owner: ClassId,
    first_key: Option<u32>,
    method_name: &str,
    descriptor: &str,
    is_plain: bool,
    frame_depth: usize,
) {
    let Some(first_key) = first_key.filter(|_| is_plain) else {
        return;
    };
    WITNESS.with(|w| {
        if let Witness::Armed {
            receiver_class,
            static_key: Some(armed_key),
            name_ptr,
            name_len,
            desc_ptr,
            desc_len,
            native_depth,
            frame_depth: armed_frame_depth,
        } = w.get()
        {
            if native_depth == NATIVE_DEPTH.with(Cell::get)
                && armed_frame_depth == frame_depth
                && receiver_class == owner.as_u32()
                && armed_key == first_key
                && name_ptr == method_name.as_ptr() as usize
                && name_len == method_name.len()
                && desc_ptr == descriptor.as_ptr() as usize
                && desc_len == descriptor.len()
            {
                w.set(Witness::Reached {
                    declaring_class: declaring_class.as_u32(),
                });
            }
        }
    });
}

/// [`lookup`] for a STATIC key.
#[inline]
pub fn lookup_static(
    shared: &crate::vm::SharedVm,
    owner: ClassId,
    first_key: u32,
    method_name: &str,
    descriptor: &str,
) -> Option<ClassId> {
    bump(PROBE);
    let owner = owner.as_u32();
    let want = stamp(shared);
    let idx = static_slot(owner, first_key, method_name, descriptor);
    let hit = TABLE.with(|t| {
        let table = t.borrow();
        let entry = table.get(idx)?.as_ref()?;
        (entry.receiver_class == owner
            && entry.static_key == Some(first_key)
            && entry.stamp == want
            && &*entry.method_name == method_name
            && &*entry.descriptor == descriptor)
            .then_some(entry.declaring_class)
    })?;
    bump(HIT);
    Some(ClassId::new(hit))
}

/// [`fill`] for a STATIC key.
pub fn fill_static(
    shared: &crate::vm::SharedVm,
    owner: ClassId,
    first_key: u32,
    method_name: &str,
    descriptor: &str,
    declaring_class: ClassId,
) {
    let owner = owner.as_u32();
    let idx = static_slot(owner, first_key, method_name, descriptor);
    let entry = Entry {
        receiver_class: owner,
        static_key: Some(first_key),
        method_name: method_name.into(),
        descriptor: descriptor.into(),
        stamp: stamp(shared),
        declaring_class: declaring_class.as_u32(),
    };
    TABLE.with(|t| {
        let mut table = t.borrow_mut();
        if table.is_empty() {
            table.resize_with(SLOTS, || None);
        }
        if table[idx].is_some() {
            bump(EVICT);
        }
        table[idx] = Some(entry);
    });
    bump(FILL);
}

/// `[probes, hits, fills, evictions]`, for the exit census.
pub fn census() -> [u64; 4] {
    [
        COUNTS[PROBE].load(Ordering::Relaxed),
        COUNTS[HIT].load(Ordering::Relaxed),
        COUNTS[FILL].load(Ordering::Relaxed),
        COUNTS[EVICT].load(Ordering::Relaxed),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The VM half of the stamp is the VM's never-reused identity, not its
    /// address, so a VM built where a dropped one lived cannot redeem the
    /// dropped VM's entries (the same class ids name other classes there).
    #[test]
    fn a_memo_is_scoped_to_the_vm_identity() {
        let first = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        assert_eq!(stamp(&first).vm, first.vm_identity);
        fill(&first, ClassId::new(5), "run", "()V", ClassId::new(6));
        let second = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        assert_ne!(stamp(&first).vm, stamp(&second).vm);
        assert_eq!(lookup(&second, ClassId::new(5), "run", "()V"), None);
    }

    /// Interpreter round i1 wave 21 (lane L5): the definition half of the
    /// stamp is the VM's own class store's, so a class defined in ANOTHER VM
    /// keeps this VM's entry, and one defined in this VM retires it.
    #[test]
    fn only_this_vms_class_definitions_retire_its_entries() {
        let mine = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let other = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        if cratonvm_classloading::store_epoch_slot(mine.jit.class_layout_domain)
            == cratonvm_classloading::store_epoch_slot(other.jit.class_layout_domain)
        {
            // The two stores share an epoch slot: the documented
            // conservative case, nothing to assert.
            return;
        }
        // A name bound in the VM's namespace: the class-definition event the
        // stamp's `class_definition` half tracks, and nothing else. (A
        // fabricated synthetic class would also rewrite a provenance in place,
        // which moves the process-wide `class_origin_epoch` and retires every
        // VM's entries on purpose.)
        let define = |vm: &crate::vm::SharedVm, name: &str| {
            vm.classes.class_manager.write().register_class_name(
                crate::classloading::ClassLoaderId::Application,
                name,
                ClassId::new(1),
            );
        };
        let (receiver, name, desc) = (21_005u32, "w21L5", "()V");
        let mut checked = false;
        for round in 0..50 {
            // Taken BEFORE the fill, so an unchanged stamp afterwards proves
            // the fill carried it too.
            let before = stamp(&mine);
            fill(&mine, ClassId::new(receiver), name, desc, ClassId::new(7));
            define(&other, format!("cratonvm/test/W21L5Other{round}").as_str());
            if stamp(&mine) != before {
                // A process stamp moved, or a concurrent test's store shares
                // this VM's slot.
                continue;
            }
            assert_eq!(
                lookup(&mine, ClassId::new(receiver), name, desc),
                Some(ClassId::new(7)),
                "another VM's class definition must not retire this VM's entry"
            );
            checked = true;
            break;
        }
        assert!(checked, "no quiet window for this VM's stamp");
        let before = stamp(&mine);
        define(&mine, "cratonvm/test/W21L5Mine");
        assert_ne!(
            stamp(&mine).class_definition,
            before.class_definition,
            "a definition in this VM moves its store's epoch"
        );
        assert_eq!(lookup(&mine, ClassId::new(receiver), name, desc), None);
    }

    #[test]
    fn a_slot_index_is_stable_and_spread() {
        // The same triple always lands in the same slot...
        assert_eq!(
            slot(7, "postComplete", "()V"),
            slot(7, "postComplete", "()V")
        );
        // ...and the receiver class is part of it, which is what keeps two
        // classes' answers apart.
        let a = slot(7, "postComplete", "()V");
        let b = slot(8, "postComplete", "()V");
        assert_ne!(a, b, "the receiver id must move the slot");
        // Empty strings must not panic — `slot` samples first/last bytes.
        let _ = slot(0, "", "");
    }

    #[test]
    fn the_witness_is_only_claimed_by_its_own_triple() {
        let name = "postComplete";
        let desc = "()V";
        let save = arm(ClassId::new(42), name, desc, 3);
        // A nested arrival with a DIFFERENT triple (a `<clinit>`, say) must
        // not claim it.
        note_plain_bytecode_tail(ClassId::new(99), None, "<clinit>", "()V", true, 3);
        // ...nor may an arrival with the right strings but a non-plain callee.
        note_plain_bytecode_tail(
            ClassId::new(99),
            Some(ClassId::new(42)),
            name,
            desc,
            false,
            3,
        );
        note_plain_bytecode_tail(
            ClassId::new(77),
            Some(ClassId::new(42)),
            name,
            desc,
            true,
            3,
        );
        assert_eq!(disarm(save), Some(ClassId::new(77)));
        // And the witness is back to idle, so a later tail claims nothing.
        let save = arm(ClassId::new(1), name, desc, 3);
        assert_eq!(disarm(save), None);
    }

    #[test]
    fn a_nested_arm_restores_the_outer_candidate() {
        let name = "postComplete";
        let desc = "()V";
        let outer = arm(ClassId::new(42), name, desc, 3);
        {
            let inner = arm(ClassId::new(43), "inner", "()I", 3);
            note_plain_bytecode_tail(
                ClassId::new(43),
                Some(ClassId::new(43)),
                "inner",
                "()I",
                true,
                3,
            );
            assert_eq!(disarm(inner), Some(ClassId::new(43)));
        }
        // The outer candidate survived the nested dispatch.
        note_plain_bytecode_tail(
            ClassId::new(42),
            Some(ClassId::new(42)),
            name,
            desc,
            true,
            3,
        );
        assert_eq!(disarm(outer), Some(ClassId::new(42)));
    }

    #[test]
    fn a_tail_one_native_deeper_does_not_claim_the_outer_witness() {
        // The outer call is answered by a special arm's native, whose body
        // calls back on the SAME receiver with the SAME literals by a route
        // that arms nothing. That tail is one native deeper than the arm.
        let name = "size";
        let desc = "()I";
        let save = arm(ClassId::new(10), name, desc, 3);
        {
            let _native = enter_native();
            note_plain_bytecode_tail(
                ClassId::new(30),
                Some(ClassId::new(10)),
                name,
                desc,
                true,
                3,
            );
        }
        assert_eq!(disarm(save), None);
        // At the arming depth the same tail is the candidate's own.
        let save = arm(ClassId::new(10), name, desc, 3);
        note_plain_bytecode_tail(
            ClassId::new(30),
            Some(ClassId::new(10)),
            name,
            desc,
            true,
            3,
        );
        assert_eq!(disarm(save), Some(ClassId::new(30)));
        // And arming inside a native compares against THAT depth.
        let _outer_native = enter_native();
        let save = arm(ClassId::new(10), name, desc, 3);
        note_plain_bytecode_tail(
            ClassId::new(31),
            Some(ClassId::new(10)),
            name,
            desc,
            true,
            3,
        );
        assert_eq!(disarm(save), Some(ClassId::new(31)));
    }

    #[test]
    fn a_tail_on_another_receiver_with_the_same_strings_does_not_claim() {
        // The `size` -> `backing.size()` shape: same literals, same addresses,
        // but the tail's receiver is the BACKING MAP, not the armed set.
        let name = "size";
        let desc = "()I";
        let save = arm(ClassId::new(10), name, desc, 3);
        note_plain_bytecode_tail(
            ClassId::new(20),
            Some(ClassId::new(20)),
            name,
            desc,
            true,
            3,
        );
        assert_eq!(disarm(save), None);
    }

    #[test]
    fn a_tail_from_deeper_java_code_does_not_claim_the_outer_witness() {
        // No native separates the nested arrival from the arm (same native
        // depth), and it names the same receiver class and the same string
        // addresses — but it comes from inside Java code the dispatch ran, so
        // it is at least one frame deeper than the arm.
        let name = "size";
        let desc = "()I";
        let save = arm(ClassId::new(10), name, desc, 3);
        note_plain_bytecode_tail(
            ClassId::new(30),
            Some(ClassId::new(10)),
            name,
            desc,
            true,
            4,
        );
        assert_eq!(disarm(save), None);
        // The candidate's own tail, at the arm's frame depth, still claims it,
        // and a deeper nested arrival first does not spoil that.
        let save = arm(ClassId::new(10), name, desc, 3);
        note_plain_bytecode_tail(
            ClassId::new(31),
            Some(ClassId::new(10)),
            name,
            desc,
            true,
            5,
        );
        note_plain_bytecode_tail(
            ClassId::new(30),
            Some(ClassId::new(10)),
            name,
            desc,
            true,
            3,
        );
        assert_eq!(disarm(save), Some(ClassId::new(30)));
    }

    /// Interpreter round i1 wave 18, lane L4: an entry filled before a class
    /// redefinition is not redeemed after it, and the memo keeps serving what
    /// is filled afterwards (it used to be switched off for the process).
    /// The process count is not moved here (other tests fill memos in this
    /// process); an entry is planted with an earlier count instead.
    #[test]
    fn an_entry_filled_before_a_redefinition_is_not_redeemed() {
        let vm = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let (receiver, name, desc) = (18_431u32, "i18L4", "()V");
        let now = stamp(&vm);
        assert_eq!(
            now.redefinitions,
            crate::classloading::class_redefinition_count()
        );
        TABLE.with(|t| {
            let mut table = t.borrow_mut();
            if table.is_empty() {
                table.resize_with(SLOTS, || None);
            }
            table[slot(receiver, name, desc)] = Some(Entry {
                receiver_class: receiver,
                static_key: None,
                method_name: name.into(),
                descriptor: desc.into(),
                stamp: Stamp {
                    redefinitions: now.redefinitions.wrapping_sub(1),
                    ..now
                },
                declaring_class: 7,
            });
        });
        assert_eq!(lookup(&vm, ClassId::new(receiver), name, desc), None);
        fill(&vm, ClassId::new(receiver), name, desc, ClassId::new(7));
        if stamp(&vm) == now {
            assert_eq!(
                lookup(&vm, ClassId::new(receiver), name, desc),
                Some(ClassId::new(7))
            );
        }
    }

    /// Round 13 wave 11 (lane callcost5): a static witness is claimed only by
    /// the static tail of its own owner and first-argument key, never by an
    /// instance tail, and an instance witness never by a static tail.
    #[test]
    fn a_static_witness_is_only_claimed_by_its_own_static_tail() {
        let (name, desc) = ("twice", "(I)I");
        let (owner, decl, other) = (ClassId::new(40), ClassId::new(41), ClassId::new(99));
        let not_obj = STATIC_FIRST_NOT_AN_OBJECT;
        let key = Some(not_obj);
        let save = arm_static(owner, not_obj, name, desc, 3);
        // An instance tail, another first-argument key, another owner, another
        // frame depth, a non-plain callee, a declined key: none claims it.
        note_plain_bytecode_tail(decl, Some(owner), name, desc, true, 3);
        note_plain_static_tail(decl, owner, Some(7), name, desc, true, 3);
        note_plain_static_tail(decl, other, key, name, desc, true, 3);
        note_plain_static_tail(decl, owner, key, name, desc, true, 4);
        note_plain_static_tail(decl, owner, key, name, desc, false, 3);
        note_plain_static_tail(decl, owner, None, name, desc, true, 3);
        assert_eq!(disarm(save), None);
        let save = arm_static(owner, not_obj, name, desc, 3);
        note_plain_static_tail(decl, owner, key, name, desc, true, 3);
        assert_eq!(disarm(save), Some(decl));
        // An instance witness is never claimed by a static tail.
        let save = arm(owner, name, desc, 3);
        note_plain_static_tail(decl, owner, key, name, desc, true, 3);
        assert_eq!(disarm(save), None);
    }

    /// A static entry and an instance entry of the same ids and strings are
    /// different entries.
    #[test]
    fn static_and_instance_entries_do_not_alias() {
        let vm = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let now = stamp(&vm);
        fill_static(&vm, ClassId::new(12_010), 5, "w11cc5", "()V", ClassId::new(9));
        if stamp(&vm) == now {
            assert_eq!(lookup(&vm, ClassId::new(12_010), "w11cc5", "()V"), None);
            assert_eq!(lookup_static(&vm, ClassId::new(12_010), 6, "w11cc5", "()V"), None);
            assert_eq!(
                lookup_static(&vm, ClassId::new(12_010), 5, "w11cc5", "()V"),
                Some(ClassId::new(9))
            );
        }
    }

    /// Wave 26: the yielding native alone does not pin; a native under it
    /// (one that re-entered Java) or a `pin_continuation` door does, and each
    /// guard releases what it took.
    #[test]
    fn a_native_under_the_yielding_one_pins_the_continuation() {
        assert!(!continuation_pinned_by_native());
        let outer = enter_native();
        assert!(!continuation_pinned_by_native(), "the yielding native alone");
        {
            let _inner = enter_native();
            assert!(continuation_pinned_by_native(), "a native re-entered Java");
        }
        assert!(!continuation_pinned_by_native());
        {
            let _door = pin_continuation();
            assert!(continuation_pinned_by_native(), "a pinning door");
        }
        {
            // A method-handle door (`outer`) that declared its nested call
            // transparent, and the native that yields beneath it.
            let _transparent = cratonvm_native_api::continuation_pin::transparent();
            let _yielder = enter_native();
            assert!(!continuation_pinned_by_native(), "a transparent door");
            let _opaque = enter_native();
            let _yielder_deeper = enter_native();
            assert!(continuation_pinned_by_native(), "an opaque native beneath it");
        }
        drop(outer);
        assert!(!continuation_pinned_by_native());
    }
}
