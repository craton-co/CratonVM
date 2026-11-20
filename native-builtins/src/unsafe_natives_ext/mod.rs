// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `jdk.internal.misc.Unsafe` / `sun.misc.Unsafe` natives, the off-heap arena and the field-offset model.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use crate::lang_class::ClassTextName;
use super::*;
use crate::unsafe_natives::slot0_read_keep_alive;
/// JIT round 23 wave 1, lane L4 (`CRATONVM_UNSAFE_STATIC_HW_RMW`): every
/// `Unsafe` read-modify-write of a registered static asks the hardware update
/// BEFORE its plain read-then-write, so the unlocked fallback is reached only
/// when the VM declines. A source check: the order is the contract, and a
/// mock context has no static cells to race.
#[cfg(test)]
mod r23_w1_l4_static_hw_rmw_tests;

/// JIT tier2 wave 4, page `jit-r21-scas21-residual-proposals.md` (tag `scas21`): the
/// static-publication note asks the descriptor through the registration row's
/// memo, never through a fresh `declared_fields` scan (atm21 review R4: the
/// scan allocated per field on every whole-static atomic of an offset that can
/// never publish). A source check: a mock context has no declared fields to
/// count, and the memo is the contract.
#[cfg(test)]
mod scas21_note_descriptor_memo_tests;

/// r23w1 (R23W1L4-1): the behavioural half of the ratchet above. A mock context
/// owns no hardware cell (`static_field_hw_update` is the trait default,
/// `None`), so [`unsafe_static_rmw`] takes its fallback: the read-modify-write
/// under the class monitor (`vh_with_static_lock`). The fallback must answer
/// what the hardware update answers (the witness and whether it stored), leave
/// the monitor balanced, keep a failed compare from storing, and carry a
/// reference operand through the pin / re-read the lock entry makes.
#[cfg(test)]
mod r23w1_l4_static_rmw_lock_fallback_tests;

#[cfg(test)]
mod i127_float_bits_cas_tests;
macro_rules! unsafe_subword_volatile {
    ($get:ident, $put:ident, $mbget:ident, $mbput:ident, $width:expr, $access:expr) => {
        /// A volatile sub-word load: raw memory and array byte lanes by the
        /// plain accessor (which reads the REAL bytes), everything else by the
        /// width-matched volatile native.
        pub fn $get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            if unsafe_subword_is_real_bytes(ctx, args, $width)
                || ($width == 1 && unsafe_is_boolean_array(ctx, args))
            {
                return $mbget(ctx, args);
            }
            // Interpreter round i4 wave 95, lane L3: a FIELD of another kind,
            // or an offset inside a field, is the field view's, as for the
            // plain getters (`getShortVolatile` of a `char` is its 16 bits as
            // a short). JIT round 21, lane unsafe (page `i94-...-20261127.md`):
            // every other answer is narrowed to the access: the int-width
            // native answers the whole slot (a `long` field's `Value::Long`
            // reached the `S` return).
            if let Some(v) = unsafe_field_view_get(&*ctx, args, $access, false) {
                return Ok(Some(v));
            }
            unsafe_narrow_read(native_unsafe_get_int_volatile(ctx, args), $access, false)
        }

        /// The store half; see the getter.
        pub fn $put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            if unsafe_subword_is_real_bytes(ctx, args, $width)
                || ($width == 1 && unsafe_is_boolean_array(ctx, args))
            {
                return $mbput(ctx, args);
            }
            // JIT round 20 wave 8c: under byte field offsets, a store of
            // `$width` bytes inside / narrower than an instance field
            // replaces only those bytes (the int-width native would write
            // four). JIT round 21 (lane unsafefix; correctness,
            // unconditional): a STATIC's own (minted) offset with the switch
            // off too -- the narrow store merges into the static.
            if ctx.unsafe_byte_field_offsets() || is_synthetic_offset(unsafe_offset(args, 2)) {
                if let (Some(obj), Some(Value::Int(v))) = (unsafe_obj(args, 1), args.get(3)) {
                    // Cast: the value's low bytes in memory order.
                    let bytes = (*v as u32).to_le_bytes();
                    if unsafe_narrow_field_store(ctx, obj, unsafe_offset(args, 2), &bytes[..$width])
                    {
                        return Ok(None);
                    }
                }
            }
            // JIT round 21, lane unsafe: a field of another kind at this
            // offset (one the narrow store above declined: the same width)
            // takes the field view (`putShortVolatile` into a `char`
            // stored the sign-extended short into the char's slot).
            if unsafe_field_view_put(ctx, args, $access) {
                return Ok(None);
            }
            native_unsafe_put_int_volatile(ctx, args)
        }
    };
}

macro_rules! unsafe_multibyte_get {
    ($name:ident, $width:expr, $assemble:expr, $access:expr) => {
        pub fn $name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            // A FIELD of another kind (i4 wave 87, lane L3).
            if let Some(v) =
                unsafe_field_view_get(&*ctx, args, $access, unsafe_big_endian_arg(args))
            {
                return Ok(Some(v));
            }
            let offset = unsafe_offset(args, 2);
            if unsafe_obj(args, 1).is_none() {
                let addr = unsafe_raw_addr(args, 2);
                let mut bytes = [0u8; $width];
                if ctx.copy_from_native_memory(addr, &mut bytes) {
                    let v: i64 = $assemble(&bytes, unsafe_big_endian_arg(args));
                    return Ok(Some(Value::Int(v as i32)));
                }
            }
            if let Some(obj) = unsafe_obj(args, 1) {
                // Stack buffer, not a `Vec`: this is the per-accessor path
                // under every `HeapByteBuffer.getShort`/`getInt`, so the
                // allocation was one malloc per scalar read.
                let mut buf = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
                if unsafe_read_bytes_into(ctx, obj, offset, $width, &mut buf) {
                    let big_endian = unsafe_big_endian_arg(args);
                    let v: i64 = $assemble(&buf[..$width], big_endian);
                    return Ok(Some(Value::Int(v as i32)));
                }
            }
            // Not a primitive-array/direct-memory target: generic element/field
            // access, narrowed to this access (JIT round 21, lane unsafe).
            let big_endian = unsafe_big_endian_arg(args);
            unsafe_narrow_read(native_unsafe_get_int(ctx, args), $access, big_endian)
        }
    };
}

macro_rules! unsafe_multibyte_put {
    ($name:ident, $width:expr, $generic:ident, $natural:ident, $access:expr) => {
        pub fn $name(
            ctx: &mut dyn NativeContext,
            args: &[Value],
        ) -> MethodCallResult {
            // A FIELD of another kind (i4 wave 87, lane L3).
            if unsafe_field_view_put(ctx, args, $access) {
                return Ok(None);
            }
            let offset = unsafe_offset(args, 2);
            let val = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            // The bigEndian flag, when present, is args[4].
            let big_endian = matches!(args.get(4), Some(Value::Int(z)) if *z != 0);
            let raw = (val as u32) & ((1u64 << ($width * 8)) - 1) as u32;
            let mut bytes = [0u8; $width];
            if big_endian {
                for i in 0..$width {
                    bytes[$width - 1 - i] = (raw >> (i * 8)) as u8;
                }
            } else {
                for i in 0..$width {
                    bytes[i] = (raw >> (i * 8)) as u8;
                }
            }
            match unsafe_obj(args, 1) {
                None => {
                    if ctx.copy_to_native_memory(unsafe_raw_addr(args, 2), &bytes) {
                        return Ok(None);
                    }
                }
                Some(obj) => {
                    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
                        && matches!(
                            ctx.heap_element_type_of(obj),
                            cratonvm_types::ArrayElementType::Byte
                                | cratonvm_types::ArrayElementType::Boolean
                        )
                        && unsafe_write_bytes_to_byte_array(ctx, obj, offset, &bytes)
                    {
                        return Ok(None);
                    }
                    // A short inside a wider array element (`SubwordLane`):
                    // atomic against the element's other lanes.
                    if $width <= 2 && unsafe_subword_array_store(ctx, obj, offset, &bytes) {
                        return Ok(None);
                    }
                    // SBR3 ffmperf (after the lane: whatever it declines):
                    // another primitive element type, or an
                    // access straddling elements: the raw store.
                    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
                        && unsafe_put_needs_raw(
                            ctx.heap_element_type_of(obj),
                            cratonvm_types::ArrayElementType::$natural,
                            offset,
                            $width,
                        )
                        && unsafe_array_write_bytes(ctx, obj, offset, &bytes)
                    {
                        return Ok(None);
                    }
                    // JIT round 20 wave 8 (lane huntDisp): a store narrower
                    // than the instance field at this offset.
                    if unsafe_narrow_field_store(ctx, obj, offset, &bytes) {
                        return Ok(None);
                    }
                }
            }
            $generic(ctx, args)
        }
    };
}

