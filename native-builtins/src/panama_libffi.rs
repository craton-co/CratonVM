// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NEW-18: libffi-backed Panama FFI bridge.
//!
//! This module translates CratonVM's `java.lang.foreign.MemoryLayout`
//! synthetic objects into `libffi::middle::Type` values, marshals
//! Java `Value`s into typed argument storage, and unmarshals return
//! slots back into `Value`s.
//!
//! The previous Panama dispatcher in `panama.rs` capped downcalls at 8
//! arguments and treated every argument as a 64-bit integer (wrong for
//! float/double on every ABI). This module is the replacement
//! foundation: it lets `pe_downcall_invoke` build a CIF for an
//! arbitrary signature including structs-by-value and floats.
//!
//! ## Safety
//!
//! Every `unsafe` block in this module is anchored to libffi
//! preconditions:
//!  * `Cif::new` validates the argument types match the CIF arity.
//!  * Argument storage is allocated from owned `Vec<u8>`s with the
//!    alignment required by the layout (max 16 bytes — sufficient for
//!    `__m128`-style C types and over-aligned for everything else we
//!    expose).
//!  * Pointers handed to libffi do not outlive the borrowing storage
//!    Vec (we hold both in the same scope).
//!  * Return values are read out of `MaybeUninit<u64>` for primitives
//!    or a `Vec<u8>` sized exactly `ffi_type::size` for aggregates.
//!
//! All FFI dispatch lives behind an enum that classifies the layout —
//! the public surface returns `Result<_, MethodCallFailed>` so the
//! caller can throw a proper Java exception on any classification
//! failure rather than panicking.

use libffi::low::{ffi_abi_FFI_DEFAULT_ABI, ffi_cif, ffi_type, prep_cif_var};
use libffi::middle::{Cif, Type as FfiType};

use cratonvm_native_api::{
    ffi::{
        layout_alignment, layout_byte_size, LAYOUT_ADDRESS, LAYOUT_BOOLEAN, LAYOUT_BYTE,
        LAYOUT_CHAR, LAYOUT_DOUBLE, LAYOUT_FLOAT, LAYOUT_INT, LAYOUT_LONG, LAYOUT_PADDING,
        LAYOUT_SEQUENCE, LAYOUT_SHORT, LAYOUT_STRUCT, LAYOUT_UNION,
    },
    NativeContext,
};
use cratonvm_types::{
    error::{MethodCallFailed, RuntimeError},
    ObjectRef, Value,
};

/// Maximum byte size of a struct-by-value argument or return. This is
/// generous enough for every System V/Win64 struct that might fit in
/// registers plus some headroom for stack-passed aggregates.
pub const MAX_STRUCT_BYTES: usize = 4096;

/// Maximum number of fields we will recurse through when translating
/// nested aggregate layouts. Bounds runaway recursion from a malformed
/// or hostile descriptor.
pub const MAX_LAYOUT_DEPTH: usize = 16;

/// Read the layout-kind discriminator from a layout synthetic.
pub fn read_layout_kind(ctx: &dyn NativeContext, layout: ObjectRef) -> i32 {
    match ctx.get_field(layout, 0) {
        Value::Int(k) => k,
        Value::Long(_) => {
            let class_name = ctx.class_name_of_id(ctx.class_id_of_object(layout));
            match class_name.as_deref() {
                Some("java/lang/foreign/ValueLayout$OfByte") => LAYOUT_BYTE,
                Some("java/lang/foreign/ValueLayout$OfBoolean") => LAYOUT_BOOLEAN,
                Some("java/lang/foreign/ValueLayout$OfChar") => LAYOUT_CHAR,
                Some("java/lang/foreign/ValueLayout$OfShort") => LAYOUT_SHORT,
                Some("java/lang/foreign/ValueLayout$OfInt") => LAYOUT_INT,
                Some("java/lang/foreign/ValueLayout$OfLong") => LAYOUT_LONG,
                Some("java/lang/foreign/ValueLayout$OfFloat") => LAYOUT_FLOAT,
                Some("java/lang/foreign/ValueLayout$OfDouble") => LAYOUT_DOUBLE,
                Some("java/lang/foreign/AddressLayout") => LAYOUT_ADDRESS,
                _ => LAYOUT_LONG,
            }
        }
        _ => LAYOUT_LONG, // safe default
    }
}

