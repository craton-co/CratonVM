// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A single execution frame (stack frame) in the JVM.
//!
//! Each method invocation creates a new `Frame` containing:
//! - Local variables (`CompactValue` slots, 8 bytes each, NaN-boxed tag inline)
//! - Operand stack (also NaN-boxed `CompactValue` slots, via `ValueStack`)
//!
//! Both are `SlotVec`s (`runtime::slot_slab`): owned buffers, or one window of
//! the `FrameStack`'s slot slab for a frame the cached install paths built
//! (stage 1 of the contiguous interpreter stack, interpreter round i1 wave 29).
//! - Program counter
//! - Method bytecode and exception table

use std::collections::HashMap;
use std::sync::Arc;

use cratonvm_reader::attribute::ExceptionTableEntry;

use crate::classloading::resolution::CachedBytecodeMethod;
use crate::classloading::ClassId;
use crate::runtime::slot_slab::{SlabWindow, SlotSlab, SlotVec, SLAB_MARK_UNKNOWN};
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

/// Frame-shape census of every interpreter frame retired through
/// `pop_and_recycle_frame_with_reason`, for `CRATONVM_DBG_INVOKE_PHASES=1`:
/// `[frames, locals slots, locals slots allocated, operand-stack slots
/// allocated, declared max_stack, declared max_locals]`.
///
/// Stage 0 of
/// `docs/known-issues/interpreter/i1-L4-proposal-contiguous-interpreter-stack-20260923.md`.
/// The phase table says how much TIME the frame lifecycle costs; this says how
/// much MEMORY a frame holds against what a contiguous slot stack would give it
/// (`max_locals + max_stack` slots, arguments overlapped), so the proposal's
/// memory claim is measured rather than argued.
///
/// Wave 5: the locals half is counted by CAPACITY as well as length (a pooled
/// buffer keeps the capacity of the largest frame it ever served), and the
/// "needed" side uses the DECLARED `max_locals` rather than `locals.len()`,
/// which the frame builder may pad; before, "held" undercounted the locals and
/// "needed" overcounted them.
///
/// Wave 29 (lane L7): a seventh column counts the retired frames whose locals
/// and operand stack were a window of the frame stack's slot slab
/// (`runtime::slot_slab`, stage 1 of the contiguous-stack proposal) -- the
/// positive control for `CRATONVM_JIT_NO_LOCALS_SLAB`: near `retired` with the
/// slab on, 0 with it off. A window's memory is released at the pop, so a
/// windowed frame holds exactly its window, which "held" counts.
static FRAME_SHAPE: [std::sync::atomic::AtomicU64; 7] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Record one retiring frame's shape. One relaxed byte load when the
/// instrument is off.
#[inline]
pub(crate) fn note_retired_frame_shape(frame: &Frame) {
    if !crate::runtime::interpreter::invoke_phases::on() {
        return;
    }
    let shape = [
        1,
        frame.locals_len() as u64,
        frame.locals.capacity() as u64,
        frame.stack.capacity() as u64,
        u64::from(frame.max_stack),
        u64::from(frame.max_locals),
        u64::from(frame.locals.is_window()),
    ];
    for (counter, v) in FRAME_SHAPE.iter().zip(shape) {
        counter.fetch_add(v, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Print the census (called from `invoke_phases::dump`). Prints zeros too: a
/// census of zero frames means the retire path it hooks never ran.
pub fn dump_frame_shape_census() {
    let [frames, locals, locals_cap, stack_cap, max_stack, max_locals, windowed] = FRAME_SHAPE
        .each_ref()
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed));
    let per = |v: u64| {
        if frames == 0 {
            0.0
        } else {
            v as f64 / frames as f64
        }
    };
    // Each slot is an 8-byte `CompactValue` plus a 1-byte kind mark.
    let held = (locals_cap + stack_cap) * 9;
    let needed = (max_locals + max_stack) * 9;
    eprintln!(
        "[invoke-phases] frame shape: retired={frames} locals={:.1} locals_alloc={:.1} \
         max_locals={:.1} stack_alloc={:.1} max_stack={:.1} slots/frame; slot bytes \
         held={:.1} vs max_locals+max_stack={:.1} per frame; Frame header={} bytes",
        per(locals),
        per(locals_cap),
        per(max_locals),
        per(stack_cap),
        per(max_stack),
        per(held),
        per(needed),
        std::mem::size_of::<Frame>(),
    );
    // A separate line, so the one above keeps the shape the wave-5 analysis
    // parses.
    eprintln!(
        "[invoke-phases] slot slab: windowed={windowed} of retired={frames} \
         (CRATONVM_JIT_NO_LOCALS_SLAB={})",
        if crate::runtime::env_cache::no_locals_slab() {
            "set: pooled buffers"
        } else {
            "unset: slab windows"
        },
    );
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
    /// Set only on a frame moved onto a body its class's redefinition
    /// replaced ([`Frame::adopt_redefined_body`]); `None` everywhere else.
    pub replaced_body: Option<Arc<ReplacedBody>>,
}

/// What a frame running a method body its class's redefinition replaced (JEP
/// 109) needs besides its translated code, where the class's current method
/// cannot answer for it (interpreter round i1 wave 21, lane L4;
/// `interpreter::obsolete_frames`).
#[derive(Debug)]
pub struct ReplacedBody {
    /// The body's own `LineNumberTable`, sorted by `start_pc`, when it
    /// differs from the line table of the class's method as of the frame's
    /// stamp (`None`: that table is the body's). A stack trace through the
    /// frame reports these lines, as HotSpot's obsolete `Method*` does.
    pub(crate) line_numbers: Option<Arc<[cratonvm_reader::attribute::LineNumberEntry]>>,
    /// The one-byte `ldc` sites whose constant the merged pool holds above
    /// index 255, as `(pc, index)`: their operand byte in the frame's code is
    /// stale, and the frame runs on [`Self::widened_stream`] instead.
    pub(crate) widened_ldc: Box<[(usize, u16)]>,
    /// The decoded stream that reads [`Self::widened_ldc`]
    /// (`QuickenedCode::with_widened_ldc`); `Some` iff that list is
    /// non-empty. Such a frame never runs on the raw-bytecode fast path.
    pub(crate) widened_stream: Option<Arc<cratonvm_reader::QuickenedCode>>,
    /// The code allocations this frame ran before its moves (and was not
    /// moved back onto): a dispatch loop suspended inside one of the frame's
    /// instructions when it moved may still hold a raw pointer into the code
    /// it loaded at its last top. Kept with the frame, so they are freed
    /// when it is popped -- by its own loop, from an iteration on its current
    /// code -- as its current code already is, instead of when the thread
    /// ends (interpreter round i1 wave 22, lane L3). A move whose bytes equal
    /// one of these goes back onto it (`obsolete_frames::convert_frames`), so
    /// an agent toggling a class between two versions keeps at most two.
    pub(crate) retired_code: Box<[Arc<[u8]>]>,
    /// Set by the dispatch loop when it reaches its top on this frame running
    /// the code this body was attached with (the fast-path gate's refresh,
    /// which every move reaches: a move changes the code allocation and bumps
    /// [`FrameStack::code_moves`]). From then on no loop iteration can hold a
    /// raw pointer into [`Self::retired_code`], so the next move frees those
    /// copies instead of keeping them until the pop (interpreter round i1
    /// wave 23, lane L3;
    /// `docs/internal/fixed-bugs/interpreter-L3-retired-obsolete-code-is-kept-for-the-threads-lifetime-FIXED-20260927.md`).
    /// Only the owning thread reads or writes it.
    ///
    /// Also set for every frame below the loop's current one in its range
    /// when a move brought the refresh (interpreter round i1 wave 24, lane
    /// L3): no iteration is suspended in any of their code.
    pub(crate) code_reached_top: std::sync::atomic::AtomicBool,
    /// The frame's code was installed together with this body by a move onto
    /// a new allocation (not moved back onto one it kept, not left as it
    /// was). While [`Self::code_reached_top`] is still unset, no dispatch
    /// loop has loaded that code at a top -- the first top to load it
    /// refreshes the loop's gate, which a move always forces
    /// (`FrameStack::note_code_moved`) -- so no iteration holds a pointer into
    /// it, and the next move frees it instead of retiring it (interpreter
    /// round i1 wave 24, lane L3). A frame suspended inside one instruction
    /// across many moves then keeps the copy that iteration holds and its
    /// current one, not one per move. This rests on the gate's move-count
    /// compare: without it, a new copy allocated at the address of a freed
    /// one the gate last saw would be loaded with no refresh.
    pub(crate) fresh_code: bool,
    /// The decoded stream of the frame's code, built the first time the
    /// dispatch loop runs the frame on its decoded path
    /// ([`Self::decoded_stream`]). Interpreter round i1 wave 25, lane L3:
    /// such a frame used to go to the process-wide intern table
    /// (`cratonvm_reader::quickened::intern`), whose entry holds a strong
    /// reference to the code it decoded -- so every private copy a moved frame
    /// ran on that path stayed allocated until its shard happened to be
    /// cleared, long after the frame had moved on or been popped. Kept here,
    /// the stream lives exactly as long as the body it decodes.
    pub(crate) decoded: std::sync::OnceLock<Option<Arc<cratonvm_reader::QuickenedCode>>>,
}

impl ReplacedBody {
    /// The decoded stream of `code`, the code of the frame this body is
    /// attached to: [`Self::widened_stream`] when its `ldc`s need one, else
    /// built once and kept here (see [`Self::decoded`]). A body is attached
    /// together with the code it describes and replaced whenever the frame
    /// moves onto another allocation, so the stream is always of `code`.
    pub(crate) fn decoded_stream(
        &self,
        code: &Arc<[u8]>,
    ) -> Option<Arc<cratonvm_reader::QuickenedCode>> {
        if let Some(stream) = &self.widened_stream {
            return Some(Arc::clone(stream));
        }
        self.decoded
            .get_or_init(|| cratonvm_reader::QuickenedCode::build(code))
            .clone()
    }

    /// Nothing to carry: the class's method answers for everything and the
    /// frame never ran another code allocation.
    pub(crate) fn is_empty(&self) -> bool {
        self.line_numbers.is_none() && self.widened_stream.is_none() && self.retired_code.is_empty()
    }

    /// The dispatch loop reached its top on the frame this body belongs to
    /// (see [`Self::code_reached_top`]).
    #[inline]
    pub(crate) fn note_code_reached_top(&self) {
        self.code_reached_top
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Has a dispatch loop reached its top on the frame since this body was
    /// attached?
    #[inline]
    pub(crate) fn code_reached_top(&self) -> bool {
        self.code_reached_top
            .load(std::sync::atomic::Ordering::Relaxed)
    }
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
/// Locals are stored as `CompactValue` slots (8 bytes/slot, NaN-boxed tag
/// embedded in the high bits of the u64) in a `SlotVec`: an owned buffer, or
/// the front of the frame's slot-slab window (see `FrameStack`'s `slab`). This collapses what used to be a
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
    ///
    /// An owned buffer, or the front of this frame's window in its
    /// `FrameStack`'s slot slab (`runtime::slot_slab`; the cached install
    /// paths, unless `CRATONVM_JIT_NO_LOCALS_SLAB`). Every accessor indexes it
    /// as a slice, so the choice costs the readers nothing.
    locals: SlotVec<CompactValue>,

    /// Per-local kind marks paralleling `locals` (see [`LKIND_OTHER`]).
    /// `local_kinds[i]` is `LKIND_LONG` / `LKIND_DOUBLE` iff slot `i` currently
    /// holds a primitive `long` / `double`. Maintained alongside `locals` by
    /// every setter; its sole consumer is the GC root scan/update, which must
    /// never treat a primitive cat-2 value as a heap reference even when its
    /// NaN-boxed bits collide with the `SUB_OBJECT` pattern (BouncyCastle F2m
    /// `LongArray` `0xfffd_…` words). Mirrors `ValueStack::kinds`. Reuses the
    /// `Vec<u8>` half of the frame pool tuple that the 2026-05-16 SoA collapse
    /// left unused. Owned or windowed together with `locals`.
    local_kinds: SlotVec<u8>,

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

    /// The `backward_count` at which this activation's next out-of-line OSR
    /// poll (`interpreter::try_osr_with_backoff`) is due; `0` = at the VM's
    /// back-edge floor, as before this field existed.
    ///
    /// Set by `try_osr_with_backoff` whenever it declines, to the loop
    /// header's own next due point (`osr_threshold << attempts`, never for a
    /// retired header) or `OSR_POLL_STRIDE` back edges on, whichever is
    /// sooner: HotSpot's back-edge notify frequency. Before it, a frame whose
    /// loop header was refused and retired made the out-of-line call on every
    /// back edge past the floor for the rest of the activation, only to be
    /// told "no" by `should_try_osr`. Compared only once `backward_count` is
    /// past the floor, so a loop below the floor pays nothing for it. Reset
    /// with `backward_count` (interpreter round i1 wave 16, lane L5).
    pub osr_poll_at: u32,

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

    /// The `FrameStack`'s slot-slab mark when this frame was pushed: popping
    /// it (or truncating to its depth) releases the slab to this mark, which
    /// frees this frame's window and every window above it. Set by every
    /// `FrameStack` push, on owned frames too, so a pop needs no test of what
    /// the frame holds; `SLAB_MARK_UNKNOWN` on a frame no stack has pushed,
    /// which releases nothing (`SlotSlab::release_to` only moves down).
    slab_mark: u64,

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
    /// worse than no index, and the place that can strand one —
    /// [`Frame::from_frozen_frame`] — clears it explicitly rather than by
    /// omission. (The other, `Frame::reset_for_tail_call`, was deleted with
    /// the self-recursive tail-call elimination it served.)
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

    /// The objects this frame's own `monitorenter`s locked and its
    /// `monitorexit`s have not released (JVMS §2.11.10 structured locking;
    /// `interpreter::held_monitors`, interpreter round i1 wave 23, lane L7).
    /// Inline for up to two entries, so a frame without block monitors pays
    /// nothing. Object references: rooted and remapped wherever
    /// `monitor_on_exit` is.
    pub(crate) held_monitors: crate::runtime::interpreter::held_monitors::HeldMonitors,

    /// `cratonvm_classloading::class_redefinition_count()` when this frame was
    /// built, i.e. which generation of its class's constant pool its `code`
    /// indexes; [`OBSOLETE_METHOD_STAMP_BIT`] set once the frame was moved
    /// onto a body its class's redefinition replaced and that differs from
    /// the current one (a JDWP / JVMTI obsolete method). Read through
    /// [`Self::redefine_stamp`] / [`Self::runs_obsolete_method`]; written at
    /// construction and by [`Self::adopt_redefined_body`] only (interpreter
    /// round i1 wave 19, lane L3, `interpreter::obsolete_frames`).
    /// [`PREDATES_REDEFINITION_STAMP_BIT`] set once the frame was moved
    /// across a redefinition of its class at all, obsolete or EMCP
    /// ([`Self::predates_its_class_redefinition`], wave 39).
    redefine_stamp: u64,
}

/// The bit of `Frame::redefine_stamp` that marks a frame running an obsolete
/// method (see [`Frame::runs_obsolete_method`]). The count itself never gets
/// near it.
pub(crate) const OBSOLETE_METHOD_STAMP_BIT: u64 = 1 << 63;

/// The bit of `Frame::redefine_stamp` that marks an activation begun before
/// its class's redefinition, which a conversion moved across it (see
/// [`Frame::predates_its_class_redefinition`]; interpreter round i1 wave 39,
/// lane L3). The count itself never gets near it either.
pub(crate) const PREDATES_REDEFINITION_STAMP_BIT: u64 = 1 << 62;

/// Both mark bits of `Frame::redefine_stamp`.
const REDEFINE_STAMP_MARK_BITS: u64 = OBSOLETE_METHOD_STAMP_BIT | PREDATES_REDEFINITION_STAMP_BIT;

/// The stamp a frame built now carries (see `Frame::redefine_stamp`): one
/// `Acquire` load of a counter written only by class redefinition.
#[inline]
fn current_redefine_stamp() -> u64 {
    cratonvm_classloading::class_redefinition_count() & !REDEFINE_STAMP_MARK_BITS
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

/// Cached `CRATONVM_DBG_BUG03` gate (the per-slot remap trace in
/// [`Frame::update_local_refs`]).
#[inline]
fn bug03_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BUG03").is_some())
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

/// The root a `long` local contributes: its object when the bits are a live
/// minted smuggled handle for `heap` (`memory::smuggled_longs`), else none.
/// Costs one relaxed load when nothing in the process was ever minted.
#[inline]
fn minted_long_local_root(heap: &crate::memory::VmHeap, bits: u64) -> Option<ObjectRef> {
    if bits == 0 || bits & 0x7 != 0 || !crate::memory::smuggled_longs::is_minted(heap, bits) {
        return None;
    }
    heap.is_object_address(bits as usize)
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
/// `CRATONVM_DBG_DEADREF_STORE`: report an argument that is already dead at the
/// moment it is laid into a callee frame's locals.
///
/// This is the choke point the `set_local` guard cannot see. An invoke pops its
/// arguments off the caller's operand stack into a Rust slice, and only then
/// builds the callee frame — resolving the method, initialising its class, and
/// carving the frame's buffers, any of which can run a moving young collection.
/// The slice is not a GC root and is not remapped, so a collection there leaves
/// every reference in it naming the pre-move address, and the frame is then
/// built from those. Nothing downstream can attribute it: the value arrives in
/// the callee's `local[n]` looking exactly like one the caller passed.
///
/// Cheap and off by default — a `OnceLock<bool>` load and a predicted
/// not-taken branch per argument.
#[inline]
fn note_dead_arg(args: &[Value], site: &'static str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| cratonvm_types::flags().gc.dbg_deadref_store) {
        return;
    }
    for (i, a) in args.iter().enumerate() {
        let Value::Object(Some(o)) = a else { continue };
        let Some(reason) = cratonvm_gc::gen_heap::dead_young_ref_reason_global(o.as_ptr() as usize)
        else {
            continue;
        };
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
            eprintln!(
                "[deadref-arg] {reason} {site}: arg[{i}] = 0x{:x} is already dead as it is laid \
                 into the callee's locals — the argument slice was held across a collection. \
                 caller:
{:?}",
                o.as_ptr() as usize,
                std::backtrace::Backtrace::force_capture(),
            );
        }
    }
}

/// Public shim for [`note_dead_arg`], so the invoke paths can ask the same
/// question at their own entry — see the call in `try_stackless_invoke`.
pub(crate) fn note_dead_arg_pub(args: &[Value], site: &'static str) {
    note_dead_arg(args, site);
}

fn push_args_to_locals(
    locals: &mut Vec<CompactValue>,
    kinds: &mut Vec<u8>,
    args: &[Value],
    cap: usize,
) {
    note_dead_arg(args, "push_args_to_locals");
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
/// Test-only since 2026-09-23: its last production caller was the deleted
/// `Frame::reset_for_tail_call`. It stays as the reference layout
/// `pushed_locals_match_the_resize_then_copy_layout` checks
/// [`push_args_to_locals`] against — see that function for why the push path
/// differs.
#[cfg(test)]
fn copy_args_to_locals(locals: &mut [CompactValue], kinds: &mut [u8], args: &[Value]) {
    note_dead_arg(args, "copy_args_to_locals");
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
    note_dead_compact_args(cached, args);
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

/// The compact half of `note_dead_arg` — same choke point, same reason, but
/// the values arrive already encoded so the check reads the pointer out of
/// the `CompactValue` instead of a `Value`. Shared by the pooled build
/// ([`build_cached_compact_parts`]) and the slab-window emplace
/// (`FrameStack::emplace_cached_compact_args`), which lay the same arguments.
#[inline]
fn note_dead_compact_args(cached: &CachedBytecodeMethod, args: &[(CompactValue, u8)]) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| cratonvm_types::flags().gc.dbg_deadref_store) {
        for (i, (cv, _)) in args.iter().enumerate() {
            let Some(p) = cv.as_object_ptr() else {
                continue;
            };
            let Some(reason) = cratonvm_gc::gen_heap::dead_young_ref_reason_global(p as usize)
            else {
                continue;
            };
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                eprintln!(
                    "[deadref-arg] {reason} build_cached_compact_parts: arg[{i}] = 0x{p:x} \
                     is already dead as it is laid into {}'s locals — the argument slice was \
                     held across a collection. caller:
{:?}",
                    cached.method_name,
                    std::backtrace::Backtrace::force_capture(),
                );
            }
        }
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

// ---------------------------------------------------------------------------
// Slab windows (stage 1 of the contiguous interpreter stack, wave 29 lane L7)
// ---------------------------------------------------------------------------
//
// A frame built by the cached install paths takes ONE window of its
// `FrameStack`'s slot slab (`runtime::slot_slab`): `n` local slots, then the
// padded operand stack, each slot with its kind byte. The functions below lay
// the locals exactly as the pooled builders above do, so a windowed frame and
// a pooled one hold the same bytes; only where the bytes live differs.

/// Is the slot slab on? `CRATONVM_JIT_NO_LOCALS_SLAB` turns it off, and every
/// cached install then builds its buffers from the thread pools as before.
#[inline(always)]
fn locals_slab_enabled() -> bool {
    !crate::runtime::env_cache::no_locals_slab()
}

/// The operand-stack slots every frame builder gives a frame of `cached`
/// (`max(max_stack, 16) + 8`, as the pooled builders size theirs).
#[inline(always)]
fn padded_operand_stack(cached: &CachedBytecodeMethod) -> usize {
    (cached.max_stack as usize).max(16) + 8
}

/// `(local slots, effective max_locals)` for compact arguments: the declared
/// `max_locals`, raised to what the arguments occupy (a `long` / `double`
/// takes two), exactly as [`build_cached_compact_parts`] computes it.
#[inline(always)]
fn compact_locals_len(cached: &CachedBytecodeMethod, args: &[(CompactValue, u8)]) -> (usize, u16) {
    let needed: usize = args
        .iter()
        .map(|(_, t)| if matches!(*t, b'J' | b'D') { 2 } else { 1 })
        .sum();
    let n = (cached.max_locals as usize).max(needed);
    (n, u16::try_from(n).unwrap_or(u16::MAX))
}

/// Lay compact arguments and the filler into the first `n` slots of a slab
/// window: slot for slot what [`build_cached_compact_parts`] pushes (a
/// category-2 argument's upper slot is `uninitialized` / `LKIND_OTHER`, every
/// slot past the arguments the same).
///
/// # Safety
///
/// `vals` and `kinds` point at `n` writable slots, and `n` is at least the
/// slots the arguments occupy ([`compact_locals_len`]).
#[inline(always)]
unsafe fn lay_compact_args(
    vals: *mut CompactValue,
    kinds: *mut u8,
    n: usize,
    args: &[(CompactValue, u8)],
) {
    let mut i = 0usize;
    for &(cv, tag) in args {
        vals.add(i).write(cv);
        match tag {
            b'J' | b'D' => {
                kinds.add(i).write(if tag == b'J' {
                    LKIND_LONG
                } else {
                    LKIND_DOUBLE
                });
                vals.add(i + 1).write(CompactValue::uninitialized());
                kinds.add(i + 1).write(LKIND_OTHER);
                i += 2;
            }
            _ => {
                kinds.add(i).write(LKIND_OTHER);
                i += 1;
            }
        }
    }
    while i < n {
        vals.add(i).write(CompactValue::uninitialized());
        kinds.add(i).write(LKIND_OTHER);
        i += 1;
    }
}

/// Lay the locals of an OVERLAPPED window (stage 2 of the contiguous
/// interpreter stack, `FrameStack::push_cached_compact_overlapping`): its
/// first `args.len()` slots are the caller's argument slots, still holding
/// the arguments the door read from them. Returns `true` when the locals had
/// to be laid again from `args` (a category-2 argument).
///
/// * No `long` / `double` argument: the operand stack and the locals agree
///   slot for slot, so the values stay where they are. Only the kind bytes
///   are rewritten -- a category-1 argument's local mark is `LKIND_OTHER`,
///   exactly what [`lay_compact_args`] writes -- and the slots past the
///   arguments get the filler.
/// * Otherwise each category-2 argument takes two local slots against one
///   operand-stack slot, so the arguments must move up. They are laid again
///   from `args` by [`lay_compact_args`]: `args` is the door's own buffer,
///   so the writes cannot clobber a value still to be read.
///
/// Either way the `n` slots hold exactly what [`lay_compact_args`] would
/// write into a fresh window (`an_overlapped_frame_holds_what_a_fresh_window_holds`).
///
/// # Safety
///
/// `vals` and `kinds` point at `n` writable slots of a live window whose
/// first `args.len()` value slots hold `args`' values in order; `n` is at
/// least the slots the arguments occupy ([`compact_locals_len`]); `args` does
/// not alias the window.
#[inline(always)]
unsafe fn lay_overlapped_args(
    vals: *mut CompactValue,
    kinds: *mut u8,
    n: usize,
    args: &[(CompactValue, u8)],
) -> bool {
    if args.iter().any(|&(_, t)| matches!(t, b'J' | b'D')) {
        lay_compact_args(vals, kinds, n, args);
        return true;
    }
    let nargs = args.len();
    debug_assert!(nargs <= n);
    debug_assert!(
        args.iter()
            .enumerate()
            .all(|(i, (cv, _))| vals.add(i).read().raw_bits() == cv.raw_bits()),
        "an overlapped window must start on the arguments the door read"
    );
    std::ptr::write_bytes(kinds, LKIND_OTHER, nargs);
    let mut i = nargs;
    while i < n {
        vals.add(i).write(CompactValue::uninitialized());
        kinds.add(i).write(LKIND_OTHER);
        i += 1;
    }
    false
}

/// Lay the locals of an overlapped window IN PLACE, with no copy of the
/// arguments anywhere (stage 2b of the contiguous interpreter stack,
/// `FrameStack::push_cached_compact_in_place`; interpreter round i1 wave 38,
/// lane L7). The window's first `tags.len()` value slots are the caller's
/// argument slots, still holding the arguments in operand-stack form (one slot
/// each); `tags` are their descriptor tags (`L` for a receiver). Returns `true`
/// when a category-2 argument made the arguments move.
///
/// * No `long` / `double`: [`lay_overlapped_args`]'s first arm -- the values
///   stay, the kind bytes become `LKIND_OTHER`.
/// * Otherwise argument `i` moves up by the number of category-2 arguments
///   before it. The arguments are laid from the LAST to the first: every
///   destination is at or above its source, and at or above every source not
///   yet read, so no value is overwritten before it is read. A category-2
///   argument writes its upper filler slot too, which lies below the next
///   argument's destination.
///
/// Either way the `n` slots end up exactly as [`lay_compact_args`] writes a
/// fresh window (`an_in_place_overlap_lays_what_a_fresh_window_holds`).
///
/// # Safety
///
/// `vals` and `kinds` point at `n` writable slots of a live window whose
/// first `tags.len()` value slots hold the arguments in order, and `n` is at
/// least the local slots the arguments occupy.
#[inline(always)]
unsafe fn lay_overlapped_args_in_place(
    vals: *mut CompactValue,
    kinds: *mut u8,
    n: usize,
    tags: &[u8],
) -> bool {
    let nargs = tags.len();
    let cat2 = tags.iter().filter(|&&t| matches!(t, b'J' | b'D')).count();
    let occupied = nargs + cat2;
    debug_assert!(occupied <= n);
    if cat2 == 0 {
        std::ptr::write_bytes(kinds, LKIND_OTHER, nargs);
    } else {
        // One past the last local slot still to be written.
        let mut dest_end = occupied;
        for i in (0..nargs).rev() {
            let tag = tags[i];
            let v = vals.add(i).read();
            if matches!(tag, b'J' | b'D') {
                dest_end -= 2;
                vals.add(dest_end).write(v);
                kinds.add(dest_end).write(if tag == b'J' {
                    LKIND_LONG
                } else {
                    LKIND_DOUBLE
                });
                vals.add(dest_end + 1).write(CompactValue::uninitialized());
                kinds.add(dest_end + 1).write(LKIND_OTHER);
            } else {
                dest_end -= 1;
                vals.add(dest_end).write(v);
                kinds.add(dest_end).write(LKIND_OTHER);
            }
        }
        debug_assert_eq!(dest_end, 0);
    }
    let mut i = occupied;
    while i < n {
        vals.add(i).write(CompactValue::uninitialized());
        kinds.add(i).write(LKIND_OTHER);
        i += 1;
    }
    cat2 != 0
}

/// [`lay_compact_args`] for arguments that are still `Value`s: slot for slot
/// what [`push_args_to_locals`] followed by the filler `resize` lays,
/// including its clamp at `n`.
///
/// # Safety
///
/// `vals` and `kinds` point at `n` writable slots.
#[inline]
unsafe fn lay_value_args(vals: *mut CompactValue, kinds: *mut u8, n: usize, args: &[Value]) {
    note_dead_arg(args, "push_args_to_locals");
    let mut i = 0usize;
    for arg in args {
        if i >= n {
            break;
        }
        vals.add(i).write(CompactValue::from_value_kinded(*arg));
        kinds.add(i).write(lkind_of_value(arg));
        i += 1;
        if arg.is_category2() && i < n {
            vals.add(i).write(CompactValue::uninitialized());
            kinds.add(i).write(LKIND_OTHER);
            i += 1;
        }
    }
    while i < n {
        vals.add(i).write(CompactValue::uninitialized());
        kinds.add(i).write(LKIND_OTHER);
        i += 1;
    }
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
    #[cfg(test)]
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

    /// This activation's OSR attempt count at `entry_pc`, `None` before its
    /// first offer there. The interpreter opens the entry (at 0) when it asks
    /// the method-wide budget, so `None` means "not asked yet" in this
    /// activation (`jit_bridge::osr_loop_offer_allowed`).
    #[inline]
    pub fn osr_attempts_at(&self, entry_pc: usize) -> Option<u32> {
        self.osr_attempt_counts
            .iter()
            .find(|(pc, _)| *pc == entry_pc)
            .map(|(_, n)| *n)
    }

    /// Set this activation's OSR attempt count at `entry_pc`:
    /// `OSR_MAX_ATTEMPTS` retires the loop header for the rest of the
    /// activation (a verdict the remaining offers could not change), `0`
    /// opens the entry without spending anything.
    #[inline]
    pub fn set_osr_attempts(&mut self, entry_pc: usize, attempts: u32) {
        for (pc, n) in &mut self.osr_attempt_counts {
            if *pc == entry_pc {
                *n = attempts;
                return;
            }
        }
        self.osr_attempt_counts.push((entry_pc, attempts));
    }

    /// Throttle the next poll while an off-thread OSR compile is pending without
    /// consuming the bounded permanent-rejection budget.
    #[inline]
    pub fn record_osr_background_pending(&mut self) {
        self.backward_count = 0;
        self.osr_poll_at = 0;
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
            locals: SlotVec::from_vec(locals),
            local_kinds: SlotVec::from_vec(local_kinds),
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
                    replaced_body: None,
                }))
            },
            // The caller passes loose parts, not a resolved method; use
            // `set_method_index` where the slot is known.
            method_index: None,
            backward_count: 0,
            osr_poll_at: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            held_monitors: crate::runtime::interpreter::held_monitors::HeldMonitors::new(),
            seq: next_frame_seq(),
            exec_epoch: 0,
            slab_mark: SLAB_MARK_UNKNOWN,
            redefine_stamp: current_redefine_stamp(),
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
            locals: SlotVec::from_vec(locals),
            local_kinds: SlotVec::from_vec(local_kinds),
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
                    replaced_body: None,
                }))
            },
            // Same as `Frame::new`: loose Arcs, no resolved method slot.
            method_index: None,
            backward_count: 0,
            osr_poll_at: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            held_monitors: crate::runtime::interpreter::held_monitors::HeldMonitors::new(),
            seq: next_frame_seq(),
            exec_epoch: 0,
            slab_mark: SLAB_MARK_UNKNOWN,
            redefine_stamp: current_redefine_stamp(),
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
            locals: SlotVec::from_vec(locals),
            local_kinds: SlotVec::from_vec(local_kinds),
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
                    replaced_body: None,
                }))
            },
            // Same as `Frame::new`: loose Arcs, no resolved method slot.
            method_index: None,
            backward_count: 0,
            osr_poll_at: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            held_monitors: crate::runtime::interpreter::held_monitors::HeldMonitors::new(),
            seq: next_frame_seq(),
            exec_epoch: 0,
            slab_mark: SLAB_MARK_UNKNOWN,
            redefine_stamp: current_redefine_stamp(),
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
            locals: SlotVec::from_vec(locals),
            local_kinds: SlotVec::from_vec(kinds),
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
            osr_poll_at: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            held_monitors: crate::runtime::interpreter::held_monitors::HeldMonitors::new(),
            seq: next_frame_seq(),
            exec_epoch: 0,
            slab_mark: SLAB_MARK_UNKNOWN,
            redefine_stamp: current_redefine_stamp(),
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
    ///
    /// The locals are taken out as `Vec`s and put back (wave 29): a slab
    /// window yields an empty `Vec` there (`SlotVec::take_vec`), so a retired
    /// frame's window -- memory the slab may already have handed to another
    /// frame -- is replaced by an owned buffer, never written.
    pub fn reset_cached_compact(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        args: &[(CompactValue, u8)],
    ) {
        let (n, eff_max_locals) = compact_locals_len(&cached, args);

        // An owned buffer that already holds `n` slots is rewritten where it
        // is (interpreter round i1 wave 32): the `Vec` round trip below cost
        // every pooled reuse two header moves per buffer, which is where the
        // wave-29 call rows went. `lay_compact_args` lays exactly the slots
        // the pushes below do (`a_windowed_frame_holds_what_a_pooled_frame_holds`).
        if self.locals.owned_capacity() >= n && self.local_kinds.owned_capacity() >= n {
            // SAFETY: both buffers are owned with room for `n` slots, and
            // `lay_compact_args` initialises all `n`.
            unsafe {
                lay_compact_args(
                    self.locals.as_mut_ptr(),
                    self.local_kinds.as_mut_ptr(),
                    n,
                    args,
                );
                self.locals.set_len(n);
                self.local_kinds.set_len(n);
            }
            self.reset_cached_tail(cached, eff_max_locals);
            return;
        }
        let mut locals = self.locals.take_vec();
        let mut kinds = self.local_kinds.take_vec();
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
        self.locals = SlotVec::from_vec(locals);
        self.local_kinds = SlotVec::from_vec(kinds);

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
        self.stack.reset_in_place(padded_operand_stack(&cached));
        self.reset_cached_scalars(cached, eff_max_locals);
    }

    /// Every field of a cached-frame reset except the four slot buffers
    /// (and `slab_mark`, which the caller sets with them): shared by the
    /// pooled resets above and the slab-window rebuild
    /// ([`Self::reset_cached_in_window`]), so the two storage layouts cannot
    /// drift in what they leave behind.
    #[inline(always)]
    fn reset_cached_scalars(&mut self, cached: Arc<CachedBytecodeMethod>, eff_max_locals: u16) {
        self.class_id = cached.declaring_class_id;
        self.pc = 0;
        self.last_instr_pc = 0;
        // A retired slot rebuilt for the method it last ran (recursion, and a
        // loop calling one callee: the common case of slot reuse) already
        // holds this code. Keeping it saves the clone's atomic increment and
        // the old `Arc`'s atomic decrement, two locked read-modify-writes per
        // call (interpreter round i1 wave 37, lane L7). A different body --
        // another method, or an obsolete frame's translated code
        // (`adopt_redefined_body`) -- is a different allocation and is
        // replaced as before.
        if !Arc::ptr_eq(&self.code, &cached.code) {
            self.code = cached.code.clone();
        }
        self.max_stack = cached.max_stack;
        self.max_locals = eff_max_locals;
        self.method_index = None;
        self.backward_count = 0;
        self.osr_poll_at = 0;
        self.osr_attempt_counts.clear();
        self.monitor_on_exit = None;
        self.held_monitors.clear();
        self.seq = next_frame_seq();
        self.exec_epoch = 0;
        self.redefine_stamp = current_redefine_stamp();
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

        // An owned buffer with room is rewritten in place, as in
        // `reset_cached_compact`; `lay_value_args` mirrors
        // `push_args_to_locals` and the filler `resize`, clamp included.
        if self.locals.owned_capacity() >= n && self.local_kinds.owned_capacity() >= n {
            // SAFETY: both buffers are owned with room for `n` slots, and
            // `lay_value_args` initialises all `n`.
            unsafe {
                lay_value_args(
                    self.locals.as_mut_ptr(),
                    self.local_kinds.as_mut_ptr(),
                    n,
                    args,
                );
                self.locals.set_len(n);
                self.local_kinds.set_len(n);
            }
            self.reset_cached_tail(cached, eff_max_locals);
            return;
        }
        // Otherwise taken out and put back (a window is never written through
        // a retired frame): see `reset_cached_compact`.
        let mut locals = self.locals.take_vec();
        let mut kinds = self.local_kinds.take_vec();
        locals.clear();
        kinds.clear();
        locals.reserve(n);
        kinds.reserve(n);
        push_args_to_locals(&mut locals, &mut kinds, args, n);
        locals.resize(n, CompactValue::uninitialized());
        kinds.resize(n, LKIND_OTHER);
        debug_assert_eq!(locals.len(), n);
        debug_assert_eq!(kinds.len(), n);
        self.locals = SlotVec::from_vec(locals);
        self.local_kinds = SlotVec::from_vec(kinds);

        self.reset_cached_tail(cached, eff_max_locals);
    }

    /// Rebuild this frame in place as a call to `cached` whose locals and
    /// operand stack are the slab window `w`: `n` local slots, which the
    /// caller has already laid ([`lay_compact_args`] / [`lay_value_args`]),
    /// then `padded` operand-stack slots.
    ///
    /// The slab-window twin of [`Self::reset_cached_compact`] /
    /// [`Self::reset_cached_value`]: the same scalar reset, and the locals the
    /// same bytes (the lay functions mirror the pooled builders slot for slot;
    /// `a_windowed_frame_holds_what_a_pooled_frame_holds` pins it). The
    /// frame's previous buffers are dropped (a window's drop is nothing; an
    /// owned buffer is freed, which only happens for a husk or a caller that
    /// did not harvest).
    ///
    /// # Safety
    ///
    /// `w` is a window of at least `n + padded` slots that `SlotSlab::alloc`
    /// just handed out for this frame, on the `FrameStack` that holds it.
    #[inline(always)]
    unsafe fn reset_cached_in_window(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        w: SlabWindow,
        n: usize,
        padded: usize,
        eff_max_locals: u16,
    ) {
        self.locals = SlotVec::window(w.vals, n, n);
        self.local_kinds = SlotVec::window(w.kinds, n, n);
        self.stack = ValueStack::from_window(
            std::ptr::NonNull::new_unchecked(w.vals.as_ptr().add(n)),
            std::ptr::NonNull::new_unchecked(w.kinds.as_ptr().add(n)),
            padded,
        );
        self.slab_mark = w.mark;
        self.reset_cached_scalars(cached, eff_max_locals);
    }

    /// A new frame for `cached` in the slab window `w` (see
    /// [`Self::reset_cached_in_window`], whose layout and fields it matches).
    ///
    /// # Safety
    ///
    /// As for [`Self::reset_cached_in_window`].
    #[inline(always)]
    unsafe fn cached_in_window(
        cached: Arc<CachedBytecodeMethod>,
        w: SlabWindow,
        n: usize,
        padded: usize,
        eff_max_locals: u16,
    ) -> Self {
        let class_id = cached.declaring_class_id;
        let code = cached.code.clone();
        let max_stack = cached.max_stack;
        count_frame_kind(false);
        Self {
            class_id,
            pc: 0,
            last_instr_pc: 0,
            locals: SlotVec::window(w.vals, n, n),
            local_kinds: SlotVec::window(w.kinds, n, n),
            stack: ValueStack::from_window(
                std::ptr::NonNull::new_unchecked(w.vals.as_ptr().add(n)),
                std::ptr::NonNull::new_unchecked(w.kinds.as_ptr().add(n)),
                padded,
            ),
            code,
            max_stack,
            max_locals: eff_max_locals,
            inner: FrameInner::Cached(cached),
            method_index: None,
            backward_count: 0,
            osr_poll_at: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            held_monitors: crate::runtime::interpreter::held_monitors::HeldMonitors::new(),
            seq: next_frame_seq(),
            exec_epoch: 0,
            slab_mark: w.mark,
            redefine_stamp: current_redefine_stamp(),
        }
    }

    /// Do this frame's locals begin inside its caller's operand stack (stage
    /// 2 of the contiguous interpreter stack, argument overlap:
    /// `FrameStack::push_cached_compact_overlapping`)? One bit test of a
    /// header field, for the value-return arms.
    #[inline(always)]
    pub(crate) fn locals_overlap_caller(&self) -> bool {
        self.locals.is_shared_with_caller()
    }

    /// Drop this returning frame's view of its locals and operand stack,
    /// right before a value return pushes the result into the caller.
    ///
    /// An overlapped frame's first slot is the caller's next operand-stack
    /// slot, and the return arms push the result there BEFORE this frame
    /// leaves (so the result is rooted across `MethodExit`, the synchronized
    /// monitor's release and `FramePop`, which may collect). Without this the
    /// slot would be one root of two frames for that span, and a moving
    /// collection would remap it twice -- wrong whenever an object's new
    /// address is another relocated object's old one. Nothing reads a
    /// returning frame's locals or operand stack after its value is taken:
    /// JVMTI cannot read locals here (`can_access_local_variables` is
    /// refused) and the door does not overlap under JDWP. The window stays
    /// the frame's until it is retired; only the lengths change.
    #[inline(always)]
    pub(crate) fn release_caller_overlap(&mut self) {
        // SAFETY: 0 is within any capacity and admits no slot.
        unsafe {
            self.locals.set_len(0);
            self.local_kinds.set_len(0);
        }
        self.stack.clear();
    }

    /// Copy this frame's slab window (locals and operand stack) into owned
    /// buffers, contents unchanged: the frame is leaving its `FrameStack` by
    /// value (`FrameStack::pop`, the `Vec<Frame>` conversions), and its window
    /// is memory the next push there reuses. No-op for an owned frame.
    pub(crate) fn detach_from_slab(&mut self) {
        if self.locals.is_window() || self.stack.is_windowed() {
            self.locals.make_owned();
            self.local_kinds.make_owned();
            self.stack.detach_from_slab();
        }
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
            locals: SlotVec::from_vec(locals),
            local_kinds: SlotVec::from_vec(local_kinds),
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
            osr_poll_at: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            held_monitors: crate::runtime::interpreter::held_monitors::HeldMonitors::new(),
            seq: next_frame_seq(),
            exec_epoch: 0,
            slab_mark: SLAB_MARK_UNKNOWN,
            redefine_stamp: current_redefine_stamp(),
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
        locals_pool.push((
            compact_vec_to_u64(self.locals.into_vec()),
            self.local_kinds.into_vec(),
        ));
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
            compact_vec_to_u64(self.locals.into_vec()),
            self.local_kinds.into_vec(),
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
    /// `docs/internal/performance/interpreted-invoke-cost-350ns-RETIRED-20260911.md`.
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
        // A windowed frame hands back four EMPTY `Vec`s (its memory is the
        // slab's), which `JvmThread::harvest_retired_slot` already skips.
        let (stack_vals, stack_tags) = self.stack.take_inner_in_place();
        let locals = self.locals.take_vec();
        let local_kinds = self.local_kinds.take_vec();
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

    // ── Redefinition (JEP 109 obsolete methods) ─────────────────────────

    /// `class_redefinition_count()` when this frame's code was taken: the
    /// generation of its class's constant pool that `code` indexes (see
    /// `interpreter::obsolete_frames`).
    #[inline]
    pub fn redefine_stamp(&self) -> u64 {
        self.redefine_stamp & !REDEFINE_STAMP_MARK_BITS
    }

    /// Does this frame run a method body its class's redefinition replaced,
    /// with bytecode that differs from the current body (a JDWP / JVMTI
    /// obsolete method)? Set only by [`Self::adopt_redefined_body`]; a frame
    /// whose replaced body was never translated answers `false` here, and
    /// `obsolete_frames::frame_runs_replaced_code` is the full question.
    #[inline]
    pub fn runs_obsolete_method(&self) -> bool {
        self.redefine_stamp & OBSOLETE_METHOD_STAMP_BIT != 0
    }

    /// Was this activation begun before a redefinition of its class that a
    /// conversion has since moved it across (`obsolete_frames::convert_frames`)?
    /// True for an obsolete frame and for an EMCP one alike -- HotSpot's
    /// "old" method versions, whose `StackWalker` frames report no line
    /// (`tools/probes/interp/L3/L3W39HotSwapWalkerLines.java`). A frame not
    /// yet moved answers `false`: its stale [`Self::redefine_stamp`] says it
    /// (`stackwalker::frame_predates_its_class_redefinition` asks both).
    /// Interpreter round i1 wave 39, lane L3.
    #[inline]
    pub fn predates_its_class_redefinition(&self) -> bool {
        self.redefine_stamp & PREDATES_REDEFINITION_STAMP_BIT != 0
    }

    /// Mark this frame as moved across a redefinition of its class (see
    /// [`Self::predates_its_class_redefinition`]). The mark stays for the
    /// activation's life: later moves keep it.
    #[inline]
    pub(crate) fn mark_predates_its_class_redefinition(&mut self) {
        self.redefine_stamp |= PREDATES_REDEFINITION_STAMP_BIT;
    }

    /// Move this frame onto `code` (padded) and `exception_table`: the body
    /// it runs, with its constant-pool operands translated into the class's
    /// current pool. The instruction layout is the same, so `pc`, the locals
    /// and the operand stack carry over unchanged. `stamp` is the
    /// redefinition the translation reaches; `obsolete` marks a body that
    /// differs from the current one. Returns the code the frame ran until
    /// now, which the caller keeps alive (a suspended dispatch loop may still
    /// hold a raw pointer into it): `obsolete_frames::convert_frames` has
    /// already put it in `replaced`'s [`ReplacedBody::retired_code`].
    ///
    /// The frame becomes an owned-metadata frame: a `CachedBytecodeMethod`
    /// describes the method as its class now has it, and its memoized
    /// quickened stream is of the untranslated bytes. The method slot is
    /// dropped too, since it names the current body's line table.
    /// `replaced` carries what the class's current method cannot answer for
    /// the body (its line table, a widened `ldc` stream); `None` when
    /// nothing.
    pub(crate) fn adopt_redefined_body(
        &mut self,
        code: Arc<[u8]>,
        exception_table: Arc<[ExceptionTableEntry]>,
        stamp: u64,
        obsolete: bool,
        replaced: Option<Arc<ReplacedBody>>,
    ) -> Arc<[u8]> {
        let meta = OwnedFrameMeta {
            class_name: self.class_name_arc(),
            method_name: self.method_name_arc(),
            method_descriptor: self.method_descriptor_arc(),
            source_file: self.source_file_arc(),
            exception_table,
            replaced_body: replaced.filter(|r| !r.is_empty()),
        };
        self.inner = FrameInner::Owned(Box::new(meta));
        self.method_index = None;
        self.redefine_stamp = (stamp & !REDEFINE_STAMP_MARK_BITS)
            | (self.redefine_stamp & PREDATES_REDEFINITION_STAMP_BIT)
            | if obsolete {
                OBSOLETE_METHOD_STAMP_BIT
            } else {
                0
            };
        std::mem::replace(&mut self.code, code)
    }

    /// [`Self::adopt_redefined_body`] for a body whose translation left its
    /// bytes and exception table as they are (every constant it names kept
    /// its index): the frame keeps its code, and its cached method and slot
    /// unless `replaced` carries something the class's method cannot answer
    /// (then it becomes an owned-metadata frame holding it). Only the stamp
    /// and the obsolete mark move. Interpreter round i1 wave 21, lane L4.
    pub(crate) fn restamp_redefined_body(
        &mut self,
        stamp: u64,
        obsolete: bool,
        replaced: Option<Arc<ReplacedBody>>,
    ) {
        if let Some(replaced) = replaced.filter(|r| !r.is_empty()) {
            let meta = OwnedFrameMeta {
                class_name: self.class_name_arc(),
                method_name: self.method_name_arc(),
                method_descriptor: self.method_descriptor_arc(),
                source_file: self.source_file_arc(),
                exception_table: self.exception_table_arc(),
                replaced_body: Some(replaced),
            };
            self.inner = FrameInner::Owned(Box::new(meta));
            // The slot names the class's method, whose lines are not this
            // body's.
            self.method_index = None;
        }
        self.redefine_stamp = (stamp & !REDEFINE_STAMP_MARK_BITS)
            | (self.redefine_stamp & PREDATES_REDEFINITION_STAMP_BIT)
            | if obsolete {
                OBSOLETE_METHOD_STAMP_BIT
            } else {
                0
            };
    }

    /// What this frame carries for the replaced body it runs, if it was
    /// moved onto one and the class's method cannot answer for it (see
    /// [`ReplacedBody`]).
    #[inline]
    pub(crate) fn replaced_body(&self) -> Option<&Arc<ReplacedBody>> {
        match &self.inner {
            FrameInner::Owned(o) => o.replaced_body.as_ref(),
            FrameInner::Cached(_) => None,
        }
    }

    /// The decoded stream this frame must run on because some `ldc` of its
    /// replaced body names a constant above index 255
    /// ([`ReplacedBody::widened_stream`]).
    #[inline]
    pub(crate) fn widened_stream(&self) -> Option<&Arc<cratonvm_reader::QuickenedCode>> {
        self.replaced_body().and_then(|r| r.widened_stream.as_ref())
    }

    /// The line of `bci` in the replaced body this frame runs, when that
    /// body's line table differs from its class's method's (a JEP 109
    /// obsolete method reports its own lines). `None` otherwise: the class
    /// resolves the line, eagerly or lazily, as for any frame.
    #[inline]
    pub(crate) fn own_line_number(&self, bci: i32) -> Option<i32> {
        let lines = self.replaced_body()?.line_numbers.as_ref()?;
        let bci = usize::try_from(bci).ok()?;
        cratonvm_classloading::obsolete_code::line_number_at(lines, bci).map(i32::from)
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
            // `CRATONVM_DBG_DEADREF_STORE`: the arm the ledger below cannot
            // reach.
            //
            // The ledger is exact only while the address is still un-reissued,
            // and this workload's stale references are installed into frames
            // AFTER the allocator has handed the address out again — so the
            // ledger has forgotten it and the guard below prints nothing. The
            // geometry predicate has no such window: a young address in the
            // emptied semispace, or one in the active semispace whose header
            // words are zero, names no live object whatever the ledger
            // remembers.
            //
            // This is the store that matters, because a frame local is where a
            // stale reference stops being a VM-internal value and becomes one
            // bytecode can dispatch on.
            {
                static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                if *ON.get_or_init(|| cratonvm_types::flags().gc.dbg_deadref_store) {
                    if let Value::Object(Some(o)) = value {
                        if let Some(reason) =
                            cratonvm_gc::gen_heap::dead_young_ref_reason_global(o.as_ptr() as usize)
                        {
                            static M: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            if M.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                                eprintln!(
                                    "[deadref-local] {reason} storing 0x{:x} into {}.{} \
                                     local[{i}] pc={} — the value names no live object, so this \
                                     is where a dead reference becomes a Java-visible one. \
                                     caller:
{:?}",
                                    o.as_ptr() as usize,
                                    self.class_name(),
                                    self.method_name(),
                                    self.pc,
                                    std::backtrace::Backtrace::force_capture(),
                                );
                            }
                        }
                    }
                }
            }
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
                                "a STALE reference is being stored into a frame local — the \
                                 collector moved this object and nothing has been allocated at \
                                 the old address since. The backtrace names the VM code that \
                                 still held it.",
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
            // The pool generation the code indexes travels with it: a
            // continuation thawed after a redefinition is still recognised
            // as running the replaced body.
            redefine_stamp: self.redefine_stamp,
            // So does what the class's method cannot answer for that body
            // (its line table, a widened `ldc` stream).
            replaced_body: self.replaced_body().cloned(),
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
            locals: SlotVec::from_vec(locals),
            local_kinds: SlotVec::from_vec(local_kinds),
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
                    replaced_body: frozen.replaced_body,
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
            osr_poll_at: 0,
            osr_attempt_counts: Vec::new(),
            monitor_on_exit: None,
            held_monitors: crate::runtime::interpreter::held_monitors::HeldMonitors::new(),
            seq: next_frame_seq(),
            exec_epoch: 0,
            slab_mark: SLAB_MARK_UNKNOWN,
            redefine_stamp: frozen.redefine_stamp,
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
    ///
    /// **Minted handles (i1 wave 5):** the one `long` local that IS rooted is
    /// one whose exact bits are registered in `memory::smuggled_longs` (a
    /// jobject a mint chokepoint handed Java as a `long`); `update_local_refs`
    /// remaps the same set, so the pair stays symmetric.
    pub fn scan_local_objects(&self, roots: &mut Vec<ObjectRef>, heap: &crate::memory::VmHeap) {
        self.scan_local_objects_inner(roots, heap, true, None);
    }

    /// [`Self::scan_local_objects`] for a caller that holds the VM: `maps` is
    /// that VM's verifier type-map store (`shared.classes.type_maps`). The
    /// root set is identical; the store only feeds the
    /// `CRATONVM_DBG_VERIFY_OOP_MAPS` shadow comparison (stage 1 of
    /// `docs/known-issues/interpreter/i1-L4-proposal-precise-interpreter-oop-maps-20260923.md`),
    /// and is where stage 3 will read the precise map from. Its remap twin is
    /// [`Self::update_frame_refs_mapped`].
    pub fn scan_local_objects_mapped(
        &self,
        roots: &mut Vec<ObjectRef>,
        heap: &crate::memory::VmHeap,
        maps: &cratonvm_classloading::TypeMapStore,
    ) {
        self.scan_local_objects_inner(roots, heap, true, Some(maps));
    }

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

    /// The per-bci live-local set [`Self::scan_local_objects`] filters this
    /// frame's roots with, at its current pc: `mask.is_live(i)` = slot `i` may
    /// still be read.
    ///
    /// Exposed so a diagnostic can ask the collector's own question instead of
    /// a weaker one. `audit_frames_for_reclaimed_slots` needs exactly this:
    /// a frame local that is DEAD and points into a reclaimed span is the
    /// liveness filter working as designed (the `PreparedStatement` a seed loop
    /// finished with, still in slot 8 for the rest of the method), and
    /// reporting it would bury the case that matters — a LIVE local whose
    /// object the collector took anyway.
    pub(crate) fn live_locals_mask_here(&self) -> crate::runtime::local_liveness::LiveMask {
        if crate::runtime::env_cache::no_local_liveness() {
            crate::runtime::local_liveness::LiveMask::all_live()
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
        self.scan_local_objects_inner(roots, heap, false, None);
    }

    fn scan_local_objects_inner(
        &self,
        roots: &mut Vec<ObjectRef>,
        heap: &crate::memory::VmHeap,
        honor_liveness: bool,
        maps: Option<&cratonvm_classloading::TypeMapStore>,
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
            crate::runtime::local_liveness::LiveMask::all_live()
        } else {
            crate::runtime::local_liveness::live_locals_mask(
                &self.code,
                self.exception_table(),
                self.max_locals,
                [self.pc, self.last_instr_pc],
            )
        };
        self.scan_locals_in(0..self.locals.len(), roots, heap, &live_mask);
        // Stage 1 of the precise-oop-map proposal: compare this root set with
        // the VM's verifier type map at the same pcs. Opt-in, one cached load
        // off; a caller without the VM (`maps` = `None`) is not compared.
        if let Some(maps) = maps {
            if honor_liveness && crate::runtime::local_liveness::oop_map_shadow::enabled() {
                self.shadow_compare_oop_maps(heap, &live_mask, maps);
            }
        }
    }

    /// The body of [`Self::scan_local_objects_inner`] over the local slots in
    /// `range` (always within `0..self.locals.len()`), filtered by `live_mask`.
    #[inline(always)]
    fn scan_locals_in(
        &self,
        range: std::ops::Range<usize>,
        roots: &mut Vec<ObjectRef>,
        heap: &crate::memory::VmHeap,
        live_mask: &crate::runtime::local_liveness::LiveMask,
    ) {
        for i in range {
            let cv = &self.locals[i];
            if !live_mask.is_live(i) {
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
                                heap.collection_count(),
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
            //
            // The one exception is a `long` whose exact bits are a live MINTED
            // smuggled handle (`memory::smuggled_longs`): the operand stack
            // roots and remaps those, so without this a handle was rooted
            // while on the stack and dropped the moment it was stored to a
            // local (i1 wave 5, lane L4).
            if self.local_kinds[i] == LKIND_LONG || self.local_kinds[i] == LKIND_DOUBLE {
                if self.local_kinds[i] == LKIND_LONG {
                    if let Some(obj) = minted_long_local_root(heap, cv.raw_bits()) {
                        roots.push(obj);
                    }
                }
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

    /// Shadow comparison of this frame's heuristic roots against the
    /// verifier's type map (`CRATONVM_DBG_VERIFY_OOP_MAPS`; see
    /// `local_liveness::oop_map_shadow`). Reports, never changes, the root set.
    ///
    /// The map is read at both pcs the liveness filter unions (`pc`,
    /// `last_instr_pc`): a slot is a precise reference if either mapped pc
    /// says so, and a precise non-reference only if every mapped pc does. The
    /// operand stack is compared only at a pc whose recorded depth equals the
    /// runtime depth, because a frame stopped inside an instruction (an invoke
    /// that popped its arguments) is between two map rows.
    #[cold]
    #[inline(never)]
    fn shadow_compare_oop_maps(
        &self,
        heap: &crate::memory::VmHeap,
        live_mask: &crate::runtime::local_liveness::LiveMask,
        store: &cratonvm_classloading::TypeMapStore,
    ) {
        use crate::runtime::local_liveness::oop_map_shadow as shadow;
        // The VM's own store (`shared.classes.type_maps`), handed down by the
        // root walk: keyed by this VM's `ClassId`s, so no other VM's maps.
        let Some((local_rows, stack_rows)) = self.shadow_map_rows(store) else {
            shadow::note_unmapped_frame();
            return;
        };
        shadow::note_mapped_frame();
        let describe = || self.shadow_describe();
        for i in 0..self.locals.len() {
            let map_ref = shadow::MapAnswer::from_rows(local_rows.iter().map(|r| r.get(i)));
            let live = live_mask.is_live(i);
            // A dead slot is never rooted, so asking the scan again is moot.
            let rooted = live && {
                let mut one = Vec::new();
                self.scan_locals_in(i..i + 1, &mut one, heap, live_mask);
                !one.is_empty()
            };
            let cv = self.locals[i];
            let holds_value = !cv.is_null() && cv.raw_bits() != 0;
            let verdict = shadow::classify(rooted, holds_value, map_ref, live);
            shadow::note(shadow::Area::Local, verdict, i, cv.raw_bits(), &describe);
        }
        if stack_rows.is_empty() {
            shadow::note_stack_depth_mismatch();
            return;
        }
        for i in 0..self.stack.len() {
            let map_ref = shadow::MapAnswer::from_rows(stack_rows.iter().map(|r| r.get(i)));
            let rooted = self.stack.slot_is_scanned_root(i, heap);
            let (bits, holds_value) = match self.stack.get_compact(i) {
                Some(cv) => (cv.raw_bits(), !cv.is_null() && cv.raw_bits() != 0),
                None => (0, false),
            };
            let verdict = shadow::classify(rooted, holds_value, map_ref, true);
            shadow::note(shadow::Area::Stack, verdict, i, bits, &describe);
        }
    }

    /// The verifier rows the shadow comparisons read for this frame, from
    /// `store`: the locals rows at the two pcs the liveness filter unions
    /// (`pc`, `last_instr_pc`), and the operand-stack rows of those whose
    /// recorded depth equals the runtime depth (a frame stopped inside an
    /// invoke has popped its arguments and sits between two rows). `None`
    /// when the method has no maps or neither pc has a row.
    #[allow(clippy::type_complexity)]
    fn shadow_map_rows<'m>(
        &self,
        store: &'m cratonvm_classloading::TypeMapStore,
    ) -> Option<(
        Vec<cratonvm_classloading::LocalOopBits<'m>>,
        Vec<cratonvm_classloading::StackOopBits<'m>>,
    )> {
        // A frame moved onto a body its class's redefinition replaced (JEP
        // 109): the maps filed under its method's name describe the class's
        // CURRENT body, not its code, so a comparison would only report
        // differences that are not there (interpreter round i1 wave 22, lane
        // L3). An EMCP body's code is the current one's, so its maps hold.
        if self.runs_obsolete_method() || self.widened_stream().is_some() {
            return None;
        }
        let maps = store.type_maps_for_named(
            self.class_id,
            self.method_name(),
            self.method_descriptor(),
        )?;
        let pcs = [self.pc as u32, self.last_instr_pc as u32];
        let local_rows: Vec<_> = pcs
            .iter()
            .filter_map(|&pc| maps.local_oops_at(pc))
            .collect();
        if local_rows.is_empty() {
            return None;
        }
        let depth = self.stack.len();
        let stack_rows: Vec<_> = pcs
            .iter()
            .filter(|&&pc| maps.stack_depth_at(pc).map(usize::from) == Some(depth))
            .filter_map(|&pc| maps.stack_oops_at(pc))
            .collect();
        Some((local_rows, stack_rows))
    }

    /// `Cls.m(desc) pc=.. last_pc=..`, for a shadow difference line.
    fn shadow_describe(&self) -> String {
        format!(
            "{}.{}{} pc={} last_pc={}",
            self.class_name(),
            self.method_name(),
            self.method_descriptor(),
            self.pc,
            self.last_instr_pc
        )
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
        // Read ONCE per process, not once per local per frame per collection:
        // an undeclared name falls through `runtime_var_os` to
        // `std::env::var_os`, which takes the process environment lock and
        // allocates, and this loop runs for every local of every frame of
        // every thread on each moving collection.
        let bug03 = bug03_dbg_enabled();
        // Indexed loop so the per-slot `local_kinds` mark can be consulted
        // without aliasing the `&mut self.locals` borrow.
        for i in 0..self.locals.len() {
            // BUG-03 diag (gated): for any slot whose address is in this GC's
            // pointer_map, log the kind/tag/decision — captures the exact slot
            // (Thread.<init> local[7] = `parent`) that the remap skips.
            if bug03 {
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
                // A MINTED handle stored to a `long` local is rooted by
                // `scan_local_objects_inner`, so it is remapped here: exactly
                // the rule `ValueStack::update_object_refs` applies, minus the
                // `CRATONVM_LONGREWRITE_LOOSE` hatch (locals never had a loose
                // rewrite, and a primitive collision must stay verbatim).
                if self.local_kinds[i] == LKIND_LONG {
                    let raw = self.locals[i].raw_bits();
                    if raw != 0 && raw & 0x7 == 0 {
                        if let Some(&new_addr) = pointer_map.get(&(raw as usize)) {
                            if crate::memory::smuggled_longs::is_minted(heap, raw)
                                || crate::memory::smuggled_longs::is_minted(heap, new_addr as u64)
                            {
                                self.locals[i] = CompactValue::long(new_addr as i64);
                            }
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

    /// Remap twin of [`Self::scan_local_objects_mapped`]: the
    /// [`Self::update_local_refs`] + `stack.update_object_refs` pair a
    /// per-thread remap runs, for a caller that holds the VM. The rewrite is
    /// identical to calling the two directly; `maps` (the VM's
    /// `shared.classes.type_maps`) only feeds the `CRATONVM_DBG_VERIFY_OOP_MAPS`
    /// shadow comparison of what the remap rewrote against the verifier's map.
    pub fn update_frame_refs_mapped(
        &mut self,
        pointer_map: &cratonvm_types::PointerMap,
        heap: &crate::memory::VmHeap,
        maps: &cratonvm_classloading::TypeMapStore,
    ) {
        let before = if crate::runtime::local_liveness::oop_map_shadow::enabled() {
            Some(self.shadow_slot_snapshot())
        } else {
            None
        };
        self.update_local_refs(pointer_map, heap);
        self.stack.update_object_refs(pointer_map, heap);
        if let Some((locals, stack)) = before {
            self.shadow_compare_remap(pointer_map, maps, &locals, &stack);
        }
    }

    /// The locals and operand-stack words before a remap, for
    /// [`Self::shadow_compare_remap`].
    #[cold]
    #[inline(never)]
    fn shadow_slot_snapshot(&self) -> (Vec<CompactValue>, Vec<CompactValue>) {
        let stack = (0..self.stack.len())
            .filter_map(|i| self.stack.get_compact(i))
            .collect();
        (self.locals.to_vec(), stack)
    }

    /// Shadow comparison of one remap against the verifier's map: a slot the
    /// map calls a live reference whose old value named a moved object
    /// (a `pointer_map` key) but that the remap left unchanged is `Missed` (a
    /// stale pointer the precise map would have fixed); a live slot the remap
    /// rewrote that every mapped pc calls a non-reference is `Extra` (a
    /// primitive the remap may have corrupted). Dead locals are never either:
    /// rewriting a dead slot is harmless and leaving one stale is too.
    #[cold]
    #[inline(never)]
    fn shadow_compare_remap(
        &self,
        pointer_map: &cratonvm_types::PointerMap,
        store: &cratonvm_classloading::TypeMapStore,
        locals_before: &[CompactValue],
        stack_before: &[CompactValue],
    ) {
        use crate::runtime::local_liveness::oop_map_shadow as shadow;
        let Some((local_rows, stack_rows)) = self.shadow_map_rows(store) else {
            return;
        };
        shadow::note_remap_frame();
        let moved = |cv: CompactValue| {
            lost_tag_local_candidates(cv)
                .iter()
                .any(|&c| c != 0 && pointer_map.contains_key(&(c as usize)))
        };
        let describe = || self.shadow_describe();
        let live_mask = self.live_locals_mask_here();
        for (i, &old) in locals_before.iter().enumerate().take(self.locals.len()) {
            let live = live_mask.is_live(i);
            let rewritten = live && self.locals[i].raw_bits() != old.raw_bits();
            let map_ref = shadow::MapAnswer::from_rows(local_rows.iter().map(|r| r.get(i)));
            let verdict = shadow::classify(rewritten, moved(old), map_ref, live);
            shadow::note(
                shadow::Area::RemapLocal,
                verdict,
                i,
                old.raw_bits(),
                &describe,
            );
        }
        if stack_rows.is_empty() || stack_before.len() != self.stack.len() {
            return;
        }
        for (i, &old) in stack_before.iter().enumerate() {
            let now = self.stack.get_compact(i).map_or(0, |cv| cv.raw_bits());
            let rewritten = now != old.raw_bits();
            let map_ref = shadow::MapAnswer::from_rows(stack_rows.iter().map(|r| r.get(i)));
            let verdict = shadow::classify(rewritten, moved(old), map_ref, true);
            shadow::note(
                shadow::Area::RemapStack,
                verdict,
                i,
                old.raw_bits(),
                &describe,
            );
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
    /// Incremented every time a frame on this stack is moved onto another
    /// code allocation by a class redefinition
    /// (`interpreter::obsolete_frames`). The dispatch loop's fast-path gate
    /// is keyed on the code allocation's ADDRESS, which a moved frame's new
    /// copy can share with a callee's private copy freed at its pop; the
    /// loop refreshes the gate when this changes (interpreter round i1 wave
    /// 23, lane L3). Wrapping is harmless for the same reason as
    /// `reloc_epoch`'s.
    ///
    /// Since wave 26 (lane L7) the loop does not load it per bytecode: a move
    /// also bumps the running OS thread's poll word
    /// (`threading::gc_barrier::note_code_moved_on_this_thread`), whose one
    /// load per bytecode sends the loop to its slow path, and the slow path
    /// compares this count.
    code_moves: u32,
    /// A class redefinition left frames of this stack to move, and the
    /// conversion that owed it could not take the class-manager lock
    /// (`obsolete_frames::convert_obsolete_frames_if_redefined`): the
    /// dispatch loop retries it at its next top, before it runs a bytecode
    /// (interpreter round i1 wave 24, lane L3). The move count was bumped with
    /// it, which is what sends the loop to the retry.
    conversion_pending: bool,
    /// Retries of the pending conversion since it was deferred; bounded by
    /// [`Self::MAX_CONVERSION_RETRIES`].
    conversion_retries: u32,
    /// When the pending conversion's first retry ran (`None`: no retry
    /// since it was deferred): bounds the retries' wall time by
    /// [`Self::MAX_CONVERSION_RETRY_WALL`] too (interpreter round i1 wave
    /// 39, lane L3).
    conversion_retry_began: Option<std::time::Instant>,
    /// `class_redefinition_count()` at which the loop last gave a deferred
    /// conversion up ([`Self::NEVER_GAVE_UP`]: never): a later deferral at
    /// the same count is not retried again, so a thread that runs Java while
    /// it holds the class-manager lock itself spends the retries once per
    /// redefinition, not at every safepoint.
    ///
    /// Not `0` for "never" (interpreter round i1 wave 41, lane L3): `0` is
    /// the count a redefinition fence goes up at for the process's FIRST
    /// redefinition (the count advances under the class-manager writer,
    /// after the fence), so a loop top that met that fence took its
    /// deferral for one it had given up and ran on. Read from the code; the
    /// wave-41 host runs found no loop top meeting the first fence at all
    /// (`docs/internal/fixed-bugs/interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007.md`,
    /// "Progress (wave 41)"), so this is a latent hole, not that page's cause.
    conversion_given_up_at: u64,
    /// The slot memory of this stack's windowed frames (`runtime::slot_slab`,
    /// stage 1 of the contiguous interpreter stack, wave 29 lane L7): the
    /// cached install paths lay each frame's locals and operand stack in one
    /// window of it instead of four pooled buffers.
    ///
    /// # The slab's half of the address-stability contract
    ///
    /// * A window never moves: the slab grows by appending chunks, so no
    ///   `reloc_epoch` exists for it and none is needed. A frame pointer that
    ///   is valid under the contract above reads valid locals.
    /// * Every push records the slab's mark in the frame (`Frame::slab_mark`),
    ///   and every drop in depth (`pop`, `truncate`, `retire_top`, `clear`)
    ///   releases the slab to the mark of the lowest frame that leaves, so the
    ///   next push at that depth reuses exactly the memory the popped frames
    ///   used -- as a retired slot's buffers are reused today.
    /// * A frame leaves by value only detached (`pop`, the `Vec<Frame>`
    ///   conversions copy its window into owned buffers), so no window
    ///   outlives this stack.
    slab: SlotSlab,
    /// The frame limit this thread's requested stack size grants
    /// (`Thread(group, task, name, stackSize)`, see
    /// [`frame_limit_for_stack_bytes`]); `0` for none. It only ever RAISES
    /// the VM's limit (`VmConfig::max_stack_depth`), and it is read only once
    /// that limit is reached, so the per-call check costs what it did
    /// (interpreter round i1 wave 38, lane L7): see [`Self::at_frame_limit`].
    requested_frame_limit: usize,
}

/// Interpreter frames the VM's default frame limit
/// (`VmConfig::max_stack_depth`, 8192) stands for per MiB of requested thread
/// stack: HotSpot's default `-Xss` is 1 MiB. A thread asking for N MiB (its
/// `stackSize`, or `-Xss`) gets `N * 8192` frames, as HotSpot's own depth
/// grows with the stack it is given (interpreter round i1 wave 38, lane L7).
pub const FRAMES_PER_STACK_MIB: usize = 8192;

/// The most frames a requested stack size can grant: 2 Mi frames, what 256 MiB
/// buys at [`FRAMES_PER_STACK_MIB`]. HotSpot caps `-Xss` at 1 GiB, but a frame
/// here costs its ~240-byte `Frame` plus its slab window (about twice what a
/// HotSpot interpreted frame of a small method costs), so the bound keeps a
/// runaway recursion in a huge-stack thread to about 1 GiB before it throws.
pub const MAX_REQUESTED_FRAME_LIMIT: usize = 1 << 21;

/// The interpreter frame limit a thread stack of `bytes` bytes grants, at
/// [`FRAMES_PER_STACK_MIB`] frames per MiB, capped at
/// [`MAX_REQUESTED_FRAME_LIMIT`]. `0` (or less) grants nothing: the thread
/// keeps the VM's limit.
pub fn frame_limit_for_stack_bytes(bytes: i64) -> usize {
    if bytes <= 0 {
        return 0;
    }
    // Widening: a positive i64 fits u128; the product cannot overflow u128.
    let frames = (bytes as u128) * (FRAMES_PER_STACK_MIB as u128) / (1u128 << 20);
    // Cast: bounded by MAX_REQUESTED_FRAME_LIMIT (2 Mi), fits usize.
    frames.min(MAX_REQUESTED_FRAME_LIMIT as u128) as usize
}

impl FrameStack {
    /// How many times the dispatch loop retries a deferred conversion
    /// before it lets the frames run as they are until the thread's next
    /// safepoint or blocking-region exit: a writer holding the class-manager
    /// lock for a class definition or a redefinition releases it within far
    /// fewer; a thread running Java while it holds that lock itself never
    /// would. The first [`Self::CONVERSION_RETRY_YIELDS`] tries yield, the
    /// rest sleep 20 us each (`obsolete_frames::retry_deferred_conversion`),
    /// so the bound is about a second of wall time, not ~65,000 yields --
    /// which a redefinition verifying a large class can outlast (interpreter
    /// round i1 wave 25, lane L3). The sleeps' granularity made it longer;
    /// since wave 39 [`Self::MAX_CONVERSION_RETRY_WALL`] bounds it too.
    pub(crate) const MAX_CONVERSION_RETRIES: u32 = 1 << 16;

    /// The wall time the retries of one deferred conversion may take, past
    /// the yields, whatever [`Self::MAX_CONVERSION_RETRIES`] says. The count
    /// alone sized the bound by the sleeps' granularity: a 20 us
    /// `std::thread::sleep` sleeps its timer slack on Linux (several seconds
    /// over the bound) and at least the system timer's resolution on Windows
    /// (1 to 15.6 ms: a minute or more). A thread held by a redefinition
    /// fence while it holds the class-manager lock itself waits the whole
    /// bound, and so does the redefinition behind it
    /// (`docs/internal/fixed-bugs/interpreter-L3-a-fenced-loop-waits-out-the-retry-bound-when-its-thread-holds-the-class-manager-lock-FIXED-20261010.md`).
    /// One second: the "about a second" wave 25 sized the count for, which a
    /// writer verifying a large class stays within. Interpreter round i1
    /// wave 39, lane L3.
    pub(crate) const MAX_CONVERSION_RETRY_WALL: std::time::Duration =
        std::time::Duration::from_secs(1);

    /// See [`Self::MAX_CONVERSION_RETRIES`].
    pub(crate) const CONVERSION_RETRY_YIELDS: u32 = 1 << 12;

    /// Retries of the pending conversion so far (see
    /// [`Self::count_conversion_retry`]).
    #[inline]
    pub(crate) fn conversion_retries(&self) -> u32 {
        self.conversion_retries
    }

    /// An empty stack that has not allocated. Matches the old
    /// `frames: Vec::new()` cost for threads that never run bytecode.
    pub const fn new() -> Self {
        Self {
            buf: Vec::new(),
            depth: 0,
            reloc_epoch: 0,
            code_moves: 0,
            conversion_pending: false,
            conversion_retries: 0,
            conversion_retry_began: None,
            conversion_given_up_at: Self::NEVER_GAVE_UP,
            slab: SlotSlab::new(),
            requested_frame_limit: 0,
        }
    }

    /// `true` when `pending` more frames on top of this stack would reach this
    /// thread's frame limit: the VM's `vm_limit` (`VmConfig::max_stack_depth`)
    /// or, when larger, the limit its requested stack size granted
    /// ([`Self::set_requested_frame_limit`]). Every door and push site asks
    /// this instead of comparing `len()` with `vm_limit` itself.
    ///
    /// The second comparison runs only once the first holds, i.e. only at a
    /// depth the VM's own limit already refuses, so on every ordinary call this
    /// is the one predictable compare it replaced.
    #[inline(always)]
    pub fn at_frame_limit(&self, pending: usize, vm_limit: usize) -> bool {
        let depth = self.depth + pending;
        depth >= vm_limit && depth >= self.requested_frame_limit
    }

    /// Record the frame limit this thread's requested stack size grants
    /// ([`frame_limit_for_stack_bytes`]); `0` clears it. Set once, when the
    /// thread starts, before it runs bytecode.
    pub fn set_requested_frame_limit(&mut self, limit: usize) {
        self.requested_frame_limit = limit;
    }

    /// See [`Self::set_requested_frame_limit`].
    #[inline]
    pub fn requested_frame_limit(&self) -> usize {
        self.requested_frame_limit
    }

    /// See the `conversion_pending` field.
    #[inline]
    pub(crate) fn conversion_pending(&self) -> bool {
        self.conversion_pending
    }

    /// [`Self::conversion_given_up_at`] of a stack that never gave a
    /// conversion up: no count reaches it.
    pub(crate) const NEVER_GAVE_UP: u64 = u64::MAX;

    /// The conversion this stack owed at redefinition count `count` could
    /// not take the class-manager lock: mark it pending and bump
    /// the move count, so the dispatch loop's next top retries it -- unless the
    /// loop already gave a conversion at this count up. Whether it was marked.
    pub(crate) fn note_conversion_deferred(&mut self, count: u64) -> bool {
        if count == self.conversion_given_up_at {
            return false;
        }
        self.conversion_pending = true;
        self.note_code_moved();
        true
    }

    /// The owed conversion ran: nothing pending.
    #[inline]
    pub(crate) fn clear_conversion_pending(&mut self) {
        self.conversion_pending = false;
        self.conversion_retries = 0;
        self.conversion_retry_began = None;
    }

    /// The loop spent [`Self::MAX_CONVERSION_RETRIES`] on the conversion owed
    /// at `count`: nothing pending, and no retry at this count again.
    pub(crate) fn give_up_conversion(&mut self, count: u64) {
        self.conversion_given_up_at = count;
        self.clear_conversion_pending();
    }

    /// Count one retry of the pending conversion; `false` once
    /// [`Self::MAX_CONVERSION_RETRIES`] were spent, or once the retries past
    /// the first [`Self::CONVERSION_RETRY_YIELDS`] ran longer than
    /// [`Self::MAX_CONVERSION_RETRY_WALL`] since the first one (one clock
    /// read per retry that sleeps anyway).
    pub(crate) fn count_conversion_retry(&mut self) -> bool {
        self.conversion_retries = self.conversion_retries.saturating_add(1);
        if self.conversion_retries > Self::MAX_CONVERSION_RETRIES {
            return false;
        }
        let began = *self
            .conversion_retry_began
            .get_or_insert_with(std::time::Instant::now);
        self.conversion_retries <= Self::CONVERSION_RETRY_YIELDS
            || began.elapsed() <= Self::MAX_CONVERSION_RETRY_WALL
    }

    /// How long the pending conversion has been retried (zero before its
    /// first retry). For the give-up report.
    pub(crate) fn conversion_retry_elapsed(&self) -> std::time::Duration {
        self.conversion_retry_began
            .map_or(std::time::Duration::ZERO, |began| began.elapsed())
    }

    /// See the `code_moves` field: how many times a frame on this stack was
    /// moved onto another code allocation by a class redefinition. Read by
    /// the dispatch loop's slow path.
    #[inline(always)]
    pub(crate) fn code_moves(&self) -> u32 {
        self.code_moves
    }

    /// A frame on this stack now runs another code allocation
    /// (`obsolete_frames::convert_frames`): bump the count, and the poll
    /// word of the OS thread this runs on -- which is the thread running
    /// this stack's dispatch loops, if any runs it (a move takes `&mut` of
    /// the stack, which a running loop lends only to its own callees) -- so
    /// every such loop takes its slow path at its next top (interpreter round
    /// i1 wave 26, lane L7).
    #[inline]
    pub(crate) fn note_code_moved(&mut self) {
        self.code_moves = self.code_moves.wrapping_add(1);
        crate::threading::gc_barrier::note_code_moved_on_this_thread();
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

    /// The frame `p` addresses, as a `&mut` borrowed from `self`: a hoisted
    /// [`Self::frame_ptr`] turned back into a borrow-checked reference without
    /// `IndexMut`'s two bounds checks (the live-prefix slice and the index)
    /// and its multiply by `size_of::<Frame>()`.
    ///
    /// The result borrows the whole stack mutably, exactly as
    /// `&mut stack[idx]` does, so every caller keeps the compile-time
    /// discipline an indexed borrow gives it (the dispatch loop's `let _ =
    /// frame;` before it calls back into its thread). Interpreter round i1
    /// wave 37, lane L7 (`i25-L7-the-interpreter-dispatch-loop-is-slower-than-on-wave-23-20260927.md`).
    ///
    /// # Safety
    ///
    /// `p` is `self.frame_ptr(idx)` for an `idx` that is still live, taken
    /// after the last `&mut` reborrow of `self` and with
    /// [`Self::reloc_epoch`] unchanged since (the type-level rules 2-4), and
    /// no other reference to that frame is live.
    #[inline(always)]
    pub unsafe fn frame_mut_from_ptr(&mut self, p: *mut Frame) -> &mut Frame {
        debug_assert!(
            {
                let base = self.buf.as_ptr() as usize;
                let at = p as usize;
                at >= base
                    && at < base + self.depth * std::mem::size_of::<Frame>()
                    && (at - base) % std::mem::size_of::<Frame>() == 0
            },
            "a hoisted frame pointer outside the live frames"
        );
        &mut *p
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
    pub fn push(&mut self, mut frame: Frame) {
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
        // A by-value frame owns its buffers (constructors never take a
        // window); recording the mark keeps the pop that releases it exact.
        frame.slab_mark = self.slab.mark();
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
        let mut frame = self.buf.pop()?;
        self.slab.release_to(frame.slab_mark);
        // Its window is memory the next push reuses: it leaves with owned
        // copies (a rare path -- the return path retires in place).
        frame.detach_from_slab();
        Some(frame)
    }

    /// Drop everything above depth `len`. Never moves any surviving frame.
    #[inline(always)]
    pub fn truncate(&mut self, len: usize) {
        if len < self.depth {
            // `len < depth <= buf.len()`: the lowest frame that leaves.
            self.slab.release_to(self.buf[len].slab_mark);
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
            self.slab.release_to(self.buf[len].slab_mark);
            self.depth = len;
        }
        self.buf.truncate(self.depth);
    }

    /// Drop every frame. Retains the (stable) capacity, and the slab's chunks.
    #[inline(always)]
    pub fn clear(&mut self) {
        self.depth = 0;
        self.buf.clear();
        self.slab.release_all();
    }

    /// Insert at `idx`, shifting the frames above it.
    ///
    /// NOTE: this is the one operation that moves frames *without* a
    /// relocation-epoch bump, because the buffer itself does not move — the
    /// contents shift. It exists only for `Vec` parity; the Java call stack is
    /// strictly LIFO and nothing in the VM currently uses it. Any future
    /// caller must invalidate its own frame pointers.
    #[inline]
    pub fn insert(&mut self, idx: usize, mut frame: Frame) {
        self.reserve_stable(1);
        // The frame now at `idx` leaves when a truncate to `idx` does, so it
        // takes over the mark of the frame it displaces.
        frame.slab_mark = match self.buf.get(idx) {
            Some(displaced) if idx < self.depth => displaced.slab_mark,
            _ => self.slab.mark(),
        };
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
            // The live depth; `buf.len()` is the high-water mark (retired
            // slots included), which this line printed as the depth until
            // interpreter round i1 wave 38.
            .field("depth", &self.depth)
            .field("retained", &self.buf.len())
            .field("stable_capacity", &self.buf.capacity())
            .field("reloc_epoch", &self.reloc_epoch)
            .field("slab_mark", &self.slab.mark())
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
        // One load and a min: the retired frame's window (if it has one) is
        // free for the next push at this depth, which is where a retired
        // slot's buffers were always reused.
        let d = self.depth;
        self.slab.release_to(self.buf[d].slab_mark);
        true
    }

    /// Whether a retired slot is waiting at the current depth.
    #[inline(always)]
    pub fn has_retired_slot(&self) -> bool {
        self.depth < self.buf.len()
    }

    /// The retired slot at the current depth, for harvesting its buffers
    /// before an ordinary by-value push overwrites it.
    ///
    /// Harvest only (`Frame::take_pool_parts_in_place`): never read the
    /// slot's locals or operand stack, nor format it. A retired frame's slab
    /// window is released memory, and the slab may have freed the spare chunk
    /// it pointed into (`SlotSlab::alloc`'s slow path), so a read would be a
    /// use after free (interpreter round i1 wave 29).
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
        let t0 = crate::runtime::interpreter::invoke_phases::now();
        let d = self.depth;
        if self.retired_slot_takes_a_window(d) {
            let (n, eff_max_locals) = compact_locals_len(&cached, args);
            let padded = padded_operand_stack(&cached);
            let w = self.slab.alloc(n + padded);
            // SAFETY: `w` is a fresh window of `n + padded` slots for the frame
            // at `d`, and `n` covers every argument slot.
            unsafe {
                lay_compact_args(w.vals.as_ptr(), w.kinds.as_ptr(), n, args);
                self.buf[d].reset_cached_in_window(cached, w, n, padded, eff_max_locals);
            }
        } else {
            self.reset_pooled_compact_at(d, cached, args);
        }
        self.depth += 1;
        crate::runtime::interpreter::invoke_phases::charge_slot_build(t0);
        true
    }

    /// The pooled arm of [`Self::push_cached_compact_reusing`]: rebuild the
    /// retired slot at `d` in the buffers it still owns.
    ///
    /// Out of line (interpreter round i1 wave 32). With the slab on, a warm
    /// slot is a window and this arm runs only for a slot a by-value push
    /// left behind; inlined next to the window arm it doubled the door's code
    /// and pushed the window arm's live values onto the stack.
    #[inline(never)]
    fn reset_pooled_compact_at(
        &mut self,
        d: usize,
        cached: Arc<CachedBytecodeMethod>,
        args: &[(CompactValue, u8)],
    ) {
        let mark = self.slab.mark();
        let frame = &mut self.buf[d];
        frame.reset_cached_compact(cached, args);
        frame.slab_mark = mark;
    }

    /// [`Self::reset_pooled_compact_at`] for `Value` arguments.
    #[inline(never)]
    fn reset_pooled_value_at(
        &mut self,
        d: usize,
        cached: Arc<CachedBytecodeMethod>,
        args: &[Value],
    ) {
        let mark = self.slab.mark();
        let frame = &mut self.buf[d];
        frame.reset_cached_value(cached, args);
        frame.slab_mark = mark;
    }

    /// Is the retired slot at the current depth still holding POOLED buffers
    /// while the slot slab is on? Such a slot is rebuilt in its own buffers
    /// ([`Self::retired_slot_takes_a_window`]) for as long as it keeps them;
    /// `JvmThread::convert_retired_slot_to_window` hands them to the thread
    /// pools once, so the next install takes a window (interpreter round i1
    /// wave 32).
    #[inline(always)]
    pub fn retired_slot_holds_pooled_buffers(&self) -> bool {
        let d = self.depth;
        d < self.buf.len()
            && !self.buf[d].locals.is_window()
            && self.buf[d].stack.capacity() != 0
            && locals_slab_enabled()
    }

    /// Does the retired slot at `d` get rebuilt in a slab window?
    ///
    /// A windowed slot always does: its old window may already be another
    /// frame's memory, so it is never written again. A husk (its buffers were
    /// harvested) does when the slab is on. A slot that still owns pooled
    /// buffers (a by-value push left it) is rebuilt in them, as it always was,
    /// so the slab never frees a pooled buffer only to allocate a window next
    /// to it. The flag is read only for a husk.
    #[inline(always)]
    fn retired_slot_takes_a_window(&self, d: usize) -> bool {
        let slot = &self.buf[d];
        slot.locals.is_window() || (slot.stack.capacity() == 0 && locals_slab_enabled())
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
        let t0 = crate::runtime::interpreter::invoke_phases::now();
        let d = self.depth;
        if self.retired_slot_takes_a_window(d) {
            let eff_max_locals = effective_max_locals(cached.max_locals, args);
            let n = eff_max_locals as usize;
            let padded = padded_operand_stack(&cached);
            let w = self.slab.alloc(n + padded);
            // SAFETY: `w` is a fresh window of `n + padded` slots for the frame
            // at `d`; `lay_value_args` writes at most `n` local slots.
            unsafe {
                lay_value_args(w.vals.as_ptr(), w.kinds.as_ptr(), n, args);
                self.buf[d].reset_cached_in_window(cached, w, n, padded, eff_max_locals);
            }
        } else {
            self.reset_pooled_value_at(d, cached, args);
        }
        self.depth += 1;
        crate::runtime::interpreter::invoke_phases::charge_slot_build(t0);
        true
    }

    /// Install a frame for `cached` as the next live frame, for the fast doors
    /// when no slot is retired (the first call at a depth, and every call
    /// under `CRATONVM_JIT_NO_FRAME_SLOT_REUSE`).
    ///
    /// With the slot slab on (the default) the locals and operand stack are
    /// one window of it and the thread pools are not touched; with
    /// `CRATONVM_JIT_NO_LOCALS_SLAB` this is exactly the pooled
    /// `take_cached_compact_parts` + [`Self::emplace_cached_compact`] pair
    /// the doors called before.
    #[inline]
    pub fn emplace_cached_compact_args(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        args: &[(CompactValue, u8)],
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) {
        let t0 = crate::runtime::interpreter::invoke_phases::now();
        if locals_slab_enabled() {
            self.emplace_compact_in_window(cached, args);
        } else {
            let (locals, kinds, stack, eff_max_locals) =
                take_cached_compact_parts(&cached, args, locals_pool, stacks_pool);
            self.emplace_cached_compact(cached, locals, kinds, stack, eff_max_locals);
        }
        crate::runtime::interpreter::invoke_phases::charge_slot_build(t0);
    }

    /// The slab arm of [`Self::emplace_cached_compact_args`].
    #[inline]
    fn emplace_compact_in_window(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        args: &[(CompactValue, u8)],
    ) {
        let (n, eff_max_locals) = compact_locals_len(&cached, args);
        note_dead_compact_args(&cached, args);
        let padded = padded_operand_stack(&cached);
        let w = self.slab.alloc(n + padded);
        // SAFETY: `w` is a fresh window of `n + padded` slots, and `n` covers
        // every argument slot.
        unsafe {
            lay_compact_args(w.vals.as_ptr(), w.kinds.as_ptr(), n, args);
            self.emplace_windowed(cached, w, n, padded, eff_max_locals);
        }
    }

    /// [`Self::emplace_cached_compact_args`] for the general dispatchers,
    /// whose arguments are still `Value`s (`install_cached_frame`); the
    /// pooled arm is `take_cached_value_parts` +
    /// [`Self::emplace_cached_compact`], as before.
    #[inline]
    pub fn emplace_cached_value_args(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        args: &[Value],
        locals_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
        stacks_pool: &mut Vec<(Vec<u64>, Vec<u8>)>,
    ) {
        let t0 = crate::runtime::interpreter::invoke_phases::now();
        if locals_slab_enabled() {
            self.emplace_value_in_window(cached, args);
        } else {
            let (locals, kinds, stack, eff_max_locals) =
                take_cached_value_parts(&cached, args, locals_pool, stacks_pool);
            self.emplace_cached_compact(cached, locals, kinds, stack, eff_max_locals);
        }
        crate::runtime::interpreter::invoke_phases::charge_slot_build(t0);
    }

    /// The slab arm of [`Self::emplace_cached_value_args`].
    #[inline]
    fn emplace_value_in_window(&mut self, cached: Arc<CachedBytecodeMethod>, args: &[Value]) {
        let eff_max_locals = effective_max_locals(cached.max_locals, args);
        let n = eff_max_locals as usize;
        let padded = padded_operand_stack(&cached);
        let w = self.slab.alloc(n + padded);
        // SAFETY: `w` is a fresh window of `n + padded` slots; `lay_value_args`
        // writes at most `n` local slots.
        unsafe {
            lay_value_args(w.vals.as_ptr(), w.kinds.as_ptr(), n, args);
            self.emplace_windowed(cached, w, n, padded, eff_max_locals);
        }
    }

    /// Stage 2 of the contiguous interpreter stack, argument overlap
    /// (interpreter round i1 wave 37, lane L7): install a frame for `cached`
    /// whose locals BEGIN at the caller's first argument slot, so the
    /// arguments stay where the caller pushed them and become the callee's
    /// first locals, as in HotSpot's interpreter.
    ///
    /// `args` are the `args.len()` slots the fast door read verbatim off the
    /// top of the caller's operand stack (`invoke_fast::read_args_verbatim`)
    /// and has already discarded (`ValueStack::discard_top`), with no
    /// safepoint since. They live in the door's own buffer, never in the
    /// slab. The caller is the top frame.
    ///
    /// # The layouts differ for `long` and `double`
    ///
    /// A category-2 value is ONE operand-stack slot here and TWO local slots
    /// (the upper one filler). Without one, the arguments' slots are already
    /// the callee's locals and only their kind bytes (operand-stack marks,
    /// numerically the same as the local marks for a `long` / `double`) are
    /// rewritten; with one, the locals are laid again from the door's copy
    /// (`lay_overlapped_args`) -- never from the slab, which the lay
    /// overwrites.
    ///
    /// # What the caller sees
    ///
    /// Its operand stack already looks popped (`discard_top`), so the GC root
    /// scans, `update_object_refs` and freeze/thaw read each argument once,
    /// as the callee's local. The one moment two frames cover a slot is a
    /// value return: the return arms push the result into the caller BEFORE
    /// the callee leaves (so it is rooted across `MethodExit` / `FramePop`),
    /// and that push lands on the callee's first slot. Those arms call
    /// [`Frame::release_caller_overlap`] first, which drops the callee's view
    /// of its locals and operand stack.
    ///
    /// # Refusals
    ///
    /// `Err(cached)`, with nothing changed, when the caller's operand stack
    /// is not a slab window (an owned frame) or the slab refuses the window
    /// (`SlotSlab::alloc_overlapping`: another chunk, no room). The door then
    /// builds the frame the stage-1 way. JDWP (`--jdwp` / a debugger that
    /// reads locals) is refused by the door before this is called, because a
    /// released callee would show a debugger no locals at `MethodExit`.
    #[inline]
    pub fn push_cached_compact_overlapping(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        args: &[(CompactValue, u8)],
    ) -> Result<(), Arc<CachedBytecodeMethod>> {
        let t0 = crate::runtime::interpreter::invoke_phases::now();
        let d = self.depth;
        let top_and_end = match d.checked_sub(1) {
            Some(c) => self.buf[c].stack.window_top_and_end(),
            None => None,
        };
        let Some((start, end)) = top_and_end else {
            crate::runtime::interpreter::invoke_phases::note_overlap(
                t0,
                crate::runtime::interpreter::invoke_phases::P_OVERLAP_DECLINED_N,
            );
            return Err(cached);
        };
        let (n, eff_max_locals) = compact_locals_len(&cached, args);
        let padded = padded_operand_stack(&cached);
        let Some(w) = self.slab.alloc_overlapping(start, end, n + padded) else {
            crate::runtime::interpreter::invoke_phases::note_overlap(
                t0,
                crate::runtime::interpreter::invoke_phases::P_OVERLAP_DECLINED_N,
            );
            return Err(cached);
        };
        note_dead_compact_args(&cached, args);
        // SAFETY: `w` is a window of `n + padded` slots the slab just handed
        // out, starting at the caller's first (popped) argument slot, so its
        // first `args.len()` slots hold the arguments; `n` covers every
        // argument slot, and `args` is the door's buffer, not slab memory.
        let relaid = unsafe { lay_overlapped_args(w.vals.as_ptr(), w.kinds.as_ptr(), n, args) };
        if crate::runtime::interp_census::interp_frames_enabled() {
            crate::runtime::interp_census::record_interp_frame(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
        }
        self.reserve_stable(1);
        // SAFETY: `w` is the window just handed out for the frame at `d`, of
        // `n + padded` slots with its `n` locals laid.
        unsafe {
            if d < self.buf.len() {
                // A retired slot: a window's drop is nothing; the door handed
                // a slot's pooled buffers to the pools before calling here.
                self.buf[d].reset_cached_in_window(cached, w, n, padded, eff_max_locals);
            } else {
                // `reserve_stable(1)` guarantees `capacity > buf.len() == d`.
                let dst = self.buf.as_mut_ptr().add(d);
                std::ptr::write(
                    dst,
                    Frame::cached_in_window(cached, w, n, padded, eff_max_locals),
                );
                self.buf.set_len(d + 1);
            }
        }
        let frame = &mut self.buf[d];
        frame.locals.mark_shared_with_caller();
        frame.local_kinds.mark_shared_with_caller();
        self.depth += 1;
        crate::runtime::interpreter::invoke_phases::charge_slot_build(t0);
        crate::runtime::interpreter::invoke_phases::note_overlap(
            t0,
            if relaid {
                crate::runtime::interpreter::invoke_phases::P_OVERLAP_RELAID_N
            } else {
                crate::runtime::interpreter::invoke_phases::P_OVERLAP_N
            },
        );
        Ok(())
    }

    /// [`Self::push_cached_compact_overlapping`] with the arguments still ON
    /// the caller's operand stack and no copy of them anywhere (stage 2b of the
    /// contiguous interpreter stack; interpreter round i1 wave 38, lane L7).
    /// `tags` are the arguments' descriptor tags in operand-stack order (`L`
    /// for a receiver), and the door has validated every one of those slots
    /// (`invoke_fast::validate_args_verbatim`) with no safepoint since.
    ///
    /// The window starts at the caller's first argument slot. Only once the
    /// slab grants it does the caller give the arguments up (`discard_top`),
    /// and they are then laid as the callee's locals where they are
    /// ([`lay_overlapped_args_in_place`]): a category-2 argument moves them up
    /// within the window instead of being laid again from a copy.
    ///
    /// `Err(cached)` with NOTHING changed -- the arguments still on the
    /// caller's stack -- on the same refusals as the stage-2 install (no top
    /// caller, an owned caller, another chunk, no room), so the door can still
    /// read them for the stage-1 install.
    #[inline]
    pub fn push_cached_compact_in_place(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        tags: &[u8],
    ) -> Result<(), Arc<CachedBytecodeMethod>> {
        let t0 = crate::runtime::interpreter::invoke_phases::now();
        let d = self.depth;
        let nargs = tags.len();
        let top_and_end = match d.checked_sub(1) {
            Some(c) if self.buf[c].stack.len() >= nargs => self.buf[c].stack.window_top_and_end(),
            _ => None,
        };
        let Some((top, end)) = top_and_end else {
            crate::runtime::interpreter::invoke_phases::note_overlap(
                t0,
                crate::runtime::interpreter::invoke_phases::P_OVERLAP_DECLINED_N,
            );
            return Err(cached);
        };
        // The caller's first argument slot: `nargs` slots below its stack top,
        // inside its window (its stack holds at least `nargs` slots).
        let start = top - nargs * std::mem::size_of::<CompactValue>();
        let cat2 = tags.iter().filter(|&&t| matches!(t, b'J' | b'D')).count();
        let n = (cached.max_locals as usize).max(nargs + cat2);
        let eff_max_locals = u16::try_from(n).unwrap_or(u16::MAX);
        let padded = padded_operand_stack(&cached);
        let Some(w) = self.slab.alloc_overlapping(start, end, n + padded) else {
            crate::runtime::interpreter::invoke_phases::note_overlap(
                t0,
                crate::runtime::interpreter::invoke_phases::P_OVERLAP_DECLINED_N,
            );
            return Err(cached);
        };
        // Granted: from here the argument slots are the callee's locals, and
        // the caller's stack no longer counts them (so every root scan and
        // frame reader sees each argument once, as the callee's local).
        self.buf[d - 1].stack.discard_top(nargs);
        // SAFETY: `w` is a window of `n + padded` slots the slab just handed
        // out, starting at the caller's first argument slot, so its first
        // `nargs` value slots hold the arguments; `n` covers every argument
        // slot.
        let relaid =
            unsafe { lay_overlapped_args_in_place(w.vals.as_ptr(), w.kinds.as_ptr(), n, tags) };
        if crate::runtime::interp_census::interp_frames_enabled() {
            crate::runtime::interp_census::record_interp_frame(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
        }
        self.reserve_stable(1);
        // SAFETY: `w` is the window just handed out for the frame at `d`, of
        // `n + padded` slots with its `n` locals laid.
        unsafe {
            if d < self.buf.len() {
                // A retired slot: a window's drop is nothing; the door handed
                // a slot's pooled buffers to the pools before calling here.
                self.buf[d].reset_cached_in_window(cached, w, n, padded, eff_max_locals);
            } else {
                // `reserve_stable(1)` guarantees `capacity > buf.len() == d`.
                let dst = self.buf.as_mut_ptr().add(d);
                std::ptr::write(
                    dst,
                    Frame::cached_in_window(cached, w, n, padded, eff_max_locals),
                );
                self.buf.set_len(d + 1);
            }
        }
        let frame = &mut self.buf[d];
        frame.locals.mark_shared_with_caller();
        frame.local_kinds.mark_shared_with_caller();
        self.depth += 1;
        crate::runtime::interpreter::invoke_phases::charge_slot_build(t0);
        crate::runtime::interpreter::invoke_phases::note_overlap(
            t0,
            if relaid {
                crate::runtime::interpreter::invoke_phases::P_OVERLAP_RELAID_N
            } else {
                crate::runtime::interpreter::invoke_phases::P_OVERLAP_N
            },
        );
        crate::runtime::interpreter::invoke_phases::note_overlap(
            t0,
            crate::runtime::interpreter::invoke_phases::P_OVERLAP_IN_PLACE_N,
        );
        Ok(())
    }

    /// Make a frame for `cached` in the window `w` (its `n` locals already
    /// laid) the next live frame: written in place like
    /// [`Self::emplace_cached_compact`], or rebuilt in a retired slot a caller
    /// did not harvest.
    ///
    /// # Safety
    ///
    /// `w` is the window `self.slab.alloc(n + padded)` just returned.
    unsafe fn emplace_windowed(
        &mut self,
        cached: Arc<CachedBytecodeMethod>,
        w: SlabWindow,
        n: usize,
        padded: usize,
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
        let d = self.depth;
        if d < self.buf.len() {
            // A retired slot is here after all. Rebuilding it drops its old
            // buffers, as the assignment in `emplace_cached_compact` does.
            self.buf[d].reset_cached_in_window(cached, w, n, padded, eff_max_locals);
        } else {
            // `reserve_stable(1)` guarantees `capacity > buf.len() == d`, so
            // `dst` is the one slot past the last initialised element; it is
            // published by `set_len` only once fully written.
            let dst = self.buf.as_mut_ptr().add(d);
            std::ptr::write(
                dst,
                Frame::cached_in_window(cached, w, n, padded, eff_max_locals),
            );
            self.buf.set_len(d + 1);
        }
        self.depth += 1;
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
        let slab_mark = self.slab.mark();
        if self.depth < self.buf.len() {
            // A retired slot is here after all (a caller that did not harvest).
            // Assigning drops it, which is exactly what `push` would have done.
            let mut frame = Frame::from_cached_compact_parts(
                cached,
                locals,
                local_kinds,
                stack,
                eff_max_locals,
            );
            frame.slab_mark = slab_mark;
            self.buf[self.depth] = frame;
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
                        locals: SlotVec::from_vec(locals),
                        local_kinds: SlotVec::from_vec(local_kinds),
                        stack,
                        code,
                        max_stack,
                        max_locals: eff_max_locals,
                        inner: FrameInner::Cached(cached),
                        method_index: None,
                        backward_count: 0,
                        osr_poll_at: 0,
                        osr_attempt_counts: Vec::new(),
                        monitor_on_exit: None,
                        held_monitors:
                            crate::runtime::interpreter::held_monitors::HeldMonitors::new(),
                        seq,
                        exec_epoch: 0,
                        slab_mark,
                        redefine_stamp: current_redefine_stamp(),
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
        // The slab goes with `self`: every frame leaves with owned buffers.
        for frame in self.buf.iter_mut() {
            frame.detach_from_slab();
        }
        self.buf.into_iter()
    }
}

impl FromIterator<Frame> for FrameStack {
    fn from_iter<T: IntoIterator<Item = Frame>>(iter: T) -> Self {
        let buf: Vec<Frame> = iter.into_iter().collect();
        Self::from(buf)
    }
}

impl From<Vec<Frame>> for FrameStack {
    #[inline]
    fn from(mut buf: Vec<Frame>) -> Self {
        // By-value frames own their buffers; the new stack's slab is empty,
        // so every one of them was pushed at its bottom mark.
        for frame in buf.iter_mut() {
            frame.slab_mark = 0;
        }
        let depth = buf.len();
        Self {
            buf,
            depth,
            reloc_epoch: 0,
            code_moves: 0,
            conversion_pending: false,
            conversion_retries: 0,
            conversion_retry_began: None,
            conversion_given_up_at: Self::NEVER_GAVE_UP,
            slab: SlotSlab::new(),
            requested_frame_limit: 0,
        }
    }
}

impl From<FrameStack> for Vec<Frame> {
    #[inline]
    fn from(mut fs: FrameStack) -> Vec<Frame> {
        fs.trim_retired();
        // The slab is dropped with `fs`: every frame leaves with owned buffers.
        for frame in fs.buf.iter_mut() {
            frame.detach_from_slab();
        }
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
        Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
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
            },
        ))
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
            f.local_kinds.to_vec(),
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
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
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
            },
        ));
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
            method_descriptor: None,
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
        assert!(!mask.is_live(1), "analyzer must report slot 1 dead");

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

    /// i1 wave 5, lane L4 — precise-oop-map shadow, stage 1: against a
    /// published verifier map, a reference local whose value the heuristic
    /// scan cannot root is `Missed`, and a rooted local the map types `int` is
    /// `Extra`.
    #[test]
    fn oop_map_shadow_reports_missed_and_extra_locals() {
        use crate::memory::VmHeap;
        use crate::runtime::local_liveness::{oop_map_shadow, LiveMask};
        use cratonvm_classloading::vtype::VType;
        use cratonvm_gc::GcBackend;

        // A VM's own store (wave 7: the shadow no longer reads the process
        // mirror). Inside the store's 4,194,304 addressable ids: an id past it
        // is silently unpublished and the frame reads as unmapped.
        let cid = ClassId::new(0x003F_4C34);
        let mut builder = cratonvm_classloading::MethodTypeMapsBuilder::new(2, 1);
        builder.record_slots(
            0,
            &[VType::ObjectRef(Arc::from("java/lang/Object")), VType::Int],
            &[],
        );
        let maps = builder.finish(true);
        let store = cratonvm_classloading::TypeMapStore::new();
        assert!(store.publish(
            cid,
            cratonvm_classloading::ClassTypeMaps::new(vec![(
                Arc::from("m"),
                Arc::from("()V"),
                Some(maps),
            )]),
        ));

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let mut frame = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            2,
            &[],
        );
        // Slot 0: typed as a reference, holding an int — nothing the scan can root.
        frame.set_local_unchecked(0, Value::Int(12345));
        // Slot 1: typed `int`, holding a live object — rooted by its tag.
        frame.set_local_unchecked(1, Value::Object(Some(obj)));

        let [_, _, _, _, _, missed_before, extra_before] = oop_map_shadow::counts();
        frame.shadow_compare_oop_maps(&heap, &LiveMask::all_live(), &store);
        let [_, _, _, _, _, missed_after, extra_after] = oop_map_shadow::counts();
        assert!(missed_after > missed_before, "slot 0 is a missed reference");
        assert!(extra_after > extra_before, "slot 1 is an extra root");

        // The same frame against a store that never saw the class: unmapped,
        // no verdicts — the process store's maps are not consulted.
        let empty = cratonvm_classloading::TypeMapStore::new();
        let [_, unmapped_before, _, _, _, _, _] = oop_map_shadow::counts();
        frame.shadow_compare_oop_maps(&heap, &LiveMask::all_live(), &empty);
        let [_, unmapped_after, _, _, _, _, _] = oop_map_shadow::counts();
        assert!(unmapped_after > unmapped_before);
    }

    /// i1 wave 7, lane L4 — the remap twin of the shadow: a reference-typed
    /// local that names a moved object but carries a `long` kind mark (the
    /// hazard `local_kind_at` documents) is left stale by the remap and is
    /// `Missed`; an `int`-typed slot holding an object the remap rewrote is
    /// `Extra`. The rewrite itself is exactly `update_local_refs` +
    /// `stack.update_object_refs`.
    #[test]
    fn oop_map_remap_shadow_reports_missed_and_extra_locals() {
        use crate::memory::VmHeap;
        use crate::runtime::local_liveness::oop_map_shadow;
        use cratonvm_classloading::vtype::VType;
        use cratonvm_gc::GcBackend;

        let cid = ClassId::new(0x0000_0107);
        let mut builder = cratonvm_classloading::MethodTypeMapsBuilder::new(2, 1);
        builder.record_slots(
            0,
            &[VType::ObjectRef(Arc::from("java/lang/Object")), VType::Int],
            &[],
        );
        let store = cratonvm_classloading::TypeMapStore::new();
        assert!(store.publish(
            cid,
            cratonvm_classloading::ClassTypeMaps::new(vec![(
                Arc::from("m"),
                Arc::from("()V"),
                Some(builder.finish(true)),
            )]),
        ));

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        // The minted-handle tables are keyed by the heap's address (a stack slot
        // here): drop any entry an earlier test on this thread left at it, or a
        // stale mint equal to `old_addr` makes the `long` slot below remappable.
        crate::memory::smuggled_longs::forget_heap(&heap);
        let old_obj = heap.alloc_object(ClassId::new(0), 0);
        let new_obj = heap.alloc_object(ClassId::new(0), 0);
        let old_addr = old_obj.as_ptr() as usize;
        let new_addr = new_obj.as_ptr() as usize;
        // Slot 0 (map: reference): an `int` whose bits equal a moved object's
        // old address is not rewritten. Slot 1 (map: `int`): an object that is.
        let mut frame = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            // aload_0; iload_1; return — both slots live at pc 0.
            vec![0x2a, 0x1b, 0xb1],
            vec![],
            2,
            2,
            &[],
        );
        let lost = CompactValue::long(old_addr as i64);
        frame.set_local_compact_unchecked(0, lost);
        frame.set_local_unchecked(1, Value::Object(Some(old_obj)));
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_addr, new_addr);
        // Without the mark, the lost-tag path would recover and rewrite slot
        // 0; with it the remap treats the slot as a primitive (unminted, so
        // verbatim) — the stale reference the precise map would have fixed.
        frame.local_kinds[0] = LKIND_LONG;

        let [_, missed_before, extra_before] = oop_map_shadow::remap_counts();
        let snapshot = frame.shadow_slot_snapshot();
        frame.update_local_refs(&map, &heap);
        frame.stack.update_object_refs(&map, &heap);
        frame.shadow_compare_remap(&map, &store, &snapshot.0, &snapshot.1);
        let [_, missed_after, extra_after] = oop_map_shadow::remap_counts();
        assert_eq!(
            frame.get_local_raw(0),
            lost.raw_bits(),
            "slot 0 is left alone"
        );
        assert_eq!(
            frame.get_local_compact(1).as_object_ptr(),
            Some(new_addr as u64),
            "slot 1 follows the move"
        );
        assert!(missed_after > missed_before, "slot 0 is a missed remap");
        assert!(extra_after > extra_before, "slot 1 is an extra rewrite");

        // The public entry rewrites exactly as the direct pair does.
        let mut twin = Frame::new(
            cid,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x2a, 0x1b, 0xb1],
            vec![],
            2,
            2,
            &[],
        );
        twin.set_local_unchecked(1, Value::Object(Some(old_obj)));
        twin.update_frame_refs_mapped(&map, &heap, &store);
        assert_eq!(
            twin.get_local_compact(1).as_object_ptr(),
            Some(new_addr as u64)
        );
    }

    /// i1 wave 5, lane L4: a `long` local holding a MINTED smuggled handle is
    /// rooted and remapped, like the same value on the operand stack; an
    /// unminted `long` equal to a live object's address stays unrooted and
    /// verbatim.
    #[test]
    fn minted_long_local_is_rooted_and_remapped_unminted_is_not() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let minted = heap.alloc_object(ClassId::new(0), 0);
        let plain = heap.alloc_object(ClassId::new(0), 0);
        let moved_to = heap.alloc_object(ClassId::new(0), 0);
        let minted_addr = minted.as_ptr() as u64;
        let plain_addr = plain.as_ptr() as u64;
        let new_addr = moved_to.as_ptr() as u64;
        // The mint registry is keyed by the heap's address, which this local
        // shares with earlier tests' heaps on the thread: start clean.
        crate::memory::smuggled_longs::forget_heap(&heap);
        crate::memory::smuggled_longs::record_minted_long(&heap, minted_addr);

        let mut frame = Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            10,
            4,
            &[],
        );
        frame.set_local_unchecked(0, Value::Long(minted_addr as i64));
        frame.set_local_unchecked(2, Value::Long(plain_addr as i64));

        let mut roots = Vec::new();
        frame.scan_local_objects_all_live(&mut roots, &heap);
        assert!(roots.iter().any(|r| r.as_ptr() as u64 == minted_addr));
        assert!(!roots.iter().any(|r| r.as_ptr() as u64 == plain_addr));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(minted_addr as usize, new_addr as usize);
        map.insert(plain_addr as usize, new_addr as usize);
        frame.update_local_refs(&map, &heap);
        assert_eq!(
            frame.get_local_raw(0),
            new_addr,
            "minted handle follows the move"
        );
        assert_eq!(
            frame.get_local_raw(2),
            plain_addr,
            "a primitive stays verbatim"
        );
        crate::memory::smuggled_longs::forget_heap(&heap);
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

    /// An entry opened at 0 reads as "asked" without spending an attempt, and
    /// one set to `OSR_MAX_ATTEMPTS` retires the loop for the activation.
    #[test]
    fn osr_attempt_entries_open_at_zero_and_retire_at_the_cap() {
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
        frame.backward_count = OSR_THRESHOLD_FOR_TEST;
        assert_eq!(frame.osr_attempts_at(4), None);
        frame.set_osr_attempts(4, 0);
        assert_eq!(frame.osr_attempts_at(4), Some(0));
        assert!(frame.should_try_osr(4, OSR_THRESHOLD_FOR_TEST));
        frame.record_osr_rejection(4);
        assert_eq!(frame.osr_attempts_at(4), Some(1));
        frame.set_osr_attempts(4, OSR_MAX_ATTEMPTS);
        assert_eq!(frame.osr_attempt_counts, vec![(4, OSR_MAX_ATTEMPTS)]);
        assert!(!frame.should_try_osr(4, OSR_THRESHOLD_FOR_TEST));
        assert_eq!(
            frame.osr_attempts_at(9),
            None,
            "other loop headers untouched"
        );
    }

    const OSR_THRESHOLD_FOR_TEST: u32 = 1_000;

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

    /// Interpreter round i1 wave 26, lane L7: the dispatch loop polls its OS
    /// thread's poll word, not the stack's move count, so a move must bump
    /// both -- the stack's count (which the loop's slow path compares) and the
    /// word of the thread it runs on (which sends the loop there), by whole
    /// `MOVE_STEP`s that never touch the pause count.
    #[test]
    fn a_move_bumps_the_stack_count_and_this_threads_poll_word() {
        use crate::threading::gc_barrier::{GcBarrier, LoopPollWord};
        let barrier = GcBarrier::new();
        let word = barrier.loop_poll_word_for_this_thread();
        // SAFETY: this OS thread's thread-local word, alive for the whole test.
        let before = unsafe { &*word }.load();
        let mut frames = FrameStack::new();
        assert_eq!(frames.code_moves(), 0);
        frames.note_code_moved();
        frames.note_conversion_deferred(7);
        assert_eq!(frames.code_moves(), 2);
        assert!(frames.conversion_pending());
        // SAFETY: as above.
        let after = unsafe { &*word }.load();
        assert_eq!(after.wrapping_sub(before), 2 * LoopPollWord::MOVE_STEP);
        assert_eq!(after & LoopPollWord::PAUSES, 0, "a move never counts a pause");
        assert_eq!(barrier.loop_poll_word_for_this_thread(), word, "one word per OS thread");
    }

    /// Interpreter round i1 wave 41, lane L3: a stack that never gave a
    /// conversion up defers one owed at redefinition count 0 -- the count a
    /// redefinition fence of the process's first redefinition is raised at.
    /// `0` used to mean "never gave up", so that deferral was refused and the
    /// fenced loop ran on across the swap. A give-up at a count still refuses
    /// a later deferral at that count, and only at it.
    #[test]
    fn a_fresh_stack_defers_a_conversion_owed_at_count_zero() {
        let mut frames = FrameStack::new();
        assert!(frames.note_conversion_deferred(0), "count 0 is not a give-up");
        assert!(frames.conversion_pending());
        frames.give_up_conversion(0);
        assert!(!frames.conversion_pending());
        assert!(!frames.note_conversion_deferred(0), "given up at 0");
        assert!(!frames.conversion_pending());
        assert!(frames.note_conversion_deferred(1));
        let mut from_vec = FrameStack::from(Vec::<Frame>::new());
        assert!(from_vec.note_conversion_deferred(0), "the Vec conversion starts fresh too");
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
    // FrameStack — slot-slab windows (wave 29, lane L7)
    // ===================================================================
    //
    // These call the slab arms directly (`emplace_*_in_window`), so they do
    // not depend on `CRATONVM_JIT_NO_LOCALS_SLAB`; a retired window is rebuilt
    // in a window whatever the flag says.

    /// The claim the slab's A/B rests on: a frame whose buffers are a window
    /// holds exactly what the pooled builders give the same call -- including
    /// a `long` and a `double` argument, whose upper slots are filler.
    #[test]
    fn a_windowed_frame_holds_what_a_pooled_frame_holds() {
        let cached = cached_probe("m", "(JIDLjava/lang/Object;)V", 9, 5);
        let compact = [
            (
                CompactValue::from_value_kinded(Value::Long(-9_000_000_000)),
                b'J',
            ),
            (CompactValue::from_value_kinded(Value::Int(7)), b'I'),
            (CompactValue::from_value_kinded(Value::Double(2.5)), b'D'),
            (CompactValue::null(), b'L'),
        ];
        let values = [
            Value::Long(-9_000_000_000),
            Value::Int(7),
            Value::Double(2.5),
            Value::Object(None),
        ];
        let mut locals_pool = Vec::new();
        let mut stacks_pool = Vec::new();
        let pooled_compact = Frame::new_pooled_cached_compact(
            Arc::clone(&cached),
            &compact,
            &mut locals_pool,
            &mut stacks_pool,
        );
        let pooled_value = Frame::new_pooled_cached(
            Arc::clone(&cached),
            &values,
            &mut locals_pool,
            &mut stacks_pool,
        );

        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&cached), &compact);
        frames.emplace_value_in_window(Arc::clone(&cached), &values);
        assert_eq!(frames.len(), 2);
        for f in frames.iter() {
            assert!(f.locals.is_window() && f.local_kinds.is_window() && f.stack.is_windowed());
        }
        assert_eq!(frame_shape(&frames[0]), frame_shape(&pooled_compact));
        assert_eq!(frame_shape(&frames[1]), frame_shape(&pooled_value));
        assert_eq!(frames[0].stack.capacity(), pooled_compact.stack.capacity());
        assert_eq!(frames[0].get_local(2), Value::Int(7));
        assert_eq!(frames[0].get_local(3), Value::Double(2.5));
        // One window per frame: the operand stack follows the locals, and the
        // next frame's window follows that (addresses compared as integers).
        let slot = std::mem::size_of::<CompactValue>();
        let window = frames[0].locals_len() + frames[0].stack.capacity();
        assert_eq!(
            frames[1].locals.as_ptr() as usize,
            frames[0].locals.as_ptr() as usize + window * slot
        );
        let _ = frames[0].stack.push(Value::Int(41));
        assert_eq!(frames[0].stack.peek_int_unchecked(), 41);
        assert_eq!(
            frames[1].get_local(2),
            Value::Int(7),
            "a push stays in its own window"
        );
    }

    /// Recursion, the shape the slab is for: windows deep enough to span
    /// several slab chunks keep their contents and addresses while frames
    /// above them are pushed, retired (the interpreter's return), rebuilt in
    /// their retired slots, and truncated.
    #[test]
    fn windowed_frames_keep_their_locals_across_deeper_pushes_and_pops() {
        const DEPTH: usize = 3000;
        let cached = cached_probe("f", "(I)I", 3, 4);
        let mut frames = FrameStack::new();
        frames.reserve_stable(DEPTH);
        let mut addrs: Vec<*const CompactValue> = Vec::with_capacity(DEPTH);
        for i in 0..DEPTH {
            frames.emplace_compact_in_window(
                Arc::clone(&cached),
                &[(CompactValue::int(i as i32), b'I')],
            );
            frames[i].set_local_int_unchecked(2, -(i as i32));
            let _ = frames[i].stack.push(Value::Int(i as i32 * 3));
            addrs.push(frames[i].locals.as_ptr());
        }
        let check = |frames: &FrameStack, upto: usize| {
            for i in 0..upto {
                assert_eq!(
                    frames[i].get_local_int_unchecked(0),
                    i as i32,
                    "arg of frame {i}"
                );
                assert_eq!(
                    frames[i].get_local_int_unchecked(2),
                    -(i as i32),
                    "local of frame {i}"
                );
                assert_eq!(
                    frames[i].stack.peek_int_unchecked(),
                    i as i32 * 3,
                    "stack of frame {i}"
                );
                assert_eq!(
                    frames[i].locals.as_ptr(),
                    addrs[i],
                    "window of frame {i} moved"
                );
            }
        };
        check(&frames, DEPTH);

        // Return from the upper half the way the interpreter does, then call
        // down through the retired slots again.
        for _ in 0..DEPTH / 2 {
            assert!(frames.retire_top());
        }
        for i in DEPTH / 2..DEPTH {
            assert!(
                frames.push_cached_compact_reusing(
                    Arc::clone(&cached),
                    &[(CompactValue::int(7), b'I')],
                )
            );
            assert!(
                frames[i].locals.is_window(),
                "a retired window is rebuilt in a window"
            );
            assert_eq!(frames[i].get_local_int_unchecked(0), 7);
            assert_eq!(
                frames[i].locals.as_ptr(),
                addrs[i],
                "the same depth, the same window"
            );
            assert!(
                frames[i].stack.is_empty(),
                "a rebuilt frame starts with an empty stack"
            );
        }
        check(&frames, DEPTH / 2);
        frames.truncate(10);
        check(&frames, 10);
        frames.clear();
        frames.emplace_compact_in_window(Arc::clone(&cached), &[(CompactValue::int(1), b'I')]);
        assert_eq!(
            frames[0].locals.as_ptr(),
            addrs[0],
            "a cleared stack starts at the bottom"
        );
    }

    /// Owned and windowed frames interleave on one stack (a by-value push
    /// between two cached calls): popping past the owned frame still releases
    /// exactly the windows above the frame that stays.
    #[test]
    fn owned_frames_between_windowed_ones_release_the_right_windows() {
        let cached = cached_probe("m", "(I)V", 2, 2);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&cached), &[(CompactValue::int(1), b'I')]);
        frames.push(test_frame("owned", 2));
        frames.emplace_compact_in_window(Arc::clone(&cached), &[(CompactValue::int(3), b'I')]);
        assert!(frames[0].locals.is_window());
        assert!(!frames[1].locals.is_window());
        assert!(frames[2].locals.is_window());
        let third = frames[2].locals.as_ptr();

        frames.truncate(1);
        frames.emplace_compact_in_window(Arc::clone(&cached), &[(CompactValue::int(4), b'I')]);
        assert_eq!(
            frames[1].locals.as_ptr(),
            third,
            "the released window is reused"
        );
        assert_eq!(
            frames[0].get_local_int_unchecked(0),
            1,
            "the frame below is intact"
        );
        assert_eq!(frames[1].get_local_int_unchecked(0), 4);

        // An owned retired slot is rebuilt in its own buffers, not a window.
        frames.truncate(0);
        frames.push(test_frame("owned", 2));
        assert!(frames.retire_top());
        assert!(frames
            .push_cached_compact_reusing(Arc::clone(&cached), &[(CompactValue::int(5), b'I')],));
        assert!(
            !frames[0].locals.is_window(),
            "pooled buffers are kept, not freed"
        );
        assert_eq!(frames[0].get_local_int_unchecked(0), 5);
    }

    /// A frame that leaves by value takes owned copies: the slab reuses its
    /// window for the next push, and the copy must not change when it does.
    #[test]
    fn a_popped_windowed_frame_leaves_with_its_own_copy() {
        let cached = cached_probe("m", "(JI)V", 4, 2);
        let args = [
            (CompactValue::from_value_kinded(Value::Long(1 << 40)), b'J'),
            (CompactValue::int(5), b'I'),
        ];
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&cached), &args);
        let _ = frames[0].stack.push(Value::Int(11));
        let window = frames[0].locals.as_ptr();
        let before = frame_shape(&frames[0]);

        let popped = frames.pop().expect("one frame");
        assert!(!popped.locals.is_window() && !popped.stack.is_windowed());
        assert_eq!(frame_shape(&popped), before);
        assert_eq!(popped.stack.peek_int_unchecked(), 11);

        let other = cached_probe("n", "(II)V", 4, 2);
        frames.emplace_compact_in_window(
            other,
            &[(CompactValue::int(99), b'I'), (CompactValue::int(98), b'I')],
        );
        assert_eq!(frames[0].locals.as_ptr(), window, "the window was released");
        assert_eq!(frame_shape(&popped), before, "the popped copy is its own");

        // The `Vec<Frame>` conversions detach every live frame the same way.
        frames.push(test_frame("owned", 1));
        let v: Vec<Frame> = frames.into();
        assert_eq!(v.len(), 2);
        assert!(v
            .iter()
            .all(|f| !f.locals.is_window() && !f.stack.is_windowed()));
        assert_eq!(v[0].get_local_int_unchecked(0), 99);
        assert_eq!(v[0].get_local_int_unchecked(1), 98);
    }

    /// The collector reads a windowed frame's locals and operand stack
    /// through the same scans as a pooled frame's, and its remap writes land
    /// in the window.
    #[test]
    fn the_gc_scans_and_remaps_a_windowed_frames_locals_and_stack() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let a = heap.alloc_object(ClassId::new(0), 0);
        let b = heap.alloc_object(ClassId::new(0), 0);
        let a_moved = heap.alloc_object(ClassId::new(0), 0);
        let b_moved = heap.alloc_object(ClassId::new(0), 0);

        // Slot 0 is always live, whatever the liveness analysis says of `return`.
        let cached = cached_probe("m", "(Ljava/lang/Object;)V", 2, 2);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(
            Arc::clone(&cached),
            &[(
                CompactValue::from_value_kinded(Value::Object(Some(a))),
                b'L',
            )],
        );
        frames[0].stack.push(Value::Object(Some(b))).expect("push");
        assert!(frames[0].locals.is_window());

        let mut roots = Vec::new();
        frames[0].scan_local_objects(&mut roots, &heap);
        frames[0].stack.scan_object_refs(&mut roots, &heap);
        assert_eq!(roots, vec![a, b]);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(a.as_ptr() as usize, a_moved.as_ptr() as usize);
        map.insert(b.as_ptr() as usize, b_moved.as_ptr() as usize);
        frames[0].update_local_refs(&map, &heap);
        frames[0].stack.update_object_refs(&map, &heap);
        assert_eq!(
            frames[0].get_local_compact(0).as_object_ptr(),
            Some(a_moved.as_ptr() as u64)
        );
        assert_eq!(
            frames[0].stack.peek_compact().as_object_ptr(),
            Some(b_moved.as_ptr() as u64)
        );
    }

    /// A retired slot rebuilt for the method it last ran keeps its code
    /// `Arc` (no increment, no decrement); rebuilt for another method it
    /// takes that method's code and releases the old one.
    #[test]
    fn a_slot_rebuilt_for_the_same_method_keeps_its_code_arc() {
        let f = cached_probe("f", "(I)I", 2, 2);
        let g = cached_probe("g", "(I)I", 2, 2);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&f), &[(CompactValue::int(1), b'I')]);
        let held = Arc::strong_count(&f.code);
        for i in 0..4 {
            assert!(frames.retire_top());
            assert!(frames.push_cached_compact_reusing(
                Arc::clone(&f),
                &[(CompactValue::int(i), b'I')]
            ));
            assert!(Arc::ptr_eq(&frames[0].code, &f.code));
            assert_eq!(Arc::strong_count(&f.code), held);
            assert_eq!(frames[0].get_local_int_unchecked(0), i);
        }
        assert!(frames.retire_top());
        assert!(frames.push_cached_compact_reusing(Arc::clone(&g), &[(CompactValue::int(9), b'I')]));
        assert!(Arc::ptr_eq(&frames[0].code, &g.code));
        assert_eq!(Arc::strong_count(&f.code), held - 1, "the old code is released");
        assert_eq!(frames[0].method_name(), "g");
    }

    // ===================================================================
    // FrameStack — argument overlap (stage 2, wave 37, lane L7)
    // ===================================================================

    /// What a fast door hands the overlap: the top `tags.len()` operand-stack
    /// slots of `frame`, first argument first, read without popping
    /// (`invoke_fast::read_args_verbatim`).
    fn door_args(frame: &Frame, tags: &[u8]) -> Vec<(CompactValue, u8)> {
        let n = tags.len();
        (0..n)
            .map(|i| (frame.stack.peek_with_kind_at(n - 1 - i).0, tags[i]))
            .collect()
    }

    /// Push `vals` onto the top frame, read them as a door does, pop them,
    /// and overlap a frame for `callee` on them.
    fn call_overlapped(
        frames: &mut FrameStack,
        callee: &Arc<CachedBytecodeMethod>,
        vals: &[Value],
        tags: &[u8],
    ) -> Result<(), Arc<CachedBytecodeMethod>> {
        let top = frames.len() - 1;
        for v in vals {
            frames[top].stack.push(*v).expect("push");
        }
        let args = door_args(&frames[top], tags);
        frames[top].stack.discard_top(args.len());
        frames.push_cached_compact_overlapping(Arc::clone(callee), &args)
    }

    /// The claim stage 2 rests on: an overlapped frame holds exactly what a
    /// fresh window holds for the same call, with and without category-2
    /// arguments (one operand-stack slot, two local slots), and its locals
    /// start on the caller's first argument slot.
    #[test]
    fn an_overlapped_frame_holds_what_a_fresh_window_holds() {
        let cases: [(&str, Vec<Value>, &[u8]); 2] = [
            (
                "(ILjava/lang/Object;F)V",
                vec![Value::Int(7), Value::Object(None), Value::Float(1.5)],
                &b"ILF"[..],
            ),
            (
                "(JIDLjava/lang/Object;)V",
                vec![
                    Value::Long(-9_000_000_000),
                    Value::Int(3),
                    Value::Double(2.5),
                    Value::Object(None),
                ],
                &b"JIDL"[..],
            ),
        ];
        let slot = std::mem::size_of::<CompactValue>();
        for (desc, vals, tags) in cases {
            let caller = cached_probe("c", "()V", 2, 6);
            let callee = cached_probe("m", desc, 8, 3);
            let mut frames = FrameStack::new();
            frames.emplace_compact_in_window(Arc::clone(&caller), &[]);
            frames[0].stack.push(Value::Int(99)).expect("push");
            let (live_top, _) = frames[0].stack.window_top_and_end().expect("windowed");
            for v in &vals {
                frames[0].stack.push(*v).expect("push");
            }
            let args = door_args(&frames[0], tags);
            frames[0].stack.discard_top(args.len());
            assert!(frames
                .push_cached_compact_overlapping(Arc::clone(&callee), &args)
                .is_ok());
            assert_eq!(frames.len(), 2, "{desc}");
            assert!(frames[1].locals_overlap_caller(), "{desc}");
            assert_eq!(
                frames[1].locals.as_ptr() as usize,
                live_top,
                "{desc}: the callee's locals start on the first argument"
            );
            let mut fresh = FrameStack::new();
            fresh.emplace_compact_in_window(Arc::clone(&callee), &args);
            assert_eq!(frame_shape(&frames[1]), frame_shape(&fresh[0]), "{desc}");
            let (callee_stack, _) = frames[1].stack.window_top_and_end().expect("windowed");
            assert_eq!(
                callee_stack,
                live_top + frames[1].locals_len() * slot,
                "{desc}: the operand stack follows the locals"
            );
            assert_eq!(frames[0].stack.len(), 1, "{desc}: the caller looks popped");
            assert_eq!(frames[0].stack.peek_int_unchecked(), 99, "{desc}");
        }
    }

    /// Stage 2b (interpreter round i1 wave 38, lane L7): an overlap laid IN
    /// PLACE, from the caller's stack with no copy of the arguments, holds
    /// exactly what a fresh window holds -- including arguments that must move
    /// up past a `long` / `double` -- and a refusal leaves the arguments on the
    /// caller's stack.
    #[test]
    fn an_in_place_overlap_lays_what_a_fresh_window_holds() {
        let cases: [(&str, Vec<Value>, &[u8]); 4] = [
            (
                "(ILjava/lang/Object;F)V",
                vec![Value::Int(7), Value::Object(None), Value::Float(1.5)],
                &b"ILF"[..],
            ),
            (
                "(JIDLjava/lang/Object;)V",
                vec![
                    Value::Long(-9_000_000_000),
                    Value::Int(3),
                    Value::Double(2.5),
                    Value::Object(None),
                ],
                &b"JIDL"[..],
            ),
            (
                "(DJ)V",
                vec![Value::Double(-0.25), Value::Long(1 << 40)],
                &b"DJ"[..],
            ),
            ("()V", vec![], &b""[..]),
        ];
        for (desc, vals, tags) in cases {
            let caller = cached_probe("c", "()V", 2, 6);
            let callee = cached_probe("m", desc, 8, 3);
            let mut frames = FrameStack::new();
            frames.emplace_compact_in_window(Arc::clone(&caller), &[]);
            frames[0].stack.push(Value::Int(99)).expect("push");
            let (live_top, _) = frames[0].stack.window_top_and_end().expect("windowed");
            for v in &vals {
                frames[0].stack.push(*v).expect("push");
            }
            let args = door_args(&frames[0], tags);
            assert!(frames
                .push_cached_compact_in_place(Arc::clone(&callee), tags)
                .is_ok());
            assert_eq!(frames.len(), 2, "{desc}");
            assert!(frames[1].locals_overlap_caller(), "{desc}");
            assert_eq!(
                frames[1].locals.as_ptr() as usize,
                live_top,
                "{desc}: the callee's locals start on the first argument"
            );
            let mut fresh = FrameStack::new();
            fresh.emplace_compact_in_window(Arc::clone(&callee), &args);
            assert_eq!(frame_shape(&frames[1]), frame_shape(&fresh[0]), "{desc}");
            assert_eq!(frames[0].stack.len(), 1, "{desc}: the caller looks popped");
            assert_eq!(frames[0].stack.peek_int_unchecked(), 99, "{desc}");
        }
        // Refused (an owned caller): nothing moves, the arguments stay.
        let callee = cached_probe("m", "(I)V", 1, 1);
        let mut frames = FrameStack::new();
        frames.push(test_frame("owned", 2));
        frames[0].stack.push(Value::Int(5)).expect("push");
        assert!(frames
            .push_cached_compact_in_place(Arc::clone(&callee), b"I")
            .is_err());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].stack.len(), 1, "the argument is still the caller's");
        assert_eq!(frames[0].stack.peek_int_unchecked(), 5);
    }

    /// While the callee runs, each argument is ONE root (the callee's local;
    /// the caller's stack looks popped). A value return releases the
    /// callee's slots before its push into the caller lands on them, so the
    /// returned reference is one root too; without the release it would be
    /// the caller's and the callee's, and a moving collection would remap it
    /// twice. The slot is then overlapped again at the same depth.
    #[test]
    fn an_overlapped_argument_is_one_root_and_the_value_return_releases_it() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let a = heap.alloc_object(ClassId::new(0), 0);
        let b = heap.alloc_object(ClassId::new(0), 0);
        let roots = |frames: &FrameStack| {
            let mut r = Vec::new();
            for f in frames.iter() {
                f.scan_local_objects(&mut r, &heap);
                f.stack.scan_object_refs(&mut r, &heap);
            }
            r
        };
        let caller = cached_probe("c", "()V", 1, 4);
        let callee = cached_probe("m", "(Ljava/lang/Object;)Ljava/lang/Object;", 1, 2);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&caller), &[]);
        assert!(call_overlapped(&mut frames, &callee, &[Value::Object(Some(a))], b"L").is_ok());
        assert_eq!(roots(&frames), vec![a], "the argument is the callee's local only");

        // The value-return arms: release, push into the caller, retire.
        frames[1].release_caller_overlap();
        assert_eq!(frames[1].locals_len(), 0);
        frames[0].stack.push(Value::Object(Some(b))).expect("push");
        assert_eq!(roots(&frames), vec![b], "the result is the caller's root only");
        assert!(frames.retire_top());
        assert_eq!(
            frames[0].stack.peek_compact().as_object_ptr(),
            Some(b.as_ptr() as u64)
        );

        // The retired slot at depth 1 is overlapped again, on the new top.
        assert!(call_overlapped(&mut frames, &callee, &[Value::Object(Some(a))], b"L").is_ok());
        assert_eq!(frames.len(), 2);
        assert!(frames[1].locals_overlap_caller());
        assert_eq!(roots(&frames), vec![b, a]);
    }

    /// Calls nested two deep, each overlapped on its caller's live stack top:
    /// every frame keeps its values, the returns restore the slab to the
    /// caller's mark, and a plain window taken afterwards lands past the
    /// caller's whole window, not on its free stack slots.
    #[test]
    fn nested_overlaps_restore_the_slab_and_a_plain_window_lands_past_the_caller() {
        let caller = cached_probe("c", "()V", 1, 4);
        let mid = cached_probe("m", "(II)I", 3, 4);
        let leaf = cached_probe("l", "(I)I", 1, 2);
        let mut frames = FrameStack::new();
        frames.emplace_compact_in_window(Arc::clone(&caller), &[]);
        let (_, caller_end) = frames[0].stack.window_top_and_end().expect("windowed");
        let mark0 = frames.slab.mark();

        frames[0].stack.push(Value::Int(5)).expect("push");
        assert!(call_overlapped(&mut frames, &mid, &[Value::Int(1), Value::Int(2)], b"II").is_ok());
        frames[1].set_local_int_unchecked(2, 77);
        frames[1].stack.push(Value::Int(10)).expect("push");
        assert!(call_overlapped(&mut frames, &leaf, &[Value::Int(3)], b"I").is_ok());
        assert_eq!(frames[2].get_local_int_unchecked(0), 3);
        assert_eq!(frames[1].get_local_int_unchecked(0), 1);
        assert_eq!(frames[1].get_local_int_unchecked(1), 2);
        assert_eq!(frames[1].get_local_int_unchecked(2), 77);
        assert_eq!(frames[1].stack.peek_int_unchecked(), 10);
        assert_eq!(frames[0].stack.peek_int_unchecked(), 5);

        // Return from the leaf, then from the middle frame, as the arms do.
        frames[2].release_caller_overlap();
        frames[1].stack.push(Value::Int(4)).expect("push");
        assert!(frames.retire_top());
        assert_eq!(frames[1].stack.len(), 2);
        assert_eq!(frames[1].stack.peek_int_unchecked(), 4);
        assert_eq!(frames[1].get_local_int_unchecked(2), 77);
        frames[1].release_caller_overlap();
        frames[0].stack.push(Value::Int(6)).expect("push");
        assert!(frames.retire_top());
        assert_eq!(frames.slab.mark(), mark0, "the caller's mark is back");

        frames.emplace_compact_in_window(Arc::clone(&leaf), &[(CompactValue::int(8), b'I')]);
        assert_eq!(frames[1].locals.as_ptr() as usize, caller_end);
        assert!(!frames[1].locals_overlap_caller());
        assert_eq!(frames[0].stack.len(), 2);
        assert_eq!(frames[0].stack.peek_int_unchecked(), 6);
    }

    /// Refusals change nothing: no caller, and an owned caller.
    #[test]
    fn an_overlap_without_a_windowed_caller_is_refused_and_changes_nothing() {
        let callee = cached_probe("m", "(I)V", 1, 2);
        let mut empty = FrameStack::new();
        assert!(empty
            .push_cached_compact_overlapping(Arc::clone(&callee), &[])
            .is_err());
        assert_eq!(empty.len(), 0);

        let mut frames = FrameStack::new();
        frames.push(test_frame("owned", 2));
        let before = frames.slab.mark();
        assert!(call_overlapped(&mut frames, &callee, &[Value::Int(1)], b"I").is_err());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames.slab.mark(), before);
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

