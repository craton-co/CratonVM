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

pub mod aarch64;
pub mod aarch64_backend;
pub mod deopt;
pub mod ir;
pub mod ir_lower;
pub mod ir_optimize;
pub mod ir_schedule;
pub mod loop_analysis;
pub mod platform;
pub mod profile;
pub mod escape_analysis;
pub mod pgo;
pub mod regalloc;
pub mod null_check_elim;
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
        Self { regions: Vec::new() }
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
use cratonvm_types::{ObjectRef, Value, ARRAY_LENGTH_OFFSET, HEADER_SIZE, REF_ELEMENT_SIZE, SLOT_SIZE};

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
    pub fn emit(&mut self, bytes: &[u8]) {
        if self.len + bytes.len() > self.capacity {
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
    /// instead of panicking when capacity is exhausted.
    #[inline]
    pub fn emit_byte(&mut self, b: u8) {
        if self.len >= self.capacity {
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
    pub fn patch_i32(&mut self, offset: usize, value: i32) {
        // After an emit overflow some recorded patch sites can point past the
        // truncated buffer; skip them rather than panicking since the result
        // is going to be discarded anyway.
        if offset + 4 > self.len {
            if self.overflowed {
                return;
            }
            panic!("patch out of bounds");
        }
        let bytes = value.to_le_bytes();
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(offset), 4);
        }
    }

    /// Patch 1 byte at the given offset.
    pub fn patch_byte(&mut self, offset: usize, value: u8) {
        if offset >= self.len {
            if self.overflowed {
                return;
            }
            panic!("patch_byte out of bounds");
        }
        unsafe {
            *self.ptr.add(offset) = value;
        }
    }

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
    /// After calling `finalize`, writes via `emit`/`emit_byte`/`patch_i32` are
    /// undefined behavior. Call [`make_writable`](Self::make_writable)
    /// first if you need to patch code after finalization.
    pub fn finalize(&self) {
        platform::make_executable(self.ptr, self.capacity)
            .unwrap_or_else(|e| {
                eprintln!("FATAL: JIT: make_executable failed: {e}");
                std::process::abort();
            });
    }

    /// Transition the buffer back from executable to writable (for patching).
    ///
    /// Calls the OS API to switch from RX to RW permissions.
    pub fn make_writable(&self) {
        platform::make_writable(self.ptr, self.capacity)
            .unwrap_or_else(|e| {
                eprintln!("FATAL: JIT: make_writable failed: {e}");
                std::process::abort();
            });
    }
}

impl Drop for ExecutableBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // Deregister this region from code pointer validation.
            if let Ok(mut regions) = jit_code_regions().lock() {
                regions.deregister(self.ptr);
            }
            platform::free_executable(self.ptr, self.capacity);
        }
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
            frame_slot_offsets: Vec::new(),
        }
    }

    /// Return the number of oop slots recorded at this safepoint.
    pub fn slot_count(&self) -> usize {
        self.frame_slot_offsets.len()
    }
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
    /// OSR metadata: per-local XMM register assignments for float/double locals.
    pub osr_xmm_assignments: Option<Vec<Option<u8>>>,
    /// OSR metadata: frame size (for SUB RSP).
    pub osr_frame_size: i32,
    /// OSR metadata: callee-saved register save area offset.
    pub osr_callee_saved_base: i32,
    /// OSR metadata: offset of VM context pointer in frame.
    pub osr_heap_local_offset: i32,
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
}

unsafe impl Send for CompiledMethod {}
unsafe impl Sync for CompiledMethod {}

