#[allow(unused_imports)]
use super::*;

/// Read `n` bytes from a primitive array starting at the raw byte offset
/// `byte_off` (an `Unsafe` offset including `ABASE` = 16). Each element is
/// decomposed into native (little-endian) byte order. Returns `None` if the
/// object is not a primitive array or the range is out of bounds.
pub fn unsafe_array_read_bytes(
    ctx: &dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    byte_off: usize,
    n: usize,
) -> Option<Vec<u8>> {
    if ctx.heap_kind_of(arr) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let et = ctx.heap_element_type_of(arr);
    if et == cratonvm_types::ArrayElementType::Reference {
        return None;
    }
    const ABASE: usize = 16;
    let start = byte_off.checked_sub(ABASE)?;
    let width = array_elem_byte_width(et);
    let len = ctx.array_length(arr);
    let total = len.checked_mul(width)?;
    if start + n > total {
        return None;
    }
    // Perf fast path: for byte/boolean arrays the element width is 1, so the
    // raw byte offset `start` is exactly the element index and each element is
    // a single byte in native (and Java) order. Route through the bulk
    // `read_byte_array_into` NativeContext intrinsic, which the VM overrides
    // with a single `ptr::copy_nonoverlapping`, avoiding a per-element
    // `get_array_element` (Value box + dynamic dispatch) on the hot path.
    // Bounds are already validated above (`start + n <= total == len`, and
    // `width == 1` so `start <= len`); the intrinsic clamps independently as
    // well, so this cannot read past the array.
    if matches!(
        et,
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
    ) {
        debug_assert_eq!(width, 1);
        let mut out = vec![0u8; n];
        let copied = ctx.read_byte_array_into(arr, start, &mut out);
        // `start + n <= len` guarantees the intrinsic copies exactly `n`.
        debug_assert_eq!(copied, n);
        if copied != n {
            return None;
        }
        return Some(out);
    }
    // SBR3 lane ffmperf: every other primitive width is ALSO a contiguous
    // native-endian payload, so the whole range is one `memcpy` through the
    // byte-level bulk read (`MemorySegment.copy(seg, JAVA_INT, .., int[], ..)`
    // and `toArray` reach here through the real `Unsafe.copyMemory`; the loop
    // below paid a `Value` round trip per element, 30x the carrier's bulk
    // copy). A context with no bulk path answers 0 and takes the loop.
    {
        let mut out = vec![0u8; n];
        if n == 0 || ctx.read_primitive_array_bytes(arr, start, &mut out) == n {
            return Some(out);
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut produced = 0usize;
    let mut elem_idx = start / width;
    let mut byte_in_elem = start % width;
    while produced < n {
        let v = ctx.get_array_element(arr, elem_idx);
        let raw: u64 = match et {
            cratonvm_types::ArrayElementType::Float => match v {
                Value::Float(f) => f.to_bits() as u64,
                Value::Int(i) => i as u32 as u64,
                _ => 0,
            },
            cratonvm_types::ArrayElementType::Double => match v {
                Value::Long(l) => l as u64,
                Value::Double(d) => d.to_bits(),
                _ => v.as_long().unwrap_or(0) as u64,
            },
            cratonvm_types::ArrayElementType::Long => match v {
                Value::Long(l) => l as u64,
                _ => v.as_long().unwrap_or(0) as u64,
            },
            _ => (v.as_int().unwrap_or(0) as i64) as u64,        };
        let elem_bytes = raw.to_le_bytes();
        while byte_in_elem < width && produced < n {
            out.push(elem_bytes[byte_in_elem]);
            byte_in_elem += 1;
            produced += 1;
        }
        byte_in_elem = 0;
        elem_idx += 1;
    }
    Some(out)
}

/// Write a byte stream into a primitive array starting at the raw byte
/// offset `byte_off`. Each destination element is reassembled from native
/// (little-endian) bytes. Partial-element writes preserve the surrounding
/// bytes of the touched element. Returns `false` if `arr` is not a
/// primitive array or the range is out of bounds.
pub fn unsafe_array_write_bytes(
    ctx: &mut dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    byte_off: usize,
    bytes: &[u8],
) -> bool {
    if ctx.heap_kind_of(arr) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let et = ctx.heap_element_type_of(arr);
    if et == cratonvm_types::ArrayElementType::Reference {
        return false;
    }
    const ABASE: usize = 16;
    let start = match byte_off.checked_sub(ABASE) {
        Some(s) => s,
        None => return false,
    };
    let width = array_elem_byte_width(et);
    let len = ctx.array_length(arr);
    let total = match len.checked_mul(width) {
        Some(t) => t,
        None => return false,
    };
    if start + bytes.len() > total {
        return false;
    }
    // Perf fast path: for byte/boolean arrays width is 1, so `start` is the
    // element index and there are no partial-element (read-modify-write)
    // concerns — every touched byte is a whole element. Route through the bulk
    // `write_byte_array_from` NativeContext intrinsic (VM override =
    // `ptr::copy_nonoverlapping`), avoiding the per-element get/set Value
    // boxing + dynamic dispatch. Bounds are already validated above
    // (`start + bytes.len() <= total == len`, `width == 1`); the intrinsic
    // re-checks `dst_off + src.len() <= array_length` and returns `false`
    // without writing anything on mismatch, preserving the bounds contract.
    if matches!(
        et,
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
    ) {
        debug_assert_eq!(width, 1);
        return ctx.write_byte_array_from(arr, start, bytes);
    }
    // SBR3 lane ffmperf: the bulk twin of `unsafe_array_read_bytes`'s: one
    // `memcpy` into the payload (partial elements keep their other bytes
    // trivially), the read-modify-write loop below only for a context with no
    // bulk path.
    if bytes.is_empty() || ctx.write_primitive_array_bytes(arr, start, bytes) {
        return true;
    }
    let mut consumed = 0usize;
    let mut elem_idx = start / width;
    let mut byte_in_elem = start % width;
    while consumed < bytes.len() {
        // Read-modify-write the touched element so partial writes are safe.
        let cur = ctx.get_array_element(arr, elem_idx);
        let mut elem_bytes = match et {
            cratonvm_types::ArrayElementType::Double => match cur {
                Value::Long(l) => (l as u64).to_le_bytes(),
                Value::Double(d) => d.to_bits().to_le_bytes(),
                _ => 0u64.to_le_bytes(),
            },
            cratonvm_types::ArrayElementType::Long => match cur {
                Value::Long(l) => (l as u64).to_le_bytes(),
                _ => 0u64.to_le_bytes(),
            },
            // A `float[]` element is a `Value::Float`; `as_int` would read it
            // as 0 and a partial store would zero the element's other bytes
            // (nio20 wave 2 L4, found reviewing the raw-store fallback).
            cratonvm_types::ArrayElementType::Float => match cur {
                Value::Float(f) => u64::from(f.to_bits()).to_le_bytes(),
                Value::Int(i) => u64::from(i as u32).to_le_bytes(),
                _ => 0u64.to_le_bytes(),
            },
            _ => ((cur.as_int().unwrap_or(0) as i64) as u64).to_le_bytes(),        };
        while byte_in_elem < width && consumed < bytes.len() {
            elem_bytes[byte_in_elem] = bytes[consumed];
            byte_in_elem += 1;
            consumed += 1;
        }
        let raw = u64::from_le_bytes(elem_bytes);
        let new_val = match et {
            cratonvm_types::ArrayElementType::Boolean | cratonvm_types::ArrayElementType::Byte => {
                Value::Int(raw as u8 as i8 as i32)
            }
            cratonvm_types::ArrayElementType::Char => Value::Int(raw as u16 as i32),
            cratonvm_types::ArrayElementType::Short => Value::Int(raw as u16 as i16 as i32),
            cratonvm_types::ArrayElementType::Int => Value::Int(raw as u32 as i32),
            cratonvm_types::ArrayElementType::Float => Value::Float(f32::from_bits(raw as u32)),
            cratonvm_types::ArrayElementType::Long => Value::Long(raw as i64),
            // A `double[]` element is a `Value::Double`, NOT the `Value::Long`
            // this arm used to share with `long[]`. MEASURED 2026-09-20
            // (lane 4 wave 18, `L4TypedBufferSweep`, linux/x86_64):
            // `ByteBuffer.allocate(32).asDoubleBuffer()` filled with
            // {1.5, 2.5, 3.5, 4.5}, then `get(out, 1, 2)`, answered
            // `[0.0, 0.0, 0.0, 0.0]` where HotSpot answers
            // `[0.0, 2.5, 3.5, 0.0]` -- and only for `double`, because the
            // `float` arm below already built a `Value::Float` and the
            // integral widths are `Value::Int`/`Value::Long` anyway. The
            // store of a `Long` into a `double[]` does not land, so the
            // elements keep their zero: a SILENT WRONG ANSWER on every
            // `Unsafe.copyMemory` whose destination is a `double[]` --
            // `DoubleBuffer.get(double[],int,int)` above the
            // `JNI_COPY_TO_ARRAY_THRESHOLD`, `MemorySegment.toArray(JAVA_DOUBLE)`
            // and the FFM bulk paths among them. It was invisible while
            // `DoubleBuffer.get([DII)` was a native; wave 17 retired that row
            // and the probe found it on the first Linux run.
            cratonvm_types::ArrayElementType::Double => Value::Double(f64::from_bits(raw)),
            cratonvm_types::ArrayElementType::Reference => unreachable!(),
        };
        ctx.set_array_element(arr, elem_idx, new_val);
        byte_in_elem = 0;
        elem_idx += 1;
    }
    true
}

pub fn native_unsafe_copy_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, srcObj, srcOffset, destObj, destOffset, bytes]
    let src_obj = unsafe_obj(args, 1);
    let src_offset = unsafe_offset(args, 2);
    let dest_obj = unsafe_obj(args, 3);
    let dest_offset = unsafe_offset(args, 4);
    let bytes = match args.get(5) {
        Some(Value::Long(b)) => *b as usize,
        Some(Value::Int(b)) => *b as usize,
        _ => 0,
    };
    if bytes == 0 {
        return Ok(None);
    }
    if bytes > MAX_UNSAFE_COPY_SIZE {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: format!(
                "Unsafe.copyMemory size {} exceeds maximum of {} bytes",
                bytes, MAX_UNSAFE_COPY_SIZE
            ),
        }
        .into());
    }
    if let (Some(src), Some(dst)) = (src_obj, dest_obj) {
        // Primitive-array → primitive-array: `copyMemory` is a raw byte copy
        // (the JDK's NIO buffer-view classes — ByteBufferAsCharBuffer,
        // ICUBinary.getChars/getInts — rely on this to move trie data from a
        // `byte[]`-backed buffer into a `char[]`/`int[]`). Decompose each
        // source element to bytes and reassemble into the destination
        // element type, honoring per-element byte width and offsets.
        let src_is_array = ctx.heap_kind_of(src) == cratonvm_types::ObjectKind::Array
            && ctx.heap_element_type_of(src) != cratonvm_types::ArrayElementType::Reference;
        let dst_is_array = ctx.heap_kind_of(dst) == cratonvm_types::ObjectKind::Array
            && ctx.heap_element_type_of(dst) != cratonvm_types::ArrayElementType::Reference;
        if src_is_array || dst_is_array {
            // At least one side is a primitive array: the only correct copy is
            // the byte-exact array path, which is bounds-checked. If it cannot
            // complete (out-of-bounds, or one side is not an array) we must NOT
            // fall through to the unbounded slot loop — that would let an
            // attacker-controlled `bytes`/offset drive `set_field` past the
            // array's storage (finding M3). Throw IndexOutOfBoundsException
            // (HotSpot's `Unsafe.copyMemory` behaviour on a bad range).
            if src_is_array && dst_is_array {
                if let Some(buf) = unsafe_array_read_bytes(ctx, src, src_offset, bytes) {
                    if unsafe_array_write_bytes(ctx, dst, dest_offset, &buf) {
                        return Ok(None);
                    }
                }
            }
            return Err(
                cratonvm_types::error::RuntimeError::aioobe_index_only(dest_offset as i32).into(),
            );
        }
        // JIT round 20 wave 8c: under byte field offsets both sides are
        // field BYTES (a byte no primitive field holds reads 0 / is not
        // written).
        if ctx.unsafe_byte_field_offsets() {
            for i in 0..bytes {
                let b = unsafe_field_byte_at(&*ctx, src, src_offset.wrapping_add(i)).unwrap_or(0);
                let _ = unsafe_field_set_byte_at(ctx, dst, dest_offset.wrapping_add(i), b);
            }
            return Ok(None);
        }
        // Legacy slot-by-slot copy for object-field (non-array) targets only.
        // `bytes` is capped above by MAX_UNSAFE_COPY_SIZE.
        for i in 0..bytes {
            let val = ctx.get_field(src, src_offset + i);
            ctx.set_field(dst, dest_offset + i, val);
        }
    }
    Ok(None)
}