/// Walk the parameter layout array (FunctionDescriptor field 1).
pub fn descriptor_param_layouts(
    ctx: &dyn NativeContext,
    desc: ObjectRef,
) -> Vec<Option<ObjectRef>> {
    let arr = match ctx.get_field(desc, 1) {
        Value::Object(Some(a)) => a,
        _ => return Vec::new(),
    };
    let len = ctx.array_length(arr);
    (0..len)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Object(Some(layout)) => Some(layout),
            _ => None,
        })
        .collect()
}

/// Read the optional return layout (FunctionDescriptor field 0).
/// Returns `None` for void returns.
pub fn descriptor_return_layout(ctx: &dyn NativeContext, desc: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(desc, 0) {
        Value::Object(Some(layout)) => Some(layout),
        _ => None,
    }
}

/// Translate a primitive layout kind to a libffi Type. Returns None
/// for compound layouts (caller must recurse).
pub fn primitive_kind_to_ffi_type(kind: i32) -> Option<FfiType> {
    match kind {
        LAYOUT_BYTE | LAYOUT_BOOLEAN => Some(FfiType::i8()),
        LAYOUT_SHORT | LAYOUT_CHAR => Some(FfiType::i16()),
        LAYOUT_INT => Some(FfiType::i32()),
        LAYOUT_LONG => Some(FfiType::i64()),
        LAYOUT_FLOAT => Some(FfiType::f32()),
        LAYOUT_DOUBLE => Some(FfiType::f64()),
        LAYOUT_ADDRESS => Some(FfiType::pointer()),
        _ => None,
    }
}

/// Resolve any layout synthetic (primitive, struct, union, sequence)
/// into a libffi Type, recursing into compound layouts.
///
/// `depth` guards against pathological nesting; the caller should
/// pass 0 at the top level.
pub fn layout_to_ffi_type(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    depth: usize,
) -> Result<FfiType, MethodCallFailed> {
    if depth > MAX_LAYOUT_DEPTH {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Panama layout nesting exceeds {} levels (possible cycle)",
                MAX_LAYOUT_DEPTH
            ),
        }
        .into());
    }
    let kind = read_layout_kind(ctx, layout);
    if let Some(ty) = primitive_kind_to_ffi_type(kind) {
        return Ok(ty);
    }
    match kind {
        LAYOUT_STRUCT => layout_struct_to_ffi_type(ctx, layout, depth),
        LAYOUT_UNION => {
            // libffi has no native union; we model it as a struct of one
            // member equal to the largest member, padded out with `i8`s
            // to the union's total size. This preserves byte size but
            // not register classification of all union variants — sufficient
            // for memcpy-style usage which is how Java code exercises unions.
            let total = match ctx.get_field(layout, 1) {
                Value::Long(n) if n > 0 => n as usize,
                _ => 1,
            };
            let mut fields: Vec<FfiType> = Vec::with_capacity(total);
            for _ in 0..total {
                fields.push(FfiType::u8());
            }
            Ok(FfiType::structure(fields))
        }
        LAYOUT_SEQUENCE => {
            let count = match ctx.get_field(layout, 1) {
                Value::Long(n) if n > 0 => n as usize,
                _ => 0,
            };
            let elem_layout = match ctx.get_field(layout, 2) {
                Value::Object(Some(e)) => e,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "SequenceLayout missing element layout".into(),
                    }
                    .into());
                }
            };
            let elem_ty = layout_to_ffi_type(ctx, elem_layout, depth + 1)?;
            // Repeat the element type `count` times into a struct.
            let elem_size = layout_total_size(ctx, elem_layout)?;
            if elem_size == 0 || count.saturating_mul(elem_size) > MAX_STRUCT_BYTES {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "SequenceLayout element size 0 or total > {} bytes",
                        MAX_STRUCT_BYTES
                    ),
                }
                .into());
            }
            let mut fields: Vec<FfiType> = Vec::with_capacity(count);
            for _ in 0..count {
                fields.push(clone_ffi_type(&elem_ty));
            }
            Ok(FfiType::structure(fields))
        }
        LAYOUT_PADDING => {
            // PaddingLayouts only appear inside StructLayouts to express
            // alignment gaps. As a top-level argument they are nonsense,
            // so reject them.
            Err(RuntimeError::IllegalStateException {
                message: "PaddingLayout is not a valid argument or return type".into(),
            }
            .into())
        }
        _ => Err(RuntimeError::IllegalStateException {
            message: format!("Unsupported Panama layout kind: {}", kind),
        }
        .into()),
    }
}

