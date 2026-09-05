// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A single execution frame (stack frame) in the JVM.
//!
//! Each method invocation creates a new `Frame` containing:
//! - Local variables (`Vec<CompactValue>`, 8 bytes/slot, NaN-boxed tag inline)
//! - Operand stack (also NaN-boxed `Vec<CompactValue>` via `ValueStack`)
//! - Program counter
//! - Method bytecode and exception table

use std::collections::HashMap;
use std::sync::Arc;

use cratonvm_reader::attribute::ExceptionTableEntry;

use crate::classloading::resolution::CachedBytecodeMethod;
use crate::classloading::ClassId;
use crate::runtime::ValueStack;
use crate::types::{
    CompactTag, CompactValue, ObjectRef, Value, VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG,
    VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR, VTAG_UNINIT,
};

/// Create a bytecode Arc with 2 trailing zero bytes for safe speculative reads.
/// This allows the hot loop to unconditionally read `code[pc+1]` and `code[pc+2]`
/// without bounds checks, since the padding guarantees valid memory.
///
/// # Prefer [`padded_bytecode_for_method`] on per-invocation paths
///
/// This function copies the whole method body into a fresh allocation on
/// *every* call. Call sites that run once per method *resolution* (building a
/// [`CachedBytecodeMethod`], a JIT record, …) are fine. Call sites that run
/// once per *frame construction* — notably the uncached invoke path in
/// `interpreter.rs` — pay an allocation + full memcpy per method entry and
/// should use [`padded_bytecode_for_method`], which memoizes the padded Arc
/// under the method's own identity.
pub fn padded_bytecode(code: &[u8]) -> Arc<[u8]> {
    let mut padded = Vec::with_capacity(code.len() + 2);
    padded.extend_from_slice(code);
    padded.push(0);
    padded.push(0);
    Arc::from(padded.into_boxed_slice())
}

// ---------------------------------------------------------------------------
// Per-method padded-bytecode memo
// ---------------------------------------------------------------------------
//
// WHY KEYED BY METHOD IDENTITY AND NOT BY CONTENT
// -----------------------------------------------
// The obvious implementation of "intern the padded bytecode" is a
// content-addressed table: hash the bytes, return a shared Arc for equal
// bodies. That is UNSOUND here, because the *pointer identity* of
// `Frame::code` is already used elsewhere in the VM as a proxy for method
// identity:
//
//   `runtime/local_liveness.rs` caches a per-method `LivenessTable` under the
//   key `(Arc::as_ptr(code), code.len())`, validated with `Arc::ptr_eq`. The
//   table is computed by `analyze(code, exception_table)` — it depends on the
//   method's **exception table**, which is NOT part of the hashed bytes.
//
// Two distinct methods can have byte-identical `Code` but different exception
// table ranges (generated / obfuscated / hand-written classfiles). A
// content-addressed interner would hand them the same Arc, the liveness cache
// would answer the second method from the first method's handler edges, and an
// under-approximated live-locals mask drops a GC root — the silent
// heap-corruption class of bug. So we key on *method identity* instead:
// distinct methods never share an Arc, and identical bodies are deliberately
// NOT merged.
//
// The content check on a hit is still required: class redefinition /
// retransformation rewrites the body under an unchanged
// `(class_id, name, descriptor)`. On a content mismatch we mint a fresh Arc and
// replace the entry, so `Arc` sharing semantics are identical to the
// non-memoized path (old frames keep the old Arc alive; the liveness cache's
// `Weak` + `ptr_eq` guard sees a different pointer and recomputes).
//
// THE OTHER CODE-POINTER-KEYED CACHE
// ----------------------------------
// `reader/src/quickened.rs::intern` also keys on `(code.as_ptr(), code.len())`
// (reached via `interpreter.rs::quickened_for_frame`). That one is *content
// only* — `QuickenedCode::build` is a pure pre-decode of `Instruction::decode`
// over the bytes, with no constant-pool dependence — and its `Some` entries pin
// a strong `Arc<[u8]>`, so a recycled address cannot yield a wrong hit.
// Method-identity keying is safe for it and in fact helps: two frames of the
// same method now share one code allocation, so the quickened stream is reused
// instead of rebuilt and re-interned under a fresh address. `local_liveness` is
// the constraint, not this.

/// `(class_id, hash(method_name ++ descriptor))`.
type PaddedCodeKey = (u32, u64);

/// Entry cap for the padded-bytecode memo. On overflow the whole table is
/// dropped (entries are cheap to recompute) — mirrors the bounded-cache policy
/// in `local_liveness.rs`. Keeps runaway class generators (ByteBuddy / CGLIB)
/// from retaining every synthetic method body forever.
const PADDED_CODE_CACHE_CAP: usize = 16384;

/// Bodies larger than this are not memoized: they are rare, and retaining them
/// would dominate the cache's footprint.
const PADDED_CODE_CACHE_MAX_LEN: usize = 32 * 1024;

fn padded_code_cache() -> &'static std::sync::Mutex<HashMap<PaddedCodeKey, Arc<[u8]>>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<PaddedCodeKey, Arc<[u8]>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// FNV-1a over `bytes`, seeded with `seed` so several fields can be chained.
#[inline]
fn fnv1a64(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = seed;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Padded bytecode for one *method*, memoized under that method's identity.
///
/// Semantically identical to [`padded_bytecode`] — same bytes, same `>= 2`
/// trailing zero bytes, same `Arc<[u8]>` type — but repeat calls for the same
/// method return the *same* Arc instead of a fresh allocation + full memcpy.
///
/// Distinct methods never share an Arc (see the module comment above): the key
/// is `(class_id, method_name, descriptor)`, and a hit is additionally verified
/// against the actual bytes so a redefined method mints a fresh Arc.
pub fn padded_bytecode_for_method(
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    code: &[u8],
) -> Arc<[u8]> {
    if code.len() > PADDED_CODE_CACHE_MAX_LEN {
        return padded_bytecode(code);
    }
    let h = fnv1a64(
        fnv1a64(0xcbf2_9ce4_8422_2325, method_name.as_bytes()),
        method_descriptor.as_bytes(),
    );
    let key: PaddedCodeKey = (class_id.as_u32(), h);
    let mut cache = match padded_code_cache().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(hit) = cache.get(&key) {
        // Verify the body actually matches: guards both a `(class_id, hash)`
        // collision between two methods of the same class and a redefinition
        // that rewrote the body under an unchanged identity.
        if hit.len() == code.len() + 2 && hit[..code.len()] == *code {
            return Arc::clone(hit);
        }
    }
    if cache.len() >= PADDED_CODE_CACHE_CAP {
        cache.clear();
    }
    let fresh = padded_bytecode(code);
    cache.insert(key, Arc::clone(&fresh));
    fresh
}

/// JVM slots required to hold `args` as the initial local variable array
/// (category-2 types occupy two consecutive slots).
#[inline]
fn invoke_arg_slot_count(args: &[Value]) -> usize {
    let mut n = 0usize;
    for a in args {
        n += if a.is_category2() { 2 } else { 1 };
    }
    n
}

/// `Code.max_locals` must be at least this many slots for the parameters the
/// verifier expects, but some classfiles in the wild are wrong. If declared
/// `max_locals` is too small, `copy_args_to_locals` would silently drop tail
/// arguments and callees would see `null`/uninitialized locals (e.g. Surefire
/// `JUnitPlatformProvider.<init>(ProviderParameters,Launcher)` with `launcher`
/// never stored).
#[inline]
fn effective_max_locals(declared: u16, args: &[Value]) -> u16 {
    let needed = invoke_arg_slot_count(args);
    let needed_u16 = u16::try_from(needed).unwrap_or(u16::MAX);
    declared.max(needed_u16)
}

/// Cold-path frame metadata: either owned Arcs or a shared CachedBytecodeMethod.
/// For cached calls, storing a single Arc<CachedBytecodeMethod> avoids cloning
/// 5 separate Arc fields per call (class_name, method_name, descriptor, source_file,
/// exception_table), saving ~10 atomic ops per call cycle (clone + drop).

/// How many frames of each kind have been constructed, for
/// `CRATONVM_DBG_INVOKE_PHASES=1`.
///
/// Boxing `OwnedFrameMeta` trades one heap allocation per `Owned` frame for 64
/// bytes off EVERY frame. That trade is only correct if `Owned` frames are rare
/// on real workloads, which is a claim about execution frequency — and the
/// static evidence is ambiguous, since `Frame::new` has ~74 call sites against
/// `new_pooled_cached`'s 8. So it is counted rather than argued.
static FRAMES_OWNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FRAMES_CACHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[inline]
pub(crate) fn count_frame_kind(owned: bool) {
    if !crate::runtime::interpreter::invoke_phases::on() {
        return;
    }
    let c = if owned { &FRAMES_OWNED } else { &FRAMES_CACHED };
    c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(owned, cached)` frame construction counts. Printed by
/// `invoke_phases::dump()`.
pub fn frame_kind_counts() -> (u64, u64) {
    (
        FRAMES_OWNED.load(std::sync::atomic::Ordering::Relaxed),
        FRAMES_CACHED.load(std::sync::atomic::Ordering::Relaxed),
    )
}
/// The identity a frame carries when it is NOT backed by a
/// `CachedBytecodeMethod` — five fat pointers, boxed.
///
/// Boxed because an enum is sized by its LARGEST variant. Inline, these five
/// made `FrameInner` 80 bytes and `Frame` 296, so every frame on the hot
/// interpreted path paid 80 bytes for a variant it never uses — its own
/// `Cached` payload is a single 8-byte `Arc`. `Frame` is built and then MOVED
/// by value into the frame stack on every call, so those bytes are memory
/// traffic per invocation, and again on pop.
///
/// The trade is one heap allocation per `Owned` frame, and the frequency of
/// those was MEASURED rather than reasoned about, because reasoning about it
/// gave the wrong answer twice. Call-site counts suggest `Owned` dominates
/// (~74 `Frame::new` sites against 8 for `new_pooled_cached`); two small runs
/// then reported near-identical absolute counts (466 over 8M calls, 478 over
/// ~1.6k), which reads as a fixed bootstrap cost. Both were wrong. Scaling the
/// workload shows `Owned` frames growing at exactly ONE PER REFLECTIVE INVOKE:
///
/// ```text
///   plain calls   owned=242   1.2%   (bootstrap only)
///   lambdas/indy  owned=242   1.2%   (bootstrap only — indy builds NONE)
///   throw/catch   owned=244          (bootstrap only — unwinding builds NONE)
///   reflection    owned=20241 97.5%  (one per `Method.invoke`)
/// ```
///
/// So the allocation lands on reflection alone. It is invisible there: a
/// reflective invoke costs ~9us in this interpreter, against ~25ns for the
/// malloc — 0.3%, and a reflection-dominated A/B measured no regression.
/// `CRATONVM_DBG_INVOKE_PHASES=1` reports the split so this stays checked.
#[derive(Clone)]
pub struct OwnedFrameMeta {
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    pub method_descriptor: Arc<str>,
    pub source_file: Option<Arc<str>>,
    pub exception_table: Arc<[ExceptionTableEntry]>,
}

enum FrameInner {
    /// Non-cached frame: owns all metadata Arcs, behind one pointer.
    Owned(Box<OwnedFrameMeta>),
    /// Cached frame: all cold metadata derived from a single Arc.
    Cached(Arc<CachedBytecodeMethod>),
}

impl std::fmt::Debug for FrameInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameInner::Owned(o) => {
                write!(f, "Owned({}.{})", o.class_name, o.method_name)
            }
            FrameInner::Cached(cm) => {
                write!(f, "Cached({}.{})", cm.class_name, cm.method_name)
            }
        }
    }
}

/// A single execution frame for a method invocation.
///
/// Locals are stored as `Vec<CompactValue>` (8 bytes/slot, NaN-boxed tag
/// embedded in the high bits of the u64). This collapses what used to be a
/// parallel `Vec<u64> + Vec<u8>` SoA pair into a single cache-line-friendly
/// buffer: every `iload_N` / `istore_N` / `iinc` opcode now touches one
/// allocation instead of two (HIGH-8 audit fix, 2026-05-16).
///
/// ## Field ordering (round-7 HIGH #4)
///
/// Fields are deliberately grouped by access frequency so the hot footprint
/// fits in the first ~128 bytes (~2 cache lines on x86-64) and cold metadata
/// trails after them. Per-opcode hot reads/writes hit `class_id`, `pc`,
/// `last_instr_pc`, `locals`, `stack`, `code`, `max_stack`, `max_locals`,
/// and `backward_count`. Cold-only state (the
/// `FrameInner` metadata enum, the OSR backoff vector, the synchronized
/// monitor-on-exit slot) is placed *after* the hot region so the dispatch
/// loop's frame load doesn't drag those bytes into L1.
///
/// Round-6 left `backward_count` *between* `inner` (cold) and
/// `osr_attempt_counts` (cold), which interleaved a per-back-edge mutated
/// field with two cold-only fields and forced a second cache line to be
/// touched on every backward branch. This struct layout is intentionally
/// non-`#[repr(C)]` — Rust may reorder for natural alignment, but the
/// declaration order already provides a hot-then-cold partition that the
/// compiler's auto-layout preserves.
#[derive(Debug)]
pub struct Frame {
    // ── Hot fields (touched on every opcode) ────────────────────────────
    /// The class that declares this method.
    pub class_id: ClassId,

    /// Current program counter (byte offset into `code`).
    pub pc: usize,

    /// PC of the instruction that started the current/last execution cycle.
    /// Used for exception table lookup when unwinding through parent frames,
    /// since `pc` may have already been advanced past the invoke instruction.
    pub last_instr_pc: usize,

    /// Local variable storage (NaN-boxed CompactValues, one 8-byte slot each).
    locals: Vec<CompactValue>,

    /// Per-local kind marks paralleling `locals` (see [`LKIND_OTHER`]).
    /// `local_kinds[i]` is `LKIND_LONG` / `LKIND_DOUBLE` iff slot `i` currently
    /// holds a primitive `long` / `double`. Maintained alongside `locals` by
    /// every setter; its sole consumer is the GC root scan/update, which must
    /// never treat a primitive cat-2 value as a heap reference even when its
    /// NaN-boxed bits collide with the `SUB_OBJECT` pattern (BouncyCastle F2m
    /// `LongArray` `0xfffd_…` words). Mirrors `ValueStack::kinds`. Reuses the
    /// `Vec<u8>` half of the frame pool tuple that the 2026-05-16 SoA collapse
    /// left unused.
    local_kinds: Vec<u8>,

    /// Operand stack (NaN-boxed CompactValues internally).
    pub stack: ValueStack,

    /// The raw bytecode of the method (hot path — kept as direct Arc).
    pub code: Arc<[u8]>,

    /// Maximum operand stack depth (from Code attribute).
    pub max_stack: u16,

    /// Local variable slot count for this frame: at least the `Code.max_locals`
    /// value from the class file and at least the slots required by the actual
    /// invocation arguments (defensive clamp; see `effective_max_locals`).
    pub max_locals: u16,

    /// Backward branch counter for OSR (On-Stack Replacement).
    /// Incremented on each backward branch; triggers JIT when exceeding threshold.
    /// Hot — bumped on every back-edge of every loop in the interpreter.
    pub backward_count: u32,

    /// Per-frame-instance unique id, used ONLY by the opt-in root-snapshot
    /// cache (`CRATONVM_ROOTSNAP_CACHE`). A frame still present at index `k`
    /// with an unchanged `seq` proves — by the LIFO stack discipline — that
    /// every frame below it has been continuously present. Zero (and never
    /// assigned) when the cache gate is off, so the default build pays nothing.
    ///
    /// CORRECTNESS NOTE: `seq` alone is INSUFFICIENT as a root-snapshot cache
    /// key. "Continuously present" is NOT "frozen": a frame below the top can
    /// still RE-EXECUTE and reassign its locals (it becomes the top again when a
    /// callee returns into it, runs more bytecode — e.g. a loop that allocates a
    /// fresh object into a local each iteration — then calls another method). Its
    /// `seq` is unchanged (it was never popped), so a seq-only cache would reuse
    /// stale roots that MISS the reassigned local → that live object is left
    /// unrooted → reclaimed by the next GC → the all-zero-header
    /// (`AbstractMethodError: ... has no Code attribute`) corruption cascade.
    /// `exec_epoch` (below) closes this: it is bumped every time a callee returns
    /// into this frame (i.e. the frame is about to re-execute) and every time
    /// bytecode mutates a local slot. The cache key `(seq, exec_epoch)`
    /// invalidates on root-shape changes while still reusing the roots of a
    /// genuinely-frozen deep frame (whose locals do not change until the very end
    /// — the perf case the cache exists for).
    pub seq: u64,

    /// Mutation counter for the root-snapshot cache key (paired with `seq`).
    /// Bumped in `pop_and_recycle_frame_with_reason` when a callee frame is
    /// popped and THIS frame becomes the top again, and also by local stores
    /// themselves. See the `seq` doc above.
    pub exec_epoch: u64,

    // ── Cold fields (metadata, rarely-mutated state) ────────────────────
    /// Cold-path metadata (method name, descriptor, exception table, etc.).
    inner: FrameInner,

    /// CR-CLO-2 — this method's slot in its declaring class's `Class::methods`
    /// list, when the pusher knew it. `None` means "not known", which is always
    /// a correct answer: every consumer falls back to today's behaviour.
    ///
    /// # Why it exists
    ///
    /// `stackwalker::capture_frames_no_lines` is the thread-dump depositor. It
    /// runs on the blocking thread at every safepoint/blocking point and
    /// therefore must not take a `ClassStore` borrow, so the
    /// `StackTraceEntry::method_index` it publishes is `None` and deferred
    /// resolution (`resolve_line_numbers_in_place`) falls back to the
    /// unambiguous-name rule. That rule fails closed on an overload set —
    /// overloads share a name and have different `LineNumberTable`s — so
    /// overloaded frames in a cross-thread dump report no line at all. A `u32`
    /// copied out of the frame needs no borrow and no allocation, which is the
    /// whole point.
    ///
    /// # It is an index, never a borrow, and never trusted alone
    ///
    /// `resolve_line_numbers_in_place` re-reads `class.methods[idx]` from the
    /// live `ClassStore` and re-checks that its `name` equals the recorded
    /// method name before using it. That check catches a *wrong name*; it does
    /// **not** catch another member of the same overload set, which is exactly
    /// the population this field exists to disambiguate. So a stale index is
    /// worse than no index, and the two places that can strand one —
    /// [`Frame::reset_for_tail_call`] and [`Frame::from_frozen_frame`] — clear
    /// it explicitly rather than by omission.
    ///
    /// # Placement
    ///
    /// On `Frame` rather than inside `FrameInner`, deliberately: it then covers
    /// `Owned` and `Cached` frames uniformly with one field, one accessor and
    /// one reset rule, instead of a per-variant setter that would silently
    /// no-op on the `Cached` half (whose metadata `Arc` is shared across every
    /// frame of that method and must not carry per-push state). The enum is
    /// sized by its `Owned` variant either way, so nothing is saved by hiding
    /// the field in there. See the doc for the `CachedBytecodeMethod`-side
    /// follow-up that makes the cached path resolve once per *method*.
    method_index: Option<u32>,

    /// CRIT-PERF (audit 2026-05-17): per-loop OSR attempt counter with
    /// exponential backoff.
    ///
    /// The trigger condition used to be `bc == OSR_THRESHOLD` (exact
    /// equality), which meant that if the OSR compile was queued but the
    /// JIT entry trampoline wasn't ready yet, `try_osr` rejected the
    /// attempt and **the counter immediately moved past the threshold**
    /// — so the loop kept iterating in the interpreter forever without
    /// ever retrying OSR.
    ///
    /// Round-4 wave-1 first replaced this with a permanent-ban `Vec<usize>`
    /// — but that swung too far the other way: a *single* transient reject
    /// (e.g. compile queued but trampoline not yet installed) blocked any
    /// future OSR attempt for that loop forever, even though the next
    /// back-edge ~µs later might well have succeeded.
    ///
    /// Round-5 fix (audit `round5-vm.md`): track an attempt counter
    /// per entry PC and use **exponential backoff** — first retry after
    /// `OSR_THRESHOLD` back-edges, second after `2 * OSR_THRESHOLD`,
    /// third after `4 * OSR_THRESHOLD`, etc.  After
    /// `OSR_MAX_ATTEMPTS` (=5) rejections we give up permanently
    /// (equivalent to the old permanent ban for truly hot loops the
    /// compiler can't handle).
    ///
    /// Stored as a small `Vec<(entry_pc, attempts)>` rather than a hashmap
    /// because a method has only a handful of loops and the per-frame
    /// allocation cost (24-byte Vec header) dwarfs hashmap overhead.
    /// Capacity stays at zero unless OSR actually fires.
    pub osr_attempt_counts: Vec<(usize, u32)>,

    /// For synchronized methods dispatched via the stackless path: the monitor
    /// object that must be released when this frame returns or is unwound by
    /// an exception.  `None` for non-synchronized methods.
    pub monitor_on_exit: Option<ObjectRef>,
}

// ---------------------------------------------------------------------------
// Per-local kind marks (GC root-scan disambiguation)
// ---------------------------------------------------------------------------
//
// A `long` / `double` is stored in a single NaN-boxed `CompactValue` slot,
// bit-exact (see `CompactValue::long`). A handful of those bit patterns
// collide with the `SUB_OBJECT` NaN-box tag (bits 63-50 all set, sub-tag
// 010) — e.g. the BouncyCastle F2m `LongArray` `0xfffd_…` words produced by
// `lxor`/`lshl`/`lushr` over `long[]`. For such a slot `is_object()` returns
// `true`, so a *context-free* GC root scan would mistake the primitive long
// for a heap reference and (when its low 47 bits happen to land on a live
// object) root + relocate it, corrupting the long and the object graph.
//
// The operand stack avoids this with its parallel `ValueStack::kinds`; the
// 2026-05-16 SoA collapse dropped the equivalent array for locals, which is
// what reintroduced the corruption. These marks restore it: a slot tagged
// `LKIND_LONG` / `LKIND_DOUBLE` is a primitive and is NEVER a GC root or a
// relocation target, regardless of bit pattern.
const LKIND_OTHER: u8 = 0; // reference / int / float / null / uninit / retaddr
const LKIND_LONG: u8 = 1;
const LKIND_DOUBLE: u8 = 2;

thread_local! {
    /// Per-OS-thread monotonic frame-instance counter for [`Frame::seq`].
    /// Per-thread (not a global atomic) so frame creation pays no cross-thread
    /// cache-line contention. Uniqueness is only required within one thread's
    /// own frame stack — the root-snapshot cache only ever compares this
    /// thread's frames against this thread's cached seqs. Starts at 1 so a
    /// freshly-default `seq = 0` (gate-off frames) never aliases a real id.
    static FRAME_SEQ_CTR: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// Next unique frame-instance id, or 0 when the root-snapshot cache is
/// disabled — so opt-out builds pay only a cached-bool read per frame creation,
/// not the thread-local bump.
#[inline]
fn next_frame_seq() -> u64 {
    if !crate::runtime::env_cache::rootsnap_cache() {
        return 0;
    }
    FRAME_SEQ_CTR.with(|c| {
        let v = c.get();
        c.set(v.wrapping_add(1).max(1));
        v
    })
}

/// Cached `CRATONVM_DBG_STALELONG` gate (bc math-ec 0x4 smear hunt): log
/// LONG-kind locals whose bits match a `pointer_map` key at remap time —
/// candidate smuggled-object-ref-as-long going stale.
#[inline]
fn stalelong_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STALELONG").is_some())
}

