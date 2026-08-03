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
pub fn weakref_null_referents_pre_gc(shared: &SharedVm) {
    if !weakref_clear_enabled() {
        return;
    }
    let pairs = {
        let rp = shared.mem.ref_processor.lock();
        rp.weak_phantom_active_pairs()
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
    if pairs.is_empty() {
        return;
    }
    if dbg_weakref() {
        eprintln!(
            "[weakref] pre-gc null pass: {} weak/phantom referent(s)",
            pairs.len()
        );
    }
    for (ref_obj_addr, _referent) in pairs {
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
        if shared.mem.heap.num_fields(ref_obj) >= 2 {
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

/// DBG (CRATONVM_DBG_MTROOTS): publish the in-progress collection's context
/// (reason / initiator thread / blocked-thread count) so the sweep-zero detector
/// can name WHICH GC reclaimed a live object — pinning the initiator-vs-blocked
/// root-coverage gap. Reason: 1=System.gc, 2=alloc-young, 3=forced-alloc.
#[inline]
fn mtroots_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MTROOTS").is_some())
}

#[inline]
fn mtroots_set_gc_ctx(shared: &SharedVm, thread: &JvmThread, reason: u8) {
    if mtroots_on() {
        cratonvm_gc::gen_heap::set_gc_context(
            reason,
            // Widening: smaller value -> u32 (value fits)
            thread.thread_id.0 as u32,
            // Widening: smaller value -> u32 (value fits)
            shared.mem.gc_barrier.blocked_count() as u32,
        );
    }
}

/// DBG (CRATONVM_DBG_MTROOTS): dump the GC initiator's frames + their
/// object-local heap addresses, so a later `[sweep-zero] RECLAIMED-LIVE ptr=X`
/// can be cross-referenced — was X actually present as a scanned root here?
/// (Cross-reference within ONE run: addresses are run-specific.)
fn mtroots_dump_initiator(shared: &SharedVm, thread: &JvmThread, reason: u8) {
    if !mtroots_on() {
        return;
    }
    let mut buf = String::new();
    use std::fmt::Write as _;
    let _ = write!(
        buf,
        "[mtroots] GC reason={} initiator_tid={} alive={} blocked={} frames={}",
        reason,
        thread.thread_id.0,
        shared.threads.thread_registry.alive_count(),
        shared.mem.gc_barrier.blocked_count(),
        thread.frames.len(),
    );
    // Top frames only (the relevant Java call site).
    for frame in thread.frames.iter().rev().take(8) {
        let mut objs: Vec<ObjectRef> = Vec::new();
        frame.scan_local_objects(&mut objs, &shared.mem.heap);
        let _ = write!(
            buf,
            "\n[mtroots]   {}.{} locals=[",
            frame.class_name(),
            frame.method_name()
        );
        for (i, o) in objs.iter().enumerate() {
            if i > 0 {
                let _ = write!(buf, " ");
            }
            let _ = write!(buf, "{:p}", o.as_ptr());
        }
        let _ = write!(buf, "]");
    }
    // Per-thread blocked-state census: a thread holding a live oop while counted
    // BLOCKED here is excluded from `expected` and its (possibly stale) deposit
    // snapshot is used instead of its current frames — the suspected gap.
    let states = shared.threads.thread_registry.dump_blocked_states();
    let nblk = states.iter().filter(|(_, b, _)| *b).count();
    let _ = write!(
        buf,
        "\n[mtroots]   thread-census ({} alive, {} in_blocked):",
        states.len(),
        nblk
    );
    for (tid, blk, snap) in states {
        let _ = write!(buf, " t{}{}({})", tid, if blk { "B" } else { "R" }, snap);
    }
    eprintln!("{buf}");
}

/// DBG (CRATONVM_DBG_MTROOTS): after a thread resumes from an STW, scan its OWN
/// frames for object refs whose header is all-zero — i.e. an object the sweep
/// RECLAIMED while this thread still references it (a missed-root reclamation),
/// naming the holder thread + its blocked state + the method, for ANY failure
/// mode (checkcast CCE / SIGSEGV / invokevirtual), not just the invokevirtual
/// all-zero-receiver detector.
fn mtroots_selfcheck(thread: &JvmThread, heap: &crate::memory::VmHeap, location: &str) {
    if !mtroots_on() {
        return;
    }
    let blocked = thread
        .gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire);
    for frame in thread.frames.iter().rev() {
        let mut objs: Vec<ObjectRef> = Vec::new();
        frame.scan_local_objects(&mut objs, heap);
        for o in objs {
            // SAFETY: `o` is a live ObjectRef scanned from this frame; its pointer is a valid heap address with at least a 16-byte readable header.
            let hdr: [u8; 16] = unsafe { std::ptr::read(o.as_ptr() as *const [u8; 16]) };
            if hdr == [0u8; 16] {
                eprintln!(
                    "[mtroots] SELFCHECK@{} tid={} in_blocked={} kind={:?} method={}.{} \
                     holds RECLAIMED(all-zero) obj @{:p}",
                    location,
                    thread.thread_id.0,
                    blocked,
                    thread.kind,
                    frame.class_name(),
                    frame.method_name(),
                    o.as_ptr(),
                );
            }
        }
    }
}

/// BUG-03 — drive the cross-thread STW JIT root scan for a multi-threaded GC
/// initiator.
///
/// A thread spinning in JIT-compiled code never cooperatively reaches an
/// interpreter safepoint, so `wait_for_all` would either hang on it or (when
/// it slips through) let the collector mark off its stale `root_snapshot` and
/// reclaim a still-live object → SIGSEGV. This forcibly stops every in-JIT
/// peer at the OS level, conservatively scans its registers + stack into
/// `xt_roots` (pinned, since the frozen peers keep the sweep non-moving), and
/// excludes them from the barrier so the remaining cooperative mutators can
/// be waited for normally. The returned [`TakenOver`] must be `resume`d once
/// the collection has completed.
///
/// When the feature is disabled (`CRATONVM_XT_JIT_ROOT_SCAN=0`) or unsupported
/// for the current heap, this is byte-for-byte the legacy `wait_for_all()` —
/// zero behaviour change.
///
/// Perf/starvation fix (2026-07-13 — elinjsp-socket-read-timeout,
/// stw-crossthread-jit-takeover-hang-cluster, wildfly-standalone-boot-stw-
/// jit-takeover-hang): the per-round `take_over_pass` OS-level scan (Windows:
/// a full `CreateToolhelp32Snapshot` plus per-peer `OpenThread`/
/// `SuspendThread`/`GetThreadContext`/`ResumeThread`; Linux: a signal-and-wait
/// per peer) is expensive, and suspending/resuming every peer thread —
/// including the exact mutator this loop is waiting for — competes with that
/// peer for scheduler time. Previously this ran unconditionally on every 1ms
/// tick once the loop had spun even once (`rounds != 0`), regardless of
/// whether anything was actually in JIT, for as long as the wait continued —
/// a self-amplifying livelock where the longer the wait takes, the more it
/// starves the very thread it is waiting for. Confirmed empirically: a
/// single JSP-compile GC pause with exactly one pending (non-JIT,
/// non-blocked) mutator took several minutes, logging hundreds of thousands
/// of "0 newly taken over" scans, one per millisecond, before the mutator
/// (itself just slow to reach its own next safepoint under the induced
/// scheduling pressure) finally arrived.
fn stw_takeover_should_scan(rounds: u32, jit_hint: bool) -> bool {
    // Scan every round for the first FAST_SCAN_ROUNDS — preserves
    // zero-added-latency behavior for the common case, where a genuinely
    // in-JIT peer is taken over within single-digit milliseconds — then back
    // off geometrically. A peer that enters JIT during the slow phase is
    // still guaranteed to be found; it just carries up to one
    // SLOW_SCAN_PERIOD/VERY_SLOW_SCAN_PERIOD round of added detection
    // latency, negligible next to a stall already long enough to reach that
    // phase, while cutting steady-state OS-call volume (and the
    // peer-starvation feedback loop) by 1-2 orders of magnitude.
    //
    // Round 0 is deliberately left gated on the cheap `any_thread_in_jit()`
    // hint alone (unchanged from before), so an ordinary, fully-cooperative
    // GC pause that never needed a scan at all still does not pay for one.
    // The hint is NOT used to gate rounds >= 1: that counter is a single
    // process-global depth and cannot distinguish "a peer is in JIT" from "I
    // am" — this function's caller is commonly reached via `maybe_gc` called
    // from JIT-compiled code, so the initiator's own live JIT-entry guard can
    // hold the hint permanently true regardless of any peer's actual state.
    const FAST_SCAN_ROUNDS: u32 = 20;
    const SLOW_SCAN_PERIOD: u32 = 20;
    const VERY_SLOW_SCAN_ROUNDS: u32 = 500;
    const VERY_SLOW_SCAN_PERIOD: u32 = 200;
    if rounds == 0 {
        jit_hint
    } else if rounds < FAST_SCAN_ROUNDS {
        true
    } else if rounds < VERY_SLOW_SCAN_ROUNDS {
        rounds % SLOW_SCAN_PERIOD == 0
    } else {
        rounds % VERY_SLOW_SCAN_PERIOD == 0
    }
}

fn stw_take_over_and_wait(
    shared: &SharedVm,
    xt_roots: &mut Vec<ObjectRef>,
    counted_os_tids: &[u32],
) -> crate::jit::xt_root_scan::TakenOver {
    use crate::jit::xt_root_scan as xt;
    // The forcible take-over is only sound on a heap that can collect while a
    // frozen peer holds an un-retired TLAB and un-rewritable roots:
    // Generational degrades to the non-moving sweep that consumes the JIT
    // TLAB skip regions; G1 (INT-3) skips the published tails in every region
    // walker and pins everything a frozen peer can address out of the CSet
    // (see `pin_frozen_peer_roots_for_g1`); ZGC (INT-3 residual) is trivially
    // safe — non-moving, registry-walked sweep, and its mutators never hold
    // TLABs. The `supports_jit_tlab_skip` gate is retained for any future
    // backend that can't make one of those arguments.
    if !xt::enabled() || !shared.mem.heap.supports_jit_tlab_skip() {
        shared.mem.gc_barrier.wait_for_all();
        return xt::TakenOver::default();
    }
    let mut taken = xt::TakenOver::default();
    // Take over currently-in-JIT peers, then wait briefly for the remaining
    // cooperative mutators. If the wait does not complete, scan again: a peer can
    // enter JIT after the previous takeover pass and would otherwise never
    // arrive at the barrier. Keep looping until the barrier is satisfied.
    const WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(1);
    const WARN_AFTER_ROUNDS: u32 = 64;
    // See `stw_takeover_should_scan`'s doc for why the scan cadence backs off
    // instead of running unconditionally on every round.
    let mut rounds = 0u32;
    let mut warned = false;
    loop {
        let tids_before = taken.tids.len();
        let should_scan =
            stw_takeover_should_scan(rounds, crate::jit::conservative_roots::any_thread_in_jit());
        let newly = if should_scan {
            xt::take_over_pass(
                &mut taken,
                &|a| shared.mem.heap.is_object_address(a),
                xt_roots,
            )
        } else {
            0
        };
        if newly > 0 {
            // xt-hardening (2026-07-03): identity-based excusal. Only excuse
            // frozen peers that were actually COUNTED in the barrier's
            // `expected` (the OS-tid snapshot is taken atomically with the
            // expected computation, under the same registry lock). Excusing
            // an uncounted newcomer — a thread spawned after the snapshot
            // that reached JIT code during the takeover loop — over-reduces
            // `expected`, releasing the barrier while a genuinely counted
            // mutator still runs: mutation concurrent with mark+sweep. An
            // uncounted frozen peer stays frozen and scanned but excuses
            // nobody (the barrier never expected it).
            let mut excuse = 0u32;
            for &tid in &taken.tids[tids_before..] {
                if counted_os_tids.contains(&tid) {
                    excuse += 1;
                } else {
                    static N: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                        tracing::warn!(
                            os_tid = tid,
                            "xt takeover froze an uncounted newcomer thread — \
                             scanned but not excused from the barrier"
                        );
                    }
                }
            }
            if excuse > 0 {
                shared.mem.gc_barrier.reduce_expected(excuse);
            }
        }
        rounds += 1;
        if shared.mem.gc_barrier.wait_for_all_timeout(WAIT_SLICE) {
            break;
        }
        if !warned && rounds >= WARN_AFTER_ROUNDS {
            let pending = shared.mem.gc_barrier.pending_count();
            tracing::warn!(
                rounds,
                pending,
                taken = taken.count(),
                "STW cross-thread JIT takeover is still waiting for cooperative mutators"
            );
            // GCBARRIER-LIVELOCK-FIX (2026-07-18) tripwire: cross-check the
            // barrier's legacy `threads_blocked` atomic (bumped by any
            // `GcBarrier::enter_blocked()` / `mark_blocked_region_enter()`
            // call) against the registry's authoritative `in_blocked_region`
            // census — the ONLY signal the production `expected` computation
            // (`request_stw_counted_with_live_blocked` /
            // `alive_count_blocked_and_os_tids`) actually excludes threads
            // on. A caller that reaches `enter_blocked()` without first
            // depositing a root snapshot (`in_blocked_region` stays false)
            // bumps the legacy counter but stays invisible to the census —
            // silently inflating `expected` by one uncounted mutator that can
            // never arrive. This is the exact shape of a livelock fixed at
            // this date in `vm/src/vm/vm_exec.rs`'s thread-termination
            // "notify waiting joiners" block (a `block_enter()` call missing
            // the `deposit_root_snapshot()` its own doc comment requires).
            // Always-on (not gated behind CRATONVM_DBG_STW_CENSUS) because it
            // only runs once takeover is already stuck for 64+ rounds — a
            // rare, already-anomalous path — and a mismatch here is the
            // single fastest signal to root-cause a recurrence of this bug
            // class at any OTHER call site.
            let (census_alive, census_blocked, _tids, _blocked_tids) = shared
                .threads
                .thread_registry
                .alive_count_blocked_and_os_tids();
            let legacy_blocked = shared.mem.gc_barrier.blocked_count() as usize;
            if legacy_blocked > census_blocked {
                eprintln!(
                    "[gcbarrier-tripwire] legacy blocked_count()={legacy_blocked} > \
                     census in_blocked_region count={census_blocked} (alive={census_alive}) \
                     -- a thread called GcBarrier::enter_blocked()/mark_blocked_region_enter() \
                     WITHOUT first depositing a root snapshot, so it is invisible to the \
                     production STW census but still occupies an `expected` slot no arrival \
                     can ever satisfy. Set CRATONVM_DBG_STW_CENSUS=1 for a full per-thread dump.\n{}",
                    shared.threads.thread_registry.debug_thread_census()
                );
            }
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_CENSUS").is_some()
                || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_XT_JIT_ROOT_SCAN").is_some()
            {
                eprintln!(
                    "[stw-census] rounds={rounds} pending={pending} taken={} blocked={} alive={}{}",
                    taken.count(),
                    shared.mem.gc_barrier.blocked_count(),
                    shared.threads.thread_registry.alive_count(),
                    shared.threads.thread_registry.debug_thread_census()
                );
                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_NATIVE_RING").is_some() {
                    cratonvm_native_api::native_ring::dump_to_stderr();
                }
            }
            warned = true;
        }
    }
    // XT-FRAME-SCAN fix (2026-07-22, stw-residual-close): a frozen in-JIT
    // peer's interpreter frames live in Rust Vecs (`JvmThread::frames`) —
    // invisible to the conservative register/native-stack scan — and its
    // deposited `root_snapshot` is only as fresh as its last publish
    // (safepoint arrival, block-enter). Any ref pushed onto an interpreter
    // operand stack (or stored into a local) after that publish and before
    // entering compiled code was therefore missing from the mark roots for
    // the whole frozen window, and the non-moving sweep zeroed the still-live
    // object in place (WildFly parallel-extension-add: fresh
    // StringBuilder/Reader receivers reading back all-zero — see
    // docs/internal/fixed-suite-bugs/wildfly/wildfly-interpreter-operand-stack-slot-stale-after-nested-alloc-FIXED.md).
    // Walk each frozen peer's interpreter frames directly into `xt_roots`.
    //
    // SAFETY: each address was published by its owning thread with the
    // `tlab_addr` discipline (stable for the thread's life, cleared before
    // the `JvmThread` drops), and the peer is OS-frozen with `Rip` inside
    // registered compiled code (`take_over_pass` freezes nothing else).
    // Compiled code never mutates the `JvmThread`'s interpreter state — the
    // code paths that do (interpreter, JIT helpers) put `Rip` outside every
    // registered range — and frozen peers are resumed only after the
    // collection completes (`xt_root_scan::resume`), so the reference is
    // dropped before any mutation can resume. A frozen thread also cannot
    // exit, so the entry stays alive and the `JvmThread` cannot drop.
    // Contributed roots go into `xt_roots`, which already forces the
    // non-moving sweep (Generational) / region pinning (G1) for this cycle,
    // so a stale or dead value can only over-retain — never relocate or
    // corrupt.
    if taken.count() > 0 {
        let peer_addrs = shared
            .threads
            .thread_registry
            .frozen_peer_thread_addrs(&taken.tids);
        let contributed_from = xt_roots.len();
        for addr in peer_addrs {
            // SAFETY: see the block comment above.
            let peer: &crate::threading::jvm_thread::JvmThread =
                unsafe { &*(addr as *const crate::threading::jvm_thread::JvmThread) };
            for frame in peer.frames.iter() {
                frame.scan_local_objects(xt_roots, &shared.mem.heap);
                let sb = xt_roots.len();
                frame.stack.scan_object_refs(xt_roots, &shared.mem.heap);
                if xt_roots.len() > sb {
                    // Operand-stack candidates are validated strictly, exactly
                    // like the deposit path (`scan_frame_roots`): a pointer-
                    // shaped primitive long must not become a root.
                    let added = xt_roots.split_off(sb);
                    for o in added {
                        if shared
                            .mem
                            .heap
                            .is_object_address(o.as_ptr() as usize)
                            .is_some()
                        {
                            xt_roots.push(o);
                        }
                    }
                }
                if let Some(m) = frame.monitor_on_exit {
                    xt_roots.push(m);
                }
            }
            for r in peer.native_pin_roots.iter() {
                xt_roots.push(*r);
            }
            if let Some(r) = peer.native_pending_return {
                xt_roots.push(r);
            }
            // TOMCAT-JNDIREALM-JIT.3 — a frozen peer's per-thread native
            // caches live in NO frame, so the frame walk above cannot reach
            // them. A peer taken over mid-JIT is exactly a thread whose
            // deposited snapshot is stale, so these must be contributed here
            // too (same set as `deposit_root_snapshot_inner` publishes).
            for entry in peer.jit_hashmap_string_node_cache.iter() {
                xt_roots.push(entry.map);
                xt_roots.push(entry.node);
                if let Some(key_object) = entry.key_object {
                    xt_roots.push(key_object);
                }
            }
            for entry in peer.string_case_cache.iter() {
                xt_roots.push(entry.source);
                xt_roots.push(entry.first);
                xt_roots.push(entry.second);
                if let Some(locale) = entry.locale {
                    xt_roots.push(locale);
                }
            }
        }
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_XT_JIT_ROOT_SCAN").is_some() {
            eprintln!(
                "[xt-frame-scan] frozen_peers={} contributed_roots={}",
                taken.count(),
                xt_roots.len() - contributed_from
            );
        }
    }

    // A4 (fork6-fjp) — helper-window coverage. The barrier is satisfied, so
    // every remaining un-scanned root holder is a BLOCKED thread (excluded via
    // `threads_blocked`, covered only by its `deposit_root_snapshot`, which
    // never scans JIT frames). A worker blocked in `join()`/park under
    // JIT-compiled `runWorker`/`doExec` frames that are the sole holder of a
    // forked subtask would otherwise lose it to the non-moving sweep (the
    // Fork6 stale all-zero receivers). Scan each remaining peer's register
    // file + used stack once, contributing roots only when the stack actually
    // carries a JIT return address. Gated to collections where the gap can
    // exist at all: a blocked thread while some thread holds live JIT frames.
    let mut helper_windows = 0usize;
    if xt::helper_window_scan_enabled()
        && shared.mem.gc_barrier.blocked_count() > 0
        && crate::jit::conservative_roots::any_thread_in_jit()
    {
        // xt-hardening follow-up (2026-07-03): scope the pass to threads
        // ACTUALLY in a blocked region (the only gap it exists to close —
        // see helper_window_pass's doc comment). A cooperatively-arrived
        // mutator already published its JIT roots via update_root_snapshot;
        // re-scanning it only widens the conservative-candidate volume that
        // feeds the mark-phase writer, with zero coverage benefit.
        let blocked_os_tids = shared.threads.thread_registry.blocked_os_tids();
        let (windows, _roots) = xt::helper_window_pass(
            &taken,
            &|a| shared.mem.heap.is_object_address(a),
            xt_roots,
            &blocked_os_tids,
        );
        helper_windows = windows;
    }
    // Publish any reserved TLAB tails still present after the barrier is
    // satisfied. Usually only forcibly-stopped in-JIT peers have one; collecting
    // all live threads also hardens the sweep against a blocked/tearing-down
    // thread that missed its retire before it left the counted mutator set.
    // Cleared by the caller after the collection completes.
    let regions = shared.threads.thread_registry.collect_reserved_tlab_tails();
    if taken.count() > 0 || helper_windows > 0 || !regions.is_empty() {
        shared.mem.heap.set_jit_tlab_skip_regions(&regions);
    }
    if taken.count() > 0 || helper_windows > 0 {
        // Helper-window roots are conservative (unprovable coverage) — the
        // collection must stay non-moving so a false-positive candidate can
        // only over-retain, never relocate under a live JIT/blocked frame.
        // xt-hardening (2026-07-03): such a cycle ALSO disables selective
        // promotion (a frozen peer's registers can hold only a derived/interior
        // pointer whose base would otherwise be evacuated from under it, then
        // zeroed and re-served). Scoped to cycles with actually-frozen/scanned
        // peers — reserved TLAB tails alone freeze nobody, and gating on them
        // would starve promotion on every cooperative multi-threaded cycle.
        //
        // HIB-GCOVERHEAD-HALFFULL.1: the promotion half now travels on its own
        // narrow flag. `mark_moving_young_coverage_incomplete` acquired dozens
        // of unrelated callers (every unproven compiled-frame oop map) and had
        // stopped meaning "un-rewritable peer state" — see
        // `gc_quiescence::unrewritable_peer_state`. Both are set here because
        // this cycle genuinely satisfies both.
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete();
        cratonvm_gc::gc_quiescence::mark_unrewritable_peer_state();
    }
    taken
}

/// INT-3 (G1) — pin-in-place everything a forcibly-frozen peer can address.
///
/// A frozen peer is excused from the STW barrier, so it never applies this
/// collection's pointer map to its own state; under an EVACUATING collector
/// every object it references directly must therefore stay put. Two sources:
///
/// 1. `xt_roots` — the conservative register/stack scan of each frozen peer
///    (plus the helper-window scans of blocked threads, whose Rust-stack
///    locals are equally un-rewritable).
/// 2. The frozen peers' deposited root snapshots — the collector's only view
///    of their interpreter frames. The snapshot entries are merged into the
///    collection roots (which keeps the objects ALIVE and rewrites the
///    merged copies), but the peer's actual frame slots are never remapped:
///    it skips the safepoint-resume `apply_pointer_map_to_thread`. Pinning
///    the referenced regions keeps those slots valid; the objects' own
///    fields are still fixed up in place by the phase-4 walk.
///
/// `add_pinned_jit_root` keys the pins to the INITIATOR's registry entry, so
/// they last exactly one cycle (the initiator's next deposit or
/// `collect_roots` replaces/clears them) and over-pinning only keeps a
/// region out of one CSet. MUST run after `collect_roots` (which clears the
/// initiator's entry) and before `collect_garbage`.
///
/// No-op on non-G1 backends: Generational frozen-peer cycles run the fully
/// non-moving sweep (`mark_moving_young_coverage_incomplete`), so nothing
/// moves and no pin is needed.
fn pin_frozen_peer_roots_for_g1(
    shared: &SharedVm,
    xt_roots: &[ObjectRef],
    taken: &crate::jit::xt_root_scan::TakenOver,
) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    for r in xt_roots {
        cratonvm_gc::gc_quiescence::add_pinned_jit_root(r.as_ptr() as usize);
    }
    for r in shared
        .threads
        .thread_registry
        .root_snapshots_for_os_tids(&taken.tids)
    {
        cratonvm_gc::gc_quiescence::add_pinned_jit_root(r.as_ptr() as usize);
    }
}

// stw-residual-close (CRATONVM_DBG_REMAP_TRACE): per-OS-thread debug rings.
// (a) participation trace: one line per GC-relevant transition on this thread
// (publish/deposit/arrive/apply/wake/initiator) with the top frame + pc and
// the map/fixup size, so a stale-ref capture can reconstruct exactly how the
// holder participated in the fatal epoch. (b) native-return ring: the last
// object-returning natives and their returned addresses, so a capture can
// name the producer of a stale value that was pushed from a Rust-side copy.
pub(crate) fn remap_trace_on() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_REMAP_TRACE").is_some())
}

thread_local! {
    static REMAP_TRACE: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static NRET_RING: std::cell::RefCell<Vec<(usize, usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static GETFIELD_RING: std::cell::RefCell<Vec<(usize, usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Record a reference-typed getfield push: (parent, field_index, pushed).
thread_local! {
    static DEPOSIT_GAP_RING: std::cell::RefCell<Vec<(usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// After a snapshot build, diff it against a RAW walk of the frames: ring
/// every Object-decoding local/stack slot whose address the snapshot lacks.
pub(crate) fn deposit_gap_diff(
    thread: &JvmThread,
    snapshot: &[crate::types::ObjectRef],
    tag: &str,
) {
    if !remap_trace_on() {
        return;
    }
    let have: std::collections::HashSet<usize> =
        snapshot.iter().map(|o| o.as_ptr() as usize).collect();
    DEPOSIT_GAP_RING.with(|ring| {
        let mut ring = ring.borrow_mut();
        for (fi, fr) in thread.frames.iter().enumerate() {
            for li in 0..fr.locals_len() {
                if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                    let a = o.as_ptr() as usize;
                    if !have.contains(&a) {
                        if ring.len() >= 512 {
                            ring.drain(..128);
                        }
                        ring.push((
                            a,
                            format!(
                                "{tag} local f#{fi} {}.{} pc={} slot={li}",
                                fr.class_name(),
                                fr.method_name(),
                                fr.pc
                            ),
                        ));
                    }
                }
            }
            for si in 0..fr.stack.len() {
                if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                    let a = o.as_ptr() as usize;
                    if !have.contains(&a) {
                        if ring.len() >= 512 {
                            ring.drain(..128);
                        }
                        ring.push((
                            a,
                            format!(
                                "{tag} stack f#{fi} {}.{} pc={} slot={si}",
                                fr.class_name(),
                                fr.method_name(),
                                fr.pc
                            ),
                        ));
                    }
                }
            }
        }
    });
}

thread_local! {
    static PUSH_PROV_RING: std::cell::RefCell<Vec<(usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Record an invoke-return Object push with its call-site description.
pub(crate) fn push_prov_record(addr: usize, site: &str) {
    if !remap_trace_on() {
        return;
    }
    PUSH_PROV_RING.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() >= 128 {
            r.drain(..32);
        }
        r.push((addr, site.to_string()));
    });
}

/// Record a kind-preserving shuffle (dup*/swap) re-push of an Object slot,
/// for the same pushprov ring `push_prov_record` feeds. Cheap no-op unless
/// `CRATONVM_DBG_REMAP_TRACE` is set.
#[inline]
fn record_shuffle_push(cv: crate::types::CompactValue, site: &str) {
    if remap_trace_on() {
        if let Value::Object(Some(o)) = cv.to_value() {
            push_prov_record(o.as_ptr() as usize, site);
        }
    }
}

/// Probe recent invoke-return pushes for `addr`: (pushes-ago, site).
pub(crate) fn push_prov_find(addr: usize) -> Vec<(usize, String)> {
    PUSH_PROV_RING.with(|r| {
        let r = r.borrow();
        let n = r.len();
        r.iter()
            .enumerate()
            .filter(|(_, (a, _))| *a == addr)
            .map(|(i, (_, s))| (n - i, s.clone()))
            .collect()
    })
}

/// Probe the deposit-gap ring for `addr`: (entries-ago, description).
pub(crate) fn deposit_gap_find(addr: usize) -> Vec<(usize, String)> {
    DEPOSIT_GAP_RING.with(|r| {
        let r = r.borrow();
        let n = r.len();
        r.iter()
            .enumerate()
            .filter(|(_, (a, _))| *a == addr)
            .map(|(i, (_, d))| (n - i, d.clone()))
            .collect()
    })
}

pub(crate) fn getfield_ring_record(parent: usize, idx: usize, pushed: usize) {
    if !remap_trace_on() {
        return;
    }
    GETFIELD_RING.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() >= 96 {
            r.remove(0);
        }
        r.push((parent, idx, pushed));
    });
}

/// Find `addr` among recent reference getfield pushes:
/// (pushes-ago, parent, field_index).
pub(crate) fn getfield_ring_find(addr: usize) -> Vec<(usize, usize, usize)> {
    GETFIELD_RING.with(|r| {
        let r = r.borrow();
        let n = r.len();
        r.iter()
            .enumerate()
            .filter(|(_, (_, _, a))| *a == addr)
            .map(|(i, (p, f, _))| (n - i, *p, *f))
            .collect()
    })
}

pub(crate) fn remap_trace_push(shared: &SharedVm, thread: &JvmThread, tag: &str, extra: &str) {
    if !remap_trace_on() {
        return;
    }
    let top = thread
        .frames
        .last()
        .map(|f| format!("{}.{} pc={}", f.class_name(), f.method_name(), f.pc))
        .unwrap_or_else(|| "<no-frame>".to_string());
    let line = format!(
        "e{} {} tid={} frames={} top={} {}",
        shared.mem.heap.collection_count(),
        tag,
        thread.thread_id.0,
        thread.frames.len(),
        top,
        extra,
    );
    REMAP_TRACE.with(|t| {
        let mut t = t.borrow_mut();
        if t.len() >= 24 {
            t.remove(0);
        }
        t.push(line);
    });
}

pub(crate) fn remap_trace_dump() -> String {
    REMAP_TRACE.with(|t| {
        t.borrow().join(
            "
    ",
        )
    })
}

pub(crate) fn nret_record(cb: usize, addr: usize, site: &str) {
    if !remap_trace_on() {
        return;
    }
    NRET_RING.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() >= 64 {
            r.remove(0);
        }
        r.push((cb, addr, site.to_string()));
    });
}

