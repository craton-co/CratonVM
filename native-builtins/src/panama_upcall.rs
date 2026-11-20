// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! FFM upcall stubs: `Linker.upcallStub` (JIT round 12 wave 7, lane `upcall`).
//!
//! # Why `upcallStub` was an `AbstractMethodError`
//!
//! `Linker.nativeLinker()` is intercepted (`phases_late/foreign_ffm.rs`) and
//! hands out an instance whose class is the `java.lang.foreign.Linker`
//! INTERFACE itself. Every `Linker` method that works on it works because a
//! `Bridge` native is registered on `java/lang/foreign/Linker` for that exact
//! triple and the JDK declaration is abstract (`vm_exec::resolve_dispatch`'s
//! "no `Code`, but something is registered" arm). `downcallHandle` has such a
//! row; `upcallStub` never had one. The only upcall row was
//! `Linker.upcallHandle(MethodHandle, FunctionDescriptor, Arena)`, a
//! pre-release spelling no JDK 22+ declares, registered only by the
//! synthetic-JDK registrar (`panama::register_pe_linker`), which neither
//! shipping binary runs.
//!
//! # How an upcall runs
//!
//! [`register_panama_upcall`] adds the missing `Bridge` row. The stub is a
//! libffi closure whose CIF is built from the `FunctionDescriptor`; its
//! callback, [`upcall_entry`], runs on the thread C calls it on. A same-thread
//! upcall happens inside a downcall, and `panama::pe_downcall_invoke` installs
//! its `NativeContext` in `panama_libffi`'s thread-local
//! (`ActiveContextGuard`) around `ffi_call`, so the callback re-enters Java
//! through that context: it decodes the C arguments per the descriptor
//! (an address becomes a native `MemorySegment` sized by the address layout's
//! target layout), invokes the `MethodHandle` the way the `invokeExact` door
//! does (`lang_invoke::mh_dispatch`), and encodes the result.
//!
//! An exception that escapes the target terminates the VM, as HotSpot's
//! `SharedUtils.handleUncaughtException` does: the stack trace, then
//! `Unrecoverable uncaught exception encountered. The VM will now exit`, then
//! `System.exit(1)`. A call on a thread with no active downcall (a foreign
//! thread, or a kept pointer called later) goes through the VM's
//! [`UpcallAttachHook`] (round 13), which runs it on the thread's own VM
//! context or attaches the thread; with no hook it terminates loudly instead
//! of returning a fabricated 0
//! (`r12w7-upcall-foreign-thread-upcalls-are-unsupported`).
//!
//! # Lifetime
//!
//! A stub made in a closeable arena records that arena's session. Once the
//! session's close walk has run (`foreign_ffm::p67_session_run_close_actions`
//! calls [`retire_upcall_stubs_of_session`]) the stub is dead: calling it
//! terminates the VM with a message (HotSpot frees the code in that walk, so a
//! call is undefined behaviour there), and its closure is freed at once, or by
//! the sweep at the next stub creation when an upcall of it was still
//! running. A stub of `Arena.ofAuto()` is freed by the sweep at a later stub
//! creation once a collection found its arena's session unreachable (a weak
//! handle, round 13 wave 9); one of `Arena.global()` lives until the process
//! exits.
//!
//! # Another VM's stub
//!
//! Two VMs in one process: a stub of VM B called inside a downcall of VM A
//! runs on a helper thread attached to B while A's thread waits
//! (`upcall_on_helper_thread`, round 13 wave 8).
//!
//! # State
//!
//! The registry is the process-wide map the libffi closures already lived in
//! (it moved here from `panama.rs`; nothing new is global). It is keyed by the
//! code address C holds, and every entry records its VM: the GC halves
//! (`panama::gc_scan_upcall_target_roots` / `gc_update_upcall_target_refs`)
//! root and remap only the collecting VM's targets and sessions, and the sweep
//! only frees the calling VM's stubs. Why it is not per VM:
//! `r12w7-upcall-residuals-FIXED-20260929.md`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use cratonvm_native_api::ffi::{
    LAYOUT_ADDRESS, LAYOUT_BOOLEAN, LAYOUT_BYTE, LAYOUT_CHAR, LAYOUT_DOUBLE, LAYOUT_FLOAT,
    LAYOUT_INT, LAYOUT_LONG, LAYOUT_PADDING, LAYOUT_SEQUENCE, LAYOUT_SHORT, LAYOUT_STRUCT,
    LAYOUT_UNION,
};
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};
use libffi::low::ffi_cif;
use libffi::middle::{Cif, Closure, Type};

use crate::panama_libffi as plf;
use crate::phases_late::foreign_ffm;

/// The class the upcall row is registered on: the interface `nativeLinker()`
/// stamps its linker with.
pub(crate) const LINKER_CLASS: &str = "java/lang/foreign/Linker";

/// `Linker.upcallStub(MethodHandle, FunctionDescriptor, Arena, Linker.Option...)`.
pub(crate) const UPCALL_STUB_DESCRIPTOR: &str = "(Ljava/lang/invoke/MethodHandle;Ljava/lang/foreign/FunctionDescriptor;Ljava/lang/foreign/Arena;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/foreign/MemorySegment;";

const SEGMENT_DESCRIPTOR: &str = "Ljava/lang/foreign/MemorySegment;";

/// Register `Linker.upcallStub`. States its own kind: a `Bridge`, the row the
/// abstract JDK declaration needs, and the same kind `downcallHandle` has.
/// Called from `register_essential_natives_with_shims` (see
/// `r12w7-upcall-register-upcallstub-in-the-essential-registrar-patch`).
///
/// The triple is spelled as literals, not through the constants above, so the
/// source-scanning registrar gates (`registrar_drift.rs`) can read it.
pub(crate) fn register_panama_upcall(r: &mut NativeMethodRegistry) {
            r.register(
        "java/lang/foreign/Linker",
        "upcallStub",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/foreign/FunctionDescriptor;Ljava/lang/foreign/Arena;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/foreign/MemorySegment;",
        crate::panama::pe_upcall_stub,
        NativeKind::Bridge,
    );
    // The address-less downcall (round 12 wave 8 orchestrator,
    // `r12w8-orch-probe-findings-on-w8` item 3): abstract on the interface
    // like `upcallStub`, and it had no row at all, so a real-JDK `Linker`
    // answered `AbstractMethodError ... has no Code attribute`.
    r.register(
        "java/lang/foreign/Linker",
        "downcallHandle",
        "(Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;",
        crate::panama::pe_downcall_handle_unbound,
        NativeKind::Bridge,
    );
    }

/// `CRATONVM_FFM_UPCALL_STUBS` (default on). Off makes `Linker.upcallStub`
/// throw `UnsupportedOperationException` instead of building a stub. Read per
/// stub creation, which is rare, so it needs no cache.
pub(crate) fn upcall_stubs_enabled() -> bool {
    true
}









// ---------------------------------------------------------------------------
// The shape of an upcall: what C passes and what it expects back
// ---------------------------------------------------------------------------

/// A C-side carrier an upcall stub supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpcallKind {
    Boolean,
    Byte,
    Short,
    Char,
    Int,
    Long,
    Float,
    Double,
    Address,
    /// A by-value struct or union (round 13 wave 11, lane ffm6): its JDK
    /// `byteSize` and `byteAlignment`. The Java side is a `MemorySegment`; the
    /// C side's libffi type is built from the layout at creation
    /// ([`upcall_shape_and_types`]), not from this kind.
    Group {
        size: u32,
        align: u32,
    },
}

impl UpcallKind {
    /// The kind for a `panama_libffi::read_layout_kind` answer, or `None` for
    /// a group, padding or unclassifiable layout.
    pub(crate) fn from_layout_kind(kind: i32) -> Option<Self> {
        Some(match kind {
            LAYOUT_BOOLEAN => Self::Boolean,
            LAYOUT_BYTE => Self::Byte,
            LAYOUT_SHORT => Self::Short,
            LAYOUT_CHAR => Self::Char,
            LAYOUT_INT => Self::Int,
            LAYOUT_LONG => Self::Long,
            LAYOUT_FLOAT => Self::Float,
            LAYOUT_DOUBLE => Self::Double,
            LAYOUT_ADDRESS => Self::Address,
            _ => return None,
        })
    }

    /// The JVM descriptor of the Java carrier (`FunctionDescriptor.toMethodType`).
    pub(crate) fn descriptor(self) -> &'static str {
        match self {
            Self::Boolean => "Z",
            Self::Byte => "B",
            Self::Short => "S",
            Self::Char => "C",
            Self::Int => "I",
            Self::Long => "J",
            Self::Float => "F",
            Self::Double => "D",
            Self::Address | Self::Group { .. } => SEGMENT_DESCRIPTOR,
        }
    }

    /// The libffi type of the C carrier. `char` and `boolean` are the unsigned
    /// C types (`uint16_t`, `bool`): what matters is how the closure glue
    /// widens the value it hands back, and a Java `char` is unsigned. `None`
    /// for a group, whose type is its layout's.
    fn ffi_type(self) -> Option<Type> {
        Some(match self {
            Self::Boolean => Type::u8(),
            Self::Byte => Type::i8(),
            Self::Short => Type::i16(),
            Self::Char => Type::u16(),
            Self::Int => Type::i32(),
            Self::Long => Type::i64(),
            Self::Float => Type::f32(),
            Self::Double => Type::f64(),
            Self::Address => Type::pointer(),
            Self::Group { .. } => return None,
        })
    }
}

/// One C parameter. `target_size` is the byte size of an address layout's
/// target layout (`ADDRESS.withTargetLayout(JAVA_INT)` gives 4), 0 without
/// one: the size of the segment the Java side receives, as on HotSpot.
/// `target_align` is that target layout's byte alignment, 1 without one (or
/// with [`upcall_address_align_check`] off): an address C passes that it does
/// not divide is HotSpot's `IllegalArgumentException` (round 13 wave 13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UpcallParam {
    pub(crate) kind: UpcallKind,
    pub(crate) target_size: i64,
    pub(crate) target_align: i64,
}

/// The parameters and the return of an upcall, `None` for `void`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpcallShape {
    pub(crate) params: Vec<UpcallParam>,
    pub(crate) ret: Option<UpcallKind>,
}

impl UpcallShape {
    /// The descriptor of `FunctionDescriptor.toMethodType()`.
    pub(crate) fn method_descriptor(&self) -> String {
        let mut s = String::from("(");
        for p in &self.params {
            s.push_str(p.kind.descriptor());
        }
        s.push(')');
        s.push_str(self.ret.map_or("V", UpcallKind::descriptor));
        s
    }
}

fn illegal_argument(message: String) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException { message }.into()
}

fn layout_class_name(ctx: &dyn NativeContext, layout: ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// The kind of one layout of an upcall descriptor, or the refusal.
fn upcall_kind(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    what: &str,
) -> Result<UpcallKind, MethodCallFailed> {
    let kind = plf::read_layout_kind(ctx, layout);
    if let Some(k) = UpcallKind::from_layout_kind(kind) {
        return Ok(k);
    }
    // Round 13 wave 11 (lane ffm6): a struct or union by value
    // (`CRATONVM_FFM_UPCALL_BY_VALUE_GROUPS`). A size of 0 (`structLayout()`,
    // which libffi cannot describe), one past the marshaller's bound, or none
    // readable keeps the refusal below.
    if matches!(kind, LAYOUT_STRUCT | LAYOUT_UNION) {
        let size = plf::layout_total_size(ctx, layout).unwrap_or(0);
        if size > 0 && size <= plf::MAX_STRUCT_BYTES {
            let align = plf::layout_align(ctx, layout).clamp(1, plf::MAX_STRUCT_BYTES);
            return Ok(UpcallKind::Group {
                size: size as u32,
                align: align as u32,
            });
        }
    }
    let class = layout_class_name(ctx, layout);
    if matches!(kind, LAYOUT_STRUCT | LAYOUT_UNION | LAYOUT_SEQUENCE) {
        // HotSpot supports these. A loud refusal at creation is better than a
        // stub that hands C a wrong frame; see the residuals page.
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!(
                "CratonVM upcall stubs do not support a by-value group layout as the {what} \
                 ({class}); pass it by address"
            ),
        }
        .into());
    }
    Err(illegal_argument(format!(
        "Unsupported layout for an upcall {what}: {class}"
    )))
}



/// `CRATONVM_FFM_DOWNCALL_CHECK_LAYOUTS` (default on; round 13 wave 9, lane
/// ffm5): `Linker.downcallHandle` runs [`check_linker_layouts`] before it reads
/// the options, as `AbstractLinker.downcallHandle0` runs `checkLayouts`. `0`
/// links a non-canonical value layout again (the call then marshals the other
/// byte order where HotSpot refuses to link). Read per handle creation.
pub(crate) fn downcall_check_layouts_enabled() -> bool {
    true
}

/// The letter `ValueLayouts` renders a carrier with, lower case.
fn kind_letter(kind: UpcallKind) -> char {
    match kind {
        UpcallKind::Boolean => 'z',
        UpcallKind::Byte => 'b',
        UpcallKind::Short => 's',
        UpcallKind::Char => 'c',
        UpcallKind::Int => 'i',
        UpcallKind::Long => 'j',
        UpcallKind::Float => 'f',
        UpcallKind::Double => 'd',
        UpcallKind::Address => 'a',
        // Never asked: only value layouts are rendered through this.
        UpcallKind::Group { .. } => '?',
    }
}

/// `AbstractLinker.checkLayouts` (JDK 25) for what a descriptor's top level
/// can hold: the return layout, then each argument layout. A sequence layout
/// is `IllegalArgumentException: Unsupported layout: <layout>`. A value layout
/// must, with its name (and an address layout's target) stripped, be the
/// linker's canonical layout for its carrier -- native byte order, natural
/// alignment -- else `Unsupported layout: <stripped layout>`, e.g. `I4` for
/// `JAVA_INT.withOrder(BIG_ENDIAN)` and `1%i4` for `JAVA_INT_UNALIGNED`
/// (`checkSupported`). Struct and union layouts, and every layout nested in
/// one, are checked by [`check_layout_recursive`] (round 13 wave 11, lane
/// ffm6; `CRATONVM_FFM_CHECK_LAYOUTS_GROUPS`).
/// Before this, such a value layout was accepted and the stub read or wrote
/// the other byte order where HotSpot refuses to link (round 13 wave 9, lane
/// ffm5). A kind-tagged carrier (slot 0 an `Int`) records no size or
/// alignment and is not checked. Plain reads.
pub(crate) fn check_linker_layouts(
    ctx: &dyn NativeContext,
    descriptor: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let ret = plf::descriptor_return_layout(ctx, descriptor);
    let params = plf::descriptor_param_layouts(ctx, descriptor);
    let groups = true;
    for layout in ret.into_iter().chain(params.into_iter().flatten()) {
        let raw_kind = plf::read_layout_kind(ctx, layout);
        if raw_kind == LAYOUT_SEQUENCE {
            return Err(illegal_argument(format!(
                "Unsupported layout: {}",
                foreign_ffm::p67_layout_render(ctx, layout)
            )));
        }
        if groups {
            check_layout_recursive(ctx, layout, 0)?;
        } else {
            check_value_layout(ctx, layout, raw_kind)?;
        }
    }
    Ok(())
}



/// `checkSupported` for one value layout (see [`check_linker_layouts`]). Any
/// other kind, and a carrier that records no size, passes.
fn check_value_layout(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    raw_kind: i32,
) -> Result<(), MethodCallFailed> {
    let Some(kind) = UpcallKind::from_layout_kind(raw_kind) else {
        return Ok(());
    };
    let slots = foreign_ffm::p67_layout_slots(ctx, layout);
    let Value::Long(size) = ctx.get_field(layout, slots.byte_size) else {
        return Ok(());
    };
    let align = match ctx.get_field(layout, slots.byte_alignment) {
        Value::Long(align) if align > 0 => align,
        _ => size.max(1),
    };
    let little = foreign_ffm::p67_layout_is_little(ctx, layout);
    if little == cfg!(target_endian = "little") && align == size {
        return Ok(());
    }
    let letter = kind_letter(kind);
    let letter = if little {
        letter
    } else {
        letter.to_ascii_uppercase()
    };
    let prefix = if align == size {
        String::new()
    } else {
        format!("{align}%")
    };
    Err(illegal_argument(format!(
        "Unsupported layout: {prefix}{letter}{size}"
    )))
}