// ===========================================================================
// WP1.2 — Process-local arena / cleaner registries used by unsafe_natives.
//
// The arena/cleaner state used to live over in `vm/src/runtime/unsafe_helpers.rs`
// but the crate graph is `vm -> native-builtins`, so putting the store
// there would create a cycle. The authoritative store now lives in
// this crate (next to every other Unsafe native); `unsafe_helpers` in
// vm now provides a VM-internal facade over the same singletons via
// the `LocalUnsafeArena` trait in native-api. That trait gives
// VM-side subsystems (GC, JIT arena cleanup on shutdown) access
// without forcing them to take a dependency on this crate.
// ===========================================================================

mod unsafe_arena {
    use parking_lot::{Mutex, RwLock};
    use std::collections::BTreeMap;

    /// R1 (silent-corruption fix): reserved high tag bit OR-ed into every
    /// arena handle. Real OS pointers on every platform CratonVM targets live
    /// in the low address space — Windows x64 user mode is capped at 2^47-1
    /// (128 TiB) and even Linux 5-level paging tops user space at 2^56-1 — so
    /// bit 62 is NEVER set on a real pointer (e.g. a `ByteBuffer.allocateDirect`
    /// address from `dbb_allocate`). Setting it on every handle makes arena
    /// handles *provably disjoint* from real pointers, so the range-membership
    /// scan in `locate`/`contains` can never mis-classify a real pointer that
    /// happens to fall numerically inside a live block's `[base, base+len)` as
    /// an arena handle and silently corrupt it.
    ///
    /// The tag is part of the address value end-to-end — it is NEVER stripped.
    /// Handles flow opaquely through Java as `long` (`DirectByteBuffer.address()`,
    /// `Unsafe.get/put/copy/free`) and come back to `locate` unchanged, so every
    /// consumer round-trips the tagged value. We deliberately avoid bit 63 so
    /// handles stay positive `i64`.
    pub(super) const ARENA_TAG: i64 = 1 << 62; // 0x4000_0000_0000_0000
    /// nio21 wave 1 L3 (the JIT-readable arena): bit 61 marks a LEGACY handle,
    /// one bumped out of the byte-granular handle space below. A handle with
    /// bit 62 set and bit 61 clear is a SLOTTED handle (see [`JitSlot`]).
    /// Nothing outside this module reads bit 61: every other consumer tests
    /// bit 62 only, and both kinds round-trip as opaque `long`s.
    const LEGACY_BIT: i64 = 1 << 61;
    /// First legacy handle handed out: the tag and the legacy bit OR-ed onto
    /// the historical 2^36 base.
    const ARENA_BASE: i64 = ARENA_TAG | LEGACY_BIT | 0x10_0000_0000;

    // -----------------------------------------------------------------------
    // The JIT-readable arena (nio21 wave 1 L3; page
    // `docs/feature-designs/native-dispatch-architecture.md`)
    // -----------------------------------------------------------------------
    //
    // A block smaller than `2^JIT_SLOT_SHIFT` bytes gets a SLOTTED handle
    // `ARENA_TAG | seq << JIT_SLOT_SHIFT`, where `seq` is a process-monotonic
    // 31-bit sequence number (bits 30..60), and its `(ptr, len)` is published in
    // `table[seq & (JIT_SLOTS - 1)]` under the key = the handle itself. The
    // optimizing tier's scoped fold (`jit/src/ir_lower.rs`
    // `Lowerer::emit_scoped_arena_resolve`) then resolves any address inside
    // the block with no lock and no call:
    //
    //   slot = table + ((a >> 30) & mask) * 32
    //   key  = a & !(2^30 - 1)
    //   [slot].key == key  ;  (a & (2^30-1)) + width <= [slot].len
    //   real = [slot].ptr + (a & (2^30-1))  ;  [slot].key == key again
    //
    // # Why a key that matches twice makes `(ptr, len)` the block's own
    //
    // * A key is published at most ONCE in the life of the process: `seq` never
    //   repeats (it is never reset, and an exhausted sequence falls back to
    //   legacy handles), so after a slot's key is cleared that key never
    //   reappears in any slot.
    // * The writer stores `ptr`, `len`, then `key` (release); a reader loads
    //   `key`, `len`, `ptr`, `key` (x86-64 loads are not reordered with older
    //   loads, and stores are seen in one total order). If both key loads see
    //   K, no clear of K had become visible before the second one, so neither
    //   had the next occupant's `ptr` / `len` (stored after that clear); and
    //   the first load seeing K means K's own `ptr` / `len` were visible.
    // * `free` and `reallocate` clear the key BEFORE they touch the block, so a
    //   use-after-free or use-after-realloc that is ordered after the free (the
    //   same thread, or a happens-before edge) misses and leaves for the
    //   interpreter, whose slow path keeps answering "no such block".
    //
    // # What it does not cover (the same as HotSpot)
    //
    // A `freeMemory` on another thread RACING a compiled access to the same
    // block (no happens-before between them) may let the access touch the
    // freed buffer. That is a data race on raw memory that the Java program
    // owns, and HotSpot's `free(3)` gives no more. Every path the fold serves
    // has no such race by construction: a `DirectByteBuffer` frees its block
    // through its `Cleaner` only once it is unreachable, and the compiled
    // access holds it (the `reachabilityFence(this)` keep-alive node, and no
    // safepoint between the lookup and the access); a confined session closes
    // only on its owner thread, which is the accessing thread; the never-closing
    // sessions never close; a SHARED session is not admitted by the fold (it
    // keeps its deopt), so a shared close never frees memory under a compiled
    // access.
    //
    // A block that is reallocated in place loses its slot for good (the key
    // cannot be re-published), and keeps working through the locked slow path.
    // `CRATONVM_UNSAFE_ARENA_JIT_TABLE=0` (read once, when the store is built)
    // hands out legacy handles only and publishes nothing.

    /// Low bits of a slotted handle that address inside its block.
    pub(super) const JIT_SLOT_SHIFT: u32 = 30;
    /// Bits of `seq` that index the table.
    pub(super) const JIT_INDEX_BITS: u32 = 15;
    /// `log2(size_of::<JitSlot>())`.
    pub(super) const JIT_ENTRY_SHIFT: u32 = 5;
    const JIT_SLOTS: usize = 1 << JIT_INDEX_BITS;
    /// `seq` is 31 bits (handle bits 30..60); bit 61 stays the legacy bit.
    const JIT_SEQ_LIMIT: u64 = 1 << 31;
    /// A slotted block must be strictly shorter than its handle window, so
    /// every offset inside it (and one past its end) keeps the window's key.
    const JIT_MAX_BLOCK: usize = (1 << JIT_SLOT_SHIFT) - 16;
    /// How many occupied table entries a claim skips before it settles for a
    /// legacy handle.
    const JIT_CLAIM_TRIES: usize = 64;
    /// A table entry claimed but not yet published. Never a handle's key: a
    /// key has its low 30 bits clear.
    const JIT_CLAIMED: i64 = 1;

    /// One entry of the JIT-readable table. `#[repr(C)]`, 32 bytes: the
    /// optimizing tier bakes the field offsets (`ir_lower::SCOPED_ARENA_*`;
    /// the VM's `jit_well_known_class_id` arm asserts they agree).
    #[repr(C, align(32))]
    #[derive(Default)]
    pub(super) struct JitSlot {
        /// The slotted handle of the block published here, `0` for a free
        /// entry, [`JIT_CLAIMED`] while an allocation is publishing it.
        key: std::sync::atomic::AtomicI64,
        /// The block's length in bytes.
        len: std::sync::atomic::AtomicU64,
        /// The real address of the block's first byte.
        ptr: std::sync::atomic::AtomicUsize,
    }

    pub(super) const JIT_SLOT_KEY_OFFSET: usize = std::mem::offset_of!(JitSlot, key);
    pub(super) const JIT_SLOT_LEN_OFFSET: usize = std::mem::offset_of!(JitSlot, len);
    pub(super) const JIT_SLOT_PTR_OFFSET: usize = std::mem::offset_of!(JitSlot, ptr);
    const _: () = assert!(std::mem::size_of::<JitSlot>() == 1 << JIT_ENTRY_SHIFT);
    const _: () = assert!(JIT_SLOT_SHIFT as u64 + 31 == 61);

    /// The two handle cursors, under the one `next_addr` mutex.
    struct HandleCursor {
        /// Next legacy handle.
        legacy: i64,
        /// Next slotted `seq`; [`JIT_SEQ_LIMIT`] once exhausted.
        seq: u64,
    }

