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
pub(crate) const ARENA_HANDLE_TAG: i64 = 1 << 62;

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

/// `CRATONVM_FFM_OPAQUE_LOW_POINTER_ARGS` (default on): a zero-length
/// segment passed as a pointer argument is screened for the arena-handle tag
/// only, not for the low window (see the downcall argument arm).
///
/// Cached (round 13 wave 3, lane ffm2 review): every `MemorySegment.NULL`
/// argument is zero-length, so an uncached read was an environment lookup per
/// null pointer argument per downcall.
pub(crate) fn opaque_low_pointer_args_allowed() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_OPAQUE_LOW_POINTER_ARGS")
    })
}

/// The arena-handle half of [`checked_foreign_addr`] alone, for pointer
/// VALUES the VM never dereferences itself.
pub(crate) fn checked_foreign_handle_tag(addr: i64, what: &str) -> Result<i64, MethodCallFailed> {
    if addr & ARENA_HANDLE_TAG != 0 {
        return checked_foreign_addr(addr, what);
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
    if !ctx.object_is_array(arr) {
        // SBR2 wave 8: a REAL `FunctionDescriptorImpl` (the retired
        // `FunctionDescriptor.of`/`ofVoid`) keeps `argLayouts` -- slot 1, the
        // same slot -- as an immutable `List`, not the carrier's array.
        return match crate::phases_late::foreign_ffm::p67_member_list_decode(ctx, arr) {
            Some((store, count)) => (0..count)
                .map(|i| {
                    match crate::phases_late::foreign_ffm::p67_group_member_at(ctx, store, i) {
                        Value::Object(Some(layout)) => Some(layout),
                        _ => None,
                    }
                })
                .collect(),
            None => Vec::new(),
        };
    }
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
        // A Java `char` is UNSIGNED: HotSpot passes `(char) -1` to C as 65535
        // (`abs((char) x)` answers `x`). JIT round 22 (lane memory, found by
        // `R22FfmCompiledRows`); `CRATONVM_FFM_CHAR_ARG_UNSIGNED=0` restores
        // `sint16`.
        LAYOUT_CHAR if char_arg_unsigned() => Some(FfiType::u16()),
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
            // Round 13 wave 12 (lane misc12): on x86-64 SysV the byte struct
            // classifies every eightbyte INTEGER, where C merges the members'
            // classes, so `union { double; float }` travelled in RDI/RAX
            // instead of XMM0 -- in both directions, as the upcall stub builds
            // its type here too. Win64 passes an aggregate by size and keeps
            // the byte struct.
            if cfg!(all(target_arch = "x86_64", not(windows))) && union_eightbyte_class() {
                if let Some(ty) = union_sysv_eightbyte_ffi_type(ctx, layout, total, depth) {
                    return Ok(ty);
                }
            }
            // Round 13 wave 13 (lane ffm7): on AAPCS64 (Linux, macOS and
            // Windows on ARM64 alike) a union whose members are all `float`
            // (or all `double`) is a Homogeneous Floating-point Aggregate and
            // travels in `s0..s3` / `d0..d3`; the byte struct went in `x0`.
            if cfg!(target_arch = "aarch64") && union_eightbyte_class() {
                if let Some(ty) = union_aapcs64_hfa_ffi_type(ctx, layout, total, depth) {
                    return Ok(ty);
                }
            }
            let mut fields: Vec<FfiType> = Vec::with_capacity(total);
            for _ in 0..total {
                fields.push(FfiType::u8());
            }
            Ok(FfiType::structure(fields))
        }
        LAYOUT_SEQUENCE => {
            // The payload slot by name (round 13 wave 5, lane ffm3): the real
            // `SequenceLayoutImpl` keeps `elementLayout` where its fields put
            // it; a fabricated carrier keeps it at 2, which is what the
            // resolver answers for one.
            let elem_slot = if group_members_by_name() {
                crate::phases_late::foreign_ffm::p67_layout_slots(ctx, layout).payload
            } else {
                2
            };
            let elem_layout = match ctx.get_field(layout, elem_slot) {
                Value::Object(Some(e)) => e,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "SequenceLayout missing element layout".into(),
                    }
                    .into());
                }
            };
            let elem_ty = layout_to_ffi_type(ctx, elem_layout, depth + 1)?;
            // NOM-2 (G6-1). The carrier's shared prefix is
            // `[0] byteSize, [1] byteAlignment, [2] element, [3] name` — the
            // JDK's own `AbstractLayout` head plus the element — and a
            // sequence carrier minted since G6-1 §8.2 carries a fifth slot,
            // `[4] elementCount`, holding the STORED count. Prefer that slot
            // exactly as `SequenceLayout.elementCount()`
            // (`phases_late/foreign_ffm.rs`) does, and fall back to dividing
            // the total by the element size only for a four-slot carrier
            // minted elsewhere — the division is not equivalent for an
            // element whose own byteSize is 0 (`structLayout()`,
            // `unionLayout()`), where it answers 0 for every count.
            //
            // The read this whole scheme replaces was `get_field(layout, 1)`,
            // i.e. the ALIGNMENT. `sequenceLayout(10, JAVA_INT)` is
            // byteSize=40 byteAlignment=4 on the oracle, so it built a
            // FOUR-element ffi struct for a ten-element sequence: sixteen
            // bytes where forty were meant, on every by-value sequence
            // argument and return.
            let slots = crate::phases_late::foreign_ffm::p67_layout_slots(ctx, layout);
            let stored_count = if ctx.object_num_fields(layout) > slots.element_count {
                match ctx.get_field(layout, slots.element_count) {
                    Value::Long(n) if n >= 0 => Some(n as usize),
                    _ => None,
                }
            } else {
                None
            };
            let total = layout_total_size(ctx, layout)?;
            if total > MAX_STRUCT_BYTES {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("SequenceLayout total > {} bytes", MAX_STRUCT_BYTES),
                }
                .into());
            }
            let count = match stored_count {
                Some(n) => n,
                None => {
                    let elem_size = layout_total_size(ctx, elem_layout)?;
                    if elem_size == 0 {
                        return Err(RuntimeError::IllegalStateException {
                            message: "SequenceLayout element size 0".into(),
                        }
                        .into());
                    }
                    total / elem_size
                }
            };
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
/// A fabricated group carrier keeps its member ARRAY in slot 2 (F16's
/// encoding); a REAL `jdk.internal.foreign.layout.StructLayoutImpl` -- which is
/// what `MemoryLayout.structLayout` mints since the layout carriers moved onto
/// their real classes -- keeps `AbstractLayout.name` there and its members in
/// `AbstractGroupLayout.elements`, a `java.util.List`. Both are read through
/// `foreign_ffm::p67_group_members` (round 13 wave 5, lane ffm3; it used to
/// refuse the real shape by name, which refused every struct passed or
/// returned by value).
fn layout_struct_to_ffi_type(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    depth: usize,
) -> Result<FfiType, MethodCallFailed> {
    // Round 13 wave 5 (lane ffm3): through `foreign_ffm::p67_group_members`,
    // which reads the payload slot BY NAME and unwraps the real class's
    // `elements` `List` (`ImmutableCollections$ListN` / `ArrayList`) as well as
    // a fabricated carrier's bare array. `MemoryLayout.structLayout` mints the
    // REAL `StructLayoutImpl` now, so the slot-2 array read this replaced
    // refused every by-value struct a Java program builds
    // (`R13Ffm3StructReturn`: "does not carry a member ARRAY in slot 2").
    let (members_arr, count) = if group_members_by_name() {
        match crate::phases_late::foreign_ffm::p67_group_members(ctx, layout) {
            Some(found) => found,
            None => (layout, 0),
        }
    } else {
        match ctx.get_field(layout, 2) {
            Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => {
                (a, ctx.array_length(a))
            }
            other => {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "Panama downcall signature: group layout {} does not carry a member \
                         ARRAY in slot 2 (found {:?}); CRATONVM_FFM_GROUP_MEMBERS_BY_NAME=0",
                        layout_class_name(ctx, layout),
                        other
                    ),
                }
                .into());
            }
        }
    };
    if count == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "StructLayout has no members".into(),
        }
        .into());
    }
    let mut fields: Vec<FfiType> = Vec::with_capacity(count);
    for i in 0..count {
        match crate::phases_late::foreign_ffm::p67_group_member_at(ctx, members_arr, i) {
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

// ---------------------------------------------------------------------------
// A by-value union's SysV classification (round 13 wave 12, lane misc12)
// ---------------------------------------------------------------------------
//
// `r13w11-ffm6-union-ffi-type-is-always-integer-class`. libffi has no union
// type; the union arm builds `struct { uint8_t[byteSize] }`, which libffi's
// `classify_argument` (ffi64.c) puts in INTEGER for every eightbyte. The psABI
// classifies a union eightbyte by merging its members' classes: INTEGER if any
// integer or pointer member overlaps it, SSE if only `float` / `double` do. So
// for a union of at most 16 bytes with an all-float eightbyte, the struct is
// rebuilt with one float element there (the rest stays bytes), which libffi
// classifies exactly as C does. Larger unions are MEMORY class whatever their
// members, and a union with no SSE eightbyte keeps the byte struct unchanged.

/// `CRATONVM_FFM_UNION_EIGHTBYTE_CLASS` (default on; round 13 wave 12, lane
/// misc12): a by-value union's all-float eightbytes are passed and returned in
/// SSE registers on x86-64 SysV, as C does; since round 13 wave 13 (lane ffm7)
/// also an all-`float` / all-`double` union is an HFA on AArch64
/// ([`union_aapcs64_hfa_ffi_type`]). `0` restores the byte struct (INTEGER
/// eightbytes, general registers). Read when a signature is built, not per
/// call.
fn union_eightbyte_class() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UNION_EIGHTBYTE_CLASS")
}

/// A SysV eightbyte's merged class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Eightbyte {
    /// Only padding overlaps it.
    NoClass,
    Integer,
    Sse,
}

/// One element of the rebuilt union struct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnionElem {
    Byte,
    F32,
    F64,
}

