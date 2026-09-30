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
    entry_from_frame_with_lines(class_store, frame, true)
}

/// [`entry_from_frame`] with optional source-line resolution.
///
/// A Throwable capture retains the exact class, method slot and BCI even when
/// it defers source-line resolution.  That lets a later reader recover the
/// same line as an eager capture, including for overloaded methods, without
/// paying to scan every `LineNumberTable` for a caught-and-discarded exception.
fn entry_from_frame_with_lines(
    class_store: &ClassStore,
    frame: &Frame,
    resolve_lines: bool,
) -> StackTraceEntry {
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
            let line = resolve_lines
                .then(|| {
                    idx.and_then(|i| class.methods.get(i as usize))
                        .and_then(|m| line_number_for_bci_in_method(m, bci))
                })
                .flatten()
                .unwrap_or(LINE_NUMBER_UNKNOWN);
            (idx, line)
        }
        None => (None, LINE_NUMBER_UNKNOWN),
    };
    // A frame running a replaced body whose lines differ from the class's
    // method reports its own, whatever `resolve_lines` says: the lazy
    // resolver would read the current body's table.
    let line_number = frame.own_line_number(bci_i32).unwrap_or(line_number);

    StackTraceEntry {
        class_name,
        method_name,
        method_descriptor: Some(method_descriptor),
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
    capture_full_trace_with_lines(class_store, frames, true)
}

/// [`capture_full_trace`] for a `StackWalker` walk: an entry of an activation
/// begun before a redefinition of its class -- an old version of its method,
/// obsolete or EMCP -- has no line ([`LINE_NUMBER_UNKNOWN`]), as HotSpot's
/// `StackFrameInfo` of such a frame has none (measured, JDK 25, JIT and
/// `-Xint`: `tools/probes/interp/L3/L3W39HotSwapWalkerLines.java`; even a
/// redefinition with identical bytes). A `Throwable` of the same activation
/// keeps the old body's line, so this is the walker's capture only
/// (`NativeExceptionAccess::capture_stack_walk_trace`). Interpreter round i1
/// wave 39, lane L3
/// (`docs/internal/fixed-bugs/interpreter-L3-a-stack-walk-reports-a-line-for-an-obsolete-method-FIXED-20261003.md`).
///
/// Exactly [`capture_full_trace`] while no class of the process was ever
/// redefined (one load). Otherwise the entries are matched with the slots
/// [`collect_trace_slots`] lays out over the same compiled frames, which
/// refuses exactly where the entry builders do (see that function), so the
/// two are index-aligned; a length disagreement leaves every line as it is.
/// `CRATONVM_DBG_RETRANSFORM=1` prints `[redefine] stack walk: no line for an
/// old activation:` per entry this clears (the positive control).
///
/// Every entry with a class id names its class's source file, except one this
/// clears ([`fill_missing_source_files`], wave 40): so an entry with a class
/// id and no source file has no file to show, which the native `walk`'s
/// carrier, whose file is otherwise read lazily from the class, records
/// (`reflect_invoke::populate_stack_frame`).
pub(crate) fn capture_stack_walk_trace(
    cm: &crate::classloading::ClassManager,
    frames: &[Frame],
) -> Vec<StackTraceEntry> {
    let class_store = &cm.class_store;
    if cratonvm_classloading::class_redefinition_count() == 0 {
        let mut out = capture_full_trace(class_store, frames);
        fill_missing_source_files(class_store, &mut out);
        return out;
    }
    let jit = crate::jit::conservative_roots::active_compiled_frames();
    if jit.is_empty() {
        let mut out: Vec<StackTraceEntry> = frames
            .iter()
            .map(|f| entry_from_frame_with_lines(class_store, f, true))
            .collect();
        fill_missing_source_files(class_store, &mut out);
        for (entry, frame) in out.iter_mut().zip(frames) {
            if frame_predates_its_class_redefinition(cm, frame) {
                clear_old_activation_line(entry);
            }
        }
        return out;
    }
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    let mut out = interleave_compiled_frames(class_store, frames, &jit, &osr_bci, true);
    fill_missing_source_files(class_store, &mut out);
    let slots = collect_trace_slots(frames, &jit, &osr_bci);
    if slots.len() == out.len() {
        for (entry, slot) in out.iter_mut().zip(&slots) {
            if slot_predates_its_class_redefinition(cm, slot) {
                clear_old_activation_line(entry);
            }
        }
    }
    out
}

/// Did `frame`'s activation begin before a redefinition of its class? Moved
/// across one (`Frame::predates_its_class_redefinition`, which an obsolete
/// frame always is), or not yet moved while its stamp is older than the
/// class's last redefinition.
fn frame_predates_its_class_redefinition(
    cm: &crate::classloading::ClassManager,
    frame: &Frame,
) -> bool {
    frame.predates_its_class_redefinition()
        || frame.runs_obsolete_method()
        || cm
            .redefinition_history(frame.class_id)
            .is_some_and(|history| history.is_stale(frame.redefine_stamp()))
}

/// [`frame_predates_its_class_redefinition`] for a capture slot. A compiled
/// activation, and a callee it inlined, runs the bytecode its compilation
/// read: older than its class's last redefinition when its
/// `compile_cp_stamp` is (`ActiveCompiledFrame::compile_cp_stamp`). A slot
/// with no stamp, or an inlined level with no recorded class, keeps its line.
fn slot_predates_its_class_redefinition(
    cm: &crate::classloading::ClassManager,
    slot: &TraceSlot<'_>,
) -> bool {
    let stale = |class_id: u32, stamp: Option<u64>| {
        class_id != 0
            && stamp.is_some_and(|stamp| {
                cm.redefinition_history(ClassId::new(class_id))
                    .is_some_and(|history| history.is_stale(stamp))
            })
    };
    match slot {
        TraceSlot::Interp { frame, .. } => frame_predates_its_class_redefinition(cm, frame),
        TraceSlot::Compiled(compiled) => {
            stale(compiled.owner_class_id, compiled.compile_cp_stamp)
        }
        TraceSlot::Inlined(level, stamp) => stale(level.class_id, *stamp),
    }
}

/// Leave `entry`'s line and source file unknown: it stands for an old
/// activation. HotSpot's element of such a frame has neither (measured: the
/// probe's `file` row). The source file is honoured by the JDK walk's
/// carrier (`lang_stackwalker::populate_sfi`) and, since wave 40, by the
/// native `walk`'s carrier, which reads the file lazily from the class only
/// for an entry that names one (`reflect_invoke::populate_stack_frame`).
fn clear_old_activation_line(entry: &mut StackTraceEntry) {
    if entry.line_number == LINE_NUMBER_UNKNOWN && entry.source_file.is_none() {
        return;
    }
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!(
            "[redefine] stack walk: no line for an old activation: {}.{} bci={} line-was={}",
            entry.class_name, entry.method_name, entry.byte_code_index, entry.line_number
        );
    }
    entry.line_number = LINE_NUMBER_UNKNOWN;
    entry.source_file = None;
}

/// Give every entry of `entries` that has a class id but no source file its
/// class's `SourceFile`, as [`resolve_line_numbers_in_place`] does for a
/// deferred capture. A frame's cached method carries the file, so this is a
/// branch per entry and a class lookup for the rare frame built without it.
/// Afterwards an entry with a class id and no file stands for a class with no
/// `SourceFile` -- or, once [`clear_old_activation_line`] ran, an old
/// activation: either way it has no file to show (interpreter round i1 wave
/// 40, lane L3;
/// `docs/internal/fixed-bugs/interpreter-L3-a-stack-frames-line-and-file-are-not-read-lazily-FIXED-20261005.md`
/// item 1).
fn fill_missing_source_files(class_store: &ClassStore, entries: &mut [StackTraceEntry]) {
    for entry in entries.iter_mut() {
        if entry.source_file.is_some() {
            continue;
        }
        let Some(class) = entry.class_id.and_then(|class_id| class_store.get(class_id)) else {
            continue;
        };
        entry.source_file = class.source_file.as_deref().map(Arc::from);
    }
}

/// Capture the complete frame set while deferring source-line resolution.
///
/// This is for `Throwable` construction. It preserves the full live/JIT/OSR
/// frame topology, source-file name, BCI, class id, and exact method slot. A
/// reader later passes the result through [`resolve_line_numbers_in_place`].
pub fn capture_full_trace_without_lines(
    class_store: &ClassStore,
    frames: &[Frame],
) -> Vec<StackTraceEntry> {
    let jit = crate::jit::conservative_roots::active_compiled_frames();
    if jit.is_empty() {
        return capture_frames_no_lines_exact(frames);
    }
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    interleave_compiled_frames(class_store, frames, &jit, &osr_bci, false)
}

/// Capture a complete Throwable trace without a `ClassStore` borrow.
///
/// Interpreter frames already carry class, method, descriptor, source file and
/// BCI. Compiled-frame labels carry the same identity except source file. The
/// latter, along with the line number, is restored by
/// [`resolve_line_numbers_in_place`] only when Java observes the trace.
pub fn capture_full_trace_without_store(frames: &[Frame]) -> Vec<StackTraceEntry> {
    let jit = crate::jit::conservative_roots::active_compiled_frames();
    if jit.is_empty() {
        return capture_frames_no_lines_exact(frames);
    }
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    interleave_compiled_frames_without_store(frames, &jit, &osr_bci, None)
}

/// HotSpot's `MaxJavaStackTraceDepth` default: `java_lang_Throwable::
/// fill_in_stack_trace` records at most this many frames, the INNERMOST ones,
/// counted after the fill-frame skip ([`trim_throwable_fill_frames`]). So
/// `new Throwable().getStackTrace().length` is at most 1024 however deep the
/// stack is — including for a `StackOverflowError`, which is by construction
/// built at the deepest stack a thread can have and used to capture (and then
/// print) every frame of it.
///
/// Throwable traces only. `StackWalker` and `Thread.getStackTrace` are not
/// capped, as on HotSpot. This is the DEFAULT of the per-VM
/// `VmConfig::max_java_stack_trace_depth` (`-XX:MaxJavaStackTraceDepth=N`),
/// which is what the capture paths read; `0` there means "unlimited" to
/// [`cap_throwable_trace`], as it does for the HotSpot flag.
pub const MAX_JAVA_STACK_TRACE_DEPTH: usize = 1024;

/// Interpreter frames a bounded throwable capture keeps beyond the cap, for
/// the fill-frame trim to remove (the throwable class's own `fillInStackTrace`
/// and `<init>` chain). A trim that leaves fewer than `max_depth` slots in the
/// window is detected and the capture is redone unbounded -- the slack is a
/// cost knob, never a correctness one.
const THROWABLE_TRIM_SLACK: usize = 64;

/// How many innermost interpreter frames a throwable capture capped at
/// `max_depth` needs to walk: the cap plus the trim slack, or everything for
/// an uncapped (`0`) capture.
fn throwable_capture_budget(max_depth: usize) -> usize {
    if max_depth == 0 {
        usize::MAX
    } else {
        max_depth.saturating_add(THROWABLE_TRIM_SLACK)
    }
}

/// Keep the innermost `max_depth` entries of an OUTERMOST-first throwable
/// trace. `0` keeps everything. See [`MAX_JAVA_STACK_TRACE_DEPTH`].
pub fn cap_throwable_trace<T>(trace: &mut Vec<T>, max_depth: usize) {
    if max_depth != 0 && trace.len() > max_depth {
        let excess = trace.len() - max_depth;
        trace.drain(..excess);
    }
}

/// [`capture_full_trace_without_store`] followed by
/// [`trim_throwable_fill_frames`], with the trim decided BEFORE any
/// [`StackTraceEntry`] is built.
///
/// The frames the trim removes are exactly the ones a throwable's construction
/// puts on top of its throw site: the `fillInStackTrace` frames and the
/// `<init>` chain of its own class. Under `--jdk-only` that chain is real
/// bytecode -- five levels for `ArrayIndexOutOfBoundsException`, usually
/// inlined into the compiled caller -- so building their entries (a label
/// lookup and four shared-string clones each) only to drop them again was the
/// larger part of an ordinary `new SomeException(msg)` capture. Here the
/// frames are first laid out as borrowed [`TraceSlot`]s, the trim runs on
/// those, and entries are built only for the survivors.
///
/// The result is identical to building everything and trimming afterwards:
/// the slots are collected under the same refusals the entry builders apply
/// (so a slot exists iff an entry would), the trim decision is the shared
/// [`throwable_fill_frame_keep_len`], and a slot's identity is read from the
/// same source its entry's `method_name`/`class_id`/`class_name` would be.
///
/// `is_a` is [`throwable_holder_is_a`] for the throwable being filled; it is
/// asked only once a frame's method name already matched, so the caller can
/// defer taking the class-manager lock until then, as the build-then-trim path
/// did.
///
/// The result is capped at `max_depth` innermost entries
/// ([`cap_throwable_trace`]; `0` is unlimited), and the capture is BOUNDED to
/// match: when no compiled frame is active, only the innermost
/// [`throwable_capture_budget`] interpreter frames are laid out as slots. Every
/// interpreter frame yields exactly one slot in that case, and the trim stops
/// at the first non-matching slot from the innermost end, so whenever the
/// windowed trim leaves at least `max_depth` slots it stopped inside the window
/// at the same place the full trim would, and the capped answer is identical.
/// Otherwise the window is discarded and the capture redone unbounded. A
/// `StackOverflowError` at depth N therefore costs `max_depth + slack` slots,
/// not N slots and N entries of which all but `max_depth` were then dropped.
pub fn capture_throwable_trace_without_store(
    frames: &[Frame],
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
) -> Vec<StackTraceEntry> {
    capture_throwable_slots(frames, max_depth, is_a, &[])
}

/// [`capture_throwable_trace_without_store`] as [`BacktraceFrame`]s, the form
/// the VM's throwable registry retains: the same frames, in the same order,
/// under the same trim and cap, but an interpreter frame of a cached method
/// costs one refcount instead of four (see [`BacktraceFrame`]).
///
/// `splices`: the JDK frames of the reflective calls the thread is inside
/// ([`reflective_splices`]; empty outside any), listed between each caller and
/// its target as HotSpot's trace lists them, and counted towards `max_depth`
/// as on HotSpot (interpreter round i1 wave 43, lane L3).
pub(crate) fn capture_throwable_backtrace_without_store(
    frames: &[Frame],
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<BacktraceFrame> {
    capture_throwable_slots(frames, max_depth, is_a, splices)
}

/// The body of both capture doors above; `T` only decides what one surviving
/// slot is built into. `splices` (a reflective call's JDK frames) turn off
/// the bounded window, whose slots would not start at frame 0.
fn capture_throwable_slots<T: FromTraceSlot>(
    frames: &[Frame],
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<T> {
    // An armed trap snapshot stands in for the compiled-frame walk: see
    // `arm_trap_capture`.
    let jit = match armed_trap_capture_frames(frames.len()) {
        Some(snapshot) => snapshot,
        None => crate::jit::conservative_roots::active_compiled_frames(),
    };
    let (jit, osr_bci) = if jit.is_empty() {
        let start = frames
            .len()
            .saturating_sub(throwable_capture_budget(max_depth));
        if start > 0 && splices.is_empty() {
            let no_osr = OsrBciOverrides::default();
            let slots = collect_trace_slots(&frames[start..], &[], &no_osr);
            let keep = throwable_fill_frame_keep_len(&slots, is_a);
            // `start > 0` implies a finite budget, i.e. `max_depth != 0`.
            if keep >= max_depth {
                return slots[keep - max_depth..keep]
                    .iter()
                    .filter_map(T::from_slot)
                    .collect();
            }
        }
        (jit, OsrBciOverrides::default())
    } else {
        drop_osr_continuations(frames, jit)
    };
    let slots = collect_trace_slots(frames, &jit, &osr_bci);
    let keep = throwable_fill_frame_keep_len(&slots, is_a);
    let mut trace: Vec<T> = if splices.is_empty() {
        slots[..keep].iter().filter_map(T::from_slot).collect()
    } else {
        let positions = reflective_splice_positions(&slots, splices, true);
        build_slots_with_splices(&slots, keep, splices, &positions)
    };
    cap_throwable_trace(&mut trace, max_depth);
    trace
}

/// The trap snapshot armed for the throwable a JIT door is constructing.
/// See [`arm_trap_capture`].
struct ArmedSnapshot {
    frames: Vec<ActiveCompiledFrame>,
    /// `frames.len()` of the thread's interpreter stack when it was armed.
    base_depth: usize,
    /// Did any capture take the snapshot's frames?
    used: bool,
}

thread_local! {
    /// Per THREAD, like the snapshot it holds (a stack capture reads only its
    /// own thread's frames); empty outside [`arm_trap_capture`]'s window.
    static TRAP_CAPTURE: std::cell::RefCell<Option<ArmedSnapshot>> =
        const { std::cell::RefCell::new(None) };
}

/// Round 12 wave 4 (lane exc2), `docs/internal/jit-proposals/jit-r12-exc-proposals-RETIRED-20260928.md` W3-2: ONE stack walk
/// per compiled implicit exception.
///
/// A JIT door that builds the throwable for an implicit signal holds the
/// compiled frames the trapping helper snapshotted. It used to build the
/// throwable (whose `fillInStackTrace` walked the compiled frames AGAIN) and
/// then splice the snapshot onto the stored trace
/// (`exceptions::attach_snapshotted_trap_frames`: a registry read, a copy of
/// every entry, the dedupe and overlap merge, a re-store). While armed, every
/// throwable capture on this thread uses the snapshot AS its compiled-frame
/// list instead of walking, so the trace comes out whole and the splice is not
/// needed.
///
/// That is the same trace only when the snapshot is exactly the compiled half
/// of the stack the capture would lay out, i.e. when
/// [`trap_snapshot_can_stand_in_for_walk`] holds; the caller checks it. Every
/// frame of such a snapshot sits above the innermost interpreter frame, so:
/// a frame still live is the activation the walk would find, suspended at the
/// same call, and the snapshot's bci for it is at least as good (the live
/// walk reads a frame that made the helper call with no safepoint id -- a
/// same-frame compiled `catch` -- at its last call's bci or `-1`, which is what
/// printed the catching method twice,
/// `r12w3-exc-same-frame-implicit-catch-trace-duplicates-top-frame-FIXED-20260926.md`);
/// a frame that has unwound is what the splice appended; and a compiled frame
/// pushed after the trap is part of the throwable's own construction, which the
/// fill-frame trim removes either way.
///
/// Returns the snapshot back when it cannot be armed (already armed: a door
/// reached inside another door's construction keeps the splice).
pub(crate) fn arm_trap_capture(
    frames: Vec<ActiveCompiledFrame>,
    base_depth: usize,
) -> Result<ArmedTrapCapture, Vec<ActiveCompiledFrame>> {
    TRAP_CAPTURE.with(|c| {
        let Ok(mut slot) = c.try_borrow_mut() else {
            return Err(frames);
        };
        if slot.is_some() {
            return Err(frames);
        }
        *slot = Some(ArmedSnapshot {
            frames,
            base_depth,
            used: false,
        });
        Ok(ArmedTrapCapture { _private: () })
    })
}

/// Whether `snapshot` can replace the compiled-frame walk of a capture made on
/// top of `frames` (see [`arm_trap_capture`]): non-empty, and every frame
/// recorded at exactly `frames.len()`, i.e. above the innermost interpreter
/// frame. A frame under an interpreter frame may have been rebuilt as that
/// frame since the trap (an optimizing caller's deopt), and a frame recorded
/// deeper than the stack now is would be laid out above the constructor frames
/// the trim must reach; both keep the walk and the splice.
pub(crate) fn trap_snapshot_can_stand_in_for_walk(
    frames: &[Frame],
    snapshot: &[ActiveCompiledFrame],
) -> bool {
    !snapshot.is_empty()
        && snapshot
            .iter()
            .all(|f| usize::try_from(f.interp_depth).ok() == Some(frames.len()))
}

/// The armed snapshot's frames for a capture made on top of `depth`
/// interpreter frames, or `None` (walk as usual). A clone, not a take: a
/// throwable whose constructor captures AND whose explicit `fillInStackTrace`
/// captures again must get the same answer from both, and a throwable built
/// inside the constructor sees the trap frames exactly as HotSpot, which builds
/// the implicit exception at the trap, would show them.
fn armed_trap_capture_frames(depth: usize) -> Option<Vec<ActiveCompiledFrame>> {
    TRAP_CAPTURE.with(|c| {
        let mut slot = c.try_borrow_mut().ok()?;
        let armed = slot.as_mut()?;
        if depth < armed.base_depth {
            return None;
        }
        armed.used = true;
        Some(armed.frames.clone())
    })
}

/// The window [`arm_trap_capture`] opened. Dropping it disarms, so a panic
/// inside the construction cannot leave a snapshot armed for the next,
/// unrelated capture on this thread.
pub(crate) struct ArmedTrapCapture {
    _private: (),
}

impl ArmedTrapCapture {
    /// Close the window: the snapshot back, and whether any capture used it.
    /// An unused snapshot is the caller's to splice as before.
    pub(crate) fn disarm(self) -> (Option<Vec<ActiveCompiledFrame>>, bool) {
        TRAP_CAPTURE.with(|c| match c.try_borrow_mut() {
            Ok(mut slot) => match slot.take() {
                Some(armed) => (Some(armed.frames), armed.used),
                None => (None, false),
            },
            Err(_) => (None, false),
        })
    }
}

impl Drop for ArmedTrapCapture {
    fn drop(&mut self) {
        TRAP_CAPTURE.with(|c| {
            if let Ok(mut slot) = c.try_borrow_mut() {
                *slot = None;
            }
        });
    }
}

/// What a capture builds from one surviving [`TraceSlot`]. `None` exactly
/// where [`TraceSlot::build`] refuses, so both forms keep the same frames.
trait FromTraceSlot: Sized {
    fn from_slot(slot: &TraceSlot<'_>) -> Option<Self>;
    /// A frame no slot stands for (a reflective call's JDK frame,
    /// [`ReflectiveSplice`]).
    fn from_entry(entry: StackTraceEntry) -> Self;
}

impl FromTraceSlot for StackTraceEntry {
    fn from_slot(slot: &TraceSlot<'_>) -> Option<Self> {
        slot.build()
    }

    fn from_entry(entry: StackTraceEntry) -> Self {
        entry
    }
}

impl FromTraceSlot for BacktraceFrame {
    fn from_entry(entry: StackTraceEntry) -> Self {
        BacktraceFrame::Entry(entry)
    }

    fn from_slot(slot: &TraceSlot<'_>) -> Option<Self> {
        match slot {
            TraceSlot::Interp { frame, bci } => match frame.cached_method() {
                Some(method) => Some(BacktraceFrame::Method {
                    method: Arc::clone(method),
                    bci: *bci,
                    class_id: frame.class_id,
                }),
                None => slot.build().map(BacktraceFrame::Entry),
            },
            // Round 12 wave 5 (lane exc3), `docs/internal/jit-proposals/jit-r12-exc-proposals-RETIRED-20260928.md` W4-3:
            // the label, not a built entry. `None` under exactly the refusals
            // `compiled_frame_entry_without_store` /
            // `inlined_frame_entry_without_store` apply (`split_frame_label`
            // is `parse_frame_label_uncached`'s predicate, the bci bound is the
            // same constant), so both forms keep the same frames.
            TraceSlot::Compiled(f) if compact_compiled_backtrace_enabled() => {
                split_frame_label(&f.label)?;
                Some(BacktraceFrame::Compiled {
                    label: Arc::clone(&f.label),
                    owner_class_id: f.owner_class_id,
                    bci: f.bci,
                    cp_stamp: f.compile_cp_stamp,
                })
            }
            TraceSlot::Inlined(level, cp_stamp) if compact_compiled_backtrace_enabled() => {
                if level.bci >= MAX_INLINED_LEVEL_BCI {
                    return None;
                }
                split_frame_label(&level.label)?;
                Some(BacktraceFrame::Inlined {
                    label: Arc::clone(&level.label),
                    class_id: level.class_id,
                    bci: level.bci,
                    cp_stamp: *cp_stamp,
                })
            }
            _ => slot.build().map(BacktraceFrame::Entry),
        }
    }
}

/// JVMS 4.9.1's `code_length` bound, as the inlined-level builders apply it.
const MAX_INLINED_LEVEL_BCI: u32 = 65_536;

/// `CRATONVM_JIT_COMPACT_COMPILED_BACKTRACE` -- default ON; `0` retains
/// compiled and inlined frames as built [`StackTraceEntry`]s again (the
/// pre-round-12-wave-5 form). Cached: read per retained compiled frame.
fn compact_compiled_backtrace_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_COMPACT_COMPILED_BACKTRACE")
    })
}

/// The entry a retained compiled / inlined frame stands for when its label
/// does not parse -- unreachable, because [`FromTraceSlot::from_slot`] refuses
/// such a label, but [`BacktraceFrame::to_entry`] is infallible and must not
/// panic. Names the raw label rather than inventing a split.
fn unparsed_label_entry(
    label: &Arc<str>,
    class_id: Option<ClassId>,
    bci: i32,
) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::clone(label),
        method_name: Arc::from(""),
        method_descriptor: None,
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id,
        method_index: None,
    }
}

/// Is `frame` a frame HotSpot leaves out of a `Throwable`'s stack trace
/// (`Method::is_hidden`, without `-XX:+ShowHiddenFrames`): a method of a
/// hidden class, or one annotated `@jdk.internal.vm.annotation.Hidden`
/// (`Thread.runWith`, `ScopedValue$Carrier.runWith`, ...)? Interpreter round
/// i1 wave 34, applied by the `--jdk-only` throwable capture.
///
/// A cached method's answer is decided once (`CachedBytecodeMethod::
/// hidden_frame`), so a warm capture of such frames pays one load each. An
/// entry (an `Owned` frame's, rare) is judged from its class id each time. A
/// compiled activation or an inlined level is judged from its label and class
/// id, memoized per label ([`compiled_label_is_hidden`], wave 37); one whose
/// class cannot be hidden ([`label_may_be_hidden`]) is decided without the
/// store.
pub(crate) fn backtrace_frame_is_hidden(store: &ClassStore, frame: &BacktraceFrame) -> bool {
    match frame {
        BacktraceFrame::Method { method, .. } => *method.hidden_frame.get_or_init(|| {
            method_is_hidden(
                store,
                method.declaring_class_id,
                &method.method_name,
                Some(&method.method_descriptor),
                None,
            )
        }),
        BacktraceFrame::Compiled {
            label,
            owner_class_id,
            ..
        } => compiled_label_is_hidden(store, label, *owner_class_id),
        BacktraceFrame::Inlined {
            label, class_id, ..
        } => *class_id != 0 && compiled_label_is_hidden(store, label, *class_id),
        BacktraceFrame::Entry(entry) => stack_entry_is_hidden(store, entry),
    }
}

/// [`backtrace_frame_is_hidden`]'s answer if it needs no class store: `None`
/// for a cached method not yet judged, and for an entry, compiled activation
/// or inlined level whose class name allows it ([`entry_may_be_hidden`]).
pub(crate) fn backtrace_frame_hidden_decided(frame: &BacktraceFrame) -> Option<bool> {
    match frame {
        BacktraceFrame::Method { method, .. } => method.hidden_frame.get().copied(),
        BacktraceFrame::Compiled { label, .. } => (!label_may_be_hidden(label)).then_some(false),
        BacktraceFrame::Inlined {
            label, class_id, ..
        } => (*class_id == 0 || !label_may_be_hidden(label)).then_some(false),
        BacktraceFrame::Entry(entry) if entry.class_id.is_some() => {
            (!entry_may_be_hidden(&entry.class_name)).then_some(false)
        }
        BacktraceFrame::Entry(_) => Some(false),
    }
}

/// A short name for `frame`'s shape, for the `CRATONVM_DBG_STTRACE` lines.
pub(crate) fn backtrace_frame_kind(frame: &BacktraceFrame) -> &'static str {
    match frame {
        BacktraceFrame::Method { .. } => "interpreted",
        BacktraceFrame::Compiled { .. } => "compiled",
        BacktraceFrame::Inlined { .. } => "inlined",
        BacktraceFrame::Entry(_) => "entry",
    }
}

/// Can a frame of `class_name` be hidden at all? HotSpot honours `@Hidden`
/// only in privileged (JDK) classes, and a hidden class's name carries a
/// `/0x…` segment; any other entry (an application's `main`, typically, which
/// the launcher runs in an `Owned` frame) is decided without the class
/// manager.
pub(crate) fn entry_may_be_hidden(class_name: &str) -> bool {
    class_name.starts_with("java/")
        || class_name.starts_with("jdk/")
        || class_name.starts_with("sun/")
        || class_name
            .rsplit('/')
            .next()
            .is_some_and(|last| last.starts_with("0x"))
}

/// [`backtrace_frame_is_hidden`] for a plain entry (a `StackWalker` walk's,
/// interpreter round i1 wave 35).
///
/// A compiled activation's or an inlined level's entry (`capture_full_trace`
/// builds them with the owner's, or the recorded, class id and the class's
/// own name) is judged the same way, so a walk sees a hot hidden method as it
/// sees an interpreted one. The entry's `method_index`, when the capture
/// resolved one, names the method without a scan (wave 37).
pub(crate) fn stack_entry_is_hidden(store: &ClassStore, entry: &StackTraceEntry) -> bool {
    entry.class_id.is_some_and(|class_id| {
        entry_may_be_hidden(&entry.class_name)
            && method_is_hidden(
                store,
                class_id,
                &entry.method_name,
                entry.method_descriptor.as_deref(),
                entry.method_index,
            )
    })
}

/// `Method::is_hidden` for `class_id.name desc` (any descriptor when `desc`
/// is `None`). `index`, when given, is the method's slot in the class as a
/// capture resolved it: used when that slot still names `name desc`, which
/// saves the scan; otherwise the scan decides.
fn method_is_hidden(
    store: &ClassStore,
    class_id: ClassId,
    name: &str,
    desc: Option<&str>,
    index: Option<u32>,
) -> bool {
    let Some(class) = store.get(class_id) else {
        return false;
    };
    if class.is_hidden() {
        return true;
    }
    // HotSpot honours `@Hidden` only in privileged code (the boot and platform
    // loaders' classes); the name screen is this VM's approximation of that,
    // applied to every frame shape alike (wave 37: an interpreted frame used
    // to skip it).
    if !entry_may_be_hidden(&class.name) {
        return false;
    }
    let named = |m: &ClassFileMethod| &*m.name == name && desc.is_none_or(|d| &*m.descriptor == d);
    if let Some(m) = index.and_then(|i| class.methods.get(i as usize)) {
        if named(m) {
            return method_has_hidden_annotation(class, m);
        }
    }
    class
        .methods
        .iter()
        .filter(|m| named(*m))
        .any(|m| method_has_hidden_annotation(class, m))
}

/// Is `method` (one of `class`'s) annotated `@jdk.internal.vm.annotation.Hidden`?
fn method_has_hidden_annotation<'a>(class: &'a Class, method: &'a ClassFileMethod) -> bool {
    cratonvm_classloading::annotations::method_annotations(method)
        .find_by_type_descriptor("Ljdk/internal/vm/annotation/Hidden;", |i| {
            class.constant_pool.get_utf8(i)
        })
        .is_some()
}

/// One frame of a throwable's RETAINED backtrace (the VM's registry,
/// `SharedVm::store_throwable_backtrace`), materialised into a
/// [`StackTraceEntry`] only when Java reads the trace.
///
/// Stage 1a of `docs/internal/fixed-bugs/interpreter-L5-proposal-compact-backtrace-RETIRED-20261003.md`.
/// An entry holds four shared name strings (`class_name`, `method_name`,
/// `method_descriptor`, `source_file`), so capturing a frame cost four atomic
/// increments on the per-method `Arc<str>`s and dropping it four decrements —
/// on cache lines every thread throwing through the same methods shares. A
/// frame running a cached method already points at ONE shared record holding
/// exactly those four strings, so the backtrace keeps that record instead:
/// one increment, one decrement. [`Self::to_entry`] produces the entry the
/// eager capture ([`TraceSlot::build`]) would have built, field for field.
///
/// `Owned` interpreter frames (reflective / uncached pushes) keep a full
/// entry. Compiled and inlined frames keep their shared label (round 12 wave
/// 5, `Compiled` / `Inlined`; `CRATONVM_JIT_COMPACT_COMPILED_BACKTRACE=0`
/// retains them as full entries again).
#[derive(Clone)]
pub(crate) enum BacktraceFrame {
    /// An interpreter frame whose method metadata is a shared
    /// `CachedBytecodeMethod`.
    Method {
        method: Arc<crate::classloading::resolution::CachedBytecodeMethod>,
        /// The bci the entry reports (`last_instr_pc`, or the OSR override).
        bci: i32,
        /// The frame's own `ClassId` (`Frame::class_id`).
        class_id: ClassId,
    },
    /// A compiled activation: its artifact's shared label (one refcount)
    /// instead of an entry's four name strings. Materialises exactly as
    /// `compiled_frame_entry_without_store` builds it.
    Compiled {
        /// `ActiveCompiledFrame::label`, shared with the artifact.
        label: Arc<str>,
        /// `ActiveCompiledFrame::owner_class_id`.
        owner_class_id: u32,
        /// `ActiveCompiledFrame::bci` (`-1` = none).
        bci: i32,
        /// `ActiveCompiledFrame::compile_cp_stamp`: the class version this
        /// activation ran (interpreter round i1 wave 38, lane L3).
        cp_stamp: Option<u64>,
    },
    /// A callee a compiled activation inlined, as
    /// `inlined_frame_entry_without_store` builds it.
    Inlined {
        /// `InlinedLevel::label`, shared with the emitter's chain.
        label: Arc<str>,
        /// `InlinedLevel::class_id` (`0` = none recorded).
        class_id: u32,
        /// `InlinedLevel::bci`, below the JVMS 4.9.1 bound.
        bci: u32,
        /// The inlining activation's `compile_cp_stamp`: the splice is of
        /// the callee's bytecode as it was then (wave 38, lane L3).
        cp_stamp: Option<u64>,
    },
    /// Any other frame, already built.
    Entry(StackTraceEntry),
}

impl BacktraceFrame {
    /// The [`StackTraceEntry`] this frame stands for, line unresolved.
    pub(crate) fn to_entry(&self) -> StackTraceEntry {
        match self {
            BacktraceFrame::Method {
                method,
                bci,
                class_id,
            } => StackTraceEntry {
                class_name: Arc::clone(&method.class_name),
                method_name: Arc::clone(&method.method_name),
                method_descriptor: Some(Arc::clone(&method.method_descriptor)),
                source_file: method.source_file.clone(),
                line_number: LINE_NUMBER_UNKNOWN,
                byte_code_index: *bci,
                class_id: Some(*class_id),
                method_index: None,
            },
            BacktraceFrame::Compiled {
                label,
                owner_class_id,
                bci,
                ..
            } => {
                let class_id = Some(ClassId::new(*owner_class_id));
                frame_entry_from_label_without_store(label, class_id, *bci)
                    .unwrap_or_else(|| unparsed_label_entry(label, class_id, *bci))
            }
            BacktraceFrame::Inlined {
                label,
                class_id,
                bci,
                ..
            } => {
                let class_id = (*class_id != 0).then(|| ClassId::new(*class_id));
                // Cast: `from_slot` admits only a bci below the JVMS 4.9.1
                // bound, the cast `inlined_frame_entry_without_store` makes.
                let bci = *bci as i32;
                frame_entry_from_label_without_store(label, class_id, bci)
                    .unwrap_or_else(|| unparsed_label_entry(label, class_id, bci))
            }
            BacktraceFrame::Entry(entry) => entry.clone(),
        }
    }

    /// For a compiled activation or a callee it inlined, the constant-pool
    /// stamp of its compilation: the class version whose line table the
    /// frame's bci indexes (`obsolete_frames::resolve_lines_as_captured`;
    /// interpreter round i1 wave 38, lane L3). `None` for any other frame.
    pub(crate) fn compile_cp_stamp(&self) -> Option<u64> {
        match self {
            BacktraceFrame::Compiled { cp_stamp, .. } | BacktraceFrame::Inlined { cp_stamp, .. } => {
                *cp_stamp
            }
            BacktraceFrame::Method { .. } | BacktraceFrame::Entry(_) => None,
        }
    }
}

impl std::fmt::Debug for BacktraceFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BacktraceFrame::Method { method, bci, .. } => {
                write!(
                    f,
                    "Method({}.{} bci={bci})",
                    method.class_name, method.method_name
                )
            }
            BacktraceFrame::Compiled { label, bci, .. } => {
                write!(f, "Compiled({label} bci={bci})")
            }
            BacktraceFrame::Inlined { label, bci, .. } => {
                write!(f, "Inlined({label} bci={bci})")
            }
            BacktraceFrame::Entry(entry) => f.debug_tuple("Entry").field(entry).finish(),
        }
    }
}