    /// The alignment of every block's storage: what `malloc` gives on the
    /// 64-bit targets, and what the JDK relies on. `NativeMemorySegmentImpl
    /// .makeNativeSegment` over-allocates only for an alignment above
    /// `MAX_MALLOC_ALIGN` (16) and otherwise aligns the address
    /// `allocateMemory` returned UP in place, so a real address that is less
    /// aligned than this would put the segment past its block. (The global
    /// allocator is mimalloc, whose small blocks are 8-aligned, so a `Vec<u8>`
    /// does not give it.)
    const ARENA_ALIGN: usize = 16;

    /// A block's storage: `len` bytes at an [`ARENA_ALIGN`]-aligned address,
    /// owning `cap` bytes (the allocation, never smaller than `len`). Reads as
    /// the `[u8]` it holds. Shrinking keeps the allocation, growing within
    /// `cap` zero-fills, as `Vec::resize` within its capacity does; growing
    /// past `cap` is a new buffer ([`Self::try_resized`]).
    pub(super) struct ArenaBuf {
        ptr: std::ptr::NonNull<u8>,
        len: usize,
        cap: usize,
    }

    // SAFETY: an `ArenaBuf` owns its allocation exclusively, as a `Vec<u8>` does.
    unsafe impl Send for ArenaBuf {}
    // SAFETY: shared access is `&[u8]` only.
    unsafe impl Sync for ArenaBuf {}

    impl ArenaBuf {
        fn layout(cap: usize) -> Option<std::alloc::Layout> {
            std::alloc::Layout::from_size_align(cap.max(1), ARENA_ALIGN).ok()
        }