/// Kind mark for a `Value` about to be written into a local slot. Only the
/// category-2 primitives need a positive mark; everything else (including
/// genuine object references) stays `LKIND_OTHER` so the GC continues to scan
/// it as a potential root.
#[inline(always)]
fn lkind_of_value(v: &Value) -> u8 {
    match v {
        Value::Long(_) => LKIND_LONG,
        Value::Double(_) => LKIND_DOUBLE,
        _ => LKIND_OTHER,
    }
}

/// Kind mark for compact locals when the VM can prove the slot is a
/// category-2 primitive from its compact tag.
#[inline(always)]
fn lkind_of_compact(cv: &CompactValue) -> u8 {
    match cv.tag() {
        CompactTag::Long => LKIND_LONG,
        _ => LKIND_OTHER,
    }
}

#[inline]
fn lost_tag_local_candidates(cv: CompactValue) -> [u64; 3] {
    [
        cv.as_object_ptr().unwrap_or(0),
        cv.as_long().map(|l| l as u64).unwrap_or(0),
        cv.raw_bits(),
    ]
}

/// Pool-backed local-Vec initialisation.
///
/// The pool stores `(Vec<u64>, Vec<u8>)` tuples — the `u64` half is the
/// recycled locals buffer (transmuted to/from `Vec<CompactValue>` via
/// `#[repr(transparent)]`), and the `u8` half is now unused (locals no longer
/// have a parallel tag Vec; tags are encoded inline in each CompactValue).
/// The `u8` Vec is preserved in the pool tuple shape so cross-crate callers
/// (`JvmThread::recycle_frame_with_shared`, `VecPool<u8>` spill paths) keep
/// working without churn.
fn init_locals_pooled(
    max_locals: u16,
    args: &[Value],
    pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
) -> (Vec<CompactValue>, Vec<u8>, u16) {
    init_locals_from_parts(max_locals, args, pool.pop())
}

/// The body of [`init_locals_pooled`], taking the recycled `(vals, tags)` pair
/// directly instead of popping it from a pool. Lets the non-pooled
/// constructors source their buffers from the per-OS-thread pool
/// ([`tls_pop_locals`]) without materialising a throw-away pool `Vec`.
///
/// `parts == None` is exactly the old allocating path: `unwrap_or_default()`
/// yields empty Vecs which are then resized to `n`, so the pooled and unpooled
/// arms cannot drift apart. `copy_args_to_locals` fills the argument slots and
/// every other slot is `CompactValue::uninitialized()` / `LKIND_OTHER`, so no
/// recycled content can survive into the new frame.
fn init_locals_from_parts(
    max_locals: u16,
    args: &[Value],
    parts: Option<(Vec<u64>, Vec<u8>)>,
) -> (Vec<CompactValue>, Vec<u8>, u16) {
    let eff = effective_max_locals(max_locals, args);
    let n = eff as usize;
    let (vals, mut kinds) = parts.unwrap_or_default();
    // SAFETY: CompactValue is repr(transparent) over u64 — transmute is a
    // no-op layout-wise. The `Vec<u8>` half (formerly the discarded local-tag
    // Vec) is reused as the parallel `local_kinds` buffer; clear + resize
    // overwrites any stale recycled content so no kind leaks across reuse.
    let mut locals = u64_vec_to_compact(vals);
    locals.clear();
    kinds.clear();
    // Arguments first, filler second, so every slot is written exactly once.
    // The previous order (`resize` to `n`, then overwrite the leading argument
    // slots) wrote each argument slot twice on every frame push — nine bytes
    // per slot, on the hottest path in the VM. The end state is identical:
    // `push_args_to_locals` lays down exactly the slots `copy_args_to_locals`
    // would have, and the `resize` below fills exactly the ones it would have
    // left as filler.
    push_args_to_locals(&mut locals, &mut kinds, args, n);
    locals.resize(n, CompactValue::uninitialized());
    kinds.resize(n, LKIND_OTHER);
    debug_assert_eq!(locals.len(), n, "locals must be exactly max_locals long");
    debug_assert_eq!(kinds.len(), n, "kinds must parallel locals");
    (locals, kinds, eff)
}

// ---------------------------------------------------------------------------
// Per-OS-thread SoA buffer pool for the *non-pooled* Frame constructors
// ---------------------------------------------------------------------------
//
// `Frame::new_pooled` / `Frame::new_pooled_cached` take the owning
// `JvmThread`'s `locals_pool` / `stacks_pool` by `&mut`, so they never
// allocate. `Frame::new` and `Frame::new_from_arcs` have no access to the
// thread (they are called from paths that only hold the frame's inputs), so
// until now each one ran `vec![…; n]` twice for the locals pair and
// `ValueStack::new` — which itself zero-fills TWO more Vecs of `max_stack`.
// Four allocations plus four zero-fills per frame, and `Frame::new_from_arcs`
// is the hot *uncached* invoke path in `interpreter.rs`.
//
// This pool closes that hole without touching the thread-owned pools, so the
// dynamics of the cached invoke path are bit-for-bit unchanged. (That matters:
// a previous attempt — noted in `interpreter.rs` as "P4 reverted" — routed the
// uncached path through the *thread* pools and hung the BouncyCastle suites.
// Nothing here competes with those pools for buffers.)
//
// It is fed by `JvmThread::recycle_frame` from the branch that used to simply
// DROP a popped frame's Vecs once `locals_pool` was already at `MAX_POOL_SIZE`,
// so it is a strict improvement: buffers that were previously freed are now
// reused, and when the pool is empty both constructors fall back to exactly the
// allocating path they used before.

/// Per-OS-thread reusable `(values, tags)` buffers, split by role so a locals
/// buffer sized for `max_locals` is not handed to an operand stack sized for
/// `max_stack` (and vice versa) — either would work, but keeping them apart
/// preserves the size distribution and avoids repeated grow/shrink churn.
struct FrameSoaTls {
    locals: Vec<(Vec<u64>, Vec<u8>)>,
    stacks: Vec<(Vec<u64>, Vec<u8>)>,
}

/// Max buffers retained per role per OS thread.
const TLS_SOA_POOL_CAP: usize = 32;

/// Buffers with more slots than this are not retained — a single pathological
/// `max_stack` / `max_locals` method must not pin a large allocation for the
/// lifetime of the thread.
const TLS_SOA_MAX_RETAINED_SLOTS: usize = 4096;

thread_local! {
    static FRAME_SOA_TLS: std::cell::RefCell<FrameSoaTls> = const {
        std::cell::RefCell::new(FrameSoaTls {
            locals: Vec::new(),
            stacks: Vec::new(),
        })
    };
}

/// Take a recycled locals buffer pair, or `None` when the pool is empty (or the
/// thread-local is already destroyed, during thread teardown).
#[inline]
fn tls_pop_locals() -> Option<(Vec<u64>, Vec<u8>)> {
    FRAME_SOA_TLS
        .try_with(|p| p.borrow_mut().locals.pop())
        .ok()
        .flatten()
}

/// Take a recycled operand-stack buffer pair, or `None`.
#[inline]
fn tls_pop_stack() -> Option<(Vec<u64>, Vec<u8>)> {
    FRAME_SOA_TLS
        .try_with(|p| p.borrow_mut().stacks.pop())
        .ok()
        .flatten()
}

/// Offer a popped frame's four buffers to this OS thread's pool.
///
/// Called by `JvmThread::recycle_frame` when the thread-owned pool is full —
/// i.e. exactly where the buffers used to be dropped. Buffers that exceed
/// [`TLS_SOA_MAX_RETAINED_SLOTS`] or that would push a role past
/// [`TLS_SOA_POOL_CAP`] are dropped as before.
pub fn offer_frame_parts_to_tls_pool(
    local_vals: Vec<u64>,
    local_tags: Vec<u8>,
    stack_vals: Vec<u64>,
    stack_tags: Vec<u8>,
) {
    let _ = FRAME_SOA_TLS.try_with(|p| {
        let mut p = p.borrow_mut();
        if p.locals.len() < TLS_SOA_POOL_CAP
            && local_vals.capacity() <= TLS_SOA_MAX_RETAINED_SLOTS
            && local_tags.capacity() <= TLS_SOA_MAX_RETAINED_SLOTS
        {
            p.locals.push((local_vals, local_tags));
        }
        if p.stacks.len() < TLS_SOA_POOL_CAP
            && stack_vals.capacity() <= TLS_SOA_MAX_RETAINED_SLOTS
            && stack_tags.capacity() <= TLS_SOA_MAX_RETAINED_SLOTS
        {
            p.stacks.push((stack_vals, stack_tags));
        }
    });
}

/// Build an operand stack of `max_size` slots, reusing a pooled buffer pair
/// when one is available. Falls back to `ValueStack::new` (which allocates and
/// zero-fills) only when the pool is empty.
#[inline]
fn value_stack_from_tls_pool(max_size: usize) -> ValueStack {
    match tls_pop_stack() {
        Some((vals, tags)) => ValueStack::from_pooled(vals, tags, max_size),
        None => ValueStack::new(max_size),
    }
}

/// Test/diagnostic helper: current per-role depth of this thread's SoA pool.
#[doc(hidden)]
pub fn tls_soa_pool_depths() -> (usize, usize) {
    FRAME_SOA_TLS
        .try_with(|p| {
            let p = p.borrow();
            (p.locals.len(), p.stacks.len())
        })
        .unwrap_or((0, 0))
}

/// Test helper: drop every pooled buffer on this OS thread so a test starts
/// from a known state. `libtest` may run several tests on one thread
/// (`--test-threads=1`), so tests must not assume an empty pool.
#[cfg(test)]
fn tls_soa_pool_clear() {
    let _ = FRAME_SOA_TLS.try_with(|p| {
        let mut p = p.borrow_mut();
        p.locals.clear();
        p.stacks.clear();
    });
}

/// Append the argument slots to freshly-cleared `locals` / `kinds`, in the
/// JVMS layout (category-2 values take two slots, the upper one unset).
///
/// # Why this exists beside [`copy_args_to_locals`]
///
/// [`init_locals_from_parts`] used to `resize` both buffers to `max_locals`
/// and then overwrite the leading argument slots — so every argument slot was
/// written **twice** on every frame push, once with the filler and once with
/// the argument. Pushing the arguments first and resizing the *remainder*
/// writes each slot exactly once, which is the same end state by construction:
/// the filler value is identical (`CompactValue::uninitialized()` /
/// `LKIND_OTHER`) and it now only ever lands in slots no argument occupies.
///
/// `cap` is `effective_max_locals`, which is already clamped to at least the
/// slots the arguments need, so the bound below is defensive rather than
/// load-bearing — it preserves [`copy_args_to_locals`]'s clamp exactly.
fn push_args_to_locals(
    locals: &mut Vec<CompactValue>,
    kinds: &mut Vec<u8>,
    args: &[Value],
    cap: usize,
) {
    for arg in args {
        if locals.len() >= cap {
            return;
        }
        // Paired with the `lkind_of_value` mark below — a `double` argument
        // keeps its NaN payload across the call boundary.
        locals.push(CompactValue::from_value_kinded(*arg));
        kinds.push(lkind_of_value(arg));
        // Category 2 values (long, double) occupy two slots; the upper half is
        // left uninitialised by JVM convention.
        if arg.is_category2() && locals.len() < cap {
            locals.push(CompactValue::uninitialized());
            kinds.push(LKIND_OTHER);
        }
    }
}

