// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use std::collections::HashMap;

use crate::error::RuntimeError;
use crate::memory::VmHeap;
use crate::types::{jlong_bits_as_aligned_object_ptr, CompactTag, CompactValue, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Diagnostic instrumentation (K1 stack-tag mismatch hunt).
//
// Compiled only in `debug_assertions` builds, and further gated at runtime by
// CRATONVM_DEBUG_STACK_TAG=1.  Release builds see a pure no-op stub that the
// optimizer strips entirely.  The machinery remains in tree so future stack-
// tag regressions can be diagnosed without re-deriving the harness — reverse
// the `cfg(debug_assertions)` pair below to re-enable for release profiling.
// ---------------------------------------------------------------------------

#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(debug_assertions)]
static DIAG_ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(debug_assertions)]
static DIAG_INIT: std::sync::Once = std::sync::Once::new();

#[cfg(debug_assertions)]
#[inline(always)]
fn stack_diag_enabled() -> bool {
    DIAG_INIT.call_once(|| {
        if cratonvm_types::flags::runtime_var("CRATONVM_DEBUG_STACK_TAG")
            .ok()
            .as_deref()
            == Some("1")
        {
            DIAG_ENABLED.store(true, Ordering::Relaxed);
        }
    });
    DIAG_ENABLED.load(Ordering::Relaxed)
}

#[cfg(debug_assertions)]
thread_local! {
    static STACK_DIAG_CTX: std::cell::Cell<StackDiagCtx> = const { std::cell::Cell::new(StackDiagCtx::empty()) };
}

#[cfg(debug_assertions)]
#[derive(Clone, Copy)]
struct StackDiagCtx {
    pc: u32,
    opcode: u8,
    class_ptr: *const u8,
    class_len: u16,
    method_ptr: *const u8,
    method_len: u16,
}

#[cfg(debug_assertions)]
impl StackDiagCtx {
    const fn empty() -> Self {
        Self {
            pc: 0,
            opcode: 0,
            class_ptr: std::ptr::null(),
            class_len: 0,
            method_ptr: std::ptr::null(),
            method_len: 0,
        }
    }
}

/// Update the per-thread diagnostic context just before each opcode dispatch.
/// Release builds compile to an empty function; debug builds early-return
/// unless `CRATONVM_DEBUG_STACK_TAG=1` is set.
///
/// SAFETY: the pointers stored here reference Arc<str> data owned by the
/// current Frame; the Cell is only read inside the same interpreter tick
/// (while the frame is still alive), so the pointers remain valid.
#[cfg(debug_assertions)]
#[inline(always)]
pub fn update_diag_ctx(class_name: &str, method_name: &str, pc: usize, opcode: u8) {
    if !stack_diag_enabled() {
        return;
    }
    STACK_DIAG_CTX.with(|c| {
        c.set(StackDiagCtx {
            pc: pc as u32,
            opcode,
            class_ptr: class_name.as_ptr(),
            class_len: class_name.len().min(u16::MAX as usize) as u16,
            method_ptr: method_name.as_ptr(),
            method_len: method_name.len().min(u16::MAX as usize) as u16,
        });
    });
}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn update_diag_ctx(_class_name: &str, _method_name: &str, _pc: usize, _opcode: u8) {}

#[cfg(debug_assertions)]
fn log_tag_mismatch(expected: &str, cv: CompactValue, stack_len: usize) {
    if !stack_diag_enabled() {
        return;
    }
    let ctx = STACK_DIAG_CTX.with(|c| c.get());
    let class = if !ctx.class_ptr.is_null() {
        // SAFETY: ctx pointers reference Arc<str> data alive for the current
        // opcode tick. Slice is reconstructed within the same tick.
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                ctx.class_ptr,
                ctx.class_len as usize,
            ))
            .to_string()
        }
    } else {
        String::from("<unknown>")
    };
    let method = if !ctx.method_ptr.is_null() {
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                ctx.method_ptr,
                ctx.method_len as usize,
            ))
            .to_string()
        }
    } else {
        String::from("<unknown>")
    };
    tracing::error!(
        target: "cratonvm_stack_tag_error",
        expected = expected,
        actual = ?cv.tag(),
        raw_bits = format!("{:#018x}", cv.to_bits()).as_str(),
        class = class.as_str(),
        method = method.as_str(),
        pc = ctx.pc,
        opcode = format!("{:#04x}", ctx.opcode).as_str(),
        slot_depth = stack_len,
        "stack-tag mismatch"
    );
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn log_tag_mismatch(_expected: &str, _cv: CompactValue, _stack_len: usize) {}

/// The operand stack for a single method frame.
///
/// T10.6 — the operand stack now stores slots as 8-byte NaN-boxed
/// [`CompactValue`]s, halving the per-slot footprint compared to the 16-byte
/// `Value` enum and shaving one byte per slot vs. the prior SoA
/// `(Vec<u64>, Vec<u8>)` layout.
///
/// The public API preserves the same `Value`-based signatures as the SoA
/// implementation so the 500+ interpreter call sites are unaffected; the
/// conversion `CompactValue::from_value` / `CompactValue::to_value` is
/// `#[inline(always)]` and degrades to a handful of bit-ops per push / pop.
// Per-slot "kind" marks (parallel to `ValueStack::slots`).
//
// The NaN-boxed `CompactValue` cannot self-describe a 64-bit `long` (or
// `double`): both are stored as raw 64 bits and are distinguished only by
// opcode context. When a long's raw bits land in the NaN-tag space (e.g. BC
// safegcd / EC accumulators in `0xFFFC_0000_0000_xxxx`), the slot is
// bit-identical to a tagged `Int`, and `pop_long`'s context-free decode would
// sign-extend it, dropping the high bits and corrupting the value (the
// `bc-ec-mod` / `Mod.modOddInverse` infinite-loop family; see
// `gaps/bc-ec-mod-mododdinverse-investigation.md`).
//
// `kinds[i]` records when slot `i` was pushed by a *genuine* long/double
// producer, letting the typed pops read the bits verbatim. The design is
// **safe by degradation**: a slot left `KIND_UNKNOWN` falls back to the exact
// pre-existing decode (including the i2l/i2d widening crutch for synthetic
// bytecode that leaves a narrower type), so a missed mark is never a
// regression. The only unsafe direction is *over*-marking, which is avoided
// by setting `KIND_LONG`/`KIND_DOUBLE` solely at real long/double push sites.
const KIND_UNKNOWN: u8 = 0;
const KIND_LONG: u8 = 1;
const KIND_DOUBLE: u8 = 2;

/// Cached `CRATONVM_LONGROOT_STRICT` gate (bc math-ec 0x4): make the O1
/// hybrid Long|Double smuggle-rooting branch kind-strict — a slot the kind
/// side-array marks as a genuine primitive long/double is never rooted via
/// the loose `is_heap_addr` path.
#[inline]
fn longroot_strict() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_LONGROOT_STRICT").is_some())
}

/// Cached `CRATONVM_LONGREWRITE_LOOSE` escape hatch: restore the pre-registry
/// behavior of rewriting ANY Long/Double slot on a pointer-map hit (see the
/// mint-provenance gate in `update_object_refs`). Use only to diagnose an
/// unregistered mint path; the loose mode can silently corrupt a primitive
/// long that collides with a moved object's address.
fn longrewrite_loose() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_LONGREWRITE_LOOSE").is_some())
}

/// Cached `CRATONVM_DBG_LONGROOT` gate: log every rooting the O1 hybrid
/// branch performs (value bits + kind mark).
#[inline]
fn longroot_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LONGROOT").is_some())
}

#[derive(Debug)]
pub struct ValueStack {
    slots: Vec<CompactValue>,
    /// Parallel kind marks (see [`KIND_UNKNOWN`]). `kinds[i]` is meaningful for
    /// `i < len`; entries are (re)written by every push, so they cannot go
    /// stale across frame reuse.
    ///
    /// # Why this array cannot be collapsed into `slots` (2026-07-26 audit)
    ///
    /// It looks like redundant SoA: `Frame`'s doc says the parallel
    /// `Vec<u64> + Vec<u8>` pair was collapsed into one NaN-boxed buffer, and
    /// this is the leftover. It is not redundant, and collapsing it would be a
    /// silent-heap-corruption bug. A `CompactValue` is exactly 8 bytes and
    /// NaN-boxes its tag in the high bits, so **an arbitrary 64-bit `long`
    /// cannot carry a tag at all** — its payload already uses all 64 bits.
    /// Concretely:
    ///
    /// * A `long` whose top bits land in the NaN-tag space reads back with the
    ///   wrong `tag()`. `0xfffd_…` words (BouncyCastle `LongArray`
    ///   `lxor`/`lshl`/`lushr` over `long[]`) tag as `SUB_OBJECT`, so a
    ///   context-free GC scan would treat the primitive as a heap reference and
    ///   relocate it. That is exactly the corruption this array exists to stop
    ///   (mirrored for locals by `Frame::local_kinds`).
    /// * `SUB_INT` collisions are worse still: as `pop_long` documents, the
    ///   real `Value::Int(0)` and the `long` `0xFFFC_0000_0000_0000` are
    ///   *bit-identical*. No in-slot encoding can separate them.
    ///
    /// The only in-band alternative is widening the slot past 8 bytes, which
    /// (a) doubles operand-stack and locals memory traffic — a pessimisation,
    /// not an optimisation — and (b) breaks the `Vec<u64> <-> Vec<CompactValue>`
    /// `repr(transparent)` transmute that the frame pool, `into_inner` /
    /// `from_pooled`, `snapshot_raw` / `from_snapshot`, and the GC root
    /// scanners in `memory/gc.rs`, `memory/roots.rs` and
    /// `jit/conservative_roots.rs` all rely on.
    ///
    /// The real fix is out-of-band and out of this file's reach: consume the
    /// verifier's per-pc operand-stack type maps (`classloading/type_maps.rs`,
    /// `MethodTypeMaps`) so the *kind of every slot at every pc* is a static
    /// fact and no per-slot runtime tag is needed at all. Until the interpreter
    /// consumes those maps, this array stays.
    ///
    /// The per-frame *allocation* cost this array used to imply is separately
    /// addressed: every `Frame` constructor now sources both halves from a
    /// buffer pool (`Frame::new_pooled*` from the thread pool, `Frame::new` /
    /// `Frame::new_from_arcs` from the per-OS-thread pool in `runtime::frame`),
    /// so the steady state does no allocation and no zero-fill for either Vec.
    kinds: Vec<u8>,
    len: usize,
    max_size: usize,
}

impl ValueStack {
    pub fn new(max_size: usize) -> Self {
        let slots = vec![CompactValue::zero(); max_size];
        Self {
            slots,
            kinds: vec![KIND_UNKNOWN; max_size],
            len: 0,
            max_size,
        }
    }

    /// Decode slot `idx` to a `Value`, honoring its kind mark so that a
    /// genuine long/double whose raw bits collide with the NaN-tag space is
    /// not mis-decoded. Unmarked slots use the exact legacy `to_value()` path.
    #[inline(always)]
    fn value_at(&self, idx: usize) -> Value {
        match self.kinds[idx] {
            KIND_LONG => Value::Long(self.slots[idx].as_long_unchecked()),
            KIND_DOUBLE => Value::Double(f64::from_bits(self.slots[idx].to_bits())),
            _ => self.slots[idx].to_value(),
        }
    }

    /// Kind mark to record for a `Value` about to be stored.
    #[inline(always)]
    fn kind_of_value(v: &Value) -> u8 {
        match v {
            Value::Long(_) => KIND_LONG,
            Value::Double(_) => KIND_DOUBLE,
            _ => KIND_UNKNOWN,
        }
    }

    /// Create a ValueStack reusing pooled Vecs (clears and resizes them).
    /// Ensures at least `max_size` capacity for unsafe push operations.
    ///
    /// The incoming `vals` is treated as a pool of raw u64 slots; the
    /// `tags` vec is ignored (tags are encoded inline via NaN-boxing) — the
    /// signature is preserved so existing pool callers stay unchanged.
    pub fn from_pooled(vals: Vec<u64>, tags: Vec<u8>, max_size: usize) -> Self {
        // CompactValue is repr(transparent) over u64 — Vec<u64> can be
        // transmuted to Vec<CompactValue> without reallocation.
        let mut slots = u64_vec_to_compact(vals);
        // A pooled slot buffer keeps whatever the previous frame left in it,
        // and that is fine: every reader of `slots` — the pops and peeks, the
        // GC scans (`scan_object_refs*`), the pointer rewrite
        // (`update_object_refs`), freeze/thaw — stops at `len`, and a slot
        // below `len` is always written by a push first. So a buffer that is
        // already long enough is handed over as it is instead of being
        // cleared and zero-filled on every call (`max_stack + 24` words,
        // ~300 bytes of memset per invoke); only a buffer that is too short
        // grows, and the grown tail is zero-filled as before (2026-09-02).
        if slots.len() < max_size {
            slots.resize(max_size, CompactValue::zero());
        } else {
            slots.truncate(max_size);
        }
        // Reuse the pooled tag Vec as the `kinds` array. It MUST be cleared:
        // stale marks from a prior frame would over-mark fresh slots (the one
        // unsafe direction), so reset every entry to KIND_UNKNOWN.
        let mut kinds = tags;
        kinds.clear();
        kinds.resize(max_size, KIND_UNKNOWN);
        Self {
            slots,
            kinds,
            len: 0,
            max_size,
        }
    }

    /// Grow the stack's capacity to at least `new_max` slots, returning `true`
    /// if it actually grew.
    ///
    /// Used by tail-call frame reuse ([`Frame::reset_for_tail_call`]): the
    /// reused frame's stack was sized for the PREVIOUS method's `max_stack`,
    /// but the tail-called method may declare a LARGER `max_stack`. Without
    /// this, the new method's pushes overflow the smaller backing Vecs — the
    /// `push_compact` "index out of bounds: len == max_size" panic observed
    /// in WildFly's `RegularEnumSet$EnumSetIterator` path (Long → Integer
    /// `numberOfTrailingZeros` tail call). Only ever GROWS (a no-op when
    /// already large enough), so it can never shrink a live stack or drop
    /// in-use slots; newly added slots are zero / `KIND_UNKNOWN`.
    pub fn ensure_max_size(&mut self, new_max: usize) -> bool {
        if new_max > self.max_size {
            self.slots.resize(new_max, CompactValue::zero());
            self.kinds.resize(new_max, KIND_UNKNOWN);
            self.max_size = new_max;
            true
        } else {
            false
        }
    }

    /// Consume this stack and return the inner Vecs for pooling.
    ///
    /// The tag half is returned as an empty Vec (CompactValue encodes its
    /// type tag inline); callers that pool both halves will simply see a
    /// fresh empty allocation for the tag slot.
    pub fn into_inner(self) -> (Vec<u64>, Vec<u8>) {
        // FIX: clear the `kinds` array before handing it back. The documented
        // contract (and the `from_pooled` round-trip) is that the tag half is
        // returned *empty* — CompactValue encodes its type tag inline, so the
        // pool only needs the allocation, not the stale marks. Returning the
        // marks un-cleared (the previous behavior) leaked a non-empty tag vec
        // out of `into_inner`, contradicting the doc contract and the pooling
        // invariant relied on by `into_inner_preserves_capacity`.
        //
        // `Vec::clear` preserves capacity, so the pool still recycles the
        // allocation (`from_pooled` clears+resizes it back to KIND_UNKNOWN);
        // we get the empty contract *and* zero reallocation.
        let mut kinds = self.kinds;
        kinds.clear();
        (compact_vec_to_u64(self.slots), kinds)
    }

    /// [`Self::into_inner`] without consuming the stack — swap both buffers
    /// out by header and leave an empty stack behind.
    ///
    /// Exists so a frame can be recycled where it lies instead of being moved
    /// out of its `FrameStack` slot first; see
    /// [`crate::runtime::frame::Frame::take_pool_parts_in_place`] for the
    /// measurement that motivated it. Same contract as `into_inner`, including
    /// the empty (capacity-retaining) tag half.
    ///
    /// `len` is reset with the buffers: a husk whose `slots` is empty but whose
    /// `len` still claims depth would report a stack that is not there, and the
    /// husk is observable until the enclosing frame is dropped (a GC root scan
    /// can walk the stack in between).
    pub fn take_inner_in_place(&mut self) -> (Vec<u64>, Vec<u8>) {
        let mut kinds = std::mem::take(&mut self.kinds);
        kinds.clear();
        let slots = std::mem::take(&mut self.slots);
        self.len = 0;
        (compact_vec_to_u64(slots), kinds)
    }