/// Find `addr` among recent native object returns:
/// (returns-ago, callback, java-site).
pub(crate) fn nret_find(addr: usize) -> Vec<(usize, usize, String)> {
    NRET_RING.with(|r| {
        let r = r.borrow();
        let n = r.len();
        r.iter()
            .enumerate()
            .filter(|(_, (_, a, _))| *a == addr)
            .map(|(i, (cb, _, s))| (n - i, *cb, s.clone()))
            .collect()
    })
}

/// Finalizable roots every collection must seed, whichever path runs it.
///
/// Two disjoint sets, both of which the collector would otherwise miss:
///
///   * `ref_processor.finalizer_referent_addresses()` — registered
///     finalizables not yet claimed. Only the `System.gc` path used to pass
///     these; the ordinary allocation-driven collections below passed `&[]`.
///   * `finalizer_thread.pending_addresses()` — objects already claimed and
///     queued, waiting for `finalize()` to actually run. `mark_finalizer_enqueued`
///     deliberately drops these from the first list, so nothing else roots
///     them, yet the queue keeps only a raw address. `run_finalizers` also
///     bails out early whenever a JIT borrow is live, which routinely leaves
///     entries queued across several collections — a wide window in which a
///     non-moving young sweep frees the object and leaves the queue pointing
///     at reclaimed memory (SEGV in `run_finalizers`' `class_id_of`).
fn finalizable_roots(shared: &SharedVm) -> Vec<usize> {
    let mut addrs = {
        let rp = shared.mem.ref_processor.lock();
        rp.finalizer_referent_addresses()
    };
    addrs.extend(shared.mem.finalizer_thread.pending_addresses());
    addrs.sort_unstable();
    addrs.dedup();
    addrs
}

pub(crate) fn maybe_gc(shared: &SharedVm, thread: &mut JvmThread) {
    // First, check if another thread requested STW — if so, participate
    safepoint_check(shared, thread);

    if shared.mem.heap.needs_gc()
        || shared
            .mem
            .gc_requested
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    {
        // Retire TLAB before GC — its memory is in from-space
        thread.tlab.retire();
        // Round-5 fix (CRIT — UAF): the GC initiator never passes through
        // `safepoint_check`'s arrive_and_wait, so drain its OWN per-thread
        // SATB buffer here. Without this the initiator's last up-to-255
        // overwritten references vanish on every cycle; the bug bites
        // hardest in single-threaded mode where the initiator IS every
        // mutator.
        shared.mem.heap.flush_thread_satb();
        // Update our root snapshot before requesting STW
        cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
        update_root_snapshot(shared, thread);
        mtroots_set_gc_ctx(shared, thread, 2); // 2 = alloc-young (maybe_gc)
        mtroots_dump_initiator(shared, thread, 2);

        // DBG (bc math-ec, CRATONVM_DBG_ECWATCH): detect watched-cell corruption
        // at GC ENTRY (addresses still valid, pre-relocation). Fires even if the
        // corruptor was NOT dispatched via `safe_native_call` (e.g. an inline
        // interpreter intrinsic), confirming the watch machinery works and that
        // a watched EC field really did flip to a small value before this GC.
        if crate::runtime::ec_watch::enabled() {
            let watched = crate::runtime::ec_watch::size(shared.vm_identity);
            let gc_hits = crate::runtime::ec_watch::detect(shared.vm_identity);
            if !gc_hits.is_empty() {
                eprintln!(
                    "[ecwatch-GC] {} CORRUPTED-at-GC of {} watched cells:",
                    gc_hits.len(),
                    watched,
                );
                for (holder, idx, expected, now) in gc_hits {
                    eprintln!(
                        "[ecwatch-GC]   holder@0x{holder:x} fld[{idx}]: 0x{expected:x} -> 0x{now:x}"
                    );
                }
            } else if watched > 0 {
                eprintln!("[ecwatch-GC] {watched} watched cells, all clean at this GC");
            }
        }

        // Truncation-checked: alive_count (usize) to u32; thread count realistically bounded
        let alive_count =
            u32::try_from(shared.threads.thread_registry.alive_count()).unwrap_or(u32::MAX);
        if alive_count <= 1 {
            // Single-threaded fast path: no barrier needed
            let gc_start = std::time::Instant::now();
            let mut roots = collect_roots(shared, thread);
            // STW invariant: single-threaded path means this thread is
            // the only mutator — every other thread is implicitly
            // "parked" (it doesn't exist). Construct the token directly.
            // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
            weakref_null_referents_pre_gc(shared);
            // SAFETY: the STW invariant stated just above holds — every other
            // mutator is parked at a safepoint (or was forcibly stopped and
            // conservatively scanned), so this thread is the only mutator and
            // may assert exclusive heap access.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            let fin_roots = finalizable_roots(shared);
            let result = shared
                .mem
                .heap
                .collect_garbage_with_finalizers(
                    &stw,
                    &mut roots,
                    &fin_roots,
                    &shared.threads.monitors,
                )
                .0;
            process_references_after_gc(shared, &result.pointer_map);
            update_all_roots(shared, thread, &result.pointer_map);
            // DBG (bc math-ec, CRATONVM_DBG_ECWATCH): the moving collector
            // relocated survivors — REMAP each watched holder through the
            // pointer_map so watches PERSIST across this GC (the corruption
            // frequently hits an object that survived the GC that wrote it).
            crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
            // GC-EXIT detect: a watched cell that was clean at GC ENTRY (above)
            // but reads 0x4 here was corrupted *by collect_garbage itself*
            // (between entry and exit) — isolating GC-vs-mutator definitively.
            if crate::runtime::ec_watch::enabled() {
                for (holder, idx, expected, now) in crate::runtime::ec_watch::detect(shared.vm_identity) {
                    eprintln!(
                        "[ecwatch-GCEXIT] holder@0x{holder:x} fld[{idx}]: 0x{expected:x} -> 0x{now:x} (corrupted DURING collect_garbage)"
                    );
                }
            }
            // bc math-ec 0x4 (CRATONVM_DBG_MEMWATCH): post-GC poll of the
            // watched absolute address — a HIT here (vs at a mutator
            // safepoint) means the flip happened inside collect_garbage /
            // reference processing.
            crate::runtime::memwatch::poll("post-gc", || {
                thread
                    .frames
                    .iter()
                    .rev()
                    .take(28)
                    .map(|f| {
                        format!(
                            "  {}.{}{} pc={}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            // DBG (env-gated): validate every young object's header size against
            // its class — pins a JIT `new` that wrote a wrong-size header.
            crate::memory::gc::validate_object_sizes(shared);
            // DBG: run the heap-stale verifier after EVERY GC (incl. the
            // non-moving JIT-active sweep, where update_all_roots early-returns
            // on the empty pointer_map). It flags any LIVE object whose field
            // points to a ZEROED/reclaimed object — i.e. a live object the sweep
            // wrongly reclaimed (missing root). The referrer names the bug.
            crate::memory::gc::verify_heap_object_fields(shared, &result.pointer_map);
            // DBG (CRATONVM_DBG_CORRUPT_FRAMES): on the FIRST GC that detects
            // sweep corruption, dump the mutator's Java stack. With a tiny young
            // gen (frequent GC) this fires close to the JIT corruptor — the
            // interpreted frame on top is the BC method that called the
            // JIT-compiled corruptor.
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CORRUPT_FRAMES").is_some()
                && cratonvm_gc::gen_heap::SWEEP_CORRUPTION_HITS
                    .load(std::sync::atomic::Ordering::Relaxed)
                    > 0
            {
                use std::sync::atomic::{AtomicBool, Ordering as DbgO};
                static DUMPED: AtomicBool = AtomicBool::new(false);
                if !DUMPED.swap(true, DbgO::Relaxed) {
                    eprintln!(
                        "[corrupt-frames] FIRST sweep corruption — mutator Java stack ({} frames, top first):",
                        thread.frames.len()
                    );
                    for (i, f) in thread.frames.iter().enumerate().rev().take(60) {
                        eprintln!(
                            "  [{}] {}.{}{} pc={}",
                            i,
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc
                        );
                    }
                }
            }
            // Truncation-checked: as_millis returns u128 but GC duration fits u64
            let gc_duration_ms = u64::try_from(gc_start.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::debug!(
                "GC completed (single-thread): {} objects copied, {} bytes freed, {}ms",
                result.stats.objects_copied,
                result.stats.bytes_freed,
                gc_duration_ms,
            );
            // Record JFR GC event
            {
                let mut jfr = shared.debug.flight_recorder.lock();
                let now_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    // Truncation-checked: nanos since epoch fits u64 until year ~2554
                    .as_nanos() as u64;
                cratonvm_jfr::builtin::emit_gc_event(
                    &mut jfr,
                    1, // gc_id
                    "YoungGC",
                    "Allocation Failure",
                    now_ns.saturating_sub(gc_duration_ms * 1_000_000),
                    gc_duration_ms * 1_000_000,
                );
                cratonvm_jfr::builtin::emit_young_gc_event(
                    &mut jfr,
                    1,
                    15, // default tenuring threshold
                    now_ns.saturating_sub(gc_duration_ms * 1_000_000),
                    gc_duration_ms * 1_000_000,
                );
                // Truncation-checked: heap bytes (usize) to i64; heaps > 8 EiB are unrealistic
                let heap_used =
                    i64::try_from(shared.mem.heap.allocated_bytes()).unwrap_or(i64::MAX);
                cratonvm_jfr::builtin::emit_gc_heap_summary_event(
                    &mut jfr,
                    1,
                    "After GC",
                    "Young Gen",
                    heap_used,
                    heap_used,     // committed ≈ used for our simple heap
                    heap_used * 2, // max ≈ 2x used estimate
                    now_ns,
                );
            }
            // T19.3.G1 — bump the cycle counter so operators can
            // measure GC frequency against the 0.2 Hz allocation-storm
            // target.
            shared
                .mem
                .gc_cycle_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // After minor GC, check if old gen needs concurrent collection
            maybe_concurrent_gc(shared, thread);
            // Run any pending finalizers
            run_finalizers(shared, thread);
        } else {
            // Multi-threaded path: coordinate via GC barrier
            let mut counted_os_tids: Vec<u32> = Vec::new();
            let should_initiate_gc = {
                // xt-hardening (2026-07-03): snapshot the counted alive
                // set's OS tids atomically with the expected computation
                // (same closure, same barrier lock) for identity-based
                // takeover excusal.
                counted_os_tids.clear();
                shared.mem.gc_barrier.request_stw_counted_with_live_blocked(
                    thread.thread_id,
                    || {
                        let (n, blocked, tids, blocked_tids) = shared
                            .threads
                            .thread_registry
                            .alive_count_blocked_and_os_tids();
                        counted_os_tids = tids;
                        // DIAGNOSTIC (2026-07-13, STW takeover 5-class cluster
                        // investigation): print the EXACT identity set counted
                        // as "expected" (alive AND NOT in_blocked_region) at
                        // the instant this pause is requested, to disambiguate
                        // whether a thread later seen parked was already
                        // excluded at request time or genuinely raced in.
                        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STW_EXPECTED_IDS")
                            .is_some()
                        {
                            let expected_ids: Vec<u64> = shared
                                .threads
                                .thread_registry
                                .alive_thread_ids_excluding(&blocked_tids);
                            eprintln!(
                                "[stw-expected] initiator={} n={} blocked={} expected_ids={:?}",
                                thread.thread_id.0, n, blocked, expected_ids
                            );
                        }
                        (
                            u32::try_from(n).unwrap_or(u32::MAX),
                            u32::try_from(blocked).unwrap_or(u32::MAX),
                            blocked_tids,
                        )
                    },
                )
            };
            if should_initiate_gc {
                // We are the GC initiator. BUG-03 — forcibly stop in-JIT
                // peers and conservatively scan them before waiting for the
                // cooperative mutators.
                let mut xt_roots: Vec<ObjectRef> = Vec::new();
                let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);

                // Collect roots: current thread + all snapshots + shared state
                let mut roots = collect_roots(shared, thread);
                let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
                roots.extend(snapshot_roots);
                // INT-3 (G1) — everything a frozen peer can address must not
                // move; must follow collect_roots (which clears the pins).
                pin_frozen_peer_roots_for_g1(shared, &xt_roots, &taken);
                // BUG-03 — conservative roots from forcibly-stopped in-JIT peers.
                roots.extend(xt_roots);

                // STW invariant: `gc_barrier.wait_for_all()` returned, so
                // every mutator has parked at its safepoint poll OR (BUG-03)
                // been forcibly stopped in JIT and conservatively scanned.
                // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
                weakref_null_referents_pre_gc(shared);
                // SAFETY: the STW invariant stated just above holds — every other
                // mutator is parked at a safepoint (or was forcibly stopped and
                // conservatively scanned), so this thread is the only mutator and
                // may assert exclusive heap access.
                let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
                let fin_roots = finalizable_roots(shared);
                let result = shared
                    .mem
                    .heap
                    .collect_garbage_with_finalizers(
                        &stw,
                        &mut roots,
                        &fin_roots,
                        &shared.threads.monitors,
                    )
                    .0;
                process_references_after_gc(shared, &result.pointer_map);

                // Update shared VM state (statics, string pool, etc.)
                update_all_roots(shared, thread, &result.pointer_map);
                // DBG (bc math-ec): remap watchpoints through the pointer_map.
                crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);

                tracing::debug!(
                    "GC completed (multi-thread, {} threads): {} objects copied, {} bytes freed",
                    alive_count,
                    result.stats.objects_copied,
                    result.stats.bytes_freed,
                );

                // BUG-03 — drop the TLAB skip regions and resume the
                // forcibly-stopped in-JIT peers now that the heap is
                // consistent again (non-moving sweep on Generational /
                // pinned-in-place regions + walker-skipped TLAB tails on G1
                // (INT-3) → their pointers are unchanged). xt-hardening
                // (2026-07-03): BOTH must happen
                // BEFORE `complete_gc` reopens the world — a released mutator
                // could otherwise win the NEXT STW, re-freeze the
                // still-suspended peers and publish fresh skip regions that
                // THIS initiator's late clear would wipe, letting the next
                // sweep walk (and free-list) the frozen peers' reserved
                // tails. A resumed peer that immediately requests the next
                // GC blocks until `complete_gc` anyway (the barrier is still
                // closed here), so the reorder introduces no new window.
                shared.mem.heap.clear_jit_tlab_skip_regions();
                crate::jit::xt_root_scan::resume(taken);
                // Signal all threads with the pointer map
                shared.mem.gc_barrier.complete_gc(result.pointer_map);

                // T19.3.G1 — bump the cycle counter (multi-threaded
                // path, fires only on the GC initiator).
                shared
                    .mem
                    .gc_cycle_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // After minor GC, check if old gen needs concurrent collection
                maybe_concurrent_gc(shared, thread);
                // Run any pending finalizers
                run_finalizers(shared, thread);
            } else {
                // Another thread is already doing GC — just participate
                safepoint_check(shared, thread);
            }
        }
    }
}

/// Force a GC cycle regardless of threshold.
/// Used by allocation helpers when the fast-path allocation fails.
/// Public wrapper so sibling modules (exceptions, invokedynamic) can force a
/// GC cycle when a direct allocation fails.
/// Self-call identity proof for the raw direct self-recursive CALL routing
/// (see `cratonvm_jit::set_self_call_identity_stable`): true iff `class_id`
/// was defined by a BUILTIN loader (bootstrap/extension/application) AND the
/// loader-qualified exact-name lookup maps the class's name back to this exact
/// `ClassId`. Builtin loader registries hold one class per name and resolve a
/// self-reference to the already-defined class, so a same-named shadow can
/// never rebind the target; `UserDefined` loaders (enhancement/duplicating
/// loaders) return false and keep the dispatch route.
fn self_call_identity_stable(shared: &SharedVm, class_id: ClassId) -> bool {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    !matches!(
        class.loader_id,
        cratonvm_types::ClassLoaderId::UserDefined(_)
    ) && cm.class_defined_by_loader_exact(&class.name, class.loader_id) == Some(class_id)
}

pub fn maybe_gc_forced_pub(shared: &SharedVm, thread: &mut JvmThread) {
    maybe_gc_forced(shared, thread);
}

/// Allocate a dynamically-produced `java.lang.String` under the SAME
/// heap-exhaustion contract as `new`: collect, retry, and finally raise a
/// catchable `OutOfMemoryError` -- never abort the process.
///
/// `vm_object::create_java_string_uninterned` cannot do this: it takes only a
/// `&SharedVm`, and every GC entry point needs the calling thread (to retire
/// its TLAB and contribute its roots). So its exhaustion path was a bare
/// `eprintln!` + `std::process::abort()`, which turned a plain `"a" + b` on a
/// full heap into an un-catchable VM kill. HotSpot throws
/// `OutOfMemoryError: Java heap space` there, and a Java program is entitled to
/// catch it. Reproduced with a 25-line probe under `-Xmx64m -XX:+UseG1GC`:
/// `FATAL: heap exhausted allocating java/lang/String (46 units)` (from the
/// `System.out.println("iter=" + i)` in the allocation loop) followed by
/// SIGABRT, where HotSpot reports `OutOfMemoryError` and keeps running.
pub(crate) fn create_string_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    use crate::vm::try_create_java_string_uninterned as try_new_string;
    if let Some(obj) = try_new_string(shared, text) {
        return Ok(obj);
    }
    // Same escalation ladder as `alloc_object_shared`: forced young/full GC,
    // then G1's last-ditch complete mark cycle (dead Old/humongous spans are
    // only reclaimed by a finished cycle's cleanup), then OOM.
    thread.tlab.retire();
    maybe_gc_forced(shared, thread);
    if let Some(obj) = try_new_string(shared, text) {
        return Ok(obj);
    }
    g1_force_full_cycle(shared, thread);
    if let Some(obj) = try_new_string(shared, text) {
        return Ok(obj);
    }
    maybe_dump_heap_on_oom(shared, thread);
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::OutOfMemoryError {
            message: format!("Java heap space (String of {} chars)", text.len()),
        },
    )))
}

fn maybe_gc_forced(shared: &SharedVm, thread: &mut JvmThread) {
    // CRIT (TLAB UAF) — retire this thread's TLAB before initiating GC, exactly
    // as `maybe_gc` and `force_gc_from_native` do. This forced path (allocation
    // failure / `create_exception_object`) was the one GC initiator that did NOT
    // retire: its TLAB `[cursor,end)` stays in young-from across the collection,
    // so its unfilled tail is un-walkable to the sweep and, after the young
    // swap+reset, the stale TLAB hands out memory the collector considers free —
    // the same use-after-free / heap-desync class as the parked/blocked-thread
    // TLAB bugs. Surfaced as a SIGSEGV in the moving collector's post-copy
    // `pointer_map` walk under multi-threaded churn (TestFileStoreConcurrency).
    thread.tlab.retire();
    // GC-overhead accounting: live set BEFORE the collection (post-TLAB-retire),
    // so `note_gc_productivity` can compute how much this forced GC actually
    // freed (`before - after`). See `note_gc_productivity` / `gc_overhead_limit_exceeded`.
    //
    // Use the free-list-aware live estimate, NOT `allocated_bytes`: the default
    // (non-moving) young sweep reclaims dead objects into the from-space free
    // list without retreating the bump cursor, so `allocated_bytes` reads a
    // perfectly-productive sweep as "freed 0" and latches the overhead limit
    // after `GC_OVERHEAD_LIMIT_CYCLES` young fills — a spurious
    // `OutOfMemoryError` on a heap that is almost entirely garbage. Restores
    // part 3 of a9c580aff, which d8092acba ("fix-tests-real-jdk-contracts")
    // reverted in this file while leaving both accessors in place and
    // caller-less; see docs/internal/fixed-suite-bugs/tomcat/
    // 24-stringcache-oom-under-load.md.
    let before_live = shared.mem.heap.live_bytes_estimate();
    let before_promoted = shared.mem.heap.bytes_promoted_total();
    // Round-5 fix (CRIT — UAF): see comment in `maybe_gc`. The forced
    // path is also an initiator path; drain its per-thread SATB buffer
    // before scanning roots.
    shared.mem.heap.flush_thread_satb();
    cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
    update_root_snapshot(shared, thread);
    mtroots_set_gc_ctx(shared, thread, 3); // 3 = forced-alloc (maybe_gc_forced)
    mtroots_dump_initiator(shared, thread, 3);

    let alive_count = shared.threads.thread_registry.alive_count() as u32; // Widening: thread count to u32
    if alive_count <= 1 {
        let mut roots = collect_roots(shared, thread);
        // STW invariant: single-threaded fast path — see `maybe_gc`.
        // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
        weakref_null_referents_pre_gc(shared);
        // SAFETY: the STW invariant stated just above holds — every other
        // mutator is parked at a safepoint (or was forcibly stopped and
        // conservatively scanned), so this thread is the only mutator and
        // may assert exclusive heap access.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        let fin_roots = finalizable_roots(shared);
        let result = shared
            .mem
            .heap
            .collect_garbage_with_finalizers(&stw, &mut roots, &fin_roots, &shared.threads.monitors)
            .0;
        process_references_after_gc(shared, &result.pointer_map);
        update_all_roots(shared, thread, &result.pointer_map);
        crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
        // T19.3.G1 — count forced cycles (allocation-failure-driven) too.
        shared
            .mem
            .gc_cycle_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        note_gc_productivity(shared, before_live, before_promoted);
    } else {
        let mut counted_os_tids: Vec<u32> = Vec::new();
        let should_initiate_gc = {
            // xt-hardening (2026-07-03): see maybe_gc — atomic counted-set
            // snapshot for identity-based takeover excusal.
            counted_os_tids.clear();
            shared
                .mem
                .gc_barrier
                .request_stw_counted_with_live_blocked(thread.thread_id, || {
                    let (n, blocked, tids, blocked_tids) = shared
                        .threads
                        .thread_registry
                        .alive_count_blocked_and_os_tids();
                    counted_os_tids = tids;
                    (
                        u32::try_from(n).unwrap_or(u32::MAX),
                        u32::try_from(blocked).unwrap_or(u32::MAX),
                        blocked_tids,
                    )
                })
        };
        if should_initiate_gc {
            // BUG-03 — forcibly stop + conservatively scan in-JIT peers.
            let mut xt_roots: Vec<ObjectRef> = Vec::new();
            let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
            let mut roots = collect_roots(shared, thread);
            let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
            roots.extend(snapshot_roots);
            // INT-3 (G1) — everything a frozen peer can address must not
            // move; must follow collect_roots (which clears the pins).
            pin_frozen_peer_roots_for_g1(shared, &xt_roots, &taken);
            roots.extend(xt_roots); // BUG-03 cross-thread JIT conservative roots
                                    // STW invariant: `wait_for_all()` returned — every mutator
                                    // has parked at its safepoint poll (or, BUG-03, been forcibly
                                    // stopped in JIT and conservatively scanned).
                                    // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
            weakref_null_referents_pre_gc(shared);
            // SAFETY: the STW invariant stated just above holds — every other
            // mutator is parked at a safepoint (or was forcibly stopped and
            // conservatively scanned), so this thread is the only mutator and
            // may assert exclusive heap access.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            let fin_roots = finalizable_roots(shared);
            let result = shared
                .mem
                .heap
                .collect_garbage_with_finalizers(
                    &stw,
                    &mut roots,
                    &fin_roots,
                    &shared.threads.monitors,
                )
                .0;
            process_references_after_gc(shared, &result.pointer_map);
            update_all_roots(shared, thread, &result.pointer_map);
            // Step 5 GAP D: remap the ec_watch corruption-watch table across this
            // multi-threaded forced collection too. The single-threaded GC paths
            // already do (mirrors the `update_all_roots` -> `ec_watch::remap`
            // pairing at maybe_gc:419 / maybe_gc_forced:636); this multi-threaded
            // initiator path was missing it, so a relocating G1 evacuation left
            // ec_watch holders stale and the watchpoint read moved-away memory.
            crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
            // xt-hardening (2026-07-03): clear regions + resume BEFORE
            // complete_gc (see maybe_gc's epilogue for the race rationale).
            shared.mem.heap.clear_jit_tlab_skip_regions(); // BUG-03
            crate::jit::xt_root_scan::resume(taken); // BUG-03 resume frozen peers
            shared.mem.gc_barrier.complete_gc(result.pointer_map);
            // T19.3.G1 — count forced cycles (multi-threaded initiator).
            shared
                .mem
                .gc_cycle_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            note_gc_productivity(shared, before_live, before_promoted);
        } else {
            safepoint_check(shared, thread);
        }
    }
}

/// Default number of consecutive unproductive allocation-failure GCs (each
/// freeing < 2% of capacity *while the old generation cannot absorb 2% of
/// capacity* — see [`note_gc_productivity`]) after which the allocation paths
/// declare OOM instead of continuing to GC-thrash. Overridable via
/// `CRATONVM_GC_OVERHEAD_LIMIT` (set to `0` to disable the limit entirely).
const GC_OVERHEAD_LIMIT_CYCLES: u32 = 8;

/// After a forced (allocation-failure) GC completes, record whether it was
/// *productive* — i.e. whether it actually relieved heap pressure. Measured as
/// the bytes it freed: `before - after` live bytes, where `before` is the live
/// set at `maybe_gc_forced` entry (post-TLAB-retire) and `after` is the live set
/// once the collection finishes. A forced GC that freed < 2% of total heap
/// capacity counts toward the GC-overhead streak — provided the old generation
/// is also too full to absorb 2% of capacity, see the HIB-GCOVERHEAD-HALFFULL.1
/// note below; one that freed more resets the streak either way.
///
/// The *freed-amount* signal (not post-GC fullness) is the right one for a
/// generational heap: in a retained-allocation death-spiral the young semi-space
/// is emptied every cycle (so total *fullness* sits near young/total ≈ 50% and
/// never looks exhausted), yet the GC frees ~nothing net because every survivor
/// is promoted into an already-full old generation. Promoted bytes DO count
/// toward productivity (they re-enable young allocation, which is the point of
/// the collection) — but the 2%-of-capacity threshold still catches the
/// death-spiral: a wedged, ~full old generation cannot absorb 2% of total heap
/// capacity per cycle, so its sliver-promotions stay "unproductive" and the
/// streak still trips the overhead limit. A healthy young→old drain moves far
/// more than 2% and resets it.
/// Forced GCs only happen on genuine allocation failure (young full *and*
/// promotion blocked), so this never fires during ordinary young-GC churn.
///
/// HIB-GCOVERHEAD-HALFFULL.1 (2026-07-31) — the freed-bytes test is only HALF
/// of HotSpot's `UseGCOverheadLimit`, which additionally requires a free-space
/// condition before it will convert GC pressure into an `OutOfMemoryError`.
/// Without that half, any defect that stops young draining reads identically to
/// the death spiral: `DefaultCatalogAndSchemaTest` died with `OutOfMemoryError`
/// after thirty forced GCs on a heap that was **49 % full with 570 MB free**,
/// because a 5 KB array allocation ran into a latched streak rather than a full
/// heap. The condition added here is the death spiral's own defining fact, taken
/// straight from the paragraph above: *"a wedged, ~full old generation cannot
/// absorb 2 % of total heap capacity per cycle"*. So a cycle counts toward the
/// streak only when the old generation genuinely cannot absorb that much. It is
/// deliberately NOT a total-fullness gate — those were rejected for the reason
/// stated above, and rightly.
///
/// This is the safety net, not the fix. The `promoted=0`-forever condition that
/// exposed it was a real collector defect (selective promotion switched off by a
/// flag that had changed meaning — see `gc_quiescence::unrewritable_peer_state`)
/// and is fixed at its source. What this guarantees is that the next such defect
/// surfaces as slowness, which is diagnosable, rather than as a spurious OOM on
/// a half-empty heap, which is not.
fn note_gc_productivity(shared: &SharedVm, before_live: usize, before_promoted: u64) {
    let cap = shared.mem.heap.heap_capacity();
    if cap == 0 {
        return;
    }
    // Free-list-aware live metric (see the capture site in `maybe_gc_forced`):
    // the non-moving sweep reclaims into the young free list without moving
    // the bump cursor, so `allocated_bytes` would read a fully-productive
    // sweep as "freed 0" and falsely latch the overhead limit.
    let after_live = shared.mem.heap.live_bytes_estimate();
    // A promotion-only cycle conserves live bytes but still did useful
    // allocation-enabling work (it drained young), so credit promoted bytes.
    // The 2%-of-capacity threshold below still catches the genuine
    // everything-survives-into-a-full-old-gen death spiral.
    let promoted = shared
        .mem
        .heap
        .bytes_promoted_total()
        .saturating_sub(before_promoted) as usize;
    let freed = before_live
        .saturating_sub(after_live)
        .saturating_add(promoted);
    // The free-space half (see the doc comment): the old generation must be
    // unable to absorb 2% of total capacity — the death spiral's own definition
    // of "wedged" — before a sliver-freeing cycle counts toward the streak.
    // Same 2%-of-`cap` yardstick as the freed-bytes test, so the two halves
    // cannot drift apart.
    let old_headroom = shared.mem.heap.old_gen_headroom();
    // Cast: numeric/representation conversion
    let old_gen_wedged = (old_headroom as u128) * 100 < (cap as u128) * 2;
    // unproductive: freed < 2% of capacity AND the old gen is wedged
    let freed_sliver = (freed as u128) * 100 < (cap as u128) * 2;
    let unproductive = freed_sliver && old_gen_wedged;
    let streak = if unproductive {
        shared
            .mem
            .gc_unproductive_streak
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    } else {
        shared
            .mem
            .gc_unproductive_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        0
    };
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_GC_OVERHEAD").is_some() {
        eprintln!(
            "[GC_OVERHEAD] before={before_live} after={after_live} promoted={promoted} \
             freed={freed} cap={cap} old_headroom={old_headroom} freed_sliver={freed_sliver} \
             old_gen_wedged={old_gen_wedged} unproductive={unproductive} streak={streak}"
        );
    }
}

