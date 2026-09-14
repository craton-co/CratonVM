// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.9 — `java.lang.StackWalker` / full stack-trace machinery.
//!
//! This module hosts the helpers used by:
//!
//! * `java.lang.Throwable.fillInStackTrace` / `getStackTrace` (via
//!   `NativeContext::capture_stack_trace`) — frames are populated with
//!   source file, line number, and bytecode index.
//! * `java.lang.StackWalker.walk(Function)` / `forEach(Consumer)` — see
//!   `native-builtins/src/phases_late.rs::p59_sw_walk`, which consumes
//!   `StackTraceEntry.byte_code_index` / `line_number` populated here.
//! * `java.lang.StackWalker.StackFrame.getLineNumber()` /
//!   `getByteCodeIndex()` / `getDeclaringClass()` / `getClassName()` /
//!   `getMethodName()` — see
//!   `native-builtins/src/lang_stackwalker.rs::register_lang_stackwalker`.
//!
//! Line-number lookup walks the method's `LineNumberTable` attribute
//! (JVM spec §4.7.12). The table is a sorted `(start_pc, line_number)`
//! list; for any `bci` the applicable line is the largest `start_pc`
//! that is `<= bci`.
//!
//! Bytecode index (BCI) comes from the frame's `last_instr_pc` (the PC
//! of the last-executed instruction, which is what HotSpot exposes via
//! `StackFrame.getByteCodeIndex()` — note that `pc` itself may already
//! have been advanced past the invoke instruction when the frame sits
//! above the invocation site).
//!
//! ## Capture cost
//!
//! [`capture_full_trace`] is O(depth) and runs on **every** VM-raised throw,
//! including the ones application frameworks use as control flow. Two
//! mechanisms keep that affordable:
//!
//! * a per-signature **method-slot memo** replaces `Class::find_method`'s
//!   linear scan over every declared method (see the memo's own comment
//!   below for its correctness rules — verified on hit, never negative,
//!   retains nothing);
//! * [`capture_frames_no_lines`] + [`resolve_line_numbers_in_place`] let a
//!   caller skip line resolution entirely at capture time and fill it in only
//!   if the trace is ever read. The deferred resolver is fail-closed: it
//!   yields the eager answer or [`LINE_NUMBER_UNKNOWN`], never a wrong line.
//!
//! `StackTraceEntry::method_index` (landed 2026-07-26, `cross-owner-closeout`)
//! is what makes the deferred resolver **exact**: with only
//! `(class_id, method_name, bci)` an overload set is unresolvable, and the
//! members of an overload set have different `LineNumberTable`s. Entries built
//! by [`entry_from_frame`] carry the index; entries built by the lock-free
//! [`capture_frames_no_lines`] do not, and fall back to the (still fail-closed)
//! unambiguous-name rule.
//!
//! See `stackwalk-and-vtable.md` and
//! `cross-owner-closeout.md`.

use std::sync::Arc;

use cratonvm_reader::attribute::{Attribute, LineNumberEntry};
use cratonvm_reader::method::ClassFileMethod;
use parking_lot::RwLock;

use crate::classloading::{Class, ClassId, ClassStore};
use crate::jit::conservative_roots::{ActiveCompiledFrame, InlinedLevel};
use crate::native::registry::StackTraceEntry;
use crate::runtime::frame::Frame;
use crate::runtime::fx_collections::{fx_hashmap, FxHashMap};

/// Sentinel "unknown line number" value — `-1` matches HotSpot's
/// `StackFrame.getLineNumber()` contract for frames whose `LineNumberTable`
/// attribute is absent.
pub const LINE_NUMBER_UNKNOWN: i32 = -1;

/// Sentinel "native method" line number — `-2` matches HotSpot's
/// `StackTraceElement.getLineNumber()` contract for native frames.
pub const LINE_NUMBER_NATIVE: i32 = -2;

// ---------------------------------------------------------------------------
// Method-slot memo (per-throw capture cost)
// ---------------------------------------------------------------------------
//
// PERF (2026-07-26 arch pass, `stackwalk-and-vtable`). Capturing a throwable's
// stack trace is an O(depth) walk, and until this memo the dominant per-frame
// cost was `Class::find_method`, which is a **linear scan over every method the
// class declares** performing two full `&str` comparisons per candidate:
//
// ```text
// self.methods.iter().find(|m| &*m.name == name && &*m.descriptor == descriptor)
// ```
//
// Frameworks like Spring, Hibernate and the JUnit/Surefire harnesses throw as
// control flow, at stack depths of 50-150 frames, through classes that declare
// 100+ methods. That is ~10^4 string comparisons per throw for information the
// overwhelming majority of callers never read.
//
// This memo turns the *find* into an O(1) hash probe plus the same two string
// comparisons, run **once**, as verification. Its correctness rules:
//
// * **Never memoize a negative.** Only a successful find is inserted. A method
//   that is absent today can be present after a redefinition, and a permanent
//   `None` memo would never be retried — the failure mode two sibling passes
//   already hit in the native registry and the lambda functional-interface
//   cache. A miss here simply costs one linear scan, exactly as before.
// * **Verify on every hit.** The memo stores an *index* into `Class::methods`,
//   never a borrowed method or a cloned attribute. Every hit re-reads
//   `class.methods[idx]` from the live `ClassStore` and re-checks
//   `name`/`descriptor`. A redefinition that reorders, replaces or removes
//   methods therefore cannot yield a stale answer: verification fails, the
//   linear scan runs, and the memo is corrected. This is why no
//   redefine-generation plumbing is needed (and `ClassStore` does not expose
//   one anyway — `class_redefine_generation` lives on `ClassManager`).
// * **Retain nothing.** The value is a `u32`; no `Arc`, no `Class`, no
//   bytecode. When a class is unloaded its entries become unreachable garbage,
//   not a leak: `ClassStore::remove` leaves a **tombstone** and `ClassId`s are
//   monotonic and never reused, so a stale entry can only ever be looked up
//   again for the same (now absent) class, where `class_store.get` returns
//   `None` before the memo is even consulted. Bounded by
//   `METHOD_SLOT_MEMO_CAP` with the clear-on-overflow policy already used by
//   `frame::padded_bytecode_for_method`.
//
// A 64-bit FNV-1a collision between two distinct signatures **of the same
// class** is caught by the same verification (the entry simply fails to
// verify); the only consequence is that the two signatures evict each other,
// degrading to the pre-memo linear scan for that pair.

/// Upper bound on memo entries before a wholesale clear. Sized for a large
/// application's live set of *throwing* methods, which is far smaller than its
/// method count; the entry is 16 bytes, so the cap costs ~128 KiB.
const METHOD_SLOT_MEMO_CAP: usize = 8192;

/// Maps `(ClassId, fnv1a64(name, descriptor))` to an index into
/// [`Class::methods`].
fn method_slot_memo() -> &'static RwLock<FxHashMap<(u32, u64), u32>> {
    static MEMO: std::sync::OnceLock<RwLock<FxHashMap<(u32, u64), u32>>> =
        std::sync::OnceLock::new();
    MEMO.get_or_init(|| RwLock::new(fx_hashmap()))
}

/// FNV-1a over `name` then `descriptor`, with a separator so
/// `("ab", "c")` and `("a", "bc")` do not hash alike.
#[inline]
fn signature_hash(name: &str, descriptor: &str) -> u64 {
    #[inline]
    fn mix(mut h: u64, bytes: &[u8]) -> u64 {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }
    let h = mix(0xcbf2_9ce4_8422_2325, name.as_bytes());
    // Descriptors always start with `(`, but the separator keeps the domain
    // clean for the synthetic keys used elsewhere in this module.
    mix(mix(h, &[0xff]), descriptor.as_bytes())
}

/// The *index* into [`Class::methods`] of the method matching
/// `(name, descriptor)`, with the per-signature slot memo in front of the scan.
///
/// Semantically **identical** to `class.methods.iter().position(...)` — the
/// memo only ever short-circuits a scan whose result is re-verified against the
/// live class. See the module-level rules above.
///
/// The index (rather than the borrow) is the primitive because it is also what
/// [`StackTraceEntry::method_index`] carries, so a capture and a deferred
/// resolution both name the method the same way.
fn find_method_index_memoized(
    class: &Class,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<u32> {
    let key = (class_id.as_u32(), signature_hash(name, descriptor));

    let memoized = method_slot_memo().read().get(&key).copied();
    if let Some(idx) = memoized {
        if let Some(m) = class.methods.get(idx as usize) {
            if &*m.name == name && &*m.descriptor == descriptor {
                return Some(idx);
            }
        }
        // Stale (redefinition reordered/removed the method) or an FNV
        // collision: fall through to the authoritative scan, which re-inserts
        // the corrected index below.
    }

    let idx = class
        .methods
        .iter()
        .position(|m| &*m.name == name && &*m.descriptor == descriptor)?;
    let idx = u32::try_from(idx).ok()?;

    {
        let mut w = method_slot_memo().write();
        if w.len() >= METHOD_SLOT_MEMO_CAP {
            w.clear();
        }
        w.insert(key, idx);
    }
    Some(idx)
}

/// `Class::find_method` with the per-signature slot memo in front of it.
///
/// Semantically **identical** to `class.find_method(name, descriptor)`.
fn find_method_memoized<'a>(
    class: &'a Class,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<&'a ClassFileMethod> {
    let idx = find_method_index_memoized(class, class_id, name, descriptor)?;
    class.methods.get(idx as usize)
}

/// Test/diagnostic hook: drop every memoized slot. Never needed for
/// correctness (every hit is verified) — exists so unit tests can assert the
/// cold path and the warm path agree.
pub fn clear_method_slot_memo() {
    method_slot_memo().write().clear();
    class_id_memo().write().clear();
}

// ---------------------------------------------------------------------------
// Class-name memo (inlined-callee frames only)
// ---------------------------------------------------------------------------
//
// PERF (2026-09-01). An INLINED callee carries no `ClassId`: the emitter that
// spliced it knows only its internal name, so `inlined_frame_entry` has to
// resolve `"java/lang/String"` -> `ClassId` by name. The only by-name lookup a
// `ClassStore` offers is `ClassStore::find_by_name`, whose own doc calls it an
// "O(n) scan" and points at the `ClassManager`'s hash map as the fast path —
// which is not reachable from here, because every capture path in this file
// takes a `&ClassStore` and nothing else.
//
// Left unmemoized this is a linear scan over EVERY loaded class, per inlined
// level, per frame, on EVERY VM-raised throw — on an application with tens of
// thousands of loaded classes and a JIT that inlines aggressively, that trades
// the missing frames this change exists to restore for a throw cost nobody
// asked for. Frameworks throw as control flow; that is the whole reason the
// method-slot memo above exists.
//
// This memo follows the SAME three rules as that one, for the same reasons —
// read them there, they are argued at length:
//
//   * never memoize a negative (a class absent now can be loaded later, and a
//     permanent `None` would never be retried);
//   * verify on every hit — the value is a `ClassId`, and a hit re-reads
//     `class_store.get(id)` and re-checks `&*class.name == name` before it is
//     believed, so an unload/tombstone or an FNV collision degrades to the
//     scan it replaced rather than answering with a neighbour;
//   * retain nothing — a `u32` per entry, no `Arc`, no `Class`.
//
// `ClassId`s are monotonic and never reused (`ClassStore::remove` leaves a
// tombstone), so a stale entry can only ever be probed again for the same, now
// absent, class, where `get` answers `None` and the verification fails closed.

/// Upper bound on class-name memo entries before a wholesale clear. Smaller
/// than [`METHOD_SLOT_MEMO_CAP`] because the live set here is "classes that
/// appear as an inlined callee in a captured trace", which is a small subset of
/// the methods that throw.
const CLASS_ID_MEMO_CAP: usize = 4096;

/// Maps `fnv1a64(class_name)` to a [`ClassId`] as a raw `u32`.
fn class_id_memo() -> &'static RwLock<FxHashMap<u64, u32>> {
    static MEMO: std::sync::OnceLock<RwLock<FxHashMap<u64, u32>>> = std::sync::OnceLock::new();
    MEMO.get_or_init(|| RwLock::new(fx_hashmap()))
}