/// One logical frame of a capture, borrowed, before its [`StackTraceEntry`]
/// exists. See [`capture_throwable_trace_without_store`].
enum TraceSlot<'a> {
    /// An interpreter frame, with the bci the entry reports for it (its own
    /// `last_instr_pc`, or the OSR override).
    Interp { frame: &'a Frame, bci: i32 },
    /// A compiled activation.
    Compiled(&'a ActiveCompiledFrame),
    /// A callee a compiled activation inlined, with that activation's
    /// `compile_cp_stamp` (`None` for an OSR override's chain).
    Inlined(&'a InlinedLevel, Option<u64>),
}

impl TraceSlot<'_> {
    fn build(&self) -> Option<StackTraceEntry> {
        match self {
            TraceSlot::Interp { frame, bci } => Some(StackTraceEntry {
                class_name: frame.class_name_arc(),
                method_name: frame.method_name_arc(),
                method_descriptor: Some(frame.method_descriptor_arc()),
                source_file: frame.source_file_arc(),
                // A replaced body's own line, else resolved later from the class.
                line_number: frame.own_line_number(*bci).unwrap_or(LINE_NUMBER_UNKNOWN),
                byte_code_index: *bci,
                class_id: Some(frame.class_id),
                method_index: None,
            }),
            TraceSlot::Compiled(slot) => compiled_frame_entry_without_store(slot),
            TraceSlot::Inlined(level, _) => inlined_frame_entry_without_store(level),
        }
    }
}

impl FillFrameIdentity for TraceSlot<'_> {
    fn fill_identity(&self) -> Option<(&str, Option<ClassId>, &str)> {
        match self {
            TraceSlot::Interp { frame, .. } => {
                Some((frame.method_name(), Some(frame.class_id), frame.class_name()))
            }
            TraceSlot::Compiled(slot) => {
                let (class_name, method_name) = split_frame_label(&slot.label)?;
                Some((method_name, Some(ClassId::new(slot.owner_class_id)), class_name))
            }
            TraceSlot::Inlined(level, _) => {
                let (class_name, method_name) = split_frame_label(&level.label)?;
                let holder = (level.class_id != 0).then(|| ClassId::new(level.class_id));
                Some((method_name, holder, class_name))
            }
        }
    }
}

/// `(class, method)` of a `"class/Name.method:descriptor"` label, borrowed,
/// under exactly [`parse_frame_label_uncached`]'s refusals.
fn split_frame_label(label: &str) -> Option<(&str, &str)> {
    let (owner_and_method, _descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() {
        return None;
    }
    Some((class_name, method_name))
}

/// [`interleave_compiled_frames_without_store`]'s layout, as slots. Each
/// `push_*` below keeps the refusal of the entry builder it mirrors, so a slot
/// is pushed iff that builder would have produced an entry.
fn collect_trace_slots<'a>(
    frames: &'a [Frame],
    jit: &'a [ActiveCompiledFrame],
    osr_bci: &'a OsrBciOverrides,
) -> Vec<TraceSlot<'a>> {
    let inlined = jit.iter().map(|f| f.inline_chain.len()).sum::<usize>()
        + osr_bci.values().map(|(_, chain)| chain.len()).sum::<usize>();
    let mut out = Vec::with_capacity(frames.len() + jit.len() + inlined);
    let mut next = 0usize;
    for (i, frame) in frames.iter().enumerate() {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled_slots(&mut out, &jit[next]);
            next += 1;
        }
        match osr_bci.get(&i) {
            Some((bci, chain)) => {
                out.push(TraceSlot::Interp { frame, bci: *bci });
                push_inlined_slots(&mut out, chain, None);
            }
            None => out.push(TraceSlot::Interp {
                frame,
                bci: frame.last_instr_pc.min(i32::MAX as usize) as i32,
            }),
        }
    }
    for slot in &jit[next..] {
        push_compiled_slots(&mut out, slot);
    }
    out
}

/// [`push_compiled_frames_without_store`]: no slot, and none for its chain,
/// when the label does not parse.
fn push_compiled_slots<'a>(out: &mut Vec<TraceSlot<'a>>, slot: &'a ActiveCompiledFrame) {
    if split_frame_label(&slot.label).is_none() {
        return;
    }
    out.push(TraceSlot::Compiled(slot));
    push_inlined_slots(out, &slot.inline_chain, slot.compile_cp_stamp);
}

/// [`push_inlined_chain_without_store`]: outermost level first, and the first
/// level that cannot be named ends the chain. `cp_stamp` is the inlining
/// activation's compile stamp.
fn push_inlined_slots<'a>(
    out: &mut Vec<TraceSlot<'a>>,
    chain: &'a [InlinedLevel],
    cp_stamp: Option<u64>,
) {
    const MAX_CODE_LENGTH: u32 = 65_536;
    for level in chain.iter().rev() {
        if level.bci >= MAX_CODE_LENGTH || split_frame_label(&level.label).is_none() {
            break;
        }
        out.push(TraceSlot::Inlined(level, cp_stamp));
    }
}

fn capture_full_trace_with_lines(
    class_store: &ClassStore,
    frames: &[Frame],
    resolve_lines: bool,
) -> Vec<StackTraceEntry> {
    let jit = crate::jit::conservative_roots::active_compiled_frames();
    if jit.is_empty() {
        return frames
            .iter()
            .map(|f| entry_from_frame_with_lines(class_store, f, resolve_lines))
            .collect();
    }
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    interleave_compiled_frames(class_store, frames, &jit, &osr_bci, resolve_lines)
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
    let keep = throwable_fill_frame_keep_len(trace, &mut |holder, holder_name| {
        throwable_holder_is_a(class_store, tclass, holder, holder_name)
    });
    trace.truncate(keep);
}

/// How many of `items` (OUTERMOST-first) survive [`trim_throwable_fill_frames`]'s
/// two phases. The single decision both the entry-level trim above and the
/// slot-level pre-trim in [`capture_throwable_trace_without_store`] make, so the
/// two cannot drift.
///
/// `is_a(holder, holder_name)` answers `throwable->is_a(holder)`; it is asked
/// only for a frame whose method name already matched, so a caller can defer
/// any lock it needs until then.
fn throwable_fill_frame_keep_len<T: FillFrameIdentity>(
    items: &[T],
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
) -> usize {
    let mut keep = items.len();
    for phase in ["fillInStackTrace", "<init>"] {
        while let Some(item) = keep.checked_sub(1).map(|i| &items[i]) {
            match item.fill_identity() {
                Some((method, holder, holder_name))
                    if method == phase && is_a(holder, holder_name) =>
                {
                    keep -= 1
                }
                _ => break,
            }
        }
    }
    keep
}

/// `throwable->is_a(holder)` for [`throwable_fill_frame_keep_len`].
///
/// Prefer the frame's own `ClassId` — the loader-faithful answer; fall back to
/// the loader-blind name walk only for a synthesized entry that carries no id
/// (see `StackTraceEntry::class_id`).
///
/// The id arm first walks the throwable's own superclass chain. Every frame the
/// trim removes is a constructor or `fillInStackTrace` of one of those classes,
/// so the chain answers the common case without `is_subclass_of`'s visited
/// set -- which `ArrayIndexOutOfBoundsException` used to pay up to six times
/// per capture, each re-walking the same four supertypes. A miss is not a "no":
/// it falls through to the full walk, which also covers interfaces, so the
/// answer is unchanged either way.
pub fn throwable_holder_is_a(
    class_store: &ClassStore,
    tclass: &Class,
    holder: Option<ClassId>,
    holder_name: &str,
) -> bool {
    let Some(holder) = holder else {
        return tclass.is_subclass_of_by_name(holder_name, class_store);
    };
    let mut cursor = Some(tclass.id);
    let mut steps = 0usize;
    while let Some(cid) = cursor {
        if cid == holder {
            return true;
        }
        steps += 1;
        if steps > 64 {
            break;
        }
        cursor = class_store.get(cid).and_then(|c| c.superclass);
    }
    tclass.is_subclass_of(holder, class_store)
}

/// The `(method name, holder id, holder name)` a fill-frame trim decides on.
/// `None` means "not a frame the trim may remove".
trait FillFrameIdentity {
    fn fill_identity(&self) -> Option<(&str, Option<ClassId>, &str)>;
}

impl FillFrameIdentity for StackTraceEntry {
    fn fill_identity(&self) -> Option<(&str, Option<ClassId>, &str)> {
        Some((&self.method_name, self.class_id, &self.class_name))
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
pub(crate) fn drop_osr_continuations(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
) -> (Vec<ActiveCompiledFrame>, OsrBciOverrides) {
    if !osr_frame_dedupe_enabled() {
        return (jit, OsrBciOverrides::default());
    }
    let call_dedupe = call_frame_dedupe_enabled();
    // The `(depth, label)` pairs that have already had their one body entry
    // removed, by EITHER rule. See "How the two rules compose" above for why
    // this is shared rather than one ledger per rule.
    let mut deduped: Vec<(u32, Arc<str>)> = Vec::new();
    let mut overrides = OsrBciOverrides::default();
    // An owning loop rather than `filter(|f| ..)` over borrowed entries: an
    // entry this drops gives its `label` to the ledger and its `inline_chain`
    // to the overrides by MOVE. Cloning them was one allocation per inlined
    // level on every capture made from inside an OSR'd loop -- for
    // `new SomeException(msg)` that is the whole `<init>` chain, which the
    // fill-frame trim then discards anyway.
    let mut kept = Vec::with_capacity(jit.len());
    for f in jit {
        // `None`: keep. `Some(i)`: drop -- rule 1 when `i` names the frame the
        // continuation belongs to, rule 2 otherwise (see the two arms).
        let verdict: Option<Option<usize>> = 'decide: {
            // The frame this entry was pushed FROM. An OSR continuation was
            // pushed from the very frame it continues, so that frame is still
            // there and names the same method.
            let Some(i) = (f.interp_depth as usize).checked_sub(1) else {
                break 'decide None;
            };
            let Some(frame) = frames.get(i) else {
                break 'decide None;
            };
            if !label_names_frame(&f.label, frame) {
                break 'decide None;
            }
            if f.cm_ptr == 0 {
                break 'decide None;
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
                    break 'decide None;
                }
                cratonvm_jit::note_stack_walk_dedupe(if authoritative {
                    cratonvm_jit::DEDUPE_OSR_AUTHORITATIVE
                } else {
                    cratonvm_jit::DEDUPE_OSR_HEURISTIC
                });
                // The half that knows where control is, handed to the frame
                // that survives -- but ONLY on the authoritative arm. See "The
                // overrides" above.
                break 'decide Some((authoritative && f.bci >= 0).then_some(i));
            }
            if authoritative {
                // The registry named a DIFFERENT artifact as this frame's
                // body, so this entry is a distinct activation nested under
                // it, and rule 2's premise does not reach it. See the doc.
                break 'decide None;
            }
            // Rule 2 — the ordinary compiled activation. A frame suspended at
            // an invoke is a CALLER and both activations are real; anything
            // else cannot have called this method and is therefore its own
            // body running compiled.
            if !call_dedupe || frame_is_suspended_at_invoke(frame) {
                break 'decide None;
            }
            if already_deduped(&deduped, f.interp_depth, &f.label) {
                break 'decide None;
            }
            cratonvm_jit::note_stack_walk_dedupe(cratonvm_jit::DEDUPE_CALL_OPCODE);
            Some(None)
        };
        match verdict {
            None => kept.push(f),
            Some(override_for) => {
                if let Some(i) = override_for {
                    overrides.insert(i, (f.bci, f.inline_chain));
                }
                deduped.push((f.interp_depth, f.label));
            }
        }
    }
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
/// cover both.
///
/// The bci overrides it computes are RETURNED, not discarded. They used to be
/// dropped here on the argument that they re-point an interpreter frame the
/// LIVE capture is about to emit and this path emits none. But the frame they
/// re-point was already emitted -- by the late capture this snapshot is being
/// spliced onto -- and at its stale `pc`, because the compiled half that knew
/// better had unwound by then. Dropping the override printed the back-edge the
/// method tiered up at instead of the call it was suspended in:
///
/// ```text
/// interpreter oracle  after_main_osr  [leaf:54 mid:55 outer:56 probe:71 main:95]
/// overrides dropped                   [leaf:54 mid:55 outer:56 probe:71 main:91]
/// ```
///
/// measured on `tools/probes/StackTraceAfterOsr.java`, 2026-09-23, in both
/// compatibility modes. [`apply_snapshot_osr_overrides`] hands them to the
/// captured trace.
///
/// `frames` is the thread's frame stack at CONSTRUCTION time, not at the trap.
/// The two agree for every shape this can be reached in — a frame popped
/// between the two would have taken its compiled entry with it — and the rule
/// fails in the safe direction anyway: a depth that no longer names a matching
/// frame KEEPS the compiled entry.
pub(crate) fn dedupe_compiled_snapshot(
    frames: &[Frame],
    snapshot: Vec<ActiveCompiledFrame>,
) -> (Vec<ActiveCompiledFrame>, OsrBciOverrides) {
    let snapshot = drop_still_live(frames, snapshot);
    drop_osr_continuations(frames, snapshot)
}

/// Remove the snapshotted compiled frames that cannot have unwound: those an
/// interpreter frame in `frames` still sits above.
///
/// A compiled activation recorded at `interp_depth == d` is placed below
/// interpreter frame `d` (see `interleave_compiled_frames`). If that frame
/// still exists when the throwable is constructed, the compiled activation
/// under it is still on the machine stack too, so the late capture already
/// walked it live -- with the same bci, since it has not moved since it made
/// the call it is suspended in. Splicing its snapshot copy on as well printed it
/// twice, measured on tools/probes/StackTraceAfterOsr.java under `--jdk-only`
/// with `CRATONVM_BG_COMPILE=0`, where `main` runs as a method-entry compiled
/// frame at depth 0 below three interpreted callers:
///
/// ```text
/// interpreter oracle  after_helper_warm  [leaf:54 mid:55 outer:56 probe:71 main:87]
/// splice unfiltered                      [leaf:54 mid:55 main:87 outer:56 probe:71 main:87]
/// ```
///
/// [`trailing_overlap`] cannot catch it: that finds the snapshot's frames at
/// the inner END of the capture, which is the shape only when every compiled
/// frame was live, and a still-live frame here sits at the OUTER end with the
/// interpreter frames above it in between.
///
/// Only frames at `d >= frames.len()` -- above the innermost interpreter frame
/// -- are the ones the snapshot exists to recover. An OSR continuation of the
/// innermost frame is recorded at exactly `frames.len()`, so it survives this
/// and still reaches [`drop_osr_continuations`] for its bci override.
fn drop_still_live(
    frames: &[Frame],
    snapshot: Vec<ActiveCompiledFrame>,
) -> Vec<ActiveCompiledFrame> {
    let innermost_interp = frames.len();
    if snapshot
        .iter()
        .all(|f| f.interp_depth as usize >= innermost_interp)
    {
        return snapshot;
    }
    snapshot
        .into_iter()
        .filter(|f| f.interp_depth as usize >= innermost_interp)
        .collect()
}

/// Apply the OSR bci overrides [`dedupe_compiled_snapshot`] returned to a trace
/// that was captured AFTER the compiled frames had unwound.
///
/// Each override names an interpreter frame by its index into `frames`. The
/// trace does not record which entry came from which frame, so the entry is
/// found by walking both lists outermost-first together: frame `i`'s entry is
/// the first one at or after frame `i - 1`'s whose class, method, descriptor
/// and bci are the frame's own, `last_instr_pc` being the bci the capture
/// reports for an interpreter frame. The walk is in order, so recursion lines
/// each frame up with its own entry rather than with the first entry of the
/// same method.
///
/// The entry is re-pointed at the compiled half's bci, and the callees that
/// half had inlined at that point go immediately after it -- exactly what
/// `interleave_compiled_frames` does with the same override on the live path.
///
/// Fails in the safe direction. A frame whose entry cannot be found ends the
/// walk, and every override from there on is left unapplied, which is the
/// historical trace: a stale line, never a moved one.
pub(crate) fn apply_snapshot_osr_overrides(
    class_store: &ClassStore,
    frames: &[Frame],
    overrides: &OsrBciOverrides,
    trace: &mut Vec<StackTraceEntry>,
) {
    if overrides.is_empty() {
        return;
    }
    let last = overrides.keys().copied().max().unwrap_or(0);
    let mut cursor = 0usize;
    for (i, frame) in frames.iter().enumerate().take(last + 1) {
        let bci = frame.last_instr_pc.min(i32::MAX as usize) as i32;
        let Some(k) = trace[cursor..]
            .iter()
            .position(|e| {
                e.byte_code_index == bci
                    && *e.method_name == *frame.method_name()
                    && *e.class_name == *frame.class_name()
                    && e.method_descriptor.as_deref() == Some(&*frame.method_descriptor())
            })
            .map(|k| cursor + k)
        else {
            return;
        };
        cursor = k + 1;
        let Some((bci, chain)) = overrides.get(&i) else {
            continue;
        };
        let entry = &mut trace[k];
        entry.byte_code_index = *bci;
        entry.line_number = LINE_NUMBER_UNKNOWN;
        resolve_line_numbers_in_place(class_store, std::slice::from_mut(entry));
        let mut inlined = Vec::with_capacity(chain.len());
        push_inlined_chain(&mut inlined, class_store, chain, true);
        cursor += inlined.len();
        trace.splice(k + 1..k + 1, inlined);
    }
}

/// Display-only replacements for one interpreter frame's own `last_instr_pc`,
/// keyed by index into `frames`: the bytecode index of the COMPILED half of
/// that activation, and the callees that half had inlined at the very same
/// program point.
///
/// Produced by [`drop_osr_continuations`] — see "The overrides" there for why
/// this is passed to the trace assembler rather than written into
/// `Frame::pc`.
pub(crate) type OsrBciOverrides = rustc_hash::FxHashMap<usize, (i32, Vec<InlinedLevel>)>;

/// Has this `(depth, label)` already had its one body entry removed by either
/// rule of [`drop_osr_continuations`]?
///
/// A linear scan of a `Vec` rather than a set: the ledger holds one element
/// per method with BOTH halves live at one depth, which is zero or one on
/// every trace measured, and hashing the label would cost more than the
/// compare it replaces.
fn already_deduped(seen: &[(u32, Arc<str>)], depth: u32, label: &str) -> bool {
    seen.iter().any(|(d, l)| *d == depth && &**l == label)
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
    resolve_lines: bool,
) -> Vec<StackTraceEntry> {
    let mut out = Vec::with_capacity(frames.len() + jit.len());
    let mut next = 0usize;
    for (i, f) in frames.iter().enumerate() {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled_frames(&mut out, class_store, &jit[next], resolve_lines);
            next += 1;
        }
        let mut entry = entry_from_frame_with_lines(class_store, f, resolve_lines);
        match osr_bci.get(&i) {
            Some((bci, chain)) => {
                reline_entry(class_store, &mut entry, *bci, resolve_lines);
                out.push(entry);
                // The callees inlined into the compiled half of THIS frame's
                // own activation, which left with the entry
                // `drop_osr_continuations` removed. Same direction as
                // `push_compiled_frames`, and for the same reason.
                push_inlined_chain(&mut out, class_store, chain, resolve_lines);
            }
            None => out.push(entry),
        }
    }
    for slot in &jit[next..] {
        push_compiled_frames(&mut out, class_store, slot, resolve_lines);
    }
    out
}

/// [`interleave_compiled_frames`] without source/line resolution.
///
/// This is the Throwable construction path. It reads no global class metadata:
/// every identity value comes from either a live interpreter frame or the JIT
/// artifact label that produced the compiled activation.
///
/// `positions`, when given, receives each interpreter frame's index in the
/// result, in frame order (the published trace's per-frame monitor
/// attribution needs it, [`capture_published_trace`]).
fn interleave_compiled_frames_without_store(
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    mut positions: Option<&mut Vec<u32>>,
) -> Vec<StackTraceEntry> {
    // Sized for the inlined levels too: they are most of a trace captured
    // under a spliced `<init>` chain, and growing past `frames + jit` cost a
    // reallocation and a full copy of the entries on every such capture.
    let inlined = jit.iter().map(|f| f.inline_chain.len()).sum::<usize>()
        + osr_bci.values().map(|(_, chain)| chain.len()).sum::<usize>();
    let mut out = Vec::with_capacity(frames.len() + jit.len() + inlined);
    let mut next = 0usize;
    for (i, frame) in frames.iter().enumerate() {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled_frames_without_store(&mut out, &jit[next]);
            next += 1;
        }
        let bci = frame.last_instr_pc.min(i32::MAX as usize) as i32;
        let mut entry = StackTraceEntry {
            class_name: frame.class_name_arc(),
            method_name: frame.method_name_arc(),
            method_descriptor: Some(frame.method_descriptor_arc()),
            source_file: frame.source_file_arc(),
            line_number: frame.own_line_number(bci).unwrap_or(LINE_NUMBER_UNKNOWN),
            byte_code_index: bci,
            class_id: Some(frame.class_id),
            method_index: None,
        };
        if let Some(positions) = positions.as_deref_mut() {
            // Cast: a trace index, bounded by the frame count plus the compiled
            // and inlined levels; far below u32::MAX.
            positions.push(out.len() as u32);
        }
        if let Some((bci, chain)) = osr_bci.get(&i) {
            entry.byte_code_index = *bci;
            out.push(entry);
            push_inlined_chain_without_store(&mut out, chain);
        } else {
            out.push(entry);
        }
    }
    for slot in &jit[next..] {
        push_compiled_frames_without_store(&mut out, slot);
    }
    out
}

fn push_compiled_frames_without_store(out: &mut Vec<StackTraceEntry>, slot: &ActiveCompiledFrame) {
    let Some(entry) = compiled_frame_entry_without_store(slot) else {
        return;
    };
    out.push(entry);
    push_inlined_chain_without_store(out, &slot.inline_chain);
}

fn push_inlined_chain_without_store(out: &mut Vec<StackTraceEntry>, chain: &[InlinedLevel]) {
    for level in chain.iter().rev() {
        match inlined_frame_entry_without_store(level) {
            Some(entry) => out.push(entry),
            None => break,
        }
    }
}

/// The three `Arc<str>` a compiled-frame label splits into.
type ParsedFrameLabel = (Arc<str>, Arc<str>, Arc<str>);

/// Upper bound on [`PARSED_FRAME_LABELS`]; the memo is simply cleared when it
/// fills. Distinct labels are distinct compiled or inlined METHODS, so a thread
/// that captures traces through more than this many is not a hot loop.
const PARSED_FRAME_LABEL_CAP: usize = 4096;

