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
//! See `docs/internal/arch-2026-07-26/stackwalk-and-vtable.md` for what a
//! fully-lazy throw path additionally needs.

use std::sync::Arc;

use cratonvm_reader::attribute::{Attribute, LineNumberEntry};
use cratonvm_reader::method::ClassFileMethod;
use parking_lot::RwLock;

use crate::classloading::{Class, ClassId, ClassStore};
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

/// `Class::find_method` with the per-signature slot memo in front of it.
///
/// Semantically **identical** to `class.find_method(name, descriptor)` — the
/// memo only ever short-circuits a scan whose result is re-verified against
/// the live class. See the module-level rules above.
fn find_method_memoized<'a>(
    class: &'a Class,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<&'a ClassFileMethod> {
    let key = (class_id.as_u32(), signature_hash(name, descriptor));

    let memoized = method_slot_memo().read().get(&key).copied();
    if let Some(idx) = memoized {
        if let Some(m) = class.methods.get(idx as usize) {
            if &*m.name == name && &*m.descriptor == descriptor {
                return Some(m);
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

    {
        let mut w = method_slot_memo().write();
        if w.len() >= METHOD_SLOT_MEMO_CAP {
            w.clear();
        }
        w.insert(key, idx as u32);
    }
    class.methods.get(idx)
}

/// Test/diagnostic hook: drop every memoized slot. Never needed for
/// correctness (every hit is verified) — exists so unit tests can assert the
/// cold path and the warm path agree.
pub fn clear_method_slot_memo() {
    method_slot_memo().write().clear();
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
pub fn entry_from_frame(class_store: &ClassStore, frame: &Frame) -> StackTraceEntry {
    let class_name = frame.class_name_arc();
    let method_name = frame.method_name_arc();
    let method_descriptor = frame.method_descriptor_arc();
    let source_file = frame.source_file_arc();
    let bci = frame.last_instr_pc;
    let bci_i32 = bci.min(i32::MAX as usize) as i32;

    let line_number = line_number_for_bci(
        class_store,
        frame.class_id,
        &method_name,
        &method_descriptor,
        bci,
    )
    .unwrap_or(LINE_NUMBER_UNKNOWN);

    StackTraceEntry {
        class_name,
        method_name,
        source_file,
        line_number,
        byte_code_index: bci_i32,
        class_id: Some(frame.class_id),
    }
}

/// Walk a frame slice and produce a `StackTraceEntry` vector with full
/// source-file / line-number / BCI data.
///
/// Iteration order is top-of-stack → bottom (i.e. the caller chain from
/// innermost to outermost), matching the order produced by
/// `JvmThread.frames.iter()`.
pub fn capture_full_trace(class_store: &ClassStore, frames: &[Frame]) -> Vec<StackTraceEntry> {
    frames
        .iter()
        .map(|f| entry_from_frame(class_store, f))
        .collect()
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
/// once a `ClassStore` borrow is available. That resolves every frame whose
/// method name is unambiguous within its declaring class and **leaves the rest
/// at `UNKNOWN`** — the descriptor needed to disambiguate overloads is not
/// carried on `StackTraceEntry` (see the doc's cross-owner request CR-SW-1).
/// It therefore never reports a *wrong* line, only an unknown one, which is
/// strictly better than the all-`UNKNOWN` snapshot this function returns.
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
/// 5. **exactly one** method declared by that class carries the recorded name.
///    With more than one, the entry names an overload set and
///    `StackTraceEntry` does not carry the descriptor needed to pick the right
///    member; two overloads have different `LineNumberTable`s, so guessing
///    would print a line from the wrong method body. Those entries keep
///    `UNKNOWN`.
///
/// Returns the number of entries that were filled in.
///
/// # Do not wire this onto the `Throwable` path yet
///
/// Rule 5 is a real fidelity ceiling, not a formality: overloaded methods are
/// everywhere in the JDK and in framework code (`StringBuilder.append`,
/// `Arrays.copyOf`, every builder API), so a throwable trace resolved this way
/// would print `Unknown Source` for frames that the eager
/// [`capture_full_trace`] resolves correctly today. That is a *regression* on
/// `printStackTrace`, which is precisely the trade this repo has been burned by
/// before (`tco-breaks-stacktrace-fidelity`).
///
/// It is a strict *improvement* for [`capture_frames_no_lines`] consumers,
/// which have no line numbers at all right now.
///
/// Closing the gap needs `StackTraceEntry` to carry the frame's method
/// identity — its descriptor, or better its index into `Class::methods` — so
/// deferred resolution is exact for every frame. `StackTraceEntry` lives in
/// `native-api/src/registry.rs`, which this pass does not own; see cross-owner
/// request CR-SW-1 in
/// `docs/internal/arch-2026-07-26/stackwalk-and-vtable.md`.
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
        let mut only: Option<&ClassFileMethod> = None;
        for m in &class.methods {
            if &*m.name == name {
                if only.is_some() {
                    // Overload set — cannot disambiguate without a descriptor.
                    only = None;
                    break;
                }
                only = Some(m);
            }
        }
        let Some(method) = only else {
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
    }
}

/// Consult a [`Class`]'s attributes for the `SourceFile` attribute.
/// Fallback for frames whose cached `source_file_arc()` is `None`.
pub fn source_file_of_class(class: &Class) -> Option<Arc<str>> {
    class.source_file.as_deref().map(Arc::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::{ClassLoaderId, ClassState};
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LineNumberEntry};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::method::ClassFileMethod;

    // -- ClassStore-backed fixtures -------------------------------------
    //
    // Mirrors `classloading::class::tests::make_class` so this module can
    // exercise `line_number_for_bci` / `resolve_line_numbers_in_place` through
    // a real `ClassStore` rather than reimplementing the scan (which is what
    // the pre-existing tests below had to do).

    fn named_method(name: &str, descriptor: &str, lines: Vec<LineNumberEntry>) -> ClassFileMethod {
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

    fn store_with(methods: Vec<ClassFileMethod>) -> (ClassStore, ClassId) {
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
            is_synthetic_stub: false,
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        };
        let id = store.add(class);
        (store, id)
    }

    fn entry_for(class_id: ClassId, method: &str, bci: i32) -> StackTraceEntry {
        StackTraceEntry {
            class_name: Arc::from("probe/Target"),
            method_name: Arc::from(method),
            source_file: Some(Arc::from("Target.java")),
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: bci,
            class_id: Some(class_id),
        }
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