/// Returns `true` when the heap has GC-thrashed past the overhead limit — i.e.
/// `GC_OVERHEAD_LIMIT_CYCLES` consecutive forced GCs each freed < 2% of the
/// heap while the old generation was too full to absorb that much (both halves
/// required — see `note_gc_productivity`, and
/// `docs/internal/fixed-suite-bugs/hibernate/` for the spurious-OOM-at-49%-full
/// report that added the second half).
/// The allocation-failure paths call this right after `maybe_gc_forced`
/// and, when it is `true`, surface a catchable `OutOfMemoryError` (the
/// pre-allocated `singleton_oom`) instead of retrying into an O(n²) death-spiral
/// on a heap full of live (retained) objects. Mirrors HotSpot's
/// `UseGCOverheadLimit`. Disabled (always `false`) when
/// `CRATONVM_GC_OVERHEAD_LIMIT=0`.
pub fn gc_overhead_limit_exceeded(shared: &SharedVm) -> bool {
    // PERF: this runs on the per-allocation slow path (`jit_new_object` and
    // the interpreter allocation sites). An uncached `cratonvm_types::flags::runtime_var` here was
    // ~6% of binarytrees-18 wall time (getenv does a linear environ scan) —
    // read the knob once. `Some(0)` = explicitly disabled.
    use std::sync::OnceLock;
    static LIMIT: OnceLock<u32> = OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_GC_OVERHEAD_LIMIT") {
            Ok(v) => v.trim().parse::<u32>().unwrap_or(GC_OVERHEAD_LIMIT_CYCLES),
            Err(_) => GC_OVERHEAD_LIMIT_CYCLES,
        }
    });
    if limit == 0 {
        return false; // explicitly disabled
    }
    shared
        .mem
        .gc_unproductive_streak
        .load(std::sync::atomic::Ordering::Relaxed)
        >= limit
}

/// Force a GC cycle from a native method (e.g. System.gc()).
/// Runs GC with finalizer-aware resurrection, processes references,
/// and invokes pending finalizers.
pub fn force_gc_from_native(shared: &SharedVm, thread: &mut JvmThread) {
    // Retire TLAB before GC
    thread.tlab.retire();
    // Round-5 fix (CRIT — UAF): drain this thread's per-thread SATB
    // buffer before initiating GC; see `maybe_gc` for the full rationale.
    shared.mem.heap.flush_thread_satb();
    // Real HotSpot's `System.gc()` triggers a FULL (old-gen-inclusive)
    // collection by default — request one explicitly, since the collector's
    // own Phase 5 otherwise only runs a major cycle when old gen crosses an
    // occupancy threshold. See `gc_quiescence`'s doc comment for the full
    // rationale (an already-promoted, genuinely-dead object is never swept by
    // a `System.gc()` that only triggers a minor collection).
    cratonvm_gc::gc_quiescence::request_major_gc();
    cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
    update_root_snapshot(shared, thread);
    mtroots_set_gc_ctx(shared, thread, 1); // 1 = System.gc
    mtroots_dump_initiator(shared, thread, 1);

    // Snapshot finalizable object addresses so the GC can resurrect dead ones
    let fin_addrs: Vec<usize> = finalizable_roots(shared);

    let alive_count = shared.threads.thread_registry.alive_count() as u32; // Widening: thread count to u32
    if alive_count <= 1 {
        let mut roots = collect_roots(shared, thread);
        // STW invariant: single-threaded fast path — see `maybe_gc`.
        // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
        weakref_null_referents_pre_gc(shared);
        // SAFETY: the STW invariant stated just above holds — every other
        // mutator is parked at a safepoint (or was forcibly stopped and
        // conservatively scanned), so this thread is the only mutator and
        // may assert exclusive heap access.
        let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
        let (result, dead_finalizers) = shared.mem.heap.collect_garbage_with_finalizers(
            &stw,
            &mut roots,
            &fin_addrs,
            &shared.threads.monitors,
        );
        process_references_after_gc(shared, &result.pointer_map);
        update_all_roots(shared, thread, &result.pointer_map);
        crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
        // Enqueue dead finalizable objects (their new addresses) for finalization
        for new_addr in &dead_finalizers {
            shared.mem.finalizer_thread.enqueue(*new_addr);
        }
        // Once-only finalization: flag the processor entries for the objects
        // just enqueued. `process_references_after_gc` above already ran
        // `update_after_gc`, so entry referents hold post-GC addresses
        // matching `dead_finalizers`. Without this, the resurrected object
        // looks alive to every later cycle and the GC re-resurrects +
        // re-enqueues it (finalize() observed running 3× per object).
        if !dead_finalizers.is_empty() {
            shared
                .mem
                .ref_processor
                .lock()
                .mark_finalizer_enqueued(&dead_finalizers);
        }
    } else {
        let mut counted_os_tids: Vec<u32> = Vec::new();
        let should_initiate_gc = {
            // xt-hardening (2026-07-03): see maybe_gc — atomic counted-set
            // snapshot for identity-based takeover excusal.
            counted_os_tids.clear();
            shared
                .mem
                .gc_barrier
                .request_stw_counted_with_live_blocked(thread.thread_id, || {
                    let (n, blocked, tids, blocked_tids) = shared
                        .threads
                        .thread_registry
                        .alive_count_blocked_and_os_tids();
                    counted_os_tids = tids;
                    (
                        u32::try_from(n).unwrap_or(u32::MAX),
                        u32::try_from(blocked).unwrap_or(u32::MAX),
                        blocked_tids,
                    )
                })
        };
        if should_initiate_gc {
            // BUG-03 — forcibly stop + conservatively scan in-JIT peers.
            let mut xt_roots: Vec<ObjectRef> = Vec::new();
            let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
            let mut roots = collect_roots(shared, thread);
            let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
            roots.extend(snapshot_roots);
            // INT-3 (G1) — everything a frozen peer can address must not
            // move; must follow collect_roots (which clears the pins).
            pin_frozen_peer_roots_for_g1(shared, &xt_roots, &taken);
            roots.extend(xt_roots); // BUG-03 cross-thread JIT conservative roots
                                    // STW invariant: `wait_for_all()` returned — every mutator
                                    // has parked at its safepoint poll (or, BUG-03, been forcibly
                                    // stopped in JIT and conservatively scanned).
                                    // HIB-CV-24: null Weak/Phantom referents before marking (restored post-GC).
            weakref_null_referents_pre_gc(shared);
            // SAFETY: the STW invariant stated just above holds — every other
            // mutator is parked at a safepoint (or was forcibly stopped and
            // conservatively scanned), so this thread is the only mutator and
            // may assert exclusive heap access.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            let (result, dead_finalizers) = shared.mem.heap.collect_garbage_with_finalizers(
                &stw,
                &mut roots,
                &fin_addrs,
                &shared.threads.monitors,
            );
            process_references_after_gc(shared, &result.pointer_map);
            update_all_roots(shared, thread, &result.pointer_map);
            // Step 5 GAP D: keep the ec_watch corruption-watch table consistent
            // across this multi-threaded finalizer collection (single-threaded
            // paths already remap it; this initiator path was missing the call).
            crate::runtime::ec_watch::remap(shared.vm_identity, &result.pointer_map);
            for new_addr in &dead_finalizers {
                shared.mem.finalizer_thread.enqueue(*new_addr);
            }
            // Once-only finalization — see the single-threaded arm above.
            if !dead_finalizers.is_empty() {
                shared
                    .mem
                    .ref_processor
                    .lock()
                    .mark_finalizer_enqueued(&dead_finalizers);
            }
            // xt-hardening (2026-07-03): clear regions + resume BEFORE
            // complete_gc (see maybe_gc's epilogue for the race rationale).
            shared.mem.heap.clear_jit_tlab_skip_regions(); // BUG-03
            crate::jit::xt_root_scan::resume(taken); // BUG-03 resume frozen peers
            shared.mem.gc_barrier.complete_gc(result.pointer_map);
        } else {
            safepoint_check(shared, thread);
        }
    }
    // INT-8: a forced GC advances the G1 concurrent-cycle machinery exactly
    // like an allocation-triggered young GC (`maybe_gc`'s epilogue calls
    // this at 819/903). Without it, a `System.gc()`-driven application —
    // whose forced young collections keep Eden below the allocation-GC
    // threshold — could NEVER start or complete a marking cycle: no cleanup
    // ever reclaimed dead Old regions and no remark-time reference
    // processing ever ran. HotSpot's default `System.gc()` under G1 is a
    // full collection that processes every generation's references; this
    // IHOP/completion check is the closest cycle-machinery equivalent.
    maybe_concurrent_gc(shared, thread);
    if shared.mem.heap.is_g1() {
        // An explicit System.gc() is a full-collection request, not merely an
        // Eden evacuation. Finish the G1 mark/remark/cleanup synchronously so
        // dead old regions, weak loaders, and their metadata are observable
        // before System.gc() returns.
        g1_force_full_cycle(shared, thread);
    }
    // Run pending finalizers
    run_finalizers(shared, thread);
    // Run pending Cleaner actions (NEW-17). These were submitted to
    // shared.mem.cleaner_thread by process_references_after_gc.
    run_cleaner_actions(shared, thread);
}

/// Drain pending Cleaner actions and invoke their Runnable.run() method.
///
/// Each entry is the address of a `java/lang/ref/Cleaner$Cleanable`
/// synthetic. Field 0 holds the Runnable action; field 1 is the cleaned
/// flag (idempotency guard, also set by user-triggered Cleanable.clean()).
///
/// Per the `Cleaner` contract, exceptions thrown by an action are caught
/// and logged — they must not propagate into the GC pipeline.
fn run_cleaner_actions(shared: &SharedVm, thread: &mut JvmThread) {
    // bc math-ec 0x4 exclusion switches — see `process_references_after_gc`.
    if no_refproc() || no_cleaners() {
        return;
    }
    // Re-entrancy safety: if a JIT helper currently holds the `&mut JvmThread`
    // (we were reached via `jit_invoke_dispatch` → `bail_to_interpreter` →
    // interpreter → `maybe_gc`), running a cleaner action's `run()` could
    // execute JIT-compiled code that calls `jit_thread_mut`, aliasing the live
    // borrow (debug: aliasing assert; release: UB/SEGV — the avrora crash
    // exposed by real-bytecode RAF's FileCleanable cleanups). Leave the actions
    // queued; they are GC-relocated (`update_after_gc`) and run at the next
    // top-level (non-JIT) safepoint.
    if crate::jit::helpers::is_jit_thread_set() {
        return;
    }
    // S-bytebuddy r3 — independent recursion guard for cleaner-action
    // dispatch. A Runnable.run() invoked from here can itself enqueue
    // (or trigger GC of) another Cleanable, which lands back in
    // `run_cleaner_actions` on the same OS thread. The aggregate
    // EXEC_DEPTH guard in `execute` catches this too, but only after
    // we have already burned ~10 Rust frames per turn of the loop.
    // A small dedicated counter trips earlier and avoids the loop
    // accumulating frames before the bigger guard fires.
    thread_local! {
        static CLEANER_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }
    struct CleanerDepthGuard;
    impl Drop for CleanerDepthGuard {
        fn drop(&mut self) {
            CLEANER_DEPTH.with(|d| {
                let v = d.get();
                d.set(v.saturating_sub(1));
            });
        }
    }
    let cdepth = CLEANER_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    // Limit calibrated to roughly 1/10 of EXEC_DEPTH (so a runaway
    // cleaner cascade trips here long before the main guard). The
    // per-iteration step factor is implicit: each cleaner action burns
    // ~10 native frames between dispatch and return.
    if cdepth > 1_000 {
        CLEANER_DEPTH.with(|d| {
            let v = d.get();
            d.set(v.saturating_sub(1));
        });
        // Don't propagate as a Java exception — the cleaner contract
        // forbids exceptions escaping. Just drop the remaining actions
        // on the floor; they will be retried on the next GC tick.
        return;
    }
    let _cleaner_depth_guard = CleanerDepthGuard;

    let addrs = shared.mem.cleaner_thread.drain_actions();
    for addr in addrs {
        // SAFETY: addr was produced by the cleaner thread's drain_actions and points at a valid object header within the heap arena.
        let cleanable = unsafe { ObjectRef::from_raw(addr as *mut u8) };
        // Idempotency: skip if user code already invoked clean().
        let already = matches!(shared.mem.heap.get_field(cleanable, 1), Value::Int(1),);
        if already {
            continue;
        }
        shared.mem.heap.set_field(cleanable, 1, Value::Int(1));
        let action = match shared.mem.heap.get_field(cleanable, 0) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        // Clear the action slot so it can be GC'd on the next cycle.
        shared.mem.heap.set_field(cleanable, 0, Value::Object(None));
        let class_id = shared.mem.heap.class_id_of(action);

        // Fast path: if the action is a lambda proxy (the common case —
        // `cleaner.register(buf, () -> {...})`), `invoke_shared` would
        // fall through to the abstract `Runnable.run()` declaration which
        // has no Code attribute. Route through `try_lambda_dispatch` so
        // the proxy's SAM impl_handle actually fires.
        let is_lambda = shared.classes.lambda_proxies.read().contains_key(&class_id);
        if is_lambda {
            // Errors are silently swallowed per the Cleaner contract.
            let _ = try_lambda_dispatch(shared, thread, action, class_id, "run", "()V", &[]);
            continue;
        }

        let class_name = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string());
        if let Some(name) = class_name {
            // Errors are silently swallowed per the Cleaner contract
            // (JDK catches Throwable inside CleanerImpl.run()).
            let _ = crate::vm::invoke_shared(
                shared,
                thread,
                &name,
                "run",
                "()V",
                &[Value::Object(Some(action))],
            );
        }
    }
}

/// Dequeue pending finalizable objects and invoke their finalize() method.
fn run_finalizers(shared: &SharedVm, thread: &mut JvmThread) {
    // bc math-ec 0x4 exclusion switches — see `process_references_after_gc`.
    if no_refproc() || no_cleaners() {
        return;
    }
    // Same JIT-borrow re-entrancy guard as `run_cleaner_actions`: a `finalize()`
    // invoked while a JIT helper holds the `&mut JvmThread` could re-enter the
    // JIT and alias the borrow. Defer to the next top-level safepoint; the
    // queue is GC-relocated via `FinalizerThread::update_after_gc`.
    if crate::jit::helpers::is_jit_thread_set() {
        return;
    }
    loop {
        let obj_addr = match shared.mem.finalizer_thread.dequeue() {
            Some(addr) => addr,
            None => break,
        };
        // SAFETY: obj_addr was produced by the finalizer thread's dequeue and points at a valid object header within the heap arena.
        let obj_ref = unsafe { ObjectRef::from_raw(obj_addr as *mut u8) };
        let class_id = shared.mem.heap.class_id_of(obj_ref);

        let class_name = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string());

        if let Some(name) = class_name {
            // Invoke finalize()V — errors are silently swallowed per JLS §12.6
            let _ = crate::vm::invoke_shared(
                shared,
                thread,
                &name,
                "finalize",
                "()V",
                &[Value::Object(Some(obj_ref))],
            );
        }
    }
}

/// Resolve the field slot used to link a `java.lang.ref.Reference` (or
/// subclass instance) onto its `ReferenceQueue`'s linked list when the GC
/// auto-enqueues it -- the `next` field as declared on `java/lang/ref/Reference`
/// itself (referent, queue, next, discovered — real-JDK layout).
///
/// MUST be resolved BY NAME against `Reference`'s own declaring class, not
/// by a field-count heuristic on the receiver's most-derived class: a
/// `java.util.WeakHashMap$Entry` (itself a `WeakReference` subclass) also
/// declares its OWN field named `next` (used for its hash-BUCKET chain, a
/// completely different linked list). The old heuristic ("index 2 if the
/// object has more than 2 fields") cannot tell these apart — for a
/// `WeakHashMap$Entry` specifically it can land on the wrong `next` slot,
/// so a GC-driven auto-enqueue (a live-application-scale WeakHashMap
/// *will* eventually have a stale entry to expunge) splices the
/// ReferenceQueue's link over the bucket-chain link, corrupting whatever
/// hash bucket that entry lived in — a later `WeakHashMap.get()`/
/// `expungeStaleEntries()` walking that bucket's `Entry.next` chain then
/// loops forever (observed: `com.sun.beans.TypeResolver`'s internal
/// `WeakCache`'s `WeakHashMap.get()` permanently stuck inside
/// `matchesKey()`, hanging Spring Boot's Thymeleaf layout-dialect
/// `createLayoutFromConfigClass` test). See
/// docs/known-issues/springboot/thymeleaf-groovy-layoutdialect-metaclass-introspection-hang.md.
fn gc_reference_next_slot(shared: &SharedVm) -> usize {
    let cm = shared.classes.class_manager.read();
    cm.find_bootstrap_class_by_name("java/lang/ref/Reference")
        .and_then(|reference_cid| {
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(
                reference_cid,
                "next",
                cm.class_store(),
            )
        })
        // `java/lang/ref/Reference` should always be loaded by the time any
        // Reference object exists to enqueue; this fallback only guards
        // against that invariant somehow not holding.
        .unwrap_or(2)
}