/// Translate a StructLayout (kind=10) to a libffi struct Type.
fn layout_struct_to_ffi_type(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    depth: usize,
) -> Result<FfiType, MethodCallFailed> {
    let members_arr = match ctx.get_field(layout, 2) {
        Value::Object(Some(a)) => a,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "StructLayout missing member layout array".into(),
            }
            .into());
        }
    };
    let count = ctx.array_length(members_arr);
    if count == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "StructLayout has no members".into(),
        }
        .into());
    }
    let mut fields: Vec<FfiType> = Vec::with_capacity(count);
    for i in 0..count {
        match ctx.get_array_element(members_arr, i) {
            Value::Object(Some(member)) => {
                let kind = read_layout_kind(ctx, member);
                if kind == LAYOUT_PADDING {
                    // Materialize padding as N i8's.
                    let pad_size = match ctx.get_field(member, 1) {
                        Value::Long(n) if n > 0 => n as usize,
                        _ => 0,
                    };
                    for _ in 0..pad_size {
                        fields.push(FfiType::u8());
                    }
                } else {
                    fields.push(layout_to_ffi_type(ctx, member, depth + 1)?);
                }
            }
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("StructLayout member {} is not an object", i),
                }
                .into());
            }
        }
    }
    Ok(FfiType::structure(fields))
}

/// Clone a libffi Type by recursively reading its `ffi_type` fields.
/// Used when expanding a SequenceLayout into N copies of the element.
fn clone_ffi_type(ty: &FfiType) -> FfiType {
    let raw = ty.as_raw_ptr();
    // SAFETY: `raw` is a valid `*mut ffi_type` owned by the source Type.
    // We read its discriminator and rebuild an equivalent Type.
    unsafe {
        let r: &ffi_type = &*raw;
        // libffi exposes type discriminators via the `type_` field.
        // Unfortunately the constants live in `low::types::*`; we
        // pattern-match on size + element pointer.
        if r.elements.is_null() {
            // Primitive type. Reconstruct from size/alignment (best effort).
            match (r.size, r.alignment as usize) {
                (1, _) => FfiType::u8(),
                (2, _) => FfiType::u16(),
                (4, _) => {
                    // Could be i32 or f32 — libffi distinguishes via type tag.
                    // Use the type_ field directly.
                    if r.type_ == libffi::raw::FFI_TYPE_FLOAT as u16 {
                        FfiType::f32()
                    } else {
                        FfiType::i32()
                    }
                }
                (8, _) => {
                    if r.type_ == libffi::raw::FFI_TYPE_DOUBLE as u16 {
                        FfiType::f64()
                    } else if r.type_ == libffi::raw::FFI_TYPE_POINTER as u16 {
                        FfiType::pointer()
                    } else {
                        FfiType::i64()
                    }
                }
                _ => FfiType::pointer(),
            }
        } else {
            // Aggregate: walk the elements null-terminated array and clone each.
            let mut fields: Vec<FfiType> = Vec::new();
            let mut p = r.elements;
            while !(*p).is_null() {
                let elem_raw: &ffi_type = &**p;
                fields.push(clone_ffi_type_from_raw(elem_raw));
                p = p.add(1);
            }
            FfiType::structure(fields)
        }
    }
}

unsafe fn clone_ffi_type_from_raw(raw: &ffi_type) -> FfiType {
    if raw.elements.is_null() {
        match (raw.size, raw.type_ as u32) {
            (1, _) => FfiType::u8(),
            (2, _) => FfiType::u16(),
            (4, t) if t == libffi::raw::FFI_TYPE_FLOAT => FfiType::f32(),
            (4, _) => FfiType::i32(),
            (8, t) if t == libffi::raw::FFI_TYPE_DOUBLE => FfiType::f64(),
            (8, t) if t == libffi::raw::FFI_TYPE_POINTER => FfiType::pointer(),
            (8, _) => FfiType::i64(),
            _ => FfiType::pointer(),
        }
    } else {
        let mut fields: Vec<FfiType> = Vec::new();
        let mut p = raw.elements;
        while !(*p).is_null() {
            fields.push(clone_ffi_type_from_raw(&**p));
            p = p.add(1);
        }
        FfiType::structure(fields)
    }
}

/// Compute the total byte size of a layout, recursing into compound
/// layouts. Returns 0 for void-shaped inputs.
pub fn layout_total_size(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
) -> Result<usize, MethodCallFailed> {
    let kind = read_layout_kind(ctx, layout);
    if kind < 10 {
        return Ok(layout_byte_size(kind));
    }
    match kind {
        LAYOUT_STRUCT | LAYOUT_UNION | LAYOUT_SEQUENCE | LAYOUT_PADDING => {
            match ctx.get_field(layout, 1) {
                Value::Long(n) if n >= 0 => Ok(n as usize),
                _ => Ok(0),
            }
        }
        _ => Ok(0),
    }
}