/// In-place argument copy into already-sized buffers.
///
/// Retained for the callers that hand over a slice they did not just build
/// (deopt resume, frozen-frame rehydration). The frame-push path uses
/// [`push_args_to_locals`] instead — see that function for why.
fn copy_args_to_locals(locals: &mut [CompactValue], kinds: &mut [u8], args: &[Value]) {
    let mut slot = 0;
    for arg in args {
        if slot < locals.len() {
            // Paired with the `lkind_of_value` mark below - a `double`
            // argument keeps its NaN payload across the call boundary.
            locals[slot] = CompactValue::from_value_kinded(*arg);
            // `kinds` is always the same length as `locals` (both sized to
            // `n` by every caller), so this index is in bounds whenever the
            // `locals` write above is.
            kinds[slot] = lkind_of_value(arg);
            slot += 1;
            // Category 2 values (long, double) occupy two slots; the upper
            // half is left uninitialised by JVM convention.
            if arg.is_category2() && slot < locals.len() {
                locals[slot] = CompactValue::uninitialized();
                kinds[slot] = LKIND_OTHER;
                slot += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Vec<u64> <-> Vec<CompactValue> transmute helpers (safe under
// CompactValue's repr(transparent) guarantee — identical size and alignment).
// Used at the boundary between the pooled `Vec<u64>` slot buffers and the
// frame's internal `Vec<CompactValue>` locals storage.
// ---------------------------------------------------------------------------

#[inline(always)]
fn u64_vec_to_compact(v: Vec<u64>) -> Vec<CompactValue> {
    // SAFETY: CompactValue is #[repr(transparent)] over u64 — identical size,
    // alignment, and validity invariants (any u64 is a valid CompactValue
    // bit pattern). Vec's length/capacity/allocator are preserved.
    let mut v = std::mem::ManuallyDrop::new(v);
    let len = v.len();
    let cap = v.capacity();
    let ptr = v.as_mut_ptr() as *mut CompactValue;
    unsafe { Vec::from_raw_parts(ptr, len, cap) }
}

#[inline(always)]
fn compact_vec_to_u64(v: Vec<CompactValue>) -> Vec<u64> {
    // SAFETY: see u64_vec_to_compact.
    let mut v = std::mem::ManuallyDrop::new(v);
    let len = v.len();
    let cap = v.capacity();
    let ptr = v.as_mut_ptr() as *mut u64;
    unsafe { Vec::from_raw_parts(ptr, len, cap) }
}

/// Convert a local-slot (u64 + tag) directly to a CompactValue,
/// skipping the Value-enum round-trip (T10.9.D hot-path helper).
///
/// Mirrors the policy of `decode_value`: malformed or zero-init Object
/// slots degrade to null rather than panicking.
#[inline(always)]
fn local_slot_to_compact(val: u64, tag: u8) -> CompactValue {
    match tag {
        VTAG_INT => CompactValue::int(val as i32),
        VTAG_LONG => CompactValue::long(val as i64),
        VTAG_FLOAT => CompactValue::float(f32::from_bits(val as u32)),
        VTAG_DOUBLE => CompactValue::from_bits(val),
        VTAG_OBJECT => {
            // Guard against zero-initialised / stale / unaligned object slots
            // the same way decode_value does.
            if val == 0 || (val as usize) % 8 != 0 {
                CompactValue::null()
            } else {
                CompactValue::object(val)
            }
        }
        VTAG_NULL => CompactValue::null(),
        VTAG_RETADDR => CompactValue::return_address(val as u32),
        _ => CompactValue::uninitialized(),
    }
}

/// Convert a CompactValue back into a legacy (u64 + tag) slot pair.
///
/// Used at boundaries where the legacy SoA representation is still required
/// (continuation freeze/thaw, `locals_snapshot`, `to_frozen_frame`). The
/// hot interpreter path no longer needs this helper.
#[inline(always)]
fn compact_to_local_slot(cv: CompactValue) -> (u64, u8) {
    match cv.tag() {
        CompactTag::Int => {
            // A `long` whose bit pattern collides into the SUB_INT sub-tag
            // (e.g. BC safegcd `0xFFFC_…` accumulators) tags as `Int`. Handing
            // it to the JIT/OSR via `as_int()` would truncate to the low 32
            // bits — the residual long-bit loss that surfaced as JIT-only
            // failures in bc-math-raw's InterleaveTest after the interpreter
            // paths were fixed. Recover the full i64 when the payload proves
            // it cannot be a real int. See
            // bc-ec-mod-mododdinverse-investigation.md.
            if let Some(raw) = cv.int_tag_collision_long() {
                (raw as u64, VTAG_LONG)
            } else {
                (cv.as_int().unwrap_or(0) as u32 as u64, VTAG_INT)
            }
        }
        CompactTag::Long => (cv.as_long_unchecked() as u64, VTAG_LONG),
        CompactTag::Float => (cv.as_float().unwrap_or(0.0).to_bits() as u64, VTAG_FLOAT),
        CompactTag::Double => {
            // Untagged — raw bits. Could be a Double or a Long that was
            // created via CompactValue::long (both untagged).  Prefer the
            // Double tag; stores from Lstore use set_local with Value::Long
            // which goes through encode_value-equivalent paths.
            (cv.raw_bits(), VTAG_DOUBLE)
        }
        CompactTag::Object => {
            let ptr = cv.as_object_ptr().unwrap_or(0);
            (ptr, VTAG_OBJECT)
        }
        CompactTag::Null => (0, VTAG_NULL),
        CompactTag::Uninitialized => (0, VTAG_UNINIT),
        CompactTag::ReturnAddress => (cv.as_return_address().unwrap_or(0) as u64, VTAG_RETADDR),
    }
}

/// CRIT-PERF cap: after this many failed OSR attempts for a single entry
/// PC we stop retrying — the loop is presumably uncompilable.  Matches the
/// effective behaviour of the round-4 wave-1 permanent-ban Vec for hot
/// loops whose IR the JIT genuinely can't lower.
pub const OSR_MAX_ATTEMPTS: u32 = 5;

/// The four buffer-shaped pieces of a cached-method frame.
///
/// Built once and consumed either by [`Frame::new_pooled_cached_compact`],
/// which returns a `Frame` by value, or by
/// [`FrameStack::emplace_cached_compact`], which writes one straight into the
/// stack slot. Sharing the build is what keeps the two from drifting.
struct CachedCompactParts {
    locals: Vec<CompactValue>,
    local_kinds: Vec<u8>,
    stack: ValueStack,
    eff_max_locals: u16,
}

/// Take the locals and operand-stack buffers for a call to `cached` from the
/// thread's pools and lay the arguments into them.
///
/// Identical in every observable to what `new_pooled_cached_compact` did
/// inline before this was factored out: same filler, same category-2 layout,
/// same `effective_max_locals` clamp, same pooled `ValueStack`.
#[inline]
fn build_cached_compact_parts(
    cached: &CachedBytecodeMethod,
    args: &[(CompactValue, u8)],
    locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
) -> CachedCompactParts {
    let needed: usize = args
        .iter()
        .map(|(_, t)| if matches!(*t, b'J' | b'D') { 2 } else { 1 })
        .sum();
    let n = (cached.max_locals as usize).max(needed);
    let eff_max_locals = u16::try_from(n).unwrap_or(u16::MAX);
    let (vals, mut kinds) = locals_pool.pop().unwrap_or_default();
    let mut locals = u64_vec_to_compact(vals);
    locals.clear();
    kinds.clear();
    locals.reserve(n);
    kinds.reserve(n);
    for (cv, tag) in args {
        locals.push(*cv);
        match *tag {
            b'J' => {
                kinds.push(LKIND_LONG);
                locals.push(CompactValue::uninitialized());
                kinds.push(LKIND_OTHER);
            }
            b'D' => {
                kinds.push(LKIND_DOUBLE);
                locals.push(CompactValue::uninitialized());
                kinds.push(LKIND_OTHER);
            }
            _ => kinds.push(LKIND_OTHER),
        }
    }
    locals.resize(n, CompactValue::uninitialized());
    kinds.resize(n, LKIND_OTHER);
    debug_assert_eq!(locals.len(), n);
    debug_assert_eq!(kinds.len(), n);
    let padded_max = (cached.max_stack as usize).max(16) + 8;
    let stack = if let Some((vals, tags)) = stacks_pool.pop() {
        ValueStack::from_pooled(vals, tags, padded_max)
    } else {
        ValueStack::new(padded_max)
    };
    CachedCompactParts {
        locals,
        local_kinds: kinds,
        stack,
        eff_max_locals,
    }
}


/// [`build_cached_compact_parts`] for arguments that are still `Value`s.
///
/// The general dispatchers decode their arguments before they know which
/// callee shape they have, so they cannot use the fast doors' verbatim
/// `(slot, tag)` transfer. Everything after that point is the same, and
/// sharing [`CachedCompactParts`] is what lets them share the emplace.
#[inline]
fn build_cached_value_parts(
    cached: &CachedBytecodeMethod,
    args: &[Value],
    locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
) -> CachedCompactParts {
    let (locals, local_kinds, eff_max_locals) =
        init_locals_pooled(cached.max_locals, args, locals_pool);
    let padded_max = (cached.max_stack as usize).max(16) + 8;
    let stack = if let Some((vals, tags)) = stacks_pool.pop() {
        ValueStack::from_pooled(vals, tags, padded_max)
    } else {
        ValueStack::new(padded_max)
    };
    CachedCompactParts {
        locals,
        local_kinds,
        stack,
        eff_max_locals,
    }
}

/// [`build_cached_value_parts`] for callers outside this module. Tuple, for
/// the same reason [`take_cached_compact_parts`] is one.
#[inline]
pub(crate) fn take_cached_value_parts(
    cached: &CachedBytecodeMethod,
    args: &[Value],
    locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
) -> (Vec<CompactValue>, Vec<u8>, ValueStack, u16) {
    let p = build_cached_value_parts(cached, args, locals_pool, stacks_pool);
    (p.locals, p.local_kinds, p.stack, p.eff_max_locals)
}

/// [`build_cached_compact_parts`] for callers outside this module.
///
/// Returns the pieces as a tuple so the struct itself can stay private:
/// `(locals, local_kinds, stack, effective_max_locals)`.
#[inline]
pub(crate) fn take_cached_compact_parts(
    cached: &CachedBytecodeMethod,
    args: &[(CompactValue, u8)],
    locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
) -> (Vec<CompactValue>, Vec<u8>, ValueStack, u16) {
    let p = build_cached_compact_parts(cached, args, locals_pool, stacks_pool);
    (p.locals, p.local_kinds, p.stack, p.eff_max_locals)
}

impl Frame {
    /// Return `true` if a fresh OSR attempt should be made for `entry_pc`
    /// given the current `backward_count` and the per-loop exponential
    /// backoff schedule.  See `osr_attempt_counts` for the rationale.
    ///
    /// `osr_threshold` is the base back-edge count required for the first
    /// attempt (typically `OSR_THRESHOLD`).  The k-th retry fires at
    /// `osr_threshold << k` back-edges; once `OSR_MAX_ATTEMPTS` is reached
    /// the loop is permanently shadowed.
    #[inline]
    pub fn should_try_osr(&self, entry_pc: usize, osr_threshold: u32) -> bool {
        let bc = self.backward_count;
        // Linear scan: a method has only a handful of loops in practice.
        let attempts = self
            .osr_attempt_counts
            .iter()
            .find(|(pc, _)| *pc == entry_pc)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        if attempts >= OSR_MAX_ATTEMPTS {
            return false;
        }
        // Exponential backoff: 1×, 2×, 4×, 8×, 16× the base threshold.
        // `checked_shl` defends against overflow if `OSR_MAX_ATTEMPTS` is
        // ever bumped large enough to push past u32::MAX.
        let shift = attempts;
        let backoff = osr_threshold.checked_shl(shift).unwrap_or(u32::MAX);
        bc >= backoff
    }

    /// Record that an OSR attempt for `entry_pc` failed (returned `None`
    /// from `try_osr`).  Bumps the per-loop attempt counter so that the
    /// next attempt waits exponentially longer.
    #[inline]
    pub fn record_osr_rejection(&mut self, entry_pc: usize) {
        for (pc, n) in &mut self.osr_attempt_counts {
            if *pc == entry_pc {
                *n = n.saturating_add(1);
                return;
            }
        }
        self.osr_attempt_counts.push((entry_pc, 1));
    }

    /// Throttle the next poll while an off-thread OSR compile is pending without
    /// consuming the bounded permanent-rejection budget.
    #[inline]
    pub fn record_osr_background_pending(&mut self) {
        self.backward_count = 0;
    }

    /// Create a new frame for a method (converts owned String/Vec to Arc).
    ///
    /// `args` are copied into the first local variable slots.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        class_id: ClassId,
        class_name: String,
        method_name: String,
        method_descriptor: String,
        source_file: Option<String>,
        code: Vec<u8>,
        exception_table: Vec<ExceptionTableEntry>,
        max_stack: u16,
        max_locals: u16,
        args: &[Value],
    ) -> Self {
        let (locals, local_kinds, eff_max_locals) =
            init_locals_from_parts(max_locals, args, tls_pop_locals());
        let code = padded_bytecode_for_method(class_id, &method_name, &method_descriptor, &code);
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals,
            local_kinds,
            stack: value_stack_from_tls_pool((max_stack as usize).max(16) + 8),
            code,
            max_stack,
            max_locals: eff_max_locals,
            inner: {
                count_frame_kind(true);
                FrameInner::Owned(Box::new(OwnedFrameMeta {
                    class_name: Arc::from(class_name.as_str()),
                    method_name: Arc::from(method_name.as_str()),
                    method_descriptor: Arc::from(method_descriptor.as_str()),
                    source_file: source_file.map(|s| Arc::from(s.as_str())),
                    exception_table: Arc::from(exception_table.into_boxed_slice()),
                }))
            },
            // The caller passes loose parts, not a resolved method; use
            // `set_method_index` where the slot is known.
            method_index: None,
            backward_count: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            seq: next_frame_seq(),
            exec_epoch: 0,
        }
    }

    /// Create a new frame from pre-built Arc data (zero-copy for code and strings).
    ///
    /// # Bytecode padding precondition (SAFETY)
    ///
    /// The hot interpreter dispatch loop unconditionally reads `code[pc+1]` and
    /// `code[pc+2]` for multi-byte opcodes without per-read bounds checks (see
    /// e.g. `interpreter.rs` opcode decoding for `bipush`, `sipush`, `getfield`,
    /// jump targets, etc.). The decode relies on **at least 2 trailing zero
    /// bytes** of padding at the end of `code` so the speculative reads never
    /// fall off the end of the allocation.
    ///
    /// Callers **must** pass an already-padded `Arc<[u8]>` — typically the
    /// result of [`padded_bytecode`]. This zero-copy constructor exists
    /// precisely to avoid an extra allocation when the caller already holds a
    /// padded Arc (e.g. cached from a prior load); validating/re-padding here
    /// would defeat that goal. The `debug_assert!` below catches violations
    /// in debug builds; in release builds an unpadded input leads to an
    /// out-of-bounds read in the hot loop, which is UB.
    ///
    /// If the caller cannot guarantee padding, use [`Frame::new_pooled`] or
    /// `Frame::new` (which both run their input through `padded_bytecode`).
    #[allow(clippy::too_many_arguments)]
    pub fn new_from_arcs(
        class_id: ClassId,
        class_name: Arc<str>,
        method_name: Arc<str>,
        method_descriptor: Arc<str>,
        source_file: Option<Arc<str>>,
        code: Arc<[u8]>,
        exception_table: Arc<[ExceptionTableEntry]>,
        max_stack: u16,
        max_locals: u16,
        args: &[Value],
    ) -> Self {
        // SAFETY/precondition check: the dispatch loop reads up to 2 bytes past
        // the last opcode. Empty `code` is permissible only if no instruction is
        // ever decoded (defensive: still require the 2-byte tail). If `code` is
        // non-empty it must have at least 2 trailing zero bytes.
        debug_assert!(
            code.len() >= 2 && code[code.len() - 1] == 0 && code[code.len() - 2] == 0,
            "Frame::new_from_arcs: bytecode Arc must be padded with >=2 trailing zero \
             bytes (use `padded_bytecode()` on raw classfile bytes). len={}",
            code.len(),
        );
        let (locals, local_kinds, eff_max_locals) =
            init_locals_from_parts(max_locals, args, tls_pop_locals());
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals,
            local_kinds,
            stack: value_stack_from_tls_pool((max_stack as usize).max(16) + 8),
            code,
            max_stack,
            max_locals: eff_max_locals,
            inner: {
                count_frame_kind(true);
                FrameInner::Owned(Box::new(OwnedFrameMeta {
                    class_name,
                    method_name,
                    method_descriptor,
                    source_file,
                    exception_table,
                }))
            },
            // Same as `Frame::new`: loose Arcs, no resolved method slot.
            method_index: None,
            backward_count: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            seq: next_frame_seq(),
            exec_epoch: 0,
        }
    }

    /// Create a new frame from Arc data, reusing pooled Vecs for locals and stack.
    #[allow(clippy::too_many_arguments)]
    pub fn new_pooled(
        class_id: ClassId,
        class_name: Arc<str>,
        method_name: Arc<str>,
        method_descriptor: Arc<str>,
        source_file: Option<Arc<str>>,
        code: Arc<[u8]>,
        exception_table: Arc<[ExceptionTableEntry]>,
        max_stack: u16,
        max_locals: u16,
        args: &[Value],
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) -> Self {
        let (locals, local_kinds, eff_max_locals) =
            init_locals_pooled(max_locals, args, locals_pool);
        let padded_max = (max_stack as usize).max(16) + 8;
        let stack = if let Some((vals, tags)) = stacks_pool.pop() {
            ValueStack::from_pooled(vals, tags, padded_max)
        } else {
            ValueStack::new(padded_max)
        };
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals,
            local_kinds,
            stack,
            code,
            max_stack,
            max_locals: eff_max_locals,
            inner: {
                count_frame_kind(true);
                FrameInner::Owned(Box::new(OwnedFrameMeta {
                    class_name,
                    method_name,
                    method_descriptor,
                    source_file,
                    exception_table,
                }))
            },
            // Same as `Frame::new`: loose Arcs, no resolved method slot.
            method_index: None,
            backward_count: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            seq: next_frame_seq(),
            exec_epoch: 0,
        }
    }

    /// Create a new frame from a cached method, reusing pooled Vecs.
    /// Only clones the `code` Arc (hot path) — all cold metadata is accessed
    /// through the single `Arc<CachedBytecodeMethod>`, saving ~10 atomic ops per call.
    pub fn new_pooled_cached(
        cached: Arc<CachedBytecodeMethod>,
        args: &[Value],
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) -> Self {
        // CR-CLO-2 cached half lives in `from_cached_compact_parts` now:
        // `CachedBytecodeMethod` does not yet carry a method slot (see the
        // field doc — its 38 struct literals live in four crates and none has
        // a `..` tail), so `method_index` is `None` there for both
        // constructors, and that is the only line that changes when it lands.
        let CachedCompactParts {
            locals,
            local_kinds,
            stack,
            eff_max_locals,
        } = build_cached_value_parts(&cached, args, locals_pool, stacks_pool);
        Frame::from_cached_compact_parts(cached, locals, local_kinds, stack, eff_max_locals)
    }

    /// Reset this frame in-place for tail-call elimination.
    /// Reuses the existing Vec allocations (locals, stack) to avoid allocation.
    /// `new_pooled_cached` for arguments that are still operand-stack slots.
    ///
    /// The invoke fast door hands over `(slot, descriptor tag)` pairs read
    /// straight off the caller's operand stack, so each argument is copied
    /// once, `CompactValue` to `CompactValue`, with no `Value` in between.
    /// `tag` is the descriptor's first byte for the parameter (`b'L'` for the
    /// receiver): `b'J'` / `b'D'` occupy two local slots with the same filler
    /// `copy_args_to_locals` writes, everything else one. The slot bits are
    /// stored verbatim, which is exactly what `from_value_kinded` produced
    /// from the decoded `Value` (`long` / `double_raw` / tagged scalars), so
    /// the resulting locals are bit-identical to the general path's.
    pub fn new_pooled_cached_compact(
        cached: Arc<CachedBytecodeMethod>,
        args: &[(CompactValue, u8)],
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) -> Self {
        let CachedCompactParts {
            locals,
            local_kinds: kinds,
            stack,
            eff_max_locals,
        } = build_cached_compact_parts(&cached, args, locals_pool, stacks_pool);
        let class_id = cached.declaring_class_id;
        let code = cached.code.clone();
        let max_stack = cached.max_stack;
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals,
            local_kinds: kinds,
            stack,
            code,
            max_stack,
            max_locals: eff_max_locals,
            inner: {
                count_frame_kind(false);
                FrameInner::Cached(cached)
            },
            method_index: None,
            backward_count: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            seq: next_frame_seq(),
            exec_epoch: 0,
        }
    }

    /// Rebuild this frame in place as a call to `cached`, reusing its
    /// `locals`, `local_kinds` and operand-stack buffers.
    ///
    /// This is [`Self::new_pooled_cached_compact`] with the allocation
    /// question already answered: the frame is a retired slot of the thread's
    /// `FrameStack` (see `FrameStack::push_cached_compact_reusing`), so its
    /// four buffers are already here and none of them has to travel through
    /// the thread's pools. Measured 2026-09-02, that churn — not the filling
    /// of the buffers — is what `frame_build` and `ret_recycle` are mostly
    /// made of.
    ///
    /// Every field is overwritten, so nothing of the retired frame survives
    /// into the new one. The buffers' *contents* above what this writes are
    /// not live: `locals` is sized and filled here exactly as the constructor
    /// does, and the operand stack is empty with `len = 0`.
    pub fn reset_cached_compact(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        args: &[(CompactValue, u8)],
    ) {
        let needed: usize = args
            .iter()
            .map(|(_, t)| if matches!(*t, b'J' | b'D') { 2 } else { 1 })
            .sum();
        let n = (cached.max_locals as usize).max(needed);
        let eff_max_locals = u16::try_from(n).unwrap_or(u16::MAX);

        self.locals.clear();
        self.local_kinds.clear();
        self.locals.reserve(n);
        self.local_kinds.reserve(n);
        for (cv, tag) in args {
            self.locals.push(*cv);
            match *tag {
                b'J' => {
                    self.local_kinds.push(LKIND_LONG);
                    self.locals.push(CompactValue::uninitialized());
                    self.local_kinds.push(LKIND_OTHER);
                }
                b'D' => {
                    self.local_kinds.push(LKIND_DOUBLE);
                    self.locals.push(CompactValue::uninitialized());
                    self.local_kinds.push(LKIND_OTHER);
                }
                _ => self.local_kinds.push(LKIND_OTHER),
            }
        }
        self.locals.resize(n, CompactValue::uninitialized());
        self.local_kinds.resize(n, LKIND_OTHER);
        debug_assert_eq!(self.locals.len(), n);
        debug_assert_eq!(self.local_kinds.len(), n);

        self.reset_cached_tail(cached, eff_max_locals);
    }

    /// Everything a cached-frame reset does once its locals are laid down:
    /// the operand stack, and every scalar field.
    ///
    /// Shared by [`Self::reset_cached_compact`] and [`Self::reset_cached_value`]
    /// so the two argument representations cannot drift in what they leave
    /// behind. Every field is overwritten, so nothing of the retired frame
    /// survives into the new one.
    #[inline]
    fn reset_cached_tail(&mut self, cached: Arc<CachedBytecodeMethod>, eff_max_locals: u16) {
        self.stack
            .reset_in_place((cached.max_stack as usize).max(16) + 8);

        self.class_id = cached.declaring_class_id;
        self.pc = 0;
        self.last_instr_pc = 0;
        self.code = cached.code.clone();
        self.max_stack = cached.max_stack;
        self.max_locals = eff_max_locals;
        self.method_index = None;
        self.backward_count = 0;
        self.osr_attempt_counts.clear();
        self.monitor_on_exit = None;
        self.seq = next_frame_seq();
        self.exec_epoch = 0;
        count_frame_kind(false);
        self.inner = FrameInner::Cached(cached);
    }

    /// [`Self::reset_cached_compact`] for arguments that are still `Value`s.
    ///
    /// The general dispatchers' half of frame-slot reuse. `push_args_to_locals`
    /// is the same function `init_locals_pooled` uses, so the locals this
    /// leaves are those `Frame::new_pooled_cached` would have built — the
    /// difference is only that the buffers were already here.
    pub fn reset_cached_value(&mut self, cached: Arc<CachedBytecodeMethod>, args: &[Value]) {
        let eff_max_locals = effective_max_locals(cached.max_locals, args);
        let n = eff_max_locals as usize;

        self.locals.clear();
        self.local_kinds.clear();
        self.locals.reserve(n);
        self.local_kinds.reserve(n);
        push_args_to_locals(&mut self.locals, &mut self.local_kinds, args, n);
        self.locals.resize(n, CompactValue::uninitialized());
        self.local_kinds.resize(n, LKIND_OTHER);
        debug_assert_eq!(self.locals.len(), n);
        debug_assert_eq!(self.local_kinds.len(), n);

        self.reset_cached_tail(cached, eff_max_locals);
    }


    /// A `Frame` from pieces [`build_cached_compact_parts`] produced.
    #[inline]
    fn from_cached_compact_parts(
        cached: Arc<CachedBytecodeMethod>,
        locals: Vec<CompactValue>,
        local_kinds: Vec<u8>,
        stack: ValueStack,
        eff_max_locals: u16,
    ) -> Self {
        let class_id = cached.declaring_class_id;
        let code = cached.code.clone();
        let max_stack = cached.max_stack;
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals,
            local_kinds,
            stack,
            code,
            max_stack,
            max_locals: eff_max_locals,
            inner: {
                count_frame_kind(false);
                FrameInner::Cached(cached)
            },
            method_index: None,
            backward_count: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            seq: next_frame_seq(),
            exec_epoch: 0,
        }
    }

    pub fn reset_for_tail_call(
        &mut self,
        class_id: ClassId,
        code: Arc<[u8]>,
        max_stack: u16,
        max_locals: u16,
        args: &[Value],
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        source_file: Option<Arc<str>>,
        exception_table: Arc<[ExceptionTableEntry]>,
    ) {
        self.class_id = class_id;
        self.pc = 0;
        self.last_instr_pc = 0;
        self.backward_count = 0;
        self.osr_attempt_counts.clear();
        self.code = code;
        self.max_stack = max_stack;
        let eff_max_locals = effective_max_locals(max_locals, args);
        self.max_locals = eff_max_locals;
        // Update inner metadata so class_name(), method_name(), exception_table() are correct
        count_frame_kind(true);
        self.inner = FrameInner::Owned(Box::new(OwnedFrameMeta {
            class_name,
            method_name,
            method_descriptor: descriptor,
            source_file,
            exception_table,
        }));
        // CR-CLO-2 — MUST be cleared alongside `class_id` and `inner`. A tail
        // call replaces the method executing in this frame while reusing the
        // allocation, so an index left over from the *caller* would be read
        // back as the callee's slot. `resolve_line_numbers_in_place` guards a
        // stale index only by re-checking the method's NAME, which is precisely
        // the check that cannot separate two members of one overload set — and
        // an overload set is the only population this index exists to
        // disambiguate. A stale index is therefore strictly worse than none:
        // it can print a line from the wrong method body, which the fallback
        // never does. `None` restores the fail-closed unambiguous-name rule.
        self.method_index = None;
        // Reset locals
        let n = eff_max_locals as usize;
        self.locals.clear();
        self.locals.resize(n, CompactValue::uninitialized());
        self.local_kinds.clear();
        self.local_kinds.resize(n, LKIND_OTHER);
        copy_args_to_locals(&mut self.locals, &mut self.local_kinds, args);
        // Reset operand stack. CRITICAL: the reused stack was sized for the
        // PREVIOUS method's max_stack; the tail-called method may declare a
        // LARGER max_stack, so grow the backing Vecs to match — otherwise its
        // pushes overflow the smaller stack (the `push_compact` "index out of
        // bounds: len == max_size" panic seen in WildFly's
        // RegularEnumSet$EnumSetIterator → Long/Integer.numberOfTrailingZeros
        // tail-call path). `locals` is already resized for the new method
        // above; the operand stack needs the same treatment.
        self.stack.clear();
        let grew = self.stack.ensure_max_size(max_stack as usize);
        if grew && crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[TCO_GROW] {}.{} reused stack grown to max_stack={}",
                self.class_name(),
                self.method_name(),
                max_stack
            );
        }
    }

    /// Return this frame's Vec allocations to the pool for reuse.
    ///
    /// The locals tuple's `Vec<u8>` half carries the `local_kinds` buffer so
    /// the allocation is recycled alongside the `Vec<CompactValue>` locals
    /// (transmuted to/from `Vec<u64>` at the pool boundary via
    /// `#[repr(transparent)]`). `init_locals_pooled` clears + resizes it on
    /// the next reuse, so stale kinds never leak across frames.
    pub fn recycle(
        self,
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) {
        locals_pool.push((compact_vec_to_u64(self.locals), self.local_kinds));
        stacks_pool.push(self.stack.into_inner());
    }

    /// T10.7 — consume the frame and return its four pooled `Vec`s as
    /// `(local_vals, local_tags, stack_vals, stack_tags)`.
    ///
    /// Used by `JvmThread::recycle_frame_with_shared` to decide per-vector
    /// whether to keep the allocation in the thread-local pool or spill it
    /// into the VM-wide `VecPool` on `SharedVm`.
    ///
    /// `local_tags` carries the `local_kinds` buffer (its `Vec<u8>` slot was
    /// repurposed from the now-removed SoA local-tag Vec); the per-vector
    /// spill routing in `recycle_frame_with_shared` recycles it like any other
    /// pooled `Vec<u8>`, and `init_locals_pooled` overwrites it on reuse.
    pub fn take_pool_parts(self) -> (Vec<u64>, Vec<u8>, Vec<u64>, Vec<u8>) {
        let (stack_vals, stack_tags) = self.stack.into_inner();
        (
            compact_vec_to_u64(self.locals),
            self.local_kinds,
            stack_vals,
            stack_tags,
        )
    }

    /// [`Self::take_pool_parts`] without consuming the frame — harvest the four
    /// pooled `Vec`s out of a frame that is still sitting in its
    /// [`FrameStack`] slot, leaving an empty husk the caller then drops in
    /// place.
    ///
    /// # Why this exists
    ///
    /// `Frame` is a ~300-byte by-value struct, and the return path used to
    /// move it three times: out of the buffer (`FrameStack::pop`), into
    /// `recycle_frame_with_shared`, and again into `take_pool_parts`. `perf`
    /// on the interpreted-invoke probe put `memcpy` under `Vec::pop<Frame>`
    /// and `pop_and_recycle_frame_with_reason` at 6.97% of the invoke arm,
    /// inside a frame-lifecycle group that was ~24.7% of it — see
    /// `known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`.
    /// Nothing on that path needed the frame moved anywhere; it needed four
    /// `Vec` headers out of it.
    ///
    /// Every buffer is swapped out by header (`std::mem::take`), so the
    /// pointed-to allocations are untouched and the pool sees exactly what
    /// `take_pool_parts` gave it. The husk left behind holds four empty
    /// `Vec`s, and `FrameStack::truncate` runs its `Drop` where it lies.
    ///
    /// The `kinds` half of the operand stack is cleared here for the same
    /// reason [`ValueStack::into_inner`] clears it: the pool's contract is
    /// that the tag vector comes back **empty** (capacity retained), and
    /// `from_pooled` clears-and-resizes it on reuse. Returning stale marks
    /// would break the round-trip that `into_inner_preserves_capacity` pins.
    pub fn take_pool_parts_in_place(&mut self) -> (Vec<u64>, Vec<u8>, Vec<u64>, Vec<u8>) {
        let (stack_vals, stack_tags) = self.stack.take_inner_in_place();
        let locals = std::mem::take(&mut self.locals);
        let local_kinds = std::mem::take(&mut self.local_kinds);
        (
            compact_vec_to_u64(locals),
            local_kinds,
            stack_vals,
            stack_tags,
        )
    }

    // ── Cold-path accessors (method metadata, exception table) ──────────

    /// Access the class name (cold path — error messages, stack traces).
    #[inline]
    pub fn class_name(&self) -> &str {
        match &self.inner {
            FrameInner::Owned(o) => &o.class_name,
            FrameInner::Cached(cm) => &cm.class_name,
        }
    }

    /// The shared `CachedBytecodeMethod` behind this frame, when it was
    /// pushed through the cached-invoke path. Used by the interpreter to
    /// reach the per-method memoized quickened instruction stream without a
    /// hash lookup; `Owned` frames fall back to the process-wide intern
    /// table keyed on the bytecode allocation.
    #[inline]
    pub(crate) fn cached_method(&self) -> Option<&Arc<CachedBytecodeMethod>> {
        match &self.inner {
            FrameInner::Owned(_) => None,
            FrameInner::Cached(cm) => Some(cm),
        }
    }

    /// This frame's method return-type tag byte — the character after `')'`
    /// in its descriptor, or `b'V'` for void and for a malformed descriptor.
    ///
    /// # Why it is a method rather than a call to `jit::return_type`
    ///
    /// `areturn` needs this on **every** reference return, to pick the
    /// `coerce_value_for_return_validated` shape, and in object-oriented
    /// bytecode most returns are reference returns. The call it replaces
    /// (`crate::jit::return_type(frame.method_descriptor())`) is a linear scan
    /// of the descriptor string for `')'`, so a method returning
    /// `Ljava/util/Map;` from a two-reference-parameter signature paid forty
    /// byte comparisons per return.
    ///
    /// A `Cached` frame answers from the `OnceLock` on its shared
    /// `CachedBytecodeMethod`, so the scan happens once per method. An
    /// `Owned` frame has no such record and keeps the scan — those are the
    /// uncached, reflective and synthetic pushes, not the hot path.
    ///
    /// `CachedBytecodeMethod::return_tag` honours
    /// `CRATONVM_JIT_NO_DESCRIPTOR_FACTS`, so the switch reverts this to the
    /// per-return scan on both arms.
    #[inline]
    pub fn return_tag(&self) -> u8 {
        match &self.inner {
            FrameInner::Cached(cm) => cm.return_tag(),
            FrameInner::Owned(o) => cratonvm_jit::return_type(&o.method_descriptor),
        }
    }

    /// Access the method name (cold path — error messages, stack traces).
    #[inline]
    pub fn method_name(&self) -> &str {
        match &self.inner {
            FrameInner::Owned(o) => &o.method_name,
            FrameInner::Cached(cm) => &cm.method_name,
        }
    }

    /// Access the method descriptor (cold path — error messages).
    #[inline]
    pub fn method_descriptor(&self) -> &str {
        match &self.inner {
            FrameInner::Owned(o) => &o.method_descriptor,
            FrameInner::Cached(cm) => &cm.method_descriptor,
        }
    }

    /// Access the source file (cold path — stack traces).
    #[inline]
    pub fn source_file(&self) -> Option<&str> {
        match &self.inner {
            FrameInner::Owned(o) => o.source_file.as_deref(),
            FrameInner::Cached(cm) => cm.source_file.as_deref(),
        }
    }

    /// Access the exception table (cold path — catch block lookup).
    #[inline]
    pub fn exception_table(&self) -> &[ExceptionTableEntry] {
        match &self.inner {
            FrameInner::Owned(o) => &o.exception_table,
            FrameInner::Cached(cm) => &cm.exception_table,
        }
    }

    /// Clone class_name as Arc<str> for stack trace capture (cold path).
    pub fn class_name_arc(&self) -> Arc<str> {
        match &self.inner {
            FrameInner::Owned(o) => o.class_name.clone(),
            FrameInner::Cached(cm) => cm.class_name.clone(),
        }
    }

    /// Clone method_name as Arc<str> for stack trace capture (cold path).
    pub fn method_name_arc(&self) -> Arc<str> {
        self.method_name_arc_ref().clone()
    }

    /// Borrow the method_name `Arc<str>` without cloning. Use this from
    /// hot paths (PGO branch / back-edge recording) that build a
    /// `MethodKey` and want to defer the refcount bump until the
    /// borrow is actually consumed.
    pub fn method_name_arc_ref(&self) -> &Arc<str> {
        match &self.inner {
            FrameInner::Owned(o) => &o.method_name,
            FrameInner::Cached(cm) => &cm.method_name,
        }
    }

    /// Clone method_descriptor as Arc<str> (cold path — PGO profiling, error messages).
    pub fn method_descriptor_arc(&self) -> Arc<str> {
        self.method_descriptor_arc_ref().clone()
    }

    /// Borrow the method_descriptor `Arc<str>` without cloning. Same
    /// rationale as `method_name_arc_ref`.
    pub fn method_descriptor_arc_ref(&self) -> &Arc<str> {
        match &self.inner {
            FrameInner::Owned(o) => &o.method_descriptor,
            FrameInner::Cached(cm) => &cm.method_descriptor,
        }
    }

    /// Clone source_file as Option<Arc<str>> for stack trace capture (cold path).
    pub fn source_file_arc(&self) -> Option<Arc<str>> {
        match &self.inner {
            FrameInner::Owned(o) => o.source_file.clone(),
            FrameInner::Cached(cm) => cm.source_file.clone(),
        }
    }

    /// CR-CLO-2 — this frame's slot in its declaring class's `Class::methods`,
    /// when the pusher knew it. See the `Frame::method_index` field doc.
    ///
    /// A plain `u32` copy: no `ClassStore` borrow, no lock, no allocation.
    /// That is the whole reason the field exists — it is what lets the
    /// deliberately lock-free thread-dump depositor
    /// (`stackwalker::capture_frames_no_lines`) publish
    /// `method_index: f.method_index()` instead of `None`, which in turn lets
    /// deferred resolution put an exact line on an *overloaded* frame rather
    /// than failing closed to `UNKNOWN`.
    ///
    /// `None` is always a correct answer and simply keeps today's behaviour.
    #[inline]
    pub fn method_index(&self) -> Option<u32> {
        self.method_index
    }

    /// Record this frame's slot in its declaring class's `Class::methods`.
    ///
    /// Called by a pusher that already holds the `ClassStore` borrow and the
    /// resolved `ClassFileMethod` — i.e. that has the index in hand for free.
    /// It must be **this** frame's own method: the index is re-verified against
    /// the live class by name only, and a name check cannot separate two
    /// members of one overload set (see the field doc).
    ///
    /// Passing `None` is always safe and restores the unambiguous-name
    /// fallback; a pusher that is unsure should pass `None` rather than guess.
    #[inline]
    pub fn set_method_index(&mut self, method_index: Option<u32>) {
        self.method_index = method_index;
    }

    // ── Local variable access ───────────────────────────────────────────

    /// Number of local variable slots.
    #[inline(always)]
    pub fn locals_len(&self) -> usize {
        self.locals.len()
    }

    /// Get a local variable by index.
    ///
    /// Honors the `local_kinds` mark for a category-2 primitive the same way
    /// `ValueStack::value_at` honors `kinds`: a `double` whose raw bits collide
    /// with the NaN-box tag space is stored verbatim (see
    /// `CompactValue::double_raw`) and must not be re-read through the
    /// context-free `to_value()`, which would report the sub-tag's type. For
    /// every pattern that predates the verbatim store this arm is a no-op -
    /// an untagged double already decodes as `Value::Double` with the same
    /// bits.
    pub fn get_local(&self, index: u16) -> Value {
        let i = index as usize;
        if i >= self.locals.len() {
            return Value::Uninitialized;
        }
        if self.local_kinds[i] == LKIND_DOUBLE {
            return Value::Double(f64::from_bits(self.locals[i].raw_bits()));
        }
        self.locals[i].to_value()
    }

    /// Get a local variable by index without error wrapping.
    /// Used by the fast-path interpreter for verified bytecode.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_unchecked(&self, index: usize) -> Value {
        // Kind-aware for the same reason as `get_local` - see its doc.
        if self.local_kinds[index] == LKIND_DOUBLE {
            return Value::Double(f64::from_bits(self.locals[index].raw_bits()));
        }
        self.locals[index].to_value()
    }

    /// Set a local variable by index.
    pub fn set_local(&mut self, index: u16, value: Value) {
        let i = index as usize;
        if i < self.locals.len() {
            // `CRATONVM_DBG_VACATED_FRAMES`: catch a stale reference AS IT
            // ENTERS a frame, which is the one moment the Rust producer is
            // still on the stack. Every frame this VM builds sets its incoming
            // arguments through here, so this covers the invoke paths that keep
            // arguments in a Rust buffer between the pop and the frame build —
            // the fast/cached dispatchers pin nothing and repair with
            // `refresh_stale_object_args`, i.e. with `load_and_forward`, which
            // cannot repair anything on a collector that leaves no forwarding
            // word (see `VmHeap::load_and_forward`).
            //
            // The ledger is exact: `gc_quiescence::note_allocated` drops an
            // address the moment the allocator re-issues it, so a hit here is a
            // reference to memory the collector moved an object out of and
            // nothing has been allocated into since.
            if cratonvm_gc::gc_quiescence::vacated_frames_enabled() {
                if let Value::Object(Some(o)) = value {
                    cratonvm_gc::gc_quiescence::check_stale_use(
                        o.as_ptr() as usize,
                        "frame local store",
                    );
                    if let Some(moved_to) =
                        cratonvm_gc::gc_quiescence::was_vacated(o.as_ptr() as usize)
                    {
                        static N: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 12 {
                            tracing::error!(
                                target: "cratonvm::gc::guard",
                                obj = format!("{:#x}", o.as_ptr() as usize),
                                moved_to = format!("{moved_to:#x}"),
                                class = %self.class_name(),
                                method = %self.method_name(),
                                pc = self.pc,
                                slot = i,
                                backtrace = %std::backtrace::Backtrace::force_capture(),
                                "a STALE reference is being stored into a frame local — the                                  collector moved this object and nothing has been allocated at                                  the old address since. The backtrace names the VM code that                                  still held it.",
                            );
                        }
                    }
                }
            }
            self.note_local_write();
            // `lkind_of_value` marks a `Value::Double` LKIND_DOUBLE two lines
            // below, which is the mark `from_value_kinded` needs to store a
            // tag-colliding NaN payload verbatim instead of flattening it.
            self.locals[i] = CompactValue::from_value_kinded(value);
            let k = lkind_of_value(&value);
            self.local_kinds[i] = k;
            self.invalidate_cat2_upper_half(i, k);
        }
    }

    /// A category-2 (`long`/`double`) value occupies JVM local slots `i` and
    /// `i+1`, but this VM keeps the whole 64-bit value in slot `i` alone — slot
    /// `i+1` is a spec-mandated reservation that carries no value and is never
    /// read on its own. `set_local*` must nonetheless OVERWRITE slot `i+1`:
    /// if it previously held an object reference (e.g. a scoped-out reference
    /// temp that javac reused the pair for), leaving it intact makes the GC
    /// root scan treat that dead reference as live for the frame's whole
    /// lifetime. On the SteadyChurn repro `main`'s setup-loop `Node` temp lived
    /// in the slot the churn-loop `long` reused, so every appended node stayed
    /// reachable through its `next` chain — unbounded retention / OOM at any
    /// heap size (`CRATONVM_G1_DBG_ROOTCENSUS=1`). The upper half is set to a
    /// non-object int filler and marked cat-2 so the scan's `LKIND` gate skips
    /// it before it ever reaches the lost-tag pointer probe.
    #[inline(always)]
    fn invalidate_cat2_upper_half(&mut self, i: usize, kind: u8) {
        if (kind == LKIND_LONG || kind == LKIND_DOUBLE) && i + 1 < self.locals.len() {
            // Marking `local_kinds[i + 1]` as LKIND_LONG/_DOUBLE tells every
            // other reader (the GC scan gate at `scan_local_objects`,
            // `update_local_refs`, and `get_local_raw`) to treat this slot's
            // `CompactValue` as raw untagged bits, not NaN-boxed — that's
            // exactly how a real Long/Double `CompactValue` is stored (see
            // `CompactValue::long`/`double`). The filler must honor that same
            // contract: `CompactValue::int(0)` is NaN-boxed (`raw_bits()` ==
            // `0xFFFC_0000_0000_0000`, not `0`), so a kind-consistent reader
            // like `get_local_raw` would report a bogus non-zero "upper half"
            // instead of the clean `0` this comment promises. `long(0)` is
            // stored bit-exact as `0` and is still provably a non-object
            // (untagged bit patterns never satisfy `is_nan_tagged`), so it's
            // just as safe a filler while being raw-bits-correct too.
            self.locals[i + 1] = CompactValue::long(0);
            self.local_kinds[i + 1] = kind;
        }
    }

    /// Set a local variable by index without error wrapping.
    /// Used by the fast-path interpreter for verified bytecode.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn set_local_unchecked(&mut self, index: usize, value: Value) {
        self.note_local_write();
        // See `set_local`: the LKIND_DOUBLE mark written below is what makes
        // the verbatim double store sound.
        self.locals[index] = CompactValue::from_value_kinded(value);
        let k = lkind_of_value(&value);
        self.local_kinds[index] = k;
        // Invalidate the reserved upper half of a cat-2 store — see
        // `invalidate_cat2_upper_half`. Guarded on `index + 1` so a
        // (malformed) cat-2 store into the last slot cannot panic.
        self.invalidate_cat2_upper_half(index, k);
    }

    /// Set a local int slot directly (T10.9.D hot-path, mirrors
    /// `ValueStack::push_int_unchecked`). Avoids the `Value` enum round-trip
    /// for `istore_N` / `iinc` / int-typed `wide_istore`.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn set_local_int_unchecked(&mut self, index: usize, v: i32) {
        self.note_local_write();
        self.locals[index] = CompactValue::int(v);
        // An int never aliases the SUB_OBJECT pattern, but clearing any prior
        // cat-2 mark keeps `local_kinds` an exact reflection of the slot.
        self.local_kinds[index] = LKIND_OTHER;
    }

    /// Get a local int slot directly (T10.9.D hot-path, mirrors
    /// `ValueStack::pop_int_unchecked`). Returns 0 if the slot does not
    /// currently hold an int (mirrors the prior `as_int().unwrap_or(0)`
    /// fall-back used at `iinc` sites).
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_int_unchecked(&self, index: usize) -> i32 {
        self.locals[index].as_int().unwrap_or(0)
    }

    /// Get the raw u64 value of a local (for JIT/OSR interop).
    ///
    /// Returns the legacy SoA-style raw bits (e.g. `i32 as u32 as u64` for
    /// ints, `f64::to_bits()` for doubles), reconstructed from the inline
    /// CompactValue. The JIT ABI expects this exact representation.
    ///
    /// Honors the `local_kinds` mark first, exactly like `locals_snapshot`:
    /// a long/double whose bits collide with the NaN-tag space (e.g. into the
    /// `SUB_OBJECT` sub-tag) would otherwise be misread by
    /// `compact_to_local_slot` as an object slot, truncating the value to its
    /// low 47 bits. `try_osr`'s per-local `jit_locals` snapshot is this
    /// method's only production caller, so this was a real, unguarded
    /// OSR-entry-state-reconstruction corruption path: a collision long got
    /// silently handed to the OSR-compiled frame as a fabricated, truncated
    /// "pointer" instead of its true bit-exact value.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_raw(&self, index: usize) -> u64 {
        match self.local_kinds[index] {
            LKIND_LONG | LKIND_DOUBLE => self.locals[index].raw_bits(),
            _ => {
                let (v, _t) = compact_to_local_slot(self.locals[index]);
                v
            }
        }
    }

    /// Get the legacy VTAG byte of a local (for JIT/OSR interop).
    ///
    /// Derived from the inline CompactValue tag — the parallel `local_tags`
    /// Vec no longer exists. Honors the `local_kinds` mark first; see
    /// [`get_local_raw`](Self::get_local_raw) for why this matters.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_tag(&self, index: usize) -> u8 {
        match self.local_kinds[index] {
            LKIND_LONG => VTAG_LONG,
            LKIND_DOUBLE => VTAG_DOUBLE,
            _ => {
                let (_v, t) = compact_to_local_slot(self.locals[index]);
                t
            }
        }
    }

    /// Get a local as a `CompactValue` without the `Value` enum round-trip
    /// (T10.9.D hot-path).  Bounds-safe: out-of-range returns
    /// `CompactValue::uninitialized()` to preserve prior `get_local` semantics.
    #[inline(always)]
    pub fn get_local_compact(&self, index: u16) -> CompactValue {
        let i = index as usize;
        if i >= self.locals.len() {
            return CompactValue::uninitialized();
        }
        self.locals[i]
    }

    /// Set a local from a `CompactValue` (T10.9.D hot-path).  Silently no-ops
    /// on out-of-range index to mirror `set_local`.
    #[inline(always)]
    pub fn set_local_compact(&mut self, index: u16, cv: CompactValue) {
        let i = index as usize;
        if i < self.locals.len() {
            self.note_local_write();
            self.locals[i] = cv;
            // Keep legacy behavior for the hot-path int/float/reference
            // setters, but preserve the explicit long-tagged compact path for
            // NaN-box collision cases where a compact value is genuinely a
            // primitive `long`.
            let k = lkind_of_compact(&cv);
            self.local_kinds[i] = k;
            self.invalidate_cat2_upper_half(i, k);
        }
    }

    /// Set a local from a `CompactValue` without bounds check (fast path).
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn set_local_compact_unchecked(&mut self, index: usize, cv: CompactValue) {
        self.note_local_write();
        self.locals[index] = cv;
        // Preserve the compact-tagged category-2 path for NaN-box collisions
        // while keeping the existing int/float/reference fast path.
        let k = lkind_of_compact(&cv);
        self.local_kinds[index] = k;
        self.invalidate_cat2_upper_half(index, k);
    }

    /// Get a local as a `CompactValue` without bounds check (fast path).
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    #[inline(always)]
    pub fn get_local_compact_unchecked(&self, index: usize) -> CompactValue {
        self.locals[index]
    }

    #[inline(always)]
    fn note_local_write(&mut self) {
        if self.seq != 0 {
            self.exec_epoch = self.exec_epoch.wrapping_add(1);
        }
    }

    // ── Continuation freeze/thaw ─────────────────��───────────────────

    /// Clone the exception table Arc (cold path — for continuation freeze).
    pub fn exception_table_arc(&self) -> Arc<[ExceptionTableEntry]> {
        match &self.inner {
            FrameInner::Owned(o) => o.exception_table.clone(),
            FrameInner::Cached(cm) => cm.exception_table.clone(),
        }
    }

    /// Snapshot local variable raw values (for continuation freeze).
    ///
    /// Locals are stored inline as `CompactValue`, but the public snapshot
    /// shape is preserved as `(Vec<u64>, Vec<u8>)` for compatibility with
    /// `FrozenFrame` and external callers. Each slot is decomposed back into
    /// the legacy (bits, vtag) pair via `compact_to_local_slot`.
    pub fn locals_snapshot(&self) -> (Vec<u64>, Vec<u8>) {
        let n = self.locals.len();
        let mut vals = Vec::with_capacity(n);
        let mut tags = Vec::with_capacity(n);
        for (i, cv) in self.locals.iter().enumerate() {
            // Honor the kind mark so a long/double whose bits collide with the
            // NaN-tag space round-trips bit-exact: `compact_to_local_slot`
            // would otherwise tag a `0xfffd_…` collision long as VTAG_OBJECT
            // and emit only its low 47 bits, losing the value across thaw.
            let (v, t) = match self.local_kinds[i] {
                LKIND_LONG => (cv.raw_bits(), VTAG_LONG),
                LKIND_DOUBLE => (cv.raw_bits(), VTAG_DOUBLE),
                _ => compact_to_local_slot(*cv),
            };
            vals.push(v);
            tags.push(t);
        }
        (vals, tags)
    }

    /// Freeze this frame into a `FrozenFrame` that can be stored in a continuation.
    /// Captures all state needed to reconstruct the frame later.
    pub fn to_frozen_frame(&self) -> crate::threading::virtual_threads::FrozenFrame {
        let (stack_vals, stack_tags) = self.stack.snapshot_raw();
        let (locals_vals, locals_tags) = self.locals_snapshot();
        crate::threading::virtual_threads::FrozenFrame {
            class_name: self.class_name().to_string(),
            method_name: self.method_name().to_string(),
            descriptor: self.method_descriptor().to_string(),
            bytecode_pc: self.pc,
            locals: locals_vals,
            local_tags: locals_tags,
            stack: stack_vals,
            stack_tags,
            // Restoration metadata
            code: Some(self.code.clone()),
            class_id: Some(self.class_id),
            max_stack: Some(self.max_stack),
            max_locals: Some(self.max_locals),
            exception_table: Some(self.exception_table_arc()),
            source_file: self.source_file().map(|s| s.to_string()),
        }
    }

    /// Restore a Frame from a `FrozenFrame` (continuation thaw).
    /// The FrozenFrame must have been produced by `to_frozen_frame()` (i.e. have
    /// restoration metadata).
    ///
    /// # Panics
    /// Panics if the FrozenFrame lacks restoration metadata (code, class_id, etc.).
    pub fn from_frozen_frame(frozen: crate::threading::virtual_threads::FrozenFrame) -> Self {
        let code = frozen.code.expect("FrozenFrame missing code for thaw");
        let class_id = frozen
            .class_id
            .expect("FrozenFrame missing class_id for thaw");
        let max_stack = frozen
            .max_stack
            .expect("FrozenFrame missing max_stack for thaw");
        let max_locals = frozen
            .max_locals
            .expect("FrozenFrame missing max_locals for thaw");
        let exception_table = frozen
            .exception_table
            .expect("FrozenFrame missing exception_table for thaw");

        let stack = ValueStack::from_snapshot(frozen.stack, frozen.stack_tags, max_stack as usize);

        // Rebuild the inline-CompactValue locals from the legacy (bits, vtag)
        // pair carried in the frozen frame.
        let n = frozen.locals.len();
        debug_assert_eq!(
            n,
            frozen.local_tags.len(),
            "FrozenFrame locals/local_tags length mismatch on thaw"
        );
        let mut locals = Vec::with_capacity(n);
        let mut local_kinds = Vec::with_capacity(n);
        for i in 0..n {
            let tag = frozen.local_tags.get(i).copied().unwrap_or(VTAG_UNINIT);
            let val = frozen.locals[i];
            locals.push(local_slot_to_compact(val, tag));
            // Preserve the cat-2 primitive mark across thaw so the GC root
            // scan keeps skipping a long/double whose bits collide with
            // SUB_OBJECT (symmetric with `locals_snapshot`).
            local_kinds.push(match tag {
                VTAG_LONG => LKIND_LONG,
                VTAG_DOUBLE => LKIND_DOUBLE,
                _ => LKIND_OTHER,
            });
        }

        Self {
            class_id,
            pc: frozen.bytecode_pc,
            last_instr_pc: frozen.bytecode_pc,
            locals,
            local_kinds,
            stack,
            code,
            max_stack,
            max_locals,
            inner: {
                count_frame_kind(true);
                FrameInner::Owned(Box::new(OwnedFrameMeta {
                    class_name: Arc::from(frozen.class_name.as_str()),
                    method_name: Arc::from(frozen.method_name.as_str()),
                    method_descriptor: Arc::from(frozen.descriptor.as_str()),
                    source_file: frozen.source_file.map(|s| Arc::from(s.as_str())),
                    exception_table,
                }))
            },
            // CR-CLO-2 — explicit, not incidental. `FrozenFrame` carries no
            // method slot (it round-trips names and a descriptor, and lives in
            // `threading/virtual_threads.rs`), so a thawed frame has no
            // trustworthy index. Spelling `None` here rather than relying on
            // the field's absence from `FrozenFrame` means that if the frozen
            // shape ever gains an index, this site has to make a deliberate
            // decision about re-verifying it against the class as it exists on
            // the *resuming* side — a continuation can be thawed long after a
            // redefinition reordered the overload set it was captured from,
            // and the name re-check in `resolve_line_numbers_in_place` cannot
            // catch that reordering.
            method_index: None,
            backward_count: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            seq: next_frame_seq(),
            exec_epoch: 0,
        }
    }

    // ── GC scanning and pointer update helpers ─────────────────────────

    /// Collect all non-null Object references from locals for GC root scanning.
    ///
    /// **Spring Boot SEGV fix (2026-05-15):** Previously this function also treated
    /// any `VTAG_LONG` local whose bits happened to look like an aligned object
    /// pointer (`jlong_bits_as_aligned_object_ptr`) as a root. That heuristic was
    /// added as a backstop for JIT/native bridges that occasionally smuggled
    /// `jobject` handles through `Value::Long`, but it is **not safe in the
    /// interpreter's local-variable scan**: by JVM spec a `VTAG_LONG` slot is a
    /// primitive `long`, never a heap reference. Locals are tag-typed via
    /// `astore`/`lstore` and `coerce_value_for_return` already promotes any
    /// smuggled jobject to `VTAG_OBJECT` before it reaches a local slot.
    ///
    /// **BC SM2 fix (2026-05-28):** `CompactValue::long` now stores longs
    /// bit-exact (no lossy re-tag), so a long whose natural sub-tag bits
    /// happen to be `SUB_OBJECT` (bits 49-47 = 010, e.g. the BC LongArray
    /// `0xfffd_…` regression) will satisfy `is_object()`. The
    /// `heap.is_object_address` filter below drops those spurious roots
    /// before they reach the GC's mark phase. Real object slots pass the
    /// filter unchanged; long-bit-patterns whose lower 47 bits don't point
    /// at a live heap object are correctly excluded.
    pub fn scan_local_objects(&self, roots: &mut Vec<ObjectRef>, heap: &crate::memory::VmHeap) {
        self.scan_local_objects_inner(roots, heap, true);
    }

    /// The per-bci live-local mask [`Self::scan_local_objects`] filters this
    /// frame's roots with, at its current pc. Bit `i` set = slot `i` may still
    /// be read; slots at index >= 64 are outside the mask and always live.
    ///
    /// Exposed so a diagnostic can ask the collector's own question instead of
    /// a weaker one. `audit_frames_for_reclaimed_slots` needs exactly this:
    /// a frame local that is DEAD and points into a reclaimed span is the
    /// liveness filter working as designed (the `PreparedStatement` a seed loop
    /// finished with, still in slot 8 for the rest of the method), and
    /// reporting it would bury the case that matters — a LIVE local whose
    /// object the collector took anyway.
    /// The `local_kinds` mark for slot `idx` — `LKIND_LONG` / `LKIND_DOUBLE`
    /// mean `Frame::scan_local_objects` SKIPS the slot outright, because a
    /// primitive `long` whose NaN-boxed bits collide with the object sub-tag
    /// must never be rooted or remapped.
    ///
    /// Exposed for the stale-address reporter: a live object local that the
    /// root snapshot does not contain is either liveness-filtered or
    /// kind-filtered, and those two have completely different fixes. `aload`
    /// does not consult this mark, so a slot whose kind byte says LONG while
    /// its value is a genuine reference reads back fine from bytecode and is
    /// invisible to every root scan — exactly the shape of a live local that
    /// was reclaimed.
    pub(crate) fn local_kind_at(&self, idx: usize) -> u8 {
        self.local_kinds.get(idx).copied().unwrap_or(u8::MAX)
    }

    pub(crate) fn live_locals_mask_here(&self) -> u64 {
        if crate::runtime::env_cache::no_local_liveness() {
            crate::runtime::local_liveness::ALL_LIVE
        } else {
            crate::runtime::local_liveness::live_locals_mask(
                &self.code,
                self.exception_table(),
                self.max_locals,
                [self.pc, self.last_instr_pc],
            )
        }
    }

    /// Variant used by the non-moving ForkJoin stress snapshot path.
    ///
    /// It keeps the exact same kind/tag filtering as [`Self::scan_local_objects`]
    /// but treats every non-primitive local as live. This is only sound for the
    /// non-moving selective-promotion collector, where an extra dead reference
    /// can only over-retain/pin; the moving collector must continue using the
    /// liveness-filtered scan to avoid retaining scoped-out references for whole
    /// frame lifetimes.
    pub fn scan_local_objects_all_live(
        &self,
        roots: &mut Vec<ObjectRef>,
        heap: &crate::memory::VmHeap,
    ) {
        self.scan_local_objects_inner(roots, heap, false);
    }

    fn scan_local_objects_inner(
        &self,
        roots: &mut Vec<ObjectRef>,
        heap: &crate::memory::VmHeap,
        honor_liveness: bool,
    ) {
        // Per-bci local liveness (HotSpot interpreter-oop-map equivalent): a
        // slot whose value can never be read again under bytecode semantics
        // is NOT a root. Without this, a scoped-out local (e.g. a loop
        // construction temp) retains its last referent for the frame's whole
        // lifetime — unbounded retention on linked structures (the
        // SteadyChurn `node[4095]` anchor, `CRATONVM_G1_DBG_ROOTCENSUS=1`).
        // The analysis is conservative (all-live on anything it cannot fully
        // model), both the current and last-started instruction pcs are
        // unioned, and slot 0 is always kept. Opt out with
        // `CRATONVM_NO_LOCAL_LIVENESS=1`. The NON-MOVING conservative scan
        // (`scan_locals_conservative`) is intentionally unfiltered.
        let live_mask = if !honor_liveness || crate::runtime::env_cache::no_local_liveness() {
            crate::runtime::local_liveness::ALL_LIVE
        } else {
            crate::runtime::local_liveness::live_locals_mask(
                &self.code,
                self.exception_table(),
                self.max_locals,
                [self.pc, self.last_instr_pc],
            )
        };
        for (i, cv) in self.locals.iter().enumerate() {
            if i < 64 && live_mask & (1u64 << i) == 0 {
                // `CRATONVM_DBG_VACATED_FRAMES`: remember what the filter
                // dropped. Its contract is that this slot can never be read
                // again; if the address turns up later as a failing receiver,
                // that contract was broken for this exact method and slot.
                if cratonvm_gc::gc_quiescence::vacated_frames_enabled()
                    && self.local_kinds[i] != LKIND_LONG
                    && self.local_kinds[i] != LKIND_DOUBLE
                {
                    if cv.is_object() {
                        if let Some(ptr) = cv.as_object_ptr() {
                            cratonvm_gc::gc_quiescence::note_liveness_filtered(
                                ptr as usize,
                                || {
                                    format!(
                                        "{}.{} pc={} local[{}]",
                                        self.class_name(),
                                        self.method_name(),
                                        self.pc,
                                        i
                                    )
                                },
                            );
                        }
                    }
                }
                continue;
            }
            // A primitive `long` / `double` is never a heap reference — not
            // even when its NaN-boxed bits collide with the SUB_OBJECT tag
            // (BC F2m `LongArray` `0xfffd_…` words). The `is_heap_addr` filter
            // below cannot reject a collision long whose low 47 bits happen to
            // land on a live object, so without this `local_kinds` gate the GC
            // would root it and then relocate it, corrupting the long and the
            // object graph. This is the locals-side counterpart to
            // `ValueStack::scan_object_refs` honoring its `kinds` marks.
            if self.local_kinds[i] == LKIND_LONG || self.local_kinds[i] == LKIND_DOUBLE {
                continue;
            }
            if cv.is_object() {
                if let Some(ptr) = cv.as_object_ptr() {
                    // Gate on region-membership (`is_heap_addr`), NOT a header
                    // probe (`is_object_address`). A freshly-allocated young /
                    // mid-init object whose header `is_object_address` cannot yet
                    // vouch for (kind not finalized / not in the "live" half) is
                    // a GENUINE root held in a local — dropping it lets the
                    // collector reclaim a still-referenced object, which then
                    // surfaces as a stale all-zero-header receiver on the next
                    // use (observed: BouncyCastle `EC5Util.getCurve` LOCAL[5]
                    // holding a just-fetched `X9ECParameters` named-curve param
                    // collected mid-method under GC pressure → "Cannot invoke
                    // getCurve on null"). `is_heap_addr` still rejects long-bit
                    // false positives whose payload isn't an aligned in-region
                    // address. This mirrors the operand-stack root scan fix in
                    // `ValueStack::scan_object_refs`.
                    if ptr != 0 && heap.is_heap_addr(ptr as usize).is_some() {
                        roots.push(unsafe { ObjectRef::from_raw(ptr as *mut u8) });
                    }
                }
            } else {
                // Moving-GC lost-tag hardening: a reference-typed local can
                // transiently carry a raw object address under a non-object
                // CompactValue tag at a safepoint. Primitive long/double
                // locals were excluded above; use the strict header probe so
                // this fallback only roots real live objects.
                for candidate in lost_tag_local_candidates(*cv) {
                    if candidate == 0 {
                        continue;
                    }
                    if let Some(obj) = heap.is_object_address(candidate as usize) {
                        roots.push(obj);
                        break;
                    }
                }
            }
        }
    }

    /// Conservative local scan for the NON-MOVING sweep (gc_quiescence active).
    ///
    /// `scan_local_objects` is *tag-filtered*: it skips `LKIND_LONG`/`_DOUBLE`
    /// slots and only roots `is_object()` CompactValues. That is correct for the
    /// MOVING collector (a false-positive root would be relocated, corrupting a
    /// primitive `long` that merely looks like a pointer). But it MISSES a
    /// genuine object reference whose slot tag was lost — e.g. a JIT-compiled
    /// callee's object return value mis-tagged on the transition back to the
    /// interpreter, leaving `main`'s `f = POOL.submit(t)` local holding the
    /// ForkJoinTask under a non-object tag. The tag-filtered scan then omits it,
    /// so selective promotion (which pins by root VALUE) does not pin it,
    /// evacuates it, zeroes the young slot, and the stale local reads an
    /// all-zero header (the Fork6 multi-thread reclamation).
    ///
    /// This conservative variant additionally probes EVERY local's pointer-shaped
    /// candidates (the object-ptr decode, the `long` payload, and the raw bits)
    /// with the STRICT `is_object_address` header probe — only a slot that
    /// actually lands on a live object header is rooted. It is sound ONLY under
    /// the non-moving sweep: nothing is relocated, so a false-positive root can
    /// only over-retain (and over-pin against evacuation) — never corrupt a
    /// primitive. Callers MUST gate this on `gc_quiescence::is_active()`.
    pub fn scan_locals_conservative(
        &self,
        roots: &mut Vec<ObjectRef>,
        heap: &crate::memory::VmHeap,
    ) {
        for cv in self.locals.iter() {
            // Three pointer candidates covering the encodings a lost-tag object
            // ref can take: a properly object-tagged ptr, a `long`-tagged
            // payload, and the raw NaN-box bits (an untagged raw store).
            let cands = [
                cv.as_object_ptr().unwrap_or(0),
                cv.as_long().map(|l| l as u64).unwrap_or(0),
                cv.raw_bits(),
            ];
            for c in cands {
                if c != 0 {
                    if let Some(obj) = heap.is_object_address(c as usize) {
                        roots.push(obj);
                        break;
                    }
                }
            }
        }
    }

    /// Update Object references in locals after GC using the pointer map.
    ///
    /// Symmetric with [`Self::scan_local_objects`]: only verified heap-resident
    /// slots are remapped. `VTAG_LONG` slots are primitive `long`s by spec and
    /// must never be remapped by GC — even when a long's bit pattern
    /// coincidentally looks like `SUB_OBJECT` after the lossless verbatim
    /// encoding (BC SM2 fix, 2026-05-28). The `heap.is_object_address` check
    /// rejects long bit patterns whose lower 47 bits don't point at a live
    /// heap object, leaving the long value intact.
    pub fn update_local_refs(
        &mut self,
        pointer_map: &cratonvm_types::PointerMap,
        heap: &crate::memory::VmHeap,
    ) {
        let _ = heap;
        // Indexed loop so the per-slot `local_kinds` mark can be consulted
        // without aliasing the `&mut self.locals` borrow.
        for i in 0..self.locals.len() {
            // BUG-03 diag (gated): for any slot whose address is in this GC's
            // pointer_map, log the kind/tag/decision — captures the exact slot
            // (Thread.<init> local[7] = `parent`) that the remap skips.
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BUG03").is_some() {
                let cv = self.locals[i];
                let obj = cv.as_object_ptr().map(|p| p as usize);
                let raw = cv.raw_bits() as usize;
                let in_map = obj.map(|a| pointer_map.contains_key(&a)).unwrap_or(false)
                    || pointer_map.contains_key(&(raw & 0x7fff_ffff_ffff));
                if in_map {
                    eprintln!(
                        "[BUG03-ulr] {}.{} local[{}] kind={} is_object={} obj={:?} raw=0x{:x} skip_kind={} skip_nonobj={}",
                        self.class_name(), self.method_name(), i, self.local_kinds[i],
                        cv.is_object(), obj, raw,
                        self.local_kinds[i] == LKIND_LONG || self.local_kinds[i] == LKIND_DOUBLE,
                        !cv.is_object(),
                    );
                }
            }
            // Never remap a primitive `long` / `double` slot. The `pointer_map`
            // is the *global* relocation record, so a collision long whose low
            // 47 bits happen to equal some unrelated object's from-space
            // address WOULD match a key here — rewriting it to the moved
            // address silently corrupts the long's value. A primitive is a
            // value, not a pointer, and must be preserved verbatim. This is
            // the missing symmetric guard for `scan_local_objects`'s kind gate
            // (the prior comment's "a false positive was never rooted so it
            // can't be a key" was wrong: rooting and remapping key off
            // *different* objects).
            if self.local_kinds[i] == LKIND_LONG || self.local_kinds[i] == LKIND_DOUBLE {
                // bc math-ec 0x4 smear hunt (CRATONVM_DBG_STALELONG): a LONG-
                // kind local whose raw bits equal a pointer_map KEY is either a
                // collision long (benign — must NOT be remapped) or a SMUGGLED
                // OBJECT REF stored as a long (jobject-as-Long contract) — which
                // this skip leaves STALE after the move. The frame's method name
                // discriminates: LongArray/BigInteger methods holding [J/[I args
                // as LONG-kind locals are the smear suspects.
                if stalelong_enabled() {
                    let raw = self.locals[i].raw_bits() as usize;
                    if let Some(&new_addr) = pointer_map.get(&raw) {
                        use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
                        static N: AtomicUsize = AtomicUsize::new(0);
                        let k = N.fetch_add(1, AOrd::Relaxed);
                        if k < 24 {
                            eprintln!(
                                "[stalelong] #{k} LONG-kind local[{i}] bits=0x{raw:x} MATCHES moved obj (-> 0x{new_addr:x}) in {}.{}{} — NOT remapped (stale if smuggled ref)",
                                self.class_name(),
                                self.method_name(),
                                self.method_descriptor(),
                            );
                        }
                    }
                }
                continue;
            }
            let cv = self.locals[i];
            if cv.is_object() {
                let Some(old_ptr) = cv.as_object_ptr() else {
                    continue;
                };
                // Gate on `pointer_map` membership, NOT a live-header probe of
                // `old_ptr`. After a moving GC, `old_ptr` is the *from-space*
                // address, whose header has already been zeroed/reclaimed — so
                // `heap.is_object_address(old_ptr)` returns `None` for precisely
                // the slots that were relocated and need remapping, leaving them
                // dangling. That was the H2 `TestAll` `System.gc()` crash:
                // POST-GC STALE LOCAL + ZERO-HEADER on `Utils.collectGarbage` /
                // `TestAll.*` frames, cascading into the OOBFIELD `class_id=0`
                // probe, an IllegalMonitorStateException on frame pop, and a final
                // NPE. The `pointer_map` is the authoritative record of
                // relocations and is exactly the criterion `verify_no_stale_refs`
                // uses, so remap iff the slot's address is a key.
                if let Some(&new_addr) = pointer_map.get(&(old_ptr as usize)) {
                    // Record construction provenance for the relocated address
                    // (mirrors `ValueStack::update_object_refs`): writing the
                    // raw bits via `try_from_pointer` does not by itself mark
                    // `new_addr` as a known live-object payload, so a later
                    // context-free decode of this local (`to_value` /
                    // `decode_value`) would find `object_ref_payload_is_known`
                    // false and degrade a live, moved reference to Long/null.
                    // The `from_raw` round-trip is the canonical recorder.
                    // SAFETY: `new_addr` is a live moved object's address from
                    // the GC pointer map; only its bits are used.
                    let _ = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                    self.locals[i] = CompactValue::try_from_pointer(new_addr as u64)
                        .unwrap_or_else(CompactValue::null);
                }
            } else {
                for candidate in lost_tag_local_candidates(cv) {
                    if candidate == 0 {
                        continue;
                    }
                    let Some(&new_addr) = pointer_map.get(&(candidate as usize)) else {
                        continue;
                    };
                    if heap.is_object_address(new_addr).is_some() {
                        // Same provenance fix as above for the lost-tag path.
                        // SAFETY: `new_addr` is a live moved object's address,
                        // confirmed by `heap.is_object_address` just above.
                        let _ = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                        self.locals[i] = CompactValue::try_from_pointer(new_addr as u64)
                            .unwrap_or_else(CompactValue::null);
                    }
                    break;
                }
            }
        }
    }

    /// BUG-03 debug: dump this frame's operand stack (kind/tag/addr per slot).
    #[doc(hidden)]
    pub fn dbg_stack_dump(&self) -> String {
        self.stack.dbg_dump()
    }

    /// BUG-03 debug: locate `addr` among this frame's locals/operand stack and
    /// report the slot + its kind tag. Used to find a mis-tagged object slot that
    /// the kind-gated remap skips (the concurrent-spawn stale-`parent` root cause).
    #[doc(hidden)]
    pub fn dbg_locate_addr(&self, addr: usize) -> Option<String> {
        let mask = 0x7fff_ffff_ffffusize;
        for i in 0..self.locals.len() {
            let obj = self.locals[i].as_object_ptr().map(|p| p as usize);
            let raw = self.locals[i].raw_bits() as usize;
            if obj == Some(addr) || (raw & mask) == (addr & mask) {
                return Some(format!(
                    "local[{}] kind={} is_object={} obj_match={}",
                    i,
                    self.local_kinds[i],
                    self.locals[i].is_object(),
                    obj == Some(addr)
                ));
            }
        }
        self.stack
            .dbg_locate_addr(addr)
            .map(|s| format!("operand-{s}"))
    }
}

