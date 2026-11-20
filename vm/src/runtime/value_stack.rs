// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use std::collections::HashMap;

use crate::error::RuntimeError;
use crate::memory::VmHeap;
use crate::runtime::slot_slab::SlotVec;
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
        if Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
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
// `bc-ec-mod-mododdinverse-investigation.md`).
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

/// gcj j2/pn2: does a slot's kind mark (`None`: no mark) say it holds a
/// primitive `long` / `double`? The conservative probe's skip rule
/// ([`ValueStack::scan_object_refs_conservative`]); pure, for the tests.
#[inline]
fn conservative_slot_is_primitive_kind(kind: Option<u8>) -> bool {
    matches!(kind, Some(KIND_LONG) | Some(KIND_DOUBLE))
}

/// Cached `CRATONVM_LONGROOT_STRICT` gate (bc math-ec 0x4): make the O1
/// hybrid Long|Double smuggle-rooting branch kind-strict — a slot the kind
/// side-array marks as a genuine primitive long/double is never rooted via
/// the loose `is_heap_addr` path.
#[inline]
fn longroot_strict() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())}

/// The minted-only rule of [`ValueStack::scan_object_refs_ruled`] for every
/// production root scan, in BOTH modes: an operand-stack slot kind-marked
/// `long` / `double` roots only a minted smuggled handle, the one value the
/// remap ([`ValueStack::update_object_refs`]) would rewrite. HotSpot never
/// keeps an object alive through a primitive. `--real-jdk` took the rule in
/// interpreter round i1 wave 9 (the census over the regression suite and the
/// jdk-only corpus, four collectors, JIT on and off, found no unminted
/// kind-marked root in either suite); `--compatible` kept the loose rule by a
/// policy AGENTS.md has since retired (both modes target HotSpot), so gcj
/// j1/pn item 3
/// (`docs/feature-designs/real-jdk-mode.md`)
/// gave it the same rule. `CRATONVM_LONGREWRITE_LOOSE=1` is still the escape
/// hatch (it restores the loose root and rewrite together).
pub(crate) const MINTED_ONLY_PRIMITIVE_STACK_ROOTS: bool = true;

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

/// May a Java `long` whose bits `old_bits` name an object that the collector
/// moved to `new_addr` be REWRITTEN to the new address?
///
/// Only when it is a minted smuggled handle (see `memory::smuggled_longs`):
/// the old bits (asked in the same pause, before the registry was remapped)
/// or the relocation target (asked after it was) is registered for THIS heap.
/// A primitive long that merely equals a moved object's address is left
/// alone. `CRATONVM_LONGREWRITE_LOOSE=1` restores rewrite-on-any-hit.
///
/// The one rule for every place that relocates a `long`: the operand-stack
/// update below and the native-call pinned-argument remaps in `vm_exec.rs`,
/// which used to rewrite any pinned `long` argument unconditionally.
/// Called on post-GC paths only.
pub(crate) fn smuggled_long_rewrite_admitted(heap: &VmHeap, old_bits: u64, new_addr: u64) -> bool {
    longrewrite_loose()
        || crate::memory::smuggled_longs::is_minted(heap, old_bits)
        || crate::memory::smuggled_longs::is_minted(heap, new_addr)
}

/// Cached `CRATONVM_DBG_LONGROOT` gate: log every rooting the O1 hybrid
/// branch performs (value bits + kind mark).
#[inline]
fn longroot_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())}

/// Run totals of the `CRATONVM_DBG_LONGROOT` loose-root census in
/// [`ValueStack::scan_object_refs`], for
/// `docs/feature-designs/real-jdk-mode.md`.
/// Counted per root per scan (a word that sits on a stack across ten scans
/// counts ten times). Written only while the census is armed.
mod longroot_census {
    use std::sync::atomic::{AtomicUsize, Ordering};
    /// Roots the loose (untagged `Long` / `Double`) branch produced.
    pub(super) static N: AtomicUsize = AtomicUsize::new(0);
    /// ... of which not a minted handle (`smuggled_longs::is_minted`).
    pub(super) static UNMINTED: AtomicUsize = AtomicUsize::new(0);
    /// ... unminted AND an object start: a producer that could be minted.
    pub(super) static UNMINTED_OBJECT_START: AtomicUsize = AtomicUsize::new(0);
    /// ... unminted AND kind-marked `long` / `double` at push time.
    pub(super) static UNMINTED_PRIMITIVE_KIND: AtomicUsize = AtomicUsize::new(0);
    /// Loose roots that are not an object start (minted or not).
    pub(super) static INTERIOR: AtomicUsize = AtomicUsize::new(0);