/// `Utils.alignUp` for a power-of-two `align` and a non-negative `value`.
fn layout_align_up(value: i64, align: i64) -> i64 {
    if align <= 1 {
        return value;
    }
    value.saturating_add(align - 1) / align * align
}

/// The members of a group layout, in order (empty for `structLayout()` or a
/// carrier whose member list cannot be read).
fn group_member_layouts(ctx: &dyn NativeContext, layout: ObjectRef) -> Vec<ObjectRef> {
    match foreign_ffm::p67_group_members(ctx, layout) {
        Some((array, count)) => (0..count)
            .filter_map(|i| match foreign_ffm::p67_group_member_at(ctx, array, i) {
                Value::Object(Some(member)) => Some(member),
                _ => None,
            })
            .collect(),
        None => Vec::new(),
    }
}

/// JDK 25 `AbstractLinker.checkLayoutRecursive` (round 13 wave 11, lane
/// ffm6), with its messages. A carrier that records no size (the kind-tagged
/// test shape) is not checked, as in [`check_value_layout`]; nesting deeper
/// than `panama_libffi::MAX_LAYOUT_DEPTH` is left to the ffi-type builder,
/// which refuses it. Plain reads.
fn check_layout_recursive(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    depth: usize,
) -> Result<(), MethodCallFailed> {
    if depth > plf::MAX_LAYOUT_DEPTH {
        return Ok(());
    }
    let kind = plf::read_layout_kind(ctx, layout);
    let render = |l: ObjectRef| foreign_ffm::p67_layout_render(ctx, l);
    match kind {
        LAYOUT_STRUCT | LAYOUT_UNION | LAYOUT_SEQUENCE => {}
        LAYOUT_PADDING => return Ok(()),
        _ => return check_value_layout(ctx, layout, kind),
    }
    let Some((size, align)) = foreign_ffm::p67_member_size_align(ctx, layout) else {
        return Ok(());
    };
    if kind == LAYOUT_SEQUENCE {
        let slots = foreign_ffm::p67_layout_slots(ctx, layout);
        let Value::Object(Some(element)) = ctx.get_field(layout, slots.payload) else {
            return Ok(());
        };
        let element_align = foreign_ffm::p67_member_size_align(ctx, element).map_or(1, |(_, a)| a);
        if align != element_align {
            return Err(illegal_argument(format!(
                "Layout alignment must be natural alignment: {}",
                render(layout)
            )));
        }
        if plf::read_layout_kind(ctx, element) == LAYOUT_PADDING {
            return Err(illegal_argument(format!(
                "Member layout '{}', of '{}' not supported because a sequence of a padding \
                 layout is not allowed",
                render(element),
                render(layout)
            )));
        }
        return check_layout_recursive(ctx, element, depth + 1);
    }
    let members = group_member_layouts(ctx, layout);
    // Members this VM cannot read (a non-empty group whose list it cannot
    // decode, or a member carrier with no size) are not evidence against the
    // layout: a refusal HotSpot does not make would break a working program.
    if members.is_empty() && size > 0 {
        return Ok(());
    }
    let Some(size_align) = members
        .iter()
        .map(|m| foreign_ffm::p67_member_size_align(ctx, *m))
        .collect::<Option<Vec<(i64, i64)>>>()
    else {
        return Ok(());
    };
    let natural = size_align.iter().map(|&(_, a)| a).fold(1, i64::max);
    if align != natural {
        return Err(illegal_argument(format!(
            "Layout alignment must be natural alignment: {}",
            render(layout)
        )));
    }
    let is_padding = |m: ObjectRef| plf::read_layout_kind(ctx, m) == LAYOUT_PADDING;
    let max_unpadded = if kind == LAYOUT_STRUCT {
        let mut offset = 0i64;
        let mut last_unpadded = 0i64;
        let mut preceding_padding: Option<ObjectRef> = None;
        for (member, &(member_size, member_align)) in members.iter().zip(&size_align) {
            // The offset first, so an error names the outermost layout.
            let expected = layout_align_up(last_unpadded, member_align);
            if expected != offset {
                return Err(illegal_argument(format!(
                    "Member layout '{}', of '{}' found at unexpected offset: {offset} != \
                     {expected}",
                    render(*member),
                    render(layout)
                )));
            }
            check_layout_recursive(ctx, *member, depth + 1)?;
            offset = offset.saturating_add(member_size);
            if is_padding(*member) {
                if let Some(previous) = preceding_padding {
                    return Err(illegal_argument(format!(
                        "The padding layout {} was preceded by another padding layout {} in {}",
                        render(*member),
                        render(previous),
                        render(layout)
                    )));
                }
                preceding_padding = Some(*member);
            } else {
                last_unpadded = offset;
                preceding_padding = None;
            }
        }
        if !members.is_empty() && members.iter().all(|m| is_padding(*m)) {
            return Err(illegal_argument(format!(
                "Layout '{}' is non-empty and only has padding layouts",
                render(layout)
            )));
        }
        last_unpadded
    } else {
        let max_unpadded = members
            .iter()
            .zip(&size_align)
            .filter(|(m, _)| !is_padding(**m))
            .map(|(_, &(s, _))| s)
            .max()
            .unwrap_or(0);
        let mut has_padding = false;
        for (member, &(member_size, _)) in members.iter().zip(&size_align) {
            check_layout_recursive(ctx, *member, depth + 1)?;
            if is_padding(*member) {
                if has_padding {
                    return Err(illegal_argument(format!(
                        "More than one padding in {}",
                        render(layout)
                    )));
                }
                has_padding = true;
                if member_size <= max_unpadded {
                    return Err(illegal_argument(format!(
                        "Superfluous padding {} in {}",
                        render(*member),
                        render(layout)
                    )));
                }
            }
        }
        max_unpadded
    };
    // `checkGroup`: the trailing padding C would add must be in the layout.
    let expected = layout_align_up(max_unpadded, align);
    if size != expected {
        return Err(illegal_argument(format!(
            "Layout '{}' has unexpected size: {size} != {expected}",
            render(layout)
        )));
    }
    Ok(())
}

/// An address layout's target layout, if it has one.
fn address_target_layout(ctx: &dyn NativeContext, layout: ObjectRef) -> Option<ObjectRef> {
    let slot = foreign_ffm::p67_layout_slots(ctx, layout).target;
    if ctx.object_num_fields(layout) <= slot {
        return None;
    }
    match ctx.get_field(layout, slot) {
        Value::Object(Some(target)) => Some(target),
        _ => None,
    }
}

/// The byte size of an address layout's target layout, 0 without one.
pub(crate) fn address_target_size(ctx: &dyn NativeContext, layout: ObjectRef) -> i64 {
    address_target_layout(ctx, layout).map_or(0, |target| {
        foreign_ffm::p67_layout_size_of(ctx, target).max(0)
    })
}

/// `CRATONVM_FFM_UPCALL_ADDRESS_ALIGN_CHECK` (default on; round 13 wave 13,
/// lane ffm7): an address argument C hands an upcall is checked against its
/// target layout's alignment, as JDK 25 `Utils.longToAddress(addr, size,
/// align)` (the upcall's `BoxAddress` binding) checks it: a misaligned one is
/// `IllegalArgumentException: Invalid alignment constraint for address: 0x..`
/// inside the upcall, i.e. the uncaught-exception exit. `0` hands the target
/// the misaligned segment as before. Read per stub creation.
fn upcall_address_align_check() -> bool {
    true
}

/// The byte alignment of an address layout's target layout
/// (`Utils.pointeeByteAlign`), 1 without one or when the carrier's answer is
/// not a power of two (no evidence to refuse on).
pub(crate) fn address_target_align(ctx: &dyn NativeContext, layout: ObjectRef) -> i64 {
    let Some(target) = address_target_layout(ctx, layout) else {
        return 1;
    };
    let align = foreign_ffm::p67_layout_align_of(ctx, target);
    if align > 0 && (align as u64).is_power_of_two() {
        align
    } else {
        1
    }
}

/// Whether `address` breaks an address parameter's target alignment.
fn misaligned_address(address: i64, target_align: i64) -> bool {
    target_align > 1 && address & (target_align - 1) != 0
}

/// Read an upcall's shape off its `FunctionDescriptor` carrier (the same
/// field-0 return / field-1 parameter reads the downcall path makes).
#[cfg(test)]
pub(crate) fn upcall_shape(
    ctx: &dyn NativeContext,
    descriptor: ObjectRef,
) -> Result<UpcallShape, MethodCallFailed> {
    upcall_shape_and_types(ctx, descriptor).map(|(shape, _)| shape)
}

/// The libffi types of a stub's CIF: one per parameter, then the return.
pub(crate) struct UpcallFfiTypes {
    params: Vec<Type>,
    ret: Type,
}

/// The libffi type of one upcall carrier: the kind's own, or for a group the
/// struct type the downcall path builds from the same layout.
fn upcall_ffi_type(
    ctx: &dyn NativeContext,
    kind: UpcallKind,
    layout: ObjectRef,
) -> Result<Type, MethodCallFailed> {
    match kind.ffi_type() {
        Some(ty) => Ok(ty),
        None => plf::layout_to_ffi_type(ctx, layout, 0),
    }
}

/// An upcall's shape and the CIF types it needs (round 13 wave 11, lane ffm6:
/// a by-value group's libffi type comes from its layout).
pub(crate) fn upcall_shape_and_types(
    ctx: &dyn NativeContext,
    descriptor: ObjectRef,
) -> Result<(UpcallShape, UpcallFfiTypes), MethodCallFailed> {
    let mut params = Vec::new();
    let mut param_types = Vec::new();
    let align_check = upcall_address_align_check();
    for (i, layout) in plf::descriptor_param_layouts(ctx, descriptor)
        .into_iter()
        .enumerate()
    {
        let layout = layout
            .ok_or_else(|| illegal_argument(format!("Upcall parameter {i} layout is null")))?;
        let kind = upcall_kind(ctx, layout, "parameter")?;
        let (target_size, target_align) = if kind == UpcallKind::Address {
            let align = if align_check {
                address_target_align(ctx, layout)
            } else {
                1
            };
            (address_target_size(ctx, layout), align)
        } else {
            (0, 1)
        };
        params.push(UpcallParam {
            kind,
            target_size,
            target_align,
        });
        param_types.push(upcall_ffi_type(ctx, kind, layout)?);
    }
    let (ret, ret_type) = match plf::descriptor_return_layout(ctx, descriptor) {
        Some(layout) => {
            let kind = upcall_kind(ctx, layout, "return")?;
            (Some(kind), upcall_ffi_type(ctx, kind, layout)?)
        }
        None => (None, Type::void()),
    };
    Ok((
        UpcallShape { params, ret },
        UpcallFfiTypes {
            params: param_types,
            ret: ret_type,
        },
    ))
}

/// `target.type()`'s descriptor, read from the handle's `type` field, or
/// `None` when any step is not what a `MethodType` looks like. The flag says
/// whether every class in it resolved to a name: `mirror_to_descriptor`
/// answers `Object` for a mirror it cannot name, so without the flag an
/// `Object` is not evidence (round 13 wave 5, lane ffm3).
fn target_method_descriptor(ctx: &dyn NativeContext, target: ObjectRef) -> Option<(String, bool)> {
    let Value::Object(Some(mt)) = ctx.get_field_by_name(target, "type") else {
        return None;
    };
    let Value::Object(Some(ptypes)) = ctx.get_field_by_name(mt, "ptypes") else {
        return None;
    };
    if !ctx.object_is_array(ptypes) {
        return None;
    }
    let mut s = String::from("(");
    let mut named = true;
    for i in 0..ctx.array_length(ptypes) {
        let Value::Object(Some(p)) = ctx.get_array_element(ptypes, i) else {
            return None;
        };
        named &= crate::lang_invoke::resolve_class_name_robust(ctx, p).is_some();
        s.push_str(&crate::lang_invoke::mirror_to_descriptor(ctx, p));
    }
    s.push(')');
    let Value::Object(Some(r)) = ctx.get_field_by_name(mt, "rtype") else {
        return None;
    };
    named &= crate::lang_invoke::resolve_class_name_robust(ctx, r).is_some();
    s.push_str(&crate::lang_invoke::mirror_to_descriptor(ctx, r));
    Some((s, named))
}

/// Whether `actual` (the target's type) CERTAINLY differs from `expected`
/// (the descriptor's). Every way of not knowing is an accept: descriptors that
/// do not parse, and -- unless `object_is_evidence` (every mirror of the
/// target's type was named) -- a parameter or return read as `Object`, which
/// is what `mirror_to_descriptor` answers for a mirror it cannot name. A
/// missed refusal is the state before this check; a refusal HotSpot does not
/// make would break a working program.
pub(crate) fn upcall_type_mismatch(expected: &str, actual: &str, object_is_evidence: bool) -> bool {
    if expected == actual {
        return false;
    }
    let (Some((ep, er)), Some((ap, ar))) = (
        crate::lang_invoke::split_descriptor_params(expected),
        crate::lang_invoke::split_descriptor_params(actual),
    ) else {
        return false;
    };
    if ep.len() != ap.len() {
        return true;
    }
    ep.iter()
        .chain(std::iter::once(&er))
        .zip(ap.iter().chain(std::iter::once(&ar)))
        .any(|(e, a)| e != a && (object_is_evidence || a != "Ljava/lang/Object;"))
}

// ---------------------------------------------------------------------------
// Invoking the target
// ---------------------------------------------------------------------------

/// How a stub invokes its target: the target, the decoded arguments, the
/// shape (the generic path boxes against it).
pub(crate) type UpcallInvoker =
    fn(&mut dyn NativeContext, ObjectRef, &[Value], &UpcallShape) -> MethodCallResult;

/// A handle this VM's `MethodHandle` model minted (or a downcall handle):
/// dispatched exactly as the `invokeExact` door dispatches it. The arguments
/// already have the target's exact types, which the creation-time type check
/// establishes, so the door's conversions would all be identities.
fn invoke_synthetic(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    args: &[Value],
    _shape: &UpcallShape,
) -> MethodCallResult {
    crate::lang_invoke::mh_dispatch(ctx, target, args)
}

/// [`invoke_synthetic`] at the descriptor's call site (round 15 wave 5, lane
/// mh4, applying `r15w5-upcall1-synthetic-upcall-dispatch-has-no-call-site-patch`;
/// [`upcall_synthetic_site_enabled`]): HotSpot calls the target `invokeExact`
/// at `FunctionDescriptor.toMethodType()`, and the leaf boxes a raw primitive
/// for a reference parameter by that site's type
/// (`lang_invoke::leaf_box_raw_for_reference`).
fn invoke_synthetic_at_site(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    args: &[Value],
    shape: &UpcallShape,
) -> MethodCallResult {
    crate::lang_invoke::mh_dispatch_at_site(ctx, target, &shape.method_descriptor(), args)
}

/// `CRATONVM_FFM_UPCALL_SYNTHETIC_SITE` (default on; round 15 wave 5, lane
/// mh4): a stub over a synthetic handle dispatches it at the descriptor's call
/// site ([`invoke_synthetic_at_site`]). `0` takes the bare [`invoke_synthetic`].
/// Read per stub creation.
fn upcall_synthetic_site_enabled() -> bool {
    true
}