// ===========================================================================
// FrameStack — the Java call stack with STABLE frame addresses
// ===========================================================================

/// Frames reserved the first time a thread pushes anything.
///
/// Sized so that essentially every real Java stack fits without a single
/// relocation, while costing a bounded amount for a thread that only ever runs
/// a shallow stack (`256 * size_of::<Frame>()`, a few tens of KiB) — and
/// nothing at all for a thread that never executes bytecode, since
/// [`FrameStack::new`] does not allocate.
pub const FRAME_STACK_INITIAL_STABLE_CAP: usize = 256;

/// The Java call stack of one thread: `Vec<Frame>` semantics, plus an explicit
/// **address-stability contract**.
///
/// # Why this type exists
///
/// The backing store used to be a plain `Vec<Frame>`. Because `Vec::push`
/// reallocates when it runs out of capacity — moving *every* live frame — no
/// `&mut Frame` (nor any raw `*mut Frame`) could be held across a call. The
/// interpreter therefore had to re-index `thread.frames[frame_idx]` on every
/// single access: 622 occurrences in `runtime/interpreter.rs`, six of them in
/// the one dispatch site that executes a single bytecode. Each is a bounds
/// check plus an `imul` by `size_of::<Frame>()` (which is not a power of two)
/// plus a pointer chase.
///
/// `FrameStack` makes it possible to hoist that: capacity is reserved in
/// advance and growth is an explicit, observable event.
///
/// # Address-stability contract
///
/// * A frame's address never changes while it is on the stack **unless** the
///   backing buffer grows.
/// * A growth is the *only* thing that moves frames, it happens only inside
///   [`FrameStack::reserve_stable`] (which [`FrameStack::push`] calls), and it
///   always increments [`FrameStack::reloc_epoch`].
/// * `reserve_stable(n)` guarantees the next `n` pushes cannot grow, hence
///   cannot move anything. [`FrameStack::stable_headroom`] reports how many
///   pushes are currently guaranteed relocation-free.
/// * Popping never moves a frame; neither does [`FrameStack::truncate`].
///
/// So the pattern the interpreter should adopt is:
///
/// ```ignore
/// thread.frames.reserve_stable(1);      // next push cannot relocate
/// let epoch = thread.frames.reloc_epoch();
/// let f: *mut Frame = thread.frames.frame_ptr(frame_idx);
/// // … push a callee frame, run it, pop it …
/// debug_assert_eq!(epoch, thread.frames.reloc_epoch());
/// let f: &mut Frame = unsafe { &mut *f };   // still valid
/// ```
///
/// # Aliasing rules for the raw-pointer API
///
/// Address stability is *not* the same as aliasing permission. While a
/// `*mut Frame` obtained from [`FrameStack::frame_ptr`] is in use:
///
/// 1. No `&Frame` / `&mut Frame` to the *same* frame may be live — that
///    includes `thread.frames[idx]`, `.last()`, `.iter()`, and the `&[Frame]`
///    view produced by `Deref` (so no `capture_full_trace(&thread.frames)`
///    while a frame reference is held). Other frames are unaffected.
/// 2. The pointer must be re-derived (`frame_ptr`) after any `&mut` reborrow
///    of the `FrameStack` itself, for strict provenance. The *address* is
///    unchanged, so the re-derivation is one load plus one add and still
///    removes the bounds check and the `imul`; caching it across a whole
///    dispatch iteration is the win.
/// 3. `reloc_epoch()` must be unchanged since the pointer was taken.
/// 4. The frame must not have been popped (`idx < len()`).
///
/// # Drop-in surface
///
/// Every method the rest of the VM already calls on `thread.frames` is
/// provided inherently (`len`, `is_empty`, `iter`, `iter_mut`, `push`, `pop`,
/// `last`, `last_mut`, `get`, `get_mut`, `truncate`, `clear`, `insert`), plus
/// `Index`/`IndexMut` over any slice index, `Deref`/`DerefMut` to `[Frame]`
/// (so `&thread.frames` still coerces to `&[Frame]` for
/// `stackwalker::capture_full_trace`), and `IntoIterator` for `&`/`&mut`
/// (so `for f in &thread.frames` keeps working).
pub struct FrameStack {
    /// Backing storage. Never reallocated except through `grow_for`, which
    /// bumps `reloc_epoch`.
    buf: Vec<Frame>,
    /// Logical top of stack. `buf.len()` is the **high-water mark**: the
    /// slots in `depth..buf.len()` hold retired frames whose `locals`,
    /// `local_kinds` and operand-stack buffers are kept allocated, so the
    /// next call at that depth is built in them instead of routing a fresh
    /// set through the thread's pools.
    ///
    /// Nothing above `depth` is live: every accessor on this type is
    /// bounded by it, so the GC root scan, the stack walker and every
    /// `frames[i]` see exactly the frames they saw before. A retired
    /// frame's leftover contents are overwritten by
    /// [`Frame::reset_cached_compact`] before the slot becomes live again.
    depth: usize,
    /// Incremented every time the backing buffer is reallocated with live
    /// frames in it — i.e. every time frame addresses change. Wrapping is
    /// harmless: consumers only ever compare for equality across a short
    /// window, and a wrap needs 2^32 relocations.
    reloc_epoch: u32,
}