/// The scalar leaves of `layout` placed at `base`, as `(offset, size,
/// is_float)`, recursing through structs (members back to back, as the JDK
/// lays them out), unions (every member at `base`) and sequences. Padding adds
/// nothing. `false` when a member cannot be read or classified.
fn collect_layout_leaves(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    base: usize,
    depth: usize,
    out: &mut Vec<(usize, usize, bool)>,
) -> bool {
    if depth > MAX_LAYOUT_DEPTH || out.len() > 64 {
        return false;
    }
    let kind = read_layout_kind(ctx, layout);
    match kind {
        LAYOUT_PADDING => true,
        LAYOUT_FLOAT | LAYOUT_DOUBLE | LAYOUT_BYTE | LAYOUT_BOOLEAN | LAYOUT_SHORT
        | LAYOUT_CHAR | LAYOUT_INT | LAYOUT_LONG | LAYOUT_ADDRESS => {
            let Ok(size) = layout_total_size(ctx, layout) else {
                return false;
            };
            out.push((base, size, matches!(kind, LAYOUT_FLOAT | LAYOUT_DOUBLE)));
            true
        }
        LAYOUT_STRUCT | LAYOUT_UNION => {
            let members = if group_members_by_name() {
                crate::phases_late::foreign_ffm::p67_group_members(ctx, layout)
            } else {
                match ctx.get_field(layout, 2) {
                    Value::Object(Some(a)) if ctx.object_is_array(a) => {
                        Some((a, ctx.array_length(a)))
                    }
                    _ => None,
                }
            };
            let Some((members, count)) = members else {
                return false;
            };
            let mut offset = base;
            for i in 0..count {
                let Value::Object(Some(member)) =
                    crate::phases_late::foreign_ffm::p67_group_member_at(ctx, members, i)
                else {
                    return false;
                };
                let at = if kind == LAYOUT_STRUCT { offset } else { base };
                if !collect_layout_leaves(ctx, member, at, depth + 1, out) {
                    return false;
                }
                if kind == LAYOUT_STRUCT {
                    let Ok(size) = layout_total_size(ctx, member) else {
                        return false;
                    };
                    offset += size;
                }
            }
            true
        }
        LAYOUT_SEQUENCE => {
            let slots = crate::phases_late::foreign_ffm::p67_layout_slots(ctx, layout);
            let elem_slot = if group_members_by_name() {
                slots.payload
            } else {
                2
            };
            let Value::Object(Some(elem)) = ctx.get_field(layout, elem_slot) else {
                return false;
            };
            let (Ok(total), Ok(elem_size)) =
                (layout_total_size(ctx, layout), layout_total_size(ctx, elem))
            else {
                return false;
            };
            if elem_size == 0 {
                return true;
            }
            for i in 0..total / elem_size {
                if !collect_layout_leaves(ctx, elem, base + i * elem_size, depth + 1, out) {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

/// The merged class of each eightbyte of a `total`-byte aggregate whose
/// scalar leaves are `leaves` (psABI 3.2.3: INTEGER wins, then SSE; padding is
/// NO_CLASS). `None` when a leaf is not naturally aligned (C would make the
/// aggregate MEMORY, and a JDK linker refuses such a layout).
fn sysv_eightbyte_classes(total: usize, leaves: &[(usize, usize, bool)]) -> Option<Vec<Eightbyte>> {
    let mut classes = vec![Eightbyte::NoClass; total.div_ceil(8)];
    for &(offset, size, is_float) in leaves {
        if size == 0 {
            continue;
        }
        if offset % size != 0 || offset + size > total {
            return None;
        }
        for class in &mut classes[offset / 8..=(offset + size - 1) / 8] {
            *class = match (*class, is_float) {
                (_, false) | (Eightbyte::Integer, true) => Eightbyte::Integer,
                _ => Eightbyte::Sse,
            };
        }
    }
    Some(classes)
}

/// The elements of the rebuilt union struct: bytes for an INTEGER or padding
/// eightbyte (what the byte struct had), a `double` for an 8-byte SSE
/// eightbyte of an 8-aligned union, two `float`s for one of a 4-aligned union,
/// a `float` for a 4-byte final one. `None` (keep the byte struct) when no
/// eightbyte is SSE, or when the float elements would change the libffi
/// type's size from `total`.
fn union_eightbyte_elems(
    total: usize,
    align: usize,
    classes: &[Eightbyte],
) -> Option<Vec<UnionElem>> {
    if total > 16 || !classes.contains(&Eightbyte::Sse) {
        return None;
    }
    let mut elems = Vec::with_capacity(total);
    let mut widest = 1usize;
    for (k, class) in classes.iter().enumerate() {
        let len = (total - 8 * k).min(8);
        match (class, len) {
            (Eightbyte::Sse, 8) if align >= 8 => {
                elems.push(UnionElem::F64);
                widest = 8;
            }
            (Eightbyte::Sse, 8) if align >= 4 => {
                elems.extend([UnionElem::F32, UnionElem::F32]);
                widest = widest.max(4);
            }
            (Eightbyte::Sse, 4) if align >= 4 => {
                elems.push(UnionElem::F32);
                widest = widest.max(4);
            }
            (Eightbyte::Sse, _) => return None,
            _ => elems.extend(std::iter::repeat_n(UnionElem::Byte, len)),
        }
    }
    // libffi rounds the struct up to its widest element; the union's own size
    // must already be a multiple of it, or the argument copy would over-read.
    (total % widest == 0).then_some(elems)
}

/// [`union_eightbyte_elems`] for a union layout, as a libffi type. `None`
/// keeps the byte struct.
fn union_sysv_eightbyte_ffi_type(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    total: usize,
    depth: usize,
) -> Option<FfiType> {
    if total > 16 {
        return None;
    }
    let mut leaves = Vec::new();
    if !collect_layout_leaves(ctx, layout, 0, depth, &mut leaves) {
        return None;
    }
    let classes = sysv_eightbyte_classes(total, &leaves)?;
    let elems = union_eightbyte_elems(total, layout_align(ctx, layout), &classes)?;
    Some(FfiType::structure(elems.into_iter().map(|e| match e {
        UnionElem::Byte => FfiType::u8(),
        UnionElem::F32 => FfiType::f32(),
        UnionElem::F64 => FfiType::f64(),
    })))
}

// ---------------------------------------------------------------------------
// A by-value union's AAPCS64 HFA classification (round 13 wave 13, lane ffm7)
// ---------------------------------------------------------------------------
//
// `r13w12-misc12-aarch64-union-hfa-travels-in-general-registers`. AAPCS64
// makes a composite whose fundamental members are all `float` (or all
// `double`), one to four of them, a Homogeneous Floating-point Aggregate
// passed and returned in `s0..s3` / `d0..d3`. For a union the member count is
// the largest member's (GCC `aapcs_vfp_sub_candidate`, `UNION_TYPE`), and the
// union's size must be exactly that many base elements. libffi decides
// HFA-ness from the element types, so such a union is rebuilt as
// `struct { float x N }` / `struct { double x N }`; anything else keeps the
// byte struct. Compiled and unit-tested on every target, used on AArch64 only.

/// The HFA element and count of a `total`-byte union whose scalar leaves are
/// `leaves` (`collect_layout_leaves`), or `None` when it is not one: a
/// non-floating or mixed-width leaf, a leaf off its base-size grid, a size
/// that is not one to four base elements, or a base slot no member covers
/// (C would count fewer elements than the size holds).
fn union_hfa_elems(total: usize, leaves: &[(usize, usize, bool)]) -> Option<(UnionElem, usize)> {
    let &(_, base, _) = leaves.first()?;
    if base != 4 && base != 8 {
        return None;
    }
    if leaves.iter().any(|&(offset, size, is_float)| {
        !is_float || size != base || offset % base != 0 || offset + size > total
    }) {
        return None;
    }
    if total % base != 0 {
        return None;
    }
    let count = total / base;
    if !(1..=4).contains(&count) {
        return None;
    }
    let mut covered = [false; 4];
    for &(offset, _, _) in leaves {
        covered[offset / base] = true;
    }
    if !covered[..count].iter().all(|slot| *slot) {
        return None;
    }
    Some((
        if base == 8 {
            UnionElem::F64
        } else {
            UnionElem::F32
        },
        count,
    ))
}

/// [`union_hfa_elems`] for a union layout, as a libffi type. `None` keeps the
/// byte struct.
fn union_aapcs64_hfa_ffi_type(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    total: usize,
    depth: usize,
) -> Option<FfiType> {
    if total > 32 {
        return None;
    }
    let mut leaves = Vec::new();
    if !collect_layout_leaves(ctx, layout, 0, depth, &mut leaves) {
        return None;
    }
    let (elem, count) = union_hfa_elems(total, &leaves)?;
    Some(FfiType::structure((0..count).map(|_| match elem {
        UnionElem::F64 => FfiType::f64(),
        _ => FfiType::f32(),
    })))
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
///
/// `translated` is the `Unsafe.allocateMemory` arena handle whose real
/// storage this slot passes (a pointer argument over an `Arena` segment), or
/// `0`. The translation is given back when the slot drops -- after the C call,
/// or on the refusal that abandons it -- so a block passed to native code is
/// not left "translated" for its `free` to retain
/// (`FFM-door-seg-probe-residuals-FIXED-20261007.md`).
pub struct ArgSlot {
    pub bytes: Vec<u8>,
    pub translated: i64,
    /// With `translated`: the bytes of the block from the pointer passed to
    /// its end ([`ArgSlot::handle_for`]).
    pub span: usize,
}

impl ArgSlot {
    /// The arena handle of `addr`, a machine address the callee returned,
    /// when it lies in the `Arena` block this slot passed: from the pointer
    /// passed to the block's end (one past it included). `None` otherwise.
    ///
    /// The Java side of this VM knows that block only by its handle, so a
    /// pointer into it handed back by C (`strchr(seg, c)`, `strstr`,
    /// `memchr`) must come back as the handle plus the same offset, or
    /// `at.address() - seg.address()` is the distance between a machine
    /// address and a tagged handle (`R18FfmcostShapes` `ptr-return sum`;
    /// HotSpot: the offset).
    pub fn handle_for(&self, addr: i64) -> Option<i64> {
        if self.translated == 0 {
            return None;
        }
        let real = usize::from_ne_bytes(
            self.bytes
                .get(..std::mem::size_of::<usize>())?
                .try_into()
                .ok()?,
        );
        let off = usize::try_from(addr).ok()?.checked_sub(real)?;
        (off <= self.span).then(|| self.translated.wrapping_add(off as i64))
    }
}

/// An `Arena` block's handle resolved to its storage for a pointer argument:
/// the machine address and the bytes from it to the block's end, or `None`
/// (a freed handle, or a block shorter than `want`). On `Some` the caller owns
/// one translation of the block, given back by `ArgSlot`'s drop.
fn resolve_arena_pointer(handle: i64, want: i64) -> Option<(i64, usize)> {
    let want = usize::try_from(want.max(1)).unwrap_or(usize::MAX);
    let (ptr, remaining) = crate::unsafe_natives_ext::unsafe_arena_real_ptr(handle)?;
    if remaining < want {
        crate::unsafe_natives_ext::unsafe_arena_release_translation(handle);
        return None;
    }
    Some((ptr as i64, remaining))
}

impl Drop for ArgSlot {
    fn drop(&mut self) {
        if self.translated != 0 {
            crate::unsafe_natives_ext::unsafe_arena_release_translation(self.translated);
        }
    }
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
    let mk = |bytes: Vec<u8>| {
        Ok(ArgSlot {
            bytes,
            translated: 0,
            span: 0,
        })
    };
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
            //
            // `crate::panama::is_alias_heap_segment` covers both H1 (a real
            // `HeapMemorySegmentImpl`) and H2 (this VM's `ofArray` ALIAS
            // carrier for all seven `ofArray` element types; H3, the retired
            // native-mirror carrier, is gone). The int[]/long[]/float[]/
            // double[] ones reach here too unless `pe_downcall_invoke_general`
            // stood a scratch buffer in first (`critical(true)`, or the opt-in
            // Elasticsearch carve-out `CRATONVM_COMPAT_FFM_OFARRAY_HEAP_DOWNCALL_CARVEOUT`).
            // Until this check widened past H1 alone, an H2 segment fell through to
            // `segment_address` (0, a legitimate null) and the call silently
            // received `NULL` instead of a refusal.
            //
            // The exception TYPE and wording match the oracle exactly, not
            // just the intent — MEASURED on HotSpot 25.0.3+9-LTS
            // (`FfmRepro2.java`): both a byte[] and an int[] heap segment
            // handed straight to a downcall throw
            // `IllegalArgumentException: Heap segment not allowed: ...`, the
            // same exception `panama.rs`'s `set(ADDRESS, …)` refusal already
            // raises for the identical reason. j27 wave 4 (lane FFMFLIP): the
            // message is the segment's own `toString()` text, as HotSpot's
            // (`panama::heap_segment_not_allowed`); it was
            // `MemorySegment{ kind: heap, class: <carrier class> }`.
            if let Value::Object(Some(seg)) = arg {
                if crate::panama::is_alias_heap_segment(ctx, *seg) {
                    return Err(crate::panama::heap_segment_not_allowed(ctx, *seg));
                }
            }
            // A zero-length segment (`MemorySegment.ofAddress(x)`) is an opaque
            // pointer VALUE -- a Windows HANDLE, a `void*` cookie -- that the
            // callee need not dereference, and HotSpot passes it unchanged
            // (`WaitForSingleObject(h)` with `h == 0x1dc`). Only a sized
            // segment's base can be the size-for-base carrier mix-up the low
            // window screen exists for.
            // Round 14 wave 1 (lane ffm): a Java `null` pointer argument is
            // `NullPointerException` (JDK 25's `SharedUtils.unboxSegment`);
            // `MemorySegment.NULL` is the C null. It was passed as 0.
            // Message-less, as MEASURED on HotSpot 25 (round 14 wave 5
            // hand-back, `R14Fixup2AddressNullMessage`: `downcall .. NPE null`).
            if matches!(arg, Value::Object(None)) && crate::panama::ffm_null_address_npe() {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            let (v, opaque) = match arg {
                Value::Object(Some(seg)) => (
                    segment_address(ctx, *seg),
                    segment_byte_size(ctx, *seg) == 0,
                ),
                Value::Long(n) => (*n, true),
                Value::Int(n) => (*n as i64, true),
                _ => (0, false),
            };
            // The CALLEE dereferences this one, so a bad value faults inside
            // foreign code where we can neither catch nor report it. Screen it
            // here, where a refusal is still a Java exception. Null passes: it
            // is a legitimate C argument.
            // An `Arena` block's storage, resolved below, is handed back when
            // the slot drops (`ArgSlot::translated`).
            let mut translated = 0i64;
            let mut span = 0usize;
            let v = if opaque && opaque_low_pointer_args_allowed() {
                // A zero-length segment over an `Arena` block
                // (`MemorySegment.ofAddress(arenaSegment.address())`) carries
                // the block's HANDLE as its address on this VM; HotSpot passes
                // that block's address, so the callee gets the storage
                // (`R16FfmsegBind` `zero-length-of-address`: `strlen` of
                // `"hello"`, 5, where this refused with the arena-handle
                // `IllegalStateException`). A freed or foreign handle keeps
                // the refusal.
                let v = if is_arena_handle(v) {
                    match resolve_arena_pointer(v, 1) {
                        None => v,
                        Some((real, rest)) => {
                            translated = v;
                            span = rest;
                            // Round 19: also for this call's pointer returns
                            // and upcall arguments (`arena_call_pointer_to_handle`).
                            let _ = arena_call_address(v, 0);
                            real
                        }
                    }
                } else {
                    v
                };
                checked_foreign_handle_tag(v, "downcall pointer argument")?
            } else {
                // SBR3 ffmperf: a SIZED segment over an `Unsafe.allocateMemory`
                // arena block (every allocation of the JDK's own `Arena` on this
                // VM, `Arena.allocateFrom("..")` included) is passed as the
                // block's REAL storage, which lives as long as the segment's
                // session does, and the session is acquired for the call
                // (round 19: `arena_call_address`, recorded for the call so a
                // pointer the C function hands back maps to its handle; a
                // freed or short block keeps today's refusal below).
                let v = match arg {
                    Value::Object(Some(seg)) if is_arena_handle(v) => {
                        match resolve_arena_pointer(v, segment_byte_size(ctx, *seg)) {
                            None => v,
                            Some((real, rest)) => {
                                translated = v;
                                span = rest;
                                // Round 19: see the arm above.
                                let _ = arena_call_address(v, 0);
                                real
                            }
                        }
                    }
                    _ => v,
                };
                checked_foreign_addr(v, "downcall pointer argument")?
            };
            // Address is sizeof(usize); use ptr layout.
            Ok(ArgSlot {
                bytes: (v as usize).to_ne_bytes().to_vec(),
                translated,
                span,
            })
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
            // SBR3 ffmperf: an arena-handle base resolves to the block's storage
            // (see the pointer arm above).
            // The translation is given back once the bytes are copied (by
            // `ArgSlot`'s drop on a refusal or after the copy below).
            let mut held = ArgSlot {
                bytes: Vec::new(),
                translated: 0,
                span: 0,
            };
            let addr = if is_arena_handle(addr) {
                match host_base_address(addr, total as i64) {
                    0 => addr,
                    real => {
                        held.translated = addr;
                        real
                    }
                }
            } else {
                addr
            };
            let addr = checked_foreign_addr(addr, "struct-by-value MemorySegment base")?;
            let mut bytes = vec![0u8; total];
            // SAFETY: `addr` was sourced from a live MemorySegment field.
            // The segment's owning Arena keeps the backing memory valid
            // for the duration of this downcall (Arena lifetime > call).
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, bytes.as_mut_ptr(), total);
            }
            drop(held);
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

/// JIT round 16 wave 11 (lane ffmcost): [`marshal_arg`]'s conversion for a
/// primitive VALUE kind, written into one 8-byte, 8-aligned cell instead of a
/// fresh `Vec` -- the per-argument work of `panama.rs`'s plain-downcall memo
/// path (`CRATONVM_FFM_DOWNCALL_PLAIN_MEMO`). The conversions are
/// `marshal_arg`'s arms, kind for kind; the unit test
/// `plain_cell_matches_marshal_arg` holds the two together. libffi reads the
/// type's own width from the start of the cell, as it reads an [`ArgSlot`].
///
/// `false`, cell untouched, for a kind it does not cover: `ADDRESS` (its
/// screens and segment reads stay in `marshal_arg`) and every group.
#[inline]
pub fn marshal_primitive_into(kind: i32, arg: &Value, cell: &mut u64) -> bool {
    let mut bits = [0u8; 8];
    match kind {
        LAYOUT_BYTE | LAYOUT_BOOLEAN => {
            bits[..1].copy_from_slice(&(arg.as_int().unwrap_or(0) as i8).to_ne_bytes())
        }
        LAYOUT_SHORT | LAYOUT_CHAR => {
            bits[..2].copy_from_slice(&(arg.as_int().unwrap_or(0) as i16).to_ne_bytes())
        }
        LAYOUT_INT => bits[..4].copy_from_slice(&arg.as_int().unwrap_or(0).to_ne_bytes()),
        LAYOUT_LONG => {
            let v = match arg {
                Value::Long(n) => *n,
                Value::Int(n) => *n as i64,
                _ => 0,
            };
            bits.copy_from_slice(&v.to_ne_bytes())
        }
        LAYOUT_FLOAT => {
            let v = match arg {
                Value::Float(f) => *f,
                Value::Int(n) => *n as f32,
                _ => 0.0,
            };
            bits[..4].copy_from_slice(&v.to_ne_bytes())
        }
        LAYOUT_DOUBLE => {
            let v = match arg {
                Value::Double(d) => *d,
                Value::Float(f) => *f as f64,
                Value::Int(n) => *n as f64,
                Value::Long(n) => *n as f64,
                _ => 0.0,
            };
            bits.copy_from_slice(&v.to_ne_bytes())
        }
        _ => return false,
    }
    // The cell's memory is these eight bytes in order, so the value sits at
    // offset 0 on either byte order, where libffi reads it.
    *cell = u64::from_ne_bytes(bits);
    true
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

/// Un-bias a REAL heap segment's raw `offset` field into a byte offset within
/// the backing array's DATA — i.e. `MemorySegment.address()`, not the field
/// itself, which carries `Unsafe.arrayBaseOffset` baked in.
///
/// `HeapMemorySegmentImpl.offset` is `Unsafe`-style: this VM publishes
/// [`cratonvm_native_io::ARRAY_BYTE_BASE_OFFSET`] as that bias for every
/// primitive array type (measured 16 on the oracle too, F35-1 §4.2), so a
/// fresh full-array segment's `offset` is 16, not 0.
///
/// One constant for a subtraction that used to be written three times: this
/// exact rule, via a local alias of the same constant, once in
/// `panama.rs::heap_segment_view` (as `HEAP_ARRAY_BASE_OFFSET`) and once in
/// `lang_invoke.rs::segment_raw_access` as its own literal
/// `const ABASE: i64 = 16` (NOM F35-2). Each call site still resolves the
/// `offset` field itself, by whichever fallback its carrier needs — only the
/// bias is centralised here.
pub fn heap_segment_byte_start(raw_offset: i64) -> i64 {
    raw_offset - cratonvm_native_io::ARRAY_BYTE_BASE_OFFSET
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

/// Whether `addr` is an `Unsafe.allocateMemory` arena handle: bit 62 set and
/// positive (a negative value, e.g. `-1`, is a bogus pointer and is NOT a
/// handle; the arithmetic checks of the callers reject it).
pub(crate) fn is_arena_handle(addr: i64) -> bool {
    addr > 0 && addr & ARENA_HANDLE_TAG != 0
}

// ---------------------------------------------------------------------------
// Arena handles across one downcall (`r16-ffm-plain-downcall-costs-microseconds`,
// round 19)
// ---------------------------------------------------------------------------
//
// Under `--jdk-only` every `Arena` segment is the JDK's own
// `NativeMemorySegmentImpl` over `Unsafe.allocateMemory`, whose addresses on
// this VM are tagged arena HANDLES (`ARENA_HANDLE_TAG`), not machine
// addresses. A downcall passes C the block's real storage
// (`arena_call_address`); for the length of the call each such block is
// recorded on this thread, so that a pointer the C side hands BACK -- a
// returned `char *` into the argument (`strchr`), the element pointers
// `qsort` passes its comparator -- names the same bytes as a handle again
// (`arena_call_pointer_to_handle`), exactly as `MemorySegment.address()`
// names them to Java. Without it Java saw two address spaces at once: the
// handle it allocated and the raw pointer C answered (`strchr`'s offset came
// out as `-4611683887557756134`).

thread_local! {
    /// `(real base, length, handle base)` of every arena block passed to the
    /// downcalls this thread is inside, innermost last.
    static ARENA_CALL_BLOCKS: std::cell::RefCell<Vec<(usize, usize, i64)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// How many [`ArenaCallScope`]s are open on this thread. A block is
    /// recorded only inside one: a translation outside any call (an argument
    /// screen, an upcall's own conversion) used to stay in the table for the
    /// thread's life, and later named a freed block's memory by the dead
    /// handle (`ffm-address-read-back-of-an-arena-pointer-is-the-real-address-not-the-handle`).
    static ARENA_CALL_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One downcall's span of [`ARENA_CALL_BLOCKS`]: opened before the arguments
/// are marshalled, dropped after the result is built, so nested downcalls (an
/// up-call making its own) keep their own blocks and the outer call's survive
/// them.
pub(crate) struct ArenaCallScope {
    mark: usize,
}

impl ArenaCallScope {
    pub(crate) fn open() -> Self {
        ARENA_CALL_DEPTH.with(|d| d.set(d.get() + 1));
        Self {
            mark: ARENA_CALL_BLOCKS.with(|b| b.borrow().len()),
        }
    }
}

impl Drop for ArenaCallScope {
    fn drop(&mut self) {
        ARENA_CALL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        ARENA_CALL_BLOCKS.with(|b| {
            if let Ok(mut b) = b.try_borrow_mut() {
                b.truncate(self.mark);
            }
        });
    }
}

/// The real address a downcall passes for the arena handle `handle` of a
/// segment of `size` bytes, recorded for the call (see above). `None` for an
/// untagged address, a freed handle or a block shorter than `size` (the
/// caller then keeps its refusal). The caller holds the segment's session for
/// the call (or the segment has none), so the block lives until it returns.
pub(crate) fn arena_call_address(handle: i64, size: i64) -> Option<i64> {
    arena_call_address_by(handle, size, false)
}

/// [`arena_call_address`]; `table_first` asks the arena's JIT-readable table
/// before its locked map (`unsafe_arena_peek_table_first`, the same answer).
pub(crate) fn arena_call_address_by(handle: i64, size: i64, table_first: bool) -> Option<i64> {
    let want = usize::try_from(size.max(0)).ok()?;
    let (ptr, real, len, base) = if table_first {
        crate::unsafe_natives_ext::unsafe_arena_peek_table_first(handle, want)?
    } else {
        crate::unsafe_natives_ext::unsafe_arena_peek(handle, want)?
    };
    if ARENA_CALL_DEPTH.with(std::cell::Cell::get) == 0 {
        return Some(ptr as i64);
    }
    ARENA_CALL_BLOCKS.with(|b| {
        if let Ok(mut b) = b.try_borrow_mut() {
            if !b.iter().any(|e| e.0 == real) {
                b.push((real, len, base));
            }
        }
    });
    Some(ptr as i64)
}

/// A pointer C handed back, as Java names the same bytes: inside (or one past
/// the end of) an arena block the enclosing downcalls passed, or any other live
/// arena block, its handle; anything else unchanged.
///
/// The second half (`ffm-address-read-back-of-an-arena-pointer-is-the-real-address-not-the-handle`)
/// is what a pointer READ BACK out of memory needs: `slot.set(ADDRESS, 0, s)`
/// stores `s`'s block's real address so that C can read it
/// ([`address_for_native_store`]), and `slot.get(ADDRESS, 0).address()` must be
/// `s.address()` again, as on HotSpot -- also when no downcall is in flight.
/// The two read doors (`Utils.longToAddress`, the VM-built `ADDRESS` reads)
/// and every pointer C answers come through here.
pub(crate) fn arena_call_pointer_to_handle(raw: i64) -> i64 {
    if raw <= 0 || is_arena_handle(raw) {
        return raw;
    }
    let at = raw as usize;
    let in_call = ARENA_CALL_BLOCKS.with(|b| {
        let b = b.try_borrow().ok()?;
        b.iter()
            .rev()
            .find(|&&(real, len, _)| at >= real && at - real <= len)
            .map(|&(real, _, base)| base + (at - real) as i64)
    });
    in_call
        .or_else(|| crate::unsafe_natives_ext::unsafe_arena_handle_of_real(at))
        .unwrap_or(raw)
}

/// The value a Java store of a `MemorySegment` into an `ADDRESS` slot puts in memory.
///
/// `MemorySegment.address()` names an `Unsafe.allocateMemory` block by its arena HANDLE, which
/// is what Java must keep seeing, but the 8 bytes stored into a segment are read later by C as
/// a pointer: `Arena.allocateFrom(ADDRESS, seg)` / `set(ADDRESS, off, seg)` build the
/// `const unsigned char **` that `d2i_X509` and `strsep` take. The handle is a tagged
/// non-canonical value there (a general-protection fault in the callee), so a live block's
/// handle is stored as the block's real address, the same translation a downcall's pointer
/// argument gets. A freed handle, an untagged address and `0` are stored as they are. The
/// block's `Vec` does not move while the arena lives (`reallocate` retains the displaced
/// buffer), so the stored pointer stays good for the segment's lifetime, as on HotSpot.
pub(crate) fn address_for_native_store(addr: i64) -> i64 {
    if !is_arena_handle(addr) {
        return addr;
    }
    crate::unsafe_natives_ext::unsafe_arena_peek(addr, 0).map_or(addr, |(ptr, _, _, _)| ptr as i64)
}

/// Turn a segment's BASE address into one this process can dereference.
///
/// A segment minted over a direct `ByteBuffer` (`MemorySegment.ofBuffer`, or
/// `Buffer.address`) reports the buffer's address, which on this VM is a tagged
/// `Unsafe.allocateMemory` arena handle ([`ARENA_HANDLE_TAG`]) and not a machine
/// address: handing it to `ptr::read` is a fault on a non-canonical address
/// (Windows reports it as `read at 0xFFFFFFFFFFFFFFFF`). The raw-access sites
/// of `MemorySegment` call this on the base they are about to dereference; the
/// address the Java side SEES (`MemorySegment.address()`, slices, an
/// `asByteBuffer` view) stays the handle.
///
/// An untagged address is returned as is. A tagged one resolves to the block's
/// own storage when the live block covers `size` bytes from the handle, and to
/// `0` otherwise (a freed handle, a block shorter than the segment), which every
/// caller already refuses as "no address" instead of faulting.
pub(crate) fn host_base_address(base: i64, size: i64) -> i64 {
    if !is_arena_handle(base) {
        return base;
    }
    let want = usize::try_from(size.max(1)).unwrap_or(usize::MAX);
    match crate::unsafe_natives_ext::unsafe_arena_real_ptr_bounded(base, want) {
        Some(ptr) => ptr as i64,
        None => 0,
    }
}

/// Resolve an arena-backed MemorySegment for an in-VM access that already
/// holds its scope's access window (or the enclosing downcall's acquired
/// session). Unlike [`host_base_address`], this is a borrow, not a native
/// pointer publication: the caller uses the address synchronously and must
/// not retain it after that window/session ends. Recording a long-lived
/// translation here leaks one count per load/store; an upcall such as qsort's
/// comparator can otherwise leave thousands of stale translations on its
/// array block when the arena closes.
#[inline]
pub(crate) fn borrowed_host_base_address(base: i64, size: i64) -> i64 {
    if !is_arena_handle(base) {
        return base;
    }
    let want = usize::try_from(size.max(1)).unwrap_or(usize::MAX);
    crate::unsafe_natives_ext::unsafe_arena_peek(base, want).map_or(0, |(ptr, _, _, _)| ptr as i64)
}

/// [`segment_address`] resolved by [`host_base_address`]: the base a raw
/// load/store may use. Segment methods hold an `AccessWindow` after checking
/// the scope, and an upcall's enclosing downcall holds the segment session, so
/// this address is borrowed only for the synchronous access below that window.
pub fn segment_host_address(ctx: &dyn NativeContext, seg: ObjectRef) -> i64 {
    borrowed_host_base_address(segment_address(ctx, seg), segment_byte_size(ctx, seg))
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

/// `CRATONVM_FFM_CHAR_RETURN_UNSIGNED` (default on): a downcall's `JAVA_CHAR`
/// return is zero-extended, as a Java `char` is unsigned. Off restores the
/// historical sign extension. Cached the way `panama.rs` caches its switches:
/// a downcall can return `char` in a hot loop.
pub(crate) fn char_return_unsigned() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_CHAR_RETURN_UNSIGNED")
    })
}

/// `CRATONVM_FFM_CHAR_ARG_UNSIGNED` (default on; JIT round 22, lane memory): a
/// downcall's `JAVA_CHAR` ARGUMENT is zero-extended into its register -- the
/// libffi type is `uint16` ([`primitive_kind_to_ffi_type`]) and the direct
/// stubs widen it so ([`direct_stub_widen`]) -- as HotSpot passes a Java
/// `char`, which is unsigned. Off restores the historical `sint16`. Read once.
pub fn char_arg_unsigned() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_CHAR_ARG_UNSIGNED")
    })
}

/// `CRATONVM_FFM_GROUP_MEMBERS_BY_NAME` (default on; round 13 wave 5, lane
/// ffm3): a by-value struct's members and a sequence's element layout are read
/// through `foreign_ffm`'s by-name layout slots, which understand the REAL
/// layout classes. Off restores the fixed slot-2 read (fabricated carriers
/// only).
fn group_members_by_name() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_GROUP_MEMBERS_BY_NAME")
    })
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
        // A Java `char` is UNSIGNED: sign-extending it answered `(char) -2`
        // (`'￾'`, 65534 on HotSpot) as -2 for a `JAVA_CHAR` return. Round
        // 12 wave 7 (lane upcall); `CRATONVM_FFM_CHAR_RETURN_UNSIGNED=0` restores
        // the sign extension.
        LAYOUT_CHAR if char_return_unsigned() => {
            let widened = usize::from_ne_bytes(read::<{ std::mem::size_of::<usize>() }>(slot));
            Value::Int(i32::from(widened as u16))
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

// ---------------------------------------------------------------------------
// The take-over signal masked around a counted C call (JIT round 17 wave 2,
// lane ffm3; proposal EINTR17-1, `CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL`)
// ---------------------------------------------------------------------------
//
// The Linux take-over (`vm/src/jit/xt_root_scan.rs`) reads
// `/proc/self/task/<tid>/syscall` and does not signal a peer blocked in a
// system call outside compiled code. A peer the read saw RUNNING in C can
// still enter a blocking syscall (`usleep`, `poll`) before the `SIGUSR2`
// lands, and the syscall then fails with `EINTR`
// (`r17-takeover-signal-residual-eintr-windows-20261005`, section 1).
// EINTR17-1 proposed a CAS handshake on a per-thread word; the kernel's own
// signal mask is that handshake, with no word to publish: a counted downcall
// blocks the take-over signal for exactly the C call. A blocked signal stays
// pending and interrupts nothing (`signal_pending` ignores blocked signals),
// so no syscall in C sees `EINTR` from the VM, whenever the signal was sent;
// it is delivered when the mask is restored after the call, in Rust, where
// the handler answers "not in compiled code" (or finds its slot cancelled).
// The take-over reads the mask (`SigBlk` of `/proc/self/task/<tid>/status`)
// and treats a peer that blocks the signal as one in C: not signalled, or,
// signalled in the race, given up on without waiting out its deadline. An
// up-call unblocks it for the Java it runs (`panama_upcall::upcall_entry`).
// Linux x86-64 only (the only signal-based take-over); elsewhere a no-op.

/// `CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL` (default OFF, opt-in: two
/// `rt_sigprocmask` calls per counted downcall; JIT round 17 wave 2, lane
/// ffm3, EINTR17-1). Read once.
pub fn ffm_downcall_mask_takeover_signal() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL")
    })
}