/// Any other handle: through `invokeWithArguments`, with the primitives boxed
/// against the descriptor. The boxes and the array allocate, so the target
/// and every reference argument are pinned until the array holds them.
fn invoke_generic(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    args: &[Value],
    shape: &UpcallShape,
) -> MethodCallResult {
    let base = ctx.pin_native_root(target);
    let mut pins: Vec<Option<usize>> = Vec::with_capacity(args.len());
    for value in args {
        pins.push(match value {
            Value::Object(Some(obj)) => Some(ctx.pin_native_root(*obj)),
            _ => None,
        });
    }
    // `invokeWithArguments(Object...)`: a genuine `Object[]`, allocated with
    // its component (`typed_array_producer_ratchet.rs`).
    let object_class = crate::lang_class::reflection_component_id(ctx, "java/lang/Object");
    // zs s3/nd3 (`upcall_args_collect`): on ZGC the array and the boxes are
    // minted with a collection before the `OutOfMemoryError`; the target and
    // every reference argument are pinned above, the array below.
    let collecting = upcall_args_collect(ctx);
    let array = if collecting {
        match generic_args_array_collecting(ctx, object_class, args.len()) {
            Ok(array) => array,
            Err(err) => {
                ctx.unpin_native_roots(base);
                return Err(err);
            }
        }
    } else {
        ctx.new_ref_array(object_class, args.len())
    };
    let array_pin = ctx.pin_native_root(array);
    for (i, value) in args.iter().enumerate() {
        let element = match (*value, pins.get(i).copied().flatten()) {
            (Value::Object(Some(obj)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(pin, obj)))
            }
            (Value::Object(None), _) => Value::Object(None),
            (primitive, _) => {
                let desc = shape.params.get(i).map_or("I", |p| p.kind.descriptor());
                if collecting {
                    match box_value_collecting(ctx, primitive, desc) {
                        Ok(boxed) => boxed,
                        Err(err) => {
                            ctx.unpin_native_roots(base);
                            return Err(err);
                        }
                    }
                } else {
                    crate::lang_class::box_value(ctx, primitive, desc)
                }
            }
        };
        let array = ctx.read_native_pin(array_pin, array);
        ctx.set_array_element(array, i, element);
    }
    let target = ctx.read_native_pin(base, target);
    let array = ctx.read_native_pin(array_pin, array);
    ctx.unpin_native_roots(base);
    ctx.invoke_virtual(
        target,
        "invokeWithArguments",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(array))],
    )
}

/// The generic invoker's `Object[]` through the fallible door and the reclaim
/// ladder (zs s3/nd3, [`upcall_args_collect`]); a spent ladder is the
/// catchable `OutOfMemoryError`. The caller holds no unpinned reference.
fn generic_args_array_collecting(
    ctx: &mut dyn NativeContext,
    component: cratonvm_types::ClassId,
    len: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(array) = ctx.try_new_ref_array(component, len) {
        return Ok(array);
    }
    crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
        ctx.try_new_ref_array(component, len)
    })
    .ok_or_else(|| crate::util_concurrent_ext::native_heap_oom("java.lang.Object[]"))
}

/// [`crate::lang_class::box_value`] (a fresh wrapper, the same answer) through
/// the fallible door and the reclaim ladder (zs s3/nd3,
/// [`upcall_args_collect`]). The wrapper's one field is its value, as
/// `box_value` writes it. The caller holds no unpinned reference.
fn box_value_collecting(
    ctx: &mut dyn NativeContext,
    value: Value,
    desc: &str,
) -> Result<Value, MethodCallFailed> {
    let class = match desc {
        "I" => "java/lang/Integer",
        "J" => "java/lang/Long",
        "F" => "java/lang/Float",
        "D" => "java/lang/Double",
        "Z" => "java/lang/Boolean",
        "B" => "java/lang/Byte",
        "S" => "java/lang/Short",
        "C" => "java/lang/Character",
        "V" => return Ok(Value::Object(None)),
        _ => return Ok(value),
    };
    // `alloc_wrapper`'s resolution (class id 0 when the class cannot be
    // initialised, its synthetic fallback).
    let class_id = ctx
        .ensure_class_initialized(class)
        .unwrap_or_else(|_| cratonvm_types::ClassId::new(0));
    let boxed = match ctx.try_alloc_object(class_id, 1) {
        Some(obj) => obj,
        None => crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
            ctx.try_alloc_object(class_id, 1)
        })
        .ok_or_else(|| crate::util_concurrent_ext::native_heap_oom(class))?,
    };
    ctx.set_field(boxed, 0, value);
    Ok(Value::Object(Some(boxed)))
}

/// [`invoke_synthetic`] when `lang_invoke::mh_dispatch` can run `target`: it
/// carries `MH_CLASS` (without it `mh_dispatch` answers `null` for everything)
/// or is a downcall handle. Otherwise [`invoke_generic`].
///
/// Round 15 wave 5 (lane upcall1): an `MH_CLASS` handle takes
/// [`invoke_synthetic`] only when its type is PROVEN to be the descriptor's
/// (`exact`: `target_method_descriptor` read it and it equals
/// `shape.method_descriptor()`, which is also what HotSpot requires). That
/// invoker hands the decoded C values over RAW, and a leaf's argument
/// conversion passes a raw primitive to a REFERENCE parameter unchanged
/// (`lang_invoke::adapt_leaf_arg` converts only toward a primitive), so a
/// member typed `(Object)` under an `(int)` upcall would read an `int`'s bits
/// as an object reference. The type check accepts every target whose type it
/// cannot read or name (and all of them with `CRATONVM_FFM_UPCALL_TYPE_CHECK=0`),
/// so it does not rule that out. Such a stub goes through
/// `invokeWithArguments` with the primitives boxed ([`invoke_generic`]), which
/// converts or refuses.
///
/// JIT round 16 wave 9 (lane ffm, proposal UP1-1): any other handle whose type
/// is PROVEN to be the descriptor's takes [`invoke_exact_at_site`] with
/// the default-on `CRATONVM_FFM_UPCALL_EXACT_DOOR` (see [`invoker_choice`]).
fn choose_invoker(ctx: &dyn NativeContext, target: ObjectRef, exact: bool) -> UpcallInvoker {
    let downcall = crate::panama::is_downcall_handle(ctx, target);
    // Read only when it can decide (a field read per stub creation).
    // Round 19: a REAL JDK handle (every `--real-jdk` target) has no model
    // slots, and reading `MH_CLASS` off one warned `field index OOB`.
    let model_handle = !downcall
        && !crate::lang_invoke::mh_is_real_handle(ctx, target)
        && crate::lang_invoke::mh_read_class(ctx, target).is_some();
    let choice = invoker_choice(
        downcall,
        model_handle,
        exact,
        true,
        upcall_synthetic_site_enabled(),
        true,
    );
    // The positive control for the switches above: which road each stub took
    // (`CRATONVM_DBG_MH_DISPATCH=1`).
    if crate::nbflags().dbg_mh_dispatch {
        eprintln!(
            "[FFM_UPCALL_INVOKER] {choice:?} (downcall={downcall} model={model_handle} exact={exact})"
        );
    }
    match choice {
        InvokerChoice::Synthetic => invoke_synthetic as UpcallInvoker,
        InvokerChoice::SyntheticAtSite => invoke_synthetic_at_site as UpcallInvoker,
        InvokerChoice::ExactDoor => invoke_exact_at_site as UpcallInvoker,
        InvokerChoice::Generic => invoke_generic as UpcallInvoker,
    }
}

/// Which invoker [`choose_invoker`] gives a stub (a pure function of what it
/// read, for the unit tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InvokerChoice {
    Synthetic,
    SyntheticAtSite,
    ExactDoor,
    Generic,
}

/// The rule behind [`choose_invoker`]: a downcall handle, or a model
/// (`MH_CLASS`) handle whose type is proven (or with
/// `CRATONVM_FFM_UPCALL_UNPROVEN_TYPE_GENERIC=0`, any model handle), goes to
/// `mh_dispatch` (at the descriptor's site unless
/// `CRATONVM_FFM_UPCALL_SYNTHETIC_SITE=0`). Every other handle goes through
/// `invokeWithArguments` -- unless its type is proven and the exact door is on
/// (UP1-1), when it is invoked `invokeExact` at the descriptor's site.
fn invoker_choice(
    downcall: bool,
    model_handle: bool,
    exact: bool,
    unproven_generic: bool,
    synthetic_site: bool,
    exact_door: bool,
) -> InvokerChoice {
    let synthetic_fits = exact || !unproven_generic;
    if downcall || (synthetic_fits && model_handle) {
        if synthetic_site {
            InvokerChoice::SyntheticAtSite
        } else {
            InvokerChoice::Synthetic
        }
    } else if exact && exact_door {
        InvokerChoice::ExactDoor
    } else {
        InvokerChoice::Generic
    }
}

/// UP1-1 (JIT round 16 wave 9, lane ffm; `jit-r15w5-upcall1-proposals-RETIRED-20261003.md`):
/// HotSpot's `UpcallLinker` invokes the target `invokeExact` at
/// `FunctionDescriptor.toMethodType()`. For a handle that is not this VM's
/// model (no `MH_CLASS`) and whose `type()` was read and EQUALS the
/// descriptor's (`exact`, the only way [`invoker_choice`] picks this), the
/// same call: the `MethodHandle.invokeExact` door with the descriptor as its
/// call site (armed by the VM's signature-polymorphic dispatch,
/// `arms_poly_call_site`), the decoded C values passed as they are. No
/// `Object[]`, no box per primitive argument, no `invokeWithArguments`
/// spreading, and the `invokeExact` type check cannot refuse (the types are
/// equal). A return comes back typed or boxed; [`return_bits`] takes both.
fn invoke_exact_at_site(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    args: &[Value],
    shape: &UpcallShape,
) -> MethodCallResult {
    ctx.invoke_virtual(target, "invokeExact", &shape.method_descriptor(), args)
}





// ---------------------------------------------------------------------------
// The stub registry
// ---------------------------------------------------------------------------

/// What an upcall stub's closure reads on every call.
///
/// `target` and `session` are heap addresses kept as atomics so the GC remap
/// (`gc_update_refs`) can rewrite them in place; both are roots through
/// `gc_scan_roots`. The trampoline loads them at a point where this thread is
/// not at a safepoint, and the remap runs stop-the-world, so `Relaxed` is
/// enough.
pub(crate) struct UpcallStubData {
    target: AtomicUsize,
    /// The closeable arena's session, or 0 for an arena that never closes.
    session: AtomicUsize,
    /// Where the session keeps its state word (`p67_session_slots(..).state`,
    /// a property of the class, so it cannot go stale). `Int(0)` is closed.
    session_state_slot: usize,
    /// The `vm_identity` whose heap `target` and `session` point into.
    vm: usize,
    shape: UpcallShape,
    invoker: UpcallInvoker,
    /// Upcalls of this stub currently running. The sweep never frees a stub
    /// with one in flight, so an upcall that closes its own stub's arena and
    /// then creates a stub does not free the code it is returning into.
    in_flight: AtomicUsize,
    /// The arena's close walk has run (round 13 wave 8, lane ffm4): the close
    /// is final. A state word of 0 alone is not, because a shared close that
    /// finds an acquired session reopens it (`foreign_ffm::p67_session_just_close`).
    retired: std::sync::atomic::AtomicBool,
    /// A WEAK global handle on a non-closeable arena's session, 0 without one
    /// (round 13 wave 9, lane ffm5; [`upcall_auto_arena_free`]). Not a root.
    auto_session: AtomicUsize,
    /// For messages: the descriptor's `MethodType` descriptor.
    label: String,
    /// Set once at creation when the target is a plain direct static handle
    /// ([`DirectStatic`]); empty otherwise.
    direct: std::sync::OnceLock<DirectStatic>,
    /// j27 wave 5 (lane FFMUP): this VM's segment mint for the address
    /// arguments, resolved at the first up-call that could ([`mint_address_args_batched`];
    /// class ids, slots and the door choice only -- no heap reference).
    mint: std::sync::OnceLock<crate::panama::JdkSegmentMint>,
}

/// Round 14 wave 1 (lane ffm; `r13w13-ffm7-upcall-per-call-cost`): a stub
/// whose target is a plain direct static handle
/// (`lang_invoke::mh_plain_static_leaf`) whose descriptor is the stub's own,
/// invoked straight through `invoke_static_settling` -- the call
/// `mh_dispatch`'s static arm makes for it, without re-reading three strings,
/// the markings and the `type` on every upcall. No heap references: the
/// target handle stays the stub's root either way.
struct DirectStatic {
    class: String,
    name: String,
    desc: String,
    owner: Option<cratonvm_types::ClassId>,
    /// `invoke_static_settling`'s settled owner, 0 before.
    settled: std::sync::atomic::AtomicU32,
    /// The member's parameter count (round 15 wave 5, lane upcall1): an upcall
    /// takes this door only with exactly this many decoded arguments. This
    /// door does not pass through `lang_invoke::adapt_leaf_args`, whose
    /// short-arity refusal is what keeps a member from being entered with a
    /// missing parameter (the VM fills every local past the arguments with the
    /// uninitialized marker, which an `aload` then reads as a reference:
    /// `r15w5-mh4-review-findings` item 7). The creation filter already makes
    /// `desc` the stub's own descriptor, so the count cannot differ today; the
    /// check makes that local, and a mismatch takes the invoker, which refuses.
    params: usize,
    /// j27 wave 1 (lane FFMVH): the target is a REAL JDK `DirectMethodHandle`
    /// ([`upcall_real_direct_static`]). While a debugger observes frames such
    /// a stub takes its invoker, whose `invokeExact` door runs the JDK's forms
    /// (as `lang_invoke::mh_real_direct_fast` declines then).
    real: bool,
}

/// `CRATONVM_FFM_UPCALL_DIRECT_STATIC_REAL` (j27 wave 1, lane FFMVH; perf,
/// default ON, `=0` restores the `invokeExact` door for every real handle;
/// needs `CRATONVM_FFM_UPCALL_DIRECT_STATIC`): a stub whose target is a REAL
/// JDK `DirectMethodHandle` of a `REF_invokeStatic` member called at its own
/// type -- what `MethodHandles.lookup().findStatic(..)` returns under
/// `--real-jdk` -- gets a [`DirectStatic`] too, read off its `MemberName`
/// once at creation (`lang_invoke::mh_real_direct_member`). Before, only this
/// model's carrier handles qualified (`mh_plain_static_leaf` refuses a real
/// handle), so every `--real-jdk` up-call paid the whole `invokeExact` door
/// (`mh_real_direct_before_linker`, the lane customization checks, the real
/// handle record) to reach the same `invoke_static_settling`
/// (page `ffm-upcall-per-call-costs-microseconds-FIXED-20261102.md`, 74% of the
/// up-call). Read per stub creation.
fn upcall_direct_static_real_enabled() -> bool {
    true
}

/// [`upcall_direct_static_real_enabled`]'s admission: `(class, name, desc,
/// owner)` of a real `DirectMethodHandle` whose member is `REF_invokeStatic`
/// (not a constructor, never `$Special` / virtual / interface dispatch) and
/// whose `type()` is the member's own descriptor (no adapter, nothing bound:
/// a caller-sensitive member's handle is a bound caller wrapper, not a
/// `DirectMethodHandle`, and is refused here). The member's class
/// initialization is `invoke_static_settling`'s, as on [`DirectStatic`]'s
/// carrier arm. Pure reads (the real-handle record memo).
fn upcall_real_direct_static(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
) -> Option<(String, String, String, Option<cratonvm_types::ClassId>)> {
    if !upcall_direct_static_real_enabled() {
        return None;
    }
    let member = crate::lang_invoke::mh_real_direct_member(ctx, target)?;
    if member.ref_kind != 6 || member.declared != member.desc || member.class_id == 0 {
        return None;
    }
    Some((
        member.class,
        member.name,
        member.desc,
        Some(cratonvm_types::ClassId::new(member.class_id)),
    ))
}

/// Whether a plain direct static member with descriptor `desc` may serve the
/// stub of `shape` through [`DirectStatic`]: the stub's own descriptor, hence
/// its own parameter count. Round 15 wave 5 (lane upcall1): the count is
/// compared as well, so the door's memory safety does not rest on two
/// renderings of one descriptor agreeing.
fn direct_static_fits(desc: &str, shape: &UpcallShape) -> bool {
    desc == shape.method_descriptor()
        && crate::lang_invoke::split_descriptor_params(desc)
            .is_some_and(|(params, _)| params.len() == shape.params.len())
}