impl FrameStack {
    /// An empty stack that has not allocated. Matches the old
    /// `frames: Vec::new()` cost for threads that never run bytecode.
    pub const fn new() -> Self {
        Self {
            buf: Vec::new(),
            depth: 0,
            reloc_epoch: 0,
        }
    }

    // ── Address-stability API (the point of this type) ──────────────────

    /// Relocation generation. Any cached `*mut Frame` is invalidated when this
    /// changes; nothing else invalidates it except popping the frame.
    #[inline(always)]
    pub fn reloc_epoch(&self) -> u32 {
        self.reloc_epoch
    }

    /// How many more frames can be pushed with **no** frame moving.
    #[inline(always)]
    pub fn stable_headroom(&self) -> usize {
        // Measured against the LIVE depth, not the allocation: a push
        // overwrites a retired slot before it appends, so a retired slot is
        // headroom.
        self.buf.capacity() - self.depth
    }

    /// Guarantee that the next `additional` pushes will not move any frame.
    ///
    /// If a growth is required it happens *now* (bumping
    /// [`Self::reloc_epoch`]) rather than in the middle of a push, which is
    /// what lets a caller take a frame pointer *after* this call and keep it
    /// across the pushes.
    #[inline(always)]
    pub fn reserve_stable(&mut self, additional: usize) {
        if self.depth.saturating_add(additional) > self.buf.capacity() {
            self.grow_for(additional);
        }
    }

    /// Grow the backing buffer. Cold: after the initial reservation this runs
    /// at most a handful of times over a thread's entire life (the capacity
    /// doubles), and never at all for a stack that stays under
    /// [`FRAME_STACK_INITIAL_STABLE_CAP`].
    #[cold]
    #[inline(never)]
    fn grow_for(&mut self, additional: usize) {
        let len = self.buf.len();
        let cap = self.buf.capacity();
        let needed = self.depth.saturating_add(additional);
        let target = if cap == 0 {
            // No frames exist yet, so nothing can move: no epoch bump.
            needed.max(FRAME_STACK_INITIAL_STABLE_CAP)
        } else {
            // Live frames are about to be memcpy'd to a new allocation.
            self.reloc_epoch = self.reloc_epoch.wrapping_add(1);
            needed.max(cap.saturating_mul(2))
        };
        self.buf.reserve_exact(target - len);
    }

    /// Raw pointer to frame `idx`.
    ///
    /// Returns a null pointer for an out-of-range `idx` rather than panicking,
    /// so a caller can check once instead of paying a bounds check per use.
    ///
    /// # Safety of the *returned pointer*
    ///
    /// Dereferencing it is `unsafe` and subject to the aliasing rules in the
    /// type-level docs. The pointer itself is always safe to obtain.
    #[inline(always)]
    pub fn frame_ptr(&mut self, idx: usize) -> *mut Frame {
        if idx < self.depth {
            // SAFETY: idx is in bounds, so the offset is inside the allocation.
            unsafe { self.buf.as_mut_ptr().add(idx) }
        } else {
            std::ptr::null_mut()
        }
    }

    /// Raw pointer to the top frame, or null when the stack is empty.
    #[inline(always)]
    pub fn current_ptr(&mut self) -> *mut Frame {
        let len = self.depth;
        if len == 0 {
            std::ptr::null_mut()
        } else {
            // SAFETY: len > 0, so len - 1 is in bounds.
            unsafe { self.buf.as_mut_ptr().add(len - 1) }
        }
    }

    /// Base pointer of the backing buffer. Only valid up to `len()`.
    #[inline(always)]
    pub fn as_mut_ptr(&mut self) -> *mut Frame {
        self.buf.as_mut_ptr()
    }

    /// Safe `&mut` to the currently executing (top) frame.
    ///
    /// This is the *safe* half of the hoisting API: it borrows the stack, so
    /// it cannot be held across a push — use [`Self::current_ptr`] plus
    /// [`Self::reserve_stable`] for that.
    #[inline(always)]
    pub fn current_mut(&mut self) -> Option<&mut Frame> {
        self.live_mut().last_mut()
    }