/// Interpreter round i1 wave 19, lane L3: the redefinition stamp and the move
/// onto a translated body (`interpreter::obsolete_frames`).
#[cfg(test)]
mod redefine_stamp_tests {
    use super::*;

    /// A moved frame keeps its pc, locals and names, runs the new bytes,
    /// hands back the old ones, drops its cached method and slot, and
    /// carries the stamp and the obsolete mark -- through a continuation too.
    #[test]
    fn adopting_a_redefined_body_keeps_the_frame_state_and_marks_it() {
        let mut frame = Frame::new(
            ClassId::new(3),
            "T".to_string(),
            "m".to_string(),
            "(I)V".to_string(),
            None,
            vec![0x12, 4, 0x57, 0xb1], // ldc #4; pop; return
            vec![],
            4,
            2,
            &[Value::Int(41)],
        );
        frame.pc = 2;
        frame.set_method_index(Some(1));
        let built = frame.redefine_stamp();
        assert!(!frame.runs_obsolete_method());

        let old = frame.adopt_redefined_body(
            padded_bytecode(&[0x12, 9, 0x57, 0xb1]),
            Arc::from(Vec::<ExceptionTableEntry>::new()),
            built + 5,
            true,
            None,
        );
        assert_eq!(&old[..4], &[0x12, 4, 0x57, 0xb1]);
        assert_eq!(frame.code[1], 9);
        assert_eq!(frame.pc, 2);
        assert_eq!(frame.get_local(0), Value::Int(41));
        assert_eq!(frame.method_name(), "m");
        assert_eq!(frame.method_descriptor(), "(I)V");
        assert!(frame.cached_method().is_none());
        assert_eq!(frame.method_index(), None);
        assert_eq!(frame.redefine_stamp(), built + 5);
        assert!(frame.runs_obsolete_method());

        let thawed = Frame::from_frozen_frame(frame.to_frozen_frame());
        assert_eq!(thawed.redefine_stamp(), built + 5);
        assert!(
            thawed.runs_obsolete_method(),
            "the mark survives a continuation"
        );

        let mut emcp = Frame::new(
            ClassId::new(3),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            1,
            0,
            &[],
        );
        let _ = emcp.adopt_redefined_body(
            padded_bytecode(&[0xb1]),
            Arc::from(Vec::<ExceptionTableEntry>::new()),
            built + 5,
            false,
            None,
        );
        assert!(!emcp.runs_obsolete_method());
        assert_eq!(emcp.redefine_stamp(), built + 5);
        assert!(emcp.replaced_body().is_none());

        // Wave 39, lane L3: the "predates its class's redefinition" mark is
        // outside the stamp, survives later moves and a continuation, and is
        // never set by a move itself.
        assert!(!emcp.predates_its_class_redefinition());
        emcp.mark_predates_its_class_redefinition();
        assert!(emcp.predates_its_class_redefinition());
        assert!(!emcp.runs_obsolete_method());
        assert_eq!(emcp.redefine_stamp(), built + 5);
        emcp.restamp_redefined_body(built + 7, true, None);
        assert!(emcp.predates_its_class_redefinition());
        assert!(emcp.runs_obsolete_method());
        assert_eq!(emcp.redefine_stamp(), built + 7);
        let _ = emcp.adopt_redefined_body(
            padded_bytecode(&[0xb1]),
            Arc::from(Vec::<ExceptionTableEntry>::new()),
            built + 9,
            false,
            None,
        );
        assert!(emcp.predates_its_class_redefinition());
        assert!(!emcp.runs_obsolete_method());
        assert_eq!(emcp.redefine_stamp(), built + 9);
        let thawed = Frame::from_frozen_frame(emcp.to_frozen_frame());
        assert!(thawed.predates_its_class_redefinition());
        assert_eq!(thawed.redefine_stamp(), built + 9);
        assert!(!frame.predates_its_class_redefinition());
    }

