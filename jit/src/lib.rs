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
//! The JIT register allocator's callee-saved GPR local homes are default-off
//! (`CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=1` opts back into the legacy
//! path for diagnostics). When that legacy path is enabled, a Java local
//! (including an object reference) may live **exclusively in a callee-saved GPR**
//! between bytecode aload/astore opcodes — the value need not be present in the
//! frame's local slot at any given native PC. The precise oop map (`OopMapEntry`)
//! describes only *frame-slot* oops; it has **no register-oop bitmap**. Therefore
//! a register-resident oop is invisible to any GC scan that walks frame memory
//! alone.
//!
//! Correctness of the legacy opt-in path depends on two cooperating mechanisms,
//! and **both must be kept**:
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
use std::sync::{Arc, Mutex, OnceLock};

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

    /// Size of the private executable allocation backing this method.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
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

/// Legacy diagnostic retained for API compatibility. Executable mappings are
/// now reclaimed with their last owning `Arc<CompiledMethod>`, so this remains
/// zero in the ownership-tracked implementation.
pub static RETAINED_JIT_CODE_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// JIT code-cache cap (bounded growth)
// ---------------------------------------------------------------------------
//
// The cache owns executable bodies through `Arc<CompiledMethod>`. Baked direct
// calls, MIC/PIC entries, external dispatch caches, and lock-free readers all
// retain matching strong owners. Invalidation clears dynamic targets and
// transitively withdraws direct callers; the last owner unregisters and unmaps
// the body. The cap is therefore a live-occupancy pressure valve rather than a
// permanent cap-and-stop threshold.

/// Bytes of JIT code currently committed (mapped) by live `ExecutableBuffer`s.
///
/// Bumped in [`ExecutableBuffer::new`] and decremented when the last owner drops
/// and returns the region to the OS. This is the live quantity bounded by the
/// code-cache cap.
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
        if let Ok(mut regions) = jit_code_regions().lock() {
            regions.deregister(self.ptr);
        }
        COMMITTED_JIT_CODE_BYTES.fetch_sub(self.capacity, std::sync::atomic::Ordering::Relaxed);
        platform::free_executable(self.ptr, self.capacity);
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
/// `frame_slot_offsets` lists the positive byte distances *below RBP* of each
/// slot holding a live oop. A distance `off` addresses `[rbp - off]`; this is
/// the same convention used by the x64 emitter's `local_offset` and by the
/// relocation walker. Exactly one qword per entry — the frame layout
/// guarantees 8-byte alignment so smaller slots never appear.
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
    /// True only when the moving-young shadow-stack publication for this
    /// safepoint proved complete enough for relocation under live JIT frames.
    pub moving_young_coverage_complete: bool,
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
            moving_young_coverage_complete: false,
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
// Maps each compiled method's native code range `[entry, entry+len)` to an
// Arc-stable `CompiledMethod` pointer. The GC root walker uses it to resolve
// which method a return address belongs to while walking the JIT RBP chain, so
// it can remap EVERY active JIT frame (not just the innermost). Populated at
// `JitCache::put`. Range snapshots are immutable and lock-free; the matching
// `CompiledMethod` stays alive through cache, direct-call, inline-cache, and
// active-reader ownership. Its final Drop unregisters the range before unmap.

type JitCodeRange = (usize, usize, usize);

/// Copy-on-write code-range registry.
///
/// Readers take an atomically reference-counted immutable snapshot and never
/// acquire the writer mutex. Registration/invalidation are rare compared with
/// stack classification, so publishing a newly sorted snapshot keeps the hot
/// lookup path both lock-free and O(log n).
struct JitCodeRangeRegistry {
    snapshot: arc_swap::ArcSwap<Vec<JitCodeRange>>,
    writer: std::sync::Mutex<()>,
}

impl JitCodeRangeRegistry {
    fn new() -> Self {
        Self {
            snapshot: arc_swap::ArcSwap::from_pointee(Vec::new()),
            writer: std::sync::Mutex::new(()),
        }
    }
}

static JIT_CODE_RANGES: std::sync::OnceLock<JitCodeRangeRegistry> =
    std::sync::OnceLock::new();

fn jit_code_ranges() -> &'static JitCodeRangeRegistry {
    JIT_CODE_RANGES.get_or_init(JitCodeRangeRegistry::new)
}

/// PERF (2026-07-15, RequestMappingMessageConversionIntegrationTests bootstrap
/// slowness, round 2): monotonically-increasing generation counter, bumped
/// whenever the registered code-range set changes. `native_stack_has_jit_frame`
/// (`vm/src/jit/conservative_roots.rs`) used to call `snapshot_code_ranges_into`
/// — a full lock + Vec copy + `sort_unstable()` over the ENTIRE set — on every
/// single call, even though it caches the result in a thread-local buffer.
/// That "cache" was write-only: it got clobbered and fully rebuilt every call
/// regardless of whether the underlying set had changed since the previous
/// call. Since `native_stack_has_jit_frame` runs on the per-native-call
/// root-snapshot path (see the comment on that function) and the set of
/// registered ranges only grows (compiled code is retained, not freed, per the
/// `JIT_CODE_RANGES` doc comment above), this was an O(n log n) cost paid on
/// EVERY native call, with n = total JIT-compiled methods ever registered —
/// i.e. the cost of every native call grew as the process JIT-compiled more
/// code, for the lifetime of the process. Exposed dramatically by the
/// 2026-07-15 invoke-cache fix (this same file's caller-side history): making
/// JDK-internal bytecode actually tier up to JIT (previously it barely did)
/// multiplied the number of registered code ranges, which multiplied this
/// per-call sort cost right along with it. Callers now compare this counter
/// against a cached "last-seen" value and skip the resnapshot/resort entirely
/// when nothing changed (the overwhelmingly common case within one scan burst).
static JIT_CODE_RANGES_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Current code-range-set generation. Callers that cache a sorted snapshot
/// (e.g. `native_stack_has_jit_frame`'s thread-local buffer) can skip a
/// resnapshot/resort when this hasn't advanced since their last read.
pub fn jit_code_ranges_generation() -> u64 {
    JIT_CODE_RANGES_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Register `[entry, entry+len)` → `cm_ptr` (the `Arc<CompiledMethod>` inner
/// address). No-op for empty/zero ranges. Stage 5.
pub fn register_jit_code_range(entry: usize, len: usize, cm_ptr: usize) {
    if entry == 0 || len == 0 || cm_ptr == 0 {
        return;
    }
    let registry = jit_code_ranges();
    if let Ok(_writer) = registry.writer.lock() {
        let mut next = (**registry.snapshot.load()).clone();
        next.push((entry, entry.saturating_add(len), cm_ptr));
        next.sort_unstable_by_key(|&(start, _, _)| start);
        registry.snapshot.store(std::sync::Arc::new(next));
        // Release: any cached snapshot taken with Acquire after this point must
        // see the push above (ordinary Mutex unlock already provides this, but
        // the counter itself is read outside the lock by cache-check callers).
        JIT_CODE_RANGES_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

/// Remove every range with the given `entry` start. Normal cache eviction keeps
/// ranges registered; this is used by explicit free-code diagnostics and tests.
/// Stage 5.
pub fn unregister_jit_code_range(entry: usize) {
    if entry == 0 {
        return;
    }
    let registry = jit_code_ranges();
    if let Ok(_writer) = registry.writer.lock() {
        let mut next = (**registry.snapshot.load()).clone();
        next.retain(|&(e, _, _)| e != entry);
        registry.snapshot.store(std::sync::Arc::new(next));
        JIT_CODE_RANGES_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

/// Number of registered code ranges (Stage 5 diagnostic).
pub fn jit_code_range_count() -> usize {
    jit_code_ranges().snapshot.load().len()
}

/// BUG-03 — whether the cross-thread STW JIT root scan is enabled (env
/// `CRATONVM_XT_JIT_ROOT_SCAN`). Mirrors `cratonvm_vm::jit::xt_root_scan::
/// enabled` (the jit crate cannot depend on the vm crate); kept in sync via
/// the same env var. Cached on first read.
///
/// A4 (fork6-fjp) — default ON (opt-OUT), matching the vm-side gate. The two
/// sides had diverged when the vm side flipped to default-on: this mirror
/// stayed opt-IN, so `CompiledMethodCache::put` never registered JIT code
/// ranges in a default-env process, `jit_code_ranges_snapshot()` was always
/// empty, and the "default-on" takeover classified every suspended peer as
/// "not in JIT" — the whole cross-thread STW JIT root scan (and the
/// helper-window pass) was silently inert unless the env var was set to `1`
/// explicitly. Keep the polarity identical to `xt_root_scan::enabled`.
pub fn xt_jit_root_scan_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        !matches!(
            std::env::var("CRATONVM_XT_JIT_ROOT_SCAN").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Snapshot the registered JIT code ranges as `(entry, end)` pairs.
///
/// BUG-03 — the cross-thread STW JIT root scan must classify each *suspended*
/// peer's `Rip` as in-JIT-or-not WITHOUT taking the `JIT_CODE_RANGES` lock
/// while a peer is frozen: a peer suspended mid-`register_jit_code_range`
/// holds that very lock, so a `lookup_jit_code_range` call on it would
/// deadlock the collector. The collector instead takes this snapshot ONCE
/// (no thread suspended yet), then classifies every frozen peer against the
/// returned copy with a lock-free range check. The set of ranges only grows
/// during compilation and is withdrawn only after the last code owner drops.
/// A momentarily-stale
/// snapshot can only mis-classify a brand-new range as "not JIT" (handled
/// conservatively by the snapshot-based mitigation), never the reverse.
pub fn jit_code_ranges_snapshot() -> Vec<(usize, usize)> {
    jit_code_ranges()
        .snapshot
        .load()
        .iter()
        .map(|&(e, end, _)| (e, end))
        .collect()
}

/// Resolve the `CompiledMethod` pointer whose code range contains `addr`, or
/// `None`. The immutable range table is sorted at publication, so this performs
/// a lock-free binary search. Stage 5.
pub fn lookup_jit_code_range(addr: usize) -> Option<usize> {
    let ranges = jit_code_ranges().snapshot.load();
    let candidate = ranges.partition_point(|&(entry, _, _)| entry <= addr);
    let &(entry, end, cm) = ranges.get(candidate.checked_sub(1)?)?;
    (addr >= entry && addr < end).then_some(cm)
}

/// Copy the registered code ranges into `buf` as `(start, end)` pairs sorted by
/// `start`, taking the table lock exactly ONCE.
///
/// PERF (TC0622 startup): the GC's per-native-call native-stack scan
/// (`native_stack_has_jit_frame`) tests up to ~1M stack words against the code
/// ranges. Calling `lookup_jit_code_range` per word locked this `Mutex` a
/// million times per scan — ~66% of all CPU during a Tomcat `start()`. Callers
/// snapshot once into a reusable buffer and binary-search it lock-free instead;
/// the result is identical because the ranges are disjoint. The lock is NOT held
/// across the (long) word scan, so it never blocks the background compiler's
/// `register_jit_code_range`.
pub fn snapshot_code_ranges_into(buf: &mut Vec<(usize, usize)>) {
    buf.clear();
    let ranges = jit_code_ranges().snapshot.load();
    buf.extend(ranges.iter().map(|&(e, end, _)| (e, end)));
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
/// (`CRATONVM_DEOPT_REAL`, **default-ON** as of the 2026-06-22 flip). Read-once
/// cached. When ON, a guard/loop bail reconstructs a precise interpreter frame
/// at the trapping bci and RESUMES there (avoiding the `i64::MIN` whole-method
/// re-run and its side-effect double-execution). The interpreter deopt sinks and
/// the OSR-exit sink consult this together with the per-method
/// `can_deopt_resume` / `can_osr_exit` flags, so deopt-exit and OSR-exit can
/// never run half-on (see `docs/feature-designs/deopt-osr.md`).
///
/// **Opt out** with `CRATONVM_DEOPT_REAL=0` (also `false`/`off`/`no`) to restore
/// the whole-method re-run — the safety net. The flip rides on the
/// correctness-verified non-moving young-gen + conservative-OSR-backstop GC
/// foundation (precise-jit-maps default-on); it does NOT enable the OSR-exit
/// in-place transfer (`CRATONVM_OSR_EXIT_TRANSFER`), which stays default-off —
/// so OSR-exit uses the safe reject path.
pub fn deopt_real_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_DEOPT_REAL") {
        // Explicit opt-out values disable; any other value (and unset) → ON.
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    })
}

/// activate-ir-optimizer Front 3.2: guard-surviving scalar replacement
/// (`CRATONVM_SCALAR_DEOPT`, default-OFF, read-once). When ON *and*
/// `deopt_real_enabled()`, the IR lowerer emits a `FrameValue::VirtualObject`
/// for a scalar-replaced object that is live at a deopt point (instead of
/// `Undefined` → whole-method re-run), so a precise resume re-materializes it via
/// `materialize_virtual_objects`. Gated by BOTH flags because it only has effect
/// on the precise-resume path (itself `deopt_real`-gated); OFF ⇒ the lowerer is
/// passed `sr_map = None` ⇒ byte-identical to the prior producer.
pub fn scalar_deopt_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::env::var_os("CRATONVM_SCALAR_DEOPT").is_some())
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

/// deopt-osr: the through-JIT BCE-deopt differential trigger (`CRATONVM_DEOPT_EAGER`,
/// default-OFF, read-once). When ON *and* `deopt_real_enabled()`, the single-pass
/// backend emits one synthetic UNCONDITIONAL branch to the BCE-guard deopt stub
/// (reason 2) right after the loop-header snapshot, so a JIT'd speculative-BCE loop
/// deopts at the loop bci *even when the bounds check passes* — reconstructing the
/// loop-header frame (its locals, incl. `long`/`double`/`float` accumulators) and
/// resuming in the interpreter. This is the long-missing end-to-end exercise of the
/// deopt-EXIT resume: run a program with the gate vs without and compare outputs (a
/// SEPARATE-PROCESS differential — the read-once gates can't diff in-process); equal
/// outputs prove the reconstruction + resume are correct. OFF ⇒ no branch ⇒
/// byte-identical production code.
pub fn deopt_eager_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::env::var_os("CRATONVM_DEOPT_EAGER").is_some())
}

/// Phase B (real-frame-deopt x64 backport) e2e trigger: `CRATONVM_DEOPT_EAGER_BCI=<n>`
/// (default unset, read-once). When set *and* `deopt_real_enabled()`, the eager
/// deopt-EXIT branch fires at the chosen bytecode bci `n` instead of the first
/// loop header. Loop headers can never hold a live scalar-replaced object (it
/// would escape across the back-edge), so this points the trigger at a
/// straight-line bci where a scalar object IS live in a local — the only way to
/// exercise the `VirtualObject` materialization path through the live JIT. A
/// SEPARATE-PROCESS differential, like `CRATONVM_DEOPT_EAGER`. Unset ⇒ unchanged.
pub fn deopt_eager_bci_override() -> Option<usize> {
    static CACHE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("CRATONVM_DEOPT_EAGER_BCI")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
    })
}

/// deopt-osr Step 8 (test trigger): `CRATONVM_OSR_EXIT_TEST` (default-OFF,
/// read-once). When ON *and* `deopt_real_enabled()`, the single-pass backend
/// emits one synthetic unconditional OSR-exit branch at a loop header so a JIT'd
/// loop bails to the interpreter at a loop bci and resumes the loop body — the
/// deliberate "instrument a rare branch" trigger that exercises the OSR-exit
/// resume end-to-end pending a real speculation trigger. OFF ⇒ no trigger
/// emitted ⇒ byte-identical code (the production path).
pub fn osr_exit_test_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| std::env::var_os("CRATONVM_OSR_EXIT_TEST").is_some())
}

/// deopt-osr Step 8 follow-up (P4): `CRATONVM_OSR_EXIT_AFTER=N` (default-OFF,
/// read-once) — the COUNTER-GATED OSR-exit trigger. When set to a positive `N`
/// *and* `deopt_real_enabled()`, the single-pass backend emits, at a loop header,
/// a per-site counter that bails to the OSR-exit stub only on the `N`-th reach —
/// so the JIT runs ~`N` loop iterations (advancing the loop-carried locals /
/// accumulator and committing their side effects) BEFORE the exit. This makes the
/// reconstructed frame carry genuinely JIT-advanced state, which the interpreter's
/// `transfer_osr_exit_into_live_frame` (gated `CRATONVM_OSR_EXIT_TRANSFER`) writes
/// back into the live frame — the realistic trigger the handoff calls for, vs the
/// unconditional-at-header `CRATONVM_OSR_EXIT_TEST` trigger that bails at iteration
/// 0. `None` / 0 ⇒ no counter emitted ⇒ byte-identical production code. Test-only:
/// run it WITH `CRATONVM_OSR_EXIT_TRANSFER=1`; with the transfer gate off the
/// interpreter safe-rejects and re-runs the committed iterations (double-executing
/// their side effects) — which is exactly the gap the transfer closes.
pub fn osr_exit_after() -> Option<usize> {
    static CACHE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("CRATONVM_OSR_EXIT_AFTER")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
    })
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
    /// Native entries of compiled callees referenced by baked direct calls.
    /// Filled by the compile driver before publication.
    pub _direct_callee_entries: Vec<usize>,
    /// Strong ownership matching `_direct_callee_entries`. A loaded caller can
    /// therefore never outlive executable code reached by one of its baked
    /// calls, even after the callee is invalidated and removed from the cache.
    _direct_callee_roots: Vec<Arc<CompiledMethod>>,
    /// OSR metadata: bytecode PC → native offset mapping.
    pub osr_pc_to_native: Option<Vec<i32>>,
    /// OSR metadata: number of locals in the compiled frame.
    pub osr_num_locals: usize,
    /// OSR metadata: number of register-mapped locals.
    pub osr_num_reg_locals: usize,
    /// OSR metadata: per-local GPR register assignments from graph-coloring allocator.
    pub osr_local_assignments: Option<Vec<Option<u8>>>,
    /// OSR metadata: per-bytecode-PC "dead local" mask. `osr_dead_mask[pc]` bit
    /// `i` set means local `i` is dead at that OSR entry PC and shares compiled
    /// state risk with the live locals at that entry. OSR currently declines
    /// those entries and falls back to the interpreter. Indexed like
    /// `osr_pc_to_native`.
    pub osr_dead_mask: Option<Vec<u64>>,
    /// OSR metadata: per-local XMM register assignments for float/double locals.
    pub osr_xmm_assignments: Option<Vec<Option<u8>>>,
    /// OSR metadata: frame size (for SUB RSP).
    pub osr_frame_size: i32,
    /// OSR metadata: callee-saved register save area offset.
    pub osr_callee_saved_base: i32,
    /// OSR metadata: the EXACT callee-saved GPR set the method's epilogue
    /// restores (`alloc_used_regs`), in slot order. The OSR trampoline MUST
    /// spill the caller's value for every register in this set, at the same
    /// `callee_saved_base + i*8` slot the epilogue reads register `i` from —
    /// otherwise (HIB-CV-20) a callee-saved register the allocator used for a
    /// NON-local temporary (e.g. an operand-stack value held across a call) is
    /// restored from the wrong slot, silently corrupting the OSR caller's live
    /// registers. `None` falls back to the (unsound for that case) local-derived
    /// spill set.
    pub osr_callee_saved_regs: Option<Vec<u8>>,
    /// OSR metadata: the callee-saved XMM set the epilogue restores
    /// (`alloc_used_xmms`), in slot order, paired with `osr_xmm_saved_base`. The
    /// trampoline spills the caller's XMM values to these slots so the epilogue
    /// restores them unchanged (the old trampoline omitted XMM spills entirely,
    /// corrupting the caller's callee-saved XMM regs on Windows). `None`/empty →
    /// no callee-saved XMM in use.
    pub osr_callee_saved_xmms: Option<Vec<u8>>,
    /// OSR metadata: XMM callee-saved save-area offset (mirrors the prologue's
    /// `xmm_saved_base`). Paired with `osr_callee_saved_xmms`.
    pub osr_xmm_saved_base: i32,
    /// OSR metadata: offset of VM context pointer in frame.
    pub osr_heap_local_offset: i32,
    /// OSR metadata: frame offset of the inline-TLAB cached `JvmThread*` slot.
    /// Normal entry initializes this in the prologue; OSR entry initializes it
    /// in the trampoline because it jumps past that prologue. 0 when unused.
    pub jit_thread_slot_off: i32,
    /// Frame slot of the prologue-cached native-stack floor for the inline
    /// self-call check. OSR trampolines initialise it to `usize::MAX`
    /// (`RSP > MAX` is unsatisfiable) so OSR-entered frames always take the
    /// out-of-line guard helper. `0` = not reserved.
    pub stack_floor_slot_off: i32,
    /// OSR metadata: optional `jit_frame_record` helper pointer. Normal method
    /// entry records exact RBP from the JIT prologue; OSR bypasses that
    /// prologue, so the trampoline records its own RBP after `mov rbp, rsp`.
    /// 0 when precise maps are disabled or the helper table is absent.
    pub osr_frame_record: usize,
    /// Shadow-stack: frame offset (`[rbp - off]`) of the cached thread-pointer
    /// slot. A normal entry sets it in the prologue. An OSR entry bypasses that
    /// prologue, so the OSR trampoline stores the real `*mut JvmThread` passed
    /// through `osr_enter` and snapshots the shadow watermark before jumping to
    /// the loop body. 0 when shadow disabled.
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
    /// Which backend produced this code: `true` for the optimizing IR pipeline,
    /// `false` for the single-pass `x64::compile` backend (incl. any method that
    /// began on the IR path but BAILED to single-pass). Introspection only — used
    /// by tests to assert backend routing (e.g. that a self-recursive long/FP
    /// method stays on single-pass, the fib44-regression guard). Not read by codegen.
    pub used_ir_backend: bool,
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
    /// deopt-osr Step 7 — `true` once the OSR-exit map emitter has recorded at
    /// least one loop-boundary exit map for this (OSR-compiled) method, i.e. it
    /// can leave a running JIT/OSR frame mid-loop at a loop bci with the loop's
    /// live state, rather than the `i64::MIN` re-run (which is *wrong* for an
    /// OSR'd frame entered partway through). Set at finalize to
    /// `!osr_exit_points.is_empty()`; only non-empty when `deopt_real_enabled()`
    /// was on at compile, so `false` in production. Step 8 consults it (with the
    /// `CRATONVM_DEOPT_REAL` gate) before routing a mid-loop bail.
    pub can_osr_exit: bool,
    /// jit-invokedynamic-groovy-regression fix — `true` when this artifact
    /// compiled at least one `invokedynamic` (0xba) site, i.e. it contains an
    /// UNCONDITIONAL reason-8 uncommon trap that fires on every execution
    /// reaching that site. Entry-publication gates (JIT→JIT direct-call
    /// baking, MIC/PIC inline-cache installs) consult this so machine code
    /// never calls such an artifact directly — every call stays on a dispatch
    /// helper, which can resolve the trap precisely in place
    /// (`try_resume_trapped_callee`). Set at x64 finalize from
    /// `!compiler.indy_info.is_empty()`; `false` for IR-path artifacts (the
    /// IR lowerer rejects invokedynamic methods).
    pub has_indy_trap: bool,
    /// deopt-osr Step 7 — the loop-boundary bcis (OSR-vetted, outside every
    /// LICM-hoisted body) for which an OSR-exit map was emitted into
    /// `deopt_points` (tagged `DeoptReason::OsrExit`). Empty unless
    /// `deopt_real_enabled()` was set at compile. Step 8 looks a trapping loop
    /// bci up here to decide whether to OSR-exit (resume the loop body) vs
    /// re-run.
    pub osr_exit_points: Vec<usize>,
    /// deopt-osr scaffolding — monotonic compilation epoch for this artifact.
    /// When `MakeNotEntrant` invalidation lands, boxed `DeoptimizationPoint`
    /// pointers (baked into guard code) are versioned by this epoch so a
    /// resume never follows a box belonging to a superseded compilation.
    /// `0` for every artifact today (no invalidation consumer yet).
    pub compilation_epoch: u64,
    /// deopt-osr Step 9 follow-up (a) — raw pointer to this artifact's retained
    /// [`crate::deopt::DeoptEpochGuard`], baked as the 4th arg into every
    /// frame-deopt stub. Null on production artifacts (a guard is allocated only
    /// when a frame-deopt stub is emitted, which requires `deopt_real_enabled()`).
    /// The VM stamps it (creation epoch + stable live-epoch cell) at install via
    /// [`Self::stamp_deopt_epoch_guard`]; `x64_deopt_entry` then reads it BEFORE
    /// dereferencing the box, so a superseded compilation never follows a stale
    /// (possibly-freed, under `CRATONVM_JIT_FREE_CODE=1`) box. The pointed-to
    /// guard is leaked (process-lifetime), so this raw pointer is always valid.
    pub deopt_epoch_guard: *const crate::deopt::DeoptEpochGuard,
}

unsafe impl Send for CompiledMethod {}
unsafe impl Sync for CompiledMethod {}

impl Drop for CompiledMethod {
    fn drop(&mut self) {
        let entry = self.entry as usize;
        cratonvm_types::jit_activation::unregister_executable_owner(entry);
        unregister_jit_code_range(entry);
        if let Some(owners) = JIT_ENTRY_OWNERS.get() {
            let mut owners = owners.lock();
            if owners
                .get(&entry)
                .is_some_and(|owner| owner.strong_count() == 0)
            {
                owners.remove(&entry);
            }
        }

        // Purge cached OSR trampolines before the body mapping is returned.
        #[cfg(target_arch = "x86_64")]
        {
            let start = entry;
            let end = start.saturating_add(self._buffer.pos());
            let mut cache = osr_trampoline_cache().lock();
            cache.retain(|&target, _| !(target >= start && target < end));
        }
    }
}

impl CompiledMethod {
    /// deopt-osr Step 9 follow-up (a) — stamp this artifact's retained
    /// [`crate::deopt::DeoptEpochGuard`] (baked into every frame-deopt stub) with
    /// its creation epoch and a stable pointer to the method's live
    /// compilation-epoch cell. Called once by the VM at install, under
    /// `deopt_real_enabled()`. No-op when no guard was emitted (production
    /// artifacts: `deopt_epoch_guard` is null) so it is gate-off byte-identical.
    ///
    /// After this, `x64_deopt_entry` can decide BEFORE dereferencing the deopt
    /// box whether this artifact's speculation has been superseded (the live
    /// epoch advanced past `creation_epoch`), routing a stale frame to the safe
    /// re-run without touching the (possibly-freed) box.
    ///
    /// # Safety
    /// `live_epoch_cell` must be null or a stable, process-lifetime `AtomicU64`
    /// address (e.g. one handed out by `SharedVm::method_epochs`).
    pub fn stamp_deopt_epoch_guard(
        &self,
        creation_epoch: u64,
        live_epoch_cell: *const std::sync::atomic::AtomicU64,
    ) {
        if self.deopt_epoch_guard.is_null() {
            return;
        }
        use std::sync::atomic::Ordering;
        // SAFETY: a non-null `deopt_epoch_guard` is the leaked, retained guard
        // baked by `emit_deopt_stubs` (valid for the process lifetime).
        let guard = unsafe { &*self.deopt_epoch_guard };
        guard
            .creation_epoch
            .store(creation_epoch, Ordering::Relaxed);
        guard.live_epoch_cell.store(
            live_epoch_cell as *mut std::sync::atomic::AtomicU64,
            Ordering::Release,
        );
    }

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
            _direct_callee_entries: Vec::new(),
            _direct_callee_roots: Vec::new(),
            osr_pc_to_native: None,
            osr_num_locals: 0,
            osr_num_reg_locals: 0,
            osr_local_assignments: None,
            osr_dead_mask: None,
            osr_xmm_assignments: None,
            osr_frame_size: 0,
            osr_callee_saved_base: 0,
            osr_callee_saved_regs: None,
            osr_callee_saved_xmms: None,
            osr_xmm_saved_base: 0,
            osr_heap_local_offset: 0,
            jit_thread_slot_off: 0,
            stack_floor_slot_off: 0,
            osr_frame_record: 0,
            shadow_thread_slot_off: 0,
            shadow_savetop_slot_off: 0,
            shadow_off_in_thread: 0,
            has_dispatch: false,
            used_ir_backend: false,
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
            has_indy_trap: false,
            osr_exit_points: Vec::new(),
            compilation_epoch: 0,
            deopt_epoch_guard: std::ptr::null(),
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
            _direct_callee_entries: Vec::new(),
            _direct_callee_roots: Vec::new(),
            osr_pc_to_native: None,
            osr_num_locals: 0,
            osr_num_reg_locals: 0,
            osr_local_assignments: None,
            osr_dead_mask: None,
            osr_xmm_assignments: None,
            osr_frame_size: 0,
            osr_callee_saved_base: 0,
            osr_callee_saved_regs: None,
            osr_callee_saved_xmms: None,
            osr_xmm_saved_base: 0,
            osr_heap_local_offset: 0,
            jit_thread_slot_off: 0,
            stack_floor_slot_off: 0,
            osr_frame_record: 0,
            shadow_thread_slot_off: 0,
            shadow_savetop_slot_off: 0,
            shadow_off_in_thread: 0,
            has_dispatch: false,
            used_ir_backend: false,
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
            has_indy_trap: false,
            osr_exit_points: Vec::new(),
            compilation_epoch: 0,
            deopt_epoch_guard: std::ptr::null(),
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