pub fn native_unsafe_set_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset, bytes, value]
    let obj = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let offset = match args.get(2) {
        Some(Value::Long(v)) => *v as usize,
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let bytes = match args.get(3) {
        Some(Value::Long(v)) => *v as usize,
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let value = match args.get(4) {
        Some(Value::Int(v)) => *v as u8,
        _ => 0,
    };
    if bytes == 0 {
        return Ok(None);
    }
    if bytes > MAX_UNSAFE_COPY_SIZE {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: format!(
                "Unsafe.setMemory size {} exceeds maximum of {} bytes",
                bytes, MAX_UNSAFE_COPY_SIZE
            ),
        }
        .into());
    }
    if let Some(obj_ref) = obj {
        // Primitive-array target: fill `bytes` raw bytes with `value`. This is
        // the only correct path for an array and it is bounds-checked. If it
        // cannot complete (out-of-bounds) we must NOT fall through to the
        // unbounded slot loop — doing so would let attacker-controlled
        // `bytes`/`offset` drive `set_field` past the array storage, and for
        // non-array objects the loop ran unconditionally (finding M3).
        if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array
            && ctx.heap_element_type_of(obj_ref) != cratonvm_types::ArrayElementType::Reference
        {
            let fill = vec![value; bytes];
            if unsafe_array_write_bytes(ctx, obj_ref, offset, &fill) {
                return Ok(None);
            }
            return Err(
                cratonvm_types::error::RuntimeError::aioobe_index_only(offset as i32).into(),
            );
        }
        // JIT round 20 wave 8c: under byte field offsets `bytes` counts the
        // fields' BYTES.
        if ctx.unsafe_byte_field_offsets() {
            if unsafe_field_fill_bytes(ctx, obj_ref, offset, bytes, value) {
                return Ok(None);
            }
            return Err(
                cratonvm_types::error::RuntimeError::aioobe_index_only(offset as i32).into(),
            );
        }
        // Object-field (non-array) target: bounded slot fill. `bytes` is capped
        // above by MAX_UNSAFE_COPY_SIZE.
        let fill_value = Value::Int(value as i32);
        for i in 0..bytes {
            ctx.set_field(obj_ref, offset + i, fill_value);
        }
    }
    Ok(None)
}

