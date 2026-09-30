// SPDX-License-Identifier: Apache-2.0
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
    let prev = r.current_category();
    r.set_category(NativeKind::Bridge);
    r.register_with_kind(
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
    r.register_with_kind(
        "java/lang/foreign/Linker",
        "downcallHandle",
        "(Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;",
        crate::panama::pe_downcall_handle_unbound,
        NativeKind::Bridge,
    );
    r.set_category(prev);
}

/// `CRATONVM_FFM_UPCALL_STUBS` (default on). Off makes `Linker.upcallStub`
/// throw `UnsupportedOperationException` instead of building a stub. Read per
/// stub creation, which is rare, so it needs no cache.
pub(crate) fn upcall_stubs_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_STUBS")
}

/// `CRATONVM_FFM_UPCALL_TYPE_CHECK` (default on): refuse a target whose
/// `type()` certainly differs from the descriptor's `MethodType`, with
/// HotSpot's `IllegalArgumentException: Wrong method handle type`. Off skips
/// the check.
fn upcall_type_check_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_TYPE_CHECK")
}

/// `CRATONVM_FFM_UPCALL_TYPE_CHECK_OBJECT` (default on; round 13 wave 5, lane
/// ffm3): when every class in the target's `type()` resolves to a NAME, an
/// `Object` in it is a real `Object` and a mismatch like any other (HotSpot
/// compares the two `MethodType`s with `equals`). `0` keeps treating every
/// `Object` as "unreadable mirror, accept". Read per stub creation.
fn upcall_type_check_object_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_TYPE_CHECK_OBJECT")
}

/// `CRATONVM_FFM_UPCALL_CHECK_EXCEPTIONS` (default on; round 13 wave 1, lane
/// mhffm): `Linker.upcallStub` refuses a direct target whose member declares
/// exceptions, with HotSpot's `IllegalArgumentException: Target handle may
/// throw exceptions: [...]`. `0` skips the check. Read per stub creation.
fn upcall_check_exceptions_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_CHECK_EXCEPTIONS")
}

/// `CRATONVM_FFM_UPCALL_BY_VALUE_GROUPS` (default on; round 13 wave 11, lane
/// ffm6): `Linker.upcallStub` accepts a struct or union layout as a parameter
/// or the return, as HotSpot does. A group parameter reaches the target as a
/// segment of a confined arena made for the one upcall and closed after it (a
/// retained segment is then `Already closed`, HotSpot's `newBoundedArena`
/// frame); a group return is copied out of the segment the target returns. `0`
/// restores the `UnsupportedOperationException` at creation. Read per stub
/// creation.
fn upcall_by_value_groups_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_BY_VALUE_GROUPS")
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
    Group { size: u32, align: u32 },
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
    if matches!(kind, LAYOUT_STRUCT | LAYOUT_UNION) && upcall_by_value_groups_enabled() {
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

/// `CRATONVM_FFM_UPCALL_CHECK_LAYOUTS` (default on; round 13 wave 9, lane
/// ffm5): `Linker.upcallStub` runs [`check_linker_layouts`] first, as
/// `AbstractLinker.upcallStub` runs `checkLayouts`. `0` accepts any value
/// layout of a supported carrier again (and refuses a sequence with the
/// by-value-group `UnsupportedOperationException`). Read per stub creation.
fn upcall_check_layouts_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_CHECK_LAYOUTS")
}

/// `CRATONVM_FFM_DOWNCALL_CHECK_LAYOUTS` (default on; round 13 wave 9, lane
/// ffm5): `Linker.downcallHandle` runs [`check_linker_layouts`] before it reads
/// the options, as `AbstractLinker.downcallHandle0` runs `checkLayouts`. `0`
/// links a non-canonical value layout again (the call then marshals the other
/// byte order where HotSpot refuses to link). Read per handle creation.
pub(crate) fn downcall_check_layouts_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_DOWNCALL_CHECK_LAYOUTS")
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
    let groups = check_layouts_groups_enabled();
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