/// Process weak/soft references after a GC cycle.
/// Calls the ReferenceProcessor, nulls referent fields of cleared references,
/// and relocates ref processor addresses using the pointer map.
fn process_references_after_gc(
    shared: &SharedVm,
    pointer_map: &std::collections::HashMap<usize, usize>,
) {
    // HIB-CV-24 (Manifestation B): reconcile the defining-loader side-table with
    // this collection. A user `ClassLoader` the application no longer references
    // is now collectable (it is not GC-rooted under `CRATONVM_LOADER_UNLOAD`);
    // prune its stale side-table entry and remap survivors using the SAME
    // survivor predicate the reference processor uses below. Done BEFORE the
    // diagnostic `no_refproc` short-circuit so the side-table never holds a
    // dangling/stale ObjectRef after a collection, independent of that switch.
    {
        let is_marked = |addr: usize| -> bool {
            pointer_map.contains_key(&addr) || shared.mem.heap.is_addr_live(addr)
        };
        let dead_class_hints = cratonvm_native_builtins::classloader::gc_reconcile_defining_loaders(
            &is_marked,
            pointer_map,
        );
        let unload = crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints);
        if unload.classes_unloaded != 0 {
            tracing::debug!(
                loaders = unload.loaders_unloaded,
                classes = unload.classes_unloaded,
                jit_entries = unload.jit_entries_retired,
                "class-loader metadata unloaded"
            );
        }
        // Prune dead entries from the overlay-backed-collection side-tables
        // (LinkedList / LinkedHashMap / TreeMap / TreeSet — `roots.rs` step 17
        // / `native_collections::gc_scan_collection_overlay_roots`). This
        // function existed but was never called from anywhere in the tree
        // (confirmed: `gc_prune_dead_collection_overlays` had zero call
        // sites) — a collection whose OWN object becomes genuinely
        // unreachable left its registry entry (and every element it ever
        // held) permanently behind, since nothing ever shrank these tables.
        // Wiring this in is a real, independent, safe fix (verified: prunes
        // ~1000/1470 stale entries per GC cycle in the Tomcat suite) with no
        // change to rooting behavior — it only removes bookkeeping for
        // collections `is_marked` already agrees are dead.
        //
        // NOTE: this does NOT fully close
        // `docs/known-issues/tomcat-08-07/defaultinstancemanager-classunloading-count-mismatch.md`.
        // `roots.rs` step 17 itself has a separate, deeper bug this session
        // found but did not fix: `gc_scan_collection_overlay_roots` roots
        // EVERY element of EVERY overlay-backed collection unconditionally,
        // with no gate on whether the backing collection is reachable. A
        // scratch `List<StackMapFrame>` the JDT compiler uses transiently
        // during JSP compilation gets its elements force-rooted this way;
        // forward-tracing from that illegitimate root walks back through the
        // compiler's real field references into the evicted JSP's
        // `JspServletWrapper` and its `ClassLoader`, keeping the whole
        // cluster permanently, artificially reachable — confirmed via a
        // root-membership closure check (21 direct hits, all contributed by
        // step 17, not by any other root source). A full fix needs the same
        // conditional-rooting + mark-time-propagation treatment this session
        // gave `class_mirrors` (see `cratonvm_types::mirror_pin`), but scoped
        // to every overlay table instead of just one cache — a materially
        // larger, higher-risk change than fit in this session; left for a
        // dedicated follow-up.
        //
        // Called here (not `update_all_roots`/gc.rs, where the existing
        // remap call `gc_update_collection_overlay_refs` lives) for the same
        // reason `reconcile_class_mirrors` is here and not there:
        // `update_all_roots` early-returns when `pointer_map` is empty (the
        // common case for the non-moving JIT-active sweep), so it would
        // never run for that path. `is_marked` already handles PRE-GC
        // addresses correctly for both the moving and non-moving cases
        // (pointer_map lookup for moved survivors, `is_addr_live` for
        // not-moved ones) — the same pattern `gc_reconcile_defining_loaders`
        // above already relies on — so pruning here with pre-remap addresses
        // is correct; the later `gc_update_collection_overlay_refs` remap
        // pass in `update_all_roots` only touches whatever prune left behind.
        //
        // MEASURED FALSE-DEAD (2026-08-01): the paragraph above is wrong about
        // which addresses reach here. `run_non_moving_young_cycle` calls
        // `remap_external_roots` INSIDE the collector, so by this point
        // `slot.last_ptr` is already POST-GC — and `is_marked`'s first arm
        // (`pointer_map.contains_key`) only ever matches a PRE-GC address, so
        // for anything that moved the whole verdict falls to `is_addr_live`.
        // `ROverlaySystemGcStress` shows that verdict killing 10 LIVE
        // collections in one cycle (`dead_keys=10`, immediately followed by a
        // populated `TreeMap` reading back as `size 0`). A false-dead here is
        // silent data loss with no dangling pointer for any verifier to find;
        // a false-live is one cycle of retained bookkeeping. The two are not
        // symmetric and this predicate treats them as if they were.
        //
        // `CRATONVM_DBG_OVERLAY_PRUNE=1` reports every address this is about
        // to condemn, with the evidence, so the arm responsible is a fact and
        // not another inference.
        let prune_dbg =
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_OVERLAY_PRUNE").is_some();
        let is_live_for_prune = |addr: usize| -> bool {
            let live = is_marked(addr);
            if prune_dbg && !live {
                let in_heap = shared.mem.heap.is_heap_addr(addr).is_some();
                let words: [u64; 3] = if in_heap {
                    // SAFETY: `is_heap_addr` placed `addr` in a mapped region.
                    unsafe { std::ptr::read_unaligned(addr as *const [u64; 3]) }
                } else {
                    [0; 3]
                };
                let (old_alloc, young_surv, region) = shared.mem.heap.liveness_arms(addr);
                eprintln!(
                    "[overlay-prune] CONDEMNED 0x{addr:x} in_heap={in_heap} region={region} \
                     in_pointer_map={} old_gen_allocated={old_alloc} young_survivor={young_surv} \
                     w0=0x{:016x} w1=0x{:016x} w2=0x{:016x}",
                    pointer_map.contains_key(&addr),
                    words[0],
                    words[1],
                    words[2],
                );
            }
            live
        };
        cratonvm_gc::external_roots::prune_external_roots(&is_live_for_prune);
        // Opt-in audit of what prune left behind: an overlay ref pointing at a
        // reclaimed object. Here rather than in `update_all_roots` because that
        // early-returns on an empty `pointer_map`, i.e. never runs on the
        // non-moving sweep — the path a `System.gc()` takes.
        crate::memory::gc::audit_overlay_refs(shared);
        // Companion reconciliation for the class-mirror cache — see
        // `memory::gc::reconcile_class_mirrors` / `roots.rs` step 6. Same
        // "before the no_refproc short-circuit" rationale: the cache must
        // never hold a stale ObjectRef after a collection, independent of
        // that diagnostic switch.
        crate::memory::gc::reconcile_class_mirrors(shared, &is_marked);
        // Rebuild the mirror_pin registry the GC marker consults (gen_heap.rs)
        // from the now-pruned class_mirrors + just-remapped defining-loader
        // side-table, so the marker sees current addresses next cycle.
        crate::memory::gc::rebuild_mirror_pins(shared, pointer_map);
    }

    // bc math-ec 0x4 (CRATONVM_DBG_NO_REFPROC): subsystem-level exclusion
    // switch — skip ALL post-GC reference processing (clears, enqueues,
    // finalizer/cleaner submissions). If the corruption persists with this
    // set, the whole reference subsystem is exonerated in one experiment;
    // if it stops, the writer is in here. Diagnostic only (weak refs never
    // clear; memory grows).
    if no_refproc() {
        return;
    }
    // Pressure input to the SoftReference LRU policy (HotSpot's
    // `LRUMaxHeapPolicy`: clear when `idle_ms > SoftRefLRUPolicyMSPerMB *
    // free_heap_mb`). This used to be a hardcoded `64`, which pinned the
    // threshold at a constant 64 seconds of idleness no matter how full the
    // heap was, so the policy could not respond to memory pressure at all.
    // `soft_ref_policy_free_mb()` returns whole megabytes of *allocatable*
    // headroom (min of young/eden and old-gen promotion room, capped by
    // whole-heap headroom, rounded down so a sub-MB remainder reads as 0 =
    // maximum pressure) — the right figure under the non-moving fragmenting
    // collector that actually runs by default, where unused bytes and
    // obtainable bytes diverge. Read *before* taking the reference-processor
    // lock: the accessor reaches into the heap's own generation stats, and
    // there is no reason to nest those acquisitions.
    // See `docs/internal/arch-2026-07-26/refs-metaspace-unloading.md` §2/§R1.
    let free_mb = shared.mem.heap.soft_ref_policy_free_mb();
    // ClassManager is rank L10 and the reference processor is L7, so resolve
    // the JDK field before acquiring the lower-ranked processor lock.
    let reference_next_slot = gc_reference_next_slot(shared);
    let mut ref_proc = shared.mem.ref_processor.lock();

    // An object is "marked" (survived GC) if:
    // 1. It appears in the pointer map (evacuated/copied during collection), OR
    // 2. It resides in a live (non-collected) heap region (G1: Old/Humongous regions
    //    that weren't in the collection set are still live).
    // Every address reaching this predicate belongs to the reference
    // processor and was therefore published as watched by
    // `weakref_null_referents_pre_gc` — so the exact predicate applies. It
    // only differs from the permissive `is_addr_live` when this cycle
    // reclaimed old-gen storage, in which case an old-gen address absent from
    // the pointer map genuinely did not survive (see
    // `VmHeap::watched_pre_gc_addr_survived`). Fall back to the permissive
    // form when the pre-GC publication pass is switched off, since then no
    // watch set was published and no identity entries were emitted.
    let is_marked = |addr: usize| -> bool {
        if weakref_clear_enabled() {
            shared
                .mem
                .heap
                .watched_pre_gc_addr_survived(addr, pointer_map)
        } else {
            pointer_map.contains_key(&addr) || shared.mem.heap.is_addr_live(addr)
        }
    };

    // The `0` third argument is deliberate, not a second hardcode: when the
    // caller passes 0, `gc::reference` substitutes the mutator clock it
    // observes through `touch_soft_reference` (`last_observed_clock_ms`).
    let result = ref_proc.process_references(&is_marked, free_mb, 0);

    // bc math-ec 0x4 ROOT-CAUSE FIX (2026-06-09, hexdump-proven): the
    // cleared/enqueue lists hold PRE-GC addresses; `pointer_map.get(..)
    // .unwrap_or(addr)` keeps the STALE address for a Reference that was
    // RECLAIMED this cycle (a live young object is ALWAYS in the pointer map
    // after a moving young GC). Writing through that stale address corrupts
    // whatever now occupies the memory: the measured corruption was THIS
    // loop's `Object(None)` referent-clear landing mis-gridded — victim
    // payload = 0x4 (the Object discriminant), next word nulled (hexdump in
    // docs/internal/h2-testscript-segv-findings.md). The earlier
    // `num_fields < 2` guard was too weak (a phantom header at the stale
    // address can read num_slots >= 2). PRECISE criterion: a pre-GC address
    // in EITHER young semispace that is NOT a pointer-map key did not
    // survive — skip it entirely. Old-gen addresses don't move in a minor GC
    // (major relocations ARE merged into the map) and stay processed.
    // Backend-generic since 2026-07-10 (`pre_gc_addr_did_not_survive`): the
    // original closure checked only the Generational young semispaces, so it
    // was hardwired inert for G1/ZGC — dead finalize/cleaner/enqueue
    // addresses flowed through unguarded and `run_finalizers` later
    // dereferenced freed CSet memory (finalize-on-recycled-object UAF).
    // OLD-GEN RECLAMATION FIX (HIB-CV-32 family,
    // `TestMVStoreCachePerformance`): `pre_gc_addr_did_not_survive` has the
    // same old-generation blind spot `is_addr_live` had — for the Generational
    // heap it only rejects YOUNG addresses absent from the pointer map, and
    // answers "survived" for every old-gen address on the grounds that a minor
    // GC does not move old gen. That stops being true the moment the SAME
    // cycle reclaims old-gen storage (mark-compact `major_gc`, or the in-place
    // `sweep_old_gen_non_moving`): the pre-GC address of a reclaimed old-gen
    // Reference / queue / cleaner action now names the zeroed compaction tail,
    // a live object slid onto it, or a free block. The cleared/enqueue writes
    // below then landed on an innocent occupant, and — worse — the finalize
    // and cleaner loops handed that address to `run_finalizers` /
    // `run_cleaner_actions`, which INVOKE Java methods on it.
    //
    // Every address that reaches this predicate belongs to the reference
    // processor and was published as watched by
    // `weakref_null_referents_pre_gc`, so `watched_pre_gc_addr_survived` is
    // exact for it (both old-gen paths emit an identity `pointer_map` entry
    // for watched survivors). OR the two verdicts: a "did not survive" from
    // either predicate declines the write, which is always the safe
    // direction, and keeps the young rule exactly as strict as before (the
    // `bc math-ec 0x4` fix). Falls back to the young-only rule when the
    // pre-GC publication pass is off, since then nothing was watched.
    let is_stale_young = |addr: usize| -> bool {
        let young_stale = shared
            .mem
            .heap
            .pre_gc_addr_did_not_survive(addr, pointer_map);
        if !weakref_clear_enabled() {
            return young_stale;
        }
        young_stale
            || !shared
                .mem
                .heap
                .watched_pre_gc_addr_survived(addr, pointer_map)
    };

    // Null referent field (field 0) on cleared weak/soft references.
    // ROOT-CAUSE FIX (2026-06-10): once-only emission — the legacy
    // `cleared_ref_objects()` re-emitted every ever-cleared Reference on
    // EVERY GC; after the Reference died, the per-cycle null-write through
    // its recycled (and then legitimately-remapped!) address corrupted the
    // innocent object reusing the memory. A referent is nulled exactly once.
    let cleared = ref_proc.take_newly_cleared();
    for ref_addr in cleared {
        // ROOT-CAUSE guard (see `is_stale_young` above): a pre-GC young
        // address absent from the pointer map did NOT survive this GC —
        // writing the `Object(None)` clear through it would corrupt the
        // memory's new occupant (the PROVEN bc-math-ec 0x4 writer).
        if is_stale_young(ref_addr) {
            if straystack_enabled() {
                eprintln!("[refproc] SKIP dead CLEARED ref @0x{ref_addr:x} (young, not in map)");
            }
            continue;
        }
        // The ref object itself may have been relocated
        let actual_addr = pointer_map.get(&ref_addr).copied().unwrap_or(ref_addr);
        // SAFETY: actual_addr was produced by process_references and points at a valid object header within the heap arena.
        let obj_ref = unsafe { ObjectRef::from_raw(actual_addr as *mut u8) };
        // Belt-and-suspenders: a live `java.lang.ref.Reference` always has
        // >= 2 instance fields (referent, queue); a reused/zeroed slot is a
        // bare 0-field `Object`. (Kept in addition to the precise
        // `is_stale_young` guard — also covers old-gen reuse after a major GC.)
        if shared.mem.heap.num_fields(obj_ref) < 2 {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP stale CLEARED ref @0x{:x} (num_fields={})",
                    actual_addr,
                    shared.mem.heap.num_fields(obj_ref),
                );
            }
            continue;
        }
        shared.mem.heap.set_field(obj_ref, 0, Value::Object(None));
    }

    // Enqueue cleared/phantom references into their ReferenceQueues on the heap.
    // The linked-list protocol: push ref onto queue's head, use referent field as "next" ptr,
    // clear the ref's queue field (one-shot enqueue), increment queue size.
    for (ref_addr, queue_addr) in &result.to_enqueue {
        // ROOT-CAUSE guard (see `is_stale_young` above): skip the whole
        // enqueue when either the Reference or its queue did not survive —
        // the head/size/next writes below through a stale address are the
        // same proven corruption class as the cleared-referent write.
        if is_stale_young(*ref_addr) || is_stale_young(*queue_addr) {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP dead ENQUEUE ref@0x{ref_addr:x}/q@0x{queue_addr:x} (young, not in map)"
                );
            }
            continue;
        }
        let actual_ref = pointer_map.get(ref_addr).copied().unwrap_or(*ref_addr);
        let actual_q = pointer_map.get(queue_addr).copied().unwrap_or(*queue_addr);
        // SAFETY: actual_ref and actual_q were produced by process_references (with pointer_map relocation) and point at valid object headers within the heap arena.
        let ref_obj = unsafe { ObjectRef::from_raw(actual_ref as *mut u8) };
        let q_obj = unsafe { ObjectRef::from_raw(actual_q as *mut u8) }; // Cast: GC object pointer conversion
                                                                         // avrora `get_field` OOB fix (residual): `pending_queues` is keyed on
                                                                         // addresses and is re-emitted every GC. A `ReferenceQueue` reachable
                                                                         // only through this pending-enqueue record (no live Java reference) is
                                                                         // reclaimed by the sweep and its slot reused for a bare
                                                                         // `java.lang.Object`; the synthetic head/size writes below would then
                                                                         // trip the `gen_heap` out-of-bounds guard. Checking the POST-relocation
                                                                         // (`actual_q`) layout distinguishes a genuinely-dead queue (reused as a
                                                                         // 0-field `Object`) from a live one (still `>= 2` fields) — a live
                                                                         // queue, even one relocated this cycle, is remapped through
                                                                         // `pointer_map` and keeps its real layout, so legitimate enqueues are
                                                                         // unaffected. A dead queue has no consumer to `poll()` the reference
                                                                         // back out, so dropping the enqueue is correct.
        if shared.mem.heap.num_fields(q_obj) < 2 {
            continue;
        }
        // bc math-ec 0x4 STALE-REF FIX: the same reclaimed-and-reused hazard
        // applies to `ref_obj` (writes to its fields 0 and 1 below), which —
        // unlike `q_obj` — was NOT liveness-checked. A `ref_addr` not present in
        // `pointer_map` keeps its stale PRE-GC address via `unwrap_or`; if that
        // Reference was reclaimed and its slot reused, `set_field(ref_obj, ..)`
        // strays into the reusing object (and `set_field(q_obj,0,ref_obj)` would
        // publish a dangling head). A live Reference has >= 2 fields; skip the
        // whole enqueue otherwise (a dead ref has no consumer to poll it back).
        if shared.mem.heap.num_fields(ref_obj) < 2 {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP stale ENQUEUE ref @0x{:x} (num_fields={}) into q@0x{:x}",
                    actual_ref,
                    shared.mem.heap.num_fields(ref_obj),
                    actual_q,
                );
            }
            continue;
        }
        // Push onto queue's linked list head (field 0 = head, field 1 = size).
        //
        // Queue linkage uses the Reference's `next` field (slot 2, matching
        // the real-JDK `java.lang.ref.Reference` layout: referent, queue,
        // next, discovered) — NOT the referent slot. The old protocol reused
        // slot 0 (the referent) as the next pointer, so every
        // enqueued-but-not-yet-polled WeakReference answered `get()` with
        // the NEXT Reference in the queue instead of null (only the
        // first-enqueued, whose next was null, read as cleared — the
        // RefCheck `deadCleared=1/256 enqueued=256` signature). References
        // with fewer than 3 fields (legacy synthetic shape) fall back to the
        // old slot-0 linkage, which is at least consistent with the poll
        // side's identical fallback.
        let old_head = shared.mem.heap.get_field(q_obj, 0); // RQ_FIELD_HEAD
        shared
            .mem
            .heap
            .set_field(q_obj, 0, Value::Object(Some(ref_obj))); // new head
        let next_slot = if shared.mem.heap.num_fields(ref_obj) <= 2 {
            0 // legacy synthetic 2-field shape: referent, queue only
        } else {
            reference_next_slot
        };
        shared.mem.heap.set_field(ref_obj, next_slot, old_head); // REF_FIELD_NEXT
                                                                 // RQ_FIELD_SIZE. Slot 1 is `size` in the synthetic two-slot shape but
                                                                 // `queueLength` — a `long` — on a real JDK ReferenceQueue, whose own
                                                                 // `enqueue0`/`poll0` bytecode reads it back. Preserve the stored width.
        let new_size = match shared.mem.heap.get_field(q_obj, 1) {
            Value::Long(v) => Value::Long(v + 1),
            Value::Int(v) => Value::Int(v + 1),
            _ => Value::Int(1),
        };
        shared.mem.heap.set_field(q_obj, 1, new_size);
        // Mark as enqueued — sentinel Int(1) distinguishes from "never had queue"
        shared.mem.heap.set_field(ref_obj, 1, Value::Int(1)); // REF_FIELD_QUEUE = enqueued sentinel
    }

    // Enqueue objects for finalization (M8 fix: relocate via pointer_map
    // because the ref-processor holds pre-GC addresses).
    for obj_addr in &result.to_finalize {
        // Finalizable objects are rooted via `finalizer_addrs`, so a LIVE one
        // is always in the pointer map after a moving young GC; a stale young
        // address here would be dereferenced later by `run_finalizers`.
        if is_stale_young(*obj_addr) {
            if straystack_enabled() {
                eprintln!("[refproc] SKIP dead FINALIZE obj @0x{obj_addr:x} (young, not in map)");
            }
            continue;
        }
        let actual = pointer_map.get(obj_addr).copied().unwrap_or(*obj_addr);
        shared.mem.finalizer_thread.enqueue(actual);
    }

    // Relocate any cleaner actions DEFERRED from earlier GC cycles (queued but
    // not yet run because a JIT borrow was live — see `run_cleaner_actions`).
    // Their cleanable objects may have been evacuated by this collection, so
    // remap their raw addresses before any later drain dereferences them.
    shared.mem.cleaner_thread.update_after_gc(pointer_map);
    // Same for any finalizers deferred from earlier GC cycles.
    shared.mem.finalizer_thread.update_after_gc(pointer_map);

    // Submit cleaner actions — same pre-GC→post-GC relocation as above.
    // Without this, run_cleaner_actions later derefs a stale address
    // pointing at evacuated memory → SEGV at class_id_of (cleaner_probe).
    for action_addr in &result.cleaner_actions {
        // Same staleness guard as the finalize loop above.
        if is_stale_young(*action_addr) {
            if straystack_enabled() {
                eprintln!(
                    "[refproc] SKIP dead CLEANER action @0x{action_addr:x} (young, not in map)"
                );
            }
            continue;
        }
        let actual = pointer_map
            .get(action_addr)
            .copied()
            .unwrap_or(*action_addr);
        shared.mem.cleaner_thread.submit_action(actual);
    }

    // Drain ref_processor's finalization_queue → FinalizerThread (inline
    // to avoid re-locking ref_processor which we already hold).
    while let Some(obj_addr) = ref_proc.dequeue_for_finalization() {
        shared.mem.finalizer_thread.enqueue(obj_addr);
    }

    // HIB-CV-24 (Manifestation B): restore the referent slots of Weak/Phantom
    // references whose referent SURVIVED, and prune Reference objects collected
    // this cycle. The pre-collection pass (`weakref_null_referents_pre_gc`)
    // nulled every active Weak/Phantom referent slot so the marker could not keep
    // the referent alive through the live Reference. `process_references` above
    // then flagged the dead-referent entries `cleared`/`enqueued` (weak cleared /
    // phantom queued) — so the entries STILL active here are exactly those whose
    // referent survived via a strong path. Write their (relocated) referent
    // address back so `get()` keeps returning the live object; dead ones keep the
    // null slot. Runs before `update_after_gc` so processor addresses are still
    // the pre-collection (pointer-map key) view.
    if weakref_clear_enabled() {
        let active = ref_proc.weak_phantom_active_pairs();
        if dbg_weakref() && !active.is_empty() {
            eprintln!(
                "[weakref] post-gc restore pass: {} surviving weak/phantom referent(s)",
                active.len()
            );
        }
        for (ref_obj_old, referent_old) in active {
            // Locate the (possibly relocated) Reference object; skip if it did
            // not itself survive — never write through freed/reused memory.
            let ref_obj_new = match pointer_map.get(&ref_obj_old) {
                Some(&a) => a,
                None if shared
                    .mem
                    .heap
                    .watched_pre_gc_addr_survived(ref_obj_old, pointer_map) =>
                {
                    ref_obj_old
                }
                None => continue,
            };
            // The referent survived (this entry was not cleared/enqueued): find
            // its post-collection address (relocated → pointer map; old-gen
            // in place → live).
            let referent_new = match pointer_map.get(&referent_old) {
                Some(&a) => a,
                None if shared
                    .mem
                    .heap
                    .watched_pre_gc_addr_survived(referent_old, pointer_map) =>
                {
                    referent_old
                }
                // Defensive: should not happen for an active entry, but never
                // write a stale referent — leave the slot null.
                None => continue,
            };
            // SAFETY: both addresses are live post-collection object headers.
            let ro = unsafe { ObjectRef::from_raw(ref_obj_new as *mut u8) };
            let rt = unsafe { ObjectRef::from_raw(referent_new as *mut u8) };
            // HIB-WEAKREF-RECYCLE.1 (2026-07-31): same belt-and-suspenders
            // shape check the `cleared` loop above already applies, and for the
            // same documented reason — neither the pointer map nor
            // `is_addr_live` can tell a live old-gen Reference from freed
            // old-gen memory that has been recycled, because `is_old_gen_addr`
            // is a pure address-range test. A live `java.lang.ref.Reference`
            // always has >= 2 instance fields; anything else at this address is
            // the memory's new occupant, and writing slot 0 of it corrupts an
            // unrelated object (or trips the `gen_heap` OOB guard, which is how
            // this was found).
            if shared.mem.heap.num_fields(ro) < 2 {
                if straystack_enabled() {
                    eprintln!(
                        "[refproc] SKIP stale weak/phantom RESTORE ref @0x{:x} (num_fields={})",
                        ref_obj_new,
                        shared.mem.heap.num_fields(ro),
                    );
                }
                continue;
            }
            // Slot 0 = REF_FIELD_REFERENT. `set_field` fires the write barrier,
            // so a young referent restored into a promoted (old-gen) Reference
            // re-marks the old→young card.
            shared.mem.heap.set_field(ro, 0, Value::Object(Some(rt)));
        }
        // Drop entries whose Reference object was collected this cycle so the
        // side-lists stay bounded and the pre-GC null pass never dereferences a
        // freed Reference (same survivor predicate as everything above).
        ref_proc.remove_collected(&is_marked);
        // HIB-WEAKREF-RECYCLE.1 (2026-07-31): `is_marked` cannot see old-gen
        // reuse — `is_old_gen_addr` is a pure range check, so a weak/phantom
        // entry whose Reference object was reclaimed by an old-gen sweep
        // survives `remove_collected` forever and both referent passes keep
        // targeting recycled memory every cycle. Follow up with the shape test:
        // resolve each entry to its post-collection address exactly as the
        // restore loop above does, and keep it only if a `Reference` (>= 2
        // instance fields) is still what lives there. Runs BEFORE
        // `update_after_gc`, so the stored addresses are still the pre-GC view
        // the pointer map is keyed on.
        let still_a_reference = |addr: usize| -> bool {
            let cur = match pointer_map.get(&addr) {
                Some(&a) => a,
                None if shared.mem.heap.is_addr_live(addr) => addr,
                None => return false,
            };
            // SAFETY: `cur` is a heap address the collector just reported as
            // live (relocated target, or unmoved and in a live region), so its
            // object header is mapped and readable.
            let o = unsafe { ObjectRef::from_raw(cur as *mut u8) };
            shared.mem.heap.num_fields(o) >= 2
        };
        ref_proc.retain_shaped_weak_phantom(&still_a_reference);
    }

    // Relocate all addresses in the ref processor to match the new heap layout
    ref_proc.update_after_gc(pointer_map);
}

/// Try to allocate an object, running GC and retrying on failure.
/// Returns the ObjectRef or a RuntimeError::OutOfMemoryError.
///
/// Fast path: TLAB bump-pointer (no lock).
/// Medium path: refill TLAB from shared arena (one lock acquisition).
/// Slow path: GC + retry.
fn gc_alloc_object(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};

    let total_size = HEADER_SIZE + num_fields * SLOT_SIZE;

    // TLAB fast path: try thread-local bump allocation (no lock)
    let obj = if total_size <= cratonvm_gc::tlab::tlab_max_alloc() {
        if let Some(ptr) = tlab_alloc_object(thread, shared, class_id, num_fields, total_size) {
            ptr
        } else {
            // TLAB miss: fall through to shared heap
            alloc_object_shared(shared, thread, class_id, num_fields)?
        }
    } else {
        // Large object: skip TLAB, allocate directly from shared heap
        alloc_object_shared(shared, thread, class_id, num_fields)?
    };

    // If the class overrides finalize(), register the object with the
    // reference processor so GC can enqueue it for finalization (JLS §12.6).
    let has_fin = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    if has_fin {
        shared.register_finalizable(obj.as_ptr() as usize); // Cast: GC object pointer to address
    }

    // Initialize primitive-typed instance fields to their JVM default values.
    // Zero-initialized memory reads as Object(None) due to Rust enum layout,
    // which is correct for reference fields (null). But int/long/float/double
    // fields need explicit initialization to Int(0)/Long(0)/Float(0.0)/Double(0.0).
    init_primitive_fields(shared, obj, class_id);

    Ok(obj)
}

/// Initialize primitive-typed instance fields to their JVM default values.
///
/// Zero-initialized heap memory decodes as `Object(None)` via `std::ptr::read::<Value>()`.
/// This is correct for reference-typed fields (default null per JVM spec §2.3), but
/// int/boolean/byte/char/short fields must be `Int(0)`, long fields `Long(0)`,
/// float fields `Float(0.0)`, and double fields `Double(0.0)`.
///
/// We walk the class hierarchy to find all primitive instance fields and write
/// the proper typed zero value to their heap slots.
pub fn init_primitive_fields(shared: &SharedVm, obj: ObjectRef, class_id: ClassId) {
    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
    let mut cid = Some(class_id);
    while let Some(current_id) = cid {
        if let Some(class) = store.get(current_id) {
            let mut inst_idx = class.first_field_index;
            for f in &class.fields {
                if f.is_static() {
                    continue;
                }
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                let default = match desc_first {
                    b'I' | b'B' | b'C' | b'S' | b'Z' => Some(Value::Int(0)),
                    b'J' => Some(Value::Long(0)),
                    b'F' => Some(Value::Float(0.0)),
                    b'D' => Some(Value::Double(0.0)),
                    _ => None, // Reference types: already Object(None) from zero memory
                };
                if let Some(val) = default {
                    shared.mem.heap.set_field(obj, inst_idx, val);
                }
                inst_idx += 1;
            }
            cid = class.superclass;
        } else {
            break;
        }
    }
}

/// TLAB fast path for object allocation. Returns None on TLAB miss.
///
/// T19.3.G1 (GC allocation-storm): the refill size consulted here is
/// adaptive — after the first refill on a given thread the tracker
/// inside the thread's [`cratonvm_gc::Tlab`] recommends the next size
/// based on fill time and alloc count, so a thread that just burned
/// through 64 KB in under a millisecond gets a 128 KB chunk next
/// time and so on up to the documented cap. Each refill bumps
/// `shared.mem.tlab_refill_count` so operators can spot-check the
/// refill rate against the hit-rate target.
#[inline(always)]
pub(crate) fn tlab_alloc_object(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
    total_size: usize,
) -> Option<ObjectRef> {
    tlab_alloc_object_inner(thread, shared, class_id, num_fields, total_size, false)
}

/// TLAB hit-only path for the tiny byte arrays backing compact dynamic Strings.
/// The caller falls back to the heap allocator on a miss, so this never refills
/// or collects after another newly-created object is live in its native helper.
pub(crate) fn tlab_alloc_byte_array(
    thread: &mut JvmThread,
    shared: &SharedVm,
    length: usize,
) -> Option<ObjectRef> {
    use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
    let length_u32 = u32::try_from(length).ok()?;
    let total_size = HEADER_SIZE.checked_add(length)?;
    if total_size > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    let ptr = thread.tlab.alloc_initialized(total_size, 8, |ptr| {
        let header = ObjectHeader::new(
            ClassId::new(0),
            ObjectKind::Array,
            ArrayElementType::Byte,
            shared.mem.heap.next_identity_hash(),
            length_u32,
            length_u32,
        );
        // SAFETY: `ptr` is the base of a TLAB chunk the allocator just
        // reserved for this object and has not published, so nothing can race
        // the store. It is header-aligned, at least `size_of::<ObjectHeader>()`
        // bytes, and uninitialised — hence `ptr::write`, not an assignment.
        unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
    })?;
    use std::sync::atomic::Ordering;
    shared.mem.tlab_hit_count.fetch_add(1, Ordering::Relaxed);
    shared
        .mem
        .bytes_allocated_total
        .fetch_add(total_size as u64, Ordering::Relaxed);
    cratonvm_gc::a2dbg::record(
        ptr as usize,
        0,
        ObjectKind::Array as u8,
        ArrayElementType::Byte as u8,
        length_u32,
        length_u32,
        total_size,
    );
    // SAFETY: `ptr` is the just-initialised object base from the TLAB bump
    // above — non-null, header-aligned, and its header was written before this
    // point, so it is a well-formed object reference.
    Some(unsafe { ObjectRef::from_raw(ptr) })
}

/// [`tlab_alloc_object`] for the JIT allocation slow path (`jit_new_object`):
/// identical bump allocation, but the REFILL arm first asks — in O(1) —
/// whether young can supply the chunk WITHOUT another GC (bump-tail headroom
/// OR a coalesced reclaimed span via the cached-bound early-exit probe), and
/// bails to the caller's old-gen spill when it cannot.
///
/// History (why the gate looks like this): the JIT slow path historically
/// never refilled TLABs — once the inline bump's TLAB filled, EVERY
/// allocation took the global slow chain. Round 1 (2026-07-06) tried the
/// interpreter's unconditional refill: bt16 got 10x faster but bt18 (large
/// LIVE young set under the non-moving sweep) collapsed 10.6s→463s; gating
/// on `try_alloc_young_probe(requested)` still 39s (its
/// `largest_free_block` fallback FULL-SCANNED the fragmented free list per
/// allocation — 55% of wall); a bump-tail-only gate still ~119s. Round 2
/// uses the machinery that did not exist then: `young_has_free_block`
/// early-exits at the first satisfying span and fail-fasts through the
/// cached `Arena::max_free_upper` bound, so a post-sweep+coalesce young (a
/// handful of big spans) serves refills at bump speed, while a
/// genuinely-full young answers `false` in O(1) → old-gen spill exactly
/// like the historical no-refill behaviour.
#[inline(always)]
pub(crate) fn tlab_alloc_object_guarded_refill(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
    total_size: usize,
) -> Option<ObjectRef> {
    tlab_alloc_object_inner(thread, shared, class_id, num_fields, total_size, true)
}

/// DBG (CRATONVM_DBG_INVOKESTATS): counts invoke-dispatch path outcomes to
/// diagnose whether the monomorphic inline cache (`InvokeCache`) is actually
/// staying warm for a workload, or whether calls are falling through to the
/// vtable-fast / full-slow-path resolution on every call. index: 0=inline
/// cache HIT, 1=inline cache MISS, 2=vtable_fast reached, 3=execute_invoke_kind
/// (full slow path) reached. Prints a running tally every 100000 events on
/// each counter to bound output volume for long-running processes.
fn dbg_invoke_stats_record(index: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON
        .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INVOKESTATS").is_some())
    {
        return;
    }
    static COUNTS: [AtomicU64; 4] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    let n = COUNTS[index].fetch_add(1, Ordering::Relaxed) + 1;
    if n % 100000 == 1 {
        eprintln!(
            "[invokestats] cache_hit={} cache_miss={} vtable_fast={} slow_path={}",
            COUNTS[0].load(Ordering::Relaxed),
            COUNTS[1].load(Ordering::Relaxed),
            COUNTS[2].load(Ordering::Relaxed),
            COUNTS[3].load(Ordering::Relaxed),
        );
    }
}

/// DBG (CRATONVM_DBG_TLABMISS): gate-failure state dump — the live young
/// arena facts at the moment the guarded-refill young-room gate said no.
/// Sampled every 2^20 failures (plus the first).
fn dbg_refill_fail_state(shared: &SharedVm, requested: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_TLABMISS").is_some())
    {
        return;
    }
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n & 0xFFFFF == 1 {
        let (used, cap, fl, largest) = shared.mem.heap.young_arena_diag();
        eprintln!(
            "[gatefail] n={n} requested={requested} used={used}/{cap} free_list={fl} largest_free={largest} headroom={} has_free={}",
            shared.mem.heap.young_bump_headroom(requested),
            shared.mem.heap.young_has_free_block(requested),
        );
    }
}

/// DBG (CRATONVM_DBG_TLABMISS): which step of the guarded TLAB refill fails
/// and with what request size. stage: 0=young-room gate, 1=refill_tlab
/// returned None. Prints every 2^20 events per stage.
#[inline]
fn dbg_refill_fail(stage: usize, requested: usize) {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_TLABMISS").is_some())
    {
        return;
    }
    static COUNTS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
    static LAST_REQ: AtomicUsize = AtomicUsize::new(0);
    LAST_REQ.store(requested, Ordering::Relaxed);
    let n = COUNTS[stage].fetch_add(1, Ordering::Relaxed) + 1;
    if n & 0xFFFFF == 1 {
        eprintln!(
            "[refillfail] gate={} refill_none={} last_requested={}",
            COUNTS[0].load(Ordering::Relaxed),
            COUNTS[1].load(Ordering::Relaxed),
            requested,
        );
    }
}

/// Wedge-breaker for the guarded TLAB refill (perf/halfgap-20260717, the
/// second TLAB-remnant wedge — see `cratonvm_gc::tlab::FRAG_TLAB_FLOOR` for
/// the live capture).
///
/// The guarded-refill young-room gate can fail on EVERY allocation for the
/// rest of a run: tiny per-object allocations keep succeeding off free-list
/// slivers, so no allocation failure ever forces the young collection whose
/// sweep+coalesce would heal the fragmentation (observed live: 10.5 million
/// consecutive gate failures, one young GC in a 23-second run). This
/// counts CONSECUTIVE gate failures and, past a threshold, forces one
/// orchestrated collection so TLAB flow can resume.
///
/// Storm guard: a forced break re-arms only after `WEDGE_REARM_BYTES` of
/// further allocation (read from `shared.mem.bytes_allocated_total`). If the
/// collection did not heal the free list (nothing coalescable — genuinely
/// full young of live data), the gate keeps failing but no further forced
/// collections fire until real allocation progress has been made, so the
/// worst case adds one young GC per `WEDGE_REARM_BYTES` allocated — never
/// a per-allocation GC storm (the failure mode that forced the round-1
/// unconditional-refill revert documented at the JIT call site).
/// Wedge-breaker state — module-scope so the gate-pass reset in
/// `tlab_alloc_object_inner` shares the counter with the breaker.
static TLAB_GATE_CONSECUTIVE_FAILS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static TLAB_LAST_BREAK_ALLOC_TOTAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Slow-path entries since the last refill-time `needs_gc()` fire (the
/// crumb-treadmill fix in `tlab_alloc_object_inner`). Entry-counted, not
/// byte-counted: the degraded modes this guards (per-object allocation,
/// crumb-sized mini-TLABs) enter the slow path orders of magnitude more
/// often than healthy TLAB flow, so the counter accelerates exactly when
/// the wedge deepens, and a bytes-based stamp would freeze (the per-object
/// path doesn't bump `bytes_allocated_total`).
static TLAB_SLOWPATH_ENTRIES_SINCE_GC: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Gate for the two refill-time GC triggers (the wedge-breaker and the
/// `needs_gc()` consult) — the crumb-treadmill cure (10.5M consecutive
/// refill failures, one GC per 23 s run without them). **Default ON**
/// (opt out with `CRATONVM_TLAB_GC_TRIGGER=0`).
///
/// History: the triggers shipped default-OFF (perf/halfgap-20260717)
/// because the extra mid-drain collections exposed a latent walk-grid
/// corruption (bt18 5/5 wrong checksums, "cursor overshot into free
/// block"). Root cause fixed 2026-07-18: an unaligned young-arena capacity
/// (1 GiB - 4) made `refill_tlab`'s `requested.min(available)` mint
/// unaligned TLAB sizes whose free-list split remnants sat off the 8-byte
/// object grid (plus an untracked `Tlab::new` round-down sliver), derailing
/// the non-moving walk and truncating the mark oracle. See
/// docs/internal/tlab-trigger-gc-young-walk-corruption-FIXED.md; the arena
/// now enforces grid alignment end-to-end and the mark oracle fails safe
/// above a truncated walk's frontier.
fn tlab_gc_trigger_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_TLAB_GC_TRIGGER")
            .map(|v| {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            })
            .unwrap_or(true)
    })
}