/// `Unsafe.allocateMemory(long)` — delegates to the arena-backed
/// [`unsafe_natives::native_unsafe_allocate_memory_consolidated`].
///
/// This used to allocate a managed `Object[]` and return its HEAP ADDRESS as
/// the "off-heap" pointer. The registration was always overwritten by
/// `register_consolidated_off_heap_store` (both boot paths call it later), but
/// the body was one registration-order change away from putting an unminted
/// heap address into a Java `long` — a value the operand-stack root scan would
/// root and the post-GC remap would never rewrite
/// (`docs/feature-designs/native-dispatch-architecture.md`).
/// The heap block was also unreadable by the raw get/put natives, which only
/// address the arena. Delegating makes every registration of this triple mean
/// the same thing.
pub fn native_unsafe_allocate_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unsafe_natives::native_unsafe_allocate_memory_consolidated(ctx, args)
}

/// `Unsafe.freeMemory(long)` / `freeMemory0(long)` — release an off-heap block.
///
/// This was `native_noop_with_this`, i.e. every block handed out by
/// `allocateMemory` stayed live for the lifetime of the process. All off-heap
/// addressing in this VM is consolidated onto the arena store (see the V5
/// SECURITY FIX note in `unsafe_natives.rs`): `unsafe_arena_free` removes the
/// whole `[base, base+len)` entry from the store's `BTreeMap`, and that map IS
/// the bookkeeping table — dropping the entry both releases the bytes and
/// makes every later access to the address fail the liveness check, which is
/// what turns a use-after-free into an `IllegalArgumentException` instead of a
/// silent read of recycled memory.
///
/// `freeMemory(0)` is a no-op by contract (`Unsafe` mirrors C `free(NULL)`),
/// and an address the arena never handed out — e.g. a real
/// `ByteBuffer.allocateDirect` pointer, which does not carry the arena tag —
/// is left alone rather than reported as an error, matching the consolidated
/// `unsafe_natives::native_unsafe_free_memory` handler.
pub fn native_unsafe_free_memory_ext(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] is the `Unsafe` receiver; args[1] is the address.
    let addr = match args.get(1) {
        Some(Value::Long(a)) => *a,
        Some(Value::Int(a)) => *a as i64,
        _ => return Ok(None),
    };
    if addr == 0 {
        return Ok(None);
    }
    unsafe_arena_free(addr);
    Ok(None)
}

/// `Unsafe.monitorEnter(Object)` — acquire the argument's monitor.
///
/// This was `native_noop_with_this`, which meant a caller that locks through
/// `Unsafe` rather than the `monitorenter` bytecode got NO mutual exclusion:
/// every thread "acquired" the monitor simultaneously and the critical section
/// ran concurrently. CratonVM already has a working monitor implementation —
/// `NativeThreadAccess::monitor_enter`/`monitor_exit`, the same pair that backs
/// `java/lang/Object.wait`/`notify`/`notifyAll` — so route to it.
///
/// A null argument is an NPE in HotSpot; `obj_arg` produces exactly that.
/// Plain `monitor_enter` (not `monitor_enter_gc_safe`) is correct here: the
/// receiver is not touched after the acquire, so a relocation during a blocked
/// enter cannot leave a stale reference behind.
pub fn native_unsafe_monitor_enter(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] is the `Unsafe` receiver; args[1] is the object to lock.
    let obj = obj_arg(args, 1)?;
    ctx.monitor_enter(obj);
    Ok(None)
}

/// `Unsafe.monitorExit(Object)` — release the argument's monitor.
/// Counterpart of [`native_unsafe_monitor_enter`]; see its doc for why the
/// previous no-op was a correctness bug.
pub fn native_unsafe_monitor_exit(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = obj_arg(args, 1)?;
    ctx.monitor_exit(obj);
    Ok(None)
}

/// `Unsafe.reallocateMemory(long, long)` — delegates to the arena-backed
/// [`unsafe_natives::native_unsafe_reallocate_memory_consolidated`], for the
/// reason given on [`native_unsafe_allocate_memory`] (this body also returned
/// a heap `Object[]` address, and dropped the old block's contents).
pub fn native_unsafe_allocate_memory_realloc(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unsafe_natives::native_unsafe_reallocate_memory_consolidated(ctx, args)
}


/// Allocate an arena block and return its tagged handle.
///
/// `pub` alongside [`unsafe_arena_real_ptr`] and [`unsafe_arena_copy_in`]:
/// production code reaches the arena through the `Unsafe` natives, but the JNI
/// boundary in `cratonvm-vm` needs a live tagged handle to test that
/// `GetDirectBufferAddress` really does bound the pointer it publishes by the
/// capacity `GetDirectBufferCapacity` advertises. That guard is memory safety,
/// and a guard whose wiring is only tested one crate away from where it is
/// wired is a guard nobody has tested.
pub fn unsafe_arena_allocate(size: usize) -> i64 {
    unsafe_arena::store().allocate(size)
}

/// nio21 wave 1 L3: the layout of the JIT-readable arena table (see the
/// section comment above `unsafe_arena::JIT_SLOT_SHIFT`). The optimizing tier
/// bakes the same numbers (`cratonvm_jit::ir_lower::SCOPED_ARENA_*`); the VM's
/// `jit_well_known_class_id` arm asserts at compile time that they agree.
pub const UNSAFE_ARENA_JIT_SLOT_SHIFT: u32 = unsafe_arena::JIT_SLOT_SHIFT;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_INDEX_BITS: u32 = unsafe_arena::JIT_INDEX_BITS;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_ENTRY_SHIFT: u32 = unsafe_arena::JIT_ENTRY_SHIFT;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_KEY_OFFSET: usize = unsafe_arena::JIT_SLOT_KEY_OFFSET;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_LEN_OFFSET: usize = unsafe_arena::JIT_SLOT_LEN_OFFSET;
/// See [`UNSAFE_ARENA_JIT_SLOT_SHIFT`].
pub const UNSAFE_ARENA_JIT_PTR_OFFSET: usize = unsafe_arena::JIT_SLOT_PTR_OFFSET;

/// The address of the JIT-readable arena table, or `0` when
/// `CRATONVM_UNSAFE_ARENA_JIT_TABLE=0`. Fixed for the life of the process once
/// non-zero (the store is a never-dropped `OnceLock`, the table is never
/// resized). Asked by the VM's `jit_well_known_class_id` for the optimizing
/// tier's scoped fold.
pub fn unsafe_arena_jit_table_addr() -> usize {
    unsafe_arena::store().jit_table_addr()
}

/// JIT r23 wave 4/5 (lane L8, `CRATONVM_FFM_ARENA_JIT_RESOLVE`): the real
/// address of the `width` bytes at arena handle `addr`, read from the
/// JIT-readable table with no lock (key, length, pointer, key again -- the
/// optimizing tier's scoped-fold resolve, step for step), or `None` for a
/// legacy or untagged address, a freed or resized block, an access that does
/// not fit, or the table off. The caller keeps the locked
/// [`unsafe_arena_copy_out`] / [`unsafe_arena_copy_in`] for a `None`.
///
/// The address is valid for an access the caller makes before the block can
/// be freed, which the FFM element door guarantees the way the scoped fold
/// does: inside an `ffm_fast::AccessWindow`, after the session's state read,
/// so a session close (and the free after it) waits for the access.
#[inline]
pub fn unsafe_arena_jit_resolve(addr: i64, width: u64) -> Option<usize> {
    unsafe_arena::store().jit_resolve(addr, width)
}