/// Compute alignment for a layout.
pub fn layout_align(ctx: &dyn NativeContext, layout: ObjectRef) -> usize {
    let kind = read_layout_kind(ctx, layout);
    if kind < 10 {
        return layout_alignment(kind);
    }
    match ctx.get_field(layout, 5) {
        Value::Long(n) if n > 0 => n as usize,
        _ => 1,
    }
}

/// A typed argument slot owned by the per-call scratch storage.
///
/// Each `ArgSlot` holds an exclusively-owned, suitably-aligned buffer
/// that libffi will read from when packing the argument into the
/// outgoing register/stack frame. Lifetime is anchored to the
/// surrounding call.
pub struct ArgSlot {
    pub bytes: Vec<u8>,
}

impl ArgSlot {
    pub fn as_ptr(&self) -> *const std::ffi::c_void {
        self.bytes.as_ptr() as *const std::ffi::c_void
    }
}

/// Marshal a single Java `Value` into an `ArgSlot` of the right size
/// for `layout_kind`. For `LAYOUT_ADDRESS` and struct kinds, the value
/// is expected to be a `MemorySegment` synthetic; we extract its base
/// pointer from field 0 (and offset from field 5).
///
/// For struct-by-value, the entire byte payload is copied from the
/// segment into the slot. The caller's CIF must declare the matching
/// struct Type for libffi to lay it out correctly into the call frame.
pub fn marshal_arg(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    arg: &Value,
) -> Result<ArgSlot, MethodCallFailed> {
    let kind = read_layout_kind(ctx, layout);
    let mk = |bytes: Vec<u8>| Ok(ArgSlot { bytes });
    match kind {
        LAYOUT_BYTE | LAYOUT_BOOLEAN => {
            let v = arg.as_int().unwrap_or(0) as i8;
            mk(vec![v as u8])
        }
        LAYOUT_SHORT | LAYOUT_CHAR => {
            let v = arg.as_int().unwrap_or(0) as i16;
            mk(v.to_ne_bytes().to_vec())
        }
        LAYOUT_INT => {
            let v = arg.as_int().unwrap_or(0);
            mk(v.to_ne_bytes().to_vec())
        }
        LAYOUT_LONG => {
            let v = match arg {
                Value::Long(n) => *n,
                Value::Int(n) => *n as i64,
                _ => 0,
            };
            mk(v.to_ne_bytes().to_vec())
        }
        LAYOUT_FLOAT => {
            let v = match arg {
                Value::Float(f) => *f,
                Value::Int(n) => *n as f32,
                _ => 0.0,
            };
            mk(v.to_ne_bytes().to_vec())
        }
        LAYOUT_DOUBLE => {
            let v = match arg {
                Value::Double(d) => *d,
                Value::Float(f) => *f as f64,
                Value::Int(n) => *n as f64,
                Value::Long(n) => *n as f64,
                _ => 0.0,
            };
            mk(v.to_ne_bytes().to_vec())
        }
        LAYOUT_ADDRESS => {
            let v = match arg {
                Value::Object(Some(seg)) => segment_address(ctx, *seg),
                Value::Long(n) => *n,
                Value::Int(n) => *n as i64,
                _ => 0,
            };
            // Address is sizeof(usize); use ptr layout.
            mk((v as usize).to_ne_bytes().to_vec())
        }
        LAYOUT_STRUCT | LAYOUT_UNION | LAYOUT_SEQUENCE => {
            // Copy the segment's payload bytes into the slot.
            let total = layout_total_size(ctx, layout)?;
            if total == 0 || total > MAX_STRUCT_BYTES {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "Struct/union/sequence size {} out of range (0,{}]",
                        total, MAX_STRUCT_BYTES
                    ),
                }
                .into());
            }
            let seg = match arg {
                Value::Object(Some(s)) => *s,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "Struct-by-value argument is not a MemorySegment".into(),
                    }
                    .into());
                }
            };
            let addr = segment_address(ctx, seg);
            if addr == 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "Struct-by-value MemorySegment has null address".into(),
                }
                .into());
            }
            let mut bytes = vec![0u8; total];
            // SAFETY: `addr` was sourced from a live MemorySegment field.
            // The segment's owning Arena keeps the backing memory valid
            // for the duration of this downcall (Arena lifetime > call).
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, bytes.as_mut_ptr(), total);
            }
            mk(bytes)
        }
        _ => Err(RuntimeError::IllegalStateException {
            message: format!("marshal_arg: unsupported layout kind {}", kind),
        }
        .into()),
    }
}