    // ── Drop-in `Vec<Frame>` surface ────────────────────────────────────

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.depth
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.depth == 0
    }

    #[inline(always)]
    pub fn iter(&self) -> std::slice::Iter<'_, Frame> {
        self.live().iter()
    }

    #[inline(always)]
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Frame> {
        self.live_mut().iter_mut()
    }

    #[inline(always)]
    pub fn first(&self) -> Option<&Frame> {
        self.live().first()
    }

    #[inline(always)]
    pub fn last(&self) -> Option<&Frame> {
        self.live().last()
    }

    #[inline(always)]
    pub fn last_mut(&mut self) -> Option<&mut Frame> {
        self.live_mut().last_mut()
    }

    #[inline(always)]
    pub fn get(&self, idx: usize) -> Option<&Frame> {
        self.live().get(idx)
    }

    #[inline(always)]
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut Frame> {
        self.live_mut().get_mut(idx)
    }

    #[inline(always)]
    pub fn as_slice(&self) -> &[Frame] {
        self.live()
    }

    #[inline(always)]
    pub fn as_mut_slice(&mut self) -> &mut [Frame] {
        self.live_mut()
    }

    /// Push a frame. Existing frames keep their addresses unless this call has
    /// to grow the buffer — see [`Self::reserve_stable`] to rule that out.
    #[inline(always)]
    pub fn push(&mut self, frame: Frame) {
        // `CRATONVM_DBG_INTERP_FRAMES=1` — the census of what actually runs
        // interpreted. One relaxed load when unarmed; see
        // `crate::runtime::interp_census`.
        if crate::runtime::interp_census::interp_frames_enabled() {
            crate::runtime::interp_census::record_interp_frame(
                frame.class_name(),
                frame.method_name(),
                frame.method_descriptor(),
            );
        }
        self.reserve_stable(1);
        if self.depth < self.buf.len() {
            // Overwrite a retired slot. Its buffers are dropped with it;
            // `push_frame_and_fire_entry` harvests them into the thread
            // pools first, so the pooled constructors keep the recycling
            // they have always relied on.
            self.buf[self.depth] = frame;
        } else {
            self.buf.push(frame);
        }
        self.depth += 1;
    }

    /// Pop the top frame. Never moves any remaining frame.
    #[inline(always)]
    pub fn pop(&mut self) -> Option<Frame> {
        if self.depth == 0 {
            return None;
        }
        // A by-value pop hands the frame out, so the retired tail above it
        // cannot stay behind: drop it and let the frame leave normally.
        self.buf.truncate(self.depth);
        self.depth -= 1;
        self.buf.pop()
    }

    /// Drop everything above depth `len`. Never moves any surviving frame.
    #[inline(always)]
    pub fn truncate(&mut self, len: usize) {
        if len < self.depth {
            self.depth = len;
        }
    }

    /// `truncate`, and release the retired slots above it in the same step.
    ///
    /// This is the shape the pooled recycle path wants: it has just harvested
    /// the top frame's buffers, so the husk must actually be dropped rather
    /// than retired with nothing in it.
    #[inline(always)]
    pub fn truncate_hard(&mut self, len: usize) {
        if len < self.depth {
            self.depth = len;
        }
        self.buf.truncate(self.depth);
    }

    /// Drop every frame. Retains the (stable) capacity.
    #[inline(always)]
    pub fn clear(&mut self) {
        self.depth = 0;
        self.buf.clear();
    }

    /// Insert at `idx`, shifting the frames above it.
    ///
    /// NOTE: this is the one operation that moves frames *without* a
    /// relocation-epoch bump, because the buffer itself does not move — the
    /// contents shift. It exists only for `Vec` parity; the Java call stack is
    /// strictly LIFO and nothing in the VM currently uses it. Any future
    /// caller must invalidate its own frame pointers.
    #[inline]
    pub fn insert(&mut self, idx: usize, frame: Frame) {
        self.reserve_stable(1);
        self.buf.truncate(self.depth);
        self.buf.insert(idx, frame);
        self.depth += 1;
    }
}

impl Default for FrameStack {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FrameStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameStack")
            .field("depth", &self.buf.len())
            .field("stable_capacity", &self.buf.capacity())
            .field("reloc_epoch", &self.reloc_epoch)
            .finish()
    }
}

impl FrameStack {
    /// The live frames, `0..depth`. Everything the rest of the VM can see.
    #[inline(always)]
    fn live(&self) -> &[Frame] {
        &self.buf[..self.depth]
    }

    #[inline(always)]
    fn live_mut(&mut self) -> &mut [Frame] {
        let d = self.depth;
        &mut self.buf[..d]
    }

    /// Retire the top frame without destroying it, keeping its buffers in the
    /// slot for the next call at this depth. The frame stops being live the
    /// instant `depth` drops.
    #[inline(always)]
    pub fn retire_top(&mut self) -> bool {
        if self.depth == 0 {
            return false;
        }
        self.depth -= 1;
        true
    }

    /// Whether a retired slot is waiting at the current depth.
    #[inline(always)]
    pub fn has_retired_slot(&self) -> bool {
        self.depth < self.buf.len()
    }

    /// The retired slot at the current depth, for harvesting its buffers
    /// before an ordinary by-value push overwrites it.
    #[inline(always)]
    pub fn retired_slot_mut(&mut self) -> Option<&mut Frame> {
        if self.depth < self.buf.len() {
            let d = self.depth;
            Some(&mut self.buf[d])
        } else {
            None
        }
    }

    /// Rebuild the retired slot at the current depth as a frame for `cached`
    /// and make it live, reusing its buffers. `false` when no slot is retired,
    /// and the caller builds a frame the ordinary way.
    #[inline]
    pub fn push_cached_compact_reusing(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        args: &[(CompactValue, u8)],
    ) -> bool {
        if self.depth >= self.buf.len() {
            return false;
        }
        // `CRATONVM_DBG_INTERP_FRAMES=1` — this is a frame push like any
        // other, and the census must see it. It is recorded here rather than
        // in `reset_cached_compact` because `FrameStack::push` records at the
        // same level, and because a reset that is not a push (tail call) is
        // not a new frame.
        if crate::runtime::interp_census::interp_frames_enabled() {
            crate::runtime::interp_census::record_interp_frame(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
        }
        let d = self.depth;
        self.buf[d].reset_cached_compact(cached, args);
        self.depth += 1;
        true
    }

    /// [`Self::push_cached_compact_reusing`] for the general dispatchers,
    /// whose arguments are still `Value`s.
    ///
    /// The two differ only in how the argument slots are laid down; the
    /// resulting locals are identical, which is what
    /// [`Frame::reset_cached_value`] and [`Frame::reset_cached_compact`]
    /// sharing [`Frame::reset_cached_tail`] pins.
    #[inline]
    pub fn push_cached_value_reusing(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        args: &[Value],
    ) -> bool {
        if self.depth >= self.buf.len() {
            return false;
        }
        if crate::runtime::interp_census::interp_frames_enabled() {
            crate::runtime::interp_census::record_interp_frame(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
        }
        let d = self.depth;
        self.buf[d].reset_cached_value(cached, args);
        self.depth += 1;
        true
    }


    /// Build a frame for `cached` **in** the stack's next slot.
    ///
    /// The counterpart to [`Self::push_cached_compact_reusing`] for the case
    /// where no slot is retired — the first call at a depth, and every
    /// by-value push, which harvests and trims the slot it would have reused.
    /// `push` receives a `Frame` that has already been built somewhere else
    /// and moves ~220 bytes into the slot; this writes the struct where it
    /// belongs, so the only things that travel are the three buffer handles
    /// the caller just took from the pools.
    ///
    /// The struct literal is written by `ptr::write` in this function, so the
    /// destination is known to the compiler at the point of construction and
    /// there is no intermediate to copy from.
    #[inline]
    pub fn emplace_cached_compact(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        locals: Vec<CompactValue>,
        local_kinds: Vec<u8>,
        stack: ValueStack,
        eff_max_locals: u16,
    ) {
        if crate::runtime::interp_census::interp_frames_enabled() {
            crate::runtime::interp_census::record_interp_frame(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
        }
        self.reserve_stable(1);
        if self.depth < self.buf.len() {
            // A retired slot is here after all (a caller that did not harvest).
            // Assigning drops it, which is exactly what `push` would have done.
            self.buf[self.depth] = Frame::from_cached_compact_parts(
                cached,
                locals,
                local_kinds,
                stack,
                eff_max_locals,
            );
        } else {
            let class_id = cached.declaring_class_id;
            let code = cached.code.clone();
            let max_stack = cached.max_stack;
            count_frame_kind(false);
            let seq = next_frame_seq();
            // SAFETY: `reserve_stable(1)` guarantees `capacity > buf.len()`,
            // and `depth == buf.len()` in this branch, so `dst` is the one
            // slot past the last initialised element and is valid for a write
            // of a `Frame`. `set_len` publishes it only after it is fully
            // initialised, and nothing in between can panic or observe it.
            unsafe {
                let dst = self.buf.as_mut_ptr().add(self.depth);
                std::ptr::write(
                    dst,
                    Frame {
                        class_id,
                        pc: 0,
                        last_instr_pc: 0,
                        locals,
                        local_kinds,
                        stack,
                        code,
                        max_stack,
                        max_locals: eff_max_locals,
                        inner: FrameInner::Cached(cached),
                        method_index: None,
                        backward_count: 0,
                        osr_attempt_counts: Vec::new(),
                        monitor_on_exit: None,
                        seq,
                        exec_epoch: 0,
                    },
                );
                self.buf.set_len(self.depth + 1);
            }
        }
        self.depth += 1;
    }

    /// Drop every retired slot, releasing their buffers.
    pub fn trim_retired(&mut self) {
        let d = self.depth;
        self.buf.truncate(d);
    }
}

impl std::ops::Deref for FrameStack {
    type Target = [Frame];
    #[inline(always)]
    fn deref(&self) -> &[Frame] {
        self.live()
    }
}

impl std::ops::DerefMut for FrameStack {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [Frame] {
        self.live_mut()
    }
}

impl<I: std::slice::SliceIndex<[Frame]>> std::ops::Index<I> for FrameStack {
    type Output = I::Output;
    #[inline(always)]
    fn index(&self, index: I) -> &Self::Output {
        std::ops::Index::index(self.live(), index)
    }
}

impl<I: std::slice::SliceIndex<[Frame]>> std::ops::IndexMut<I> for FrameStack {
    #[inline(always)]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        std::ops::IndexMut::index_mut(self.live_mut(), index)
    }
}

impl<'a> IntoIterator for &'a FrameStack {
    type Item = &'a Frame;
    type IntoIter = std::slice::Iter<'a, Frame>;
    #[inline(always)]
    fn into_iter(self) -> Self::IntoIter {
        self.live().iter()
    }
}

impl<'a> IntoIterator for &'a mut FrameStack {
    type Item = &'a mut Frame;
    type IntoIter = std::slice::IterMut<'a, Frame>;
    #[inline(always)]
    fn into_iter(self) -> Self::IntoIter {
        self.live_mut().iter_mut()
    }
}

impl IntoIterator for FrameStack {
    type Item = Frame;
    type IntoIter = std::vec::IntoIter<Frame>;
    #[inline(always)]
    fn into_iter(mut self) -> Self::IntoIter {
        // Retired slots are not frames anyone may see.
        self.trim_retired();
        self.buf.into_iter()
    }
}

impl FromIterator<Frame> for FrameStack {
    fn from_iter<T: IntoIterator<Item = Frame>>(iter: T) -> Self {
        let buf: Vec<Frame> = iter.into_iter().collect();
        let depth = buf.len();
        Self {
            buf,
            depth,
            reloc_epoch: 0,
        }
    }
}

impl From<Vec<Frame>> for FrameStack {
    #[inline]
    fn from(buf: Vec<Frame>) -> Self {
        let depth = buf.len();
        Self {
            buf,
            depth,
            reloc_epoch: 0,
        }
    }
}

impl From<FrameStack> for Vec<Frame> {
    #[inline]
    fn from(mut fs: FrameStack) -> Vec<Frame> {
        fs.trim_retired();
        fs.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the frame-push locals layout against the resize-then-overwrite form
    /// it replaced.
    ///
    /// `init_locals_from_parts` used to size both buffers to `max_locals` and
    /// then copy the arguments over the leading slots; it now pushes the
    /// arguments and resizes the remainder, so each slot is written once. The
    /// two must produce byte-identical buffers, or a frame starts life with the
    /// wrong local — which is a silent wrong value, and for a reference slot a
    /// GC-root question.
    ///
    /// Covers the three shapes that make the layouts differ: category-2
    /// arguments (two slots, upper one filler), a tail of unset locals, and the
    /// defensive clamp where the arguments alone would overrun `max_locals`.
    #[test]
    fn pushed_locals_match_the_resize_then_copy_layout() {
        fn reference(max_locals: u16, args: &[Value]) -> (Vec<CompactValue>, Vec<u8>) {
            let n = effective_max_locals(max_locals, args) as usize;
            let mut locals = vec![CompactValue::uninitialized(); n];
            let mut kinds = vec![LKIND_OTHER; n];
            copy_args_to_locals(&mut locals, &mut kinds, args);
            (locals, kinds)
        }

        let obj_free_cases: Vec<(u16, Vec<Value>)> = vec![
            (0, vec![]),
            (4, vec![]),
            (4, vec![Value::Int(7)]),
            (4, vec![Value::Int(1), Value::Int(2)]),
            // Category-2: each takes two slots with the upper half unset.
            (4, vec![Value::Long(0x1234_5678_9abc_def0)]),
            (4, vec![Value::Double(-0.0)]),
            (6, vec![Value::Long(1), Value::Int(2), Value::Double(3.5)]),
            // Mixed with a null reference, and with a retaddr.
            (5, vec![Value::Object(None), Value::Int(9)]),
            (5, vec![Value::ReturnAddress(11), Value::Float(1.5)]),
            // Declared max_locals SMALLER than the arguments need: the
            // effective count is clamped up, and both forms must agree on it.
            (1, vec![Value::Long(5), Value::Long(6)]),
            (0, vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        ];

        for (max_locals, args) in obj_free_cases {
            let (want_locals, want_kinds) = reference(max_locals, &args);
            let (got_locals, got_kinds, eff) = init_locals_from_parts(max_locals, &args, None);
            assert_eq!(
                eff as usize,
                want_locals.len(),
                "effective_max_locals for max_locals={max_locals} args={args:?}"
            );
            assert_eq!(
                got_locals.iter().map(|c| c.raw_bits()).collect::<Vec<_>>(),
                want_locals.iter().map(|c| c.raw_bits()).collect::<Vec<_>>(),
                "locals differ for max_locals={max_locals} args={args:?}"
            );
            assert_eq!(
                got_kinds, want_kinds,
                "local kinds differ for max_locals={max_locals} args={args:?}"
            );
        }
    }

    /// The same equivalence when the buffers come from the pool with stale
    /// content in them — the recycled bytes must not survive into either form.
    #[test]
    fn pushed_locals_do_not_leak_recycled_slots() {
        let args = vec![Value::Int(3)];
        let dirty_vals: Vec<u64> = vec![0xDEAD_BEEF_DEAD_BEEF; 16];
        let dirty_kinds: Vec<u8> = vec![LKIND_LONG; 16];
        let (locals, kinds, eff) =
            init_locals_from_parts(4, &args, Some((dirty_vals, dirty_kinds)));
        assert_eq!(eff, 4);
        assert_eq!(locals.len(), 4);
        assert_eq!(kinds.len(), 4);
        assert_eq!(locals[0].as_int(), Some(3));
        assert_eq!(kinds[0], LKIND_OTHER);
        for i in 1..4 {
            assert_eq!(
                locals[i].raw_bits(),
                CompactValue::uninitialized().raw_bits(),
                "slot {i} kept recycled content"
            );
            assert_eq!(kinds[i], LKIND_OTHER, "kind {i} kept recycled content");
        }
    }

    #[test]
    fn frame_creation_with_args() {
        let frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[Value::Int(42), Value::Long(100)],
        );

        assert_eq!(frame.locals_len(), 5);
        assert_eq!(frame.get_local(0).as_int(), Some(42));
        // CompactValue stores Long untagged, so `get_local(...).as_long()`
        // on the `Value` round-trip cannot distinguish Long from Double.
        // Use the CompactValue accessor with context (Long-by-instruction).
        assert_eq!(frame.get_local_compact(1).as_long(), Some(100));
        // Slot 2 is the second half of the long → Uninitialized
    }

    // ── CR-CLO-2 — `Frame::method_index` ────────────────────────────────
    //
    // The index makes deferred stack-trace line resolution EXACT across an
    // overload set. Its whole failure mode is staleness: `resolve_line_numbers_
    // in_place` guards an index by re-checking the method's NAME, and a name
    // check cannot separate two overloads — which is the one population the
    // index exists for. So the two places that reuse a `Frame` allocation
    // under a new method get dedicated tests.

    fn probe_frame(class_id: ClassId, method: &str, descriptor: &str) -> Frame {
        Frame::new(
            class_id,
            "probe/Target".to_string(),
            method.to_string(),
            descriptor.to_string(),
            Some("Target.java".to_string()),
            vec![0xb1],
            vec![],
            1,
            1,
            &[],
        )
    }

    #[test]
    fn a_frames_method_index_starts_absent_and_round_trips_through_the_setter() {
        // `None` is the correct default everywhere: it restores exactly
        // today's unambiguous-name behaviour, so a pusher that does not know
        // the slot never has to guess.
        let mut f = probe_frame(ClassId::new(0), "m", "(I)V");
        assert_eq!(f.method_index(), None);
        f.set_method_index(Some(7));
        assert_eq!(f.method_index(), Some(7));
        f.set_method_index(None);
        assert_eq!(f.method_index(), None);
    }

    #[test]
    fn a_tail_call_clears_the_method_index_rather_than_stranding_it() {
        // Tail-call elimination reuses the frame allocation under a DIFFERENT
        // method. An index left behind would be read back as the callee's
        // slot; if it happened to point at another overload of the callee's
        // name it would pass the name re-check and print a line from the wrong
        // method body — the one failure the fallback rule can never produce.
        let mut f = probe_frame(ClassId::new(0), "m", "(I)V");
        f.set_method_index(Some(3));
        assert_eq!(f.method_index(), Some(3));

        f.reset_for_tail_call(
            ClassId::new(1),
            padded_bytecode(&[0xb1]),
            2,
            2,
            &[],
            Arc::from("probe/Other"),
            Arc::from("m"), // same NAME on purpose: the name check cannot help
            Arc::from("(J)V"),
            Some(Arc::from("Other.java")),
            Arc::from(Vec::<ExceptionTableEntry>::new().into_boxed_slice()),
        );

        assert_eq!(
            f.method_index(),
            None,
            "a tail call must clear the index along with class_id and inner"
        );
        assert_eq!(f.method_name(), "m");
        assert_eq!(f.class_id, ClassId::new(1));
    }

    #[test]
    fn a_thawed_continuation_frame_carries_no_method_index() {
        // `FrozenFrame` round-trips names and a descriptor, never a slot, and a
        // continuation can be resumed long after a redefinition reordered the
        // overload set it was captured from. Thaw must therefore start from
        // "unknown", not from whatever the pre-freeze frame happened to hold.
        let mut f = probe_frame(ClassId::new(0), "m", "(I)V");
        f.set_method_index(Some(5));
        let frozen = f.to_frozen_frame();
        let thawed = Frame::from_frozen_frame(frozen);
        assert_eq!(thawed.method_index(), None);
        assert_eq!(thawed.method_name(), "m");
    }

    /// A `CachedBytecodeMethod` shaped for the frame-install tests.
    fn cached_probe(
        name: &str,
        descriptor: &str,
        max_locals: u16,
        max_stack: u16,
    ) -> Arc<CachedBytecodeMethod> {
        Arc::new(CachedBytecodeMethod {
            declaring_class_id: ClassId::new(0),
            class_name: Arc::from("probe/Target"),
            method_name: Arc::from(name),
            method_descriptor: Arc::from(descriptor),
            source_file: Some(Arc::from("Target.java")),
            code: padded_bytecode(&[0xb1]),
            exception_table: Arc::from(Vec::<ExceptionTableEntry>::new().into_boxed_slice()),
            max_stack,
            max_locals,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        })
    }

    /// Everything a caller can observe about an installed frame, so the three
    /// install paths can be compared as wholes rather than field by field
    /// (`seq` excluded: it is a fresh counter value by design).
    fn frame_shape(f: &Frame) -> (ClassId, usize, u16, u16, usize, Vec<u64>, Vec<u8>) {
        (
            f.class_id,
            f.pc,
            f.max_stack,
            f.max_locals,
            f.stack.len(),
            f.locals.iter().map(|c| c.raw_bits()).collect(),
            f.local_kinds.clone(),
        )
    }

    #[test]
    fn rebuilding_a_retired_slot_holds_what_a_by_value_push_held() {
        // The claim the general dispatchers' slot reuse rests on. If these two
        // can differ, a call that happens to find a retired slot runs with
        // different locals from the same call that does not — which is a
        // wrong-answer bug that no test of either path alone can see.
        let cached = cached_probe("m", "(IJLjava/lang/Object;)V", 6, 3);
        let args = [
            Value::Int(7),
            Value::Long(-9_000_000_000),
            Value::Object(None),
        ];

        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        let fresh = Frame::new_pooled_cached(
            Arc::clone(&cached),
            &args,
            &mut locals_pool,
            &mut stacks_pool,
        );

        // A frame that has been used and is now being rebuilt: dirty locals, a
        // non-empty operand stack, a pc partway through, a monitor to release.
        let mut used = Frame::new_pooled_cached(
            cached_probe("other", "(D)V", 12, 9),
            &[Value::Double(1.5)],
            &mut locals_pool,
            &mut stacks_pool,
        );
        used.pc = 17;
        used.backward_count = 42;
        let _ = used.stack.push(Value::Int(1234));
        used.reset_cached_value(Arc::clone(&cached), &args);

        assert_eq!(frame_shape(&used), frame_shape(&fresh));
        assert_eq!(used.backward_count, 0);
        assert!(used.monitor_on_exit.is_none());
        assert_eq!(used.method_name(), "m");
    }

    #[test]
    fn the_value_and_compact_resets_lay_down_the_same_locals() {
        // `reset_cached_value` and `reset_cached_compact` are the two argument
        // representations of one operation. A `long` is the case that can
        // diverge: it occupies two local slots and the upper half is filler.
        let cached = cached_probe("m", "(JI)V", 4, 2);
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();

        let mut by_value =
            Frame::new_pooled_cached(Arc::clone(&cached), &[], &mut locals_pool, &mut stacks_pool);
        by_value.reset_cached_value(Arc::clone(&cached), &[Value::Long(5), Value::Int(6)]);

        let mut by_slot =
            Frame::new_pooled_cached(Arc::clone(&cached), &[], &mut locals_pool, &mut stacks_pool);
        by_slot.reset_cached_compact(
            Arc::clone(&cached),
            &[
                (CompactValue::from_value_kinded(Value::Long(5)), b'J'),
                (CompactValue::from_value_kinded(Value::Int(6)), b'I'),
            ],
        );

        assert_eq!(frame_shape(&by_slot), frame_shape(&by_value));
    }

    #[test]
    fn an_emplaced_value_frame_matches_a_pushed_one() {
        // The other half of the ladder: no retired slot, so the frame is
        // written into the next slot instead of being built and moved.
        let cached = cached_probe("m", "(FI)V", 5, 4);
        let args = [Value::Float(2.5), Value::Int(-3)];
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();

        let mut pushed = FrameStack::new();
        pushed.push(Frame::new_pooled_cached(
            Arc::clone(&cached),
            &args,
            &mut locals_pool,
            &mut stacks_pool,
        ));

        let mut emplaced = FrameStack::new();
        let (locals, kinds, stack, eff) =
            take_cached_value_parts(&cached, &args, &mut locals_pool, &mut stacks_pool);
        emplaced.emplace_cached_compact(Arc::clone(&cached), locals, kinds, stack, eff);

        assert_eq!(emplaced.len(), 1);
        assert_eq!(frame_shape(&emplaced[0]), frame_shape(&pushed[0]));
    }

    #[test]
    fn a_value_reuse_push_is_refused_when_no_slot_is_retired() {
        // `push_cached_value_reusing` must never grow the stack: the caller
        // reads `false` as "build one", and a version that pushed anyway would
        // install the frame twice.
        let cached = cached_probe("m", "()V", 1, 1);
        let mut frames = FrameStack::new();
        assert!(!frames.push_cached_value_reusing(Arc::clone(&cached), &[]));
        assert_eq!(frames.len(), 0);

        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        frames.push(Frame::new_pooled_cached(
            Arc::clone(&cached),
            &[],
            &mut locals_pool,
            &mut stacks_pool,
        ));
        assert!(frames.retire_top());
        assert!(frames.push_cached_value_reusing(cached, &[]));
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn a_cached_frame_reports_no_index_until_the_shared_entry_carries_one() {
        // Pins the documented state of the cached half: `CachedBytecodeMethod`
        // has no `method_index` field (its 38 struct literals live in four
        // crates and none has a `..` tail), so a frame pushed through the
        // cached-invoke path answers `None` and falls back to the name rule.
        // When that field lands, `new_pooled_cached` seeds this from the Arc
        // and this assertion flips to `Some(..)`.
        let cached = Arc::new(CachedBytecodeMethod {
            declaring_class_id: ClassId::new(0),
            class_name: Arc::from("probe/Target"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("(I)V"),
            source_file: Some(Arc::from("Target.java")),
            code: padded_bytecode(&[0xb1]),
            exception_table: Arc::from(Vec::<ExceptionTableEntry>::new().into_boxed_slice()),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            descriptor_facts_cache: std::sync::OnceLock::new(),
            intercept_shape_cache: std::sync::OnceLock::new(),
            interp_invocations: std::sync::atomic::AtomicU32::new(0),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        });
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        let mut f = Frame::new_pooled_cached(cached, &[], &mut locals_pool, &mut stacks_pool);
        assert_eq!(f.method_index(), None);
        // The per-frame field is still writable for a cached frame — the
        // uniform placement on `Frame` is what buys that.
        f.set_method_index(Some(1));
        assert_eq!(f.method_index(), Some(1));
    }

    /// The payoff, end to end: an overload set resolves to an EXACT line when
    /// the frame carries its index, and fails closed to `UNKNOWN` without it.
    ///
    /// This is written against the entry shape
    /// `stackwalker::capture_frames_no_lines` produces once its `method_index:
    /// None` becomes `method_index: f.method_index()` (one line, in a file this
    /// pass does not own). Everything the change depends on is pinned here.
    #[test]
    fn an_overload_set_resolves_exactly_only_when_the_frame_carries_its_index() {
        use crate::runtime::stackwalker::{resolve_line_numbers_in_place, LINE_NUMBER_UNKNOWN};
        use cratonvm_reader::attribute::LineNumberEntry;

        // Two overloads of `m`: same name, different LineNumberTables. The
        // unambiguous-name rule cannot choose between them by construction.
        let (store, class_id) = crate::runtime::stackwalker::test_support::store_with(vec![
            crate::runtime::stackwalker::test_support::named_method(
                "m",
                "(I)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 11,
                }],
            ),
            crate::runtime::stackwalker::test_support::named_method(
                "m",
                "(J)V",
                vec![LineNumberEntry {
                    start_pc: 0,
                    line_number: 22,
                }],
            ),
        ]);

        let entry_for = |f: &Frame| cratonvm_native_api::StackTraceEntry {
            class_name: f.class_name_arc(),
            method_name: f.method_name_arc(),
            source_file: f.source_file_arc(),
            line_number: LINE_NUMBER_UNKNOWN,
            byte_code_index: f.last_instr_pc.min(i32::MAX as usize) as i32,
            class_id: Some(f.class_id),
            method_index: f.method_index(),
        };

        // Without an index: declines, and specifically does NOT guess.
        let plain = probe_frame(class_id, "m", "(J)V");
        let mut entries = vec![entry_for(&plain)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 0);
        assert_eq!(entries[0].line_number, LINE_NUMBER_UNKNOWN);

        // With the index: exact, and it picks the SECOND overload — the one a
        // name-only rule could never have reached.
        let mut indexed = probe_frame(class_id, "m", "(J)V");
        indexed.set_method_index(Some(1));
        let mut entries = vec![entry_for(&indexed)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
        assert_eq!(entries[0].line_number, 22);

        // And the first overload resolves to its own line, not the other's.
        let mut first = probe_frame(class_id, "m", "(I)V");
        first.set_method_index(Some(0));
        let mut entries = vec![entry_for(&first)];
        assert_eq!(resolve_line_numbers_in_place(&store, &mut entries), 1);
        assert_eq!(entries[0].line_number, 11);
    }

    /// If `Code.max_locals` is smaller than the invocation argument slots,
    /// the frame must still receive every argument (Surefire-style bad metadata).
    #[test]
    fn frame_expands_locals_when_declared_max_smaller_than_invoke_args() {
        let args = [Value::Int(10), Value::Int(20), Value::Int(30)];
        let frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            2,
            &args,
        );
        assert_eq!(frame.max_locals, 3);
        assert_eq!(frame.locals_len(), 3);
        assert_eq!(frame.get_local(0).as_int(), Some(10));
        assert_eq!(frame.get_local(1).as_int(), Some(20));
        assert_eq!(frame.get_local(2).as_int(), Some(30));
    }

    #[test]
    fn frame_expands_locals_for_category2_when_declared_too_small() {
        let args = [Value::Long(0x1122_3344_5566_7788)];
        let frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            1,
            &args,
        );
        assert_eq!(frame.max_locals, 2);
        assert_eq!(frame.locals_len(), 2);
        // Long/Double tag ambiguity in CompactValue — use the compact
        // accessor with explicit Long context.
        assert_eq!(
            frame.get_local_compact(0).as_long(),
            Some(0x1122_3344_5566_7788)
        );
        assert_eq!(frame.get_local(1), Value::Uninitialized);
    }

    #[test]
    fn frame_set_and_get_local() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            3,
            &[],
        );

        frame.set_local(0, Value::Int(1));
        frame.set_local(1, Value::Float(2.5));
        assert_eq!(frame.get_local(0).as_int(), Some(1));
        assert_eq!(frame.get_local(1).as_float(), Some(2.5));
    }