/// Fallible `allocateMemory` backing: `None` means "the allocation failed",
/// which the native maps to `OutOfMemoryError`. See
/// `unsafe_arena::ArenaStore::try_allocate`.
pub fn unsafe_arena_try_allocate(size: usize) -> Option<i64> {
    unsafe_arena::store().try_allocate(size)
}

/// Fallible `reallocateMemory` backing. See
/// `unsafe_arena::ArenaStore::try_reallocate`.
pub fn unsafe_arena_try_reallocate(addr: i64, new_size: usize) -> Option<i64> {
    unsafe_arena::store().try_reallocate(addr, new_size)
}

/// True if `addr` is a live `Unsafe.allocateMemory` arena handle (as opposed
/// to a real OS pointer). NIO native I/O (e.g. `sun/nio/ch/Net.read0/write0`,
/// which live in the `native-io` crate) uses this — via the `NativeContext`
/// bridge — to read/write `DirectByteBuffer` memory that `Util`'s temp-buffer
/// path backs with arena handles instead of raw pointers. Without it,
/// `net_write0` would `memcpy` from a tagged synthetic handle and SIGSEGV.
///
/// R1: handles carry [`unsafe_arena::ARENA_TAG`] (bit 62), which is never set
/// on a real OS pointer, so this is an exact classifier — a real
/// `allocateDirect` pointer can never be mistaken for a handle.
pub fn unsafe_arena_contains(addr: i64) -> bool {
    unsafe_arena::store().contains(addr)
}

/// Is `addr` `Unsafe.allocateMemory` memory -- an address the arena's own
/// accessors ([`unsafe_arena_real_ptr`], [`unsafe_arena_copy_out`], ...) serve?
///
/// For the sites that choose the ARENA path for arena memory rather than
/// translate a handle: the CAS / atomic / volatile natives (whose other
/// null-base path is a private side store, not memory), `copySwapMemory`,
/// direct-buffer `VarHandle` atomics, the JIT's buffer-view fast path. In
/// handle mode it is the tag test ([`unsafe_arena_addr_is_tagged`]: a freed
/// handle too, which those paths then refuse); in real-pointer mode a live
/// block's membership, since a real address carries no mark (a freed block's
/// address is then nobody's, as on HotSpot).
pub fn unsafe_arena_names_block(addr: i64) -> bool {
    let store = unsafe_arena::store();
    if !store.real_pointers() {
        return unsafe_arena_addr_is_tagged(addr);
    }
    thread_local! {
        static LAST_REAL_BLOCK: std::cell::Cell<Option<(i64, u64)>> = const {
            std::cell::Cell::new(None)
        };
    }
    let generation = store.membership_generation();
    LAST_REAL_BLOCK.with(|last| {
        if last.get() == Some((addr, generation)) {
            return true;
        }
        let live = store.contains(addr);
        // Only cache positives: allocation can make a negative live without a removal.
        if live && generation == store.membership_generation() {
            last.set(Some((addr, generation)));
        }
        live
    })
}

/// The pointer an `Unsafe` atomic or volatile access on arena memory works on:
/// the live block holding `addr`, good for `width` bytes, or `None` when `addr`
/// names no block (the caller keeps its other path) or the block is short.
/// In real-pointer mode `addr` itself, after one read-locked bounds check
/// ([`unsafe_arena_names_block`] then [`unsafe_arena_real_ptr_bounded`] cost a
/// read walk plus a write-locked one per CAS: `Sbr3FfmUnsafe`'s +12 %); in
/// handle mode a tagged handle's translation, as before.
pub fn unsafe_arena_access_ptr(addr: i64, width: usize) -> Option<*mut u8> {
    let store = unsafe_arena::store();
    if store.real_pointers() {
        return store
            .real_mode_covers(addr, width)
            .then_some(addr as usize as *mut u8);
    }
    if !unsafe_arena_addr_is_tagged(addr) {
        return None;
    }
    unsafe_arena_real_ptr_bounded(addr, width)
}

/// ADDR21-1 Option A: is the `Unsafe` arena in real-pointer mode
/// (`CRATONVM_UNSAFE_REAL_POINTERS`, read once)? `allocateMemory` then returns
/// the block's real address, as on HotSpot, and no handle is ever minted.
pub fn unsafe_arena_real_pointers() -> bool {
    unsafe_arena::store().real_pointers()
}

/// True if `addr` carries the arena tag bit (bit 62) — i.e. it is an
/// `Unsafe.allocateMemory` handle (LIVE **or** freed), as opposed to a real OS
/// pointer such as a `ByteBuffer.allocateDirect` address from `dbb_allocate`.
///
/// The single-element `Unsafe.get/putX(long)` natives use this to classify a
/// store-rejected address: a *tagged* reject is a freed/out-of-bounds handle
/// and must keep surfacing the use-after-free `IllegalArgumentException`, while
/// an *untagged* reject is a real pointer that should fall through to a raw
/// access (mirroring the `copyMemory` real-pointer path). Unlike
/// [`unsafe_arena_contains`], this is true for freed handles too — it tests the
/// tag, not liveness.
///
/// `pub` because the VM's `copy_from_native_memory` / `copy_to_native_memory`
/// bridge classifies with THIS rather than with `unsafe_arena_contains`: the
/// liveness test sends a freed handle down the raw-pointer branch, where it is
/// dereferenced as an OS address. A tag test refuses it instead, and costs one
/// AND rather than an `RwLock` read plus a `BTreeMap` range probe.
///
/// Never true for an `Unsafe.allocateMemory` address in real-pointer mode
/// ([`unsafe_arena_real_pointers`]): every caller then takes its real-pointer
/// branch, which is the block's own memory.
pub fn unsafe_arena_addr_is_tagged(addr: i64) -> bool {
    addr & unsafe_arena::ARENA_TAG != 0
}

/// Translate a live arena handle into a REAL pointer a native library can
/// dereference, together with the bytes remaining in its block. `None` for an
/// untagged address (already a real pointer — nothing to translate) and for a
/// freed or out-of-bounds handle.
///
/// This is the one place the arena's backing store is exposed rather than
/// copied, and it exists for the JNI boundary alone: `GetDirectBufferAddress`
/// must answer with something C can dereference. Every in-VM consumer should
/// keep using [`unsafe_arena_copy_out`] / [`unsafe_arena_copy_in`], which are
/// bounds-checked on every access.
pub fn unsafe_arena_real_ptr(addr: i64) -> Option<(*mut u8, usize)> {
    unsafe_arena::store().real_ptr(addr)
}

/// `r16-ffm-plain-downcall-costs-microseconds` (round 19): the live block
/// holding `addr`, for one downcall that holds the block's session -- see
/// `ArenaStore::peek`. `(real pointer, block real base, block length, block
/// handle base)`.
pub fn unsafe_arena_peek(addr: i64, want: usize) -> Option<(*mut u8, usize, usize, i64)> {
    unsafe_arena::store().peek(addr, want)
}

/// The arena handle Java names the byte at REAL address `real` by, when it is
/// inside (or one past the end of) a live `Unsafe.allocateMemory` block; `None`
/// otherwise. See `ArenaStore::handle_of_real`.
pub fn unsafe_arena_handle_of_real(real: usize) -> Option<i64> {
    unsafe_arena::store().handle_of_real(real)
}

