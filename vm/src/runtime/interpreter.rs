// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Bytecode interpreter — the heart of the VM.
//!
//! Executes JVM bytecode instructions in a loop, handling:
//! - Local variable loads/stores
//! - Operand stack manipulation
//! - Arithmetic and type conversions
//! - Control flow (branches, returns)
//! - Object creation and method invocation
//! - Exception throw/catch via exception table
//!
//! # Error-handling discipline (NEW-7)
//!
//! This file is on the JVM execution hot path and must NEVER panic in
//! production builds. A stray `unwrap`, `expect`, or `panic!` here would
//! tear down the entire VM for what should be a recoverable `VmError`.
//!
//! The `#![cfg_attr(not(test), deny(...))]` gate below makes clippy refuse
//! to compile this module in a release build when any of the following
//! appear in non-test code:
//!
//! - `.unwrap()` / `.expect()` — use `?` against [`crate::error::VmError`]
//!   or an explicit `match` with a typed error instead.
//! - `panic!()` / `unimplemented!()` / `todo!()` — convert to a
//!   `VmError::Internal` or the appropriate `RuntimeError::*` variant.
//! - `unreachable!()` — if the arm is truly unreachable given JVMS
//!   invariants, annotate it with a `// SAFETY:` comment and use the
//!   `crate::runtime::unreachable_invariant!()` macro which returns a
//!   typed error rather than panicking.
//!
//! Tests inside `#[cfg(test)] mod tests { ... }` are exempt — assertion
//! panics are the standard Rust test failure mechanism and the gate only
//! applies to `not(test)` compilations.

#![cfg_attr(not(test), deny(clippy::panic, clippy::unimplemented, clippy::todo,))]
// T1.8.5 — every `unsafe {}` block in this file now carries a
// `// SAFETY:` comment (27 total after the T1 fourth-pass backfill).
// Promoted from `warn` to `deny` so any new unsafe block without a
// comment fails CI.

use std::sync::Arc;

use cratonvm_native_api::{
    NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
};
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::constant_pool::ConstantPoolEntry;
use cratonvm_reader::instruction::Instruction;
use tracing::trace;

use crate::classloading::resolution::{
    CachedBytecodeMethod, CachedInvokeTarget as GenericCachedInvokeTarget, MethodHandleKind,
    RedefineGate, ResolvedField, ResolvedMethod,
};
use crate::classloading::{find_field_recursive, ClassId};
use crate::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use crate::jit::profile::MethodKey;
use crate::memory::gc::{update_all_roots, update_value_ref};
use crate::memory::heap::ArrayElementType;
use crate::memory::roots::collect_roots;
use crate::runtime::exceptions::convert_class_not_found;
use crate::runtime::frame::Frame;
use crate::runtime::redefine_state::{
    class_was_redefined, hierarchy_fingerprint_in, named_class_was_redefined,
    native_shadow_suppressed_in,
};
use crate::threading::jvm_thread::JvmThread;
use crate::types::{CompactTag, CompactValue, ObjectRef, Value};
use crate::vm::{
    coerce_value_for_return, coerce_value_for_return_validated, create_java_string,
    create_java_string_from_units, ensure_class_initialized_shared, ensure_system_stdin_object,
    get_or_create_class_mirror, get_static_shared, invoke_on_class_shared, invoke_or_native,
    invoke_shared, read_java_string, set_static_shared, SharedVm,
};

type CachedInvokeTarget = GenericCachedInvokeTarget<cratonvm_jit::RetainedCode>;

// ---------------------------------------------------------------------------
// GC trigger helper
// ---------------------------------------------------------------------------

/// Check if the heap needs garbage collection, and if so, run a full GC cycle.
///
/// This should be called after any allocation instruction (new, newarray,
/// anewarray, multianewarray). It collects roots from the current thread
/// and shared VM state, runs the Cheney copying collector, and updates
/// all references in-place.
///
/// In multi-threaded mode, this coordinates with other threads via the GC barrier:
/// 1. The initiating thread requests stop-the-world
/// 2. Other threads deposit their root snapshots and pause
/// 3. The initiator collects all roots and runs GC
/// 4. All threads update their own frame references from the pointer map
/// Memoized `class_id -> is a watched EC holder` decision for the
/// `CRATONVM_DBG_ECWATCH` software watchpoint (only the few-instance curve /
/// asn1.x9 holders). The class-name resolution happens once per class id;
/// subsequent reference-field stores pay only a hashmap lookup, so the
/// per-ref-putfield overhead stays negligible.
/// Cached `CRATONVM_DBG_STRAYSTACK` gate (bc math-ec `0x4`): dump the Java
/// stack at a putfield whose receiver header is the stray/stale signature.
#[inline]
fn straystack_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STRAYSTACK").is_some())
}

/// Cached `CRATONVM_DBG_ARRSTORE` gate (bc math-ec 0x4 smear hunt): validate
/// primitive-array-store receivers at the write.
#[inline]
fn arrstore_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ARRSTORE").is_some())
}

/// Cached `CRATONVM_DBG_CCE_BT` gate (WildFly `parallel-extension-add` CCE
/// family): print receiver identity (class + address, and `via_pin` where
/// applicable) at the moment a `ClassCastException` is constructed, so a
/// wrong-object read can be correlated against GC cycle logs and the
/// `CRATONVM_DBG_STALE_OBJREF` quarantine ring. The native-collections
/// natural-order sites have a matching hook (with native backtrace) behind
/// the same variable.
#[inline]
pub fn dbg_cce_bt_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CCE_BT").is_some())
}

/// Cached `CRATONVM_DBG_NO_REFPROC` gate (bc math-ec 0x4): skip ALL post-GC
/// reference processing — subsystem-level exclusion experiment.
#[inline]
fn no_refproc() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NO_REFPROC").is_some())
}

/// HIB-CV-24 (Manifestation B) — end-to-end Weak/Phantom reference *clearing*.
///
/// CratonVM's mark phase traces every reference slot strongly, including a
/// `java.lang.ref.Reference`'s `referent`, so a *live* Weak/Phantom reference
/// pins its referent forever and `get()` never returns null / phantom queues
/// never fire (e.g. Hibernate's `ClassLoaderLeaksUtilityTest`). When ON
/// (default), the VM nulls Weak/Phantom referent slots *before* a collection so
/// the unmodified marker cannot keep the referent alive, lets the existing
/// reference processor clear/enqueue dead ones, then restores the slots of
/// survivors afterwards. Opt-out `CRATONVM_WEAKREF_CLEAR=0` restores the legacy
/// (never-clearing) behavior — byte-identical, the safety net.
fn weakref_clear_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_WEAKREF_CLEAR")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

thread_local! {
    /// Armed by the allocation-failure escalation ladder immediately before the
    /// collection it runs as its final attempt, so that collection clears every
    /// SoftReference rather than only the idle ones — the `java.lang.ref`
    /// last-ditch guarantee (see
    /// `ReferenceProcessor::condemn_all_soft_refs`).
    ///
    /// Thread-local rather than a process-global flag for two reasons. It is
    /// armed and read on ONE thread: `last_ditch_reclaim` arms it, calls
    /// `maybe_gc_forced`, and that function either initiates the collection on
    /// this same thread — running `weakref_null_referents_pre_gc` here, where
    /// the flag is visible — or declines because another thread is already
    /// collecting, in which case no collection of ours happens and the flag
    /// must not affect anyone else's. And a process can host more than one VM,
    /// which a `static AtomicBool` would silently share; nothing about
    /// "this thread is out of memory" belongs to a sibling VM.
    static LAST_DITCH_SOFT_CLEAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with the last-ditch soft-clear rule armed for any collection it
/// initiates. Restores the previous value on the way out, including on unwind,
/// so a panic inside the collection cannot leave the rule latched on for every
/// later GC on this thread.
pub(crate) fn with_last_ditch_soft_clear<R>(f: impl FnOnce() -> R) -> R {
    struct Disarm(bool);
    impl Drop for Disarm {
        fn drop(&mut self) {
            LAST_DITCH_SOFT_CLEAR.with(|c| c.set(self.0));
        }
    }
    let _restore = Disarm(LAST_DITCH_SOFT_CLEAR.with(|c| c.replace(true)));
    f()
}

/// Whether the collection about to run is the allocation failure's last
/// attempt. See [`with_last_ditch_soft_clear`].
fn last_ditch_soft_clear_armed() -> bool {
    LAST_DITCH_SOFT_CLEAR.with(|c| c.get())
}

/// `CRATONVM_DBG_WEAKREF` — trace the Weak/Phantom referent null/restore passes.
fn dbg_weakref() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_WEAKREF").is_some())
}

/// HIB-CV-24 — pre-collection pass: null the `referent` slot of every active
/// Weak/Phantom reference so the mark phase does NOT keep the referent alive
/// through the (live) Reference object. Surviving referents are restored by
/// `process_references_after_gc`. MUST run under STW, after `collect_roots` and
/// immediately before `collect_garbage` (the Reference objects are still at
/// their pre-collection addresses and no mutator can observe the transient
/// null). No-op when the gate is off or no Weak/Phantom references exist.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn weakref_null_referents_pre_gc(shared: &SharedVm) {
    if !weakref_clear_enabled() {
        return;
    }
    // Heap headroom for the soft-reference LRU policy below. Read BEFORE the
    // reference-processor lock is taken -- the heap's own locks rank above L7.
    let soft_free_mb = shared.mem.heap.soft_ref_policy_free_mb();
    let (pairs, soft_pairs) = {
        let mut rp = shared.mem.ref_processor.lock();
        let weak_phantom = rp.weak_phantom_active_triples();
        // SOFT-CLEAR GAP (2026-08-15). This is the measured answer to the
        // residual left by the retired `zgc-resourceleakdetector-corpse-read`
        // write-up: "the marker traces referents as strong edges; whether the
        // VM-level pass compensates is not established". It compensates for
        // WEAK and PHANTOM -- that is what the loop below does. It did NOT for
        // SOFT, on any collector: `process_soft_refs` skips an entry whose
        // referent `is_marked`, and a soft referent is always marked through
        // its own `SoftReference`'s slot 0, so the LRU policy beneath that
        // check could never fire. In a 64 MiB heap HotSpot cleared the soft
        // reference and allocated 30 MiB past it while CratonVM threw
        // `OutOfMemoryError` under both ZGC and G1.
        //
        // `condemn_idle_soft_refs` applies the policy here instead, and only
        // the entries it condemns join the null pass; the rest stay traced
        // strongly and are retained exactly as before. `0` for the clock is
        // the same convention `process_references_after_gc` uses -- the
        // processor substitutes the mutator clock it has observed.
        //
        // The clock is a real `SystemTime` reading, not the `0` that
        // `process_references_after_gc` passes. `0` means "use the last value a
        // mutator handed `touch_soft_reference`", which is the moment of the
        // most recent `SoftReference.get()` in the process — so the idle window
        // of the reference that made that call is zero, and a program looping
        // on its own soft-referenced cache never opens one. This caller is
        // ordinary VM code and has a clock; `SoftReference.<init>` and `.get()`
        // stamp entries from the same `SystemTime` epoch, so the two are
        // directly comparable.
        let soft = if last_ditch_soft_clear_armed() {
            // The allocation has already failed and this is the last
            // collection before `OutOfMemoryError`. The specification requires
            // every softly-reachable object to be released first, whatever the
            // LRU policy thinks.
            rp.condemn_all_soft_refs()
        } else {
            rp.condemn_idle_soft_refs(soft_free_mb, now_ms())
        };
        // Same stamp column the weak/phantom half carries -- the write loop
        // below screens both the same way.
        let soft: Vec<(usize, usize, i32)> = soft
            .into_iter()
            .map(|(r, t)| (r, t, rp.identity_stamp(r).unwrap_or(0)))
            .collect();
        (weak_phantom, soft)
    };
    // RandomizedContext WeakHashMap<Thread,...> fix: publish this cycle's
    // referent addresses so the non-moving young sweep can recognize a
    // kept-in-place (unpromoted) survivor as having genuinely survived (see
    // gc_quiescence::is_watched_referent). Unconditional — even an empty
    // list must be published so a previous cycle's entries can never leak
    // into this one.
    //
    // Young-GC live-reclaim ROOT FIX (2026-07-07, RRWL/ThreadLocalMap$Entry
    // IMSE/hang family): watch the REFERENCE OBJECTS' own addresses too, not
    // just their referents. The post-GC restore pass and `remove_collected`
    // judge survival by `pointer_map.contains_key(addr) || is_addr_live(addr)`,
    // and for the Generational heap `is_addr_live` is old-gen-only — so after
    // a NON-MOVING young sweep (which produces NO pointer_map entries for
    // kept-in-place survivors) a live YOUNG Reference object was judged dead:
    // its pre-GC-nulled referent slot was never restored and its processor
    // entry was pruned. A live young `ThreadLocalMap$Entry` (a WeakReference)
    // then answered `refersTo(null) == true`, so the mutator itself expunged
    // the live entry — losing the RRWL `readHolds` hold counter (the
    // `IllegalMonitorStateException: attempt to unlock read lock` /
    // permanent all-parked hang in `RwlReadTearingProbe` and Elasticsearch
    // `LongRandomBinaryDocValuesRangeQueryTests.testAllEqual`), and losing
    // WeakHashMap entries generally (the RandomizedContext residual). The
    // watched set already flows into identity `pointer_map` entries for every
    // kept-in-place survivor the sweep retains (side-marked or header-marked),
    // which is exactly the survival proof the restore pass needs — the
    // referent half of this fix simply never covered the reference objects.
    // Old-gen reclamation fix (HIB-CV-32 family, TestMVStoreCachePerformance):
    // publish EVERY address the processor holds, not just the weak/phantom
    // pairs' and their queues'. `process_references` and `remove_collected`
    // consult the survival predicate for soft/cleaner/finalizer entries too,
    // and after an old-gen reclamation that predicate is only exact for
    // addresses the collector was asked to prove — see
    // `VmHeap::watched_pre_gc_addr_survived`. The set is bounded by the number
    // of live `Reference` objects, and a non-surviving address simply never
    // gets an entry, so widening it costs one hash insert per tracked
    // reference and cannot over-retain.
    let watch_addrs: Vec<usize> = {
        let rp = shared.mem.ref_processor.lock();
        rp.all_tracked_addrs()
    };

    if crate::runtime::env_cache::dbg_watchref() {
        eprintln!(
            "[watchref] publishing {} watched referent(s): {:x?}",
            watch_addrs.len(),
            watch_addrs
        );
    }
    cratonvm_gc::gc_quiescence::set_watched_referents(&watch_addrs);
    if pairs.is_empty() && soft_pairs.is_empty() {
        return;
    }
    if dbg_weakref() {
        eprintln!(
            "[weakref] pre-gc null pass: {} weak/phantom referent(s), \
             {} policy-condemned soft referent(s)",
            pairs.len(),
            soft_pairs.len()
        );
    }
    // THE PRE-GC PASS WAS THE UNSCREENED WRITE SITE.
    //
    // `process_references_after_gc`'s cleared / enqueue / restore loops were
    // given a class-shape guard on 2026-08-16 (`is_reference_shaped`) after two
    // measured corruptions -- a `java.lang.String` published as a
    // `ReferenceQueue` head, and field 0 of a `String` nulled. THIS loop, which
    // performs the same kind of write through the same kind of address, kept
    // only the `num_fields >= 2` test, which almost every class passes: an
    // `org.h2.engine.SessionLocal` passes it, and so does every `org.h2.value.Value`.
    // So a processor entry whose `Reference` had been reclaimed and its address
    // re-issued nulled slot 0 of whatever now lived there -- the
    // `NullPointerException: Cannot invoke "org.h2.value.Value.getValueType()"
    // because "v" is null` half of the H2 `TestMultiThread` MVStore-writer
    // report, whose own analysis records that the failure "survives the shape
    // guard that now screens every reference-processor write". It did, because
    // this write was not one of the screened ones.
    //
    // Screened here with BOTH tests:
    //
    //  * the same class-shape guard as the post-GC loops, and
    //  * the identity stamp, which is the exact version of it: a reclaimed
    //    `Reference`'s address re-issued to ANOTHER `Reference` is shape-clean
    //    and identity-wrong, and H2 allocates a `CloseWatcher` (a
    //    `PhantomReference`) per connection, so same-class reuse is the common
    //    case rather than the exotic one.
    let class_manager = shared.classes.class_manager.read();
    let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
    // The referent's CLASS, read from slot 0 on the way past and handed to the
    // processor below. This is the only point in a collection that holds a
    // referent and the heap at the same time, and the post-GC restore pass
    // needs it: that pass writes an object back into slot 0 from an ADDRESS,
    // and an address is not an identity once a compacting collector has
    // re-issued it. See `ReferenceProcessor::referent_class_stamps`.
    let mut referent_classes: Vec<(usize, u32)> = Vec::new();
    for (ref_obj_addr, _referent, stamp) in pairs.into_iter().chain(soft_pairs) {
        // The Reference object is live (or dead-but-not-yet-collected) at this
        // point, so its memory is valid; writing its referent slot is safe.
        // SAFETY: `ref_obj_addr` is a current Reference-object address held by
        // the reference processor (kept current by `update_after_gc` /
        // `remove_collected`); it points at a valid heap object header.
        let ref_obj = unsafe { ObjectRef::from_raw(ref_obj_addr as *mut u8) };
        // HIB-WEAKREF-RECYCLE.1 (2026-07-31): a real `java.lang.ref.Reference`
        // always has >= 2 instance fields (referent, queue) — the same
        // invariant `process_references_after_gc`'s cleared/enqueue loops
        // already rely on ("also covers old-gen reuse after a major GC").
        // `>= 1` was too weak in exactly the case those loops call out: an
        // entry whose Reference object was reclaimed by an old-gen sweep keeps
        // a `reference_obj` address that `is_addr_live` still answers `true`
        // for (`is_old_gen_addr` is a pure range check), so the entry is never
        // pruned and this pass nulls slot 0 of whatever now occupies that
        // memory. When the new occupant has 0 fields the `gen_heap` OOB guard
        // catches it (observed 174x running Hibernate's
        // `DefaultCatalogAndSchemaTest` under `--nojit`, receiver reading
        // `class_id=0 java/lang/Object` and garbage `<unresolved>` ids, ending
        // in SIGSEGV); when it has exactly 1 the write lands SILENTLY on a
        // real field. Requiring >= 2 declines both, and can never skip a
        // genuine Reference.
        // Not a `Reference` any more ⇒ the address no longer names what this
        // processor recorded. `None` (class not loaded) admits: no Reference
        // object can exist yet, so the guard has nothing to judge.
        if let Some(cid) = reference_cid {
            if !class_manager.is_subclass_of(shared.mem.heap.class_id_of(ref_obj), cid) {
                continue;
            }
        }
        // Still a `Reference`, but is it THE Reference? `identity_hash_code`
        // mints for an object that has none, so a re-issued address answers
        // with a fresh counter value rather than the recorded one. A `0` answer
        // is "cannot tell" (the object is thin-locked, so its hash is not in
        // the mark word) and falls back to the shape guard above rather than
        // declining a legitimate entry.
        if stamp != 0 {
            let now = shared.mem.heap.identity_hash_code(ref_obj);
            if now != 0 && now != stamp {
                continue;
            }
        }
        if shared.mem.heap.num_fields(ref_obj) >= 2 {
            // Read the referent out of the slot BEFORE nulling it, and record
            // its class. The slot is authoritative here in a way the
            // processor's recorded `referent` address is not: every guard
            // above has just established that this object IS the Reference
            // that was discovered, so whatever slot 0 holds right now is its
            // referent by construction.
            if let Value::Object(Some(rt)) = shared.mem.heap.get_field(ref_obj, 0) {
                referent_classes.push((ref_obj_addr, shared.mem.heap.class_id_of(rt).as_u32()));
            }
            // Slot 0 = REF_FIELD_REFERENT (matches the real JDK Reference layout
            // and the synthetic constant in native-builtins).
            //
            // INT-8: PROTOCOL write — SATB pre-barrier suppressed. This null
            // is restored (for survivors) before mutators resume, so the
            // snapshot graph is unchanged; letting it fire logged EVERY
            // active referent as a G1 mark root at EVERY mid-cycle young
            // pause, which kept weakly-reachable Old objects bitmap-marked
            // and made remark-time reference processing inert.
            shared
                .mem
                .heap
                .set_field_suppress_satb(ref_obj, 0, Value::Object(None));
        }
    }
    drop(class_manager);
    if !referent_classes.is_empty() {
        let mut rp = shared.mem.ref_processor.lock();
        for (ref_obj_addr, class_id) in referent_classes {
            rp.stamp_referent_class(ref_obj_addr, class_id);
        }
    }
}

/// Cached `CRATONVM_DBG_REFDISC` gate: trace every `discover_reference` with
/// the reference object's CLASS NAME. The numeric `ref_type` cannot tell an
/// ordinary `PhantomReference` from a `jdk.internal.ref.Cleaner` (the latter
/// is a subclass, so its `super(referent, dummyQueue)` arrives with the same
/// phantom tag) — which is exactly the question
/// `direct-bytebuffers-are-never-reclaimed-20260805.md` needed answered
/// before its Cleaner routing could be written.
#[inline]
pub fn dbg_refdisc_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_REFDISC").is_some())
}

/// Cached `CRATONVM_DBG_NO_CLEANERS` gate (bc math-ec 0x4 bisect): skip ONLY
/// `run_cleaner_actions` + `run_finalizers` (the Java invokes on queued —
/// possibly stale — addresses), keeping `process_references_after_gc` live.
/// Discriminates "the invokes corrupt" from "the refproc loops corrupt".
#[inline]
fn no_cleaners() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NO_CLEANERS").is_some())
}

/// Cached `CRATONVM_DBG_AIOOBE` gate (perf P1, audit `vm-runtime.md`): the
/// array load/store AIOOBE diagnostic. The previous call sites invoked
/// `cratonvm_types::flags::runtime_var("CRATONVM_DBG_AIOOBE")` (which locks the process env and
/// allocates a `String`) on the array-bounds error path; cache it once like
/// the sibling gates above.
#[inline]
fn aioobe_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_AIOOBE").is_some())
}

/// Cached `CRATONVM_DBG_AIOOBE2` gate (perf P1): secondary array-bounds
/// diagnostic; same rationale as [`aioobe_dbg`].
#[inline]
fn aioobe2_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_AIOOBE2").is_some())
}

/// `CRATONVM_DBG_LINKAGE=1` — trace VM-raised `LinkageError`s through the
/// interpreter's error routing. A linkage error that never reaches
/// `throw_linkage_error` stays a `VmError` and unwinds past every Java handler,
/// which looks identical to a VM crash; this names the routing arm it took and
/// the Java frames it took it from.
pub(crate) fn dbg_linkage() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LINKAGE").is_some())
}

/// Dump `where` plus the live Java frames, deepest first. `CRATONVM_DBG_LINKAGE` only.
pub(crate) fn dbg_linkage_dump(thread: &JvmThread, where_: &str, detail: &str) {
    eprintln!("[DBG_LINKAGE] {where_}: {detail}");
    for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
        eprintln!(
            "[DBG_LINKAGE-STK {i}] {}.{}{} pc={}",
            f.class_name(),
            f.method_name(),
            f.method_descriptor(),
            f.pc
        );
    }
}

/// Validate a primitive-array-store receiver header; dump receiver + Java
/// stack when it is not a plausible array (the stale-ref smear signature).
/// Reads raw header BYTES (not enum fields) — a garbage `kind`/`element_type`
/// byte outside the declared variants would be UB to materialize as the enum.
fn arrstore_check(
    _shared: &SharedVm,
    thread: &JvmThread,
    array_ref: cratonvm_types::ObjectRef,
    index: i32,
    op: &str,
) {
    let p = array_ref.as_ptr();
    // SAFETY: `array_ref` was decoded from the operand stack as an in-heap
    // pointer; reading the first 16 header bytes of managed memory is safe
    // (arenas stay mapped) even if the contents are garbage.
    let (class_id_raw, kind_byte, elem_byte, array_len) = unsafe {
        (
            // Cast: reinterpret raw header byte pointer as *const u32 to read packed fields
            (p as *const u32).read_unaligned(),
            *p.add(4),
            *p.add(5),
            // Cast: reinterpret raw header byte pointer as *const u32 to read packed fields
            (p.add(12) as *const u32).read_unaligned(),
        )
    };
    // ObjectKind::Array == 1; ArrayElementType has < 16 variants; a real
    // array_length is <= i32::MAX. Anything else = garbage header = stale ref.
    // Widening: i32::MAX -> u32 (positive constant fits in u32)
    let plausible = kind_byte == 1 && elem_byte < 16 && array_len <= i32::MAX as u32;
    if plausible {
        return;
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let k = N.fetch_add(1, Ordering::Relaxed);
    if k >= 8 {
        return;
    }
    eprintln!(
        "[arrstore] #{k} {op} through GARBAGE-HEADER receiver @0x{:x}: class_id_raw={} kind_byte={} elem_byte={} array_len={} index={}",
        // Cast: raw pointer to integer address for diagnostic formatting
        p as usize, class_id_raw, kind_byte, elem_byte, array_len, index,
    );
    eprintln!("[arrstore] Java stack (top first):");
    for f in thread.frames.iter().rev().take(28) {
        eprintln!(
            "[arrstore]   {}.{}{} pc={}",
            f.class_name(),
            f.method_name(),
            f.method_descriptor(),
            f.pc,
        );
    }
}

fn ec_is_watched_class(shared: &SharedVm, cid: cratonvm_types::ClassId) -> bool {
    use parking_lot::Mutex;
    use std::sync::OnceLock;
    // PER-VM STATE (P0, `docs/architecture/per-vm-state.md`): the memo answers
    // "does this ClassId's name match the EC watch list?", and `ClassId`s are
    // allocated per-VM, so the key must carry `vm_identity` or a second VM
    // reads the first VM's verdict for an unrelated class.
    static MEMO: OnceLock<Mutex<std::collections::HashMap<(usize, u32), bool>>> = OnceLock::new();
    let memo = MEMO.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = (shared.vm_identity, cid.as_u32());
    if let Some(&v) = memo.lock().get(&key) {
        return v;
    }
    let v = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(cid)
            .map(|c| {
                c.name.contains("/asn1/x9/")
                    || c.name.contains("Curve")
                    // bc math-ec 0x4: the mutator-written victim VARIES across
                    // runs and is often a small, frequently-allocated JDK class
                    // (the pre-GC young 0x4 was measured on java/util/HexFormat
                    // fld[1] and java/util/logging/Level). Watch those too so the
                    // per-native detect (CRATONVM_DBG_ECWATCH_NATIVE) names the
                    // writer whichever object the stray write lands on.
                    || c.name.contains("java/util/HexFormat")
                    || c.name.contains("java/util/logging/Level")
            })
            .unwrap_or(false)
    };
    memo.lock().insert(key, v);
    v
}

// ---------------------------------------------------------------------------
// Allocation, GC, and the root protocol
// ---------------------------------------------------------------------------
//
// TLABs, the GC entry points, the STW takeover protocol, root snapshots and
// the moving-collector remap, plus the provenance tracing built to debug
// them: `interpreter/gc_and_alloc.rs`.

// ---------------------------------------------------------------------------
// Instruction execution result (internal)
// ---------------------------------------------------------------------------

/// The result of executing a single instruction.
enum InstructionResult {
    /// Continue to next instruction.
    Continue,
    /// Method returned a value (or void = None).
    Return(Option<Value>),
    /// A new bytecode frame was pushed; caller should update frame_idx.
    FramePushed,
}

/// Result of a cached inline call attempt (stackless dispatch).
#[derive(Debug)]
enum CachedCallResult {
    /// Bytecode frame pushed onto thread.frames. Caller should update frame_idx.
    FramePushed,
    /// Call fully handled (native method executed, return value pushed).
    Handled,
    /// No cache hit — fall through to slow path.
    CacheMiss,
}

// ---------------------------------------------------------------------------
// H6 — native-stack-aware re-entrant recursion guard
// ---------------------------------------------------------------------------
//
// The `execute` re-entrancy guard (see the `EXEC_DEPTH` block inside
// `execute`) must trip — throwing a *catchable* `StackOverflowError` —
// BEFORE deep re-entrant native dispatch (e.g. reflective `Method.invoke`
// cascades) exhausts the OS thread's native stack and aborts the whole
// process (uncatchable).
//
// The ceiling cannot be a single hard-coded constant because different
// threads run on very different native stacks: the main VM thread is spawned
// with a 128 MiB stack, whereas worker / `java.lang.Thread` carriers default
// to 8 MiB (see `vm_exec.rs`, honouring `RUST_MIN_STACK`). A constant tuned
// for one is wrong for the other — too low throttles legitimate deep
// recursion on the big stack, too high lets a worker blow its 8 MiB stack
// before the guard ever trips.
//
// So we derive a per-thread ceiling from the thread's ACTUAL native stack
// size. Threads that host Java execution call
// [`init_thread_exec_depth_ceiling`] with their configured native stack size
// at start-up; the guard reads the resulting per-thread ceiling.

/// Conservative estimate of how many bytes of native stack a single
/// re-entrant `execute` level can consume. Each `execute` call adds several
/// Rust frames (the recursive `execute_frame`, invoke dispatch, and the JIT
/// entry trampoline). The original guard assumed ~6.4 KiB/level (10 000
/// levels for a 64 MiB stack); we use a deliberately pessimistic 8 KiB so the
/// derived ceiling trips with margin to spare before the OS guard page.
const NATIVE_STACK_BYTES_PER_EXEC_LEVEL: usize = 8 * 1024;

/// Fraction of the native stack we are willing to spend on re-entrant
/// `execute` recursion before tripping the guard. The remainder is reserved
/// head-room for the deepest single Java frame's own locals/operands plus any
/// native callee (the verifier, GC, JIT) running on top of the deepest level.
/// `2` => use at most half the stack for recursion depth.
const NATIVE_STACK_SAFETY_DIVISOR: usize = 2;

/// Default native stack assumption for a thread that never called
/// [`init_thread_exec_depth_ceiling`]. The process main VM thread is spawned
/// with a 128 MiB stack (see `vm-cli` `main`), so defaulting to 128 MiB keeps
/// the deep-recursion head-room that workloads such as `binaryTrees` rely on
/// even if the explicit init call is not wired for that thread. Worker carrier
/// threads (8 MiB) DO call the init function and therefore get a correctly
/// lowered ceiling rather than this default.
const DEFAULT_NATIVE_STACK_BYTES: usize = 128 * 1024 * 1024;

/// Absolute floor for the derived ceiling. Even on a tiny stack we allow at
/// least this many re-entrant levels so ordinary (non-pathological) call
/// graphs are never spuriously rejected.
const MIN_EXEC_DEPTH_CEILING: u32 = 256;

/// Compute the re-entrant `execute` depth ceiling for a thread whose native
/// stack is `native_stack_bytes` large.
#[inline]
fn derive_exec_depth_ceiling(native_stack_bytes: usize) -> u32 {
    // Diagnostic / safety override: `CRATONVM_EXEC_DEPTH_CEILING=<n>` forces the
    // `execute` recursion ceiling. Useful to (a) confirm a runaway recursion goes
    // through `execute` by forcing an early *catchable* StackOverflowError (whose
    // Java stack trace reveals the cycle), and (b) cap deep frames whose real
    // per-level native-stack cost exceeds NATIVE_STACK_BYTES_PER_EXEC_LEVEL.
    if let Ok(v) = cratonvm_types::flags::runtime_var("CRATONVM_EXEC_DEPTH_CEILING") {
        if let Ok(n) = v.trim().parse::<u32>() {
            return n.max(1);
        }
    }
    let usable = native_stack_bytes / NATIVE_STACK_SAFETY_DIVISOR;
    let levels = usable / NATIVE_STACK_BYTES_PER_EXEC_LEVEL;
    // Clamp into u32 and apply the floor.
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    let levels = levels.min(u32::MAX as usize) as u32;
    levels.max(MIN_EXEC_DEPTH_CEILING)
}

/// Conservative estimate of how many bytes of native stack a single
/// re-entrant JIT *dispatch* level can consume. The helper contains argument
/// decoding, MIC/PIC handling, a SATB flush, and a compiled-Java frame. Its
/// release footprint is nevertheless within the interpreter's proven 8 KiB
/// per-level budget; the counter is also an active call-chain depth rather
/// than a recursion counter. We therefore budget a
/// The counter tracks every active JIT-to-JIT dispatch, not only recursive
/// calls.  Production Lucene vector search legitimately keeps more than 128
/// such calls live on an 8 MiB worker carrier, so the former 32 KiB estimate
/// converted an ordinary call chain into a spurious `StackOverflowError`.
/// Reserve 8 KiB per level, matching the interpreter's proven conservative
/// frame budget: an 8 MiB carrier still retains half its stack as head-room
/// and the guard continues to turn pathological recursion into a catchable
/// Java exception before the native guard page.
const NATIVE_STACK_BYTES_PER_JIT_DISPATCH_LEVEL: usize = 8 * 1024;

/// Absolute floor for the derived JIT-dispatch ceiling. Distinct from (and
/// lower than) [`MIN_EXEC_DEPTH_CEILING`] because the JIT per-level budget is
/// 4× larger: applying the 256-level exec floor here would imply 256·32 KiB =
/// 8 MiB of recursion, which on an 8 MiB worker carrier stack could exceed the
/// stack the floor was meant to protect. 64 levels (≤ 2 MiB at the pessimistic
/// budget) keeps ordinary non-pathological compiled recursion safe even on the
/// smallest carrier while never floating the ceiling above stack capacity.
const MIN_JIT_DISPATCH_DEPTH_CEILING: u32 = 64;

/// Compute the re-entrant JIT-dispatch depth ceiling for a thread whose native
/// stack is `native_stack_bytes` large. Mirrors [`derive_exec_depth_ceiling`]
/// but uses the larger per-level JIT-dispatch frame estimate and a lower floor
/// (see [`MIN_JIT_DISPATCH_DEPTH_CEILING`]).
#[inline]
fn derive_jit_dispatch_depth_ceiling(native_stack_bytes: usize) -> u32 {
    let usable = native_stack_bytes / NATIVE_STACK_SAFETY_DIVISOR;
    let levels = usable / NATIVE_STACK_BYTES_PER_JIT_DISPATCH_LEVEL;
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    let levels = levels.min(u32::MAX as usize) as u32;
    levels.max(MIN_JIT_DISPATCH_DEPTH_CEILING)
}

thread_local! {
    /// Per-thread re-entrant `execute` depth ceiling, derived from the
    /// thread's native stack size. Defaults to the main-VM-thread value
    /// (`DEFAULT_NATIVE_STACK_BYTES`) so a thread that never calls
    /// [`init_thread_exec_depth_ceiling`] still gets a safe (high) ceiling.
    static EXEC_DEPTH_CEILING: std::cell::Cell<u32> =
        std::cell::Cell::new(derive_exec_depth_ceiling(DEFAULT_NATIVE_STACK_BYTES));

    /// Per-thread re-entrant JIT-dispatch depth ceiling, derived from the same
    /// native stack size as [`EXEC_DEPTH_CEILING`] but with the larger
    /// per-level JIT-dispatch frame budget. Read by the JIT dispatch helpers
    /// (`jit_invoke_dispatch` / `jit_invoke_virtual_mic` in `jit/helpers.rs`)
    /// via [`jit_dispatch_depth_ceiling`] so a JIT→JIT recursion (e.g.
    /// `binaryTrees(18)` deep `make()` recursion) throws a *catchable*
    /// `StackOverflowError` before the OS native stack overflows.
    static JIT_DISPATCH_DEPTH_CEILING: std::cell::Cell<u32> =
        std::cell::Cell::new(derive_jit_dispatch_depth_ceiling(DEFAULT_NATIVE_STACK_BYTES));
}

/// Per-thread re-entrant JIT-dispatch depth ceiling.
///
/// The JIT→JIT call path (`jit_invoke_dispatch` → compiled entry → … and
/// `jit_invoke_virtual_mic` → compiled entry → …) bypasses
/// [`execute`] entirely, so the interpreter's `EXEC_DEPTH` guard never fires
/// for purely-compiled recursion. The JIT dispatch helpers therefore maintain
/// their OWN depth counter and trip it against this ceiling, throwing a
/// catchable `java/lang/StackOverflowError` instead of letting deep compiled
/// recursion blow the OS native stack (an uncatchable rc=127 abort).
///
/// Derived per-thread from the same native stack size recorded by
/// [`init_thread_exec_depth_ceiling`]; defaults to the safe 128 MiB-derived
/// value for threads that never called it.
#[inline]
pub fn jit_dispatch_depth_ceiling() -> u32 {
    JIT_DISPATCH_DEPTH_CEILING.with(|c| c.get())
}

/// Record the calling thread's native stack size so the re-entrant `execute`
/// recursion guard can derive a correct [`StackOverflowError`] ceiling for it.
///
/// Call this ONCE, from inside the thread, before it begins executing Java
/// bytecode. Worker / `java.lang.Thread` carriers (which default to an 8 MiB
/// native stack) MUST call this so the guard trips before their smaller stack
/// overflows; the process main thread may also call it but otherwise inherits
/// the safe 128 MiB-derived default.
///
/// `native_stack_bytes` is the value passed to
/// `std::thread::Builder::stack_size` for the calling thread.
pub fn init_thread_exec_depth_ceiling(native_stack_bytes: usize) {
    let ceiling = derive_exec_depth_ceiling(native_stack_bytes);
    EXEC_DEPTH_CEILING.with(|c| c.set(ceiling));
    // Derive the companion JIT-dispatch ceiling from the SAME native stack
    // size so the JIT→JIT recursion guard (see `jit_dispatch_depth_ceiling`)
    // is calibrated for this thread's real stack too. Worker carriers (8 MiB)
    // get a correspondingly lower ceiling; the main 128 MiB thread keeps deep
    // head-room for legitimate recursion like `binaryTrees`.
    let jit_ceiling = derive_jit_dispatch_depth_ceiling(native_stack_bytes);
    JIT_DISPATCH_DEPTH_CEILING.with(|c| c.set(jit_ceiling));
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// activate-ir-optimizer (runtime wiring): cached `CRATONVM_JIT_C2_FIRST_CALL`
/// flag. The `fn execute` first-call compile path uses the single-pass backend
/// (`jit::x64::compile`) directly and caches a non-IR body on call #1, which
/// preempts the optimizing IR pipeline (`jit::try_compile`) on every later path
/// (dispatcher warmup, OSR, background worker all probe `jit_cache` first). When
/// this flag is set, that eager first-call single-pass compile is replaced by an
/// invocation-counted upgrade through `try_jit_compile_callee` →
/// `jit::try_compile(optimize=true)` (which SUBSUMES single-pass), so hot methods
/// actually reach the IR optimizer. Read once and cached (this is on the hot
/// uncached-invocation path). Default-OFF → behaviour is byte-for-byte unchanged.
fn c2_first_call_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_C2_FIRST_CALL").is_some()
    })
}

// ---------------------------------------------------------------------------
// JDK-only mode: kind-aware native resolution for this file's dispatch sites
// ---------------------------------------------------------------------------

/// The one native lookup used by every native-dispatch site in this file
/// (`docs/feature-designs/jdk-only-mode.md` §7).
///
/// Replaces the bare `NativeMethodRegistry::find` calls that handed back a
/// `NativeCallback` with the `NativeKind` thrown away. A lookup that discards
/// the kind cannot tell a reviewed `Intrinsic` from a `SyntheticStub`, so it
/// cannot enforce §1.3 or §1.4 — it was a policy bypass sitting *beside*
/// `resolve_dispatch` rather than routing through it. Every `find` in
/// `fn execute`'s no-`Code` rescue chain now comes through here.
///
/// Why the wave-1 name-only adapter and not [`crate::vm::resolve_dispatch`]
/// itself: these sites hold a class *name* (and in the receiver-walk case a
/// `ClassId` whose `&Class` borrow is already pinned by an in-scope
/// `class_manager` read guard), but never the resolved `&Method` — the whole
/// point of this chain is that the resolved method has **no** `Code`, so the
/// `&Method` §7 wants is the abstract declaration we are trying to get away
/// from. Re-resolving one just to satisfy the signature would add a
/// class-manager read lock plus a hierarchy walk per rescue. The adapter takes
/// the identical policy decision from the names, and `bytecode_available` is
/// the one §7 step-3 input the names cannot supply.
///
/// Cost is unchanged: `resolve_id` is the *same* single triple hash `find`
/// already paid (`resolve_id(..).and_then(callback_of)` is documented to equal
/// `find(..)`, descriptor-quirk fallback included), and the slot handle it
/// returns makes the §4 census increment one relaxed add instead of a second
/// hash. No allocation, no formatting; violation objects are built only on the
/// reject path, inside the adapter's `#[cold]` constructors.
///
/// Returns:
/// * `Ok(Some(cb))` — dispatch `cb`. The invocation is already counted.
/// * `Ok(None)` — no native registered, or (under `JdkOnly`) bytecode wins.
///   The caller continues down its existing fallback chain, exactly as it did
///   when `find` returned `None`.
/// * `Err(violation)` — `JdkOnly` refusal (§1.3). Surface it as
///   `VmError::JdkOnly`; never silently fall back to another implementation.
///
/// **`Compatible` mode is bit-for-bit today's behaviour.** `compat_native_wins`
/// is passed `true`, which is exactly the unconditional "a registered native
/// wins here" that the `find` call sites encoded, and in `Compatible` mode
/// `resolve_native_dispatch_wave1` is a pure function of that boolean.
///
/// ORCHESTRATOR — this function is the **single** point of coupling between
/// this file and agent E's `vm/src/vm/vm_exec.rs`. All five native-dispatch
/// sites in `fn execute` funnel through it, so if E's wave-1 surface lands with
/// a different shape, this body is the only thing to reconcile. It depends on
/// exactly three items, all re-exported through `crate::vm` by
/// `pub use vm_exec::*`:
///
/// * `dispatch_policy(&SharedVm) -> cratonvm_types::compat::ExecutionPolicy`
/// * `resolve_native_dispatch_wave1(policy, class_name, method_name,
///   descriptor, Option<(NativeCallback, NativeKind)>, compat_native_wins:
///   bool, bytecode_available: bool) -> Option<DispatchDecision<'static>>`
/// * `DispatchDecision::{Reject, native_callback}`
///
/// The contract's §7 `resolve_dispatch(policy, &Class, &Method, native)` is
/// deliberately not called here — see the paragraph above on why these sites
/// have no `&Method`. If E's name-only adapter is dropped, the replacement is
/// to re-acquire the `class_manager` read lock at each of the five sites and
/// call `resolve_dispatch` proper; that is a correctness-neutral but
/// measurably more expensive shape, which is why it was not done first.
#[inline]
fn resolve_native_for_dispatch(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    bytecode_available: bool,
) -> Result<Option<cratonvm_native_api::NativeCallback>, cratonvm_types::error::JdkOnlyViolation> {
    let registry = &shared.natives.native_methods;
    let id = match registry.resolve_id(class_name, method_name, method_descriptor) {
        Some(id) => id,
        None => return Ok(None),
    };
    let callback = match registry.callback_of(id) {
        Some(callback) => callback,
        None => return Ok(None),
    };
    // `kind_of_id` reports the slot's true kind. `find_with_kind` would report
    // `Bridge` on its descriptor-quirk cold path; the difference is invisible in
    // `Compatible` mode (every kind yields the same callback) and strictly more
    // accurate under `JdkOnly`.
    let kind = registry
        .kind_of_id(id)
        .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
    match crate::vm::resolve_native_dispatch_wave1(
        crate::vm::DispatchDoor::NoCodeRescue,
        crate::vm::dispatch_policy(shared),
        class_name,
        method_name,
        method_descriptor,
        Some((callback, kind)),
        // JDK-ONLY-WAVE2 §10 — AUDITED 2026-08-06, not a live defect, and
        // deliberately still a constant.
        //
        // The `true` reproduces the pre-§7 "a registered native unconditionally
        // wins here" of the `find` calls this adapter replaced, and that is
        // faithful rather than lazy: `resolve_native_dispatch_wave1` uses the
        // flag only to choose between "the site preferred bytecode" and the
        // kind-based ladder, and this site never preferred bytecode. The
        // `NativeShadowsBytecode` observation a `false` would have produced is
        // still recorded — by the `Bridge if bytecode_available` arm one level
        // down — so nothing is lost from the report either.
        //
        // The record asks for "the real per-site compatibility verdict once
        // `force_native_over_real_jdk_bytecode` and the forced-native `String`
        // list are unified". That unification is §11's exercise, not this
        // site's: until those name lists collapse there is no per-site verdict
        // to read, and inventing one here would be a THIRD answer for methods
        // that already have two. Revisit when §11's Compatible-mode deletion
        // lands; under `JdkOnly` the chain is already not consulted.
        true,
        bytecode_available,
    ) {
        Some(crate::vm::DispatchDecision::Reject(violation)) => Err(violation),
        Some(decision) => match decision.native_callback() {
            Some(callback) => {
                // §4 census: one relaxed increment, no hashing, no allocation.
                // The acceptance criterion is *zero* synthetic-stub invocations
                // through **any** path, and an uncounted path is unverifiable.
                registry.record_invocation(id);
                Ok(Some(callback))
            }
            // `Bytecode` is unreachable from the name-only adapter, but treat it
            // as "no native" rather than assuming.
            None => Ok(None),
        },
        // JdkOnly, §7 step 3: concrete bytecode beats this bridge.
        None => Ok(None),
    }
}

/// The concrete class whose registered natives `execute`'s "Path B" borrows
/// when an interface method resolved to an abstract declaration and the
/// receiver matched no class in the store — or `""` when this interface has no
/// such stand-in.
///
/// Extracted from the `match` that used to sit inline at the Path B call site
/// (JDK-ONLY-WAVE2 §8) for one reason: the array refusal immediately after it
/// has to ask "is this one of the substituted interfaces?", and a second
/// hard-coded copy of these six names would be a list that can drift from the
/// list it is supposed to mirror. There is now exactly one copy, and
/// `g13_array_receiver_tests` pins it.
///
/// **Deliberately absent: `java/lang/Cloneable` and `java/io/Serializable`.**
/// They are the only two interfaces an array type actually implements
/// (JLS 4.10.3), they declare no methods, and nothing may map them here — that
/// absence is what makes "canonical is non-empty AND the receiver is an array"
/// a sound proof of `IncompatibleClassChangeError` rather than a heuristic.
fn canonical_concrete_for_interface(iface: &str) -> &'static str {
    match iface {
        "java/util/Set" | "java/util/Collection" => "java/util/HashSet",
        // Iterable has no collection shape of its own. Keep synthetic
        // List-style receivers on the established ArrayList bridge after
        // lambda proxies have already had a chance to dispatch their SAM
        // implementation.
        "java/lang/Iterable" => "java/util/ArrayList",
        "java/util/List" => "java/util/ArrayList",
        "java/util/Map" => "java/util/HashMap",
        "java/util/Iterator" => "java/util/HashMap$KeyItr",
        _ => "",
    }
}

/// Execute a method on the given class.
///
/// This is called by `invoke_on_class_shared` for non-native methods.
pub fn execute(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    if crate::runtime::env_cache::exec_frame_trace()
        && method_name == "aotContributedInitializerStartsManagementContext"
    {
        let (cname, loader) = {
            let cm = shared.classes.class_manager.read();
            (
                cm.get_class(class_id).map(|c| c.name.to_string()),
                cm.get_loader_id(class_id),
            )
        };
        eprintln!(
            "[EXEC-FRAME-TRACE] method={} class_id={:?} class_name={:?} loader={:?}",
            method_name, class_id, cname, loader
        );
    }
    // S-bytebuddy r1 — Rust-side recursion guard.
    //
    // ByteBuddy's `JavaDispatcher.run()` performs deep reflection via
    // `Method.invoke`, whose bytecode dispatches back into this `execute`
    // function. The Java-level frame counter (`thread.frames`) is checked at
    // each push (see `max_stack_depth` guard below), BUT a `Method.invoke`
    // chain that re-enters `execute` re-invokes this Rust function on the
    // native call stack BEFORE the Java frame is pushed. That recursion is
    // not bounded by `max_stack_depth`. Result: under a deep ByteBuddy
    // reflection cascade, the Rust call stack blows past the OS guard page
    // (even at the 64 MB main-vm setting) and the process aborts with the
    // Rust stack-overflow handler (rc=127, SIGABRT) — bypassing any Java
    // try/catch.
    //
    // Throw a Java `StackOverflowError` BEFORE we recurse further. JVMS lets
    // the implementation raise SOE at any depth; ByteBuddy's reflection
    // helpers catch `Throwable` and recover.
    //
    // Limit calibration (H6): the ceiling is no longer a single hard-coded
    // constant. It is derived per-thread from the thread's ACTUAL native
    // stack size (see `EXEC_DEPTH_CEILING` / `derive_exec_depth_ceiling` /
    // `init_thread_exec_depth_ceiling` above). The previous fixed `10_000`
    // assumed a 64 MiB stack and overflowed the 8 MiB worker-carrier stack —
    // a hard, uncatchable process abort — before it ever tripped. Each
    // `execute` level burns several KiB of native stack (recursive
    // `execute_frame` + invoke dispatch + JIT entry trampoline); the derived
    // ceiling reserves head-room so the guard fires (throwing a catchable
    // `StackOverflowError`) before the OS guard page does.
    thread_local! {
        static EXEC_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }
    struct DepthGuard;
    impl Drop for DepthGuard {
        fn drop(&mut self) {
            EXEC_DEPTH.with(|d| {
                let v = d.get();
                d.set(v.saturating_sub(1));
            });
        }
    }
    let depth = EXEC_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    // H6: trip against the per-thread ceiling derived from the thread's
    // ACTUAL native stack size (see `EXEC_DEPTH_CEILING` /
    // `init_thread_exec_depth_ceiling`). The previous hard-coded `10_000`
    // was calibrated for a 64 MiB stack and overflowed the 8 MiB worker
    // carriers' native stack — a hard, uncatchable process abort — before
    // it tripped. Throwing here yields a *catchable* `StackOverflowError`.
    let ceiling = EXEC_DEPTH_CEILING.with(|c| c.get());
    if depth > ceiling {
        EXEC_DEPTH.with(|d| {
            let v = d.get();
            d.set(v.saturating_sub(1));
        });
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }
    let _exec_depth_guard = DepthGuard;

    // letsgo postmortem instrumentation: record every bytecode-method
    // entry into the global dispatch ring. Gated by `CRATONVM_DBG_LETSGO=1`
    // (cheap atomic-bool check on the disabled path).
    if crate::dispatch_trace::is_enabled() {
        let class_name_owned = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("cid#{class_id:?}"));
        crate::dispatch_trace::record_bytecode(
            // Widening: small integer index -> usize (non-negative, fits in pointer width)
            thread.thread_id.0 as usize,
            &class_name_owned,
            method_name,
            method_descriptor,
        );
    }
    if crate::runtime::env_cache::bd_debug() && method_name == "intValue" {
        eprintln!(
            "[interpreter::execute] class_id={:?} method={} desc={} args.len={}",
            class_id,
            method_name,
            method_descriptor,
            args.len()
        );
    }
    // CRATONVM_IAE_TRACE: log args when executing AnnotationScopeMetadataResolver.<init>
    //
    // Perf: the eprintln! only ever fires for `<init>` methods, so gate the
    // env-flag check + `class_manager.read()` RwLock acquire + `to_string()`
    // allocation behind the cheap `method_name == "<init>"` predicate first.
    // For every non-`<init>` call (the overwhelming majority) this path now
    // does zero work even when CRATONVM_IAE_TRACE is set. Behaviour is
    // identical — `method_name == "<init>"` was already a required conjunct.
    if method_name == "<init>" && crate::runtime::env_cache::iae_trace_os() {
        let class_name_for_trace = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        if class_name_for_trace.contains("AnnotationScopeMetadataResolver") {
            eprintln!(
                "[execute] {}.{}{} args={:?}",
                class_name_for_trace, method_name, method_descriptor, args
            );
        }
    }
    // Find the method
    //
    // S112r9 — when the resolved method has no Code attribute (abstract or
    // interface declaration), throw a real Java `AbstractMethodError` instead
    // of returning an opaque `VmError::Internal`. Java callers (e.g. Spring's
    // `try { ... } catch (Throwable t) { handleRunFailure(...); throw new
    // IllegalStateException(t); }`) can then catch and rewrap. Previously
    // this produced a Rust-side `MethodCallFailed::InternalError` that
    // propagated past Java try/catch handlers and was either converted to a
    // CLI `bail!()` (rc=1, JIT off) or — when it crossed a JIT dispatch
    // boundary — silently dropped (rc=0, JIT on). Either way Spring Boot
    // never reached `printBanner`. Throwing AbstractMethodError lets
    // SpringApplication.run() catch the failure, log it through its own
    // failure path, and at least produce a partial banner / startup-failure
    // banner before exiting.
    let (code_attr, source_file, class_name_str, is_synchronized, is_static) = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id).ok_or_else(|| VmError::Internal {
            message: format!("class {class_id} not found"),
        })?;
        let method = class
            .find_method(method_name, method_descriptor)
            .ok_or_else(|| {
                VmError::Linkage(LinkageError::NoSuchMethodError {
                    class_name: class.name.to_string(),
                    method_name: method_name.to_string(),
                    method_descriptor: method_descriptor.to_string(),
                })
            })?;
        let has_code = method.code().is_some();
        let is_synchronized = method.is_synchronized();
        let is_static = method.is_static();
        let class_name_owned = class.name.to_string();
        let code_attr_opt = method.code().cloned();
        let source_file = class.source_file.clone();
        drop(cm);
        if !has_code {
            // A native registered directly on the resolved class IS the
            // intended implementation of an otherwise-abstract method — e.g.
            // the synthetic `java/nio/channels/FileChannel`, whose
            // `read`/`write`/`size`/`position`/... natives back a real fd.
            // The receiver-walk rescue below only dispatches when the
            // target class differs from `class_id` (to avoid re-resolving
            // back through the same abstract declaration), so it misses the
            // case where the receiver's runtime class IS that abstract
            // class. A native is a concrete Rust fn — there is no
            // re-resolution loop — so dispatch straight to it.
            // Stream.forEachOrdered(Consumer) gap: its only native registration
            // lives in `register_phase56_stream_extras`, reachable solely from
            // `register_synthetic_overrides` (synthetic-jdk feature, compiled out
            // of the real-JDK CLI). So an `invokeinterface Stream.forEachOrdered`
            // resolves to the abstract interface declaration (no Code) and the
            // interface->concrete retarget that already makes `forEach` work does
            // not fire for `forEachOrdered`, surfacing as
            //   AbstractMethodError: Stream.forEachOrdered(...)V has no Code attribute
            // (24+ WildFly `ejb.security` tests, plus any real-JDK code using it).
            // For our sequential streams `forEachOrdered` is semantically identical
            // to `forEach`; re-dispatch as `forEach`, whose receiver-walk rescue
            // (Path A below) resolves the concrete override on the receiver.
            if method_name == "forEachOrdered"
                && method_descriptor == "(Ljava/util/function/Consumer;)V"
            {
                return execute(shared, thread, class_id, "forEach", method_descriptor, args);
            }
            // JVMTI redefine guard: when this class has been redefined in
            // place by an agent, its woven bytecode is authoritative — skip the
            // per-class native shadow so the interpreter runs the (instrumented)
            // body and the advice fires. Fast-pathed on `any_class_redefined`.
            //
            // JDK-ONLY-WAVE2: the `redefine_immune_*` predicates are
            // hard-coded class/method-name exception lists (defined in
            // `native_override.rs`, not owned here). They encode "this native
            // keeps winning even over instrumented bytecode", which is a §1.4
            // shadow decision taken outside `resolve_dispatch`. What should
            // replace them: `NativeKind` — exactly `Intrinsic` should be
            // redefine-immune, and everything else should yield to redefined
            // bytecode, with no name list at all. NOT deleted this wave; the
            // lists gate real Mockito/ByteBuddy behaviour.
            //
            // This site used to open-code `reflection && string_builder &&
            // path`, which is the aggregate MINUS five arms: `jfr`,
            // `bc_crypto_math`, `stamped_lock`, the `FileHandler` ctor/publish
            // group, and `synthetic_collection`. The last one is the one that
            // bites: a redefined `java/util/HashMap` reaching here lost its
            // registered native and ran real JDK bytecode against a CratonVM
            // synthetic object, which `redefine_immune_synthetic_collection_native`
            // says can never work.
            //
            // That is the SAME defect `layout_immunity_is_not_open_coded`
            // exists to prevent, and it was invisible to it: that gate
            // `include_str!`s `native_override.rs` and polices only its own
            // file, while this hand-rolled chain lives here. The gate now scans
            // its siblings too — a fifth staleness mode, after the three its
            // own comment lists and the CRLF one below them.
            let class_redefined = crate::classloading::any_class_redefined()
                && shared
                    .classes
                    .class_manager
                    .read()
                    .class_redefine_generation(class_id)
                    > 0
                && !redefine_immune_forced_native(
                    &class_name_owned,
                    method_name,
                    method_descriptor,
                );
            if method_name != "<init>" && method_name != "<clinit>" && !class_redefined {
                // JDK-only §7, resolved-class native. `bytecode_available =
                // false`: this whole arm only runs when the resolved method has
                // NO `Code` attribute, so the registered native is the only
                // implementation the method has (§7 step 3b) — there is no
                // bytecode for it to shadow, and §1.4 is not in play.
                match resolve_native_for_dispatch(
                    shared,
                    &class_name_owned,
                    method_name,
                    method_descriptor,
                    false,
                ) {
                    Ok(Some(cb)) => {
                        return crate::vm::safe_native_call(shared, thread, cb, args);
                    }
                    Ok(None) => {}
                    Err(violation) => {
                        return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
                    }
                }
            }
            // S111r10 — interface-dispatch receiver-walk fallback. The
            // canonical Spring Boot fat-jar tripwire is
            // `HashSet.iterator()` line 183 = `map.keySet().iterator()`:
            // the inner `iterator()` is `invokeinterface Set.iterator`, but
            // dispatch resolves to the abstract `Set.iterator` declaration
            // (no Code) instead of the receiver's concrete override.
            // Before throwing AbstractMethodError, walk the receiver's
            // runtime-class chain for a same-name+descriptor method that
            // does have Code (or a registered native), and dispatch
            // through there. This generalises the S111r7/r8 collection-view
            // rescue to any interface-method call where the cp class
            // resolved to an abstract declaration but the receiver carries
            // a concrete override on its real runtime class.
            //
            // Guards:
            //  * Only attempts the rescue for non-`<init>` instance methods
            //    (`<init>` and `<clinit>` aren't virtually dispatched).
            //  * Only fires when the receiver's runtime class differs from
            //    `class_id` AND is a non-interface concrete class — keeps
            //    the rescue from looping back through the same abstract
            //    declaration.
            //  * Bytecode dispatch is delegated through
            //    `invoke_on_class_shared_no_retarget` on the receiver's
            //    class so `find_method_recursive` walks superclasses
            //    starting from the receiver, NOT from the interface
            //    declaration we just came from.
            if method_name != "<init>" && method_name != "<clinit>" {
                if let Some(Value::Object(Some(recv_obj))) = args.first().copied() {
                    let recv_cid = shared.mem.heap.class_id_of(recv_obj);
                    let recv_kind = shared.mem.heap.kind_of(recv_obj);
                    // Round 7 — receiver-is-lambda-proxy rescue. When an
                    // invokeinterface lands on an interface declaration with no
                    // Code (e.g. `CacheOverride.close()V`) but the receiver is
                    // a lambda proxy implementing that interface (e.g.
                    // `SoftReferenceConfigurationPropertyCache#NOOP`,
                    // declared as `CacheOverride o = () -> {}`), route through
                    // try_lambda_dispatch so the proxy's SAM impl_handle runs
                    // instead of throwing AbstractMethodError.
                    let recv_is_lambda =
                        shared.classes.lambda_proxies.read().contains_key(&recv_cid);
                    if recv_is_lambda {
                        let rest = if args.is_empty() { &[][..] } else { &args[1..] };
                        if let Some(inner) = try_lambda_dispatch(
                            shared,
                            thread,
                            recv_obj,
                            recv_cid,
                            method_name,
                            method_descriptor,
                            rest,
                        )? {
                            return Ok(inner);
                        }
                        if let Some(inner) = try_lambda_default_method_dispatch(
                            shared,
                            thread,
                            recv_cid,
                            method_name,
                            method_descriptor,
                            args,
                        )? {
                            return Ok(inner);
                        }
                    }
                    // Receiver-is-annotation-proxy rescue. An annotation
                    // member call (e.g. JUnit5's `ExtendWith.value()`)
                    // resolves to the abstract interface declaration (no
                    // Code), but the receiver is one of our synthetic
                    // `java/lang/annotation/AnnotationProxy` objects whose
                    // members live in its name/value element arrays — there
                    // is no bytecode body to find anywhere. Route through
                    // the annotation-proxy element dispatch (same handler
                    // the direct invoke path at `execute_invoke` uses)
                    // instead of throwing AbstractMethodError.
                    if recv_kind == cratonvm_types::ObjectKind::Object
                        && shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(recv_cid)
                            .map(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
                            .unwrap_or(false)
                    {
                        let rest = if args.is_empty() { &[][..] } else { &args[1..] };
                        let result = crate::vm::annotation_proxy_invoke_shared(
                            shared,
                            thread,
                            recv_obj,
                            method_name,
                            rest,
                        )?;
                        // Unbox primitive-returning members. The proxy stores
                        // its element values as boxed wrappers, but an
                        // `invokeinterface <Ann>.member()` whose declared return
                        // is a primitive needs the UNBOXED value — otherwise the
                        // wrapper reference is reinterpreted as the primitive
                        // (e.g. ByteBuddy reads `@Advice.OnMethodEnter.skipOnIndex()`
                        // and sees the Integer object pointer — a positive int —
                        // instead of -1, so `RelocationHandler.ForType.of` throws
                        // "void is not an array type but an index for a
                        // relocation is defined" and Hibernate's BytecodeProvider
                        // service-load fails). Mirror the direct AnnotationProxy
                        // dispatch path in `execute_invoke`.
                        if let Some(value) = result {
                            let ret_char = method_descriptor
                                .rsplit(')')
                                .next()
                                .unwrap_or("L")
                                .chars()
                                .next()
                                .unwrap_or('L');
                            let unboxed = match ret_char {
                                'I' | 'Z' | 'B' | 'C' | 'S' | 'J' | 'F' | 'D' => {
                                    if let Value::Object(Some(obj)) = value {
                                        shared.mem.heap.get_field(obj, 0)
                                    } else {
                                        value
                                    }
                                }
                                _ => value,
                            };
                            return Ok(Some(unboxed));
                        }
                        return Ok(None);
                    }
                    // Receiver-own-class native rescue (general). When the
                    // resolved method has no Code, but a native is registered
                    // on the receiver's OWN runtime class (or a superclass) for
                    // the same name+descriptor, dispatch to it — provided the
                    // receiver has no real bytecode override (those are handled
                    // by Path A below). This covers synthetic objects stamped
                    // with an interface/abstract runtime class that carry their
                    // method natives on that exact name, regardless of whether
                    // the class store flags it `interface` — e.g. the
                    // scheduled-executor shim's `ScheduledFuture` object, whose
                    // `cancel(Z)Z` native is what JUnit's `@Timeout` finally
                    // block needs (the cp dispatch resolved to the abstract
                    // `Future.cancel`, so the resolved-class native check above
                    // missed it). Without this, every `@Timeout` test throws
                    // `AbstractMethodError: Future.cancel(Z)Z has no Code`.
                    if recv_cid != ClassId::new(0) {
                        let (recv_native_cb, has_bytecode, jdk_only_violation) = {
                            let cm2 = shared.classes.class_manager.read();
                            let bytecode = crate::classloading::find_method_recursive(
                                recv_cid,
                                method_name,
                                method_descriptor,
                                &cm2.class_store,
                            )
                            .map(|(m, _)| m.code().is_some())
                            .unwrap_or(false);
                            let mut cb = None;
                            let mut violation = None;
                            // JDK-only §7, receiver-hierarchy native rescue.
                            //
                            // The walk used to run unconditionally and its result
                            // was then discarded whenever `has_bytecode` held.
                            // Hoisting that test keeps the §4 census honest — a
                            // resolution that can never dispatch must not be
                            // counted as an invocation — and is otherwise
                            // behaviour-identical: `recv_native_cb` has no other
                            // reader, so the walk was pure work in that case.
                            if !bytecode {
                                let mut walk = Some(recv_cid);
                                while let Some(cid) = walk {
                                    if let Some(cls) = cm2.class_store.get(cid) {
                                        match resolve_native_for_dispatch(
                                            shared,
                                            &cls.name,
                                            method_name,
                                            method_descriptor,
                                            // §7 step 3b: nothing in the
                                            // receiver's chain has `Code` for
                                            // this signature (that is what
                                            // `!bytecode` means), so a
                                            // registered native is the only
                                            // implementation and shadows
                                            // nothing.
                                            false,
                                        ) {
                                            Ok(Some(found)) => {
                                                cb = Some(found);
                                                break;
                                            }
                                            // Under `JdkOnly` a `SyntheticStub`
                                            // here is a refusal, NOT a reason to
                                            // keep walking: continuing to the
                                            // superclass would be precisely the
                                            // silent fallback §1.3 forbids.
                                            Err(v) => {
                                                violation = Some(v);
                                                break;
                                            }
                                            Ok(None) => {}
                                        }
                                        walk = cls.superclass;
                                    } else {
                                        break;
                                    }
                                }
                            }
                            (cb, bytecode, violation)
                        };
                        if let Some(violation) = jdk_only_violation {
                            return Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                                violation,
                            )));
                        }
                        if !has_bytecode {
                            if let Some(cb) = recv_native_cb {
                                let r = crate::vm::safe_native_call(shared, thread, cb, args)?;
                                return Ok(r);
                            }
                        }
                    }
                    // Path A — receiver carries a real (non-zero) class_id.
                    //   Walk its runtime-class chain for a same-signature
                    //   override that has Code (or a registered native) and
                    //   dispatch through there. This catches the canonical
                    //   `HashSet.iterator()` → `map.keySet().iterator()`
                    //   chain when `keySet()` returned a concrete subclass
                    //   (e.g. HashMap$KeySet) but the cp dispatch resolved
                    //   to the abstract Set.iterator declaration.
                    if recv_cid != ClassId::new(0) {
                        let (recv_concrete, has_better, better_decl) = {
                            let cm2 = shared.classes.class_manager.read();
                            let concrete = cm2
                                .get_class(recv_cid)
                                .map(|c| !c.is_interface())
                                .unwrap_or(false);
                            let (better, decl) = if concrete {
                                match crate::classloading::find_method_recursive(
                                    recv_cid,
                                    method_name,
                                    method_descriptor,
                                    &cm2.class_store,
                                ) {
                                    Some((m, d)) => (m.code().is_some(), Some(d)),
                                    None => (false, None),
                                }
                            } else {
                                (false, None)
                            };
                            (concrete, better, decl)
                        };
                        // JDK-only §7: this is a *routing* probe, not a dispatch.
                        // It only decides whether Path A retargets to
                        // `invoke_on_class_shared_no_retarget`, which is itself
                        // routed through `resolve_dispatch` — so the policy
                        // decision is taken there, with the resolved `&Class` /
                        // `&Method` in hand, and no invocation is counted here
                        // (nothing is invoked here).
                        //
                        // The kind is deliberately NOT filtered: refusing to
                        // retarget on a `SyntheticStub` under `JdkOnly` would
                        // convert what should be a structured
                        // `SyntheticNativeInvocation` refusal into a silent
                        // `AbstractMethodError` from the fall-through below.
                        // `find_with_kind` is used anyway so the probe is on a
                        // kind-aware API (identical cost — same `slot_for_exact`
                        // fast path, same descriptor-quirk fallback).
                        let recv_native = if recv_concrete {
                            let cm2 = shared.classes.class_manager.read();
                            let mut walk = Some(recv_cid);
                            let mut found = false;
                            while let Some(cid) = walk {
                                if let Some(cls) = cm2.class_store.get(cid) {
                                    if shared
                                        .natives
                                        .native_methods
                                        .find_with_kind(&cls.name, method_name, method_descriptor)
                                        .is_some()
                                    {
                                        found = true;
                                        break;
                                    }
                                    walk = cls.superclass;
                                } else {
                                    break;
                                }
                            }
                            found
                        } else {
                            false
                        };
                        let target_cid = if has_better {
                            better_decl.unwrap_or(recv_cid)
                        } else {
                            recv_cid
                        };
                        if recv_concrete && (has_better || recv_native) && target_cid != class_id {
                            return crate::vm::invoke_on_class_shared_no_retarget(
                                shared,
                                thread,
                                target_cid,
                                method_name,
                                method_descriptor,
                                args,
                            );
                        }
                    }
                    // Path B — receiver is a synthetic alloc with cid=0
                    //   (no class_id ever stamped onto its header) and the
                    //   cp class is a well-known collection interface. The
                    //   receiver shape matches no concrete class in our
                    //   class store, but the registered native for the
                    //   canonical concrete subclass (e.g. HashSet for Set,
                    //   HashMap$KeyItr for Iterator) implements the
                    //   external contract correctly. Look up that native
                    //   and dispatch through it. This generalises the
                    //   S111r7/r8 collection-view rescue to the case where
                    //   the cp dispatch class is the *interface* itself
                    //   (Set/Iterator/Collection/Map/List).
                    let recv_is_iface = {
                        let cm2 = shared.classes.class_manager.read();
                        cm2.get_class(recv_cid)
                            .map(|c| c.is_interface())
                            .unwrap_or(false)
                    };
                    if recv_cid == ClassId::new(0)
                        || recv_kind == cratonvm_types::ObjectKind::Array
                        || recv_is_iface
                    {
                        // Map well-known interfaces -> canonical concrete
                        // class whose natives we register.
                        //
                        // JDK-ONLY-WAVE2: hard-coded class-name exception list.
                        // This substitutes a *different class's* native for an
                        // unresolvable interface call — a compatibility
                        // substitution in the §1 sense, and one that is silent:
                        // the receiver is not an instance of `canonical`. What
                        // should replace it: a real interface-method resolution
                        // (JVMS §5.4.3.4 `selectMethod` over the receiver's
                        // runtime class), with the `ClassOrigin` of the receiver
                        // deciding whether a shim receiver is even legal. Under
                        // `JdkOnly` real class bytes make every one of these
                        // interfaces resolvable, so the map should become
                        // unreachable rather than conditional. NOT deleted this
                        // wave — removing shim mappings has regressed real-JDK
                        // boot before.
                        let canonical: &'static str =
                            canonical_concrete_for_interface(&class_name_owned);
                        // G13-1, 2026-08-17. An ARRAY receiver cannot be an
                        // instance of ANY interface this map covers: JLS 4.10.3
                        // gives an array type exactly two superinterfaces,
                        // `java.lang.Cloneable` and `java.io.Serializable`, and
                        // neither is in the list. So for an array the
                        // substitution is not "a shim that is probably right" —
                        // it is provably wrong, and it is the shape that hides
                        // the wrongness best, because every one of the canonical
                        // natives reads its receiver through an `elementData`/
                        // bucket layout an array does not have and reports
                        // **empty** rather than refusing.
                        //
                        // MEASURED, 2026-08-17, on the `d87dff06a`+2 binary:
                        // `LinkedHashMap.values()` mints a real
                        // `LinkedHashMap$LinkedValues` carrier, whose
                        // `elementData` slot (absolute 1, from the real
                        // `java/util/ArrayList` layout) collides with the ONE
                        // member of that carrier family that declares two
                        // fields — `LinkedValues` has `reversed` at 0 and
                        // `this$0` at 1. The view's source-map read then hands
                        // native-collections the `Object[]` element buffer as
                        // though it were the backing `Map`, and it arrives here
                        // as `ctx.invoke("java/util/Map", "isEmpty", "()Z",
                        // [Object[11]])`. Proven by reflection on both VMs
                        // (`--add-opens java.base/java.util=ALL-UNNAMED`):
                        // HotSpot reports `this$0 -> java.util.LinkedHashMap`,
                        // CratonVM `this$0 -> [Ljava.lang.Object;[len=11]`.
                        // Under `Compatible` that substitution answered
                        // `isEmpty() == true` for a three-entry map and the
                        // whole `values()` view silently came back EMPTY; under
                        // `--jdk-only` the §8 refusal below turned it into an
                        // `AbstractMethodError` naming `java/util/Map` — which
                        // reads as an interface-door defect and is not one.
                        //
                        // Refusing here is what makes the two modes agree and
                        // what puts the receiver's real shape in the message.
                        // The blast radius is MEASURED, not argued: across all
                        // 105 corpus main classes, in `--jdk-only` and in
                        // `Compatible`, `[CANONICAL_CENSUS]` reports this map
                        // firing exactly ONCE — `java/util/Map -> java/util/
                        // HashMap isEmpty 1`, in `RJdkMapViews`, the vector this
                        // record is about. No currently-green vector reaches it.
                        //
                        // The root cause is the slot collision, and it lives in
                        // `native-collections/src/lib.rs` (nominated in
                        // `G13-1-…-20260817.md`). This site cannot fix it; it
                        // can stop laundering it into a wrong answer.
                        if !canonical.is_empty() && recv_kind == cratonvm_types::ObjectKind::Array {
                            let msg = format!(
                                "array receiver does not implement the requested interface \
                                 {class_name_owned} (dispatching \
                                 {class_name_owned}.{method_name}{method_descriptor})"
                            );
                            match super::exceptions::create_exception_object(
                                shared,
                                thread,
                                "java/lang/IncompatibleClassChangeError",
                                Some(&msg),
                            ) {
                                Ok(exc) => {
                                    return Err(MethodCallFailed::ExceptionThrown(exc));
                                }
                                // Same fallback the AbstractMethodError path
                                // below takes: never lose the diagnostic to a
                                // heap exhaustion during exception construction.
                                Err(_) => {
                                    return Err(MethodCallFailed::InternalError(
                                        VmError::Internal { message: msg },
                                    ));
                                }
                            }
                        }
                        // JDK-ONLY-WAVE2 §8, 2026-08-06. The record says: "Under
                        // `JdkOnly` real class bytes make every one of these
                        // interfaces resolvable, so the map should become
                        // unreachable rather than conditional." That is now
                        // enforced instead of hoped for.
                        //
                        // Substituting a DIFFERENT class's native for an
                        // unresolvable interface call is a compatibility
                        // substitution in the §1 sense and a silent one — the
                        // receiver is not an instance of `canonical`, so
                        // `HashMap$KeyItr`'s native runs against something that
                        // is not one. Strict mode may not do that quietly.
                        //
                        // Measured before changing anything: across all 53
                        // regression-corpus classes the map fires **zero**
                        // times, in `--real-jdk` and `--jdk-only` alike
                        // (`CRATONVM_DBG_CHECK_OVERRIDE=1`, `[CANONICAL_CENSUS]
                        // rows=0` in both). So this is a guard against a
                        // regression, not a live path being taken away — which
                        // is also why it is a refusal and not a rewrite: the
                        // record warns that "removing shim mappings has
                        // regressed real-JDK boot before", and `Compatible`
                        // keeps the mapping untouched.
                        if !canonical.is_empty() && crate::vm::dispatch_policy(shared).is_jdk_only()
                        {
                            crate::vm::record_canonical_substitution(
                                &class_name_owned,
                                canonical,
                                method_name,
                            );
                            crate::vm::record_interface_substitution_refusal(
                                &class_name_owned,
                                canonical,
                                method_name,
                                method_descriptor,
                            );
                            // Fall through to the ordinary no-native handling
                            // below, which raises the resolution error the JVMS
                            // calls for. Deliberately NOT a silent `Ok(None)`.
                        } else if !canonical.is_empty() {
                            crate::vm::record_canonical_substitution(
                                &class_name_owned,
                                canonical,
                                method_name,
                            );
                            // JDK-only §7. `bytecode_available = false`
                            // throughout: we are in the no-`Code` arm and the
                            // receiver matched no concrete class, so a
                            // registered native is the only implementation
                            // (§7 step 3b).
                            match resolve_native_for_dispatch(
                                shared,
                                canonical,
                                method_name,
                                method_descriptor,
                                false,
                            ) {
                                Ok(Some(cb)) => {
                                    let r = crate::vm::safe_native_call(shared, thread, cb, args)?;
                                    return Ok(r);
                                }
                                Ok(None) => {}
                                // §1.3: a refused stub must NOT fall through to
                                // the interface-name probe below. Falling
                                // through would be a silent second attempt at
                                // exactly the substitution strict mode refused.
                                Err(violation) => {
                                    return Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                                        violation,
                                    )));
                                }
                            }
                            // Also try the cp class itself — natives may be
                            // registered directly on the interface name.
                            match resolve_native_for_dispatch(
                                shared,
                                &class_name_owned,
                                method_name,
                                method_descriptor,
                                false,
                            ) {
                                Ok(Some(cb)) => {
                                    let r = crate::vm::safe_native_call(shared, thread, cb, args)?;
                                    return Ok(r);
                                }
                                Ok(None) => {}
                                Err(violation) => {
                                    return Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                                        violation,
                                    )));
                                }
                            }
                        }
                        // No native implements this interface method on the
                        // unrecognised receiver. The cp class resolves to an
                        // abstract method declaration, so fall through to the
                        // AbstractMethodError path below. Fabricating a benign
                        // result here is forbidden: it would make a non-empty
                        // collection silently appear empty.
                    }
                }
            }
            // Build an AbstractMethodError so Java try/catch can see it.
            let msg = format!(
                "method {class_name_owned}.{method_name}{method_descriptor} has no Code attribute"
            );
            if crate::runtime::env_cache::nocode_dbg() {
                let recv_info = match args.first().copied() {
                    Some(Value::Object(Some(r))) => {
                        let rc = shared.mem.heap.class_id_of(r);
                        let rn = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(rc)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| format!("<cid {rc}>"));
                        // G13-1: the KIND matters as much as the class here,
                        // and it was the missing half. `class_id_of` reports
                        // `ClassId(0)` for an array as well as for an
                        // unstamped synthetic object, and `get_class(0)`
                        // resolves to `java/lang/Object` — so the two shapes
                        // printed identically ("recv_cid=0
                        // recv_class=java/lang/Object") and a lane reading
                        // this line could not tell an `Object[]` receiver
                        // from a class-less allocation. That is exactly the
                        // distinction that separates "the interface door is
                        // missing a row" from "a native handed us the wrong
                        // object", and this record's whole first hypothesis
                        // was the wrong one of those two.
                        //
                        // `recv_is_declaring` is the second discriminator, and
                        // it separates the two mechanisms G13-1 measured behind
                        // one message. `true` means the receiver's runtime
                        // class IS the abstract/interface class the call
                        // resolved to — i.e. a native minted an instance of an
                        // abstract type and the method invoked on it has no
                        // native either (`HttpRequest.version()`,
                        // `PathMatcher.matches()`). `false` with
                        // `recv_kind=Array`, or with a class unrelated to the
                        // message, means something handed dispatch an object
                        // that is not an instance of the resolved type at all
                        // (`Map.isEmpty()` on an `Object[]`). The first needs a
                        // registration; the second needs the caller fixed. They
                        // are not the same bug and they print the same
                        // sentence.
                        let rk = shared.mem.heap.kind_of(r);
                        let recv_is_declaring = rc == class_id;
                        format!(
                            "recv_cid={rc} recv_class={rn} recv_kind={rk:?} \
                             recv_is_declaring={recv_is_declaring}"
                        )
                    }
                    other => format!("recv={other:?}"),
                };
                eprintln!("[DBG_NOCODE] {msg} | {recv_info}");
            }
            match super::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/AbstractMethodError",
                Some(&msg),
            ) {
                Ok(exc) => {
                    return Err(MethodCallFailed::ExceptionThrown(exc));
                }
                Err(_) => {
                    // Heap-exhausted or class-load failure during exception
                    // construction — fall back to the legacy InternalError so
                    // we never lose the diagnostic entirely.
                    return Err(MethodCallFailed::InternalError(VmError::Internal {
                        message: msg,
                    }));
                }
            }
        }
        // `has_code` is `true` here (the `!has_code` arm above always
        // returns), so `code_attr_opt` is `Some`. Route the impossible
        // `None` to a typed `VmError::Internal` instead of `.expect()` so
        // this hot dispatch file stays panic-free (NEW-7 / B3 gate).
        let code_attr = match code_attr_opt {
            Some(c) => c,
            None => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: "has_code true implies code present".to_string(),
                }));
            }
        };
        (
            code_attr,
            source_file,
            class_name_owned,
            is_synchronized,
            is_static,
        )
    };

    // If the JIT early-compile path encounters an exception from a callee
    // dispatch, we stash it here and fall through to the interpreter, which
    // pushes a frame and routes through the exception table.
    let mut jit_early_exception: Option<ObjectRef> = None;
    // The bci the compiled body stamped at the throw site that produced
    // `jit_early_exception`, captured at the drain because later work clears the
    // signal. `-1` means "not stamped"; see the routing site below for why this
    // sink cannot afford to route without it.
    let mut jit_early_throw_bci: i64 = -1;

    // Try JIT compilation for this method.
    {
        let skip_key: (Arc<str>, Arc<str>, Arc<str>) = (
            Arc::from(&*class_name_str),
            Arc::from(method_name),
            Arc::from(method_descriptor),
        );
        /// Announce a permanent `jit_skip_set` seal under `CRATONVM_DBG_JITC`.
        ///
        /// Sealing is STRICTLY STRONGER than a compile bail: a bail is retried
        /// (up to `MAX_TIER_FAIL_RETRIES`) and is reported by the tier
        /// manager, whereas a seal removes the method from every later path —
        /// including the invocation-counter upgrade that reaches
        /// `jit::try_compile`'s relaxed, dataflow-based gates. A sealed method
        /// is not merely uncompiled, it is UNTRACKED: it appears nowhere in
        /// `CRATONVM_DBG=jit-method-stats`, not even as hot-but-stuck, so
        /// "this method is never even considered" had no observable signal at
        /// all before this line.
        fn note_jit_skip_seal(site: &str, key: &(Arc<str>, Arc<str>, Arc<str>)) {
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some() {
                eprintln!(
                    "[cratonvm-jitc] jit-skip-seal site={site} {}.{}{}",
                    key.0, key.1, key.2
                );
            }
        }
        let already_skipped = shared.jit.jit_skip_set.read().contains(&skip_key);
        // PERF FIX (2026-07-15, companion to the invoke-cache + jit-bail-list
        // memoization fixes): `already_skipped` (a single `jit_skip_set`
        // RwLock-read + hash lookup) was computed first but NOT used to
        // short-circuit the expensive eligibility computation below — every
        // call to `execute()` for an already-skip-listed method still paid
        // the `class_manager` read lock, `should_skip_jit_with_init`'s
        // policy walk, the ForkJoinTask ancestor-chain check
        // (`is_fjp_subclass_blocklisted`), a `native_methods.find` hash
        // lookup, AND — worst case — `jit_method_calls_native_shadowed`'s
        // full O(method-bytecode-size) decode-and-scan, only to have the
        // combined `if` condition below discard every one of those results
        // in favor of the already-known `already_skipped == true` verdict.
        // Once a method is skip-listed it can never leave the list within
        // this process (mirrors the `mark_jit_bail_listed` invariant this
        // same session's other fix relies on), so none of this is needed
        // when `already_skipped` is true — skip straight to cheap defaults.
        // The POSITIVE half of the same short-circuit. `already_skipped` covers
        // methods that FAIL this gate; a method that PASSES was recorded
        // nowhere, so it re-ran the whole computation on every `execute()`
        // entry — forever, and *before* the `JitCache` consult in the `else`
        // branch below, so a fully compiled hot method paid it too. The
        // expensive term is `jit_method_calls_native_shadowed`: an
        // O(method-bytecode) decode with a three-string-hash `slot_for_exact`
        // probe per invoke instruction in the body. Measured on netty
        // `AdaptiveByteBufAllocatorTest`, that scan reached 2.15% of CPU
        // through `slot_for_exact` alone while only 637 methods were ever
        // sealed for the reason it computes — it was re-running, not running
        // once per method.
        //
        // Stamped with `redefine_epoch()` because a stale PASS is unsafe in a
        // way a stale seal is not: see `JitRealm::jit_gate_pass`.
        // Keyed on `ClassId`, not on the class name the negative set uses — see
        // `JitRealm::jit_gate_pass` for why the name is safe there and unsafe
        // here. The two `Arc` clones are refcount bumps, not allocations.
        let gate_pass_key = (skip_key.1.clone(), skip_key.2.clone());
        let gate_pass_memo = if already_skipped || !crate::runtime::env_cache::jit_gate_pass_memo()
        {
            None
        } else {
            let epoch = cratonvm_jit::redefine_epoch();
            shared
                .jit
                .jit_gate_pass
                .read()
                .get(&(class_id, gate_pass_key.0.clone(), gate_pass_key.1.clone()))
                .copied()
                .and_then(|(e, iface)| (e == epoch).then_some(iface))
        };
        let (is_interface_default, static_skip_reason, fjp_skip, native_skip) = if already_skipped {
            (false, None, false, false)
        } else if let Some(is_interface_default) = gate_pass_memo {
            // Recorded eligible under the current redefine epoch: all three
            // skip reasons were false when it was recorded, and each is a pure
            // function of this method's static bytecode and metadata.
            cratonvm_jit::note_jit_gate_pass_hit();
            (is_interface_default, None, false, false)
        } else {
            // Static eligibility check — see vm/src/jit/skip_list.rs for the full
            // policy mapping (each entry is documented against a roadmap item in
            // docs/roadmap.md Phase A1).
            let is_interface_default = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id).map_or(false, |c| c.is_interface())
            };
            // The static JIT ban list was deleted 2026-07-31 (see
            // docs/known-issues/jit-bans/jit-bans-all-disabled-20260731.md).
            // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
            // remaining force-interpret lever, applied in `jit::try_compile`.
            let policy = ();
            // T1.1.f — classify init complexity so trivial `<init>`/`<clinit>`
            // methods (just `aload_0; invokespecial; return`) become
            // JIT-eligible. The classifier walks the bytecode and returns
            // `Trivial` iff there are no field stores, no synchronization,
            // and no invokedynamic. For any other method name, the
            // classifier result is `Unknown` (the classifier is only
            // consulted for `<init>`/`<clinit>`).
            let _ = policy;
            let static_skip_reason: Option<()> = None;
            // RFJP.1 (RETIRED, lever-only) — see `is_fjp_subclass_blocklisted`.
            // Inert unless `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1`.
            let fjp_skip = is_fjp_subclass_blocklisted(shared, &class_name_str, Some(class_id));
            // S111r15 - refuse to JIT a method directly backed by a Rust native at
            // this FIRST-CALL compile path too. Without this, `Character.toLowerCase(C)C`
            // bypassed the native override and corrupted Spring property-name parsing.
            //
            // bytebuddy_probe / ANTLR cold-path follow-up: do not reject every
            // bytecode override merely because an ancestor has an identity native
            // (`Object.equals`/`hashCode`/`toString`). A real override shadows that
            // native and can compile. Keep the ByteBuddy safety case by rejecting
            // methods whose own bytecode contains an invoke that resolves to a
            // native-shadowed target such as `Object.equals`.
            //
            // JDK-only §7: a compile-eligibility probe, not a resolution — no
            // dispatch happens here and no invocation is counted. Every kind
            // must keep suppressing the compile: an `Intrinsic` legitimately
            // supersedes bytecode (§1.4) so compiling the bytecode would be
            // wrong, a `Bridge`/`SyntheticStub` shadow is adjudicated by
            // `resolve_dispatch` on the interpreter path, and under `JdkOnly` a
            // `SyntheticStub` must reach that adjudication to be *refused*
            // rather than be quietly compiled around. So the kind is read
            // (`find_with_kind`, identical cost) but deliberately not filtered.
            let native_skip = if native_override::registered_native_will_run(
                shared,
                &class_name_str,
                method_name,
                method_descriptor,
            ) {
                true
            } else if !crate::runtime::env_cache::jit_native_shadow_caller_seal() {
                // MEASUREMENT LEVER ONLY — `CRATONVM_JIT=-native-shadow-caller-seal`.
                //
                // This seal is a CORRECTNESS guard: a compiled direct call
                // bypasses the interpreter's native-vs-bytecode decision, so a
                // caller compiled in spite of it can enter JDK bytecode the VM
                // deliberately replaced. Running with it off is expected to
                // MISBEHAVE, and it must never be a shipping configuration.
                //
                // It exists because the seal excludes 1,281 methods from the JIT
                // on a Spring Boot context startup — more than the 1,155 that
                // reach C2 — and the per-arm census shows the population is
                // dominated by PRECISE hits (`direct=1015`), not by the
                // class-blind arm (169, whose removal was measured worth
                // nothing). So the question is no longer "is the detection too
                // wide" but "is the per-METHOD granularity worth replacing with
                // per-SITE", and that is a large compiler change. Pricing the
                // ceiling first is cheaper than building it: if the whole seal
                // is worth ~0 on this workload, the change should not be
                // attempted at all.
                false
            } else {
                jit_method_calls_native_shadowed(
                    shared,
                    class_id,
                    &code_attr.code,
                    code_attr.code.len(),
                )
            };
            // Record the ELIGIBLE verdict so the next entry short-circuits.
            // Scoped to exactly the three static per-method facts the seal
            // block below scopes itself to — `env_disable_jit` /
            // `redefine_jit_quiesced` / `gpu_gate_skip` / `clinit_skip` are
            // runtime or call-site-dependent and are deliberately NOT folded
            // in, in either direction.
            if crate::runtime::env_cache::jit_gate_pass_memo()
                && static_skip_reason.is_none()
                && !fjp_skip
                && !native_skip
            {
                cratonvm_jit::note_jit_gate_pass_fill();
                shared.jit.jit_gate_pass.write().insert(
                    (class_id, gate_pass_key.0.clone(), gate_pass_key.1.clone()),
                    (cratonvm_jit::redefine_epoch(), is_interface_default),
                );
            }
            (
                is_interface_default,
                static_skip_reason,
                fjp_skip,
                native_skip,
            )
        };
        // DEBUG diagnostic — print every JIT compile decision for the
        // LazyProjection.equals method while bytebuddy_probe diagnosis
        // is in progress. Remove after fix lands.
        if crate::runtime::env_cache::dbg_bblp()
            && class_name_str.contains("LazyProjection")
            && method_name == "equals"
        {
            eprintln!(
                "[BBLP-firstcall] class={} method={} desc={} native_skip={} static_skip={:?} fjp_skip={} already_skipped={}",
                &*class_name_str, method_name, method_descriptor,
                native_skip, static_skip_reason, fjp_skip, already_skipped,
            );
        }
        // Kill-switch: CRATONVM_DISABLE_JIT=1 forces interpreter-only execution.
        // Mirrors the gates in `try_jit_compile_callee` / `try_jit_upgrade_with_gate` /
        // `try_osr` so the user-facing CRATONVM_DISABLE_JIT flag actually disables
        // the FIRST-CALL JIT compile path here too.
        // Continuations currently freeze precise interpreter frames. Until a
        // compiled activation can deopt directly into a heap stack chunk, a
        // virtual thread must not enter machine code that may cross a yield.
        // This is a per-thread execution gate; platform threads still compile
        // and use the shared artifacts normally.
        //
        // The BISECT levers ride along here for the same reason. This block's
        // eager first-call `x64::compile_with_param_slots` (further down)
        // reaches the backend WITHOUT going through `cratonvm_jit::try_compile`,
        // which is where `CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY` used
        // to be applied — so the single-pass tier was force-interpretable by
        // neither lever, and a bisect that could not stop the compile read as
        // an exoneration. Folded into `env_disable_jit` rather than into the
        // static-reason group below on purpose: that group SEALS the method
        // into `jit_skip_set`, and these are process-wide diagnostic flags, not
        // per-method facts about the bytecode. See
        // `cratonvm_jit::jit_force_interpret`.
        let env_disable_jit = crate::runtime::env_cache::disable_jit()
            || matches!(thread.kind, crate::threading::ThreadKind::Virtual)
            || cratonvm_jit::jit_force_interpret(&class_name_str, method_name);
        // Redefinition does NOT permanently bar a class from compiling.
        //
        // This used to be `class_was_redefined(shared, class_id)`. The
        // generation only ever increases, so that predicate is true forever
        // once a class has been redefined — and it sat in the skip set below,
        // which meant a class redefined once was condemned to the interpreter
        // for the life of the process. That is the whole of the "every call
        // into a redefined class costs ~33 µs" defect: one
        // `Mockito.mock(Foo.class)` permanently de-optimized `Foo`. Measured
        // on `docs/known-issues/repros/redefine-call-cost`: 691 ns/call
        // compiled before the redefine, 33,773 ns/call interpreted after it,
        // from redefining with *byte-identical* bytecode.
        //
        // It was also protecting nothing. `redefine_class` already evicts
        // every compiled artifact — `jit_cache.write().clear_all()` plus
        // `invalidate_jit_for_class` in `vm_exec.rs` — so no code compiled
        // from the old body can survive the redefinition, and a later
        // compilation necessarily reads the current (agent-woven) bytecode
        // out of the class store. Blocking recompilation on top of a full
        // eviction is belt-and-braces that only costs throughput.
        //
        // What is deliberately NOT claimed: this does not order a compile
        // already in flight against a concurrent redefinition. A body
        // compiled from generation N could in principle publish after the
        // `clear_all()` for generation N+1. That race predates this change
        // (it applies to the first redefinition of any class), and closing it
        // wants a publish-time generation check rather than a permanent ban.
        //
        // `RedefineCorrectnessProbe` in the repro directory is the guard: it
        // redefines with a body whose arithmetic differs, then drives the
        // method hot, and fails if the recompiled code reverts to the old
        // body. That assertion was vacuous while this gate blocked
        // compilation, and is load-bearing now.
        let redefine_jit_quiesced = false;
        // GPU-offload JIT admission gate (known-issues followups item 2):
        // while `--gpu` is active, a caller whose bytecode contains an
        // offload-eligible invokestatic must stay interpreted, or its
        // JIT-compiled body would bypass the offload hook and silently
        // end GPU dispatch for that call site. One bool read when --gpu
        // is off; see offload_jit_gate for the cache/scan details.
        #[cfg(feature = "gpu-offload")]
        let gpu_gate_skip = crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
            shared,
            class_id,
            method_name,
            method_descriptor,
        );
        #[cfg(not(feature = "gpu-offload"))]
        let gpu_gate_skip = false;
        // A `<clinit>` must not take the eager FIRST-CALL compile door.
        //
        // Not a codegen-quality judgement — an ordering one. Entering a
        // compiled artifact runs the `static_init_classes` pre-walk a hundred
        // lines below, which `ensure_class_initialized`s the declaring class of
        // EVERY getstatic/putstatic site anywhere in the body, before bytecode
        // zero. For an ordinary method that is a sound approximation of JVMS
        // §5.5. For a `<clinit>` it inverts the very order the initializer
        // exists to establish, and the JDK has initializers whose correctness
        // is exactly that order:
        //
        //   `java/lang/constant/ConstantDescs.<clinit>` assigns
        //   `BSM_PRIMITIVE_CLASS` (line 198) and only then reads
        //   `PrimitiveClassDescImpl.CD_int` (line 249) — and
        //   `PrimitiveClassDescImpl.<clinit>`'s own ctor reads
        //   `ConstantDescs.BSM_PRIMITIVE_CLASS`. Hoisting the line-249 trigger
        //   to method entry runs that ctor against a null `BSM_PRIMITIVE_CLASS`
        //   → `NullPointerException` → `ExceptionInInitializerError`, and both
        //   classes are poisoned for the rest of the process. `ConstantDescs
        //   .<clinit>` then never executes a single putstatic. Deterministic:
        //   `MethodHandles.arrayElementVarHandle(int[].class)` reaches it
        //   through `MethodTypeForm` → `sun/invoke/util/Wrapper.<clinit>`.
        //
        // Only reachable with `CRATONVM_BG_COMPILE=0` today, because the
        // default background pipeline does not first-call-compile here — which
        // is why the documented opt-out was unusable on any real classpath.
        //
        // Refusing costs nothing: a `<clinit>` runs at most once per class per
        // loader, so a method-entry compile can never amortize its own codegen.
        // A `<clinit>` with a genuinely hot loop still reaches the OSR door,
        // which enters mid-body and does not run this pre-walk.
        let clinit_skip = method_name == "<clinit>";
        if env_disable_jit
            || redefine_jit_quiesced
            || already_skipped
            || static_skip_reason.is_some()
            || fjp_skip
            || native_skip
            || gpu_gate_skip
            || clinit_skip
        {
            // Method has known JIT issues — skip JIT.
            //
            // PERF FIX (2026-07-15, completes the `already_skipped` short-
            // circuit added above): `static_skip_reason` / `fjp_skip` /
            // `native_skip` are all pure functions of this method's static
            // bytecode+metadata (policy table lookup, ForkJoinTask ancestor
            // chain, native-shadow bytecode scan) — none of them can change
            // for a given class+method+descriptor within this process. Yet
            // this branch previously left `jit_skip_set` untouched, so
            // `already_skipped` could NEVER become true for a
            // native_skip/static_skip_reason/fjp_skip method: every future
            // call to `execute()` for it recomputed all three from scratch
            // (worst case, `jit_method_calls_native_shadowed`'s full
            // O(bytecode-size) decode-and-scan) forever, defeating the very
            // short-circuit `already_skipped` exists for. Seal it here, the
            // same way a permanent backend-compile failure already seals via
            // `mark_jit_bail_listed`/the `jit_skip_set.write().insert(...)`
            // calls in the compile-attempt branch below. Scoped to the
            // static per-method reasons only — `env_disable_jit` /
            // `redefine_jit_quiesced` / `gpu_gate_skip` are runtime or
            // call-site-dependent, not per-method-permanent, so they must NOT
            // poison this method's entry for future calls where those flags
            // may differ.
            // `clinit_skip` joins the sealed set for the same reason the other
            // three do: it is a pure function of the method name, so recomputing
            // it on every call is waste. (`<clinit>` is called once, so the seal
            // rarely pays — but leaving it out would make `already_skipped`
            // unreachable for it, which is the trap this seal exists to avoid.)
            if static_skip_reason.is_some() || fjp_skip || native_skip || clinit_skip {
                // Name WHICH of the four fired. The single label
                // `static-policy-or-native-shadow` covered all of them, and a
                // Spring Boot context startup seals 856 methods through here —
                // more than the 69 whose compile was attempted and refused —
                // with no way to tell a policy-table entry from a native-shadow
                // scan hit. Those want opposite fixes: one is a list somebody
                // can shorten, the other is a scan that may be over-matching.
                // Priority order, not a set: the reasons can co-occur, and the
                // first one listed is the one that would still seal the method
                // if every other were lifted.
                let seal_site = if static_skip_reason.is_some() {
                    "static-policy-table"
                } else if native_skip {
                    "calls-native-shadowed-method"
                } else if fjp_skip {
                    "forkjointask-subclass"
                } else {
                    "clinit"
                };
                note_jit_skip_seal(seal_site, &skip_key);
                // Counted as well as traced: `CRATONVM_DBG_JITC` produces ~1 GB
                // on a Spring startup, so the census has to be readable from the
                // one-line `jit-method-stats` dump instead.
                cratonvm_jit::note_jit_skip_seal_reason(seal_site);
                shared.jit.jit_skip_set.write().insert(skip_key.clone());
            }
        } else {
            {
                // RBC.5 — consult the JIT cache FIRST. An already-compiled method
                // needs NO `padded_bytecode` (alloc + memcpy), NO `jit_scan`
                // (linear bytecode walk) and NO Arc key allocations —
                // `JitCache::get` takes `&str`. The previous order paid all of
                // that on EVERY uncached invocation of every JIT-eligible
                // method. The org/bouncycastle blanket ban happened to
                // short-circuit it for BC code at the `static_skip_reason` gate,
                // which made LIFTING the ban look ~4.7× slower on the asn1
                // RegressionTest even when not a single BC method was compiled
                // (the ~190s CPU-bound anomaly in
                // docs/bc-jit-ban-investigation.md).
                let compiled = {
                    let jit_cache = shared.jit.jit_cache.read();
                    jit_cache.get(&class_name_str, method_name, method_descriptor, class_id)
                };
                // CRATONVM_JIT_C2_FIRST_CALL: set when the gated branch DEFERS a
                // not-yet-hot method (returns None to interpret rather than compile).
                // The first-call-failure seal below must NOT fire for that case —
                // sealing inserts `skip_key` into `jit_skip_set`, which makes the next
                // call's `already_skipped` gate skip the whole JIT block, permanently
                // stopping the invocation counter from ever re-running (so an
                // `execute`-only-reached hot method would never compile gate-ON). A
                // genuine hot-path backend bail (try_jit_compile_callee → None) leaves
                // this false and still seals, matching the gate-OFF semantics.
                let mut c2_not_hot = false;
                let compiled = compiled.or_else(|| {
                    // wire-tiered-manager Step 7 (full eager-reroute): when the
                    // background pipeline is the default (`bg_compile`, default-ON),
                    // the WORKER is the compiler — do NOT eager- or inline-compile on
                    // the mutator here. Count invocations and, once warm, ensure the
                    // worker + enqueue via the tiered manager, then return None to
                    // INTERPRET; the worker compiles off-thread and publishes into
                    // `jit_cache`, and a later invocation (here, for
                    // reflective/uncached-hot methods, or via the cached-dispatch
                    // fast-path) flips this call site to the Jit body. `c2_not_hot =
                    // true` keeps the counter running so the method is never sealed as
                    // permanently-uncompiled. Opt-out (`CRATONVM_BG_COMPILE=0`) falls
                    // through to the historical eager/inline paths below.
                    if crate::runtime::env_cache::bg_compile() {
                        let invoc_key = {
                            let mut h = 0u32;
                            for &b in method_name.as_bytes() {
                                h = h.wrapping_mul(31).wrapping_add(b as u32); // Widening: u8 → u32
                            }
                            for &b in method_descriptor.as_bytes() {
                                h = h.wrapping_mul(31).wrapping_add(b as u32); // Widening: u8 → u32
                            }
                            // Widening: u32 → u64 (value preserved)
                            ((class_id.as_u32() as u64) << 32) | (h as u64)
                        };
                        let n = shared.jit.profile_store.increment_invocation(invoc_key);
                        if n >= crate::runtime::env_cache::jit_invocation_threshold() {
                            ensure_bg_compiler_started(shared);
                            let tiered_key = crate::jit::tiered::MethodKey::new(
                                class_name_str.as_str(),
                                method_name,
                                method_descriptor,
                            );
                            // Pass the REAL per-method invocation count: this
                            // hook only fires at stride boundaries, and the
                            // manager's historical `+= 1` counting deflated its
                            // hotness view 64x (first C1 recommendation at
                            // ~threshold + 64×c1_threshold real calls).
                            let _ = shared
                                .jit.tiered_manager
                                .on_method_invocation_observed(&tiered_key, n as u64);
                        }
                        c2_not_hot = true; // keep the counter running; do not seal
                        return None; // interpret — the worker compiles off-thread
                    }
                    // activate-ir-optimizer (runtime wiring, CRATONVM_JIT_C2_FIRST_CALL,
                    // default-OFF): replace the eager single-pass first-call compile
                    // below with an invocation-counted upgrade through the optimizing
                    // IR pipeline. The eager `x64::compile` (further down) caches a
                    // non-IR body on call #1 and thereby preempts `jit::try_compile`
                    // on every later path. Here we instead: (a) count invocations and,
                    // below the warmup threshold, return None so the method INTERPRETS
                    // — which ALSO un-preempts the dispatcher/OSR warmup paths that are
                    // already wired to `try_compile(optimize=true)`, so a method that
                    // goes hot through the stackless dispatcher still reaches IR there;
                    // and (b) once hot, compile via `try_jit_compile_callee` →
                    // `jit::try_compile(optimize=true)`, which SUBSUMES single-pass
                    // (it falls back to `x64::compile_with_param_slots` internally for
                    // IR-incompatible bodies), then re-fetch the cached body. The
                    // single-pass block below is unreached while the flag is set.
                    if c2_first_call_enabled() {
                        let invoc_key = {
                            let mut h = 0u32;
                            for &b in method_name.as_bytes() {
                                // Widening: smaller value -> u32 (value fits)
                                h = h.wrapping_mul(31).wrapping_add(b as u32);
                            }
                            for &b in method_descriptor.as_bytes() {
                                // Widening: smaller value -> u32 (value fits)
                                h = h.wrapping_mul(31).wrapping_add(b as u32);
                            }
                            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                            ((class_id.as_u32() as u64) << 32) | (h as u64)
                        };
                        let n = shared.jit.profile_store.increment_invocation(invoc_key);
                        if n < crate::runtime::env_cache::jit_invocation_threshold() {
                            c2_not_hot = true; // defer, do NOT seal (counter must keep running)
                            return None; // not hot yet — interpret (dispatcher/OSR may reach IR)
                        }
                        return match try_jit_compile_callee(
                            shared,
                            &class_name_str,
                            method_name,
                            method_descriptor,
                            true, // optimize = C2 / optimizing IR pipeline
                        ) {
                            Some(_) => {
                                let jit_cache = shared.jit.jit_cache.read();
                                jit_cache.get(&class_name_str, method_name, method_descriptor, class_id)
                            }
                            None => None,
                        };
                    }
                    // ── The admission gate, third door ────────────────────
                    //
                    // This block reaches `x64::compile_with_param_slots`
                    // directly, like `compile_osr_artifact` and unlike
                    // `jit::try_compile`. It had already been taught the
                    // kill-switch and the bisect levers by hand (see
                    // `env_disable_jit` above) but never the permanent
                    // bail-list, the code-cache cap, or the compile-epoch
                    // witness — so an eager first-call compile could re-run the
                    // pipeline on a method the backend had permanently refused,
                    // commit code past a cap the ordinary door was respecting,
                    // and publish a body stamped at buffer finalize rather than
                    // from before the first constant-pool read.
                    // `compile_gate::admit` asks all of them; the token owns
                    // the epoch witness and must outlive the resolution below.
                    let admission = cratonvm_jit::compile_gate::admit(
                        &class_name_str,
                        method_name,
                        method_descriptor,
                        cratonvm_jit::compile_gate::CompileDoor::EagerFirstCall,
                    )
                    .ok()?;
                    let padded = crate::runtime::frame::padded_bytecode(&code_attr.code);
                    let code_len = code_attr.code.len();
                    let scan = match crate::jit::x64::jit_scan(&padded, code_len, method_descriptor)
                    {
                        Some(s) => s,
                        None => {
                            // RBC.4 — seal scan-rejected methods so this path
                            // doesn't re-run jit_scan on every uncached
                            // invocation.
                            note_jit_skip_seal("early-jit-scan-reject", &skip_key);
                            shared.jit.jit_skip_set.write().insert(skip_key.clone());
                            return None;
                        }
                    };
                    // RBC.6 — athrow methods need an EMPTY exception table
                    // (mirrors jit::try_compile_inner); seal otherwise so the
                    // probe isn't re-run per call.
                    if scan.has_athrow && !code_attr.exception_table.is_empty() {
                        note_jit_skip_seal("early-rbc6-athrow-with-handler", &skip_key);
                        shared.jit.jit_skip_set.write().insert(skip_key.clone());
                        return None;
                    }
                    let class_name_arc: Arc<str> = Arc::from(&*class_name_str);
                    let method_name_arc: Arc<str> = Arc::from(method_name);
                    let descriptor_arc: Arc<str> = Arc::from(method_descriptor);
                    // Resolve multianewarray entries if present
                    let mut mna_info = Vec::new();
                    if !scan.multianewarray_ops.is_empty() {
                        let cm = shared.classes.class_manager.read();
                        let class = cm.get_class(class_id)?;
                        for &(pc, cp_idx, _ndims) in &scan.multianewarray_ops {
                            // A malformed CP entry is still a whole-compile refusal.
                            let _ = class.constant_pool.get_class_name(cp_idx)?;
                            mna_info
                                .push((pc, crate::jit::pack_multianewarray_site(class_id.as_u32(), cp_idx)));
                        }
                    }
                    // Resolve typecheck entries (checkcast/instanceof) if present
                    let mut typecheck_info: Vec<(usize, *const u8, usize)> = Vec::new();
                    let mut owned_jit_strings: Vec<Box<str>> = Vec::new();
                    if !scan.typecheck_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        let class = cm_lock.get_class(class_id)?;
                        for &(pc, cp_idx) in &scan.typecheck_ops {
                            let class_name = class.constant_pool.get_class_name(cp_idx)?;
                            // Interned for the life of the process rather than
                            // owned by this compilation: `jit_checkcast` /
                            // `jit_instanceof` memoize on the `(ptr, len)` pair
                            // in thread-locals that outlive the compiled
                            // method, so a recycled address would answer for
                            // the class that used to live there. See
                            // `cratonvm_jit::intern_typecheck_target`.
                            //
                            // Resolved here through THIS class's own defining
                            // loader, exactly as the interpreter's constant-pool
                            // resolution would, and interned under that
                            // identity. The class dictionary is keyed by
                            // `(ClassLoaderId, name)`, so handing the runtime
                            // helper a bare name left it guessing between two
                            // loaders' same-named copies.
                            let target_id = cm_lock
                                .find_class_by_name_for_class(class_name, class_id)
                                .map(|id| id.as_u32());
                            let (ptr, len) =
                                cratonvm_jit::intern_typecheck_target(class_name, target_id);
                            typecheck_info.push((pc, ptr, len));
                        }
                    }
                    // Resolve static field entries (getstatic/putstatic) if present
                    let mut static_field_info: Vec<(usize, u32, usize, u8, bool)> = Vec::new();
                    if !scan.static_field_ops.is_empty() {
                        for &(pc, cp_idx) in &scan.static_field_ops {
                            let field = resolve_field_ref(shared, class_id, cp_idx).ok()?;
                            let cm_lock = shared.classes.class_manager.read();
                            let class = cm_lock.get_class(class_id)?;
                            let nat_idx = match class.constant_pool.get(cp_idx) {
                                Some(ConstantPoolEntry::FieldReference {
                                    name_and_type_index,
                                    ..
                                }) => *name_and_type_index,
                                _ => return None,
                            };
                            let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
                            let type_tag = *descriptor.as_bytes().first()?;
                            static_field_info.push((
                                pc,
                                field.declaring_class_id.as_u32(),
                                field.field_index,
                                type_tag,
                                field.is_volatile,
                            ));
                        }
                    }
                    // Resolve invoke entries (invokevirtual/invokespecial/invokeinterface) if present
                    let mut invoke_info: Vec<(usize, *const crate::jit::JitInvokeInfo)> =
                        Vec::new();
                    let mut owned_jit_invoke_infos: Vec<Box<crate::jit::JitInvokeInfo>> =
                        Vec::new();
                    let mut direct_calls_early: Vec<(usize, crate::jit::JitDirectCall)> =
                        Vec::new();
                    let mut mic_slots_early: Vec<(usize, *const crate::jit::JitMICSlot)> =
                        Vec::new();
                    let mut owned_mic_slots_early: Vec<Box<crate::jit::JitMICSlot>> =
                        Vec::new();
                    let mut pic_slots_early: Vec<(usize, *const crate::jit::JitPICSlot)> =
                        Vec::new();
                    let mut owned_pic_slots_early: Vec<Box<crate::jit::JitPICSlot>> =
                        Vec::new();
                    // Per-allocation ctor-dispatch elision: a `new C(); dup;
                    // invokespecial C.<init>()V` whose `C.<init>` is the empty
                    // default constructor (`is_elidable_construction`) has NO
                    // observable effect — the inline-TLAB `new` already zeroed
                    // the fields and the only call is the no-op `Object.<init>`.
                    // For such a site we emit the call site AS `Object.<init>`
                    // so the existing codegen elision (the fix-2 `0xb7` arm)
                    // drops the per-object `jit_invoke_dispatch` entirely. This
                    // is the dominant mutator cost on allocation-heavy code
                    // (object `binarytrees`: `TreeNode.<init>` was dispatched
                    // ~135K times at depth 10). Sound in BOTH paths: even if the
                    // elision did not fire, dispatching `Object.<init>` on the
                    // freshly-allocated object is the same no-op as `C.<init>`.
                    // Opt out with `CRATONVM_NO_CTOR_DIRECT_CALL`.
                    let ctor_direct_call_off =
                        crate::runtime::env_cache::ctor_direct_call_disabled();
                    // Trivial `invokespecial …<init>()V` sites, deferred so their
                    // target class can be resolved via `load_class_concurrent`
                    // AFTER `cm_lock` is dropped — `find_class_by_name` does not
                    // see app-loaded classes in this context (the `new`-site path
                    // resolves classes the same way for the same reason).
                    let mut pending_ctor_sites: Vec<(usize, String, usize)> = Vec::new();
                    if !scan.invoke_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        let class = cm_lock.get_class(class_id)?;
                        for &(pc, cp_idx, opcode) in &scan.invoke_ops {
                            // Resolve method reference from constant pool
                            let (ref_class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
                                Some(ConstantPoolEntry::MethodReference {
                                    class_index,
                                    name_and_type_index,
                                    ..
                                }) => (*class_index, *name_and_type_index),
                                Some(ConstantPoolEntry::InterfaceMethodReference {
                                    class_index,
                                    name_and_type_index,
                                    ..
                                }) => (*class_index, *name_and_type_index),
                                _ => continue,
                            };
                            let target_class =
                                match class.constant_pool.get_class_name(ref_class_idx) {
                                    Some(n) => n,
                                    None => continue,
                                };
                            let (method_name_ref, descriptor_ref) =
                                match class.constant_pool.get_name_and_type(nat_idx) {
                                    Some(pair) => pair,
                                    None => continue,
                                };
                            // Count JIT arg slots (receiver + params for virtual, just params for static)
                            let param_count = crate::jit::count_param_slots(descriptor_ref);
                            let mut invoke_kind = match opcode {
                                0xb6 => 0u8, // invokevirtual
                                0xb7 => 1,   // invokespecial
                                0xb9 => 2,   // invokeinterface
                                0xb8 => 3,   // invokestatic
                                _ => continue,
                            };
                            // JVMS 5.4.6 — an `invokevirtual` naming a PRIVATE
                            // method is not a dispatch site. This door reaches
                            // the backend WITHOUT going through
                            // `cratonvm_jit::try_compile`, so it does not
                            // inherit the reclassification made there and has to
                            // make the same one itself. See
                            // `invoke::invokevirtual_site_targets_private`; it is
                            // this door, reached only under
                            // `CRATONVM_BG_COMPILE=0`, that kept
                            // `TCPSSLOptions.init()` from ever running.
                            if invoke_kind == 0
                                && crate::runtime::interpreter::invoke::invokevirtual_site_targets_private(
                                    &cm_lock,
                                    class_id,
                                    target_class,
                                    method_name_ref,
                                    descriptor_ref,
                                )
                            {
                                invoke_kind = 1;
                                cratonvm_jit::PRIVATE_INVOKEVIRTUAL_PINNED
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            let invoke_kind = invoke_kind;
                            let is_recursive_call = target_class == &*class_name_str
                                && method_name_ref == method_name
                                && descriptor_ref == method_descriptor;
                            let use_raw_tail_self_call = invoke_kind == 3
                                && is_recursive_call
                                && crate::jit::invokestatic_self_call_uses_tail_jump(
                                    &padded, code_len, pc,
                                );
                            if use_raw_tail_self_call {
                                continue;
                            }
                            // BUG-1 companion (eager first-call compile parity
                            // with `jit::try_compile`'s routing): NON-tail
                            // static self-recursive sites stay on the raw
                            // direct-CALL path — the backend emits the cheap
                            // `self_call_stack_guard` call (always wired via
                            // `build_helpers`) instead of the full
                            // `jit_invoke_dispatch` round trip. `scan.needs_heap`
                            // is already true (every invoke op sets it), so the
                            // guard's vm_ptr frame slot exists.
                            if invoke_kind == 3 && is_recursive_call {
                                continue;
                            }
                            // Math.sqrt intrinsic: inline as SQRTSD (no dispatch overhead)
                            if invoke_kind == 3
                                && target_class == "java/lang/Math"
                                && method_name_ref == "sqrt"
                                && descriptor_ref == "(D)D"
                            {
                                direct_calls_early.push((
                                    pc,
                                    crate::jit::JitDirectCall {
                                        entry: crate::jit::MATH_SQRT_INTRINSIC,
                                        needs_context: false,
                                        num_params: 1,
                                        return_type: b'D',
                                        guard_class_id: 0,
                                    },
                                ));
                                continue;
                            }
                            // `Integer.valueOf(I)` thin direct call — statically
                            // bound NATIVE callee, so the eager callee compile can
                            // never succeed and the generic dispatch round trip is
                            // pure fixed overhead on the hottest autoboxing path.
                            // Parity with the recognition in `jit::try_compile`;
                            // see `jit::helpers::jit_integer_value_of_direct`.
                            if invoke_kind == 3
                                && target_class == "java/lang/Integer"
                                && method_name_ref == "valueOf"
                                && descriptor_ref == "(I)Ljava/lang/Integer;"
                            {
                                // VM crate: take the helper's address directly —
                                // no registration-order dependency (the atomic in
                                // the jit crate is only for `jit::try_compile`,
                                // which cannot name VM symbols).
                                let entry = crate::jit::helpers::jit_integer_value_of_direct
                                    as *const ()
                                    as usize;
                                direct_calls_early.push((
                                    pc,
                                    crate::jit::JitDirectCall {
                                        entry,
                                        needs_context: true,
                                        num_params: 1,
                                        return_type: b'L',
                                        guard_class_id: 0,
                                    },
                                ));
                                continue;
                            }
                            // `Integer.intValue()` thin direct call — `Integer`
                            // is `final`, so a site declared against it is
                            // statically monomorphic (guard-free); the helper
                            // handles the null-receiver NPE itself.
                            if invoke_kind == 0
                                && target_class == "java/lang/Integer"
                                && method_name_ref == "intValue"
                                && descriptor_ref == "()I"
                            {
                                let entry = crate::jit::helpers::jit_integer_int_value_direct
                                    as *const ()
                                    as usize;
                                direct_calls_early.push((
                                    pc,
                                    crate::jit::JitDirectCall {
                                        entry,
                                        needs_context: true,
                                        num_params: 0,
                                        return_type: b'I',
                                        guard_class_id: 0,
                                    },
                                ));
                                continue;
                            }
                            // Defer trivial `<init>()V` sites for after-lock
                            // elidability resolution (see the block + pending
                            // list comments above).
                            if invoke_kind == 1
                                && method_name_ref == "<init>"
                                && descriptor_ref == "()V"
                                && !ctor_direct_call_off
                            {
                                pending_ctor_sites.push((
                                    pc,
                                    target_class.to_string(),
                                    param_count,
                                ));
                                continue;
                            }
                            let num_jit_args = if invoke_kind == 3 {
                                param_count
                            } else {
                                param_count + 1
                            }; // +1 for receiver
                            let return_type = crate::jit::return_type(descriptor_ref);
                            // Store strings in owned vec; take raw pointers for JIT metadata
                            let class_box: Box<str> = target_class.to_string().into_boxed_str();
                            let method_box: Box<str> = method_name_ref.to_string().into_boxed_str();
                            let desc_box: Box<str> = descriptor_ref.to_string().into_boxed_str();
                            let class_ref = &*class_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                            let method_ref = &*method_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                            let desc_ref = &*desc_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                            owned_jit_strings.push(class_box);
                            owned_jit_strings.push(method_box);
                            owned_jit_strings.push(desc_box);
                            // SAFETY: class_ref, method_ref, desc_ref point into the boxed strs that were just pushed to owned_jit_strings, which outlives the JitInvokeInfo.
                            let info = Box::new(crate::jit::JitInvokeInfo {
                                class_name: unsafe { &*class_ref },
                                method_name: unsafe { &*method_ref },
                                descriptor: unsafe { &*desc_ref },
                                num_jit_args,
                                return_type,
                                invoke_kind,
                                declaring_class_id: class_id.as_u32(),
                            });
                            let info_ptr: *const _ = &*info;
                            owned_jit_invoke_infos.push(info);
                            invoke_info.push((pc, info_ptr));
                            allocate_dynamic_dispatch_slots(
                                invoke_kind,
                                pc,
                                &mut mic_slots_early,
                                &mut owned_mic_slots_early,
                                &mut pic_slots_early,
                                &mut owned_pic_slots_early,
                            );
                        }
                    }
                    // Resolve the deferred trivial-ctor sites now that `cm_lock`
                    // is released. For each `invokespecial C.<init>()V`, resolve
                    // `C` (already loaded — `load_class_concurrent` returns the
                    // cached id without side effects) and check
                    // `is_elidable_construction`. If elidable, emit the site AS
                    // `java/lang/Object.<init>` so the codegen's fix-2 elision
                    // drops the per-object dispatch; otherwise emit the real
                    // dispatch `JitInvokeInfo`. (Soundness: an elidable `C.<init>`
                    // writes no field and only calls the no-op `Object.<init>`,
                    // so eliding it — or dispatching `Object.<init>` on the
                    // freshly zeroed object if the elision somehow did not fire —
                    // is identical to running `C.<init>`.)
                    let dbg_ctor = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CTOR_FIX").is_some();
                    // Pcs whose `<init>()V` target `is_elidable_construction` PROVED empty. The
                    // backend may elide only these; a no-arg constructor that is NOT proven empty
                    // keeps both its allocation and its call, because eliding it would drop
                    // whatever the body writes to global state (see
                    // docs/known-issues/netty/jit-elided-constructor-side-effects-20260812.md).
                    let mut elidable_init_pcs: std::collections::HashSet<usize> =
                        std::collections::HashSet::new();
                    for (pc, tclass, pcount) in pending_ctor_sites {
                        let elidable = shared
                            .load_class_concurrent(&tclass)
                            .ok()
                            .map(|tid| {
                                let cm2 = shared.classes.class_manager.read();
                                is_elidable_construction(shared, &cm2, tid)
                            })
                            .unwrap_or(false);
                        if elidable {
                            elidable_init_pcs.insert(pc);
                        }
                        if dbg_ctor {
                            eprintln!(
                                "[ctor-fix] {}.{}{} ctor site pc={} target={} elidable={}",
                                &*class_name_str,
                                method_name,
                                method_descriptor,
                                pc,
                                tclass,
                                elidable,
                            );
                        }
                        let info_class: &str = if elidable {
                            "java/lang/Object"
                        } else {
                            &tclass
                        };
                        let class_box: Box<str> = info_class.to_string().into_boxed_str();
                        let method_box: Box<str> = "<init>".to_string().into_boxed_str();
                        let desc_box: Box<str> = "()V".to_string().into_boxed_str();
                        let class_ref = &*class_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                        let method_ref = &*method_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                        let desc_ref = &*desc_box as *const str; // Cast: string slice to raw pointer for JIT lifetime
                        owned_jit_strings.push(class_box);
                        owned_jit_strings.push(method_box);
                        owned_jit_strings.push(desc_box);
                        // SAFETY: refs point into the boxed strs just pushed to owned_jit_strings,
                        // which outlives the JitInvokeInfo (same lifetime contract as the loop above).
                        let info = Box::new(crate::jit::JitInvokeInfo {
                            class_name: unsafe { &*class_ref },
                            method_name: unsafe { &*method_ref },
                            descriptor: unsafe { &*desc_ref },
                            num_jit_args: pcount + 1, // receiver + params
                            return_type: b'V',
                            invoke_kind: 1,
                            declaring_class_id: class_id.as_u32(),
                        });
                        let info_ptr: *const _ = &*info;
                        owned_jit_invoke_infos.push(info);
                        invoke_info.push((pc, info_ptr));
                    }
                    // Resolve new/anewarray info (Phase 39: correct ClassId + field count for JIT new)
                    // Only resolve for non-synthetic classes (real JDK bytecode) to avoid
                    // expensive class loading cascades during JIT of synthetic code.
                    // Tuple: (pc, class_id, num_fields,
                    // has_nonzero_tag_primitive_init, has_finalizer).
                    // The last two are conservative true/true here so the JIT goes through
                    // the post-init helper — matches pre-CRIT-2 behavior. A follow-up should
                    // extract the real flags from class metadata to enable the skip path.
                    let mut new_info: Vec<(usize, u32, usize, bool, bool)> = Vec::new();
                    let mut anewarray_info: Vec<(usize, u32)> = Vec::new();
                    // Cold-`new` fix - sites this path cannot resolve at compile
                    // time. Previously baked as the nonsense sentinel entry
                    // `(pc, class_id 0, 0 fields, true, true)`, which did NOT
                    // "make the JIT skip this site and defer to the interpreter"
                    // as the comment below claimed: the codegen found an entry at
                    // that pc and compiled an allocation against class id 0. They
                    // now compile to the CP-indexed helper, which performs the
                    // real loader-faithful resolution, the JVMS 5.4.4 access
                    // check and `<clinit>` at the actual program point - the same
                    // work `Instruction::New` does.
                    let mut new_deferred_info: Vec<(usize, u32, u16)> = Vec::new();
                    let mut anewarray_deferred_info: Vec<(usize, u32, u16)> = Vec::new();
                    let is_real_class = shared
                        .classes.class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| !c.origin.is_compatibility_stub())
                        .unwrap_or(false);
                    if is_real_class && (!scan.new_ops.is_empty() || !scan.anewarray_ops.is_empty())
                    {
                        // Collect class names from constant pool (read lock)
                        let new_class_names: Vec<(usize, u16, Option<String>)> = {
                            let cm_lock = shared.classes.class_manager.read();
                            if let Some(class) = cm_lock.get_class(class_id) {
                                scan.new_ops
                                    .iter()
                                    .map(|&(pc_new, cp_idx)| {
                                        (
                                            pc_new,
                                            cp_idx,
                                            class
                                                .constant_pool
                                                .get_class_name(cp_idx)
                                                .map(|s| s.to_string()),
                                        )
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        };
                        let arr_class_names: Vec<(usize, u16, Option<String>)> = {
                            let cm_lock = shared.classes.class_manager.read();
                            if let Some(class) = cm_lock.get_class(class_id) {
                                scan.anewarray_ops
                                    .iter()
                                    .map(|&(pc_arr, cp_idx)| {
                                        (
                                            pc_arr,
                                            cp_idx,
                                            class
                                                .constant_pool
                                                .get_class_name(cp_idx)
                                                .map(|s| s.to_string()),
                                        )
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        };
                        // Resolve class names to ClassIds (write lock for loading)
                        for (pc_new, cp_idx_new, name_opt) in new_class_names {
                            if let Some(name) = name_opt {
                                let load_result = shared.load_class_concurrent(&name);
                                if let Ok(target_id) = load_result {
                                    // JVMS 5.4.4 / 6.5 `new`: don't bake an
                                    // inlined fast-path allocation for a `new`
                                    // site the accessor is not permitted to
                                    // reach (see the matching check added to
                                    // `Instruction::New` above). Falling back
                                    // to the sentinel entry -- exactly what
                                    // the load-failure arm below already does
                                    // -- makes the JIT skip this site and
                                    // defer to the interpreter, which performs
                                    // the real access check and throws
                                    // `IllegalAccessError`. Without this, a
                                    // method that gets tiered up to JIT before
                                    // its first *interpreted* execution could
                                    // silently bypass access control.
                                    let accessible = {
                                        let cm_lock = shared.classes.class_manager.read();
                                        match (cm_lock.get_class(class_id), cm_lock.get_class(target_id)) {
                                            (Some(accessor), Some(target)) => {
                                                crate::classloading::access_control::check_class_access(accessor, target).is_ok()
                                            }
                                            // Defensive: if either class can't be looked up here,
                                            // don't invent a denial -- let the interpreter's own
                                            // check (which always has both classes) be authoritative.
                                            _ => true,
                                        }
                                    };
                                    if accessible {
                                        // `(true, true)` unless
                                        // `CRATONVM_JIT_REAL_NEW_SITE_FLAGS`
                                        // is set. The in-tree TODO that stood
                                        // here asking for the real flags is
                                        // answered by
                                        // `jit_bridge::jit_new_site_flags`,
                                        // which also records why turning them
                                        // on by default buys nothing today.
                                        let (num_fields, has_prim_init, has_finalizer) = {
                                            let cm = shared.classes.class_manager.read();
                                            if crate::runtime::env_cache::jit_real_new_site_flags() {
                                                crate::runtime::interpreter::jit_bridge::jit_new_site_flags(
                                                    &cm, target_id,
                                                )
                                            } else {
                                                (
                                                    cm.get_class(target_id)
                                                        .map(|c| c.num_total_fields)
                                                        .unwrap_or(0),
                                                    true,
                                                    true,
                                                )
                                            }
                                        };
                                        new_info.push((
                                            pc_new,
                                            target_id.as_u32(),
                                            num_fields,
                                            has_prim_init,
                                            has_finalizer,
                                        ));
                                    } else {
                                        new_deferred_info
                                            .push((pc_new, class_id.as_u32(), cp_idx_new));
                                    }
                                } else {
                                    new_deferred_info.push((pc_new, class_id.as_u32(), cp_idx_new));
                                }
                            }
                        }
                        for (pc_arr, cp_idx_arr, name_opt) in arr_class_names {
                            if let Some(name) = name_opt {
                                if let Ok(target_id) = shared.load_class_concurrent(&name) {
                                    anewarray_info.push((pc_arr, target_id.as_u32()));
                                } else {
                                    anewarray_deferred_info
                                        .push((pc_arr, class_id.as_u32(), cp_idx_arr));
                                }
                            }
                        }
                    }

                    // invokedynamic-uncommon-trap fix: resolve each `indy_ops`
                    // site's target descriptor to the minimal stack-effect info
                    // the codegen needs (arg slot count + return type tag) — see
                    // the `indy_info` field doc on the x64 `Compiler` struct. A
                    // site that cannot be resolved (class not loaded, malformed
                    // pool) is simply omitted; the x64 codegen's 0xba arm then
                    // bails the whole compile (`return false`) rather than
                    // guessing, exactly like the OSR/hot-path resolvers.
                    let mut indy_info: Vec<(usize, usize, u8, Vec<u8>, usize)> = Vec::new();
                    if !scan.indy_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        if let Some(class) = cm_lock.get_class(class_id) {
                            for &(pc_indy, cp_idx) in &scan.indy_ops {
                                if let Some(ConstantPoolEntry::InvokeDynamic {
                                    name_and_type_index,
                                    ..
                                }) = class.constant_pool.get(cp_idx)
                                {
                                    if let Some((_, descriptor)) =
                                        class.constant_pool.get_name_and_type(*name_and_type_index)
                                    {
                                        let arg_slots = crate::jit::count_param_slots(descriptor);
                                        let ret_type = crate::jit::return_type(descriptor);
                                        let arg_type_tags =
                                            crate::jit::indy_arg_type_tags(descriptor);
                                        let bridge_site = crate::runtime::invokedynamic::make_jit_indy_bridge_site_from_parts(
                                            &class.constant_pool,
                                            &class.bootstrap_methods,
                                            cp_idx,
                                            class_id,
                                        )
                                        .unwrap_or(0);
                                        indy_info.push((
                                            pc_indy,
                                            arg_slots,
                                            ret_type,
                                            arg_type_tags,
                                            bridge_site,
                                        ));
                                    }
                                }
                            }
                        }
                    }

                    // Resolve instance field info for getfield/putfield (M5 fix)
                    let mut field_info: Vec<(usize, usize, u8)> = Vec::new();
                    // Compact reference-field layout: per-pc packed offset + ref-ness
                    // so the single-pass codegen emits inline compact field access.
                    let mut compact_field_info: Vec<(usize, u32, bool)> = Vec::new();
                    let compact_fields = cratonvm_types::compact_ref_fields_enabled();
                    if !scan.field_ops.is_empty() {
                        for &(pc_f, cp_idx) in &scan.field_ops {
                            if let Ok(field) = resolve_field_ref(shared, class_id, cp_idx) {
                                let cm_lock = shared.classes.class_manager.read();
                                if let Some(class) = cm_lock.get_class(class_id) {
                                    let nat_idx = match class.constant_pool.get(cp_idx) {
                                        Some(ConstantPoolEntry::FieldReference {
                                            name_and_type_index,
                                            ..
                                        }) => *name_and_type_index,
                                        _ => continue,
                                    };
                                    if let Some((_, descriptor)) =
                                        class.constant_pool.get_name_and_type(nat_idx)
                                    {
                                        let type_tag =
                                            *descriptor.as_bytes().first().unwrap_or(&b'I');
                                        field_info.push((pc_f, field.field_index, type_tag));
                                        if compact_fields {
                                            if let Some((c_off, c_ref)) =
                                                cratonvm_types::compact_field_slot(
                                                    field.declaring_class_id.as_u32(),
                                                    field.field_index,
                                                )
                                            {
                                                compact_field_info.push((
                                                    pc_f,
                                                    c_off as u32,
                                                    c_ref,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Resolve ldc/ldc_w constants (int, float).
                    // Skip early compilation for methods with String/Class ldc —
                    // those will be handled by OSR which can wire callees as direct calls.
                    let mut ldc_info_early: Vec<(usize, i64)> = Vec::new();
                    let mut ldc_string_info_early: Vec<(usize, u32, u16)> = Vec::new();
                    let mut ldc_class_info_early: Vec<(usize, u32, u16)> = Vec::new();
                    // The `ldc`-family pcs whose constant is floating-point.
                    // Codegen types these by their consuming opcode, but the
                    // deopt operand-stack snapshot has no consuming opcode to
                    // ask — see `x64::Compiler::ldc_fp_pcs`.
                    let mut ldc_fp_pcs_early: rustc_hash::FxHashSet<usize> =
                        rustc_hash::FxHashSet::default();
                    let mut has_unsupported_ldc = false;
                    if !scan.ldc_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        if let Some(class) = cm_lock.get_class(class_id) {
                            for &(pc_ldc, cp_idx) in &scan.ldc_ops {
                                match class.constant_pool.get(cp_idx) {
                                    Some(ConstantPoolEntry::Integer(v)) => {
                                        // Widening: i32 -> i64 (sign-extended, JVM i2l)
                                        ldc_info_early.push((pc_ldc, *v as i64));
                                        // Cast: JIT ABI — i64 register convention
                                    }
                                    Some(ConstantPoolEntry::Float(v)) => {
                                        ldc_info_early.push((pc_ldc, v.to_bits() as i64));
                                        // Cast: JIT ABI -- float bits to i64
                                        ldc_fp_pcs_early.insert(pc_ldc);
                                    }
                                    Some(ConstantPoolEntry::StringReference { string_index })
                                        if class
                                            .constant_pool
                                            .get_utf8_wide(*string_index)
                                            .is_none() =>
                                    {
                                        // Wired 2026-07-18, mirroring the OSR-artifact
                                        // path. Before this, ANY method with a string
                                        // constant went into `jit_skip_set` here, which
                                        // also blocked the hot-path `jit::try_compile` -
                                        // OSR artifacts were such methods' ONLY compiled
                                        // form.
                                        //
                                        // The SITE is recorded, not the text: since
                                        // 2026-08-20 codegen materialises through
                                        // `helpers.ldc_string_cp`, which answers from
                                        // the `(class, cp index)`-keyed record JVMS
                                        // §5.4.3 requires. The `get_utf8` call is only
                                        // the representability test the `get_utf8_wide`
                                        // guard above pairs with.
                                        match class.constant_pool.get_utf8(*string_index) {
                                            Some(_) => {
                                                ldc_string_info_early
                                                    .push((pc_ldc, class_id.as_u32(), cp_idx));
                                            }
                                            None => has_unsupported_ldc = true,
                                        }
                                    }
                                    // `ldc <Class>`: recorded as a site and
                                    // served at run time by
                                    // `helpers.ldc_class_cp`. Recorded even if
                                    // that helper turns out to be unwired — the
                                    // backend then refuses the method, which is
                                    // a bail, whereas `has_unsupported_ldc`
                                    // additionally inserts it into
                                    // `jit_skip_set` and so would block the
                                    // hot-path compile too.
                                    Some(ConstantPoolEntry::ClassReference { .. }) => {
                                        ldc_class_info_early.push((
                                            pc_ldc,
                                            class_id.as_u32(),
                                            cp_idx,
                                        ));
                                    }
                                    // Wide-string / MethodHandle / condy ldc
                                    // kinds - still unwired on this path (as on
                                    // the OSR path).
                                    _ => {
                                        has_unsupported_ldc = true;
                                    }
                                }
                            }
                        }
                    }
                    // Unsupported ldc kinds (wide-string/Class/...) cannot be
                    // early-compiled; OSR handles such methods later.
                    if has_unsupported_ldc {
                        // Mark as skipped so we don't retry
                        note_jit_skip_seal("early-unsupported-ldc", &skip_key);
                        shared.jit.jit_skip_set.write().insert(skip_key.clone());
                    }

                    // Resolve ldc2_w constants (long, double)
                    let mut ldc2w_info_early: Vec<(usize, i64)> = Vec::new();
                    if !scan.ldc2w_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        if let Some(class) = cm_lock.get_class(class_id) {
                            for &(pc_ldc, cp_idx) in &scan.ldc2w_ops {
                                let val = match class.constant_pool.get(cp_idx) {
                                    Some(ConstantPoolEntry::Long(v)) => *v,
                                    Some(ConstantPoolEntry::Double(v)) => {
                                        ldc_fp_pcs_early.insert(pc_ldc);
                                        v.to_bits() as i64 // Cast: JIT ABI -- float bits to i64
                                    }
                                    _ => continue,
                                };
                                ldc2w_info_early.push((pc_ldc, val));
                            }
                        }
                    }

                    // Unsupported ldc kind present — fall through to the interpreter.
                    if has_unsupported_ldc {
                        return None;
                    }

                    // Category-2 (long/double) PARAMETER guard.
                    //
                    // This early-compile path passes `param_slots = args.len()` to the
                    // bare `x64::compile`, which lays parameters out sequentially by
                    // argument index (one JVM slot each). That is wrong whenever a
                    // parameter is a long/double: those occupy TWO JVM local slots, so
                    // every parameter after the first category-2 one is read from the
                    // wrong (un-populated) slot. Concrete symptom: a `(long,long)long`
                    // lambda body (`lload_0; lload_2; ladd`) read its 2nd argument from
                    // the empty `locals[2]`, so e.g. `Long::sum(100,23)` returned 100.
                    //
                    // The hot-path `jit::try_compile` handles this correctly via
                    // `compute_param_jvm_slots` + `compile_with_param_slots`. Bail here
                    // (fall through to the interpreter); the method still JIT-compiles
                    // later through the correct path once it goes hot. We do NOT add it
                    // to `jit_skip_set` so that path remains available.
                    if crate::jit::count_param_slots_jvm_spec(method_descriptor)
                        != crate::jit::count_param_slots(method_descriptor)
                    {
                        return None;
                    }

                    // Try to compile
                    let param_slots = args.len();
                    let helpers = crate::jit::helpers::build_helpers_for(shared);
                    // HIB-CV-20 — like the OSR path (and unlike the legacy
                    // `x64::compile` wrapper, which hardcodes `param_oop_mask = 0`),
                    // seed the local-oop dataflow with this method's reference
                    // PARAMETERS so an oop param living in a callee-saved register
                    // across an early safepoint is reloaded after a moving GC
                    // (`emit_post_safepoint_reload`). Without it the register stays
                    // stale → heap corruption. The category-2 slot-layout guard
                    // above guarantees "arg index == slot", so the descriptor-derived
                    // mask lines up with the `&[]`/`0` legacy layout. Gate on the
                    // precise-maps flag so the gate-off path stays byte-identical.
                    let early_is_static = shared
                        .classes.class_manager
                        .read()
                        .get_class(class_id)
                        .and_then(|class| {
                            class
                                .methods
                                .iter()
                                .find(|m| {
                                    &*m.name == method_name && &*m.descriptor == method_descriptor
                                })
                                .map(|m| m.is_static())
                        })
                        .unwrap_or(false);
                    // Also seed under `deopt_real_enabled()` — see the matching
                    // comment at the hot-path `jit::try_compile` seed site
                    // (jit/src/lib.rs) for why an unseeded mask makes every
                    // deopt at this compile silently double-execute side
                    // effects via the whole-method re-run fallback.
                    let param_oop_mask = if crate::jit::x64::precise_jit_maps_enabled()
                        || crate::jit::x64::moving_young_enabled()
                        || cratonvm_jit::deopt_real_enabled()
                    {
                        crate::jit::compute_param_oop_mask(method_descriptor, early_is_static)
                    } else {
                        0
                    };
                    // -- The String-intrinsic pin, ASKED at this door -- D1, 2026-09-01
                    //
                    // The third door. The measurement that found the pin
                    // installed at ONE of three, and why the answer is inert at
                    // the two that reach the single-pass backend directly, is
                    // written out at the OSR door (`jit_bridge.rs`, at
                    // `osr_string_pin_declines`); the topology is in
                    // `compile_gate`'s "installed at ONE door" section.
                    //
                    // This door passes `string_layout: None` below -- "String
                    // intrinsics land in a later wave" -- so the pin's honest
                    // verdict here is `BlindNoLayout`: a site declared on a
                    // String-family receiver, no layout resolved at this door,
                    // and the rule FAILS OPEN (`false`, do not pin). That is
                    // not a shortcut. Without a layout the single-pass backend
                    // emits no intrinsic either, so pinning would cost the
                    // method a C2 body and buy nothing back. The `None` is
                    // passed deliberately rather than papered over: the whole
                    // gain is that `blind-no-layout` becomes a MEASURED number
                    // for this door instead of an invisible absence.
                    //
                    // Consequence, so nobody reads more into the counter than
                    // it carries: with `layout == None` the pin can only answer
                    // `false` here. `NoSite` and `BlindNoLayout` are `false` by
                    // rule, and the `BlindNoResolver` fail-closed arm requires
                    // `layout.is_some()`. So this ask changes no machine code
                    // today; it is a census entry, not a decision. It becomes a
                    // real decision the moment the `string_layout: None`
                    // argument below becomes a resolved layout -- the `if` is
                    // the tripwire for exactly that.
                    //
                    // A resolver IS supplied even though the layout is not,
                    // because without one the verdict would be
                    // `BlindNoResolver` -- the STRONGER blindness, which says
                    // nothing about whether a layout would have mattered -- and
                    // the narrower fact is the whole reason to ask here.
                    //
                    // COST: once per eager first-call compile, off any loop.
                    // The resolver takes the `class_manager` read lock per site,
                    // matching `c_invoke_resolver` in `jit_bridge.rs` and the
                    // field-op loop above, which already takes it per field op;
                    // `string_intrinsic_pin_verdict` asks nothing at all for a
                    // method with no `invokevirtual`/`invokeinterface` site and
                    // stops at the first String-family receiver otherwise. No
                    // lock is held here -- the `early_is_static` probe above
                    // took and released its own.
                    let eager_pin_invoke_resolver =
                        |cp_idx: u16| -> Option<(String, String, String)> {
                            let cm = shared.classes.class_manager.read();
                            let class = cm.get_class(class_id)?;
                            let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
                                Some(ConstantPoolEntry::MethodReference {
                                    class_index,
                                    name_and_type_index,
                                    ..
                                }) => (*class_index, *name_and_type_index),
                                Some(ConstantPoolEntry::InterfaceMethodReference {
                                    class_index,
                                    name_and_type_index,
                                    ..
                                }) => (*class_index, *name_and_type_index),
                                _ => return None,
                            };
                            let target_class = class.constant_pool.get_class_name(class_idx)?;
                            let (mn, desc) = class.constant_pool.get_name_and_type(nat_idx)?;
                            Some((target_class.to_string(), mn.to_string(), desc.to_string()))
                        };
                    let eager_string_pin_declines = admission.string_intrinsic_pin_declines(
                        &scan.invoke_ops,
                        Some(&eager_pin_invoke_resolver),
                        // The SAME value handed to `compile_with_param_slots`
                        // below as its `string_layout` argument. Asking about a
                        // layout this compile does not use would make the
                        // census describe a compile that did not happen.
                        None,
                    );
                    if eager_string_pin_declines && crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] eager-first-call String-intrinsic pin declines the \
                             optimizing tier for {class_name_arc}.{method_name_arc}{descriptor_arc} \
                             -- unreachable while this door passes `string_layout: None`. If this \
                             ever fires, the later wave landed and this ask is now a decision.",
                        );
                    }
                    let mut cm = crate::jit::x64::compile_with_param_slots(
                        &admission,
                        &padded,
                        code_len,
                        param_slots,
                        code_attr.max_locals as usize, // Widening: u16 to usize
                        // A BRIDGED indy site calls a helper, which loads the
                        // hidden `SharedVm` pointer from the frame slot
                        // `heap_local_offset` names — and that slot only EXISTS
                        // when `needs_heap` is set. `scan.needs_heap` does not
                        // know about it, because an `invokedynamic` used to
                        // lower to a trap that calls nothing. See the same
                        // one-line remedy at the other two compile doors.
                        scan.needs_heap
                            || indy_info
                                .iter()
                                .any(|&(_, _, _, _, bridge_site)| bridge_site != 0),
                        mna_info,
                        field_info,
                        typecheck_info,
                        static_field_info,
                        new_info,
                        new_deferred_info,
                        anewarray_info,
                        anewarray_deferred_info,
                        invoke_info,
                        direct_calls_early,
                        mic_slots_early,
                        pic_slots_early,
                        ldc_info_early,
                        ldc_string_info_early, // wired 2026-07-18 — see the
                        // StringReference arm in the ldc resolver above.
                        ldc_class_info_early,
                        ldc2w_info_early,
                        ldc_fp_pcs_early,
                        std::collections::HashMap::new(), // branch_hints
                        std::collections::HashMap::new(), // loop_unroll_hints
                        &helpers,
                        scan.non_escaping_new.clone(), // escape analysis results
                        std::collections::HashMap::new(), // inline_sites
                        std::collections::HashMap::new(), // inline_guard_variants (PGO-02, no guarded plan from this scan-based fast path)
                        None, // string_layout — String intrinsics land in a later wave
                        &[],  // param_jvm_slots — legacy "arg index == slot" layout
                        0,    // param_slot_span — legacy layout
                        param_oop_mask,
                        compact_field_info,
                        // method_key — bakes this method's identity into its
                        // deopt snapshots (resume sinks verify it before
                        // resuming a stashed frame). Also enables the per-bci
                        // de-spec consult (inert in production).
                        &format!("{class_name_arc}.{method_name_arc}:{descriptor_arc}"),
                        indy_info,
                        Some(elidable_init_pcs),
                    )?;
                    // Attach owned metadata to compiled method
                    cm._jit_strings = owned_jit_strings;
                    cm._jit_invoke_infos = owned_jit_invoke_infos;
                    cm._jit_mic_slots.extend(owned_mic_slots_early);
                    cm._jit_pic_slots.extend(owned_pic_slots_early);
                    stamp_compilation_epoch(
                        shared,
                        &class_name_arc,
                        &method_name_arc,
                        &descriptor_arc,
                        &mut cm,
                    );
                    let code_size = 0usize; // TODO: expose compiled code size
                                            // Round-7 HIGH-2 fix: do NOT hold `jit_cache.write()` across
                                            // `flight_recorder.lock()`.  The global JIT cache writer
                                            // would otherwise serialise JFR formatting for every
                                            // sibling compiler thread.  Two-phase: (1) put + get under
                                            // the JIT cache write, drop it; (2) JFR event emitted with
                                            // only the flight_recorder lock held.
                    let cached_result = {
                        let mut jit_cache = shared.jit.jit_cache.write();
                        jit_cache.put(
                            class_name_arc.clone(),
                            method_name_arc.clone(),
                            descriptor_arc.clone(),
                            class_id,
                            cm,
                        );
                        jit_cache.get(&class_name_arc, &method_name_arc, &descriptor_arc, class_id)
                        // `jit_cache` write-lock dropped here at end of scope.
                    };
                    // This first-call path compiles thousands of methods per run
                    // (everything reached via `invoke_method_shared`, incl. the
                    // reflective-invoke chain), and was the only compile site with
                    // NO success logging — which made the Bug-4 testAdHocData JIT
                    // execution invisible to CRATONVM_DBG_JITC-based bisection.
                    if let Some(c) = &cached_result {
                        if crate::runtime::env_cache::dbg_jitc() {
                            eprintln!(
                                "[cratonvm-jitc] first-compile {}.{}{} entry={:p} len={}",
                                class_name_arc,
                                method_name_arc,
                                descriptor_arc,
                                c.entry_ptr(),
                                c.code_bytes().len()
                            );
                        }
                        crate::jit::disasm::maybe_dump_annotated(
                            "first",
                            &class_name_arc,
                            &method_name_arc,
                            &descriptor_arc,
                            c.entry_ptr(),
                            c.code_bytes(),
                            c.osr_pc_to_native.as_deref(),
                            c.osr_local_assignments.as_deref(),
                        );
                    }
                    // Record JFR compilation event — flight_recorder lock taken
                    // *after* the JIT cache write has been released.
                    //
                    // Round-9 cross-cutting LOW-11: the previous implementation
                    // called `format!("{}.{}:{}", ...)` here which allocated a
                    // fresh `String` on every JIT compile. A thread-local scratch
                    // buffer lets us `write!` the formatted key into a reused
                    // allocation, then borrow it as `&str` for the emit call.
                    // The buffer is cleared (not freed) on each entry so capacity
                    // carries across compiles.
                    {
                        let now_ns = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64; // Cast: duration to u64 nanoseconds
                        thread_local! {
                            static METHOD_KEY_SCRATCH: std::cell::RefCell<String> =
                                std::cell::RefCell::new(String::with_capacity(256));
                        }
                        METHOD_KEY_SCRATCH.with(|cell| {
                            use std::fmt::Write as _;
                            let mut buf = cell.borrow_mut();
                            buf.clear();
                            // `write!` into a `String` is infallible; discard the
                            // formatter `Result` rather than `unwrap`-ing it so we
                            // stay clean under the file-level
                            // `clippy::unwrap_used = deny`.
                            let _ = write!(
                                &mut *buf,
                                "{}.{}:{}",
                                class_name_arc, method_name_arc, descriptor_arc,
                            );
                            let mut jfr = shared.debug.flight_recorder.lock();
                            // Round-9 HIGH-5: convert the scratch buffer to an
                            // `Arc<str>` once and pass it to the `_arc` variant so
                            // the emit path doesn't re-do `Arc::from(&str)`.
                            let method_arc: Arc<str> = Arc::from(buf.as_str());
                            cratonvm_jfr::builtin::emit_compilation_event_arc(
                                &mut jfr,
                                method_arc,
                                1,     // compile_id
                                2,     // tier (C2-equivalent)
                                true,  // success
                                false, // not OSR
                                // Truncation-checked: code_size to i32; JVM method code limited to 64K
                                i32::try_from(code_size).unwrap_or(i32::MAX),
                                0, // code_size, inlined_bytes
                                now_ns,
                                0, // start_time, duration (not tracked)
                            );
                        });
                    }
                    cached_result
                });

                if compiled.is_none() && !c2_not_hot {
                    // RBC.4 — seal first-call compile failures for the same
                    // reason as the scan-reject seal above: without it every
                    // uncached invocation of a backend-bailing method re-ran
                    // resolution + x64 codegen here. (A method sealed by a
                    // transient resolver miss can still be compiled later by
                    // the invocation-counter upgrade path; the cache fast-path
                    // then routes calls to it.)
                    //
                    // `!c2_not_hot`: under CRATONVM_JIT_C2_FIRST_CALL a not-yet-hot
                    // method returned None on purpose (to interpret until its
                    // invocation counter crosses the threshold) — sealing it here
                    // would set `already_skipped` and permanently stop that counter.
                    note_jit_skip_seal("early-backend-bail", &skip_key);
                    shared.jit.jit_skip_set.write().insert(skip_key.clone());
                }
                // `RetainedCode`: this handle is regularly the LAST owner of
                // a body that has since been superseded, and it is released on
                // the mutator, so a bare `Arc` drop here unmaps executable
                // memory with no quiescence proof behind it. See
                // `cratonvm_jit::RetainedCode`.
                if let Some(compiled) = compiled.map(cratonvm_jit::RetainedCode::new) {
                    if crate::runtime::env_cache::jit_entry_dbg() {
                        eprintln!(
                            "[JIT_ENTRY] {}.{}{}",
                            class_name_str, method_name, method_descriptor
                        );
                    }
                    // Ensure all classes referenced by static field ops are initialized.
                    // The JIT directly accesses static field memory, bypassing the
                    // interpreter's ensure_class_initialized_shared call.
                    //
                    // RBC.5 — the class-id list now comes from the artifact
                    // (`static_init_classes`, recorded by `x64::compile` from the
                    // already-resolved `static_field_info`), and the ensure-walk
                    // runs ONCE per artifact (`static_inits_done`) instead of
                    // re-resolving every constant-pool ref + re-taking the init
                    // locks on every single call. A failed init does NOT set the
                    // done-flag, so it is retried on the next call exactly like
                    // the per-call code it replaces.
                    if !compiled.static_init_classes.is_empty()
                        && !compiled
                            .static_inits_done
                            .load(std::sync::atomic::Ordering::Acquire)
                    {
                        let mut all_ok = true;
                        for &cid_raw in &compiled.static_init_classes {
                            match ensure_class_initialized_shared(
                                shared,
                                thread,
                                ClassId::new(cid_raw),
                            ) {
                                Ok(()) => {}
                                Err(MethodCallFailed::ExceptionThrown(exc)) => {
                                    // Class init failed (e.g. ExceptionInInitializerError).
                                    // Don't propagate directly — fall through to interpreter
                                    // so the exception table can catch it.
                                    jit_early_exception = Some(exc);
                                    all_ok = false;
                                    break;
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        if all_ok {
                            compiled
                                .static_inits_done
                                .store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                    // Skip JIT execution if class init already produced an exception
                    if jit_early_exception.is_some() {
                        // Fall through to interpreter to handle via exception table
                    } else {
                        // Convert Value args to i64 for JIT calling convention
                        let ret_type = crate::jit::return_type(method_descriptor);
                        // Windows x64 ABI: 4 register args; SysV: 6 register args
                        const MAX_JIT_ARGS: usize = if cfg!(target_os = "windows") { 4 } else { 6 };
                        let mut jit_args = [0i64; 6];
                        let mut jit_count = 0;
                        for arg in args {
                            // Bail out before writing past the end of `jit_args`. The
                            // post-loop guard further narrows to MAX_JIT_ARGS, but
                            // that check fires too late on its own: a Java method with
                            // 7+ args (common in libraries like Lucene) would panic
                            // here with "index out of bounds: the len is 6 but the
                            // index is 6" on `jit_args[6]` before the post-loop guard
                            // ever runs. Bail with the existing usize::MAX sentinel
                            // so the interpreter handles the call instead.
                            if jit_count >= jit_args.len() {
                                jit_count = usize::MAX;
                                break;
                            }
                            match arg {
                                Value::Int(v) => {
                                    jit_args[jit_count] = *v as i64; // Cast: JIT ABI -- i64 register convention
                                    jit_count += 1;
                                }
                                Value::Long(v) => {
                                    jit_args[jit_count] = *v;
                                    jit_count += 1;
                                }
                                Value::Object(Some(obj)) => {
                                    jit_args[jit_count] = obj.as_ptr() as i64; // Cast: JIT ABI -- pointer to i64 register
                                    jit_count += 1;
                                }
                                Value::Object(None) => {
                                    jit_args[jit_count] = 0;
                                    jit_count += 1;
                                }
                                Value::Float(v) => {
                                    jit_args[jit_count] = v.to_bits() as i64; // Cast: JIT ABI -- float bits to i64
                                    jit_count += 1;
                                }
                                Value::Double(v) => {
                                    jit_args[jit_count] = v.to_bits() as i64; // Cast: JIT ABI -- float bits to i64
                                    jit_count += 1;
                                }
                                _ => {
                                    // Unsupported arg type: can't use JIT
                                    jit_count = usize::MAX;
                                    break;
                                }
                            }
                        }
                        if jit_count != usize::MAX && jit_count <= MAX_JIT_ARGS {
                            // Set JIT thread context for invoke dispatch callbacks.
                            // Save the old pointer so re-entrant JIT calls don't lose it.
                            let saved_jit_thread = crate::jit::helpers::set_jit_thread(thread);
                            // NEW-1.5 + T1.1.a: record the native stack pointer so the
                            // GC root scanner can walk our JIT spill region. When the
                            // compiled method carries precise oop maps
                            // (`has_precise_oop_maps() == true`), the walker
                            // enumerates exact oop slots and backstops with a
                            // conservative sweep. Otherwise the walker falls back to
                            // the pure conservative scan. The guard pops on drop so
                            // it's panic-safe.
                            let jit_result = {
                                let _jit_root_guard = crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled(
                                    &compiled,
                                );
                                let compiled_ref = &compiled;
                                let args_slice = &jit_args[..jit_count];
                                let needs_heap = compiled_ref.needs_heap();
                                let vm_ptr = shared as *const _ as i64; // Cast: JIT ABI -- pointer to i64 register
                                                                        // task #44: migrated from `call`/`call_with_context` (panicking shims)
                                                                        // to `try_call`/`try_call_with_context`. A JIT runtime invocation
                                                                        // failure (invalid code pointer / too-many-args) surfaces as
                                                                        // `Err(CompileError)` instead of being silently downgraded to 0.
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    if needs_heap {
                                        // SAFETY: compiled is a finalized JIT CompiledMethod whose entry point was validated; args match the method's JVM descriptor.
                                        unsafe {
                                            compiled_ref.try_call_with_context(vm_ptr, args_slice)
                                        }
                                    } else {
                                        // SAFETY: compiled is a finalized JIT CompiledMethod whose entry point was validated; args match the method's JVM descriptor.
                                        unsafe { compiled_ref.try_call(args_slice) }
                                    }
                                }))
                            };
                            // Restore the saved JIT thread pointer (supports re-entrancy)
                            crate::jit::helpers::restore_jit_thread(saved_jit_thread);
                            // Check for pending Java exception from JIT dispatch callbacks.
                            // jit_invoke_dispatch stores exceptions in TLS when a callee throws.
                            // We do NOT return Err here — the current method's exception table
                            // hasn't been consulted yet (no frame pushed). Instead, save the
                            // exception and fall through to the interpreter, which will push a
                            // frame and route through the exception table.
                            if let Some(exc) =
                                crate::jit::helpers::take_jit_pending_exception(thread)
                            {
                                // This legacy sink routes the exception against a
                                // freshly pushed, method-entry frame rather than
                                // through `route_jit_signal_exception`, so a frame
                                // naming THIS method can never be used here — drop it
                                // rather than leave it for a later invocation to
                                // mis-claim. A callee's frame is left alone: its
                                // exception is still in flight.
                                if lambda_dispatch_active() {
                                    let cached = Arc::new(CachedBytecodeMethod {
                                        declaring_class_id: class_id,
                                        class_name: Arc::from(class_name_str.as_str()),
                                        method_name: Arc::from(method_name),
                                        method_descriptor: Arc::from(method_descriptor),
                                        source_file: source_file.as_deref().map(Arc::from),
                                        code: crate::runtime::frame::padded_bytecode(
                                            &code_attr.code,
                                        ),
                                        exception_table: Arc::from(
                                            code_attr.exception_table.as_slice(),
                                        ),
                                        max_stack: code_attr.max_stack,
                                        max_locals: code_attr.max_locals,
                                        num_params: count_method_params(method_descriptor) as u16,
                                        is_synchronized,
                                        is_static,
                                        force_native_cache: std::sync::OnceLock::new(),
                                        descriptor_facts_cache: std::sync::OnceLock::new(),
                                        intercept_shape_cache: std::sync::OnceLock::new(),
                                        interp_invocations: std::sync::atomic::AtomicU32::new(0),
                                        native_callback_cache: std::sync::OnceLock::new(),
                                        invoc_key: std::sync::OnceLock::new(),
                                        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
                                        quickened: std::sync::OnceLock::new(),
                                    });
                                    let throw_pc = jit_local_athrow_pc(
                                        &cached,
                                        crate::jit::helpers::peek_jit_athrow_bci(),
                                    );
                                    crate::jit::helpers::clear_jit_athrow_bci();
                                    if let Ok(result) = run_jit_callee_handler(
                                        shared, thread, &cached, throw_pc, exc, args,
                                    ) {
                                        return result;
                                    }
                                    return Err(MethodCallFailed::ExceptionThrown(exc));
                                }
                                drop_own_exceptional_frame(
                                    &class_name_str,
                                    method_name,
                                    method_descriptor,
                                );
                                jit_early_throw_bci = crate::jit::helpers::peek_jit_athrow_bci();
                                crate::jit::helpers::clear_jit_athrow_bci();
                                jit_early_exception = Some(exc);
                            } else {
                                let result = match jit_result {
                                    Ok(Ok(v)) => v,
                                    Ok(Err(jit_err)) => {
                                        // task #44: surface `try_call*` Err as InternalError
                                        // instead of the old silent return-0 fallback.
                                        return Err(MethodCallFailed::InternalError(
                                            VmError::Internal {
                                                message: format!("JIT call failed: {jit_err}"),
                                            },
                                        ));
                                    }
                                    Err(panic_payload) => {
                                        return Err(jit_panic_to_exception(
                                            shared,
                                            thread,
                                            panic_payload,
                                        ));
                                    }
                                };
                                // Round-8 CRIT fix (NPE leak): drain the pending-NPE flag on
                                // EVERY JIT return path, not only the `i64::MIN` deopt arm.
                                // A void-return store helper (`jit_iastore`/`jit_bastore`/
                                // `jit_aastore`) that hits a null array sets the flag and
                                // returns; without this hoist, the flag would leak to the
                                // next unrelated JIT helper call. The drain runs before any
                                // normal-return path so the NPE surfaces at the right method.
                                //
                                // Round-9/10 HIGH fix: route the NPE through the JIT'd
                                // method's exception table instead of throwing it past
                                // the method boundary. Building a real
                                // java/lang/NullPointerException object and stashing it
                                // into `jit_early_exception` reuses the existing
                                // post-frame-push routing at ~line 2340, which calls
                                // `find_exception_handler_any_pc` against the about-to-be-
                                // pushed frame. Without this, an in-method
                                // `try { ... } catch (NullPointerException npe) { ... }`
                                // surrounding a JIT'd null-array store would silently
                                // bubble the NPE to the caller instead of running the
                                // catch block. The `InternalError` fallback handles the
                                // rt.jar-not-loaded boot path (no NPE class yet).
                                let npe_routed = if crate::jit::helpers::take_jit_pending_npe() {
                                    // Taken BEFORE the construction below, which is
                                    // what re-captures the (now compiled-frame-free)
                                    // stack. See `attach_snapshotted_npe_frames`.
                                    let snapshot =
                                        crate::jit::helpers::take_jit_pending_npe_compiled_frames();
                                    match crate::runtime::exceptions::throw_runtime_error(
                                        shared,
                                        thread,
                                        RuntimeError::NullPointerException { message: None },
                                    ) {
                                        MethodCallFailed::ExceptionThrown(exc) => {
                                            crate::runtime::exceptions::attach_snapshotted_npe_frames(
                                                shared, &thread.frames, exc, snapshot,
                                            );
                                            jit_early_exception = Some(exc);
                                            true
                                        }
                                        other => return Err(other),
                                    }
                                } else {
                                    false
                                };
                                // Round-11 fix (AIOOBE leak): complete the NPE drain for the
                                // pending-AIOOBE flag, drained AFTER the NPE (a frame cannot
                                // have both pending at once, matching JVM semantics). A JIT
                                // void-return store helper that hits an out-of-bounds index
                                // sets `JIT_PENDING_AIOOBE` and returns normally (it cannot
                                // use the i64::MIN sentinel), so without this drain the
                                // ArrayIndexOutOfBoundsException would leak to the next
                                // unrelated JIT helper call. Stash it into
                                // `jit_early_exception` so the post-frame-push handler walker
                                // (~line 2340) can catch it in the JIT'd method, mirroring
                                // the NPE block above. The `create_exception_object` error
                                // arm covers the rt.jar-not-loaded boot path (no AIOOBE class
                                // yet).
                                let aioobe_routed = if let Some((index, length)) =
                                    crate::jit::helpers::take_jit_pending_aioobe()
                                {
                                    let msg =
                                        format!("Index {index} out of bounds for length {length}");
                                    match crate::runtime::exceptions::create_exception_object(
                                        shared,
                                        thread,
                                        "java/lang/ArrayIndexOutOfBoundsException",
                                        Some(&msg),
                                    ) {
                                        Ok(exc) => {
                                            jit_early_exception = Some(exc);
                                            true
                                        }
                                        Err(other) => return Err(other),
                                    }
                                } else {
                                    false
                                };
                                // Divide-by-zero direct-throw drain (sibling of the
                                // NPE/AIOOBE blocks above). The JIT div-by-zero stub sets
                                // `JIT_PENDING_ARITHMETIC` and returns i64::MIN; route the
                                // `ArithmeticException` into `jit_early_exception` so the
                                // post-frame-push handler walker can catch it in the JIT'd
                                // method, instead of re-running from entry (which double-
                                // executes side effects preceding the trap).
                                let arith_routed =
                                    if crate::jit::helpers::take_jit_pending_arithmetic() {
                                        match crate::runtime::exceptions::throw_runtime_error(
                                            shared,
                                            thread,
                                            RuntimeError::ArithmeticException {
                                                message: "/ by zero".to_string(),
                                            },
                                        ) {
                                            MethodCallFailed::ExceptionThrown(exc) => {
                                                jit_early_exception = Some(exc);
                                                true
                                            }
                                            other => return Err(other),
                                        }
                                    } else {
                                        false
                                    };
                                // Deopt sentinel: i64::MIN means the JIT method was deoptimized
                                // via jit_uncommon_trap.  Fall through to the interpreter to
                                // re-execute the method from scratch.
                                // If an NPE or AIOOBE was routed above, also fall through so
                                // the post-frame-push exception handler walker (~line 2340)
                                // gets a chance to catch it in the JIT'd method.
                                if !npe_routed
                                    && !aioobe_routed
                                    && !arith_routed
                                    && result != i64::MIN
                                {
                                    return match ret_type {
                                        // Cast: JIT ABI -- i64 register convention
                                        b'I' | b'Z' | b'B' | b'C' | b'S' => {
                                            Ok(Some(Value::Int(result as i32)))
                                        }
                                        b'J' => Ok(Some(Value::Long(result))),
                                        b'F' => {
                                            // Cast: integer word reinterpreted as float/double bit pattern
                                            Ok(Some(Value::Float(f32::from_bits(result as u32))))
                                        } // Cast: JIT ABI -- i64 register convention
                                        b'D' => {
                                            Ok(Some(Value::Double(f64::from_bits(result as u64))))
                                        } // Cast: JIT ABI -- i64 register convention
                                        b'[' | b'L' => {
                                            if result == 0 {
                                                Ok(Some(Value::Object(None)))
                                            } else {
                                                // SAFETY: result is a non-zero JIT return value encoding a heap pointer to a valid object header.
                                                Ok(Some(Value::Object(Some(unsafe {
                                                    crate::types::ObjectRef::from_raw(
                                                        result as *mut u8,
                                                    ) // Cast: JIT ABI — i64 register convention
                                                }))))
                                            }
                                        }
                                        _ => Ok(None),
                                    };
                                }
                                // FIX (2026-07-07, jit-invokedynamic-groovy-regression,
                                // FOURTH-pass root cause): this is the OLDEST JIT call-dispatch
                                // site in the interpreter (the first-call tier-up path in
                                // `execute()` itself), predating the real-frame-deopt resume
                                // machinery. It never consumed `take_last_deopt()`, so a
                                // reconstructed frame stashed by `x64_deopt_entry` here (which,
                                // since the invokedynamic-trap snapshot became unconditional,
                                // happens on every reason-8 trap reached through THIS path) was
                                // silently left behind instead of cleared -- a LATER, UNRELATED
                                // deopt check on the same thread could then pick up this STALE
                                // frame and resume with the wrong locals/stack.
                                //
                                // That alone would not be sufficient: unlike the safe-reject
                                // helper (`jit_uncommon_trap`, the `frame_box_ptr == None` arm of
                                // `emit_deopt_stubs`), which synchronously calls
                                // `DeoptimizationController::deoptimize` and so blacklists
                                // (`MakeNotCompilable`) an `UnreachedCode` trap on FIRST
                                // occurrence regardless of which VM call site is running, the
                                // precise-resume path (`x64_deopt_entry`) relies on the CALLER to
                                // drive de-speculation via `real_frame_deopt_resume_and_
                                // despeculate` -- which only the three newer call sites do. This
                                // legacy site fell through to "re-run the whole method
                                // interpreted" WITHOUT EVER BLACKLISTING the method, so the next
                                // call re-entered the JIT-compiled artifact and re-hit the SAME
                                // trap again -- for Groovy's `IndyInterface`-heavy dispatch
                                // (essentially every call), that meant re-running a script's own
                                // class/method-generation bytecode from entry ~2000 times in a
                                // single test, each re-run re-registering a `doCall` method
                                // Groovy's own compiler had already registered -- exactly the
                                // "doCall duplicates another method" symptom. Fixed by driving the
                                // same de-speculation call here that `jit_uncommon_trap` does,
                                // using this method's own identity (`class_name_str`/
                                // `method_name`/`method_descriptor` are already in scope, since
                                // this whole block IS that method's own first-call JIT tier-up)
                                // and the reason/bci recovered from the stashed frame itself
                                // before discarding it.
                                if let Some(rframe_for_despec) =
                                    cratonvm_jit::deopt::take_last_deopt()
                                {
                                    dbg_deopt_sink(
                                        "execute-first-call-tierup",
                                        &rframe_for_despec,
                                        &format!(
                                            "{class_name_str}.{method_name}{method_descriptor}"
                                        ),
                                    );
                                    let deopt_reason = compiled
                                        .deopt_points
                                        .iter()
                                        .find(|dp| dp.bci == rframe_for_despec.bci)
                                        .map(|dp| dp.reason)
                                        .unwrap_or(cratonvm_jit::deopt::DeoptReason::UnreachedCode);
                                    // Which of the three independent gates
                                    // refused is the whole diagnosis, and they
                                    // need completely different fixes — record
                                    // it for the message below.
                                    let resume_gate_ok = compiled.can_deopt_resume;
                                    let key_matches = deopt_frame_matches_method(
                                        &rframe_for_despec,
                                        &class_name_str,
                                        method_name,
                                        method_descriptor,
                                    );
                                    // De-speculate THIS method only when the
                                    // frame is this method's. It used to run
                                    // unconditionally, so a stash belonging to
                                    // a nested compiled callee blacklisted the
                                    // innocent method whose tier-up happened to
                                    // be running — while the `!key_matches`
                                    // branch below ALSO de-speculated the
                                    // frame's real owner. Two methods made
                                    // not-compilable per foreign frame, one of
                                    // them for no reason: on `TestScript` that
                                    // is `StringFunction1.getValue`, a
                                    // per-row expression evaluator.
                                    if key_matches {
                                        let _ = crate::jit::helpers::DeoptimizationController::deoptimize(
                                            shared,
                                            &class_name_str,
                                            method_name,
                                            method_descriptor,
                                            deopt_reason,
                                            rframe_for_despec.bci,
                                        );
                                    }
                                    // This first-call tier-up sink historically
                                    // discarded the reconstructed frame and
                                    // restarted the method at bci 0. That
                                    // duplicates every side effect committed
                                    // before the guard. Build the same cached
                                    // metadata the hot callsites use and resume
                                    // the captured frame directly.
                                    // The stash belongs to a DIFFERENT method — a
                                    // nested compiled callee whose sentinel bubbled
                                    // out to here because the call site that invoked
                                    // it emitted no callee-deopt service check (the
                                    // statically-bound JIT-to-JIT direct-call gap
                                    // fixed in jit/src/lib.rs; it was NOT inlining —
                                    // the x64 inliner rolls back any body that
                                    // publishes deopt metadata, and an inlined deopt
                                    // point carries the CALLER's method_key anyway).
                                    // THIS method never trapped, so the
                                    // "refusing side-effecting replay" arm below does
                                    // not apply to it: that refusal is about a frame
                                    // proving OUR OWN native code ran past bci 0, and
                                    // a foreign frame proves nothing of the sort.
                                    //
                                    // Raising a fatal `InternalError` here also made
                                    // an orphan permanent in a second way: the frame
                                    // had already been TAKEN, so every later
                                    // `has_last_deopt()` was clean, but only because
                                    // the VM had died. Before the take it poisoned the
                                    // sentinel disambiguation
                                    // (`jit_dispatch_threw`) for unrelated call sites.
                                    //
                                    // Do what the sibling tier-up sink
                                    // (`jit-callsite-b`, jit_bridge.rs) already does
                                    // for exactly this case: de-speculate the frame's
                                    // real owner so it stops re-trapping, drop the
                                    // frame, and fall through to interpreted
                                    // execution of this (innocent) method. Measured on
                                    // `org.h2.test.scripts.TestScript`, which the
                                    // fatal error killed at ~212 s with
                                    // `stashed key "org/h2/util/StringUtils.cache:..."`
                                    // while running `StringFunction1.getValue`.
                                    let mut materialize_failed = false;
                                    if resume_gate_ok && key_matches {
                                        let cached = Arc::new(CachedBytecodeMethod {
                                            declaring_class_id: class_id,
                                            class_name: Arc::from(class_name_str.as_str()),
                                            method_name: Arc::from(method_name),
                                            method_descriptor: Arc::from(method_descriptor),
                                            source_file: source_file.as_deref().map(Arc::from),
                                            code: crate::runtime::frame::padded_bytecode(
                                                &code_attr.code,
                                            ),
                                            exception_table: Arc::from(
                                                code_attr.exception_table.as_slice(),
                                            ),
                                            max_stack: code_attr.max_stack,
                                            max_locals: code_attr.max_locals,
                                            num_params: count_method_params(method_descriptor)
                                                as u16,
                                            is_synchronized,
                                            is_static,
                                            force_native_cache: std::sync::OnceLock::new(),
                                            descriptor_facts_cache: std::sync::OnceLock::new(),
                                            intercept_shape_cache: std::sync::OnceLock::new(),
                                            interp_invocations: std::sync::atomic::AtomicU32::new(0),
                                            native_callback_cache: std::sync::OnceLock::new(),
                                            invoc_key: std::sync::OnceLock::new(),
                                            jit_probe_generation: std::sync::atomic::AtomicU64::new(
                                                0,
                                            ),
                                            quickened: std::sync::OnceLock::new(),
                                        });
                                        let pin_base = thread.native_pin_roots.len();
                                        let built = build_deopt_frame_inner(
                                            shared,
                                            thread,
                                            &cached,
                                            &rframe_for_despec,
                                            false,
                                        );
                                        materialize_failed = built.is_none();
                                        if let Some(frame) = built {
                                            // Keep materialization pins live
                                            // through the frame-push handoff and
                                            // the resumed execution. The frame
                                            // itself is authoritative after push;
                                            // retaining the pins a little longer
                                            // is conservative and guarantees
                                            // cleanup on every returned result.
                                            let resumed =
                                                execute_prebuilt_frame(shared, thread, frame);
                                            thread.native_pin_roots.truncate(pin_base);
                                            return resumed;
                                        }
                                        thread.native_pin_roots.truncate(pin_base);
                                    }
                                    if !key_matches {
                                        // The stash is a DIFFERENT method's — a
                                        // nested compiled callee whose sentinel
                                        // bubbled out to here because the call site
                                        // that invoked it had no callee-deopt service
                                        // check, so `try_resume_trapped_callee` never
                                        // ran there and the frame kept travelling
                                        // outward looking for the "outer consumer that
                                        // CAN attribute it" — which, once the sentinel
                                        // has passed through the callee's own caller,
                                        // no longer exists. This arm is now defence in
                                        // depth: the emission gap it was written for
                                        // is fixed in jit/src/lib.rs.
                                        //
                                        // The refusal below does NOT apply to it: it
                                        // exists because a frame belonging to THIS
                                        // method proves this method's native code ran
                                        // past bci 0, and a foreign frame proves
                                        // nothing about this method at all. Raising a
                                        // fatal `InternalError` for someone else's
                                        // orphan killed the whole VM run — measured on
                                        // `org.h2.test.scripts.TestScript`, dead at
                                        // ~212 s with `stashed key
                                        // "org/h2/util/StringUtils.cache:(...)"`
                                        // while running
                                        // `StringFunction1.getValue`, and on
                                        // `TestCrashAPI` the same way.
                                        //
                                        // Do exactly what the sibling tier-up sink
                                        // already does for this case
                                        // (`jit-callsite-b`, jit_bridge.rs): the
                                        // frame's real owner is de-speculated above
                                        // via `DeoptimizationController::deoptimize`
                                        // so it stops re-trapping, the orphan is
                                        // dropped (it was taken at the top of this
                                        // block, which is also what stops it
                                        // poisoning `has_last_deopt`'s sentinel
                                        // disambiguation at unrelated later call
                                        // sites), and this innocent method falls
                                        // through to interpreted execution.
                                        despeculate_stashed_frame_method(
                                            shared,
                                            &rframe_for_despec,
                                        );
                                    } else if replay_from_entry_is_observably_equivalent(
                                        &code_attr.code,
                                        compiled.spliced_bodies_side_effect_free,
                                        rframe_for_despec.bci,
                                    ) {
                                        // The refusal below is about DUPLICATED
                                        // SIDE EFFECTS, and this method has none
                                        // to duplicate: no store outside the
                                        // frame, no call, no monitor action. Its
                                        // locals are rebuilt from the same
                                        // arguments, so a re-run from bci 0 is
                                        // observably the abandoned attempt.
                                        //
                                        // This arm is not a relaxation, it is the
                                        // other half of a rule that was only ever
                                        // written down once.
                                        // `ir_unresumable_protected_trap` ADMITS
                                        // the optimizing tier for a protected
                                        // range carrying an unresumable trap when
                                        // the range is read-only, on the stated
                                        // ground that "a read-only
                                        // `try { return a[i]; } catch (...)`
                                        // replays harmlessly, so the refusal would
                                        // buy nothing and cost the compile" — and
                                        // then this sink refused every replay,
                                        // harmless or not. The compiler's
                                        // narrowing rested on a behaviour the
                                        // consumer did not have, so exactly the
                                        // shape it deliberately let through was
                                        // the shape that died here.
                                        //
                                        // `probes/EscapeKindProbe.java` is the
                                        // witness: its `virtual` / `iface` /
                                        // `special` arms are
                                        // `try { return 10 / (i - i); }
                                        // catch (IllegalStateException e) { ... }`
                                        // reached through the dispatch helper, and
                                        // all three died with `InternalError:
                                        // precise deoptimization unavailable ...
                                        // refusing side-effecting replay` where
                                        // HotSpot and `--nojit` return the
                                        // ArithmeticException the caller catches.
                                        //
                                        // The two ends now ask ONE predicate
                                        // (`opcode_commits_side_effect`) so they
                                        // cannot drift apart again.
                                        if cratonvm_types::flags::runtime_var_os(
                                            "CRATONVM_DBG_DEOPT",
                                        )
                                        .is_some()
                                        {
                                            eprintln!(
                                                "[cratonvm-deopt] replaying {}.{}{} from entry: \
                                                 unresumable at bci {} but the body commits no \
                                                 side effect",
                                                class_name_str,
                                                method_name,
                                                method_descriptor,
                                                rframe_for_despec.bci,
                                            );
                                        }
                                    } else {
                                        // Precise reconstruction is a correctness
                                        // requirement once native code has executed
                                        // past bci 0. Refuse a whole-method replay:
                                        // it is observably wrong for methods with
                                        // stores, I/O, monitor actions, or callbacks.
                                        let why = if !resume_gate_ok {
                                            "can_deopt_resume=false (no deopt points, \
                                         or an elided monitor)"
                                        } else if materialize_failed {
                                            "the frame could not be materialised from its map"
                                        } else {
                                            "unknown"
                                        };
                                        // `deopt_reason` DEFAULTS to `UnreachedCode` when no
                                        // point carries this bci, so printing it bare names a
                                        // reason nothing ever requested — which is how the
                                        // spliced-bci defect read as an `UnreachedCode` trap
                                        // for a day. Say which of the two this is.
                                        let reason_is_real = compiled
                                            .deopt_points
                                            .iter()
                                            .any(|dp| dp.bci == rframe_for_despec.bci);
                                        let reason_note = if reason_is_real {
                                            "reason"
                                        } else {
                                            "NO deopt point carries this bci, so the reason \
                                         below is this sink's default rather than a \
                                         request — reason"
                                        };
                                        return Err(MethodCallFailed::InternalError(
                                            VmError::Internal {
                                                message: format!(
                                                    "precise deoptimization unavailable for \
                                                 {}.{}{} at bci {} ({}, stashed key {:?}, \
                                                 inline callers {}, {} {:?}); \
                                                 refusing side-effecting replay",
                                                    class_name_str,
                                                    method_name,
                                                    method_descriptor,
                                                    rframe_for_despec.bci,
                                                    why,
                                                    rframe_for_despec.method_key,
                                                    rframe_for_despec.caller_frames.len(),
                                                    reason_note,
                                                    deopt_reason,
                                                ),
                                            },
                                        ));
                                    }
                                }
                                // Deoptimized — pending-NPE drain was hoisted above the
                                // i64::MIN branch (round-8 CRIT fix); fall through to
                                // interpreter execution.
                            }
                        }
                    } // end else (jit_early_exception.is_none())
                }
            }
        } // end if !already_skipped
    } // end JIT block

    // Check stack overflow before pushing frame
    if thread.frames.len() >= shared.config.max_stack_depth {
        dump_stack_on_soe(thread);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }

    // Record the frame depth before we push so that on any error path we can
    // truncate the stack back to exactly this depth (plus the one frame we push).
    // This prevents orphaned inner frames when execute_frame returns early via
    // `return Err(e)` without unwinding the frames it pushed for callees.
    let frames_depth_before_push = thread.frames.len();

    // Create the frame.
    // PERF (round-5 vm #9 fix): use `Frame::new_from_arcs` so that the
    // bytecode is taken straight from `code_attr.code: ByteView` (deref
    // to `&[u8]`) through `padded_bytecode(&[u8])` (one alloc for the +2
    // zero-padding) instead of first cloning the bytes into an owned
    // `Vec<u8>` (`.to_vec()`) and then copying *that* into the padded Arc
    // inside `Frame::new` — two full bytecode mem-copies per method entry.
    // The synthetic test frames keep the convenient `Frame::new` entry
    // point that owns its `Vec<u8>`.
    //
    // NOTE (P4 reverted): an attempt to pool locals/stack here via
    // `Frame::new_pooled` caused execution hangs across the BouncyCastle
    // regression suites (bc-asn1/crypto/crypto-prng went rc=0 -> rc=124).
    // The uncached invoke path is not a safe consumer of the thread-local
    // frame pools as-is; reverted to the proven `new_from_arcs` path.
    let mut frame = Frame::new_from_arcs(
        class_id,
        std::sync::Arc::from(class_name_str.as_str()),
        std::sync::Arc::from(method_name),
        std::sync::Arc::from(method_descriptor),
        source_file.map(|s| std::sync::Arc::from(s.as_str())),
        // Interned per *method identity* (see `frame-arena.md` §5.2), not
        // content-addressed: `local_liveness.rs` keys a per-method liveness
        // table on `Arc::as_ptr(code)` and derives it from the method's
        // exception table, so two byte-identical methods with different
        // handler ranges must keep distinct Arcs. This ends a full
        // method-body memcpy plus an allocation per uncached method entry,
        // and lets `quickened.rs::intern` hit across frames of the same
        // method instead of rebuilding under a fresh address.
        crate::runtime::frame::padded_bytecode_for_method(
            class_id,
            method_name,
            method_descriptor,
            &code_attr.code,
        ),
        std::sync::Arc::from(code_attr.exception_table.into_boxed_slice()),
        code_attr.max_stack,
        code_attr.max_locals,
        args,
    );

    // GC-safety: if the JIT early-compile path produced an exception, root its
    // oop on the operand stack BEFORE `push_frame_and_fire_entry` fires the
    // JVMTI MethodEntry callback. That callback may allocate Java heap and
    // trigger a moving young-gen GC; an oop reachable only through the
    // `jit_early_exception` Rust local across the fire would be unrooted and
    // could be relocated, leaving a stale pointer for the handler walk /
    // propagation below. The frame's operand stack is GC-scanned, so we read
    // the (possibly relocated) reference back from it after the fire. The
    // pre-fire push is best-effort (`let _ =`): if it does not take — e.g. a
    // `max_stack == 0` method, which by definition has no operand-using
    // handler — we fall back to the original local, no worse than before.
    // Mirrors `route_jit_exception_through_method` / `resume_from_ir_deopt`.
    if let Some(exc) = jit_early_exception {
        let _ = frame.stack.push(Value::Object(Some(exc)));
    }

    // Push frame onto thread
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/invoke_method_shared] depth={} {}.{}{}",
            thread.frames.len(),
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor()
        );
    }
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);
    if shared
        .mem
        .gc_barrier
        .stw_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        safepoint_check(shared, thread);
    }

    // If the JIT early-compile path encountered a Java exception from a callee,
    // route it through this method's exception table before interpreter execution.
    if let Some(early_exc) = jit_early_exception {
        // The frame has been pushed. Search its exception table for a handler.
        let frame_idx = thread.frames.len() - 1;
        // Re-read the exception oop from the (GC-scanned) operand stack: a GC
        // during the MethodEntry callback may have relocated it, and only the
        // scanned frame slot was updated — not the original Rust local. Fall
        // back to the bound `early_exc` (the known-Some local) if the pre-fire
        // push did not take (e.g. a `max_stack == 0` method). Binding it in the
        // `if let` keeps this fallback panic-free (no `.expect()`), satisfying
        // the strict-zero production-panic gate.
        let exc = match thread.frames[frame_idx].stack.pop() {
            Ok(Value::Object(Some(r))) => r,
            _ => early_exc,
        };
        // The JIT executed the entire method body as native code, so there is
        // no live throw-site PC. Previously this passed the freshly-pushed
        // frame's `last_instr_pc` (always 0) to the PC-ranged search, which
        // silently missed every handler whose `try` region does not start at
        // offset 0 — e.g. a JIT-compiled `Main.main` whose `catch (Throwable)`
        // protects `[2,26)` would never catch a callee exception, leaking it
        // past the method (Jetty `start.jar` launcher). Use the PC-unknown
        // search instead: it skips catch-all `finally` entries (unsafe to
        // match without a PC) but matches typed handlers by exception class.
        // ...unless the compiled body stamped its own throw site, which it does
        // at every throwing bci inside a protected range. The PC-unknown search
        // matches typed handlers by exception CLASS ALONE, so a method with two
        // protected ranges catching the same type gets the FIRST row's handler
        // whichever range actually threw. bc-java's
        // `ProvRevocationChecker.check` is exactly that shape — `try { crl }
        // catch (Recoverable) { ...ocsp... }` and `try { ocsp } catch
        // (Recoverable) { ...crl... }` — so an OCSP failure ran the CRL branch's
        // handler, which re-called OCSP, and the exception escaped a method that
        // was supposed to fall back. The stamp makes the range check possible;
        // `jit_local_athrow_pc_in_frame` refuses it unless it lands in one of
        // THIS method's ranges, so a foreign stamp still degrades to the
        // pc-unknown search rather than picking a handler at random.
        let found =
            match jit_local_athrow_pc_in_frame(&thread.frames[frame_idx], jit_early_throw_bci) {
                JitThrowPc::InRange(pc) => {
                    find_exception_handler_any_pc(shared, &thread.frames[frame_idx], pc, exc)
                }
                // The compiled body named a throw site of its own that no `try`
                // covers: nothing here can catch it, and the pc-unknown search
                // would match a typed row by exception class alone.
                JitThrowPc::OutsideAllRanges(_) => None,
                JitThrowPc::Unknown => {
                    find_exception_handler_pc_unknown(shared, &thread.frames[frame_idx], exc)
                }
            };
        match found {
            Some((handler_pc, exc_ref)) => {
                thread.frames[frame_idx].stack.clear();
                let _ = thread.frames[frame_idx]
                    .stack
                    .push(Value::Object(Some(exc_ref)));
                thread.frames[frame_idx].pc = handler_pc;
                fire_jvmti_exception_catch(
                    shared.vm_identity,
                    &thread.frames[frame_idx],
                    handler_pc,
                );
                // Fall through to execute_frame which will resume at handler_pc
            }
            None => {
                // No handler in this method — pop frame and propagate
                pop_and_recycle_frame(shared, thread);
                return Err(MethodCallFailed::ExceptionThrown(exc));
            }
        }
    }

    // Execute (with panic protection for stack underflow/overflow).
    //
    // NOTE(round-4-wave-3): the per-method `catch_unwind` here is load-bearing
    // and intentionally retained. The interpreter's super-instruction fast
    // path uses `pop_unchecked` / `set_local_unchecked`, which can panic if a
    // malformed or bridge-corrupted frame violates verified stack shapes.
    // Without this
    // catch_unwind, those panics would propagate past the JIT entry frame and
    // abort the process under the Windows SEH / signal-handler interop in
    // `runtime/signals.rs` (the signal handler converts SIGSEGV / SIGFPE via
    // `catch_unwind`, but a Rust panic crossing the JIT-call boundary is not
    // catchable by the OS unwinder). Global `-Xverify:none` disables the raw
    // handlers; this `catch_unwind` remains the final guard for trusted
    // per-class verification bypasses and corrupted bridge state. The ~10 ns setup cost is
    // amortized over the entire `execute_frame` invocation — hundreds to
    // thousands of bytecodes — not per-bytecode.
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        execute_frame(shared, thread)
    })) {
        Ok(r) => r,
        Err(panic_info) => {
            // Convert panics (e.g., pop_unchecked underflow) to errors
            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic in bytecode execution".to_string()
            };
            if let Some(f) = thread.frames.last() {
                eprintln!(
                    "[PANIC_IN] {}.{}{} pc={} max_stack={} :: {}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc,
                    f.max_stack,
                    msg
                );
            }
            // cceres2: if this panic is the CRATONVM_DBG_STALE_OBJREF canary,
            // attribute the stale address against THIS thread's own state —
            // the heap-side holder scan runs in get_header; this covers the
            // thread-local half (frame locals/stack, pins, snapshot), which
            // is where a root-remap gap lives.
            {
                let stale = cratonvm_gc::stale_objref_debug::LAST_STALE_ADDR
                    .swap(0, std::sync::atomic::Ordering::AcqRel);
                if stale != 0 {
                    let mut found = 0usize;
                    for (fi, fr) in thread.frames.iter().enumerate() {
                        for li in 0..fr.locals_len() {
                            if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                                if o.as_ptr() as usize == stale {
                                    eprintln!(
                                        "[STALE-FRAME] frame#{fi} {}.{} pc={} local[{li}] holds stale 0x{stale:x}",
                                        fr.class_name(), fr.method_name(), fr.pc,
                                    );
                                    found += 1;
                                }
                            }
                        }
                        for si in 0..fr.stack.len() {
                            if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                                if o.as_ptr() as usize == stale {
                                    eprintln!(
                                        "[STALE-FRAME] frame#{fi} {}.{} pc={} stack[top-{si}] holds stale 0x{stale:x}",
                                        fr.class_name(), fr.method_name(), fr.pc,
                                    );
                                    found += 1;
                                }
                            }
                        }
                    }
                    for (pi, p) in thread.native_pin_roots.iter().enumerate() {
                        if p.as_ptr() as usize == stale {
                            eprintln!(
                                "[STALE-FRAME] native_pin_roots[{pi}] holds stale 0x{stale:x}"
                            );
                            found += 1;
                        }
                    }
                    {
                        let snap = thread.root_snapshot.lock();
                        for (si, r) in snap.iter().enumerate() {
                            if r.as_ptr() as usize == stale {
                                eprintln!(
                                    "[STALE-FRAME] root_snapshot[{si}] holds stale 0x{stale:x}"
                                );
                                found += 1;
                            }
                        }
                    }
                    eprintln!(
                        "[STALE-FRAME] summary: {} thread-state slot(s) hold stale 0x{stale:x} (tid={}, blocked={})",
                        found,
                        thread.thread_id.0,
                        thread
                            .gc_block_state
                            .in_blocked_region
                            .load(std::sync::atomic::Ordering::Acquire),
                    );
                }
            }
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NotImplemented { feature: msg },
            )))
        }
    };

    // A continuation yield is a scheduler control transfer, not a method
    // failure. Keep every frame exactly as execute_frame left it so the
    // virtual-thread manager can freeze the complete Java stack.
    if matches!(
        result,
        Err(MethodCallFailed::InternalError(
            VmError::ContinuationYield { .. }
        ))
    ) {
        return result;
    }

    // Truncate any orphaned inner frames that execute_frame may have left on the
    // stack when it returned early via `return Err(e)` without popping callees.
    // We expect exactly `frames_depth_before_push + 1` frames here (the one we
    // pushed above). Pop everything above that level before popping our own frame.
    while thread.frames.len() > frames_depth_before_push + 1 {
        pop_and_recycle_frame(shared, thread);
    }

    // Pop frame and recycle its Vec allocations
    pop_and_recycle_frame(shared, thread);

    result
}

/// Resume a complete Java stack restored from a virtual-thread continuation.
///
/// Unlike `execute`, no new root frame is created: every frame already carries
/// its exact bytecode PC, locals, operand stack, exception table, and method
/// identity. Dispatch begins at the youngest frame and may return/unwind
/// through all restored callers down to index zero.
pub fn resume_continuation(shared: &SharedVm, thread: &mut JvmThread) -> MethodCallResult {
    if thread.frames.is_empty() {
        return Ok(None);
    }
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[CONTINUATION_RESUME] frames={}", thread.frames.len());
        for (index, frame) in thread.frames.iter().enumerate() {
            eprintln!(
                "  [{index}] {}.{}{} pc={} stack={}",
                frame.class_name(),
                frame.method_name(),
                frame.method_descriptor(),
                frame.pc,
                frame.stack.len(),
            );
        }
    }
    let result = execute_frame_from_index(shared, thread, 0);
    if matches!(
        result,
        Err(MethodCallFailed::InternalError(
            VmError::ContinuationYield { .. }
        ))
    ) {
        return result;
    }
    while thread.frames.len() > 1 {
        pop_and_recycle_frame(shared, thread);
    }
    pop_and_recycle_frame(shared, thread);
    result
}

/// jit-invokedynamic-groovy-regression fix — run an already-materialized
/// interpreter frame (e.g. one reconstructed from a JIT deopt snapshot, with
/// its `pc` mid-method and operand stack pre-populated) to completion on this
/// thread, returning the method result exactly like [`execute`].
///
/// This is the dispatch-helper precise-resume primitive
/// (`try_resume_trapped_callee`, vm/src/jit/helpers.rs): a compiled callee
/// that hit its unconditional `invokedynamic` uncommon trap left a precise
/// frame snapshot; resuming that frame HERE — at the helper call site, before
/// any compiled caller's epilogue bail — completes the callee in the
/// interpreter and hands the real result back to the compiled caller, so no
/// side effect is re-run and no sentinel escapes.
///
/// Mirrors [`execute`]'s tail exactly: push + entry hooks, STW safepoint
/// check, panic-protected `execute_frame`, orphaned-inner-frame truncation,
/// pop + recycle. See `execute`'s own comments for why each step exists.
pub(crate) fn execute_prebuilt_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame: Frame,
) -> MethodCallResult {
    let frames_depth_before_push = thread.frames.len();
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/execute_prebuilt_frame] depth={} {}.{}{} pc={}",
            thread.frames.len(),
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor(),
            frame.pc
        );
    }
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);
    run_pushed_frame_to_completion(shared, thread, frames_depth_before_push)
}

/// The tail of [`execute_prebuilt_frame`], for a caller that has ALREADY
/// pushed the frame it wants run.
///
/// `frames_depth_before_push` is the depth `thread.frames` had before that
/// push, so the orphaned-inner-frame truncation and the final pop can restore
/// it exactly. Split out for the deopt-resume / exception-routing sinks, which
/// build and push their frame themselves and then need it run to completion
/// synchronously rather than handed to the interpreter's stepping loop — see
/// `jit_bridge::execute_jit_call_oneshot`, whose whole reason for existing is
/// that a one-shot dispatch helper has no such loop to hand it to.
pub(crate) fn run_pushed_frame_to_completion(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frames_depth_before_push: usize,
) -> MethodCallResult {
    debug_assert!(thread.frames.len() > frames_depth_before_push);
    if shared
        .mem
        .gc_barrier
        .stw_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        safepoint_check(shared, thread);
    }

    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        execute_frame(shared, thread)
    })) {
        Ok(r) => r,
        Err(panic_info) => {
            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic in bytecode execution".to_string()
            };
            if let Some(f) = thread.frames.last() {
                eprintln!(
                    "[PANIC_IN/prebuilt] {}.{}{} pc={} max_stack={} :: {}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc,
                    f.max_stack,
                    msg
                );
            }
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NotImplemented { feature: msg },
            )))
        }
    };

    // Truncate any orphaned inner frames execute_frame may have left, then pop
    // our own frame — same accounting as `execute`.
    while thread.frames.len() > frames_depth_before_push + 1 {
        pop_and_recycle_frame(shared, thread);
    }
    pop_and_recycle_frame(shared, thread);

    result
}

// ---------------------------------------------------------------------------
// PGO profiling helpers
// ---------------------------------------------------------------------------

/// Build a `MethodKey` from a frame, used to key into the `ProfileStore`.
///
/// Called only at branch/invoke sites during warmup — the Arc clones are
/// acceptable overhead before JIT compilation takes over.
///
/// AUDIT CRIT-3 fix: removed `#[inline(never)]`.  In the hot-loop branch
/// sites this is guarded by `jit_profile::is_profiling_enabled()`, so it
/// only executes during active warmup; allowing inlining lets LLVM CSE
/// the two `Arc<str>` clones across adjacent record_branch/record_backedge
/// pairs (e.g. `if_icmp*` + back-edge `record_backedge`).
///
/// LOW — takes the frame by ref and uses the ref-returning
/// `method_name_arc_ref` / `method_descriptor_arc_ref` accessors to
/// hold off on the `Arc::clone` refcount bumps until the `MethodKey`
/// is actually being constructed (the call site can elide the call
/// entirely when profiling is disabled). Previously each call did 2
/// `Arc::clone` atomic ops up-front in the accessor, plus 2 more
/// implicit clones into the MethodKey fields.
#[inline]
fn make_method_key(frame: &crate::runtime::frame::Frame) -> MethodKey {
    MethodKey {
        class_id: frame.class_id.as_u32(),
        method_name: Arc::clone(frame.method_name_arc_ref()),
        descriptor: Arc::clone(frame.method_descriptor_arc_ref()),
    }
}

/// PERF (round-5 vm #7): borrowed-key variant of `make_method_key` used by
/// the hot PGO recording paths.  Returns the three key components by value
/// (`u32`) and reference (`&Arc<str>`), letting the `ProfileStore`
/// `record_*_borrowed` family skip the two `Arc::clone` atomic bumps that
/// `MethodKey` construction otherwise requires per branch / back-edge.
#[inline]
fn method_key_parts<'a>(
    frame: &'a crate::runtime::frame::Frame,
) -> (u32, &'a Arc<str>, &'a Arc<str>) {
    (
        frame.class_id.as_u32(),
        frame.method_name_arc_ref(),
        frame.method_descriptor_arc_ref(),
    )
}

// ---------------------------------------------------------------------------
// Frame pop helper (releases synchronized monitor if present)
// ---------------------------------------------------------------------------

/// Pop the top frame from `thread.frames`, release its monitor (if any), and
/// recycle the frame's allocations.  This must be used in place of bare
/// `thread.frames.pop()` + `thread.recycle_frame()` whenever a frame pushed
/// via the stackless invoke path is being removed (return or exception unwind).
///
/// T10.7 — delegates to `JvmThread::recycle_frame_with_shared` so overflow
/// from the per-thread pool spills into the VM-wide `operand_stack_pool` /
/// `tag_pool` instead of being dropped on the floor.
#[inline]
pub fn pop_and_recycle_frame(shared: &SharedVm, thread: &mut JvmThread) {
    pop_and_recycle_frame_with_reason(shared, thread, /*was_popped_by_exception=*/ false);
}

/// Pop the top frame and return its storage to the per-thread / VM-wide
/// pools.  `was_popped_by_exception` controls whether the JVMTI `FramePop`
/// and `MethodExit` events fire with the abrupt-completion flag set.
///
/// The callers reach this via:
///   * Normal return (`pop_and_recycle_frame`) — `false`.
///   * Exception unwind through stackless / bytecode frames —
///     pass `true` directly.
pub fn pop_and_recycle_frame_with_reason(
    shared: &SharedVm,
    thread: &mut JvmThread,
    was_popped_by_exception: bool,
) {
    // T17.Δ.2 — JVMTI MethodExit on exception unwind. Normal-return exits
    // are already fired from the return opcodes; here we handle only the
    // abrupt case. Cost when no agent is subscribed: single Acquire load.
    //
    // The guard is the process-wide union flag (over-approximates: it can be
    // true because a *different* VM has a MethodExit listener); the delivery
    // is `_for_vm`, which resolves this VM's environment and re-checks that
    // environment's own enable set. Guards may over-approximate, delivery
    // may not.
    if was_popped_by_exception && crate::runtime::jvmti::any_method_exit_listener_active() {
        if let Some(top) = thread.frames.last() {
            let method_id = synth_method_id(top);
            crate::runtime::jvmti::fire_method_exit_for_vm(
                shared.vm_identity,
                thread.thread_id.0,
                method_id,
                true,
                crate::runtime::jvmti::LocalValue::Object(None),
            );
        }
    }
    // T17.Δ.5 — JVMTI FramePop before the frame vanishes.
    fire_jvmti_frame_pop_if_requested(shared.vm_identity, thread, was_popped_by_exception);
    // The dying frame is read THROUGH THE STACK, not moved out of it.
    //
    // This block used to open with `if let Some(f) = thread.frames.pop()`, and
    // `f` then travelled into `recycle_frame_with_shared` and again into
    // `take_pool_parts` — three moves of a ~300-byte `Frame` to arrive at four
    // `Vec` headers. `perf` on the interpreted-invoke probe put `memcpy` under
    // `Vec::pop<Frame>` here, inside a frame-lifecycle group worth ~24.7% of
    // the invoke arm (see the annotation-scan known-issues page). Nothing below
    // needs the frame anywhere but where it already is.
    let depth = thread.frames.len();
    if depth > 0 {
        // Root-snapshot cache correctness: the frame that becomes the top again
        // (the caller this return/unwind exposes) is about to RE-EXECUTE and may
        // reassign its locals. Bump its `exec_epoch` so the `(seq, exec_epoch)`
        // cache key invalidates its stale cached roots — `seq` alone is unchanged
        // (the caller was never popped) and would otherwise reuse roots that miss
        // a freshly-allocated local (the AME all-zero-receiver corruption). See
        // the `Frame::seq` / `exec_epoch` docs. Cheap: one add on the (cold)
        // return/unwind path. `wrapping_add` so a (practically impossible) u64
        // overflow can never alias a live cache key into a false match.
        //
        // Hoisted above the borrow of the dying frame: with that frame still on
        // the stack the caller is at `depth - 2`, and the bump needs `&mut`.
        if depth >= 2 {
            let caller = &mut thread.frames[depth - 2];
            caller.exec_epoch = caller.exec_epoch.wrapping_add(1);
        }
        let thread_id = thread.thread_id;
        let f = &thread.frames[depth - 1];
        // Harvest this activation's loop work towards the method's tier-up
        // counter. This is the ONLY point at which the count is complete and
        // still attributable: `Frame::backward_count` is reset on every reuse,
        // so a loop that runs a few hundred iterations per call — the shape of
        // `ConstantPool.<init>` and `ClassParser.readFields` in Tomcat's
        // annotation scan — is otherwise thrown away wholesale, over and over,
        // and never reaches the OSR back-edge threshold within any one frame.
        //
        // Harvesting HERE rather than on a back-edge stride is deliberate: a
        // stride can only ever credit loops longer than the stride, which is
        // exactly the set this gap does NOT contain. Every iteration counts,
        // however short the loop, at a cost of one hash + one relaxed atomic
        // per *invocation that actually looped*.
        if f.backward_count > 0 && loop_work_tierup_enabled() {
            let key = cratonvm_jit_api::invoc_key_parts(
                f.class_id.as_u32(),
                f.method_name(),
                f.method_descriptor(),
            );
            let total = shared
                .jit
                .profile_store
                .add_loop_work(key, f.backward_count);
            // Crossing the threshold is not enough: the ONLY place that acts on
            // the counter is the dispatch site, and it tests
            // `cnt == threshold || (cnt - threshold) % 64 == 0` against the value
            // ITS OWN increment returned. Credit applied here lands between two
            // dispatches, so those exact trigger points are simply stepped over
            // — `ConstantPool.<init>` was measured reaching a count of 1149,
            // more than twice the threshold, without ever being nominated.
            // Nominate from here instead, the same way the dispatch site does.
            let threshold = crate::runtime::env_cache::jit_invocation_threshold();
            if total >= threshold && crate::runtime::env_cache::bg_compile() {
                // Re-nominating an already-published method is cheap and
                // idempotent (the manager dedups), so a coarse retry stride is
                // enough to cover a nomination the worker dropped.
                let crossed_now = total.saturating_sub(f.backward_count / 32) < threshold;
                if crossed_now || total % 64 == 0 {
                    ensure_bg_compiler_started(shared);
                    let tiered_key = crate::jit::tiered::MethodKey::new(
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor(),
                    );
                    let _ = shared
                        .jit
                        .tiered_manager
                        .on_method_invocation_observed(&tiered_key, total as u64);
                }
            }
            // `CRATONVM_DBG=loop-work` — the lever's own witness. A tier-up
            // change that cannot be seen doing anything is indistinguishable
            // from an inert one, and this lever has already been inert twice.
            if loop_work_dbg() {
                static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 40 || total >= crate::runtime::env_cache::jit_invocation_threshold() {
                    eprintln!(
                        "[loop-work] {}.{}{} backedges={} count_now={}",
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor(),
                        f.backward_count,
                        total
                    );
                }
            }
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[FRAME_POP] depth={} {}.{}{}",
                thread.frames.len(),
                f.class_name(),
                f.method_name(),
                f.method_descriptor()
            );
        }
        if let Some(obj) = f.monitor_on_exit {
            // B8: implicit monitorexit on synchronized-method frame pop.
            // We can't propagate the error (the frame MUST be recycled here),
            // but silently swallowing loses diagnostics on monitor-state
            // corruption (e.g. user code that manually `monitorexit`ed past
            // the sync method's own counter). Log via tracing for visibility.
            if let Err(e) = crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, obj, thread_id)
            {
                tracing::warn!(
                    class = %f.class_name(),
                    method = %f.method_name(),
                    descriptor = %f.method_descriptor(),
                    thread_id = ?thread_id,
                    error = ?e,
                    "implicit monitorexit on synchronized-method-frame-pop failed"
                );
            }
        }
        // Harvest the four pooled `Vec`s out of the frame where it lies and let
        // `truncate` drop the husk in place — no `Frame` is moved.
        thread.recycle_top_frame_in_place(&shared.mem.operand_stack_pool, &shared.mem.tag_pool);
    }
}

// ---------------------------------------------------------------------------
// OSR back-edge orchestration helper
// ---------------------------------------------------------------------------

/// Outcome of a back-edge OSR attempt. Returned by [`try_osr_with_backoff`]
/// so callers can dispatch cleanly between the three possible outcomes
/// (return-from-execute_frame, continue-the-dispatch-loop, fall-through-to-
/// safepoint).
///
/// Round-7 HIGH #1: the surrounding OSR-rejection orchestration used to be
/// duplicated 14 times at each back-edge site (goto/if_icmp{eq,ne,lt,le,gt,
/// ge}/if{eq,ne,lt,le,gt,ge}/super-instruction iload-iload-if_icmplt). All
/// 14 callsites shared the identical sequence:
///
///   1. `should_try_osr(entry_pc, OSR_THRESHOLD)` — backoff schedule check.
///   2. `try_osr(...)` — JIT compile + entry trampoline.
///   3. On `Some(value)`: pop child frame if any, push return value, then
///      either `continue` the dispatch loop (nested call) or `return Ok(v)`
///      (root-frame return).
///   4. On `None`: `record_osr_rejection(entry_pc)` to schedule the next
///      retry exponentially further out.
///
/// This helper folds 1–4 into a single call so each callsite shrinks to
/// the dispatch-match on `OsrBackoffOutcome` plus the natural
/// `safepoint_check`/`continue` tail.
pub(crate) enum OsrBackoffOutcome {
    /// OSR didn't fire (either backoff not yet, or `try_osr` rejected and
    /// the rejection has been recorded). Caller falls through to its
    /// post-back-edge work (typically `safepoint_check` then `continue`).
    ///
    /// Since the RBC.6b lift this also covers a case where OSR very much DID
    /// fire: the OSR'd body raised an exception this method catches, and the
    /// live frame has been left parked at the handler. The caller's action is
    /// identical — resume interpreting this frame — but no rejection is
    /// recorded, because nothing was rejected. See `try_osr`'s `committed_out`.
    Skip,
    /// OSR completed and we're back at the root frame of this
    /// `execute_frame` invocation — bubble the return value up to the
    /// outer caller via `return Ok(value)`.
    ReturnOuter(Option<Value>),
    /// OSR completed an inner (stackless) call frame; the helper has
    /// already popped the recycled frame and pushed the return value
    /// onto the now-current frame's stack. Caller should `continue` the
    /// dispatch loop so it picks up at the parent's next instruction.
    /// The new current frame index is communicated back via the
    /// `frame_idx` out-parameter (which the helper mutates).
    ContinueDispatch,
    /// The OSR'd code exited with a Java exception in flight that this frame
    /// cannot catch. Until the RBC.6b lift that was a property of the whole
    /// population — an OSR'd method provably declared no exception table — and
    /// now it is a per-throw verdict reached by
    /// `route_osr_exception_out_of_artifact`: either no handler covers the
    /// precise throw bci, or the throw site lies outside every protected range.
    /// The caller must hand the throwable to the dispatch loop's
    /// `pending_java_exception` channel so it unwinds from THIS frame, instead
    /// of resuming the loop.
    ///
    /// The old behaviour here was `Skip` + a re-stashed exception, i.e.
    /// "keep interpreting this frame from where it was". That is correct only
    /// when the bail precedes any committed loop iteration; the exception can
    /// surface at any invoke, arbitrarily far into the loop, and every
    /// iteration the OSR'd body had already committed was then executed a
    /// second time by the interpreter. See the retired
    /// `jit-osr-bail-on-callee-exception-reruns-loop-iterations` write-up.
    ThrowJava(ObjectRef),
}

/// `CRATONVM_JIT=loop-work-tierup` — count loop iterations towards the method
/// invocation threshold. Read once and cached; this sits on the interpreter's
/// back-edge path. Default-OFF → behaviour byte-for-byte unchanged.
fn loop_work_tierup_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_LOOP_WORK_TIERUP").is_some()
    })
}

/// `CRATONVM_DBG=loop-work` — trace what [`loop_work_tierup_enabled`] actually
/// credits, so an inert lever is visibly inert instead of quietly so.
fn loop_work_dbg() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LOOP_WORK").is_some())
}

/// Run the standard back-edge OSR orchestration: backoff check → try_osr →
/// rejection bookkeeping. See [`OsrBackoffOutcome`] for the meaning of
/// each return variant.
///
/// **Borrow contract:** callers MUST drop any outstanding `&mut Frame`
/// borrow before calling this — the helper re-borrows `thread.frames`
/// internally (both for the should-try check and for the rejection
/// record). The standard sequence at each callsite is:
///
/// ```ignore
/// frame.backward_count += 1;
/// let entry_pc = frame.pc;
/// let _ = frame; // drop &mut borrow
/// match try_osr_with_backoff(shared, thread, &mut frame_idx,
///                             initial_frame_idx, entry_pc) {
///     OsrBackoffOutcome::ReturnOuter(v) => return Ok(v),
///     OsrBackoffOutcome::ContinueDispatch => continue,
///     OsrBackoffOutcome::ThrowJava(exc) => {
///         pending_java_exception = Some((exc, OSR_FRAME_DECLINED_TO_CATCH));
///         continue;
///     }
///     OsrBackoffOutcome::Skip => {}
/// }
/// safepoint_check(shared, thread);
/// ```
#[inline]
pub(crate) fn try_osr_with_backoff(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: &mut usize,
    initial_frame_idx: usize,
    entry_pc: usize,
) -> OsrBackoffOutcome {
    // osr-02 frame comparator: record the back-edge ARRIVAL here, ahead of
    // every early return below.
    //
    // The placement is the whole point. This function is the one funnel all
    // fourteen back-edge sites go through, and the records have to be taken
    // under conditions that do NOT depend on OSR — the ground-truth arm runs
    // `--nojit`, where every check below would decline. A hook placed after the
    // virtual-thread test, the `CRATONVM_JIT_OSR` gate or the backoff schedule
    // would emit in one arm and not the other, and the comparison would be
    // between two different things rather than between two runs of one.
    //
    // Costs one `OnceLock` bool load when the flag is unset, on a path that
    // already performs several cached environment reads.
    if osr_frame_trace::enabled() {
        osr_frame_trace::record_arrival(&thread.frames[*frame_idx], entry_pc);
    }
    if matches!(thread.kind, crate::threading::ThreadKind::Virtual) {
        return OsrBackoffOutcome::Skip;
    }
    // Back-edge OSR is default-on after the known entry-state corruption
    // blockers were retired. Whole-method JIT is unaffected. `CRATONVM_JIT_OSR=0`
    // opts out for diagnosis/bisection. This is the canonical entry for BOTH the
    // inline and background-OSR paths, so the gate disables OSR everywhere.
    if !crate::runtime::env_cache::osr_backedge_enabled() {
        return OsrBackoffOutcome::Skip;
    }
    // wire-tiered-manager Step 6: the per-frame back-edge OSR trigger is now an
    // env knob (`CRATONVM_TIER_OSR_BACKEDGE`); `OSR_THRESHOLD` remains the
    // canonical default. Live on both the inline and background-OSR paths. Unset
    // → identical to before.
    let osr_backedge_threshold =
        crate::runtime::env_cache::tier_osr_backedge().unwrap_or(OSR_THRESHOLD);
    // Loop work performed across SHORT invocations is otherwise invisible to
    // tier-up, and that gap is what keeps Tomcat's BCEL annotation scan
    // interpreted. Both counters miss it:
    //
    //   * the method invocation counter accumulates globally, but
    //     `ConstantPool.<init>` / `ClassParser.readFields` run ONCE PER CLASS —
    //     468 calls over the whole probe, under the 500 threshold;
    //   * `Frame::backward_count` is per-frame and reset on every invocation
    //     (`Frame::reset`), so a ~300-iteration constant-pool loop never reaches
    //     the 1000-back-edge OSR threshold WITHIN one frame — no matter how many
    //     classes are parsed. That is permanent, not a warm-up artifact.
    //
    // Measured: with the stock thresholds this workload reports `osr=0` — not a
    // single OSR body in the entire scan — while the per-constant methods it
    // calls (`Constant.readConstant`, `ConstantUtf8.getInstance`) compile fine.
    // See docs/known-issues/perf/interpreted-invoke-cost-350ns-20260825.md.
    //
    // Credit loop work towards that same invocation counter, the way HotSpot
    // sums its invocation and back-edge counters against a single threshold.
    // This reuses the existing counter and compile path exactly — no new state,
    // and no OSR involvement: the method simply crosses the ordinary threshold
    // and its NEXT call runs compiled.
    //
    // The credit itself is applied in `pop_and_recycle_frame_with_reason`, at
    // the one point where an activation's back-edge count is both complete and
    // still attributable. See `ProfileStore::add_loop_work`.
    if !thread.frames[*frame_idx].should_try_osr(entry_pc, osr_backedge_threshold) {
        return OsrBackoffOutcome::Skip;
    }
    // wire-tiered-manager Step 5 (precise background OSR): when the bg/tiered
    // pipeline is enabled, OSR compilation runs OFF the mutator. On a hot
    // back-edge (the per-frame `should_try_osr` schedule above is the throttle)
    // we enqueue an OSR task for the worker and KEEP INTERPRETING; we only enter
    // a worker-PUBLISHED `compiled_via_osr` artifact (the existing reuse path) —
    // never inline-compile here. Gated default-OFF: the historical inline OSR
    // below is byte-for-byte unchanged when `CRATONVM_BG_COMPILE` is unset.
    if crate::runtime::env_cache::bg_compile() {
        let (cn, mn, md, frame_class_id) = {
            let f = &thread.frames[*frame_idx];
            (
                f.class_name().to_string(),
                f.method_name().to_string(),
                f.method_descriptor().to_string(),
                f.class_id,
            )
        };
        // Three-way classification, not two. `reusable` is the enter-now case;
        // `published_but_unenterable` is a PUBLISHED OSR artifact that cannot be
        // entered at THIS pc, which is a permanent property, not a
        // still-compiling one (see the rejection arm below).
        let (reusable, published_but_unenterable) = {
            let jc = shared.jit.jit_cache.read();
            match jc.get_osr(&cn, &mn, &md, frame_class_id) {
                Some(c) if c.compiled_via_osr => {
                    let ok = c.can_osr_enter(entry_pc);
                    (ok, !ok)
                }
                _ => (false, false),
            }
        };
        if !reusable {
            // Not compiled yet: ensure the worker is running, request an OSR
            // compile (idempotent), and back off so we re-probe later rather
            // than spin. A subsequent hot back-edge finds the published
            // artifact and falls through to the reuse-enter below.
            let key = crate::jit::tiered::MethodKey::new(cn, mn, md);
            // An artifact this same path already published, which reports
            // `can_osr_enter(entry_pc) == false`, will report that forever:
            // `osr_pc_to_native[entry_pc]` is a pure function of the bytecode
            // and the entry pc (the codegen writes `-1` for a pc strictly
            // inside a LICM-hoisted loop body, whose pre-header an OSR entry
            // would skip). Recompiling reproduces it byte for byte — the OSR
            // path passes empty branch/unroll hint maps, so nothing profile-
            // dependent can change the outcome; a class redefinition, the one
            // thing that would, replaces the cached artifact and re-evaluates
            // this check naturally.
            //
            // Treating it as "background compile pending" instead —
            // `record_osr_background_pending()` resets `backward_count` without
            // consuming the bounded per-pc rejection budget — made the loop
            // re-request a compile every `osr_threshold` back-edges forever.
            // Measured on the `CallRate.allocPutOld` probe: 200 full C2
            // pipelines for 200 000 iterations (one per 1 000), 199 of them
            // producing the identical un-enterable artifact, with the loop
            // interpreted throughout. Route it to the existing bounded
            // exponential-backoff schedule instead, which is already keyed
            // per-pc, so other loop headers in the same method keep their OSR
            // eligibility (unlike `mark_osr_denied`, which is method-wide).
            //
            // Strictly a waste-elimination change: it removes compiles, never
            // adds compiled execution. The loop runs interpreted either way.
            if crate::jit::tiered::is_osr_denied(&key) || published_but_unenterable {
                // Counted, because this path is why `osr_entered=0` can appear
                // next to `osr_refused_entry=0` and a non-zero `osr=` compile
                // count — a combination that reads like "OSR was never even
                // tried" when in fact an artifact was built and found
                // un-enterable at this pc. `osr_refused_entry` is only recorded
                // inside `try_osr`, which this arm returns before reaching, so
                // without these two the whole OSR lifecycle line is silent about
                // the most common way OSR fails to happen.
                cratonvm_jit::metrics::record_osr_event(if published_but_unenterable {
                    "osr_published_but_unenterable"
                } else {
                    "osr_method_denied"
                });
                thread.frames[*frame_idx].record_osr_rejection(entry_pc);
                return OsrBackoffOutcome::Skip;
            }
            ensure_bg_compiler_started(shared);
            let _ = shared.jit.tiered_manager.request_osr(&key, entry_pc as u32);
            // Waiting for an off-thread compile is not a failed OSR attempt.
            // The old attempt counter reached its permanent cap within a few
            // thousand interpreted iterations, usually before the worker had
            // published, so the frame never probed the completed artifact.
            // Restart the stride instead: this polls at most once per threshold
            // while the task is queued. A real compile failure marks the method
            // OSR-denied below and returns to the bounded rejection schedule.
            thread.frames[*frame_idx].record_osr_background_pending();
            return OsrBackoffOutcome::Skip;
        }
        // `reusable`: fall through to `try_osr`, which reuses the published
        // artifact via its `osr_reused` fast path (no inline compile).
    }
    let osr_class_id = thread.frames[*frame_idx].class_id;
    // Out-channel for an exception the OSR'd body exited with (see
    // `OsrBackoffOutcome::ThrowJava`). `try_osr`'s return type has no error
    // arm, and widening it would touch every `return None` in a ~500-line
    // function; a single out-parameter written on exactly one path is the
    // minimal honest channel.
    let mut osr_throw: Option<ObjectRef> = None;
    // Sibling out-channel: the OSR'd body ran and advanced this frame, but
    // returned no value and threw nothing out — the RBC.6b lift's handler
    // entry. See `try_osr`'s parameter doc.
    let mut osr_committed = false;
    let osr_result = try_osr(
        shared,
        thread,
        *frame_idx,
        osr_class_id,
        entry_pc,
        &mut osr_throw,
        &mut osr_committed,
    );
    // Checked BEFORE the rejection bookkeeping below: the OSR'd body RAN (and
    // committed loop iterations), so this is not a rejected attempt and must
    // not consume the per-pc rejection budget.
    if let Some(exc) = osr_throw {
        return OsrBackoffOutcome::ThrowJava(exc);
    }
    // Same rule, same reason, for the path that ran and CAUGHT. `Skip`'s "fall
    // through to your post-back-edge work" is the right action here — the frame
    // is parked at a handler with the throwable on its stack, so the dispatch
    // loop resumes there (after its safepoint check) and re-enters the cached
    // artifact at the next hot back-edge. What must NOT happen is the rejection
    // bookkeeping below: charging a caught exception against the per-pc budget
    // retires OSR after five of them.
    if osr_committed {
        return OsrBackoffOutcome::Skip;
    }
    match osr_result {
        Some(osr_val) => {
            if *frame_idx > initial_frame_idx {
                pop_and_recycle_frame(shared, thread);
                *frame_idx -= 1;
                if let Some(value) = osr_val {
                    thread.frames[*frame_idx].stack.push_unchecked(value);
                }
                OsrBackoffOutcome::ContinueDispatch
            } else {
                OsrBackoffOutcome::ReturnOuter(osr_val)
            }
        }
        None => {
            thread.frames[*frame_idx].record_osr_rejection(entry_pc);
            OsrBackoffOutcome::Skip
        }
    }
}

// ---------------------------------------------------------------------------
// Main execution loop
// ---------------------------------------------------------------------------

/// Fetch (building on first use) the pre-decoded instruction stream for the
/// method this frame is executing.
///
/// Two tiers, both one-time-per-method:
///
/// * `Cached` frames read a `OnceLock` on the shared `CachedBytecodeMethod`
///   -- an acquire load after the first call.
/// * `Owned` frames (non-cached invoke paths, synthetic frames, JNI entry
///   stubs) go to the process-wide intern table, which is keyed on the
///   identity of the bytecode allocation so both tiers share one stream per
///   method.
///
/// `None` means the method is not quickenable and the caller must keep using
/// `Instruction::decode`.
#[inline]
fn quickened_for_frame(frame: &Frame) -> Option<Arc<cratonvm_reader::QuickenedCode>> {
    match frame.cached_method() {
        Some(cm) => cm
            .quickened
            .get_or_init(|| cratonvm_reader::quickened::intern(&cm.code))
            .clone(),
        None => cratonvm_reader::quickened::intern(&frame.code),
    }
}

fn execute_frame(shared: &SharedVm, thread: &mut JvmThread) -> MethodCallResult {
    let initial_frame_idx = thread.frames.len() - 1;
    execute_frame_from_index(shared, thread, initial_frame_idx)
}

/// How a fast-path invoke dispatch failure should re-enter the main loop.
enum FastPathInvokeError {
    /// Convert through `throw_runtime_error` at the top of the loop.
    Runtime(RuntimeError),
    /// Already a Java throwable — route it through the exception table.
    Java(ObjectRef),
    /// Not expressible as a Java throwable; unwind the whole invocation.
    Fatal(MethodCallFailed),
}

/// Classify an error returned by one of the stackless invoke fast paths
/// (`0xb6`/`0xb7`/`0xb8`/`0xb9`).
///
/// Those arms handle their own errors and `continue`, so they never reach the
/// per-opcode conversion that guards the slow path at the bottom of the loop —
/// they have to perform the same conversions themselves. `VmError::Linkage`
/// was the one they did not: every `java.lang.LinkageError` subclass is an
/// ordinary throwable (JVMS §5.4), but the fast paths returned it raw, which
/// skipped every exception handler in every frame and killed the process at
/// `main-vm run()`.
///
/// Concretely: `ClassLoader.defineClass1` rejecting a bad-magic class file is
/// reached by `invokestatic` from `ClassLoader.defineClass`, so
/// `assertThatExceptionOfType(ClassFormatError.class)` in
/// `RestartClassLoaderTests.getUpdatedClass` aborted the VM instead of
/// passing, and a `catch (Throwable)` wrapped directly around `defineClass`
/// never ran. `probes/DefineClassFormatErrorProbe.java` and
/// `probes/DefineClassWhereLostProbe.java` cover both shapes.
#[cold]
fn classify_fastpath_invoke_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    err: MethodCallFailed,
) -> FastPathInvokeError {
    if dbg_linkage() {
        if let MethodCallFailed::InternalError(VmError::Linkage(l)) = &err {
            dbg_linkage_dump(thread, "fast-path invoke", &format!("{l:?}"));
        }
    }
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(re)) => FastPathInvokeError::Runtime(re),
        MethodCallFailed::ExceptionThrown(exc) => FastPathInvokeError::Java(exc),
        MethodCallFailed::InternalError(VmError::Linkage(linkage_err)) => {
            match crate::runtime::exceptions::throw_linkage_error(shared, thread, linkage_err) {
                MethodCallFailed::ExceptionThrown(exc) => FastPathInvokeError::Java(exc),
                // The throwable could not be built (`throw_linkage_error` logs
                // why). Preserve the old behaviour rather than losing the error.
                other => FastPathInvokeError::Fatal(other),
            }
        }
        other => FastPathInvokeError::Fatal(other),
    }
}

/// Execute a previously frozen stack. `initial_frame_idx` is the oldest frame
/// owned by this invocation, while dispatch resumes at the current top frame.
fn execute_frame_from_index(
    shared: &SharedVm,
    thread: &mut JvmThread,
    initial_frame_idx: usize,
) -> MethodCallResult {
    let mut frame_idx = thread.frames.len() - 1;
    // AUDIT CRIT-3 fix: hoist the PGO-enabled atomic load ONCE per
    // execute_frame invocation.  Branch sites in the interpreter hot loop
    // (~13 of them) test this local instead of doing an atomic load + 2
    // `Arc<str>` clones (for make_method_key) every iteration.  Profiling
    // state observed at frame entry — the next `execute_frame` invocation
    // re-reads the global gate, so newly-enabled profiling picks up on the
    // next call rather than mid-loop.
    let pgo_enabled = crate::jit::profile::is_profiling_enabled();
    // T17.Δ.3 — hoist the JVMTI single-step listener gate out of the per-bytecode
    // loop (same contract as `pgo_enabled` above). `any_single_step_listener_active()`
    // is a `GLOBAL_MANAGER` OnceLock load + an AtomicBool load; reading it on EVERY
    // bytecode was pure overhead in the universal no-agent case. A single-step agent
    // that subscribes mid-method is observed on the next `execute_frame` entry
    // (call/return) — the accepted pgo-style tradeoff; per-thread step enablement is
    // still re-checked per-bytecode inside `fire_jvmti_single_step` when a listener
    // is active.
    let single_step_active = crate::runtime::jvmti::any_single_step_listener_active();
    // Hoisted for the same reason as `pgo_enabled` above: the `if_acmpne`
    // fast-path arm declines while the `CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE`
    // diagnostic is armed, so the decoded arm (which owns that instrument)
    // still runs. Reading the cached gate once per `execute_frame` keeps the
    // arm's admission test to a register compare. Arming it mid-method is
    // observed on the next call/return, the accepted pgo-style tradeoff.
    let acmp_identity_trace = crate::runtime::env_cache::active_profiles_identity_trace();
    // ── Back-edge poll word (2026-09-02) ─────────────────────────────────
    //
    // Every backward branch used to call `safepoint_check` UNCONDITIONALLY and
    // then `continue` — straight into the loop-top poll thirty lines below,
    // which asks `stw_requested` first and only calls `safepoint_check` if it
    // is set. So the back-edge call was redundant with the very next thing the
    // loop does, *except* for its tail: the async-exception drain, which is
    // the only work at a back edge that the loop top does not repeat.
    //
    // That tail was not cheap. `take_async_exception` goes through
    // `self_async_slot`: a thread-local `RefCell` borrow, an `Arc::clone`, a
    // `swap(0, AcqRel)` and an `Arc` drop — three locked read-modify-writes
    // per loop iteration, inside a function too large to inline (it carries
    // the memwatch and blocked-access debug hooks). Measured with
    // `probes/BackEdge.java` (two loops with identical total body-bytecode
    // counts and an 8x difference in back-edge count, so the per-back-edge
    // cost falls out of the difference): one interpreted backward branch cost
    // 33-37 ns against HotSpot's template interpreter at 4.9 ns.
    //
    // Hoisting the slot handle turns the back-edge question into two relaxed
    // loads and a predicted branch. See
    // `ThreadRegistry::self_async_slot_handle` for why observing registration
    // once per `execute_frame` entry is sound (same pgo-style tradeoff as
    // `pgo_enabled` and `single_step_active` above).
    let async_exception_slot = shared
        .threads
        .thread_registry
        .self_async_slot_handle(thread.thread_id);
    // The condition every back edge now tests before paying for
    // `safepoint_check`. Written as a macro rather than a closure because the
    // four call sites sit inside `&mut thread` borrows and a closure capturing
    // `shared`/`async_exception_slot` would still have to be called in a
    // position where `thread` is reborrowed.
    //
    // `CRATONVM_JIT_NO_BACKEDGE_POLL_GATE=1` (or `CRATONVM_JIT=-backedge-poll-
    // gate`) makes the macro answer `true` unconditionally, restoring the
    // unconditional call so the two arms can be priced inside one binary.
    //
    // ── The two diagnostics that must keep their sampling rate ──────────
    //
    // Skipping `safepoint_check` is equivalent to calling it only when the
    // call would have been a no-op, and there are exactly two ways it is not:
    // `memwatch::poll` and `blocked_access_debug`, both of which fire from
    // inside it and neither of which the loop-top poll reaches. A memwatch is
    // a *sampling* instrument — its own doc promises it "catches the
    // corrupting write within one safepoint window" — so silently cutting its
    // rate would weaken a diagnostic rather than speed anything up, and would
    // do it invisibly.
    //
    // Both are startup-static gates, so folding them in costs one more `or`
    // in the hoisted `bool` and nothing per back edge. An armed run keeps
    // exactly the old poll frequency; the universal unarmed run keeps the two
    // relaxed loads.
    let backedge_poll_gate_off = crate::runtime::env_cache::no_backedge_poll_gate()
        || crate::runtime::memwatch::is_watching()
        || cratonvm_gc::blocked_access_debug::enabled();
    // ── Quickened field / array arms (2026-09-02) ───────────────────────
    // `Some(heap)` iff this frame may take the `getfield` / `putfield` and
    // primitive `*aload` / `*astore` fast arms; see `field_fast` for the
    // contract and for what turns them off. Hoisted per `execute_frame`
    // entry like every other gate above (same pgo-style tradeoff).
    let fast_field_zgc = field_fast::fast_field_zgc(shared);
    // ── Invoke fast door admission (2026-09-02) ─────────────────────────
    // Off while anything the general dispatcher would have to observe per
    // call is armed: PGO (it records call sites and receivers), the invoke
    // traces, the frame trace, or a virtual thread. See
    // `execute_invokevirtual_fast_door`.
    let invoke_fast_door_on = !crate::runtime::env_cache::no_invoke_fast_door()
        && !pgo_enabled
        && !crate::runtime::env_cache::frame_trace()
        && !crate::runtime::env_cache::dbg_h2trace()
        && !crate::runtime::env_cache::dbg_loader_trace()
        && !crate::runtime::env_cache::dbg_gse()
        && !crate::runtime::env_cache::dbg_pbstart()
        && !matches!(thread.kind, crate::threading::ThreadKind::Virtual);
    // ── OSR call floor (2026-09-02) ─────────────────────────────────────
    // `try_osr_with_backoff` cannot do anything until `Frame::backward_count`
    // reaches the smallest threshold `Frame::should_try_osr` accepts (the
    // attempt backoff only raises it), so the four back-edge sites compare
    // the count against this floor inline and make the out-of-line call only
    // past it. A virtual thread or `CRATONVM_JIT_OSR=0` makes the call a
    // guaranteed no-op, so the floor is `u32::MAX`; the arrival trace
    // (`CRATONVM_DBG_OSR_FRAME_TRACE`) records every arrival inside the
    // call, so it keeps the floor at 0 and the old call rate.
    // `CRATONVM_JIT_NO_OSR_INLINE_GATE=1` restores the unconditional call.
    let osr_call_floor: u32 = if crate::runtime::env_cache::no_osr_inline_gate()
        || osr_frame_trace::enabled()
    {
        0
    } else if matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        || !crate::runtime::env_cache::osr_backedge_enabled()
    {
        u32::MAX
    } else {
        crate::runtime::env_cache::tier_osr_backedge().unwrap_or(OSR_THRESHOLD)
    };
    macro_rules! backedge_poll_needed {
        () => {
            backedge_poll_gate_off
                || shared
                    .mem
                    .gc_barrier
                    .stw_requested
                    .load(std::sync::atomic::Ordering::Acquire)
                || async_exception_slot
                    .as_ref()
                    .is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed) != 0)
        };
    }
    // When a fast-path bytecode needs to throw a RuntimeError (AIOOBE, NPE, etc.),
    // it sets this to Some(...) and breaks out of the fast-path match instead of
    // returning directly. The main loop then converts it to a catchable Java exception.
    let mut pending_runtime_error: Option<(RuntimeError, usize)> = None;
    // When an invoke handler receives an ExceptionThrown error (e.g. from JIT
    // dispatch), it stores the exception here instead of returning directly.
    // The main loop then routes it through the exception table for proper
    // try/catch handling.  The usize is the PC of the invoke instruction.
    let mut pending_java_exception: Option<(ObjectRef, usize)> = None;
    // T19.H1 — per-invocation guard so an `execute()` frame dumps its
    // stack at most once when the watchdog flag is sticky-true.
    let mut stack_dump_emitted = false;
    // --- Quickening (bytecode pre-decode) state, valid for the frame whose
    // bytecode allocation lives at `quick_code_ptr`.
    //
    // `quick` is an owned `Arc` deliberately: the dispatch below borrows an
    // `&Instruction` out of it and hands that borrow to `execute_instruction`,
    // which takes `&mut thread`. Keeping the stream in a loop-local (rather
    // than reading it back out of `thread.frames[..]` every iteration) is what
    // makes those two borrows disjoint.
    //
    // The pointer key is sound because a live `QuickenedCode` holds a strong
    // `Arc<[u8]>` to the very bytecode it was built from, so this address
    // cannot be freed and recycled by a different method while `quick` is
    // `Some`. When `quick` is `None` a recycled address can only mislabel
    // another method as un-quickened -- a pessimisation, never a miscompare.
    // ARCH-2026-08-04 A4b — hoisted out of the dispatch loop.
    //
    // `VmConfig` is immutable after `SharedVm::new`: `with_skip_verification` /
    // `with_xverify_mode` are `mut self -> Self` builders that run before
    // construction, so this is loop-invariant for the whole `execute()` call
    // (indeed for the process). It was re-read once per bytecode as a
    // `shared -> config -> bool` pointer chase.
    //
    // What it selects, and why it is a whole-mode switch rather than a
    // per-opcode one: the fast-path local-access handlers (`lload`/`dload`,
    // `istore`/`fstore`, `astore`, `lstore`/`dstore`, and the
    // `get_local_compact_unchecked` / `set_local_compact_unchecked` helpers the
    // `iload`/`iadd` fusions use) index `frame.locals` with the raw bytecode
    // operand, relying on the verifier having proven `operand < max_locals`.
    // Under `-noverify` / `-Xverify:none` that proof is gone. Note the indexing
    // is ordinary Rust `Vec` indexing, so the failure is a *panic*, not memory
    // unsafety — but a panic is still not an acceptable answer to a valid
    // command line, hence the fallback to the bounds-checked decoded path.
    //
    // That fallback is what makes `--noverify` select a different
    // implementation for all 122 fast-path arms at once, which is why
    // `difftest`'s `interp-decoded` axis has to run (it did not until A4b; see
    // `difftest/src/main.rs`).
    let use_fast_path = !shared.config.skip_verification;
    let mut quick: Option<Arc<cratonvm_reader::QuickenedCode>> = None;
    let mut quick_code_ptr: *const u8 = std::ptr::null();
    // (The former `quick_hint` local is gone: `QuickenedCode` now resolves any
    // pc in O(1) via an instruction-start bitmap plus per-block popcount, so
    // the fall-through hint only bought one popcount on the fall-through path
    // at the cost of a compare on every branch, back-edge, handler entry and
    // switch target. `resolve()` is the hint-free form.)
    // ── Conditional-branch arm, written once ──────────────────────────────
    //
    // Each of the interpreter's conditional-branch fast-path arms is the same
    // twenty lines around a one-line predicate: record the branch outcome for
    // PGO, take the branch or fall through, and — when the target is BACKWARD
    // — record the back edge, bump `Frame::backward_count`, and offer the
    // frame to `try_osr_with_backoff`.
    //
    // It is written once here because the copied form had already lost a whole
    // opcode family. `ifnull` (0xc6), `ifnonnull` (0xc7), `if_acmpeq` (0xa5)
    // and `if_acmpne` (0xa6) had no fast-path arm at all, so they fell through
    // to the decoded handler in `opcodes.rs` — which sets `frame.pc` and
    // nothing else. A loop closed by one of those four therefore:
    //
    //   * never recorded a PGO back edge,
    //   * never incremented `Frame::backward_count`, so it could not reach the
    //     `OSR_THRESHOLD` and could never enter an OSR-compiled body, and
    //   * never earned whole-method tier-up credit either, because
    //     `pop_and_recycle_frame_with_reason` feeds `ProfileStore::add_loop_work`
    //     from that same counter.
    //
    // That is the ordinary shape of `do { … } while (p != null)`, of
    // `do { … } while (o != sentinel)`, and of every `for`/`while` emitted by a
    // frontend that puts the loop test at the BOTTOM (ECJ, and the Kotlin and
    // Scala backends) rather than at the top with a closing `goto` the way
    // javac does. The comment on `try_osr_with_backoff` calls itself "the one
    // funnel all fourteen back-edge sites go through" — fourteen was the count
    // of arms that had been written, not of branches that can close a loop.
    //
    // `frame`, `saved_pc`, `b1` and `b2` are parameters rather than captures:
    // `macro_rules!` gives local-variable identifiers definition-site hygiene
    // and those four are bound inside the dispatch loop, below this point.
    // Everything else the body touches (`shared`, `thread`, `frame_idx`,
    // `initial_frame_idx`, `pgo_enabled`, `pending_java_exception`) is already
    // in scope here.
    macro_rules! cond_branch_arm {
        ($frame:expr, $saved_pc:expr, $b1:expr, $b2:expr, $taken:expr) => {{
            let taken = $taken;
            if pgo_enabled {
                let (cid, mn, md) = method_key_parts($frame);
                shared
                    .jit
                    .profile_store
                    .record_branch_borrowed(cid, mn, md, $saved_pc, taken);
            }
            if taken {
                // Cast: bytecode operand decoding
                let offset = (($b1 as i16) << 8) | ($b2 as i16);
                // Cast: signed branch offset arithmetic
                $frame.pc = ($saved_pc as isize + offset as isize) as usize;
                if offset < 0 {
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts($frame);
                        shared
                            .jit
                            .profile_store
                            .record_backedge_borrowed(cid, mn, md, $saved_pc);
                    }
                    $frame.backward_count += 1;

                    let entry_pc = $frame.pc;
                    let osr_due = $frame.backward_count >= osr_call_floor;
                    let _ = $frame;
                    if osr_due {
                    match try_osr_with_backoff(
                        shared,
                        thread,
                        &mut frame_idx,
                        initial_frame_idx,
                        entry_pc,
                    ) {
                        OsrBackoffOutcome::ReturnOuter(v) => return Ok(v),
                        OsrBackoffOutcome::ContinueDispatch => continue,
                        OsrBackoffOutcome::ThrowJava(exc) => {
                            pending_java_exception = Some((exc, OSR_FRAME_DECLINED_TO_CATCH));
                            continue;
                        }
                        OsrBackoffOutcome::Skip => {}
                    }
                    }
                    if backedge_poll_needed!() {
                        safepoint_check(shared, thread);
                    }
                }
            } else {
                $frame.pc = $saved_pc + 3;
            }
            continue;
        }};
    }

    loop {
        // Route callee-thrown Java exceptions before the safepoint poll below.
        // `pending_java_exception` is only a Rust local between the callee's
        // return and this block; it is not present in any GC-scanned frame slot
        // yet. Polling first can let STW reclaim the Throwable before a caller
        // catch handler stores it.
        if let Some((exc, invoke_pc)) = pending_java_exception.take() {
            // The handler walk and its GC pin live in
            // `exception_dispatch::unwind_to_handler` — shared with the
            // `pending_runtime_error` arm below, which used to carry a
            // byte-identical copy (ARCH-2026-08-04 A4a).
            unwind_to_handler(
                shared,
                thread,
                &mut frame_idx,
                initial_frame_idx,
                exc,
                invoke_pc,
            )?;
            continue;
        }

        // T19.H1 — opportunistic stack-dump hook.
        //
        // When the CLI watchdog fires (`--stack-dump-on-timeout=N`), it sets
        // `shared.debug.stack_dump_requested`. Every interpreter thread observes
        // the flag on its next dispatch iteration and self-dumps its frame
        // chain before the watchdog aborts the process. The load is
        // `Ordering::Relaxed` — a single predicted branch per bytecode in
        // the common case (flag always false).
        //
        // Under `--stack-sample-ms` the same hook is driven as a periodic
        // SAMPLER: the per-`execute()` latch is bypassed and the request is
        // consumed here, so the sampler thread's next re-arm produces the
        // next sample. That distinction is the whole point — with the latch
        // in place this hook fires once per nested interpreter entry, and the
        // resulting "profile" ranks methods by call count rather than by time
        // (a cheap method entered 100k times outranks the one that actually
        // burned the wall clock).
        //
        // What the sample POSITION means, because three profiles on
        // docs/known-issues/perf/interpreted-invoke-cost-350ns-20260825.md
        // were read wrong: this hook is the first thing a loop iteration does,
        // and an invoke pushes the callee frame and `continue`s. So the time
        // an expensive INVOKE burns is reported against the callee at
        // `pc=0 last_pc=0` — a frame that has executed nothing. Anyone
        // aggregating leaf frames by method name files invoke cost under the
        // callee's name, where it reads as a slow body. Bucket
        // `pc == 0 && last_pc == 0` separately.
        // `probes/InvokeAttributionProbe.java` is the calibration: a
        // three-bytecode callee behind an `invokevirtual` takes 54% of the
        // samples at its entry and one sample anywhere in its body, and that
        // share tracks the separately-timed invoke delta (290-417 ns).
        if (!stack_dump_emitted || shared.stack_sample_mode()) && shared.stack_dump_pending() {
            shared.dump_current_thread_frames(thread);
            if shared.stack_sample_mode() {
                shared.clear_stack_dump_request();
            } else {
                stack_dump_emitted = true;
            }
            // Don't park or sleep here — the watchdog aborts the process
            // after a short grace period, and if it doesn't (e.g. crashed
            // mid-way) we'd rather keep running than hang forever. The
            // `stack_dump_emitted` guard ensures we dump at most once per
            // nested `execute()` call so ack counts remain meaningful.
        }

        // T19.H7 diag — opcode counter. Removed; documented findings in
        // history/roadmap-100.md T19.H7 section. Last localization:
        // `org/jboss/modules/Main.main` pc=1306 dispatched, then a native
        // call from that opcode never returns (interpreter loop never
        // re-entered).
        // Feature-gated (off by default) — see vm/Cargo.toml
        // (See above — diagnostic block removed.)

        // A newly-started or long straight-line frame may not hit an allocation
        // or backward-branch poll before another thread requests STW. Keep the
        // hot path to one atomic load; call the full safepoint machinery only
        // while a pause is actually active.
        if shared
            .mem
            .gc_barrier
            .stw_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            safepoint_check(shared, thread);
        }

        // Handle any pending runtime error from the previous iteration's fast path.
        if let Some((re, invoke_pc)) = pending_runtime_error.take() {
            if aioobe2_dbg() {
                if let RuntimeError::ArrayIndexOutOfBoundsException { index, message } = &re {
                    let f = &thread.frames[frame_idx];
                    eprintln!(
                        "[AIOOBE2] index={} message={} class={} method={}{} pc={}",
                        index,
                        message.as_deref().unwrap_or("<none>"),
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor(),
                        invoke_pc
                    );
                }
            }
            // Normalize the operand-stack overflow BEFORE throwing, exactly as
            // the decoded path's conversion does further down.
            //
            // That site calls itself "the only point that converts runtime
            // errors into Java exceptions". It is not, and has not been for as
            // long as the invoke fast paths have routed through here: this arm
            // converts too. `ValueStack` reports an overflow as
            // `NotImplemented { feature: "operand stack overflow" }`, which
            // `throw_runtime_error` maps to an *uncatchable* internal error, so
            // a stack overflow arriving through a fast-path arm hard-unwound
            // the whole call stack instead of surfacing as a catchable
            // `java.lang.StackOverflowError` that an in-method
            // `catch (StackOverflowError)` / `catch (Throwable)` can observe.
            // Two conversion points that disagree is one conversion point too
            // many; until they are merged they must at least agree.
            let re = match re {
                RuntimeError::NotImplemented { feature } if feature == "operand stack overflow" => {
                    RuntimeError::StackOverflowError
                }
                other => other,
            };
            let exc_result = super::exceptions::throw_runtime_error(shared, thread, re);
            match exc_result {
                MethodCallFailed::ExceptionThrown(exc) => {
                    // Same walk as the `pending_java_exception` arm above, and
                    // now literally the same code (ARCH-2026-08-04 A4a). The
                    // two copies had already drifted in comments only, but the
                    // GC pin they share is subtle enough that a fix landing in
                    // one and not the other is a use-after-free reproducing on
                    // just one of the two throw paths.
                    unwind_to_handler(
                        shared,
                        thread,
                        &mut frame_idx,
                        initial_frame_idx,
                        exc,
                        invoke_pc,
                    )?;
                    continue;
                }
                other => return Err(other),
            }
        }

        // ── Frame-pointer hoist, preamble (frame-arena.md §6.1) ──────────
        // `FrameStack` gives frames stable addresses, so one address
        // computation can serve the whole per-bytecode preamble instead of
        // six bounds-checked `imul`-by-`size_of::<Frame>()` re-indexes. The
        // region this covers runs from here to the `code_ptr`/`b2` reads just
        // below and contains no push, no pop and no `&mut` reborrow of
        // `thread.frames`; `hot_fp` is dead by the time the fast-path match
        // takes its real `&mut thread.frames[frame_idx]` borrow (which is
        // deliberately left as a borrow-checked reference, so the arms' `let
        // _ = frame;` discipline before calling back into `thread` keeps its
        // compile-time enforcement). Null iff `frame_idx >= len()`, which
        // routes through `VmError` where indexing would have panicked.
        let hot_fp: *mut Frame = thread.frames.frame_ptr(frame_idx);
        if hot_fp.is_null() {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!(
                    "frame index {frame_idx} out of range (depth {})",
                    thread.frames.len()
                ),
            }));
        }
        // SAFETY: `hot_fp` is non-null and in bounds (checked above); see the
        // hoist note for the aliasing argument covering every deref below.
        let saved_pc = unsafe { (*hot_fp).pc };
        unsafe { (*hot_fp).last_instr_pc = saved_pc };

        // T17.Δ.3 — JVMTI single-step dispatch hook, gated by the hoisted
        // `single_step_active` so the no-agent case (universal) does zero atomics
        // and never materializes the frame borrow here. `fire_jvmti_single_step`
        // re-checks the listener + per-thread step flag when a listener is active.
        if single_step_active {
            // Deliberately still re-indexed: this call takes `&JvmThread`,
            // which exposes the whole frame stack, so hoist rule 1 forbids
            // routing it through `hot_fp`. Cold and listener-gated — it costs
            // nothing in the universal no-agent case.
            fire_jvmti_single_step(
                shared.vm_identity,
                thread,
                &thread.frames[frame_idx],
                saved_pc,
            );
        }

        // --- Fast path: handle hot bytecodes directly from raw bytes ---
        // Pre-read opcode + 2 operand bytes to avoid borrow conflicts with frame.
        // Bytecode is padded with 2 trailing zero bytes, so pc+1 and pc+2 are always
        // safe to read when pc is within the original (unpadded) code region.
        // The raw-bytecode handlers are the common interpreter path for every
        // verified class. Package names are not execution-policy inputs: the
        // old java/jdk/sun/Spring deny-list made identical bytecode select a
        // second implementation with different semantics and ~2.7x lower
        // throughput. Unsupported opcodes and guarded edge cases still fall
        // through to the shared decoded handler below.
        //
        // H7: `use_fast_path` is hoisted above the loop (ARCH-2026-08-04 A4b)
        // — see the note at its binding for what it selects and why the
        // per-class `skip_verification` case still relies on the per-site
        // checks below as a second line of defence.
        // SAFETY (every `hot_fp` deref below): see the hoist note above —
        // reads only, no push, no `&mut` reborrow of the stack in between.
        // Explicit `&` on the place expression: calling `.len()` directly on
        // `(*hot_fp).code` autorefs through the raw pointer, which the
        // `dangerous_implicit_autorefs` lint denies. The borrow is confined to
        // this statement, so it cannot alias a later `&mut Frame`.
        // SAFETY: `hot_fp` addresses the live executing frame and is not
        // invalidated in this push-free region; see the autoref note above.
        let padded_code_len = unsafe { (&(*hot_fp).code).len() };
        debug_assert!(
            padded_code_len >= 2,
            "bytecode must be padded with at least 2 trailing bytes"
        );
        let code_len = padded_code_len.wrapping_sub(2); // original unpadded length
        if use_fast_path && saved_pc < code_len {
            // SAFETY: same live frame as the `padded_code_len` read above; the
            // pointer is only read within this push-free region.
            let code_ptr = unsafe { (*hot_fp).code.as_ptr() };
            // SAFETY: code_ptr points to the method's bytecode array; saved_pc is bounds-checked against code_len above, and the bytecode is padded with 2 trailing bytes.
            let opcode = unsafe { *code_ptr.add(saved_pc) };
            let b1 = unsafe { *code_ptr.add(saved_pc + 1) };
            let b2 = unsafe { *code_ptr.add(saved_pc + 2) };
            let frame = &mut thread.frames[frame_idx];
            match opcode {
                // iload_0..3 with superinstruction look-ahead
                0x1a | 0x1b | 0x1c | 0x1d => {
                    let local_idx = (opcode - 0x1a) as usize; // Cast: bytecode operand decoding
                                                              // Peek at the next opcode for superinstruction fusion.
                                                              // b1 is already pre-read as the byte at saved_pc+1.
                                                              // Check if the next byte is another iload_N (for two-operand fusions).
                    if b1 >= 0x1a && b1 <= 0x1d && saved_pc + 2 < code_len {
                        let local_y = (b1 - 0x1a) as usize; // Widening: index conversion
                                                            // SAFETY: code_ptr points to the method's bytecode array; saved_pc + 2 < code_len is checked above.
                        let next2 = unsafe { *code_ptr.add(saved_pc + 2) };
                        // iload_X; iload_Y; iadd → push locals[X] + locals[Y]
                        // Round-3: typed int helpers avoid the Value enum round-trip
                        // when both locals are already int-tagged.
                        if next2 == 0x60 {
                            let cvx = frame.get_local_compact_unchecked(local_idx);
                            let cvy = frame.get_local_compact_unchecked(local_y);
                            if let (Some(vx), Some(vy)) = (cvx.as_int(), cvy.as_int()) {
                                frame.stack.push_int_unchecked(vx.wrapping_add(vy));
                                frame.pc = saved_pc + 3;
                                continue;
                            }
                        }
                        // iload_X; iload_Y; if_icmplt offset → compare and branch
                        if next2 == 0xa1 && saved_pc + 4 < code_len {
                            if let (Value::Int(vx), Value::Int(vy)) = (
                                frame.get_local_unchecked(local_idx),
                                frame.get_local_unchecked(local_y),
                            ) {
                                // SAFETY: code_ptr points to the method's bytecode array; saved_pc + 4 < code_len is checked above.
                                let ob1 = unsafe { *code_ptr.add(saved_pc + 3) };
                                let ob2 = unsafe { *code_ptr.add(saved_pc + 4) };
                                let taken = vx < vy;
                                if pgo_enabled {
                                    let (cid, mn, md) = method_key_parts(frame);
                                    shared.jit.profile_store.record_branch_borrowed(
                                        cid,
                                        mn,
                                        md,
                                        saved_pc + 2, // profile at the if_icmplt pc
                                        taken,
                                    );
                                }
                                if taken {
                                    let offset = ((ob1 as i16) << 8) | (ob2 as i16); // Cast: bytecode operand decoding
                                                                                     // Cast: signed branch offset arithmetic
                                    frame.pc = ((saved_pc + 2) as isize + offset as isize) as usize;
                                    if offset < 0 {
                                        if pgo_enabled {
                                            let (cid, mn, md) = method_key_parts(frame);
                                            shared.jit.profile_store.record_backedge_borrowed(
                                                cid,
                                                mn,
                                                md,
                                                saved_pc + 2,
                                            );
                                        }
                                        frame.backward_count += 1;

                                        let entry_pc = frame.pc;
                                        let osr_due = frame.backward_count >= osr_call_floor;
                                        let _ = frame;
                                        if osr_due {
                                        match try_osr_with_backoff(
                                            shared,
                                            thread,
                                            &mut frame_idx,
                                            initial_frame_idx,
                                            entry_pc,
                                        ) {
                                            OsrBackoffOutcome::ReturnOuter(v) => return Ok(v),
                                            OsrBackoffOutcome::ContinueDispatch => continue,
                                            OsrBackoffOutcome::ThrowJava(exc) => {
                                                pending_java_exception =
                                                    Some((exc, OSR_FRAME_DECLINED_TO_CATCH));
                                                continue;
                                            }
                                            OsrBackoffOutcome::Skip => {}
                                        }
                                        }
                                        if backedge_poll_needed!() {
                                            safepoint_check(shared, thread);
                                        }
                                    }
                                } else {
                                    frame.pc = saved_pc + 5; // skip iload_X + iload_Y + if_icmplt(3)
                                }
                                continue;
                            }
                        }
                    }
                    // iload_X; iconst_1; iadd; istore_X → locals[X] += 1
                    // Round-3: typed int helpers — same wrapping_add, no Value enum.
                    if b1 == 0x04 && saved_pc + 3 < code_len {
                        // SAFETY: code_ptr points to the method's bytecode array; saved_pc + 3 < code_len is checked above.
                        let next2 = unsafe { *code_ptr.add(saved_pc + 2) };
                        let next3 = unsafe { *code_ptr.add(saved_pc + 3) };
                        // istore_0..3 opcodes are 0x3b..0x3e
                        if next2 == 0x60
                            && next3 >= 0x3b
                            && next3 <= 0x3e
                            // Widening: small integer index -> usize (non-negative, fits in pointer width)
                            && (next3 - 0x3b) as usize == local_idx
                        // Cast: bytecode operand decoding
                        {
                            let cv = frame.get_local_compact_unchecked(local_idx);
                            if let Some(v) = cv.as_int() {
                                frame.set_local_int_unchecked(local_idx, v.wrapping_add(1));
                                frame.pc = saved_pc + 4;
                                continue;
                            }
                        }
                    }
                    // iload_X; arraylength → get array length directly
                    if b1 == 0xbe {
                        let arr_val = frame.get_local_unchecked(local_idx);
                        if let Value::Object(Some(arr_ref)) = arr_val {
                            let len = shared.mem.heap.array_length(arr_ref);
                            // JVM spec: arraylength returns i32; array length bounded by Integer.MAX_VALUE
                            frame.stack.push_int_unchecked(len as i32);
                            frame.pc = saved_pc + 2;
                            continue;
                        }
                    }
                    // No superinstruction matched — fall back to plain iload.
                    // Round-3: push the raw CompactValue (no Value round-trip).
                    frame
                        .stack
                        .push_compact(frame.get_local_compact_unchecked(local_idx));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // istore_0..3 — Round-3: pop the raw CompactValue and stash directly.
                // No Value enum round-trip; tag is preserved verbatim from the stack slot.
                0x3b => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(0, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x3c => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(1, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x3d => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(2, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x3e => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(3, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // aload_0..3 — Round-3: branch on compact tag.
                //
                // Object / Null / Uninitialized locals are pushed verbatim with
                // zero validation overhead.  Only NaN-tagged Long slots
                // (SUB_LONG_LO/HI) — the legacy JNI smuggled-jobject path —
                // need `coerce_value_for_return_validated` to reject pointer-
                // shaped longs that aren't heap-mapped (Letsgo AV family).
                // Untagged Double-shaped slots are not reference candidates
                // here; aload is a single-slot reference load by spec.
                0x2a => {
                    let cv = frame.get_local_compact_unchecked(0);
                    if cv.tag() == CompactTag::Long {
                        let v = coerce_value_for_return_validated(shared, cv.to_value(), b'L');
                        frame.stack.push_unchecked(v);
                    } else {
                        frame.stack.push_compact(cv);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x2b => {
                    let cv = frame.get_local_compact_unchecked(1);
                    if cv.tag() == CompactTag::Long {
                        let v = coerce_value_for_return_validated(shared, cv.to_value(), b'L');
                        frame.stack.push_unchecked(v);
                    } else {
                        frame.stack.push_compact(cv);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x2c => {
                    let cv = frame.get_local_compact_unchecked(2);
                    if cv.tag() == CompactTag::Long {
                        let v = coerce_value_for_return_validated(shared, cv.to_value(), b'L');
                        frame.stack.push_unchecked(v);
                    } else {
                        frame.stack.push_compact(cv);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x2d => {
                    let cv = frame.get_local_compact_unchecked(3);
                    if cv.tag() == CompactTag::Long {
                        let v = coerce_value_for_return_validated(shared, cv.to_value(), b'L');
                        frame.stack.push_unchecked(v);
                    } else {
                        frame.stack.push_compact(cv);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // astore_0..3 — Round-3: branch on compact tag.
                //
                // Only Long-tagged slots flow through `coerce_value_for_return_validated`
                // (the JNI smuggled-jobject path that GCs the underlying handle —
                // Letsgo AV after `ConfigurationClassEnhancer.enhance`).
                // Object, Null, Int, Float, Double, Uninitialized, ReturnAddress
                // are stashed directly with no decoding.
                0x4b => {
                    let cv = frame.stack.pop_compact();
                    if cv.tag() == CompactTag::Long {
                        let v = coerce_value_for_return_validated(shared, cv.to_value(), b'L');
                        frame.set_local_unchecked(0, v);
                    } else {
                        frame.set_local_compact_unchecked(0, cv);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x4c => {
                    let cv = frame.stack.pop_compact();
                    if cv.tag() == CompactTag::Long {
                        let v = coerce_value_for_return_validated(shared, cv.to_value(), b'L');
                        frame.set_local_unchecked(1, v);
                    } else {
                        frame.set_local_compact_unchecked(1, cv);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x4d => {
                    let cv = frame.stack.pop_compact();
                    if cv.tag() == CompactTag::Long {
                        let v = coerce_value_for_return_validated(shared, cv.to_value(), b'L');
                        frame.set_local_unchecked(2, v);
                    } else {
                        frame.set_local_compact_unchecked(2, cv);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x4e => {
                    let cv = frame.stack.pop_compact();
                    if cv.tag() == CompactTag::Long {
                        let v = coerce_value_for_return_validated(shared, cv.to_value(), b'L');
                        frame.set_local_unchecked(3, v);
                    } else {
                        frame.set_local_compact_unchecked(3, cv);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // iadd — AUDIT CRIT-4: int-typed pop/push, no Value enum round-trip.
                0x60 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va.wrapping_add(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // isub
                0x64 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va.wrapping_sub(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // imul
                0x68 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va.wrapping_mul(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // idiv — needs to fall back to slow path on division by zero
                // so the spec-mandated ArithmeticException is thrown.  Push
                // raw bits back onto the stack and break out of the fast match.
                0x6c => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    if vb != 0 {
                        frame.stack.push_int_unchecked(va.wrapping_div(vb));
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    // Restore stack for the slow path.
                    frame.stack.push_int_unchecked(va);
                    frame.stack.push_int_unchecked(vb);
                }
                // irem — same fallback policy as idiv.
                0x70 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    if vb != 0 {
                        frame.stack.push_int_unchecked(va.wrapping_rem(vb));
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    frame.stack.push_int_unchecked(va);
                    frame.stack.push_int_unchecked(vb);
                }
                // iconst_m1..5 — AUDIT CRIT-4: direct CompactValue push, no Value enum.
                0x02 => {
                    frame.stack.push_int_unchecked(-1);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x03 => {
                    frame.stack.push_int_unchecked(0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x04 => {
                    frame.stack.push_int_unchecked(1);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x05 => {
                    frame.stack.push_int_unchecked(2);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x06 => {
                    frame.stack.push_int_unchecked(3);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x07 => {
                    frame.stack.push_int_unchecked(4);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x08 => {
                    frame.stack.push_int_unchecked(5);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // bipush — AUDIT CRIT-4: int-typed push.
                0x10 => {
                    let val = b1 as i8 as i32; // Cast: bytecode operand decoding
                    frame.stack.push_int_unchecked(val);
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // sipush — AUDIT CRIT-4: int-typed push.
                0x11 => {
                    let val = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                    frame.stack.push_int_unchecked(val as i32); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 3;
                    continue;
                }
                // iinc — Round-3: typed int read/write, no Value enum.
                //
                // `get_local_int_unchecked` returns 0 for non-Int slots (mirrors
                // the prior `Value::Int(_)` match — falling through to the slow
                // path on type-mismatch is unnecessary because verified
                // bytecode guarantees Int at this site).
                // iinc — the local index is the raw bytecode operand and the
                // accessors below index `frame.locals` without a bounds check,
                // so this arm needs the same `b1 < max_locals` admission test
                // its `iload`/`istore`/`astore` neighbours carry. Without it a
                // per-class `skip_verification` class (which the global
                // `use_fast_path` gate does NOT cover) reaches
                // `set_local_int_unchecked` with an out-of-range index and
                // PANICS on the `Vec` index — the one outcome this module's
                // zero-panic gate exists to prevent. Out of range falls through
                // to the bounds-checked decoded handler, unchanged.
                0x84 if (b1 as usize) < frame.max_locals as usize => {
                    let idx = b1 as usize; // Cast: bytecode operand decoding
                    let inc = b2 as i8 as i32; // Cast: bytecode operand decoding
                    let v = frame.get_local_int_unchecked(idx);
                    frame.set_local_int_unchecked(idx, v.wrapping_add(inc));
                    frame.pc = saved_pc + 3;
                    continue;
                }
                // goto
                0xa7 => {
                    let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                    frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                    if offset < 0 {
                        // Backward branch — record back-edge for PGO loop trip profiling
                        if pgo_enabled {
                            let (cid, mn, md) = method_key_parts(frame);
                            shared
                                .jit
                                .profile_store
                                .record_backedge_borrowed(cid, mn, md, saved_pc);
                        }
                        // Backward branch — increment OSR counter
                        frame.backward_count += 1;

                        let entry_pc = frame.pc;
                        let osr_due = frame.backward_count >= osr_call_floor;
                        let _ = frame; // drop borrow before try_osr
                        if osr_due {
                        match try_osr_with_backoff(
                            shared,
                            thread,
                            &mut frame_idx,
                            initial_frame_idx,
                            entry_pc,
                        ) {
                            OsrBackoffOutcome::ReturnOuter(v) => return Ok(v),
                            OsrBackoffOutcome::ContinueDispatch => continue,
                            OsrBackoffOutcome::ThrowJava(exc) => {
                                pending_java_exception = Some((exc, OSR_FRAME_DECLINED_TO_CATCH));
                                continue;
                            }
                            OsrBackoffOutcome::Skip => {}
                        }
                        }
                        if backedge_poll_needed!() {
                            safepoint_check(shared, thread);
                        }
                    }
                    continue;
                }
                // if_icmpge — AUDIT CRIT-4: int-typed pop, no Value enum match.
                0xa2 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va >= vb);
                }
                // if_icmplt
                0xa1 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va < vb);
                }
                // if_icmple
                0xa4 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va <= vb);
                }
                // if_icmpgt
                0xa3 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va > vb);
                }
                // if_icmpne
                0xa0 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va != vb);
                }
                // if_icmpeq
                0x9f => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va == vb);
                }
                // ireturn / lreturn / freturn / dreturn / areturn
                0xac..=0xb0 => {
                    // Calibration: two adjacent reads measuring nothing, so the
                    // other phases can be corrected for rdtsc latency instead of
                    // compared against an assumed one.
                    {
                        let c0 = crate::runtime::interpreter::invoke_phases::now();
                        let c1 = crate::runtime::interpreter::invoke_phases::now();
                        crate::runtime::interpreter::invoke_phases::charge(
                            crate::runtime::interpreter::invoke_phases::P_CALIB,
                            c0,
                            c1,
                        );
                    }
                    let ph_r0 = crate::runtime::interpreter::invoke_phases::now();
                    // Bit-exact return-value transfer. The prior
                    // `pop_unchecked()` + `push_unchecked()` round-tripped the
                    // slot through `to_value()`/`from_value()`, which decoded a
                    // category-2 long whose NaN-box bit pattern collides with a
                    // tagged sub-tag (e.g. `lreturn` of `0xFFFC_…`, a BC safegcd
                    // `Mod.updateDE30`/`updateFG30` accumulator) as `Value::Int`,
                    // dropping the high bits. Copy the raw CompactValue for
                    // i/l/f/d-return; areturn still normalizes jobject-as-Long
                    // handles via `coerce_value_for_return`. See
                    // gaps/bc-ec-mod-mododdinverse-investigation.md.
                    //
                    // Underflow guard: an empty operand stack at a value
                    // return means earlier execution desynced the stack
                    // (valid bytecode can't reach here empty). The unchecked
                    // pop would wrap `len` to usize::MAX and PANIC the whole
                    // VM (observed: Gradle ProjectBuilder classes in the
                    // Spring Boot buildSrc suite; kafka bug-03 is the same
                    // family). Surface a diagnosable error instead.
                    if frame.stack.is_empty() {
                        let mname = frame.method_name().to_string();
                        let mdesc = frame.method_descriptor().to_string();
                        let cname = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(frame.class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        eprintln!(
                            "[cratonvm] operand-stack underflow at value return: {}.{}{} pc={} opcode=0x{:x}",
                            cname, mname, mdesc, saved_pc, opcode
                        );
                        return Err(MethodCallFailed::InternalError(VmError::Internal {
                            message: format!(
                                "operand-stack underflow at value return in {cname}.{mname}{mdesc} pc={saved_pc}"
                            ),
                        }));
                    }
                    let (cv, kind) = frame.stack.pop_with_kind_unchecked();
                    let desc_byte = match opcode {
                        0xad => b'J', // lreturn
                        0xae => b'F', // freturn
                        0xaf => b'D', // dreturn
                        0xb0 => b'L', // areturn
                        _ => b'I',    // ireturn (also B/C/S/Z)
                    };
                    // areturn (0xb0): mirror slow-path `Instruction::Areturn` — JNI
                    // / bridges can leave a jobject as `Value::Long` on the stack;
                    // fast-path frames (non-JDK packages) must still normalize
                    // before pushing to the caller or returning from the outer VM.
                    // lreturn (0xad): a KIND_LONG slot is read bit-exact so a
                    // collision-shaped long return keeps its high bits.
                    let value = if opcode == 0xb0 {
                        let ret = frame.return_tag();
                        coerce_value_for_return_validated(shared, cv.to_value(), ret)
                    } else {
                        decode_arg_kind_aware(cv, kind, desc_byte)
                    };
                    if crate::runtime::env_cache::trace_sb_filter() {
                        let cn = frame.class_name();
                        let mn = frame.method_name();
                        let interesting = (cn.contains("FilteringSpringBootCondition")
                            && mn == "match")
                            || (cn.contains("ImportCandidates")
                                && (mn == "getCandidates"
                                    || mn == "readCandidateConfigurations"
                                    || mn == "load"
                                    || mn == "stripComment"))
                            || (cn.contains("AutoConfigurationImportSelector")
                                && (mn == "removeDuplicates"
                                    || mn == "getCandidateConfigurations"
                                    || mn == "getAutoConfigurationImportFilters"
                                    || mn == "getExclusions"))
                            || (cn.contains("ConfigurationClassFilter") && mn == "filter")
                            || (cn.contains("AutoConfigurationEntry") && mn == "getConfigurations");
                        if interesting {
                            let extra = match &value {
                                Value::Object(Some(o)) => {
                                    let r = *o;
                                    let cid = shared.mem.heap.class_id_of(r);
                                    let kind = shared.mem.heap.kind_of(r);
                                    // Try to read as String first
                                    if let Some(s) =
                                        crate::vm::read_java_string(&shared.mem.heap, r)
                                    {
                                        format!(" cid={:?} kind={:?} STRING=\"{}\"", cid, kind, s)
                                    } else if matches!(
                                        kind,
                                        crate::memory::heap::ObjectKind::Object
                                    ) {
                                        // Read ArrayList-style 'size' field heuristically — sample
                                        // field 0..3 to find any Int-typed field
                                        let mut fields_desc = String::new();
                                        for fi in 0..6usize {
                                            let v = shared.mem.heap.get_field(r, fi);
                                            fields_desc.push_str(&format!("f{}={:?} ", fi, v));
                                        }
                                        format!(
                                            " cid={:?} kind={:?} fields=[{}]",
                                            cid, kind, fields_desc
                                        )
                                    } else {
                                        // Try array
                                        let len = shared.mem.heap.array_length(r);
                                        let mut samples = String::new();
                                        let to_read = len.min(20);
                                        for i in 0..to_read {
                                            match shared.mem.heap.get_array_element(r, i) {
                                                Ok(Value::Int(v)) => {
                                                    samples.push_str(&format!("{},", v))
                                                }
                                                Ok(Value::Object(Some(oo))) => {
                                                    if let Some(s) = crate::vm::read_java_string(
                                                        &shared.mem.heap,
                                                        oo,
                                                    ) {
                                                        samples.push_str(&format!("\"{}\",", s));
                                                    } else {
                                                        samples.push_str("O,");
                                                    }
                                                }
                                                Ok(Value::Object(None)) => samples.push_str("N,"),
                                                _ => samples.push('?'),
                                            }
                                        }
                                        format!(
                                            " cid={:?} kind={:?} array_len={} samples=[{}]",
                                            cid, kind, len, samples
                                        )
                                    }
                                }
                                _ => String::new(),
                            };
                            eprintln!("[SBF-RET] {}.{} -> {:?}{}", cn, mn, value, extra);
                        }
                    }
                    // Keep the raw-byte and decoded return handlers
                    // observationally identical for JVMTI. The package gate
                    // previously hid this missing MethodExit event for most
                    // JDK/Spring frames.
                    let return_value = Some(value);
                    let _ = frame;
                    fire_jvmti_method_exit_normal(
                        shared.vm_identity,
                        thread,
                        &thread.frames[frame_idx],
                        &return_value,
                    );
                    if frame_idx > initial_frame_idx {
                        // Stackless return: pop child frame, push value to parent.
                        let ph_r1 = crate::runtime::interpreter::invoke_phases::now();
                        pop_and_recycle_frame(shared, thread);
                        let ph_r2 = crate::runtime::interpreter::invoke_phases::now();
                        crate::runtime::interpreter::invoke_phases::charge(
                            crate::runtime::interpreter::invoke_phases::P_RET_RECYCLE,
                            ph_r1,
                            ph_r2,
                        );
                        frame_idx -= 1;
                        if opcode == 0xb0 {
                            // areturn: push the normalized reference value.
                            thread.frames[frame_idx].stack.push_unchecked(value);
                        } else if opcode == 0xad {
                            // lreturn: `push_compact` alone marks the parent
                            // slot KIND_UNKNOWN, discarding the kind mark
                            // distinction this arm just computed via
                            // `pop_with_kind_unchecked`. A
                            // collision-shaped long (bits alias the NaN-tag
                            // int space, e.g. `0xFFFC_...` whose masked
                            // payload also fits 32 bits) is then
                            // indistinguishable from a real int the next time
                            // the parent frame pops it (invokestatic argument
                            // marshalling, `lload`, etc.), silently truncating
                            // it. Mark KIND_LONG so it survives bit-exact.
                            thread.frames[frame_idx].stack.push_compact_long(cv);
                        } else if opcode == 0xaf {
                            // dreturn: same reasoning as lreturn, KIND_DOUBLE.
                            thread.frames[frame_idx].stack.push_compact_double(cv);
                        } else {
                            // i/f-return: no collision ambiguity for 32-bit
                            // values, raw copy is sufficient.
                            thread.frames[frame_idx].stack.push_compact(cv);
                        }
                        crate::runtime::interpreter::invoke_phases::charge(
                            crate::runtime::interpreter::invoke_phases::P_RET_TOTAL,
                            ph_r0,
                            crate::runtime::interpreter::invoke_phases::now(),
                        );
                        continue;
                    }
                    return Ok(Some(value));
                }
                // return (void)
                0xb1 => {
                    let _ = frame;
                    fire_jvmti_method_exit_normal(
                        shared.vm_identity,
                        thread,
                        &thread.frames[frame_idx],
                        &None,
                    );
                    if frame_idx > initial_frame_idx {
                        pop_and_recycle_frame(shared, thread);
                        frame_idx -= 1;
                        continue;
                    }
                    return Ok(None);
                }
                // ifle — AUDIT CRIT-4
                0x9e => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val <= 0);
                }
                // ifge
                0x9c => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val >= 0);
                }
                // ifgt
                0x9d => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val > 0);
                }
                // iflt
                0x9b => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val < 0);
                }
                // ifne
                0x9a => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val != 0);
                }
                // ifeq
                0x99 => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val == 0);
                }
                // i2l — AUDIT CRIT-4: int-typed pop avoids the 8-arm Value match.
                0x85 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2l sign-extends int → long (lossless).
                    // Direct CompactValue push avoids any Value → CompactValue
                    // boundary tagging drift on long slots.
                    frame.stack.push_long_unchecked(i64::from(val));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ladd — WP4.3: use as_long_unchecked to bypass tag-erasure.
                // `pop_unchecked()` decodes a CompactValue::long(N) (untagged
                // raw bits) as `Value::Double(<denormal>)`, so the
                // `Value::Long` pattern match below was dead code — every
                // long add was a no-op, leaving sums stuck at 0.
                0x61 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va.wrapping_add(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lsub
                0x65 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va.wrapping_sub(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lmul
                0x69 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va.wrapping_mul(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lload_0..3 — route raw CompactValue bits straight through
                // (bit-exact, mirroring the slow-path `Lload` and fast-path
                // `lstore_N`). The prior `get_local_unchecked().to_value()` +
                // `push_unchecked()` round-trip decoded a collision-pattern
                // long (e.g. `0xFFFC_….` whose NaN sub-tag reads as Int) to
                // `Value::Int`, dropping bits 32-46 on re-encode. push_compact
                // preserves every bit and skips the Value decode/encode.
                0x1e => {
                    frame
                        .stack
                        .push_compact_long(frame.get_local_compact_unchecked(0));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x1f => {
                    frame
                        .stack
                        .push_compact_long(frame.get_local_compact_unchecked(1));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x20 => {
                    frame
                        .stack
                        .push_compact_long(frame.get_local_compact_unchecked(2));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x21 => {
                    frame
                        .stack
                        .push_compact_long(frame.get_local_compact_unchecked(3));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lstore_0..3 — WP4.3: route through CompactValue directly so
                // an untagged long bit-pattern keeps its raw bits without
                // detouring via `Value::Double`. The parent slow-path `Lstore`
                // already uses the typed `pop_long`, so this fast-path
                // mirrors that for parity.
                0x3f => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Long(cv.as_long_unchecked());
                    frame.set_local_unchecked(0, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x40 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Long(cv.as_long_unchecked());
                    frame.set_local_unchecked(1, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x41 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Long(cv.as_long_unchecked());
                    frame.set_local_unchecked(2, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x42 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Long(cv.as_long_unchecked());
                    frame.set_local_unchecked(3, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lconst_0, lconst_1
                0x09 => {
                    // Direct CompactValue push — Long is an 8-byte slot on
                    // CompactValue so no category-2 double push is required.
                    frame.stack.push_long_unchecked(0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x0a => {
                    frame.stack.push_long_unchecked(1);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lcmp — WP4.3: bypass tag-erasure via as_long_unchecked.
                0x94 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    let r = if va > vb {
                        1
                    } else if va < vb {
                        -1
                    } else {
                        0
                    };
                    frame.stack.push_unchecked(Value::Int(r));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // iand (0x7e) — AUDIT CRIT-4
                0x7e => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va & vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ior (0x80)
                0x80 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va | vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ixor (0x82)
                0x82 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va ^ vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ishl (0x78) — JVM spec: shift amount masked to 5 bits
                0x78 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame
                        .stack
                        // Widening: smaller value -> u32 (value fits)
                        .push_int_unchecked(va.wrapping_shl(vb as u32 & 0x1f));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ishr (0x7a)
                0x7a => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame
                        .stack
                        // Widening: smaller value -> u32 (value fits)
                        .push_int_unchecked(va.wrapping_shr(vb as u32 & 0x1f));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // iushr (0x7c) — unsigned shift right
                0x7c => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame
                        .stack
                        // Widening: smaller value -> u32 (value fits)
                        .push_int_unchecked(((va as u32).wrapping_shr(vb as u32 & 0x1f)) as i32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ineg (0x74)
                0x74 => {
                    let val = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(val.wrapping_neg());
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lneg (0x75) — WP4.3 tag-erasure bypass.
                0x75 => {
                    let cv = frame.stack.pop_compact();
                    let v = cv.as_long_unchecked();
                    frame.stack.push_long_unchecked(v.wrapping_neg());
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // land (0x7f)
                0x7f => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va & vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lor (0x81)
                0x81 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va | vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lxor (0x83)
                0x83 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va ^ vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ldiv (0x6d)
                0x6d => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    if vb != 0 {
                        frame.stack.push_long_unchecked(va.wrapping_div(vb));
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    // Re-push so the slow path can throw ArithmeticException.
                    frame.stack.push_compact(cva);
                    frame.stack.push_compact(cvb);
                }
                // lrem (0x71)
                0x71 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    if vb != 0 {
                        frame.stack.push_long_unchecked(va.wrapping_rem(vb));
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    frame.stack.push_compact(cva);
                    frame.stack.push_compact(cvb);
                }
                // fload_0..3 (0x22-0x25)
                0x22 => {
                    frame.stack.push_unchecked(frame.get_local_unchecked(0));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x23 => {
                    frame.stack.push_unchecked(frame.get_local_unchecked(1));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x24 => {
                    frame.stack.push_unchecked(frame.get_local_unchecked(2));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x25 => {
                    frame.stack.push_unchecked(frame.get_local_unchecked(3));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fstore_0..3 (0x43-0x46)
                0x43 => {
                    let v = frame.stack.pop_unchecked();
                    frame.set_local_unchecked(0, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x44 => {
                    let v = frame.stack.pop_unchecked();
                    frame.set_local_unchecked(1, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x45 => {
                    let v = frame.stack.pop_unchecked();
                    frame.set_local_unchecked(2, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x46 => {
                    let v = frame.stack.pop_unchecked();
                    frame.set_local_unchecked(3, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dload_0..3 (0x26-0x29)
                0x26 => {
                    frame.stack.push_unchecked(frame.get_local_unchecked(0));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x27 => {
                    frame.stack.push_unchecked(frame.get_local_unchecked(1));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x28 => {
                    frame.stack.push_unchecked(frame.get_local_unchecked(2));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x29 => {
                    frame.stack.push_unchecked(frame.get_local_unchecked(3));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dstore_0..3 (0x47-0x4a) — WP4.3 typed-pop routing so an
                // untagged double bit-pattern lands as `Value::Double`.
                0x47 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Double(f64::from_bits(cv.to_bits()));
                    frame.set_local_unchecked(0, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x48 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Double(f64::from_bits(cv.to_bits()));
                    frame.set_local_unchecked(1, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x49 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Double(f64::from_bits(cv.to_bits()));
                    frame.set_local_unchecked(2, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x4a => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Double(f64::from_bits(cv.to_bits()));
                    frame.set_local_unchecked(3, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fadd (0x62) — Round-3: typed float helpers, no Value enum.
                //
                // Verified bytecode guarantees the top two slots are Float-tagged
                // here.  IEEE 754 semantics (NaN propagation, ±Inf on divide-by-
                // zero) are preserved by the underlying `f32` arithmetic.
                0x62 => {
                    let b = frame.stack.pop_float_unchecked();
                    let a = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(a + b);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fsub (0x66)
                0x66 => {
                    let b = frame.stack.pop_float_unchecked();
                    let a = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(a - b);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fmul (0x6a)
                0x6a => {
                    let b = frame.stack.pop_float_unchecked();
                    let a = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(a * b);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fdiv (0x6e) — JVMS: f / 0.0 = ±Inf / NaN (no exception).
                0x6e => {
                    let b = frame.stack.pop_float_unchecked();
                    let a = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(a / b);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dadd (0x63) — Round-3: typed double helpers, no Value enum.
                //
                // Verified bytecode guarantees Double operands. IEEE 754
                // semantics preserved by the underlying `f64` arithmetic.
                0x63 => {
                    let b = frame.stack.pop_double_unchecked();
                    let a = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(a + b);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dsub (0x67)
                0x67 => {
                    let b = frame.stack.pop_double_unchecked();
                    let a = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(a - b);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dmul (0x6b)
                0x6b => {
                    let b = frame.stack.pop_double_unchecked();
                    let a = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(a * b);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ddiv (0x6f) — JVMS: d / 0.0 = ±Inf / NaN (no exception).
                0x6f => {
                    let b = frame.stack.pop_double_unchecked();
                    let a = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(a / b);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // i2b (0x91), i2c (0x92), i2s (0x93) — AUDIT CRIT-4
                0x91 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2b narrows int to byte via sign-extension
                    frame.stack.push_int_unchecked(val as i8 as i32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x92 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2c narrows int to char (unsigned 16-bit)
                    frame.stack.push_int_unchecked(val as u16 as i32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x93 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2s narrows int to short via sign-extension
                    frame.stack.push_int_unchecked(val as i16 as i32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // i2f (0x86), i2d (0x87) — AUDIT CRIT-4 pop side
                0x86 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2f converts int to float (may lose precision)
                    frame.stack.push_float_unchecked(val as f32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x87 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2d widens int to double (lossless).
                    // Direct CompactValue push keeps the 8-byte slot
                    // tagged with the Double NaN-box encoding.
                    frame.stack.push_double_unchecked(f64::from(val));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // l2i (0x88) — WP4.3 tag-erasure bypass.
                0x88 => {
                    let cv = frame.stack.pop_compact();
                    let val = cv.as_long_unchecked();
                    // JVM spec: l2i narrows long to int (truncates upper 32 bits)
                    frame.stack.push_unchecked(Value::Int(val as i32));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // swap (0x5f)
                0x5f => {
                    let b = frame.stack.pop_unchecked();
                    let a = frame.stack.pop_unchecked();
                    frame.stack.push_unchecked(b);
                    frame.stack.push_unchecked(a);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fconst_0..2 (0x0b-0x0d) — direct CompactValue float push (no Value enum encode).
                0x0b => {
                    frame.stack.push_float_unchecked(0.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x0c => {
                    frame.stack.push_float_unchecked(1.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x0d => {
                    frame.stack.push_float_unchecked(2.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dconst_0, dconst_1 (0x0e-0x0f)
                0x0e => {
                    // Direct CompactValue push — double is an 8-byte slot.
                    frame.stack.push_double_unchecked(0.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x0f => {
                    frame.stack.push_double_unchecked(1.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // nop (0x00)
                0x00 => {
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dup
                0x59 => {
                    let v = frame.stack.peek();
                    frame.stack.push_unchecked(v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // pop — discard the top slot without decoding (no Value enum round-trip).
                0x57 => {
                    frame.stack.pop_compact();
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // aconst_null — push a CompactValue::null() directly (no Value enum encode).
                0x01 => {
                    frame.stack.push_compact(CompactValue::null());
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // iload (0x15), fload (0x17), aload (0x19)
                //
                // H7: `b1` is the raw bytecode operand. The `_unchecked`
                // local accessors index `frame.locals` (len == max_locals)
                // without a bounds check, relying on the verifier. Guard the
                // index here as defence-in-depth for unverified bytecode
                // (per-class `skip_verification` classes not covered by the
                // global `use_fast_path` gate); on an out-of-range operand we
                // skip the unchecked arm and fall through to the
                // bounds-checked slow path (which re-decodes from `saved_pc`,
                // unchanged here).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                0x15 | 0x17 | 0x19 if (b1 as usize) < frame.max_locals as usize => {
                    let mut v = frame.get_local_unchecked(b1 as usize); // Cast: bytecode operand decoding
                    if opcode == 0x19 {
                        // Validated: see aload_0..3 above.
                        v = coerce_value_for_return_validated(shared, v, b'L');
                    }
                    frame.stack.push_unchecked(v);
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // lload (0x16), dload (0x18) — bit-exact CompactValue pass
                // through, matching the slow-path `Lload`/`Dload` arms
                // (`get_local_compact` + `push_compact_checked`). Avoids the
                // lossy `to_value()`/`from_value()` round-trip that dropped
                // collision-pattern longs (see lload_0..3 above).
                // H7: bounds-guard `b1` (see iload/fload/aload above).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                0x16 | 0x18 if (b1 as usize) < frame.max_locals as usize => {
                    let cv = frame.get_local_compact_unchecked(b1 as usize); // Cast: bytecode operand decoding
                                                                             // Mark the slot's category so a NaN-tag-colliding long is
                                                                             // popped bit-exact (lload) rather than sign-extended.
                    if opcode == 0x16 {
                        frame.stack.push_compact_long(cv);
                    } else {
                        frame.stack.push_compact_double(cv);
                    }
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // istore (0x36), fstore (0x38)
                // H7: bounds-guard `b1` (see iload/fload/aload above).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                0x36 | 0x38 if (b1 as usize) < frame.max_locals as usize => {
                    let v = frame.stack.pop_unchecked();
                    frame.set_local_unchecked(b1 as usize, v); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // astore (0x3a) — reference local; coerce jlong jobject handles.
                // Validated to reject aligned non-heap long bits (Letsgo AV).
                // H7: bounds-guard `b1` (see iload/fload/aload above).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                0x3a if (b1 as usize) < frame.max_locals as usize => {
                    let v = frame.stack.pop_unchecked();
                    frame.set_local_unchecked(
                        b1 as usize, // Cast: bytecode operand decoding
                        coerce_value_for_return_validated(shared, v, b'L'),
                    );
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // lstore (0x37), dstore (0x39) — WP4.3 typed-pop routing.
                // H7: bounds-guard `b1` (see iload/fload/aload above).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                0x37 | 0x39 if (b1 as usize) < frame.max_locals as usize => {
                    let cv = frame.stack.pop_compact();
                    let v = if opcode == 0x37 {
                        Value::Long(cv.as_long_unchecked())
                    } else {
                        // dstore: untagged slot is raw f64 bits.
                        Value::Double(f64::from_bits(cv.to_bits()))
                    };
                    frame.set_local_unchecked(b1 as usize, v); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // xaload: iaload..saload (0x2e..=0x35)
                0x2e..=0x35 => {
                    let idx_val = frame.stack.pop_unchecked();
                    let arr_val = frame.stack.pop_unchecked();
                    if let (Value::Object(Some(arr_ref)), Value::Int(index)) = (arr_val, idx_val) {
                        if index < 0 {
                            let _ = frame;
                            pending_runtime_error = Some((
                                RuntimeError::aioobe(
                                    index,
                                    shared.mem.heap.array_length(arr_ref) as i32,
                                ),
                                saved_pc,
                            ));
                            continue;
                        }
                        // Widening: index conversion
                        if let Some(zgc) = fast_field_zgc {
                            if field_fast::array_load_prim(zgc, &mut frame.stack, arr_ref, index, opcode) {
                                frame.pc = saved_pc + 1;
                                continue;
                            }
                        }
                        match shared.mem.heap.get_array_element(arr_ref, index as usize) {
                            Ok(value) => {
                                frame.stack.push_unchecked(value);
                                frame.pc = saved_pc + 1;
                                continue;
                            }
                            Err(i) => {
                                let _ = frame;
                                pending_runtime_error = Some((
                                    RuntimeError::aioobe(
                                        i,
                                        shared.mem.heap.array_length(arr_ref) as i32,
                                    ),
                                    saved_pc,
                                ));
                                continue;
                            }
                        }
                    }
                    frame.stack.push_unchecked(arr_val);
                    frame.stack.push_unchecked(idx_val);
                }
                // xastore: iastore(0x4f), lastore(0x50), fastore(0x51), dastore(0x52),
                //          bastore(0x54), castore(0x55), sastore(0x56)
                //
                // WP4.3 — opcode-typed pop.  The plain `pop_unchecked()` (=
                // `to_value()`) decodes an untagged long bit-pattern as
                // `Value::Double(<denormal>)`, then `set_array_element` →
                // `write_prim_element` only matches `Value::Long(_)` for a
                // long[] slot and falls through to zero.  That makes
                // `arr[i] = 10L; System.out.println(arr[i])` print `0`.
                // Inspect the opcode and coerce to the JVMS-declared type
                // so the operand-stack tag-erasure cannot regress here.
                0x4f..=0x52 | 0x54..=0x56 => {
                    let (cv, kind_of_popped) = frame.stack.pop_with_kind_unchecked();
                    if let Some(zgc) = fast_field_zgc {
                        if frame.stack.len() >= 2 {
                            let idx_cv = frame.stack.peek_compact();
                            let arr_cv = frame.stack.peek_compact_at(1);
                            if let (Some(index), Some(aptr)) = (idx_cv.as_int(), arr_cv.as_object_ptr()) {
                                // SAFETY: an `Object`-tagged operand-stack slot holds a
                                // heap address; `array_store_prim` re-validates it.
                                let arr_ref = unsafe { ObjectRef::from_raw(aptr as *mut u8) };
                                if field_fast::array_store_prim(zgc, arr_ref, index, opcode, cv, kind_of_popped) {
                                    frame.stack.pop_compact();
                                    frame.stack.pop_compact();
                                    frame.pc = saved_pc + 1;
                                    continue;
                                }
                            }
                        }
                    }
                    let value = match opcode {
                        0x50 => Value::Long(cv.as_long_unchecked()),
                        0x52 => {
                            // dastore: untagged slot is raw f64 bits, and a slot
                            // the push marked KIND_DOUBLE is raw f64 bits too
                            // even when they collide with the NaN-box tag space
                            // — which is why the mark is consulted before the
                            // tag (see `CompactValue::double_raw`).
                            use crate::types::CompactTag;
                            if kind_of_popped == crate::runtime::ValueStack::KIND_MARK_DOUBLE {
                                Value::Double(f64::from_bits(cv.to_bits()))
                            } else {
                                match cv.tag() {
                                    CompactTag::Double => {
                                        Value::Double(f64::from_bits(cv.to_bits()))
                                    }
                                    CompactTag::Long => {
                                        // Rare: NaN-tagged-collision long landing
                                        // in a double slot; reinterpret the bits.
                                        Value::Double(f64::from_bits(cv.to_bits()))
                                    }
                                    _ => cv.to_value(),
                                }
                            }
                        }
                        // iastore / fastore / bastore / castore / sastore —
                        // the value is single-slot Int / Float, the existing
                        // decode path handles them correctly.
                        _ => cv.to_value(),
                    };
                    // Round-3: typed int pop for the array index — the spec
                    // mandates an int operand for every x-astore.
                    let index = frame.stack.pop_int_unchecked();
                    let arr_val = frame.stack.pop_unchecked();
                    if let Value::Object(Some(arr_ref)) = arr_val {
                        if index < 0 {
                            let _ = frame;
                            pending_runtime_error = Some((
                                RuntimeError::aioobe(
                                    index,
                                    shared.mem.heap.array_length(arr_ref) as i32,
                                ),
                                saved_pc,
                            ));
                            continue;
                        }
                        match shared
                            .mem.heap
                            .set_array_element(arr_ref, index as usize, value) // Widening: index conversion
                        {
                            Ok(()) => {
                                frame.pc = saved_pc + 1;
                                continue;
                            }
                            Err(i) => {
                                let _ = frame;
                                pending_runtime_error = Some((
                                    RuntimeError::aioobe(i, shared.mem.heap.array_length(arr_ref) as i32),
                                    saved_pc,
                                ));
                                continue;
                            }
                        }
                    }
                    frame.stack.push_unchecked(arr_val);
                    frame.stack.push_int_unchecked(index);
                    frame.stack.push_unchecked(value);
                }
                // aastore (0x53) — needs SATB pre-barrier + write barrier
                0x53 => {
                    let raw_value = coerce_value_for_return_validated(
                        shared,
                        frame.stack.pop_unchecked(),
                        b'L',
                    );
                    // A valid `aastore` always receives a reference.  A few
                    // native/reflection bridges can nevertheless surface a raw
                    // primitive at this boundary (notably serialization's
                    // primitive field path).  Do the Java boxing here, while
                    // the executing thread is still available, instead of
                    // letting the GC manufacture an untyped AUTOBOX sentinel.
                    let value = normalize_aastore_value(shared, raw_value);
                    // Round-3: typed int pop for the array index.
                    let index = frame.stack.pop_int_unchecked();
                    let arr_val = frame.stack.pop_unchecked();
                    if let Value::Object(Some(arr_ref)) = arr_val {
                        // JVMS 6.5 fixes the order NPE -> AIOOBE -> ASE, and the
                        // bounds test is TWO-SIDED. This arm checked only
                        // `index < 0`, so an index PAST THE END fell through to
                        // the covariance check below and reported
                        // ArrayStoreException where HotSpot reports
                        // ArrayIndexOutOfBoundsException (measured:
                        // RArrayStoreTiers s15, String[] as Object[], index 5
                        // into length 1).
                        //
                        // The slow-path `Instruction::Aastore` arm in opcodes.rs
                        // received the full two-sided check first. This fast-path
                        // twin is the one the interpreter actually dispatches, so
                        // fixing only the other one changed nothing observable --
                        // the same two-handlers-for-one-opcode drift as the JIT
                        // emitter that never called `jit_aastore`.
                        let arr_len = shared.mem.heap.array_length(arr_ref) as i32;
                        if index < 0 || index >= arr_len {
                            let _ = frame;
                            pending_runtime_error =
                                Some((RuntimeError::aioobe(index, arr_len), saved_pc));
                            continue;
                        }
                        // JVMS §aastore covariance check (mirrors the slow-path
                        // `Instruction::Aastore` arm): a non-null element whose
                        // runtime type is not assignment-compatible with the
                        // array's component type throws ArrayStoreException.
                        // `aastore_element_assignable` fails open on imprecise
                        // type info, so this is additive and never a false ASE.
                        if let Value::Object(Some(elem_ref)) = value {
                            if shared.mem.heap.kind_of(arr_ref) == cratonvm_types::ObjectKind::Array
                                && shared.mem.heap.element_type_of(arr_ref)
                                    == ArrayElementType::Reference
                                && !aastore_element_assignable(shared, arr_ref, elem_ref)
                            {
                                let _ = frame;
                                // Name the VALUE'S OWN class, HotSpot-style. On
                                // a reference array the header class id is the
                                // COMPONENT's, so the raw lookup answers
                                // `java.lang.Integer` for an `Integer[]` where
                                // HotSpot answers `[Ljava.lang.Integer;`
                                // (`RArrayStoreTiers` s04). `cce_display_class_
                                // name` is the existing repair for exactly that
                                // — see the long note on the slow-path
                                // `Instruction::Aastore` twin in opcodes.rs.
                                // Separate statements: the helper takes the
                                // class-manager read lock itself.
                                let raw_elem_name = shared
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(shared.mem.heap.class_id_of(elem_ref))
                                    .map(|c| c.name.to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                let elem_cls =
                                    cce_display_class_name(shared, elem_ref, &raw_elem_name);
                                pending_runtime_error = Some((
                                    RuntimeError::ArrayStoreException { message: elem_cls },
                                    saved_pc,
                                ));
                                continue;
                            }
                        }
                        // SATB pre-barrier: log old array element before overwrite
                        // Widening: index conversion
                        if let Ok(old_elem) =
                            shared.mem.heap.get_array_element(arr_ref, index as usize)
                        {
                            shared.mem.heap.satb_barrier(old_elem);
                        }
                        match shared
                            .mem.heap
                            .set_array_element(arr_ref, index as usize, value) // Widening: index conversion
                        {
                            Ok(()) => {
                                // write_barrier fires automatically inside set_array_element
                                frame.pc = saved_pc + 1;
                                continue;
                            }
                            Err(i) => {
                                let _ = frame;
                                pending_runtime_error = Some((
                                    RuntimeError::aioobe(i, shared.mem.heap.array_length(arr_ref) as i32),
                                    saved_pc,
                                ));
                                continue;
                            }
                        }
                    }
                    frame.stack.push_unchecked(arr_val);
                    frame.stack.push_int_unchecked(index);
                    frame.stack.push_unchecked(value);
                }
                // arraylength (0xbe) — direct int push on the success path.
                0xbe => {
                    let arr_val = frame.stack.pop_unchecked();
                    if let Value::Object(Some(arr_ref)) = arr_val {
                        let len = shared.mem.heap.array_length(arr_ref);
                        frame.stack.push_int_unchecked(len as i32); // Cast: array length to JVM int
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    frame.stack.push_unchecked(arr_val);
                }
                // invokevirtual — stackless dispatch with monomorphic inline cache
                0xb6 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    let _ = frame;
                    thread.frames[frame_idx].pc = saved_pc + 3;
                    if invoke_fast_door_on {
                        match execute_invokevirtual_fast_door(
                            shared,
                            thread,
                            frame_idx,
                            cp_index,
                            false,
                            fast_field_zgc,
                        ) {
                            Some(Ok(CachedCallResult::FramePushed)) => {
                                frame_idx = thread.frames.len() - 1;
                                continue;
                            }
                            Some(Ok(_)) => {
                                continue;
                            }
                            Some(Err(e)) => match classify_fastpath_invoke_error(shared, thread, e) {
                                FastPathInvokeError::Runtime(re) => {
                                    pending_runtime_error = Some((re, saved_pc));
                                    continue;
                                }
                                FastPathInvokeError::Java(exc) => {
                                    pending_java_exception = Some((exc, saved_pc));
                                    continue;
                                }
                                FastPathInvokeError::Fatal(e) => return Err(e),
                            },
                            None => {}
                        }
                    }
                    // PERF: consult the cheap thread-local inline cache FIRST.
                    // A warm monomorphic site hits here and dispatches with one
                    // class-id compare + arg decode + frame push — no locks, no
                    // hierarchy walk. Only on a miss (cold site, or the receiver
                    // class changed) do we fall to the heavier `vtable_fast`
                    // resolution, which re-populates the inline cache. (This is
                    // the order the `execute_invokevirtual_vtable_fast` header
                    // comment always described — "only invoked on its miss
                    // path" — but the dispatch had it inverted, so `vtable_fast`'s
                    // 3 RwLocks + 2 hierarchy walks ran on EVERY virtual call and
                    // the inline cache was never consulted.)
                    let cached_result = execute_invokevirtual_cached(
                        shared, thread, frame_idx, cp_index, saved_pc, false, false,
                    );
                    match cached_result {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                    // Miss path — VtableManager lock-free dispatch; a hit here
                    // populates invoke_cache for the next call.
                    match execute_invokevirtual_vtable_fast(
                        shared, thread, frame_idx, cp_index, saved_pc, false,
                    ) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                    match execute_invoke(shared, thread, frame_idx, cp_index, false, saved_pc) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(_) => {
                            continue;
                        }
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                }
                // invokespecial — stackless dispatch with cache
                0xb7 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    let _ = frame;
                    thread.frames[frame_idx].pc = saved_pc + 3;
                    let cached_result = execute_invokevirtual_cached(
                        shared, thread, frame_idx, cp_index, saved_pc, true, false,
                    );
                    match cached_result {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                    match execute_invoke(shared, thread, frame_idx, cp_index, true, saved_pc) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(_) => {
                            continue;
                        }
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                }
                // invokestatic — stackless dispatch with cache
                0xb8 => {
                    // Collapse TDigest's exact Integer -> Function.apply -> Double
                    // adapter before it allocates either wrapper in interpreter mode.
                    // The helper verifies both the lambda metadata and its concrete
                    // getter bytecode; a miss preserves the ordinary invoke path.
                    //
                    // ORDER MATTERS (2026-09-02). This recognizer is
                    // workload-specific and this arm is EVERY `invokestatic` in
                    // the VM. It used to open with
                    // `frame.class_name() == "org/elasticsearch/tdigest/Dist"`,
                    // so every static call in every program paid a `FrameInner`
                    // match, an `Arc<str>` deref and a length compare before
                    // reaching the dispatch it actually wanted.
                    //
                    // The operands are all pure, so `&&` may be reordered
                    // freely, and the BYTECODE-SHAPE half is both cheaper and
                    // far more selective: three byte loads from `code_ptr`,
                    // which the preamble has already pulled into L1, against a
                    // four-opcode window (`invokestatic; invokeinterface; …
                    // checkcast; … invokevirtual`) that essentially no other
                    // call site matches. The name tests now run only for a call
                    // site that already looks exactly like the kernel.
                    //
                    // SAFETY: `code_ptr` addresses this frame's bytecode and the
                    // `saved_pc + 14 <= code_len` test proves offsets +3/+8/+11
                    // are in bounds of the padded buffer.
                    if saved_pc + 14 <= code_len
                        && unsafe { *code_ptr.add(saved_pc + 3) } == 0xb9
                        && unsafe { *code_ptr.add(saved_pc + 8) } == 0xc0
                        && unsafe { *code_ptr.add(saved_pc + 11) } == 0xb6
                        && frame.class_name() == "org/elasticsearch/tdigest/Dist"
                        && matches!(frame.method_name(), "quantile" | "cdf")
                        && frame.method_descriptor() == "(DILjava/util/function/Function;)D"
                    {
                        let index = frame.stack.pop_unchecked();
                        let lambda = frame.stack.pop_unchecked();
                        if let (Value::Int(index), Value::Object(Some(proxy))) = (index, lambda) {
                            if let Some(value) = try_tdigest_lambda_double_get(shared, proxy, index)
                            {
                                frame.stack.push_unchecked(Value::Double(value));
                                frame.pc = saved_pc + 14;
                                continue;
                            }
                        }
                        frame.stack.push_unchecked(lambda);
                        frame.stack.push_unchecked(index);
                    }
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    let _ = frame;
                    thread.frames[frame_idx].pc = saved_pc + 3;
                    let cached_result =
                        execute_invokestatic_cached(shared, thread, frame_idx, cp_index, saved_pc);
                    match cached_result {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                    match execute_invokestatic(shared, thread, frame_idx, cp_index, saved_pc) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(_) => {
                            continue;
                        }
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                }
                // invokeinterface — stackless dispatch with monomorphic inline cache
                0xb9 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    let _ = frame;
                    // invokeinterface is 5 bytes: opcode(1) + index(2) + count(1) + 0(1)
                    thread.frames[frame_idx].pc = saved_pc + 5;
                    if invoke_fast_door_on {
                        match execute_invokevirtual_fast_door(
                            shared,
                            thread,
                            frame_idx,
                            cp_index,
                            true,
                            fast_field_zgc,
                        ) {
                            Some(Ok(CachedCallResult::FramePushed)) => {
                                frame_idx = thread.frames.len() - 1;
                                continue;
                            }
                            Some(Ok(_)) => {
                                continue;
                            }
                            Some(Err(e)) => match classify_fastpath_invoke_error(shared, thread, e) {
                                FastPathInvokeError::Runtime(re) => {
                                    pending_runtime_error = Some((re, saved_pc));
                                    continue;
                                }
                                FastPathInvokeError::Java(exc) => {
                                    pending_java_exception = Some((exc, saved_pc));
                                    continue;
                                }
                                FastPathInvokeError::Fatal(e) => return Err(e),
                            },
                            None => {}
                        }
                    }
                    let cached_result = execute_invokevirtual_cached(
                        shared, thread, frame_idx, cp_index, saved_pc, false, true,
                    );
                    match cached_result {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                    // Miss path: interface dispatch shares the same vtable fast-path
                    // because the receiver's vtable already carries the
                    // concrete (name, desc) → slot mapping regardless of
                    // whether the call-site is invokevirtual or
                    // invokeinterface.
                    match execute_invokevirtual_vtable_fast(
                        shared, thread, frame_idx, cp_index, saved_pc, true,
                    ) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                    // invokeinterface: thread is_interface=true so γ's stash
                    // arms the default-method rescue.
                    match execute_invoke_kind(
                        shared, thread, frame_idx, cp_index, false, true, saved_pc,
                    ) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(_) => {
                            continue;
                        }
                        Err(e) => match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                                continue;
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        },
                    }
                }
                // ── Fast-path coverage completion (interpreter audit
                // 2026-08-18) ────────────────────────────────────────────
                //
                // Everything below reached the decoded handler until now: the
                // quickened stream had to resolve `saved_pc` (instruction-start
                // bitmap + per-block popcount), the ~200-arm `Instruction` match
                // in `opcodes.rs` had to be entered, and each arm re-indexed
                // `thread.frames[frame_idx]` — bounds-checked, `imul` by
                // `size_of::<Frame>()` — once per operand it touched.
                //
                // The gaps were not chosen, they are what the fast path grew up
                // around: `ishl`/`ishr`/`iushr` had arms but `lshl`/`lshr`/`lushr`
                // did not, `ineg`/`lneg` did but `fneg`/`dneg` did not,
                // `irem`/`lrem` did but `frem`/`drem` did not, `lcmp` did but
                // `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` did not, `i2l`/`i2f`/`i2d`/`l2i`
                // did but the eight remaining conversions did not, and `dup`/`pop`
                // did but the six other shuffles did not. The long shifts alone
                // are the whole of a 64-bit hash mixer (`h ^= h >>> 33`) and of
                // most of the bit-twiddling in the crypto suites this VM runs.

                // ── Reference comparisons: if_acmpeq / if_acmpne ──────────
                // These close a loop in `do { … } while (a != b)` and in any
                // bottom-test frontend's output; see `cond_branch_arm!` for
                // what having no arm here used to cost such a loop.
                0xa5 => {
                    let vb = frame.stack.pop_unchecked();
                    let va = frame.stack.pop_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, refs_equal(&va, &vb));
                }
                // The `active_profiles_identity_trace` diagnostic lives on the
                // decoded `IfAcmpne` arm and prints class names for both
                // operands. Decline the fast path while it is armed rather than
                // duplicating it, so turning the flag on still reaches it.
                0xa6 if !acmp_identity_trace => {
                    let vb = frame.stack.pop_unchecked();
                    let va = frame.stack.pop_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, !refs_equal(&va, &vb));
                }
                // ── Null tests: ifnull / ifnonnull ────────────────────────
                // `ref_operand_is_null` (not `Value::is_null`) so a JNI jobject
                // null carried as `Value::Long(0)` reads as null — identical to
                // the decoded arm this replaces.
                0xc6 => {
                    let v = frame.stack.pop_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, ref_operand_is_null(&v));
                }
                0xc7 => {
                    let v = frame.stack.pop_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, !ref_operand_is_null(&v));
                }

                // ── Long shifts (JVMS: shift distance masked to 6 bits) ───
                0x79 => {
                    let sh = frame.stack.pop_int_unchecked() & 0x3F;
                    let v = frame.stack.pop_long_unchecked();
                    frame.stack.push_long_unchecked(v << sh);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x7b => {
                    let sh = frame.stack.pop_int_unchecked() & 0x3F;
                    let v = frame.stack.pop_long_unchecked();
                    frame.stack.push_long_unchecked(v >> sh);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x7d => {
                    let sh = frame.stack.pop_int_unchecked() & 0x3F;
                    // Widening: unsigned conversion for the logical shift
                    let v = frame.stack.pop_long_unchecked() as u64;
                    // Cast: JIT ABI -- i64 register convention
                    frame.stack.push_long_unchecked((v >> sh) as i64);
                    frame.pc = saved_pc + 1;
                    continue;
                }

                // ── Float / double negate and remainder ───────────────────
                // Rust's `%` on floats is fmod, which is exactly what JVMS
                // §6.5 frem/drem specify (NOT the IEEE 754 remainder).
                0x72 => {
                    let vb = frame.stack.pop_float_unchecked();
                    let va = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(va % vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x73 => {
                    let vb = frame.stack.pop_double_unchecked();
                    let va = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(va % vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x76 => {
                    let v = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(-v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x77 => {
                    let v = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(-v);
                    frame.pc = saved_pc + 1;
                    continue;
                }

                // ── The eight remaining primitive conversions ─────────────
                // Narrowings to integer defer to the same saturation helpers
                // the decoded arms use (JVMS §2.8.3: NaN → 0, +inf → MAX,
                // -inf → MIN); widenings are plain casts.
                0x89 => {
                    let v = frame.stack.pop_long_unchecked();
                    // JVM spec: l2f may lose precision
                    frame.stack.push_float_unchecked(v as f32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8a => {
                    let v = frame.stack.pop_long_unchecked();
                    // JVM spec: l2d may lose precision above 2^53
                    frame.stack.push_double_unchecked(v as f64);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8b => {
                    let v = frame.stack.pop_float_unchecked();
                    frame.stack.push_int_unchecked(float_to_int(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8c => {
                    let v = frame.stack.pop_float_unchecked();
                    frame.stack.push_long_unchecked(float_to_long(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8d => {
                    let v = frame.stack.pop_float_unchecked();
                    frame.stack.push_double_unchecked(f64::from(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8e => {
                    let v = frame.stack.pop_double_unchecked();
                    frame.stack.push_int_unchecked(double_to_int(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8f => {
                    let v = frame.stack.pop_double_unchecked();
                    frame.stack.push_long_unchecked(double_to_long(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x90 => {
                    let v = frame.stack.pop_double_unchecked();
                    // JVM spec: d2f may lose precision
                    frame.stack.push_float_unchecked(v as f32);
                    frame.pc = saved_pc + 1;
                    continue;
                }

                // ── Float / double comparisons ────────────────────────────
                // `l` and `g` differ only in the NaN answer.
                0x95 | 0x96 => {
                    let vb = frame.stack.pop_float_unchecked();
                    let va = frame.stack.pop_float_unchecked();
                    let nan_result = if opcode == 0x95 { -1 } else { 1 };
                    let result = if va.is_nan() || vb.is_nan() {
                        nan_result
                    } else if va > vb {
                        1
                    } else if va == vb {
                        0
                    } else {
                        -1
                    };
                    frame.stack.push_int_unchecked(result);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x97 | 0x98 => {
                    let vb = frame.stack.pop_double_unchecked();
                    let va = frame.stack.pop_double_unchecked();
                    let nan_result = if opcode == 0x97 { -1 } else { 1 };
                    let result = if va.is_nan() || vb.is_nan() {
                        nan_result
                    } else if va > vb {
                        1
                    } else if va == vb {
                        0
                    } else {
                        -1
                    };
                    frame.stack.push_int_unchecked(result);
                    frame.pc = saved_pc + 1;
                    continue;
                }

                // ── Operand-stack shuffles ────────────────────────────────
                // Every one of these moves slot bits AND the kind mark, which
                // is the whole reason they use `*_with_kind_unchecked` rather
                // than a `Value` round-trip: `ValueStack::is_cat2_kind` reads
                // the mark to decide whether a NaN-tag-colliding long is one
                // logical category-2 value or two category-1 slots, and the GC
                // reads it to decide whether a pointer-shaped slot is a
                // reference. Structure mirrors the decoded arms exactly.
                0x58 => {
                    let (val, kind) = frame.stack.pop_with_kind_unchecked();
                    if !crate::runtime::ValueStack::is_cat2_kind(kind, val) {
                        frame.stack.pop_with_kind_unchecked();
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5a => {
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                    frame.stack.push_with_kind_unchecked(v1, k1);
                    frame.stack.push_with_kind_unchecked(v2, k2);
                    frame.stack.push_with_kind_unchecked(v1, k1);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5b => {
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                    if crate::runtime::ValueStack::is_cat2_kind(k2, v2) {
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    } else {
                        let (v3, k3) = frame.stack.pop_with_kind_unchecked();
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v3, k3);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5c => {
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    if crate::runtime::ValueStack::is_cat2_kind(k1, v1) {
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    } else {
                        let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5d => {
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                    if crate::runtime::ValueStack::is_cat2_kind(k1, v1) {
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    } else {
                        let (v3, k3) = frame.stack.pop_with_kind_unchecked();
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v3, k3);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5e => {
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                    let v1c2 = crate::runtime::ValueStack::is_cat2_kind(k1, v1);
                    let v2c2 = crate::runtime::ValueStack::is_cat2_kind(k2, v2);
                    if v1c2 && v2c2 {
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    } else if v1c2 {
                        let (v3, k3) = frame.stack.pop_with_kind_unchecked();
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v3, k3);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    } else if v2c2 {
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    } else {
                        let (v3, k3) = frame.stack.pop_with_kind_unchecked();
                        let (v4, k4) = frame.stack.pop_with_kind_unchecked();
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v4, k4);
                        frame.stack.push_with_kind_unchecked(v3, k3);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ── Constant-pool and object opcodes ──────────────────────
                //
                // These had no arm, so each paid the quickened `resolve(pc)`,
                // the frame-pointer hoist and its `code_ptr` compare, a
                // non-inlined call into `execute_instruction`, the ~200-variant
                // `Instruction` match and the post-call diagnostic checks —
                // all before its body ran.
                //
                // The bodies are NOT copied here. Each lives in exactly one
                // `opcodes::op_*` function and BOTH paths call it, so there is
                // still one implementation per opcode. Two copies of an opcode
                // is precisely the shape `difftest`'s `interp-decoded` axis
                // exists to catch — `getfield` alone is 450 lines, and a fix
                // landing in one copy and not the other is the failure that
                // axis was built after.
                //
                // `pc` is advanced BEFORE the call because the decoded path
                // does (it writes `next_pc` ahead of dispatch) and several of
                // these bodies read `frame.pc` back — `monitorenter` and
                // `monitorexit` snapshot it, and the diagnostic blocks print
                // it. Advancing after the call would change what they observe.
                // getfield / putfield — quickened arm first (`field_fast`), the
                // full handler on any miss. The full handler refills the site.
                0xb4 | 0xb5 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    if let Some(zgc) = fast_field_zgc {
                        let hit = if opcode == 0xb4 {
                            field_fast::getfield_fast(
                                shared,
                                zgc,
                                &mut thread.fast_field_sites,
                                frame,
                                cp_index,
                            )
                        } else {
                            field_fast::putfield_fast(
                                zgc,
                                &mut thread.fast_field_sites,
                                frame,
                                cp_index,
                            )
                        };
                        if hit {
                            frame.pc = saved_pc + 3;
                            continue;
                        }
                    }
                    let _ = frame;
                    thread.frames[frame_idx].pc = saved_pc + 3;
                    let outcome = if opcode == 0xb4 {
                        op_getfield(shared, thread, frame_idx, cp_index)
                    } else {
                        op_putfield(shared, thread, frame_idx, cp_index)
                    };
                    if let Err(e) = outcome {
                        match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        }
                    }
                    continue;
                }
                0xb2 | 0xb3 | 0xbb | 0xc0 | 0xc1 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    let _ = frame;
                    thread.frames[frame_idx].pc = saved_pc + 3;
                    let outcome = match opcode {
                        0xb2 => op_getstatic(shared, thread, frame_idx, cp_index),
                        0xb3 => op_putstatic(shared, thread, frame_idx, cp_index),
                        0xbb => op_new(shared, thread, frame_idx, cp_index),
                        0xc0 => op_checkcast(shared, thread, frame_idx, cp_index),
                        // 0xc1
                        _ => op_instanceof(shared, thread, frame_idx, cp_index),
                    };
                    if let Err(e) = outcome {
                        // Same classifier the invoke fast paths use: it performs
                        // the Runtime/Linkage conversions the slow path's
                        // per-opcode guard would otherwise have done, which is
                        // the step those arms once skipped and killed the
                        // process over.
                        match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        }
                    }
                    continue;
                }
                // monitorenter / monitorexit — single-byte, so `pc` advances by
                // one rather than three; otherwise identical to the arm above.
                0xc2 | 0xc3 => {
                    let _ = frame;
                    thread.frames[frame_idx].pc = saved_pc + 1;
                    let outcome = if opcode == 0xc2 {
                        op_monitorenter(shared, thread, frame_idx)
                    } else {
                        op_monitorexit(shared, thread, frame_idx)
                    };
                    if let Err(e) = outcome {
                        match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        }
                    }
                    continue;
                }
                // ldc (0x12, 1-byte index) / ldc_w (0x13) / ldc2_w (0x14).
                // `execute_ldc` / `execute_ldc2w` were already factored out, so
                // these arms are pure dispatch removal. The `ldc` family also
                // needs the malformed-constant-pool conversion the decoded arms
                // apply, so it is applied here too rather than dropped.
                0x12 | 0x13 | 0x14 => {
                    let _ = frame;
                    let (cp_index, width) = if opcode == 0x12 {
                        (b1 as u16, 2) // Cast: bytecode operand decoding
                    } else {
                        (((b1 as u16) << 8) | (b2 as u16), 3) // Cast: bytecode operand decoding
                    };
                    thread.frames[frame_idx].pc = saved_pc + width;
                    let raw = if opcode == 0x14 {
                        execute_ldc2w(shared, thread, frame_idx, cp_index)
                    } else {
                        execute_ldc(shared, thread, frame_idx, cp_index)
                    };
                    if let Err(e) = raw {
                        let e = convert_ldc_class_format_error(shared, thread, e);
                        match classify_fastpath_invoke_error(shared, thread, e) {
                            FastPathInvokeError::Runtime(re) => {
                                pending_runtime_error = Some((re, saved_pc));
                            }
                            FastPathInvokeError::Java(exc) => {
                                pending_java_exception = Some((exc, saved_pc));
                            }
                            FastPathInvokeError::Fatal(e) => return Err(e),
                        }
                    }
                    continue;
                }
                _ => { /* fall through to slow path */ }
            }
        }

        // --- Slow path: pre-decoded (quickened) lookup, else full decode ---
        //
        // The quickened stream is a pure memoization of `Instruction::decode`
        // keyed by bytecode pc: `resolve` only ever hands back the record whose
        // recorded pc equals `saved_pc`, together with exactly the `next_pc`
        // `decode` returned there. Every pc-keyed consumer downstream
        // (exception table, stack maps, line numbers, JVMTI single-step,
        // JIT/OSR entry) therefore sees identical values. Any pc the stream
        // does not know -- dead bytes, a desynchronised landing pad -- falls
        // through to the original decode below.
        // ── Frame-pointer hoist (frame-arena.md §6.1) ────────────────────
        // `JvmThread::frames` is a `FrameStack`, which guarantees that a
        // frame's address never changes unless the backing buffer grows, and
        // that growth happens only inside `reserve_stable`/`push` and always
        // bumps `reloc_epoch()`. That makes it sound to hold one raw frame
        // pointer across the whole dispatch region instead of re-indexing.
        //
        // The region this pointer covers runs from the derivation below to
        // the `execute_instruction` call, and contains **no push, no pop, and
        // no access to `thread.frames` other than through `fp`**. The four
        // aliasing rules therefore hold:
        //   1. No overlapping reference. Every frame access in the region
        //      goes through `fp`; in particular `quickened_for_frame` takes a
        //      single `&Frame`, which we derive from `fp` itself, and nothing
        //      here takes the `&[Frame]` slice view (no `capture_full_trace`,
        //      no root scan, no unwinding — those all live outside).
        //   2. No `&mut` reborrow of the stack, so `fp`'s provenance stays
        //      live; `thread` is next touched by `execute_instruction`, by
        //      which point `fp` is dead.
        //   3. No relocation, hence no epoch change — asserted in debug.
        //   4. Frame still live: the null check below *is* the
        //      `frame_idx < thread.frames.len()` test, and it routes a
        //      violation through `VmError` where indexing would have panicked.
        //
        // This collapses 13 bounds-checked, `imul`-by-`size_of::<Frame>()`
        // re-indexes per executed bytecode down to one address computation.
        #[cfg(debug_assertions)]
        let reloc_epoch_at_hoist = thread.frames.reloc_epoch();
        let fp: *mut Frame = thread.frames.frame_ptr(frame_idx);
        if fp.is_null() {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!(
                    "frame index {frame_idx} out of range (depth {}) at pc={saved_pc}",
                    thread.frames.len()
                ),
            }));
        }
        // SAFETY: `fp` is non-null and in bounds (checked above), and rules
        // 1-3 above hold for every dereference in this region.
        let code_ptr_now = unsafe { (*fp).code.as_ptr() };
        if code_ptr_now != quick_code_ptr {
            // SAFETY: as above. `quickened_for_frame` only reads the frame.
            quick = quickened_for_frame(unsafe { &*fp });
            quick_code_ptr = code_ptr_now;
        }
        // One O(1) call yielding both the pre-decoded instruction and the
        // `next_pc` that `Instruction::decode` reported there, replacing
        // `index_of_pc` + `op` + `next_pc` and their three bounds checks.
        let quick_hit: Option<(&Instruction, usize)> =
            quick.as_deref().and_then(|q| q.resolve(saved_pc));
        // Only populated on the fallback path; keeps the freshly decoded
        // instruction alive for as long as `instruction` borrows it.
        let mut fallback_decoded: Option<Instruction> = None;
        if quick_hit.is_none() {
            // SAFETY: hoist rules 1-4 above. All accesses here are reads of
            // the frame plus the single `pc` write-back; the `&[u8]` borrow of
            // `code` ends when `decode` returns, before that write.
            let decoded_at_pc = unsafe {
                Instruction::decode(&(*fp).code, (*fp).pc).map_err(|e| {
                    MethodCallFailed::InternalError(VmError::Internal {
                        message: format!(
                            "decode error at pc={} in {}.{}: {}",
                            (*fp).pc,
                            (*fp).method_name(),
                            (*fp).method_descriptor(),
                            e
                        ),
                    })
                })
            };
            let (decoded, next_pc) = decoded_at_pc?;
            // SAFETY: `fp` is the hoisted pointer to the executing frame,
            // null-checked at the top of the dispatch region. This region
            // performs no push, so the frame stack cannot have reallocated and
            // moved the frame; nothing else references it here.
            unsafe { (*fp).pc = next_pc };
            fallback_decoded = Some(decoded);
        }
        // Panic-free selection (B3 / NEW-7 zero-panic gate): the `(None, None)`
        // arm is unreachable by construction -- the block above always fills
        // `fallback_decoded` when there is no quickened hit -- but it is routed
        // through `VmError` rather than an `unreachable!`.
        let instruction: &Instruction = match (quick_hit, fallback_decoded.as_ref()) {
            (Some((quickened, next_pc)), _) => {
                // SAFETY: hoist rules 1-4 above.
                unsafe { (*fp).pc = next_pc };
                quickened
            }
            (None, Some(decoded)) => decoded,
            (None, None) => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("no instruction decoded at pc={saved_pc}"),
                }))
            }
        };

        trace!(
            pc = saved_pc,
            instruction = ?instruction,
            // SAFETY: hoist rules 1-4 above.
            stack_depth = unsafe { (*fp).stack.len() },
            "execute"
        );

        // K1 diagnostic: capture per-opcode context (class, method, pc, opcode byte).
        // Debug-only and further gated at runtime by CRATONVM_DEBUG_STACK_TAG=1;
        // release builds compile update_diag_ctx to an empty function.
        #[cfg(debug_assertions)]
        {
            // SAFETY: hoist rules 1-4 above; all three accesses are reads.
            let frame: &Frame = unsafe { &*fp };
            let opcode_byte = frame.code.get(saved_pc).copied().unwrap_or(0);
            crate::runtime::value_stack::update_diag_ctx(
                frame.class_name(),
                frame.method_name(),
                saved_pc,
                opcode_byte,
            );
        }

        // Last use of `fp`: `execute_instruction` takes `&mut thread` and may
        // push frames, so the hoisted pointer must not outlive this point.
        #[cfg(debug_assertions)]
        {
            // Hoist rule 3: nothing in the region above may relocate frames.
            // A bump here would mean `fp` had gone stale mid-region.
            assert_eq!(
                reloc_epoch_at_hoist,
                thread.frames.reloc_epoch(),
                "frame relocation inside the dispatch region would invalidate \
                 the hoisted frame pointer"
            );
        }
        let exec_result = execute_instruction(shared, thread, frame_idx, instruction, saved_pc);

        // DIAG (gated `CRATONVM_DBG_UNDERFLOW=1`): pinpoint an operand-stack
        // underflow — log the offending method/bci/opcode + the Java frame chain
        // the first time one surfaces, so a mis-modelled bytecode path can be
        // minimized without a full TestAll run. (The H2 TestAll underflow is no
        // longer reproducible after the toArray(T[]) template-type fix; this is
        // a low-cost tripwire — it only runs on the rare IllegalState error path.)
        if let Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalStateException { message },
        ))) = &exec_result
        {
            if message == "operand stack underflow"
                && cratonvm_types::flags::runtime_var("CRATONVM_DBG_UNDERFLOW").is_ok()
            {
                use std::sync::atomic::{AtomicBool, Ordering};
                static FIRED: AtomicBool = AtomicBool::new(false);
                if !FIRED.swap(true, Ordering::Relaxed) {
                    let f = &thread.frames[frame_idx];
                    eprintln!(
                        "[DBG_UNDERFLOW] at {}.{}{} bci={} opcode={:?} stack_len={}",
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor(),
                        saved_pc,
                        instruction,
                        f.stack.len(),
                    );
                    for (i, fr) in thread.frames.iter().enumerate().rev() {
                        eprintln!(
                            "    [{}] {}.{}{} pc={}",
                            i,
                            fr.class_name(),
                            fr.method_name(),
                            fr.method_descriptor(),
                            fr.pc
                        );
                    }
                }
            }
        }

        // DIAG (gated `CRATONVM_DBG_POPINT=1`): pinpoint a `pop_int` type
        // mismatch ("expected int on stack, got ref(...)") — log the offending
        // method/bci/opcode + the Java frame chain the first time one surfaces.
        if let Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NotImplemented { feature },
        ))) = &exec_result
        {
            if feature.starts_with("expected int on stack, got")
                && cratonvm_types::flags::runtime_var("CRATONVM_DBG_POPINT").is_ok()
            {
                use std::sync::atomic::{AtomicBool, Ordering};
                static FIRED_POPINT: AtomicBool = AtomicBool::new(false);
                if !FIRED_POPINT.swap(true, Ordering::Relaxed) {
                    let f = &thread.frames[frame_idx];
                    eprintln!(
                        "[DBG_POPINT] {} at {}.{}{} bci={} opcode={:?} stack_len={}",
                        feature,
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor(),
                        saved_pc,
                        instruction,
                        f.stack.len(),
                    );
                    for (i, fr) in thread.frames.iter().enumerate().rev() {
                        eprintln!(
                            "    [{}] {}.{}{} pc={}",
                            i,
                            fr.class_name(),
                            fr.method_name(),
                            fr.method_descriptor(),
                            fr.pc
                        );
                    }
                }
            }
        }

        // Convert RuntimeErrors from native methods into catchable Java exceptions.
        let exec_result = match exec_result {
            Err(MethodCallFailed::InternalError(VmError::Runtime(runtime_err)))
                if !matches!(
                    runtime_err,
                    RuntimeError::NotImplemented { .. } | RuntimeError::StackOverflowError
                ) =>
            {
                let mcf =
                    crate::runtime::exceptions::throw_runtime_error(shared, thread, runtime_err);
                Err(mcf)
            }
            Err(MethodCallFailed::InternalError(VmError::Linkage(linkage_err))) => {
                if dbg_linkage() {
                    dbg_linkage_dump(thread, "slow-path opcode", &format!("{linkage_err:?}"));
                }
                let mcf =
                    crate::runtime::exceptions::throw_linkage_error(shared, thread, linkage_err);
                Err(mcf)
            }
            other => other,
        };

        #[allow(unreachable_patterns)]
        match exec_result {
            Ok(InstructionResult::Continue) => {
                // ── Decoded-path back-edge accounting ───────────────────
                //
                // The raw-bytecode arms above account for their own branches.
                // Anything that reaches the decoded handler and jumps BACKWARDS
                // had no accounting at all: `goto_w`, `tableswitch`,
                // `lookupswitch`, `ret`, and — under `-noverify`, where
                // `use_fast_path` is false — every branch in the instruction
                // set. A loop closed by one of those was invisible to back-edge
                // OSR and to the loop-work tier-up credit
                // `pop_and_recycle_frame_with_reason` computes from
                // `Frame::backward_count`.
                //
                // Testing the resulting pc rather than enumerating opcodes is
                // deliberate: it is the property that actually matters, it
                // cannot be forgotten when an opcode is added, and it is one
                // compare against a value already in hand — on the slow path
                // only, since a fast-path arm never reaches here.
                //
                // `Continue` is the only result this can key off: `FramePushed`
                // and `Return` change the frame under `frame_idx`, and a thrown
                // exception leaves through `Err`, so a handler landing pad
                // below `saved_pc` is not mistaken for a loop back edge.
                let new_pc = thread.frames[frame_idx].pc;
                if new_pc < saved_pc {
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                        shared
                            .jit
                            .profile_store
                            .record_backedge_borrowed(cid, mn, md, saved_pc);
                    }
                    thread.frames[frame_idx].backward_count += 1;
                    let osr_due = thread.frames[frame_idx].backward_count >= osr_call_floor;
                    if osr_due {
                    match try_osr_with_backoff(
                        shared,
                        thread,
                        &mut frame_idx,
                        initial_frame_idx,
                        new_pc,
                    ) {
                        OsrBackoffOutcome::ReturnOuter(v) => return Ok(v),
                        OsrBackoffOutcome::ContinueDispatch => continue,
                        OsrBackoffOutcome::ThrowJava(exc) => {
                            pending_java_exception = Some((exc, OSR_FRAME_DECLINED_TO_CATCH));
                            continue;
                        }
                        OsrBackoffOutcome::Skip => {}
                    }
                    }
                    if backedge_poll_needed!() {
                        safepoint_check(shared, thread);
                    }
                }
                continue;
            }
            Ok(InstructionResult::FramePushed) => {
                // A new bytecode frame was pushed — execute it iteratively
                frame_idx = thread.frames.len() - 1;
                continue;
            }
            Ok(InstructionResult::Return(value)) => {
                // Slow-path return — check for stackless frames
                if frame_idx > initial_frame_idx {
                    let ret = thread.frames[frame_idx].return_tag();
                    let value = value.map(|v| {
                        if ret == b'V' {
                            v
                        } else {
                            coerce_value_for_return(v, ret)
                        }
                    });
                    pop_and_recycle_frame(shared, thread);
                    frame_idx -= 1;
                    if let Some(v) = value {
                        // T18.K4 — route J/D through the tag-exact push so
                        // invoke* callees returning longs/doubles keep
                        // their tag on the caller's operand stack.
                        push_invoke_return_value(&mut thread.frames[frame_idx].stack, v)
                            .map_err(|e| MethodCallFailed::InternalError(VmError::Runtime(e)))?;
                    }
                    continue;
                }
                return Ok(value);
            }
            Err(MethodCallFailed::InternalError(
                yield_signal @ VmError::ContinuationYield { .. },
            )) => {
                // Yield crosses every nested execute_frame boundary without
                // unwinding a Java frame. In particular, preserve the leaf
                // frame whose native sleep/park issued the signal: its PC is
                // already after the invoke and its operand stack contains the
                // live values needed when the continuation is remounted.
                return Err(MethodCallFailed::InternalError(yield_signal));
            }
            Err(MethodCallFailed::InternalError(VmError::Runtime(re))) => {
                // B4 fix (audit `vm-runtime.md`): an operand-stack overflow is
                // reported by `ValueStack::push` as
                // `NotImplemented { feature: "operand stack overflow" }`, which
                // `throw_runtime_error` maps to an *uncatchable* internal error
                // (exceptions.rs) — so a JVM stack overflow on unverified
                // bytecode (or a verifier gap) hard-unwinds the whole call stack
                // instead of surfacing as a Java-catchable
                // `java.lang.StackOverflowError`. Normalize it to the real
                // `StackOverflowError` runtime variant here (the only point that
                // converts runtime errors into Java exceptions) so the routing
                // below builds a real throwable and an in-method
                // `catch (StackOverflowError)` / `catch (Throwable)` observes it.
                let re = match re {
                    RuntimeError::NotImplemented { feature }
                        if feature == "operand stack overflow" =>
                    {
                        RuntimeError::StackOverflowError
                    }
                    other => other,
                };
                // Convert VM-generated RuntimeErrors (AIOOBE, NPE, CCE, etc.)
                // into real Java exception objects so they can be caught by
                // Java try/catch blocks.
                let exc_result = super::exceptions::throw_runtime_error(shared, thread, re);
                match exc_result {
                    MethodCallFailed::ExceptionThrown(exc) => {
                        // Route through the ExceptionThrown handler below
                        // GC-root gap: see the pin in the `pending_java_exception`
                        // arm earlier in this function — same
                        // unpinned-across-frame-pops-and-lazy-class-load hazard.
                        let mut exc_pc = saved_pc;
                        let pin_base = thread.native_pin_roots.len();
                        thread.native_pin_roots.push(exc);
                        loop {
                            let current_exc = thread.native_pin_roots[pin_base];
                            match find_exception_handler(
                                shared,
                                &thread.frames[frame_idx],
                                exc_pc,
                                current_exc,
                            ) {
                                Some((handler_pc, exc_ref)) => {
                                    thread.frames[frame_idx].stack.clear();
                                    thread.frames[frame_idx]
                                        .stack
                                        .push(Value::Object(Some(exc_ref)))
                                        .map_err(|e| {
                                            MethodCallFailed::InternalError(VmError::Runtime(e))
                                        })?;
                                    thread.frames[frame_idx].pc = handler_pc;
                                    fire_jvmti_exception_catch(
                                        shared.vm_identity,
                                        &thread.frames[frame_idx],
                                        handler_pc,
                                    );
                                    thread.native_pin_roots.truncate(pin_base);
                                    break;
                                }
                                None => {
                                    if frame_idx > initial_frame_idx {
                                        // T17.Δ — abrupt completion: this
                                        // frame is unwinding an exception.
                                        pop_and_recycle_frame_with_reason(shared, thread, true);
                                        frame_idx -= 1;
                                        exc_pc = thread.frames[frame_idx].last_instr_pc;
                                    } else {
                                        let current_exc = thread.native_pin_roots[pin_base];
                                        thread.native_pin_roots.truncate(pin_base);
                                        return Err(MethodCallFailed::ExceptionThrown(current_exc));
                                    }
                                }
                            }
                        }
                    }
                    // If we couldn't create the Java exception object, fall back
                    // to the old behavior (unwind as internal error).
                    // `throw_runtime_error` only ever returns the two
                    // `MethodCallFailed` variants above; the match is
                    // exhaustive, so no panicking `_ => unreachable!()`
                    // catch-all is needed (NEW-7 / B3 panic-free gate).
                    other @ MethodCallFailed::InternalError(_) => {
                        while frame_idx > initial_frame_idx {
                            pop_and_recycle_frame_with_reason(shared, thread, true);
                            frame_idx -= 1;
                        }
                        return Err(other);
                    }
                }
            }
            Err(MethodCallFailed::InternalError(e)) => {
                // Non-Runtime internal errors (linkage, classfile, etc.) — unwind.
                if dbg_linkage() {
                    dbg_linkage_dump(thread, "unwind (uncatchable)", &format!("{e:?}"));
                }
                while frame_idx > initial_frame_idx {
                    pop_and_recycle_frame_with_reason(shared, thread, true);
                    frame_idx -= 1;
                }
                return Err(MethodCallFailed::InternalError(e));
            }
            Err(MethodCallFailed::ExceptionThrown(exc)) => {
                // `native_pending_return` roots a native call's returned
                // object across the Rust<->Java boundary until it is pushed
                // onto the caller's operand stack (see `safe_native_call` /
                // `native_return_pushed_to_stack`). It is UNRELATED to the
                // exception being unwound here in the common case — it can
                // still hold a leftover value if the native call that set it
                // never reached the "push to stack" step (e.g. its return
                // value was discarded, or a later, independent exception
                // fired before the pending value was consumed). Previously
                // this code unconditionally preferred `native_pending_return`
                // whenever it was `Some`, which meant a stale leftover object
                // (observed: a `CommonToken` left over from ANTLR HQL
                // parsing) silently replaced the REAL exception being
                // propagated, misattributing the uncaught exception's class
                // in fatal-error reporting.
                //
                // Only fall back to `native_pending_return` when `exc` itself
                // has gone stale (relocated/reclaimed by a moving GC that ran
                // during the failing native call, so `exc`'s address is no
                // longer a valid live object) — mirroring the staleness check
                // `safe_native_call` already performs in `vm_exec.rs`. The
                // slot is always drained via `.take()` so a leftover value
                // can never survive to poison a later, unrelated exception.
                let pending_return = thread.native_pending_return.take();
                let exc = if shared
                    .mem
                    .heap
                    .is_object_address(exc.as_ptr() as usize)
                    .is_some()
                {
                    exc
                } else {
                    pending_return.unwrap_or(exc)
                };
                // Try to find handler, unwinding through stackless frames
                // GC-root gap: see the pin in the `pending_java_exception` arm
                // earlier in this function — same
                // unpinned-across-frame-pops-and-lazy-class-load hazard.
                let mut exc_pc = saved_pc;
                let pin_base = thread.native_pin_roots.len();
                thread.native_pin_roots.push(exc);
                loop {
                    let current_exc = thread.native_pin_roots[pin_base];
                    match find_exception_handler(
                        shared,
                        &thread.frames[frame_idx],
                        exc_pc,
                        current_exc,
                    ) {
                        Some((handler_pc, exc_ref)) => {
                            // Perf: a leftover Quarkus trace block here took a
                            // `class_manager.read()` RwLock + `to_string()` on
                            // every exception caught by a handler, plus a second
                            // RwLock + alloc for the (also-unused) `exc_class`.
                            // The trace site was empty, so all of it was dead
                            // computation. Dropped — behaviour is identical.
                            thread.frames[frame_idx].stack.clear();
                            thread.frames[frame_idx]
                                .stack
                                .push(Value::Object(Some(exc_ref)))
                                .map_err(|e| {
                                    MethodCallFailed::InternalError(VmError::Runtime(e))
                                })?;
                            thread.frames[frame_idx].pc = handler_pc;
                            fire_jvmti_exception_catch(
                                shared.vm_identity,
                                &thread.frames[frame_idx],
                                handler_pc,
                            );
                            thread.native_pin_roots.truncate(pin_base);
                            break;
                        }
                        None => {
                            if frame_idx > initial_frame_idx {
                                // T17.Δ — Unwind to parent frame (exception).
                                pop_and_recycle_frame_with_reason(shared, thread, true);
                                frame_idx -= 1;
                                // Use last_instr_pc to point at the invoke opcode itself.
                                // `pc` has already advanced past the 3-byte (invokevirtual/
                                // static/special) or 5-byte (invokeinterface/invokedynamic)
                                // instruction, so `pc - 1` would land inside operand bytes
                                // and miss the handler's [start_pc, end_pc) range.
                                // `last_instr_pc` is written by the dispatch loop (see
                                // interpreter.rs:2358) immediately before each opcode, so
                                // it is current at any throw site reachable from this path.
                                exc_pc = thread.frames[frame_idx].last_instr_pc;
                            } else {
                                // Perf: the previous `exc_class` / `caller` /
                                // `mname` bindings here each took a
                                // `class_manager.read()` RwLock + `to_string()`
                                // allocation on every top-frame exception
                                // propagation, but their values were never used
                                // (no trace site consumed them). Dropped —
                                // behaviour is identical (pure, discarded
                                // computations).
                                let current_exc = thread.native_pin_roots[pin_base];
                                thread.native_pin_roots.truncate(pin_base);
                                return Err(MethodCallFailed::ExceptionThrown(current_exc));
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-opcode dispatch
// ---------------------------------------------------------------------------
//
// `execute_instruction`, one arm per JVM opcode: `interpreter/opcodes.rs`.

// ---------------------------------------------------------------------------
// Helper: lambda proxy type-check for checkcast/instanceof
// ---------------------------------------------------------------------------
//
// Moved to `interpreter/typecheck.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod typecheck;
pub use typecheck::*;
// ---------------------------------------------------------------------------
// Helper: LDC / LDC_W (load constant from pool)
// ---------------------------------------------------------------------------
//
// Moved to `interpreter/constants.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
pub(crate) mod constants;
pub use constants::*;
// ---------------------------------------------------------------------------
// Helper: Field resolution
// ---------------------------------------------------------------------------
//
// Moved to `interpreter/field_access.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
//
// C2 P0 — the module declaration itself is `pub(crate)` (it was private) so
// that `crate::runtime::resolve::MemberResolver` can name the field-resolution
// core it delegates to. Nothing inside became more visible: the glob re-export
// still caps each item at its own declared visibility, and only the one item
// `resolve` calls was widened. Call `runtime::resolve`, not this module —
// `runtime::resolve::guard` enforces it.
pub(crate) mod field_access;
pub use field_access::*;
mod field_fast;
// The interpreter's resolved constant pool: per-thread, lock-free site caches
// for field and method constant-pool references. `pub` so `vm-cli` can print
// the `CRATONVM_DBG=field-site` tally at exit.
pub mod invoke_phases;
pub mod site_cache;
pub use site_cache::{
    CastSiteCache, ClassSiteCache, FastFieldSite, FastFieldSiteCache, FieldSiteCache,
    IfaceSelectSiteCache, MethodSiteCache, MethodSiteInfo, ResolvedNewSite,
};
// ---------------------------------------------------------------------------
// Helper: Method invocation
// ---------------------------------------------------------------------------
//
// Moved to `interpreter/invoke.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
//
// C2 P0 — `pub(crate)` for the same reason as `field_access` above: it is the
// method-resolution core that `crate::runtime::resolve::MemberResolver`
// delegates to. Only `resolve_method_metadata` was widened.
pub(crate) mod invoke;
pub use invoke::*;
mod gc_and_alloc;
pub use gc_and_alloc::*;
mod opcodes;
pub use opcodes::*;
mod deopt_resume;
pub use deopt_resume::*;
mod exception_dispatch;
pub use exception_dispatch::*;
mod jvmti_events;
pub use jvmti_events::*;
mod dispatch_virtual;
pub use dispatch_virtual::*;
mod dispatch_static;
pub use dispatch_static::*;
mod lambda;
pub use lambda::*;
mod native_override;
pub use native_override::*;
pub(crate) mod jit_bridge;
pub use jit_bridge::*;
// The frame-level half of the osr-02 exit differential: back-edge arrivals and
// OSR-exit resumed frames, in one format, under one flag. Inert unless
// `CRATONVM_DBG_OSR_FRAME_TRACE` names a class substring.
mod osr_frame_trace;

// ---------------------------------------------------------------------------
// Helper: loader-faithful ARRAY class resolution (JVMS §5.3.3)
// ---------------------------------------------------------------------------

/// Resolve the array class `name` the way JVMS §5.3.3 requires, from the
/// perspective of `referencing_class_id`'s defining loader.
///
/// > If the component type is a `reference` type, the Java Virtual Machine
/// > marks C to have the defining loader of the component type as its defining
/// > loader. Otherwise, the Java Virtual Machine marks C to have the bootstrap
/// > class loader as its defining loader.
/// >
/// > — JVMS SE 21 §5.3.3, step 2
///
/// So `[Lp/X;` referenced from a class defined by loader A is a *different*
/// runtime class from `[Lp/X;` referenced from loader B whenever A and B each
/// define their own `p/X`. Collapsing the two is type confusion: array class
/// identity is what `checkcast`/`instanceof` on an array type, the verifier's
/// array-assignability rules and `Class.getComponentType()` all read.
///
/// Returns `None` — meaning "no loader-faithful answer, use the ordinary global
/// resolution" — for a non-array name, for a referencing class whose defining
/// loader is built-in, or when the class manager cannot synthesise the array.
/// It can therefore only ever return a *more* precise answer, never fail a
/// resolution that the global path would have answered.
///
/// Locking: the O(1) exact-key probe runs under the class-manager **read**
/// lock, and the write lock is taken only on the cold "this loader has never
/// resolved this array descriptor" path — at most once per
/// `(loader, array descriptor)` pair. Same discipline as
/// `SharedVm::load_class_concurrent`; each guard is bound to a `let` so it is
/// dropped at the semicolon rather than being held across the next acquisition
/// (`parking_lot::RwLock` is not reentrant — see the note in
/// `typecheck::array_is_assignable_to_impl`).
pub(crate) fn resolve_array_class_loader_aware(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if !name.starts_with('[') {
        return None;
    }
    let requesting_loader = {
        shared
            .classes
            .class_manager
            .read()
            .get_loader_id(referencing_class_id)
    }?;
    // Only a user-defined component loader can produce a non-bootstrap array
    // class (see `ClassManager::array_defining_loader` for why the three
    // built-in loaders are deliberately collapsed onto `Bootstrap`), so for a
    // built-in referencing class this path has nothing to add and must not pay
    // for the probe.
    if !matches!(
        requesting_loader,
        cratonvm_types::ClassLoaderId::UserDefined(_)
    ) {
        return None;
    }
    // Fast path. Exact key only: a delegating probe would hand back another
    // loader's array class, which is the whole bug.
    //
    // Probing under `requesting_loader` is sound even though the array's key is
    // its *component's* loader — a hit means an array class named `name` is
    // filed under this loader, which by construction (the key is derived from
    // the component) means its component was defined by this loader.
    let cached = {
        shared
            .classes
            .class_manager
            .read()
            .loaded_class_under_exact_key(name, requesting_loader)
    };
    if let Some(id) = cached {
        return Some(id);
    }
    // §5.3.3 step 1: "the algorithm of this section is applied recursively
    // **using L** in order to load and thereby create the component type".
    // `ClassManager` cannot invoke a Java `loadClass`, so drive the component
    // through this loader here, before deriving the array's defining loader
    // from it. Without this the component would fall back to the flat global
    // store and the array would come out bootstrap-keyed again.
    if let Some(component) = array_component_class_name(name) {
        let known = {
            shared
                .classes
                .class_manager
                .read()
                .find_class_by_name_for_loader(component, requesting_loader)
        };
        if known.is_none() {
            let _ = drive_defining_loader_load(shared, thread, referencing_class_id, component);
        }
    }
    shared
        .classes
        .class_manager
        .write()
        .load_array_class_for_loader(name, requesting_loader)
        .ok()
}

/// [`resolve_array_class_loader_aware`] with the ordinary
/// [`resolve_class_loader_aware`] as its fallback — the drop-in replacement for
/// a `CONSTANT_Class` resolution site that may be handed either an array
/// descriptor or a plain class name.
pub(crate) fn resolve_class_or_array_loader_aware(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Result<ClassId, MethodCallFailed> {
    if name.starts_with('[') {
        let loader_faithful =
            resolve_array_class_loader_aware(shared, thread, referencing_class_id, name);
        if let Some(id) = loader_faithful {
            return Ok(id);
        }
    }
    resolve_class_loader_aware(shared, thread, referencing_class_id, name)
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

fn pop_object_ref(stack: &mut crate::runtime::ValueStack) -> Result<ObjectRef, MethodCallFailed> {
    pop_object_ref_ctx(stack, None)
}

/// Variant of `pop_object_ref_ctx` that defers the NPE-context message
/// construction until the NPE actually fires. Use this from call sites
/// that need a `format!`-built message — previously they paid the
/// per-pop formatting cost on every reference-touching opcode even
/// when the pop succeeded.
///
/// `heap` is required because Double/Long slots can carry smuggled
/// `jobject` bit patterns (the JNI "long-as-jobject" pattern used by
/// e.g. WildFly's jboss-modules bootloader). Without a heap-membership
/// check, an honest `Value::Double` whose `to_bits()` value happens to
/// satisfy the 8-aligned + <2^48 predicate would be coerced into a
/// wild `ObjectRef` and dereferenced by the next GC scan or field
/// access (C7). Mirrors the validation used by
/// `value_stack::scan_object_refs` for the same smuggle path.
fn pop_object_ref_ctx_with<F>(
    stack: &mut crate::runtime::ValueStack,
    heap: &crate::memory::vm_heap::VmHeap,
    make_context: F,
) -> Result<ObjectRef, MethodCallFailed>
where
    F: FnOnce() -> String,
{
    match stack.pop()? {
        Value::Object(Some(obj_ref)) => Ok(obj_ref),
        Value::Object(None) => Err(RuntimeError::NullPointerException {
            message: Some(make_context()),
        }
        .into()),
        Value::Uninitialized => Err(RuntimeError::NullPointerException {
            message: Some(make_context()),
        }
        .into()),
        Value::Int(0) | Value::Long(0) => Err(RuntimeError::NullPointerException {
            message: Some(make_context()),
        }
        .into()),
        Value::Double(d) => {
            let bits = d.to_bits();
            if bits == 0 {
                Err(RuntimeError::NullPointerException {
                    message: Some(make_context()),
                }
                .into())
            } else if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                // C7 fix: alignment + 48-bit-range alone are NOT sufficient
                // to prove a Double slot holds a smuggled jobject — an honest
                // f64 (e.g. `f64::from_bits(0x800)`) can satisfy them too. Use
                // `is_heap_addr` (the same loose arena+alignment check that
                // `value_stack::scan_object_refs` uses for the JNI long-as-
                // jobject smuggle pattern) to confirm the bits actually point
                // into the managed heap before fabricating an `ObjectRef`.
                // Widening: small integer index -> usize (non-negative, fits in pointer width)
                if let Some(obj_ref) = heap.is_heap_addr(bits as usize) {
                    // SAFETY: `is_heap_addr` returned `Some(ObjectRef)`,
                    // meaning the address is within one of the GC's managed
                    // arenas and 8-byte aligned. The returned `ObjectRef`
                    // was constructed by the heap layer itself (not by us);
                    // we forward it unchanged. The GC's MAX_SANE_OBJECT_SIZE
                    // sanity guard in `forward_object` handles any residual
                    // bogus root gracefully (over-retention only).
                    Ok(obj_ref)
                } else {
                    Err(VmError::Internal {
                        message: format!(
                            "expected object reference, got double({d}) with non-heap bit pattern"
                        ),
                    }
                    .into())
                }
            } else {
                Err(VmError::Internal {
                    message: format!("expected object reference, got double({d})"),
                }
                .into())
            }
        }
        Value::Long(l) => {
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            let bits = l as u64;
            if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                // C7 fix: see Double-arm comment above. The JNI long-as-
                // jobject smuggle (WildFly jboss-modules bootloader) is
                // preserved, but we now require the bits to land inside a
                // managed-heap arena before we trust them as an `ObjectRef`.
                // Widening: small integer index -> usize (non-negative, fits in pointer width)
                if let Some(obj_ref) = heap.is_heap_addr(bits as usize) {
                    // SAFETY: `is_heap_addr` returned `Some(ObjectRef)`,
                    // meaning the address is within one of the GC's managed
                    // arenas and 8-byte aligned. The returned `ObjectRef`
                    // was constructed by the heap layer itself; we forward
                    // it unchanged. The GC's MAX_SANE_OBJECT_SIZE sanity
                    // guard in `forward_object` handles any residual bogus
                    // root gracefully (over-retention only).
                    Ok(obj_ref)
                } else {
                    Err(VmError::Internal {
                        message: format!(
                            "expected object reference, got long({l}) with non-heap bit pattern"
                        ),
                    }
                    .into())
                }
            } else {
                Err(VmError::Internal {
                    message: format!("expected object reference, got long({l})"),
                }
                .into())
            }
        }
        other => {
            let ctx = make_context();
            if crate::runtime::env_cache::iae_trace_os() {
                eprintln!(
                    "[pop_object_ref] ERROR: expected object reference, got {other} ctx={ctx:?}"
                );
                let bt = std::backtrace::Backtrace::capture();
                eprintln!("[pop_object_ref] Rust backtrace:\n{bt}");
            }
            Err(VmError::Internal {
                message: format!("expected object reference, got {other} ctx={ctx:?}"),
            }
            .into())
        }
    }
}

/// HIGH — accepts the NPE-context message as `Option<&str>` so the
/// common case (literal context like `"Cannot load from null array"`)
/// pays zero allocations: the borrowed slice is only copied into an
/// owned `String` on the NPE-error path. Previously this took
/// `Option<String>`, forcing callers to `.to_string()` every pop —
/// which fires once per `aaload`/`aastore`/`getfield`/`monitorenter`,
/// i.e. essentially every reference-touching opcode.
///
/// Callers with a dynamically-formatted message that needs string
/// interpolation should use `pop_object_ref_ctx_with`, which defers
/// the formatting until the NPE actually fires.
fn pop_object_ref_ctx(
    stack: &mut crate::runtime::ValueStack,
    context: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    match stack.pop()? {
        Value::Object(Some(obj_ref)) => Ok(obj_ref),
        Value::Object(None) => Err(RuntimeError::NullPointerException {
            message: context.map(|s| s.to_string()),
        }
        .into()),
        // SAFETY: The JVM heap zero-initializes object fields.  When a
        // reference-typed field has never been written, the raw bits are
        // 0x0000…, which our tagged-value representation decodes as
        // `Value::Int(0)` (or `Value::Long(0)` on 64-bit fields).
        // Treating these as null matches the JVM spec §2.3 default
        // value semantics for reference types (null == zero).
        //
        // `Value::Uninitialized` is the same family: it surfaces when a
        // CompactValue with `SUB_UNINIT` subtag or a local/stack slot with
        // `VTAG_UNINIT` is decoded back to a `Value`.  This happens for
        // never-written slots that went through a tag-mismatched path
        // (e.g. a reference field whose backing CompactValue was carved
        // out by the uninit slot helper rather than `CompactValue::null`).
        // Per JVMS §2.3 the spec default for a reference type is `null`,
        // so coerce to NPE rather than panicking — matches how Int(0) /
        // Long(0) are handled above, and produces the same surface
        // behaviour the caller's `ctx` message expects ("…object is
        // null").  Observed crash signature on ActiveMQ boot:
        //   internal error: expected object reference, got <uninitialized>
        //   ctx=Some("Cannot read field 'formatter' because the object is null")
        Value::Uninitialized => Err(RuntimeError::NullPointerException {
            message: context.map(|s| s.to_string()),
        }
        .into()),
        Value::Int(0) | Value::Long(0) => Err(RuntimeError::NullPointerException {
            message: context.map(|s| s.to_string()),
        }
        .into()),
        // K1-family: a `CompactValue` storing a tag-lost ObjectRef pointer
        // (pushed via the long path or via a static-field round-trip that
        // dropped the SUB_OBJECT tag) decodes as `Value::Double` because
        // `to_value()` on an untagged `CompactValue` unconditionally yields
        // a Double. Recover the pointer if its bit pattern matches a valid
        // heap address (8-byte aligned, fits in the 47-bit user-space
        // window). Same rule the existing native-side `recover_object_arg`
        // helper applies at native-call boundaries — extending it to the
        // op-stack pop path closes the symmetric ChmStress-style failure.
        Value::Double(d) => {
            let bits = d.to_bits();
            if bits == 0 {
                Err(RuntimeError::NullPointerException {
                    message: context.map(|s| s.to_string()),
                }
                .into())
            } else if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                // 8-byte alignment + 47-bit user-space heap address window.
                // SAFETY: same untagged pointer pattern used by the array
                // reference reader and `recover_object_arg`. `from_raw` does
                // not dereference until a downstream heap accessor validates.
                Ok(unsafe { ObjectRef::from_raw(bits as usize as *mut u8) })
            } else {
                Err(VmError::Internal {
                    message: format!("expected object reference, got double({d})"),
                }
                .into())
            }
        }
        // K1-family: a non-zero Long sitting where an object reference was
        // expected — happens when the long path pushed raw bits and the
        // current opcode demands a reference. Recover if the bits look like
        // a valid heap pointer.
        Value::Long(l) => {
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            let bits = l as u64;
            if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                // SAFETY: bits is guarded to be 8-byte-aligned and within the 48-bit heap address range before reinterpreting as an object pointer.
                Ok(unsafe { ObjectRef::from_raw(bits as usize as *mut u8) })
            } else {
                Err(VmError::Internal {
                    message: format!("expected object reference, got long({l})"),
                }
                .into())
            }
        }
        other => {
            if crate::runtime::env_cache::iae_trace_os() {
                eprintln!("[pop_object_ref] ERROR: expected object reference, got {other} ctx={context:?}");
                // Print a Rust backtrace to identify the calling opcode handler
                let bt = std::backtrace::Backtrace::capture();
                eprintln!("[pop_object_ref] Rust backtrace:\n{bt}");
            }
            Err(VmError::Internal {
                message: format!("expected object reference, got {other} ctx={context:?}"),
            }
            .into())
        }
    }
}

fn branch_target(saved_pc: usize, offset: i16) -> usize {
    (saved_pc as isize + offset as isize) as usize // Cast: signed branch offset arithmetic
}

/// Test whether two reference values are the same object (identity comparison).
/// Exposed as `pub` for testing from vm.rs.
pub fn test_refs_equal(a: &Value, b: &Value) -> bool {
    refs_equal(a, b)
}

#[inline]
fn value_as_object_ptr(v: &Value) -> Option<*mut u8> {
    match v {
        Value::Object(Some(o)) => Some(o.as_ptr()),
        Value::Long(bits) => {
            // Cast: reinterpret pointer/address to typed pointer
            crate::types::jlong_bits_as_aligned_object_ptr(*bits as u64).map(|p| p as *mut u8)
        }
        _ => None,
    }
}

/// JVMS `ifnull`/`ifnonnull` (and any shared reference null test): determine
/// whether a reference operand is the null reference.
///
/// Beyond the obvious `Value::Object(None)` / `Value::Uninitialized`, this also
/// treats a reference-typed `Value::Long(0)` as null — a JNI `jobject` null
/// handle smuggled through the operand stack as raw long bits (the same
/// `Long(0)`-is-null contract already honored by `pop_object_ref*` and
/// `refs_equal`). A non-zero `Value::Long` whose bits form an aligned heap
/// pointer is a live jobject and is NOT null; any non-reference numeric value
/// cannot legally reach `ifnull`/`ifnonnull` (the verifier requires a reference
/// operand), so only the zero case is special-cased.
#[inline]
fn ref_operand_is_null(v: &Value) -> bool {
    match v {
        Value::Object(None) | Value::Uninitialized => true,
        // jobject-as-long: 0 is the null handle. A non-zero long is either a
        // live jobject pointer or an honest long value — neither is null.
        Value::Long(0) => true,
        _ => false,
    }
}

fn refs_equal(a: &Value, b: &Value) -> bool {
    match (value_as_object_ptr(a), value_as_object_ptr(b)) {
        (Some(pa), Some(pb)) => pa == pb,
        (None, None) => match (a, b) {
            (Value::Object(None), Value::Object(None)) => true,
            (Value::Int(va), Value::Int(vb)) => va == vb,
            (Value::Int(0), Value::Object(None)) | (Value::Object(None), Value::Int(0)) => true,
            (Value::Long(0), Value::Object(None)) | (Value::Object(None), Value::Long(0)) => true,
            // Both sides a JNI-smuggled null handle. `ref_operand_is_null`
            // already calls `Value::Long(0)` the null reference, and the arm
            // above says a `Long(0)` equals an `Object(None)` — so leaving this
            // pair out made `if_acmpeq(nullHandle, nullHandle)` answer FALSE
            // while both `if_acmpeq(nullHandle, null)` and `ifnull(nullHandle)`
            // answered TRUE. Equality on the same value has to be reflexive.
            (Value::Long(0), Value::Long(0)) => true,
            _ => false,
        },
        _ => false,
    }
}

pub(crate) fn count_method_params(descriptor: &str) -> usize {
    let mut count = 0;
    let bytes = descriptor.as_bytes();
    let mut i = 1; // skip opening '('

    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => {
                count += 1;
                i += 1;
            }
            b'L' => {
                count += 1;
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() {
                    if bytes[i] == b'L' {
                        while i < bytes.len() && bytes[i] != b';' {
                            i += 1;
                        }
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                count += 1;
            }
            _ => i += 1,
        }
    }

    count
}

// -- Arithmetic helpers --

// T10.9.D — direct CompactValue arithmetic helpers.
//
// pop_int/pop_long/pop_float/pop_double already inspect raw compact slots;
// the push side now builds a CompactValue directly and avoids the
// Value-enum round-trip.

fn int_binop(frame: &mut Frame, op: impl FnOnce(i32, i32) -> i32) -> Result<(), RuntimeError> {
    let b = frame.stack.pop_int()?;
    let a = frame.stack.pop_int()?;
    frame.stack.push_int(op(a, b))
}

fn long_binop(frame: &mut Frame, op: impl FnOnce(i64, i64) -> i64) -> Result<(), RuntimeError> {
    let b = frame.stack.pop_long()?;
    let a = frame.stack.pop_long()?;
    frame.stack.push_long(op(a, b))
}

fn float_binop(frame: &mut Frame, op: impl FnOnce(f32, f32) -> f32) -> Result<(), RuntimeError> {
    let b = frame.stack.pop_float()?;
    let a = frame.stack.pop_float()?;
    frame.stack.push_float(op(a, b))
}

fn double_binop(frame: &mut Frame, op: impl FnOnce(f64, f64) -> f64) -> Result<(), RuntimeError> {
    let b = frame.stack.pop_double()?;
    let a = frame.stack.pop_double()?;
    frame.stack.push_double(op(a, b))
}

// -- Multi-dimensional array allocation --

/// The part of a `multianewarray` site's answer that does not depend on the
/// dimension VALUES: the leaf element type, the descriptor's bracket count, and
/// the per-level component class ids.
///
/// All three are functions of `(referencing class, cp index, dimensions)` only,
/// and that triple is fixed for a given bytecode site. Recomputing them per
/// execution costs a `class_manager` read lock, a `get_class_name`, two `String`
/// builds and up to two loader-aware class resolutions — measured at +227 ns on
/// a 300k-iteration `new String[4][4]` loop (418 → 645 ns/op), which is 55% on
/// top of the allocation itself. Memoized, the site pays that once.
struct MultiANewArrayPlan {
    leaf_et: ArrayElementType,
    total_array_depth: usize,
    /// Component class id per allocated level, outermost first.
    component_ids: Box<[ClassId]>,
}

/// Per-site memo for [`MultiANewArrayPlan`], keyed by
/// `(referencing_class_id, cp_index, dimensions)`.
///
/// Only FULLY resolved plans are inserted (see `plan_is_cacheable`). A level
/// whose component class did not resolve falls back to `ClassId(0)` — the
/// pre-existing behaviour — and caching that would make a transient resolution
/// failure permanent, which is exactly the shape of bug this whole change is
/// fixing.
type MultiANewArrayPlanCache =
    parking_lot::RwLock<rustc_hash::FxHashMap<(u32, u16, u8), Arc<MultiANewArrayPlan>>>;

fn multianewarray_plan_cache() -> &'static MultiANewArrayPlanCache {
    static CACHE: std::sync::OnceLock<MultiANewArrayPlanCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// Drop every memoized [`MultiANewArrayPlan`].
///
/// **Class ids are recycled.** `unload_user_classes` can retire `p/X` under a
/// live loader and let that loader define a fresh `p/X` with the same id — the
/// exact aliasing `ClassManager::array_class_for` guards against by
/// re-synthesising rather than trusting its own cache. A plan holds ids on both
/// sides (the key names the referencing class, the value names each level's
/// component class), so it is invalidated wholesale from the one place that
/// already retires the JIT's other class-keyed caches
/// (`memory::gc`'s unload path). Unloading is rare and batched; refilling a
/// site costs one resolution.
pub(crate) fn invalidate_multianewarray_plans() {
    multianewarray_plan_cache().write().clear();
}

/// Resolve a `multianewarray` site's array class and allocate the array.
///
/// **This is the ONE implementation of JVMS §multianewarray's typing rules in
/// this VM.** The interpreter's `Instruction::Multianewarray` arm and the JIT's
/// `jit_multianewarray_2d` helper are both thin callers of it, and that is
/// deliberate: the two used to be separate transcriptions and only the
/// interpreter's carried the component-class resolution below. The JIT's copy
/// allocated every level with `ClassId(0)`, so a JIT-compiled `new String[a][b]`
/// produced an object whose `getClass()` read back `[Ljava.lang.Object;` — the
/// `DSCompiler.getCompiler` `ClassCastException` witness.
///
/// `sizes` is outermost-first and must be non-empty; its length is the
/// `dimensions` operand, which JVMS §4.9.1 allows to be SMALLER than the
/// referenced array class's bracket count (the unspecified inner dimensions
/// stay null).
pub(crate) fn multianewarray_alloc(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    cp_index: u16,
    sizes: &[usize],
) -> Result<ObjectRef, MethodCallFailed> {
    if sizes.is_empty() {
        return Err(VmError::Internal {
            message: "multianewarray: dimensions must be >= 1".to_string(),
        }
        .into());
    }
    // `dimensions` is a single bytecode operand, so it never exceeds u8::MAX;
    // the guard below rejects anything past the descriptor's bracket count
    // anyway, and 255 is the JVMS ceiling on that.
    let dims_key = u8::try_from(sizes.len()).unwrap_or(u8::MAX);
    let cache_key = (referencing_class_id.as_u32(), cp_index, dims_key);

    // Clone the `Arc` and DROP THE LOCK before allocating. `alloc_multi_array`
    // can trigger a GC, and the GC's own `unload_user_classes` path is what
    // calls `invalidate_multianewarray_plans` — i.e. it takes this lock for
    // WRITE. Holding the read guard across the allocation would let a thread
    // wait for a collection that is waiting for this reader, through a
    // `parking_lot::RwLock` that is neither reentrant nor writer-starving. One
    // refcount bump is the whole cost of not having that edge.
    let hit = multianewarray_plan_cache().read().get(&cache_key).cloned();
    if let Some(plan) = hit {
        return alloc_multi_array(
            shared,
            sizes,
            0,
            plan.leaf_et,
            plan.total_array_depth,
            &plan.component_ids,
        );
    }

    // Resolve the leaf element type AND total array depth from the array class
    // descriptor. The `dimensions` operand may be less than the total `[`
    // count, in which case the unspecified inner dimensions stay null and the
    // deepest *allocated* array must hold references (not the leaf type) — see
    // `alloc_multi_array`.
    let (leaf_et, total_array_depth, leaf_desc) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(referencing_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;
        let array_class_name = class
            .constant_pool
            .get_class_name(cp_index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{cp_index}"),
            })?;
        // Strip leading '[' to find the leaf type descriptor; the count of
        // stripped `[`s is the total array depth.
        let total_depth = array_class_name
            .as_bytes()
            .iter()
            .take_while(|&&b| b == b'[')
            .count();
        let leaf = &array_class_name.as_bytes()[total_depth..];
        let et = match leaf.first() {
            Some(b'I') => ArrayElementType::Int,
            Some(b'J') => ArrayElementType::Long,
            Some(b'F') => ArrayElementType::Float,
            Some(b'D') => ArrayElementType::Double,
            Some(b'B') => ArrayElementType::Byte,
            Some(b'C') => ArrayElementType::Char,
            Some(b'S') => ArrayElementType::Short,
            Some(b'Z') => ArrayElementType::Boolean,
            _ => ArrayElementType::Reference,
        };
        (et, total_depth, array_class_name[total_depth..].to_string())
    };

    // JVMS §4.9.1 static constraint: `dimensions` must not exceed the number of
    // leading `[` in the referenced array class.
    //
    // SECURITY (defense-in-depth, same policy as `execute_ldc`'s
    // `ClassFormatError` conversion): the type-state verifier does enforce this
    // (`verify_insn.rs`, `Instruction::Multianewarray`), but that pass does NOT
    // run for every class. Pass 3 is deferred wholesale for any class defined
    // by a user-defined loader while `loader_aware_resolution()` is on — which
    // is the default, and covers every Spring / WildFly / H2 application class
    // — and the structural-only substitute
    // (`verifier::verify_method_structural`) never looks at this operand.
    // `-Xverify:none` removes it too.
    //
    // Without this guard `total_array_depth - d - 1` below underflows: in a
    // release build (overflow-checks off) it wraps to `usize::MAX`, and
    // `"[".repeat(usize::MAX)` then asks the allocator for `usize::MAX` bytes,
    // which aborts the process rather than raising anything Java can catch.
    // It sits in front of the cache insert as well as the subtraction: a site
    // that throws here must throw every time, never be memoized into a plan.
    if sizes.len() > total_array_depth {
        let (cls, mname) = match thread.frames.last() {
            Some(f) => (f.class_name().to_string(), f.method_name().to_string()),
            None => (String::new(), String::new()),
        };
        return Err(crate::runtime::exceptions::throw_linkage_error(
            shared,
            thread,
            LinkageError::VerifyError {
                class_name: cls,
                method_name: mname,
                message: format!(
                    "multianewarray: dimensions {} exceeds array bracket count {} \
                     of type at cp#{cp_index}",
                    sizes.len(),
                    total_array_depth
                ),
            },
        ));
    }

    // Resolve the *component* class id for each allocated array level so the
    // array objects carry their precise class (e.g. the outer level of
    // `new String[8][8]` is a `[[Ljava/lang/String;` whose component is
    // `[Ljava/lang/String;`). Without this every multi-dim array was allocated
    // with `ClassId(0)` and `getClass().getName()` collapsed to
    // `[Ljava/lang/Object;`. Each level d's component descriptor is
    // `[`×(total_depth-d-1) followed by the leaf descriptor; a primitive leaf
    // (`I`, `C`, …) needs no class (the element type drives naming).
    let leaf_is_reference = leaf_desc.starts_with('L') && leaf_desc.ends_with(';');
    let mut component_ids: Vec<ClassId> = Vec::with_capacity(sizes.len());
    // Every level except a primitive leaf names a class that must resolve; if
    // any did not, the plan is a fallback and must not be memoized.
    let mut fully_resolved = true;
    for d in 0..sizes.len() {
        let comp_brackets = total_array_depth - d - 1;
        let cid = if comp_brackets > 0 {
            // Component is itself an array class — resolve `[…`.
            //
            // JVMS §5.3.3: that inner array class is defined by the defining
            // loader of ITS component, so it must be resolved loader-faithfully.
            // This `ClassId` is stamped into the allocated array object's header
            // and is what a later `getClass()` / `getComponentType()` reads back,
            // so collapsing two loaders' `[Lp/X;` here would make
            // `new p.X[2][2]` report the wrong loader's element type.
            let comp_desc = format!("{}{}", "[".repeat(comp_brackets), leaf_desc);
            resolve_class_or_array_loader_aware(shared, thread, referencing_class_id, &comp_desc)
                .unwrap_or(ClassId::new(0))
        } else if leaf_is_reference {
            // Reference leaf — component is the element class itself.
            let comp_name = &leaf_desc[1..leaf_desc.len() - 1];
            resolve_class_loader_aware(shared, thread, referencing_class_id, comp_name)
                .unwrap_or(ClassId::new(0))
        } else {
            // Primitive leaf: element type carries the descriptor.
            ClassId::new(0)
        };
        if cid == ClassId::new(0) && (comp_brackets > 0 || leaf_is_reference) {
            fully_resolved = false;
        }
        component_ids.push(cid);
    }

    if fully_resolved {
        multianewarray_plan_cache().write().insert(
            cache_key,
            Arc::new(MultiANewArrayPlan {
                leaf_et,
                total_array_depth,
                component_ids: component_ids.clone().into_boxed_slice(),
            }),
        );
    }

    alloc_multi_array(shared, sizes, 0, leaf_et, total_array_depth, &component_ids)
}

/// Maximum recursion depth for multianewarray to prevent stack overflow.
/// The JVM spec allows at most 255 dimensions, but we cap at 255 to be safe.
const MAX_MULTI_ARRAY_DEPTH: usize = 255;

fn alloc_multi_array(
    shared: &SharedVm,
    sizes: &[usize],
    depth: usize,
    leaf_et: ArrayElementType,
    total_array_depth: usize,
    component_ids: &[ClassId],
) -> Result<ObjectRef, MethodCallFailed> {
    // Component class id for the array allocated at this depth (so it carries
    // its precise array class). Falls back to `ClassId(0)` when unresolved.
    let level_class_id = component_ids.get(depth).copied().unwrap_or(ClassId::new(0));
    if depth >= MAX_MULTI_ARRAY_DEPTH {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NotImplemented {
                feature: format!(
                    "multianewarray: dimension depth {} exceeds maximum {}",
                    depth, MAX_MULTI_ARRAY_DEPTH
                ),
            },
        )));
    }

    let length = sizes[depth];

    if depth == sizes.len() - 1 {
        // Innermost SPECIFIED dimension. Per JVM spec for multianewarray, the
        // array type descriptor can have more leading `[` than the `dimensions`
        // operand; unspecified inner dimensions are left null/uninitialized.
        // So the last-allocated array's element type is the descriptor's leaf
        // type ONLY when `sizes.len() == total_array_depth`. Otherwise the
        // element type must be Reference (each slot would hold an array-of-
        // arrays-of-...-of-leaf that we are NOT instantiating here).
        //
        // Example: `multianewarray [[[[C, 3` (descriptor depth=4, sizes.len()=3)
        // → outer Ref[N], mid Ref[N], inner Ref[N] (null slots). Using `Char`
        // here would build a `char[N]` leaf and crash on the subsequent
        // `aaload`/`aastore` that walks the unallocated 4th dim
        // (e.g. Eclipse ecj `CharDeduplication.charArray_length`).
        let element_type = if sizes.len() == total_array_depth {
            leaf_et
        } else {
            ArrayElementType::Reference
        };
        // SB-LOADER-ZIPCONTENT (2026-08-04): `_full`, not the young-only
        // variant. `alloc_multi_array` holds already-allocated dimension arrays
        // in Rust locals across the recursion, so it deliberately never forces
        // a GC — which leaves it with no second chance at all. Spilling into
        // old gen is that second chance, and it relocates nothing (see
        // `try_alloc_array_humongous`), so it is safe from exactly here.
        let arr = shared
            .mem
            .heap
            .try_alloc_array_full(level_class_id, element_type, length)
            .ok_or_else(|| {
                MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                    message: format!(
                        "Java heap space (multianewarray leaf dim, length={})",
                        length
                    ),
                }))
            })?;
        Ok(arr)
    } else {
        // Intermediate dimensions: always Reference (array of arrays)
        // `_full` for the same reason as the leaf dimension above.
        let arr = shared
            .mem
            .heap
            .try_alloc_array_full(level_class_id, ArrayElementType::Reference, length)
            .ok_or_else(|| {
                MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                    message: format!(
                        "Java heap space (multianewarray dim {}, length={})",
                        depth, length
                    ),
                }))
            })?;
        for i in 0..length {
            let sub_array = alloc_multi_array(
                shared,
                sizes,
                depth + 1,
                leaf_et,
                total_array_depth,
                component_ids,
            )?;
            shared
                .mem
                .heap
                .set_array_element(arr, i, Value::Object(Some(sub_array)))
                .map_err(|idx| {
                    RuntimeError::aioobe(idx, shared.mem.heap.array_length(arr) as i32)
                })?;
        }
        Ok(arr)
    }
}

// -- Float/double to int/long conversions (JVM spec 2.8.3) --

fn float_to_int(v: f32) -> i32 {
    if v.is_nan() {
        0
    // JVM spec: f2i bounds check
    } else if v >= i32::MAX as f32 {
        i32::MAX
    // JVM spec: f2i bounds check
    } else if v <= i32::MIN as f32 {
        i32::MIN
    } else {
        v as i32 // JVM spec: bounded float-to-int conversion
    }
}

fn float_to_long(v: f32) -> i64 {
    if v.is_nan() {
        0
    // JVM spec: f2l bounds check
    } else if v >= i64::MAX as f32 {
        i64::MAX
    // JVM spec: f2l bounds check
    } else if v <= i64::MIN as f32 {
        i64::MIN
    } else {
        v as i64 // JVM spec: bounded float-to-long conversion
    }
}

fn double_to_int(v: f64) -> i32 {
    if v.is_nan() {
        0
    // JVM spec: d2i bounds check
    } else if v >= i32::MAX as f64 {
        i32::MAX
    // JVM spec: d2i bounds check
    } else if v <= i32::MIN as f64 {
        i32::MIN
    } else {
        v as i32 // JVM spec: bounded float-to-int conversion
    }
}

fn double_to_long(v: f64) -> i64 {
    if v.is_nan() {
        0
    // JVM spec: d2l bounds check
    } else if v >= i64::MAX as f64 {
        i64::MAX
    // JVM spec: d2l bounds check
    } else if v <= i64::MIN as f64 {
        i64::MIN
    } else {
        v as i64 // JVM spec: bounded float-to-long conversion
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// DBG (`CRATONVM_DBG_IMSE`): dump the complete `ReentrantReadWriteLock$Sync`
/// read-hold bookkeeping at the moment an `IllegalMonitorStateException` is
/// thrown from `tryReleaseShared` — names WHICH invariant broke (firstReader
/// identity, cachedHoldCounter tid/count, or the current thread's `readHolds`
/// ThreadLocalMap entry) for the ES `testAllEqual` hold-count-loss face.
fn dump_imse_holdcount_state(shared: &SharedVm, thread: &JvmThread, exc: ObjectRef) {
    let exc_class = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(shared.mem.heap.class_id_of(exc))
            .map(|c| c.name.to_string())
            .unwrap_or_default()
    };
    if !exc_class.contains("IllegalMonitorStateException") {
        return;
    }
    // Find the tryReleaseShared frame; local 0 is the Sync receiver.
    let sync = thread.frames.iter().rev().find_map(|f| {
        if f.method_name() == "tryReleaseShared" && f.class_name().contains("ReadWriteLock") {
            match f.get_local(0) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            }
        } else {
            None
        }
    });
    let Some(sync) = sync else {
        eprintln!("[imse] IllegalMonitorStateException thrown but no tryReleaseShared frame");
        return;
    };
    let cm = shared.classes.class_manager.read();
    let field = |obj: ObjectRef, name: &str| -> Value {
        let cid = shared.mem.heap.class_id_of(obj);
        match crate::vm::vm_exec::resolve_field_index_in_hierarchy(cid, name, &cm.class_store) {
            Some(idx) => shared.mem.heap.get_field(obj, idx),
            None => Value::Uninitialized,
        }
    };
    let me = thread.java_thread_obj;
    let first_reader = field(sync, "firstReader");
    let first_count = field(sync, "firstReaderHoldCount");
    let cached = field(sync, "cachedHoldCounter");
    let read_holds = field(sync, "readHolds");
    eprintln!(
        "[imse] tid={} me={:?} sync={:p} firstReader={:?} firstReaderHoldCount={:?}",
        thread.thread_id.0,
        me.map(|o| o.as_ptr()),
        sync.as_ptr(),
        first_reader,
        first_count,
    );
    if let Value::Object(Some(rh)) = cached {
        eprintln!(
            "[imse]   cachedHoldCounter={:p} count={:?} tid={:?}",
            rh.as_ptr(),
            field(rh, "count"),
            field(rh, "tid"),
        );
    } else {
        eprintln!("[imse]   cachedHoldCounter={cached:?}");
    }
    // Walk the CURRENT thread's ThreadLocalMap for the readHolds entry.
    let Value::Object(Some(read_holds)) = read_holds else {
        eprintln!("[imse]   readHolds={read_holds:?} (field missing?)");
        return;
    };
    let Some(me) = me else {
        eprintln!("[imse]   no java_thread_obj for current thread");
        return;
    };
    let tl_map = field(me, "threadLocals");
    let Value::Object(Some(tl_map)) = tl_map else {
        eprintln!(
            "[imse]   readHolds={:p} but thread has NO threadLocals map",
            read_holds.as_ptr()
        );
        return;
    };
    let table = field(tl_map, "table");
    let Value::Object(Some(table)) = table else {
        eprintln!(
            "[imse]   threadLocals map {:p} has NO table",
            tl_map.as_ptr()
        );
        return;
    };
    let len = shared.mem.heap.array_length(table);
    let mut found = false;
    for i in 0..len {
        let Ok(Value::Object(Some(entry))) = shared.mem.heap.get_array_element(table, i) else {
            continue;
        };
        // Entry extends WeakReference<ThreadLocal>; referent is field 0.
        let referent = shared.mem.heap.get_field(entry, 0);
        let is_ours =
            matches!(referent, Value::Object(Some(r)) if r.as_ptr() == read_holds.as_ptr());
        if is_ours {
            found = true;
            let value = field(entry, "value");
            let vdesc = if let Value::Object(Some(hc)) = value {
                format!(
                    "HoldCounter@{:p} count={:?} tid={:?}",
                    hc.as_ptr(),
                    field(hc, "count"),
                    field(hc, "tid"),
                )
            } else {
                format!("{value:?}")
            };
            eprintln!(
                "[imse]   readHolds={:p}: table[{i}] entry={:p} value={vdesc}",
                read_holds.as_ptr(),
                entry.as_ptr(),
            );
        }
    }
    if !found {
        eprintln!(
            "[imse]   readHolds={:p}: NO ThreadLocalMap entry in current thread's table (len={len}) — the entry is GONE",
            read_holds.as_ptr(),
        );
    }
}

/// G13-1 (2026-08-17) — the Path B interface-substitution table, and the
/// property that makes an ARRAY receiver a refusal rather than a guess.
///
/// These live inline rather than in `interpreter/tests.rs` because this lane
/// owns exactly one file. They are pure-function tests: nothing here builds a
/// VM, so they run in milliseconds and cannot flake.
#[cfg(test)]
mod g13_array_receiver_tests {
    use super::canonical_concrete_for_interface;

    /// The six names Path B substitutes for, pinned. If a lane adds a seventh,
    /// this test is where it has to say so — and the array refusal at the call
    /// site picks it up automatically, because it reads the same function.
    #[test]
    fn the_substituted_interfaces_are_exactly_these_six() {
        let mapped: Vec<(&str, &str)> = [
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/util/List",
            "java/util/Map",
            "java/util/Iterator",
        ]
        .into_iter()
        .map(|i| (i, canonical_concrete_for_interface(i)))
        .collect();
        assert_eq!(
            mapped,
            vec![
                ("java/util/Set", "java/util/HashSet"),
                ("java/util/Collection", "java/util/HashSet"),
                ("java/lang/Iterable", "java/util/ArrayList"),
                ("java/util/List", "java/util/ArrayList"),
                ("java/util/Map", "java/util/HashMap"),
                ("java/util/Iterator", "java/util/HashMap$KeyItr"),
            ]
        );
    }

    /// The load-bearing absence. An array type implements `Cloneable` and
    /// `java.io.Serializable` and nothing else (JLS 4.10.3). If either ever
    /// gained a canonical mapping, "canonical is non-empty AND receiver is an
    /// array" would stop being a proof of `IncompatibleClassChangeError` — so
    /// the refusal's soundness is asserted here, not just commented.
    #[test]
    fn the_two_interfaces_an_array_really_implements_are_never_substituted() {
        assert_eq!(canonical_concrete_for_interface("java/lang/Cloneable"), "");
        assert_eq!(canonical_concrete_for_interface("java/io/Serializable"), "");
    }

    /// The refusal must not swallow ordinary `Object` methods on an array.
    /// `clone`/`hashCode`/`getClass` on an `Object[]` are legal and reach the
    /// no-`Code` arm through `java/lang/Object`, which maps to nothing — so
    /// the array check cannot fire for them.
    #[test]
    fn object_and_unrelated_types_are_not_substituted() {
        for name in [
            "java/lang/Object",
            "java/util/Map$Entry",
            "java/util/SequencedCollection",
            "java/net/http/HttpRequest",
            "java/util/stream/Stream",
            "",
        ] {
            assert_eq!(
                canonical_concrete_for_interface(name),
                "",
                "{name} must not be substituted"
            );
        }
    }

    /// The exact triple this record was written about:
    /// `java/util/Map.isEmpty()Z` on an `Object[]` receiver. `java/util/Map`
    /// is substituted, so an array receiver at that site is refused.
    ///
    /// MEASURED counterpart: `[CANONICAL] java/util/Map java/util/HashMap
    /// isEmpty 1` — the only row the whole 105-class corpus produces, in
    /// either policy mode.
    #[test]
    fn the_map_is_empty_triple_is_the_substituted_one() {
        assert_eq!(
            canonical_concrete_for_interface("java/util/Map"),
            "java/util/HashMap"
        );
        assert!(!canonical_concrete_for_interface("java/util/Map").is_empty());
    }
}

#[cfg(test)]
mod wave1_adoption_tests;

#[cfg(test)]
mod tests;