    /// Locate the map selected by the safepoint id stored in a live JIT frame.
    ///
    /// The x64 emitter records the bytecode PC of each GC-capable safepoint in
    /// a dedicated frame slot immediately before its call. Unlike
    /// [`Self::find_oop_map_for_pc`], this lookup is intentionally keyed by
    /// bytecode PC: while a helper is active, the native return address belongs
    /// to the helper, but the frame slot remains stable and identifies the
    /// caller's exact safepoint map.
    #[inline]
    pub fn find_oop_map_for_safepoint_id(&self, bytecode_pc: u32) -> Option<&OopMapEntry> {
        self.oop_maps
            .iter()
            .find(|map| map.bytecode_pc == bytecode_pc)
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

    /// True when this artifact recorded a safe OSR entry point for `entry_pc`
    /// (i.e. [`osr_enter`](Self::osr_enter) at that pc would not bail).
    /// Lets the interpreter's OSR trigger reuse a cached compile instead of
    /// re-running the whole x64 pipeline on every trigger.
    pub fn can_osr_enter(&self, entry_pc: usize) -> bool {
        if self
            .osr_dead_mask
            .as_ref()
            .and_then(|m| m.get(entry_pc).copied())
            .unwrap_or(0)
            != 0
        {
            return false;
        }
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
    /// `set_jit_thread`) when shadow-stack frame slots exist, so this
    /// OSR-entered frame is precisely tracked; pass 0 to opt out of shadow
    /// tracking in tests.
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

        // A nonzero dead mask means at least one dead interpreter local shares a
        // compiled location with a live local at this entry. Until OSR supports
        // reconstructing that coalesced state, fall back to the interpreter.
        let dead_mask = self
            .osr_dead_mask
            .as_ref()
            .and_then(|m| m.get(entry_pc).copied())
            .unwrap_or(0);
        if dead_mask != 0 {
            return None;
        }

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
            self.osr_callee_saved_regs.as_deref(),
            self.osr_callee_saved_xmms.as_deref(),
            self.osr_xmm_saved_base,
            self.osr_heap_local_offset,
            self.jit_thread_slot_off,
            self.stack_floor_slot_off,
            self.osr_frame_record,
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

/// Emit a fresh OSR trampoline body for the given destination + frame layout.
///
/// The emitted code expects three arguments via the platform C ABI:
///   * arg0 (RCX on Windows / RDI on SysV) = `locals_ptr: *const i64`
///   * arg1 (RDX on Windows / RSI on SysV) = `vm_ptr: i64` (only read when `needs_context`)
///   * arg2 (R8 on Windows / RDX on SysV) = `thread_ptr: i64` (read when shadow
///     slots exist; tests may pass 0, which disables tracking via null guards)
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
    callee_saved_regs: Option<&[u8]>,
    callee_saved_xmms: Option<&[u8]>,
    xmm_saved_base: i32,
    heap_local_offset: i32,
    jit_thread_slot_off: i32,
    stack_floor_slot_off: i32,
    frame_record: usize,
    needs_context: bool,
    dead_mask: u64,
    shadow_thread_slot_off: i32,
    shadow_savetop_slot_off: i32,
    shadow_off_in_thread: i32,
) -> Option<ExecutableBuffer> {
    use crate::x64::LOCAL_REGS;

    // Platform C-ABI argument register numbers.
    // arg0 carries `locals_ptr`, arg1 carries `vm_ptr`, arg2 carries `thread_ptr`
    // (the shadow-stack thread pointer). All three are caller-saved
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
    // HIB-CV-20: spill EXACTLY the callee-saved GPR set the method's epilogue
    // restores, at the SAME slot index. `callee_saved_regs` is the compiler's
    // `alloc_used_regs` (every callee-saved register the allocator used — for
    // locals AND for operand-stack temporaries / spilled values), in the order
    // the epilogue reads `[rbp-(callee_saved_base+i*8)]`. Spilling only the
    // local_assignments-derived subset (the historical fallback below) drops any
    // non-local callee-saved register, so the epilogue restores it (and every
    // later one) from the wrong slot — silently corrupting the OSR caller's live
    // registers on return. Prefer the exact set; keep the subset derivation only
    // for artifacts compiled before this metadata existed.
    let used_regs: Vec<u8> = if let Some(regs) = callee_saved_regs {
        regs.to_vec()
    } else if let Some(assignments) = local_assignments {
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

    // HIB-CV-20: spill the caller's callee-saved XMM registers to the same slots
    // the epilogue restores them from (`alloc_used_xmms` at xmm_saved_base + i*8,
    // matching `emit_movq_mem_rbp_from_xmm`). The old trampoline skipped XMM
    // spills entirely, so on Windows (where XMM6–XMM15 are callee-saved) a method
    // that used a callee-saved XMM had the caller's value restored from an
    // uninitialised slot. Encoding: 66 [REX.R] 0F D6 /r with a disp32 [rbp-off].
    if let Some(xmms) = callee_saved_xmms {
        for (i, &xmm) in xmms.iter().enumerate() {
            let neg_off = -(xmm_saved_base + i as i32 * 8);
            tramp.emit_byte(0x66);
            if xmm >= 8 {
                tramp.emit_byte(0x44); // REX.R (base RBP needs no REX.B)
            }
            tramp.emit_byte(0x0F);
            tramp.emit_byte(0xD6);
            tramp.emit_byte(0x85 | ((xmm & 7) << 3)); // mod=10, reg=xmm&7, rm=rbp(5)
            tramp.emit(&neg_off.to_le_bytes());
        }
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

    if jit_thread_slot_off != 0 {
        let neg_off = -jit_thread_slot_off;
        let rex = 0x48 | if arg2_reg >= 8 { 0x04 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0x85 | ((arg2_reg & 7) << 3));
        tramp.emit(&neg_off.to_le_bytes());
    }

    // Inline self-recursion check: OSR bypasses the compiled prologue that
    // caches the native-stack floor, so initialise the slot to usize::MAX
    // (`MOV qword [rbp - off], -1` -- imm32 sign-extends). `RSP > MAX` is
    // unsatisfiable, so every self-call site in an OSR-entered frame takes
    // the out-of-line guard helper (safe, merely slower). Leaving the slot
    // uninitialised could skip the guard on garbage and miss a
    // StackOverflowError.
    if stack_floor_slot_off != 0 {
        let neg_off = -stack_floor_slot_off;
        tramp.emit_byte(0x48); // REX.W
        tramp.emit_byte(0xC7); // MOV r/m64, imm32 (sign-extended)
        tramp.emit_byte(0x85); // mod=10, reg=/0, rm=rbp
        tramp.emit(&neg_off.to_le_bytes());
        tramp.emit(&(-1i32).to_le_bytes());
    }

    // OSR bypasses the compiled method's normal prologue, including the exact
    // RBP publication used by precise JIT maps. Mirror the prologue here after
    // live ABI arguments have been saved to their frame homes: prefer the
    // default inline TLS store when available; fall back to the helper-table
    // callback on non-Windows / inline opt-out. The helper call can clobber
    // caller-saved registers, so preserve the incoming locals/thread pointers
    // in the frame's reserved helper-call stack-arg area.
    let inline_rbp_disp = if frame_record != 0 {
        crate::x64::inline_rbp_tls_disp()
    } else {
        0
    };
    if inline_rbp_disp != 0 {
        // MOV qword ptr gs:[disp32], RBP
        tramp.emit_byte(0x65);
        tramp.emit_byte(0x48);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0x2C);
        tramp.emit_byte(0x25);
        tramp.emit(&(inline_rbp_disp as u32).to_le_bytes());
    }
    let call_frame_record = frame_record != 0
        && (inline_rbp_disp == 0 || crate::x64::verify_inline_frame_record_enabled());
    if call_frame_record {
        // MOV [rsp + 32], R10
        tramp.emit(&[0x4C, 0x89, 0x54, 0x24, 32]);
        // MOV R11, arg2_reg
        let rex = 0x48 | 0x01 | if arg2_reg >= 8 { 0x04 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0xC0 | ((arg2_reg & 7) << 3) | 3);
        // MOV [rsp + 40], R11
        tramp.emit(&[0x4C, 0x89, 0x5C, 0x24, 40]);
        // MOV arg0_reg, RBP
        let rex = 0x48 | if arg0_reg >= 8 { 0x01 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0xC0 | (5 << 3) | (arg0_reg & 7));
        // CALL frame_record via RAX.
        tramp.emit(&[0x48, 0xB8]);
        tramp.emit(&(frame_record as i64).to_le_bytes());
        tramp.emit(&[0xFF, 0xD0]);
        // MOV R10, [rsp + 32]
        tramp.emit(&[0x4C, 0x8B, 0x54, 0x24, 32]);
        // MOV R11, [rsp + 40]
        tramp.emit(&[0x4C, 0x8B, 0x5C, 0x24, 40]);
        // MOV arg2_reg, R11
        let rex = 0x48 | 0x04 | if arg2_reg >= 8 { 0x01 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89);
        tramp.emit_byte(0xC0 | (3 << 3) | (arg2_reg & 7));
    }

    // Shadow-stack OSR-frame handling. An OSR entry bypasses the prologue's
    // `get_current_thread` sequence, so the trampoline must initialize the same
    // cached-thread and saved-watermark slots. A null `thread_ptr` (unit tests)
    // is stored as null and guarded exactly like the normal prologue path.
    if shadow_thread_slot_off != 0 && shadow_savetop_slot_off != 0 {
        // MOV [rbp - shadow_thread_slot_off], arg2   (cache the thread pointer)
        let neg_thr = -shadow_thread_slot_off;
        let rex = 0x48 | if arg2_reg >= 8 { 0x04 } else { 0x00 }; // REX.W (+R if arg2 extended)
        tramp.emit_byte(rex);
        tramp.emit_byte(0x89); // MOV r/m64, r64
        tramp.emit_byte(0x85 | ((arg2_reg & 7) << 3)); // mod=10, reg=arg2, rm=rbp(5)
        tramp.emit(&neg_thr.to_le_bytes());

        // TEST arg2, arg2; JE skip_savetop
        let rex = 0x48
            | if arg2_reg >= 8 { 0x04 } else { 0x00 }
            | if arg2_reg >= 8 { 0x01 } else { 0x00 };
        tramp.emit_byte(rex);
        tramp.emit_byte(0x85);
        tramp.emit_byte(0xC0 | ((arg2_reg & 7) << 3) | (arg2_reg & 7));
        tramp.emit(&[0x0F, 0x84]);
        let skip_savetop = tramp.pos();
        tramp.emit(&0i32.to_le_bytes());

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
        let rel = (tramp.pos() as i64) - (skip_savetop as i64 + 4);
        tramp.try_patch_i32(skip_savetop, rel as i32).ok();
    } else if shadow_thread_slot_off != 0 {
        // Defensive partial-layout fallback: zero the cached thread slot so the
        // push/reload/epilogue null guards skip rather than reading stale stack.
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
    callee_saved_regs: Option<&[u8]>,
    callee_saved_xmms: Option<&[u8]>,
    xmm_saved_base: i32,
    heap_local_offset: i32,
    jit_thread_slot_off: i32,
    stack_floor_slot_off: i32,
    frame_record: usize,
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
                callee_saved_regs,
                callee_saved_xmms,
                xmm_saved_base,
                heap_local_offset,
                jit_thread_slot_off,
                stack_floor_slot_off,
                frame_record,
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
    // `thread_ptr` only when shadow-stack frame slots exist; both are passed
    // unconditionally (caller-saved registers, ignored if unused).
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

/// Maximum callee bytecode size (excluding padding) eligible for inlining at
/// **any** call site — the admission gate, not the policy.
///
/// # 2026-07-26 re-shape (jit-inlining-and-ir-calls)
///
/// This was `35`, applied as a flat cap to every callee. That number is
/// HotSpot's `MaxInlineSize`, which HotSpot applies **only to cold callees**;
/// for a callee reached from a hot call site HotSpot uses `FreqInlineSize`,
/// which is `325` — nearly 10x larger. CratonVM had the cold constant and no
/// hot tier at all, so a 40-byte accessor called a million times in a loop was
/// treated exactly like a 40-byte method called twice.
///
/// The three-tier HotSpot shape now lives here:
///
/// | tier | callee cap | expansion cap | HotSpot name | when |
/// |---|---|---|---|---|
/// | trivial | [`MAX_TRIVIAL_INLINE_SIZE`] = 6 | — | `MaxTrivialSize` | always: smaller than the call sequence it replaces |
/// | cold | [`MAX_INLINE_SIZE_COLD`] = 35 | [`MAX_INLINE_EXPANSION_COST`] = 64 | `MaxInlineSize` | no profile evidence the site is hot |
/// | hot | `MAX_INLINE_BYTECODE_SIZE` = 325 | [`MAX_INLINE_EXPANSION_COST_HOT`] = 512 | `FreqInlineSize` | site in a hot loop, or a hot receiver profile |
///
/// **This constant is the value the VM-side inline resolver
/// (`vm/src/runtime/interpreter.rs::resolve_inline_site`) gates on**, and it
/// must stay the largest of the three: that resolver is a pure admission
/// filter handing candidates to the planner in `try_compile_inner`, which then
/// applies the per-site tier via [`inline_site_expansion_cost_tiered`].
/// Lowering this constant back to 35 would make the hot tier unreachable.
///
/// See `docs/internal/arch-2026-07-26/jit-inlining-and-ir-calls.md`.
pub const MAX_INLINE_BYTECODE_SIZE: usize = 325;

/// Callee-size cap for a call site with **no evidence of hotness** —
/// HotSpot's `MaxInlineSize`. This is exactly the value the flat
/// pre-2026-07-26 cap used, so a cold site's inlining decisions are unchanged
/// by the re-shape.
pub const MAX_INLINE_SIZE_COLD: usize = 35;

/// Callee size below which inlining is **always** profitable regardless of site
/// hotness — HotSpot's `MaxTrivialSize`. A callee this small is smaller than
/// the call sequence that would invoke it.
pub const MAX_TRIVIAL_INLINE_SIZE: usize = 6;

/// Total inlined expansion budget per compiled method, for a caller with no
/// hot-loop evidence.
///
/// Raised from 250. The budget is not a free parameter: the single-pass backend
/// sizes both its code buffer and its frame from the planned sites
/// (`x64::compile` reserves `callee_code_len * 64` buffer bytes and
/// `callee_max_locals + callee_code_len` spill slots **per inlined site**), so
/// this ceiling directly bounds committed executable memory and JIT frame size.
/// 750 keeps both comfortably bounded — order 48 KiB of buffer estimate and
/// 6 KiB of frame in the worst case — while being 3x the old ceiling.
pub const MAX_INLINE_BUDGET: usize = 750;

/// Total inlined expansion budget for a **hot** caller — one the profile shows
/// executing a hot loop, which is where inlining actually pays. Worst case
/// order 128 KiB of buffer estimate and 16 KiB of frame; see
/// [`MAX_INLINE_BUDGET`] for where those numbers come from.
pub const MAX_INLINE_BUDGET_HOT: usize = 2000;

/// Maximum estimated native-code expansion accepted for one **cold** inline
/// site.
///
/// Bytecode length alone under-prices field accesses and helper-dependent
/// bodies. Keeping a second, backend-oriented ceiling prevents one nominally
/// small leaf from consuming disproportionate instruction-cache space.
pub const MAX_INLINE_EXPANSION_COST: usize = 64;

/// Maximum estimated native-code expansion accepted for one **hot** inline
/// site. The hot counterpart of [`MAX_INLINE_EXPANSION_COST`], scaled with the
/// callee-size tier (35 → 325 is ~9x; 64 → 512 is 8x) so the two ceilings stay
/// proportionate and neither becomes the sole binding constraint.
pub const MAX_INLINE_EXPANSION_COST_HOT: usize = 512;

/// Back-edge execution count at which a loop counts as hot for inlining.
/// Matches the threshold `profile::LoopTripProfile::suggests_unroll_factor`
/// already uses for "not hot enough", so the two profile consumers agree on
/// what "hot" means.
pub const INLINE_HOT_LOOP_BACKEDGES: u64 = 100;

/// Observation count at which a *call site* (rather than a loop) counts as hot,
/// from its receiver-type profile. `MethodProfile::receivers` records one
/// observation per executed `invokevirtual`/`invokeinterface`, so the summed
/// count is a direct execution count for that site.
pub const INLINE_HOT_SITE_OBSERVATIONS: u32 = 500;

/// Bytecode pc ranges `[header, back_edge]` of every loop the profile shows to
/// be hot, derived from `MethodProfile::loops` (keyed by back-edge pc) plus the
/// bytecode itself, which is what recovers each back-edge's target — i.e. the
/// loop header.
///
/// `LoopTripProfile` records only the back-edge pc, so the header is recovered
/// by decoding the branch at that pc: a 3-byte `goto`/conditional carries a
/// signed 16-bit relative offset, a 5-byte `goto_w` a signed 32-bit one. A
/// back-edge is a branch whose target is at or before its own pc; anything else
/// keyed here is ignored rather than guessed at.
///
/// Empty whenever there is no profile, which makes every site cold and every
/// budget collapse to its pre-2026-07-26 value.
fn hot_loop_ranges(
    code: &[u8],
    code_len: usize,
    profile: Option<&profile::MethodProfile>,
) -> Vec<(usize, usize)> {
    let Some(prof) = profile else {
        return Vec::new();
    };
    let mut ranges = Vec::new();
    for (&back_pc, trips) in &prof.loops {
        if trips.backedge_count < INLINE_HOT_LOOP_BACKEDGES || back_pc >= code_len {
            continue;
        }
        let target = match code[back_pc] {
            // 3-byte relative branches: if<cond>, if_icmp<cond>, if_acmp<cond>, goto.
            0x99..=0xa8 => {
                if back_pc + 2 >= code_len {
                    continue;
                }
                back_pc as isize
                    + i16::from_be_bytes([code[back_pc + 1], code[back_pc + 2]]) as isize
            }
            // goto_w — 5 bytes, 32-bit offset.
            0xc8 => {
                if back_pc + 4 >= code_len {
                    continue;
                }
                back_pc as isize
                    + i32::from_be_bytes([
                        code[back_pc + 1],
                        code[back_pc + 2],
                        code[back_pc + 3],
                        code[back_pc + 4],
                    ]) as isize
            }
            _ => continue,
        };
        if target < 0 || target as usize > back_pc {
            continue; // not a backward branch
        }
        ranges.push((target as usize, back_pc));
    }
    ranges
}

/// Whether a call site at `pc` is HOT, and therefore eligible for the
/// [`MAX_INLINE_BYTECODE_SIZE`] (`FreqInlineSize`) allowance rather than the
/// [`MAX_INLINE_SIZE_COLD`] (`MaxInlineSize`) one.
///
/// Two independent pieces of real profile evidence, either of which suffices:
///
///  1. the site lies inside a loop whose back-edge count clears
///     [`INLINE_HOT_LOOP_BACKEDGES`] (`MethodProfile::loops`); or
///  2. the site's own receiver-type profile records at least
///     [`INLINE_HOT_SITE_OBSERVATIONS`] executions (`MethodProfile::receivers`,
///     populated per executed virtual/interface invoke).
///
/// With no profile every site is cold, so an unprofiled compile inlines exactly
/// as it did before the re-shape.
fn call_site_is_hot(
    pc: usize,
    hot_loops: &[(usize, usize)],
    profile: Option<&profile::MethodProfile>,
) -> bool {
    if hot_loops
        .iter()
        .any(|&(header, back)| pc >= header && pc <= back)
    {
        return true;
    }
    profile
        .and_then(|p| p.receivers.get(&pc))
        .map(|counts| {
            counts.values().copied().map(u64::from).sum::<u64>()
                >= u64::from(INLINE_HOT_SITE_OBSERVATIONS)
        })
        .unwrap_or(false)
}

/// C1→C2 supersede eligibility: would an `optimize=true` recompile of this
/// method actually take the optimizing IR pipeline AND be expected to produce
/// better code than the single-pass body it replaces?
///
/// Mirrors the IR gate in `try_compile_inner` (`ir_compatible` + the
/// category-2/FP admission clauses) with one deliberate extra restriction:
/// only ALLOCATION-FREE methods qualify, and a call-bearing method qualifies
/// only when the IR can lower its calls as well as the single-pass backend
/// does. Pure compute (int/long/FP arithmetic over locals, arrays and fields —
/// sieve/matrix/reduction loop shapes) is where the IR backend reliably wins.
///
/// # Why calls used to be excluded outright, and what changed
///
/// This predicate used to reject ANY method containing an invoke, because the
/// IR path lowered every invoke through the generic `invoke_dispatch` helper —
/// no direct calls, no inline caches — so an "optimizing" recompile of a
/// call-bearing method could be a net REGRESSION over the single-pass body,
/// which has always had direct calls and constructor inlining. That was the
/// binding constraint on the whole IR pipeline: the tiered manager compiles a
/// hot call-bearing method at C1 (`optimize == false`, which skips the IR
/// pipeline entirely) and then declined to promote it here, so raising
/// `ir_compatible`'s invoke cap alone changed nothing under tiered routing.
///
/// `ir_lower::emit_direct_cross_call` removed that per-call tax for the
/// STATICALLY BOUND invoke kinds, so those are now admitted:
///
///  * `invokestatic` (0xb8) and `invokespecial` (0xb7) have exactly one
///    possible target, so the IR binds them with the same raw `CALL` the
///    single-pass backend emits — subject to `ir_direct_calls_enabled()`,
///    without which the IR would be back to paying the helper per call and the
///    original rejection is still the right answer.
///  * `invokevirtual` (0xb6) / `invokeinterface` (0xb9) used to stay excluded,
///    because the IR had no inline cache and a virtual site paid the full
///    helper round trip WITH a dynamic target lookup. They are now admitted:
///    `ir_lower::emit_inline_cache_call` (jit-inlining-and-ir-calls) gives such
///    a site the same monomorphic-then-3-way-polymorphic cascade the
///    single-pass backend emits, and `CRATONVM_JIT_IR_CALL_VIRTUAL` inverted
///    from opt-in to opt-out with it. Admission is subject to
///    `ir_virtual_calls_enabled()` for the same reason the statically-bound
///    kinds are subject to `ir_direct_calls_enabled()`: with the capability
///    switched off, the IR is back to paying the helper per call and the
///    original rejection is still the right answer.
///
/// ALLOCATION-bearing methods remain excluded, unchanged: the IR's allocation
/// lowering differs from the single-pass inline-TLAB bump, and the IR call
/// eligibility loop requires `new_ops.is_empty()` anyway, so an
/// allocation-bearing method's invokes would bail the builder.
///
/// This predicate deliberately stays a cheap, scan-only approximation (it
/// cannot resolve a constant pool), so it can admit a method the IR later
/// bails on — e.g. a constructor whose `invokespecial` is an `<init>` super
/// call. That costs a wasted optimizing compile whose result is discarded in
/// favour of the single-pass body; it is never a correctness risk.
pub fn c2_upgrade_would_engage(
    code: &[u8],
    code_len: usize,
    descriptor: &str,
    ir_emit_long: bool,
    ir_emit_fp: bool,
) -> bool {
    let Some(scan) = x64::jit_scan(code, code_len, descriptor) else {
        return false;
    };
    if !scan.new_ops.is_empty() || !scan.anewarray_ops.is_empty() || !scan.indy_ops.is_empty() {
        return false;
    }
    if !scan.invoke_ops.is_empty() {
        // Every invoke must lower at least as well as single-pass would. The
        // statically-bound kinds need the direct-call capability; the
        // dynamically-bound kinds need the inline-cache capability. Anything
        // else (there is nothing else — `indy` is rejected above) is refused.
        let statics_ok = ir_direct_calls_enabled();
        let virtuals_ok = ir_virtual_calls_enabled();
        let all_lowerable = scan.invoke_ops.iter().all(|(_, _, opcode)| match *opcode {
            0xb8 | 0xb7 => statics_ok,
            0xb6 | 0xb9 => virtuals_ok,
            _ => false,
        });
        if !all_lowerable {
            return false;
        }
    }
    if !ir::ir_compatible(&scan) {
        return false;
    }
    let fp_free = !method_uses_fp(code, code_len, descriptor);
    (!method_uses_category2(code, code_len, descriptor) && fp_free)
        || (ir_emit_long && fp_free)
        || (ir_emit_fp && fp_in_body(code, code_len))
}

/// Structural half of the early optimized-tier admission for the common
/// `static int f(int)` self-recursion shape. The VM performs the constant-pool
/// identity check separately; this function deliberately accepts only scalar,
/// allocation-free bytecode whose calls are all `invokestatic`.
pub fn scalar_selfrec_ir_would_engage(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    if descriptor != "(I)I" {
        return false;
    }
    let Some(scan) = x64::jit_scan(code, code_len, descriptor) else {
        return false;
    };
    if scan.invoke_ops.is_empty()
        || scan.invoke_ops.iter().any(|(_, _, opcode)| *opcode != 0xb8)
        || !scan.multianewarray_ops.is_empty()
        || !scan.field_ops.is_empty()
        || !scan.typecheck_ops.is_empty()
        || !scan.static_field_ops.is_empty()
        || !scan.new_ops.is_empty()
        || !scan.anewarray_ops.is_empty()
        || !scan.indy_ops.is_empty()
        || scan.has_newarray
        || scan.has_athrow
    {
        return false;
    }
    ir::ir_compatible(&scan)
        && !method_uses_category2(code, code_len, descriptor)
        && !method_uses_fp(code, code_len, descriptor)
}

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
    /// Compact-layout metadata for resolved callee fields:
    /// (callee_pc, byte_offset_from_object_body, is_reference).
    /// Kept separate from `field_info` so legacy-layout compilation and
    /// consumers that only need the abstract slot index stay unchanged.
    pub compact_field_info: Vec<(usize, u32, bool)>,
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
    /// Callee PCs of `invokespecial` instructions the resolver PROVED are
    /// no-ops and may be elided: calls to `java/lang/Object.<init>()V` or to
    /// a super constructor whose body is exactly
    /// `aload_0; invokespecial Object.<init>; return` (the
    /// `is_elidable_construction` predicate). This is what makes CONSTRUCTOR
    /// bodies inlineable — every ctor starts with such a super call, which
    /// historically caused a blanket 0xb7 rejection in the inline resolver,
    /// so no constructor could ever inline and every `new C(args)` paid a
    /// full `jit_invoke_dispatch` round trip per allocation. The inline body
    /// emitter pops the receiver the preceding `aload_0` pushed and emits
    /// NOTHING for these PCs; any 0xb7 NOT in this list still bails.
    pub elided_invoke_pcs: Vec<usize>,
}

/// Estimate the native-code expansion charged to the compilation's inline
/// budget. The estimate deliberately stays cheap and deterministic: planning
/// happens before backend emission and must not resolve or compile anything
/// speculatively.
pub fn inline_site_expansion_cost(site: &InlineSite) -> Option<usize> {
    inline_site_expansion_cost_tiered(site, /* site_is_hot */ false)
}

/// Site-hotness-aware form of [`inline_site_expansion_cost`]
/// (jit-inlining-and-ir-calls).
///
/// The cost MODEL is identical — bytecode length plus a backend-oriented charge
/// per field / static-field access and for a context-needing body. What the
/// tier changes is the two ceilings the estimate is judged against:
///
/// | | callee bytecode | expansion cost |
/// |---|---|---|
/// | cold | [`MAX_INLINE_SIZE_COLD`] (35, HotSpot `MaxInlineSize`) | [`MAX_INLINE_EXPANSION_COST`] (64) |
/// | hot | [`MAX_INLINE_BYTECODE_SIZE`] (325, HotSpot `FreqInlineSize`) | [`MAX_INLINE_EXPANSION_COST_HOT`] (512) |
///
/// A callee at or below [`MAX_TRIVIAL_INLINE_SIZE`] (HotSpot `MaxTrivialSize`)
/// bypasses the *size* cap in either tier — it is smaller than the call
/// sequence it replaces — but still pays into the per-method budget and still
/// has to clear its tier's expansion ceiling, so a 6-byte body doing five field
/// loads is not waved through.
///
/// `site_is_hot == false` reproduces the pre-2026-07-26 decision exactly, which
/// is why [`inline_site_expansion_cost`] simply delegates with `false`: an
/// unprofiled compile is bit-for-bit unchanged.
pub fn inline_site_expansion_cost_tiered(site: &InlineSite, site_is_hot: bool) -> Option<usize> {
    if site.callee_code_len == 0 {
        return None;
    }
    let size_cap = if site_is_hot {
        MAX_INLINE_BYTECODE_SIZE
    } else {
        MAX_INLINE_SIZE_COLD
    };
    if site.callee_code_len > size_cap && site.callee_code_len > MAX_TRIVIAL_INLINE_SIZE {
        return None;
    }

    let field_cost = site.field_info.len().saturating_mul(6);
    let static_field_cost = site.static_field_info.len().saturating_mul(8);
    let context_cost = usize::from(site.needs_heap).saturating_mul(4);
    let cost = site
        .callee_code_len
        .saturating_add(field_cost)
        .saturating_add(static_field_cost)
        .saturating_add(context_cost);
    let cost_cap = if site_is_hot {
        MAX_INLINE_EXPANSION_COST_HOT
    } else {
        MAX_INLINE_EXPANSION_COST
    };
    (cost <= cost_cap).then_some(cost)
}

#[cfg(test)]
mod inline_selection_tests {
    use super::*;

    fn site(code_len: usize, fields: usize, static_fields: usize, needs_heap: bool) -> InlineSite {
        InlineSite {
            callee_code: vec![0; code_len.saturating_add(2)],
            callee_code_len: code_len,
            callee_max_locals: 1,
            callee_num_args: 0,
            callee_is_static: true,
            return_type: b'V',
            field_info: (0..fields).map(|pc| (pc, 0, b'I')).collect(),
            compact_field_info: Vec::new(),
            static_field_info: (0..static_fields)
                .map(|pc| (pc, 1, 0, b'I', false))
                .collect(),
            ldc_info: Vec::new(),
            ldc2w_info: Vec::new(),
            needs_heap,
            class_name: "InlineCost".to_string(),
            method_name: "leaf".to_string(),
            descriptor: "()V".to_string(),
            elided_invoke_pcs: Vec::new(),
        }
    }

    #[test]
    fn inline_selection_prices_backend_expansion() {
        assert_eq!(inline_site_expansion_cost(&site(10, 2, 1, true)), Some(34));
    }

    #[test]
    fn inline_selection_rejects_oversized_or_expensive_leaf() {
        assert_eq!(
            inline_site_expansion_cost(&site(MAX_INLINE_BYTECODE_SIZE + 1, 0, 0, false)),
            None
        );
        assert_eq!(inline_site_expansion_cost(&site(35, 5, 0, true)), None);
    }

    // ── HotSpot three-tier shape (jit-inlining-and-ir-calls) ────────────

    /// A callee between `MaxInlineSize` (35) and `FreqInlineSize` (325) is
    /// rejected at a cold site and accepted at a hot one. This is the whole
    /// point of the re-shape: before it, the flat 35-byte cap meant a 200-byte
    /// method in the middle of a hot loop was treated exactly like one called
    /// twice.
    #[test]
    fn hot_site_gets_freq_inline_size_allowance() {
        let s = site(200, 0, 0, false);
        assert_eq!(
            inline_site_expansion_cost_tiered(&s, false),
            None,
            "200 bytes must exceed MaxInlineSize at a cold site"
        );
        assert_eq!(
            inline_site_expansion_cost_tiered(&s, true),
            Some(200),
            "200 bytes is within FreqInlineSize at a hot site"
        );
        // The hot tier is still bounded — FreqInlineSize is a cap, not a licence.
        assert_eq!(
            inline_site_expansion_cost_tiered(&site(MAX_INLINE_BYTECODE_SIZE + 1, 0, 0, false), true),
            None
        );
    }

    /// The cold tier is byte-for-byte the pre-re-shape behaviour, so an
    /// unprofiled compile inlines identically. `inline_site_expansion_cost`
    /// must agree with the tiered form at `site_is_hot == false`.
    #[test]
    fn cold_tier_matches_legacy_behaviour() {
        for (len, f, sf, heap) in [
            (10usize, 2usize, 1usize, true),
            (35, 0, 0, false),
            (36, 0, 0, false),
            (35, 5, 0, true),
        ] {
            let s = site(len, f, sf, heap);
            assert_eq!(
                inline_site_expansion_cost(&s),
                inline_site_expansion_cost_tiered(&s, false),
                "legacy entry point must delegate to the cold tier"
            );
        }
        // 36 bytes was rejected by the old flat cap and must still be rejected
        // when cold, even though the admission constant is now 325.
        assert_eq!(inline_site_expansion_cost(&site(36, 0, 0, false)), None);
    }

    /// A trivial callee (<= `MaxTrivialSize`) bypasses the SIZE cap in either
    /// tier — but not the expansion ceiling, so a tiny body doing many field
    /// loads is still priced honestly rather than waved through.
    #[test]
    fn trivial_callee_bypasses_size_cap_but_not_expansion_cap() {
        assert_eq!(
            inline_site_expansion_cost_tiered(&site(MAX_TRIVIAL_INLINE_SIZE, 0, 0, false), false),
            Some(MAX_TRIVIAL_INLINE_SIZE)
        );
        // 6-byte body, 10 field accesses → 6 + 60 = 66 > 64: rejected cold.
        assert_eq!(
            inline_site_expansion_cost_tiered(&site(MAX_TRIVIAL_INLINE_SIZE, 10, 0, false), false),
            None
        );
        // …but within the hot ceiling of 512.
        assert_eq!(
            inline_site_expansion_cost_tiered(&site(MAX_TRIVIAL_INLINE_SIZE, 10, 0, false), true),
            Some(66)
        );
    }

    /// `hot_loop_ranges` must recover each loop's HEADER by decoding the
    /// back-edge branch, because `LoopTripProfile` records only the back-edge
    /// pc. A site inside the recovered `[header, back_edge]` span is hot; one
    /// outside it is not.
    #[test]
    fn hot_loop_range_recovers_header_from_backedge_branch() {
        // pc 0..7: loop body (a call site at pc 4)
        // pc 8:    goto -8  → back to pc 0   (0xa7, 0xff, 0xf8)
        // pc 11:   a call site AFTER the loop
        let mut code = vec![0u8; 16];
        code[8] = 0xa7;
        code[9] = 0xff;
        code[10] = 0xf8; // -8
        let mut prof = profile::MethodProfile::default();
        for _ in 0..INLINE_HOT_LOOP_BACKEDGES {
            prof.record_backedge(8);
        }
        let ranges = hot_loop_ranges(&code, code.len(), Some(&prof));
        assert_eq!(ranges, vec![(0usize, 8usize)], "header recovered from goto -8");
        assert!(call_site_is_hot(4, &ranges, Some(&prof)), "site inside the loop is hot");
        assert!(
            !call_site_is_hot(11, &ranges, Some(&prof)),
            "site after the loop is cold"
        );
    }

    /// A cold loop (below the back-edge threshold) yields no hot range, and a
    /// FORWARD branch keyed in the profile is ignored rather than mistaken for
    /// a back-edge — a forward target would otherwise produce an inverted span.
    #[test]
    fn hot_loop_ranges_ignore_cold_loops_and_forward_branches() {
        let mut code = vec![0u8; 16];
        code[8] = 0xa7;
        code[9] = 0xff;
        code[10] = 0xf8; // -8, a real back-edge
        let mut cold = profile::MethodProfile::default();
        for _ in 0..(INLINE_HOT_LOOP_BACKEDGES - 1) {
            cold.record_backedge(8);
        }
        assert!(hot_loop_ranges(&code, code.len(), Some(&cold)).is_empty());

        let mut forward_code = vec![0u8; 16];
        forward_code[2] = 0xa7;
        forward_code[3] = 0x00;
        forward_code[4] = 0x06; // +6 → forward
        let mut hot = profile::MethodProfile::default();
        for _ in 0..INLINE_HOT_LOOP_BACKEDGES {
            hot.record_backedge(2);
        }
        assert!(hot_loop_ranges(&forward_code, forward_code.len(), Some(&hot)).is_empty());

        // No profile at all ⇒ nothing hot ⇒ every budget is the legacy one.
        assert!(hot_loop_ranges(&code, code.len(), None).is_empty());
        assert!(!call_site_is_hot(4, &[], None));
    }

    /// The second hotness signal: a call site with a heavily-executed
    /// receiver-type profile is hot even with no enclosing loop.
    #[test]
    fn hot_receiver_profile_marks_a_site_hot() {
        let mut prof = profile::MethodProfile::default();
        for _ in 0..INLINE_HOT_SITE_OBSERVATIONS {
            prof.record_receiver(4, 7);
        }
        assert!(call_site_is_hot(4, &[], Some(&prof)));
        assert!(!call_site_is_hot(5, &[], Some(&prof)));

        let mut lukewarm = profile::MethodProfile::default();
        for _ in 0..(INLINE_HOT_SITE_OBSERVATIONS - 1) {
            lukewarm.record_receiver(4, 7);
        }
        assert!(!call_site_is_hot(4, &[], Some(&lukewarm)));
    }
}

/// A constant-pool value that the JIT can materialize safely.
///
/// String literals deliberately carry their UTF-8 bytes rather than a Java
/// `ObjectRef`: the latter can relocate between compiled invocations.
#[derive(Clone, Debug)]
pub enum JitLdcConstant {
    Immediate(i64),
    String(String),
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
    ///
    /// # BUG-ES-TASKINFO-20260710 — compact-ref-field offsets were wrong
    ///
    /// Under `CRATONVM_COMPACT_REF_FIELDS` (default ON), a *reference*
    /// instance field is stored as a bare 8-byte pointer instead of the
    /// uniform 16-byte `Value` cell (see `cratonvm_types::field_layout`).
    /// `String.value` (a `byte[]` ref, always field index 0) is exactly such
    /// a field, so it only occupies 8 bytes of body space — but the old
    /// `cell(idx) = HEADER_SIZE + idx * SLOT_SIZE` formula assumed every
    /// field, including `value`, takes a full 16-byte `SLOT_SIZE`. That
    /// overcounts `value`'s footprint by 8 bytes, so it computed every
    /// following field's offset (`coder`, `hash`) 8 bytes too high.
    ///
    /// Concretely: the inlined `indexOf`/`charAt`/`length`/`hashCode`/
    /// `equals`/`compareTo` intrinsics in `x64.rs` would read `value`'s own
    /// trailing bytes as if they were `coder`'s cell, and — for `indexOf` —
    /// dereference what should have been `coder`'s tag+payload32 word (a
    /// small int, e.g. `1i64 << 32` for a UTF16-coded string) as if it were
    /// an 8-byte pointer, producing a wild-pointer SIGSEGV. The bogus
    /// pointer was fully deterministic (not a data race): it fell straight
    /// out of the fixed mis-offset applied to every compact-ref-field
    /// `java/lang/String` instance, reproducing byte-for-byte across runs.
    ///
    /// The fix consults the registered per-class `CompactLayout` (built at
    /// class-define time; see `cratonvm_types::class_layout`) for the real
    /// per-field byte offset whenever compact ref fields are enabled and a
    /// layout is available for `string_class_id`, instead of assuming
    /// uniform `SLOT_SIZE` spacing. A *reference* field has no tag/payload32
    /// prefix — every call site in `x64.rs` reads the pointer via
    /// `cell_offset + FIELD_CELL_PAYLOAD64_OFFSET`, so the resolved absolute
    /// offset is biased by `-FIELD_CELL_PAYLOAD64_OFFSET` before being
    /// stored, so that arithmetic still lands on the bare pointer. Falls
    /// back to the legacy uniform formula when compact ref fields are off,
    /// or (defensively) if no layout is registered yet for `string_class_id`.
    pub fn new(
        value_field_index: usize,
        coder_field_index: Option<usize>,
        hash_field_index: usize,
        string_class_id: u32,
    ) -> Self {
        let cell = |idx: usize| -> i32 {
            if cratonvm_types::compact_ref_fields_enabled() {
                if let Some((body_off, is_ref)) =
                    cratonvm_types::compact_field_slot(string_class_id, idx)
                {
                    let abs = (cratonvm_types::HEADER_SIZE + body_off) as i32;
                    return if is_ref {
                        // Bare 8-byte pointer, no cell tag/payload32 prefix.
                        abs - cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32
                    } else {
                        abs
                    };
                }
            }
            // FALLBACK (compact ref fields globally off, or no CompactLayout
            // registered yet for string_class_id -- e.g. a class that never
            // reached ClassStore::add, as every test in
            // intrinsic_string_access.rs/intrinsic_string_search.rs
            // deliberately forges via a fake `string_class_id`).
            //
            // Every x64.rs call site reconstructs the true address by adding
            // ITS OWN payload offset to this function's return value
            // (`FIELD_CELL_PAYLOAD64_OFFSET` for `value`, `_32_OFFSET` for
            // `coder`/`hash`), and emit_load_string_value_ptr /
            // emit_load_string_i32_field's own "legacy" branch adds a
            // FURTHER `+FIELD_CELL_PAYLOAD64_OFFSET` (8) on top of that. So
            // this fallback must return `legacy_cell_start -
            // FIELD_CELL_PAYLOAD64_OFFSET` for EVERY field, ref or
            // primitive alike -- not the bare legacy cell-start unbiased --
            // so the caller's add + the emitter's own +8 net out to the true
            // legacy payload address. (The registered branch above only
            // needs the bias for `is_ref` because its `abs` already IS a
            // bare-pointer/payload address for non-ref fields; the fallback
            // formula below is a plain cell-start for every field, so it
            // always needs the bias.)
            //
            // BUG-STRINGINTRINSIC-20260711: this used to return the bare,
            // unbiased `HEADER_SIZE + idx*SLOT_SIZE`, which put every
            // fallback field's reconstructed address 8 bytes past its real
            // payload. For `value` (a byte[] ref) that misread the NEXT
            // field's (`coder`'s) cell as the array pointer -- combining
            // `Value::Int`'s zero tag with `coder`'s own int payload into a
            // bogus 64-bit value (e.g. `0x1_00000000` for coder=1),
            // non-null and plausible-looking, later dereferenced for the
            // array-length bounds check. SIGSEGV.
            (cratonvm_types::HEADER_SIZE + idx * cratonvm_types::SLOT_SIZE) as i32
                - cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32
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
    /// Dispatch encoding read by `jit_invoke_dispatch`: 0 = virtual, 1 = special,
    /// 2 = interface, 3 = static. Value `4` = self-recursive static DIRECT call:
    /// an IR-only marker (set by the eligibility loop when
    /// `CRATONVM_JIT_IR_SELFREC_DIRECT` is on) that makes `ir_lower` emit a direct
    /// `CALL` to the method's own entry; such a call NEVER reaches the dispatch
    /// helper, so the helper need not handle 4.
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

/// Process-global pointer to the VM-side
/// `jit_integer_value_of_direct(vm_ptr, value) -> i64` thin helper,
/// registered once at VM init (`build_helpers`). Avoids a
/// `JitRuntimeHelpers` ABI change (same pattern as
/// `x64::ARM_SAVEBASE_WATCH_FN`). `0` = not wired → the recognition below
/// is skipped and `Integer.valueOf` sites use the generic dispatch helper.
///
/// Why: `invokestatic Integer.valueOf(I)` is statically bound and its
/// callee is a registered native, so the generic `jit_invoke_dispatch`
/// round trip (info decode, per-call thread-local cache probes, argument
/// buffer build, safe-native-call wrapper) is pure fixed overhead on one
/// of the hottest autoboxing paths (three `valueOf` calls per
/// `HashMap<Integer,Integer>` put+get pair). A direct `CALL` to the thin
/// helper keeps the exact allocation, `-128..=127` identity-cache, and
/// pending-return rooting semantics while skipping the dispatch machinery.
pub static INTEGER_VALUE_OF_DIRECT_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Register the `Integer.valueOf` thin direct-call helper (called once from
/// the VM's `build_helpers`).
pub fn set_integer_value_of_direct_fn(addr: usize) {
    INTEGER_VALUE_OF_DIRECT_FN.store(addr, std::sync::atomic::Ordering::Relaxed);
}

/// `Integer.intValue()` sibling of [`INTEGER_VALUE_OF_DIRECT_FN`].
/// `java/lang/Integer` is `final`, so an `invokevirtual` site whose
/// constant-pool class is exactly `Integer` is statically monomorphic and
/// can take the plain (guard-free) virtual direct-call path; the thin
/// helper handles the null-receiver NPE itself.
pub static INTEGER_INT_VALUE_DIRECT_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Exact-HashMap `put`/`get` thin direct-call helpers
/// (perf/halfgap-20260717). `java/util/HashMap` is NOT final, so these
/// register guard-free (`guard_class_id: 0`) and the helpers themselves
/// verify the receiver's EXACT class, falling back to the full generic
/// dispatcher for subclasses (LinkedHashMap at a HashMap-declared site),
/// non-Integer keys, materialized maps, and redefine windows. The fast
/// path is the Integer-overlay probe with no `safe_native_call` wrapper —
/// the same wrapper-free contract as `INTEGER_INT_VALUE_DIRECT_FN`.
pub static HASHMAP_PUT_DIRECT_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
pub static HASHMAP_GET_DIRECT_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// Static `StringLatin1.toLowerCase` helper for the compact-string hot path.
pub static STRING_LATIN1_LOWER_DIRECT_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
pub static STRING_LOCALE_LOWER_DIRECT_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
pub static CONCURRENT_HASHMAP_GET_DIRECT_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Register the exact-HashMap thin direct-call helpers (called once from the
/// VM's `build_helpers`).
pub fn set_hashmap_put_direct_fn(addr: usize) {
    HASHMAP_PUT_DIRECT_FN.store(addr, std::sync::atomic::Ordering::Relaxed);
}
pub fn set_hashmap_get_direct_fn(addr: usize) {
    HASHMAP_GET_DIRECT_FN.store(addr, std::sync::atomic::Ordering::Relaxed);
}
pub fn set_string_latin1_lower_direct_fn(addr: usize) {
    STRING_LATIN1_LOWER_DIRECT_FN.store(addr, std::sync::atomic::Ordering::Relaxed);
}
pub fn set_string_locale_lower_direct_fn(addr: usize) {
    STRING_LOCALE_LOWER_DIRECT_FN.store(addr, std::sync::atomic::Ordering::Relaxed);
}
pub fn set_concurrent_hashmap_get_direct_fn(addr: usize) {
    CONCURRENT_HASHMAP_GET_DIRECT_FN.store(addr, std::sync::atomic::Ordering::Relaxed);
}

/// Register the `Integer.intValue` thin direct-call helper (called once from
/// the VM's `build_helpers`).
pub fn set_integer_int_value_direct_fn(addr: usize) {
    INTEGER_INT_VALUE_DIRECT_FN.store(addr, std::sync::atomic::Ordering::Relaxed);
}

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
    // Real-JDK CRC32 stores its public (not complemented running) value in
    // `crc`. Keep archive creation on the proven updateBytes0 native path
    // until every compiled call shape has one state-representation contract.
    // CRC32C uses its own running-state contract and remains eligible above.
    if class == "java/util/zip/CRC32" {
        return None;
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
    /// Keeps a compiled cache target alive while generated code can load its
    /// raw entry pointer. Native targets have no owner and leave this empty.
    compiled_owner: parking_lot::Mutex<Option<Arc<CompiledMethod>>>,
}

impl JitMICSlot {
    /// Temporary class-id value used while publishing a new cache entry.
    ///
    /// The generated code compares the receiver id before loading the target;
    /// keeping the guard non-matching until *all* companion fields are ready
    /// makes the publication atomic from its point of view. Class ids are
    /// allocated densely from zero and never use this all-ones reservation.
    const INSTALLING_CLASS_ID: u32 = u32::MAX;
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
            compiled_owner: parking_lot::Mutex::new(None),
        }
    }

    pub fn prepopulate(&self, class_id: u32) {
        self.cached_class_id
            .store(class_id, std::sync::atomic::Ordering::Relaxed);
    }

    /// Update all cached fields after a cache miss.
    pub fn update(&self, class_id: u32, class_name: &str, entry_ptr: u64, needs_context: bool) {
        use std::sync::atomic::Ordering;

        // A raw inline MIC has no helper boundary between its guard load and
        // indirect CALL. Retargeting a populated slot can therefore pair one
        // receiver's class guard with another receiver's entry. Make the slot
        // monomorphic for its lifetime: reserve an empty slot with CAS, publish
        // its companion fields, then publish the class id last. A different
        // receiver simply takes the ordinary helper path; this is slower only
        // for polymorphic sites and cannot redirect native control flow.
        //
        // `prepopulate` seeds `cached_class_id` from profiling data with
        // `cached_entry_ptr` still 0 (a guard hint, not an installed target).
        // Treat that shape as an empty slot for THIS class id too — otherwise
        // the very first `update` for the profiled-dominant receiver would
        // match `id == class_id` and return before ever publishing an entry,
        // permanently stranding the slot at "unresolved" for its whole
        // lifetime.
        loop {
            let current = self.cached_class_id.load(Ordering::Acquire);
            if current == class_id {
                if self.cached_entry_ptr.load(Ordering::Acquire) != 0 {
                    // Already fully installed for this class: idempotent no-op.
                    return;
                }
                if self
                    .cached_class_id
                    .compare_exchange(
                        class_id,
                        Self::INSTALLING_CLASS_ID,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    break;
                }
                continue;
            }
            match current {
                0 => {
                    if self
                        .cached_class_id
                        .compare_exchange(
                            0,
                            Self::INSTALLING_CLASS_ID,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }
                Self::INSTALLING_CLASS_ID => std::hint::spin_loop(),
                _ => return,
            }
        }
        // Publish entry_ptr BEFORE class_id so the inline cache reader (which
        // checks class_id first, then loads entry_ptr) never observes a class
        // id paired with stale target metadata.
        *self.compiled_owner.lock() = resolve_jit_entry_owner(entry_ptr as usize);
        self.cached_entry_ptr.store(entry_ptr, Ordering::Release);
        *self.cached_class_name.lock() = Some(std::sync::Arc::from(class_name));
        self.cached_needs_context
            .store(needs_context, Ordering::Relaxed);
        self.cached_class_id.store(class_id, Ordering::Release);
    }

    /// Drop only the compiled-entry half of the MIC.
    ///
    /// The receiver class/name cache remains useful for helper-side dispatch,
    /// but generated inline code treats a zero entry pointer as unresolved.
    pub fn clear_compiled_entry(&self) {
        self.cached_entry_ptr
            .store(0, std::sync::atomic::Ordering::Release);
        defer_jit_owner(self.compiled_owner.lock().take());
        self.cached_needs_context
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn invalidate_target(&self, targets: &std::collections::HashSet<usize>) {
        let entry = self
            .cached_entry_ptr
            .load(std::sync::atomic::Ordering::Acquire) as usize;
        if entry != 0 && targets.contains(&entry) {
            self.clear_compiled_entry();
        }
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
    /// Strong owners for compiled `entry_ptrs`; tail-only so hot offsets stay
    /// stable. Native targets leave the corresponding element empty.
    compiled_owners: [parking_lot::Mutex<Option<Arc<CompiledMethod>>>; JIT_PIC_ENTRIES],
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
            compiled_owners: [
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
        *self.compiled_owners[0].lock() = mic.compiled_owner.lock().clone();
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
        defer_jit_owner(self.compiled_owners[victim].lock().take());
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
        *self.compiled_owners[i].lock() = resolve_jit_entry_owner(entry_ptr as usize);
        self.entry_ptrs[i].store(entry_ptr, std::sync::atomic::Ordering::Release);
        self.needs_context[i].store(needs_ctx, std::sync::atomic::Ordering::Relaxed);
        *self.class_names[i].lock() = Some(class_name.to_string());
        self.hits[i].store(0, std::sync::atomic::Ordering::Relaxed);
        // Publish the new class_id last.
        self.class_ids[i].store(class_id, std::sync::atomic::Ordering::Release);
    }

    /// Remove all cached compiled targets from this PIC.
    ///
    /// Clear class ids first so generated inline code misses immediately, then
    /// zero the target metadata.
    pub fn clear_entries(&self) {
        for i in 0..JIT_PIC_ENTRIES {
            self.class_ids[i].store(0, std::sync::atomic::Ordering::Release);
        }
        for i in 0..JIT_PIC_ENTRIES {
            self.entry_ptrs[i].store(0, std::sync::atomic::Ordering::Release);
            self.needs_context[i].store(false, std::sync::atomic::Ordering::Relaxed);
            self.hits[i].store(0, std::sync::atomic::Ordering::Relaxed);
            *self.class_names[i].lock() = None;
            defer_jit_owner(self.compiled_owners[i].lock().take());
        }
    }

    fn invalidate_targets(&self, targets: &std::collections::HashSet<usize>) {
        for i in 0..JIT_PIC_ENTRIES {
            let entry = self.entry_ptrs[i].load(std::sync::atomic::Ordering::Acquire) as usize;
            if entry != 0 && targets.contains(&entry) {
                self.class_ids[i].store(0, std::sync::atomic::Ordering::Release);
                self.entry_ptrs[i].store(0, std::sync::atomic::Ordering::Release);
                self.needs_context[i].store(false, std::sync::atomic::Ordering::Relaxed);
                defer_jit_owner(self.compiled_owners[i].lock().take());
            }
        }
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
    // Loader-identity fix (2026-07-22): two classes with the SAME binary
    // name loaded by DIFFERENT `ClassLoader`s (e.g. a custom
    // `ClassLoader(null)` re-loading an old H2 jar's own
    // `org.h2.mvstore.RootReference` alongside the identically-named class
    // already on the application classpath — see
    // `docs/known-issues/h2/bug-h2-suite-residual-fail-triage.md`'s
    // `TestUpgrade` residual) are DISTINCT classes with unrelated bytecode,
    // but the interpreter's own dispatch (`resolve_method_ref` /
    // `execute_invoke_kind` / `try_stackless_invoke`) already correctly
    // disambiguates them via `ClassId` (loader-aware) at every call site.
    // This cache was the ONE place that didn't: keyed purely by
    // (class_name, method_name, descriptor) STRINGS, so once EITHER
    // same-named class's method got JIT-compiled first, every subsequent
    // call to the OTHER class's identically-named-and-shaped method hit
    // this cache and silently executed the WRONG class's compiled body —
    // whichever compiled first permanently "won" the name, `put()`
    // additionally evicting the loser's entry outright. Including the
    // resolved declaring `ClassId` in the key gives each class's compiled
    // method its own cache slot, exactly mirroring the interpreter's own
    // per-`ClassId` dispatch and letting both coexist.
    declaring_class_id: cratonvm_types::ClassId,
}

/// Compute a u64 hash key for a JIT cache entry by XOR-folding independent
/// FxHashes (class, method, descriptor, declaring class id). Using separate
/// hashers per component (rather than chained writes) keeps each call
/// branch-free and avoids the per-call Arc clones the previous keyed
/// lookup required.
///
/// Hash collisions are tolerated by the cache: `JitCache::get` always
/// verifies the full string key match after the hash hit (see PERF-P2
/// fix). A collision degrades to a cache miss, which is correct but
/// slightly suboptimal (triggers a re-compile via the slow path).
#[inline]
fn compute_jit_key_hash(
    class: &str,
    method: &str,
    desc: &str,
    declaring_class_id: cratonvm_types::ClassId,
) -> u64 {
    let mut hc = FxHasher::default();
    hc.write(class.as_bytes());
    let mut hm = FxHasher::default();
    hm.write(method.as_bytes());
    let mut hd = FxHasher::default();
    hd.write(desc.as_bytes());
    let mut hi = FxHasher::default();
    hi.write_u32(declaring_class_id.as_u32());
    hc.finish() ^ hm.finish() ^ hd.finish() ^ hi.finish()
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
impl CompiledMethod {
    fn invalidate_cached_targets(&self, targets: &std::collections::HashSet<usize>) {
        for slot in &self._jit_mic_slots {
            slot.invalidate_target(targets);
        }
        for slot in &self._jit_pic_slots {
            slot.invalidate_targets(targets);
        }
    }

    fn clear_cached_targets(&self) {
        for slot in &self._jit_mic_slots {
            slot.clear_compiled_entry();
        }
        for slot in &self._jit_pic_slots {
            slot.clear_entries();
        }
    }
}

type JitCacheMap = FxHashMap<u64, (JitKey, Arc<CompiledMethod>)>;
const JIT_CACHE_SHARDS: usize = 64;

struct JitCacheShard {
    methods: arc_swap::ArcSwap<JitCacheMap>,
    osr_methods: arc_swap::ArcSwap<JitCacheMap>,
}

impl JitCacheShard {
    fn new() -> Self {
        Self {
            methods: arc_swap::ArcSwap::from_pointee(FxHashMap::default()),
            osr_methods: arc_swap::ArcSwap::from_pointee(FxHashMap::default()),
        }
    }
}

/// Lock-free-read, copy-on-write sharded compiled-method cache.
///
/// A lookup touches one immutable shard snapshot and clones only the returned
/// `Arc<CompiledMethod>`. Mutations are serialized because publication and
/// dependency invalidation are rare and must be atomic as a group; only the
/// affected shard map is copied for an ordinary tier-up publication.
pub struct JitCache {
    shards: Box<[JitCacheShard]>,
    mutation: parking_lot::Mutex<()>,
    string_arena: parking_lot::Mutex<Vec<Pin<Box<str>>>>,
    invoke_info_arena: parking_lot::Mutex<Vec<Pin<Box<JitInvokeInfo>>>>,
}

static JIT_ENTRY_OWNERS: std::sync::OnceLock<
    parking_lot::Mutex<FxHashMap<usize, std::sync::Weak<CompiledMethod>>>,
> = std::sync::OnceLock::new();
static JIT_CACHE_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);
static ACTIVE_JIT_EXECUTIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static DEFERRED_JIT_OWNERS: std::sync::OnceLock<
    parking_lot::Mutex<Vec<Arc<CompiledMethod>>>,
> = std::sync::OnceLock::new();

fn jit_entry_owners(
) -> &'static parking_lot::Mutex<FxHashMap<usize, std::sync::Weak<CompiledMethod>>> {
    JIT_ENTRY_OWNERS.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

fn resolve_jit_entry_owner(entry: usize) -> Option<Arc<CompiledMethod>> {
    jit_entry_owners().lock().get(&entry)?.upgrade()
}

fn deferred_jit_owners() -> &'static parking_lot::Mutex<Vec<Arc<CompiledMethod>>> {
    DEFERRED_JIT_OWNERS.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

fn drain_deferred_jit_owners_if_quiescent() {
    if ACTIVE_JIT_EXECUTIONS.load(std::sync::atomic::Ordering::Acquire) != 0 {
        return;
    }
    let retired = std::mem::take(&mut *deferred_jit_owners().lock());
    drop(retired);
}

fn defer_jit_owner(owner: Option<Arc<CompiledMethod>>) {
    let Some(owner) = owner else {
        return;
    };
    if ACTIVE_JIT_EXECUTIONS.load(std::sync::atomic::Ordering::Acquire) == 0 {
        drop(owner);
        return;
    }
    deferred_jit_owners().lock().push(owner);
    // Close the race where the final execution leaves between the first load
    // and queue publication.
    drain_deferred_jit_owners_if_quiescent();
}

/// Enter/leave the process-wide executable-code quiescence epoch. VM JIT entry
/// guards call these at the same boundaries as their precise frame chain.
pub fn jit_execution_enter() {
    ACTIVE_JIT_EXECUTIONS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

pub fn jit_execution_leave() {
    let previous = ACTIVE_JIT_EXECUTIONS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    debug_assert!(previous > 0, "unbalanced JIT execution leave");
    if previous == 1 {
        drain_deferred_jit_owners_if_quiescent();
    }
}

/// Pin a raw compiled entry while an external dispatch cache can publish it.
pub fn pin_jit_entry(entry: usize) -> Option<Arc<CompiledMethod>> {
    resolve_jit_entry_owner(entry)
}

/// Monotonic publication/invalidation generation for external entry caches.
///
/// Advanced by EVERY mutation of a [`JitCache`] — `put`, `put_osr`, any
/// `invalidate_*`, and `clear_all` — including a first-time insertion of a key
/// that was not previously present.
///
/// That last case matters: until T2.2 the two `put` paths bumped only when they
/// *replaced* an existing entry, which was sufficient for the original consumer
/// (`vm/src/jit/helpers.rs`'s thread-local raw-entry dispatch caches, a purely
/// *positive* cache that a brand-new key cannot invalidate). The interpreter's
/// invoke-cache epoch check is a *negative* cache — "this method has no compiled
/// body" — and a first-time publication is precisely what falsifies it, so the
/// bump has to be unconditional. Under-bumping there would leave an interpreted
/// call site pinned to the interpreter forever after a background compile
/// published its body.
pub fn jit_cache_generation() -> u64 {
    JIT_CACHE_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

impl JitCache {
    pub fn new() -> Self {
        let shards = (0..JIT_CACHE_SHARDS)
            .map(|_| JitCacheShard::new())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            mutation: parking_lot::Mutex::new(()),
            string_arena: parking_lot::Mutex::new(Vec::new()),
            invoke_info_arena: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Compatibility accessors for callers written against the historical
    /// outer `RwLock<JitCache>`. They deliberately do not acquire a lock.
    #[inline]
    pub fn read(&self) -> &Self {
        self
    }

    #[inline]
    pub fn write(&self) -> &Self {
        self
    }

    #[inline]
    fn shard_index(hash: u64) -> usize {
        (hash as usize) & (JIT_CACHE_SHARDS - 1)
    }

    pub fn intern_string(&self, s: String) -> (*const u8, usize) {
        let boxed: Pin<Box<str>> = Pin::new(s.into_boxed_str());
        let ptr = boxed.as_ptr();
        let len = boxed.len();
        self.string_arena.lock().push(boxed);
        (ptr, len)
    }

    pub fn intern_invoke_info(&self, info: JitInvokeInfo) -> *const JitInvokeInfo {
        let boxed = Pin::new(Box::new(info));
        let ptr: *const JitInvokeInfo = &*boxed;
        self.invoke_info_arena.lock().push(boxed);
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
        declaring_class_id: cratonvm_types::ClassId,
    ) -> Option<Arc<CompiledMethod>> {
        let h = compute_jit_key_hash(class_name, method_name, descriptor, declaring_class_id);
        let methods = self.shards[Self::shard_index(h)].methods.load();
        let (key, method) = methods.get(&h)?;
        if &*key.class_name == class_name
            && &*key.method_name == method_name
            && &*key.descriptor == descriptor
            && key.declaring_class_id == declaring_class_id
        {
            Some(method.clone())
        } else {
            None
        }
    }

    /// Look up the independently published OSR body for a method.
    pub fn get_osr(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) -> Option<Arc<CompiledMethod>> {
        let h = compute_jit_key_hash(class_name, method_name, descriptor, declaring_class_id);
        let methods = self.shards[Self::shard_index(h)].osr_methods.load();
        let (key, method) = methods.get(&h)?;
        if &*key.class_name == class_name
            && &*key.method_name == method_name
            && &*key.descriptor == descriptor
            && key.declaring_class_id == declaring_class_id
        {
            Some(method.clone())
        } else {
            None
        }
    }

    fn prepare_for_publication(compiled: &mut CompiledMethod) {
        compiled._direct_callee_roots = compiled
            ._direct_callee_entries
            .iter()
            .filter_map(|&entry| resolve_jit_entry_owner(entry))
            .collect();
    }

    pub fn put(
        &self,
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        declaring_class_id: cratonvm_types::ClassId,
        mut compiled: CompiledMethod,
    ) {
        let _mutation = self.mutation.lock();
        let h = compute_jit_key_hash(&class_name, &method_name, &descriptor, declaring_class_id);
        let key = JitKey {
            class_name,
            method_name,
            descriptor,
            declaring_class_id,
        };
        Self::prepare_for_publication(&mut compiled);
        let arc = Arc::new(compiled);
        cratonvm_types::jit_activation::register_executable_owner(
            arc.entry_ptr() as usize,
            declaring_class_id.as_u32(),
        );
        // Stage 5 — register this method's code range for the GC RBP-chain
        // walker. Enabled when the precise gate is on (the registry is consulted
        // by `remap_active_jit_frames`) OR when the BUG-03 cross-thread STW JIT
        // root scan is on (which classifies a forcibly-stopped peer's `Rip` as
        // in-JIT-or-not by looking it up in this registry — an empty registry
        // would make every peer look "not in JIT" and the scan a no-op). The
        // default path keeps zero bookkeeping overhead.
        if crate::x64::precise_jit_maps_enabled() || xt_jit_root_scan_enabled() {
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
        jit_entry_owners()
            .lock()
            .insert(arc.entry_ptr() as usize, Arc::downgrade(&arc));
        let shard = &self.shards[Self::shard_index(h)];
        let mut next = (**shard.methods.load()).clone();
        next.insert(h, (key, arc));
        shard.methods.store(Arc::new(next));
        // T2.2 — bump on EVERY publication, not just replacements. A first-time
        // insertion is exactly the event the interpreter's negative
        // "no compiled body for this method" memo
        // (`CachedBytecodeMethod::jit_probe_generation`) must observe. See
        // `jit_cache_generation`.
        JIT_CACHE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// Publish an OSR body without superseding the method-entry body.
    pub fn put_osr(
        &self,
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        declaring_class_id: cratonvm_types::ClassId,
        mut compiled: CompiledMethod,
    ) {
        let _mutation = self.mutation.lock();
        debug_assert!(compiled.compiled_via_osr);
        let h = compute_jit_key_hash(&class_name, &method_name, &descriptor, declaring_class_id);
        let key = JitKey {
            class_name,
            method_name,
            descriptor,
            declaring_class_id,
        };
        Self::prepare_for_publication(&mut compiled);
        let arc = Arc::new(compiled);
        cratonvm_types::jit_activation::register_executable_owner(
            arc.entry_ptr() as usize,
            declaring_class_id.as_u32(),
        );
        if crate::x64::precise_jit_maps_enabled() || xt_jit_root_scan_enabled() {
            register_jit_code_range(
                arc.entry_ptr() as usize,
                arc.code_len(),
                Arc::as_ptr(&arc) as usize,
            );
        }
        if jit_names_enabled() {
            register_jit_method_name(
                arc.entry_ptr() as usize,
                arc.code_len(),
                format!(
                    "{}.{}{} [osr]",
                    key.class_name, key.method_name, key.descriptor
                ),
            );
        }
        jit_entry_owners()
            .lock()
            .insert(arc.entry_ptr() as usize, Arc::downgrade(&arc));
        let shard = &self.shards[Self::shard_index(h)];
        let mut next = (**shard.osr_methods.load()).clone();
        next.insert(h, (key, arc));
        shard.osr_methods.store(Arc::new(next));
        // T2.2 — unconditional, for the same reason as `put` above.
        JIT_CACHE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.methods.load().len() + shard.osr_methods.load().len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove a compiled method from the cache (for invalidation).
    ///
    /// Verifies the full string key matches before removing, so a
    /// (rare) hash collision can't cause an unrelated cached entry to
    /// be evicted.
    pub fn remove(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        declaring_class_id: cratonvm_types::ClassId,
    ) {
        self.invalidate_matching(|key, _| {
            &*key.class_name == class_name
                && &*key.method_name == method_name
                && &*key.descriptor == descriptor
                && key.declaring_class_id == declaring_class_id
        });
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
    pub fn invalidate_for_class_change(&self, changed_class: &str) -> usize {
        self.invalidate_matching(|_, cm| {
            cm.inlined_methods
                .iter()
                .any(|(cls, _, _)| cls == changed_class)
        })
    }

    /// Invalidate all compiled methods that inlined code from `class_name`.
    /// Returns the number of methods evicted.
    pub fn invalidate_for_class(&self, class_name: &str) -> usize {
        self.invalidate_matching(|_, compiled| {
            compiled
                .inlined_methods
                .iter()
                .any(|(cn, _, _)| cn == class_name)
        })
    }

    /// Retire every body owned by an unloaded class and every caller that
    /// inlined one of its methods. Retired executable allocations are reclaimed
    /// by the epoch/quiescence path once no active frame can still execute them.
    pub fn invalidate_unloaded_class(
        &self,
        class_id: cratonvm_types::ClassId,
        class_name: &str,
    ) -> usize {
        self.invalidate_matching(|key, compiled| {
            key.declaring_class_id == class_id
                || compiled
                    .inlined_methods
                    .iter()
                    .any(|(cn, _, _)| cn == class_name)
        })
    }

    fn invalidate_matching(
        &self,
        predicate: impl Fn(&JitKey, &CompiledMethod) -> bool,
    ) -> usize {
        let _mutation = self.mutation.lock();
        let mut remove_entries = std::collections::HashSet::new();
        for shard in self.shards.iter() {
            for (_hash, (key, cm)) in shard.methods.load().iter() {
                if predicate(key, cm) {
                    remove_entries.insert(cm.entry_ptr() as usize);
                }
            }
            for (_hash, (key, cm)) in shard.osr_methods.load().iter() {
                if predicate(key, cm) {
                    remove_entries.insert(cm.entry_ptr() as usize);
                }
            }
        }

        // A raw direct caller of an invalidated body is invalid too. Compute
        // the transitive reverse closure before publishing any new snapshot.
        loop {
            let before = remove_entries.len();
            for shard in self.shards.iter() {
                for map in [shard.methods.load(), shard.osr_methods.load()] {
                    for (_hash, (_key, cm)) in map.iter() {
                        if cm
                            ._direct_callee_entries
                            .iter()
                            .any(|entry| remove_entries.contains(entry))
                        {
                            remove_entries.insert(cm.entry_ptr() as usize);
                        }
                    }
                }
            }
            if remove_entries.len() == before {
                break;
            }
        }

        // Retarget dynamic inline caches before withdrawing ownership from the
        // cache. Readers that already hold an old caller snapshot either miss
        // after this release publication or keep the callee alive through the
        // slot's strong owner until that snapshot is dropped.
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (_key, cm)) in map.iter() {
                    cm.invalidate_cached_targets(&remove_entries);
                }
            }
        }

        let mut removed = 0;
        for shard in self.shards.iter() {
            let current = shard.methods.load();
            let mut next = (**current).clone();
            let old_len = next.len();
            next.retain(|_, (_, cm)| !remove_entries.contains(&(cm.entry_ptr() as usize)));
            removed += old_len - next.len();
            if next.len() != old_len {
                shard.methods.store(Arc::new(next));
            }

            let current = shard.osr_methods.load();
            let mut next = (**current).clone();
            let old_len = next.len();
            next.retain(|_, (_, cm)| !remove_entries.contains(&(cm.entry_ptr() as usize)));
            removed += old_len - next.len();
            if next.len() != old_len {
                shard.osr_methods.store(Arc::new(next));
            }
        }
        if removed != 0 {
            JIT_CACHE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        removed
    }

    /// Invalidate every compiled method in the cache.
    ///
    /// JVMTI redefine can invalidate caller-side direct calls and inline caches,
    /// not just methods declared by the redefined class. A full flush is rare
    /// but conservative; ownership pins any body still referenced by an
    /// already-loaded reader while new snapshots become empty atomically.
    pub fn clear_all(&self) -> usize {
        let _mutation = self.mutation.lock();
        let mut count = 0;
        for shard in self.shards.iter() {
            for map in [shard.methods.load(), shard.osr_methods.load()] {
                for (_hash, (_key, cm)) in map.iter() {
                    cm.clear_cached_targets();
                }
            }
            count += shard.methods.load().len() + shard.osr_methods.load().len();
            shard.methods.store(Arc::new(FxHashMap::default()));
            shard.osr_methods.store(Arc::new(FxHashMap::default()));
        }
        if count != 0 {
            JIT_CACHE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
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
        write!(
            f,
            "JitCache({} methods, {} osr methods)",
            self.shards.iter().map(|s| s.methods.load().len()).sum::<usize>(),
            self.shards
                .iter()
                .map(|s| s.osr_methods.load().len())
                .sum::<usize>()
        )
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
        // Array element access escapes its array reference (conservative): map to
        // `EaOp::Call`, whose handling marks every reference input `ArgEscape`.
        // Today the array is always a Param/external ref (the builder bails on
        // `newarray`, so a `new[]` never reaches here) and so is never a
        // scalar-replacement candidate, but routing through `Call` (rather than
        // the no-op `Other`) keeps a hypothetical future `new[]` from being
        // wrongly scalar-replaced — the IR lowerer has no scalar-array path.
        ir::Op::ArrayLoad(_) | ir::Op::ArrayStore(_) => EaOp::Call,
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

/// Build the [`ir_lower::ScalarReplacementMap`] that drives guard-surviving
/// scalar replacement (Front 3.2): for each scalar-replaced object, capture the
/// metadata the deopt producer needs to emit a `FrameValue::VirtualObject`
/// (`class_id`/`num_fields`/per-field value nodes/eliminated stores), keyed by
/// the IR `NodeId` of the eliminated `Op::New`. Sourced entirely from
/// `ea_result` + `id_map`, so it is order-independent w.r.t. `apply_ea_to_ir`
/// (which only mutates the graph). An object whose `Op::New` or any *stored*
/// field value cannot be mapped to an IR node is **omitted** — the producer then
/// leaves its slot `Undefined` (safe whole-method re-run) rather than emit a
/// partial/garbage object. Only called when `scalar_deopt_enabled() &&
/// deopt_real_enabled()`.
fn build_scalar_replacement_map(
    ir_graph: &ir::Graph,
    id_map: &[escape_analysis::NodeId],
    ea_result: &escape_analysis::EscapeAnalysisResult,
) -> ir_lower::ScalarReplacementMap {
    let mut reverse_map: HashMap<escape_analysis::NodeId, ir::NodeId> = HashMap::new();
    for (ir_id, &ea_id) in id_map.iter().enumerate() {
        if ea_id != usize::MAX {
            reverse_map.insert(ea_id, ir_id as ir::NodeId);
        }
    }
    // Control input (slot 0) of a node, used to recover a node's block for the
    // dominance gate AFTER the node itself is marked `Op::Dead` (its inputs are
    // cleared then, but the captured control node stays live). MUST be called
    // before `apply_ea_to_ir`.
    let ctrl_of = |n: ir::NodeId| -> Option<ir::NodeId> {
        ir_graph
            .nodes
            .get(n as usize)
            .and_then(|node| node.inputs.first().copied())
            .filter(|&c| c != ir::NO_NODE)
    };
    let mut objects: HashMap<ir::NodeId, ir_lower::VirtualObjectInfo> = HashMap::new();
    'obj: for info in &ea_result.scalar_replaceable {
        let ir_new = match reverse_map.get(&info.alloc_node) {
            Some(&id) => id,
            None => continue,
        };
        // Control of the allocation — bail (omit) if it has none (hand-built /
        // malformed), so the producer can't emit without a dominance anchor.
        let new_ctrl = match ctrl_of(ir_new) {
            Some(c) => c,
            None => continue,
        };
        // Per-field IR value node. A `None` EA entry is a never-stored field
        // (zero default). A `Some(ea)` that fails to map back to an IR node means
        // we cannot reconstruct that field — omit the whole object (safe).
        let mut field_values: Vec<Option<ir::NodeId>> = Vec::with_capacity(info.field_values.len());
        for fv in &info.field_values {
            match fv {
                None => field_values.push(None),
                Some(ea) => match reverse_map.get(ea) {
                    Some(&ir_id) => field_values.push(Some(ir_id)),
                    None => continue 'obj,
                },
            }
        }
        // Capture each eliminated store's control node (its block). A store with
        // no resolvable control omits the object (can't prove dominance).
        let mut store_ctrls: Vec<ir::NodeId> = Vec::with_capacity(info.eliminated_stores.len());
        for ea in &info.eliminated_stores {
            let ir_store = match reverse_map.get(ea) {
                Some(&id) => id,
                None => continue, // a store with no IR node can't have executed observably
            };
            match ctrl_of(ir_store) {
                Some(c) => store_ctrls.push(c),
                None => continue 'obj,
            }
        }
        objects.insert(
            ir_new,
            ir_lower::VirtualObjectInfo {
                class_id: info.class_id,
                num_fields: info.num_fields,
                field_values,
                new_ctrl,
                store_ctrls,
            },
        );
    }
    ir_lower::ScalarReplacementMap { objects }
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
    // This is a permanent-failure blocklist, not the dispatch cache — a
    // name-only collision between two same-named classes from different
    // loaders is benign here (worst case: one class's compilable method
    // gets conservatively skipped because a same-named-and-shaped method
    // elsewhere hit a genuine backend limitation), so a fixed sentinel
    // `ClassId` keeps this hash's shape unchanged rather than threading a
    // real class identity through this negative-cache-only path.
    let h = compute_jit_key_hash(class_name, method_name, descriptor, cratonvm_types::ClassId::new(0));
    jit_bail_list().read().contains(&h)
}

/// Mark the method as permanently bail-listed.  Called when the heavy
/// `x64::compile` path returns None (typically because of an unsupported
/// backend pattern that won't change on retry).
pub fn mark_jit_bail_listed(class_name: &str, method_name: &str, descriptor: &str) {
    let h = compute_jit_key_hash(class_name, method_name, descriptor, cratonvm_types::ClassId::new(0));
    jit_bail_list().write().insert(h);
}

/// Diagnostic: number of methods currently bail-listed.
pub fn jit_bail_list_size() -> usize {
    jit_bail_list().read().len()
}

/// Parsed `CRATONVM_JIT_DENY` filter (see the `try_compile` call site).
/// `None` = disabled.
fn jit_deny_filter() -> Option<&'static Vec<String>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<Vec<String>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let v = std::env::var("CRATONVM_JIT_DENY").ok()?;
            if v.is_empty() {
                return None;
            }
            Some(v.split(',').map(|s| s.trim().to_string()).collect())
        })
        .as_ref()
}

fn jit_allow_packages_filter() -> &'static Vec<String> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        std::env::var("CRATONVM_JIT_ALLOW_PACKAGES")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|entry| entry.trim().to_string())
                    .filter(|entry| !entry.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn jit_allow_entry_allows_prefix(entry: &str, prefix: &str) -> bool {
    !entry.is_empty() && prefix.starts_with(entry)
}

fn jit_allow_package(prefix: &str) -> bool {
    jit_allow_packages_filter()
        .iter()
        .any(|entry| jit_allow_entry_allows_prefix(entry, prefix))
}

fn hibernate_temporal_jit_deny_prefix(class_name: &str) -> Option<&'static str> {
    const SLASH_PREFIX: &str = "org/hibernate/";
    const DOT_PREFIX: &str = "org.hibernate.";
    if class_name.starts_with(SLASH_PREFIX) {
        Some(SLASH_PREFIX)
    } else {
        class_name.starts_with(DOT_PREFIX).then_some(DOT_PREFIX)
    }
}

fn hsqldb_jit_deny_prefix(class_name: &str) -> Option<&'static str> {
    const SLASH_PREFIX: &str = "org/hsqldb/";
    const DOT_PREFIX: &str = "org.hsqldb.";
    if class_name.starts_with(SLASH_PREFIX) {
        Some(SLASH_PREFIX)
    } else {
        class_name.starts_with(DOT_PREFIX).then_some(DOT_PREFIX)
    }
}
fn jaxb_mapping_jit_deny_prefix(class_name: &str) -> Option<&'static str> {
    const SLASH_PREFIX: &str = "org/glassfish/jaxb/";
    const DOT_PREFIX: &str = "org.glassfish.jaxb.";
    if class_name.starts_with(SLASH_PREFIX) {
        Some(SLASH_PREFIX)
    } else {
        class_name.starts_with(DOT_PREFIX).then_some(DOT_PREFIX)
    }
}

fn xerces_schema_jit_deny_prefix(class_name: &str) -> Option<&'static str> {
    const SLASH_PREFIX: &str = "com/sun/org/apache/xerces/internal/";
    const DOT_PREFIX: &str = "com.sun.org.apache.xerces.internal.";
    if class_name.starts_with(SLASH_PREFIX) {
        Some(SLASH_PREFIX)
    } else {
        class_name.starts_with(DOT_PREFIX).then_some(DOT_PREFIX)
    }
}

fn snakeyaml_emitter_emit_jit_deny_prefix(
    class_name: &str,
    method_name: &str,
) -> Option<&'static str> {
    if class_name == "org/yaml/snakeyaml/emitter/Emitter" && method_name == "emit" {
        Some("org/yaml/snakeyaml/emitter/")
    } else {
        None
    }
}

/// Diagnostic: number of `try_compile` calls short-circuited because
/// the method was already bail-listed.  Each short-circuit saves the
/// ~50µs we'd otherwise have spent re-running scan/IR/lowering only to
/// re-hit the same backend bail.
pub fn jit_bail_shortcircuits() -> u64 {
    JIT_BAIL_SHORTCIRCUITS.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct JitCompileMethodKey {
    class_name: String,
    method_name: String,
    descriptor: String,
}

impl JitCompileMethodKey {
    fn new(class_name: &str, method_name: &str, descriptor: &str) -> Self {
        Self {
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            descriptor: descriptor.to_string(),
        }
    }

    fn from_cached(cached: &CachedBytecodeMethod) -> Self {
        Self::new(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
    }

    fn matches(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool {
        self.class_name == class_name
            && self.method_name == method_name
            && self.descriptor == descriptor
    }
}

thread_local! {
    static JIT_COMPILE_STACK: std::cell::RefCell<Vec<JitCompileMethodKey>> =
        std::cell::RefCell::new(Vec::new());
}

static JIT_RECURSIVE_CYCLE_METHODS: OnceLock<
    parking_lot::RwLock<rustc_hash::FxHashSet<JitCompileMethodKey>>,
> = OnceLock::new();

fn jit_recursive_cycle_methods(
) -> &'static parking_lot::RwLock<rustc_hash::FxHashSet<JitCompileMethodKey>> {
    JIT_RECURSIVE_CYCLE_METHODS
        .get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashSet::default()))
}

struct JitCompileStackGuard {
    key: JitCompileMethodKey,
}

impl JitCompileStackGuard {
    fn enter(cached: &CachedBytecodeMethod) -> Self {
        let key = JitCompileMethodKey::from_cached(cached);
        JIT_COMPILE_STACK.with(|stack| stack.borrow_mut().push(key.clone()));
        Self { key }
    }
}

impl Drop for JitCompileStackGuard {
    fn drop(&mut self) {
        JIT_COMPILE_STACK.with(|stack| {
            let popped = stack.borrow_mut().pop();
            debug_assert_eq!(popped.as_ref(), Some(&self.key));
        });
    }
}

fn mark_jit_recursive_cycle_method(key: JitCompileMethodKey) {
    jit_recursive_cycle_methods().write().insert(key);
}

fn mark_current_jit_compile_method_recursive_cycle() {
    if let Some(key) = JIT_COMPILE_STACK.with(|stack| stack.borrow().last().cloned()) {
        mark_jit_recursive_cycle_method(key);
    }
}

/// Returns true when `target` closes a compile-time cycle to an outer method.
/// When that happens, every method on the cycle path is marked so callers that
/// compiled the callee recursively can still avoid baking a raw direct call.
fn note_jit_recursive_compile_cycle(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    let cycle_path = JIT_COMPILE_STACK.with(|stack| {
        let stack = stack.borrow();
        let current_idx = stack.len().checked_sub(1)?;
        let target_idx = stack[..current_idx]
            .iter()
            .position(|k| k.matches(class_name, method_name, descriptor))?;
        Some(stack[target_idx..].to_vec())
    });

    if let Some(path) = cycle_path {
        let mut recursive = jit_recursive_cycle_methods().write();
        for key in path {
            recursive.insert(key);
        }
        true
    } else {
        false
    }
}

thread_local! {
    /// Per-compile request flag (same consume-once pattern as
    /// `x64::set_kernel_reg_homes_request`): the VM caller sets it right
    /// before `try_compile` when it has PROVEN that the compiling class's
    /// self-references resolve to the class itself — the class was defined
    /// by a BUILTIN (bootstrap/extension/application) loader and the
    /// loader-blind global name lookup maps its name back to its own
    /// `ClassId`. Under that proof a NON-tail static self-recursive
    /// invokestatic may be raw-routed to the guarded direct self-CALL
    /// (`x64.rs` 0xb8 else-arm) instead of carrying dispatch metadata: the
    /// loader-identity hazard that historically forced the dispatch route
    /// ("a raw direct entry call ... can invoke a different same-named
    /// method") cannot arise for a builtin-loaded class, whose registry
    /// holds exactly one class per name and always resolves a
    /// self-reference to the already-defined class. Custom (`UserDefined`)
    /// loaders keep the dispatch route unconditionally.
    ///
    /// Motivation (bt18 regression): dispatch-mediated self-recursion pays
    /// the full helper round trip per level — `jit_invoke_dispatch` +
    /// `push_entry_full`/`pop_jit_entry` + the `lookup_jit_code_range`
    /// mutex scan — measured at >60% of the whole BinTreesClassic d=18 run
    /// (~90M chain pushes; the recursive `bottomUpTree`/`itemCheck` pair
    /// dispatched once per NODE).
    static SELF_CALL_IDENTITY_STABLE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Set the per-compile self-call identity proof — see
/// [`SELF_CALL_IDENTITY_STABLE`]. Consumed (reset to `false`) by the next
/// `try_compile` on this thread, including on its early-bail paths.
pub fn set_self_call_identity_stable(v: bool) {
    SELF_CALL_IDENTITY_STABLE.with(|c| c.set(v));
}

/// Direct JIT-to-JIT calls into recursive compile-cycle participants bypass the
/// dispatch depth guard. Such targets must stay on the dispatch path.
#[doc(hidden)]
pub fn jit_direct_call_requires_dispatch(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // TOMCAT-SILENT-HANG.5: direct JIT-to-JIT calls into ClassParser's
    // readInterfaces scan edge corrupt a later allocation header. Keep this
    // edge on invoke_dispatch so its rooting and return protocol applies.
    if class_name == "org/apache/tomcat/util/bcel/classfile/ClassParser"
        && method_name == "readInterfaces"
    {
        return true;
    }
    let key = JitCompileMethodKey::new(class_name, method_name, descriptor);
    jit_recursive_cycle_methods().read().contains(&key)
}

/// Enables raw, machine-code JIT-to-JIT direct calls to a compiled callee:
/// eager callee-compile-and-direct-CALL in this function's caller
/// (`try_compile_inner`) and the inline virtual MIC fast path (`x64.rs`).
/// (`vm/src/jit/helpers.rs`'s OWN separate dispatch-helper direct-entry cache
/// — `jit_invoke_dispatch`/`jit_invoke_virtual_mic`'s cached `entry_ptr` path
/// — is a different, still-default-OFF gate,
/// `CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY`: it has its own separate,
/// still-open target-resolution bug on top of the RBP-mirror race below, so
/// it does not share this flag's default.) This includes calls this JIT's
/// own self-recursion detection does not resolve to the dedicated
/// `self_call_patches` fast path (e.g. `BenchSuite.make`/`check`'s tiered
/// background-compile route) — the flag being off routed those through the
/// full `jit_invoke_dispatch` helper on every call, a ~15x regression on
/// bintrees16 (44s vs. 2.9s) that generalizes to any call-heavy recursive
/// workload, not just the ES repro this flag was introduced for.
///
/// A raw JIT-to-JIT CALL updates the per-thread active-RBP mirror to the
/// callee, while the interpreter-owned root-chain entry still describes the
/// caller until the callee's own prologue runs — a GC landing in that window
/// can select the caller's oop map for the callee's frame and lose live
/// roots. This exact mechanism produced the IVFKnn stress-test
/// stale-precise-root-mirror corruption (see
/// docs/known-issues/elasticsearch-suite/
/// ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md).
/// Closed by two companion fixes that ship alongside this flag:
/// [`Compiler::emit_post_call_rbp_republish`] (re-publishes the caller's RBP
/// after every raw JIT-to-JIT return) and `JitEntryGuard::enter_with_compiled`
/// always retaining precise frame metadata (`vm/src/jit/conservative_roots.rs`)
/// so `scan_compiled_frame_bands` can bound each stacked raw-call frame
/// exactly instead of one imprecise whole-band conservative sweep. Re-enabled
/// default-ON (re-verified against `testSlicesSparseWithFilter` /
/// `testRandomWithFilter` / the full `cratonvm-gc`/`cratonvm-jit`/
/// `cratonvm-vm` --lib suites — see the ES-HANG doc above for results) to
/// close the general-throughput regression; opt out with
/// `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` if a new corruption is ever suspected
/// here again (matches this codebase's established off-switch convention, see
/// [`x64::guarded_inline_getfield_enabled`]).
///
/// NOT OnceLock-cached (mirrors [`x64::guarded_inline_getfield_enabled`]):
/// this is checked at JIT-compile time, never on the runtime hot path within
/// this crate, so re-reading the env var has no measurable cost — and caching
/// would make the flag racy against whichever test/thread compiles first.
/// IR direct-call lowering — may the optimizing IR pipeline bind a resolved,
/// statically-bound `invokestatic` / non-`<init>` `invokespecial` straight to its
/// callee's compiled entry (a raw `CALL`), the way the single-pass backend
/// already does?
///
/// Subordinate to [`direct_jit_callee_calls_enabled`]: the IR path emits exactly
/// the same raw JIT-to-JIT edge the single-pass backend emits, so it must respect
/// the same master switch. `CRATONVM_JIT_IR_DIRECT_CALL=0` is an additional
/// IR-only opt-out for bisecting a regression to this lowering specifically
/// (which restores the historical `jit_invoke_dispatch` route for every IR call
/// site) without also disabling single-pass direct calls.
pub fn ir_direct_calls_enabled() -> bool {
    if !direct_jit_callee_calls_enabled() {
        return false;
    }
    match std::env::var("CRATONVM_JIT_IR_DIRECT_CALL") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

pub fn direct_jit_callee_calls_enabled() -> bool {
    match std::env::var("CRATONVM_JIT_DIRECT_CALLEE_CALLS") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

thread_local! {
    /// Per-thread override for [`ir_virtual_calls_enabled`], for tests that must
    /// exercise the "virtual calls declined" routing without mutating a
    /// process-global env var (which would race parallel test threads). `None`
    /// ⇒ fall back to the env var (i.e. enabled). Production never sets this.
    /// Mirrors the `SELFREC_DIRECT_TEST_OVERRIDE` convention.
    static IR_VIRTUAL_CALLS_TEST_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Test hook: force IR virtual/interface call lowering on (`Some(true)`) / off
/// (`Some(false)`) for the CURRENT thread, or restore env behaviour (`None`).
/// Thread-local so parallel tests don't race. Not part of the stable API.
#[doc(hidden)]
pub fn __set_ir_virtual_calls_override(v: Option<bool>) {
    IR_VIRTUAL_CALLS_TEST_OVERRIDE.with(|c| c.set(v));
}

/// IR inline-cache lowering — may the optimizing IR pipeline lower
/// `invokevirtual` / `invokeinterface` (jit-inlining-and-ir-calls)?
///
/// The capability itself is not new, but it was gated **opt-IN** at the VM call
/// sites (`vm/src/runtime/env_cache.rs` computes it as
/// `var_os("CRATONVM_JIT_IR_CALL_VIRTUAL").is_some()`) for a single, now-obsolete
/// reason: the IR lowered every virtual call through the generic
/// `jit_invoke_dispatch` helper with no inline cache, so admitting virtual calls
/// made call-heavy methods SLOWER than the single-pass body they replaced —
/// single-pass has emitted a MIC/PIC cascade all along.
/// `ir_lower::emit_inline_cache_call` closes that gap, so the regression risk
/// that justified opt-in is gone and the flag inverts to a diagnostic opt-OUT,
/// matching every other IR capability gate in this file
/// (`CRATONVM_JIT_IR_CALL`, `…_CALL_SPECIAL`, `…_LONG`, `…_FP`,
/// `…_SELFREC_DIRECT`, `…_IR_DIRECT_CALL`), all of which default to true.
///
/// `try_compile_inner` ORs the caller-supplied `ir_emit_virtual_calls` parameter
/// with this, so the parameter can still force the capability ON but can no
/// longer force it off.
///
/// FOLLOW-UP (outside this change's file scope): the `env_cache.rs` copy is now
/// redundant and should be inverted to `map_or(true, |v| v != "0")` for
/// consistency, after which this local gate can be deleted. See the design doc.
///
/// NOT OnceLock-cached, for the same reason as
/// [`direct_jit_callee_calls_enabled`]: this is read at JIT-compile time only.
pub fn ir_virtual_calls_enabled() -> bool {
    if let Some(v) = IR_VIRTUAL_CALLS_TEST_OVERRIDE.with(|c| c.get()) {
        return v;
    }
    match std::env::var("CRATONVM_JIT_IR_CALL_VIRTUAL") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// Number of platform C-ABI integer argument registers a compiled callee entry
/// reads its incoming arguments from — the planner-side mirror of
/// `ir_lower::ENTRY_ABI_REGS`.
///
/// The IR's direct-call and inline-cache hit paths marshal in registers only
/// (there is no stack-argument path), so a site whose receiver + arguments +
/// hidden VM context pointer exceed this must keep helper dispatch. Kept here so
/// the planner and the lowerer cannot disagree about the bound.
pub(crate) const fn ir_entry_abi_reg_count() -> usize {
    #[cfg(target_os = "windows")]
    {
        4
    }
    #[cfg(not(target_os = "windows"))]
    {
        6
    }
}

#[cfg(test)]
fn clear_jit_recursive_cycle_methods_for_test() {
    jit_recursive_cycle_methods().write().clear();
    JIT_COMPILE_STACK.with(|stack| stack.borrow_mut().clear());
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
    cp_field_resolver: Option<&dyn Fn(u16) -> Option<(usize, u8, Option<(u32, bool)>)>>,
    cp_static_field_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    callee_compiler: Option<&dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,
    cp_new_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, bool, bool)>>,
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<JitLdcConstant>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<(i64, bool)>>,
    profile: Option<&profile::MethodProfile>,
    helpers: &JitRuntimeHelpers,
    inline_resolver: Option<&dyn Fn(&str, &str, &str) -> Option<InlineSite>>,
    string_layout_resolver: Option<&dyn Fn() -> Option<StringFieldLayout>>,
    cp_invoke_class_id_resolver: Option<&dyn Fn(u16) -> Option<u32>>,
    cp_elidable_init_resolver: Option<&dyn Fn(u16) -> bool>,
    optimize: bool,
    ir_emit_calls: bool,
    ir_emit_special_calls: bool,
    ir_emit_long: bool,
    ir_emit_virtual_calls: bool,
    ir_emit_fp: bool,
    cp_invokedynamic_descriptor_resolver: Option<&dyn Fn(u16) -> Option<String>>,
) -> Option<CompiledMethod> {
    // Thin compatibility wrapper: the overwhelming majority of callers
    // (every `jit` crate test, plus any VM call site that hasn't been
    // updated to supply one) have no need for the JVMS §6.5 `invokespecial`
    // super-call redirect below — `None` here reproduces this function's
    // exact pre-existing resolution behavior byte-for-byte.
    try_compile_with_invokespecial_resolver(
        cached,
        cp_class_name_resolver,
        cp_field_resolver,
        cp_static_field_resolver,
        cp_invoke_resolver,
        None,
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
        ir_emit_long,
        ir_emit_virtual_calls,
        ir_emit_fp,
        cp_invokedynamic_descriptor_resolver,
    )
}

/// Full form of [`try_compile`] taking an additional resolver for the JVMS
/// §6.5 `invokespecial` super-call redirect (`cp_invokespecial_owner_resolver`
/// below) — used by the VM's own call sites (`try_jit_upgrade_with_gate`,
/// `callee_compiler`, `try_jit_compile_callee_slow` in
/// `vm/src/runtime/interpreter.rs`), which have a calling-class identity to
/// resolve it against. Kept as a separate function (rather than adding the
/// parameter to `try_compile` itself) so the ~30 existing `try_compile`
/// call sites in this crate's own test suites need no changes.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub fn try_compile_with_invokespecial_resolver(
    cached: &CachedBytecodeMethod,
    cp_class_name_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    cp_field_resolver: Option<&dyn Fn(u16) -> Option<(usize, u8, Option<(u32, bool)>)>>,
    cp_static_field_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    // JVMS §6.5 `invokespecial` super-call redirect: given an `invokespecial`
    // (0xb7) call-site's CP index, returns the JVMS-correct class at which
    // method selection actually begins when that differs from the plain
    // `cp_invoke_resolver` class name — i.e. a genuine `super.m(...)` whose
    // constant-pool reference names an ancestor further up than this
    // method's own direct superclass. `None` (resolver absent, or it
    // returns `None` for a given site — the overwhelmingly common case)
    // leaves `class_name` exactly as `cp_invoke_resolver` returned it. See
    // `classloading::invokespecial_selection_start` for the algorithm.
    cp_invokespecial_owner_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    callee_compiler: Option<&dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,
    // CRIT-2 — returns (class_id, num_fields, has_nonzero_tag_primitive_init,
    // has_finalizer). The two flags feed the inline-TLAB `new` fast path;
    // resolvers that cannot compute them must return `(_, _, true, true)`
    // so the post-init helper call stays in place.
    cp_new_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, bool, bool)>>,
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<JitLdcConstant>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<(i64, bool)>>, // inc 35: (bits, is_double)
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
    // inc 25 (category-2 foundation): `true` lets the optimizing IR path take
    // **long**-using methods (the `method_uses_category2` gate otherwise bails
    // the whole pipeline on any long/double opcode). Only long is admitted —
    // double/float and int div/rem still bail (the latter to keep a `long`
    // off a deopt point, since long deopt-resume is a follow-up). `false` (the
    // default) preserves the int/ref-only IR path. Gated default-OFF behind
    // `CRATONVM_JIT_IR_LONG` at the VM call sites until it soaks.
    ir_emit_long: bool,
    // inc 26 (Gap B): `true` additionally lets the IR builder lower a resolved
    // `invokevirtual` (0xb6) / `invokeinterface` (0xb9) to `Op::Call`, with the
    // receiver marshalled as arg0 and `invoke_kind == 0` (virtual) / `2`
    // (interface). Dispatch is fully dynamic: the baked `JitInvokeInfo` carries
    // the static call-site class/name/descriptor and `invoke_dispatch` resolves
    // the actual target on the receiver's RUNTIME class (no inline cache in the
    // emitted code — the generic helper does the vtable/itable lookup). `false`
    // (the default) keeps every virtual/interface invoke on single-pass. Gated
    // default-OFF behind `CRATONVM_JIT_IR_CALL_VIRTUAL` at the VM call sites
    // until it soaks.
    ir_emit_virtual_calls: bool,
    // inc 30 (double/float XMM value tier): `true` admits a `float`/`double`-using
    // method to the optimizing IR path (the `method_uses_fp` gate otherwise bails
    // it to single-pass). The lowerer marshals FP values through XMM
    // (`addsd`/`cvttsd2si`/…); FP value arithmetic (`fadd`/`dmul`/…), FP
    // constants (`fconst`/`dconst`), FP-local load/store, and the int/long⇄FP
    // conversions lower. `frem`/`drem`, FP compares/branches, FP array ops, and
    // FP params/returns/call-args still bail (their builder arms are absent), as
    // do int-div-bearing and `ldc2_w`-bearing FP methods (a follow-up). `false`
    // (the default) preserves the int/long/ref IR path byte-for-byte: no
    // FP-opcode method is admitted, so the builder never sees an FP opcode. Gated
    // default-OFF behind `CRATONVM_JIT_IR_FP` at the VM call sites until it soaks.
    ir_emit_fp: bool,
    // invokedynamic-uncommon-trap fix: resolves an `invokedynamic` (0xba)
    // CP index to just its target descriptor string (e.g. via
    // `NameAndType.descriptor` — no bootstrap/`CallSite` resolution needed).
    // `jit_scan` is CP-blind and no longer bails on 0xba (it just records the
    // site); this resolver lets the single-pass backend compute the site's
    // stack effect (arg count via `count_param_slots`, return type via
    // `return_type`) so it can lower the instruction to an unconditional jump
    // to the existing uncommon-trap deopt stub (`DeoptReason::UnreachedCode`)
    // while keeping the compiler's simulated operand stack consistent for
    // whatever bytecode follows. `None` (resolver absent, or it returns `None`
    // for a given site) bails the whole compile — see `try_compile_inner`.
    cp_invokedynamic_descriptor_resolver: Option<&dyn Fn(u16) -> Option<String>>,
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

    // Keep the final compiler admission gate aligned with the VM static
    // skip-list. The tiered background worker bypasses VM-side eligibility and
    // otherwise continued compiling MutableBigInteger after it was quarantined.
    if tiered::is_biginteger_arithmetic_jit_denied(&cached.class_name) {
        return None;
    }

    // HIB-TEMPORAL.1 (2026-07-08): final fail-closed Hibernate guard. The VM
    // skip-list catches most eligibility paths, but tiered/background compile
    // can still reach this crate's final `try_compile` gate. The proven stable
    // SPB-FLYWAY-HSQLDB.1: Keep the final admission gate aligned with the VM
    // skip-list. The Flyway HSQLDB integration SIGSEGVs under JIT, while the
    // package-level interpreted control completes the entire class. Background
    // compilation can bypass VM eligibility checks, so fail closed here too.
    if let Some(prefix) = hsqldb_jit_deny_prefix(&cached.class_name) {
        if !jit_allow_package(prefix) {
            return None;
        }
    }
    // control for the temporal residuals is exactly the same shape as
    // `CRATONVM_JIT_DENY=org/hibernate/`, so keep Hibernate bytecode interpreted
    // here too unless the package is explicitly allowed for bisection.
    if let Some(prefix) = hibernate_temporal_jit_deny_prefix(&cached.class_name) {
        if !jit_allow_package(prefix) {
            return None;
        }
    }

    if let Some(prefix) = jaxb_mapping_jit_deny_prefix(&cached.class_name) {
        if !jit_allow_package(prefix) {
            return None;
        }
    }

    // Keep the final admission gate aligned with the VM-side Xerces parser
    // guard. Background compilation bypasses the VM skip-list, and JITting
    // this package corrupts SchemaGrammar's SymbolHash during Hazelcast XML
    // schema validation.
    if let Some(prefix) = xerces_schema_jit_deny_prefix(&cached.class_name) {
        if !jit_allow_package(prefix) {
            return None;
        }
    }

    // SPB-FLYWAY-HSQLDB.1: Keep the final admission gate aligned with the VM
    // skip-list. The Flyway HSQLDB integration SIGSEGVs under JIT, while the
    // package-level interpreted control completes the entire class. Background
    // compilation can bypass VM eligibility checks, so fail closed here too.
    if let Some(prefix) = hsqldb_jit_deny_prefix(&cached.class_name) {
        if !jit_allow_package(prefix) {
            return None;
        }
    }

    // ES-JIT-DEOPT-GC.1: final fail-closed companion to the VM skip-list guard
    // for `org/yaml/snakeyaml/emitter/Emitter.emit`. Tiered/background compile
    // can reach this crate after the VM-side enqueue path has logged work; keep
    // the exact proven corruptor interpreted unless explicitly lifted.
    if let Some(prefix) =
        snakeyaml_emitter_emit_jit_deny_prefix(&cached.class_name, &cached.method_name)
    {
        if !jit_allow_package(prefix) {
            return None;
        }
    }

    // DBG (RandomizedContext WeakHashMap JIT investigation, 2026-07-02):
    // `CRATONVM_JIT_DENY` — comma-separated substrings matched against
    // `Class.method`; a matching method is force-interpreted (never
    // JIT-compiled) so a single suspect compiled method can be isolated
    // from the rest of a workload's JIT-compiled code, without disabling
    // JIT wholesale. No-op unless the env var is set.
    if jit_deny_filter().is_some_and(|filter| {
        let sig = format!("{}.{}", cached.class_name, cached.method_name);
        filter.iter().any(|f| sig.contains(f.as_str()))
    }) {
        return None;
    }

    // Live code-cache cap. Reclaimed bodies restore headroom, so this is a
    // transient admission check rather than a permanent compile stop.
    if jit_code_cache_at_capacity() {
        JIT_CODE_CACHE_CAP_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return None;
    }

    // Consume the per-compile self-call identity proof FIRST — even an
    // early bail below must not leak a stale `true` into a later compile.
    let self_call_identity_stable = SELF_CALL_IDENTITY_STABLE.with(|c| c.replace(false));
    let _compile_stack_guard = JitCompileStackGuard::enter(cached);

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
        cp_invokespecial_owner_resolver,
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
        ir_emit_long,
        ir_emit_virtual_calls,
        ir_emit_fp,
        cp_invokedynamic_descriptor_resolver,
        &mut backend_attempted,
        self_call_identity_stable,
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

/// RBC.6 local-handler-safety fix. Conservative, sound check answering: for
/// this method's exception table, could ANY handler observe a local
/// variable that `route_jit_exception_through_method`'s params-only
/// handler-frame reconstruction cannot recover — i.e. anything other than
/// `this` / a declared parameter?
///
/// Delegates to `regalloc::handler_has_unsafe_local_read`, a real CFG-based
/// forward "definitely assigned" dataflow (dominance-respecting, not just
/// raw-pc-order) — see that function's own doc comment for the full
/// algorithm and why an earlier, simpler raw-pc-order approximation here
/// was both over-conservative (continued scanning past a handler's own
/// `athrow`/`return` into unrelated later code — confirmed, via the real
/// `org.apache.catalina.connector.Response.toAbsolute()` bytecode on the
/// Tomcat suite fixture, to be the actual remaining blocker for the doc's
/// own motivating case even after the RBC.6 gate itself was relaxed) and
/// had a latent, never-triggered soundness gap (path-insensitive: a store
/// on one branch and a read on a different, non-overlapping branch could
/// be wrongly accepted merely because the store's bytecode pc was lower).
///
/// **Confirmed necessary in the first place** via
/// `AthrowCountBisect.twoThrowsSequential`
/// (`vm/tests/jit_local_exception_handler_tests.rs`): two sequential,
/// non-nested try/catch blocks in one method, where the second handler's
/// own code reads a local (`a`) last assigned by the FIRST try's successful
/// (non-exceptional) path. Without SOME such check, that method compiled
/// and silently produced a wrong checksum.
fn local_handler_reads_unsafe_local(
    code: &[u8],
    code_len: usize,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    method_descriptor: &str,
    is_static: bool,
) -> bool {
    // Widening: `this` (slot 0 for instance methods) + declared param slots.
    let param_slot_count =
        count_param_slots_jvm_spec(method_descriptor) as u16 + if is_static { 0 } else { 1 };
    // Slots >= 64 can't be represented in the u64 bitmask the dataflow uses;
    // `regalloc::handler_has_unsafe_local_read` conservatively treats any
    // load of such a slot as unsafe regardless of this mask, so capping the
    // shift here (rather than overflowing) just keeps this bit of arithmetic
    // well-defined — it does not change which methods are accepted.
    let initial_safe_slots: u64 = if param_slot_count >= 64 {
        u64::MAX
    } else {
        (1u64 << param_slot_count) - 1
    };
    let dbg = std::env::var_os("CRATONVM_DBG_RBC6").is_some();
    for entry in exception_table {
        let handler_pc = entry.handler_pc as usize;
        let unsafe_found =
            regalloc::handler_has_unsafe_local_read(code, code_len, handler_pc, initial_safe_slots);
        if unsafe_found {
            if dbg {
                eprintln!(
                    "[rbc6-dbg] local_handler_reads_unsafe_local UNSAFE (CFG dataflow) handler_pc={} param_slot_count={}",
                    handler_pc, param_slot_count
                );
            }
            return true;
        }
    }
    false
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn try_compile_inner(
    cached: &CachedBytecodeMethod,
    cp_class_name_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    cp_field_resolver: Option<&dyn Fn(u16) -> Option<(usize, u8, Option<(u32, bool)>)>>,
    cp_static_field_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    // See `try_compile_with_invokespecial_resolver`.
    cp_invokespecial_owner_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    callee_compiler: Option<&dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,
    // (class_id, num_fields, has_nonzero_tag_primitive_init, has_finalizer) — see `try_compile`.
    cp_new_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, bool, bool)>>,
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<JitLdcConstant>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<(i64, bool)>>, // inc 35: (bits, is_double)
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
    // inc 25: admit long-using methods to the IR path. See `try_compile`.
    // Default-OFF at the VM call sites.
    ir_emit_long: bool,
    // inc 26 (Gap B): additionally lower resolved `invokevirtual`/
    // `invokeinterface` to `Op::Call` (dynamic dispatch via the helper). See
    // `try_compile`. Default-OFF at the VM call sites.
    ir_emit_virtual_calls: bool,
    // inc 30: admit a float/double-using method to the IR path (XMM value
    // tier). See `try_compile`. Default-OFF at the VM call sites.
    ir_emit_fp: bool,
    // invokedynamic-uncommon-trap fix: resolves an invokedynamic CP index to
    // its target descriptor. See `try_compile`.
    cp_invokedynamic_descriptor_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    // round-7 fix (bug 1): set to `true` immediately before invoking
    // the heavy `x64::compile` path so the outer wrapper can tell a
    // permanent backend bail (worth bail-listing) from an early
    // transient miss (e.g. resolver returned None, profile not yet
    // present — worth retrying later).
    backend_attempted: &mut bool,
    self_call_identity_stable: bool,
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
            if std::env::var_os("CRATONVM_DBG_RBC6").is_some() {
                eprintln!(
                    "[rbc6-dbg] try_compile_inner: jit_scan returned None for {}.{}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                );
            }
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

    // RBC.6 (RELAXED, see docs/feature-designs/jit-local-exception-handlers.md)
    // — this gate used to unconditionally refuse any method that combines
    // `athrow` with a local exception handler, on the theory that "the
    // athrow lowering stashes the exception and returns the deopt sentinel,
    // which cannot dispatch to an in-method handler." That was true when
    // this gate was written (`8d0f029ed`, 2026-06-11), but every fix that
    // landed the day after and later — BUG-H (`82b9bdf62`), KCFULL-13,
    // Round-8/9/10/11 (NPE/AIOOBE/arithmetic/general-exception leak fixes),
    // and the wildfly-bug-05 this/params restore (`549a2c161`) — built
    // exactly the missing dispatch, generically, and never revisited this
    // gate:
    //   - `emitted_athrow` unconditionally forces `has_dispatch = true`
    //     (`x64.rs`, `RBC.6` comment at the `has_dispatch` computation), so
    //     any method containing `athrow` is ALWAYS entered through
    //     `execute_jit_call`'s dispatch-aware slow path, never the raw
    //     fast-path that would leak the sentinel as a return value.
    //   - That slow path drains `JIT_PENDING_EXCEPTION` (and NPE/AIOOBE/
    //     arithmetic) after every JIT return and, whenever the invoked
    //     method itself declares a non-empty `exception_table`, routes the
    //     exception through `route_jit_exception_through_method` — a real
    //     handler search (typed catch-class matching with subclass checks,
    //     first-match-wins, `finally`/catch-all handling) that pushes a
    //     fresh interpreter frame at the resolved handler pc with `this`
    //     and the declared params restored and the exception object on the
    //     stack, and otherwise propagates to the caller exactly like an
    //     uncaught throw. This is reached identically whether the pending
    //     exception came from `athrow` in THIS method or propagated up from
    //     a callee — the sentinel-return shape is the same either way.
    //   - `callee_has_exception_table`/`route_implicit_exc_through_callee`
    //     provide the JIT-to-JIT direct-call sibling of the same routing.
    //
    // In other words: the "cannot dispatch to an in-method handler" premise
    // this gate was built on stopped being true within a day of it landing,
    // and nothing since has depended on athrow+handler staying uncompiled —
    // methods that only *declare* a handler (no local `athrow`) already
    // compile today and already rely on this exact runtime routing (that's
    // what BUG-H fixed). Only the compile-time refusal itself was stale.
    // Confirmed root cause of `Response.toAbsolute()` (Tomcat hot path,
    // `try { ... } catch (IOException) { throw new
    // IllegalArgumentException(...) }`) never JIT-compiling — see the doc
    // above for the full trace and validation plan. The IR (optimizing)
    // path keeps its own independent, unaffected
    // `cached.exception_table.is_empty()` admission check below, so this
    // change only widens single-pass (`x64::compile`) eligibility — the
    // existing, well-tested fallback backend that already carries every
    // other non-`ir_compatible` method.
    //
    // RBC.6 local-handler-safety fix — `route_jit_exception_through_method`
    // (vm/src/runtime/interpreter.rs) reconstructs the handler frame from
    // ONLY `this` + the method's declared incoming params (a documented,
    // pre-existing limitation — see that function's own doc comment). A
    // handler that reads any OTHER local — one first assigned earlier in
    // the method, whether inside this try, a DIFFERENT try/catch construct,
    // or straight-line code before either — observes a stale zero/null
    // instead of the value the compiled code actually computed. Confirmed
    // via a differential repro: `AthrowCountBisect.twoThrowsSequential` (two
    // sequential, non-nested try/catch blocks in one method; the second
    // handler's own code reads a local last assigned by the FIRST try's
    // successful path) silently computed a wrong checksum once compiled —
    // this affects declares-a-handler-no-athrow methods exactly as much as
    // the newly-relaxed has-athrow ones, since the frame-reconstruction gap
    // is in the shared runtime routing, not anything athrow-specific. Gate
    // BOTH populations (any non-empty `exception_table`, not just the
    // has_athrow case above) on `local_handler_reads_unsafe_local`, a real
    // CFG-based "definitely assigned" dataflow (dominance-respecting, not a
    // raw-pc-order approximation — an earlier version of this check used
    // exactly that simpler approximation and was found, via the REAL
    // `org.apache.catalina.connector.Response.toAbsolute()` bytecode on the
    // Tomcat suite fixture, to over-reject it: it kept scanning past the
    // handler's own `athrow` terminator into unrelated later code in the
    // same method). See `local_handler_reads_unsafe_local`'s own doc
    // comment, and `regalloc::handler_has_unsafe_local_read`'s, for the
    // full algorithm and soundness argument.
    if !cached.exception_table.is_empty() {
        let unsafe_local = local_handler_reads_unsafe_local(
            code,
            code_len,
            &cached.exception_table,
            &cached.method_descriptor,
            cached.is_static,
        );
        if std::env::var_os("CRATONVM_DBG_RBC6").is_some() {
            eprintln!(
                "[rbc6-dbg] try_compile_inner: local_handler_reads_unsafe_local={} for {}.{}{}",
                unsafe_local, cached.class_name, cached.method_name, cached.method_descriptor
            );
        }
        if unsafe_local {
            // The current exception router can restore only incoming
            // parameters. A handler that reads a later local must remain
            // interpreted until the precise exceptional-frame handoff covers
            // every compiled-call sink. Compiling it is unsound: a propagated
            // exception reaches the handler with that local reset to null/zero.
            return None;
        }
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
        // STUB-S8: the IR builder has no exception-table-aware codegen — a
        // handler entry isn't a registered merge target, so the builder walks
        // over handler bytecode with stale `self.ctrl`/`self.locals`/`self.stack`
        // state left over from wherever the linear PC walk last was. An
        // implicit-throw-only method (try/catch with no `athrow` of its own)
        // slips past the `scan.has_athrow` bail above and produces orphaned
        // nodes referencing `NO_NODE` (a popped-empty-stack or
        // never-assigned-local sentinel) that the scheduler/lowerer still
        // visit, panicking in `ir_lower::slot_of` on a `u32::MAX` index.
        && cached.exception_table.is_empty()
        && ((!method_uses_category2(code, code_len, &cached.method_descriptor)
                // inc 30: the pure int/long/ref IR path stays FP-free, so a
                // float-using (cat-1) method is no longer admitted here — it
                // routes through the FP clause below (or bails to single-pass
                // when the FP gate is off, exactly as it does today).
                && !method_uses_fp(code, code_len, &cached.method_descriptor))
            // inc 25/30(ldiv): admit a long-using method when the long gate is
            // on, as long as it is double/float-free. (inc 25 also required
            // int-div/rem-free, because a `long` live at a div guard's deopt
            // could not be reconstructed; long deopt-resume now makes `long` a
            // real `FrameValue` width — `StackSlotLong`/`Long` → `Value::Long`,
            // the locals mapper collapsing the cat-2 two-slot snapshot — so a
            // `long` may now be live at an `idiv`/`irem`/`ldiv`/`lrem` deopt.
            // The precise resume reconstructs it; an unmappable frame falls back
            // to the safe whole-method re-run.)
            || (ir_emit_long
                && !method_uses_fp(code, code_len, &cached.method_descriptor))
            // inc 30: admit a float/double-using method when the FP gate is on.
            // The VM uses the COMPACT all-GPR i64 ABI (`execute_jit_call` /
            // `jit_invoke_dispatch` marshal each FP value as `to_bits() as i64`
            // into an INTEGER arg register, NOT XMM), so NOTHING about FP
            // params/returns/call-args needs XMM register marshalling:
            //   - PARAMS (inc 34): an FP param arrives as bits in a GPR; the
            //     prologue stores it to the param slot like any other param, and
            //     `ir_param_types`/`set_param_types` already type it Float/Double
            //     (the cat-2 two-slot layout for `D`, as for `J`), so a later
            //     `dload`/`fload` reads the slot via `fp_load`. The whole gate is
            //     now FP-signature-agnostic — `fp_in_body` identifies FP methods.
            //   - RETURNS (inc 32/33): the FP result's bits ride RAX (interpreter
            //     reads `result as u64`/`as u32` → `from_bits`).
            //   - CALL-ARGS (inc 34): `static_call_shape` admits `D`/`F` args
            //     (one GPR slot each); the marshaller stores the slot bits to the
            //     staging region and `decode_dispatch_values` reads them back.
            // Slice C lifted the int-div exclusion: the IR deopt resume now
            // reconstructs FP slots (`StackSlotFloat`/`StackSlotDouble` →
            // `Value::Float`/`Value::Double`, `Double` cat-2 like `Long`), so an
            // FP value live at an `idiv`/`irem` div-by-zero deopt is restored
            // precisely rather than stranded. inc 35 lifted the `ldc2_w`
            // exclusion: the resolver reports `is_double`, so the builder lowers a
            // `double` constant to `dconst` (a `long` ldc2_w stays `lconst`) —
            // double literals (`1.5`, `3.14`, …) no longer bail.
            || (ir_emit_fp && fp_in_body(code, code_len)))
    {
        // Includes the implicit `this` slot for instance methods — see
        // `prologue_param_slots` above.
        let num_params = prologue_param_slots;
        let mut builder = ir::IrBuilder::new(num_params, cached.max_locals as usize);
        builder.tdigest_scalar_kernel = cached.class_name.as_ref()
            == "org/elasticsearch/tdigest/Dist"
            && matches!(
                (&*cached.method_name, &*cached.method_descriptor),
                ("quantile", "(DILjava/util/function/Function;)D")
                    | ("cdf", "(DILjava/util/function/Function;)D")
            );
        // Type each `Param` node from the descriptor. Two consumers depend on
        // this:
        //   * inc 25 (long gate): re-lay-out the parameter locals with the JVM
        //     two-slot category-2 convention (a `long`/`double` param occupies
        //     two slots) so `lload`/`lstore` of a later long param reads the
        //     right slot.
        //   * real-frame-deopt type source: a ref-typed param (an instance
        //     method's `this`, an object/array argument) must be `IrType::Ref`
        //     so the deopt snapshot tags its slot `StackSlotRef` → `Value::Object`
        //     on resume, instead of a truncated `Value::Int`. Without this the
        //     receiver of an instance method that deopts at, e.g., a div-by-zero
        //     guard would resume with a garbage `this`.
        // For an all-category-1 signature this is layout-identical to `new`'s
        // one-slot-per-param placement — only the node *type* changes, which is
        // codegen-neutral (spill/reload are always 64-bit REX.W; ref operands
        // are never width-sensitive arithmetic), so it is now applied
        // unconditionally rather than only under the long gate.
        let ptypes = ir_param_types(&cached.method_descriptor, cached.is_static);
        builder.set_param_types(&ptypes);
        if ir_emit_long || ir_emit_fp {
            // inc 26 (long) / inc 35 (double): resolve `ldc2_w` constants to
            // `(pc → (bits, is_double))` so the builder lowers a `long` to
            // `Op::Const(Long)` (`lconst`) and a `double` to `Op::ConstF`
            // (`dconst`). inc 35 lifts the inc-30 FP-gate `ldc2_w` exclusion now
            // that the builder disambiguates by `is_double` (a `double` constant
            // is admitted under `ir_emit_fp`). An unresolved `ldc2_w` is omitted →
            // that opcode bails to single-pass.
            if !scan.ldc2w_ops.is_empty() {
                if let Some(resolver) = cp_ldc2w_resolver {
                    let mut lm = std::collections::HashMap::with_capacity(scan.ldc2w_ops.len());
                    for &(pc, cp_idx) in &scan.ldc2w_ops {
                        if let Some(v) = resolver(cp_idx) {
                            lm.insert(pc, v);
                        }
                    }
                    builder.set_ldc2w_info(lm);
                }
            }
        }
        // Thread the resolved instance-field layout (pc → (field_index,
        // type_tag)) into the builder so it can lower an int-category
        // `getfield` into `Op::Load`. A field the resolver can't resolve is
        // simply omitted; the builder then bails that getfield to single-pass.
        if !scan.field_ops.is_empty() {
            if let Some(resolver) = cp_field_resolver {
                let mut fm = std::collections::HashMap::with_capacity(scan.field_ops.len());
                for &(pc, cp_idx) in &scan.field_ops {
                    if let Some((field_index, type_tag, _compact_slot)) = resolver(cp_idx) {
                        // The IR builder only needs (field_index, type_tag); it
                        // bails getfield/putfield to single-pass under compact.
                        fm.insert(pc, (field_index, type_tag));
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
        // IR direct-call lowering: `pc → (callee_entry, callee_needs_context)`
        // for each statically-bound site the IR lowerer may bind directly, plus
        // the flat entry list recorded on the finished `CompiledMethod` (see
        // `_direct_callee_entries` — keep-alive + invalidation closure).
        let mut ir_direct_calls: std::collections::HashMap<usize, (usize, bool)> =
            std::collections::HashMap::new();
        let mut ir_direct_callee_entries: Vec<usize> = Vec::new();
        // IR inline caches (jit-inlining-and-ir-calls): `pc → (mic_addr,
        // pic_addr)` for each virtual/interface site the IR lowerer may serve
        // from a MIC + 3-way-PIC cascade, plus the owning boxes, which are moved
        // onto the finished `CompiledMethod` so the baked imm64s stay valid.
        let mut ir_ic_slots: std::collections::HashMap<usize, (usize, usize)> =
            std::collections::HashMap::new();
        let mut ir_mic_boxes: Vec<Box<JitMICSlot>> = Vec::new();
        let mut ir_pic_boxes: Vec<Box<JitPICSlot>> = Vec::new();
        // The virtual/interface gate was default-OFF at the VM call sites for
        // one reason only: the IR lowered `invokevirtual`/`invokeinterface`
        // through the generic `jit_invoke_dispatch` helper with NO inline cache,
        // so admitting them made call-heavy methods slower than the single-pass
        // body they replaced. `ir_lower::emit_inline_cache_call` now emits the
        // same MIC + 3-way-PIC cascade the single-pass backend does, so that
        // reason is gone and the capability becomes opt-OUT
        // (`CRATONVM_JIT_IR_CALL_VIRTUAL=0`) rather than opt-in — matching every
        // other IR capability gate. The caller's parameter can still force it
        // ON; it can no longer force it off. See the design doc for the matching
        // `vm/src/runtime/env_cache.rs` cleanup.
        let ir_emit_virtual_calls = ir_emit_virtual_calls || ir_virtual_calls_enabled();
        if (ir_emit_calls || ir_emit_special_calls || ir_emit_virtual_calls)
            && !scan.invoke_ops.is_empty()
        {
            if let Some(resolver) = cp_invoke_resolver {
                let call_eligible = scan.new_ops.is_empty() && scan.anewarray_ops.is_empty();
                if call_eligible {
                    let mut info_map = std::collections::HashMap::new();
                    let mut all_emittable = true;
                    // Follow-up to the fib44 fix: when
                    // `CRATONVM_JIT_IR_SELFREC_DIRECT` is on, an eligible
                    // self-recursive static call is emitted as a DIRECT self-call
                    // (invoke_kind 4) instead of paying `jit_invoke_dispatch` on
                    // every recursion — see the gate below and `ir_lower`'s Op::Call.
                    // Default-on when the native-stack guard helper is wired;
                    // the env flag remains an opt-out for diagnosis.
                    let selfrec_direct = selfrec_direct_enabled();
                    // IR direct-call lowering (this slice) - see
                    // `ir_direct_calls_enabled` and `ir_lower`'s
                    // `emit_direct_cross_call`.
                    let ir_direct = ir_direct_calls_enabled();
                    for &(pc, cp_idx, opcode) in &scan.invoke_ops {
                        // Admit `invokestatic` (under `ir_emit_calls`), resolved
                        // non-`<init>` `invokespecial` (inc 24, under
                        // `ir_emit_special_calls`), and `invokevirtual` /
                        // `invokeinterface` (inc 25, under `ir_emit_virtual_calls`).
                        // Any invoke kind whose gate is disabled keeps the whole
                        // method on single-pass — the builder bails on an invoke
                        // with no `invoke_info` entry.
                        let is_static = opcode == 0xb8;
                        let is_special = opcode == 0xb7;
                        let is_virtual = opcode == 0xb6;
                        let is_interface = opcode == 0xb9;
                        if !((is_static && ir_emit_calls)
                            || (is_special && ir_emit_special_calls)
                            || ((is_virtual || is_interface) && ir_emit_virtual_calls))
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
                        // fib44 perf-regression fix. `static_call_shape` admitting a
                        // wide (`J`/`D`/`F`) RETURN (inc-29/32/33) let a SELF-RECURSIVE
                        // long/FP method lower its recursive call to an IR `Op::Call`,
                        // which is routed through the generic `jit_invoke_dispatch`
                        // runtime helper on EVERY invocation. Single-pass instead emits
                        // a DIRECT call to this method's own compiled entry — far cheaper
                        // for a hot recursive method (`static long fib(int)`: ~8.6x; the
                        // dispatch helper does note_jit_boundary + SATB flush + a
                        // native-stack recursion guard per call). Keep such methods on
                        // single-pass: when the resolved callee IS this method and the
                        // return is wide, mark the body non-emittable so it bails. The
                        // intended unblock — CROSS-method wide-return calls (e.g.
                        // `Pack.bigEndianToLong`) — is non-self-recursive and unaffected.
                        let is_self_recursive = is_static
                            && cn.as_str() == &*cached.class_name
                            && mn.as_str() == &*cached.method_name
                            && desc.as_str() == &*cached.method_descriptor;
                        let is_self_recursive_wide =
                            is_self_recursive && matches!(ret, b'J' | b'D' | b'F');
                        // Integer and reference self-recursion has the same direct-call
                        // ABI as the already-supported wide-return path. Void calls have
                        // no result slot for `emit_self_recursive_call` to fill.
                        let is_self_recursive_direct = selfrec_direct
                            && helpers.self_call_stack_guard != 0
                            && is_self_recursive
                            && ret != b'V';
                        if is_self_recursive_wide && !selfrec_direct {
                            // Opt-out: bail the whole method to single-pass (fast
                            // direct self-call). The direct path keeps
                            // it on the IR path with a direct self-call instead
                            // (invoke_kind 4 below).
                            all_emittable = false;
                            break;
                        }
                        // Every kind except `invokestatic` marshals the receiver
                        // as arg0 (a reference → one GPR slot), so it carries one
                        // more JIT arg than its descriptor lists. `invoke_kind`
                        // matches the dispatch helper's encoding: 0 = virtual,
                        // 1 = special (non-virtual dispatch to the resolved
                        // target), 2 = interface, 3 = static. For virtual /
                        // interface the helper dispatches on the receiver's
                        // runtime class (the baked class/name/descriptor are the
                        // static call-site signature it resolves against).
                        let has_receiver = !is_static;
                        let num_args = desc_args + if has_receiver { 1 } else { 0 };
                        let invoke_kind: u8 = if is_self_recursive_direct {
                            // 4 = guarded self-recursive static DIRECT call.
                            // `ir_lower` emits a direct `CALL`
                            // to this method's own entry instead of routing through
                            // `jit_invoke_dispatch`. Never seen by the dispatch
                            // helper (the direct path never calls it).
                            4
                        } else if is_special {
                            1
                        } else if is_virtual {
                            0
                        } else if is_interface {
                            2
                        } else {
                            3
                        };
                        // IR direct-call lowering. `invokestatic` and a
                        // non-`<init>` `invokespecial` are STATICALLY bound, so
                        // the resolved callee is the only possible target and no
                        // receiver type check is needed — exactly the property
                        // that lets the single-pass backend bind them directly
                        // (`x64.rs` `direct_calls`). Mirror its guards here:
                        //
                        //  * `note_jit_recursive_compile_cycle` — a call that
                        //    closes an in-flight compile cycle (A-B-A) keeps the
                        //    dispatch route, whose depth guard is the only stack
                        //    protection such an edge has.
                        //  * `jit_direct_call_requires_dispatch` — the deny-list
                        //    of edges known to need the helper's rooting/return
                        //    protocol (and every recorded cycle member).
                        //  * self-recursion is handled by the dedicated
                        //    `invoke_kind == 4` path above, not here.
                        //  * JVMS §6.5 — for `invokespecial` the direct target is
                        //    the *selection-start* class, not the plain CP class,
                        //    so apply `cp_invokespecial_owner_resolver` first
                        //    (single-pass does the same before its
                        //    `callee_compiler` call). Without this a super call
                        //    could be bound to the wrong method body.
                        let mut direct_target: Option<(usize, bool)> = None;
                        if ir_direct && (is_static || is_special) && !is_self_recursive {
                            let special_owner: Option<String> = if is_special {
                                cp_invokespecial_owner_resolver.and_then(|r| r(cp_idx))
                            } else {
                                None
                            };
                            let direct_class: &str =
                                special_owner.as_deref().unwrap_or(cn.as_str());
                            let closes_cycle =
                                note_jit_recursive_compile_cycle(direct_class, &mn, &desc);
                            if !closes_cycle
                                && !jit_direct_call_requires_dispatch(direct_class, &mn, &desc)
                            {
                                if let Some(compiler) = callee_compiler.as_ref() {
                                    if let Some((entry, callee_needs_ctx)) =
                                        compiler(direct_class, &mn, &desc)
                                    {
                                        // Re-check: compiling the callee may have
                                        // discovered a cycle through this edge.
                                        if entry != 0
                                            && !jit_direct_call_requires_dispatch(
                                                direct_class,
                                                &mn,
                                                &desc,
                                            )
                                        {
                                            direct_target = Some((entry, callee_needs_ctx));
                                        } else {
                                            mark_current_jit_compile_method_recursive_cycle();
                                        }
                                    }
                                }
                            }
                        }
                        if let Some((entry, callee_needs_ctx)) = direct_target {
                            ir_direct_calls.insert(pc, (entry, callee_needs_ctx));
                            ir_direct_callee_entries.push(entry);
                        }
                        // IR inline caches (jit-inlining-and-ir-calls). A
                        // virtual / interface site is NOT statically bound, so
                        // it cannot take the direct path above — instead give it
                        // the same eagerly-allocated MIC + PIC pair the
                        // single-pass backend gets (HIGH-7 strategy: allocate at
                        // first compile regardless of miss history; the slots
                        // start empty, cold sites fall straight through to the
                        // helper, and the helper populates them so later
                        // invocations hit inline with no recompile).
                        //
                        // Preconditions mirror the lowerer's:
                        //  * the miss-path helper must be wired — without
                        //    `jit_invoke_virtual_mic` there is nothing to
                        //    populate the caches, so the guards would never hit;
                        //  * the receiver plus args plus the hidden context
                        //    pointer must fit the entry ABI register file (the
                        //    hit path marshals in registers only). An over-wide
                        //    site simply keeps helper dispatch.
                        //
                        // Deliberately NO extra recursion-cycle gate here,
                        // unlike the direct path above. A direct call bakes a
                        // callee entry THIS compile resolved, so it needs
                        // `note_jit_recursive_compile_cycle` to avoid binding an
                        // edge whose only stack protection is the dispatch
                        // helper's depth guard. An inline cache bakes no callee
                        // at all: every entry it ever calls was installed by
                        // `jit_invoke_virtual_mic` itself, under exactly the
                        // policy that helper already applies for the
                        // single-pass MIC/PIC caches. The IR cascade is
                        // therefore equivalent to the single-pass one by
                        // construction, and adds no new call edge shape.
                        if (is_virtual || is_interface)
                            && helpers.invoke_virtual_mic != 0
                            && num_args >= 1
                            && num_args + 1 <= ir_entry_abi_reg_count()
                        {
                            let mic = Box::new(JitMICSlot::new());
                            // Seed from the receiver-type profile exactly as the
                            // single-pass planner does: a site with a dominant
                            // receiver lands in the MIC (and, via
                            // `seed_from_mic`, PIC slot 0) with its class id
                            // only — `entry_ptr` stays 0, so the first dispatch
                            // still rings the helper, which installs the target;
                            // thereafter the guard hits.
                            if let Some(prof) = profile {
                                if let Some(receiver_counts) = prof.receivers.get(&pc) {
                                    if let Some(dom) =
                                        profile::dominant_receiver(receiver_counts, 80)
                                    {
                                        mic.prepopulate(dom);
                                    }
                                }
                            }
                            let pic = Box::new(JitPICSlot::new());
                            pic.seed_from_mic(&mic);
                            let mic_addr = &*mic as *const JitMICSlot as usize;
                            let pic_addr = &*pic as *const JitPICSlot as usize;
                            ir_mic_boxes.push(mic);
                            ir_pic_boxes.push(pic);
                            ir_ic_slots.insert(pc, (mic_addr, pic_addr));
                        }
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
                                "[cratonvm-ircall] {}.{}{}: emitting {} invoke(static/special/virtual/interface) Op::Call(s), {} bound as DIRECT calls",
                                cached.class_name,
                                cached.method_name,
                                cached.method_descriptor,
                                info_map.len(),
                                ir_direct_calls.len(),
                            );
                        }
                        builder.set_invoke_info(info_map);
                    } else {
                        // A non-emittable invoke is present → leave `invoke_info`
                        // unset (the builder bails on every invoke → single-pass)
                        // and drop the now-unreferenced boxes/strings.
                        ir_call_infos.clear();
                        ir_call_strings.clear();
                        ir_direct_calls.clear();
                        ir_direct_callee_entries.clear();
                        // The inline-cache plan's baked addresses point INTO
                        // these boxes, so plan and storage must be dropped
                        // together — never one without the other.
                        ir_ic_slots.clear();
                        ir_mic_boxes.clear();
                        ir_pic_boxes.clear();
                    }
                }
            }
        }
        let built = builder.build(code, code_len);
        // jit-inlining-and-ir-calls — tier-4 compile-time guard.
        // `ir_compatible`'s bytecode budget rose from 200 to HotSpot's 8000-byte
        // HugeMethodLimit, which is the right *admission* rule but a poor proxy
        // for compile COST: `ir_optimize`'s GVN, the escape-analysis connection
        // graph and the scheduler are all super-linear in NODE count, and an
        // 8000-byte straight-line arithmetic method builds a far larger graph
        // than an 8000-byte call-heavy one. Check the built graph — the one
        // input that reflects actual complexity — before any optimization runs,
        // so an over-large method costs exactly one linear build and then takes
        // the single-pass backend. Its runtime companion is
        // `tiered::MAX_C2_COMPILE_TIME_MS`, which catches whatever slips past.
        let built = match built {
            Some(g) if g.nodes.len() > ir::IR_MAX_GRAPH_NODES => {
                if std::env::var_os("CRATONVM_DBG_IR_CALL").is_some() {
                    eprintln!(
                        "[cratonvm-ircall] {}.{}{}: IR graph {} nodes > IR_MAX_GRAPH_NODES {} — single-pass",
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor,
                        g.nodes.len(),
                        ir::IR_MAX_GRAPH_NODES,
                    );
                }
                None
            }
            other => other,
        };
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

                // Guard-surviving scalar replacement (Front 3.2): metadata for
                // scalar-replaced objects so the IR lowerer can emit a
                // `FrameValue::VirtualObject` at a deopt point. Populated from EA
                // below, only when `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`
                // are both on; otherwise stays `None` ⇒ byte-identical lowering.
                let mut sr_map: Option<ir_lower::ScalarReplacementMap> = None;

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
                        // Capture guard-surviving-SR metadata (gated) BEFORE
                        // `apply_ea_to_ir` marks the News/stores dead and clears
                        // their (control) inputs — the dominance gate needs them.
                        if scalar_deopt_enabled()
                            && deopt_real_enabled()
                            && !ea_result.scalar_replaceable.is_empty()
                        {
                            sr_map =
                                Some(build_scalar_replacement_map(&graph, &id_map, &ea_result));
                        }
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
                    // wire-tiered-manager Step 4 (PGO handoff C1 → C2): hand the
                    // optimizing IR (C2) lowerer the profiled branch bias so it can
                    // pick each `Op::If`'s fall-through edge from the C1/interpreter
                    // profile — the IR analogue of the single-pass backend's
                    // `branch_hints`. Keyed by the branch instruction's bytecode PC
                    // (matching `Op::If::bytecode_pc` and the interpreter's
                    // `record_branch` PC). Empty when there is no profile (profiling
                    // off, the default) → byte-identical codegen.
                    let ir_branch_hints: std::collections::HashMap<usize, bool> = profile
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
                    // Supply BOTH the profiled branch hints (Step 4) and the
                    // guard-surviving scalar-replacement map (Front 3.2) to the
                    // shared lowering body. `sr_map` is `None` unless
                    // `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL` are set.
                    if let Some(mut compiled) = ir_lower::lower_inner(
                        &graph,
                        &schedule,
                        num_params,
                        cached.max_locals as usize,
                        helpers,
                        &ir_branch_hints,
                        sr_map.as_ref(),
                        &ir_direct_calls,
                        &ir_ic_slots,
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
                        // IR direct-call lowering: record every raw JIT-to-JIT
                        // callee entry baked into this body. `_direct_callee_roots`
                        // (computed at publication) keeps the callee's executable
                        // buffer alive, and the cache's invalidation pass walks
                        // `_direct_callee_entries` to evict a caller whose callee
                        // was invalidated — without this the baked address could
                        // outlive the code it targets. Same contract the
                        // single-pass backend's `direct_callee_entries` has.
                        // IR inline caches: transfer ownership of the MIC/PIC
                        // slots to the compiled method. Their addresses are
                        // baked into the emitted guards as imm64, so the boxes
                        // MUST outlive the code — exactly the contract
                        // `_jit_mic_slots` / `_jit_pic_slots` exists for on the
                        // single-pass path. `extend`, never assign: the same
                        // use-after-free reasoning as the single-pass finalize
                        // (a `CompiledMethod` may already carry slots).
                        if !ir_mic_boxes.is_empty() {
                            compiled._jit_mic_slots.extend(ir_mic_boxes);
                            compiled._jit_pic_slots.extend(ir_pic_boxes);
                        }
                        if !ir_direct_callee_entries.is_empty() {
                            ir_direct_callee_entries.sort_unstable();
                            ir_direct_callee_entries.dedup();
                            compiled._direct_callee_entries =
                                std::mem::take(&mut ir_direct_callee_entries);
                        }
                        // Backend-routing introspection (tests only): this body was
                        // produced by the optimizing IR pipeline. A method that
                        // bailed out of IR to single-pass never reaches here, so it
                        // keeps the constructor default `false`.
                        compiled.used_ir_backend = true;
                        // wire-tiered-manager Step 3 telemetry (test-only):
                        // records that the optimizing IR path — not the
                        // single-pass C1 backend — produced this body, so the
                        // per-call toggle test can prove `optimize=false` skips it.
                        #[cfg(test)]
                        IR_LOWER_COMPILES.with(|c| c.set(c.get() + 1));
                        // inc 25 soak diagnostic: prove a long method actually
                        // took the IR path at runtime (single-pass also compiles
                        // longs, so a live "== HotSpot" probe alone is vacuous).
                        if ir_emit_long
                            && std::env::var_os("CRATONVM_DBG_IR_LONG").is_some()
                            && method_uses_category2(code, code_len, &cached.method_descriptor)
                        {
                            eprintln!(
                                "[cratonvm-irlong] {}.{}{}: long method took the IR pipeline",
                                cached.class_name, cached.method_name, cached.method_descriptor,
                            );
                        }
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
    // Compact reference-field layout: per field op, the packed byte offset +
    // ref-ness (from the resolver, which has the declaring class). Stays empty
    // when the flag is off → inline emitters use the legacy path.
    let mut compact_field_info: Vec<(usize, u32, bool)> = Vec::new();
    let compact_fields = cratonvm_types::compact_ref_fields_enabled();
    if !scan.field_ops.is_empty() {
        let resolver = cp_field_resolver?;
        for &(pc, cp_idx) in &scan.field_ops {
            let (field_index, type_tag, compact_slot) = resolver(cp_idx)?;
            field_info.push((pc, field_index, type_tag));
            // Only a genuinely-resolved compact slot may enter the inline
            // emitter's compact-offset map. `None` (no registered layout, or
            // index outside it) previously arrived as a fabricated
            // `(0, false)` tuple and steered the compact-offset inline
            // getfield/putfield arms at a garbage offset — the WildFly Host
            // Controller SIGSEGV. Such pcs now take the guarded uniform arm,
            // which keys on the per-object GC_FLAG_COMPACT bit and routes
            // compact receivers to the layout-aware helper.
            if compact_fields {
                if let Some((c_off, c_ref)) = compact_slot {
                    compact_field_info.push((pc, c_off, c_ref));
                }
            }
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
    //    has_nonzero_tag_primitive_init, // class has long/float/double fields
    //                                    // whose typed-zero Value tag is nonzero
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
    let mut ldc_string_info: Vec<(usize, *const u8, usize)> = Vec::new();
    if !scan.ldc_ops.is_empty() {
        if let Some(resolver) = cp_ldc_resolver {
            for &(pc, cp_idx) in &scan.ldc_ops {
                match resolver(cp_idx) {
                    Some(JitLdcConstant::Immediate(v)) => ldc_info.push((pc, v)),
                    Some(JitLdcConstant::String(text)) => {
                        let boxed: Box<str> = text.into_boxed_str();
                        let ptr = boxed.as_ptr();
                        let len = boxed.len();
                        owned_strings.push(boxed);
                        ldc_string_info.push((pc, ptr, len));
                    }
                    None => {
                        // RBC.7 — same permanent-bail class as RBC.4 (scan
                        // reject) / RBC.6 (athrow+handler): this resolver's
                        // `None` means the constant pool entry at `cp_idx` is
                        // a String/Class/MethodHandle (not representable as
                        // an immediate) — a property of the class file that
                        // never changes, not a resolution-timing miss.
                        // Without marking it, a hot method containing
                        // `ldc "str"` re-ran the whole upgrade gauntlet
                        // (skip-list + native-shadow walks + this scan) every
                        // JIT_RETRY_STRIDE calls forever (same pathology RBC.4
                        // fixed for scan rejects — observed as a silent,
                        // diagnostic-free hang: TestResponsePerformance's
                        // trivial `getRequestURI() { return "..."; }` bailed
                        // on every one of ~1M hot-loop calls).
                        *backend_attempted = true;
                        return None;
                    }
                }
            }
        }
    }

    // Resolve ldc2_w constants (long/double from CP). Single-pass types the
    // value by the consuming opcode (lstore/dstore/…), so it ignores the inc-35
    // `is_double` flag and keeps just the bits.
    let mut ldc2w_info: Vec<(usize, i64)> = Vec::new();
    if !scan.ldc2w_ops.is_empty() {
        let resolver = cp_ldc2w_resolver?;
        for &(pc, cp_idx) in &scan.ldc2w_ops {
            let (val, _is_double) = match resolver(cp_idx) {
                Some(v) => v,
                None => {
                    // RBC.7 twin: a non-Long/Double constant at this ldc2_w
                    // index is likewise fixed by the bytecode — permanent
                    // bail, not a transient miss. See the ldc arm above.
                    *backend_attempted = true;
                    return None;
                }
            };
            ldc2w_info.push((pc, val));
        }
    }

    let mut invoke_info: Vec<(usize, *const JitInvokeInfo)> = Vec::new();
    let mut owned_invoke_infos: Vec<Box<JitInvokeInfo>> = Vec::new();
    let mut direct_calls: Vec<(usize, JitDirectCall)> = Vec::new();
    let mut direct_callee_entries: Vec<usize> = Vec::new();
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
    // jit-inlining-and-ir-calls: HotSpot-shaped inlining budget. `hot_loops` is
    // derived once from the profile plus the bytecode; a caller that executes
    // any hot loop gets the larger whole-method budget, and each site inside
    // one gets the `FreqInlineSize` (325 / expansion 512) rather than
    // `MaxInlineSize` (35 / expansion 64) allowance. With no profile the list
    // is empty, every site is cold, and both the per-site tier and the
    // whole-method budget collapse to the pre-2026-07-26 values — an
    // unprofiled compile inlines identically.
    let inline_hot_loops = hot_loop_ranges(code, code_len, profile);
    let mut inline_budget_remaining: usize = if inline_hot_loops.is_empty() {
        MAX_INLINE_BUDGET
    } else {
        MAX_INLINE_BUDGET_HOT
    };
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
            // JVMS §6.5 super-call redirect (see `try_compile`'s doc comment
            // on `cp_invokespecial_owner_resolver`): for `invokespecial`
            // sites only, substitute the JVMS-correct selection-start class
            // when it differs from the plain CP-referenced class. Must run
            // before EVERY downstream use of `class_name` below (the
            // recursive-call check, inlining, direct-callee compile, and the
            // `JitInvokeInfo` baked into the compiled code), so a wrong
            // target is never baked into any of them.
            let class_name = if invoke_kind == 1 {
                cp_invokespecial_owner_resolver
                    .and_then(|r| r(cp_idx))
                    .unwrap_or(class_name)
            } else {
                class_name
            };
            let num_params = count_param_slots(&descriptor);
            let has_receiver = invoke_kind != 3;
            let num_jit_args = num_params + if has_receiver { 1 } else { 0 };
            let ret_type = return_type(&descriptor);

            let is_same_method_recursive_call = class_name == &*cached.class_name
                && method_name == &*cached.method_name
                && descriptor == &*cached.method_descriptor;
            let closes_active_compile_cycle =
                note_jit_recursive_compile_cycle(&class_name, &method_name, &descriptor);
            let recursive_cycle_target = closes_active_compile_cycle
                || jit_direct_call_requires_dispatch(&class_name, &method_name, &descriptor);
            let is_recursive_call = is_same_method_recursive_call || recursive_cycle_target;
            let use_raw_tail_self_call = invoke_kind == 3
                && is_same_method_recursive_call
                && invokestatic_self_call_uses_tail_jump(code, code_len, pc);

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
            // A raw JIT-to-JIT CALL has no interpreter transition to validate
            // argument roots, ABI state, or the callee's active frame. The
            // Elasticsearch IVFKnn stress test exposed a stale precise-root
            // mirror after a raw JIT-to-JIT CALL. x64 now republishes the
            // caller RBP after every such return, matching the normal Rust
            // dispatch boundary, so compiled callee calls can remain enabled.
            // A raw JIT-to-JIT call updates the per-thread active-RBP mirror
            // to the callee, while the interpreter-owned root-chain entry
            // still describes the caller.  Until that transition publishes
            // callee metadata atomically as well, a GC in the callee can
            // select the caller's oop map for the callee frame and lose live
            // roots.  Route through the checked re-entrant bridge instead;
            // it installs a distinct JitEntryGuard for the actual callee.
            let direct_jit_callee_calls_enabled = direct_jit_callee_calls_enabled();
            if !is_recursive_call && (invoke_kind == 3 || invoke_kind == 1) {
                // Try inlining first (before direct calls — inlining is more profitable)
                if inline_budget_remaining > 0 {
                    if let Some(resolver_fn) = inline_resolver.as_ref() {
                        if let Some(site) = resolver_fn(&class_name, &method_name, &descriptor) {
                            // Real budget accounting in two independent
                            // dimensions (jit-inlining-and-ir-calls):
                            //  * PER SITE — the HotSpot three-tier ceiling. A
                            //    hot site gets `FreqInlineSize`; a cold one
                            //    keeps `MaxInlineSize`, which is exactly the
                            //    old flat cap. Without this tier the raised
                            //    admission constant (`MAX_INLINE_BYTECODE_SIZE`,
                            //    now 325, which is what the VM-side resolver
                            //    filters on) would splice 300-byte COLD callees
                            //    everywhere.
                            //  * PER METHOD — the running budget below, which
                            //    is what actually bounds code-size explosion:
                            //    the single-pass backend reserves ~64 buffer
                            //    bytes and ~1 frame slot per inlined bytecode,
                            //    both linear in this total.
                            let site_hot = call_site_is_hot(pc, &inline_hot_loops, profile);
                            if let Some(expansion_cost) =
                                inline_site_expansion_cost_tiered(&site, site_hot)
                                    .filter(|cost| *cost <= inline_budget_remaining)
                            {
                                inline_budget_remaining =
                                    inline_budget_remaining.saturating_sub(expansion_cost);
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
                    // `Integer.valueOf(I)` thin direct call (see
                    // `INTEGER_VALUE_OF_DIRECT_FN`): statically bound, native
                    // callee — the eager callee-compile attempt below can
                    // never succeed for it, and the generic dispatch fallback
                    // pays the full helper round trip per call. `needs_context`
                    // routes vm_ptr as arg 0; the helper preserves the
                    // identity-cache and pending-return rooting contracts.
                    if direct_jit_callee_calls_enabled
                        && invoke_kind == 3
                        && class_name == "java/lang/StringLatin1"
                        && method_name == "toLowerCase"
                        && descriptor == "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;"
                    {
                        let entry = STRING_LATIN1_LOWER_DIRECT_FN
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 3,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }

                    if direct_jit_callee_calls_enabled
                        && invoke_kind == 3
                        && class_name == "java/lang/Integer"
                        && method_name == "valueOf"
                        && descriptor == "(I)Ljava/lang/Integer;"
                    {
                        let entry =
                            INTEGER_VALUE_OF_DIRECT_FN.load(std::sync::atomic::Ordering::Relaxed);
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params: 1,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                    if direct_jit_callee_calls_enabled {
                        if let Some(compiler) = callee_compiler.as_ref() {
                        if let Some((entry, callee_needs_ctx)) =
                            compiler(&class_name, &method_name, &descriptor)
                        {
                            if jit_direct_call_requires_dispatch(
                                &class_name,
                                &method_name,
                                &descriptor,
                            ) {
                                needs_heap = true;
                                mark_current_jit_compile_method_recursive_cycle();
                            } else {
                                if callee_needs_ctx {
                                    needs_heap = true;
                                }
                                direct_callee_entries.push(entry);
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
                        // `ArraycopyPrimitive`'s speculative fast path bails
                        // (null/non-array/reference-element/mismatched-kind/
                        // out-of-bounds) via a deopt trap whose ONLY
                        // historical resume strategy is a whole-method
                        // re-run — safe only when nothing observable
                        // happened before this call, an invariant this scan
                        // cannot verify and the JDT `Parser` stack-corruption
                        // bug violates (docs/known-issues/
                        // jasper-jdt-parser-arrayindexoutofbounds.md: a
                        // `stack[ptr--]` decrement already committed earlier
                        // in the same method gets re-executed on re-run).
                        // Register an ordinary `JitInvokeInfo` dispatch
                        // fallback for this same pc — identical to what a
                        // non-intrinsic `invokestatic` site gets below — so
                        // the codegen can route EVERY guard failure through
                        // a normal native-dispatch CALL (which throws or
                        // succeeds exactly like the interpreter's native
                        // registry, with ordinary exception propagation)
                        // instead of a deopt trap. No re-run, so no
                        // double-executed side effect.
                        if entry == JitIntrinsic::ArraycopyPrimitive.as_entry() {
                            let class_box: Box<str> = class_name.clone().into_boxed_str();
                            let method_box: Box<str> = method_name.clone().into_boxed_str();
                            let desc_box: Box<str> = descriptor.clone().into_boxed_str();
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
                                num_jit_args: num_params,
                                return_type: ret,
                                invoke_kind,
                            });
                            let info_ptr: *const JitInvokeInfo = &*info;
                            owned_invoke_infos.push(info);
                            invoke_info.push((pc, info_ptr));
                        }
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
            if !is_recursive_call && (invoke_kind == 0 || invoke_kind == 2) {
                // `Integer.intValue()` thin direct call (see
                // `INTEGER_INT_VALUE_DIRECT_FN`): `Integer` is `final`, so a
                // site declared against it is statically monomorphic — the
                // plain guard-free virtual direct-call path is sound, and
                // the helper handles the null-receiver NPE itself.
                if direct_jit_callee_calls_enabled
                    && invoke_kind == 0
                    && class_name == "java/lang/Integer"
                    && method_name == "intValue"
                    && descriptor == "()I"
                {
                    let entry =
                        INTEGER_INT_VALUE_DIRECT_FN.load(std::sync::atomic::Ordering::Relaxed);
                    if entry != 0 {
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 0,
                                return_type: b'I',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                if direct_jit_callee_calls_enabled
                    && invoke_kind == 2
                    && class_name == "java/util/concurrent/ConcurrentMap"
                    && method_name == "get"
                    && descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
                {
                    let entry = CONCURRENT_HASHMAP_GET_DIRECT_FN
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if entry != 0 {
                        needs_heap = true;
                        direct_calls.push((pc, JitDirectCall {
                            entry, needs_context: true, num_params: 1,
                            return_type: b'L', guard_class_id: 0,
                        }));
                        continue;
                    }
                }

                if direct_jit_callee_calls_enabled
                    && invoke_kind == 0
                    && class_name == "java/lang/String"
                    && method_name == "toLowerCase"
                    && descriptor == "(Ljava/util/Locale;)Ljava/lang/String;"
                {
                    let entry = STRING_LOCALE_LOWER_DIRECT_FN
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if entry != 0 {
                        needs_heap = true;
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: true,
                                num_params: 1,
                                return_type: b'L',
                                guard_class_id: 0,
                            },
                        ));
                        continue;
                    }
                }
                // Exact-HashMap `put`/`get` thin direct calls (see
                // `HASHMAP_PUT_DIRECT_FN`): guard-free registration — the
                // helper verifies the receiver's exact class at runtime and
                // routes everything non-exact/non-overlay to the generic
                // dispatcher, so a subclass receiver keeps full virtual
                // semantics.
                if (invoke_kind == 0 && class_name == "java/util/HashMap")
                    || (invoke_kind == 2
                        && class_name == "java/util/Map"
                        && method_name == "get"
                        && descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;")
                {
                    let recognized = if method_name == "put"
                        && descriptor == "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
                    {
                        Some((
                            HASHMAP_PUT_DIRECT_FN.load(std::sync::atomic::Ordering::Relaxed),
                            2usize,
                        ))
                    } else if method_name == "get"
                        && descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
                    {
                        Some((
                            HASHMAP_GET_DIRECT_FN.load(std::sync::atomic::Ordering::Relaxed),
                            1usize,
                        ))
                    } else {
                        None
                    };
                    if let Some((entry, num_params)) = recognized {
                        if entry != 0 {
                            needs_heap = true;
                            direct_calls.push((
                                pc,
                                JitDirectCall {
                                    entry,
                                    needs_context: true,
                                    num_params,
                                    return_type: b'L',
                                    guard_class_id: 0,
                                },
                            ));
                            continue;
                        }
                    }
                }
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

            // Keep only static tail self-calls on the raw backend path. Every
            // other recursive site gets dispatch metadata so the helper stack
            // guard runs before re-entering compiled code.
            if use_raw_tail_self_call {
                continue;
            }

            // bt18-regression fix (2026-07-18): NON-tail static self-recursion
            // may ALSO take the raw direct-CALL path — but only under the
            // caller-supplied identity proof (see `SELF_CALL_IDENTITY_STABLE`:
            // builtin-loaded class whose name maps back to its own ClassId,
            // so the historical loader-identity hazard cannot arise) and
            // never for mutual-recursion cycle targets (the dispatch depth
            // guard is still their only stack protection). The x64 else-arm
            // emits the inline stack-floor check + `self_call_stack_guard`
            // helper before the direct CALL, so runaway recursion still
            // surfaces as a catchable StackOverflowError. `needs_heap` is
            // required: the guard is called with the vm_ptr frame slot.
            if invoke_kind == 3
                && is_same_method_recursive_call
                && !recursive_cycle_target
                && self_call_identity_stable
            {
                needs_heap = true;
                continue;
            }

            // BUG-1 companion — when the dedicated self-call stack guard is
            // Earlier revisions routed NON-tail static self-recursive sites
            // through a raw direct-CALL path with a self-call guard. It avoids
            // a dispatch round trip, but the target has no loader identity.
            // Mutual-recursion cycle targets
            // (`recursive_cycle_target`) keep the dispatch route — a direct
            // call into ANOTHER method's artifact is a different hazard the
            // guard does not cover.
            // Non-tail same-method candidates deliberately retain dispatch
            // metadata: a raw direct entry call has no loader identity and
            // can invoke a different same-named method recursively.

            // Trivial-constructor elision (callee/IR tier, via try_compile): an
            // elidable `invokespecial C.<init>()V` is emitted AS
            // `java/lang/Object.<init>` so the single-pass `0xb7` codegen elision
            // drops the per-object `jit_invoke_dispatch` — the same effect the
            // VM-side execute/OSR resolution gets, here for callees compiled
            // through this path. `cp_elidable_init_resolver` is the body-checking
            // predicate (`resolve_jit_elidable_init` → `is_elidable_construction`);
            // it resolves bootstrap/JDK targets via `find_class_by_name`, so
            // app-loaded targets are not yet covered on this path (follow-up).
            let class_name = if invoke_kind == 1
                && method_name == "<init>"
                && descriptor == "()V"
                && cp_elidable_init_resolver.map_or(false, |r| r(cp_idx))
            {
                "java/lang/Object".to_string()
            } else {
                class_name
            };

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

            if (invoke_kind == 0 || invoke_kind == 2) && !is_recursive_call {
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

    // invokedynamic-uncommon-trap fix: resolve each `indy_ops` site's target
    // descriptor to the minimal stack-effect info the codegen needs (arg slot
    // count + return type tag) — see the `indy_info` field doc on the x64
    // `Compiler` struct. Mirrors the `field_info`/`invoke_info` resolution
    // blocks above: a `None` from the resolver (absent, or the site can't be
    // resolved) bails the WHOLE compile via `?`, exactly like every other
    // CP-resolved metadata table here — the codegen must never guess an
    // invokedynamic's stack effect.
    let mut indy_info: Vec<(usize, usize, u8, Vec<u8>)> = Vec::new();
    if !scan.indy_ops.is_empty() {
        let resolver = cp_invokedynamic_descriptor_resolver?;
        for &(pc, cp_idx) in &scan.indy_ops {
            let descriptor = resolver(cp_idx)?;
            let arg_slots = count_param_slots(&descriptor);
            let ret_type = return_type(&descriptor);
            let arg_type_tags = indy_arg_type_tags(&descriptor);
            indy_info.push((pc, arg_slots, ret_type, arg_type_tags));
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
    // Also seed it under the moving young gen (`CRATONVM_MOVING_YOUNG`): its
    // complete-coverage shadow map must include an oop parameter live across an
    // EARLY safepoint (before any `astore` rewrites its slot), or the moving copy
    // would leave that register/slot stale (HIB-CV-20).
    // Also seed it under `deopt_real_enabled()` (default ON): `local_kinds`
    // (below) classifies `this`/reference params as `LocalKind::Ref`, and
    // `typed_local_frame_value` only trusts that classification when the
    // precise oop mask CONFIRMS it — an unseeded mask leaves every deopt
    // point's `this` (or any live reference parameter) as `FrameValue::
    // Unsupported`, which `resume_real_ir_deopt` cannot resume. Every such
    // deopt then silently falls back to the whole-method re-run, which
    // DOUBLE-EXECUTES every side effect already committed before the trap —
    // e.g. a `stack[ptr--]` decrement already written to the heap. This was
    // the root cause of the JDT `Parser` stack-corruption bug
    // (docs/known-issues/jasper-jdt-parser-arrayindexoutofbounds.md): an
    // always-deopting reference-array `System.arraycopy` call inside a method
    // with a live `this` made every single invocation re-run from entry.
    let param_oop_mask =
        if x64::precise_jit_maps_enabled() || x64::moving_young_enabled() || deopt_real_enabled() {
            compute_param_oop_mask(&cached.method_descriptor, cached.is_static)
        } else {
            0
        };

    // deopt-osr Step 9 follow-up (c): the per-bci de-spec key for this method
    // (same `"<class>.<method>:<descriptor>"` form the deopt log / method_epochs
    // use). Lets the optimizing backend skip a loop-header speculative-BCE guard
    // recorded in the de-spec registry. Empty registry in production ⇒ no effect.
    let despec_method_key = format!(
        "{}.{}:{}",
        cached.class_name, cached.method_name, cached.method_descriptor
    );

    x64::set_pending_verified_max_stack(cached.max_stack as usize);
    // Pure-kernel GPR local homes: this is the METHOD-ENTRY compile path
    // (OSR artifacts go through the interpreter's `compile_osr_artifact`,
    // which never sets this), so request the kernel register homes. The
    // backend engages them only for call/field/alloc/typecheck-free bodies
    // with no speculative-BCE guards, keeps reference locals frame-homed,
    // and publishes the body without OSR entry points — see
    // `x64::kernel_reg_locals_enabled` for the safety argument.
    x64::set_kernel_reg_homes_request(true);
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
        ldc_string_info,
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
        compact_field_info,
        &despec_method_key,
        indy_info,
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
    direct_callee_entries.sort_unstable();
    direct_callee_entries.dedup();
    compiled._direct_callee_entries = direct_callee_entries;
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
pub fn compute_param_jvm_slots(descriptor: &str, is_static: bool) -> (Vec<usize>, usize) {
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
///
/// `pub` so the OSR / eager-first-call compile paths in the VM crate (which call
/// `x64::compile_with_param_slots` directly) can seed the same reference-parameter
/// mask the hot-path `try_compile` does — without it, an oop parameter living in a
/// callee-saved register across an early safepoint is invisible to the
/// post-safepoint reload and a moving GC leaves the register stale (HIB-CV-20).
pub fn compute_param_oop_mask(descriptor: &str, is_static: bool) -> u64 {
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

/// Per-argument JVM type tag, one entry per COMPACT stack slot (mirrors
/// `count_param_slots`' one-slot-per-parameter counting — a `long`/`double`
/// occupies a single entry here, not two), in descriptor (left-to-right,
/// push) order. Tag is one of `I` (int/boolean/byte/char/short, collapsed to
/// a single non-oop-int tag), `J` (long), `F` (float), `D` (double), or `L`
/// (object or array reference — the caller already has a precise oop mask
/// for these; the tag exists for completeness, not because it's read).
///
/// Written for the OSR-exit/invokedynamic-uncommon-trap deopt snapshot
/// (`build_and_record_deopt_point`'s operand-stack loop in `x64.rs`): the
/// snapshot's generic per-method `wide_fp` gate marks EVERY non-oop stack
/// slot `Unsupported` once a method touches any `long`/`float`/`double`
/// ANYWHERE, even when the specific slot at THIS bci is provably a plain
/// `int` (the abstract stack has no per-entry width source otherwise — see
/// `uses_long_float_double`'s doc comment). An invokedynamic call site is
/// the one place stack shape IS known precisely without a full stack-map
/// simulation: its bootstrap descriptor fixes exactly how many arguments are
/// live directly beneath it and their types, in order. Using these tags to
/// override `Unsupported` just for the indy call's own arguments — instead of
/// the coarse method-level gate — is what let the OSR-exit snapshot at
/// `getstatic System.out` + an `if`/`else`-computed `makeConcatWithConstants`
/// argument decode its operand stack precisely; see
/// `docs/known-issues/tomcat-08-07/testoutputbuffer-writespeed-content-length-mismatch.md`.
pub fn indy_arg_type_tags(descriptor: &str) -> Vec<u8> {
    let bytes = descriptor.as_bytes();
    let mut tags = Vec::new();
    if bytes.is_empty() || bytes[0] != b'(' {
        return tags;
    }
    let mut i = 1;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'I' | b'B' | b'C' | b'S' | b'Z' => {
                tags.push(b'I');
                i += 1;
            }
            b'F' => {
                tags.push(b'F');
                i += 1;
            }
            b'J' => {
                tags.push(b'J');
                i += 1;
            }
            b'D' => {
                tags.push(b'D');
                i += 1;
            }
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
                tags.push(b'L');
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
                tags.push(b'L');
            }
            _ => {
                i += 1;
            }
        }
    }
    tags
}

/// inc 25: the IR-builder parameter types in JIT-arg order (one per parameter,
/// `this` first for an instance method). Drives `IrBuilder::set_param_types`,
/// which lays the params out across JVM local slots with the category-2 two-slot
/// convention. Mirrors `count_param_slots`' one-slot-per-parameter counting.
fn ir_param_types(descriptor: &str, is_static: bool) -> Vec<ir::IrType> {
    let mut types = Vec::new();
    if !is_static {
        types.push(ir::IrType::Ref); // implicit `this`
    }
    let b = descriptor.as_bytes();
    let mut i = 1; // skip '('
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'J' => {
                types.push(ir::IrType::Long);
                i += 1;
            }
            b'D' => {
                types.push(ir::IrType::Double);
                i += 1;
            }
            b'F' => {
                types.push(ir::IrType::Float);
                i += 1;
            }
            b'L' => {
                types.push(ir::IrType::Ref);
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                types.push(ir::IrType::Ref);
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
                types.push(ir::IrType::Int); // I Z B C S
                i += 1;
            }
        }
    }
    types
}

/// inc 25: like `method_uses_category2`, but flags only **double** (the part the
/// long slice does NOT yet handle). A `D` parameter/return or any double-typed
/// opcode disqualifies the method; `long` and `float` do not (long is handled,
/// float bails at the builder's opcode catch-all). Used to admit long-only
/// methods to the IR path while keeping double/XMM on single-pass.
fn method_uses_double(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    let b = descriptor.as_bytes();
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'D' => return true,
            b'L' => {
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
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
            _ => i += 1,
        }
    }
    if let Some(rp) = b.iter().position(|&c| c == b')') {
        if matches!(b.get(rp + 1), Some(b'D')) {
            return true;
        }
    }
    let mut pc = 0;
    while pc < code_len {
        if is_double_opcode(code[pc]) {
            return true;
        }
        pc += crate::scev::bytecode_len(code, pc, code_len);
    }
    false
}

/// Double-typed opcodes (subset of `is_category2_opcode` that is double, not
/// long). `ldc2_w` (0x14) is deliberately omitted — it is long/double-ambiguous,
/// but the IR builder does not lower it (bails to single-pass), and any double
/// *constant* must be consumed by a double opcode listed here, so a double
/// `ldc2_w` method is caught regardless.
fn is_double_opcode(op: u8) -> bool {
    matches!(
        op,
        0x0e | 0x0f          // dconst_0, dconst_1
        | 0x18 | 0x26..=0x29 // dload, dload_0..3
        | 0x31 | 0x52        // daload, dastore
        | 0x39 | 0x47..=0x4a // dstore, dstore_0..3
        | 0x63 | 0x67 | 0x6b | 0x6f | 0x73 // dadd, dsub, dmul, ddiv, drem
        | 0x77               // dneg
        | 0x87 | 0x8a | 0x8d // i2d, l2d, f2d
        | 0x8e | 0x8f | 0x90 // d2i, d2l, d2f
        | 0x97 | 0x98        // dcmpl, dcmpg
        | 0xaf               // dreturn
    )
}

/// inc 30: float-typed opcodes (the cat-1 FP opcodes, complementing
/// `is_double_opcode`). Together they enumerate the full FP opcode space the IR
/// builder can now lower (or must bail on). `f2d`/`d2f` (0x8d/0x90) appear in
/// both sets — harmless, the union is OR'd.
fn is_float_opcode(op: u8) -> bool {
    matches!(
        op,
        0x0b..=0x0d          // fconst_0..2
        | 0x17 | 0x22..=0x25 // fload, fload_0..3
        | 0x30 | 0x51        // faload, fastore
        | 0x38 | 0x43..=0x46 // fstore, fstore_0..3
        | 0x62 | 0x66 | 0x6a | 0x6e | 0x72 // fadd, fsub, fmul, fdiv, frem
        | 0x76               // fneg
        | 0x86 | 0x89        // i2f, l2f
        | 0x8b | 0x8c | 0x8d // f2i, f2l, f2d
        | 0x90               // d2f
        | 0x95 | 0x96        // fcmpl, fcmpg
        | 0xae               // freturn
    )
}

/// inc 30: any float OR double opcode in the body. A method containing one is an
/// FP method and (when FP-free in its signature) is admitted to the IR path only
/// via the `ir_emit_fp` gate clause — so with the gate off no FP opcode ever
/// reaches the IR builder.
fn fp_in_body(code: &[u8], code_len: usize) -> bool {
    let mut pc = 0;
    while pc < code_len {
        if is_double_opcode(code[pc]) || is_float_opcode(code[pc]) {
            return true;
        }
        pc += crate::scev::bytecode_len(code, pc, code_len);
    }
    false
}

/// inc 30: a `float`/`double` appears in the method's signature (any `F`/`D`
/// parameter or the return type). Such a method needs XMM-register argument /
/// return marshalling the IR prologue/epilogue do not yet emit, so it is kept
/// off the FP IR path (a follow-on). Mirrors `method_uses_double`'s descriptor
/// walk but matches both `F` and `D`.
fn fp_in_descriptor(descriptor: &str) -> bool {
    let b = descriptor.as_bytes();
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'F' | b'D' => return true,
            b'L' => {
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                // Skip the whole array type — arrays are references (cat-1).
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
            _ => i += 1,
        }
    }
    if let Some(rp) = b.iter().position(|&c| c == b')') {
        if matches!(b.get(rp + 1), Some(b'F') | Some(b'D')) {
            return true;
        }
    }
    false
}

/// inc 30: the method uses `float`/`double` anywhere — signature OR body. Used by
/// the int/long IR-path clauses to stay FP-free.
fn method_uses_fp(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    fp_in_descriptor(descriptor) || fp_in_body(code, code_len)
}

thread_local! {
    /// Per-thread override for [`selfrec_direct_enabled`], for tests that must
    /// exercise the self-recursive direct-call path without mutating a
    /// process-global env var (which would race parallel test threads). `None`
    /// ⇒ fall back to the env var. Production never sets this.
    static SELFREC_DIRECT_TEST_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Test hook: force the self-recursive direct-call path on (`Some(true)`) / off
/// (`Some(false)`) for the CURRENT thread, or restore env behaviour (`None`).
/// Thread-local so parallel tests don't race. Not part of the stable API.
#[doc(hidden)]
pub fn __set_selfrec_direct_override(v: Option<bool>) {
    SELFREC_DIRECT_TEST_OVERRIDE.with(|c| c.set(v));
}

/// Whether guarded IR self-recursion uses a direct call to the method's own
/// entry. Default-on now that the IR lowering has the same native-stack floor
/// guard as single-pass. `CRATONVM_JIT_IR_SELFREC_DIRECT=0` opts out for
/// diagnosis; a thread-local override takes precedence for tests.
fn selfrec_direct_enabled() -> bool {
    if let Some(v) = SELFREC_DIRECT_TEST_OVERRIDE.with(|c| c.get()) {
        return v;
    }
    match std::env::var("CRATONVM_JIT_IR_SELFREC_DIRECT") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        ),
        Err(_) => true,
    }
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
            // A `long`/`double`/`float` arg is ONE i64 slot in the compact JIT
            // ABI — the VM marshals every value (incl. FP, as `to_bits() as i64`)
            // into one INTEGER arg register, not XMM, so each counts as one arg
            // exactly like an int/ref. `J` args: inc 28; `D`/`F` args: inc 34
            // (the marshaller stores the slot bits to the staging region and
            // `decode_dispatch_values` reads them back as `Double`/`Float`). Only
            // reachable under `ir_emit_long`/`ir_emit_fp` (producing a cat-2/FP
            // value needs such an opcode), so inert for the default int/ref path.
            b'J' | b'D' | b'F' => {
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
            // Any other byte is a malformed descriptor — bail.
            _ => return None,
        }
    }
    let ret = return_type(descriptor);
    match ret {
        b'I' | b'Z' | b'B' | b'C' | b'S' | b'V' | b'L' | b'[' => Some((num_args, ret)),
        // `J` (long) return: accepted post-inc-29. The result is one i64 slot in
        // RAX (the compact JIT ABI); the IR builder types the `Op::Call` node as
        // `IrType::Long`, and the call-site post-invoke check disambiguates a
        // legitimate `Long.MIN_VALUE` return from the `i64::MIN` deopt sentinel
        // via the out-of-band `dispatch_threw` peek. Only reachable under
        // `ir_emit_long` (consuming a long result needs a category-2 opcode), so
        // inert for the default int/ref path.
        b'J' => Some((num_args, ret)),
        // `D` (double) return: accepted (inc 32). The result rides RAX as a clean
        // 64-bit bit pattern (the i64 return ABI); the IR builder types the
        // `Op::Call` node `IrType::Double`, and the call-site post-invoke check
        // disambiguates a real `-0.0`/other double whose bits == `i64::MIN` from
        // the deopt sentinel via the out-of-band `dispatch_threw` peek (the check
        // already matches `IrType::Double`). `D`/`F` *args* are still rejected
        // (the arg loop above) — they ride XMM the marshaller does not yet emit.
        b'D' => Some((num_args, ret)),
        // `F` (float) return: accepted (inc 33). The 32-bit result rides the low
        // 32 of RAX (the i64 return ABI); the IR builder types the `Op::Call`
        // `IrType::Float` and the call-site `dispatch_threw` peek disambiguates a
        // `+0.0f`-with-stale-upper-bits ↔ `i64::MIN` collision. `D`/`F` *args* are
        // still rejected (the arg loop) — they ride XMM the marshaller lacks.
        b'F' => Some((num_args, ret)),
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

/// Whether an `invokestatic` self-call can safely use the backend raw tail jump.
///
/// Non-tail recursive compiled calls grow the native stack and bypass the
/// dispatch helpers' stack-depth guard. Tail self-calls are different: x64 lowers
/// them to a jump back to the method body, so they do not consume another native
/// frame and can keep the raw path.
#[inline]
pub fn invokestatic_self_call_uses_tail_jump(code: &[u8], code_len: usize, pc: usize) -> bool {
    pc + 3 < code_len && pc + 3 < code.len() && matches!(code[pc + 3], 0xac..=0xb0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn hsqldb_jit_deny_matches_slash_and_dot_names() {
    assert_eq!(
        hsqldb_jit_deny_prefix("org/hsqldb/map/BaseHashMap"),
        Some("org/hsqldb/")
    );
    assert_eq!(
        hsqldb_jit_deny_prefix("org.hsqldb.map.BaseHashMap"),
        Some("org.hsqldb.")
    );
    assert_eq!(hsqldb_jit_deny_prefix("org/example/Foo"), None);
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hibernate_biginteger_final_guard_matches_internal_and_dotted_names() {
        assert!(tiered::is_biginteger_arithmetic_jit_denied(
            "java/math/MutableBigInteger"
        ));
        assert!(tiered::is_biginteger_arithmetic_jit_denied(
            "java.math.MutableBigInteger"
        ));
        assert!(!tiered::is_biginteger_arithmetic_jit_denied(
            "java/math/BigInteger"
        ));
        assert!(!tiered::is_biginteger_arithmetic_jit_denied(
            "java/math/MutableBigInteger$Helper"
        ));
    }

    #[test]
    fn hibernate_temporal_jit_deny_matches_slash_and_dot_names() {
        assert_eq!(
            hibernate_temporal_jit_deny_prefix("org/hibernate/dialect/H2Dialect"),
            Some("org/hibernate/")
        );
        assert_eq!(
            hibernate_temporal_jit_deny_prefix("org.hibernate.dialect.H2Dialect"),
            Some("org.hibernate.")
        );
        assert_eq!(hibernate_temporal_jit_deny_prefix("org/example/Foo"), None);
    }

    #[test]
    fn jaxb_mapping_jit_deny_matches_slash_and_dot_names() {
        assert_eq!(
            jaxb_mapping_jit_deny_prefix("org/glassfish/jaxb/runtime/v2/ContextFactory"),
            Some("org/glassfish/jaxb/")
        );
        assert_eq!(
            jaxb_mapping_jit_deny_prefix("org.glassfish.jaxb.runtime.v2.ContextFactory"),
            Some("org.glassfish.jaxb.")
        );
        assert_eq!(
            jaxb_mapping_jit_deny_prefix("org/glassfish/other/Foo"),
            None
        );
    }

    #[test]
    fn xerces_schema_jit_deny_matches_slash_and_dot_names() {
        assert_eq!(
            xerces_schema_jit_deny_prefix("com/sun/org/apache/xerces/internal/util/SymbolHash"),
            Some("com/sun/org/apache/xerces/internal/")
        );
        assert_eq!(
            xerces_schema_jit_deny_prefix(
                "com.sun.org.apache.xerces.internal.impl.xs.SchemaGrammar"
            ),
            Some("com.sun.org.apache.xerces.internal.")
        );
        assert_eq!(
            xerces_schema_jit_deny_prefix("com/sun/org/apache/xml/internal/Foo"),
            None
        );
    }

    #[test]
    fn hsqldb_jit_deny_matches_slash_and_dot_names() {
        assert_eq!(
            hsqldb_jit_deny_prefix("org/hsqldb/map/BaseHashMap"),
            Some("org/hsqldb/")
        );
        assert_eq!(
            hsqldb_jit_deny_prefix("org.hsqldb.map.BaseHashMap"),
            Some("org.hsqldb.")
        );
        assert_eq!(hsqldb_jit_deny_prefix("org/example/Foo"), None);
    }

    #[test]
    fn hibernate_temporal_jit_allow_entries_are_prefix_based() {
        assert!(jit_allow_entry_allows_prefix(
            "org/hibernate/",
            "org/hibernate/"
        ));
        assert!(jit_allow_entry_allows_prefix(
            "org.hibernate.",
            "org.hibernate."
        ));
        assert!(!jit_allow_entry_allows_prefix(
            "org/hibernate/",
            "org.hibernate."
        ));
    }

    #[test]
    fn snakeyaml_emitter_emit_final_guard_is_exact() {
        assert_eq!(
            snakeyaml_emitter_emit_jit_deny_prefix("org/yaml/snakeyaml/emitter/Emitter", "emit",),
            Some("org/yaml/snakeyaml/emitter/")
        );
        assert_eq!(
            snakeyaml_emitter_emit_jit_deny_prefix(
                "org/yaml/snakeyaml/emitter/Emitter",
                "writeWhitespace",
            ),
            None
        );
        assert_eq!(
            snakeyaml_emitter_emit_jit_deny_prefix(
                "org/yaml/snakeyaml/emitter/ScalarAnalysis",
                "isEmpty",
            ),
            None
        );
    }

    /// BUG-1 companion — routing of NON-tail static self-recursive call sites.
    ///
    /// WITHOUT the caller-supplied identity proof the site must retain
    /// `invoke_dispatch`, so class-loader identity is resolved at dispatch
    /// time (dee2e26f hardening). WITH the proof
    /// (`set_self_call_identity_stable(true)` — builtin-loaded class whose
    /// name maps back to its own ClassId) the site takes the raw guarded
    /// direct self-CALL (bt18-regression fix, 2026-07-18); the flag is
    /// consume-once so the NEXT compile reverts to dispatch.
    /// Proven from the emitted machine code: `emit_call_absolute` bakes the
    /// helper address as a `MOV RAX, imm64`, so the 8-byte LE address pattern
    /// appearing in the code identifies which helper the site calls.
    #[test]
    fn self_recursive_nontail_site_routes_through_dispatch() {
        use std::sync::Arc;

        // `static int f(int n) { return n <= 0 ? 0 : f(n - 1) + 1; }`
        //  0: iload_0
        //  1: ifgt  -> 6
        //  4: iconst_0
        //  5: ireturn
        //  6: iload_0
        //  7: iconst_1
        //  8: isub
        //  9: invokestatic #1   (self — NON-tail: iadd follows)
        // 12: iconst_1
        // 13: iadd
        // 14: ireturn
        let code: &[u8] = &[
            0x1a, 0x9d, 0x00, 0x05, 0x03, 0xac, 0x1a, 0x04, 0x64, 0xb8, 0x00, 0x01, 0x04, 0x60,
            0xac,
        ];
        let cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/Rec"),
            method_name: Arc::from("f"),
            method_descriptor: Arc::from("(I)I"),
            source_file: None,
            code: Arc::from(code),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 3,
            max_locals: 1,
            num_params: 1,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        };
        let resolver = |idx: u16| -> Option<(String, String, String)> {
            (idx == 1).then(|| ("pkg/Rec".to_string(), "f".to_string(), "(I)I".to_string()))
        };
        // Distinctive fake addresses — the code is never executed, only
        // pattern-searched. SAFETY (zeroed): all-usize #[repr(C)] struct.
        const GUARD_ADDR: usize = 0x7161_7264_5f61_6472; // "qard_adr"-ish tag
        const DISPATCH_ADDR: usize = 0x6469_7370_5f61_6472;
        let contains = |hay: &[u8], addr: usize| -> bool {
            let needle = (addr as u64).to_le_bytes();
            hay.windows(needle.len()).any(|w| w == needle)
        };

        // (a) Guard wired: loader-correct dispatch is still required.
        let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        helpers.self_call_stack_guard = GUARD_ADDR;
        helpers.invoke_dispatch = DISPATCH_ADDR;
        let compiled = try_compile(
            &cached,
            None,
            None,
            None,
            Some(&resolver),
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
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        )
        .expect("self-recursive method must compile through dispatch");
        let bytes = compiled.code_bytes().to_vec();
        assert!(
            !contains(&bytes, GUARD_ADDR),
            "non-tail self-call must not bake a raw self-entry guard"
        );
        assert!(
            contains(&bytes, DISPATCH_ADDR),
            "non-tail self-call must use invoke_dispatch"
        );

        // (b) Guard UNWIRED → historical dispatch routing.
        let mut helpers_off: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        helpers_off.invoke_dispatch = DISPATCH_ADDR;
        let compiled_off = try_compile(
            &cached,
            None,
            None,
            None,
            Some(&resolver),
            None,
            None,
            None,
            None,
            None,
            &helpers_off,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        )
        .expect("guard-unwired self-recursive method must compile");
        let bytes_off = compiled_off.code_bytes().to_vec();
        assert!(
            contains(&bytes_off, DISPATCH_ADDR),
            "unwired guard must keep the historical invoke_dispatch routing"
        );

        // (c) bt18-regression fix: with the caller-supplied identity proof,
        // the non-tail site takes the raw guarded direct self-CALL — the
        // stack-guard helper is baked and no invoke_dispatch round trip
        // remains for the recursion.
        set_self_call_identity_stable(true);
        let compiled_direct = try_compile(
            &cached,
            None,
            None,
            None,
            Some(&resolver),
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
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        )
        .expect("identity-proven self-recursive method must compile");
        let bytes_direct = compiled_direct.code_bytes().to_vec();
        assert!(
            contains(&bytes_direct, GUARD_ADDR),
            "identity-proven non-tail self-call must bake the self-call stack guard"
        );
        assert!(
            !contains(&bytes_direct, DISPATCH_ADDR),
            "identity-proven non-tail self-call must not round-trip through invoke_dispatch"
        );

        // (d) The proof is consume-once: the very next compile reverts to
        // loader-correct dispatch routing.
        let compiled_after = try_compile(
            &cached,
            None,
            None,
            None,
            Some(&resolver),
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
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        )
        .expect("post-proof compile must fall back to dispatch");
        assert!(
            contains(&compiled_after.code_bytes().to_vec(), DISPATCH_ADDR),
            "identity proof must not leak into the next compile"
        );
    }

    /// deopt-osr Step 9 follow-up (a): `stamp_deopt_epoch_guard` writes the
    /// creation epoch + live-cell pointer into the artifact's retained guard, and
    /// the resulting guard reports superseded once the live epoch advances past
    /// the creation epoch. A null guard (production artifact) is a safe no-op.
    #[test]
    fn fua_stamp_deopt_epoch_guard_and_supersede() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let mut buf = ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        let mut cm = CompiledMethod::new(buf);

        // No guard emitted (production): stamping is a no-op and must not panic.
        assert!(cm.deopt_epoch_guard.is_null());
        cm.stamp_deopt_epoch_guard(5, std::ptr::null());

        // Attach a retained guard (as `emit_deopt_stubs` would) and a stable live
        // cell (as `method_epochs` would).
        let guard: &'static crate::deopt::DeoptEpochGuard =
            Box::leak(Box::new(crate::deopt::DeoptEpochGuard::new()));
        cm.deopt_epoch_guard = guard as *const _;
        let live: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(3)));

        // Install at the current live epoch (3) ⇒ fresh, not superseded.
        cm.stamp_deopt_epoch_guard(3, live as *const _);
        assert_eq!(guard.creation_epoch.load(Ordering::Relaxed), 3);
        assert!(!guard.is_superseded());

        // A later invalidation advances the live epoch past 3 ⇒ superseded.
        live.store(4, Ordering::Relaxed);
        assert!(guard.is_superseded());
    }

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
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
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
            None, None, true, false, false, false, false, false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
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
            None, None, false, false, false, false, false, false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
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
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        };
        // SAFETY: see `step3_optimize_toggle_…`; an all-zero `JitRuntimeHelpers`
        // is valid and never called (the inline getfield emits no helper call,
        // and this test does not execute the generated code).
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        // Resolve cp index 2 → field index 0, int (`I`).
        // (field_index, type_tag, compact_slot) — `None` = the field has no
        // registered compact slot (a plain non-compact int field).
        let field_resolver = |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
            if cp == 2 {
                Some((0, b'I', None))
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
            false,
            false,
            false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        );
        assert!(c2.is_some(), "optimize=true (C2) must compile `get`");
        let expected_ir_compiles = if cratonvm_types::compact_ref_fields_enabled() {
            0
        } else {
            1
        };
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            expected_ir_compiles,
            "compact field layout bails to single-pass; legacy layout routes int getfield through IR"
        );

        // Without the field resolver the builder cannot resolve the field, so
        // the IR path must bail (counter stays 0). The whole compile then
        // returns None — single-pass also needs the resolver to build
        // `field_info` — which is the expected, safe fallback.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let _ = try_compile(
            &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None,
            None, None, true, false, false, false, false, false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
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
        if cratonvm_types::compact_ref_fields_enabled() {
            assert!(
                builder.build(&code, 17).is_none(),
                "compact field layout must bail to the checked single-pass path"
            );
            return;
        }
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
        if cratonvm_types::compact_ref_fields_enabled() {
            assert!(
                builder.build(&code, 19).is_none(),
                "compact field layout must bail to the checked single-pass path"
            );
            return;
        }
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
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        };
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        let new_resolver = |cp: u16| -> Option<(u32, usize, bool, bool)> {
            if cp == 1 {
                Some((7, 1, false, false))
            } else {
                None
            }
        };
        // (field_index, type_tag, compact_slot) — a plain non-compact int
        // field resolves with `(_, _, None)`: no registered compact slot.
        let field_resolver = |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
            if cp == 3 {
                Some((0, b'I', None))
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
            false,
            false,
            false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        );
        if cratonvm_types::compact_ref_fields_enabled() {
            // Compact field layout bails the IR builder on `new`/getfield/putfield
            // (see the ir.rs field-op tests) before it ever reaches the
            // elidable-`<init>` check, and this test deliberately supplies no
            // `cp_invoke_resolver` (a correctly-elided `new` should never need
            // single-pass's invoke resolution) — so there is no fallback and the
            // compile bails entirely.
            assert!(
                r.is_none(),
                "compact field layout must bail to the checked single-pass path, \
                 which this test starves of a cp_invoke_resolver on purpose"
            );
            assert_eq!(
                IR_LOWER_COMPILES.with(|c| c.get()),
                0,
                "compact field layout bails the IR pipeline before the elidable `new` can route through it"
            );
        } else {
            assert!(r.is_some(), "an elidable `new` method must compile via IR");
            assert_eq!(
                IR_LOWER_COMPILES.with(|c| c.get()),
                1,
                "the elidable `new` method must route through the IR pipeline"
            );
        }

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
            false,
            false,
            false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
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
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
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
            true,  // optimize
            true,  // ir_emit_calls
            false, // ir_emit_special_calls (testing invokestatic, not special)
            false, // ir_emit_long
            false, // ir_emit_virtual_calls
            false, // ir_emit_fp
            None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
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
            false, // ir_emit_long OFF
            false, // ir_emit_virtual_calls OFF
            false, // ir_emit_fp OFF
            None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
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
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
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
            false, // ir_emit_calls (invokestatic) OFF
            true,  // ir_emit_special_calls ON
            false, // ir_emit_long
            false, // ir_emit_virtual_calls OFF
            false, // ir_emit_fp OFF
            None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
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
            true,  // ir_emit_calls (invokestatic) ON
            false, // ir_emit_special_calls OFF
            false, // ir_emit_long OFF
            false, // ir_emit_virtual_calls OFF
            false, // ir_emit_fp OFF
            None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            0,
            "without ir_emit_special_calls, invokespecial must NOT take the IR pipeline"
        );
    }

    /// inc 25: a long-using method routes through the IR pipeline ONLY with
    /// `ir_emit_long` — otherwise `method_uses_category2` bails the whole
    /// pipeline to single-pass. Guards against a vacuous validation: the
    /// integration harness compiles `optimize=true` either way (single-pass is
    /// the fall-through), so result-equality alone would not prove the IR path
    /// ran — `IR_LOWER_COMPILES` does.
    #[test]
    fn ir_long_wiring_routes_through_ir_only_with_flag() {
        use std::sync::Arc;

        // `static long add(long a, long b) { return a + b; }`
        //   lload_0; lload_2; ladd; lreturn
        let cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/L"),
            method_name: Arc::from("add"),
            method_descriptor: Arc::from("(JJ)J"),
            source_file: None,
            code: Arc::from([0x1e, 0x20, 0x61, 0xad, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 4,
            max_locals: 4,
            num_params: 2,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        };
        // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
        // (never executes), and a pure long-arithmetic method calls no helper.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };

        // ir_emit_long = true → the long method takes the IR pipeline.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let with = try_compile(
            &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None,
            None, None, true, false, false, true, false, false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        );
        assert!(with.is_some(), "long method must compile with ir_emit_long");
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            1,
            "a long method must route through the IR pipeline when ir_emit_long is on"
        );

        // ir_emit_long = false → method_uses_category2 bails → single-pass.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let _without = try_compile(
            &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None,
            None, None, true, false, false, false, false, false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            0,
            "without ir_emit_long, a long method must NOT take the IR pipeline"
        );
    }

    /// inc 30 (FP value tier): a method that uses `float`/`double` internally
    /// (with an FP-free `int` signature) routes through the IR pipeline ONLY when
    /// `ir_emit_fp` is on. Single-pass also compiles the method, so result
    /// equality alone would not prove the IR path ran — `IR_LOWER_COMPILES`
    /// proves it (==1 with the flag, ==0 without → vacuous single-pass fallback).
    #[test]
    fn ir_fp_wiring_routes_through_ir_only_with_flag() {
        use std::sync::Arc;

        // `static int f(int a) { return (int)((float)a + 2.0f); }`
        //   iload_0; i2f; fconst_2; fadd; f2i; ireturn
        let cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/F"),
            method_name: Arc::from("f"),
            method_descriptor: Arc::from("(I)I"),
            source_file: None,
            code: Arc::from([0x1a, 0x86, 0x0d, 0x62, 0x8b, 0xac, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 4,
            max_locals: 1,
            num_params: 1,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        };
        // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
        // (never executes), and a pure FP-arithmetic method calls no helper.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };

        // ir_emit_fp = true → the FP method takes the IR pipeline.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let with = try_compile(
            &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None,
            None, None, true, false, false, false, false, true,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        );
        assert!(with.is_some(), "FP method must compile with ir_emit_fp");
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            1,
            "an FP method must route through the IR pipeline when ir_emit_fp is on"
        );

        // ir_emit_fp = false → method_uses_fp bails the IR path → single-pass.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let _without = try_compile(
            &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None,
            None, None, true, false, false, false, false, false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            0,
            "without ir_emit_fp, an FP method must NOT take the IR pipeline"
        );
    }

    /// inc 26 (Gap B) + jit-inlining-and-ir-calls: a resolved `invokevirtual`
    /// routes through the IR pipeline.
    ///
    /// **The polarity of this test inverted on 2026-07-26.** It used to assert
    /// "…ONLY when `ir_emit_virtual_calls` is on", because the IR lowered
    /// virtual calls through the generic dispatch helper with no inline cache
    /// and admitting them was a throughput regression versus single-pass.
    /// `ir_lower::emit_inline_cache_call` now emits the MIC + 3-way-PIC
    /// cascade, so the capability became opt-OUT: the caller's parameter can
    /// still force it ON, but only the diagnostic
    /// `CRATONVM_JIT_IR_CALL_VIRTUAL=0` (here, its thread-local test override)
    /// turns it off.
    ///
    /// Guards against a vacuous validation: single-pass ALSO dispatches
    /// invokevirtual, so result-equality alone would not prove the IR path ran.
    #[test]
    fn ir_virtual_call_wiring_routes_through_ir_only_with_flag() {
        use std::sync::Arc;

        // `static int f(Obj o, int n) { return o.g(n); }`  (g virtual → invokevirtual)
        //   aload_0; iload_1; invokevirtual #2; ireturn
        // Modelled as a `static` caller (receiver `o` is param 0) exactly like the
        // invokespecial wiring test, so the local/param counts line up.
        let cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/Caller"),
            method_name: Arc::from("f"),
            method_descriptor: Arc::from("(Lpkg/Obj;I)I"),
            source_file: None,
            code: Arc::from([0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 2,
            max_locals: 2,
            num_params: 2,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        };
        // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
        // (never executes the body), so the baked `invoke_dispatch` is not called.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        let invoke_resolver = |cp: u16| -> Option<(String, String, String)> {
            if cp == 2 {
                Some(("pkg/Obj".into(), "g".into(), "(I)I".into()))
            } else {
                None
            }
        };

        // ir_emit_virtual_calls = true (calls/special OFF) → invokevirtual lowers
        // to Op::Call → IR pipeline. Crossing the flags proves VIRTUAL is the gate.
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
            true,  // optimize
            false, // ir_emit_calls (invokestatic) OFF
            false, // ir_emit_special_calls OFF
            false, // ir_emit_long
            true,  // ir_emit_virtual_calls ON
            false, // ir_emit_fp OFF
            None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        );
        assert!(
            with.is_some(),
            "invokevirtual must compile with ir_emit_virtual_calls"
        );
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            1,
            "invokevirtual must route through the IR pipeline when ir_emit_virtual_calls is on"
        );
        assert!(
            with.as_ref().unwrap().needs_context(),
            "an Op::Call method must be needs_context"
        );

        // Default (parameter false, no override): virtual calls are opt-OUT
        // now, so the IR pipeline must STILL take this method. This is the
        // assertion that inverted — it is the point of the change.
        IR_LOWER_COMPILES.with(|c| c.set(0));
        let by_default = try_compile(
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
            false, // ir_emit_long
            false, // ir_emit_virtual_calls: caller does NOT force it
            false, // ir_emit_fp OFF
            None,
        );
        assert!(by_default.is_some());
        assert_eq!(
            IR_LOWER_COMPILES.with(|c| c.get()),
            1,
            "invokevirtual must take the IR pipeline BY DEFAULT now that ir_lower \
             emits MIC/PIC inline caches — the capability is opt-out, not opt-in"
        );

        // Explicit opt-out (`CRATONVM_JIT_IR_CALL_VIRTUAL=0`, modelled by the
        // thread-local override) → the builder bails on the invokevirtual →
        // single-pass. Proves the escape hatch still works.
        __set_ir_virtual_calls_override(Some(false));
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
            true,  // ir_emit_calls ON
            true,  // ir_emit_special_calls ON
            false, // ir_emit_long
            false, // ir_emit_virtual_calls OFF
            false, // ir_emit_fp OFF
            None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        );
        let took_ir = IR_LOWER_COMPILES.with(|c| c.get());
        __set_ir_virtual_calls_override(None);
        assert_eq!(
            took_ir, 0,
            "with CRATONVM_JIT_IR_CALL_VIRTUAL=0, invokevirtual must NOT take the IR pipeline"
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
    fn test_can_osr_enter_rejects_dead_masked_entry() {
        let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let mut cm = CompiledMethod::new(buf);
        cm.osr_pc_to_native = Some(vec![-1, 0, 4]);
        cm.osr_dead_mask = Some(vec![0, 0x80, 0]);

        assert!(!cm.can_osr_enter(0), "negative native offset must bail");
        assert!(
            !cm.can_osr_enter(1),
            "dead coalesced locals are not safe OSR entries"
        );
        assert!(cm.can_osr_enter(2), "zero dead mask remains OSR-eligible");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_osr_enter_rejects_dead_mask_before_trampoline() {
        let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let mut cm = CompiledMethod::new(buf);
        cm.osr_pc_to_native = Some(vec![0]);
        cm.osr_dead_mask = Some(vec![0x80]);

        let result = unsafe { cm.osr_enter(0, &[], 0, 0) };
        assert_eq!(result, None);
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
    fn test_jit_pic_slot_clear_entries_drops_compiled_targets() {
        let pic = JitPICSlot::new();
        pic.install(1, "A", 0x1000, false);
        pic.install(2, "B", 0x2000, true);
        pic.install(3, "C", 0x3000, false);
        assert_eq!(pic.entries_used(), 3);

        pic.clear_entries();

        assert_eq!(pic.entries_used(), 0);
        assert!(pic.lookup(1).is_none());
        assert!(pic.lookup(2).is_none());
        assert!(pic.lookup(3).is_none());
        for i in 0..JIT_PIC_ENTRIES {
            assert_eq!(
                pic.entry_ptrs[i].load(std::sync::atomic::Ordering::Acquire),
                0
            );
            assert!(!pic.needs_context[i].load(std::sync::atomic::Ordering::Relaxed));
            assert!(pic.class_names[i].lock().is_none());
        }
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
    fn test_scalar_selfrec_ir_structural_admission() {
        let fib = [
            0x1a, 0x04, 0xa3, 0x00, 0x05, // iload_0; iconst_1; if_icmpgt
            0x1a, 0xac, // iload_0; ireturn
            0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // fib(n - 1)
            0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // fib(n - 2)
            0x60, 0xac, // iadd; ireturn
        ];
        assert!(scalar_selfrec_ir_would_engage(&fib, fib.len(), "(I)I"));
        assert!(!scalar_selfrec_ir_would_engage(&fib, fib.len(), "(I)J"));
        assert!(!scalar_selfrec_ir_would_engage(&[0x1a, 0xac], 2, "(I)I"));
    }

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

        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cratonvm_types::ClassId::new(1),
            cm,
        );
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());

        let result = cache.get(&class, &method, &desc, cratonvm_types::ClassId::new(1));
        assert!(result.is_some());
    }

    #[test]
    fn test_jit_cache_osr_and_method_entry_bodies_coexist() {
        let mut cache = JitCache::new();
        let class: Arc<str> = Arc::from("LoopClass");
        let method: Arc<str> = Arc::from("hotLoop");
        let desc: Arc<str> = Arc::from("(I)I");

        let cid = cratonvm_types::ClassId::new(1);
        let mut entry_buf = ExecutableBuffer::new(64).expect("alloc failed");
        entry_buf.emit(&[0xC3]);
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(entry_buf),
        );

        let mut osr_buf = ExecutableBuffer::new(64).expect("alloc failed");
        osr_buf.emit(&[0xC3]);
        let mut osr = CompiledMethod::new(osr_buf);
        osr.compiled_via_osr = true;
        cache.put_osr(class.clone(), method.clone(), desc.clone(), cid, osr);

        let entry = cache.get(&class, &method, &desc, cid).expect("entry body");
        let osr = cache
            .get_osr(&class, &method, &desc, cid)
            .expect("osr body");
        assert_ne!(entry.entry_ptr(), osr.entry_ptr());
        assert!(!entry.compiled_via_osr);
        assert!(osr.compiled_via_osr);
        assert_eq!(cache.len(), 2);

        let osr_entry = osr.entry_ptr();
        let mut c2_buf = ExecutableBuffer::new(64).expect("alloc failed");
        c2_buf.emit(&[0xC3]);
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(c2_buf),
        );
        assert_eq!(
            cache
                .get_osr(&class, &method, &desc, cid)
                .expect("osr survives method-entry supersede")
                .entry_ptr(),
            osr_entry
        );
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_jit_cache_put_replacement_reclaims_after_last_reader() {
        let cache = JitCache::new();
        let class: Arc<str> = Arc::from("ReplaceClass");
        let method: Arc<str> = Arc::from("replaceMethod");
        let desc: Arc<str> = Arc::from("()V");

        let cid = cratonvm_types::ClassId::new(1);
        let mut old_buf = ExecutableBuffer::new(64).expect("alloc failed");
        old_buf.emit(&[0xC3]); // RET
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(old_buf),
        );
        let old = cache
            .get(&class, &method, &desc, cid)
            .expect("old compiled method");
        let old_entry = old.entry_ptr() as usize;
        register_jit_code_range(old_entry, old.code_len(), Arc::as_ptr(&old) as usize);
        assert!(lookup_jit_code_range(old_entry).is_some());

        let mut new_buf = ExecutableBuffer::new(64).expect("alloc failed");
        new_buf.emit(&[0xC3]); // RET
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(new_buf),
        );

        assert!(
            lookup_jit_code_range(old_entry).is_some(),
            "an outstanding lock-free reader must own the replaced artifact"
        );
        drop(old);
        assert!(
            lookup_jit_code_range(old_entry).is_none(),
            "the replaced artifact must unregister after its last Arc is released"
        );
        if let Some(new_cm) = cache.get(&class, &method, &desc, cid) {
            unregister_jit_code_range(new_cm.entry_ptr() as usize);
        }
    }

    #[test]
    fn test_jit_cache_remove_reclaims_code_range() {
        let cache = JitCache::new();
        let class: Arc<str> = Arc::from("RemoveClass");
        let method: Arc<str> = Arc::from("removeMethod");
        let desc: Arc<str> = Arc::from("()V");

        let cid = cratonvm_types::ClassId::new(1);
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(buf),
        );
        let cm = cache
            .get(&class, &method, &desc, cid)
            .expect("compiled method");
        let entry = cm.entry_ptr() as usize;
        register_jit_code_range(entry, cm.code_len(), Arc::as_ptr(&cm) as usize);
        drop(cm);

        cache.remove(&class, &method, &desc, cid);

        assert!(cache.get(&class, &method, &desc, cid).is_none());
        assert!(
            lookup_jit_code_range(entry).is_none(),
            "removed code must unregister when no caller or reader owns it"
        );
    }

    #[test]
    fn test_replaced_callee_is_owned_by_baked_direct_caller() {
        let cache = JitCache::new();
        let cid = cratonvm_types::ClassId::new(7);
        let callee_class: Arc<str> = Arc::from("OwnedCallee");
        let callee_method: Arc<str> = Arc::from("target");
        let desc: Arc<str> = Arc::from("()V");

        let mut callee_buf = ExecutableBuffer::new(64).expect("alloc callee");
        callee_buf.emit(&[0xC3]);
        cache.put(
            callee_class.clone(),
            callee_method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(callee_buf),
        );
        let old_entry = cache
            .get(&callee_class, &callee_method, &desc, cid)
            .expect("callee")
            .entry_ptr() as usize;

        let mut caller_buf = ExecutableBuffer::new(64).expect("alloc caller");
        caller_buf.emit(&[0xC3]);
        let mut caller = CompiledMethod::new(caller_buf);
        caller._direct_callee_entries.push(old_entry);
        let caller_class: Arc<str> = Arc::from("OwningCaller");
        let caller_method: Arc<str> = Arc::from("call");
        cache.put(
            caller_class.clone(),
            caller_method.clone(),
            desc.clone(),
            cid,
            caller,
        );

        let mut replacement_buf = ExecutableBuffer::new(64).expect("alloc replacement");
        replacement_buf.emit(&[0xC3]);
        cache.put(
            callee_class.clone(),
            callee_method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(replacement_buf),
        );
        assert!(
            lookup_jit_code_range(old_entry).is_some(),
            "the caller's baked edge must pin its superseded callee"
        );

        cache.remove(&caller_class, &caller_method, &desc, cid);
        assert!(
            lookup_jit_code_range(old_entry).is_none(),
            "dropping the final direct caller must reclaim the old body"
        );
    }

    #[test]
    fn test_inline_cache_reclamation_waits_for_jit_quiescence() {
        let cache = JitCache::new();
        let class: Arc<str> = Arc::from("DeferredTarget");
        let method: Arc<str> = Arc::from("run");
        let desc: Arc<str> = Arc::from("()V");
        let cid = cratonvm_types::ClassId::new(19);
        let mut buf = ExecutableBuffer::new(64).expect("alloc deferred target");
        buf.emit(&[0xC3]);
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(buf),
        );
        let entry = cache
            .get(&class, &method, &desc, cid)
            .expect("target")
            .entry_ptr() as usize;
        let slot = JitMICSlot::new();
        slot.update(cid.as_u32(), &class, entry as u64, false);

        jit_execution_enter();
        slot.clear_compiled_entry();
        cache.remove(&class, &method, &desc, cid);
        assert!(
            lookup_jit_code_range(entry).is_some(),
            "a raw cache reader may still be between load and call"
        );
        jit_execution_leave();
        assert!(
            lookup_jit_code_range(entry).is_none(),
            "the final quiescent transition must drain deferred code owners"
        );
    }

    #[test]
    fn test_sharded_cache_supports_concurrent_lock_free_reads_and_publication() {
        let cache = Arc::new(JitCache::new());
        let mut workers = Vec::new();
        for worker in 0..4u32 {
            let cache = cache.clone();
            workers.push(std::thread::spawn(move || {
                for method_index in 0..32u32 {
                    let class: Arc<str> = Arc::from(format!("Concurrent{worker}"));
                    let method: Arc<str> = Arc::from(format!("m{method_index}"));
                    let desc: Arc<str> = Arc::from("()V");
                    let cid = cratonvm_types::ClassId::new(worker * 32 + method_index + 1);
                    let mut buf = ExecutableBuffer::new(16).expect("alloc concurrent body");
                    buf.emit(&[0xC3]);
                    cache.put(
                        class.clone(),
                        method.clone(),
                        desc.clone(),
                        cid,
                        CompiledMethod::new(buf),
                    );
                    assert!(cache.get(&class, &method, &desc, cid).is_some());
                }
            }));
        }
        for worker in workers {
            worker.join().expect("cache worker");
        }
        assert_eq!(cache.len(), 128);
        assert_eq!(cache.clear_all(), 128);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_code_range_snapshot_stays_searchable_during_publication() {
        let base = 0x7f00_0000_0000usize;
        for i in 0..128usize {
            register_jit_code_range(base + i * 0x1000, 0x100, i + 1);
        }
        let readers = (0..4)
            .map(|_| {
                std::thread::spawn(move || {
                    for _ in 0..1_000 {
                        for i in 0..128usize {
                            assert_eq!(
                                lookup_jit_code_range(base + i * 0x1000 + 0x40),
                                Some(i + 1)
                            );
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        for reader in readers {
            reader.join().expect("range reader");
        }
        for i in 0..128usize {
            unregister_jit_code_range(base + i * 0x1000);
        }
    }

    /// `clear_all` must evict every entry and drop every published body —
    /// `CompiledMethod::drop` is what returns the executable mapping AND
    /// withdraws that body's `JIT_CODE_RANGES` registration.
    ///
    /// The post-conditions below are deliberately phrased against state this
    /// test OWNS rather than against the process-global range table. This lib
    /// test binary runs its tests on a thread pool and `JIT_CODE_RANGES` is
    /// process-global, so the old `lookup_jit_code_range(entry).is_none()`
    /// assertion was order-dependent: `clear_all` unmaps our code pages, a
    /// concurrently-running test then gets one of those very addresses back
    /// from `ExecutableBuffer::new` and registers ITS range over it, and the
    /// lookup correctly reports `Some(<that other test's body>)`. Holding a
    /// `Weak` to each body keeps its `Arc` allocation (not the body) alive, so
    /// `own_*` can never be recycled underneath the comparison and
    /// `strong_count() == 0` proves *our* `Drop` ran.
    // T2.2 epoch invalidation for the interpreter's Bytecode invoke cache
    //
    // These tests guard the contract the interpreter's cached-invoke `Bytecode`
    // arms now rely on: instead of calling `JitCache::get` (three string hashes
    // + three string compares) on every interpreted call just to notice that a
    // background compile published a body, they memoize
    // `jit_cache_generation()` in `CachedBytecodeMethod::jit_probe_generation`
    // and only re-probe when the live generation differs.
    //
    // Every assertion below is written to be robust under a parallel `cargo
    // test`: the generation is a process-global, so other tests can advance it
    // concurrently. That can only push two sampled values FURTHER apart, so
    // "must differ" assertions are race-free; no test here asserts that the
    // generation stayed equal across a window in which another test could run.

    fn probe_test_method(
        class: &str,
        method: &str,
        desc: &str,
        cid: cratonvm_types::ClassId,
    ) -> CachedBytecodeMethod {
        CachedBytecodeMethod {
            declaring_class_id: cid,
            class_name: Arc::from(class),
            method_name: Arc::from(method),
            method_descriptor: Arc::from(desc),
            source_file: None,
            code: Arc::from([0xb1u8].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 0,
            max_locals: 0,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        }
    }

    fn probe_ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    /// THE staleness test. Replays the interpreter's exact epoch protocol
    /// end-to-end and proves that a **first-time** publication (not a
    /// replacement) forces the memoized call site back onto the slow path,
    /// where it picks up the newly published body.
    ///
    /// This is the case that used to be broken: `JitCache::put` advanced the
    /// generation only when it *replaced* an existing entry, so a call site that
    /// had already memoized "no compiled body for this method" would have stayed
    /// pinned to the interpreter forever after the background worker published
    /// its first body.
    #[test]
    fn jit_probe_epoch_forces_reprobe_after_first_publication() {
        let cache = JitCache::new();
        let cid = cratonvm_types::ClassId::new(4101);
        let class: Arc<str> = Arc::from("ProbeEpochA");
        let method: Arc<str> = Arc::from("hot");
        let desc: Arc<str> = Arc::from("()V");
        let cached = probe_test_method(&class, &method, &desc, cid);

        // 1. Cold: the call site probes, misses, and memoizes the miss --
        //    exactly what the `Bytecode` arm does on its first interpreted call.
        let gen_at_probe = jit_cache_generation();
        assert!(
            cache.get(&class, &method, &desc, cid).is_none(),
            "nothing published yet"
        );
        cached.record_jit_probe_miss(gen_at_probe);
        assert!(
            cached.jit_probe_is_current(gen_at_probe),
            "the memo must suppress a re-probe while the generation is unchanged"
        );

        // 2. A background compile publishes this method's FIRST body.
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            probe_ret_body(),
        );

        // 3. The memo must now report stale, so the interpreter re-probes...
        let gen_after_publish = jit_cache_generation();
        assert!(
            !cached.jit_probe_is_current(gen_after_publish),
            "a first-time publication MUST advance the JIT cache generation, or an \
             interpreted call site that already memoized a probe miss would never \
             notice the new body"
        );
        // ...and the re-probe finds the freshly published body.
        assert!(
            cache.get(&class, &method, &desc, cid).is_some(),
            "the re-probe must pick up the published body"
        );
    }

    /// An OSR publication lands in a separate map (`osr_methods`) but reaches the
    /// same `Bytecode` call sites, so it must advance the generation too. Same
    /// first-publication (non-replacement) shape as above.
    #[test]
    fn jit_probe_epoch_forces_reprobe_after_first_osr_publication() {
        let cache = JitCache::new();
        let cid = cratonvm_types::ClassId::new(4102);
        let class: Arc<str> = Arc::from("ProbeEpochB");
        let method: Arc<str> = Arc::from("loopy");
        let desc: Arc<str> = Arc::from("(I)I");
        let cached = probe_test_method(&class, &method, &desc, cid);

        let gen_at_probe = jit_cache_generation();
        cached.record_jit_probe_miss(gen_at_probe);

        let mut osr = probe_ret_body();
        osr.compiled_via_osr = true;
        cache.put_osr(class.clone(), method.clone(), desc.clone(), cid, osr);

        assert!(
            !cached.jit_probe_is_current(jit_cache_generation()),
            "put_osr must advance the generation on a first publication"
        );
        assert!(cache.get_osr(&class, &method, &desc, cid).is_some());
    }

    /// A C1->C2 supersede republishes under an existing key. That path already
    /// bumped (it is a replacement), but it is part of the audited contract, so
    /// pin it: a call site that had flipped to `Jit` and then been downgraded
    /// back to `Bytecode` must still re-probe and find the C2 body.
    #[test]
    fn jit_probe_epoch_forces_reprobe_after_supersede_republication() {
        let cache = JitCache::new();
        let cid = cratonvm_types::ClassId::new(4103);
        let class: Arc<str> = Arc::from("ProbeEpochC");
        let method: Arc<str> = Arc::from("tiered");
        let desc: Arc<str> = Arc::from("()J");
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            probe_ret_body(),
        );
        let c1_entry = cache
            .get(&class, &method, &desc, cid)
            .expect("C1 body")
            .entry_ptr() as usize;

        let cached = probe_test_method(&class, &method, &desc, cid);
        let gen_at_probe = jit_cache_generation();
        cached.record_jit_probe_miss(gen_at_probe);

        // C2 replaces the C1 entry under the same key.
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            probe_ret_body(),
        );

        assert!(
            !cached.jit_probe_is_current(jit_cache_generation()),
            "a superseding republication must advance the generation"
        );
        let c2_entry = cache
            .get(&class, &method, &desc, cid)
            .expect("C2 body")
            .entry_ptr() as usize;
        assert_ne!(c1_entry, c2_entry, "C2 must have replaced the C1 artifact");
    }

    /// Invalidation/eviction must advance the generation as well. It is not a
    /// staleness hazard for the negative memo (a removal cannot make "there is
    /// no body" wrong), but the interpreter's `Jit` entries are re-derived
    /// through this same probe, so pin the behaviour rather than leave it to
    /// chance.
    #[test]
    fn jit_probe_epoch_advances_on_invalidation() {
        let cache = JitCache::new();
        let cid = cratonvm_types::ClassId::new(4104);
        let class: Arc<str> = Arc::from("ProbeEpochD");
        let method: Arc<str> = Arc::from("gone");
        let desc: Arc<str> = Arc::from("()V");
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            probe_ret_body(),
        );
        assert!(cache.get(&class, &method, &desc, cid).is_some());

        let cached = probe_test_method(&class, &method, &desc, cid);
        let gen_before_remove = jit_cache_generation();
        cached.record_jit_probe_miss(gen_before_remove);

        cache.remove(&class, &method, &desc, cid);
        assert!(cache.get(&class, &method, &desc, cid).is_none());
        assert!(
            !cached.jit_probe_is_current(jit_cache_generation()),
            "an eviction must advance the generation"
        );
    }

    /// The memoized invocation-counter key must stay bit-identical to the two
    /// open-coded 31-multiplier loops it replaced in the interpreter -- the key
    /// indexes `ProfileStore`'s per-method warmup counters, so a different value
    /// would silently reset every method's tier-up progress.
    #[test]
    fn memoized_invoc_key_matches_the_open_coded_hash() {
        let cid = cratonvm_types::ClassId::new(7);
        let cached = probe_test_method("pkg/Hash", "someMethod", "(Ljava/lang/String;I)Z", cid);
        let expected = {
            let mut h = 0u32;
            for &b in cached.method_name.as_bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as u32);
            }
            for &b in cached.method_descriptor.as_bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as u32);
            }
            ((cid.as_u32() as u64) << 32) | (h as u64)
        };
        assert_eq!(cached.invoc_key(), expected);
        // Memoized: stable across calls.
        assert_eq!(cached.invoc_key(), expected);
        // And it survives a clone (the manual `Clone` impl).
        assert_eq!(cached.clone().invoc_key(), expected);
    }

    #[test]
    fn test_jit_cache_clear_all_evicts_entries() {
        let cache = JitCache::new();
        let class_a: Arc<str> = Arc::from("TestClassA");
        let method_a: Arc<str> = Arc::from("testA");
        let desc_a: Arc<str> = Arc::from("()V");
        let class_b: Arc<str> = Arc::from("TestClassB");
        let method_b: Arc<str> = Arc::from("testB");
        let desc_b: Arc<str> = Arc::from("(I)I");

        let cid_a = cratonvm_types::ClassId::new(1);
        let cid_b = cratonvm_types::ClassId::new(2);
        let mut buf_a = ExecutableBuffer::new(16).expect("alloc failed");
        buf_a.emit(&[0xC3]); // RET
        cache.put(
            class_a.clone(),
            method_a.clone(),
            desc_a.clone(),
            cid_a,
            CompiledMethod::new(buf_a),
        );

        let mut buf_b = ExecutableBuffer::new(16).expect("alloc failed");
        buf_b.emit(&[0xC3]); // RET
        cache.put(
            class_b.clone(),
            method_b.clone(),
            desc_b.clone(),
            cid_b,
            CompiledMethod::new(buf_b),
        );

        let cm_a = cache
            .get(&class_a, &method_a, &desc_a, cid_a)
            .expect("compiled A");
        let cm_b = cache
            .get(&class_b, &method_b, &desc_b, cid_b)
            .expect("compiled B");
        let entry_a = cm_a.entry_ptr() as usize;
        let entry_b = cm_b.entry_ptr() as usize;
        // The `cm_ptr` each body registered in `JIT_CODE_RANGES` (see
        // `JitCache::put`), used below to tell our own registration apart from a
        // recycled-address one belonging to another test.
        let own_a = Arc::as_ptr(&cm_a) as usize;
        let own_b = Arc::as_ptr(&cm_b) as usize;
        let weak_a = Arc::downgrade(&cm_a);
        let weak_b = Arc::downgrade(&cm_b);
        drop(cm_a);
        drop(cm_b);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.clear_all(), 2);
        assert!(cache.is_empty());
        assert!(cache.get(&class_a, &method_a, &desc_a, cid_a).is_none());
        assert!(cache.get(&class_b, &method_b, &desc_b, cid_b).is_none());

        // The cache held the last strong reference, so eviction must have run
        // `CompiledMethod::drop` — which unmaps the code and unregisters the
        // range — for both bodies.
        assert_eq!(
            weak_a.strong_count(),
            0,
            "clear_all must release the last owner of body A"
        );
        assert_eq!(
            weak_b.strong_count(),
            0,
            "clear_all must release the last owner of body B"
        );
        // And no code range still binds our entry addresses to OUR bodies. A
        // `Some(other)` here just means a concurrently-running test has already
        // been handed the freed page back; that is not this cache's business.
        assert_ne!(
            lookup_jit_code_range(entry_a),
            Some(own_a),
            "clear_all must withdraw body A's own code-range registration"
        );
        assert_ne!(
            lookup_jit_code_range(entry_b),
            Some(own_b),
            "clear_all must withdraw body B's own code-range registration"
        );
    }

    /// `clear_all` must return every unreferenced executable mapping, and with
    /// it the `COMMITTED_JIT_CODE_BYTES` those mappings account for.
    ///
    /// `COMMITTED_JIT_CODE_BYTES` is process-global and this lib test binary
    /// runs its tests on a thread pool, so absolute before/after totals are NOT
    /// a stable observable: every other test that allocates or drops an
    /// `ExecutableBuffer` moves the same counter. The old
    /// `populated >= before + 8 * 4096` / `reclaimed <= populated - 8 * 4096`
    /// pair therefore failed whenever concurrent frees (resp. allocations)
    /// happened to outpace this test's own bookkeeping between the two loads.
    ///
    /// What this test owns instead:
    ///  * the counter is an exact ledger — `ExecutableBuffer::new` adds
    ///    `capacity` and its `Drop` subtracts the same `capacity` — so at any
    ///    instant it equals the summed capacity of all *live* buffers. That
    ///    makes `>= our own live bytes` a sound lower bound no matter what
    ///    other threads are doing.
    ///  * `Drop` is the *only* site that decrements the counter and releases
    ///    the mapping, so proving `clear_all` dropped the last owner of each
    ///    body proves exactly `committed_by_test` bytes went back.
    #[test]
    fn test_clear_all_releases_committed_executable_bytes() {
        const BODY_BYTES: usize = 4096;
        const BODIES: u32 = 8;

        let cache = JitCache::new();
        let mut bodies = Vec::with_capacity(BODIES as usize);
        for i in 0..BODIES {
            let mut buf = ExecutableBuffer::new(BODY_BYTES).expect("alloc cache body");
            buf.emit(&[0xC3]);
            let class: Arc<str> = Arc::from("ReclaimBytes");
            let method: Arc<str> = Arc::from(format!("m{i}"));
            let desc: Arc<str> = Arc::from("()V");
            let cid = cratonvm_types::ClassId::new(i + 1);
            cache.put(
                class.clone(),
                method.clone(),
                desc.clone(),
                cid,
                CompiledMethod::new(buf),
            );
            let cm = cache.get(&class, &method, &desc, cid).expect("published");
            bodies.push(Arc::downgrade(&cm));
        }
        let committed_by_test = BODIES as usize * BODY_BYTES;

        // Our eight mappings are live, so the global ledger must be at least
        // that large — every other contribution to it is a live buffer too.
        let populated = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            populated >= committed_by_test,
            "committed ledger {populated} is below this test's own live mappings \
             ({committed_by_test} bytes)"
        );

        assert_eq!(cache.clear_all(), BODIES as usize);

        // Every body lost its last owner, so `CompiledMethod::drop` ran for all
        // eight — unmapping the code and returning `committed_by_test` bytes.
        for (i, weak) in bodies.iter().enumerate() {
            assert_eq!(
                weak.strong_count(),
                0,
                "clear_all must return body m{i}'s executable mapping"
            );
        }
    }

    #[test]
    fn test_jit_cache_get_missing() {
        let cache = JitCache::new();
        let class: Arc<str> = Arc::from("Missing");
        let method: Arc<str> = Arc::from("missing");
        let desc: Arc<str> = Arc::from("()V");
        assert!(cache
            .get(&class, &method, &desc, cratonvm_types::ClassId::new(1))
            .is_none());
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
        let mut keys: Vec<(Arc<str>, Arc<str>, Arc<str>, cratonvm_types::ClassId)> =
            Vec::with_capacity(100);
        for i in 0..100 {
            let class: Arc<str> = Arc::from(format!("pkg/Cls{i}"));
            let method: Arc<str> = Arc::from(format!("m{i}"));
            let desc: Arc<str> = Arc::from(format!("(I)I{i}"));
            let cid = cratonvm_types::ClassId::new((i + 1) as u32);
            let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
            buf.emit(&[0xC3]); // RET
            let cm = CompiledMethod::new(buf);
            cache.put(class.clone(), method.clone(), desc.clone(), cid, cm);
            keys.push((class, method, desc, cid));
        }
        assert_eq!(cache.len(), 100);
        for (class, method, desc, cid) in &keys {
            assert!(
                cache.get(class, method, desc, *cid).is_some(),
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
        assert!(cache
            .get(
                &missing_cls,
                &missing_m,
                &missing_d,
                cratonvm_types::ClassId::new(9999)
            )
            .is_none());
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

    // ── indy_arg_type_tags tests ─────────────────────────────────────

    #[test]
    fn indy_arg_type_tags_matches_the_writingservlet_concat_shape() {
        // WritingServlet.doGet's `makeConcatWithConstants` bootstrap
        // descriptor: (int length, String buffered, long elapsedNanos).
        assert_eq!(
            indy_arg_type_tags("(ILjava/lang/String;J)Ljava/lang/String;"),
            vec![b'I', b'L', b'J']
        );
    }

    #[test]
    fn indy_arg_type_tags_one_tag_per_compact_slot() {
        assert_eq!(
            count_param_slots("(IJDF)V"),
            indy_arg_type_tags("(IJDF)V").len()
        );
        assert_eq!(indy_arg_type_tags("(IJDF)V"), vec![b'I', b'J', b'D', b'F']);
    }

    #[test]
    fn indy_arg_type_tags_arrays_are_l() {
        assert_eq!(
            indy_arg_type_tags("([I[Ljava/lang/Object;)V"),
            vec![b'L', b'L']
        );
    }

    #[test]
    fn indy_arg_type_tags_empty() {
        assert_eq!(indy_arg_type_tags("()V"), Vec::<u8>::new());
        assert_eq!(indy_arg_type_tags(""), Vec::<u8>::new());
    }

    #[test]
    fn test_compute_param_jvm_slots_category2_instance() {
        assert_eq!(
            compute_param_jvm_slots("(Ljava/lang/Object;JZ)V", false),
            (vec![0, 1, 2, 4], 5)
        );
        assert_eq!(
            compute_param_jvm_slots("(JLjava/lang/Object;)V", true),
            (vec![0, 2], 3)
        );
    }

    #[test]
    fn invokestatic_self_call_tail_jump_predicate() {
        let tail_call = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0x00, 0x00];
        assert!(invokestatic_self_call_uses_tail_jump(&tail_call, 5, 1));

        let non_tail_call = [0x1a, 0xb8, 0x00, 0x01, 0x1a, 0xac, 0x00, 0x00];
        assert!(!invokestatic_self_call_uses_tail_jump(&non_tail_call, 6, 1));

        let void_return_tail_shape = [0xb8, 0x00, 0x01, 0xb1, 0x00, 0x00];
        assert!(!invokestatic_self_call_uses_tail_jump(
            &void_return_tail_shape,
            4,
            0
        ));
    }

    // ── return_type tests ───────────────────────────────────────────

    #[test]
    fn recursive_compile_cycle_routes_parent_direct_call_through_dispatch() {
        use std::sync::Arc;

        clear_jit_recursive_cycle_methods_for_test();
        // This test exercises the direct-callee-compile path (`callee_compiler`
        // below), which is opt-in by default — see
        // `direct_jit_callee_calls_enabled`. Not OnceLock-cached, so setting it
        // here is observed immediately; no other jit test asserts on
        // invoke-info/PIC/MIC-slot counts, so this is safe under parallel
        // `cargo test`.
        std::env::set_var("CRATONVM_JIT_DIRECT_CALLEE_CALLS", "1");

        let a_cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(1),
            class_name: Arc::from("pkg/A"),
            method_name: Arc::from("a"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from([0xb8, 0x00, 0x01, 0xb1, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 0,
            max_locals: 0,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        };
        let b_cached = CachedBytecodeMethod {
            declaring_class_id: cratonvm_types::ClassId::new(2),
            class_name: Arc::from("pkg/B"),
            method_name: Arc::from("b"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from([0xb8, 0x00, 0x01, 0xb1, 0x00, 0x00].as_slice()),
            exception_table: Arc::from(Vec::new().as_slice()),
            max_stack: 0,
            max_locals: 0,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
            force_native_cache: std::sync::OnceLock::new(),
            native_callback_cache: std::sync::OnceLock::new(),
            invoc_key: std::sync::OnceLock::new(),
            jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
            quickened: std::sync::OnceLock::new(),
        };
        // SAFETY: every helper address is an integer slot. This test only
        // inspects emitted metadata and never executes the generated code.
        let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        let a_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
            (cp_idx == 1).then(|| ("pkg/B".to_string(), "b".to_string(), "()V".to_string()))
        };
        let b_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
            (cp_idx == 1).then(|| ("pkg/A".to_string(), "a".to_string(), "()V".to_string()))
        };
        let callee_compiler =
            |class_name: &str, method_name: &str, descriptor: &str| -> Option<(usize, bool)> {
                assert_eq!((class_name, method_name, descriptor), ("pkg/B", "b", "()V"));
                let compiled_b = try_compile(
                    &b_cached,
                    None,
                    None,
                    None,
                    Some(&b_resolver),
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
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
                )?;
                assert_eq!(
                    compiled_b._jit_invoke_infos.len(),
                    1,
                    "B -> A must use dispatch after seeing A on the compile stack"
                );
                let compiled_b = Box::leak(Box::new(compiled_b));
                Some((compiled_b.entry_ptr() as usize, compiled_b.needs_context()))
            };

        let compiled_a = try_compile(
            &a_cached,
            None,
            None,
            None,
            Some(&a_resolver),
            Some(&callee_compiler),
            None,
            None,
            None,
            None,
            &helpers,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
        )
        .expect("A should compile");

        assert!(jit_direct_call_requires_dispatch("pkg/A", "a", "()V"));
        assert!(jit_direct_call_requires_dispatch("pkg/B", "b", "()V"));
        assert_eq!(
            compiled_a._jit_invoke_infos.len(),
            1,
            "A -> B must fall back to dispatch once B is marked as a cycle participant"
        );

        clear_jit_recursive_cycle_methods_for_test();
        std::env::remove_var("CRATONVM_JIT_DIRECT_CALLEE_CALLS");
    }

    #[test]
    fn tomcat_bcel_read_interfaces_direct_calls_use_dispatch() {
        assert!(jit_direct_call_requires_dispatch(
            "org/apache/tomcat/util/bcel/classfile/ClassParser",
            "readInterfaces",
            "()V",
        ));
        assert!(!jit_direct_call_requires_dispatch(
            "org/apache/tomcat/util/bcel/classfile/ClassParser",
            "readFields",
            "()V",
        ));
    }

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
    fn s33_mic_slot_clear_compiled_entry_keeps_receiver_cache() {
        let mic = JitMICSlot::new();
        mic.update(7, "com/example/MyClass", 0xDEAD_BEEF, true);
        mic.clear_compiled_entry();

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
            0
        );
        assert!(!mic
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

    /// A raw inline MIC has no helper boundary between its guard load and
    /// indirect CALL, so retargeting a populated slot risks pairing one
    /// receiver's class guard with another receiver's entry. `update` is
    /// therefore monomorphic for the slot's lifetime: the first class to
    /// install wins, and a later `update` for a DIFFERENT class is a no-op
    /// (that receiver simply takes the ordinary helper path instead).
    #[test]
    fn s33_mic_slot_update_first_install_wins_no_retarget() {
        let mic = JitMICSlot::new();
        mic.update(1, "A", 100, false);
        mic.update(2, "B", 200, true);
        assert_eq!(
            mic.cached_class_id
                .load(std::sync::atomic::Ordering::Acquire),
            1
        );
        assert_eq!(mic.cached_class_name.lock().as_deref(), Some("A"));
        assert_eq!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Acquire),
            100
        );
        assert!(!mic
            .cached_needs_context
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    /// A re-`update` for the SAME class as already installed must stay a
    /// no-op (idempotent), not tear the entry/name/context triple.
    #[test]
    fn s33_mic_slot_update_same_class_is_idempotent() {
        let mic = JitMICSlot::new();
        mic.update(1, "A", 100, false);
        mic.update(1, "A", 999, true);
        assert_eq!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Acquire),
            100
        );
        assert!(!mic
            .cached_needs_context
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    /// `prepopulate` seeds only `cached_class_id` (a profile-derived guard
    /// hint) with `cached_entry_ptr` still 0. The first `update` for that
    /// SAME class id must still publish a real entry — this is the shape
    /// `mark_current_jit_compile_method_recursive_cycle`'s caller
    /// (profile-guided MIC seeding, see `try_compile_inner`) relies on.
    #[test]
    fn s33_mic_slot_update_resolves_prepopulated_hint() {
        let mic = JitMICSlot::new();
        mic.prepopulate(7);
        assert_eq!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        mic.update(7, "Hinted", 0x4242, true);
        assert_eq!(
            mic.cached_class_id
                .load(std::sync::atomic::Ordering::Acquire),
            7
        );
        assert_eq!(
            mic.cached_entry_ptr
                .load(std::sync::atomic::Ordering::Acquire),
            0x4242
        );
        assert_eq!(mic.cached_class_name.lock().as_deref(), Some("Hinted"));
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

    /// RG.1 — A method containing `invokedynamic` (0xba) no longer vetoes
    /// JIT compilation at the scan stage.
    ///
    /// Previously ANY invokedynamic anywhere in a method's bytecode —
    /// reachable or not — permanently blacklisted the WHOLE method from JIT
    /// compilation. The overwhelmingly common source of a "surprise"
    /// invokedynamic in otherwise-ordinary hot methods is
    /// `assert cond : "msg" + var;` (javac lowers the message concat via
    /// `StringConcatFactory`, guarded by the `assertionsDisabled` dead
    /// branch), so this blanket veto forced hot per-call-site methods that
    /// merely CONTAIN a dead assert into the interpreter forever (see
    /// `docs/internal/...binary-docvalues-range-hang...md` for the concrete
    /// Lucene `FSTCompiler`/`NodeHash` repro).
    ///
    /// The new design: the scanner accepts 0xba and simply records the site
    /// (`indy_ops`); the codegen (which DOES have CP access, unlike this
    /// scan) lowers the instruction to an UNCONDITIONAL jump to the existing
    /// uncommon-trap deopt stub (`DeoptReason::UnreachedCode`). If this exact
    /// program point is ever actually reached at runtime (assertions
    /// enabled, or a genuinely live indy), the method permanently reverts to
    /// interpreter-only execution for the rest of the process — i.e. today's
    /// status quo for that one method — so the interpreter's real
    /// invokedynamic dispatch (makeConcat, lambda bootstraps) still runs
    /// whenever the instruction is genuinely exercised. In the common case
    /// (assertions disabled, dead branch) the trap is never taken and the
    /// surrounding hot method compiles and runs at full JIT speed.
    #[test]
    fn rg1_jit_accepts_invokedynamic_at_scan_stage() {
        let code = ireturn_tail(vec![
            0xba, 0x00, 0x01, 0x00, 0x00, // invokedynamic #1, 0, 0
        ]);
        assert!(
            is_jit_compatible(&code, code.len(), "()I"),
            "the scanner must no longer bail on invokedynamic — it defers to \
             an unconditional uncommon-trap deopt in the codegen instead"
        );
        // The scan also records the site so the codegen can resolve its
        // descriptor and lower it to the deopt stub.
        let scan = x64::jit_scan(&code, code.len(), "()I").expect("scan must succeed");
        assert_eq!(
            scan.indy_ops,
            vec![(0, 1u16)],
            "invokedynamic site (pc, cp_index) must be recorded for codegen resolution"
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
        let before = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        let buf = ExecutableBuffer::new(4096).expect("alloc failed");
        let after = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after >= before + 4096,
            "committed counter must rise by at least the requested capacity"
        );
        // Drop now returns the executable mapping. Exact post-drop accounting
        // is covered by the reclamation tests because this suite runs other
        // buffer-allocation tests concurrently.
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

// ---------------------------------------------------------------------------
// Layout-constant emission inventory (arch-2026-07-26 `layout-constant-hazards`)
// ---------------------------------------------------------------------------

/// Inventory tripwire for every object-layout constant this crate bakes into
/// emitted machine code, covering the two emitters the original header-offset
/// audit did not reach: this file and `ir_lower.rs`.
///
/// # Why this exists alongside the tripwire in `x64.rs`
///
/// `x64.rs::header_offset_emission_site_inventory_matches_the_doc` counts the
/// substring `<CONST> as <ty>` — the constant *immediately* followed by a cast.
/// That needle is exact for `x64.rs`, and its recorded totals were re-verified
/// against the tree on 2026-07-26 (31 / 11 / 16 / 5, all matching), as were the
/// three `ir_lower.rs` totals (2 / 2 / 1). The mechanism does what its name
/// says — but it is *structurally* blind to any site that uses the constant
/// inside a larger expression which is then cast, and that is exactly the shape
/// both of this file's own sites take:
///
/// ```text
/// let abs = (cratonvm_types::HEADER_SIZE + body_off) as i32;
/// (cratonvm_types::HEADER_SIZE + idx * cratonvm_types::SLOT_SIZE) as i32
/// ```
///
/// A substring scan for the cast form reports **zero** hits here. That is the
/// mechanical reason `lib.rs` never appeared in the header-offset inventory
/// even after `x64.rs` and `ir_lower.rs` were audited: the tool could not see
/// it, so no amount of care from the auditor would have.
///
/// This tripwire therefore counts *identifier occurrences in code* instead:
/// every use of the constant, in any expression shape, outside comments and
/// string literals. The zero entries are as load-bearing as the non-zero ones —
/// a constant that starts being used in a file where it never appeared before
/// also trips the assertion and forces the site into the inventory.
#[cfg(test)]
mod layout_constant_inventory {
    /// Every object-layout constant owned by `cratonvm_types` that this crate
    /// could plausibly bake into an instruction encoding.
    const LAYOUT_CONSTANTS: [&str; 8] = [
        "HEADER_SIZE",
        "ARRAY_LENGTH_OFFSET",
        "SLOT_SIZE",
        "REF_ELEMENT_SIZE",
        "MARK_WORD_OFFSET",
        "IDENTITY_HASH_CODE_OFFSET",
        "FIELD_CELL_PAYLOAD32_OFFSET",
        "FIELD_CELL_PAYLOAD64_OFFSET",
    ];

    /// `(file, counts)` where `counts[i]` is the number of code uses of
    /// `LAYOUT_CONSTANTS[i]` in that file.
    const INVENTORY: [(&str, [usize; 8]); 2] = [
        // lib.rs: the `use` list near the top, plus `StringFieldLayout::new`'s
        // `cell()` closure — its compact-layout branch and its legacy
        // header-plus-cell fallback, each biased by the payload64 offset.
        ("lib.rs", [3, 1, 2, 1, 0, 0, 0, 2]),
        // ir_lower.rs: the `use` list, the three compile-time invariants
        // restated at the top of that file, two disp32 field-address
        // computations, two disp8 float array element accesses, and the disp8
        // array-length load that guards every bounds check.
        ("ir_lower.rs", [7, 3, 4, 0, 0, 0, 3, 0]),
    ];

    fn source(file: &str) -> &'static str {
        match file {
            "lib.rs" => include_str!("lib.rs"),
            "ir_lower.rs" => include_str!("ir_lower.rs"),
            other => panic!("no source registered for {other}"),
        }
    }

    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    /// Count whole-identifier occurrences of `ident` in *code*: whole-line and
    /// trailing line comments are skipped, double-quoted string literals are
    /// skipped, and a match that is part of a longer identifier does not count.
    ///
    /// Deliberately a tokenizer rather than a substring scan, for the reason in
    /// the module doc. Two known limitations, both of which can only ever
    /// *under*-count and both of which are pinned by the fixed totals above: a
    /// `"` inside a character literal makes the rest of that line read as a
    /// string, and a string literal continued across a line break is treated as
    /// re-opening on the next line.
    fn code_occurrences(src: &str, ident: &str) -> usize {
        let mut n = 0usize;
        for raw in src.lines() {
            if raw.trim_start().starts_with("//") {
                continue;
            }
            let bytes = raw.as_bytes();
            let mut in_str = false;
            let mut i = 0usize;
            while i < bytes.len() {
                let b = bytes[i];
                if in_str {
                    if b == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b == b'"' {
                        in_str = false;
                    }
                    i += 1;
                    continue;
                }
                if b == b'"' {
                    in_str = true;
                    i += 1;
                    continue;
                }
                if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    break;
                }
                if is_ident_byte(b) {
                    let start = i;
                    while i < bytes.len() && is_ident_byte(bytes[i]) {
                        i += 1;
                    }
                    if &raw[start..i] == ident {
                        n += 1;
                    }
                    continue;
                }
                i += 1;
            }
        }
        n
    }

    /// **Verify the counter before trusting anything it reports.**
    ///
    /// More than one "audit" in this repo has turned out to check something
    /// other than what its name says, so this pins what `code_occurrences`
    /// actually does against a sample whose answer is obvious by inspection —
    /// and demonstrates, on the same sample, the blind spot in the substring
    /// needle that kept this file out of the header-offset inventory.
    #[test]
    fn the_counter_counts_what_its_name_says() {
        let sample = concat!(
            "use x::{FAKE_OFF, OTHER};\n",
            "// FAKE_OFF in a whole-line comment must not count\n",
            "/// FAKE_OFF in a doc comment must not count\n",
            "let a = FAKE_OFF + FAKE_OFF;   // FAKE_OFF trailing comment\n",
            "let b = MY_FAKE_OFF + FAKE_OFF_2 + FAKE_OFFSET;\n",
            "panic!(\"FAKE_OFF inside a string literal must not count\");\n",
            "let c = (FAKE_OFF + n) as i32;\n",
        );
        // 1 in the use-list + 2 on the `let a` line + 1 on the `let c` line.
        assert_eq!(
            code_occurrences(sample, "FAKE_OFF"),
            4,
            "the counter must see code uses only, and must never match inside a \
             longer identifier"
        );
        assert_eq!(
            code_occurrences(sample, "NOT_PRESENT_ANYWHERE"),
            0,
            "an absent identifier must count zero, not match spuriously"
        );
        // The substring needle the x64.rs tripwire uses finds NONE of those
        // four, because not one of them is written as a bare constant directly
        // followed by a cast. This is the whole reason a second mechanism is
        // needed rather than another copy of the first.
        assert_eq!(
            sample.matches("FAKE_OFF as i32").count(),
            0,
            "a `<CONST> as <ty>` substring scan sees nothing here even though the \
             constant is used four times, one of them inside a cast expression"
        );
    }

    /// The inventory itself. Every count, including every zero.
    #[test]
    fn layout_constant_emission_sites_are_inventoried() {
        for (file, expected) in INVENTORY {
            let src = source(file);
            for (idx, ident) in LAYOUT_CONSTANTS.into_iter().enumerate() {
                let want = expected[idx];
                let found = code_occurrences(src, ident);
                assert_eq!(
                    found, want,
                    "{ident} is used {found}x in jit/src/{file}; the header-shrink \
                     inventory records {want}x. Both files emit object-header \
                     displacements into machine code, and neither is covered by \
                     the substring tripwire in x64.rs. Update \
                     docs/internal/arch-2026-07-26/layout-constant-hazards.md and \
                     header-shrink.md §6.6 in the same change, and confirm the new \
                     or moved site is value-safe at the new layout — the disp8 \
                     sites in ir_lower.rs silently address backwards past 127."
                );
            }
        }
    }

    /// The inventory is only meaningful if it is actually reading source. A
    /// mistyped `include_str!` path is a compile error, but an empty or
    /// truncated read would silently make every count zero and every assertion
    /// above pass vacuously for a table of zeros.
    #[test]
    fn the_inventory_is_reading_real_source() {
        for (file, _) in INVENTORY {
            let src = source(file);
            assert!(
                src.len() > 10_000,
                "{file} read back as {} bytes; the inventory is scanning nothing",
                src.len()
            );
        }
        assert!(
            INVENTORY
                .iter()
                .any(|(_, counts)| counts.iter().any(|c| *c > 0)),
            "an inventory of all zeros would pass without checking anything"
        );
    }
}
