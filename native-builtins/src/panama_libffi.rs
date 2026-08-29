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
        align_up, layout_alignment, layout_byte_size, LAYOUT_ADDRESS, LAYOUT_BOOLEAN, LAYOUT_BYTE,
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

/// Bit 62 — the tag `unsafe_natives_ext.rs`'s `unsafe_arena::ARENA_TAG` ORs
/// into every handle its arena store hands out.
///
/// **Duplicated deliberately.** The authoritative constant is `pub(super)`
/// inside a private module of `unsafe_natives_ext.rs` and is not importable
/// from here; see the residual filed against this lane for exporting it. Its
/// contract is what matters and it is documented as permanent: the tag "is part
/// of the address value end-to-end — it is NEVER stripped", so a tagged handle
/// is a Rust-side arena key, not a machine address, and dereferencing one
/// always faults.
const ARENA_HANDLE_TAG: i64 = 1 << 62;

/// Lowest address any platform CratonVM targets will map.
///
/// Windows reserves the low 64 KiB of every process's address space, and Linux
/// enforces `vm.mmap_min_addr` (64 KiB on the distributions we test). Nothing
/// below this can be a real mapping — but it IS exactly what a segment carrier
/// that stored its byte SIZE where its base pointer belongs yields, which is
/// the shape that made `Arena.allocateFrom(String)` hand `strlen` the pointer
/// `0x5`.
const MIN_MAPPED_ADDR: i64 = 0x1_0000;

/// Screen an address that is about to be dereferenced by native code — either
/// by us (struct-by-value copy) or by the callee (pointer argument).
///
/// **Why this exists.** Everything downstream of here is a raw machine access
/// with no recovery: a bad pointer is a SIGSEGV inside libffi or inside the
/// foreign function, with no Java exception and no stack. Two classes of bad
/// pointer are *statically* known to be bad, and both were reachable:
///
///  * a **tagged arena handle** ([`ARENA_HANDLE_TAG`]). `Unsafe.allocateMemory`
///    returns one of these, so every direct `ByteBuffer`'s `address` is one —
///    measured: `MemorySegment.ofBuffer(ByteBuffer.allocateDirect(32))` reports
///    `0x4000001000000000`. Handing that to a callee is a guaranteed fault.
///  * an address in the **unmappable low window** ([`MIN_MAPPED_ADDR`]), which
///    is what a size-for-base carrier mix-up produces.
///
/// `0` is NOT screened here: a null pointer is a legitimate C argument and the
/// callers that cannot accept one reject it themselves.
///
/// Refusing is strictly better than the crash it replaces — the caller gets a
/// Java exception naming the step instead of losing the VM.
pub(crate) fn checked_foreign_addr(addr: i64, what: &str) -> Result<i64, MethodCallFailed> {
    if addr & ARENA_HANDLE_TAG != 0 {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Panama {what}: address {addr:#x} is a CratonVM arena handle, not a machine \
                 address (tag bit 62 set). It cannot be dereferenced by native code. This \
                 segment is backed by Unsafe.allocateMemory (e.g. a direct ByteBuffer), which \
                 CratonVM does not yet expose to foreign calls."
            ),
        }
        .into());
    }
    if addr > 0 && addr < MIN_MAPPED_ADDR {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Panama {what}: address {addr:#x} lies in the reserved low {MIN_MAPPED_ADDR:#x} \
                 bytes of the address space and can never be a valid mapping. The MemorySegment \
                 carrier is reporting a byte size or an offset where its base pointer belongs."
            ),
        }
        .into());
    }
    Ok(addr)
}

/// Maximum number of fields we will recurse through when translating
/// nested aggregate layouts. Bounds runaway recursion from a malformed
/// or hostile descriptor.
pub const MAX_LAYOUT_DEPTH: usize = 16;

/// A layout carrier this VM cannot classify.
///
/// **This is deliberately not a plausible number.** The value it replaces was
/// `LAYOUT_LONG`, and a `_ => LAYOUT_LONG` default is exactly the shape
/// `[_ => default]` warns about: it answers "eight-byte integer" for anything
/// unrecognised, which is a legal-looking answer that no caller checks. Every
/// consumer in this file now names the class in a refusal instead. It is
/// negative so that the `kind < 10` "is this a value layout" tests scattered
/// through `panama.rs` cannot accidentally admit it as one — they must be
/// written `(0..10).contains(&kind)`, and the ones that are not are NOMINATED
/// in this lane's record.
///
/// **`-2`, not `-1`, and that is not arbitrary.** `panama.rs`'s upcall-stub
/// builder already uses `-1` as its VOID sentinel
/// (`let return_kind = match return_layout { … None => -1 }`), so an unknown
/// kind spelled `-1` would be indistinguishable from "this function returns
/// nothing" in the one place that reads a kind without a layout beside it.
/// Nothing can currently reach that confusion — `layout_to_ffi_type` refuses an
/// unknown carrier on the line after — but a sentinel that collides with
/// another sentinel is a defect waiting for a reorder.
pub const LAYOUT_UNKNOWN: i32 = -2;