/// FFM417-5 (j27 wave 4, lane FFMROWS): whether a downcall whose region entry
/// left it in `region` runs C with the take-over signal masked. Decided from
/// the region actually ENTERED: a lazy entry the context declines with no
/// fallback leaves the call counted, and a counted call is what the mask is
/// for (`switch_on`: [`ffm_downcall_mask_takeover_signal`]).
pub fn downcall_masks_takeover(region: DowncallRegion, switch_on: bool) -> bool {
    switch_on && !region.is_open()
}

/// [`downcall_masks_takeover`] for this thread's innermost downcall, right
/// after its region entry.
pub fn downcall_takeover_mask_now() -> bool {
    downcall_masks_takeover(active_downcall_region(), ffm_downcall_mask_takeover_signal())
}

/// The take-over's signal (`xt_root_scan::imp::TAKEOVER_SIGNAL`).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const TAKEOVER_SIGNAL: i32 = libc::SIGUSR2;

/// A change of this thread's mask for the take-over signal, undone on drop
/// (the previous mask restored whole, so nesting is exact).
pub struct TakeoverSignalMask {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    old: Option<libc::sigset_t>,
}

impl TakeoverSignalMask {
    /// Block the take-over signal when `on` (a counted downcall's C call).
    #[inline]
    pub fn block(on: bool) -> Self {
        Self::change(on, true)
    }

    /// Unblock it when `on` (an up-call's Java, inside a masked call).
    #[inline]
    pub fn unblock(on: bool) -> Self {
        Self::change(on, false)
    }

    #[inline]
    #[allow(unused_variables)]
    fn change(on: bool, block: bool) -> Self {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            if on {
                // SAFETY: `sigset_t` is plain data, initialised by
                // `sigemptyset`; `pthread_sigmask` changes only this thread's
                // mask and writes the old one into `old`.
                unsafe {
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, TAKEOVER_SIGNAL);
                    let mut old: libc::sigset_t = std::mem::zeroed();
                    let how = if block {
                        libc::SIG_BLOCK
                    } else {
                        libc::SIG_UNBLOCK
                    };
                    if libc::pthread_sigmask(how, &set, &mut old) == 0 {
                        return Self { old: Some(old) };
                    }
                }
            }
            Self { old: None }
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            Self {}
        }
    }
}