/// [`unsafe_arena_peek`], asking the JIT-readable table first (JIT round 22,
/// lane memory, FFM22-3: the compiled segment call's address, which the
/// store's lock and ordered-map walk made ~700 instructions of a ~2 400
/// instruction call). The same answer.
pub fn unsafe_arena_peek_table_first(
    addr: i64,
    want: usize,
) -> Option<(*mut u8, usize, usize, i64)> {
    let store = unsafe_arena::store();
    store
        .jit_peek(addr, want)
        .or_else(|| store.peek(addr, want))
}

/// Translate a live arena handle into a real pointer that is good for at least
/// `want` bytes, or refuse.
///
/// [`unsafe_arena_real_ptr`] returns the bound and every caller used to throw it
/// away. This is the same translation with the bound APPLIED: a JNI caller that
/// is about to publish a pointer alongside an advertised length passes that
/// length here, and a block that cannot cover it answers `None` instead of
/// handing out a pointer the callee will legitimately run off the end of.
///
/// `None` is the right answer at the JNI boundary because the spec already
/// reserves NULL for "not a direct buffer", so natives that check anything
/// check for it — whereas a short pointer is checked by nobody.
pub fn unsafe_arena_real_ptr_bounded(addr: i64, want: usize) -> Option<*mut u8> {
    let (ptr, remaining) = unsafe_arena_real_ptr(addr)?;
    if remaining < want {
        unsafe_arena::SHORT_TRANSLATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            target: "cratonvm::unsafe_arena",
            handle = format!("{addr:#x}"),
            remaining,
            want,
            "refusing to publish a real pointer for an Unsafe-arena block that is SHORTER than the length being advertised with it -- the callee would write past the block"
        );
        return None;
    }
    Some(ptr)
}

/// The lifetime audit for real pointers handed out of the Unsafe arena.
///
/// # Why this exists
///
/// [`unsafe_arena_real_ptr`] is the one place the arena's backing store is
/// exposed rather than copied, and it has two production callers, both in
/// `jni.rs`: `jni_long_arg_bits` (a tagged handle arriving as a JNI `jlong`
/// argument — netty-tcnative's `SSL.bioWrite(long bio, long address, int len)`
/// is the named case) and `direct_buffer_native_address`. Both USED to discard
/// the `remaining` bound, so the callee received a bare pointer into a
/// `Vec<u8>` with no length and nothing checked what it wrote.
///
/// Two ways that becomes a write into unrelated memory, and where each stands:
///
/// * **the callee writes past the block.** `direct_buffer_native_address` now
///   goes through [`unsafe_arena_real_ptr_bounded`] with the buffer's own
///   advertised capacity, so a block that cannot cover it is refused and
///   counted in `short_translations`. `jni_long_arg_bits` cannot be bounded —
///   a `jlong` argument carries no length — and is counted in `translations`
///   only;
/// * **the block is `reallocate`d or `free`d while the pointer is
///   outstanding.** `reallocate` no longer drops the displaced buffer when a
///   pointer is outstanding: it RETAINS it, so the pointer keeps naming memory
///   this process owns (`retained_on_realloc`). `free` still ends the
///   pointer's life, which is what the contract says on any VM.
///
/// # Reading a zero
///
/// `translations` is the DENOMINATOR and is why this is printed rather than
/// just warned about: without it, "the hazard never fired" and "no pointer was
/// ever handed out on this workload" are the same run. `stale_on_*` and
/// `short_translations` are only meaningful beside a non-zero `translations`.
///
/// # Provenance
///
/// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` hunted a writer
/// that puts an 8-byte arena-pointer-shaped value onto a live object's header,
/// and eliminated every managed store path, the allocator, the slide and
/// `copy_to_native_memory`. This path was its last untested row. The crash was
/// later bisected to the collector's own reference processing (`aa4bc7922`),
/// so this is not that writer — but the bound was genuinely being dropped, and
/// that is fixed here rather than left as a probe.
pub fn unsafe_arena_translation_stats() -> ArenaTranslationStats {
    use std::sync::atomic::Ordering;
    ArenaTranslationStats {
        translations: unsafe_arena::TRANSLATIONS.load(Ordering::Relaxed),
        stale_on_realloc: unsafe_arena::STALE_ON_REALLOC.load(Ordering::Relaxed),
        stale_on_free: unsafe_arena::STALE_ON_FREE.load(Ordering::Relaxed),
        retained_on_realloc: unsafe_arena::RETAINED_ON_REALLOC.load(Ordering::Relaxed),
        short_translations: unsafe_arena::SHORT_TRANSLATIONS.load(Ordering::Relaxed),
    }
}

/// The arena real-pointer audit, as one greppable line, at process exit.
///
/// HERE in `native-builtins` and not only in `vm-cli`, for the reason the
/// corrupt-cell census records beside it on the same path: **a JUnit runner
/// leaves through `System.exit` and never reaches `vm-cli`'s normal-return
/// arm.** The netty class this counter was built for --
/// `io.netty.util.ResourceLeakDetectorTest` under
/// `zgc-rewrite-pass-walks-off-a-reference-array-20260815` -- is exactly such a
/// runner, and it exits NON-ZERO on its one failing test. A line printed only
/// on the normal-return path is therefore missing from precisely the runs
/// anyone would look at, and reads as a clean zero.
///
/// Prints when there is something to report, or when GC statistics were asked
/// for. `translations` is the DENOMINATOR: without it, "the hazard never
/// fired" and "no pointer was ever handed out on this workload" are the same
/// run, which is the whole reason the count exists.
///
/// A `Once` keeps the two exit paths from printing it twice. On the
/// `System.exit` path only `CRATONVM_GC_STATS` is consulted -- `--verbose:gc`
/// is a launcher flag and is not reachable from a native.
pub fn arena_translation_exit_summary(always: bool) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let s = unsafe_arena_translation_stats();
    let noteworthy = s.stale_on_realloc != 0 || s.stale_on_free != 0 || s.short_translations != 0;
    // nio21 wave 10 lane L5: a noteworthy count alone prints only under
    // `CRATONVM_DBG=exit-census` (HotSpot's stderr holds no such line); GC
    // stats (`always`) still print the row unconditionally.
    if !always && !(noteworthy && crate::lang_system::exit_census_on()) {
        return;
    }
    ONCE.call_once(|| {
        eprintln!(
            "[VM] arena-ptr: translations={} short_translations={} stale_on_realloc={} retained_on_realloc={} stale_on_free={}",
            s.translations,
            s.short_translations,
            s.stale_on_realloc,
            s.retained_on_realloc,
            s.stale_on_free,
        );
    });
}

/// End one translation taken by [`unsafe_arena_real_ptr`] / [`unsafe_arena_real_ptr_bounded`]
/// for `addr` (the native call that held the pointer has returned).
pub fn unsafe_arena_release_translation(addr: i64) {
    unsafe_arena::store().release_ptr(addr);
}

/// See [`unsafe_arena_translation_stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArenaTranslationStats {
    /// Real pointers handed out of the arena. The denominator: every other
    /// field is unreadable without it.
    pub translations: u64,
    /// Resizes that MOVED a block while a real pointer into it was
    /// outstanding. Each one is also a `retained_on_realloc`.
    pub stale_on_realloc: u64,
    /// Frees of a block while a real pointer into it was outstanding.
    pub stale_on_free: u64,
    /// Displaced buffers kept alive rather than dropped, so a stale pointer
    /// names memory this process still owns.
    pub retained_on_realloc: u64,
    /// Translations REFUSED because the block was shorter than the length
    /// being advertised alongside the pointer.
    pub short_translations: u64,
}