    /// Count one loose root. Returns `(index of this root, unminted total,
    /// interior total)` for the running print.
    pub(super) fn count(
        minted: bool,
        object_start: bool,
        primitive_kind: bool,
    ) -> (usize, usize, usize) {
        let unminted = if minted {
            UNMINTED.load(Ordering::Relaxed)
        } else {
            if object_start {
                UNMINTED_OBJECT_START.fetch_add(1, Ordering::Relaxed);
            }
            if primitive_kind {
                UNMINTED_PRIMITIVE_KIND.fetch_add(1, Ordering::Relaxed);
            }
            UNMINTED.fetch_add(1, Ordering::Relaxed) + 1
        };
        let interior = if object_start {
            INTERIOR.load(Ordering::Relaxed)
        } else {
            INTERIOR.fetch_add(1, Ordering::Relaxed) + 1
        };
        (N.fetch_add(1, Ordering::Relaxed), unminted, interior)
    }

    #[cfg(test)]
    mod tests;
}

/// Print the loose-root census totals once, at VM exit, when
/// `CRATONVM_DBG_LONGROOT` is armed; nothing otherwise. The running lines
/// stop at power-of-two totals, so this is the only exact reading.
///
/// `unminted=0` is the go signal for making the scan mint-only;
/// `unminted_object_start` counts roots a registration could fix, and
/// `unminted - unminted_object_start` roots no mint can (interior words).
/// `loose_roots=0` means no untagged slot passed `is_heap_addr` in any scan.
pub fn report_longroot_census_at_exit() {
    use std::sync::atomic::Ordering::Relaxed;
    if !longroot_dbg() {
        return;
    }
    eprintln!(
        "[longroot-summary] loose_roots={} unminted={} unminted_object_start={} \
         unminted_primitive_kind={} interior={}",
        longroot_census::N.load(Relaxed),
        longroot_census::UNMINTED.load(Relaxed),
        longroot_census::UNMINTED_OBJECT_START.load(Relaxed),
        longroot_census::UNMINTED_PRIMITIVE_KIND.load(Relaxed),
        longroot_census::INTERIOR.load(Relaxed),
    );
}

/// Which JVMS 6.5 `dup2_x2` form an operand stack is presenting.
///
/// Produced by [`ValueStack::dup2_x2_form`], which documents why the choice
/// cannot be made from `value1` and `value2` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dup2X2Form {
    /// value1, value2 category 1; value3, value4 category 1.
    Form1,
    /// value1 category 2; value2, value3 category 1.
    Form2,
    /// value1, value2 category 1; value3 category 2.
    Form3,
    /// value1, value2 both category 2.
    Form4,
}

impl Dup2X2Form {
    /// How many entries the form consumes from the top.
    pub const fn consumed(self) -> usize {
        match self {
            Dup2X2Form::Form4 => 2,
            Dup2X2Form::Form2 | Dup2X2Form::Form3 => 3,
            Dup2X2Form::Form1 => 4,
        }
    }

    /// The entries to push back, left to right, as depths-from-top *before*
    /// the pops: `0` is value1, `1` is value2, and so on.
    pub const fn push_order(self) -> &'static [usize] {
        match self {
            Dup2X2Form::Form4 => &[0, 1, 0],
            Dup2X2Form::Form2 => &[0, 2, 1, 0],
            Dup2X2Form::Form3 => &[1, 0, 2, 1, 0],
            Dup2X2Form::Form1 => &[1, 0, 3, 2, 1, 0],
        }
    }
}