impl Drop for TakeoverSignalMask {
    #[inline]
    fn drop(&mut self) {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            if let Some(old) = self.old.take() {
                // SAFETY: restores the mask `change` read; `pthread_sigmask`
                // reports failure in its result, never in `errno`, so the C
                // call's `errno` survives (a pending take-over signal's
                // handler, which runs as the mask drops, answers with plain
                // stores, and its one parking syscall puts `errno` back).
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Direct call stubs (JIT round 16 wave 16, lane ffmlean; proposals FFS16-4 /
// FFD16-1): a fixed-arity call in place of the generic `ffi_call`
// ---------------------------------------------------------------------------
//
// `ffi_call` re-classifies the CIF's every argument on each call
// (`ffi_call_int`: a type switch per argument into a register image, then the
// `ffi_call_unix64` assembly shuffle). For a non-variadic C function whose
// parameters and return are all INTEGER class -- the integral layouts and
// `ADDRESS` -- and that takes at most six of them, the x86-64 C conventions
// put every argument in a general-purpose register (SysV: rdi, rsi, rdx, rcx,
// r8, r9; Win64: the first four in registers, the rest on the stack, which
// the Rust caller lays out just as libffi does) and the result in rax. A Rust
// call through an `extern "C" fn(u64, ..) -> u64` of the same arity puts the
// same bits in the same places. The two things libffi adds are reproduced
// here: each argument widened to the full register the way libffi widens it
// ([`direct_stub_widen`]), and the result read back by the caller through
// [`unmarshal_return_primitive`], which reads only the kind's own low bytes
// (the upper bits of rax are undefined for a narrow C return, as they are in
// libffi's widened slot's interpretation).

/// The most arguments a direct stub passes: SysV x86-64's six integer
/// argument registers.
pub const DIRECT_STUB_MAX_ARGS: usize = 6;

/// Is `kind` an INTEGER-class layout a direct stub passes in a general
/// register and returns in rax: the integral value kinds and `ADDRESS`? No
/// float or double (those go in xmm registers) and no group.
pub fn direct_stub_kind_ok(kind: i32) -> bool {
    matches!(
        kind,
        LAYOUT_BYTE
            | LAYOUT_BOOLEAN
            | LAYOUT_SHORT
            | LAYOUT_CHAR
            | LAYOUT_INT
            | LAYOUT_LONG
            | LAYOUT_ADDRESS
    )
}

/// Widen one argument cell (the value at its low bytes, as
/// [`marshal_primitive_into`] or a compiled caller's slot holds it) to the
/// full 64-bit register image libffi's x86-64 `ffi_call` would load for the
/// CIF type [`primitive_kind_to_ffi_type`] gives `kind`: `sint8` for `BYTE` and
/// `BOOLEAN`, `sint16` for `SHORT`, `sint32` for `INT`, each sign-extended;
/// `uint16` for `CHAR`, zero-extended (`sint16` under
/// `CRATONVM_FFM_CHAR_ARG_UNSIGNED=0`); `sint64` and `pointer` as they are.
#[inline(always)]
pub fn direct_stub_widen(kind: i32, cell: u64) -> u64 {
    match kind {
        LAYOUT_BYTE | LAYOUT_BOOLEAN => cell as u8 as i8 as i64 as u64,
        LAYOUT_CHAR if char_arg_unsigned() => u64::from(cell as u16),
        LAYOUT_SHORT | LAYOUT_CHAR => cell as u16 as i16 as i64 as u64,
        LAYOUT_INT => cell as u32 as i32 as i64 as u64,
        _ => cell,
    }
}

/// Call the C function at `fn_addr` with the first `arity` of `regs` (each
/// already [`direct_stub_widen`]ed) and answer rax, or `None` -- nothing
/// called -- for an arity above [`DIRECT_STUB_MAX_ARGS`] or a target other
/// than x86-64 (aarch64 is out of this stub's scope; its callers keep
/// `ffi_call`).
///
/// # Safety
///
/// `fn_addr` must be a callable, NON-variadic C function of the platform's
/// default C ABI whose `arity` parameters and return are all integer class
/// ([`direct_stub_kind_ok`]) -- the shape the caller's CIF was built for; the
/// callee's own obligations (pointer arguments live for the call, and so on)
/// are the caller's, as for `ffi_call`. A `void` function may be called this
/// way: rax is then garbage the caller does not read.
#[inline(always)]
#[allow(unreachable_code)]
pub unsafe fn direct_stub_call(
    fn_addr: usize,
    arity: usize,
    regs: &[u64; DIRECT_STUB_MAX_ARGS],
) -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        type F0 = unsafe extern "C" fn() -> u64;
        type F1 = unsafe extern "C" fn(u64) -> u64;
        type F2 = unsafe extern "C" fn(u64, u64) -> u64;
        type F3 = unsafe extern "C" fn(u64, u64, u64) -> u64;
        type F4 = unsafe extern "C" fn(u64, u64, u64, u64) -> u64;
        type F5 = unsafe extern "C" fn(u64, u64, u64, u64, u64) -> u64;
        type F6 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64;
        let r = regs;
        // SAFETY: the caller's contract above; `fn_addr` is a code address of
        // the arity matched, so each transmute names its real ABI shape.
        return Some(unsafe {
            match arity {
                0 => std::mem::transmute::<usize, F0>(fn_addr)(),
                1 => std::mem::transmute::<usize, F1>(fn_addr)(r[0]),
                2 => std::mem::transmute::<usize, F2>(fn_addr)(r[0], r[1]),
                3 => std::mem::transmute::<usize, F3>(fn_addr)(r[0], r[1], r[2]),
                4 => std::mem::transmute::<usize, F4>(fn_addr)(r[0], r[1], r[2], r[3]),
                5 => std::mem::transmute::<usize, F5>(fn_addr)(r[0], r[1], r[2], r[3], r[4]),
                6 => std::mem::transmute::<usize, F6>(fn_addr)(r[0], r[1], r[2], r[3], r[4], r[5]),
                _ => return None,
            }
        });
    }
    let _ = (fn_addr, arity, regs);
    None
}

/// The two general return registers of a SysV x86-64 call, as a C function
/// returning an all-integer struct of 9-16 bytes leaves them (`rax`, `rdx`):
/// a `repr(C)` pair of `u64` is classified INTEGER, INTEGER, so a Rust
/// `extern "C"` call returning it reads exactly those two registers.
#[cfg(all(target_arch = "x86_64", not(windows)))]
#[repr(C)]
#[derive(Clone, Copy)]
struct DirectRegPair {
    rax: u64,
    rdx: u64,
}

/// JIT round 18 wave 11 (lane ffmret; FFMCOST18-3): [`direct_stub_call`] for a
/// C function returning a by-value group in general registers, answering the
/// group's bytes as `[rax, rdx]`. SysV x86-64: up to 16 bytes of an
/// all-integer group come back in `rax` then `rdx` (read through
/// [`DirectRegPair`]). Win64: only a group of exactly 1, 2, 4 or 8 bytes is
/// register-returned, in `rax` alone (larger ones take a hidden pointer, which
/// this stub never passes), so it is [`direct_stub_call`] with `rdx` = 0.
/// `None` -- nothing called -- as [`direct_stub_call`].
///
/// # Safety
///
/// [`direct_stub_call`]'s, with the return a group the target returns in
/// general registers (the caller's classification, from the CIF's prepared
/// return type).
#[inline(always)]
#[allow(unreachable_code)]
pub unsafe fn direct_stub_call_pair(
    fn_addr: usize,
    arity: usize,
    regs: &[u64; DIRECT_STUB_MAX_ARGS],
) -> Option<[u64; 2]> {
    #[cfg(all(target_arch = "x86_64", not(windows)))]
    {
        type P = DirectRegPair;
        type F0 = unsafe extern "C" fn() -> P;
        type F1 = unsafe extern "C" fn(u64) -> P;
        type F2 = unsafe extern "C" fn(u64, u64) -> P;
        type F3 = unsafe extern "C" fn(u64, u64, u64) -> P;
        type F4 = unsafe extern "C" fn(u64, u64, u64, u64) -> P;
        type F5 = unsafe extern "C" fn(u64, u64, u64, u64, u64) -> P;
        type F6 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64) -> P;
        let r = regs;
        // SAFETY: the caller's contract above; `fn_addr` is a code address of
        // the arity matched, returning its group in rax:rdx.
        let pair = unsafe {
            match arity {
                0 => std::mem::transmute::<usize, F0>(fn_addr)(),
                1 => std::mem::transmute::<usize, F1>(fn_addr)(r[0]),
                2 => std::mem::transmute::<usize, F2>(fn_addr)(r[0], r[1]),
                3 => std::mem::transmute::<usize, F3>(fn_addr)(r[0], r[1], r[2]),
                4 => std::mem::transmute::<usize, F4>(fn_addr)(r[0], r[1], r[2], r[3]),
                5 => std::mem::transmute::<usize, F5>(fn_addr)(r[0], r[1], r[2], r[3], r[4]),
                6 => std::mem::transmute::<usize, F6>(fn_addr)(r[0], r[1], r[2], r[3], r[4], r[5]),
                _ => return None,
            }
        };
        return Some([pair.rax, pair.rdx]);
    }
    #[cfg(all(target_arch = "x86_64", windows))]
    {
        // SAFETY: the caller's contract: a 1/2/4/8-byte group in rax.
        return unsafe { direct_stub_call(fn_addr, arity, regs) }.map(|rax| [rax, 0]);
    }
    let _ = (fn_addr, arity, regs);
    None
}

/// Floating-point parameters a [`direct_stub_call_fp`] passes: SysV x86-64's
/// eight vector argument registers, `xmm0`-`xmm7`.
pub const DIRECT_STUB_MAX_FP_ARGS: usize = 8;

/// Is `kind` a FLOAT-class value layout ([`direct_stub_call_fp`] passes it in
/// an `xmm` register and reads its return from `xmm0`)?
pub fn direct_stub_fp_kind(kind: i32) -> bool {
    matches!(kind, LAYOUT_FLOAT | LAYOUT_DOUBLE)
}

/// Does this target have [`direct_stub_call_fp`]? SysV x86-64 only: its
/// integer and vector argument registers are allocated independently, each in
/// parameter order, which is what lets one fixed signature serve every
/// interleaving. Win64 assigns registers by position and stays on libffi.
pub const fn direct_stub_fp_supported() -> bool {
    cfg!(all(target_arch = "x86_64", not(windows)))
}

/// Round 19 (`r16-ffm-plain-downcall-costs-microseconds`): [`direct_stub_call`]
/// for a function with FLOAT-class parameters or return. `ints` are the integer
/// class parameters in order (each [`direct_stub_widen`]ed), `fps` the float
/// and double ones in order (each cell's bits: an `f32` in the low 32 bits, as
/// libffi's cell and the JIT's slot hold it; the register's upper bits are
/// unspecified for a `float` parameter, as in C). Answers rax, or with
/// `ret_fp` the bits of `xmm0` (a `float` return in the low 32). `None` --
/// nothing called -- off SysV x86-64.
///
/// Unused registers carry zeros, which the callee never reads: a SysV callee
/// reads exactly its own parameters' registers, and `al` (the vector register
/// count) matters only to a VARIADIC callee, which this stub never calls.
///
/// # Safety
///
/// [`direct_stub_call`]'s, with at most six integer-class and eight
/// float-class parameters.
#[inline(always)]
#[allow(unreachable_code)]
pub unsafe fn direct_stub_call_fp(
    fn_addr: usize,
    ints: &[u64; DIRECT_STUB_MAX_ARGS],
    fps: &[u64; DIRECT_STUB_MAX_FP_ARGS],
    ret_fp: bool,
) -> Option<u64> {
    #[cfg(all(target_arch = "x86_64", not(windows)))]
    {
        type R = unsafe extern "C" fn(
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
        ) -> u64;
        type X = unsafe extern "C" fn(
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
        ) -> f64;
        let i = ints;
        let f = fps.map(f64::from_bits);
        // The callee's own prototype is narrower than the fixed signature
        // (the ABI makes that sound); hiding the address keeps the optimizer
        // from seeing a known function called through another type, which it
        // may treat as undefined (a test's `extern "C" fn` came back 0).
        let fn_addr = std::hint::black_box(fn_addr);
        // SAFETY: the caller's contract above; the integer arguments land in
        // rdi..r9 and the vector ones in xmm0..xmm7 whatever their
        // interleaving in the C prototype (SysV ABI 3.2.3).
        return Some(unsafe {
            if ret_fp {
                std::mem::transmute::<usize, X>(fn_addr)(
                    i[0], i[1], i[2], i[3], i[4], i[5], f[0], f[1], f[2], f[3], f[4], f[5], f[6],
                    f[7],
                )
                .to_bits()
            } else {
                std::mem::transmute::<usize, R>(fn_addr)(
                    i[0], i[1], i[2], i[3], i[4], i[5], f[0], f[1], f[2], f[3], f[4], f[5], f[6],
                    f[7],
                )
            }
        });
    }
    let _ = (fn_addr, ints, fps, ret_fp);
    None
}

/// Does this target have direct stubs ([`direct_stub_call`])?
pub const fn direct_stubs_supported() -> bool {
    cfg!(target_arch = "x86_64")
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
// (signal handlers, async callbacks, foreign threads) see `None`, and an
// upcall stub then terminates the VM loudly rather than hand C a made-up
// zero (`panama_upcall::upcall_without_context`, round 12 wave 7).
//
// SAFETY: The thread-local is *only* dereferenced inside a closure
// callback, and the surrounding downcall guarantees the pointee
// `NativeContext` outlives the closure. The downcall installs the
// pointer with `set_active_context_for_downcall` and clears it via the
// `_RestoreOnDrop` guard before returning to Java. Cross-thread access
// is impossible because the cell is `thread_local`.

use std::cell::Cell;

// The thread-local holds the fat `*mut dyn NativeContext` itself, as an
// `Option<NonNull<..>>`: `None` is a const initial value, so no non-null fat
// pointer has to be invented for it. (It used to hold a THIN pointer to a
// `Box` carrying the fat one -- one malloc/free pair per downcall; JIT round
// 16 wave 11, lane ffmcost, proposal FFM16-1.) `None` means "no active
// downcall". `dyn NativeContext + 'static` lets the fat raw pointer sit in a
// `Cell` without dragging a borrowed lifetime through the slot.
// SAFETY: the static bound on the trait object is artificial — every
// real installed pointer is bounded by the `ActiveContextGuard`'s
// lifetime, and the `with_active_context` API enforces nesting.
type ActiveContextPtr = Option<std::ptr::NonNull<dyn NativeContext + 'static>>;

thread_local! {
    static ACTIVE_NATIVE_CONTEXT: Cell<ActiveContextPtr> = const { Cell::new(None) };
    /// Whether the innermost active downcall is inside a GC blocking region
    /// (`CRATONVM_FFM_DOWNCALL_GC_SAFE`, round 13 wave 5, lane ffm3): an
    /// upcall must leave it before touching the heap and re-enter it after.
    /// Saved and restored by each `ActiveContextGuard`, so it nests with them.
    /// JIT round 16 wave 9 (lane ffm): a [`DowncallRegion`], so the leave and
    /// the re-entry use the transition the region was opened with.
    static ACTIVE_REGION: Cell<DowncallRegion> = const { Cell::new(DowncallRegion::Counted) };
}

/// The GC state of the innermost active downcall on this thread (JIT round 16
/// wave 9, lane ffm; a `bool` since round 13 wave 5).
///
/// The two transitions must not be mixed: a region opened with
/// `NativeContext::begin_native_region` ends with `end_blocking_region` (its
/// entry published a code-reclamation summary), one opened with
/// `begin_native_region_quiet` ends with `end_native_region_quiet` and the
/// answer of ITS entry (no summary). So the state remembers which, also while
/// an upcall or a JNIEnv function has left the region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DowncallRegion {
    /// No region: the thread is a counted mutator (the flag off, a
    /// `Linker.Option.critical` handle, before and after the C call).
    Counted,
    /// In the full region (`begin_native_region`).
    Open,
    /// In the quiet region; what `begin_native_region_quiet` answered.
    OpenQuiet(u64, u64),
    /// Left an [`Self::Open`] region for an upcall or a JNIEnv function.
    Left,
    /// Left an [`Self::OpenQuiet`] region.
    LeftQuiet,
    /// JIT round 16 wave 14 (lane ffm3): in the LAZY in-native state
    /// (`NativeContext::begin_native_region_lazy`) over the stack band
    /// `[lo, hi)`; a pause claims the thread instead of waiting for it.
    OpenLazy(usize, usize),
    /// Left an [`Self::OpenLazy`] state; the band is kept for the re-entry
    /// (the downcall's own frames, and the spilled registers in them, are
    /// still live).
    LeftLazy(usize, usize),
}

impl DowncallRegion {
    /// Inside a region: a pause does not wait for the thread (it is excluded,
    /// or claimed while in C), so it must not touch the heap.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            DowncallRegion::Open | DowncallRegion::OpenQuiet(..) | DowncallRegion::OpenLazy(..)
        )
    }
}