        /// `len` zeroed bytes, or `None` when the allocation fails (never the
        /// allocation-error hook, which aborts: see [`ArenaStore::try_allocate`]).
        /// A zero-length buffer still owns one allocation, so its address is
        /// distinct from every other live block's, as `malloc(0)`'s is.
        pub(super) fn try_zeroed(len: usize) -> Option<Self> {
            let layout = Self::layout(len)?;
            // SAFETY: `layout` has a non-zero size.
            let ptr = std::ptr::NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })?;
            Some(Self { ptr, len, cap: len })
        }

        /// A new buffer of `len` bytes holding this one's first
        /// `min(len, self.len)` bytes, the rest zero.
        fn try_resized(&self, len: usize) -> Option<Self> {
            let mut fresh = Self::try_zeroed(len)?;
            let n = len.min(self.len);
            fresh[..n].copy_from_slice(&self[..n]);
            Some(fresh)
        }

        /// Resize without moving, when `len` fits the allocation; `false`
        /// (and nothing changed) otherwise.
        fn resize_in_place(&mut self, len: usize) -> bool {
            if len > self.cap {
                return false;
            }
            if len > self.len {
                // SAFETY: `[self.len, len)` lies inside the allocation (`len <= cap`).
                unsafe { std::ptr::write_bytes(self.ptr.as_ptr().add(self.len), 0, len - self.len) };
            }
            self.len = len;
            true
        }
    }

    impl std::ops::Deref for ArenaBuf {
        type Target = [u8];
        fn deref(&self) -> &[u8] {
            // SAFETY: `len <= cap` bytes of the buffer's own allocation, initialised
            // (zeroed at allocation, written only through this type or by the
            // natives it is lent to).
            unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
        }
    }

    impl std::ops::DerefMut for ArenaBuf {
        fn deref_mut(&mut self) -> &mut [u8] {
            // SAFETY: as `deref`, and `&mut self` is exclusive.
            unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
        }
    }

    impl Drop for ArenaBuf {
        fn drop(&mut self) {
            if let Some(layout) = Self::layout(self.cap) {
                // SAFETY: allocated in `try_zeroed` with this very layout.
                unsafe { std::alloc::dealloc(self.ptr.as_ptr(), layout) };
            }
        }
    }

    struct Arena {
        bytes: ArenaBuf,
        /// Bytes of HANDLE space this block owns, which is what `try_allocate`
        /// bumped `next_addr` by -- not what the block currently holds.
        ///
        /// `locate` resolves an address to the greatest base at or below it, so
        /// a block whose `bytes` grow past this would answer for handles
        /// already issued to the blocks above it: two live blocks aliasing one
        /// address range, with every access to the later one silently
        /// redirected into the earlier. `reallocate` keeps
        /// `bytes.len() <= reserved` and relocates rather than break it.
        reserved: usize,
    }

    /// Handle space a block of `size` bytes owns. `next_addr` bumps by this,
    /// and [`Arena::reserved`] records it.
    fn reserved_for(size: usize) -> usize {
        (size.max(1) + 15) & !15
    }

    pub(super) struct ArenaStore {
        inner: RwLock<BTreeMap<i64, Arena>>,
        next_addr: Mutex<HandleCursor>,
        /// The JIT-readable table (see the section comment above
        /// [`JIT_SLOT_SHIFT`]); empty when `CRATONVM_UNSAFE_ARENA_JIT_TABLE=0`.
        /// Allocated once and never resized, so its address is fixed for the
        /// life of the process (the store is never dropped).
        jit: Box<[JitSlot]>,
        /// Blocks whose REAL pointer has been handed to native code, and how
        /// many times. See [`ArenaStore::real_ptr`].
        ///
        /// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Every acquisition
        /// is a single statement over a `BTreeMap<i64, u64>` with no
        /// `NativeContext` in scope.
        ///
        /// It is safely `Scratch` only BECAUSE the enclosing lock is
        /// unordered: `real_ptr` holds `inner.write()` across this
        /// acquisition. Same caveat the JFR tables carry — if `inner` is ever
        /// given a level it must be a HIGHER one than this, never an equal.
        translated: cratonvm_types::lock_order::OrderedPlMutex<BTreeMap<i64, u64>>,
        /// The backing buffer a `reallocate` displaced out from under an
        /// OUTSTANDING real pointer, kept alive so that the pointer keeps
        /// naming memory this process owns. See [`ArenaStore::reallocate`].
        ///
        /// LOCK LEVEL: `Scratch`, for the same reason `translated` is. Unlike
        /// `translated` it is acquired with NO other arena lock held, so the
        /// caveat there about the enclosing `inner.write()` does not apply.
        retired: cratonvm_types::lock_order::OrderedPlMutex<BTreeMap<i64, ArenaBuf>>,
        /// Every live non-empty block by its REAL base: `real base -> (length,
        /// handle base)`, the reverse of `inner` (see [`ArenaStore::handle_of_real`]).
        /// Written only while `inner`'s write lock is held, and in step with
        /// it; read alone.
        by_real: cratonvm_types::lock_order::OrderedPlRwLock<BTreeMap<usize, (usize, i64)>>,
        membership_generation: AtomicU64,
        /// ADDR21-1 Option A (`CRATONVM_UNSAFE_REAL_POINTERS`, default ON since 2026-10-27, `=0` off, read
        /// once when the store is built): a block's key -- the address
        /// `allocateMemory` returns -- is its storage's REAL address, as on
        /// HotSpot, instead of a tagged handle. Every lookup below is a range
        /// test on the key, so it answers the same way in both modes. The JIT
        /// table exists but stays empty (an untagged address never resolves
        /// through it); no handle cursor and no [`Self::by_real`] (the
        /// reverse of an identity). An address C or the JIT dereferences directly is then
        /// the block's own memory; a freed block's address is no longer
        /// anyone's and is reused by the allocator, which is HotSpot's model
        /// (see `docs/feature-designs/jit-ir-optimizations.md`).
        real: bool,
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Real pointers handed to native code — see [`ArenaStore::real_ptr`].
    pub(super) static TRANSLATIONS: AtomicU64 = AtomicU64::new(0);
    /// Reallocations of a block whose real pointer was already handed out.
    ///
    /// **`Vec::resize` may MOVE the buffer**, so every pointer handed out before
    /// this call is now dangling and points at memory the process allocator has
    /// taken back. Nonzero here means native code is holding at least one such
    /// pointer.
    pub(super) static STALE_ON_REALLOC: AtomicU64 = AtomicU64::new(0);
    /// Frees of a block whose real pointer was already handed out. Same hazard:
    /// the `Vec` is dropped and its memory returned to the allocator.
    pub(super) static STALE_ON_FREE: AtomicU64 = AtomicU64::new(0);
    /// Displaced buffers that [`ArenaStore::reallocate`] RETAINED rather than
    /// dropped, because a real pointer into them was outstanding. This is the
    /// count of times the dangling-pointer window above was closed.
    pub(super) static RETAINED_ON_REALLOC: AtomicU64 = AtomicU64::new(0);
    /// Translations refused by `unsafe_arena_real_ptr_bounded` because the
    /// block could not cover the length being advertised with the pointer.
    pub(super) static SHORT_TRANSLATIONS: AtomicU64 = AtomicU64::new(0);

    impl ArenaStore {
        fn new() -> Self {
            let real =
                true;
            Self::with_mode(real)
        }

        /// The store in handle mode (`real == false`, `CRATONVM_UNSAFE_REAL_POINTERS=0`) or in
        /// real-pointer mode ([`Self::real`]).
        pub(super) fn with_mode(real: bool) -> Self {
            // Real-pointer mode keeps the table too, though nothing is ever
            // published in it: the JIT admits its off-heap folds only when the
            // table exists (`ir_lower::scoped_arena_jit_table`), and their
            // lowering already loads an untagged address raw. Without it every
            // compiled native-segment access declined to the call (300x on
            // `R26SegLongLoops` native-put).
            let jit: Box<[JitSlot]> = {
                (0..JIT_SLOTS).map(|_| JitSlot::default()).collect()
            };
            let seq = if jit.is_empty() { JIT_SEQ_LIMIT } else { 1 };
            Self {
                inner: RwLock::new(BTreeMap::new()),
                next_addr: Mutex::new(HandleCursor {
                    legacy: ARENA_BASE,
                    seq,
                }),
                jit,
                translated: cratonvm_types::lock_order::OrderedPlMutex::new(
                    BTreeMap::new(),
                    cratonvm_types::lock_order::LockLevel::Scratch,
                ),
                retired: cratonvm_types::lock_order::OrderedPlMutex::new(
                    BTreeMap::new(),
                    cratonvm_types::lock_order::LockLevel::Scratch,
                ),
                // Leaf index operations take no other lock and never re-enter Java.
                by_real: cratonvm_types::lock_order::OrderedPlRwLock::new(
                    BTreeMap::new(),
                    cratonvm_types::lock_order::LockLevel::Scratch,
                ),
                membership_generation: AtomicU64::new(0),
                real,
            }
        }

        /// Is this store in real-pointer mode ([`Self::real`])?
        pub(super) fn real_pointers(&self) -> bool {
            self.real
        }

        #[inline]
        pub(super) fn membership_generation(&self) -> u64 {
            self.membership_generation.load(Ordering::Acquire)
        }

        /// Can `addr` name a block of this store at all? A tagged handle in
        /// handle mode; any positive address in real-pointer mode. The
        /// cheap screen in front of every locked lookup.
        #[inline]
        fn may_own(&self, addr: i64) -> bool {
            if self.real {
                addr > 0
            } else {
                addr & ARENA_TAG != 0
            }
        }

        /// The key a new block of `size` bytes and `reserved` bytes of handle
        /// space is filed under: its storage's address `ptr` in real-pointer
        /// mode, else a fresh handle ([`Self::claim_handle`]). `true` when the
        /// handle's JIT table entry was claimed for it.
        fn claim_key(&self, size: usize, reserved: usize, ptr: usize) -> (i64, bool) {
            if self.real {
                // Cast: a user-space address, below 2^47.
                (ptr as i64, false)
            } else {
                self.claim_handle(size, reserved)
            }
        }

        /// Insert `arena` at `handle` into `inner` (whose write lock the caller
        /// holds) and into [`Self::by_real`].
        fn put(&self, inner: &mut BTreeMap<i64, Arena>, handle: i64, arena: Arena) {
            if !self.real && !arena.bytes.is_empty() {
                self.by_real
                    .write()
                    .insert(arena.bytes.as_ptr() as usize, (arena.bytes.len(), handle));
            }
            inner.insert(handle, arena);
        }

        /// Remove the block at `handle` from `inner` (whose write lock the
        /// caller holds) and from [`Self::by_real`].
        fn take(&self, inner: &mut BTreeMap<i64, Arena>, handle: i64) -> Option<Arena> {
            let arena = inner.remove(&handle)?;
            // Free and every resize retire positive membership proofs before storage changes.
            if self.real {
                self.membership_generation.fetch_add(1, Ordering::Release);
            }
            if !self.real && !arena.bytes.is_empty() {
                let mut by_real = self.by_real.write();
                let real = arena.bytes.as_ptr() as usize;
                if by_real.get(&real).is_some_and(|&(_, h)| h == handle) {
                    by_real.remove(&real);
                }
            }
            Some(arena)
        }

        /// The handle Java names the byte at REAL address `real` by: inside (or
        /// one past the end of) a live block, the block's handle plus the
        /// offset; `None` for any other address.
        ///
        /// The reverse of [`Self::peek`], for a pointer read back out of memory
        /// (`ffm-address-read-back-of-an-arena-pointer-is-the-real-address-not-the-handle`):
        /// a Java store of a segment into an `ADDRESS` slot writes the block's
        /// real address so that C can read it, and reading it back must answer
        /// the segment `address()` named, as on HotSpot. A block that starts
        /// exactly where another ends wins the shared address (offset 0 of the
        /// later block, not one past the earlier).
        pub(super) fn handle_of_real(&self, real: usize) -> Option<i64> {
            // Real-pointer mode: the real address IS the name.
            if self.real {
                return None;
            }
            let by_real = self.by_real.read();
            let (&base, &(len, handle)) = by_real.range(..=real).next_back()?;
            let offset = real - base;
            if offset > len {
                return None;
            }
            // Cast: `offset <= len`, a block's length.
            Some(handle + offset as i64)
        }

        /// `None` when the backing buffer could not be allocated.
        ///
        /// `vec![0u8; size]` is INFALLIBLE: on failure Rust runs the
        /// allocation-error hook, which aborts the process. That is what
        /// `Unsafe.allocateMemory(Long.MAX_VALUE)` did -- SIGABRT with
        /// `memory allocation of 9223372036854775807 bytes failed`, taking the
        /// whole VM down where HotSpot throws. A Java caller must never be
        /// able to abort the runtime by passing a large number to a method
        /// whose contract is "returns 0 / throws OutOfMemoryError".
        pub(super) fn try_allocate(&self, size: usize) -> Option<i64> {
            let bytes = ArenaBuf::try_zeroed(size)?;
            let reserved = reserved_for(size);
            // The buffer does not move when the `ArenaBuf` moves into the map.
            let (ptr, len) = (bytes.as_ptr() as usize, bytes.len());
            let (addr, slotted) = self.claim_key(size, reserved, ptr);
            self.put(&mut self.inner.write(), addr, Arena { bytes, reserved });
            if slotted {
                self.jit_publish(addr, ptr, len);
            }
            Some(addr)
        }

        /// Claim handle space for a block. Never reuses an address (neither
        /// cursor ever moves back): a freed block's handles stay dead so a
        /// use-after-free keeps resolving to nothing instead of to somebody
        /// else's bytes.
        ///
        /// A fresh handle for a block of `size` bytes owning `reserved` bytes of
        /// handle space: SLOTTED (`true`, its table entry claimed for it) when
        /// the table is on, the block fits a window, `seq` is not exhausted and
        /// one of the next [`JIT_CLAIM_TRIES`] entries is free; LEGACY otherwise.
        ///
        /// Takes no lock but `next_addr`, so it is safe to call while `inner`
        /// is held.
        fn claim_handle(&self, size: usize, reserved: usize) -> (i64, bool) {
            let mut c = self.next_addr.lock();
            if !self.jit.is_empty() && size > 0 && size < JIT_MAX_BLOCK {
                for _ in 0..JIT_CLAIM_TRIES {
                    let seq = c.seq;
                    if seq >= JIT_SEQ_LIMIT {
                        break;
                    }
                    c.seq = seq + 1;
                    // Cast: `seq < 2^31`, so the index is in range.
                    let slot = &self.jit[(seq as usize) & (JIT_SLOTS - 1)];
                    // Only a `0` entry is claimable, and only under this lock;
                    // `jit_unpublish` is the one writer of `0`.
                    if slot.key.load(Ordering::Acquire) == 0 {
                        slot.key.store(JIT_CLAIMED, Ordering::Relaxed);
                        // Cast: `seq < 2^31`, so `seq << 30 < 2^61`.
                        return (ARENA_TAG | ((seq as i64) << JIT_SLOT_SHIFT), true);
                    }
                }
            }
            let a = c.legacy;
            c.legacy = c.legacy.saturating_add(reserved as i64);
            (a, false)
        }

        /// The table entry a SLOTTED handle `addr` indexes, or `None` for a
        /// legacy or untagged address (or the table off).
        fn jit_slot_of(&self, addr: i64) -> Option<&JitSlot> {
            if self.jit.is_empty() || addr & ARENA_TAG == 0 || addr & LEGACY_BIT != 0 {
                return None;
            }
            // Cast: a positive i64 shifted right; masked to the index bits.
            let index = ((addr >> JIT_SLOT_SHIFT) as usize) & (JIT_SLOTS - 1);
            self.jit.get(index)
        }

        /// Publish the block whose (slotted) handle is `addr`: `ptr` / `len`
        /// first, the key last, with release order (see the section comment).
        /// Its entry was claimed for it by [`Self::claim_handle`].
        fn jit_publish(&self, addr: i64, ptr: usize, len: usize) {
            let Some(slot) = self.jit_slot_of(addr) else {
                return;
            };
            slot.ptr.store(ptr, Ordering::Relaxed);
            slot.len.store(len as u64, Ordering::Relaxed);
            slot.key.store(addr, Ordering::Release);
        }

        /// Withdraw the block whose handle is `addr` from the table, for good:
        /// its key will never be published again. A no-op unless the entry is
        /// that block's. Called BEFORE the block is freed or resized.
        fn jit_unpublish(&self, addr: i64) {
            if let Some(slot) = self.jit_slot_of(addr) {
                let _ = slot
                    .key
                    .compare_exchange(addr, 0, Ordering::AcqRel, Ordering::Relaxed);
            }
        }

        /// The table's address, `0` when it is off. Fixed for the process.
        pub(super) fn jit_table_addr(&self) -> usize {
            if self.jit.is_empty() {
                0
            } else {
                self.jit.as_ptr() as usize
            }
        }

        /// Is the block whose handle is `addr` published in the table now?
        #[cfg(test)]
        pub(super) fn jit_published(&self, addr: i64) -> bool {
            self.jit_slot_of(addr)
                .is_some_and(|s| s.key.load(Ordering::Acquire) == addr)
        }

        /// What the optimizing tier's inline resolve answers for `addr` and an
        /// access of `width` bytes: the real address, or `None` (the fold's
        /// deopt). The same steps in the same order. Since JIT r23 wave 5 (lane
        /// L8) also the compiled FFM element door's resolve
        /// ([`super::unsafe_arena_jit_resolve`]), not only the tests'.
        pub(super) fn jit_resolve(&self, addr: i64, width: u64) -> Option<usize> {
            let slot = self.jit_slot_of(addr)?;
            let key = addr & !((1i64 << JIT_SLOT_SHIFT) - 1);
            if slot.key.load(Ordering::Acquire) != key {
                return None;
            }
            // Cast: masked to 30 bits.
            let off = (addr & ((1i64 << JIT_SLOT_SHIFT) - 1)) as u64;
            if off.checked_add(width)? > slot.len.load(Ordering::Relaxed) {
                return None;
            }
            // Cast: masked to 30 bits.
            let real = slot.ptr.load(Ordering::Relaxed).checked_add(off as usize)?;
            if slot.key.load(Ordering::Acquire) != key {
                return None;
            }
            Some(real)
        }

        pub(super) fn allocate(&self, size: usize) -> i64 {
            self.try_allocate(size).unwrap_or(0)
        }

        /// The live block holding `addr`, for the length of ONE downcall
        /// (`panama_libffi::arena_call_address`): the real pointer for `addr`,
        /// the block's real base, its length and its handle base. `None` for an
        /// untagged address, a freed handle, or a block that cannot cover `want`
        /// bytes from `addr` (`want == 0` may name one past the end). Records no
        /// translation: the caller holds the block's session for the call, so
        /// the block cannot be freed before the C function returns (a close
        /// meets the acquire), and reallocation never touches an arena block.
        pub(super) fn peek(&self, addr: i64, want: usize) -> Option<(*mut u8, usize, usize, i64)> {
            if !self.may_own(addr) {
                return None;
            }
            let inner = self.inner.read();
            let (&base, arena) = inner.range(..=addr).next_back()?;
            let offset = usize::try_from(addr - base).ok()?;
            let len = arena.bytes.len();
            if offset > len || (offset == len && want > 0) || len - offset < want {
                return None;
            }
            let real = arena.bytes.as_ptr() as *mut u8;
            // SAFETY: `offset <= len`, inside (or one past the end of) the
            // block's own allocation.
            Some((unsafe { real.add(offset) }, real as usize, len, base))
        }

        /// [`Self::peek`] through the JIT-readable table alone (JIT round 22,
        /// lane memory, FFM22-3): the same answer for a SLOTTED block -- its
        /// handle base is its entry's key, its real base the entry's `ptr`,
        /// its length the entry's `len` -- read as [`Self::jit_resolve`] reads
        /// it (the key before and after), with no lock. `None` for anything
        /// the table does not hold now; the caller then asks [`Self::peek`].
        pub(super) fn jit_peek(
            &self,
            addr: i64,
            want: usize,
        ) -> Option<(*mut u8, usize, usize, i64)> {
            let slot = self.jit_slot_of(addr)?;
            let key = addr & !((1i64 << JIT_SLOT_SHIFT) - 1);
            if slot.key.load(Ordering::Acquire) != key {
                return None;
            }
            // Cast: masked to 30 bits.
            let offset = (addr - key) as usize;
            let len = usize::try_from(slot.len.load(Ordering::Relaxed)).ok()?;
            let real = slot.ptr.load(Ordering::Relaxed);
            if slot.key.load(Ordering::Acquire) != key
                || offset > len
                || (offset == len && want > 0)
                || len - offset < want
            {
                return None;
            }
            Some(((real + offset) as *mut u8, real, len, key))
        }

        /// End ONE outstanding translation of the block holding `addr` (the pointer a
        /// native call took has been given up: the call returned). The counterpart of
        /// `real_ptr`'s record, so a short-lived pointer does not leave the block
        /// "translated" for `free` to report. A no-op for an untagged or unknown address.
        pub(super) fn release_ptr(&self, addr: i64) {
            if !self.may_own(addr) {
                return;
            }
            let base = {
                let inner = self.inner.read();
                match Self::locate(&inner, addr) {
                    Some((base, _)) => base,
                    None => return,
                }
            };
            let mut translated = self.translated.lock();
            if let Some(n) = translated.get_mut(&base) {
                if *n > 1 {
                    *n -= 1;
                } else {
                    translated.remove(&base);
                }
            }
        }

        #[cfg(test)]
        pub(super) fn block_is_translated(&self, addr: i64) -> bool {
            self.translated.lock().contains_key(&addr)
        }

        #[cfg(test)]
        pub(super) fn block_is_retired(&self, addr: i64) -> bool {
            self.retired.lock().contains_key(&addr)
        }

        /// `None` when the resize could not be allocated -- see
        /// [`Self::try_allocate`]. The block is left UNTOUCHED on failure,
        /// which is what `realloc(3)` guarantees and what the JDK's
        /// `reallocateMemory` contract relies on when it throws.
        ///
        /// `Some` carries the handle the block now lives at, which is NOT
        /// necessarily the one passed in -- see [`Self::reallocate`].
        pub(super) fn try_reallocate(&self, addr: i64, new_size: usize) -> Option<i64> {
            match self.reallocate(addr, new_size) {
                0 => None,
                a => Some(a),
            }
        }

        /// Resize a block: 0 if the new size could not be allocated, otherwise
        /// the handle the block now lives at, WHICH MAY DIFFER FROM `addr`.
        ///
        /// # Growing past the reserved handle space RELOCATES
        ///
        /// `try_allocate` bumps `next_addr` by the rounded request, so a block
        /// owns exactly that much handle space, and `locate` resolves an
        /// address to the greatest base at or below it. Growing `bytes` past
        /// that made the block answer for every handle issued after it: two
        /// live blocks aliasing one range, with reads and writes through the
        /// LATER handle silently landing in the earlier block. A 64-byte block
        /// grown to 4096 swallowed its successor whole.
        ///
        /// So a grow that will not fit is served from a fresh handle and the
        /// old one dies, which is what `realloc(3)` does and what
        /// `Unsafe.reallocateMemory` already documents -- its caller must use
        /// the returned address, and `native_unsafe_reallocate_memory_consolidated`
        /// passes this straight back to Java.
        ///
        /// # Keeping any buffer a real pointer still names
        ///
        /// `Vec::resize` reallocates when it must grow past its capacity, and a
        /// reallocation MOVES the bytes; so does relocating. Every real pointer
        /// [`Self::real_ptr`] handed out before that point would then name
        /// memory the process allocator has taken back -- an unbounded
        /// use-after-free from native code, which is the shape
        /// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` spent
        /// seven passes hunting.
        ///
        /// So a block whose pointer is OUTSTANDING does not lose its old
        /// buffer: the displaced `Vec` moves to `retired`, keyed by whatever
        /// handle the block ends up at, and stays owned by this process until
        /// that handle is freed. The stale pointer then reads stale-but-mapped
        /// bytes instead of corrupting whatever the allocator handed out next.
        /// That is a downgrade, not a cure -- the native is still using an
        /// address the Java side has moved on from -- but it bounds the blast
        /// radius to the block itself, and `STALE_ON_REALLOC` reports every
        /// occurrence.
        ///
        /// Retention is at most ONE buffer per handle: a second displacement
        /// drops the first, and `free` drops whatever is left. A resize that
        /// does NOT move the buffer retains nothing and reports nothing, which
        /// is why `STALE_ON_REALLOC` counts moves rather than resizes.
        pub(super) fn reallocate(&self, addr: i64, new_size: usize) -> i64 {
            // nio21 wave 1 L3: out of the JIT-readable table BEFORE anything
            // moves or shrinks, for good (a key is never re-published), so a
            // compiled access ordered after this resize takes the slow path.
            // Done even when the resize then fails: that only costs the block
            // its inline route.
            self.jit_unpublish(addr);
            // BEFORE anything moves, which is the only moment this is knowable.
            let outstanding = self.translated.lock().get(&addr).copied();
            let mut inner = self.inner.write();
            let Some(Arena { mut bytes, reserved }) = self.take(&mut inner, addr) else {
                // A resize of an address that names no live block: served from
                // a fresh block (`reallocate` must not conjure a block at an
                // arbitrary address -- that is the aliasing above by another
                // route), with nothing to copy or displace.
                let Some(fresh) = ArenaBuf::try_zeroed(new_size) else {
                    return 0;
                };
                return self.relocate(inner, addr, fresh, None, None, new_size);
            };

            // Within the buffer's own allocation: nothing moves, and the block
            // keeps its key in both modes (`realloc(3)` shrinking in place).
            if bytes.resize_in_place(new_size) {
                self.put(&mut inner, addr, Arena { bytes, reserved });
                return addr;
            }
            let Some(fresh) = bytes.try_resized(new_size) else {
                // `realloc(3)`'s contract on failure: the block is left
                // exactly as it was.
                self.put(&mut inner, addr, Arena { bytes, reserved });
                return 0;
            };

            // Handle mode, and the handle space still holds the new size: the
            // block keeps its handle and only its storage moves. `reserved`
            // bounds the handle space and the allocation bounds the buffer,
            // and those are different numbers -- a 60-byte block reserves 64.
            if !self.real && new_size <= reserved {
                self.put(
                    &mut inner,
                    addr,
                    Arena {
                        bytes: fresh,
                        reserved,
                    },
                );
                drop(inner);
                self.displace(addr, addr, bytes, outstanding, new_size);
                return addr;
            }
            // Past the handle space, or real-pointer mode (where the key is the
            // storage's address): a new key, and the old one dies.
            self.relocate(inner, addr, fresh, Some(bytes), outstanding, new_size)
        }

        /// File `fresh` (the resized block that was at `addr`) under a new key
        /// and retire `displaced`, its old storage: the relocating half of
        /// [`Self::reallocate`]. Releases `inner`.
        fn relocate(
            &self,
            mut inner: parking_lot::RwLockWriteGuard<'_, BTreeMap<i64, Arena>>,
            addr: i64,
            fresh: ArenaBuf,
            displaced: Option<ArenaBuf>,
            outstanding: Option<u64>,
            new_size: usize,
        ) -> i64 {
            let new_reserved = reserved_for(new_size);
            let (fresh_ptr, fresh_len) = (fresh.as_ptr() as usize, fresh.len());
            let (new_addr, slotted) = self.claim_key(new_size, new_reserved, fresh_ptr);
            self.put(
                &mut inner,
                new_addr,
                Arena {
                    bytes: fresh,
                    reserved: new_reserved,
                },
            );
            drop(inner);
            if slotted {
                self.jit_publish(new_addr, fresh_ptr, fresh_len);
            }
            if let Some(displaced) = displaced {
                self.displace(addr, new_addr, displaced, outstanding, new_size);
            }
            new_addr
        }

        /// Move the translation bookkeeping from `from` to `to` and retain the
        /// buffer `displaced` if a real pointer into it is outstanding.
        ///
        /// The new block is deliberately NOT marked translated: nobody has been
        /// given a pointer into it. The retained buffer is keyed by the block's
        /// new handle so that freeing the block frees it too.
        ///
        /// Takes no arena lock but `translated`/`retired`, so callers must have
        /// released `inner`.
        fn displace(
            &self,
            from: i64,
            to: i64,
            displaced: ArenaBuf,
            outstanding: Option<u64>,
            new_size: usize,
        ) {
            let mut translated = self.translated.lock();
            translated.remove(&from);
            drop(translated);
            let mut retired = self.retired.lock();
            let carried = retired.remove(&from);
            match outstanding {
                Some(n) => {
                    STALE_ON_REALLOC.fetch_add(1, Ordering::Relaxed);
                    RETAINED_ON_REALLOC.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        target: "cratonvm::unsafe_arena",
                        handle = format!("{from:#x}"),
                        now_at = format!("{to:#x}"),
                        translations = n,
                        old_size = displaced.len(),
                        new_size,
                        "Unsafe-arena block MOVED while native code holds a real \
                         pointer into it -- the displaced buffer is RETAINED so that \
                         pointer keeps naming memory this process owns, and is \
                         dropped when the handle is freed"
                    );
                    retired.insert(to, displaced);
                    drop(retired);
                    if let Some(prev) = carried {
                        super::notify_native_span_free(prev.as_ptr() as usize, prev.len());
                        drop(prev);
                    }
                }
                None => {
                    // Nothing is holding the displaced buffer, so it dies here.
                    // Any buffer retained by an EARLIER displacement follows the
                    // block to its new handle rather than being orphaned.
                    if let Some(prev) = carried {
                        retired.insert(to, prev);
                    }
                    drop(retired);
                    super::notify_native_span_free(
                        displaced.as_ptr() as usize,
                        displaced.len(),
                    );
                    drop(displaced);
                }
            }
        }

        pub(super) fn free(&self, addr: i64) {
            if let Some(n) = self.translated.lock().remove(&addr) {
                STALE_ON_FREE.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    target: "cratonvm::unsafe_arena",
                    handle = format!("{addr:#x}"),
                    translations = n,
                    "Unsafe-arena block is being FREED while native code holds a real pointer into it -- the `Vec` is dropped and its memory returned to the process allocator"
                );
            }
            // nio21 wave 1 L3: out of the JIT-readable table BEFORE the buffer
            // dies, so a compiled access ordered after this free misses.
            self.jit_unpublish(addr);
            let removed_arena = self.take(&mut self.inner.write(), addr);
            // Whatever a resize retained for this handle dies with it. Freeing
            // is the point at which the JNI/`Unsafe` contract says the pointer
            // is dead on any VM, so there is nothing to preserve past it --
            // and retaining past `free` would leak on an
            // allocate/translate/free loop.
            let removed_retired = self.retired.lock().remove(&addr);
            if let Some(arena) = removed_arena {
                super::notify_native_span_free(
                    arena.bytes.as_ptr() as usize,
                    arena.bytes.len(),
                );
                drop(arena);
            }
            if let Some(retired) = removed_retired {
                super::notify_native_span_free(retired.as_ptr() as usize, retired.len());
                drop(retired);
            }
        }

        pub(super) fn get_byte(&self, addr: i64) -> u8 {
            self.read::<1>(addr).map(|b| b[0]).unwrap_or(0)
        }

        // audit-round5 fix #3 (HIGH): bounds-checked getters used by the
        // get-side natives to throw IAE on out-of-arena addresses
        // (mirror of the put-side IAE behavior fixed in round-4 wave-1).
        pub(super) fn try_get_byte(&self, addr: i64) -> Option<u8> {
            self.read::<1>(addr).map(|b| b[0])
        }

        pub(super) fn put_byte(&self, addr: i64, v: u8) -> bool {
            self.write::<1>(addr, &[v])
        }

        pub(super) fn get_short(&self, addr: i64) -> i16 {
            i16::from_le_bytes(self.read::<2>(addr).unwrap_or([0; 2]))
        }

        pub(super) fn try_get_short(&self, addr: i64) -> Option<i16> {
            self.read::<2>(addr).map(i16::from_le_bytes)
        }

        pub(super) fn put_short(&self, addr: i64, v: i16) -> bool {
            self.write::<2>(addr, &v.to_le_bytes())
        }

        pub(super) fn get_int(&self, addr: i64) -> i32 {
            i32::from_le_bytes(self.read::<4>(addr).unwrap_or([0; 4]))
        }

        pub(super) fn try_get_int(&self, addr: i64) -> Option<i32> {
            self.read::<4>(addr).map(i32::from_le_bytes)
        }

        pub(super) fn put_int(&self, addr: i64, v: i32) -> bool {
            self.write::<4>(addr, &v.to_le_bytes())
        }

        pub(super) fn get_long(&self, addr: i64) -> i64 {
            i64::from_le_bytes(self.read::<8>(addr).unwrap_or([0; 8]))
        }

        pub(super) fn try_get_long(&self, addr: i64) -> Option<i64> {
            self.read::<8>(addr).map(i64::from_le_bytes)
        }

        pub(super) fn put_long(&self, addr: i64, v: i64) -> bool {
            self.write::<8>(addr, &v.to_le_bytes())
        }

        // audit-2026-05-16: find the arena whose `[base, base+size)`
        // contains `addr`. The previous impl looked up `addr` as the
        // HashMap key directly, which only succeeded at offset 0, so a
        // fallback full scan over every live arena was added to cover
        // non-zero offsets.
        //
        // PERF FIX (2026-07-25, H2 TestFileSystem.testConcurrent profiling):
        // that O(n)-in-live-arena-count scan was the single dominant cost
        // (~24% of total CPU, `perf record -F 999`, 38854 samples) of a
        // real-disk-I/O workload doing many small Unsafe reads/writes
        // through direct ByteBuffers backed by this arena store — every
        // non-zero-offset byte/short/int/long access re-scanned every live
        // arena. `next_addr` in `allocate()` only ever increases, so live
        // arenas are always disjoint, non-overlapping `[base, base+len)`
        // ranges keyed by their (ordered) base address — exactly what a
        // `BTreeMap` range query answers in O(log n): the arena containing
        // `addr`, if any, is the one with the largest base <= addr.
        fn locate(inner: &BTreeMap<i64, Arena>, addr: i64) -> Option<(i64, usize)> {
            let (&base, arena) = inner.range(..=addr).next_back()?;
            let offset = (addr - base) as u64;
            if offset < arena.bytes.len() as u64 {
                Some((base, offset as usize))
            } else {
                None
            }
        }

        fn read<const N: usize>(&self, addr: i64) -> Option<[u8; N]> {
            let inner = self.inner.read();
            let (base, offset) = Self::locate(&inner, addr)?;
            let arena = inner.get(&base)?;
            let end = offset.checked_add(N)?;
            if end > arena.bytes.len() {
                return None;
            }
            let mut buf = [0u8; N];
            buf.copy_from_slice(&arena.bytes[offset..end]);
            Some(buf)
        }

        /// audit-2026-05-16: returns `true` on success, `false` on
        /// out-of-bounds (do NOT silently grow the arena).
        fn write<const N: usize>(&self, addr: i64, data: &[u8; N]) -> bool {
            let mut inner = self.inner.write();
            let (base, offset) = match Self::locate(&inner, addr) {
                Some(v) => v,
                None => return false,
            };
            let arena = match inner.get_mut(&base) {
                Some(a) => a,
                None => return false,
            };
            let end = match offset.checked_add(N) {
                Some(e) => e,
                None => return false,
            };
            if end > arena.bytes.len() {
                return false;
            }
            arena.bytes[offset..end].copy_from_slice(data);
            true
        }

        /// True if `addr` falls inside any live arena block. Exact membership
        /// (not a range heuristic) — used by native I/O to decide whether a
        /// pointer is an Unsafe-arena handle vs a real OS address.
        ///
        /// R1: every handle carries [`ARENA_TAG`], which no real pointer can
        /// have set, so an untagged `addr` (a real OS pointer) is rejected
        /// up front without taking the lock — this is what makes the
        /// arena/real-pointer classification *provably* unambiguous rather
        /// than relying on the two ranges happening not to overlap. A tagged
        /// `addr` still goes through the exact live-block membership check, so
        /// a freed handle correctly reports `false`.
        pub(super) fn contains(&self, addr: i64) -> bool {
            if !self.may_own(addr) {
                return false;
            }
            let inner = self.inner.read();
            Self::locate(&inner, addr).is_some()
        }

        /// Real-pointer mode only: `addr` is inside a live block with at least
        /// `want` bytes from it to the block's end. One read-locked walk: the
        /// address IS the byte's storage there, so a caller that dereferences
        /// it needs no [`Self::real_ptr`] (whose write lock and `translated`
        /// count audit handles turned into pointers; there are none here).
        pub(super) fn real_mode_covers(&self, addr: i64, want: usize) -> bool {
            if !self.real || !self.may_own(addr) {
                return false;
            }
            let inner = self.inner.read();
            let Some((&base, arena)) = inner.range(..=addr).next_back() else {
                return false;
            };
            let offset = (addr - base) as u64;
            offset < arena.bytes.len() as u64 && arena.bytes.len() as u64 - offset >= want as u64
        }

        /// The REAL, dereferenceable address of the byte that arena handle
        /// `addr` names, plus how many bytes remain in its block.
        ///
        /// Every other accessor on this store copies, because every other
        /// caller is Rust and can. JNI cannot: a native library that calls
        /// `GetDirectBufferAddress` is handed a `void*` and dereferences it
        /// itself, so a handle — whose whole point is that it is NOT a real
        /// pointer — is a guaranteed SIGSEGV the moment it reaches C. Handing
        /// out the backing pointer is what makes that call answerable at all;
        /// see `jni_get_direct_buffer_address`.
        ///
        /// The pointer is into the block's buffer, so writes through it land
        /// in the arena with no copy-back, which is the aliasing a direct
        /// buffer is supposed to have. It stays valid until that block is
        /// freed or reallocated — `Vec`'s heap buffer does not move when the
        /// `BTreeMap` rebalances around it, only when `reallocate` resizes it.
        /// That is the same lifetime a real `malloc`'d direct buffer gives a
        /// native under HotSpot, and it is the caller's (the JNI spec's)
        /// contract not to outlive it.
        pub(super) fn real_ptr(&self, addr: i64) -> Option<(*mut u8, usize)> {
            if !self.may_own(addr) {
                return None;
            }
            let mut inner = self.inner.write();
            let (base, offset) = Self::locate(&inner, addr)?;
            let arena = inner.get_mut(&base)?;
            let remaining = arena.bytes.len().checked_sub(offset)?;
            // SAFETY: `locate` established `offset < arena.bytes.len()`, so the
            // offset is in bounds of the block's own allocation.
            let ptr = unsafe { arena.bytes.as_mut_ptr().add(offset) };
            // A RAW POINTER INTO A BLOCK'S BUFFER IS NOW IN NATIVE HANDS, and both
            // call sites (`jni_long_arg_bits`, `direct_buffer_native_address`)
            // discard `remaining` -- so the callee has a pointer and no bound.
            // Record the block so `reallocate`/`free` can say whether anyone was
            // still holding one when the buffer moved or died. See
            // `unsafe_arena_translation_stats`.
            TRANSLATIONS.fetch_add(1, Ordering::Relaxed);
            *self.translated.lock().entry(base).or_insert(0) += 1;
            Some((ptr, remaining))
        }

        /// Copy `out.len()` bytes OUT of the arena (arena → `out`). Returns
        /// false if the `[addr, addr+len)` range is not fully inside one live
        /// arena block.
        pub(super) fn copy_out(&self, addr: i64, out: &mut [u8]) -> bool {
            // A ZERO-LENGTH copy reads no bytes, so it cannot be out of
            // bounds -- and it is legal at exactly one past the end of the
            // block. `locate` is an EXCLUSIVE range test, so without this
            // it reports "not in any live block" for `base + len` and the
            // caller turns that into a spurious exception. Every direct
            // `ByteBuffer` bulk `get`/`put` of an empty range while
            // positioned at the buffer's limit lands here -- netty's
            // `AbstractByteBufTest.writerIndexBoundaryCheck4` does exactly
            // that (`writeBytes(ByteBuffer.wrap(EMPTY_BYTES))` on a full
            // direct buffer) and threw `IllegalStateException`.
            if out.is_empty() {
                return true;
            }
            let inner = self.inner.read();
            // ONE tree walk: the block with the largest base <= addr is the
            // block and the walk already holds it (`locate` + `get` walked twice).
            let Some((&base, arena)) = inner.range(..=addr).next_back() else {
                return false;
            };
            let offset = (addr - base) as u64;
            if offset >= arena.bytes.len() as u64 {
                return false;
            }
            let offset = offset as usize;
            let end = match offset.checked_add(out.len()) {
                Some(e) => e,
                None => return false,
            };
            if end > arena.bytes.len() {
                return false;
            }
            out.copy_from_slice(&arena.bytes[offset..end]);
            true
        }

        /// Copy `data` INTO the arena (`data` → arena). Symmetric to
        /// [`Self::copy_out`].
        pub(super) fn copy_in(&self, addr: i64, data: &[u8]) -> bool {
            // Zero-length write: see `copy_out` above. Writes nothing, so it
            // is in bounds anywhere, including one past the end of a block.
            if data.is_empty() {
                return true;
            }
            let mut inner = self.inner.write();
            // ONE tree walk, as in `copy_out`.
            let Some((&base, arena)) = inner.range_mut(..=addr).next_back() else {
                return false;
            };
            let offset = (addr - base) as u64;
            if offset >= arena.bytes.len() as u64 {
                return false;
            }
            let offset = offset as usize;
            let end = match offset.checked_add(data.len()) {
                Some(e) => e,
                None => return false,
            };
            if end > arena.bytes.len() {
                return false;
            }
            arena.bytes[offset..end].copy_from_slice(data);
            true
        }
    }

    pub(super) fn store() -> &'static ArenaStore {
        use std::sync::OnceLock;
        #[cfg(any(test, feature = "arena-test-hooks"))]
        if let Some(handles) = HANDLE_STORE_OVERRIDE.with(|c| c.get()) {
            return handles;
        }
        static S: OnceLock<ArenaStore> = OnceLock::new();
        S.get_or_init(ArenaStore::new)
    }

    #[cfg(any(test, feature = "arena-test-hooks"))]
    thread_local! {
        static HANDLE_STORE_OVERRIDE: std::cell::Cell<Option<&'static ArenaStore>> =
            const { std::cell::Cell::new(None) };
    }

    /// Test-only: while the guard lives, this thread's arena calls are served
    /// by one shared store in handle mode (`CRATONVM_UNSAFE_REAL_POINTERS=0`).
    /// The process store follows the default, real pointers since ADDR21-1's
    /// flip, so a test of the handle mode's own answers (the tag, the JIT
    /// table, the handle <-> real maps, the use-after-free refusal, which in
    /// real mode is a write into freed memory) takes this first. Tests that
    /// touch the shared store hold `crate::arena_test_lock()` as before.
    #[cfg(any(test, feature = "arena-test-hooks"))]
    #[must_use]
    pub struct HandleModeStore(Option<&'static ArenaStore>);

    #[cfg(any(test, feature = "arena-test-hooks"))]
    pub fn handle_mode_store_for_test() -> HandleModeStore {
        use std::sync::OnceLock;
        static H: OnceLock<ArenaStore> = OnceLock::new();
        let handles = H.get_or_init(|| ArenaStore::with_mode(false));
        HandleModeStore(HANDLE_STORE_OVERRIDE.with(|c| c.replace(Some(handles))))
    }

    #[cfg(any(test, feature = "arena-test-hooks"))]
    impl Drop for HandleModeStore {
        fn drop(&mut self) {
            HANDLE_STORE_OVERRIDE.with(|c| c.set(self.0));
        }
    }

    // jr20 wave 9 / gcE: `InvokedCleaners` (a process-wide set of `ByteBuffer`
    // heap addresses, never swept or remapped, whose answer
    // `native_unsafe_invoke_cleaner` ignored) is deleted: a write-only leak
    // (gch-h1ak-raw-address-keyed-audit-RETIRED-UNFIXED-20261018 item 6).
}