#[derive(Debug)]
pub struct ValueStack {
    /// The slot buffer: an owned allocation, or a window of the frame stack's
    /// slot slab (`runtime::slot_slab`; a frame built by the cached install
    /// paths). Every reader indexes it as a slice either way.
    slots: SlotVec<CompactValue>,
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
    /// # And do not reach for the packed-mask shortcut instead
    ///
    /// Collapsing `kinds` / `Frame::local_kinds` from `Vec<u8>` to a packed
    /// 2-bit mask is the obvious cheaper move — three values, and the second
    /// `Vec` costs a bounds check, a cache line and a pooled buffer per frame.
    /// It is not a point fix, and it carries one specific hazard:
    ///
    /// * 43 call sites read the two arrays, plus the GC root scan, freeze/thaw,
    ///   deopt, `snapshot_raw` / `from_snapshot`, and the transmute listed
    ///   above. `Frame::local_kinds` deliberately **is** the pool tuple's
    ///   `Vec<u8>` half, so removing it reshapes the frame pool.
    /// * `max_stack` and `max_locals` are `u16`. An inline `u64`/`u128` mask
    ///   therefore needs a spill path for the tail — and **a fixed-width
    ///   structure that silently stops describing slots past its width is
    ///   precisely the defect this tree has already shipped once**, when
    ///   precise oop maps stopped at 64 locals and said nothing about it.
    ///
    /// Whoever takes this should build the type-map consumer first; it deletes
    /// the array rather than shrinking it, and it has no width to overrun.
    ///
    /// The per-frame *allocation* cost this array used to imply is separately
    /// addressed: every `Frame` constructor now sources both halves from a
    /// buffer pool (`Frame::new_pooled*` from the thread pool, `Frame::new` /
    /// `Frame::new_from_arcs` from the per-OS-thread pool in `runtime::frame`),
    /// so the steady state does no allocation and no zero-fill for either Vec.
    kinds: SlotVec<u8>,
    len: usize,
    max_size: usize,
}

impl ValueStack {
    pub fn new(max_size: usize) -> Self {
        let slots = vec![CompactValue::zero(); max_size];
        Self {
            slots: SlotVec::from_vec(slots),
            kinds: SlotVec::from_vec(vec![KIND_UNKNOWN; max_size]),
            len: 0,
            max_size,
        }
    }

    /// A stack of `max_size` slots in a window of the frame stack's slot slab
    /// (`FrameStack`'s windowed installs). The kind bytes are cleared, exactly
    /// as [`Self::from_pooled`] clears a pooled tag buffer; the slots are not,
    /// for the reason `from_pooled` gives (nothing reads above `len`).
    ///
    /// # Safety
    ///
    /// `vals` and `kinds` must each be the start of `max_size` slots of a live
    /// slab window that this stack's frame owns for as long as the stack is
    /// used (see `SlotVec::window`).
    #[inline(always)]
    pub(crate) unsafe fn from_window(
        vals: std::ptr::NonNull<CompactValue>,
        kinds: std::ptr::NonNull<u8>,
        max_size: usize,
    ) -> Self {
        // `KIND_UNKNOWN` is 0: one memset, the same bytes `from_pooled`'s
        // `clear` + `resize` write.
        std::ptr::write_bytes(kinds.as_ptr(), KIND_UNKNOWN, max_size);
        Self {
            slots: SlotVec::window(vals, max_size, max_size),
            kinds: SlotVec::window(kinds, max_size, max_size),
            len: 0,
            max_size,
        }
    }

    /// Is this stack a window of the frame stack's slot slab?
    #[inline(always)]
    pub(crate) fn is_windowed(&self) -> bool {
        self.slots.is_window()
    }

    /// For a windowed stack, `(address of slot len, address one past the
    /// window)`: where the next push would land and where the frame's window
    /// ends. `None` for an owned stack. Addresses only; the slab derives the
    /// pointers it writes through from its own chunk (stage 2 of the
    /// contiguous interpreter stack, `FrameStack::push_cached_compact_overlapping`).
    #[inline(always)]
    pub(crate) fn window_top_and_end(&self) -> Option<(usize, usize)> {
        if !self.slots.is_window() {
            return None;
        }
        const SLOT: usize = std::mem::size_of::<CompactValue>();
        let base = self.slots.as_ptr() as usize;
        Some((base + self.len * SLOT, base + self.slots.capacity() * SLOT))
    }

