#[allow(unused_imports)]
use super::*;


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
pub(in crate::runtime::stackwalker) const METHOD_SLOT_MEMO_CAP: usize = 8192;

/// Maps `(ClassId, fnv1a64(name, descriptor))` to an index into
/// [`Class::methods`].
pub(in crate::runtime::stackwalker) fn method_slot_memo() -> &'static RwLock<FxHashMap<(u32, u64), u32>> {
    static MEMO: std::sync::OnceLock<RwLock<FxHashMap<(u32, u64), u32>>> =
        std::sync::OnceLock::new();
    MEMO.get_or_init(|| RwLock::new(fx_hashmap()))
}

/// FNV-1a over `name` then `descriptor`, with a separator so
/// `("ab", "c")` and `("a", "bc")` do not hash alike.
#[inline]
pub(in crate::runtime::stackwalker) fn signature_hash(name: &str, descriptor: &str) -> u64 {
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
pub(in crate::runtime::stackwalker) fn find_method_index_memoized(
    class: &Class,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<u32> {
    // The repeat of this thread's last answer (same class, same name and
    // descriptor slices): no hash, no lock; verified as every hit below is.
    let last_key = method_slot_last_key(class_id, name, descriptor);
    if let Some(idx) = method_slot_last_get(last_key) {
        if let Some(m) = class.methods.get(idx as usize) {
            if &*m.name == name && &*m.descriptor == descriptor {
                return Some(idx);
            }
        }
    }
    let idx = find_method_index_memoized_hashed(class, class_id, name, descriptor)?;
    method_slot_last_put(last_key, idx);
    Some(idx)
}

/// [`find_method_index_memoized`] past this thread's last-answer check.
pub(in crate::runtime::stackwalker) fn find_method_index_memoized_hashed(
    class: &Class,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<u32> {
    let key = (class_id.as_u32(), signature_hash(name, descriptor));

    // WALK21-8 (`CRATONVM_STACKWALK_METHOD_SLOT_FRONT_CACHE`): this thread's
    // lock-free front cache first, verified exactly as a shared-memo hit.
    let front = crate::jit::conservative_roots::r21_method_slot_front_cache_enabled();
    if front {
        if let Some(idx) = method_slot_front_get(key) {
            if let Some(m) = class.methods.get(idx as usize) {
                if &*m.name == name && &*m.descriptor == descriptor {
                    return Some(idx);
                }
            }
        }
    }

    let memoized = method_slot_memo().read().get(&key).copied();
    if let Some(idx) = memoized {
        if let Some(m) = class.methods.get(idx as usize) {
            if &*m.name == name && &*m.descriptor == descriptor {
                if front {
                    method_slot_front_put(key, idx);
                }
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
    if front {
        method_slot_front_put(key, idx);
    }
    Some(idx)
}

/// `Class::find_method` with the per-signature slot memo in front of it.
///
/// Semantically **identical** to `class.find_method(name, descriptor)`.
pub(in crate::runtime::stackwalker) fn find_method_memoized<'a>(
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
    method_slot_front_clear();
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
pub(in crate::runtime::stackwalker) const CLASS_ID_MEMO_CAP: usize = 4096;

/// Maps `fnv1a64(class_name)` to a [`ClassId`] as a raw `u32`.
pub(in crate::runtime::stackwalker) fn class_id_memo() -> &'static RwLock<FxHashMap<u64, u32>> {
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
/// tests all exercise **one** implementation of the JVM spec §4.7.12 rule.
///
/// The rule is HotSpot 25's `Method::line_number_from_bci`, table order
/// included (j27 wave 5, lane STACKHUNT,
/// `j27-line-table-duplicate-start-pc-picks-the-last-row-FIXED-20261029.md`): the
/// FIRST row whose `start_pc` equals `bci` wins outright; otherwise the row
/// with the largest `start_pc` below `bci`, the LAST of equals. A table with
/// two rows at one pc (Kotlin, Groovy, bytecode weavers; javac never writes
/// one) used to answer the last row at that exact pc. A bci at or past the
/// code's end has no line, as HotSpot's `0 <= bci && bci < code_size()`
/// (j27 wave 5, lane REFSP; it used to answer the last row).
pub(in crate::runtime::stackwalker) fn line_number_for_bci_in_method(method: &ClassFileMethod, bci: usize) -> Option<i32> {
    let code = method.code()?;
    if bci >= code.code.len() {
        return None;
    }
    // Multiple LineNumberTable attributes are permitted by the spec; the
    // union, in attribute order, is the effective table (HotSpot's parser
    // appends them to one stream in that order).
    let rows = code.attributes.iter().flat_map(|attr| match attr {
        Attribute::LineNumberTable(entries) => entries.as_slice(),
        _ => &[][..],
    });
    line_from_rows(rows, bci).map(i32::from)
}

/// [`line_number_for_bci_in_method`]'s scan over rows in table order.
pub(in crate::runtime::stackwalker) fn line_from_rows<'a>(
    rows: impl Iterator<Item = &'a LineNumberEntry>,
    bci: usize,
) -> Option<u16> {
    let mut best: Option<(u16, u16)> = None;
    for row in rows {
        let start = usize::from(row.start_pc);
        if start == bci {
            return Some(row.line_number);
        }
        if start < bci && best.is_none_or(|(s, _)| row.start_pc >= s) {
            best = Some((row.start_pc, row.line_number));
        }
    }
    best.map(|(_, line)| line)
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
pub(in crate::runtime::stackwalker) fn entry_from_frame_with_lines(
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
        class_id: frame.stack_trace_class_id(),
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
/// `-Xint`: `L3W39HotSwapWalkerLines.java`; even a
/// redefinition with identical bytes). A `Throwable` of the same activation
/// keeps the old body's line, so this is the walker's capture only
/// (`NativeExceptionAccess::capture_stack_walk_trace`). Interpreter round i1
/// wave 39, lane L3
/// (`docs/feature-designs/real-jdk-mode.md`).
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
    let jit = whole_stack_compiled_frames();
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

/// JIT round 19 wave 2 (lane throws2, WALK17-3 step 1;
/// `CRATONVM_STACKWALK_LEAN_CAPTURE=1`): [`capture_stack_walk_trace`] with the
/// line numbers DEFERRED, so a `StackWalker` walk that writes few frames
/// (`findFirst`, a short `limit`) does not search every frame's
/// `LineNumberTable`.
///
/// Only while no class of the process was ever redefined (the eager path's
/// own fast arm); otherwise exactly [`capture_stack_walk_trace`]. The entries
/// are the eager capture's built with `resolve_lines == false` (the same
/// builders, the same walk, the same OSR / call dedupe) and then marked: an
/// entry whose eager line is read from its method's table carries
/// [`cratonvm_native_api::LINE_NUMBER_DEFERRED`] (read at its
/// `byte_code_index`) or [`cratonvm_native_api::LINE_NUMBER_DEFERRED_AT_BCI_ZERO`]
/// (an OSR-overridden interpreter frame whose override bci is negative, which
/// the eager re-line reads at 0), and [`resolve_deferred_walk_line`] gives it
/// that line. Which entries those are is decided per slot of
/// [`collect_trace_slots`] over the same walk, index-aligned with the entries
/// (the alignment [`capture_stack_walk_trace`]'s redefinition arm already
/// relies on); a length disagreement takes the eager capture instead:
///
/// * an interpreter frame: when its replaced body gives no line of its own
///   (`Frame::own_line_number`, which the eager line defers to) and its method
///   index was found;
/// * an OSR-overridden interpreter frame: when its method index was found
///   (the eager re-line ignores the body's own line);
/// * a compiled activation: when its bci is known (`>= 0`) and its method
///   index was found;
/// * an inlined level: when its method index was found.
///
/// Every other entry's line is already the eager one.
pub(crate) fn capture_stack_walk_trace_deferred(
    cm: &crate::classloading::ClassManager,
    frames: &[Frame],
) -> Vec<StackTraceEntry> {
    let class_store = &cm.class_store;
    if cratonvm_classloading::class_redefinition_count() != 0 {
        return capture_stack_walk_trace(cm, frames);
    }
    let jit = whole_stack_compiled_frames();
    let mut out = if jit.is_empty() {
        let mut out: Vec<StackTraceEntry> = frames
            .iter()
            .map(|f| entry_from_frame_with_lines(class_store, f, false))
            .collect();
        for (entry, frame) in out.iter_mut().zip(frames) {
            defer_interp_line(entry, frame);
        }
        out
    } else {
        let (jit, osr_bci) = drop_osr_continuations(frames, jit);
        let mut out = interleave_compiled_frames(class_store, frames, &jit, &osr_bci, false);
        let slots = collect_trace_slots(frames, &jit, &osr_bci);
        if slots.len() != out.len() {
            drop(slots);
            return capture_stack_walk_trace(cm, frames);
        }
        let overridden: Vec<*const Frame> = osr_bci
            .keys()
            .filter_map(|&i| frames.get(i))
            .map(|f| f as *const Frame)
            .collect();
        for (entry, slot) in out.iter_mut().zip(&slots) {
            match slot {
                TraceSlot::Interp { frame, .. } => {
                    if overridden.iter().any(|&p| std::ptr::eq(p, *frame)) {
                        if entry.method_index.is_some() && entry.class_id.is_some() {
                            entry.line_number = if entry.byte_code_index < 0 {
                                cratonvm_native_api::LINE_NUMBER_DEFERRED_AT_BCI_ZERO
                            } else {
                                cratonvm_native_api::LINE_NUMBER_DEFERRED
                            };
                        }
                    } else {
                        defer_interp_line(entry, frame);
                    }
                }
                TraceSlot::Compiled(_) => {
                    if entry.byte_code_index >= 0 && entry.method_index.is_some() {
                        entry.line_number = cratonvm_native_api::LINE_NUMBER_DEFERRED;
                    }
                }
                TraceSlot::Inlined(..) => {
                    if entry.method_index.is_some() {
                        entry.line_number = cratonvm_native_api::LINE_NUMBER_DEFERRED;
                    }
                }
            }
        }
        out
    };
    fill_missing_source_files(class_store, &mut out);
    out
}

/// [`capture_stack_walk_trace_deferred`]'s rule for a non-overridden
/// interpreter frame's entry (built by `entry_from_frame_with_lines(.., false)`).
pub(in crate::runtime::stackwalker) fn defer_interp_line(entry: &mut StackTraceEntry, frame: &Frame) {
    if entry.method_index.is_some() && frame.own_line_number(entry.byte_code_index).is_none() {
        entry.line_number = cratonvm_native_api::LINE_NUMBER_DEFERRED;
    }
}

/// Give a [`capture_stack_walk_trace_deferred`] entry its line (WALK17-3 step
/// 1): the method `class_id` / `method_index` name, at the bci its sentinel
/// says, exactly as the eager builders read it; `-1` where they found none.
/// A no-op on an entry with no deferred line.
///
/// A redefinition between the capture and this call (a stream that redefines
/// a class mid-walk) can no longer read the table the eager capture read: an
/// entry of a class redefined since is then an activation older than its
/// class, and gets what [`capture_stack_walk_trace`] gives one -- no line and
/// no source file (`clear_old_activation_line`). The capture took the deferred
/// path only with no redefinition in the process, so any history means "since".
pub(crate) fn resolve_deferred_walk_line(
    cm: &crate::classloading::ClassManager,
    entry: &mut StackTraceEntry,
) {
    let at = match entry.line_number {
        cratonvm_native_api::LINE_NUMBER_DEFERRED => entry.byte_code_index.max(0) as usize,
        cratonvm_native_api::LINE_NUMBER_DEFERRED_AT_BCI_ZERO => 0,
        _ => return,
    };
    entry.line_number = LINE_NUMBER_UNKNOWN;
    let Some(class_id) = entry.class_id else {
        return;
    };
    if cratonvm_classloading::class_redefinition_count() != 0
        && cm.redefinition_history(class_id).is_some()
    {
        clear_old_activation_line(entry);
        return;
    }
    entry.line_number = cm
        .class_store
        .get(class_id)
        .zip(entry.method_index)
        .and_then(|(class, idx)| class.methods.get(idx as usize))
        .and_then(|m| line_number_for_bci_in_method(m, at))
        .unwrap_or(LINE_NUMBER_UNKNOWN);
}

/// Did `frame`'s activation begin before a redefinition of its class? Moved
/// across one (`Frame::predates_its_class_redefinition`, which an obsolete
/// frame always is), or not yet moved while its stamp is older than the
/// class's last redefinition.
pub(in crate::runtime::stackwalker) fn frame_predates_its_class_redefinition(
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
pub(in crate::runtime::stackwalker) fn slot_predates_its_class_redefinition(
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
        TraceSlot::Compiled(compiled) => stale(compiled.owner_class_id, compiled.compile_cp_stamp),
        TraceSlot::Inlined(level, stamp) => stale(level.class_id, *stamp),
    }
}

/// Leave `entry`'s line and source file unknown: it stands for an old
/// activation. HotSpot's element of such a frame has neither (measured: the
/// probe's `file` row). The source file is honoured by the JDK walk's
/// carrier (`lang_stackwalker::populate_sfi`) and, since wave 40, by the
/// native `walk`'s carrier, which reads the file lazily from the class only
/// for an entry that names one (`reflect_invoke::populate_stack_frame`).
pub(in crate::runtime::stackwalker) fn clear_old_activation_line(entry: &mut StackTraceEntry) {
    if entry.line_number == LINE_NUMBER_UNKNOWN && entry.source_file.is_none() {
        return;
    }
    if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_ok() {
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
/// `docs/feature-designs/real-jdk-mode.md`
/// item 1).
pub(in crate::runtime::stackwalker) fn fill_missing_source_files(class_store: &ClassStore, entries: &mut [StackTraceEntry]) {
    for entry in entries.iter_mut() {
        if entry.source_file.is_some() {
            continue;
        }
        let Some(class) = entry
            .class_id
            .and_then(|class_id| class_store.get(class_id))
        else {
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
    let jit = crate::jit::stack_trace::active_compiled_frames();
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
    let jit = crate::jit::stack_trace::active_compiled_frames();
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
pub(in crate::runtime::stackwalker) const THROWABLE_TRIM_SLACK: usize = 64;

/// How many innermost interpreter frames a throwable capture capped at
/// `max_depth` needs to walk: the cap plus the trim slack, or everything for
/// an uncapped (`0`) capture.
pub(in crate::runtime::stackwalker) fn throwable_capture_budget(max_depth: usize) -> usize {
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

/// Does `store` keep hidden-method bits (`CRATONVM_HIDDEN_METHOD_BITS`, read
/// once per store, so per VM)? Asked of a class the caller knows is stored
/// (`probe`, the throwable's class), whose bits it then keeps. Interpreter
/// round i6 wave 123, lane L3.
pub(crate) fn store_keeps_hidden_method_bits(store: &ClassStore, probe: ClassId) -> bool {
    store.hidden_method_bits(probe, hidden_method_bits_of).is_some()
}

/// The raw window [`recapture_past_hidden_cut`] tries first: `max_depth`
/// scaled by the cut capture's frames per kept frame (`captured` frames, of
/// which `kept` survived the hidden drop), plus the trim slack, and always
/// past both `max_depth` and `captured` so every try walks further.
pub(in crate::runtime::stackwalker) fn hidden_cut_first_window(max_depth: usize, captured: usize, kept: usize) -> usize {
    let scaled = if kept == 0 {
        max_depth.saturating_mul(2)
    } else {
        let wide = (max_depth as u128).saturating_mul(captured as u128) / kept as u128;
        usize::try_from(wide).unwrap_or(usize::MAX)
    };
    scaled
        .saturating_add(THROWABLE_TRIM_SLACK)
        .max(max_depth.saturating_add(1))
        .max(captured.saturating_add(1))
}

/// Step 3 of `i37-L6-proposal-hidden-method-bit-at-link-time`, the cut half
/// (interpreter round i6 wave 123, lane L3; `CRATONVM_HIDDEN_METHOD_BITS`,
/// default off, performance).
///
/// HotSpot's `fill_in_stack_trace` skips a hidden frame before it counts it,
/// so `MaxJavaStackTraceDepth` counts recorded frames only. The capture here
/// cuts at the cap first and drops hidden frames after (the `Thread.run` and
/// loader middle rules must still see them), and wave 37 redid a cut capture
/// that lost hidden frames WHOLE: a `StackOverflowError` through lambda forms
/// walked and built every frame of a ten-thousand-deep stack to keep 1024.
///
/// This redoes it in growing windows instead. `attempt(window)` is the same
/// capture the whole redo ran -- walk, middle rules, hidden drop -- capped at
/// `window` raw frames, answering the processed trace and how many raw
/// frames the walk gave. A capped capture is the innermost suffix of the
/// whole one, every middle rule judges an adjacent pair (a frame cut at the
/// window's outer edge only loses frames outer to it) and the drop is per
/// frame, so the processed window is a suffix of the processed whole: once
/// it holds `max_depth` frames, or the walk ran out (`raw < window`), its
/// cap is the whole redo's cap exactly. Otherwise the window doubles.
///
/// With the stamp (wave 122), the drop decides a stamped inlined level with
/// no class-store read; the first window is sized from the cut capture's own
/// hidden share (`captured` frames, `kept` after the drop).
pub(crate) fn recapture_past_hidden_cut(
    max_depth: usize,
    captured: usize,
    kept: usize,
    attempt: &mut dyn FnMut(usize) -> (Vec<BacktraceFrame>, usize),
) -> Vec<BacktraceFrame> {
    if max_depth == 0 {
        // Uncapped: nothing was cut. Kept total.
        return attempt(0).0;
    }
    let mut window = hidden_cut_first_window(max_depth, captured, kept);
    loop {
        let (mut trace, raw) = attempt(window);
        if raw < window || trace.len() >= max_depth {
            cap_throwable_trace(&mut trace, max_depth);
            return trace;
        }
        match window.checked_mul(2) {
            Some(wider) => window = wider,
            None => {
                let (mut whole, _) = attempt(0);
                cap_throwable_trace(&mut whole, max_depth);
                return whole;
            }
        }
    }
}

/// [`capture_full_trace_without_store`] followed by
/// [`trim_throwable_fill_frames`], with the trim decided BEFORE any
/// [`StackTraceEntry`] is built.
///
/// The frames the trim removes are exactly the ones a throwable's construction
/// puts on top of its throw site: the `fillInStackTrace` frames and the
/// `<init>` chain of its own class. Under `--real-jdk` that chain is real
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

/// [`capture_throwable_backtrace_without_store`] on compiled frames the caller
/// already took with [`throwable_capture_compiled_frames`] for the same
/// `frames` (the capture memo keys on them), handing the walk back as it came
/// in for the memo to keep (JIT round 16 wave 9, lane throwpath; it replaces
/// wave 7's `capture_throwable_backtrace_with_compiled`). Wave 7's memo
/// cloned the whole walk BEFORE every capture --
/// every frame's label refcount and inline-chain `Vec` -- to keep a copy the
/// capture would consume; that clone was paid on every miss, and with the memo
/// never made in compiled code (wave 9's finding) on every capture. Here the
/// walk is laid out by reference and returned; it is copied only when the OSR
/// / call dedupe has to rewrite it (a compiled frame names the interpreter
/// frame it was pushed from), which is the one case the layout cannot borrow.
/// The trace is [`capture_throwable_slots_with`]'s on the same walk, exactly.
pub(crate) fn capture_throwable_backtrace_returning_walk(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> (Vec<BacktraceFrame>, Vec<ActiveCompiledFrame>) {
    if jit.is_empty() {
        // The bounded interpreter window; there is no walk to keep.
        let trace = capture_throwable_slots_with(frames, Vec::new(), max_depth, is_a, splices);
        return (trace, jit);
    }
    // `drop_osr_continuations` hands an unnamed walk back unchanged and
    // without overrides; only a naming one is rewritten.
    let rewritten = osr_frame_dedupe_enabled()
        && jit
            .iter()
            .any(|f| compiled_frame_names_its_caller(frames, f));
    if !rewritten {
        let no_osr = OsrBciOverrides::default();
        let trace = trace_from_compiled_layout(frames, &jit, &no_osr, max_depth, is_a, splices);
        return (trace, jit);
    }
    let walk = jit.clone();
    let (kept, osr_bci) = drop_osr_continuations(frames, jit);
    let trace = trace_from_compiled_layout(frames, &kept, &osr_bci, max_depth, is_a, splices);
    (trace, walk)
}

/// The compiled frames a throwable capture made on top of `depth` interpreter
/// frames lays out: the armed trap snapshot when one stands in for the walk
/// (see `arm_trap_capture`), else the live walk, each chain entry read to at
/// most [`throwable_walk_activation_bound`]`(max_depth)` activations.
pub(crate) fn throwable_capture_compiled_frames(
    depth: usize,
    max_depth: usize,
) -> Vec<ActiveCompiledFrame> {
    throwable_capture_compiled_frames_with(depth, false, max_depth)
}

/// The compiled-walk options a THROWABLE capture asks for
/// ([`crate::jit::stack_trace::CompiledWalkOptions`]): THROWS18-2's
/// shared inline chains and WALK17-4's activation memo, each under its switch.
/// Every reader of a throwable capture's walk reads a chain through
/// [`ActiveCompiledFrame::chain`] (or [`ActiveCompiledFrame::into_chain_vec`]).
/// The same frames either way.
pub(crate) fn throwable_walk_options() -> crate::jit::stack_trace::CompiledWalkOptions {
    use crate::jit::conservative_roots as walk;
    walk::CompiledWalkOptions {
        shared_inline_chains: walk::throws2_shared_inline_chains_enabled(),
        activation_memo: walk::throws2_activation_memo_enabled(),
    }
}

/// THROWS19-8 (round 19 wave 2; `CRATONVM_THROWABLE_MEMO_MISS_FUSED=1`): a
/// capture-memo miss lays its trace out with the fused door and keeps a COPY
/// of the walk for the memo (the memo's compact key), instead of the plain
/// door. Read by `vm_exec`'s capture.
pub(crate) fn memo_miss_fused_enabled() -> bool {
    crate::jit::conservative_roots::throws2_memo_miss_fused_enabled()
}

/// WALK17-3 step 1 (round 19 wave 2; `CRATONVM_STACKWALK_LEAN_CAPTURE=1`):
/// a `StackWalker` walk is captured with its lines deferred
/// ([`capture_stack_walk_trace_deferred`]) and its ordered list built
/// without cloning the trace.
pub(crate) fn stackwalk_lean_capture_enabled() -> bool {
    crate::jit::conservative_roots::throws2_stackwalk_lean_capture_enabled()
}

/// The whole compiled walk of a `Thread.getStackTrace` / `StackWalker`
/// capture ([`active_compiled_frames`](crate::jit::stack_trace::active_compiled_frames)),
/// with WALK17-4's per-walk activation memo under
/// `CRATONVM_COMPILED_WALK_ACTIVATION_MEMO=1` (round 19 wave 2). The same
/// frames. Chains stay owned, except under
/// `CRATONVM_WALK_SHARED_INLINE_CHAINS=1` (round 19 wave 4, lane throws4,
/// THROWS2-1): every reader of this walk -- `drop_osr_continuations` (which
/// copies a shared chain into its override), `interleave_compiled_frames` /
/// `push_compiled_frames` / `push_inlined_chain`, `collect_trace_slots` and
/// the redefinition / deferred-line passes over its slots -- reads a chain
/// through [`ActiveCompiledFrame::chain`], the readers the throwable path's
/// THROWS18-2 already proves.
pub(in crate::runtime::stackwalker) fn whole_stack_compiled_frames() -> Vec<ActiveCompiledFrame> {
    crate::jit::stack_trace::active_compiled_frames_whole_opts(
        // The residual-costs page, item 3: one code-range view per walk.
        crate::jit::conservative_roots::stackwalk_scoped_code_lookups_enabled(),
        crate::jit::stack_trace::CompiledWalkOptions {
            shared_inline_chains:
                crate::jit::conservative_roots::throws4_walk_shared_inline_chains_enabled(),
            activation_memo: crate::jit::conservative_roots::throws2_activation_memo_enabled(),
        },
    )
}

/// THROWS19-8 (round 19 wave 2): the memo-miss capture through the fused door.
/// Exactly [`capture_throwable_backtrace_returning_walk`]'s trace (the fused
/// door is the plain door's trace, TF17-1 / THROWS18-1), and the walk the
/// memo keeps is a copy of the one the fused door consumes. With shared
/// inline chains (THROWS18-2) the copy is one label and one chain reference
/// count per compiled activation, which is the compact key THROWS19-8 asks
/// for: [`ThrowableCaptureMemo`] reads only `same_compiled_frame`'s fields and
/// the OSR dedupe inputs out of it.
pub(crate) fn capture_throwable_backtrace_fused_returning_walk(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    max_depth: usize,
    rewritten: bool,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> (Vec<BacktraceFrame>, Vec<ActiveCompiledFrame>) {
    let kept = jit.clone();
    let trace =
        capture_throwable_backtrace_fused_with(frames, jit, max_depth, rewritten, is_a, splices);
    (trace, kept)
}

/// The per-entry activation bound of a throwable capture's compiled walk
/// capped at `max_depth` (JIT round 17 wave 2, lane walk).
///
/// Until that wave every walk stopped a chain entry after 4096 RBP steps
/// (`r17-compiled-walk-4096-activation-guard-drops-middle-frames-FIXED-20261005.md`).
/// The whole-entry walk (`CRATONVM_JIT_COMPILED_WALK_WHOLE_ENTRY`, default on)
/// reads a deep compiled recursion to its end, which `Thread.getStackTrace`,
/// `StackWalker` and an uncapped throwable need -- and which a capped
/// throwable must not pay for: a `StackOverflowError` is built at the deepest
/// stack the thread can have, tens of thousands of compiled activations on a
/// default thread, to keep 1024. So a capped capture keeps the bound the guard
/// used to give it (4097 activations: the innermost and 4096 steps), widened to
/// the cap and twice the trim slack when the cap is larger: the same frames
/// and the same cost as before the fix for every cap up to about 3900 (the
/// boundary method is no longer pushed after the cut, which only ever stood
/// 4097 frames out, past the cap). Uncapped (`0`), or with the whole-entry
/// walk switched off, there is no bound (the legacy walk exactly).
///
/// `CRATONVM_THROWABLE_BOUNDED_WALK=1` (TF17-5) bounds it to the cap and
/// verifies the answer instead: [`capture_throwable_backtrace_bounded`].
pub(crate) fn throwable_walk_activation_bound(max_depth: usize) -> usize {
    const LEGACY_ACTIVATIONS: usize = 4097;
    if max_depth == 0 || !crate::jit::conservative_roots::compiled_walk_whole_entry_enabled() {
        return usize::MAX;
    }
    max_depth
        .saturating_add(THROWABLE_TRIM_SLACK.saturating_mul(2))
        .max(LEGACY_ACTIVATIONS)
}

/// [`throwable_capture_compiled_frames`], with the live walk's lookups scoped
/// to the walk when `walk_scoped` (JIT round 16 wave 15, lane throwfill;
/// `CRATONVM_THROWABLE_WALK_SCOPED_LOOKUPS=1`,
/// `conservative_roots::active_compiled_frames_walk_scoped`): the same frames.
pub(crate) fn throwable_capture_compiled_frames_with(
    depth: usize,
    walk_scoped: bool,
    max_depth: usize,
) -> Vec<ActiveCompiledFrame> {
    if let Some(snapshot) = armed_trap_capture_frames(depth) {
        return snapshot;
    }
    let bound = throwable_walk_activation_bound(max_depth);
    // Round 19 wave 2 (lane throws2): the walk options the switches ask for;
    // the default options are the walks below exactly.
    let opts = throwable_walk_options();
    if bound != usize::MAX {
        let mut cuts = Vec::new();
        return crate::jit::stack_trace::active_compiled_frames_cut_opts(
            bound,
            walk_scoped,
            &mut cuts,
            opts,
        );
    }
    crate::jit::stack_trace::active_compiled_frames_whole_opts(walk_scoped, opts)
}

/// The per-entry activation budget of a bounded throwable walk capped at
/// `max_depth` (TF17-5): the cap plus the trim slack. Every compiled activation
/// lays out at least one slot unless its label is refused, so a cut entry
/// almost always leaves `max_depth` survivors inside the exact part; when it
/// does not, the capture is redone unbounded -- a cost knob, not a correctness
/// one.
pub(in crate::runtime::stackwalker) fn throwable_bounded_walk_budget(max_depth: usize) -> usize {
    max_depth.saturating_add(THROWABLE_TRIM_SLACK)
}

/// JIT round 17 wave 2 (lane walk, TF17-5; `CRATONVM_THROWABLE_BOUNDED_WALK=1`,
/// default OFF): a throwable capture capped at `max_depth` that walks only the
/// innermost [`throwable_bounded_walk_budget`] activations of each chain entry,
/// as HotSpot's `fill_in_stack_trace` stops at `MaxJavaStackTraceDepth`.
/// Exactly [`capture_throwable_backtrace_from_walk`] (or, `fused`,
/// [`capture_throwable_backtrace_fused`]) on the whole walk.
///
/// The bounded walk ([`crate::jit::stack_trace::active_compiled_frames_cut`])
/// says which entries it cut. With no cut it IS the whole walk. With a cut,
/// the answer is accepted only when [`bounded_capture_first_kept`] proves the
/// innermost `max_depth` survivors lie inside the part of the layout the cut
/// cannot have changed; otherwise the walk is redone unbounded. An armed trap
/// snapshot is used whole, as every capture uses it.
pub(crate) fn capture_throwable_backtrace_bounded(
    frames: &[Frame],
    max_depth: usize,
    walk_scoped: bool,
    fused: bool,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<BacktraceFrame> {
    let snapshot = armed_trap_capture_frames(frames.len());
    if max_depth == 0
        || snapshot.is_some()
        || !crate::jit::conservative_roots::compiled_walk_whole_entry_enabled()
    {
        let jit = match snapshot {
            Some(snapshot) => snapshot,
            None => throwable_capture_compiled_frames_with(frames.len(), walk_scoped, max_depth),
        };
        return throwable_whole_walk_door(frames, jit, max_depth, fused, is_a, splices);
    }
    let mut cuts = Vec::new();
    let jit = crate::jit::stack_trace::active_compiled_frames_cut_opts(
        throwable_bounded_walk_budget(max_depth),
        walk_scoped,
        &mut cuts,
        throwable_walk_options(),
    );
    let Some(cut) = cuts.last().copied() else {
        return throwable_whole_walk_door(frames, jit, max_depth, fused, is_a, splices);
    };
    if let Some(trace) = bounded_capture_verified(frames, jit, cut, max_depth, is_a, splices) {
        return trace;
    }
    let whole = throwable_capture_compiled_frames_with(frames.len(), walk_scoped, 0);
    throwable_whole_walk_door(frames, whole, max_depth, fused, is_a, splices)
}

/// The verified half of [`capture_throwable_backtrace_bounded`]: the capped
/// trace of the bounded walk `jit` whose innermost cut is `cut`, when it is
/// provably the whole walk's ([`bounded_capture_first_kept`]); `None` (the walk
/// dropped) when the shape is one the fused layout does not cover -- a
/// reflective splice, `CRATONVM_JIT_COMPACT_COMPILED_BACKTRACE=0`, a compiled
/// frame naming the interpreter frame it was pushed from -- or the proof fails.
pub(in crate::runtime::stackwalker) fn bounded_capture_verified(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    cut: crate::jit::conservative_roots::CompiledWalkCut,
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Option<Vec<BacktraceFrame>> {
    let covered = max_depth != 0
        && splices.is_empty()
        && compact_compiled_backtrace_enabled()
        && !(osr_frame_dedupe_enabled()
            && jit
                .iter()
                .any(|f| compiled_frame_names_its_caller(frames, f)));
    if !covered {
        return None;
    }
    let no_osr = OsrBciOverrides::default();
    let (slots, origins) = collect_trace_slots_with_origins_osr(frames, &jit, &no_osr);
    let keep = throwable_fill_frame_keep_len(&slots, is_a).min(origins.len());
    let first = bounded_capture_first_kept(&slots, &origins, keep, cut, max_depth);
    drop(slots);
    let first = first?;
    Some(build_fused_survivors_osr(
        frames,
        jit,
        &no_osr,
        &origins[..keep],
        first,
        max_depth,
    ))
}

/// The door [`capture_throwable_backtrace_bounded`] hands a whole walk to: the
/// fused one under `CRATONVM_THROWABLE_FUSED_CAPTURE=1`, else the plain one.
pub(in crate::runtime::stackwalker) fn throwable_whole_walk_door(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    max_depth: usize,
    fused: bool,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<BacktraceFrame> {
    if fused {
        capture_throwable_backtrace_fused(frames, jit, max_depth, is_a, splices)
    } else {
        capture_throwable_backtrace_from_walk(frames, jit, max_depth, is_a, splices)
    }
}

/// Is the innermost cut of a bounded throwable walk outside the capped trace?
/// (TF17-5.) `slots` / `origins` are the fused layout of the bounded walk,
/// `keep` the fill-frame trim's answer on it, `cut` its innermost cut.
///
/// The slots from the cut entry's outermost reported activation on (`q`, the
/// first compiled or inlined slot of `jit[cut.first_reported..]`) are exactly
/// the last slots of the whole walk's layout: the activations the cut removed
/// (and the boundary it did not push) would lie immediately before that slot,
/// laid out by the same entry's push, and the layout is otherwise the same
/// function of the same frames. When none of the cut entry's kept activations
/// laid out a slot, `q` is an inner entry's first slot (a smaller exact part,
/// still exact) or there is none (`None`). The trim, which works from the
/// inner end, decides the same `keep` on both while `keep > q`.
/// So when the innermost `max_depth` built survivors all lie in `[q, keep)`,
/// the capped trace is the whole walk's: `Some(first)`, the position of the
/// outermost kept survivor, as [`capture_throwable_backtrace_fused`] counts it.
/// `None` otherwise (the capture redoes the walk unbounded).
///
/// The caller has refused every walk with a compiled frame naming the
/// interpreter frame it was pushed from: then the whole walk's OSR / call
/// dedupe can touch only activations the cut removed and the interpreter
/// frames below them (outer to `q`), whose `(depth, label)` ledger is read
/// only for naming frames.
pub(in crate::runtime::stackwalker) fn bounded_capture_first_kept(
    slots: &[TraceSlot<'_>],
    origins: &[FusedOrigin],
    keep: usize,
    cut: crate::jit::conservative_roots::CompiledWalkCut,
    max_depth: usize,
) -> Option<usize> {
    if max_depth == 0 {
        return None;
    }
    let q = origins.iter().position(|origin| match *origin {
        FusedOrigin::Interp(_) | FusedOrigin::OsrInterp(_) | FusedOrigin::OsrInlined(..) => false,
        FusedOrigin::Compiled(j) | FusedOrigin::Inlined(j, _) => j >= cut.first_reported,
    })?;
    let end = keep.min(slots.len());
    if q >= end {
        return None;
    }
    let mut built = 0usize;
    for (offset, slot) in slots[q..end].iter().enumerate().rev() {
        let builds = match slot {
            TraceSlot::Interp { .. } => BacktraceFrame::from_slot(slot).is_some(),
            TraceSlot::Compiled(_) | TraceSlot::Inlined(..) => true,
        };
        if builds {
            built += 1;
            if built == max_depth {
                return Some(q + offset);
            }
        }
    }
    None
}

/// [`capture_throwable_backtrace_without_store`] on compiled frames the caller
/// already took ([`throwable_capture_compiled_frames_with`] for the same
/// `frames`): exactly its trace (JIT round 16 wave 15, lane throwfill).
pub(crate) fn capture_throwable_backtrace_from_walk(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<BacktraceFrame> {
    capture_throwable_slots_with(frames, jit, max_depth, is_a, splices)
}

/// JIT round 17 wave 1 (lane throwfill, TF17-1;
/// `CRATONVM_THROWABLE_FUSED_CAPTURE=1`): [`capture_throwable_backtrace_from_walk`]
/// with the per-frame work of a compiled capture cut to what the retained
/// trace needs. Exactly its trace.
///
/// The plain door lays the walk out as borrowed [`TraceSlot`]s, builds each
/// survivor by CLONING its label `Arc` (and each inlined level's), and then
/// drops the walk, releasing every one of those labels again: two atomic
/// read-modify-writes per compiled frame and per inlined level that change
/// nothing, on top of the walk's own clone and the retained frame's eventual
/// release. Here the layout records where each slot came from
/// ([`FusedOrigin`]), the trim runs on the same slots, and the survivors are
/// built by CONSUMING the walk in order, so a retained compiled or inlined
/// frame takes the label the walk already owns. The label validity split
/// ([`split_frame_label`], the layout's refusal) is asked once per distinct
/// label (`LabelSplitMemo`: a compiled recursion, or two methods calling each
/// other, repeat the same two labels down the whole stack).
///
/// Every shape the fused layout does not cover goes to the plain door
/// unchanged: no compiled frame (the bounded interpreter window), a
/// reflective splice, `CRATONVM_JIT_COMPACT_COMPILED_BACKTRACE=0`, and any
/// walk with a compiled frame naming the interpreter frame it was pushed from
/// (the OSR / call dedupe rewrites such a walk). With none of those,
/// [`drop_osr_continuations`] hands the walk back unchanged with no
/// overrides, which is the layout below.
pub(crate) fn capture_throwable_backtrace_fused(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<BacktraceFrame> {
    capture_throwable_backtrace_fused_with(frames, jit, max_depth, false, is_a, splices)
}

/// [`capture_throwable_backtrace_fused`], and with `rewritten` (JIT round 19
/// wave 1, lane throwfill, THROWS18-1; `CRATONVM_THROWABLE_FUSED_REWRITTEN=1`)
/// a walk the OSR / call dedupe rewrites -- the [`FusedRefusal::Rewritten`]
/// refusal -- is laid out by the fused door too: [`drop_osr_continuations`]
/// runs first, exactly as the plain door runs it, and the fused layout is
/// handed the deduped walk with its bci overrides (an overridden interpreter
/// frame's bci, then the chain the override carries, laid out as
/// [`collect_trace_slots`] lays them out). Exactly the plain door's trace:
/// the plain door is that same dedupe followed by
/// [`trace_from_compiled_layout`] on the same `(kept, overrides)`, and the
/// fused layout of a walk with overrides is the plain layout's slots in the
/// same order. Every other refusal still goes to the plain door.
pub(crate) fn capture_throwable_backtrace_fused_with(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    max_depth: usize,
    rewritten: bool,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<BacktraceFrame> {
    match fused_capture_refusal(frames, &jit, splices) {
        None => {
            let no_osr = OsrBciOverrides::default();
            fused_layout_and_build(frames, jit, &no_osr, max_depth, is_a)
        }
        Some(FusedRefusal::Rewritten) if rewritten => {
            let (kept, osr_bci) = drop_osr_continuations(frames, jit);
            fused_layout_and_build(frames, kept, &osr_bci, max_depth, is_a)
        }
        Some(_) => capture_throwable_slots_with(frames, jit, max_depth, is_a, splices),
    }
}

/// The fused door's body once its refusals are passed: the layout with
/// origins (with the dedupe's `osr_bci` overrides, empty for an unrewritten
/// walk), the trim, the cap's first kept slot, the survivors built by
/// consuming `jit`. No reflective splice reaches here.
pub(in crate::runtime::stackwalker) fn fused_layout_and_build(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    osr_bci: &OsrBciOverrides,
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
) -> Vec<BacktraceFrame> {
    let (slots, origins) = collect_trace_slots_with_origins_osr(frames, &jit, osr_bci);
    debug_assert_eq!(slots.len(), origins.len());
    let keep = throwable_fill_frame_keep_len(&slots, is_a).min(origins.len());
    // Only the innermost `max_depth` survivors are kept by the cap, so a
    // capture deeper than the cap (a `StackOverflowError`) builds only those:
    // the first kept slot is found from the inner end, counting the slots
    // that build a frame. A compiled or inlined slot always does (the layout
    // already applied its refusals); an interpreter slot is asked.
    let first = if max_depth != 0 && keep > max_depth {
        let mut built = 0usize;
        let mut first = 0usize;
        for (at, slot) in slots[..keep].iter().enumerate().rev() {
            let builds = match slot {
                TraceSlot::Interp { .. } => BacktraceFrame::from_slot(slot).is_some(),
                TraceSlot::Compiled(_) | TraceSlot::Inlined(..) => true,
            };
            if builds {
                built += 1;
                if built == max_depth {
                    first = at;
                    break;
                }
            }
        }
        first
    } else {
        0
    };
    // The slots borrow the walk; from here on only the origins are read.
    drop(slots);
    build_fused_survivors_osr(frames, jit, osr_bci, &origins[..keep], first, max_depth)
}

/// Why [`capture_throwable_backtrace_fused`] hands a walk to the plain door
/// (JIT round 18 wave 10, lane throws). The discriminants are the refusal
/// columns of the TF17-0 census (`ThrowableLayoutCache::note_throwfill_shape`,
/// `THROWFILL_SHAPE_REFUSAL_NAMES` order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FusedRefusal {
    /// No compiled frame: the bounded interpreter window.
    NoCompiledFrame = 0,
    /// A reflective call's JDK frames are spliced in.
    Splice = 1,
    /// `CRATONVM_JIT_COMPACT_COMPILED_BACKTRACE=0`.
    CompactOff = 2,
    /// A compiled frame names the interpreter frame it was pushed from (the
    /// OSR / call dedupe rewrites the walk).
    Rewritten = 3,
}

/// The fused door's refusal of `jit` (the compiled walk of `frames`), or
/// `None` when [`capture_throwable_backtrace_fused`] lays it out itself. Pure;
/// the census asks it of the same walk the door is then handed.
pub(crate) fn fused_capture_refusal(
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    splices: &[ReflectiveSplice],
) -> Option<FusedRefusal> {
    if jit.is_empty() {
        return Some(FusedRefusal::NoCompiledFrame);
    }
    if !splices.is_empty() {
        return Some(FusedRefusal::Splice);
    }
    if !compact_compiled_backtrace_enabled() {
        return Some(FusedRefusal::CompactOff);
    }
    if osr_frame_dedupe_enabled()
        && jit
            .iter()
            .any(|f| compiled_frame_names_its_caller(frames, f))
    {
        return Some(FusedRefusal::Rewritten);
    }
    None
}

/// Where one slot of the fused layout came from (TF17-1): interpreter frame
/// `frames[i]`, compiled activation `jit[j]`, or level `k` of `jit[j]`'s
/// inline chain (`k` indexes the chain as stored, innermost first). THROWS18-1
/// (round 19): `OsrInterp(i)` is interpreter frame `frames[i]` at the bci
/// its OSR override gives, and `OsrInlined(i, k)` level `k` of the chain that
/// override carries (an override's chain is laid out with no compile stamp).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::stackwalker) enum FusedOrigin {
    Interp(usize),
    Compiled(usize),
    Inlined(usize, usize),
    OsrInterp(usize),
    OsrInlined(usize, usize),
}

/// The label validity screen of the layout ([`split_frame_label`] is
/// `Some`), remembered for the last two labels that passed, by the address
/// and length of the shared string (TF17-1). Sound while the walk that owns
/// the labels is alive: an `Arc<str>` allocation cannot be freed and reused
/// for other text while a clone of it is held, so equal identity is equal
/// text. A label that fails is never remembered.
#[derive(Default)]
pub(in crate::runtime::stackwalker) struct LabelSplitMemo {
    pub(in crate::runtime::stackwalker) ways: [(usize, usize); 2],
    pub(in crate::runtime::stackwalker) next: usize,
}

impl LabelSplitMemo {
    pub(in crate::runtime::stackwalker) fn splits(&mut self, label: &Arc<str>) -> bool {
        let key = (label.as_ptr() as usize, label.len());
        if key.0 != 0 && self.ways.contains(&key) {
            return true;
        }
        let splits = split_frame_label(label).is_some();
        if splits && key.0 != 0 {
            self.ways[self.next] = key;
            self.next ^= 1;
        }
        splits
    }
}

/// [`collect_trace_slots`] (with the OSR overrides `osr_bci`, empty for every
/// walk the dedupe did not rewrite), and the origin of every slot, index for
/// index. The same slots in the same order: each compiled activation before
/// the first interpreter frame deeper than its push depth, refused with its
/// chain when its label does not split, its chain outermost-first up to the
/// first level that cannot be named; an overridden interpreter frame at the
/// override's bci, followed by the override's chain under the same rule
/// (THROWS18-1, round 19).
pub(in crate::runtime::stackwalker) fn collect_trace_slots_with_origins_osr<'a>(
    frames: &'a [Frame],
    jit: &'a [ActiveCompiledFrame],
    osr_bci: &'a OsrBciOverrides,
) -> (Vec<TraceSlot<'a>>, Vec<FusedOrigin>) {
    let has_osr = !osr_bci.is_empty();
    let mut inlined = jit.iter().map(|f| f.chain().len()).sum::<usize>();
    if has_osr {
        inlined += osr_bci
            .values()
            .map(|(_, chain)| chain.len())
            .sum::<usize>();
    }
    let capacity = frames.len() + jit.len() + inlined;
    let mut slots = Vec::with_capacity(capacity);
    let mut origins = Vec::with_capacity(capacity);
    let mut memo = LabelSplitMemo::default();
    let mut next = 0usize;
    for (i, frame) in frames.iter().enumerate() {
        while next < jit.len() && (jit[next].interp_depth as usize) <= i {
            push_compiled_slots_with_origins(&mut slots, &mut origins, jit, next, &mut memo);
            next += 1;
        }
        if let Some((bci, chain)) = if has_osr { osr_bci.get(&i) } else { None } {
            slots.push(TraceSlot::Interp { frame, bci: *bci });
            origins.push(FusedOrigin::OsrInterp(i));
            // `push_inlined_slots(.., chain, None)`, with origins.
            for (k, level) in chain.iter().enumerate().rev() {
                if level.bci >= MAX_INLINED_LEVEL_BCI || !memo.splits(&level.label) {
                    break;
                }
                slots.push(TraceSlot::Inlined(level, None));
                origins.push(FusedOrigin::OsrInlined(i, k));
            }
            continue;
        }
        slots.push(TraceSlot::Interp {
            frame,
            bci: frame.last_instr_pc.min(i32::MAX as usize) as i32,
        });
        origins.push(FusedOrigin::Interp(i));
    }
    while next < jit.len() {
        push_compiled_slots_with_origins(&mut slots, &mut origins, jit, next, &mut memo);
        next += 1;
    }
    (slots, origins)
}

/// [`push_compiled_slots`] + [`push_inlined_slots`] for `jit[j]`, recording
/// origins, with the label screen memoised.
pub(in crate::runtime::stackwalker) fn push_compiled_slots_with_origins<'a>(
    slots: &mut Vec<TraceSlot<'a>>,
    origins: &mut Vec<FusedOrigin>,
    jit: &'a [ActiveCompiledFrame],
    j: usize,
    memo: &mut LabelSplitMemo,
) {
    let f = &jit[j];
    if !memo.splits(&f.label) {
        return;
    }
    slots.push(TraceSlot::Compiled(f));
    origins.push(FusedOrigin::Compiled(j));
    for (k, level) in f.chain().iter().enumerate().rev() {
        if level.bci >= MAX_INLINED_LEVEL_BCI || !memo.splits(&level.label) {
            break;
        }
        slots.push(TraceSlot::Inlined(level, f.compile_cp_stamp));
        origins.push(FusedOrigin::Inlined(j, k));
    }
}

/// The inline chain of the activation [`build_fused_survivors_osr`] is
/// building: the walk's own levels, consumed backward (labels moved), or --
/// THROWS18-2 (round 19 wave 2; `CRATONVM_THROWABLE_SHARED_INLINE_CHAINS`) --
/// the artifact's shared chain, read in place (a retained level clones its
/// label: one reference count, where the copied chain paid one in the walk).
pub(in crate::runtime::stackwalker) enum OpenLevels {
    Owned(std::iter::Rev<std::iter::Enumerate<std::vec::IntoIter<InlinedLevel>>>),
    Shared(crate::jit::conservative_roots::SharedInlineChain),
}

impl OpenLevels {
    /// Level `k` (innermost-first index). The origins ask each chain's levels
    /// in decreasing `k`, so the owned iterator only ever moves forward.
    pub(in crate::runtime::stackwalker) fn take(&mut self, k: usize) -> Option<InlinedLevel> {
        match self {
            Self::Owned(levels) => levels
                .find(|(index, _)| *index == k)
                .map(|(_, level)| level),
            Self::Shared(chain) => chain.get(k).cloned(),
        }
    }
}

/// Build the retained frames for `kept` (a prefix of a fused layout's
/// origins), consuming the walk: what [`BacktraceFrame::from_slot`] builds for
/// each surviving slot, with a compiled frame's and an inlined level's label
/// MOVED out of the walk instead of cloned. Then the cap.
///
/// The origins name the walk's activations in increasing order, and each
/// activation's levels in decreasing `k` right after it (the layout's order),
/// so one forward pass over the walk and one backward pass over each chain
/// meet every origin; an activation or level no origin names (a refused
/// label, a trimmed frame) is dropped as the walk would have been.
///
/// Origins before `first` are outside the cap: they are passed over (the
/// walk and the chains still advanced through them) and build nothing.
///
/// THROWS18-1 (round 19): `osr_bci` are the dedupe's overrides the layout
/// used (empty when the walk was not rewritten). An overridden interpreter
/// frame and its override's levels are built exactly as
/// [`BacktraceFrame::from_slot`] builds their slots (the override's labels
/// are cloned: the map, not the walk, owns them; at most one chain per OSR'd
/// frame).
pub(in crate::runtime::stackwalker) fn build_fused_survivors_osr(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    osr_bci: &OsrBciOverrides,
    kept: &[FusedOrigin],
    first: usize,
    max_depth: usize,
) -> Vec<BacktraceFrame> {
    type OpenChain = (usize, OpenLevels, Option<u64>);
    let mut trace: Vec<BacktraceFrame> = Vec::with_capacity(kept.len().saturating_sub(first));
    let mut walk = jit.into_iter().enumerate();
    let mut open: Option<OpenChain> = None;
    for (position, origin) in kept.iter().enumerate() {
        let emit = position >= first;
        match *origin {
            FusedOrigin::OsrInterp(_) | FusedOrigin::OsrInlined(..) if !emit => {}
            FusedOrigin::OsrInterp(i) => {
                let frame = frames.get(i);
                let bci = osr_bci.get(&i).map(|(bci, _)| *bci);
                debug_assert!(
                    frame.is_some() && bci.is_some(),
                    "a fused OSR origin names a missing frame or override"
                );
                let (Some(frame), Some(bci)) = (frame, bci) else {
                    continue;
                };
                if let Some(built) = BacktraceFrame::from_slot(&TraceSlot::Interp { frame, bci }) {
                    trace.push(built);
                }
            }
            FusedOrigin::OsrInlined(i, k) => {
                let level = osr_bci.get(&i).and_then(|(_, chain)| chain.get(k));
                debug_assert!(
                    level.is_some(),
                    "a fused OSR inlined origin names a missing level"
                );
                let Some(level) = level else {
                    continue;
                };
                if let Some(built) = BacktraceFrame::from_slot(&TraceSlot::Inlined(level, None)) {
                    trace.push(built);
                }
            }
            FusedOrigin::Interp(_) if !emit => {}
            FusedOrigin::Interp(i) => {
                let frame = frames.get(i);
                debug_assert!(frame.is_some(), "a fused origin names a missing frame");
                let Some(frame) = frame else {
                    continue;
                };
                let slot = TraceSlot::Interp {
                    frame,
                    bci: frame.last_instr_pc.min(i32::MAX as usize) as i32,
                };
                if let Some(built) = BacktraceFrame::from_slot(&slot) {
                    trace.push(built);
                }
            }
            FusedOrigin::Compiled(j) => {
                let f = walk.by_ref().find_map(|(at, f)| (at == j).then_some(f));
                debug_assert!(f.is_some(), "fused origins out of walk order");
                let Some(f) = f else {
                    continue;
                };
                let ActiveCompiledFrame {
                    label,
                    owner_class_id,
                    bci,
                    compile_cp_stamp,
                    inline_chain,
                    inline_chain_shared,
                    ..
                } = f;
                if emit {
                    trace.push(BacktraceFrame::Compiled {
                        label,
                        owner_class_id,
                        bci,
                        cp_stamp: compile_cp_stamp,
                    });
                }
                // THROWS18-2 (round 19 wave 2): a shared chain is read in
                // place; a retained level clones its label from it.
                let levels = match inline_chain_shared {
                    Some(shared) => OpenLevels::Shared(shared),
                    None => OpenLevels::Owned(inline_chain.into_iter().enumerate().rev()),
                };
                open = Some((j, levels, compile_cp_stamp));
            }
            FusedOrigin::Inlined(j, k) => {
                let level = match open.as_mut() {
                    Some((at, levels, cp_stamp)) if *at == j => {
                        levels.take(k).map(|level| (level, *cp_stamp))
                    }
                    _ => None,
                };
                debug_assert!(
                    level.is_some(),
                    "a fused inlined origin outside its activation"
                );
                let Some((level, cp_stamp)) = level else {
                    continue;
                };
                // `from_slot`'s bound; the layout already stopped there.
                if !emit || level.bci >= MAX_INLINED_LEVEL_BCI {
                    continue;
                }
                trace.push(BacktraceFrame::Inlined {
                    label: level.label,
                    class_id: level.class_id,
                    bci: level.bci,
                    cp_stamp,
                    hidden: level.hidden,
                });
            }
        }
    }
    cap_throwable_trace(&mut trace, max_depth);
    trace
}

/// The body of both capture doors above; `T` only decides what one surviving
/// slot is built into. `splices` (a reflective call's JDK frames) turn off
/// the bounded window, whose slots would not start at frame 0.
pub(in crate::runtime::stackwalker) fn capture_throwable_slots<T: FromTraceSlot>(
    frames: &[Frame],
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<T> {
    // An armed trap snapshot stands in for the compiled-frame walk: see
    // `arm_trap_capture`.
    let jit = throwable_capture_compiled_frames(frames.len(), max_depth);
    capture_throwable_slots_with(frames, jit, max_depth, is_a, splices)
}

/// [`capture_throwable_slots`] after the compiled-frame walk.
pub(in crate::runtime::stackwalker) fn capture_throwable_slots_with<T: FromTraceSlot>(
    frames: &[Frame],
    jit: Vec<ActiveCompiledFrame>,
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<T> {
    let (jit, osr_bci) = if jit.is_empty() {
        let start = frames
            .len()
            .saturating_sub(throwable_capture_budget(max_depth));
        // Round 15 wave 3 (lane trace4): splices whose calls stand BELOW the
        // window (every started thread's bottom `Thread.run` row,
        // `ServedThreadRunCalls`) keep it. Without compiled frames a call at
        // interpreter depth `d` goes right before slot `d`, outside the window
        // when `d < start`, and a windowed answer (`keep >= max_depth`) is the
        // innermost `max_depth` slots, which the full capture's cap keeps too.
        let splices_below_window = splices
            .iter()
            .all(|s| usize::try_from(s.interp_depth).is_ok_and(|depth| depth < start));
        if start > 0 && splices_below_window {
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
    trace_from_compiled_layout(frames, &jit, &osr_bci, max_depth, is_a, splices)
}

/// The tail of [`capture_throwable_slots_with`] once the compiled frames are
/// laid out (deduped, with their OSR overrides): slots, the fill-frame trim,
/// the survivors built, the cap.
pub(in crate::runtime::stackwalker) fn trace_from_compiled_layout<T: FromTraceSlot>(
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
) -> Vec<T> {
    trace_from_compiled_layout_with(
        frames,
        jit,
        osr_bci,
        max_depth,
        is_a,
        splices,
        plain_cap_build_enabled(),
    )
}

/// JIT round 19 proposal wave (lane throws19, TF17-6):
/// `CRATONVM_THROWABLE_PLAIN_CAP_BUILD` (default ON since JIT round 19, kill
/// word `0`: R17ThrowfillDepth depth-1024 80 -> 63 us, no row worse) -- the
/// plain capture door builds only the survivors a capped trace keeps
/// ([`build_capped_survivors`]). Read once, like the door's other switches
/// ([`compact_compiled_backtrace_enabled`]); the door has no VM to latch it on.
pub(in crate::runtime::stackwalker) fn plain_cap_build_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        true
    })
}

/// TF17-6: the innermost `max_depth` survivors of `slots` (the trimmed layout,
/// OUTERMOST first), built from the inner end and stopping at the cap, in
/// layout order. Exactly `slots.iter().filter_map(T::from_slot)` followed by
/// [`cap_throwable_trace`]: a slot that builds nothing is skipped by both, and
/// the cap keeps the last `max_depth` built frames, which are the first
/// `max_depth` built counting from the inner end. A `StackOverflowError`'s
/// capture (thousands of compiled activations, 1024 kept) no longer builds
/// and then drops every frame past the cap -- a label clone and release each
/// for a compiled frame. `max_depth == 0` (uncapped) builds everything.
pub(in crate::runtime::stackwalker) fn build_capped_survivors<T: FromTraceSlot>(slots: &[TraceSlot<'_>], max_depth: usize) -> Vec<T> {
    if max_depth == 0 {
        return slots.iter().filter_map(T::from_slot).collect();
    }
    let mut trace = Vec::with_capacity(max_depth.min(slots.len()));
    for slot in slots.iter().rev() {
        if trace.len() == max_depth {
            break;
        }
        if let Some(built) = T::from_slot(slot) {
            trace.push(built);
        }
    }
    trace.reverse();
    trace
}

/// [`trace_from_compiled_layout`] with TF17-6's switch passed in (`cap_build`);
/// the same trace either way.
pub(in crate::runtime::stackwalker) fn trace_from_compiled_layout_with<T: FromTraceSlot>(
    frames: &[Frame],
    jit: &[ActiveCompiledFrame],
    osr_bci: &OsrBciOverrides,
    max_depth: usize,
    is_a: &mut dyn FnMut(Option<ClassId>, &str) -> bool,
    splices: &[ReflectiveSplice],
    cap_build: bool,
) -> Vec<T> {
    let slots = collect_trace_slots(frames, jit, osr_bci);
    let keep = throwable_fill_frame_keep_len(&slots, is_a);
    if cap_build && splices.is_empty() && max_depth != 0 {
        // TF17-6: only the survivors the cap keeps; `cap_throwable_trace`
        // would be a no-op on the result.
        return build_capped_survivors(&slots[..keep], max_depth);
    }
    let mut trace: Vec<T> = if splices.is_empty() {
        // Sized once (interpreter round i1 wave 49, lane L7): a `filter_map`
        // collect starts at four entries and regrows, copying the ~100-byte
        // frames, three or four times for an ordinary trace.
        let mut trace = Vec::with_capacity(keep);
        trace.extend(slots[..keep].iter().filter_map(T::from_slot));
        trace
    } else {
        let positions = reflective_splice_positions(&slots, splices, true);
        build_slots_with_splices(&slots, keep, splices, &positions)
    };
    cap_throwable_trace(&mut trace, max_depth);
    trace
}

/// What a [`ThrowableCaptureMemo`] answer depends on besides the frames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ThrowableCaptureKey {
    /// The class of the throwable being filled (the fill-frame trim, the
    /// stand-in screens).
    pub(crate) throwable_class: ClassId,
    /// `cratonvm_classloading::class_redefinition_count()` at the capture.
    pub(crate) redefinitions: u64,
    /// `MaxJavaStackTraceDepth`.
    pub(crate) max_depth: usize,
    /// The thread's `java.lang.Thread` object address, `0` for none (the
    /// bottom `Thread.run` rule reads its class; a moved object only misses).
    pub(crate) thread_obj: usize,
    /// Interpreter round i3 wave 73, lane L2
    /// (`CRATONVM_THROWABLE_LOADER_MISS_MEMO=1`): the served built-in loader
    /// whose miss this capture lists (`JvmThread::served_loader_miss_hint`),
    /// `None` for none. Its frames are a function of the loader's class and
    /// the throwable's (`served_loader_miss_frames`) and of the innermost
    /// frame; with the switch off a hinted capture takes no memo at all.
    pub(crate) loader_miss: Option<ClassId>,
}

/// One interpreter frame of a [`ThrowableCaptureMemo`] key: its method's
/// identity (held, so no address can be reused while the memo lives) and the
/// positions a capture reads.
pub(in crate::runtime::stackwalker) struct MemoInterpFrame {
    pub(in crate::runtime::stackwalker) method: MemoFrameMethod,
    pub(in crate::runtime::stackwalker) class_id: ClassId,
    pub(in crate::runtime::stackwalker) pc: usize,
    pub(in crate::runtime::stackwalker) last_instr_pc: usize,
}

/// The method a memoised interpreter frame runs.
pub(in crate::runtime::stackwalker) enum MemoFrameMethod {
    /// A frame pushed through the cached-invoke path: its shared record.
    Cached(Arc<crate::classloading::resolution::CachedBytecodeMethod>),
    /// An `Owned` frame (the launcher's `main`, a started thread's entry):
    /// every name a capture builds its entry from, compared by allocation.
    /// 2026-10-07: the memo used to refuse any stack holding one -- that is
    /// every stack under the launcher's `main`, so it was never made for a
    /// probe or a main-thread throw loop, and its switch could only measure
    /// its own overhead.
    Owned {
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        source_file: Option<Arc<str>>,
        replaced: Option<Arc<crate::runtime::frame::ReplacedBody>>,
    },
}

pub(in crate::runtime::stackwalker) fn same_arc_opt<T: ?Sized>(a: Option<&Arc<T>>, b: Option<&Arc<T>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

impl MemoInterpFrame {
    pub(in crate::runtime::stackwalker) fn of(frame: &Frame) -> Self {
        let method = match (frame.cached_method(), frame.owned_meta()) {
            (Some(method), _) => MemoFrameMethod::Cached(Arc::clone(method)),
            (None, Some(meta)) => MemoFrameMethod::Owned {
                class_name: Arc::clone(&meta.class_name),
                method_name: Arc::clone(&meta.method_name),
                descriptor: Arc::clone(&meta.method_descriptor),
                source_file: meta.source_file.clone(),
                replaced: meta.replaced_body.clone(),
            },
            (None, None) => unreachable!("a frame is cached or owned"),
        };
        Self {
            method,
            class_id: frame.class_id,
            pc: frame.pc,
            last_instr_pc: frame.last_instr_pc,
        }
    }

    pub(in crate::runtime::stackwalker) fn matches(&self, frame: &Frame) -> bool {
        frame.class_id == self.class_id
            && frame.last_instr_pc == self.last_instr_pc
            && frame.pc == self.pc
            && match &self.method {
                MemoFrameMethod::Cached(method) => frame
                    .cached_method()
                    .is_some_and(|m| Arc::ptr_eq(m, method)),
                MemoFrameMethod::Owned {
                    class_name,
                    method_name,
                    descriptor,
                    source_file,
                    replaced,
                } => frame.owned_meta().is_some_and(|meta| {
                    Arc::ptr_eq(&meta.class_name, class_name)
                        && Arc::ptr_eq(&meta.method_name, method_name)
                        && Arc::ptr_eq(&meta.method_descriptor, descriptor)
                        && same_arc_opt(meta.source_file.as_ref(), source_file.as_ref())
                        && same_arc_opt(meta.replaced_body.as_ref(), replaced.as_ref())
                }),
            }
    }
}

/// JIT round 16 wave 7 (lane throwpath, TP16-6): one thread's last throwable
/// capture, reused while the next one is made on the SAME stack.
///
/// A throw loop -- `new X(..)` at one site, an exception per element of a
/// parse, a retry -- captures the same frames every time, and after the walk
/// the capture lays them out as slots, trims the fill frames (under the
/// class-manager lock), builds the retained frames, runs the hidden-frame drop
/// and the stand-in screens, all to arrive at the answer it gave last time.
/// HotSpot's capture is one walk writing method ids and bcis into the
/// backtrace; this skips everything after the walk when the walk's answer and
/// the interpreter frames are the ones the memo was made from.
///
/// The memo is made only from a capture whose answer is a function of what it
/// keys (`vm_exec`'s `capture_throwable_stack_trace` decides, see
/// [`Self::new`]): no reflective or served-`Thread.run` splices, no served
/// hint, every interpreter frame a cached method's, and none of the
/// class-lookup screens (middle `Thread.run` / loader frames, native stand-in
/// frames) engaged. The bottom `Thread.run` frame is the per-thread decision
/// [`ThreadRunBottomMemo`] already memoises on less. Kept per `JvmThread`;
/// holds no heap reference.
///
/// Wave 9 (lane throwpath): wave 7's memo was never made for a throw out of
/// compiled code, the shape it was written for. Two of its refusals took
/// every such capture: a compiled innermost frame "may stand at a stand-in
/// call" (the lock-free screen answers `true` for any compiled frame; the
/// decoded answer, `compiled_innermost_gets_no_standin_frames`, is now what
/// `vm_exec` asks), and a compiled frame naming the interpreter frame it was
/// pushed from -- every capture inside an OSR'd loop, which is where a throw
/// loop runs. The OSR / call dedupe's thread-state inputs are now part of the
/// key instead ([`OsrDedupeInput`], the `osr` field).
///
/// `CRATONVM_THROWABLE_CAPTURE_MEMO`, default ON since 2026-10-07 (`=0` off;
/// `ThrowableLayoutCache::capture_memo_enabled`).
pub(crate) struct ThrowableCaptureMemo {
    pub(in crate::runtime::stackwalker) key: ThrowableCaptureKey,
    pub(in crate::runtime::stackwalker) interp: Vec<MemoInterpFrame>,
    pub(in crate::runtime::stackwalker) compiled: Vec<ActiveCompiledFrame>,
    /// What the walks that produced `compiled` read (2026-10-07): when the
    /// next capture's stack reads the same words as one of them, `compiled`
    /// is its walk too and the walk is skipped ([`Self::lookup_raw`]). The
    /// memo's own walk first, then up to [`THROWABLE_CAPTURE_MEMO_RAW_ALIASES`]
    /// more learned from walk-compared hits ([`ThrowableCaptureMemos::alias`]):
    /// one bytecode call site can have several return addresses -- an
    /// unrolled loop's copies -- that the decoded walk names alike. An
    /// unrecorded key (an armed trap snapshot's frames) is never kept.
    pub(in crate::runtime::stackwalker) raw: Vec<crate::jit::conservative_roots::RawWalkKey>,
    /// The dedupe's inputs beyond the keyed frames, one per compiled frame that
    /// names its caller, in walk order ([`osr_dedupe_inputs`]); empty for
    /// a walk outside any OSR'd loop or recursive interp->compiled call.
    pub(in crate::runtime::stackwalker) osr: Box<[OsrDedupeInput]>,
    /// Shared with every throwable a hit stores it for (nio21 wave 15 L4,
    /// `SharedVm::store_throwable_backtrace_shared`): a hit is one refcount,
    /// not a clone of every frame.
    pub(in crate::runtime::stackwalker) trace: Arc<[BacktraceFrame]>,
    /// JIT round 20 wave 12 (lane qMisc, L4W15-2 of
    /// `jit-r21-nio-proposals-w15-L4.md`; `CRATONVM_THROWABLE_CAPTURE_MEMO_SPLICES`):
    /// the reflective splices the capture laid out ([`Self::with_splices`]),
    /// held -- so no frame list's allocation can be reused while it is a key --
    /// and compared by identity ([`same_reflective_splices`]). Empty for an
    /// unspliced capture; a memo answers only a capture with the same splices,
    /// so an unspliced lookup never takes a spliced memo nor the reverse.
    pub(in crate::runtime::stackwalker) splices: Box<[ReflectiveSplice]>,
}

/// Do two captures lay out the same reflective splices: the same calls (the
/// interpreter depth and JIT entry-chain length each was entered at) with the
/// same frame lists, by allocation (a memo slot is replaced, never edited:
/// [`reflective_splices`], [`ThrowableCaptureMemos::reflective_raise_built`])?
/// JIT round 20 wave 12 (lane qMisc, L4W15-2).
pub(in crate::runtime::stackwalker) fn same_reflective_splices(a: &[ReflectiveSplice], b: &[ReflectiveSplice]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.interp_depth == y.interp_depth
                && x.jit_depth == y.jit_depth
                && Arc::ptr_eq(&x.entries, &y.entries)
                && arc_slices_same(&x.raise, &y.raise)
        })
}

/// What [`drop_osr_continuations`] reads, for one compiled frame naming its
/// caller, beyond the frames a [`ThrowableCaptureMemo`] already compares: the
/// thread's OSR registry answer for that frame (`live`), and -- for its
/// heuristic arm -- whether the artifact can OSR-enter at the caller's pc and
/// whether the caller is suspended at an invoke. Recorded whole, whichever
/// arm the dedupe takes, so two equal records mean the same decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct OsrDedupeInput {
    pub(in crate::runtime::stackwalker) live: Option<usize>,
    pub(in crate::runtime::stackwalker) can_osr_enter: bool,
    pub(in crate::runtime::stackwalker) suspended_at_invoke: bool,
}

/// [`OsrDedupeInput`] of every compiled frame of `compiled` naming its caller
/// in `frames`, in walk order. `live` answers the thread's OSR registry for
/// an interpreter frame index (`jit_bridge::live_osr_continuation_artifact`
/// in the VM; a stub in tests). `can_osr_enter` is read only for a frame with
/// an artifact pointer and no registry answer, as the dedupe reads it.
pub(in crate::runtime::stackwalker) fn osr_dedupe_inputs(
    frames: &[Frame],
    compiled: &[ActiveCompiledFrame],
    live: &mut dyn FnMut(usize) -> Option<usize>,
) -> Vec<OsrDedupeInput> {
    let mut out = Vec::new();
    for f in compiled {
        if !compiled_frame_names_its_caller(frames, f) {
            continue;
        }
        // `compiled_frame_names_its_caller` proved the index is in range.
        let i = f.interp_depth as usize - 1;
        let frame = &frames[i];
        let live_cm = live(i);
        let can_osr_enter = live_cm.is_none() && f.cm_ptr != 0 && {
            // SAFETY: as in `drop_osr_continuations`: the pointer came from a
            // chain entry whose frame is live on this thread's stack.
            let cm = unsafe { &*(f.cm_ptr as *const cratonvm_jit::CompiledMethod) };
            cm.can_osr_enter(frame.pc)
        };
        out.push(OsrDedupeInput {
            live: live_cm,
            can_osr_enter,
            suspended_at_invoke: frame_is_suspended_at_invoke(frame),
        });
    }
    out
}

/// [`osr_dedupe_inputs`] against this thread's OSR registry.
pub(in crate::runtime::stackwalker) fn live_osr_dedupe_inputs(
    frames: &[Frame],
    compiled: &[ActiveCompiledFrame],
) -> Vec<OsrDedupeInput> {
    osr_dedupe_inputs(frames, compiled, &mut |i| {
        crate::runtime::interpreter::jit_bridge::live_osr_continuation_artifact(i)
    })
}

impl ThrowableCaptureMemo {
    /// The deepest interpreter stack a memo is made for: comparing the frames
    /// must stay cheaper than the capture it skips, and a deep stack's
    /// capture is the bounded window anyway.
    pub(crate) const MAX_FRAMES: usize = 256;

    /// The memo of a capture of `frames` with compiled frames `compiled` that
    /// produced `trace`, or `None` when the stack is not one a memo is kept
    /// for: too deep, or an interpreter frame with no cached method.
    pub(crate) fn new(
        key: ThrowableCaptureKey,
        frames: &[Frame],
        compiled: Vec<ActiveCompiledFrame>,
        raw: crate::jit::conservative_roots::RawWalkKey,
        trace: &[BacktraceFrame],
    ) -> Option<Self> {
        if !throwable_capture_memoisable(frames, &compiled) {
            return None;
        }
        Self::new_shared(key, frames, compiled, raw, Arc::from(trace))
    }

    /// [`Self::new`] keeping `trace` itself, which the capture that made it
    /// also stores (nio21 wave 15 L4): one allocation for both.
    pub(crate) fn new_shared(
        key: ThrowableCaptureKey,
        frames: &[Frame],
        compiled: Vec<ActiveCompiledFrame>,
        raw: crate::jit::conservative_roots::RawWalkKey,
        trace: Arc<[BacktraceFrame]>,
    ) -> Option<Self> {
        if !throwable_capture_memoisable(frames, &compiled) {
            return None;
        }
        let interp = frames.iter().map(MemoInterpFrame::of).collect::<Vec<_>>();
        let osr = live_osr_dedupe_inputs(frames, &compiled).into_boxed_slice();
        Some(Self {
            key,
            interp,
            compiled,
            raw: if raw.is_recorded() {
                vec![raw]
            } else {
                Vec::new()
            },
            osr,
            trace,
            splices: Box::default(),
        })
    }

    /// The memo of a capture that laid out `splices` (JIT round 20 wave 12,
    /// lane qMisc, L4W15-2): its lookups then answer only a capture laying out
    /// the same ones ([`same_reflective_splices`]).
    pub(crate) fn with_splices(mut self, splices: &[ReflectiveSplice]) -> Self {
        self.splices = splices.into();
        self
    }

    /// [`Self::lookup`] without the walk: the memoised trace when the
    /// interpreter frames match and the compiled-frame walk would read what
    /// the memo's walk read ([`crate::jit::conservative_roots::raw_walk_key_matches`]),
    /// so its walk is `compiled` exactly.
    pub(crate) fn lookup_raw(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
    ) -> Option<Arc<[BacktraceFrame]>> {
        self.lookup_raw_spliced(key, frames, &[])
    }

    /// [`Self::lookup_raw`] for a capture laying out `splices` (JIT round 20
    /// wave 12, lane qMisc, L4W15-2).
    pub(crate) fn lookup_raw_spliced(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
        splices: &[ReflectiveSplice],
    ) -> Option<Arc<[BacktraceFrame]>> {
        if self.raw.is_empty()
            || self.key != *key
            || !same_reflective_splices(&self.splices, splices)
            || self.interp.len() != frames.len()
            || !self.interp.iter().zip(frames).all(|(k, f)| k.matches(f))
        {
            return None;
        }
        if !self
            .raw
            .iter()
            .any(crate::jit::conservative_roots::raw_walk_key_matches)
        {
            if raw_mismatch_report_due() {
                if let Some(why) =
                    crate::jit::conservative_roots::raw_walk_key_mismatch(&self.raw[0])
                {
                    eprintln!(
                        "[capture-memo] raw miss: {why} (+{} aliases)",
                        self.raw.len() - 1
                    );
                }
            }
            return None;
        }
        if !self.osr.is_empty() && *self.osr != *live_osr_dedupe_inputs(frames, &self.compiled) {
            return None;
        }
        Some(Arc::clone(&self.trace))
    }

    /// The memoised trace when the capture about to be made is the one the
    /// memo was made from.
    pub(crate) fn lookup(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
        compiled: &[ActiveCompiledFrame],
    ) -> Option<Arc<[BacktraceFrame]>> {
        self.lookup_spliced(key, frames, compiled, &[])
    }

    /// [`Self::lookup`] for a capture laying out `splices` (JIT round 20 wave
    /// 12, lane qMisc, L4W15-2).
    pub(crate) fn lookup_spliced(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
        compiled: &[ActiveCompiledFrame],
        splices: &[ReflectiveSplice],
    ) -> Option<Arc<[BacktraceFrame]>> {
        if !same_reflective_splices(&self.splices, splices)
            || !self.same_stack(key, frames, compiled)
        {
            return None;
        }
        // Equal frames name their callers alike, so the same compiled frames
        // have dedupe inputs now; only their thread-state half can differ.
        if !self.osr.is_empty() && *self.osr != *live_osr_dedupe_inputs(frames, compiled) {
            return None;
        }
        Some(Arc::clone(&self.trace))
    }

    /// [`Self::lookup`]'s frame compare, without the OSR registry.
    pub(in crate::runtime::stackwalker) fn same_stack(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
        compiled: &[ActiveCompiledFrame],
    ) -> bool {
        self.key == *key
            && self.interp.len() == frames.len()
            && self.compiled.len() == compiled.len()
            && self.interp.iter().zip(frames).all(|(k, f)| k.matches(f))
            && self
                .compiled
                .iter()
                .zip(compiled)
                .all(|(a, b)| same_compiled_frame(a, b))
    }
}

/// `CRATONVM_DBG_CAPTURE_MEMO=1`: print this raw-key miss? The first 16 per
/// thread (debug only).
pub(in crate::runtime::stackwalker) fn raw_mismatch_report_due() -> bool {
    thread_local! {
        /// Reports made on this thread; `u32::MAX` once the flag read off.
        static REPORTED: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    }
    REPORTED.with(|r| {
        let n = r.get().unwrap_or_else(|| {
            if false {
                0            } else {
                u32::MAX            }        });
        r.set(Some(n.saturating_add(1)));
        n < 16
    })
}

/// The ways of a thread's [`ThrowableCaptureMemo`]s (2026-10-07): a throw
/// loop that alternates a few sites -- two `throw` statements in one method, a
/// parse failing at two places -- keeps every site's memo.
pub(in crate::runtime::stackwalker) const THROWABLE_CAPTURE_MEMO_WAYS: usize = 4;

/// Raw walk keys a [`ThrowableCaptureMemo`] keeps besides its own: an IR
/// body unrolls a throw loop four ways by default, and each copy of the call
/// returns to its own address.
pub(in crate::runtime::stackwalker) const THROWABLE_CAPTURE_MEMO_RAW_ALIASES: usize = 8;

/// One thread's recent [`ThrowableCaptureMemo`]s, replaced round robin.
///
/// A capture that takes the memo path pays for it when it misses: the walk is
/// recorded, the plain door lays the trace out (the fused one consumes the
/// walk the memo keeps), and a memo is built. A thread whose captures do not
/// repeat -- or cannot be memoised (an interpreted innermost frame, a refill
/// at a non-constructor call) -- would pay that on every throw, so the memo
/// path backs off: a window of [`THROWABLE_CAPTURE_MEMO_WINDOW`] attempts with
/// fewer than one hit in eight turns it off for the next
/// [`THROWABLE_CAPTURE_MEMO_BACKOFF`] captures on this thread, then it tries
/// again. Which path a capture takes never changes its trace.
///
/// Boxed on first use: the field lives in `JvmThread`, whose layout the
/// dispatch loop's ratchet freezes (`layout_ratchet_tests`), and a thread that
/// never throws needs none of it.
#[derive(Default)]
pub(crate) struct ThrowableCaptureMemos(pub(in crate::runtime::stackwalker) Option<Box<ThrowableCaptureMemoWays>>);

/// The state behind [`ThrowableCaptureMemos`].
#[derive(Default)]
pub(in crate::runtime::stackwalker) struct ThrowableCaptureMemoWays {
    pub(in crate::runtime::stackwalker) ways: [Option<Box<ThrowableCaptureMemo>>; THROWABLE_CAPTURE_MEMO_WAYS],
    pub(in crate::runtime::stackwalker) next: usize,
    /// Attempts and hits in the current window.
    pub(in crate::runtime::stackwalker) window_attempts: u32,
    pub(in crate::runtime::stackwalker) window_hits: u32,
    /// Captures left to skip the memo path for.
    pub(in crate::runtime::stackwalker) backoff: u32,
    /// nio21 wave 15 L4: the compiled-handler sinks' recent
    /// `handler_resume_needs_precise_locals` answers
    /// ([`ThrowableCaptureMemos::handler_precise_locals`]), round robin. The
    /// record is held, so its address cannot be reused while it is a key.
    pub(in crate::runtime::stackwalker) precise_locals: [Option<(
        Arc<crate::classloading::resolution::CachedBytecodeMethod>,
        bool,
    )>; PRECISE_LOCALS_MEMO_WAYS],
    pub(in crate::runtime::stackwalker) precise_locals_next: usize,
    /// `CRATONVM_HANDLER_PRECISE_LOCALS_MEMO`, read on first use.
    pub(in crate::runtime::stackwalker) precise_locals_memo_on: Option<bool>,
    /// nio21 wave 15 L4: the recent reflective raise sites found
    /// ([`ThrowableCaptureMemos::reflective_raise_site`]); nio21 wave 17 L1:
    /// round robin over [`REFLECTIVE_RAISE_MEMO_WAYS`], absent sites included.
    pub(in crate::runtime::stackwalker) reflective_raise: [Option<ReflectiveRaiseSiteMemo>; REFLECTIVE_RAISE_MEMO_WAYS],
    pub(in crate::runtime::stackwalker) reflective_raise_next: usize,
    /// `CRATONVM_REFLECTIVE_RAISE_SITE_MEMO`, read on first use.
    pub(in crate::runtime::stackwalker) reflective_raise_memo_on: Option<bool>,
    /// `CRATONVM_REFLECTIVE_RAISE_SITE_ABSENT_MEMO`, read on first use.
    pub(in crate::runtime::stackwalker) reflective_raise_absent_on: Option<bool>,
    /// `CRATONVM_THROWABLE_MEMO_BORROWS_TRAP_SNAPSHOT`, read on first use.
    pub(in crate::runtime::stackwalker) borrows_trap_snapshot_on: Option<bool>,
    /// JIT round 20 wave 12 (lane qMisc, L4W15-2): the reflective raise frame
    /// lists recently built ([`ThrowableCaptureMemos::reflective_raise_built`]),
    /// round robin, so a capture memo keyed by the raise's identity can hit.
    pub(in crate::runtime::stackwalker) raise_built: [Option<ReflectiveRaiseBuiltMemo>; REFLECTIVE_RAISE_MEMO_WAYS],
    pub(in crate::runtime::stackwalker) raise_built_next: usize,
    /// `CRATONVM_THROWABLE_CAPTURE_MEMO_SPLICES`, read on first use.
    pub(in crate::runtime::stackwalker) capture_memo_splices_on: Option<bool>,
}

/// One [`reflective_raise_entries`] answer and what it was a function of:
/// the call's frames (held, compared by allocation), the raise site and the
/// class-redefinition count the site was found at. JIT round 20 wave 12,
/// lane qMisc (L4W15-2).
pub(in crate::runtime::stackwalker) struct ReflectiveRaiseBuiltMemo {
    pub(in crate::runtime::stackwalker) entries: Arc<[StackTraceEntry]>,
    pub(in crate::runtime::stackwalker) site: (ClassId, usize, usize),
    pub(in crate::runtime::stackwalker) redefinitions: u64,
    pub(in crate::runtime::stackwalker) built: Arc<[StackTraceEntry]>,
}

/// One [`reflective_raise_site_in`] answer and what it was a function of.
pub(in crate::runtime::stackwalker) struct ReflectiveRaiseSiteMemo {
    pub(in crate::runtime::stackwalker) kind: cratonvm_native_api::ReflectiveCallKind,
    pub(in crate::runtime::stackwalker) throwable_class: Box<str>,
    pub(in crate::runtime::stackwalker) cause: ReflectiveRaiseCause,
    /// `cratonvm_classloading::class_redefinition_count()` when it was found:
    /// a redefined accessor has other pcs.
    pub(in crate::runtime::stackwalker) redefinitions: u64,
    /// `None` for a remembered [`RaiseSiteLookup::Absent`].
    pub(in crate::runtime::stackwalker) site: Option<(ClassId, usize, usize)>,
}

/// Ways of [`ThrowableCaptureMemoWays::reflective_raise`] (nio21 wave 17 L1).
pub(in crate::runtime::stackwalker) const REFLECTIVE_RAISE_MEMO_WAYS: usize = 4;

/// What [`reflective_raise_site_in`] found (nio21 wave 17 L1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RaiseSiteLookup {
    /// The accessor method's `new throwable_class` the cause picks, as
    /// `(class id, method index, pc)`.
    Found((ClassId, usize, usize)),
    /// The accessor class and method are loaded and their code decoded, and
    /// it has no `new throwable_class`: as stable as a found site.
    Absent,
    /// The accessor class or method is not there (yet), or its code did not
    /// decode: never remembered.
    Unknown,
}

impl RaiseSiteLookup {
    /// The site, `None` for both other answers.
    pub(crate) fn site(self) -> Option<(ClassId, usize, usize)> {
        match self {
            Self::Found(site) => Some(site),
            Self::Absent | Self::Unknown => None,
        }
    }
}

/// Ways of [`ThrowableCaptureMemoWays::precise_locals`].
pub(in crate::runtime::stackwalker) const PRECISE_LOCALS_MEMO_WAYS: usize = 4;

/// Memo-path attempts per back-off decision ([`ThrowableCaptureMemos`]).
pub(in crate::runtime::stackwalker) const THROWABLE_CAPTURE_MEMO_WINDOW: u32 = 256;
/// Captures that skip the memo path after an unproductive window.
pub(in crate::runtime::stackwalker) const THROWABLE_CAPTURE_MEMO_BACKOFF: u32 = 4096;

impl ThrowableCaptureMemos {
    pub(in crate::runtime::stackwalker) fn state(&mut self) -> &mut ThrowableCaptureMemoWays {
        self.0.get_or_insert_with(Default::default)
    }

    /// Should this capture take the memo path? `false` while backing off
    /// (each call counts one capture off the back-off).
    pub(crate) fn attempt(&mut self) -> bool {
        self.state().attempt()
    }

    /// The outcome of a memo-path capture [`Self::attempt`] admitted.
    pub(crate) fn note_outcome(&mut self, hit: bool) {
        self.state().note_outcome(hit);
    }

    /// The first way's [`ThrowableCaptureMemo::lookup_raw`] answer.
    pub(crate) fn lookup_raw(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
    ) -> Option<Arc<[BacktraceFrame]>> {
        self.lookup_raw_spliced(key, frames, &[])
    }

    /// [`Self::lookup_raw`] for a capture laying out `splices` (JIT round 20
    /// wave 12, lane qMisc, L4W15-2).
    pub(crate) fn lookup_raw_spliced(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
        splices: &[ReflectiveSplice],
    ) -> Option<Arc<[BacktraceFrame]>> {
        self.0.as_deref()?.lookup_raw(key, frames, splices)
    }

    /// The first way's [`ThrowableCaptureMemo::lookup`] answer, and the way.
    pub(crate) fn lookup(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
        compiled: &[ActiveCompiledFrame],
    ) -> Option<(usize, Arc<[BacktraceFrame]>)> {
        self.lookup_spliced(key, frames, compiled, &[])
    }

    /// [`Self::lookup`] for a capture laying out `splices` (JIT round 20 wave
    /// 12, lane qMisc, L4W15-2).
    pub(crate) fn lookup_spliced(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
        compiled: &[ActiveCompiledFrame],
        splices: &[ReflectiveSplice],
    ) -> Option<(usize, Arc<[BacktraceFrame]>)> {
        self.0.as_deref()?.lookup(key, frames, compiled, splices)
    }

    /// Way `way` answered a capture whose walk (recorded as `raw`) it compared
    /// equal: keep `raw` as one more key of that memo, so the next capture
    /// reading the same words skips the walk.
    pub(crate) fn alias(&mut self, way: usize, raw: crate::jit::conservative_roots::RawWalkKey) {
        if let Some(state) = self.0.as_deref_mut() {
            state.alias(way, raw);
        }
    }

    /// Keep `memo`, replacing the oldest way.
    pub(crate) fn remember(&mut self, memo: ThrowableCaptureMemo) {
        self.state().remember(memo);
    }

    /// `compute()` (a [`reflective_raise_site_in`] answer) for a reflective
    /// call of `kind` raising `throwable_class` with `cause`, remembered per
    /// thread under the class-redefinition count (nio21 wave 15 L4).
    ///
    /// nio21 wave 17 L1: [`REFLECTIVE_RAISE_MEMO_WAYS`] answers are kept
    /// (round robin), and a definitive "this accessor has no such site"
    /// ([`RaiseSiteLookup::Absent`]) is kept too
    /// (`CRATONVM_REFLECTIVE_RAISE_SITE_ABSENT_MEMO`, default ON). The w16
    /// `ite` profile spent 2.6% in [`reflective_raise_site_in`] plus the
    /// bytecode decode: a COMPILED reflective target pushes no interpreter
    /// frame, so the throwable it raises itself (`IllegalStateException`)
    /// passes `reflective_raise_site`'s screen, and the accessor's whole
    /// `invoke` was decoded on every call to find no `new
    /// IllegalStateException` in it. An absent site is a function of exactly
    /// what a found one is (the accessor's loaded bytes, under the same
    /// redefinition count); only [`RaiseSiteLookup::Unknown`] -- the accessor
    /// class or method not there yet -- stays unremembered.
    pub(in crate::runtime::stackwalker) fn reflective_raise_site(
        &mut self,
        kind: cratonvm_native_api::ReflectiveCallKind,
        throwable_class: &str,
        cause: ReflectiveRaiseCause,
        compute: impl FnOnce() -> RaiseSiteLookup,
    ) -> Option<(ClassId, usize, usize)> {
        let state = self.state();
        let on = *state.reflective_raise_memo_on.get_or_insert_with(|| {
            true
        });
        if !on {
            return compute().site();
        }
        let redefinitions = cratonvm_classloading::class_redefinition_count();
        for m in state.reflective_raise.iter().flatten() {
            if m.kind == kind
                && m.cause == cause
                && m.redefinitions == redefinitions
                && &*m.throwable_class == throwable_class
            {
                return m.site;
            }
        }
        let lookup = compute();
        let keep = match lookup {
            RaiseSiteLookup::Found(_) => true,
            RaiseSiteLookup::Absent => *state.reflective_raise_absent_on.get_or_insert_with(|| {
                true
            }),
            // The accessor class may simply not be loaded yet, and loading it
            // moves no redefinition count.
            RaiseSiteLookup::Unknown => false,
        };
        let site = lookup.site();
        if !keep {
            return site;
        }
        let way = state.reflective_raise_next % REFLECTIVE_RAISE_MEMO_WAYS;
        state.reflective_raise_next = (way + 1) % REFLECTIVE_RAISE_MEMO_WAYS;
        state.reflective_raise[way] = Some(ReflectiveRaiseSiteMemo {
            kind,
            throwable_class: Box::from(throwable_class),
            cause,
            redefinitions,
            site,
        });
        site
    }

    /// `build()` (the raise frames [`reflective_raise_entries`] lays out for
    /// a call's frames `entries` at the raise `site`), the SAME allocation as
    /// the last time while the call's frame list is the same allocation, the
    /// site is the same and no class was redefined since: the answer is a
    /// function of exactly those, and the capture memo keys a spliced capture
    /// by the raise's identity ([`same_reflective_splices`]), so a fresh list
    /// per throw would never hit. JIT round 20 wave 12 (lane qMisc, L4W15-2;
    /// `CRATONVM_THROWABLE_CAPTURE_MEMO_SPLICES=0`: built every time).
    pub(in crate::runtime::stackwalker) fn reflective_raise_built(
        &mut self,
        entries: &Arc<[StackTraceEntry]>,
        site: (ClassId, usize, usize),
        build: impl FnOnce() -> Option<Arc<[StackTraceEntry]>>,
    ) -> Option<Arc<[StackTraceEntry]>> {
        let state = self.state();
        let on = *state
            .capture_memo_splices_on
            .get_or_insert_with(capture_memo_splices_enabled);
        if !on {
            return build();
        }
        let redefinitions = cratonvm_classloading::class_redefinition_count();
        for m in state.raise_built.iter().flatten() {
            if m.site == site
                && m.redefinitions == redefinitions
                && Arc::ptr_eq(&m.entries, entries)
            {
                return Some(Arc::clone(&m.built));
            }
        }
        let built = build()?;
        let way = state.raise_built_next % REFLECTIVE_RAISE_MEMO_WAYS;
        state.raise_built_next = (way + 1) % REFLECTIVE_RAISE_MEMO_WAYS;
        state.raise_built[way] = Some(ReflectiveRaiseBuiltMemo {
            entries: Arc::clone(entries),
            site,
            redefinitions,
            built: Arc::clone(&built),
        });
        Some(built)
    }

    /// `CRATONVM_THROWABLE_MEMO_BORROWS_TRAP_SNAPSHOT` (default ON), read once
    /// per thread: may [`lookup_memo_on_armed_trap_capture`] answer?
    pub(in crate::runtime::stackwalker) fn borrows_trap_snapshot(&mut self) -> bool {
        *self
            .state()
            .borrows_trap_snapshot_on
            .get_or_insert_with(|| {
                true
            })
    }

    /// `compute()` for `method`, remembered per thread (nio21 wave 15 L4).
    ///
    /// `compute` is a pure function of the method's immutable bytecode and
    /// exception table (`cratonvm_jit::handler_resume_requires_precise_locals`,
    /// a CFG dataflow over the method plus a flag read), so the answer of the
    /// same `Arc` record is the answer. A caught-AIOOBE loop asked it on every
    /// throw: a SipHash map per handler and a whole-method store scan.
    pub(crate) fn handler_precise_locals(
        &mut self,
        method: &Arc<crate::classloading::resolution::CachedBytecodeMethod>,
        compute: impl FnOnce() -> bool,
    ) -> bool {
        let state = self.state();
        // `CRATONVM_HANDLER_PRECISE_LOCALS_MEMO=0`: ask every time. Read once
        // per thread.
        let on = *state.precise_locals_memo_on.get_or_insert_with(|| {
            true
        });
        if !on {
            return compute();
        }
        for (held, answer) in state.precise_locals.iter().flatten() {
            if Arc::ptr_eq(held, method) {
                return *answer;
            }
        }
        let answer = compute();
        let next = state.precise_locals_next;
        state.precise_locals[next] = Some((Arc::clone(method), answer));
        state.precise_locals_next = (next + 1) % PRECISE_LOCALS_MEMO_WAYS;
        answer
    }
}

impl ThrowableCaptureMemoWays {
    pub(in crate::runtime::stackwalker) fn attempt(&mut self) -> bool {
        if self.backoff > 0 {
            self.backoff -= 1;
            return false;
        }
        true
    }

    pub(in crate::runtime::stackwalker) fn note_outcome(&mut self, hit: bool) {
        self.window_attempts += 1;
        self.window_hits += u32::from(hit);
        if self.window_attempts >= THROWABLE_CAPTURE_MEMO_WINDOW {
            if self.window_hits * 8 < self.window_attempts {
                self.backoff = THROWABLE_CAPTURE_MEMO_BACKOFF;
            }
            self.window_attempts = 0;
            self.window_hits = 0;
        }
    }

    pub(in crate::runtime::stackwalker) fn lookup_raw(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
        splices: &[ReflectiveSplice],
    ) -> Option<Arc<[BacktraceFrame]>> {
        self.ways
            .iter()
            .flatten()
            .find_map(|memo| memo.lookup_raw_spliced(key, frames, splices))
    }

    pub(in crate::runtime::stackwalker) fn lookup(
        &self,
        key: &ThrowableCaptureKey,
        frames: &[Frame],
        compiled: &[ActiveCompiledFrame],
        splices: &[ReflectiveSplice],
    ) -> Option<(usize, Arc<[BacktraceFrame]>)> {
        self.ways.iter().enumerate().find_map(|(way, memo)| {
            memo.as_deref()
                .and_then(|memo| memo.lookup_spliced(key, frames, compiled, splices))
                .map(|trace| (way, trace))
        })
    }

    pub(in crate::runtime::stackwalker) fn alias(&mut self, way: usize, raw: crate::jit::conservative_roots::RawWalkKey) {
        if !raw.is_recorded() {
            return;
        }
        if let Some(Some(memo)) = self.ways.get_mut(way) {
            // JIT round 19 wave 2 (lane throws2, THROWS19-9 review): `<=`
            // is the documented bound, not an off-by-one. `raw` holds the
            // memo's own key first, so a push at `len == ALIASES` leaves the
            // own key plus `ALIASES` aliases. Only a memo made with an
            // unrecorded own key (`ThrowableCaptureMemo::new` keeps none) can hold one
            // alias more, `ALIASES + 1`; the total is bounded either way.
            if memo.raw.len() <= THROWABLE_CAPTURE_MEMO_RAW_ALIASES && !memo.raw.contains(&raw) {
                memo.raw.push(raw);
            }
        }
    }

    pub(in crate::runtime::stackwalker) fn remember(&mut self, memo: ThrowableCaptureMemo) {
        self.ways[self.next] = Some(Box::new(memo));
        self.next = (self.next + 1) % THROWABLE_CAPTURE_MEMO_WAYS;
    }
}

/// [`throwable_capture_compiled_frames_with`], recording the walk's raw key
/// into `raw` (left unrecorded for an armed trap snapshot, whose frames are
/// not the walk's). The capped capture's activation bound is the same.
pub(crate) fn throwable_capture_compiled_frames_recorded(
    depth: usize,
    walk_scoped: bool,
    max_depth: usize,
    raw: &mut crate::jit::conservative_roots::RawWalkKey,
) -> Vec<ActiveCompiledFrame> {
    if let Some(snapshot) = armed_trap_capture_frames(depth) {
        return snapshot;
    }
    crate::jit::stack_trace::active_compiled_frames_walk_recorded_opts(
        throwable_walk_activation_bound(max_depth),
        walk_scoped,
        raw,
        throwable_walk_options(),
    )
}

/// Is a trap snapshot armed for this thread's next capture? A capture memo
/// keyed on the raw walk must not answer for one (the snapshot, not the
/// walk, gives its compiled frames).
pub(crate) fn trap_capture_armed() -> bool {
    TRAP_CAPTURE.with(|t| t.borrow().is_some())
}

/// The stack shape a [`ThrowableCaptureMemo`] may be made for (see
/// [`ThrowableCaptureMemo::new`]): at most
/// [`ThrowableCaptureMemo::MAX_FRAMES`] interpreter frames, each a cached
/// method's. Wave 9: a compiled frame naming its caller no longer refuses
/// (its dedupe inputs are keyed, [`OsrDedupeInput`]); `compiled` is kept in
/// the signature for the callers that pass the walk.
pub(crate) fn throwable_capture_memoisable(
    frames: &[Frame],
    _compiled: &[ActiveCompiledFrame],
) -> bool {
    frames.len() <= ThrowableCaptureMemo::MAX_FRAMES
}

/// Do two walks' answers for one activation agree in everything a capture
/// reads? Labels compare by content (`Arc<str>`'s `==` tests the pointer
/// first), so a re-shared label is still a hit.
pub(in crate::runtime::stackwalker) fn same_compiled_frame(a: &ActiveCompiledFrame, b: &ActiveCompiledFrame) -> bool {
    a.interp_depth == b.interp_depth
        && a.chain_index == b.chain_index
        && a.owner_class_id == b.owner_class_id
        && a.cm_ptr == b.cm_ptr
        && a.bci == b.bci
        && a.chain_exact == b.chain_exact
        && a.trap_site_exact == b.trap_site_exact
        && a.chain_poisoned == b.chain_poisoned
        && a.compile_cp_stamp == b.compile_cp_stamp
        && a.label == b.label
        && a.chain().len() == b.chain().len()
        && a.chain()
            .iter()
            .zip(b.chain())
            .all(|(x, y)| x.bci == y.bci && x.class_id == y.class_id && x.label == y.label)
}

/// The trap snapshot armed for the throwable a JIT door is constructing.
/// See [`arm_trap_capture`].
pub(in crate::runtime::stackwalker) struct ArmedSnapshot {
    pub(in crate::runtime::stackwalker) frames: Vec<ActiveCompiledFrame>,
    /// `frames.len()` of the thread's interpreter stack when it was armed.
    pub(in crate::runtime::stackwalker) base_depth: usize,
    /// Did any capture take the snapshot's frames?
    pub(in crate::runtime::stackwalker) used: bool,
    /// JIT round 20 (lane throw,
    /// `r19-a-capture-nested-in-an-implicit-exception-construction-takes-the-trap-snapshot-FIXED-20261012.md`):
    /// the class of the throwable whose construction armed the window, when
    /// the door knew it ([`bind_trap_capture_owner`]). A capture of a
    /// throwable of ANOTHER class made inside the window (a
    /// `StackOverflowError` or `OutOfMemoryError` raised inside the
    /// constructor, an agent's throwable) walks its own frames
    /// ([`suspend_trap_capture_for_foreign`]). `None`: every capture takes the
    /// snapshot, as before.
    pub(in crate::runtime::stackwalker) owner_class: Option<ClassId>,
}

thread_local! {
    /// Per THREAD, like the snapshot it holds (a stack capture reads only its
    /// own thread's frames); empty outside [`arm_trap_capture`]'s window.
    pub(in crate::runtime::stackwalker) static TRAP_CAPTURE: std::cell::RefCell<Option<ArmedSnapshot>> =
        const { std::cell::RefCell::new(None) };
}

/// Round 12 wave 4 (lane exc2), `docs/feature-designs/jit-ir-optimizations.md` W3-2: ONE stack walk
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
            owner_class: None,
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
pub(in crate::runtime::stackwalker) fn armed_trap_capture_frames(depth: usize) -> Option<Vec<ActiveCompiledFrame>> {
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

/// nio21 wave 15 L4: [`ThrowableCaptureMemos::lookup`] against the armed trap
/// snapshot's frames BORROWED, for a capture on top of `frames`; `None` when
/// no snapshot is armed for it or no memo answers. A hit marks the snapshot
/// used, exactly as the clone [`armed_trap_capture_frames`] hands the walk
/// does. A compiled implicit exception (AIOOBE, NPE) always captures through
/// an armed snapshot, so the walk-free [`ThrowableCaptureMemos::lookup_raw`]
/// never serves it, and the clone (a label refcount and an inline-chain
/// allocation per compiled activation) was paid only to be compared and
/// dropped. `CRATONVM_THROWABLE_MEMO_BORROWS_TRAP_SNAPSHOT=0` restores the
/// clone.
pub(crate) fn lookup_memo_on_armed_trap_capture(
    memos: &mut ThrowableCaptureMemos,
    key: &ThrowableCaptureKey,
    frames: &[Frame],
) -> Option<Arc<[BacktraceFrame]>> {
    lookup_memo_on_armed_trap_capture_spliced(memos, key, frames, &[])
}

/// [`lookup_memo_on_armed_trap_capture`] for a capture laying out `splices`
/// (JIT round 20 wave 12, lane qMisc, L4W15-2).
pub(crate) fn lookup_memo_on_armed_trap_capture_spliced(
    memos: &mut ThrowableCaptureMemos,
    key: &ThrowableCaptureKey,
    frames: &[Frame],
    splices: &[ReflectiveSplice],
) -> Option<Arc<[BacktraceFrame]>> {
    if !memos.borrows_trap_snapshot() {
        return None;
    }
    let memos = &*memos;
    TRAP_CAPTURE.with(|c| {
        let mut slot = c.try_borrow_mut().ok()?;
        let armed = slot.as_mut()?;
        if frames.len() < armed.base_depth {
            return None;
        }
        let (_, hit) = memos.lookup_spliced(key, frames, &armed.frames, splices)?;
        armed.used = true;
        Some(hit)
    })
}

/// JIT round 20 (lane throw): name the class of the throwable the armed
/// window is building (see [`ArmedSnapshot::owner_class`]). A no-op when
/// nothing is armed on this thread.
pub(crate) fn bind_trap_capture_owner(owner_class: ClassId) {
    TRAP_CAPTURE.with(|c| {
        if let Ok(mut slot) = c.try_borrow_mut() {
            if let Some(armed) = slot.as_mut() {
                armed.owner_class = Some(owner_class);
            }
        }
    });
}

/// A capture of a throwable of a class other than the armed window's owner,
/// with the snapshot taken out of the window for its duration; dropping it
/// puts the snapshot back (unless a new window was armed meanwhile, which
/// keeps the new one: the old one's door then splices, as for any snapshot
/// no capture used).
pub(crate) struct SuspendedTrapCapture {
    pub(in crate::runtime::stackwalker) armed: Option<ArmedSnapshot>,
}

impl Drop for SuspendedTrapCapture {
    fn drop(&mut self) {
        let Some(armed) = self.armed.take() else {
            return;
        };
        TRAP_CAPTURE.with(|c| {
            if let Ok(mut slot) = c.try_borrow_mut() {
                if slot.is_none() {
                    *slot = Some(armed);
                }
            }
        });
    }
}

/// JIT round 20 (lane throw): when the armed window belongs to a throwable of
/// another class than `throwable_class` (the class of the throwable about to
/// be captured), take the snapshot out of the window until the returned guard
/// drops, so this capture walks its own compiled frames: the snapshot stands
/// only for the frames at the trap, and a throwable built INSIDE the
/// construction (a `StackOverflowError` in the constructor chain) has live
/// compiled frames above them that the snapshot does not hold. `None` when
/// nothing is armed, the owner is unknown, or this is the owner's capture.
pub(crate) fn suspend_trap_capture_for_foreign(
    throwable_class: ClassId,
) -> Option<SuspendedTrapCapture> {
    TRAP_CAPTURE.with(|c| {
        let mut slot = c.try_borrow_mut().ok()?;
        let owner = slot.as_ref()?.owner_class?;
        if owner == throwable_class {
            return None;
        }
        Some(SuspendedTrapCapture { armed: slot.take() })
    })
}

/// The window [`arm_trap_capture`] opened. Dropping it disarms, so a panic
/// inside the construction cannot leave a snapshot armed for the next,
/// unrelated capture on this thread.
pub(crate) struct ArmedTrapCapture {
    pub(in crate::runtime::stackwalker) _private: (),
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
pub(in crate::runtime::stackwalker) trait FromTraceSlot: Sized {
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
            // Round 12 wave 5 (lane exc3), `docs/feature-designs/jit-ir-optimizations.md` W4-3:
            // the label, not a built entry. `None` under exactly the refusals
            // `compiled_frame_entry_without_store` /
            // `inlined_frame_entry_without_store` apply (`split_frame_label`
            // is `parse_frame_label_uncached`'s predicate, the bci bound is the
            // same constant), so both forms keep the same frames.
            // JIT round 16 (lane throwpath, TP16-1): no second split here. A
            // `Compiled` / `Inlined` slot exists only where `push_compiled_slots`
            // / `push_inlined_slots` already split its label (and bounded the
            // inlined bci by the same constant), so the refusal this repeated
            // can never fire; asserted in debug builds.
            TraceSlot::Compiled(f) if compact_compiled_backtrace_enabled() => {
                debug_assert!(split_frame_label(&f.label).is_some());
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
                debug_assert!(split_frame_label(&level.label).is_some());
                Some(BacktraceFrame::Inlined {
                    label: Arc::clone(&level.label),
                    class_id: level.class_id,
                    bci: level.bci,
                    cp_stamp: *cp_stamp,
                    hidden: level.hidden,
                })
            }
            _ => slot.build().map(BacktraceFrame::Entry),
        }
    }
}

/// JVMS 4.9.1's `code_length` bound, as the inlined-level builders apply it.
pub(in crate::runtime::stackwalker) const MAX_INLINED_LEVEL_BCI: u32 = 65_536;

/// `CRATONVM_JIT_COMPACT_COMPILED_BACKTRACE` -- default ON; `0` retains
/// compiled and inlined frames as built [`StackTraceEntry`]s again (the
/// pre-round-12-wave-5 form). Cached: read per retained compiled frame.
pub(in crate::runtime::stackwalker) fn compact_compiled_backtrace_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        true
    })
}

/// The entry a retained compiled / inlined frame stands for when its label
/// does not parse -- unreachable, because [`FromTraceSlot::from_slot`] refuses
/// such a label, but [`BacktraceFrame::to_entry`] is infallible and must not
/// panic. Names the raw label rather than inventing a split.
pub(in crate::runtime::stackwalker) fn unparsed_label_entry(label: &Arc<str>, class_id: Option<ClassId>, bci: i32) -> StackTraceEntry {
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
/// i1 wave 34, applied by the `--real-jdk` throwable capture.
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
        // Wave 122 (L2): a level the JIT's resolver stamped answers its bit.
        BacktraceFrame::Inlined {
            hidden: Some(hidden),
            ..
        } => *hidden,
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
        // Wave 122 (L2): decided at compile time, with no class store.
        BacktraceFrame::Inlined {
            hidden: Some(hidden),
            ..
        } => Some(*hidden),
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
        // JIT round 16 wave 8 (lane trace): the indy bridge's frame
        // ([`entry_is_vm_bridge`]).
}

/// The class name of the synthetic interpreter frame the compiled
/// `invokedynamic` bridge pushes to run an instruction on
/// (`runtime::invokedynamic`: `<jit-indy>.bridge` for a generic site,
/// `<jit-indy>.concat` for a string concatenation). It names no Java class
/// (`<` cannot begin a class name javac emits), and HotSpot's frames for the
/// same work are hidden `LambdaForm` frames, so no trace lists it.
pub(crate) const JIT_INDY_BRIDGE_FRAME_CLASS: &str = "<jit-indy>";



/// Is `frame` the compiled indy bridge's synthetic frame (an `Owned` frame
/// named [`JIT_INDY_BRIDGE_FRAME_CLASS`]), with the switch on? A frame with a
/// cached method is a real method's and is decided by one load.
pub(crate) fn frame_is_vm_bridge(frame: &Frame) -> bool {
    frame.cached_method().is_none()
        && frame.is_vm_synthetic()
        && frame.class_name() == JIT_INDY_BRIDGE_FRAME_CLASS
}

/// [`frame_is_vm_bridge`] for an entry a capture already built.
pub(crate) fn entry_is_vm_bridge(entry: &StackTraceEntry) -> bool {
    entry.class_id.is_none() && &*entry.class_name == JIT_INDY_BRIDGE_FRAME_CLASS
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
    if entry_is_vm_bridge(entry) {
        return true;
    }
    entry.class_id.is_some_and(|class_id| {
        entry_may_be_hidden(&entry.class_name)
            // Interpreter round i2 wave 55, lane L3: a lambda proxy's stand-in
            // frame (`lambda_proxy_standin_frames`, a `SHOW_HIDDEN_FRAMES`
            // walk's) names a class the store does not hold; HotSpot spins it
            // as a HIDDEN class, so its frame is hidden (a `DROP_METHOD_INFO`
            // frame's ` hidden` tag; probe `L3W54StackFrameInfoShapes`, row
            // `drop-hidden`).
            && (class_id.as_u32() >= crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE
                || method_is_hidden(
                    store,
                    class_id,
                    &entry.method_name,
                    entry.method_descriptor.as_deref(),
                    entry.method_index,
                ))
    })
}

/// `Method::is_hidden` for `class_id.name desc` (any descriptor when `desc`
/// is `None`). `index`, when given, is the method's slot in the class as a
/// capture resolved it: used when that slot still names `name desc`, which
/// saves the scan; otherwise the scan decides.
pub(in crate::runtime::stackwalker) fn method_is_hidden(
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
    // Interpreter round i6 wave 118, lane L3 (proposal
    // `i37-L6-proposal-hidden-method-bit-at-link-time`, steps 1, 2 and 4;
    // `CRATONVM_HIDDEN_METHOD_BITS`, default off): the class's bit set, kept
    // by its store from the first ask until the class changes
    // (`ClassStore::hidden_method_bits`), answers by slot: no annotation
    // decode per frame per capture. The same rule as below
    // ([`hidden_method_bits_of`]).
    if let Some(bits) = store.hidden_method_bits(class_id, hidden_method_bits_of) {
        let bit = |i: usize| bits.get(i / 64).is_some_and(|w| (w >> (i % 64)) & 1 != 0);
        let slot = index
            .map(|i| i as usize)
            .filter(|&i| class.methods.get(i).is_some_and(|m| named(m)));
        return match slot {
            Some(i) => bit(i),
            None => class
                .methods
                .iter()
                .enumerate()
                .any(|(i, m)| named(m) && bit(i)),
        };
    }
    let annotated = match index.and_then(|i| class.methods.get(i as usize)) {
        Some(m) if named(m) => method_has_hidden_annotation(class, m),
        _ => class
            .methods
            .iter()
            .filter(|m| named(*m))
            .any(|m| method_has_hidden_annotation(class, m)),
    };
    // Interpreter round i6 wave 115, lane L3 (proposal
    // `i37-L6-proposal-hidden-method-bit-at-link-time`, its privilege half):
    // the name screen above lets through a `jdk/...` or `sun/...` class an
    // application loader defined, whose `@Hidden` HotSpot's `ClassFileParser`
    // drops (`AnnotationCollector`: honoured only when the boot or platform
    // loader defines the class). Asked only of an annotated method, so the
    // switch read is off every common path.
    // `CRATONVM_HIDDEN_ANNOTATION_PRIVILEGED_ONLY=0` restores the name screen
    // alone.
    annotated && (class_may_be_caller_sensitive(class))
}

/// The `Method::is_hidden` stamp the JIT's inline resolver puts on a spliced
/// callee (`InlineSite::hidden`, then `InlinedLevel::hidden`): `Some` only
/// when `store` keeps hidden-method bits (`CRATONVM_HIDDEN_METHOD_BITS`) and
/// holds the class, decided by [`method_is_hidden`], the rule every other
/// frame shape asks; `None` otherwise, which leaves the capture's label test
/// in charge. Interpreter round i6 wave 122, lane L2 (step 3 of
/// `i37-L6-proposal-hidden-method-bit-at-link-time`).
pub(crate) fn inline_callee_hidden_stamp(
    store: &ClassStore,
    class_id: ClassId,
    name: &str,
    desc: &str,
) -> Option<bool> {
    store.hidden_method_bits(class_id, hidden_method_bits_of)?;
    Some(method_is_hidden(store, class_id, name, Some(desc), None))
}

/// Does HotSpot compute NO extended (JEP 358) message for an NPE whose top
/// frame is `class_id.name desc`? `JVM_GetExtendedNPEMessage` answers `null`
/// when `fill_in_stack_trace` skipped a hidden top frame
/// (`java_lang_Throwable::get_top_method_and_bci`, `trace_hidden_offset`),
/// and that skip asks `Method::is_hidden`: every method of a hidden class and
/// an `@Hidden` method of a privileged class -- [`method_is_hidden`], the rule
/// the capture drops the frame by. The caller keeps the
/// `-XX:+ShowHiddenFrames` gate (HotSpot then skips nothing).
///
/// The one decision both NPE message paths ask (the interpreter's
/// `invoke::npe_frame_is_hidden`, the compiled `jit_npe_message` trap site);
/// j27 wave 5, lane NPEHIDDEN. `CRATONVM_NPE_HIDDEN_METHOD_RULE=0` restores
/// the class-bit rule; it is read only when the two rules disagree.
pub(crate) fn npe_top_frame_is_hidden(
    store: &ClassStore,
    class_id: ClassId,
    name: &str,
    desc: &str,
) -> bool {
    npe_top_frame_is_hidden_with(store, class_id, name, desc, npe_hidden_method_rule)
}

/// [`npe_top_frame_is_hidden`] with the switch read handed in (`method_rule`,
/// asked only for a visible class's `@Hidden` method).
pub(crate) fn npe_top_frame_is_hidden_with(
    store: &ClassStore,
    class_id: ClassId,
    name: &str,
    desc: &str,
    method_rule: fn() -> bool,
) -> bool {
    let Some(class) = store.get(class_id) else {
        return false;
    };
    if class.is_hidden() {
        return true;
    }
    entry_may_be_hidden(&class.name)
        && method_is_hidden(store, class_id, name, Some(desc), None)
        && method_rule()
}

/// `CRATONVM_NPE_HIDDEN_METHOD_RULE` (default ON; `0` restores the class-bit
/// rule). Not cached: read only for an NPE in an `@Hidden` JDK method.
pub(in crate::runtime::stackwalker) fn npe_hidden_method_rule() -> bool {
    true
}

/// `Method::is_hidden` for every method of `class`, one bit per
/// `Class::methods` slot: all of a hidden class's; otherwise a method carrying
/// `@jdk.internal.vm.annotation.Hidden` in a class whose name may hold hidden
/// frames ([`entry_may_be_hidden`]) and that is privileged (the boot or
/// platform loader defined it), as [`method_is_hidden`] decides one method.
/// What the class store keeps (`ClassStore::hidden_method_bits`).
/// Interpreter round i6 wave 118, lane L3.
pub(in crate::runtime::stackwalker) fn hidden_method_bits_of(class: &Class) -> Box<[u64]> {
    let mut bits = vec![0u64; class.methods.len().div_ceil(64)];
    let all = class.is_hidden();
    let may_annotate = !all
        && entry_may_be_hidden(&class.name)
        && (class_may_be_caller_sensitive(class));
    for (i, m) in class.methods.iter().enumerate() {
        if all || (may_annotate && method_has_hidden_annotation(class, m)) {
            bits[i / 64] |= 1 << (i % 64);
        }
    }
    bits.into_boxed_slice()
}



/// Can a method of `class` be `@CallerSensitive` at all? HotSpot's
/// `ClassFileParser` honours the annotation only in a privileged class: one
/// the boot or platform loader defines (`AnnotationCollector`; the same rule
/// `lang_invoke::mhn_method_is_caller_sensitive` applies). Interpreter round
/// i2 wave 55, lane L3 approximated that with a `java/`, `jdk/`, `sun/` name
/// screen, which missed the JDK's own `com/sun/...` classes and honoured a
/// `sun/...` class a user loader defined; interpreter round i4 wave 96, lane
/// L3 asks the defining loader instead.
pub(crate) fn class_may_be_caller_sensitive(class: &Class) -> bool {
    matches!(
        class.loader_id,
        cratonvm_types::ClassLoaderId::Bootstrap | cratonvm_types::ClassLoaderId::Extension
    )
}

/// Is `entry`'s method annotated `@jdk.internal.reflect.CallerSensitive`
/// (HotSpot's `Method::is_caller_sensitive`)? The entry's `method_index`,
/// when it still names the method, saves the scan; an entry without a
/// descriptor takes any overload of the name that carries it.
pub(crate) fn stack_entry_is_caller_sensitive(store: &ClassStore, entry: &StackTraceEntry) -> bool {
    let Some(class_id) = entry.class_id else {
        return false;
    };
    let Some(class) = store.get(class_id) else {
        return false;
    };
    if !class_may_be_caller_sensitive(class) {
        return false;
    }
    let desc = entry.method_descriptor.as_deref();
    let named = |m: &ClassFileMethod| {
        *m.name == *entry.method_name && desc.is_none_or(|d| &*m.descriptor == d)
    };
    let annotated = |m: &ClassFileMethod| {
        cratonvm_classloading::annotations::method_annotations(m)
            .find_by_type_descriptor("Ljdk/internal/reflect/CallerSensitive;", |i| {
                class.constant_pool.get_utf8(i)
            })
            .is_some()
    };
    if let Some(m) = entry
        .method_index
        .and_then(|i| class.methods.get(i as usize))
    {
        if named(m) {
            return annotated(m);
        }
    }
    class.methods.iter().filter(|m| named(m)).any(annotated)
}

/// The descriptor of the caller-sensitive adapter of the static
/// `@CallerSensitive` method `(name, descriptor)` of class `class_id`, as
/// HotSpot 25's `MethodHandleAccessorFactory.findCSMethodAdapter` finds it:
/// a declared method of the same name whose parameters are the method's plus
/// a trailing `java.lang.Class` (the JDK marks it `@CallerSensitiveAdapter`,
/// a class-retention annotation the lookup does not read), here also static
/// with the same return type (interpreter round i2 wave 61, lane L3). `None`
/// for any other method, or when the class declares no such adapter.
pub(crate) fn caller_sensitive_adapter_descriptor(
    store: &ClassStore,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<String> {
    let class = store.get(class_id)?;
    if !class_may_be_caller_sensitive(class) {
        return None;
    }
    let target = class.find_method(name, descriptor)?;
    let caller_sensitive = cratonvm_classloading::annotations::method_annotations(target)
        .find_by_type_descriptor("Ljdk/internal/reflect/CallerSensitive;", |i| {
            class.constant_pool.get_utf8(i)
        })
        .is_some();
    if !target.is_static() || !caller_sensitive {
        return None;
    }
    let close = descriptor.find(')')?;
    let adapter_descriptor = format!(
        "{}Ljava/lang/Class;{}",
        &descriptor[..close],
        &descriptor[close..]
    );
    let adapter = class.find_method(name, &adapter_descriptor)?;
    adapter.is_static().then_some(adapter_descriptor)
}

/// Is `method` (one of `class`'s) annotated `@jdk.internal.vm.annotation.Hidden`?
pub(in crate::runtime::stackwalker) fn method_has_hidden_annotation<'a>(class: &'a Class, method: &'a ClassFileMethod) -> bool {
    cratonvm_classloading::annotations::method_annotations(method)
        .find_by_type_descriptor("Ljdk/internal/vm/annotation/Hidden;", |i| {
            class.constant_pool.get_utf8(i)
        })
        .is_some()}

/// One frame of a throwable's RETAINED backtrace (the VM's registry,
/// `SharedVm::store_throwable_backtrace`), materialised into a
/// [`StackTraceEntry`] only when Java reads the trace.
///
/// Stage 1a of `docs/feature-designs/real-jdk-mode.md`.
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
        /// `InlinedLevel::hidden`: the callee's `Method::is_hidden` as the
        /// JIT's resolver stamped it at compile time (interpreter round i6
        /// wave 122, lane L2, step 3 of `i37-L6-proposal-hidden-method-bit-at-link-time`,
        /// `CRATONVM_HIDDEN_METHOD_BITS`). `Some` decides
        /// [`backtrace_frame_hidden_decided`] with no class store and no
        /// label test; `None` (not stamped) judges the label as before.
        hidden: Option<bool>,
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
            BacktraceFrame::Compiled { cp_stamp, .. }
            | BacktraceFrame::Inlined { cp_stamp, .. } => *cp_stamp,
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
pub(in crate::runtime::stackwalker) enum TraceSlot<'a> {
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
    pub(in crate::runtime::stackwalker) fn build(&self) -> Option<StackTraceEntry> {
        match self {
            // JIT round 16 wave 8 (lane trace): the compiled indy bridge's
            // synthetic frame is no Java method; a throwable leaves it out in
            // both modes ([`frame_is_vm_bridge`]).
            TraceSlot::Interp { frame, .. } if frame_is_vm_bridge(frame) => None,
            TraceSlot::Interp { frame, bci } => Some(StackTraceEntry {
                class_name: frame.class_name_arc(),
                method_name: frame.method_name_arc(),
                method_descriptor: Some(frame.method_descriptor_arc()),
                source_file: frame.source_file_arc(),
                // A replaced body's own line, else resolved later from the class.
                line_number: frame.own_line_number(*bci).unwrap_or(LINE_NUMBER_UNKNOWN),
                byte_code_index: *bci,
                class_id: frame.stack_trace_class_id(),
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
            TraceSlot::Interp { frame, .. } => Some((
                frame.method_name(),
                Some(frame.class_id),
                frame.class_name(),
            )),
            TraceSlot::Compiled(slot) => {
                let (class_name, method_name) = split_frame_label(&slot.label)?;
                Some((
                    method_name,
                    Some(ClassId::new(slot.owner_class_id)),
                    class_name,
                ))
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
///
/// JIT round 16 (lane throwpath, TP16-1): two backward byte scans, not
/// `rsplit_once(char)`. The answer is the same -- `:` and `.` are ASCII, so
/// the last byte equal to either is the last such char and a char boundary --
/// but the generic `CharSearcher` per call was ~10% of a throwable's fill
/// (`next_match_back` / `memrchr`, sampled), and a capture splits every
/// compiled frame's label several times.
pub(in crate::runtime::stackwalker) fn split_frame_label(label: &str) -> Option<(&str, &str)> {
    let (dot, colon) = label_split_positions(label.as_bytes())?;
    let (class_name, method_name) = (&label[..dot], &label[dot + 1..colon]);
    if class_name.is_empty() || method_name.is_empty() {
        return None;
    }
    Some((class_name, method_name))
}

/// The last `:` of a label and the last `.` before it, by SIMD `memrchr`
/// (2026-10-07: the byte-at-a-time `rposition` scans, run several times per
/// frame per capture, were ~10% of a compiled `new RuntimeException` across
/// the layout, the trim and the screens -- FP `perf`, `JkiThrowCpu`). Exactly
/// `rposition`'s answer: `:` and `.` are ASCII, so the last such byte is the
/// last such char and a char boundary.
#[inline]
pub(in crate::runtime::stackwalker) fn label_split_positions(bytes: &[u8]) -> Option<(usize, usize)> {
    let colon = memchr::memrchr(b':', bytes)?;
    let dot = memchr::memrchr(b'.', &bytes[..colon])?;
    Some((dot, colon))
}

/// [`interleave_compiled_frames_without_store`]'s layout, as slots. Each
/// `push_*` below keeps the refusal of the entry builder it mirrors, so a slot
/// is pushed iff that builder would have produced an entry.
pub(in crate::runtime::stackwalker) fn collect_trace_slots<'a>(
    frames: &'a [Frame],
    jit: &'a [ActiveCompiledFrame],
    osr_bci: &'a OsrBciOverrides,
) -> Vec<TraceSlot<'a>> {
    collect_trace_slots_from(frames, 0, jit, osr_bci)
}

/// [`collect_trace_slots`] for the tail of the layout that starts at interpreter
/// frame `start`: the same slots in the same order as the whole layout's
/// suffix, provided `jit` holds exactly the compiled entries pushed at
/// interpreter depth `start + 1` or deeper (those are the entries the whole
/// layout places after frame `start`; shallower ones sit before it). Indices
/// (`interp_depth`, the OSR override keys) stay ABSOLUTE.
pub(in crate::runtime::stackwalker) fn collect_trace_slots_from<'a>(
    frames: &'a [Frame],
    start: usize,
    jit: &'a [ActiveCompiledFrame],
    osr_bci: &'a OsrBciOverrides,
) -> Vec<TraceSlot<'a>> {
    let inlined = jit.iter().map(|f| f.chain().len()).sum::<usize>()
        + osr_bci
            .values()
            .map(|(_, chain)| chain.len())
            .sum::<usize>();
    let mut out = Vec::with_capacity(frames.len().saturating_sub(start) + jit.len() + inlined);
    let mut next = 0usize;
    for (i, frame) in frames.iter().enumerate().skip(start) {
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
pub(in crate::runtime::stackwalker) fn push_compiled_slots<'a>(out: &mut Vec<TraceSlot<'a>>, slot: &'a ActiveCompiledFrame) {
    if split_frame_label(&slot.label).is_none() {
        return;
    }
    out.push(TraceSlot::Compiled(slot));
    push_inlined_slots(out, slot.chain(), slot.compile_cp_stamp);
}

/// [`push_inlined_chain_without_store`]: outermost level first, and the first
/// level that cannot be named ends the chain. `cp_stamp` is the inlining
/// activation's compile stamp.
pub(in crate::runtime::stackwalker) fn push_inlined_slots<'a>(
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

pub(in crate::runtime::stackwalker) fn capture_full_trace_with_lines(
    class_store: &ClassStore,
    frames: &[Frame],
    resolve_lines: bool,
) -> Vec<StackTraceEntry> {
    let jit = whole_stack_compiled_frames();
    if jit.is_empty() {
        return frames
            .iter()
            .map(|f| entry_from_frame_with_lines(class_store, f, resolve_lines))
            .collect();
    }
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    interleave_compiled_frames(class_store, frames, &jit, &osr_bci, resolve_lines)
}

/// The innermost `want` entries of what [`capture_full_trace`] returns (same
/// order, OUTERMOST-first), without building the rest, and whether the result is
/// the WHOLE trace (`true`: fewer than `want` frames exist, or the window grew
/// to the whole stack). Line numbers are not resolved (the entry's
/// `line_number` is unknown), which is all a caller-class reader ignores.
///
/// SBR wave 6 (lane dispatch), for `Reflection.getCallerClass`: it read two
/// frames off a whole-stack capture with line numbers -- 1.3 us at five frames,
/// and proportional to the depth of the stack and the number of compiled
/// activations. The tail is exact because the layout is a concatenation:
/// interpreter frame `i`, its OSR continuation's inlined chain, then the
/// compiled entries pushed at depth `i + 1`. So the suffix from frame `start`
/// needs only the compiled entries at depth `start + 1` or deeper
/// ([`active_compiled_frames_from`]) and the duplicate-frame rules'
/// per-`(depth, label)` ledger never crosses those depths. When the window
/// holds too few entries (a frame refused, a short stack) it is widened, ending
/// at the whole stack, so the answer is never a shorter tail than `want`
/// unless the whole trace is shorter.
///
/// `CRATONVM_CALLER_TAIL_VERIFY=1` compares every answer with the whole capture
/// (the caller does it: `NativeContextImpl::capture_stack_trace_tail`).
/// JIT round 21 wave 3, lane walk (WALK21-9 / WALK21-10): the innermost
/// `want` entries of what [`capture_stack_walk_trace`] builds BEFORE its
/// splices and pair rules (`NativeContextImpl::capture_stack_walk_trace_with`
/// applies those; its caller proves they cannot apply to the window), lines
/// resolved exactly as that capture resolves them, OUTERMOST-first -- or
/// `None` when the window would be the whole stack (the caller takes the whole
/// capture then) or a class was ever redefined (that capture's old-activation
/// rule needs every slot).
///
/// Unlike [`capture_tail_trace`] the window bounds the compiled walk too: a
/// deep compiled recursion is one chain entry above a shallow interpreter
/// stack, so a window over interpreter frames alone covers the whole stack.
/// Each chain entry reports at most its innermost `bound` activations
/// ([`active_compiled_frames_bounded`]); an entry that reached the bound may be
/// truncated (its outer activations missing), but with `bound >= want` the
/// innermost truncated entry alone holds at least `want` slots, so the last
/// `want` slots never cross a truncation gap. The window widens (x4) until it
/// holds `want` slots or is the whole stack.
pub(crate) fn capture_walk_tail(
    class_store: &ClassStore,
    frames: &[Frame],
    want: usize,
) -> Option<Vec<StackTraceEntry>> {
    if cratonvm_classloading::class_redefinition_count() != 0 || want == 0 {
        return None;
    }
    let mut window = want.saturating_mul(2).max(8);
    loop {
        let start = frames.len().saturating_sub(window);
        let min_depth = if start == 0 {
            0
        } else {
            u32::try_from(start + 1).unwrap_or(u32::MAX)
        };
        let bound = window.max(want);
        let jit = crate::jit::stack_trace::active_compiled_frames_bounded(min_depth, bound);
        // An entry that reported `bound` activations may have more.
        let mut per_entry: rustc_hash::FxHashMap<u32, usize> = rustc_hash::FxHashMap::default();
        for f in &jit {
            *per_entry.entry(f.chain_index).or_insert(0) += 1;
        }
        let maybe_truncated = per_entry.values().any(|&n| n >= bound);
        if start == 0 && !maybe_truncated {
            // The whole stack: the caller's whole capture is exact and cheap
            // enough at this size.
            return None;
        }
        let (jit, osr_bci) = if jit.is_empty() {
            (jit, OsrBciOverrides::default())
        } else {
            drop_osr_continuations(frames, jit)
        };
        let slots = collect_trace_slots_from(frames, start, &jit, &osr_bci);
        if slots.len() >= want {
            let from = slots.len() - want;
            let mut out: Vec<StackTraceEntry> = Vec::with_capacity(want);
            for slot in &slots[from..] {
                let entry = match slot {
                    TraceSlot::Interp { frame, bci } => {
                        let mut entry = entry_from_frame_with_lines(class_store, frame, true);
                        // An OSR override's bci (the capture relines it).
                        if *bci != entry.byte_code_index {
                            reline_entry(class_store, &mut entry, *bci, true);
                        }
                        Some(entry)
                    }
                    TraceSlot::Compiled(compiled) => compiled_frame_entry(class_store, compiled, true),
                    TraceSlot::Inlined(level, _) => inlined_frame_entry(class_store, level, true),
                };
                // A slot exists exactly where its builder answers (see
                // `collect_trace_slots_from`); a refusal here is answered by
                // the whole capture.
                out.push(entry?);
            }
            fill_missing_source_files(class_store, &mut out);
            return Some(out);
        }
        if start == 0 && bound >= usize::MAX / 8 {
            return None;
        }
        window = window.saturating_mul(4);
    }
}

/// WALK21-10: [`capture_stack_walk_trace`] of the frames below an anchor --
/// the first `interp_depth` interpreter frames and the compiled activations of
/// the JIT chain entries `0 .. chain_len` -- as that capture builds them
/// (lines, the old-activation rule, source files). A walk that took the anchor
/// at its start gets, from deeper in its own call, exactly the frames its
/// capture would have listed then: the frames below the anchor are suspended
/// and unchanged, and everything the call pushed since lies above it.
pub(crate) fn capture_stack_walk_trace_below(
    cm: &crate::classloading::ClassManager,
    frames: &[Frame],
    interp_depth: usize,
    chain_len: u32,
) -> Vec<StackTraceEntry> {
    let class_store = &cm.class_store;
    let frames = &frames[..interp_depth.min(frames.len())];
    let jit: Vec<ActiveCompiledFrame> = whole_stack_compiled_frames()
        .into_iter()
        .filter(|f| f.chain_index < chain_len)
        .collect();
    if jit.is_empty() {
        let mut out: Vec<StackTraceEntry> = frames
            .iter()
            .map(|f| entry_from_frame_with_lines(class_store, f, true))
            .collect();
        fill_missing_source_files(class_store, &mut out);
        if cratonvm_classloading::class_redefinition_count() != 0 {
            for (entry, frame) in out.iter_mut().zip(frames) {
                if frame_predates_its_class_redefinition(cm, frame) {
                    clear_old_activation_line(entry);
                }
            }
        }
        return out;
    }
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    let mut out = interleave_compiled_frames(class_store, frames, &jit, &osr_bci, true);
    fill_missing_source_files(class_store, &mut out);
    if cratonvm_classloading::class_redefinition_count() != 0 {
        let slots = collect_trace_slots(frames, &jit, &osr_bci);
        if slots.len() == out.len() {
            for (entry, slot) in out.iter_mut().zip(&slots) {
                if slot_predates_its_class_redefinition(cm, slot) {
                    clear_old_activation_line(entry);
                }
            }
        }
    }
    out
}

/// WALK21-9 / WALK21-10: may a walk window (`entries`, a suffix of the raw
/// capture) be handed out without the walk capture's pair rules? Exactly when
/// neither rule's own screen passes for any of its entries: the
/// `Thread.run` middle frame needs a `run`-named callee
/// (`may_be_thread_run_middle_callee_name`), the callback-native middle frames
/// a `<clinit>` / `loadClass` / `findClass` callee
/// (`insert_callback_native_walk_entries`). The bottom `Thread.run` frame
/// goes outermost, and a window is never the whole stack.
pub(crate) fn walk_tail_needs_no_pair_rule(entries: &[StackTraceEntry]) -> bool {
    !entries.iter().any(|e| {
        matches!(&*e.method_name, "<clinit>" | "loadClass" | "findClass")
            || may_be_thread_run_middle_callee_name(&e.method_name)
    })
}

pub fn capture_tail_trace(
    class_store: &ClassStore,
    frames: &[Frame],
    want: usize,
) -> (Vec<StackTraceEntry>, bool) {
    let mut window = want.saturating_mul(2).max(8);
    loop {
        let start = frames.len().saturating_sub(window);
        let whole = start == 0;
        let min_depth = if whole {
            0
        } else {
            u32::try_from(start + 1).unwrap_or(u32::MAX)
        };
        let jit = crate::jit::stack_trace::active_compiled_frames_bounded(
            min_depth,
            if whole { usize::MAX } else { window },
        );
        let (jit, osr_bci) = if jit.is_empty() {
            (jit, OsrBciOverrides::default())
        } else {
            drop_osr_continuations(frames, jit)
        };
        let slots = collect_trace_slots_from(frames, start, &jit, &osr_bci);
        if whole || slots.len() >= want {
            let from = slots.len().saturating_sub(want);
            let entries = slots[from..]
                .iter()
                .filter_map(|slot| match slot {
                    // The frame's own identity, without the class-store probe
                    // (`method_index`, line numbers) a caller-class reader
                    // never looks at.
                    TraceSlot::Interp { .. } => slot.build(),
                    TraceSlot::Compiled(compiled) => {
                        compiled_frame_entry(class_store, compiled, false)
                    }
                    TraceSlot::Inlined(level, _) => inlined_frame_entry(class_store, level, false),
                })
                .collect();
            return (entries, whole);        }
        window = window.saturating_mul(4);
    }
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
/// `ThrowableFamilySweep.java` and `IoSystemSweep` ask for exactly
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
pub(in crate::runtime::stackwalker) fn throwable_fill_frame_keep_len<T: FillFrameIdentity>(
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
pub(in crate::runtime::stackwalker) trait FillFrameIdentity {
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
    let jit = crate::jit::stack_trace::active_compiled_frames();
    if jit.is_empty() {
        return frames.iter().rev().map(|f| f.class_id).collect();
    }
    interleave_class_ids(
        frames,
        0,
        jit,
        crate::runtime::interpreter::jit_bridge::live_osr_continuation_artifact,
    )
}