#[cfg(any(test, feature = "arena-test-hooks"))]
pub use unsafe_arena::{handle_mode_store_for_test, HandleModeStore};
#[cfg(test)]
mod r23w5_arena_jit_resolve_tests;
#[cfg(test)]
pub fn unsafe_arena_block_is_translated(addr: i64) -> bool {
    unsafe_arena::store().block_is_translated(addr)
}

#[cfg(test)]
pub fn unsafe_arena_block_is_retired(addr: i64) -> bool {
    unsafe_arena::store().block_is_retired(addr)
}

/// The arena handle → real pointer translation that `GetDirectBufferAddress`
/// depends on. Written against the *observable* the JNI boundary needs: a
/// pointer that can be dereferenced from C, that aliases the arena rather than
/// copying it, and that refuses a handle no live block owns.
/// Serializes the tests whose assertions depend on the process-wide arena's
/// layout (what lies at a block's end, whether a grow can reach the next
/// block). Run in parallel, one test's block could start exactly at another's
/// `end` and make its "a write past the end is refused" check fail: seen 2
/// runs in 10 at the gc-common w8 verification.
#[cfg(test)]
fn arena_layout_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod arena_membership_tests;

#[cfg(test)]
mod unsafe_arena_real_pointer_tests;

#[cfg(test)]
mod unsafe_arena_handle_of_real_tests;