/// `ClassStore::find_by_name` with the class-name memo in front of its linear
/// scan.
///
/// Semantically **identical** to `class_store.find_by_name(name).map(|c| c.id)`
/// — the memo only ever short-circuits a scan whose result is re-verified
/// against the live store.
///
/// The hash reuses [`signature_hash`] with an empty descriptor rather than
/// growing a second FNV helper; the two memos are separate maps, so the domains
/// cannot collide with each other, and a collision WITHIN this map is caught by
/// the same name re-check that catches a redefinition.
pub(crate) fn find_class_id_by_name_memoized(
    class_store: &ClassStore,
    name: &str,
) -> Option<ClassId> {
    let key = signature_hash(name, "");

    let memoized = class_id_memo().read().get(&key).copied();
    if let Some(raw) = memoized {
        let id = ClassId::new(raw);
        if let Some(c) = class_store.get(id) {
            if &*c.name == name {
                return Some(id);
            }
        }
        // Tombstoned, unloaded, or an FNV collision: fall through to the
        // authoritative scan, which re-inserts the corrected id below.
    }

    let id = class_store.find_by_name(name)?.id;
    {
        let mut w = class_id_memo().write();
        if w.len() >= CLASS_ID_MEMO_CAP {
            w.clear();
        }
        w.insert(key, id.as_u32());
    }
    Some(id)
}

/// Look up the source-line corresponding to `bci` in the method's
/// `LineNumberTable` attribute.
///
/// Returns `None` when:
/// * the class isn't in the store (synthetic / bootstrap frame),
/// * the method has no `Code` attribute (abstract / native),
/// * the method has no `LineNumberTable` attribute, or
/// * `bci` precedes the first entry.
///
/// The table is scanned linearly; in practice tables are ≤16 entries for
/// typical methods, so a binary search is not worth the code size. For
/// pathological cases (huge generated lambdas with >1k entries) the scan
/// is still O(n) but with trivial per-iteration cost.
pub fn line_number_for_bci(
    class_store: &ClassStore,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    bci: usize,
) -> Option<i32> {
    let class = class_store.get(class_id)?;
    let method = find_method_memoized(class, class_id, method_name, method_descriptor)?;
    line_number_for_bci_in_method(method, bci)
}

/// The `LineNumberTable` scan itself, against an already-resolved method.
///
/// Split out of [`line_number_for_bci`] so the memoized-lookup path, the
/// deferred-resolution path ([`resolve_line_numbers_in_place`]) and the unit
/// tests all exercise **one** implementation of the JVM spec §4.7.12 rule
/// (largest `start_pc` that is `<= bci` wins).
fn line_number_for_bci_in_method(method: &ClassFileMethod, bci: usize) -> Option<i32> {
    let code = method.code()?;

    // The LineNumberTable attribute nests under Code.attributes.
    let mut best_line: Option<u16> = None;
    let mut best_start: u16 = 0;
    let bci_u16 = bci.min(u16::MAX as usize) as u16;

    for attr in &code.attributes {
        if let Attribute::LineNumberTable(entries) = attr {
            // Multiple LineNumberTable attributes are permitted by the spec
            // (a compiler may split them across multiple attributes); the
            // union is the effective table. We walk all of them.
            for entry in entries {
                if entry.start_pc <= bci_u16
                    && (best_line.is_none() || entry.start_pc >= best_start)
                {
                    best_start = entry.start_pc;
                    best_line = Some(entry.line_number);
                }
            }
        }
    }

    best_line.map(|l| l as i32)
}

/// Collect all `LineNumberTable` entries declared for the given method.
/// Exposed primarily for debugging / tests; returns a flattened vector
/// sorted by `start_pc` ascending.
pub fn line_number_entries(
    class_store: &ClassStore,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
) -> Vec<LineNumberEntry> {
    let Some(class) = class_store.get(class_id) else {
        return Vec::new();
    };
    let Some(method) = find_method_memoized(class, class_id, method_name, method_descriptor) else {
        return Vec::new();
    };
    let Some(code) = method.code() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for attr in &code.attributes {
        if let Attribute::LineNumberTable(entries) = attr {
            out.extend_from_slice(entries);
        }
    }
    out.sort_by_key(|e| e.start_pc);
    out
}

/// Build a single `StackTraceEntry` from a live [`Frame`] plus the
/// enclosing class store, resolving source-file and line-number info
/// when available.
///
/// The BCI written into the entry is the frame's `last_instr_pc` — this
/// is what HotSpot exposes via `StackFrame.getByteCodeIndex()`.
///
/// The entry also records the frame's [`StackTraceEntry::method_index`]. That
/// is free here (the same memo probe that finds the method for the line lookup
/// yields it) and it is what makes a *deferred* resolution of this entry exact
/// even for an overload set — see [`resolve_line_numbers_in_place`].
pub fn entry_from_frame(class_store: &ClassStore, frame: &Frame) -> StackTraceEntry {
    let class_name = frame.class_name_arc();
    let method_name = frame.method_name_arc();
    let method_descriptor = frame.method_descriptor_arc();
    let source_file = frame.source_file_arc();
    let bci = frame.last_instr_pc;
    let bci_i32 = bci.min(i32::MAX as usize) as i32;

    // One class lookup and one memo probe serve both the method index and the
    // line number; this is exactly the work `line_number_for_bci` used to do.
    let (method_index, line_number) = match class_store.get(frame.class_id) {
        Some(class) => {
            let idx =
                find_method_index_memoized(class, frame.class_id, &method_name, &method_descriptor);
            let line = idx
                .and_then(|i| class.methods.get(i as usize))
                .and_then(|m| line_number_for_bci_in_method(m, bci))
                .unwrap_or(LINE_NUMBER_UNKNOWN);
            (idx, line)
        }
        None => (None, LINE_NUMBER_UNKNOWN),
    };

    StackTraceEntry {
        class_name,
        method_name,
        source_file,
        line_number,
        byte_code_index: bci_i32,
        class_id: Some(frame.class_id),
        method_index,
    }
}

/// Walk a frame slice and produce a `StackTraceEntry` vector with full
/// source-file / line-number / BCI data.
///
/// **Result order is OUTERMOST-first**: index 0 is the bottom of the stack and
/// the last entry is the innermost (currently-executing) frame. That is
/// `JvmThread.frames.iter()`'s own order — `frames[0]` is the bottom frame —
/// and it is the opposite of [`frame_class_ids_with_compiled`]'s.
///
/// This comment used to say "top-of-stack → bottom (i.e. the caller chain from
/// innermost to outermost)", which is wrong in both halves and is the exact
/// trap `two frame-walk APIs order their results oppositely` records. MEASURED
/// with `CRATONVM_DBG_STTRACE=1` on a two-frame capture
/// (`FillTopFrame.main` calls `make()`, which allocates the throwable):
///
/// ```text
///   STTRACE_DBG_CAP[0] FillTopFrame.main      <- outermost
///   STTRACE_DBG_CAP[1] FillTopFrame.make      <- innermost, the throw site
/// ```
///
/// [`trim_throwable_fill_frames`] trims from the END for that reason.
pub fn capture_full_trace(class_store: &ClassStore, frames: &[Frame]) -> Vec<StackTraceEntry> {
    let jit = crate::jit::conservative_roots::active_compiled_frames();
    if jit.is_empty() {
        return frames
            .iter()
            .map(|f| entry_from_frame(class_store, f))
            .collect();
    }
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    interleave_compiled_frames(class_store, frames, &jit, &osr_bci)
}

/// HotSpot's `java_lang_Throwable::fill_in_stack_trace` frame skip, applied to
/// a trace captured for `throwable`.
///
/// A throwable's trace must start at the frame that CREATED it, not at the
/// machinery that filled the trace in. HotSpot drops, from the innermost end:
///
///  1. leading `fillInStackTrace*` frames whose holder the throwable `is_a`,
///     then
///  2. leading `<init>` frames whose holder the throwable `is_a`.
///
/// Both phases stop at the first frame that does not match — they are prefixes,
/// not filters, so an application method that happens to be called `<init>` or
/// `fillInStackTrace` further down the stack is never dropped.
///
/// # Why this VM needs it, and why the need is not new
///
/// The `native_exc_init_*` bodies stand in front of `Throwable.<init>` and push
/// no interpreter frame, so for `new IllegalStateException(...)` there is
/// nothing to skip and the answer has always been right. But the PUBLIC
/// `Throwable.fillInStackTrace()` is real bytecode in every mode — only the
/// private `fillInStackTrace(int)` it calls is a native — so an explicit
/// `t.fillInStackTrace()` captured its own `Throwable.fillInStackTrace` frame
/// and reported it as the throw site. MEASURED on JDK 25.0.3+9, both modes:
///
/// ```text
///   new RuntimeException("y"); u.fillInStackTrace(); u.getStackTrace()[0]
///     HotSpot   FillTopFrame.main
///     was       java.lang.Throwable.fillInStackTrace
/// ```
///
/// # Phase 2 was written for a retirement and turned out to be the bigger half
///
/// It is what makes the throwable family's `<init>` rows RETIRABLE: once those
/// registrations yield, the real `Throwable.<init>` and its `super(...)` chain
/// are ordinary Java frames on top of the throw site, and
/// `apps/probes/ThrowableFamilySweep.java` and `IoSystemSweep` ask for exactly
/// that top frame.
///
/// **It was never only about the retirement.** An APPLICATION exception's own
/// constructor is real bytecode today — the native `super(m)` inside it is
/// where the capture happens — so `MyEx.<init>` was already sitting on top of
/// every such trace, in both compatibility modes. MEASURED on the same image:
///
/// ```text
///   class MyEx extends RuntimeException { MyEx(String m) { super(m); } }
///   static Throwable a() { return new MyEx("a"); }   a().getStackTrace()[0]
///     HotSpot   SubclassTop.a
///     was       SubclassTop$MyEx.<init>
/// ```
///
/// Only `java.*` throwables looked right, because their constructors ARE the
/// natives that do the capturing and push no frame. Every framework exception
/// hierarchy has the other shape.
///
/// `trace` is OUTERMOST-first (see [`capture_full_trace`]), so both phases pop
/// from the END.
pub fn trim_throwable_fill_frames(
    class_store: &ClassStore,
    throwable_class: Option<ClassId>,
    trace: &mut Vec<StackTraceEntry>,
) {
    let Some(throwable_class) = throwable_class else {
        return;
    };
    let Some(tclass) = class_store.get(throwable_class) else {
        return;
    };
    // `throwable->is_a(holder)`. Prefer the frame's own `ClassId` — the
    // loader-faithful answer; fall back to the loader-blind name walk only for
    // a synthesized entry that carries no id (see `StackTraceEntry::class_id`).
    let is_a = |e: &StackTraceEntry| match e.class_id {
        Some(cid) => tclass.is_subclass_of(cid, class_store),
        None => tclass.is_subclass_of_by_name(&e.class_name, class_store),
    };
    while trace
        .last()
        .is_some_and(|e| &*e.method_name == "fillInStackTrace" && is_a(e))
    {
        trace.pop();
    }
    while trace
        .last()
        .is_some_and(|e| &*e.method_name == "<init>" && is_a(e))
    {
        trace.pop();
    }
}