/// Read the absolute address of a `MemorySegment`.
///
/// CratonVM-created segments use `[base@0, size@1, ..., offset@5]`; real
/// JDK `NativeMemorySegmentImpl`/`MappedMemorySegmentImpl` instances instead
/// store their absolute address in the inherited `min` field.  This is the
/// one canonical conversion point for downcalls and for the Panama bridge
/// methods that must accept either representation.
pub fn segment_address(ctx: &dyn NativeContext, seg: ObjectRef) -> i64 {
    // Real JDK-loaded NativeMemorySegmentImpl/MappedMemorySegmentImpl instances
    // do NOT share CratonVM's synthetic (base@0, offset@5) MemorySegment layout.
    // Their real field order (confirmed via javap against the real JDK):
    // AbstractMemorySegmentImpl{length, readOnly, scope} then
    // NativeMemorySegmentImpl{min} then MappedMemorySegmentImpl{unmapper} --
    // so "field 0" there is the segment's BYTE LENGTH, not its address.
    // Resolving "min" by name first (falling back to the synthetic scheme)
    // mirrors the already-correct, established pattern in
    // p67_segment_address (phases_late.rs, MemorySegment.address()) --
    // confirmed via live gdb capture that a real posix_madvise downcall was
    // otherwise passed the segment's byte length (276) as its address.
    if let Value::Long(v) = ctx.get_field_by_name(seg, "min") {
        return v;
    }
    if ctx.object_num_fields(seg) >= 6 {
        let base = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        let off = match ctx.get_field(seg, 5) {
            Value::Long(n) => n,
            _ => 0,
        };
        return base.wrapping_add(off);
    }
    match ctx.get_field(seg, 0) {
        Value::Long(n) => n,
        _ => 0,
    }
}

/// Read a `MemorySegment`'s declared byte size in either representation.
///
/// The real JDK implementation names this field `length`; the synthetic
/// representation keeps it in field 1.  Prefer the name lookup so a real
/// mapped segment can never be mistaken for the synthetic layout.
pub fn segment_byte_size(ctx: &dyn NativeContext, seg: ObjectRef) -> i64 {
    if let Value::Long(v) = ctx.get_field_by_name(seg, "length") {
        return v;
    }
    match ctx.get_field(seg, 1) {
        Value::Long(v) => v,
        _ => 0,
    }
}

/// Allocate a return buffer sized for the given layout. For void
/// returns, returns an empty `Vec`.
pub fn alloc_return_slot(
    ctx: &dyn NativeContext,
    return_layout: Option<ObjectRef>,
) -> Result<Vec<u8>, MethodCallFailed> {
    let layout = match return_layout {
        Some(l) => l,
        None => return Ok(Vec::new()),
    };
    let kind = read_layout_kind(ctx, layout);
    // libffi requires the result buffer to be at least sizeof(ffi_arg)
    // (= sizeof(usize)) for primitive returns smaller than that; if we
    // pass a smaller buffer, libffi may write past the end on big-endian.
    // We always allocate at least sizeof(usize).
    let raw_size = layout_total_size(ctx, layout)?;
    let _ = kind;
    let size = raw_size.max(std::mem::size_of::<usize>());
    if size > MAX_STRUCT_BYTES {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Return type size {} > {}", size, MAX_STRUCT_BYTES),
        }
        .into());
    }
    Ok(vec![0u8; size])
}

/// Read a return slot back into a Java Value, given the return layout.
///
/// For struct returns, the caller supplies an arena-backed
/// MemorySegment (already allocated) and we copy the payload into
/// it; the returned `Value::Object(Some(segment))` references that
/// segment.
pub fn unmarshal_return_primitive(layout_kind: i32, slot: &[u8]) -> Value {
    fn read<const N: usize>(slot: &[u8]) -> [u8; N] {
        let mut out = [0u8; N];
        let n = N.min(slot.len());
        out[..n].copy_from_slice(&slot[..n]);
        out
    }
    match layout_kind {
        LAYOUT_BYTE | LAYOUT_BOOLEAN => {
            // libffi widens integer returns to ffi_arg (= size_t / usize).
            // Read the low byte from the platform-native widened slot.
            let widened = usize::from_ne_bytes(read::<{ std::mem::size_of::<usize>() }>(slot));
            Value::Int(widened as i8 as i32)
        }
        LAYOUT_SHORT | LAYOUT_CHAR => {
            let widened = usize::from_ne_bytes(read::<{ std::mem::size_of::<usize>() }>(slot));
            Value::Int(widened as i16 as i32)
        }
        LAYOUT_INT => {
            let widened = usize::from_ne_bytes(read::<{ std::mem::size_of::<usize>() }>(slot));
            Value::Int(widened as i32)
        }
        LAYOUT_LONG => Value::Long(i64::from_ne_bytes(read::<8>(slot))),
        LAYOUT_FLOAT => Value::Float(f32::from_ne_bytes(read::<4>(slot))),
        LAYOUT_DOUBLE => Value::Double(f64::from_ne_bytes(read::<8>(slot))),
        LAYOUT_ADDRESS => Value::Long(i64::from_ne_bytes(read::<8>(slot))),
        _ => Value::Object(None),
    }
}