#[cfg(test)]
mod unsafe_arena_real_ptr_tests;

/// nio21 wave 1 L3: the JIT-readable arena table (the section comment above
/// `unsafe_arena::JIT_SLOT_SHIFT`). `jit_resolve` is the optimizing tier's
/// inline resolve step for step, so these pin what compiled code answers.
#[cfg(test)]
mod nio21_arena_jit_table_tests;

#[cfg(test)]
mod unsafe_static_field_offset_tests;

#[cfg(test)]
mod arena_zero_length_copy_tests;

#[cfg(test)]
mod unsafe_unaligned_read_tests;

/// Round nio20 wave 2, lane L4 (proposal L2-4): the int/long volatile and CAS
/// natives over a primitive array of another element type act on the raw
/// little-endian bytes, as the plain `*_mb` accessors already did.
#[cfg(test)]
mod unsafe_wide_raw_tests;

#[cfg(test)]
mod allocate_memory_delegation_tests;

/// gc-common w7-a (`common-w6a-unsafe-synthetic-field-store-is-shared-across-vms`):
/// the two `ObjectRef`-holding Unsafe side stores are keyed by VM, and their GC
/// hooks root, remap and forget one VM's rows only.
#[cfg(test)]
mod unsafe_side_store_vm_isolation_tests;