    #[test]
    fn frame_soa_all_types() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            8,
            &[],
        );

        frame.set_local(0, Value::Int(42));
        frame.set_local(1, Value::Long(9999999999));
        frame.set_local(2, Value::Float(3.15));
        frame.set_local(3, Value::Double(2.719));
        frame.set_local(4, Value::Object(None));
        frame.set_local(5, Value::Uninitialized);
        frame.set_local(6, Value::ReturnAddress(123));

        assert_eq!(frame.get_local(0).as_int(), Some(42));
        // Long via the context-aware compact accessor.
        assert_eq!(frame.get_local_compact(1).as_long(), Some(9999999999));
        // Bit equality, like the int/long/object slots asserted beside them:
        // a local read back is a round trip with no arithmetic on the path.
        assert_eq!(
            frame.get_local(2).as_float().unwrap().to_bits(),
            3.15f32.to_bits()
        );
        assert_eq!(
            frame.get_local(3).as_double().unwrap().to_bits(),
            2.719f64.to_bits()
        );
        assert!(frame.get_local(4).is_null());
        assert_eq!(frame.get_local(5), Value::Uninitialized);
        if let Value::ReturnAddress(a) = frame.get_local(6) {
            assert_eq!(a, 123);
        } else {
            panic!("expected ReturnAddress");
        }
    }

    #[test]
    fn frame_get_local_raw() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            3,
            &[Value::Int(42), Value::Long(100)],
        );

        assert_eq!(frame.get_local_raw(0), 42);
        assert_eq!(frame.get_local_raw(1) as i64, 100);
    }

    /// REGRESSION (OSR entry-state reconstruction / `CRATONVM_JIT_OSR`): a
    /// primitive `long` whose NaN-boxed bits collide with the `SUB_OBJECT`
    /// sub-tag must still round-trip through `get_local_raw`/`get_local_tag`
    /// bit-exact as `VTAG_LONG`. `try_osr` is this pair's only production
    /// caller (building the `jit_locals` snapshot handed to `osr_enter`); before
    /// the `local_kinds` gate, `compact_to_local_slot` classified such a slot
    /// as `VTAG_OBJECT` purely from `CompactValue::tag()` and returned only its
    /// low 47-bit payload — silently truncating the long's true value into a
    /// fabricated "pointer" at OSR entry, mirroring the exact hazard
    /// `scan_local_objects_skips_collision_long_aliasing_live_object` and
    /// `locals_snapshot`'s `local_kinds` gate already guard against elsewhere.
    #[test]
    fn get_local_raw_preserves_collision_long_as_long_not_truncated_object() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        // Bit-identical to a genuine `CompactValue::object(addr)` for some
        // plausible-looking, 8-byte-aligned, above-guard-page address — but
        // stored as a `long`, so `local_kinds[0]` is `LKIND_LONG`.
        let addr = 0x20000000000u64;
        assert_eq!(addr & 0x7, 0);
        assert!(addr < (1 << 47));
        let collision_bits = CompactValue::object(addr).raw_bits();
        frame.set_local_unchecked(0, Value::Long(collision_bits as i64));

        // Sanity: the underlying CompactValue really is tag-ambiguous.
        assert!(frame.get_local_compact(0).is_object());

        assert_eq!(
            frame.get_local_raw(0),
            collision_bits,
            "get_local_raw must return the bit-exact long, not the masked 47-bit payload"
        );
        assert_eq!(frame.get_local_tag(0), VTAG_LONG);
    }

    /// A `VTAG_LONG` local whose bits happen to look like an aligned object
    /// pointer is **NOT** a heap root — primitives are not references by JVM
    /// spec, and the bridges that used to smuggle `jobject` through `Long`
    /// now promote them to `VTAG_OBJECT` via `coerce_value_for_return` before
    /// the value ever reaches a local. This test guards against the Spring
    /// Boot SEGV regression where pointer-shaped long bits were mis-rooted.
    #[test]
    fn scan_local_objects_does_not_root_long_with_pointer_shaped_bits() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        let fake = 0x1000usize as i64;
        frame.set_local_unchecked(0, Value::Long(fake));

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);
        assert!(roots.is_empty(), "VTAG_LONG must never produce a root");
    }

    #[test]
    fn scan_local_objects_skips_long_that_is_not_aligned_object_pattern() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_unchecked(0, Value::Long(7));

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);
        assert!(roots.is_empty());
    }

    /// Symmetric with `scan_local_objects`: `VTAG_LONG` slots are primitives,
    /// GC must NOT remap them even if their bits look like a moved pointer.
    #[test]
    fn update_local_refs_does_not_touch_long_with_pointer_shaped_bits() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        use std::collections::HashMap;

        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        let old = 0x2000usize as i64;
        frame.set_local_unchecked(0, Value::Long(old));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x2000usize, 0x3000usize);
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        frame.update_local_refs(&map, &heap);

        // Long primitive must be preserved verbatim. Use the compact
        // accessor (CompactValue's Long/Double tag ambiguity is resolved
        // by the caller's instruction context).
        assert_eq!(frame.get_local_compact(0).as_long(), Some(0x2000));
    }

    /// REGRESSION (bc math-ec collision-long corruption): a primitive `long`
    /// whose NaN-boxed bits *collide* with the SUB_OBJECT pattern AND whose
    /// 47-bit payload coincides with a live heap object's address must NOT be
    /// treated as a GC root. Before the `local_kinds` gate, the collision
    /// long was bit-identical to a genuine reference, so `scan_local_objects`
    /// rooted it and the moving collector then relocated + corrupted it. This
    /// is the discriminating case the older `…pointer_shaped_bits` test misses
    /// (a low-address long is not even `is_object()`).
    #[test]
    fn scan_local_objects_skips_collision_long_aliasing_live_object() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        // A real, live heap object — its address passes `is_heap_addr`.
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as u64;
        assert_eq!(addr & 0x7, 0, "heap objects are 8-byte aligned");
        assert!(addr < (1 << 47), "user-space heap addr fits in 47 bits");

        // A BC-F2m-style primitive `long` whose bits collide with SUB_OBJECT
        // and whose payload equals the live object's address — bit-identical
        // to `CompactValue::object(addr)`, but it is a *value*, not a pointer.
        let collision_bits = CompactValue::object(addr).raw_bits();

        // Bytecode that READS both slots at pc 0 so the per-bci liveness
        // filter (which correctly drops never-read-again slots) keeps them:
        // this test is about tag-collision semantics, not liveness.
        //   0: lload_0 ; 1: pop2 ; 2: aload_1 ; 3: pop ; 4: return
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x1e, 0x58, 0x2b, 0x57, 0xb1],
            vec![],
            10,
            4,
            &[],
        );
        // Slot 0: stored as a long ⇒ LKIND_LONG ⇒ never a root.
        frame.set_local_unchecked(0, Value::Long(collision_bits as i64));
        // Slot 1: a genuine reference at the *same* address ⇒ a real root.
        frame.set_local_unchecked(1, Value::Object(Some(obj)));
        // Both slots carry identical SUB_OBJECT bits…
        assert!(frame.get_local_compact(0).is_object());
        assert_eq!(frame.get_local_compact(0).as_object_ptr(), Some(addr));

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);

        // …yet only the reference slot is rooted; the collision long is not.
        assert_eq!(roots.len(), 1, "only the genuine reference is a GC root");
        assert_eq!(roots[0].as_ptr() as u64, addr);
    }

    /// A `double` whose raw bits collide with the NaN-box tag space round-trips
    /// through a local slot bit-exact, and is never a GC root even when its
    /// 47-bit payload is a live object's address.
    ///
    /// The sibling of `scan_local_objects_skips_collision_long_aliasing_live_object`
    /// for the other category-2 primitive. `CompactValue::long` has stored
    /// verbatim since the BC SM2 fix (2026-05-28) and the `local_kinds` gate is
    /// what makes that safe; doubles now store verbatim through
    /// `CompactValue::double_raw` for the same reason, which is what keeps a NaN
    /// payload alive across `dstore` / `dload` (see
    /// `nan-payloads-lost-to-the-compactvalue-tag-collision-FIXED-20260828`).
    #[test]
    fn tag_colliding_double_local_round_trips_and_is_never_a_root() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as u64;
        // Bit-identical to a genuine reference — but stored as a double.
        let collision_bits = CompactValue::object(addr).raw_bits();

        //   0: dload_0 ; 1: pop2 ; 2: aload_2 ; 3: pop ; 4: return
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x26, 0x58, 0x2c, 0x57, 0xb1],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(0, Value::Double(f64::from_bits(collision_bits)));
        frame.set_local_unchecked(2, Value::Object(Some(obj)));

        // The payload survived the store…
        assert_eq!(frame.get_local_raw(0), collision_bits);
        match frame.get_local(0) {
            Value::Double(d) => assert_eq!(d.to_bits(), collision_bits),
            other => panic!("a double local decoded as {other:?}"),
        }
        match frame.get_local_unchecked(0) {
            Value::Double(d) => assert_eq!(d.to_bits(), collision_bits),
            other => panic!("a double local decoded as {other:?}"),
        }
        // …and the slot really does carry the SUB_OBJECT bit pattern.
        assert!(frame.get_local_compact(0).is_object());
        assert_eq!(frame.get_local_compact(0).as_object_ptr(), Some(addr));

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);
        assert_eq!(roots.len(), 1, "only the genuine reference is a GC root");
        assert_eq!(roots[0].as_ptr() as u64, addr);

        // A moving collector must not rewrite the double either.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(addr as usize, (addr + 0x1000) as usize);
        frame.update_local_refs(&map, &heap);
        assert_eq!(
            frame.get_local_raw(0),
            collision_bits,
            "relocation must not touch a LKIND_DOUBLE slot",
        );
    }

    /// An argument passed by value keeps a tag-colliding NaN payload across the
    /// call boundary — `copy_args_to_locals` writes the LKIND_DOUBLE mark, so it
    /// can store the bits verbatim.
    #[test]
    fn tag_colliding_double_argument_survives_the_call_boundary() {
        let bits: u64 = 0xFFFE_5E0E_8000_0000;
        let frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "(D)V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            4,
            &[Value::Double(f64::from_bits(bits))],
        );
        assert_eq!(frame.get_local_raw(0), bits);
        match frame.get_local(0) {
            Value::Double(d) => assert_eq!(d.to_bits(), bits),
            other => panic!("a double argument decoded as {other:?}"),
        }
    }

    /// A category-2 (`long`/`double`) store must INVALIDATE the reserved upper
    /// half so a former object reference in that slot is not rooted forever.
    /// This is the interpreter-side root of the SteadyChurn unbounded-retention
    /// leak: `main`'s setup-loop `Node` temp lived in the slot pair the
    /// churn-loop `long` reused, so without clearing the upper half the dead
    /// node (and its whole `next` chain) stayed reachable.
    #[test]
    fn cat2_store_invalidates_reserved_upper_half() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);

        // Bytecode that reads slots 2 and 3 as a long at pc 0 so per-bci
        // liveness keeps the pair live — the test is about the store clearing
        // the upper half, not liveness dropping it.
        //   0: lload_2 ; 1: pop2 ; 2: return
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x20, 0x58, 0xb1],
            vec![],
            10,
            6,
            &[],
        );

        // Slot 3 first holds a live object (the "scoped-out temp").
        frame.set_local_unchecked(3, Value::Object(Some(obj)));
        // A long stored at slot 2 reserves slots 2 AND 3 — slot 3's stale
        // object reference must be gone.
        frame.set_local_unchecked(2, Value::Long(0x1_0000_0002));

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);
        assert!(
            roots.is_empty(),
            "cat-2 store did not clear the reserved upper half (stale object rooted)"
        );
        // The long still reads back intact from slot 2.
        assert_eq!(frame.get_local_compact(2).as_long(), Some(0x1_0000_0002));
    }

    /// `scan_local_objects` must NOT root a genuinely scoped-out OBJECT local
    /// (written once, never read again) — the general liveness imprecision,
    /// independent of the cat-2-store leak. Verifies the frame-level WIRING of
    /// the per-bci liveness filter (the analyzer itself is unit-tested in
    /// `runtime::local_liveness`).
    #[test]
    fn scan_local_objects_drops_scoped_out_object_local() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);

        // Bytecode: slot 1 written at pc 0, then an endless counter loop over
        // slot 2 that never reads slot 1.
        //   0: astore_1 ; 1: iinc 2,1 ; 4: goto 1
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x4c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xfd],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(1, Value::Object(Some(obj)));
        // Position the frame in the loop (pc 1), where slot 1 is dead.
        frame.pc = 1;
        frame.last_instr_pc = 1;

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);
        assert!(
            roots.is_empty(),
            "a scoped-out object local was rooted (liveness filter not applied)"
        );

        // With the kill-switch the historical behaviour returns: the slot is
        // rooted. (Cannot toggle the cached env flag here, so assert the
        // analyzer agrees the slot is dead — the wiring above proves the
        // filter consults it.)
        let mask = crate::runtime::local_liveness::live_locals_mask(
            &frame.code,
            frame.exception_table(),
            frame.max_locals,
            [frame.pc, frame.last_instr_pc],
        );
        assert_eq!(mask & 0b010, 0, "analyzer must report slot 1 dead");

        let mut all_live_roots = Vec::new();
        frame.scan_local_objects_all_live(&mut all_live_roots, &heap);
        assert_eq!(
            all_live_roots,
            vec![obj],
            "all-live snapshot scan must keep the scoped-out reference"
        );
    }

    #[test]
    fn scan_local_objects_roots_lost_tag_other_local() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as u64;

        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );

        // Model a reference-typed local whose CompactValue object tag was lost:
        // the slot kind is still LKIND_OTHER, but the bits are a raw heap addr.
        frame.set_local_compact_unchecked(0, CompactValue::long(addr as i64));
        assert!(!frame.get_local_compact(0).is_object());

        let mut roots = Vec::new();
        frame.scan_local_objects(&mut roots, &heap);

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].as_ptr() as u64, addr);
    }

    #[test]
    fn set_local_compact_long_preserves_kind_and_upper_half_mark() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );

        // `CompactValue::long(-1)` is a real compact-tagged `Long` collision.
        // Preserving the kind keeps this category-2 slot from being treated as a
        // stale upper half in later scans and OSR snapshots.
        let cv = CompactValue::long(-1);
        assert_eq!(cv.tag(), CompactTag::Long);
        frame.set_local_compact(1, CompactValue::object(0x1000));
        frame.set_local_compact(0, cv);

        assert_eq!(frame.get_local_tag(0), VTAG_LONG);
        assert_eq!(frame.get_local_raw(0), u64::MAX);
        assert_eq!(frame.get_local_tag(1), VTAG_LONG);
        assert_eq!(frame.get_local_raw(1), 0);
    }

    #[test]
    fn update_local_refs_remaps_lost_tag_other_local() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        use std::collections::HashMap;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let old_obj = heap.alloc_object(ClassId::new(0), 0);
        let new_obj = heap.alloc_object(ClassId::new(0), 0);
        let old_addr = old_obj.as_ptr() as u64;
        let new_addr = new_obj.as_ptr() as u64;
        assert_ne!(old_addr, new_addr);

        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_compact_unchecked(0, CompactValue::long(old_addr as i64));
        assert!(!frame.get_local_compact(0).is_object());

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_addr as usize, new_addr as usize);
        frame.update_local_refs(&map, &heap);

        let remapped = frame.get_local_compact(0);
        assert!(remapped.is_object());
        assert_eq!(remapped.as_object_ptr(), Some(new_addr));
    }

    /// Symmetric `update_local_refs` regression: a collision long whose payload
    /// equals some *unrelated* object's from-space address must be left
    /// verbatim even though that address is a `pointer_map` key (the map is
    /// global — rooting and remapping key off different objects).
    #[test]
    fn update_local_refs_preserves_collision_long_matching_pointer_map_key() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        use std::collections::HashMap;

        // Collision long: SUB_OBJECT bits with payload 0x2000.
        let collision_bits = CompactValue::object(0x2000).raw_bits();
        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_unchecked(0, Value::Long(collision_bits as i64));
        assert!(frame.get_local_compact(0).is_object());
        assert_eq!(frame.get_local_compact(0).as_object_ptr(), Some(0x2000));

        // A relocation of *some other* object that happened to live at 0x2000.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x2000usize, 0x3000usize);
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        frame.update_local_refs(&map, &heap);

        // The primitive long is preserved bit-exact (NOT rewritten to 0x3000).
        assert_eq!(
            frame.get_local_compact(0).as_long_unchecked(),
            collision_bits as i64
        );
    }

    #[test]
    #[should_panic]
    fn get_local_unchecked_panics_out_of_bounds() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        let _ = frame.get_local_unchecked(99); // should panic
    }

    #[test]
    #[should_panic]
    fn set_local_unchecked_panics_out_of_bounds() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local_unchecked(99, Value::Int(1)); // should panic
    }

    #[test]
    fn get_local_out_of_bounds_returns_uninitialized() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        assert_eq!(frame.get_local(99), Value::Uninitialized);
    }

    #[test]
    fn set_local_out_of_bounds_is_no_op() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        frame.set_local(99, Value::Int(42)); // should not panic
        assert_eq!(frame.get_local(99), Value::Uninitialized);
    }

    #[test]
    fn frame_metadata_accessors() {
        let frame = Frame::new(
            ClassId::new(5),
            "com/example/Foo".to_string(),
            "bar".to_string(),
            "(I)V".to_string(),
            Some("Foo.java".to_string()),
            vec![0xb1], // return void
            vec![],
            10,
            3,
            &[],
        );
        assert_eq!(frame.class_name(), "com/example/Foo");
        assert_eq!(frame.method_name(), "bar");
        assert_eq!(frame.method_descriptor(), "(I)V");
        assert_eq!(frame.source_file(), Some("Foo.java"));
        assert_eq!(frame.class_id, ClassId::new(5));
    }

    #[test]
    fn padded_bytecode_adds_trailing_zeros() {
        let code = vec![0x2a, 0xb1]; // aload_0, return
        let padded = padded_bytecode(&code);
        assert_eq!(padded.len(), 4);
        assert_eq!(padded[0], 0x2a);
        assert_eq!(padded[1], 0xb1);
        assert_eq!(padded[2], 0);
        assert_eq!(padded[3], 0);
    }

    #[test]
    fn frame_recycle_returns_vecs() {
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[Value::Int(1)],
        );
        frame.recycle(&mut locals_pool, &mut stacks_pool);
        assert_eq!(locals_pool.len(), 1);
        assert_eq!(stacks_pool.len(), 1);
    }

    #[test]
    #[should_panic]
    fn get_local_raw_panics_out_of_bounds() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        let _ = frame.get_local_raw(99); // should panic
    }

    #[test]
    #[should_panic]
    fn get_local_tag_panics_out_of_bounds() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        let _ = frame.get_local_tag(99); // should panic
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn frame_creation_with_zero_locals() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            0, // zero locals
            &[],
        );
        assert_eq!(frame.locals_len(), 0);
        // get_local out of bounds returns Uninitialized
        assert_eq!(frame.get_local(0), Value::Uninitialized);
    }

    #[test]
    fn frame_creation_with_max_locals() {
        // Use a large max_locals value
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            u16::MAX, // 65535 locals
            &[],
        );
        assert_eq!(frame.locals_len(), u16::MAX as usize);
        // All locals should be uninitialized
        assert_eq!(frame.get_local(0), Value::Uninitialized);
        assert_eq!(frame.get_local(100), Value::Uninitialized);
        assert_eq!(frame.get_local(65534), Value::Uninitialized);
    }

    #[test]
    fn frame_local_set_get_boundary() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            3,
            &[],
        );
        // Set first and last local
        frame.set_local(0, Value::Int(100));
        frame.set_local(2, Value::Int(200));
        assert_eq!(frame.get_local(0).as_int(), Some(100));
        assert_eq!(frame.get_local(2).as_int(), Some(200));
        // Middle is still uninitialized
        assert_eq!(frame.get_local(1), Value::Uninitialized);

        // Out of bounds set is a no-op
        frame.set_local(3, Value::Int(300));
        assert_eq!(frame.get_local(3), Value::Uninitialized);
    }

    #[test]
    fn exception_table_lookup_matching() {
        let exception_table = vec![
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 10,
                handler_pc: 20,
                catch_type: 5, // specific exception class
            },
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 10,
                handler_pc: 30,
                catch_type: 0, // catch-all (finally)
            },
        ];
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            exception_table,
            4,
            2,
            &[],
        );
        let table = frame.exception_table();
        assert_eq!(table.len(), 2);
        // First entry covers pc 0..10, handler at 20
        assert_eq!(table[0].start_pc, 0);
        assert_eq!(table[0].end_pc, 10);
        assert_eq!(table[0].handler_pc, 20);
        assert_eq!(table[0].catch_type, 5);
    }

    #[test]
    fn exception_table_non_matching_range() {
        let exception_table = vec![ExceptionTableEntry {
            start_pc: 10,
            end_pc: 20,
            handler_pc: 30,
            catch_type: 0,
        }];
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            exception_table,
            4,
            2,
            &[],
        );
        let table = frame.exception_table();
        // PC 5 is outside [10, 20), so no handler matches
        let handler = table
            .iter()
            .find(|e| 5 >= e.start_pc as usize && 5 < e.end_pc as usize);
        assert!(handler.is_none());
    }

    #[test]
    fn exception_table_nested_handlers() {
        let exception_table = vec![
            ExceptionTableEntry {
                start_pc: 0,
                end_pc: 50,
                handler_pc: 100,
                catch_type: 0, // outer catch-all
            },
            ExceptionTableEntry {
                start_pc: 10,
                end_pc: 30,
                handler_pc: 60,
                catch_type: 5, // inner specific
            },
        ];
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            exception_table,
            4,
            2,
            &[],
        );
        let table = frame.exception_table();
        // PC 15 is inside both ranges
        let matches: Vec<_> = table
            .iter()
            .filter(|e| 15 >= e.start_pc as usize && 15 < e.end_pc as usize)
            .collect();
        assert_eq!(matches.len(), 2);
        // First match is the outer handler, second is inner
        assert_eq!(matches[0].handler_pc, 100);
        assert_eq!(matches[1].handler_pc, 60);
    }

    #[test]
    fn frame_with_no_exception_handlers() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![], // no exception handlers
            4,
            2,
            &[],
        );
        assert!(frame.exception_table().is_empty());
    }

    #[test]
    fn frame_source_file_none() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None, // no source file
            vec![],
            vec![],
            4,
            2,
            &[],
        );
        assert!(frame.source_file().is_none());
        assert!(frame.source_file_arc().is_none());
    }

    #[test]
    fn frame_pc_starts_at_zero() {
        let frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![0x2a, 0xb1],
            vec![],
            4,
            2,
            &[],
        );
        assert_eq!(frame.pc, 0);
        assert_eq!(frame.backward_count, 0);
    }

    #[test]
    fn background_osr_pending_restarts_stride_without_spending_rejection() {
        let mut frame = Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            "hotLoop".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            1,
            &[],
        );
        frame.backward_count = 10_000;
        frame.osr_attempt_counts.push((4, 2));

        frame.record_osr_background_pending();

        assert_eq!(frame.backward_count, 0);
        assert_eq!(frame.osr_attempt_counts, vec![(4, 2)]);
    }

    // ===================================================================
    // FrameStack — address stability
    // ===================================================================

    fn test_frame(name: &str, max_locals: u16) -> Frame {
        Frame::new(
            ClassId::new(0),
            "Test".to_string(),
            name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            2,
            max_locals,
            &[],
        )
    }

    /// The core guarantee: after `reserve_stable(n)`, `n` pushes move nothing.
    /// This is what lets the interpreter hoist a `*mut Frame` across the
    /// dispatch loop instead of re-indexing `thread.frames[frame_idx]`.
    #[test]
    fn frame_addresses_are_stable_across_a_reserved_deep_push_sequence() {
        const DEPTH: usize = 4096;
        let mut frames = FrameStack::new();
        frames.reserve_stable(DEPTH);
        let epoch_after_reserve = frames.reloc_epoch();
        assert!(frames.stable_headroom() >= DEPTH);

        let mut addrs = Vec::with_capacity(DEPTH);
        for i in 0..DEPTH {
            frames.push(test_frame("deep", 1));
            addrs.push(&frames[i] as *const Frame);
            // Nothing already on the stack may have moved.
            assert_eq!(
                frames.reloc_epoch(),
                epoch_after_reserve,
                "push #{} relocated despite reserve_stable({})",
                i,
                DEPTH
            );
        }

        for (i, &expected) in addrs.iter().enumerate() {
            assert_eq!(
                &frames[i] as *const Frame, expected,
                "frame {i} moved during a reserved push sequence"
            );
        }

        // Popping never moves a survivor either.
        for _ in 0..DEPTH / 2 {
            frames.pop();
        }
        for (i, &expected) in addrs.iter().enumerate().take(DEPTH / 2) {
            assert_eq!(
                &frames[i] as *const Frame, expected,
                "frame {i} moved during pops"
            );
        }
    }

    /// The raw-pointer API must observe the same address as the safe one, and
    /// stay valid across pushes *and* pops of frames above it.
    #[test]
    fn frame_ptr_stays_valid_across_pushes_and_pops() {
        let mut frames = FrameStack::new();
        frames.reserve_stable(64);
        frames.push(test_frame("bottom", 3));

        let epoch = frames.reloc_epoch();
        let bottom = frames.frame_ptr(0);
        assert!(!bottom.is_null());
        assert_eq!(bottom as *const Frame, &frames[0] as *const Frame);
        assert_eq!(frames.current_ptr(), bottom, "depth 1: top == bottom");

        // SAFETY: index 0 is live, epoch unchanged, no other reference to it.
        unsafe { (*bottom).pc = 0x1234 };

        for _ in 0..48 {
            frames.push(test_frame("callee", 1));
        }
        assert_eq!(frames.reloc_epoch(), epoch, "reserved pushes must not grow");
        while frames.len() > 1 {
            frames.pop();
        }

        assert_eq!(frames.frame_ptr(0), bottom, "bottom frame moved");
        // SAFETY: same conditions as above.
        assert_eq!(
            unsafe { (*bottom).pc },
            0x1234,
            "bottom frame was clobbered"
        );

        // Out of range yields null rather than panicking.
        assert!(frames.frame_ptr(1).is_null());
        frames.pop();
        assert!(frames.current_ptr().is_null());
    }

    /// Growth is the ONLY thing that moves frames, and it is always announced
    /// via `reloc_epoch`.
    #[test]
    fn growth_is_the_only_relocation_and_always_bumps_the_epoch() {
        let mut frames = FrameStack::new();
        assert_eq!(frames.reloc_epoch(), 0);

        // First push allocates, but there is nothing to move → no bump.
        frames.push(test_frame("first", 1));
        assert_eq!(frames.reloc_epoch(), 0, "initial reservation moved nothing");
        assert!(frames.stable_headroom() >= FRAME_STACK_INITIAL_STABLE_CAP - 1);

        // Fill exactly to capacity: still no relocation.
        while frames.stable_headroom() > 0 {
            frames.push(test_frame("fill", 1));
        }
        assert_eq!(frames.reloc_epoch(), 0);

        // The next push must grow, which must bump the epoch. (Addresses taken
        // before this point are stale by construction — that is precisely what
        // the epoch bump reports — so we assert the epoch and never dereference
        // a pre-growth pointer.)
        frames.push(test_frame("overflow", 1));
        assert_eq!(frames.reloc_epoch(), 1, "growth must bump reloc_epoch");
        // Capacity at least doubled, so a long run of pushes is stable again.
        assert!(frames.stable_headroom() >= FRAME_STACK_INITIAL_STABLE_CAP - 1);

        // truncate/clear keep the (now larger) stable capacity and move nothing.
        let cap = frames.stable_headroom() + frames.len();
        frames.truncate(10);
        assert_eq!(frames.reloc_epoch(), 1);
        assert_eq!(frames.stable_headroom() + frames.len(), cap);
        frames.clear();
        assert!(frames.is_empty());
        assert_eq!(frames.stable_headroom(), cap);
    }

    /// The drop-in surface the rest of the VM depends on (`Deref` to
    /// `&[Frame]` for `stackwalker::capture_full_trace`, iteration by
    /// reference, `Index`, `last`/`get`).
    #[test]
    fn frame_stack_is_a_drop_in_for_vec_frame() {
        let mut frames = FrameStack::new();
        for i in 0..4 {
            frames.push(test_frame(&format!("m{i}"), 1));
        }

        // Deref to a slice — what `capture_full_trace(&thread.frames)` needs.
        fn takes_slice(f: &[Frame]) -> usize {
            f.len()
        }
        assert_eq!(takes_slice(&frames), 4);
        assert_eq!(frames[..2].len(), 2);

        // Iteration in both directions, by shared and mutable reference.
        assert_eq!(frames.iter().count(), 4);
        assert_eq!(
            frames.iter().enumerate().rev().take(2).count(),
            2,
            "iter() must be DoubleEnded + ExactSize"
        );
        assert!(frames.iter().any(|f| f.method_name() == "m3"));
        for f in &frames {
            assert_eq!(f.class_name(), "Test");
        }
        for f in &mut frames {
            f.pc = 7;
        }
        assert!(frames.iter().all(|f| f.pc == 7));

        assert_eq!(frames.last().map(|f| f.method_name()), Some("m3"));
        frames.last_mut().expect("non-empty").pc = 9;
        assert_eq!(frames[3].pc, 9);
        assert!(frames.get(9).is_none());
        assert_eq!(frames.get_mut(0).map(|f| f.pc), Some(7));
        assert_eq!(
            frames.pop().map(|f| f.method_name().to_string()),
            Some("m3".to_string())
        );
        assert_eq!(frames.len(), 3);
    }

    // ===================================================================
    // Per-OS-thread SoA pool
    // ===================================================================

    /// A recycled buffer pair must be picked up by the non-pooled constructors
    /// and must produce a frame indistinguishable from a freshly allocated one
    /// — in particular with no stale locals and no stale kind marks.
    #[test]
    fn tls_soa_pool_is_reused_and_leaves_no_stale_state() {
        tls_soa_pool_clear();
        assert_eq!(tls_soa_pool_depths(), (0, 0));

        // Dirty a frame, then offer its buffers to the pool.
        let mut dirty = test_frame("dirty", 4);
        dirty.set_local(0, Value::Long(-1));
        dirty.set_local(2, Value::Double(2.5));
        dirty.stack.push(Value::Long(i64::MIN)).expect("push");
        let (lv, lt, sv, st) = dirty.take_pool_parts();
        offer_frame_parts_to_tls_pool(lv, lt, sv, st);
        assert_eq!(
            tls_soa_pool_depths(),
            (1, 1),
            "recycled buffers were not pooled"
        );

        // The next non-pooled construction must drain the pool …
        let reused = test_frame("reused", 4);
        assert_eq!(
            tls_soa_pool_depths(),
            (0, 0),
            "pooled buffers were not reused by Frame::new"
        );

        // … and must look exactly like a fresh frame.
        assert_eq!(reused.stack.len(), 0, "recycled operand stack not empty");
        for i in 0u16..4 {
            assert_eq!(
                reused.get_local(i),
                Value::Uninitialized,
                "recycled local {i} kept stale content"
            );
        }
        tls_soa_pool_clear();
    }

    /// A pooled frame must be byte-for-byte equivalent to an unpooled one —
    /// same locals, same argument copy-in, same operand-stack capacity.
    #[test]
    fn pooled_and_unpooled_frames_are_equivalent() {
        let args = [Value::Int(11), Value::Long(-7), Value::Object(None)];

        tls_soa_pool_clear();
        let fresh = Frame::new(
            ClassId::new(5),
            "Eq".to_string(),
            "m".to_string(),
            "(IJLjava/lang/Object;)V".to_string(),
            None,
            vec![0xb1],
            vec![],
            3,
            6,
            &args,
        );

        // Prime the pool with a dirty buffer pair, then build the same frame.
        let mut dirty = test_frame("dirty", 6);
        dirty.set_local(0, Value::Long(i64::MIN));
        dirty.set_local(1, Value::Double(-1.0));
        let (lv, lt, sv, st) = dirty.take_pool_parts();
        offer_frame_parts_to_tls_pool(lv, lt, sv, st);
        let pooled = Frame::new(
            ClassId::new(5),
            "Eq".to_string(),
            "m".to_string(),
            "(IJLjava/lang/Object;)V".to_string(),
            None,
            vec![0xb1],
            vec![],
            3,
            6,
            &args,
        );

        assert_eq!(pooled.max_locals, fresh.max_locals);
        for i in 0u16..fresh.max_locals {
            assert_eq!(
                pooled.get_local(i),
                fresh.get_local(i),
                "pooled local {i} differs from the unpooled frame"
            );
        }
        assert_eq!(pooled.stack.len(), fresh.stack.len());
        tls_soa_pool_clear();
    }

    /// Oversized buffers must not be pinned for the lifetime of the thread.
    #[test]
    fn tls_soa_pool_declines_oversized_buffers() {
        tls_soa_pool_clear();
        let huge = vec![0u64; TLS_SOA_MAX_RETAINED_SLOTS + 1];
        offer_frame_parts_to_tls_pool(huge, Vec::new(), Vec::new(), Vec::new());
        assert_eq!(
            tls_soa_pool_depths(),
            (0, 1),
            "oversized locals buffer must be dropped; the small stack half is \
             still poolable"
        );
        tls_soa_pool_clear();
    }

    /// The pool must saturate rather than grow without bound.
    #[test]
    fn tls_soa_pool_is_capped() {
        tls_soa_pool_clear();
        for _ in 0..(TLS_SOA_POOL_CAP * 2) {
            offer_frame_parts_to_tls_pool(Vec::new(), Vec::new(), Vec::new(), Vec::new());
        }
        assert_eq!(
            tls_soa_pool_depths(),
            (TLS_SOA_POOL_CAP, TLS_SOA_POOL_CAP),
            "TLS SoA pool exceeded its cap"
        );
        tls_soa_pool_clear();
    }

    // ===================================================================
    // Per-method padded-bytecode memo
    // ===================================================================

    #[test]
    fn padded_bytecode_memo_returns_the_same_arc_for_the_same_method() {
        let code = vec![0x1a, 0x04, 0x60, 0xac];
        let a = padded_bytecode_for_method(ClassId::new(4242), "memoA", "()I", &code);
        let b = padded_bytecode_for_method(ClassId::new(4242), "memoA", "()I", &code);
        assert!(Arc::ptr_eq(&a, &b), "identical method must reuse the Arc");
        assert_eq!(&a[..code.len()], &code[..]);
        assert_eq!(a.len(), code.len() + 2);
        assert_eq!(a[code.len()], 0);
        assert_eq!(a[code.len() + 1], 0);
    }

    /// Distinct methods must NEVER share an Arc even when their bodies are
    /// byte-identical — `local_liveness.rs` keys its per-method table on
    /// `Arc::as_ptr(code)` and computes it from the method's exception table,
    /// which is not part of the bytes.
    #[test]
    fn padded_bytecode_memo_never_merges_distinct_methods() {
        let code = vec![0xb1];
        let a = padded_bytecode_for_method(ClassId::new(7), "same", "()V", &code);
        let other_name = padded_bytecode_for_method(ClassId::new(7), "different", "()V", &code);
        let other_desc = padded_bytecode_for_method(ClassId::new(7), "same", "(I)V", &code);
        let other_class = padded_bytecode_for_method(ClassId::new(8), "same", "()V", &code);
        assert!(!Arc::ptr_eq(&a, &other_name));
        assert!(!Arc::ptr_eq(&a, &other_desc));
        assert!(!Arc::ptr_eq(&a, &other_class));
    }

    /// Redefinition rewrites the body under an unchanged identity; the memo
    /// must notice and mint a fresh Arc rather than serve the old bytes.
    #[test]
    fn padded_bytecode_memo_detects_a_redefined_body() {
        let v1 = vec![0xb1];
        let v2 = vec![0x04, 0xac];
        let a = padded_bytecode_for_method(ClassId::new(99), "redef", "()V", &v1);
        let b = padded_bytecode_for_method(ClassId::new(99), "redef", "()V", &v2);
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(&b[..v2.len()], &v2[..]);
        // The old Arc is untouched — existing frames keep executing old code.
        assert_eq!(&a[..v1.len()], &v1[..]);
    }

    /// `Frame::new` goes through the memo, so two frames for the same method
    /// share one bytecode allocation instead of copying the body per frame.
    #[test]
    fn frame_new_shares_bytecode_across_frames_of_the_same_method() {
        let f1 = Frame::new(
            ClassId::new(31337),
            "Shared".to_string(),
            "body".to_string(),
            "()V".to_string(),
            None,
            vec![0x03, 0xac],
            vec![],
            1,
            1,
            &[],
        );
        let f2 = Frame::new(
            ClassId::new(31337),
            "Shared".to_string(),
            "body".to_string(),
            "()V".to_string(),
            None,
            vec![0x03, 0xac],
            vec![],
            1,
            1,
            &[],
        );
        assert!(
            Arc::ptr_eq(&f1.code, &f2.code),
            "per-frame bytecode copy was not eliminated"
        );
        // Padding contract still holds (the hot loop reads code[pc+1..2]).
        assert!(f1.code.len() >= 2);
        assert_eq!(f1.code[f1.code.len() - 1], 0);
        assert_eq!(f1.code[f1.code.len() - 2], 0);
    }
}