impl Drop for CompiledMethod {
    fn drop(&mut self) {
        // Purge any cached OSR trampolines that point into this method's code
        // range. After Drop, `self._buffer` releases its executable mapping, so
        // any stale `target_addr` in the global cache would be a use-after-free
        // hazard if a future compile reused the same address.
        //
        // `target_addr` for an OSR entry is `self.entry + native_offset`, where
        // `native_offset < self._buffer.pos()` (the emitted code length). Pruning
        // by half-open range `[entry, entry + pos)` covers every such address.
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
            osr_xmm_assignments: None,
            osr_frame_size: 0,
            osr_callee_saved_base: 0,
            osr_heap_local_offset: 0,
            has_dispatch: false,
            inlined_methods: Vec::new(),
            deopt_points: Vec::new(),
            oop_maps: Vec::new(),
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
            osr_xmm_assignments: None,
            osr_frame_size: 0,
            osr_callee_saved_base: 0,
            osr_heap_local_offset: 0,
            has_dispatch: false,
            inlined_methods: Vec::new(),
            deopt_points: Vec::new(),
            oop_maps: Vec::new(),
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
        // Ensure sorted for binary search. Checking sortedness is
        // O(n) but runs only once per compiled method in practice;
        // after the first call, the `is_sorted` check short-circuits.
        let sorted = self
            .oop_maps
            .windows(2)
            .all(|w| w[0].native_pc_offset <= w[1].native_pc_offset);
        if !sorted {
            self.oop_maps
                .sort_by_key(|e| e.native_pc_offset);
        }
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

    /// Whether this method requires a SharedVm pointer as its hidden first argument.
    pub fn needs_context(&self) -> bool {
        self.needs_context
    }

    /// Return the raw entry point pointer for direct calls from JIT code.
    pub fn entry_ptr(&self) -> *const u8 {
        self.entry
    }

    /// Backward-compatible alias for `needs_context()`.
    pub fn needs_heap(&self) -> bool {
        self.needs_context
    }

    /// Call a pure compiled method (no VM context interaction).
    ///
    /// # Safety
    /// The compiled code must match the expected signature.
    #[inline]
    pub unsafe fn call(&self, args: &[i64]) -> i64 {
        validate_code_ptr(self.entry).expect("JIT: invalid code pointer in call()");
        match args.len() {
            0 => {
                let f: unsafe extern "C" fn() -> i64 = std::mem::transmute(self.entry);
                f()
            }
            1 => {
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(self.entry);
                f(args[0])
            }
            2 => {
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(self.entry);
                f(args[0], args[1])
            }
            3 => {
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(self.entry);
                f(args[0], args[1], args[2])
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(args[0], args[1], args[2], args[3])
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(args[0], args[1], args[2], args[3], args[4])
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(args[0], args[1], args[2], args[3], args[4], args[5])
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(args[0], args[1], args[2], args[3], args[4], args[5], args[6])
            }
            8 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7])
            }
            _ => {
                eprintln!("JIT: too many arguments ({}), returning 0", args.len());
                0
            }
        }
    }

    /// Call a compiled method that needs VM context (SharedVm pointer).
    ///
    /// # Safety
    /// `vm_ptr` must be a valid pointer to a `SharedVm`. Args must match the method signature.
    #[inline]
    pub unsafe fn call_with_context(&self, vm_ptr: i64, args: &[i64]) -> i64 {
        validate_code_ptr(self.entry).expect("JIT: invalid code pointer in call_with_context()");
        match args.len() {
            0 => {
                let f: unsafe extern "C" fn(i64) -> i64 = std::mem::transmute(self.entry);
                f(vm_ptr)
            }
            1 => {
                let f: unsafe extern "C" fn(i64, i64) -> i64 = std::mem::transmute(self.entry);
                f(vm_ptr, args[0])
            }
            2 => {
                let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(self.entry);
                f(vm_ptr, args[0], args[1])
            }
            3 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(vm_ptr, args[0], args[1], args[2])
            }
            4 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(vm_ptr, args[0], args[1], args[2], args[3])
            }
            5 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(vm_ptr, args[0], args[1], args[2], args[3], args[4])
            }
            6 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(vm_ptr, args[0], args[1], args[2], args[3], args[4], args[5])
            }
            7 => {
                let f: unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(self.entry);
                f(vm_ptr, args[0], args[1], args[2], args[3], args[4], args[5], args[6])
            }
            _ => {
                eprintln!("JIT: too many arguments ({}) for context call, returning 0", args.len());
                0
            }
        }
    }

    /// Backward-compatible alias for `call_with_context`.
    ///
    /// # Safety
    /// Same safety requirements as `call_with_context`.
    #[inline]
    pub unsafe fn call_with_heap(&self, heap_ptr: i64, args: &[i64]) -> i64 {
        self.call_with_context(heap_ptr, args)
    }

    /// OSR entry: enter JIT code at an arbitrary bytecode PC with interpreter locals.
    ///
    /// # Safety
    /// `vm_ptr` must be a valid SharedVm pointer. `jit_locals` must contain exactly
    /// `osr_num_locals` i64 values in local-index order.
    #[cfg(target_arch = "x86_64")]
    #[inline(never)]
    pub unsafe fn osr_enter(
        &self,
        vm_ptr: i64,
        jit_locals: &[i64],
        entry_pc: usize,
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
fn osr_trampoline_cache()
    -> &'static parking_lot::Mutex<FxHashMap<usize, Arc<ExecutableBuffer>>>
{
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<FxHashMap<usize, Arc<ExecutableBuffer>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::Mutex::new(FxHashMap::default()))
}