/// nio21 wave 7 L3 (`docs/feature-designs/native-dispatch-architecture.md`):
/// the arm order. A 600 MiB array is impractical, so the witness is the order
/// itself: a band offset against an ARRAY base goes to the array arm (where a
/// small array's out-of-range answer is "no heap touch"), never the side store.
#[cfg(test)]
mod nio21_w7_array_before_synthetic_tests;

/// gci i17/vt (`CRATONVM_VT_PINNED_PARK_COMPENSATE`, opt-in): the live
/// `Unsafe.park(ZJ)V` compensates the scheduler for a pinned park of a mounted
/// JDK virtual thread, and only for one.
#[cfg(test)]
mod gci_i17vt_pinned_park_compensation_tests;

/// gci i21/qh: `Unsafe.park`'s timeout arithmetic against HotSpot 25's
/// `Parker::park` (`unsafe_park_timeout`).
#[cfg(test)]
mod gci_i21qh_park_timeout_tests;

/// gci i22/rv (`gci-i22rv-i21-default-path-review-findings-FIXED-20261016`
/// findings 3-4): an elapsed deadline never takes the virtual-thread yield
/// (whose zero wake-up means "untimed"), and an absolute `0` posts HotSpot's
/// untimed event fields.
#[cfg(test)]
mod gci_i22rv_park_tests;