/// End the region `state` names, if it names an open one.
fn end_region(ctx: &mut dyn NativeContext, state: DowncallRegion) {
    match state {
        DowncallRegion::Open => ctx.end_blocking_region(),
        DowncallRegion::OpenQuiet(generation, folds) => {
            ctx.end_native_region_quiet((generation, folds))
        }
        DowncallRegion::OpenLazy(..) => ctx.end_native_region_lazy(),
        DowncallRegion::Counted
        | DowncallRegion::Left
        | DowncallRegion::LeftQuiet
        | DowncallRegion::LeftLazy(..) => {}
    }
}

/// JIT round 16 wave 14 (lane ffm3): the callee-saved general registers of the
/// downcall's caller chain, stored into the downcall's own frame right before
/// it goes into C lazily (`CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY`).
///
/// A pause that claims the thread scans its stack band conservatively; a
/// reference a compiled (or Rust) caller keeps only in a callee-saved register
/// across the call would be in neither the band (C may keep it in the
/// register, or save it below the band) nor any other root set. Every such
/// register either still holds the caller's value at the capture (it is
/// copied here, inside the band) or was saved by the prologue of a frame
/// between the caller and here (inside the band too). Capture it in the frame
/// that makes the C call, before entering the lazy state, and keep it live
/// until after the leave ([`Self::keep_alive`]).
#[repr(C, align(16))]
pub struct CalleeSavedSpill([usize; 8]);

impl CalleeSavedSpill {
    /// rbx, rbp, rdi, rsi, r12-r15 (the union of the Win64 and SysV
    /// callee-saved general registers) on x86_64; zeros elsewhere, where the
    /// lazy state is never entered (`NativeContextImpl::begin_native_region_lazy`
    /// declines).
    #[inline(always)]
    pub fn capture() -> Self {
        #[cfg_attr(not(target_arch = "x86_64"), allow(unused_mut))]
        let mut spill = CalleeSavedSpill([0; 8]);
        #[cfg(target_arch = "x86_64")]
        // SAFETY: eight stores into this local array, which is 64 bytes long;
        // no register is changed and the stack is not touched.
        unsafe {
            // The pointer in `rax`, caller-saved under both ABIs: whether or
            // not this is inlined, no callee-saved register is borrowed (and
            // its caller's value lost in a frame that is gone) to hold it.
            core::arch::asm!(
                "mov qword ptr [rax], rbx",
                "mov qword ptr [rax + 8], rbp",
                "mov qword ptr [rax + 16], rdi",
                "mov qword ptr [rax + 24], rsi",
                "mov qword ptr [rax + 32], r12",
                "mov qword ptr [rax + 40], r13",
                "mov qword ptr [rax + 48], r14",
                "mov qword ptr [rax + 56], r15",
                in("rax") spill.0.as_mut_ptr(),
                options(nostack, preserves_flags),
            );
        }
        spill
    }

    /// Keep the spill's stack slot live (and unreused) until here: call it
    /// after the leave.
    #[inline(always)]
    pub fn keep_alive(&self) {
        std::hint::black_box(&self.0);
    }
}

thread_local! {
    /// JIT round 16 wave 14 (lane ffm3): this OS thread's stack top, or 0
    /// before the first lazy downcall asks for it (one OS query per thread).
    static LAZY_STACK_TOP: Cell<usize> = const { Cell::new(0) };
}

/// This OS thread's stack top (exclusive high end), or `None` where the OS
/// does not say.
#[cfg(windows)]
fn os_stack_top() -> Option<usize> {
    extern "system" {
        fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    }
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: two out-pointers to locals, the current thread.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    (high > low && low != 0).then_some(high)
}

#[cfg(target_os = "linux")]
fn os_stack_top() -> Option<usize> {
    // SAFETY: the pthread attribute is initialised by `pthread_getattr_np`
    // on success and destroyed after the read.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let rc = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        (rc == 0 && !addr.is_null() && size != 0).then(|| addr as usize + size)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn os_stack_top() -> Option<usize> {
    None
}

/// JIT round 16 wave 14 (lane ffm3): the current native stack pointer, below
/// every local in this frame. On the supported x86-64 targets this is a single
/// register read; other targets retain the conservative local-marker probe.
#[inline(always)]
fn stack_pointer_here() -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        let sp: usize;
        // SAFETY: this only copies the current stack pointer to a general
        // register. It neither changes RSP nor accesses the stack.
        unsafe {
            core::arch::asm!(
                "mov {}, rsp",
                out(reg) sp,
                options(nomem, nostack, preserves_flags),
            );
        }
        sp
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let marker = 0u64;
        std::hint::black_box(&marker) as *const u64 as usize
    }
}

/// JIT round 16 wave 14 (lane ffm3): the stack band a pause scans if it claims
/// this thread in a lazy downcall. RSP is below this frame's locals and every
/// caller frame (including the caller's [`CalleeSavedSpill`]), so the scan
/// remains conservative without a second marker call. `None` when the stack
/// top is unknown or the pointer is invalid; the downcall then stays counted.
fn lazy_band_from_here() -> Option<(usize, usize)> {
    let lo = stack_pointer_here() & !7;
    let mut hi = LAZY_STACK_TOP.try_with(Cell::get).ok()?;
    if hi == 0 {
        hi = os_stack_top()?;
        let _ = LAZY_STACK_TOP.try_with(|c| c.set(hi));
    }
    (lo != 0 && lo < hi).then_some((lo, hi))
}

/// Interpreter round i7 wave 139, lane L1b (`CRATONVM_PROCESS_WAIT_LAZY_NATIVE`,
/// performance, default OFF; the vm's `NativeContext::run_in_native_wait_lazy`):
/// run `wait` -- one OS wait that touches no Java object and holds no Java
/// reference -- in the lazy in-native state, as a lazy FFM downcall runs its C
/// call: spill the callee-saved registers into this frame, take the band from
/// here to the stack top, `begin_native_region_lazy`, wait, leave. `false`
/// (nothing ran) when the band is unknown or the context declines (a pause
/// that cannot claim the thread, an unreadable JIT chain, a non-x86-64 target,
/// where the spill is zeros and the context declines). Frames below the band
/// are `wait`'s own and hold no reference, by the contract.
#[inline(never)]
pub fn run_in_native_wait_lazy(ctx: &mut dyn NativeContext, wait: &mut dyn FnMut()) -> bool {
    let spill = CalleeSavedSpill::capture();
    let Some((lo, hi)) = lazy_band_from_here() else {
        return false;
    };
    if !ctx.begin_native_region_lazy(lo, hi) {
        return false;
    }
    wait();
    ctx.end_native_region_lazy();
    spill.keep_alive();
    true
}

/// Open a region, the quiet way or the full one; the state it is then in.
fn begin_region(ctx: &mut dyn NativeContext, quiet: bool) -> DowncallRegion {
    if quiet {
        let (generation, folds) = ctx.begin_native_region_quiet();
        DowncallRegion::OpenQuiet(generation, folds)
    } else {
        // Round 14 wave 1 (lane ffm): the downcall's RUNNABLE in-native state.
        ctx.begin_native_region();
        DowncallRegion::Open
    }
}

/// RAII guard installing the active NativeContext for the duration of
/// a downcall. On drop, restores the previous value.
pub struct ActiveContextGuard {
    prev: ActiveContextPtr,
    /// The enclosing downcall's [`ACTIVE_REGION`], restored on drop.
    prev_region: DowncallRegion,
}

impl ActiveContextGuard {
    /// Mark this downcall as running inside a full GC blocking region
    /// (`true`: the caller has just entered one with `begin_native_region`)
    /// or not (`false`: it has just left it). Upcalls read it through
    /// [`active_downcall_gc_safe`]; the caller clears it as soon as the region
    /// ends, because the guard stays installed after the call and Java that
    /// runs meanwhile can reach an upcall stub. `pe_downcall_invoke` uses
    /// [`Self::enter_region`] / [`Self::leave_region`] since JIT round 16
    /// wave 9.
    pub fn set_gc_safe(&self, on: bool) {
        let state = if on {
            DowncallRegion::Open
        } else {
            DowncallRegion::Counted
        };
        ACTIVE_REGION.with(|c| c.set(state));
    }

    /// JIT round 16 wave 9 (lane ffm): open this downcall's GC-safe region
    /// right before the C call -- the quiet in-native transition
    /// (`begin_native_region_quiet`) or the full one -- and record which, so
    /// an upcall or a JNIEnv function leaves and re-enters it the same way.
    pub fn enter_region(&self, ctx: &mut dyn NativeContext, quiet: bool) {
        let state = begin_region(ctx, quiet);
        ACTIVE_REGION.with(|c| c.set(state));
    }

    /// JIT round 16 wave 14 (lane ffm3): go into C lazily
    /// (`NativeContext::begin_native_region_lazy`: one release store, no
    /// deposit) over the band from here to the stack top. The caller must
    /// have captured a [`CalleeSavedSpill`] in its own frame first. When the
    /// context declines (or the band is unknown), `fallback` decides: `None`
    /// keeps the thread counted, `Some(quiet)` opens the deposit-based region
    /// [`Self::enter_region`] would.
    #[inline(always)]
    pub fn enter_region_lazy(&self, ctx: &mut dyn NativeContext, fallback: Option<bool>) {
        let state = match lazy_band_from_here() {
            Some((lo, hi)) if ctx.begin_native_region_lazy(lo, hi) => {
                DowncallRegion::OpenLazy(lo, hi)
            }
            _ => match fallback {
                Some(quiet) => begin_region(ctx, quiet),
                None => DowncallRegion::Counted,
            },
        };
        ACTIVE_REGION.with(|c| c.set(state));
    }

    /// JIT round 16 wave 9 (lane ffm): close what [`Self::enter_region`]
    /// opened, right after the C call, with the answer of the LATEST entry
    /// (an upcall re-enters with a fresh one). The state is cleared first,
    /// for the reason [`Self::set_gc_safe`] gives. A no-op when no region is
    /// open.
    pub fn leave_region(&self, ctx: &mut dyn NativeContext) {
        let state = ACTIVE_REGION.with(|c| c.replace(DowncallRegion::Counted));
        end_region(ctx, state);
    }

    pub fn install(ctx: &mut dyn NativeContext) -> Self {
        // SAFETY: we artificially extend the lifetime of `ctx` to
        // `'static` only inside the thread-local slot. The
        // `ActiveContextGuard` returned from this function will restore
        // the slot in its `Drop` impl, so the fat pointer never
        // outlives the original borrow. Callers must never let an
        // `ActiveContextGuard` live past the surrounding downcall.
        let raw: *mut (dyn NativeContext + 'static) = unsafe {
            std::mem::transmute::<*mut dyn NativeContext, *mut (dyn NativeContext + 'static)>(
                ctx as *mut dyn NativeContext,
            )
        };
        // A reference's address is never null, so this is always `Some`.
        let installed = std::ptr::NonNull::new(raw);
        let prev = ACTIVE_NATIVE_CONTEXT.with(|c| c.replace(installed));
        let prev_region = ACTIVE_REGION.with(|c| c.replace(DowncallRegion::Counted));
        ActiveContextGuard { prev, prev_region }
    }
}

/// Whether the innermost active downcall on this thread is inside a GC
/// blocking region (see [`ActiveContextGuard::set_gc_safe`]).
pub fn active_downcall_gc_safe() -> bool {
    ACTIVE_REGION.with(|c| c.get()).is_open()
}

/// The innermost active downcall's [`DowncallRegion`] (JIT round 16 wave 9).
pub fn active_downcall_region() -> DowncallRegion {
    ACTIVE_REGION.with(|c| c.get())
}

/// gcd d5/f: leave the innermost downcall's GC-safe region for code that must
/// run as a mutator -- an upcall's Java (`panama_upcall::upcall_entry`) -- and
/// mark it LEFT, so nothing else on this thread leaves it a second time while
/// that code runs (a JNIEnv function a JNI native in the upcall's Java calls
/// asks [`leave_active_downcall_region`]; before this it would have found
/// the region still marked open and ended it twice). A no-op when the region
/// is not open. Pair with [`reenter_downcall_region`].
///
/// JIT round 16 wave 9 (lane ffm): a quiet region is left the quiet way
/// (`end_native_region_quiet`, with its entry's answer) and marked
/// [`DowncallRegion::LeftQuiet`], so the re-entry is quiet too.
pub fn leave_downcall_region(ctx: &mut dyn NativeContext) {
    let state = ACTIVE_REGION.with(|c| c.get());
    let left = match state {
        DowncallRegion::Open => DowncallRegion::Left,
        DowncallRegion::OpenQuiet(..) => DowncallRegion::LeftQuiet,
        DowncallRegion::OpenLazy(lo, hi) => DowncallRegion::LeftLazy(lo, hi),
        DowncallRegion::Counted
        | DowncallRegion::Left
        | DowncallRegion::LeftQuiet
        | DowncallRegion::LeftLazy(..) => return,
    };
    ACTIVE_REGION.with(|c| c.set(left));
    end_region(ctx, state);
}

/// gcd d5/f: back into the downcall's GC-safe region after
/// [`leave_downcall_region`] left it: the quiet way after a quiet region (JIT
/// round 16 wave 9: a fresh deposit and a fresh answer, which the downcall's
/// own leave then uses), the full way otherwise, as before.
pub fn reenter_downcall_region(ctx: &mut dyn NativeContext) {
    let left = ACTIVE_REGION.with(|c| c.get());
    // JIT round 16 wave 14 (lane ffm3): back into the lazy state over the
    // downcall's own band (its frames and spilled registers are unchanged:
    // the up-call's or JNIEnv function's frames were below it and are gone).
    // Should the context now decline, the quiet region keeps the thread out
    // of every pause instead.
    if let DowncallRegion::LeftLazy(lo, hi) = left {
        let state = if ctx.begin_native_region_lazy(lo, hi) {
            DowncallRegion::OpenLazy(lo, hi)
        } else {
            begin_region(ctx, true)
        };
        ACTIVE_REGION.with(|c| c.set(state));
        return;
    }
    let quiet = left == DowncallRegion::LeftQuiet;
    let state = begin_region(ctx, quiet);
    ACTIVE_REGION.with(|c| c.set(state));
}

/// gcd d5/f: for a JNIEnv function that C code calls from INSIDE a GC-safe
/// downcall (`CRATONVM_FFM_DOWNCALL_GC_SAFE`; the vm's
/// `native::jni::ForeignJniEntry`): leave the downcall's region through the
/// downcall's own context before the function touches the heap, exactly as an
/// upcall does. `true` if it left; the caller must then
/// [`reenter_active_downcall_region`] when the function returns. `false`
/// (nothing done) outside a downcall, or when the region is not open.
pub fn leave_active_downcall_region() -> bool {
    if !active_downcall_gc_safe() {
        return false;
    }
    with_active_context(|ctx| {
        leave_downcall_region(ctx);
        true
    })
    .unwrap_or(false)
}