/// `CRATONVM_FFM_UPCALL_RETIRE_AT_CLOSE` (default on; round 13 wave 8, lane
/// ffm4): an upcall stub is dead -- refused when called, freed by the sweep --
/// only once its arena's close walk has run
/// ([`retire_upcall_stubs_of_session`]), which is where HotSpot frees it (a
/// `ResourceCleanup` in the arena's list). `0` restores round 12's test, the
/// session's state word, which also reads 0 during a shared close that then
/// fails with `Session is acquired by N clients` and reopens the session: a
/// stub freed in that window was used after free once the session reopened.
fn upcall_retire_at_close() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}



/// `CRATONVM_FFM_UPCALL_AUTO_ARENA_FREE` (default on; round 13 wave 9, lane
/// ffm5): a stub made in a non-closeable arena (`Arena.ofAuto()`, and
/// `Arena.global()`, whose session never dies) keeps a WEAK handle on the
/// arena's session, and the sweep at the next `Linker.upcallStub` frees the
/// stub once a collection has found that session unreachable -- the arena and
/// every segment of it, the stub's own included, are gone. That is when
/// HotSpot's `ImplicitSession` cleaner frees it. `0` keeps such stubs until
/// the process exits, as before. Read per stub creation.
fn upcall_auto_arena_free() -> bool {
    true
}

impl UpcallStubData {
    /// The session's state word reads closed. `false` for a stub with no
    /// closeable session.
    fn session_reads_closed(&self, ctx: &dyn NativeContext) -> bool {
        let session = self.session.load(Ordering::Relaxed);
        if session == 0 {
            return false;
        }
        // SAFETY: a non-zero `session` was stored from a live `ObjectRef` of
        // this VM (every caller filters by VM or checks it first) and is kept
        // alive and remapped by `gc_scan_roots` / `gc_update_refs`.
        let session = unsafe { ObjectRef::from_raw(session as *mut u8) };
        matches!(
            ctx.get_field(session, self.session_state_slot),
            Value::Int(0)
        )
    }

    /// The stub is dead: its arena's close is final (see
    /// [`upcall_retire_at_close`]).
    fn dead(&self, ctx: &dyn NativeContext) -> bool {
        if upcall_retire_at_close() {
            self.retired.load(Ordering::SeqCst)
        } else {
            self.session_reads_closed(ctx)
        }
    }

    /// A non-closeable arena's stub whose session a collection found
    /// unreachable ([`upcall_auto_arena_free`]), and which nothing is running.
    fn auto_reclaimable(&self, ctx: &dyn NativeContext) -> bool {
        let handle = self.auto_session.load(Ordering::SeqCst);
        handle != 0
            && self.in_flight.load(Ordering::SeqCst) == 0
            && ctx.resolve_global_root(handle).is_none()
    }

    /// Dead, and nothing is running it.
    fn reclaimable(&self, ctx: &dyn NativeContext) -> bool {
        self.session.load(Ordering::Relaxed) != 0
            && self.in_flight.load(Ordering::SeqCst) == 0
            && self.dead(ctx)
    }
}

/// A registry entry: the closure (which owns the executable trampoline) and
/// the userdata it reads, freed in that order by `Drop`.
struct UpcallStubEntry {
    closure: Option<Closure<'static>>,
    data: *mut UpcallStubData,
    vm: usize,
}

// SAFETY: the closure is immutable after construction and its trampoline page
// is not tied to a thread; `data` is only read (through atomics where it
// changes) until `Drop`, which runs after the entry has left the registry.
unsafe impl Send for UpcallStubEntry {}
unsafe impl Sync for UpcallStubEntry {}

impl UpcallStubEntry {
    fn data(&self) -> &UpcallStubData {
        // SAFETY: `data` came from `Box::into_raw` and is freed only by `Drop`.
        unsafe { &*self.data }
    }
}

impl Drop for UpcallStubEntry {
    fn drop(&mut self) {
        // The closure first: it is the only other holder of `data`.
        drop(self.closure.take());
        // SAFETY: `data` came from `Box::into_raw` in `install_upcall_stub_typed`,
        // nothing else frees it, and the closure that read it is gone.
        unsafe { drop(Box::from_raw(self.data)) };
    }
}

// addr-keyed: not a heap address -- the upcall stub's native code address.
type Registry = parking_lot::Mutex<std::collections::HashMap<usize, UpcallStubEntry>>;

/// Code address -> stub. Pre-existing process state (it was
/// `panama::UPCALL_REGISTRY`), VM-tagged per entry.
static UPCALL_STUBS: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();

fn registry() -> &'static Registry {
    UPCALL_STUBS.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// GC root scan: every target and session of `vm`'s stubs. Companion of
/// [`gc_update_refs`]; the two visit the same set. The registry lock is a leaf
/// (nothing allocates or runs Java under it).
pub(crate) fn gc_scan_roots(vm: usize, out: &mut Vec<ObjectRef>) {
    let reg = registry().lock();
    for entry in reg.values().filter(|e| e.vm == vm) {
        let data = entry.data();
        for cell in [&data.target, &data.session] {
            let addr = cell.load(Ordering::Relaxed);
            if addr != 0 {
                // SAFETY: a non-zero address stored from a live `ObjectRef` of
                // this VM and kept current by `gc_update_refs`; used as a root.
                out.push(unsafe { ObjectRef::from_raw(addr as *mut u8) });
            }
        }
    }
}

/// GC remap: rewrite `vm`'s stub targets and sessions after a move.
pub(crate) fn gc_update_refs(vm: usize, map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let reg = registry().lock();
    for entry in reg.values().filter(|e| e.vm == vm) {
        let data = entry.data();
        for cell in [&data.target, &data.session] {
            let old = cell.load(Ordering::Relaxed);
            if old == 0 {
                continue;
            }
            if let Some(&new) = map.get(&old) {
                cell.store(new, Ordering::Relaxed);
            }
        }
    }
}

/// The close walk of `session` has run (`foreign_ffm::p67_session_run_close_actions`,
/// after `justClose` succeeded): mark every stub of the calling VM made in
/// that session's arena dead, then free the dead ones no upcall is running.
/// Answers how many were freed. No Java, no allocation: `session` is current
/// because nothing between the caller's read and this compare can move it.
pub(crate) fn retire_upcall_stubs_of_session(ctx: &dyn NativeContext, session: ObjectRef) -> usize {
    let vm = ctx.vm_identity();
    let addr = session.as_ptr() as usize;
    {
        let reg = registry().lock();
        for entry in reg.values().filter(|e| e.vm == vm) {
            let data = entry.data();
            if addr != 0 && data.session.load(Ordering::Relaxed) == addr {
                data.retired.store(true, Ordering::SeqCst);
            }
        }
    }
    reclaim_closed_upcall_stubs(ctx)
}

/// Free every stub of the calling VM that is dead (its arena's close walk ran;
/// see [`upcall_retire_at_close`]) and which no upcall is running. Answers how
/// many were freed. Runs at each stub creation and at the end of every close
/// walk ([`retire_upcall_stubs_of_session`]).
pub(crate) fn reclaim_closed_upcall_stubs(ctx: &dyn NativeContext) -> usize {
    let vm = ctx.vm_identity();
    let dead: Vec<UpcallStubEntry> = {
        let mut reg = registry().lock();
        let keys: Vec<usize> = reg
            .iter()
            .filter(|(_, e)| e.vm == vm && e.data().reclaimable(ctx))
            .map(|(k, _)| *k)
            .collect();
        keys.into_iter().filter_map(|k| reg.remove(&k)).collect()
    };
    // Dropped outside the lock: `closure_free` is not Java, but it need not
    // run under a lock the collector takes.
    let freed = dead.len();
    drop(dead);
    freed
}

/// Free every stub of the calling VM made in a non-closeable arena whose
/// session is gone and which no upcall is running, and release their weak
/// handles (round 13 wave 9, lane ffm5; [`upcall_auto_arena_free`]). Answers
/// how many were freed. Runs at each stub creation. The weak handles are
/// resolved under the registry lock, as the closed-arena sweep reads session
/// fields there: a resolve is a table lookup, no Java and no allocation, and
/// doing it outside would let a freed stub's code address and handle both be
/// reused by a new, live stub between the resolve and the removal.
pub(crate) fn reclaim_auto_upcall_stubs(ctx: &mut dyn NativeContext) -> usize {
    let vm = ctx.vm_identity();
    let dead: Vec<UpcallStubEntry> = {
        let mut reg = registry().lock();
        let keys: Vec<usize> = reg
            .iter()
            .filter(|(_, e)| e.vm == vm && e.data().auto_reclaimable(&*ctx))
            .map(|(k, _)| *k)
            .collect();
        keys.into_iter().filter_map(|k| reg.remove(&k)).collect()
    };
    // Outside the lock: the entries are ours now.
    for entry in &dead {
        let handle = entry.data().auto_session.load(Ordering::SeqCst);
        if handle != 0 {
            ctx.remove_global_root(handle);
        }
    }
    let freed = dead.len();
    drop(dead);
    freed
}

/// Give the stub at `code` its weak session handle. `false` when no such stub
/// is registered (the caller then releases `handle`).
fn set_upcall_stub_auto_session(code: usize, handle: usize) -> bool {
    match registry().lock().get(&code) {
        Some(entry) => {
            entry.data().auto_session.store(handle, Ordering::SeqCst);
            true
        }
        None => false,
    }
}

/// Whether a stub with this code address is registered (for tests and for the
/// close-path patch's own test).
pub(crate) fn upcall_stub_registered(code: usize) -> bool {
    registry().lock().contains_key(&code)
}

/// [`install_upcall_stub_typed`] with each carrier's own libffi type (a group
/// becomes a struct of its bytes). For tests, which build shapes by hand.
#[cfg(test)]
pub(crate) fn install_upcall_stub(
    ctx: &dyn NativeContext,
    target: ObjectRef,
    shape: UpcallShape,
    session: Option<(ObjectRef, usize)>,
    invoker: UpcallInvoker,
) -> usize {
    fn test_type(kind: UpcallKind) -> Type {
        match (kind.ffi_type(), kind) {
            (Some(ty), _) => ty,
            (None, UpcallKind::Group { size, .. }) => {
                Type::structure((0..size).map(|_| Type::u8()).collect::<Vec<_>>())
            }
            (None, _) => Type::pointer(),
        }
    }
    let types = UpcallFfiTypes {
        params: shape.params.iter().map(|p| test_type(p.kind)).collect(),
        ret: shape.ret.map_or_else(Type::void, test_type),
    };
    install_upcall_stub_typed(ctx, target, shape, types, session, invoker)
}

/// Build the closure for `shape` over `types`, register it, and answer its
/// code address.
///
/// No allocation and no Java: `target` and `session` are stored as roots
/// before anything can move them.
pub(crate) fn install_upcall_stub_typed(
    ctx: &dyn NativeContext,
    target: ObjectRef,
    shape: UpcallShape,
    types: UpcallFfiTypes,
    session: Option<(ObjectRef, usize)>,
    invoker: UpcallInvoker,
) -> usize {
    let cif = Cif::new(types.params, types.ret);
    let group_return = matches!(shape.ret, Some(UpcallKind::Group { .. }));
    let vm = ctx.vm_identity();
    let (session_addr, session_state_slot) = match session {
        Some((s, slot)) => (s.as_ptr() as usize, slot),
        None => (0, 0),
    };
    let label = shape.method_descriptor();
    let data = Box::into_raw(Box::new(UpcallStubData {
        target: AtomicUsize::new(target.as_ptr() as usize),
        session: AtomicUsize::new(session_addr),
        session_state_slot,
        vm,
        shape,
        invoker,
        in_flight: AtomicUsize::new(0),
        retired: std::sync::atomic::AtomicBool::new(false),
        auto_session: AtomicUsize::new(0),
        label,
        direct: std::sync::OnceLock::new(),
        mint: std::sync::OnceLock::new(),
    }));
    // SAFETY: `data` stays allocated until the registry entry that owns it is
    // dropped, and that drop frees the closure (the only reader) first.
    let data_ref: &'static UpcallStubData = unsafe { &*data };
    // A group return is written as the struct's bytes, never as an `ffi_arg`
    // (round 13 wave 11, lane ffm6): see `upcall_entry_group`.
    let closure: Closure<'static> = if group_return {
        Closure::new(cif, upcall_entry_group, data_ref)
    } else {
        Closure::new(cif, upcall_entry, data_ref)
    };
    let code = *closure.code_ptr() as *const () as usize;
    let replaced = registry().lock().insert(
        code,
        UpcallStubEntry {
            closure: Some(closure),
            data,
            vm,
        },
    );
    // libffi never hands out a live closure's address twice; a stale entry
    // here would be one whose closure was already freed. Dropped outside the
    // lock either way.
    drop(replaced);
    code
}

/// Unregister and free the stub at `code`, if there is one.
pub(crate) fn remove_upcall_stub(code: usize) {
    let removed = registry().lock().remove(&code);
    drop(removed);
}

// ---------------------------------------------------------------------------
// Creating a stub: `Linker.upcallStub`
// ---------------------------------------------------------------------------

/// The arena's session when it is one this VM models, and whether the arena
/// can be closed. Plain reads.
fn modelled_arena_session(ctx: &dyn NativeContext, arena: ObjectRef) -> Option<(ObjectRef, bool)> {
    let slots = foreign_ffm::p67_arena_slots(ctx, arena);
    let width = ctx.object_num_fields(arena);
    if width <= slots.session {
        return None;
    }
    let Value::Object(Some(session)) = ctx.get_field(arena, slots.session) else {
        return None;
    };
    if !crate::panama::pe_session_modelled(ctx, session) {
        return None;
    }
    let closeable =
        width > slots.closeable && matches!(ctx.get_field(arena, slots.closeable), Value::Int(1));
    Some((session, closeable))
}

/// The body of `Linker.upcallStub` after `panama::pe_upcall_stub`'s gates
/// (native access, the null checks, the capability row, the kill switch).
/// HotSpot's order from there (`AbstractLinker.upcallStub`): the layouts, the
/// options, the type check, then the arena (`addOrCleanupIfFail` refuses a
/// closed one).
pub(crate) fn create_upcall_stub(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    descriptor: ObjectRef,
    arena: ObjectRef,
    options: Option<ObjectRef>,
) -> MethodCallResult {
    // Every argument is pinned for the whole creation and read back through
    // its pin at each use. The stub stores `target` (and the session) RAW in
    // its registry, where a stale address is a root nothing remaps: the
    // up-call later invokes whatever object reuses it. Several steps below
    // take `&mut ctx`, and any of them that reaches a GC point would leave a
    // raw copy stale (`R22FfmUpcallChurn`; HotSpot's stub holds a JNI global).
    let pins = UpcallCreatePins {
        target: ctx.pin_native_root(target),
        descriptor: ctx.pin_native_root(descriptor),
        arena: ctx.pin_native_root(arena),
        options: options.map(|o| ctx.pin_native_root(o)),
        refs: (target, descriptor, arena, options),
    };
    let result = create_upcall_stub_pinned(ctx, &pins);
    ctx.unpin_native_roots(pins.target);
    result
}

/// [`create_upcall_stub`]'s arguments, pinned in this order (so `target`'s
/// pin is the base that releases all of them).
struct UpcallCreatePins {
    target: usize,
    descriptor: usize,
    arena: usize,
    options: Option<usize>,
    refs: (ObjectRef, ObjectRef, ObjectRef, Option<ObjectRef>),
}