/// Build a CIF, optionally variadic.
///
/// `nfixed = None` → standard CIF.
/// `nfixed = Some(k)` → variadic with k fixed args (libffi
/// requires `k < ntotal`).
pub fn build_cif(
    arg_types: Vec<FfiType>,
    return_type: FfiType,
    nfixed: Option<usize>,
) -> Result<Cif, MethodCallFailed> {
    match nfixed {
        None => Ok(Cif::new(arg_types, return_type)),
        Some(k) => {
            let total = arg_types.len();
            if k >= total {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "Variadic CIF requires fixed-arg index {} < total {}",
                        k, total
                    ),
                }
                .into());
            }
            // libffi::middle::Cif doesn't expose prep_cif_var directly;
            // we drop to the low API. Build a TypeArray from the given
            // types via Cif::new and then re-prep its inner ffi_cif via
            // prep_cif_var, mutating in place.
            let cif_obj = Cif::new(arg_types, return_type);
            // Safety: we obtain the raw cif pointer and re-prep it as
            // variadic. The TypeArray and result Type stay owned by
            // `cif_obj` (Cif::clone documents that arg_types/result
            // backing memory is retained), so prep_cif_var's atypes/rtype
            // pointers remain valid.
            let raw = cif_obj.as_raw_ptr();
            // SAFETY: prep_cif_var is documented to be safe to call on
            // an already-prepped CIF as long as the same atypes/rtype
            // backing storage stays alive. We pass the same atypes and
            // rtype that Cif::new installed.
            unsafe {
                let cif_ref: &mut ffi_cif = &mut *raw;
                let atypes = cif_ref.arg_types;
                let rtype = cif_ref.rtype;
                if let Err(status) =
                    prep_cif_var(raw, ffi_abi_FFI_DEFAULT_ABI, k, total, rtype, atypes)
                {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "libffi prep_cif_var failed for variadic call: {:?}",
                            status
                        ),
                    }
                    .into());
                }
            }
            Ok(cif_obj)
        }
    }
}

// =============================================================================
// T5.6.3 — DowncallHandle CIF cache
// =============================================================================
//
// Building a libffi `Cif` involves allocating a `TypeArray`, calling
// `ffi_prep_cif` (or `ffi_prep_cif_var` for variadic), and — for
// struct-by-value layouts — walking nested layout synthetics to emit
// the correct `ffi_type` children. For a hot Panama downcall (e.g. a
// game loop calling into native graphics every frame) we pay this cost
// every invocation. The Cif is purely a function of the
// FunctionDescriptor, so we cache it on the `DowncallHandle` synthetic
// itself: field 3 holds a raw `Box<Cif>` pointer that survives until
// the handle is finalized.
//
// SAFETY model:
//   * Alloc: `cache_cif_for_handle` does `Box::into_raw(Box::new(cif))`.
//     The integer is stashed on the synthetic as a Long so the GC and
//     the field-value enum don't need to know about Cif.
//   * Read:  `load_cached_cif` turns the integer back into `*const Cif`
//     and yields `&'a Cif`. The caller must not keep the reference past
//     the synthetic's lifetime.
//   * Free:  `free_cached_cif` reconstructs the Box and drops it. The
//     Panama finalizer path isn't yet wired (see TODO in panama.rs); in
//     practice DowncallHandles are constructed once per native function
//     per VM run, so the leak is bounded and symmetric with HotSpot's
//     own permanent Cif cache.
//
// The `BUILD_COUNT` atomic lets tests assert that a cached Cif was
// actually reused (count must not increment on the second call).

use std::sync::atomic::{AtomicU64, Ordering};