/// Copy bytes out of the Unsafe arena (arena → `out`). Returns false if the
/// range isn't fully inside one live arena block. See [`unsafe_arena_contains`].
pub fn unsafe_arena_copy_out(addr: i64, out: &mut [u8]) -> bool {
    unsafe_arena::store().copy_out(addr, out)
}

/// Copy bytes into the Unsafe arena (`data` → arena). Symmetric to
/// [`unsafe_arena_copy_out`].
pub fn unsafe_arena_copy_in(addr: i64, data: &[u8]) -> bool {
    unsafe_arena::store().copy_in(addr, data)
}

pub fn unsafe_arena_reallocate(addr: i64, new_size: usize) -> i64 {
    unsafe_arena::store().reallocate(addr, new_size)
}

static NATIVE_SPAN_FREE_HOOKS: parking_lot::Mutex<Vec<fn(usize, usize)>> =
    parking_lot::Mutex::new(Vec::new());

/// Register a process-wide callback invoked with `(ptr, byte_len)` right
/// before an off-heap `unsafe_arena` allocation or retired relocation buffer
/// is dropped and returned to the host allocator.
pub fn register_native_span_free_hook(hook: fn(usize, usize)) {
    let mut guard = NATIVE_SPAN_FREE_HOOKS.lock();
    if !guard.iter().any(|&h| h as usize == hook as usize) {
        guard.push(hook);
    }
}

pub(super) fn notify_native_span_free(ptr: usize, len: usize) {
    if ptr == 0 || len == 0 {
        return;
    }
    let hooks: Vec<fn(usize, usize)> = NATIVE_SPAN_FREE_HOOKS.lock().clone();
    for hook in hooks {
        hook(ptr, len);
    }
}

/// Free an arena block. `pub` for the same reason [`unsafe_arena_allocate`] is.
pub fn unsafe_arena_free(addr: i64) {
    unsafe_arena::store().free(addr);
}

pub fn unsafe_arena_get_byte(addr: i64) -> u8 {
    unsafe_arena::store().get_byte(addr)
}

// audit-round5 fix #3 (HIGH): bounds-checked get* shims. Return `Some(v)`
// when the address falls inside a live arena, `None` otherwise. The
// get-at-address natives map `None` to IllegalArgumentException to match
// the put-side behavior fixed in round-4 wave-1.
pub fn unsafe_arena_try_get_byte(addr: i64) -> Option<u8> {
    unsafe_arena::store().try_get_byte(addr)
}

/// audit-2026-05-16: returns `true` on success, `false` when the write
/// would extend the arena past its original size. Java natives map `false`
/// to `IllegalArgumentException`.
pub fn unsafe_arena_put_byte(addr: i64, v: u8) -> bool {
    unsafe_arena::store().put_byte(addr, v)
}

pub fn unsafe_arena_get_short(addr: i64) -> i16 {
    unsafe_arena::store().get_short(addr)
}

pub fn unsafe_arena_try_get_short(addr: i64) -> Option<i16> {
    unsafe_arena::store().try_get_short(addr)
}

pub fn unsafe_arena_put_short(addr: i64, v: i16) -> bool {
    unsafe_arena::store().put_short(addr, v)
}

pub fn unsafe_arena_get_int(addr: i64) -> i32 {
    unsafe_arena::store().get_int(addr)
}

pub fn unsafe_arena_try_get_int(addr: i64) -> Option<i32> {
    unsafe_arena::store().try_get_int(addr)
}

pub fn unsafe_arena_put_int(addr: i64, v: i32) -> bool {
    unsafe_arena::store().put_int(addr, v)
}

pub fn unsafe_arena_get_long(addr: i64) -> i64 {
    unsafe_arena::store().get_long(addr)
}

pub fn unsafe_arena_try_get_long(addr: i64) -> Option<i64> {
    unsafe_arena::store().try_get_long(addr)
}

pub fn unsafe_arena_put_long(addr: i64, v: i64) -> bool {
    unsafe_arena::store().put_long(addr, v)
}

/// Unsafe.throwException(Throwable) — throw a checked exception unchecked.
pub fn native_unsafe_throw_exception(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match args.get(1) {
        Some(Value::Object(Some(exc))) => Err(MethodCallFailed::ExceptionThrown(*exc)),
        // A null throwable cannot be thrown. HotSpot 25.0.4+7 does not refuse
        // it either -- it SIGSEGVs inside `Unsafe_ThrowException` -- so this is
        // not "match the oracle"; the oracle has no answer here. Returning
        // quietly is the one option that is certainly wrong: the caller wrote
        // `throwException(x)` expecting control not to reach the next line.
        Some(Value::Object(None)) => Err(RuntimeError::NullPointerException {
            message: Some("Unsafe.throwException: null throwable".to_string()),
        }
        .into()),
        _ => Ok(None),
    }
}

// ===========================================================================
// Session 10: New native method implementations for native bridging
// ===========================================================================