/// The re-entry half of [`leave_active_downcall_region`].
pub fn reenter_active_downcall_region() {
    #[allow(clippy::redundant_closure)]
    let _ = with_active_context(|ctx| reenter_downcall_region(ctx));
}

impl Drop for ActiveContextGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        ACTIVE_NATIVE_CONTEXT.with(|c| c.set(prev));
        let prev_region = self.prev_region;
        ACTIVE_REGION.with(|c| c.set(prev_region));
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
    let fat_ptr = ACTIVE_NATIVE_CONTEXT.with(|c| c.get())?;
    // SAFETY: see function-level safety comment.
    unsafe { Some(f(&mut *fat_ptr.as_ptr())) }
}

/// JIT round 16 wave 12 (lane inpkg): is a downcall's context installed on
/// this thread (an up-call stub called now would be entered through it, see
/// [`with_active_context`])? Reads the slot only; forms no reference to the
/// context. The vm's JNI in-native package keeps a JNI native counted then
/// (`native::jni::JniNativeCall`).
pub fn has_active_downcall_context() -> bool {
    ACTIVE_NATIVE_CONTEXT
        .try_with(|c| c.get().is_some())
        .unwrap_or(false)
}

/// Test helper: returns true if the active context cell currently
/// holds a non-null context pointer.
#[cfg(test)]
pub fn has_active_context() -> bool {
    ACTIVE_NATIVE_CONTEXT.with(|c| c.get()).is_some()
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

    /// JIT round 16 wave 11 (lane ffmcost): the plain-downcall cell writer
    /// produces `marshal_arg`'s bytes for every primitive kind and every
    /// `Value` shape either can be handed, and refuses ADDRESS and groups.
    #[test]
    fn plain_cell_matches_marshal_arg() {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("java/lang/foreign/ValueLayout")
            .unwrap();
        let values = [
            Value::Int(0),
            Value::Int(-1),
            Value::Int(0x1_2345),
            Value::Int(i32::MIN),
            Value::Long(-7),
            Value::Long(i64::MAX),
            Value::Float(-1.5),
            Value::Float(f32::NAN),
            Value::Double(2.25),
            Value::Double(-0.0),
            Value::Object(None),
        ];
        for kind in [
            LAYOUT_BYTE,
            LAYOUT_BOOLEAN,
            LAYOUT_SHORT,
            LAYOUT_CHAR,
            LAYOUT_INT,
            LAYOUT_LONG,
            LAYOUT_FLOAT,
            LAYOUT_DOUBLE,
        ] {
            let layout = ctx.alloc_object(cid, 2);
            ctx.set_field(layout, 0, Value::Int(kind));
            for v in &values {
                let want = match marshal_arg(&ctx, layout, v) {
                    Ok(slot) => slot.bytes.clone(),
                    Err(_) => panic!("kind {kind} refused {v:?}"),
                };
                let mut cell = u64::MAX;
                assert!(marshal_primitive_into(kind, v, &mut cell), "kind {kind}");
                let got = cell.to_ne_bytes();
                assert_eq!(&got[..want.len()], &want[..], "kind {kind} value {v:?}");
                assert!(got[want.len()..].iter().all(|b| *b == 0), "kind {kind}");
            }
        }
        for kind in [
            LAYOUT_ADDRESS,
            LAYOUT_STRUCT,
            LAYOUT_UNION,
            LAYOUT_SEQUENCE,
            LAYOUT_UNKNOWN,
        ] {
            let mut cell = 0x55u64;
            assert!(!marshal_primitive_into(kind, &Value::Long(1), &mut cell));
            assert_eq!(cell, 0x55, "a refused kind leaves the cell untouched");
        }
    }

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
            translated: 0,
            span: 0,
        };
        assert_eq!(
            slot.handle_for(0x1234_5678),
            None,
            "an untranslated slot maps nothing"
        );
        let real = 0x7000_0000usize;
        let mapped = ArgSlot {
            bytes: real.to_ne_bytes().to_vec(),
            translated: ARENA_HANDLE_TAG | 0x40,
            span: 6,
        };
        assert_eq!(
            mapped.handle_for(real as i64 + 2),
            Some(ARENA_HANDLE_TAG | 0x42)
        );
        assert_eq!(
            mapped.handle_for(real as i64 + 6),
            Some(ARENA_HANDLE_TAG | 0x46),
            "one past the end"
        );
        assert_eq!(mapped.handle_for(real as i64 + 7), None, "past the block");
        assert_eq!(
            mapped.handle_for(real as i64 - 1),
            None,
            "before the pointer passed"
        );
        std::mem::forget(mapped); // its handle was never translated
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

    /// NOM-3 (G6-1). The five-slot sibling of the test above: a carrier
    /// minted with a STORED count at slot 4 must have that count preferred
    /// over the total/element-size division, exactly like
    /// `SequenceLayout.elementCount()`. G6-1 §8.2's own motivating case
    /// (`sequenceLayout(3, structLayout())`, where the division answers 0
    /// because the element's `byteSize()` is 0) is not reachable through
    /// *this* function specifically — `layout_to_ffi_type` refuses a
    /// zero-member `StructLayout` outright as unmarshallable by value,
    /// independent of NOM-2 — so this test proves the same precedence with a
    /// marshallable `JAVA_INT` element instead: the carrier's OWN `byteSize`
    /// (slot 0) is deliberately wrong for its stored count (40 bytes would
    /// divide to 10 elements of a 4-byte int) so a division-based reader and
    /// a slot-4-preferring one give different, checkable answers.
    #[test]
    fn sequence_carrier_with_a_stored_count_prefers_it_over_the_division() {
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

        let seq = ctx.alloc_object(cid, 5);
        ctx.set_field(seq, 0, Value::Long(40));
        ctx.set_field(seq, 1, Value::Long(4));
        ctx.set_field(seq, 2, Value::Object(Some(elem)));
        ctx.set_field(seq, 3, Value::Object(None));
        ctx.set_field(seq, 4, Value::Long(3));

        assert_eq!(read_layout_kind(&ctx, seq), LAYOUT_SEQUENCE);
        assert_eq!(
            layout_total_size(&ctx, seq).unwrap() / layout_total_size(&ctx, elem).unwrap(),
            10,
            "the division alone would answer 10, not the stored 3"
        );
        let ffi = layout_to_ffi_type(&ctx, seq, 0).expect("a stored count must be readable");
        // `libffi::middle::Type` has no public element-count accessor; walk
        // the null-terminated `elements` array on the raw `ffi_type` it
        // wraps, the same shape `ffi_type_array_create` builds it in.
        let raw = ffi.as_raw_ptr();
        let mut count = 0usize;
        unsafe {
            let mut p = (*raw).elements;
            while !(*p).is_null() {
                count += 1;
                p = p.add(1);
            }
        }
        assert_eq!(
            count, 3,
            "the stored count (slot 4) must win over the 0/0 division"
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
    ///
    /// The exception TYPE and wording are pinned to the oracle, not just the
    /// intent — MEASURED on HotSpot 25.0.3+9-LTS (`FfmRepro2.java`):
    /// `strlen.invoke(MemorySegment.ofArray(new byte[]{'h','i',0}))` throws
    /// `java.lang.IllegalArgumentException: Heap segment not allowed:
    /// MemorySegment{ kind: heap, heapBase: [B@…, address: 0x0, byteSize: 3 }`.
    /// This test used to assert `IllegalStateException`, which nobody had
    /// checked against the oracle for this exact call site; corrected here
    /// alongside widening the refusal past H1.
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
                RuntimeError::IllegalArgumentException { message },
            )) => assert!(
                message.contains("Heap segment not allowed"),
                "the refusal must match the oracle's own wording: {message}"
            ),
            other => panic!("expected IllegalArgumentException, got {other:?}"),
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

    /// H2 (this VM's `ofArray(byte[]|short[]|char[])` ALIAS carrier) must be
    /// refused exactly like H1: before `is_alias_heap_segment` existed,
    /// `marshal_arg`'s downcall-pointer screen checked only
    /// `is_real_heap_segment`, so this shape fell through to
    /// `segment_address` (which answers 0 for it — a legitimate C null) and a
    /// downcall silently received `NULL` instead of a refusal. The eight-slot
    /// layout mirrors production exactly: see `panama::pe_of_array_alias` and
    /// its own test-module twin `panama::tests::heap_alias_segment`.
    #[test]
    fn an_alias_heap_segment_pointer_argument_is_also_refused() {
        let mut ctx = mock_ctx();
        let layout_cid = ctx
            .ensure_class_initialized("java/lang/foreign/AddressLayout")
            .unwrap();
        let layout = ctx.alloc_object(layout_cid, 4);
        ctx.set_field(layout, 0, Value::Long(8));
        ctx.set_field(layout, 1, Value::Long(8));

        let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 3);

        let seg_cid = ctx
            .ensure_class_initialized("java/lang/foreign/MemorySegment")
            .unwrap();
        let seg = ctx.alloc_object(seg_cid, 8);
        ctx.set_field(seg, 0, Value::Long(0)); // no machine address
        ctx.set_field(seg, 1, Value::Long(3)); // byteSize
        ctx.set_field(seg, 2, Value::Object(None)); // scope
        ctx.set_field(seg, 3, Value::Int(0)); // readOnly
        ctx.set_field(seg, 4, Value::Int(1)); // alive
        ctx.set_field(seg, 5, Value::Long(0)); // offset
        ctx.set_field(seg, 6, Value::Object(Some(array))); // heap base
        ctx.set_field(seg, 7, Value::Long(0)); // start within array

        let Err(err) = marshal_arg(&ctx, layout, &Value::Object(Some(seg))) else {
            panic!("an alias heap MemorySegment has no address to pass to native code");
        };
        match err {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { message },
            )) => assert!(
                message.contains("Heap segment not allowed"),
                "the refusal must match the oracle's own wording: {message}"
            ),
            other => panic!("expected IllegalArgumentException, got {other:?}"),
        }
    }

    /// `is_alias_heap_segment` classifies by FIELD COUNT alone (`> 7`, i.e.
    /// exactly the real alias carrier's width), which is deliberately
    /// narrower than "any heap-shaped carrier" — a seven-field object is not
    /// wide enough to match, whatever it holds. This shape briefly
    /// corresponded to a real carrier — `ofArray(int[]|long[]|float[]|
    /// double[])`'s retired native-mirror allocation, "H3" — and is kept as
    /// a hand-built boundary case now that nothing mints it: a segment
    /// `marshal_arg` doesn't recognize as alias-only must still marshal its
    /// slot-0 value as a plain pointer, not be refused like a real H1/H2
    /// heap segment is (the two tests above).
    #[test]
    fn a_seven_field_carrier_below_the_alias_width_still_marshals() {
        let mut ctx = mock_ctx();
        let layout_cid = ctx
            .ensure_class_initialized("java/lang/foreign/AddressLayout")
            .unwrap();
        let layout = ctx.alloc_object(layout_cid, 4);
        ctx.set_field(layout, 0, Value::Long(8));
        ctx.set_field(layout, 1, Value::Long(8));

        let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);

        // The retired seven-slot mirror shape, preserved here only as a
        // field-count boundary case: [0]=native ptr, [1]=byteSize,
        // [2]=array, [3]=?, [4]=kind, [5]=offset, [6]=session.
        let seg_cid = ctx
            .ensure_class_initialized("java/lang/foreign/MemorySegment")
            .unwrap();
        let seg = ctx.alloc_object(seg_cid, 7);
        ctx.set_field(seg, 0, Value::Long(0x7f00_0000)); // native mirror address
        ctx.set_field(seg, 1, Value::Long(8)); // byteSize
        ctx.set_field(seg, 3, Value::Int(0)); // read-write
        ctx.set_field(seg, 4, Value::Int(1)); // alive
        ctx.set_field(seg, 5, Value::Long(0)); // offset
        ctx.set_field(seg, 2, Value::Object(Some(array))); // SEG_BACKING_ARRAY_FIELD
        ctx.set_field(seg, 6, Value::Object(None)); // SEG_MIRROR_SESSION_FIELD

        assert!(
            !crate::panama::is_alias_heap_segment(&ctx, seg),
            "a mirror carrier must not classify as an alias-only heap segment"
        );
        let slot = marshal_arg(&ctx, layout, &Value::Object(Some(seg)))
            .expect("a mirror carrier's real native buffer must still marshal");
        assert_eq!(
            slot.bytes,
            (0x7f00_0000usize).to_ne_bytes().to_vec(),
            "must marshal the mirror's real native pointer, not a refusal"
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

    /// Round 12 wave 7 (lane upcall): a `JAVA_CHAR` return is unsigned. The
    /// widened slot of `(char) 0xFFFE` answered -2; a `short` keeps its sign.
    #[test]
    fn unmarshal_char_is_zero_extended_and_short_is_not() {
        let slot = 0xFFFEusize.to_ne_bytes();
        if char_return_unsigned() {
            assert_eq!(
                unmarshal_return_primitive(LAYOUT_CHAR, &slot),
                Value::Int(0xFFFE)
            );
        }
        assert_eq!(
            unmarshal_return_primitive(LAYOUT_SHORT, &slot),
            Value::Int(-2)
        );
    }
}

#[cfg(test)]
mod r13w5_ffm3_gc_safe_tests {
    //! Round 13 wave 5 (lane ffm3): the "downcall is inside a GC blocking
    //! region" flag an upcall reads nests with the context guards: a nested
    //! downcall starts clear, and each guard's drop restores its enclosing
    //! downcall's value.
    use super::*;
    use crate::test_utils::mock_ctx;

    #[test]
    fn the_gc_safe_flag_nests_with_the_guards() {
        let mut outer_ctx = mock_ctx();
        let mut inner_ctx = mock_ctx();
        assert!(!active_downcall_gc_safe());
        {
            let outer = ActiveContextGuard::install(&mut outer_ctx);
            assert!(!active_downcall_gc_safe(), "a fresh guard starts clear");
            outer.set_gc_safe(true);
            assert!(active_downcall_gc_safe());
            {
                let _inner = ActiveContextGuard::install(&mut inner_ctx);
                assert!(!active_downcall_gc_safe(), "a nested downcall starts clear");
            }
            assert!(
                active_downcall_gc_safe(),
                "the outer downcall's flag is restored"
            );
            outer.set_gc_safe(false);
            assert!(!active_downcall_gc_safe());
        }
        assert!(!active_downcall_gc_safe());
        assert!(!has_active_context());
    }
}

#[cfg(test)]
mod r16w9_ffm_quiet_region_tests {
    //! JIT round 16 wave 9 (lane ffm): a downcall's region is left and
    //! re-entered the way it was opened, every entry is paired with exactly
    //! one exit, and the state nests with the guards. The mock does not
    //! override the quiet pair, so its trait defaults count one ordinary
    //! blocking region per entry (`blocking_region_counts`).
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::NativeThreadAccess;