fn tlab_refill_wedge_break(thread: &mut JvmThread, shared: &SharedVm) -> bool {
    use std::sync::atomic::Ordering;
    /// Consecutive gate failures before a forced collection. At the
    /// observed wedge rate this is a few milliseconds of per-object
    /// slow-path work — long enough that transient pressure never trips
    /// it, short enough that a real wedge is broken almost immediately.
    const WEDGE_BREAK_THRESHOLD: u64 = 16_384;
    /// Allocation progress required before a second forced collection.
    const WEDGE_REARM_BYTES: u64 = 64 * 1024 * 1024;

    let fails = TLAB_GATE_CONSECUTIVE_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
    if fails < WEDGE_BREAK_THRESHOLD {
        return false;
    }
    let alloc_total = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
    let last = TLAB_LAST_BREAK_ALLOC_TOTAL.load(Ordering::Relaxed);
    if last != 0 && alloc_total.saturating_sub(last) < WEDGE_REARM_BYTES {
        return false;
    }
    if TLAB_LAST_BREAK_ALLOC_TOTAL
        .compare_exchange(
            last,
            alloc_total.max(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        )
        .is_err()
    {
        // Another thread is breaking the same wedge; let it.
        return false;
    }
    TLAB_GATE_CONSECUTIVE_FAILS.store(0, Ordering::Relaxed);
    thread.tlab.retire();
    maybe_gc_forced(shared, thread);
    true
}

/// Which header [`tlab_alloc_object_inner`] should stamp on the region it
/// reserves. Everything else about the allocation — the TLAB fast path, the
/// refill gate, the wedge breakers, the retire-before-replace protocol — is
/// identical for both shapes, so arrays reuse this function rather than
/// carrying a second, less-hardened copy of that machinery.
#[derive(Clone, Copy)]
enum TlabShape {
    /// `num_fields` object slots.
    Object { num_fields: usize },
    /// `length` elements of `element_type`.
    Array {
        element_type: ArrayElementType,
        length_u32: u32,
    },
}

impl TlabShape {
    /// Stamp the shape's header at `ptr`.
    ///
    /// SAFETY: the caller must have reserved at least `HEADER_SIZE` bytes at
    /// `ptr`, 8-byte aligned and exclusively owned until the TLAB cursor is
    /// committed.
    #[inline(always)]
    unsafe fn init_header(self, ptr: *mut u8, class_id: ClassId, hash: i32) {
        match self {
            // H1: mint a fresh non-zero identity hash at allocation time so
            // the object header is never all-zero. This matches the slow-path
            // allocators (`alloc_object`/`alloc_array`) and prevents the
            // stale-pointer detector in `execute_invoke` from mis-flagging
            // legitimate `new Object()` instances as stale memory.
            TlabShape::Object { num_fields } => init_object_header(ptr, class_id, num_fields, hash),
            TlabShape::Array {
                element_type,
                length_u32,
            } => {
                use cratonvm_gc::heap::{ObjectHeader, ObjectKind};
                let header = ObjectHeader::new(
                    class_id,
                    ObjectKind::Array,
                    element_type,
                    hash,
                    length_u32,
                    length_u32,
                );
                std::ptr::write(ptr as *mut ObjectHeader, header);
            }
        }
    }
}

#[inline(always)]
fn tlab_alloc_object_inner(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    num_fields: usize,
    total_size: usize,
    refill_needs_young_room: bool,
) -> Option<ObjectRef> {
    tlab_alloc_shaped_inner(
        thread,
        shared,
        class_id,
        TlabShape::Object { num_fields },
        total_size,
        refill_needs_young_room,
    )
}

#[inline(always)]
fn tlab_alloc_shaped_inner(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    shape: TlabShape,
    total_size: usize,
    refill_needs_young_room: bool,
) -> Option<ObjectRef> {
    use std::sync::atomic::Ordering;

    // Fast path: bump-allocate from the current TLAB without taking
    // any lock. This is the steady-state path for ~99% of allocations
    // once the adaptive sizer has settled.
    if let Some(ptr) = thread.tlab.alloc_initialized(total_size, 8, |ptr| {
        let hash = shared.mem.heap.next_identity_hash();
        // SAFETY: `alloc_initialized` reserved `total_size` (>= HEADER_SIZE)
        // bytes at `ptr`, 8-byte aligned and privately owned until commit.
        unsafe { shape.init_header(ptr, class_id, hash) };
    }) {
        shared.mem.tlab_hit_count.fetch_add(1, Ordering::Relaxed);
        // Truncation-checked: usize → u64 widening is loss-free on 64-bit
        // platforms; on 32-bit the upper bound (usize::MAX ≈ 4 GiB) still
        // fits in u64 so `as u64` is exact.
        shared
            .mem
            .bytes_allocated_total
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            .fetch_add(total_size as u64, Ordering::Relaxed);
        // SAFETY: ptr was produced by TLAB allocation and points at a valid object header within the heap arena.
        return Some(unsafe { ObjectRef::from_raw(ptr) });
    }

    // Crumb-treadmill fix (perf/halfgap-20260717): the JIT allocation path
    // never consulted `needs_gc()` — it only reacted to HARD allocation
    // failure (probe/alloc returning None). With the fragmentation-floor
    // refill serving ever-smaller free-list crumbs, allocation can succeed
    // indefinitely off a degrading free list (and spill to old gen) without
    // any failure ever occurring, so the young collection whose
    // sweep+coalesce would restore full-size TLAB flow never triggers —
    // observed live as ONE young GC in a 23-second BinTreesClassic d=18 run
    // at -Xmx8g. Consult the same live-occupancy trigger the interpreter
    // path uses, once per refill (never per object), rate-limited by
    // allocation progress so a genuinely-full-of-live-data young gen cannot
    // thrash back-to-back collections (the "150 no-progress sweeps" failure
    // mode documented on `needs_gc` itself).
    // Rate limit: needs_gc() is naturally self-limiting (the collection
    // tenures survivors via selective promotion and rebuilds the free list,
    // so `live = used - free_list` collapses immediately after), but keep a
    // slow-path-entries-since-last-fire backstop against a pathological
    // live-set-≥-threshold loop. NOTE: the re-arm metric deliberately is
    // NOT `bytes_allocated_total` — the per-object slow path this guard
    // exists for never bumps that counter, so a bytes-based re-arm freezes
    // in exactly the wedge it guards (measured: 11.5M refill failures, one
    // GC, counter parked).
    if refill_needs_young_room && tlab_gc_trigger_enabled() {
        use std::sync::atomic::Ordering;
        const NEEDSGC_MIN_ENTRIES_BETWEEN_FIRES: u64 = 65_536;
        let entries = TLAB_SLOWPATH_ENTRIES_SINCE_GC.fetch_add(1, Ordering::Relaxed) + 1;
        if entries >= NEEDSGC_MIN_ENTRIES_BETWEEN_FIRES
            && shared.mem.heap.needs_gc_for_jit_allocation()
        {
            TLAB_SLOWPATH_ENTRIES_SINCE_GC.store(0, Ordering::Relaxed);
            thread.tlab.retire();
            maybe_gc_forced(shared, thread);
        }
    }

    // Slow path: the current TLAB is exhausted. Ask the adaptive sizer
    // for the next refill size, request it from the shared arena, and
    // install a fresh TLAB. The sizer looks at the just-retired TLAB's
    // fill-time and alloc-count stats to grow/shrink/keep the request.
    let requested = if thread.tlab.is_empty() {
        // First allocation on this thread — no history yet. Use the
        // "start big" baseline so a static-init burst doesn't refill
        // three times before the sizer gets a chance to weigh in.
        cratonvm_gc::tlab::initial_refill_size()
    } else {
        // Subsequent refill — consult the thread-local pressure
        // tracker attached to the just-retired TLAB.
        let n = thread.tlab.next_refill_size();
        // Never fall below the documented floor even if the tracker
        // somehow returns zero (pathological input).
        n.max(cratonvm_gc::tlab::min_tlab_size())
    };

    // JIT slow path (`tlab_alloc_object_guarded_refill`): carve a fresh TLAB
    // only when young can supply the chunk WITHOUT a GC — the O(1) bump-tail
    // check, then the amortized-O(1) reclaimed-span probe. Otherwise bail to
    // the caller's non-TLAB fallback (old-gen spill). See the wrapper doc
    // for the failure modes this gate was shaped by.
    // Fragmentation-tolerant free-block probe: any reclaimed span that can
    // hold a minimum-sized TLAB is worth refilling from — `refill_tlab`'s
    // fragmentation fallback serves the largest available block capped at
    // `requested` (see the wedge note there). Probing for the FULL
    // `requested` size wedged this gate shut on a free list made entirely of
    // just-under-`requested` split remnants (the bimodal-bt18 4.5s mode:
    // ~2 GiB of 131056-byte blocks vs a 131072-byte request, every
    // allocation crawling through the per-object slow path while the young
    // collection that would re-coalesce them never triggered).
    // Second-wedge fix (perf/halfgap-20260717): probe at the FRAGMENTATION
    // floor, not `min_tlab_size()`. Steady-state splitting converges on
    // remnants just under whatever floor this gate probes for (observed
    // live: a free list of exactly-4080-byte blocks against the old 8192
    // floor — 10.5M consecutive gate failures, every allocation in the
    // per-object slow path). `refill_tlab`'s fragmentation fallback serves
    // the largest available block at the same floor, so gate and server
    // agree — the [[tlab-remnant-wedge]] "allocator gate and server must
    // agree on satisfiability" rule, applied one level further down.
    let free_block_floor = cratonvm_gc::tlab::frag_tlab_floor().min(requested);
    if refill_needs_young_room
        && !shared.mem.heap.young_bump_headroom(requested)
        && !shared.mem.heap.young_has_free_block(free_block_floor)
    {
        dbg_refill_fail_state(shared, requested);
        // Sustained gate failure = the wedge: per-object allocations keep
        // succeeding so nothing else will ever trigger the collection that
        // coalesces the free list. Force one (rate-limited) and re-probe.
        if !tlab_gc_trigger_enabled()
            || !tlab_refill_wedge_break(thread, shared)
            || (!shared.mem.heap.young_bump_headroom(requested)
                && !shared.mem.heap.young_has_free_block(free_block_floor))
        {
            return None;
        }
    }
    // NOTE: the wedge-breaker's consecutive-failure counter is reset ONLY on
    // a successful refill below — NOT on a gate pass. The gate can pass on
    // every attempt (a ≥floor block exists, or its cached bound is stale)
    // while `refill_tlab` itself fails every time; resetting here parked the
    // counter at 1 through an 11.5M-failure wedge (measured).

    // Bug-D fix (TLAB tail-filler on refill, 2026-06-12): retire the OUTGOING
    // TLAB *before* replacing it. The fast path above returned `None` because
    // `total_size` did not fit the TLAB's REMAINING tail — not because the TLAB
    // was fully consumed. That leftover tail (up to `total_size - 1` bytes, and
    // for a large object/array refill potentially many KiB) is zeroed arena
    // memory inside the young from-space's live `[base, used)` range. Replacing
    // `thread.tlab` without retiring drops that tail un-tracked: it is neither a
    // walkable object nor a free-list hole. The moving collector never notices
    // (it traces live roots, not a linear walk), but the **non-moving young
    // sweep** that runs while JIT frames are active walks young linearly — it
    // strides into the zeroed tail, decodes it as a run of 40-byte all-zero
    // "objects", and desyncs off the object grid when the tail length is not a
    // multiple of 40, mis-reading a later object's payload as a header. That is
    // the `RemoteCIDRFilter` / `bintrees` "implausible object size" corruption
    // (a subsequent moving GC then SIGSEGVs walking the wrecked heap).
    //
    // `retire()` installs a synthetic `int[]` filler over `[cursor, end)` so the
    // walker strides the tail in O(1); it is a no-op on an empty (first-alloc)
    // TLAB. `requested` (read from the outgoing TLAB's pressure tracker above)
    // is already computed, so retiring here does not disturb the sizer.
    thread.tlab.retire();

    let mut refill = shared.mem.heap.refill_tlab(requested);
    if refill.is_none() {
        dbg_refill_fail(1, requested);
        // Second-wedge fix, stage-1 arm (perf/halfgap-20260717): the gate
        // above can keep PASSING on a stale cached free-block bound while
        // the real free list has degraded to sub-floor dust, so
        // `refill_tlab` itself is where a wedge can spin (observed live:
        // 9.4M consecutive stage-1 failures with ZERO stage-0 gate
        // failures). Count these toward the same breaker; on a sustained
        // run force one coalescing collection and retry the refill once.
        if refill_needs_young_room
            && tlab_gc_trigger_enabled()
            && tlab_refill_wedge_break(thread, shared)
        {
            refill = shared.mem.heap.refill_tlab(requested);
        }
    } else {
        TLAB_GATE_CONSECUTIVE_FAILS.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some((buf, size)) = refill {
        shared.mem.tlab_refill_count.fetch_add(1, Ordering::Relaxed);
        // SAFETY: buf and size were just returned by the arena allocator and the memory is zeroed.
        thread.tlab = unsafe { cratonvm_gc::Tlab::new(buf, size) };
        // Start the new refill-window timer so `next_refill_size`
        // measures this TLAB's lifetime from the moment we installed it.
        thread.tlab.begin_refill(size);
        if let Some(ptr) = thread.tlab.alloc_initialized(total_size, 8, |ptr| {
            // H1: see fast-path comment above.
            let hash = shared.mem.heap.next_identity_hash();
            // SAFETY: same contract as the fast path — a freshly reserved,
            // 8-byte-aligned, privately-owned `total_size` region.
            unsafe { shape.init_header(ptr, class_id, hash) };
        }) {
            shared.mem.tlab_hit_count.fetch_add(1, Ordering::Relaxed);
            shared
                .mem
                .bytes_allocated_total
                // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                .fetch_add(total_size as u64, Ordering::Relaxed);
            // SAFETY: ptr was produced by TLAB allocation and points at a valid object header within the heap arena.
            return Some(unsafe { ObjectRef::from_raw(ptr) });
        }
    }
    None
}

/// Initialize an object header at the given pointer.
///
/// H1: `identity_hash_code` is now eagerly assigned at allocation time
/// (caller passes `shared.mem.heap.next_identity_hash()`). The previous
/// behavior of storing 0 and "lazily" filling on first `hashCode()` call
/// was not actually wired up anywhere — every fresh TLAB-allocated
/// `new Object()` (cid=0, fields=0) produced an all-zero first 16 bytes
/// of header that the stale-pointer detector in `execute_invoke`
/// mis-flagged as stale memory, causing CGLIB's HashMap operations to
/// emit spurious "Stale pointer detected" warnings on every legitimate
/// `Object` key. The non-TLAB allocators in `gc::heap`/`gc::gen_heap`/
/// `gc::g1` have always assigned a fresh hash here; this brings the
/// fast path into agreement with them.
#[inline(always)]
fn init_object_header(ptr: *mut u8, class_id: ClassId, num_fields: usize, identity_hash_code: i32) {
    use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind};
    let header = ObjectHeader::new(
        class_id,
        ObjectKind::Object,
        ArrayElementType::Reference,
        identity_hash_code,
        0,
        // A class-file field table is u16-sized, so this is unreachable for a
        // verified Java class. Keep the allocation path panic-free if a corrupt
        // synthetic caller nevertheless violates that invariant.
        u32::try_from(num_fields).unwrap_or(u32::MAX),
    );
    // SAFETY: ptr points to freshly allocated, properly aligned memory for an ObjectHeader.
    unsafe { std::ptr::write(ptr as *mut ObjectHeader, header) };
    // A2 breadcrumb (CRATONVM_DBG_A2): the interpreter TLAB fast path bypasses
    // gen_heap, so record the legacy-layout object header it writes here.
    cratonvm_gc::a2dbg::record(
        ptr as usize,
        class_id.as_u32(),
        ObjectKind::Object as u8,
        ArrayElementType::Reference as u8,
        0,
        u32::try_from(num_fields).unwrap_or(u32::MAX),
        cratonvm_gc::heap::HEADER_SIZE + num_fields * cratonvm_gc::heap::SLOT_SIZE,
    );
}

/// Shared-heap allocation path (with lock). Used for TLAB misses and large objects.
pub(crate) fn alloc_object_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    use cratonvm_gc::heap::{HEADER_SIZE, SLOT_SIZE};
    let total_size = HEADER_SIZE + num_fields.saturating_mul(SLOT_SIZE);
    if let Some(obj) = shared.mem.heap.try_alloc_object(class_id, num_fields) {
        // T19.3.G1 — slow-path bytes count toward the allocation rate
        // just like TLAB-served bytes, so `--verbose:gc` reflects true
        // throughput even for large objects that skipped the TLAB.
        // Widening: usize → u64 is loss-free on all supported targets.
        shared
            .mem
            .bytes_allocated_total
            .fetch_add(total_size as u64, std::sync::atomic::Ordering::Relaxed);
        return Ok(obj);
    }
    // Retire TLAB before GC — its memory is in the arena that will be collected
    thread.tlab.retire();
    maybe_gc_forced(shared, thread);
    // GC-overhead limit: if repeated forced GCs have freed almost nothing, the
    // heap is full of live objects — declare OOM now rather than retrying into a
    // death-spiral (a sliver freed each cycle would otherwise let allocation
    // limp on, GC-thrashing). The catch site / drain surfaces the singleton.
    if gc_overhead_limit_exceeded(shared) {
        maybe_dump_heap_on_oom(shared, thread);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (alloc_object with {} fields)", num_fields),
            },
        )));
    }
    if let Some(obj) = shared.mem.heap.try_alloc_object(class_id, num_fields) {
        shared
            .mem
            .bytes_allocated_total
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            .fetch_add(total_size as u64, std::sync::atomic::Ordering::Relaxed);
        return Ok(obj);
    }
    // G1 last-ditch: see `gc_alloc_array` — dead Old/humongous spans need a
    // completed mark cycle's cleanup; run one synchronously and retry once.
    g1_force_full_cycle(shared, thread);
    shared
        .mem
        .heap
        .try_alloc_object(class_id, num_fields)
        .map(|obj| {
            shared
                .mem
                .bytes_allocated_total
                .fetch_add(total_size as u64, std::sync::atomic::Ordering::Relaxed);
            obj
        })
        .ok_or_else(|| {
            // T1.7.7 — write an HPROF dump on OOM if `-XX:+HeapDumpOnOutOfMemoryError`.
            maybe_dump_heap_on_oom(shared, thread);
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (alloc_object with {} fields)", num_fields),
            }))
        })
}

/// T1.7.7 — write an HPROF heap dump when allocation fails and the
/// `heap_dump_on_oom` flag is set. Best-effort: any I/O error is logged
/// but does not propagate, because we are *already* about to throw OOM
/// and replacing that with an I/O error would be worse for the user.
///
/// The dump runs at most once per VM lifetime (gated by an atomic
/// flag on the shared VM) so a tight allocation loop doesn't write
/// thousands of dumps.
fn maybe_dump_heap_on_oom(shared: &SharedVm, thread: &JvmThread) {
    use std::sync::atomic::Ordering;
    if !shared.config.heap_dump_on_oom {
        return;
    }
    if shared
        .debug
        .oom_dump_written
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return; // already written
    }
    let path = shared
        .config
        .heap_dump_path
        .clone()
        .unwrap_or_else(|| format!("./java_pid{}.hprof", std::process::id()));
    let arc = shared.get_arc();
    // obsaudit D11: pass this thread's id so dump_heap can request a real
    // stop-the-world pause for the walk instead of racing live mutators.
    match crate::runtime::hprof::dump_heap(&arc, &path, thread.thread_id) {
        Ok(bytes) => tracing::error!(
            "wrote {} byte HPROF heap dump to {} on OutOfMemoryError",
            bytes,
            path
        ),
        Err(e) => tracing::error!("failed to write HPROF heap dump on OutOfMemoryError: {}", e),
    }
}

/// TLAB hit-only fast path for array allocation — the array twin of
/// [`tlab_alloc_object`], and the general-element-type twin of
/// [`tlab_alloc_byte_array`].
///
/// Why this exists: object allocation has had a TLAB fast path for a long
/// time, but *array* allocation never did — `gc_alloc_array` went straight to
/// `heap.try_alloc_array`, and every young allocation there takes the single
/// global `young_from` mutex and holds it across the bump, the `write_bytes`
/// zeroing of the whole object, and the header `init`. Because the zeroing is
/// inside the lock, the hold time grows with the allocation, so arrays
/// serialise harder than objects do.
///
/// Measured on this box (`apps/hib-suite-runner/AllocScaleProbe.java`,
/// aggregate throughput, 1 -> 4 threads):
///
/// | shape        | 1 thread | 4 threads | scaling |
/// |--------------|---------:|----------:|--------:|
/// | `new Object()` (TLAB) | 19.8 Mops/s | 41.4 Mops/s | 2.10x |
/// | `new long[16]` (no TLAB) | 13.1 Mops/s | 11.9 Mops/s | **0.91x** |
///
/// HotSpot scales the same array shape ~27x over the same range. Array
/// allocation was therefore capped at roughly one thread's worth of
/// throughput no matter how many cores were available — which is why a
/// 5-thread allocation-heavy workload (an ORM opening sessions and building
/// `ArrayList`/`HashMap`/`StringBuilder` backing arrays) could not use them.
///
/// It shares [`tlab_alloc_shaped_inner`] with the object path, so it inherits
/// that path's already-hardened refill gate, wedge breakers and
/// retire-before-replace protocol verbatim rather than carrying a second copy
/// of them. A hit-only first attempt is not enough on its own: a workload that
/// allocates mostly arrays drains the TLAB and then misses back to the global
/// lock on every subsequent allocation (measured — hit-only bought ~23% on one
/// thread and moved multi-thread scaling not at all).
///
/// Zeroing: [`GenerationalHeap::refill_tlab`] zeroes the whole TLAB region
/// when it is handed out, so the array body already reads back as Java's
/// mandated default values and the initializer only has to write the header —
/// the same contract [`tlab_alloc_byte_array`] relies on.
fn tlab_alloc_array(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    element_type: ArrayElementType,
    length: usize,
) -> Option<ObjectRef> {
    use cratonvm_gc::heap::{ObjectKind, HEADER_SIZE};
    let length_u32 = u32::try_from(length).ok()?;
    let data_size = cratonvm_gc::heap::array_data_size_checked(length, element_type)?;
    let total_size = HEADER_SIZE.checked_add(data_size)?;
    // Keep well clear of the humongous threshold: anything at or above the
    // TLAB's per-allocation cap goes down the ordinary path, which owns the
    // young-vs-old-gen routing decision.
    if total_size > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    let obj = tlab_alloc_shaped_inner(
        thread,
        shared,
        class_id,
        TlabShape::Array {
            element_type,
            length_u32,
        },
        total_size,
        // Same value the interpreter's object path passes: the young-room
        // pre-check is the JIT slow path's gate, not this one's.
        false,
    )?;
    cratonvm_gc::a2dbg::record(
        obj.as_ptr() as usize,
        class_id.as_u32(),
        ObjectKind::Array as u8,
        element_type as u8,
        length_u32,
        length_u32,
        total_size,
    );
    Some(obj)
}

/// Try to allocate an array, running GC and retrying on failure.
///
/// `try_alloc_array_full` is deliberately used rather than the young-only
/// `try_alloc_array`: it has the same young-first policy, but once a
/// non-moving JIT-safe sweep has left the young generation fragmented it can
/// spill the request into old space.  This matches `alloc_object_shared`'s
/// object path.  Retrying young-only here used to report OOM for a tiny array
/// while most of the heap was available as old-generation headroom.
fn gc_alloc_array(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    element_type: ArrayElementType,
    length: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(arr) = tlab_alloc_array(thread, shared, class_id, element_type, length) {
        return Ok(arr);
    }
    if let Some(arr) = shared
        .mem
        .heap
        .try_alloc_array_full(class_id, element_type, length)
    {
        return Ok(arr);
    }
    // Retire TLAB before GC
    thread.tlab.retire();
    maybe_gc_forced(shared, thread);
    // GC-overhead limit (see alloc_object_shared): bail to OOM if the heap is
    // GC-thrashing rather than spinning on slivers.
    if gc_overhead_limit_exceeded(shared) {
        maybe_dump_heap_on_oom(shared, thread);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (alloc_array length {})", length),
            },
        )));
    }
    if let Some(arr) = shared
        .mem
        .heap
        .try_alloc_array_full(class_id, element_type, length)
    {
        return Ok(arr);
    }
    // G1 last-ditch: the young pause above cannot reclaim dead Old/humongous
    // spans — only a completed mark cycle's cleanup can. Run one
    // synchronously and retry once before surfacing OOM.
    g1_force_full_cycle(shared, thread);
    shared
        .mem
        .heap
        .try_alloc_array_full(class_id, element_type, length)
        .ok_or_else(|| {
            maybe_dump_heap_on_oom(shared, thread);
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (alloc_array length {})", length),
            }))
        })
}

/// Update the thread's root snapshot with current frame ObjectRefs.
/// Called at safepoints and before blocking operations.
///
/// **Spring Boot SEGV fix (2026-05-16):** `ValueStack::scan_object_refs`
/// still reports every `CompactTag::Long` operand-stack slot whose bits
/// happen to look like an aligned pointer as a heap root, without consulting
/// the heap. That mirrors the bug already removed from
/// `Frame::scan_local_objects` (see frame.rs) but the operand-stack file is
/// restricted from edits. Filter the freshly-scanned operand-stack roots
/// against `heap.is_object_address` so a primitive `long` carrying e.g. a
/// file size, hash code, or jboss-modules-internal token can no longer
/// poison the root set and cause a `0xC0000005` SEGV when the GC later
/// dereferences it. Locals (already cleaned), `native_pin_roots`, and
/// `native_pending_return` come from validated paths and are appended
/// after the filter.
/// DBG (CRATONVM_DBG_ROOTSNAP): instrumentation for the per-native-call root
/// snapshot cost. Confirms/quantifies whether `update_root_snapshot` is the
/// embedded-server deployment hotspot (O(stack-depth) full-frame scan + the
/// per-operand-stack-object `is_object_address` triple-lock validation, run on
/// every object-returning native call). Prints a cumulative line every 200k
/// calls. Default-off; zero cost when the gate is unset (cached OnceLock).
fn rootsnap_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOTSNAP").is_some())
}
/// DBG (CRATONVM_DBG_ROOTSNAP_VERIFY): after the frozen-frame cache builds a
/// snapshot, re-scan every frame the default (uncached) way and diff the two.
/// The cached path claims to be byte-identical to the default path; any root
/// the fresh scan reports that the cached snapshot lacks is a LOST ROOT —
/// precisely the failure the retired real-ForkJoinPool bypass claimed to
/// prevent, and the check that retired it (see `update_root_snapshot`). Prints
/// a line per miss plus a periodic tally; run it under `CRATONVM_DBG_ROOTSNAP`
/// so the tally shows how much of the snapshot came from the cache. Costs a
/// full extra frame scan per snapshot, so it is a diagnostic, not a default:
/// unset, this is one cached `OnceLock` read.
fn rootsnap_verify_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOTSNAP_VERIFY").is_some()
    })
}

/// Print period for the two DBG tallies (`CRATONVM_DBG_ROOTSNAP_EVERY`,
/// default 200k). Short workloads publish far fewer snapshots than that, so a
/// smaller period is what makes "the cache was actually exercised" visible
/// rather than assumed.
fn rootsnap_dbg_every() -> u64 {
    use std::sync::OnceLock;
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_ROOTSNAP_EVERY")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(200_000)
    })
}

static ROOTSNAP_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// Cache engagement (`CRATONVM_DBG_ROOTSNAP`): snapshots that took the cached
// path, and how many frames / roots those reused instead of re-scanning.
static ROOTSNAP_CACHED_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_REUSED_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_REUSED_ROOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// Cache soundness (`CRATONVM_DBG_ROOTSNAP_VERIFY`): snapshots diffed against a
// fresh full scan, and the roots the cached snapshot was missing.
static ROOTSNAP_VERIFIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ROOTSNAP_MISS_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Scan ONE frame's GC roots (locals + operand stack, with the operand-stack
/// pointer-shaped-Long validation) onto the end of `out`. This is exactly the
/// per-frame body of `update_root_snapshot`'s loop, factored out so the opt-in
/// root-snapshot cache can scan a single frame into a cache entry as well as
/// into the live snapshot. Behaviour is byte-identical to the inline loop.
#[inline]
fn scan_frame_roots(frame: &Frame, out: &mut Vec<ObjectRef>, heap: &crate::memory::VmHeap) {
    frame.scan_local_objects(out, heap);
    let before = out.len();
    frame.stack.scan_object_refs(out, heap);
    let len = out.len();
    if len > before {
        let mut write = before;
        for read in before..len {
            let o = out[read];
            // Cast: object/code pointer to integer address
            if heap.is_object_address(o.as_ptr() as usize).is_some() {
                out[write] = o;
                write += 1;
            }
        }
        out.truncate(write);
    }
}