/// Unsafe.objectFieldOffset1(Class, String) — lookup field offset by name.
/// JDK 25 uses this when the Field object is not available.
pub fn native_unsafe_object_field_offset1(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0]=Unsafe this, args[1]=Class mirror, args[2]=field name String
    let field_name = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Long(0))),
    };
    let class_mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Long(0))),
    };
    // Resolve the Class mirror's ClassId. Real-JDK Class layout doesn't
    // have classId in slot 0 (slot 0 is `cachedConstructor` or similar
    // ObjectRef), so we go through the reverse-map registered when the
    // mirror is allocated. The synthetic-jdk path keeps slot 0 = Int(cid)
    // and is handled by the fallback below.
    let resolved_cid: Option<cratonvm_types::ClassId> = ctx
        .class_id_from_mirror(class_mirror)
        .or_else(|| match ctx.get_field(class_mirror, 0) {
            // `cid >= 0` matters: a PRIMITIVE mirror carries `Int(-1)` in slot 0
            // as its marker, and without this guard that decodes to
            // `ClassId(0xFFFF_FFFF)` — a plausible-looking id for a class that
            // does not exist. `mirror_class_id` in lang_class.rs has always had
            // the guard; this copy did not.
            Value::Int(cid) if cid >= 0 => Some(cratonvm_types::ClassId::new(cid as u32)),
            _ => None,
        });
    if let Some(class_id) = resolved_cid {
        // By `ClassId`, not name: see `native_unsafe_object_field_offset` (same-named classes of two loaders).
        if let Some(slot) = ctx.resolve_field_index_by_class_id(class_id, &field_name) {
            // Wave 8c: a byte offset under byte field offsets.
            let encoded = unsafe_encode_field_offset(ctx.unsafe_byte_field_offsets(), slot);
            return Ok(Some(Value::Long(encoded as i64)));
        }

        // T19.H1 — `declared_fields` only lists fields declared by
        // `class_id` itself, not inherited ones. HotSpot resolves
        // `Unsafe.objectFieldOffset(C.class, "name")` by walking the
        // inheritance chain; we must do the same, otherwise a field
        // inherited from a superclass (e.g. AbstractMap.keySet) is
        // invisible and we return 0, which collides with the object
        // header / first slot and causes `Unsafe.compareAndSetInt` to
        // read the wrong storage — livelocking every CAS loop built on
        // top of it (the original KC16 + KC26 hang at
        // ConcurrentHashMap.initTable).
        let mut cid_opt = Some(class_id);
        while let Some(cid) = cid_opt {
            let fields = ctx.declared_fields(cid);
            for f in &fields {
                if !f.is_static && f.name == field_name {
                    // Wave 8c: a byte offset under byte field offsets.
                    let encoded =
                        unsafe_encode_field_offset(ctx.unsafe_byte_field_offsets(), f.slot_index);
                    return Ok(Some(Value::Long(encoded as i64)));
                }
            }
            cid_opt = ctx.superclass_of(cid);
        }
        // Static fields live in a separate slot space. Only walk them
        // if the caller explicitly asked for a static via
        // `staticFieldOffset` — instance lookups stop at the chain end.
        if field_name.starts_with("static:") {
            // Conservative — HotSpot's staticFieldOffset is a distinct
            // entry point; this branch is defensive so we don't silently
            // fall through to zero and reintroduce the livelock if a
            // caller mis-dispatches.
            let real_name = &field_name["static:".len()..];
            let mut cid_opt = Some(class_id);
            while let Some(cid) = cid_opt {
                let fields = ctx.declared_fields(cid);
                for f in &fields {
                    if f.is_static && f.name == real_name {
                        return Ok(Some(Value::Long(f.slot_index as i64)));
                    }
                }
                cid_opt = ctx.superclass_of(cid);
            }
        }
    }
    // Lookup failed.  Returning `0` here used to alias slot 0 of the
    // receiver and livelock the caller's CAS loop (see WildFly's
    // `Class$Atomic.casReflectionData` and the Spring Boot
    // `AbstractClassLoaderValue.putIfAbsent` watchdog hangs).  Mint a
    // unique non-zero synthetic offset instead — the Unsafe.{CAS,get,put}
    // natives detect the synthetic range and route through a per-object
    // side store (`synthetic_field_store`) that yields self-consistent
    // load/CAS/store semantics.  This unblocks lazy-init guards whose
    // backing field happens to live on a class our layout doesn't
    // expose (typically `java.lang.Class`'s synthetic
    // `reflectionData` / `annotationData` / `annotationType` slots, or
    // ConcurrentHashMap's `table` reference when the populator failed
    // to wire the rj_slot metadata).
    let cname = resolved_cid
        .and_then(|cid| ctx.class_name_of_id(cid))
        .unwrap_or_default();
    // A STATIC field, asked for by its plain name. HotSpot accepts this at
    // this door and CratonVM must too -- measured: `internal oFO(Class,String)
    // static name` is `no-throw` on HotSpot.
    //
    // The `static:`-prefixed branch above is for a caller that mis-dispatches;
    // this is the ordinary spelling. Registering the offset (rather than
    // merely numbering it, which is what the old mint did) is what makes a
    // later get/CAS reach the REAL static instead of a phantom slot -- the
    // same distinction the memory-access latch repair turned on.
    if let Some(cid0) = resolved_cid {
        let mut cid_opt = Some(cid0);
        while let Some(cid) = cid_opt {
            let mut static_idx = 0usize;
            for f in &ctx.declared_fields(cid) {
                if f.is_static {
                    if f.name == field_name {
                        let cname = ctx.class_name_of_id(cid).unwrap_or_default();
                        // Wave 103, L2: keyed by the class too.
                        let offset =
                            crate::static_field_synthetic_offset(&cname, &field_name, cid);
                        let vm = ctx.vm_identity();
                        remember_unsafe_static_field_offset(vm, offset, cid, static_idx);
                        return Ok(Some(Value::Long(offset as i64)));
                    }
                    static_idx += 1;
                }
            }
            cid_opt = ctx.superclass_of(cid);
        }
    }

    // REFUSE, as HotSpot does. This used to mint a synthetic offset and route
    // it through a per-object side store, so a lazy-init guard ran against a
    // phantom field and its CAS reported success against nothing any reader
    // can see.
    //
    // The mint's two documented consumers were measured away (record §14:
    // `Class$Atomic` resolves its three real names on JDK 25;
    // `AbstractClassLoaderValue` references `Unsafe` zero times), and the path
    // takes 0 mints across 120 regression vectors, 136 Netty buffer/util
    // classes, both DoD workloads and both H2 vectors -- each with a firing
    // positive control (`MintReachProbe`, 2).
    //
    // WildFly/Keycloak/Elasticsearch are not runnable on this host. If one of
    // them does reach here, this now names the class and the field in a
    // catchable `InternalError` instead of diverging silently.
    Err(RuntimeError::InternalError {
        message: format!("no such field {field_name:?} on class {cname:?}"),
    }
    .into())
}

pub fn unsafe_compare_exchange_static_field(
    ctx: &mut dyn NativeContext,
    offset: usize,
    expected: Value,
    update: Value,
) -> Option<Value> {
    let (class_id, field_index) = unsafe_static_field_target(&*ctx, offset)?;
    // JIT round 21 (lane staticcas): the witness of ONE atomic
    // compare-and-exchange. The loop it replaces read the static, then CASed
    // through a read-then-write `unsafe_static_cas`, so its witness and its
    // swap were two different instants.
    let (witness, _) =
        ctx.compare_and_exchange_static_field(class_id, field_index, expected, update);
    Some(unsafe_static_witness_as(witness, expected))
}

/// A static's CAS witness in the access's own kind: a `float` / `double`
/// static exchanged through the JDK's `compareAndExchangeInt` / `Long` (its
/// `*Float` / `*Double` forms pass the raw bits) answers those bits, not a
/// `Float` the `int` / `long` natives would replace with `expected`.
pub fn unsafe_static_witness_as(witness: Value, expected: Value) -> Value {
    // Casts: bit reinterpretation.
    match (witness, expected) {
        (Value::Float(f), Value::Int(_)) => Value::Int(f.to_bits() as i32),
        (Value::Double(d), Value::Long(_)) => Value::Long(d.to_bits() as i64),
        (w, _) => w,
    }
}

pub fn unsafe_compare_exchange_synthetic_field(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    expected: Value,
    update: Value,
) -> Value {
    // JIT round 21 (lane staticcas): a whole static on its mirror base is ONE
    // compare-and-exchange; the loop below read the witness, then CASed, and
    // its `unsafe_cas_values_equal` never matched a `float` / `double` static
    // against the raw bits the JDK's `*Float` / `*Double` forms pass. Byte `k`
    // of a static keeps the loop (`synthetic_cas`'s lane arm).
    if unsafe_static_sub_target(&*ctx, Some(obj), offset).is_none() {
        if let Some((class_id, field_index)) =
            unsafe_static_field_target_for_base(&*ctx, obj, offset)
        {
            let (witness, _) =
                ctx.compare_and_exchange_static_field(class_id, field_index, expected, update);
            return unsafe_static_witness_as(witness, expected);
        }
    }
    let mut attempts = 0usize;
    loop {
        let old = synthetic_get(ctx, obj, offset);
        if !unsafe_cas_values_equal(old.clone(), expected.clone()) {
            return old;
        }
        if synthetic_cas(ctx, obj, offset, expected.clone(), update.clone()) {
            return old;
        }
        attempts = attempts.wrapping_add(1);
        if attempts % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
}

pub fn unsafe_compare_exchange_heap_slot(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    index: usize,
    expected: Value,
    update: Value,
) -> Value {
    let is_array = ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array;
    let mut attempts = 0usize;
    loop {
        let old = if is_array {
            ctx.get_array_element(obj, index)
        } else {
            let value = ctx.get_field_volatile(obj, index);
            // A failed exchange still hands a strong witness to Java.
            slot0_read_keep_alive(ctx, obj, index, value)
        };
        if !unsafe_cas_values_equal(old.clone(), expected.clone()) {
            return old;
        }
        // Interpreter round i6 wave 127, lane L4: a `float` / `double` slot
        // that matched an int / long expectation by its bits is exchanged
        // as itself: the CAS expects the `Value::Float` / `Value::Double` it
        // just read (same bits) and stores the update re-tagged, so a
        // context whose CAS compares tags strictly still lands it, and the
        // slot keeps its own kind.
        let (cas_expected, cas_update) = match (&old, &expected) {
            (Value::Float(_), Value::Int(_)) | (Value::Double(_), Value::Long(_)) => {
                (old, unsafe_retag_bits_like(&old, update))
            }
            _ => (expected, update),
        };
        if ctx.compare_and_swap_field(obj, index, cas_expected, cas_update) {
            return old;
        }
        attempts = attempts.wrapping_add(1);
        if attempts % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
}

/// Unsafe.compareAndExchangeInt atomically sets field to update if current == expected,
/// returns the witness value (old value).
pub fn native_unsafe_compare_and_exchange_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Raw bytes for an int-width exchange over another element type; see
    // [`native_unsafe_cas_int`].
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Int, 4)
    {
        let (ex, nv) = (
            unsafe_wide_arg_bits(args, 3, 4),
            unsafe_wide_arg_bits(args, 4, 4),
        );
        if let Some((cur, _)) = unsafe_wide_raw_cas(ctx, obj, off, 4, ex, nv) {
            return Ok(Some(Value::Int(cur as u32 as i32)));        }    }
    native_unsafe_compare_and_exchange_int_element(ctx, args)
}