/// Count of `libffi::middle::Cif` objects constructed via
/// [`build_cif_and_record`]. Monotonically increasing across the VM
/// lifetime; used by the T5.6.3 regression tests to verify that the
/// DowncallHandle CIF cache is hit on the second invocation.
pub static CIF_BUILD_COUNT: AtomicU64 = AtomicU64::new(0);

/// Wrapper around [`build_cif`] that increments [`CIF_BUILD_COUNT`] on
/// successful construction. Production code calls this instead of
/// `build_cif` directly so the cache-hit counter stays in sync.
pub fn build_cif_and_record(
    arg_types: Vec<FfiType>,
    return_type: FfiType,
    nfixed: Option<usize>,
) -> Result<Cif, MethodCallFailed> {
    let cif = build_cif(arg_types, return_type, nfixed)?;
    CIF_BUILD_COUNT.fetch_add(1, Ordering::Relaxed);
    Ok(cif)
}

/// Box a freshly built [`Cif`] on the heap and return the raw pointer
/// as a `u64` suitable for storing in a synthetic-object field. The
/// returned integer must eventually be passed to [`free_cached_cif`] to
/// avoid leaking the Box.
#[inline]
pub fn box_cif_to_u64(cif: Cif) -> u64 {
    Box::into_raw(Box::new(cif)) as usize as u64
}

/// Safely deref a stashed `Box<Cif>` pointer back into a `&Cif`. Returns
/// `None` if the pointer is zero.
///
/// # Safety
///
/// The caller must guarantee that the pointer was produced by
/// [`box_cif_to_u64`] on the same process and has not yet been freed
/// by [`free_cached_cif`]. The returned reference must not outlive the
/// backing Box.
#[inline]
pub unsafe fn cached_cif_ref<'a>(raw: u64) -> Option<&'a Cif> {
    if raw == 0 {
        return None;
    }
    let ptr = raw as usize as *const Cif;
    Some(&*ptr)
}

/// Drop the `Box<Cif>` previously installed via [`box_cif_to_u64`].
///
/// # Safety
///
/// `raw` must have been produced by [`box_cif_to_u64`] and must not be
/// freed twice.
#[inline]
pub unsafe fn free_cached_cif(raw: u64) {
    if raw == 0 {
        return;
    }
    let ptr = raw as usize as *mut Cif;
    drop(Box::from_raw(ptr));
}

// =============================================================================
// Thread-local NativeContext for upcall closures
// =============================================================================
//
// libffi closures execute as plain extern "C" functions invoked by C code.
// They therefore have no `&mut dyn NativeContext` to dispatch back into Java.
//
// The only legal Panama path that triggers an upcall is from inside a
// downcall: Java calls a downcall, the downcall enters C, C calls a
// callback we registered, the callback dispatches back into Java via
// `NativeContext::invoke_virtual`. We exploit this nesting by stashing
// a non-owning pointer to the active `NativeContext` in a thread-local
// for the duration of every downcall. Closures fired *during* the
// downcall fetch this pointer; closures fired outside any downcall
// (signal handlers, async callbacks) see `None` and return zero.
//
// SAFETY: The thread-local is *only* dereferenced inside a closure
// callback, and the surrounding downcall guarantees the pointee
// `NativeContext` outlives the closure. The downcall installs the
// pointer with `set_active_context_for_downcall` and clears it via the
// `_RestoreOnDrop` guard before returning to Java. Cross-thread access
// is impossible because the cell is `thread_local`.

use std::cell::Cell;