pub(crate) fn update_root_snapshot(shared: &SharedVm, thread: &mut JvmThread) {
    remap_trace_push(shared, thread, "publish", "");
    // cceres3 FIX: self-heal a leaked blocked-region exit. If a blocking
    // native returned without `check_post_block_gc` (unpaired exit), this
    // thread is running with an unconsumed fixup chain / slot-origin set —
    // its frames still hold from-space addresses from every GC it slept
    // through. Apply them here, at the first safepoint publish, before this
    // thread's stale refs can leak into reachable object graphs.
    {
        let pending = !thread.gc_block_state.fixup.lock().is_empty()
            || thread
                .gc_block_state
                .slot_origins
                .lock()
                .iter()
                .any(|so| so.cur != so.orig);
        if pending {
            let n = crate::vm::vm_exec::apply_pending_blocked_fixups(shared, thread);
            if n > 0 && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some() {
                eprintln!(
                    "[blockgc] SAFEPOINT-HEAL tid={} applied {} pending fixups (leaked blocked-region exit upstream)",
                    thread.thread_id.0, n,
                );
            }
        }
        // A raised flag at an interpreter safepoint means some raise site's
        // exit skipped `check_post_block_gc` (the monitor_wait early-return
        // bug class): this thread is RUNNING, yet every census still excludes
        // it, so moving collections keep completing under its feet. Restore
        // the invariant: wait out any in-flight pause and clear the flag
        // (idempotent with the eventual legitimate wake, whose fixup-take
        // then finds an empty map).
        if thread
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire)
        {
            shared.mem.gc_barrier.leave_blocked_region_flagged(
                thread.thread_id,
                &thread.gc_block_state.in_blocked_region,
            );
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some() {
                eprintln!(
                    "[blockgc] SAFEPOINT-FLAG-CLEAR tid={} - in_blocked_region was raised on a running thread",
                    thread.thread_id.0,
                );
            }
        }
    }
    // DIAGNOSTIC-ONLY (cceres3): first-miss hunter. Once per GC epoch per
    // thread, verify no frame slot holds an already-forwarded (quarantined)
    // address at the safepoint publish. A hit here bounds the miss window to
    // "since the previous safepoint" on a RUNNING thread, which none of the
    // DEPOSIT/WAKE/ARRIVE verifiers can see.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some() {
        thread_local! {
            static LAST_CC: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
        }
        let cc = shared.mem.heap.collection_count();
        let prev = LAST_CC.with(|c| c.replace(cc));
        if cc != prev && prev != u64::MAX {
            for (fi, fr) in thread.frames.iter().enumerate() {
                for li in 0..fr.locals_len() {
                    if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = shared.mem.heap.debug_forwarded_target(a) {
                            eprintln!(
                                "[blockgc] SAFEPOINT-STALE e{cc} tid={} frame#{fi} {}.{} pc={} local[{li}] 0x{a:x}->0x{new:x}",
                                thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            );
                        }
                    }
                }
                for si in 0..fr.stack.len() {
                    if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = shared.mem.heap.debug_forwarded_target(a) {
                            eprintln!(
                                "[blockgc] SAFEPOINT-STALE e{cc} tid={} frame#{fi} {}.{} pc={} stack[{si}] 0x{a:x}->0x{new:x}",
                                thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            );
                        }
                    }
                }
            }
        }
    }

    let _rs_t0 = if rootsnap_dbg_enabled() {
        Some((std::time::Instant::now(), thread.frames.len()))
    } else {
        None
    };
    // Lock via an Arc clone so the guard does not borrow `thread`, leaving the
    // disjoint `frames` / `rs_cache` fields freely (mutably) borrowable below.
    let snap_arc = thread.root_snapshot.clone();
    let mut snapshot = snap_arc.lock();
    snapshot.clear();

    // The frozen-frame cache yields to the default path while the
    // conservative-locals hardening is engaged: the cached `scan_frame_roots`
    // does not run `scan_locals_conservative`, so caching under it could drop a
    // lost-tag local from the snapshot. `conservative_locals_enabled()` reads
    // the RESOLVED real-ForkJoinPool flag and is additionally gated on GC
    // quiescence, i.e. it answers "is the hardening running right now" rather
    // than "was the lane explicitly requested".
    //
    // There is no separate real-ForkJoinPool bypass any more (2026-07-31): the
    // old one keyed off the PRESENCE of `CRATONVM_REAL_FORKJOINPOOL`, which
    // stopped being set when the real pool became the default, so it had not
    // fired on a default run since that flip. Before removing it the cache was
    // measured against the loss it claimed to prevent — see
    // `rootsnap_verify_enabled` (`CRATONVM_DBG_ROOTSNAP_VERIFY`), which
    // re-scans every frame the uncached way after each cached snapshot and
    // reports any root the cached snapshot lacks; the real-lane
    // Fork6/Fork6Hard GC-stress repros reported none.
    //
    // The mechanism behind that result: the real-FJP lane keeps a Bridge native
    // surface for `submit`/`invoke`/`fork`/`join`, which runs every task INLINE
    // on the submitting thread (measured: every `compute()` in this lane runs
    // on `main`, vs 15 pool workers on HotSpot). The hazard the bypass
    // described — a pool worker holding a forked subtask in its own frames
    // across a peer-triggered collection — has no thread to happen on. If real
    // ForkJoinPool ever gains workers that actually execute tasks, re-run the
    // verifier against that lane before trusting this.
    let conservative_locals = crate::memory::roots::conservative_locals_enabled();
    if crate::runtime::env_cache::rootsnap_cache() && !conservative_locals {
        // ── Frozen-frame cached path ────────────────────────────────────────
        // Reuse the cached roots of the deep, continuously-frozen frames and
        // re-scan only the churning top. Correctness rests on the LIFO stack
        // discipline: if `frames[k]` is still the SAME instance (`seq`
        // unchanged) it has never been popped, so by the stack property every
        // frame *below* it (`0..k`) has been continuously present AND frozen
        // (you cannot pop the middle of a stack) — their cached roots are still
        // exact. A GC may have *moved/promoted* objects, changing addresses, so
        // the whole cache is only valid while `collection_count()` is unchanged.
        let gen = shared.mem.heap.collection_count();
        let len = thread.frames.len();
        // Longest prefix whose frame instances are unchanged since the cache
        // was built (prefix-closed: a matching `frames[p]` implies every frame
        // below it matches too).
        let mut p = 0usize;
        if gen == thread.rs_cache_gen {
            let maxk = thread.rs_cache.len().min(len);
            while p < maxk
                && thread.frames[p].seq != 0
                // Key on (seq, exec_epoch): seq proves the frame was never
                // popped; exec_epoch proves it has not RE-EXECUTED (and thus
                // possibly reassigned a local) since it was cached. A mismatch in
                // either ends the reusable prefix so this frame and all above it
                // are re-scanned — the fix for the stale-reassigned-local AME.
                && (thread.frames[p].seq, thread.frames[p].exec_epoch) == thread.rs_cache[p].0
            {
                p += 1;
            }
        }
        // The deepest matching frame (`p-1`) VOUCHES for everything strictly
        // below it, but its own above-neighbour is the divergence point, so its
        // own roots may be stale — exclude it. Never reuse the current top
        // (index `len-1`), which churns its operand stack between snapshots.
        let reuse = p.saturating_sub(1).min(len.saturating_sub(1));

        // Build the next cache as we go (frozen frames `0..len-1`); the top is
        // never cached.
        let mut new_cache: Vec<((u64, u64), Vec<ObjectRef>)> =
            Vec::with_capacity(len.saturating_sub(1));
        if _rs_t0.is_some() {
            use std::sync::atomic::Ordering::Relaxed;
            ROOTSNAP_CACHED_CALLS.fetch_add(1, Relaxed);
            ROOTSNAP_REUSED_FRAMES.fetch_add(reuse as u64, Relaxed);
            ROOTSNAP_REUSED_ROOTS.fetch_add(
                thread.rs_cache[..reuse]
                    .iter()
                    .map(|e| e.1.len() as u64)
                    .sum::<u64>(),
                Relaxed,
            );
        }
        // (a) reused frozen frames — copy their cached roots into the snapshot
        //     and carry the entry forward (move, no re-alloc).
        for i in 0..reuse {
            snapshot.extend_from_slice(&thread.rs_cache[i].1);
            new_cache.push(std::mem::take(&mut thread.rs_cache[i]));
        }
        // (b) re-scan `reuse..len`. Frozen ones (`< len-1`) are scanned into the
        //     snapshot and cached; the top is scanned into the snapshot only.
        for i in reuse..len {
            let start = snapshot.len();
            scan_frame_roots(&thread.frames[i], &mut snapshot, &shared.mem.heap);
            if i + 1 < len {
                new_cache.push((
                    (thread.frames[i].seq, thread.frames[i].exec_epoch),
                    snapshot[start..].to_vec(),
                ));
            }
        }
        thread.rs_cache = new_cache;
        thread.rs_cache_gen = gen;
        if rootsnap_verify_enabled() {
            use std::sync::atomic::Ordering::Relaxed;
            let mut fresh: Vec<ObjectRef> = Vec::with_capacity(snapshot.len());
            for frame in &thread.frames {
                scan_frame_roots(frame, &mut fresh, &shared.mem.heap);
            }
            ROOTSNAP_VERIFIED.fetch_add(1, Relaxed);
            let have: std::collections::HashSet<usize> =
                snapshot.iter().map(|o| o.as_ptr() as usize).collect();
            let mut missed = 0u64;
            for (i, o) in fresh.iter().enumerate() {
                let a = o.as_ptr() as usize;
                if !have.contains(&a) {
                    missed += 1;
                    if ROOTSNAP_MISSES.load(Relaxed) < 40 {
                        eprintln!(
                            "[ROOTSNAP-MISS] tid={} depth={} reuse={} fresh_idx={} addr=0x{a:x} top={}.{}",
                            thread.thread_id.0,
                            len,
                            reuse,
                            i,
                            thread.frames[len - 1].class_name(),
                            thread.frames[len - 1].method_name(),
                        );
                    }
                }
            }
            if missed > 0 {
                ROOTSNAP_MISSES.fetch_add(missed, Relaxed);
                ROOTSNAP_MISS_CALLS.fetch_add(1, Relaxed);
            }
            let n = ROOTSNAP_VERIFIED.load(Relaxed);
            if n == 1 || n % rootsnap_dbg_every() == 0 {
                eprintln!(
                    "[ROOTSNAP-VERIFY] verified_snapshots={} miss_snapshots={} missed_roots={}",
                    n,
                    ROOTSNAP_MISS_CALLS.load(Relaxed),
                    ROOTSNAP_MISSES.load(Relaxed),
                );
            }
        }
    } else {
        // ── Default path (unchanged) ────────────────────────────────────────
        // Multi-thread non-moving-sweep root hardening (Fork6): see
        // `roots::conservative_locals_enabled`. Capture lost-tag object refs in
        // THIS thread's frame locals so a parked/running worker (or main)
        // publishes them, pinning them against selective-promotion evacuation.
        // (`conservative_locals` was computed above, where it also gates the
        // cached path off so this hardening is never skipped.)
        for frame in &thread.frames {
            if conservative_locals {
                // The non-moving FJP stress path uses root values as pins. A
                // liveness-filtered scan can drop an active task receiver at a
                // call boundary; all-live reference scanning only over-retains
                // in this collector and prevents reclaiming that receiver.
                frame.scan_local_objects_all_live(&mut snapshot, &shared.mem.heap);
            } else {
                frame.scan_local_objects(&mut snapshot, &shared.mem.heap);
            }
            if conservative_locals {
                frame.scan_locals_conservative(&mut snapshot, &shared.mem.heap);
            }
            let before = snapshot.len();
            frame
                .stack
                .scan_object_refs(&mut snapshot, &shared.mem.heap);
            // Validate every operand-stack-sourced root against the heap.
            // `Frame::scan_local_objects` was already cleaned to drop the
            // pointer-shaped-Long heuristic; the operand-stack scanner is
            // restricted from edits, so filter at the boundary instead.
            //
            // Done IN PLACE (compact valid entries down over the invalid ones,
            // then truncate) rather than `split_off` — `update_root_snapshot` runs
            // on every object-returning native call (tens of millions during an
            // embedded-server deploy), and the old `split_off` allocated a fresh
            // Vec for every frame that had operand-stack objects. The in-place
            // retain is allocation-free and keeps identical semantics.
            let len = snapshot.len();
            if len > before {
                let mut write = before;
                for read in before..len {
                    let o = snapshot[read];
                    // Cast: object/code pointer to integer address
                    if shared
                        .mem
                        .heap
                        .is_object_address(o.as_ptr() as usize)
                        .is_some()
                    {
                        snapshot[write] = o;
                        write += 1;
                    }
                }
                snapshot.truncate(write);
            }
            if conservative_locals {
                frame
                    .stack
                    .scan_object_refs_conservative(&mut snapshot, &shared.mem.heap);
            }
        }
    }
    snapshot.extend(thread.native_pin_roots.iter().copied());
    // Native handle scopes are outside interpreter frames just like the pin
    // stack. A peer-initiated collection sees this parked thread only through
    // `root_snapshot`, so publish every live handle slot here as well.
    snapshot.extend(thread.handle_slots.iter().flatten().copied());
    snapshot.extend(thread.native_alloc_pool.iter().copied());
    if let Some(r) = thread.native_pending_return {
        snapshot.push(r);
    }
    crate::memory::roots::push_off_frame_thread_roots(thread, &mut snapshot);
    // Direct JIT HashMap node cache: unlike the ordinary current-thread root
    // scan, a cross-thread collector can see this parked thread only through
    // `root_snapshot`.  Keep both cache handles in that snapshot so a
    // collection initiated by another worker cannot reclaim or relocate a
    // cached node behind the JIT fast path.  The three pointer-map consumers
    // (`gc.rs`, `apply_pointer_map_to_thread`, and blocked wake-up) each
    // forward these entries before the owner can read the cache again.
    for entry in &thread.jit_hashmap_string_node_cache {
        snapshot.push(entry.map);
        snapshot.push(entry.node);
        if let Some(key_object) = entry.key_object {
            snapshot.push(key_object);
        }
    }
    // TOMCAT-JNDIREALM-JIT.3 (2026-07-26) — the ASCII case-conversion cache,
    // exactly the same contract as the HashMap node cache above. It was wired
    // into `roots::collect_roots` + `gc.rs` (the INITIATOR's own scan and
    // remap) but into neither published snapshot, so a collection initiated by
    // ANOTHER thread never saw it: the non-moving young sweep reclaimed the
    // cached `first`/`second` Strings, and the owner's next
    // `get_ascii_case_string_cached` handed the freed address straight back to
    // bytecode as an all-zero-header `java/lang/String` receiver at
    // `String.equals`/`String.indexOf`. Real repro: Tomcat's
    // `TestJNDIRealmIntegration` 76-case matrix with `com/unboundid/`
    // JIT-eligible, whose `StaticUtils.toLowerCase` is the hot cache consumer.
    for entry in &thread.string_case_cache {
        snapshot.push(entry.source);
        snapshot.push(entry.first);
        snapshot.push(entry.second);
        if let Some(locale) = entry.locale {
            snapshot.push(locale);
        }
    }
    // JNI local references (INT-5, safepoint half): a JNI native that
    // obtained local refs and re-entered Java parks HERE — and a
    // cross-thread collector marks this thread only from this snapshot, so
    // an object reachable solely through this thread's `JNI_LOCAL_FRAMES`
    // was reclaimed. Thread-local storage; this deposit always runs on the
    // owning thread. The resume-side remap is `update_local_refs_after_gc`
    // in `apply_pointer_map_to_thread`.
    crate::native::jni::collect_local_ref_roots(&mut snapshot);

    // This thread's own `java.lang.Thread` mirror (and any pending async
    // exception). These live in `JvmThread` fields, not on any frame, so the
    // frame scan above never captures them — yet `Thread.currentThread()`
    // hands the mirror straight back to bytecode. When ANOTHER thread initiates
    // a collection while this one is parked, the cross-thread collector marks
    // and remaps only from this deposited snapshot (it never runs
    // `collect_roots` for a non-current thread). Without depositing the mirror
    // here it is (a) not marked — a non-moving sweep reclaims it — and (b) not
    // seeded into the blocked-thread fixup chain, so a moving collection
    // relocates it and `check_post_block_gc` leaves `self.thread.java_thread_obj`
    // dangling. Either way the next `currentThread()` returns an all-zero-header
    // object and real-JDK `Thread.getThreadGroup()` NPEs on a null `holder`
    // (Tomcat TestDigestAuthenticator: a worker-thread GC orphaned the parked
    // JUnit main thread's mirror). Depositing it closes both holes; the wake
    // remap is `check_post_block_gc`, the safepoint-resume remap is
    // `apply_pointer_map_to_thread` (both already forward `java_thread_obj`).
    if let Some(obj) = thread.java_thread_obj {
        snapshot.push(obj);
    }
    if let Some(exc) = thread.pending_async_exception {
        snapshot.push(exc);
    }

    let moving_young_precise_only = crate::jit::conservative_roots::moving_young_enabled()
        && crate::jit::conservative_roots::refresh_moving_young_coverage_for_current_thread()
        && !cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete();

    // Cross-thread JIT-root hardening: also publish the conservative roots of
    // every active JIT frame on THIS thread into the snapshot.
    //
    // The cross-thread STW collector reads each thread's `root_snapshot` (via
    // `collect_all_root_snapshots`) — it does NOT call `collect_roots` for a
    // non-current thread (that path's `scan_active_jit_frames` is thread-local
    // and would scan the *collector's* empty JIT chain, not the parked
    // worker's). So an object whose only live reference lives in a *parked*
    // worker thread's JIT spill slot was absent from the snapshot the collector
    // marks from — a latent reclamation risk for the non-moving young-gen sweep
    // (relocation is already prevented: the collector runs non-moving while any
    // thread is in JIT). `update_root_snapshot` always runs on the thread it is
    // snapshotting, so the thread-local scan captures exactly that worker's live
    // JIT spill region; empty (no-op) when not in JIT; false positives filtered
    // by `is_object_address`.
    //
    // NOTE: this closes a real latent gap but does NOT fix the avrora real-RAF
    // SEGV — that crash was experimentally shown to be NOT a GC reclamation /
    // relocation bug (it reproduces with the young sweep capturing these JIT
    // roots and with the concurrent old-gen collector disabled). See
    // docs/real-raf-segv-root-cause.md.
    if !moving_young_precise_only {
        // `update_root_snapshot` is also called at ordinary native-call
        // boundaries, not only immediately before a safepoint.  Its JIT-root
        // contribution is therefore a future cross-thread collector's only
        // view of this thread while it is parked.  Do not let the per-thread
        // scan cache republish a scan taken before the interpreted/native
        // callee below the JIT frame allocated a new live object: none of the
        // cache's keys change for that mutation.  The next collector could
        // otherwise reclaim the omitted object and hand a zero-header slot
        // back to compiled code.  This mirrors the safepoint and blocked
        // snapshot paths, both of which already invalidate before publishing.
        crate::jit::conservative_roots::invalidate_scan_cache_for_gc();
        let jit_scan_start = snapshot.len();
        crate::jit::conservative_roots::scan_active_jit_frames(&shared.mem.heap, &mut snapshot);
        // G1 pin-in-place, cross-thread half: the snapshot keeps these
        // conservatively-discovered objects ALIVE, but under G1 (a moving
        // collector) their regions must also be EXCLUDED from the collection
        // set — the JIT register/spill slots holding them cannot be
        // rewritten when the object moves. The initiator only publishes its
        // OWN JIT roots (roots.rs); every parked/blocked mutator must
        // publish here, into the process-global per-thread pin registry
        // consumed by `G1Collector::jit_pinned_region_set`. Replace
        // semantics: a deposit with no live JIT frames clears this thread's
        // stale pins.
        if shared.mem.heap.is_g1() {
            let addrs: Vec<usize> = snapshot[jit_scan_start..]
                .iter()
                .map(|r| r.as_ptr() as usize)
                .collect();
            cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&addrs);
        }
    } else if shared.mem.heap.is_g1() {
        // Precise-relocation mode covers every JIT oop with rewritable
        // shadow-stack slots — no conservative pins needed; drop stale ones.
        cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&[]);
    }

    // §4 (multi-thread shadow scan, marking half). Also publish THIS thread's
    // shadow-stack precise roots into the snapshot. With `CRATONVM_SHADOW_STACK`
    // the moving collector is allowed to run while threads are in JIT, so a
    // cross-thread STW cycle (which marks from each parked thread's
    // `root_snapshot`, never `collect_roots`) must see a parked worker's
    // shadow-held oops or they are reclaimed. Mirrors the current-thread fold-in
    // in `roots.rs`; every slot is an oop by construction (re-validated via
    // `is_object_address`). The matching remap is in `apply_pointer_map_to_thread`.
    // No-op when the gate is off or the shadow stack is empty/unallocated.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.for_each_value(|v| {
            if let Some(obj_ref) = shared.mem.heap.is_object_address(v) {
                snapshot.push(obj_ref);
            }
        });
    }
    if let Some((t0, nframes)) = _rs_t0 {
        use std::sync::atomic::Ordering::Relaxed;
        drop(snapshot); // release the lock before the (rare) print
        let calls = ROOTSNAP_CALLS.fetch_add(1, Relaxed) + 1;
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        ROOTSNAP_NANOS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        ROOTSNAP_FRAMES.fetch_add(nframes as u64, Relaxed);
        if calls == 1 || calls % rootsnap_dbg_every() == 0 {
            let nanos = ROOTSNAP_NANOS.load(Relaxed);
            let frames = ROOTSNAP_FRAMES.load(Relaxed);
            eprintln!(
                "[ROOTSNAP] calls={} total_ms={} avg_us={:.2} avg_frames={:.1} cached_calls={} reused_frames={} reused_roots={}",
                calls,
                nanos / 1_000_000,
                // Cast: numeric/representation conversion
                (nanos as f64 / calls as f64) / 1000.0,
                // Cast: numeric/representation conversion
                frames as f64 / calls as f64,
                ROOTSNAP_CACHED_CALLS.load(Relaxed),
                ROOTSNAP_REUSED_FRAMES.load(Relaxed),
                ROOTSNAP_REUSED_ROOTS.load(Relaxed),
            );
        }
    }
}

#[cfg(test)]
mod root_snapshot_cache_tests;