    /// `CRATONVM_DBG_VACATED_FRAMES`: catch a stale reference as it is PUSHED.
    ///
    /// `Frame::set_local`'s twin, and the one that matters for the residual the
    /// H2 MVStore-writer page is chasing: a value returned by a method or a
    /// native goes onto the operand stack and is consumed by the very next
    /// `checkcast`, so it never reaches a local and `set_local` reports nothing.
    /// The Rust backtrace is the point — it names the producer while it is
    /// still on the stack.
    #[cold]
    fn report_vacated_push(value: &Value, moved_to: usize) {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 12 {
            return;
        }
        let addr = match value {
            Value::Object(Some(o)) => o.as_ptr() as usize,
            _ => 0,
        };
        tracing::error!(
            target: "cratonvm::gc::guard",
            obj = format!("{addr:#x}"),
            moved_to = format!("{moved_to:#x}"),
            backtrace = %std::backtrace::Backtrace::force_capture(),
            "a STALE reference is being pushed onto the operand stack — the collector moved              this object and nothing has been allocated at the old address since. The              backtrace names the VM code that produced it."
        );
    }

    #[inline(always)]
    fn check_vacated_compact(cv: &CompactValue) {
        if !cratonvm_gc::gc_quiescence::vacated_frames_enabled() {
            return;
        }
        if cv.is_object() {
            if let Some(ptr) = cv.as_object_ptr() {
                cratonvm_gc::gc_quiescence::check_stale_use(ptr as usize, "operand stack push");
                if let Some(moved_to) = cratonvm_gc::gc_quiescence::was_vacated(ptr as usize) {
                    Self::report_vacated_push(&Value::Object(None), moved_to);
                }
            }
        }
    }

    #[inline(always)]
    fn check_vacated_push(value: &Value) {
        if !cratonvm_gc::gc_quiescence::vacated_frames_enabled() {
            return;
        }
        if let Value::Object(Some(o)) = value {
            cratonvm_gc::gc_quiescence::check_stale_use(o.as_ptr() as usize, "operand stack push");
            if let Some(moved_to) = cratonvm_gc::gc_quiescence::was_vacated(o.as_ptr() as usize) {
                Self::report_vacated_push(value, moved_to);
            }
        }
    }

    pub fn push(&mut self, value: Value) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            // B4 (audit `vm-runtime.md`): an operand-stack overflow is a
            // JVM-recoverable condition that MUST surface as a *catchable*
            // `java.lang.StackOverflowError`, not the uncatchable
            // `NotImplemented` internal error (which `exceptions.rs` maps to a
            // `MethodCallFailed::InternalError` and the interpreter explicitly
            // excludes from runtime-error → Java-exception conversion). Return
            // the dedicated `StackOverflowError` variant, which the interpreter
            // already routes to a real `java/lang/StackOverflowError` throwable
            // (`exceptions.rs` map + `interpreter.rs` runtime-error routing).
            return Err(RuntimeError::StackOverflowError);
        }
        Self::check_vacated_push(&value);
        self.kinds[self.len] = Self::kind_of_value(&value);
        // The line above marks a `Value::Double` slot `KIND_DOUBLE`, which is
        // exactly the out-of-band mark `from_value_kinded` requires in order to
        // keep a NaN payload that collides with the tag space.
        self.slots[self.len] = CompactValue::from_value_kinded(value);
        self.len += 1;
        Ok(())
    }

    /// Push without error wrapping. Used by the fast-path interpreter
    /// for verified bytecode where stack overflow is impossible.
    ///
    /// # Invariant
    /// The caller MUST guarantee `self.len < self.max_size` before calling.
    /// This is satisfied by the classfile verifier, which proves every
    /// method's operand stack never exceeds its declared `max_stack`; the
    /// fast-path interpreter relies on that proof and skips the bounds check
    /// for speed. The invariant is only `debug_assert!`-checked here, so in a
    /// release build a verifier gap (e.g. running with `skip_verification`)
    /// turns the would-be controlled panic into a *silent* slot overwrite at
    /// `self.len == max_size`. Memory safety is still upheld — the slot index
    /// is Rust-bounds-checked against the backing vec, so an out-of-range
    /// `len` produces a controlled index panic, never UB.
    ///
    /// Slow / deopt / unverified callers that cannot uphold the invariant
    /// must use the checked sibling [`Self::push_checked`] (or [`Self::push`]),
    /// which returns an `Err` on overflow instead of relying on the verifier.
    ///
    /// # Panics
    /// Panics if the stack is full. This is a safety net — verified bytecode
    /// should never trigger this.
    #[inline(always)]
    pub fn push_unchecked(&mut self, value: Value) {
        debug_assert!(self.len < self.max_size, "stack overflow in push_unchecked");
        Self::check_vacated_push(&value);
        self.kinds[self.len] = Self::kind_of_value(&value);
        // The line above marks a `Value::Double` slot `KIND_DOUBLE`, which is
        // exactly the out-of-band mark `from_value_kinded` requires in order to
        // keep a NaN payload that collides with the tag space.
        self.slots[self.len] = CompactValue::from_value_kinded(value);
        self.len += 1;
    }

    /// B12: checked sibling of [`Self::push_unchecked`] for callers that
    /// can't rely on a verifier guarantee (slow-path / JIT-deopt entry).
    /// Returns `Err(IllegalStateException)` on overflow rather than panicking.
    #[inline(always)]
    pub fn push_checked(&mut self, value: Value) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack overflow".to_string(),
            });
        }
        Self::check_vacated_push(&value);
        self.kinds[self.len] = Self::kind_of_value(&value);
        // The line above marks a `Value::Double` slot `KIND_DOUBLE`, which is
        // exactly the out-of-band mark `from_value_kinded` requires in order to
        // keep a NaN payload that collides with the tag space.
        self.slots[self.len] = CompactValue::from_value_kinded(value);
        self.len += 1;
        Ok(())
    }

    /// Push a pre-encoded `CompactValue` directly — zero-cost over writing
    /// `slots[n] = cv`. Used internally where the compact form is already
    /// in hand (e.g. `dup`, `swap`, local reload).
    ///
    /// # Invariant
    /// The caller MUST guarantee `self.len < self.max_size` before calling.
    /// As with [`Self::push_unchecked`], this is enforced by the classfile
    /// verifier's `max_stack` proof and only `debug_assert!`-checked here for
    /// speed; a verifier gap under `skip_verification` degrades to a Rust
    /// bounds-checked index panic (controlled, never UB) rather than a clean
    /// `Err`. Slow / deopt / unverified callers must use the checked sibling
    /// [`Self::push_compact_checked`] instead.
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_compact(&mut self, cv: CompactValue) {
        debug_assert!(self.len < self.max_size, "stack overflow in push_compact");
        // Same check as the `Value` pushes, decoded from the compact form. This
        // is the path `dup`, a local reload and the cached field/return
        // producers take, so leaving it out would blind the instrument to
        // exactly the values that reach a `checkcast` without touching a local.
        Self::check_vacated_compact(&cv);
        // Raw compact push: the bits alone cannot distinguish a collision-long
        // from a tagged value, so mark UNKNOWN (safe fallback). Genuine long/
        // double producers call push_long/push_double instead.
        self.kinds[self.len] = KIND_UNKNOWN;
        self.slots[self.len] = cv;
        self.len += 1;
    }

    /// Push a raw `CompactValue` bit-exact AND mark the slot `KIND_LONG`.
    ///
    /// Used by `lload`/`lload_<n>` to forward a long from a local without the
    /// lossy `to_value()`/`from_value()` round-trip, while still recording the
    /// long kind so the eventual `pop_long` reads the bits verbatim instead of
    /// sign-extending a NaN-tag-colliding `0xFFFC_…` value.
    #[inline(always)]
    pub fn push_compact_long(&mut self, cv: CompactValue) {
        debug_assert!(
            self.len < self.max_size,
            "stack overflow in push_compact_long"
        );
        self.kinds[self.len] = KIND_LONG;
        self.slots[self.len] = cv;
        self.len += 1;
    }

    /// Push a raw `CompactValue` bit-exact AND mark the slot `KIND_DOUBLE`.
    /// Double sibling of [`Self::push_compact_long`] (used by `dload`).
    #[inline(always)]
    pub fn push_compact_double(&mut self, cv: CompactValue) {
        debug_assert!(
            self.len < self.max_size,
            "stack overflow in push_compact_double"
        );
        self.kinds[self.len] = KIND_DOUBLE;
        self.slots[self.len] = cv;
        self.len += 1;
    }

    /// Checked, `KIND_LONG`-marking compact push for the slow-path `Lload`
    /// (which otherwise uses the unmarked `push_compact_checked`).
    #[inline(always)]
    pub fn push_compact_long_checked(&mut self, cv: CompactValue) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack overflow".to_string(),
            });
        }
        self.kinds[self.len] = KIND_LONG;
        self.slots[self.len] = cv;
        self.len += 1;
        Ok(())
    }

    /// Checked, `KIND_DOUBLE`-marking compact push for the slow-path `Dload`.
    #[inline(always)]
    pub fn push_compact_double_checked(&mut self, cv: CompactValue) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack overflow".to_string(),
            });
        }
        self.kinds[self.len] = KIND_DOUBLE;
        self.slots[self.len] = cv;
        self.len += 1;
        Ok(())
    }

    /// B12: checked sibling of [`Self::push_compact`]. Returns
    /// `Err(IllegalStateException)` on overflow instead of debug-aborting.
    #[inline(always)]
    pub fn push_compact_checked(&mut self, cv: CompactValue) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack overflow".to_string(),
            });
        }
        // Raw compact push: the bits alone cannot distinguish a collision-long
        // from a tagged value, so mark UNKNOWN (safe fallback). Genuine long/
        // double producers call push_long/push_double instead.
        self.kinds[self.len] = KIND_UNKNOWN;
        self.slots[self.len] = cv;
        self.len += 1;
        Ok(())
    }

    // ── Kind-preserving shuffle primitives (dup*/swap/pop2) ───────────────
    //
    // `pop_compact_checked` + `push_compact_checked` LOSE the kind mark (the
    // re-push always lands `KIND_UNKNOWN`). For an int/ref/float that is
    // harmless. But a collision-shaped `long` (SUB_OBJECT bit pattern, e.g.
    // BouncyCastle F2m `LongArray` `0xfffd_…`) shuffled through `dup2`/
    // `dup_x2`/`swap` would then sit `KIND_UNKNOWN` with reference-looking
    // bits — and the GC root scan would mistake it for a live pointer,
    // relocate the object it aliases, and corrupt the long. These primitives
    // carry the exact kind byte across the shuffle so a long stays a long.

    /// Pop the top slot returning its `CompactValue` AND kind byte.
    #[inline]
    pub fn pop_with_kind(&mut self) -> Result<(CompactValue, u8), RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack underflow".to_string(),
            });
        }
        self.len -= 1;
        Ok((self.slots[self.len], self.kinds[self.len]))
    }

    /// Peek the top slot's `CompactValue` + kind byte (non-popping).
    #[inline]
    pub fn peek_with_kind(&self) -> Result<(CompactValue, u8), RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack underflow".to_string(),
            });
        }
        Ok((self.slots[self.len - 1], self.kinds[self.len - 1]))
    }

    /// Push a `CompactValue` with an explicit kind byte (inverse of
    /// [`pop_with_kind`]).
    #[inline]
    pub fn push_with_kind(&mut self, cv: CompactValue, kind: u8) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack overflow".to_string(),
            });
        }
        self.slots[self.len] = cv;
        self.kinds[self.len] = kind;
        self.len += 1;
        Ok(())
    }

    /// Verifier-trusted sibling of [`Self::pop_with_kind`] for the raw-bytecode
    /// fast path (`pop2`, `dup_x1`, `dup2`, `dup_x2`, `dup2_x1`, `dup2_x2`).
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_with_kind_unchecked(&mut self) -> (CompactValue, u8) {
        debug_assert!(self.len > 0, "stack underflow in pop_with_kind_unchecked");
        self.len -= 1;
        (self.slots[self.len], self.kinds[self.len])
    }

    /// Verifier-trusted sibling of [`Self::peek_with_kind`].
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn peek_with_kind_unchecked(&self) -> (CompactValue, u8) {
        debug_assert!(self.len > 0, "stack underflow in peek_with_kind_unchecked");
        (self.slots[self.len - 1], self.kinds[self.len - 1])
    }

    /// Verifier-trusted sibling of [`Self::push_with_kind`].
    ///
    /// Copies the slot bits AND the kind mark verbatim, which is what makes the
    /// shuffle opcodes bit-exact: a `Value` round-trip would re-encode a
    /// NaN-payload double through [`CompactValue::double`] and lose the payload,
    /// and would drop the `KIND_LONG` mark that tells the GC a pointer-shaped
    /// long slot is a primitive.
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_with_kind_unchecked(&mut self, cv: CompactValue, kind: u8) {
        debug_assert!(
            self.len < self.max_size,
            "stack overflow in push_with_kind_unchecked"
        );
        self.slots[self.len] = cv;
        self.kinds[self.len] = kind;
        self.len += 1;
    }

    /// Category-2 (long/double) test that honors the slot's kind mark. A
    /// collision-shaped long is `CompactValue::is_category2() == false` by
    /// bits (its NaN-box sub-tag reads as `SUB_OBJECT`), but it IS a genuine
    /// category-2 long per its `KIND_LONG` mark — so `dup2`/`pop2`/`dup_x2`
    /// must treat it as occupying one logical slot pair. Unmarked slots fall
    /// back to the bit-level test (an unmarked numeric long/double is still
    /// untagged → `is_category2() == true`).
    #[inline]
    pub fn is_cat2_kind(kind: u8, cv: CompactValue) -> bool {
        matches!(kind, KIND_LONG | KIND_DOUBLE) || cv.is_category2()
    }

    /// The `kinds` mark a genuine `long` producer writes.
    ///
    /// Published so the invoke-argument and field-store paths outside this
    /// module can ask "did the push prove this slot is a category-2
    /// primitive?" instead of re-deriving the answer from bits that, for a
    /// verbatim `long` or `double`, carry no tag at all.
    pub const KIND_MARK_LONG: u8 = KIND_LONG;

    /// The `kinds` mark a genuine `double` producer writes. See
    /// [`KIND_MARK_LONG`](Self::KIND_MARK_LONG) and
    /// `CompactValue::double_raw`.
    pub const KIND_MARK_DOUBLE: u8 = KIND_DOUBLE;

    /// True when the top slot was pushed by a genuine `double` producer.
    ///
    /// `false` on an empty stack, so a caller can use it as a guard
    /// immediately before the pop it is deciding about.
    #[inline(always)]
    pub fn peek_kind_is_double(&self) -> bool {
        self.len > 0 && self.kinds[self.len - 1] == KIND_DOUBLE
    }

    /// Push an int directly as a CompactValue (T10.9.D hot-path).
    ///
    /// Returns `Err` on overflow so the signature mirrors `push(Value)`.
    #[inline(always)]
    pub fn push_int(&mut self, v: i32) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            // B4 (audit `vm-runtime.md`): an operand-stack overflow is a
            // JVM-recoverable condition that MUST surface as a *catchable*
            // `java.lang.StackOverflowError`, not the uncatchable
            // `NotImplemented` internal error (which `exceptions.rs` maps to a
            // `MethodCallFailed::InternalError` and the interpreter explicitly
            // excludes from runtime-error → Java-exception conversion). Return
            // the dedicated `StackOverflowError` variant, which the interpreter
            // already routes to a real `java/lang/StackOverflowError` throwable
            // (`exceptions.rs` map + `interpreter.rs` runtime-error routing).
            return Err(RuntimeError::StackOverflowError);
        }
        self.kinds[self.len] = KIND_UNKNOWN;
        self.slots[self.len] = CompactValue::int(v);
        self.len += 1;
        Ok(())
    }

    /// Push a long directly as a CompactValue (T10.9.D hot-path).
    #[inline(always)]
    pub fn push_long(&mut self, v: i64) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            // B4 (audit `vm-runtime.md`): an operand-stack overflow is a
            // JVM-recoverable condition that MUST surface as a *catchable*
            // `java.lang.StackOverflowError`, not the uncatchable
            // `NotImplemented` internal error (which `exceptions.rs` maps to a
            // `MethodCallFailed::InternalError` and the interpreter explicitly
            // excludes from runtime-error → Java-exception conversion). Return
            // the dedicated `StackOverflowError` variant, which the interpreter
            // already routes to a real `java/lang/StackOverflowError` throwable
            // (`exceptions.rs` map + `interpreter.rs` runtime-error routing).
            return Err(RuntimeError::StackOverflowError);
        }
        self.kinds[self.len] = KIND_LONG;
        self.slots[self.len] = CompactValue::long(v);
        self.len += 1;
        Ok(())
    }

    /// Push a float directly as a CompactValue (T10.9.D hot-path).
    #[inline(always)]
    pub fn push_float(&mut self, v: f32) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            // B4 (audit `vm-runtime.md`): an operand-stack overflow is a
            // JVM-recoverable condition that MUST surface as a *catchable*
            // `java.lang.StackOverflowError`, not the uncatchable
            // `NotImplemented` internal error (which `exceptions.rs` maps to a
            // `MethodCallFailed::InternalError` and the interpreter explicitly
            // excludes from runtime-error → Java-exception conversion). Return
            // the dedicated `StackOverflowError` variant, which the interpreter
            // already routes to a real `java/lang/StackOverflowError` throwable
            // (`exceptions.rs` map + `interpreter.rs` runtime-error routing).
            return Err(RuntimeError::StackOverflowError);
        }
        self.kinds[self.len] = KIND_UNKNOWN;
        self.slots[self.len] = CompactValue::float(v);
        self.len += 1;
        Ok(())
    }

    /// Push a double directly as a CompactValue (T10.9.D hot-path).
    #[inline(always)]
    pub fn push_double(&mut self, v: f64) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            // B4 (audit `vm-runtime.md`): an operand-stack overflow is a
            // JVM-recoverable condition that MUST surface as a *catchable*
            // `java.lang.StackOverflowError`, not the uncatchable
            // `NotImplemented` internal error (which `exceptions.rs` maps to a
            // `MethodCallFailed::InternalError` and the interpreter explicitly
            // excludes from runtime-error → Java-exception conversion). Return
            // the dedicated `StackOverflowError` variant, which the interpreter
            // already routes to a real `java/lang/StackOverflowError` throwable
            // (`exceptions.rs` map + `interpreter.rs` runtime-error routing).
            return Err(RuntimeError::StackOverflowError);
        }
        self.kinds[self.len] = KIND_DOUBLE;
        self.slots[self.len] = CompactValue::double_raw(v);
        self.len += 1;
        Ok(())
    }

    /// Push `null` directly as a CompactValue (T10.9.D hot-path).
    #[inline(always)]
    pub fn push_null(&mut self) -> Result<(), RuntimeError> {
        if self.len >= self.max_size {
            // B4 (audit `vm-runtime.md`): an operand-stack overflow is a
            // JVM-recoverable condition that MUST surface as a *catchable*
            // `java.lang.StackOverflowError`, not the uncatchable
            // `NotImplemented` internal error (which `exceptions.rs` maps to a
            // `MethodCallFailed::InternalError` and the interpreter explicitly
            // excludes from runtime-error → Java-exception conversion). Return
            // the dedicated `StackOverflowError` variant, which the interpreter
            // already routes to a real `java/lang/StackOverflowError` throwable
            // (`exceptions.rs` map + `interpreter.rs` runtime-error routing).
            return Err(RuntimeError::StackOverflowError);
        }
        self.kinds[self.len] = KIND_UNKNOWN;
        self.slots[self.len] = CompactValue::null();
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Result<Value, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack underflow".to_string(),
            });
        }
        self.len -= 1;
        Ok(self.value_at(self.len))
    }

    /// Pop without error wrapping. Used by the fast-path interpreter
    /// for verified bytecode where stack underflow is impossible.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_unchecked(&mut self) -> Value {
        debug_assert!(self.len > 0, "stack underflow in pop_unchecked");
        self.len -= 1;
        self.value_at(self.len)
    }

    /// B12: checked sibling of [`Self::pop_unchecked`]. Returns
    /// `Err(IllegalStateException)` on underflow.
    #[inline(always)]
    pub fn pop_checked(&mut self) -> Result<Value, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack underflow".to_string(),
            });
        }
        self.len -= 1;
        Ok(self.value_at(self.len))
    }

    /// Pop a raw `CompactValue` slot without decoding to `Value`.
    /// Used by `dup`/`swap`/`dup2`-style opcodes that simply copy bits.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_compact(&mut self) -> CompactValue {
        debug_assert!(self.len > 0, "stack underflow in pop_compact");
        self.len -= 1;
        self.slots[self.len]
    }

    /// B12: checked sibling of [`Self::pop_compact`]. Returns
    /// `Err(IllegalStateException)` on underflow.
    #[inline(always)]
    pub fn pop_compact_checked(&mut self) -> Result<CompactValue, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack underflow".to_string(),
            });
        }
        self.len -= 1;
        Ok(self.slots[self.len])
    }

    /// Pop one slot and decode it as an `invoke*` argument of `desc_byte`,
    /// honoring the `KIND_LONG` mark.
    ///
    /// A `J` or `D` parameter whose slot was pushed by a genuine category-2
    /// producer is read bit-exact via [`CompactValue::as_long_unchecked`] /
    /// [`CompactValue::to_bits`], so a
    /// collision-shaped long (top bits `0xFFFC_…`, low 32-bit payload) keeps
    /// its high bits instead of being truncated by
    /// [`CompactValue::decode_by_descriptor`]'s i2l-widening fallback. All
    /// other slots fall through to the descriptor-aware decode (which keeps
    /// the legacy widening for synthetic int-where-long).
    #[inline]
    pub fn pop_arg_for_descriptor_checked(&mut self, desc_byte: u8) -> Result<Value, RuntimeError> {
        let (cv, kind) = self.pop_with_kind()?;
        Ok(if matches!(kind, KIND_LONG | KIND_DOUBLE) {
            match desc_byte {
                b'J' => Value::Long(cv.as_long_unchecked()),
                // Both marks mean the same thing here: the slot IS its 64
                // bits. `CompactValue::double_raw` stores a double verbatim
                // exactly as `CompactValue::long` stores a long, so a `D`
                // parameter whose bits collide with the NaN-box tag space
                // must be reinterpreted, not decoded by the sub-tag it
                // happens to match.
                b'D' => Value::Double(f64::from_bits(cv.to_bits())),
                _ => cv.decode_by_descriptor(desc_byte),
            }
        } else {
            cv.decode_by_descriptor(desc_byte)
        })
    }

    // ── AUDIT CRIT-4: int-specialised push/pop (no Value enum round-trip) ──
    //
    // Hot int opcodes (`iadd`, `imul`, `iload_*`, `istore_*`, `if_icmp*` …)
    // previously paid 3× 8-arm `match` per execution: pop → Value::Int, pop
    // → Value::Int, push Value::Int(_). These helpers stay in CompactValue
    // form throughout, bypassing the `to_value` / `from_value` arms.
    //
    // Bytecode verifier guarantees the operand types, so the unchecked
    // helpers do not type-check — the same contract as `pop_unchecked` /
    // `push_unchecked`.

    /// Push an i32 directly as a `CompactValue::int(_)` without going
    /// through `Value::Int(_) → from_value(_)`.
    ///
    /// # Invariant
    /// The caller MUST guarantee `self.len < self.max_size` before calling.
    /// Like the other unchecked pushes ([`Self::push_unchecked`],
    /// [`Self::push_compact`]) this is the classfile verifier's `max_stack`
    /// contract, only `debug_assert!`-checked here for speed; under a
    /// verifier gap (`skip_verification`) the overflow degrades to a Rust
    /// bounds-checked index panic (controlled, never UB) rather than an
    /// `Err`. Slow / deopt / unverified callers that cannot rely on the
    /// verifier must use a checked push such as [`Self::push_checked`]
    /// (there is no int-specialised checked variant — wrap the value as
    /// `Value::Int` for the slow path).
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_int_unchecked(&mut self, v: i32) {
        debug_assert!(
            self.len < self.max_size,
            "stack overflow in push_int_unchecked"
        );
        self.kinds[self.len] = KIND_UNKNOWN;
        self.slots[self.len] = CompactValue::int(v);
        self.len += 1;
    }

    /// Pop an i32 directly from a `CompactValue::int(_)` slot without
    /// going through `to_value()` / `Value::Int(_)`.
    ///
    /// Bytecode verifier guarantees the top-of-stack is an Int slot at
    /// every site that calls this helper.  If the slot is something else
    /// (`Null`, `Uninitialized`) this returns 0 — matching the slow-path
    /// `pop_int` semantics for the K1-family null/uninit coercion.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_int_unchecked(&mut self) -> i32 {
        debug_assert!(self.len > 0, "stack underflow in pop_int_unchecked");
        self.len -= 1;
        // CompactValue exposes `as_int() -> Option<i32>` (None when the slot
        // is not an Int tag).  Verified bytecode is Int-typed at this site,
        // so `unwrap_or(0)` matches the slow-path null/uninit coercion and
        // compiles down to the same single payload mask + cast after
        // inlining.
        self.slots[self.len].as_int().unwrap_or(0)
    }

    /// Push an f32 directly as a `CompactValue::float(_)` (skip `Value` decode).
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_float_unchecked(&mut self, v: f32) {
        debug_assert!(
            self.len < self.max_size,
            "stack overflow in push_float_unchecked"
        );
        self.kinds[self.len] = KIND_UNKNOWN;
        self.slots[self.len] = CompactValue::float(v);
        self.len += 1;
    }

    /// Pop an f32 directly from a `CompactValue::float(_)` slot.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_float_unchecked(&mut self) -> f32 {
        debug_assert!(self.len > 0, "stack underflow in pop_float_unchecked");
        self.len -= 1;
        self.slots[self.len].as_float().unwrap_or(0.0)
    }

    /// Push an f64 directly as a `CompactValue::double(_)` (skip `Value` decode).
    ///
    /// Round-2 VM review: mirrors `push_float_unchecked` for the d-arithmetic
    /// hot path (`dadd`, `dsub`, `dmul`, `ddiv`).
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_double_unchecked(&mut self, v: f64) {
        debug_assert!(
            self.len < self.max_size,
            "stack overflow in push_double_unchecked"
        );
        self.kinds[self.len] = KIND_DOUBLE;
        self.slots[self.len] = CompactValue::double_raw(v);
        self.len += 1;
    }

    /// Pop an f64 directly from a `CompactValue::double(_)` slot.
    ///
    /// Doubles are stored as raw f64 bits in the CompactValue, verbatim: the
    /// `KIND_DOUBLE` mark the matching push writes is what lets
    /// `CompactValue::double_raw` skip the collision canonicalization, so a
    /// NaN payload arrives here intact.  Verified bytecode guarantees this
    /// slot was produced by a Double-producing opcode (dconst, dload, dadd …),
    /// so reading the raw bits is the JVMS-correct decode.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_double_unchecked(&mut self) -> f64 {
        debug_assert!(self.len > 0, "stack underflow in pop_double_unchecked");
        self.len -= 1;
        // CompactValue::double_raw(v) stores `v.to_bits()` verbatim, and there
        // is no separate Double sub-tag — the slot's raw u64 IS the double's
        // bit pattern. Same decode policy as pop_double's KIND_DOUBLE arm.
        f64::from_bits(self.slots[self.len].to_bits())
    }

    /// Push an i64 directly as a `CompactValue::long(_)` (skip `Value` decode).
    ///
    /// Round-2 VM review: mirrors `push_int_unchecked` for long ops where the
    /// caller has the raw i64 in hand.
    ///
    /// # Panics
    /// Panics if the stack is full.
    #[inline(always)]
    pub fn push_long_unchecked(&mut self, v: i64) {
        debug_assert!(
            self.len < self.max_size,
            "stack overflow in push_long_unchecked"
        );
        self.kinds[self.len] = KIND_LONG;
        self.slots[self.len] = CompactValue::long(v);
        self.len += 1;
    }

    /// Pop an i64 directly from the top stack slot.
    ///
    /// Verified bytecode guarantees the slot is a long (either tagged Long or
    /// untagged Double — both store raw i64 bits in the CompactValue
    /// representation). Mirrors `pop_long`'s fast-path arms without the
    /// per-slot tag-match overhead.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn pop_long_unchecked(&mut self) -> i64 {
        debug_assert!(self.len > 0, "stack underflow in pop_long_unchecked");
        self.len -= 1;
        // Both CompactTag::Long (NaN-tag collision) and CompactTag::Double
        // (untagged) store the raw i64 bits directly in self.0 — see
        // `pop_long`'s Long|Double arm.
        self.slots[self.len].as_long_unchecked()
    }

    /// Peek the top-of-stack as an i32 without popping.  Returns 0 for
    /// non-Int slots (matches `pop_int_unchecked`).
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn peek_int_unchecked(&self) -> i32 {
        debug_assert!(self.len > 0, "stack underflow in peek_int_unchecked");
        self.slots[self.len - 1].as_int().unwrap_or(0)
    }

    /// Pop raw u64 value without decoding to Value enum.
    /// Used by JIT dispatch to avoid decode/re-encode overhead.
    #[inline(always)]
    pub fn pop_raw(&mut self) -> u64 {
        debug_assert!(self.len > 0, "stack underflow in pop_raw");
        self.len -= 1;
        self.slots[self.len].to_bits()
    }

    pub fn peek_checked(&self) -> Result<Value, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack underflow".to_string(),
            });
        }
        Ok(self.value_at(self.len - 1))
    }

    /// Peek at the top value. Used by the fast-path interpreter.
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn peek(&self) -> Value {
        debug_assert!(self.len > 0, "stack underflow in peek");
        self.value_at(self.len - 1)
    }

    /// Peek the top slot as a raw `CompactValue` (zero-copy).
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn peek_compact(&self) -> CompactValue {
        debug_assert!(self.len > 0, "stack underflow in peek_compact");
        self.slots[self.len - 1]
    }

    /// Peek the raw slot `depth` below the top (`0` is the top) without
    /// decoding. Used by the quickened `putfield` arm to read the receiver
    /// under the value before committing either pop.
    ///
    /// # Panics
    /// Panics if fewer than `depth + 1` slots are live.
    #[inline(always)]