thread_local! {
    /// Label -> its split `(class, method, descriptor)`, shared `Arc`s.
    ///
    /// A throwable built in a compiled method reaches here once per compiled
    /// activation AND once per level the JIT inlined at that point — for
    /// `new SomeException(msg)` that is the whole `<init>` chain, five levels
    /// for `ArrayIndexOutOfBoundsException`, which the fill-frame trim then
    /// drops. Splitting and minting three fresh `Arc<str>` for each of them on
    /// every capture was three allocations per frame for text that never
    /// changes. A pure function of the label, so a hit is exactly the parse.
    ///
    /// The same row carries the label's hidden-frame decision
    /// ([`compiled_label_is_hidden`], interpreter round i1 wave 37), so a warm
    /// capture through a compiled JDK frame judges it without a method scan.
    static PARSED_FRAME_LABELS: std::cell::RefCell<
        rustc_hash::FxHashMap<Box<str>, FrameLabelMemo>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// One row of [`PARSED_FRAME_LABELS`].
struct FrameLabelMemo {
    /// The label's split; a pure function of the label.
    parsed: Option<ParsedFrameLabel>,
    /// The last [`compiled_label_is_hidden`] answer for this label:
    /// `(class store address, class id, hidden)`. Trusted only for the same
    /// store (one per VM, so a second VM on this thread recomputes) and the
    /// same class id; `None` until a capture asked.
    hidden: Option<(usize, u32, bool)>,
}

fn parse_frame_label_uncached(label: &str) -> Option<ParsedFrameLabel> {
    let (owner_and_method, descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() {
        return None;
    }
    Some((
        Arc::from(class_name),
        Arc::from(method_name),
        Arc::from(descriptor),
    ))
}

fn parse_frame_label(label: &str) -> Option<ParsedFrameLabel> {
    PARSED_FRAME_LABELS
        .try_with(|memo| {
            let Ok(mut memo) = memo.try_borrow_mut() else {
                return parse_frame_label_uncached(label);
            };
            if let Some(hit) = memo.get(label) {
                return hit.parsed.clone();
            }
            if memo.len() >= PARSED_FRAME_LABEL_CAP {
                memo.clear();
            }
            let parsed = parse_frame_label_uncached(label);
            memo.insert(
                Box::from(label),
                FrameLabelMemo {
                    parsed: parsed.clone(),
                    hidden: None,
                },
            );
            parsed
        })
        .unwrap_or_else(|_| parse_frame_label_uncached(label))
}

/// Can the compiled or inlined frame `label` (`"class/Name.method:desc"`) be
/// hidden at all? A superset of [`entry_may_be_hidden`] of its class, read
/// from the label without splitting it (a JDK package prefix, or a `/0x`
/// anywhere), so a frame of an application class never reaches the class
/// store or the memo and costs no parse per capture.
fn label_may_be_hidden(label: &str) -> bool {
    label.starts_with("java/")
        || label.starts_with("jdk/")
        || label.starts_with("sun/")
        || label.contains("/0x")
}

/// `Method::is_hidden` for a COMPILED activation or an INLINED level: the
/// method `label` names, in class `class_id` (the artifact's owner, or the id
/// the resolver recorded for the spliced callee; an inlined level's `0` means
/// none was recorded, and the caller keeps such a frame without asking).
/// Interpreter round i1 wave 37: waves 34-35
/// judged only interpreter frames, so a hot hidden method (a hidden class's
/// method, the `@Hidden` `ScopedValue$Carrier.runWith` inlined into `call`)
/// showed in the trace once it was compiled.
///
/// Memoized per label on this thread ([`FrameLabelMemo::hidden`]): a compiled
/// frame has no `CachedBytecodeMethod` to hold the answer, and a JDK class's
/// answer otherwise costs a method scan per frame per capture. The row is
/// keyed by the store's address and the class id as well; a store address
/// reused by a later VM with the same class id and label is the same JDK
/// method or the same kind of hidden class, whose answer does not change.
fn compiled_label_is_hidden(store: &ClassStore, label: &str, class_id: u32) -> bool {
    if !label_may_be_hidden(label) {
        return false;
    }
    let store_key = store as *const ClassStore as usize;
    PARSED_FRAME_LABELS
        .try_with(|memo| {
            let Ok(mut memo) = memo.try_borrow_mut() else {
                return compiled_label_is_hidden_uncached(store, label, class_id);
            };
            if let Some(FrameLabelMemo {
                hidden: Some((memo_store, memo_class, hidden)),
                ..
            }) = memo.get(label)
            {
                if *memo_store == store_key && *memo_class == class_id {
                    return *hidden;
                }
            }
            let hidden = compiled_label_is_hidden_uncached(store, label, class_id);
            if let Some(row) = memo.get_mut(label) {
                row.hidden = Some((store_key, class_id, hidden));
            } else {
                if memo.len() >= PARSED_FRAME_LABEL_CAP {
                    memo.clear();
                }
                memo.insert(
                    Box::from(label),
                    FrameLabelMemo {
                        parsed: parse_frame_label_uncached(label),
                        hidden: Some((store_key, class_id, hidden)),
                    },
                );
            }
            hidden
        })
        .unwrap_or_else(|_| compiled_label_is_hidden_uncached(store, label, class_id))
}

/// [`compiled_label_is_hidden`] without the memo.
fn compiled_label_is_hidden_uncached(store: &ClassStore, label: &str, class_id: u32) -> bool {
    let Some((owner_and_method, descriptor)) = label.rsplit_once(':') else {
        return false;
    };
    let Some((_, method_name)) = owner_and_method.rsplit_once('.') else {
        return false;
    };
    method_is_hidden(
        store,
        ClassId::new(class_id),
        method_name,
        Some(descriptor),
        None,
    )
}

fn frame_entry_from_label_without_store(
    label: &str,
    class_id: Option<ClassId>,
    bci: i32,
) -> Option<StackTraceEntry> {
    let (class_name, method_name, descriptor) = parse_frame_label(label)?;
    Some(StackTraceEntry {
        class_name,
        method_name,
        method_descriptor: Some(descriptor),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id,
        method_index: None,
    })
}

fn compiled_frame_entry_without_store(frame: &ActiveCompiledFrame) -> Option<StackTraceEntry> {
    frame_entry_from_label_without_store(
        &frame.label,
        Some(ClassId::new(frame.owner_class_id)),
        frame.bci,
    )
}

fn inlined_frame_entry_without_store(level: &InlinedLevel) -> Option<StackTraceEntry> {
    const MAX_CODE_LENGTH: u32 = 65_536;
    if level.bci >= MAX_CODE_LENGTH {
        return None;
    }
    frame_entry_from_label_without_store(
        &level.label,
        (level.class_id != 0).then(|| ClassId::new(level.class_id)),
        level.bci as i32,
    )
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
    resolve_lines: bool,
) {
    let Some(enclosing) = compiled_frame_entry(class_store, slot, resolve_lines) else {
        return;
    };
    out.push(enclosing);
    push_inlined_chain(out, class_store, &slot.inline_chain, resolve_lines);
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
    resolve_lines: bool,
) {
    // The chain is innermost-first; `out` is outermost-first.
    for level in chain.iter().rev() {
        match inlined_frame_entry(class_store, level, resolve_lines) {
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
fn inlined_frame_entry(
    class_store: &ClassStore,
    level: &InlinedLevel,
    resolve_lines: bool,
) -> Option<StackTraceEntry> {
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
            let line_number = resolve_lines
                .then(|| {
                    method_index
                        .and_then(|i| c.methods.get(i as usize))
                        .and_then(|m| line_number_for_bci_in_method(m, bci as usize))
                })
                .flatten()
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
        method_descriptor: Some(Arc::from(method_descriptor)),
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
///
/// `drop_hidden` (`--jdk-only`, interpreter round i1 wave 37) leaves the
/// snapshot's hidden frames out ([`stack_entry_is_hidden`]) BEFORE the
/// overlap merge, as the construction-time capture left them out of `trace`:
/// merging an unfiltered snapshot onto a filtered trace would both show the
/// hidden frame and defeat the overlap (a hidden frame between two live ones
/// makes the two lists disagree), printing the live ones twice.
pub fn append_snapshotted_compiled_frames(
    class_store: &ClassStore,
    snapshot: &[ActiveCompiledFrame],
    mut trace: Vec<StackTraceEntry>,
    drop_hidden: bool,
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
        push_compiled_frames(&mut fresh, class_store, f, false);
    }
    if drop_hidden {
        let dbg = crate::runtime::env_cache::dbg_sttrace();
        fresh.retain(|e| {
            let hidden = stack_entry_is_hidden(class_store, e);
            if hidden && dbg {
                eprintln!(
                    "STTRACE_DBG_HIDDEN kind=trap-splice {}.{}",
                    e.class_name, e.method_name
                );
            }
            !hidden
        });
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
/// Since round 12 wave 4 that door hands the snapshot to the capture itself
/// ([`arm_trap_capture`]) whenever the snapshot can stand in for the walk, and
/// then splices nothing; this merge serves the shapes and doors that cannot.
/// The overlap cannot repair a still-live frame the walk read at a different
/// bci than the snapshot (a same-frame compiled `catch`): that pair compares
/// unequal and the frame would print twice, which is one reason the door
/// prefers the one-walk route.
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
fn reline_entry(
    class_store: &ClassStore,
    entry: &mut StackTraceEntry,
    bci: i32,
    resolve_lines: bool,
) {
    entry.byte_code_index = bci;
    entry.line_number = if resolve_lines {
        entry
            .class_id
            .and_then(|cid| class_store.get(cid))
            .zip(entry.method_index)
            .and_then(|(class, idx)| class.methods.get(idx as usize))
            .and_then(|m| line_number_for_bci_in_method(m, bci.max(0) as usize))
            .unwrap_or(LINE_NUMBER_UNKNOWN)
    } else {
        LINE_NUMBER_UNKNOWN
    };
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
    resolve_lines: bool,
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
    let line_number = if resolve_lines && *bci >= 0 {
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
        method_descriptor: Some(std::sync::Arc::from(method_descriptor)),
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
/// number is left [`LINE_NUMBER_UNKNOWN`], except for a frame running a body a
/// redefinition replaced, which carries its own lines (`Frame::own_line_number`).
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
            method_descriptor: None,
            source_file: f.source_file_arc(),
            line_number: f
                .own_line_number(f.last_instr_pc.min(i32::MAX as usize) as i32)
                .unwrap_or(LINE_NUMBER_UNKNOWN),
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

/// The trace a thread PUBLISHES for other threads (`JvmThread::frame_trace`,
/// read by another thread's `Thread.getStackTrace()`, `getAllStackTraces()`
/// and thread dumps through `ThreadRegistry::frame_trace_of_resolved`), with
/// the thread's compiled activations interleaved -- the same frames an
/// own-thread capture shows. Runs on the publishing thread itself (its JIT
/// activation chain is thread-local), at the blocking deposit and at a
/// safepoint a cross-thread read asked for.
///
/// It used to be [`capture_frames_no_lines`] alone, so a thread parked under
/// compiled callers published its park frames and the interpreter frames
/// around them with every compiled method between them missing (interpreter
/// round i1 wave 38, lane L7;
/// `docs/internal/fixed-bugs/interpreter-L6-another-threads-trace-omits-compiled-frames-FIXED-20261002.md`).
/// A thread that entered no compiled code has an empty chain and gets exactly
/// the entries it got before.
///
/// `CRATONVM_DBG_STTRACE=1` prints one `[sttrace] publish:` line per capture
/// that had compiled activations.
///
/// A compiled activation of a body compiled before its class's redefinition
/// (the obsolete activation JEP 109 keeps running) gets the line its OWN
/// body's table gives its bci, here, as a `Throwable`'s retained compiled
/// frame does ([`publish_compiled_lines_at_their_stamps`]; interpreter round
/// i1 wave 40, lane L3). Before, its entry carried no line and the reader
/// resolved it from the class as it is then: the edited body's table.
///
/// `reflective_calls` / `reflective_named` are the thread's
/// `JvmThread::reflective_calls` and `reflective_frames_named`: the JDK frames
/// of each reflective call the thread is inside go between the caller and the
/// target, as the thread's own capture lists them
/// ([`splice_published_reflective_frames`]; interpreter round i1 wave 45,
/// lane L3). A thread outside every reflective call pays one `is_empty`.
pub fn capture_published_trace(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    reflective_calls: &[ReflectiveCallRow],
    reflective_named: &ReflectiveFramesNamed,
) -> PublishedTrace {
    let jit = crate::jit::conservative_roots::active_compiled_frames();
    if jit.is_empty() {
        let mut trace = PublishedTrace {
            entries: capture_frames_no_lines(frames),
            frame_positions: None,
        };
        if !reflective_calls.is_empty() {
            splice_published_reflective_frames(
                frames,
                &jit,
                &OsrBciOverrides::default(),
                reflective_calls,
                reflective_named,
                cratonvm_classloading::class_redefinition_count(),
                &mut trace,
            );
        }
        return trace;
    }
    let activations = jit.len();
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    let mut positions = Vec::with_capacity(frames.len());
    let mut entries =
        interleave_compiled_frames_without_store(frames, &jit, &osr_bci, Some(&mut positions));
    if cratonvm_classloading::class_redefinition_count() != 0 {
        publish_compiled_lines_at_their_stamps(shared, frames, &jit, &osr_bci, &mut entries);
    }
    if crate::runtime::env_cache::dbg_sttrace() {
        eprintln!(
            "[sttrace] publish: interpreted={} compiled-activations={} entries={}",
            frames.len(),
            activations,
            entries.len()
        );
    }
    let mut trace = PublishedTrace {
        entries,
        frame_positions: Some(positions),
    };
    if !reflective_calls.is_empty() {
        splice_published_reflective_frames(
            frames,
            &jit,
            &osr_bci,
            reflective_calls,
            reflective_named,
            cratonvm_classloading::class_redefinition_count(),
            &mut trace,
        );
    }
    trace
}

/// The JDK frames of `calls` (the publishing thread's reflective calls) in
/// `trace`, a published capture of `frames` laid out as
/// [`collect_trace_slots`] lays out `frames` / `jit` / `osr_bci` (one entry per
/// slot), each at [`trace_anchor_position`] -- the placement the thread's own
/// capture uses -- and `trace.frame_positions` shifted to match, so the
/// per-frame monitor attribution (`publish_jmx_frame_monitors`) keeps naming
/// the right rows. `count` is the class-redefinition count now: frames named
/// at another count are stale and left out.
///
/// The frames come from `named` ONLY ([`reflective_splices_memoized`]): a
/// publish may run while the thread holds the class-manager lock (a blocking
/// deposit inside class loading), so it names nothing itself. The native
/// entering a call names them ([`name_reflective_frames_if_stale`]), so a call
/// is left out only when that found the lock busy, or they cannot be named.
/// A call that pushed nothing yet is left out (a thread parked in the native
/// itself). Interpreter round i1 wave 45, lane L3 (the proposal
/// `docs/internal/fixed-bugs/interpreter-L3-proposal-published-traces-carry-the-reflective-calls-FIXED-20261009.md`);
/// `CRATONVM_DBG_STTRACE=1` prints one `[sttrace] publish-reflect:` line per
/// publish that listed a call.
fn splice_published_reflective_frames(
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    calls: &[ReflectiveCallRow],
    named: &ReflectiveFramesNamed,
    count: u64,
    trace: &mut PublishedTrace,
) {
    let splices = reflective_splices_memoized(named, calls, count);
    if splices.is_empty() {
        return;
    }
    let slots = collect_trace_slots(frames, jit, osr_bci);
    if slots.len() != trace.entries.len() {
        return;
    }
    let positions = reflective_splice_positions(&slots, &splices, false);
    if positions.is_empty() {
        return;
    }
    // Each interpreter frame's entry index, before the splice.
    let before: Vec<u32> = match trace.frame_positions.take() {
        Some(p) => p,
        None => (0..frames.len())
            .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
            .collect(),
    };
    // After it: grown by the frames spliced at or before each entry (a call's
    // frames go right BEFORE the entry at its position).
    let mut after = Vec::with_capacity(before.len());
    let mut shift = 0usize;
    let mut next = positions.iter().peekable();
    for &p in &before {
        while let Some(&&(position, k)) = next.peek() {
            if position > p as usize {
                break;
            }
            shift += splices[k].entries.len();
            next.next();
        }
        after.push(u32::try_from(p as usize + shift).unwrap_or(u32::MAX));
    }
    let mut listed = 0usize;
    // From the innermost, so an earlier position still indexes the entry it
    // named; calls at one position keep their order, the outer one first.
    for &(position, k) in positions.iter().rev() {
        let inner = trace.entries.split_off(position);
        trace.entries.extend(splices[k].entries.iter().cloned());
        trace.entries.extend(inner);
        listed += 1;
    }
    trace.frame_positions = Some(after);
    if crate::runtime::env_cache::dbg_sttrace() {
        eprintln!(
            "[sttrace] publish-reflect: calls={} listed={listed} entries={}",
            calls.len(),
            trace.entries.len()
        );
    }
}

/// What [`capture_published_trace`] returns.
pub struct PublishedTrace {
    /// The trace, outermost frame first.
    pub entries: Vec<StackTraceEntry>,
    /// Each interpreter frame's index in `entries`, in frame order; `None`
    /// when there is one entry per frame (no compiled activation), so frame
    /// `i` is entry `i`. The per-frame locked-monitor attribution published
    /// with the trace (`ThreadRegistry::publish_jmx_frame_monitors`) is
    /// translated through it for a reader of the whole trace.
    pub frame_positions: Option<Vec<u32>>,
}

/// The lines of `entries` (a published trace, laid out as
/// [`collect_trace_slots`] lays out the same `frames` / `jit` / `osr_bci`)
/// whose compiled activation, or a callee it inlined, runs a body compiled
/// before its class's last redefinition: from the table that body's class
/// had at the compile stamp (`obsolete_frames::resolve_lines_as_captured`,
/// the `Throwable` path's resolver, with the stamps the stack walk's capture
/// also reads). Interpreter round i1 wave 40, lane L3
/// (`tools/probes/interp/L3/L3W40OtherThreadObsoleteLine.java`,
/// `docs/internal/fixed-bugs/interpreter-L3-another-threads-trace-gives-a-compiled-obsolete-activation-the-new-bodys-line-FIXED-20261004.md`).
///
/// HotSpot's thread dump / `Thread.getStackTrace()` of such an activation
/// names its old body's line, as a `Throwable` does. The reader
/// (`ThreadRegistry::frame_trace_of_resolved`) resolves an entry with no line
/// from the class as it is when read, which after the redefinition is the
/// edited body's table at the old body's bci; an entry resolved here keeps its
/// line.
///
/// Runs only once some class of the process was redefined and the thread has
/// compiled activations; takes the class-manager lock only when one of them
/// is older than the current redefinition count, and never waits for it (the
/// publishing thread may hold it): a busy lock leaves the entries as before.
fn publish_compiled_lines_at_their_stamps(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    entries: &mut [StackTraceEntry],
) {
    let now = cratonvm_classloading::class_redefinition_count();
    let slots = collect_trace_slots(frames, jit, osr_bci);
    if slots.len() != entries.len() {
        return;
    }
    let stamps: Vec<Option<u64>> = slots
        .iter()
        .map(|slot| match slot {
            TraceSlot::Interp { .. } => None,
            TraceSlot::Compiled(compiled) => compiled.compile_cp_stamp,
            TraceSlot::Inlined(_, stamp) => *stamp,
        })
        .collect();
    if !stamps.iter().flatten().any(|&stamp| stamp < now) {
        return;
    }
    let Some(cm) = shared.classes.class_manager.try_read() else {
        return;
    };
    crate::runtime::interpreter::obsolete_frames::resolve_lines_as_captured(
        &cm,
        now,
        entries,
        Some(stamps.as_slice()),
    );
}

/// Lock-free, exact-descriptor variant for Throwable construction.
///
/// Unlike the diagnostic deposit snapshot, a live `Frame` already carries its
/// descriptor. Retaining it avoids both a `ClassStore` read lock and the
/// per-signature memo lookup at construction time while still letting a reader
/// resolve an overloaded method exactly later.
pub fn capture_frames_no_lines_exact(frames: &[Frame]) -> Vec<StackTraceEntry> {
    frames
        .iter()
        .map(|f| StackTraceEntry {
            class_name: f.class_name_arc(),
            method_name: f.method_name_arc(),
            method_descriptor: Some(f.method_descriptor_arc()),
            source_file: f.source_file_arc(),
            line_number: f
                .own_line_number(f.last_instr_pc.min(i32::MAX as usize) as i32)
                .unwrap_or(LINE_NUMBER_UNKNOWN),
            byte_code_index: f.last_instr_pc.min(i32::MAX as usize) as i32,
            class_id: Some(f.class_id),
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
/// Throwable construction uses [`capture_full_trace_without_lines`]. Its
/// entries carry an exact `method_index`, and `SharedVm::throwable_stack_trace`
/// calls this resolver before exposing a Java-visible trace. Therefore an
/// overloaded method resolves to the same line as eager capture, while a
/// caught-and-discarded Throwable avoids the work altogether.
///
/// It is a strict *improvement* for [`capture_frames_no_lines`] consumers,
/// which have no line numbers at all otherwise.
pub fn resolve_line_numbers_in_place(
    class_store: &ClassStore,
    entries: &mut [StackTraceEntry],
) -> usize {
    let mut resolved = 0usize;
    for entry in entries.iter_mut() {
        if entry.byte_code_index < 0 {
            continue;
        }
        let Some(class_id) = entry.class_id else {
            continue;
        };
        let Some(class) = class_store.get(class_id) else {
            continue;
        };
        if entry.source_file.is_none() {
            entry.source_file = class.source_file.as_deref().map(Arc::from);
        }
        if entry.line_number != LINE_NUMBER_UNKNOWN {
            continue;
        }
        let name = &*entry.method_name;

        // Exact path: a captured descriptor avoids an eager class-store lookup
        // on the construction path. The legacy slot is the fallback for
        // entries captured before descriptors were retained.
        let mut method: Option<&ClassFileMethod> = entry
            .method_descriptor
            .as_deref()
            .and_then(|descriptor| class.find_method(name, descriptor));
        if method.is_none() {
            method = entry
                .method_index
                .and_then(|idx| class.methods.get(idx as usize))
                .filter(|m| &*m.name == name);
        }

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
        method_descriptor: None,
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

// ---------------------------------------------------------------------------
// Stand-in frames: the JDK frames a VM-implemented method runs without
// ---------------------------------------------------------------------------
//
// Round 13 wave 13 (lane trace3),
// `r13w12-orch-vm-implemented-jdk-methods-leave-no-stack-frames-FIXED-20260929.md`.
// A method the VM serves itself pushes no Java frame, so an exception raised
// inside it was captured with its CALLER on top. HotSpot, for an interrupted
// `Thread.sleep(10)` on JDK 25:
//
// ```text
// java.base/java.lang.Thread.sleepNanos0(Native Method)
// java.base/java.lang.Thread.sleepNanos(Thread.java:509)
// java.base/java.lang.Thread.sleep(Thread.java:540)
// Main.main(Main.java:3)
// ```
//
// `--compatible` (a registered `Thread.sleep`) recorded only `Main.main`;
// `--jdk-only` (real `sleep` and `sleepNanos`, native `sleepNanos0`) had no
// native leaf. The innermost captured frame names what was running: it stands
// at the invoke of the method the VM served. [`native_standin_frames`] rebuilds
// that method's frames from the real class bytes -- each Java level at its own
// call to the next (its `LineNumberTable` line), down to the `native` leaf
// (line -2) -- for the census below only, and only for the throwables HotSpot
// raises INSIDE that leaf. The throwable screen is what keeps an exception the
// invoke itself raised before entering (a null receiver's
// `NullPointerException` at `o.wait()`) on the caller, where HotSpot has it.

/// One JDK method [`native_standin_frames`] may rebuild a frame for.
struct StandinMethod {
    class: &'static str,
    name: &'static str,
    descriptor: &'static str,
    /// The throwables HotSpot raises from inside this method's native leaf.
    throws: &'static [&'static str],
    /// May the chain START here (the innermost captured frame calls it)?
    /// `false` for a row reached only as an inner hop, whose call site out
    /// is right only for the way that hop enters it (`Thread.join(long)`
    /// from `join()`: the `wait(0)` loop, not the timed one).
    entry: bool,
}

const STANDIN_SLEEP_THROWS: &[&str] = &["java/lang/InterruptedException"];
const STANDIN_JOIN_THROWS: &[&str] = &["java/lang/InterruptedException"];
const STANDIN_INTERRUPTED: &str = "java/lang/InterruptedException";

/// Round 14 wave 6 (lane trace5): the served timed joins, `Thread.join(J)V`
/// and `join(JI)V`: not entries of the census (the frames cannot tell their
/// `wait` loop), entries of [`timed_join_standin_frames`] (the native can).
fn is_served_timed_join_row(row: &StandinMethod) -> bool {
    row.class == "java/lang/Thread"
        && row.name == "join"
        && matches!(row.descriptor, "(J)V" | "(JI)V")
}
const STANDIN_WAIT_THROWS: &[&str] = &[
    "java/lang/InterruptedException",
    "java/lang/IllegalMonitorStateException",
];

/// The census. A chain is found in the real bytecode, not written down here,
/// so one table serves JDK 17 (`sleep(long)` and `wait(long)` are themselves
/// `native`), 21 (`sleep0`) and 25 (`sleepNanos` -> `sleepNanos0`, `wait0`).
const STANDIN_METHODS: &[StandinMethod] = &[
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(JI)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(Ljava/time/Duration;)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleepNanos",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleepNanos0",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep0",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/util/concurrent/TimeUnit",
        name: "sleep",
        descriptor: "(J)V",
        throws: STANDIN_SLEEP_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait",
        descriptor: "()V",
        throws: STANDIN_WAIT_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait",
        descriptor: "(J)V",
        throws: STANDIN_WAIT_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait",
        descriptor: "(JI)V",
        throws: STANDIN_WAIT_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait0",
        descriptor: "(J)V",
        throws: STANDIN_WAIT_THROWS,
        entry: true,
    },
    // Round 14 wave 4 (lane trace3; `r13w13-trace3-vm-served-jdk-methods-residuals`
    // item 1): a registered `join()` (`--compatible`). JDK 17/21/25 `join()` is
    // `join(0L)`, whose LAST `wait` call is the `millis == 0` loop's `wait(0)`
    // (line 1887 on JDK 25), then `Object.wait(long)` -> `wait0`. That call
    // names `java/lang/Thread.wait(J)V` (javac qualifies by the enclosing
    // class), which the owner resolution of `last_standin_call` walks up to
    // `Object`. `join(long)` is NOT an entry: a direct timed join HotSpot
    // leaves from its OTHER `wait(delay)` site (line 1881), which the frames
    // alone cannot tell apart. `CRATONVM_THROWABLE_STANDIN_JOIN_FRAMES=0` drops
    // both rows. Round 14 wave 6 (lane trace5): the served native tells it
    // (`timed_join_standin_frames`), and the `join(long)` hop picks its
    // `wait` by argument, not position ([`StandinScreen::join_waits_zero`]).
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "()V",
        throws: STANDIN_JOIN_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "(J)V",
        throws: STANDIN_JOIN_THROWS,
        entry: false,
    },
    // Round 14 wave 6 (lane trace5): the registered `join(long, int)` is
    // `join(millis)` after its checks (JDK 17/21/25), and the served native
    // runs the timed join, whose hint picks the `join(long)` hop's `wait`.
    // Reached only through `timed_join_standin_frames`.
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "(JI)V",
        throws: STANDIN_JOIN_THROWS,
        entry: false,
    },
    // Round 14 wave 5 (lane trace4): `Array.newInstance(Class, int)` is Java
    // in JDK 17-25 (`return newArray(componentType, length);`, line 76 on 25),
    // but a registered `Bridge` serves it in both modes
    // (`reflect_annotations::register_reflect_array_natives`), so a
    // `NegativeArraySizeException` had the caller on top. HotSpot:
    // `Array.newArray(Native Method)`, `Array.newInstance(Array.java:76)`.
    // The null / `void.class` component (NPE / IAE) is raised by `newArray`
    // too; the call is static, so no receiver NPE is in play.
    // `CRATONVM_THROWABLE_STANDIN_ARRAY_FRAMES=0` drops both rows.
    StandinMethod {
        class: "java/lang/reflect/Array",
        name: "newInstance",
        descriptor: "(Ljava/lang/Class;I)Ljava/lang/Object;",
        throws: STANDIN_NEW_ARRAY_THROWS,
        entry: true,
    },
    StandinMethod {
        class: "java/lang/reflect/Array",
        name: "newArray",
        descriptor: "(Ljava/lang/Class;I)Ljava/lang/Object;",
        throws: STANDIN_NEW_ARRAY_THROWS,
        entry: true,
    },
];

const STANDIN_NEW_ARRAY_THROWS: &[&str] = &[
    "java/lang/NegativeArraySizeException",
    "java/lang/NullPointerException",
    "java/lang/IllegalArgumentException",
];

/// Is `row` switched on? The `join` rows (round 14 wave 4) and the `Array`
/// rows (wave 5) have a switch of their own; it is read only once a call
/// matched one.
fn standin_row_enabled(row: &StandinMethod) -> bool {
    if row.name == "join" {
        return cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_STANDIN_JOIN_FRAMES",
        );
    }
    if row.class == "java/lang/reflect/Array" {
        return cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_STANDIN_ARRAY_FRAMES",
        );
    }
    true
}

/// Which rows a stand-in chain may use.
#[derive(Clone, Copy)]
enum StandinScreen<'a> {
    /// A throwable raised inside the leaf: the rows that raise it.
    Throwable(&'a str),
    /// A thread PARKED inside the leaf (another thread's published stack,
    /// round 14 wave 4 T4-3): every row -- the thread is in the leaf, so
    /// there is no throwable to screen by.
    Parked,
    /// A throwable raised inside the leaf on a VIRTUAL thread (round 14 wave
    /// 5, lane trace4): the `Object.wait` rows only, each hop at its FIRST
    /// census call. See [`virtual_thread_standin_frames`].
    VirtualThrowable(&'a str),
    /// An `InterruptedException` raised by a served DIRECT `Thread.join(long)`
    /// (round 14 wave 6, lane trace5, TR5-3), which the native says ran with a
    /// positive timeout (`timed`) or none. See [`timed_join_standin_frames`].
    TimedJoin { timed: bool },
}

impl StandinScreen<'_> {
    fn admits(self, row: &StandinMethod) -> bool {
        match self {
            Self::Throwable(throwable_class) => row.throws.contains(&throwable_class),
            Self::Parked => true,
            // The `Object.wait` rows, and the `Array` rows (no thread branch).
            Self::VirtualThrowable(throwable_class) => {
                matches!(row.class, "java/lang/Object" | "java/lang/reflect/Array")
                    && row.throws.contains(&throwable_class)
            }
            Self::TimedJoin { .. } => row.throws.contains(&STANDIN_INTERRUPTED),
        }
    }

    /// May a chain START at `row`? The entry rows; and, for
    /// [`Self::TimedJoin`], the `entry: false` rows `Thread.join(J)V` /
    /// `join(JI)V`, whose `wait` call the native's hint decides.
    fn enters_at(self, row: &StandinMethod) -> bool {
        row.entry || (matches!(self, Self::TimedJoin { .. }) && is_served_timed_join_row(row))
    }

    /// Which `wait` call the `Thread.join(long)` hop leaves from, told apart by
    /// its ARGUMENT rather than its position (round 14 wave 6, lane trace5):
    /// `true` = the `wait(0)` (an `lconst_0` argument: `join()` is
    /// `join(0L)`, and a direct `join(0)`), `false` = the timed `wait(delay)`.
    /// JDK 21/25 put the timed loop first, JDK 17 the `wait(0)` loop, so the
    /// "last call" rule answered JDK 17's `join()` from the wrong loop.
    fn join_waits_zero(self) -> bool {
        match self {
            Self::TimedJoin { timed } => !timed,
            _ => true,
        }
    }

    /// Does a hop leave its body from its FIRST census call rather than its
    /// last? Only on a virtual thread: JDK 25 `Object.wait(long)` calls `wait0`
    /// twice, the virtual-thread branch first.
    fn first_call(self) -> bool {
        matches!(self, Self::VirtualThrowable(_))
    }
}

/// The longest chain rebuilt (JDK 25's longest is `TimeUnit.sleep` ->
/// `Thread.sleep` -> `sleepNanos` -> `sleepNanos0`).
const STANDIN_CHAIN_MAX: usize = 8;

/// Could a throwable of this class get stand-in frames at all? The first,
/// cheapest screen of [`native_standin_frames`].
pub(crate) fn is_standin_throwable(throwable_class: &str) -> bool {
    STANDIN_METHODS
        .iter()
        .any(|m| m.throws.contains(&throwable_class))
        || throwable_class == STANDIN_ARG_CHECK_THROWABLE
}

/// Round 14 wave 2 (lane trace; `r13w13-trace3-vm-served-jdk-methods-residuals`
/// item 2): the census methods whose OWN bytecode rejects a bad argument before
/// reaching the leaf. HotSpot, `Thread.sleep(-1)` on JDK 25: ONE frame,
/// `java.base/java.lang.Thread.sleep(Thread.java:537)`, at the
/// `IllegalArgumentException` construction -- no leaf frames. A registered
/// `sleep` / `wait` threw it with the caller on top.
const STANDIN_ARG_CHECK_THROWABLE: &str = "java/lang/IllegalArgumentException";
const STANDIN_ARG_CHECK_METHODS: &[StandinMethod] = &[
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(J)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Object",
        name: "wait",
        descriptor: "(J)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
    },
    // Round 14 wave 4 (lane trace3): `join(-1)` of a registered timed `join`
    // (`--compatible`); one construction site on JDK 17/21/25.
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "(J)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
    },
];

/// The one frame of an argument-check stand-in (see
/// [`STANDIN_ARG_CHECK_METHODS`]): the method at its ONLY
/// `invokespecial IllegalArgumentException.<init>` (a body with two such sites
/// is ambiguous and gets none), or at `(Native Method)` when the method is
/// itself `native` (JDK 17's `sleep(long)`). `CRATONVM_THROWABLE_STANDIN_ARG_CHECK_FRAMES=0`
/// turns it off.
fn standin_arg_check_frames(
    store: &ClassStore,
    owner_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_reader::instruction::Instruction;
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_STANDIN_ARG_CHECK_FRAMES") {
        return None;
    }
    let (class_id, class) = STANDIN_ARG_CHECK_METHODS
        .iter()
        .filter(|m| m.name == name && m.descriptor == descriptor && standin_row_enabled(m))
        .find_map(|m| standin_declaring_class(store, owner_id, m))?;
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
    if method.is_native() {
        return Some(vec![BacktraceFrame::Entry(standin_entry(
            class, class_id, method, index, None,
        ))]);
    }
    let code = method.code()?;
    let bytes: &[u8] = &code.code;
    let mut site = None;
    let mut pc = 0usize;
    while pc < bytes.len() {
        let (instruction, next) = Instruction::decode(bytes, pc).ok()?;
        if let Instruction::Invokespecial(cp_index) = instruction {
            if let Some((owner, "<init>", _)) = cp_method_ref(&class.constant_pool, cp_index) {
                if owner == STANDIN_ARG_CHECK_THROWABLE {
                    if site.is_some() {
                        return None;
                    }
                    site = Some(pc);
                }
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    let site = site?;
    Some(vec![BacktraceFrame::Entry(standin_entry(
        class,
        class_id,
        method,
        index,
        Some(site),
    ))])
}

/// Lock-free pre-screen of the innermost frame: `false` when it certainly does
/// not stand at a census call. An interpreted frame must stand at an
/// `invokevirtual` / `invokestatic`, or at an `invokespecial` inside
/// `java.lang.Object` (javac's call of the private `wait0`); every other shape
/// is decided under the class store.
pub(crate) fn may_stand_at_standin_call(innermost: &BacktraceFrame) -> bool {
    match innermost {
        BacktraceFrame::Method { method, bci, .. } => {
            let Ok(bci) = usize::try_from(*bci) else {
                return false;
            };
            match method.code.get(bci).copied() {
                Some(0xb6 | 0xb8) => true,
                Some(0xb7) => &*method.class_name == "java/lang/Object",
                _ => false,
            }
        }
        _ => true,
    }
}

/// `(class, method, descriptor, bci)` of the frame whose call is decoded.
fn standin_call_site(frame: &BacktraceFrame) -> Option<(ClassId, Arc<str>, Arc<str>, usize)> {
    let (class_id, name, descriptor, bci) = match frame {
        BacktraceFrame::Method {
            method,
            bci,
            class_id,
        } => (
            *class_id,
            Arc::clone(&method.method_name),
            Arc::clone(&method.method_descriptor),
            *bci,
        ),
        other => {
            let entry = other.to_entry();
            (
                entry.class_id?,
                entry.method_name,
                entry.method_descriptor?,
                entry.byte_code_index,
            )
        }
    };
    Some((class_id, name, descriptor, usize::try_from(bci).ok()?))
}

/// `(owner, name, descriptor)` of the method `CONSTANT_Methodref` (or
/// `InterfaceMethodref`) `index` names.
fn cp_method_ref(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    index: u16,
) -> Option<(&str, &str, &str)> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let (class_index, nat_index) = match cp.get(index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner = cp.get_class_name(class_index)?;
    let (name, descriptor) = cp.get_name_and_type(nat_index)?;
    Some((owner, name, descriptor))
}

/// The method the `invokevirtual` / `invokespecial` / `invokestatic` at `pc`
/// calls, or `None` for any other instruction.
fn invoke_target_at<'a>(
    cp: &'a cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    pc: usize,
) -> Option<(&'a str, &'a str, &'a str)> {
    use cratonvm_reader::instruction::Instruction;
    match Instruction::decode(code, pc).ok()?.0 {
        Instruction::Invokevirtual(index)
        | Instruction::Invokespecial(index)
        | Instruction::Invokestatic(index) => cp_method_ref(cp, index),
        _ => None,
    }
}

/// The LAST call in `code` of a census method `screen` admits, with its pc,
/// the row, and the id of the row's class the call reaches. Last, because
/// where a JDK body has two (JDK 25 `wait(long)`: the virtual-thread branch,
/// then the platform one) the platform path is the later; a virtual thread's
/// screen ([`StandinScreen::first_call`], round 14 wave 5) takes the FIRST
/// instead. Any decode failure answers `None`: a
/// chain is rebuilt whole or not at all.
///
/// `reaches(owner, row)` answers the id of `row.class` when the call's
/// constant-pool `owner` resolves to it (round 14 wave 4, lane trace3, T4-2 of
/// `jit-r14-trace4-proposals.md`): javac qualifies an unqualified call by the
/// enclosing class (JLS 13.1), so `wait(delay)` inside `Thread.join(long)`
/// names `java/lang/Thread.wait(J)V`, which the literal compare it replaced
/// never matched. A call whose owner does not reach the row is not a census
/// call.
///
/// `zero_timeout` (round 14 wave 6, lane trace5): `Some(zero)` considers only
/// the calls whose argument is (`true`) or is not (`false`) an `lconst_0`
/// pushed right before the call -- the `Thread.join(long)` hop's pick between
/// its `wait(0)` and `wait(delay)` ([`StandinScreen::join_waits_zero`]).
fn last_standin_call(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    screen: StandinScreen<'_>,
    zero_timeout: Option<bool>,
    reaches: &mut dyn FnMut(&str, &'static StandinMethod) -> Option<ClassId>,
) -> Option<(usize, &'static StandinMethod, ClassId)> {
    use cratonvm_reader::instruction::Instruction;
    let mut found = None;
    let mut pc = 0usize;
    let mut after_lconst_0 = false;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let argument_admitted = zero_timeout.is_none_or(|zero| zero == after_lconst_0);
        after_lconst_0 = matches!(instruction, Instruction::Lconst0);
        let index = match instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index)
                if argument_admitted =>
            {
                Some(index)
            }
            _ => None,
        };
        if let Some((owner, name, descriptor)) = index.and_then(|index| cp_method_ref(cp, index)) {
            let reached = STANDIN_METHODS
                .iter()
                .filter(|m| m.name == name && m.descriptor == descriptor && screen.admits(m))
                .find_map(|m| Some((m, reaches(owner, m)?)));
            if let Some((row, class_id)) = reached {
                found = Some((pc, row, class_id));
                // Round 14 wave 5 (lane trace4): a virtual thread's hop.
                if screen.first_call() {
                    return found;
                }
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    found
}

/// A stand-in frame of `method` (slot `index` of `class`): at `call_pc` with
/// its line, or the native leaf (line -2) when `call_pc` is `None`.
fn standin_entry(
    class: &Class,
    class_id: ClassId,
    method: &ClassFileMethod,
    index: usize,
    call_pc: Option<usize>,
) -> StackTraceEntry {
    let (line_number, byte_code_index) = match call_pc {
        Some(pc) => (
            line_number_for_bci_in_method(method, pc).unwrap_or(LINE_NUMBER_UNKNOWN),
            i32::try_from(pc).unwrap_or(-1),
        ),
        None => (LINE_NUMBER_NATIVE, -1),
    };
    StackTraceEntry {
        class_name: Arc::clone(&class.name),
        method_name: Arc::clone(&method.name),
        method_descriptor: Some(Arc::clone(&method.descriptor)),
        // HotSpot names the holder's source file on a native frame too
        // (`StackTraceElement.getFileName()` is "Thread.java").
        source_file: source_file_of_class(class),
        line_number,
        byte_code_index,
        class_id: Some(class_id),
        method_index: u32::try_from(index).ok(),
    }
}

/// From the class `owner_id` a call names, up the superclass chain to the
/// census class `target.class`; `None` when the chain does not reach it, or a
/// class on the way declares the method itself (a subclass's own
/// `static void sleep(long)` hides `Thread.sleep`).
fn standin_declaring_class<'s>(
    store: &'s ClassStore,
    owner_id: ClassId,
    target: &StandinMethod,
) -> Option<(ClassId, &'s Class)> {
    let mut id = owner_id;
    for _ in 0..64 {
        let class = store.get(id)?;
        if &*class.name == target.class {
            return Some((id, class));
        }
        if class.find_method(target.name, target.descriptor).is_some() {
            return None;
        }
        id = class.superclass?;
    }
    None
}

/// The JDK frames the method the innermost captured frame was calling stands
/// in for, OUTERMOST-first (append them to an outermost-first trace), or
/// `None`. See the section comment above.
///
/// `resolve_class(requester, name)` resolves a class name as the requester's
/// loader would (the class manager's `find_class_by_name_for_class`). For a
/// platform thread only: a virtual thread's `sleep` runs other JDK code
/// (`VirtualThread.sleepNanos`, `parkNanos`), not these leaves, and its
/// `wait` leaves from the other branch ([`virtual_thread_standin_frames`]).
pub(crate) fn native_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    standin_frames_screened(
        store,
        innermost,
        throwable_class,
        StandinScreen::Throwable(throwable_class),
        resolve_class,
    )
}

/// Round 14 wave 5 (lane trace4; `r13w13-trace3-vm-served-jdk-methods-residuals`
/// item 5): [`native_standin_frames`] for a capture on a VIRTUAL thread. Its
/// `sleep` runs `VirtualThread.sleepNanos` (Java, which raises the
/// `InterruptedException` itself: no native leaf to rebuild), but its
/// `Object.wait` still ends in `wait0` -- JDK 25 `wait(long)` calls it from
/// the virtual-thread branch, the FIRST of its two calls; JDK 21 has one.
/// So only the `Object.wait` rows chain, each hop at its first census call;
/// HotSpot 25 for an interrupted registered `wait()` on a virtual thread:
/// `Object.wait0(Native Method)`, `Object.wait(Object.java:<first wait0
/// call's line>)`, `Object.wait(Object.java:...)`, caller. The argument-check
/// rows are unchanged (their check precedes the branch).
/// `CRATONVM_THROWABLE_STANDIN_VIRTUAL_WAIT_FRAMES=0` answers nothing for a
/// wait chain (read only once one was found). The `Array` rows chain here too
/// (their bodies do not branch on the thread).
///
/// Only a SERVED `wait` is rebuilt this way. A virtual thread on this VM is a
/// `ThreadBuilders$BoundVirtualThread` (`ContinuationSupport.isSupported0`
/// answers `false`), not a `java.lang.VirtualThread`, so where the real
/// `wait(long)` bytecode runs (`--jdk-only`) it takes the PLATFORM branch and
/// its own frame says so (`Object.java:389` on JDK 25, HotSpot's virtual
/// thread 382). That frame is what ran; it is not rewritten.
pub(crate) fn virtual_thread_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    let frames = standin_frames_screened(
        store,
        innermost,
        throwable_class,
        StandinScreen::VirtualThrowable(throwable_class),
        resolve_class,
    )?;
    let is_wait_chain = throwable_class != STANDIN_ARG_CHECK_THROWABLE
        && frames
            .first()
            .is_some_and(|f| &*f.to_entry().class_name == "java/lang/Object");
    if is_wait_chain
        && !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_STANDIN_VIRTUAL_WAIT_FRAMES",
        )
    {
        return None;
    }
    Some(frames)
}

/// `CRATONVM_THROWABLE_STANDIN_TIMED_JOIN_FRAMES` (default on; round 14 wave
/// 6, lane trace5): [`timed_join_standin_frames`], and the argument pick of
/// the `Thread.join(long)` hop ([`StandinScreen::join_waits_zero`]). Read only
/// once a chain reached that hop, or a served direct `join(long)` threw.
fn timed_join_frames_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_STANDIN_TIMED_JOIN_FRAMES")
}

/// Round 14 wave 6 (lane trace5; TR5-3 of `jit-r14-trace5-proposals.md`): the
/// JDK frames of an `InterruptedException` a served DIRECT `Thread.join(long)`
/// raised (`--compatible`: `join(J)V` is registered), OUTERMOST-first, or
/// `None`. The frames alone cannot tell which `wait` loop HotSpot's
/// `join(long)` was in; the native can, and left `timed` (millis > 0) in
/// `JvmThread::served_timed_join_hint`. HotSpot 25, `t.join(500)`
/// interrupted: `Object.wait0(Native Method)`, `Object.wait(Object.java:389)`,
/// `Thread.join(Thread.java:1881)` (the timed loop's `wait(delay)`), caller;
/// `t.join(0)` leaves from the `wait(0)` loop (1887). The innermost captured
/// frame must stand at a call of `join(J)V` or `join(JI)V` itself (the VM
/// serves `join(long, int)` through the timed join with the rounded millis,
/// which is what HotSpot's `join(long, int)` hands `join(long)`).
pub(crate) fn timed_join_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    timed: bool,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let caller_code = caller.code()?;
    let (owner, name, descriptor) =
        invoke_target_at(&caller_class.constant_pool, &caller_code.code, bci)?;
    let served_join = name == "join" && matches!(descriptor, "(J)V" | "(JI)V");
    if !served_join || !timed_join_frames_enabled() {
        return None;
    }
    standin_chain(
        store,
        caller_id,
        (owner, name, descriptor),
        StandinScreen::TimedJoin { timed },
        resolve_class,
    )
}

/// `(owner, name, descriptor)` of the call the innermost captured frame stands
/// at, with the caller's class id (its constant pool names the owner).
fn innermost_call_target<'s>(
    store: &'s ClassStore,
    innermost: &BacktraceFrame,
) -> Option<(ClassId, &'s Class, (&'s str, &'s str, &'s str))> {
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let code = caller.code()?;
    let target = invoke_target_at(&caller_class.constant_pool, &code.code, bci)?;
    Some((caller_id, caller_class, target))
}

/// Round 14 wave 7 (lane trace6; TW6-2 of `jit-r14-trace5w6-proposals.md`,
/// `r13w13-trace3-vm-served-jdk-methods-residuals` item 5): does the served
/// `Thread.join()` the innermost frame stands at decline the census's `wait`
/// chain for this `InterruptedException`? The native's hint (`declines`,
/// `JvmThread::served_timed_join_hint`) is `true` when its target is a
/// `java.lang.VirtualThread`, whose JDK `join(long)` runs
/// `VirtualThread.joinNanos` rather than the `wait` loops -- HotSpot's frames
/// there are `CountDownLatch` / AQS code the census does not describe, so the
/// capture keeps the caller on top rather than show loops that did not run.
/// The served timed joins decline by leaving no hint (`join(J)V` / `join(JI)V`
/// are no census entries). A `BoundVirtualThread` target (every "virtual"
/// thread on this VM) is not declined: its JDK body runs the platform loops.
pub(crate) fn served_join_declines_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    declines: bool,
) -> bool {
    declines
        && throwable_class == STANDIN_INTERRUPTED
        && innermost_call_target(store, innermost)
            .is_some_and(|(_, _, (_, name, descriptor))| name == "join" && descriptor == "()V")
}

/// Round 14 wave 7 (lane trace6; TW6-3 of `jit-r14-trace5w6-proposals.md`,
/// `r13w13-trace3-vm-served-jdk-methods-residuals` item 1): the `(long, int)`
/// overloads whose JDK body rejects a bad argument at TWO
/// `IllegalArgumentException` construction sites -- `millis < 0` first, the
/// `nanos` range second, in JDK 17, 21 and 25 alike -- which the one-site rule
/// ([`standin_arg_check_frames`]) refuses as ambiguous.
const STANDIN_ARG_CHECK_TWO_SITE_METHODS: &[StandinMethod] = &[
    StandinMethod {
        class: "java/lang/Thread",
        name: "join",
        descriptor: "(JI)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
    },
    StandinMethod {
        class: "java/lang/Thread",
        name: "sleep",
        descriptor: "(JI)V",
        throws: &[STANDIN_ARG_CHECK_THROWABLE],
        entry: true,
    },
    // Not `Object.wait(long, int)`: its native (`native_object_wait_timeout_nanos`)
    // is registered only by `register_synthetic_overrides`; on a real JDK its
    // bytecode runs and throws from its own frame.
];