/// Check if a stop-the-world pause is requested and participate if so.
///
/// Called at safepoints: allocation sites and backward branches (loop iterations).
/// If STW is active, this thread deposits its roots and waits for GC to complete,
/// then applies the pointer map to update its own frame references.
pub(crate) fn safepoint_check(shared: &SharedVm, thread: &mut JvmThread) {
    use std::sync::atomic::Ordering;
    // CRATONVM_DBG_BLOCKED_ACCESS: reaching an interpreter safepoint with the
    // thread's own `in_blocked_region` flag still raised means every STW
    // census is excluding a RUNNING mutator — a moving GC can complete under
    // its feet, and nothing ever applies its accumulated blocked-fixup. This
    // catches stuck flags from any raise site whose wake/error path skipped
    // `check_post_block_gc` (the monitor_wait early-return bug class). No-op
    // when the gate is off.
    if cratonvm_gc::blocked_access_debug::enabled()
        && thread
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
    {
        cratonvm_gc::blocked_access_debug::report_blocked_violation(
            "interpreter safepoint reached with in_blocked_region raised",
            0,
        );
    }
    // bc math-ec 0x4 (CRATONVM_DBG_MEMWATCH): O(1) poll of one absolute
    // watched address at full safepoint frequency — catches the corrupting
    // write within one safepoint window, with the live Java stack. Off ⇒
    // a single predicted branch. On a HIT the dump includes the TOP frame's
    // locals (raw bits + array header/extent for in-heap-shaped values) so
    // the watched address can be placed inside/outside the frame's array
    // receivers (legit-write-to-reused-slot vs stale/OOB receiver).
    crate::runtime::memwatch::poll("safepoint", || {
        let mut s = thread
            .frames
            .iter()
            .rev()
            .take(28)
            .map(|f| {
                format!(
                    "  {}.{}{} pc={}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(top) = thread.frames.last() {
            s.push_str("\n  -- top-frame locals --");
            let n = top.locals_len().min(10);
            for i in 0..n {
                let raw = top.get_local_raw(i);
                // Widening: small integer index -> usize (non-negative, fits in pointer width)
                let p = (raw & 0x0000_7fff_ffff_ffff) as usize;
                let mut extra = String::new();
                if p != 0 && p % 8 == 0 && shared.mem.heap.is_heap_addr(p).is_some() {
                    // SAFETY: in-heap address; first 16 header bytes of managed
                    // memory are always readable (raw bytes, not enum fields).
                    let (kind_b, elem_b, alen) = unsafe {
                        let q = p as *const u8;
                        (
                            *q.add(4),
                            *q.add(5),
                            // Cast: reinterpret pointer/address to typed pointer
                            (q.add(12) as *const u32).read_unaligned(),
                        )
                    };
                    extra = format!(
                        " [heap obj kind={kind_b} elem={elem_b} len={alen} data=0x{:x}..0x{:x}]",
                        p + 40,
                        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                        p + 40 + (alen as usize) * 8,
                    );
                }
                s.push_str(&format!("\n  local[{i}] = 0x{raw:x}{extra}"));
            }
        }
        s
    });
    if shared.mem.gc_barrier.stw_requested.load(Ordering::Acquire) {
        // CRIT (TLAB UAF) — retire this thread's TLAB before parking for GC,
        // exactly as the GC initiator does in `maybe_gc`. The moving collector
        // run by the initiator can `grow()` (realloc) the young arena while we
        // are parked, freeing the old backing buffer; a TLAB that still held
        // `[cursor,end)` into that buffer would then be dangling, so the first
        // post-GC fast-path bump on this thread writes the object header into
        // freed/unmapped memory → EXCEPTION_ACCESS_VIOLATION in
        // `init_object_header` (deterministic once a grow frees the old buffer;
        // reproduces with JIT off because that path always uses the moving
        // collector). Retiring empties the TLAB so the next allocation refills
        // from the current arena, and installs a walkable filler over
        // `[cursor,end)` so the collector's from-space walk doesn't desync on
        // the unfilled tail (the same reason the initiator retires).
        thread.tlab.retire();
        // Round-5 fix (CRIT — UAF): drain this thread's per-thread SATB
        // buffer into the global queue BEFORE we park at the barrier.
        // The thread-local buffer holds up to 256 overwritten references;
        // without an explicit flush they sit invisible to the marker and
        // the next evacuation cycle turns the SATB lost-object scenario
        // into a use-after-free. Must run on every mutator at every
        // safepoint arrival — `flush_thread_satb` short-circuits cheaply
        // on `is_active() == false` when concurrent marking is idle.
        shared.mem.heap.flush_thread_satb();

        // Update root snapshot before pausing. This snapshot is the ONLY view a
        // cross-thread STW collector has of this (parked) thread's roots, so the
        // JIT-frame portion must be a fresh scan, not the possibly-stale cached
        // one (see `invalidate_scan_cache_for_gc`): the conservative scan covers
        // the native stack below this thread's JIT frames, which mutated since
        // the last boundary bump, and a dropped live root would be reclaimed by
        // the collector this thread is about to park for.
        crate::jit::conservative_roots::invalidate_scan_cache_for_gc();
        update_root_snapshot(shared, thread);
        if remap_trace_on() {
            let snap = thread.root_snapshot.lock().clone();
            deposit_gap_diff(thread, &snap, "publish");
        }

        // Arrive at barrier and wait for GC to complete. Census-aware (auto):
        // a genuine safepoint arrival is normally counted, but if this pause's
        // census excluded us as blocked (a finding-1(a) window), participating
        // would fill a counted mutator's quota slot.
        let pointer_map = shared.mem.gc_barrier.arrive_and_wait_auto(thread.thread_id);

        remap_trace_push(
            shared,
            thread,
            if pointer_map.is_empty() {
                "arrive-nomap"
            } else {
                "arrive"
            },
            &format!("map={}", pointer_map.len()),
        );
        // Apply pointer map to this thread's frames
        if !pointer_map.is_empty() {
            apply_pointer_map_to_thread(thread, &pointer_map, &shared.mem.heap);
        }
        mtroots_selfcheck(thread, &shared.mem.heap, "safepoint-resume");
    }
    // T1.5.1 — pick up any async exception posted by another thread
    // (e.g. `Thread.stop0`). The cross-thread poster writes into the
    // registry's slot; we consume it here and move it into the
    // per-thread `pending_async_exception` field so the next
    // exception-raising point observes it. The *actual* raise
    // happens at the next opcode boundary in `execute_instruction`
    // via `check_pending_async_exception`, which returns the stored
    // throwable as a `MethodCallFailed`.
    if thread.pending_async_exception.is_none() {
        if let Some(throwable) = shared
            .threads
            .thread_registry
            .take_async_exception(thread.thread_id)
        {
            thread.pending_async_exception = Some(throwable);
        }
    }
}

/// T1.5.1 — check the per-thread async-exception slot and, if set,
/// clear it and return a `MethodCallFailed::ExceptionThrown` carrying
/// the Throwable.
///
/// Callers that observe `Some` should immediately propagate the
/// failure through the interpreter's normal exception-table walk so
/// the Throwable lands in the first enclosing `catch` block (or
/// unwinds the method entirely if none applies).
pub fn check_pending_async_exception(
    thread: &mut JvmThread,
) -> Option<crate::error::MethodCallFailed> {
    let throwable = thread.pending_async_exception.take()?;
    Some(crate::error::MethodCallFailed::ExceptionThrown(throwable))
}

/// Apply a GC pointer map to a thread's frame locals and operand stacks.
pub(crate) fn apply_pointer_map_to_thread(
    thread: &mut JvmThread,
    pointer_map: &std::collections::HashMap<usize, usize>,
    heap: &crate::memory::VmHeap,
) {
    // JNI local references (INT-2, safepoint-resume half): rewrite THIS
    // thread's `JNI_LOCAL_FRAMES` handles through the pointer map — a JNI
    // native that re-entered Java and parked at the safepoint poll must not
    // resume with dangling local jobjects after a moving collection. The
    // storage is thread-local and this function always runs on the resuming
    // thread, so this is the only place that can reach these handles.
    crate::native::jni::update_local_refs_after_gc(pointer_map);
    // BUG-03 trace (gated): record that the safepoint-peer remap ran for main.
    if thread.thread_id.0 == 0
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BUG03").is_some()
    {
        let jto = thread
            .java_thread_obj
            .map(|o| o.as_ptr() as usize)
            .unwrap_or(0);
        eprintln!(
            "[BUG03-fm] e{} path=peer(apply_pointer_map) tid0 jto=0x{:x} jto_in_map={} pm.len={}",
            heap.collection_count(),
            jto,
            jto != 0 && pointer_map.contains_key(&jto),
            pointer_map.len()
        );
    }
    for frame in &mut thread.frames {
        frame.update_local_refs(pointer_map, heap);
        frame.stack.update_object_refs(pointer_map, heap);
        // Forward the synchronized-method monitor object too. A `synchronized`
        // method records the object it locked on entry in `monitor_on_exit` and
        // releases it on frame-pop. If a *cross-thread* moving GC relocated that
        // object while this thread was parked at the STW safepoint barrier
        // (`arrive_and_wait` in the safepoint-poll path), a stale
        // `monitor_on_exit` makes the implicit `monitorexit` target the old
        // address — surfacing as "thread does not own the monitor" (observed as
        // an intermittent IllegalMonitorStateException in the ES RestClient
        // `org/elasticsearch/client/Cancellable$RequestCancellable.
        // runIfNotCancelled`, whose `synchronized` body allocates heavily under
        // `-Xmx1g` GC pressure). The GC-initiator (`update_all_roots` in
        // memory/gc.rs) and the native-blocked-thread wake path
        // (`check_post_block_gc` in vm/vm_exec.rs) already forward it; this
        // non-initiator safepoint-resume path was the missing third site. Keep
        // it consistent with the relocated object, identically to those two.
        if let Some(ref mut obj_ref) = frame.monitor_on_exit {
            // Cast: object/code pointer to integer address
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
    // DIAGNOSTIC-ONLY (cceres3): mirror of the wake-time WAKE-STALE verifier;
    // catches a frame slot left stale right after a safepoint-arrival remap.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some() {
        for (fi, fr) in thread.frames.iter().enumerate() {
            for li in 0..fr.locals_len() {
                if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                    let a = o.as_ptr() as usize;
                    if let Some(new) = heap.debug_forwarded_target(a) {
                        eprintln!(
                            "[blockgc] ARRIVE-STALE tid={} frame#{fi} {}.{} pc={} local[{li}] 0x{a:x}->0x{new:x} in_map={}",
                            thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            pointer_map.contains_key(&a),
                        );
                    }
                }
            }
            for si in 0..fr.stack.len() {
                if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                    let a = o.as_ptr() as usize;
                    if let Some(new) = heap.debug_forwarded_target(a) {
                        eprintln!(
                            "[blockgc] ARRIVE-STALE tid={} frame#{fi} {}.{} pc={} stack[{si}] 0x{a:x}->0x{new:x} in_map={}",
                            thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            pointer_map.contains_key(&a),
                        );
                    }
                }
            }
        }
    }
    // Step 5 GAP B (precise-JIT remap, non-initiator half). The GC initiator's
    // `update_all_roots` (memory/gc.rs:73) remaps this collection's precise JIT
    // oop-map slots via `remap_active_jit_frames`, but a thread that was PARKED at
    // the STW barrier reaches HERE instead and was missing that call. Under a
    // moving collector this stranded a non-initiator's JIT-frame oops at their old
    // addresses after a relocation — a use-after-free with CRATONVM_PRECISE_JIT_MAPS
    // on. It is specifically a G1 hazard: G1 young/mixed move unconditionally,
    // whereas the generational collector falls back to a non-moving sweep whenever
    // any thread is in JIT (`gc_quiescence`), so its non-initiator JIT frames never
    // see relocation. Mirror the initiator: remap THIS resuming thread's precise
    // JIT oop slots before the shadow stack. Thread-local (walks this thread's JIT
    // entry chain — sound because we run on the resuming thread itself) and inert
    // unless a precise-map frame is live, so it is a no-op on the default path.
    crate::jit::conservative_roots::remap_active_jit_frames(pointer_map);
    // §4 (multi-thread shadow scan, remap half). Remap THIS thread's shadow-stack
    // precise roots in place, so a worker resuming from the STW barrier sees the
    // relocated addresses in the JIT registers/slots it reloads from its shadow
    // stack. (The GC initiator's own shadow stack is remapped by `update_all_roots`
    // in `gc.rs`; a non-initiator reaches here instead.) Every shadow slot is a
    // known oop, so the rewrite is unconditionally safe. No-op when the gate is
    // off or the shadow stack is empty.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.remap(pointer_map);
    }
    // Also update printed values and java_thread_obj
    for val in &mut thread.printed {
        update_value_ref(val, pointer_map);
    }
    if let Some(ref mut obj_ref) = thread.java_thread_obj {
        let old_addr = obj_ref.as_ptr() as usize; // Cast: GC object pointer to address
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by pointer_map and points at a valid object header within the heap arena.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    // Native-held per-thread roots. A thread running native code that pinned
    // ObjectRefs across allocations (`pin_native_root`, e.g. the synthetic
    // HttpServer dispatcher holding the handler + exchange across exchange-build
    // allocations) can be parked at THIS safepoint barrier when another thread's
    // moving GC relocates those objects. The GC-initiator path
    // (`update_all_roots`) and the native-blocked wake path
    // (`check_post_block_gc`) already forward these; this non-initiator
    // safepoint-resume path must too, or `read_native_pin` hands back a stale
    // address (surfaced as the `java/lang/Object.handle` NoSuchMethodError
    // storm). Forward the same set those two siblings do.
    for obj_ref in &mut thread.native_pin_roots {
        // Cast: object/code pointer to integer address
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    // Cross-thread safepoint-resume counterpart of the handle-slot snapshot
    // above. The collector remaps the snapshot copy, not this owning table.
    crate::memory::gc::remap_handle_slots(&mut thread.handle_slots, pointer_map);
    for obj_ref in &mut thread.native_alloc_pool {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: the pointer map contains only relocated live objects.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    if let Some(ref mut obj_ref) = thread.native_pending_return {
        // Cast: object/code pointer to integer address
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    // The JIT HashMap fast path owns raw map/node ObjectRefs outside frames.
    // A thread parked at this safepoint can miss a moving collection initiated
    // by a peer, so mirror the initiator and blocked-wake remaps before JIT
    // code resumes and probes the cache.
    for entry in &mut thread.jit_hashmap_string_node_cache {
        for obj_ref in [&mut entry.map, &mut entry.node] {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        if let Some(key_object) = entry.key_object.as_mut() {
            let old_addr = key_object.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                *key_object = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
    // TOMCAT-JNDIREALM-JIT.3 — remap companion to the publish added above.
    // Same reasoning as the HashMap node cache: the entries are raw
    // `ObjectRef`s outside any frame, so a peer-initiated moving collection
    // would strand them.
    for entry in &mut thread.string_case_cache {
        for obj_ref in [&mut entry.source, &mut entry.first, &mut entry.second] {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        if let Some(locale) = entry.locale.as_mut() {
            let old_addr = locale.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                *locale = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
    for (_key_id, key_ref, val) in &mut thread.scoped_values {
        if let Some(obj_ref) = key_ref {
            // Cast: object/code pointer to integer address
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        update_value_ref(val, pointer_map);
    }
    if let Some(ref mut obj_ref) = thread.pending_async_exception {
        // Cast: object/code pointer to integer address
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
    // Lever #3 (bug 04): keep this thread's rootsnap frozen-frame cache valid
    // across the relocation we just applied, in lockstep with the frames above.
    remap_rs_cache_after_gc(thread, pointer_map, heap);

    // DIAG (CRATONVM_GC_VERIFY_STALE=1): after a NON-INITIATOR thread resumes
    // from the STW barrier and applies the pointer map, walk its own frames and
    // flag any Object slot whose header is ZEROED (class_id=0 && num_slots=0).
    // A zeroed header means the object was NOT copied by the collector (a missed
    // marking root — its ref was absent from this thread's deposited snapshot,
    // e.g. a lost operand-stack tag), then young-from was reset over it. This is
    // the per-PARKED-THREAD analogue of `verify_no_stale_refs` (which only
    // checks the GC initiator) — it localizes the missed-root that surfaces as
    // the teardown "all-zero header" corruption / reactor-thread leak.
    // Cache the gate so the OFF path (the default) is a single relaxed load, not
    // a per-parked-thread-per-GC environment lookup.
    fn gc_verify_stale_enabled() -> bool {
        use std::sync::OnceLock;
        static E: OnceLock<bool> = OnceLock::new();
        *E.get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_GC_VERIFY_STALE")
                .ok()
                .as_deref()
                == Some("1")
        })
    }
    if gc_verify_stale_enabled() {
        use cratonvm_types::ObjectHeader;
        let tname = thread.thread_id.0;
        for (fi, frame) in thread.frames.iter().enumerate() {
            let cn = frame.class_name();
            let mn = frame.method_name();
            for li in 0..frame.locals_len() {
                // Cast: numeric/representation conversion
                if let crate::types::Value::Object(Some(o)) = frame.get_local(li as u16) {
                    // Cast: object/code pointer to integer address
                    let a = o.as_ptr() as usize;
                    if a != 0 {
                        // SAFETY: `a` is a non-null heap address from a live Object local; reading its ObjectHeader is valid for the lifetime of the borrow.
                        let h = unsafe { &*(a as *const ObjectHeader) };
                        if h.class_id.as_u32() == 0 && h.num_slots() == 0 && h.array_length() == 0 {
                            eprintln!(
                                "POST-GC ZERO-HEADER PARKED tid={} frame[{}] {}.{} local[{}] pc={} addr=0x{:x}",
                                tname, fi, cn, mn, li, frame.pc, a
                            );
                        }
                    }
                }
            }
            let mut stk = Vec::new();
            frame.stack.scan_object_refs(&mut stk, heap);
            for o in stk {
                // Cast: object/code pointer to integer address
                let a = o.as_ptr() as usize;
                if a != 0 {
                    // SAFETY: `a` is a non-null heap address from a live Object stack slot; reading its ObjectHeader is valid for the lifetime of the borrow.
                    let h = unsafe { &*(a as *const ObjectHeader) };
                    if h.class_id.as_u32() == 0 && h.num_slots() == 0 && h.array_length() == 0 {
                        eprintln!(
                            "POST-GC ZERO-HEADER PARKED-STACK tid={} frame[{}] {}.{} pc={} addr=0x{:x}",
                            tname, fi, cn, mn, frame.pc, a
                        );
                    }
                }
            }
        }
    }
}

/// Keep the `update_root_snapshot` frozen-frame cache (`rs_cache`) valid across
/// a GC instead of letting the next snapshot discard it on the
/// `collection_count` bump. The cache stores object ADDRESSES, which a
/// collection only invalidates by RELOCATING the object (the default non-moving
/// young sweep still relocates via selective promotion). So remap the cached
/// roots through the same `pointer_map` that relocated the frames, then tag the
/// cache with the post-collection count so `update_root_snapshot`'s gen gate
/// accepts it.
///
/// FAIL-SAFE: `rs_cache_gen` is advanced ONLY here. Any GC path that relocates
/// this thread's objects WITHOUT calling this leaves `rs_cache_gen` stale, so
/// the gen gate rebuilds the cache from scratch; a stale cached address is
/// never trusted. Default-on with opt-outs (`CRATONVM_ROOTSNAP_CACHE=0` or
/// `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC=0`). Must be called at every site that
/// applies a `pointer_map` to a thread's frames (`apply_pointer_map_to_thread`
/// here, `update_all_roots` in memory/gc.rs); missing one only costs a rebuild,
/// never correctness. (Until 2026-07-31 this also cleared the cache outright in
/// the "real ForkJoinPool lane", keyed off a presence test on
/// `CRATONVM_REAL_FORKJOINPOOL` that stopped being set when that lane became
/// the default — see `env_cache.rs` for why that bypass was retired instead of
/// repointed.)
pub(crate) fn remap_rs_cache_after_gc(
    thread: &mut JvmThread,
    pointer_map: &std::collections::HashMap<usize, usize>,
    heap: &crate::memory::VmHeap,
) {
    if !crate::runtime::env_cache::rootsnap_cache()
        || !crate::runtime::env_cache::rootsnap_cache_survive_gc()
    {
        thread.rs_cache.clear();
        return;
    }
    // An empty map means nothing moved → cached addresses are already valid;
    // skip the walk but still re-tag the gen below so the cache is kept.
    if !pointer_map.is_empty() {
        for (_key, roots) in thread.rs_cache.iter_mut() {
            for r in roots.iter_mut() {
                // Cast: object/code pointer to integer address
                if let Some(&new_addr) = pointer_map.get(&(r.as_ptr() as usize)) {
                    // SAFETY: new_addr came from the GC's pointer_map and points
                    // at the relocated object's header within the heap arena —
                    // the same invariant the frame/snapshot remaps above rely on.
                    *r = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }
    thread.rs_cache_gen = heap.collection_count();
}

/// Check if the old generation needs a concurrent GC cycle.
///
/// If the old gen is above its capacity threshold, this starts a concurrent
/// mark-sweep cycle using brief STW pauses for initial mark and remark,
/// with the marking phase running concurrently with application threads.
fn maybe_concurrent_gc(shared: &SharedVm, thread: &mut JvmThread) {
    // G1 backend: trigger concurrent marking when IHOP threshold crossed
    if shared.mem.heap.is_g1() {
        // Finish first, start second: if an active cycle's background marker
        // has drained to a fixed point, run the STW final remark + cleanup
        // NOW, on this thread (it has the STW-barrier context). The remark
        // is NOT optional — the SATB log and a fresh root scan must reach
        // the bitmap before cleanup acts on it (see
        // `g1_final_remark_and_cleanup`); the old completion path (a watcher
        // thread calling straight into cleanup) discarded both.
        if shared.mem.heap.g1_is_marking_active() && shared.mem.heap.g1_concurrent_mark_finished() {
            g1_final_remark_cleanup(shared, thread);
            return;
        }
        if shared.mem.heap.g1_should_start_marking() && !shared.mem.heap.g1_is_marking_active() {
            g1_concurrent_mark_cycle(shared, thread);
        }
        return;
    }

    // Only proceed if old gen needs collection and we have a concurrent marker
    if !shared.mem.heap.old_gen_needs_gc() {
        return;
    }

    let (old_gen_base, old_gen_size) = shared.mem.heap.old_gen_info();

    // Create a temporary concurrent marker for this cycle.
    //
    // fork6 GC_STRESS fix — build it on the heap's SHARED SATB queue + phase
    // state (attached via `enable_concurrent_gc` at SharedVm construction).
    // The previous `ConcurrentMarker::new` created a private queue + state per
    // cycle while the heap's `satb_barrier` gated on the HEAP's (formerly
    // never-attached) instances: the write barrier was a hard no-op, nothing
    // ever reached this cycle's remark, and the concurrent mark effectively
    // ran against live mutators with no write barrier — the sweep then freed
    // old objects whose only reference moved during the concurrent phase.
    let marker = cratonvm_gc::ConcurrentMarker::with_shared(
        old_gen_base,
        old_gen_size,
        shared.mem.concurrent_satb.clone(),
        shared.mem.concurrent_gc_state.clone(),
    );

    // Phase 1: Initial Mark — brief STW pause.
    //
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) instead of `brief_stw_counted_with_live_blocked`, whose
    // internal plain `wait_for_all()` stalls forever on a peer spinning in
    // compiled code — and whose root set covered such a peer only by its
    // STALE deposit snapshot. Mark-only pause: the frozen peers' fresh
    // conservative roots are extra MARK roots; nothing moves, so no
    // pin/pointer-map concerns.
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let initial_mark_done =
        shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(thread.thread_id, || {
                let (n, blocked, tids, blocked_tids) = shared
                    .threads
                    .thread_registry
                    .alive_count_blocked_and_os_tids();
                counted_os_tids = tids;
                (
                    u32::try_from(n).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
    if !initial_mark_done {
        return; // Another STW was in progress
    }
    {
        // Forcibly stop + conservatively scan in-JIT peers, then wait for
        // the cooperative mutators (byte-identical to wait_for_all() when
        // no thread is in JIT).
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
        // Collect root pointers for old-gen marking
        let roots = collect_roots(shared, thread);
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let mut root_ptrs: Vec<*mut u8> = roots
            .iter()
            .chain(snapshot_roots.iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.iter())
            .map(|r| r.as_ptr())
            .collect();
        // fork6 GC_STRESS fix — young→old references are mandatory
        // old-marking roots. `initial_mark` filters this list with
        // `old_gen.contains`, so an old object whose only path from a
        // root goes THROUGH a young object (root → young holder → old
        // target) was invisible and the sweep freed it live. Selective
        // promotion mass-produces exactly that shape (it tenures a
        // pinned young holder's children), which is why the Fork6Hard
        // GC_STRESS lane corrupted even single-threaded during clinit.
        // Safe here: brief STW, mutators quiesced, TLABs retired.
        root_ptrs.extend(
            shared
                .mem
                .heap
                .collect_young_to_old_roots()
                .into_iter()
                .map(|a| a as *mut u8),
        );
        if let Some(guard) = shared.mem.heap.old_gen_lock() {
            marker.initial_mark(&root_ptrs, &*guard);
        }
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        shared.mem.heap.clear_jit_tlab_skip_regions();
        crate::jit::xt_root_scan::resume(taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(std::collections::HashMap::new());
    }

    // Phase 2: Concurrent Mark — runs while app threads continue
    if let Some(guard) = shared.mem.heap.old_gen_lock() {
        marker.concurrent_mark(&*guard);
    }

    // Phase 3: Remark — brief STW pause.
    // INT-3 residual fix: open-coded for the same takeover-wait reason as
    // Phase 1 above (a never-polling in-JIT peer must not stall the remark
    // nor be covered only by its stale deposit snapshot).
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let remark_done =
        shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(thread.thread_id, || {
                // Finding 1(a): remark pauses use the identity census too, so blocked
                // threads are excluded BY IDENTITY and their wake-time arrivals cannot
                // satisfy this pause's quota (`arrive_and_wait_auto`). The anonymous
                // `threads_blocked` subtraction this replaces excluded the same
                // population without recording who it excluded.
                let (n, blocked, tids, blocked_tids) = shared
                    .threads
                    .thread_registry
                    .alive_count_blocked_and_os_tids();
                counted_os_tids = tids;
                (
                    u32::try_from(n).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
    if remark_done {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
        // Round-5 fix (CRIT — UAF): drain the initiator's per-thread
        // SATB buffer before remark drains the global queue. Other
        // mutators flushed when they arrived at the STW barrier;
        // the initiator must drain its own.
        shared.mem.heap.flush_thread_satb();
        let roots = collect_roots(shared, thread);
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let mut root_ptrs: Vec<*mut u8> = roots
            .iter()
            .chain(snapshot_roots.iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.iter())
            .map(|r| r.as_ptr())
            .collect();
        // fork6 GC_STRESS fix — refresh the young→old roots at remark
        // too: a young→old edge created during the concurrent phase
        // (e.g. a promoted child stored into a fresh young holder) must
        // be in the final bitmap before the sweep.
        root_ptrs.extend(
            shared
                .mem
                .heap
                .collect_young_to_old_roots()
                .into_iter()
                .map(|a| a as *mut u8),
        );
        if let Some(guard) = shared.mem.heap.old_gen_lock() {
            marker.remark(&root_ptrs, &*guard);
        }
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        shared.mem.heap.clear_jit_tlab_skip_regions();
        crate::jit::xt_root_scan::resume(taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(std::collections::HashMap::new());
    }

    // fork6 GC_STRESS fix — the remark STW is NOT optional. If another
    // thread's STW won the race (`brief_stw_counted` returned false — a
    // near-certainty under allocation storms, where a young-GC request is
    // always pending), the closure never ran: the SATB queue is undrained
    // and the roots were never rescanned, so the mark bitmap is NOT final.
    // The old code fell through to the sweep anyway and freed live objects.
    // Abort the cycle instead (deactivate the barrier, discard the bitmap);
    // the next `old_gen_needs_gc` trigger starts over.
    if !remark_done {
        marker.abort_cycle();
        return;
    }

    // Phase 4: Concurrent Sweep
    if let Some(mut guard) = shared.mem.heap.old_gen_lock() {
        let swept = marker.concurrent_sweep(&mut *guard);
        if swept > 0 {
            tracing::debug!("Concurrent GC: swept {} old-gen objects", swept,);
        }
    }
    // Cycle complete — phase back to Idle (the write barrier's
    // `is_marking_active()` gate is already false after remark, but leaving
    // the shared state at `ConcurrentSweep` would misreport the VM as
    // mid-cycle to any observer).
    marker.finish_cycle();
}

// ---------------------------------------------------------------------------
// G1 concurrent marking cycle
// ---------------------------------------------------------------------------

/// Execute a full G1 concurrent marking cycle:
/// 1. Initial Mark (brief STW) — mark roots, activate SATB
/// 2. Concurrent Mark (background thread) — traverse heap regions
/// 3. Remark (brief STW) — drain SATB buffers, re-mark roots
/// 4. Cleanup — compute per-region liveness, free empty regions
fn g1_concurrent_mark_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    // Phase 1: Initial Mark — brief STW pause.
    // Activates SATB write barrier and marks root-reachable objects.
    //
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) instead of `brief_stw_counted_with_live_blocked`, whose
    // internal plain `wait_for_all()` stalls forever on a peer spinning in
    // compiled code — and whose root set covered such a peer only by its
    // STALE deposit snapshot (a missed mark root here = cleanup frees a
    // live object). Mark-only pause: the frozen peers' fresh conservative
    // roots are extra MARK roots; nothing moves, so no pin/pointer-map
    // concerns.
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let initial_mark_done =
        shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(thread.thread_id, || {
                let (n, blocked, tids, blocked_tids) = shared
                    .threads
                    .thread_registry
                    .alive_count_blocked_and_os_tids();
                counted_os_tids = tids;
                (
                    u32::try_from(n).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
    if !initial_mark_done {
        return; // Another STW in progress
    }
    {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
        // Round-5 fix (CRIT — UAF): drain the initiator's per-thread
        // SATB buffer on the way into initial-mark. Other mutators
        // already flushed on their `safepoint_check` arrival; the
        // initiator hasn't, and any buffered overwrites from before
        // SATB activation must reach the global queue before the
        // marker starts consuming it.
        shared.mem.heap.flush_thread_satb();
        shared.mem.heap.g1_start_concurrent_mark();
        // INT-8: publish the referent-slot skip set for this cycle —
        // the Weak/Soft/Phantom Reference OBJECT addresses currently
        // registered. Inside this STW the snapshot is consistent (no
        // mutator can construct, move, or free a Reference), and it
        // must land before the roots below seed the gray set so no
        // Reference is ever scanned without the skip in force. G1
        // carries the set across every mid-cycle evacuation pause
        // internally (remap survivors, prune CSet casualties).
        let ref_objs = shared.mem.ref_processor.lock().reference_object_addresses();
        shared.mem.heap.g1_set_reference_skip_set(&ref_objs);
        // Mark roots into the G1 mark bitmap
        let roots =
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread));
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let all_roots: Vec<cratonvm_types::ObjectRef> = roots
            .into_iter()
            .chain(snapshot_roots.into_iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.into_iter())
            .collect();
        shared.mem.heap.g1_mark_roots(&all_roots);
        tracing::debug!("[G1] Initial mark: {} roots marked", all_roots.len());
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        shared.mem.heap.clear_jit_tlab_skip_regions();
        crate::jit::xt_root_scan::resume(taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(std::collections::HashMap::new());
    }

    // Phase 2: Concurrent Mark — the background worker that drains
    // the mark worklist is owned by the VmHeap-level
    // `ConcurrentMarkController` (task #56). `g1_start_concurrent_mark`
    // above spawned it during the initial-mark STW, so by the time we
    // get here the marker is already running concurrently with mutators.
    //
    // Phases 3+4 (STW final remark + cleanup) are driven by
    // `maybe_concurrent_gc`: after each subsequent young collection the
    // GC-initiating mutator checks `g1_concurrent_mark_finished()` and,
    // once the worker has drained to a fixed point, runs
    // `g1_final_remark_cleanup` under its own brief STW. The previous
    // design — a detached watcher thread polling quiescence and calling
    // `g1_signal_marking_complete` (i.e. cleanup) directly — never ran a
    // final remark at all: the SATB log was discarded wholesale and the
    // roots were never re-scanned, so the sweep verdicts raced every
    // reference the mutators rewrote during the concurrent phase. A
    // watcher thread also cannot run the remark itself: it has no
    // JvmThread/barrier context to initiate an STW.
    //
    // Deferral note: if allocation stops entirely after IHOP fired, no
    // young GC follows and the cycle stays open (SATB active, cleanup
    // pending). That is benign — a heap nobody allocates into needs no
    // reclamation — and the next allocation-triggered GC closes it.
}

/// Phases 3+4 of the G1 cycle: STW final remark, then cleanup.
///
/// Called by `maybe_concurrent_gc` on the GC-initiating mutator once the
/// background marker has quiesced. Collects the full root set (all
/// threads) under a brief STW and hands it to
/// [`VmHeap::g1_final_remark_and_cleanup`], which re-marks roots, drains
/// the SATB log, completes the transitive closure, and runs cleanup —
/// all while the world is stopped.
///
/// fork6-pattern race handling: if another thread's STW wins
/// (`brief_stw_counted` returns false), nothing ran — the cycle simply
/// stays open (SATB active, worklist quiescent) and the next
/// `maybe_concurrent_gc` retries. Unlike the generational remark there is
/// nothing to abort: no sweep decision has been made yet.
fn g1_final_remark_cleanup(shared: &SharedVm, thread: &mut JvmThread) {
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) so a never-polling in-JIT peer neither stalls the pause
    // forever nor is covered only by its stale deposit snapshot (a missed
    // mark root here = cleanup frees a live object). Unlike the young/mixed
    // pauses nothing moves, so no pins — but `cleanup` DOES linearly walk
    // every non-Free region computing live bytes, so the takeover's
    // frozen-TLAB-tail publication (consumed by the region walkers' skip
    // checks) is load-bearing here too.
    let mut counted_os_tids: Vec<u32> = Vec::new();
    let done =
        shared
            .mem
            .gc_barrier
            .request_stw_counted_with_live_blocked(thread.thread_id, || {
                // Finding 1(a): remark pauses use the identity census too, so blocked
                // threads are excluded BY IDENTITY and their wake-time arrivals cannot
                // satisfy this pause's quota (`arrive_and_wait_auto`). The anonymous
                // `threads_blocked` subtraction this replaces excluded the same
                // population without recording who it excluded.
                let (n, blocked, tids, blocked_tids) = shared
                    .threads
                    .thread_registry
                    .alive_count_blocked_and_os_tids();
                counted_os_tids = tids;
                (
                    u32::try_from(n).unwrap_or(u32::MAX),
                    u32::try_from(blocked).unwrap_or(u32::MAX),
                    blocked_tids,
                )
            });
    if done {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        let taken = stw_take_over_and_wait(shared, &mut xt_roots, &counted_os_tids);
        // Drain the initiator's per-thread SATB buffer; the other
        // mutators' buffers are pulled by `remark` itself
        // (`flush_all_thread_satb_buffers`) now that they are parked.
        shared.mem.heap.flush_thread_satb();
        let roots =
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| collect_roots(shared, thread));
        let snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        let all_roots: Vec<cratonvm_types::ObjectRef> = roots
            .into_iter()
            .chain(snapshot_roots.into_iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.into_iter())
            .collect();
        // INT-8: run VM reference processing against the completed mark
        // bitmap between the remark drain and cleanup — the only point
        // in the cycle where a weak/soft ref to a dead OLD-region
        // referent can be observed dead (evacuation pauses only ever
        // see CSet deaths). The callback returns the addresses the
        // heap must resurrect before cleanup's in-place frees.
        let mut process = |is_live: &dyn Fn(usize) -> bool| -> Vec<usize> {
            g1_remark_process_references(shared, is_live)
        };
        let completed = shared
            .mem
            .heap
            .g1_final_remark_and_cleanup(&all_roots, Some(&mut process));
        tracing::debug!(
            "[G1] Final remark: {} roots, cycle_completed={}",
            all_roots.len(),
            completed
        );
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        shared.mem.heap.clear_jit_tlab_skip_regions();
        crate::jit::xt_root_scan::resume(taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(std::collections::HashMap::new());
    } else {
        tracing::debug!("[G1] Final remark lost the STW race — retrying at next GC");
    }
}

/// INT-8 — remark-time reference processing (G1 only). Runs INSIDE the
/// final-remark STW, after the gray set drained to a fixed point and BEFORE
/// `cleanup()` frees anything, with `is_marked` = the collector's
/// bitmap+TAMS verdict. This is the HotSpot-shaped point where weak/soft
/// references to dead OLD-region referents finally clear: with referent-slot
/// hiding, the bitmap holds an untainted verdict for every referent, and the
/// young-pause processing path (whose `is_marked` treats every non-CSet
/// region as live) can never see these deaths.
///
/// Mirrors `process_references_after_gc`'s consumer protocol with two
/// deliberate differences:
/// - no pointer map (nothing moved in this pause) — the staleness guard is
///   dead-BY-MARK instead: a Reference/queue that is itself unmarked is
///   skipped (writing through it would be resurrection-by-side-effect right
///   before its region is freed);
/// - referent clears use the SATB-suppressed store: the clear is the
///   processor's decided verdict, and SATB-logging the old referent would
///   feed it straight back into the resurrection drain that follows.
///
/// Returns every address that must stay live through this cycle's cleanup:
/// dead finalizables about to run `finalize()` (this closes the
/// finalize-never-runs gap for in-place-freed regions), submitted cleaner
/// actions, pending cleaner chains, and policy-retained soft referents.
fn g1_remark_process_references(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
) -> Vec<usize> {
    // G1's marker follows loader/mirror/metadata side edges during a full mark.
    // Reconcile those weak ownership tables against the completed bitmap before
    // cleanup frees dead regions and before the optional reference-processor
    // short-circuit.
    crate::memory::gc::reconcile_class_mirrors(shared, is_marked);
    let no_moves = std::collections::HashMap::new();
    let dead_class_hints =
        cratonvm_native_builtins::classloader::gc_reconcile_defining_loaders(is_marked, &no_moves);
    let unloaded = crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints);
    if unloaded.classes_unloaded != 0 {
        tracing::debug!(
            loaders = unloaded.loaders_unloaded,
            classes = unloaded.classes_unloaded,
            jit_entries = unloaded.jit_entries_retired,
            "G1 class-loader metadata unloaded at final remark"
        );
    }

    // Same subsystem-level exclusion switch as the post-GC path.
    if no_refproc() {
        return Vec::new();
    }
    // Same pressure input as the post-GC path — see the long comment there for
    // why this is allocatable headroom rather than the former hardcoded `64`,
    // and why the `0` clock argument is correct rather than a second hardcode.
    let free_mb = shared.mem.heap.soft_ref_policy_free_mb();
    // See `process_references_after_gc`: ClassManager must be consulted
    // before taking the lower-ranked reference-processor lock.
    let reference_next_slot = gc_reference_next_slot(shared);
    let mut ref_proc = shared.mem.ref_processor.lock();
    let result = ref_proc.process_references(is_marked, free_mb, 0);

    // Null the referent slot of newly-cleared references (once-only per
    // entry, same contract as the post-GC path).
    let cleared = ref_proc.take_newly_cleared();
    for ref_addr in cleared {
        if !is_marked(ref_addr) {
            // The Reference itself is dead this cycle — no mutator can ever
            // observe its slot again and cleanup may free it momentarily.
            continue;
        }
        // SAFETY: `ref_addr` is a registry address kept current by the
        // per-pause `update_after_gc`; nothing has been freed since.
        let obj_ref = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
        if shared.mem.heap.num_fields(obj_ref) < 2 {
            continue; // belt-and-suspenders, mirrors the post-GC path
        }
        // SATB-suppressed: the clear is a decided verdict, not a semantic
        // overwrite — logging the old referent would resurrect it in the
        // re-drain below and retain the memory a full extra cycle.
        shared
            .mem
            .heap
            .set_field_suppress_satb(obj_ref, 0, Value::Object(None));
    }

    // Queue links for cleared/phantom references. Skip the whole enqueue
    // when the Reference or its queue is dead-by-mark: linking a dead
    // Reference into a live queue would resurrect it into a region cleanup
    // is about to free (dangling queue head), and a dead queue has no
    // consumer to poll it.
    for (ref_addr, queue_addr) in &result.to_enqueue {
        if !is_marked(*ref_addr) || !is_marked(*queue_addr) {
            continue;
        }
        // SAFETY: registry addresses, current as above; both marked live.
        let ref_obj = unsafe { ObjectRef::from_raw(*ref_addr as *mut u8) };
        let q_obj = unsafe { ObjectRef::from_raw(*queue_addr as *mut u8) };
        if shared.mem.heap.num_fields(q_obj) < 2 || shared.mem.heap.num_fields(ref_obj) < 2 {
            continue;
        }
        // Same linked-list protocol as the post-GC path: head/size on the
        // queue, linkage through the Reference's `next` slot (slot 2 on the
        // real-JDK layout; legacy 2-field shape falls back to slot 0).
        let old_head = shared.mem.heap.get_field(q_obj, 0);
        shared
            .mem
            .heap
            .set_field(q_obj, 0, Value::Object(Some(ref_obj)));
        let next_slot = if shared.mem.heap.num_fields(ref_obj) <= 2 {
            0 // legacy synthetic 2-field shape: referent, queue only
        } else {
            reference_next_slot
        };
        shared.mem.heap.set_field(ref_obj, next_slot, old_head);
        let size = match shared.mem.heap.get_field(q_obj, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        shared.mem.heap.set_field(q_obj, 1, Value::Int(size + 1));
        shared.mem.heap.set_field(ref_obj, 1, Value::Int(1)); // enqueued sentinel
    }

    // Everything handed out below must survive this cycle's cleanup — the
    // caller marks these and re-drains the closure before any region is
    // freed.
    let mut resurrect: Vec<usize> = Vec::new();

    // Dead finalizables: submit for finalize() AND resurrect. This is the
    // half of INT-8 that closes the finalize-never-runs gap: previously a
    // finalizable object in a wholly-dead Old region was freed in place by
    // cleanup and the post-GC staleness guard then (correctly) dropped its
    // stale submission — finalize() silently never ran.
    for obj_addr in &result.to_finalize {
        shared.mem.finalizer_thread.enqueue(*obj_addr);
        resurrect.push(*obj_addr);
    }
    while let Some(obj_addr) = ref_proc.dequeue_for_finalization() {
        shared.mem.finalizer_thread.enqueue(obj_addr);
        resurrect.push(obj_addr);
    }

    // Cleaner actions fired by this round: submit + resurrect (the action
    // object is dereferenced later by run_cleaner_actions).
    for action_addr in &result.cleaner_actions {
        shared.mem.cleaner_thread.submit_action(*action_addr);
        resurrect.push(*action_addr);
    }

    // Pending (not-yet-fired) cleaner chains: the registry will hand these
    // out on a later cycle, so cleanup must not free them — the HotSpot
    // equivalent is the Cleaner's internal strong list. Self-referent
    // (finalizer-style) registrations are excluded inside the accessor.
    resurrect.extend(ref_proc.cleaner_pending_object_addresses());

    // Policy-retained soft referents: the marker never traced them
    // (referent-slot hiding), so a softly-only-reachable referent is
    // unmarked even though the LRU policy kept it — exactly like HotSpot,
    // reference processing itself keeps them alive.
    resurrect.extend(ref_proc.soft_survivor_referents());

    // DBG (CRATONVM_DBG_REFPROC_REMARK): per-remark mechanism evidence —
    // distinguishes clears that happened HERE (against the mark bitmap)
    // from clears the evacuation-pause path produced, which black-box
    // probes cannot tell apart.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_REFPROC_REMARK").is_some() {
        eprintln!(
            "[refproc-remark] soft_cleared={} weak_cleared={} enqueued={} finalize={} \
             cleaner_actions={} resurrect={}",
            result.stats.soft_refs_cleared,
            result.stats.weak_refs_cleared,
            result.to_enqueue.len(),
            result.to_finalize.len(),
            result.cleaner_actions.len(),
            resurrect.len(),
        );
    }

    resurrect
}

/// Last-ditch G1 full marking cycle before declaring OutOfMemoryError.
///
/// Young/mixed pauses reclaim only collection-set regions; dead Old regions
/// and dead humongous spans are reclaimed exclusively by a completed mark
/// cycle's cleanup phase (`reclaim_dead_humongous_spans_locked`). When an
/// allocation still fails after the forced young GC, the heap may simply be
/// full of *unmarked dead* Old/humongous data — run one complete cycle
/// synchronously (start → drain → final remark → cleanup) and let the caller
/// retry the allocation once more before throwing OOM. Mirrors HotSpot's
/// last-ditch full GC on allocation failure.
///
/// No-op on non-G1 backends. Bounded: gives up after ~2s if the background
/// marker never quiesces or the STW races never resolve — the caller then
/// proceeds to OOM; this can delay an inevitable OOM slightly but never
/// hangs the allocation path.
pub(crate) fn g1_force_full_cycle(shared: &SharedVm, thread: &mut JvmThread) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    // If a cycle is already mid-flight we simply help finish it — its
    // cleanup reclaims the same dead spans a fresh cycle would.
    let mut saw_active = shared.mem.heap.g1_is_marking_active();
    loop {
        if shared.mem.heap.g1_is_marking_active() {
            saw_active = true;
            if shared.mem.heap.g1_concurrent_mark_finished() {
                // Runs remark+cleanup under a brief STW; on a lost STW race
                // the cycle stays open and the loop retries.
                g1_final_remark_cleanup(shared, thread);
            } else {
                std::thread::yield_now();
            }
        } else if saw_active {
            return; // cycle completed — cleanup has run
        } else {
            // Not started yet (or the initial-mark STW lost its race —
            // g1_concurrent_mark_cycle returns without activating in that
            // case). Start/retry it.
            g1_concurrent_mark_cycle(shared, thread);
        }
        if std::time::Instant::now() >= deadline {
            tracing::debug!("[G1] last-ditch full cycle timed out — proceeding to OOM");
            return;
        }
    }
}

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
) -> Result<
    Option<cratonvm_native_api::NativeCallback>,
    cratonvm_types::error::JdkOnlyViolation,
> {
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
        crate::vm::dispatch_policy(shared),
        class_name,
        method_name,
        method_descriptor,
        Some((callback, kind)),
        // JDK-ONLY-WAVE2: hard-coded `true` reproduces the pre-§7 "a registered
        // native unconditionally wins here" of the `find` calls this replaces.
        // Wave 2 replaces it with the real per-site compatibility verdict once
        // `force_native_over_real_jdk_bytecode` and the forced-native `String`
        // list (both in `vm/src/runtime/interpreter/invoke.rs` /
        // `vm/src/vm/vm_exec.rs`) are unified.
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
            // JDK-ONLY-WAVE2: the three `redefine_immune_*` predicates are
            // hard-coded class/method-name exception lists (defined in
            // `vm/src/runtime/interpreter/invoke.rs`, not owned here). They
            // encode "this native keeps winning even over instrumented
            // bytecode", which is a §1.4 shadow decision taken outside
            // `resolve_dispatch`. What should replace them: `NativeKind` —
            // exactly `Intrinsic` should be redefine-immune, and everything
            // else should yield to redefined bytecode, with no name list at
            // all. NOT deleted this wave; the lists gate real Mockito/ByteBuddy
            // behaviour. Call site marked so wave 2 finds it mechanically.
            let class_redefined = crate::classloading::any_class_redefined()
                && shared
                    .classes
                    .class_manager
                    .read()
                    .class_redefine_generation(class_id)
                    > 0
                && !redefine_immune_reflection_native(&class_name_owned, method_name)
                && !redefine_immune_string_builder_native(
                    &class_name_owned,
                    method_name,
                    method_descriptor,
                )
                && !redefine_immune_path_native(&class_name_owned, method_name, method_descriptor);
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
                        let canonical: &'static str = match &*class_name_owned {
                            "java/util/Set" | "java/util/Collection" => "java/util/HashSet",
                            // Iterable has no collection shape of its own. Keep
                            // synthetic List-style receivers on the established
                            // ArrayList bridge after lambda proxies have already
                            // had a chance to dispatch their SAM implementation.
                            "java/lang/Iterable" => "java/util/ArrayList",
                            "java/util/List" => "java/util/ArrayList",
                            "java/util/Map" => "java/util/HashMap",
                            "java/util/Iterator" => "java/util/HashMap$KeyItr",
                            _ => "",
                        };
                        if !canonical.is_empty() {
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
                        format!("recv_cid={rc} recv_class={rn}")
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
        let (is_interface_default, static_skip_reason, fjp_skip, native_skip) = if already_skipped {
            (false, None, false, false)
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
            // RFJP.1 — see is_fjp_subclass_blocklisted: methods on classes that
            // transitively extend `java/util/concurrent/ForkJoinTask` miscompile
            // under deep recursion and must run in the interpreter pending a
            // proper regalloc fix.
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
            let native_skip = if shared
                .natives
                .native_methods
                .find_with_kind(&class_name_str, method_name, method_descriptor)
                .is_some()
            {
                true
            } else {
                jit_method_calls_native_shadowed(
                    shared,
                    class_id,
                    &code_attr.code,
                    code_attr.code.len(),
                )
            };
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
        let env_disable_jit = crate::runtime::env_cache::disable_jit()
            || matches!(thread.kind, crate::threading::ThreadKind::Virtual);
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
        if env_disable_jit
            || redefine_jit_quiesced
            || already_skipped
            || static_skip_reason.is_some()
            || fjp_skip
            || native_skip
            || gpu_gate_skip
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
            if static_skip_reason.is_some() || fjp_skip || native_skip {
                note_jit_skip_seal("static-policy-or-native-shadow", &skip_key);
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
                            let class_name_ref = class.constant_pool.get_class_name(cp_idx)?;
                            let leaf = class_name_ref.trim_start_matches('[');
                            let leaf_et = match leaf.as_bytes().first() {
                                Some(b'I') => 10u8,
                                Some(b'J') => 11,
                                Some(b'F') => 6,
                                Some(b'D') => 7,
                                Some(b'B') => 8,
                                Some(b'C') => 5,
                                Some(b'S') => 9,
                                Some(b'Z') => 4,
                                _ => 0,
                            };
                            mna_info.push((pc, leaf_et));
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
                            let boxed: Box<str> = class_name.to_string().into_boxed_str();
                            let ptr = boxed.as_ptr();
                            let len = boxed.len();
                            owned_jit_strings.push(boxed);
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
                            let invoke_kind = match opcode {
                                0xb6 => 0u8, // invokevirtual
                                0xb7 => 1,   // invokespecial
                                0xb9 => 2,   // invokeinterface
                                0xb8 => 3,   // invokestatic
                                _ => continue,
                            };
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
                    for (pc, tclass, pcount) in pending_ctor_sites {
                        let elidable = shared
                            .load_class_concurrent(&tclass)
                            .ok()
                            .map(|tid| {
                                let cm2 = shared.classes.class_manager.read();
                                is_elidable_construction(shared, &cm2, tid)
                            })
                            .unwrap_or(false);
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
                        .map(|c| !c.is_synthetic_stub)
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
                                        let num_fields = shared
                                            .classes.class_manager
                                            .read()
                                            .get_class(target_id)
                                            .map(|c| c.num_total_fields)
                                            .unwrap_or(0);
                                        new_info.push((
                                            pc_new,
                                            target_id.as_u32(),
                                            num_fields,
                                            true,
                                            true,
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
                                        let concat_site = crate::runtime::invokedynamic::make_jit_string_concat_site_from_parts(
                                            &class.constant_pool,
                                            &class.bootstrap_methods,
                                            cp_idx,
                                        )
                                        .unwrap_or(0);
                                        indy_info.push((
                                            pc_indy,
                                            arg_slots,
                                            ret_type,
                                            arg_type_tags,
                                            concat_site,
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
                    let mut ldc_string_info_early: Vec<(usize, *const u8, usize)> = Vec::new();
                    let mut ldc_class_info_early: Vec<(usize, u32, u16)> = Vec::new();
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
                                    }
                                    Some(ConstantPoolEntry::StringReference { string_index })
                                        if class
                                            .constant_pool
                                            .get_utf8_wide(*string_index)
                                            .is_none() =>
                                    {
                                        // Wired 2026-07-18, mirroring the OSR-artifact
                                        // path: boxed text retained via
                                        // `owned_jit_strings` -> `cm._jit_strings`;
                                        // codegen materializes through
                                        // `helpers.ldc_string`. Before this, ANY
                                        // method with a string constant went into
                                        // `jit_skip_set` here, which also blocked the
                                        // hot-path `jit::try_compile` - OSR artifacts
                                        // were such methods' ONLY compiled form.
                                        match class.constant_pool.get_utf8(*string_index) {
                                            Some(s) => {
                                                let boxed: Box<str> =
                                                    s.to_string().into_boxed_str();
                                                let ptr = boxed.as_ptr();
                                                let len = boxed.len();
                                                owned_jit_strings.push(boxed);
                                                ldc_string_info_early.push((pc_ldc, ptr, len));
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
                                    Some(ConstantPoolEntry::Double(v)) => v.to_bits() as i64, // Cast: JIT ABI -- float bits to i64
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
                    let mut cm = crate::jit::x64::compile_with_param_slots(
                        &padded,
                        code_len,
                        param_slots,
                        code_attr.max_locals as usize, // Widening: u16 to usize
                        scan.needs_heap,
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
                        std::collections::HashMap::new(), // branch_hints
                        std::collections::HashMap::new(), // loop_unroll_hints
                        &helpers,
                        scan.non_escaping_new.clone(), // escape analysis results
                        std::collections::HashMap::new(), // inline_sites
                        std::collections::HashMap::new(), // inline_guard_class_ids (PGO-02, no guarded plan from this scan-based fast path)
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
                            if let Some(exc) = crate::jit::helpers::take_jit_pending_exception(thread) {
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
                                    if let Some(result) = run_jit_callee_handler(
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
                                    match crate::runtime::exceptions::throw_runtime_error(
                                        shared,
                                        thread,
                                        RuntimeError::NullPointerException { message: None },
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
                                    let _ =
                                        crate::jit::helpers::DeoptimizationController::deoptimize(
                                            shared,
                                            &class_name_str,
                                            method_name,
                                            method_descriptor,
                                            deopt_reason,
                                            rframe_for_despec.bci,
                                        );
                                    // This first-call tier-up sink historically
                                    // discarded the reconstructed frame and
                                    // restarted the method at bci 0. That
                                    // duplicates every side effect committed
                                    // before the guard. Build the same cached
                                    // metadata the hot callsites use and resume
                                    // the captured frame directly.
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
                                    // Precise reconstruction is a correctness
                                    // requirement once native code has executed
                                    // past bci 0. Refuse a whole-method replay:
                                    // it is observably wrong for methods with
                                    // stores, I/O, monitor actions, or callbacks.
                                    let why = if !resume_gate_ok {
                                        "can_deopt_resume=false (no deopt points, \
                                         or an elided monitor)"
                                    } else if !key_matches {
                                        "the stashed frame belongs to a different method"
                                    } else if materialize_failed {
                                        "the frame could not be materialised from its map"
                                    } else {
                                        "unknown"
                                    };
                                    return Err(MethodCallFailed::InternalError(
                                        VmError::Internal {
                                            message: format!(
                                                "precise deoptimization unavailable for \
                                                 {}.{}{} at bci {} ({}, stashed key {:?}, \
                                                 inline callers {}, reason {:?}); \
                                                 refusing side-effecting replay",
                                                class_name_str,
                                                method_name,
                                                method_descriptor,
                                                rframe_for_despec.bci,
                                                why,
                                                rframe_for_despec.method_key,
                                                rframe_for_despec.caller_frames.len(),
                                                deopt_reason,
                                            ),
                                        },
                                    ));
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
        match find_exception_handler_pc_unknown(shared, &thread.frames[frame_idx], exc) {
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
    if let Some(f) = thread.frames.pop() {
        // Root-snapshot cache correctness: the frame that becomes the top again
        // (the caller this return/unwind exposes) is about to RE-EXECUTE and may
        // reassign its locals. Bump its `exec_epoch` so the `(seq, exec_epoch)`
        // cache key invalidates its stale cached roots — `seq` alone is unchanged
        // (the caller was never popped) and would otherwise reuse roots that miss
        // a freshly-allocated local (the AME all-zero-receiver corruption). See
        // the `Frame::seq` / `exec_epoch` docs. Cheap: one add on the (cold)
        // return/unwind path. `wrapping_add` so a (practically impossible) u64
        // overflow can never alias a live cache key into a false match.
        if let Some(caller) = thread.frames.last_mut() {
            caller.exec_epoch = caller.exec_epoch.wrapping_add(1);
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
            if let Err(e) = shared.threads.monitors.exit(obj, thread.thread_id) {
                tracing::warn!(
                    class = %f.class_name(),
                    method = %f.method_name(),
                    descriptor = %f.method_descriptor(),
                    thread_id = ?thread.thread_id,
                    error = ?e,
                    "implicit monitorexit on synchronized-method-frame-pop failed"
                );
            }
            if !shared.threads.monitors.holds(obj, thread.thread_id) {
                shared
                    .threads
                    .thread_registry
                    .remove_jmx_locked_monitor(thread.thread_id, obj);
            }
        }
        thread.recycle_frame_with_shared(f, &shared.mem.operand_stack_pool, &shared.mem.tag_pool);
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
    /// cannot catch (an OSR'd method provably declares no exception table —
    /// see RBC.6b in `compile_osr_artifact`). The caller must hand the
    /// throwable to the dispatch loop's `pending_java_exception` channel so it
    /// unwinds from THIS frame, instead of resuming the loop.
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
///         pending_java_exception = Some((exc, entry_pc));
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
    let osr_result = try_osr(
        shared,
        thread,
        *frame_idx,
        osr_class_id,
        entry_pc,
        &mut osr_throw,
    );
    // Checked BEFORE the rejection bookkeeping below: the OSR'd body RAN (and
    // committed loop iterations), so this is not a rejected attempt and must
    // not consume the per-pc rejection budget.
    if let Some(exc) = osr_throw {
        return OsrBackoffOutcome::ThrowJava(exc);
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
    let mut quick: Option<Arc<cratonvm_reader::QuickenedCode>> = None;
    let mut quick_code_ptr: *const u8 = std::ptr::null();
    // (The former `quick_hint` local is gone: `QuickenedCode` now resolves any
    // pc in O(1) via an instruction-start bitmap plus per-block popcount, so
    // the fall-through hint only bought one popcount on the fall-through path
    // at the cost of a compare on every branch, back-edge, handler entry and
    // switch target. `resolve()` is the hint-free form.)
    loop {
        // Route callee-thrown Java exceptions before the safepoint poll below.
        // `pending_java_exception` is only a Rust local between the callee's
        // return and this block; it is not present in any GC-scanned frame slot
        // yet. Polling first can let STW reclaim the Throwable before a caller
        // catch handler stores it.
        if let Some((exc, invoke_pc)) = pending_java_exception.take() {
            let mut exc_pc = invoke_pc;
            // GC-root gap: this loop can pop MANY frames while searching for a
            // handler (unwinding all the way out of the method if none is
            // found), and `find_exception_handler` -> `find_exception_handler_impl`
            // lazily loads an unresolved catch-type class on a cache miss
            // (`load_class_concurrent`, which runs <clinit> and can allocate/
            // trigger a GC). Once a frame is popped it no longer roots the
            // propagating exception, and nothing else does until a handler is
            // found (pushed onto a frame's stack) or the method returns it as
            // an Err - pin it in `native_pin_roots` for the whole walk so a GC
            // mid-unwind can't reclaim it.
            let pin_base = thread.native_pin_roots.len();
            thread.native_pin_roots.push(exc);
            loop {
                let current_exc = thread.native_pin_roots[pin_base];
                match find_exception_handler(shared, &thread.frames[frame_idx], exc_pc, current_exc)
                {
                    Some((handler_pc, exc_ref)) => {
                        thread.frames[frame_idx].stack.clear();
                        thread.frames[frame_idx]
                            .stack
                            .push(Value::Object(Some(exc_ref)))
                            .map_err(|e| MethodCallFailed::InternalError(VmError::Runtime(e)))?;
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
        if !stack_dump_emitted && shared.stack_dump_pending() {
            shared.dump_current_thread_frames(thread);
            stack_dump_emitted = true;
            // Don't park or sleep here — the watchdog aborts the process
            // after a short grace period, and if it doesn't (e.g. crashed
            // mid-way) we'd rather keep running than hang forever. The
            // `stack_dump_emitted` guard ensures we dump at most once per
            // nested `execute()` call so ack counts remain meaningful.
        }

        // T19.H7 diag — opcode counter. Removed; documented findings in
        // docs/roadmap-100.md T19.H7 section. Last localization:
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
                if let RuntimeError::ArrayIndexOutOfBoundsException { index } = &re {
                    let f = &thread.frames[frame_idx];
                    eprintln!(
                        "[AIOOBE2] index={} class={} method={}{} pc={}",
                        index,
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor(),
                        invoke_pc
                    );
                }
            }
            let exc_result = super::exceptions::throw_runtime_error(shared, thread, re);
            match exc_result {
                MethodCallFailed::ExceptionThrown(exc) => {
                    let mut exc_pc = invoke_pc;
                    // GC-root gap: see the identical pin in the
                    // `pending_java_exception` arm above — this loop has the
                    // same unpinned-across-frame-pops-and-lazy-class-load hazard.
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
                                    // T17.Δ — exception-unwind.
                                    pop_and_recycle_frame_with_reason(shared, thread, true);
                                    frame_idx -= 1;
                                    exc_pc = thread.frames[frame_idx].last_instr_pc;
                                } else {
                                    // Perf: the previous `exc_class` / `caller` /
                                    // `mname` bindings here each took a
                                    // `class_manager.read()` RwLock + `to_string()`
                                    // allocation on every top-frame exception
                                    // unwind, but their values were never used
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
        // H7: the fast-path local-access handlers (lload/dload, istore/fstore,
        // astore, lstore/dstore — and the `_unchecked` get/set helpers used by
        // the iload/iadd/etc. fusions) index `frame.locals` with the raw
        // bytecode operand WITHOUT a bounds check, relying entirely on the
        // bytecode verifier having proven the operand `< max_locals`. When
        // verification is globally disabled (`-noverify` / `-Xverify:none`)
        // that invariant no longer holds, so an out-of-range operand would
        // index out of bounds. Disable the unchecked fast path entirely in
        // that mode and fall back to the bounds-checked slow path; the
        // per-site checks below are a second line of defence (e.g. for
        // per-class `skip_verification` generated classes that this cheap
        // global flag does not cover). `skip_verification` is a single bool
        // load — no per-instruction RwLock acquire.
        // SAFETY (every `hot_fp` deref below): see the hoist note above —
        // reads only, no push, no `&mut` reborrow of the stack in between.
        let use_fast_path = !shared.config.skip_verification;
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
                                        let _ = frame;
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
                                                pending_java_exception = Some((exc, entry_pc));
                                                continue;
                                            }
                                            OsrBackoffOutcome::Skip => {}
                                        }
                                        safepoint_check(shared, thread);
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
                0x84 => {
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
                        let _ = frame; // drop borrow before try_osr
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
                                pending_java_exception = Some((exc, entry_pc));
                                continue;
                            }
                            OsrBackoffOutcome::Skip => {}
                        }
                        safepoint_check(shared, thread);
                    }
                    continue;
                }
                // if_icmpge — AUDIT CRIT-4: int-typed pop, no Value enum match.
                0xa2 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    let taken = va >= vb;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;

                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // if_icmplt
                0xa1 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    let taken = va < vb;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;

                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // if_icmple
                0xa4 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    let taken = va <= vb;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // if_icmpgt
                0xa3 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    let taken = va > vb;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // if_icmpne
                0xa0 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    let taken = va != vb;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // if_icmpeq
                0x9f => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    let taken = va == vb;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // ireturn / lreturn / freturn / dreturn / areturn
                0xac..=0xb0 => {
                    // Bit-exact return-value transfer. The prior
                    // `pop_unchecked()` + `push_unchecked()` round-tripped the
                    // slot through `to_value()`/`from_value()`, which decoded a
                    // category-2 long whose NaN-box bit pattern collides with a
                    // tagged sub-tag (e.g. `lreturn` of `0xFFFC_…`, a BC safegcd
                    // `Mod.updateDE30`/`updateFG30` accumulator) as `Value::Int`,
                    // dropping the high bits. Copy the raw CompactValue for
                    // i/l/f/d-return; areturn still normalizes jobject-as-Long
                    // handles via `coerce_value_for_return`. See
                    // docs/bc-ec-mod-mododdinverse-investigation.md.
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
                    let (cv, is_long) = frame.stack.pop_compact_with_long_mark_unchecked();
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
                        let ret = crate::jit::return_type(frame.method_descriptor());
                        coerce_value_for_return_validated(shared, cv.to_value(), ret)
                    } else {
                        decode_arg_kind_aware(cv, is_long, desc_byte)
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
                        pop_and_recycle_frame(shared, thread);
                        frame_idx -= 1;
                        if opcode == 0xb0 {
                            // areturn: push the normalized reference value.
                            thread.frames[frame_idx].stack.push_unchecked(value);
                        } else if opcode == 0xad {
                            // lreturn: `push_compact` alone marks the parent
                            // slot KIND_UNKNOWN, discarding the `is_long`
                            // distinction this arm just computed via
                            // `pop_compact_with_long_mark_unchecked`. A
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
                    let taken = val <= 0;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // ifge
                0x9c => {
                    let val = frame.stack.pop_int_unchecked();
                    let taken = val >= 0;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // ifgt
                0x9d => {
                    let val = frame.stack.pop_int_unchecked();
                    let taken = val > 0;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // iflt
                0x9b => {
                    let val = frame.stack.pop_int_unchecked();
                    let taken = val < 0;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // ifne
                0x9a => {
                    let val = frame.stack.pop_int_unchecked();
                    let taken = val != 0;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
                }
                // ifeq
                0x99 => {
                    let val = frame.stack.pop_int_unchecked();
                    let taken = val == 0;
                    if pgo_enabled {
                        let (cid, mn, md) = method_key_parts(frame);
                        shared
                            .jit
                            .profile_store
                            .record_branch_borrowed(cid, mn, md, saved_pc, taken);
                    }
                    if taken {
                        let offset = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                        frame.pc = (saved_pc as isize + offset as isize) as usize; // Cast: signed branch offset arithmetic
                        if offset < 0 {
                            if pgo_enabled {
                                let (cid, mn, md) = method_key_parts(frame);
                                shared
                                    .jit
                                    .profile_store
                                    .record_backedge_borrowed(cid, mn, md, saved_pc);
                            }
                            frame.backward_count += 1;
                            let entry_pc = frame.pc;
                            let _ = frame;
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
                                    pending_java_exception = Some((exc, entry_pc));
                                    continue;
                                }
                                OsrBackoffOutcome::Skip => {}
                            }
                            safepoint_check(shared, thread);
                        }
                    } else {
                        frame.pc = saved_pc + 3;
                    }
                    continue;
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
                                RuntimeError::ArrayIndexOutOfBoundsException { index },
                                saved_pc,
                            ));
                            continue;
                        }
                        // Widening: index conversion
                        match shared.mem.heap.get_array_element(arr_ref, index as usize) {
                            Ok(value) => {
                                frame.stack.push_unchecked(value);
                                frame.pc = saved_pc + 1;
                                continue;
                            }
                            Err(i) => {
                                let _ = frame;
                                pending_runtime_error = Some((
                                    RuntimeError::ArrayIndexOutOfBoundsException { index: i },
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
                    let cv = frame.stack.pop_compact();
                    let value = match opcode {
                        0x50 => Value::Long(cv.as_long_unchecked()),
                        0x52 => {
                            // dastore: untagged slot is raw f64 bits.
                            use crate::types::CompactTag;
                            match cv.tag() {
                                CompactTag::Double => Value::Double(f64::from_bits(cv.to_bits())),
                                CompactTag::Long => {
                                    // Rare: NaN-tagged-collision long landing
                                    // in a double slot; reinterpret the bits.
                                    Value::Double(f64::from_bits(cv.to_bits()))
                                }
                                _ => cv.to_value(),
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
                                RuntimeError::ArrayIndexOutOfBoundsException { index },
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
                                    RuntimeError::ArrayIndexOutOfBoundsException { index: i },
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
                        if index < 0 {
                            let _ = frame;
                            pending_runtime_error = Some((
                                RuntimeError::ArrayIndexOutOfBoundsException { index },
                                saved_pc,
                            ));
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
                                let elem_cls = shared
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(shared.mem.heap.class_id_of(elem_ref))
                                    .map(|c| c.name.to_string())
                                    .unwrap_or_else(|| "?".to_string());
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
                                    RuntimeError::ArrayIndexOutOfBoundsException { index: i },
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
                    let tdigest_kernel = frame.class_name() == "org/elasticsearch/tdigest/Dist"
                        && matches!(frame.method_name(), "quantile" | "cdf")
                        && frame.method_descriptor() == "(DILjava/util/function/Function;)D";
                    // SAFETY: `code_ptr` addresses this frame's bytecode and the
                    // preceding length check proves every inspected offset is in bounds.
                    if tdigest_kernel
                        && saved_pc + 14 <= code_len
                        && unsafe { *code_ptr.add(saved_pc + 3) } == 0xb9
                        && unsafe { *code_ptr.add(saved_pc + 8) } == 0xc0
                        // SAFETY: `saved_pc + 14 <= code_len` was checked
                        // above, so offsets +3/+8/+11 are all in bounds of the
                        // frame's padded bytecode buffer.
                        && unsafe { *code_ptr.add(saved_pc + 11) } == 0xb6
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
                    match execute_invoke_kind(shared, thread, frame_idx, cp_index, false, true, saved_pc) {
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
            Ok(InstructionResult::Continue) => continue,
            Ok(InstructionResult::FramePushed) => {
                // A new bytecode frame was pushed — execute it iteratively
                frame_idx = thread.frames.len() - 1;
                continue;
            }
            Ok(InstructionResult::Return(value)) => {
                // Slow-path return — check for stackless frames
                if frame_idx > initial_frame_idx {
                    let ret = crate::jit::return_type(thread.frames[frame_idx].method_descriptor());
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
mod constants;
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
mod jit_bridge;
pub use jit_bridge::*;

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
        let arr = shared
            .mem
            .heap
            .try_alloc_array(level_class_id, element_type, length)
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
        let arr = shared
            .mem
            .heap
            .try_alloc_array(level_class_id, ArrayElementType::Reference, length)
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
                .map_err(|idx| RuntimeError::ArrayIndexOutOfBoundsException { index: idx })?;
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

#[cfg(test)]
mod wave1_adoption_tests;

#[cfg(test)]
mod tests;