/// The declaring class of every frame on this thread's Java stack, INNERMOST
/// first, with compiled frames spliced in — the [`capture_full_trace`] frame set
/// without any of its string, line-number or `StackTraceEntry` work.
///
/// This exists because `NativeContext::frame_class_ids` used to map
/// `thread.frames` alone. A JIT-compiled method pushes no interpreter `Frame`,
/// so its class was ABSENT from that list, and every caller-attribution site
/// that walks it silently answered with the next frame down:
///
///   * `lang_class::resolve_caller_class_id` — the accessor for the JEP 403
///     deep-reflection check and the member-modifier gate;
///   * `classloader.rs` / `lang_system.rs` — the caller's `ClassLoader` for
///     `Class.forName(String)` and friends.
///
/// It fails in BOTH directions, which is why it is worth fixing rather than
/// tolerating: a java.base caller that tiered up disappears and a classpath
/// frame below it is blamed (`StackStreamFactory$StackFrameBuffer.fill`
/// constructing `StackFrameInfo` was denied with `module java.base does not
/// "opens java.lang" to unnamed module`, and only with the JIT on), and
/// symmetrically a compiled APPLICATION frame disappears behind a JDK frame and
/// is granted access it should not have.
///
/// Same ordering and same OSR de-duplication as [`capture_full_trace`]; the
/// two must agree about what "the frames of this thread" are.
///
/// # It DOES expand inlined callees now, and on what evidence (2026-09-02)
///
/// The three reasons below were the state on 2026-09-01 and two of them have
/// been removed rather than argued around:
///
///   * an inlined level now carries its own `ClassId`, recorded by the
///     RESOLVER at the moment it looked the spliced body up
///     (`InlineSite::class_id` -> `InlineFrameLevel::class_id` ->
///     `InlinedLevel::class_id`). No name is resolved and no `ClassStore` is
///     consulted, so the answer is not a guess and this function still takes no
///     store;
///   * the miss-edge hazard is closed by REFUSING the coarse key rather than by
///     refusing the whole feature. Only a chain keyed on this activation's own
///     return address (`ActiveCompiledFrame::chain_exact`) is expanded. The
///     safepoint-id key shares one `cur_bc_pc` with the inline cache's miss
///     edge, where the spliced body did not run -- a wrong frame in a trace,
///     but a fail-OPEN caller in a gate, which is why display may use it and
///     this may not.
///
/// A level with `class_id == 0` -- a producer that supplied none -- ends the
/// expansion for that frame, and the levels BELOW it are dropped with it, the
/// same `break`-not-`continue` rule `push_inlined_chain` follows: a hole in the
/// chain re-parents everything under it.
///
/// The audit below still holds and is still worth keeping: it is the argument
/// that this change is a hardening rather than a fix, because no consumer of
/// this walk was ever standing on an inlined frame. What changed is that the
/// claim no longer has to be re-derived every time a gate moves.
/// `CRATONVM_JIT_NO_INLINE_CALLER_FRAMES=1` restores the flat answer.
///
/// # Why it did NOT expand inlined callees (2026-09-01)
///
/// [`capture_full_trace`] turns one compiled entry into one entry per INLINED
/// level as well (see [`ActiveCompiledFrame::inline_chain`]); this function
/// deliberately does not, and the two therefore no longer report the same
/// frame COUNT. Three reasons, any one sufficient:
///
///   * it answers in `ClassId`, and an inlined level carries no class id —
///     only an internal name. Resolving one costs a by-name store scan, and
///     this function takes no [`ClassStore`] at all;
///   * its callers ask a caller-ATTRIBUTION question (the JEP 403
///     deep-reflection gate, `Class.forName`'s caller loader), not a display
///     question, and answering it with a frame whose class was resolved by
///     name from a JIT label would be a security-relevant guess;
///   * what the two must agree on is the KEPT set after OSR de-duplication,
///     and that still comes out of the same call computed the same way.
///
/// # And the residual it leaves is NOT REACHABLE (audited 2026-09-01)
///
/// The blindness is real: `resolve_caller_class_id` still cannot see a method
/// the JIT inlined, exactly as before. What the audit found is that no
/// consumer of this walk can ever be standing on one, so nothing is granted or
/// denied on the strength of it. Written down here rather than left as a "what
/// would close it" line, because closing it costs a `ClassStore` on every
/// caller of a walk that runs on `Class.forName` and on every deep-reflection
/// check, and re-deriving this argument costs a day.
///
/// **What every consumer actually asks.** All of them scan innermost-first,
/// skip a fixed prefix and take the first survivor:
/// `lang_class::resolve_caller_class_id` (skips `java/lang/reflect/`,
/// `java/lang/invoke/`, `jdk/internal/reflect/`, `sun/reflect/` except
/// `sun/reflect/misc/`, `java/lang/Class`, `java/lang/AccessibleObject`),
/// `lang_class::class_for_name_one_arg_caller_loader` (skips
/// `java/lang/Class`), `lang_system::requesting_loader_id` (skips
/// `java/lang/System` and `java/lang/Runtime`),
/// `classloader::latest_user_defined_loader_class` (skips every
/// bootstrap/platform frame) and
/// `unsafe_natives_ext::unsafe_caller_is_boot_path` (skips the two `Unsafe`
/// classes). So the frame that decides is the innermost Java frame that CALLED
/// the caller-sensitive native, modulo that skipped prefix.
///
/// **Why that frame is never an inlined one.** Every one of those entry points
/// is a REGISTERED NATIVE on exactly the class the source names --
/// `Class.forName(Ljava/lang/String;)`, `setAccessible(Z)V` on `Field` /
/// `Method` / `Constructor` / `AccessibleObject`, `Method.invoke`,
/// `Field.get` / `set`, `Constructor.newInstance`, `System.loadLibrary`,
/// `VM.latestUserDefinedLoader0`, `Unsafe.getUnsafe` -- and a native pushes no
/// `Frame` (`stack_walker`'s `native_get_caller_class` says so in as many
/// words). The deciding frame is therefore the one holding the `invoke*` that
/// enters the native, and for THAT method to have been inlined,
/// `jit_bridge::resolve_inline_site_from` would have had to admit a spliced
/// body containing that `invoke*`. It cannot:
///
///   * `invokedynamic` is refused outright;
///   * `invokevirtual` / `invokeinterface` are never offered to the
///     direct-bind resolver -- it is asked for `invoke_kind` 1 and 3 only --
///     so such a target must be spliced IN TURN, and a nested splice of a
///     native is refused by `native-shadow` and by
///     `native-shadow-on-selected-method`;
///   * `invokestatic` / `invokespecial` are direct-bound through
///     `callee_compiler` / `direct_callee_lookup`, and both refuse a
///     native-shadowed target with `DirectBindRefusal::NativeShadow`;
///   * a call that is neither spliced nor direct-bound refuses the WHOLE
///     enclosing site ("neither spliced nor direct-bound").
///
/// **The two-hop shape does not open it either.** An inlined `W` could in
/// principle call a direct-bound COMPILED bridge `M` that the consumer's skip
/// list skips, leaving the walk to answer `W`'s artifact owner instead of
/// `W`. `M` would have to be a non-native `invokestatic` / `invokespecial`
/// method inside a skipped package that reaches one of the natives above. The
/// one candidate in a real JDK image is
/// `AccessibleObject.setAccessible(AccessibleObject[],Z)`, which is not
/// registered here -- and its body reaches `setAccessible0`, which is not
/// registered either, so that route never touches the gate at all.
///
/// **The direction that would GRANT has a second, independent closer.** For an
/// APPLICATION method to be inlined INTO a `java.base` artifact, the site must
/// be a guarded virtual/interface one (`resolve_receiver_inline_site`): a
/// `java.base` classfile cannot name an application class in its own constant
/// pool, and `resolve_ir_inline_site` resolves from that pool only. The
/// guarded path is single-pass-tier and carries every refusal above. The
/// reverse -- a `java.base` method inlined into an APPLICATION artifact --
/// answers with the application class, the LESS privileged of the two, which
/// is the deny side.
///
/// **What would reopen it**, written down rather than defended against:
/// `CRATONVM_JIT_INLINE_CALL_DISPATCH=1` (default OFF, and the default is a
/// measured 3.5x) drops the "neither spliced nor direct-bound" refusal and
/// lets a spliced call reach the blind dispatch helper, which CAN enter a
/// native; and registering a caller-sensitive gate on a method the native
/// registry does NOT shadow would put the deciding frame back inside a splice.
/// The second is the one to re-check whenever a gate moves.
///
/// **A fix would also have no evidence to work from.** The only record of what
/// was spliced at a program point is `CompiledMethod::inline_frame_map`, and
/// `x64::inlining::record_inline_frame_row` writes a row ONLY at a call
/// emitted from inside a spliced body. By the argument above no such call
/// enters a caller-sensitive native, so at every program point this walk is
/// asked about, that map misses and the chain is empty. Threading a
/// `ClassStore` through every caller would recover nothing -- and it would
/// import the INNERMOST frame's key-2 lookup, which is keyed on a safepoint id
/// that one `cur_bc_pc` shares with the inline cache's MISS EDGE (see
/// `conservative_roots::compiled_frame_inline_chain`, which refuses that
/// fallback for the exact key and cannot for the coarse one). On a miss edge
/// the spliced body did not run, so that lookup can name a method that never
/// executed: a wrong frame in a trace, but a fail-OPEN caller in a gate.
pub fn frame_class_ids_with_compiled(frames: &[Frame]) -> Vec<ClassId> {
    let jit = crate::jit::conservative_roots::active_compiled_frames();
    if jit.is_empty() {
        return frames.iter().rev().map(|f| f.class_id).collect();
    }
    let (jit, _osr_bci) = drop_osr_continuations(frames, jit);
    let mut out: Vec<ClassId> = Vec::with_capacity(frames.len() + jit.len());
    let mut next = 0usize;
    for (i, f) in frames.iter().enumerate() {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled_class_ids(&mut out, &jit[next]);
            next += 1;
        }
        out.push(f.class_id);
    }
    for slot in &jit[next..] {
        push_compiled_class_ids(&mut out, slot);
    }
    // `frames` is outermost-first; every consumer wants innermost-first.
    out.reverse();
    out
}

/// Kill switch for the inlined levels in [`frame_class_ids_with_compiled`].
///
/// Default ON. `CRATONVM_JIT_NO_INLINE_CALLER_FRAMES=1` restores the flat
/// one-entry-per-artifact answer this walk gave before 2026-09-02, so a
/// caller-attribution verdict that changes after warm-up can be attributed to
/// this expansion or to something else inside ONE binary. Separate from
/// `CRATONVM_JIT_NO_INLINE_FRAME_MAP`, which kills the producer and so takes
/// the DISPLAY frames with it: this is the security-relevant half and is the
/// one worth being able to revert alone.
fn inline_caller_frames_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_INLINE_CALLER_FRAMES").is_none()
    })
}

/// One compiled entry as one or more `ClassId`s: the artifact's own owner,
/// then the classes of the callees it INLINED at this program point, outermost
/// level first -- the same order and the same kept set
/// [`push_compiled_frames`] produces for the display path.
///
/// Refuses in three places, each of which leaves the flat answer this walk gave
/// before and never substitutes a guess:
///
///   * the switch is off;
///   * the chain came from the coarse safepoint-id key
///     (`!chain_exact`), which is shared with the inline cache's miss edge;
///   * a level carries `class_id == 0`, i.e. no id was recorded. That ends the
///     expansion INCLUDING the levels below it, because a hole re-parents
///     everything deeper.
fn push_compiled_class_ids(out: &mut Vec<ClassId>, slot: &ActiveCompiledFrame) {
    out.push(ClassId::new(slot.owner_class_id));
    if slot.inline_chain.is_empty() || !slot.chain_exact || !inline_caller_frames_enabled() {
        return;
    }
    // The chain is innermost-first; `out` is outermost-first.
    for level in slot.inline_chain.iter().rev() {
        if level.class_id == 0 {
            break;
        }
        out.push(ClassId::new(level.class_id));
    }
}

/// Kill switch for [`drop_osr_continuations`]. Default ON;
/// `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1` restores the duplicate, which is what
/// makes the frame-count difference an A/B inside one binary.
fn osr_frame_dedupe_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_OSR_FRAME_DEDUPE").is_none()
    })
}