    #[test]
    fn a_quiet_region_is_left_and_reentered_quietly_and_paired() {
        let mut ctx = mock_ctx();
        {
            let guard = ActiveContextGuard::install(&mut ctx);
            guard.enter_region(&mut ctx, true);
            assert!(matches!(
                active_downcall_region(),
                DowncallRegion::OpenQuiet(..)
            ));
            assert!(active_downcall_gc_safe());
            assert_eq!(ctx.blocking_region_counts(), (1, 0));
            // An upcall: leave, run Java, back in.
            leave_downcall_region(&mut ctx);
            assert_eq!(active_downcall_region(), DowncallRegion::LeftQuiet);
            assert!(!active_downcall_gc_safe());
            assert_eq!(ctx.blocking_region_counts(), (1, 1));
            // A second leave while left (a JNIEnv call in the upcall's Java)
            // ends nothing.
            assert!(!leave_active_downcall_region());
            leave_downcall_region(&mut ctx);
            assert_eq!(ctx.blocking_region_counts(), (1, 1));
            {
                // A nested downcall in the upcall's Java starts counted and
                // gives the outer state back.
                let mut inner_ctx = mock_ctx();
                let inner = ActiveContextGuard::install(&mut inner_ctx);
                assert_eq!(active_downcall_region(), DowncallRegion::Counted);
                inner.enter_region(&mut inner_ctx, true);
                inner.leave_region(&mut inner_ctx);
                assert_eq!(inner_ctx.blocking_region_counts(), (1, 1));
            }
            assert_eq!(active_downcall_region(), DowncallRegion::LeftQuiet);
            reenter_downcall_region(&mut ctx);
            assert!(matches!(
                active_downcall_region(),
                DowncallRegion::OpenQuiet(..)
            ));
            assert_eq!(ctx.blocking_region_counts(), (2, 1));
            // A JNIEnv function C calls: leaves through the active context.
            assert!(leave_active_downcall_region());
            assert_eq!(active_downcall_region(), DowncallRegion::LeftQuiet);
            reenter_active_downcall_region();
            assert!(matches!(
                active_downcall_region(),
                DowncallRegion::OpenQuiet(..)
            ));
            assert_eq!(ctx.blocking_region_counts(), (3, 2));
            guard.leave_region(&mut ctx);
            assert_eq!(active_downcall_region(), DowncallRegion::Counted);
            assert_eq!(ctx.blocking_region_counts(), (3, 3));
            // Nothing left to end.
            guard.leave_region(&mut ctx);
            assert_eq!(ctx.blocking_region_counts(), (3, 3));
        }
        assert_eq!(active_downcall_region(), DowncallRegion::Counted);
        assert!(!has_active_context());
    }

    #[test]
    fn a_full_region_is_left_and_reentered_the_full_way() {
        let mut ctx = mock_ctx();
        {
            let guard = ActiveContextGuard::install(&mut ctx);
            guard.enter_region(&mut ctx, false);
            assert_eq!(active_downcall_region(), DowncallRegion::Open);
            leave_downcall_region(&mut ctx);
            assert_eq!(active_downcall_region(), DowncallRegion::Left);
            reenter_downcall_region(&mut ctx);
            assert_eq!(active_downcall_region(), DowncallRegion::Open);
            guard.leave_region(&mut ctx);
            assert_eq!(ctx.blocking_region_counts(), (2, 2));
        }
        assert_eq!(active_downcall_region(), DowncallRegion::Counted);
    }

    #[test]
    fn the_trait_default_quiet_pair_is_one_ordinary_region() {
        let mut ctx = mock_ctx();
        let sync = ctx.begin_native_region_quiet();
        assert_eq!(ctx.blocking_region_counts(), (1, 0));
        ctx.end_native_region_quiet(sync);
        assert_eq!(ctx.blocking_region_counts(), (1, 1));
    }
}

#[cfg(test)]
mod r16w14_ffm3_lazy_region_tests {
    //! JIT round 16 wave 14 (lane ffm3): the lazy in-native state of a
    //! downcall (`CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY`) falls back as asked
    //! when the context declines, and otherwise is left and re-entered over
    //! the downcall's own band, which holds the calling frame.
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::NativeThreadAccess;

    #[test]
    fn a_declined_lazy_entry_stays_counted_or_takes_the_fallback() {
        let mut ctx = mock_ctx();
        {
            let guard = ActiveContextGuard::install(&mut ctx);
            guard.enter_region_lazy(&mut ctx, None);
            assert_eq!(active_downcall_region(), DowncallRegion::Counted);
            assert!(!active_downcall_gc_safe());
            guard.leave_region(&mut ctx);
            guard.enter_region_lazy(&mut ctx, Some(true));
            assert!(matches!(
                active_downcall_region(),
                DowncallRegion::OpenQuiet(..)
            ));
            guard.leave_region(&mut ctx);
            assert_eq!(ctx.blocking_region_counts(), (1, 1));
        }
        assert_eq!(ctx.lazy_native_log.0, 0, "the mock declined every entry");
        assert_eq!(active_downcall_region(), DowncallRegion::Counted);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_lazy_region_is_left_and_reentered_over_its_own_band() {
        let mut ctx = mock_ctx();
        ctx.lazy_native_accept = true;
        let spill = CalleeSavedSpill::capture();
        let here = &spill as *const CalleeSavedSpill as usize;
        {
            let guard = ActiveContextGuard::install(&mut ctx);
            guard.enter_region_lazy(&mut ctx, Some(true));
            let DowncallRegion::OpenLazy(lo, hi) = active_downcall_region() else {
                panic!("the accepting context must enter the lazy state");
            };
            assert!(
                lo < here && here < hi,
                "the calling frame and its register spill lie inside the band"
            );
            assert_eq!(lo & 7, 0);
            assert!(active_downcall_gc_safe());
            // An up-call: leave, run Java, back in over the same band.
            leave_downcall_region(&mut ctx);
            assert_eq!(active_downcall_region(), DowncallRegion::LeftLazy(lo, hi));
            assert!(!active_downcall_gc_safe());
            assert!(!leave_active_downcall_region(), "already left");
            reenter_downcall_region(&mut ctx);
            assert_eq!(active_downcall_region(), DowncallRegion::OpenLazy(lo, hi));
            // A JNIEnv function C calls.
            assert!(leave_active_downcall_region());
            reenter_active_downcall_region();
            assert_eq!(active_downcall_region(), DowncallRegion::OpenLazy(lo, hi));
            guard.leave_region(&mut ctx);
            assert_eq!(active_downcall_region(), DowncallRegion::Counted);
            assert_eq!(ctx.lazy_native_log.0, 3, "three entries");
            assert_eq!(ctx.lazy_native_log.1, 3, "each one left once");
            assert_eq!(ctx.lazy_native_log.2, (lo, hi));
        }
        assert_eq!(
            ctx.blocking_region_counts(),
            (0, 0),
            "no deposit-based region was opened"
        );
        spill.keep_alive();
    }

    /// The band's stack top is asked of the OS once per thread and cached.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn the_band_runs_from_below_the_caller_to_the_cached_stack_top() {
        let local = 0u64;
        let here = std::hint::black_box(&local) as *const u64 as usize;
        let Some((lo, hi)) = lazy_band_from_here() else {
            panic!("the stack top of a test thread is known");
        };
        assert!(lo < here && here < hi);
        assert_eq!(LAZY_STACK_TOP.with(Cell::get), hi);
        assert_eq!(lazy_band_from_here().map(|b| b.1), Some(hi));
    }
}

#[cfg(test)]
mod r13w12_misc12_union_class_tests {
    //! Round 13 wave 12 (lane misc12): a by-value union's SysV eightbyte
    //! classes (`r13w11-ffm6-union-ffi-type-is-always-integer-class`).
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ClassId;

    /// `union { double d; float f; }`: one SSE eightbyte, one `double`.
    #[test]
    fn a_union_of_double_and_float_is_one_double() {
        let classes = sysv_eightbyte_classes(8, &[(0, 8, true), (0, 4, true)]).unwrap();
        assert_eq!(classes, vec![Eightbyte::Sse]);
        assert_eq!(
            union_eightbyte_elems(8, 8, &classes),
            Some(vec![UnionElem::F64])
        );
    }

    /// An integer member anywhere in an eightbyte makes it INTEGER, and a
    /// union with no SSE eightbyte keeps the byte struct.
    #[test]
    fn an_integer_member_wins_its_eightbyte() {
        // union { float f; int i; }
        let classes = sysv_eightbyte_classes(4, &[(0, 4, true), (0, 4, false)]).unwrap();
        assert_eq!(classes, vec![Eightbyte::Integer]);
        assert_eq!(union_eightbyte_elems(4, 4, &classes), None);
        // union { double d; long l; }
        let classes = sysv_eightbyte_classes(8, &[(0, 8, true), (0, 8, false)]).unwrap();
        assert_eq!(union_eightbyte_elems(8, 8, &classes), None);
    }

    /// Mixed and partial eightbytes, and the refusals.
    #[test]
    fn mixed_eightbytes_keep_their_size_and_the_limits_fall_back() {
        // union { struct { float x, y, z; } v; float f; }: 12 bytes, align 4.
        let leaves = [(0, 4, true), (4, 4, true), (8, 4, true), (0, 4, true)];
        let classes = sysv_eightbyte_classes(12, &leaves).unwrap();
        assert_eq!(classes, vec![Eightbyte::Sse, Eightbyte::Sse]);
        assert_eq!(
            union_eightbyte_elems(12, 4, &classes),
            Some(vec![UnionElem::F32, UnionElem::F32, UnionElem::F32])
        );
        // union { struct { double d; long l; } s; double e; }: 16 bytes.
        let leaves = [(0, 8, true), (8, 8, false), (0, 8, true)];
        let classes = sysv_eightbyte_classes(16, &leaves).unwrap();
        assert_eq!(classes, vec![Eightbyte::Sse, Eightbyte::Integer]);
        let elems = union_eightbyte_elems(16, 8, &classes).unwrap();
        assert_eq!(elems.len(), 9);
        assert_eq!(elems[0], UnionElem::F64);
        assert!(elems[1..].iter().all(|e| *e == UnionElem::Byte));
        // Larger than 16 bytes: MEMORY class, the byte struct stays.
        let classes = vec![Eightbyte::Sse; 3];
        assert_eq!(union_eightbyte_elems(24, 8, &classes), None);
        // A misaligned double: C would pass it in memory.
        assert_eq!(sysv_eightbyte_classes(12, &[(4, 8, true)]), None);
        // An all-float eightbyte in a byte-aligned union cannot be rebuilt.
        assert_eq!(union_eightbyte_elems(8, 1, &[Eightbyte::Sse]), None);
    }

    fn value_layout(ctx: &mut MockNativeContext, class: &str, size: i64) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(class).unwrap();
        let l = ctx.alloc_object(cid, 4);
        ctx.set_field(l, 0, Value::Long(size));
        ctx.set_field(l, 1, Value::Long(size));
        ctx.set_field(l, 2, Value::Object(None));
        ctx.set_field(l, 3, Value::Object(None));
        l
    }

    /// The whole path on a union carrier: `unionLayout(JAVA_DOUBLE,
    /// JAVA_FLOAT)` becomes a struct of ONE `FFI_TYPE_DOUBLE`.
    #[test]
    fn a_double_float_union_carrier_builds_one_double_element() {
        let mut ctx = mock_ctx();
        let d = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfDouble", 8);
        let f = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfFloat", 4);
        let cid = ctx
            .ensure_class_initialized("java/lang/foreign/UnionLayout")
            .unwrap();
        // A typed producer (`typed_array_producer_ratchet`).
        let members = ctx.new_ref_array(ClassId::new(1), 2);
        ctx.set_array_element(members, 0, Value::Object(Some(d)));
        ctx.set_array_element(members, 1, Value::Object(Some(f)));
        let u = ctx.alloc_object(cid, 4);
        ctx.set_field(u, 0, Value::Long(8));
        ctx.set_field(u, 1, Value::Long(8));
        ctx.set_field(u, 2, Value::Object(Some(members)));
        ctx.set_field(u, 3, Value::Object(None));

        let ty = union_sysv_eightbyte_ffi_type(&ctx, u, 8, 0).expect("an SSE union");
        // SAFETY: `ty` owns the struct type and its null-terminated elements.
        unsafe {
            let raw = &*ty.as_raw_ptr();
            assert!(!raw.elements.is_null());
            let first = *raw.elements;
            assert!(!first.is_null());
            assert_eq!((*first).type_ as u32, libffi::raw::FFI_TYPE_DOUBLE);
            assert!((*raw.elements.add(1)).is_null(), "exactly one element");
        }
    }

    /// Round 13 wave 13 (lane ffm7): the AAPCS64 HFA rule for unions
    /// (`r13w12-misc12-aarch64-union-hfa-travels-in-general-registers`).
    #[test]
    fn an_all_float_union_is_an_hfa_of_its_widest_member() {
        // union { float f; float g[2]; }: two floats.
        assert_eq!(
            union_hfa_elems(8, &[(0, 4, true), (0, 4, true), (4, 4, true)]),
            Some((UnionElem::F32, 2))
        );
        // union { double d; }: one double.
        assert_eq!(
            union_hfa_elems(8, &[(0, 8, true)]),
            Some((UnionElem::F64, 1))
        );
        // union { struct { double x, y, z, w; } v; double d; }: four doubles.
        let four = [
            (0, 8, true),
            (8, 8, true),
            (16, 8, true),
            (24, 8, true),
            (0, 8, true),
        ];
        assert_eq!(union_hfa_elems(32, &four), Some((UnionElem::F64, 4)));
    }

    /// What is not an HFA keeps the byte struct.
    #[test]
    fn a_mixed_or_integer_union_is_not_an_hfa() {
        // union { float f; double d; }: two base types.
        assert_eq!(union_hfa_elems(8, &[(0, 4, true), (0, 8, true)]), None);
        // union { float f; int i; }
        assert_eq!(union_hfa_elems(4, &[(0, 4, true), (0, 4, false)]), None);
        // Five floats: more than four elements.
        let five: Vec<(usize, usize, bool)> = (0..5).map(|k| (4 * k, 4, true)).collect();
        assert_eq!(union_hfa_elems(20, &five), None);
        // union { float f; } padded to 8: the second slot has no member.
        assert_eq!(union_hfa_elems(8, &[(0, 4, true)]), None);
        // No leaves at all.
        assert_eq!(union_hfa_elems(4, &[]), None);
    }

    /// The whole builder on a carrier, on every host (it is called on AArch64
    /// only): `unionLayout(JAVA_FLOAT, sequenceLayout(2, JAVA_FLOAT))` becomes
    /// a struct of exactly two `FFI_TYPE_FLOAT`.
    #[test]
    fn a_float_union_carrier_builds_two_float_elements() {
        let mut ctx = mock_ctx();
        let f = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfFloat", 4);
        let f2 = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfFloat", 4);
        let g = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfFloat", 4);
        let scid = ctx
            .ensure_class_initialized("java/lang/foreign/StructLayout")
            .unwrap();
        // `struct { float; float; }` stands in for `float[2]`: same leaves.
        let struct_members = ctx.new_ref_array(ClassId::new(1), 2);
        ctx.set_array_element(struct_members, 0, Value::Object(Some(f2)));
        ctx.set_array_element(struct_members, 1, Value::Object(Some(g)));
        let pair = ctx.alloc_object(scid, 4);
        ctx.set_field(pair, 0, Value::Long(8));
        ctx.set_field(pair, 1, Value::Long(4));
        ctx.set_field(pair, 2, Value::Object(Some(struct_members)));
        ctx.set_field(pair, 3, Value::Object(None));
        let ucid = ctx
            .ensure_class_initialized("java/lang/foreign/UnionLayout")
            .unwrap();
        let members = ctx.new_ref_array(ClassId::new(1), 2);
        ctx.set_array_element(members, 0, Value::Object(Some(f)));
        ctx.set_array_element(members, 1, Value::Object(Some(pair)));
        let u = ctx.alloc_object(ucid, 4);
        ctx.set_field(u, 0, Value::Long(8));
        ctx.set_field(u, 1, Value::Long(4));
        ctx.set_field(u, 2, Value::Object(Some(members)));
        ctx.set_field(u, 3, Value::Object(None));

        let ty = union_aapcs64_hfa_ffi_type(&ctx, u, 8, 0).expect("an HFA union");
        // SAFETY: `ty` owns the struct type and its null-terminated elements.
        unsafe {
            let raw = &*ty.as_raw_ptr();
            assert!(!raw.elements.is_null());
            for k in 0..2 {
                let elem = *raw.elements.add(k);
                assert!(!elem.is_null());
                assert_eq!((*elem).type_ as u32, libffi::raw::FFI_TYPE_FLOAT);
            }
            assert!((*raw.elements.add(2)).is_null(), "exactly two elements");
        }
    }
}