impl UpcallCreatePins {
    fn target(&self, ctx: &dyn NativeContext) -> ObjectRef {
        ctx.read_native_pin(self.target, self.refs.0)
    }
    fn descriptor(&self, ctx: &dyn NativeContext) -> ObjectRef {
        ctx.read_native_pin(self.descriptor, self.refs.1)
    }
    fn arena(&self, ctx: &dyn NativeContext) -> ObjectRef {
        ctx.read_native_pin(self.arena, self.refs.2)
    }
    fn options(&self, ctx: &dyn NativeContext) -> Option<ObjectRef> {
        self.options
            .zip(self.refs.3)
            .map(|(pin, obj)| ctx.read_native_pin(pin, obj))
    }
}

fn create_upcall_stub_pinned(
    ctx: &mut dyn NativeContext,
    pins: &UpcallCreatePins,
) -> MethodCallResult {
    // `checkLayouts(function)` comes first (round 13 wave 9, lane ffm5).
    {
        check_linker_layouts(ctx, pins.descriptor(&*ctx))?;
    }
    let (shape, ffi_types) = upcall_shape_and_types(ctx, pins.descriptor(&*ctx))?;
    // `SharedUtils.checkExceptions(target)` (round 13 wave 1, lane mhffm): a
    // direct handle whose member declares ANY exception (`throws` clause,
    // checked or not) is refused; an adapted or bound handle is "unknown"
    // and passes, as on HotSpot.
    {
        let target = pins.target(&*ctx);
        if let Some(exceptions) = crate::lang_invoke::mh_direct_member_exceptions(ctx, target) {
            if !exceptions.is_empty() {
                let shown: Vec<String> = exceptions
                    .iter()
                    .map(|name| format!("class {}", name.replace('/', ".")))
                    .collect();
                return Err(illegal_argument(format!(
                    "Target handle may throw exceptions: [{}]",
                    shown.join(", ")
                )));
            }
        }
    }
    // `LinkerOptions.forUpcall`: every JDK 25 option refuses an upcall.
    if let Some(options) = pins.options(&*ctx) {
        if ctx.object_is_array(options) && ctx.array_length(options) > 0 {
            return match ctx.get_array_element(options, 0) {
                Value::Object(Some(option)) => Err(illegal_argument(format!(
                    "Not supported for upcall: {}",
                    layout_class_name(ctx, option).replace('/', ".")
                ))),
                _ => Err(RuntimeError::NullPointerException { message: None }.into()),
            };
        }
    }
    // Plain reads (no GC point) off the current `target`.
    let target = pins.target(&*ctx);
    let target_type = target_method_descriptor(ctx, target);
    let exact = target_type
        .as_ref()
        .is_some_and(|(actual, _)| *actual == shape.method_descriptor());
    let invoker = choose_invoker(ctx, target, exact);
    {
        if let Some((actual, named)) = target_type {
            let object_is_evidence = named;
            if upcall_type_mismatch(&shape.method_descriptor(), &actual, object_is_evidence) {
                return Err(illegal_argument(format!(
                    "Wrong method handle type: {actual} (the descriptor needs {})",
                    shape.method_descriptor()
                )));
            }
        }
    }
    // Round 13 wave 9 (lane ffm5): stubs of collected `Arena.ofAuto()` arenas.
    reclaim_auto_upcall_stubs(ctx);
    // Round 13 wave 9 (lane ffm5): an access window from before the arena's
    // liveness check until the stub is in the registry
    // (`CRATONVM_FFM_UPCALL_CREATE_WINDOW`). A SHARED close on another thread
    // flips the state, waits for every window open at its probe, and only
    // then walks the list that retires the arena's stubs. Without the window
    // a stub whose check passed just before that close was installed after
    // the walk: never retired, never freed, callable after the close. With
    // it, the close either waits for the install (and its walk retires the
    // stub) or this check reads the arena as closed and throws. Nothing under
    // it runs Java; it is dropped before the segment allocation. The validity
    // check can raise (and so allocate).
    let create_window = true.then(crate::ffm_fast::AccessWindow::open);
    if let Some((mut session, _)) = modelled_arena_session(ctx, pins.arena(&*ctx)) {
        foreign_ffm::p67_session_check_valid(ctx, &mut session)?;
    }
    reclaim_closed_upcall_stubs(ctx);
    // Round 14 wave 1 (lane ffm): field and string reads only.
    // j27 wave 1 (lane FFMVH): or a REAL `findStatic` `DirectMethodHandle`
    // (`upcall_real_direct_static`), the target every `--real-jdk` up-call has.
    let direct = {
        let target = pins.target(&*ctx);
        crate::lang_invoke::mh_plain_static_leaf(ctx, target)
            .map(|leaf| (leaf, false))
            .or_else(|| upcall_real_direct_static(ctx, target).map(|leaf| (leaf, true)))
            .filter(|((_, _, desc, _), _)| direct_static_fits(desc, &shape))
    };
    // The raw references the install stores, read last: plain reads from
    // here to the registry insert.
    let session = match modelled_arena_session(ctx, pins.arena(&*ctx)) {
        Some((session, true)) => {
            let state_slot = foreign_ffm::p67_session_slots(ctx, session).state;
            Some((session, state_slot))
        }
        _ => None,
    };
    let target = pins.target(&*ctx);
    let code = install_upcall_stub_typed(ctx, target, shape, ffi_types, session, invoker);
    if let Some(((class, name, desc, owner), real)) = direct {
        // The MEMBER's count (equal to the stub's, `direct_static_fits`).
        let params = crate::lang_invoke::split_descriptor_params(&desc)
            .map_or(usize::MAX, |(params, _)| params.len());
        // Before the stub is handed to Java: nothing can call it yet.
        if let Some(entry) = registry().lock().get(&code) {
            let _ = entry.data().direct.set(DirectStatic {
                class,
                name,
                desc,
                owner,
                settled: std::sync::atomic::AtomicU32::new(0),
                params,
                real,
            });
        }
    }
    drop(create_window);

    // The only allocation: the target and the session are registry roots by
    // now, and the arena is still pinned.
    let segment = match crate::panama::alloc_segment_carrier(ctx, 6) {
        Ok(segment) => segment,
        Err(err) => {
            remove_upcall_stub(code);
            return Err(err);
        }
    };
    let arena = pins.arena(&*ctx);
    // Round 13 wave 9 (lane ffm5): a non-closeable arena's stub is freed once
    // its session is collected (`CRATONVM_FFM_UPCALL_AUTO_ARENA_FREE`). The
    // session is read after the allocation; nothing below moves it.
    if upcall_auto_arena_free() {
        if let Some((session, false)) = modelled_arena_session(ctx, arena) {
            let handle = ctx.add_weak_global_root(session);
            if handle != 0 && !set_upcall_stub_auto_session(code, handle) {
                ctx.remove_global_root(handle);
            }
        }
    }
    // `MemorySegment.ofAddress(entry).reinterpret(arena, null)`: zero length,
    // scoped to the arena (slot 2, so a downcall handed the stub after the
    // close refuses it), not read-only. The arena's session when it has one
    // (round 13 wave 5: `panama::pe_carrier_scope`).
    let scope = crate::panama::pe_carrier_scope(ctx, arena);
    ctx.set_field(segment, 0, Value::Long(code as i64));
    ctx.set_field(segment, 1, Value::Long(0));
    ctx.set_field(segment, 2, Value::Object(Some(scope)));
    ctx.set_field(segment, 3, Value::Int(0));
    ctx.set_field(segment, 4, Value::Int(1));
    ctx.set_field(segment, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(segment))))
}

// ---------------------------------------------------------------------------
// Running an upcall
// ---------------------------------------------------------------------------

/// Why an upcall cannot return to C normally.
#[derive(Debug)]
pub(crate) enum UpcallFatal {
    /// The target threw, or its result does not convert: HotSpot terminates.
    Uncaught(MethodCallFailed),
    /// CratonVM cannot run this upcall at all.
    Unsupported(String),
}

/// Read one C argument.
///
/// # Safety
/// `slot` must point at a value of the C type `kind` maps to (libffi hands the
/// closure one such pointer per CIF parameter).
unsafe fn decode_arg(kind: UpcallKind, slot: *const c_void) -> Value {
    // SAFETY: the caller's contract; unaligned reads cost nothing here and
    // drop an assumption about the closure glue's spill slots.
    unsafe {
        match kind {
            UpcallKind::Boolean => {
                Value::Int(i32::from(std::ptr::read_unaligned(slot as *const u8) != 0))
            }
            UpcallKind::Byte => Value::Int(i32::from(std::ptr::read_unaligned(slot as *const i8))),
            UpcallKind::Short => {
                Value::Int(i32::from(std::ptr::read_unaligned(slot as *const i16)))
            }
            UpcallKind::Char => Value::Int(i32::from(std::ptr::read_unaligned(slot as *const u16))),
            UpcallKind::Int => Value::Int(std::ptr::read_unaligned(slot as *const i32)),
            UpcallKind::Long => Value::Long(std::ptr::read_unaligned(slot as *const i64)),
            UpcallKind::Float => Value::Float(std::ptr::read_unaligned(slot as *const f32)),
            UpcallKind::Double => Value::Double(std::ptr::read_unaligned(slot as *const f64)),
            UpcallKind::Address => {
                Value::Long(std::ptr::read_unaligned(slot as *const usize) as i64)
            }
            // The struct's bytes are AT `slot` (libffi hands a closure a
            // pointer to each by-value aggregate); the caller copies them.
            UpcallKind::Group { .. } => Value::Long(slot as usize as i64),
        }
    }
}

/// Up-calls with at most this many parameters decode them into a stack buffer
/// ([`upcall_dispatch`]) and may mint their address arguments in one batch
/// ([`mint_address_args_batched`]).
const INLINE_ARGS: usize = 8;

/// `CRATONVM_FFM_UPCALL_MINT_BATCH` (j27 wave 5, lane FFMUP; perf, default
/// ON, `=0` restores the per-argument [`new_arg_segment`] loop): see
/// [`mint_address_args_batched`]. Asked per up-call: memoised.
// WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
fn upcall_mint_batch_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// Whether [`mint_address_args_batched`] may serve a stub with `params`: the
/// switch is on, every parameter fits the stack buffer, and none is a
/// by-value group (whose copy has its own arena and pins).
fn mint_batch_fits(params: &[UpcallParam]) -> bool {
    params.len() <= INLINE_ARGS
        && !params.iter().any(|p| matches!(p.kind, UpcallKind::Group { .. }))
        && upcall_mint_batch_enabled()
}

/// j27 wave 5 (lane FFMUP; page `ffm-upcall-per-call-costs-microseconds-FIXED-20261102.md`
/// item 2): every address argument of an up-call made into its segment, as
/// [`new_arg_segment`] makes each one, but through ONE
/// [`crate::panama::JdkSegmentMint`]: the mode check, the shape, the door
/// choice and `MemorySessionImpl.GLOBAL_SESSION` are read once per up-call,
/// not once per argument. The segments are allocated first (each a GC point;
/// the session and every segment so far stay pinned), then initialised
/// together with no GC point in between. Values in `values` are replaced by
/// the segments, in the argument order HotSpot converts them (a misaligned
/// address fails at its own position, after the earlier mints).
///
/// `None`: not taken ([`mint_batch_fits`] refuses, not the JDK-segment mode,
/// or the session is not there yet): the caller's per-argument loop runs, so
/// the carrier fallback is unchanged. `Some(Err)`: the up-call's uncaught
/// exception; nothing is left pinned.
fn mint_address_args_batched(
    ctx: &mut dyn NativeContext,
    cached: &std::sync::OnceLock<crate::panama::JdkSegmentMint>,
    params: &[UpcallParam],
    values: &mut [Value],
) -> Option<Result<(), MethodCallFailed>> {
    if !mint_batch_fits(params) {
        return None;
    }
    let mint = stub_segment_mint(ctx, cached)?;
    let global = mint.global(ctx)?;
    let base = ctx.pin_native_root(global);
    // (argument index, pin, address, size, segment)
    let mut made = [(0usize, 0usize, 0i64, 0i64, global); INLINE_ARGS];
    let mut count = 0;
    for (i, param) in params.iter().enumerate() {
        let (UpcallKind::Address, Value::Long(raw)) = (param.kind, values[i]) else {
            continue;
        };
        let segment = if misaligned_address(raw, param.target_align) {
            Err(illegal_argument(format!(
                "Invalid alignment constraint for address: 0x{raw:x}"
            )))
        } else {
            mint.alloc(ctx)
        };
        let segment = match segment {
            Ok(segment) => segment,
            Err(err) => {
                ctx.unpin_native_roots(base);
                return Some(Err(err));
            }
        };
        let pin = ctx.pin_native_root(segment);
        let address = crate::panama_libffi::arena_call_pointer_to_handle(raw);
        made[count] = (i, pin, address, param.target_size, segment);
        count += 1;
    }
    let global = ctx.read_native_pin(base, global);
    for &(i, pin, address, size, segment) in &made[..count] {
        let segment = ctx.read_native_pin(pin, segment);
        mint.init(ctx, segment, address, size, global);
        values[i] = Value::Object(Some(segment));
    }
    ctx.unpin_native_roots(base);
    Some(Ok(()))
}

/// Called only after a successful batch. Groups and unbatched/carrier
/// fallbacks cannot provide this fact; primitives already have a cheap key.
fn batched_first_class(params: &[UpcallParam], mint: Option<&crate::panama::JdkSegmentMint>)
    -> Option<cratonvm_types::ClassId> {
    params.first().filter(|p| matches!(p.kind, UpcallKind::Address))?;
    Some(mint?.class_id())
}

/// The stub's [`crate::panama::JdkSegmentMint`], resolved once: the mode,
/// the shape and the door choice are per-VM constants (the stub belongs to one
/// VM). A `None` (not this mode, classes not there yet) is not kept, so a
/// stub created before the classes resolve asks again.
fn stub_segment_mint(
    ctx: &dyn NativeContext,
    cached: &std::sync::OnceLock<crate::panama::JdkSegmentMint>,
) -> Option<crate::panama::JdkSegmentMint> {
    if let Some(mint) = cached.get() {
        return Some(*mint);
    }
    let mint = crate::panama::jdk_segment_mint(ctx, crate::panama::ffm_mint_collects(ctx))?;
    Some(*cached.get_or_init(|| mint))
}

/// The native segment an address argument becomes: `size` bytes at `address`,
/// no arena (always alive), as `Utils.longToAddress` makes it on HotSpot.
fn new_arg_segment(
    ctx: &mut dyn NativeContext,
    address: i64,
    size: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    // Round 19 (`r16-ffm-plain-downcall-costs-microseconds`): a pointer into
    // an arena block the enclosing downcall passed (`qsort`'s element
    // pointers) is that block's handle, as Java names it; under `--real-jdk`
    // the JDK's own global-scope `NativeMemorySegmentImpl`.
    let address = crate::panama_libffi::arena_call_pointer_to_handle(address);
    // zs s2/nd2 (`zgc-zs1nd-jit-helper-native-doors-20261014.md` item 2): the
    // caller (`upcall_dispatch`) holds no unpinned reference here -- the
    // arguments are raw C values and every segment made so far is pinned --
    // so on ZGC a refused mint collects and retries, then answers the
    // `OutOfMemoryError` (the upcall's uncaught exception, as on HotSpot,
    // whose `Utils.longToAddress` allocates in Java). The one-attempt door
    // aborted the process from a JIT-run downcall (permission cleared) and
    // unwound through the `extern "C"` closure entry from an interpreted one.
    // `CRATONVM_ZGC_FFM_MINT_COLLECT=0`, Generational and G1: as before.
    let collecting = crate::panama::ffm_mint_collects(ctx);
    if let Some(segment) = crate::panama::pe_jdk_native_segment_with(ctx, address, size, collecting)
    {
        return segment;
    }
    let segment = crate::panama::alloc_segment_carrier_with(ctx, 6, collecting)?;
    ctx.set_field(segment, 0, Value::Long(address));
    ctx.set_field(segment, 1, Value::Long(size));
    ctx.set_field(segment, 2, Value::Object(None));
    ctx.set_field(segment, 3, Value::Int(0));
    ctx.set_field(segment, 4, Value::Int(1));
    ctx.set_field(segment, 5, Value::Long(0));
    Ok(segment)
}