/// Classify a layout by the class name of its carrier.
///
/// **One name-matcher, not two.** The list this replaces carried the nine
/// `java/lang/foreign/ValueLayout$Of*` spellings and nothing else, so it could
/// not see the real JDK's own carriers — and in `--jdk-only` those are the ONLY
/// ones there are. `vm/src/vm/vm_util.rs::make_prepared_value_layout` is
/// dropped under `CompatibilityMode::JdkOnly` and the real
/// `ValueLayout.<clinit>` runs, which mints
/// `jdk/internal/foreign/layout/ValueLayouts$OfIntImpl` and friends;
/// `foreign_ffm.rs` registers natives on all nine of those class names
/// (`:3260-3347`), which is independent evidence that they reach this code.
/// They fell to the old `_ => LAYOUT_LONG`, so on `--jdk-only`
/// **every value layout was an eight-byte integer**: `JAVA_INT` read and wrote
/// eight bytes, and `JAVA_FLOAT` was passed to a downcall in an integer
/// register.
///
/// The value-layout half delegates to
/// [`p67_layout_carrier_name`](crate::phases_late::foreign_ffm::p67_layout_carrier_name),
/// which already matches both spellings with `.contains("OfInt")` and is the
/// same function `p67_layout_render` uses for the JDK's `toString` letters.
/// Delegating is what stops the two from drifting again; adding nine more
/// strings here would only have made the drift wider.
pub fn layout_kind_of_class(class_name: &str) -> i32 {
    // Group / sequence / padding FIRST. Both spellings in one test each: this
    // VM mints `java/lang/foreign/StructLayout` (foreign_ffm.rs `structLayout`)
    // and the real JDK's are `jdk/internal/foreign/layout/StructLayoutImpl`,
    // `UnionLayoutImpl`, `SequenceLayoutImpl`, `PaddingLayoutImpl` (javap,
    // 25.0.3+9-LTS). No `ValueLayout$Of*`/`ValueLayouts$Of*Impl` name contains
    // any of these substrings, so the order is a readability choice and not a
    // correctness one.
    if class_name.contains("PaddingLayout") {
        return LAYOUT_PADDING;
    }
    if class_name.contains("SequenceLayout") {
        return LAYOUT_SEQUENCE;
    }
    if class_name.contains("UnionLayout") {
        return LAYOUT_UNION;
    }
    if class_name.contains("StructLayout") || class_name.contains("GroupLayout") {
        return LAYOUT_STRUCT;
    }
    match crate::phases_late::foreign_ffm::p67_layout_carrier_name(class_name) {
        "boolean" => LAYOUT_BOOLEAN,
        "byte" => LAYOUT_BYTE,
        "char" => LAYOUT_CHAR,
        "short" => LAYOUT_SHORT,
        "int" => LAYOUT_INT,
        "long" => LAYOUT_LONG,
        "float" => LAYOUT_FLOAT,
        "double" => LAYOUT_DOUBLE,
        "java/lang/foreign/MemorySegment" => LAYOUT_ADDRESS,
        // `p67_layout_carrier_name`'s own fallback is "java/lang/Object", which
        // is not a layout carrier at all.
        _ => LAYOUT_UNKNOWN,
    }
}

/// Read the layout-kind discriminator from a layout carrier.
///
/// Two carriers reach here. `panama.rs`'s `pe_make_layout` shape tags the kind
/// in slot 0 as an `Int`; it survives only as a `#[cfg(test)]` fixture (F16,
/// 2026-08-13) but its decode is kept so those fixtures keep working. Everything
/// production mints — and every real JDK layout object — has
/// `[0] = Long(byteSize)`, and is classified by its CLASS.
///
/// Answers [`LAYOUT_UNKNOWN`] rather than a plausible eight for a carrier it
/// does not recognise.
pub fn read_layout_kind(ctx: &dyn NativeContext, layout: ObjectRef) -> i32 {
    if let Value::Int(k) = ctx.get_field(layout, 0) {
        return k;
    }
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_default();
    layout_kind_of_class(&class_name)
}

/// The class name of a layout carrier, for use in a refusal message.
fn layout_class_name(ctx: &dyn NativeContext, layout: ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// The refusal every consumer raises for a carrier this VM cannot classify.
fn unclassifiable_layout(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    what: &str,
) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: format!(
            "Panama {what}: {} is not a memory layout this VM can classify (slot 0 = {:?}). It is \
             neither a kind-tagged carrier nor a `[byteSize, byteAlignment, …]` one, and its class \
             name matches no ValueLayout, group, sequence or padding spelling.",
            layout_class_name(ctx, layout),
            ctx.get_field(layout, 0)
        ),
    }
    .into()
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
            //
            // SLOT 1 IS THE ALIGNMENT, NOT THE SIZE. This read used to be
            // `get_field(layout, 1)` and predates F16's carrier consolidation
            // (2026-08-13), which made `[0] byteSize, [1] byteAlignment` the one
            // encoding. It happened to answer correctly for the two unions in
            // the tree's tests — `unionLayout(JAVA_INT, JAVA_LONG)` is 8/8 and
            // `unionLayout(JAVA_BYTE, JAVA_INT)` is 4/4 on the oracle, size ==
            // align in both — and wrongly for every union whose widest member
            // is not also its most-aligned one. Measured on 25.0.3+9-LTS:
            //
            //   unionLayout(JAVA_BYTE, sequenceLayout(7, JAVA_BYTE))
            //       -> byteSize=7 byteAlignment=1   [b1|[7:b1]]
            //   unionLayout(sequenceLayout(7, JAVA_BYTE), JAVA_INT)
            //       -> byteSize=7 byteAlignment=4   [[7:b1]|i4]
            //
            // so the old read built a ONE-byte ffi type for the first and a
            // FOUR-byte one for the second, both of which are seven bytes wide.
            let total = layout_total_size(ctx, layout)?;
            if total == 0 || total > MAX_STRUCT_BYTES {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "UnionLayout size {} out of range (0,{}]",
                        total, MAX_STRUCT_BYTES
                    ),
                }
                .into());
            }
            let mut fields: Vec<FfiType> = Vec::with_capacity(total);
            for _ in 0..total {
                fields.push(FfiType::u8());
            }
            Ok(FfiType::structure(fields))
        }
        LAYOUT_SEQUENCE => {
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
            // THE ELEMENT COUNT IS NOT STORED. The carrier is
            // `[0] byteSize, [1] byteAlignment, [2] element, [3] name` — the
            // JDK's own `AbstractLayout` head plus the element — so the count is
            // `byteSize / elementLayout.byteSize()`, which is how
            // `p67_layout_render` reconstructs `[10:i4]` for the same object.
            //
            // The read this replaces was `get_field(layout, 1)`, i.e. the
            // ALIGNMENT. `sequenceLayout(10, JAVA_INT)` is byteSize=40
            // byteAlignment=4 on the oracle, so it built a FOUR-element ffi
            // struct for a ten-element sequence: sixteen bytes where forty were
            // meant, on every by-value sequence argument and return.
            let elem_size = layout_total_size(ctx, elem_layout)?;
            let total = layout_total_size(ctx, layout)?;
            if elem_size == 0 || total > MAX_STRUCT_BYTES {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "SequenceLayout element size 0 or total > {} bytes",
                        MAX_STRUCT_BYTES
                    ),
                }
                .into());
            }
            let count = total / elem_size;
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
        LAYOUT_UNKNOWN => Err(unclassifiable_layout(ctx, layout, "downcall signature")),
        _ => Err(RuntimeError::IllegalStateException {
            message: format!(
                "Unsupported Panama layout kind {} on {}",
                kind,
                layout_class_name(ctx, layout)
            ),
        }
        .into()),
    }
}