    /// Interpreter round i1 wave 21, lane L4: a moved frame carries its
    /// body's own line table and widened `ldc` stream, reports its lines
    /// from them, and keeps both through a continuation; an empty record is
    /// not kept at all.
    #[test]
    fn a_replaced_body_travels_with_the_frame() {
        use cratonvm_reader::attribute::LineNumberEntry;
        let new_frame = || {
            Frame::new(
                ClassId::new(3),
                "T".to_string(),
                "m".to_string(),
                "()V".to_string(),
                None,
                vec![0x12, 4, 0x57, 0xb1], // ldc #4; pop; return
                vec![],
                1,
                0,
                &[],
            )
        };
        let code = padded_bytecode(&[0x12, 4, 0x57, 0xb1]);
        let stream = cratonvm_reader::QuickenedCode::with_widened_ldc(&code, &[(0, 300)]);
        assert!(stream.is_some(), "a whole body with an ldc at pc 0");
        let replaced = Arc::new(ReplacedBody {
            line_numbers: Some(Arc::from(vec![
                LineNumberEntry {
                    start_pc: 0,
                    line_number: 40,
                },
                LineNumberEntry {
                    start_pc: 2,
                    line_number: 41,
                },
            ])),
            widened_ldc: vec![(0, 300)].into_boxed_slice(),
            widened_stream: stream,
            retired_code: Box::new([]),
            code_reached_top: std::sync::atomic::AtomicBool::new(false),
            fresh_code: false,
            decoded: std::sync::OnceLock::new(),
        });
        let mut frame = new_frame();
        assert_eq!(frame.own_line_number(0), None, "not moved");
        let stamp = frame.redefine_stamp() + 1;
        let _ = frame.adopt_redefined_body(
            Arc::clone(&code),
            Arc::from(Vec::<ExceptionTableEntry>::new()),
            stamp,
            true,
            Some(Arc::clone(&replaced)),
        );
        assert_eq!(frame.own_line_number(0), Some(40));
        assert_eq!(frame.own_line_number(3), Some(41));
        assert_eq!(frame.own_line_number(-1), None);
        assert!(frame.widened_stream().is_some());

        let thawed = Frame::from_frozen_frame(frame.to_frozen_frame());
        assert_eq!(thawed.own_line_number(2), Some(41));
        assert!(
            thawed.widened_stream().is_some(),
            "the stream survives a continuation: its operand bytes are stale"
        );

        let mut plain = new_frame();
        let _ = plain.adopt_redefined_body(
            code,
            Arc::from(Vec::<ExceptionTableEntry>::new()),
            stamp,
            true,
            Some(Arc::new(ReplacedBody {
                line_numbers: None,
                widened_ldc: Box::new([]),
                widened_stream: None,
                retired_code: Box::new([]),
                code_reached_top: std::sync::atomic::AtomicBool::new(false),
                fresh_code: false,
                decoded: std::sync::OnceLock::new(),
            })),
        );
        assert!(plain.replaced_body().is_none(), "nothing to carry");
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

/// Interpreter round i1 wave 38, lane L7: a thread's requested stack size
/// raises its frame limit, and only raises it.
#[cfg(test)]
mod i38_l7_requested_frame_limit_tests {
    use super::{
        frame_limit_for_stack_bytes, FrameStack, FRAMES_PER_STACK_MIB, MAX_REQUESTED_FRAME_LIMIT,
    };

    #[test]
    fn a_requested_stack_grants_its_frames_per_mib_up_to_the_cap() {
        assert_eq!(frame_limit_for_stack_bytes(0), 0);
        assert_eq!(frame_limit_for_stack_bytes(-5), 0);
        assert_eq!(frame_limit_for_stack_bytes(1 << 20), FRAMES_PER_STACK_MIB);
        assert_eq!(frame_limit_for_stack_bytes(256 << 20), 256 * FRAMES_PER_STACK_MIB);
        assert_eq!(frame_limit_for_stack_bytes(i64::MAX), MAX_REQUESTED_FRAME_LIMIT);
    }

    #[test]
    fn the_requested_limit_only_raises_the_vms_limit() {
        let mut fs = FrameStack::new();
        assert!(fs.at_frame_limit(0, 0));
        assert!(!fs.at_frame_limit(0, 1));
        fs.set_requested_frame_limit(3);
        assert!(!fs.at_frame_limit(0, 0));
        assert!(!fs.at_frame_limit(2, 1));
        assert!(fs.at_frame_limit(3, 1));
        // A request below the VM's limit leaves the VM's limit in force.
        fs.set_requested_frame_limit(1);
        assert!(!fs.at_frame_limit(1, 2));
        assert!(fs.at_frame_limit(2, 2));
        assert_eq!(fs.requested_frame_limit(), 1);
    }
}

#[cfg(test)]
mod i39_l3_conversion_retry_wall_tests {
    use super::FrameStack;

    /// Interpreter round i1 wave 39, lane L3: past the yields, a deferred
    /// conversion's retries stop at the wall bound whatever the count; the
    /// yields are bounded by the count only; a cleared conversion starts its
    /// clock again.
    #[test]
    fn a_deferred_conversions_retries_are_bounded_in_wall_time() {
        let mut stack = FrameStack::new();
        assert!(stack.count_conversion_retry(), "the first retry");
        assert_eq!(stack.conversion_retries(), 1);
        stack.conversion_retries = FrameStack::CONVERSION_RETRY_YIELDS;
        assert!(stack.count_conversion_retry(), "just past the yields, in time");
        let Some(long_ago) =
            std::time::Instant::now().checked_sub(FrameStack::MAX_CONVERSION_RETRY_WALL * 2)
        else {
            return;
        };
        stack.conversion_retry_began = Some(long_ago);
        assert!(!stack.count_conversion_retry(), "over the wall bound");
        assert!(stack.conversion_retry_elapsed() > FrameStack::MAX_CONVERSION_RETRY_WALL);
        stack.clear_conversion_pending();
        assert_eq!(stack.conversion_retry_elapsed(), std::time::Duration::ZERO);
        stack.conversion_retry_began = Some(long_ago);
        assert!(
            stack.count_conversion_retry(),
            "a yielding retry is bounded by the count only"
        );
    }
}