/// The one frame of an `IllegalArgumentException` a served two-site overload
/// ([`STANDIN_ARG_CHECK_TWO_SITE_METHODS`]) raised, OUTERMOST-first, or `None`.
/// The native says which check failed (`second_check`: the hint it left,
/// `JvmThread::served_timed_join_hint`); the frame is the method at that
/// check's `invokespecial IllegalArgumentException.<init>`, with its line --
/// HotSpot, `t.join(0, -1)` on JDK 25: `Thread.join(Thread.java:1928)`, caller.
/// A body without exactly two sites answers nothing (fail-closed, as before);
/// a `native` overload is its own `(Native Method)` frame whichever check
/// failed. `CRATONVM_THROWABLE_STANDIN_ARG_CHECK_SITE_HINT=0` (or
/// `CRATONVM_THROWABLE_STANDIN_ARG_CHECK_FRAMES=0`) answers nothing.
pub(crate) fn served_arg_check_standin_frames(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    second_check: bool,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    use cratonvm_reader::instruction::Instruction;
    if throwable_class != STANDIN_ARG_CHECK_THROWABLE {
        return None;
    }
    let (caller_id, caller_class, (owner, name, descriptor)) =
        innermost_call_target(store, innermost)?;
    if !STANDIN_ARG_CHECK_TWO_SITE_METHODS
        .iter()
        .any(|m| m.name == name && m.descriptor == descriptor)
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_STANDIN_ARG_CHECK_SITE_HINT",
        )
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_STANDIN_ARG_CHECK_FRAMES",
        )
    {
        return None;
    }
    let owner_id = if owner == &*caller_class.name {
        caller_id
    } else {
        resolve_class(caller_id, owner)?
    };
    let (class_id, class) = STANDIN_ARG_CHECK_TWO_SITE_METHODS
        .iter()
        .filter(|m| m.name == name && m.descriptor == descriptor && standin_row_enabled(m))
        .find_map(|m| standin_declaring_class(store, owner_id, m))?;
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
    if method.is_native() {
        return Some(vec![BacktraceFrame::Entry(standin_entry(
            class, class_id, method, index, None,
        ))]);
    }
    let code = method.code()?;
    let bytes: &[u8] = &code.code;
    let mut sites = [None; 2];
    let mut count = 0usize;
    let mut pc = 0usize;
    while pc < bytes.len() {
        let (instruction, next) = Instruction::decode(bytes, pc).ok()?;
        if let Instruction::Invokespecial(cp_index) = instruction {
            if let Some((STANDIN_ARG_CHECK_THROWABLE, "<init>", _)) =
                cp_method_ref(&class.constant_pool, cp_index)
            {
                if count >= sites.len() {
                    return None;
                }
                sites[count] = Some(pc);
                count += 1;
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    if count != sites.len() {
        return None;
    }
    let site = sites[usize::from(second_check)]?;
    Some(vec![BacktraceFrame::Entry(standin_entry(
        class,
        class_id,
        method,
        index,
        Some(site),
    ))])
}

/// Round 14 wave 7 (lane trace6; RV6-2 of `jit-r14-review6-proposals.md`):
/// `true` when the throwable is a `NullPointerException` and the innermost
/// frame, interpreted, stands at an `invokevirtual` / `invokespecial` /
/// `invokeinterface` -- the VM's own receiver NPE, or one a served instance
/// method raised. Neither stand-in rule can answer it: the census admits an
/// NPE only on its `Array` rows (static calls), and [`native_leaf_frame`]
/// refuses a non-static call's NPE. Decided from the opcode alone, before
/// either decodes the call or resolves its owner. Compiled innermost frames
/// are left to the rules (their code is not at hand without the class store).
/// `CRATONVM_THROWABLE_RECEIVER_NPE_SCREEN=0` turns it off (read only once a
/// frame matched).
pub(crate) fn receiver_npe_gets_no_standin_frames(
    innermost: &BacktraceFrame,
    throwable_class: &str,
) -> bool {
    let BacktraceFrame::Method { method, bci, .. } = innermost else {
        return false;
    };
    throwable_class == "java/lang/NullPointerException"
        && usize::try_from(*bci).is_ok_and(|bci| code_is_instance_invoke_at(&method.code, bci))
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_RECEIVER_NPE_SCREEN")
}

/// `code[bci]` is an `invokevirtual` / `invokespecial` / `invokeinterface`.
fn code_is_instance_invoke_at(code: &[u8], bci: usize) -> bool {
    matches!(code.get(bci).copied(), Some(0xb6 | 0xb7 | 0xb9))
}

/// The body of [`native_standin_frames`] / [`virtual_thread_standin_frames`];
/// `screen` picks the chain rows.
fn standin_frames_screened(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable_class: &str,
    screen: StandinScreen<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    if !is_standin_throwable(throwable_class) {
        return None;
    }
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let caller_code = caller.code()?;
    let (owner, name, descriptor) =
        invoke_target_at(&caller_class.constant_pool, &caller_code.code, bci)?;
    // Screen by name first: every compiled `new IllegalArgumentException`
    // reaches here standing at its own `<init>` call.
    if throwable_class == STANDIN_ARG_CHECK_THROWABLE
        && STANDIN_ARG_CHECK_METHODS
            .iter()
            .any(|m| m.name == name && m.descriptor == descriptor)
    {
        let owner_id = resolve_class(caller_id, owner)?;
        return standin_arg_check_frames(store, owner_id, name, descriptor);
    }
    // Round 14 wave 5 (lane trace4): no chain can start at a call that names
    // no entry row -- decided before any class is resolved (a receiver
    // `NullPointerException` at any invoke reaches here since the `Array`
    // rows admit NPE). An `IllegalArgumentException` at a call that is no
    // argument-check row may still start one (`Array.newInstance`).
    if !STANDIN_METHODS
        .iter()
        .any(|m| m.entry && m.name == name && m.descriptor == descriptor)
    {
        return None;
    }
    standin_chain(
        store,
        caller_id,
        (owner, name, descriptor),
        screen,
        resolve_class,
    )
}

/// The chain of [`native_standin_frames`] from the call `callee` (`(owner,
/// name, descriptor)` as the constant pool of class `caller_id` names it),
/// for the rows `screen` admits.
fn standin_chain(
    store: &ClassStore,
    caller_id: ClassId,
    callee: (&str, &str, &str),
    screen: StandinScreen<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    let (owner, name, descriptor) = callee;
    let owner_id = resolve_class(caller_id, owner)?;
    let (mut class_id, mut class) = STANDIN_METHODS
        .iter()
        .filter(|m| {
            screen.enters_at(m)
                && m.name == name
                && m.descriptor == descriptor
                && screen.admits(m)
                && standin_row_enabled(m)
        })
        .find_map(|m| standin_declaring_class(store, owner_id, m))?;
    let (mut name, mut descriptor) = (name, descriptor);
    let mut out = Vec::new();
    for _ in 0..STANDIN_CHAIN_MAX {
        let (index, method) = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
        if method.is_native() {
            out.push(BacktraceFrame::Entry(standin_entry(
                class, class_id, method, index, None,
            )));
            return Some(out);
        }
        let code = method.code()?;
        let current_id = class_id;
        let current_name: &str = &class.name;
        let mut reaches = |owner: &str, row: &'static StandinMethod| -> Option<ClassId> {
            if owner == row.class {
                // The literal owner (every JDK sleep / wait hop): as before.
                if owner == current_name {
                    return Some(current_id);
                }
                let id = resolve_class(current_id, owner)?;
                return (store.get(id).map(|c| &*c.name) == Some(row.class)).then_some(id);
            }
            // T4-2: an owner naming a class below the row's
            // (`java/lang/Thread.wait(J)V`), read only once a call matched a
            // row by name and descriptor.
            if !cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_STANDIN_OWNER_RESOLUTION",
            ) {
                return None;
            }
            let owner_id = if owner == current_name {
                current_id
            } else {
                resolve_class(current_id, owner)?
            };
            standin_declaring_class(store, owner_id, row).map(|(id, _)| id)
        };
        // Round 14 wave 6 (lane trace5): the `Thread.join(long)` hop leaves
        // from the `wait` its argument names, not the last one.
        // `CRATONVM_THROWABLE_STANDIN_TIMED_JOIN_FRAMES=0` restores the last.
        let zero_timeout = (current_name == "java/lang/Thread"
            && name == "join"
            && descriptor == "(J)V"
            && timed_join_frames_enabled())
        .then(|| screen.join_waits_zero());
        let (call_pc, next, next_id) = last_standin_call(
            &class.constant_pool,
            &code.code,
            screen,
            zero_timeout,
            &mut reaches,
        )?;
        out.push(BacktraceFrame::Entry(standin_entry(
            class,
            class_id,
            method,
            index,
            Some(call_pc),
        )));
        class_id = next_id;
        class = store.get(class_id)?;
        if &*class.name != next.class {
            return None;
        }
        name = next.name;
        descriptor = next.descriptor;
    }
    None
}

// ---------------------------------------------------------------------------
// Reflection frames: the JDK frames a reflective call runs without
// ---------------------------------------------------------------------------
//
// Interpreter round i1 wave 43, lane L3
// (`docs/internal/fixed-bugs/interpreter-L3-a-reflective-call-leaves-no-method-invoke-frame-FIXED-20261007.md`;
// what is still not listed:
// `docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md`).
// The VM serves
// `Method.invoke` and `Constructor.newInstance` with natives (both modes; the
// interpreter's cached-invoke door keeps them native,
// `dispatch_virtual::native_override_for_cached_reflect_invoke`), so the
// target's frame sat directly on its caller's. HotSpot, JDK 25, for a static
// `R.dump` reached by `Method.invoke` from `R.main` (a `Throwable`'s trace,
// `Thread.getStackTrace` and a `SHOW_REFLECT_FRAMES` walk alike):
//
// ```text
// R.dump(R.java:5)
// java.base/jdk.internal.reflect.DirectMethodHandleAccessor.invoke(DirectMethodHandleAccessor.java:104)
// java.base/java.lang.reflect.Method.invoke(Method.java:565)
// R.main(R.java:13)
// ```
//
// and for a constructor `DirectConstructorHandleAccessor.newInstance`,
// `Constructor.newInstanceWithCaller` and `Constructor.newInstance`. The
// method handle frames between the accessor and the target are hidden
// (`@Hidden` / hidden classes) and are not listed. The natives record the call
// on the thread (`JvmThread::reflective_calls`: the interpreter depth and the
// JIT entry-chain length at the call); a capture lists the JDK frames, each at
// its call to the next (its `LineNumberTable` line, found in the real class
// bytes as the stand-in frames above are), right below the first frame the
// call put on the stack: the first interpreter frame it pushed, or, since
// wave 44, a compiled activation its target entered without one (a compiled
// activation names the JIT entry-chain entry it came from,
// `ActiveCompiledFrame::chain_index`, so the ones entered after the call are
// told from the call's compiled callers: `trace_anchor_position`).
// Fail-closed, the capture lists nothing for a call when:
//
// * nothing was pushed above it and the capture is not a throwable's (a
//   throwable the native itself raised, `InvocationTargetException` or an
//   argument check's `IllegalArgumentException`, lists the call on top since
//   waves 45 and 46, at the lines HotSpot's frames stand at:
//   [`reflective_raise_entries`], [`reflective_check_entries`]);
// * a class or method the frames name is not loaded or not found (JDK 17's
//   reflection has other frames).

/// One reflective call a thread is inside (`JvmThread::reflective_calls`):
/// where its JDK frames go in a capture of that thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReflectiveCallRow {
    /// The thread's interpreter depth at the call: the first interpreter
    /// frame the call pushed is `frames[interp_depth]`.
    pub interp_depth: u32,
    /// The thread's JIT entry-chain length at the call
    /// (`conservative_roots::current_thread_jit_depth`): entries below it are
    /// the call's compiled callers.
    pub jit_depth: u32,
    /// Which call.
    pub kind: cratonvm_native_api::ReflectiveCallKind,
    /// The argument check of the call that failed, noted by its native right
    /// before it builds the check's throwable inside the call
    /// (`NativeExceptionAccess::note_reflective_check`; `None`: none failed).
    /// A capture of that throwable lists the check's frames
    /// ([`reflective_check_entries`]). Interpreter round i1 wave 46, lane L3.
    pub check: Option<cratonvm_native_api::ReflectiveCheck>,
}

/// One JDK frame of a reflective call: `class.name descriptor`, standing at
/// its call of `calls_owner.calls_name calls_descriptor`.
struct ReflectiveStep {
    class: &'static str,
    name: &'static str,
    descriptor: &'static str,
    calls_owner: &'static str,
    calls_name: &'static str,
    calls_descriptor: &'static str,
}

/// `Method.invoke`'s JDK frames, OUTERMOST first (JDK 25; read with `javap
/// -c -p` from JDK 25.0.3: `Method.invoke` calls the accessor through
/// `invokeinterface MethodAccessor.invoke`, the accessor its private
/// `invokeImpl` through `invokevirtual`).
const METHOD_INVOKE_STEPS: &[ReflectiveStep] = &[
    ReflectiveStep {
        class: "java/lang/reflect/Method",
        name: "invoke",
        descriptor: "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        calls_owner: "jdk/internal/reflect/MethodAccessor",
        calls_name: "invoke",
        calls_descriptor: "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
    },
    ReflectiveStep {
        class: "jdk/internal/reflect/DirectMethodHandleAccessor",
        name: "invoke",
        descriptor: "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        calls_owner: "jdk/internal/reflect/DirectMethodHandleAccessor",
        calls_name: "invokeImpl",
        calls_descriptor: "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
    },
];

/// `Constructor.newInstance`'s JDK frames, OUTERMOST first (JDK 25, as
/// above).
const CONSTRUCTOR_NEW_INSTANCE_STEPS: &[ReflectiveStep] = &[
    ReflectiveStep {
        class: "java/lang/reflect/Constructor",
        name: "newInstance",
        descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
        calls_owner: "java/lang/reflect/Constructor",
        calls_name: "newInstanceWithCaller",
        calls_descriptor: "([Ljava/lang/Object;ZLjava/lang/Class;)Ljava/lang/Object;",
    },
    ReflectiveStep {
        class: "java/lang/reflect/Constructor",
        name: "newInstanceWithCaller",
        descriptor: "([Ljava/lang/Object;ZLjava/lang/Class;)Ljava/lang/Object;",
        calls_owner: "jdk/internal/reflect/ConstructorAccessor",
        calls_name: "newInstance",
        calls_descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
    },
    ReflectiveStep {
        class: "jdk/internal/reflect/DirectConstructorHandleAccessor",
        name: "newInstance",
        descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
        calls_owner: "jdk/internal/reflect/DirectConstructorHandleAccessor",
        calls_name: "invokeImpl",
        calls_descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
    },
];

fn reflective_steps(kind: cratonvm_native_api::ReflectiveCallKind) -> &'static [ReflectiveStep] {
    match kind {
        cratonvm_native_api::ReflectiveCallKind::MethodInvoke => METHOD_INVOKE_STEPS,
        cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance => {
            CONSTRUCTOR_NEW_INSTANCE_STEPS
        }
    }
}

/// The accessor class `kind`'s JDK frames name, which the VM's native never
/// loads itself: the native entering the call loads it once per thread
/// (`JvmThread::reflective_accessors_loaded`), so a capture can name it.
pub(crate) fn reflective_accessor_class(kind: cratonvm_native_api::ReflectiveCallKind) -> &'static str {
    match kind {
        cratonvm_native_api::ReflectiveCallKind::MethodInvoke => {
            "jdk/internal/reflect/DirectMethodHandleAccessor"
        }
        cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance => {
            "jdk/internal/reflect/DirectConstructorHandleAccessor"
        }
    }
}

/// `kind`'s bit in `JvmThread::reflective_accessors_loaded`.
pub(crate) fn reflective_kind_bit(kind: cratonvm_native_api::ReflectiveCallKind) -> u8 {
    match kind {
        cratonvm_native_api::ReflectiveCallKind::MethodInvoke => 1,
        cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance => 2,
    }
}

/// The pc of the first `invoke*` in `code` that calls `owner.name
/// descriptor`, or `None` (a decode failure included).
fn first_call_to(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    owner: &str,
    name: &str,
    descriptor: &str,
) -> Option<usize> {
    use cratonvm_reader::instruction::Instruction;
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let index = match instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index) => Some(index),
            Instruction::Invokeinterface { index, .. } => Some(index),
            _ => None,
        };
        if let Some(index) = index {
            if matches!(cp_method_ref(cp, index), Some((o, n, d)) if o == owner && n == name && d == descriptor)
            {
                return Some(pc);
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    None
}

/// `kind`'s JDK frames, OUTERMOST first, each at its call of the next with
/// its line, or `None` when one of them cannot be named. The
/// `name_frames` of [`reflective_splices`], under the caller's class-manager
/// read guard.
pub(crate) fn reflective_entries(
    store: &ClassStore,
    kind: cratonvm_native_api::ReflectiveCallKind,
) -> Option<Vec<StackTraceEntry>> {
    let steps = reflective_steps(kind);
    let mut out = Vec::with_capacity(steps.len());
    for step in steps {
        let class_id = find_class_id_by_name_memoized(store, step.class)?;
        let class = store.get(class_id)?;
        let (index, method) = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == step.name && &*m.descriptor == step.descriptor)?;
        let code = method.code()?;
        let pc = first_call_to(
            &class.constant_pool,
            &code.code,
            step.calls_owner,
            step.calls_name,
            step.calls_descriptor,
        )?;
        out.push(standin_entry(class, class_id, method, index, Some(pc)));
    }
    Some(out)
}

/// A reflective call's JDK frames, ready to be listed in a capture.
pub(crate) struct ReflectiveSplice {
    interp_depth: u32,
    jit_depth: u32,
    /// OUTERMOST first.
    entries: Arc<[StackTraceEntry]>,
    /// The frames listed when the throwable being captured is one the call's
    /// native raised itself (the call's frames go ON TOP of the trace, the
    /// innermost at the accessor's `throw new <that class>` line:
    /// [`reflective_raise_entries`]); `None` lists `entries` there.
    /// Interpreter round i1 wave 45, lane L3.
    raise: Option<Arc<[StackTraceEntry]>>,
}

/// A thread's reflective-call JDK frames as last named
/// (`JvmThread::reflective_frames_named`), per kind (index
/// [`reflective_kind_bit`] − 1): the class-redefinition count they were named
/// at, and the frames (`None`: they could not be named). A capture inside a
/// reflective call reuses them while no class was redefined since, so a
/// throwable raised under a reflective call (every JUnit test method's) takes
/// no class-manager lock and scans no bytecode for them.
pub type ReflectiveFramesNamed = [Option<(u64, Option<Arc<[StackTraceEntry]>>)>; 2];

/// The JDK frames of each of `calls` (a thread's `reflective_calls`,
/// outermost first) that can be named: from `named` while no class was
/// redefined since they were named, else from `name_frames`
/// ([`reflective_entries`] under the caller's class-manager read guard, taken
/// only then), which refreshes `named`.
pub(crate) fn reflective_splices(
    named: &mut ReflectiveFramesNamed,
    calls: &[ReflectiveCallRow],
    name_frames: &mut dyn FnMut(
        cratonvm_native_api::ReflectiveCallKind,
    ) -> Option<Vec<StackTraceEntry>>,
) -> Vec<ReflectiveSplice> {
    let count = cratonvm_classloading::class_redefinition_count();
    let mut out = Vec::with_capacity(calls.len());
    for call in calls {
        let slot = &mut named[usize::from(reflective_kind_bit(call.kind)) - 1];
        if !matches!(&*slot, Some((at, _)) if *at == count) {
            let entries: Option<Arc<[StackTraceEntry]>> = name_frames(call.kind).map(Arc::from);
            *slot = Some((count, entries));
        }
        let Some(entries) = slot.as_ref().and_then(|(_, entries)| entries.clone()) else {
            continue;
        };
        out.push(ReflectiveSplice {
            interp_depth: call.interp_depth,
            jit_depth: call.jit_depth,
            entries,
            raise: None,
        });
    }
    if crate::runtime::env_cache::dbg_sttrace() {
        eprintln!(
            "STTRACE_DBG_REFLECT calls={} named={}",
            calls.len(),
            out.len()
        );
    }
    out
}

/// [`reflective_splices`] from `named` alone: the calls whose frames are
/// memoized at the class-redefinition count `count`, and nothing else. For a
/// capture that may not take the class-manager lock (the published trace,
/// [`capture_published_trace`]: a blocking deposit may run while the thread
/// holds it). A call not named yet is left out, as a capture leaves out what
/// it cannot name. Interpreter round i1 wave 45, lane L3.
fn reflective_splices_memoized(
    named: &ReflectiveFramesNamed,
    calls: &[ReflectiveCallRow],
    count: u64,
) -> Vec<ReflectiveSplice> {
    calls
        .iter()
        .filter_map(|call| {
            let slot = &named[usize::from(reflective_kind_bit(call.kind)) - 1];
            let entries = match slot {
                Some((at, Some(entries))) if *at == count => Arc::clone(entries),
                _ => return None,
            };
            Some(ReflectiveSplice {
                interp_depth: call.interp_depth,
                jit_depth: call.jit_depth,
                entries,
                raise: None,
            })
        })
        .collect()
}

/// Name `kind`'s JDK frames into `named` unless they are already named at the
/// current class-redefinition count. `name_frames` runs only then
/// ([`reflective_entries`] under a class-manager read guard); `None` from it
/// (the guard was not free) leaves `named` as it was, so a later call tries
/// again. The native entering a reflective call runs this, so the thread's
/// published trace ([`capture_published_trace`], which reads only `named`)
/// can list the call without the thread first building a throwable under it
/// (interpreter round i1 wave 45, lane L3).
pub(crate) fn name_reflective_frames_if_stale(
    named: &mut ReflectiveFramesNamed,
    kind: cratonvm_native_api::ReflectiveCallKind,
    name_frames: impl FnOnce() -> Option<Option<Vec<StackTraceEntry>>>,
) {
    let count = cratonvm_classloading::class_redefinition_count();
    let slot = &mut named[usize::from(reflective_kind_bit(kind)) - 1];
    if matches!(&*slot, Some((at, _)) if *at == count) {
        return;
    }
    if let Some(entries) = name_frames() {
        *slot = Some((count, entries.map(Arc::from)));
    }
}

// ---------------------------------------------------------------------------
// A throwable the reflective native raises itself (wave 45, lane L3)
// ---------------------------------------------------------------------------
//
// Item 1 of
// `docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md`.
// The natives serving `Method.invoke` / `Constructor.newInstance` build the
// `InvocationTargetException` wrapping a target's throw themselves (running its
// real `<init>`, `lang_class::wrap_as_invocation_target_exception`), inside
// the call's record. Its capture then has the call's first pushed frame at the
// throwable's own constructor, which the fill-frame trim removes, so the call
// was listed nowhere and the trace started at the caller. HotSpot (JDK 25.0.3,
// measured, `tools/probes/interp/L3/L3W45ReflectiveRaise.java`) starts it in
// the accessor, at its `throw new InvocationTargetException(e)`:
//
// ```text
// jdk.internal.reflect.DirectMethodHandleAccessor.invoke(DirectMethodHandleAccessor.java:119)
// java.lang.reflect.Method.invoke(Method.java:565)
// <caller>
// ```
//
// (line 116 for a target's `NullPointerException`, 110 for a
// `ClassCastException`: the accessor's three `catch` arms). A throwable
// filled by a constructor the call itself runs (`Constructor.newInstance` of a
// throwable class) lists the frames there at their call lines, as HotSpot does
// (`DirectConstructorHandleAccessor.newInstance:62`). So a call whose first
// pushed slot is the first slot the trim removed, or which pushed none, is
// listed on top of the trace: at the accessor's `new <throwable class>` site
// when its innermost JDK frame has one ([`reflective_raise_entries`]), else at
// the call lines.

/// What a raised throwable wraps, for the choice between the accessor's
/// `catch` arms ([`reflective_raise_entries`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReflectiveRaiseCause {
    /// A `ClassCastException` or `WrongMethodTypeException`: the first arm.
    ClassCast,
    /// A `NullPointerException`: the second arm.
    NullPointer,
    /// Anything else, or nothing known: the last arm.
    Other,
}

/// The `catch` arm `class_id`'s class picks ([`ReflectiveRaiseCause`]), from
/// its superclass chain.
pub(crate) fn reflective_raise_cause(store: &ClassStore, class_id: ClassId) -> ReflectiveRaiseCause {
    let mut id = class_id;
    for _ in 0..64 {
        let Some(class) = store.get(id) else {
            break;
        };
        match &*class.name {
            "java/lang/NullPointerException" => return ReflectiveRaiseCause::NullPointer,
            "java/lang/ClassCastException" | "java/lang/invoke/WrongMethodTypeException" => {
                return ReflectiveRaiseCause::ClassCast;
            }
            "java/lang/Throwable" | "java/lang/Object" => break,
            _ => {}
        }
        let Some(superclass) = class.superclass else {
            break;
        };
        id = superclass;
    }
    ReflectiveRaiseCause::Other
}

/// The innermost reflective call of `calls` when a throwable of class
/// `throwable_class` being filled on top of `frames` may be one its native
/// raised: the call pushed no interpreter frame, or its first is a
/// constructor of `throwable_class` itself. With that frame, whose arguments
/// name what the throwable wraps. A cheap screen (a throwable of another
/// class, built by a reflectively called constructor, fails it); the capture
/// decides whether the call is really on top ([`build_slots_with_splices`]).
pub(crate) fn reflective_raise_site<'f>(
    frames: &'f [Frame],
    calls: &[ReflectiveCallRow],
    throwable_class: ClassId,
) -> Option<(ReflectiveCallRow, Option<&'f Frame>)> {
    let call = *calls.last()?;
    match frames.get(usize::try_from(call.interp_depth).ok()?) {
        None => Some((call, None)),
        Some(frame) if frame.class_id == throwable_class && frame.method_name() == "<init>" => {
            Some((call, Some(frame)))
        }
        Some(_) => None,
    }
}

/// `kind`'s JDK frames (`entries`, OUTERMOST first, as [`reflective_entries`]
/// names them) for a `throwable_class` its native raised: the innermost at
/// the `new throwable_class` of its method the `cause` picks (the first, the
/// second or the last such site), or `None` when that method has no such
/// site (the frames are then listed at their call lines).
pub(crate) fn reflective_raise_entries(
    store: &ClassStore,
    kind: cratonvm_native_api::ReflectiveCallKind,
    entries: &[StackTraceEntry],
    throwable_class: &str,
    cause: ReflectiveRaiseCause,
) -> Option<Arc<[StackTraceEntry]>> {
    use cratonvm_reader::instruction::Instruction;
    let steps = reflective_steps(kind);
    let step = steps.last()?;
    if entries.len() != steps.len() {
        return None;
    }
    let class_id = find_class_id_by_name_memoized(store, step.class)?;
    let class = store.get(class_id)?;
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == step.name && &*m.descriptor == step.descriptor)?;
    let code = &method.code()?.code;
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        if let Instruction::New(cp_index) = instruction {
            if class.constant_pool.get_class_name(cp_index) == Some(throwable_class) {
                sites.push(pc);
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    let last = sites.len().checked_sub(1)?;
    let pick = match cause {
        ReflectiveRaiseCause::ClassCast => 0,
        ReflectiveRaiseCause::NullPointer => 1,
        ReflectiveRaiseCause::Other => last,
    }
    .min(last);
    let mut out: Vec<StackTraceEntry> = entries[..entries.len() - 1].to_vec();
    out.push(standin_entry(class, class_id, method, index, Some(sites[pick])));
    Some(Arc::from(out))
}

/// `class_name.name descriptor` in `store`: its class id, class, method
/// index and method.
fn named_method_in<'s>(
    store: &'s ClassStore,
    class_name: &str,
    name: &str,
    descriptor: &str,
) -> Option<(ClassId, &'s Class, usize, &'s ClassFileMethod)> {
    let class_id = find_class_id_by_name_memoized(store, class_name)?;
    let class = store.get(class_id)?;
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
    Some((class_id, class, index, method))
}