/// JIT round 16 wave 16 (lane ffmlean; FFS16-4 / FFD16-1): a direct stub
/// passes and returns what `ffi_call` on the same CIF passes and returns.
#[cfg(all(test, target_arch = "x86_64"))]
mod r16w16_ffmlean_direct_stub_tests {
    use super::*;

    extern "C" fn t_none() -> i64 {
        77
    }
    extern "C" fn t_neg8(a: i8) -> i32 {
        -(a as i32)
    }
    extern "C" fn t_narrow_ret(a: i32) -> i8 {
        a as i8
    }
    extern "C" fn t_six(a: i8, b: i16, c: i32, d: i64, e: u16, f: *const i64) -> i64 {
        // SAFETY: the tests pass a live `i64`.
        let deref = unsafe { *f };
        a as i64 + 10 * b as i64 + 100 * c as i64 + 1000 * d + 10_000 * e as i64 + deref
    }
    extern "C" fn t_void(p: *mut i64) {
        // SAFETY: the tests pass a live `i64`.
        unsafe { *p = 5 };
    }

    /// One call both ways: `ffi_call` on a CIF of `kinds` -> `ret`, and the
    /// direct stub; answers the two unmarshalled results.
    fn both(fn_addr: usize, kinds: &[i32], args: &[Value], ret: Option<i32>) -> (Value, Value) {
        let types: Vec<FfiType> = kinds
            .iter()
            .map(|k| primitive_kind_to_ffi_type(*k).unwrap())
            .collect();
        let cif = Cif::new(
            types,
            ret.map_or_else(FfiType::void, |k| primitive_kind_to_ffi_type(k).unwrap()),
        );
        let mut cells = [0u64; DIRECT_STUB_MAX_ARGS];
        for (i, (kind, arg)) in kinds.iter().zip(args).enumerate() {
            if *kind == LAYOUT_ADDRESS {
                cells[i] = arg.as_long().unwrap() as u64;
            } else {
                assert!(marshal_primitive_into(*kind, arg, &mut cells[i]));
            }
        }
        let mut ptrs = [std::ptr::null_mut::<std::ffi::c_void>(); DIRECT_STUB_MAX_ARGS];
        for (p, c) in ptrs.iter_mut().zip(cells.iter_mut()) {
            *p = c as *mut u64 as *mut std::ffi::c_void;
        }
        let mut slot = [0u64; 2];
        // SAFETY: the CIF is the test function's own shape.
        unsafe {
            libffi::raw::ffi_call(
                cif.as_raw_ptr(),
                Some(
                    *libffi::middle::CodePtr::from_ptr(fn_addr as *const std::ffi::c_void)
                        .as_safe_fun(),
                ),
                slot.as_mut_ptr() as *mut std::ffi::c_void,
                ptrs.as_mut_ptr(),
            );
        }
        let mut regs = [0u64; DIRECT_STUB_MAX_ARGS];
        for (i, kind) in kinds.iter().enumerate() {
            regs[i] = direct_stub_widen(*kind, cells[i]);
        }
        // SAFETY: as above.
        let raw = unsafe { direct_stub_call(fn_addr, kinds.len(), &regs) }.unwrap();
        let read = |r: [u64; 2]| -> Value {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&r[0].to_ne_bytes());
            bytes[8..].copy_from_slice(&r[1].to_ne_bytes());
            ret.map_or(Value::Object(None), |k| {
                unmarshal_return_primitive(k, &bytes)
            })
        };
        (read(slot), read([raw, 0]))
    }

    #[test]
    fn direct_stub_answers_what_ffi_call_answers() {
        let (a, b) = both(t_none as usize, &[], &[], Some(LAYOUT_LONG));
        assert_eq!((a, b), (Value::Long(77), Value::Long(77)));
        for v in [-128, -5, 0, 5, 127] {
            let (a, b) = both(
                t_neg8 as usize,
                &[LAYOUT_BYTE],
                &[Value::Int(v)],
                Some(LAYOUT_INT),
            );
            assert_eq!(a, Value::Int(-v));
            assert_eq!(a, b, "byte {v}");
        }
        for v in [-1, 200, -129, 0x1234_5678] {
            let (a, b) = both(
                t_narrow_ret as usize,
                &[LAYOUT_INT],
                &[Value::Int(v)],
                Some(LAYOUT_BYTE),
            );
            assert_eq!(a, Value::Int(v as i8 as i32));
            assert_eq!(a, b, "narrow return {v}");
        }
        let cell = 3i64;
        let args = [
            Value::Int(-7),
            Value::Int(-300),
            Value::Int(-70_000),
            Value::Long(-9),
            Value::Int(0xFFFF),
            Value::Long(&cell as *const i64 as i64),
        ];
        let kinds = [
            LAYOUT_BYTE,
            LAYOUT_SHORT,
            LAYOUT_INT,
            LAYOUT_LONG,
            LAYOUT_CHAR,
            LAYOUT_ADDRESS,
        ];
        let (a, b) = both(t_six as usize, &kinds, &args, Some(LAYOUT_LONG));
        assert_eq!(a, b, "six arguments");
        let mut out = 0i64;
        let (a, b) = both(
            t_void as usize,
            &[LAYOUT_ADDRESS],
            &[Value::Long(&mut out as *mut i64 as i64)],
            None,
        );
        assert_eq!((a, b, out), (Value::Object(None), Value::Object(None), 5));
    }

    #[test]
    fn direct_stub_serves_integer_class_up_to_six_only() {
        assert!(direct_stubs_supported());
        for kind in [
            LAYOUT_BYTE,
            LAYOUT_BOOLEAN,
            LAYOUT_SHORT,
            LAYOUT_CHAR,
            LAYOUT_INT,
            LAYOUT_LONG,
            LAYOUT_ADDRESS,
        ] {
            assert!(direct_stub_kind_ok(kind));
        }
        assert!(!direct_stub_kind_ok(LAYOUT_FLOAT));
        assert!(!direct_stub_kind_ok(LAYOUT_DOUBLE));
        assert!(!direct_stub_kind_ok(LAYOUT_UNKNOWN));
        let regs = [0u64; DIRECT_STUB_MAX_ARGS];
        // SAFETY: an arity above six calls nothing.
        assert_eq!(unsafe { direct_stub_call(t_none as usize, 7, &regs) }, None);
        assert_eq!(direct_stub_widen(LAYOUT_BYTE, 0xFF), u64::MAX);
        assert_eq!(direct_stub_widen(LAYOUT_SHORT, 0xFFFF), u64::MAX);
        let char_bits = if char_arg_unsigned() {
            0xFFFF
        } else {
            u64::MAX
        };
        assert_eq!(direct_stub_widen(LAYOUT_CHAR, 0xFFFF), char_bits);
        assert_eq!(
            direct_stub_widen(LAYOUT_INT, 0x8000_0000),
            0xFFFF_FFFF_8000_0000
        );
        assert_eq!(direct_stub_widen(LAYOUT_LONG, 0x8000_0000), 0x8000_0000);
    }

    extern "C" fn t_mixed(a: f64, b: i32, c: f32, d: i64, e: f64) -> f64 {
        a * 1000.0 + f64::from(b) * 100.0 + f64::from(c) * 10.0 + d as f64 + e / 10.0
    }

    extern "C" fn t_float_ret(a: i32, b: f32) -> f32 {
        a as f32 + b
    }

    extern "C" fn t_int_ret(a: f64, b: i64) -> i64 {
        a as i64 * 10 + b
    }

    /// Round 19: the float-class stub passes interleaved integer and vector
    /// arguments, each class in its own parameter order, and reads a `double`
    /// or `float` return from `xmm0` (SysV x86-64 only).
    #[test]
    fn direct_stub_fp_passes_each_class_in_order() {
        assert!(direct_stub_fp_kind(LAYOUT_FLOAT) && direct_stub_fp_kind(LAYOUT_DOUBLE));
        assert!(!direct_stub_fp_kind(LAYOUT_INT) && !direct_stub_fp_kind(LAYOUT_ADDRESS));
        let mut ints = [0u64; DIRECT_STUB_MAX_ARGS];
        let mut fps = [0u64; DIRECT_STUB_MAX_FP_ARGS];
        ints[0] = direct_stub_widen(LAYOUT_INT, (-3i32) as u32 as u64);
        ints[1] = 7;
        fps[0] = 2.0f64.to_bits();
        fps[1] = u64::from(0.5f32.to_bits());
        fps[2] = 40.0f64.to_bits();
        // SAFETY: `t_mixed`'s own shape: two integer and three vector parameters.
        let got = unsafe { direct_stub_call_fp(t_mixed as usize, &ints, &fps, true) };
        if !direct_stub_fp_supported() {
            assert_eq!(got, None);
            return;
        }
        assert_eq!(
            f64::from_bits(got.unwrap()),
            2000.0 - 300.0 + 5.0 + 7.0 + 4.0
        );
        let mut ints = [0u64; DIRECT_STUB_MAX_ARGS];
        let mut fps = [0u64; DIRECT_STUB_MAX_FP_ARGS];
        ints[0] = 4;
        fps[0] = u64::from(0.25f32.to_bits());
        // SAFETY: `t_float_ret`'s shape.
        let got = unsafe { direct_stub_call_fp(t_float_ret as usize, &ints, &fps, true) }.unwrap();
        assert_eq!(f32::from_bits(got as u32), 4.25);
        fps[0] = 3.0f64.to_bits();
        ints[0] = 5;
        // SAFETY: `t_int_ret`'s shape.
        let got = unsafe { direct_stub_call_fp(t_int_ret as usize, &ints, &fps, false) }.unwrap();
        assert_eq!(got as i64, 35);
    }
}

/// JIT round 17 wave 2 (lane ffm3; EINTR17-1): the take-over signal's mask is
/// blocked for a counted call, unblocked for an up-call inside it, and each
/// guard puts back exactly the mask it found.
#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod r17w2_ffm3_takeover_mask_tests {
    use super::*;

    fn takeover_signal_blocked() -> bool {
        // SAFETY: a query of this thread's mask into a zeroed set.
        unsafe {
            let mut now: libc::sigset_t = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut now);
            libc::sigismember(&now, TAKEOVER_SIGNAL) == 1
        }
    }

    #[test]
    fn the_mask_guards_nest_and_restore() {
        assert!(!takeover_signal_blocked(), "a test thread starts unmasked");
        {
            let _off = TakeoverSignalMask::block(false);
            assert!(!takeover_signal_blocked(), "the switch off changes nothing");
        }
        {
            let _call = TakeoverSignalMask::block(true);
            assert!(takeover_signal_blocked());
            {
                let _upcall = TakeoverSignalMask::unblock(true);
                assert!(!takeover_signal_blocked());
            }
            assert!(takeover_signal_blocked(), "C resumes masked");
        }
        assert!(
            !takeover_signal_blocked(),
            "the call's guard restores the mask"
        );
    }
}

#[cfg(test)]
mod r19_arena_call_tests {
    //! Round 19 (`r16-ffm-plain-downcall-costs-microseconds`): an arena handle
    //! passed to C is the block's real storage, and a pointer C hands back
    //! into that block is the handle again, for exactly the call's span.
    use super::*;

    #[test]
    fn a_pointer_into_a_passed_block_maps_back_to_its_handle_inside_the_call_only() {
        let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
        let handle = crate::unsafe_natives_ext::unsafe_arena_try_allocate(64).expect("block");
        let real = {
            let _call = ArenaCallScope::open();
            let real = arena_call_address(handle + 8, 16).expect("live block");
            assert!(!is_arena_handle(real), "C gets a machine address");
            assert_eq!(arena_call_pointer_to_handle(real), handle + 8);
            assert_eq!(arena_call_pointer_to_handle(real + 4), handle + 12);
            // One past the end of the block still names it (`strchr` of "").
            assert_eq!(arena_call_pointer_to_handle(real + 56), handle + 64);
            // Not in any passed block: unchanged.
            assert_eq!(arena_call_pointer_to_handle(0x1000), 0x1000);
            {
                let _nested = ArenaCallScope::open();
                assert_eq!(
                    arena_call_pointer_to_handle(real),
                    handle + 8,
                    "outer blocks survive"
                );
            }
            assert_eq!(arena_call_pointer_to_handle(real), handle + 8);
            real
        };
        // After the call the block is still live, so its real address still
        // names it (a pointer read back out of memory); once freed, it does not.
        assert_eq!(arena_call_pointer_to_handle(real), handle + 8, "still live");
        // A block that cannot cover the length, and a freed handle, refuse.
        assert!(arena_call_address(handle + 60, 16).is_none());
        assert!(
            arena_call_address(handle + 64, 0).is_some(),
            "zero length at the end"
        );
        crate::unsafe_natives_ext::unsafe_arena_free(handle);
        assert!(arena_call_address(handle, 1).is_none());
        // A freed block's handle never comes back (its memory may already be
        // another live block's, which then names it: handles are never reused).
        // The translations above made outside any call recorded nothing.
        assert_ne!(arena_call_pointer_to_handle(real), handle + 8, "a freed block");
    }

    #[test]
    fn an_in_vm_segment_borrow_does_not_leak_a_native_translation() {
        let handle = crate::unsafe_natives_ext::unsafe_arena_try_allocate(16).expect("block");
        assert!(crate::unsafe_natives_ext::unsafe_arena_copy_in(
            handle,
            &[0x5a; 16]
        ));
        let before = crate::unsafe_natives_ext::unsafe_arena_translation_stats().translations;
        let real = borrowed_host_base_address(handle, 16);
        assert_ne!(real, 0, "live block resolves to its storage");
        // SAFETY: `handle` stays live through this synchronous access, and
        // `borrowed_host_base_address` checked the complete 16-byte range.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(real as *const u8, 16) },
            &[0x5a; 16]
        );
        assert_eq!(
            crate::unsafe_natives_ext::unsafe_arena_translation_stats().translations,
            before,
            "an in-VM borrow is not a native pointer publication"
        );
        crate::unsafe_natives_ext::unsafe_arena_free(handle);
    }
}

#[cfg(test)]
mod j27_ffmrows_mask_from_region_tests {
    //! FFM417-5 (j27 wave 4, lane FFMROWS): the take-over mask is decided from
    //! the region the downcall actually ENTERED, not from the one it asked for.
    use super::*;
    use crate::test_utils::mock_ctx;

    /// A lazy entry the context declines with no fallback leaves the call
    /// COUNTED, so the switch must mask it; the old `!lazy && switch` left it
    /// unmasked (a counted C call open to the take-over signal's `EINTR`).
    #[test]
    fn a_declined_lazy_entry_is_counted_and_so_masked() {
        let mut ctx = mock_ctx();
        let guard = ActiveContextGuard::install(&mut ctx);
        guard.enter_region_lazy(&mut ctx, None);
        assert_eq!(active_downcall_region(), DowncallRegion::Counted);
        assert!(downcall_masks_takeover(active_downcall_region(), true));
        assert!(!downcall_masks_takeover(active_downcall_region(), false));
        drop(guard);
    }

    /// An open region is excluded from pauses: never masked, switch or not.
    #[test]
    fn an_open_region_is_never_masked() {
        let mut ctx = mock_ctx();
        let guard = ActiveContextGuard::install(&mut ctx);
        guard.enter_region(&mut ctx, false);
        assert!(active_downcall_gc_safe());
        assert!(!downcall_masks_takeover(active_downcall_region(), true));
        guard.leave_region(&mut ctx);
        drop(guard);
    }
}