/// Round 15 wave 6 (lane sweepC; `r15w5-upcall1-review-findings` item 4): an
/// `Address` upcall's result that is not a `MemorySegment`. `invoke_generic`
/// (`invokeWithArguments`, return `Object`) serves a target whose type was not
/// proven, so it can hand back any object, and `segment_address` would then
/// give C a field of it as a pointer. HotSpot's target type is exact, so the
/// cast is what it would have failed: `ClassCastException: Cannot cast <class>
/// to java.lang.foreign.MemorySegment`, reported as the upcall's uncaught
/// exception. Refused only on a definite answer (`Some(false)` and no
/// synthetic `implements`; CratonVM's own carriers never are); the switch
/// `CRATONVM_FFM_UPCALL_RETURN_SEGMENT_CAST` is read only on a refusal.
fn upcall_return_not_a_segment(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Option<MethodCallFailed> {
    let class_id = ctx.class_id_of_object(obj);
    let assignable = ctx.class_assignable_to_name(class_id, crate::panama::PE_SEGMENT_INTERFACE);
    if assignable != Some(false) {
        // A real segment (or a context that cannot tell): no name lookup.
        return None;
    }
    let class_name = ctx.class_name_of_id(class_id)?;
    if !upcall_return_class_refused(assignable, &class_name)
        || ctx.synthetic_implements_declared(class_id, crate::panama::PE_SEGMENT_INTERFACE)
    {
        return None;
    }
    Some(
        RuntimeError::ClassCastException {
            message: format!(
                "Cannot cast {} to java.lang.foreign.MemorySegment",
                class_name.replace('/', ".")
            ),
        }
        .into(),
    )
}

/// The class half of [`upcall_return_not_a_segment`]: only a definite
/// "not assignable", and never CratonVM's own segment stamps.
fn upcall_return_class_refused(assignable: Option<bool>, class_name: &str) -> bool {
    assignable == Some(false)
        && class_name != crate::panama::CRATON_SEGMENT_CLASS
        && class_name != crate::panama::PE_SEGMENT_INTERFACE
}

/// The C return value for `value`, widened to a full `ffi_arg` as libffi
/// requires of a closure (sign-extended for the signed types, zero-extended
/// for `char` and `boolean`). A boxed result is unboxed first: an adapter may
/// hand one back.
pub(crate) fn return_bits(
    ctx: &dyn NativeContext,
    kind: UpcallKind,
    value: Value,
) -> Result<u64, MethodCallFailed> {
    let value = match value {
        Value::Object(Some(obj)) if kind != UpcallKind::Address => {
            crate::lang_class::unbox_value(ctx, obj)
        }
        other => other,
    };
    Ok(match (kind, value) {
        (UpcallKind::Boolean, Value::Int(n)) => (n & 1) as u64,
        (UpcallKind::Byte, Value::Int(n)) => n as i8 as i64 as u64,
        (UpcallKind::Short, Value::Int(n)) => n as i16 as i64 as u64,
        (UpcallKind::Char, Value::Int(n)) => u64::from(n as u16),
        (UpcallKind::Int, Value::Int(n)) => n as i64 as u64,
        (UpcallKind::Long, Value::Long(n)) => n as u64,
        (UpcallKind::Float, Value::Float(f)) => u64::from(f.to_bits()),
        (UpcallKind::Double, Value::Double(d)) => d.to_bits(),
        (UpcallKind::Address, Value::Object(Some(segment))) => {
            if let Some(refusal) = upcall_return_not_a_segment(ctx, segment) {
                return Err(refusal);
            }
            // `SharedUtils.unboxSegment`: a heap segment has no address, and
            // the refusal is `checkNative`'s text (j27 wave 4, lane FFMMSG).
            if crate::panama::is_alias_heap_segment(ctx, segment)
                || plf::is_real_heap_segment(ctx, segment)
            {
                return Err(crate::panama::heap_segment_not_allowed(ctx, segment));
            }
            let address = plf::segment_address(ctx, segment);
            plf::checked_foreign_addr(address, "upcall return")? as u64
        }
        (UpcallKind::Address, Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())
        }
        (kind, other) => {
            return Err(RuntimeError::IllegalStateException {
                message: format!("upcall target returned {other:?} for a {kind:?} return"),
            }
            .into())
        }
    })
}

/// Where a by-value group return goes (round 13 wave 11, lane ffm6): libffi's
/// result buffer and its length (`cif->rtype->size`, never less than the
/// layout's `byteSize`, which is what is copied).
#[derive(Clone, Copy)]
struct GroupReturn {
    ptr: *mut u8,
    len: usize,
}

/// A by-value group argument as the target sees it: `size` bytes copied from
/// `src` into a block of the per-upcall confined arena, created (and pinned,
/// the pin becoming `pin_base` when it is the first) on the first group
/// argument. HotSpot copies into a `newBoundedArena` frame closed after the
/// upcall, so a segment the target keeps is `Already closed` afterwards.
///
/// # Safety
/// `src` must point at `size` readable bytes (libffi's copy of the aggregate,
/// whose C size is at least the layout's `byteSize`).
#[allow(clippy::too_many_arguments)]
unsafe fn new_group_arg_segment(
    ctx: &mut dyn NativeContext,
    arena: &mut Option<(usize, ObjectRef)>,
    pin_base: &mut Option<usize>,
    src: *const u8,
    size: u32,
    align: u32,
    collecting: bool,
    blocks: &mut Vec<i64>,
) -> Result<ObjectRef, MethodCallFailed> {
    let current = match *arena {
        Some((pin, made)) => ctx.read_native_pin(pin, made),
        None => {
            let made = foreign_ffm::p67_new_vm_confined_arena_with(ctx, collecting)?;
            let pin = ctx.pin_native_root(made);
            if pin_base.is_none() {
                *pin_base = Some(pin);
            }
            *arena = Some((pin, made));
            made
        }
    };
    // zs s3/nd3: the collecting arm (`upcall_args_collect`) skips the
    // session's block record; `upcall_dispatch` frees `blocks` after the close.
    let allocated = if collecting {
        upcall_group_block(ctx, current, size, align, blocks)?
    } else {
        crate::panama::pe_arena_allocate(
            ctx,
            &[
                Value::Object(Some(current)),
                Value::Long(i64::from(size)),
                Value::Long(i64::from(align)),
            ],
        )?
    };
    let Some(Value::Object(Some(segment))) = allocated else {
        return Err(RuntimeError::IllegalStateException {
            message: "upcall: no segment for a by-value group argument".into(),
        }
        .into());
    };
    let address = plf::segment_address(ctx, segment);
    if address == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "upcall: a by-value group argument's segment has no address".into(),        }
        .into());
    }
    // SAFETY: the caller's contract for `src`; `address` is a fresh native
    // block of `size` bytes this arena owns until it is closed.
    unsafe { std::ptr::copy_nonoverlapping(src, address as usize as *mut u8, size as usize) };
    Ok(segment)
}

/// zs s3/nd3 (`zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md` §1): may the
/// upcall's argument mints beyond the address argument -- a by-value GROUP
/// argument's per-upcall arena and segment, and the generic invoker's
/// `Object[]` and boxes -- collect and retry on a refused allocation? Only
/// where [`crate::panama::ffm_mint_collects`] holds (the VM's ZGC heap with
/// `CRATONVM_ZGC_NATIVE_DOORS_COLLECT` and `CRATONVM_ZGC_FFM_MINT_COLLECT`
/// on) and `CRATONVM_ZGC_UPCALL_ARGS_COLLECT` (default ON, `=0` restores the
/// one-attempt mints). Generational and G1: never (zt t1/nd `4d9001deab` made
/// [`crate::panama::ffm_mint_collects`] ZGC only again after the zs/dev
/// merge; zt t2/x1: this said "Generational and G1 too").
///
/// Why: those mints ran through the one-attempt doors, which abort the
/// process from a JIT-run downcall (permission cleared: ZGC's infallible
/// arm) and unwind through the `extern "C"` closure entry from an
/// interpreted one (Rust turns that into an abort too). HotSpot allocates
/// them in Java: collect, then the upcall's uncaught `OutOfMemoryError`
/// (`SharedUtils.handleUncaughtException`). Every reference the dispatch
/// holds there is pinned (`pin_base`, the invoker's pins), so the ladder's
/// precondition holds.
pub(crate) fn upcall_args_collect(ctx: &dyn NativeContext) -> bool {
    // zs s5/nd5: the switch is read once per process (it was one flag read
    // per upcall).
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    crate::panama::ffm_mint_collects(ctx)
        && *ON.get_or_init(|| {
            true
        })
}

/// The collecting arm of a group argument's block (zs s3/nd3,
/// [`upcall_args_collect`]): `Arena.allocate(size, align)` on the fresh,
/// confined per-upcall arena, without the session's block record. The record
/// (a `long[]` that grows, and its close-action node) is two more one-attempt
/// allocations on that path; here the block's id goes into `blocks`, which
/// `upcall_dispatch` frees after the arena's close succeeded -- what the
/// close walk does with the record. Nothing else in `pe_arena_allocate_impl`
/// applies to this arena: it is open (made by this dispatch), confined to
/// this thread (no shared close races the window), never an `ofAuto` arena
/// and never the four-slot synthetic-JDK layout. The carrier is the same
/// six-slot segment with the same fields.
fn upcall_group_block(
    ctx: &mut dyn NativeContext,
    arena: ObjectRef,
    size: u32,
    align: u32,
    blocks: &mut Vec<i64>,
) -> MethodCallResult {
    let size = i64::from(size);
    let (alloc_id, ptr) = ctx
        .allocate_native_memory(size.max(1) as usize, align.max(1) as usize)
        .ok_or_else(|| RuntimeError::OutOfMemoryError {
            message: format!("Unable to allocate {} bytes", size.wrapping_add(7) & !7),
        })?;
    let arena_pin = ctx.pin_native_root(arena);
    let made = crate::panama::alloc_segment_carrier_with(ctx, 6, true);
    let arena = ctx.read_native_pin(arena_pin, arena);
    ctx.unpin_native_roots(arena_pin);
    let segment = match made {
        Ok(segment) => segment,
        Err(err) => {
            ctx.free_native_memory(alloc_id);
            return Err(err);
        }
    };
    blocks.push(alloc_id);
    let scope = crate::panama::pe_carrier_scope(ctx, arena);
    ctx.set_field(segment, 0, Value::Long(ptr as i64));
    ctx.set_field(segment, 1, Value::Long(size));
    ctx.set_field(segment, 2, Value::Object(Some(scope)));
    ctx.set_field(segment, 3, Value::Int(0));
    ctx.set_field(segment, 4, Value::Int(1));
    ctx.set_field(segment, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(segment))))
}

/// Free the blocks [`upcall_group_block`] made, once the arena's close
/// succeeded (a failed close keeps them: a segment the target kept is then
/// still alive, as the record path leaves it).
fn free_upcall_blocks(ctx: &mut dyn NativeContext, closed: bool, blocks: &[i64]) {
    if closed {
        for &alloc_id in blocks {
            ctx.free_native_memory(alloc_id);
        }
    }
}

/// Copy a by-value group return out of the segment the target returned into
/// libffi's result buffer: `MemorySegment.copy` / `bufferLoad` on HotSpot, so
/// a heap segment is accepted, a closed one is `IllegalStateException` and a
/// short one `IndexOutOfBoundsException` (both then the uncaught-exception
/// exit). No allocation on the success path.
fn copy_group_return(
    ctx: &mut dyn NativeContext,
    value: Value,
    size: usize,
    dest: Option<GroupReturn>,
) -> Result<(), MethodCallFailed> {
    let segment = match value {
        Value::Object(Some(segment)) => segment,
        Value::Object(None) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())
        }
        other => {
            return Err(RuntimeError::IllegalStateException {
                message: format!("upcall target returned {other:?} for a by-value group return"),
            }
            .into())
        }
    };
    let Some(dest) = dest else {
        return Ok(());
    };
    // From before the scope check to the last read: a shared close of the
    // segment's arena on another thread waits for the copy, or the check sees
    // the arena closed.
    let _window = crate::ffm_fast::AccessWindow::open();
    crate::panama::pe_segment_check_scope(ctx, segment)?;
    let available = plf::segment_byte_size(ctx, segment);
    if available < size as i64 {
        // j27 wave 4 (lane FFMMSG): the segment's `toString()`, as
        // `outOfBoundException`; it printed `MemorySegment{ byteSize: N }`.
        return Err(crate::panama::segment_out_of_bounds(ctx, segment, 0, size as i64));
    }
    let len = size.min(dest.len);
    if let Some(view) = crate::panama::heap_segment_view(ctx, segment) {
        let bytes = crate::panama::heap_read_bytes(ctx, &view, 0, len).ok_or_else(|| {
            MethodCallFailed::from(RuntimeError::IllegalStateException {
                message: "upcall: the returned heap segment cannot be read".into(),
            })
        })?;
        // SAFETY: `dest` is libffi's result buffer of `dest.len >= len` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), dest.ptr, len) };
        return Ok(());
    }
    let address = plf::segment_address(ctx, segment);
    if address == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "upcall: the returned segment has no address".into(),        }
        .into());
    }
    let address = plf::checked_foreign_addr(address, "upcall struct return")?;
    // SAFETY: the segment is alive (checked under the window) and at least
    // `size >= len` bytes long; `dest` holds `len` bytes.
    unsafe { std::ptr::copy_nonoverlapping(address as usize as *const u8, dest.ptr, len) };
    Ok(())
}