/// For each `new throwable_class` of `code`, in order, the pc of the
/// `invokespecial throwable_class.<init>` that completes it (the `new`'s own
/// pc when none does): the pc HotSpot's frame stands at while the
/// constructor fills the trace, whose line differs from the `new`'s when the
/// constructor's arguments span lines (`checkReceiver`'s
/// `throw new IllegalArgumentException("object of type " + ...` is line 198,
/// its constructor call line 199). `None` when `code` does not decode.
fn throwable_init_sites(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    throwable_class: &str,
) -> Option<Vec<usize>> {
    use cratonvm_reader::instruction::Instruction;
    let mut sites = Vec::new();
    // The sites whose constructor call is still to come, innermost last.
    let mut open: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        match instruction {
            Instruction::New(cp_index) if cp.get_class_name(cp_index) == Some(throwable_class) => {
                open.push(sites.len());
                sites.push(pc);
            }
            Instruction::Invokespecial(cp_index)
                if matches!(cp_method_ref(cp, cp_index), Some((o, n, _)) if o == throwable_class && n == "<init>") =>
            {
                if let Some(k) = open.pop() {
                    sites[k] = pc;
                }
            }
            _ => {}
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    Some(sites)
}

// ---------------------------------------------------------------------------
// A reflective call's argument checks (wave 46, lane L3)
// ---------------------------------------------------------------------------
//
// The rest of item 1 of
// `docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md`.
// The natives' argument checks note which check failed on the call's record
// (`NativeExceptionAccess::note_reflective_check`) and build the check's
// throwable by its real constructor inside the call, so its capture lists the
// call on top, as an `InvocationTargetException`'s does. HotSpot (JDK 25.0.3,
// measured, `tools/probes/interp/L3/L3W46ReflectiveChecks.java`) starts such a
// trace at the check:
//
// ```text
// argument count      DirectMethodHandleAccessor.checkArgumentCount:324, .invoke:102
// receiver type       DirectMethodHandleAccessor.checkReceiver:199, .invoke:100
// null receiver       Method.invoke:557 alone (its access check's obj.getClass())
//   (setAccessible)   DirectMethodHandleAccessor.checkReceiver:197, .invoke:100
// argument type       DirectMethodHandleAccessor.invoke:108
// null to primitive   DirectMethodHandleAccessor.invoke:114
// constructor         DirectConstructorHandleAccessor.newInstance:59 / :65 / :70
//                     (count / type / null to a primitive)
// ```
//
// each above the call's outer frames at their call lines (`Method.invoke:565`;
// `Constructor.newInstanceWithCaller:499`, `Constructor.newInstance:483`).

/// `kind`'s JDK frames (`entries`, OUTERMOST first, as [`reflective_entries`]
/// names them) for the throwable of class `throwable_class` the failed
/// argument check `check` raised: the frames HotSpot lists for that check
/// (see the section comment), found in the real class bytes. `None` when
/// the throwable is not the check's (`IllegalArgumentException`, or
/// `NullPointerException` for a null receiver), the check has no frames for
/// `kind`, or a method or site is not found; the capture then lists the frames
/// as [`reflective_raise_entries`] or the call lines say.
pub(crate) fn reflective_check_entries(
    store: &ClassStore,
    kind: cratonvm_native_api::ReflectiveCallKind,
    entries: &[StackTraceEntry],
    throwable_class: &str,
    check: cratonvm_native_api::ReflectiveCheck,
) -> Option<Arc<[StackTraceEntry]>> {
    use cratonvm_native_api::{ReflectiveCallKind as Kind, ReflectiveCheck as Check};
    const OBJECT_GET_CLASS: (&str, &str, &str) =
        ("java/lang/Object", "getClass", "()Ljava/lang/Class;");
    let steps = reflective_steps(kind);
    if entries.len() != steps.len() {
        return None;
    }
    if let Check::NullToPrimitive(primitive) = check {
        if throwable_class == "java/lang/NullPointerException" {
            // The `IllegalArgumentException`'s cause: the call's frames at
            // their call lines, `ValueConversions.unbox<Wrapper>` on top.
            let unbox = null_unbox_frame(store, primitive)?;
            let mut out = entries.to_vec();
            out.push(unbox);
            return Some(Arc::from(out));
        }
    }
    let raised = match check {
        Check::NullReceiverAtAccessCheck | Check::NullReceiverAtReceiverCheck => {
            "java/lang/NullPointerException"
        }
        _ => "java/lang/IllegalArgumentException",
    };
    if throwable_class != raised {
        return None;
    }
    let accessor = steps.last()?;
    let mut out: Vec<StackTraceEntry> = entries[..entries.len() - 1].to_vec();
    match (kind, check) {
        (Kind::MethodInvoke, Check::NullReceiverAtAccessCheck) => {
            // `Method.invoke` alone, at its access check's `obj.getClass()`.
            let outer = steps.first()?;
            let (class_id, class, index, method) =
                named_method_in(store, outer.class, outer.name, outer.descriptor)?;
            let (owner, name, descriptor) = OBJECT_GET_CLASS;
            let pc = first_call_to(&class.constant_pool, &method.code()?.code, owner, name, descriptor)?;
            out.clear();
            out.push(standin_entry(class, class_id, method, index, Some(pc)));
        }
        (
            Kind::MethodInvoke,
            Check::ArgumentCount | Check::Receiver | Check::NullReceiverAtReceiverCheck,
        ) => {
            // The accessor at its call of the check, the check on top.
            let (callee_name, callee_descriptor) = if check == Check::ArgumentCount {
                ("checkArgumentCount", "(I[Ljava/lang/Object;)V")
            } else {
                ("checkReceiver", "(Ljava/lang/Object;)V")
            };
            let (class_id, class, index, method) =
                named_method_in(store, accessor.class, accessor.name, accessor.descriptor)?;
            let call_pc = first_call_to(
                &class.constant_pool,
                &method.code()?.code,
                accessor.class,
                callee_name,
                callee_descriptor,
            )?;
            let (callee_index, callee) = class
                .methods
                .iter()
                .enumerate()
                .find(|(_, m)| &*m.name == callee_name && &*m.descriptor == callee_descriptor)?;
            let callee_code = &callee.code()?.code;
            let raise_pc = if check == Check::NullReceiverAtReceiverCheck {
                let (owner, name, descriptor) = OBJECT_GET_CLASS;
                first_call_to(&class.constant_pool, callee_code, owner, name, descriptor)?
            } else {
                *throwable_init_sites(&class.constant_pool, callee_code, throwable_class)?.first()?
            };
            out.push(standin_entry(class, class_id, method, index, Some(call_pc)));
            out.push(standin_entry(class, class_id, callee, callee_index, Some(raise_pc)));
        }
        _ => {
            // The accessor at the `new IllegalArgumentException` of the arm:
            // `DirectMethodHandleAccessor.invoke` has two (type, null), the
            // constructor accessor's `newInstance` three (count, type, null).
            let nth = match (kind, check) {
                (Kind::MethodInvoke, Check::ArgumentType) => 0,
                (Kind::MethodInvoke, Check::NullToPrimitive(_)) => 1,
                (Kind::ConstructorNewInstance, Check::ArgumentCount) => 0,
                (Kind::ConstructorNewInstance, Check::ArgumentType) => 1,
                (Kind::ConstructorNewInstance, Check::NullToPrimitive(_)) => 2,
                _ => return None,
            };
            let (class_id, class, index, method) =
                named_method_in(store, accessor.class, accessor.name, accessor.descriptor)?;
            let sites =
                throwable_init_sites(&class.constant_pool, &method.code()?.code, throwable_class)?;
            let pc = *sites.get(nth)?;
            out.push(standin_entry(class, class_id, method, index, Some(pc)));
        }
    }
    Some(Arc::from(out))
}

/// The class whose `unbox<Wrapper>(Object, boolean)` raises the
/// `NullPointerException` a null argument for a primitive parameter meets in
/// HotSpot's reflection ([`null_unbox_frame`]). The native noting that check
/// loads it, so the capture can name it.
pub(crate) const VALUE_CONVERSIONS_CLASS: &str = "sun/invoke/util/ValueConversions";

/// `ValueConversions.unbox<Wrapper>(Object, boolean)` for the primitive
/// descriptor `primitive`, standing at its `primitiveConversion(...).<x>Value()`
/// call (JDK 25: `unboxInteger:81`, `unboxLong:126`, `unboxBoolean:108`, ...,
/// measured with HotSpot 25.0.3), or `None` when it cannot be named.
fn null_unbox_frame(store: &ClassStore, primitive: char) -> Option<StackTraceEntry> {
    let (name, descriptor, value, value_descriptor) = match primitive {
        'I' => ("unboxInteger", "(Ljava/lang/Object;Z)I", "intValue", "()I"),
        'J' => ("unboxLong", "(Ljava/lang/Object;Z)J", "longValue", "()J"),
        'Z' => ("unboxBoolean", "(Ljava/lang/Object;Z)Z", "intValue", "()I"),
        'C' => ("unboxCharacter", "(Ljava/lang/Object;Z)C", "intValue", "()I"),
        'B' => ("unboxByte", "(Ljava/lang/Object;Z)B", "byteValue", "()B"),
        'S' => ("unboxShort", "(Ljava/lang/Object;Z)S", "shortValue", "()S"),
        'F' => ("unboxFloat", "(Ljava/lang/Object;Z)F", "floatValue", "()F"),
        'D' => ("unboxDouble", "(Ljava/lang/Object;Z)D", "doubleValue", "()D"),
        _ => return None,
    };
    let (class_id, class, index, method) =
        named_method_in(store, VALUE_CONVERSIONS_CLASS, name, descriptor)?;
    let pc = first_call_to(
        &class.constant_pool,
        &method.code()?.code,
        "java/lang/Number",
        value,
        value_descriptor,
    )?;
    Some(standin_entry(class, class_id, method, index, Some(pc)))
}

/// Give the splice of `call` (the innermost reflective call, from
/// [`reflective_raise_site`]) the frames [`reflective_raise_entries`] built
/// for a throwable its native raised. Returns whether one was given.
pub(crate) fn set_reflective_raise(
    splices: &mut [ReflectiveSplice],
    call: &ReflectiveCallRow,
    raise: Arc<[StackTraceEntry]>,
) -> bool {
    match splices.last_mut() {
        Some(last) if last.interp_depth == call.interp_depth && last.jit_depth == call.jit_depth => {
            last.raise = Some(raise);
            true
        }
        _ => false,
    }
}

/// The JDK frames `splices`' innermost call lists (OUTERMOST first), for
/// [`reflective_raise_entries`].
pub(crate) fn innermost_splice_entries(splices: &[ReflectiveSplice]) -> Option<Arc<[StackTraceEntry]>> {
    splices.last().map(|s| Arc::clone(&s.entries))
}

/// Where each of `splices` goes in a capture laid out as `slots`: as
/// `(position, splice index)`, the JDK frames going right before
/// `slots[position]`, the first slot the call put on the stack
/// ([`trace_anchor_position`]: the first interpreter frame it pushed, or the
/// first compiled activation entered at its depth since it, whichever comes
/// first). A call with no such slot is left out (see the section comment).
/// Positions never decrease (the calls nest).
fn reflective_splice_positions(
    slots: &[TraceSlot<'_>],
    splices: &[ReflectiveSplice],
    unanchored_on_top: bool,
) -> Vec<(usize, usize)> {
    splices
        .iter()
        .enumerate()
        .filter_map(|(k, splice)| {
            let mut position =
                trace_anchor_position(slots, splice.interp_depth, splice.jit_depth);
            // Wave 45: a throwable capture lists a call that pushed nothing
            // on top (its native raised the throwable itself).
            if position.is_none() && unanchored_on_top {
                position = Some(slots.len());
            }
            if crate::runtime::env_cache::dbg_sttrace() {
                // `compiled_after=true`: the call's frames go right below a
                // compiled activation its target entered (wave 44); before
                // wave 44 such a call was left out (`compiled_since=true`).
                let compiled_after =
                    matches!(position.and_then(|p| slots.get(p)), Some(TraceSlot::Compiled(_)));
                eprintln!(
                    "STTRACE_DBG_REFLECT depth={} jit_depth={} position={position:?} \
                     compiled_after={compiled_after}",
                    splice.interp_depth, splice.jit_depth
                );
            }
            Some((position?, k))
        })
        .collect()
}

/// Where a record kept outside the JIT entry chain goes in a capture laid out
/// as `slots`, given the thread's interpreter depth `depth` and JIT
/// entry-chain length `chain_len` when it was made: right before the first
/// slot pushed after it, which is interpreter frame `depth`, or a compiled
/// activation whose chain entry was pushed after the record
/// (`ActiveCompiledFrame::chain_index >= chain_len`) at that depth or deeper,
/// whichever comes first. `None` when no slot was pushed after it (the record
/// is the innermost thing on the stack).
///
/// The compiled activations at `depth` with `chain_index < chain_len` are the
/// record's compiled callers and stay below it. Interpreter round i1 wave 44,
/// lane L3 (the proposal
/// `docs/internal/fixed-bugs/interpreter-L3-proposal-compiled-activations-name-their-jit-entry-FIXED-20261008.md`):
/// a reflective call's JDK frames ([`reflective_splice_positions`]) and a JNI
/// native's JVMTI row ([`capture_trace_with_anchor_positions`]).
fn trace_anchor_position(slots: &[TraceSlot<'_>], depth: u32, chain_len: u32) -> Option<usize> {
    let mut interp = 0u32;
    for (i, slot) in slots.iter().enumerate() {
        match slot {
            TraceSlot::Interp { .. } => {
                if interp == depth {
                    return Some(i);
                }
                interp = interp.saturating_add(1);
            }
            TraceSlot::Compiled(compiled) => {
                if compiled.chain_index >= chain_len && compiled.interp_depth >= depth {
                    return Some(i);
                }
            }
            TraceSlot::Inlined(..) => {}
        }
    }
    None
}

/// The trace of `frames` a JVMTI stack function lists (the published
/// capture's entries, as [`capture_published_trace`] builds them, or one entry
/// per frame without compiled activations), and, for each of `anchors` (a JNI
/// native's row: the interpreter depth and the JIT entry-chain length at its
/// call, outermost first), the index of the entry it goes right before
/// ([`trace_anchor_position`]), `entries.len()` for on top. Positions never
/// decrease. Interpreter round i1 wave 44, lane L3: a compiled method a
/// native's own JNI upcall entered is listed above the native
/// (`docs/internal/fixed-bugs/interpreter-L1-a-native-is-listed-below-a-compiled-method-its-upcall-entered-FIXED-20261008.md`).
/// Only `experimental-debug` builds have the JVMTI C table that reads it.
#[cfg(feature = "experimental-debug")]
pub(crate) fn capture_trace_with_anchor_positions(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    anchors: &[(u32, u32)],
) -> (Vec<StackTraceEntry>, Vec<usize>) {
    let jit = if crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
        crate::jit::conservative_roots::active_compiled_frames()
    } else {
        Vec::new()
    };
    if jit.is_empty() {
        let entries = capture_frames_no_lines_exact(frames);
        let len = entries.len();
        let positions = anchors
            .iter()
            .map(|&(depth, _)| usize::try_from(depth).map_or(len, |d| d.min(len)))
            .collect();
        return (entries, positions);
    }
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    let mut frame_positions = Vec::with_capacity(frames.len());
    let mut entries = interleave_compiled_frames_without_store(
        frames,
        &jit,
        &osr_bci,
        Some(&mut frame_positions),
    );
    if cratonvm_classloading::class_redefinition_count() != 0 {
        publish_compiled_lines_at_their_stamps(shared, frames, &jit, &osr_bci, &mut entries);
    }
    let slots = collect_trace_slots(frames, &jit, &osr_bci);
    let len = entries.len();
    let positions: Vec<usize> = if slots.len() == len {
        anchors
            .iter()
            .map(|&(depth, chain_len)| trace_anchor_position(&slots, depth, chain_len).unwrap_or(len))
            .collect()
    } else {
        // The slots and the entries disagree (they never should): place by
        // interpreter frame alone, the wave-42 answer.
        anchors
            .iter()
            .map(|&(depth, _)| {
                usize::try_from(depth)
                    .ok()
                    .and_then(|d| frame_positions.get(d))
                    .and_then(|&p| usize::try_from(p).ok())
                    .unwrap_or(len)
            })
            .collect()
    };
    if crate::runtime::env_cache::dbg_sttrace() {
        eprintln!(
            "STTRACE_DBG_ANCHORS entries={len} slots={} anchors={anchors:?} positions={positions:?}",
            slots.len()
        );
    }
    (entries, positions)
}

/// `slots[..keep]` built, with the JDK frames of `splices` at `positions`
/// ([`reflective_splice_positions`]) spliced in. A position past `keep` (the
/// call is inside the frames the fill-frame trim removed) lists nothing. A
/// position AT `keep` -- the call's first pushed slot is the first one the
/// trim removed, or it pushed none -- is a throwable filled inside the call
/// with nothing of it above: its frames go on top, as its `raise` frames when
/// it has them (wave 45, lane L3; before, it listed nothing).
fn build_slots_with_splices<T: FromTraceSlot>(
    slots: &[TraceSlot<'_>],
    keep: usize,
    splices: &[ReflectiveSplice],
    positions: &[(usize, usize)],
) -> Vec<T> {
    let extra: usize = positions
        .iter()
        .map(|&(_, k)| splices[k].entries.len())
        .sum();
    let mut out = Vec::with_capacity(keep + extra);
    let mut pending = positions.iter().peekable();
    for (i, slot) in slots[..keep].iter().enumerate() {
        while let Some(&&(position, k)) = pending.peek() {
            if position > i {
                break;
            }
            if position == i {
                out.extend(splices[k].entries.iter().cloned().map(T::from_entry));
            }
            pending.next();
        }
        if let Some(built) = T::from_slot(slot) {
            out.push(built);
        }
    }
    for &(position, k) in pending {
        if position != keep {
            continue;
        }
        let splice = &splices[k];
        let listed = splice.raise.as_ref().unwrap_or(&splice.entries);
        if crate::runtime::env_cache::dbg_sttrace() {
            eprintln!(
                "STTRACE_DBG_REFLECT on-top listed={} raise={}",
                listed.len(),
                splice.raise.is_some()
            );
        }
        out.extend(listed.iter().cloned().map(T::from_entry));
    }
    out
}

/// `trace`, a capture of `frames` OUTERMOST first with one entry per slot
/// [`collect_trace_slots`] lays out over them (`capture_full_trace`,
/// `capture_stack_walk_trace`), with the JDK frames of `splices` spliced in
/// (`Thread.getStackTrace` and the stack walks). A trace of another length is
/// left as it is.
pub(crate) fn splice_reflective_frames(
    frames: &[Frame],
    trace: &mut Vec<StackTraceEntry>,
    splices: &[ReflectiveSplice],
) {
    if splices.is_empty() {
        return;
    }
    let jit = crate::jit::conservative_roots::active_compiled_frames();
    let (jit, osr_bci) = if jit.is_empty() {
        (jit, OsrBciOverrides::default())
    } else {
        drop_osr_continuations(frames, jit)
    };
    let slots = collect_trace_slots(frames, &jit, &osr_bci);
    if slots.len() != trace.len() {
        return;
    }
    let positions = reflective_splice_positions(&slots, splices, false);
    // From the innermost, so an earlier position still indexes the entry it
    // named; calls at one position keep their order, the outer one first.
    for &(position, k) in positions.iter().rev() {
        let inner = trace.split_off(position);
        trace.extend(splices[k].entries.iter().cloned());
        trace.extend(inner);
    }
}

// Round 14 wave 5 (lane trace4; T3-1 of `jit-r13-trace3-proposals-RETIRED-20260929.md`,
// `r13w13-trace3-vm-served-jdk-methods-residuals` item 4). HotSpot shows the
// frame of EVERY `native` method that raises a throwable -- the frame is
// pushed before the native body runs:
//
// ```text
// java.lang.ArrayIndexOutOfBoundsException: arraycopy: last source index 11 out of bounds for int[10]
//     at java.base/java.lang.System.arraycopy(Native Method)
//     at Main.main(Main.java:3)
// ```
//
// The VM calls a native without a Java frame, so the capture had the caller
// on top. The census above rebuilds whole chains for eleven methods; this is
// the one-frame rule for the rest: the innermost captured frame stands at an
// `invokestatic` / `invokespecial` / `invokevirtual` whose resolved method is
// `ACC_NATIVE` in the real class bytes, and the throwable is one the call
// cannot have raised BEFORE entering it. What an invoke raises itself:
//
// - `NullPointerException` for a null receiver (every non-static call);
// - resolution and initialization failures of the call (the
//   `IncompatibleClassChangeError` family: `NoSuchMethodError`,
//   `IllegalAccessError`, `AbstractMethodError`; `NoClassDefFoundError`,
//   `ExceptionInInitializerError`); `UnsatisfiedLinkError` is raised by the
//   native's own linking, inside its frame on HotSpot, and is kept;
// - `StackOverflowError`.
//
// And the call must enter THAT native: `invokestatic` / `invokespecial` do;
// an `invokevirtual` only when the method cannot be overridden (`final`,
// `private`, or a `final` class) or is `Object.clone` (whose `protected`
// access pins the receiver to the caller's own subclasses, whose overrides
// push frames). A non-final virtual native (`Object.hashCode`) may have
// dispatched to a VM-served override, whose throwable is not the native's.
// `MethodHandle` / `VarHandle` natives are signature-polymorphic: the VM runs
// their TARGET there, never a native frame, and are refused.
//
// The census runs first; this answers only when it did not.

/// Lock-free pre-screen of the innermost frame for [`native_leaf_frame`]:
/// `false` when it certainly does not stand at a call of a `native` method.
pub(crate) fn may_stand_at_native_call(innermost: &BacktraceFrame) -> bool {
    match innermost {
        BacktraceFrame::Method { method, bci, .. } => {
            usize::try_from(*bci).is_ok_and(|bci| code_may_call_native_at(&method.code, bci))
        }
        _ => true,
    }
}

/// `code[bci]` is an `invokevirtual` / `invokestatic`, or an `invokespecial`
/// not followed by `athrow`: `throw new X(..)` stands at `X.<init>` (never
/// native) when the capture runs, and that is most throws, so it is refused
/// here without the class-manager lock.
fn code_may_call_native_at(code: &[u8], bci: usize) -> bool {
    match code.get(bci).copied() {
        Some(0xb6 | 0xb8) => true,
        Some(0xb7) => code.get(bci + 3).copied() != Some(0xbf),
        _ => false,
    }
}

/// See the section comment above: may `throwable` have been raised INSIDE a
/// native entered by the call (`static_call`: an `invokestatic`)?
fn native_leaf_throwable_admitted(store: &ClassStore, throwable: &Class, static_call: bool) -> bool {
    match &*throwable.name {
        "java/lang/NullPointerException" => return static_call,
        "java/lang/UnsatisfiedLinkError" => return true,
        "java/lang/StackOverflowError" => return false,
        _ => {}
    }
    let mut class = Some(throwable);
    for _ in 0..64 {
        let Some(c) = class else {
            return true;
        };
        if matches!(
            &*c.name,
            "java/lang/IncompatibleClassChangeError"
                | "java/lang/NoClassDefFoundError"
                | "java/lang/ExceptionInInitializerError"
        ) {
            return false;
        }
        class = c.superclass.and_then(|id| store.get(id));
    }
    false
}

/// The `(Native Method)` frame of the `native` method the innermost captured
/// frame was calling, or `None`. See the section comment above.
/// `resolve_class(requester, name)` resolves a class name as the requester's
/// loader would. `CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES=0` turns it off (read
/// only once a frame was found).
pub(crate) fn native_leaf_frame(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable: &Class,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<BacktraceFrame> {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    use cratonvm_reader::instruction::Instruction;
    #[derive(Clone, Copy, PartialEq)]
    enum Call {
        Static,
        Special,
        Virtual,
    }
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let code = caller.code()?;
    let (cp_index, call) = match Instruction::decode(&code.code, bci).ok()?.0 {
        Instruction::Invokestatic(cp_index) => (cp_index, Call::Static),
        Instruction::Invokespecial(cp_index) => (cp_index, Call::Special),
        Instruction::Invokevirtual(cp_index) => (cp_index, Call::Virtual),
        _ => return None,
    };
    let (owner, name, descriptor) = cp_method_ref(&caller_class.constant_pool, cp_index)?;
    if name.starts_with('<') || !native_leaf_throwable_admitted(store, throwable, call == Call::Static)
    {
        return None;
    }
    let owner_id = if owner == &*caller_class.name {
        caller_id
    } else {
        resolve_class(caller_id, owner)?
    };
    // Interface methods are never `native` (JVMS 4.6).
    if store.get(owner_id)?.is_interface() {
        return None;
    }
    // JVMS 6.5 `invokespecial` of a superclass method (`super.clone()`):
    // selection starts at the caller's direct superclass.
    let mut id = if call == Call::Special && owner_id != caller_id {
        caller_class.superclass?
    } else {
        owner_id
    };
    for _ in 0..64 {
        let class = store.get(id)?;
        let found = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor);
        let Some((index, method)) = found else {
            id = class.superclass?;
            continue;
        };
        if !method.is_native()
            || method.is_static() != (call == Call::Static)
            || matches!(
                &*class.name,
                "java/lang/invoke/MethodHandle" | "java/lang/invoke/VarHandle"
            )
        {
            return None;
        }
        // A hidden native (`@Hidden`, a hidden class) is no frame on HotSpot;
        // and the `--jdk-only` hidden drop may have left the innermost frame
        // standing at a call INTO hidden frames that raised the throwable.
        if method_is_hidden(store, id, name, Some(descriptor), u32::try_from(index).ok()) {
            return None;
        }
        let entered = match call {
            Call::Static | Call::Special => true,
            Call::Virtual => {
                method
                    .access_flags
                    .intersects(MethodAccessFlags::FINAL | MethodAccessFlags::PRIVATE)
                    || class.is_final()
                    || (&*class.name == "java/lang/Object" && name == "clone")
            }
        };
        if !entered
            || !cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES",
            )
        {
            return None;
        }
        return Some(BacktraceFrame::Entry(standin_entry(
            class, id, method, index, None,
        )));
    }
    None
}

/// Round 14 wave 4 (lane trace3; T4-3 of `jit-r14-trace4-proposals.md`,
/// `r13w13-trace3-vm-served-jdk-methods-residuals` item 6): the JDK frames of
/// the census method a PARKED thread is blocked in, OUTERMOST-first, for its
/// published stack (`innermost` is that stack's innermost entry). HotSpot's
/// `Thread.getStackTrace()` of a thread sleeping in a registered `sleep` ends
/// `Thread.sleepNanos0(Native Method)`, `Thread.sleepNanos`, `Thread.sleep`,
/// where the published stack ended at the caller.
///
/// The caller proves the thread is inside the call: it is blocked
/// `WAITING` / `TIMED_WAITING` now, and a blocked thread's published stack is
/// the one its blocking native deposited, standing at the invoke of what it
/// blocks in. A published entry carries no descriptor
/// ([`capture_frames_no_lines`]): the method is the ONE method of that name
/// whose code at the entry's bci is a call of a census method (two answer
/// nothing). Virtual threads are the caller's to refuse.
pub(crate) fn parked_thread_standin_frames(
    store: &ClassStore,
    innermost: &StackTraceEntry,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<StackTraceEntry>> {
    let caller_id = innermost.class_id?;
    let bci = usize::try_from(innermost.byte_code_index).ok()?;
    let caller_class = store.get(caller_id)?;
    let is_census_call = |(_, name, descriptor): &(&str, &str, &str)| {
        STANDIN_METHODS
            .iter()
            .any(|m| m.entry && m.name == *name && m.descriptor == *descriptor)
    };
    let mut callee = None;
    for method in caller_class
        .methods
        .iter()
        .filter(|m| *m.name == *innermost.method_name)
        .filter(|m| {
            innermost
                .method_descriptor
                .as_deref()
                .is_none_or(|d| *m.descriptor == *d)
        })
    {
        let Some(code) = method.code() else {
            continue;
        };
        let Some(target) = invoke_target_at(&caller_class.constant_pool, &code.code, bci) else {
            continue;
        };
        if !is_census_call(&target) {
            continue;
        }
        if callee.is_some() {
            return None;
        }
        callee = Some(target);
    }
    let frames = standin_chain(store, caller_id, callee?, StandinScreen::Parked, resolve_class)?;
    Some(frames.iter().map(BacktraceFrame::to_entry).collect())
}

/// Round 14 wave 4 (lane trace3): the lines of a PUBLISHED trace's entries
/// (`trace`, OUTERMOST-first) whose method is an overload set. A published
/// entry carries no descriptor ([`capture_frames_no_lines`]), so
/// [`resolve_line_numbers_in_place`] leaves an overloaded method at
/// `UNKNOWN`: every JDK `Object.wait` / `Thread.join` / `Thread.sleep` frame
/// of a parked thread printed `(Object.java)` where HotSpot prints
/// `(Object.java:389)`. Entry `i` stands at a call of entry `i + 1`'s method,
/// so its method is the ONE overload whose code at its bci is an invoke of a
/// method of that name; the entry gets that overload's descriptor, slot and
/// line. Two candidates (or none) leave it as it was: fail-closed like the
/// resolver it follows. The innermost entry has no callee (unless leaf frames
/// were appended after it). Answers how many entries were resolved.
pub(crate) fn resolve_published_overloads_by_callee(
    store: &ClassStore,
    trace: &mut [StackTraceEntry],
) -> usize {
    use cratonvm_reader::instruction::Instruction;
    let mut resolved = 0;
    for i in 1..trace.len() {
        let (head, tail) = trace.split_at_mut(i);
        let entry = &mut head[i - 1];
        let callee_name: &str = &tail[0].method_name;
        if entry.line_number != LINE_NUMBER_UNKNOWN || entry.method_descriptor.is_some() {
            continue;
        }
        let (Some(class_id), Ok(bci)) = (entry.class_id, usize::try_from(entry.byte_code_index))
        else {
            continue;
        };
        let Some(class) = store.get(class_id) else {
            continue;
        };
        let mut only: Option<(usize, &ClassFileMethod)> = None;
        let mut ambiguous = false;
        for (index, method) in class.methods.iter().enumerate() {
            if *method.name != *entry.method_name {
                continue;
            }
            let Some(code) = method.code() else {
                continue;
            };
            let cp_index = match Instruction::decode(&code.code, bci).map(|(i, _)| i) {
                Ok(
                    Instruction::Invokevirtual(cp_index)
                    | Instruction::Invokespecial(cp_index)
                    | Instruction::Invokestatic(cp_index)
                    | Instruction::Invokeinterface { index: cp_index, .. },
                ) => cp_index,
                _ => continue,
            };
            let calls_callee = cp_method_ref(&class.constant_pool, cp_index)
                .is_some_and(|(_, name, _)| name == callee_name);
            if !calls_callee {
                continue;
            }
            if only.is_some() {
                ambiguous = true;
                break;
            }
            only = Some((index, method));
        }
        let Some((index, method)) = only.filter(|_| !ambiguous) else {
            continue;
        };
        entry.method_descriptor = Some(Arc::clone(&method.descriptor));
        entry.method_index = u32::try_from(index).ok();
        if let Some(line) = line_number_for_bci_in_method(method, bci) {
            entry.line_number = line;
            resolved += 1;
        }
    }
    resolved
}

// Round 14 wave 3 (lane trace;
// `r13w13-trace3-vm-served-jdk-methods-residuals-FIXED-20260929.md` item 3). A
// started platform thread whose class does not override `run()` enters
// `Thread.run()`, which the VM serves (a registered, force-listed `Bridge`) by
// calling `task.run()` itself, so the task's `run` was the OUTERMOST captured
// frame. HotSpot 25 has `java.base/java.lang.Thread.run(Thread.java:1474)`
// below it (the `@Hidden runWith` between them is never shown). This rebuilds
// that one frame from the real class bytes, at the call of `runWith` (JDK 21+)
// or of `target.run()` (JDK 17).

const THREAD_CLASS: &str = "java/lang/Thread";

/// The class of `frame` when it is a `run()V` activation of a class other
/// than `java.lang.Thread`: the only shape the `Thread.run` stand-in applies
/// to. String compares only (every capture on a platform thread asks).
fn thread_run_task_frame_class(frame: &BacktraceFrame) -> Option<ClassId> {
    let (class_name, method_name, descriptor, class_id) = backtrace_frame_identity(frame)?;
    // A published entry (`capture_frames_no_lines`, another thread's stack)
    // carries no descriptor. Taken as `()V` (round 14 wave 3, lane trace2):
    // the bottom rule then also proves that `run()V` dispatched on the task
    // lands on this class, and a frame the served `Thread.run` called is that
    // `run()V`. (The middle rule needs the CALLER's descriptor, so it never
    // answers for such a stack.)
    let descriptor = descriptor.unwrap_or("()V");
    if method_name != "run" || descriptor != "()V" || class_name == THREAD_CLASS {
        return None;
    }
    class_id
}

/// `(class name, method name, descriptor, class id)` of `frame`, string
/// splits only. The descriptor is `None` only for an entry built without one.
fn backtrace_frame_identity(
    frame: &BacktraceFrame,
) -> Option<(&str, &str, Option<&str>, Option<ClassId>)> {
    Some(match frame {
        BacktraceFrame::Method {
            method, class_id, ..
        } => (
            &*method.class_name,
            &*method.method_name,
            Some(&*method.method_descriptor),
            Some(*class_id),
        ),
        BacktraceFrame::Compiled {
            label,
            owner_class_id,
            ..
        } => {
            let (owner_and_method, descriptor) = label.rsplit_once(':')?;
            let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
            (
                class_name,
                method_name,
                Some(descriptor),
                Some(ClassId::new(*owner_class_id)),
            )
        }
        BacktraceFrame::Inlined {
            label, class_id, ..
        } => {
            let (owner_and_method, descriptor) = label.rsplit_once(':')?;
            let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
            (
                class_name,
                method_name,
                Some(descriptor),
                (*class_id != 0).then(|| ClassId::new(*class_id)),
            )
        }
        BacktraceFrame::Entry(entry) => (
            &*entry.class_name,
            &*entry.method_name,
            entry.method_descriptor.as_deref(),
            entry.class_id,
        ),
    })
}

/// Lock-free pre-screen of the OUTERMOST captured frame for
/// [`thread_run_standin_frame`].
pub(crate) fn may_be_thread_run_task_frame(outermost: &BacktraceFrame) -> bool {
    thread_run_task_frame_class(outermost).is_some()
}

/// Lock-free pre-screen of the OUTERMOST captured frame for
/// [`thread_run_standin_frame_for_lambda`] (round 14 wave 3, lane trace2): any
/// frame not named `main` may be the implementation method of a LAMBDA task.
/// A lambda proxy here has no class in the store and pushes no frame:
/// `task.run()` on it enters the implementation method directly
/// (`invokedynamic`'s `LambdaCallSite`), so that method is the outermost
/// frame. The `main` refusal keeps the main thread's throws off the
/// class-manager lock.
///
/// Round 14 wave 4 (lane trace3): tightened, still lock-free. Every throw on
/// a non-main platform thread whose outermost frame is not a task's `run()V`
/// (a JNI-attached thread, a VM-driven `ClassLoader.loadClass` whose
/// delegation throws `ClassNotFoundException`, a `<clinit>`) paid the
/// class-manager read lock and two field walks here to learn it is not a
/// lambda task. A `Runnable` proxy's implementation method takes the SAM's
/// parameters (none) after the captured ones, so it is one of:
/// - a method reference's target, bound or static: no parameters, `()...`
///   (a constructor reference's `<init>()V` too);
/// - a synthesized lambda body with captured parameters, which every
///   compiler that spins `invokedynamic` lambdas names as such: javac / ecj
///   / Kotlin `...lambda$...`, Scala `$anonfun$...`.
///
/// `<clinit>` is never an implementation method. A descriptor-less entry is
/// kept. `CRATONVM_THROWABLE_THREAD_RUN_LAMBDA_SCREEN=0` restores the wave-3
/// screen (any frame not named `main`); it is read only for a frame the new
/// screen refuses and the old one took.
pub(crate) fn may_be_thread_run_lambda_frame(outermost: &BacktraceFrame) -> bool {
    let Some((_, name, descriptor, _)) = backtrace_frame_identity(outermost) else {
        return false;
    };
    if name == "main" || name == "<clinit>" {
        return false;
    }
    lambda_implementation_shaped(name, descriptor)
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_THREAD_RUN_LAMBDA_SCREEN",
        )
}

/// See [`may_be_thread_run_lambda_frame`]: could `name` / `descriptor` be the
/// implementation method of a `Runnable` lambda proxy?
fn lambda_implementation_shaped(name: &str, descriptor: Option<&str>) -> bool {
    descriptor.is_none_or(|d| d.starts_with("()"))
        || name.contains("lambda$")
        || name.contains("$anonfun$")
}

/// Lock-free refusal for [`thread_run_standin_frame`] (round 14 wave 4, lane
/// trace3): `outermost` is a `run()V` declared by the running thread's OWN
/// class (`thread_class`, the `Thread` object's), so that class overrides
/// `run` and HotSpot's bottom frame is that override itself -- a
/// `ForkJoinWorkerThread`, any `Thread` subclass with its own `run`. Exact:
/// both the task rule and the lambda rule refuse such a thread class under
/// the class-manager lock anyway.
pub(crate) fn outermost_is_threads_own_run(
    outermost: &BacktraceFrame,
    thread_class: ClassId,
) -> bool {
    backtrace_frame_identity(outermost).is_some_and(|(_, name, descriptor, class_id)| {
        name == "run" && descriptor.unwrap_or("()V") == "()V" && class_id == Some(thread_class)
    })
}

/// Round 14 wave 6 (lane trace5; TR5-2 of `jit-r14-trace5-proposals.md`):
/// one thread's last bottom `Thread.run` decision
/// (`vm_exec::prepend_thread_run_standin_frame`), keyed by what it depends
/// on. The task (`holder.task` is final on JDK 21+; JDK 17's `target` is
/// cleared only by `Thread.exit()`, after `run` returned) and the `Thread`
/// object are fixed for a `JvmThread`, so the answer is a function of the
/// thread's class, the outermost frame's method and the class versions.
pub(crate) struct ThreadRunBottomMemo {
    /// `cratonvm_classloading::class_redefinition_count()` when made.
    redefinitions: u64,
    /// The `Thread` object's class.
    thread_class: ClassId,
    /// The outermost frame's class id, method name and descriptor. A frame
    /// without a class id (an inlined level that recorded none) is never
    /// memoised: its name alone does not say which class it is.
    frame: (ClassId, Arc<str>, Option<Arc<str>>),
    answer: Option<BacktraceFrame>,
}

impl ThreadRunBottomMemo {
    pub(crate) fn new(
        redefinitions: u64,
        thread_class: ClassId,
        outermost: &BacktraceFrame,
        answer: Option<BacktraceFrame>,
    ) -> Option<Self> {
        let (_, name, descriptor, class_id) = backtrace_frame_identity(outermost)?;
        let class_id = class_id?;
        Some(Self {
            redefinitions,
            thread_class,
            frame: (class_id, Arc::from(name), descriptor.map(Arc::from)),
            answer,
        })
    }

    /// The memoised answer when the key matches (`Some(None)`: "no frame").
    pub(crate) fn lookup(
        &self,
        redefinitions: u64,
        thread_class: ClassId,
        outermost: &BacktraceFrame,
    ) -> Option<Option<&BacktraceFrame>> {
        let (_, name, descriptor, class_id) = backtrace_frame_identity(outermost)?;
        let hit = self.redefinitions == redefinitions
            && self.thread_class == thread_class
            && Some(self.frame.0) == class_id
            && &*self.frame.1 == name
            && self.frame.2.as_deref() == descriptor;
        hit.then_some(self.answer.as_ref())
    }
}

/// The `java.lang.Thread.run()` frame HotSpot shows below `outermost` when
/// the thread's task is a LAMBDA proxy (round 14 wave 3, lane trace2): the
/// proxy's `run` is not a frame here (and hidden on HotSpot), so `outermost`
/// is the proxy's implementation method `impl_owner.impl_name impl_desc` (a
/// `lambda$...` body or a method reference's target) -- or, for a virtual
/// method reference, an override of it in a subclass. `sam` is the proxy's
/// `(name, erased descriptor)`: only a `Runnable`-shaped `run()V` answers.
/// Then the thread-class and `Thread.run`-site rules of
/// [`thread_run_standin_frame`] apply. Same switch.
pub(crate) fn thread_run_standin_frame_for_lambda(
    store: &ClassStore,
    outermost: &BacktraceFrame,
    thread_class: ClassId,
    sam: (&str, &str),
    impl_method: (&str, &str, &str),
) -> Option<BacktraceFrame> {
    if sam != ("run", "()V") {
        return None;
    }
    let (impl_owner, impl_name, impl_desc) = impl_method;
    let (class_name, method_name, descriptor, class_id) = backtrace_frame_identity(outermost)?;
    if method_name != impl_name || descriptor.is_some_and(|d| d != impl_desc) {
        return None;
    }
    if class_name != impl_owner {
        // A virtual method reference dispatched to a subclass's override.
        let mut id = class_id?;
        let mut reaches = false;
        for _ in 0..64 {
            let Some(class) = store.get(id) else {
                break;
            };
            if &*class.name == impl_owner {
                reaches = true;
                break;
            }
            match class.superclass {
                Some(superclass) => id = superclass,
                None => break,
            }
        }
        if !reaches {
            return None;
        }
    }
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_THREAD_RUN_FRAME") {
        return None;
    }
    let (thread_id, thread) = thread_class_inheriting_run(store, thread_class)?;
    thread_run_site_entry(thread, thread_id).map(BacktraceFrame::Entry)
}

/// Is `thread_class` (a `Thread` object's class) a virtual thread's
/// (`java.lang.VirtualThread`, `BaseVirtualThread` or below)? Round 14 wave 4
/// (lane trace3): another thread's published stack has no `ThreadKind`.
pub(crate) fn is_virtual_thread_class(store: &ClassStore, thread_class: ClassId) -> bool {
    let mut id = thread_class;
    for _ in 0..64 {
        let Some(class) = store.get(id) else {
            return false;
        };
        if matches!(&*class.name, "java/lang/VirtualThread" | "java/lang/BaseVirtualThread") {
            return true;
        }
        match class.superclass {
            Some(superclass) => id = superclass,
            None => return false,
        }
    }
    false
}

/// `java.lang.Thread` (id and class) when `thread_class` reaches it without
/// overriding `run()V` and is not a virtual thread's class; `None` otherwise.
fn thread_class_inheriting_run(
    store: &ClassStore,
    thread_class: ClassId,
) -> Option<(ClassId, &Class)> {
    let mut id = thread_class;
    for _ in 0..64 {
        let class = store.get(id)?;
        if &*class.name == THREAD_CLASS {
            return Some((id, class));
        }
        // Round 14 wave 3 (lane trace2): a virtual thread runs its task
        // through `VirtualThread.run(Runnable)`, never `Thread.run()`. The
        // throwable capture screens `ThreadKind::Virtual` first; a walk of
        // ANOTHER thread's published stack has only the object's class.
        if matches!(&*class.name, "java/lang/VirtualThread" | "java/lang/BaseVirtualThread") {
            return None;
        }
        if class.find_method("run", "()V").is_some() {
            return None;
        }
        id = class.superclass?;
    }
    None
}

/// The `java.lang.Thread.run()` frame HotSpot shows below `outermost`, or
/// `None`. `thread_class` is the running thread's `Thread` object's class and
/// `task_class` the class of the `Runnable` the VM's `Thread.run` called (the
/// same `target` / `holder.task` field it reads). Answers only when all hold:
/// `outermost` is `X.run()V` with `X` not `Thread`; `thread_class` inherits
/// `run()V` from `java.lang.Thread` (an override is itself the bottom frame, as
/// on HotSpot); `run()V` dispatched on `task_class` lands on `X`; and
/// `Thread.run` has bytecode with exactly ONE call of `runWith` or of
/// `run()V`. `CRATONVM_THROWABLE_THREAD_RUN_FRAME=0` turns it off.
pub(crate) fn thread_run_standin_frame(
    store: &ClassStore,
    outermost: &BacktraceFrame,
    thread_class: ClassId,
    task_class: ClassId,
) -> Option<BacktraceFrame> {
    let frame_class = thread_run_task_frame_class(outermost)?;
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_THREAD_RUN_FRAME") {
        return None;
    }
    // The thread's class must reach `java.lang.Thread` without overriding `run`.
    let (thread_id, thread) = thread_class_inheriting_run(store, thread_class)?;
    // `task.run()` must dispatch to the frame's class: the first class up the
    // task's superclass chain that declares `run()V`, or an interface default
    // the task implements.
    let mut id = task_class;
    let mut declaring = None;
    for _ in 0..64 {
        let class = store.get(id)?;
        if class.find_method("run", "()V").is_some() {
            declaring = Some(id);
            break;
        }
        match class.superclass {
            Some(superclass) => id = superclass,
            None => break,
        }
    }
    match declaring {
        Some(declaring) if declaring == frame_class => {}
        Some(_) => return None,
        None => {
            let frame_type = store.get(frame_class)?;
            if frame_type.find_method("run", "()V").is_none()
                || !store.get(task_class)?.is_subclass_of(frame_class, store)
            {
                return None;
            }
        }
    }
    thread_run_site_entry(thread, thread_id).map(BacktraceFrame::Entry)
}