    /// Copy a windowed stack into owned buffers, contents and depth unchanged
    /// (a frame leaving its `FrameStack` by value). No-op for an owned stack.
    pub(crate) fn detach_from_slab(&mut self) {
        self.slots.make_owned();
        self.kinds.make_owned();
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
            slots: SlotVec::from_vec(slots),
            kinds: SlotVec::from_vec(kinds),
            len: 0,
            max_size,
        }
    }

    /// Re-arm this stack for a new frame of `max_size` slots, reusing the
    /// allocation it already has.
    ///
    /// The same contract as [`Self::from_pooled`], which this replaces on the
    /// frame-slot-reuse path: a slot buffer that is already long enough is
    /// kept as it is (nothing above `len` is ever read, and every push writes
    /// its slot before `len` admits it), while `kinds` is cleared because a
    /// stale `KIND_LONG` mark is the one direction that is not safe — it would
    /// tell `scan_object_refs` to skip a slot that holds a reference.
    ///
    /// The buffers are taken out as `Vec`s and put back. A slab window yields
    /// an empty `Vec` there (`SlotVec::take_vec`), so a retired frame's window
    /// -- memory the slab may already have handed to another frame -- is
    /// replaced by a fresh owned buffer rather than written.
    pub fn reset_in_place(&mut self, max_size: usize) {
        // Owned buffers with room are rewritten where they are (interpreter
        // round i1 wave 32), leaving the bytes the round trip below leaves:
        // slots past the old length zeroed, every kind byte cleared.
        if self.slots.owned_capacity() >= max_size && self.kinds.owned_capacity() >= max_size {
            let old_len = self.slots.len();
            // SAFETY: both buffers are owned with room for `max_size` slots;
            // the slots below `old_len` are initialised and the rest are
            // written here before the length admits them.
            unsafe {
                let vals = self.slots.as_mut_ptr();
                for i in old_len..max_size {
                    vals.add(i).write(CompactValue::zero());
                }
                self.slots.set_len(max_size);
                std::ptr::write_bytes(self.kinds.as_mut_ptr(), KIND_UNKNOWN, max_size);
                self.kinds.set_len(max_size);
            }
            self.len = 0;
            self.max_size = max_size;
            return;
        }
        let mut slots = self.slots.take_vec();
        if slots.len() < max_size {
            slots.resize(max_size, CompactValue::zero());
        } else {
            slots.truncate(max_size);
        }
        let mut kinds = self.kinds.take_vec();
        kinds.clear();
        kinds.resize(max_size, KIND_UNKNOWN);
        self.slots = SlotVec::from_vec(slots);
        self.kinds = SlotVec::from_vec(kinds);
        self.len = 0;
        self.max_size = max_size;
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
        // A windowed stack has no allocation to hand over: both halves come
        // back empty (`SlotVec::into_vec`).
        let mut kinds = self.kinds.into_vec();
        kinds.clear();
        (compact_vec_to_u64(self.slots.into_vec()), kinds)
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
        let mut kinds = self.kinds.take_vec();
        kinds.clear();
        let slots = self.slots.take_vec();
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
            "a STALE reference is being pushed onto the operand stack — the collector moved \
             this object and nothing has been allocated at the old address since. The \
             backtrace names the VM code that produced it."
        );
    }

    #[inline(always)]
    fn check_vacated_compact(cv: &CompactValue) {
        // See `check_dead_push` — this is the compact half, and it is the path
        // `dup`, a local reload and the cached field/return producers take, so
        // leaving it out would blind the probe to exactly the values that reach
        // an invoke without ever touching a `Value`.
        //
        // Both gates are tested BEFORE the slot is decoded: this runs on every
        // `aload`, `dup` and quickened field/array load, and in a default run
        // both instruments are off, so an unarmed push should pay two
        // predicted loads and nothing else — not a NaN-box decode per push
        // that only an armed run can use.
        if !Self::dead_push_enabled() && !cratonvm_gc::gc_quiescence::vacated_frames_enabled() {
            return;
        }
        Self::check_vacated_compact_armed(cv);
    }

    /// The armed half of [`Self::check_vacated_compact`]; each instrument
    /// still re-checks its own gate, exactly as before the split.
    #[cold]
    #[inline(never)]
    fn check_vacated_compact_armed(cv: &CompactValue) {
        if cv.is_object() {
            if let Some(ptr) = cv.as_object_ptr() {
                Self::check_dead_push(ptr as usize, "push_compact");
            }
        }
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
    /// `CRATONVM_DBG_DEADREF_STORE`: a reference PUSHED onto the operand stack
    /// that names no live object.
    ///
    /// The arm `check_vacated_push` below cannot see this. That one reads the
    /// vacated ledger, which forgets an address the moment the allocator
    /// re-issues it, and a stale reference on this workload is pushed after
    /// re-issue — so it reported zero on every run while the operand stack was
    /// demonstrably carrying a dead value into an invoke.
    ///
    /// An operand-stack slot is already a GC root, so a hit here is one of two
    /// things and the backtrace says which: something produced a dead value (a
    /// native return, a field read), or the frame-root remap missed the slot
    /// this value was copied from. Both are defects; they have different fixes.
    #[cold]
    fn report_dead_push(addr: usize, reason: &'static str, site: &'static str) {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 8 {
            return;
        }
        eprintln!(
            "[deadref-push] {reason} {site}: 0x{addr:x} pushed onto the operand stack names no \
             live object. caller:
{:?}",
            std::backtrace::Backtrace::force_capture(),
        );
    }

    /// `CRATONVM_DBG_DEADREF_STORE`, latched.
    #[inline(always)]
    fn dead_push_enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| cratonvm_types::flags().gc.dbg_deadref_store)
    }

    #[inline]
    fn check_dead_push(addr: usize, site: &'static str) {
        if !Self::dead_push_enabled() {
            return;
        }
        if let Some(reason) = cratonvm_gc::gen_heap::dead_young_ref_reason_global(addr) {
            Self::report_dead_push(addr, reason, site);
        }
    }

    fn check_vacated_push(value: &Value) {
        if let Value::Object(Some(o)) = value {
            Self::check_dead_push(o.as_ptr() as usize, "push");
        }
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

    /// Classify the top of the stack for `dup2_x2` (JVMS 6.5).
    ///
    /// # Why this is a stack query and not a decision over popped entries
    ///
    /// The four forms consume different numbers of entries, and `value3`'s
    /// category is the *only* thing separating FORM 1 from FORM 3. An
    /// implementation that pops `value1` and `value2` first and branches on
    /// those two alone cannot tell them apart, so it falls back on "pop a
    /// fourth" -- which is wrong twice over. On a stack exactly three entries
    /// deep the fourth pop underflows `len` to `usize::MAX`; on a deeper one it
    /// does not fault at all, it silently lifts a live value from *beneath* the
    /// shuffle and leaves it duplicated in the result. Both interpreter copies
    /// of this opcode carried that bug until 2026-09-16, which is why the
    /// decision now lives in one place with its own tests.
    ///
    /// Returns `None` when the stack presents no form JVMS defines -- fewer
    /// entries than the shape needs, or a category-1 `value1` over a
    /// category-2 `value2`, which no form admits. Verified bytecode reaches
    /// neither; refusing keeps an unverifiable shape from rewriting entries the
    /// shuffle was never meant to touch.
    #[inline]
    pub fn dup2_x2_form(&self) -> Option<Dup2X2Form> {
        // `depth` is measured from the top: 0 is value1, 1 is value2, ...
        let cat2_at = |depth: usize| -> Option<bool> {
            let i = self.len.checked_sub(depth + 1)?;
            Some(Self::is_cat2_kind(self.kinds[i], self.slots[i]))
        };
        match (cat2_at(0)?, cat2_at(1)?) {
            (true, true) => Some(Dup2X2Form::Form4),
            // FORM 2 also requires value3 category 1.
            (true, false) => (!cat2_at(2)?).then_some(Dup2X2Form::Form2),
            (false, true) => None,
            (false, false) => {
                if cat2_at(2)? {
                    Some(Dup2X2Form::Form3)
                } else {
                    // FORM 1 also requires value4 category 1.
                    (!cat2_at(3)?).then_some(Dup2X2Form::Form1)
                }
            }
        }
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
    pub fn peek_compact_at(&self, depth: usize) -> CompactValue {
        debug_assert!(self.len > depth, "stack underflow in peek_compact_at");
        self.slots[self.len - 1 - depth]
    }

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

    /// The top slot's object address, read raw: `Some` only for a slot
    /// tagged as a non-null object reference and not kind-marked as a long
    /// or a double (the marks [`Self::value_at`] honours). No `Value` decode
    /// and no `ObjectRef`: the caller validates the address against the heap
    /// (the cast-site hit, `VmConfig::cast_hit_raw`; interpreter round i1
    /// wave 51, lane L7). `None` for an empty stack.
    #[inline(always)]
    pub fn peek_object_ptr(&self) -> Option<u64> {
        let i = self.len.checked_sub(1)?;
        if self.kinds[i] != KIND_UNKNOWN {
            return None;
        }
        self.slots[i].as_object_ptr()
    }

    /// Overwrite the top slot with an int: `instanceof`'s answer replacing
    /// its operand, where a pop and a push decoded and re-encoded the slot
    /// (`VmConfig::cast_hit_raw`). An int needs no kind mark and no push
    /// diagnostic (those inspect references only).
    ///
    /// # Panics
    /// Panics if the stack is empty.
    #[inline(always)]
    pub fn replace_top_with_int(&mut self, v: i32) {
        debug_assert!(self.len > 0, "stack underflow in replace_top_with_int");
        let i = self.len - 1;
        self.kinds[i] = KIND_UNKNOWN;
        self.slots[i] = CompactValue::int(v);
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
        // bc-ec-mod-mododdinverse-investigation.md.
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

    /// Whether one more slot fits under `max_size` (a checked push would
    /// succeed). For a fused group that pushes after a side effect and must
    /// decline BEFORE it (interpreter round i3 wave 71, lane L7: the fused
    /// field increment whose value is used).
    #[inline(always)]
    pub fn has_room(&self) -> bool {
        self.len < self.max_size
    }

    /// Slots this stack has allocated (each with its kind byte) — the memory
    /// it holds, not the verifier's `max_stack`. For the frame-shape census.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    // ── GC scanning and pointer update helpers ─────────────────────────

    /// Collect all non-null object references from the stack for GC root scanning.
    ///
    /// Scanned slot kinds:
    /// - NaN-boxed object slots (`CompactValue::is_object`) not kind-marked
    ///   `KIND_LONG` / `KIND_DOUBLE` (a primitive whose bits collide with the
    ///   object sub-tag is never rooted; see `applogs/letsgo-segv-L1-diagnosis.md`).
    /// - `Long` / `Double` slots whose bits are an 8-aligned address that
    ///   `heap.is_heap_addr` accepts (region membership, not an object start):
    ///   the "O1 hybrid" loose branch for JNI `jobject`-as-`jlong` smuggling. It
    ///   roots primitive longs too, while [`Self::update_object_refs`] rewrites
    ///   only minted handles; `CRATONVM_LONGROOT_STRICT` skips kind-marked
    ///   primitives. Why it is still loose, and the census that decides it:
    ///   `docs/feature-designs/real-jdk-mode.md`.
    pub fn scan_object_refs(&self, roots: &mut Vec<ObjectRef>, heap: &VmHeap) {
        self.scan_object_refs_in(0..self.len, roots, heap, true, false);
    }

    /// [`Self::scan_object_refs`] with the loose branch's rule chosen by the
    /// caller. `minted_only_primitives = true` roots a slot kind-marked
    /// `KIND_LONG` / `KIND_DOUBLE` only when its bits are a minted handle
    /// (`smuggled_longs::is_minted`), the rule [`Self::update_object_refs`]
    /// already applies to the rewrite and `Frame::scan_local_objects` to long
    /// locals; `CRATONVM_LONGREWRITE_LOOSE=1` turns it off again. `false` is
    /// exactly [`Self::scan_object_refs`]. The production root scans pass
    /// [`MINTED_ONLY_PRIMITIVE_STACK_ROOTS`] in both modes.
    pub fn scan_object_refs_ruled(
        &self,
        roots: &mut Vec<ObjectRef>,
        heap: &VmHeap,
        minted_only_primitives: bool,
    ) {
        self.scan_object_refs_in(0..self.len, roots, heap, true, minted_only_primitives);
    }

    /// [`Self::scan_object_refs_ruled`] leaving out every slot `excluded`
    /// marks (interpreter round i6 wave 122, lane L7;
    /// `CRATONVM_INTERP_PRECISE_OOP_MAPS`, through
    /// `Frame::scan_stack_objects_mapped`): each other slot gets exactly the
    /// decision the whole-stack scan gives it, census included. A slot past
    /// `excluded`'s length is scanned.
    pub(crate) fn scan_object_refs_ruled_except(
        &self,
        roots: &mut Vec<ObjectRef>,
        heap: &VmHeap,
        minted_only_primitives: bool,
        excluded: &[bool],
    ) {
        for i in 0..self.len {
            if excluded.get(i).copied().unwrap_or(false) {
                continue;
            }
            self.scan_object_refs_in(i..i + 1, roots, heap, true, minted_only_primitives);
        }
    }

    /// Whether slot `i` carries the `long` / `double` kind mark, or holds a
    /// `Long` / `Double`-tagged value whatever its mark (a slot past the depth
    /// reads `false`). For the precise stack scan's rule, which never leaves
    /// such a slot out: its smuggled-handle root is the scan's own
    /// (`scan_object_refs_in`'s `CompactTag::Long | Double` branch, which
    /// does not ask the mark), and a `KIND_UNKNOWN` slot can hold a long
    /// (wave 123 L9 review: the kind mark alone let a smuggled handle pushed
    /// by an unmarking path be dropped).
    pub(crate) fn slot_is_wide(&self, i: usize) -> bool {
        i < self.len
            && (self.kinds[i] == KIND_LONG
                || self.kinds[i] == KIND_DOUBLE
                || matches!(self.slots[i].tag(), CompactTag::Long | CompactTag::Double))
    }

    /// Does slot `i` contribute a root to [`Self::scan_object_refs`]? The
    /// same decision, asked one slot at a time, without the
    /// `CRATONVM_DBG_LONGROOT` census (so asking twice does not double-count).
    /// For the precise-oop-map shadow comparison (`local_liveness::oop_map_shadow`).
    pub(crate) fn slot_is_scanned_root(&self, i: usize, heap: &VmHeap) -> bool {
        if i >= self.len {
            return false;
        }
        let mut one = Vec::new();
        // The rule every production root scan passes (wave 124: the shadow
        // used the loose rule and reported a primitive long's loose root,
        // which the collector never takes, as an `extra`).
        self.scan_object_refs_in(
            i..i + 1,
            &mut one,
            heap,
            false,
            MINTED_ONLY_PRIMITIVE_STACK_ROOTS,
        );
        !one.is_empty()
    }

    /// The body of [`Self::scan_object_refs`] over `range` (always within
    /// `0..self.len`). `census` arms the `CRATONVM_DBG_LONGROOT` print;
    /// `minted_only_primitives` is [`Self::scan_object_refs_ruled`]'s rule.
    #[inline(always)]
    fn scan_object_refs_in(
        &self,
        range: std::ops::Range<usize>,
        roots: &mut Vec<ObjectRef>,
        heap: &VmHeap,
        census: bool,
        minted_only_primitives: bool,
    ) {
        for i in range {
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
                        if census && longroot_dbg() {
                            // Census for
                            // docs/feature-designs/real-jdk-mode.md:
                            // `update_object_refs` remaps only MINTED handles,
                            // so every root logged here with `minted=false` is
                            // either a primitive being rooted or an
                            // unregistered smuggle path. `object_start=false`
                            // (with `interior_total`) is the page's blocker 2:
                            // a word no mint probe (`is_object_address`) can
                            // ever register, which a mint-only scan would drop.
                            // Run totals: `report_longroot_census_at_exit`.
                            let minted =
                                crate::memory::smuggled_longs::is_minted(heap, cv.to_bits());
                            let object_start = heap.is_object_address(p).is_some();
                            let (k, unminted, interior) = longroot_census::count(
                                minted,
                                object_start,
                                self.kinds[i] == KIND_LONG || self.kinds[i] == KIND_DOUBLE,
                            );
                            if k < 40
                                || (!minted && unminted.is_power_of_two())
                                || (!object_start && interior.is_power_of_two())
                            {
                                eprintln!(
                                    "[longroot] #{k} rooting tagged-{:?} slot[{i}] bits=0x{:x} kind={} minted={minted} object_start={object_start} unminted_total={unminted} interior_total={interior} (loose is_heap_addr pass)",
                                    cv.tag(),
                                    cv.to_bits(),
                                    self.kinds[i],
                                );
                            }
                        }
                        // The minted-only rule, after the census on purpose:
                        // the census keeps describing what the LOOSE rule
                        // roots, which is the population this rule drops.
                        if minted_only_primitives
                            && (self.kinds[i] == KIND_LONG || self.kinds[i] == KIND_DOUBLE)
                            && !longrewrite_loose()
                            && !crate::memory::smuggled_longs::is_minted(heap, cv.to_bits())
                        {
                            continue;
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
    ///
    /// gcj j2/pn2 (`CRATONVM_GEN_CONSERVATIVE_LOCALS_SKIP_PRIMITIVE_KINDS`,
    /// default ON (`=0` kills), Generational only; the locals twin is
    /// `Frame::scan_locals_conservative`): a `KIND_LONG` / `KIND_DOUBLE` slot
    /// holds that primitive (the kind mark the precise scan's minted-only rule
    /// already trusts), so it roots only a MINTED smuggled handle. Without it
    /// a `long` whose value equals a young object's address roots (and, under
    /// selective promotion, pins) that object; HotSpot never does.
    pub fn scan_object_refs_conservative(&self, roots: &mut Vec<ObjectRef>, heap: &VmHeap) {
        let skip_primitive_kinds = heap.is_generational()
            && crate::runtime::frame::conservative_locals_skip_primitive_kinds_enabled();
        for i in 0..self.len {
            let cv = self.slots[i];
            if skip_primitive_kinds
                && conservative_slot_is_primitive_kind(self.kinds.get(i).copied())
            {
                let bits = cv.to_bits();
                if crate::memory::smuggled_longs::is_minted(heap, bits) {
                    if let Some(obj) = heap.is_object_address(bits as usize) {
                        roots.push(obj);
                    }
                }
                continue;
            }
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

    /// Encode current operand stack length and raw `(bits, kind)` pairs
    /// for [`crate::native::jni::InNativeRepeat`] above-chain matching.
    #[inline]
    pub(crate) fn push_repeat_key_words(&self, out: &mut Vec<u64>) {
        out.push(self.len as u64);
        for i in 0..self.len {
            out.push(self.slots[i].raw_bits());
            out.push(self.kinds[i] as u64);
        }
    }

    #[inline]
    pub(crate) fn matches_repeat_key_words(&self, words: &[u64], pos: &mut usize) -> bool {
        if words.get(*pos).copied() != Some(self.len as u64) {
            return false;
        }
        *pos += 1;
        let need = self.len * 2;
        if words.len().saturating_sub(*pos) < need {
            return false;
        }
        for i in 0..self.len {
            if words[*pos] != self.slots[i].raw_bits()
                || words[*pos + 1] != (self.kinds[i] as u64)
            {
                return false;
            }
            *pos += 2;
        }
        true
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

    /// Update object references after GC using the pointer map.
    ///
    /// - NaN-boxed object slots (`is_object`) that are not kind-marked
    ///   primitive: remapped via the `pointer_map` (membership, not a probe of
    ///   the from-space address).
    /// - `Long` / `Double` slots (the JNI long-as-jobject pattern): remapped
    ///   only when `smuggled_long_rewrite_admitted` says the bits are a
    ///   MINTED handle (`CRATONVM_LONGREWRITE_LOOSE=1` restores
    ///   rewrite-on-any-map-hit), and skipped under the same
    ///   `CRATONVM_LONGROOT_STRICT` kind-strict gate as the scan. This is
    ///   narrower than [`Self::scan_object_refs`]'s loose root rule, on
    ///   purpose: a primitive whose bits equal a moved object's from-space
    ///   address is rooted but never rewritten.
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
                        if !smuggled_long_rewrite_admitted(heap, bits, new_addr as u64) {
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
            slots: SlotVec::from_vec(slots),
            kinds: SlotVec::from_vec(kinds),
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
mod tests;