/// `CRATONVM_FFM_CHECK_LAYOUTS_GROUPS` (default on; round 13 wave 11, lane
/// ffm6): [`check_linker_layouts`] walks struct, union and nested sequence
/// layouts as JDK 25 `AbstractLinker.checkLayoutRecursive` does -- natural
/// alignment, member offsets, padding, trailing size -- and checks every value
/// layout inside them. Before, a group was never checked: a struct without its
/// trailing padding (`structLayout(JAVA_LONG, JAVA_INT)`, 12 bytes where C
/// has 16) or with a big-endian member linked, and libffi then marshalled the
/// C layout, not the Java one. `0` checks top-level value layouts only, as in
/// wave 9. Read per link request.
fn check_layouts_groups_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_CHECK_LAYOUTS_GROUPS")
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
            .filter_map(|i| match ctx.get_array_element(array, i) {
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
    address_target_layout(ctx, layout)
        .map_or(0, |target| foreign_ffm::p67_layout_size_of(ctx, target).max(0))
}

/// `CRATONVM_FFM_UPCALL_ADDRESS_ALIGN_CHECK` (default on; round 13 wave 13,
/// lane ffm7): an address argument C hands an upcall is checked against its
/// target layout's alignment, as JDK 25 `Utils.longToAddress(addr, size,
/// align)` (the upcall's `BoxAddress` binding) checks it: a misaligned one is
/// `IllegalArgumentException: Invalid alignment constraint for address: 0x..`
/// inside the upcall, i.e. the uncaught-exception exit. `0` hands the target
/// the misaligned segment as before. Read per stub creation.
fn upcall_address_align_check() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_ADDRESS_ALIGN_CHECK")
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
        let layout =
            layout.ok_or_else(|| illegal_argument(format!("Upcall parameter {i} layout is null")))?;
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
    let array = ctx.new_ref_array(object_class, args.len());
    let array_pin = ctx.pin_native_root(array);
    for (i, value) in args.iter().enumerate() {
        let element = match (*value, pins.get(i).copied().flatten()) {
            (Value::Object(Some(obj)), Some(pin)) => {
                Value::Object(Some(ctx.read_native_pin(pin, obj)))
            }
            (Value::Object(None), _) => Value::Object(None),
            (primitive, _) => {
                let desc = shape.params.get(i).map_or("I", |p| p.kind.descriptor());
                crate::lang_class::box_value(ctx, primitive, desc)
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

/// [`invoke_synthetic`] when `lang_invoke::mh_dispatch` can run `target`: it
/// carries `MH_CLASS` (without it `mh_dispatch` answers `null` for everything)
/// or is a downcall handle. Otherwise [`invoke_generic`].
fn choose_invoker(ctx: &dyn NativeContext, target: ObjectRef) -> UpcallInvoker {
    let invoker: UpcallInvoker = if crate::panama::is_downcall_handle(ctx, target)
        || crate::lang_invoke::mh_read_class(ctx, target).is_some()
    {
        invoke_synthetic
    } else {
        invoke_generic
    };
    invoker
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
}

/// `CRATONVM_FFM_UPCALL_DIRECT_STATIC` (default on; round 14 wave 1, lane
/// ffm): a stub over a plain `findStatic` handle invokes its member directly
/// ([`DirectStatic`]). `0` dispatches every upcall through the stub's invoker
/// (`mh_dispatch`), as before. Read per stub creation.
fn upcall_direct_static_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_DIRECT_STATIC")
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
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_RETIRE_AT_CLOSE")
    })
}

/// `CRATONVM_FFM_UPCALL_CREATE_WINDOW` (default on; round 13 wave 9, lane
/// ffm5): `Linker.upcallStub` holds an `ffm_fast::AccessWindow` from the
/// arena's liveness check to the stub's registration, so a racing shared close
/// retires the new stub or refuses its creation (see `create_upcall_stub`).
/// `0` restores the unguarded check. Read per stub creation.
fn upcall_create_window() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_CREATE_WINDOW")
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_AUTO_ARENA_FREE")
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
    // `checkLayouts(function)` comes first (round 13 wave 9, lane ffm5).
    if upcall_check_layouts_enabled() {
        check_linker_layouts(ctx, descriptor)?;
    }
    let (shape, ffi_types) = upcall_shape_and_types(ctx, descriptor)?;
    // `SharedUtils.checkExceptions(target)` (round 13 wave 1, lane mhffm): a
    // direct handle whose member declares ANY exception (`throws` clause,
    // checked or not) is refused; an adapted or bound handle is "unknown"
    // and passes, as on HotSpot.
    if upcall_check_exceptions_enabled() {
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
    if let Some(options) = options {
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
    let invoker = choose_invoker(ctx, target);
    if upcall_type_check_enabled() {
        if let Some((actual, named)) = target_method_descriptor(ctx, target) {
            let object_is_evidence = named && upcall_type_check_object_enabled();
            if upcall_type_mismatch(&shape.method_descriptor(), &actual, object_is_evidence) {
                return Err(illegal_argument(format!(
                    "Wrong method handle type: {actual} (the descriptor needs {})",
                    shape.method_descriptor()
                )));
            }
        }
    }
    // Round 13 wave 9 (lane ffm5): stubs of collected `Arena.ofAuto()` arenas.
    // No Java and no allocation, so `target` and `arena` stay current.
    reclaim_auto_upcall_stubs(ctx);
    // The validity check can raise (and so allocate); nothing below holds a
    // raw reference across it.
    let target_pin = ctx.pin_native_root(target);
    let arena_pin = ctx.pin_native_root(arena);
    // Round 13 wave 9 (lane ffm5): an access window from before the arena's
    // liveness check until the stub is in the registry
    // (`CRATONVM_FFM_UPCALL_CREATE_WINDOW`). A SHARED close on another thread
    // flips the state, waits for every window open at its probe, and only
    // then walks the list that retires the arena's stubs. Without the window
    // a stub whose check passed just before that close was installed after
    // the walk: never retired, never freed, callable after the close. With
    // it, the close either waits for the install (and its walk retires the
    // stub) or this check reads the arena as closed and throws. Nothing under
    // it runs Java; it is dropped before the segment allocation.
    let create_window = upcall_create_window().then(crate::ffm_fast::AccessWindow::open);
    if let Some((mut session, _)) = modelled_arena_session(ctx, arena) {
        if let Err(err) = foreign_ffm::p67_session_check_valid(ctx, &mut session) {
            ctx.unpin_native_roots(target_pin);
            return Err(err);
        }
    }
    let target = ctx.read_native_pin(target_pin, target);
    let arena = ctx.read_native_pin(arena_pin, arena);
    let session = match modelled_arena_session(ctx, arena) {
        Some((session, true)) => {
            let state_slot = foreign_ffm::p67_session_slots(ctx, session).state;
            Some((session, state_slot))
        }
        _ => None,
    };

    reclaim_closed_upcall_stubs(ctx);
    // Round 14 wave 1 (lane ffm): field and string reads only (no GC point),
    // so `target` is still current for the install below.
    let direct = if upcall_direct_static_enabled() {
        crate::lang_invoke::mh_plain_static_leaf(ctx, target)
            .filter(|(_, _, desc, _)| *desc == shape.method_descriptor())
    } else {
        None
    };
    let code = install_upcall_stub_typed(ctx, target, shape, ffi_types, session, invoker);
    if let Some((class, name, desc, owner)) = direct {
        // Before the stub is handed to Java: nothing can call it yet.
        if let Some(entry) = registry().lock().get(&code) {
            let _ = entry.data().direct.set(DirectStatic {
                class,
                name,
                desc,
                owner,
                settled: std::sync::atomic::AtomicU32::new(0),
            });
        }
    }
    drop(create_window);

    // The only allocation: the target and the session are registry roots by
    // now, and the arena is still pinned.
    let segment = crate::panama::alloc_segment_carrier(ctx, 6);
    let arena = ctx.read_native_pin(arena_pin, arena);
    ctx.unpin_native_roots(target_pin);
    let segment = match segment {
        Ok(segment) => segment,
        Err(err) => {
            remove_upcall_stub(code);
            return Err(err);
        }
    };
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

/// The native segment an address argument becomes: `size` bytes at `address`,
/// no arena (always alive), as `Utils.longToAddress` makes it on HotSpot.
fn new_arg_segment(
    ctx: &mut dyn NativeContext,
    address: i64,
    size: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let segment = crate::panama::alloc_segment_carrier(ctx, 6)?;
    ctx.set_field(segment, 0, Value::Long(address));
    ctx.set_field(segment, 1, Value::Long(size));
    ctx.set_field(segment, 2, Value::Object(None));
    ctx.set_field(segment, 3, Value::Int(0));
    ctx.set_field(segment, 4, Value::Int(1));
    ctx.set_field(segment, 5, Value::Long(0));
    Ok(segment)
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
            // `SharedUtils.unboxSegment`: a heap segment has no address.
            if crate::panama::is_alias_heap_segment(ctx, segment)
                || plf::is_real_heap_segment(ctx, segment)
            {
                return Err(illegal_argument(
                    "Heap segment not allowed as an upcall return".to_string(),
                ));
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
unsafe fn new_group_arg_segment(
    ctx: &mut dyn NativeContext,
    arena: &mut Option<(usize, ObjectRef)>,
    pin_base: &mut Option<usize>,
    src: *const u8,
    size: u32,
    align: u32,
) -> Result<ObjectRef, MethodCallFailed> {
    let current = match *arena {
        Some((pin, made)) => ctx.read_native_pin(pin, made),
        None => {
            let made = foreign_ffm::p67_new_vm_confined_arena(ctx)?;
            let pin = ctx.pin_native_root(made);
            if pin_base.is_none() {
                *pin_base = Some(pin);
            }
            *arena = Some((pin, made));
            made
        }
    };
    let allocated = crate::panama::pe_arena_allocate(
        ctx,
        &[
            Value::Object(Some(current)),
            Value::Long(i64::from(size)),
            Value::Long(i64::from(align)),
        ],
    )?;
    let Some(Value::Object(Some(segment))) = allocated else {
        return Err(RuntimeError::IllegalStateException {
            message: "upcall: no segment for a by-value group argument".into(),
        }
        .into());
    };
    let address = plf::segment_address(ctx, segment);
    if address == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "upcall: a by-value group argument's segment has no address".into(),
        }
        .into());
    }
    // SAFETY: the caller's contract for `src`; `address` is a fresh native
    // block of `size` bytes this arena owns until it is closed.
    unsafe { std::ptr::copy_nonoverlapping(src, address as usize as *mut u8, size as usize) };
    Ok(segment)
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
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!(
                "Out of bound access on segment MemorySegment{{ byteSize: {available} }}; \
                 new offset = 0; new length = {size}"
            )),
        }
        .into());
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
            message: "upcall: the returned segment has no address".into(),
        }
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
    let mut values: Vec<Value> = Vec::with_capacity(params.len());
    for (i, param) in params.iter().enumerate() {
        // SAFETY: the caller's contract.
        let slot = unsafe { *args.add(i) };
        // SAFETY: `slot` points at a value of `param.kind`'s C type.
        values.push(unsafe { decode_arg(param.kind, slot) });
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
    for (i, param) in params.iter().enumerate() {
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
            // SAFETY: `raw` is libffi's pointer to the aggregate (`decode_arg`).
            UpcallKind::Group { size, align } => unsafe {
                new_group_arg_segment(
                    ctx,
                    &mut arena,
                    &mut pin_base,
                    raw as usize as *const u8,
                    size,
                    align,
                )
            },
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
                let err = close_upcall_arena(ctx, arena, err);
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
        return Err(UpcallFatal::Unsupported("the upcall stub has no target".into()));
    }
    // SAFETY: stored from a live `ObjectRef` of this VM (checked above), and
    // kept alive and current by `gc_scan_roots` / `gc_update_refs`.
    let target = unsafe { ObjectRef::from_raw(target as *mut u8) };
    // Round 14 wave 1 (lane ffm): a plain direct static target skips
    // `mh_dispatch` (`DirectStatic`, `CRATONVM_FFM_UPCALL_DIRECT_STATIC`).
    let invoked = match data.direct.get() {
        Some(direct) => {
            let settled = match direct.settled.load(Ordering::Relaxed) {
                0 => None,
                raw => Some(cratonvm_types::ClassId::new(raw)),
            };
            let (result, now) = ctx.invoke_static_settling(
                direct.owner,
                settled,
                &direct.class,
                &direct.name,
                &direct.desc,
                &values,
            );
            if let Some(owner) = now {
                direct.settled.store(owner.as_u32(), Ordering::Relaxed);
            }
            result
        }
        None => (data.invoker)(ctx, target, &values, &data.shape),
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
                let _ = foreign_ffm::p67_close_vm_arena(ctx, made);
                Ok(bits)
            }
            None => Ok(bits),
        },
        Err(err) => Err(close_upcall_arena(ctx, arena, err)),
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
) -> MethodCallFailed {
    let Some((pin, made)) = arena else {
        return failure;
    };
    let made = ctx.read_native_pin(pin, made);
    match failure {
        MethodCallFailed::ExceptionThrown(thrown) => {
            let thrown_pin = ctx.pin_native_root(thrown);
            let _ = foreign_ffm::p67_close_vm_arena(ctx, made);
            let thrown = ctx.read_native_pin(thrown_pin, thrown);
            ctx.unpin_native_roots(thrown_pin);
            MethodCallFailed::ExceptionThrown(thrown)
        }
        other => {
            let _ = foreign_ffm::p67_close_vm_arena(ctx, made);
            other
        }
    }
}

/// HotSpot's `SharedUtils.handleUncaughtException`: the trace, the message,
/// `System.exit(1)`. Returns only when the exit is soft-returned
/// (`CRATONVM_SOFT_EXIT`); the stub then hands C a zero.
fn terminate_uncaught(ctx: &mut dyn NativeContext, failure: MethodCallFailed) {
    const MESSAGE: &str = "Unrecoverable uncaught exception encountered. The VM will now exit";
    let java_err = upcall_uncaught_java_err();
    match failure {
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
    }
    if !(java_err && print_to_system_err(ctx, MESSAGE)) {
        eprintln!("{MESSAGE}");
    }
    let _ = crate::lang_system::native_runtime_exit(ctx, &[Value::Int(1)]);
}

/// `CRATONVM_FFM_UPCALL_UNCAUGHT_JAVA_ERR` (default on; round 13 wave 9, lane
/// ffm5): the uncaught-upcall report goes where HotSpot's
/// `SharedUtils.handleUncaughtException` sends it -- `t.printStackTrace()`,
/// then `System.err.println(..)`, both through the program's `System.err`
/// (which it may have replaced with `System.setErr`) -- and a failure this VM
/// raised natively (e.g. `IllegalArgumentException: Heap segment not allowed
/// as an upcall return`) is printed as the Java exception it stands for, not
/// as `Exception in upcall: <VM error>`. `0` writes both lines to the process
/// stderr as before. Read only on that exit path.
fn upcall_uncaught_java_err() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_UNCAUGHT_JAVA_ERR")
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