/// `java.lang.Thread.run()V` (class `thread`, id `thread_id`) as a frame
/// standing at its ONLY call of `runWith` (JDK 21+) or of `run()V` (JDK 17's
/// `target.run()`), with that call's line; `None` for a native `run`, or a body
/// with no such call or two.
fn thread_run_site_entry(thread: &Class, thread_id: ClassId) -> Option<StackTraceEntry> {
    use cratonvm_reader::instruction::Instruction;
    let (index, method) = thread
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == "run" && &*m.descriptor == "()V")?;
    if method.is_native() {
        return None;
    }
    let code = method.code()?;
    let bytes: &[u8] = &code.code;
    let mut site = None;
    let mut pc = 0usize;
    while pc < bytes.len() {
        let (instruction, next) = Instruction::decode(bytes, pc).ok()?;
        let cp_index = match instruction {
            Instruction::Invokevirtual(cp_index)
            | Instruction::Invokespecial(cp_index)
            | Instruction::Invokestatic(cp_index)
            | Instruction::Invokeinterface { index: cp_index, .. } => Some(cp_index),
            _ => None,
        };
        if let Some((_, name, descriptor)) =
            cp_index.and_then(|cp_index| cp_method_ref(&thread.constant_pool, cp_index))
        {
            if name == "runWith" || (name == "run" && descriptor == "()V") {
                if site.is_some() {
                    return None;
                }
                site = Some(pc);
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    Some(standin_entry(thread, thread_id, method, index, Some(site?)))
}

// Round 14 wave 3 (lane trace2; T14W3-3 of `jit-r14-trace3-proposals.md`,
// `r13w13-trace3-vm-served-jdk-methods-residuals-FIXED-20260929.md` item 3's middle
// case). `class W extends Thread { public void run() { ...; super.run(); } }`
// (JBoss `JBossThread`, framework thread wrappers) calls the VM-served
// `Thread.run`, which calls `task.run()` itself: the capture showed
// `task.run | W.run` where HotSpot 25 shows `task.run | Thread.run | W.run`.
// Also a direct `new Thread(r).run()`. The frame is inserted between a caller
// standing at an invoke that resolves to `java.lang.Thread.run()V` and its
// callee, when that callee is a `run()V` of a class other than `Thread`:
//
// - `invokespecial` (`super.run()`) is exact: the call entered `Thread.run`.
// - `invokevirtual` may have dispatched to the receiver's override instead.
//   An override's class extends `Thread`; a task's class (almost never)
//   does, so a callee whose class extends `Thread` gets no frame -- the rare
//   miss is a `Thread` handed to another `Thread` as its task.
//
// A bytecode-served `Thread.run` never reaches it (its own frame is then the
// callee, which the `run()V`-not-`Thread` screen refuses).

/// Lock-free pre-screen of one caller/callee pair for
/// [`thread_run_middle_frame`]: `false` when the pair certainly gets no frame.
/// An interpreted caller must stand at an `invokevirtual` / `invokespecial`.
pub(crate) fn may_need_thread_run_middle_frame(
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
) -> bool {
    let caller_may = match caller {
        BacktraceFrame::Method { method, bci, .. } => usize::try_from(*bci)
            .ok()
            .and_then(|bci| method.code.get(bci).copied())
            .is_some_and(|op| op == 0xb6 || op == 0xb7),
        _ => true,
    };
    caller_may && thread_run_task_frame_class(callee).is_some()
}

/// Does `class_id`'s superclass chain reach `java.lang.Thread`?
fn class_extends_thread(store: &ClassStore, class_id: ClassId) -> bool {
    let mut id = class_id;
    for _ in 0..64 {
        let Some(class) = store.get(id) else {
            return false;
        };
        if &*class.name == THREAD_CLASS {
            return true;
        }
        match class.superclass {
            Some(superclass) => id = superclass,
            None => return false,
        }
    }
    false
}

/// The `java.lang.Thread.run()` frame HotSpot shows between `caller` and
/// `callee` (adjacent frames of an OUTERMOST-first capture), or `None`. See
/// the section comment above. `resolve_class(requester, name)` resolves a
/// class name as the requester's loader would.
/// `CRATONVM_THROWABLE_THREAD_RUN_MIDDLE_FRAME=0` turns it off.
pub(crate) fn thread_run_middle_frame(
    store: &ClassStore,
    caller: &BacktraceFrame,
    callee: &BacktraceFrame,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<StackTraceEntry> {
    use cratonvm_reader::instruction::Instruction;
    let callee_class = thread_run_task_frame_class(callee)?;
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(caller)?;
    let caller_class = store.get(caller_id)?;
    let caller_method = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let caller_code = caller_method.code()?;
    let bytes: &[u8] = &caller_code.code;
    let (cp_index, virtual_call) = match Instruction::decode(bytes, bci).ok()?.0 {
        Instruction::Invokevirtual(cp_index) => (cp_index, true),
        Instruction::Invokespecial(cp_index) => (cp_index, false),
        _ => return None,
    };
    let (owner, name, descriptor) = cp_method_ref(&caller_class.constant_pool, cp_index)?;
    if name != "run" || descriptor != "()V" {
        return None;
    }
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_THROWABLE_THREAD_RUN_MIDDLE_FRAME")
    {
        return None;
    }
    // The call resolves to the first `run()V` up the owner's superclass
    // chain: it must be `java.lang.Thread`'s own.
    let mut id = resolve_class(caller_id, owner)?;
    let mut thread = None;
    for _ in 0..64 {
        let class = store.get(id)?;
        if class.find_method("run", "()V").is_some() {
            if &*class.name == THREAD_CLASS {
                thread = Some((id, class));
            }
            break;
        }
        id = class.superclass?;
    }
    let (thread_id, thread) = thread?;
    if virtual_call && class_extends_thread(store, callee_class) {
        return None;
    }
    thread_run_site_entry(thread, thread_id)
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

    /// Interpreter round i1 wave 34: only a JDK class or a hidden class can
    /// have a hidden frame, so any other entry is decided without the class
    /// manager.
    #[test]
    fn only_jdk_and_hidden_class_entries_may_be_hidden() {
        assert!(entry_may_be_hidden("java/lang/Thread"));
        assert!(entry_may_be_hidden("jdk/internal/vm/ScopedValueContainer"));
        assert!(entry_may_be_hidden("sun/nio/ch/Net"));
        assert!(entry_may_be_hidden(
            "com/example/Gen$Hid/0x0000000800c01000"
        ));
        assert!(!entry_may_be_hidden("com/example/Main"));
        assert!(!entry_may_be_hidden("Main"));
        assert!(!entry_may_be_hidden("javax/swing/JFrame"));
    }

    /// Interpreter round i1 wave 37: the label screen admits every label
    /// whose class [`entry_may_be_hidden`] admits, without splitting it.
    #[test]
    fn the_label_screen_is_a_superset_of_the_class_screen() {
        for label in [
            "java/lang/ScopedValue$Carrier.runWith:(Ljava/lang/Runnable;)V",
            "jdk/internal/vm/Continuation.enter:()V",
            "sun/nio/ch/Net.poll:()I",
            "com/example/Gen$Hid/0x0000000800c01000.apply:(I)I",
        ] {
            let (class_name, _) = split_frame_label(label).expect("parses");
            assert!(entry_may_be_hidden(class_name), "{label}");
            assert!(label_may_be_hidden(label), "{label}");
        }
        assert!(!label_may_be_hidden("com/example/Main.main:([Ljava/lang/String;)V"));
        assert!(!label_may_be_hidden("javax/swing/JFrame.show:()V"));
    }

    /// `store_with`'s class is `ClassId(0)` in its fresh store, and an
    /// inlined level reads a class id of `0` as "none recorded" (it keeps the
    /// frame without asking). Re-add the class at the next, non-zero id so a
    /// fixture's inlined level names it.
    fn moved_to_a_nonzero_id(store: &mut ClassStore, id: ClassId) -> ClassId {
        let mut class = store.remove(id).expect("the fixture's class");
        class.id = store.next_id();
        let moved = store.add(class);
        assert_ne!(moved.as_u32(), 0);
        moved
    }

    /// Interpreter round i1 wave 37: a compiled activation and an inlined
    /// level of a hidden class's method are hidden frames, as its interpreted
    /// frame is (waves 34-35 kept them); an inlined level with no recorded
    /// class id is kept, and a frame of an ordinary class is decided without
    /// the store.
    #[test]
    fn compiled_and_inlined_frames_of_a_hidden_class_are_hidden() {
        let (mut store, id) = store_with(vec![named_method("apply", "(I)I", Vec::new())]);
        let id = moved_to_a_nonzero_id(&mut store, id);
        {
            let class = store.get_mut(id).expect("class");
            class.name = Arc::from("probe/W37Gen$Hid/0x10");
            class.hidden = true;
        }
        let label: Arc<str> = Arc::from("probe/W37Gen$Hid/0x10.apply:(I)I");
        let compiled = BacktraceFrame::Compiled {
            label: Arc::clone(&label),
            owner_class_id: id.as_u32(),
            bci: 1,
            cp_stamp: None,
        };
        let inlined = BacktraceFrame::Inlined {
            label: Arc::clone(&label),
            class_id: id.as_u32(),
            bci: 1,
            cp_stamp: None,
        };
        let unrecorded = BacktraceFrame::Inlined {
            label: Arc::clone(&label),
            class_id: 0,
            bci: 1,
            cp_stamp: None,
        };
        assert_eq!(backtrace_frame_hidden_decided(&compiled), None);
        assert_eq!(backtrace_frame_hidden_decided(&inlined), None);
        assert_eq!(backtrace_frame_hidden_decided(&unrecorded), Some(false));
        assert!(backtrace_frame_is_hidden(&store, &compiled));
        // The memoized answer is the same.
        assert!(backtrace_frame_is_hidden(&store, &compiled));
        assert!(backtrace_frame_is_hidden(&store, &inlined));
        assert!(!backtrace_frame_is_hidden(&store, &unrecorded));

        let plain = BacktraceFrame::Compiled {
            label: Arc::from("probe/W37Plain.apply:(I)I"),
            owner_class_id: id.as_u32(),
            bci: 1,
            cp_stamp: None,
        };
        assert_eq!(backtrace_frame_hidden_decided(&plain), Some(false));
    }

    /// Interpreter round i1 wave 37: an `@Hidden` JDK method is hidden as a
    /// compiled activation, an inlined level and a walk's entry (with and
    /// without its method index); its unannotated overload is not, and the
    /// same annotation on a non-JDK class is ignored, as HotSpot honours it
    /// only in privileged code.
    #[test]
    fn an_annotated_jdk_method_is_hidden_in_every_frame_shape() {
        use cratonvm_reader::attribute::{Annotation, LazyAttribute};
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        let mut annotated = named_method("runWith", "(Ljava/lang/Runnable;)V", Vec::new());
        annotated
            .attributes
            .push(LazyAttribute::new_decoded(Attribute::RuntimeVisibleAnnotations(vec![
                Annotation {
                    type_index: 1,
                    element_value_pairs: Vec::new(),
                },
            ])));
        let plain = named_method("runWith", "()V", Vec::new());
        let (mut store, id) = store_with(vec![plain, annotated]);
        let id = moved_to_a_nonzero_id(&mut store, id);
        {
            let class = store.get_mut(id).expect("class");
            class.name = Arc::from("java/lang/W37Carrier");
            class.constant_pool = ConstantPool::new(vec![
                ConstantPoolEntry::Tombstone,
                ConstantPoolEntry::Utf8(Arc::from("Ljdk/internal/vm/annotation/Hidden;")),
            ]);
        }
        let label = "java/lang/W37Carrier.runWith:(Ljava/lang/Runnable;)V";
        let compiled = BacktraceFrame::Compiled {
            label: Arc::from(label),
            owner_class_id: id.as_u32(),
            bci: 0,
            cp_stamp: None,
        };
        let inlined = BacktraceFrame::Inlined {
            label: Arc::from(label),
            class_id: id.as_u32(),
            bci: 0,
            cp_stamp: None,
        };
        assert!(backtrace_frame_is_hidden(&store, &compiled));
        assert!(backtrace_frame_is_hidden(&store, &inlined));
        let overload = BacktraceFrame::Compiled {
            label: Arc::from("java/lang/W37Carrier.runWith:()V"),
            owner_class_id: id.as_u32(),
            bci: 0,
            cp_stamp: None,
        };
        assert!(!backtrace_frame_is_hidden(&store, &overload));

        let mut entry = synthetic_entry(Arc::from("java/lang/W37Carrier"), Arc::from("runWith"));
        entry.class_id = Some(id);
        entry.method_descriptor = Some(Arc::from("(Ljava/lang/Runnable;)V"));
        assert!(stack_entry_is_hidden(&store, &entry));
        entry.method_index = Some(1);
        assert!(stack_entry_is_hidden(&store, &entry));
        // A stale index (it names the overload) falls back to the scan.
        entry.method_index = Some(0);
        assert!(stack_entry_is_hidden(&store, &entry));

        store.get_mut(id).expect("class").name = Arc::from("com/example/W37Carrier");
        let mut app = entry.clone();
        app.class_name = Arc::from("com/example/W37Carrier");
        assert!(!stack_entry_is_hidden(&store, &app));
    }
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LineNumberEntry};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    use cratonvm_reader::method::ClassFileMethod;

    /// A deferred entry with no `method_index` — i.e. what the lock-free
    /// `capture_frames_no_lines` snapshot produces.
    fn entry_for(class_id: ClassId, method: &str, bci: i32) -> StackTraceEntry {
        StackTraceEntry {
            class_name: Arc::from("probe/Target"),
            method_name: Arc::from(method),
            method_descriptor: None,
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
            method_descriptor: None,
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: None,
            method_index: None,
        }
    }

    /// The property the retired `may_need_throwable_fill_frame_trim` pre-check
    /// existed for: `is_a` -- which takes the class-manager lock -- is asked
    /// only for a frame whose NAME already matched, so an ordinary trace never
    /// takes it. Plus the two phase rules: `fillInStackTrace` first, then
    /// `<init>`, each a prefix from the innermost end, each stopped by the
    /// first frame whose holder the throwable is not.
    #[test]
    fn fill_frame_trim_asks_is_a_only_for_a_matching_name() {
        let mut asked = 0usize;
        let plain = [point("P", "main", 1), point("P", "throwSite", 2)];
        let keep = throwable_fill_frame_keep_len(&plain, &mut |_, _| {
            asked += 1;
            true
        });
        assert_eq!((keep, asked), (2, 0));

        let built = [
            point("P", "main", 1),
            point("E", "<init>", 2),
            point("T", "<init>", 3),
            point("T", "fillInStackTrace", 4),
        ];
        assert_eq!(throwable_fill_frame_keep_len(&built, &mut |_, _| true), 1);

        // `fillInStackTrace` BELOW an `<init>` is past both prefixes.
        let reordered = [point("P", "fillInStackTrace", 1), point("E", "<init>", 2)];
        assert_eq!(throwable_fill_frame_keep_len(&reordered, &mut |_, _| true), 1);

        // A constructor of a class the throwable is not stops the phase.
        let foreign = [point("P", "main", 1), point("Other", "<init>", 2)];
        assert_eq!(
            throwable_fill_frame_keep_len(&foreign, &mut |_, name| name != "Other"),
            2
        );
        assert_eq!(throwable_fill_frame_keep_len::<StackTraceEntry>(&[], &mut |_, _| true), 0);
    }

    /// HotSpot `MaxJavaStackTraceDepth`: the cap keeps the INNERMOST entries,
    /// which are the LAST ones of an outermost-first trace, and `0` is
    /// unlimited.
    #[test]
    fn throwable_trace_cap_keeps_the_innermost_entries() {
        let trace: Vec<StackTraceEntry> = (0..10).map(|i| point("P", "m", i)).collect();
        let mut capped = trace.clone();
        cap_throwable_trace(&mut capped, 4);
        let bcis: Vec<i32> = capped.iter().map(|e| e.byte_code_index).collect();
        assert_eq!(
            bcis,
            vec![6, 7, 8, 9],
            "the throw site (last entry) survives"
        );

        let mut short = trace.clone();
        cap_throwable_trace(&mut short, 10);
        assert_eq!(short.len(), 10, "at the cap: untouched");
        let mut unlimited = trace;
        cap_throwable_trace(&mut unlimited, 0);
        assert_eq!(unlimited.len(), 10, "0 means unlimited");

        assert_eq!(MAX_JAVA_STACK_TRACE_DEPTH, 1024, "HotSpot's default");
        assert_eq!(throwable_capture_budget(0), usize::MAX);
        assert!(throwable_capture_budget(MAX_JAVA_STACK_TRACE_DEPTH) > MAX_JAVA_STACK_TRACE_DEPTH);
    }

    /// Interpreter frames `probe/Deep.m0 .. m{n-1}` (outermost first) followed
    /// by `probe/Deep.<init>` x `inits`, each at a distinct bci.
    fn deep_frames(n: usize, inits: usize) -> Vec<Frame> {
        (0..n + inits)
            .map(|i| {
                let name = if i < n {
                    format!("m{i}")
                } else {
                    "<init>".to_string()
                };
                let mut f = Frame::new(
                    ClassId::new(1),
                    "probe/Deep".to_string(),
                    name,
                    "()V".to_string(),
                    None,
                    vec![0xb1],
                    Vec::new(),
                    1,
                    1,
                    &[],
                );
                f.last_instr_pc = i;
                f
            })
            .collect()
    }

    /// The bounded capture (only `max_depth + slack` innermost interpreter
    /// frames walked) gives exactly the unbounded capture's capped answer --
    /// including when the fill-frame trim eats more than the slack and the
    /// capture has to be redone unbounded.
    #[test]
    fn bounded_throwable_capture_matches_the_unbounded_one() {
        if !crate::jit::conservative_roots::active_compiled_frames().is_empty() {
            // Compiled frames on this thread take the unbounded path anyway.
            return;
        }
        let bcis =
            |t: &[StackTraceEntry]| -> Vec<i32> { t.iter().map(|e| e.byte_code_index).collect() };
        // Holder class 1 is the throwable's class: every `<init>` is trimmed.
        let mut trim_all = |holder: Option<ClassId>, _: &str| holder == Some(ClassId::new(1));
        for (n, inits, max_depth) in [
            (300, 0, 10),
            (300, 3, 10),
            (300, 64, 10),  // the trim leaves exactly `max_depth` in the window
            (300, 65, 10),  // ... one fewer: redone unbounded
            (300, 100, 10), // the trim eats all of the 64-frame slack
            (20, 3, 10),    // shallower than the budget: no window
            (300, 3, 0),    // unlimited
        ] {
            let frames = deep_frames(n, inits);
            let mut full = capture_full_trace_without_store(&frames);
            full.truncate(n);
            cap_throwable_trace(&mut full, max_depth);
            let got = capture_throwable_trace_without_store(&frames, max_depth, &mut trim_all);
            assert_eq!(
                bcis(&got),
                bcis(&full),
                "n={n} inits={inits} max_depth={max_depth}"
            );
            let expect_len = if max_depth == 0 { n } else { n.min(max_depth) };
            assert_eq!(got.len(), expect_len);
            assert_eq!(got.last().map(|e| e.byte_code_index), Some(n as i32 - 1));
        }
    }

    /// Stage 1a of the compact-backtrace proposal: the retained form
    /// ([`BacktraceFrame`]) materialises into exactly the entries the eager
    /// capture builds, field for field, for cached and owned interpreter
    /// frames alike, under the fill-frame trim and the depth cap.
    #[test]
    fn backtrace_capture_materialises_the_eager_entries() {
        if !crate::jit::conservative_roots::active_compiled_frames().is_empty() {
            return;
        }
        let cached = Arc::new(
            crate::classloading::resolution::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: ClassId::new(2),
                    class_name: Arc::from("probe/Cached"),
                    method_name: Arc::from("hot"),
                    method_descriptor: Arc::from("()V"),
                    source_file: Some(Arc::from("Cached.java")),
                    code: Arc::from(vec![0xb1u8]),
                    exception_table: Arc::from(Vec::new()),
                    max_stack: 1,
                    max_locals: 1,
                    num_params: 0,
                    is_synchronized: false,
                    is_static: true,
                },
            ),
        );
        // 40 named frames and 3 trailing `<init>`s of the throwable's class
        // (trimmed); every third named frame runs the cached method.
        let mut frames = deep_frames(40, 3);
        for i in (0..40).step_by(3) {
            let mut f = Frame::new_pooled_cached(
                Arc::clone(&cached),
                &[],
                &mut Vec::new(),
                &mut Vec::new(),
            );
            f.last_instr_pc = 1000 + i;
            frames[i] = f;
        }
        let key = |e: &StackTraceEntry| {
            (
                e.class_name.to_string(),
                e.method_name.to_string(),
                e.method_descriptor.as_deref().map(str::to_string),
                e.source_file.as_deref().map(str::to_string),
                e.line_number,
                e.byte_code_index,
                e.class_id,
                e.method_index,
            )
        };
        let mut trim_all = |holder: Option<ClassId>, _: &str| holder == Some(ClassId::new(1));
        for max_depth in [0, 10, 1024] {
            let eager = capture_throwable_trace_without_store(&frames, max_depth, &mut trim_all);
            let compact =
                capture_throwable_backtrace_without_store(&frames, max_depth, &mut trim_all, &[]);
            assert!(
                compact
                    .iter()
                    .any(|f| matches!(f, BacktraceFrame::Method { .. })),
                "cached frames must be retained compactly (max_depth={max_depth})"
            );
            let got: Vec<_> = compact.iter().map(|f| key(&f.to_entry())).collect();
            let want: Vec<_> = eager.iter().map(key).collect();
            assert_eq!(got, want, "max_depth={max_depth}");
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

    // -- the trap snapshot: still-live frames and the OSR override ------

    fn interp_frame(method: &str, class_id: ClassId, last_instr_pc: usize) -> Frame {
        let mut f = Frame::new(
            class_id,
            "probe/Target".to_string(),
            method.to_string(),
            "()V".to_string(),
            None,
            vec![0; 128],
            vec![],
            4,
            4,
            &[],
        );
        f.last_instr_pc = last_instr_pc;
        f
    }

    fn compiled_at(interp_depth: u32, method: &str, bci: i32) -> ActiveCompiledFrame {
        ActiveCompiledFrame {
            interp_depth,
            chain_index: 0,
            label: Arc::from(format!("probe/Target.{method}:()V")),
            owner_class_id: 0,
            cm_ptr: 0,
            bci,
            inline_chain: Vec::new(),
            chain_exact: false,
            trap_site_exact: false,
            compile_cp_stamp: None,
        }
    }

    fn captured(class_id: ClassId, method: &str, bci: i32) -> StackTraceEntry {
        StackTraceEntry {
            method_descriptor: Some(Arc::from("()V")),
            ..entry_for(class_id, method, bci)
        }
    }

    /// A compiled frame an interpreter frame still sits above cannot have
    /// unwound, so the late capture already walked it; only the frames above
    /// the innermost interpreter frame are the snapshot's to recover. The
    /// `--jdk-only` + `CRATONVM_BG_COMPILE=0` shape: `main` compiled at depth 0
    /// under two interpreted callers, `leaf` compiled above them.
    #[test]
    fn a_compiled_frame_under_a_live_interpreter_frame_is_not_spliced_again() {
        let cid = ClassId::new(1);
        let frames = vec![interp_frame("probe", cid, 7), interp_frame("outer", cid, 1)];
        let kept = drop_still_live(
            &frames,
            vec![compiled_at(0, "main", 69), compiled_at(2, "leaf", 13)],
        );
        let names: Vec<&str> = kept.iter().map(|f| &*f.label).collect();
        assert_eq!(names, vec!["probe/Target.leaf:()V"]);
    }

    /// Every frame at or above the innermost interpreter frame is kept -- which
    /// includes an OSR continuation of that frame, recorded at exactly
    /// `frames.len()`, so it still reaches the override.
    #[test]
    fn frames_that_unwound_are_all_kept() {
        let cid = ClassId::new(1);
        let frames = vec![interp_frame("main", cid, 84)];
        let kept = drop_still_live(
            &frames,
            vec![compiled_at(1, "main", 130), compiled_at(1, "probe", 7)],
        );
        assert_eq!(kept.len(), 2);
    }

    /// The override re-points the entry of ITS frame and re-resolves the line.
    /// Two activations of one method at one bci: the in-order walk must pick
    /// the second entry for frame 1, not the first entry that matches by name.
    #[test]
    fn an_osr_override_repoints_its_own_frames_entry() {
        let (store, cid) = store_with(vec![named_method(
            "rec",
            "()V",
            vec![
                LineNumberEntry {
                    start_pc: 0,
                    line_number: 10,
                },
                LineNumberEntry {
                    start_pc: 50,
                    line_number: 20,
                },
            ],
        )]);
        let frames = vec![interp_frame("rec", cid, 7), interp_frame("rec", cid, 7)];
        let mut trace = vec![captured(cid, "rec", 7), captured(cid, "rec", 7)];
        let mut overrides = OsrBciOverrides::default();
        overrides.insert(1, (60, Vec::new()));
        apply_snapshot_osr_overrides(&store, &frames, &overrides, &mut trace);
        assert_eq!(trace[0].byte_code_index, 7);
        assert_eq!(trace[1].byte_code_index, 60);
        assert_eq!(trace[1].line_number, 20);
    }

    /// A frame whose entry is not in the capture ends the walk and leaves the
    /// trace exactly as it was: a stale line, never a moved one.
    #[test]
    fn an_override_with_no_matching_entry_changes_nothing() {
        let (store, cid) = store_with(vec![named_method("rec", "()V", Vec::new())]);
        let frames = vec![interp_frame("rec", cid, 7)];
        let mut trace = vec![captured(cid, "rec", 9)];
        let mut overrides = OsrBciOverrides::default();
        overrides.insert(0, (60, Vec::new()));
        apply_snapshot_osr_overrides(&store, &frames, &overrides, &mut trace);
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].byte_code_index, 9);
        assert_eq!(trace[0].line_number, LINE_NUMBER_UNKNOWN);
    }

    // -- round 12 wave 4 (exc2): the armed trap snapshot (W3-2) ------------

    fn names_and_bcis(trace: &[StackTraceEntry]) -> Vec<(String, i32)> {
        trace
            .iter()
            .map(|e| (e.method_name.to_string(), e.byte_code_index))
            .collect()
    }

    /// While armed, a throwable capture lays the snapshot out as its compiled
    /// frames -- below the constructor frames, which the fill-frame trim then
    /// removes -- and a disarmed capture walks as before.
    #[test]
    fn an_armed_trap_snapshot_stands_in_for_the_compiled_walk() {
        let cid = ClassId::new(1);
        let base = vec![interp_frame("main", cid, 3), interp_frame("outer", cid, 5)];
        let snapshot = vec![compiled_at(2, "catcher", 11), compiled_at(2, "leaf", 13)];
        assert!(trap_snapshot_can_stand_in_for_walk(&base, &snapshot));
        let building = vec![
            interp_frame("main", cid, 3),
            interp_frame("outer", cid, 5),
            interp_frame("<init>", cid, 0),
            interp_frame("fillInStackTrace", cid, 0),
        ];
        let mut is_a = |_: Option<ClassId>, _: &str| true;

        let window = arm_trap_capture(snapshot, base.len()).expect("nothing armed yet");
        // A second door cannot arm over the first.
        assert!(arm_trap_capture(vec![compiled_at(2, "other", 1)], base.len()).is_err());
        // A capture below the armed depth is not the one being built.
        let shallow = capture_throwable_trace_without_store(&base[..1], 0, &mut is_a);
        assert_eq!(names_and_bcis(&shallow), vec![("main".to_string(), 3)]);
        let whole = capture_throwable_trace_without_store(&building, 0, &mut is_a);
        assert_eq!(
            names_and_bcis(&whole),
            vec![
                ("main".to_string(), 3),
                ("outer".to_string(), 5),
                ("catcher".to_string(), 11),
                ("leaf".to_string(), 13),
            ]
        );
        let (back, used) = window.disarm();
        assert!(used);
        assert_eq!(back.map(|f| f.len()), Some(2));

        let walked = capture_throwable_trace_without_store(&building, 0, &mut is_a);
        assert_eq!(
            names_and_bcis(&walked),
            vec![("main".to_string(), 3), ("outer".to_string(), 5)]
        );
    }

    /// Round 12 wave 5 (lane exc3), W4-3: compiled and inlined frames are
    /// retained as their shared label and materialise into exactly the entries
    /// the eager capture builds -- labelled, inlined (with and without a
    /// recorded class id), line-less (`bci == -1`) -- while an unparsable label
    /// and an out-of-bound inlined bci are dropped by both forms alike.
    #[test]
    fn compiled_frames_are_retained_compactly_and_materialise_identically() {
        let cid = ClassId::new(1);
        let base = vec![interp_frame("main", cid, 3)];
        let level = |label: &str, bci: u32, class_id: u32| InlinedLevel {
            label: Arc::from(label),
            bci,
            class_id,
        };
        let mut spliced = compiled_at(1, "catcher", 11);
        spliced.owner_class_id = 7;
        // Innermost first, as the walk carries it.
        spliced.inline_chain = vec![
            level("probe/Target.inner:(I)I", 4, 0),
            level("probe/Other.mid:()V", 9, 5),
        ];
        let mut lineless = compiled_at(1, "noline", -1);
        lineless.owner_class_id = 7;
        // Innermost first: `far` (out of bound) sits BELOW `deep`, so the
        // chain keeps `deep` and ends at `far`.
        let mut cut = compiled_at(1, "cut", 2);
        cut.inline_chain = vec![
            level("probe/Target.far:()V", 70_000, 0),
            level("probe/Target.deep:()V", 1, 0),
        ];
        let garbled = ActiveCompiledFrame {
            label: Arc::from("no-separators"),
            ..compiled_at(1, "x", 1)
        };
        let snapshot = vec![spliced, lineless, cut, garbled, compiled_at(1, "leaf", 13)];
        let building = vec![
            interp_frame("main", cid, 3),
            interp_frame("<init>", cid, 0),
            interp_frame("fillInStackTrace", cid, 0),
        ];
        let key = |e: &StackTraceEntry| {
            (
                e.class_name.to_string(),
                e.method_name.to_string(),
                e.method_descriptor.as_deref().map(str::to_string),
                e.source_file.as_deref().map(str::to_string),
                e.line_number,
                e.byte_code_index,
                e.class_id,
                e.method_index,
            )
        };
        let mut is_a = |_: Option<ClassId>, _: &str| true;
        let window = arm_trap_capture(snapshot.clone(), base.len()).expect("nothing armed yet");
        let eager = capture_throwable_trace_without_store(&building, 0, &mut is_a);
        drop(window);
        let window = arm_trap_capture(snapshot, base.len()).expect("the first window closed");
        let compact = capture_throwable_backtrace_without_store(&building, 0, &mut is_a, &[]);
        drop(window);
        if compact_compiled_backtrace_enabled() {
            assert!(compact
                .iter()
                .any(|f| matches!(f, BacktraceFrame::Compiled { .. })));
            assert!(compact
                .iter()
                .any(|f| matches!(f, BacktraceFrame::Inlined { .. })));
        }
        let got: Vec<_> = compact.iter().map(|f| key(&f.to_entry())).collect();
        let want: Vec<_> = eager.iter().map(key).collect();
        assert_eq!(got, want);
        // main, catcher, mid, inner, noline, cut, deep (far cuts the chain),
        // leaf; the garbled label yields nothing.
        assert_eq!(
            eager.iter().map(|e| e.method_name.to_string()).collect::<Vec<_>>(),
            vec!["main", "catcher", "mid", "inner", "noline", "cut", "deep", "leaf"]
        );
    }

    /// Dropping the window disarms, so a construction that unwound by panic
    /// cannot hand its snapshot to the next capture on the thread.
    #[test]
    fn a_dropped_trap_capture_window_disarms() {
        let cid = ClassId::new(1);
        let base = vec![interp_frame("main", cid, 3)];
        {
            let _window = arm_trap_capture(vec![compiled_at(1, "leaf", 13)], base.len())
                .expect("nothing armed yet");
        }
        let mut is_a = |_: Option<ClassId>, _: &str| true;
        let walked = capture_throwable_trace_without_store(&base, 0, &mut is_a);
        assert_eq!(names_and_bcis(&walked), vec![("main".to_string(), 3)]);
        let window = arm_trap_capture(vec![compiled_at(1, "leaf", 13)], base.len())
            .expect("the dropped window disarmed");
        let (back, used) = window.disarm();
        assert!(!used);
        assert_eq!(back.map(|f| f.len()), Some(1));
    }

    /// Only a snapshot lying wholly above the innermost interpreter frame can
    /// replace the walk.
    #[test]
    fn a_snapshot_under_or_past_the_interpreter_top_keeps_the_splice() {
        let cid = ClassId::new(1);
        let base = vec![interp_frame("main", cid, 3), interp_frame("outer", cid, 5)];
        assert!(!trap_snapshot_can_stand_in_for_walk(&base, &[]));
        assert!(!trap_snapshot_can_stand_in_for_walk(
            &base,
            &[compiled_at(1, "under", 4), compiled_at(2, "leaf", 13)]
        ));
        assert!(!trap_snapshot_can_stand_in_for_walk(
            &base,
            &[compiled_at(3, "past", 4)]
        ));
    }

    /// Interpreter round i1 wave 44, lane L3: a record kept outside the JIT
    /// entry chain (a reflective call, a JNI native's JVMTI row) goes right
    /// before the first slot pushed after it -- above the compiled
    /// activations at its depth that were entered before it (chain index
    /// below the chain length it recorded), below those entered after it
    /// and below the interpreter frame it pushed.
    #[test]
    fn a_record_outside_the_chain_goes_between_the_activations_entered_before_and_after_it() {
        let cid = ClassId::new(1);
        let entered = |depth: u32, method: &str, chain_index: u32| ActiveCompiledFrame {
            chain_index,
            ..compiled_at(depth, method, 1)
        };
        // main, a (interpreted); `caller` compiled from a (chain entry 0),
        // the record made in it (chain length 1), `target` compiled after it
        // (entry 1) at the same depth, then `b` interpreted above them.
        let frames = vec![
            interp_frame("main", cid, 0),
            interp_frame("a", cid, 0),
            interp_frame("b", cid, 0),
        ];
        let jit = vec![entered(2, "caller", 0), entered(2, "target", 1)];
        let no_osr = OsrBciOverrides::default();
        let slots = collect_trace_slots(&frames, &jit, &no_osr);
        let name = |i: usize| slots[i].build().map(|e| e.method_name.to_string());
        let order: Vec<Option<String>> = (0..slots.len()).map(name).collect();
        assert_eq!(
            order,
            ["main", "a", "caller", "target", "b"].map(|n| Some(n.to_string())).to_vec()
        );
        assert_eq!(trace_anchor_position(&slots, 2, 1), Some(3), "above caller, below target");
        assert_eq!(trace_anchor_position(&slots, 2, 0), Some(2), "below both activations");
        assert_eq!(trace_anchor_position(&slots, 2, 2), Some(4), "entered after both: below b");
        assert_eq!(trace_anchor_position(&slots, 1, 0), Some(1), "made in main: below a");
        assert_eq!(trace_anchor_position(&slots, 3, 2), None, "innermost: nothing above");
        // Without `b`: the target is the only thing the record's call pushed.
        let slots = collect_trace_slots(&frames[..2], &jit, &no_osr);
        assert_eq!(trace_anchor_position(&slots, 2, 1), Some(3));
        assert_eq!(trace_anchor_position(&slots, 2, 2), None);
    }
    /// Interpreter round i1 wave 45, lane L3: a published trace lists a
    /// reflective call's JDK frames from the thread's memo alone, at the
    /// placement the thread's own capture uses, and shifts every interpreter
    /// frame's published position past them; frames memoized at another
    /// redefinition count are not listed.
    #[test]
    fn a_published_trace_lists_the_memoized_reflective_frames_and_shifts_the_positions() {
        let cid = ClassId::new(1);
        // main -> (Method.invoke) -> target -> park
        let frames = vec![
            interp_frame("main", cid, 0),
            interp_frame("target", cid, 0),
            interp_frame("park", cid, 0),
        ];
        let calls = [ReflectiveCallRow {
            interp_depth: 1,
            jit_depth: 0,
            kind: cratonvm_native_api::ReflectiveCallKind::MethodInvoke,
            check: None,
        }];
        let jdk: Arc<[StackTraceEntry]> =
            Arc::from(vec![entry_for(cid, "invoke", 7), entry_for(cid, "accessorInvoke", 9)]);
        let mut named: ReflectiveFramesNamed = Default::default();
        named[usize::from(reflective_kind_bit(calls[0].kind)) - 1] = Some((5, Some(jdk)));
        let no_osr = OsrBciOverrides::default();
        let publish = |count: u64| {
            let mut trace = PublishedTrace {
                entries: capture_frames_no_lines(&frames),
                frame_positions: None,
            };
            splice_published_reflective_frames(&frames, &[], &no_osr, &calls, &named, count, &mut trace);
            trace
        };
        let trace = publish(5);
        let order: Vec<&str> = trace.entries.iter().map(|e| &*e.method_name).collect();
        assert_eq!(order, ["main", "invoke", "accessorInvoke", "target", "park"]);
        assert_eq!(trace.frame_positions.as_deref(), Some(&[0u32, 3, 4][..]));
        // Named at another count: stale, nothing listed, positions untouched.
        let stale = publish(6);
        assert_eq!(stale.entries.len(), 3);
        assert_eq!(stale.frame_positions, None);
        // A call that pushed nothing yet (parked in the native itself).
        let mut trace = PublishedTrace {
            entries: capture_frames_no_lines(&frames[..1]),
            frame_positions: None,
        };
        splice_published_reflective_frames(&frames[..1], &[], &no_osr, &calls, &named, 5, &mut trace);
        assert_eq!(trace.entries.len(), 1);
    }

    /// Interpreter round i1 wave 45, lane L3: a throwable filled inside a
    /// reflective call with nothing of the call above it -- its first pushed
    /// frame is the first one the fill-frame trim removed, or it pushed none
    /// -- lists the call's frames on top: its `raise` frames when it has
    /// them, else its frames at their call lines. A call past `keep` still
    /// lists nothing.
    #[test]
    fn a_throwable_raised_by_a_reflective_native_lists_the_call_on_top() {
        let cid = ClassId::new(1);
        let frames = vec![
            interp_frame("main", cid, 0),
            interp_frame("<init>", cid, 0),
            interp_frame("fillInStackTrace", cid, 0),
        ];
        let no_osr = OsrBciOverrides::default();
        let slots = collect_trace_slots(&frames, &[], &no_osr);
        let splice = |raise: Option<&str>| ReflectiveSplice {
            interp_depth: 1,
            jit_depth: 0,
            entries: Arc::from(vec![entry_for(cid, "invoke", 7), entry_for(cid, "accessorInvoke", 9)]),
            raise: raise.map(|m| {
                Arc::from(vec![entry_for(cid, "invoke", 7), entry_for(cid, m, 30)])
            }),
        };
        let names = |built: Vec<StackTraceEntry>| -> Vec<String> {
            built.iter().map(|e| e.method_name.to_string()).collect()
        };
        // keep = 1: `<init>` and `fillInStackTrace` trimmed; the call's first
        // pushed slot is slot 1.
        let splices = [splice(Some("accessorRaise"))];
        let positions = reflective_splice_positions(&slots, &splices, true);
        assert_eq!(positions, vec![(1, 0)]);
        let built: Vec<StackTraceEntry> = build_slots_with_splices(&slots, 1, &splices, &positions);
        assert_eq!(names(built), ["main", "invoke", "accessorRaise"]);
        let splices = [splice(None)];
        let built: Vec<StackTraceEntry> = build_slots_with_splices(&slots, 1, &splices, &positions);
        assert_eq!(names(built), ["main", "invoke", "accessorInvoke"]);
        // Inside the trimmed frames (the call was made by `<init>`): nothing.
        let deeper = [ReflectiveSplice {
            interp_depth: 2,
            ..splice(None)
        }];
        let positions = reflective_splice_positions(&slots, &deeper, true);
        let built: Vec<StackTraceEntry> = build_slots_with_splices(&slots, 1, &deeper, &positions);
        assert_eq!(names(built), ["main"]);
        // Nothing pushed at all: on top of an untrimmed capture; the walks
        // (`unanchored_on_top == false`) still leave it out.
        let top = [ReflectiveSplice {
            interp_depth: 3,
            ..splice(Some("accessorRaise"))
        }];
        let positions = reflective_splice_positions(&slots, &top, true);
        assert_eq!(positions, vec![(3, 0)]);
        let built: Vec<StackTraceEntry> = build_slots_with_splices(&slots, 3, &top, &positions);
        assert_eq!(
            names(built),
            ["main", "<init>", "fillInStackTrace", "invoke", "accessorRaise"]
        );
        assert!(reflective_splice_positions(&slots, &top, false).is_empty());
    }
}