/// gci i23/pk (`gci-i22rv-i21-default-path-review-findings-FIXED-20261016`
/// finding 6): a negative relative park carries its raw time to the
/// `jdk.ThreadPark` event (finding 5's pin is in `vm`'s
/// `virtual_threads.rs` tests: the change is the VM's park).
#[cfg(test)]
mod gci_i23pk_park_tests;

/// JIT round 20 wave 8c (lane huntDisp): byte field offsets
/// (`CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`), both arms of the switch.
#[cfg(test)]
mod byte_field_offset_tests;

/// gci cl48 (page
/// `gci-i47w-jw1-compatible-child-first-loader-statics-read-back-wrong-FIXED-20261017`):
/// the child-first copies of one class share the static offset minted for
/// their name, and each copy's static is still its own -- through its mirror
/// base, and through a handle that carries the base.
#[cfg(test)]
mod gci_i48_cl48_tests;

#[cfg(test)]
mod jr21_atomics_lock_arm_tests;

/// JIT round 21, lane unsafe (page `i94-orch-dev-jit-round-20-regresses-four-
/// interpreter-probes-20261127.md`): narrow `Unsafe` reads answer the access's
/// own bytes, re-extended as its type, under both offset encodings, and the
/// char-field short atomics find their field under byte offsets.
#[cfg(test)]
mod jr21_unsafe_narrow_read_tests;

/// JIT round 21, lane unsafefix (page
/// `docs/feature-designs/jit-ir-optimizations.md`,
/// probe `R21UnsafeNarrowRows`): a narrow `Unsafe` store AT a static's own
/// offset (`staticFieldBase` mirror base, `k = 0`) replaces only the static's
/// low bytes, or re-reads a same-width store as the static's type, in both
/// offset encodings; the narrow compare-and-exchange witness stays the
/// access's Java value.
#[cfg(test)]
mod jr21_unsafefix_static_narrow_put_tests;

mod unsafe_offset_is_heap_slot;
#[allow(unused_imports)]
pub use unsafe_offset_is_heap_slot::*;
#[allow(unused_imports)]
use unsafe_offset_is_heap_slot::*;
mod native_unsafe_get_long_volatile;
#[allow(unused_imports)]
pub use native_unsafe_get_long_volatile::*;
#[allow(unused_imports)]
use native_unsafe_get_long_volatile::*;
mod unsafe_array_read_bytes;
#[allow(unused_imports)]
pub use unsafe_array_read_bytes::*;
#[allow(unused_imports)]
use unsafe_array_read_bytes::*;