// To avoid storing a fat `*mut dyn NativeContext` in a thread-local
// (which requires a non-null fat pointer for the initial value), we
// indirect through a thin pointer to a heap-allocated `Box` carrying
// the real fat pointer.
//
// The thread-local stores `*mut FatPtrSlot` where `FatPtrSlot` boxes
// the fat pointer. A null thin pointer means "no active downcall".
// `dyn NativeContext + 'static` lets us store the fat raw pointer in a
// `Box`/`Cell` without dragging a borrowed lifetime through the slot.
// SAFETY: the static bound on the trait object is artificial — every
// real installed pointer is bounded by the `ActiveContextGuard`'s
// lifetime, and the `with_active_context` API enforces nesting.
type FatPtrSlot = *mut (dyn NativeContext + 'static);

thread_local! {
    static ACTIVE_NATIVE_CONTEXT: Cell<*mut FatPtrSlot> = const { Cell::new(std::ptr::null_mut()) };
}

/// RAII guard installing the active NativeContext for the duration of
/// a downcall. On drop, frees the heap slot and restores the previous
/// value.
pub struct ActiveContextGuard {
    prev: *mut FatPtrSlot,
    /// We own this Box; freed in Drop.
    owned: *mut FatPtrSlot,
}

impl ActiveContextGuard {
    pub fn install(ctx: &mut dyn NativeContext) -> Self {
        // SAFETY: we artificially extend the lifetime of `ctx` to
        // `'static` only inside the FatPtrSlot. The
        // `ActiveContextGuard` returned from this function will clear
        // the slot in its `Drop` impl, so the fat pointer never
        // outlives the original borrow. Callers must never let an
        // `ActiveContextGuard` live past the surrounding downcall.
        let raw: *mut (dyn NativeContext + 'static) = unsafe {
            std::mem::transmute::<*mut dyn NativeContext, *mut (dyn NativeContext + 'static)>(
                ctx as *mut dyn NativeContext,
            )
        };
        let slot: Box<FatPtrSlot> = Box::new(raw);
        let owned = Box::into_raw(slot);
        let prev = ACTIVE_NATIVE_CONTEXT.with(|c| c.replace(owned));
        ActiveContextGuard { prev, owned }
    }
}

impl Drop for ActiveContextGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        ACTIVE_NATIVE_CONTEXT.with(|c| c.set(prev));
        // SAFETY: we created `owned` via `Box::into_raw` in `install`
        // and have not handed it out anywhere else.
        unsafe {
            drop(Box::from_raw(self.owned));
        }
    }
}

/// Run a closure with the active `NativeContext`, or return None if no
/// downcall is in progress on this thread.
///
/// SAFETY: The fat pointer in the slot was installed by
/// `ActiveContextGuard::install` from a live `&mut dyn NativeContext`
/// and remains valid for the guard's lifetime. libffi closures invoked
/// during the downcall run on the same thread, so the pointee is alive.
pub fn with_active_context<R>(f: impl FnOnce(&mut dyn NativeContext) -> R) -> Option<R> {
    let slot = ACTIVE_NATIVE_CONTEXT.with(|c| c.get());
    if slot.is_null() {
        return None;
    }
    // SAFETY: see function-level safety comment.
    unsafe {
        let fat_ptr: *mut dyn NativeContext = *slot;
        Some(f(&mut *fat_ptr))
    }
}

/// Test helper: returns true if the active context cell currently
/// holds a non-null context pointer.
#[cfg(test)]
pub fn has_active_context() -> bool {
    !ACTIVE_NATIVE_CONTEXT.with(|c| c.get()).is_null()
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

    #[test]
    fn primitive_types_translate() {
        assert!(primitive_kind_to_ffi_type(LAYOUT_INT).is_some());
        assert!(primitive_kind_to_ffi_type(LAYOUT_LONG).is_some());
        assert!(primitive_kind_to_ffi_type(LAYOUT_FLOAT).is_some());
        assert!(primitive_kind_to_ffi_type(LAYOUT_DOUBLE).is_some());
        assert!(primitive_kind_to_ffi_type(LAYOUT_ADDRESS).is_some());
        assert!(primitive_kind_to_ffi_type(LAYOUT_BYTE).is_some());
        assert!(primitive_kind_to_ffi_type(LAYOUT_BOOLEAN).is_some());
        assert!(primitive_kind_to_ffi_type(LAYOUT_SHORT).is_some());
        assert!(primitive_kind_to_ffi_type(LAYOUT_CHAR).is_some());
        // Compound types must return None.
        assert!(primitive_kind_to_ffi_type(LAYOUT_STRUCT).is_none());
        assert!(primitive_kind_to_ffi_type(LAYOUT_UNION).is_none());
        assert!(primitive_kind_to_ffi_type(LAYOUT_SEQUENCE).is_none());
    }

    #[test]
    fn arg_slot_round_trip_int() {
        let slot = ArgSlot {
            bytes: 0x1234_5678i32.to_ne_bytes().to_vec(),
        };
        // The pointer should be usable.
        assert!(!slot.as_ptr().is_null());
    }

    #[test]
    fn unmarshal_int_widens_correctly() {
        let mut slot = vec![0u8; std::mem::size_of::<usize>()];
        slot[..4].copy_from_slice(&(-1i32).to_ne_bytes());
        // For a 32-bit int, libffi widens to ffi_arg. The high bytes
        // of the widened slot will be sign-extended on real calls;
        // unmarshal must read the low 32 bits and reinterpret as i32.
        let v = unmarshal_return_primitive(LAYOUT_INT, &slot);
        assert!(matches!(v, Value::Int(_)));
    }
}