/// Round 13 wave 13 (lane trace3): [`native_standin_frames`] over JDK-25-shaped
/// `Thread` / `Object` bodies.
#[cfg(test)]
mod r13_trace3_standin_tests {
    use super::*;
    use crate::classloading::{ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute, LineNumberEntry};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    const INTERRUPTED: &str = "java/lang/InterruptedException";
    const MONITOR: &str = "java/lang/IllegalMonitorStateException";

    /// Methodref `k` of `refs` lands at constant-pool index `6k + 6`.
    pub(super) fn pool(refs: &[(&str, &str, &str)]) -> ConstantPool {
        let mut entries = vec![ConstantPoolEntry::Tombstone];
        for (k, (owner, name, descriptor)) in refs.iter().enumerate() {
            let base = (1 + 6 * k) as u16;
            entries.push(ConstantPoolEntry::Utf8(Arc::from(*owner)));
            entries.push(ConstantPoolEntry::ClassReference { name_index: base });
            entries.push(ConstantPoolEntry::Utf8(Arc::from(*name)));
            entries.push(ConstantPoolEntry::Utf8(Arc::from(*descriptor)));
            entries.push(ConstantPoolEntry::NameAndType {
                name_index: base + 2,
                descriptor_index: base + 3,
            });
            entries.push(ConstantPoolEntry::MethodReference {
                class_index: base + 1,
                name_and_type_index: base + 4,
            });
        }
        ConstantPool::new(entries)
    }

    pub(super) fn java(
        name: &str,
        descriptor: &str,
        code: Vec<u8>,
        lines: &[(u16, u16)],
    ) -> ClassFileMethod {
        let code = CodeAttribute {
            max_stack: 4,
            max_locals: 4,
            code: cratonvm_reader::ByteView::from_vec(code),
            exception_table: Vec::new(),
            attributes: vec![Attribute::LineNumberTable(
                lines
                    .iter()
                    .map(|&(start_pc, line_number)| LineNumberEntry {
                        start_pc,
                        line_number,
                    })
                    .collect(),
            )],
        };
        ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(code))],
        }
    }

    pub(super) fn native(name: &str, descriptor: &str) -> ClassFileMethod {
        ClassFileMethod {
            access_flags: MethodAccessFlags::PRIVATE | MethodAccessFlags::NATIVE,
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            attributes: Vec::new(),
        }
    }

    pub(super) fn add_class(
        store: &mut ClassStore,
        name: &str,
        superclass: Option<ClassId>,
        constant_pool: ConstantPool,
        methods: Vec<ClassFileMethod>,
    ) -> ClassId {
        let id = store.next_id();
        let simple = name.rsplit('/').next().unwrap_or(name);
        let class = Class {
            id,
            loader_id: ClassLoaderId::Bootstrap,
            name: Arc::from(name),
            source_file: Some(format!("{simple}.java")),
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool,
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass,
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
        store.add(class)
    }

    struct Fixture {
        store: ClassStore,
        main: ClassId,
        thread: ClassId,
    }

    /// `Object` / `Thread` as JDK 25 compiles them (lines as in its sources),
    /// and a `probe/Main` whose `main()V` calls, at bci 1 / 5 / 9 / 13:
    /// `Thread.sleep(long)`, `Worker.sleep(long)` (inherited),
    /// `HiddenWorker.sleep(long)` (its own static hides `Thread`'s) and
    /// `this.wait()`.
    fn fixture() -> Fixture {
        let mut store = ClassStore::new();
        let object = add_class(
            &mut store,
            "java/lang/Object",
            None,
            pool(&[
                ("java/lang/Object", "wait0", "(J)V"),
                ("java/lang/Object", "wait", "(J)V"),
            ]),
            vec![
                // wait() { wait(0L); }
                java("wait", "()V", vec![0x2a, 0x09, 0xb6, 0, 12, 0xb1], &[(0, 351)]),
                // wait(long): the virtual-thread wait0 at pc 2, the platform one at pc 7.
                java(
                    "wait",
                    "(J)V",
                    vec![0x2a, 0x1f, 0xb7, 0, 6, 0x2a, 0x1f, 0xb7, 0, 6, 0xb1],
                    &[(0, 383), (5, 389)],
                ),
                native("wait0", "(J)V"),
            ],
        );
        let thread = add_class(
            &mut store,
            "java/lang/Thread",
            Some(object),
            pool(&[
                ("java/lang/Thread", "sleepNanos", "(J)V"),
                ("java/lang/Thread", "sleepNanos0", "(J)V"),
                ("java/lang/VirtualThread", "sleepNanos", "(J)V"),
            ]),
            vec![
                java("sleep", "(J)V", vec![0x1e, 0xb8, 0, 6, 0xb1], &[(0, 535), (1, 540)]),
                // The virtual-thread call (pc 1) is not a census method; the
                // `sleepNanos0` call is at pc 5.
                java(
                    "sleepNanos",
                    "(J)V",
                    vec![0x1e, 0xb8, 0, 18, 0x1e, 0xb8, 0, 12, 0xb1],
                    &[(0, 507), (4, 509)],
                ),
                native("sleepNanos0", "(J)V"),
            ],
        );
        add_class(&mut store, "probe/Worker", Some(thread), pool(&[]), Vec::new());
        add_class(
            &mut store,
            "probe/HiddenWorker",
            Some(thread),
            pool(&[]),
            vec![java("sleep", "(J)V", vec![0xb1], &[(0, 1)])],
        );
        let main = add_class(
            &mut store,
            "probe/Main",
            Some(object),
            pool(&[
                ("java/lang/Thread", "sleep", "(J)V"),
                ("probe/Worker", "sleep", "(J)V"),
                ("probe/Main", "wait", "()V"),
                ("probe/HiddenWorker", "sleep", "(J)V"),
            ]),
            vec![java(
                "main",
                "()V",
                vec![
                    0x09, 0xb8, 0, 6, 0x09, 0xb8, 0, 12, 0x09, 0xb8, 0, 24, 0x2a, 0xb6, 0, 18, 0xb1,
                ],
                &[(0, 3), (4, 4), (8, 5), (12, 6)],
            )],
        );
        Fixture {
            store,
            main,
            thread,
        }
    }

    fn at(class_id: ClassId, name: &str, descriptor: &str, bci: i32) -> BacktraceFrame {
        BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from("unused"),
            method_name: Arc::from(name),
            method_descriptor: Some(Arc::from(descriptor)),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(class_id),
            method_index: None,
        })
    }

    /// `(class.method, line, bci)` of each rebuilt frame, outermost first.
    fn expand(
        fx: &Fixture,
        innermost: &BacktraceFrame,
        throwable: &str,
    ) -> Option<Vec<(String, i32, i32)>> {
        let store = &fx.store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        native_standin_frames(store, innermost, throwable, &mut resolve).map(|frames| {
            frames
                .iter()
                .map(|f| {
                    let e = f.to_entry();
                    (
                        format!("{}.{}", e.class_name, e.method_name),
                        e.line_number,
                        e.byte_code_index,
                    )
                })
                .collect()
        })
    }

    fn row(frame: &str, line: i32, bci: i32) -> (String, i32, i32) {
        (frame.to_string(), line, bci)
    }

    /// `--compatible`: the registered `Thread.sleep` ran; all three JDK frames
    /// come back, each Java level at its call of the next, the leaf native.
    #[test]
    fn a_registered_sleep_gets_hotspots_three_jdk_frames() {
        let fx = fixture();
        let got = expand(&fx, &at(fx.main, "main", "()V", 1), INTERRUPTED);
        assert_eq!(
            got,
            Some(vec![
                row("java/lang/Thread.sleep", 540, 1),
                row("java/lang/Thread.sleepNanos", 509, 5),
                row("java/lang/Thread.sleepNanos0", LINE_NUMBER_NATIVE, -1),
            ])
        );
    }

    /// `--jdk-only`: `sleep` and `sleepNanos` ran as bytecode; only the native
    /// leaf is missing.
    #[test]
    fn real_sleep_bytecode_gets_only_the_native_leaf() {
        let fx = fixture();
        let got = expand(&fx, &at(fx.thread, "sleepNanos", "(J)V", 5), INTERRUPTED);
        assert_eq!(
            got,
            Some(vec![row("java/lang/Thread.sleepNanos0", LINE_NUMBER_NATIVE, -1)])
        );
    }

    /// The leaf's own frame names its holder's source file, as HotSpot's does.
    #[test]
    fn the_native_leaf_names_the_source_file() {
        let fx = fixture();
        let frames = native_standin_frames(
            &fx.store,
            &at(fx.thread, "sleepNanos", "(J)V", 5),
            INTERRUPTED,
            &mut |_, name| fx.store.iter().find(|c| &*c.name == name).map(|c| c.id),
        )
        .expect("a leaf");
        assert_eq!(frames[0].to_entry().source_file.as_deref(), Some("Thread.java"));
    }

    /// Only the throwables raised INSIDE the leaf: an `IllegalArgumentException`
    /// keeps the caller on top when `sleep`'s bytecode constructs none (round
    /// 14 wave 2: it gets `sleep`'s own frame when it does, see
    /// `an_argument_check_gets_the_one_frame_that_throws`).
    #[test]
    fn other_throwables_keep_the_caller_on_top() {
        let fx = fixture();
        let site = at(fx.main, "main", "()V", 1);
        assert_eq!(expand(&fx, &site, "java/lang/IllegalArgumentException"), None);
        assert_eq!(expand(&fx, &site, MONITOR), None, "IMSE is a wait-family throwable");
    }

    /// A call qualified by a subclass resolves up to `Thread`, unless the
    /// subclass hides `sleep` with its own static.
    #[test]
    fn a_subclass_qualified_call_resolves_unless_hidden() {
        let fx = fixture();
        let inherited = expand(&fx, &at(fx.main, "main", "()V", 5), INTERRUPTED);
        assert_eq!(inherited.map(|v| v.len()), Some(3));
        assert_eq!(expand(&fx, &at(fx.main, "main", "()V", 9), INTERRUPTED), None);
    }

    /// `wait()` -> `wait(long)` at its LAST `wait0` call (the platform-thread
    /// branch) -> `wait0`, for both wait-family throwables.
    #[test]
    fn wait_rebuilds_the_platform_thread_chain() {
        let fx = fixture();
        for throwable in [INTERRUPTED, MONITOR] {
            let got = expand(&fx, &at(fx.main, "main", "()V", 13), throwable);
            assert_eq!(
                got,
                Some(vec![
                    row("java/lang/Object.wait", 351, 2),
                    row("java/lang/Object.wait", 389, 7),
                    row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
                ]),
                "{throwable}"
            );
        }
    }

    /// A frame not standing at an invoke, or an unknown bci, gets nothing.
    #[test]
    fn a_frame_not_at_an_invoke_gets_nothing() {
        let fx = fixture();
        assert_eq!(expand(&fx, &at(fx.main, "main", "()V", 0), INTERRUPTED), None);
        assert_eq!(expand(&fx, &at(fx.main, "main", "()V", -1), INTERRUPTED), None);
        assert!(!is_standin_throwable("java/lang/RuntimeException"));
        assert!(is_standin_throwable(INTERRUPTED));
    }

    /// Interpreter round i1 wave 46, lane L3: a reflective call's failed
    /// argument check lists the accessor at the constructor call of its
    /// arm's `new IllegalArgumentException` (the constructor accessor has
    /// three: count, type, null to a primitive); a throwable of another class,
    /// a check the accessor has no frame for, or a class that is not loaded
    /// gets none.
    #[test]
    fn a_reflective_check_stands_at_its_arms_constructor_call() {
        use cratonvm_native_api::{ReflectiveCallKind, ReflectiveCheck};
        const IAE: &str = "java/lang/IllegalArgumentException";
        // Three times `new #2; dup; aconst_null; invokespecial #6; athrow`.
        let mut code: Vec<u8> = Vec::new();
        for _ in 0..3 {
            code.extend([0xbb, 0, 2, 0x59, 0x01, 0xb7, 0, 6, 0xbf]);
        }
        let mut store = ClassStore::new();
        let id = add_class(
            &mut store,
            "jdk/internal/reflect/DirectConstructorHandleAccessor",
            None,
            pool(&[(IAE, "<init>", "(Ljava/lang/Throwable;)V")]),
            vec![java(
                "newInstance",
                "([Ljava/lang/Object;)Ljava/lang/Object;",
                code,
                &[(0, 59), (9, 65), (18, 70)],
            )],
        );
        let class = store.get(id).unwrap();
        let method = &class.methods[0];
        assert_eq!(
            throwable_init_sites(&class.constant_pool, &method.code().unwrap().code, IAE),
            Some(vec![5, 14, 23])
        );
        let outer = standin_entry(class, id, method, 0, Some(0));
        let entries = vec![outer.clone(), outer.clone(), outer];
        let top = |throwable: &str, check: ReflectiveCheck| {
            reflective_check_entries(
                &store,
                ReflectiveCallKind::ConstructorNewInstance,
                &entries,
                throwable,
                check,
            )
            .map(|listed| {
                let top = listed.last().unwrap();
                (listed.len(), top.line_number, top.byte_code_index)
            })
        };
        assert_eq!(top(IAE, ReflectiveCheck::ArgumentCount), Some((3, 59, 5)));
        assert_eq!(top(IAE, ReflectiveCheck::ArgumentType), Some((3, 65, 14)));
        assert_eq!(top(IAE, ReflectiveCheck::NullToPrimitive('I')), Some((3, 70, 23)));
        // Not the check's throwable; no receiver on a constructor; the cause's
        // `ValueConversions` is not loaded here.
        assert_eq!(top("java/lang/IllegalStateException", ReflectiveCheck::ArgumentCount), None);
        assert_eq!(top(IAE, ReflectiveCheck::Receiver), None);
        assert_eq!(
            top("java/lang/NullPointerException", ReflectiveCheck::NullToPrimitive('I')),
            None
        );
    }

    /// Wave 46, lane L3: nested `new`s of one class pair with their own
    /// constructor calls (the inner call completes first).
    #[test]
    fn nested_throwable_sites_pair_with_their_own_constructor_calls() {
        const IAE: &str = "java/lang/IllegalArgumentException";
        let cp = pool(&[(IAE, "<init>", "(Ljava/lang/Throwable;)V")]);
        // new #2; dup; new #2; dup; aconst_null; invokespecial #6;
        // invokespecial #6; athrow
        let code = [0xbb, 0, 2, 0x59, 0xbb, 0, 2, 0x59, 0x01, 0xb7, 0, 6, 0xb7, 0, 6, 0xbf];
        assert_eq!(throwable_init_sites(&cp, &code, IAE), Some(vec![12, 9]));
        assert_eq!(throwable_init_sites(&cp, &code, "java/lang/Other"), Some(vec![]));
    }

    /// Round 14 wave 2 (lane trace): a `Thread` whose `sleep(long)` is `sleep`
    /// (bytecode with `IllegalArgumentException` sites at the pcs given, line
    /// 537 from pc 0) or native (`None`), and a `probe/Main.main()V` calling it
    /// at bci 1.
    fn arg_check_fixture(iae_sites: Option<&[usize]>) -> Fixture {
        let mut store = ClassStore::new();
        let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), Vec::new());
        let sleep = match iae_sites {
            None => {
                let mut m = native("sleep", "(J)V");
                m.access_flags |= MethodAccessFlags::STATIC;
                m
            }
            Some(sites) => {
                // `new #2; dup; invokespecial #6; athrow` per site, padded
                // with `nop` up to each site's `invokespecial` pc.
                let mut code = Vec::new();
                for &site in sites {
                    while code.len() + 4 < site {
                        code.push(0x00);
                    }
                    code.extend_from_slice(&[0xbb, 0, 2, 0x59, 0xb7, 0, 6, 0xbf]);
                }
                code.push(0xb1);
                java("sleep", "(J)V", code, &[(0, 537)])
            }
        };
        let thread = add_class(
            &mut store,
            "java/lang/Thread",
            Some(object),
            pool(&[(STANDIN_ARG_CHECK_THROWABLE, "<init>", "(Ljava/lang/String;)V")]),
            vec![sleep],
        );
        let main = add_class(
            &mut store,
            "probe/Main",
            Some(object),
            pool(&[("java/lang/Thread", "sleep", "(J)V")]),
            vec![java("main", "()V", vec![0x09, 0xb8, 0, 6, 0xb1], &[(0, 3)])],
        );
        Fixture {
            store,
            main,
            thread,
        }
    }

    /// HotSpot's `Thread.sleep(-1)`: ONE frame, `sleep` at its
    /// `IllegalArgumentException` construction; no leaf frames.
    #[test]
    fn an_argument_check_gets_the_one_frame_that_throws() {
        let fx = arg_check_fixture(Some(&[4]));
        let site = at(fx.main, "main", "()V", 1);
        assert_eq!(
            expand(&fx, &site, STANDIN_ARG_CHECK_THROWABLE),
            Some(vec![row("java/lang/Thread.sleep", 537, 4)])
        );
        // The leaf census is untouched by it.
        assert_eq!(expand(&fx, &site, INTERRUPTED), None);
        assert!(is_standin_throwable(STANDIN_ARG_CHECK_THROWABLE));
        let _ = fx.thread;
    }

    /// Two construction sites are ambiguous (no frame); a `native` `sleep`
    /// (JDK 17) is the frame itself, at `(Native Method)`.
    #[test]
    fn an_ambiguous_or_native_argument_check() {
        let fx = arg_check_fixture(Some(&[4, 12]));
        assert_eq!(
            expand(&fx, &at(fx.main, "main", "()V", 1), STANDIN_ARG_CHECK_THROWABLE),
            None
        );
        let fx = arg_check_fixture(None);
        assert_eq!(
            expand(&fx, &at(fx.main, "main", "()V", 1), STANDIN_ARG_CHECK_THROWABLE),
            Some(vec![row("java/lang/Thread.sleep", LINE_NUMBER_NATIVE, -1)])
        );
    }
}

#[cfg(test)]
mod r14w3_trace_thread_run_tests {
    use super::r13_trace3_standin_tests::{add_class, java, native, pool};
    use super::*;

    struct Fixture {
        store: ClassStore,
        thread: ClassId,
        worker: ClassId,
        own_run: ClassId,
        task: ClassId,
        sub_task: ClassId,
        other: ClassId,
    }

    /// JDK 25's `Thread.run()`: `scopedValueBindings()` at pc 0 (line 1472),
    /// `runWith(bindings, task)` at pc 7 (line 1474).
    fn jdk25_run() -> ClassFileMethod {
        java(
            "run",
            "()V",
            vec![0xb8, 0, 12, 0x4c, 0x2a, 0x2b, 0x2b, 0xb7, 0, 6, 0xb1],
            &[(0, 1472), (4, 1474)],
        )
    }

    fn fixture(run: ClassFileMethod) -> Fixture {
        let mut store = ClassStore::new();
        let thread = add_class(
            &mut store,
            "java/lang/Thread",
            None,
            pool(&[
                (
                    "java/lang/Thread",
                    "runWith",
                    "(Ljava/lang/Object;Ljava/lang/Runnable;)V",
                ),
                ("java/lang/Thread", "scopedValueBindings", "()Ljava/lang/Object;"),
            ]),
            vec![run],
        );
        let body = || java("run", "()V", vec![0xb1], &[(0, 10)]);
        let worker = add_class(&mut store, "probe/Worker", Some(thread), pool(&[]), vec![]);
        let own_run = add_class(&mut store, "probe/OwnRun", Some(thread), pool(&[]), vec![body()]);
        let task = add_class(&mut store, "probe/Task", None, pool(&[]), vec![body()]);
        let sub_task = add_class(&mut store, "probe/SubTask", Some(task), pool(&[]), vec![]);
        let other = add_class(&mut store, "probe/Other", None, pool(&[]), vec![body()]);
        Fixture {
            store,
            thread,
            worker,
            own_run,
            task,
            sub_task,
            other,
        }
    }

    fn frame(class: &str, id: ClassId, method: &str) -> BacktraceFrame {
        BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from(class),
            method_name: Arc::from(method),
            method_descriptor: Some(Arc::from("()V")),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: 0,
            class_id: Some(id),
            method_index: None,
        })
    }

    fn answer(
        fx: &Fixture,
        outermost: &BacktraceFrame,
        thread: ClassId,
        task: ClassId,
    ) -> Option<(String, String, i32, i32, Option<ClassId>)> {
        thread_run_standin_frame(&fx.store, outermost, thread, task).map(|f| {
            let e = f.to_entry();
            (
                e.class_name.to_string(),
                e.method_name.to_string(),
                e.line_number,
                e.byte_code_index,
                e.class_id,
            )
        })
    }

    #[test]
    fn a_task_run_by_the_served_thread_run_gets_the_thread_run_frame() {
        let fx = fixture(jdk25_run());
        let want = Some((
            "java/lang/Thread".to_string(),
            "run".to_string(),
            1474,
            7,
            Some(fx.thread),
        ));
        let task_run = frame("probe/Task", fx.task, "run");
        assert!(may_be_thread_run_task_frame(&task_run));
        assert_eq!(answer(&fx, &task_run, fx.thread, fx.task), want);
        // A Thread subclass that inherits `run`, and a task inheriting its `run`.
        assert_eq!(answer(&fx, &task_run, fx.worker, fx.sub_task), want);
        // A compiled outermost activation names its class in its label.
        let compiled = BacktraceFrame::Compiled {
            label: Arc::from("probe/Task.run:()V"),
            owner_class_id: fx.task.as_u32(),
            bci: 0,
            cp_stamp: None,
        };
        assert_eq!(answer(&fx, &compiled, fx.thread, fx.task), want);
    }

    #[test]
    fn no_thread_run_frame_where_hotspot_has_none() {
        let fx = fixture(jdk25_run());
        let task_run = frame("probe/Task", fx.task, "run");
        // The thread overrides `run`: the override is the bottom frame.
        assert_eq!(answer(&fx, &task_run, fx.own_run, fx.task), None);
        // The task's `run` is not the outermost frame's.
        assert_eq!(answer(&fx, &task_run, fx.thread, fx.other), None);
        // `Thread.run` itself ran as bytecode, or the frame is not `run()V`.
        let thread_run = frame("java/lang/Thread", fx.thread, "run");
        assert!(!may_be_thread_run_task_frame(&thread_run));
        assert_eq!(answer(&fx, &thread_run, fx.thread, fx.task), None);
        let call = frame("probe/Task", fx.task, "call");
        assert!(!may_be_thread_run_task_frame(&call));
        assert_eq!(answer(&fx, &call, fx.thread, fx.task), None);
    }

    #[test]
    fn an_ambiguous_or_native_thread_run_gets_no_frame() {
        let two_calls = java(
            "run",
            "()V",
            vec![0x2a, 0x01, 0x01, 0xb7, 0, 6, 0x2a, 0x01, 0x01, 0xb7, 0, 6, 0xb1],
            &[(0, 1474)],
        );
        for run in [two_calls, native("run", "()V")] {
            let fx = fixture(run);
            let task_run = frame("probe/Task", fx.task, "run");
            assert_eq!(answer(&fx, &task_run, fx.thread, fx.task), None);
        }
    }
}

#[cfg(test)]
mod r14w3_trace2_thread_run_middle_tests {
    use super::r13_trace3_standin_tests::{add_class, java, pool};
    use super::*;

    struct Fixture {
        store: ClassStore,
        thread: ClassId,
        base: ClassId,
        own_run: ClassId,
        caller: ClassId,
        task: ClassId,
    }

    /// `Thread` with JDK 25's `run()` (`runWith` at pc 7, line 1474); `Base
    /// extends Thread` without `run`; `OwnRun extends Thread` with its own;
    /// `Task`; and a `Caller` whose methods call, at bci 1 (0 for the
    /// static): `invokespecial Base.run` (javac's `super.run()` in a
    /// subclass of `Base`), `invokevirtual Thread.run`, `invokespecial
    /// OwnRun.run`, `invokestatic Thread.run`.
    fn fixture() -> Fixture {
        let mut store = ClassStore::new();
        let thread = add_class(
            &mut store,
            "java/lang/Thread",
            None,
            pool(&[
                (
                    "java/lang/Thread",
                    "runWith",
                    "(Ljava/lang/Object;Ljava/lang/Runnable;)V",
                ),
                ("java/lang/Thread", "scopedValueBindings", "()Ljava/lang/Object;"),
            ]),
            vec![java(
                "run",
                "()V",
                vec![0xb8, 0, 12, 0x4c, 0x2a, 0x2b, 0x2b, 0xb7, 0, 6, 0xb1],
                &[(0, 1472), (4, 1474)],
            )],
        );
        let body = || java("run", "()V", vec![0xb1], &[(0, 10)]);
        let base = add_class(&mut store, "probe/Base", Some(thread), pool(&[]), vec![]);
        let own_run = add_class(&mut store, "probe/OwnRun", Some(thread), pool(&[]), vec![body()]);
        let task = add_class(&mut store, "probe/Task", None, pool(&[]), vec![body()]);
        let caller = add_class(
            &mut store,
            "probe/Caller",
            None,
            pool(&[
                ("probe/Base", "run", "()V"),
                ("java/lang/Thread", "run", "()V"),
                ("probe/OwnRun", "run", "()V"),
                ("java/lang/Thread", "run", "()V"),
            ]),
            vec![
                java("viaSuper", "()V", vec![0x2a, 0xb7, 0, 6, 0xb1], &[(0, 20)]),
                java("viaVirtual", "()V", vec![0x2a, 0xb6, 0, 12, 0xb1], &[(0, 30)]),
                java("viaOwn", "()V", vec![0x2a, 0xb7, 0, 18, 0xb1], &[(0, 40)]),
                java("viaStatic", "()V", vec![0xb8, 0, 24, 0xb1], &[(0, 50)]),
            ],
        );
        Fixture {
            store,
            thread,
            base,
            own_run,
            caller,
            task,
        }
    }

    fn at(class: &str, id: ClassId, method: &str, bci: i32) -> BacktraceFrame {
        BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from(class),
            method_name: Arc::from(method),
            method_descriptor: Some(Arc::from("()V")),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(id),
            method_index: None,
        })
    }

    fn answer(
        fx: &Fixture,
        caller: &BacktraceFrame,
        callee: &BacktraceFrame,
    ) -> Option<(String, String, i32, i32, Option<ClassId>)> {
        let names = [
            ("java/lang/Thread", fx.thread),
            ("probe/Base", fx.base),
            ("probe/OwnRun", fx.own_run),
        ];
        let mut resolve = |_: ClassId, name: &str| {
            names.iter().find(|(n, _)| *n == name).map(|(_, id)| *id)
        };
        assert!(may_need_thread_run_middle_frame(caller, callee));
        thread_run_middle_frame(&fx.store, caller, callee, &mut resolve).map(|e| {
            (
                e.class_name.to_string(),
                e.method_name.to_string(),
                e.line_number,
                e.byte_code_index,
                e.class_id,
            )
        })
    }

    #[test]
    fn a_call_that_entered_the_served_thread_run_gets_the_middle_frame() {
        let fx = fixture();
        let want = Some((
            "java/lang/Thread".to_string(),
            "run".to_string(),
            1474,
            7,
            Some(fx.thread),
        ));
        let task_run = at("probe/Task", fx.task, "run", 0);
        // `super.run()` through a class that inherits `Thread.run`.
        let via_super = at("probe/Caller", fx.caller, "viaSuper", 1);
        assert_eq!(answer(&fx, &via_super, &task_run), want);
        // A direct `thread.run()` whose callee is a task, not an override.
        let via_virtual = at("probe/Caller", fx.caller, "viaVirtual", 1);
        assert_eq!(answer(&fx, &via_virtual, &task_run), want);
        // `invokespecial` is exact even when the task is itself a `Thread`.
        let own_run = at("probe/OwnRun", fx.own_run, "run", 0);
        assert_eq!(answer(&fx, &via_super, &own_run), want);
    }

    #[test]
    fn a_lambda_tasks_implementation_method_gets_the_bottom_frame() {
        let fx = fixture();
        let lambda_body = || {
            BacktraceFrame::Entry(StackTraceEntry {
                class_name: Arc::from("probe/Caller"),
                method_name: Arc::from("lambda$main$0"),
                method_descriptor: Some(Arc::from("()V")),
                source_file: None,
                line_number: LINE_NUMBER_UNKNOWN,
                byte_code_index: 0,
                class_id: Some(fx.caller),
                method_index: None,
            })
        };
        let run = ("run", "()V");
        let body = ("probe/Caller", "lambda$main$0", "()V");
        let frame = lambda_body();
        assert!(may_be_thread_run_lambda_frame(&frame));
        let got = thread_run_standin_frame_for_lambda(&fx.store, &frame, fx.base, run, body)
            .map(|f| f.to_entry());
        let got = got.map(|e| (e.class_name.to_string(), e.line_number, e.class_id));
        assert_eq!(got, Some(("java/lang/Thread".to_string(), 1474, Some(fx.thread))));
        // Not a `Runnable`, another method, or a thread overriding `run`.
        let answer = |sam, body, thread| {
            thread_run_standin_frame_for_lambda(&fx.store, &frame, thread, sam, body)
        };
        assert!(answer(("call", "()Ljava/lang/Object;"), body, fx.base).is_none());
        assert!(answer(run, ("probe/Caller", "lambda$main$1", "()V"), fx.base).is_none());
        assert!(answer(run, ("probe/Other", "lambda$main$0", "()V"), fx.base).is_none());
        assert!(answer(run, body, fx.own_run).is_none());
        // The main thread's outermost frame is never screened in.
        let main = at("probe/Caller", fx.caller, "main", 0);
        assert!(!may_be_thread_run_lambda_frame(&main));
    }

    #[test]
    fn no_middle_frame_where_hotspot_has_none() {
        let fx = fixture();
        let task_run = at("probe/Task", fx.task, "run", 0);
        // A virtual call whose callee extends `Thread` dispatched to the
        // receiver's override, not to `Thread.run`.
        let via_virtual = at("probe/Caller", fx.caller, "viaVirtual", 1);
        let own_run = at("probe/OwnRun", fx.own_run, "run", 0);
        assert_eq!(answer(&fx, &via_virtual, &own_run), None);
        // The call resolves to an override, or is not a virtual/special call.
        let via_own = at("probe/Caller", fx.caller, "viaOwn", 1);
        assert_eq!(answer(&fx, &via_own, &task_run), None);
        let via_static = at("probe/Caller", fx.caller, "viaStatic", 0);
        assert_eq!(answer(&fx, &via_static, &task_run), None);
        // The caller does not stand at the call.
        let off_site = at("probe/Caller", fx.caller, "viaSuper", 0);
        assert_eq!(answer(&fx, &off_site, &task_run), None);
        // `Thread.run` ran as bytecode: its own frame is the callee.
        let thread_run = at("java/lang/Thread", fx.thread, "run", 7);
        let via_super = at("probe/Caller", fx.caller, "viaSuper", 1);
        assert!(!may_need_thread_run_middle_frame(&via_super, &thread_run));
    }
}

/// Round 14 wave 4 (lane trace3): the `join` rows and owner resolution
/// (T4-2), the parked-thread leaf frames (T4-3), and the tightened lock-free
/// `Thread.run` screens.
#[cfg(test)]
mod r14w4_trace3_tests {
    use super::r13_trace3_standin_tests::{add_class, java, native, pool};
    use super::*;

    const INTERRUPTED: &str = "java/lang/InterruptedException";

    struct Fixture {
        store: ClassStore,
        main: ClassId,
    }

    /// JDK-25-shaped `Object.wait(long)` (its platform `wait0` at pc 7, line
    /// 389) and `Thread.join()` / `join(long)`: `join()` is `join(0L)` (pc 2,
    /// line 1901); `join(long)` calls `java/lang/Thread.wait(J)V` -- javac's
    /// qualification -- at pc 2 (the timed loop, line 1881) and pc 7 (the
    /// `wait(0)` loop, line 1887). `probe/Main.main()V` calls `join()` at bci
    /// 1 and `join(long)` at bci 6.
    fn fixture() -> Fixture {
        let mut store = ClassStore::new();
        let object = add_class(
            &mut store,
            "java/lang/Object",
            None,
            pool(&[("java/lang/Object", "wait0", "(J)V")]),
            vec![
                java(
                    "wait",
                    "(J)V",
                    vec![0x2a, 0x1f, 0xb7, 0, 6, 0x2a, 0x1f, 0xb7, 0, 6, 0xb1],
                    &[(0, 383), (5, 389)],
                ),
                native("wait0", "(J)V"),
            ],
        );
        add_class(
            &mut store,
            "java/lang/Thread",
            Some(object),
            pool(&[
                ("java/lang/Thread", "join", "(J)V"),
                ("java/lang/Thread", "wait", "(J)V"),
            ]),
            vec![
                java("join", "()V", vec![0x2a, 0x09, 0xb6, 0, 6, 0xb1], &[(0, 1901)]),
                java(
                    "join",
                    "(J)V",
                    vec![0x2a, 0x1f, 0xb6, 0, 12, 0x2a, 0x09, 0xb6, 0, 12, 0xb1],
                    &[(0, 1881), (5, 1887)],
                ),
            ],
        );
        let main = add_class(
            &mut store,
            "probe/Main",
            Some(object),
            pool(&[
                ("java/lang/Thread", "join", "()V"),
                ("java/lang/Thread", "join", "(J)V"),
            ]),
            vec![java(
                "main",
                "()V",
                vec![0x2a, 0xb6, 0, 6, 0x2a, 0x09, 0xb6, 0, 12, 0xb1],
                &[(0, 3), (4, 4)],
            )],
        );
        Fixture { store, main }
    }

    fn entry(
        class_id: ClassId,
        name: &str,
        descriptor: Option<&str>,
        bci: i32,
    ) -> StackTraceEntry {
        StackTraceEntry {
            class_name: Arc::from("probe/Main"),
            method_name: Arc::from(name),
            method_descriptor: descriptor.map(Arc::from),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(class_id),
            method_index: None,
        }
    }

    fn rows(entries: &[StackTraceEntry]) -> Vec<(String, i32, i32)> {
        entries
            .iter()
            .map(|e| {
                (
                    format!("{}.{}", e.class_name, e.method_name),
                    e.line_number,
                    e.byte_code_index,
                )
            })
            .collect()
    }

    fn joined() -> Vec<(String, i32, i32)> {
        vec![
            ("java/lang/Thread.join".to_string(), 1901, 2),
            ("java/lang/Thread.join".to_string(), 1887, 7),
            ("java/lang/Object.wait".to_string(), 389, 7),
            ("java/lang/Object.wait0".to_string(), LINE_NUMBER_NATIVE, -1),
        ]
    }

    /// Item 1 / T4-2: an interrupted registered `join()` gets HotSpot's four
    /// JDK frames, through the `Thread`-qualified `wait(J)V` call; a direct
    /// timed `join(long)` is not an entry and gets none.
    #[test]
    fn a_registered_join_gets_the_wait_chain_through_owner_resolution() {
        let fx = fixture();
        let store = &fx.store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        let untimed = BacktraceFrame::Entry(entry(fx.main, "main", Some("()V"), 1));
        let got = native_standin_frames(store, &untimed, INTERRUPTED, &mut resolve).map(|v| {
            let entries: Vec<StackTraceEntry> = v.iter().map(BacktraceFrame::to_entry).collect();
            rows(&entries)
        });
        assert_eq!(got, Some(joined()));
        let timed = BacktraceFrame::Entry(entry(fx.main, "main", Some("()V"), 6));
        assert!(native_standin_frames(store, &timed, INTERRUPTED, &mut resolve).is_none());
        // `join` raises no `IllegalMonitorStateException` from inside.
        let monitor = "java/lang/IllegalMonitorStateException";
        assert!(native_standin_frames(store, &untimed, monitor, &mut resolve).is_none());
    }