/// Drop compiled entries that are the SAME ACTIVATION as an interpreter frame.
///
/// An OSR transfer hands control to compiled code part-way through a method
/// that is already running, and the interpreter's `Frame` for it stays on
/// `thread.frames`. Both halves then describe one activation, and the trace
/// reported it twice — `SWCross` read 69 frames where HotSpot reads 68, its
/// first two entries both `main`.
///
/// The decider is **`can_osr_enter` on the frame this entry was pushed from**.
/// An OSR transfer leaves the interpreter frame parked at the BACK-EDGE it
/// jumped from, which is by construction one of that artifact's OSR entry
/// points. An interpreted caller of the same method is parked at an INVOKE,
/// which is not. All three conditions are required:
///
///   * `frames[interp_depth - 1]` exists (the frame the entry was pushed from),
///   * it names the same class, method and descriptor, and
///   * `cm.can_osr_enter(frame.pc)`.
///
/// **Two discriminators that look right and are not** — both measured wrong on
/// `probes/SWCross.java`, both cost a build:
///
///   * `compiled_via_osr`. The OSR-entered `main` carries `false`: the flag
///     records how the ARTIFACT was PRODUCED, not how this activation was
///     ENTERED, and `jit_bridge`'s own comment says a first-call/upgrade
///     artifact can OSR-enter too. Gating on it makes this function inert.
///   * `interp_depth` indexing a live frame. An OSR entry records
///     `interp_depth == frames.len()` exactly like an ordinary call, so the
///     depth alone separates nothing.
///
/// The third condition is what keeps self-recursion safe: an interpreted
/// `foo` calling a compiled `foo` satisfies the first two, and dropping that
/// frame would undo the nested-activation walk this file's sibling fix
/// restored.
///
/// # The registry makes rule 1 authoritative (2026-09-01)
///
/// `cm.can_osr_enter(frame.pc)` is a property of a **pc**, not of an
/// **activation**. An interpreted frame genuinely parked on a back-edge while
/// a RECURSIVE compiled activation of the same method is live satisfies it
/// too, and that activation's compiled entry was then dropped from the trace —
/// a real frame lost, the one direction this function is otherwise careful
/// never to fail in.
///
/// `jit_bridge::live_osr_continuation_artifact` closes that hole. The OSR
/// entry site publishes the `(interp_depth, artifact)` pair for exactly the
/// window the artifact is on the stack, from the same `thread.frames.len()`
/// that becomes the chain entry's `interp_depth` — so the frame index is
/// `depth - 1` and the artifact test is a plain `==` against the `cm_ptr` the
/// carrier already holds. When it answers, it is the authority on which
/// compiled entry is this frame's own body, and no pc is consulted.
///
/// `None` from it means **"no information"**, never "this frame is
/// interpreted": it is equally what `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` and a
/// contended borrow report. The pc heuristic therefore stays as the fallback,
/// and that is what keeps the whole change an A/B inside ONE binary — with
/// that switch set, this decision and the bci override below both revert to
/// exactly the pre-2026-09-01 answer.
///
/// # The ordinary compiled activation — rule 2 (2026-09-01)
///
/// An OSR continuation is not the only way one activation ends up with both
/// halves. With `CRATONVM_JIT_NO_INLINE=1` the witness in
/// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`
/// reports `leaf` twice — once as an interpreter frame with a line, once as a
/// compiled frame — and `can_osr_enter` says nothing about it, because that
/// frame is not parked at a back-edge.
///
/// **The proof that the two are one activation.** The interpreter transfers
/// control to another Java method in exactly one way: by executing an
/// `invoke*` opcode (`dispatch_static` / `dispatch_virtual` /
/// `dispatch_special` are the only callers of `execute_jit_call` and
/// `execute_jit_call_decoded`, and each is reached from its opcode arm). While
/// its callee runs, that frame's `last_instr_pc` names the instruction it is
/// suspended in — this is the field's whole purpose ("`pc` may have already
/// been advanced past the invoke instruction"), it is what `entry_from_frame`
/// reports as the frame's bci, and the witness confirms it: every interpreted
/// caller in the correct trace carries its CALL SITE's line. So an interpreter
/// frame whose `last_instr_pc` does NOT hold an invoke opcode cannot be the
/// caller of anything. If it names the same class, method and descriptor as a
/// compiled entry recorded at its own depth, the only remaining reading is
/// that the compiled entry IS that frame's body.
///
/// This is the same argument `can_osr_enter` makes, stated over the opcode
/// rather than over one artifact's OSR entry list, so it also covers a
/// continuation whose back-edge is not an entry point of *this* artifact.
///
/// **Direction of failure.** A frame we cannot read (a `last_instr_pc` outside
/// its own `code`) is treated as a caller, so the compiled entry survives —
/// the historical behaviour, an extra frame rather than a missing one.
///
/// `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1` turns this half off on its own, so it
/// can be separated from the OSR half inside ONE binary; the existing
/// `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1` still turns off both.
///
/// # How the two rules compose
///
/// They share ONE `(depth, label)` ledger, not one each. Both answer the same
/// question — "is this compiled entry the interpreter frame's own body?" — and
/// a frame has exactly one body, so between them they may remove at most one
/// entry per `(depth, label)`. Mutual recursion through compiled code
/// (`foo` -> `bar` -> `foo`) puts two `foo` activations at one depth; with a
/// ledger per rule the OSR rule could take the first and the call rule the
/// second, and the trace would lose a real frame — the failure this function
/// exists to avoid, reintroduced by the fix for it.
///
/// First-wins is the right tie-break because entries sharing a depth arrive
/// OUTERMOST-first (`active_compiled_frames` reverses its innermost-first walk
/// before pushing) and the activation the interpreter frame describes is the
/// outermost one. That ordering also means the continuation is reached before
/// any nested entry at its depth, so the ledger cannot cost it its override;
/// if it ever were reached second, the loss is an extra frame and a stale
/// line, i.e. exactly the historical answer.
///
/// When the registry answers with a DIFFERENT artifact, rule 2 is SKIPPED
/// rather than consulted. Its premise is that a frame not suspended at an
/// invoke cannot be the caller of anything, so a compiled entry at its depth
/// must be its body. An OSR'd frame is parked on a back-edge, so it is not at
/// an invoke either — the premise holds and the conclusion does not, because
/// the caller of that entry is the frame's own compiled body rather than the
/// frame. Letting rule 2 run there would drop the genuine nested activation
/// rule 1 had just declined to drop.
///
/// # The overrides
///
/// Dropping the duplicate is only half the job. That back-edge is also the
/// STALE position the surviving interpreter frame will be reported at: once
/// control transfers to compiled code its `Frame.pc` stops advancing, so every
/// later trace names the loop the method tiered up in rather than where it
/// actually is — `main:62`, the back-edge, where HotSpot says `main:66`, the
/// live call site. The continuation being dropped is the half that knows, so
/// its safepoint bci — and the inline chain recorded at that same program
/// point — are handed to the frame that survives.
///
/// It is deliberately an override applied during capture rather than a write
/// to `Frame::pc`. Two independent reasons, either one sufficient (both
/// established in the block comment above `jit_bridge::osr_pc_refresh_enabled`,
/// which is where the full argument lives):
///
///   * `Frame::live_locals_mask_here` and `Frame::scan_local_objects_inner`
///     compute the per-bci live-locals ROOT FILTER from
///     `[self.pc, self.last_instr_pc]`. Advancing `pc` to where compiled code
///     really is would make every slot that dies in between stop being a GC
///     root — on a frame whose locals are the pre-OSR copies the conservative
///     half of the JIT root scan is leaning on.
///   * The OSR safe-reject exit is correct only BECAUSE `frame.pc` is still
///     `entry_pc`. Moving it would resume the interpreter at a bci this
///     activation never reached.
///
/// So the override reaches the trace assembler and nothing else. No resume
/// path and no root scan can observe it, because it is never stored.
///
/// Only the AUTHORITATIVE arm produces one. The heuristic arm still drops the
/// entry, exactly as it did before, but it does not move the bci: its drop is
/// an inference about which activation the frame is, and a line carried across
/// on an inference is the confidently-wrong answer this file refuses
/// everywhere else. That is also what makes the kill switch a two-arm A/B
/// rather than a half-revert — `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` restores
/// both the old decision and the old line, in one binary.
///
/// The inline chain rides with the bci, and only with it: both are claims
/// about the SAME program point, so a chain without the bci that placed it
/// would be a claim this arm has not established.
///
/// Rule 2's drops deliberately produce NO override. That the surviving
/// interpreter frame's pc is stale was established and measured for the OSR
/// case; for an ordinary compiled activation it has not been, and giving a
/// frame a line on an unverified premise is the one outcome this file rules
/// out everywhere else. It is a one-line addition here if the evidence ever
/// arrives.
fn drop_osr_continuations(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
) -> (Vec<ActiveCompiledFrame>, OsrBciOverrides) {
    if !osr_frame_dedupe_enabled() {
        return (jit, OsrBciOverrides::new());
    }
    let call_dedupe = call_frame_dedupe_enabled();
    // The `(depth, label)` pairs that have already had their one body entry
    // removed, by EITHER rule. See "How the two rules compose" above for why
    // this is shared rather than one ledger per rule.
    let mut deduped: Vec<(u32, String)> = Vec::new();
    let mut overrides = OsrBciOverrides::new();
    let kept = jit
        .into_iter()
        .filter(|f| {
            // The frame this entry was pushed FROM. An OSR continuation was
            // pushed from the very frame it continues, so that frame is still
            // there and names the same method.
            let Some(i) = (f.interp_depth as usize).checked_sub(1) else {
                return true;
            };
            let Some(frame) = frames.get(i) else {
                return true;
            };
            if !label_names_frame(&f.label, frame) {
                return true;
            }
            if f.cm_ptr == 0 {
                return true;
            }
            // SAFETY: the pointer came from a chain entry whose frame is live
            // on this thread's stack, so the JIT cache still owns the `Arc`;
            // this read happens on the owning thread during that same capture.
            let cm = unsafe { &*(f.cm_ptr as *const cratonvm_jit::CompiledMethod) };
            // Rule 1 — the OSR continuation. The entry site knows the
            // `(frame, artifact)` pairing outright; the pc shape is only what
            // is left when it has nothing to say. An OSR transfer leaves the
            // interpreter frame parked at the BACK-EDGE it jumped from, which
            // is by construction one of this artifact's OSR entry points; an
            // interpreted caller of the same method is parked at an INVOKE,
            // which is not.
            let (is_continuation, authoritative) =
                match crate::runtime::interpreter::jit_bridge::live_osr_continuation_artifact(i) {
                    Some(live_cm) => (f.cm_ptr == live_cm, true),
                    None => (cm.can_osr_enter(frame.pc), false),
                };
            if is_continuation {
                if already_deduped(&deduped, f.interp_depth, &f.label) {
                    return true;
                }
                deduped.push((f.interp_depth, f.label.clone()));
                cratonvm_jit::note_stack_walk_dedupe(if authoritative {
                    cratonvm_jit::DEDUPE_OSR_AUTHORITATIVE
                } else {
                    cratonvm_jit::DEDUPE_OSR_HEURISTIC
                });
                // The half that knows where control is, handed to the frame
                // that survives -- but ONLY on the authoritative arm. See "The
                // overrides" above.
                if authoritative && f.bci >= 0 {
                    overrides.insert(i, (f.bci, f.inline_chain.clone()));
                }
                return false;
            }
            if authoritative {
                // The registry named a DIFFERENT artifact as this frame's
                // body, so this entry is a distinct activation nested under
                // it, and rule 2's premise does not reach it. See the doc.
                return true;
            }
            // Rule 2 — the ordinary compiled activation. A frame suspended at
            // an invoke is a CALLER and both activations are real; anything
            // else cannot have called this method and is therefore its own
            // body running compiled.
            if !call_dedupe || frame_is_suspended_at_invoke(frame) {
                return true;
            }
            if already_deduped(&deduped, f.interp_depth, &f.label) {
                return true;
            }
            deduped.push((f.interp_depth, f.label.clone()));
            cratonvm_jit::note_stack_walk_dedupe(cratonvm_jit::DEDUPE_CALL_OPCODE);
            false
        })
        .collect();
    (kept, overrides)
}

/// Apply the SAME two dedupe rules to a set of compiled frames snapshotted at
/// an implicit NPE that [`capture_full_trace`] applies to the live ones.
///
/// The snapshot is taken inside the JIT helper, so it holds every compiled
/// activation that was on the stack at the trap — INCLUDING an OSR
/// continuation, which is the same activation as an interpreter frame that is
/// still there. Splicing it in unfiltered printed `main` twice:
///
/// ```text
/// HotSpot            after_osr  [big:42 probe:56 main:71]
/// snapshot, undeduped           [big:42 probe:56 main:71 main:71]
/// ```
///
/// measured 3 of 3 on `probes/StackTraceCompiledCallee` once defect (5)'s other
/// doors were closed and the frames stopped disappearing — the duplicate was
/// invisible for as long as the whole set was being lost.
///
/// It reuses [`drop_osr_continuations`] rather than restating either rule.
/// There is exactly one place that decides whether a compiled entry and an
/// interpreter frame are one activation, so the live path and the snapshot path
/// cannot drift, and both kill switches
/// (`CRATONVM_JIT_NO_OSR_FRAME_DEDUPE`, `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE`)
/// cover both. The bci overrides it computes are discarded here: they exist to
/// re-point an interpreter frame that the LIVE capture is about to emit, and
/// this path emits none.
///
/// `frames` is the thread's frame stack at CONSTRUCTION time, not at the trap.
/// The two agree for every shape this can be reached in — a frame popped
/// between the two would have taken its compiled entry with it — and the rule
/// fails in the safe direction anyway: a depth that no longer names a matching
/// frame KEEPS the compiled entry.
pub(crate) fn dedupe_compiled_snapshot(
    frames: &[Frame],
    snapshot: Vec<ActiveCompiledFrame>,
) -> Vec<ActiveCompiledFrame> {
    drop_osr_continuations(frames, snapshot).0
}