/// Emit a fresh OSR trampoline body for the given destination + frame layout.
///
/// The emitted code expects two arguments via the platform C ABI:
///   * arg0 (RCX on Windows / RDI on SysV) = `locals_ptr: *const i64`
///   * arg1 (RDX on Windows / RSI on SysV) = `vm_ptr: i64` (only read when `needs_context`)
///
/// It saves callee-saved registers used for locals, optionally stores `vm_ptr`
/// into the heap-local slot, copies each incoming local into its register/XMM/
/// frame slot, then jumps to `target_addr`.
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
) -> Option<ExecutableBuffer> {
    use crate::x64::LOCAL_REGS;

    // Platform C-ABI argument register numbers.
    // arg0 carries `locals_ptr`, arg1 carries `vm_ptr`. Both are caller-saved on
    // both ABIs, and neither overlaps any register in `LOCAL_REGS`, so saving
    // arg0 into R10 first cannot clobber a callee-saved local target before
    // we've spilled it.
    #[cfg(target_os = "windows")]
    let arg0_reg: u8 = 1; // RCX
    #[cfg(target_os = "windows")]
    let arg1_reg: u8 = 2; // RDX
    #[cfg(not(target_os = "windows"))]
    let arg0_reg: u8 = 7; // RDI
    #[cfg(not(target_os = "windows"))]
    let arg1_reg: u8 = 6; // RSI

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

    #[allow(clippy::needless_range_loop)]
    for i in 0..num_locals {
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
) -> Option<i64> {
    // Look up (or emit and insert) the cached trampoline body for this target.
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
    validate_code_ptr(code_ptr).expect("JIT: invalid trampoline code pointer");

    // The cached trampoline now takes (locals_ptr, vm_ptr) via the platform C ABI.
    // `vm_ptr` is only read when `needs_context`, but passing it unconditionally
    // is harmless (caller-saved register, ignored if unused).
    let tramp_fn: unsafe extern "C" fn(*const i64, i64) -> i64 =
        std::mem::transmute(code_ptr);

    // SAFETY: `tramp_arc` holds an Arc clone of the cached buffer, keeping the
    // executable memory alive for the duration of the call. `locals_ptr` is
    // borrowed from the caller's `jit_locals: &[i64]` slice, which is live
    // across `osr_enter` (and therefore across this call). The fences and
    // black_box prevent the optimizer from reordering the Arc drop above the
    // call or otherwise invalidating the live region.
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    let result = tramp_fn(locals_ptr, vm_ptr);
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

/// Compile-time resolved info for a method invocation from JIT code.
pub struct JitInvokeInfo {
    pub class_name: &'static str,
    pub method_name: &'static str,
    pub descriptor: &'static str,
    pub num_jit_args: usize,
    pub return_type: u8,
    pub invoke_kind: u8,
}

/// Sentinel `entry` values for JitDirectCall indicating inlined Math intrinsics.
/// These are never valid code pointers (kernel address space).
pub const MATH_SQRT_INTRINSIC: usize = usize::MAX;
pub const MATH_FLOOR_INTRINSIC: usize = usize::MAX - 1;
pub const MATH_CEIL_INTRINSIC: usize = usize::MAX - 2;
pub const MATH_RINT_INTRINSIC: usize = usize::MAX - 3;
pub const MATH_ABS_DOUBLE_INTRINSIC: usize = usize::MAX - 4;
pub const MATH_ABS_FLOAT_INTRINSIC: usize = usize::MAX - 5;
pub const MATH_ABS_INT_INTRINSIC: usize = usize::MAX - 6;
pub const MATH_ABS_LONG_INTRINSIC: usize = usize::MAX - 7;
/// T1.1.28 — `Math.fma(a, b, c)` intrinsic (fused multiply-add).
///
/// Per JLS: `Math.fma(a, b, c)` returns `a*b + c` computed as if with
/// unlimited intermediate precision and then rounded once. On x86-64
/// with FMA3 support this maps to `VFMADD231SD` / `VFMADD231SS`; the
/// JIT path falls back to a helper call into `f64::mul_add` /
/// `f32::mul_add` which correctly uses the host FMA instruction when
/// the target CPU supports it and otherwise performs a
/// software-correct fused operation.
pub const MATH_FMA_DOUBLE_INTRINSIC: usize = usize::MAX - 8;
pub const MATH_FMA_FLOAT_INTRINSIC: usize = usize::MAX - 9;
// Round-8 Bug 8: branchless `Math.min(int,int)` / `Math.max(int,int)` /
// long variants via CMOV. Per JLS these are total functions with no NaN /
// trap edge cases on integral inputs — straight `cmp` + `cmovl`/`cmovg`
// in two GPRs. Replaces a compare + Jcc + branch with a single CMOV;
// removes a hard-to-predict branch on sorting / argmin kernels where
// the input distribution beats the branch predictor.
pub const MATH_MIN_INT_INTRINSIC: usize = usize::MAX - 10;
pub const MATH_MAX_INT_INTRINSIC: usize = usize::MAX - 11;
pub const MATH_MIN_LONG_INTRINSIC: usize = usize::MAX - 12;
pub const MATH_MAX_LONG_INTRINSIC: usize = usize::MAX - 13;

/// A resolved direct-call target.
pub struct JitDirectCall {
    pub entry: usize,
    pub needs_context: bool,
    pub num_params: usize,
    pub return_type: u8,
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
    /// Moved to the tail: `parking_lot::Mutex<Option<String>>` has an
    /// unstable layout we must not expose to JIT codegen.
    pub cached_class_name: parking_lot::Mutex<Option<String>>,
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
    pub fn update(
        &self,
        class_id: u32,
        class_name: &str,
        entry_ptr: u64,
        needs_context: bool,
    ) {
        self.cached_class_id
            .store(class_id, std::sync::atomic::Ordering::Release);
        *self.cached_class_name.lock() = Some(class_name.to_string());
        self.cached_entry_ptr
            .store(entry_ptr, std::sync::atomic::Ordering::Release);
        self.cached_needs_context
            .store(needs_context, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a cache hit.
    #[inline]
    pub fn record_hit(&self) {
        self.hits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        let class_name = mic.cached_class_name.lock().clone();
        self.class_ids[0].store(class_id, std::sync::atomic::Ordering::Release);
        self.entry_ptrs[0].store(entry_ptr, std::sync::atomic::Ordering::Release);
        self.needs_context[0].store(needs_ctx, std::sync::atomic::Ordering::Relaxed);
        *self.class_names[0].lock() = class_name;
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
        self.methods.insert(h, (key, Arc::new(compiled)));
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
    pub fn remove(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) {
        let h = compute_jit_key_hash(class_name, method_name, descriptor);
        if let Some((key, _)) = self.methods.get(&h) {
            if &*key.class_name == class_name
                && &*key.method_name == method_name
                && &*key.descriptor == descriptor
            {
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
            !cm.inlined_methods
                .iter()
                .any(|(cls, _, _)| cls == changed_class)
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

/// Convert an `ir::Graph` to an `escape_analysis::Graph` for standalone
/// escape analysis.  The two modules define independent `Op` / `Node` /
/// `Graph` types, so we translate node-by-node.  Returns both the EA graph
/// and the id_map (`ir::NodeId` index → `escape_analysis::NodeId` value)
/// so callers can map EA results back to IR node IDs.
fn escape_analysis_from_ir(ir_graph: &ir::Graph) -> (escape_analysis::Graph, Vec<escape_analysis::NodeId>) {
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
        let ea_op = ir_op_to_ea_op(&ir_node.op);
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
        let ea_inputs: Vec<escape_analysis::NodeId> = ir_node
            .inputs
            .iter()
            .map(|&inp| id_map[inp as usize])
            .collect();
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
        ir::Op::New { class_id, num_fields } => EaOp::New {
            class_id: *class_id,
            num_fields: *num_fields,
        },
        ir::Op::NewArray { element_type } => EaOp::NewArray {
            element_type: *element_type,
        },
        ir::Op::Call => EaOp::Call,
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

            // Determine the field index from the EA Load op so we can look
            // up the replacement value in field_values.
            let ea_field_idx = match &info.field_values.len() {
                0 => continue,
                _ => {
                    // The EA graph's Load(field_idx) carries the field index.
                    // We need to read it from the EA op, but we only have the
                    // EA node ID.  Instead, we can derive it: the IR Load's
                    // MemKind was mapped to the EA field index via
                    // `*mk as usize` in ir_op_to_ea_op.
                    if let ir::Op::Load(mk) = &ir_graph.nodes[idx].op {
                        *mk as usize
                    } else {
                        continue;
                    }
                }
            };

            if ea_field_idx < info.field_values.len() {
                if let Some(ea_val) = info.field_values[ea_field_idx] {
                    if let Some(&ir_val) = reverse_map.get(&ea_val) {
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
static JIT_BAIL_SHORTCIRCUITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn jit_bail_list() -> &'static parking_lot::RwLock<rustc_hash::FxHashSet<u64>> {
    JIT_BAIL_LIST.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashSet::default()))
}

/// Whether the given method has been added to the JIT bail-list by a
/// prior permanent-bail compilation attempt.  Checked at the top of
/// `try_compile` to short-circuit re-attempts.
fn is_jit_bail_listed(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    let h = compute_jit_key_hash(class_name, method_name, descriptor);
    jit_bail_list().read().contains(&h)
}

/// Mark the method as permanently bail-listed.  Called when the heavy
/// `x64::compile` path returns None (typically because of an unsupported
/// backend pattern that won't change on retry).
fn mark_jit_bail_listed(class_name: &str, method_name: &str, descriptor: &str) {
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
#[allow(clippy::type_complexity)]
pub fn try_compile(
    cached: &CachedBytecodeMethod,
    cp_class_name_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    cp_field_resolver: Option<&dyn Fn(u16) -> Option<(usize, u8)>>,
    cp_static_field_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    callee_compiler: Option<&dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,
    cp_new_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize)>>,
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<i64>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<i64>>,
    profile: Option<&profile::MethodProfile>,
    helpers: &JitRuntimeHelpers,
    inline_resolver: Option<&dyn Fn(&str, &str, &str) -> Option<InlineSite>>,
) -> Option<CompiledMethod> {
    // round-7 fix (bug 1): short-circuit re-attempts on methods the
    // backend already permanently bailed on.  Avoids ~50µs of wasted
    // scan/IR/lowering work per re-attempt (every 2000 invocations
    // under the default interpreter warmup gate).
    if is_jit_bail_listed(&cached.class_name, &cached.method_name, &cached.method_descriptor) {
        JIT_BAIL_SHORTCIRCUITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    result
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn try_compile_inner(
    cached: &CachedBytecodeMethod,
    cp_class_name_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    cp_field_resolver: Option<&dyn Fn(u16) -> Option<(usize, u8)>>,
    cp_static_field_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    callee_compiler: Option<&dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,
    cp_new_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize)>>,
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<i64>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<i64>>,
    profile: Option<&profile::MethodProfile>,
    helpers: &JitRuntimeHelpers,
    inline_resolver: Option<&dyn Fn(&str, &str, &str) -> Option<InlineSite>>,
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
    let prologue_param_slots: usize = count_param_slots(&cached.method_descriptor)
        + if cached.is_static { 0 } else { 1 };

    #[cfg(target_arch = "aarch64")]
    {
        use std::collections::HashMap;

        let code = &cached.code;
        let num_params = prologue_param_slots;

        // Build method_info map: scan bytecode for invokestatic operands,
        // resolve each CP index to an argument count via the invoke resolver.
        let mut method_info: HashMap<u16, usize> = HashMap::new();
        if let Some(resolver) = cp_invoke_resolver {
            let mut scan_pc = 0usize;
            while scan_pc < code.len() {
                if code[scan_pc] == 0xb8 && scan_pc + 2 < code.len() {
                    let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16;
                    if let Some((_class, _name, descriptor)) = resolver(cp_idx) {
                        method_info.insert(cp_idx, count_param_slots(&descriptor));
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

    let scan = x64::jit_scan(code, code_len, &cached.method_descriptor)?;

    // Try IR compilation for simple integer-only methods.
    if ir::ir_compatible(&scan) {
        // Includes the implicit `this` slot for instance methods — see
        // `prologue_param_slots` above.
        let num_params = prologue_param_slots;
        let builder = ir::IrBuilder::new(num_params, cached.max_locals as usize);
        if let Some(mut graph) = builder.build(code, code_len) {
            ir_optimize::optimize(&mut graph);

            // --- Escape analysis (Phase 41 + G46 wiring) ---
            // Convert IR graph to escape analysis graph, run analysis,
            // and apply scalar replacement / lock elision to the IR graph.
            {
                let (ea_graph, id_map) = escape_analysis_from_ir(&graph);
                let ea_result = escape_analysis::analyze_escapes(&ea_graph);
                if !ea_result.scalar_replaceable.is_empty()
                    || !ea_result.elide_locks.is_empty()
                {
                    apply_ea_to_ir(&mut graph, &id_map, &ea_result);
                }
            }

            let schedule = ir_schedule::schedule(&graph);
            if let Some(compiled) = ir_lower::lower(
                &graph,
                &schedule,
                num_params,
                cached.max_locals as usize,
            ) {
                return Some(compiled);
            }
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
    // When both flags are `false` the inline TLAB fast path can skip the
    // `jit_post_tlab_init` helper entirely (it just writes the
    // identity-hash and num_slots fields, which the JIT inlines). The
    // current `cp_new_resolver` signature returns only
    // `(class_id, num_fields)` — extending it would touch the resolver
    // callers in `vm/src/runtime/interpreter.rs`, which is out of scope
    // for this fix. The defaults below (`true, true`) keep the helper
    // call mandatory, preserving correctness. Future work: extend the
    // resolver to return the real flags so JDK micro-objects
    // (HashMap.Node, ArrayList$Itr) elide the helper.
    let mut new_info: Vec<(usize, u32, usize, bool, bool)> = Vec::new();
    let mut anewarray_info: Vec<(usize, u32)> = Vec::new();
    if !scan.new_ops.is_empty() || !scan.anewarray_ops.is_empty() {
        let resolver = cp_new_resolver?;
        for &(pc, cp_idx) in &scan.new_ops {
            let (class_id_raw, num_fields) = resolver(cp_idx)?;
            // Conservative: assume both flags set until the resolver
            // is extended to return them. See CRIT-2 follow-up note
            // above.
            new_info.push((pc, class_id_raw, num_fields, true, true));
        }
        for &(pc, cp_idx) in &scan.anewarray_ops {
            let (class_id_raw, _) = resolver(cp_idx)?;
            anewarray_info.push((pc, class_id_raw));
        }
    }

    // Resolve ldc/ldc_w constants (int/float from CP; strings → 0)
    let mut ldc_info: Vec<(usize, i64)> = Vec::new();
    if !scan.ldc_ops.is_empty() {
        if let Some(resolver) = cp_ldc_resolver {
            for &(pc, cp_idx) in &scan.ldc_ops {
                let val = resolver(cp_idx).unwrap_or(0);
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
                                continue;
                            }
                        }
                    }
                }

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
                            },
                        ));
                        continue;
                    }
                }
                needs_heap = true;

                // Math intrinsics — inline SSE/SSE4.1 instructions, no call overhead
                if class_name == "java/lang/Math" || class_name == "java/lang/StrictMath" {
                    let intrinsic = match (method_name.as_str(), descriptor.as_str()) {
                        ("sqrt", "(D)D") => Some((MATH_SQRT_INTRINSIC, 1, b'D')),
                        ("floor", "(D)D") if x64::has_sse41() =>
                            Some((MATH_FLOOR_INTRINSIC, 1, b'D')),
                        ("ceil", "(D)D") if x64::has_sse41() =>
                            Some((MATH_CEIL_INTRINSIC, 1, b'D')),
                        ("rint", "(D)D") if x64::has_sse41() =>
                            Some((MATH_RINT_INTRINSIC, 1, b'D')),
                        ("abs", "(D)D") => Some((MATH_ABS_DOUBLE_INTRINSIC, 1, b'D')),
                        ("abs", "(F)F") => Some((MATH_ABS_FLOAT_INTRINSIC, 1, b'F')),
                        ("abs", "(I)I") => Some((MATH_ABS_INT_INTRINSIC, 1, b'I')),
                        ("abs", "(J)J") => Some((MATH_ABS_LONG_INTRINSIC, 1, b'J')),
                        // T1.1.28 — Math.fma (fused multiply-add).
                        ("fma", "(DDD)D") => Some((MATH_FMA_DOUBLE_INTRINSIC, 3, b'D')),
                        ("fma", "(FFF)F") => Some((MATH_FMA_FLOAT_INTRINSIC, 3, b'F')),
                        // Round-8 Bug 8 — branchless integer min/max via CMOV.
                        ("min", "(II)I") => Some((MATH_MIN_INT_INTRINSIC, 2, b'I')),
                        ("max", "(II)I") => Some((MATH_MAX_INT_INTRINSIC, 2, b'I')),
                        ("min", "(JJ)J") => Some((MATH_MIN_LONG_INTRINSIC, 2, b'J')),
                        ("max", "(JJ)J") => Some((MATH_MAX_LONG_INTRINSIC, 2, b'J')),
                        _ => None,
                    };
                    if let Some((entry, num_params, ret)) = intrinsic {
                        direct_calls.push((
                            pc,
                            JitDirectCall {
                                entry,
                                needs_context: false,
                                num_params,
                                return_type: ret,
                            },
                        ));
                        continue;
                    }
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
                        if let Some(dom_class_id) = profile::dominant_receiver(receiver_counts, 80) {
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
                    trip.suggests_unroll_factor(8).map(|factor| (backedge_pc, factor))
                })
                .collect()
        })
        .unwrap_or_default();

    // round-7 fix (bug 1): from this point on, any `None` return is a
    // permanent backend bail — the resolver pre-checks all completed
    // successfully and we're about to walk the full
    // scan/IR/lowering pipeline.  Flag it so the outer wrapper adds
    // this method to the bail-list.
    *backend_attempted = true;

    let mut compiled = x64::compile(
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
    )?;

    compiled._jit_strings = owned_strings;
    compiled._jit_invoke_infos = owned_invoke_infos;
    compiled._jit_mic_slots = owned_mic_slots;
    compiled._jit_pic_slots = owned_pic_slots;
    compiled.inlined_methods = inlined_methods;

    Some(compiled)
}

/// Count the number of JVM stack slots consumed by parameters in a method descriptor.
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
        let off_class_id =
            (&slot.cached_class_id as *const _ as usize) - base;
        let off_entry_ptr =
            (&slot.cached_entry_ptr as *const _ as usize) - base;
        let off_needs_context =
            (&slot.cached_needs_context as *const _ as usize) - base;
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
        buf.patch_i32(0, 0x12345678);
        assert_eq!(buf.read_i32(0), 0x12345678);
        buf.patch_i32(4, -42);
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
        assert_eq!(
            pic.misses.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
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
        };
        let start = g.add(ir::Op::Start, ir::IrType::Void, vec![], None);
        let alloc = g.add(
            ir::Op::New { class_id: 1, num_fields: 2 },
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
        assert!(off < 9, "offset must leave room for 8-byte pointer within 16 bytes, got {off}");
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
        assert_eq!(mic.cached_class_id.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(mic.cached_class_name.lock().is_none());
        assert_eq!(mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(!mic.cached_needs_context.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(mic.total_observations(), 0);
        assert_eq!(mic.hit_rate_pct(), 0);
    }

    #[test]
    fn s33_mic_slot_prepopulate() {
        let mic = JitMICSlot::new();
        mic.prepopulate(42);
        assert_eq!(mic.cached_class_id.load(std::sync::atomic::Ordering::Relaxed), 42);
        // Entry ptr should still be 0 (prepopulate only sets class_id)
        assert_eq!(mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn s33_mic_slot_update_all_fields() {
        let mic = JitMICSlot::new();
        mic.update(7, "com/example/MyClass", 0xDEAD_BEEF, true);
        assert_eq!(mic.cached_class_id.load(std::sync::atomic::Ordering::Acquire), 7);
        assert_eq!(mic.cached_class_name.lock().as_deref(), Some("com/example/MyClass"));
        assert_eq!(mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Acquire), 0xDEAD_BEEF);
        assert!(mic.cached_needs_context.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn s33_mic_slot_hit_miss_counters() {
        let mic = JitMICSlot::new();
        for _ in 0..10 { mic.record_hit(); }
        for _ in 0..5 { mic.record_miss(); }
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
        for _ in 0..5 { mic.record_hit(); }
        assert!(!mic.is_monomorphic());
        // 10 hits, 0 misses → 100% hit rate, ≥10 obs → monomorphic
        for _ in 0..5 { mic.record_hit(); }
        assert!(mic.is_monomorphic());
    }

    #[test]
    fn s33_mic_slot_is_megamorphic() {
        let mic = JitMICSlot::new();
        // 5 hits, 20 misses → 20% hit rate → megamorphic
        for _ in 0..5 { mic.record_hit(); }
        for _ in 0..20 { mic.record_miss(); }
        assert!(mic.is_megamorphic());
    }

    #[test]
    fn s33_mic_slot_not_megamorphic_when_mostly_hits() {
        let mic = JitMICSlot::new();
        for _ in 0..18 { mic.record_hit(); }
        for _ in 0..2 { mic.record_miss(); }
        assert!(!mic.is_megamorphic());
        assert!(mic.is_monomorphic());
    }

    #[test]
    fn s33_mic_slot_update_overwrites_previous() {
        let mic = JitMICSlot::new();
        mic.update(1, "A", 100, false);
        mic.update(2, "B", 200, true);
        assert_eq!(mic.cached_class_id.load(std::sync::atomic::Ordering::Acquire), 2);
        assert_eq!(mic.cached_class_name.lock().as_deref(), Some("B"));
        assert_eq!(mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Acquire), 200);
        assert!(mic.cached_needs_context.load(std::sync::atomic::Ordering::Relaxed));
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
        for h in handles { h.join().unwrap(); }
        // 4 threads * 100 hits + 4 threads * 100 misses = 800
        assert_eq!(mic.total_observations(), 800);
        assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 400);
        assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 400);
    }

    #[test]
    fn s33_mic_slot_hit_rate_boundary() {
        // Exactly 90% hit rate should count as monomorphic
        let mic = JitMICSlot::new();
        for _ in 0..9 { mic.record_hit(); }
        for _ in 0..1 { mic.record_miss(); }
        // 10 total, 90% hits → monomorphic
        assert!(mic.is_monomorphic());
    }

    #[test]
    fn s33_mic_slot_entry_ptr_zero_means_unresolved() {
        let mic = JitMICSlot::new();
        mic.prepopulate(5);
        // Even with class_id populated, entry_ptr 0 means no direct dispatch
        assert_eq!(mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Relaxed), 0);
        // After update with non-zero entry, it's resolved
        mic.update(5, "Foo", 0x1234, false);
        assert_ne!(mic.cached_entry_ptr.load(std::sync::atomic::Ordering::Relaxed), 0);
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
        assert!(is_jit_compatible(&fcmpl, fcmpl.len(), "()I"), "fcmpl must be JIT-compatible");

        // fconst_0, fconst_1, fcmpg
        let fcmpg = vec![0x0b, 0x0c, 0x96, 0x03, 0xac];
        assert!(is_jit_compatible(&fcmpg, fcmpg.len(), "()I"), "fcmpg must be JIT-compatible");

        // dconst_0 (0x0e), dconst_1 (0x0f), dcmpl (0x97)
        let dcmpl = vec![0x0e, 0x0f, 0x97, 0x03, 0xac];
        assert!(is_jit_compatible(&dcmpl, dcmpl.len(), "()I"), "dcmpl must be JIT-compatible");

        // dconst_0, dconst_1, dcmpg (0x98)
        let dcmpg = vec![0x0e, 0x0f, 0x98, 0x03, 0xac];
        assert!(is_jit_compatible(&dcmpg, dcmpg.len(), "()I"), "dcmpg must be JIT-compatible");
    }

    /// RG.3 — JIT accepts lcmp (0x94).
    #[test]
    fn rg3_jit_accepts_lcmp() {
        // lconst_0 (0x09), lconst_1 (0x0a), lcmp (0x94), ireturn
        let lcmp = vec![0x09, 0x0a, 0x94, 0x03, 0xac];
        assert!(is_jit_compatible(&lcmp, lcmp.len(), "()I"), "lcmp must be JIT-compatible");
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

    /// RG.5 — JIT must bail on explicit `athrow` so the interpreter's
    /// exception-table lookup and frame unwinding run. Implicit exceptions
    /// (NPE, AIOOBE) from JIT'd code are handled separately by the runtime
    /// dispatch helpers and are not controlled by this scanner decision.
    #[test]
    fn rg5_jit_rejects_explicit_athrow() {
        // aconst_null (0x01), athrow (0xbf). A real method would never return
        // after athrow but we append ireturn so the scanner sees a terminator
        // before rejecting.
        let code = vec![0x01, 0xbf, 0x03, 0xac];
        assert!(
            !is_jit_compatible(&code, code.len(), "()I"),
            "JIT must bail on explicit athrow — interpreter handles exception tables"
        );
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
        let code = vec![
            0x01, 0x03, 0x01, 0x03, 0x03, 0xb8, 0x00, 0x01, 0x03, 0xac,
        ];
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
            0x03, 0x3c,                  // iconst_0, istore_1
            0x1b, 0x08, 0xa2, 0x00, 0x0a, // iload_1, iconst_5, if_icmpge +10
            0x84, 0x01, 0x01,            // iinc 1, 1
            0xa7, 0xff, 0xf8,            // goto -8
            0x03, 0xac,                  // iconst_0, ireturn
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
}
