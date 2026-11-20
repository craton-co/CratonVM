#[allow(unused_imports)]
use super::*;

#[inline]
pub fn s2_bb_arr(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<ObjectRef> {
    // Prefer the real-JDK `hb` field — when the JDK ByteBuffer class
    // is loaded, the indexed slot 0 lands on Buffer.mark (int
    // descriptor) and any Object write was coerced to Int(low_bits).
    if let Value::Object(Some(a)) = ctx.get_field_by_name(buf, "hb") {
        return Some(a);
    }
    if let Value::Object(Some(a)) = ctx.get_field(buf, BB_ARRAY) {
        return Some(a);
    }
    // BUG (found 2026-07-11): typed-buffer-view objects (IntBuffer/
    // LongBuffer/ShortBuffer/FloatBuffer/DoubleBuffer, e.g. from
    // `ByteBuffer.asFloatBuffer()`) have NO `hb` field at all (only
    // HeapFloatBuffer etc. declare it, not the abstract class our synthetic
    // views are stamped with) — same descriptor-coercion trap as `hb`
    // applies to the indexed slot 0 above (real slot 0 = `mark: int`), so
    // `s2_typed_buffer_view_fns!`/`s2_view_buf_fn!` (servlet.rs) instead
    // stash the array reference in slot 5 — the real `Buffer.segment` field
    // (`final java.lang.foreign.MemorySegment segment`), the ONLY
    // Object-typed field Buffer itself declares, so `ctx.set_field` doesn't
    // coerce an Object write there. Silently returning 0 for every read
    // when this fallback was missing surfaced as "expected:<X> but
    // was:<0.0>" across nearly the entire ES vector-codec test family —
    // every value read through a typed-buffer view came back zero
    // regardless of what was actually written.
    //
    // W7-83: and slot 5 is `Buffer.segment` on a REAL loaded `java.nio.Buffer`,
    // where the value is a `MemorySegment`, not an array. Measured on Eclipse
    // Adoptium 25.0.3.9: `ByteBuffer.allocate(16)` has `segment == null`,
    // `ByteBuffer.allocateDirect(16)` has `segment == null`, and
    // `Arena.ofAuto().allocate(16).asByteBuffer()` has `hb == null` and
    // `segment == jdk.internal.foreign.NativeMemorySegmentImpl`.
    //
    // Without the kind screen this function returned that `MemorySegment` to
    // `array()`, whose declared return type is `[B`, and made `hasArray()`
    // answer `true` where HotSpot answers `false`. **This registration wins in
    // Compatible mode** (W7-76 §2: `set_drop_real_layout_synthetic(true)` runs
    // before `register_io_natives`, so `register_nio_natives` is skipped and
    // nothing overwrites s2), and `array`/`hasArray`/`arrayOffset` are all on
    // `native_override.rs`'s forced-native list for `java/nio/ByteBuffer`, so
    // the native answers even though the real bytecode is present.
    //
    // Rejecting the segment is what makes the receiver fall through to the
    // callers' direct arms: `array()`'s `None if s2_bb_direct_addr(..)` arm
    // raises `UnsupportedOperationException` and `hasArray()` answers false —
    // exactly HotSpot. `s2_bb_direct_addr` keeps its own
    // `is_plausible_native_addr` screen, which is untouched and is still what
    // stops a heap buffer's `address = 16` being dereferenced.
    match ctx.get_field(buf, BB_SEGMENT_SLOT) {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == ObjectKind::Array => Some(a),
        _ => None,
    }
}

/// Native-memory address of a DIRECT buffer (real-JDK `DirectByteBuffer`
/// has no `hb` heap array; its storage lives at the `address` field). Same
/// name-first/slot-4-fallback resolution as native-io's
/// `directbuffer_address`. `None` for heap buffers and storage-less
/// synthetics.
///
/// Heap buffers ALSO carry a non-zero `address` (`bb_write_hb` seeds
/// ARRAY_BYTE_BASE_OFFSET + offset, i.e. 16+, mirroring the real
/// HeapByteBuffer ctor) — that is an array-relative offset, not a process
/// pointer, so the presence of a heap array must always win over the
/// address field. Guard here so callers can consult this helper directly
/// without repeating the array check.
pub fn s2_bb_direct_addr(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<i64> {
    if s2_bb_heap_window(ctx, buf).is_some() {
        return None;
    }
    match ctx.get_field_by_name(buf, "address") {
        Value::Long(v) if is_plausible_native_addr(v) => Some(v),
        _ => match ctx.get_field(buf, 4) {
            Value::Long(v) if is_plausible_native_addr(v) => Some(v),
            _ => None,
        },
    }
}

/// A `Buffer.address` below the first mappable page is never a process
/// pointer: Linux refuses to map below `vm.mmap_min_addr` (65536 by default)
/// and Windows reserves the low 64 KiB of every address space. What DOES live
/// down there is an array-relative `Unsafe` offset —
/// `ARRAY_BYTE_BASE_OFFSET + offset` — belonging to a HEAP-backed buffer whose
/// array the caller failed to resolve.
///
/// This is a backstop, not the contract: `s2_bb_heap_window` above is what
/// actually resolves those buffers. It earns its place because dereferencing
/// such an "address" is an immediate, unrecoverable SIGSEGV — `addr=0x10`,
/// i.e. exactly `ARRAY_BYTE_BASE_OFFSET`, on 51 of the 53 crashes in the
/// 2026-08-10 three-GC-variant H2 sweep — whereas answering "no storage"
/// degrades to this module's existing benign zero, which a caller can survive.
#[inline]
pub fn is_plausible_native_addr(v: i64) -> bool {
    v >= 0x1_0000
}

/// Array-base offset of a heap buffer — the real-JDK `ByteBuffer.offset`
/// field (element index of logical byte 0 inside `hb`). 0 for fresh
/// allocations/wraps and for layouts with no `offset` field to resolve
/// (typed views, bare synthetics). Non-zero only for aliasing views made
/// by `slice()`/`slice(int,int)` below, mirroring real HeapByteBuffer.
#[inline]
pub fn s2_bb_heap_base(ctx: &dyn NativeContext, buf: ObjectRef) -> usize {
    match ctx.get_field_by_name(buf, "offset") {
        Value::Int(v) if v > 0 => v as usize,
        // A `--synthetic-jdk` stub heap view has no `offset` field (the
        // by-name read is `Object(None)`); `s2_bb_new_heap_view` keeps its
        // non-zero base in `BB_STUB_HEAP_OFFSET_SLOT` instead (interpreter
        // round i5 wave 106, lane L5). A real layout always answers `Int`
        // above, so this arm cannot match there.
        Value::Object(None)
            if ctx.object_num_fields(buf) > BB_STUB_HEAP_OFFSET_SLOT
                && s2_buf_stub_layout(ctx, buf) =>
        {
            match ctx.get_field(buf, BB_STUB_HEAP_OFFSET_SLOT) {
                Value::Int(v) if v > 0 => v as usize,
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// See [`cratonvm_native_io::BB_STUB_HEAP_OFFSET_SLOT`].
pub const BB_STUB_HEAP_OFFSET_SLOT: usize = cratonvm_native_io::BB_STUB_HEAP_OFFSET_SLOT;

/// `Unsafe.ARRAY_BYTE_BASE_OFFSET` as this VM publishes it — the value
/// `bb_write_hb` seeds into a heap `Buffer.address`, and the value the real
/// JDK's own `HeapByteBuffer` ctor adds to its array-base `offset`. Element 0
/// of a `byte[]` sits at this unsafe offset, so subtracting it turns a
/// `Buffer.address` back into a plain byte index.
///
/// **DEFINED AS native-io's, not beside it (F26-1).** Two crates screening the
/// same `Buffer.address` values against two independently-spelled `16`s is the
/// W7-76 §8.2 shape; a `const` initialised from the other crate's `pub const`
/// is const-evaluated, costs nothing, leaves all nine use sites in this file
/// untouched, and makes drift impossible.
pub const ARRAY_BYTE_BASE_OFFSET: i64 = cratonvm_native_io::ARRAY_BYTE_BASE_OFFSET;

/// Backing array + byte index of element 0 for a real-JDK
/// `java/nio/ByteBufferAs<T>Buffer{B,L}` — the concrete view class
/// `ByteBuffer.as<T>Buffer()` returns.
///
/// `asLongBuffer` and friends are NOT in `force_native_over_real_jdk_bytecode`,
/// so against a real JDK they run the JDK's own bytecode and hand back one of
/// these. Its storage lives on the backing `bb` ByteBuffer; the view's own `hb`
/// is null, and `Buffer.address` holds `bb.address + bb.position()` — an
/// `Unsafe` offset (`ARRAY_BYTE_BASE_OFFSET + byteIndex`), **not** a process
/// pointer.
///
/// The bulk `get([JII)`/`put([JII)` accessors, on the other hand, ARE forced:
/// they are declared on the abstract `java/nio/LongBuffer`, which these views
/// do not override, so such a receiver lands in `s2_lb_get_bulk` and from there
/// in `s2_bb_get_byte`. Before this helper existed that path found no array
/// (`s2_bb_arr` looks at `hb`, slot 0 and `Buffer.segment`, none of which the
/// view populates), fell through to `s2_bb_direct_addr`, and dereferenced the
/// `address` value as a pointer: `copy_from_native_memory(0x10, 1)` →
/// SIGSEGV at `addr=0x10`. `org.h2.mvstore.Chunk.readToC`'s
/// `buff.asLongBuffer().get(toc)` does exactly this on every MVStore chunk
/// read, which is why 16-18 H2 classes crashed identically under all three
/// collectors.
///
/// Mirrors `bbacb_read_underlying_bytes` (native-builtins/src/lib.rs), which
/// already resolves the `ByteBufferAsCharBuffer{B,L}` family the same way.
/// `None` for a view over a DIRECT ByteBuffer — there the view's `address`
/// genuinely IS a process pointer and the direct path handles it correctly.
pub fn s2_bb_view_backing(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<(ObjectRef, usize)> {
    let bb = match ctx.get_field_by_name(buf, "bb") {
        Value::Object(Some(b)) => b,
        _ => return None,
    };
    let arr = s2_bb_arr(ctx, bb)?;
    let base = match ctx.get_field_by_name(buf, "address") {
        Value::Long(a) if a >= ARRAY_BYTE_BASE_OFFSET => (a - ARRAY_BYTE_BASE_OFFSET) as usize,
        Value::Int(a) if i64::from(a) >= ARRAY_BYTE_BASE_OFFSET => {
            (i64::from(a) - ARRAY_BYTE_BASE_OFFSET) as usize
        }
        // No usable `address` (a synthetic-layout source): fall back to the
        // backing buffer's own array-base offset, as the char-view helper does.
        // This drops the source's position-at-creation, but a stale window is
        // recoverable where a wild pointer is not.
        _ => s2_bb_heap_base(ctx, bb),
    };
    Some((arr, base))
}

/// Heap storage of ANY buffer this module handles: `(array, byte index of the
/// buffer's logical byte 0)`. Covers the buffer's own array plus its
/// array-base `offset` (heap ByteBuffers, synthetic typed views) and the
/// backing array of a real-JDK `ByteBufferAs<T>Buffer{B,L}` view. `None` for
/// direct and storage-less buffers.
pub fn s2_bb_heap_window(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<(ObjectRef, usize)> {
    if let Some(arr) = s2_bb_arr(ctx, buf) {
        return Some((arr, s2_bb_heap_base(ctx, buf)));
    }
    s2_bb_view_backing(ctx, buf)
}

/// Resolved backing storage of an s2-managed buffer: a heap array plus the
/// buffer's array-base offset, OR a direct native address. This is the
/// single storage-view helper the residual doc
/// (s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md)
/// called for: every method that used to read `s2_bb_arr` only — and
/// silently produced empty/zero results on a DIRECT receiver — goes
/// through here instead.
pub enum S2BbStorage {
    Heap { arr: ObjectRef, base: usize },
    Direct { addr: i64 },
}

pub fn s2_bb_storage(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<S2BbStorage> {
    if let Some((arr, base)) = s2_bb_heap_window(ctx, buf) {
        return Some(S2BbStorage::Heap { arr, base });
    }
    s2_bb_direct_addr(ctx, buf).map(|addr| S2BbStorage::Direct { addr })
}

/// Read `len` bytes starting at logical byte index `from` (position-space,
/// i.e. NOT including the heap array-base offset) from either storage kind.
/// `None` when the buffer is storage-less or a direct native read fails.
pub fn s2_bb_read_window(
    ctx: &dyn NativeContext,
    buf: ObjectRef,
    from: i32,
    len: usize,
) -> Option<Vec<u8>> {
    if from < 0 {
        return None;
    }
    match s2_bb_storage(ctx, buf)? {
        S2BbStorage::Heap { arr, base } => {
            let start = base.checked_add(from as usize)?;
            let end = start.checked_add(len)?;
            if end > ctx.array_length(arr) {
                return None;
            }
            Some(
                (start..end)
                    .map(|i| ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8)
                    .collect(),
            )
        }
        S2BbStorage::Direct { addr } => {
            let mut bytes = vec![0u8; len];
            if ctx.copy_from_native_memory(addr.saturating_add(from as i64), &mut bytes) {
                Some(bytes)
            } else {
                None
            }
        }
    }
}

/// True when the buffer's real-JDK `isReadOnly` flag is set (layouts with
/// no such field — bare synthetics, typed views — always report writable,
/// matching this family's historic behaviour).
///
/// CONVERGED (F14-1 N1). This was one of THREE byte-identical transcriptions of
/// the same one-line field read — this, `charset_buffers.rs::cb_is_read_only`,
/// and `native-io`'s. It is now a delegation to the `native-io` copy: the crate
/// dependency runs `cratonvm-native-builtins` → `cratonvm-native-io` and not
/// the reverse, so that is the only one of the three the other two can import.
/// The local name is kept because it is the name fifteen call sites in this
/// file already use, and because a delegation is one implementation whichever
/// name it wears — what F14-1 objected to was three BODIES that can drift, not
/// three names.
#[inline]
pub fn s2_bb_is_read_only(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    cratonvm_native_io::buffer_is_read_only(ctx, buf)
}

/// The abstract public buffer classes this crate stamps on a carrier it
/// allocated itself.
///
/// A name test, but on the PUBLIC API classes, not on the JDK's generated
/// implementation names — `java.nio.IntBuffer` cannot be renamed by a JDK
/// release without breaking every program in the world, which is not true of
/// `ByteBufferAsIntBufferRB`.
#[inline]
pub fn s2_is_our_buffer_carrier(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(buf);
    matches!(
        ctx.class_name_arc_of_id(cid).as_deref(),
        Some(
            "java/nio/ByteBuffer"
                | "java/nio/CharBuffer"
                | "java/nio/IntBuffer"
                | "java/nio/LongBuffer"
                | "java/nio/ShortBuffer"
                | "java/nio/FloatBuffer"
                | "java/nio/DoubleBuffer"
        )
    )
}

/// [`s2_bb_is_read_only`], **plus the answer for a receiver that keeps its
/// read-only-ness in an OVERRIDDEN METHOD rather than in the field.**
///
/// `buffer_is_read_only` reads the `isReadOnly` FIELD by name, and that field
/// is the whole answer for `HeapByteBufferR` and friends, whose constructors
/// set it. It is NOT the answer for the `ByteBufferAs<T>Buffer R{B,L}` family —
/// the views `ByteBuffer.as<T>Buffer().asReadOnlyBuffer()` returns — which
/// leave the field FALSE and override `isReadOnly()` to return `true`. Measured
/// on Temurin 25.0.4+7:
///
/// ```text
/// viewRo.isReadOnly()                 HotSpot true    this VM true
/// IntBuffer.isReadOnly (the FIELD)    HotSpot false   this VM false
/// viewRo.put(new int[]{5}, 0, 1)      HotSpot ReadOnlyBufferException
///                                     this VM no-throw, and bb.getInt(0) == 5
/// ```
///
/// **A silent write through a read-only handle, into the caller's own
/// `ByteBuffer`.** It reached exactly one door and no other: the JDK's
/// read-only view classes DECLARE `put(int)` and `put(int,int)` — so those
/// dispatch to their own bodies and refuse — and do NOT declare
/// `put(int[],int,int)`, whose most-derived declaration is on the abstract
/// `IntBuffer` this crate registers against. *The door asks about the
/// DECLARING class*, so one of a family's three `put` overloads was ours and
/// two were the JDK's, which is why the scalar row passed and the bulk row did
/// not.
///
/// The second question is one virtual call and is asked only of a receiver this
/// crate did not allocate; our own carriers carry the flag in the field and
/// answer before it.
///
/// gc-common w13-d: `buf` is `&mut` because that virtual call runs Java, and
/// every caller goes on to read and write the receiver; it is refreshed here
/// (the `&mut ObjectRef` shape `scripts/stale-receiver-audit.py` asks for).
pub fn s2_buf_read_only(ctx: &mut dyn NativeContext, buf: &mut ObjectRef) -> bool {
    if s2_bb_is_read_only(ctx, *buf) {
        return true;
    }
    if s2_is_our_buffer_carrier(ctx, *buf) {
        return false;
    }
    let receiver = *buf;
    keep_one_across(ctx, buf, |ctx| {
        matches!(
            ctx.invoke_virtual(receiver, "isReadOnly", "()Z", &[]),
            Ok(Some(Value::Int(v))) if v != 0
        )
    })
}

/// The WRITE half of [`s2_bb_is_read_only`], for the four view producers.
///
/// `slice()`, `slice(int,int)` and `duplicate()` inherit the source's flag and
/// `asReadOnlyBuffer()` sets it unconditionally — MEASURED across all seven
/// buffer families on jdk-25.0.3+9
/// (`scratchpad/f21/F21ViewContagionProbe.java`); read-only-ness is contagious
/// and `asReadOnlyBuffer()` is one-way with no operation anywhere that clears
/// it. Each of those four had heap and direct arms that carried `ro`
/// correctly and a copying fallback arm that did not, and the fallback runs
/// `bb_write_hb`, which writes `isReadOnly = 0` — so the flag was actively
/// cleared rather than merely left alone. This exists so the corrected arms
/// name the operation instead of open-coding a fifth `set_field_by_name`.
///
/// Deliberately by NAME and not through `BB_*`: `isReadOnly` is a real-JDK
/// `ByteBuffer` field with no slot in the bare 6-slot synthetic layout, so on a
/// carrier that has no such field this is a no-op — which is the same
/// behaviour, and the same reason, as `bb_write_hb`'s own `isReadOnly` write
/// one screen up.
#[inline]
pub fn s2_bb_set_read_only(ctx: &mut dyn NativeContext, buf: ObjectRef, read_only: bool) {
    // By name, plus the `--synthetic-jdk` stub's flag slot (interpreter round
    // i5 wave 105, lane L5; `native_io::write_buffer_read_only`).
    cratonvm_native_io::write_buffer_read_only(ctx, buf, read_only);
}

/// Does `buf` keep its state in the indexed `BB_*` slots? The bare 6-slot
/// synthetic layout ([`s2_bb_synthetic_layout`]) or a `--synthetic-jdk` stub
/// ([`s2_buf_stub_layout`]: the fabricated 10-slot `java/nio/ByteBuffer`, whose
/// slots 0..5 are that same layout and whose by-name writes are no-ops).
///
/// Interpreter round i5 wave 105, lane L5: the view and allocation builders
/// below gated their indexed writes on [`s2_bb_synthetic_layout`] alone, so on
/// the stub every by-name write was dropped and the indexed ones skipped:
/// `ByteBuffer.wrap(b).asReadOnlyBuffer()` came back with no backing storage,
/// and its first `get(byte[])` aborted the VM with `internal error: ByteBuffer
/// missing backing storage` (probe `L5W101DefineClassByteBufferSynthetic`,
/// row `heap-readonly`, `--synthetic-jdk` only; every real-JDK class declares
/// `position`, so the stub arm cannot match there).
#[inline]
pub fn s2_bb_indexed_layout(ctx: &dyn NativeContext, buf: ObjectRef) -> bool {
    s2_bb_synthetic_layout(ctx, buf) || s2_buf_stub_layout(ctx, buf)
}

/// `throw new UnsupportedOperationException()` — the NO-ARGUMENT constructor,
/// so `getMessage()` is null exactly as HotSpot's is.
///
/// The `array()` / `arrayOffset()` arms below raised this with a
/// `"direct buffer has no backing array"` detail message. The CLASS was right,
/// so no `catch` and no class-name assertion could see the difference — only a
/// message-exact differential can, which is why it survived W7-83 §7.1.
/// MEASURED on `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`, all four UOE
/// arms of this family report `getMessage() == null`:
///
/// ```text
/// directByteBuffer.array        java.lang.UnsupportedOperationException  getMessage=null
/// directByteBuffer.arrayOffset  java.lang.UnsupportedOperationException  getMessage=null
/// charView.array                java.lang.UnsupportedOperationException  getMessage=null
/// stringCharBuffer.array        java.lang.UnsupportedOperationException  getMessage=null
/// new UOE().getMessage          OK null
/// ```
///
/// The EMPTY string is the documented spelling for "no message" in
/// `types/src/error.rs`, which maps `""` to `None` so the `()V` ctor is used;
/// `Some("")` would set a non-null empty detail message. Same spelling as
/// `charset_buffers.rs::cb_no_backing_array` and
/// `native-io/src/lib.rs::buffer_no_backing_array` — this WAS the third and
/// last copy in the family. Record: F14-1 N3.
///
/// CONVERGED (F14-1 N1): now a delegation to the `native-io` copy, for the same
/// dependency-edge reason as [`s2_bb_is_read_only`]. The empty-string spelling
/// is precisely the detail a fourth transcription would get wrong, and the
/// existing test on this arm matches
/// `RuntimeError::UnsupportedOperationException { .. }`, which is blind to the
/// message by construction — the message-reading test lives beside the single
/// remaining body.
#[inline]
pub fn s2_bb_no_backing_array() -> MethodCallFailed {
    cratonvm_native_io::buffer_no_backing_array()
}

/// The canonical `java.nio.ByteOrder` object for `ord` (0=BIG_ENDIAN,
/// 1=LITTLE_ENDIAN). Prefers the REAL class statics — ensuring the class
/// is initialized first, since callers like `buffer.order()` can run
/// before any Java-side `ByteOrder` access — so identity comparisons
/// (`order() == ByteOrder.LITTLE_ENDIAN`) and `toString()` behave exactly
/// like HotSpot. Falls back to a 1-slot synthetic (field 0 = order int)
/// only when the real class/statics are unavailable (synthetic-jdk mode).
///
/// G38-1: the FALLBACK arm was the unrepaired twin of
/// `phases_late::foreign_ffm::p67_byte_order_object`. Reaching it does not
/// prove the real class is absent — `ensure_class_initialized` can succeed and
/// `static_field_index_by_name`/`get_static_field` still miss (a class loaded
/// but whose `<clinit>` has not published the constants yet, and every
/// compatibility-mode arm where a synthetic registrar runs against real class
/// bytes). On that path `alloc_concurrent_synthetic` hands back an object of
/// the REAL `java.nio.ByteOrder`, whose only instance field is
/// `private final String name` at slot 0 — so `Value::Int(ord)` was silently
/// coerced to `null` by `heap::coerce_field_value_by_descriptor`
/// (G30-1-the-silent-reference-slot-coercion-20260817.md), and BOTH readers
/// below then decoded the null as BIG_ENDIAN: `LITTLE_ENDIAN.toString()`
/// printed `BIG_ENDIAN` and `equals` called the two constants equal.
///
/// The repair is the correct value in the right slot, not a refusal. The flag
/// write stays — it is the synthetic-stub layout, and `s2_byte_order_ord`
/// still falls back to it — and on top of it, when the CLASS actually declares
/// `name` at a slot this object has, a genuine String goes there. Three
/// shapes, all covered, exactly as in `p67_byte_order_object`: a fabricated
/// stub names its fields `_f0..`, so `name` does not resolve and only the flag
/// lands (byte-identical to before); a real `java.nio.ByteOrder` resolves
/// `name` to slot 0 and gets the String, which `toString`/`s2_byte_order_ord`
/// already decode; a resolved index out of range is skipped rather than
/// written out of bounds.
pub fn s2_byte_order_object(
    ctx: &mut dyn NativeContext,
    ord: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let cid = ctx
        .ensure_class_initialized("java/nio/ByteOrder")
        .ok()
        .or_else(|| ctx.class_id_by_name("java/nio/ByteOrder"));
    if let Some(cid) = cid {
        let field = if ord == 1 {
            "LITTLE_ENDIAN"
        } else {
            "BIG_ENDIAN"
        };
        if let Some(idx) = ctx.static_field_index_by_name(cid, field) {
            if let Value::Object(Some(o)) = ctx.get_static_field(cid, idx) {
                return Ok(o);
            }
        }
    }
    let bo = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteOrder", 1)?;
    ctx.set_field(bo, 0, Value::Int(ord));
    let bo_cid = ctx.class_id_of_object(bo);
    // Bound before the `if let` so the immutable reborrow of `ctx` ends here
    // rather than spanning the block that needs `&mut ctx`.
    let name_slot = ctx.resolve_field_index_by_class_id(bo_cid, "name");
    if let Some(slot) = name_slot {
        if slot < ctx.object_num_fields(bo) {
            // `create_string` allocates and can move `bo` (native stale-local
            // family) — pin it across the call.
            let bo_pin = ctx.pin_native_root(bo);
            let name = ctx.create_string(if ord == 1 {
                "LITTLE_ENDIAN"
            } else {
                "BIG_ENDIAN"
            });
            let bo = ctx.read_native_pin(bo_pin, bo);
            ctx.unpin_native_roots(bo_pin);
            ctx.set_field(bo, slot, Value::Object(Some(name)));
            return Ok(bo);
        }
    }
    Ok(bo)
}

/// Write a buffer's `position`. Buffers with a heap array keep this
/// family's historic `BB_POS` slot write (self-consistent with `s2_bb_pos`
/// on both real heap and synthetic buffers). DIRECT buffers are otherwise
/// managed by name-based natives whose layout the `BB_POS` slot is not
/// guaranteed to match — write their `position` by name.
pub fn s2_bb_set_pos(ctx: &mut dyn NativeContext, buf: ObjectRef, v: i32) {
    if s2_bb_arr(ctx, buf).is_some() {
        ctx.set_field(buf, BB_POS, Value::Int(v));
    } else {
        ctx.set_field_by_name(buf, "position", Value::Int(v));
    }
}

/// Bulk `byte[]` → `byte[]` copy for the ByteBuffer natives, through the VM's
/// `memcpy` intrinsics instead of a per-element accessor loop.
///
/// Every heap↔heap arm of `get([B)`, `get([BII)`, `put([B)`, `put([BII)` and
/// `put(Ljava/nio/ByteBuffer;)` used to move one byte per `get_array_element` /
/// `set_array_element` call, i.e. two dynamic accessor calls per byte. That is
/// what `native-api`'s own doc comment on `write_byte_array_from` calls out as
/// the migration these callers were waiting for, and it is the whole of
/// `TestAsyncMessagesPerformance`'s inter-chunk gap: the WebSocket client moves
/// ~32 KiB through these five methods for every 8 KiB partial message it
/// delivers (socket → `response` → `inputBuffer` → `messageBufferBinary` →
/// the defensive `copy` handed to `onMessage`).
/// See `32-doc04-residual-perf-assertions-CLOSED.md` §32.3.
///
/// Returns `false` — having written nothing — when either intrinsic declines
/// (non-byte array kind, or bounds it refuses); the caller must then fall back
/// to the element loop. Copying via an owned buffer also makes an overlapping
/// same-array copy well defined, which the element loop was not.
pub fn s2_bb_bulk_array_copy(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    src_off: usize,
    dst: ObjectRef,
    dst_off: usize,
    len: usize,
) -> bool {
    if len == 0 {
        return true;
    }
    let mut buf = vec![0u8; len];
    if ctx.read_byte_array_into(src, src_off, &mut buf) != len {
        return false;
    }
    ctx.write_byte_array_from(dst, dst_off, &buf)
}

pub fn s2_bb_get_byte(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i8 {
    // B8: a negative `idx` (overflowed/garbage index from Java bytecode)
    // would become a huge `usize` and either panic or read out of bounds.
    // Bound-check explicitly here: a negative or out-of-range index reads
    // back 0 (the JDK would throw IndexOutOfBoundsException; our synthetic
    // path stays panic-free and returns a benign zero byte instead).
    if idx < 0 {
        return 0;
    }
    if let Some((arr, base)) = s2_bb_heap_window(ctx, buf) {
        let i = base.saturating_add(idx as usize);
        if i >= ctx.array_length(arr) {
            return 0;
        }
        ctx.get_array_element(arr, i).as_int().unwrap_or(0) as i8
    } else if let Some(addr) = s2_bb_direct_addr(ctx, buf) {
        // DIRECT buffer (no heap array): fall back to native-memory
        // access, mirroring `put(Ljava/nio/ByteBuffer;)`'s direct-buffer
        // fix. Reached whenever an s2-forced native touches a genuine
        // direct receiver that real-JDK `DirectByteBuffer`'s own
        // overridden bytecode didn't intercept first (slices/duplicates
        // of a direct buffer, typed views over one, etc). This helper has
        // no `MethodCallResult` to propagate a Java exception through (it
        // is a private byte-level primitive called from ~15 sites), so a
        // failed native-memory read stays panic-free and benign (0),
        // matching this function's existing out-of-range convention —
        // callers that DO have a `MethodCallResult` (the bulk get/put
        // registrations below) throw `IllegalStateException` instead.
        let mut b = [0u8; 1];
        if ctx.copy_from_native_memory(addr.saturating_add(idx as i64), &mut b) {
            b[0] as i8
        } else {
            0
        }
    } else {
        0
    }
}

pub fn s2_bb_put_byte(ctx: &mut dyn NativeContext, buf: ObjectRef, idx: i32, b: i8) {
    // B8: mirror the read-side bound check — a negative or out-of-range
    // index is silently dropped rather than panicking / clobbering memory.
    if idx < 0 {
        return;
    }
    if let Some((arr, base)) = s2_bb_heap_window(ctx, buf) {
        let i = base.saturating_add(idx as usize);
        if i >= ctx.array_length(arr) {
            return;
        }
        ctx.set_array_element(arr, i, Value::Int(b as i32));
    } else if let Some(addr) = s2_bb_direct_addr(ctx, buf) {
        // DIRECT buffer: mirror the get-side fallback above. Best-effort —
        // see `s2_bb_get_byte`'s comment for why this stays panic-free
        // and benign (silently drops the write) rather than surfacing a
        // Java exception from this deep a helper.
        let _ = ctx.copy_to_native_memory(addr.saturating_add(idx as i64), &[b as u8]);
    }
}

/// Remaining bytes (pos..limit) as Vec<u8> without advancing position.
/// Storage-aware: heap (honouring the array-base offset) and direct
/// buffers both work; storage-less synthetics stay an empty Vec.
pub fn s2_bb_remaining_bytes(ctx: &dyn NativeContext, buf: ObjectRef) -> Vec<u8> {
    let pos = s2_bb_pos(ctx, buf).max(0);
    let lim = s2_bb_limit(ctx, buf).max(pos);
    s2_bb_read_window(ctx, buf, pos, (lim - pos) as usize).unwrap_or_default()
}

/// B8: compute `idx + off` for a multi-byte ByteBuffer access without
/// overflowing. A wrap (in release) or panic (in debug) is reachable when
/// Java bytecode hands us an `idx` near `i32::MAX`. On overflow we return a
/// negative sentinel, which `s2_bb_get_byte` / `s2_bb_put_byte` treat as
/// out-of-range (read 0 / drop the write) — matching the benign behaviour of
/// a genuine bounds miss.
#[inline]
pub fn s2_bb_off(idx: i32, off: i32) -> i32 {
    // A negative starting index is already out of range; keep it negative (the
    // out-of-range sentinel) instead of letting `idx + off` cross zero into a
    // valid-looking positive offset. Overflow on a valid index saturates to -1.
    if idx < 0 {
        return -1;
    }
    idx.checked_add(off).unwrap_or(-1)
}

/// B8: byte offset of int-unit index `unit` (relative to byte-start `bs`),
/// i.e. `bs + unit * 4`, computed without overflow. Used by the IntBuffer
/// view get/put. Overflow saturates to a negative sentinel so the byte
/// accessors treat it as out-of-range.
#[inline]
pub fn s2_bb_int_byte_off(bs: i32, unit: i32) -> i32 {
    unit.checked_mul(4)
        .and_then(|b| bs.checked_add(b))
        .unwrap_or(-1)
}

/// Read `N` bytes starting at logical index `idx`, resolving the buffer's
/// storage ONCE.
///
/// The six multi-byte accessors below used to call [`s2_bb_get_byte`] /
/// [`s2_bb_put_byte`] once per byte, and each of those re-resolves the whole
/// storage view from scratch — `get_field_by_name(buf, "hb")`, then `"offset"`,
/// then possibly `"address"` — i.e. up to three NAME-keyed field lookups per
/// byte. A `putLong` therefore paid that resolution EIGHT times.
///
/// **This was written as a throughput fix and it is NOT one — recorded here so
/// the next reader does not re-derive the same wrong hypothesis.** Interleaved
/// before/after on `probes/NioAccessorRate.java` (2026-08-17, real-JDK mode,
/// G1, two rounds), `direct ByteBuffer.putLong` measured 918 -> 943 and
/// 864 -> 877 ns/op: no change outside the noise. The per-byte re-resolution
/// was real, but it is not what the time goes to.
///
/// What the time actually goes to is the NATIVE CALL itself. On the same
/// probe a single-byte `ByteBuffer.put(int,byte)` — one native, one stored
/// byte — costs ~260 ns against HotSpot's 0.29, and `putLong` is ~3.5x that
/// rather than 8x, which is the shape of "one native call plus a few
/// name-keyed field reads", not "eight byte stores". See
/// `httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md`
/// for the full decomposition and for why this is what makes
/// `testZipBomb` exceed its wall.
///
/// The rewrite is kept because it is strictly less work per access and it is
/// pinned by a differential oracle (`probes/NioAccessorOracle.java`, which
/// checksums every width x endianness x storage-kind x alignment plus the
/// out-of-range and read-only contracts, and must print the same TOTAL on
/// HotSpot and CratonVM). It is not kept on the strength of a measurement.
///
/// Semantics are preserved exactly, including the benign out-of-range
/// behaviour this family documents: a read past the array reads back zero for
/// the bytes that are out of range, and a write past it drops them, rather
/// than throwing from this deep a helper. Only the number of storage
/// resolutions changes.
pub fn s2_bb_read_n<const N: usize>(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> [u8; N] {
    let mut out = [0u8; N];
    if idx < 0 {
        return out;
    }
    match s2_bb_storage(ctx, buf) {
        Some(S2BbStorage::Heap { arr, base }) => {
            let Some(start) = base.checked_add(idx as usize) else {
                return out;
            };
            let len = ctx.array_length(arr);
            for (k, slot) in out.iter_mut().enumerate() {
                let Some(i) = start.checked_add(k) else { break };
                if i >= len {
                    break;
                }
                *slot = ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8;
            }
        }
        Some(S2BbStorage::Direct { addr }) => {
            // One bulk copy; on failure every byte keeps this family's benign
            // zero, exactly as the per-byte helper's `else { 0 }` arm did.
            if !ctx.copy_from_native_memory(addr.saturating_add(idx as i64), &mut out) {
                out = [0u8; N];
            }
        }
        None => {}
    }
    out
}

/// Write side of [`s2_bb_read_n`] — same single-resolution contract, same
/// benign drop-on-out-of-range behaviour.
pub fn s2_bb_write_n<const N: usize>(
    ctx: &mut dyn NativeContext,
    buf: ObjectRef,
    idx: i32,
    bytes: [u8; N],
) {
    if idx < 0 {
        return;
    }
    match s2_bb_storage(ctx, buf) {
        Some(S2BbStorage::Heap { arr, base }) => {
            let Some(start) = base.checked_add(idx as usize) else {
                return;
            };
            let len = ctx.array_length(arr);
            for (k, &b) in bytes.iter().enumerate() {
                let Some(i) = start.checked_add(k) else {
                    return;
                };
                if i >= len {
                    return;
                }
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
        }
        Some(S2BbStorage::Direct { addr }) => {
            let _ = ctx.copy_to_native_memory(addr.saturating_add(idx as i64), &bytes);
        }
        None => {}
    }
}

pub fn s2_bb_read2(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i16 {
    let bs = s2_bb_read_n::<2>(ctx, buf, idx);
    if s2_bb_order(ctx, buf) == 1 {
        i16::from_le_bytes(bs)
    } else {
        i16::from_be_bytes(bs)
    }
}

pub fn s2_bb_write2(ctx: &mut dyn NativeContext, buf: ObjectRef, idx: i32, val: i16) {
    let bytes = if s2_bb_order(ctx, buf) == 1 {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    s2_bb_write_n::<2>(ctx, buf, idx, bytes);
}

pub fn s2_bb_read4(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i32 {
    let bs = s2_bb_read_n::<4>(ctx, buf, idx);
    if s2_bb_order(ctx, buf) == 1 {
        i32::from_le_bytes(bs)
    } else {
        i32::from_be_bytes(bs)
    }
}

pub fn s2_bb_write4(ctx: &mut dyn NativeContext, buf: ObjectRef, idx: i32, val: i32) {
    let bytes = if s2_bb_order(ctx, buf) == 1 {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    s2_bb_write_n::<4>(ctx, buf, idx, bytes);
}

pub fn s2_bb_read8(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i64 {
    let bs = s2_bb_read_n::<8>(ctx, buf, idx);
    if s2_bb_order(ctx, buf) == 1 {
        i64::from_le_bytes(bs)
    } else {
        i64::from_be_bytes(bs)
    }
}

pub fn s2_bb_write8(ctx: &mut dyn NativeContext, buf: ObjectRef, idx: i32, val: i64) {
    let bytes = if s2_bb_order(ctx, buf) == 1 {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    s2_bb_write_n::<8>(ctx, buf, idx, bytes);
}

// ---- Socket address helper -------------------------------------------------

pub fn s2_parse_socket_addr(ctx: &dyn NativeContext, addr: ObjectRef) -> Option<(String, u16)> {
    // Real-JDK 25 InetSocketAddress lays its host / port behind a `holder`
    // chain: `InetSocketAddress.holder.{hostname, addr, port}`. The synthetic
    // 2-field layout (host string at 0, port int at 1) is the legacy probe
    // shape. Try the real-JDK shape first, then the synthetic.
    if let Value::Object(Some(h)) = ctx.get_field_by_name(addr, "holder") {
        let port = match ctx.get_field_by_name(h, "port") {
            Value::Int(p) => p as u16,
            _ => 0,
        };
        // hostname is set when the address was built via
        // `InetSocketAddress(String, int)`; otherwise we fall back to the
        // wildcard so a `bind(null, port)` matches HotSpot's behaviour of
        // listening on all interfaces.
        let host = match ctx.get_field_by_name(h, "hostname") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let host = if !host.is_empty() {
            host
        } else if let Value::Object(Some(ia)) = ctx.get_field_by_name(h, "addr") {
            // InetAddress.holder.hostName fallback for
            // `InetSocketAddress(InetAddress, int)` callers.
            match ctx.get_field_by_name(ia, "holder") {
                Value::Object(Some(iah)) => match ctx.get_field_by_name(iah, "hostName") {
                    Value::Object(Some(s)) => {
                        ctx.read_string(s).unwrap_or_else(|| "0.0.0.0".into())
                    }
                    _ => "0.0.0.0".into(),
                },
                _ => "0.0.0.0".into(),
            }
        } else {
            "0.0.0.0".into()
        };
        return Some((host, port));
    }

    // Synthetic 2-field fallback.
    let host = match ctx.get_field(addr, 0) {
        Value::Object(Some(h)) => ctx.read_string(h)?,
        _ => return None,
    };
    let port = ctx.get_field(addr, 1).as_int()? as u16;
    Some((host, port))
}

// ---- NEW-3: Cross-platform poll abstraction --------------------------------
//
// Unix exposes `poll(2)` via `libc::poll`, which operates on an array of
// `libc::pollfd` (fd = `c_int`). Windows' `libc` crate does NOT expose
// `WSAPoll` or `WSAPOLLFD`, so for Windows we bind the raw symbols from
// Ws2_32.dll directly below. Both platforms are then hidden behind the
// platform-neutral `PollReq` / `selector_poll` abstraction so the rest of
// this module stays cfg-free.
//
// We use the following bit definitions for `PollReq::events`:
//   POLL_IN  = readable (analogous to POLLIN / POLLRDNORM)
//   POLL_OUT = writable (analogous to POLLOUT / POLLWRNORM)
//   POLL_ERR = error bit (analogous to POLLERR)
//   POLL_HUP = hang-up bit (analogous to POLLHUP)
// These are translated to the platform-specific constants by the
// dispatcher below.

pub const POLL_IN: i16 = 0x1;
pub const POLL_OUT: i16 = 0x2;
pub const POLL_ERR: i16 = 0x4;
pub const POLL_HUP: i16 = 0x8;

#[derive(Clone, Copy, Debug)]
pub struct PollReq {
    /// Raw OS handle, stored as i64 to fit `c_int` on Unix and `SOCKET` on
    /// Windows without loss. The dispatcher casts it back to the platform
    /// type before invoking the OS poll.
    pub fd: i64,
    /// Bitwise-OR of `POLL_IN` / `POLL_OUT`.
    pub events: i16,
}

/// Run the OS poll on a slice of `PollReq`. Returns a Vec of revents
/// (neutral bits — POLL_IN / POLL_OUT / POLL_ERR / POLL_HUP) in the same
/// order as the input, or an empty Vec if the OS poll itself failed or
/// the input was empty.
pub fn selector_poll(reqs: &[PollReq], timeout_ms: i32) -> Vec<i16> {
    if reqs.is_empty() {
        return Vec::new();
    }

    #[cfg(unix)]
    {
        // Translate neutral events → POLLIN/POLLOUT.
        let mut pfds: Vec<libc::pollfd> = reqs
            .iter()
            .map(|r| {
                let mut ev: libc::c_short = 0;
                if r.events & POLL_IN != 0 {
                    ev |= libc::POLLIN;
                }
                if r.events & POLL_OUT != 0 {
                    ev |= libc::POLLOUT;
                }
                libc::pollfd {
                    fd: r.fd as libc::c_int,
                    events: ev,
                    revents: 0,
                }
            })
            .collect();

        // SAFETY: pfds is a valid &mut slice with correct layout; libc
        // reads it and writes only the revents fields.
        let ret = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
        if ret < 0 {
            return Vec::new();
        }
        pfds.iter()
            .map(|p| {
                let mut rev: i16 = 0;
                if p.revents & libc::POLLIN != 0 {
                    rev |= POLL_IN;
                }
                if p.revents & libc::POLLOUT != 0 {
                    rev |= POLL_OUT;
                }
                if p.revents & libc::POLLERR != 0 {
                    rev |= POLL_ERR;
                }
                if p.revents & libc::POLLHUP != 0 {
                    rev |= POLL_HUP;
                }
                rev
            })
            .collect()
    }

    #[cfg(windows)]
    {
        // Direct FFI to WSAPoll — the `libc` crate does not re-export it
        // on Windows. WSAPOLLFD is defined in winsock2.h / mswsock.h.
        #[repr(C)]
        struct Wsapollfd {
            fd: usize, // SOCKET = UINT_PTR, usize on both 32/64-bit Win
            events: i16,
            revents: i16,
        }
        // Winsock event bit constants (from winsock2.h).
        const WSAPOLLRDNORM: i16 = 0x0100;
        const WSAPOLLWRNORM: i16 = 0x0010;
        const WSAPOLLERR: i16 = 0x0001;
        const WSAPOLLHUP: i16 = 0x0002;

        #[link(name = "Ws2_32")]
        extern "system" {
            fn WSAPoll(fd_array: *mut Wsapollfd, fds: u32, timeout: i32) -> i32;
        }

        let mut pfds: Vec<Wsapollfd> = reqs
            .iter()
            .map(|r| {
                let mut ev: i16 = 0;
                if r.events & POLL_IN != 0 {
                    ev |= WSAPOLLRDNORM;
                }
                if r.events & POLL_OUT != 0 {
                    ev |= WSAPOLLWRNORM;
                }
                Wsapollfd {
                    fd: r.fd as usize,
                    events: ev,
                    revents: 0,
                }
            })
            .collect();

        // SAFETY: pfds is a valid &mut slice with correct layout; WSAPoll
        // only reads the events field and writes the revents field.
        let ret = unsafe { WSAPoll(pfds.as_mut_ptr(), pfds.len() as u32, timeout_ms) };
        if ret < 0 {
            return Vec::new();
        }
        pfds.iter()
            .map(|p| {
                let mut rev: i16 = 0;
                if p.revents & WSAPOLLRDNORM != 0 {
                    rev |= POLL_IN;
                }
                if p.revents & WSAPOLLWRNORM != 0 {
                    rev |= POLL_OUT;
                }
                if p.revents & WSAPOLLERR != 0 {
                    rev |= POLL_ERR;
                }
                if p.revents & WSAPOLLHUP != 0 {
                    rev |= POLL_HUP;
                }
                rev
            })
            .collect()
    }
}

#[cfg(unix)]
pub fn stream_pollreq_fd(stream: &TcpStream) -> i64 {
    use std::os::unix::io::AsRawFd;
    stream.as_raw_fd() as i64
}

#[cfg(windows)]
pub fn stream_pollreq_fd(stream: &TcpStream) -> i64 {
    use std::os::windows::io::AsRawSocket;
    stream.as_raw_socket() as i64
}

#[cfg(unix)]
pub fn listener_pollreq_fd(listener: &TcpListener) -> i64 {
    use std::os::unix::io::AsRawFd;
    listener.as_raw_fd() as i64
}

#[cfg(windows)]
pub fn listener_pollreq_fd(listener: &TcpListener) -> i64 {
    use std::os::windows::io::AsRawSocket;
    listener.as_raw_socket() as i64
}

#[cfg(unix)]
pub fn dgram_pollreq_fd(sock: &UdpSocket) -> i64 {
    use std::os::unix::io::AsRawFd;
    sock.as_raw_fd() as i64
}

#[cfg(windows)]
pub fn dgram_pollreq_fd(sock: &UdpSocket) -> i64 {
    use std::os::windows::io::AsRawSocket;
    sock.as_raw_socket() as i64
}

// ---- NEW-3: Per-selector wakeup channel ------------------------------------
//
// Each `Selector` gets a lazily-constructed, self-connected `UdpSocket` that
// is added to the `pollfds` array on every `select()` call. `Selector.wakeup`
// writes one byte to this socket; the poll wakes up, the byte is drained
// from the receive queue, and the function returns promptly. The selector's
// identity (used as the map key) is its stable Java identity hash plus VM
// identity. Raw ObjectRef pointer bits are not safe here because a moving GC
// can relocate a selector while another thread is blocked in select(). Entries
// are removed when the Java `Selector.close()` runs.
//
// The UDP socket is bound to 127.0.0.1:0, which the OS fills in with an
// ephemeral port. We then call `connect()` on the same socket back to its
// own `local_addr()` so that `send(&[byte])` loops the byte right back into
// the socket's recv queue. This works identically on Unix and Windows with
// zero platform-specific code paths — UDP send-to-self is a well-defined
// Berkeley-socket operation that every major OS implements.

pub struct WakeupChannel {
    pub socket: UdpSocket,
}

/// `(vm, the selector's weak lock key)`.
///
/// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 29): the selector half used to be its identity hash. Two live
/// selectors of one VM can share one, and then they shared ONE wakeup socket:
/// `wakeup()` on one woke the other, and a `select()` on either drained the
/// other's pending wakeup byte, so a real wakeup could be lost. A weak lock key
/// is unique per live selector, and a selector that dies unclosed now takes
/// its socket with it ([`forget_selector_wakeup_keys`]); it used to hold the
/// OS socket until its VM went away.
pub type SelectorWakeupKey = (usize, usize);

pub static SELECTOR_WAKEUPS: OnceLock<parking_lot::Mutex<HashMap<SelectorWakeupKey, WakeupChannel>>> =
    OnceLock::new();

pub fn selector_wakeups() -> &'static parking_lot::Mutex<HashMap<SelectorWakeupKey, WakeupChannel>> {
    SELECTOR_WAKEUPS.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Produce a stable identity key for the selector, minting its weak lock key
/// on first use. Computed before the table lock, never under it.
pub fn selector_key(ctx: &dyn NativeContext, sel: ObjectRef) -> SelectorWakeupKey {
    let key = crate::gc_stable_weak_lock_key(ctx, sel)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"));
    (ctx.vm_identity(), key)
}

/// Drop the wakeup channels of selectors whose weak lock keys were freed (the
/// selector died unclosed, or its VM is going). The sockets close AFTER the
/// table guard drops (lesson x). Called by
/// `lib.rs::{sweep_lock_keys, forget_vm_lock_keys}`. gc-common w29-d.
pub fn forget_selector_wakeup_keys(freed: &[usize]) {
    if freed.is_empty() {
        return;
    }
    let closed: Vec<WakeupChannel> = {
        let mut map = selector_wakeups().lock();
        if map.is_empty() {
            return;
        }
        let freed: std::collections::HashSet<usize> = freed.iter().copied().collect();
        let dead: Vec<SelectorWakeupKey> = map
            .keys()
            .filter(|(_, key)| freed.contains(key))
            .copied()
            .collect();
        dead.iter().filter_map(|key| map.remove(key)).collect()
    };
    drop(closed);
}

/// Construct a new self-connected UDP socket for wakeup signaling. Returns
/// `None` if the OS refuses to give us a loopback socket, in which case the
/// caller falls back to a plain blocking poll without wakeup support (which
/// can still return via the user-supplied timeout).
pub fn build_wakeup_channel() -> Option<WakeupChannel> {
    let socket = UdpSocket::bind("127.0.0.1:0").ok()?;
    let addr = socket.local_addr().ok()?;
    // Non-blocking so the drain read after poll() never stalls.
    socket.set_nonblocking(true).ok()?;
    socket.connect(addr).ok()?;
    Some(WakeupChannel { socket })
}

/// Get the wakeup channel for this selector, creating one lazily if needed.
/// Returns `None` if construction failed (e.g. loopback unavailable).
pub fn ensure_wakeup_channel<F, T>(ctx: &dyn NativeContext, sel: ObjectRef, f: F) -> Option<T>
where
    F: FnOnce(&WakeupChannel) -> T,
{
    let key = selector_key(ctx, sel);
    let mut map = selector_wakeups().lock();
    if !map.contains_key(&key) {
        let ch = build_wakeup_channel()?;
        map.insert(key, ch);
    }
    map.get(&key).map(f)
}

/// Drop the wakeup channel (called by `Selector.close()`). A selector that was
/// never keyed has no channel, so nothing is minted here.
pub fn release_wakeup_channel(ctx: &dyn NativeContext, sel: ObjectRef) {
    let Some(key) = crate::existing_weak_lock_key(ctx, sel) else {
        return;
    };
    let closed = selector_wakeups().lock().remove(&(ctx.vm_identity(), key));
    // The socket closes after the guard drops (lesson x).
    drop(closed);
}

/// Signal the wakeup channel. Called by `Selector.wakeup()`.
/// Writes a single byte; the blocked `poll()` observes POLLIN on the
/// wakeup fd, returns, and drains the byte.
pub fn signal_wakeup(ctx: &dyn NativeContext, sel: ObjectRef) {
    ensure_wakeup_channel(ctx, sel, |ch| {
        // Best-effort: if the socket buffer is full (caller wake'd N times
        // without any poll clearing it), the send fails and we silently
        // proceed — the byte that is already pending is enough to wake
        // the next poll.
        let _ = ch.socket.send(&[1u8]);
    });
}

/// Drain any queued wakeup bytes on the wakeup socket. Called after every
/// `poll()` return so the next `select()` does not spuriously wake on a
/// leftover byte.
pub fn drain_wakeup(ctx: &dyn NativeContext, sel: ObjectRef) {
    ensure_wakeup_channel(ctx, sel, |ch| {
        let mut buf = [0u8; 64];
        loop {
            match ch.socket.recv(&mut buf) {
                Ok(n) if n > 0 => continue, // keep draining
                _ => break,                 // empty / would block / closed
            }
        }
    });
}

// ---- Selector poll ---------------------------------------------------------

/// Poll with a timeout in milliseconds.
///
/// Semantics (matches `java.nio.channels.Selector`):
///   * `timeout_ms < 0`  — block indefinitely (caller uses -1)
///   * `timeout_ms == 0` — non-blocking poll, return immediately
///   * `timeout_ms > 0`  — block for at most `timeout_ms` milliseconds
///
/// The underlying `libc::poll` (`poll(2)` on Unix, `WSAPoll` on Windows)
/// uses the same contract directly. This function also registers the
/// per-selector wakeup fd into the pollfd array, so `Selector.wakeup()`
/// from another thread can interrupt a blocking call.
pub fn s2_selector_do_poll_with_timeout(
    ctx: &mut dyn NativeContext,
    sel: ObjectRef,
    timeout_ms: i32,
) -> i32 {
    let mut sel = sel;
    let n = ctx.get_field(sel, S2SEL_NKEYS).as_int().unwrap_or(0) as usize;
    let keys_arr = match ctx.get_field(sel, S2SEL_KEYS) {
        Value::Object(Some(arr)) => arr,
        _ => {
            // No registered keys yet; honor the wakeup + timeout contract
            // anyway so that `select(timeout)` on an empty selector sleeps
            // correctly instead of spinning.
            return poll_empty_selector(ctx, sel, timeout_ms);
        }
    };
    if n == 0 {
        return poll_empty_selector(ctx, sel, timeout_ms);
    }

    // Gather channel info for each registered key.
    struct PollEntry {
        key_ref: ObjectRef,
        channel_ref: ObjectRef,
        interest: i32,
        sock_id: i32,
        listener_id: i32,
        dgram_id: i32,
    }
    let mut entries: Vec<PollEntry> = Vec::with_capacity(n);
    for i in 0..n {
        let key = match ctx.get_array_element(keys_arr, i) {
            Value::Object(Some(k)) => k,
            _ => continue,
        };
        let interest = ctx.get_field(key, 2).as_int().unwrap_or(0);
        let channel = match ctx.get_field(key, 0) {
            Value::Object(Some(ch)) => ch,
            _ => continue,
        };
        let sock_id = ctx.get_field(channel, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        let listener_id = ctx
            .get_field(channel, S2SSC_LISTENER_ID)
            .as_int()
            .unwrap_or(-1);
        let dgram_id = ctx.get_field(channel, S2DC_SOCK_ID).as_int().unwrap_or(-1);
        entries.push(PollEntry {
            key_ref: key,
            channel_ref: channel,
            interest,
            sock_id,
            listener_id,
            dgram_id,
        });
    }

    // Build the PollReq array. One entry per channel with a real fd,
    // plus one trailing entry for the wakeup channel. `req_to_entry`
    // maps each PollReq index back to the original `entries` slot
    // (None = wakeup).
    let reg = s2_registry().lock();
    let mut reqs: Vec<PollReq> = Vec::with_capacity(entries.len() + 1);
    let mut req_to_entry: Vec<Option<usize>> = Vec::with_capacity(entries.len() + 1);

    for (ei, entry) in entries.iter().enumerate() {
        let mut events: i16 = 0;

        if entry.sock_id >= 0 {
            if let Some(stream) = reg.streams.get(&entry.sock_id) {
                if entry.interest & 1 != 0 {
                    events |= POLL_IN;
                }
                if (entry.interest & 4) != 0 || (entry.interest & 8) != 0 {
                    // OP_WRITE and OP_CONNECT both map to POLL_OUT.
                    events |= POLL_OUT;
                }
                reqs.push(PollReq {
                    fd: stream_pollreq_fd(stream),
                    events,
                });
                req_to_entry.push(Some(ei));
                continue;
            }
        }

        if entry.listener_id >= 0 && entry.interest & 16 != 0 {
            if let Some(listener) = reg.listeners.get(&entry.listener_id) {
                reqs.push(PollReq {
                    fd: listener_pollreq_fd(listener),
                    events: POLL_IN,
                });
                req_to_entry.push(Some(ei));
                continue;
            }
        }

        if entry.dgram_id >= 0 {
            if let Some(dgram) = reg.dgrams.get(&entry.dgram_id) {
                if entry.interest & 1 != 0 {
                    events |= POLL_IN;
                }
                if entry.interest & 4 != 0 {
                    events |= POLL_OUT;
                }
                reqs.push(PollReq {
                    fd: dgram_pollreq_fd(dgram),
                    events,
                });
                req_to_entry.push(Some(ei));
                continue;
            }
        }
        // No fd for this entry — still counted for post-processing
        // (OP_CONNECT of an already-connected socket).
    }

    // Append the wakeup fd. If the wakeup channel can't be built, we
    // proceed without it; the caller can still return via timeout.
    drop(reg); // drop registry lock before touching wakeup map
    let wakeup_req = ensure_wakeup_channel(ctx, sel, |ch| PollReq {
        fd: dgram_pollreq_fd(&ch.socket),
        events: POLL_IN,
    });
    if let Some(req) = wakeup_req {
        reqs.push(req);
        req_to_entry.push(None);
    }

    // Call the OS poll. On Unix this hits `poll(2)`; on Windows it hits
    // `WSAPoll` via direct FFI. Both honor the same timeout contract. Blocking
    // waits must enter the VM's GC-blocked protocol so a stop-the-world GC does
    // not wait for an event-loop thread parked in the kernel. Pin selector-local
    // ObjectRefs before the deposit so a moving GC can remap them on wake.
    let revents = if timeout_ms == 0 {
        selector_poll(&reqs, timeout_ms)
    } else {
        let pin_base = ctx.pin_native_root(sel);
        let sel_pin = pin_base;
        let keys_pin = ctx.pin_native_root(keys_arr);
        let entry_pins: Vec<(usize, usize)> = entries
            .iter()
            .map(|entry| {
                (
                    ctx.pin_native_root(entry.key_ref),
                    ctx.pin_native_root(entry.channel_ref),
                )
            })
            .collect();
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let revents = selector_poll(&reqs, timeout_ms);
        ctx.end_blocking_region();
        sel = ctx.read_native_pin(sel_pin, sel);
        let _keys_arr = ctx.read_native_pin(keys_pin, keys_arr);
        for (entry, (key_pin, channel_pin)) in entries.iter_mut().zip(entry_pins) {
            entry.key_ref = ctx.read_native_pin(key_pin, entry.key_ref);
            entry.channel_ref = ctx.read_native_pin(channel_pin, entry.channel_ref);
        }
        ctx.unpin_native_roots(pin_base);
        revents
    };
    if !revents.is_empty() {
        for (pi, rev) in revents.iter().enumerate() {
            let ei = match req_to_entry[pi] {
                Some(e) => e,
                None => continue, // wakeup fd — handled after the loop
            };
            let entry = &entries[ei];
            let mut ready = 0i32;
            if rev & POLL_IN != 0 {
                if entry.interest & 1 != 0 {
                    ready |= 1;
                } // OP_READ
                if entry.interest & 16 != 0 {
                    ready |= 16;
                } // OP_ACCEPT
            }
            if rev & POLL_OUT != 0 {
                if entry.interest & 4 != 0 {
                    ready |= 4;
                } // OP_WRITE
                if entry.interest & 8 != 0 {
                    ready |= 8;
                } // OP_CONNECT
            }
            if rev & (POLL_HUP | POLL_ERR) != 0 {
                // POLL_HUP / POLL_ERR after a read interest: surface as
                // readable so the Java code can observe EOF / error via
                // a normal `read()` that returns -1.
                ready |= entry.interest & 1;
            }
            // Merge with any previously-set ready bits so
            // post-processing (accept / connect) can augment.
            let prev = ctx.get_field(entry.key_ref, 3).as_int().unwrap_or(0);
            ctx.set_field(entry.key_ref, 3, Value::Int(prev | ready));
        }
    }

    // Drain the wakeup channel (no-op if no wakeup was pending). We do
    // this unconditionally so a spurious leftover byte from a previous
    // cycle is also cleared.
    drain_wakeup(ctx, sel);

    // Post-processing: OP_ACCEPT eagerly pulls the next connection into
    // the SSC's `pending` slot so the Java caller can hand it out via
    // `accept()`. OP_CONNECT synthesizes readiness for channels that
    // were already connected at registration time.
    for entry in &entries {
        if entry.interest & 16 != 0 && entry.listener_id >= 0 {
            let ready = ctx.get_field(entry.key_ref, 3).as_int().unwrap_or(0);
            if ready & 16 != 0 {
                let result = {
                    let mut reg = s2_registry().lock();
                    s2_try_accept_nonblocking(&mut reg, entry.listener_id)
                };
                if let Some(sid) = result {
                    ctx.set_field(entry.channel_ref, S2SSC_PENDING, Value::Int(sid));
                }
            }
        }
        if entry.interest & 8 != 0
            && ctx
                .get_field(entry.channel_ref, S2SC_CONNECTED)
                .as_int()
                .unwrap_or(0)
                != 0
        {
            let old = ctx.get_field(entry.key_ref, 3).as_int().unwrap_or(0);
            ctx.set_field(entry.key_ref, 3, Value::Int(old | 8));
        }
    }

    // Count keys whose readyOps are non-zero. This is the select() return
    // value: the number of channels whose readiness set changed.
    let mut count = 0;
    for entry in &entries {
        if ctx.get_field(entry.key_ref, 3).as_int().unwrap_or(0) != 0 {
            count += 1;
        }
    }
    count
}

/// Specialization of `s2_selector_do_poll_with_timeout` for a selector that
/// has zero registered keys. We still honor the wakeup + timeout contract
/// so that idiomatic code like `selector.select(1000)` on a not-yet-bound
/// selector sleeps for the timeout instead of spinning.
pub fn poll_empty_selector(ctx: &mut dyn NativeContext, sel: ObjectRef, timeout_ms: i32) -> i32 {
    if timeout_ms == 0 {
        drain_wakeup(ctx, sel);
        return 0;
    }
    // Construct a single-entry PollReq for the wakeup channel and block
    // on it for the specified timeout. If the wakeup channel could not
    // be built, fall back to `std::thread::sleep` so the caller is still
    // rate-limited rather than busy-looping.
    let req = ensure_wakeup_channel(ctx, sel, |ch| PollReq {
        fd: dgram_pollreq_fd(&ch.socket),
        events: POLL_IN,
    });
    match req {
        Some(r) => {
            let pin_base = ctx.pin_native_root(sel);
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let _ = selector_poll(&[r], timeout_ms);
            ctx.end_blocking_region();
            let sel = ctx.read_native_pin(pin_base, sel);
            ctx.unpin_native_roots(pin_base);
            drain_wakeup(ctx, sel);
            0
        }
        None => {
            if timeout_ms > 0 {
                let pin_base = ctx.pin_native_root(sel);
                ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
                ctx.end_blocking_region();
                ctx.unpin_native_roots(pin_base);
            }
            0
        }
    }
}

// ---- Close-awareness for the synthetic NIO surface -------------------------
//
// A thread parked in a blocking accept/read/write on one of these sockets must
// come back when another thread closes the channel. It could not: `close`
// removes the entry from `s2_registry()`, but the parked thread cloned the
// `Arc` / `try_clone`d the handle out before it started, so the OS socket stays
// open and the syscall stays in the kernel. That is the same defect the three
// readers fixed on 2026-08-07/11 had, and the loop below is the same loop —
// park in `poll`, re-ask the registry every slice — rather than a second
// mechanism doing the same job.
//
// `net_phase_e.rs` documents this file's accept as broken in a comment of its
// own ("on Windows, closing one duplicated socket handle does not unblock a
// thread blocked in `accept()` on another duplicate"); the fix landed there and
// not here.

/// How long a parked synthetic-NIO operation waits inside one poll before
/// re-asking `s2_registry()` whether its socket was closed under it. Same value
/// and same role as `native-io/src/net.rs`'s `NET_READ_CLOSE_POLL_MS`.
pub const S2_CLOSE_POLL_MS: i32 = 25;

/// Readiness with a bounded wait, over this file's existing [`selector_poll`]
/// abstraction rather than a fresh binding of `poll(2)`/`WSAPoll`.
///
/// `Some(true)` ready (including POLLERR/POLLHUP, which the following syscall
/// then surfaces as the concrete error); `Some(false)` the slice expired;
/// `None` the OS poll itself failed — the caller's signal to fall back to one
/// plain blocking syscall rather than spin on something that can never report
/// readiness.
pub fn s2_poll_ready(fd: i64, want_write: bool, timeout_ms: i32) -> Option<bool> {
    let req = PollReq {
        fd,
        events: if want_write { POLL_OUT } else { POLL_IN },
    };
    let revents = selector_poll(&[req], timeout_ms);
    let bits = *revents.first()?;
    let wanted = if want_write { POLL_OUT } else { POLL_IN };
    Some(bits & (wanted | POLL_ERR | POLL_HUP) != 0)
}

/// Is `lid` still a live listener? `ServerSocketChannel.close()` removes the
/// entry, so this flips exactly when Java closed it.
pub fn s2_listener_still_registered(lid: i32) -> bool {
    s2_registry().lock().listeners.contains_key(&lid)
}

/// Is `sid` still a live stream? `SocketChannel.close()` removes the entry.
pub fn s2_stream_still_registered(sid: i32) -> bool {
    s2_registry().lock().streams.contains_key(&sid)
}

/// Is `sid` still a live datagram socket? `DatagramChannel.close()` removes the
/// entry. Exposed for `phases_late::net_channels`, whose `DatagramChannel` sites
/// share this registry.
pub fn s2_dgram_still_registered(sid: i32) -> bool {
    s2_registry().lock().dgrams.contains_key(&sid)
}

/// The error a parked synthetic-NIO operation reports once its socket has been
/// closed from another thread. `ErrorKind::Interrupted` is the carrier all
/// three landed close-aware readers use, and it is unambiguous here because
/// [`s2_poll_ready`] never reports EINTR as an error.
pub fn s2_async_closed_err() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "channel closed")
}

/// Park until `fd` is ready or the socket `still_registered` names is closed.
///
/// `Ok(true)` ready; `Ok(false)` no usable poll primitive, fall back to one
/// plain blocking syscall; `Err(Interrupted)` closed from another thread.
///
/// The registry lock is taken inside `still_registered` for that lookup alone
/// and is never held across the poll — the whole point of cloning the handle
/// out first, and the reason `s2_registry()` being a single process-wide mutex
/// is survivable at all.
///
/// # On expiry
///
/// The per-pass `S2_CLOSE_POLL_MS` slice expiring is not an outcome — it is
/// only the point at which the registry is re-asked, and the loop continues.
/// There is no second deadline here because none of these call sites has a
/// `SO_TIMEOUT` to honour: this synthetic `SocketChannel` surface exposes no
/// timed read, and `ServerSocketChannel.accept()` is untimed by contract.
pub fn s2_wait_ready_close_aware(
    fd: i64,
    want_write: bool,
    still_registered: &dyn Fn() -> bool,
) -> std::io::Result<bool> {
    loop {
        let Some(ready) = s2_poll_ready(fd, want_write, S2_CLOSE_POLL_MS) else {
            return Ok(false);
        };
        // Asked AFTER the poll so a close landing while we are parked is seen
        // on the very next pass, and a close that raced a readiness edge still
        // wins.
        if !still_registered() {
            return Err(s2_async_closed_err());
        }
        if ready {
            return Ok(true);
        }
    }
}

pub fn s2_try_accept_nonblocking(reg: &mut SocketRegistry, lid: i32) -> Option<i32> {
    let listener = reg.listeners.get(&lid)?;
    let _ = listener.set_nonblocking(true);
    let result = listener.accept();
    match result {
        // gc-common w13-d: minted by `s2_next_free_id` like every other row.
        // The hand-rolled minting this replaces took `next_id` without
        // checking it (only the id AFTER it), never checked `dgrams` or
        // `tls_streams`, and could return 0 after a wrap, so an accepted
        // stream could overwrite a live row of another table's id. The stream
        // belongs to the listener's VM.
        Ok((stream, _)) => Some(s2_insert_accepted_stream(reg, lid, stream)),
        Err(_) => None,
    }
}

pub fn s2_blocking_accept(lid: i32) -> Option<i32> {
    // Clone the listener handle out under a SHORT lock, then run the blocking
    // accept() WITHOUT holding s2_registry, and re-lock only to register the
    // accepted stream. Holding the global registry lock across a blocking
    // accept() deadlocks every other synthetic-socket operation process-wide
    // (the Hibernate JTA default-mode hang — see net_phase_e::re2_accept_into).
    let listener = {
        let reg = s2_registry().lock();
        let l = reg.listeners.get(&lid)?;
        let _ = l.set_nonblocking(false);
        l.try_clone().ok()?
    };
    // CLOSE-AWARENESS 2026-08-12: `try_clone()` is precisely why the close
    // could not reach this accept. `ServerSocketChannel.close()` removes the
    // registry entry and drops ITS listener; this thread is parked on a
    // DUPLICATE handle, and on Windows closing one duplicate does not abort a
    // blocking call on another. So park in `poll` and re-ask the registry —
    // the same loop `net::net_accept_close_aware`,
    // `socket_channel::accept_close_aware` and `re2_accept_into` all use.
    //
    // `Ok(false)` (no poll primitive on this target) falls through to the plain
    // blocking accept, which cannot see the close but at least still accepts.
    match s2_wait_ready_close_aware(listener_pollreq_fd(&listener), false, &|| {
        s2_listener_still_registered(lid)
    }) {
        Ok(_) => {}
        // Closed under us. `None` is this function's existing "no connection"
        // answer and every caller already handles it.
        Err(_) => return None,
    }
    // NAMED RESIDUAL: with two threads accepting the same `lid`, the loser of
    // the race between the poll above and this `accept()` parks again until the
    // next connection, and that park is not close-aware. It is not closed by
    // flipping the clone non-blocking: `try_clone` shares the blocking mode with
    // the registry's listener on both platforms (a `dup`'s O_NONBLOCK lives on
    // the open file description; a Windows duplicate shares the socket's FIONBIO
    // state), so this thread would be changing the other one's contract. Every
    // accept parked before this change; at most one loser parks after it.
    match listener.accept() {
        // Same minting and ownership as `s2_try_accept_nonblocking`.
        Ok((stream, _)) => Some(s2_alloc_accepted_stream(lid, stream)),
        Err(_) => None,
    }
}

// ---- Main entry point ------------------------------------------------------

pub fn register_s2_nio(r: &mut NativeMethodRegistry) {
    register_s2_bytebuffer(r);
    register_s2_byteorder(r);
    register_s2_socket_channel(r);
    register_s2_server_socket_channel(r);
    register_s2_selector(r);
}

pub fn register_s2_bytebuffer_essentials(r: &mut NativeMethodRegistry) {
    register_s2_bytebuffer(r);
    register_s2_byteorder(r);
}

s2_view_buf_fn!(s2_bb_as_long_buffer, "java/nio/LongBuffer", 8);
s2_view_buf_fn!(s2_bb_as_short_buffer, "java/nio/ShortBuffer", 2);
s2_view_buf_fn!(s2_bb_as_float_buffer, "java/nio/FloatBuffer", 4);
s2_view_buf_fn!(s2_bb_as_double_buffer, "java/nio/DoubleBuffer", 8);

/// Typed views encode their source byte offset as -(offset + 1) in the
/// Buffer.address slot. That slot is a long on real JDK buffer layouts,
/// while synthetic views use an int, so preserve both representations.
#[inline]
pub fn s2_typed_view_byte_start(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    let marker = match ctx.get_field(this, BB_MARK) {
        Value::Int(v) => v,
        Value::Long(v) => i32::try_from(v).unwrap_or(-1),
        _ => -1,
    };
    if marker < 0 {
        -(marker + 1)
    } else {
        0
    }
}

/// `ByteBuffer.asCharBuffer()` — the view returned MUST have its backing
/// store stored in the real-JDK `hb` field so JDK bytecode that reads
/// `hb` (e.g. `CharBuffer.hasArray`, `CharBuffer.array`) sees a non-null
/// char[]. The previous indexed-only write (`set_field(vb, BB_ARRAY, …)`)
/// hit the real-JDK Buffer.mark slot — descriptor coercion (`I`) turned
/// the Object reference into Int(low_bits_of_ptr), so when icu4j
/// invoked `CharBuffer.subSequence(...).toString()` on the view, the
/// JDK bytecode read a null `hb` and threw IllegalStateException
/// "CharBuffer has no backing array" (icu4j-68.2 / icu4j-70.1).
///
/// The byte storage is also transcoded eagerly into a freshly-allocated
/// char[] using the source ByteBuffer's byte order — the previous
/// shim left slot 0 pointing at the byte[] source, which our native
/// `charAt` interpreted as one char per byte (truncated UTF-16
/// high bytes to zero).
/// The real `ByteBufferAsCharBuffer{B,L}` for this byte order, when the class
/// library actually has one.
///
/// `None` in synthetic-JDK mode (where the class does not exist, or exists only
/// as a fabricated stub with no method bodies), which is what keeps the copying
/// fallback below reachable.
pub fn s2_bbacb_view_class(ctx: &mut dyn NativeContext, order: i32) -> Option<&'static str> {
    let cls = if order == 1 {
        "java/nio/ByteBufferAsCharBufferL"
    } else {
        "java/nio/ByteBufferAsCharBufferB"
    };
    // `class_id_by_name` alone answers "already loaded", and nothing loads this
    // class before the first `asCharBuffer` — so asking that way said "absent"
    // on a real JDK and silently kept the copying fallback. Drive the load.
    //
    // The return value of `ensure_class_initialized` is not the test:
    // `--real-jdk` aside, it fabricates a bare stub rather than failing. Ask
    // the property instead — a stub has no method bodies, so handing one back
    // would trade `AbstractMethodError` for a buffer whose every method is a
    // no-op.
    if ctx.ensure_class_initialized(cls).is_err() {
        return None;
    }
    if ctx.is_class_synthetic_stub(cls) {
        return None;
    }
    Some(cls)
}

pub fn s2_bb_as_char_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = s2_bb_pos(ctx, this) as usize;
    let lim = s2_bb_limit(ctx, this) as usize;
    let order = s2_bb_order(ctx, this); // 0 = BIG_ENDIAN, 1 = LITTLE_ENDIAN
    let rem_bytes = lim.saturating_sub(pos);
    let rem_chars = rem_bytes / 2;

    // Real-JDK: hand back a genuine `ByteBufferAsCharBuffer{B,L}` over the SAME
    // ByteBuffer, which is what `asCharBuffer` is specified to return.
    //
    // The copying fallback below stamps the ABSTRACT `java/nio/CharBuffer` and
    // that is wrong three ways at once, all of them measured against HotSpot by
    // `probes/NioBufferStampProbe`:
    //
    //   * `slice`, `duplicate`, `asReadOnlyBuffer`, `put(int,char)` and bulk
    //     `put(CharBuffer)` have no native on the abstract class, so they
    //     resolve to the abstract declaration and throw
    //     `AbstractMethodError: ... has no Code attribute`. Adding a native per
    //     method is whack-a-mole; a concrete receiver gets all of them from the
    //     JDK's own bodies.
    //   * A copy is not a view. `bb.asCharBuffer().put('A')` left the backing
    //     ByteBuffer untouched — HotSpot writes through. A lost write is the
    //     worse failure of the two, because nothing reports it.
    //   * `hasArray()` answered `true` (the copy has a `char[]`); a real char
    //     view over a ByteBuffer has no accessible array.
    //
    // `address` is the whole aliasing mechanism: the JDK computes each element's
    // byte offset as `(i << 1) + address`, and seeds `address` to the source's
    // own address plus its position. Carry that across so the existing
    // `ByteBufferAsCharBuffer*` natives — and the JDK bodies — index the same
    // bytes the source does.
    if let Some(view_cls) = s2_bbacb_view_class(ctx, order) {
        let src_address = match ctx.get_field_by_name(this, "address") {
            Value::Long(v) => v,
            Value::Int(v) => i64::from(v),
            // No `address` on the receiver (a synthetic-layout source): fall
            // back to the ARRAY_BYTE_BASE_OFFSET seeding `bb_write_hb` uses.
            _ => 16,
        };
        // Was an open-coded `matches!(…, Value::Int(1))` — the FIFTH
        // transcription of the `isReadOnly` field read, and the only one that
        // tested `== 1` rather than `!= 0`, so a flag written as any other
        // non-zero value made this arm disagree with `hasArray`/`array`/
        // `arrayOffset` about one receiver. Routed through the converged
        // `s2_bb_is_read_only` (F14-1 N1 / F21-1 §3). Nothing writes such a
        // value today: a latent divergence closed, not a flip.
        let read_only = s2_bb_is_read_only(ctx, this);
        let vb = try_alloc_concurrent_synthetic(ctx, view_cls, 0)?;
        ctx.set_field_by_name(vb, "bb", Value::Object(Some(this)));
        ctx.set_field_by_name(vb, "mark", Value::Int(-1));
        ctx.set_field_by_name(vb, "position", Value::Int(0));
        ctx.set_field_by_name(vb, "limit", Value::Int(rem_chars as i32));
        ctx.set_field_by_name(vb, "capacity", Value::Int(rem_chars as i32));
        ctx.set_field_by_name(vb, "isReadOnly", Value::Int(i32::from(read_only)));
        ctx.set_field_by_name(vb, "address", Value::Long(src_address + pos as i64));
        return Ok(Some(Value::Object(Some(vb))));
    }
    // Transcode bytes → chars using the source ByteBuffer's byte order.
    let chars_arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, rem_chars);
    // Storage-aware byte reads (heap incl. array-base offset, or direct) —
    // the previous `s2_bb_arr`-only loop silently produced an all-zero
    // char[] for direct sources (residual-doc item 3).
    for i in 0..rem_chars {
        let hi = s2_bb_get_byte(ctx, this, (pos + 2 * i) as i32) as i32 & 0xFF;
        let lo = s2_bb_get_byte(ctx, this, (pos + 2 * i + 1) as i32) as i32 & 0xFF;
        let ch = if order == 1 {
            (lo << 8) | hi
        } else {
            (hi << 8) | lo
        };
        ctx.set_array_element(chars_arr, i, Value::Int(ch));
    }
    // Read the source's read-only flag BEFORE the allocation below. `this` is
    // an `ObjectRef` and `try_alloc_concurrent_synthetic` can trigger a
    // collection that RELOCATES it; carrying a `bool` across the allocation is
    // something no collector can move, which is the same reason
    // `native-io`'s `buf_write_read_only` takes a `bool` rather than the
    // source. (The `if let` arm above already reads it before its own alloc.)
    let src_read_only = s2_bb_is_read_only(ctx, this);
    // GC-safety: `alloc_concurrent_synthetic` below allocates and can
    // trigger a collection that relocates `chars_arr` (written into the
    // new CharBuffer's fields further below); pin it and re-read.
    let chars_arr_pin = ctx.pin_native_root(chars_arr);
    let vb = try_alloc_concurrent_synthetic(ctx, "java/nio/CharBuffer", 6)?;
    let chars_arr = ctx.read_native_pin(chars_arr_pin, chars_arr);
    ctx.unpin_native_roots(chars_arr_pin);
    // Write to BOTH indexed slot 0 (synthetic-mode layout used by our
    // own CharBuffer natives) AND the real-JDK `hb` field by name (so
    // JDK bytecode reading `hb` / `hasArray` / `array` sees the char[]).
    ctx.set_field_by_name(vb, "hb", Value::Object(Some(chars_arr)));
    ctx.set_field_by_name(vb, "offset", Value::Int(0));
    // Was a flat `Int(0)`, i.e. this fallback arm ACTIVELY CLEARED the flag —
    // the same shape F21-1 §6.2 found in `slice`/`slice(II)`/`duplicate`'s
    // copying arms, in the one member of the family nobody had re-read.
    // `roBB.asCharBuffer()` is a `java.nio.ByteBufferAsCharBufferRB` with
    // `isReadOnly() == true` on HotSpot (MEASURED), and the arm above already
    // propagates; only this one did not. A copy is not the JDK's shape either
    // way, but a WRITABLE copy of a read-only buffer's contents is the strictly
    // worse of the two answers available here.
    ctx.set_field_by_name(vb, "isReadOnly", Value::Int(i32::from(src_read_only)));
    ctx.set_field_by_name(vb, "position", Value::Int(0));
    ctx.set_field_by_name(vb, "limit", Value::Int(rem_chars as i32));
    ctx.set_field_by_name(vb, "capacity", Value::Int(rem_chars as i32));
    ctx.set_field_by_name(vb, "mark", Value::Int(-1));
    // Heap CharBuffers use ARRAY_CHAR_BASE_OFFSET (16) as their address.
    // Do not unconditionally write the old indexed overlay: in real-JDK
    // layout its slot 4 is Buffer.address, so BB_MARK=-1 made bulk get()
    // call Unsafe.copyMemory with an invalid source offset.
    ctx.set_field_by_name(vb, "address", Value::Long(16));
    if s2_bb_synthetic_layout(ctx, vb) {
        ctx.set_field(vb, BB_ARRAY, Value::Object(Some(chars_arr)));
        ctx.set_field(vb, BB_POS, Value::Int(0));
        ctx.set_field(vb, BB_LIMIT, Value::Int(rem_chars as i32));
        ctx.set_field(vb, BB_CAP, Value::Int(rem_chars as i32));
        ctx.set_field(vb, BB_MARK, Value::Int(-1));
        ctx.set_field(vb, BB_ORDER, Value::Int(order));
    }
    Ok(Some(Value::Object(Some(vb))))
}

/// Build an ALIASING heap ByteBuffer view (used by `slice`/`slice(II)`/
/// `duplicate`/`asReadOnlyBuffer`): shares `arr` with the source and
/// records the view's array-base `offset`, exactly like real-JDK
/// HeapByteBuffer views. `address` mirrors the real ctor's
/// ARRAY_BYTE_BASE_OFFSET + offset seeding (see `bb_write_hb`).
#[allow(clippy::too_many_arguments)]
pub fn s2_bb_new_heap_view(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    offset: usize,
    pos: i32,
    lim: i32,
    cap: i32,
    mark: i32,
    read_only: bool,
    ord: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let cls = s2_bb_heap_class(ctx, read_only);
    let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
    ctx.set_field_by_name(buf, "hb", Value::Object(Some(arr)));
    ctx.set_field_by_name(buf, "offset", Value::Int(offset as i32));
    s2_bb_set_read_only(ctx, buf, read_only);
    ctx.set_field_by_name(buf, "position", Value::Int(pos));
    ctx.set_field_by_name(buf, "limit", Value::Int(lim));
    ctx.set_field_by_name(buf, "capacity", Value::Int(cap));
    ctx.set_field_by_name(buf, "mark", Value::Int(mark));
    ctx.set_field_by_name(
        buf,
        "address",
        Value::Long(16i64.saturating_add(offset as i64)),
    );
    // A `--synthetic-jdk` stub (interpreter round i5 wave 105,
    // `s2_bb_indexed_layout`) has no `offset` field either. Wave 105 took
    // the indexed writes at a zero base only, so a stub view at a non-zero
    // base (`slice()` of a positioned stub, then `asReadOnlyBuffer()`) had
    // no storage and its first `get` raised an internal error. Wave 106
    // (lane L5): the base goes into `BB_STUB_HEAP_OFFSET_SLOT`, which
    // `s2_bb_heap_base` and native-io's `bb_resolve_heap_offset` both read.
    let stub = !s2_bb_synthetic_layout(ctx, buf) && s2_buf_stub_layout(ctx, buf);
    if stub && ctx.object_num_fields(buf) > BB_STUB_HEAP_OFFSET_SLOT {
        ctx.set_field(buf, BB_STUB_HEAP_OFFSET_SLOT, Value::Int(offset as i32));
    }
    if s2_bb_synthetic_layout(ctx, buf)
        || (stub && (offset == 0 || ctx.object_num_fields(buf) > BB_STUB_HEAP_OFFSET_SLOT))
    {
        // Bare-synthetic layout: no `offset` field exists, so aliasing at a
        // non-zero base is not representable — the callers below keep the
        // legacy copying behaviour for that mode instead of reaching here.
        ctx.set_field(buf, BB_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(buf, BB_POS, Value::Int(pos));
        ctx.set_field(buf, BB_LIMIT, Value::Int(lim));
        ctx.set_field(buf, BB_CAP, Value::Int(cap));
        ctx.set_field(buf, BB_MARK, Value::Int(mark));
    }
    s2_bb_set_order(ctx, buf, ord);
    Ok(buf)
}

/// Build an ALIASING direct ByteBuffer view over native memory at `addr`
/// (already advanced to the view's byte 0). The object is stamped with the
/// abstract `java/nio/ByteBuffer` class — every accessor reaches the
/// storage through `s2_bb_direct_addr`, closing residual-doc item 3's
/// "slices/duplicates of a direct buffer come back empty" gap.
#[allow(clippy::too_many_arguments)]
pub fn s2_bb_new_direct_view(
    ctx: &mut dyn NativeContext,
    addr: i64,
    pos: i32,
    lim: i32,
    cap: i32,
    mark: i32,
    read_only: bool,
    ord: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 6)?;
    s2_bb_set_read_only(ctx, buf, read_only);
    ctx.set_field_by_name(buf, "position", Value::Int(pos));
    ctx.set_field_by_name(buf, "limit", Value::Int(lim));
    ctx.set_field_by_name(buf, "capacity", Value::Int(cap));
    ctx.set_field_by_name(buf, "mark", Value::Int(mark));
    ctx.set_field_by_name(buf, "address", Value::Long(addr));
    // The indexed layouts carry no names (interpreter round i5 wave 105,
    // lane L5): the same slots `s2_bb_alloc_direct` writes, so a direct
    // view of a `--synthetic-jdk` buffer keeps its storage.
    if s2_bb_indexed_layout(ctx, buf) {
        ctx.set_field(buf, BB_ARRAY, Value::Object(None));
        ctx.set_field(buf, BB_POS, Value::Int(pos));
        ctx.set_field(buf, BB_LIMIT, Value::Int(lim));
        ctx.set_field(buf, BB_CAP, Value::Int(cap));
        ctx.set_field(buf, BB_MARK, Value::Long(addr));
    }
    s2_bb_set_order(ctx, buf, ord);
    Ok(buf)
}

/// Raise `buf`'s closed-session error the way the JDK's bulk copy does, for a
/// segment-backed buffer (`MemorySegment.asByteBuffer()`), and do nothing for
/// any other buffer.
///
/// JIT round 17 wave 8, lane nio3 (`docs/feature-designs/native-dispatch-architecture.md`
/// `r17-compat-bytebuffer-bulk-skips-closed-session-FIXED-20261005.md`).
/// `ByteBuffer.get(byte[])` / `get(byte[],int,int)` / `put(byte[])` /
/// `put(byte[],int,int)` / `put(ByteBuffer)` are force-dispatched to the
/// natives in [`register_s2_bytebuffer`] (`native_override.rs`'s
/// `java/nio/ByteBuffer` block), and a `DirectByteBuffer` inherits all five
/// from `ByteBuffer`. Their JDK bodies end in
/// `SCOPED_MEMORY_ACCESS.copyMemory(session(), ...)` (or a per-element
/// `get(j)` / `put(j, x)` loop, which checks the same session), so after
/// `arena.close()` HotSpot throws `IllegalStateException: Already closed`.
/// The natives copied from `address` with no check: `R17Nio2Typed` fact
/// `session` 10 (`bb.get(new byte[4])`) answered `ok` under `--compatible`.
/// Under `--real-jdk` the five rows are retired and the bytecode runs.
///
/// `p67_segment_check_scope` handles a real, a modelled and a synthetic
/// session; a real one runs `checkValidState()` bytecode, which can collect,
/// so `buf` and `other` are pinned across it and re-read.
/// Merged with dev's independent fix of the same divergence (`7d7a682b63`, `57cde99e79`:
/// `s2_bb_check_session` on the four byte[] bulk natives and `put(ByteBuffer)`): this helper
/// covers those and the typed bulk pairs and `hashCode` / `equals` / `compareTo`, and pins
/// both buffers across the check, so it replaced dev's at the round-17 merge.
pub fn s2_bb_check_live_session(
    ctx: &mut dyn NativeContext,
    buf: &mut ObjectRef,
    other: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let Some(seg_idx) = ctx.resolve_field_index("java/nio/Buffer", "segment") else {
        return Ok(());
    };
    let Value::Object(Some(mut segment)) = ctx.get_field(*buf, seg_idx) else {
        return Ok(());
    };
    let mut keep = [Value::Object(Some(*buf)), Value::Object(Some(*other))];
    let checked = keep_across(ctx, &mut keep, |ctx| {
        crate::phases_late::foreign_ffm::p67_segment_check_scope(ctx, &mut segment)
    });
    if let [Value::Object(Some(b)), Value::Object(Some(o))] = keep {
        *buf = b;
        *other = o;
    }
    checked
}

pub fn register_s2_bytebuffer(r: &mut NativeMethodRegistry) {
    use cratonvm_types::ArrayElementType;
    let bb = "java/nio/ByteBuffer";
    // The five read accessors below are retired rows of the `JdkNioBuffers`
    // family, which `NativeMethodRegistry::register` drops on a real JDK under
    // `--compatible` (default on) and `--real-jdk`. The NIO2-17-1 opt-in that
    // also dropped them with that row off (`CRATONVM_COMPAT_REAL_BUFFER_ACCESSORS`)
    // was deleted in nio20 wave 2 (NIO20-L4-2): moot under the family.

    r.register(bb, "allocate", "(I)Ljava/nio/ByteBuffer;", |ctx, args| {
        let requested = args.first().and_then(|v| v.as_int()).unwrap_or(0);
        // `ByteBuffer.allocate` opens with
        // `if (capacity < 0) throw createCapacityException(capacity)`.
        // Clamping to 0 handed back an empty buffer and reported success.
        if requested < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("capacity < 0: ({requested} < 0)"),
            }
            .into());
        }
        let cap = requested as usize;
        match s2_bb_alloc(ctx, cap)? {
            Some(buf) => Ok(Some(Value::Object(Some(buf)))),
            // The message names the site and the size. It used to be a bare
            // "Java heap space", which is also what the pre-allocated singleton
            // OOME carries and what three unrelated natives throw -- so the
            // string identified nothing. Chasing the H2 `TestBenchmark` refusal
            // cost a run per candidate site purely to find out which of them had
            // produced it; the two bytecode paths already name themselves
            // ("alloc_array length N"), and this is the third allocator of
            // caller-sized arrays.
            None => Err(RuntimeError::OutOfMemoryError {
                message: format!("Java heap space (ByteBuffer.allocate {cap})"),
            }
            .into()),
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        bb,
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        |ctx, args| {
            let cap = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            if cap < 0 {
                // `Buffer.createCapacityException`, the text `allocate` above
                // already uses; the bare "capacity < 0" matched nothing
                // HotSpot says (JIT round 17 wave 3, lane compatbridge;
                // `R17CompatBridge` fact `alloc`).
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("capacity < 0: ({cap} < 0)"),
                }
                .into());
            }
            // Real-JDK mode: run the genuine `DirectByteBuffer(int)`
            // constructor. It installs the JDK's own `Cleaner`/`Deallocator`
            // and `Bits` accounting, which is strictly better than anything we
            // can synthesise, so nothing changes on that path.
            //
            // Both probes are needed and neither alone is enough:
            // `would_fabricate_synthetic_stub` is the non-destructive "are the
            // class bytes reachable" question, but it answers "no stub" once
            // ANY earlier caller has already minted the stub (it short-circuits
            // on `resolve_fast_path_class_id`); `is_class_synthetic_stub`
            // covers exactly that case by asking what the loaded class IS.
            let real_direct_byte_buffer = !ctx
                .would_fabricate_synthetic_stub("java/nio/DirectByteBuffer")
                && !ctx.is_class_synthetic_stub("java/nio/DirectByteBuffer");
            if real_direct_byte_buffer {
                return ctx.new_object_initialized(
                    "java/nio/DirectByteBuffer",
                    "(I)V",
                    &[Value::Int(cap)],
                );
            }
            // Synthetic-JDK mode: there is no `DirectByteBuffer` bytecode to
            // run and `new_object_initialized` would raise NoClassDefFound —
            // `allocateDirect` simply threw. Build a genuinely direct buffer
            // here instead (NEW-17).
            s2_bb_alloc_direct(ctx, cap)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(bb, "wrap", "([B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let len = ctx.array_length(arr) as i32;
        let cls = s2_bb_heap_class(ctx, false);
        let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
        bb_write_hb(ctx, buf, arr, len);
        Ok(Some(Value::Object(Some(buf))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "wrap", "([BII)Ljava/nio/ByteBuffer;", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let off = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let cap = i32::try_from(ctx.array_length(arr)).unwrap_or(i32::MAX);
        // `ByteBuffer.wrap(array, offset, length)` is
        // `try { new HeapByteBuffer(array, offset, length, null) }
        //  catch (IllegalArgumentException x) { throw new IndexOutOfBoundsException(); }`
        // — the range check is the constructor's, and it raises
        // `IndexOutOfBoundsException` with no detail message. Clamping the
        // limit with `.min(cap)` instead silently produced a SHORTER buffer
        // than asked for: `wrap(new byte[4], 0, 9)` returned a 4-byte window
        // and reported success.
        if off < 0 || len < 0 || i64::from(off) + i64::from(len) > i64::from(cap) {
            return Err(RuntimeError::ioobe_no_message().into());
        }
        let cls = s2_bb_heap_class(ctx, false);
        let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
        bb_write_hb(ctx, buf, arr, cap);
        // Override position/limit set by bb_write_hb.
        ctx.set_field_by_name(buf, "position", Value::Int(off));
        ctx.set_field_by_name(buf, "limit", Value::Int(off + len));
        ctx.set_field(buf, BB_POS, Value::Int(off));
        ctx.set_field(buf, BB_LIMIT, Value::Int(off + len));
        Ok(Some(Value::Object(Some(buf))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // get
    r.register(bb, "get", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let b = s2_bb_get_byte(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Int(b as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "get", "(I)B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 1)?;
        Ok(Some(Value::Int(s2_bb_get_byte(ctx, this, idx) as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // JIT round 17 wave 8, lane nio3: the byte[] / ByteBuffer bulk accessors, the
    // typed bulk pair and `hashCode` / `equals` / `compareTo` below check a
    // segment-backed receiver's session (`CRATONVM_COMPAT_BUFFER_BULK_SESSION`,
    // default ON, `=0` registers the unchecked bodies). Read once per registry;
    // the choice picks which monomorphised body is registered, so no static
    // holds it. See `s2_bb_check_live_session`.
    let bulk_session =
        true;
    fn s2_bb_get_bytes_range<const SESSION: bool>(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        let mut dst = obj_arg(args, 1)?;
        // BUG [nb-servlet]: `off`/`len` were taken as raw i32 with NO negativity
        // check, and `off` was cast to usize BEFORE validation. A negative `len`
        // passed the `pos + len > limit` test (it makes the sum smaller), then
        // `for i in 0..len as usize` reinterpreted the negative as ~1.8e19 → hang/DoS.
        // A negative `off` cast straight to a huge usize. Fix: reject off<0/len<0
        // with (Array)IndexOutOfBoundsException first, do the bounds math in widened
        // i64 to avoid overflow, and verify off+len fits the destination array.
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let dst_cap = i32::try_from(ctx.array_length(dst)).unwrap_or(i32::MAX);
        s2_check_from_index_size(off, len, dst_cap)?;
        let pos = s2_bb_pos(ctx, this);
        // Widened arithmetic: pos+len cannot wrap into a "passing" value.
        if (pos as i64) + (len as i64) > s2_bb_limit(ctx, this) as i64 {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        // `getArray`'s copy (or its per-element `get(j)` loop) checks the
        // session after the bounds checks above, and not at all for length 0.
        if SESSION && len > 0 {
            s2_bb_check_live_session(ctx, &mut this, &mut dst)?;
        }
        let off = off as usize;
        // Mirrors put(Ljava/nio/ByteBuffer;)'s direct-buffer fix
        // (2026-07-10): the pre-fix `s2_bb_arr(ctx, this).unwrap_or(dst)`
        // made `dst` its own copy source whenever `this` is a DIRECT
        // buffer (no heap array) — a silent self-copy that left `dst`
        // untouched while position still advanced and the call reported
        // success. `IOUtil.read` routes every buffered `FileChannel` read
        // through a temporary direct buffer and then bulk-`get`s it into a
        // byte[], so this exact path is how Lucene's footer/checksum bytes
        // came back as zero (ES-FAIL-FAMILY-20260709).
        if let Some(arr) = s2_bb_arr(ctx, this) {
            let base = s2_bb_heap_base(ctx, this);
            let src_off = base + pos as usize;
            if !s2_bb_bulk_array_copy(ctx, arr, src_off, dst, off, len as usize) {
                for i in 0..len as usize {
                    let b = ctx.get_array_element(arr, src_off + i);
                    ctx.set_array_element(dst, off + i, b);
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            let mut bytes = vec![0u8; len as usize];
            if !ctx.copy_from_native_memory(addr.saturating_add(pos as i64), &mut bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.get([BII): direct source read failed".to_string(),
                }
                .into());
            }
            if !ctx.write_byte_array_from(dst, off, &bytes) {
                for (i, byte) in bytes.iter().enumerate() {
                    ctx.set_array_element(dst, off + i, Value::Int(*byte as i8 as i32));
                }
            }
        } else {
            // Genuinely storage-less synthetic buffer — keep the historic
            // silent no-op (position unchanged), matching
            // put(ByteBuffer;)'s same fallback for half-built synthetics.
            return Ok(Some(Value::Object(Some(this))));
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    }
    r.register(
        bb,
        "get",
        "([BII)Ljava/nio/ByteBuffer;",
        if bulk_session {
            s2_bb_get_bytes_range::<true>
        } else {
            s2_bb_get_bytes_range::<false>
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    fn s2_bb_get_bytes<const SESSION: bool>(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        let mut dst = obj_arg(args, 1)?;
        let len = ctx.array_length(dst) as i32;
        let pos = s2_bb_pos(ctx, this);
        if pos + len > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        // `get(byte[])` is `get(dst, 0, dst.length)`: the same check as above.
        if SESSION && len > 0 {
            s2_bb_check_live_session(ctx, &mut this, &mut dst)?;
        }
        if let Some(arr) = s2_bb_arr(ctx, this) {
            let base = s2_bb_heap_base(ctx, this);
            let src_off = base + pos as usize;
            if !s2_bb_bulk_array_copy(ctx, arr, src_off, dst, 0, len as usize) {
                for i in 0..len as usize {
                    let b = ctx.get_array_element(arr, src_off + i);
                    ctx.set_array_element(dst, i, b);
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            let mut bytes = vec![0u8; len as usize];
            if !ctx.copy_from_native_memory(addr.saturating_add(pos as i64), &mut bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.get([B): direct source read failed".to_string(),
                }
                .into());
            }
            if !ctx.write_byte_array_from(dst, 0, &bytes) {
                for (i, byte) in bytes.iter().enumerate() {
                    ctx.set_array_element(dst, i, Value::Int(*byte as i8 as i32));
                }
            }
        } else {
            return Ok(Some(Value::Object(Some(this))));
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    }
    r.register(
        bb,
        "get",
        "([B)Ljava/nio/ByteBuffer;",
        if bulk_session {
            s2_bb_get_bytes::<true>
        } else {
            s2_bb_get_bytes::<false>
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // put
    r.register(bb, "put", "(B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i8;
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_put_byte(ctx, this, pos, b);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "put", "(IB)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let b = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i8;
        s2_bb_check_index(ctx, this, idx, 1)?;
        s2_bb_put_byte(ctx, this, idx, b);
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    fn s2_bb_put_bytes_range<const SESSION: bool>(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let mut src = obj_arg(args, 1)?;
        // BUG [nb-servlet]: symmetric to get([BII). `off`/`len` were raw i32 with
        // no negativity check and `off` cast to usize before validation. A negative
        // `len` slipped past `pos + len > limit` then `for i in 0..len as usize`
        // looped ~1.8e19 times (hang/DoS); a negative `off` indexed src wildly. Fix:
        // reject off<0/len<0 with (Array)IndexOutOfBoundsException, widen the buffer
        // bounds math to i64, and verify off+len fits the source array.
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let src_cap = i32::try_from(ctx.array_length(src)).unwrap_or(i32::MAX);
        s2_check_from_index_size(off, len, src_cap)?;
        let pos = s2_bb_pos(ctx, this);
        // Widened arithmetic: pos+len cannot wrap into a "passing" value.
        if (pos as i64) + (len as i64) > s2_bb_limit(ctx, this) as i64 {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        // `putArray`'s copy (or its per-element `put(j, x)` loop): after the
        // checks above, and not at all for length 0.
        if SESSION && len > 0 {
            s2_bb_check_live_session(ctx, &mut this, &mut src)?;
        }
        let off = off as usize;
        // Mirrors put(Ljava/nio/ByteBuffer;)'s direct-buffer fix — see
        // get([BII)'s comment above for the matching read-side rationale.
        if let Some(arr) = s2_bb_arr(ctx, this) {
            let base = s2_bb_heap_base(ctx, this);
            let dst_off = base + pos as usize;
            if !s2_bb_bulk_array_copy(ctx, src, off, arr, dst_off, len as usize) {
                for i in 0..len as usize {
                    let b = ctx.get_array_element(src, off + i);
                    ctx.set_array_element(arr, dst_off + i, b);
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            let mut bytes = vec![0u8; len as usize];
            if ctx.read_byte_array_into(src, off, &mut bytes) != len as usize {
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = ctx.get_array_element(src, off + i).as_int().unwrap_or(0) as u8;
                }
            }
            if !ctx.copy_to_native_memory(addr.saturating_add(pos as i64), &bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.put([BII): direct destination write failed".to_string(),
                }
                .into());
            }
        } else {
            return Ok(Some(Value::Object(Some(this))));
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    }
    r.register(
        bb,
        "put",
        "([BII)Ljava/nio/ByteBuffer;",
        if bulk_session {
            s2_bb_put_bytes_range::<true>
        } else {
            s2_bb_put_bytes_range::<false>
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    fn s2_bb_put_bytes<const SESSION: bool>(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let mut src = obj_arg(args, 1)?;
        let len = ctx.array_length(src) as i32;
        let pos = s2_bb_pos(ctx, this);
        if pos + len > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        // `put(byte[])` is `put(src, 0, src.length)`: the same check as above.
        if SESSION && len > 0 {
            s2_bb_check_live_session(ctx, &mut this, &mut src)?;
        }
        if let Some(arr) = s2_bb_arr(ctx, this) {
            let base = s2_bb_heap_base(ctx, this);
            let dst_off = base + pos as usize;
            if !s2_bb_bulk_array_copy(ctx, src, 0, arr, dst_off, len as usize) {
                for i in 0..len as usize {
                    let b = ctx.get_array_element(src, i);
                    ctx.set_array_element(arr, dst_off + i, b);
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            let mut bytes = vec![0u8; len as usize];
            if ctx.read_byte_array_into(src, 0, &mut bytes) != len as usize {
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = ctx.get_array_element(src, i).as_int().unwrap_or(0) as u8;
                }
            }
            if !ctx.copy_to_native_memory(addr.saturating_add(pos as i64), &bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.put([B): direct destination write failed".to_string(),
                }
                .into());
            }
        } else {
            return Ok(Some(Value::Object(Some(this))));
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    }
    r.register(
        bb,
        "put",
        "([B)Ljava/nio/ByteBuffer;",
        if bulk_session {
            s2_bb_put_bytes::<true>
        } else {
            s2_bb_put_bytes::<false>
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    fn s2_bb_put_buffer<const SESSION: bool>(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        let mut src = obj_arg(args, 1)?;
        // `ByteBuffer.put(ByteBuffer src)` rejects a self-copy outright:
        // `if (src == this) throw createSameBufferException()`. Checked
        // FIRST, ahead of the read-only test, matching the JDK's own order
        // — a read-only buffer put into itself reports the
        // IllegalArgumentException, not ReadOnlyBufferException.
        //
        // CratonVM used to perform the copy. Since the 2026-07-31 bulk
        // rewrite that copy at least went through an owned intermediate,
        // so it was well defined rather than an overlapping element-wise
        // walk — but `ByteBuffer.wrap(new byte[16]).put(b)` still left
        // pos=16 where HotSpot throws, and any code doing it is already
        // broken on a real JVM.
        if src == this {
            return Err(RuntimeError::IllegalArgumentException {
                message: "The source buffer is this buffer".to_string(),
            }
            .into());
        }
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let src_pos = s2_bb_pos(ctx, src);
        let src_lim = s2_bb_limit(ctx, src);
        let n = (src_lim - src_pos).max(0) as usize;
        let pos = s2_bb_pos(ctx, this);
        if pos + n as i32 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        // `putBuffer` is one `SCOPED_MEMORY_ACCESS.copyMemory(src.session(),
        // session(), ...)`: the source's session first, then this one's, and
        // for every length (no zero-length shortcut, unlike `getArray`).
        if SESSION {
            s2_bb_check_live_session(ctx, &mut src, &mut this)?;
            s2_bb_check_live_session(ctx, &mut this, &mut src)?;
        }
        // Either side may be a DIRECT buffer (real-JDK `DirectByteBuffer`:
        // `hb == null`, storage at `address`). The pre-fix code silently
        // returned without copying OR advancing positions whenever a side
        // had no heap array. `IOUtil.read` routes every buffered
        // `FileChannel` read through a temporary direct buffer and then
        // `dst.put(directSrc)`, so file reads "succeeded" (count
        // returned) while delivering ZERO bytes with an unmoved
        // destination position — Lucene's `BufferedIndexInput.refill()`
        // then flipped an empty buffer and the first `readByte()` threw
        // `BufferUnderflowException` (doc:
        // ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object).
        let mut bytes = vec![0u8; n];
        if let Some(src_arr) = s2_bb_arr(ctx, src) {
            let src_base = s2_bb_heap_base(ctx, src);
            let src_start = src_base + src_pos as usize;
            if ctx.read_byte_array_into(src_arr, src_start, &mut bytes) != n {
                for (i, b) in bytes.iter_mut().enumerate() {
                    *b = ctx
                        .get_array_element(src_arr, src_start + i)
                        .as_int()
                        .unwrap_or(0) as u8;
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, src) {
            if !ctx.copy_from_native_memory(addr.saturating_add(src_pos as i64), &mut bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.put: direct source read failed".to_string(),
                }
                .into());
            }
        } else {
            // Genuinely storage-less synthetic buffer — keep the historic
            // silent no-op so half-built synthetic buffers stay benign.
            return Ok(Some(Value::Object(Some(this))));
        }
        if let Some(dst_arr) = s2_bb_arr(ctx, this) {
            let dst_base = s2_bb_heap_base(ctx, this);
            let dst_start = dst_base + pos as usize;
            if !ctx.write_byte_array_from(dst_arr, dst_start, &bytes) {
                for (i, b) in bytes.iter().enumerate() {
                    ctx.set_array_element(dst_arr, dst_start + i, Value::Int(*b as i8 as i32));
                }
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            if !ctx.copy_to_native_memory(addr.saturating_add(pos as i64), &bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.put: direct destination write failed".to_string(),
                }
                .into());
            }
        } else {
            return Ok(Some(Value::Object(Some(this))));
        }
        s2_bb_set_pos(ctx, src, src_lim);
        s2_bb_set_pos(ctx, this, pos + n as i32);
        Ok(Some(Value::Object(Some(this))))
    }
    r.register(
        bb,
        "put",
        "(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;",
        if bulk_session {
            s2_bb_put_buffer::<true>
        } else {
            s2_bb_put_buffer::<false>
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // getShort / putShort
    r.register(bb, "getShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read2(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Int(v as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "getShort", "(I)S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 2)?;
        Ok(Some(Value::Int(s2_bb_read2(ctx, this, idx) as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putShort", "(S)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write2(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putShort", "(IS)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        s2_bb_check_index(ctx, this, idx, 2)?;
        s2_bb_write2(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getChar / putChar
    r.register(bb, "getChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read2(ctx, this, pos) as u16;
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Int(v as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "getChar", "(I)C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 2)?;
        Ok(Some(Value::Int(s2_bb_read2(ctx, this, idx) as u16 as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putChar", "(C)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write2(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putChar", "(IC)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        s2_bb_check_index(ctx, this, idx, 2)?;
        s2_bb_write2(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getInt / putInt
    r.register(bb, "getInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read4(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Int(v)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "getInt", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 4)?;
        Ok(Some(Value::Int(s2_bb_read4(ctx, this, idx))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putInt", "(I)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write4(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putInt", "(II)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 4)?;
        s2_bb_write4(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getLong / putLong
    r.register(bb, "getLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read8(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Long(v)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "getLong", "(I)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 8)?;
        Ok(Some(Value::Long(s2_bb_read8(ctx, this, idx))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putLong", "(J)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = match args.get(1) {
            Some(Value::Long(l)) => *l,
            Some(Value::Int(i)) => *i as i64,
            _ => 0,
        };
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write8(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putLong", "(IJ)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = match args.get(2) {
            Some(Value::Long(l)) => *l,
            Some(Value::Int(i)) => *i as i64,
            _ => 0,
        };
        s2_bb_check_index(ctx, this, idx, 8)?;
        s2_bb_write8(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getFloat / putFloat
    r.register(bb, "getFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let bits = s2_bb_read4(ctx, this, pos) as u32;
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Float(f32::from_bits(bits))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "getFloat", "(I)F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_check_index(ctx, this, idx, 4)?;
        Ok(Some(Value::Float(f32::from_bits(
            s2_bb_read4(ctx, this, idx) as u32,
        ))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putFloat", "(F)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = match args.get(1) {
            Some(Value::Float(f)) => *f,
            Some(Value::Int(i)) => f32::from_bits(*i as u32),
            _ => 0.0,
        };
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write4(ctx, this, pos, v.to_bits() as i32);
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getDouble / putDouble
    r.register(bb, "getDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let bits = s2_bb_read8(ctx, this, pos) as u64;
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Double(f64::from_bits(bits))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "putDouble", "(D)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let v = match args.get(1) {
            Some(Value::Double(d)) => *d,
            _ => 0.0,
        };
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write8(ctx, this, pos, v.to_bits() as i64);
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // flip / clear / rewind / mark (both Buffer and ByteBuffer return types)
    for ret in &["()Ljava/nio/Buffer;", "()Ljava/nio/ByteBuffer;"] {
        let ret = *ret;
        r.register(bb, "flip", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            ctx.set_field(this, BB_LIMIT, Value::Int(pos));
            ctx.set_field(this, BB_POS, Value::Int(0));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(bb, "clear", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cap = s2_bb_cap(ctx, this);
            ctx.set_field(this, BB_POS, Value::Int(0));
            ctx.set_field(this, BB_LIMIT, Value::Int(cap));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(bb, "rewind", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, BB_POS, Value::Int(0));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(bb, "mark", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            s2_bb_set_mark(ctx, this, pos);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
    }
    r.register(bb, "reset", "()Ljava/nio/Buffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mark = s2_bb_get_mark(ctx, this);
        if mark < 0 {
            // THE TYPE, not a message that names the type. This raised
            // `IllegalStateException` whose detail message was the STRING
            // "InvalidMarkException" -- so `catch (InvalidMarkException)`, the
            // only handler anyone writes for this, did not match.
            //
            // `InvalidMarkException extends IllegalStateException`, so the
            // supertype handler still worked and nothing failed loudly. What
            // made it visible was the DESCRIPTOR: this registration covers
            // `()Ljava/nio/Buffer;` only, and javac emits that spelling solely
            // when the reference is typed `Buffer`. Through a `ByteBuffer`
            // reference the real bytecode runs and answers correctly, so the
            // bridge and its target DISAGREED:
            //
            //   ByteBuffer bb = ...;  bb.reset()   InvalidMarkException
            //   Buffer     b  = bb;   b.reset()    IllegalStateException
            //
            // MEASURED with `L4TailSweep2.java`, which types the
            // reference as `Buffer` for exactly this reason.
            //
            // Round 15 wave 4, lane jdkonly5 (R2 of
            // `r15w4-jdkonly4-builder-review-findings-CLOSED-20260930.md`): one
            // builder. `new_object` + an ignored `<init>` result threw the
            // half-built object when the constructor failed; now a constructor
            // that threw propagates its own throwable, and only "no such
            // class" takes the flat fallback.
            return Err(crate::throwable_mint::construct_thrown(
                ctx,
                "java/nio/InvalidMarkException",
                "()V",
                &[],
                || {
                    RuntimeError::IllegalStateException {
                        message: "InvalidMarkException".into(),
                    }
                    .into()
                },
            ));
        }
        ctx.set_field(this, BB_POS, Value::Int(mark));
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // position / limit
    r.register(bb, "position", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_POS)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    for ret in &["(I)Ljava/nio/Buffer;", "(I)Ljava/nio/ByteBuffer;"] {
        r.register(bb, "position", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            let limit = s2_bb_limit(ctx, this);
            if v < 0 || v > limit {
                // `Buffer.createPositionException`: "> limit" only when the
                // position is past the limit, else "< 0". A negative position
                // read `newPosition > limit: (-1 > 16)` (JIT round 17 wave 3,
                // lane compatbridge; `R17CompatBuffers` row `posneg`).
                let message = if v > limit {
                    format!("newPosition > limit: ({v} > {limit})")
                } else {
                    format!("newPosition < 0: ({v} < 0)")
                };
                return Err(RuntimeError::IllegalArgumentException { message }.into());
            }
            if s2_bb_get_mark(ctx, this) > v {
                s2_bb_set_mark(ctx, this, -1);
            }
            ctx.set_field(this, BB_POS, Value::Int(v));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
    }
    r.register(bb, "limit", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_LIMIT)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    for ret in &["(I)Ljava/nio/Buffer;", "(I)Ljava/nio/ByteBuffer;"] {
        r.register(bb, "limit", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            let cap = s2_bb_cap(ctx, this);
            if v < 0 || v > cap {
                // `Buffer.createLimitException`, the same split as
                // `position(int)` above (`R17CompatBuffers` row `limneg`).
                let message = if v > cap {
                    format!("newLimit > capacity: ({v} > {cap})")
                } else {
                    format!("newLimit < 0: ({v} < 0)")
                };
                return Err(RuntimeError::IllegalArgumentException { message }.into());
            }
            let pos = s2_bb_pos(ctx, this);
            if pos > v {
                ctx.set_field(this, BB_POS, Value::Int(v));
            }
            if s2_bb_get_mark(ctx, this) > v {
                s2_bb_set_mark(ctx, this, -1);
            }
            ctx.set_field(this, BB_LIMIT, Value::Int(v));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
    }
    r.register(bb, "capacity", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_CAP)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "remaining", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            (s2_bb_limit(ctx, this) - s2_bb_pos(ctx, this)).max(0),
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "hasRemaining", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            if s2_bb_limit(ctx, this) > s2_bb_pos(ctx, this) {
                1            } else {
                0            },
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "compact", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if s2_bb_is_read_only(ctx, this) {
            return Err(RuntimeError::ReadOnlyBufferException.into());
        }
        let pos = s2_bb_pos(ctx, this).max(0);
        let lim = s2_bb_limit(ctx, this).max(pos);
        let cap = s2_bb_cap(ctx, this);
        let n = (lim - pos) as usize;
        // Storage-aware move (residual-doc item 3: direct receivers were a
        // silent no-op). Reading the window first makes the overlapping
        // move safe for both storage kinds.
        if let Some(bytes) = s2_bb_read_window(ctx, this, pos, n) {
            match s2_bb_storage(ctx, this) {
                Some(S2BbStorage::Heap { arr, base }) => {
                    for (i, b) in bytes.iter().enumerate() {
                        ctx.set_array_element(arr, base + i, Value::Int(*b as i8 as i32));
                    }
                }
                Some(S2BbStorage::Direct { addr }) => {
                    let _ = ctx.copy_to_native_memory(addr, &bytes);
                }
                None => {}
            }
        }
        ctx.set_field(this, BB_POS, Value::Int(n as i32));
        ctx.set_field(this, BB_LIMIT, Value::Int(cap));
        s2_bb_set_mark(ctx, this, -1);
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // array / hasArray / isDirect / isReadOnly / arrayOffset — all
    // storage/flag-aware now (previously hardcoded heap-and-writable, so a
    // direct or read-only receiver answered wrong on every one of these).
    //
    // CONVERGED (F14-1 N1): the three-way decision is
    // `cratonvm_native_io::buffer_array_access`, the one copy of it the
    // dependency edge lets this crate import. The behaviour is unchanged —
    // `Accessible`/`ReadOnly` are the two arms that were here, and `Absent`
    // keeps this file's FOURTH arm (the storage-less synthetic's historic
    // benign null), which is a CratonVM-only state the JDK's two-valued
    // `hb == null` question has no cell for.
    r.register(bb, "array", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = s2_bb_arr(ctx, this);
        match buffer_array_access(arr.is_some(), s2_bb_is_read_only(ctx, this)) {
            BufferArrayAccess::Accessible => Ok(Some(Value::Object(arr))),
            BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
            BufferArrayAccess::Absent if s2_bb_direct_addr(ctx, this).is_some() => {
                Err(s2_bb_no_backing_array())
            }
            // Storage-less synthetic: keep the historic benign null.
            BufferArrayAccess::Absent => Ok(Some(Value::Object(None))),
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // `arrayOffset()` has the SAME two refusals as `array()` eighteen lines
    // above, and had neither — a direct receiver answered `0`, which is a
    // perfectly ordinary offset, so `hasArray()`-less code that reached for the
    // offset got a number instead of the exception that tells it to take the
    // direct path. The real JDK body is three lines and both of them are in it:
    //
    // ```java
    // if (hb == null)  throw new UnsupportedOperationException();
    // if (isReadOnly)  throw new ReadOnlyBufferException();
    // return offset;
    // ```
    //
    // Transcribed from the `array()` arm rather than written afresh, so the two
    // cannot drift; the storage classification and both `RuntimeError` variant
    // shapes are that arm's. Measured oracle rows, probes/DirectByteBufferStateProbe.expected.txt
    // on jdk-25.0.3.9: `direct.arrayOffset.throws`, `direct.win.arrayOffset.throws`
    // and `direct.win.readOnly.arrayOffset.throws` are `UnsupportedOperationException`;
    // `heap.win.readOnly.arrayOffset.throws` is `ReadOnlyBufferException`; the
    // happy paths are `heap.arrayOffset = 0` and `heap.win.arrayOffset = 4`, so
    // the window's base still has to come through. Record: W7-83 §7.1.
    r.register(bb, "arrayOffset", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match buffer_array_access(
            s2_bb_arr(ctx, this).is_some(),
            s2_bb_is_read_only(ctx, this),
        ) {
            BufferArrayAccess::Accessible => {
                Ok(Some(Value::Int(s2_bb_heap_base(ctx, this) as i32)))
            }
            BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
            BufferArrayAccess::Absent if s2_bb_direct_addr(ctx, this).is_some() => {
                Err(s2_bb_no_backing_array())
            }
            // Storage-less synthetic: keep the historic benign zero, for the
            // same reason `array()` keeps its historic benign null.
            BufferArrayAccess::Absent => Ok(Some(Value::Int(s2_bb_heap_base(ctx, this) as i32))),
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // `(hb != null) && !isReadOnly` — the SAME decision as the two arms above,
    // so it is the same call. Written out as a conjunction here, it was the
    // fifth transcription of a rule the JDK writes once.
    r.register(bb, "hasArray", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let access = buffer_array_access(
            s2_bb_arr(ctx, this).is_some(),
            s2_bb_is_read_only(ctx, this),
        );
        Ok(Some(Value::Int(i32::from(
            access == BufferArrayAccess::Accessible,
        ))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "isDirect", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            s2_bb_direct_addr(ctx, this).is_some() as i32
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "isReadOnly", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(s2_bb_is_read_only(ctx, this) as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // order
    r.register(bb, "order", "()Ljava/nio/ByteOrder;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ord = s2_bb_order(ctx, this);
        // Canonical (real-static) ByteOrder object: JDK and library
        // bytecode compares the result with `==` against
        // `ByteOrder.LITTLE_ENDIAN` (e.g. Lucene's `assert buffer.order()
        // == LITTLE_ENDIAN`). The helper also ensures the class is
        // INITIALIZED first — the previous lookup silently fell back to a
        // fresh synthetic (printing as BIG_ENDIAN, failing identity
        // comparisons) when `order()` ran before any Java-side ByteOrder
        // access had triggered <clinit> (residual-doc item 6).
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, ord)?))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        bb,
        "order",
        "(Ljava/nio/ByteOrder;)Ljava/nio/ByteBuffer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The argument is usually one of the REAL `ByteOrder` statics,
            // whose field 0 is the `name` String — the old
            // `get_field(bo, 0).as_int()` decode silently yielded 0
            // (BIG_ENDIAN) for every real constant, so
            // `order(LITTLE_ENDIAN)` was a no-op in real-JDK mode. Decode
            // both representations.
            let ord = match args.get(1) {
                Some(Value::Object(Some(bo))) => match ctx.get_field(*bo, 0) {
                    Value::Int(v) => v,
                    Value::Object(Some(name)) => match ctx.read_string(name).as_deref() {
                        Some("LITTLE_ENDIAN") => 1,
                        _ => 0,
                    },
                    _ => 0,
                },
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            s2_bb_set_order(ctx, this, ord);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // slice / slice(II) / duplicate / asReadOnlyBuffer — ALIASING views.
    //
    // The previous implementations copied the remaining bytes into a fresh
    // heap array (slice) or dropped the storage entirely for a DIRECT
    // source (all four) — residual-doc items 2/3: writes through a slice
    // never reached the parent, and every view over a direct buffer came
    // back empty/zero. Views now share the parent's storage exactly like
    // real-JDK buffers: heap views record an array-base `offset` (honoured
    // by every accessor via `s2_bb_heap_base`), direct views record the
    // advanced native `address`. The bare-synthetic 6-slot layout has no
    // `offset` field to carry a base, so it keeps the legacy copying
    // behaviour (data-correct, aliasing not representable).
    //
    // BYTE ORDER IS **NOT** CARRIED ACROSS ANY OF THESE FOUR. All four ran
    // `let ord = s2_bb_order(ctx, this)` and propagated it, and that is wrong in
    // a way no amount of aliasing correctness compensates for.
    //
    // The mechanism, because it is not obvious from any javadoc: each of these
    // four returns a NEW buffer built by a `ByteBuffer` constructor, and
    // `boolean bigEndian = true` is a FIELD INITIALISER on `ByteBuffer` — it
    // runs on every construction, so a derived view comes back BIG_ENDIAN
    // however the source was set. These methods preserve CONTENT, not ORDER.
    // And `order()` is `public final`, reading the field directly, so there is
    // no per-subclass override point at which a propagated order could be
    // corrected afterwards.
    //
    // The `s2` registrar WINS in Compatible mode and all four descriptors are
    // force-native, so the propagation was live:
    // `ByteBuffer.allocate(16).order(LITTLE_ENDIAN).slice().order()` answered
    // LITTLE_ENDIAN where HotSpot answers BIG_ENDIAN, and **every typed read
    // through such a view was byteswapped relative to HotSpot** — a wrong value,
    // not an exception, which is the quiet kind.
    //
    // Measured, jdk-25.0.3.9:
    // `{direct,heap}.ord.{slice,sliceRange,duplicate,readOnly}.order = BIG_ENDIAN`.
    // Record: W7-76 §10.
    //
    // THE EXCLUSION, and it is the reason this is a comment and not a one-line
    // diff: `as<T>Buffer()` DOES carry the order, and must keep doing so. It
    // reaches it through `s2_bb_order`'s `java/nio/ByteBufferAs…{B,L}`
    // class-name arm — a different mechanism at a different site — because the
    // JDK picks the `B` or the `L` view class from the source's order at
    // construction time. Do not "fix the inconsistency" by unifying the two.
    r.register(bb, "slice", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this).max(0);
        let lim = s2_bb_limit(ctx, this).max(pos);
        let rem = lim - pos;
        let ro = s2_bb_is_read_only(ctx, this);
        let ord = 0; // BIG_ENDIAN — HotSpot RESETS the order on a derived view; see the block comment above
        if s2_bb_synthetic_layout(ctx, this) {
            let new_arr = ctx.new_array(ArrayElementType::Byte, rem as usize);
            if let Some(src) = s2_bb_arr(ctx, this) {
                for i in 0..rem as usize {
                    let b = ctx.get_array_element(src, pos as usize + i);
                    ctx.set_array_element(new_arr, i, b);
                }
            }
            let cls = s2_bb_heap_class(ctx, false);
            let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
            bb_write_hb(ctx, buf, new_arr, rem);
            s2_bb_set_order(ctx, buf, ord);
            // `ro` was computed above and used ONLY by the two aliasing arms
            // below, so this copying arm dropped it. `bb_write_hb` writes
            // `isReadOnly = 0` unconditionally, so the flag was not merely
            // left unset — it was actively cleared, which is the wrong
            // CAPABILITY: the returned slice answers `hasArray() == true` and
            // hands its backing array over. MEASURED, jdk-25.0.3+9, all seven
            // families: `<fam>.ro.slice.isReadOnly OK true`. Written AFTER
            // `bb_write_hb` for exactly that reason.
            s2_bb_set_read_only(ctx, buf, ro);
            return Ok(Some(Value::Object(Some(buf))));
        }
        let buf = match s2_bb_storage(ctx, this) {
            Some(S2BbStorage::Heap { arr, base }) => {
                s2_bb_new_heap_view(ctx, arr, base + pos as usize, 0, rem, rem, -1, ro, ord)
            }
            Some(S2BbStorage::Direct { addr }) => s2_bb_new_direct_view(
                ctx,
                addr.saturating_add(pos as i64),
                0,
                rem,
                rem,
                -1,
                ro,
                ord,
            ),
            // Storage-less synthetic: keep the historic empty-copy result.
            None => {
                let new_arr = ctx.new_array(ArrayElementType::Byte, rem as usize);
                let cls = s2_bb_heap_class(ctx, false);
                let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
                bb_write_hb(ctx, buf, new_arr, rem);
                s2_bb_set_read_only(ctx, buf, ro);
                Ok(buf)
            }
        };
        Ok(Some(Value::Object(Some(buf?))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // JDK 13+ `slice(int index, int length)` — absolute-indexed aliasing
    // view, independent of position/limit. Abstract on the real class, so
    // an s2-stamped receiver needs this registration to avoid
    // AbstractMethodError (same pattern as the typed-buffer views).
    r.register(bb, "slice", "(II)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let index = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let length = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let lim = s2_bb_limit(ctx, this);
        s2_check_from_index_size(index, length, lim)?;
        let ro = s2_bb_is_read_only(ctx, this);
        let ord = 0; // BIG_ENDIAN — HotSpot RESETS the order on a derived view; see the block comment above
        let buf = match s2_bb_storage(ctx, this) {
            Some(S2BbStorage::Heap { arr, base }) if !s2_bb_synthetic_layout(ctx, this) => {
                s2_bb_new_heap_view(
                    ctx,
                    arr,
                    base + index as usize,
                    0,
                    length,
                    length,
                    -1,
                    ro,
                    ord,
                )
            }
            Some(S2BbStorage::Direct { addr }) => s2_bb_new_direct_view(
                ctx,
                addr.saturating_add(index as i64),
                0,
                length,
                length,
                -1,
                ro,
                ord,
            ),
            _ => {
                // Bare-synthetic / storage-less: copying fallback.
                let new_arr = ctx.new_array(ArrayElementType::Byte, length as usize);
                if let Some(src) = s2_bb_arr(ctx, this) {
                    for i in 0..length as usize {
                        let b = ctx.get_array_element(src, index as usize + i);
                        ctx.set_array_element(new_arr, i, b);
                    }
                }
                let cls = s2_bb_heap_class(ctx, false);
                let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
                bb_write_hb(ctx, buf, new_arr, length);
                s2_bb_set_order(ctx, buf, ord);
                s2_bb_set_read_only(ctx, buf, ro);
                Ok(buf)
            }
        };
        Ok(Some(Value::Object(Some(buf?))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bb, "duplicate", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        let lim = s2_bb_limit(ctx, this);
        let cap = s2_bb_cap(ctx, this);
        let mark = s2_bb_get_mark(ctx, this);
        let ro = s2_bb_is_read_only(ctx, this);
        let ord = 0; // BIG_ENDIAN — HotSpot RESETS the order on a derived view; see the block comment above
        let buf = match s2_bb_storage(ctx, this) {
            Some(S2BbStorage::Heap { arr, base }) if !s2_bb_synthetic_layout(ctx, this) => {
                s2_bb_new_heap_view(ctx, arr, base, pos, lim, cap, mark, ro, ord)
            }
            Some(S2BbStorage::Direct { addr }) => {
                s2_bb_new_direct_view(ctx, addr, pos, lim, cap, mark, ro, ord)
            }
            _ => {
                // Bare-synthetic / storage-less: legacy shared-array
                // rebuild (aliases the array, no offset support needed).
                let cls = s2_bb_heap_class(ctx, false);
                let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
                if let Some(src_arr) = s2_bb_arr(ctx, this) {
                    bb_write_hb(ctx, buf, src_arr, cap);
                    ctx.set_field_by_name(buf, "position", Value::Int(pos));
                    ctx.set_field_by_name(buf, "limit", Value::Int(lim));
                    ctx.set_field_by_name(buf, "mark", Value::Int(mark));
                    ctx.set_field(buf, BB_POS, Value::Int(pos));
                    ctx.set_field(buf, BB_LIMIT, Value::Int(lim));
                    ctx.set_field(buf, BB_MARK, Value::Int(mark));
                }
                ctx.set_field(buf, BB_ORDER, Value::Int(ord));
                s2_bb_set_read_only(ctx, buf, ro);
                Ok(buf)
            }
        };
        Ok(Some(Value::Object(Some(buf?))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        bb,
        "asReadOnlyBuffer",
        "()Ljava/nio/ByteBuffer;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            let lim = s2_bb_limit(ctx, this);
            let cap = s2_bb_cap(ctx, this);
            let mark = s2_bb_get_mark(ctx, this);
            let ord = 0; // BIG_ENDIAN — HotSpot RESETS the order on a derived view; see the block comment above
            let buf = match s2_bb_storage(ctx, this) {
                Some(S2BbStorage::Heap { arr, base }) if !s2_bb_synthetic_layout(ctx, this) => {
                    s2_bb_new_heap_view(ctx, arr, base, pos, lim, cap, mark, true, ord)
                }
                Some(S2BbStorage::Direct { addr }) => {
                    s2_bb_new_direct_view(ctx, addr, pos, lim, cap, mark, true, ord)
                }
                _ => {
                    let cls = s2_bb_heap_class(ctx, false);
                    let buf = try_alloc_concurrent_synthetic(ctx, cls, 6)?;
                    if let Some(src_arr) = s2_bb_arr(ctx, this) {
                        bb_write_hb(ctx, buf, src_arr, cap);
                        ctx.set_field_by_name(buf, "position", Value::Int(pos));
                        ctx.set_field_by_name(buf, "limit", Value::Int(lim));
                        ctx.set_field_by_name(buf, "mark", Value::Int(mark));
                        ctx.set_field(buf, BB_POS, Value::Int(pos));
                        ctx.set_field(buf, BB_LIMIT, Value::Int(lim));
                        ctx.set_field(buf, BB_MARK, Value::Int(mark));
                    }
                    ctx.set_field(buf, BB_ORDER, Value::Int(ord));
                    // MOVED OUT of the `if let` above, which is the whole
                    // change on this arm: `asReadOnlyBuffer()` on a receiver
                    // with no resolvable backing array returned a buffer with
                    // `isReadOnly` never written — a WRITABLE result from the
                    // one method whose entire contract is that its result is
                    // not writable. It still has to follow `bb_write_hb`,
                    // which writes `isReadOnly = 0`. MEASURED, jdk-25.0.3+9:
                    // `asReadOnlyBuffer().isReadOnly()` is `true` for every
                    // receiver in all seven families, and there is no receiver
                    // for which it is conditional.
                    s2_bb_set_read_only(ctx, buf, true);
                    Ok(buf)
                }
            };
            Ok(Some(Value::Object(Some(buf?))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // asXxxBuffer view buffers — each needs a named function (no closure captures)
    r.register(
        bb,
        "asIntBuffer",
        "()Ljava/nio/IntBuffer;",
        s2_bb_as_int_buffer, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        bb,
        "asLongBuffer",
        "()Ljava/nio/LongBuffer;",
        s2_bb_as_long_buffer, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        bb,
        "asShortBuffer",
        "()Ljava/nio/ShortBuffer;",
        s2_bb_as_short_buffer, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        bb,
        "asFloatBuffer",
        "()Ljava/nio/FloatBuffer;",
        s2_bb_as_float_buffer, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        bb,
        "asDoubleBuffer",
        "()Ljava/nio/DoubleBuffer;",
        s2_bb_as_double_buffer, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        bb,
        "asCharBuffer",
        "()Ljava/nio/CharBuffer;",
        s2_bb_as_char_buffer, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // IntBuffer get/put (positions in int units; byte_start from BB_MARK)
    let ib = "java/nio/IntBuffer";
    r.register(ib, "get", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let bs = {
            let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            if m < 0 {
                -(m + 1)
            } else {
                0
            }
        };
        // B8: `pos * 4` and the following add can overflow for a corrupt
        // position; `s2_bb_int_byte_off` saturates to a negative sentinel that
        // the byte accessors treat as out-of-range.
        let v = s2_bb_read4(ctx, this, s2_bb_int_byte_off(bs, pos));
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Int(v)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ib, "get", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let bs = {
            let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            if m < 0 {
                -(m + 1)
            } else {
                0
            }
        };
        Ok(Some(Value::Int(s2_bb_read4(
            ctx,
            this,
            s2_bb_int_byte_off(bs, idx),
        ))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ib, "put", "(I)Ljava/nio/IntBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        let bs = {
            let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            if m < 0 {
                -(m + 1)
            } else {
                0
            }
        };
        s2_bb_write4(ctx, this, s2_bb_int_byte_off(bs, pos), v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ib, "put", "(II)Ljava/nio/IntBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let bs = {
            let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            if m < 0 {
                -(m + 1)
            } else {
                0
            }
        };
        s2_bb_write4(ctx, this, s2_bb_int_byte_off(bs, idx), v);
        Ok(Some(Value::Object(Some(this))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // Shared Buffer methods for all view-buffer types
    for cls in &[
        "java/nio/IntBuffer",
        "java/nio/LongBuffer",
        "java/nio/ShortBuffer",
        "java/nio/FloatBuffer",
        "java/nio/DoubleBuffer",
    ] {
        let cls = *cls;
        // NIO2-17-1: slots 1..3 are the real `Buffer` fields on the typed
        // views too, so `Buffer`'s bytecode reads what these read.
        r.register(cls, "position", "()I", |ctx, args| {
            Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_POS)))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(cls, "limit", "()I", |ctx, args| {
            Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_LIMIT)))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(cls, "capacity", "()I", |ctx, args| {
            Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_CAP)))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(cls, "remaining", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(
                (s2_bb_limit(ctx, this) - s2_bb_pos(ctx, this)).max(0),
            )))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(cls, "hasRemaining", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(
                if s2_bb_limit(ctx, this) > s2_bb_pos(ctx, this) {
                    1                } else {
                    0                },
            )))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        // `flip`/`clear`/`rewind` DISCARD THE MARK. These two bodies are the
        // `ByteBuffer` twins forty lines up with the `s2_bb_set_mark` line
        // dropped, and the omission is invisible from this class's own
        // methods: `mark()` and `reset()` are NOT registered for the typed
        // buffers, so the real `Buffer` bytecode sets and reads the real
        // `mark` field while these natives move position and limit in the
        // side slots. Nothing reconciles them.
        //
        //   IntBuffer ib = ...;  Buffer b = ib;
        //   b.mark(); b.flip(); b.reset();
        //     HotSpot   InvalidMarkException
        //     this VM   no throw, and the position silently jumps back into
        //               a region flip just excluded
        //
        // MEASURED with `L4BridgeSweep.java` on Short/Int/Long/
        // Float/DoubleBuffer, heap and view arms alike -- 11 rows. `ByteBuffer`
        // was green because its twin has the line, and `CharBuffer` because
        // `charset_buffers.rs` has it too; this loop is the one copy that
        // never got it.
        //
        // Only reachable through a `Buffer`-typed reference, since that is the
        // only spelling javac emits for `()Ljava/nio/Buffer;` -- which is why
        // 501 rows of `L4TypedBufferSweep` over these exact classes never saw
        // it.
        r.register(cls, "flip", "()Ljava/nio/Buffer;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            ctx.set_field(this, BB_LIMIT, Value::Int(pos));
            ctx.set_field(this, BB_POS, Value::Int(0));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(cls, "clear", "()Ljava/nio/Buffer;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cap = s2_bb_cap(ctx, this);
            ctx.set_field(this, BB_POS, Value::Int(0));
            ctx.set_field(this, BB_LIMIT, Value::Int(cap));
            s2_bb_set_mark(ctx, this, -1);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        // The element type differs per family, and this loop registered all
        // FIVE under `()[I`. Measured with `--dump-native-registry`
        // (2026-08-13): four PHANTOM rows nothing can dispatch to
        // (`DoubleBuffer.array()[I` etc.), and for `IntBuffer` — the one whose
        // descriptor happened to be right — this row OWNED the slot, so
        // `native-io`'s `native_tb_array` and its F14-1 refusals never ran.
        //
        // The body was `Ok(Some(Value::Object(s2_bb_arr(...))))`: a `None`
        // became a silent Java `null`, so `ByteBuffer.allocate(16)
        // .asIntBuffer().array().length` raised NullPointerException where
        // HotSpot throws UnsupportedOperationException. That is the same
        // wrong-capability shape F14-1 fixed in the twin, in the copy that
        // actually wins — "diff a family fix against every member".
        //
        // `Absent` is UOE here with no fourth arm: unlike `ByteBuffer` above,
        // every receiver of a typed view is array-less in the JDK, and F37-1
        // measured views answering UnsupportedOperationException.
        let array_desc = match cls {
            "java/nio/LongBuffer" => "()[J",
            "java/nio/ShortBuffer" => "()[S",
            "java/nio/FloatBuffer" => "()[F",
            "java/nio/DoubleBuffer" => "()[D",
            _ => "()[I",
        };
        r.register(cls, "array", array_desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = s2_bb_arr(ctx, this);
            match buffer_array_access(arr.is_some(), s2_bb_is_read_only(ctx, this)) {
                BufferArrayAccess::Accessible => Ok(Some(Value::Object(arr))),
                BufferArrayAccess::ReadOnly => Err(RuntimeError::ReadOnlyBufferException.into()),
                BufferArrayAccess::Absent => Err(s2_bb_no_backing_array()),
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        // `isDirect` — IMPLEMENTED wave 4 (2026-07-28). The wave-3 note here
        // claimed "every receiver is a heap view backed by the int[]", which is
        // false: `s2_view_buf_fn!` has a second branch — when the SOURCE
        // ByteBuffer is direct (`s2_bb_direct_addr`), the view it hands out has
        // NO backing array and carries a native `address` instead. Real JDK
        // agrees that such a view is direct
        // (`allocateDirect(n).asIntBuffer().isDirect() == true`), so the
        // constant `false` was wrong for exactly that case — and wrong in the
        // direction that makes callers take the "copy via array()" path on a
        // buffer that has no array. Answer from the same storage probe every
        // accessor in this file uses: `s2_bb_direct_addr` returns `Some` only
        // when `s2_bb_arr` finds nothing AND a positive address slot exists.
        r.register(cls, "isDirect", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(
                if s2_bb_direct_addr(ctx, this).is_some() {
                    1                } else {
                    0                },
            )))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        // `isReadOnly` — NO LONGER A CONSTANT (F26-1, closing F21-1's N3).
        //
        // The comment this replaces argued the `Int(0)` was EXACT, and its
        // argument was sound when written: `$ro` aliased `$dup` and so could
        // not mint a read-only view, therefore no receiver reaching here could
        // be read-only. **F21 falsified the premise from the other crate.**
        // `native-io`'s `tb_abstract_view_fns!` `$ro_fn` now stamps
        // `isReadOnly = 1` by name, and `register_io_natives` runs AFTER
        // `register_essential_natives_with_shims` in both `vm_init` real-JDK
        // arms (VERIFIED: `vm/src/vm/vm_init.rs` L2055 < L2252 and L2593 <
        // L2788; registration is last-write-wins), so it is native-io's
        // `asReadOnlyBuffer` that answers for these five classes and read-only
        // typed views DO now exist.
        //
        // Left alone the constant reproduced, in the sibling, exactly the
        // impossible state that got `native_bb_is_read_only` fixed: `array()`
        // and `arrayOffset()` raise `ReadOnlyBufferException` and `hasArray()`
        // answers `false` (all three read the flag via
        // `native_io::buffer_array_access`), while `isReadOnly()` said the
        // buffer was writable — so a caller that branches on `isReadOnly()`
        // instead of `hasArray()` is steered straight into the throw.
        // MEASURED, jdk-25.0.3+9 (`scratchpad/f26/F26AliasProbe.java` §7):
        // `allocate(32).asReadOnlyBuffer().asIntBuffer()` is a
        // `java.nio.ByteBufferAsIntBufferRB` with `isReadOnly() == true`, and
        // the same holds for Long/Short/Float/Double/Char and for the direct
        // twin (`DirectIntBufferRS`, also `true`).
        //
        // **THE WIDTH CLAIM, CHECKED RATHER THAN INHERITED.** The old comment
        // said this needs "a wider allocation" because "the 6-slot synthetic is
        // full". That is true of the SYNTHETIC layout and irrelevant to this
        // registration, in both directions:
        //
        //   * Real-JDK mode: `java.nio.IntBuffer` DECLARES `boolean isReadOnly`
        //     (VERIFIED, `javap -p java.nio.IntBuffer` on 25.0.3+9: `int[] hb;
        //     int offset; boolean isReadOnly;` — identical on Long/Short/Float/
        //     Double), and `VmExec::alloc_object` clamps a native allocator's
        //     requested slot count UP to the resolved class's declared field
        //     count (VERIFIED, `vm/src/vm/vm_exec.rs` "Layout-mismatch guard").
        //     So the field is present on every one of these receivers and the
        //     by-name read resolves. Nothing needs widening.
        //   * Synthetic-JDK mode: there is no such field, `get_field_by_name`
        //     yields a non-`Int`, and `s2_bb_is_read_only` answers `false` —
        //     byte-for-byte the constant this replaces. So this is a no-op in
        //     the mode the width argument was about.
        //
        // Reads the same FIELD through the same converged helper as
        // `hasArray`/`array`/`arrayOffset`, so the four accessors on one
        // receiver cannot disagree.
        //
        // DISCLOSED RESIDUAL, deliberately not fixed here: the `$put` /
        // `$put_abs` / `$compact` arms of `s2_typed_buffer_view_fns!` still do
        // not consult the flag, so a write through a read-only typed view
        // succeeds where HotSpot raises `ReadOnlyBufferException` (MEASURED:
        // `allocate(32).asReadOnlyBuffer().asIntBuffer().put(0,1)` →
        // `java.nio.ReadOnlyBufferException`). Making this accessor honest is
        // strictly a step toward that and cannot be a step away from it: it
        // moves one more accessor onto the flag the enforcement will read.
        r.register(cls, "isReadOnly", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(s2_bb_is_read_only(ctx, this)))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
    }

    // `order`/`slice`/`slice(int,int)`/`duplicate`/`asReadOnlyBuffer` are
    // `public abstract` on every typed NIO buffer subclass in real JDK 25
    // (unlike ByteBuffer, where they're concrete bytecode) — `get`/`put`
    // (relative + absolute) and `compact` are abstract too, but only
    // IntBuffer has working overrides below (legacy). Every view-buffer
    // instance handed out by `s2_view_buf_fn!` above (`ByteBuffer.asXxxBuffer()`)
    // is stamped with the LITERAL abstract class name, so an `invokevirtual`
    // for any of these against that receiver resolves to a Code-less abstract
    // declaration and throws AbstractMethodError unless registered directly
    // here. See
    // ES-FAIL-FAMILY-20260710-floatbuffer-abstract-receiver-nocode-FIXED.md
    // (found via `FloatBuffer.order()`/`put(int,float)` on a raw vector slice
    // view — `ES814HnswScalarQuantizedVectorsFormatTests.testRescoreUsesRawVectorSlice`
    // — and `IntBuffer.order()` in `PreconditionerTests`).
    //
    // `bs` below is the byte-offset (within the shared backing byte[]) where
    // this view's element 0 starts — encoded by `s2_view_buf_fn!`/`slice`/
    // `slice(II)` as `-(bs+1)` in the indexed `BB_MARK` slot (mirroring the
    // existing IntBuffer get/put below, which reads it the same way).
    macro_rules! s2_typed_buffer_view_fns {
        (
            $get:ident, $get_abs:ident, $put:ident, $put_abs:ident,
            $order:ident, $slice:ident, $slice2:ident, $dup:ident, $ro:ident, $compact:ident,
            $get_bulk:ident, $put_bulk:ident,
            $cls:literal, $width:expr, $read:ident, $write:ident,
            $to_value:expr, $from_value:expr
        ) => {
            fn $get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let pos = s2_bb_pos(ctx, this);
                if pos >= s2_bb_limit(ctx, this) {
                    return Err(RuntimeError::BufferUnderflowException.into());
                }
                let bs = s2_typed_view_byte_start(ctx, this);
                let off = pos
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(-1);
                let raw = $read(ctx, this, off);
                ctx.set_field(this, BB_POS, Value::Int(pos + 1));
                Ok(Some(($to_value)(raw)))
            }
            fn $get_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                let bs = s2_typed_view_byte_start(ctx, this);
                let off = idx
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(-1);
                let raw = $read(ctx, this, off);
                Ok(Some(($to_value)(raw)))
            }
            /// `put(x)` — F37-1 §3, landing F26-1 §9.2 / F21-1 N3's residual.
            ///
            /// The read-only check is FIRST, before the overflow check, and
            /// that ordering is MEASURED, not assumed: JDK 25's
            /// `ByteBufferAsIntBufferRB.put(int)` is a bare
            /// `throw new ReadOnlyBufferException();` with no bounds test
            /// above it (`jdk25src/.../ByteBufferAsIntBufferRB.java:166`), so
            /// a read-only view at `position == limit` answers
            /// `ReadOnlyBufferException`, not `BufferOverflowException`.
            ///
            /// MEASURED, jdk-25.0.3+9 (`scratchpad/f37/F37TypedAliasProbe.java`):
            /// `allocate(64).asReadOnlyBuffer().asIntBuffer().put(1)` →
            /// `java.nio.ReadOnlyBufferException`, `getMessage()` null, and the
            /// same for all five families and for the direct twin
            /// (`DirectIntBufferRS`).
            fn $put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let mut this = obj_arg(args, 0)?;
                if s2_buf_read_only(ctx, &mut this) {
                    return Err(RuntimeError::ReadOnlyBufferException.into());
                }
                let v = args.get(1).cloned().unwrap_or(Value::Int(0));
                let pos = s2_bb_pos(ctx, this);
                if pos >= s2_bb_limit(ctx, this) {
                    return Err(RuntimeError::BufferOverflowException.into());
                }
                let bs = s2_typed_view_byte_start(ctx, this);
                let off = pos
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(-1);
                $write(ctx, this, off, ($from_value)(&v));
                ctx.set_field(this, BB_POS, Value::Int(pos + 1));
                Ok(Some(Value::Object(Some(this))))
            }
            /// `put(index, x)` — see `$put`. Same class, same null message.
            fn $put_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let mut this = obj_arg(args, 0)?;
                if s2_buf_read_only(ctx, &mut this) {
                    return Err(RuntimeError::ReadOnlyBufferException.into());
                }
                let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                let v = args.get(2).cloned().unwrap_or(Value::Int(0));
                let bs = s2_typed_view_byte_start(ctx, this);
                let off = idx
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(-1);
                $write(ctx, this, off, ($from_value)(&v));
                Ok(Some(Value::Object(Some(this))))
            }
            fn $order(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                // Return the canonical (real-static) ByteOrder object — a
                // fresh synthetic here broke identity comparisons
                // (`view.order() == ByteOrder.LITTLE_ENDIAN`) and printed
                // as BIG_ENDIAN regardless of value in real-JDK mode
                // (residual-doc item 6).
                let ord = s2_bb_order(ctx, this);
                Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, ord)?))))
            }
            fn $slice(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let pos = s2_bb_pos(ctx, this);
                let lim = s2_bb_limit(ctx, this);
                let remaining = (lim - pos).max(0);
                let bs = s2_typed_view_byte_start(ctx, this);
                let new_bs = pos
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(bs);
                let vb = try_alloc_concurrent_synthetic(ctx, $cls, 6)?;
                // `s2_bb_heap_window`, not `s2_bb_arr`: the receiver may be a
                // real-JDK `ByteBufferAs<T>Buffer{B,L}`, whose array lives on
                // its backing `bb` and whose byte start is carried in
                // `address` rather than in the `BB_MARK` marker. The derived
                // view is abstract-stamped with base 0, so fold the resolved
                // window base into the marker it WILL read back.
                if let Some((arr, base)) = s2_bb_heap_window(ctx, this) {
                    ctx.set_field(vb, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
                    let abs = (base as i32).saturating_add(new_bs);
                    ctx.set_field(vb, BB_MARK, Value::Int(-(abs + 1)));
                } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
                    // DIRECT view: alias the native storage at the sliced
                    // element position (residual-doc item 3).
                    ctx.set_field_by_name(
                        vb,
                        "address",
                        Value::Long(addr.saturating_add((new_bs as i64).max(0))),
                    );
                }
                ctx.set_field(vb, BB_POS, Value::Int(0));
                ctx.set_field(vb, BB_LIMIT, Value::Int(remaining));
                ctx.set_field(vb, BB_CAP, Value::Int(remaining));
                let ord = s2_bb_order(ctx, this);
                s2_bb_set_order(ctx, vb, ord);
                // Read-only is CONTAGIOUS: `slice()`, `slice(int,int)` and
                // `duplicate()` INHERIT the source's flag (MEASURED, all seven
                // families, F21-1 §1.1; `<fam>.ro.slice.isReadOnly` true,
                // `<fam>.w.slice.isReadOnly` false). Landed together with the
                // `$put`/`$put_abs`/`$put_bulk`/`$compact` guards above, and it
                // has to be: a guard on the receiver that a `slice()` one call
                // later hands back writable is not a guard, it is a delay. Same
                // "the capability re-opens one call later" shape F21-1 closed
                // for ByteBuffer, and it would have been re-opened HERE by the
                // very change that closed the direct route.
                //
                // `s2_bb_set_read_only` writes by NAME, so on a bare 6-slot
                // synthetic carrier — which is what `try_alloc_concurrent_
                // synthetic($cls, 6)` mints in synthetic-JDK mode — it is a
                // no-op and nothing observable changes there.
                let src_read_only = s2_bb_is_read_only(ctx, this);
                s2_bb_set_read_only(ctx, vb, src_read_only);
                Ok(Some(Value::Object(Some(vb))))
            }
            fn $slice2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let index = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                let length = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let cap = s2_bb_cap(ctx, this);
                s2_check_from_index_size(index, length, cap)?;
                let bs = s2_typed_view_byte_start(ctx, this);
                let new_bs = index
                    .checked_mul($width)
                    .and_then(|b| bs.checked_add(b))
                    .unwrap_or(bs);
                let vb = try_alloc_concurrent_synthetic(ctx, $cls, 6)?;
                // See `$slice` for why this resolves through
                // `s2_bb_heap_window` and folds the window base in.
                if let Some((arr, base)) = s2_bb_heap_window(ctx, this) {
                    ctx.set_field(vb, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
                    let abs = (base as i32).saturating_add(new_bs);
                    ctx.set_field(vb, BB_MARK, Value::Int(-(abs + 1)));
                } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
                    ctx.set_field_by_name(
                        vb,
                        "address",
                        Value::Long(addr.saturating_add((new_bs as i64).max(0))),
                    );
                }
                ctx.set_field(vb, BB_POS, Value::Int(0));
                ctx.set_field(vb, BB_LIMIT, Value::Int(length));
                ctx.set_field(vb, BB_CAP, Value::Int(length));
                let ord = s2_bb_order(ctx, this);
                s2_bb_set_order(ctx, vb, ord);
                // Read-only is CONTAGIOUS: `slice()`, `slice(int,int)` and
                // `duplicate()` INHERIT the source's flag (MEASURED, all seven
                // families, F21-1 §1.1; `<fam>.ro.slice.isReadOnly` true,
                // `<fam>.w.slice.isReadOnly` false). Landed together with the
                // `$put`/`$put_abs`/`$put_bulk`/`$compact` guards above, and it
                // has to be: a guard on the receiver that a `slice()` one call
                // later hands back writable is not a guard, it is a delay. Same
                // "the capability re-opens one call later" shape F21-1 closed
                // for ByteBuffer, and it would have been re-opened HERE by the
                // very change that closed the direct route.
                //
                // `s2_bb_set_read_only` writes by NAME, so on a bare 6-slot
                // synthetic carrier — which is what `try_alloc_concurrent_
                // synthetic($cls, 6)` mints in synthetic-JDK mode — it is a
                // no-op and nothing observable changes there.
                let src_read_only = s2_bb_is_read_only(ctx, this);
                s2_bb_set_read_only(ctx, vb, src_read_only);
                Ok(Some(Value::Object(Some(vb))))
            }
            fn $dup(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let this = obj_arg(args, 0)?;
                let pos = s2_bb_pos(ctx, this);
                let lim = s2_bb_limit(ctx, this);
                let cap = s2_bb_cap(ctx, this);
                let vb = try_alloc_concurrent_synthetic(ctx, $cls, 6)?;
                // Re-derive the marker rather than copying `BB_MARK` raw: on a
                // real-JDK `ByteBufferAs<T>Buffer{B,L}` receiver that slot is
                // `Buffer.mark` (-1), and the byte start lives in `address`.
                // For an abstract-stamped synthetic view this reproduces the
                // old copy exactly (base 0, byte start already the marker).
                if let Some((arr, base)) = s2_bb_heap_window(ctx, this) {
                    ctx.set_field(vb, BB_SEGMENT_SLOT, Value::Object(Some(arr)));
                    let abs = (base as i32).saturating_add(s2_typed_view_byte_start(ctx, this));
                    ctx.set_field(vb, BB_MARK, Value::Int(-(abs + 1)));
                } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
                    // DIRECT duplicate: same native storage, same window.
                    ctx.set_field_by_name(vb, "address", Value::Long(addr));
                }
                ctx.set_field(vb, BB_POS, Value::Int(pos));
                ctx.set_field(vb, BB_LIMIT, Value::Int(lim));
                ctx.set_field(vb, BB_CAP, Value::Int(cap));
                let ord = s2_bb_order(ctx, this);
                s2_bb_set_order(ctx, vb, ord);
                // Read-only is CONTAGIOUS: `slice()`, `slice(int,int)` and
                // `duplicate()` INHERIT the source's flag (MEASURED, all seven
                // families, F21-1 §1.1; `<fam>.ro.slice.isReadOnly` true,
                // `<fam>.w.slice.isReadOnly` false). Landed together with the
                // `$put`/`$put_abs`/`$put_bulk`/`$compact` guards above, and it
                // has to be: a guard on the receiver that a `slice()` one call
                // later hands back writable is not a guard, it is a delay. Same
                // "the capability re-opens one call later" shape F21-1 closed
                // for ByteBuffer, and it would have been re-opened HERE by the
                // very change that closed the direct route.
                //
                // `s2_bb_set_read_only` writes by NAME, so on a bare 6-slot
                // synthetic carrier — which is what `try_alloc_concurrent_
                // synthetic($cls, 6)` mints in synthetic-JDK mode — it is a
                // no-op and nothing observable changes there.
                let src_read_only = s2_bb_is_read_only(ctx, this);
                s2_bb_set_read_only(ctx, vb, src_read_only);
                Ok(Some(Value::Object(Some(vb))))
            }
            /// `asReadOnlyBuffer()` — F37-1 §3, landing F26-1 §9.4 item 1.
            ///
            /// **This was a plain `$dup(ctx, args)`**, i.e. the one method
            /// whose entire contract is that its result cannot be written
            /// returned a WRITABLE alias. That was self-consistent only while
            /// nothing on these classes consulted the flag; `$put`/`$put_abs`/
            /// `$put_bulk`/`$compact` above now do, so leaving it would have
            /// produced the state F21-1 §6.1 named — a read-only view that
            /// still accepts writes — in the sibling registrar rather than
            /// closing it.
            ///
            /// `ForceReadOnly`, not `Inherit`: MEASURED, `asReadOnlyBuffer()`
            /// is unconditional and one-way in all seven families
            /// (`<fam>.w.aro.isReadOnly` true, `<fam>.ro.aro.isReadOnly` true,
            /// and there is no `asWritableBuffer` anywhere in java.nio). The
            /// write goes AFTER `$dup` returns, because `$dup` mints a fresh
            /// 6-slot carrier and now stamps `Inherit` on it.
            fn $ro(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let result = $dup(ctx, args)?;
                if let Some(Value::Object(Some(view))) = result {
                    s2_bb_set_read_only(ctx, view, true);
                }
                Ok(result)
            }
            /// `compact()` — see `$put`. `compact` is a WRITE (it moves the
            /// remaining elements down to index 0), so a read-only receiver
            /// refuses it: MEASURED
            /// `allocate(64).asReadOnlyBuffer().asIntBuffer().compact()` →
            /// `java.nio.ReadOnlyBufferException`. The sibling
            /// `ByteBuffer.compact()` registration in this file has had this
            /// guard since F5; the typed views never did.
            fn $compact(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let mut this = obj_arg(args, 0)?;
                if s2_buf_read_only(ctx, &mut this) {
                    return Err(RuntimeError::ReadOnlyBufferException.into());
                }
                let pos = s2_bb_pos(ctx, this);
                let lim = s2_bb_limit(ctx, this);
                let cap = s2_bb_cap(ctx, this);
                let bs = s2_typed_view_byte_start(ctx, this);
                let remaining = (lim - pos).max(0);
                for i in 0..remaining {
                    let src_off = (pos + i)
                        .checked_mul($width)
                        .and_then(|b| bs.checked_add(b))
                        .unwrap_or(-1);
                    let dst_off = i
                        .checked_mul($width)
                        .and_then(|b| bs.checked_add(b))
                        .unwrap_or(-1);
                    let raw = $read(ctx, this, src_off);
                    $write(ctx, this, dst_off, raw);
                }
                ctx.set_field(this, BB_POS, Value::Int(remaining));
                ctx.set_field(this, BB_LIMIT, Value::Int(cap));
                Ok(Some(Value::Object(Some(this))))
            }
            // Bulk `get`/`put(T[], off, len)` are CONCRETE (not abstract) on
            // every typed buffer — real JDK 25's implementation
            // (`FloatBuffer.getArray`/`putArray` etc.) reads/writes via
            // `this.address` + `ScopedMemoryAccess` directly for anything
            // beyond a trivial length, COMPLETELY bypassing virtual dispatch
            // to the single-element accessors above. Our synthetic
            // abstract-stamped view never sets a real `address`, so that
            // fast path silently read/wrote zero bytes — even after the
            // single-element get/put fix above, any bulk vector read (the
            // overwhelmingly common case: `buffer.get(vec, 0, dims)`) still
            // came back all-zero. Registering natives here is not enough by
            // itself either: these methods HAVE real Code, so the registered
            // native is inert unless force-listed in
            // `force_native_over_real_jdk_bytecode` (vm/src/runtime/interpreter.rs)
            // — added there alongside this fix.
            /// `SESSION`: check a segment-backed view's session after the
            /// bounds checks, for a length > 0, as `getArray` does (round 17
            /// wave 8, lane nio3; `s2_bb_check_live_session`).
            fn $get_bulk<const SESSION: bool>(
                ctx: &mut dyn NativeContext,
                args: &[Value],
            ) -> MethodCallResult {
                let mut this = obj_arg(args, 0)?;
                let mut dst = obj_arg(args, 1)?;
                let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                let pos = s2_bb_pos(ctx, this);
                let dst_len = i32::try_from(ctx.array_length(dst)).unwrap_or(i32::MAX);
                s2_check_from_index_size(off, len, dst_len)?;
                if pos
                    .checked_add(len)
                    .map_or(true, |e| e > s2_bb_limit(ctx, this))
                {
                    return Err(RuntimeError::BufferUnderflowException.into());
                }
                if SESSION && len > 0 {
                    s2_bb_check_live_session(ctx, &mut this, &mut dst)?;
                }
                let bs = s2_typed_view_byte_start(ctx, this);
                for i in 0..len {
                    let src_off = (pos + i)
                        .checked_mul($width)
                        .and_then(|b| bs.checked_add(b))
                        .unwrap_or(-1);
                    let raw = $read(ctx, this, src_off);
                    ctx.set_array_element(dst, (off + i) as usize, ($to_value)(raw));
                }
                ctx.set_field(this, BB_POS, Value::Int(pos + len));
                Ok(Some(Value::Object(Some(this))))
            }
            /// `put(T[], off, len)` — see `$put`.
            ///
            /// **This is the one write path of the four that is NOT shadowed**,
            /// and therefore the one whose repair is reachable today.
            /// `register_io_natives` runs after `register_essential_natives_
            /// with_shims` in every `vm_init` arm (VERIFIED: real-JDK L2252 >
            /// L2055 and L2834 > L2639; synthetic L1935 > L1934, since
            /// `register_builtins` reaches essentials through
            /// `register_essential_natives`), and `native-io` re-registers
            /// `put(X)` / `put(I X)` / `compact()` for all five classes — but
            /// NOT the bulk `put([XII)`. So `$put`/`$put_abs`/`$compact` above
            /// are corrected-but-shadowed and this one is corrected-and-live.
            /// The three are landed anyway: a family half-fixed is the
            /// inconsistency, and `native-io`'s registrations are what a future
            /// reordering would remove.
            fn $put_bulk<const SESSION: bool>(
                ctx: &mut dyn NativeContext,
                args: &[Value],
            ) -> MethodCallResult {
                let mut this = obj_arg(args, 0)?;
                // The read-only question can run Java; the source array is
                // kept current across it with the receiver.
                let mut kept_src = [args.get(1).copied().unwrap_or(Value::Object(None))];
                if keep_across(ctx, &mut kept_src, |ctx| s2_buf_read_only(ctx, &mut this)) {
                    return Err(RuntimeError::ReadOnlyBufferException.into());
                }
                let mut src = obj_arg(&kept_src, 0)?;
                let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                let pos = s2_bb_pos(ctx, this);
                let src_len = i32::try_from(ctx.array_length(src)).unwrap_or(i32::MAX);
                s2_check_from_index_size(off, len, src_len)?;
                if pos
                    .checked_add(len)
                    .map_or(true, |e| e > s2_bb_limit(ctx, this))
                {
                    return Err(RuntimeError::BufferOverflowException.into());
                }
                // `putArray`: as `$get_bulk` (round 17 wave 8, lane nio3).
                if SESSION && len > 0 {
                    s2_bb_check_live_session(ctx, &mut this, &mut src)?;
                }
                let bs = s2_typed_view_byte_start(ctx, this);
                for i in 0..len {
                    let v = ctx.get_array_element(src, (off + i) as usize);
                    let dst_off = (pos + i)
                        .checked_mul($width)
                        .and_then(|b| bs.checked_add(b))
                        .unwrap_or(-1);
                    $write(ctx, this, dst_off, ($from_value)(&v));
                }
                ctx.set_field(this, BB_POS, Value::Int(pos + len));
                Ok(Some(Value::Object(Some(this))))
            }
        };
    }

    s2_typed_buffer_view_fns!(
        s2_ib_get,
        s2_ib_get_abs,
        s2_ib_put,
        s2_ib_put_abs,
        s2_ib_order,
        s2_ib_slice,
        s2_ib_slice2,
        s2_ib_dup,
        s2_ib_ro,
        s2_ib_compact,
        s2_ib_get_bulk,
        s2_ib_put_bulk,
        "java/nio/IntBuffer",
        4,
        s2_bb_read4,
        s2_bb_write4,
        |v: i32| Value::Int(v),
        |v: &Value| v.as_int().unwrap_or(0)
    );
    s2_typed_buffer_view_fns!(
        s2_lb_get,
        s2_lb_get_abs,
        s2_lb_put,
        s2_lb_put_abs,
        s2_lb_order,
        s2_lb_slice,
        s2_lb_slice2,
        s2_lb_dup,
        s2_lb_ro,
        s2_lb_compact,
        s2_lb_get_bulk,
        s2_lb_put_bulk,
        "java/nio/LongBuffer",
        8,
        s2_bb_read8,
        s2_bb_write8,
        |v: i64| Value::Long(v),
        |v: &Value| match v {
            Value::Long(l) => *l,
            other => other.as_int().unwrap_or(0) as i64,
        }
    );
    s2_typed_buffer_view_fns!(
        s2_sb_get,
        s2_sb_get_abs,
        s2_sb_put,
        s2_sb_put_abs,
        s2_sb_order,
        s2_sb_slice,
        s2_sb_slice2,
        s2_sb_dup,
        s2_sb_ro,
        s2_sb_compact,
        s2_sb_get_bulk,
        s2_sb_put_bulk,
        "java/nio/ShortBuffer",
        2,
        s2_bb_read2,
        s2_bb_write2,
        |v: i16| Value::Int(v as i32),
        |v: &Value| v.as_int().unwrap_or(0) as i16
    );
    s2_typed_buffer_view_fns!(
        s2_fb_get,
        s2_fb_get_abs,
        s2_fb_put,
        s2_fb_put_abs,
        s2_fb_order,
        s2_fb_slice,
        s2_fb_slice2,
        s2_fb_dup,
        s2_fb_ro,
        s2_fb_compact,
        s2_fb_get_bulk,
        s2_fb_put_bulk,
        "java/nio/FloatBuffer",
        4,
        s2_bb_read4,
        s2_bb_write4,
        |v: i32| Value::Float(f32::from_bits(v as u32)),
        |v: &Value| match v {
            Value::Float(f) => f.to_bits() as i32,
            other => other.as_int().unwrap_or(0),
        }
    );
    s2_typed_buffer_view_fns!(
        s2_db_get,
        s2_db_get_abs,
        s2_db_put,
        s2_db_put_abs,
        s2_db_order,
        s2_db_slice,
        s2_db_slice2,
        s2_db_dup,
        s2_db_ro,
        s2_db_compact,
        s2_db_get_bulk,
        s2_db_put_bulk,
        "java/nio/DoubleBuffer",
        8,
        s2_bb_read8,
        s2_bb_write8,
        |v: i64| Value::Double(f64::from_bits(v as u64)),
        |v: &Value| match v {
            Value::Double(d) => d.to_bits() as i64,
            other => other.as_int().unwrap_or(0) as i64,
        }
    );

    // JIT round 17 wave 8, lane nio3: the typed bulk natives take the same
    // `CRATONVM_COMPAT_BUFFER_BULK_SESSION` choice as the byte[] ones above.
    macro_rules! pick_session {
        ($f:ident) => {
            if bulk_session {
                $f::<true>
            } else {
                $f::<false>
            }
        };
    }
    // IntBuffer keeps its own long-standing get/put above (untouched); only
    // register the previously-missing abstract methods for it.
    r.register(ib, "order", "()Ljava/nio/ByteOrder;", s2_ib_order, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ib, "slice", "()Ljava/nio/IntBuffer;", s2_ib_slice, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ib, "slice", "(II)Ljava/nio/IntBuffer;", s2_ib_slice2, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ib, "duplicate", "()Ljava/nio/IntBuffer;", s2_ib_dup, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ib, "asReadOnlyBuffer", "()Ljava/nio/IntBuffer;", s2_ib_ro, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ib, "compact", "()Ljava/nio/IntBuffer;", s2_ib_compact, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        ib,
        "get",
        "([III)Ljava/nio/IntBuffer;",
        pick_session!(s2_ib_get_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ib,
        "put",
        "([III)Ljava/nio/IntBuffer;",
        pick_session!(s2_ib_put_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );

    let lb = "java/nio/LongBuffer";
    r.register(lb, "get", "()J", s2_lb_get, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lb, "get", "(I)J", s2_lb_get_abs, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lb, "put", "(J)Ljava/nio/LongBuffer;", s2_lb_put, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lb, "put", "(IJ)Ljava/nio/LongBuffer;", s2_lb_put_abs, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lb, "order", "()Ljava/nio/ByteOrder;", s2_lb_order, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lb, "slice", "()Ljava/nio/LongBuffer;", s2_lb_slice, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lb, "slice", "(II)Ljava/nio/LongBuffer;", s2_lb_slice2, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lb, "duplicate", "()Ljava/nio/LongBuffer;", s2_lb_dup, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lb, "asReadOnlyBuffer", "()Ljava/nio/LongBuffer;", s2_lb_ro, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(lb, "compact", "()Ljava/nio/LongBuffer;", s2_lb_compact, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        lb,
        "get",
        "([JII)Ljava/nio/LongBuffer;",
        pick_session!(s2_lb_get_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        lb,
        "put",
        "([JII)Ljava/nio/LongBuffer;",
        pick_session!(s2_lb_put_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );

    let sb = "java/nio/ShortBuffer";
    r.register(sb, "get", "()S", s2_sb_get, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sb, "get", "(I)S", s2_sb_get_abs, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sb, "put", "(S)Ljava/nio/ShortBuffer;", s2_sb_put, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sb, "put", "(IS)Ljava/nio/ShortBuffer;", s2_sb_put_abs, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sb, "order", "()Ljava/nio/ByteOrder;", s2_sb_order, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sb, "slice", "()Ljava/nio/ShortBuffer;", s2_sb_slice, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sb, "slice", "(II)Ljava/nio/ShortBuffer;", s2_sb_slice2, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sb, "duplicate", "()Ljava/nio/ShortBuffer;", s2_sb_dup, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sb, "asReadOnlyBuffer", "()Ljava/nio/ShortBuffer;", s2_sb_ro, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sb, "compact", "()Ljava/nio/ShortBuffer;", s2_sb_compact, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        sb,
        "get",
        "([SII)Ljava/nio/ShortBuffer;",
        pick_session!(s2_sb_get_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        sb,
        "put",
        "([SII)Ljava/nio/ShortBuffer;",
        pick_session!(s2_sb_put_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );

    let fb = "java/nio/FloatBuffer";
    r.register(fb, "get", "()F", s2_fb_get, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(fb, "get", "(I)F", s2_fb_get_abs, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(fb, "put", "(F)Ljava/nio/FloatBuffer;", s2_fb_put, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(fb, "put", "(IF)Ljava/nio/FloatBuffer;", s2_fb_put_abs, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(fb, "order", "()Ljava/nio/ByteOrder;", s2_fb_order, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(fb, "slice", "()Ljava/nio/FloatBuffer;", s2_fb_slice, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(fb, "slice", "(II)Ljava/nio/FloatBuffer;", s2_fb_slice2, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(fb, "duplicate", "()Ljava/nio/FloatBuffer;", s2_fb_dup, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(fb, "asReadOnlyBuffer", "()Ljava/nio/FloatBuffer;", s2_fb_ro, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(fb, "compact", "()Ljava/nio/FloatBuffer;", s2_fb_compact, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        fb,
        "get",
        "([FII)Ljava/nio/FloatBuffer;",
        pick_session!(s2_fb_get_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        fb,
        "put",
        "([FII)Ljava/nio/FloatBuffer;",
        pick_session!(s2_fb_put_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );

    let db = "java/nio/DoubleBuffer";
    r.register(db, "get", "()D", s2_db_get, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(db, "get", "(I)D", s2_db_get_abs, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(db, "put", "(D)Ljava/nio/DoubleBuffer;", s2_db_put, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(db, "put", "(ID)Ljava/nio/DoubleBuffer;", s2_db_put_abs, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(db, "order", "()Ljava/nio/ByteOrder;", s2_db_order, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(db, "slice", "()Ljava/nio/DoubleBuffer;", s2_db_slice, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(db, "slice", "(II)Ljava/nio/DoubleBuffer;", s2_db_slice2, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(db, "duplicate", "()Ljava/nio/DoubleBuffer;", s2_db_dup, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        db,
        "asReadOnlyBuffer",
        "()Ljava/nio/DoubleBuffer;",
        s2_db_ro, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(db, "compact", "()Ljava/nio/DoubleBuffer;", s2_db_compact, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        db,
        "get",
        "([DII)Ljava/nio/DoubleBuffer;",
        pick_session!(s2_db_get_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        db,
        "put",
        "([DII)Ljava/nio/DoubleBuffer;",
        pick_session!(s2_db_put_bulk), cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // equals / hashCode / compareTo / toString
    // equals / hashCode / compareTo — storage-aware (residual-doc item 2:
    // these were heap-array-only, so ANY direct buffer — including a
    // genuine real-JDK DirectByteBuffer receiver, since `ByteBuffer.equals`
    // resolves on the abstract class and is force-dispatched — compared as
    // empty/zero and mixed heap/direct comparisons always failed).
    fn s2_bb_equals<const SESSION: bool>(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        let mut other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        if this == other {
            return Ok(Some(Value::Int(1)));
        }
        // `ByteBuffer.equals` opens with `if (!(ob instanceof ByteBuffer)) return false`:
        // a `String` / `Integer` / `CharBuffer` argument has other slots where `position`
        // and `limit` would be, so it must not be read as a buffer (NIO3-17-3).
        let other_class = ctx.class_id_of_object(other);
        if ctx.class_assignable_to_name(other_class, "java/nio/ByteBuffer") != Some(true) {
            return Ok(Some(Value::Int(0)));
        }
        let pa = s2_bb_pos(ctx, this).max(0);
        let la = s2_bb_limit(ctx, this).max(pa);
        let pb = s2_bb_pos(ctx, other).max(0);
        let lb = s2_bb_limit(ctx, other).max(pb);
        let na = (la - pa) as usize;
        if na != (lb - pb) as usize {
            return Ok(Some(Value::Int(0)));
        }
        // `BufferMismatch.mismatch(this, .., that, .., n)`: `this.get` first, then
        // `that.get` (or `vectorizedMismatch(a.session(), b.session(), ..)`); none for n == 0.
        // `other` is read as a `Buffer` only when it is one (`equals(Object)`).
        if SESSION && na > 0 {
            s2_bb_check_live_session(ctx, &mut this, &mut other)?;
            let other_class = ctx.class_id_of_object(other);
            if ctx.class_assignable_to_name(other_class, "java/nio/Buffer") == Some(true) {
                s2_bb_check_live_session(ctx, &mut other, &mut this)?;
            }
        }
        let wa = s2_bb_read_window(ctx, this, pa, na);
        let wb = s2_bb_read_window(ctx, other, pb, na);
        match (wa, wb) {
            (Some(a), Some(b)) => Ok(Some(Value::Int((a == b) as i32))),
            _ => Ok(Some(Value::Int(0))),
        }
    }
    r.register(
        bb,
        "equals",
        "(Ljava/lang/Object;)Z",
        pick_session!(s2_bb_equals), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    fn s2_bb_hash_code<const SESSION: bool>(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this).max(0);
        let lim = s2_bb_limit(ctx, this).max(pos);
        let n = (lim - pos) as usize;
        // The JDK loop is `get(i)` per element: the session is checked iff n > 0.
        if SESSION && n > 0 {
            let mut same = this;
            s2_bb_check_live_session(ctx, &mut this, &mut same)?;
        }
        let mut h: i32 = 1;
        if let Some(w) = s2_bb_read_window(ctx, this, pos, n) {
            // Real-JDK `Buffer.hashCode` iterates BACKWARD
            // (`for (int i = limit() - 1; i >= position(); i--)`) — the
            // previous forward loop produced a different value than
            // HotSpot for every buffer with 2+ remaining bytes.
            for b in w.iter().rev() {
                h = h.wrapping_mul(31).wrapping_add(*b as i8 as i32);
            }
        }
        Ok(Some(Value::Int(h)))
    }
    r.register(bb, "hashCode", "()I", pick_session!(s2_bb_hash_code), cratonvm_native_api::NativeKind::SyntheticStub);
    fn s2_bb_compare_to<const SESSION: bool>(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        let mut this = obj_arg(args, 0)?;
        let mut other = obj_arg(args, 1)?;
        let pa = s2_bb_pos(ctx, this).max(0);
        let la = s2_bb_limit(ctx, this).max(pa);
        let pb = s2_bb_pos(ctx, other).max(0);
        let lb = s2_bb_limit(ctx, other).max(pb);
        let na = (la - pa) as usize;
        let nb = (lb - pb) as usize;
        let n = na.min(nb);
        // `BufferMismatch.mismatch(.., Math.min(thisRem, thatRem))`, as `equals`.
        if SESSION && n > 0 {
            s2_bb_check_live_session(ctx, &mut this, &mut other)?;
            s2_bb_check_live_session(ctx, &mut other, &mut this)?;
        }
        let wa = match s2_bb_read_window(ctx, this, pa, n) {
            Some(w) => w,
            None => return Ok(Some(Value::Int(0))),
        };
        let wb = match s2_bb_read_window(ctx, other, pb, n) {
            Some(w) => w,
            None => return Ok(Some(Value::Int(0))),
        };
        for i in 0..n {
            let va = wa[i] as i8 as i32;
            let vb = wb[i] as i8 as i32;
            if va != vb {
                return Ok(Some(Value::Int(va - vb)));
            }
        }
        Ok(Some(Value::Int((na as i32) - (nb as i32))))
    }
    r.register(
        bb,
        "compareTo",
        "(Ljava/nio/ByteBuffer;)I",
        pick_session!(s2_bb_compare_to), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // SHIM-AUDIT (native-builtins-shim-audit.md, row `java/nio/ByteBuffer`):
    // `ByteBuffer` is an ABSTRACT class and never declares its own instance —
    // every receiver is a `HeapByteBuffer`, `DirectByteBuffer`,
    // `MappedByteBuffer`, one of their read-only siblings, or a third-party
    // subclass. `Buffer.toString()` has real bytecode
    // (`getClass().getName() + "[pos=" ...`), and the native-override
    // hierarchy walk in `vm/src/runtime/interpreter/invoke.rs` looks for a
    // native on each ANCESTOR *before* it checks whether that ancestor has
    // bytecode — so this registration wins for every one of those receivers.
    // It used to hard-code the literal string `java.nio.HeapByteBuffer`, i.e.
    // it answered a question it could not know: a `DirectByteBuffer` — or a
    // real-JDK `MappedByteBuffer`, or a third-party subclass — rendered as a
    // heap buffer, which is exactly the shape a "which buffer kind is this?"
    // diagnostic reads.
    //
    // Three-step, fail-closed: never invent a concrete class name.
    //   1. If the receiver has a CONCRETE class (anything other than the
    //      abstract `java/nio/ByteBuffer` itself), that class IS the answer —
    //      this is the real-JDK receiver case and the only one `Buffer
    //      .toString()`'s `getClass().getName()` would ever see.
    //   2. CratonVM's own `ByteBuffer.allocate` / `allocateDirect` mint a
    //      synthetic stand-in whose class name is the abstract
    //      `java/nio/ByteBuffer`, so step 1 cannot name it. Derive the kind
    //      from the storage the buffer actually has, which is the same source
    //      `equals`/`hashCode`/`compareTo` read: a heap array means
    //      `HeapByteBuffer`, a native window means `DirectByteBuffer`.
    //   3. Storage-less (a bare stub): render the abstract class's own name.
    //      Unhelpful, but true — better than a concrete lie.
    r.register(bb, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        let lim = s2_bb_limit(ctx, this);
        let cap = s2_bb_cap(ctx, this);
        let cid = ctx.class_id_of_object(this);
        let own = ctx.class_name_of_id(cid).unwrap_or_default();
        let name = if !own.is_empty() && own != "java/nio/ByteBuffer" {
            own.replace('/', ".")
        } else {
            match s2_bb_storage(ctx, this) {
                Some(S2BbStorage::Heap { .. }) => "java.nio.HeapByteBuffer".to_string(),
                Some(S2BbStorage::Direct { .. }) => "java.nio.DirectByteBuffer".to_string(),
                None => "java.nio.ByteBuffer".to_string(),
            }
        };
        let s = ctx.create_string(&format!("{name}[pos={pos} lim={lim} cap={cap}]"));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // NEW-17: DirectByteBuffer deallocator.
    //
    // The Cleaner machinery dispatches `run()V` on the deallocator when the
    // associated DirectByteBuffer becomes phantom-reachable. Field 0 holds
    // the NativeMemoryTable alloc_id assigned at allocateDirect time; we
    // free it through NativeContext::free_native_memory and zero the field
    // so a double-free (if anything else gets the same address) is a no-op.
    r.register(DEALLOC_CLASS, "run", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let block_id = match ctx.get_field(this, DEALLOC_ID) {
            Value::Long(v) => v,
            _ => return Ok(None),
        };
        if block_id == 0 {
            return Ok(None); // already freed
        }
        ctx.free_native_memory(block_id);
        ctx.set_field(this, DEALLOC_ID, Value::Long(0));
        // Release the global root that kept the owning `Cleaner$Cleanable`
        // (and, through it, this deallocator) reachable until the action ran.
        // Leaving it installed would pin both for the life of the VM — a slow
        // leak of exactly the shape the Cleaner exists to prevent.
        if let Value::Long(root) = ctx.get_field(this, DEALLOC_ROOT) {
            if root != 0 {
                ctx.remove_global_root(root as usize);
                ctx.set_field(this, DEALLOC_ROOT, Value::Long(0));
            }
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
}