/// One upcall, in the context of the downcall it happens inside.
///
/// # Safety
/// `args` must hold one argument pointer per parameter of `data.shape`, each
/// pointing at a value of that parameter's C type; when the return is a
/// by-value group, one more pointing at the [`GroupReturn`] to fill
/// (`upcall_entry_group` appends it).
pub(crate) unsafe fn upcall_dispatch(
    ctx: &mut dyn NativeContext,
    data: &UpcallStubData,
    args: *const *const c_void,
) -> Result<u64, UpcallFatal> {
    if ctx.vm_identity() != data.vm {
        return Err(UpcallFatal::Unsupported(
            "the upcall stub belongs to another VM than the downcall it was called from".into(),
        ));
    }
    // Round 13 wave 8 (lane ffm4): dead once the arena's close walk ran, not
    // while a close is still deciding (see `upcall_retire_at_close`). HotSpot
    // frees the stub in that walk; a call after it is undefined behaviour
    // there, and a loud exit here while the code is still ours.
    if data.dead(ctx) {
        return Err(UpcallFatal::Unsupported(
            "the upcall stub was called after its arena was closed".into(),
        ));
    }
    let params = &data.shape.params;
    // Raw values first: no allocation, so nothing moves meanwhile.
    // j27 wave 5 (lane FFMUP): into a stack buffer up to `INLINE_ARGS`
    // parameters (the heap `Vec` was ~4 % of an up-call).
    let mut inline = [Value::Int(0); INLINE_ARGS];
    let mut heap: Vec<Value> = Vec::new();
    let values: &mut [Value] = if params.len() <= INLINE_ARGS {
        &mut inline[..params.len()]
    } else {
        heap.resize(params.len(), Value::Int(0));
        &mut heap
    };
    for (i, param) in params.iter().enumerate() {
        // SAFETY: the caller's contract.
        let slot = unsafe { *args.add(i) };
        // SAFETY: `slot` points at a value of `param.kind`'s C type.
        values[i] = unsafe { decode_arg(param.kind, slot) };
    }
    // Round 13 wave 11 (lane ffm6): where a by-value group return goes.
    let group_ret: Option<GroupReturn> = match data.shape.ret {
        Some(UpcallKind::Group { .. }) => {
            // SAFETY: the caller's contract: one more pointer, at a
            // `GroupReturn`, after the parameters.
            let slot = unsafe { *args.add(params.len()) };
            // SAFETY: as above.
            (!slot.is_null()).then(|| unsafe { *(slot as *const GroupReturn) })
        }
        _ => None,
    };
    // Addresses and by-value groups become segments. Each allocation is a GC
    // point, so every segment made so far stays pinned until all of them
    // exist -- and, when a group argument made the per-upcall arena, until
    // the call has returned and that arena is closed.
    let mut pin_base: Option<usize> = None;
    let mut segments: Vec<(usize, usize, ObjectRef)> = Vec::new();
    let mut arena: Option<(usize, ObjectRef)> = None;
    // zs s3/nd3: the group arguments' mints collect on ZGC
    // (`upcall_args_collect`); their blocks are freed after the close.
    // zt t3/nd (t2/rv hunk ND-1, zs4rv3 item 9): read only when a group
    // parameter needs it (an upcall with no group argument reads no flag).
    let mut collecting: Option<bool> = None;
    let mut blocks: Vec<i64> = Vec::new();
    // j27 wave 5 (lane FFMUP): a stub without group parameters mints all its
    // address arguments in one batch (`mint_address_args_batched`); the
    // per-argument loop below then has nothing left to do.
    let mut minted_first_class = None;
    let loop_params: &[UpcallParam] = match mint_address_args_batched(ctx, &data.mint, params, values) {
        Some(Ok(())) => {
            minted_first_class = batched_first_class(params, data.mint.get());
            &[]
        },
        Some(Err(err)) => return Err(UpcallFatal::Uncaught(err)),
        None => params,
    };
    for (i, param) in loop_params.iter().enumerate() {
        let Value::Long(raw) = values[i] else {
            continue;
        };
        let made = match param.kind {
            // Round 13 wave 13 (lane ffm7): `Utils.longToAddress`'s check.
            UpcallKind::Address if misaligned_address(raw, param.target_align) => {
                Err(illegal_argument(format!(
                    "Invalid alignment constraint for address: 0x{raw:x}"
                )))
            }
            UpcallKind::Address => new_arg_segment(ctx, raw, param.target_size),
            UpcallKind::Group { size, align } => {
                let collecting = match collecting {
                    Some(on) => on,
                    None => *collecting.insert(upcall_args_collect(ctx)),
                };
                // SAFETY: `raw` is libffi's pointer to the aggregate (`decode_arg`).
                unsafe {
                    new_group_arg_segment(
                        ctx,
                        &mut arena,
                        &mut pin_base,
                        raw as usize as *const u8,
                        size,
                        align,
                        collecting,
                        &mut blocks,
                    )
                }
            }
            _ => continue,
        };
        match made {
            Ok(segment) => {
                let pin = ctx.pin_native_root(segment);
                if pin_base.is_none() {
                    pin_base = Some(pin);
                }
                segments.push((i, pin, segment));
            }
            Err(err) => {
                let err = close_upcall_arena(ctx, arena, err, &blocks);
                if let Some(base) = pin_base {
                    ctx.unpin_native_roots(base);
                }
                return Err(UpcallFatal::Uncaught(err));
            }
        }
    }
    for (i, pin, segment) in segments {
        values[i] = Value::Object(Some(ctx.read_native_pin(pin, segment)));
    }
    if arena.is_none() {
        if let Some(base) = pin_base.take() {
            ctx.unpin_native_roots(base);
        }
    }
    // Read after the allocations: the registry root scan rewrites it in place.
    let target = data.target.load(Ordering::Relaxed);
    if target == 0 {
        if let Some(base) = pin_base {
            ctx.unpin_native_roots(base);
        }
        return Err(UpcallFatal::Unsupported(
            "the upcall stub has no target".into(),
        ));
    }
    // SAFETY: stored from a live `ObjectRef` of this VM (checked above), and
    // kept alive and current by `gc_scan_roots` / `gc_update_refs`.
    let target = unsafe { ObjectRef::from_raw(target as *mut u8) };
    // Round 14 wave 1 (lane ffm): a plain direct static target skips
    // `mh_dispatch` (`DirectStatic`, `CRATONVM_FFM_UPCALL_DIRECT_STATIC`).
    // Round 15 wave 5 (lane upcall1): only with the member's own argument
    // count (see `DirectStatic::params`); otherwise the invoker, whose leaf
    // arms refuse a short list.
    // j27 wave 1 (lane FFMVH): a real handle's stub keeps the JDK's forms
    // while a debugger observes frames (`DirectStatic::real`).
    let invoked = match data
        .direct
        .get()
        .filter(|d| d.params == values.len() && !(d.real && ctx.debugger_observes_frames()))
    {
        Some(direct) => {
            let settled = match direct.settled.load(Ordering::Relaxed) {
                0 => None,
                raw => Some(cratonvm_types::ClassId::new(raw)),
            };
            let (result, now) = match minted_first_class {
                Some(first_class) => ctx.invoke_static_settling_minted(
                    direct.owner, settled, first_class, &direct.class,
                    &direct.name, &direct.desc, values),
                None => ctx.invoke_static_settling(
                    direct.owner, settled, &direct.class,
                    &direct.name, &direct.desc, values),
            };
            if let Some(owner) = now {
                direct.settled.store(owner.as_u32(), Ordering::Relaxed);
            }
            result
        }
        None => (data.invoker)(ctx, target, values, &data.shape),
    };
    let outcome = invoked.and_then(|returned| {
        let returned = returned.unwrap_or(Value::Object(None));
        match data.shape.ret {
            None => Ok(0),
            Some(UpcallKind::Group { size, .. }) => {
                copy_group_return(ctx, returned, size as usize, group_ret).map(|()| 0)
            }
            Some(kind) => return_bits(ctx, kind, returned),
        }
    });
    // After the return was read (the target may return one of its group
    // arguments): the arena is closed, which frees the argument copies.
    let outcome = match outcome {
        Ok(bits) => match arena {
            Some((pin, made)) => {
                let made = ctx.read_native_pin(pin, made);
                let closed = foreign_ffm::p67_close_vm_arena(ctx, made).is_ok();
                free_upcall_blocks(ctx, closed, &blocks);
                Ok(bits)
            }
            None => Ok(bits),
        },
        Err(err) => Err(close_upcall_arena(ctx, arena, err, &blocks)),
    };
    if let Some(base) = pin_base {
        ctx.unpin_native_roots(base);
    }
    outcome.map_err(UpcallFatal::Uncaught)
}

/// Close the per-upcall arena, if one was made, on a failure path, keeping
/// `failure`'s throwable current across the close (which may enter a
/// blocking region). The arena's own close failure is dropped: the upcall
/// already fails with `failure`.
fn close_upcall_arena(
    ctx: &mut dyn NativeContext,
    arena: Option<(usize, ObjectRef)>,
    failure: MethodCallFailed,
    blocks: &[i64],
) -> MethodCallFailed {
    let Some((pin, made)) = arena else {
        return failure;
    };
    let made = ctx.read_native_pin(pin, made);
    match failure {
        MethodCallFailed::ExceptionThrown(thrown) => {
            let thrown_pin = ctx.pin_native_root(thrown);
            let closed = foreign_ffm::p67_close_vm_arena(ctx, made).is_ok();
            free_upcall_blocks(ctx, closed, blocks);
            let thrown = ctx.read_native_pin(thrown_pin, thrown);
            ctx.unpin_native_roots(thrown_pin);
            MethodCallFailed::ExceptionThrown(thrown)
        }
        other => {
            let closed = foreign_ffm::p67_close_vm_arena(ctx, made).is_ok();
            free_upcall_blocks(ctx, closed, blocks);
            other
        }
    }
}

/// HotSpot's `SharedUtils.handleUncaughtException`: the trace, the message,
/// `System.exit(1)`. Returns only when the exit is soft-returned
/// (`CRATONVM_SOFT_EXIT`); the stub then hands C a zero.
fn terminate_uncaught(ctx: &mut dyn NativeContext, failure: MethodCallFailed) {
    let contain = true;
    report_uncaught(ctx, failure, contain);
    // zs s6/rv4: the exit runs Java (shutdown hooks) on this thread; if it
    // raises the heap-exhaustion unwind, exit without them rather than let
    // the unwind reach libffi's trampoline (the abort).
    contained_report_stage(
        ctx,
        contain,
        |ctx| {
            let _ = crate::lang_system::native_runtime_exit(ctx, &[Value::Int(1)]);
        },
        || cratonvm_native_api::process_exit::exit_process(1),
    );
}

/// The report half of [`terminate_uncaught`]: the trace, then the message.
///
/// zs s6/rv4: each stage allocates (the throwable a native failure stands
/// for, the message `String`) on a thread that may still unwind -- the
/// upcall's funnel permission -- and the failure being reported is very
/// often the heap's exhaustion itself (`upcall_dispatch_contained`). A
/// refused allocation here raised the same `NativeAllocOom` unwind that
/// `upcall_dispatch_contained` had just stopped, from OUTSIDE its
/// `catch_unwind`, and it crossed libffi's trampoline: the `failed to
/// initiate panic, error 3` abort again, on the report of the very failure
/// whose containment was the point. With `contain` (ZGC, the
/// `CRATONVM_ZGC_UPCALL_CONTAIN_OOM_UNWIND` switch) each stage is contained
/// the same way and falls back to the process stderr; `contain == false` is
/// the old sequence, byte for byte.
fn report_uncaught(ctx: &mut dyn NativeContext, failure: MethodCallFailed, contain: bool) {
    const MESSAGE: &str = "Unrecoverable uncaught exception encountered. The VM will now exit";
    let java_err = true;
    // What the trace stage's fallback prints: the failure's own line.
    let plain = match &failure {
        MethodCallFailed::InternalError(err) => Some(uncaught_plain_line(err)),
        MethodCallFailed::ExceptionThrown(_) => None,
    };
    contained_report_stage(
        ctx,
        contain,
        |ctx| match failure {
            MethodCallFailed::ExceptionThrown(throwable) => {
                let _ = ctx.invoke_virtual(throwable, "printStackTrace", "()V", &[]);
            }
            MethodCallFailed::InternalError(err) => {
                match java_err.then(|| upcall_throwable_of(ctx, &err)).flatten() {
                    Some(throwable) => {
                        let _ = ctx.invoke_virtual(throwable, "printStackTrace", "()V", &[]);
                    }
                    None => eprintln!("Exception in upcall: {err}"),
                }
            }
        },
        || {
            if let Some(line) = &plain {
                eprintln!("{line}");
            }
        },
    );
    contained_report_stage(
        ctx,
        contain,
        |ctx| {
            if !(java_err && print_to_system_err(ctx, MESSAGE)) {
                eprintln!("{MESSAGE}");
            }
        },
        || eprintln!("{MESSAGE}"),
    );
}

/// The line a natively raised upcall failure prints when its Java throwable
/// cannot be built (zs s6/rv4): `java.lang.OutOfMemoryError: Java heap space`
/// for the heap-exhaustion failure, as the first line of its trace would
/// read; `Exception in upcall: <error>` for anything without a Java face.
fn uncaught_plain_line(err: &cratonvm_types::error::VmError) -> String {
    if let cratonvm_types::error::VmError::Runtime(re) = err {
        if let Some((class_name, message)) = re.as_java_throwable() {
            let class_name = class_name.replace('/', ".");
            return match message {
                Some(message) => format!("{class_name}: {message}"),
                None => class_name,
            };
        }
    }
    format!("Exception in upcall: {err}")
}

/// Run one stage of the uncaught-upcall report (zs s6/rv4). With `contain`, a
/// heap-exhaustion unwind out of it (`native_alloc_oom_failure`: ZGC only)
/// is stopped here and `fallback` runs instead; any other panic, and every
/// unwind without `contain`, is resumed unchanged.
fn contained_report_stage(
    ctx: &mut dyn NativeContext,
    contain: bool,
    stage: impl FnOnce(&mut dyn NativeContext),
    fallback: impl FnOnce(),
) {
    if !contain {
        stage(ctx);
        return;
    }
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stage(&mut *ctx)));
    if let Err(payload) = caught {
        if ctx.native_alloc_oom_failure(&*payload).is_none() {
            std::panic::resume_unwind(payload);
        }
        fallback();
    }
}



/// The Java throwable a natively raised upcall failure stands for, or `None`
/// for a VM fault no `catch` could name.
fn upcall_throwable_of(
    ctx: &mut dyn NativeContext,
    err: &cratonvm_types::error::VmError,
) -> Option<ObjectRef> {
    let cratonvm_types::error::VmError::Runtime(re) = err else {
        return None;
    };
    let (class_name, message) = re.as_java_throwable()?;
    let built = match message {
        Some(message) => {
            let text = ctx.create_string(&message);
            ctx.new_object_initialized(
                class_name,
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(text))],
            )
        }
        None => ctx.new_object_initialized(class_name, "()V", &[]),
    };
    match built {
        Ok(Some(Value::Object(Some(throwable)))) => Some(throwable),
        _ => None,
    }
}

/// `System.err.println(text)`. Answers whether it ran.
fn print_to_system_err(ctx: &mut dyn NativeContext, text: &str) -> bool {
    let Some(system) = ctx.class_id_by_name("java/lang/System") else {
        return false;
    };
    let Some(index) = ctx.static_field_index_by_name(system, "err") else {
        return false;
    };
    let Value::Object(Some(err)) = ctx.get_static_field(system, index) else {
        return false;
    };
    let pin = ctx.pin_native_root(err);
    let text = ctx.create_string(text);
    let err = ctx.read_native_pin(pin, err);
    ctx.unpin_native_roots(pin);
    ctx.invoke_virtual(
        err,
        "println",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(text))],
    )
    .is_ok()
}

fn upcall_fatal(ctx: &mut dyn NativeContext, data: &UpcallStubData, fatal: UpcallFatal) {
    match fatal {
        UpcallFatal::Uncaught(failure) => terminate_uncaught(ctx, failure),
        UpcallFatal::Unsupported(why) => {
            eprintln!(
                "[cratonvm] FFM upcall stub {}: {why}. The VM will now exit",
                data.label
            );
            let _ = crate::lang_system::native_runtime_exit(ctx, &[Value::Int(1)]);
        }
    }
}

/// An upcall on a thread with no active downcall -- a foreign thread, or C
/// that kept the pointer and calls it from outside any downcall -- that the
/// VM's [`UpcallAttachHook`] could not run either (no hook installed, the
/// switch off, or the stub's VM gone). There is no context to run Java in,
/// and returning a made-up zero to C is the silent wrong answer this
/// replaces. HotSpot attaches the thread
/// (`r12w7-upcall-foreign-thread-upcalls-are-unsupported`).
fn upcall_without_context(data: &UpcallStubData) -> ! {
    eprintln!(
        "[cratonvm] FFM upcall stub {} was called on a thread with no active downcall \
         (a foreign thread, or outside any downcall). CratonVM runs upcalls only on the \
         thread of the downcall that reaches them. The VM will now exit",
        data.label
    );
    cratonvm_native_api::process_exit::exit_process(1)
}

/// How the VM runs an upcall on a thread that holds no FFM downcall context
/// (round 13 wave 1, lane mhffm; `r12w7-upcall-foreign-thread-upcalls-are-unsupported`):
/// `hook(vm, run)` finds a `NativeContext` of the VM whose `vm_identity` is
/// `vm` for the calling OS thread -- the thread's own, when it is already
/// bound to that VM (a JNI native, an attached host thread), else by
/// attaching it as a daemon thread the way HotSpot's `UpcallLinker::on_entry`
/// does -- calls `run` with it once, and answers whether it did. `false`
/// leaves the upcall unsupported (the loud exit).
///
/// A `fn` address the VM installs once (`set_upcall_attach_hook`), the same
/// for every VM: not per-VM state. The `vm` argument selects the VM.
pub type UpcallAttachHook = fn(vm: usize, run: &mut dyn FnMut(&mut dyn NativeContext)) -> bool;

static UPCALL_ATTACH_HOOK: std::sync::OnceLock<UpcallAttachHook> = std::sync::OnceLock::new();