#[cfg(test)]
mod frame_size_probe {
    use super::*;

    /// Not an assertion — a measurement printed so the fixed per-call cost can
    /// be reasoned about with a number instead of an estimate. `Frame` is moved
    /// by value into `FrameStack::push` on EVERY call, so its size is memory
    /// traffic paid per invocation.
    #[test]
    fn report_frame_size() {
        eprintln!("size_of::<Frame>()      = {}", std::mem::size_of::<Frame>());
        eprintln!(
            "size_of::<FrameInner>() = {}",
            std::mem::size_of::<FrameInner>()
        );
        eprintln!(
            "size_of::<ValueStack>() = {}",
            std::mem::size_of::<ValueStack>()
        );
        eprintln!(
            "align_of::<Frame>()     = {}",
            std::mem::align_of::<Frame>()
        );
    }
}

#[cfg(test)]
mod value_size_probe {
    #[test]
    fn report_value_size() {
        eprintln!(
            "size_of::<Value>()        = {}",
            std::mem::size_of::<cratonvm_types::Value>()
        );
        eprintln!(
            "16-slot args_buf bytes    = {}",
            16 * std::mem::size_of::<cratonvm_types::Value>()
        );
        eprintln!(
            "size_of::<CompactValue>() = {}",
            std::mem::size_of::<cratonvm_types::CompactValue>()
        );
    }
}

#[cfg(test)]
mod ic_entry_size_probe {
    /// `InvokeCache::get` hands back `&CachedInvokeTarget` and every caller
    /// immediately `.clone()`s it, so this is bytes copied per invoke on top of
    /// the Arc refcount traffic.
    #[test]
    fn report_ic_entry_size() {
        eprintln!(
            "size_of::<CachedInvokeTarget<RetainedCode>>() = {}",
            std::mem::size_of::<
                cratonvm_classloading::resolution::CachedInvokeTarget<cratonvm_jit::RetainedCode>,
            >()
        );
        eprintln!(
            "size_of::<RetainedCode>() = {}",
            std::mem::size_of::<cratonvm_jit::RetainedCode>()
        );
    }
}

#[cfg(test)]
mod frame_layout_probe {
    use super::*;

    /// Field-by-field accounting of the 296-byte `Frame`, which is built and
    /// then moved by value into the frame stack on EVERY interpreted call.
    #[test]
    fn report_frame_layout() {
        eprintln!("Frame                 = {}", std::mem::size_of::<Frame>());
        eprintln!(
            "  FrameInner          = {}",
            std::mem::size_of::<FrameInner>()
        );
        eprintln!(
            "  ValueStack          = {}",
            std::mem::size_of::<ValueStack>()
        );
        eprintln!(
            "  Vec<CompactValue>   = {}",
            std::mem::size_of::<Vec<CompactValue>>()
        );
        eprintln!("  Vec<u8>             = {}", std::mem::size_of::<Vec<u8>>());
        eprintln!(
            "  Vec<(usize,u32)>    = {}",
            std::mem::size_of::<Vec<(usize, u32)>>()
        );
        eprintln!(
            "  Arc<[u8]>           = {}",
            std::mem::size_of::<Arc<[u8]>>()
        );
        eprintln!(
            "  Option<ObjectRef>   = {}",
            std::mem::size_of::<Option<ObjectRef>>()
        );
        eprintln!("--- FrameInner variants ---");
        eprintln!(
            "  Arc<CachedBytecodeMethod> (Cached payload) = {}",
            std::mem::size_of::<Arc<CachedBytecodeMethod>>()
        );
        eprintln!(
            "  Owned payload (5 fat ptrs)                 = {}",
            std::mem::size_of::<Arc<str>>() * 3
                + std::mem::size_of::<Option<Arc<str>>>()
                + std::mem::size_of::<Arc<[ExceptionTableEntry]>>()
        );
    }
}
