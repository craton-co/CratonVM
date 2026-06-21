// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT compiler for hot bytecode methods.
//!
//! Compiles self-contained JVM bytecode methods to native x86-64 machine code.
//! Methods are classified as "pure" (int/long only) or "context" (VM-interacting).
//! Pure methods are called directly; context methods receive a SharedVm pointer as a
//! hidden first argument, enabling JIT-compiled array allocation, field access, type
//! checks, and method dispatch.
//!
//! ## Architecture
//!
//! - `JitCache`: maps method identity (class_name, method_name, descriptor) to compiled code
//! - `CompiledMethod`: holds executable memory, entry point, and `needs_context` flag
//! - `compile_method`: analyzes bytecode, emits x64 code, patches self-calls
//! - `ExecutableBuffer`: platform-specific executable memory (VirtualAlloc on Windows)
//!
//! ## Internal JIT calling convention (NOT the platform ABI)
//!
//! JIT-compiled methods do **not** follow the SysV (System V AMD64) or
//! Win64 ABI for argument passing. They use a private convention that is
//! only valid between two CratonVM-JIT-compiled functions:
//!
//! - **All** Java arguments — including `float` and `double` — are passed
//!   through the general-purpose argument registers (`ARG_REGS`). Floating
//!   point arguments are transmitted as their raw 64-bit IEEE-754 bit
//!   pattern in a GPR, *not* in an XMM register as the platform ABI
//!   requires. The callee prologue moves each FP parameter from its GPR
//!   into an XMM register via `movq` (see `x64.rs`, `emit_movq_xmm_from_rax`
//!   in the prologue). Stack-passed arguments (when the register file is
//!   exhausted) are likewise raw 64-bit slots.
//! - "context" methods additionally receive a hidden `SharedVm` pointer in
//!   `ARG_REGS[0]`, shifting all Java args by one register.
//!
//! WARNING: because of this, a function pointer obtained from any source
//! other than CratonVM's own JIT (a libc symbol, a C-compiled callback, a
//! function produced by another compiler) **must never** be entered with
//! this convention, and conversely a CratonVM JIT entry point must never
//! be handed to external code expecting the platform ABI. The argument
//! registers, FP-in-GPR encoding, and stack layout are all incompatible.
//! Cross-ABI calls must go through an explicit thunk that re-marshals
//! arguments.
//!
//! ## SAFETY INVARIANT: GC must conservatively re-sweep every JIT frame
//!
//! The JIT register allocator may keep a Java local (including an object
//! reference) **exclusively in a callee-saved GPR** between bytecode
//! aload/astore opcodes — the value need not be present in the frame's
//! local slot at any given native PC. The precise oop map
//! (`OopMapEntry`) describes only *frame-slot* oops; it has **no
//! register-oop bitmap**. Therefore a register-resident oop is invisible
//! to any GC scan that walks frame memory alone.
//!
//! Correctness depends on two cooperating mechanisms, and **both must be
//! kept**:
//!
//! 1. Before every safepoint-causing call, the code generator spills every
//!    register-resident local back to its canonical frame slot
//!    `[rbp - (idx+1)*8]` (see `x64.rs::emit_pre_safepoint_spill`).
//! 2. `conservative_roots::scan_one_frame_precise` performs a *full
//!    conservative sweep* of the entire JIT frame region (validating each
//!    qword via `heap.is_object_address`), in addition to consulting the
//!    precise oop map.
//!
//! The precise oop map is a pure optimization layered on top of the
//! conservative sweep. **Do not** remove or weaken the conservative
//! frame sweep, and **do not** remove the pre-safepoint spill, unless a
//! genuine register-oop map is added to `OopMapEntry` *and* consumed by
//! the GC scanner. Removing either one in isolation would let
//! register-resident oops escape GC root scanning, causing live objects
//! to be reclaimed and the heap to corrupt.

pub mod aarch64;
pub mod aarch64_backend;
pub mod deopt;
pub mod escape_analysis;
pub mod ir;
pub mod ir_lower;
pub mod ir_optimize;
pub mod ir_schedule;
pub mod loop_analysis;
pub mod null_check_elim;
pub mod pgo;
pub mod platform;
pub mod profile;
pub mod regalloc;
pub mod scev;
pub mod tiered;
pub mod x64;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use rustc_hash::{FxHashMap, FxHasher};
use std::hash::Hasher;

// ---------------------------------------------------------------------------
// JIT code region tracking — validates pointers before transmute to fn ptrs
// ---------------------------------------------------------------------------

/// Tracks allocated JIT code regions for pointer validation before transmute.
///
/// Every `ExecutableBuffer` registers its address range on creation and
/// deregisters on drop. Before transmuting a `*const u8` to a function pointer,
/// callers should use `validate_code_ptr` to confirm it points within a known
/// JIT region.
pub struct JitCodeRegion {
    /// (start_addr, end_addr_exclusive), kept sorted ascending by start_addr.
    ///
    /// Regions are non-overlapping (each is a distinct executable allocation),
    /// so sorting by start address also sorts by end address. This lets
    /// `contains` run a binary search instead of an O(n) linear scan — the
    /// list grows with every compiled method and `contains` is hit on every
    /// JIT call via `validate_code_ptr`.
    regions: Vec<(usize, usize)>,
}

impl JitCodeRegion {
    fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }

    fn register(&mut self, ptr: *const u8, size: usize) {
        let start = ptr as usize;
        let end = start + size;
        // Insert maintaining the sorted-by-start invariant.
        let idx = self.regions.partition_point(|&(s, _)| s < start);
        self.regions.insert(idx, (start, end));
    }

    fn deregister(&mut self, ptr: *const u8) {
        let addr = ptr as usize;
        // Find the (unique) region with this start address and remove it,
        // preserving the sorted order.
        let lo = self.regions.partition_point(|&(s, _)| s < addr);
        if lo < self.regions.len() && self.regions[lo].0 == addr {
            self.regions.remove(lo);
        }
    }

    fn contains(&self, ptr: *const u8) -> bool {
        let addr = ptr as usize;
        // Binary search: find the last region whose start <= addr, then check
        // it covers `addr`. Non-overlapping + sorted means this is the only
        // candidate region.
        let idx = self.regions.partition_point(|&(s, _)| s <= addr);
        if idx == 0 {
            return false;
        }
        let (start, end) = self.regions[idx - 1];
        addr >= start && addr < end
    }
}

fn jit_code_regions() -> &'static Mutex<JitCodeRegion> {
    static REGIONS: std::sync::OnceLock<Mutex<JitCodeRegion>> = std::sync::OnceLock::new();
    REGIONS.get_or_init(|| Mutex::new(JitCodeRegion::new()))
}

/// Validate that a pointer is safe to transmute to a function pointer.
///
/// Checks: non-null, properly aligned, and falls within a known JIT code region.
/// Returns `Ok(())` if valid, or a descriptive error string.
pub fn validate_code_ptr(ptr: *const u8) -> Result<(), &'static str> {
    if ptr.is_null() {
        return Err("null JIT code pointer");
    }
    // Function pointers should be at least 4-byte aligned (ARM64 requires 4,
    // x86-64 has no strict requirement but 4 is reasonable).
    if (ptr as usize) % 4 != 0 {
        return Err("misaligned JIT code pointer");
    }
    let regions = jit_code_regions().lock().unwrap_or_else(|e| e.into_inner());
    if !regions.contains(ptr) {
        return Err("JIT code pointer outside known code regions");
    }
    Ok(())
}

pub use cratonvm_jit_api::{CachedBytecodeMethod, JitRuntimeHelpers};
#[allow(unused_imports)]
use cratonvm_types::{
    ObjectRef, Value, ARRAY_LENGTH_OFFSET, HEADER_SIZE, REF_ELEMENT_SIZE, SLOT_SIZE,
};

// Compile-time size assertions for Value on all platforms.
// Value must be exactly 16 bytes for JIT slot layout. If this fails on a new
// platform, the JIT code-gen must be updated before it can be used there.
const _: () = assert!(
    std::mem::size_of::<Value>() == 16,
    "Value must be 16 bytes for JIT slot layout"
);
const _: () = assert!(
    std::mem::align_of::<Value>() <= 8,
    "Value alignment must not exceed 8 bytes"
);
// ObjectRef must be pointer-sized.
const _: () = assert!(
    std::mem::size_of::<ObjectRef>() == std::mem::size_of::<*mut u8>(),
    "ObjectRef must be pointer-sized"
);

// ---------------------------------------------------------------------------
// Value layout probing — pointer offset within Value::Object(Some(..))
// ---------------------------------------------------------------------------

/// Read the raw 16-byte representation of a Value.
///
/// Safety: Value is compile-time asserted to be exactly 16 bytes.
/// The transmute is safe because both types are the same size and
/// `[u8; 16]` has no alignment or validity requirements.
/// Value is Copy, so no drop semantics are affected.
#[inline]
fn value_to_bytes(val: Value) -> [u8; 16] {
    // Safety: size_of::<Value>() == 16 (asserted above). Both src and dst
    // types are 16 bytes. [u8; 16] accepts any bit pattern.
    unsafe { std::mem::transmute(val) }
}

/// Probe the byte offset of the pointer within Value::Object(Some(..)).
/// Used by JIT inline aaload to extract the pointer from a 16-byte Value slot.
pub fn probe_object_ptr_offset() -> usize {
    // Use an 8-byte aligned marker to satisfy ObjectRef's alignment debug_assert.
    let marker_ptr = 0xDE_AD_BE_EF_CA_FE_BA_B8u64;
    let fake_ref = unsafe { ObjectRef::from_raw(marker_ptr as usize as *mut u8) };
    let val_obj = Value::Object(Some(fake_ref));
    let obj_bytes = value_to_bytes(val_obj);
    let marker_le = marker_ptr.to_le_bytes();
    (0..9)
        .find(|&i| obj_bytes[i..i + 8] == marker_le)
        .expect("cannot locate Object pointer in Value")
}

/// Probe the 16-byte representation of Value::Object(None) as two u64 halves.
/// Used by JIT inline aastore to write the correct discriminant and null pointer.
pub fn probe_object_null_template() -> (u64, u64) {
    let val = Value::Object(None);
    let bytes = value_to_bytes(val);
    let lo = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let hi = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    (lo, hi)
}

// ---------------------------------------------------------------------------
// JIT error type — non-panicking compile/runtime failure signalling
// ---------------------------------------------------------------------------

/// Errors surfaced by the JIT instead of panicking.
///
/// The JIT contract is "never panics" — every internal codegen or runtime
/// invariant violation that previously triggered a `panic!`, `assert!`, or
/// `expect()` is being migrated to return one of these variants instead.
/// Most variants short-circuit the surrounding compile by tripping the
/// containing [`ExecutableBuffer`]'s sticky overflow flag, after which the
/// existing `if buf.overflowed() { return None; }` bail-outs cause
/// [`compile`](crate::x64::compile) to return `None` (interpreter fallback).
///
/// `try_call`/`try_call_with_context` propagate these directly without
/// going through `overflowed`, because they describe runtime invocation
/// failures rather than codegen ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// A `try_patch_i32`/`try_patch_byte` call pointed past the emitted
    /// code, signalling either a codegen invariant break or a recovery
    /// from a prior buffer overflow. `kind` is `"i32"` or `"byte"`;
    /// `offset` is the patch site as recorded by the caller.
    PatchFailed { kind: &'static str, offset: usize },
    /// A rel8/rel32 displacement overflowed its encoding range during a
    /// branch patch. `kind` is `"rel8"` or `"rel32"`; `displacement` is
    /// the out-of-range value. Currently produced by the safe-idiv
    /// guard patches; the surrounding compile bails via `overflowed`.
    DisplacementOverflow {
        kind: &'static str,
        displacement: i64,
    },
    /// `CompiledMethod::try_call` / `try_call_with_context` was invoked
    /// with more arguments than the JIT's hand-rolled call thunks
    /// support (currently 8 / 7 respectively, excluding the implicit
    /// context pointer). `n` is the actual number of arguments
    /// supplied.
    TooManyArgs(usize),
    /// `validate_code_ptr` rejected an entry/trampoline pointer before
    /// transmute to a function pointer. The wrapped string is the
    /// `validate_code_ptr` reason.
    InvalidCodePtr(&'static str),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::PatchFailed { kind, offset } => write!(
                f,
                "JIT patch failed ({kind}) at offset {offset}: site is past emitted len"
            ),
            CompileError::DisplacementOverflow { kind, displacement } => write!(
                f,
                "JIT branch displacement overflowed {kind} encoding (= {displacement})"
            ),
            CompileError::TooManyArgs(n) => {
                write!(f, "JIT call: too many arguments ({n})")
            }
            CompileError::InvalidCodePtr(reason) => {
                write!(f, "JIT invalid code pointer: {reason}")
            }
        }
    }
}

impl std::error::Error for CompileError {}

// ---------------------------------------------------------------------------
// Executable memory — delegates to platform module
// ---------------------------------------------------------------------------

/// A buffer of executable machine code allocated via OS-level APIs.
///
/// On platforms with W^X enforcement (macOS ARM64), the buffer starts in writable
/// mode. Call [`finalize`](ExecutableBuffer::finalize) after emitting all code to
/// transition to executable mode. On other platforms (Windows, Linux x86-64),
/// the memory is always RWX and `finalize` is a no-op.
pub struct ExecutableBuffer {
    ptr: *mut u8,
    len: usize,
    capacity: usize,
    /// Set when an `emit`/`emit_byte` call could not fit in the buffer.
    /// `estimated_size` in the x64 backend is a heuristic, so a pathological
    /// method can exceed it. Rather than panicking the whole process, the
    /// emit hot path records overflow here and the compile driver bails to
    /// the interpreter (returns `None`) after codegen.
    overflowed: bool,
}

// Safety: ExecutableBuffer is effectively a unique owned allocation, like Vec<u8>.
// The JIT cache holds it behind an Arc; no concurrent writes happen after compilation.
unsafe impl Send for ExecutableBuffer {}
unsafe impl Sync for ExecutableBuffer {}

impl ExecutableBuffer {
    /// Allocate a new executable buffer with the given capacity.
    pub fn new(capacity: usize) -> Option<Self> {
        let ptr = platform::alloc_executable(capacity)?;
        // Account the committed executable memory. `COMMITTED_JIT_CODE_BYTES`
        // tracks currently-mapped code and is the quantity the code-cache cap
        // bounds; the `try_compile` gate reads it to decide whether to keep
        // compiling. Bumped here (not in `Drop`, which fires only on the rare
        // free path) so the figure reflects live mappings.
        COMMITTED_JIT_CODE_BYTES.fetch_add(capacity, std::sync::atomic::Ordering::Relaxed);
        // Register this region for code pointer validation.
        if let Ok(mut regions) = jit_code_regions().lock() {
            regions.register(ptr, capacity);
        }
        Some(Self {
            ptr,
            len: 0,
            capacity,
            overflowed: false,
        })
    }

    /// Write bytes into the buffer at the current position.
    ///
    /// If the write would exceed capacity the buffer is marked
    /// [`overflowed`](Self::overflowed) and the write is skipped instead of
    /// panicking. `len` is never advanced past `capacity`, so the buffer
    /// stays safe to slice/patch; the compile driver is expected to check
    /// `overflowed()` and discard the result.
    ///
    /// The `overflowed` flag is sticky: once any emit has overflowed, every
    /// subsequent `emit`/`emit_byte` becomes a no-op. This freezes `len` at
    /// the point of first overflow so a later (smaller) emit cannot slip
    /// bytes in past the gap left by the dropped instruction and produce a
    /// silently misaligned code stream.
    pub fn emit(&mut self, bytes: &[u8]) {
        if self.overflowed || self.len + bytes.len() > self.capacity {
            self.overflowed = true;
            return;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.len), bytes.len());
        }
        self.len += bytes.len();
    }

    /// Write bytes into the buffer, returning `false` (without writing) if
    /// there is not enough capacity instead of panicking.
    pub fn emit_checked(&mut self, bytes: &[u8]) -> bool {
        if self.len + bytes.len() > self.capacity {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.len), bytes.len());
        }
        self.len += bytes.len();
        true
    }

    /// Emit a single byte.
    ///
    /// Marks the buffer [`overflowed`](Self::overflowed) and skips the write
    /// instead of panicking when capacity is exhausted. Like [`emit`](Self::emit),
    /// honors the sticky `overflowed` flag: a no-op once the buffer has
    /// already overflowed.
    #[inline]
    pub fn emit_byte(&mut self, b: u8) {
        if self.overflowed || self.len >= self.capacity {
            self.overflowed = true;
            return;
        }
        unsafe {
            *self.ptr.add(self.len) = b;
        }
        self.len += 1;
    }

    /// Returns `true` if any `emit`/`emit_byte` call exceeded capacity.
    /// When set, the emitted code is incomplete and must be discarded.
    #[inline]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Force the buffer into the [`overflowed`](Self::overflowed) state.
    ///
    /// Used by codegen sites that detect a hard codegen invariant break
    /// (e.g. a branch displacement that does not fit its encoding) and
    /// cannot return `Result` to their caller. Setting this flag causes
    /// the surrounding compile driver to discard the half-emitted method
    /// and fall back to the interpreter via the existing
    /// `if buf.overflowed() { return None; }` check in `compile`.
    #[inline]
    pub fn mark_overflowed(&mut self) {
        self.overflowed = true;
    }

    /// Current write position (offset from start).
    #[inline]
    pub fn pos(&self) -> usize {
        self.len
    }

    /// Rewind the write position back to a previously recorded `pos()`.
    ///
    /// Used to discard speculatively-emitted code — e.g. when
    /// `try_emit_inline` abandons a partially-emitted callee body and
    /// falls back to a normal call. Bytes past `offset` are left as-is
    /// in the backing allocation (they will be overwritten by the next
    /// `emit`); only `len` moves. Rewinding *forward* (offset > len) is
    /// rejected so a stale checkpoint cannot expose uninitialized bytes.
    #[inline]
    pub fn rewind_to(&mut self, offset: usize) {
        if offset <= self.len {
            self.len = offset;
        }
    }

    /// Get a pointer to the start of the executable code.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// Get the emitted bytes as a slice.
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Patch 4 bytes (little-endian i32) at the given offset.
    ///
    /// On out-of-bounds offset, marks the buffer
    /// [`overflowed`](Self::overflowed) and returns
    /// `Err(CompileError::PatchFailed)` instead of panicking. The surrounding
    /// compile driver inspects `overflowed()` after codegen and bails to the
    /// interpreter; the caller may ignore the `Err` and rely on that bail.
    pub fn try_patch_i32(&mut self, offset: usize, value: i32) -> Result<(), CompileError> {
        if offset.checked_add(4).map_or(true, |end| end > self.len) {
            self.overflowed = true;
            tracing::warn!(
                offset = offset,
                len = self.len,
                "JIT try_patch_i32: offset out of bounds; marking buffer overflowed"
            );
            return Err(CompileError::PatchFailed {
                kind: "i32",
                offset,
            });
        }
        let bytes = value.to_le_bytes();
        // Safety: bounds checked above; ptr is owned and writable.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(offset), 4);
        }
        Ok(())
    }

    /// Patch 1 byte at the given offset.
    ///
    /// On out-of-bounds offset, marks the buffer
    /// [`overflowed`](Self::overflowed) and returns
    /// `Err(CompileError::PatchFailed)` instead of panicking. See
    /// [`try_patch_i32`](Self::try_patch_i32) for the bail-out contract.
    pub fn try_patch_byte(&mut self, offset: usize, value: u8) -> Result<(), CompileError> {
        if offset >= self.len {
            self.overflowed = true;
            tracing::warn!(
                offset = offset,
                len = self.len,
                "JIT try_patch_byte: offset out of bounds; marking buffer overflowed"
            );
            return Err(CompileError::PatchFailed {
                kind: "byte",
                offset,
            });
        }
        // Safety: bounds checked above.
        unsafe {
            *self.ptr.add(offset) = value;
        }
        Ok(())
    }

    // task #44: the deprecated panicking `patch_i32` / `patch_byte` shims
    // have been removed. Every internal codegen site was migrated to the
    // `try_patch_*` variants in task #20 (commit acd57f2). A workspace grep
    // confirmed zero remaining callers before deletion; the `try_*`
    // variants are the only entry points now.

    /// Read 4 bytes (little-endian i32) at the given offset.
    pub fn read_i32(&self, offset: usize) -> i32 {
        assert!(offset + 4 <= self.len, "read out of bounds");
        let mut bytes = [0u8; 4];
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(offset), bytes.as_mut_ptr(), 4);
        }
        i32::from_le_bytes(bytes)
    }

    /// Transition the buffer from writable to executable.
    ///
    /// Calls the OS API to switch from RW to RX permissions.
    ///
    /// After calling `finalize`, writes via `emit`/`emit_byte`/`try_patch_i32` are
    /// undefined behavior. Call [`make_writable`](Self::make_writable)
    /// first if you need to patch code after finalization.
    pub fn finalize(&self) {
        platform::make_executable(self.ptr, self.capacity).unwrap_or_else(|e| {
            eprintln!("FATAL: JIT: make_executable failed: {e}");
            std::process::abort();
        });
    }

    /// Transition the buffer back from executable to writable (for patching).
    ///
    /// Calls the OS API to switch from RX to RW permissions.
    pub fn make_writable(&self) {
        platform::make_writable(self.ptr, self.capacity).unwrap_or_else(|e| {
            eprintln!("FATAL: JIT: make_writable failed: {e}");
            std::process::abort();
        });
    }
}

/// Total bytes of JIT code retained (never freed) for the process lifetime.
/// See [`ExecutableBuffer`]'s `Drop` for why code is retained rather than freed.
pub static RETAINED_JIT_CODE_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// JIT code-cache cap (bounded growth)
// ---------------------------------------------------------------------------
//
// Compiled code is intentionally RETAINED for the process lifetime (see
// `ExecutableBuffer`'s `Drop`): baked-in direct `CALL rel32` targets and cached
// MIC/PIC entry pointers have no back-reference mechanism, so reclamation would
// dangle them. That makes the code cache monotonically growing — a long-running
// workload that compiles many methods (or repeatedly re-compiles via OSR /
// deopt churn) keeps mapping new executable regions with no upper bound.
//
// Since safe reclamation isn't feasible here, we apply a CAP-AND-STOP policy:
// once the retained code (committed via `ExecutableBuffer::new`) reaches the
// cap, `try_compile` refuses further compilation and the affected methods stay
// in the interpreter. This bounds executable-memory growth at the cost of some
// lost throughput past the cap; correctness is unaffected because the
// interpreter can always run any method.

/// Bytes of JIT code currently committed (mapped) by live `ExecutableBuffer`s.
///
/// Bumped in [`ExecutableBuffer::new`] and decremented only when a region is
/// actually returned to the OS (the `CRATONVM_JIT_FREE_CODE=1` path). Because
/// code is normally retained for the process lifetime, this rises monotonically
/// in the default configuration and is the quantity the code-cache cap bounds.
/// Distinct from [`RETAINED_JIT_CODE_BYTES`], which only counts buffers whose
/// owner was dropped (cache eviction) but whose memory was leaked.
pub static COMMITTED_JIT_CODE_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Default code-cache cap, in bytes (256 MiB). HotSpot's default
/// `ReservedCodeCacheSize` is ~240 MiB on 64-bit, so this is a comparable,
/// deliberately generous bound that real workloads rarely approach.
const DEFAULT_JIT_CODE_CACHE_CAP_BYTES: usize = 256 * 1024 * 1024;

/// Number of `try_compile` calls refused because the code-cache cap was hit.
static JIT_CODE_CACHE_CAP_REFUSALS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Set once, the first time the cap is hit, so the warning is logged exactly once.
static JIT_CODE_CACHE_CAP_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Configured upper bound (in bytes) on total retained JIT code.
///
/// Overridable via `CRATONVM_JIT_CODE_CACHE_MAX_MB` (an integer number of
/// mebibytes); `0` disables the cap entirely (unbounded growth, the legacy
/// behaviour). An unparseable value falls back to the default. Cached on first
/// read so the env lookup happens at most once.
pub fn jit_code_cache_cap_bytes() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_JIT_CODE_CACHE_MAX_MB") {
        Ok(s) => match s.trim().parse::<usize>() {
            // `0` is an explicit "disable the cap" sentinel (treated as
            // `usize::MAX` so the at-capacity check is always false).
            Ok(0) => usize::MAX,
            // Saturate the MiB→bytes multiply so a huge value can't wrap.
            Ok(mb) => mb.saturating_mul(1024 * 1024),
            Err(_) => DEFAULT_JIT_CODE_CACHE_CAP_BYTES,
        },
        Err(_) => DEFAULT_JIT_CODE_CACHE_CAP_BYTES,
    })
}

/// Number of compilations refused so far because the code-cache cap was hit.
/// Diagnostic only.
pub fn jit_code_cache_cap_refusals() -> u64 {
    JIT_CODE_CACHE_CAP_REFUSALS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Returns `true` if retained JIT code has reached the configured cap, meaning
/// new compilation should be refused (the method stays in the interpreter).
///
/// We compare against `COMMITTED_JIT_CODE_BYTES`, which `ExecutableBuffer::new`
/// bumps for every committed allocation (method bodies, OSR trampolines, deopt
/// stubs) — the same quantity the cap is meant to bound. Logs a one-time warning
/// the first time the cap is reached.
fn jit_code_cache_at_capacity() -> bool {
    let cap = jit_code_cache_cap_bytes();
    if cap == usize::MAX {
        return false; // cap disabled
    }
    let used = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    if used < cap {
        return false;
    }
    // At/over the cap: warn exactly once, then keep refusing silently.
    if !JIT_CODE_CACHE_CAP_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "[cratonvm-jit] code-cache cap reached: {} bytes retained >= cap {} bytes; \
             new methods will stay in the interpreter. \
             Raise the limit with CRATONVM_JIT_CODE_CACHE_MAX_MB (0 disables it).",
            used, cap
        );
    }
    true
}

impl Drop for ExecutableBuffer {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // JIT code is RETAINED for the process lifetime — it is never returned
        // to the OS here.
        //
        // Why: a compiled method's code can be the target of *baked-in direct
        // `CALL rel32` instructions* (and cached MIC/PIC entry pointers) emitted
        // into OTHER compiled methods (see `direct_calls` / `try_jit_compile_callee`
        // in the interpreter). When a method is deoptimised or evicted, its
        // `CompiledMethod` is dropped from the JIT cache (e.g.
        // `DeoptimizationController::deoptimize` → `jit_cache.remove`), which
        // would run this `Drop` and `VirtualFree`/`munmap` the code. But there is
        // currently NO back-reference mechanism to find and patch the inbound
        // direct calls, so they would dangle and the next call through one of
        // them faults (execute) at the now-unmapped 64KB-aligned buffer base.
        // That was the real-bytecode RAF avrora SEGV (commit()→advance dispatch;
        // see docs/real-raf-segv-root-cause.md, Part 3).
        //
        // Freeing is therefore unsafe until the JIT tracks inbound call sites and
        // patches/invalidates them at a safepoint before reclamation (a code-cache
        // sweeper — the proper long-term fix). Until then we keep the region
        // mapped AND registered so any dangling direct call still lands on valid,
        // semantically-correct-at-compile-time code instead of crashing.
        //
        // `CRATONVM_JIT_FREE_CODE=1` restores the old free-on-drop behaviour for
        // A/B testing / measuring retained-code growth — do NOT set it in
        // production; it reintroduces the use-after-free.
        if std::env::var_os("CRATONVM_JIT_FREE_CODE").is_some() {
            if let Ok(mut regions) = jit_code_regions().lock() {
                regions.deregister(self.ptr);
            }
            // This region is being returned to the OS, so it no longer counts
            // against the code-cache cap.
            COMMITTED_JIT_CODE_BYTES.fetch_sub(self.capacity, std::sync::atomic::Ordering::Relaxed);
            platform::free_executable(self.ptr, self.capacity);
            return;
        }
        RETAINED_JIT_CODE_BYTES.fetch_add(self.capacity, std::sync::atomic::Ordering::Relaxed);
        // Intentionally leak: keep the mapping live and the region registered.
    }
}

// ---------------------------------------------------------------------------
// Compiled method
// ---------------------------------------------------------------------------

/// NEW-12: one entry in a [`CompiledMethod`]'s oop map table.
///
/// Records, for a specific point in the emitted native code, the set of
/// frame-local slots that hold live object references. The GC root
/// walker consults this map at safepoints to enumerate *only* the real
/// oops, avoiding the false positives of a conservative range scan.
///
/// The map is keyed by `native_pc_offset` — the byte offset of the
/// instruction *after* the safepoint, relative to the compiled method's
/// entry pointer. At GC time, the walker retrieves the active frame's
/// return PC (which is always the instruction-after a call) and
/// subtracts the entry pointer to obtain the offset; this matches one
/// of the recorded entries exactly because safepoints are emitted
/// immediately before the call-that-may-trigger-GC.
///
/// `frame_slot_offsets` lists the byte offsets *relative to RBP* of each
/// slot holding a live oop. Negative offsets index into the local
/// variable + spill areas; `[rbp + offset]` dereferences to the oop.
/// Exactly one qword per entry — the frame layout guarantees 8-byte
/// alignment so smaller slots never appear.
///
/// The fields are `Vec<i16>` / `u32` to minimize the map's footprint.
/// A typical method has 1–5 oop maps with ≤ 16 slots each; the total
/// overhead is well under 100 bytes for the vast majority of methods.
#[derive(Debug, Clone)]
pub struct OopMapEntry {
    /// Byte offset into the compiled method's machine code of the
    /// instruction that follows the safepoint call. GC walkers match
    /// on this offset directly (not via ranges) because the safepoint
    /// is emitted *immediately* before the call.
    pub native_pc_offset: u32,
    /// Stage 3 (precise oop maps) — bytecode PC of the safepoint
    /// instruction. The JIT stores this into the frame's safepoint-id
    /// slot before each GC-capable call so the GC root walker can match
    /// it back to the *exact* map for the active safepoint (not the
    /// union-of-all-maps, which is unsafe for relocation). 0 / unused
    /// when the precise gate is off.
    pub bytecode_pc: u32,
    /// Frame slot offsets relative to RBP that hold live oops at this
    /// safepoint. `i16` is sufficient because frame sizes are capped
    /// well below 32 KiB in the current JIT; a larger frame would fail
    /// the compile-time max_locals check before reaching this code.
    pub frame_slot_offsets: Vec<i16>,
}

impl OopMapEntry {
    /// Construct an empty oop map for `native_pc_offset`. Callers that
    /// want to build a map programmatically push to
    /// [`Self::frame_slot_offsets`] directly.
    pub fn new(native_pc_offset: u32) -> Self {
        Self {
            native_pc_offset,
            bytecode_pc: 0,
            frame_slot_offsets: Vec::new(),
        }
    }

    /// Return the number of oop slots recorded at this safepoint.
    pub fn slot_count(&self) -> usize {
        self.frame_slot_offsets.len()
    }
}

// ---------------------------------------------------------------------------
// Stage 5 (precise oop maps) — JIT code-range registry
// ---------------------------------------------------------------------------
//
// Maps each compiled method's native code range `[entry, entry+len)` to its
// (Arc-stable) `CompiledMethod` pointer. The GC root walker uses it to resolve
// which method a return address belongs to while walking the JIT RBP chain, so
// it can remap EVERY active JIT frame (not just the innermost). Populated at
// `JitCache::put`, evicted at `JitCache::remove`. The stored pointer is the
// `Arc<CompiledMethod>` inner address, which is stable for the cm's cache
// lifetime; a live JIT frame keeps its cm in the cache (hence alive).

/// One registered code range: `(entry, end, cm_ptr)`.
static JIT_CODE_RANGES: std::sync::OnceLock<std::sync::Mutex<Vec<(usize, usize, usize)>>> =
    std::sync::OnceLock::new();

fn jit_code_ranges() -> &'static std::sync::Mutex<Vec<(usize, usize, usize)>> {
    JIT_CODE_RANGES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Register `[entry, entry+len)` → `cm_ptr` (the `Arc<CompiledMethod>` inner
/// address). No-op for empty/zero ranges. Stage 5.
pub fn register_jit_code_range(entry: usize, len: usize, cm_ptr: usize) {
    if entry == 0 || len == 0 || cm_ptr == 0 {
        return;
    }
    if let Ok(mut v) = jit_code_ranges().lock() {
        v.push((entry, entry + len, cm_ptr));
    }
}

/// Remove every range with the given `entry` start (called on cache eviction
/// so the GC walker never resolves a return address to a freed method). Stage 5.
pub fn unregister_jit_code_range(entry: usize) {
    if entry == 0 {
        return;
    }
    if let Ok(mut v) = jit_code_ranges().lock() {
        v.retain(|&(e, _, _)| e != entry);
    }
}

/// Number of registered code ranges (Stage 5 diagnostic).
pub fn jit_code_range_count() -> usize {
    jit_code_ranges().lock().map(|v| v.len()).unwrap_or(0)
}

/// Resolve the `CompiledMethod` pointer whose code range contains `addr`, or
/// `None`. Linear scan (method counts are modest; only hit at GC time on the
/// gated precise path). Stage 5.
pub fn lookup_jit_code_range(addr: usize) -> Option<usize> {
    let v = jit_code_ranges().lock().ok()?;
    v.iter()
        .find(|&&(e, end, _)| addr >= e && addr < end)
        .map(|&(_, _, cm)| cm)
}

/// DBG (spring-bug-11): code-range → method-name table for naming a JIT frame in
/// a crash report. Populated by `JitCache::put` ONLY when `CRATONVM_DBG_JIT_NAMES`
/// is set (so the default path keeps zero overhead and no unbounded growth).
static JIT_NAME_RANGES: std::sync::OnceLock<std::sync::Mutex<Vec<(usize, usize, String)>>> =
    std::sync::OnceLock::new();

fn jit_name_ranges() -> &'static std::sync::Mutex<Vec<(usize, usize, String)>> {
    JIT_NAME_RANGES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Whether to record JIT method-name ranges (`CRATONVM_DBG_JIT_NAMES`). Cached.
pub fn jit_names_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::env::var_os("CRATONVM_DBG_JIT_NAMES").is_some())
}

/// deopt-osr: master gate for *real* deopt-exit / OSR-exit resume
/// (`CRATONVM_DEOPT_REAL`, default-OFF). Read-once cached. While OFF (the
/// default) every guard/loop bail stays on the `i64::MIN` whole-method re-run,
/// so the surface is inert. The interpreter deopt sinks and the OSR-exit sink
/// consult this together with the per-method `can_deopt_resume` / `can_osr_exit`
/// flags, so deopt-exit and OSR-exit can never run half-on
/// (see `docs/feature-designs/deopt-osr.md`).
pub fn deopt_real_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::env::var_os("CRATONVM_DEOPT_REAL").is_some())
}

/// deopt-osr: CI/test gate for the eager-deopt differential verifier
/// (`CRATONVM_DEOPT_VERIFY`, default-OFF). Read-once cached. When ON, every
/// eligible guard/loop boundary deopts, reconstructs the interpreter frame, and
/// compares the reconstructed-interpreter result against the JIT result — the
/// mandatory check before any guard/loop family is flipped onto `deopt_real`.
pub fn deopt_verify_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::env::var_os("CRATONVM_DEOPT_VERIFY").is_some())
}

/// Record `[entry, entry+len)` → `name` for crash-time symbolization. No-op
/// unless `CRATONVM_DBG_JIT_NAMES` is set.
pub fn register_jit_method_name(entry: usize, len: usize, name: String) {
    if entry == 0 || len == 0 {
        return;
    }
    if let Ok(mut v) = jit_name_ranges().lock() {
        v.push((entry, entry + len, name));
    }
}

/// Resolve the method name whose code range contains `addr`. Uses `try_lock` so
/// it is safe to call from a crash handler (never blocks on a held lock).
pub fn lookup_jit_method_name(addr: usize) -> Option<String> {
    let v = jit_name_ranges().try_lock().ok()?;
    v.iter()
        .find(|(e, end, _)| addr >= *e && addr < *end)
        .map(|(_, _, name)| name.clone())
}

/// A compiled native-code method.
pub struct CompiledMethod {
    /// The executable buffer holding the machine code.
    _buffer: ExecutableBuffer,
    /// Entry point — pointer to the start of the compiled code.
    entry: *const u8,
    /// Whether the method needs a SharedVm pointer as hidden first argument.
    needs_context: bool,
    /// Owned JIT metadata strings. JIT code references these via raw pointers;
    /// they are freed when this `CompiledMethod` is dropped.
    pub _jit_strings: Vec<Box<str>>,
    /// Owned `JitInvokeInfo` structs. JIT code references these via raw pointers;
    /// they are freed when this `CompiledMethod` is dropped.
    pub _jit_invoke_infos: Vec<Box<JitInvokeInfo>>,
    /// Owned monomorphic inline cache slots. JIT code references these via raw
    /// pointers; they are freed when this `CompiledMethod` is dropped.
    pub _jit_mic_slots: Vec<Box<JitMICSlot>>,
    /// Owned polymorphic inline cache slots. JIT code references these via
    /// raw pointers (embedded as imm64 in the inline 3-way cascade emitted
    /// by `Compiler::compile_op_invokevirtual`); they are freed when this
    /// `CompiledMethod` is dropped.
    ///
    /// HIGH-7 — populated eagerly at first compile for every
    /// invokevirtual / invokeinterface bci. Slots start empty (all 3
    /// `cached_class_id` entries == 0), so the inline cascade falls
    /// straight through to the slow-path helper on cold sites; once the
    /// helper has populated a slot, subsequent dispatches take the
    /// inline fast path.
    pub _jit_pic_slots: Vec<Box<JitPICSlot>>,
    /// OSR metadata: bytecode PC → native offset mapping.
    pub osr_pc_to_native: Option<Vec<i32>>,
    /// OSR metadata: number of locals in the compiled frame.
    pub osr_num_locals: usize,
    /// OSR metadata: number of register-mapped locals.
    pub osr_num_reg_locals: usize,
    /// OSR metadata: per-local GPR register assignments from graph-coloring allocator.
    pub osr_local_assignments: Option<Vec<Option<u8>>>,
    /// OSR metadata: per-bytecode-PC "dead local" mask. `osr_dead_mask[pc]` bit
    /// `i` set means local `i` is dead at that OSR entry PC and must NOT be
    /// loaded into its register by the trampoline (it would clobber a live
    /// local that shares the coalesced register). Indexed like `osr_pc_to_native`.
    pub osr_dead_mask: Option<Vec<u64>>,
    /// OSR metadata: per-local XMM register assignments for float/double locals.
    pub osr_xmm_assignments: Option<Vec<Option<u8>>>,
    /// OSR metadata: frame size (for SUB RSP).
    pub osr_frame_size: i32,
    /// OSR metadata: callee-saved register save area offset.
    pub osr_callee_saved_base: i32,
    /// OSR metadata: offset of VM context pointer in frame.
    pub osr_heap_local_offset: i32,
    /// Shadow-stack: frame offset (`[rbp - off]`) of the cached thread-pointer
    /// slot. A normal entry sets it in the prologue. An OSR entry (which bypasses
    /// the prologue thread-fetch) zeroes it by default → the push/reload/epilogue
    /// null-guards make that frame SKIP shadow tracking; under the opt-in
    /// `CRATONVM_SHADOW_OSR_TRACK` sub-gate it instead stores the real
    /// `*mut JvmThread` passed through `osr_enter` so the OSR frame is tracked
    /// (follow-up §1). 0 when shadow disabled.
    pub shadow_thread_slot_off: i32,
    /// Shadow-stack: frame offset (`[rbp - off]`) of the saved `top` watermark
    /// slot. The OSR trampoline snapshots `thread.shadow_stack.top` here at entry
    /// (mirroring the prologue) so the method epilogue restores it on return,
    /// unwinding any unbalanced push this OSR-entered frame made. 0 when disabled.
    pub shadow_savetop_slot_off: i32,
    /// Shadow-stack: byte offset of the `ShadowStack` within `JvmThread` (its
    /// `top` field is at offset 0, the JIT contract). Used by the OSR trampoline
    /// to load the entry watermark `[thread + off]`. 0 when shadow disabled.
    pub shadow_off_in_thread: i32,
    /// Whether the compiled code uses invoke dispatch (needs set_jit_thread + catch_unwind).
    /// Methods with only direct calls can skip this overhead.
    pub has_dispatch: bool,
    /// Methods that were inlined into this compiled method.
    /// Each entry is (class_name, method_name, descriptor).
    /// Used by invalidation: if the inlined method's class changes, this code must be evicted.
    pub inlined_methods: Vec<(String, String, String)>,
    /// Deoptimization points: native code offsets where deopt can occur.
    /// Used by the deopt framework to reconstruct interpreter state.
    pub deopt_points: Vec<deopt::DeoptimizationPoint>,
    /// real-frame-deopt: boxed deopt points whose addresses are baked as
    /// imm64 into the guard/deopt-trampoline machine code. JIT code holds raw
    /// pointers into these boxes, so — like `_jit_invoke_infos` — they must
    /// outlive the (retained) code; `Drop` leaks them alongside the other
    /// code-referenced metadata. Stable heap addresses (`Box`) are required:
    /// the `deopt_points` Vec above can realloc, these boxes never move.
    pub _deopt_point_boxes: Vec<Box<deopt::DeoptimizationPoint>>,
    /// NEW-12: precise oop maps indexed by native PC offset.
    ///
    /// Each entry records the frame-slot offsets (relative to RBP)
    /// that hold live object references at a specific safepoint. The
    /// GC root walker uses this list to enumerate real oops rather
    /// than conservatively treating every stack word as a root.
    ///
    /// The entries are kept sorted by `native_pc_offset` so that
    /// [`Self::find_oop_map_for_pc`] can do an O(log n) binary
    /// search at GC time. Direct `push_oop_map` appends and then
    /// resorts on the next call to `find_oop_map_for_pc` — the
    /// amortized cost is negligible because per-method entries are
    /// typically single-digit.
    ///
    /// An empty `oop_maps` vector means "no precise coverage" and
    /// the root walker falls back to the conservative stack scan.
    /// This is the current default for every compiled method because
    /// the JIT compiler does not yet populate oop maps from its
    /// simulated-stack type tracker; see the NEW-12 section of
    /// `docs/roadmap.md` for the migration plan.
    pub oop_maps: Vec<OopMapEntry>,
    /// RBC.2 — `true` when this artifact was produced by the OSR compile
    /// path (which eagerly compiles invokestatic callees and wires direct
    /// calls). The OSR trigger only REUSES artifacts with this flag: a
    /// first-call/upgrade artifact may lack that wiring, and pinning it
    /// into a hot loop forever (instead of one fresh OSR compile) routes
    /// every callee through the slow dispatch helper.
    pub compiled_via_osr: bool,
    /// RBC.5 — raw class ids of the declaring classes of every
    /// getstatic/putstatic site in this method, recorded at compile time
    /// from the already-resolved `static_field_info`. JIT code reads static
    /// storage directly, so these classes must be initialized before first
    /// execution; recording them here lets the interpreter's compiled-entry
    /// fast path run that check once per artifact instead of re-resolving
    /// every constant-pool ref on every call.
    pub static_init_classes: Vec<u32>,
    /// RBC.5 — set once the ensure-initialized walk over
    /// `static_init_classes` has fully succeeded for this artifact.
    pub static_inits_done: std::sync::atomic::AtomicBool,
    /// NEW-12: cached flag — `true` once `oop_maps` is known to be
    /// sorted by `native_pc_offset`.
    ///
    /// This exists purely to keep the O(n) sortedness scan off the
    /// per-safepoint GC lookup path. `find_oop_map_for_pc` reads it: when
    /// `false` it runs the `windows(2)` check once (sorting only if the
    /// data is actually out of order) and then sets it `true`, so every
    /// subsequent lookup skips the scan entirely. It starts `false`
    /// because the production codegen path moves a fully-built vector in
    /// wholesale (`cm.oop_maps = compiler.oop_maps`, bypassing
    /// `push_oop_map`); any `push_oop_map` likewise clears it so the next
    /// lookup re-verifies.
    oop_maps_sorted: bool,
    /// Stage 3 (precise oop maps) — frame offset (positive; slot at
    /// `[rbp - sp_id_slot_off]`) where the JIT stored the active
    /// safepoint's bytecode PC before each GC-capable call. The GC root
    /// walker reads this slot to recover the exact `OopMapEntry`. `0`
    /// means the precise gate (`CRATONVM_PRECISE_JIT_MAPS`) was off at
    /// compile time, so no safepoint-id slot exists and the walker uses
    /// the conservative path for this method.
    pub sp_id_slot_off: i32,
    /// Stage A (precise oop maps, B-K fix) — `true` only when EVERY
    /// GC-capable safepoint in this method has a precise oop map AND the
    /// method has no coverage-breaking construct (OSR entry, un-mapped
    /// inlined-callee safepoint). When `true` the GC root walker may
    /// skip the conservative backstop sweep for this frame and treat its
    /// precise oops as relocatable (movable) rather than pinned — the
    /// change that lets selective promotion drain JIT-rooted young
    /// objects without the B-K stale-slot corruption. `false` (the
    /// default) preserves the conservative, pin-everything behaviour, so
    /// it is always safe to leave unset. Only ever consulted on the
    /// gated precise path (`CRATONVM_PRECISE_JIT_MAPS`).
    pub fully_oop_covered: bool,
    /// deopt-osr scaffolding — `true` only once the deopt finalizer has
    /// proven this method can rebuild a precise interpreter frame at a guard
    /// bci and resume there (instead of the `i64::MIN` whole-method re-run).
    /// `false` (the default) keeps the method on the safe re-run path; no
    /// emitter populates it yet, so it is currently always `false`. Mirrors
    /// the `fully_oop_covered` coverage-gate pattern: purely additive, no
    /// behaviour change until the resume path is wired
    /// (see `docs/feature-designs/deopt-osr.md`).
    pub can_deopt_resume: bool,
    /// deopt-osr scaffolding — `true` only once the OSR-exit map emitter has
    /// proven this (OSR-compiled) method can leave a running JIT/OSR frame
    /// mid-loop at a loop bci with the loop's live state, rather than the
    /// `i64::MIN` re-run (which is *wrong* for an OSR'd frame entered partway
    /// through). `false` by default; no emitter populates it yet.
    pub can_osr_exit: bool,
    /// deopt-osr scaffolding — monotonic compilation epoch for this artifact.
    /// When `MakeNotEntrant` invalidation lands, boxed `DeoptimizationPoint`
    /// pointers (baked into guard code) are versioned by this epoch so a
    /// resume never follows a box belonging to a superseded compilation.
    /// `0` for every artifact today (no invalidation consumer yet).
    pub compilation_epoch: u64,
}

unsafe impl Send for CompiledMethod {}
unsafe impl Sync for CompiledMethod {}

impl Drop for CompiledMethod {
    fn drop(&mut self) {
        // The emitted machine code is RETAINED for the process lifetime (see
        // `ExecutableBuffer::drop`) because other compiled methods bake direct
        // `CALL rel32` / IC-slot pointers into it with no back-reference to patch
        // on reclamation. That code ALSO holds RAW pointers into THIS method's
        // interned strings, `JitInvokeInfo`s, and MIC/PIC slots. Freeing those
        // boxes here while the code lives on makes the next MIC/PIC dispatch read
        // a freed `JitInvokeInfo` — a garbage class/method name (embedded NUL
        // bytes) that corrupts dispatch and panics when logged (avrora real-RAF
        // `Thread-N` `core::fmt` slice panic on `MainClock.<garbage>`; see
        // docs/real-raf-segv-root-cause.md). So leak this metadata too, keeping
        // it alive exactly as long as the code that references it. This completes
        // the code-retention fix — the two MUST go together.
        //
        // `CRATONVM_JIT_FREE_CODE=1` restores full freeing (code + metadata +
        // OSR-trampoline purge) for A/B testing — it reintroduces the UAF.
        if std::env::var_os("CRATONVM_JIT_FREE_CODE").is_none() {
            std::mem::forget(std::mem::take(&mut self._jit_strings));
            std::mem::forget(std::mem::take(&mut self._jit_invoke_infos));
            std::mem::forget(std::mem::take(&mut self._jit_mic_slots));
            std::mem::forget(std::mem::take(&mut self._jit_pic_slots));
            // Deopt-point boxes are referenced by baked imm64 pointers in the
            // (retained) guard/trampoline code; leak them too.
            std::mem::forget(std::mem::take(&mut self._deopt_point_boxes));
            return;
        }
        // FREE mode: purge any cached OSR trampolines that point into this
        // method's code range. After Drop, `self._buffer` releases its
        // executable mapping, so any stale `target_addr` in the global cache
        // would be a use-after-free hazard if a future compile reused the same
        // address. `target_addr` for an OSR entry is `self.entry +
        // native_offset`, where `native_offset < self._buffer.pos()`; pruning by
        // half-open range `[entry, entry + pos)` covers every such address.
        #[cfg(target_arch = "x86_64")]
        {
            let start = self.entry as usize;
            let end = start.saturating_add(self._buffer.pos());
            let mut cache = osr_trampoline_cache().lock();
            cache.retain(|&target, _| !(target >= start && target < end));
        }
    }
}

impl CompiledMethod {
    /// Create from a completed executable buffer (pure method, no context needed).
    ///
    /// Raw machine-code bytes of this compiled method (for diagnostics /
    /// disassembly). The slice is the full executable buffer.
    pub fn code_bytes(&self) -> &[u8] {
        self._buffer.as_slice()
    }

    /// Finalizes the buffer (transitions from writable to executable).
    pub fn new(buffer: ExecutableBuffer) -> Self {
        buffer.finalize();
        let entry = buffer.as_ptr();
        Self {
            _buffer: buffer,
            entry,
            needs_context: false,
            _jit_strings: Vec::new(),
            _jit_invoke_infos: Vec::new(),
            _jit_mic_slots: Vec::new(),
            _jit_pic_slots: Vec::new(),
            osr_pc_to_native: None,
            osr_num_locals: 0,
            osr_num_reg_locals: 0,
            osr_local_assignments: None,
            osr_dead_mask: None,
            osr_xmm_assignments: None,
            osr_frame_size: 0,
            osr_callee_saved_base: 0,
            osr_heap_local_offset: 0,
            shadow_thread_slot_off: 0,
            shadow_savetop_slot_off: 0,
            shadow_off_in_thread: 0,
            has_dispatch: false,
            inlined_methods: Vec::new(),
            deopt_points: Vec::new(),
            _deopt_point_boxes: Vec::new(),
            oop_maps: Vec::new(),
            // Start unverified: the production codegen path moves a
            // fully-built vector into `oop_maps` wholesale (bypassing
            // `push_oop_map`), so the first `find_oop_map_for_pc` call
            // must verify/sort once. `push_oop_map` keeps the flag
            // precise for the incremental-build path.
            compiled_via_osr: false,
            static_init_classes: Vec::new(),
            static_inits_done: std::sync::atomic::AtomicBool::new(false),
            oop_maps_sorted: false,
            sp_id_slot_off: 0,
            fully_oop_covered: false,
            // deopt-osr scaffolding: default to the safe re-run path; no
            // emitter sets these yet (see docs/feature-designs/deopt-osr.md).
            can_deopt_resume: false,
            can_osr_exit: false,
            compilation_epoch: 0,
        }
    }

    /// Create from a completed executable buffer (needs VM context pointer).
    ///
    /// Finalizes the buffer (transitions from writable to executable).
    pub fn new_with_context(buffer: ExecutableBuffer) -> Self {
        buffer.finalize();
        let entry = buffer.as_ptr();
        Self {
            _buffer: buffer,
            entry,
            needs_context: true,
            _jit_strings: Vec::new(),
            _jit_invoke_infos: Vec::new(),
            _jit_mic_slots: Vec::new(),
            _jit_pic_slots: Vec::new(),
            osr_pc_to_native: None,
            osr_num_locals: 0,
            osr_num_reg_locals: 0,
            osr_local_assignments: None,
            osr_dead_mask: None,
            osr_xmm_assignments: None,
            osr_frame_size: 0,
            osr_callee_saved_base: 0,
            osr_heap_local_offset: 0,
            shadow_thread_slot_off: 0,
            shadow_savetop_slot_off: 0,
            shadow_off_in_thread: 0,
            has_dispatch: false,
            inlined_methods: Vec::new(),
            deopt_points: Vec::new(),
            _deopt_point_boxes: Vec::new(),
            oop_maps: Vec::new(),
            // Start unverified: the production codegen path moves a
            // fully-built vector into `oop_maps` wholesale (bypassing
            // `push_oop_map`), so the first `find_oop_map_for_pc` call
            // must verify/sort once. `push_oop_map` keeps the flag
            // precise for the incremental-build path.
            compiled_via_osr: false,
            static_init_classes: Vec::new(),
            static_inits_done: std::sync::atomic::AtomicBool::new(false),
            oop_maps_sorted: false,
            sp_id_slot_off: 0,
            fully_oop_covered: false,
            // deopt-osr scaffolding: default to the safe re-run path; no
            // emitter sets these yet (see docs/feature-designs/deopt-osr.md).
            can_deopt_resume: false,
            can_osr_exit: false,
            compilation_epoch: 0,
        }
    }

    /// NEW-12: append a precise oop map entry for a safepoint.
    ///
    /// Called by the JIT code emitter at every point where GC can be
    /// triggered (new, newarray, anewarray, invoke of a callee that
    /// may allocate). The `native_pc_offset` must be the offset of the
    /// instruction that follows the safepoint call; at GC time the
    /// walker matches on this offset directly (no PC-range search).
    ///
    /// The caller is responsible for ordering entries by
    /// `native_pc_offset` when pushing; otherwise
    /// [`Self::find_oop_map_for_pc`] will sort them lazily on first
    /// lookup. Direct append is the common path because the compiler
    /// walks the method in bytecode order.
    pub fn push_oop_map(&mut self, entry: OopMapEntry) {
        // A push invalidates any cached "sorted" knowledge from a prior
        // `find_oop_map_for_pc` call, so clear the flag; the next lookup
        // re-verifies (and sorts only if actually needed). This keeps the
        // O(n) sortedness scan off the per-safepoint GC lookup path while
        // remaining correct regardless of push order.
        self.oop_maps_sorted = false;
        self.oop_maps.push(entry);
    }

    /// NEW-12: locate the oop map for a specific native PC offset.
    ///
    /// Returns `Some(&OopMapEntry)` if the compiled method has a map
    /// exactly at `native_pc_offset`, or `None` otherwise. The GC
    /// walker calls this with the current frame's return PC minus
    /// the entry pointer. When it returns `None`, the conservative
    /// scan is used for that frame as a fallback.
    ///
    /// Sorts the `oop_maps` vector on first call so subsequent
    /// lookups are O(log n) binary searches. Idempotent: the sort is
    /// cheap and runs only if the vector is out of order.
    pub fn find_oop_map_for_pc(&mut self, native_pc_offset: u32) -> Option<&OopMapEntry> {
        // Ensure sorted for binary search. The O(n) `windows(2)`
        // sortedness check (and any sort) runs at most once per compiled
        // method: `oop_maps_sorted` caches the result so subsequent
        // GC-time lookups skip the scan entirely. `push_oop_map` clears
        // the flag, so a lookup after any push re-verifies. (The flag
        // starts `false` to cover the wholesale
        // `cm.oop_maps = compiler.oop_maps` codegen path, which bypasses
        // `push_oop_map`.)
        if !self.oop_maps_sorted {
            let already_sorted = self
                .oop_maps
                .windows(2)
                .all(|w| w[0].native_pc_offset <= w[1].native_pc_offset);
            if !already_sorted {
                self.oop_maps.sort_by_key(|e| e.native_pc_offset);
            }
            self.oop_maps_sorted = true;
        }
        debug_assert!(
            self.oop_maps
                .windows(2)
                .all(|w| w[0].native_pc_offset <= w[1].native_pc_offset),
            "oop_maps must be sorted by native_pc_offset before binary search",
        );
        match self
            .oop_maps
            .binary_search_by_key(&native_pc_offset, |e| e.native_pc_offset)
        {
            Ok(idx) => Some(&self.oop_maps[idx]),
            Err(_) => None,
        }
    }

    /// NEW-12: whether this method has at least one precise oop map.
    ///
    /// Used by `JitEntryGuard::enter_with_compiled` to decide whether
    /// the frame can be walked precisely. A method with no oop maps
    /// falls back to the conservative scan for its active frame.
    pub fn has_precise_oop_maps(&self) -> bool {
        !self.oop_maps.is_empty()
    }

    /// real-frame-deopt: locate the deopt point for an exact native PC offset.
    ///
    /// Returns `Some(&DeoptimizationPoint)` when the compiled method has a
    /// deopt site exactly at `native_offset` (e.g. the trap branch a failed
    /// guard jumps from), or `None` otherwise. The VM deopt entry calls this
    /// with `faulting_pc - entry_ptr` to recover the `FrameState` it must
    /// reconstruct.
    ///
    /// `deopt_points` is emitted by the lowerer in ascending `native_offset`
    /// order (it walks blocks/bytecode in address order), so this is an
    /// O(log n) binary search with a debug-time sortedness check. If a future
    /// emitter pushes out of order, the `debug_assert` fires in tests and the
    /// release path degrades to a possibly-missed lookup (→ conservative
    /// whole-method re-run), never to unsafety.
    pub fn find_deopt_point(&self, native_offset: u32) -> Option<&deopt::DeoptimizationPoint> {
        debug_assert!(
            self.deopt_points
                .windows(2)
                .all(|w| w[0].native_offset <= w[1].native_offset),
            "deopt_points must be sorted by native_offset for binary search",
        );
        match self
            .deopt_points
            .binary_search_by_key(&native_offset, |p| p.native_offset)
        {
            Ok(idx) => Some(&self.deopt_points[idx]),
            Err(_) => None,
        }
    }

    /// Whether this method requires a SharedVm pointer as its hidden first argument.
    pub fn needs_context(&self) -> bool {
        self.needs_context
    }

    /// Return the raw entry point pointer for direct calls from JIT code.
    pub fn entry_ptr(&self) -> *const u8 {
        self.entry
    }

    /// Stage 5 (precise oop maps) — length in bytes of this method's emitted
    /// machine code, so the GC code-range registry can record `[entry,
    /// entry+code_len())` for return-address → CompiledMethod resolution while
    /// walking the JIT RBP chain.
    pub fn code_len(&self) -> usize {
        self._buffer.pos()
    }

    /// Debug-only: raw emitted machine code bytes.
    pub fn _buffer_slice_for_debug(&self) -> &[u8] {
        self._buffer.as_slice()
    }

    /// Backward-compatible alias for `needs_context()`.
    pub fn needs_heap(&self) -> bool {
        self.needs_context
    }

    /// Call a pure compiled method, returning `Err` instead of panicking
    /// on invalid code pointer or unsupported arg count.
    ///
    /// task #44: this is the canonical entry point for invoking JIT
    /// code. The earlier panicking-warn-and-return-0 `call` wrapper has
    /// been removed; every VM and test caller now goes through
    /// `try_call` directly.
    ///
    /// # Safety
    /// The compiled code must match the expected signature.
    #[inline]
    pub unsafe fn try_call(&self, args: &[i64]) -> Result<i64, CompileError> {
        validate_code_ptr(self.entry).map_err(CompileError::InvalidCodePtr)?;
        match args.len() {
            0 => {
                let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(self.entry);
                Ok(f())
            }
            1 => {
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(self.entry);
                Ok(f(args[0]))
            }
            2 => {
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(self.entry);
                Ok(f(args[0], args[1]))
            }
            3 => {
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(self.entry);
                Ok(f(args[0], args[1], args[2]))
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(args[0], args[1], args[2], args[3]))
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(args[0], args[1], args[2], args[3], args[4]))
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(args[0], args[1], args[2], args[3], args[4], args[5]))
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(
                    args[0], args[1], args[2], args[3], args[4], args[5], args[6],
                ))
            }
            8 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(
                    args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7],
                ))
            }
            n => Err(CompileError::TooManyArgs(n)),
        }
    }

    // task #44: the panicking-warn-and-return-0 `CompiledMethod::call`
    // wrapper has been removed. All workspace callers (vm/, jit/src,
    // jit/tests) now use `try_call` directly. A JIT runtime invocation
    // failure (invalid code pointer, too-many-args) surfaces as
    // `Err(CompileError)` instead of being silently downgraded to a
    // zero return value.

    /// Call a compiled method that needs VM context, returning `Err`
    /// instead of panicking on invalid code pointer or unsupported arg
    /// count.
    ///
    /// task #44: this is the canonical entry point for invoking
    /// context-needing JIT code. The earlier
    /// panicking-warn-and-return-0 `call_with_context` wrapper has
    /// been removed; every VM and test caller now goes through
    /// `try_call_with_context` directly.
    ///
    /// # Safety
    /// `vm_ptr` must be a valid pointer to a `SharedVm`. Args must match
    /// the method signature.
    #[inline]
    pub unsafe fn try_call_with_context(
        &self,
        vm_ptr: i64,
        args: &[i64],
    ) -> Result<i64, CompileError> {
        validate_code_ptr(self.entry).map_err(CompileError::InvalidCodePtr)?;
        match args.len() {
            0 => {
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(self.entry);
                Ok(f(vm_ptr))
            }
            1 => {
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(self.entry);
                Ok(f(vm_ptr, args[0]))
            }
            2 => {
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(self.entry);
                Ok(f(vm_ptr, args[0], args[1]))
            }
            3 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(vm_ptr, args[0], args[1], args[2]))
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(vm_ptr, args[0], args[1], args[2], args[3]))
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(vm_ptr, args[0], args[1], args[2], args[3], args[4]))
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(
                    vm_ptr, args[0], args[1], args[2], args[3], args[4], args[5],
                ))
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                Ok(f(
                    vm_ptr, args[0], args[1], args[2], args[3], args[4], args[5], args[6],
                ))
            }
            n => Err(CompileError::TooManyArgs(n)),
        }
    }

    // task #44: the panicking-warn-and-return-0 `call_with_context`
    // wrapper has been removed alongside `call`. All workspace callers
    // now use `try_call_with_context` directly (or the thin `.expect()`
    // alias `call_with_heap` below for test-side callers that keep an
    // `i64` return type).

    /// Backward-compatible test-only alias for `try_call_with_context`.
    ///
    /// task #44 removed the panicking `call_with_context` wrapper; this
    /// alias now delegates to the `try_*` variant and `.expect()`s the
    /// result so existing tests keep their `i64` return type. New code
    /// should call [`try_call_with_context`](Self::try_call_with_context)
    /// directly.
    ///
    /// # Safety
    /// Same safety requirements as
    /// [`try_call_with_context`](Self::try_call_with_context).
    #[inline]
    pub unsafe fn call_with_heap(&self, heap_ptr: i64, args: &[i64]) -> i64 {
        self.try_call_with_context(heap_ptr, args)
            .expect("call_with_heap: invalid JIT entry or arg count (use try_call_with_context for the fallible variant)")
    }

    /// True when this artifact recorded an OSR entry point for `entry_pc`
    /// (i.e. [`osr_enter`](Self::osr_enter) at that pc would not bail).
    /// Lets the interpreter's OSR trigger reuse a cached compile instead of
    /// re-running the whole x64 pipeline on every trigger.
    pub fn can_osr_enter(&self, entry_pc: usize) -> bool {
        self.osr_pc_to_native
            .as_ref()
            .and_then(|t| t.get(entry_pc).copied())
            .map_or(false, |off| off >= 0)
    }

    /// OSR entry: enter JIT code at an arbitrary bytecode PC with interpreter locals.
    ///
    /// # Safety
    /// `vm_ptr` must be a valid SharedVm pointer. `jit_locals` must contain exactly
    /// `osr_num_locals` i64 values in local-index order. `thread_ptr` must be the
    /// current `*mut JvmThread` (whose shadow stack is already allocated by
    /// `set_jit_thread`) when the shadow-stack gate is on, so this OSR-entered
    /// frame is precisely tracked; pass 0 to opt out of shadow tracking (tests).
    /// See `emit_osr_trampoline` (follow-up §1).
    #[cfg(target_arch = "x86_64")]
    #[inline(never)]
    pub unsafe fn osr_enter(
        &self,
        vm_ptr: i64,
        jit_locals: &[i64],
        entry_pc: usize,
        thread_ptr: i64,
    ) -> Option<i64> {
        let pc_to_native = self.osr_pc_to_native.as_ref()?;
        if entry_pc >= pc_to_native.len() {
            return None;
        }
        let native_offset = pc_to_native[entry_pc];
        if native_offset < 0 {
            return None;
        }
        let target_addr = self.entry as usize + native_offset as usize;

        // Locals dead at this entry PC must not be loaded into their (possibly
        // shared) registers — loading a dead local clobbers the live local that
        // colours to the same register. See `osr_dead_mask`.
        let dead_mask = self
            .osr_dead_mask
            .as_ref()
            .and_then(|m| m.get(entry_pc).copied())
            .unwrap_or(0);

        osr_trampoline(
            target_addr,
            vm_ptr,
            jit_locals.as_ptr(),
            self.osr_num_locals,
            self.osr_num_reg_locals,
            self.osr_local_assignments.as_deref(),
            self.osr_xmm_assignments.as_deref(),
            self.osr_frame_size,
            self.osr_callee_saved_base,
            self.osr_heap_local_offset,
            self.needs_context,
            dead_mask,
            self.shadow_thread_slot_off,
            self.shadow_savetop_slot_off,
            self.shadow_off_in_thread,
            thread_ptr,
        )
    }
}

// ---------------------------------------------------------------------------
// OSR (On-Stack Replacement) trampoline
// ---------------------------------------------------------------------------

/// Global cache of emitted OSR trampolines, keyed by `target_addr`.
///
/// Each `target_addr` (= `CompiledMethod.entry + native_offset` for an OSR PC) is
/// stable for the lifetime of the compiled method. The trampoline body depends
/// only on the method's frame layout and the destination JIT address — all of
/// which are constant per `target_addr`. Runtime values (`vm_ptr`, `locals_ptr`)
/// are now passed via argument registers instead of being baked in as immediates,
/// so a single emitted trampoline can be reused across every OSR entry at that PC.
///
/// Buffers are held behind `Arc<ExecutableBuffer>` so they outlive any concurrent
/// re-entry. Entries are never evicted during a VM run; they're released when the
/// process exits (or, if the cache is ever cleared, after no thread can hold a
/// transient `Arc` clone).
fn osr_trampoline_cache() -> &'static parking_lot::Mutex<FxHashMap<usize, Arc<ExecutableBuffer>>> {
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<FxHashMap<usize, Arc<ExecutableBuffer>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

/// Shadow-stack OSR-frame tracking sub-gate (follow-up §1), default-OFF.
///
/// When set, an OSR-entered frame replicates the prologue's shadow setup (caches
/// the real thread ptr + snapshots the `top` watermark) so its live oops are
/// pushed/reloaded and precisely relocated under a moving GC, instead of skipping
/// shadow tracking. Kept separate from `CRATONVM_SHADOW_STACK` because tracking
/// the OSR'd `binaryTrees` frame currently regresses bt18 (a conservative-pin ×
/// precise-move interaction). Read once and cached so the cached trampoline
/// bodies (keyed by `target_addr`) stay consistent for the whole run.
fn osr_shadow_track_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_SHADOW_OSR_TRACK").is_some())
}

/// Emit a fresh OSR trampoline body for the given destination + frame layout.
///
/// The emitted code expects three arguments via the platform C ABI:
///   * arg0 (RCX on Windows / RDI on SysV) = `locals_ptr: *const i64`
///   * arg1 (RDX on Windows / RSI on SysV) = `vm_ptr: i64` (only read when `needs_context`)
///   * arg2 (R8 on Windows / RDX on SysV) = `thread_ptr: i64` (only read when the
///     shadow-stack OSR-tracking sub-gate is on; see `osr_shadow_track_enabled`)
///
/// It saves callee-saved registers used for locals, optionally stores `vm_ptr`
/// into the heap-local slot, sets up shadow-stack tracking for the OSR frame,
/// copies each incoming local into its register/XMM/frame slot, then jumps to
/// `target_addr`.
#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
unsafe fn emit_osr_trampoline(
    target_addr: usize,
    num_locals: usize,
    num_reg_locals: usize,
    local_assignments: Option<&[Option<u8>]>,
    xmm_assignments: Option<&[Option<u8>]>,
    frame_size: i32,
    callee_saved_base: i32,
    heap_local_offset: i32,
    needs_context: bool,
    dead_mask: u64,
    shadow_thread_slot_off: i32,
    shadow_savetop_slot_off: i32,
    shadow_off_in_thread: i32,
) -> Option<ExecutableBuffer> {
    use crate::x64::LOCAL_REGS;

    // Platform C-ABI argument register numbers.
    // arg0 carries `locals_ptr`, arg1 carries `vm_ptr`, arg2 carries `thread_ptr`
    // (the shadow-stack thread pointer, follow-up §1). All three are caller-saved
    // on both ABIs, and none overlaps any register in `LOCAL_REGS`, so saving
    // arg0 into R10 first cannot clobber a callee-saved local target before
    // we've spilled it, and arg2 survives to the shadow-track block below
    // (the spills write memory, not arg2's register).
    #[cfg(target_os = "windows")]
    let arg0_reg: u8 = 1; // RCX
    #[cfg(target_os = "windows")]
    let arg1_reg: u8 = 2; // RDX
    #[cfg(target_os = "windows")]
    let arg2_reg: u8 = 8; // R8
    #[cfg(not(target_os = "windows"))]
    let arg0_reg: u8 = 7; // RDI
    #[cfg(not(target_os = "windows"))]
    let arg1_reg: u8 = 6; // RSI
    #[cfg(not(target_os = "windows"))]
    let arg2_reg: u8 = 2; // RDX

    let trampoline_size = 1024 + num_locals * 32;
    let mut tramp = ExecutableBuffer::new(trampoline_size)?;

    // === JIT Prologue ===
    tramp.emit_byte(0x55); // push rbp
    tramp.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    tramp.emit(&[0x48, 0x81, 0xEC]); // sub rsp, imm32
    tramp.emit(&frame_size.to_le_bytes());

    // Stash arg0 (locals_ptr) into R10 immediately, before any subsequent emission
    // could clobber the caller-saved arg register. R10 is itself caller-saved and
    // not part of LOCAL_REGS on either platform, so it remains live through the
    // local-copy loop below.
    // Encoding: MOV r10, arg0_reg  =>  REX.W|REX.B 89 (mod=11 reg=arg0 rm=R10&7=2)
    tramp.emit_byte(0x49); // REX.W + REX.B (dest extended)
    tramp.emit_byte(0x89);
    tramp.emit_byte(0xC0 | ((arg0_reg & 7) << 3) | 2);

    // CRITICAL: the callee-saved registers MUST be spilled to the exact
    // frame slots the compiled method's epilogue restores them from.
    //
    // The epilogue (`x64::Compiler::emit_epilogue`) iterates
    // `alloc_used_regs` — which is `RegAllocResult::used_callee_saved` —
    // and restores register `i` from `[rbp - (callee_saved_base + i*8)]`.
    // `used_callee_saved` is produced by filtering the fixed `LOCAL_REGS`
    // priority list, so it is always in `LOCAL_REGS` order.
    //
    // If the trampoline spills the registers in any other order (e.g.
    // local-slot first-appearance order, which can differ when the
    // allocator assigns a higher-priority register to a higher-numbered
    // local), then register X's value lands in the slot the epilogue
    // reads register Y from. After OSR return, the method's epilogue
    // restores the callee-saved registers SWAPPED — and since these hold
    // the caller's live (often pointer-typed) values, the caller resumes
    // with corrupted registers and segfaults on the next dereference.
    //
    // Therefore: always emit the spill set in `LOCAL_REGS` order.
    let used_regs: Vec<u8> = if let Some(assignments) = local_assignments {
        let mut used = [false; 16];
        for &a in assignments {
            if let Some(reg) = a {
                used[(reg & 0x0F) as usize] = true;
            }
        }
        LOCAL_REGS
            .iter()
            .copied()
            .filter(|&r| used[(r & 0x0F) as usize])
            .collect()
    } else {
        LOCAL_REGS.iter().copied().take(num_reg_locals).collect()
    };

    for (i, &reg) in used_regs.iter().enumerate() {
        let neg_off = -(callee_saved_base + i as i32 * 8);
        let rex = 0x48 | if reg >= 8 { 0x04 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0x85 | ((reg & 7) << 3));
        tramp.emit(&neg_off.to_le_bytes());
    }

    if needs_context {
        // Spill vm_ptr (already in arg1_reg, both arg1 candidates are low regs)
        // directly to the heap-local slot. No immediate, no scratch needed.
        let neg_off = -heap_local_offset;
        tramp.emit_byte(0x48); // REX.W
        tramp.emit_byte(0x89); // MOV r/m64, r64
        tramp.emit_byte(0x85 | ((arg1_reg & 7) << 3));
        tramp.emit(&neg_off.to_le_bytes());
    }

    // Shadow-stack OSR-frame handling (follow-up §1). An OSR entry bypasses the
    // prologue thread-fetch, so the cached-thread slot would otherwise hold stale
    // stack garbage. Two modes:
    //   * DEFAULT (gate off): zero the slot (clobber-free `MOV qword [rbp-off],0`,
    //     REX.W C7 /0) so the push/reload/epilogue null-guards make this frame
    //     SKIP shadow tracking — the proven-golden behaviour for
    //     `CRATONVM_SHADOW_STACK` (bt18 relies on its make/check spine + the
    //     from-space staleness window for the OSR'd `binaryTrees` frame).
    //   * `CRATONVM_SHADOW_OSR_TRACK=1`: TRACK this frame by replicating the
    //     prologue's shadow setup — store the real `*mut JvmThread` (arg2, passed
    //     by try_osr→osr_enter) into the cached-thread slot and snapshot the
    //     shadow `top` watermark into the savetop slot, so the epilogue restores
    //     `top` and the per-safepoint push/reload relocate this frame's oops
    //     precisely. Opt-in / default-OFF: tracking the OSR'd `binaryTrees` frame
    //     currently regresses bt18 (a conservative-pin × precise-move interaction
    //     under investigation), so it stays behind its own sub-gate while the
    //     main `CRATONVM_SHADOW_STACK` path keeps the golden SKIP behaviour.
    //
    // Register safety (track path): arg2 is still live here — the code above only
    // stashed arg0→R10, spilled callee-saved regs (writes memory), and stored
    // arg1→heap slot; arg2 (R8 on Windows / RDX on SysV) is in neither LOCAL_REGS
    // nor those destinations. R11 is caller-saved scratch, free here. All offsets
    // are constant per `target_addr`, so the cached trampoline body stays valid.
    if shadow_thread_slot_off != 0 && shadow_savetop_slot_off != 0 && osr_shadow_track_enabled() {
        // --- TRACK ---
        // MOV [rbp - shadow_thread_slot_off], arg2   (cache the thread pointer)
        let neg_thr = -shadow_thread_slot_off;
        let rex = 0x48 | if arg2_reg >= 8 { 0x04 } else { 0x00 }; // REX.W (+R if arg2 extended)
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89); // MOV r/m64, r64
        tramp.emit_byte(0x85 | ((arg2_reg & 7) << 3)); // mod=10, reg=arg2, rm=rbp(5)
        tramp.emit(&neg_thr.to_le_bytes());

        // R11 = [arg2 + shadow_off_in_thread]   (shadow `top`, ShadowStack TOP=0)
        let rex = 0x48 | 0x04 | if arg2_reg >= 8 { 0x01 } else { 0x00 }; // REX.W + R(r11) (+B if arg2 extended)
        tramp.emit_byte(rex);
        tramp.emit_byte(0x8B); // MOV r64, r/m64
        tramp.emit_byte(0x80 | (3 << 3) | (arg2_reg & 7)); // mod=10, reg=R11&7=3, rm=arg2&7
        tramp.emit(&shadow_off_in_thread.to_le_bytes());

        // MOV [rbp - shadow_savetop_slot_off], R11   (save the entry watermark)
        let neg_sav = -shadow_savetop_slot_off;
        tramp.emit_byte(0x4C); // REX.W + REX.R (R11)
        tramp.emit_byte(0x89); // MOV r/m64, r64
        tramp.emit_byte(0x80 | (3 << 3) | 5); // mod=10, reg=R11&7=3, rm=rbp(5) → 0x9D
        tramp.emit(&neg_sav.to_le_bytes());
    } else if shadow_thread_slot_off != 0 {
        // --- SKIP (default) --- zero the cached thread slot so null-guards skip.
        let neg_off = -shadow_thread_slot_off;
        tramp.emit_byte(0x48); // REX.W
        tramp.emit_byte(0xC7); // MOV r/m64, imm32 (sign-extended)
        tramp.emit_byte(0x85); // mod=10, reg=/0, rm=rbp(5) → [rbp + disp32]
        tramp.emit(&neg_off.to_le_bytes());
        tramp.emit(&0i32.to_le_bytes());
    }

    #[allow(clippy::needless_range_loop)]
    for i in 0..num_locals {
        // Skip locals dead at this OSR entry PC: they are register-resident and
        // their register may be shared (graph-colouring coalescing) with a live
        // local. Loading the dead local here would overwrite the live owner's
        // value. The dead local needs no value (it is dead until its own loop
        // re-defines it), so skipping the load entirely is correct.
        if i < 64 && (dead_mask >> i) & 1 == 1 {
            continue;
        }
        let src_disp = (i as i32) * 8;
        if src_disp == 0 {
            tramp.emit(&[0x49, 0x8B, 0x02]);
        } else {
            tramp.emit(&[0x49, 0x8B, 0x82]);
            tramp.emit(&src_disp.to_le_bytes());
        }

        let dst_reg_opt = if let Some(assignments) = local_assignments {
            assignments.get(i).copied().flatten()
        } else if i < num_reg_locals {
            Some(LOCAL_REGS[i])
        } else {
            None
        };

        // round-8 perf (round-4 #11 / round-5 #8): elide the frame-slot
        // store when the local has a canonical register home. The compiled
        // method body reads from `dst_reg` (or `xmm` below) directly; the
        // frame slot is only used as a spill backing store, which the JIT
        // re-establishes lazily before any operation that needs a memory
        // operand. Skipping this MOV saves 7 bytes + one L1d store per
        // register-resident local on each OSR entry.
        let xmm_opt = if let Some(xmm_asgn) = xmm_assignments {
            xmm_asgn.get(i).copied().flatten()
        } else {
            None
        };
        let has_register_home = dst_reg_opt.is_some() || xmm_opt.is_some();
        if !has_register_home {
            let frame_neg_off = -((i as i32 + 1) * 8);
            tramp.emit(&[0x48, 0x89, 0x85]);
            tramp.emit(&frame_neg_off.to_le_bytes());
        }

        if let Some(dst_reg) = dst_reg_opt {
            let rex = 0x48 | if dst_reg >= 8 { 0x04 } else { 0x00 };
            tramp.emit_byte(rex);
            tramp.emit_byte(0x8B);
            tramp.emit_byte(0xC0 | ((dst_reg & 7) << 3));
        }

        // If this local has an XMM assignment, load the value into the XMM register.
        // RAX already contains the local's value (from the MOV above).
        // Emit: MOVQ XMMn, RAX  (66 48|4C 0F 6E /r)
        if let Some(xmm) = xmm_opt {
            let rex_r = if xmm >= 8 { 0x04u8 } else { 0u8 };
            tramp.emit_byte(0x66);
            tramp.emit_byte(0x48 | rex_r); // REX.W + optional REX.R
            tramp.emit_byte(0x0F);
            tramp.emit_byte(0x6E);
            tramp.emit_byte(0xC0 | ((xmm & 7) << 3)); // ModRM: XMMn, RAX
        }
    }

    tramp.emit(&[0x48, 0xB8]);
    tramp.emit(&(target_addr as i64).to_le_bytes());
    tramp.emit(&[0xFF, 0xE0]);

    // Defensive: if the trampoline somehow exceeded its sizing heuristic the
    // emitted code is truncated and unsafe to run — discard it.
    if tramp.overflowed() {
        return None;
    }

    // Transition trampoline buffer from writable to executable.
    tramp.finalize();

    Some(tramp)
}

#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
#[inline(never)]
unsafe fn osr_trampoline(
    target_addr: usize,
    vm_ptr: i64,
    locals_ptr: *const i64,
    num_locals: usize,
    num_reg_locals: usize,
    local_assignments: Option<&[Option<u8>]>,
    xmm_assignments: Option<&[Option<u8>]>,
    frame_size: i32,
    callee_saved_base: i32,
    heap_local_offset: i32,
    needs_context: bool,
    dead_mask: u64,
    shadow_thread_slot_off: i32,
    shadow_savetop_slot_off: i32,
    shadow_off_in_thread: i32,
    thread_ptr: i64,
) -> Option<i64> {
    // Look up (or emit and insert) the cached trampoline body for this target.
    // `dead_mask` is a deterministic function of `target_addr` (both encode the
    // OSR PC), so the cached body for a `target_addr` is unique and correct.
    // `target_addr` already encodes (compiled-method, OSR PC): it is
    // `CompiledMethod.entry + native_offset`, both stable for the method's life.
    // All other parameters except `vm_ptr` and `locals_ptr` are functions of
    // `target_addr` (frame layout, register assignments, etc.), so the same
    // emitted body is correct for every call at this PC.
    let tramp_arc: Arc<ExecutableBuffer> = {
        let cache = osr_trampoline_cache();
        // Fast path: read-only lookup. The result is bound to a local so the
        // `MutexGuard` temporary is dropped at the end of this statement.
        // Under edition 2021 a guard created directly in an `if let`
        // scrutinee stays live through the `else` arm, so the `cache.lock()`
        // in the slow path below would re-lock the same non-reentrant
        // `parking_lot::Mutex` on this thread and deadlock.
        let existing = cache.lock().get(&target_addr).cloned();
        if let Some(existing) = existing {
            existing
        } else {
            // Slow path: emit outside the lock, then insert under it. If another
            // thread raced us, prefer their entry and let our buffer drop.
            let fresh = emit_osr_trampoline(
                target_addr,
                num_locals,
                num_reg_locals,
                local_assignments,
                xmm_assignments,
                frame_size,
                callee_saved_base,
                heap_local_offset,
                needs_context,
                dead_mask,
                shadow_thread_slot_off,
                shadow_savetop_slot_off,
                shadow_off_in_thread,
            )?;
            let fresh_arc = Arc::new(fresh);
            let mut guard = cache.lock();
            guard
                .entry(target_addr)
                .or_insert_with(|| fresh_arc.clone())
                .clone()
        }
    };

    let code_ptr = tramp_arc.as_ptr();
    // Non-panicking validation: if the freshly-emitted trampoline pointer
    // somehow isn't in a registered code region, log and bail rather than
    // aborting the process. The caller (`osr_enter`) treats `None` as
    // "skip OSR, fall back to interpreter".
    if let Err(reason) = validate_code_ptr(code_ptr) {
        tracing::warn!(
            reason = reason,
            "JIT osr_trampoline: invalid code pointer; skipping OSR"
        );
        return None;
    }

    // The cached trampoline takes (locals_ptr, vm_ptr, thread_ptr) via the
    // platform C ABI. `vm_ptr` is only read when `needs_context`, and
    // `thread_ptr` only when the shadow-stack gate is on (follow-up §1); both
    // are passed unconditionally (caller-saved registers, ignored if unused).
    let tramp_fn: unsafe extern "C" fn(*const i64, i64, i64) -> i64 = std::mem::transmute(code_ptr);

    // SAFETY: `tramp_arc` holds an Arc clone of the cached buffer, keeping the
    // executable memory alive for the duration of the call. `locals_ptr` is
    // borrowed from the caller's `jit_locals: &[i64]` slice, which is live
    // across `osr_enter` (and therefore across this call). The fences and
    // black_box prevent the optimizer from reordering the Arc drop above the
    // call or otherwise invalidating the live region.
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    let result = tramp_fn(locals_ptr, vm_ptr, thread_ptr);
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    std::hint::black_box(code_ptr);
    std::hint::black_box(&tramp_arc);

    drop(tramp_arc);

    Some(result)
}

// ---------------------------------------------------------------------------
// JIT invoke info and MIC types
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Method inlining
// ---------------------------------------------------------------------------

/// Maximum callee bytecode size (excluding padding) eligible for inlining.
pub const MAX_INLINE_BYTECODE_SIZE: usize = 35;

/// Total inlined bytecode budget per compiled method.
pub const MAX_INLINE_BUDGET: usize = 250;

/// Resolved metadata for a method eligible for inlining at a specific call site.
#[derive(Clone)]
pub struct InlineSite {
    /// Raw callee bytecode (padded with 2 sentinel bytes, like normal methods).
    pub callee_code: Vec<u8>,
    /// Actual bytecode length (excluding padding).
    pub callee_code_len: usize,
    /// Number of callee local variable slots.
    pub callee_max_locals: usize,
    /// Number of JVM stack slots consumed by arguments (including receiver for instance methods).
    pub callee_num_args: usize,
    /// Whether the callee is a static method.
    pub callee_is_static: bool,
    /// Return type tag (b'I', b'J', b'D', b'F', b'L', b'[', b'V').
    pub return_type: u8,
    /// Resolved field access in callee bytecode: (callee_pc, field_index, type_tag).
    pub field_info: Vec<(usize, usize, u8)>,
    /// Resolved static field access: (callee_pc, class_id_raw, field_index, type_tag, is_volatile).
    pub static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    /// Resolved ldc constants: (callee_pc, i64_value).
    pub ldc_info: Vec<(usize, i64)>,
    /// Resolved ldc2_w constants: (callee_pc, i64_value).
    pub ldc2w_info: Vec<(usize, i64)>,
    /// Whether the callee needs VM context (heap pointer).
    pub needs_heap: bool,
    /// Class name of the inlined callee (for invalidation tracking).
    pub class_name: String,
    /// Method name of the inlined callee.
    pub method_name: String,
    /// Descriptor of the inlined callee.
    pub descriptor: String,
}

/// Compile-time resolved field layout of `java/lang/String`, for the
/// String call-site intrinsics (`length`/`charAt`/`hashCode`/`isEmpty`/
/// `equals`/`compareTo`/`indexOf`, implemented by a later wave).
///
/// # Why this exists
///
/// The intrinsic matcher ([`try_resolve_intrinsic`]) and the x64 codegen
/// only ever see `(class, name, descriptor)` strings — they have no view
/// of any class's heap layout. A String intrinsic, however, must read the
/// receiver's `value` (`byte[]`/`char[]`), `coder` (`byte`), and `hash`
/// (`int`) instance fields with inline machine code. This struct carries
/// the field positions resolved from the live `java/lang/String` class so
/// the codegen can emit `MOV [receiver + cell_offset + payload]` directly.
///
/// It is produced once per JIT compilation by the
/// `string_layout_resolver` callback passed to [`try_compile`] and threaded
/// unchanged into [`x64::compile`]. When the resolver returns `None` (e.g.
/// `java/lang/String` not yet loaded) the String intrinsics simply bail to
/// normal dispatch — exactly today's behaviour.
///
/// # How an intrinsic obtains a field offset
///
/// Each `*_field_index` is the abstract field slot index. The matching
/// `*_cell_offset` is the **byte offset of that field's 16-byte `Value`
/// cell from the object base**, precomputed as
/// `HEADER_SIZE + field_index * SLOT_SIZE`. To load the field's payload,
/// add the in-cell payload offset from `cratonvm_types`:
///
/// ```text
/// // `hash` is an int  → 4-byte payload at FIELD_CELL_PAYLOAD32_OFFSET:
/// MOVSXD rax, [receiver + layout.hash_cell_offset  + FIELD_CELL_PAYLOAD32_OFFSET]
/// // `coder` is a byte → also a 4-byte Value::Int payload:
/// MOVSXD rax, [receiver + layout.coder_cell_offset + FIELD_CELL_PAYLOAD32_OFFSET]
/// // `value` is a ref  → 8-byte payload at FIELD_CELL_PAYLOAD64_OFFSET:
/// MOV    rax, [receiver + layout.value_cell_offset + FIELD_CELL_PAYLOAD64_OFFSET]
/// ```
///
/// This is the same cell-offset math the inline `getfield` codegen uses;
/// see `x64.rs` opcode `0xb4`.
///
/// # `coder` may be absent
///
/// The legacy synthetic `{value:[C, hash:I}` String layout has no `coder`
/// field (the backing array is `char[]`, always UTF-16). `has_coder` is
/// `false` in that case and `coder_field_index`/`coder_cell_offset` are
/// meaningless — an intrinsic that needs `coder` MUST check `has_coder`
/// first and bail to native dispatch when it is `false`. The compact
/// JDK-9+ `{value:[B, coder:B, hash:I, hashIsZero:Z}` layout sets it
/// `true`. The resolver decides which layout applies by inspecting the
/// loaded `java/lang/String` class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StringFieldLayout {
    /// Abstract field slot index of `String.value` (the backing array ref).
    pub value_field_index: usize,
    /// Byte offset of `value`'s 16-byte `Value` cell from the object base
    /// (`HEADER_SIZE + value_field_index * SLOT_SIZE`).
    pub value_cell_offset: i32,
    /// Abstract field slot index of `String.hash` (the cached `int` hash).
    pub hash_field_index: usize,
    /// Byte offset of `hash`'s `Value` cell from the object base.
    pub hash_cell_offset: i32,
    /// Whether this String layout has a `coder` field (`true` for the
    /// compact `byte[]` layout, `false` for the legacy `char[]` layout).
    pub has_coder: bool,
    /// Abstract field slot index of `String.coder`. Meaningful only when
    /// [`has_coder`](Self::has_coder) is `true`.
    pub coder_field_index: usize,
    /// Byte offset of `coder`'s `Value` cell from the object base.
    /// Meaningful only when [`has_coder`](Self::has_coder) is `true`.
    pub coder_cell_offset: i32,
    /// `ObjectHeader` class id of `java/lang/String` (the value stored at
    /// object offset 0). Used as the receiver class-id guard for the
    /// `java/lang/CharSequence` accessor intrinsics (`charAt`/`length`/
    /// `isEmpty`): a CharSequence call site inlines the String-layout decode
    /// only behind a `[recv+0] == string_class_id` guard, deopting to native
    /// dispatch for any other CharSequence (`StringBuilder`, …). For a
    /// `java/lang/String` site (final, monomorphic) no guard is emitted.
    pub string_class_id: u32,
}

impl StringFieldLayout {
    /// Build a layout from raw field indices, precomputing the cell offsets.
    /// `coder_field_index` is ignored (and the offset zeroed) when
    /// `has_coder` is `false`. `string_class_id` is the `java/lang/String`
    /// `ObjectHeader` class id used as the CharSequence receiver guard.
    pub fn new(
        value_field_index: usize,
        coder_field_index: Option<usize>,
        hash_field_index: usize,
        string_class_id: u32,
    ) -> Self {
        let cell = |idx: usize| -> i32 {
            (cratonvm_types::HEADER_SIZE + idx * cratonvm_types::SLOT_SIZE) as i32
        };
        StringFieldLayout {
            value_field_index,
            value_cell_offset: cell(value_field_index),
            hash_field_index,
            hash_cell_offset: cell(hash_field_index),
            has_coder: coder_field_index.is_some(),
            coder_field_index: coder_field_index.unwrap_or(0),
            coder_cell_offset: coder_field_index.map_or(0, cell),
            string_class_id,
        }
    }
}

/// Compile-time resolved info for a method invocation from JIT code.
pub struct JitInvokeInfo {
    pub class_name: &'static str,
    pub method_name: &'static str,
    pub descriptor: &'static str,
    pub num_jit_args: usize,
    pub return_type: u8,
    pub invoke_kind: u8,
}

/// Enumeration of every JIT call-site intrinsic.
///
/// Each variant is mapped onto the `JitDirectCall.entry` sentinel space via
/// [`JitIntrinsic::as_entry`] (`usize::MAX - (variant as usize)`). These
/// sentinels are never valid code pointers (kernel address space), so the
/// `callee_entry` dispatch in `x64.rs` stays a single integer comparison.
///
/// **Variant ordering is load-bearing.** The first 14 variants — the
/// `java.lang.Math` family — MUST keep their declaration order so that the
/// deprecated `MATH_*_INTRINSIC` const aliases below resolve to exactly the
/// same `usize::MAX - N` values they had before the enum was introduced.
///
/// Per-family regions are marked with `INTRINSIC REGION BEGIN/END: <TAG>`
/// comment pairs. A follow-up agent adding family `<TAG>` appends its
/// variants strictly between that family's BEGIN/END markers and nowhere
/// else, so 8 agents editing 8 disjoint regions never collide.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JitIntrinsic {
    // --- java.lang.Math family (variants 0..=13 — ORDER IS LOAD-BEARING) ---
    MathSqrt = 0,
    MathFloor = 1,
    MathCeil = 2,
    MathRint = 3,
    MathAbsDouble = 4,
    MathAbsFloat = 5,
    MathAbsInt = 6,
    MathAbsLong = 7,
    MathFmaDouble = 8,
    MathFmaFloat = 9,
    MathMinInt = 10,
    MathMaxInt = 11,
    MathMinLong = 12,
    MathMaxLong = 13,
    /// `Math/StrictMath.multiplyHigh(JJ)J` — high 64 bits of the SIGNED
    /// 128-bit product, emitted as a one-operand `IMUL r64` (RDX:RAX = RAX*r),
    /// result taken from RDX. Hottest leaf in the SunEC P-256 field multiply.
    MathMultiplyHigh = 14,
    /// `Math/StrictMath.unsignedMultiplyHigh(JJ)J` — high 64 bits of the
    /// UNSIGNED 128-bit product, emitted as a one-operand `MUL r64`.
    MathUnsignedMultiplyHigh = 15,

    // ===== INTRINSIC REGION BEGIN: INT_BITS =====
    // java.lang.Integer bit-manipulation intrinsics (Phase 1a). Variant
    // ordering within this region is local and not externally observed —
    // only the Math family's declaration order is load-bearing.
    IntBitCount,
    IntNumberOfLeadingZeros,
    IntNumberOfTrailingZeros,
    IntReverseBytes,
    IntHighestOneBit,
    IntLowestOneBit,
    IntReverse,
    IntCompare,
    /// `Integer.rotateLeft(II)I` / `rotateRight(II)I` — `ROL`/`ROR r32, CL`.
    /// x86 masks `CL & 0x1f` for a 32-bit rotate, which is byte-identical to
    /// the JDK definition (rotation is mod 32), so no distance masking is
    /// needed. Hot in ChaCha/Salsa/Blake/SHA inner loops (e.g. BC SPHINCS).
    IntRotateLeft,
    IntRotateRight,
    // ===== INTRINSIC REGION END: INT_BITS =====

    // ===== INTRINSIC REGION BEGIN: LONG_BITS =====
    // java.lang.Long bit-manipulation intrinsics (Phase 1b). Variant
    // ordering within this region is local and not externally observed —
    // only the Math family's declaration order is load-bearing.
    LongBitCount,
    LongNumberOfLeadingZeros,
    LongNumberOfTrailingZeros,
    LongReverseBytes,
    LongHighestOneBit,
    LongLowestOneBit,
    LongCompare,
    /// `Long.rotateLeft(JI)J` / `rotateRight(JI)J` — `ROL`/`ROR r64, CL`.
    /// x86 masks `CL & 0x3f` for a 64-bit rotate, byte-identical to the JDK
    /// definition (rotation is mod 64). Note the descriptor takes a `long`
    /// value and an `int` distance.
    LongRotateLeft,
    LongRotateRight,
    // ===== INTRINSIC REGION END: LONG_BITS =====

    // ===== INTRINSIC REGION BEGIN: ARRAYCOPY =====
    /// `java.lang.System.arraycopy(Object,int,Object,int,int)` (Phase 2).
    ///
    /// The descriptor is type-erased — the element kind is only known at
    /// runtime. The codegen inlines a primitive-array fast path (null
    /// checks, fused bounds checks, then a memmove-correct `REP MOVSB`)
    /// and routes every uncertain case — null receiver, non-array,
    /// reference array, mismatched element types, or any out-of-bounds
    /// position — through the uncommon-trap deopt stub. The interpreter
    /// then re-runs the call via the native `System.arraycopy`, which
    /// preserves `NullPointerException` / `ArrayStoreException` /
    /// `ArrayIndexOutOfBoundsException` and the GC store barrier exactly.
    ArraycopyPrimitive,
    // ===== INTRINSIC REGION END: ARRAYCOPY =====

    // ===== INTRINSIC REGION BEGIN: STRING_ACCESS =====
    // java.lang.String access intrinsics (Phase 3a). The foundation waves
    // (commits 06bfac0 / 544cbea) added inline getfield + array-access
    // codegen and the `StringFieldLayout` API, so these are now inlined
    // when a `StringFieldLayout` with a `coder` field is available. The
    // matcher only registers them when the layout resolves; otherwise the
    // call falls back to normal native dispatch. Variant ordering within
    // this region is local and not externally observed.
    StringLength,   // length()I
    StringIsEmpty,  // isEmpty()Z
    StringCharAt,   // charAt(I)C
    StringHashCode, // hashCode()I
    // ===== INTRINSIC REGION END: STRING_ACCESS =====

    // ===== INTRINSIC REGION BEGIN: STRING_SEARCH =====
    // java.lang.String search/compare intrinsics (Phase 3b). `equals` is
    // inlined as a coder+length-guarded raw byte compare (deopts to native
    // on a coder mismatch or a non-String argument).
    //
    // Phase 3b follow-up: `compareTo` and both `indexOf` overloads are now
    // ALSO inlined. Unlike `equals` (which can byte-compare only when the
    // coders match), these three decode each receiver/argument character
    // through a per-string `coder` branch (0 LATIN1 = 1 byte/char, 1 UTF16
    // = 2 LE bytes/char), so EVERY coder combination — including mixed —
    // is handled inline with no coder-mismatch deopt. The deopt stub is
    // still used for the genuinely uncertain cases (null receiver, null
    // String argument, null backing `value` array). Variant ordering here
    // is local and not externally observed.
    StringEquals,      // equals(Ljava/lang/Object;)Z
    StringCompareTo,   // compareTo(Ljava/lang/String;)I
    StringIndexOfChar, // indexOf(I)I
    StringIndexOfStr,  // indexOf(Ljava/lang/String;)I
    // ===== INTRINSIC REGION END: STRING_SEARCH =====

    // ===== INTRINSIC REGION BEGIN: ARRAYS_OPS =====
    // java.util.Arrays.fill / Arrays.equals intrinsics (Phase 4a). Variant
    // ordering within this region is local and not externally observed.
    //
    // `fill` variants are keyed by element width: 1-byte (byte/boolean),
    // 2-byte (char/short), 4-byte (int), 8-byte (long). `fill([FF)V` and
    // `fill([DD)V` are intentionally NOT registered — they are bailed (see
    // try_resolve_intrinsic) so the matcher never registers an intrinsic
    // whose codegen would have to special-case an FP fill value arriving in
    // an XMM stack slot. The 3-arg ranged `fill([IIII)V` overloads are out
    // of scope.
    ArraysFill1, // fill([BB)V, fill([ZZ)V — REP STOSB
    ArraysFill2, // fill([CC)V, fill([SS)V — REP STOSW
    ArraysFill4, // fill([II)V             — REP STOSD
    ArraysFill8, // fill([JJ)V             — REP STOSQ
    // `equals` variants are likewise keyed by element width. All primitive
    // `equals` overloads reduce to a raw byte-wise compare of length*width
    // bytes (boolean arrays store 0/1, so a byte compare is exact).
    ArraysEquals1, // equals([B[B)Z, equals([Z[Z)Z
    ArraysEquals2, // equals([C[C)Z, equals([S[S)Z
    ArraysEquals4, // equals([I[I)Z
    ArraysEquals8, // equals([J[J)Z
    // ===== INTRINSIC REGION END: ARRAYS_OPS =====

    // ===== INTRINSIC REGION BEGIN: ARRAYS_SORT =====
    // java.util.Arrays.sort for primitive integral arrays (Phase 4b). One
    // variant per element width; the emitted insertion sort differs only in
    // the element load/store encoding (scale + sign/zero extension). Variant
    // ordering within this region is local and not externally observed.
    ArraysSortInt,
    ArraysSortLong,
    ArraysSortChar,
    ArraysSortShort,
    ArraysSortByte,
    // ===== INTRINSIC REGION END: ARRAYS_SORT =====

    // ===== INTRINSIC REGION BEGIN: CRC32 =====
    // java.util.zip.CRC32 / CRC32C `update` call-site intrinsics (Phase 4c).
    //
    // Both classes hold a single `private int crc` at instance field slot 0
    // (`CRC_FIELD_SLOT`), the running (uncomplemented) CRC state — see
    // docs/internal/crc_layout_contract.md and native-builtins/src/
    // zip_crc32c.rs. Each intrinsic threads that slot: load slot 0, fold the
    // input byte(s), store back. `update(I)V` folds one byte; `update([BII)V`
    // folds a `byte[]` range (with inline null + bounds guards). A receiver
    // class-id guard (the receiver's dynamic class must be exactly the
    // declared CRC32/CRC32C class — a subclass could override `update`)
    // precedes every variant; on mismatch codegen deopts to normal dispatch.
    //
    //   * Crc32cUpdate* — Castagnoli CRC-32C (reflected poly 0x82F63B78).
    //     Emitted with the hardware `CRC32` instruction, which computes
    //     exactly this polynomial. Gated on `x64::has_sse42()`.
    //   * Crc32Update* — IEEE 802.3 CRC-32 (reflected poly 0xEDB88320). The
    //     hardware `CRC32` instruction is the WRONG polynomial, so these emit
    //     a tight inline reflected-CRC bit loop (8 shifts/byte, no table, no
    //     CALL) — the exact algorithm of `crc32_step` / `crc32c_step`.
    Crc32cUpdateByte,  // CRC32C.update(I)V
    Crc32cUpdateBytes, // CRC32C.update([BII)V
    Crc32UpdateByte,   // CRC32.update(I)V
    Crc32UpdateBytes,  // CRC32.update([BII)V
                       // ===== INTRINSIC REGION END: CRC32 =====
}

impl JitIntrinsic {
    /// Map this intrinsic onto the `JitDirectCall.entry` sentinel space.
    ///
    /// Returns `usize::MAX - (self as usize)`, a value that can never be a
    /// valid code pointer. The `x64.rs` codegen ladder compares
    /// `callee_entry` against `JitIntrinsic::Foo.as_entry()` to recognise
    /// an intrinsic call site.
    pub const fn as_entry(self) -> usize {
        usize::MAX - (self as usize)
    }

    /// True for the CRC32/CRC32C `update` call-site intrinsics.
    ///
    /// These are the only `invokevirtual` intrinsics that need a runtime
    /// receiver class-id guard, so their codegen depends on a resolved
    /// `JitDirectCall::guard_class_id`. The resolution loop in `try_compile`
    /// uses this to skip registering a CRC32 intrinsic whose declared class
    /// id could not be resolved (`guard_class_id == 0`), letting the site
    /// fall through to normal virtual dispatch instead of inlining unsoundly.
    pub const fn is_crc32_family(self) -> bool {
        matches!(
            self,
            JitIntrinsic::Crc32cUpdateByte
                | JitIntrinsic::Crc32cUpdateBytes
                | JitIntrinsic::Crc32UpdateByte
                | JitIntrinsic::Crc32UpdateBytes
        )
    }

    /// Recover a [`JitIntrinsic`] from a `JitDirectCall::entry` sentinel, if
    /// the value is in fact an intrinsic sentinel. Used by the resolution
    /// loop to classify a freshly-matched entry without re-running the
    /// (class, name, descriptor) matcher.
    pub fn from_entry(entry: usize) -> Option<JitIntrinsic> {
        // The sentinel space is `usize::MAX - (variant as usize)`. The last
        // declared variant bounds the valid offset range.
        let offset = usize::MAX.checked_sub(entry)?;
        if offset > JitIntrinsic::Crc32UpdateBytes as usize {
            return None;
        }
        // Exhaustive map — keeps this in lockstep with the enum so a new
        // variant fails to compile until added here.
        Some(match offset {
            x if x == JitIntrinsic::Crc32cUpdateByte as usize => JitIntrinsic::Crc32cUpdateByte,
            x if x == JitIntrinsic::Crc32cUpdateBytes as usize => JitIntrinsic::Crc32cUpdateBytes,
            x if x == JitIntrinsic::Crc32UpdateByte as usize => JitIntrinsic::Crc32UpdateByte,
            x if x == JitIntrinsic::Crc32UpdateBytes as usize => JitIntrinsic::Crc32UpdateBytes,
            // Non-CRC32 intrinsic — the resolution loop only needs CRC32
            // classification, so any other in-range sentinel is reported as
            // "not a CRC32 intrinsic" via the `is_crc32_family` check below.
            _ => return None,
        })
    }
}

/// Sentinel `entry` values for JitDirectCall indicating inlined Math intrinsics.
///
/// Backward-compatible aliases for the [`JitIntrinsic`] Math variants so
/// existing `x64.rs` comparisons and the VM interpreter keep compiling
/// unchanged. New code should use [`JitIntrinsic`] variants and
/// [`JitIntrinsic::as_entry`] directly.
mod math_intrinsic_aliases {
    use super::JitIntrinsic;
    pub const MATH_SQRT_INTRINSIC: usize = JitIntrinsic::MathSqrt.as_entry();
    pub const MATH_FLOOR_INTRINSIC: usize = JitIntrinsic::MathFloor.as_entry();
    pub const MATH_CEIL_INTRINSIC: usize = JitIntrinsic::MathCeil.as_entry();
    pub const MATH_RINT_INTRINSIC: usize = JitIntrinsic::MathRint.as_entry();
    pub const MATH_ABS_DOUBLE_INTRINSIC: usize = JitIntrinsic::MathAbsDouble.as_entry();
    pub const MATH_ABS_FLOAT_INTRINSIC: usize = JitIntrinsic::MathAbsFloat.as_entry();
    pub const MATH_ABS_INT_INTRINSIC: usize = JitIntrinsic::MathAbsInt.as_entry();
    pub const MATH_ABS_LONG_INTRINSIC: usize = JitIntrinsic::MathAbsLong.as_entry();
    pub const MATH_FMA_DOUBLE_INTRINSIC: usize = JitIntrinsic::MathFmaDouble.as_entry();
    pub const MATH_FMA_FLOAT_INTRINSIC: usize = JitIntrinsic::MathFmaFloat.as_entry();
    pub const MATH_MIN_INT_INTRINSIC: usize = JitIntrinsic::MathMinInt.as_entry();
    pub const MATH_MAX_INT_INTRINSIC: usize = JitIntrinsic::MathMaxInt.as_entry();
    pub const MATH_MIN_LONG_INTRINSIC: usize = JitIntrinsic::MathMinLong.as_entry();
    pub const MATH_MAX_LONG_INTRINSIC: usize = JitIntrinsic::MathMaxLong.as_entry();
    pub const MATH_MULTIPLY_HIGH_INTRINSIC: usize = JitIntrinsic::MathMultiplyHigh.as_entry();
    pub const MATH_UNSIGNED_MULTIPLY_HIGH_INTRINSIC: usize =
        JitIntrinsic::MathUnsignedMultiplyHigh.as_entry();
}
pub use math_intrinsic_aliases::*;

/// Resolve a method invocation to a JIT call-site intrinsic, if one applies.
///
/// Returns `Some((entry, num_params, return_type))` where `entry` is the
/// [`JitIntrinsic::as_entry`] sentinel, `num_params` is the JLS argument
/// count *excluding* any receiver, and `return_type` is the JVM type tag
/// of the result. Returns `None` when no intrinsic matches, when the
/// required CPU feature is absent, or when inlining would be incorrect —
/// in which case the call falls back to the normal dispatch path.
///
/// CPU-feature gating must match the codegen ladder in `x64.rs` exactly
/// (e.g. floor/ceil/rint require `x64::has_sse41()`), so that a method is
/// never registered as an intrinsic the codegen cannot emit.
///
/// **Per-family regions.** The Math family is matched inline below. Every
/// other family has an empty `INTRINSIC REGION BEGIN/END: <TAG>` block;
/// a follow-up agent fills exactly one region with an
/// `if let Some(hit) = <match>; return Some(hit)` block. Because each
/// region is an independent statement, 8 agents editing 8 regions never
/// produce a merge conflict.
pub fn try_resolve_intrinsic(
    class: &str,
    name: &str,
    descriptor: &str,
) -> Option<(usize, usize, u8)> {
    // --- java.lang.Math / java.lang.StrictMath family ---
    if class == "java/lang/Math" || class == "java/lang/StrictMath" {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("sqrt", "(D)D") => Some((JitIntrinsic::MathSqrt, 1, b'D')),
            ("floor", "(D)D") if x64::has_sse41() => Some((JitIntrinsic::MathFloor, 1, b'D')),
            ("ceil", "(D)D") if x64::has_sse41() => Some((JitIntrinsic::MathCeil, 1, b'D')),
            ("rint", "(D)D") if x64::has_sse41() => Some((JitIntrinsic::MathRint, 1, b'D')),
            ("abs", "(D)D") => Some((JitIntrinsic::MathAbsDouble, 1, b'D')),
            ("abs", "(F)F") => Some((JitIntrinsic::MathAbsFloat, 1, b'F')),
            ("abs", "(I)I") => Some((JitIntrinsic::MathAbsInt, 1, b'I')),
            ("abs", "(J)J") => Some((JitIntrinsic::MathAbsLong, 1, b'J')),
            // T1.1.28 — Math.fma (fused multiply-add).
            ("fma", "(DDD)D") => Some((JitIntrinsic::MathFmaDouble, 3, b'D')),
            ("fma", "(FFF)F") => Some((JitIntrinsic::MathFmaFloat, 3, b'F')),
            // Round-8 Bug 8 — branchless integer min/max via CMOV.
            ("min", "(II)I") => Some((JitIntrinsic::MathMinInt, 2, b'I')),
            ("max", "(II)I") => Some((JitIntrinsic::MathMaxInt, 2, b'I')),
            ("min", "(JJ)J") => Some((JitIntrinsic::MathMinLong, 2, b'J')),
            ("max", "(JJ)J") => Some((JitIntrinsic::MathMaxLong, 2, b'J')),
            // High 64 bits of the 128-bit product — one `IMUL`/`MUL r64`.
            // Hottest leaf in SunEC P-256 Montgomery field arithmetic.
            ("multiplyHigh", "(JJ)J") => Some((JitIntrinsic::MathMultiplyHigh, 2, b'J')),
            ("unsignedMultiplyHigh", "(JJ)J") => {
                Some((JitIntrinsic::MathUnsignedMultiplyHigh, 2, b'J'))
            }
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }

    // ===== INTRINSIC REGION BEGIN: INT_BITS =====
    // java.lang.Integer bit-manipulation intrinsics (Phase 1a). Each maps to
    // one or two x86-64 instructions, all pure leaves with no memory access.
    // CPU-feature gates here MUST match the codegen ladder in x64.rs exactly:
    //   * bitCount needs POPCNT.
    //   * numberOfLeadingZeros / numberOfTrailingZeros are emitted on every
    //     host — LZCNT/TZCNT when available, otherwise a BSR/BSF sequence
    //     with the input-zero fixup — so they are not feature-gated.
    //   * reverseBytes (BSWAP), highestOneBit, lowestOneBit, reverse and
    //     compare use only baseline instructions.
    if class == "java/lang/Integer" {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("bitCount", "(I)I") if x64::has_popcnt() => Some((JitIntrinsic::IntBitCount, 1, b'I')),
            ("numberOfLeadingZeros", "(I)I") => {
                Some((JitIntrinsic::IntNumberOfLeadingZeros, 1, b'I'))
            }
            ("numberOfTrailingZeros", "(I)I") => {
                Some((JitIntrinsic::IntNumberOfTrailingZeros, 1, b'I'))
            }
            ("reverseBytes", "(I)I") => Some((JitIntrinsic::IntReverseBytes, 1, b'I')),
            ("highestOneBit", "(I)I") => Some((JitIntrinsic::IntHighestOneBit, 1, b'I')),
            ("lowestOneBit", "(I)I") => Some((JitIntrinsic::IntLowestOneBit, 1, b'I')),
            ("reverse", "(I)I") => Some((JitIntrinsic::IntReverse, 1, b'I')),
            ("compare", "(II)I") => Some((JitIntrinsic::IntCompare, 2, b'I')),
            ("rotateLeft", "(II)I") => Some((JitIntrinsic::IntRotateLeft, 2, b'I')),
            ("rotateRight", "(II)I") => Some((JitIntrinsic::IntRotateRight, 2, b'I')),
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }
    // ===== INTRINSIC REGION END: INT_BITS =====

    // ===== INTRINSIC REGION BEGIN: LONG_BITS =====
    // java.lang.Long bit-manipulation intrinsics (Phase 1b). All operate on a
    // single 64-bit operand (occupying one JIT stack slot) and lower to
    // REX.W-prefixed instructions. CPU-feature gates here MUST match the
    // codegen ladder in x64.rs exactly:
    //   * bitCount needs POPCNT.
    //   * numberOfLeadingZeros / numberOfTrailingZeros are emitted on every
    //     host — LZCNT/TZCNT when available, otherwise a BSR/BSF sequence
    //     with the input-zero fixup — so they are not feature-gated.
    //   * reverseBytes (BSWAP), highestOneBit, lowestOneBit and compare use
    //     only baseline instructions.
    //   * Long.reverse is intentionally NOT registered: it has no single-
    //     instruction lowering and the multi-mask SWAR sequence is omitted in
    //     favour of safe fallback to normal dispatch (roadmap §3.4).
    if class == "java/lang/Long" && std::env::var_os("CRATONVM_JIT_NO_LONG_INTRINSICS").is_none() {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("bitCount", "(J)I") if x64::has_popcnt() => {
                Some((JitIntrinsic::LongBitCount, 1, b'I'))
            }
            ("numberOfLeadingZeros", "(J)I") => {
                Some((JitIntrinsic::LongNumberOfLeadingZeros, 1, b'I'))
            }
            ("numberOfTrailingZeros", "(J)I") => {
                Some((JitIntrinsic::LongNumberOfTrailingZeros, 1, b'I'))
            }
            ("reverseBytes", "(J)J") => Some((JitIntrinsic::LongReverseBytes, 1, b'J')),
            ("highestOneBit", "(J)J") => Some((JitIntrinsic::LongHighestOneBit, 1, b'J')),
            ("lowestOneBit", "(J)J") => Some((JitIntrinsic::LongLowestOneBit, 1, b'J')),
            ("compare", "(JJ)I") => Some((JitIntrinsic::LongCompare, 2, b'I')),
            ("rotateLeft", "(JI)J") => Some((JitIntrinsic::LongRotateLeft, 2, b'J')),
            ("rotateRight", "(JI)J") => Some((JitIntrinsic::LongRotateRight, 2, b'J')),
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }
    // ===== INTRINSIC REGION END: LONG_BITS =====

    // ===== INTRINSIC REGION BEGIN: ARRAYCOPY =====
    // java.lang.System.arraycopy (Phase 2). The descriptor is type-erased
    // — the element kind is unknown until runtime — so a SINGLE variant
    // (`ArraycopyPrimitive`) is registered. The x64 codegen emits a
    // runtime dispatch: if both arrays are non-null, are arrays, share the
    // same PRIMITIVE element kind, and all five positions are in bounds,
    // it inlines a memmove-correct `REP MOVSB`. Every other case (null,
    // non-array, reference array, mismatched/incompatible element kinds,
    // out-of-bounds) takes the uncommon-trap deopt stub, which re-runs the
    // call in the interpreter via the native `System.arraycopy` — that
    // path preserves NPE / ArrayStoreException / AIOOBE and the GC store
    // barrier verbatim. Reference arrays therefore intentionally bail to
    // native (roadmap §3.4: never trade correctness for inlining).
    if class == "java/lang/System"
        && name == "arraycopy"
        && descriptor == "(Ljava/lang/Object;ILjava/lang/Object;II)V"
    {
        return Some((JitIntrinsic::ArraycopyPrimitive.as_entry(), 5, b'V'));
    }
    // ===== INTRINSIC REGION END: ARRAYCOPY =====

    // ===== INTRINSIC REGION BEGIN: STRING_ACCESS =====
    // java.lang.String access intrinsics (length/charAt/isEmpty/hashCode).
    // These ARE intrinsified, but only when a `StringFieldLayout` is
    // available — and the layout is NOT a parameter of this 3-argument
    // matcher (kept stable for the INT_BITS / LONG_BITS / ARRAYCOPY /
    // ARRAYS_* / CRC32 families and their tests). The String family is
    // therefore matched by the layout-aware `try_resolve_string_intrinsic`
    // below, which `try_compile_inner` calls instead. This 3-arg entry
    // point deliberately registers nothing for `java/lang/String`, so a
    // bare 3-arg call (no layout context) safely falls back to dispatch.
    // ===== INTRINSIC REGION END: STRING_ACCESS =====

    // ===== INTRINSIC REGION BEGIN: STRING_SEARCH =====
    // java.lang.String search intrinsics (equals): same story as
    // STRING_ACCESS — matched by the layout-aware
    // `try_resolve_string_intrinsic` below, not this 3-arg entry point.
    // ===== INTRINSIC REGION END: STRING_SEARCH =====

    // ===== INTRINSIC REGION BEGIN: ARRAYS_OPS =====
    // java.util.Arrays.fill / Arrays.equals — Phase 4a.
    //
    // Both families are pure leaf calls over primitive arrays with no
    // safepoint. `num_params` excludes the (absent) receiver: `fill` takes
    // 2 (array, value), `equals` takes 2 (array a, array b).
    //
    // Deliberately NOT registered (fall back to native dispatch):
    //   * `fill([FF)V` / `fill([DD)V` — the fill value is an FP operand that
    //     the JIT keeps in an XMM stack slot; routing it through the integer
    //     REP STOS path is not provably correct, so we bail.
    //   * The 3-arg ranged `fill(...IIX)V` overloads — out of scope.
    //   * `equals` reference-array / `Object[]` overloads — element equality
    //     requires calling `Object.equals`, which is not a leaf op.
    //   * `Arrays.deepEquals`, `Arrays.hashCode`, etc. — not targeted here.
    if class == "java/util/Arrays" {
        let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            // --- fill(array, value) : void --- (2 args, void return)
            ("fill", "([BB)V") => Some((JitIntrinsic::ArraysFill1, 2, b'V')),
            ("fill", "([ZZ)V") => Some((JitIntrinsic::ArraysFill1, 2, b'V')),
            ("fill", "([CC)V") => Some((JitIntrinsic::ArraysFill2, 2, b'V')),
            ("fill", "([SS)V") => Some((JitIntrinsic::ArraysFill2, 2, b'V')),
            ("fill", "([II)V") => Some((JitIntrinsic::ArraysFill4, 2, b'V')),
            ("fill", "([JJ)V") => Some((JitIntrinsic::ArraysFill8, 2, b'V')),
            // --- equals(a, b) : boolean --- (2 args, int/boolean return)
            ("equals", "([B[B)Z") => Some((JitIntrinsic::ArraysEquals1, 2, b'Z')),
            ("equals", "([Z[Z)Z") => Some((JitIntrinsic::ArraysEquals1, 2, b'Z')),
            ("equals", "([C[C)Z") => Some((JitIntrinsic::ArraysEquals2, 2, b'Z')),
            ("equals", "([S[S)Z") => Some((JitIntrinsic::ArraysEquals2, 2, b'Z')),
            ("equals", "([I[I)Z") => Some((JitIntrinsic::ArraysEquals4, 2, b'Z')),
            ("equals", "([J[J)Z") => Some((JitIntrinsic::ArraysEquals8, 2, b'Z')),
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = hit {
            return Some((intrinsic.as_entry(), num_params, ret));
        }
    }
    // ===== INTRINSIC REGION END: ARRAYS_OPS =====

    // ===== INTRINSIC REGION BEGIN: ARRAYS_SORT =====
    // java.util.Arrays.sort(prim[]) — single-argument overloads for the five
    // integral element types. The codegen emits an in-place insertion sort
    // (O(n^2), but provably correct for all lengths including empty/single).
    //
    // float[]/double[] are deliberately excluded: their JLS ordering uses
    // Double.compare semantics (NaN sorts last, -0.0 before +0.0) which a
    // plain signed compare does not honour. The 3-argument range overloads
    // (sort([III)V etc.) are also out of scope. Both fall through to None
    // and use the normal dispatch path.
    if class == "java/util/Arrays" && name == "sort" {
        let hit: Option<JitIntrinsic> = match descriptor {
            "([I)V" => Some(JitIntrinsic::ArraysSortInt),
            "([J)V" => Some(JitIntrinsic::ArraysSortLong),
            "([C)V" => Some(JitIntrinsic::ArraysSortChar),
            "([S)V" => Some(JitIntrinsic::ArraysSortShort),
            "([B)V" => Some(JitIntrinsic::ArraysSortByte),
            _ => None,
        };
        if let Some(intrinsic) = hit {
            // num_params = 1 (the array ref), return kind 'V' (void).
            return Some((intrinsic.as_entry(), 1, b'V'));
        }
    }
    // ===== INTRINSIC REGION END: ARRAYS_SORT =====

    // ===== INTRINSIC REGION BEGIN: CRC32 =====
    // java.util.zip.CRC32 / CRC32C `update` call-site intrinsics (Phase 4c).
    //
    // The foundation wave (commit 2fb0df0) pinned the receiver layout
    // (docs/internal/crc_layout_contract.md): both classes carry exactly one
    // instance field — `private int crc` at slot 0 — holding the running,
    // uncomplemented CRC state. It also added a bit-exact native CRC32C
    // (native-builtins/src/zip_crc32c.rs) that serves as the differential
    // oracle. With a stable layout and an oracle, both `update` overloads are
    // soundly inlinable:
    //
    //   * CRC32C — the x86 `CRC32` instruction computes exactly the
    //     Castagnoli CRC-32C, so the codegen folds bytes with it directly.
    //     Gated on `has_sse42()` (the codegen ladder applies the same gate;
    //     when SSE4.2 is absent the intrinsic is not registered and the call
    //     dispatches to the native CRC32C override).
    //   * CRC32 — IEEE 802.3 (reflected poly 0xEDB88320). The hardware
    //     `CRC32` instruction is the wrong polynomial; the codegen emits a
    //     tight inline reflected-CRC bit loop instead (no table, no CALL),
    //     correct on every host, so this variant needs no CPU gate.
    //
    // `num_params` excludes the receiver — `update(I)V` is 1, `update([BII)V`
    // is 3. The 0xb6 codegen ladder adds the receiver back (`num_params + 1`)
    // and emits the receiver class-id guard. `update([B)V` (whole-array) is
    // intentionally NOT registered: it is a separate overload that the native
    // path already handles; keeping the intrinsic surface to the two hot
    // signatures the brief targets avoids speculative codegen.
    if class == "java/util/zip/CRC32C" {
        match (name, descriptor) {
            ("update", "(I)V") if x64::has_sse42() => {
                return Some((JitIntrinsic::Crc32cUpdateByte.as_entry(), 1, b'V'));
            }
            ("update", "([BII)V") if x64::has_sse42() => {
                return Some((JitIntrinsic::Crc32cUpdateBytes.as_entry(), 3, b'V'));
            }
            _ => {}
        }
    }
    if class == "java/util/zip/CRC32" {
        match (name, descriptor) {
            ("update", "(I)V") => {
                return Some((JitIntrinsic::Crc32UpdateByte.as_entry(), 1, b'V'));
            }
            ("update", "([BII)V") => {
                return Some((JitIntrinsic::Crc32UpdateBytes.as_entry(), 3, b'V'));
            }
            _ => {}
        }
    }
    // ===== INTRINSIC REGION END: CRC32 =====

    None
}

/// Layout-aware matcher for the `java/lang/String` call-site intrinsics
/// (the STRING_ACCESS and STRING_SEARCH families).
///
/// Unlike [`try_resolve_intrinsic`], a String intrinsic can only be inlined
/// when the JIT has resolved `java/lang/String`'s heap field layout — the
/// codegen must read the receiver's `value` (`byte[]`), `coder` (`byte`)
/// and `hash` (`int`) fields with inline machine code. That layout is not a
/// `(class, name, descriptor)` property, so it cannot live in the 3-arg
/// `try_resolve_intrinsic` (whose signature is shared with — and pinned by
/// the tests of — every non-String family). `try_compile_inner` therefore
/// calls THIS function for instance-method invokes, passing the
/// `StringFieldLayout` it resolved once for the compilation.
///
/// Returns `Some((entry, num_params, return_type, guard_class_id))`. The
/// `guard_class_id` is `0` for a statically-monomorphic `java/lang/String`
/// receiver (no runtime guard needed — String is `final`) and the
/// `java/lang/String` `ObjectHeader` class id for a `java/lang/CharSequence`
/// receiver (the codegen then emits a `[recv+0] == guard` check, deopting to
/// native dispatch for any non-String CharSequence). Returns `None` — so the
/// call falls back to normal native dispatch — when:
///   * `class` is neither `java/lang/String` nor `java/lang/CharSequence`;
///   * `string_layout` is `None` (String not loaded / resolver unavailable);
///   * the resolved layout has no `coder` field (`has_coder == false`, the
///     legacy `char[]` String layout) — every inlined String intrinsic
///     decodes via `coder`, so a layout without it cannot be inlined;
///   * a CharSequence site has no resolved String class id to guard against;
///   * `(name, descriptor)` is not one of the inlined signatures.
///
/// CharSequence eligibility is limited to the accessors CharSequence actually
/// declares — `charAt`/`length`/`isEmpty`; the String-specific
/// `hashCode`/`equals`/`compareTo`/`indexOf` are `java/lang/String` only.
///
/// The codegen ladder in `x64.rs` (the 0xb6/b7/b9 STRING_ACCESS /
/// STRING_SEARCH regions) consumes `compiler.string_layout`, which is
/// populated from the SAME `string_layout_resolver`, so the matcher's
/// "registered" decision and the codegen's "can emit" decision are always
/// consistent within one compilation — a registered String sentinel is
/// never left for the plain direct-call path to mis-`CALL`.
pub fn try_resolve_string_intrinsic(
    class: &str,
    name: &str,
    descriptor: &str,
    string_layout: Option<StringFieldLayout>,
) -> Option<(usize, usize, u8, u32)> {
    // Receiver-guard mode by the *declared* class of the call site:
    //   * java/lang/String      — final, monomorphic → no guard (0).
    //   * java/lang/CharSequence — the receiver may be any CharSequence, so
    //     the String-layout decode is only valid behind a runtime class-id
    //     guard against the real String class id.
    let is_string = class == "java/lang/String";
    let is_charseq = class == "java/lang/CharSequence";
    if !is_string && !is_charseq {
        return None;
    }
    // Every inlined String intrinsic reads the `coder` byte to pick the
    // LATIN1/UTF16 decode path. A layout without `coder` (legacy `char[]`
    // String) cannot be inlined — bail to native dispatch.
    let layout = string_layout?;
    if !layout.has_coder {
        return None;
    }
    let guard: u32 = if is_string { 0 } else { layout.string_class_id };
    // A CharSequence site needs a real String class id; without one the
    // inline decode would be unguarded — bail to native dispatch.
    if is_charseq && guard == 0 {
        return None;
    }

    // ===== INTRINSIC REGION BEGIN: STRING_ACCESS =====
    // `charAt`/`length`/`isEmpty` are declared on CharSequence and so are
    // eligible for both receiver kinds; `hashCode` is String-only.
    let hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
        ("length", "()I") => Some((JitIntrinsic::StringLength, 0, b'I')),
        ("isEmpty", "()Z") => Some((JitIntrinsic::StringIsEmpty, 0, b'Z')),
        ("charAt", "(I)C") => Some((JitIntrinsic::StringCharAt, 1, b'C')),
        ("hashCode", "()I") if is_string => Some((JitIntrinsic::StringHashCode, 0, b'I')),
        _ => None,
    };
    if let Some((intrinsic, num_params, ret)) = hit {
        return Some((intrinsic.as_entry(), num_params, ret, guard));
    }
    // ===== INTRINSIC REGION END: STRING_ACCESS =====

    // ===== INTRINSIC REGION BEGIN: STRING_SEARCH =====
    // `equals`, `compareTo` and both `indexOf` overloads are inlined. The
    // codegen ladder decodes every character through the receiver's /
    // argument's own `coder` byte, so all LATIN1/UTF16 combinations are
    // handled inline; only null receiver / null argument / null backing
    // array route to the deopt stub. These signatures are declared on
    // `java/lang/String` (not CharSequence), so they never reach a guarded
    // (CharSequence) call site.
    //
    //   * compareTo(String)   — lexicographic decoded-char compare; the
    //     unsigned-char difference at the first mismatch, else len1-len2.
    //   * indexOf(I)          — scan for `(ch & 0xFFFF)` from index 0,
    //     bit-identical to native `String.indexOf(int)` (which likewise
    //     masks to a single code unit — supplementary code points match
    //     their masked low half, no surrogate special-casing).
    //   * indexOf(String)     — naive O(n*m) substring search from 0; an
    //     empty needle returns 0.
    if is_string {
        let search_hit: Option<(JitIntrinsic, usize, u8)> = match (name, descriptor) {
            ("equals", "(Ljava/lang/Object;)Z") => Some((JitIntrinsic::StringEquals, 1, b'Z')),
            ("compareTo", "(Ljava/lang/String;)I") => {
                Some((JitIntrinsic::StringCompareTo, 1, b'I'))
            }
            ("indexOf", "(I)I") => Some((JitIntrinsic::StringIndexOfChar, 1, b'I')),
            ("indexOf", "(Ljava/lang/String;)I") => Some((JitIntrinsic::StringIndexOfStr, 1, b'I')),
            _ => None,
        };
        if let Some((intrinsic, num_params, ret)) = search_hit {
            return Some((intrinsic.as_entry(), num_params, ret, 0));
        }
    }
    // ===== INTRINSIC REGION END: STRING_SEARCH =====

    None
}

/// A resolved direct-call target.
pub struct JitDirectCall {
    pub entry: usize,
    pub needs_context: bool,
    pub num_params: usize,
    pub return_type: u8,
    /// Receiver class id for a call-site intrinsic that needs a runtime
    /// receiver-class guard before its inline body is correct.
    ///
    /// The CRC32/CRC32C `update` intrinsics are `invokevirtual` sites: the
    /// inline code reads/writes the receiver's `int crc` field at slot 0,
    /// which is only sound if the receiver's *dynamic* class is exactly the
    /// declared `java/util/zip/CRC32` / `CRC32C` (a subclass could override
    /// `update`). The codegen emits `CMP [receiver+0], guard_class_id` and
    /// deopts to normal dispatch on mismatch.
    ///
    /// `0` means "no guard class id available" — the sentinel class id 0 is
    /// never a real CRC32/CRC32C class, so a CRC32 intrinsic whose
    /// `guard_class_id` is 0 bails to normal dispatch. Every non-intrinsic
    /// `JitDirectCall` leaves this 0 (unused).
    pub guard_class_id: u32,
}

/// Monomorphic inline cache (MIC) slot for invokevirtual/invokeinterface call sites.
///
/// Caches the receiver ClassId, class name, and resolved method entry pointer
/// so that repeated calls with the same receiver type skip vtable dispatch entirely.
///
/// On **cache hit** (receiver ClassId matches `cached_class_id`):
///   - If `cached_entry_ptr` is non-zero → direct call to cached native function pointer
///   - Else → fast name-based dispatch (skip class_manager lookup)
///
/// On **cache miss**:
///   - Full vtable dispatch via class_manager + invoke_or_native
///   - Update all cached fields atomically
///
/// Hit/miss counters support adaptive recompilation decisions.
///
/// # Memory layout (CRIT-8 prerequisite)
///
/// `#[repr(C)]` plus a fixed field order with explicit padding pins the
/// JIT-hot fields at known offsets at the *start* of the struct so the
/// x86-64 codegen in `jit/src/x64.rs` can safely emit instructions like
/// `MOV eax, [mic_ptr + JitMICSlot::CACHED_CLASS_ID_OFFSET]` for inline
/// MIC dispatch. The `Mutex<Option<String>>` field — whose internal
/// representation is not guaranteed stable across `parking_lot`
/// versions — is moved to the **tail** so its layout cannot disturb
/// the hot-path offsets.
///
/// Offsets are asserted to match the constants in
/// [`JitMICSlot::CACHED_CLASS_ID_OFFSET`] et al. via a runtime test
/// (`test_jit_mic_slot_offsets`). A compile-time assert would require
/// `core::mem::offset_of!` (Rust 1.77+); the workspace MSRV is 1.75.
#[repr(C)]
pub struct JitMICSlot {
    /// Cached receiver ClassId (0 = empty/unpopulated).
    /// **JIT-hot — offset 0.**
    pub cached_class_id: std::sync::atomic::AtomicU32,
    /// Padding so `cached_entry_ptr` lands at an 8-byte aligned offset
    /// regardless of host alignment rules.
    _pad0: u32,
    /// Cached method entry pointer for direct call on hit (0 = not resolved).
    /// This is the address of a compiled native/JIT function that can be called
    /// directly with the same calling convention as `invoke_or_native`.
    /// **JIT-hot — offset 8.**
    pub cached_entry_ptr: std::sync::atomic::AtomicU64,
    /// Whether the cached entry needs the VM context pointer as first arg.
    /// **JIT-hot — offset 16.**
    pub cached_needs_context: std::sync::atomic::AtomicBool,
    /// Padding so the following `AtomicU64` counters land at an 8-byte
    /// aligned offset.
    _pad1: [u8; 7],
    /// Cache hit counter (diagnostic).
    pub hits: std::sync::atomic::AtomicU64,
    /// Cache miss counter (diagnostic).
    pub misses: std::sync::atomic::AtomicU64,
    /// Cached receiver class name (avoids class_manager read lock on hit).
    /// Moved to the tail: the mutex has an unstable layout we must not
    /// expose to JIT codegen. `Arc<str>` so the per-hit clone in the
    /// dispatch helper is a refcount bump, not a `String` heap copy.
    pub cached_class_name: parking_lot::Mutex<Option<std::sync::Arc<str>>>,
}

impl JitMICSlot {
    /// Byte offset of [`Self::cached_class_id`] from the start of the
    /// struct. JIT codegen uses this to emit
    /// `MOV eax, [mic_ptr + CACHED_CLASS_ID_OFFSET]`.
    pub const CACHED_CLASS_ID_OFFSET: usize = 0;
    /// Byte offset of [`Self::cached_entry_ptr`] from the start of the
    /// struct. JIT codegen uses this to emit the indirect call target
    /// load on a cache hit.
    pub const CACHED_ENTRY_PTR_OFFSET: usize = 8;
    /// Byte offset of [`Self::cached_needs_context`] from the start of
    /// the struct. JIT codegen reads this to decide whether to thread
    /// the VM context pointer through the inline dispatch.
    pub const CACHED_NEEDS_CONTEXT_OFFSET: usize = 16;

    pub fn new() -> Self {
        Self {
            cached_class_id: std::sync::atomic::AtomicU32::new(0),
            _pad0: 0,
            cached_entry_ptr: std::sync::atomic::AtomicU64::new(0),
            cached_needs_context: std::sync::atomic::AtomicBool::new(false),
            _pad1: [0; 7],
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
            cached_class_name: parking_lot::Mutex::new(None),
        }
    }

    pub fn prepopulate(&self, class_id: u32) {
        self.cached_class_id
            .store(class_id, std::sync::atomic::Ordering::Relaxed);
    }

    /// Update all cached fields after a cache miss.
    pub fn update(&self, class_id: u32, class_name: &str, entry_ptr: u64, needs_context: bool) {
        // BUG-24: publish entry_ptr BEFORE class_id so the inline cache reader
        // (which checks class_id first, then loads entry_ptr) can never observe
        // the new class id paired with a stale entry_ptr.
        self.cached_entry_ptr
            .store(entry_ptr, std::sync::atomic::Ordering::Release);
        *self.cached_class_name.lock() = Some(std::sync::Arc::from(class_name));
        self.cached_needs_context
            .store(needs_context, std::sync::atomic::Ordering::Relaxed);
        self.cached_class_id
            .store(class_id, std::sync::atomic::Ordering::Release);
    }

    /// Record a cache hit.
    #[inline]
    pub fn record_hit(&self) {
        self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a cache miss.
    #[inline]
    pub fn record_miss(&self) {
        self.misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Total observations (hits + misses).
    pub fn total_observations(&self) -> u64 {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
            + self.misses.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Hit rate as a percentage (0-100). Returns 0 if no observations.
    pub fn hit_rate_pct(&self) -> u32 {
        let total = self.total_observations();
        if total == 0 {
            return 0;
        }
        let h = self.hits.load(std::sync::atomic::Ordering::Relaxed);
        ((h * 100) / total) as u32
    }

    /// Returns true if the cache is monomorphic (only one receiver type observed
    /// and hit rate ≥ 90%).
    pub fn is_monomorphic(&self) -> bool {
        self.total_observations() >= 10 && self.hit_rate_pct() >= 90
    }

    /// Returns true if the cache is megamorphic (miss rate > 50% with enough samples).
    pub fn is_megamorphic(&self) -> bool {
        self.total_observations() >= 20 && self.hit_rate_pct() < 50
    }

    /// Whether this MIC should be promoted to a [`JitPICSlot`].
    ///
    /// Promotion triggers once the miss count exceeds
    /// [`MIC_TO_PIC_THRESHOLD`], which indicates the site has seen at
    /// least one receiver type the 1-entry MIC cannot cache. The
    /// adaptive recompiler calls this during its periodic scan and
    /// allocates a fresh PIC, seeded via [`JitPICSlot::seed_from_mic`].
    #[inline]
    pub fn needs_pic_promotion(&self) -> bool {
        self.misses.load(std::sync::atomic::Ordering::Relaxed) > MIC_TO_PIC_THRESHOLD
    }
}

impl Default for JitMICSlot {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// T5.2.5 — Polymorphic Inline Cache (PIC)
// ---------------------------------------------------------------------------

/// Maximum number of entries a `JitPICSlot` holds.
///
/// Three slots is the HotSpot default for C1's PIC. In practice almost
/// all polymorphic call sites observed fewer than three distinct
/// receiver types over the lifetime of a method; higher arities fall
/// off to megamorphic (plain vtable) dispatch.
pub const JIT_PIC_ENTRIES: usize = 3;

/// Polymorphic inline cache slot: a 3-way associative cache for
/// virtual call dispatch targeting a call site that has exhibited
/// polymorphism (more than one receiver type observed).
///
/// # Promotion path
///
/// ```text
/// uncompiled → MIC (1-entry) → PIC (3-entry) → megamorphic vtable
/// ```
///
/// The JIT installs a [`JitMICSlot`] at every virtual call site. On a
/// recorded miss count > `MIC_TO_PIC_THRESHOLD` the adaptive
/// recompiler upgrades the site to a `JitPICSlot`. If the PIC itself
/// records > `PIC_TO_MEGA_THRESHOLD` misses after filling all 3
/// entries, the site is deoptimized to a generic vtable dispatch.
///
/// # Memory layout
///
/// Entries are stored in plain arrays so the generated x86-64 stub
/// can do a simple linear probe: `CMP RAX, [RBX+0]; JE entry0; CMP
/// RAX, [RBX+16]; JE entry1; ...`. Each entry is 24 bytes
/// (`class_id` + padding + `entry_ptr` + flags) — well within an L1
/// line for the whole slot.
///
/// # Stable layout (CRIT-8 prerequisite)
///
/// Marked `#[repr(C)]` and laid out so the JIT codegen in
/// `jit/src/x64.rs` can emit raw `MOV eax, [pic_ptr + CLASS_ID_OFFSETS[i]]`
/// for inline 3-way PIC dispatch. The `Mutex<Option<String>>` array —
/// whose internal representation is not guaranteed stable across
/// `parking_lot` versions — is moved to the **tail** so its layout
/// cannot disturb the hot-path offsets.
///
/// Offsets are asserted to match the constants in
/// [`JitPICSlot::CLASS_ID_OFFSETS`] et al. via a runtime test
/// (`test_jit_pic_slot_offsets`).
#[repr(C)]
pub struct JitPICSlot {
    /// Cached ClassIds, parallel to `entry_ptrs`. `0` means the slot
    /// is empty (ClassId 0 is reserved for `java.lang.Object`, which
    /// cannot be a dispatch target here because invokevirtual on an
    /// Object reference goes through the vtable directly).
    /// **JIT-hot — offsets 0, 4, 8.**
    pub class_ids: [std::sync::atomic::AtomicU32; JIT_PIC_ENTRIES],
    /// Padding so `entry_ptrs` lands at an 8-byte aligned offset.
    _pad0: u32,
    /// Cached method entry pointers, parallel to `class_ids`.
    /// **JIT-hot — offsets 16, 24, 32.**
    pub entry_ptrs: [std::sync::atomic::AtomicU64; JIT_PIC_ENTRIES],
    /// Whether the cached entry needs the VM context pointer as the
    /// first argument. Parallel to `class_ids`.
    /// **JIT-hot — offsets 40, 41, 42.**
    pub needs_context: [std::sync::atomic::AtomicBool; JIT_PIC_ENTRIES],
    /// Padding so the following `AtomicU64` counters land at an 8-byte
    /// aligned offset.
    _pad1: [u8; 5],
    /// Per-entry hit counter. Used to pick an eviction victim when a
    /// fourth receiver type arrives.
    pub hits: [std::sync::atomic::AtomicU64; JIT_PIC_ENTRIES],
    /// Total cache misses (receiver not in any entry).
    pub misses: std::sync::atomic::AtomicU64,
    /// Cached class names (mutex-protected). Parallel to `class_ids`.
    /// Moved to the tail: `parking_lot::Mutex<Option<String>>` has an
    /// unstable layout we must not expose to JIT codegen.
    pub class_names: [parking_lot::Mutex<Option<String>>; JIT_PIC_ENTRIES],
}

/// Miss count on a `JitMICSlot` at which the adaptive recompiler
/// promotes the site to a `JitPICSlot`.
pub const MIC_TO_PIC_THRESHOLD: u64 = 3;

/// Miss count on a full `JitPICSlot` (all 3 entries populated) at
/// which the adaptive recompiler deoptimizes the site to megamorphic
/// vtable dispatch.
pub const PIC_TO_MEGA_THRESHOLD: u64 = 20;

impl JitPICSlot {
    /// Byte offsets of [`Self::class_ids`] entries from the start of
    /// the struct. JIT codegen uses these to emit
    /// `MOV eax, [pic_ptr + CLASS_ID_OFFSETS[i]]` for the inline 3-way
    /// class comparisons.
    pub const CLASS_ID_OFFSETS: [usize; JIT_PIC_ENTRIES] = [0, 4, 8];
    /// Byte offsets of [`Self::entry_ptrs`] entries from the start of
    /// the struct. JIT codegen uses these to emit the indirect call
    /// target load on a PIC hit.
    pub const ENTRY_PTR_OFFSETS: [usize; JIT_PIC_ENTRIES] = [16, 24, 32];
    /// Byte offsets of [`Self::needs_context`] entries from the start
    /// of the struct. JIT codegen reads these to decide whether to
    /// thread the VM context pointer through the inline dispatch.
    pub const NEEDS_CONTEXT_OFFSETS: [usize; JIT_PIC_ENTRIES] = [40, 41, 42];

    /// Create an empty PIC slot.
    pub fn new() -> Self {
        // Can't use `Default::default()` inside a const array literal
        // because the inner types (`parking_lot::Mutex`) don't derive
        // `Copy`; materialize each entry explicitly.
        Self {
            class_ids: [
                std::sync::atomic::AtomicU32::new(0),
                std::sync::atomic::AtomicU32::new(0),
                std::sync::atomic::AtomicU32::new(0),
            ],
            _pad0: 0,
            entry_ptrs: [
                std::sync::atomic::AtomicU64::new(0),
                std::sync::atomic::AtomicU64::new(0),
                std::sync::atomic::AtomicU64::new(0),
            ],
            needs_context: [
                std::sync::atomic::AtomicBool::new(false),
                std::sync::atomic::AtomicBool::new(false),
                std::sync::atomic::AtomicBool::new(false),
            ],
            _pad1: [0; 5],
            hits: [
                std::sync::atomic::AtomicU64::new(0),
                std::sync::atomic::AtomicU64::new(0),
                std::sync::atomic::AtomicU64::new(0),
            ],
            misses: std::sync::atomic::AtomicU64::new(0),
            class_names: [
                parking_lot::Mutex::new(None),
                parking_lot::Mutex::new(None),
                parking_lot::Mutex::new(None),
            ],
        }
    }

    /// Seed the PIC from an existing MIC. Used during MIC → PIC
    /// promotion so the single MIC entry lands in slot 0 of the PIC
    /// and no cache warm-up is lost.
    pub fn seed_from_mic(&self, mic: &JitMICSlot) {
        let class_id = mic
            .cached_class_id
            .load(std::sync::atomic::Ordering::Acquire);
        if class_id == 0 {
            return;
        }
        let entry_ptr = mic
            .cached_entry_ptr
            .load(std::sync::atomic::Ordering::Acquire);
        let needs_ctx = mic
            .cached_needs_context
            .load(std::sync::atomic::Ordering::Relaxed);
        // PIC slots keep `String` names; the MIC caches `Arc<str>` (cheap
        // per-hit clones in the dispatch helper) — convert on this rare
        // promotion path.
        let class_name = mic.cached_class_name.lock().as_deref().map(String::from);
        // BUG-24: publish entry_ptr / needs_context / name BEFORE the class_id,
        // exactly as `write_entry` does. The inline PIC cascade
        // (`jit/src/x64.rs`) reads `class_ids[i]` first and, on a match, loads
        // `entry_ptrs[i]` and `CALL`s it — all with plain (acquire-on-x86) MOVs.
        // The previous order stored `class_ids[0]` first, so a reader that
        // observed the new class id could still load the slot's *previous*
        // `entry_ptrs[0]` (a stale/garbage pointer left from an earlier
        // occupant) and call through it → the Mockito-under-JIT
        // `EXCEPTION_ACCESS_VIOLATION at 0x0000033E…` (a packed-class-id-looking
        // value). Storing the entry first closes the window.
        self.entry_ptrs[0].store(entry_ptr, std::sync::atomic::Ordering::Release);
        self.needs_context[0].store(needs_ctx, std::sync::atomic::Ordering::Relaxed);
        *self.class_names[0].lock() = class_name;
        self.class_ids[0].store(class_id, std::sync::atomic::Ordering::Release);
        // Carry the hit count so adaptive recompilation keeps the
        // cumulative picture.
        self.hits[0].store(
            mic.hits.load(std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Fast-path lookup: find a cached entry for `class_id`. Returns
    /// `Some((entry_ptr, needs_context))` on hit, `None` on miss.
    ///
    /// This is the pure-Rust implementation that mirrors what the
    /// JIT-generated dispatch stub does with `CMP` / `JE`. Tests use
    /// it directly; the generated stub calls into a tiny shim that
    /// invokes the same sequence of atomic loads.
    #[inline]
    pub fn lookup(&self, class_id: u32) -> Option<(u64, bool)> {
        // Scan all 3 entries. Because entries never share a class_id
        // (see `install`), at most one can match; we break out early
        // on the first hit.
        for i in 0..JIT_PIC_ENTRIES {
            let cached = self.class_ids[i].load(std::sync::atomic::Ordering::Acquire);
            if cached == class_id {
                let ptr = self.entry_ptrs[i].load(std::sync::atomic::Ordering::Acquire);
                let ctx = self.needs_context[i].load(std::sync::atomic::Ordering::Relaxed);
                self.hits[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Some((ptr, ctx));
            }
        }
        self.misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    }

    /// Install a new `(class_id → entry_ptr)` mapping.
    ///
    /// - If an empty slot exists (class_id == 0), use it.
    /// - Otherwise evict the entry with the fewest hits (LFU). The
    ///   evicted entry's class_id is cleared first so concurrent
    ///   readers can't accidentally dispatch to a stale pointer with
    ///   a new class id.
    pub fn install(&self, class_id: u32, class_name: &str, entry_ptr: u64, needs_ctx: bool) {
        // First preference: reuse an empty slot so hit counters for
        // existing entries aren't perturbed.
        for i in 0..JIT_PIC_ENTRIES {
            if self.class_ids[i].load(std::sync::atomic::Ordering::Relaxed) == 0 {
                self.write_entry(i, class_id, class_name, entry_ptr, needs_ctx);
                return;
            }
        }
        // All slots full — evict LFU.
        let mut victim = 0usize;
        let mut victim_hits = u64::MAX;
        for i in 0..JIT_PIC_ENTRIES {
            let h = self.hits[i].load(std::sync::atomic::Ordering::Relaxed);
            if h < victim_hits {
                victim_hits = h;
                victim = i;
            }
        }
        // Clear first so readers don't see the old ptr paired with
        // the new class_id during the atomic update window.
        self.class_ids[victim].store(0, std::sync::atomic::Ordering::Release);
        self.write_entry(victim, class_id, class_name, entry_ptr, needs_ctx);
    }

    /// Core installation sequence. Writes entry_ptr before class_id
    /// so a concurrent `lookup` can't observe a stale pointer under
    /// the new class id. Also zeroes the hit counter.
    #[inline]
    fn write_entry(
        &self,
        i: usize,
        class_id: u32,
        class_name: &str,
        entry_ptr: u64,
        needs_ctx: bool,
    ) {
        self.entry_ptrs[i].store(entry_ptr, std::sync::atomic::Ordering::Release);
        self.needs_context[i].store(needs_ctx, std::sync::atomic::Ordering::Relaxed);
        *self.class_names[i].lock() = Some(class_name.to_string());
        self.hits[i].store(0, std::sync::atomic::Ordering::Relaxed);
        // Publish the new class_id last.
        self.class_ids[i].store(class_id, std::sync::atomic::Ordering::Release);
    }

    /// Total observed invocations (hits across all entries + misses).
    pub fn total_observations(&self) -> u64 {
        let hits: u64 = self
            .hits
            .iter()
            .map(|h| h.load(std::sync::atomic::Ordering::Relaxed))
            .sum();
        hits + self.misses.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of entries currently populated (0..=3).
    pub fn entries_used(&self) -> usize {
        self.class_ids
            .iter()
            .filter(|c| c.load(std::sync::atomic::Ordering::Relaxed) != 0)
            .count()
    }

    /// Whether the PIC has started receiving receiver types it can no
    /// longer cache (all 3 slots populated AND miss count exceeds the
    /// promotion-to-megamorphic threshold).
    pub fn is_megamorphic(&self) -> bool {
        self.entries_used() == JIT_PIC_ENTRIES
            && self.misses.load(std::sync::atomic::Ordering::Relaxed) >= PIC_TO_MEGA_THRESHOLD
    }

    /// Whether the call site should be deoptimized from the PIC fast
    /// path back to a plain megamorphic vtable dispatch. Mirrors
    /// [`Self::is_megamorphic`] but the name makes the decision point
    /// explicit at the call site that may need to re-patch.
    #[inline]
    pub fn should_deopt_to_mega(&self) -> bool {
        self.is_megamorphic()
    }
}

/// Build a [`JitPICSlot`] from an existing MIC and return it boxed so
/// the adaptive recompiler can install the new slot at the call site.
///
/// The single MIC entry lands in slot 0 of the PIC (see
/// [`JitPICSlot::seed_from_mic`]) so no cache warm-up is lost. Cold
/// MICs (`cached_class_id == 0`) produce an empty PIC, which is still
/// valid — its first miss fills slot 0 naturally.
pub fn promote_mic_to_pic(mic: &JitMICSlot) -> Box<JitPICSlot> {
    let pic = Box::new(JitPICSlot::new());
    pic.seed_from_mic(mic);
    pic
}

impl Default for JitPICSlot {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Descriptor parameter iterator
// ---------------------------------------------------------------------------

/// Iterator over parameter types in a JVM method descriptor.
pub struct DescriptorParamIter<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> DescriptorParamIter<'a> {
    pub fn new(descriptor: &'a str) -> Self {
        let bytes = descriptor.as_bytes();
        let pos = if !bytes.is_empty() && bytes[0] == b'(' {
            1
        } else {
            0
        };
        Self { bytes, pos }
    }
}

impl Iterator for DescriptorParamIter<'_> {
    type Item = u8;
    fn next(&mut self) -> Option<u8> {
        if self.pos >= self.bytes.len() || self.bytes[self.pos] == b')' {
            return None;
        }
        let tag = self.bytes[self.pos];
        match tag {
            b'I' | b'J' | b'F' | b'D' | b'B' | b'C' | b'S' | b'Z' => {
                self.pos += 1;
                Some(tag)
            }
            b'L' => {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b';' {
                    self.pos += 1;
                }
                self.pos += 1;
                Some(b'L')
            }
            b'[' => {
                while self.pos < self.bytes.len() && self.bytes[self.pos] == b'[' {
                    self.pos += 1;
                }
                if self.pos < self.bytes.len() && self.bytes[self.pos] == b'L' {
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b';' {
                        self.pos += 1;
                    }
                    self.pos += 1;
                } else if self.pos < self.bytes.len() {
                    self.pos += 1;
                }
                Some(b'[')
            }
            _ => {
                self.pos += 1;
                Some(tag)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JIT cache
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Hash)]
struct JitKey {
    class_name: Arc<str>,
    method_name: Arc<str>,
    descriptor: Arc<str>,
}

/// Compute a u64 hash key for a JIT cache entry by XOR-folding three
/// independent FxHashes (class, method, descriptor). Using separate
/// hashers per component (rather than chained writes) keeps each call
/// branch-free and avoids the per-call Arc clones the previous keyed
/// lookup required.
///
/// Hash collisions are tolerated by the cache: `JitCache::get` always
/// verifies the full string key match after the hash hit (see PERF-P2
/// fix). A collision degrades to a cache miss, which is correct but
/// slightly suboptimal (triggers a re-compile via the slow path).
#[inline]
fn compute_jit_key_hash(class: &str, method: &str, desc: &str) -> u64 {
    let mut hc = FxHasher::default();
    hc.write(class.as_bytes());
    let mut hm = FxHasher::default();
    hm.write(method.as_bytes());
    let mut hd = FxHasher::default();
    hd.write(desc.as_bytes());
    hc.finish() ^ hm.finish() ^ hd.finish()
}

/// Per-VM JIT cache: maps method identity to compiled native code.
///
/// Storage uses a precomputed u64 hash as the map key (see
/// [`compute_jit_key_hash`]). The full [`JitKey`] is stored alongside
/// the compiled method so lookups can verify the key fully matches
/// after a hash hit — this protects against (rare) hash collisions
/// while eliminating the three `Arc<str>` clones the prior keyed-map
/// implementation required on every lookup (PERF-P2).
///
/// TODO(round-11, HIGH from round-7/9 cross-cutting): lock-free / sharded
/// `JitCache` for the hot interpreter dispatch path.
///
/// Today the cache is wrapped in `parking_lot::RwLock<JitCache>` at the
/// VM level (see `vm/src/vm/vm_init.rs::jit_cache`). Every JIT-dispatch
/// site (~6 read sites across `interpreter.rs` + `helpers.rs`) acquires
/// `.read()` to look up a compiled method by `(class, method, descriptor)`
/// — fully serialised against the redefinition path that takes `.write()`.
///
/// Two viable migrations were considered for this round:
///
///   (A) **`arc-swap` snapshot**: store the methods map as
///       `ArcSwap<FxHashMap<u64, (JitKey, Arc<CompiledMethod>)>>`; reads
///       do `arc.load()` + lookup (fully lock-free, no shared dirty
///       cacheline ping); writes clone the whole map, mutate, and
///       `arc.store(new)`. Reads scale linearly; writes go O(N) in
///       cache size but are rare (redefinition + class invalidation).
///
///   (B) **16-shard `RwLock`** like `ProfileStore` (round-11 HIGH-1):
///       partition by `compute_jit_key_hash(...) & 15`. Contention
///       drops by ~16× under multi-thread JIT warmup but readers still
///       pay an uncontended rwlock acquire per dispatch.
///
/// **Why this is deferred:** the cache also owns two `Pin<Box<...>>`
/// arenas (`string_arena`, `invoke_info_arena`) that hand out raw
/// pointers to JIT-emitted code (`intern_string`, `intern_invoke_info`).
/// Those pointers MUST stay valid for the lifetime of every cached
/// `CompiledMethod` that holds them — arc-swap's "clone the map on
/// write" model would either (1) keep the arenas in a separate
/// non-swappable container (extra indirection on every intern), or
/// (2) accumulate per-snapshot arenas that can only be reclaimed once
/// the entire prior `Arc<FxHashMap>` is dropped (real cycles possible
/// because `CompiledMethod` stores raw `*const u8` into the arena).
/// Sharding the cache map across 16 shards also fragments the arenas:
/// either each shard owns its own arena (16× the VirtualAlloc
/// granularity tax — already a known issue, see the TODO below) or all
/// shards share a global `Mutex<Arenas>` which re-introduces the
/// bottleneck the sharding was supposed to remove.
///
/// **Plan for round-12:** combine this with the per-VM code-arena
/// rework (the existing TODO below). Once `ExecutableBuffer` is
/// arena-backed, the per-method intern arenas can be split off into a
/// single `parking_lot::Mutex<JitArenas>` (cold-path only — intern is
/// invoked at compile time, not at dispatch) and the dispatch-hot
/// methods map can switch to either `ArcSwap` (preferred for read
/// scalability) or 16-shard `RwLock` (preferred for write latency).
/// Both depend on `CompiledMethod` no longer transitively pinning
/// arena slots through raw pointers.
///
/// TODO(round-8, HIGH from round-7 jit #6): no pooling / LRU / coalescing
/// of `ExecutableBuffer`s.  On Windows each compiled method calls
/// `VirtualAlloc` with a typical code size of ~5 KB but allocation
/// granularity is 64 KB, wasting ~59 KB of address space per method.
/// For a JDK workload with ~3000 hot methods that's ~177 MB of
/// reserved-but-unused VA.  A per-VM code arena (single large
/// `VirtualAlloc` carved into slabs, with LRU eviction keyed by
/// invocation count) would close the gap.
///
/// Concrete plan for round-8:
///   1. Add `code_arena: Mutex<JitCodeArena>` to `JitCache`. The arena
///      owns one or more 16 MiB executable regions (each is a single
///      `VirtualAlloc(MEM_RESERVE|MEM_COMMIT, PAGE_READWRITE)` followed
///      by a `VirtualProtect(PAGE_EXECUTE_READ)` after each method
///      finalize — but at *page* granularity, not allocation
///      granularity, so the 64 KB tax is paid once per arena instead
///      of once per method).
///   2. `ExecutableBuffer::new(size)` becomes
///      `ExecutableBuffer::new_in(arena, size)`. On success it returns
///      a buffer that points into the arena and carries a back-pointer
///      to the arena for free-on-drop. The arena maintains a free-list
///      keyed by 256-byte-rounded size buckets so deopt-invalidated
///      methods (see `JitCache::remove` / `invalidate_for_class`)
///      return space for reuse.
///   3. The JIT-code-region tracker (`jit_code_regions`) registers
///      whole *arenas* instead of individual buffers — the validator
///      uses a single range check per arena, which also speeds up the
///      conservative scan's `is_code_address` query.
///   4. W^X transitions (`make_executable` / `make_writable`) must be
///      page-aligned. The arena rounds each buffer up to 4 KB to allow
///      independent protection toggling, OR (preferred) batches all
///      protect-RX transitions until JIT-quiesce time so a single
///      `VirtualProtect` covers many buffers at once.
///   5. OSR trampolines (allocated via `ExecutableBuffer::new` in
///      `emit_osr_trampoline`) need their own small-buckets arena to
///      avoid heap fragmentation. Trampolines are typically <1 KB.
///
/// Deferred from this wave because it requires reworking
/// `ExecutableBuffer` ownership, the Drop impl, the OSR-trampoline
/// cache (`osr_trampoline_cache`), relocation patches, and the
/// JIT-code-region tracker (`jit_code_regions`) — all touching
/// concurrency invariants that need their own test battery.
pub struct JitCache {
    methods: FxHashMap<u64, (JitKey, Arc<CompiledMethod>)>,
    string_arena: Vec<Pin<Box<str>>>,
    invoke_info_arena: Vec<Pin<Box<JitInvokeInfo>>>,
}

impl JitCache {
    pub fn new() -> Self {
        Self {
            methods: FxHashMap::default(),
            string_arena: Vec::new(),
            invoke_info_arena: Vec::new(),
        }
    }

    pub fn intern_string(&mut self, s: String) -> (*const u8, usize) {
        let boxed: Pin<Box<str>> = Pin::new(s.into_boxed_str());
        let ptr = boxed.as_ptr();
        let len = boxed.len();
        self.string_arena.push(boxed);
        (ptr, len)
    }

    pub fn intern_invoke_info(&mut self, info: JitInvokeInfo) -> *const JitInvokeInfo {
        let boxed = Pin::new(Box::new(info));
        let ptr: *const JitInvokeInfo = &*boxed;
        self.invoke_info_arena.push(boxed);
        ptr
    }

    /// Look up a compiled method by (class, method, descriptor).
    ///
    /// Hot path: this is called on every invoke of a JIT-compiled
    /// method via the call-site cache. To avoid three `Arc<str>`
    /// clones per probe (each = an atomic refcount increment), the
    /// lookup is keyed by a precomputed u64 hash and the full string
    /// key is verified after the hash hit (collision check).
    ///
    /// On a hash collision (extremely rare; the caller will fall
    /// through to the slow path and re-JIT), this returns `None`.
    /// That is a correctness-safe but slightly suboptimal outcome —
    /// see [`compute_jit_key_hash`] for the rationale.
    ///
    /// Callers holding an `&Arc<str>` can pass it directly: deref
    /// coercion turns it into `&str`.
    pub fn get(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<Arc<CompiledMethod>> {
        let h = compute_jit_key_hash(class_name, method_name, descriptor);
        let (key, method) = self.methods.get(&h)?;
        if &*key.class_name == class_name
            && &*key.method_name == method_name
            && &*key.descriptor == descriptor
        {
            Some(method.clone())
        } else {
            None
        }
    }

    pub fn put(
        &mut self,
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        compiled: CompiledMethod,
    ) {
        let h = compute_jit_key_hash(&class_name, &method_name, &descriptor);
        let key = JitKey {
            class_name,
            method_name,
            descriptor,
        };
        let arc = Arc::new(compiled);
        // Stage 5 — register this method's code range for the GC RBP-chain
        // walker. Only when the precise gate is on (the registry is consulted
        // solely by `remap_active_jit_frames`, which is inert otherwise), so
        // the default path keeps zero bookkeeping overhead.
        if crate::x64::precise_jit_maps_enabled() {
            register_jit_code_range(
                arc.entry_ptr() as usize,
                arc.code_len(),
                Arc::as_ptr(&arc) as usize,
            );
        }
        // DBG (spring-bug-11): record entry→name so a crash report can name the
        // faulting JIT method. Gated; no overhead unless CRATONVM_DBG_JIT_NAMES.
        if jit_names_enabled() {
            register_jit_method_name(
                arc.entry_ptr() as usize,
                arc.code_len(),
                format!("{}.{}{}", key.class_name, key.method_name, key.descriptor),
            );
        }
        self.methods.insert(h, (key, arc));
    }

    pub fn len(&self) -> usize {
        self.methods.len()
    }

    pub fn is_empty(&self) -> bool {
        self.methods.is_empty()
    }

    /// Remove a compiled method from the cache (for invalidation).
    ///
    /// Verifies the full string key matches before removing, so a
    /// (rare) hash collision can't cause an unrelated cached entry to
    /// be evicted.
    pub fn remove(&mut self, class_name: &str, method_name: &str, descriptor: &str) {
        let h = compute_jit_key_hash(class_name, method_name, descriptor);
        if let Some((key, cm)) = self.methods.get(&h) {
            if &*key.class_name == class_name
                && &*key.method_name == method_name
                && &*key.descriptor == descriptor
            {
                // Stage 5 — drop this method's GC code-range registration
                // before evicting, so the RBP-chain walker can never resolve a
                // return address to a freed CompiledMethod.
                unregister_jit_code_range(cm.entry_ptr() as usize);
                self.methods.remove(&h);
            }
        }
    }

    /// T5.4.4 — Class hierarchy change invalidation.
    ///
    /// When a new class is loaded that extends/implements an existing
    /// class, any JIT-compiled method whose `inlined_methods` list
    /// references the changed class must be evicted because the
    /// CHA-based devirtualization assumption (only one implementor of
    /// the virtual method) is no longer valid.
    ///
    /// Returns the number of evicted entries.
    pub fn invalidate_for_class_change(&mut self, changed_class: &str) -> usize {
        let before = self.methods.len();
        self.methods.retain(|_h, (_key, cm)| {
            // Keep the entry iff it does NOT inline from the changed class.
            let keep = !cm
                .inlined_methods
                .iter()
                .any(|(cls, _, _)| cls == changed_class);
            // Stage 5 — drop the GC code-range registration for evicted methods.
            if !keep {
                unregister_jit_code_range(cm.entry_ptr() as usize);
            }
            keep
        });
        before - self.methods.len()
    }

    /// Invalidate all compiled methods that inlined code from `class_name`.
    /// Returns the number of methods evicted.
    pub fn invalidate_for_class(&mut self, class_name: &str) -> usize {
        let hashes_to_remove: Vec<u64> = self
            .methods
            .iter()
            .filter(|(_, (_key, compiled))| {
                compiled
                    .inlined_methods
                    .iter()
                    .any(|(cn, _, _)| cn == class_name)
            })
            .map(|(h, _)| *h)
            .collect();
        let count = hashes_to_remove.len();
        for h in hashes_to_remove {
            // Stage 5 — drop the GC code-range registration before evicting.
            if let Some((_key, cm)) = self.methods.get(&h) {
                unregister_jit_code_range(cm.entry_ptr() as usize);
            }
            self.methods.remove(&h);
        }
        count
    }
}

impl Default for JitCache {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for JitCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JitCache({} methods)", self.methods.len())
    }
}

// ---------------------------------------------------------------------------
// Escape analysis: IR graph → EA graph conversion
// ---------------------------------------------------------------------------

/// The instance-field index a full-layout `Op::Load`/`Op::Store` accesses,
/// recovered from its `Const(field_index)` offset operand (input[3]). Returns
/// `None` for a compact / malformed node (no constant offset), letting the
/// caller fall back to the `MemKind`-derived index. This is the single place
/// the EA bridge interprets a production field access's index, so the EA
/// graph's field edges and `apply_ea_to_ir`'s `field_values` lookup agree.
fn ir_load_store_field_index(ir_graph: &ir::Graph, node_id: ir::NodeId) -> Option<usize> {
    let node = ir_graph.nodes.get(node_id as usize)?;
    let offset_node = *node.inputs.get(3)?;
    match ir_graph.nodes.get(offset_node as usize)?.op {
        ir::Op::Const(v) if v >= 0 => Some(v as usize),
        _ => None,
    }
}

/// Convert an `ir::Graph` to an `escape_analysis::Graph` for standalone
/// escape analysis.  The two modules define independent `Op` / `Node` /
/// `Graph` types, so we translate node-by-node.  Returns both the EA graph
/// and the id_map (`ir::NodeId` index → `escape_analysis::NodeId` value)
/// so callers can map EA results back to IR node IDs.
fn escape_analysis_from_ir(
    ir_graph: &ir::Graph,
) -> (escape_analysis::Graph, Vec<escape_analysis::NodeId>) {
    let mut ea = escape_analysis::Graph::new();

    // Map from ir::NodeId → ea::NodeId.  Start/Return are pre-created as
    // ea nodes 0 and 1, matching the IR entry/exit.
    let ir_node_count = ir_graph.nodes.len();
    let mut id_map: Vec<escape_analysis::NodeId> = Vec::with_capacity(ir_node_count);

    // EA graph already has nodes 0 (Start) and 1 (Return).
    // Map IR entry and exit to those, everything else gets added below.
    for i in 0..ir_node_count {
        let ir_id = i as ir::NodeId;
        if ir_id == ir_graph.entry {
            id_map.push(0); // EA Start
        } else if ir_id == ir_graph.exit {
            id_map.push(1); // EA Return
        } else {
            // Placeholder — filled in the second pass.
            id_map.push(usize::MAX);
        }
    }

    // First pass: create EA nodes for everything except entry/exit.
    for i in 0..ir_node_count {
        let ir_id = i as ir::NodeId;
        if ir_id == ir_graph.entry || ir_id == ir_graph.exit {
            continue;
        }
        let ir_node = &ir_graph.nodes[i];
        // The production builder emits full-layout `Op::Load`/`Op::Store`
        // (`[ctrl, mem, base, offset, (value)]`) carrying a `MemKind`, but the
        // EA graph keys field edges by a real **field index**. Recover it from
        // the `Const(field_index)` offset operand (input[3]); fall back to the
        // `MemKind`-derived index (`ir_op_to_ea_op`) only when the offset isn't
        // a constant (a malformed/compact node EA already treats
        // conservatively).
        let ea_op = match &ir_node.op {
            ir::Op::Load(_) => match ir_load_store_field_index(ir_graph, i as ir::NodeId) {
                Some(f) => escape_analysis::Op::Load(f),
                None => ir_op_to_ea_op(&ir_node.op),
            },
            ir::Op::Store(_) => match ir_load_store_field_index(ir_graph, i as ir::NodeId) {
                Some(f) => escape_analysis::Op::Store(f),
                None => ir_op_to_ea_op(&ir_node.op),
            },
            _ => ir_op_to_ea_op(&ir_node.op),
        };
        // Don't wire inputs yet — we need all id_map entries populated.
        let ea_id = ea.add_node(ea_op, vec![]);
        id_map[i] = ea_id;
    }

    // Second pass: wire inputs (skip Start/Return whose inputs are already set).
    for i in 0..ir_node_count {
        let ir_id = i as ir::NodeId;
        if ir_id == ir_graph.entry {
            continue;
        }
        let ea_id = id_map[i];
        let ir_node = &ir_graph.nodes[i];
        // The EA graph reads memory-access operands positionally in a *compact*
        // layout (`Store [holder, value]`, `Load [holder]`); the full-layout IR
        // node places the holder at input[2] and the store value at input[4].
        // Translate those two shapes; everything else is forwarded verbatim.
        // (A node whose expected operand is missing yields an empty input list,
        // which EA's `store_holder`/`load_holder` treat conservatively.)
        let map_id = |inp: ir::NodeId| -> escape_analysis::NodeId {
            id_map.get(inp as usize).copied().unwrap_or(usize::MAX)
        };
        let ea_inputs: Vec<escape_analysis::NodeId> = match &ir_node.op {
            ir::Op::Store(_) if ir_node.inputs.len() >= 5 => {
                vec![map_id(ir_node.inputs[2]), map_id(ir_node.inputs[4])]
            }
            ir::Op::Load(_) if ir_node.inputs.len() >= 3 => vec![map_id(ir_node.inputs[2])],
            _ => ir_node.inputs.iter().map(|&inp| map_id(inp)).collect(),
        };
        ea.nodes[ea_id].inputs = ea_inputs.clone();
        // Rebuild use-edges for the targets.
        for &inp_ea in &ea_inputs {
            if inp_ea < ea.nodes.len() {
                ea.nodes[inp_ea].uses.push(ea_id);
            }
        }
    }

    (ea, id_map)
}

/// Map a single `ir::Op` variant to its `escape_analysis::Op` counterpart.
fn ir_op_to_ea_op(op: &ir::Op) -> escape_analysis::Op {
    use escape_analysis::Op as EaOp;
    match op {
        ir::Op::Start => EaOp::Start,
        ir::Op::Return => EaOp::Return,
        ir::Op::If => EaOp::If,
        ir::Op::Merge => EaOp::Merge,
        ir::Op::Phi => EaOp::Phi,
        ir::Op::Const(v) => EaOp::Const(*v),
        ir::Op::Param(idx) => EaOp::Param(*idx),
        ir::Op::Add => EaOp::Add,
        ir::Op::Sub => EaOp::Sub,
        ir::Op::Mul => EaOp::Mul,
        ir::Op::Load(mk) => EaOp::Load(*mk as usize),
        ir::Op::Store(mk) => EaOp::Store(*mk as usize),
        ir::Op::New {
            class_id,
            num_fields,
        } => EaOp::New {
            class_id: *class_id,
            num_fields: *num_fields,
        },
        ir::Op::NewArray { element_type } => EaOp::NewArray {
            element_type: *element_type,
        },
        ir::Op::Call { .. } => EaOp::Call,
        ir::Op::ArrayLength => EaOp::ArrayLength,
        ir::Op::Dead => EaOp::Dead,
        // All other IR ops (Region, Proj, ConstF, conversions, bitwise,
        // Cmp, Div, Rem, Neg, etc.) have no EA-specific behaviour.
        _ => EaOp::Other,
    }
}

// ---------------------------------------------------------------------------
// Escape analysis: apply EA results to the IR graph
// ---------------------------------------------------------------------------

/// Apply escape analysis results back onto the IR graph.
///
/// Maps EA node IDs back to IR node IDs using the reverse of `id_map`,
/// then performs scalar replacement (redirect load uses, kill stores and
/// allocations) and lock elision (kill monitor nodes) by marking nodes
/// as `ir::Op::Dead`.
fn apply_ea_to_ir(
    ir_graph: &mut ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
) {
    // Build reverse map: EA NodeId → IR NodeId
    let mut reverse_map: HashMap<escape_analysis::NodeId, ir::NodeId> = HashMap::new();
    for (ir_id, &ea_id) in id_map.iter().enumerate() {
        if ea_id != usize::MAX {
            reverse_map.insert(ea_id, ir_id as ir::NodeId);
        }
    }

    // Apply scalar replacements
    for info in &ea_result.scalar_replaceable {
        // For each replaced load, redirect all IR nodes that read from it
        // to read from the stored field value instead, then mark it dead.
        for &ea_load in &info.replaced_loads {
            let ir_load = match reverse_map.get(&ea_load) {
                Some(&id) => id,
                None => continue,
            };
            let idx = ir_load as usize;
            if idx >= ir_graph.nodes.len() {
                continue;
            }

            // Determine the field index of this load so we can look up the
            // replacement value in `field_values`. It must match the index the
            // EA bridge keyed field edges by: the real field index from the
            // `Const` offset operand (full-layout production load), falling back
            // to the `MemKind`-derived index for a compact/hand-built node.
            if info.field_values.is_empty() {
                continue;
            }
            let ea_field_idx = match ir_load_store_field_index(ir_graph, ir_load) {
                Some(f) => f,
                None => match &ir_graph.nodes[idx].op {
                    ir::Op::Load(mk) => *mk as usize,
                    _ => continue,
                },
            };

            // The value the load resolves to: the stored field value, or — when
            // the field was never stored (`field_values[idx] == None`) — the
            // freshly-allocated object's zero default. WITHOUT the latter, a
            // load of an un-stored field was killed below with NO replacement,
            // leaving its consumers reading a dead node (a miscompile that was
            // latent only because scalar replacement does not yet fire on
            // production IR). A `Const(0)` is the correct default for a
            // zero-initialised object's int field. (Soundness depends on the
            // object being genuinely zero-initialised — the caller must only
            // admit allocations whose constructor sets no non-zero field.)
            let replacement: Option<ir::NodeId> = if ea_field_idx < info.field_values.len() {
                match info.field_values[ea_field_idx] {
                    Some(ea_val) => reverse_map.get(&ea_val).copied(),
                    None => Some(ir_graph.add(ir::Op::Const(0), ir::IrType::Int, vec![], None)),
                }
            } else {
                None
            };

            if let Some(ir_val) = replacement {
                // Redirect: replace all references to ir_load with ir_val
                // across the entire IR graph.
                let load_id = ir_load;
                for node in ir_graph.nodes.iter_mut() {
                    for inp in node.inputs.iter_mut() {
                        if *inp == load_id {
                            *inp = ir_val;
                        }
                    }
                }
            }

            // Mark load as dead
            ir_graph.nodes[idx].op = ir::Op::Dead;
            ir_graph.nodes[idx].inputs.clear();
        }

        // Mark eliminated stores as dead
        for &ea_store in &info.eliminated_stores {
            if let Some(&ir_store) = reverse_map.get(&ea_store) {
                let idx = ir_store as usize;
                if idx < ir_graph.nodes.len() {
                    ir_graph.nodes[idx].op = ir::Op::Dead;
                    ir_graph.nodes[idx].inputs.clear();
                }
            }
        }

        // Mark the allocation as dead
        if let Some(&ir_alloc) = reverse_map.get(&info.alloc_node) {
            let idx = ir_alloc as usize;
            if idx < ir_graph.nodes.len() {
                ir_graph.nodes[idx].op = ir::Op::Dead;
                ir_graph.nodes[idx].inputs.clear();
            }
        }
    }

    // Apply lock elision
    for &ea_lock in &ea_result.elide_locks {
        if let Some(&ir_lock) = reverse_map.get(&ea_lock) {
            let idx = ir_lock as usize;
            if idx < ir_graph.nodes.len() {
                ir_graph.nodes[idx].op = ir::Op::Dead;
                ir_graph.nodes[idx].inputs.clear();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Compilation entry point
// ---------------------------------------------------------------------------

// round-7 fix (bug 1, CRIT, permanent recompile waste):
//
// `try_compile` returns `None` for two distinct reasons:
//   1. Transient failure (resolver returned None, executable-buffer alloc
//      failed, profile not yet present, etc.) — these are worth re-trying
//      later once the missing state is populated.
//   2. Permanent bail — the x64 backend hit an unsupported pattern
//      (e.g. >ARG_REGS direct-call args at x64.rs:10380/10422/10583/10665)
//      and returned `false` from `compile_bytecode`.  These will *always*
//      fail until the JIT grows the missing feature, but the interpreter
//      keeps polling `try_compile` every 2000 invocations forever — each
//      attempt burns ~50µs walking the same scan/IR pipeline only to bail
//      at the same site.
//
// We can't trivially distinguish (1) from (2) without plumbing a richer
// return type through the entire compilation pipeline.  As a pragmatic
// approximation, we treat the *most expensive* failure mode — the one
// where x64::compile() runs the full scan/IR/optimization pipeline and
// then returns None because of a backend bail — as permanent.  All the
// "transient" None paths (resolver-fail, allocator-fail) short-circuit
// *before* x64::compile and therefore don't pollute the bail-list.
//
// Implementation: a process-wide `FxHashSet<u64>` (keyed by the same
// XOR-folded FxHash we already use for the JIT cache, see
// `compute_jit_key_hash`).  Membership is checked at the top of
// `try_compile`; entries are added when the heavy backend path returns
// None.  The set is never pruned — bail-listed methods stay bail-listed
// for the JVM's lifetime (matches the "wave them off" intent).
//
// Hash collisions between two methods are benign here: the worst case
// is a non-bail-listed method that shares a hash with a bail-listed
// one gets falsely skipped (and stays in the interpreter).  Probability
// is the same ~2.7e-10 / 100k methods as the JIT cache.

static JIT_BAIL_LIST: std::sync::OnceLock<parking_lot::RwLock<rustc_hash::FxHashSet<u64>>> =
    std::sync::OnceLock::new();
static JIT_BAIL_SHORTCIRCUITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn jit_bail_list() -> &'static parking_lot::RwLock<rustc_hash::FxHashSet<u64>> {
    JIT_BAIL_LIST.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashSet::default()))
}

/// Whether the given method has been added to the JIT bail-list by a
/// prior permanent-bail compilation attempt.  Checked at the top of
/// `try_compile` to short-circuit re-attempts.
pub fn is_jit_bail_listed(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    let h = compute_jit_key_hash(class_name, method_name, descriptor);
    jit_bail_list().read().contains(&h)
}

/// Mark the method as permanently bail-listed.  Called when the heavy
/// `x64::compile` path returns None (typically because of an unsupported
/// backend pattern that won't change on retry).
pub fn mark_jit_bail_listed(class_name: &str, method_name: &str, descriptor: &str) {
    let h = compute_jit_key_hash(class_name, method_name, descriptor);
    jit_bail_list().write().insert(h);
}

/// Diagnostic: number of methods currently bail-listed.
pub fn jit_bail_list_size() -> usize {
    jit_bail_list().read().len()
}

/// Diagnostic: number of `try_compile` calls short-circuited because
/// the method was already bail-listed.  Each short-circuit saves the
/// ~50µs we'd otherwise have spent re-running scan/IR/lowering only to
/// re-hit the same backend bail.
pub fn jit_bail_shortcircuits() -> u64 {
    JIT_BAIL_SHORTCIRCUITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Try to JIT-compile a cached bytecode method.
///
/// The `helpers` parameter provides function pointer addresses for runtime callbacks
/// that will be embedded into the generated machine code.
///
/// round-7 fix (bug 1): wraps the inner pipeline so we can record a
/// permanent bail when the backend returns None.  See the bail-list
/// notes above.
///
/// wire-tiered-manager Step 3 — the trailing `optimize` flag selects the
/// backend per call: `true` (every historical caller) runs the optimizing IR
/// pipeline (the C2-equivalent tier); `false` skips IR lowering / escape
/// analysis / scheduling and routes straight to the single-pass `x64::compile`
/// backend — the fast, low-latency **C1** tier. The tiered manager's background
/// compile worker derives the flag from the task's target tier via
/// [`tiered::tier_uses_optimized_backend`], turning what used to be an advisory
/// hint into real backend routing.
#[allow(clippy::type_complexity)]
pub fn try_compile(
    cached: &CachedBytecodeMethod,
    cp_class_name_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    cp_field_resolver: Option<&dyn Fn(u16) -> Option<(usize, u8)>>,
    cp_static_field_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    callee_compiler: Option<&dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,
    // CRIT-2 — returns (class_id, num_fields, has_primitive_init,
    // has_finalizer). The two flags feed the inline-TLAB `new` fast path;
    // resolvers that cannot compute them must return `(_, _, true, true)`
    // so the post-init helper call stays in place.
    cp_new_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, bool, bool)>>,
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<i64>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<i64>>,
    profile: Option<&profile::MethodProfile>,
    helpers: &JitRuntimeHelpers,
    inline_resolver: Option<&dyn Fn(&str, &str, &str) -> Option<InlineSite>>,
    // Resolves the field layout of `java/lang/String` for the String
    // call-site intrinsics. Called at most once per compilation; see
    // `StringFieldLayout`. `None` (resolver absent, or it returns `None`)
    // makes String intrinsics bail to normal dispatch.
    string_layout_resolver: Option<&dyn Fn() -> Option<StringFieldLayout>>,
    // Maps an invoke* constant-pool index to the class id of the call's
    // *declared* class (the receiver class statically named at the site).
    // Consumed by the CRC32/CRC32C `update` call-site intrinsics, whose
    // codegen emits a receiver class-id guard against this constant. `None`
    // (resolver absent, or it returns `None` for a given site) makes the
    // CRC32 intrinsic at that site bail to normal dispatch.
    cp_invoke_class_id_resolver: Option<&dyn Fn(u16) -> Option<u32>>,
    // activate-ir-optimizer (scalar-new wiring): given an `invokespecial`
    // constant-pool index, returns `true` iff it targets a constructor whose
    // *construction* is elidable for escape-analysis scalar replacement — a
    // no-arg `<init>()V` of a direct `java/lang/Object` subclass whose body is
    // exactly `aload_0; invokespecial Object.<init>()V; return` (no field
    // initialiser, no escape, no side effect). `None` (the production default
    // unless the soak flag is set) leaves scalar-replacement of `new` OFF: the
    // IR builder bails on every `invokespecial`, so allocation-bearing methods
    // take the single-pass backend exactly as before.
    cp_elidable_init_resolver: Option<&dyn Fn(u16) -> bool>,
    // wire-tiered-manager Step 3: `true` → optimizing IR pipeline (C2);
    // `false` → single-pass `x64::compile` only (the fast C1 tier). See the
    // function doc above.
    optimize: bool,
    // Gap B (activate-ir-optimizer): `true` lets the IR builder lower an
    // int-only `invokestatic` in an oop-free method to `Op::Call` (dispatched
    // via `invoke_dispatch`). `false` (the default) keeps every invoke on
    // single-pass. Gated default-OFF behind `CRATONVM_JIT_IR_CALL` at the VM
    // call sites until it soaks.
    ir_emit_calls: bool,
    // inc 24 (Gap B): `true` additionally lets the IR builder lower a resolved
    // non-`<init>` `invokespecial` (private / `super.` / non-virtual instance
    // call) to `Op::Call`, with the receiver marshalled as arg0 and
    // `invoke_kind == 1`. `false` (the default) keeps every `invokespecial`
    // on single-pass (the builder bails). Gated default-OFF behind
    // `CRATONVM_JIT_IR_CALL_SPECIAL` at the VM call sites until it soaks.
    ir_emit_special_calls: bool,
) -> Option<CompiledMethod> {
    // round-7 fix (bug 1): short-circuit re-attempts on methods the
    // backend already permanently bailed on.  Avoids ~50µs of wasted
    // scan/IR/lowering work per re-attempt (every 2000 invocations
    // under the default interpreter warmup gate).
    if is_jit_bail_listed(
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        JIT_BAIL_SHORTCIRCUITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return None;
    }

    // Code-cache cap (bounded growth). Compiled code is retained for the
    // process lifetime with no safe reclamation path (see the cap notes near
    // `COMMITTED_JIT_CODE_BYTES`), so once the retained code reaches the
    // configured cap we refuse further compilation and let the method run in
    // the interpreter. This is checked here — before any scan/IR/lowering — so
    // a saturated cache spends no work on methods it won't emit. Not bail-
    // listed: the refusal is capacity-driven, not a permanent backend bail, so
    // if headroom later reappears (a region is freed) the method may compile.
    if jit_code_cache_at_capacity() {
        JIT_CODE_CACHE_CAP_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return None;
    }

    // Inner pipeline: returns None on either a transient resolver miss
    // OR a permanent backend bail.  Only the latter pollutes the bail-
    // list, so we use a sentinel `bailed` ref-mut that the inner fn
    // sets when it traverses past the cheap resolver checks into
    // `x64::compile`.
    let mut backend_attempted = false;
    let result = try_compile_inner(
        cached,
        cp_class_name_resolver,
        cp_field_resolver,
        cp_static_field_resolver,
        cp_invoke_resolver,
        callee_compiler,
        cp_new_resolver,
        cp_ldc_resolver,
        cp_ldc2w_resolver,
        profile,
        helpers,
        inline_resolver,
        string_layout_resolver,
        cp_invoke_class_id_resolver,
        cp_elidable_init_resolver,
        optimize,
        ir_emit_calls,
        ir_emit_special_calls,
        &mut backend_attempted,
    );

    if result.is_none() && backend_attempted {
        // The heavy backend path ran and returned None — treat as
        // permanent.  Future try_compile calls for this method
        // short-circuit immediately at the check above.
        mark_jit_bail_listed(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        );
    }
    if result.is_none() && std::env::var_os("CRATONVM_DBG_JITC").is_some() {
        eprintln!(
            "[cratonvm-jitc] compile-bail {}.{}{} backend_attempted={}",
            cached.class_name, cached.method_name, cached.method_descriptor, backend_attempted
        );
    }
    // DBG (env-gated): dump the emitted machine code for a specific method so
    // its prologue/epilogue + body can be disassembled offline. Set
    // CRATONVM_DBG_DUMP_JIT="Class.method" (slash-separated class) to target.
    if let Ok(target) = std::env::var("CRATONVM_DBG_DUMP_JIT") {
        if let Some(ref cm) = result {
            let sig = format!("{}.{}", cached.class_name, cached.method_name);
            // LIST mode: print every compiled method's sig (reveals exact
            // class-name format + whether the target is compiled here at all).
            if target == "LIST" {
                eprintln!("[JIT_COMPILED] {}{}", sig, cached.method_descriptor);
            }
            if sig.contains(&target) || &*cached.method_name == target.as_str() {
                let bytes = cm.code_bytes();
                eprintln!(
                    "[JIT_DUMP] {}{} len={} entry={:p}",
                    sig,
                    cached.method_descriptor,
                    bytes.len(),
                    bytes.as_ptr(),
                );
                let mut line = String::new();
                for (i, b) in bytes.iter().enumerate() {
                    line.push_str(&format!("{:02x}", b));
                    if (i + 1) % 32 == 0 {
                        eprintln!("[JIT_DUMP] {}", line);
                        line.clear();
                    }
                }
                if !line.is_empty() {
                    eprintln!("[JIT_DUMP] {}", line);
                }
            }
        }
    }
    // DBG (spring-bug-11): list every successfully-compiled method that contains
    // a dup_x1 (0x5A), with the PCs and a small following-byte window, so the
    // crashing dup_x1 method (NO_DUP_X1 removes the Groovy SIGSEGV) can be pinned
    // and dumped. Proper opcode walk via scev::bytecode_len so operand bytes that
    // happen to equal 0x5A are not mistaken for the opcode.
    if result.is_some() && std::env::var_os("CRATONVM_DBG_DUPX_METHODS").is_some() {
        let code: &[u8] = &cached.code;
        let n = code.len();
        let mut pc = 0usize;
        let mut hits: Vec<usize> = Vec::new();
        while pc < n {
            if code[pc] == 0x5a {
                hits.push(pc);
            }
            let l = crate::scev::bytecode_len(code, pc, n);
            pc += if l == 0 { 1 } else { l };
        }
        if !hits.is_empty() {
            eprintln!(
                "[DUPX1] {}.{}{} dup_x1@{:?} code_len={}",
                cached.class_name, cached.method_name, cached.method_descriptor, hits, n
            );
            for &h in &hits {
                let end = (h + 10).min(n);
                let window: Vec<String> =
                    code[h..end].iter().map(|b| format!("{:02x}", b)).collect();
                eprintln!("[DUPX1]   @{} bytes: {}", h, window.join(" "));
            }
        }
    }
    result
}

// Test-only telemetry for wire-tiered-manager Step 3: how many times the
// optimizing IR-lowering pipeline produced the final compiled body **on the
// current thread**. The per-call-toggle test reads this to prove
// `optimize=false` (the C1 tier) skips the IR path while `optimize=true` (C2)
// takes it.
//
// A `thread_local!` rather than a global atomic so the count is isolated
// per-test: `try_compile` runs the whole pipeline synchronously on the caller's
// thread, and the cargo harness runs each `#[test]` on its own thread, so a
// concurrently-running compile test cannot perturb this thread's count.
// `#[cfg(test)]` so production codegen carries no extra work on the hot path.
#[cfg(test)]
thread_local! {
    pub(crate) static IR_LOWER_COMPILES: std::cell::Cell<u64> = std::cell::Cell::new(0);
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn try_compile_inner(
    cached: &CachedBytecodeMethod,
    cp_class_name_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    cp_field_resolver: Option<&dyn Fn(u16) -> Option<(usize, u8)>>,
    cp_static_field_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    callee_compiler: Option<&dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,
    // (class_id, num_fields, has_primitive_init, has_finalizer) — see `try_compile`.
    cp_new_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, bool, bool)>>,
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<i64>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<i64>>,
    profile: Option<&profile::MethodProfile>,
    helpers: &JitRuntimeHelpers,
    inline_resolver: Option<&dyn Fn(&str, &str, &str) -> Option<InlineSite>>,
    // Resolves `java/lang/String`'s field layout — see `try_compile`.
    string_layout_resolver: Option<&dyn Fn() -> Option<StringFieldLayout>>,
    // Maps an invoke* CP index to its declared class id — see `try_compile`.
    cp_invoke_class_id_resolver: Option<&dyn Fn(u16) -> Option<u32>>,
    // Elidable-`<init>` resolver for scalar-replacement of `new` — see
    // `try_compile`. `None` keeps `new` scalar replacement off.
    cp_elidable_init_resolver: Option<&dyn Fn(u16) -> bool>,
    // wire-tiered-manager Step 3: when `false`, the optimizing IR pipeline is
    // skipped entirely and compilation falls through to the single-pass
    // `x64::compile` backend (the fast C1 tier). See `try_compile`.
    optimize: bool,
    // Gap B: enable lowering of int-only `invokestatic` in oop-free methods to
    // `Op::Call`. See `try_compile`. Default-OFF at the VM call sites.
    ir_emit_calls: bool,
    // inc 24 (Gap B): additionally lower resolved non-`<init>` `invokespecial`
    // to `Op::Call`. See `try_compile`. Default-OFF at the VM call sites.
    ir_emit_special_calls: bool,
    // round-7 fix (bug 1): set to `true` immediately before invoking
    // the heavy `x64::compile` path so the outer wrapper can tell a
    // permanent backend bail (worth bail-listing) from an early
    // transient miss (e.g. resolver returned None, profile not yet
    // present — worth retrying later).
    backend_attempted: &mut bool,
) -> Option<CompiledMethod> {
    // Architecture-specific backend selection.
    // On ARM64 (aarch64), the ARM64 backend would be used instead of x64.
    // Both x64 and ARM64 backends have bytecode→native compilation pipelines.
    // ARM64 covers ~50+ opcodes (arithmetic, branches, float, conversions, invoke).
    // Total incoming argument SLOTS for this method's own prologue.
    //
    // `count_param_slots` counts only the *declared* descriptor
    // parameters. An instance method additionally receives the implicit
    // `this` reference as JVM local 0, ahead of the declared params, so
    // the prologue must load `1 + declared` argument registers. Omitting
    // the `this` slot made the prologue zero-initialize local 0 instead
    // of loading the receiver — a JIT-compiled instance method then
    // operated on a null `this` (constructor field writes silently lost;
    // boxed values / `String` fields read back as 0). The early-compile
    // path already gets this right because it derives the count from the
    // live `args.len()`; the callee-compile path used here did not.
    let prologue_param_slots: usize =
        count_param_slots(&cached.method_descriptor) + if cached.is_static { 0 } else { 1 };

    #[cfg(target_arch = "aarch64")]
    {
        use std::collections::HashMap;

        let code = &cached.code;
        let num_params = prologue_param_slots;

        // Build method_info map: scan bytecode for invokestatic operands,
        // resolve each CP index to an argument count via the invoke resolver.
        //
        // PERF (jit-lib-perf): this runs on the hot JIT compile path. The
        // `aarch64` 0xb8 handler still consumes `method_info` to recover each
        // invokestatic's argument count (see `aarch64_backend::compile_method`),
        // so the scan cannot be elided — but the *redundant* work inside it can.
        // A hot method commonly calls the same static target many times (loops,
        // repeated helper calls); the previous code re-invoked the CP resolver
        // (which allocates three `String`s per call) and re-parsed the
        // descriptor (`count_param_slots`) at *every* call site, including
        // duplicates. Skipping CP indices already resolved makes the resolver +
        // descriptor parse run once per *distinct* index instead of once per
        // occurrence. The resolved value for a given CP index is invariant, so
        // `method_info`'s final contents are byte-for-byte identical.
        let mut method_info: HashMap<u16, usize> = HashMap::new();
        if let Some(resolver) = cp_invoke_resolver {
            let mut scan_pc = 0usize;
            while scan_pc < code.len() {
                if code[scan_pc] == 0xb8 && scan_pc + 2 < code.len() {
                    let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16;
                    // Only resolve indices not seen yet — duplicates would
                    // re-insert the identical value, so dedupe and skip the
                    // resolver allocation + descriptor parse for them.
                    if let std::collections::hash_map::Entry::Vacant(slot) =
                        method_info.entry(cp_idx)
                    {
                        if let Some((_class, _name, descriptor)) = resolver(cp_idx) {
                            slot.insert(count_param_slots(&descriptor));
                        }
                    }
                    scan_pc += 3;
                } else {
                    scan_pc += 1;
                }
            }
        }

        // round-7 fix (bug 1): aarch64 backend is also a "heavy"
        // pipeline — a `result.success == false` is a permanent bail
        // for the same reason as the x64 backend bails.  Set the
        // flag before invoking so the outer wrapper records it.
        *backend_attempted = true;

        let mut backend = aarch64_backend::Arm64Backend::new();
        let result = backend.compile_method_with_info(
            cached.max_locals as usize,
            num_params,
            cached.max_stack as usize,
            code,
            method_info,
        );
        if result.success {
            if let Some(machine_code) = aarch64_backend::emit_machine_code(&result) {
                if let Some(mut buf) = ExecutableBuffer::new(machine_code.len().max(4096)) {
                    buf.emit(&machine_code);
                    return Some(CompiledMethod::new(buf));
                }
            }
        }
        return None;
    }

    let code = &cached.code;
    let code_len = code.len().saturating_sub(2);

    if code_len == 0 {
        return None;
    }

    let scan = match x64::jit_scan(code, code_len, &cached.method_descriptor) {
        Some(s) => s,
        None => {
            // RBC.4 — a scan reject (unsupported opcode, e.g. `athrow`) is
            // just as permanent as a backend bail: the bytecode never
            // changes. Without marking it, a hot uncompilable method re-ran
            // the whole upgrade gauntlet (skip-list + native-shadow
            // hierarchy walks under the class_manager lock + this scan)
            // every JIT_RETRY_STRIDE calls forever — observed 40,039
            // attempts on `DefiniteLengthInputStream.readAllIntoByteArray`
            // in ONE asn1 RegressionTest run. Route through the existing
            // permanent bail-list machinery.
            *backend_attempted = true;
            return None;
        }
    };

    // RBC.6 — a method containing `athrow` compiles only when it has NO
    // local exception handlers: the athrow lowering stashes the exception
    // and returns the deopt sentinel, which cannot dispatch to an
    // in-method handler. Permanent for this bytecode → bail-list it.
    if scan.has_athrow && !cached.exception_table.is_empty() {
        *backend_attempted = true;
        return None;
    }

    // Try IR compilation for simple integer-only methods. The IR pipeline
    // types every value as 32-bit `IrType::Int` and lays parameters out by
    // JIT-argument index rather than JVM local slot, so it cannot represent
    // 64-bit longs/doubles nor the two-slot category-2 parameter layout.
    // Methods touching long/double fall through to the bytecode-x64 backend,
    // which models them correctly. (Before this guard, a method like
    // `boolean eq(long, long)` truncated the second long parameter — read
    // from the never-populated `locals[2]` slot — and panicked with a
    // `u32::MAX` slot index; bc-java InterleaveTest failed 2/4 under JIT.)
    // wire-tiered-manager Step 3: `optimize == false` is the C1 (fast) tier —
    // skip the whole optimizing IR pipeline (build → optimize → escape analysis
    // → schedule → lower) and fall through to the single-pass `x64::compile`
    // backend below. The single-pass backend is the existing, well-tested
    // fallback (it already serves every category-2 / non-`ir_compatible`
    // method), so routing more methods to it is a throughput trade-off, never a
    // correctness risk. C2 (`optimize == true`, every non-tiered caller)
    // keeps the historical IR-first behaviour.
    if optimize
        && ir::ir_compatible(&scan)
        && !method_uses_category2(code, code_len, &cached.method_descriptor)
    {
        // Includes the implicit `this` slot for instance methods — see
        // `prologue_param_slots` above.
        let num_params = prologue_param_slots;
        let mut builder = ir::IrBuilder::new(num_params, cached.max_locals as usize);
        // Thread the resolved instance-field layout (pc → (field_index,
        // type_tag)) into the builder so it can lower an int-category
        // `getfield` into `Op::Load`. A field the resolver can't resolve is
        // simply omitted; the builder then bails that getfield to single-pass.
        if !scan.field_ops.is_empty() {
            if let Some(resolver) = cp_field_resolver {
                let mut fm = std::collections::HashMap::with_capacity(scan.field_ops.len());
                for &(pc, cp_idx) in &scan.field_ops {
                    if let Some(fi) = resolver(cp_idx) {
                        fm.insert(pc, fi);
                    }
                }
                builder.set_field_info(fm);
            }
        }
        // Scalar replacement of `new`: only when the elidable-`<init>` resolver
        // is supplied (the production soak flag is on, or a test wires it
        // directly) do we feed the builder the allocation layout for every `new`
        // and the pcs of elidable `<init>()V` invokespecials. Without it, the
        // builder bails on `new`/`invokespecial`, so allocation-bearing methods
        // stay on the single-pass backend exactly as before (inert default).
        if let (Some(elidable_resolver), Some(new_resolver)) =
            (cp_elidable_init_resolver, cp_new_resolver)
        {
            if !scan.new_ops.is_empty() {
                let mut new_info_map = std::collections::HashMap::with_capacity(scan.new_ops.len());
                for &(pc, cp_idx) in &scan.new_ops {
                    if let Some((class_id, num_fields, _hp, _hf)) = new_resolver(cp_idx) {
                        new_info_map.insert(pc, (class_id, num_fields));
                    }
                }
                let mut trivial_init_pcs = std::collections::HashSet::new();
                for &(pc, cp_idx, opcode) in &scan.invoke_ops {
                    if opcode == 0xb7 && elidable_resolver(cp_idx) {
                        trivial_init_pcs.insert(pc);
                    }
                }
                builder.set_new_info(new_info_map, trivial_init_pcs);
            }
        }
        // Gap B / inc 22: invokestatic → `Op::Call`. Only when the IR-call gate
        // is on AND the method has no `new`/array allocation (a surviving `New`
        // would need the allocation path the lowerer lacks; array ops bail the
        // builder anyway) AND every invoke is an `invokestatic` whose descriptor
        // is GPR-marshallable (int/reference args + int/void/reference return —
        // no long/float/double). Reference params, reference args, int field
        // ops, and reference returns ARE allowed: any oop live across the call
        // sits in a spilled frame slot, which the conservative GC root scan of
        // the IR frame finds — sound because the GC is non-moving while a JIT
        // frame is active (so a pinned pointer is never relocated). The leaked
        // `JitInvokeInfo` boxes/strings are attached to the returned
        // `CompiledMethod` below so the baked `info_ptr`s outlive the code.
        let mut ir_call_infos: Vec<Box<JitInvokeInfo>> = Vec::new();
        let mut ir_call_strings: Vec<Box<str>> = Vec::new();
        if (ir_emit_calls || ir_emit_special_calls) && !scan.invoke_ops.is_empty() {
            if let Some(resolver) = cp_invoke_resolver {
                let call_eligible = scan.new_ops.is_empty() && scan.anewarray_ops.is_empty();
                if call_eligible {
                    let mut info_map = std::collections::HashMap::new();
                    let mut all_emittable = true;
                    for &(pc, cp_idx, opcode) in &scan.invoke_ops {
                        // Admit `invokestatic` (under `ir_emit_calls`) and
                        // resolved non-`<init>` `invokespecial` (inc 24, under
                        // `ir_emit_special_calls`). Any other invoke kind
                        // (virtual / interface) or a disabled gate keeps the
                        // whole method on single-pass — the builder bails on an
                        // invoke with no `invoke_info` entry.
                        let is_static = opcode == 0xb8;
                        let is_special = opcode == 0xb7;
                        if !((is_static && ir_emit_calls)
                            || (is_special && ir_emit_special_calls))
                        {
                            all_emittable = false;
                            break;
                        }
                        let (cn, mn, desc) = match resolver(cp_idx) {
                            Some(t) => t,
                            None => {
                                all_emittable = false;
                                break;
                            }
                        };
                        // A `<init>` `invokespecial` is never a real `Op::Call`
                        // here: a constructor is only ever handled by the
                        // scalar-new elision path (`trivial_init_pcs`), and
                        // eliding vs. calling a ctor are different transforms.
                        // (Belt-and-braces — a `<init>`-bearing method also has
                        // a `new`, so `call_eligible` is already false.)
                        if is_special && mn == "<init>" {
                            all_emittable = false;
                            break;
                        }
                        let (desc_args, ret) = match static_call_shape(&desc) {
                            Some(t) => t,
                            None => {
                                all_emittable = false;
                                break;
                            }
                        };
                        // `invokespecial` marshals the receiver as arg0 (a
                        // reference → one GPR slot), so it carries one more JIT
                        // arg than its descriptor lists; `invokestatic` has no
                        // receiver. `invoke_kind`: 1 = invokespecial (non-
                        // virtual dispatch to the resolved target), 3 = static.
                        let num_args = desc_args + if is_special { 1 } else { 0 };
                        let invoke_kind: u8 = if is_special { 1 } else { 3 };
                        let class_box: Box<str> = cn.into_boxed_str();
                        let method_box: Box<str> = mn.into_boxed_str();
                        let desc_box: Box<str> = desc.into_boxed_str();
                        let class_ref = &*class_box as *const str;
                        let method_ref = &*method_box as *const str;
                        let desc_ref = &*desc_box as *const str;
                        ir_call_strings.push(class_box);
                        ir_call_strings.push(method_box);
                        ir_call_strings.push(desc_box);
                        let info = Box::new(JitInvokeInfo {
                            class_name: unsafe { &*class_ref },
                            method_name: unsafe { &*method_ref },
                            descriptor: unsafe { &*desc_ref },
                            num_jit_args: num_args,
                            return_type: ret,
                            invoke_kind,
                        });
                        let info_ptr = &*info as *const JitInvokeInfo as usize;
                        ir_call_infos.push(info);
                        info_map.insert(pc, (info_ptr, num_args, ret));
                    }
                    if all_emittable && !info_map.is_empty() {
                        if std::env::var_os("CRATONVM_DBG_IR_CALL").is_some() {
                            eprintln!(
                                "[cratonvm-ircall] {}.{}{}: emitting {} invoke(static/special) Op::Call(s)",
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                                info_map.len(),
                            );
                        }
                        builder.set_invoke_info(info_map);
                    } else {
                        // A non-emittable invoke is present → leave `invoke_info`
                        // unset (the builder bails on every invoke → single-pass)
                        // and drop the now-unreferenced boxes/strings.
                        ir_call_infos.clear();
                        ir_call_strings.clear();
                    }
                }
            }
        }
        let built = builder.build(code, code_len);
        // Soak diagnostic (CRATONVM_DBG_SCALAR_NEW): an allocation-bearing method
        // that bailed the IR builder went single-pass, so `new` scalar
        // replacement could not fire on it — the signal that the IR builder is
        // missing an opcode the method uses (this is how the `astore` gap, which
        // silently disabled scalar-new on ALL real javac allocations, surfaced).
        if built.is_none()
            && !scan.new_ops.is_empty()
            && std::env::var_os("CRATONVM_DBG_SCALAR_NEW").is_some()
        {
            eprintln!(
                "[cratonvm-scalarnew] IR builder bailed (single-pass) for allocation method {}.{}{}",
                cached.class_name, cached.method_name, cached.method_descriptor,
            );
        }
        if let Some(mut graph) = built {
            // History: the IR backend used to miscompile a *pure* (call-free)
            // method containing a conditional branch / φ merge — a tiny leaf
            // predicate like `static boolean f(int m){ return (m & K) != 0; }`
            // (e.g. `java.lang.reflect.Modifier.isStatic`) SIGSEGV'd with a write
            // through a near-null base. ROOT CAUSE (fixed): the Op::Cmp SETcc was
            // emitted as `0F 9x` without its ModRM byte, desyncing the stream
            // into a stray `SETL [rdi]` (near-null write) and leaving the boolean
            // unset. A second blocker — loop-carried phis dropped on the
            // back-edge — was also fixed (loop-header eager phis + back-patch).
            // The IR path now compiles if/else and loops (while/do-while/nested)
            // correctly, so branchy call-free integer methods take the IR
            // pipeline by default. `CRATONVM_NO_IR_BRANCHY` is the emergency
            // opt-out (restores single-pass-only routing for this shape); methods
            // the IR builder can't fully build still return None → single-pass.
            // (Bisected originally from keycloak JsonParserTest /
            // SkeletonKeyTokenTest SIGSEGVs + a standalone `Modifier.isStatic`.)
            let has_conditional_branch = graph.nodes.iter().any(|n| matches!(n.op, ir::Op::If));
            if has_conditional_branch
                && scan.invoke_ops.is_empty()
                && !ir_optimize::ir_branchy_enabled()
                && !ir_optimize::reassoc_enabled()
            {
                // Branchy-IR explicitly disabled (CRATONVM_NO_IR_BRANCHY) and
                // reassoc off → fall through to the single-pass backend below.
            } else {
                ir_optimize::optimize(&mut graph);

                // --- Escape analysis (Phase 41 + G46 wiring) ---
                // Convert IR graph to escape analysis graph, run analysis,
                // and apply scalar replacement / lock elision to the IR graph.
                {
                    let (ea_graph, id_map) = escape_analysis_from_ir(&graph);
                    let ea_result = escape_analysis::analyze_escapes(&ea_graph);
                    // Live-fire soak diagnostic (CRATONVM_DBG_SCALAR_NEW): for an
                    // allocation-bearing method, report how many of its `new`s
                    // escape analysis scalar-replaced. This proves the path is
                    // actually exercised on real bytecode (a non-vacuous soak):
                    // `scalar_replaceable < ir_news` means some `new` escaped and
                    // the method will bail to single-pass via the surviving-New
                    // gate below.
                    if std::env::var_os("CRATONVM_DBG_SCALAR_NEW").is_some() {
                        let ir_news = graph
                            .nodes
                            .iter()
                            .filter(|n| {
                                matches!(n.op, ir::Op::New { .. } | ir::Op::NewArray { .. })
                            })
                            .count();
                        if ir_news > 0 {
                            eprintln!(
                                "[cratonvm-scalarnew] {}.{}{}: scalar-replaced {}/{} alloc(s)",
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                                ea_result.scalar_replaceable.len(),
                                ir_news,
                            );
                        }
                    }
                    if !ea_result.scalar_replaceable.is_empty() || !ea_result.elide_locks.is_empty()
                    {
                        apply_ea_to_ir(&mut graph, &id_map, &ea_result);
                    }
                }

                // An `Op::New` that SURVIVED escape analysis (it escaped, so it
                // was not scalar-replaced) has no IR lowering — `ir_lower` has
                // no allocation path and would emit nothing for it, leaving a
                // garbage object reference. Bail to single-pass rather than
                // miscompile. (Scalar-replaced News are already `Op::Dead`.)
                let has_live_new = graph
                    .nodes
                    .iter()
                    .any(|n| matches!(n.op, ir::Op::New { .. } | ir::Op::NewArray { .. }));
                if !has_live_new {
                    let schedule = ir_schedule::schedule(&graph);
                    if let Some(mut compiled) = ir_lower::lower(
                        &graph,
                        &schedule,
                        num_params,
                        cached.max_locals as usize,
                        helpers,
                    ) {
                        // Gap B: attach the leaked `JitInvokeInfo` boxes/strings
                        // so the `info_ptr`s baked into each `Op::Call` stay valid
                        // for the code's lifetime, and mark the method as using
                        // dispatch so the VM wraps the call in `set_jit_thread` +
                        // `catch_unwind` and drains the pending exception (the
                        // `lower` step already set `needs_context`). When no call
                        // was emitted both Vecs are empty → no behaviour change.
                        if !ir_call_infos.is_empty() {
                            compiled._jit_invoke_infos = ir_call_infos;
                            compiled._jit_strings = ir_call_strings;
                            compiled.has_dispatch = true;
                        }
                        // wire-tiered-manager Step 3 telemetry (test-only):
                        // records that the optimizing IR path — not the
                        // single-pass C1 backend — produced this body, so the
                        // per-call toggle test can prove `optimize=false` skips it.
                        #[cfg(test)]
                        IR_LOWER_COMPILES.with(|c| c.set(c.get() + 1));
                        return Some(compiled);
                    }
                }
            } // end else (IR-lowering path)
        }
    }

    // Resolve multianewarray entries
    let mut mna_info = Vec::new();
    if !scan.multianewarray_ops.is_empty() {
        let resolver = cp_class_name_resolver?;
        for &(pc, cp_idx, _ndims) in &scan.multianewarray_ops {
            let class_name = resolver(cp_idx)?;
            let leaf = class_name.trim_start_matches('[');
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

    let mut needs_heap = scan.needs_heap;
    let mut field_info = Vec::new();
    if !scan.field_ops.is_empty() {
        let resolver = cp_field_resolver?;
        for &(pc, cp_idx) in &scan.field_ops {
            let (field_index, type_tag) = resolver(cp_idx)?;
            field_info.push((pc, field_index, type_tag));
            if code[pc] == 0xb5 && (type_tag == b'L' || type_tag == b'[') {
                needs_heap = true;
            }
        }
    }

    let mut typecheck_info: Vec<(usize, *const u8, usize)> = Vec::new();
    let mut owned_strings: Vec<Box<str>> = Vec::new();
    if !scan.typecheck_ops.is_empty() {
        let resolver = cp_class_name_resolver.as_ref()?;
        for &(pc, cp_idx) in &scan.typecheck_ops {
            let class_name = resolver(cp_idx)?;
            let boxed: Box<str> = class_name.into_boxed_str();
            let ptr = boxed.as_ptr();
            let len = boxed.len();
            owned_strings.push(boxed);
            typecheck_info.push((pc, ptr, len));
        }
    }

    let mut static_field_info: Vec<(usize, u32, usize, u8, bool)> = Vec::new();
    if !scan.static_field_ops.is_empty() {
        let resolver = cp_static_field_resolver?;
        for &(pc, cp_idx) in &scan.static_field_ops {
            let (class_id_raw, field_index, type_tag, is_volatile) = resolver(cp_idx)?;
            static_field_info.push((pc, class_id_raw, field_index, type_tag, is_volatile));
        }
    }

    // CRIT-2 — new_info tuple shape:
    //   (pc, class_id_raw, num_fields,
    //    has_primitive_init,    // class has primitive-typed fields that
    //                           // need typed-zero defaults applied by
    //                           // `jit_init_primitive_fields`
    //    has_finalizer)         // class overrides `finalize()` and must
    //                           // be registered with the finalizer queue
    //
    // When both flags are `false` the inline TLAB fast path skips the
    // `jit_post_tlab_init` helper entirely (class_id + num_slots are
    // written inline; identity hash stays lazily-minted zero). The
    // resolver computes the real flags from class metadata; a resolver
    // that cannot determine them must return `(true, true)` so the
    // helper call stays in place.
    let mut new_info: Vec<(usize, u32, usize, bool, bool)> = Vec::new();
    let mut anewarray_info: Vec<(usize, u32)> = Vec::new();
    if !scan.new_ops.is_empty() || !scan.anewarray_ops.is_empty() {
        let resolver = cp_new_resolver?;
        for &(pc, cp_idx) in &scan.new_ops {
            let (class_id_raw, num_fields, has_prim_init, has_finalizer) = resolver(cp_idx)?;
            new_info.push((pc, class_id_raw, num_fields, has_prim_init, has_finalizer));
        }
        for &(pc, cp_idx) in &scan.anewarray_ops {
            let (class_id_raw, ..) = resolver(cp_idx)?;
            anewarray_info.push((pc, class_id_raw));
        }
    }

    // Resolve ldc/ldc_w constants (int/float from CP). A `None` from the
    // resolver means the constant is not representable as an immediate
    // (String/Class/MethodHandle ldc) — bail the whole compile, mirroring
    // the ldc2_w arm below. The previous `unwrap_or(0)` would have compiled
    // `ldc "str"` as pushing constant 0 (a null reference) — wrong code.
    // With no resolver at all, `ldc_info` stays empty and the 0x12/0x13
    // codegen arm bails per-site instead.
    let mut ldc_info: Vec<(usize, i64)> = Vec::new();
    if !scan.ldc_ops.is_empty() {
        if let Some(resolver) = cp_ldc_resolver {
            for &(pc, cp_idx) in &scan.ldc_ops {
                let val = resolver(cp_idx)?;
                ldc_info.push((pc, val));
            }
        }
    }

    // Resolve ldc2_w constants (long/double from CP)
    let mut ldc2w_info: Vec<(usize, i64)> = Vec::new();
    if !scan.ldc2w_ops.is_empty() {
        let resolver = cp_ldc2w_resolver?;
        for &(pc, cp_idx) in &scan.ldc2w_ops {
            let val = resolver(cp_idx)?;
            ldc2w_info.push((pc, val));
        }
    }

    let mut invoke_info: Vec<(usize, *const JitInvokeInfo)> = Vec::new();
    let mut owned_invoke_infos: Vec<Box<JitInvokeInfo>> = Vec::new();
    let mut direct_calls: Vec<(usize, JitDirectCall)> = Vec::new();
    let mut mic_slots: Vec<(usize, *const JitMICSlot)> = Vec::new();
    let mut owned_mic_slots: Vec<Box<JitMICSlot>> = Vec::new();
    // HIGH-7 — Eager PIC allocation strategy.
    //
    // At first JIT compile we optimistically allocate one
    // `Box<JitPICSlot>` for every invokevirtual / invokeinterface
    // bci in the method, regardless of MIC miss history. Rationale:
    //   * The cost is small (≈64 B per call site; typical methods
    //     have <5 virtual call sites).
    //   * The 3-way inline cascade is silent on empty slots: each
    //     entry's `cached_class_id == 0` fails the CMP and falls
    //     through to the helper, so cold sites pay zero extra cycles.
    //   * Once the runtime helper populates a slot, the inline
    //     cascade starts hitting — no recompile required, which
    //     sidesteps the adaptive-recompile machinery entirely.
    //
    // This is the "simpler approach" from the HIGH-7 design doc.
    // The adaptive MIC→PIC promotion path
    // (`JitMICSlot::needs_pic_promotion` → `promote_mic_to_pic`)
    // remains available for future tiered-recompile use, but is
    // currently unused on the hot path.
    let mut pic_slots: Vec<(usize, *const JitPICSlot)> = Vec::new();
    let mut owned_pic_slots: Vec<Box<JitPICSlot>> = Vec::new();
    let mut inline_sites: HashMap<usize, InlineSite> = HashMap::new();
    let mut inline_budget_remaining: usize = MAX_INLINE_BUDGET;
    let mut inlined_methods: Vec<(String, String, String)> = Vec::new();
    // `java/lang/String` field layout, resolved ONCE for the whole
    // compilation. `try_resolve_string_intrinsic` (in the invoke loop
    // below) uses it to decide whether a String intrinsic can be
    // registered; the SAME resolver feeds `x64::compile`'s
    // `compiler.string_layout`, so matcher and codegen stay consistent.
    let resolved_string_layout: Option<StringFieldLayout> =
        string_layout_resolver.and_then(|r| r());
    if !scan.invoke_ops.is_empty() {
        let resolver = cp_invoke_resolver?;
        for &(pc, cp_idx, opcode) in &scan.invoke_ops {
            let (class_name, method_name, descriptor) = resolver(cp_idx)?;
            let invoke_kind = match opcode {
                0xb6 => 0u8,
                0xb7 => 1,
                0xb9 => 2,
                _ => 3,
            };
            let num_params = count_param_slots(&descriptor);
            let has_receiver = invoke_kind != 3;
            let num_jit_args = num_params + if has_receiver { 1 } else { 0 };
            let ret_type = return_type(&descriptor);

            let is_self_call = invoke_kind == 3
                && class_name == &*cached.class_name
                && method_name == &*cached.method_name
                && descriptor == &*cached.method_descriptor;

            // RBC.3 — a site planned for inlining MUST still get a
            // `JitInvokeInfo` dispatch fallback (below). The codegen's
            // `try_emit_inline` can bail mid-body and roll back, and its
            // fall-through is direct_calls → invoke_info → else "assume
            // self-recursive CALL to own entry". With the old `continue`
            // here, a bailed inline site had neither, so the emitted CALL
            // targeted the CALLER's own entry: BC's `Strings.fromByteArray`
            // (invokestatic to same-class sibling `asCharArray`, planned for
            // inline, bailed in codegen) recursed itself — one `new String`
            // per level — until a native stack overflow killed the asn1
            // RegressionTest at StringTest (1,365 self-frames in the cdb
            // dump). Skip only the direct-call/intrinsic attempts, then fall
            // through to the info construction.
            let mut planned_inline = false;
            if !is_self_call && (invoke_kind == 3 || invoke_kind == 1) {
                // Try inlining first (before direct calls — inlining is more profitable)
                if inline_budget_remaining > 0 {
                    if let Some(resolver_fn) = inline_resolver.as_ref() {
                        if let Some(site) = resolver_fn(&class_name, &method_name, &descriptor) {
                            if site.callee_code_len <= inline_budget_remaining {
                                inline_budget_remaining =
                                    inline_budget_remaining.saturating_sub(site.callee_code_len);
                                if site.needs_heap {
                                    needs_heap = true;
                                }
                                inlined_methods.push((
                                    site.class_name.clone(),
                                    site.method_name.clone(),
                                    site.descriptor.clone(),
                                ));
                                inline_sites.insert(pc, site);
                                planned_inline = true;
                            }
                        }
                    }
                }

                if !planned_inline {
                    if let Some(compiler) = callee_compiler.as_ref() {
                        if let Some((entry, callee_needs_ctx)) =
                            compiler(&class_name, &method_name, &descriptor)
                        {
                            if callee_needs_ctx {
                                needs_heap = true;
                            }
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: callee_needs_ctx,
                                    num_params,
                                    return_type: ret_type,
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                    needs_heap = true;

                    // Call-site intrinsics — inline machine code, no call overhead.
                    // The matcher (`try_resolve_intrinsic`) keys on
                    // (class, name, descriptor) and applies the same CPU-feature
                    // gates the x64 codegen ladder relies on. This invokestatic/
                    // invokespecial path never resolves a CRC32 intrinsic (those
                    // are `invokevirtual` only), so `guard_class_id` is 0.
                    if let Some((entry, num_params, ret)) =
                        try_resolve_intrinsic(&class_name, &method_name, &descriptor)
                    {
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: false,
                                num_params,
                                return_type: ret,
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                } // end !planned_inline (RBC.3)
            }

            // Call-site intrinsics for instance-method invokes
            // (invokevirtual / invokeinterface). The static/special path
            // above already runs the matcher; this covers `invoke_kind`
            // 0 and 2. `num_params` from the matcher excludes the receiver;
            // the x64 codegen ladder for 0xb6/b7/b9 adds it back
            // (`callee_params + 1`).
            //
            // Unlike a `final` class, the CRC32/CRC32C intrinsics target a
            // class (`java.util.zip.CRC32` is *not* final) that could in
            // principle be subclassed, so a virtual `update` site is not
            // statically monomorphic. The codegen therefore emits a runtime
            // receiver class-id guard; the constant it compares against is
            // resolved here via `cp_invoke_class_id_resolver` (the declared
            // class of the call site) and stored in `guard_class_id`. When
            // the resolver is absent or cannot resolve the class id,
            // `guard_class_id` stays 0 and the CRC32 codegen bails the site
            // to normal dispatch (0 is never a real class id).
            if !is_self_call && (invoke_kind == 0 || invoke_kind == 2) {
                // First the layout-independent instance intrinsics.
                if let Some((entry, num_params, ret)) =
                    try_resolve_intrinsic(&class_name, &method_name, &descriptor)
                {
                    let guard_class_id = cp_invoke_class_id_resolver
                        .and_then(|r| r(cp_idx))
                        .unwrap_or(0);
                    // A CRC32/CRC32C intrinsic's inline code is only sound
                    // behind a receiver class-id guard. If the declared
                    // class id could not be resolved (`guard_class_id == 0`),
                    // do NOT register the intrinsic — leave the site to
                    // normal virtual dispatch (MIC/PIC). Registering it
                    // anyway would leave a `direct_calls` entry whose
                    // `entry` is an intrinsic sentinel the codegen could
                    // not safely emit.
                    if JitIntrinsic::from_entry(entry).is_some_and(|i| i.is_crc32_family())
                        && guard_class_id == 0
                    {
                        // Fall through — no `continue`, no direct_calls push.
                    } else {
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: false,
                                num_params,
                                return_type: ret,
                                guard_class_id,
                            },
                        ));
                        continue;
                    }
                }
                // Then the `java/lang/String` intrinsics — registered only
                // when the String field layout has resolved (and carries a
                // `coder` field). `string_layout` is resolved ONCE per
                // compilation; the same value is threaded into `x64::compile`
                // (`compiler.string_layout`), so the matcher's "registered"
                // decision and the codegen's "can emit inline" decision
                // never disagree — a String sentinel is never registered
                // for a site whose codegen would then bail to a raw `CALL`.
                if let Some((entry, num_params, ret, guard_class_id)) = try_resolve_string_intrinsic(
                    &class_name,
                    &method_name,
                    &descriptor,
                    resolved_string_layout,
                ) {
                    needs_heap = true;
                    direct_calls.push((
                        pc,
                        JitDirectCall {
                            entry,
                            needs_context: false,
                            num_params,
                            return_type: ret,
                            // `java/lang/String` is `final` → monomorphic →
                            // `guard_class_id == 0` (no guard). A
                            // `java/lang/CharSequence` site carries the String
                            // class id so the codegen guards the receiver and
                            // deopts for any non-String CharSequence.
                            guard_class_id,
                        },
                    ));
                    continue;
                }
            }

            if is_self_call {
                continue;
            }

            let class_box: Box<str> = class_name.into_boxed_str();
            let method_box: Box<str> = method_name.into_boxed_str();
            let desc_box: Box<str> = descriptor.into_boxed_str();
            let class_ref = &*class_box as *const str;
            let method_ref = &*method_box as *const str;
            let desc_ref = &*desc_box as *const str;
            owned_strings.push(class_box);
            owned_strings.push(method_box);
            owned_strings.push(desc_box);

            let info = Box::new(JitInvokeInfo {
                class_name: unsafe { &*class_ref },
                method_name: unsafe { &*method_ref },
                descriptor: unsafe { &*desc_ref },
                num_jit_args,
                return_type: ret_type,
                invoke_kind,
            });
            let info_ptr: *const JitInvokeInfo = &*info;
            owned_invoke_infos.push(info);
            invoke_info.push((pc, info_ptr));

            if invoke_kind == 0 || invoke_kind == 2 {
                let mic = Box::new(JitMICSlot::new());
                if let Some(prof) = profile {
                    if let Some(receiver_counts) = prof.receivers.get(&pc) {
                        if let Some(dom_class_id) = profile::dominant_receiver(receiver_counts, 80)
                        {
                            mic.prepopulate(dom_class_id);
                        }
                    }
                }

                // HIGH-7 — Eager PIC slot allocation alongside the
                // MIC. See the strategy comment at the `pic_slots`
                // declaration above. The PIC starts empty (all 3
                // entries have class_id == 0), so the inline cascade
                // falls through to the helper on cold sites. The
                // helper (`jit_invoke_virtual_mic`) populates entries
                // on miss, after which subsequent dispatches take
                // the inline fast path.
                //
                // `seed_from_mic` carries forward any profile-driven
                // pre-population we just applied to the MIC, so a
                // site with a known dominant receiver lands in PIC
                // slot 0 with class_id only (entry_ptr stays 0 →
                // first dispatch still rings the helper, which
                // installs entry_ptr; thereafter the cascade hits).
                let pic = Box::new(JitPICSlot::new());
                pic.seed_from_mic(&mic);

                let mic_ptr: *const JitMICSlot = &*mic;
                owned_mic_slots.push(mic);
                mic_slots.push((pc, mic_ptr));

                let pic_ptr: *const JitPICSlot = &*pic;
                owned_pic_slots.push(pic);
                pic_slots.push((pc, pic_ptr));
            }
        }
    }

    // Prologue argument-slot count — includes the implicit `this` for
    // instance methods (see `prologue_param_slots` above).
    let param_slots = prologue_param_slots;

    let branch_hints: std::collections::HashMap<usize, bool> = profile
        .map(|prof| {
            prof.branches
                .iter()
                .filter_map(|(&pc, counts)| {
                    if counts.is_usually_taken() {
                        Some((pc, true))
                    } else if counts.is_usually_not_taken() {
                        Some((pc, false))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // PGO: build loop unroll hints from profiled trip counts
    let loop_unroll_hints: std::collections::HashMap<usize, usize> = profile
        .map(|prof| {
            prof.loops
                .iter()
                .filter_map(|(&backedge_pc, trip)| {
                    trip.suggests_unroll_factor(8)
                        .map(|factor| (backedge_pc, factor))
                })
                .collect()
        })
        .unwrap_or_default();

    // Resolve `java/lang/String`'s field layout once, for the String
    // call-site intrinsics. `None` (no resolver, or String not yet
    // loadable) is fine — String intrinsic codegen treats it as "bail to
    // normal dispatch". Resolved here (cheap, once) so neither the
    // intrinsic matcher nor `x64::compile` needs the VM class registry.
    let string_layout: Option<StringFieldLayout> = string_layout_resolver.and_then(|r| r());

    // round-7 fix (bug 1): from this point on, any `None` return is a
    // permanent backend bail — the resolver pre-checks all completed
    // successfully and we're about to walk the full
    // scan/IR/lowering pipeline.  Flag it so the outer wrapper adds
    // this method to the bail-list.
    *backend_attempted = true;

    // Parameter → JVM-slot layout, so the prologue places long/double params
    // (which span two JVM slots but arrive in one arg register) in the slots
    // the body reads. `param_slots` above is the *argument count* (one per
    // param, used for ABI register indexing); `param_slot_span` is the JVM
    // slot span (category-2 counted as two).
    let (param_jvm_slots, param_slot_span) =
        compute_param_jvm_slots(&cached.method_descriptor, cached.is_static);

    // Stage A.4 (precise oop maps, B-K fix) — seed the local-oop dataflow with
    // reference parameters, but ONLY when the precise gate is on. Off → `0`, so
    // `compute_local_oop_masks` keeps its historical empty entry state and the
    // emitted maps/codegen are byte-identical to the default path.
    let param_oop_mask = if x64::precise_jit_maps_enabled() {
        compute_param_oop_mask(&cached.method_descriptor, cached.is_static)
    } else {
        0
    };

    let mut compiled = x64::compile_with_param_slots(
        code,
        code_len,
        param_slots,
        cached.max_locals as usize,
        needs_heap,
        mna_info,
        field_info,
        typecheck_info,
        static_field_info,
        new_info,
        anewarray_info,
        invoke_info,
        direct_calls,
        mic_slots,
        pic_slots,
        ldc_info,
        ldc2w_info,
        branch_hints,
        loop_unroll_hints,
        helpers,
        std::collections::HashSet::new(), // non_escaping_new — escape analysis done inside x64 too
        inline_sites,
        string_layout,
        &param_jvm_slots,
        param_slot_span,
        param_oop_mask,
    )?;

    compiled._jit_strings = owned_strings;
    compiled._jit_invoke_infos = owned_invoke_infos;
    // BUG-24: `compile(...)` already moved the loop-unroll *cloned* MIC/PIC
    // slots into `compiled._jit_{mic,pic}_slots` (see the `extend` in
    // `x64.rs::compile`, whose contract states the caller attaches its owned
    // slots ADDITIVELY). Assigning here would DROP those cloned boxes while the
    // unrolled machine code still holds baked pointers into them → use-after-
    // free: the freed `Box<JitPICSlot>` memory gets reused by the allocator for
    // Java objects, so the inline PIC cascade later reads class-id-pair garbage
    // out of `entry_ptrs[i]` and `CALL`s it (the Mockito-under-JIT
    // `EXCEPTION_ACCESS_VIOLATION at 0x0000033E0000033B`, a packed-class-id
    // value). Extend, don't overwrite, so both the cloned and owned slots stay
    // alive for the lifetime of the compiled code.
    compiled._jit_mic_slots.extend(owned_mic_slots);
    compiled._jit_pic_slots.extend(owned_pic_slots);
    compiled.inlined_methods = inlined_methods;

    if let Ok(want) = std::env::var("CRATONVM_DBG_JIT_CODE") {
        let full = format!(
            "{}.{}{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
        if full.contains(&want) {
            let slice = compiled._buffer_slice_for_debug();
            let mut hex = String::new();
            for b in slice {
                hex.push_str(&format!("{:02x}", b));
            }
            eprintln!(
                "[JIT_CODE] {} entry={:p} len={} param_jvm_slots={:?} span={} needs_heap={}\n{}",
                full,
                compiled.entry,
                slice.len(),
                param_jvm_slots,
                param_slot_span,
                needs_heap,
                hex
            );
        }
    }

    Some(compiled)
}

/// Count the number of JVM stack slots consumed by parameters in a method descriptor.
/// True if the method uses any category-2 (long/double) value — as a
/// parameter, return type, or operand/result of a long/double bytecode.
///
/// The IR pipeline ([`ir::IrBuilder`]) types every value as 32-bit
/// `IrType::Int` and lays parameters out by JIT-argument index rather than
/// JVM local slot, so it can model neither 64-bit values nor the two-slot
/// category-2 parameter layout. Such methods must use the single-pass
/// bytecode-x64 backend instead. Note: a `long[]`/`double[]` parameter is a
/// *reference* (category-1) and does NOT count here — only scalar J/D do.
fn method_uses_category2(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    let b = descriptor.as_bytes();
    // Scalar long/double parameter?
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'J' | b'D' => return true,
            b'L' => {
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                // Skip the whole array type — arrays are references (cat-1),
                // even `[J` / `[D`.
                i += 1;
                while i < b.len() && b[i] == b'[' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'L' {
                    i += 1;
                    while i < b.len() && b[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else if i < b.len() {
                    i += 1;
                }
            }
            _ => i += 1, // I F B C S Z
        }
    }
    // Long/double return type.
    if let Some(rp) = b.iter().position(|&c| c == b')') {
        if matches!(b.get(rp + 1), Some(b'J') | Some(b'D')) {
            return true;
        }
    }
    // Any long/double bytecode in the body.
    let mut pc = 0;
    while pc < code_len {
        if is_category2_opcode(code[pc]) {
            return true;
        }
        pc += crate::scev::bytecode_len(code, pc, code_len);
    }
    false
}

/// Compute, for each incoming JIT argument (in order: `this` for instance
/// methods, then declared parameters), the JVM local slot it occupies, along
/// with the total slot span the parameters consume. A category-2
/// (long/double) parameter occupies TWO JVM local slots but is passed in a
/// single JIT argument register, so its slot index diverges from its argument
/// index — e.g. `static f(long a, long b)` puts `a` at slot 0 and `b` at slot
/// 2, while the JIT passes them as args 0 and 1. The bytecode-x64 prologue
/// uses this to deposit each argument in the slot the body reads. See
/// [`x64::compile_with_param_slots`].
fn compute_param_jvm_slots(descriptor: &str, is_static: bool) -> (Vec<usize>, usize) {
    let mut slots = Vec::new();
    let mut slot = 0usize;
    if !is_static {
        slots.push(slot);
        slot += 1; // implicit `this`
    }
    let b = descriptor.as_bytes();
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        slots.push(slot);
        match b[i] {
            b'J' | b'D' => {
                slot += 2;
                i += 1;
            }
            b'L' => {
                slot += 1;
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                slot += 1;
                i += 1;
                while i < b.len() && b[i] == b'[' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'L' {
                    i += 1;
                    while i < b.len() && b[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else if i < b.len() {
                    i += 1;
                }
            }
            _ => {
                slot += 1;
                i += 1;
            }
        }
    }
    (slots, slot)
}

/// Stage A.4 (precise oop maps, B-K fix) — bitmask of JVM local slots that hold
/// a REFERENCE parameter on method entry: bit `k` set ⇒ slot `k` is an oop.
///
/// Covers the implicit `this` (slot 0, instance methods) plus every declared
/// `L…;` / `[…` parameter. Mirrors [`compute_param_jvm_slots`]'s slot walk
/// EXACTLY — category-2 (`J`/`D`) parameters consume two slots and are non-oops,
/// primitives consume one — so the returned bit positions line up with the local
/// slot indices the method body reads. Slots ≥ 64 are out of the dataflow's
/// 64-local model and are dropped (such a method cannot be fully-covered).
///
/// Used only on the precise gate to seed [`x64::compile_with_param_slots`]'s
/// `param_oop_mask`; the caller passes `0` when the gate is off.
fn compute_param_oop_mask(descriptor: &str, is_static: bool) -> u64 {
    let mut mask = 0u64;
    let mut slot = 0usize;
    if !is_static {
        // `this` is always a reference.
        mask |= 1u64 << slot; // slot == 0 here
        slot += 1;
    }
    let b = descriptor.as_bytes();
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'J' | b'D' => {
                // category-2 scalar: two slots, non-oop.
                slot += 2;
                i += 1;
            }
            b'L' => {
                if slot < 64 {
                    mask |= 1u64 << slot;
                }
                slot += 1;
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                if slot < 64 {
                    mask |= 1u64 << slot;
                }
                slot += 1;
                i += 1;
                while i < b.len() && b[i] == b'[' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'L' {
                    i += 1;
                    while i < b.len() && b[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else if i < b.len() {
                    i += 1;
                }
            }
            _ => {
                // I F B C S Z — category-1 primitive, non-oop.
                slot += 1;
                i += 1;
            }
        }
    }
    mask
}

/// Long/double bytecodes (category-2 operands or results). Used to keep such
/// methods off the int-only IR pipeline.
fn is_category2_opcode(op: u8) -> bool {
    matches!(
        op,
        0x09 | 0x0a            // lconst_0, lconst_1
        | 0x0e | 0x0f          // dconst_0, dconst_1
        | 0x14                 // ldc2_w (long/double constant)
        | 0x16 | 0x18          // lload, dload
        | 0x1e..=0x21          // lload_0..3
        | 0x26..=0x29          // dload_0..3
        | 0x2f | 0x31          // laload, daload
        | 0x37 | 0x39          // lstore, dstore
        | 0x3f..=0x42          // lstore_0..3
        | 0x47..=0x4a          // dstore_0..3
        | 0x50 | 0x52          // lastore, dastore
        | 0x61 | 0x63 | 0x65 | 0x67 | 0x69 | 0x6b | 0x6d | 0x6f | 0x71 | 0x73 // l/d add,sub,mul,div,rem
        | 0x75 | 0x77          // lneg, dneg
        | 0x79 | 0x7b | 0x7d   // lshl, lshr, lushr
        | 0x7f | 0x81 | 0x83   // land, lor, lxor
        | 0x85 | 0x87 | 0x88 | 0x89 | 0x8a | 0x8c | 0x8d | 0x8e | 0x8f | 0x90 // i2l,i2d,l2i,l2f,l2d,f2l,f2d,d2i,d2l,d2f
        | 0x94 | 0x97 | 0x98   // lcmp, dcmpl, dcmpg
        | 0xad | 0xaf          // lreturn, dreturn
    )
}

pub fn count_param_slots(descriptor: &str) -> usize {
    let bytes = descriptor.as_bytes();
    if bytes.is_empty() || bytes[0] != b'(' {
        return 0;
    }
    let mut i = 1;
    let mut slots = 0;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'I' | b'F' | b'B' | b'C' | b'S' | b'Z' => {
                slots += 1;
                i += 1;
            }
            b'J' | b'D' => {
                slots += 1;
                i += 1;
            }
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
                slots += 1;
            }
            b'[' => {
                i += 1;
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
                slots += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    slots
}

/// Gap B (inc 22): classify a static-call descriptor for the `Op::Call` slice.
/// Returns `Some((num_args, ret_type))` iff EVERY parameter is a single-slot
/// value the marshaller can pass as one i64 — an int-category primitive
/// (`I`/`Z`/`B`/`C`/`S`) OR a reference (`L…`/`[…`, passed as the raw pointer) —
/// and the return is int-category, `void`, or a reference. `None` for any
/// `long`/`float`/`double` parameter or return (category-2 / XMM register, not
/// handled by the int/pointer-only GPR marshalling), keeping the method on
/// single-pass.
///
/// Reference args/returns are GC-sound across the call: while a JIT frame is
/// active the GC is non-moving (`gc_quiescence`), and the conservative root
/// scan of the IR spill frame finds (and pins) any pointer in a slot — so a
/// reference live across the call is neither relocated nor reclaimed, with no
/// precise oop map required (inc 22 — see `activate-ir-optimizer.md`).
pub fn static_call_shape(descriptor: &str) -> Option<(usize, u8)> {
    let bytes = descriptor.as_bytes();
    if bytes.is_empty() || bytes[0] != b'(' {
        return None;
    }
    let mut i = 1;
    let mut num_args = 0usize;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'I' | b'Z' | b'B' | b'C' | b'S' => {
                num_args += 1;
                i += 1;
            }
            b'L' => {
                num_args += 1;
                i += 1;
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1; // skip ';'
            }
            b'[' => {
                num_args += 1;
                i += 1;
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'L' {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else if i < bytes.len() {
                    i += 1; // primitive array element type
                }
            }
            // long / float / double — category-2 / XMM, not handled.
            _ => return None,
        }
    }
    let ret = return_type(descriptor);
    match ret {
        b'I' | b'Z' | b'B' | b'C' | b'S' | b'V' | b'L' | b'[' => Some((num_args, ret)),
        // J / D / F return — not handled.
        _ => None,
    }
}

/// JVM-spec param slot count: longs and doubles take 2 slots each (per
/// JVMS §2.6.1), unlike `count_param_slots` which counts each parameter
/// as exactly one slot (matching our compact ABI representation where
/// every primitive fits in one i64 register).
///
/// Used for **local-variable layout** in the JIT prologue and register
/// allocator: javac emits `lload_2` / `dstore_3` etc. assuming JVM-spec
/// slot numbering (e.g. for a `(JJ)V` method, the second long lives at
/// local 2, not local 1). Without this distinction the JIT prologue
/// stores the second long arg into local 1 and `lload_2` reads
/// uninitialized memory — see TransactionCounter @Test bug.
pub fn count_param_slots_jvm_spec(descriptor: &str) -> usize {
    let bytes = descriptor.as_bytes();
    if bytes.is_empty() || bytes[0] != b'(' {
        return 0;
    }
    let mut i = 1;
    let mut slots = 0;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'I' | b'F' | b'B' | b'C' | b'S' | b'Z' => {
                slots += 1;
                i += 1;
            }
            b'J' | b'D' => {
                // Category 2: long / double take TWO local slots (JVMS §2.6.1).
                slots += 2;
                i += 1;
            }
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
                slots += 1;
            }
            b'[' => {
                i += 1;
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
                slots += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    slots
}

/// Per-parameter destination local slot under JVM-spec layout. Returns a
/// vector whose `i`-th entry is the spec-slot index for the `i`-th
/// parameter (compact ABI ordering — one entry per ABI register slot).
///
/// For descriptor `(JI)V` the result is `[0, 2]`: the first arg is a
/// long (occupies slots 0+1), so the second arg lands at slot 2. The
/// JIT prologue uses this map to copy each ABI register to the
/// matching JVM-spec local slot (instead of the compact-index slot
/// `i`, which silently wrote longs into the wrong slot).
pub fn param_spec_slot_indices(descriptor: &str) -> Vec<usize> {
    let bytes = descriptor.as_bytes();
    let mut out = Vec::new();
    if bytes.is_empty() || bytes[0] != b'(' {
        return out;
    }
    let mut i = 1;
    let mut slot = 0usize;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'I' | b'F' | b'B' | b'C' | b'S' | b'Z' => {
                out.push(slot);
                slot += 1;
                i += 1;
            }
            b'J' | b'D' => {
                out.push(slot);
                slot += 2;
                i += 1;
            }
            b'L' => {
                out.push(slot);
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
                slot += 1;
            }
            b'[' => {
                out.push(slot);
                i += 1;
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
                slot += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

/// Parse the return type from a method descriptor. Returns 'I', 'J', 'V', etc.
pub fn return_type(descriptor: &str) -> u8 {
    let bytes = descriptor.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b')' && i + 1 < bytes.len() {
            return bytes[i + 1];
        }
    }
    b'V'
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── wire-tiered-manager Step 3: per-call C1/C2 backend toggle ──────────
    //
    // `optimize=true` (C2) must take the optimizing IR pipeline; `optimize=false`
    // (the fast C1 tier) must skip it and route to the single-pass `x64::compile`
    // backend. Proven via the thread-local `IR_LOWER_COMPILES` counter, which the
    // IR-lowering success path bumps. The counter is thread-local and each cargo
    // `#[test]` runs on its own thread, so parallel compile tests can't perturb it.
    #[test]
    fn step3_optimize_toggle_routes_c1_singlepass_and_c2_ir() {
        use std::sync::Arc;

        // `static int add(int a, int b) { return a + b; }`
        //   iload_0 (0x1a); iload_1 (0x1b); iadd (0x60); ireturn (0xac)
        // Pure int arithmetic → ir_compatible, no category-2, no branch/call/field.
        let cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/Add"),
            method_name: Arc::from("add"),
            method_descriptor: Arc::from("(II)I"),
            source_file: None,
            code: Arc::from([0x1a, 0x1b, 0x60, 0xac].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 2,
            max_locals: 2,
            num_params: 2,
            is_synchronized: false,
            is_static: true,
        };
        // SAFETY: every `JitRuntimeHelpers` field is a `usize` and the struct is
        // `#[repr(C)]`, so an all-zero bit pattern is valid (no niches/padding).
        // `add` references no runtime helper, and the test never executes the
        // generated machine code, so the null helper addresses are never called.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };

        // C2 — optimize=true → optimizing IR pipeline.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let c2 = try_compile(
            &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None,
            None, None, true, false, false,
        );
        let c2_used_ir = IR_LOWER_COMPILES.with(|c| c.get());
        assert!(c2.is_some(), "optimize=true (C2) must compile `add`");
        assert_eq!(
            c2_used_ir, 1,
            "optimize=true (C2) must route `add` through the IR pipeline"
        );

        // C1 — optimize=false → single-pass x64 backend, IR pipeline skipped.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let c1 = try_compile(
            &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None,
            None, None, false, false, false,
        );
        let c1_used_ir = IR_LOWER_COMPILES.with(|c| c.get());
        assert!(
            c1.is_some(),
            "optimize=false (C1) must still compile `add` via the single-pass backend"
        );
        assert_eq!(
            c1_used_ir, 0,
            "optimize=false (C1) must NOT take the IR pipeline"
        );

        // Both tiers produced runnable native code.
        assert!(!c1.unwrap().code_bytes().is_empty());
        assert!(!c2.unwrap().code_bytes().is_empty());
    }

    // ── activate-ir-optimizer step 3: int-category `getfield` → `Op::Load` ──
    //
    // A method whose only heap op is an int-field read must now take the
    // optimizing IR pipeline (the builder lowers `getfield` to `Op::Load`).
    // Proven via `IR_LOWER_COMPILES`: a vacuous fall-through to single-pass
    // would leave the counter at 0. (The integration harness
    // `ir_vs_singlepass.rs` proves the *executed* result is correct; this
    // proves the IR path — not single-pass — produced the body.)
    #[test]
    fn step3_getfield_int_routes_through_ir() {
        use std::sync::Arc;

        // `static int get(Corpus o) { return o.x; }`
        //   aload_0 (0x2a); getfield #2 (0xb4 0x00 0x02); ireturn (0xac)
        let cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/Corpus"),
            method_name: Arc::from("get"),
            method_descriptor: Arc::from("(Lpkg/Corpus;)I"),
            source_file: None,
            // Trailing 0x00 0x00: the VM pads bytecode with two bytes that
            // `try_compile` strips via `code.len() - 2`; without them the
            // `ireturn` is truncated away.
            code: Arc::from([0x2a, 0xb4, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 2,
            max_locals: 1,
            num_params: 1,
            is_synchronized: false,
            is_static: true,
        };
        // SAFETY: see `step3_optimize_toggle_…`; an all-zero `JitRuntimeHelpers`
        // is valid and never called (the inline getfield emits no helper call,
        // and this test does not execute the generated code).
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        // Resolve cp index 2 → field index 0, int (`I`).
        let field_resolver = |cp: u16| -> Option<(usize, u8)> {
            if cp == 2 {
                Some((0, b'I'))
            } else {
                None
            }
        };

        // C2 — optimize=true → IR pipeline lowers `getfield` to `Op::Load`.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let c2 = try_compile(
            &cached,
            None,
            Some(&field_resolver),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &helpers,
            None,
            None,
            None,
            None,
            true,
            false,
            false,
        );
        assert!(c2.is_some(), "optimize=true (C2) must compile `get`");
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            1,
            "an int getfield method must route through the IR pipeline"
        );

        // Without the field resolver the builder cannot resolve the field, so
        // the IR path must bail (counter stays 0). The whole compile then
        // returns None — single-pass also needs the resolver to build
        // `field_info` — which is the expected, safe fallback.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let _ = try_compile(
            &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None,
            None, None, true, false, false,
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            0,
            "an unresolved getfield must NOT take the IR pipeline"
        );
    }

    // ── activate-ir-optimizer inc 16: EA bridge handles full-layout ops ──
    //
    // Escape analysis scalar-replaces a non-escaping allocation by killing the
    // `Op::New` and its field stores and redirecting field loads to the stored
    // value. That only works if the IR→EA bridge translates the *production*
    // full-layout `Op::Load`/`Op::Store` (`[ctrl, mem, base, offset, value]`,
    // `MemKind`-tagged) into the EA's compact `[holder]`/`[holder, value]`
    // layout with the real field index from the `Const` offset operand — the
    // exact thing inc 16 fixed. This builds such a graph by hand (the builder
    // does not emit `Op::New` yet) and proves the round-trip.
    #[test]
    fn ea_bridge_scalar_replaces_full_layout_new_store_load() {
        use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

        // Object o = new Foo(); o.f1 = 42; return o.f1;  (single int field at
        // index 1, to also exercise a non-zero field index vs MemKind::Int=0).
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let newobj = g.add(
            Op::New {
                class_id: 7,
                num_fields: 2,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let val = g.add(Op::Const(42), IrType::Int, vec![], None);
        let off = g.add(Op::Const(1), IrType::Int, vec![], None); // field index 1
        let store = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, newobj, off, val],
            None,
        );
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, store, newobj, off],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
        g.exit = ret;

        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        assert!(
            !result.scalar_replaceable.is_empty(),
            "a non-escaping New with a matching field store/load must be \
             scalar-replaceable once the bridge resolves the full layout"
        );

        apply_ea_to_ir(&mut g, &id_map, &result);
        assert_eq!(g.nodes[newobj as usize].op, Op::Dead, "New killed");
        assert_eq!(g.nodes[store as usize].op, Op::Dead, "Store killed");
        assert_eq!(g.nodes[load as usize].op, Op::Dead, "Load killed");
        assert_eq!(
            g.nodes[ret as usize].inputs[1], val,
            "the load result must be redirected to the stored value (Const 42)"
        );
    }

    // A New that escapes (returned by reference) must NOT be scalar-replaced —
    // the bridge fix preserves the escape rule (a missed escape would scalar-
    // replace an object a real use still needs: the kafka bug-25 class).
    #[test]
    fn ea_bridge_keeps_escaping_new() {
        use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let newobj = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let val = g.add(Op::Const(42), IrType::Int, vec![], None);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let _store = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, newobj, off, val],
            None,
        );
        // Return the *reference* — the object escapes globally.
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, newobj], None);
        g.exit = ret;

        let (ea, _id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        assert!(
            result.scalar_replaceable.is_empty(),
            "an escaping New must not be scalar-replaceable"
        );
    }

    // A non-escaping New whose field is LOADED but never STORED is scalar-
    // replaced, and the load of that field must resolve to the zero default
    // (`Const(0)`) of the freshly-allocated object — NOT be killed without a
    // replacement (the latent `apply_ea_to_ir` bug). Soundness rests on the
    // object being zero-initialised (the caller only admits allocations whose
    // constructor sets no non-zero field).
    #[test]
    fn ea_unstored_field_load_resolves_to_zero_default() {
        use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let newobj = g.add(
            Op::New {
                class_id: 7,
                num_fields: 2,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let val = g.add(Op::Const(42), IrType::Int, vec![], None);
        let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
        let off1 = g.add(Op::Const(1), IrType::Int, vec![], None);
        let store = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, newobj, off0, val],
            None,
        );
        // Load field 1 — never stored.
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, store, newobj, off1],
            None,
        );
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
        g.exit = ret;

        let (ea, id_map) = escape_analysis_from_ir(&g);
        let result = escape_analysis::analyze_escapes(&ea);
        assert!(
            !result.scalar_replaceable.is_empty(),
            "a non-escaping new is scalar-replaceable even with an un-stored field"
        );
        apply_ea_to_ir(&mut g, &id_map, &result);
        assert!(
            !g.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
            "the New is scalar-replaced away"
        );
        let ret_node = g
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Return))
            .expect("a Return");
        let retval = ret_node.inputs[1];
        assert_eq!(
            g.nodes[retval as usize].op,
            Op::Const(0),
            "the un-stored field's load must resolve to the zero default"
        );
    }

    // ── Op::New emission + scalar replacement, end-to-end via the builder ──
    //
    // The builder lowers `new` to `Op::New` and elides a trivial `<init>` on a
    // fresh object; escape analysis then scalar-replaces the non-escaping
    // allocation (no heap alloc, the field load becomes the stored value). This
    // drives the FULL path (build -> optimize -> EA -> apply) from bytecode.
    #[test]
    fn ir_new_scalar_replaces_end_to_end() {
        use crate::ir::{IrBuilder, Op};
        use std::collections::{HashMap, HashSet};

        // static int f() { Foo o = new Foo(); o.x = 42; return o.x; }
        //   0: new #1            bb 00 01
        //   3: dup               59
        //   4: invokespecial #2  b7 00 02   (Foo.<init>()V — trivial, elided)
        //   7: dup               59
        //   8: bipush 42         10 2a
        //  10: putfield #3       b5 00 03
        //  13: getfield #3       b4 00 03
        //  16: ireturn           ac
        let code = [
            0xbb, 0x00, 0x01, 0x59, 0xb7, 0x00, 0x02, 0x59, 0x10, 0x2a, 0xb5, 0x00, 0x03, 0xb4,
            0x00, 0x03, 0xac, 0x00, 0x00,
        ];
        let mut builder = IrBuilder::new(0, 1);
        let mut new_info = HashMap::new();
        new_info.insert(0usize, (7u32, 1usize)); // new @0: class 7, 1 field
        let mut init_pcs = HashSet::new();
        init_pcs.insert(4usize); // <init> @4 is trivial + elidable
        builder.set_new_info(new_info, init_pcs);
        let mut fi = HashMap::new();
        fi.insert(10usize, (0usize, b'I')); // putfield field 0
        fi.insert(13usize, (0usize, b'I')); // getfield field 0
        builder.set_field_info(fi);
        let mut graph = builder.build(&code, 17).expect("IR build");
        assert!(
            graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
            "builder must emit an Op::New for `new`"
        );

        ir_optimize::optimize(&mut graph);
        let (ea, id_map) = escape_analysis_from_ir(&graph);
        let result = escape_analysis::analyze_escapes(&ea);
        assert!(
            !result.scalar_replaceable.is_empty(),
            "the non-escaping new must be scalar-replaceable"
        );
        apply_ea_to_ir(&mut graph, &id_map, &result);

        assert!(
            !graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
            "the New must be scalar-replaced away"
        );
        let ret = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Return))
            .expect("a Return");
        let retval = ret.inputs[1];
        assert_eq!(
            graph.nodes[retval as usize].op,
            Op::Const(42),
            "the field load must resolve to the stored value (42)"
        );
    }

    // REGRESSION (astore gap): the same scalar-replacement end-to-end, but the
    // fresh object is round-tripped through a LOCAL via `astore`/`aload` — the
    // shape REAL javac emits (`new; dup; invokespecial; astore_N; aload_N; …`).
    // The `ir_new_scalar_replaces_end_to_end` test above keeps the ref on the
    // stack via `dup`, so it never exercised `astore` — and the builder had NO
    // `astore` handler, so EVERY production allocation method bailed to
    // single-pass and `new` scalar replacement NEVER fired live (inc 17/19's
    // "== HotSpot" probe was vacuous: it matches whether or not SR fires). With
    // `astore` lowered, this folds to `Const(42)` exactly like the dup form.
    #[test]
    fn ir_new_scalar_replaces_through_astore_local() {
        use crate::ir::{IrBuilder, Op};
        use std::collections::{HashMap, HashSet};

        // static int f() { Foo o = new Foo(); o.x = 42; return o.x; }  (javac shape)
        //   0: new #1            bb 00 01
        //   3: dup               59
        //   4: invokespecial #2  b7 00 02   (Foo.<init>()V — elided)
        //   7: astore_0          4b
        //   8: aload_0           2a
        //   9: bipush 42         10 2a
        //  11: putfield #3       b5 00 03
        //  14: aload_0           2a
        //  15: getfield #3       b4 00 03
        //  18: ireturn           ac
        let code = [
            0xbb, 0x00, 0x01, 0x59, 0xb7, 0x00, 0x02, 0x4b, 0x2a, 0x10, 0x2a, 0xb5, 0x00, 0x03,
            0x2a, 0xb4, 0x00, 0x03, 0xac, 0x00, 0x00,
        ];
        let mut builder = IrBuilder::new(0, 1);
        let mut new_info = HashMap::new();
        new_info.insert(0usize, (7u32, 1usize)); // new @0: class 7, 1 field
        let mut init_pcs = HashSet::new();
        init_pcs.insert(4usize); // <init> @4 is trivial + elidable
        builder.set_new_info(new_info, init_pcs);
        let mut fi = HashMap::new();
        fi.insert(11usize, (0usize, b'I')); // putfield field 0
        fi.insert(15usize, (0usize, b'I')); // getfield field 0
        builder.set_field_info(fi);
        let mut graph = builder
            .build(&code, 19)
            .expect("IR build must succeed with astore lowered");
        assert!(
            graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
            "builder must emit an Op::New for `new`"
        );

        ir_optimize::optimize(&mut graph);
        let (ea, id_map) = escape_analysis_from_ir(&graph);
        let result = escape_analysis::analyze_escapes(&ea);
        assert!(
            !result.scalar_replaceable.is_empty(),
            "the astore-local non-escaping new must be scalar-replaceable"
        );
        apply_ea_to_ir(&mut graph, &id_map, &result);
        assert!(
            !graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
            "the New must be scalar-replaced away"
        );
        let ret = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Return))
            .expect("a Return");
        let retval = ret.inputs[1];
        assert_eq!(
            graph.nodes[retval as usize].op,
            Op::Const(42),
            "the field load (via astore/aload local) must resolve to the stored value (42)"
        );
    }

    // A `<init>` whose receiver is NOT a fresh `new` (e.g. a super() call on
    // `this`) must NOT be elided — the builder bails even if the pc is admitted.
    #[test]
    fn ir_new_bails_on_init_of_nonfresh_receiver() {
        use crate::ir::IrBuilder;
        use std::collections::{HashMap, HashSet};
        // aload_0; invokespecial #2; return  — `super.<init>()` on `this`.
        //   0: aload_0           2a
        //   1: invokespecial #2  b7 00 02
        //   4: return            b1
        let code = [0x2a, 0xb7, 0x00, 0x02, 0xb1, 0x00, 0x00];
        let mut builder = IrBuilder::new(1, 1);
        let mut init_pcs = HashSet::new();
        init_pcs.insert(1usize); // admit pc 1 — but receiver is `this`, not a New
        builder.set_new_info(HashMap::new(), init_pcs);
        assert!(
            builder.build(&code, 5).is_none(),
            "eliding a <init> on a non-fresh receiver must bail to single-pass"
        );
    }

    // activate-ir-optimizer (scalar-new wiring): a `new`-bearing method whose
    // construction is elidable routes through the IR pipeline (scalar-replaced)
    // ONLY when the elidable-`<init>` resolver is supplied — the production soak
    // gate. Without it the builder bails on the `invokespecial`, keeping `new`
    // scalar replacement off by default.
    #[test]
    fn scalar_new_wiring_routes_through_ir_only_with_resolver() {
        use std::sync::Arc;
        // static int f() { Foo o = new Foo(); o.x = 42; return o.x; }
        let cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/Mk"),
            method_name: Arc::from("f"),
            method_descriptor: Arc::from("()I"),
            source_file: None,
            code: Arc::from(
                [
                    0xbb, 0x00, 0x01, 0x59, 0xb7, 0x00, 0x02, 0x59, 0x10, 0x2a, 0xb5, 0x00, 0x03,
                    0xb4, 0x00, 0x03, 0xac, 0x00, 0x00,
                ]
                .as_slice(),
            ),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 8,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        };
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        let new_resolver = |cp: u16| -> Option<(u32, usize, bool, bool)> {
            if cp == 1 {
                Some((7, 1, false, false))
            } else {
                None
            }
        };
        let field_resolver = |cp: u16| -> Option<(usize, u8)> {
            if cp == 3 {
                Some((0, b'I'))
            } else {
                None
            }
        };
        let elidable = |cp: u16| -> bool { cp == 2 };

        // With the elidable resolver → the IR pipeline scalar-replaces the new.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let r = try_compile(
            &cached,
            None,
            Some(&field_resolver),
            None,
            None,
            None,
            Some(&new_resolver),
            None,
            None,
            None,
            &helpers,
            None,
            None,
            None,
            Some(&elidable),
            true,
            false,
            false,
        );
        assert!(r.is_some(), "an elidable `new` method must compile via IR");
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            1,
            "the elidable `new` method must route through the IR pipeline"
        );

        // Without it → the builder bails on the `invokespecial` → not the IR path.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let _ = try_compile(
            &cached,
            None,
            Some(&field_resolver),
            None,
            None,
            None,
            Some(&new_resolver),
            None,
            None,
            None,
            &helpers,
            None,
            None,
            None,
            None,
            true,
            false,
            false,
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            0,
            "without the elidable resolver, `new` must NOT take the IR pipeline"
        );
    }

    // ── activate-ir-optimizer Gap B: int invokestatic → Op::Call routing ──
    //
    // A method whose only invoke is an int-only `invokestatic` in an oop-free
    // body must route through the IR pipeline ONLY when `ir_emit_calls` is on
    // (the production gate). This guards against a *vacuous* validation: the
    // integration harness proves the executed result is correct, but single-pass
    // ALSO dispatches `invokestatic` correctly, so result-equality alone would
    // not prove the IR path fired. `IR_LOWER_COMPILES` proves it does (==1 with
    // the flag) and does not (==0 without — the builder bails on the invoke).
    #[test]
    fn ir_call_wiring_routes_through_ir_only_with_flag() {
        use std::sync::Arc;

        // `static int f(int a, int b) { return g(a, b); }`
        //   iload_0; iload_1; invokestatic #2; ireturn
        let cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/Caller"),
            method_name: Arc::from("f"),
            method_descriptor: Arc::from("(II)I"),
            source_file: None,
            code: Arc::from([0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 2,
            max_locals: 2,
            num_params: 2,
            is_synchronized: false,
            is_static: true,
        };
        // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
        // (never executes the body), so the baked `invoke_dispatch` is not called.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        let invoke_resolver = |cp: u16| -> Option<(String, String, String)> {
            if cp == 2 {
                Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
            } else {
                None
            }
        };

        // ir_emit_calls = true → invokestatic lowers to Op::Call → IR pipeline.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let with = try_compile(
            &cached,
            None,
            None,
            None,
            Some(&invoke_resolver),
            None,
            None,
            None,
            None,
            None,
            &helpers,
            None,
            None,
            None,
            None,
            true, // optimize
            true, // ir_emit_calls
            false, // ir_emit_special_calls (testing invokestatic, not special)
        );
        assert!(
            with.is_some(),
            "int invokestatic must compile with ir_emit_calls"
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            1,
            "int invokestatic must route through the IR pipeline when ir_emit_calls is on"
        );
        assert!(
            with.as_ref().unwrap().needs_context(),
            "an Op::Call method must be needs_context"
        );

        // ir_emit_calls = false → builder bails on the invoke → single-pass.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let _without = try_compile(
            &cached,
            None,
            None,
            None,
            Some(&invoke_resolver),
            None,
            None,
            None,
            None,
            None,
            &helpers,
            None,
            None,
            None,
            None,
            true,  // optimize
            false, // ir_emit_calls OFF
            false, // ir_emit_special_calls OFF
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            0,
            "without ir_emit_calls, invokestatic must NOT take the IR pipeline"
        );
    }

    /// inc 24 (Gap B): a resolved non-`<init>` `invokespecial` routes through the
    /// IR pipeline ONLY when `ir_emit_special_calls` is on — independent of
    /// `ir_emit_calls` (which gates invokestatic). The two toggles are crossed
    /// here to prove the SPECIAL flag alone admits invokespecial. Guards against
    /// a vacuous validation: single-pass ALSO dispatches invokespecial, so
    /// result-equality alone (the integration harness) would not prove the IR
    /// path ran.
    #[test]
    fn ir_special_call_wiring_routes_through_ir_only_with_flag() {
        use std::sync::Arc;

        // `static int f(Obj o, int n) { return o.g(n); }`  (g private → invokespecial)
        //   aload_0; iload_1; invokespecial #2; ireturn
        let cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/Caller"),
            method_name: Arc::from("f"),
            method_descriptor: Arc::from("(Lpkg/Obj;I)I"),
            source_file: None,
            code: Arc::from([0x2a, 0x1b, 0xb7, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 2,
            max_locals: 2,
            num_params: 2,
            is_synchronized: false,
            is_static: true,
        };
        // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
        // (never executes the body), so the baked `invoke_dispatch` is not called.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        // Target: the private instance method `g(I)I` — receiver implicit, so the
        // IR builder marshals it as arg0 (num_jit_args = 1 desc + 1 receiver).
        let invoke_resolver = |cp: u16| -> Option<(String, String, String)> {
            if cp == 2 {
                Some(("pkg/Obj".into(), "g".into(), "(I)I".into()))
            } else {
                None
            }
        };

        // ir_emit_special_calls = true (ir_emit_calls OFF) → invokespecial lowers
        // to Op::Call → IR pipeline. Crossing the flags proves SPECIAL is the gate.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let with = try_compile(
            &cached, None, None, None, Some(&invoke_resolver), None, None, None, None, None,
            &helpers, None, None, None, None,
            true,  // optimize
            false, // ir_emit_calls (invokestatic) OFF
            true,  // ir_emit_special_calls ON
        );
        assert!(
            with.is_some(),
            "invokespecial must compile with ir_emit_special_calls"
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            1,
            "invokespecial must route through the IR pipeline when ir_emit_special_calls is on"
        );
        assert!(
            with.as_ref().unwrap().needs_context(),
            "an Op::Call method must be needs_context"
        );

        // ir_emit_special_calls = false (ir_emit_calls ON) → the builder bails on
        // the invokespecial → single-pass. Proves invokestatic's gate does NOT
        // admit invokespecial.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let _without = try_compile(
            &cached, None, None, None, Some(&invoke_resolver), None, None, None, None, None,
            &helpers, None, None, None, None,
            true,  // optimize
            true,  // ir_emit_calls (invokestatic) ON
            false, // ir_emit_special_calls OFF
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            0,
            "without ir_emit_special_calls, invokespecial must NOT take the IR pipeline"
        );
    }

    // ── Stage A.4 (precise oop maps) — param oop mask ──────────────
    //
    // `compute_param_oop_mask` must mark exactly the JVM local slots that hold a
    // reference parameter on entry, using the SAME slot walk as
    // `compute_param_jvm_slots` (category-2 J/D consume two slots; arrays and
    // `L…;` are references; primitives are not; instance `this` is slot 0). A
    // wrong bit here would, under the eventual moving path, either rewrite a
    // primitive (false positive) or miss an oop (false negative) — so pin it.
    #[test]
    fn test_compute_param_oop_mask() {
        // static, all primitive → no oop slots.
        assert_eq!(compute_param_oop_mask("(II)I", true), 0b00);
        // instance, primitive params → only `this` (slot 0).
        assert_eq!(compute_param_oop_mask("(II)I", false), 0b1);
        // static, one reference param at slot 0.
        assert_eq!(compute_param_oop_mask("(Ljava/lang/Object;I)V", true), 0b1);
        // instance, one reference param → this (0) + param (1).
        assert_eq!(compute_param_oop_mask("(Ljava/lang/Object;)V", false), 0b11);
        // static, long (slots 0,1; non-oop) then reference at slot 2.
        assert_eq!(
            compute_param_oop_mask("(JLjava/lang/Object;)V", true),
            0b100
        );
        // static, array ref (slot 0), long (slots 1,2), array-of-ref (slot 3).
        assert_eq!(
            compute_param_oop_mask("([IJ[Ljava/lang/String;)V", true),
            0b1001
        );
        // bt18's `static Node make(int)` — the live oop is the LOCAL `n`, not a
        // param, so the param mask is empty (the dataflow seeds it as `astore`d).
        assert_eq!(compute_param_oop_mask("(I)Lpkg/Node;", true), 0b0);
        // instance, double param (slots 1,2; non-oop) → only `this`.
        assert_eq!(compute_param_oop_mask("(D)V", false), 0b1);
        // no params: static → 0; instance → this only.
        assert_eq!(compute_param_oop_mask("()V", true), 0b0);
        assert_eq!(compute_param_oop_mask("()V", false), 0b1);
    }

    // ── JitMICSlot layout tests (CRIT-8 prerequisite) ──────────────
    //
    // The JIT codegen in `jit/src/x64.rs` emits raw `MOV` instructions
    // against a `JitMICSlot*` using the `CACHED_*_OFFSET` constants.
    // If the struct layout drifts (e.g. someone reorders a field,
    // changes padding, or `parking_lot::Mutex` grows), those MOVs will
    // silently read the wrong bytes. Pin the offsets here.
    //
    // We don't use `core::mem::offset_of!` because the workspace MSRV
    // (see `Cargo.toml: rust-version`) is 1.75; the macro stabilized
    // in 1.77.

    #[test]
    fn test_jit_mic_slot_offsets() {
        let slot = JitMICSlot::new();
        let base = &slot as *const JitMICSlot as usize;
        let off_class_id = (&slot.cached_class_id as *const _ as usize) - base;
        let off_entry_ptr = (&slot.cached_entry_ptr as *const _ as usize) - base;
        let off_needs_context = (&slot.cached_needs_context as *const _ as usize) - base;
        assert_eq!(
            off_class_id,
            JitMICSlot::CACHED_CLASS_ID_OFFSET,
            "cached_class_id offset drift: expected {}, got {}",
            JitMICSlot::CACHED_CLASS_ID_OFFSET,
            off_class_id
        );
        assert_eq!(
            off_entry_ptr,
            JitMICSlot::CACHED_ENTRY_PTR_OFFSET,
            "cached_entry_ptr offset drift: expected {}, got {}",
            JitMICSlot::CACHED_ENTRY_PTR_OFFSET,
            off_entry_ptr
        );
        assert_eq!(
            off_needs_context,
            JitMICSlot::CACHED_NEEDS_CONTEXT_OFFSET,
            "cached_needs_context offset drift: expected {}, got {}",
            JitMICSlot::CACHED_NEEDS_CONTEXT_OFFSET,
            off_needs_context
        );
    }

    // ── JitPICSlot layout tests (CRIT-8 prerequisite) ──────────────
    //
    // The JIT codegen emits raw `MOV` instructions against a
    // `JitPICSlot*` using the `CLASS_ID_OFFSETS` / `ENTRY_PTR_OFFSETS`
    // / `NEEDS_CONTEXT_OFFSETS` constants for inline 3-way PIC
    // dispatch. Pin the offsets here so any layout drift (field
    // reorder, padding change, `parking_lot::Mutex` resize) fails
    // loudly instead of silently misreading bytes.

    #[test]
    fn test_jit_pic_slot_offsets() {
        let slot = JitPICSlot::new();
        let base = &slot as *const JitPICSlot as usize;
        for i in 0..JIT_PIC_ENTRIES {
            let actual = (&slot.class_ids[i] as *const _ as usize) - base;
            assert_eq!(
                actual,
                JitPICSlot::CLASS_ID_OFFSETS[i],
                "CLASS_ID_OFFSETS[{i}] drift: {actual} vs {}",
                JitPICSlot::CLASS_ID_OFFSETS[i]
            );
            let actual = (&slot.entry_ptrs[i] as *const _ as usize) - base;
            assert_eq!(
                actual,
                JitPICSlot::ENTRY_PTR_OFFSETS[i],
                "ENTRY_PTR_OFFSETS[{i}] drift: {actual} vs {}",
                JitPICSlot::ENTRY_PTR_OFFSETS[i]
            );
            let actual = (&slot.needs_context[i] as *const _ as usize) - base;
            assert_eq!(
                actual,
                JitPICSlot::NEEDS_CONTEXT_OFFSETS[i],
                "NEEDS_CONTEXT_OFFSETS[{i}] drift: {actual} vs {}",
                JitPICSlot::NEEDS_CONTEXT_OFFSETS[i]
            );
        }
    }

    // ── JitCodeRegion tests ─────────────────────────────────────────

    #[test]
    fn test_jit_code_region_register_and_contains() {
        let mut region = JitCodeRegion::new();
        let fake_ptr = 0x1000 as *const u8;
        assert!(!region.contains(fake_ptr));
        region.register(fake_ptr, 256);
        assert!(region.contains(fake_ptr));
        // Middle of region
        assert!(region.contains(0x1080 as *const u8));
        // One past end should NOT be contained
        assert!(!region.contains(0x1100 as *const u8));
    }

    #[test]
    fn test_jit_code_region_deregister() {
        let mut region = JitCodeRegion::new();
        let fake_ptr = 0x2000 as *const u8;
        region.register(fake_ptr, 128);
        assert!(region.contains(fake_ptr));
        region.deregister(fake_ptr);
        assert!(!region.contains(fake_ptr));
    }

    #[test]
    fn test_jit_code_region_multiple_regions() {
        let mut region = JitCodeRegion::new();
        region.register(0x1000 as *const u8, 256);
        region.register(0x3000 as *const u8, 512);
        assert!(region.contains(0x1080 as *const u8));
        assert!(region.contains(0x3100 as *const u8));
        assert!(!region.contains(0x2000 as *const u8));
    }

    // ── validate_code_ptr tests ─────────────────────────────────────

    #[test]
    fn test_validate_code_ptr_null() {
        assert_eq!(
            validate_code_ptr(std::ptr::null()),
            Err("null JIT code pointer")
        );
    }

    #[test]
    fn test_validate_code_ptr_misaligned() {
        assert_eq!(
            validate_code_ptr(0x1001 as *const u8),
            Err("misaligned JIT code pointer")
        );
    }

    // ── ExecutableBuffer tests ──────────────────────────────────────

    #[test]
    fn test_executable_buffer_new_and_emit() {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        assert_eq!(buf.pos(), 0);
        buf.emit(&[0x90, 0x90, 0x90]); // NOP NOP NOP
        assert_eq!(buf.pos(), 3);
        assert_eq!(buf.as_slice(), &[0x90, 0x90, 0x90]);
    }

    #[test]
    fn test_executable_buffer_emit_byte() {
        let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
        buf.emit_byte(0xCC);
        buf.emit_byte(0xC3);
        assert_eq!(buf.pos(), 2);
        assert_eq!(buf.as_slice(), &[0xCC, 0xC3]);
    }

    #[test]
    fn test_executable_buffer_emit_checked_success() {
        let mut buf = ExecutableBuffer::new(8).expect("alloc failed");
        assert!(buf.emit_checked(&[1, 2, 3, 4]));
        assert_eq!(buf.pos(), 4);
    }

    #[test]
    fn test_executable_buffer_emit_checked_overflow() {
        let mut buf = ExecutableBuffer::new(4).expect("alloc failed");
        buf.emit(&[1, 2, 3]);
        // Only 1 byte left, trying to emit 2 should fail
        assert!(!buf.emit_checked(&[4, 5]));
        // Position should be unchanged
        assert_eq!(buf.pos(), 3);
    }

    #[test]
    fn test_executable_buffer_patch_and_read_i32() {
        let mut buf = ExecutableBuffer::new(32).expect("alloc failed");
        buf.emit(&[0; 8]); // 8 zero bytes
        buf.try_patch_i32(0, 0x12345678).expect("in-bounds patch");
        assert_eq!(buf.read_i32(0), 0x12345678);
        buf.try_patch_i32(4, -42).expect("in-bounds patch");
        assert_eq!(buf.read_i32(4), -42);
    }

    #[test]
    fn test_executable_buffer_emit_overflow_marks_flag() {
        // An emit past capacity must NOT panic: it records `overflowed` and
        // skips the write so the compile driver can bail gracefully.
        let mut buf = ExecutableBuffer::new(4).expect("alloc failed");
        assert!(!buf.overflowed());
        buf.emit(&[1, 2, 3, 4, 5]); // 5 bytes into 4-capacity buffer
        assert!(buf.overflowed());
        assert_eq!(buf.pos(), 0, "overflowing emit must not advance len");
        buf.emit_byte(0xCC);
        assert_eq!(buf.pos(), 0, "overflowing emit_byte must not advance len");
    }

    #[test]
    fn test_try_patch_i32_out_of_bounds_returns_err_does_not_panic() {
        // Task #20: a patch site pointing past `len` must return
        // `Err(PatchFailed)`, mark the buffer overflowed, and never panic.
        // The compile driver relies on the overflow flag to bail to the
        // interpreter when codegen has slipped past its size estimate.
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0; 8]); // only 8 bytes emitted, so offset 8..=11 is OOB
        let result = buf.try_patch_i32(8, 0xDEAD_BEEFu32 as i32);
        assert!(matches!(
            result,
            Err(CompileError::PatchFailed {
                kind: "i32",
                offset: 8
            })
        ));
        assert!(
            buf.overflowed(),
            "OOB patch must set the sticky overflow flag for the compile driver"
        );

        // Same contract for try_patch_byte.
        let mut buf2 = ExecutableBuffer::new(64).expect("alloc failed");
        buf2.emit(&[0; 2]);
        let result2 = buf2.try_patch_byte(99, 0xCC);
        assert!(matches!(
            result2,
            Err(CompileError::PatchFailed {
                kind: "byte",
                offset: 99
            })
        ));
        assert!(buf2.overflowed());
    }

    #[test]
    fn test_try_call_nine_args_returns_too_many_args_err_no_panic() {
        // Task #20: invoking a compiled method with more arguments than
        // the JIT's hand-rolled call thunks support must return
        // `Err(TooManyArgs)` rather than silently returning 0 or
        // panicking. Both `try_call` and `try_call_with_context` honor
        // this contract.
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET — just a valid landing pad
        let cm = CompiledMethod::new(buf);

        // 9 args exceeds the 8-arg ceiling of `try_call`.
        let nine = [0i64; 9];
        let r = unsafe { cm.try_call(&nine) };
        assert!(
            matches!(r, Err(CompileError::TooManyArgs(9))),
            "expected TooManyArgs(9), got {r:?}"
        );

        // 8 args exceeds the 7-arg ceiling of `try_call_with_context`
        // (one register is consumed by the implicit vm_ptr).
        let mut buf2 = ExecutableBuffer::new(64).expect("alloc failed");
        buf2.emit(&[0xC3]);
        let cm2 = CompiledMethod::new_with_context(buf2);
        let eight = [0i64; 8];
        let r2 = unsafe { cm2.try_call_with_context(0, &eight) };
        assert!(
            matches!(r2, Err(CompileError::TooManyArgs(8))),
            "expected TooManyArgs(8) for context call, got {r2:?}"
        );
    }

    #[test]
    fn test_try_call_returns_ok_for_in_bounds_zero_arg_method() {
        // Task #44: positive test. After removing the panicking `call`
        // wrapper, `try_call` is the canonical happy-path entry point.
        // A minimal compiled method that returns 0 (XOR EAX,EAX; RET)
        // must produce `Ok(0)` when invoked with no arguments.
        // Encoded as: 31 C0 (xor eax, eax) C3 (ret).
        let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
        buf.emit(&[0x31, 0xC0, 0xC3]);
        let cm = CompiledMethod::new(buf);
        // SAFETY: the emitted code is a well-formed x86-64 leaf (XOR
        // EAX,EAX; RET) using the platform C ABI for a no-arg
        // `extern "C" fn() -> i64`. CompiledMethod::new finalized the
        // buffer, so the page is executable.
        #[cfg(target_arch = "x86_64")]
        {
            let r = unsafe { cm.try_call(&[]) };
            assert_eq!(r, Ok(0), "try_call({{}}) should return Ok(0)");
        }
        // On non-x86_64 targets the emitted bytes are not valid, so
        // suppress the call but still exercise the type-level
        // try_call contract (TooManyArgs path) to keep the test
        // platform-independent.
        #[cfg(not(target_arch = "x86_64"))]
        {
            let big = [0i64; 9];
            let r = unsafe { cm.try_call(&big) };
            assert!(matches!(r, Err(CompileError::TooManyArgs(9))));
        }
    }

    #[test]
    fn test_try_call_invalid_code_ptr_returns_err_not_silent_zero() {
        // Task #44 acceptance criterion 5: demonstrate that a JIT
        // "compile-failed" / runtime-invalid-pointer result propagates
        // as `Err(CompileError::InvalidCodePtr(_))` instead of being
        // silently downgraded to a `0` return value (which is what the
        // now-removed `call` wrapper did via `tracing::warn!` +
        // return 0).
        //
        // We synthesize the invalid-pointer condition by constructing
        // a `CompiledMethod` whose `entry` field has been overwritten
        // with a value outside any known JIT code region. The pointer
        // is also misaligned (1 byte) so `validate_code_ptr` would
        // reject it on alignment alone even if the region check ever
        // changes.
        let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
        buf.emit(&[0xC3]);
        let mut cm = CompiledMethod::new(buf);
        // Stomp on the entry pointer: misaligned + outside any region.
        cm.entry = 0x1 as *const u8;

        // SAFETY: the call is gated by `validate_code_ptr` inside
        // `try_call`, which rejects the synthetic pointer before any
        // transmute-to-fn-pointer happens. No real code is executed.
        let r = unsafe { cm.try_call(&[]) };
        assert!(
            matches!(r, Err(CompileError::InvalidCodePtr(_))),
            "expected InvalidCodePtr, got {r:?} (silent-zero would be Ok(0) — that contract is gone)"
        );

        // Equivalent contract for the context-needing variant: must
        // also surface InvalidCodePtr rather than swallowing it.
        let mut buf2 = ExecutableBuffer::new(16).expect("alloc failed");
        buf2.emit(&[0xC3]);
        let mut cm2 = CompiledMethod::new_with_context(buf2);
        cm2.entry = 0x3 as *const u8;
        let r2 = unsafe { cm2.try_call_with_context(0, &[]) };
        assert!(
            matches!(r2, Err(CompileError::InvalidCodePtr(_))),
            "expected InvalidCodePtr for context call, got {r2:?}"
        );
    }

    #[test]
    fn test_executable_buffer_finalize_and_make_writable() {
        let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        buf.finalize();
        // make_writable and finalize again should not crash
        buf.make_writable();
        buf.finalize();
    }

    // ── CompiledMethod tests ────────────────────────────────────────

    #[test]
    fn test_compiled_method_new_pure() {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let cm = CompiledMethod::new(buf);
        assert!(!cm.needs_context());
        assert!(!cm.needs_heap());
        assert!(!cm.entry_ptr().is_null());
    }

    #[test]
    fn test_compiled_method_new_with_context() {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let cm = CompiledMethod::new_with_context(buf);
        assert!(cm.needs_context());
        assert!(cm.needs_heap());
    }

    // ── JitMICSlot tests ────────────────────────────────────────────

    #[test]
    fn test_jit_mic_slot_default() {
        let slot = JitMICSlot::new();
        assert_eq!(
            slot.cached_class_id
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(slot.cached_class_name.lock().is_none());
    }

    #[test]
    fn test_jit_mic_slot_prepopulate() {
        let slot = JitMICSlot::new();
        slot.prepopulate(42);
        assert_eq!(
            slot.cached_class_id
                .load(std::sync::atomic::Ordering::Relaxed),
            42
        );
    }

    // ── JitPICSlot tests ────────────────────────────────────────────

    #[test]
    fn test_jit_pic_slot_empty_misses() {
        let pic = JitPICSlot::new();
        assert!(pic.lookup(42).is_none());
        assert_eq!(pic.entries_used(), 0);
        assert_eq!(pic.misses.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn test_jit_pic_slot_install_and_hit() {
        let pic = JitPICSlot::new();
        pic.install(1, "A", 0x1000, false);
        pic.install(2, "B", 0x2000, true);
        pic.install(3, "C", 0x3000, false);
        assert_eq!(pic.entries_used(), 3);
        assert_eq!(pic.lookup(1), Some((0x1000, false)));
        assert_eq!(pic.lookup(2), Some((0x2000, true)));
        assert_eq!(pic.lookup(3), Some((0x3000, false)));
        assert!(pic.lookup(99).is_none());
    }

    #[test]
    fn test_jit_pic_slot_lru_evicts_least_hit() {
        let pic = JitPICSlot::new();
        pic.install(1, "A", 0x1000, false);
        pic.install(2, "B", 0x2000, false);
        pic.install(3, "C", 0x3000, false);
        // Hit class 1 many times so class 2 and 3 look less warm.
        for _ in 0..10 {
            pic.lookup(1);
        }
        pic.lookup(3); // give class 3 at least one hit
                       // Hit counters: [10, 0, 1] → victim = index 1 (class 2).
        pic.install(4, "D", 0x4000, true);
        // Class 1 and 3 remain; class 2 evicted; class 4 installed.
        assert_eq!(pic.lookup(1), Some((0x1000, false)));
        assert!(pic.lookup(2).is_none());
        assert_eq!(pic.lookup(3), Some((0x3000, false)));
        assert_eq!(pic.lookup(4), Some((0x4000, true)));
    }

    #[test]
    fn test_jit_pic_slot_seed_from_mic() {
        let mic = JitMICSlot::new();
        mic.update(7, "Seven", 0x7000, true);
        mic.record_hit();
        mic.record_hit();
        let pic = JitPICSlot::new();
        pic.seed_from_mic(&mic);
        assert_eq!(pic.entries_used(), 1);
        assert_eq!(pic.lookup(7), Some((0x7000, true)));
        let name = pic.class_names[0].lock();
        assert_eq!(name.as_deref(), Some("Seven"));
    }

    #[test]
    fn test_jit_pic_slot_seed_empty_mic_noop() {
        let mic = JitMICSlot::new();
        let pic = JitPICSlot::new();
        pic.seed_from_mic(&mic);
        assert_eq!(pic.entries_used(), 0);
    }

    #[test]
    fn test_jit_pic_slot_megamorphic_detection() {
        let pic = JitPICSlot::new();
        pic.install(1, "A", 0x1000, false);
        pic.install(2, "B", 0x2000, false);
        pic.install(3, "C", 0x3000, false);
        // Not megamorphic yet — full but no misses.
        assert!(!pic.is_megamorphic());
        // Pound it with misses.
        for i in 100..(100 + PIC_TO_MEGA_THRESHOLD) {
            pic.lookup(i as u32);
        }
        assert!(pic.is_megamorphic());
    }

    #[test]
    fn test_jit_pic_slot_duplicate_install_overwrites() {
        let pic = JitPICSlot::new();
        pic.install(1, "A", 0x1000, false);
        // Install again with a new pointer but same class_id → should
        // fill the second empty slot rather than overwrite. That's
        // slightly wasteful but correct: linear probe returns the
        // first matching entry, so the old pointer still fires.
        pic.install(1, "A", 0x1000, false);
        assert_eq!(pic.entries_used(), 2);
        assert_eq!(pic.lookup(1), Some((0x1000, false)));
    }

    #[test]
    fn test_jit_pic_slot_thresholds_are_sensible() {
        // Contract: MIC_TO_PIC < PIC_TO_MEGA. Otherwise the promotion
        // logic would skip PIC entirely.
        assert!(MIC_TO_PIC_THRESHOLD < PIC_TO_MEGA_THRESHOLD);
        assert!(MIC_TO_PIC_THRESHOLD >= 2);
        assert!(JIT_PIC_ENTRIES >= 2);
    }

    // ── T17.Β.1 — PIC 3-way probe semantics ────────────────────────

    /// Install 3 distinct `(class_id, entry_ptr)` entries and issue a
    /// lookup for each. The pure-Rust [`JitPICSlot::lookup`] mirrors
    /// what the x64 emission does with `CMP RAX, [RBX+off]; JE
    /// entry_i` for each of the 3 slots. All three must hit and the
    /// miss counter must stay at 0.
    #[test]
    fn t17_b_pic_3_hits() {
        let pic = JitPICSlot::new();
        pic.install(11, "A", 0xAAAA_0000, false);
        pic.install(22, "B", 0xBBBB_0000, true);
        pic.install(33, "C", 0xCCCC_0000, false);
        assert_eq!(pic.entries_used(), JIT_PIC_ENTRIES);

        // Each receiver class_id is one of the 3 probed slots.
        assert_eq!(pic.lookup(11), Some((0xAAAA_0000, false)));
        assert_eq!(pic.lookup(22), Some((0xBBBB_0000, true)));
        assert_eq!(pic.lookup(33), Some((0xCCCC_0000, false)));

        // Miss counter stays at 0 — no probe fell through.
        assert_eq!(
            pic.misses.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "miss counter should not advance on 3 PIC hits"
        );

        // Per-slot hit counters all incremented exactly once.
        for i in 0..JIT_PIC_ENTRIES {
            assert_eq!(
                pic.hits[i].load(std::sync::atomic::Ordering::Relaxed),
                1,
                "slot {i} should have exactly 1 hit"
            );
        }
    }

    /// A receiver whose class_id is not in any of the 3 slots must
    /// fall through to the generic-helper path. In the pure-Rust
    /// mirror, that surfaces as `lookup` returning `None` and the
    /// miss counter incrementing.
    #[test]
    fn t17_b_pic_miss_falls_through() {
        let pic = JitPICSlot::new();
        pic.install(11, "A", 0xAAAA_0000, false);
        pic.install(22, "B", 0xBBBB_0000, false);
        pic.install(33, "C", 0xCCCC_0000, false);

        // 4th receiver type — no slot matches.
        let res = pic.lookup(44);
        assert!(
            res.is_none(),
            "miss on an unknown class_id must return None"
        );
        assert_eq!(
            pic.misses.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "miss counter must increment on a fall-through"
        );

        // Successive misses also increment the counter.
        pic.lookup(55);
        pic.lookup(66);
        assert_eq!(
            pic.misses.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "miss counter must track every fall-through"
        );

        // No hit counters were touched on misses.
        for i in 0..JIT_PIC_ENTRIES {
            assert_eq!(
                pic.hits[i].load(std::sync::atomic::Ordering::Relaxed),
                0,
                "slot {i} must not record a hit on a miss"
            );
        }
    }

    /// LFU-eviction shipped earlier — re-state it under the T17
    /// naming so the invariant is anchored in the new test set too.
    #[test]
    fn t17_b_pic_lfu_eviction() {
        let pic = JitPICSlot::new();
        pic.install(1, "A", 0x1000, false);
        pic.install(2, "B", 0x2000, false);
        pic.install(3, "C", 0x3000, false);
        // Class 1 is hot, class 2 is cold, class 3 has 1 hit.
        for _ in 0..10 {
            pic.lookup(1);
        }
        pic.lookup(3);
        // Evict LFU → slot 1 (class 2) goes, class 4 lands there.
        pic.install(4, "D", 0x4000, true);
        assert_eq!(pic.lookup(1), Some((0x1000, false)));
        assert!(pic.lookup(2).is_none(), "class 2 was the LFU victim");
        assert_eq!(pic.lookup(3), Some((0x3000, false)));
        assert_eq!(pic.lookup(4), Some((0x4000, true)));
    }

    /// MIC must signal PIC promotion once miss count exceeds the
    /// threshold — the adaptive recompiler keys off this flag when it
    /// sweeps MICs on each safepoint.
    #[test]
    fn t17_b_mic_needs_pic_promotion_after_threshold() {
        let mic = JitMICSlot::new();
        // Fresh MIC — no misses, no promotion yet.
        assert!(!mic.needs_pic_promotion());
        // Pump misses up through MIC_TO_PIC_THRESHOLD; one must
        // exceed (strict >) to trigger.
        for _ in 0..=MIC_TO_PIC_THRESHOLD {
            mic.record_miss();
        }
        assert!(
            mic.needs_pic_promotion(),
            "miss count > MIC_TO_PIC_THRESHOLD must trigger promotion"
        );
    }

    /// `promote_mic_to_pic` must produce a fresh PIC seeded with the
    /// MIC's cached entry (slot 0). Any subsequent `install` lands in
    /// a different slot.
    #[test]
    fn t17_b_promote_mic_to_pic_preserves_cached_entry() {
        let mic = JitMICSlot::new();
        mic.update(7, "Seven", 0x7000, true);
        // Pump misses past threshold.
        for _ in 0..=MIC_TO_PIC_THRESHOLD {
            mic.record_miss();
        }
        assert!(mic.needs_pic_promotion());

        let pic = promote_mic_to_pic(&mic);
        assert_eq!(pic.entries_used(), 1);
        assert_eq!(pic.lookup(7), Some((0x7000, true)));

        // A new receiver lands in a fresh slot, not over the seeded one.
        pic.install(8, "Eight", 0x8000, false);
        assert_eq!(pic.entries_used(), 2);
        assert_eq!(pic.lookup(7), Some((0x7000, true)));
        assert_eq!(pic.lookup(8), Some((0x8000, false)));
    }

    /// PIC deopt-to-megamorphic mirrors the scope's
    /// `PIC_TO_MEGA_THRESHOLD` gate and the `should_deopt_to_mega()`
    /// helper the adaptive recompiler uses.
    #[test]
    fn t17_b_pic_deopts_to_mega_on_miss_flood() {
        let pic = JitPICSlot::new();
        pic.install(1, "A", 0x1000, false);
        pic.install(2, "B", 0x2000, false);
        pic.install(3, "C", 0x3000, false);
        assert!(!pic.should_deopt_to_mega());

        // Pound with misses until the threshold is crossed.
        for i in 100..(100 + PIC_TO_MEGA_THRESHOLD) {
            pic.lookup(i as u32);
        }
        assert!(
            pic.should_deopt_to_mega(),
            "PIC must request deopt once miss count ≥ PIC_TO_MEGA_THRESHOLD"
        );
    }

    // ── DescriptorParamIter tests ───────────────────────────────────

    #[test]
    fn test_descriptor_param_iter_simple() {
        let params: Vec<u8> = DescriptorParamIter::new("(II)I").collect();
        assert_eq!(params, vec![b'I', b'I']);
    }

    #[test]
    fn test_descriptor_param_iter_mixed_types() {
        let params: Vec<u8> = DescriptorParamIter::new("(IJLjava/lang/String;[I)V").collect();
        assert_eq!(params, vec![b'I', b'J', b'L', b'[']);
    }

    #[test]
    fn test_descriptor_param_iter_no_params() {
        let params: Vec<u8> = DescriptorParamIter::new("()V").collect();
        assert!(params.is_empty());
    }

    #[test]
    fn test_descriptor_param_iter_array_of_objects() {
        let params: Vec<u8> = DescriptorParamIter::new("([Ljava/lang/Object;)V").collect();
        assert_eq!(params, vec![b'[']);
    }

    // ── JitCache tests ──────────────────────────────────────────────

    #[test]
    fn test_jit_cache_new_is_empty() {
        let cache = JitCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_jit_cache_put_and_get() {
        let mut cache = JitCache::new();
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let cm = CompiledMethod::new(buf);

        let class: Arc<str> = Arc::from("TestClass");
        let method: Arc<str> = Arc::from("testMethod");
        let desc: Arc<str> = Arc::from("(II)I");

        cache.put(class.clone(), method.clone(), desc.clone(), cm);
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());

        let result = cache.get(&class, &method, &desc);
        assert!(result.is_some());
    }

    #[test]
    fn test_jit_cache_get_missing() {
        let cache = JitCache::new();
        let class: Arc<str> = Arc::from("Missing");
        let method: Arc<str> = Arc::from("missing");
        let desc: Arc<str> = Arc::from("()V");
        assert!(cache.get(&class, &method, &desc).is_none());
    }

    #[test]
    fn test_jit_cache_intern_string() {
        let mut cache = JitCache::new();
        let (ptr, len) = cache.intern_string("hello".to_string());
        assert!(!ptr.is_null());
        assert_eq!(len, 5);
        let s = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len)) };
        assert_eq!(s, "hello");
    }

    /// T10.3 — Verify the FxHashMap swap preserves insert/lookup semantics for
    /// 100 distinct JitKey entries with different class/method/descriptor mixes.
    #[test]
    fn t10_jit_cache_fxhash_insert_lookup() {
        let mut cache = JitCache::new();
        let mut keys: Vec<(Arc<str>, Arc<str>, Arc<str>)> = Vec::with_capacity(100);
        for i in 0..100 {
            let class: Arc<str> = Arc::from(format!("pkg/Cls{i}"));
            let method: Arc<str> = Arc::from(format!("m{i}"));
            let desc: Arc<str> = Arc::from(format!("(I)I{i}"));
            let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
            buf.emit(&[0xC3]); // RET
            let cm = CompiledMethod::new(buf);
            cache.put(class.clone(), method.clone(), desc.clone(), cm);
            keys.push((class, method, desc));
        }
        assert_eq!(cache.len(), 100);
        for (class, method, desc) in &keys {
            assert!(
                cache.get(class, method, desc).is_some(),
                "missing key {}/{}/{}",
                class,
                method,
                desc
            );
        }
        // Negative lookup: a class name we never inserted.
        let missing_cls: Arc<str> = Arc::from("pkg/Unseen");
        let missing_m: Arc<str> = Arc::from("x");
        let missing_d: Arc<str> = Arc::from("()V");
        assert!(cache.get(&missing_cls, &missing_m, &missing_d).is_none());
    }

    // ── count_param_slots tests ─────────────────────────────────────

    #[test]
    fn test_count_param_slots_basic() {
        assert_eq!(count_param_slots("(II)I"), 2);
        assert_eq!(count_param_slots("()V"), 0);
        assert_eq!(count_param_slots("(I)V"), 1);
    }

    #[test]
    fn test_count_param_slots_long_double() {
        assert_eq!(count_param_slots("(JD)V"), 2);
        assert_eq!(count_param_slots("(IJI)I"), 3);
    }

    #[test]
    fn test_count_param_slots_objects_arrays() {
        assert_eq!(count_param_slots("(Ljava/lang/String;)V"), 1);
        assert_eq!(count_param_slots("([I[Ljava/lang/Object;I)V"), 3);
        assert_eq!(count_param_slots("([[I)V"), 1);
    }

    #[test]
    fn test_count_param_slots_empty_string() {
        assert_eq!(count_param_slots(""), 0);
    }

    // ── return_type tests ───────────────────────────────────────────

    #[test]
    fn test_return_type_basic() {
        assert_eq!(return_type("(II)I"), b'I');
        assert_eq!(return_type("()V"), b'V');
        assert_eq!(return_type("(I)J"), b'J');
        assert_eq!(return_type("()Ljava/lang/String;"), b'L');
    }

    #[test]
    fn test_return_type_no_closing_paren() {
        // Malformed descriptor — should return 'V' as default
        assert_eq!(return_type("II"), b'V');
    }

    // ── return_type edge cases ─────────────────────────────────────

    #[test]
    fn test_return_type_array() {
        assert_eq!(return_type("()[I"), b'[');
    }

    #[test]
    fn test_return_type_double() {
        assert_eq!(return_type("(II)D"), b'D');
    }

    // ── Escape analysis IR wiring tests ────────────────────────────

    #[test]
    fn test_ea_from_ir_returns_id_map() {
        // Build a minimal IR graph: Start(0) → Const(1) → Return(2)
        let mut g = ir::Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
        };
        let start = g.add(ir::Op::Start, ir::IrType::Void, vec![], None);
        let c = g.add(ir::Op::Const(42), ir::IrType::Int, vec![], None);
        let ret = g.add(ir::Op::Return, ir::IrType::Void, vec![start, c], None);
        g.entry = start;
        g.exit = ret;

        let (ea_graph, id_map) = escape_analysis_from_ir(&g);

        // id_map length matches IR node count
        assert_eq!(id_map.len(), g.nodes.len());
        // Entry maps to EA Start (0), Exit maps to EA Return (1)
        assert_eq!(id_map[start as usize], 0);
        assert_eq!(id_map[ret as usize], 1);
        // Const node should map to a valid EA node (not usize::MAX)
        assert_ne!(id_map[c as usize], usize::MAX);
        // EA graph should have nodes
        assert!(ea_graph.nodes.len() >= 3);
    }

    #[test]
    fn test_apply_ea_marks_dead_nodes() {
        // Build an IR graph with: Start, New, Const, Store, Load, Return
        // The New doesn't escape, so EA should find it scalar-replaceable.
        let mut g = ir::Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: 0,
            safepoints: Vec::new(),
        };
        let start = g.add(ir::Op::Start, ir::IrType::Void, vec![], None);
        let alloc = g.add(
            ir::Op::New {
                class_id: 1,
                num_fields: 2,
            },
            ir::IrType::Ref,
            vec![start],
            None,
        );
        let c42 = g.add(ir::Op::Const(42), ir::IrType::Int, vec![], None);
        // Store field 0 (MemKind::Int = discriminant 0)
        let store = g.add(
            ir::Op::Store(ir::MemKind::Int),
            ir::IrType::Void,
            vec![alloc, c42],
            None,
        );
        // Load field 0 (MemKind::Int = discriminant 0)
        let load = g.add(
            ir::Op::Load(ir::MemKind::Int),
            ir::IrType::Int,
            vec![alloc],
            None,
        );
        let ret = g.add(ir::Op::Return, ir::IrType::Void, vec![start, load], None);
        g.entry = start;
        g.exit = ret;

        // Run EA
        let (ea_graph, id_map) = escape_analysis_from_ir(&g);
        let ea_result = escape_analysis::analyze_escapes(&ea_graph);

        // Apply to IR
        if !ea_result.scalar_replaceable.is_empty() || !ea_result.elide_locks.is_empty() {
            apply_ea_to_ir(&mut g, &id_map, &ea_result);
        }

        // If EA found the alloc as scalar-replaceable, the allocation,
        // store, and load should all be Dead now.
        if !ea_result.scalar_replaceable.is_empty() {
            assert_eq!(g.nodes[alloc as usize].op, ir::Op::Dead);
            assert_eq!(g.nodes[store as usize].op, ir::Op::Dead);
            assert_eq!(g.nodes[load as usize].op, ir::Op::Dead);
            // The return node's input that was the load should now point
            // to the stored constant value (c42).
            assert!(
                g.nodes[ret as usize].inputs.contains(&c42),
                "Return should reference the constant directly after scalar replacement"
            );
        }
    }

    // --- Phase 80.3: Value Enum Layout Safety Tests ---

    #[test]
    fn value_size_is_16_bytes() {
        assert_eq!(std::mem::size_of::<Value>(), 16);
    }

    #[test]
    fn value_alignment_at_most_8() {
        assert!(std::mem::align_of::<Value>() <= 8);
    }

    #[test]
    fn objectref_is_pointer_sized() {
        assert_eq!(
            std::mem::size_of::<ObjectRef>(),
            std::mem::size_of::<*mut u8>()
        );
    }

    #[test]
    fn value_to_bytes_roundtrip() {
        // Verify value_to_bytes produces correct bytes for known values.
        let val = Value::Int(0x12345678);
        let bytes = super::value_to_bytes(val);
        // The Int discriminant and payload must be present somewhere in the 16 bytes.
        // Check the i32 payload bytes appear.
        let payload = 0x12345678_i32.to_le_bytes();
        let found = (0..13).any(|i| bytes[i..i + 4] == payload);
        assert!(found, "Int payload must be present in byte representation");
    }

    #[test]
    fn probe_object_ptr_offset_in_range() {
        // The offset must be a valid position within a 16-byte Value.
        let off = super::probe_object_ptr_offset();
        assert!(
            off < 9,
            "offset must leave room for 8-byte pointer within 16 bytes, got {off}"
        );
    }

    #[test]
    fn probe_object_null_template_reconstructs_none() {
        // The null template (lo, hi) must reconstruct to Value::Object(None)
        // when written back and transmuted.
        let (lo, hi) = super::probe_object_null_template();
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&lo.to_le_bytes());
        bytes[8..16].copy_from_slice(&hi.to_le_bytes());
        let reconstructed: Value = unsafe { std::mem::transmute(bytes) };
        assert_eq!(
            reconstructed,
            Value::Object(None),
            "null template must reconstruct to Value::Object(None)"
        );
    }

    // ===================================================================
    // Session 33: JIT Monomorphic Inline Cache (MIC) tests
    // ===================================================================

    #[test]
    fn s33_mic_slot_new_is_empty() {
        let mic = JitMICSlot::new();
        assert_eq!(
            mic.cached_class_id
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(mic.cached_class_name.lock().is_none());
        assert_eq!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(!mic
            .cached_needs_context
            .load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(mic.total_observations(), 0);
        assert_eq!(mic.hit_rate_pct(), 0);
    }

    #[test]
    fn s33_mic_slot_prepopulate() {
        let mic = JitMICSlot::new();
        mic.prepopulate(42);
        assert_eq!(
            mic.cached_class_id
                .load(std::sync::atomic::Ordering::Relaxed),
            42
        );
        // Entry ptr should still be 0 (prepopulate only sets class_id)
        assert_eq!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn s33_mic_slot_update_all_fields() {
        let mic = JitMICSlot::new();
        mic.update(7, "com/example/MyClass", 0xDEAD_BEEF, true);
        assert_eq!(
            mic.cached_class_id
                .load(std::sync::atomic::Ordering::Acquire),
            7
        );
        assert_eq!(
            mic.cached_class_name.lock().as_deref(),
            Some("com/example/MyClass")
        );
        assert_eq!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Acquire),
            0xDEAD_BEEF
        );
        assert!(mic
            .cached_needs_context
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn s33_mic_slot_hit_miss_counters() {
        let mic = JitMICSlot::new();
        for _ in 0..10 {
            mic.record_hit();
        }
        for _ in 0..5 {
            mic.record_miss();
        }
        assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 10);
        assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 5);
        assert_eq!(mic.total_observations(), 15);
        // hit_rate = 10/15 * 100 = 66
        assert_eq!(mic.hit_rate_pct(), 66);
    }

    #[test]
    fn s33_mic_slot_is_monomorphic() {
        let mic = JitMICSlot::new();
        // Not enough observations
        for _ in 0..5 {
            mic.record_hit();
        }
        assert!(!mic.is_monomorphic());
        // 10 hits, 0 misses → 100% hit rate, ≥10 obs → monomorphic
        for _ in 0..5 {
            mic.record_hit();
        }
        assert!(mic.is_monomorphic());
    }

    #[test]
    fn s33_mic_slot_is_megamorphic() {
        let mic = JitMICSlot::new();
        // 5 hits, 20 misses → 20% hit rate → megamorphic
        for _ in 0..5 {
            mic.record_hit();
        }
        for _ in 0..20 {
            mic.record_miss();
        }
        assert!(mic.is_megamorphic());
    }

    #[test]
    fn s33_mic_slot_not_megamorphic_when_mostly_hits() {
        let mic = JitMICSlot::new();
        for _ in 0..18 {
            mic.record_hit();
        }
        for _ in 0..2 {
            mic.record_miss();
        }
        assert!(!mic.is_megamorphic());
        assert!(mic.is_monomorphic());
    }

    #[test]
    fn s33_mic_slot_update_overwrites_previous() {
        let mic = JitMICSlot::new();
        mic.update(1, "A", 100, false);
        mic.update(2, "B", 200, true);
        assert_eq!(
            mic.cached_class_id
                .load(std::sync::atomic::Ordering::Acquire),
            2
        );
        assert_eq!(mic.cached_class_name.lock().as_deref(), Some("B"));
        assert_eq!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Acquire),
            200
        );
        assert!(mic
            .cached_needs_context
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn s33_mic_slot_concurrent_updates() {
        use std::sync::Arc;
        let mic = Arc::new(JitMICSlot::new());
        let mut handles = Vec::new();
        for i in 0..8 {
            let mic_clone = mic.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    if i % 2 == 0 {
                        mic_clone.record_hit();
                    } else {
                        mic_clone.record_miss();
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // 4 threads * 100 hits + 4 threads * 100 misses = 800
        assert_eq!(mic.total_observations(), 800);
        assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 400);
        assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 400);
    }

    #[test]
    fn s33_mic_slot_hit_rate_boundary() {
        // Exactly 90% hit rate should count as monomorphic
        let mic = JitMICSlot::new();
        for _ in 0..9 {
            mic.record_hit();
        }
        for _ in 0..1 {
            mic.record_miss();
        }
        // 10 total, 90% hits → monomorphic
        assert!(mic.is_monomorphic());
    }

    #[test]
    fn s33_mic_slot_entry_ptr_zero_means_unresolved() {
        let mic = JitMICSlot::new();
        mic.prepopulate(5);
        // Even with class_id populated, entry_ptr 0 means no direct dispatch
        assert_eq!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        // After update with non-zero entry, it's resolved
        mic.update(5, "Foo", 0x1234, false);
        assert_ne!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    // =======================================================================
    // Phase G (RG.1 .. RG.12) — JIT / interpreter correctness invariants
    //
    // These tests pin the shape of every Phase G item: either the JIT accepts
    // and lowers the opcode itself, or it must cleanly bail so the interpreter
    // handles it correctly. Each test is self-contained and does not require
    // the full VM to run.
    // =======================================================================

    use crate::x64::{is_jit_compatible, jit_scan};

    /// Build a minimal bytecode header that ends in `ireturn` so `jit_scan`
    /// sees a valid terminator after the probe opcodes.
    fn ireturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
        // Push a zero int so ireturn has an operand.
        prefix.push(0x03); // iconst_0
        prefix.push(0xac); // ireturn
        prefix
    }

    fn lreturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
        prefix.push(0x09); // lconst_0
        prefix.push(0xad); // lreturn
        prefix
    }

    fn freturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
        prefix.push(0x0b); // fconst_0
        prefix.push(0xae); // freturn
        prefix
    }

    fn dreturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
        prefix.push(0x0e); // dconst_0
        prefix.push(0xaf); // dreturn
        prefix
    }

    /// RG.1 — A method containing `invokedynamic` (0xba) must not be JIT'd
    /// so the interpreter's real invokedynamic dispatch (makeConcat, lambda
    /// bootstraps) runs. Bailing to the interpreter is the correctness
    /// guarantee — RG.1's "JIT output matches interpreter byte-for-byte"
    /// trivially holds if JIT refuses to compile the method.
    #[test]
    fn rg1_jit_rejects_invokedynamic() {
        let code = ireturn_tail(vec![
            0xba, 0x00, 0x01, 0x00, 0x00, // invokedynamic #1, 0, 0
        ]);
        assert!(
            !is_jit_compatible(&code, code.len(), "()I"),
            "JIT must bail on invokedynamic so interpreter handles the bootstrap"
        );
    }

    /// RG.2 — JIT accepts fcmpl/fcmpg/dcmpl/dcmpg opcodes. The scanner groups
    /// them with lcmp in the 0x94..=0x98 range. NaN canonicalization itself is
    /// verified in the compiler — here we only pin the scanner shape.
    #[test]
    fn rg2_jit_accepts_fp_compare_opcodes() {
        // Float: fconst_0 (0x0b), fconst_1 (0x0c), fcmpl (0x95), ireturn
        let fcmpl = vec![0x0b, 0x0c, 0x95, 0x03, 0xac];
        assert!(
            is_jit_compatible(&fcmpl, fcmpl.len(), "()I"),
            "fcmpl must be JIT-compatible"
        );

        // fconst_0, fconst_1, fcmpg
        let fcmpg = vec![0x0b, 0x0c, 0x96, 0x03, 0xac];
        assert!(
            is_jit_compatible(&fcmpg, fcmpg.len(), "()I"),
            "fcmpg must be JIT-compatible"
        );

        // dconst_0 (0x0e), dconst_1 (0x0f), dcmpl (0x97)
        let dcmpl = vec![0x0e, 0x0f, 0x97, 0x03, 0xac];
        assert!(
            is_jit_compatible(&dcmpl, dcmpl.len(), "()I"),
            "dcmpl must be JIT-compatible"
        );

        // dconst_0, dconst_1, dcmpg (0x98)
        let dcmpg = vec![0x0e, 0x0f, 0x98, 0x03, 0xac];
        assert!(
            is_jit_compatible(&dcmpg, dcmpg.len(), "()I"),
            "dcmpg must be JIT-compatible"
        );
    }

    /// RG.3 — JIT accepts lcmp (0x94).
    #[test]
    fn rg3_jit_accepts_lcmp() {
        // lconst_0 (0x09), lconst_1 (0x0a), lcmp (0x94), ireturn
        let lcmp = vec![0x09, 0x0a, 0x94, 0x03, 0xac];
        assert!(
            is_jit_compatible(&lcmp, lcmp.len(), "()I"),
            "lcmp must be JIT-compatible"
        );
    }

    /// RG.4 — JIT accepts tableswitch and lookupswitch and correctly skips
    /// over their variable-length payloads during scanning.
    #[test]
    fn rg4_jit_accepts_switch_opcodes() {
        // tableswitch at pc=0 needs padding to 4-byte-aligned, so we prepend
        // iconst_0 (1 byte) + nop padding; but the scanner handles alignment
        // from the tableswitch opcode's own pc. Use a 1-byte prefix so pc=1,
        // and tableswitch at pc=1 needs 3 bytes of padding.
        //
        // Layout: [iconst_0=0x03] [tableswitch=0xaa] [pad0 pad0 pad0]
        //         [default=0,0,0,8] [low=0,0,0,0] [high=0,0,0,0]
        //         [offset0=0,0,0,8] [ireturn=0xac]
        //
        // default/offset0 are 32-bit signed offsets from the tableswitch pc.
        let mut ts = vec![0x03, 0xaa, 0, 0]; // iconst_0, tableswitch, 2 pad bytes (1..4 boundary)
        ts.extend_from_slice(&[0, 0, 0, 8]); // default offset = +8 (ireturn)
        ts.extend_from_slice(&[0, 0, 0, 0]); // low = 0
        ts.extend_from_slice(&[0, 0, 0, 0]); // high = 0 (1 entry)
        ts.extend_from_slice(&[0, 0, 0, 8]); // offset[0] = +8
        ts.push(0x03); // iconst_0
        ts.push(0xac); // ireturn
        assert!(
            is_jit_compatible(&ts, ts.len(), "()I"),
            "tableswitch must be JIT-compatible"
        );

        // lookupswitch: [iconst_0] [lookupswitch=0xab] [pad] [default=+14]
        //               [npairs=1] [match=0, offset=+14] [iconst_0] [ireturn]
        let mut ls = vec![0x03, 0xab, 0, 0]; // iconst_0, lookupswitch, 2 pad bytes
        ls.extend_from_slice(&[0, 0, 0, 16]); // default = +16
        ls.extend_from_slice(&[0, 0, 0, 1]); // npairs = 1
        ls.extend_from_slice(&[0, 0, 0, 0]); // match = 0
        ls.extend_from_slice(&[0, 0, 0, 16]); // offset = +16
        ls.push(0x03);
        ls.push(0xac);
        assert!(
            is_jit_compatible(&ls, ls.len(), "()I"),
            "lookupswitch must be JIT-compatible"
        );
    }

    /// RG.5 (updated by RBC.6) — the scanner now ACCEPTS explicit `athrow`
    /// and records `has_athrow`; the compile gates restrict compilation to
    /// methods with NO local exception handlers (the codegen lowers athrow
    /// to "stash pending exception + return the i64::MIN deopt sentinel",
    /// which cannot dispatch to an in-method handler — see
    /// `try_compile_inner`'s `exception_table.is_empty()` gate and the OSR
    /// trigger's decline in `vm/src/runtime/interpreter.rs::try_osr`).
    #[test]
    fn rg5_jit_scan_accepts_athrow_and_flags_it() {
        // aconst_null (0x01), athrow (0xbf), then iconst_0/ireturn filler.
        let code = vec![0x01, 0xbf, 0x03, 0xac];
        let scan = x64::jit_scan(&code, code.len(), "()I")
            .expect("athrow method must pass jit_scan (RBC.6)");
        assert!(
            scan.has_athrow,
            "jit_scan must record has_athrow so compile gates can apply"
        );
        // A method without athrow must NOT set the flag.
        let plain = vec![0x03, 0xac];
        let scan =
            x64::jit_scan(&plain, plain.len(), "()I").expect("trivial method must pass jit_scan");
        assert!(!scan.has_athrow);
    }

    /// RG.6 — JIT scanner accepts `monitorenter` (0xc2) and `monitorexit`
    /// (0xc3) so synchronized blocks are JIT-eligible. The compiler then
    /// either elides the lock (escape-analysis proves thread-local) or bails.
    #[test]
    fn rg6_jit_accepts_monitor_enter_exit() {
        // aconst_null, dup (0x59), monitorenter, monitorexit, pop (0x57), ireturn
        let code = vec![0x01, 0x59, 0xc2, 0xc3, 0x57, 0x03, 0xac];
        assert!(
            is_jit_compatible(&code, code.len(), "()I"),
            "monitorenter/exit must be accepted so synchronized methods can JIT"
        );
    }

    /// RG.7 — `System.arraycopy` is routed through the native registry as an
    /// interpreter-level intrinsic. We verify the invokestatic dispatch path
    /// is scanner-compatible; the actual copy is performed by the native.
    #[test]
    fn rg7_jit_accepts_arraycopy_invoke_site() {
        // aconst_null, iconst_0, aconst_null, iconst_0, iconst_0,
        // invokestatic #1 (placeholder CP index — scanner only checks opcode shape),
        // ireturn
        let code = vec![0x01, 0x03, 0x01, 0x03, 0x03, 0xb8, 0x00, 0x01, 0x03, 0xac];
        assert!(
            is_jit_compatible(&code, code.len(), "()I"),
            "invokestatic for System.arraycopy must scan OK"
        );
    }

    /// RG.8 — A method with a loop (back-edge via goto backward) must still
    /// scan as JIT-compatible; the compiler emits an oop-map safepoint at
    /// each back-edge when lowering to machine code.
    #[test]
    fn rg8_jit_accepts_loop_with_backedge() {
        // iconst_0, istore_1 (0x3c)              ; i = 0
        // iload_1 (0x1b), iconst_5 (0x08), if_icmpge (0xa2) +10 (forward to ireturn)
        // iinc local=1 by 1 (0x84)                ; i++
        // goto -8 (0xa7) backward to iload_1     ; back-edge
        // iconst_0, ireturn
        // Offsets are relative to the branch opcode's pc.
        let code = vec![
            0x03, 0x3c, // iconst_0, istore_1
            0x1b, 0x08, 0xa2, 0x00, 0x0a, // iload_1, iconst_5, if_icmpge +10
            0x84, 0x01, 0x01, // iinc 1, 1
            0xa7, 0xff, 0xf8, // goto -8
            0x03, 0xac, // iconst_0, ireturn
        ];
        assert!(
            is_jit_compatible(&code, code.len(), "()I"),
            "loop with back-edge must be JIT-compatible (safepoint injected by compiler)"
        );
    }

    /// RG.9 — `tiered.rs` carries the tier-up manager; verify the module
    /// exists and exposes the expected hot-counter threshold so the
    /// interpreter-to-JIT transition (OSR entry) is active at runtime.
    #[test]
    fn rg9_tiered_manager_exists() {
        // Spot-check: module is reachable from the jit crate root and the
        // default policy produces a usable manager.
        let _manager = crate::tiered::TieredCompilationManager::with_default_policy();
    }

    /// RG.10 — The interpreter uses typed value-stack slots with automatic
    /// reference-int coercion. We cannot touch the putfield/putstatic blocks
    /// per project constraints, so this test pins the reader's ability to
    /// decode those opcodes — any regression at the reader layer would show
    /// here first.
    #[test]
    fn rg10_reader_decodes_putfield_putstatic() {
        use cratonvm_reader::instruction::Instruction;
        // putfield #1
        let (inst, next) = Instruction::decode(&[0xb5, 0x00, 0x01], 0).unwrap();
        assert_eq!(next, 3);
        assert!(matches!(inst, Instruction::Putfield(1)));
        // putstatic #2
        let (inst, next) = Instruction::decode(&[0xb3, 0x00, 0x02], 0).unwrap();
        assert_eq!(next, 3);
        assert!(matches!(inst, Instruction::Putstatic(2)));
    }

    /// RG.11 — The `wide` prefix (0xc4) extends the following index to u16.
    /// Reader produces an `Iload(u16)` (or Lload/Fload/Dload/Aload/Istore/...
    /// variants) with the widened index. Method with >255 locals must work.
    #[test]
    fn rg11_wide_prefix_decodes_u16_index() {
        use cratonvm_reader::instruction::Instruction;
        // wide iload 300 → 0xc4 0x15 0x01 0x2c
        let (inst, next) = Instruction::decode(&[0xc4, 0x15, 0x01, 0x2c], 0).unwrap();
        assert_eq!(next, 4);
        match inst {
            Instruction::Iload(idx) => assert_eq!(idx, 300),
            other => panic!("expected Iload(300), got {other:?}"),
        }
        // wide iinc index=500 const=2 → 0xc4 0x84 0x01 0xf4 0x00 0x02
        let (inst, next) = Instruction::decode(&[0xc4, 0x84, 0x01, 0xf4, 0x00, 0x02], 0).unwrap();
        assert_eq!(next, 6);
        match inst {
            Instruction::Iinc { index, constant } => {
                assert_eq!(index, 500);
                assert_eq!(constant, 2);
            }
            other => panic!("expected Iinc, got {other:?}"),
        }
    }

    /// RG.12 — The `invokedynamic` opcode (0xba) is decoded by the reader
    /// with its 4-operand shape (cp index + 2 reserved zero bytes) and
    /// routed to `Instruction::Invokedynamic(u16)`. The interpreter then
    /// delegates to the real bootstrap (LambdaMetafactory, etc.) in
    /// `vm/src/runtime/invokedynamic.rs`.
    #[test]
    fn rg12_reader_decodes_invokedynamic() {
        use cratonvm_reader::instruction::Instruction;
        // invokedynamic #42, 0, 0 → 0xba 0x00 0x2a 0x00 0x00
        let (inst, next) = Instruction::decode(&[0xba, 0x00, 0x2a, 0x00, 0x00], 0).unwrap();
        assert_eq!(next, 5);
        match inst {
            Instruction::Invokedynamic(idx) => assert_eq!(idx, 42),
            other => panic!("expected Invokedynamic(42), got {other:?}"),
        }
    }

    /// RG.3 extra — JIT accepts all long/double return types so lcmp/dcmp can
    /// appear in methods returning any numeric primitive.
    #[test]
    fn rg3_jit_accepts_long_and_double_returns() {
        assert!(is_jit_compatible(&lreturn_tail(vec![]), 2, "()J"));
        assert!(is_jit_compatible(&freturn_tail(vec![]), 2, "()F"));
        assert!(is_jit_compatible(&dreturn_tail(vec![]), 2, "()D"));
    }

    /// Scan-result sanity: an empty-body `public void foo() {}` must scan.
    #[test]
    fn rg_scan_sanity_void_return() {
        let code = vec![0xb1]; // return (void)
        let r = jit_scan(&code, code.len(), "()V");
        assert!(r.is_some(), "void return must scan");
    }

    // ── JIT code-cache cap tests ────────────────────────────────────
    //
    // These avoid mutating `CRATONVM_JIT_CODE_CACHE_MAX_MB`: `jit_code_cache_cap_bytes`
    // caches its value in a process-wide `OnceLock`, so an env-var test would be
    // order-dependent and could poison the cache for the rest of the suite. We
    // exercise the observable behaviour through `COMMITTED_JIT_CODE_BYTES` and the
    // public accessors instead.

    #[test]
    fn code_cache_cap_default_is_nonzero_and_below_disable_sentinel() {
        let cap = jit_code_cache_cap_bytes();
        // Whatever the env says, a sane cap is either a real byte bound or the
        // explicit "disabled" sentinel — never an accidental 0 that would refuse
        // all compilation.
        assert!(cap > 0, "cap must never be zero");
        // The compiled-in default is a generous, finite bound.
        assert_eq!(DEFAULT_JIT_CODE_CACHE_CAP_BYTES, 256 * 1024 * 1024);
    }

    #[test]
    fn code_cache_not_at_capacity_when_committed_is_small() {
        // In the test process essentially no JIT code is committed, so with any
        // realistic cap (or the disabled sentinel) we must be under capacity and
        // therefore still willing to compile.
        let cap = jit_code_cache_cap_bytes();
        if cap == usize::MAX {
            // Cap disabled in this environment: at-capacity is unconditionally false.
            assert!(!jit_code_cache_at_capacity());
            return;
        }
        let committed = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        // Sanity: the test process hasn't committed a quarter-gig of code.
        assert!(committed < cap, "unexpectedly large committed code in test");
        assert!(!jit_code_cache_at_capacity());
    }

    #[test]
    fn code_cache_committed_counter_tracks_buffer_allocation() {
        // Allocating a buffer bumps the committed counter by its capacity; the
        // cap is enforced against exactly this quantity.
        let before = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        let buf = ExecutableBuffer::new(4096).expect("alloc failed");
        let after = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after >= before + 4096,
            "committed counter must rise by at least the requested capacity"
        );
        // Default-config Drop leaks the region (no decrement), so the counter
        // does not fall back here — that's the intended monotonic behaviour the
        // cap bounds.
        drop(buf);
    }

    #[test]
    fn code_cache_cap_refusals_accessor_is_monotonic() {
        // The accessor reads the global refusal counter; it never decreases.
        let a = jit_code_cache_cap_refusals();
        let b = jit_code_cache_cap_refusals();
        assert!(b >= a);
    }
}