$1
    /// Peek the slot `depth` below the top together with its kind mark
    /// (`0` is the top). The invoke fast door reads every argument this way
    /// before committing the pop, so a shape it cannot transfer verbatim
    /// leaves the stack exactly as the general dispatcher expects it.
    ///
    /// # Panics
    /// Panics if fewer than `depth + 1` slots are live.
    #[inline(always)]
    pub fn peek_with_kind_at(&self, depth: usize) -> (CompactValue, u8) {
        debug_assert!(self.len > depth, "stack underflow in peek_with_kind_at");
        let i = self.len - 1 - depth;
        (self.slots[i], self.kinds[i])
    }

    /// Drop the top `n` slots without decoding them. The caller has already
    /// copied them out (the invoke fast door moves them into the callee's
    /// locals verbatim).
    ///
    /// # Panics
    /// Panics if fewer than `n` slots are live.
    #[inline(always)]
    pub fn discard_top(&mut self, n: usize) {
        debug_assert!(self.len >= n, "stack underflow in discard_top");
        self.len -= n;
    }

    /// B12: checked sibling of [`Self::peek_compact`]. Returns
    /// `Err(IllegalStateException)` on empty stack.
    #[inline(always)]
    pub fn peek_compact_checked(&self) -> Result<CompactValue, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "operand stack underflow".to_string(),
            });
        }
        Ok(self.slots[self.len - 1])
    }

    /// Peek at a value at `offset` positions from the top (0 = top, 1 = second from top, etc.).
    /// Used by invokevirtual fast path to read the receiver under method arguments.
    ///
    /// # Panics
    /// Panics if `offset_from_top >= self.len`.
    #[inline(always)]
    pub fn peek_at(&self, offset_from_top: usize) -> Value {
        debug_assert!(offset_from_top < self.len, "stack underflow in peek_at");
        self.value_at(self.len - 1 - offset_from_top)
    }

    pub fn pop_int(&mut self) -> Result<i32, RuntimeError> {
        match self.pop()? {
            Value::Int(v) => Ok(v),
            // A Long where an int is expected indicates a verifier/codegen
            // bug.  Silently truncating to i32 masks the real defect, so
            // surface it as an error instead of coercing.
            Value::Long(_) => Err(RuntimeError::NotImplemented {
                feature: "expected int on stack, got Long".to_string(),
            }),
            // Coerce a null reference to zero.  Null is the only safe
            // conversion — an actual object pointer must NOT be treated as
            // an int, since downstream code often branches on bitwise
            // masks that would alias legitimate pointers and subsequently
            // dereference the resulting "int" back as an object.
            Value::Object(None) => Ok(0),
            // Uninitialized slot reads default-zero — primitive fields whose
            // <clinit> hasn't yet run, or local-variable slots before their
            // first write.  Same K1-family fix as Object(None).
            Value::Uninitialized => Ok(0),
            other => Err(RuntimeError::NotImplemented {
                feature: format!("expected int on stack, got {other}"),
            }),
        }
    }

    pub fn pop_long(&mut self) -> Result<i64, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack underflow".to_string(),
            });
        }
        // A `pop_long` call site is a verified long-consumer (lstore / ladd /
        // lcmp / l2i / lreturn / …), so the slot holds a Long. `CompactValue`
        // stores longs verbatim, but a long whose top bits collide with the
        // NaN-tag space reads back with a non-Long `tag()` — most importantly
        // `SUB_INT`. Decode bit-exactly via the same discrimination as
        // `CompactValue::decode_by_descriptor(b'J')`:
        //
        //   * any non-Int tag (incl. untagged, SUB_LONG_*, SUB_OBJECT/NULL/…)
        //     → reinterpret the raw 64 bits as i64;
        //   * `SUB_INT` with payload bits 32-46 set → a real int can never set
        //     those (`CompactValue::int` stores `n as u32`), so the slot is a
        //     long whose bit pattern collides into the int sub-tag (BC safegcd
        //     `Mod.updateDE30` / `updateFG30` 0xFFFC_…/0xFFFE_… accumulators) —
        //     reinterpret bit-exact;
        //   * `SUB_INT` with payload < 2^32 → indistinguishable from a real
        //     `Value::Int(n)`, so apply JVMS i2l sign-extension (preserves the
        //     long-standing "native / synthetic bytecode left an int where a
        //     long is expected" widening contract — see `pop_long_widens_int`).
        //
        // The previous code widened *every* `SUB_INT` slot, silently dropping
        // the high bits of collision longs. The residual payload<2^32 SUB_INT
        // ambiguity is unsalvageable from a single 8-byte slot (a real `Int(0)`
        // and the long `0xFFFC_0000_0000_0000` are bit-identical) and needs a
        // parallel stack type tag to close fully — see
        // gaps/bc-ec-mod-mododdinverse-investigation.md.
        self.len -= 1;
        let cv = self.slots[self.len];
        // Fast, bit-exact path: the slot was pushed by a genuine long producer
        // (push_long / Value::Long). Read the raw 64 bits verbatim — this is
        // what fixes the BC EC / safegcd collision-long corruption, since the
        // tag-based decode below would sign-extend a `0xFFFC_…` long as if it
        // were an Int. Unmarked slots fall through to the legacy decode, which
        // preserves the i2l-widening crutch for synthetic int-where-long.
        if self.kinds[self.len] == KIND_LONG {
            return Ok(cv.as_long_unchecked());
        }
        match cv.decode_by_descriptor(b'J') {
            Value::Long(v) => Ok(v),
            // `decode_by_descriptor(b'J')` is total over `Value::Long`; this
            // arm is unreachable but avoids an `unwrap`/panic on the hot path.
            _ => Ok(cv.as_long_unchecked()),
        }
    }

    pub fn pop_float(&mut self) -> Result<f32, RuntimeError> {
        match self.pop()? {
            Value::Float(v) => Ok(v),
            Value::Int(v) => Ok(v as f32),
            other => Err(RuntimeError::NotImplemented {
                feature: format!("expected float on stack, got {other}"),
            }),
        }
    }

    pub fn pop_double(&mut self) -> Result<f64, RuntimeError> {
        if self.len == 0 {
            return Err(RuntimeError::NotImplemented {
                feature: "operand stack underflow".to_string(),
            });
        }
        // Untagged slots ARE doubles (raw f64 bits).  Handle the common case
        // without going through the full Value decode.
        let idx = self.len - 1;
        let cv = self.slots[idx];
        // Bit-exact fast path for genuine double producers (push_double /
        // Value::Double): read the raw f64 bits verbatim, immune to any
        // NaN-tag collision (mirrors pop_long's KIND_LONG fast path).
        if self.kinds[idx] == KIND_DOUBLE {
            self.len -= 1;
            return Ok(f64::from_bits(cv.to_bits()));
        }
        match cv.tag() {
            // KC26 K1: mirror pop_long's handling — Long-tagged values
            // (SUB_LONG_LO/HI) also store raw bits untagged; they arise
            // when a Long's bit pattern happens to collide with the NaN-tag
            // mask.  For dstore / d-arithmetic paths that coerce Long→Double,
            // reinterpret the bits.
            CompactTag::Double => {
                self.len -= 1;
                // `as_double()` returns None exclusively for NaN-tagged
                // slots, which the `CompactTag::Double` arm has already
                // excluded — the previous `unwrap_or(from_bits(to_bits()))`
                // was always taking the `Some` branch and the fallback was
                // pure dead code hiding the type-safety invariant.
                // For the same reason this `expect` cannot fire at runtime.
                Ok(cv
                    .as_double()
                    .expect("CompactTag::Double slot always decodes via as_double"))
            }
            CompactTag::Long => {
                // A raw i64 value landed here (e.g. via `CompactValue::long`
                // producing a bit-pattern collision). Coerce to double the
                // same way a Value::Long would via the slow-path match.
                self.len -= 1;
                Ok(cv.as_long_unchecked() as f64)
            }
            _ => {
                let v = self.pop()?;
                match v {
                    Value::Double(d) => Ok(d),
                    Value::Float(f) => Ok(f as f64),
                    Value::Int(i) => Ok(i as f64),
                    Value::Long(l) => Ok(l as f64),
                    other => Err(RuntimeError::NotImplemented {
                        feature: format!("expected double on stack, got {other}"),
                    }),
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    // ── GC scanning and pointer update helpers ─────────────────────────

    /// Collect all non-null object references from the stack for GC root scanning.
    ///
    /// Per JVM spec, an operand-stack `Long` slot IS a primitive long, never a heap
    /// reference, so it is intentionally NOT rooted here. Smuggling references through
    /// long slots (the JNI `jobject`-as-`jlong` contract used by some natives / JIT
    /// glue) is handled at those specific sites, not in the generic GC root scan —
    /// treating arbitrary primitive longs as roots can SEGV when their bits happen to
    /// look like an aligned heap pointer (see `applogs/letsgo-segv-L1-diagnosis.md`).
    ///
    /// Scanned slot kinds:
    /// - NaN-boxed object slots (`CompactValue::is_object`).
    /// - Untagged raw slots ([`CompactTag::Double`] in the compact encoding — ambiguous
    ///   JVM long vs double): only values that pass `heap.is_object_address` are rooted,
    ///   so numeric longs and ordinary doubles are not mistaken for references.
    pub fn scan_object_refs(&self, roots: &mut Vec<ObjectRef>, heap: &VmHeap) {
        for i in 0..self.len {
            let cv = self.slots[i];
            if cv.is_object() {
                if let Some(ptr) = cv.as_object_ptr() {
                    // FIX: distinguish a *genuine* object reference from a
                    // long-bit-pattern false positive using the parallel
                    // `kinds` mark instead of unconditionally heap-validating.
                    //
                    // The `kinds` side-array records JVM type context at push
                    // time: a real `Value::Object` push (and the dup/swap/local
                    // reload paths) leaves the slot `KIND_UNKNOWN`, whereas a
                    // primitive long that collides into the `SUB_OBJECT`
                    // sub-tag is only ever produced by a genuine long producer
                    // (`push_long` / `push(Value::Long)` / long return), which
                    // marks the slot `KIND_LONG`. So:
                    //
                    //   * `KIND_LONG` ⇒ primitive long masquerading as an
                    //     object — NEVER root it (this is the letsgo-segv L1
                    //     requirement; treating a primitive long as a live
                    //     pointer is exactly the SEGV trigger). Fall through to
                    //     the loose heap-validated smuggle path below so a JNI
                    //     long-as-jobject still survives iff it points at the
                    //     heap.
                    //   * otherwise ⇒ a reference recorded by JVM type context.
                    //     Root it unconditionally: a genuine object slot is a
                    //     live root even when its payload is a young / mid-init
                    //     address `is_heap_addr` cannot yet vouch for. The prior
                    //     code dropped such roots (use-after-free risk for newly
                    //     allocated objects) — the regression this fixes.
                    if ptr != 0 && self.kinds[i] != KIND_LONG && self.kinds[i] != KIND_DOUBLE {
                        // SAFETY: the slot was stored by `CompactValue::object`
                        // from a valid ObjectRef (genuine reference per the
                        // kind mark); reconstructing the ObjectRef from its
                        // 47-bit payload is the inverse of that encode.
                        roots.push(unsafe { ObjectRef::from_raw(ptr as *mut u8) });
                    }
                    // A `KIND_LONG` / `KIND_DOUBLE` slot whose bits collide
                    // with the SUB_OBJECT pattern (BC F2m `LongArray`
                    // `0xfffd_…`) is a primitive — NEVER root it, even when its
                    // low 47 bits land on a live object. The previous
                    // `else if … is_heap_addr` branch rooted exactly those
                    // collisions and the post-GC update then rewrote them,
                    // corrupting the long. The genuine JNI long-as-jobject
                    // smuggle uses canonical *low* heap addresses (bits 63-50
                    // clear), so it is NOT `is_object()` and is rooted by the
                    // untagged `CompactTag::Long | Double` branch below — this
                    // removal does not affect it.
                }
            } else if matches!(cv.tag(), CompactTag::Long | CompactTag::Double) {
                // O1 hybrid: tagged Long OR untagged Double bits — both
                // could be smuggled jobject refs (JNI long-as-jobject pattern
                // used by WildFly's jboss-modules bootloader). Validate via
                // is_heap_addr (loose arena+alignment check) which doesn't
                // require a parseable object header (some refs are at
                // interior offsets or mid-init objects). The generational
                // collector's MAX_SANE_OBJECT_SIZE guard in forward_object
                // handles bogus roots gracefully — over-retention is the only
                // cost, vs. WildFly SEGV with the strict is_object_address.
                //
                // bc math-ec 0x4 (2026-06-09): this branch is ASYMMETRIC — a
                // rooted Long-tagged slot is NEVER remapped by
                // `update_object_refs` (no Long arm), so a genuine smuggle goes
                // STALE the moment the referent moves; and a genuine PRIMITIVE
                // long (KIND_LONG by push-time context, e.g. a BC F2m word
                // whose value lands in the heap range) gets rooted here and
                // feeds `forward_object` an interior/garbage pointer → fake
                // to-space objects / Cheney desync / unforwarded children.
                // `CRATONVM_LONGROOT_STRICT=1` skips rooting for slots the
                // kind side-array marks as genuine primitives (mirrors the
                // 2026-06-04 kind-strict fix for the SUB_OBJECT branch above);
                // `CRATONVM_DBG_LONGROOT` logs every rooting this branch does.
                if longroot_strict() && (self.kinds[i] == KIND_LONG || self.kinds[i] == KIND_DOUBLE)
                {
                    continue;
                }
                if let Some(p) = jlong_bits_as_aligned_object_ptr(cv.to_bits()) {
                    if let Some(r) = heap.is_heap_addr(p) {
                        if longroot_dbg() {
                            use std::sync::atomic::{AtomicUsize, Ordering};
                            static N: AtomicUsize = AtomicUsize::new(0);
                            let k = N.fetch_add(1, Ordering::Relaxed);
                            if k < 40 {
                                eprintln!(
                                    "[longroot] #{k} rooting tagged-{:?} slot[{i}] bits=0x{:x} kind={} (loose is_heap_addr pass)",
                                    cv.tag(),
                                    cv.to_bits(),
                                    self.kinds[i],
                                );
                            }
                        }
                        roots.push(r);
                    }
                }
            }
        }
    }

    /// Conservative operand-stack scan for the non-moving sweep.
    ///
    /// Mirrors `Frame::scan_locals_conservative`: when JIT quiescence has
    /// already forced the young collector onto its non-moving sweep, a
    /// false-positive root can only over-retain. That lets ForkJoin stress
    /// recover object refs whose stack slot lost its CompactValue object tag
    /// before the safepoint snapshot was published.
    pub fn scan_object_refs_conservative(&self, roots: &mut Vec<ObjectRef>, heap: &VmHeap) {
        for i in 0..self.len {
            let cv = self.slots[i];
            let cands = [
                cv.as_object_ptr().unwrap_or(0),
                cv.as_long().map(|l| l as u64).unwrap_or(0),
                cv.to_bits(),
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

    /// Update object references after GC using the pointer map.
    ///
    /// Symmetric to [`Self::scan_object_refs`]: every slot the scan yields as a
    /// root is rewritten here with its relocated address, and no slot is
    /// rewritten that the scan would not have rooted. Three classes mirror the
    /// scan exactly:
    /// - NaN-boxed object slots (`is_object`) that are not kind-marked
    ///   primitive: remapped via the `pointer_map`.
    /// - Smuggled jobject references in tagged `Long` OR untagged `Double`
    ///   slots (the JNI long-as-jobject pattern): rooted by the scan's O1
    ///   hybrid branch only when they pass `heap.is_heap_addr`, so they are
    ///   remapped here under the same heap-membership check (and skipped under
    ///   the same `CRATONVM_LONGROOT_STRICT` kind-strict gate). The prior code
    ///   had a `Double`-only arm, leaving a `Long`-tagged smuggle
    ///   rooted-but-not-remapped → stale across a moving collection.
    /// - Genuine primitive `Long`/`Double` values: never rooted, never
    ///   rewritten — a bit-pattern that coincidentally matches a moved object's
    ///   from-space address is left untouched so the value is not corrupted.
    /// BUG-03 debug: dump every operand-stack slot's kind + tag + addr.
    #[doc(hidden)]
    pub fn dbg_dump(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        for i in 0..self.len {
            let cv = self.slots[i];
            let _ = write!(
                s,
                " [{}]kind={} is_obj={} obj={:?} raw=0x{:x};",
                i,
                self.kinds[i],
                cv.is_object(),
                cv.as_object_ptr().map(|p| p as usize),
                cv.raw_bits()
            );
        }
        s
    }

    /// stw-residual-close debug: dump the raw bits + kind of up to `extra`
    /// slots ABOVE the current `len` — physically still present in the
    /// backing Vec after a pop (an invoke's just-popped receiver+args). Only
    /// meaningful immediately after the pop that consumed them; used by the
    /// stale-recv / nsme_dispatch capture dumps.
    #[doc(hidden)]
    pub fn dbg_dump_popped(&self, extra: usize) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let hi = (self.len + extra).min(self.max_size);
        for i in self.len..hi {
            let cv = self.slots[i];
            let _ = write!(
                s,
                " [+{}]kind={} is_obj={} obj={:?} raw=0x{:x};",
                i - self.len,
                self.kinds[i],
                cv.is_object(),
                cv.as_object_ptr().map(|p| p as usize),
                cv.raw_bits()
            );
        }
        s
    }

    /// BUG-03 debug: locate `addr` among the operand-stack slots + report kind.
    #[doc(hidden)]
    pub fn dbg_locate_addr(&self, addr: usize) -> Option<String> {
        let mask = 0x7fff_ffff_ffffusize;
        for i in 0..self.len {
            let obj = self.slots[i].as_object_ptr().map(|p| p as usize);
            let raw = self.slots[i].raw_bits() as usize;
            if obj == Some(addr) || (raw & mask) == (addr & mask) {
                return Some(format!(
                    "stack[{}] kind={} is_object={} obj_match={}",
                    i,
                    self.kinds[i],
                    self.slots[i].is_object(),
                    obj == Some(addr)
                ));
            }
        }
        None
    }

    /// cceres3 blocked-window slot write-back: rewrite the object pointer at
    /// `offset_from_top` (same index space as `peek_at`) from `old` to `new`.
    /// Kind-strict: collision Long/Double slots are never touched (mirrors
    /// `update_object_refs`). Returns true when the slot was rewritten.
    pub fn rewrite_object_at(&mut self, offset_from_top: usize, old: usize, new: usize) -> bool {
        if offset_from_top >= self.len {
            return false;
        }
        let i = self.len - 1 - offset_from_top;
        let cv = self.slots[i];
        if !cv.is_object() || self.kinds[i] == KIND_LONG || self.kinds[i] == KIND_DOUBLE {
            return false;
        }
        match cv.as_object_ptr() {
            Some(p) if p as usize == old => {
                // SAFETY: `new` is a live moved object's address from the GC
                // pointer maps (same invariant as `update_object_refs`).
                let _ = unsafe { crate::types::ObjectRef::from_raw(new as *mut u8) };
                self.slots[i].update_object_ptr_unchecked(new as u64);
                true
            }
            _ => false,
        }
    }

    pub fn update_object_refs(&mut self, pointer_map: &cratonvm_types::PointerMap, heap: &VmHeap) {
        for i in 0..self.len {
            let cv = self.slots[i];
            if cv.is_object() {
                // A primitive long/double whose bits collide with SUB_OBJECT is
                // a value, not a pointer — never remap it. The `pointer_map` is
                // the *global* relocation record, so a collision long's payload
                // can match an unrelated moved object's from-space address; the
                // old comment's "a false positive was never rooted so it can't
                // be a key" is incorrect (rooting and remapping key off
                // *different* objects). The rooting scan no longer roots these,
                // and this guard keeps the post-GC rewrite from corrupting them.
                if self.kinds[i] == KIND_LONG || self.kinds[i] == KIND_DOUBLE {
                    continue;
                }
                if let Some(old_ptr) = cv.as_object_ptr() {
                    // Gate on `pointer_map` membership, NOT a live-header /
                    // arena probe of `old_ptr`. Post-GC, `old_ptr` is the
                    // *from-space* address whose memory has already been
                    // zeroed/reclaimed, so `heap.is_heap_addr(old_ptr)` can
                    // reject precisely the slots that were relocated and need
                    // remapping — the operand-stack half of the H2 `TestAll`
                    // POST-GC STALE STACK crash. The `pointer_map` is the
                    // authoritative relocation record (and the exact criterion
                    // `verify_no_stale_refs` checks).
                    if let Some(&new_addr) = pointer_map.get(&(old_ptr as usize)) {
                        // Record construction provenance for the relocated
                        // address (2026-07-03): `update_object_ptr_unchecked`
                        // writes raw bits, so without this a context-free
                        // decode of the rewritten slot can hit a provenance
                        // MISS on `new_addr` and degrade a live reference to
                        // Long/null (observed in the rewrites_pointer test).
                        // The `from_raw` round-trip is the canonical recorder.
                        // SAFETY: `new_addr` is a live moved object's address
                        // from the GC pointer map; only its bits are used.
                        let _ = unsafe { crate::types::ObjectRef::from_raw(new_addr as *mut u8) };
                        // SAFETY: `new_addr` comes from a `HashMap<usize,
                        // usize>` of live-heap pointers populated by the GC
                        // compactor; every entry is the moved address of a
                        // post-compaction object, which the allocator
                        // guarantees fits in the 47-bit address space (same
                        // invariant `CompactValue::object` enforces for
                        // initial construction). The checked
                        // `update_object_ptr` would still succeed here; the
                        // `_unchecked` variant skips the redundant range
                        // check on the GC hot path.
                        self.slots[i].update_object_ptr_unchecked(new_addr as u64);
                    }
                }
            } else if matches!(cv.tag(), CompactTag::Long | CompactTag::Double) {
                // SCAN/UPDATE SYMMETRY (bc math-ec 0x4 follow-up): the
                // `scan_object_refs` O1 hybrid branch roots a smuggled jobject
                // out of BOTH a tagged `Long` and an untagged `Double` slot
                // (JNI long-as-jobject, e.g. WildFly's jboss-modules
                // bootloader). The previous update only had a `Double` arm, so
                // a reference smuggled into a tagged-`Long` slot was
                // rooted-but-never-remapped: it went STALE the instant the
                // referent moved (UAF / wrong object). Mirror the scan's set
                // exactly — same tags, same kind-strict gate, same
                // heap-membership check — so every slot the scan yields as a
                // root is rewritten here, and no other.
                //
                // Kind-strict parity: under `CRATONVM_LONGROOT_STRICT`, the
                // scan skips rooting slots the kind side-array marks as genuine
                // primitive long/double. Such a slot is never in `pointer_map`
                // for the right reason, but skip the rewrite too so a primitive
                // long/double whose bits coincidentally match a moved object's
                // from-space address is never mutated.
                if longroot_strict() && (self.kinds[i] == KIND_LONG || self.kinds[i] == KIND_DOUBLE)
                {
                    continue;
                }
                let bits = cv.to_bits();
                if let Some(old_ptr) = jlong_bits_as_aligned_object_ptr(bits) {
                    if let Some(&new_addr) = pointer_map.get(&old_ptr) {
                        // B10 / Step 5 GAP A: distinguish a legitimate JNI
                        // long-as-jobject smuggle from a coincidental bit-pattern
                        // match — but probe the RELOCATION TARGET (`new_addr`),
                        // not the from-space `old_ptr`. `old_ptr` is post-GC: the
                        // generational collector leaves it resolvable (the
                        // from-space semi-space is repurposed, never freed), but
                        // G1 resets the evacuated CSet region to `Free`, so
                        // `is_heap_addr(old_ptr)` returns None for precisely the
                        // genuinely-moved jobjects that need rewriting — leaving
                        // the smuggle STALE (UAF; e.g. WildFly jboss-modules). The
                        // moved object's new address is always a live region for
                        // both collectors, so gating on `is_heap_addr(new_addr)`
                        // rewrites genuine smuggles under G1 too while still
                        // preserving a coincidental long whose key maps to a
                        // non-heap target. (A primitive long whose bits collide
                        // with a *real* moved object's key is rewritten under both
                        // collectors — a rare pre-existing ambiguity that
                        // CRATONVM_LONGROOT_STRICT resolves precisely.) The object
                        // arm above already gates on pointer_map membership for
                        // the same reason (H2 stale-stack crash).
                        // Mint-provenance corroboration (2026-07-03): a
                        // pointer-map hit alone cannot distinguish a genuine
                        // smuggled handle from a primitive long whose value
                        // coincidentally equals a moved object's from-space
                        // address — the collision target is a real object, so
                        // construction provenance always passes. Only MINT
                        // provenance discriminates: genuine smuggles are
                        // created at a handful of chokepoints (generic-JNI 'J'
                        // returns, SetLongField/SetLongArrayRegion, JVMTI
                        // GetLocal) which register the exact value in
                        // `smuggled_longs`; the registry is remapped through
                        // every pointer map, so check BOTH the old bits (same
                        // pause, pre-remap consumers) and the relocation
                        // target (safepoint peers and blocked-thread wakes
                        // applying composed multi-GC fixups after the
                        // registry was remapped). A colliding primitive is
                        // left untouched. `CRATONVM_LONGREWRITE_LOOSE=1`
                        // restores the old rewrite-on-any-map-hit behavior
                        // (escape hatch for an unregistered mint path; such a
                        // block is logged under CRATONVM_DBG_LONGROOT).
                        // Asked of THIS heap's mint table only: a handle
                        // minted against another VM's heap says nothing
                        // about this slot (see `smuggled_longs`).
                        let minted = crate::memory::smuggled_longs::is_minted(heap, bits)
                            || crate::memory::smuggled_longs::is_minted(heap, new_addr as u64);
                        if !minted && !longrewrite_loose() {
                            if longroot_dbg() {
                                use std::sync::atomic::{AtomicUsize, Ordering};
                                static N: AtomicUsize = AtomicUsize::new(0);
                                let k = N.fetch_add(1, Ordering::Relaxed);
                                if k < 40 {
                                    eprintln!(
                                        "[longroot] #{k} rewrite BLOCKED slot[{i}] bits=0x{bits:x} -> 0x{new_addr:x} (not a minted handle; preserving primitive)",
                                    );
                                }
                            }
                        } else if heap.is_heap_addr(new_addr).is_some() {
                            // Record construction provenance for the moved
                            // address (see the Object arm above): the raw-bits
                            // write below bypasses `from_raw`, and a later
                            // decode of this slot must not provenance-MISS.
                            // SAFETY: live moved address; only bits are used.
                            let _ =
                                unsafe { crate::types::ObjectRef::from_raw(new_addr as *mut u8) };
                            // Preserve the slot's raw-bits encoding (the
                            // smuggle stores the pointer verbatim as the slot's
                            // bits, for both the tagged-`Long` and untagged-
                            // `Double` cases) so the slot's type/dispatch
                            // doesn't change across GC — only the address it
                            // carries is relocated.
                            self.slots[i] = CompactValue::from_bits(new_addr as u64);
                        }
                    }
                }
            }
        }
    }

    // ── Snapshot/restore for continuation freeze/thaw ───────────────────

    /// Snapshot the active portion of the stack (vals + tags) for continuation freeze.
    /// Returns only the `len` live entries, not the full pre-allocated buffer.
    ///
    /// Tags are reconstructed from CompactValue tags so the FrozenFrame
    /// wire format (Vec<u64> + Vec<u8>) is preserved for continuation thaw.
    pub fn snapshot_raw(&self) -> (Vec<u64>, Vec<u8>) {
        let vals: Vec<u64> = self.slots[..self.len]
            .iter()
            .map(|cv| cv.to_bits())
            .collect();
        // Honor the kind mark when emitting SoA tags: a genuine long/double
        // whose raw bits collide with the NaN-tag space would otherwise be
        // tagged Int/Double by `compact_tag_to_vtag` and mis-restored.
        let tags: Vec<u8> = (0..self.len)
            .map(|i| match self.kinds[i] {
                KIND_LONG => crate::types::VTAG_LONG,
                KIND_DOUBLE => crate::types::VTAG_DOUBLE,
                _ => compact_tag_to_vtag(self.slots[i]),
            })
            .collect();
        (vals, tags)
    }

    /// Restore a ValueStack from a raw snapshot (produced by `snapshot_raw`).
    /// `max_size` is the original max_stack from the Code attribute.
    ///
    /// The (vals, tags) pair mirrors the legacy SoA wire format; each slot is
    /// re-encoded into its NaN-boxed form using the provided tag to
    /// disambiguate Long vs Double.
    pub fn from_snapshot(vals: Vec<u64>, tags: Vec<u8>, max_size: usize) -> Self {
        let len = vals.len();
        let cap = max_size.max(len);
        let mut slots: Vec<CompactValue> = Vec::with_capacity(cap);
        let mut kinds: Vec<u8> = Vec::with_capacity(cap);
        // If the tag vec is shorter (e.g. empty from into_inner), treat
        // missing entries as double so the raw u64 is preserved verbatim.
        for i in 0..len {
            let v = vals[i];
            let t = tags.get(i).copied().unwrap_or(crate::types::VTAG_DOUBLE);
            slots.push(vtag_to_compact(v, t));
            // Derive the kind mark from the restored SoA tag so a long/double
            // recovered from a snapshot pops bit-exact.
            kinds.push(match t {
                crate::types::VTAG_LONG => KIND_LONG,
                crate::types::VTAG_DOUBLE => KIND_DOUBLE,
                _ => KIND_UNKNOWN,
            });
        }
        // Extend to cap so push operations have space.
        slots.resize(cap, CompactValue::zero());
        kinds.resize(cap, KIND_UNKNOWN);
        Self {
            slots,
            kinds,
            len,
            max_size: cap,
        }
    }

    // ── Backward-compatible Value-based accessors for cold paths ────────

    /// Get a decoded Value at index (cold path — for type-checked operations).
    pub fn get_value(&self, index: usize) -> Value {
        if index >= self.len {
            return Value::Uninitialized;
        }
        self.value_at(index)
    }

    /// Get a raw `CompactValue` at an index (cold path — for GC, freeze).
    #[inline(always)]
    pub fn get_compact(&self, index: usize) -> Option<CompactValue> {
        if index >= self.len {
            None
        } else {
            Some(self.slots[index])
        }
    }
}

// ---------------------------------------------------------------------------
// Layout-transmute helpers (safe under CompactValue's repr(transparent)).
// ---------------------------------------------------------------------------

/// Reinterpret a `Vec<u64>` as `Vec<CompactValue>` without reallocation.
///
/// Safe because `CompactValue` is `#[repr(transparent)]` over `u64`, giving
/// identical size and alignment — a requirement for `Vec` transmutation as
/// documented in the Rust reference (nomicon §5.1).
#[inline(always)]
fn u64_vec_to_compact(v: Vec<u64>) -> Vec<CompactValue> {
    // SAFETY: CompactValue is repr(transparent) over u64, so the memory
    // layout is identical.  Vec's invariants (length, capacity, allocator)
    // are preserved.
    let mut v = std::mem::ManuallyDrop::new(v);
    let len = v.len();
    let cap = v.capacity();
    let ptr = v.as_mut_ptr() as *mut CompactValue;
    unsafe { Vec::from_raw_parts(ptr, len, cap) }
}

/// Inverse of [`u64_vec_to_compact`].
#[inline(always)]
fn compact_vec_to_u64(v: Vec<CompactValue>) -> Vec<u64> {
    let mut v = std::mem::ManuallyDrop::new(v);
    let len = v.len();
    let cap = v.capacity();
    let ptr = v.as_mut_ptr() as *mut u64;
    unsafe { Vec::from_raw_parts(ptr, len, cap) }
}

// ---------------------------------------------------------------------------
// Legacy VTAG <-> CompactValue conversion (for continuation snapshot/restore).
// ---------------------------------------------------------------------------

#[inline(always)]
fn compact_tag_to_vtag(cv: CompactValue) -> u8 {
    use crate::types::{
        VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR,
        VTAG_UNINIT,
    };
    match cv.tag() {
        CompactTag::Int => VTAG_INT,
        CompactTag::Long => VTAG_LONG,
        CompactTag::Float => VTAG_FLOAT,
        CompactTag::Double => VTAG_DOUBLE,
        CompactTag::Object => VTAG_OBJECT,
        CompactTag::Null => VTAG_NULL,
        CompactTag::Uninitialized => VTAG_UNINIT,
        CompactTag::ReturnAddress => VTAG_RETADDR,
    }
}

#[inline(always)]
fn vtag_to_compact(val: u64, tag: u8) -> CompactValue {
    use crate::types::{
        VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR,
        VTAG_UNINIT,
    };
    match tag {
        VTAG_INT => CompactValue::int(val as i32),
        VTAG_LONG => CompactValue::long(val as i64),
        VTAG_FLOAT => CompactValue::float(f32::from_bits(val as u32)),
        VTAG_DOUBLE => CompactValue::from_bits(val),
        VTAG_OBJECT => {
            if val == 0 || (val as usize) % 8 != 0 {
                CompactValue::null()
            } else {
                CompactValue::object(val)
            }
        }
        VTAG_NULL => CompactValue::null(),
        VTAG_UNINIT => CompactValue::uninitialized(),
        VTAG_RETADDR => CompactValue::return_address(val as u32),
        _ => CompactValue::uninitialized(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_pop() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(42)).unwrap();
        stack.push(Value::Int(99)).unwrap();
        assert_eq!(stack.pop_int().unwrap(), 99);
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn pop_empty_fails() {
        let mut stack = ValueStack::new(10);
        assert!(stack.pop().is_err());
    }

    #[test]
    fn overflow_fails() {
        let mut stack = ValueStack::new(2);
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        assert!(stack.push(Value::Int(3)).is_err());
    }

    /// B4 (audit `vm-runtime.md`): an operand-stack overflow must surface as
    /// the *catchable* `RuntimeError::StackOverflowError` (which the
    /// interpreter routes to a real `java/lang/StackOverflowError`), NOT the
    /// uncatchable `RuntimeError::NotImplemented` it previously returned. This
    /// covers every `push*` overflow site that shares the contract.
    #[test]
    fn overflow_returns_stack_overflow_error() {
        // Generic `push`.
        let mut stack = ValueStack::new(1);
        stack.push(Value::Int(1)).unwrap();
        assert!(
            matches!(
                stack.push(Value::Int(2)),
                Err(RuntimeError::StackOverflowError)
            ),
            "push overflow must be StackOverflowError, not NotImplemented"
        );

        // Type-specialised hot-path pushes share the same overflow contract.
        let mut s_int = ValueStack::new(0);
        assert!(matches!(
            s_int.push_int(1),
            Err(RuntimeError::StackOverflowError)
        ));

        let mut s_long = ValueStack::new(0);
        assert!(matches!(
            s_long.push_long(1),
            Err(RuntimeError::StackOverflowError)
        ));

        let mut s_float = ValueStack::new(0);
        assert!(matches!(
            s_float.push_float(1.0),
            Err(RuntimeError::StackOverflowError)
        ));

        let mut s_double = ValueStack::new(0);
        assert!(matches!(
            s_double.push_double(1.0),
            Err(RuntimeError::StackOverflowError)
        ));

        let mut s_null = ValueStack::new(0);
        assert!(matches!(
            s_null.push_null(),
            Err(RuntimeError::StackOverflowError)
        ));
    }

    #[test]
    fn soa_roundtrip_all_types() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(42)).unwrap();
        stack.push(Value::Long(999999999999)).unwrap();
        stack.push(Value::Float(3.15)).unwrap();
        stack.push(Value::Double(2.719)).unwrap();
        stack.push(Value::Object(None)).unwrap();
        stack.push(Value::Uninitialized).unwrap();

        assert_eq!(stack.pop().unwrap(), Value::Uninitialized);
        assert!(stack.pop().unwrap().is_null());
        // Bit equality: a value read back out of a stack slot has been
        // through no rounding step, so the round trip is exact or the slot
        // corrupted it. A tolerance here can only hide the second case.
        assert_eq!(stack.pop_double().unwrap().to_bits(), 2.719f64.to_bits());
        assert_eq!(stack.pop_float().unwrap().to_bits(), 3.15f32.to_bits());
        assert_eq!(stack.pop_long().unwrap(), 999999999999);
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn peek_works() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(42)).unwrap();
        stack.push(Value::Int(99)).unwrap();
        assert_eq!(stack.peek().as_int(), Some(99));
        assert_eq!(stack.peek_at(1).as_int(), Some(42));
    }

    /// Regression: longs whose bit pattern collides with the NaN-tag int
    /// space must survive `push_long` → `pop_long` bit-exactly *whenever the
    /// collision is resolvable*. Before the 2026-05-28 fix `pop_long` widened
    /// every `SUB_INT`-tagged slot via `as_int()`, dropping the upper bits of
    /// every such long — including the BC safegcd `Mod.updateDE30`/`updateFG30`
    /// accumulators (signed int*int products landing in the 0xFFFC_…/0xFFFE_…
    /// band with nonzero magnitude). See
    /// gaps/bc-ec-mod-mododdinverse-investigation.md.
    #[test]
    fn pop_long_preserves_resolvable_nan_tag_collisions() {
        // 0xFFFC_….: full NaN-box marker set; the 3-bit sub-tag (bits 49-47)
        // selects which "non-double" tag the bits masquerade as. A real
        // `CompactValue::int` stores `n as u32` (payload < 2^32), so a SUB_INT
        // slot whose payload bits 32-46 are set CANNOT be a real int — it is a
        // collision long and must reinterpret bit-exact.
        const NANBOX: u64 = 0xFFFC_0000_0000_0000;
        let resolvable: &[i64] = &[
            (NANBOX | (1u64 << 32)) as i64,          // SUB_INT, payload bit 32 set
            (NANBOX | 0x7FFF_FFFF_FFFF) as i64,      // SUB_INT, all 47 payload bits set
            (NANBOX | (1u64 << 47)) as i64,          // SUB_FLOAT pattern (any tag → reinterpret)
            (NANBOX | (2u64 << 47) | 0x55) as i64,   // SUB_OBJECT pattern, nonzero payload
            (NANBOX | (5u64 << 47) | 0x1234) as i64, // SUB_RETADDR pattern, payload bits set
            -1,                                      // natural SUB_LONG_HI
            i64::MIN,                                // untagged fast path
            i64::MAX,
            123_456_789_012_345,
        ];
        for &v in resolvable {
            let mut stack = ValueStack::new(2);
            stack.push_long(v).unwrap();
            assert_eq!(
                stack.pop_long().unwrap(),
                v,
                "push_long/pop_long must round-trip {v:#018x} bit-exact",
            );
        }
    }

    /// Documented residual: a `SUB_INT` slot with payload < 2^32 is
    /// bit-identical to a real `CompactValue::int`, so `pop_long` keeps the
    /// JVMS i2l-widen contract there (it cannot tell the two apart from a
    /// single 8-byte slot). Closing this fully needs a parallel stack type
    /// tag — see gaps/bc-ec-mod-mododdinverse-investigation.md. Pin the
    /// behaviour so a future encoding change is a conscious decision.
    #[test]
    fn pop_long_widens_unresolvable_int_collision() {
        const NANBOX: u64 = 0xFFFC_0000_0000_0000;
        // payload 0 → widens to 0L; payload 0x1234 → widens to 0x1234L.
        for &(bits, widened) in &[(NANBOX, 0i64), (NANBOX | 0x1234, 0x1234i64)] {
            let mut stack = ValueStack::new(2);
            // FIX: push via the *unmarked* compact path (`push_compact`), NOT
            // `push_long`. `push_long` is a genuine long producer: it marks the
            // slot `KIND_LONG`, which `pop_long` honors by reading the bits
            // bit-exact (see `pop_long_preserves_resolvable_nan_tag_collisions`,
            // which pushes via `push_long` and asserts bit-exact round-trip). A
            // `KIND_LONG` slot therefore NEVER widens — so the old `push_long`
            // here contradicted the design and yielded the raw bits
            // (-1125899906842624) instead of the widened value.
            //
            // The i2l-widening fallback this test pins is only reachable for an
            // UNRESOLVABLE collision slot, i.e. one pushed *without* the long
            // mark (e.g. `lload_N`'s `push_compact`, or a synthetic int-where-
            // long). Such a SUB_INT slot with payload < 2^32 is bit-identical to
            // a real `CompactValue::int`, so `pop_long` applies JVMS i2l
            // sign-extension — the documented unsalvageable residual.
            stack.push_compact(crate::types::CompactValue::long(bits as i64));
            assert_eq!(stack.pop_long().unwrap(), widened);
        }
    }

    /// Regression: the full `lload` → consume sequence the interpreter runs.
    /// `lload_N` (fast path) now pushes the local's raw CompactValue bits via
    /// `push_compact`; the consuming long opcode pops via `pop_long`. Both
    /// stages must be bit-exact for resolvable collision-pattern longs.
    #[test]
    fn lload_then_pop_long_is_bit_exact_for_collisions() {
        const NANBOX: u64 = 0xFFFC_0000_0000_0000;
        for &v in &[
            (NANBOX | 0x7FFF_FFFF_FFFF) as i64, // SUB_INT, payload bits 32-46 set
            (NANBOX | (2u64 << 47) | 0x55) as i64, // SUB_OBJECT pattern
        ] {
            // Simulate `lstore` writing the slot, then `lload` reading it: a
            // local slot stores the long verbatim via CompactValue::long.
            let local = crate::types::CompactValue::long(v);
            let mut stack = ValueStack::new(2);
            stack.push_compact(local); // lload_N
            assert_eq!(stack.pop_long().unwrap(), v, "lload/pop {v:#018x}");
        }
    }

    /// SCAN/UPDATE SYMMETRY: a smuggled jobject reference (JNI long-as-jobject
    /// convention) that `scan_object_refs` yields as a root MUST be rewritten
    /// by `update_object_refs` to the relocated address after a moving GC.
    ///
    /// This pins the asymmetry fix: previously the scan rooted such a smuggle
    /// out of the `CompactTag::Long | Double` hybrid branch, but the update
    /// only had a `Double` arm — so the rooted slot was never remapped and the
    /// reference went stale (UAF / wrong object) the instant the referent
    /// moved. The test allocates two live heap objects, smuggles object A's
    /// address into a long-marked slot, confirms the scan roots it, then
    /// relocates A→B via the pointer_map and asserts the slot now carries B's
    /// address.
    #[test]
    fn smuggled_jobject_is_rooted_and_remapped_symmetrically() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        use cratonvm_types::ClassId;
        use std::collections::HashMap;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        // Two real, live heap objects: A is the pre-GC referent, B stands in
        // for A's post-compaction location. Both addresses pass `is_heap_addr`.
        let a = heap.alloc_object(ClassId::new(0), 0);
        let b = heap.alloc_object(ClassId::new(0), 0);
        let old_addr = a.as_ptr() as u64;
        let new_addr = b.as_ptr() as u64;
        assert_eq!(old_addr & 0x7, 0, "heap objects are 8-byte aligned");
        assert_eq!(new_addr & 0x7, 0, "heap objects are 8-byte aligned");
        assert_ne!(old_addr, new_addr);

        // Smuggle A's address into a slot via a genuine long producer (the
        // JNI long-as-jobject contract: a heap pointer carried in a long
        // slot). `push_long` marks the slot `KIND_LONG`; a low heap address is
        // not NaN-tagged, so the slot reports `CompactTag::Double` raw bits —
        // exactly the shape `scan_object_refs`'s hybrid branch roots.
        // Mint-provenance (2026-07-03): the rewrite arm only remaps values
        // registered at a mint chokepoint — model the JNI mint explicitly.
        crate::memory::smuggled_longs::record_minted_long(&heap, old_addr);
        let mut stack = ValueStack::new(4);
        stack.push_long(old_addr as i64).unwrap();

        // SCAN: the smuggle must be yielded as a root.
        let mut roots = Vec::new();
        stack.scan_object_refs(&mut roots, &heap);
        assert!(
            roots.iter().any(|r| r.as_ptr() as u64 == old_addr),
            "scan_object_refs must root the smuggled jobject (addr {old_addr:#x})"
        );

        // UPDATE: relocate A → B and assert the slot is rewritten — the half
        // that was missing for the `Long`-classified arm.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_addr as usize, new_addr as usize);
        stack.update_object_refs(&map, &heap);

        let slot = stack.peek_compact();
        assert_eq!(
            slot.to_bits(),
            new_addr,
            "update_object_refs must remap the rooted smuggle to the moved address"
        );
    }

    #[test]
    fn smuggled_jobject_remapped_when_old_region_freed_g1() {
        // Step 5 GAP A regression: under G1 the evacuated CSet region is reset to
        // `Free`, so post-GC `is_heap_addr(old_ptr)` returns None for a genuinely
        // moved object. The remap must still rewrite the smuggle (gate on the live
        // `new_addr`, not the freed `old_ptr`). Model the freed from-space with an
        // `old_addr` that is not in any live region while `new_addr` is a real,
        // live object — exactly the asymmetry G1's CSet-free creates.
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        use cratonvm_types::ClassId;
        use std::collections::HashMap;

        let heap = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
        let b = heap.alloc_object(ClassId::new(0), 0);
        let new_addr = b.as_ptr() as u64;
        // A pointer-shaped (8-aligned, sub-2^48) address that is NOT in any live
        // region — `is_heap_addr` returns None, as it would for a freed CSet slot.
        let old_addr: u64 = 0x1_0000;
        assert!(
            heap.is_heap_addr(old_addr as usize).is_none(),
            "old_addr must look freed (not in a live region)"
        );
        assert!(
            heap.is_heap_addr(new_addr as usize).is_some(),
            "new_addr must be a live relocated object"
        );

        // Mint-provenance (2026-07-03): register the handle as a JNI mint
        // would have; the `from_raw` round-trip ALSO records construction
        // provenance, making this test independent of cross-test
        // provenance-bitmap granule pollution (old_addr=0x1_0000 previously
        // passed `jlong_bits_as_aligned_object_ptr` only when another test in
        // the same process happened to record provenance in that granule).
        // SAFETY: the pointer is only used for its bits (never dereferenced).
        let _ = unsafe { cratonvm_types::ObjectRef::from_raw(old_addr as *mut u8) };
        crate::memory::smuggled_longs::record_minted_long(&heap, old_addr);
        let mut stack = ValueStack::new(4);
        stack.push_long(old_addr as i64).unwrap();
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_addr as usize, new_addr as usize);
        stack.update_object_refs(&map, &heap);

        assert_eq!(
            stack.peek_compact().to_bits(),
            new_addr,
            "smuggle must be remapped even though old_addr's region was freed (G1)"
        );
    }

    /// Mint-provenance gate (2026-07-03): a genuine primitive long whose bits
    /// coincidentally equal a moved object's from-space address must NOT be
    /// rewritten — the collision case the provenance-of-construction check
    /// can never discriminate (the moved object is always a real,
    /// once-constructed object).
    #[test]
    fn colliding_primitive_long_is_not_rewritten() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;
        use cratonvm_types::ClassId;
        use std::collections::HashMap;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let a = heap.alloc_object(ClassId::new(0), 0);
        let b = heap.alloc_object(ClassId::new(0), 0);
        let old_addr = a.as_ptr() as u64;
        let new_addr = b.as_ptr() as u64;

        // The primitive's value equals A's address, but it was NEVER minted
        // as a handle (no record_minted_long).
        //
        // This used to `return` early when the address was already minted,
        // because the registry was one process-global set and another test's
        // heap could have handed out an identical address — i.e. the test
        // silently skipped itself. The registry is now keyed on the heap, so
        // this freshly-created heap has an empty table by construction and
        // the guard can be an assertion.
        assert!(
            !crate::memory::smuggled_longs::is_minted(&heap, old_addr),
            "a fresh heap must start with no mints registered against it"
        );
        let mut stack = ValueStack::new(4);
        stack.push_long(old_addr as i64).unwrap();

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_addr as usize, new_addr as usize);
        stack.update_object_refs(&map, &heap);

        assert_eq!(
            stack.peek_compact().to_bits(),
            old_addr,
            "an un-minted primitive long colliding with a moved address must be preserved"
        );
    }

    // B12: `*_unchecked` guards are now `debug_assert!`, so the panic only
    // fires in debug builds. Gate the `#[should_panic]` coverage on
    // `debug_assertions` so release-mode test runs don't fail these.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "stack overflow")]
    fn push_unchecked_panics_on_overflow() {
        let mut stack = ValueStack::new(1);
        stack.push_unchecked(Value::Int(1));
        stack.push_unchecked(Value::Int(2)); // should panic
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "stack underflow")]
    fn pop_unchecked_panics_on_underflow() {
        let mut stack = ValueStack::new(10);
        stack.pop_unchecked(); // should panic
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "stack underflow")]
    fn peek_panics_on_empty() {
        let stack = ValueStack::new(10);
        let _ = stack.peek(); // should panic
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "stack underflow")]
    fn peek_at_panics_out_of_range() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(1)).unwrap();
        let _ = stack.peek_at(5); // should panic
    }

    #[test]
    fn peek_checked_empty_returns_err() {
        let stack = ValueStack::new(10);
        assert!(stack.peek_checked().is_err());
    }

    #[test]
    fn get_value_out_of_range_returns_uninitialized() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(42)).unwrap();
        assert_eq!(stack.get_value(99), Value::Uninitialized);
    }

    #[test]
    fn clear_resets_stack() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        assert_eq!(stack.len(), 2);
        stack.clear();
        assert_eq!(stack.len(), 0);
        assert!(stack.is_empty());
    }

    #[test]
    fn pop_wrong_type_returns_err() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Float(1.0)).unwrap();
        assert!(stack.pop_int().is_err());
    }

    #[test]
    fn pop_long_widens_int() {
        // Session 71 stack-widening: pop_long accepts Int by sign-extension
        // (compensates for native methods or bytecode sequences that leave
        // an int where a long is expected).
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(115)).unwrap();
        assert_eq!(stack.pop_long().unwrap(), 115i64);
    }

    #[test]
    fn pop_float_widens_int() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(7)).unwrap();
        assert_eq!(stack.pop_float().unwrap(), 7.0f32);
    }

    #[test]
    fn pop_double_widens_int() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(9)).unwrap();
        assert_eq!(stack.pop_double().unwrap(), 9.0f64);
    }

    #[test]
    fn from_pooled_creates_valid_stack() {
        let vals = Vec::new();
        let tags = Vec::new();
        let mut stack = ValueStack::from_pooled(vals, tags, 5);
        stack.push(Value::Int(42)).unwrap();
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn into_inner_returns_vecs() {
        let mut stack = ValueStack::new(5);
        stack.push(Value::Int(1)).unwrap();
        let (vals, _tags) = stack.into_inner();
        assert!(vals.len() >= 5);
        // tags are now stored inline — an empty Vec is returned for the
        // legacy pool signature.
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn stack_size_zero() {
        let stack = ValueStack::new(0);
        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn stack_size_zero_push_fails() {
        let mut stack = ValueStack::new(0);
        assert!(stack.push(Value::Int(1)).is_err());
    }

    #[test]
    fn stack_size_zero_pop_fails() {
        let mut stack = ValueStack::new(0);
        assert!(stack.pop().is_err());
    }

    #[test]
    fn stack_size_one_push_pop() {
        let mut stack = ValueStack::new(1);
        stack.push(Value::Int(77)).unwrap();
        assert_eq!(stack.len(), 1);
        assert!(!stack.is_empty());
        assert_eq!(stack.pop_int().unwrap(), 77);
        assert!(stack.is_empty());
    }

    #[test]
    fn stack_size_one_overflow() {
        let mut stack = ValueStack::new(1);
        stack.push(Value::Int(1)).unwrap();
        assert!(stack.push(Value::Int(2)).is_err());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "stack overflow")]
    fn push_unchecked_panics_on_full_size_zero() {
        let mut stack = ValueStack::new(0);
        stack.push_unchecked(Value::Int(1));
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "stack underflow")]
    fn peek_at_panics_on_empty_stack() {
        let stack = ValueStack::new(5);
        let _ = stack.peek_at(0);
    }

    #[test]
    fn gc_update_object_refs_empty_map() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut stack = ValueStack::new(5);
        stack.push(Value::Int(42)).unwrap();
        let empty_map = cratonvm_types::PointerMap::default();
        stack.update_object_refs(&empty_map, &heap);
        // Stack should be unchanged
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn from_pooled_with_undersized_vecs() {
        // Start with tiny vecs that are smaller than max_size
        let vals = vec![0u64; 1];
        let tags = vec![0u8; 1];
        let mut stack = ValueStack::from_pooled(vals, tags, 10);
        // Should be able to push up to 10 elements
        for i in 0..10 {
            stack.push(Value::Int(i)).unwrap();
        }
        assert_eq!(stack.len(), 10);
        assert!(stack.push(Value::Int(99)).is_err());
    }

    #[test]
    fn from_pooled_with_oversized_vecs() {
        // Start with vecs larger than max_size -- should resize down
        let vals = vec![0xFFu64; 100];
        let tags = vec![0xFFu8; 100];
        let mut stack = ValueStack::from_pooled(vals, tags, 3);
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        stack.push(Value::Int(3)).unwrap();
        assert!(stack.push(Value::Int(4)).is_err());
    }

    #[test]
    fn into_inner_preserves_capacity() {
        let mut stack = ValueStack::new(8);
        stack.push(Value::Int(10)).unwrap();
        stack.push(Value::Long(20)).unwrap();
        let (vals, tags) = stack.into_inner();
        // Vecs should have at least max_size elements
        assert!(vals.len() >= 8);
        // Tags are no longer stored; an empty Vec is returned.
        assert!(tags.is_empty());
        // Reuse them
        let mut stack2 = ValueStack::from_pooled(vals, tags, 8);
        stack2.push(Value::Int(30)).unwrap();
        assert_eq!(stack2.pop_int().unwrap(), 30);
    }

    /// `take_inner_in_place` must hand the pool exactly what `into_inner`
    /// hands it, and must leave the husk describing an EMPTY stack.
    ///
    /// The husk half is the part that matters and the part a naive
    /// implementation gets wrong: `mem::take`ing `slots` while leaving `len`
    /// at its old depth yields a stack that claims elements it does not have,
    /// and the husk is observable (a GC root scan can walk this thread's
    /// frames) until the enclosing frame is dropped.
    #[test]
    fn take_inner_in_place_matches_into_inner_and_empties_the_husk() {
        let mut owned = ValueStack::new(8);
        owned.push(Value::Int(10)).unwrap();
        owned.push(Value::Long(20)).unwrap();
        let (want_vals, want_tags) = owned.into_inner();

        let mut husk = ValueStack::new(8);
        husk.push(Value::Int(10)).unwrap();
        husk.push(Value::Long(20)).unwrap();
        let (got_vals, got_tags) = husk.take_inner_in_place();

        assert_eq!(got_vals, want_vals, "pooled value buffer must match");
        assert!(got_tags.is_empty(), "tag half is returned empty");
        assert_eq!(got_tags.capacity(), want_tags.capacity());

        assert_eq!(husk.len(), 0, "husk must not claim a depth it cannot serve");
        assert!(husk.is_empty());

        // And the harvested buffers still round-trip through the pool.
        let mut reused = ValueStack::from_pooled(got_vals, got_tags, 8);
        reused.push(Value::Int(30)).unwrap();
        assert_eq!(reused.pop_int().unwrap(), 30);
    }

    #[test]
    fn multiple_push_pop_cycles() {
        let mut stack = ValueStack::new(4);
        // Cycle 1
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        assert_eq!(stack.pop_int().unwrap(), 2);
        assert_eq!(stack.pop_int().unwrap(), 1);
        assert!(stack.is_empty());

        // Cycle 2
        stack.push(Value::Long(100)).unwrap();
        stack.push(Value::Float(1.5)).unwrap();
        stack.push(Value::Double(2.5)).unwrap();
        assert_eq!(stack.pop_double().unwrap().to_bits(), 2.5f64.to_bits());
        assert_eq!(stack.pop_float().unwrap().to_bits(), 1.5f32.to_bits());
        assert_eq!(stack.pop_long().unwrap(), 100);
        assert!(stack.is_empty());

        // Cycle 3: fill to capacity
        for i in 0..4 {
            stack.push(Value::Int(i)).unwrap();
        }
        assert_eq!(stack.len(), 4);
        stack.clear();
        assert!(stack.is_empty());
    }

    #[test]
    fn stack_depth_tracking_accuracy() {
        let mut stack = ValueStack::new(10);
        assert_eq!(stack.len(), 0);

        stack.push(Value::Int(1)).unwrap();
        assert_eq!(stack.len(), 1);

        stack.push(Value::Int(2)).unwrap();
        assert_eq!(stack.len(), 2);

        let _ = stack.pop();
        assert_eq!(stack.len(), 1);

        stack.push(Value::Int(3)).unwrap();
        stack.push(Value::Int(4)).unwrap();
        assert_eq!(stack.len(), 3);

        stack.clear();
        assert_eq!(stack.len(), 0);

        // push_unchecked also tracks depth
        stack.push_unchecked(Value::Int(10));
        assert_eq!(stack.len(), 1);

        let _ = stack.pop_unchecked();
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn peek_at_all_positions() {
        let mut stack = ValueStack::new(5);
        stack.push(Value::Int(10)).unwrap();
        stack.push(Value::Int(20)).unwrap();
        stack.push(Value::Int(30)).unwrap();

        assert_eq!(stack.peek_at(0).as_int(), Some(30)); // top
        assert_eq!(stack.peek_at(1).as_int(), Some(20)); // second
        assert_eq!(stack.peek_at(2).as_int(), Some(10)); // bottom
    }

    #[test]
    fn get_value_within_bounds() {
        let mut stack = ValueStack::new(5);
        stack.push(Value::Int(10)).unwrap();
        stack.push(Value::Int(20)).unwrap();
        assert_eq!(stack.get_value(0).as_int(), Some(10));
        assert_eq!(stack.get_value(1).as_int(), Some(20));
    }

    #[test]
    fn snapshot_and_restore() {
        let mut stack = ValueStack::new(10);
        stack.push(Value::Int(1)).unwrap();
        stack.push(Value::Int(2)).unwrap();
        stack.push(Value::Int(3)).unwrap();
        assert_eq!(stack.len(), 3);

        let (vals, tags) = stack.snapshot_raw();
        assert_eq!(vals.len(), 3);
        assert_eq!(tags.len(), 3);

        let mut restored = ValueStack::from_snapshot(vals, tags, 10);
        assert_eq!(restored.len(), 3);
        assert_eq!(restored.pop_int().unwrap(), 3);
        assert_eq!(restored.pop_int().unwrap(), 2);
        assert_eq!(restored.pop_int().unwrap(), 1);
        assert!(restored.is_empty());

        // Can still push after restore (max_size preserved)
        restored.push(Value::Int(99)).unwrap();
        assert_eq!(restored.len(), 1);
    }

    #[test]
    fn snapshot_empty_stack() {
        let stack = ValueStack::new(5);
        let (vals, tags) = stack.snapshot_raw();
        assert!(vals.is_empty());
        assert!(tags.is_empty());

        let restored = ValueStack::from_snapshot(vals, tags, 5);
        assert!(restored.is_empty());
    }

    // ── T10.6 CompactValue-focused tests ───────────────────────────────

    #[test]
    fn t10_compact_value_stack_push_pop_roundtrip() {
        let mut stack = ValueStack::new(16);
        let cases = [
            Value::Int(i32::MIN),
            Value::Int(-1),
            Value::Int(0),
            Value::Int(i32::MAX),
            Value::Long(i64::MIN),
            Value::Long(0),
            Value::Long(i64::MAX),
            Value::Float(3.5),
            Value::Float(-0.0),
            Value::Double(std::f64::consts::PI),
            Value::Double(f64::INFINITY),
            Value::Object(None),
            Value::Uninitialized,
            Value::ReturnAddress(12345),
        ];
        for v in cases.iter().copied() {
            stack.push(v).unwrap();
        }
        // Pop in reverse, comparing each value.
        for v in cases.iter().rev() {
            let popped = stack.pop().unwrap();
            match (v, &popped) {
                // Long decodes as Double (untagged) so compare bit patterns
                // instead of enum variants.
                (Value::Long(l), Value::Double(d)) => {
                    assert_eq!(*l as u64, d.to_bits());
                }
                _ => assert_eq!(&popped, v, "roundtrip failed for {v}"),
            }
        }
    }

    #[test]
    fn t10_compact_value_stack_memory_footprint() {
        // CompactValue itself is 8 bytes.
        assert_eq!(std::mem::size_of::<CompactValue>(), 8);

        // Each slot in the stack's internal Vec is 8 bytes.
        let stack = ValueStack::new(4);
        assert_eq!(std::mem::size_of_val(&stack.slots[0]), 8);

        // The total underlying buffer is 8 bytes * slot_count.
        let byte_total = stack.slots.len() * std::mem::size_of::<CompactValue>();
        assert_eq!(byte_total, stack.slots.len() * 8);
    }

    #[test]
    fn t10_compact_value_reference_roundtrip() {
        let mut stack = ValueStack::new(4);
        // Use an 8-byte-aligned fake pointer.
        let fake_ptr = 0x1234_5678_ABC0_u64 as *mut u8;
        let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
        stack.push(Value::Object(Some(obj))).unwrap();
        let v = stack.pop().unwrap();
        match v {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr() as u64, 0x1234_5678_ABC0),
            other => panic!("expected Object ref, got {other:?}"),
        }
    }

    #[test]
    fn t10_compact_value_dup_preserves_tag() {
        // Dup-2 semantics for category-2 values: two slots round-trip correctly.
        let mut stack = ValueStack::new(8);
        stack.push(Value::Long(0x0BAD_BEEF_DEAD_CAFE)).unwrap();
        stack.push(Value::Uninitialized).unwrap(); // high half of long
                                                   // Simulate dup2: copy the two top slots.
        let high = stack.pop_compact();
        let low = stack.pop_compact();
        stack.push_compact(low);
        stack.push_compact(high);
        stack.push_compact(low);
        stack.push_compact(high);
        // Top of stack: [long_lo, long_hi, long_lo, long_hi]
        let _top_hi = stack.pop_compact(); // high pad
        let top_lo = stack.pop_compact();
        assert_eq!(top_lo.as_long_unchecked(), 0x0BAD_BEEF_DEAD_CAFE);
    }

    #[test]
    fn t10_compact_value_fibonacci_smoke() {
        // Fib(10) computed by push/pop sequences; verifies basic arithmetic
        // round-trips correctly through the CompactValue-backed stack.
        let mut stack = ValueStack::new(32);
        let mut a: i32 = 0;
        let mut b: i32 = 1;
        for _ in 0..10 {
            stack.push(Value::Int(a)).unwrap();
            stack.push(Value::Int(b)).unwrap();
            let vb = stack.pop_int().unwrap();
            let va = stack.pop_int().unwrap();
            let c = va.wrapping_add(vb);
            a = vb;
            b = c;
        }
        assert_eq!(a, 55);
    }

    #[test]
    fn t10_compact_value_push_compact_roundtrip() {
        let mut stack = ValueStack::new(4);
        let cv = CompactValue::int(42);
        stack.push_compact(cv);
        assert_eq!(stack.peek_compact().as_int(), Some(42));
        let popped = stack.pop_compact();
        assert_eq!(popped.as_int(), Some(42));
    }

    #[test]
    fn t10_gc_scan_object_refs_from_compact_stack() {
        let mut stack = ValueStack::new(4);
        let fake = 0x0000_1000_u64 as *mut u8;
        let obj = unsafe { ObjectRef::from_raw(fake) };
        stack.push(Value::Object(Some(obj))).unwrap();
        stack.push(Value::Int(99)).unwrap();
        stack.push(Value::Object(None)).unwrap();

        let mut roots = Vec::new();
        let heap = crate::memory::vm_heap::VmHeap::new(
            crate::memory::vm_heap::GcBackend::Generational,
            1024 * 1024,
        );
        stack.scan_object_refs(&mut roots, &heap);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].as_ptr() as u64, 0x1000);
    }

    // ── T10.9.D direct-push/pop helpers ────────────────────────────────

    #[test]
    fn push_int_roundtrip() {
        let mut stack = ValueStack::new(4);
        stack.push_int(42).unwrap();
        assert_eq!(stack.peek_compact().as_int(), Some(42));
        assert_eq!(stack.pop_int().unwrap(), 42);
    }

    #[test]
    fn push_long_roundtrip_negative() {
        let mut stack = ValueStack::new(4);
        stack.push_long(-i64::MAX).unwrap();
        // Untagged — read via unchecked long accessor.
        assert_eq!(stack.peek_compact().as_long_unchecked(), -i64::MAX);
        assert_eq!(stack.pop_long().unwrap(), -i64::MAX);
    }

    #[test]
    fn push_float_roundtrip() {
        let mut stack = ValueStack::new(4);
        stack.push_float(1.5).unwrap();
        assert_eq!(stack.peek_compact().as_float(), Some(1.5f32));
        assert_eq!(stack.pop_float().unwrap().to_bits(), 1.5f32.to_bits());
    }

    #[test]
    fn push_double_roundtrip() {
        let mut stack = ValueStack::new(4);
        stack.push_double(std::f64::consts::PI).unwrap();
        assert_eq!(stack.peek_compact().as_double(), Some(std::f64::consts::PI));
        // Bit equality: a stored value that is read back has been through NO rounding step, so the round trip is bit-exact or the slot corrupted it.
        assert_eq!(
            stack.pop_double().unwrap().to_bits(),
            std::f64::consts::PI.to_bits()
        );
    }

    #[test]
    fn push_null_roundtrip() {
        let mut stack = ValueStack::new(4);
        stack.push_null().unwrap();
        assert!(stack.peek_compact().is_null());
        assert!(stack.pop().unwrap().is_null());
    }

    #[test]
    fn push_int_overflow_returns_err() {
        let mut stack = ValueStack::new(1);
        stack.push_int(1).unwrap();
        assert!(stack.push_int(2).is_err());
    }

    #[test]
    fn t10_gc_update_object_refs_rewrites_pointer() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut stack = ValueStack::new(4);
        let old_ptr = 0x0000_1000_u64;
        let new_ptr = 0x0000_2000_u64;
        let obj = unsafe { ObjectRef::from_raw(old_ptr as *mut u8) };
        stack.push(Value::Object(Some(obj))).unwrap();

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_ptr as usize, new_ptr as usize);
        stack.update_object_refs(&map, &heap);

        let popped = stack.pop().unwrap();
        match popped {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr() as u64, new_ptr),
            other => panic!("expected Object, got {other:?}"),
        }
    }

    /// Per JVM spec a `CompactTag::Long` operand slot IS a primitive long, never
    /// a reference. The scan must NOT root such slots — this was the SEGV trigger
    /// described in `applogs/letsgo-segv-L1-diagnosis.md`. To exercise the buggy
    /// path we need a slot whose `tag()` actually returns `Long`, i.e. a u64 whose
    /// high bits collide with the NaN-box marker AND whose 3-bit sub-tag is
    /// SUB_LONG_LO (110) or SUB_LONG_HI (111). `CompactValue::long` only produces
    /// such a tag for specific bit patterns; build one directly via `from_bits`.
    #[test]
    fn t10_gc_scan_skips_long_slot_even_if_bits_resemble_heap_pointer() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);

        // NANBOX_BITS (0xFFFC_0000_0000_0000) | SUB_LONG_LO (6 << 47)
        //   | aligned-pointer-shaped low 47 bits (0x10000).
        // tag() returns CompactTag::Long for this pattern.
        let long_bits: u64 = 0xFFFC_0000_0000_0000 | (6u64 << 47) | 0x1_0000;

        let mut stack = ValueStack::new(4);
        stack.push_compact(CompactValue::from_bits(long_bits));
        assert_eq!(
            stack.get_compact(0).unwrap().tag(),
            CompactTag::Long,
            "test setup must produce a CompactTag::Long slot"
        );

        let mut roots = Vec::new();
        stack.scan_object_refs(&mut roots, &heap);
        assert!(
            roots.is_empty(),
            "primitive Long slots must not be treated as GC roots"
        );
    }

    /// Lost-tag operand-stack refs are retained by the non-moving conservative
    /// scan when the raw payload still names a valid heap object.
    #[test]
    fn t10_gc_conservative_scan_roots_lost_tag_stack_slot() {
        use crate::classloading::ClassId;
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as usize;

        let mut stack = ValueStack::new(4);
        stack.push_compact_long(CompactValue::long(addr as i64));

        let mut roots = Vec::new();
        stack.scan_object_refs_conservative(&mut roots, &heap);
        assert!(
            roots.iter().any(|r| r.as_ptr() as usize == addr),
            "non-moving conservative stack scan must retain lost-tag object refs"
        );
    }

    /// Untagged raw bits (CompactTag::Double — the ambiguous JVM long/double
    /// slot in this encoding) that pass `heap.is_object_address` ARE rooted.
    /// This is the safe surviving path after the Long-arm fix.
    #[test]
    fn t10_gc_scan_roots_untagged_bits_when_heap_validates() {
        use crate::classloading::ClassId;
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as usize;

        let mut stack = ValueStack::new(4);
        // Untagged Double-encoded raw bits (the ambiguous slot kind).
        stack.push_compact(CompactValue::from_bits(addr as u64));

        let mut roots = Vec::new();
        stack.scan_object_refs(&mut roots, &heap);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].as_ptr() as usize, addr);
    }

    #[test]
    fn t10_gc_scan_skips_aligned_integer_long_not_in_heap() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut stack = ValueStack::new(2);
        stack.push_compact(CompactValue::long(8));

        let mut roots = Vec::new();
        stack.scan_object_refs(&mut roots, &heap);
        assert!(roots.is_empty());
    }

    /// REGRESSION (bc math-ec collision-long corruption): a primitive `long`
    /// pushed via `push_long` (⇒ `KIND_LONG`) whose bits collide with the
    /// SUB_OBJECT pattern AND whose payload aliases a *live* heap object must
    /// be treated as a value, not a reference — neither rooted by
    /// `scan_object_refs` nor remapped by `update_object_refs`. Before the fix
    /// the `else if … is_heap_addr` rooting branch (scan) and the unconditional
    /// `is_object()` remap (update) corrupted such a long under moving GC.
    #[test]
    fn t10_gc_scan_and_update_skip_collision_long_aliasing_live_object() {
        use crate::classloading::ClassId;
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as u64;

        // Bit-identical to `CompactValue::object(addr)`, but a primitive long.
        let collision_bits = CompactValue::object(addr).raw_bits();

        let mut stack = ValueStack::new(4);
        stack.push_long(collision_bits as i64).unwrap(); // marks KIND_LONG
        assert!(
            stack.get_compact(0).unwrap().is_object(),
            "collision long must carry SUB_OBJECT bits for this test to bite",
        );

        // scan: a KIND_LONG slot is never a root, even aliasing a live object.
        let mut roots = Vec::new();
        stack.scan_object_refs(&mut roots, &heap);
        assert!(
            roots.is_empty(),
            "a KIND_LONG collision long must not be rooted",
        );

        // update: a pointer_map keyed off the collision payload must not rewrite it.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(addr as usize, (addr as usize) + 64);
        stack.update_object_refs(&map, &heap);
        assert_eq!(
            stack.get_compact(0).unwrap().to_bits(),
            collision_bits,
            "primitive long must be preserved bit-exact across GC update",
        );
    }

    /// REGRESSION (bc math-ec): the kind-preserving shuffle primitives keep a
    /// collision-shaped long's `KIND_LONG` mark across a `dup`, so the GC root
    /// scan still skips it. With the old `push_compact_checked` path the
    /// duplicate landed `KIND_UNKNOWN` and — sharing the genuine-reference bit
    /// pattern — was rooted and relocated, corrupting the long.
    #[test]
    fn kind_preserving_dup_keeps_collision_long_unrooted() {
        use crate::classloading::ClassId;
        use crate::memory::vm_heap::{GcBackend, VmHeap};

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as u64;
        let collision_bits = CompactValue::object(addr).raw_bits();

        let mut stack = ValueStack::new(8);
        stack.push_long(collision_bits as i64).unwrap(); // KIND_LONG

        // Simulate `dup` via the kind-preserving primitives (what the
        // interpreter's Dup/Dup2/Swap handlers now use).
        let (cv, kind) = stack.peek_with_kind().unwrap();
        stack.push_with_kind(cv, kind).unwrap();

        // The duplicate is a category-2 long by its kind (not its bits)…
        assert!(ValueStack::is_cat2_kind(kind, cv));
        // …and neither copy is a GC root.
        let mut roots = Vec::new();
        stack.scan_object_refs(&mut roots, &heap);
        assert!(
            roots.is_empty(),
            "a kind-preserved collision long (and its dup) must not be rooted",
        );
    }

    /// `update_object_refs` must NOT touch `CompactTag::Long` slots — they are
    /// primitive longs, never references. A pointer_map collision against a
    /// primitive's bits would otherwise silently corrupt the long value.
    /// Uses a hand-rolled bit pattern (see scan-side test) to guarantee a
    /// `Long`-tagged slot.
    #[test]
    fn t10_gc_update_object_refs_preserves_long_slot() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut stack = ValueStack::new(4);
        // NANBOX_BITS | SUB_LONG_HI (7 << 47) | aligned low bits (0x1000).
        let long_bits: u64 = 0xFFFC_0000_0000_0000 | (7u64 << 47) | 0x1000;
        let new_ptr: usize = 0x2000;
        stack.push_compact(CompactValue::from_bits(long_bits));
        assert_eq!(
            stack.get_compact(0).unwrap().tag(),
            CompactTag::Long,
            "test setup must produce a CompactTag::Long slot"
        );

        let mut map = cratonvm_types::PointerMap::default();
        // A malicious pointer_map entry keyed off the Long's low pointer-shaped
        // bits — the fix must NOT remap because Long slots are never roots.
        map.insert(0x1000_usize, new_ptr);
        // Also map the raw long_bits itself (defensive — neither key should fire).
        map.insert(long_bits as usize, new_ptr);
        stack.update_object_refs(&map, &heap);

        // Bits unchanged: primitive long is left strictly alone.
        assert_eq!(stack.get_compact(0).unwrap().to_bits(), long_bits);
    }

    /// Untagged raw `Double` slot carrying an aligned, mapped pointer IS
    /// remapped (these are the only ambiguous slots `scan_object_refs` rooted).
    /// B10: the old_ptr MUST be a real heap address — `update_object_refs`
    /// now mirrors `scan_object_refs`'s `heap.is_heap_addr` filter to avoid
    /// rewriting honest doubles whose bits coincidentally collide with a
    /// pointer_map key.
    #[test]
    fn t10_gc_update_object_refs_rewrites_untagged_pointer_shaped_slot() {
        use crate::classloading::ClassId;
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(ClassId::new(0), 0);
        let old_ptr = obj.as_ptr() as usize;
        let new_ptr: usize = old_ptr + 64;
        // Mint-provenance (2026-07-03): raw pointer-shaped slots are only
        // rewritten when the value was registered at a mint chokepoint —
        // model the JNI mint this test's smuggle represents.
        crate::memory::smuggled_longs::record_minted_long(&heap, old_ptr as u64);
        let mut stack = ValueStack::new(4);
        stack.push_compact(CompactValue::from_bits(old_ptr as u64));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_ptr, new_ptr);
        stack.update_object_refs(&map, &heap);

        // Slot now carries the remapped raw bits.
        let cv = stack.get_compact(0).expect("slot present");
        assert_eq!(cv.to_bits(), new_ptr as u64);
    }

    /// B10: an honest f64 whose bit pattern happens to collide with a
    /// pointer_map key must NOT be rewritten — the slot is not heap-resident,
    /// so it cannot have been a rooted reference. The old behaviour would
    /// silently corrupt the f64.
    #[test]
    fn t10_gc_update_object_refs_leaves_non_heap_double_untouched() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let mut stack = ValueStack::new(4);
        // Non-heap aligned pointer-shaped bits — the previous behaviour
        // would have rewritten this; with the heap-membership check it
        // stays put.
        let old_bits: u64 = 0x1000;
        let new_ptr: usize = 0x2000;
        stack.push_compact(CompactValue::from_bits(old_bits));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old_bits as usize, new_ptr);
        stack.update_object_refs(&map, &heap);

        let cv = stack.get_compact(0).expect("slot present");
        assert_eq!(
            cv.to_bits(),
            old_bits,
            "non-heap Double bits must be preserved"
        );
    }

    // -----------------------------------------------------------------
    // `kinds`-array load-bearing proof (2026-07-26 frame-arena audit)
    // -----------------------------------------------------------------
    //
    // These lock in the invariant that makes the parallel `kinds` array
    // impossible to collapse into the NaN-boxed `slots` buffer: an arbitrary
    // 64-bit `long` uses every payload bit, so its kind CANNOT live in-slot.
    // If someone later removes `kinds`, these fail.

    /// Bit patterns that are known to collide with the NaN-box tag space, plus
    /// the ordinary extremes. Every one must survive a push/pop round trip
    /// bit-exactly.
    const LONG_ROUND_TRIP_CASES: &[i64] = &[
        0,
        1,
        -1,
        i64::MAX,
        i64::MIN,
        0x7fff_ffff,
        -0x8000_0000,
        0x1_0000_0000,
        // SUB_OBJECT-colliding pattern (BouncyCastle `LongArray` lxor/lshl).
        0xfffd_1234_5678_9abcu64 as i64,
        0xfffd_0000_0000_0000u64 as i64,
        // SUB_INT-colliding patterns (BC safegcd `Mod.updateDE30`/`updateFG30`
        // accumulators). `0xFFFC_0000_0000_0000` is bit-identical to a real
        // `Value::Int(0)` — only the kind mark tells them apart.
        0xfffc_0000_0000_0000u64 as i64,
        0xfffc_0000_dead_beefu64 as i64,
        0xfffe_0000_0000_0001u64 as i64,
        0xffff_ffff_ffff_ffffu64 as i64,
    ];

    /// `push_long` → `pop_long` must be bit-exact for every case, including the
    /// tag-colliding patterns that have no in-slot representation.
    #[test]
    fn long_round_trips_bit_exactly_through_operand_stack() {
        for &v in LONG_ROUND_TRIP_CASES {
            let mut stack = ValueStack::new(4);
            stack.push_long(v).expect("push_long");
            let got = stack.pop_long().expect("pop_long");
            assert_eq!(
                got as u64, v as u64,
                "push_long/pop_long lost bits for 0x{:016x}",
                v as u64
            );
        }
    }

    /// Same via the generic `Value` API, which is what the slow interpreter
    /// paths use.
    #[test]
    fn long_round_trips_through_value_api() {
        for &v in LONG_ROUND_TRIP_CASES {
            let mut stack = ValueStack::new(4);
            stack.push(Value::Long(v)).expect("push");
            match stack.pop().expect("pop") {
                Value::Long(got) => assert_eq!(
                    got as u64, v as u64,
                    "Value::Long round trip lost bits for 0x{:016x}",
                    v as u64
                ),
                other => panic!("expected Long for 0x{:016x}, got {other}", v as u64),
            }
        }
    }

    /// The `push_compact_long` fast path (`lload` forwarding a local without
    /// the `Value` round trip) must also mark the slot, or a collision long
    /// would be sign-extended as an int on the way out.
    #[test]
    fn compact_long_push_marks_kind_and_round_trips() {
        for &v in LONG_ROUND_TRIP_CASES {
            let mut stack = ValueStack::new(4);
            stack.push_compact_long(CompactValue::long(v));
            let (_, kind) = stack.peek_with_kind().expect("peek_with_kind");
            assert_eq!(kind, KIND_LONG, "push_compact_long must mark KIND_LONG");
            assert_eq!(
                stack.pop_long().expect("pop_long") as u64,
                v as u64,
                "push_compact_long/pop_long lost bits for 0x{:016x}",
                v as u64
            );
        }
    }

    /// The exact ambiguity that proves `kinds` is not redundant: the marked
    /// long `0xFFFC_0000_0000_0000` and a real `Value::Int(0)` occupy
    /// bit-identical slots and are told apart ONLY by the kind mark.
    #[test]
    fn collision_long_and_int_zero_are_bit_identical_and_need_the_kind_mark() {
        let collision = 0xfffc_0000_0000_0000u64 as i64;

        let mut as_long = ValueStack::new(4);
        as_long.push_long(collision).expect("push_long");
        let long_bits = as_long.get_compact(0).expect("slot").to_bits();

        let mut as_int = ValueStack::new(4);
        as_int.push(Value::Int(0)).expect("push int");
        let int_bits = as_int.get_compact(0).expect("slot").to_bits();

        assert_eq!(
            long_bits, int_bits,
            "the premise of this test changed: the collision long and Int(0) \
             are no longer bit-identical"
        );

        // Same bits, different kinds, different decoded values.
        assert_eq!(
            as_long.peek_with_kind().expect("peek").1,
            KIND_LONG,
            "long slot must carry KIND_LONG"
        );
        assert_eq!(
            as_int.peek_with_kind().expect("peek").1,
            KIND_UNKNOWN,
            "int slot must stay KIND_UNKNOWN"
        );
        assert_eq!(as_long.pop_long().expect("pop") as u64, collision as u64);
        assert_eq!(as_int.pop_int().expect("pop"), 0);
    }

    /// Recycled buffers must never leak a stale kind mark into a fresh frame:
    /// an over-marked slot would hide a genuine object reference from the GC
    /// root scan. `from_pooled` clears + resizes the tag half for exactly this
    /// reason; this pins the behaviour now that the non-pooled `Frame`
    /// constructors also take buffers from a pool.
    #[test]
    fn from_pooled_clears_stale_kind_marks() {
        let mut dirty = ValueStack::new(8);
        dirty.push_long(-1).expect("push_long");
        dirty.push_double(1.5).expect("push_double");
        let (vals, tags) = dirty.into_inner();

        let mut fresh = ValueStack::from_pooled(vals, tags, 8);
        // Every slot starts UNKNOWN.
        for i in 0..8 {
            assert_eq!(
                fresh.kinds[i], KIND_UNKNOWN,
                "recycled slot {i} kept a stale kind mark"
            );
        }
        // And an object-shaped push is still scannable as a reference.
        fresh.push(Value::Int(7)).expect("push");
        assert_eq!(fresh.pop_int().expect("pop"), 7);
    }

    /// Every double whose raw bits collide with the NaN-box tag space survives
    /// the operand stack bit-exact, by all four doors.
    ///
    /// These are the patterns `probes/F2dCensus.java` measured as lost — an
    /// `f2d` of a negative float NaN with mantissa bits 22 and 21 set — and the
    /// reason they survive is the `KIND_DOUBLE` mark, not the bits.
    #[test]
    fn tag_colliding_double_payloads_survive_the_operand_stack() {
        let patterns: [u64; 5] = [
            0xFFFC_0000_0000_0000,
            0xFFFC_541A_8000_0000, // census sample: in=ffe2a0d4
            0xFFFE_5E0E_8000_0000, // census sample: in=ffb2f074
            0xFFFF_AF30_A000_0000, // census sample: in=fffd7985
            0xFFFF_FFFF_FFFF_FFFF,
        ];
        for bits in patterns {
            let d = f64::from_bits(bits);

            let mut s = ValueStack::new(8);
            s.push_double(d).expect("push_double");
            assert_eq!(s.peek_compact().raw_bits(), bits, "slot bits {bits:#018x}");
            match s.peek() {
                Value::Double(x) => assert_eq!(x.to_bits(), bits, "peek {bits:#018x}"),
                other => panic!("peek of a KIND_DOUBLE slot gave {other:?}"),
            }
            assert_eq!(
                s.pop_double().expect("pop_double").to_bits(),
                bits,
                "pop_double {bits:#018x}",
            );

            let mut s = ValueStack::new(8);
            s.push_double_unchecked(d);
            assert_eq!(s.pop_double_unchecked().to_bits(), bits);

            let mut s = ValueStack::new(8);
            s.push(Value::Double(d)).expect("push");
            match s.pop().expect("pop") {
                Value::Double(x) => assert_eq!(x.to_bits(), bits, "push/pop {bits:#018x}"),
                other => panic!("pop of a Value::Double push gave {other:?}"),
            }

            // dup must copy bits AND kind, or the copy would be re-decoded by
            // its sub-tag and (worse) offered to the root scan.
            let mut s = ValueStack::new(8);
            s.push_double(d).expect("push_double");
            let (cv, k) = s.peek_with_kind().expect("peek_with_kind");
            s.push_with_kind(cv, k).expect("push_with_kind");
            assert_eq!(s.pop_double().expect("pop dup").to_bits(), bits);
            assert_eq!(s.pop_double().expect("pop orig").to_bits(), bits);
        }
    }

    /// The GC half: a tag-colliding double on the operand stack is never a
    /// root, even when its 47-bit payload is a live object's address.
    ///
    /// This is the hazard that made storing doubles verbatim look unsafe. It is
    /// closed by the same `KIND_DOUBLE` gate that already closed it for the
    /// primitive `long` whose bits collide the same way (`CompactValue::long`
    /// has stored verbatim since the BC SM2 fix of 2026-05-28).
    #[test]
    fn scan_object_refs_skips_a_tag_colliding_double_aliasing_a_live_object() {
        use crate::memory::VmHeap;
        use cratonvm_gc::GcBackend;

        let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let obj = heap.alloc_object(cratonvm_types::ClassId::new(0), 0);
        let addr = obj.as_ptr() as u64;
        // Bit-identical to `CompactValue::object(addr)` — but a double.
        let bits = CompactValue::object(addr).raw_bits();
        assert!(CompactValue::double_raw(f64::from_bits(bits)).is_object());

        let mut s = ValueStack::new(8);
        s.push_double(f64::from_bits(bits)).expect("push_double");
        let mut roots = Vec::new();
        s.scan_object_refs(&mut roots, &heap);
        assert!(
            roots.is_empty(),
            "a KIND_DOUBLE slot must never be rooted, got {roots:?}",
        );

        // Control: the same address pushed as a genuine reference IS a root.
        let mut s = ValueStack::new(8);
        s.push(Value::Object(Some(obj))).expect("push object");
        let mut roots = Vec::new();
        s.scan_object_refs(&mut roots, &heap);
        assert_eq!(roots.len(), 1, "the control push must root");
    }
}