/// Install the VM's [`UpcallAttachHook`]. First installer wins (every VM
/// installs the same function), as with `lang_system::set_pre_exit_hook`.
pub fn set_upcall_attach_hook(hook: UpcallAttachHook) {
    let _ = UPCALL_ATTACH_HOOK.set(hook);
}



/// One upcall in `ctx`: the bits for C, or `None` after an uncaught failure
/// was reported (the VM is exiting; a soft-returned exit hands C a zero).
///
/// # Safety
/// As [`upcall_dispatch`].
unsafe fn run_upcall(
    ctx: &mut dyn NativeContext,
    data: &UpcallStubData,
    args: *const *const c_void,
) -> Option<u64> {
    // Round 13 wave 8 (lane ffm4): a stub of another VM inside this VM's
    // downcall runs on a helper thread of its own VM (`upcall_on_helper_thread`).
    if ctx.vm_identity() != data.vm {
        // Round 14 wave 1 (lane ffm): the wait is GC-safe in this VM, so a
        // stop-the-world pause of this VM may run meanwhile -- an A -> B -> A
        // chain otherwise hangs (A's pause waits for this thread, which waits
        // for B's helper, which waits for A's helper, which waits for the
        // pause). Every reference the downcall uses after the C call is
        // pinned (a same-thread upcall can already collect), and
        // `upcall_entry` calls this outside any blocking region in both
        // downcall modes (a GC-safe downcall's region is left before it)
        // (`r13w8-ffm4-cross-vm-upcall-wait-gc-safe-patch-FIXED-20260929.md`).
        let gc_safe_wait = true;
        if gc_safe_wait {
            // JIT round 17 wave 1 (lane ffmseg): RUNNABLE while waiting, as
            // a thread in the foreign code of its downcall.
            {
                ctx.begin_native_region();
            }
        }
        // SAFETY: the caller's contract.
        let outcome = unsafe { upcall_on_helper_thread(data, args) };
        if gc_safe_wait {
            ctx.end_blocking_region();
        }
        return match outcome {
            Some(done) => done,
            None => upcall_cross_vm_failed(data),
        };
    }
    // SAFETY: the caller's contract.
    match unsafe { upcall_dispatch_contained(ctx, data, args) } {
        Ok(bits) => Some(bits),
        Err(fatal) => {
            upcall_fatal(ctx, data, fatal);
            None
        }
    }
}

/// zs s5/nd5: [`upcall_dispatch`] with the VM's heap-exhaustion unwind
/// stopped HERE, below the closure entry.
///
/// An upcall from an INTERPRETED downcall runs with the native funnel's
/// unwind permission still raised (the funnel that dispatched the downcall is
/// the nearest handler), so a one-attempt allocation door refused inside the
/// dispatch -- outside the interpreter's own `catch_unwind` -- raises the
/// `NativeAllocOom` unwind. Its nearest handler is above libffi's closure
/// trampoline, a frame with no unwind information: phase 1 of the unwind
/// fails and Rust aborts the process (`fatal runtime error: failed to initiate
/// panic, error 3`, SIGABRT; the VM's panic hook is silent for that payload).
/// Host rows `Zs3Nd3UpcallMints mixed` (ZGC) aborted so after the proxy
/// leak filled the heap. HotSpot's upcall allocations are Java allocations:
/// a refused one is the upcall's uncaught `OutOfMemoryError`, i.e.
/// `SharedUtils.handleUncaughtException` (the trace, the message, exit 1),
/// which is what [`upcall_fatal`] does with `UpcallFatal::Uncaught`.
///
/// Every reference the dispatch pinned is released (the pin stack is cut
/// back to its depth at entry). Any other panic, and the heap-exhaustion
/// unwind off ZGC (`native_alloc_oom_failure` answers `None` there) or under
/// `CRATONVM_ZGC_UPCALL_CONTAIN_OOM_UNWIND=0`, is resumed unchanged (the old
/// abort).
///
/// # Safety
/// As [`upcall_dispatch`].
unsafe fn upcall_dispatch_contained(
    ctx: &mut dyn NativeContext,
    data: &UpcallStubData,
    args: *const *const c_void,
) -> Result<u64, UpcallFatal> {
    let pin_depth = upcall_pin_depth(ctx, data);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the caller's contract.
        unsafe { upcall_dispatch(&mut *ctx, data, args) }
    }));
    match caught {
        Ok(done) => done,
        Err(payload) => match ctx.native_alloc_oom_failure(&*payload) {
            Some(oome) => {
                if let Some(depth) = pin_depth {
                    ctx.unpin_native_roots(depth);
                }
                Err(UpcallFatal::Uncaught(oome))
            }
            None => std::panic::resume_unwind(payload),
        },
    }
}

/// The native pin stack's depth at an upcall's entry: pin the stub's target
/// (a live, rooted object) and release it at once; the handle IS the depth.
/// `None` when the stub has no target (the dispatch then pins nothing).
fn upcall_pin_depth(ctx: &mut dyn NativeContext, data: &UpcallStubData) -> Option<usize> {
    let target = data.target.load(Ordering::Relaxed);
    if target == 0 {
        return None;
    }
    // SAFETY: stored from a live `ObjectRef` of this VM, kept current by the
    // registry's root scan (see `upcall_dispatch`).
    let target = unsafe { ObjectRef::from_raw(target as *mut u8) };
    let depth = ctx.pin_native_root(target);
    ctx.unpin_native_roots(depth);
    Some(depth)
}



/// An upcall with no active downcall context, through the VM's attach hook.
/// `None` when there is no hook, the switch is off, or the hook could not
/// find or attach the stub's VM.
///
/// # Safety
/// As [`upcall_dispatch`].
unsafe fn upcall_via_attach_hook(
    data: &UpcallStubData,
    args: *const *const c_void,
) -> Option<Option<u64>> {
    let hook = *UPCALL_ATTACH_HOOK.get()?;
    let mut outcome: Option<Option<u64>> = None;
    let mut run = |ctx: &mut dyn NativeContext| {
        // SAFETY: the caller's contract, unchanged by the hook.
        outcome = Some(unsafe { run_upcall(ctx, data, args) });
    };
    if hook(data.vm, &mut run) {
        outcome
    } else {
        None
    }
}







/// Native stack of a cross-VM helper thread: what the embedding library gives
/// the threads it starts to run Java on (`libcratonvm`, 16 MiB).
const CROSS_VM_HELPER_STACK: usize = 16 * 1024 * 1024;

/// Run one upcall of `data`'s VM (B) through the attach hook on a fresh OS
/// thread, and wait for it. Answers what [`upcall_via_attach_hook`] answers
/// on that thread: `None` when B could not run it (B is gone, the attach
/// switch is off) or no helper thread could be started.
///
/// Why a helper: every piece of per-OS-thread VM state -- the JNI binding, the
/// foreign attachment, the interpreter's depth counters -- names ONE VM, and
/// the calling thread's names A. So B's Java does not run there: the helper is
/// attached to B as a daemon exactly as a C-created thread calling the stub is
/// attached (the attachment is reaped when the helper exits). HotSpot has one
/// VM per process and nothing to compare with; the one visible difference
/// from a same-VM upcall is `Thread.currentThread()` in the target, which is
/// the helper.
///
/// This function makes no thread-state transition; [`run_upcall`] puts the
/// calling thread in a blocking region of its VM around it
/// (`r13w8-ffm4-cross-vm-upcall-wait-gc-safe-patch-FIXED-20260929.md`).
///
/// # Safety
/// As [`upcall_dispatch`]; `args` and `data` stay valid until the helper is
/// joined, which happens before this returns.
unsafe fn upcall_on_helper_thread(
    data: &UpcallStubData,
    args: *const *const c_void,
) -> Option<Option<u64>> {
    /// The two borrowed pointers the helper needs. Raw pointers are not
    /// `Send`; the scoped join below is what makes handing them over sound.
    struct Job {
        data: *const UpcallStubData,
        args: *const *const c_void,
    }
    // SAFETY: the helper only reads `*data` (atomics where it changes) and
    // the argument slots, and `std::thread::scope` joins it before the
    // borrowed stack frames of this upcall can end.
    unsafe impl Send for Job {}
    let job = Job { data, args };
    std::thread::scope(|scope| {
        let helper = std::thread::Builder::new()
            .name("cratonvm-ffm-cross-vm-upcall".to_string())
            .stack_size(CROSS_VM_HELPER_STACK)
            .spawn_scoped(scope, move || {
                // The whole `Job` moves in, not its (non-`Send`) fields.
                let job = job;
                // SAFETY: the caller's contract; see `Job`.
                unsafe { upcall_via_attach_hook(&*job.data, job.args) }
            });
        match helper {
            Ok(helper) => helper.join().ok().flatten(),
            Err(_) => None,
        }
    })
}

/// An upcall on a thread with no active downcall: through the attach hook on
/// this thread, and -- when the hook refuses because this thread is bound to
/// ANOTHER VM (a JNI native of VM A calling VM B's kept stub) -- on a helper
/// thread attached to the stub's VM (round 13 wave 8, lane ffm4). With the
/// VM gone the helper fails too, and the caller's loud exit follows.
///
/// # Safety
/// As [`upcall_dispatch`].
unsafe fn upcall_without_downcall(
    data: &UpcallStubData,
    args: *const *const c_void,
) -> Option<Option<u64>> {
    // SAFETY: the caller's contract.
    if let Some(done) = unsafe { upcall_via_attach_hook(data, args) } {
        return Some(done);
    }
    if UPCALL_ATTACH_HOOK.get().is_some() {
        // SAFETY: the caller's contract.
        return unsafe { upcall_on_helper_thread(data, args) };
    }
    None
}

/// A cross-VM upcall inside a downcall could not run: the stub's VM is gone
/// (or the attach switch is off), or no helper thread could be started. C
/// gets no answer it could trust, so the process exits loudly.
fn upcall_cross_vm_failed(data: &UpcallStubData) -> ! {
    eprintln!(
        "[cratonvm] FFM upcall stub {} belongs to another VM than the downcall it was called \
         from, and that VM could not run it (it is gone, or no thread could be attached to \
         it). The VM will now exit",
        data.label
    );
    cratonvm_native_api::process_exit::exit_process(1)
}

/// The libffi closure callback of every upcall stub.
unsafe extern "C" fn upcall_entry(
    _cif: &ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    data: &UpcallStubData,
) {
    // `result` is at least one `ffi_arg` wide, as libffi guarantees for a
    // closure whose return type is a scalar or `void`; a stub with a group
    // return enters through `upcall_entry_group`, which hands this a scratch
    // word.
    *result = 0;
    // JIT round 17 wave 2 (lane ffm3; EINTR17-1,
    // `CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL`): the downcall around this
    // up-call blocked the take-over signal for its C code; the up-call's Java
    // must stay takeover-able (and a helper-window pass must be able to park
    // it in a blocking native), so the signal is deliverable until C resumes.
    let _unmask = plf::TakeoverSignalMask::unblock(plf::ffm_downcall_mask_takeover_signal());
    data.in_flight.fetch_add(1, Ordering::SeqCst);
    // SAFETY: libffi passes one argument pointer per CIF parameter, and the
    // CIF was built from `data.shape`.
    let outcome = plf::with_active_context(|ctx| {
        // Round 13 wave 5 (lane ffm3): a downcall run inside a GC blocking
        // region (`CRATONVM_FFM_DOWNCALL_GC_SAFE`) is a mutator again for the
        // upcall's Java, and blocked again once it returns.
        let gc_safe = plf::active_downcall_gc_safe();
        if gc_safe {
            plf::leave_downcall_region(ctx); // gcd d5/f: marks the region left
        }
        // SAFETY: as above (libffi's argument pointers match `data.shape`).
        let done = unsafe { run_upcall(ctx, data, args) };
        if gc_safe {
            plf::reenter_downcall_region(ctx); // gcd d5/f
        }
        done
    });
    // No downcall on this thread: a foreign thread, or C calling a kept
    // pointer outside any downcall. SAFETY: as above.
    let outcome = match outcome {
        Some(done) => Some(done),
        None => unsafe { upcall_without_downcall(data, args) },
    };
    data.in_flight.fetch_sub(1, Ordering::SeqCst);
    match outcome {
        Some(Some(bits)) => *result = bits,
        Some(None) => {}
        None => upcall_without_context(data),
    }
}

/// The libffi closure callback of a stub whose return is a by-value group
/// (round 13 wave 11, lane ffm6). `result` is libffi's return buffer of
/// `cif.rtype.size` bytes -- the C caller's own memory when the ABI returns
/// the aggregate through a hidden pointer (Win64 for any size but 1, 2, 4 and
/// 8; SysV above 16 bytes) -- so it is written as exactly that many bytes,
/// never as an `ffi_arg`. It is typed `&mut u8` only because `Closure::new`
/// wants a typed result; every access goes through the raw pointer taken
/// from it, with the buffer's full length. The upcall itself runs through
/// [`upcall_entry`] (with a scratch result word), with the [`GroupReturn`]
/// appended to the argument pointers for `upcall_dispatch` to fill.
unsafe extern "C" fn upcall_entry_group(
    cif: &ffi_cif,
    result: &mut u8,
    args: *const *const c_void,
    data: &UpcallStubData,
) {
    let ptr: *mut u8 = result;
    let len = if cif.rtype.is_null() {
        0
    } else {
        // SAFETY: a prepared CIF's return type.
        unsafe { (*cif.rtype).size }
    };
    // SAFETY: `ptr` is libffi's result buffer of `len` bytes. Zeroed first,
    // so an upcall that fails (and, with a soft exit, returns) hands C zeros.
    unsafe { std::ptr::write_bytes(ptr, 0, len) };
    let ret = GroupReturn { ptr, len };
    let params = data.shape.params.len();
    let mut extended: Vec<*const c_void> = Vec::with_capacity(params + 1);
    for i in 0..params {
        // SAFETY: libffi passes one argument pointer per CIF parameter.
        extended.push(unsafe { *args.add(i) });
    }
    extended.push((&ret as *const GroupReturn).cast());
    let mut scratch: u64 = 0;
    // SAFETY: `extended` holds the parameters' pointers and then the
    // `GroupReturn`, which `upcall_dispatch` reads for a group return; both
    // live until this returns, which is after any helper thread was joined.
    unsafe { upcall_entry(cif, &mut scratch, extended.as_ptr(), data) };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

#[cfg(test)]
mod r16w9_ffm_invoker_choice_tests;

/// zs s3/nd3 (`zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md` §1): the
/// collecting mints of an upcall's group arguments and of the generic
/// invoker. The mock's fallible doors refuse on demand
/// (`mock_refuse_allocations`); each mint must be served after ONE reclaim,
/// and a spent ladder must be the catchable `OutOfMemoryError`, never the
/// one-attempt door.
#[cfg(test)]
mod zs3_nd3_upcall_mint_tests;

/// zt t6/nd4 (zs3nd3 §4, carried to `zgc-zt1nd-*`): the generic invoker
/// (`invoke_generic`, a target whose type the stub could not prove) driven
/// through `upcall_dispatch` on a ZGC-answering mock: its `Object[]` mint
/// collects on a refusal and the boxed argument reaches `invokeWithArguments`.
#[cfg(test)]
mod zt6nd4_invoke_generic_dispatch_tests;

/// zs s5/nd5: the heap-exhaustion unwind inside an upcall's dispatch is
/// stopped below the closure entry (`upcall_dispatch_contained`) and becomes
/// the upcall's uncaught `OutOfMemoryError`; on the host it crossed libffi's
/// trampoline and aborted (`failed to initiate panic, error 3`).
#[cfg(test)]
mod zs5_nd5_upcall_unwind_tests;

/// j27 wave 5 (lane FFMUP): the batched address-argument mint
/// (`mint_address_args_batched`, `CRATONVM_FFM_UPCALL_MINT_BATCH`) gives each
/// address argument the segment the per-argument loop gives it, survives a
/// collection at every allocation, and fails a misaligned address in place.
#[cfg(test)]
mod j27_ffmup_mint_batch_tests;