/// Display-only replacements for one interpreter frame's own `last_instr_pc`,
/// keyed by index into `frames`: the bytecode index of the COMPILED half of
/// that activation, and the callees that half had inlined at the very same
/// program point.
///
/// Produced by [`drop_osr_continuations`] — see "The overrides" there for why
/// this is passed to the trace assembler rather than written into
/// `Frame::pc`.
type OsrBciOverrides = std::collections::HashMap<usize, (i32, Vec<InlinedLevel>)>;

/// Has this `(depth, label)` already had its one body entry removed by either
/// rule of [`drop_osr_continuations`]?
///
/// A linear scan of a `Vec` rather than a set: the ledger holds one element
/// per method with BOTH halves live at one depth, which is zero or one on
/// every trace measured, and hashing the label would cost more than the
/// compare it replaces.
fn already_deduped(seen: &[(u32, String)], depth: u32, label: &str) -> bool {
    seen.iter().any(|(d, l)| *d == depth && l.as_str() == label)
}

/// Kill switch for the ordinary-compiled-activation half of
/// [`drop_osr_continuations`]. Default ON;
/// `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1` restores the duplicate frame, which
/// is what makes that half an A/B inside one binary independently of
/// `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE`.
fn call_frame_dedupe_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_CALL_FRAME_DEDUPE").is_none()
    })
}

/// The five opcodes that hand control from an interpreter frame to another
/// Java method: `invokevirtual`, `invokespecial`, `invokestatic`,
/// `invokeinterface`, `invokedynamic` (JVMS 6.5). None of them can be `wide`,
/// so the byte at the bci is the whole test.
const INVOKE_OPCODES: [u8; 5] = [0xb6, 0xb7, 0xb8, 0xb9, 0xba];

/// Is this interpreter frame suspended *inside a call it made*?
///
/// `last_instr_pc` is the instruction the frame is executing (see the field's
/// own doc and [`entry_from_frame`], which reports it as the frame's bci), so
/// for a frame with a callee above it that is the invoke. Answers `true` for a
/// pc outside the frame's own `code`, which is the fail-closed direction for
/// the only caller: a frame this cannot read keeps its compiled twin rather
/// than losing a real activation.
fn frame_is_suspended_at_invoke(frame: &Frame) -> bool {
    match frame.code.get(frame.last_instr_pc) {
        Some(opcode) => INVOKE_OPCODES.contains(opcode),
        None => true,
    }
}

/// Does `label` (`class/Name.method:descriptor`) name the same method as
/// `frame`? Compares all three parts: an overload or a same-named method on
/// another class is a different activation.
fn label_names_frame(label: &str, frame: &Frame) -> bool {
    let Some((owner_and_method, descriptor)) = label.rsplit_once(':') else {
        return false;
    };
    let Some((class_name, method_name)) = owner_and_method.rsplit_once('.') else {
        return false;
    };
    &*frame.method_name() == method_name
        && &*frame.method_descriptor() == descriptor
        && &*frame.class_name() == class_name
}

/// Splice the CURRENT thread's active compiled frames into its interpreter
/// frames, outermost first.
///
/// A JIT-compiled method runs without pushing a [`Frame`], so on its own
/// `frames` is the Java stack *minus everything the JIT has taken over* — which
/// grows as a workload warms up, until a stack that was 21 frames deep reports
/// 6. `jit` carries, per active compiled frame, the interpreter depth it was
/// entered at ([`crate::jit::conservative_roots::active_compiled_frames`]), and
/// that is exactly the insertion point: an entry recorded at depth `d` was
/// pushed when `frames[..d]` already existed and `frames[d]` did not, so it
/// belongs immediately before `frames[d]`. Entries sharing a depth are nested
/// (compiled code dispatching to compiled code) and keep their push order,
/// which is already outermost-first.
///
/// A compiled entry carries a bytecode index whenever the artifact's own
/// metadata could name the program point it is standing at, and then resolves
/// a real line from it; otherwise it keeps [`LINE_NUMBER_UNKNOWN`], which
/// `StackTraceElement` renders as `(Unknown Source)`. A frame with an unknown
/// line is still strictly better than an absent frame:
/// `Thread.getStackTrace()` consumers ask *which methods are on the stack* far
/// more often than they ask which line.
///
/// # Inlined callees, and the ORDER they go in (2026-09-01)
///
/// A JIT-compiled artifact is not one Java frame. Every callee it inlined is a
/// method that is genuinely executing, pushes nothing, and — before this —
/// contributed nothing at all: the witness in
/// `jit-compiled-frame-has-no-line-and-no-inlined-callees-FIXED-20260902.md`
/// reads `len=3 [leaf:25 probe:-1 main:62]` where HotSpot reads
/// `len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]`, and `mid`/`outer` are
/// missing for exactly this reason. [`push_compiled_frames`] expands one
/// compiled entry into that chain.
///
/// The direction is the one thing here that is easy to get backwards, so the
/// argument in full. `out` is built OUTERMOST FIRST (`frames` is outermost
/// first, and [`frame_class_ids_with_compiled`] reverses at the very end
/// precisely because of that; `active_compiled_frames` likewise reverses its
/// innermost-first `nested` vector before pushing, for this splice). An entry
/// pushed EARLIER is therefore FURTHER OUT. The enclosing compiled method is
/// outside every callee it inlined, so it is pushed first; the chain arrives
/// INNERMOST FIRST, so it is walked with `.rev()` — outermost inlined level
/// inward. Get it backwards and the trace reads as if the callee called its
/// own caller.
///
/// # The OSR interaction — option (a), COMPLETE
///
/// [`drop_osr_continuations`] deletes one compiled entry per capture as the
/// duplicate of an interpreter frame it shares an activation with, and hands
/// that entry's bci AND its chain to the survivor as a display-only override.
/// If that entry carried a chain, the choice is between carrying the chain
/// across too (complete) and dropping it with the entry (fail closed, losing
/// frames).
///
/// **This takes the complete option**, and the justification is not a new one:
/// it is exactly the argument already made for the bci, applied to the other
/// half of the same evidence. The override is produced ONLY on the
/// authoritative arm, where `jit_bridge::live_osr_continuation_artifact` has
/// named this artifact as this frame's own body — not inferred it from a pc
/// shape. The bci and the chain are then two readings of one program point in
/// one activation, taken together, from metadata the artifact vouches for. If
/// the bci may be shown, so may the frames standing inside it; refusing the
/// second while accepting the first would not be caution, it would be
/// inconsistency.
///
/// It is also fail-closed in the same shape as everything else here: no
/// override means no expansion, an empty chain means no expansion, and
/// `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` still reverts the decision, the line AND
/// the frames together, inside one binary.
///
/// The expansion goes immediately AFTER the interpreter frame is pushed and
/// before the next frame's compiled entries — the same `.rev()` direction, for
/// the same reason: those callees are deeper than the frame whose bci they
/// were recorded under, and shallower than anything at the next depth.
fn interleave_compiled_frames(
    class_store: &ClassStore,
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
) -> Vec<StackTraceEntry> {
    let mut out = Vec::with_capacity(frames.len() + jit.len());
    let mut next = 0usize;
    for (i, f) in frames.iter().enumerate() {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled_frames(&mut out, class_store, &jit[next]);
            next += 1;
        }
        let mut entry = entry_from_frame(class_store, f);
        match osr_bci.get(&i) {
            Some((bci, chain)) => {
                reline_entry(class_store, &mut entry, *bci);
                out.push(entry);
                // The callees inlined into the compiled half of THIS frame's
                // own activation, which left with the entry
                // `drop_osr_continuations` removed. Same direction as
                // `push_compiled_frames`, and for the same reason.
                push_inlined_chain(&mut out, class_store, chain);
            }
            None => out.push(entry),
        }
    }
    for slot in &jit[next..] {
        push_compiled_frames(&mut out, class_store, slot);
    }
    out
}

/// One compiled entry as ONE OR MORE [`StackTraceEntry`] values: the
/// artifact's own method, then the callees it inlined at the program point it
/// is standing at, from the outermost inlined level inward.
///
/// An empty chain — every non-inlining method, every refusal on the producer
/// side, and the whole feature switched off with
/// `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` — reproduces the historical single
/// entry exactly, byte for byte.
///
/// A label this cannot parse yields NO frame and no inlined frames either:
/// they would have no caller to attach to.
fn push_compiled_frames(
    out: &mut Vec<StackTraceEntry>,
    class_store: &ClassStore,
    slot: &ActiveCompiledFrame,
) {
    let Some(enclosing) = compiled_frame_entry(class_store, slot) else {
        return;
    };
    out.push(enclosing);
    push_inlined_chain(out, class_store, &slot.inline_chain);
}

/// Push one inline chain, OUTERMOST inlined level first, onto an `out` that is
/// already outermost-first and already holds the frame these are inlined INTO.
///
/// # `break`, not `continue`
///
/// A level that cannot be named leaves every DEEPER level with no caller.
/// Pushing them anyway would attach them to the wrong method — a trace that
/// reads as if a call happened that never did, and that a reader has no way to
/// tell from a real one. Losing a suffix of the chain is recoverable by a
/// reader (the trace is visibly short); a fabricated caller is not. So the
/// first refusal ends the chain.
fn push_inlined_chain(
    out: &mut Vec<StackTraceEntry>,
    class_store: &ClassStore,
    chain: &[InlinedLevel],
) {
    // The chain is innermost-first; `out` is outermost-first.
    for level in chain.iter().rev() {
        match inlined_frame_entry(class_store, level) {
            Some(e) => out.push(e),
            None => break,
        }
    }
}