/// [`native_unsafe_compare_and_exchange_int`] without the raw-bytes arm (the
/// byte/short exchanges' fallback).
pub fn native_unsafe_compare_and_exchange_int_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cae, 4) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Cae, 4) {
        return Ok(Some(v));
    }
    let obj = unsafe_obj(args, 1);
    let offset = unsafe_offset(args, 2);
    let expected = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let update = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let expected_value = Value::Int(expected);
    let update_value = Value::Int(update);
    if obj.is_none() {
        if let Some(old) = unsafe_compare_exchange_static_field(
            ctx,
            offset,
            expected_value.clone(),
            update_value.clone(),
        ) {
            return Ok(Some(match old {
                Value::Int(v) => Value::Int(v),
                // Wave 127 (lane L4): a `float` slot answers its bits.
                Value::Float(f) => Value::Int(f.to_bits() as i32),
                _ => Value::Int(expected),
            }));
        }
    }
    if let Some(obj_ref) = obj {
        let old = if unsafe_synthetic_field_offset(&*ctx, obj_ref, offset) {
            unsafe_compare_exchange_synthetic_field(
                ctx,
                obj_ref,
                offset,
                expected_value,
                update_value,
            )
        } else {
            let index = if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                match unsafe_checked_array_index(ctx, obj_ref, offset) {
                    Some(i) => i,
                    None => return Ok(Some(Value::Int(expected))),
                }
            } else {
                // Wave 8c: the slot (an offset inside a field took the lane above).
                unsafe_field_slot(&*ctx, obj_ref, offset).0
            };
            unsafe_compare_exchange_heap_slot(ctx, obj_ref, index, expected_value, update_value)
        };
        return Ok(Some(match old {
            Value::Int(v) => Value::Int(v),
            // Wave 127 (lane L4): a `float` slot answers its bits.
            Value::Float(f) => Value::Int(f.to_bits() as i32),
            _ => Value::Int(expected),
        }));
    }
    Ok(Some(Value::Int(expected)))
}

/// Unsafe.compareAndExchangeLong atomically sets field to update if current == expected.
pub fn native_unsafe_compare_and_exchange_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cae, 8) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Cae, 8) {
        return Ok(Some(v));
    }
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Long, 8)
    {
        let (ex, nv) = (
            unsafe_wide_arg_bits(args, 3, 8),
            unsafe_wide_arg_bits(args, 4, 8),
        );
        if let Some((cur, _)) = unsafe_wide_raw_cas(ctx, obj, off, 8, ex, nv) {
            return Ok(Some(Value::Long(cur as i64)));
        }
    }
    let obj = unsafe_obj(args, 1);
    let offset = unsafe_offset(args, 2);
    let expected = match args.get(3) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let update = match args.get(4) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let expected_value = Value::Long(expected);
    let update_value = Value::Long(update);
    if obj.is_none() {
        if let Some(old) = unsafe_compare_exchange_static_field(
            ctx,
            offset,
            expected_value.clone(),
            update_value.clone(),
        ) {
            return Ok(Some(match old {
                Value::Long(v) => Value::Long(v),
                // Wave 127 (lane L4): a `double` slot answers its bits.
                Value::Double(d) => Value::Long(d.to_bits() as i64),
                _ => Value::Long(expected),
            }));
        }
    }
    if let Some(obj_ref) = obj {
        let old = if unsafe_synthetic_field_offset(&*ctx, obj_ref, offset) {
            unsafe_compare_exchange_synthetic_field(
                ctx,
                obj_ref,
                offset,
                expected_value,
                update_value,
            )
        } else {
            let index = if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                match unsafe_checked_array_index(ctx, obj_ref, offset) {
                    Some(i) => i,
                    None => return Ok(Some(Value::Long(expected))),
                }
            } else {
                // Wave 8c: the slot (an offset inside a field took the lane above).
                unsafe_field_slot(&*ctx, obj_ref, offset).0
            };
            unsafe_compare_exchange_heap_slot(ctx, obj_ref, index, expected_value, update_value)
        };
        return Ok(Some(match old {
            Value::Long(v) => Value::Long(v),
            // Wave 127 (lane L4): a `double` slot answers its bits.
            Value::Double(d) => Value::Long(d.to_bits() as i64),
            _ => Value::Long(expected),
        }));
    }
    Ok(Some(Value::Long(expected)))
}

/// Unsafe.compareAndExchangeReference atomically sets field to update if current == expected.
pub fn native_unsafe_compare_and_exchange_reference(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = unsafe_obj(args, 1);
    let offset = unsafe_offset(args, 2);
    let expected = recover_object_arg(args.get(3).copied().unwrap_or(Value::Object(None)));
    let update = recover_object_arg(args.get(4).copied().unwrap_or(Value::Object(None)));
    if obj.is_none() {
        if let Some(old) =
            unsafe_compare_exchange_static_field(ctx, offset, expected.clone(), update.clone())
        {
            return Ok(Some(recover_object_arg(old)));
        }
    }
    if let Some(obj_ref) = obj {
        let old = if unsafe_synthetic_field_offset(&*ctx, obj_ref, offset) {
            unsafe_compare_exchange_synthetic_field(ctx, obj_ref, offset, expected, update)
        } else {
            let index = if ctx.heap_kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                match unsafe_checked_array_index(ctx, obj_ref, offset) {
                    Some(i) => i,
                    None => return Ok(Some(Value::Object(None))),
                }
            } else {
                // Wave 8c: the slot; an offset inside a field names no
                // reference (no exchange).
                match unsafe_field_slot(&*ctx, obj_ref, offset) {
                    (_, k) if k != 0 => return Ok(Some(Value::Object(None))),
                    (slot, _) => slot,
                }
            };
            unsafe_compare_exchange_heap_slot(ctx, obj_ref, index, expected, update)
        };
        return Ok(Some(recover_object_arg(old)));
    }
    Ok(Some(Value::Object(None)))
}