/// `CRATONVM_FFM_UPCALL_ATTACH` (default on; round 13 wave 1, lane mhffm):
/// an upcall on a thread with no active downcall is run through the VM's
/// [`UpcallAttachHook`] when one is installed. `0` restores the loud exit.
/// Read only on that path.
fn upcall_attach_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_ATTACH")
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
    if ctx.vm_identity() != data.vm && upcall_cross_vm_enabled() {
        // Round 14 wave 1 (lane ffm): the wait is GC-safe in this VM, so a
        // stop-the-world pause of this VM may run meanwhile -- an A -> B -> A
        // chain otherwise hangs (A's pause waits for this thread, which waits
        // for B's helper, which waits for A's helper, which waits for the
        // pause). Every reference the downcall uses after the C call is
        // pinned (a same-thread upcall can already collect), and
        // `upcall_entry` calls this outside any blocking region in both
        // downcall modes (a GC-safe downcall's region is left before it)
        // (`r13w8-ffm4-cross-vm-upcall-wait-gc-safe-patch-FIXED-20260929.md`).
        let gc_safe_wait = upcall_cross_vm_gc_safe_wait();
        if gc_safe_wait {
            ctx.begin_blocking_region();
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
    match unsafe { upcall_dispatch(ctx, data, args) } {
        Ok(bits) => Some(bits),
        Err(fatal) => {
            upcall_fatal(ctx, data, fatal);
            None
        }
    }
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
    if !upcall_attach_enabled() {
        return None;
    }
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

/// `CRATONVM_FFM_UPCALL_CROSS_VM` (default on; round 13 wave 8, lane ffm4):
/// an upcall stub of VM B called on a thread that is bound to VM A -- inside
/// one of A's downcalls, or from a JNI native of A -- runs on a helper thread
/// attached to B ([`upcall_on_helper_thread`]). `0` restores the loud exit
/// (`r13w3-ffm2-upcall-stub-of-another-vm-inside-a-downcall`). Read only on
/// those paths.
fn upcall_cross_vm_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_CROSS_VM")
}

/// `CRATONVM_FFM_UPCALL_CROSS_VM_GC_SAFE_WAIT` (default on; round 14 wave 1,
/// lane ffm): the calling thread waits for a cross-VM upcall's helper inside
/// a GC blocking region of its own VM ([`run_upcall`]). `0` restores the
/// counted-mutator wait. Read only on that path.
fn upcall_cross_vm_gc_safe_wait() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_UPCALL_CROSS_VM_GC_SAFE_WAIT")
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
    if upcall_cross_vm_enabled() && upcall_attach_enabled() && UPCALL_ATTACH_HOOK.get().is_some() {
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
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use crate::try_alloc_concurrent_synthetic;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn param(kind: UpcallKind) -> UpcallParam {
        UpcallParam {
            kind,
            target_size: 0,
            target_align: 1,
        }
    }

    fn fake_target(ctx: &mut dyn NativeContext) -> ObjectRef {
        try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 2).unwrap()
    }

    // The fake targets below run inside an `extern "C"` callback, where a
    // panic aborts the whole test binary. So none panics: an unexpected
    // argument list answers a value the test's own assertion then rejects.

    /// `(int, int) -> int`: the sum, wrapping.
    fn add_ints(
        _ctx: &mut dyn NativeContext,
        _target: ObjectRef,
        args: &[Value],
        _shape: &UpcallShape,
    ) -> MethodCallResult {
        Ok(Some(match args {
            [Value::Int(a), Value::Int(b)] => Value::Int(a.wrapping_add(*b)),
            _ => Value::Int(-999),
        }))
    }

    /// `(double, int) -> double`: `x * k + 0.25`.
    fn scale(
        _ctx: &mut dyn NativeContext,
        _target: ObjectRef,
        args: &[Value],
        _shape: &UpcallShape,
    ) -> MethodCallResult {
        Ok(Some(match args {
            [Value::Double(x), Value::Int(k)] => Value::Double(x * f64::from(*k) + 0.25),
            _ => Value::Double(f64::NAN),
        }))
    }

    /// `(address, address) -> int`: a `qsort` comparator over two `int`s. A
    /// segment that is not 4 bytes (the target layout's size) answers "equal"
    /// for everything, which leaves the array unsorted.
    fn compare_ints(
        ctx: &mut dyn NativeContext,
        _target: ObjectRef,
        args: &[Value],
        _shape: &UpcallShape,
    ) -> MethodCallResult {
        let [Value::Object(Some(a)), Value::Object(Some(b))] = args else {
            return Ok(Some(Value::Int(0)));
        };
        if ctx.get_field(*a, 1) != Value::Long(4) || ctx.get_field(*b, 1) != Value::Long(4) {
            return Ok(Some(Value::Int(0)));
        }
        let (Value::Long(pa), Value::Long(pb)) = (ctx.get_field(*a, 0), ctx.get_field(*b, 0)) else {
            return Ok(Some(Value::Int(0)));
        };
        // SAFETY: the test hands the stub pointers into a live `i32` array.
        let (x, y) = unsafe { (*(pa as *const i32), *(pb as *const i32)) };
        Ok(Some(Value::Int(x.cmp(&y) as i32)))
    }

    /// Throws: the result of every call is an exception.
    fn throws(
        _ctx: &mut dyn NativeContext,
        _target: ObjectRef,
        _args: &[Value],
        _shape: &UpcallShape,
    ) -> MethodCallResult {
        Err(RuntimeError::IllegalStateException {
            message: "boom".into(),
        }
        .into())
    }

    #[test]
    fn layout_kinds_map_to_the_java_carriers() {
        assert_eq!(UpcallKind::from_layout_kind(LAYOUT_INT), Some(UpcallKind::Int));
        assert_eq!(UpcallKind::from_layout_kind(LAYOUT_CHAR), Some(UpcallKind::Char));
        assert_eq!(UpcallKind::from_layout_kind(LAYOUT_ADDRESS), Some(UpcallKind::Address));
        assert_eq!(UpcallKind::from_layout_kind(LAYOUT_STRUCT), None);
        assert_eq!(UpcallKind::from_layout_kind(plf::LAYOUT_UNKNOWN), None);
        let shape = UpcallShape {
            params: vec![
                param(UpcallKind::Address),
                param(UpcallKind::Char),
                param(UpcallKind::Boolean),
            ],
            ret: Some(UpcallKind::Double),
        };
        assert_eq!(
            shape.method_descriptor(),
            "(Ljava/lang/foreign/MemorySegment;CZ)D"
        );
        let void = UpcallShape {
            params: Vec::new(),
            ret: None,
        };
        assert_eq!(void.method_descriptor(), "()V");
    }

    #[test]
    fn type_mismatch_refuses_only_what_it_is_sure_of() {
        let want = "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/MemorySegment;)I";
        assert!(!upcall_type_mismatch(want, want, false));
        // A parameter too many or too few.
        assert!(upcall_type_mismatch(want, "(Ljava/lang/foreign/MemorySegment;)I", false));
        // A primitive where the descriptor wants another.
        assert!(upcall_type_mismatch(
            want,
            "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/MemorySegment;)J",
            false
        ));
        assert!(upcall_type_mismatch("(I)V", "(J)V", false));
        // `Object` is what an unreadable mirror renders as: not evidence.
        assert!(!upcall_type_mismatch(
            want,
            "(Ljava/lang/Object;Ljava/lang/foreign/MemorySegment;)I",
            false
        ));
        // Unparseable: not evidence either.
        assert!(!upcall_type_mismatch(want, "garbage", false));
    }

    /// Round 13 wave 5 (lane ffm3): when every mirror of the target's type was
    /// named, an `Object` is a real `Object` -- HotSpot's `MethodType.equals`
    /// refuses `(Object)V` for a `(JAVA_INT)V` descriptor. Equal types and
    /// unparseable ones still pass.
    #[test]
    fn a_named_object_is_evidence_of_a_mismatch() {
        assert!(upcall_type_mismatch("(I)V", "(Ljava/lang/Object;)V", true));
        assert!(upcall_type_mismatch(
            "(Ljava/lang/foreign/MemorySegment;)I",
            "(Ljava/lang/foreign/MemorySegment;)Ljava/lang/Object;",
            true
        ));
        assert!(!upcall_type_mismatch("(I)V", "(Ljava/lang/Object;)V", false));
        assert!(!upcall_type_mismatch("(I)V", "(I)V", true));
        assert!(!upcall_type_mismatch("(I)V", "garbage", true));
    }

    #[test]
    fn return_bits_widen_as_libffi_expects() {
        let ctx = mock_ctx();
        let ctx: &dyn NativeContext = &ctx;
        assert_eq!(return_bits(ctx, UpcallKind::Int, Value::Int(-2)).unwrap(), u64::MAX - 1);
        assert_eq!(return_bits(ctx, UpcallKind::Byte, Value::Int(-1)).unwrap(), u64::MAX);
        assert_eq!(return_bits(ctx, UpcallKind::Byte, Value::Int(0x17F)).unwrap(), 0x7F);
        assert_eq!(return_bits(ctx, UpcallKind::Short, Value::Int(-3)).unwrap(), u64::MAX - 2);
        // `char` is unsigned: 0xFFFE stays 0xFFFE.
        assert_eq!(return_bits(ctx, UpcallKind::Char, Value::Int(0xFFFE)).unwrap(), 0xFFFE);
        assert_eq!(return_bits(ctx, UpcallKind::Boolean, Value::Int(1)).unwrap(), 1);
        assert_eq!(return_bits(ctx, UpcallKind::Long, Value::Long(-5)).unwrap(), (-5i64) as u64);
        assert_eq!(
            return_bits(ctx, UpcallKind::Float, Value::Float(1.5)).unwrap(),
            u64::from(1.5f32.to_bits())
        );
        assert_eq!(
            return_bits(ctx, UpcallKind::Double, Value::Double(-0.0)).unwrap(),
            (-0.0f64).to_bits()
        );
        // A mismatched result is refused, not coerced to 0.
        assert!(return_bits(ctx, UpcallKind::Int, Value::Double(1.0)).is_err());
        // A null segment is the JDK's NPE.
        assert!(matches!(
            return_bits(ctx, UpcallKind::Address, Value::Object(None)),
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::NullPointerException { .. }
            )))
        ));
    }

    #[test]
    fn decode_arg_reads_each_c_type_with_its_extension() {
        let b: i8 = -7;
        let s: i16 = -300;
        let c: u16 = 0xFFFE;
        let z: u8 = 1;
        let l: i64 = -1 << 40;
        let f: f32 = 2.5;
        let d: f64 = -0.125;
        let p: usize = 0x1234_5678;
        /// `decode_arg` of a live local, which is the C type `kind` names.
        fn read<T>(kind: UpcallKind, value: &T) -> Value {
            // SAFETY: `value` is a live `T`, and every call below pairs `T`
            // with the C type of `kind`.
            unsafe { decode_arg(kind, (value as *const T).cast()) }
        }
        assert_eq!(read(UpcallKind::Byte, &b), Value::Int(-7));
        assert_eq!(read(UpcallKind::Short, &s), Value::Int(-300));
        assert_eq!(read(UpcallKind::Char, &c), Value::Int(0xFFFE));
        assert_eq!(read(UpcallKind::Boolean, &z), Value::Int(1));
        assert_eq!(read(UpcallKind::Long, &l), Value::Long(-1 << 40));
        assert_eq!(read(UpcallKind::Float, &f), Value::Float(2.5));
        assert_eq!(read(UpcallKind::Double, &d), Value::Double(-0.125));
        assert_eq!(read(UpcallKind::Address, &p), Value::Long(0x1234_5678));
    }

    /// The whole path: a libffi closure called as a C function pointer, with
    /// the downcall's context installed, reaching the invoker and returning.
    #[test]
    fn a_stub_called_from_c_runs_the_target_and_returns_its_int() {
        let mut ctx = mock_ctx();
        let target = fake_target(&mut ctx);
        let shape = UpcallShape {
            params: vec![param(UpcallKind::Int), param(UpcallKind::Int)],
            ret: Some(UpcallKind::Int),
        };
        let code = install_upcall_stub(&ctx, target, shape, None, add_ints);
        assert!(upcall_stub_registered(code));
        let result = {
            let _guard = plf::ActiveContextGuard::install(&mut ctx);
            // SAFETY: the stub's CIF is `int(int, int)`.
            let f: extern "C" fn(i32, i32) -> i32 = unsafe { std::mem::transmute(code) };
            (f(40, 2), f(i32::MAX, 1))
        };
        assert_eq!(result, (42, i32::MIN));
        // Not followed by `!upcall_stub_registered(code)`: a test running in
        // parallel may be handed the freed trampoline's address at once.
        remove_upcall_stub(code);
    }

    /// The attach hook the cross-VM test installs: a fresh context of the
    /// requested VM on the calling (helper) thread, as the VM's hook attaches
    /// that thread. No other test in this binary reaches the hook.
    fn mock_attach_hook(vm: usize, run: &mut dyn FnMut(&mut dyn NativeContext)) -> bool {
        let mut ctx = mock_ctx();
        ctx.set_vm_identity(vm);
        run(&mut ctx);
        true
    }

    /// Round 13 wave 8 (lane ffm4): a stub of VM B called inside a downcall
    /// of VM A runs (on a helper attached to B) instead of exiting. A's
    /// thread waits in one blocking region of A (round 14 wave 1, lane ffm:
    /// `r13w8-ffm4-cross-vm-upcall-wait-gc-safe-patch-FIXED-20260929.md`).
    #[test]
    fn another_vms_stub_inside_a_downcall_runs_on_a_helper_of_that_vm() {
        set_upcall_attach_hook(mock_attach_hook);
        let mut vm_a = mock_ctx();
        vm_a.set_vm_identity(0x7A80_0810);
        let mut vm_b = mock_ctx();
        vm_b.set_vm_identity(0x7A80_0811);
        let target = fake_target(&mut vm_b);
        let shape = UpcallShape {
            params: vec![param(UpcallKind::Int), param(UpcallKind::Int)],
            ret: Some(UpcallKind::Int),
        };
        let code = install_upcall_stub(&vm_b, target, shape, None, add_ints);
        let got = {
            let _guard = plf::ActiveContextGuard::install(&mut vm_a);
            // SAFETY: the stub's CIF is `int(int, int)`.
            let f: extern "C" fn(i32, i32) -> i32 = unsafe { std::mem::transmute(code) };
            f(40, 2)
        };
        assert_eq!(got, 42);
        assert_eq!(vm_a.blocking_region_counts(), (1, 1));
        remove_upcall_stub(code);
    }

    /// `(int) -> void`: marks the target, so a test can tell the invoker ran.
    fn mark_target(
        ctx: &mut dyn NativeContext,
        target: ObjectRef,
        _args: &[Value],
        _shape: &UpcallShape,
    ) -> MethodCallResult {
        ctx.set_field(target, 0, Value::Int(77));
        Ok(None)
    }

    /// Round 14 wave 1 (lane ffm): a stub with a [`DirectStatic`] answers
    /// through `invoke_static_settling` and never reaches its invoker
    /// (`r13w13-ffm7-upcall-per-call-cost`); without one the invoker runs.
    #[test]
    fn a_direct_static_stub_skips_its_invoker() {
        let mut ctx = mock_ctx();
        let target = fake_target(&mut ctx);
        ctx.set_field(target, 0, Value::Int(0));
        let shape = UpcallShape {
            params: vec![param(UpcallKind::Int)],
            ret: None,
        };
        let direct_code = install_upcall_stub(&ctx, target, shape.clone(), None, mark_target);
        let set = registry().lock().get(&direct_code).is_some_and(|entry| {
            entry
                .data()
                .direct
                .set(DirectStatic {
                    class: "p/Callbacks".into(),
                    name: "onInt".into(),
                    desc: "(I)V".into(),
                    owner: None,
                    settled: std::sync::atomic::AtomicU32::new(0),
                })
                .is_ok()
        });
        assert!(set);
        {
            let _guard = plf::ActiveContextGuard::install(&mut ctx);
            // SAFETY: the stub's CIF is `void(int)`.
            let f: extern "C" fn(i32) = unsafe { std::mem::transmute(direct_code) };
            f(5);
        }
        assert_eq!(ctx.get_field(target, 0), Value::Int(0), "the invoker ran");
        let plain_code = install_upcall_stub(&ctx, target, shape, None, mark_target);
        {
            let _guard = plf::ActiveContextGuard::install(&mut ctx);
            // SAFETY: as above.
            let f: extern "C" fn(i32) = unsafe { std::mem::transmute(plain_code) };
            f(5);
        }
        assert_eq!(ctx.get_field(target, 0), Value::Int(77));
        remove_upcall_stub(direct_code);
        remove_upcall_stub(plain_code);
    }

    /// Stubs registered for `vm` (tests give themselves a private VM identity,
    /// so this is not disturbed by tests running in parallel).
    fn stubs_of(vm: usize) -> usize {
        registry().lock().values().filter(|e| e.vm == vm).count()
    }

    #[test]
    fn a_stub_returning_double_hands_c_the_double() {
        let mut ctx = mock_ctx();
        let target = fake_target(&mut ctx);
        let shape = UpcallShape {
            params: vec![param(UpcallKind::Double), param(UpcallKind::Int)],
            ret: Some(UpcallKind::Double),
        };
        let code = install_upcall_stub(&ctx, target, shape, None, scale);
        let got = {
            let _guard = plf::ActiveContextGuard::install(&mut ctx);
            // SAFETY: the stub's CIF is `double(double, int)`.
            let f: extern "C" fn(f64, i32) -> f64 = unsafe { std::mem::transmute(code) };
            f(1.5, 3)
        };
        assert_eq!(got, 1.5 * 3.0 + 0.25);
        remove_upcall_stub(code);
    }

    /// `qsort` itself, with the stub as its comparator: address arguments
    /// become segments of the target layout's size.
    #[test]
    fn qsort_sorts_through_an_address_comparator_stub() {
        extern "C" {
            fn qsort(
                base: *mut c_void,
                n: usize,
                size: usize,
                cmp: extern "C" fn(*const c_void, *const c_void) -> i32,
            );
        }
        let mut ctx = mock_ctx();
        let target = fake_target(&mut ctx);
        let address = UpcallParam {
            kind: UpcallKind::Address,
            target_size: 4,
            target_align: 4,
        };
        let shape = UpcallShape {
            params: vec![address, address],
            ret: Some(UpcallKind::Int),
        };
        let code = install_upcall_stub(&ctx, target, shape, None, compare_ints);
        let mut data: Vec<i32> = (0..200).map(|i| (i * 7919 % 211) - 100).collect();
        let mut want = data.clone();
        want.sort_unstable();
        {
            let _guard = plf::ActiveContextGuard::install(&mut ctx);
            // SAFETY: the stub's CIF is `int(void*, void*)`.
            let cmp: extern "C" fn(*const c_void, *const c_void) -> i32 =
                unsafe { std::mem::transmute(code) };
            // SAFETY: `data` is a live, correctly sized `int` array.
            unsafe { qsort(data.as_mut_ptr().cast(), data.len(), 4, cmp) };
        }
        assert_eq!(data, want);
        // Every segment the comparator received was unpinned again.
        remove_upcall_stub(code);
    }

    #[test]
    fn an_exception_from_the_target_is_reported_as_uncaught() {
        let mut ctx = mock_ctx();
        let target = fake_target(&mut ctx);
        let shape = UpcallShape {
            params: vec![param(UpcallKind::Int)],
            ret: Some(UpcallKind::Int),
        };
        let code = install_upcall_stub(&ctx, target, shape, None, throws);
        let registry_data = registry().lock().get(&code).map(|e| e.data as *const UpcallStubData);
        let data = registry_data.expect("registered");
        let arg: i32 = 5;
        let args = [(&arg as *const i32).cast::<c_void>()];
        // SAFETY: `data` is alive while its entry is registered; `args` holds
        // one pointer to an `int`, the stub's one parameter.
        let outcome = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
        assert!(matches!(outcome, Err(UpcallFatal::Uncaught(_))));
        remove_upcall_stub(code);
    }

    /// A stub whose arena's close walk ran refuses to run, and the sweep
    /// frees it -- but not while an upcall of it is in flight. A state word
    /// of 0 alone (a close still deciding, which may reopen the session) is
    /// not death (round 13 wave 8, lane ffm4).
    #[test]
    fn a_closed_arenas_stub_is_dead_and_is_reclaimed() {
        let mut ctx = mock_ctx();
        // Private to this test: the registry is process-wide and the sweep
        // reads the session through this context.
        ctx.set_vm_identity(0x7A80_0701);
        let target = fake_target(&mut ctx);
        let Value::Object(Some(session)) = foreign_ffm::p67_memory_session(&mut ctx).unwrap() else {
            panic!("no session");
        };
        let state_slot = foreign_ffm::p67_session_slots(&ctx, session).state;
        let shape = UpcallShape {
            params: vec![param(UpcallKind::Int), param(UpcallKind::Int)],
            ret: Some(UpcallKind::Int),
        };
        let code = install_upcall_stub(&ctx, target, shape, Some((session, state_slot)), add_ints);
        let data = registry()
            .lock()
            .get(&code)
            .map(|e| e.data as *const UpcallStubData)
            .expect("registered");
        // Open: runs, and is not reclaimable.
        assert_eq!(reclaim_closed_upcall_stubs(&ctx), 0);
        let (a, b) = (3i32, 4i32);
        let args = [(&a as *const i32).cast::<c_void>(), (&b as *const i32).cast::<c_void>()];
        // SAFETY: registered, and `args` matches `int(int, int)`.
        let open = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
        assert_eq!(open.unwrap(), 7);
        // State word 0, walk not run: still alive, not reclaimable.
        ctx.set_field(session, state_slot, Value::Int(0));
        // SAFETY: as above.
        let closing = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
        assert_eq!(closing.unwrap(), 7);
        assert_eq!(reclaim_closed_upcall_stubs(&ctx), 0);
        // In flight when the walk retires it: kept, but refuses to run.
        // SAFETY: still registered.
        unsafe { (*data).in_flight.fetch_add(1, Ordering::SeqCst) };
        assert_eq!(retire_upcall_stubs_of_session(&ctx, session), 0);
        assert!(upcall_stub_registered(code));
        // SAFETY: still registered.
        let closed = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
        assert!(matches!(closed, Err(UpcallFatal::Unsupported(_))));
        // SAFETY: still registered.
        unsafe { (*data).in_flight.fetch_sub(1, Ordering::SeqCst) };
        // Idle and closed: freed.
        assert_eq!(reclaim_closed_upcall_stubs(&ctx), 1);
        assert_eq!(stubs_of(0x7A80_0701), 0);
    }

    /// The GC halves root and remap the target AND the session, for the
    /// owning VM only.
    #[test]
    fn roots_and_remap_cover_target_and_session_per_vm() {
        const VM: usize = 0x7A80_0702;
        const OTHER_VM: usize = 0x7A80_0703;
        let ctx = mock_ctx();
        ctx.set_vm_identity(VM);
        // Fake, never-dereferenced addresses: the scan and the remap only move
        // integers around.
        // SAFETY: used only as opaque keys below.
        let target = unsafe { ObjectRef::from_raw(0xABCD_0000usize as *mut u8) };
        // SAFETY: as above.
        let session = unsafe { ObjectRef::from_raw(0xABCE_0000usize as *mut u8) };
        let shape = UpcallShape {
            params: Vec::new(),
            ret: None,
        };
        let code = install_upcall_stub(&ctx, target, shape, Some((session, 0)), add_ints);

        let mut roots = Vec::new();
        gc_scan_roots(VM, &mut roots);
        let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert!(addrs.contains(&0xABCD_0000) && addrs.contains(&0xABCE_0000));
        let mut other = Vec::new();
        gc_scan_roots(OTHER_VM, &mut other);
        assert!(!other.iter().any(|r| r.as_ptr() as usize == 0xABCD_0000));

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0xABCD_0000usize, 0xABCD_8000usize);
        map.insert(0xABCE_0000usize, 0xABCE_8000usize);
        gc_update_refs(OTHER_VM, &map);
        let mut still = Vec::new();
        gc_scan_roots(VM, &mut still);
        assert!(still.iter().any(|r| r.as_ptr() as usize == 0xABCD_0000));
        gc_update_refs(VM, &map);
        let mut moved = Vec::new();
        gc_scan_roots(VM, &mut moved);
        let addrs: Vec<usize> = moved.iter().map(|r| r.as_ptr() as usize).collect();
        assert!(addrs.contains(&0xABCD_8000) && addrs.contains(&0xABCE_8000));
        // The panama.rs entry points the VM root table calls reach the same set.
        let mut via_panama = Vec::new();
        crate::panama::gc_scan_upcall_target_roots(VM, &mut via_panama);
        assert!(via_panama.iter().any(|r| r.as_ptr() as usize == 0xABCD_8000));
        remove_upcall_stub(code);
    }

    /// The descriptor carrier's layouts become the shape; an address layout's
    /// target layout sizes its segment; a group layout is refused.
    fn alloc_obj(ctx: &mut dyn NativeContext, class: &str, slots: usize) -> ObjectRef {
        try_alloc_concurrent_synthetic(ctx, class, slots).unwrap()
    }

    #[test]
    fn the_shape_is_read_off_the_function_descriptor() {
        let mut ctx = mock_ctx();
        let int_layout = alloc_obj(&mut ctx, "cratonvm/test/UpcallLayout", 2);
        ctx.set_field(int_layout, 0, Value::Int(LAYOUT_INT));
        let target_layout = alloc_obj(&mut ctx, "cratonvm/test/UpcallTargetLayout", 2);
        ctx.set_field(target_layout, 0, Value::Long(16));
        ctx.set_field(target_layout, 1, Value::Long(8));
        let address_layout = alloc_obj(&mut ctx, "cratonvm/test/UpcallAddressLayout", 5);
        ctx.set_field(address_layout, 0, Value::Int(LAYOUT_ADDRESS));
        let target_slot = foreign_ffm::p67_layout_slots(&ctx, address_layout).target;
        ctx.set_field(address_layout, target_slot, Value::Object(Some(target_layout)));
        let object_class = crate::lang_class::reflection_component_id(&mut ctx, "java/lang/Object");
        let params = ctx.new_ref_array(object_class, 2);
        ctx.set_array_element(params, 0, Value::Object(Some(address_layout)));
        ctx.set_array_element(params, 1, Value::Object(Some(int_layout)));
        let descriptor = alloc_obj(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(int_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params)));
        let shape = upcall_shape(&ctx, descriptor).unwrap();
        assert_eq!(
            shape,
            UpcallShape {
                params: vec![
                    UpcallParam {
                        kind: UpcallKind::Address,
                        target_size: 16,
                        target_align: 8,
                    },
                    param(UpcallKind::Int),
                ],
                ret: Some(UpcallKind::Int),
            }
        );

        let group = alloc_obj(&mut ctx, "cratonvm/test/UpcallGroup", 2);
        ctx.set_field(group, 0, Value::Int(LAYOUT_STRUCT));
        ctx.set_field(descriptor, 0, Value::Object(Some(group)));
        assert!(matches!(
            upcall_shape(&ctx, descriptor),
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::UnsupportedOperationException { .. }
            )))
        ));
    }

    /// `Linker.upcallStub` with an option is HotSpot's IAE, before anything
    /// is built.
    #[test]
    fn an_upcall_option_is_refused() {
        let mut ctx = mock_ctx();
        let target = fake_target(&mut ctx);
        let descriptor = alloc_obj(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        let arena = alloc_obj(&mut ctx, "cratonvm/test/UpcallArena", 2);
        let option = alloc_obj(&mut ctx, "java/lang/foreign/Linker$Option", 2);
        let object_class = crate::lang_class::reflection_component_id(&mut ctx, "java/lang/Object");
        let options = ctx.new_ref_array(object_class, 1);
        ctx.set_array_element(options, 0, Value::Object(Some(option)));
        let refused = create_upcall_stub(&mut ctx, target, descriptor, arena, Some(options));
        assert!(matches!(
            refused,
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { .. }
            )))
        ));
    }

    /// Round 13 wave 9 (lane ffm5): `AbstractLinker.checkLayouts` -- a value
    /// layout in the other byte order or without natural alignment is
    /// `Unsupported layout: <stripped layout>`, a top-level sequence is
    /// refused the same way, and a canonical layout (or a kind-tagged
    /// carrier, which records nothing to check) passes.
    #[test]
    fn a_non_canonical_value_layout_is_refused_like_check_layouts() {
        fn message(result: Result<(), MethodCallFailed>) -> Option<String> {
            match result {
                Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                    RuntimeError::IllegalArgumentException { message },
                ))) => Some(message),
                _ => None,
            }
        }
        let mut ctx = mock_ctx();
        // A fabricated carrier (`p67_layout_object`'s four slots); the class
        // name only has to classify as an `int` layout.
        let int_layout = alloc_obj(&mut ctx, "cratonvm/test/UpcallOfIntLayout", 4);
        ctx.set_field(int_layout, 0, Value::Long(4));
        ctx.set_field(int_layout, 1, Value::Long(4));
        ctx.set_field(int_layout, 2, Value::Int(i32::from(cfg!(target_endian = "little"))));
        ctx.set_field(int_layout, 3, Value::Object(None));
        let descriptor = alloc_obj(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(int_layout)));
        ctx.set_field(descriptor, 1, Value::Object(None));
        assert!(check_linker_layouts(&ctx, descriptor).is_ok(), "canonical JAVA_INT");

        // The other byte order: upper-case letter.
        ctx.set_field(int_layout, 2, Value::Int(i32::from(!cfg!(target_endian = "little"))));
        let little = foreign_ffm::p67_layout_is_little(&ctx, int_layout);
        assert_ne!(little, cfg!(target_endian = "little"), "fixture reads as foreign order");
        let want = if little {
            "Unsupported layout: i4"
        } else {
            "Unsupported layout: I4"
        };
        assert_eq!(message(check_linker_layouts(&ctx, descriptor)).as_deref(), Some(want));

        // Native order, alignment 1: the `1%` prefix.
        ctx.set_field(int_layout, 2, Value::Int(i32::from(cfg!(target_endian = "little"))));
        ctx.set_field(int_layout, 1, Value::Long(1));
        assert_eq!(
            message(check_linker_layouts(&ctx, descriptor)).as_deref(),
            Some("Unsupported layout: 1%i4")
        );

        // A kind-tagged carrier records no size: nothing to check.
        let tagged = alloc_obj(&mut ctx, "cratonvm/test/UpcallLayout", 2);
        ctx.set_field(tagged, 0, Value::Int(LAYOUT_INT));
        ctx.set_field(descriptor, 0, Value::Object(Some(tagged)));
        assert!(check_linker_layouts(&ctx, descriptor).is_ok(), "kind-tagged");

        // A sequence at the top level.
        // Five slots: the render reads the payload (2) and count (4) slots.
        let sequence = alloc_obj(&mut ctx, "cratonvm/test/UpcallSequenceLayout", 5);
        ctx.set_field(sequence, 0, Value::Int(LAYOUT_SEQUENCE));
        ctx.set_field(descriptor, 0, Value::Object(Some(sequence)));
        let refused = message(check_linker_layouts(&ctx, descriptor));
        assert!(
            refused.as_deref().is_some_and(|m| m.starts_with("Unsupported layout: ")),
            "{refused:?}"
        );
    }

    /// Round 13 wave 9 (lane ffm5): a non-closeable arena's stub is freed by
    /// the sweep once its weak session handle is cleared -- not before, not
    /// while an upcall of it runs, and never by the closed-arena sweep.
    #[test]
    fn an_auto_arenas_stub_is_freed_once_its_session_is_collected() {
        let mut ctx = mock_ctx();
        // Private to this test: the registry is process-wide.
        ctx.set_vm_identity(0x7A80_0901);
        let target = fake_target(&mut ctx);
        let Value::Object(Some(session)) = foreign_ffm::p67_memory_session(&mut ctx).unwrap() else {
            panic!("no session");
        };
        let shape = UpcallShape {
            params: vec![param(UpcallKind::Int), param(UpcallKind::Int)],
            ret: Some(UpcallKind::Int),
        };
        let code = install_upcall_stub(&ctx, target, shape, None, add_ints);
        // The mock has no weak handles: a strong one stands in, and removing
        // it is the collection clearing it.
        let handle = ctx.add_global_root(session);
        assert_ne!(handle, 0);
        assert!(set_upcall_stub_auto_session(code, handle));
        // Session alive: kept by both sweeps.
        assert_eq!(reclaim_auto_upcall_stubs(&mut ctx), 0);
        assert_eq!(reclaim_closed_upcall_stubs(&ctx), 0);
        assert!(upcall_stub_registered(code));
        assert!(ctx.remove_global_root(handle));
        let data = registry()
            .lock()
            .get(&code)
            .map(|e| e.data as *const UpcallStubData)
            .expect("registered");
        // Collected, but an upcall is running: kept.
        // SAFETY: still registered.
        unsafe { (*data).in_flight.fetch_add(1, Ordering::SeqCst) };
        assert_eq!(reclaim_auto_upcall_stubs(&mut ctx), 0);
        assert_eq!(reclaim_closed_upcall_stubs(&ctx), 0);
        assert!(upcall_stub_registered(code));
        // SAFETY: still registered.
        unsafe { (*data).in_flight.fetch_sub(1, Ordering::SeqCst) };
        // Collected and idle: freed, once.
        assert_eq!(reclaim_auto_upcall_stubs(&mut ctx), 1);
        assert_eq!(stubs_of(0x7A80_0901), 0);
        assert_eq!(reclaim_auto_upcall_stubs(&mut ctx), 0);
    }

    // -----------------------------------------------------------------------
    // Round 13 wave 11 (lane ffm6): group layouts
    // -----------------------------------------------------------------------

    /// A fabricated value layout (`p67_layout_object`'s four slots).
    fn value_layout(
        ctx: &mut dyn NativeContext,
        class: &str,
        size: i64,
        align: i64,
        little: bool,
    ) -> ObjectRef {
        let layout = alloc_obj(ctx, class, 4);
        ctx.set_field(layout, 0, Value::Long(size));
        ctx.set_field(layout, 1, Value::Long(align));
        ctx.set_field(layout, 2, Value::Int(i32::from(little)));
        ctx.set_field(layout, 3, Value::Object(None));
        layout
    }

    const NATIVE_LITTLE: bool = cfg!(target_endian = "little");

    fn int4(ctx: &mut dyn NativeContext) -> ObjectRef {
        value_layout(ctx, "cratonvm/test/UpcallOfIntLayout", 4, 4, NATIVE_LITTLE)
    }

    fn long8(ctx: &mut dyn NativeContext) -> ObjectRef {
        value_layout(ctx, "cratonvm/test/UpcallOfLongLayout", 8, 8, NATIVE_LITTLE)
    }

    fn padding(ctx: &mut dyn NativeContext, size: i64) -> ObjectRef {
        let layout = alloc_obj(ctx, "cratonvm/test/UpcallPaddingLayout", 4);
        ctx.set_field(layout, 0, Value::Long(size));
        ctx.set_field(layout, 1, Value::Long(1));
        ctx.set_field(layout, 2, Value::Object(None));
        ctx.set_field(layout, 3, Value::Object(None));
        layout
    }

    /// A fabricated group carrier: `[0]` size, `[1]` alignment, `[2]` the
    /// member array, `[3]` no name. `class` decides struct or union.
    fn group(
        ctx: &mut dyn NativeContext,
        class: &str,
        members: &[ObjectRef],
        size: i64,
        align: i64,
    ) -> ObjectRef {
        let object_class = crate::lang_class::reflection_component_id(ctx, "java/lang/Object");
        let array = ctx.new_ref_array(object_class, members.len());
        for (i, member) in members.iter().enumerate() {
            ctx.set_array_element(array, i, Value::Object(Some(*member)));
        }
        let layout = alloc_obj(ctx, class, 4);
        ctx.set_field(layout, 0, Value::Long(size));
        ctx.set_field(layout, 1, Value::Long(align));
        ctx.set_field(layout, 2, Value::Object(Some(array)));
        ctx.set_field(layout, 3, Value::Object(None));
        layout
    }

    fn struct_of(
        ctx: &mut dyn NativeContext,
        members: &[ObjectRef],
        size: i64,
        align: i64,
    ) -> ObjectRef {
        group(ctx, "cratonvm/test/UpcallStructLayout", members, size, align)
    }

    /// `FunctionDescriptor.of(ret)` (no parameters).
    fn returning(ctx: &mut dyn NativeContext, ret: ObjectRef) -> ObjectRef {
        let descriptor = alloc_obj(ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(ret)));
        ctx.set_field(descriptor, 1, Value::Object(None));
        descriptor
    }

    fn refusal(result: Result<(), MethodCallFailed>) -> Option<String> {
        match result {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { message },
            ))) => Some(message),
            _ => None,
        }
    }

    /// JDK 25 `AbstractLinker.checkLayoutRecursive`: the trailing padding C
    /// adds must be in the layout, members sit at their aligned offsets with
    /// no superfluous padding, a group is naturally aligned, nested value
    /// layouts are canonical, a union's padding is needed and single.
    #[test]
    fn group_layouts_are_checked_like_check_layout_recursive() {
        fn check(ctx: &mut dyn NativeContext, layout: ObjectRef) -> Option<String> {
            let descriptor = returning(ctx, layout);
            refusal(check_linker_layouts(&*ctx, descriptor))
        }
        let mut ctx = mock_ctx();

        // [i4i4]: fine.
        let (a, b) = (int4(&mut ctx), int4(&mut ctx));
        let ii = struct_of(&mut ctx, &[a, b], 8, 4);
        assert_eq!(check(&mut ctx, ii), None);

        // [j8i4] is 12 bytes; C's struct is 16.
        let (l, i) = (long8(&mut ctx), int4(&mut ctx));
        let li = struct_of(&mut ctx, &[l, i], 12, 8);
        assert_eq!(
            check(&mut ctx, li).as_deref(),
            Some("Layout '[j8i4]' has unexpected size: 12 != 16")
        );
        // ... with its padding it links.
        let (l, i, x) = (long8(&mut ctx), int4(&mut ctx), padding(&mut ctx, 4));
        let lix = struct_of(&mut ctx, &[l, i, x], 16, 8);
        assert_eq!(check(&mut ctx, lix), None);

        // Superfluous padding before a member: i4 found at 8, expected 4.
        let (a, x, b) = (int4(&mut ctx), padding(&mut ctx, 4), int4(&mut ctx));
        let ixi = struct_of(&mut ctx, &[a, x, b], 12, 4);
        assert_eq!(
            check(&mut ctx, ixi).as_deref(),
            Some("Member layout 'i4', of '[i4x4i4]' found at unexpected offset: 8 != 4")
        );

        // Over-aligned: 8%[i4i4].
        let (a, b) = (int4(&mut ctx), int4(&mut ctx));
        let over = struct_of(&mut ctx, &[a, b], 8, 8);
        assert_eq!(
            check(&mut ctx, over).as_deref(),
            Some("Layout alignment must be natural alignment: 8%[i4i4]")
        );

        // A nested value layout in the other byte order.
        let (a, b) = (
            int4(&mut ctx),
            value_layout(&mut ctx, "cratonvm/test/UpcallOfIntLayout", 4, 4, !NATIVE_LITTLE),
        );
        let foreign = struct_of(&mut ctx, &[a, b], 8, 4);
        let want = if NATIVE_LITTLE {
            "Unsupported layout: I4"
        } else {
            "Unsupported layout: i4"
        };
        assert_eq!(check(&mut ctx, foreign).as_deref(), Some(want));

        // Only padding.
        let x = padding(&mut ctx, 4);
        let only = struct_of(&mut ctx, &[x], 4, 1);
        assert_eq!(
            check(&mut ctx, only).as_deref(),
            Some("Layout '[x4]' is non-empty and only has padding layouts")
        );

        // A union whose padding is not wider than its widest member.
        let (a, x) = (int4(&mut ctx), padding(&mut ctx, 4));
        let union = group(&mut ctx, "cratonvm/test/UpcallUnionLayout", &[a, x], 4, 4);
        assert_eq!(
            check(&mut ctx, union).as_deref(),
            Some("Superfluous padding x4 in [i4|x4]")
        );
        // [i4|j8] is fine.
        let (a, l) = (int4(&mut ctx), long8(&mut ctx));
        let union = group(&mut ctx, "cratonvm/test/UpcallUnionLayout", &[a, l], 8, 8);
        assert_eq!(check(&mut ctx, union), None);

        // The switch off: groups are not walked (wave 9's behaviour).
        let (l, i) = (long8(&mut ctx), int4(&mut ctx));
        let li = struct_of(&mut ctx, &[l, i], 12, 8);
        let descriptor = returning(&mut ctx, li);
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_FFM_CHECK_LAYOUTS_GROUPS", Some("0"))],
            || check_linker_layouts(&ctx, descriptor).is_ok(),
        );
        assert!(off);
    }

    /// Round 14 wave 4 (lane ffm4): a group whose member list is
    /// native-collections' `List.copyOf` answer (a
    /// `cratonvm/internal/UnmodifiableList` wrapper, backing in slot 0) -- the
    /// shape `--compatible` gives `structLayout(I, I).withByteAlignment(16)`,
    /// built by the JDK's own `dup` -- is still walked, so the over-aligned
    /// struct is refused rather than linked (`R13Ffm6UpcallStruct` L3).
    #[test]
    fn a_group_whose_members_are_an_unmodifiable_list_wrapper_is_checked() {
        let mut ctx = mock_ctx();
        let (a, b) = (int4(&mut ctx), int4(&mut ctx));
        let over = struct_of(&mut ctx, &[a, b], 8, 16);
        let Value::Object(Some(array)) = ctx.get_field(over, 2) else {
            panic!("struct_of stores the member array in slot 2");
        };
        let wrapper = alloc_obj(&mut ctx, "cratonvm/internal/UnmodifiableList", 2);
        ctx.set_field(wrapper, 0, Value::Object(Some(array)));
        ctx.set_field(wrapper, 1, Value::Int(1));
        ctx.set_field(over, 2, Value::Object(Some(wrapper)));
        assert_eq!(
            foreign_ffm::p67_group_members(&ctx, over).map(|(_, n)| n),
            Some(2)
        );
        let descriptor = returning(&mut ctx, over);
        assert_eq!(
            refusal(check_linker_layouts(&ctx, descriptor)).as_deref(),
            Some("Layout alignment must be natural alignment: 16%[i4i4]")
        );
        // The switch off: the wrapper is not decoded, so the group reads as
        // member-less and passes (the pre-fix behaviour).
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_FFM_GROUP_MEMBERS_UNWRAP", Some("0"))],
            || {
                (
                    foreign_ffm::p67_group_members(&ctx, over).is_none(),
                    check_linker_layouts(&ctx, descriptor).is_ok(),
                )
            },
        );
        assert_eq!(off, (true, true));
    }

    /// A struct or union parameter or return is a `Group` of the layout's
    /// size and alignment, spelled `MemorySegment` in the method type, with a
    /// libffi type per carrier.
    #[test]
    fn a_struct_layout_becomes_a_group_carrier() {
        let mut ctx = mock_ctx();
        let (l, i, x) = (long8(&mut ctx), int4(&mut ctx), padding(&mut ctx, 4));
        let lix = struct_of(&mut ctx, &[l, i, x], 16, 8);
        let int_param = int4(&mut ctx);
        let object_class = crate::lang_class::reflection_component_id(&mut ctx, "java/lang/Object");
        let params = ctx.new_ref_array(object_class, 2);
        ctx.set_array_element(params, 0, Value::Object(Some(lix)));
        ctx.set_array_element(params, 1, Value::Object(Some(int_param)));
        let descriptor = alloc_obj(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(lix)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params)));
        let (shape, types) = upcall_shape_and_types(&ctx, descriptor).expect("a group shape");
        let group = UpcallKind::Group { size: 16, align: 8 };
        assert_eq!(shape.params[0].kind, group);
        assert_eq!(shape.params[1].kind, UpcallKind::Int);
        assert_eq!(shape.ret, Some(group));
        assert_eq!(
            shape.method_descriptor(),
            "(Ljava/lang/foreign/MemorySegment;I)Ljava/lang/foreign/MemorySegment;"
        );
        assert_eq!(types.params.len(), 2);
        // The switch off: refused at creation, as before.
        let refused = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_FFM_UPCALL_BY_VALUE_GROUPS", Some("0"))],
            || upcall_shape_and_types(&ctx, descriptor).is_err(),
        );
        assert!(refused);
    }

    /// A target that returns a native segment over `size` bytes of the
    /// pattern `k, k+1, ..` (`k` its one `int` argument), `size` read off the
    /// shape's group return. The image is leaked: a test's worth of bytes.
    fn returns_pattern(
        ctx: &mut dyn NativeContext,
        _target: ObjectRef,
        args: &[Value],
        shape: &UpcallShape,
    ) -> MethodCallResult {
        let k = match args {
            [Value::Int(k)] => *k as u8,
            _ => 0,
        };
        let size = match shape.ret {
            Some(UpcallKind::Group { size, .. }) => size as usize,
            _ => 0,
        };
        let image: &'static mut [u8] = Box::leak(
            (0..size)
                .map(|i| k.wrapping_add(i as u8))
                .collect::<Vec<u8>>()
                .into_boxed_slice(),
        );
        let segment = crate::panama::alloc_segment_carrier(ctx, 6)?;
        ctx.set_field(segment, 0, Value::Long(image.as_mut_ptr() as i64));
        ctx.set_field(segment, 1, Value::Long(size as i64));
        ctx.set_field(segment, 2, Value::Object(None));
        ctx.set_field(segment, 3, Value::Int(0));
        ctx.set_field(segment, 4, Value::Int(1));
        ctx.set_field(segment, 5, Value::Long(0));
        Ok(Some(Value::Object(Some(segment))))
    }

    /// The bytes of a `#[repr(C)]` value.
    fn bytes_of<T>(value: &T) -> Vec<u8> {
        // SAFETY: `value` is a live `T` of `size_of::<T>()` initialised bytes
        // (the structs below have no padding).
        unsafe {
            std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
        }
        .to_vec()
    }

    /// Round 13 wave 11 (lane ffm6): a stub returning a struct by value hands
    /// C exactly the struct's bytes, in registers or through the hidden
    /// return pointer: 16 bytes (two integer registers on SysV, hidden
    /// pointer on Win64), 12 (hidden pointer on Win64, never an 8-byte
    /// `ffi_arg` write past it) and 3 (hidden pointer on Win64).
    #[test]
    fn a_stub_returning_a_struct_hands_c_its_bytes() {
        #[repr(C)]
        #[derive(Clone, Copy)]
        #[allow(dead_code)] // read as bytes
        struct S16 {
            a: i64,
            b: i64,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        #[allow(dead_code)] // read as bytes
        struct S12 {
            a: i32,
            b: i32,
            c: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        #[allow(dead_code)] // read as bytes
        struct S3 {
            a: u8,
            b: u8,
            c: u8,
        }
        fn pattern(k: u8, size: usize) -> Vec<u8> {
            (0..size).map(|i| k.wrapping_add(i as u8)).collect()
        }
        fn stub(
            ctx: &dyn NativeContext,
            target: ObjectRef,
            ret: Type,
            size: u32,
            align: u32,
        ) -> usize {
            let shape = UpcallShape {
                params: vec![param(UpcallKind::Int)],
                ret: Some(UpcallKind::Group { size, align }),
            };
            let types = UpcallFfiTypes {
                params: vec![Type::i32()],
                ret,
            };
            install_upcall_stub_typed(ctx, target, shape, types, None, returns_pattern)
        }
        let mut ctx = mock_ctx();
        let target = fake_target(&mut ctx);
        let c16 = stub(&ctx, target, Type::structure(vec![Type::i64(), Type::i64()]), 16, 8);
        let c12 = stub(
            &ctx,
            target,
            Type::structure(vec![Type::i32(), Type::i32(), Type::i32()]),
            12,
            4,
        );
        let c3 = stub(
            &ctx,
            target,
            Type::structure(vec![Type::u8(), Type::u8(), Type::u8()]),
            3,
            1,
        );
        let (g16, g12, g3) = {
            let _guard = plf::ActiveContextGuard::install(&mut ctx);
            // SAFETY: each stub's CIF is `S(int)` for its `S`.
            let f16: extern "C" fn(i32) -> S16 = unsafe { std::mem::transmute(c16) };
            // SAFETY: as above.
            let f12: extern "C" fn(i32) -> S12 = unsafe { std::mem::transmute(c12) };
            // SAFETY: as above.
            let f3: extern "C" fn(i32) -> S3 = unsafe { std::mem::transmute(c3) };
            (f16(10), f12(40), f3(90))
        };
        assert_eq!(bytes_of(&g16), pattern(10, 16));
        assert_eq!(bytes_of(&g12), pattern(40, 12));
        assert_eq!(bytes_of(&g3), pattern(90, 3));
        for code in [c16, c12, c3] {
            remove_upcall_stub(code);
        }
    }

    /// The copy out of the returned segment refuses what HotSpot's copy
    /// refuses: `null`, and a segment shorter than the layout.
    #[test]
    fn a_group_return_refuses_null_and_short_segments() {
        let mut ctx = mock_ctx();
        let mut buffer = [0u8; 8];
        let dest = Some(GroupReturn {
            ptr: buffer.as_mut_ptr(),
            len: buffer.len(),
        });
        assert!(matches!(
            copy_group_return(&mut ctx, Value::Object(None), 8, dest),
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::NullPointerException { .. }
            )))
        ));
        let image: [u8; 4] = [1, 2, 3, 4];
        let segment = crate::panama::alloc_segment_carrier(&mut ctx, 6).unwrap();
        ctx.set_field(segment, 0, Value::Long(image.as_ptr() as i64));
        ctx.set_field(segment, 1, Value::Long(4));
        ctx.set_field(segment, 2, Value::Object(None));
        ctx.set_field(segment, 3, Value::Int(0));
        ctx.set_field(segment, 4, Value::Int(1));
        ctx.set_field(segment, 5, Value::Long(0));
        assert!(matches!(
            copy_group_return(&mut ctx, Value::Object(Some(segment)), 8, dest),
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IndexOutOfBoundsException { .. }
            )))
        ));
        assert!(copy_group_return(&mut ctx, Value::Object(Some(segment)), 4, dest).is_ok());
        assert_eq!(buffer, [1, 2, 3, 4, 0, 0, 0, 0]);
    }

    /// Round 13 wave 13 (lane ffm7): `Utils.isAligned`, the predicate the
    /// upcall's address boxing applies.
    #[test]
    fn misalignment_is_judged_against_the_target_alignment() {
        assert!(misaligned_address(6, 4));
        assert!(misaligned_address(0x1001, 8));
        assert!(!misaligned_address(8, 4));
        assert!(!misaligned_address(0, 8), "NULL is aligned for everything");
        assert!(!misaligned_address(3, 1), "no target layout: alignment 1");
    }

    /// Round 13 wave 13 (lane ffm7; `r12w7-upcall-residuals` item 5, bullet
    /// 4): an address C passes that its target layout's alignment does not
    /// divide is `Utils.longToAddress`'s `IllegalArgumentException` inside the
    /// upcall -- the uncaught-exception exit -- and the target never runs; an
    /// aligned one runs.
    #[test]
    fn a_misaligned_address_argument_is_refused_like_long_to_address() {
        let mut ctx = mock_ctx();
        let target = fake_target(&mut ctx);
        let address = UpcallParam {
            kind: UpcallKind::Address,
            target_size: 4,
            target_align: 4,
        };
        let shape = UpcallShape {
            params: vec![address, address],
            ret: Some(UpcallKind::Int),
        };
        let code = install_upcall_stub(&ctx, target, shape, None, compare_ints);
        let data = registry()
            .lock()
            .get(&code)
            .map(|e| e.data as *const UpcallStubData)
            .expect("registered");
        let ints: [i32; 3] = [5, 7, 9];
        let base = ints.as_ptr() as usize;
        let aligned: [usize; 2] = [base, base + 4];
        let misaligned: [usize; 2] = [base, base + 2];
        let aligned_args = [
            (&aligned[0] as *const usize).cast::<c_void>(),
            (&aligned[1] as *const usize).cast::<c_void>(),
        ];
        let misaligned_args = [
            (&misaligned[0] as *const usize).cast::<c_void>(),
            (&misaligned[1] as *const usize).cast::<c_void>(),
        ];
        let pins_before = ctx.native_pin_count_for_test();
        // SAFETY: registered; each slot points at a pointer-sized address, the
        // C type of an address parameter, into the live `ints`.
        let ran = unsafe { upcall_dispatch(&mut ctx, &*data, aligned_args.as_ptr()) };
        assert_eq!(ran.unwrap() as i32, -1, "5 < 7");
        // SAFETY: as above; the second address is refused before any read.
        let refused = unsafe { upcall_dispatch(&mut ctx, &*data, misaligned_args.as_ptr()) };
        let text = format!("{refused:?}");
        assert!(
            matches!(refused, Err(UpcallFatal::Uncaught(_)))
                && text.contains("Invalid alignment constraint for address: 0x"),
            "{text}"
        );
        assert_eq!(ctx.native_pin_count_for_test(), pins_before);
        remove_upcall_stub(code);
    }

    thread_local! {
        /// The group argument [`sums_struct_arg`] last received.
        static KEPT_ARG: std::cell::Cell<Option<ObjectRef>> = const { std::cell::Cell::new(None) };
    }

    /// `(struct { long a; long b; }) -> int`: `a * 10 + b`, read through the
    /// segment the stub made, which must be open and owned by this thread and
    /// 16 bytes long (negative answers name the failed check). Keeps the
    /// segment in [`KEPT_ARG`].
    fn sums_struct_arg(
        ctx: &mut dyn NativeContext,
        _target: ObjectRef,
        args: &[Value],
        _shape: &UpcallShape,
    ) -> MethodCallResult {
        let [Value::Object(Some(segment))] = args else {
            return Ok(Some(Value::Int(-1)));
        };
        let segment = *segment;
        if crate::panama::pe_segment_check_scope(ctx, segment).is_err() {
            return Ok(Some(Value::Int(-2)));
        }
        if plf::segment_byte_size(ctx, segment) != 16 {
            return Ok(Some(Value::Int(-3)));
        }
        let address = plf::segment_address(ctx, segment);
        if address == 0 {
            return Ok(Some(Value::Int(-4)));
        }
        // SAFETY: a live 16-byte block of the per-upcall arena (checked above).
        let (a, b) = unsafe {
            (
                std::ptr::read_unaligned(address as usize as *const i64),
                std::ptr::read_unaligned((address as usize + 8) as *const i64),
            )
        };
        KEPT_ARG.with(|k| k.set(Some(segment)));
        Ok(Some(Value::Int((a * 10 + b) as i32)))
    }

    /// Round 13 wave 13 (lane ffm7; testable since FFM6-1 gave the mock a
    /// stable thread): a by-value struct PARAMETER reaches the target as a
    /// segment of a confined arena made for the one upcall -- open and this
    /// thread's during the call, `Already closed` after it (HotSpot's
    /// `newBoundedArena` frame) -- and the upcall leaves no pin behind.
    #[test]
    fn a_struct_parameter_is_a_confined_segment_closed_after_the_upcall() {
        #[repr(C)]
        #[allow(dead_code)] // read as bytes
        struct S16 {
            a: i64,
            b: i64,
        }
        let mut ctx = mock_ctx();
        ctx.stable_current_thread();
        let target = fake_target(&mut ctx);
        let shape = UpcallShape {
            params: vec![param(UpcallKind::Group { size: 16, align: 8 })],
            ret: Some(UpcallKind::Int),
        };
        let code = install_upcall_stub(&ctx, target, shape, None, sums_struct_arg);
        let data = registry()
            .lock()
            .get(&code)
            .map(|e| e.data as *const UpcallStubData)
            .expect("registered");
        let s = S16 { a: 3, b: 4 };
        let args = [(&s as *const S16).cast::<c_void>()];
        KEPT_ARG.with(|k| k.set(None));
        let pins_before = ctx.native_pin_count_for_test();
        // SAFETY: registered; the one slot points at the aggregate's 16 bytes,
        // which is how libffi hands a closure a by-value struct.
        let got = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
        assert_eq!(got.unwrap() as i32, 34);
        assert_eq!(ctx.native_pin_count_for_test(), pins_before);
        let kept = KEPT_ARG.with(|k| k.take()).expect("the target ran");
        let after = crate::panama::pe_segment_check_scope(&mut ctx, kept);
        assert!(format!("{after:?}").contains("Already closed"), "{after:?}");
        remove_upcall_stub(code);
    }

    #[test]
    fn the_row_is_a_stated_bridge_on_the_linker_interface() {
        let mut r = NativeMethodRegistry::new();
        register_panama_upcall(&mut r);
        let rows = r.census();
        let row = rows
            .iter()
            .find(|row| row.class == LINKER_CLASS && row.name == "upcallStub")
            .expect("Linker.upcallStub registered");
        assert_eq!(row.descriptor, UPCALL_STUB_DESCRIPTOR);
        assert_eq!(row.kind, NativeKind::Bridge);
        assert!(row.kind_chosen, "the kind must be stated, not inherited");
    }
}