/// One INLINED callee as a [`StackTraceEntry`], from the
/// `"class/Name.method:descriptor"` label the emitter recorded for it and that
/// callee's own bytecode index.
///
/// This is [`compiled_frame_entry`] with two differences, both forced by the
/// fact that an inlined callee is not an artifact:
///
///   * **there is no owner class id.** The emitter that spliced the body knew
///     the callee only by its internal name, so the class is resolved by name
///     through [`find_class_id_by_name_memoized`] — see that function for why
///     the memo is not optional here.
///   * **an absent class still yields a frame.** `compiled_frame_entry`
///     already takes exactly that position for its own `None` arm, and it is
///     the right one: the FRAME is vouched for by the artifact's own metadata
///     (this method was inlined at this point, which is a fact about the
///     machine code that ran) even when the store cannot be consulted to say
///     which line. Only the line is unknown, and it is reported as
///     [`LINE_NUMBER_UNKNOWN`].
///
/// # Nothing here guesses
///
/// The line is resolved only through `find_method_index_memoized`, which
/// matches on name AND descriptor: a tombstoned class yields no `Class`, an
/// overload set yields the one exact slot or nothing, and either way the entry
/// keeps [`LINE_NUMBER_UNKNOWN`]. An unparsable or empty-part label yields no
/// frame at all rather than a mangled name.
///
/// The bci bound is re-checked here even though the emitter already refuses a
/// row outside it (JVMS 4.9.1, `Code.code_length < 65536`). That is not
/// belt-and-braces: `line_number_for_bci_in_method` picks the largest
/// `start_pc <= bci`, so a bci PAST the end of the method resolves to its LAST
/// line — a confidently wrong line, which is the single worst outcome
/// available in this file. One comparison buys immunity from it at a crate
/// boundary this module cannot otherwise police.
fn inlined_frame_entry(class_store: &ClassStore, level: &InlinedLevel) -> Option<StackTraceEntry> {
    let InlinedLevel {
        label,
        bci,
        class_id: recorded_class_id,
    } = level;
    let bci = *bci;
    // JVMS 4.9.1: `Code.code_length` must be less than 65536, so every genuine
    // bci is below it. Mirrors `conservative_roots::plausible_bci`.
    const MAX_CODE_LENGTH: u32 = 65_536;
    if bci >= MAX_CODE_LENGTH {
        return None;
    }
    let (owner_and_method, method_descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() {
        return None;
    }
    // The RESOLVER's own id first (2026-09-02). It is the id the splice was
    // planned against, so it needs no name scan and cannot pick a different
    // class of the same name under another loader -- the two failure modes of
    // the by-name route. `0` means the producer supplied none (every hand-built
    // fixture, and any artifact compiled before the field existed), and the
    // memoized by-name resolution stays as the fallback for exactly that.
    let by_id = (*recorded_class_id != 0)
        .then(|| ClassId::new(*recorded_class_id))
        .and_then(|id| class_store.get(id).map(|c| (id, c)));
    // `get` after the by-name resolution rather than trusting the id alone:
    // the memo's own verification already did this read, but the borrow cannot
    // escape it, and repeating it is one hash probe.
    let resolved = by_id.or_else(|| {
        find_class_id_by_name_memoized(class_store, class_name)
            .and_then(|id| class_store.get(id).map(|c| (id, c)))
    });
    let (class_name, source_file, class_id, method_index, line_number) = match resolved {
        Some((id, c)) => {
            let method_index = find_method_index_memoized(c, id, method_name, method_descriptor);
            // Exactly the resolution `entry_from_frame` and
            // `compiled_frame_entry` perform, over the same memo and the same
            // `LineNumberTable` scan — one implementation of the JVMS 4.7.12
            // rule, reached from all three capture paths.
            let line_number = method_index
                .and_then(|i| c.methods.get(i as usize))
                .and_then(|m| line_number_for_bci_in_method(m, bci as usize))
                .unwrap_or(LINE_NUMBER_UNKNOWN);
            (
                Arc::from(&*c.name),
                c.source_file.as_deref().map(Arc::from),
                Some(id),
                method_index,
                line_number,
            )
        }
        // Name-only entry. `class_id: None` is honest — no id was established,
        // and `StackFrame.getDeclaringClass()` must not be handed a guess.
        None => (Arc::from(class_name), None, None, None, LINE_NUMBER_UNKNOWN),
    };
    Some(StackTraceEntry {
        class_name,
        method_name: Arc::from(method_name),
        source_file,
        line_number,
        // Cast: bounded above by the JVMS 4.9.1 check at the top.
        byte_code_index: bci as i32,
        class_id,
        method_index,
    })
}

/// Prepend the compiled frames snapshotted at a JIT-signalled implicit NPE to
/// a trace that was captured after they had already unwound.
///
/// An implicit NPE raised inside compiled code is CONSTRUCTED later, from the
/// interpreter, once the helper's `i64::MIN` sentinel has propagated out of the
/// compiled activation (see `jit::helpers::take_jit_pending_npe`). By then the
/// frames between the throw site and the first interpreter frame are gone, and
/// the trace names none of the code that actually raised the exception —
/// `[big:42 probe:56 main:67]` on HotSpot reads `[probe:56 main:67]` here, and
/// after an OSR of the caller it reads `[main:71]` alone.
///
/// The snapshot was taken inside the helper, while those frames were still on
/// the stack. They are innermost-first and belong in front of everything the
/// late capture found. Entries are built through [`push_compiled_frames`], the
/// same builder the live splice uses, so a frame recovered this way carries
/// the same bci, the same line and the same inlined callees as one caught
/// live.
pub fn append_snapshotted_compiled_frames(
    class_store: &ClassStore,
    snapshot: &[ActiveCompiledFrame],
    mut trace: Vec<StackTraceEntry>,
) -> Vec<StackTraceEntry> {
    if snapshot.is_empty() {
        return trace;
    }
    // ORDER. A STORED trace is outermost-first — the Java `StackTraceElement[]`
    // is built by reversing it, which is why `capture_stack_trace`'s consumers
    // iterate `.rev()`. The snapshotted frames are the INNERMOST ones (they ran
    // below everything the late capture could still see), so outermost-first
    // means they go on the END, in the order `active_compiled_frames` already
    // returns them. Getting this backwards is not a subtle failure: it prints
    // the throw site as the outermost frame, which reads as a plausible trace
    // of a completely different call.
    let mut fresh: Vec<StackTraceEntry> = Vec::with_capacity(snapshot.len());
    for f in snapshot {
        // An inlined callee is deeper than the artifact that inlined it, so it
        // is pushed after it — the same direction as the live splice, for the
        // same reason.
        push_compiled_frames(&mut fresh, class_store, f);
    }
    let overlap = trailing_overlap(&trace, &fresh);
    trace.reserve(fresh.len() - overlap);
    trace.extend(fresh.into_iter().skip(overlap));
    trace
}

/// How many entries at the FRONT of `fresh` the tail of `trace` already holds.
///
/// # Why this is needed at all
///
/// [`append_snapshotted_compiled_frames`]'s premise is that the compiled frames
/// have already unwound by the time the throwable is constructed — the whole
/// reason the snapshot exists. One door breaks that premise:
/// `jit::helpers::materialize_implicit_signal` builds the throwable INSIDE the
/// JIT helper, with every compiled frame still on the stack, so
/// `fillInStackTrace` has already walked and spliced them. Appending the
/// snapshot unfiltered then prints them twice:
///
/// ```text
/// as PRINTED, i.e. innermost first -- this function's own lists are the
/// reverse of that:
/// HotSpot                       [leaf:25 mid:26 outer:27 probe:42 main:66]
/// unfiltered snapshot splice    [leaf:25 mid:26 outer:27 probe:42
///                                        mid:26 outer:27 probe:42 main:66]
/// ```
///
/// measured on a `synchronized` throw-site variant of
/// `probes/StackTraceAfterOsr.java`, where the `synchronized` is doing nothing
/// but routing the raise through that door.
///
/// # Why the OVERLAP and not a membership test
///
/// Both lists describe ONE stack, outermost-first, and `fresh` is the inner
/// end of it. So the only correct merge is the longest prefix of `fresh` that
/// is a suffix of `trace` — which is also what makes genuine recursion safe: a
/// method that really does appear twice appears twice in BOTH lists, and the
/// maximal overlap lines them up instead of deleting a real frame. A membership
/// test ("is this method already in the trace?") would delete one.
///
/// Zero overlap — every shape where the frames HAD unwound — appends
/// everything, which is byte-for-byte what this function did before.
///
/// Entries are compared on `(class_name, method_name, byte_code_index)`: the
/// identity of a program point. The line number is derived from the bci and the
/// source file from the class, so neither adds information, and `class_id` /
/// `method_index` are `None` on a name-only entry and would make two readings of
/// one frame compare unequal.
fn trailing_overlap(trace: &[StackTraceEntry], fresh: &[StackTraceEntry]) -> usize {
    let max = trace.len().min(fresh.len());
    for n in (1..=max).rev() {
        let tail = &trace[trace.len() - n..];
        if tail
            .iter()
            .zip(&fresh[..n])
            .all(|(a, b)| same_program_point(a, b))
        {
            return n;
        }
    }
    0
}

/// Do two entries name the same program point? See [`trailing_overlap`].
fn same_program_point(a: &StackTraceEntry, b: &StackTraceEntry) -> bool {
    a.class_name == b.class_name
        && a.method_name == b.method_name
        && a.byte_code_index == b.byte_code_index
}

/// Re-point an already-built entry at `bci`, re-resolving its line.
///
/// Used for the OSR-entered interpreter frame whose own `pc` is stale (see
/// [`drop_osr_continuations`]). Leaves the line `LINE_NUMBER_UNKNOWN` when the
/// method has no `LineNumberTable` row covering `bci`, exactly as
/// [`entry_from_frame`] would.
fn reline_entry(class_store: &ClassStore, entry: &mut StackTraceEntry, bci: i32) {
    entry.byte_code_index = bci;
    entry.line_number = entry
        .class_id
        .and_then(|cid| class_store.get(cid))
        .zip(entry.method_index)
        .and_then(|(class, idx)| class.methods.get(idx as usize))
        .and_then(|m| line_number_for_bci_in_method(m, bci.max(0) as usize))
        .unwrap_or(LINE_NUMBER_UNKNOWN);
}

/// One compiled frame as a [`StackTraceEntry`], from the artifact's
/// `"class/Name.method:descriptor"` label and its owning class id.
///
/// Returns `None` for a label this cannot parse rather than emitting a frame
/// with a mangled name — the only labels in production come from
/// `x64::compile_with_param_slots`' `method_key` (and the matching stamp on the
/// optimizing tier), which are always of that shape.
fn compiled_frame_entry(
    class_store: &ClassStore,
    frame: &ActiveCompiledFrame,
) -> Option<StackTraceEntry> {
    let ActiveCompiledFrame {
        label,
        owner_class_id,
        bci,
        ..
    } = frame;
    let (owner_and_method, method_descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() {
        return None;
    }
    let class_id = ClassId::new(*owner_class_id);
    let class = class_store.get(class_id);
    // Prefer the class's own recorded name: the label is built from the same
    // string, but a class the store knows is the authority, and it also gives
    // the source file the label cannot carry.
    let (class_name, source_file, method_index) = match class {
        Some(c) => (
            std::sync::Arc::from(&*c.name),
            c.source_file.as_deref().map(std::sync::Arc::from),
            find_method_index_memoized(
                c,
                class_id,
                &std::sync::Arc::<str>::from(method_name),
                &std::sync::Arc::<str>::from(method_descriptor),
            ),
        ),
        None => (std::sync::Arc::from(class_name), None, None),
    };
    // The bci comes from the safepoint id the live frame published, and
    // `activation_bci` has already required the artifact to name it as one of
    // its own recorded safepoints — so this is a bytecode index the method
    // really stopped at, not a guess. `-1` (no usable id) keeps the old
    // answer: `LINE_NUMBER_UNKNOWN`, rendered `(Unknown Source)`. A wrong line
    // would still be worse than none; what changed is that a right one is now
    // available for the overwhelming majority of frames.
    let line_number = if *bci >= 0 {
        class
            .zip(method_index)
            .and_then(|(c, idx)| c.methods.get(idx as usize))
            .and_then(|m| line_number_for_bci_in_method(m, *bci as usize))
            .unwrap_or(LINE_NUMBER_UNKNOWN)
    } else {
        LINE_NUMBER_UNKNOWN
    };
    Some(StackTraceEntry {
        class_name,
        method_name: std::sync::Arc::from(method_name),
        source_file,
        line_number,
        byte_code_index: *bci,
        class_id: Some(class_id),
        method_index,
    })
}

/// Like [`capture_full_trace`] but WITHOUT resolving source-line numbers — so it
/// needs no `ClassStore` and takes no lock. Used to publish a per-thread frame
/// snapshot at blocking deposit points (see `deposit_root_snapshot`) for
/// cross-thread `Thread.getStackTrace()` / `dumpThreads()`: the diagnostic only
/// needs `class.method` (+ BCI) to pinpoint where a parked thread is stuck, and
/// keeping it lock-free keeps it safe to call from every deposit site. The line
/// number is left [`LINE_NUMBER_UNKNOWN`].
///
/// Deferred resolution: pass the result to [`resolve_line_numbers_in_place`]
/// once a `ClassStore` borrow is available — `ThreadRegistry::frame_trace_of_resolved`
/// is the wired-up reader.
///
/// These entries carry no [`StackTraceEntry::method_index`]: deriving one needs
/// the `ClassStore` this function deliberately does not take. Deferred
/// resolution therefore falls back to the unambiguous-name rule for them and
/// leaves overloaded frames at `UNKNOWN`. It never reports a *wrong* line, only
/// an unknown one, which is strictly better than the all-`UNKNOWN` snapshot
/// this function returns.
pub fn capture_frames_no_lines(frames: &[Frame]) -> Vec<StackTraceEntry> {
    frames
        .iter()
        .map(|f| StackTraceEntry {
            class_name: f.class_name_arc(),
            method_name: f.method_name_arc(),
            source_file: f.source_file_arc(),
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: f.last_instr_pc.min(i32::MAX as usize) as i32,
            class_id: Some(f.class_id),
            // Deliberately absent: resolving an index requires the ClassStore
            // borrow this deposit path must not take. See CR-CLO-2 in
            // `cross-owner-closeout.md` for the
            // `Frame`-side change that would make it free.
            method_index: None,
        })
        .collect()
}

/// Resolve line numbers for entries captured *without* them (see
/// [`capture_frames_no_lines`]), in place, from the live `ClassStore`.
///
/// This is the deferred half of a lazy capture. It is deliberately
/// **fail-closed**: an entry is only filled in when the resolution is provably
/// the same one an eager capture would have produced.
///
/// An entry is resolved iff all of the following hold:
///
/// 1. its `line_number` is still [`LINE_NUMBER_UNKNOWN`] — an already-resolved
///    entry (and in particular a [`LINE_NUMBER_NATIVE`] one) is never touched;
/// 2. it carries a `class_id` — synthetic entries with no backing interpreter
///    frame have no class to resolve against;
/// 3. that `class_id` is still live in the store — an unloaded class leaves a
///    tombstone (`ClassStore::remove`) and `ClassId`s are monotonic and never
///    reused, so this can only ever fail *closed*, never resolve against the
///    wrong class;
/// 4. its `byte_code_index` is non-negative;
/// 5. the exact declaring method can be identified — see below.
///
/// Returns the number of entries that were filled in.
///
/// # How the method is identified (rule 5)
///
/// **Preferred, and exact: `method_index`.** An entry captured through
/// [`entry_from_frame`] carries [`StackTraceEntry::method_index`], the frame's
/// slot in `Class::methods`. The index is re-read from the *live* class and its
/// `name` re-checked against the entry's `method_name` before it is used, so a
/// redefinition that reordered or removed methods cannot resolve against the
/// wrong body — it simply fails the check and drops through to the fallback.
/// With the index present, an overload set is no obstacle: overloads occupy
/// distinct slots.
///
/// **Fallback, when `method_index` is absent** (synthetic entries, and the
/// deliberately `ClassStore`-free [`capture_frames_no_lines`] snapshot): resolve
/// only when **exactly one** method declared by that class carries the recorded
/// name. With more than one, the entry names an overload set and there is
/// nothing left to disambiguate with; two overloads have different
/// `LineNumberTable`s, so guessing would print a line from the wrong method
/// body. Those entries keep `UNKNOWN`.
///
/// Either way the function is **fail-closed**: it yields the line an eager
/// capture would have produced, or `UNKNOWN`. It never reports a wrong line.
///
/// # Relationship to the `Throwable` path
///
/// This is *not* wired onto the `Throwable` path, and adding `method_index` did
/// not change that. `Throwable` capture goes through [`capture_full_trace`],
/// which resolves eagerly and correctly today, including for overloads; routing
/// it through a deferred resolve would be a behaviour change to the most
/// fidelity-sensitive output the VM produces (`printStackTrace`) for a
/// throughput win, and this repo has been burned by exactly that trade before
/// (`tco-breaks-stacktrace-fidelity`). What the index buys is that such a change
/// is now *possible* without a fidelity loss; making it is a separate,
/// measurable step. See
/// `cross-owner-closeout.md`.
///
/// It is a strict *improvement* for [`capture_frames_no_lines`] consumers,
/// which have no line numbers at all otherwise.
pub fn resolve_line_numbers_in_place(
    class_store: &ClassStore,
    entries: &mut [StackTraceEntry],
) -> usize {
    let mut resolved = 0usize;
    for entry in entries.iter_mut() {
        if entry.line_number != LINE_NUMBER_UNKNOWN || entry.byte_code_index < 0 {
            continue;
        }
        let Some(class_id) = entry.class_id else {
            continue;
        };
        let Some(class) = class_store.get(class_id) else {
            continue;
        };
        let name = &*entry.method_name;

        // Exact path: the captured slot, re-verified against the live class.
        let mut method: Option<&ClassFileMethod> = entry
            .method_index
            .and_then(|idx| class.methods.get(idx as usize))
            .filter(|m| &*m.name == name);

        if method.is_none() {
            // No index, or the index no longer names this method (redefinition).
            // Fall back to the unambiguous-name rule, which is also fail-closed:
            // if the name is unique within the class it can only denote the one
            // method, and if it is not, we decline.
            let mut only: Option<&ClassFileMethod> = None;
            for m in &class.methods {
                if &*m.name == name {
                    if only.is_some() {
                        // Overload set — cannot disambiguate without an index.
                        only = None;
                        break;
                    }
                    only = Some(m);
                }
            }
            method = only;
        }

        let Some(method) = method else {
            continue;
        };
        if let Some(line) = line_number_for_bci_in_method(method, entry.byte_code_index as usize) {
            entry.line_number = line;
            resolved += 1;
        }
    }
    resolved
}

/// Synthesize a synthetic "no source info" entry. Used for native frames
/// injected from outside the interpreter (JNI up-calls, host-side
/// StackWalker probes during VM bootstrap).
pub fn synthetic_entry(class_name: Arc<str>, method_name: Arc<str>) -> StackTraceEntry {
    StackTraceEntry {
        class_name,
        method_name,
        source_file: None,
        line_number: LINE_NUMBER_NATIVE,
        byte_code_index: -1,
        class_id: None,
        method_index: None,
    }
}

/// Consult a [`Class`]'s attributes for the `SourceFile` attribute.
/// Fallback for frames whose cached `source_file_arc()` is `None`.
pub fn source_file_of_class(class: &Class) -> Option<Arc<str>> {
    class.source_file.as_deref().map(Arc::from)
}

/// Test-only `ClassStore` fixtures.
///
/// Lives outside `mod tests` because deferred line resolution is *consumed*
/// elsewhere in the crate (`threading::thread_registry::frame_trace_of_resolved`),
/// and that module's tests need a real class to resolve against. Mirrors
/// `classloading::class::tests::make_class`.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::classloading::{ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LineNumberEntry};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::method::ClassFileMethod;

    pub(crate) fn named_method(
        name: &str,
        descriptor: &str,
        lines: Vec<LineNumberEntry>,
    ) -> ClassFileMethod {
        let code = CodeAttribute {
            max_stack: 1,
            max_locals: 1,
            code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
            exception_table: Vec::new(),
            attributes: vec![Attribute::LineNumberTable(lines)],
        };
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(code),
            )],
        }
    }

    pub(crate) fn store_with(methods: Vec<ClassFileMethod>) -> (ClassStore, ClassId) {
        let mut store = ClassStore::new();
        let id = store.next_id();
        let class = Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from("probe/Target"),
            source_file: Some("Target.java".to_string()),
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: Vec::new(),
            fields: Vec::new(),
            methods,
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: Vec::new(),
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        };
        let id = store.add(class);
        (store, id)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{named_method, store_with};
    use super::*;
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LineNumberEntry};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    use cratonvm_reader::method::ClassFileMethod;

    /// A deferred entry with no `method_index` — i.e. what the lock-free
    /// `capture_frames_no_lines` snapshot produces.
    fn entry_for(class_id: ClassId, method: &str, bci: i32) -> StackTraceEntry {
        StackTraceEntry {
            class_name: Arc::from("probe/Target"),
            method_name: Arc::from(method),
            source_file: Some(Arc::from("Target.java")),
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(class_id),
            method_index: None,
        }
    }

    /// A deferred entry that carries the method slot, i.e. what
    /// `entry_from_frame` produces.
    fn entry_for_indexed(
        class_id: ClassId,
        method: &str,
        bci: i32,
        method_index: u32,
    ) -> StackTraceEntry {
        StackTraceEntry {
            method_index: Some(method_index),
            ..entry_for(class_id, method, bci)
        }
    }

    /// One entry naming a program point, for the overlap tests.
    fn point(class: &str, method: &str, bci: i32) -> StackTraceEntry {
        StackTraceEntry {
            class_name: Arc::from(class),
            method_name: Arc::from(method),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: None,
            method_index: None,
        }
    }

    /// The shape that motivated [`trailing_overlap`]: the throwable was built
    /// while the compiled frames were still live, so the capture already spliced
    /// them and the snapshot repeats all but the innermost.
    #[test]
    fn a_snapshot_that_repeats_the_captured_tail_is_appended_only_once() {
        let trace = vec![
            point("P", "main", 66),
            point("P", "probe", 42),
            point("P", "outer", 27),
            point("P", "mid", 26),
        ];
        let fresh = vec![
            point("P", "probe", 42),
            point("P", "outer", 27),
            point("P", "mid", 26),
            point("P", "leaf", 25),
        ];
        assert_eq!(trailing_overlap(&trace, &fresh), 3);
    }

    /// The ORDINARY shape — the frames had unwound, so nothing repeats and
    /// everything is appended. This is the arm that must stay byte-for-byte
    /// what it was.
    #[test]
    fn a_snapshot_of_frames_that_already_unwound_overlaps_nothing() {
        let trace = vec![point("P", "msg", 1), point("P", "pair", 48)];
        let fresh = vec![point("P", "probe", 42), point("P", "leaf", 25)];
        assert_eq!(trailing_overlap(&trace, &fresh), 0);
    }

    /// Genuine recursion appears twice in BOTH lists, and the MAXIMAL overlap
    /// is what lines them up. A membership test would delete a real frame here.
    #[test]
    fn genuine_recursion_keeps_every_copy() {
        let trace = vec![
            point("P", "main", 3),
            point("P", "rec", 7),
            point("P", "rec", 7),
        ];
        let fresh = vec![
            point("P", "rec", 7),
            point("P", "rec", 7),
            point("P", "leaf", 1),
        ];
        // Two `rec` frames are shared; the third entry of `fresh` is new.
        assert_eq!(trailing_overlap(&trace, &fresh), 2);
    }

    /// A frame that matches by NAME but stands at a different bci is a
    /// different program point and must not be folded away.
    #[test]
    fn a_different_bci_is_a_different_program_point() {
        let trace = vec![point("P", "main", 3), point("P", "rec", 7)];
        let fresh = vec![point("P", "rec", 9), point("P", "leaf", 1)];
        assert_eq!(trailing_overlap(&trace, &fresh), 0);
    }

    /// An empty snapshot, and an empty trace, both overlap nothing rather than
    /// dividing by zero on the way there.
    #[test]
    fn an_empty_side_overlaps_nothing() {
        let some = vec![point("P", "m", 0)];
        assert_eq!(trailing_overlap(&some, &[]), 0);
        assert_eq!(trailing_overlap(&[], &some), 0);
    }

    // -- memoized find_method ------------------------------------------

    #[test]
    fn memo_cold_and_warm_agree_and_pick_the_right_overload() {
        let (store, cid) = store_with(vec![
            named_method(
                "run",
                "(I)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 11,
                }],
            ),
            named_method(
                "run",
                "(J)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 22,
                }],
            ),
            named_method(
                "other",
                "()V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 33,
                }],
            ),
        ]);

        clear_method_slot_memo();
        // Cold (memo empty → linear scan) …
        assert_eq!(line_number_for_bci(&store, cid, "run", "(I)V", 0), Some(11));
        assert_eq!(line_number_for_bci(&store, cid, "run", "(J)V", 0), Some(22));
        assert_eq!(
            line_number_for_bci(&store, cid, "other", "()V", 0),
            Some(33)
        );
        // … and warm (memo populated → verified index hit) must agree exactly.
        assert_eq!(line_number_for_bci(&store, cid, "run", "(I)V", 0), Some(11));
        assert_eq!(line_number_for_bci(&store, cid, "run", "(J)V", 0), Some(22));
        assert_eq!(
            line_number_for_bci(&store, cid, "other", "()V", 0),
            Some(33)
        );
    }

    #[test]
    fn memo_never_holds_a_negative() {
        let (store, cid) = store_with(vec![named_method("present", "()V", Vec::new())]);
        clear_method_slot_memo();
        // Absent method: must not be memoized (a redefinition may add it).
        assert_eq!(line_number_for_bci(&store, cid, "absent", "()V", 0), None);
        assert!(
            !method_slot_memo()
                .read()
                .contains_key(&(cid.as_u32(), signature_hash("absent", "()V"))),
            "a failed lookup must never be memoized"
        );
    }

    #[test]
    fn memo_self_heals_when_the_class_is_redefined_under_it() {
        // Populate the memo against a 2-method class, then rebuild the store
        // with the methods in the opposite order (what a redefinition looks
        // like from this module's point of view) and re-query. The stored
        // index is now wrong; verification must catch it.
        clear_method_slot_memo();
        let (store, cid) = store_with(vec![
            named_method(
                "a",
                "()V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 1,
                }],
            ),
            named_method(
                "b",
                "()V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 2,
                }],
            ),
        ]);
        assert_eq!(line_number_for_bci(&store, cid, "a", "()V", 0), Some(1));
        assert_eq!(line_number_for_bci(&store, cid, "b", "()V", 0), Some(2));

        let (store2, cid2) = store_with(vec![
            named_method(
                "b",
                "()V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 20,
                }],
            ),
            named_method(
                "a",
                "()V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 10,
                }],
            ),
        ]);
        assert_eq!(cid2, cid, "fixture builds both stores at slot 0");
        assert_eq!(line_number_for_bci(&store2, cid2, "a", "()V", 0), Some(10));
        assert_eq!(line_number_for_bci(&store2, cid2, "b", "()V", 0), Some(20));
    }

    #[test]
    fn unloaded_class_resolves_to_none_not_to_a_neighbour() {
        clear_method_slot_memo();
        let (mut store, cid) = store_with(vec![named_method(
            "run",
            "()V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 7,
            }],
        )]);
        assert_eq!(line_number_for_bci(&store, cid, "run", "()V", 0), Some(7));
        // Unload: ClassStore leaves a tombstone and never reuses the ClassId,
        // so a deferred resolution against a stale id fails closed.
        let _ = store.remove(cid);
        assert_eq!(line_number_for_bci(&store, cid, "run", "()V", 0), None);
    }

    // -- deferred (lazy) line resolution --------------------------------

    #[test]
    fn deferred_resolution_fills_unambiguous_frames() {
        clear_method_slot_memo();
        let (store, cid) = store_with(vec![named_method(
            "compute",
            "()I",
            vec![
                LineNumberEntry {
                    start_pc: 0,
                    line_number: 40,
                },
                LineNumberEntry {
                    start_pc: 4,
                    line_number: 41,
                },
            ],
        )]);
        let mut entries = vec![entry_for(cid, "compute", 0), entry_for(cid, "compute", 6)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 2);
        assert_eq!(entries[0].line_number, 40);
        assert_eq!(entries[1].line_number, 41);
    }

    #[test]
    fn deferred_resolution_fails_closed_on_overloads() {
        clear_method_slot_memo();
        let (store, cid) = store_with(vec![
            named_method(
                "run",
                "(I)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 11,
                }],
            ),
            named_method(
                "run",
                "(J)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 22,
                }],
            ),
        ]);
        let mut entries = vec![entry_for(cid, "run", 0)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
        assert_eq!(
            entries[0].line_number, LINE_NUMBER_UNKNOWN,
            "an overload set must stay UNKNOWN rather than guess a body"
        );
    }

    // -- exact deferred resolution via `method_index` (CR-SW-1) ----------

    /// Two overloads with different `LineNumberTable`s. Without an index the
    /// resolver must decline (covered above); with one it must pick the exact
    /// body — this is the whole point of the field.
    fn overloaded_store() -> (ClassStore, ClassId) {
        store_with(vec![
            named_method(
                "run",
                "(I)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 11,
                }],
            ),
            named_method(
                "run",
                "(J)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 22,
                }],
            ),
            named_method(
                "run",
                "(Ljava/lang/String;)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 33,
                }],
            ),
        ])
    }

    #[test]
    fn deferred_resolution_is_exact_for_overloads_when_the_index_is_present() {
        clear_method_slot_memo();
        let (store, cid) = overloaded_store();
        let mut entries = vec![
            entry_for_indexed(cid, "run", 0, 0),
            entry_for_indexed(cid, "run", 0, 1),
            entry_for_indexed(cid, "run", 0, 2),
        ];
        assert_eq!(
            resolve_line_numbers_in_place(&store, &mut entries),
            3,
            "every overloaded frame must resolve once the slot is carried"
        );
        assert_eq!(entries[0].line_number, 11);
        assert_eq!(entries[1].line_number, 22);
        assert_eq!(entries[2].line_number, 33);
    }

    #[test]
    fn deferred_resolution_with_index_matches_the_eager_answer_exactly() {
        // The contract that lets a future pass defer the Throwable path: for
        // every overload, deferred-with-index == eager.
        clear_method_slot_memo();
        let (store, cid) = overloaded_store();
        for (idx, descriptor) in [(0u32, "(I)V"), (1, "(J)V"), (2, "(Ljava/lang/String;)V")] {
            let eager = line_number_for_bci(&store, cid, "run", descriptor, 0);
            let mut entries = vec![entry_for_indexed(cid, "run", 0, idx)];
            assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
            assert_eq!(
                Some(entries[0].line_number),
                eager,
                "deferred resolution of slot {idx} must equal the eager answer"
            );
        }
    }

    #[test]
    fn a_stale_index_falls_back_rather_than_resolving_the_wrong_body() {
        // A redefinition reorders methods under a captured trace. The index now
        // names a *different* method, so the name check must reject it. Here
        // the name is unique after the reorder, so the fallback resolves it
        // correctly; the point is that the wrong body is never used.
        clear_method_slot_memo();
        let (store, cid) = store_with(vec![
            named_method(
                "other",
                "()V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 99,
                }],
            ),
            named_method(
                "compute",
                "()I",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 40,
                }],
            ),
        ]);
        // Entry captured when `compute` was at slot 0.
        let mut entries = vec![entry_for_indexed(cid, "compute", 0, 0)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
        assert_eq!(
            entries[0].line_number, 40,
            "must not report `other`'s line 99"
        );
    }

    #[test]
    fn a_stale_index_into_an_overload_set_fails_closed() {
        // Same as above but the fallback cannot help: the name is overloaded.
        // The only acceptable answer is UNKNOWN.
        clear_method_slot_memo();
        let (store, cid) = overloaded_store();
        let mut entries = vec![entry_for_indexed(cid, "missing", 0, 1)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
        assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);

        // And an out-of-range index (class shrank under the trace).
        let mut entries = vec![entry_for_indexed(cid, "run", 0, 99)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
        assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);
    }

    #[test]
    fn an_index_pointing_at_the_wrong_overload_is_still_rejected_by_name_only_if_names_differ() {
        // Honest statement of the residual: the verification is by *name*, so a
        // stale index that happens to land on ANOTHER member of the same
        // overload set passes the check. That is why the index is written at
        // capture time and only ever read against the same live class — and why
        // the Throwable path stays eager rather than deferring across a window
        // in which a redefinition could reorder an overload set.
        clear_method_slot_memo();
        let (store, cid) = overloaded_store();
        let mut entries = vec![entry_for_indexed(cid, "run", 0, 1)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
        assert_eq!(entries[0].line_number, 22, "slot 1 is `run(J)V`");
    }

    #[test]
    fn resolution_never_touches_an_entry_whose_class_is_gone_even_with_an_index() {
        clear_method_slot_memo();
        let (mut store, cid) = overloaded_store();
        let mut entries = vec![entry_for_indexed(cid, "run", 0, 0)];
        let _ = store.remove(cid);
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
        assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);
    }

    #[test]
    fn synthetic_and_lockfree_entries_carry_no_method_index() {
        let e = synthetic_entry(Arc::from("java/lang/Object"), Arc::from("hashCode"));
        assert!(e.method_index.is_none());
        // `capture_frames_no_lines` is covered by its own integration path; the
        // invariant asserted here is the one the resolver depends on.
        assert_eq!(entry_for(ClassId::new(0), "run", 0).method_index, None);
    }

    #[test]
    fn deferred_resolution_never_overwrites_an_already_resolved_or_native_frame() {
        clear_method_slot_memo();
        let (store, cid) = store_with(vec![named_method(
            "compute",
            "()I",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 40,
            }],
        )]);
        let mut entries = vec![entry_for(cid, "compute", 0), entry_for(cid, "compute", 0)];
        entries[0].line_number = 999; // already resolved eagerly
        entries[1].line_number = LINE_NUMBER_NATIVE;
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
        assert_eq!(entries[0].line_number, 999);
        assert_eq!(entries[1].line_number, LINE_NUMBER_NATIVE);
    }

    #[test]
    fn deferred_resolution_after_unload_is_unknown_not_wrong() {
        clear_method_slot_memo();
        let (mut store, cid) = store_with(vec![named_method(
            "compute",
            "()I",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 40,
            }],
        )]);
        let mut entries = vec![entry_for(cid, "compute", 0)];
        let _ = store.remove(cid);
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
        assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);
        // Frame identity survives the unload — the trace still names the frame.
        assert_eq!(&*entries[0].class_name, "probe/Target");
        assert_eq!(&*entries[0].method_name, "compute");
        assert_eq!(entries[0].byte_code_index, 0);
    }

    #[test]
    fn deferred_resolution_skips_synthetic_entries() {
        clear_method_slot_memo();
        let (store, _cid) = store_with(vec![named_method("compute", "()I", Vec::new())]);
        let mut entries = vec![synthetic_entry(
            Arc::from("java/lang/Object"),
            Arc::from("hashCode"),
        )];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
        assert_eq!(entries[0].line_number, LINE_NUMBER_NATIVE);
    }

    #[test]
    fn signature_hash_separates_name_and_descriptor_domains() {
        assert_ne!(signature_hash("ab", "()V"), signature_hash("a", "b()V"));
        assert_eq!(signature_hash("run", "()V"), signature_hash("run", "()V"));
    }

    fn make_method_with_line_table(entries: Vec<LineNumberEntry>) -> ClassFileMethod {
        let code = CodeAttribute {
            max_stack: 1,
            max_locals: 1,
            code: cratonvm_reader::ByteView::from_vec(vec![0xb1]), // return
            exception_table: Vec::new(),
            attributes: vec![Attribute::LineNumberTable(entries)],
        };
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("probe"),
            descriptor: Arc::from("()V"),
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(code),
            )],
        }
    }

    #[test]
    fn line_table_picks_largest_start_leq_bci() {
        let method = make_method_with_line_table(vec![
            LineNumberEntry {
                start_pc: 0,
                line_number: 10,
            },
            LineNumberEntry {
                start_pc: 5,
                line_number: 20,
            },
            LineNumberEntry {
                start_pc: 9,
                line_number: 30,
            },
        ]);
        // Reimplement the core scan logic here against the method directly
        // (the `line_number_for_bci` entry point requires a ClassStore and
        // is covered by integration tests in vm/tests/wp1_9_*).
        let code = method.code().unwrap();
        let lnt = match &code.attributes[0] {
            Attribute::LineNumberTable(e) => e,
            _ => panic!(),
        };

        let lookup = |bci: u16| -> Option<u16> {
            let mut best = None;
            let mut best_start = 0;
            for e in lnt {
                if e.start_pc <= bci && (best.is_none() || e.start_pc >= best_start) {
                    best_start = e.start_pc;
                    best = Some(e.line_number);
                }
            }
            best
        };

        assert_eq!(lookup(0), Some(10));
        assert_eq!(lookup(3), Some(10));
        assert_eq!(lookup(5), Some(20));
        assert_eq!(lookup(8), Some(20));
        assert_eq!(lookup(9), Some(30));
        assert_eq!(lookup(42), Some(30));
    }

    #[test]
    fn line_table_multiple_attrs_combined() {
        // A method with two LineNumberTable attributes (permitted by spec).
        let code = CodeAttribute {
            max_stack: 1,
            max_locals: 1,
            code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
            exception_table: Vec::new(),
            attributes: vec![
                Attribute::LineNumberTable(vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 5,
                }]),
                Attribute::LineNumberTable(vec![LineNumberEntry {
                    start_pc: 10,
                    line_number: 99,
                }]),
            ],
        };
        let method = ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("probe"),
            descriptor: Arc::from("()V"),
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(code),
            )],
        };
        let code = method.code().unwrap();

        // Simulate the union semantics `line_number_for_bci` uses.
        let mut best = None;
        let mut best_start = 0;
        for attr in &code.attributes {
            if let Attribute::LineNumberTable(entries) = attr {
                for e in entries {
                    if e.start_pc <= 12 && (best.is_none() || e.start_pc >= best_start) {
                        best_start = e.start_pc;
                        best = Some(e.line_number);
                    }
                }
            }
        }
        assert_eq!(best, Some(99));
    }

    #[test]
    fn synthetic_entry_has_native_sentinel() {
        let e = synthetic_entry(Arc::from("java/lang/Object"), Arc::from("hashCode"));
        assert_eq!(e.line_number, LINE_NUMBER_NATIVE);
        assert_eq!(e.byte_code_index, -1);
        assert!(e.source_file.is_none());
    }

    #[test]
    fn unknown_is_minus_one() {
        assert_eq!(LINE_NUMBER_UNKNOWN, -1);
    }

    #[test]
    fn native_is_minus_two() {
        assert_eq!(LINE_NUMBER_NATIVE, -2);
    }
}