/// Translate a StructLayout (kind=10) to a libffi struct Type.
///
/// Slot 2 of a group carrier is its member ARRAY (F16's one encoding). A REAL
/// `jdk.internal.foreign.layout.StructLayoutImpl` does not have that shape — its
/// slot 2 is `AbstractLayout.name` and its members are an `AbstractGroupLayout.
/// elements` **`java.util.List`**, not an array (javap, 25.0.3+9-LTS) — so a
/// real group layout is REFUSED here by name rather than decoded as an empty or
/// garbage member set. It should not arise: `MemoryLayout.structLayout` /
/// `unionLayout` are both on `force_native_over_real_jdk_bytecode`'s list
/// (`native_override.rs`), so every group layout a Java program builds is one of
/// ours. JDK-internal code that builds one directly (`SharedUtils`,
/// `Linker.canonicalLayouts`) can still produce one, and a named refusal is the
/// honest answer for it.
fn layout_struct_to_ffi_type(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    depth: usize,
) -> Result<FfiType, MethodCallFailed> {
    let members_arr = match ctx.get_field(layout, 2) {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
        other => {
            return Err(RuntimeError::IllegalStateException {
                message: format!(
                    "Panama downcall signature: group layout {} does not carry a member ARRAY in \
                     slot 2 (found {:?}). A real jdk.internal.foreign.layout.StructLayoutImpl \
                     keeps its members in an AbstractGroupLayout.elements java.util.List and its \
                     name in slot 2; this VM cannot pass one by value.",
                    layout_class_name(ctx, layout),
                    other
                ),
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
                    //
                    // SLOT 1 IS THE ALIGNMENT. A `PaddingLayout` carrier is
                    // `[0] Long(size), [1] Long(1), …` (foreign_ffm.rs
                    // `paddingLayout`, and the JDK's own `paddingLayout(3)` is
                    // byteSize=3 byteAlignment=1), so this read answered **1
                    // for every padding member regardless of its width**. The
                    // padded idiom the JDK REQUIRES —
                    // `structLayout(JAVA_BYTE, paddingLayout(3), JAVA_INT)` —
                    // therefore built a `{i8, i8, i32}` ffi type, and every
                    // member after the padding sat at the wrong offset in the
                    // outgoing frame.
                    let pad_size = layout_total_size(ctx, member)?;
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

/// A layout's `byteSize`, in the one encoding.
///
/// **This is the function F16-1 NOM-1 named, and it answered 8 for a group
/// layout of any width.** It started from [`read_layout_kind`], whose class-name
/// list contained no group spelling, so every struct/union/sequence fell to
/// `_ => LAYOUT_LONG`, took the `kind < 10` branch and returned
/// `layout_byte_size(LAYOUT_LONG)` = 8. The `LAYOUT_STRUCT | …` arm below it —
/// which read the WRONG SLOT anyway, slot 1 being the alignment since F16's
/// consolidation — was unreachable.
///
/// It now reads `[0] = Long(byteSize)` directly. That is the same slot a real
/// `jdk.internal.foreign.layout.AbstractLayout` declares first
/// (`private final long byteSize; private final long byteAlignment;
/// private final Optional<String> name;`, jdk25src), so one read serves a
/// CratonVM carrier and a real JDK layout alike, and no class-name lookup is
/// needed at all for the common case.
///
/// The kind-tagged fallback is for `panama.rs`'s `#[cfg(test)]` `pe_make_layout`
/// fixture, which is the only remaining minter of an `Int`-slot-0 carrier.
pub fn layout_total_size(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
) -> Result<usize, MethodCallFailed> {
    if let Value::Long(n) = ctx.get_field(layout, 0) {
        if n < 0 {
            return Err(RuntimeError::IllegalStateException {
                message: format!(
                    "Panama layout {} reports a negative byteSize {n}",
                    layout_class_name(ctx, layout)
                ),
            }
            .into());
        }
        return Ok(n as usize);
    }
    let kind = read_layout_kind(ctx, layout);
    if kind == LAYOUT_UNKNOWN {
        return Err(unclassifiable_layout(ctx, layout, "layout size"));
    }
    if (0..10).contains(&kind) {
        return Ok(layout_byte_size(kind));
    }
    // Kind-tagged group carrier: `[0] Int(kind), [1] Int(byteSize)`.
    match ctx.get_field(layout, 1) {
        Value::Long(n) if n >= 0 => Ok(n as usize),
        Value::Int(n) if n >= 0 => Ok(n as usize),
        other => Err(RuntimeError::IllegalStateException {
            message: format!(
                "Panama layout {} is kind {kind} but carries no byte size in slot 1 (found \
                 {other:?})",
                layout_class_name(ctx, layout)
            ),
        }
        .into()),
    }
}

/// A layout's `byteAlignment`, in the one encoding.
///
/// Slot 5 — what this used to read for a group layout — is not a field of any
/// layout carrier this VM has minted since F16's consolidation, and never was a
/// field of a real JDK layout; the old body therefore always took its `_ => 1`
/// default for a group. It was moot in practice because [`read_layout_kind`]
/// could not classify a group at all, so the `kind < 10` branch answered
/// `layout_alignment(LAYOUT_LONG)` = 8 for every one of them.
///
/// Slot 1 is `byteAlignment` in the one encoding AND on a real
/// `AbstractLayout`. The size-as-alignment fallback below is the JDK's rule for
/// a value layout and is only reached when slot 1 holds no positive `Long` —
/// i.e. for the kind-tagged test fixture. Measured: `JAVA_INT_UNALIGNED` is
/// byteSize=4 byteAlignment=1 and DOES record the 1 separately, so the fallback
/// never has to guess for a real constant.
pub fn layout_align(ctx: &dyn NativeContext, layout: ObjectRef) -> usize {
    if let Value::Long(n) = ctx.get_field(layout, 1) {
        if n > 0 {
            return n as usize;
        }
    }
    let kind = read_layout_kind(ctx, layout);
    if (0..10).contains(&kind) {
        return layout_alignment(kind);
    }
    1
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
            // A HEAP SEGMENT PASSED AS A POINTER IS A REFUSAL, NOT A NULL.
            // `segment_address` answers 0 for one (see there), and 0 is a
            // legitimate C null that the screen below deliberately lets
            // through — so without this arm a `MemorySegment.ofArray(byte[])`
            // handed to a downcall would silently become `NULL` instead of
            // saying why. It used to become the array's byte length.
            if let Value::Object(Some(seg)) = arg {
                if is_real_heap_segment(ctx, *seg) {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "Panama downcall pointer argument: {} is a heap MemorySegment. Its \
                             bytes live in a Java array on the managed heap, so it has no machine \
                             address to pass to native code. Copy it into an Arena-allocated \
                             segment first.",
                            layout_class_name(ctx, *seg)
                        ),
                    }
                    .into());
                }
            }
            let v = match arg {
                Value::Object(Some(seg)) => segment_address(ctx, *seg),
                Value::Long(n) => *n,
                Value::Int(n) => *n as i64,
                _ => 0,
            };
            // The CALLEE dereferences this one, so a bad value faults inside
            // foreign code where we can neither catch nor report it. Screen it
            // here, where a refusal is still a Java exception. Null passes: it
            // is a legitimate C argument.
            let v = checked_foreign_addr(v, "downcall pointer argument")?;
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
            // WE dereference this one, in the `copy_nonoverlapping` below. Same
            // screen, same reason — an unmappable or tagged base takes the VM
            // down inside our own code rather than the callee's.
            let addr = checked_foreign_addr(addr, "struct-by-value MemorySegment base")?;
            let mut bytes = vec![0u8; total];
            // SAFETY: `addr` was sourced from a live MemorySegment field.
            // The segment's owning Arena keeps the backing memory valid
            // for the duration of this downcall (Arena lifetime > call).
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, bytes.as_mut_ptr(), total);
            }
            mk(bytes)
        }
        LAYOUT_UNKNOWN => Err(unclassifiable_layout(ctx, layout, "downcall argument")),
        _ => Err(RuntimeError::IllegalStateException {
            message: format!(
                "marshal_arg: unsupported layout kind {} on {}",
                kind,
                layout_class_name(ctx, layout)
            ),
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
pub fn is_real_heap_segment(ctx: &dyn NativeContext, seg: ObjectRef) -> bool {
    if let Some(name) = ctx.class_name_of_id(ctx.class_id_of_object(seg)) {
        if name.contains("HeapMemorySegmentImpl") {
            return true;
        }
    }
    // `HeapMemorySegmentImpl` declares `final Object base` and nothing this VM
    // mints does. `get_field_by_name` answers `Value::Object(None)` for an
    // absent name, so only a RESOLVED, non-null `base` gets here.
    matches!(ctx.get_field_by_name(seg, "base"), Value::Object(Some(_)))
}

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
    // A REAL HEAP SEGMENT HAS NO `min`, AND ITS SLOT 0 IS THE LENGTH.
    //
    // That `min` fix pinned only the positive half. `HeapMemorySegmentImpl`
    // does not extend `NativeMemorySegmentImpl` — the two are siblings under
    // `AbstractMemorySegmentImpl` — so a heap segment has FIVE fields
    // (javap, 25.0.3+9-LTS: `long length`, `boolean readOnly`,
    // `MemorySessionImpl scope`, then `long offset`, `Object base`), no `min`,
    // and fewer than the six that select the synthetic arm below. It therefore
    // fell all the way through to `get_field(seg, 0)` and answered its
    // **byteSize**.
    //
    // That is the crash W7-89 §7.1 records and attributes elsewhere:
    // `MemorySegment.ofArray(new byte[16]).set(JAVA_INT_UNALIGNED, 0, 7)` dies
    // with `EXCEPTION_ACCESS_VIOLATION … read at address 0x10`, and
    // **0x10 == 16 == the array's byte length**. §7.1 reads the 0x10 as "a
    // `Buffer.address` read as a pointer"; it is this function returning
    // `length`. `ofArray` IS force-routed (`native_override.rs`, the
    // `MemorySegment` name list) but `panama.rs` registers it only for `[I`,
    // `[J`, `[F` and `[D` — **not `[B`, `[S` or `[C`** — so those three run real
    // JDK bytecode and produce a real `HeapMemorySegmentImpl$OfByte/OfShort/
    // OfChar`. The three registered ones copy into native memory and hand back
    // a six-slot synthetic, which is why only the byte/short/char arms crash.
    //
    // A heap segment's bytes live in a Java array on the managed heap. There is
    // no machine address to answer with, and inventing one is how this went
    // wrong in the first place — so answer 0, which every caller already treats
    // as "not accessible" (`pe_segment_access_addr` raises
    // `IllegalStateException: Null segment address`; `marshal_arg` refuses by
    // name below). A Java exception is strictly better than a SIGSEGV, and
    // teaching the get/set path to read the backing array is NOMINATED.
    if is_real_heap_segment(ctx, seg) {
        return 0;
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
    if kind == LAYOUT_UNKNOWN {
        return Err(unclassifiable_layout(ctx, layout, "downcall return layout"));
    }
    // libffi requires the result buffer to be at least sizeof(ffi_arg)
    // (= sizeof(usize)) for primitive returns smaller than that; if we
    // pass a smaller buffer, libffi may write past the end on big-endian.
    // We always allocate at least sizeof(usize).
    let raw_size = layout_total_size(ctx, layout)?;
    // AND AT LEAST THE C SIZE OF AN AGGREGATE, WHICH IS NOT THE JDK'S BYTE SIZE.
    //
    // `panama.rs` hands this Vec's pointer to `ffi_call` as the result address,
    // and libffi writes `cif->rtype->size` bytes there. For an aggregate that
    // size is computed by `ffi_prep_cif` under C rules, which round a struct's
    // total UP to the struct's alignment. The JDK does NOT round: measured on
    // 25.0.3+9-LTS, `structLayout(JAVA_LONG, JAVA_INT).byteSize()` is **12**,
    // where the C type `struct { long; int; }` is 16. Sizing this buffer from
    // `byteSize` alone would hand libffi twelve bytes and let it write sixteen.
    //
    // This is not a hypothetical introduced by correcting `layout_total_size`.
    // BEFORE that correction the function answered 8 for a group layout of ANY
    // width, so `size` was `8.max(8)` = 8 and libffi wrote the full aggregate
    // into an eight-byte `Vec` — an out-of-bounds WRITE past a heap allocation,
    // on every by-value aggregate return wider than eight bytes. Rounding up
    // here is what makes the corrected size safe as well as right, and the
    // rounding can only ever ENLARGE the buffer, so it cannot introduce one.
    let size = align_up(raw_size, layout_align(ctx, layout).max(1))
        .max(raw_size)
        .max(std::mem::size_of::<usize>());
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
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

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

    /// A tagged arena handle must never reach native code.
    ///
    /// `0x4000_0010_0000_0000` is not a hypothetical: it is the address
    /// `MemorySegment.ofBuffer(ByteBuffer.allocateDirect(32))` reports on this
    /// VM, measured. It is non-null and 16-aligned, so every pre-existing
    /// screen passed it through to be dereferenced.
    #[test]
    fn tagged_arena_handle_is_refused() {
        let tagged = ARENA_HANDLE_TAG | 0x10_0000_0000;
        let err = checked_foreign_addr(tagged, "downcall pointer argument")
            .expect_err("a tagged arena handle must be refused, not dereferenced");
        match err {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalStateException { message },
            )) => {
                assert!(
                    message.contains("arena handle"),
                    "message must name the cause: {message}"
                );
            }
            other => panic!("expected IllegalStateException, got {other:?}"),
        }
    }

    /// The exact shape that segfaulted: `Arena.allocateFrom("abcd")` handed
    /// `strlen` the pointer `0x5` — the segment's byte SIZE read out of the
    /// slot its base pointer belongs in.
    #[test]
    fn unmappable_low_address_is_refused() {
        assert!(
            checked_foreign_addr(5, "downcall pointer argument").is_err(),
            "an address in the reserved low window must be refused"
        );
        assert!(
            checked_foreign_addr(MIN_MAPPED_ADDR - 1, "downcall pointer argument").is_err(),
            "the top of the reserved low window must still be refused"
        );
    }

    /// The screen must not fire on the values a working downcall uses: a null
    /// pointer is a legitimate C argument, and a real mapping passes.
    #[test]
    fn null_and_real_addresses_pass() {
        assert_eq!(
            checked_foreign_addr(0, "downcall pointer argument").ok(),
            Some(0),
            "NULL is a legitimate C pointer argument"
        );
        let buf = [0u8; 8];
        let real = buf.as_ptr() as i64;
        assert_eq!(
            checked_foreign_addr(real, "downcall pointer argument").ok(),
            Some(real),
            "a genuine mapping must pass the screen"
        );
        assert_eq!(
            checked_foreign_addr(MIN_MAPPED_ADDR, "downcall pointer argument").ok(),
            Some(MIN_MAPPED_ADDR),
            "the screen is exclusive at its lower bound"
        );
    }

    // -----------------------------------------------------------------
    // F27 — the layout carrier, read in the ONE encoding
    // -----------------------------------------------------------------
    //
    // Every number asserted below is `java` on Microsoft build 25.0.3+9-LTS,
    // quoted at the assertion. Nothing here was run on CratonVM.

    /// The real JDK's own value-layout carriers, which `--jdk-only` is the ONLY
    /// producer of (`vm_util.rs::make_prepared_value_layout` is dropped under
    /// `CompatibilityMode::JdkOnly`, so the real `ValueLayout.<clinit>` runs).
    ///
    /// Before this lane every one of these fell to `_ => LAYOUT_LONG`. That is
    /// the mutation to make if this test ever looks redundant: replace the
    /// delegation with the old nine-string list and all nine `…Impl` rows go to
    /// `LAYOUT_LONG`.
    #[test]
    fn real_jdk_value_layout_carriers_are_classified() {
        for (class_name, expected) in [
            (
                "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
                LAYOUT_BOOLEAN,
            ),
            (
                "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
                LAYOUT_BYTE,
            ),
            (
                "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
                LAYOUT_CHAR,
            ),
            (
                "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
                LAYOUT_SHORT,
            ),
            (
                "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
                LAYOUT_INT,
            ),
            (
                "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
                LAYOUT_LONG,
            ),
            (
                "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
                LAYOUT_FLOAT,
            ),
            (
                "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
                LAYOUT_DOUBLE,
            ),
            (
                "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
                LAYOUT_ADDRESS,
            ),
            // and this VM's own spellings, which already worked
            ("java/lang/foreign/ValueLayout$OfInt", LAYOUT_INT),
            ("java/lang/foreign/ValueLayout$OfFloat", LAYOUT_FLOAT),
            ("java/lang/foreign/AddressLayout", LAYOUT_ADDRESS),
        ] {
            assert_eq!(
                layout_kind_of_class(class_name),
                expected,
                "{class_name} must classify as {expected}, not default to LAYOUT_LONG"
            );
        }
    }

    /// Group, sequence and padding carriers in both spellings. No group class
    /// was on the old list at all, which is why `layout_total_size` answered 8
    /// for a struct of any width.
    #[test]
    fn group_and_sequence_carriers_are_classified() {
        for (class_name, expected) in [
            ("java/lang/foreign/StructLayout", LAYOUT_STRUCT),
            (
                "jdk/internal/foreign/layout/StructLayoutImpl",
                LAYOUT_STRUCT,
            ),
            ("java/lang/foreign/GroupLayout", LAYOUT_STRUCT),
            ("java/lang/foreign/UnionLayout", LAYOUT_UNION),
            ("jdk/internal/foreign/layout/UnionLayoutImpl", LAYOUT_UNION),
            ("java/lang/foreign/SequenceLayout", LAYOUT_SEQUENCE),
            (
                "jdk/internal/foreign/layout/SequenceLayoutImpl",
                LAYOUT_SEQUENCE,
            ),
            ("java/lang/foreign/PaddingLayout", LAYOUT_PADDING),
            (
                "jdk/internal/foreign/layout/PaddingLayoutImpl",
                LAYOUT_PADDING,
            ),
        ] {
            assert_eq!(
                layout_kind_of_class(class_name),
                expected,
                "{class_name} must classify as {expected}"
            );
            assert_ne!(
                layout_kind_of_class(class_name),
                LAYOUT_LONG,
                "{class_name} must not fall to the old LAYOUT_LONG default"
            );
        }
    }

    /// The unknown case must be LOUD, not plausible.
    #[test]
    fn an_unrecognised_carrier_is_unknown_not_an_eight_byte_integer() {
        for class_name in [
            "java/lang/String",
            "java/lang/foreign/MemorySegment$Scope",
            "",
        ] {
            assert_eq!(
                layout_kind_of_class(class_name),
                LAYOUT_UNKNOWN,
                "{class_name:?} is not a layout carrier and must say so"
            );
        }
        assert!(
            primitive_kind_to_ffi_type(LAYOUT_UNKNOWN).is_none(),
            "LAYOUT_UNKNOWN must not translate to any ffi type"
        );
        assert!(
            !(0..10).contains(&LAYOUT_UNKNOWN),
            "LAYOUT_UNKNOWN must not be admitted by a `is this a value layout` range test"
        );
    }

    /// `structLayout(JAVA_LONG, JAVA_INT)` on the oracle:
    /// `byteSize=12 byteAlignment=8` — the total is NOT rounded up to 16.
    ///
    /// Before this lane both readers ignored the carrier: `layout_total_size`
    /// answered `layout_byte_size(LAYOUT_LONG)` = 8 and `layout_align` the same
    /// 8, for a group layout of ANY width.
    #[test]
    fn group_layout_size_and_alignment_come_from_the_carrier() {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/foreign/StructLayout")
            .unwrap();
        let members = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
        let sl = ctx.alloc_object(cid, 4);
        ctx.set_field(sl, 0, Value::Long(12));
        ctx.set_field(sl, 1, Value::Long(8));
        ctx.set_field(sl, 2, Value::Object(Some(members)));
        ctx.set_field(sl, 3, Value::Object(None));

        assert_eq!(read_layout_kind(&ctx, sl), LAYOUT_STRUCT);
        assert_eq!(
            layout_total_size(&ctx, sl).unwrap(),
            12,
            "structLayout(JAVA_LONG, JAVA_INT).byteSize() is 12 on 25.0.3+9-LTS"
        );
        assert_eq!(
            layout_align(&ctx, sl),
            8,
            "structLayout(JAVA_LONG, JAVA_INT).byteAlignment() is 8"
        );
    }

    /// `sequenceLayout(10, JAVA_INT)` is `byteSize=40 byteAlignment=4`. The
    /// carrier records the TOTAL, never the element count — the count is
    /// `byteSize / element byteSize`. Reading slot 1 as the count (what
    /// `layout_to_ffi_type` did) yields 4.
    #[test]
    fn sequence_carrier_reports_its_total_not_its_alignment() {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/foreign/SequenceLayout")
            .unwrap();
        let elem_cid = ctx
            .ensure_class_initialized("java/lang/foreign/ValueLayout$OfInt")
            .unwrap();
        let elem = ctx.alloc_object(elem_cid, 4);
        ctx.set_field(elem, 0, Value::Long(4));
        ctx.set_field(elem, 1, Value::Long(4));
        let seq = ctx.alloc_object(cid, 4);
        ctx.set_field(seq, 0, Value::Long(40));
        ctx.set_field(seq, 1, Value::Long(4));
        ctx.set_field(seq, 2, Value::Object(Some(elem)));
        ctx.set_field(seq, 3, Value::Object(None));

        assert_eq!(read_layout_kind(&ctx, seq), LAYOUT_SEQUENCE);
        assert_eq!(layout_total_size(&ctx, seq).unwrap(), 40);
        assert_eq!(layout_align(&ctx, seq), 4);
        assert_eq!(layout_total_size(&ctx, elem).unwrap(), 4);
        assert_eq!(
            layout_total_size(&ctx, seq).unwrap() / layout_total_size(&ctx, elem).unwrap(),
            10,
            "the element count is derived, and it is 10"
        );
    }

    /// The union row that separates "size" from "alignment". Measured on
    /// 25.0.3+9-LTS:
    ///
    /// ```text
    /// unionLayout(sequenceLayout(7, JAVA_BYTE), JAVA_INT)
    ///     -> byteSize=7 byteAlignment=4   [[7:b1]|i4]
    /// ```
    ///
    /// The two other unions in the tree's tests are 8/8 and 4/4, where size and
    /// alignment coincide — which is why reading slot 1 (the alignment) as the
    /// union's SIZE in `layout_to_ffi_type` passed every test there was.
    #[test]
    fn a_unions_size_is_not_its_alignment() {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/foreign/UnionLayout")
            .unwrap();
        let members = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
        let u = ctx.alloc_object(cid, 4);
        ctx.set_field(u, 0, Value::Long(7));
        ctx.set_field(u, 1, Value::Long(4));
        ctx.set_field(u, 2, Value::Object(Some(members)));
        ctx.set_field(u, 3, Value::Object(None));

        assert_eq!(read_layout_kind(&ctx, u), LAYOUT_UNION);
        assert_eq!(layout_total_size(&ctx, u).unwrap(), 7);
        assert_eq!(layout_align(&ctx, u), 4);
        assert_ne!(
            layout_total_size(&ctx, u).unwrap(),
            layout_align(&ctx, u),
            "this row exists precisely because the two differ"
        );
    }

    /// **The out-of-bounds WRITE.** `panama.rs` hands this Vec's pointer to
    /// `ffi_call` as the result address and libffi writes `rtype->size` bytes
    /// there — the C size of the aggregate, which rounds up to the alignment.
    /// `structLayout(JAVA_LONG, JAVA_INT)` is 12 to the JDK and 16 to C.
    ///
    /// Before this lane the buffer was `8` for every aggregate return, because
    /// `layout_total_size` answered 8.
    #[test]
    fn aggregate_return_slot_covers_the_c_size_not_just_the_jdk_byte_size() {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/foreign/StructLayout")
            .unwrap();
        let members = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
        let sl = ctx.alloc_object(cid, 4);
        ctx.set_field(sl, 0, Value::Long(12));
        ctx.set_field(sl, 1, Value::Long(8));
        ctx.set_field(sl, 2, Value::Object(Some(members)));
        ctx.set_field(sl, 3, Value::Object(None));

        let slot = alloc_return_slot(&ctx, Some(sl)).unwrap();
        assert_eq!(
            slot.len(),
            16,
            "libffi writes align_up(12, 8) = 16 bytes for struct {{ long; int; }}"
        );
        assert!(
            slot.len() >= layout_total_size(&ctx, sl).unwrap(),
            "the slot can never be shorter than the layout it is sized for"
        );
    }

    /// A primitive return still gets at least `sizeof(ffi_arg)`.
    #[test]
    fn primitive_return_slot_is_still_widened_to_ffi_arg() {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/foreign/ValueLayout$OfByte")
            .unwrap();
        let vl = ctx.alloc_object(cid, 4);
        ctx.set_field(vl, 0, Value::Long(1));
        ctx.set_field(vl, 1, Value::Long(1));
        assert_eq!(
            alloc_return_slot(&ctx, Some(vl)).unwrap().len(),
            std::mem::size_of::<usize>()
        );
        assert!(alloc_return_slot(&ctx, None).unwrap().is_empty());
    }

    /// A carrier this VM cannot classify must not be sized as an eight-byte
    /// integer on the way into a downcall.
    #[test]
    fn an_unclassifiable_return_layout_is_refused() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("java/lang/Object").unwrap();
        let junk = ctx.alloc_object(cid, 2);
        ctx.set_field(junk, 0, Value::Object(None));
        let err = alloc_return_slot(&ctx, Some(junk))
            .expect_err("an unclassifiable return layout must be refused, not sized as a long");
        match err {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalStateException { message },
            )) => assert!(
                message.contains("java/lang/Object"),
                "the refusal must name the class: {message}"
            ),
            other => panic!("expected IllegalStateException, got {other:?}"),
        }
    }

    /// **W7-89 §7.1, diagnosed.** A real `HeapMemorySegmentImpl` has five
    /// fields, no `min`, and `AbstractMemorySegmentImpl.length` at slot 0
    /// (javap, 25.0.3+9-LTS). `segment_address` used to fall through to
    /// `get_field(seg, 0)` and hand out that LENGTH as a machine address:
    /// `MemorySegment.ofArray(new byte[16]).set(...)` died with
    /// `EXCEPTION_ACCESS_VIOLATION … read at address 0x10`, and 0x10 is 16.
    #[test]
    fn a_real_heap_segment_has_no_machine_address() {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("jdk/internal/foreign/HeapMemorySegmentImpl$OfByte")
            .unwrap();
        let seg = ctx.alloc_object(cid, 5);
        ctx.set_field(seg, 0, Value::Long(16)); // length
        ctx.set_field(seg, 1, Value::Int(0)); // readOnly
        ctx.set_field(seg, 2, Value::Object(None)); // scope
        ctx.set_field(seg, 3, Value::Long(0)); // offset

        assert!(is_real_heap_segment(&ctx, seg));
        assert_ne!(
            segment_address(&ctx, seg),
            16,
            "the segment's byte LENGTH must never be answered as its address"
        );
        assert_eq!(
            segment_address(&ctx, seg),
            0,
            "a heap segment has no machine address; 0 is what every caller refuses on"
        );
        assert_eq!(
            segment_byte_size(&ctx, seg),
            16,
            "its SIZE is still slot 0 and must keep answering"
        );
    }

    /// The two carriers that were already right must stay right — this is the
    /// negative control for the arm above.
    #[test]
    fn native_and_synthetic_segment_addresses_are_unchanged() {
        let mut ctx = mock_ctx();
        let native_cid = ctx
            .ensure_class_initialized("jdk/internal/foreign/NativeMemorySegmentImpl")
            .unwrap();
        let native = ctx.alloc_object(native_cid, 4);
        ctx.set_field(native, 0, Value::Long(276)); // length
        ctx.set_field_by_name(native, "min", Value::Long(0x7f00_0000));
        assert!(!is_real_heap_segment(&ctx, native));
        assert_eq!(segment_address(&ctx, native), 0x7f00_0000);

        let synth_cid = ctx
            .ensure_class_initialized("java/lang/foreign/MemorySegment")
            .unwrap();
        let synth = ctx.alloc_object(synth_cid, 6);
        ctx.set_field(synth, 0, Value::Long(0x1000)); // base
        ctx.set_field(synth, 1, Value::Long(64)); // size
        ctx.set_field(synth, 5, Value::Long(0x20)); // offset
        assert!(!is_real_heap_segment(&ctx, synth));
        assert_eq!(
            segment_address(&ctx, synth),
            0x1020,
            "the synthetic (base@0 + offset@5) carrier is untouched"
        );
    }

    /// A heap segment handed to a downcall as a pointer must be refused BY
    /// NAME. `segment_address` answers 0 for one, and 0 is a legitimate C null
    /// that `checked_foreign_addr` deliberately passes — so without the arm in
    /// `marshal_arg` the call would silently become `NULL`.
    #[test]
    fn a_heap_segment_pointer_argument_is_refused_by_name() {
        let mut ctx = mock_ctx();
        let layout_cid = ctx
            .ensure_class_initialized("java/lang/foreign/AddressLayout")
            .unwrap();
        let layout = ctx.alloc_object(layout_cid, 4);
        ctx.set_field(layout, 0, Value::Long(8));
        ctx.set_field(layout, 1, Value::Long(8));

        let seg_cid = ctx
            .ensure_class_initialized("jdk/internal/foreign/HeapMemorySegmentImpl$OfByte")
            .unwrap();
        let seg = ctx.alloc_object(seg_cid, 5);
        ctx.set_field(seg, 0, Value::Long(16));

        // `expect_err` would require `ArgSlot: Debug`; it holds the raw outgoing
        // frame and deliberately does not derive it. Destructure instead so the
        // assertion stays on the error without widening the type's API.
        let Err(err) = marshal_arg(&ctx, layout, &Value::Object(Some(seg))) else {
            panic!("a heap MemorySegment has no address to pass to native code");
        };
        match err {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalStateException { message },
            )) => assert!(
                message.contains("heap MemorySegment"),
                "the refusal must say why: {message}"
            ),
            other => panic!("expected IllegalStateException, got {other:?}"),
        }

        // Control: the same layout with a plain long address still marshals.
        assert_eq!(
            marshal_arg(&ctx, layout, &Value::Long(0x7f00_0000))
                .unwrap()
                .bytes
                .len(),
            std::mem::size_of::<usize>()
        );
    }

    /// `JAVA_INT` must marshal FOUR bytes. On `--jdk-only` the receiver is a
    /// `ValueLayouts$OfIntImpl`, which used to classify as `LAYOUT_LONG` — so
    /// the argument slot was eight bytes and the CIF said `i64`.
    #[test]
    fn a_real_jdk_int_layout_marshals_four_bytes() {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("jdk/internal/foreign/layout/ValueLayouts$OfIntImpl")
            .unwrap();
        let vl = ctx.alloc_object(cid, 5);
        ctx.set_field(vl, 0, Value::Long(4));
        ctx.set_field(vl, 1, Value::Long(4));
        assert_eq!(read_layout_kind(&ctx, vl), LAYOUT_INT);
        assert_eq!(layout_total_size(&ctx, vl).unwrap(), 4);
        assert_eq!(
            marshal_arg(&ctx, vl, &Value::Int(7)).unwrap().bytes.len(),
            4,
            "a JAVA_INT argument occupies four bytes, not eight"
        );
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