    /// T4-3: a descriptor-less published entry of a thread parked in the
    /// registered `join()` gets the same frames; a timed join, or an entry
    /// not standing at a call, gets none.
    #[test]
    fn a_parked_threads_published_stack_gets_the_leaf_frames() {
        let fx = fixture();
        let store = &fx.store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        let got =
            parked_thread_standin_frames(store, &entry(fx.main, "main", None, 1), &mut resolve);
        assert_eq!(got.map(|v| rows(&v)), Some(joined()));
        for bci in [6, 0, -1] {
            let parked = entry(fx.main, "main", None, bci);
            let got = parked_thread_standin_frames(store, &parked, &mut resolve);
            assert!(got.is_none(), "bci {bci}");
        }
        // A descriptor that names no method of that name answers nothing.
        let wrong = entry(fx.main, "main", Some("(I)V"), 1);
        assert!(parked_thread_standin_frames(store, &wrong, &mut resolve).is_none());
    }

    /// A parked joiner's published stack (`main` -> `join()` -> `join(long)`,
    /// no descriptors, `join` overloaded) plus the appended `wait` leaf: each
    /// overloaded entry gets the line of the call it stands at.
    #[test]
    fn published_overloads_get_their_lines_from_the_callee() {
        let fx = fixture();
        let id = |name: &str| fx.store.iter().find(|c| &*c.name == name).map(|c| c.id);
        let (thread, object) = (id("java/lang/Thread").unwrap(), id("java/lang/Object").unwrap());
        let mut trace = vec![
            entry(fx.main, "main", None, 1),
            entry(thread, "join", None, 2),
            entry(thread, "join", None, 7),
            entry(object, "wait", Some("(J)V"), 7),
        ];
        let resolved = resolve_published_overloads_by_callee(&fx.store, &mut trace);
        assert_eq!(resolved, 3);
        let lines: Vec<i32> = trace.iter().map(|e| e.line_number).collect();
        assert_eq!(lines, vec![3, 1901, 1887, LINE_NUMBER_UNKNOWN]);
        assert_eq!(trace[1].method_descriptor.as_deref(), Some("()V"));
        assert_eq!(trace[2].method_descriptor.as_deref(), Some("(J)V"));
        // A callee the entry does not call at its bci resolves nothing.
        let mut wrong = vec![entry(thread, "join", None, 2), entry(object, "sleep", None, 0)];
        assert_eq!(resolve_published_overloads_by_callee(&fx.store, &mut wrong), 0);
        assert_eq!(wrong[0].line_number, LINE_NUMBER_UNKNOWN);
    }

    fn frame(class_id: ClassId, name: &str, descriptor: &str) -> BacktraceFrame {
        BacktraceFrame::Entry(entry(class_id, name, Some(descriptor), 0))
    }

    /// The lambda screen keeps every implementation-method shape and refuses
    /// the frames a started thread's task cannot have run from.
    #[test]
    fn the_lambda_screen_refuses_what_cannot_be_an_implementation_method() {
        let id = ClassId::new(7);
        for (name, descriptor) in [
            ("lambda$main$0", "()V"),
            ("lambda$main$1", "(ILjava/lang/String;)V"),
            ("main$lambda$0", "(Ljava/lang/Object;)V"),
            ("$anonfun$main$1", "(I)V"),
            ("serve", "()V"),
            ("size", "()I"),
            ("<init>", "()V"),
        ] {
            assert!(may_be_thread_run_lambda_frame(&frame(id, name, descriptor)), "{name}");
        }
        for (name, descriptor) in [
            ("main", "([Ljava/lang/String;)V"),
            ("<clinit>", "()V"),
            ("loadClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
            ("checkAndLoadMain", "(ZILjava/lang/String;)Ljava/lang/Class;"),
        ] {
            assert!(!may_be_thread_run_lambda_frame(&frame(id, name, descriptor)), "{name}");
        }
    }

    /// A `run()V` of the thread's own class is refused without the lock; a
    /// task's `run`, or another method of the thread's class, is not.
    #[test]
    fn the_threads_own_run_is_refused_before_the_lock() {
        let own = ClassId::new(7);
        let task = ClassId::new(8);
        assert!(outermost_is_threads_own_run(&frame(own, "run", "()V"), own));
        assert!(!outermost_is_threads_own_run(&frame(task, "run", "()V"), own));
        assert!(!outermost_is_threads_own_run(&frame(own, "lambda$new$0", "()V"), own));
        assert!(!outermost_is_threads_own_run(&frame(own, "run", "(I)V"), own));
    }

    #[test]
    fn virtual_thread_classes_are_recognised_by_their_chain() {
        let mut store = ClassStore::new();
        let object = add_class(&mut store, "java/lang/Object", None, pool(&[]), vec![]);
        let thread = add_class(&mut store, "java/lang/Thread", Some(object), pool(&[]), vec![]);
        let base = add_class(
            &mut store,
            "java/lang/BaseVirtualThread",
            Some(thread),
            pool(&[]),
            vec![],
        );
        let vt = add_class(&mut store, "java/lang/VirtualThread", Some(base), pool(&[]), vec![]);
        let bound = add_class(
            &mut store,
            "java/lang/ThreadBuilders$BoundVirtualThread",
            Some(base),
            pool(&[]),
            vec![],
        );
        assert!(is_virtual_thread_class(&store, vt));
        assert!(is_virtual_thread_class(&store, bound));
        assert!(!is_virtual_thread_class(&store, thread));
        assert!(!is_virtual_thread_class(&store, object));
    }
}

#[cfg(test)]
mod r14w5_trace4_tests {
    use super::r13_trace3_standin_tests::{add_class, java, native, pool};
    use super::*;
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    const INTERRUPTED: &str = "java/lang/InterruptedException";

    struct Fixture {
        store: ClassStore,
        main: ClassId,
    }

    fn with_flags(mut method: ClassFileMethod, flags: MethodAccessFlags) -> ClassFileMethod {
        method.access_flags = flags;
        method
    }

    /// JDK-25-shaped `Object` (`wait()` -> `wait(long)`, whose `wait0` calls
    /// sit at pc 2 -- the virtual-thread branch, line 383 -- and pc 7, line
    /// 389; native `clone`, final native `notify`, non-final native
    /// `hashCode`), `System.arraycopy`, a signature-polymorphic
    /// `MethodHandle.invokeExact`, the throwable classes the screens name, and
    /// `probe/Main.main()V` calling, at bci:
    /// 0 `System.arraycopy`, 3 `super.clone()`, 6 `this.notify()`,
    /// 9 `this.hashCode()`, 12 `RuntimeException.<init>` then `athrow`,
    /// 16 `MethodHandle.invokeExact`, 19 `helper()` (Java), 22 `this.wait()`.
    fn fixture() -> Fixture {
        let mut store = ClassStore::new();
        let object = add_class(
            &mut store,
            "java/lang/Object",
            None,
            pool(&[
                ("java/lang/Object", "wait0", "(J)V"),
                ("java/lang/Object", "wait", "(J)V"),
            ]),
            vec![
                java("wait", "()V", vec![0x2a, 0x09, 0xb6, 0, 12, 0xb1], &[(0, 351)]),
                java(
                    "wait",
                    "(J)V",
                    vec![0x2a, 0x1f, 0xb7, 0, 6, 0x2a, 0x1f, 0xb7, 0, 6, 0xb1],
                    &[(0, 383), (5, 389)],
                ),
                native("wait0", "(J)V"),
                with_flags(
                    native("clone", "()Ljava/lang/Object;"),
                    MethodAccessFlags::PROTECTED | MethodAccessFlags::NATIVE,
                ),
                with_flags(
                    native("notify", "()V"),
                    MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL | MethodAccessFlags::NATIVE,
                ),
                with_flags(
                    native("hashCode", "()I"),
                    MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                ),
            ],
        );
        add_class(
            &mut store,
            "java/lang/System",
            Some(object),
            pool(&[]),
            vec![with_flags(
                native("arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
                MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            )],
        );
        add_class(
            &mut store,
            "java/lang/invoke/MethodHandle",
            Some(object),
            pool(&[]),
            vec![with_flags(
                native("invokeExact", "([Ljava/lang/Object;)Ljava/lang/Object;"),
                MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL | MethodAccessFlags::NATIVE,
            )],
        );
        let throwable =
            add_class(&mut store, "java/lang/Throwable", Some(object), pool(&[]), vec![]);
        let exception =
            add_class(&mut store, "java/lang/Exception", Some(throwable), pool(&[]), vec![]);
        let runtime = add_class(
            &mut store,
            "java/lang/RuntimeException",
            Some(exception),
            pool(&[]),
            vec![],
        );
        let error = add_class(&mut store, "java/lang/Error", Some(throwable), pool(&[]), vec![]);
        let linkage =
            add_class(&mut store, "java/lang/LinkageError", Some(error), pool(&[]), vec![]);
        let icce = add_class(
            &mut store,
            "java/lang/IncompatibleClassChangeError",
            Some(linkage),
            pool(&[]),
            vec![],
        );
        add_class(
            &mut store,
            "java/lang/reflect/Array",
            Some(object),
            pool(&[(
                "java/lang/reflect/Array",
                "newArray",
                "(Ljava/lang/Class;I)Ljava/lang/Object;",
            )]),
            vec![
                // newInstance(c, n) { return newArray(c, n); } -- the call at pc 2.
                java(
                    "newInstance",
                    "(Ljava/lang/Class;I)Ljava/lang/Object;",
                    vec![0x2a, 0x1b, 0xb8, 0, 6, 0xb0],
                    &[(0, 76)],
                ),
                with_flags(
                    native("newArray", "(Ljava/lang/Class;I)Ljava/lang/Object;"),
                    MethodAccessFlags::PRIVATE | MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
                ),
            ],
        );
        for (name, superclass) in [
            ("java/lang/NegativeArraySizeException", runtime),
            ("java/lang/IllegalArgumentException", runtime),
            ("java/lang/NoSuchMethodError", icce),
            ("java/lang/UnsatisfiedLinkError", linkage),
            ("java/lang/ExceptionInInitializerError", linkage),
            ("java/lang/ClassFormatError", linkage),
            ("java/lang/StackOverflowError", error),
            ("java/lang/NullPointerException", runtime),
            ("java/lang/ArrayIndexOutOfBoundsException", runtime),
            ("java/lang/IllegalMonitorStateException", runtime),
            ("java/lang/CloneNotSupportedException", exception),
            (INTERRUPTED, exception),
        ] {
            add_class(&mut store, name, Some(superclass), pool(&[]), vec![]);
        }
        let main = add_class(
            &mut store,
            "probe/Main",
            Some(object),
            pool(&[
                ("java/lang/System", "arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
                ("java/lang/Object", "clone", "()Ljava/lang/Object;"),
                ("probe/Main", "notify", "()V"),
                ("probe/Main", "hashCode", "()I"),
                ("java/lang/RuntimeException", "<init>", "()V"),
                (
                    "java/lang/invoke/MethodHandle",
                    "invokeExact",
                    "([Ljava/lang/Object;)Ljava/lang/Object;",
                ),
                ("probe/Main", "helper", "()V"),
                ("probe/Main", "wait", "()V"),
                (
                    "java/lang/reflect/Array",
                    "newInstance",
                    "(Ljava/lang/Class;I)Ljava/lang/Object;",
                ),
            ]),
            vec![
                java(
                    "main",
                    "()V",
                    vec![
                        0xb8, 0, 6, // 0: arraycopy
                        0xb7, 0, 12, // 3: super.clone()
                        0xb6, 0, 18, // 6: notify()
                        0xb6, 0, 24, // 9: hashCode()
                        0xb7, 0, 30, 0xbf, // 12: RuntimeException.<init>, athrow
                        0xb6, 0, 36, // 16: invokeExact
                        0xb8, 0, 42, // 19: helper()
                        0xb6, 0, 48, // 22: wait()
                        0xb8, 0, 54, // 25: Array.newInstance
                        0xb1,
                    ],
                    &[
                        (0, 3),
                        (3, 4),
                        (6, 5),
                        (9, 6),
                        (12, 7),
                        (16, 8),
                        (19, 9),
                        (22, 10),
                        (25, 11),
                    ],
                ),
                java("helper", "()V", vec![0xb1], &[(0, 20)]),
            ],
        );
        Fixture { store, main }
    }

    fn at(fx: &Fixture, bci: i32) -> BacktraceFrame {
        BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from("probe/Main"),
            method_name: Arc::from("main"),
            method_descriptor: Some(Arc::from("()V")),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(fx.main),
            method_index: None,
        })
    }

    fn describe(frame: &BacktraceFrame) -> (String, i32, i32) {
        let e = frame.to_entry();
        (format!("{}.{}", e.class_name, e.method_name), e.line_number, e.byte_code_index)
    }

    /// The leaf frame for a throwable of class `throwable` raised with the
    /// innermost frame at `bci`.
    fn leaf(fx: &Fixture, bci: i32, throwable: &str) -> Option<(String, i32, i32)> {
        let store = &fx.store;
        let tclass = store.iter().find(|c| &*c.name == throwable).expect("throwable class");
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        native_leaf_frame(store, &at(fx, bci), tclass, &mut resolve).map(|f| describe(&f))
    }

    fn native_row(frame: &str) -> Option<(String, i32, i32)> {
        Some((frame.to_string(), LINE_NUMBER_NATIVE, -1))
    }

    /// T3-1: a static native's own throwables, a null argument's NPE
    /// included, get its `(Native Method)` frame.
    #[test]
    fn a_static_native_gets_its_frame_for_what_it_raises() {
        let fx = fixture();
        for throwable in [
            "java/lang/ArrayIndexOutOfBoundsException",
            "java/lang/NullPointerException",
            "java/lang/UnsatisfiedLinkError",
            "java/lang/ClassFormatError",
        ] {
            assert_eq!(
                leaf(&fx, 0, throwable),
                native_row("java/lang/System.arraycopy"),
                "{throwable}"
            );
        }
    }

    /// What the invoke raises BEFORE entering keeps the caller on top.
    #[test]
    fn what_the_call_raises_before_entering_gets_no_frame() {
        let fx = fixture();
        for throwable in [
            "java/lang/NoSuchMethodError",
            "java/lang/ExceptionInInitializerError",
            "java/lang/StackOverflowError",
        ] {
            assert_eq!(leaf(&fx, 0, throwable), None, "{throwable}");
        }
        // A null receiver: every non-static call.
        assert_eq!(leaf(&fx, 6, "java/lang/NullPointerException"), None);
        assert_eq!(leaf(&fx, 3, "java/lang/NullPointerException"), None);
    }

    /// `super.clone()` and a final virtual native enter the native; a
    /// non-final virtual native may have dispatched elsewhere.
    #[test]
    fn only_calls_that_enter_the_native_get_its_frame() {
        let fx = fixture();
        assert_eq!(
            leaf(&fx, 3, "java/lang/CloneNotSupportedException"),
            native_row("java/lang/Object.clone")
        );
        assert_eq!(
            leaf(&fx, 6, "java/lang/IllegalMonitorStateException"),
            native_row("java/lang/Object.notify")
        );
        assert_eq!(leaf(&fx, 9, "java/lang/IllegalMonitorStateException"), None);
    }

    /// Constructors, signature-polymorphic natives, Java methods and
    /// non-calls get nothing.
    #[test]
    fn no_frame_for_what_is_not_a_native_call() {
        let fx = fixture();
        let any = "java/lang/IllegalMonitorStateException";
        for bci in [12, 15, 16, 19, -1, 99] {
            assert_eq!(leaf(&fx, bci, any), None, "bci {bci}");
        }
    }

    /// The lock-free screen: a call, except an `invokespecial` followed by
    /// `athrow` (`throw new X()` standing at `X.<init>`).
    #[test]
    fn the_lock_free_screen_skips_throw_new() {
        let code = [0xb8, 0, 6, 0xb7, 0, 12, 0xb6, 0, 18, 0xb7, 0, 30, 0xbf, 0x00];
        assert!(code_may_call_native_at(&code, 0));
        assert!(code_may_call_native_at(&code, 3));
        assert!(code_may_call_native_at(&code, 6));
        assert!(!code_may_call_native_at(&code, 9));
        assert!(!code_may_call_native_at(&code, 12));
        assert!(!code_may_call_native_at(&code, 13));
        assert!(!code_may_call_native_at(&code, 40));
    }

    /// Item 5: on a virtual thread an interrupted registered `wait()` leaves
    /// `wait(long)` from its FIRST `wait0` call (the virtual-thread branch,
    /// line 383); a platform thread from its last (389). Nothing chains from
    /// a call that is not an `Object.wait` row.
    #[test]
    fn a_virtual_thread_waits_from_the_virtual_branch() {
        let fx = fixture();
        let store = &fx.store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        let rows = |frames: Option<Vec<BacktraceFrame>>| {
            frames.map(|v| v.iter().map(describe).collect::<Vec<_>>())
        };
        let row = |frame: &str, line: i32, bci: i32| (frame.to_string(), line, bci);
        let site = at(&fx, 22);
        assert_eq!(
            rows(virtual_thread_standin_frames(store, &site, INTERRUPTED, &mut resolve)),
            Some(vec![
                row("java/lang/Object.wait", 351, 2),
                row("java/lang/Object.wait", 383, 2),
                row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
            ])
        );
        assert_eq!(
            rows(native_standin_frames(store, &site, INTERRUPTED, &mut resolve)),
            Some(vec![
                row("java/lang/Object.wait", 351, 2),
                row("java/lang/Object.wait", 389, 7),
                row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
            ])
        );
        let arraycopy = at(&fx, 0);
        assert!(virtual_thread_standin_frames(store, &arraycopy, INTERRUPTED, &mut resolve).is_none());
    }

    /// A registered `Array.newInstance(Class, int)` that throws gets
    /// HotSpot's `newArray` leaf and `newInstance` frame, on any thread; an
    /// NPE standing at a call that names no entry row gets nothing.
    #[test]
    fn a_served_array_new_instance_gets_its_two_frames() {
        let fx = fixture();
        let store = &fx.store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        let expected = Some(vec![
            ("java/lang/reflect/Array.newInstance".to_string(), 76, 2),
            ("java/lang/reflect/Array.newArray".to_string(), LINE_NUMBER_NATIVE, -1),
        ]);
        let site = at(&fx, 25);
        for throwable in [
            "java/lang/NegativeArraySizeException",
            "java/lang/IllegalArgumentException",
            "java/lang/NullPointerException",
        ] {
            let got = native_standin_frames(store, &site, throwable, &mut resolve)
                .map(|v| v.iter().map(describe).collect::<Vec<_>>());
            assert_eq!(got, expected, "{throwable}");
            let got = virtual_thread_standin_frames(store, &site, throwable, &mut resolve)
                .map(|v| v.iter().map(describe).collect::<Vec<_>>());
            assert_eq!(got, expected, "virtual {throwable}");
        }
        assert!(native_standin_frames(store, &site, INTERRUPTED, &mut resolve).is_none());
        let npe = "java/lang/NullPointerException";
        assert!(native_standin_frames(store, &at(&fx, 6), npe, &mut resolve).is_none());
        // `newInstance` itself is Java: the one-frame rule has nothing to add.
        assert_eq!(leaf(&fx, 25, "java/lang/NegativeArraySizeException"), None);
    }
}

#[cfg(test)]
mod r14w6_trace5_tests {
    use super::r13_trace3_standin_tests::{add_class, java, native, pool};
    use super::*;

    const INTERRUPTED: &str = "java/lang/InterruptedException";

    fn entry(
        class_id: ClassId,
        class: &str,
        name: &str,
        descriptor: &str,
        bci: i32,
    ) -> StackTraceEntry {
        StackTraceEntry {
            class_name: Arc::from(class),
            method_name: Arc::from(name),
            method_descriptor: Some(Arc::from(descriptor)),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(class_id),
            method_index: None,
        }
    }

    /// `Thread.join()` is `join(0L)` (pc 2, line 1901). JDK-25-shaped
    /// (`jdk17 == false`): `join(long)` calls `java/lang/Thread.wait(J)V` in
    /// its timed loop first (pc 2, `lload_1`, line 1881), then in its
    /// `wait(0)` loop (pc 7, `lconst_0`, line 1887); Java `Object.wait(long)`
    /// ends in `wait0` (last call pc 7, line 389). JDK-17-shaped: the `wait(0)`
    /// loop first (pc 2, line 1301), the timed `wait(delay)` second (pc 7,
    /// line 1308), `Object.wait(long)` native. `probe/Main.main()V` calls
    /// `join()` at bci 1 and `join(long)` at bci 6.
    fn fixture(jdk17: bool) -> (ClassStore, ClassId) {
        let mut store = ClassStore::new();
        let object = if jdk17 {
            add_class(
                &mut store,
                "java/lang/Object",
                None,
                pool(&[]),
                vec![native("wait", "(J)V")],
            )
        } else {
            add_class(
                &mut store,
                "java/lang/Object",
                None,
                pool(&[("java/lang/Object", "wait0", "(J)V")]),
                vec![
                    java(
                        "wait",
                        "(J)V",
                        vec![0x2a, 0x1f, 0xb7, 0, 6, 0x2a, 0x1f, 0xb7, 0, 6, 0xb1],
                        &[(0, 383), (5, 389)],
                    ),
                    native("wait0", "(J)V"),
                ],
            )
        };
        let join_long = if jdk17 {
            java(
                "join",
                "(J)V",
                vec![0x2a, 0x09, 0xb6, 0, 12, 0x2a, 0x1f, 0xb6, 0, 12, 0xb1],
                &[(0, 1301), (5, 1308)],
            )
        } else {
            java(
                "join",
                "(J)V",
                vec![0x2a, 0x1f, 0xb6, 0, 12, 0x2a, 0x09, 0xb6, 0, 12, 0xb1],
                &[(0, 1881), (5, 1887)],
            )
        };
        add_class(
            &mut store,
            "java/lang/Thread",
            Some(object),
            pool(&[
                ("java/lang/Thread", "join", "(J)V"),
                ("java/lang/Thread", "wait", "(J)V"),
            ]),
            vec![
                java("join", "()V", vec![0x2a, 0x09, 0xb6, 0, 6, 0xb1], &[(0, 1901)]),
                join_long,
                // `join(long, int)`: `join(millis)` at pc 2, line 1950.
                java("join", "(JI)V", vec![0x2a, 0x1f, 0xb6, 0, 6, 0xb1], &[(0, 1950)]),
            ],
        );
        let main = add_class(
            &mut store,
            "probe/Main",
            Some(object),
            pool(&[
                ("java/lang/Thread", "join", "()V"),
                ("java/lang/Thread", "join", "(J)V"),
                ("java/lang/Thread", "join", "(JI)V"),
            ]),
            vec![java(
                "main",
                "()V",
                vec![
                    0x2a, 0xb6, 0, 6, 0x2a, 0x09, 0xb6, 0, 12, 0x2a, 0x09, 0x03, 0xb6, 0, 18, 0xb1,
                ],
                &[(0, 3), (4, 4), (9, 5)],
            )],
        );
        (store, main)
    }

    fn rows(frames: Option<Vec<BacktraceFrame>>) -> Option<Vec<(String, i32, i32)>> {
        frames.map(|frames| {
            frames
                .iter()
                .map(|f| {
                    let e = f.to_entry();
                    let method = format!("{}.{}", e.class_name, e.method_name);
                    (method, e.line_number, e.byte_code_index)
                })
                .collect()
        })
    }

    fn row(method: &str, line: i32, bci: i32) -> (String, i32, i32) {
        (method.to_string(), line, bci)
    }

    /// TR5-3: a served direct `join(long)` interrupted with a positive
    /// timeout leaves from the timed loop's `wait(delay)`, `join(0)` from the
    /// `wait(0)` loop -- on both JDK shapes, whichever loop comes first.
    #[test]
    fn a_timed_join_leaves_from_the_wait_its_timeout_names() {
        let wait25 = || {
            vec![
                row("java/lang/Object.wait", 389, 7),
                row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
            ]
        };
        let wait17 = || vec![row("java/lang/Object.wait", LINE_NUMBER_NATIVE, -1)];
        let join = |line, bci| row("java/lang/Thread.join", line, bci);
        for (jdk17, timed_row, zero_row, wait) in [
            (false, join(1881, 2), join(1887, 7), wait25()),
            (true, join(1308, 7), join(1301, 2), wait17()),
        ] {
            let (store, main) = fixture(jdk17);
            let store = &store;
            let mut resolve =
                |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
            let direct = BacktraceFrame::Entry(entry(main, "probe/Main", "main", "()V", 6));
            let mut expected = vec![timed_row.clone()];
            expected.extend(wait.clone());
            let got = timed_join_standin_frames(store, &direct, true, &mut resolve);
            assert_eq!(rows(got), Some(expected), "jdk17 {jdk17} timed");
            // `join(long, int)` hands the rounded millis to `join(long)`.
            let nanos = BacktraceFrame::Entry(entry(main, "probe/Main", "main", "()V", 12));
            let mut expected = vec![join(1950, 2), timed_row];
            expected.extend(wait.clone());
            let got = timed_join_standin_frames(store, &nanos, true, &mut resolve);
            assert_eq!(rows(got), Some(expected), "jdk17 {jdk17} join(long, int)");
            let mut expected = vec![zero_row];
            expected.extend(wait);
            let got = timed_join_standin_frames(store, &direct, false, &mut resolve);
            assert_eq!(rows(got), Some(expected), "jdk17 {jdk17} join(0)");
            // The untimed `join()` call is the census's, not the hint's.
            let untimed = BacktraceFrame::Entry(entry(main, "probe/Main", "main", "()V", 1));
            assert!(timed_join_standin_frames(store, &untimed, true, &mut resolve).is_none());
            // Without the hint, a direct `join(long)` still gets nothing.
            assert!(native_standin_frames(store, &direct, INTERRUPTED, &mut resolve).is_none());
        }
    }

    /// The `join()` -> `join(0L)` hop picks the `wait(0)` loop by its
    /// `lconst_0` argument: JDK 25 as before (it is also the last call), and
    /// JDK 17, whose last call is the TIMED `wait(delay)` the old rule picked.
    #[test]
    fn a_registered_join_takes_the_wait_zero_loop_on_both_shapes() {
        for (jdk17, hop, wait) in [
            (
                false,
                row("java/lang/Thread.join", 1887, 7),
                vec![
                    row("java/lang/Object.wait", 389, 7),
                    row("java/lang/Object.wait0", LINE_NUMBER_NATIVE, -1),
                ],
            ),
            (
                true,
                row("java/lang/Thread.join", 1301, 2),
                vec![row("java/lang/Object.wait", LINE_NUMBER_NATIVE, -1)],
            ),
        ] {
            let (store, main) = fixture(jdk17);
            let store = &store;
            let mut resolve =
                |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
            let untimed = BacktraceFrame::Entry(entry(main, "probe/Main", "main", "()V", 1));
            let mut expected = vec![row("java/lang/Thread.join", 1901, 2), hop];
            expected.extend(wait);
            let got = native_standin_frames(store, &untimed, INTERRUPTED, &mut resolve);
            assert_eq!(rows(got), Some(expected), "jdk17 {jdk17}");
        }
    }

    /// TR5-2: the memo answers only for its own key -- the redefinition
    /// count, the thread's class and the outermost frame's class, name and
    /// descriptor -- and remembers a "no frame" answer too.
    #[test]
    fn the_thread_run_memo_answers_only_its_own_key() {
        let (thread, task, other) = (ClassId::new(7), ClassId::new(8), ClassId::new(9));
        let run = BacktraceFrame::Entry(entry(task, "p/Task", "run", "()V", 0));
        let thread_run =
            BacktraceFrame::Entry(entry(ClassId::new(3), "java/lang/Thread", "run", "()V", 4));
        let memo =
            ThreadRunBottomMemo::new(5, thread, &run, Some(thread_run)).expect("memoisable");
        let bci = |hit: Option<Option<&BacktraceFrame>>| {
            hit.map(|answer| answer.map(|f| f.to_entry().byte_code_index))
        };
        assert_eq!(bci(memo.lookup(5, thread, &run)), Some(Some(4)));
        assert_eq!(bci(memo.lookup(6, thread, &run)), None);
        assert_eq!(bci(memo.lookup(5, other, &run)), None);
        for miss in [
            entry(other, "p/Task", "run", "()V", 0),
            entry(task, "p/Task", "call", "()V", 0),
            entry(task, "p/Task", "run", "(I)V", 0),
        ] {
            assert_eq!(bci(memo.lookup(5, thread, &BacktraceFrame::Entry(miss))), None);
        }
        let none = ThreadRunBottomMemo::new(5, thread, &run, None).expect("memoisable");
        assert!(matches!(none.lookup(5, thread, &run), Some(None)));
        // A frame without a class id is never memoised.
        let anonymous = BacktraceFrame::Inlined {
            label: Arc::from("p/Task.run:()V"),
            class_id: 0,
            bci: 0,
            cp_stamp: None,
        };
        assert!(ThreadRunBottomMemo::new(5, thread, &anonymous, None).is_none());
        assert!(none.lookup(5, thread, &anonymous).is_none());
    }
}

#[cfg(test)]
mod r14w7_trace6_tests {
    use super::r13_trace3_standin_tests::{add_class, java, pool};
    use super::*;

    const INTERRUPTED: &str = "java/lang/InterruptedException";

    fn at(class_id: ClassId, bci: i32) -> BacktraceFrame {
        BacktraceFrame::Entry(StackTraceEntry {
            class_name: Arc::from("probe/Main"),
            method_name: Arc::from("main"),
            method_descriptor: Some(Arc::from("()V")),
            source_file: None,
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(class_id),
            method_index: None,
        })
    }

    /// `new #2; dup; invokespecial #6; athrow` per `IllegalArgumentException`
    /// site, padded with `nop` so each `invokespecial` lands at its pc.
    fn iae_body(sites: &[usize]) -> Vec<u8> {
        let mut code = Vec::new();
        for &site in sites {
            while code.len() + 4 < site {
                code.push(0x00);
            }
            code.extend_from_slice(&[0xbb, 0, 2, 0x59, 0xb7, 0, 6, 0xbf]);
        }
        code.push(0xb1);
        code
    }

    /// JDK-25-shaped `Thread.join(long, int)` (its two checks at pcs 4 and 12,
    /// lines as in the JDK 25 sources), `sleep(long, int)` with its checks at
    /// `sleep_sites`, a `join(long)` with one site, and an
    /// `Object.wait(long, int)` (no two-site row). `probe/Main.main()V` calls
    /// `join(JI)` at bci 1, `sleep(JI)` at 4, `join()` at 7, `join(J)` at 10
    /// and `wait(JI)` at 13.
    fn fixture(sleep_sites: &[usize]) -> (ClassStore, ClassId) {
        let iae = (STANDIN_ARG_CHECK_THROWABLE, "<init>", "(Ljava/lang/String;)V");
        let mut store = ClassStore::new();
        let object = add_class(
            &mut store,
            "java/lang/Object",
            None,
            pool(&[iae]),
            vec![java("wait", "(JI)V", iae_body(&[4]), &[(0, 492)])],
        );
        add_class(
            &mut store,
            "java/lang/Thread",
            Some(object),
            pool(&[iae]),
            vec![
                java("join", "(JI)V", iae_body(&[4, 12]), &[(0, 1924), (8, 1928)]),
                java("sleep", "(JI)V", iae_body(sleep_sites), &[(0, 567), (8, 571)]),
                java("join", "(J)V", iae_body(&[4]), &[(0, 1865)]),
                java("join", "()V", vec![0xb1], &[(0, 1963)]),
            ],
        );
        let main = add_class(
            &mut store,
            "probe/Main",
            Some(object),
            pool(&[
                ("java/lang/Thread", "join", "(JI)V"),
                ("java/lang/Thread", "sleep", "(JI)V"),
                ("java/lang/Thread", "join", "()V"),
                ("java/lang/Thread", "join", "(J)V"),
                ("java/lang/Object", "wait", "(JI)V"),
            ]),
            vec![java(
                "main",
                "()V",
                vec![
                    0x2a, 0xb6, 0, 6, 0xb8, 0, 12, 0xb6, 0, 18, 0xb6, 0, 24, 0xb6, 0, 30, 0xb1,
                ],
                &[(0, 3)],
            )],
        );
        (store, main)
    }

    fn one_frame(frames: Option<Vec<BacktraceFrame>>) -> Option<(String, i32, i32)> {
        let frames = frames?;
        assert_eq!(frames.len(), 1);
        let e = frames[0].to_entry();
        Some((
            format!("{}.{}", e.class_name, e.method_name),
            e.line_number,
            e.byte_code_index,
        ))
    }

    /// TW6-3: the native's bit picks the check -- `millis < 0` the first
    /// site, the `nanos` range the second -- for `join(long, int)` and
    /// `sleep(long, int)`; anything else answers nothing.
    #[test]
    fn a_two_site_argument_check_takes_the_site_the_native_names() {
        let (store, main) = fixture(&[4, 12]);
        let store = &store;
        let mut resolve =
            |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
        let mut got = |bci: i32, throwable: &str, second: bool| {
            one_frame(served_arg_check_standin_frames(
                store,
                &at(main, bci),
                throwable,
                second,
                &mut resolve,
            ))
        };
        let frame = |m: &str, line: i32, bci: i32| Some((m.to_string(), line, bci));
        let iae = STANDIN_ARG_CHECK_THROWABLE;
        assert_eq!(got(1, iae, false), frame("java/lang/Thread.join", 1924, 4));
        assert_eq!(got(1, iae, true), frame("java/lang/Thread.join", 1928, 12));
        assert_eq!(got(4, iae, false), frame("java/lang/Thread.sleep", 567, 4));
        assert_eq!(got(4, iae, true), frame("java/lang/Thread.sleep", 571, 12));
        // Not an `IllegalArgumentException`.
        assert_eq!(got(1, INTERRUPTED, true), None);
        // Not a two-site row: `join()`, `join(long)` (the one-site rule's),
        // `Object.wait(long, int)`.
        assert_eq!(got(7, iae, true), None);
        assert_eq!(got(10, iae, false), None);
        assert_eq!(got(13, iae, false), None);
        // Not at an invoke.
        assert_eq!(got(0, iae, false), None);
    }

    /// A row whose body does not have exactly two sites is refused, not
    /// guessed.
    #[test]
    fn a_two_site_row_with_another_site_count_gets_nothing() {
        for sites in [&[4usize][..], &[4, 12, 20][..]] {
            let (store, main) = fixture(sites);
            let store = &store;
            let mut resolve =
                |_: ClassId, name: &str| store.iter().find(|c| &*c.name == name).map(|c| c.id);
            for second in [false, true] {
                let got = served_arg_check_standin_frames(
                    store,
                    &at(main, 4),
                    STANDIN_ARG_CHECK_THROWABLE,
                    second,
                    &mut resolve,
                );
                assert!(got.is_none(), "{sites:?} {second}");
            }
        }
    }

    /// TW6-2: only an `InterruptedException` at a `join()` call, and only
    /// when the native said so.
    #[test]
    fn a_served_join_declines_only_when_told_at_a_join_call() {
        let (store, main) = fixture(&[4, 12]);
        let declined = |bci: i32, throwable: &str, declines: bool| {
            served_join_declines_standin_frames(&store, &at(main, bci), throwable, declines)
        };
        assert!(declined(7, INTERRUPTED, true));
        assert!(!declined(7, INTERRUPTED, false));
        assert!(!declined(7, STANDIN_ARG_CHECK_THROWABLE, true));
        assert!(!declined(1, INTERRUPTED, true), "join(long, int) declines by no hint");
        assert!(!declined(10, INTERRUPTED, true), "join(long) declines by no hint");
    }

    /// RV6-2: the opcode screen, and no answer for a frame whose code is not
    /// at hand.
    #[test]
    fn the_receiver_npe_screen_reads_only_instance_invokes() {
        // invokevirtual, invokespecial, invokeinterface, invokestatic, athrow.
        let code = [0xb6, 0, 1, 0xb7, 0, 1, 0xb9, 0, 1, 1, 0, 0xb8, 0, 1, 0xbf];
        assert!(code_is_instance_invoke_at(&code, 0));
        assert!(code_is_instance_invoke_at(&code, 3));
        assert!(code_is_instance_invoke_at(&code, 6));
        assert!(!code_is_instance_invoke_at(&code, 11));
        assert!(!code_is_instance_invoke_at(&code, 14));
        assert!(!code_is_instance_invoke_at(&code, 40));
        let (_, main) = fixture(&[4, 12]);
        assert!(!receiver_npe_gets_no_standin_frames(
            &at(main, 1),
            "java/lang/NullPointerException"
        ));
    }
}
