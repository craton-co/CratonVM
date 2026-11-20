// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Fast direct-build plan, 8-entry TLS cache, and direct compact field writes
//! for [`super::jit_new_throwable_direct`] and
//! [`super::try_build_implicit_throwable_directly`].

use super::{
    direct_event, note_direct_throwable, npe_extended_message_slots, throwable_own_field_index,
    FAST_THROW_SITES,
};
use crate::classloading::ClassId;
use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// What [`super::try_build_implicit_throwable_directly`] and
/// [`super::jit_new_throwable_direct`] need about one class, once its
/// constructor chain has been proven to be the JDK's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DirectBuildPlan {
    /// `java/lang/Throwable`, for its statics.
    pub(super) throwable_class: ClassId,
    /// Static-field index of `Throwable.UNASSIGNED_STACK`.
    pub(super) unassigned_stack_static: usize,
    /// Static-field index of `Throwable.SUPPRESSED_SENTINEL`.
    pub(super) suppressed_sentinel_static: usize,
    /// Static-field index of `Throwable.jfrTracing`.
    pub(super) jfr_tracing_static: usize,
    /// The chain contains `NullPointerException.fillInStackTrace()`, which the
    /// constructor's virtual call runs: `extendedMessageState` 0 -> 1.
    pub(super) npe_fill_override: bool,
}

/// One byte of an opcode shape: `Some(b)` must match, `None` is a
/// constant-pool index byte (any value).
pub(super) type ShapeByte = Option<u8>;
const CP: ShapeByte = None;
const fn op(b: u8) -> ShapeByte {
    Some(b)
}

/// `X(String s) { super(s); }`: `aload_0 aload_1 invokespecial #s return`.
pub(super) const DELEGATE_INIT_STRING: &[ShapeByte] =
    &[op(0x2a), op(0x2b), op(0xb7), CP, CP, op(0xb1)];
/// `X() { super(); }`: `aload_0 invokespecial #s return`.
pub(super) const DELEGATE_INIT_EMPTY: &[ShapeByte] = &[op(0x2a), op(0xb7), CP, CP, op(0xb1)];

/// JDK 25 `Throwable(String message)` (48 bytes): `Object.<init>`, the three
/// field initialisers, `fillInStackTrace(); detailMessage = message;` and the
/// `jfrTracing` tail (`ifeq +11`).
#[rustfmt::skip]
pub(super) const THROWABLE_INIT_STRING: &[ShapeByte] = &[
    op(0x2a), op(0xb7), CP, CP, // aload_0; invokespecial Object.<init>
    op(0x2a), op(0x2a), op(0xb5), CP, CP, // cause = this
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP, // stackTrace = UNASSIGNED_STACK
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP, // suppressedExceptions = SUPPRESSED_SENTINEL
    op(0x2a), op(0xb6), CP, CP, op(0x57), // fillInStackTrace(); pop
    op(0x2a), op(0x2b), op(0xb5), CP, CP, // detailMessage = message
    op(0xb2), CP, CP, op(0x99), op(0x00), op(0x0b), // if (jfrTracing)
    op(0x2a), op(0xb6), CP, CP, op(0x2b), op(0xb8), CP, CP, // traceThrowable(getClass(), message)
    op(0xb1),
];

/// JDK 25 `Throwable()` (43 bytes): as above without `detailMessage`, and
/// `aconst_null` for the traced message.
#[rustfmt::skip]
pub(super) const THROWABLE_INIT_EMPTY: &[ShapeByte] = &[
    op(0x2a), op(0xb7), CP, CP,
    op(0x2a), op(0x2a), op(0xb5), CP, CP,
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP,
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP,
    op(0x2a), op(0xb6), CP, CP, op(0x57),
    op(0xb2), CP, CP, op(0x99), op(0x00), op(0x0b),
    op(0x2a), op(0xb6), CP, CP, op(0x01), op(0xb8), CP, CP,
    op(0xb1),
];

/// JDK 25 `synchronized Throwable fillInStackTrace()` (29 bytes): `if
/// (stackTrace != null || backtrace != null) { fillInStackTrace(0);
/// stackTrace = UNASSIGNED_STACK; } return this;`.
#[rustfmt::skip]
pub(super) const THROWABLE_FILL_IN_STACK_TRACE: &[ShapeByte] = &[
    op(0x2a), op(0xb4), CP, CP, op(0xc7), op(0x00), op(0x0a), // stackTrace ifnonnull +10
    op(0x2a), op(0xb4), CP, CP, op(0xc6), op(0x00), op(0x10), // backtrace ifnull +16
    op(0x2a), op(0x03), op(0xb6), CP, CP, op(0x57), // fillInStackTrace(0); pop
    op(0x2a), op(0xb2), CP, CP, op(0xb5), CP, CP, // stackTrace = UNASSIGNED_STACK
    op(0x2a), op(0xb0),
];

/// JDK 25 `synchronized Throwable NullPointerException.fillInStackTrace()`
/// (41 bytes): `extendedMessageState` 0 -> 1 (1 -> 2 computing the message),
/// then `super.fillInStackTrace()`.
#[rustfmt::skip]
pub(super) const NPE_FILL_IN_STACK_TRACE: &[ShapeByte] = &[
    op(0x2a), op(0xb4), CP, CP, op(0x9a), op(0x00), op(0x0b), // state ifne +11
    op(0x2a), op(0x04), op(0xb5), CP, CP, op(0xa7), op(0x00), op(0x18), // state = 1; goto +24
    op(0x2a), op(0xb4), CP, CP, op(0x04), op(0xa0), op(0x00), op(0x10), // state if_icmpne 1, +16
    op(0x2a), op(0x2a), op(0xb6), CP, CP, op(0xb5), CP, CP, // extendedMessage = getExtendedNPEMessage()
    op(0x2a), op(0x05), op(0xb5), CP, CP, // state = 2
    op(0x2a), op(0xb7), CP, CP, op(0xb0), // return super.fillInStackTrace()
];

/// Does `code` have exactly `shape`'s opcodes and fixed operands? Pure.
pub(super) fn code_matches_shape(code: &[u8], shape: &[ShapeByte]) -> bool {
    code.len() == shape.len()
        && code
            .iter()
            .zip(shape)
            .all(|(byte, want)| want.is_none_or(|w| w == *byte))
}

/// A non-native method whose decoded `Code` has no handlers and `shape`.
pub(super) fn method_has_shape(
    method: &cratonvm_reader::method::ClassFileMethod,
    shape: &[ShapeByte],
) -> bool {
    !method.is_native()
        && method
            .code()
            .is_some_and(|c| c.exception_table.is_empty() && code_matches_shape(&c.code, shape))
}

/// One constant-pool operand of a shape the direct build relies on: the
/// operand's byte offset in the code, and the member it must name -- owner
/// (`None`: any), name, descriptor.
pub(super) type CpRef = (usize, Option<&'static str>, &'static str, &'static str);

pub(super) const THROWABLE: Option<&str> = Some("java/lang/Throwable");
const STACK_ARRAY: &str = "[Ljava/lang/StackTraceElement;";

/// The members `Throwable(String)` names, at its operand offsets.
#[rustfmt::skip]
pub(super) const THROWABLE_INIT_STRING_REFS: &[CpRef] = &[
    (2, Some("java/lang/Object"), "<init>", "()V"),
    (7, THROWABLE, "cause", "Ljava/lang/Throwable;"),
    (11, THROWABLE, "UNASSIGNED_STACK", STACK_ARRAY),
    (14, THROWABLE, "stackTrace", STACK_ARRAY),
    (18, THROWABLE, "SUPPRESSED_SENTINEL", "Ljava/util/List;"),
    (21, THROWABLE, "suppressedExceptions", "Ljava/util/List;"),
    (25, THROWABLE, "fillInStackTrace", "()Ljava/lang/Throwable;"),
    (31, THROWABLE, "detailMessage", "Ljava/lang/String;"),
    (34, THROWABLE, "jfrTracing", "Z"),
];

/// The members `Throwable()` names.
#[rustfmt::skip]
pub(super) const THROWABLE_INIT_EMPTY_REFS: &[CpRef] = &[
    (2, Some("java/lang/Object"), "<init>", "()V"),
    (7, THROWABLE, "cause", "Ljava/lang/Throwable;"),
    (11, THROWABLE, "UNASSIGNED_STACK", STACK_ARRAY),
    (14, THROWABLE, "stackTrace", STACK_ARRAY),
    (18, THROWABLE, "SUPPRESSED_SENTINEL", "Ljava/util/List;"),
    (21, THROWABLE, "suppressedExceptions", "Ljava/util/List;"),
    (25, THROWABLE, "fillInStackTrace", "()Ljava/lang/Throwable;"),
    (29, THROWABLE, "jfrTracing", "Z"),
];

/// The members `Throwable.fillInStackTrace()` names.
#[rustfmt::skip]
pub(super) const THROWABLE_FILL_REFS: &[CpRef] = &[
    (2, THROWABLE, "stackTrace", STACK_ARRAY),
    (9, THROWABLE, "backtrace", "Ljava/lang/Object;"),
    (17, THROWABLE, "fillInStackTrace", "(I)Ljava/lang/Throwable;"),
    (22, THROWABLE, "UNASSIGNED_STACK", STACK_ARRAY),
    (25, THROWABLE, "stackTrace", STACK_ARRAY),
];

/// The members `NullPointerException.fillInStackTrace()` names.
#[rustfmt::skip]
pub(super) const NPE_FILL_REFS: &[CpRef] = &[
    (2, Some("java/lang/NullPointerException"), "extendedMessageState", "I"),
    (10, Some("java/lang/NullPointerException"), "extendedMessageState", "I"),
    (17, Some("java/lang/NullPointerException"), "extendedMessageState", "I"),
    (26, Some("java/lang/NullPointerException"), "getExtendedNPEMessage", "()Ljava/lang/String;"),
    (29, Some("java/lang/NullPointerException"), "extendedMessage", "Ljava/lang/String;"),
    (34, Some("java/lang/NullPointerException"), "extendedMessageState", "I"),
    (38, None, "fillInStackTrace", "()Ljava/lang/Throwable;"),
];

/// The `CONSTANT_Fieldref` / `CONSTANT_Methodref` the two operand bytes at
/// `code[pos..pos + 2]` index: `(owner, name, descriptor)`.
pub(super) fn cp_member_at<'a>(
    cp: &'a cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    pos: usize,
) -> Option<(&'a str, &'a str, &'a str)> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let index = u16::from_be_bytes([*code.get(pos)?, *code.get(pos.checked_add(1)?)?]);
    let (class_index, nat_index) = match cp.get(index)? {
        ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner = cp.get_class_name(class_index)?;
    let (name, descriptor) = cp.get_name_and_type(nat_index)?;
    Some((owner, name, descriptor))
}

/// Does every operand `refs` lists name its member? Pure over the pool.
pub(super) fn code_refs_match(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    refs: &[CpRef],
) -> bool {
    refs.iter().all(|&(pos, owner, name, descriptor)| {
        cp_member_at(cp, code, pos).is_some_and(|(o, n, d)| {
            owner.is_none_or(|want| want == o) && n == name && d == descriptor
        })
    })
}

/// [`method_has_shape`], and its operands name `refs` in `cp` (the declaring
/// class's pool).
pub(super) fn method_has_shape_and_refs(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    method: &cratonvm_reader::method::ClassFileMethod,
    shape: &[ShapeByte],
    refs: &[CpRef],
) -> bool {
    method_has_shape(method, shape)
        && method
            .code()
            .is_some_and(|c| code_refs_match(cp, &c.code, refs))
}

/// The static-field index (`get_static_shared`'s) of `name` in `fields`.
fn static_field_index(
    fields: &[cratonvm_reader::field::ClassFileField],
    name: &str,
) -> Option<usize> {
    fields
        .iter()
        .filter(|f| f.is_static())
        .position(|f| &*f.name == name)
}

/// Why [`direct_build_plan`] refused (JIT round 23 wave 1b, lane L3, R23W1L3-2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlanRefusal {
    /// Class flags, hierarchy or decoded bytes decide it until class redefinition.
    Structural,
    /// Not known yet (`Throwable` not loaded, undecoded `Code`, etc.).
    Transient,
}

/// A failed opcode-shape check of `methods`: [`PlanRefusal::Transient`] while
/// one of them is a non-native method with no decoded `Code`, else [`PlanRefusal::Structural`].
pub(super) fn shape_refusal(methods: &[&cratonvm_reader::method::ClassFileMethod]) -> PlanRefusal {
    if methods.iter().any(|m| !m.is_native() && m.code().is_none()) {
        PlanRefusal::Transient
    } else {
        PlanRefusal::Structural
    }
}

/// Prove `class_id`'s `<init>` chain (`(String)V` when `with_message`, else
/// `()V`) is the JDK 25 one up to `java/lang/Throwable`.
pub(super) fn direct_build_plan(
    shared: &SharedVm,
    class_id: ClassId,
    with_message: bool,
) -> Result<DirectBuildPlan, PlanRefusal> {
    use PlanRefusal::{Structural, Transient};
    const NPE: &str = "java/lang/NullPointerException";
    let descriptor = if with_message {
        "(Ljava/lang/String;)V"
    } else {
        "()V"
    };
    let (delegate, root, root_refs, delegate_operand) = if with_message {
        (
            DELEGATE_INIT_STRING,
            THROWABLE_INIT_STRING,
            THROWABLE_INIT_STRING_REFS,
            3,
        )
    } else {
        (
            DELEGATE_INIT_EMPTY,
            THROWABLE_INIT_EMPTY,
            THROWABLE_INIT_EMPTY_REFS,
            2,
        )
    };
    let cm = shared.classes.class_manager.read();
    let throwable = cm
        .loaded_class_under_exact_key(
            "java/lang/Throwable",
            cratonvm_types::ClassLoaderId::Bootstrap,
        )
        .ok_or(Transient)?;
    let mut npe_fill_override = false;
    let mut cursor = class_id;
    // Bounded: a real hierarchy from these classes is five deep.
    for _ in 0..16 {
        let cls = cm.get_class(cursor).ok_or(Transient)?;
        if cls.hidden || cls.has_finalizer {
            return Err(Structural);
        }
        if cursor == throwable {
            let init = cls.find_method("<init>", descriptor).ok_or(Structural)?;
            let fill = cls
                .find_method("fillInStackTrace", "()Ljava/lang/Throwable;")
                .ok_or(Structural)?;
            let fill_native = cls
                .find_method("fillInStackTrace", "(I)Ljava/lang/Throwable;")
                .ok_or(Structural)?;
            let cp = &cls.constant_pool;
            if !method_has_shape_and_refs(cp, init, root, root_refs)
                || !fill.is_synchronized()
                || !method_has_shape_and_refs(
                    cp,
                    fill,
                    THROWABLE_FILL_IN_STACK_TRACE,
                    THROWABLE_FILL_REFS,
                )
                || !fill_native.is_native()
            {
                return Err(shape_refusal(&[init, fill]));
            }
            return Ok(DirectBuildPlan {
                throwable_class: throwable,
                unassigned_stack_static: static_field_index(&cls.fields, "UNASSIGNED_STACK")
                    .ok_or(Structural)?,
                suppressed_sentinel_static: static_field_index(&cls.fields, "SUPPRESSED_SENTINEL")
                    .ok_or(Structural)?,
                jfr_tracing_static: static_field_index(&cls.fields, "jfrTracing")
                    .ok_or(Structural)?,
                npe_fill_override,
            });
        }
        for m in cls
            .methods
            .iter()
            .filter(|m| &*m.name == "fillInStackTrace")
        {
            let jdk_npe_override = &*cls.name == NPE
                && !npe_fill_override
                && &*m.descriptor == "()Ljava/lang/Throwable;"
                && m.is_synchronized()
                && method_has_shape_and_refs(
                    &cls.constant_pool,
                    m,
                    NPE_FILL_IN_STACK_TRACE,
                    NPE_FILL_REFS,
                );
            if !jdk_npe_override {
                return Err(if &*cls.name == NPE {
                    shape_refusal(&[m])
                } else {
                    Structural
                });
            }
            npe_fill_override = true;
        }
        let superclass = cls.superclass.ok_or(Structural)?;
        let super_name = cm.get_class(superclass).ok_or(Transient)?.name.clone();
        let delegate_ref: [CpRef; 1] = [(delegate_operand, None, "<init>", descriptor)];
        let init = cls.find_method("<init>", descriptor).ok_or(Structural)?;
        if !method_has_shape_and_refs(&cls.constant_pool, init, delegate, &delegate_ref) {
            return Err(shape_refusal(&[init]));
        }
        if !init
            .code()
            .and_then(|c| cp_member_at(&cls.constant_pool, &c.code, delegate_operand))
            .is_some_and(|(owner, _, _)| owner == &*super_name)
        {
            return Err(Structural);
        }
        cursor = superclass;
    }
    Err(Structural)
}

/// [`direct_build_plan`], cached in this thread's `FAST_THROW_SITES` per VM,
/// class and constructor, and recomputed after any class redefinition.
pub(super) fn cached_direct_build_plan(
    shared: &SharedVm,
    class_id: ClassId,
    with_message: bool,
) -> Option<DirectBuildPlan> {
    let stamp = cratonvm_classloading::class_redefinition_count();
    let key = (shared.vm_identity, class_id.as_u32(), with_message);
    let hit = FAST_THROW_SITES.with(|tls| {
        let mut tls = tls.try_borrow_mut().ok()?;
        if tls.direct_stamp != stamp {
            tls.direct.clear();
            tls.direct_refused.clear();
            tls.direct_stamp = stamp;
        }
        if tls.direct_refused.contains(&key) {
            return Some(None);
        }
        tls.direct.get(&key).copied()
    });
    if let Some(verdict) = hit {
        if verdict.is_none() {
            note_direct_throwable(shared, direct_event::PLAN_REFUSAL_MEMO_HITS);
        }
        return verdict;
    }
    let verdict = direct_build_plan(shared, class_id, with_message);
    note_direct_throwable(shared, direct_event::PLAN_WALKS);
    match verdict {
        Err(PlanRefusal::Structural) => {
            note_direct_throwable(shared, direct_event::PLAN_WALKS_REFUSED_STRUCTURAL)
        }
        Err(PlanRefusal::Transient) => {
            note_direct_throwable(shared, direct_event::PLAN_WALKS_REFUSED_TRANSIENT)
        }
        Ok(_) => {}
    }
    FAST_THROW_SITES.with(|tls| {
        if let Ok(mut tls) = tls.try_borrow_mut() {
            if tls.direct_stamp == stamp {
                match verdict {
                    Ok(plan) if tls.direct.len() < 64 => {
                        tls.direct.insert(key, Some(plan));
                    }
                    Err(PlanRefusal::Structural) => {
                        if tls.direct_refused.len() >= DIRECT_REFUSAL_MEMO_CAP {
                            tls.direct_refused.clear();
                        }
                        tls.direct_refused.insert(key);
                    }
                    _ => {}
                }
            }
        }
    });
    verdict.ok()
}

/// The bound of `ImplicitThrowTls::direct_refused`.
const DIRECT_REFUSAL_MEMO_CAP: usize = 256;

/// Could anything observe the constructors NOT running?
pub(super) fn direct_build_observable(shared: &SharedVm) -> bool {
    let active = crate::runtime::interpreter::jvmti_requires_interpreter(shared)
        || crate::runtime::interpreter::exception_events_may_be_armed(shared)
        || (crate::runtime::jvmti::any_listener_active()
            && crate::runtime::jvmti::any_listener_active_for_vm(shared.vm_identity));
    #[cfg(feature = "experimental-debug")]
    {
        active
            || shared.debug.debugger_gates.method_events_armed()
            || shared.debug.debugger_gates.session_attached()
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        active
    }
}

/// The field slots and statics a direct build stores, resolved before
/// anything is allocated or changed, so a refusal leaves no trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DirectBuildSlots {
    pub(super) plan: DirectBuildPlan,
    pub(super) cause: usize,
    pub(super) stack_trace: usize,
    pub(super) suppressed: usize,
    pub(super) detail_message: usize,
    pub(super) backtrace: usize,
    pub(super) depth: usize,
    /// NullPointerException's `extendedMessageState`, when its override runs.
    pub(super) npe_state: Option<usize>,
}

/// One resolved compact field byte offset + storage kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CompactSlot {
    pub(super) offset: u32,
    pub(super) storage: cratonvm_types::FieldStorageKind,
}

/// Resolved compact byte offsets for all fields written by a direct Throwable build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DirectCompactLayout {
    pub(super) num_fields: u32,
    pub(super) total_size: u32,
    pub(super) cause: CompactSlot,
    pub(super) stack_trace: CompactSlot,
    pub(super) suppressed: CompactSlot,
    pub(super) detail_message: CompactSlot,
    pub(super) backtrace: CompactSlot,
    pub(super) depth: CompactSlot,
    pub(super) npe_state: Option<CompactSlot>,
}

/// Copy-only payload returned on an 8-entry cache hit. Never holds raw `ObjectRef`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ResolvedDirectThrow {
    pub(super) slots: DirectBuildSlots,
    pub(super) compact: Option<DirectCompactLayout>,
}

const DIRECT_THROW_CACHE_CAP: usize = 8;

#[derive(Clone)]
pub(super) struct DirectThrowCacheEntry {
    pub(super) vm_identity: usize,
    pub(super) class_id: u32,
    pub(super) with_message: bool,
    pub(super) redef_stamp: u64,
    pub(super) layout_domain: u32,
    pub(super) layout_gen: u64,
    pub(super) layout_epoch: u32,
    pub(super) class_name: Arc<str>,
    pub(super) resolved: ResolvedDirectThrow,
}

/// 8-entry thread-local cache stored inside `ImplicitThrowTls` (`FAST_THROW_SITES`),
/// avoiding any new `static` / `thread_local!` declaration.
#[derive(Default)]
pub(super) struct DirectThrowCache {
    entries: [Option<DirectThrowCacheEntry>; DIRECT_THROW_CACHE_CAP],
    next_evict: usize,
}

impl DirectThrowCache {
    #[inline]
    pub(super) fn lookup(
        &self,
        vm_identity: usize,
        class_id: u32,
        with_message: bool,
        class_name: &str,
        redef_stamp: u64,
        layout_domain: u32,
        layout_gen: u64,
        layout_epoch: u32,
    ) -> Option<ResolvedDirectThrow> {
        for slot in &self.entries {
            let Some(entry) = slot else {
                continue;
            };
            if entry.vm_identity == vm_identity
                && entry.class_id == class_id
                && entry.with_message == with_message
                && entry.redef_stamp == redef_stamp
                && entry.layout_domain == layout_domain
                && entry.layout_gen == layout_gen
                && entry.layout_epoch == layout_epoch
                && &*entry.class_name == class_name
            {
                return Some(entry.resolved);
            }
        }
        None
    }

    pub(super) fn insert(&mut self, entry: DirectThrowCacheEntry) {
        if let Some(slot) = self.entries.iter_mut().find(|slot| {
            slot.as_ref().is_none_or(|existing| {
                existing.redef_stamp != entry.redef_stamp
                    || (existing.vm_identity == entry.vm_identity
                        && existing.class_id == entry.class_id
                        && existing.with_message == entry.with_message)
            })
        }) {
            *slot = Some(entry);
            return;
        }
        let idx = self.next_evict % DIRECT_THROW_CACHE_CAP;
        self.entries[idx] = Some(entry);
        self.next_evict = (idx + 1) % DIRECT_THROW_CACHE_CAP;
    }
}

/// Resolve field slot indices for `class_id` once `plan` is known.
pub(super) fn resolve_direct_build_slots_for_plan(
    shared: &SharedVm,
    class_id: ClassId,
    plan: DirectBuildPlan,
) -> Option<DirectBuildSlots> {
    Some(DirectBuildSlots {
        plan,
        cause: throwable_own_field_index(shared, "cause")?,
        stack_trace: throwable_own_field_index(shared, "stackTrace")?,
        suppressed: throwable_own_field_index(shared, "suppressedExceptions")?,
        detail_message: throwable_own_field_index(shared, "detailMessage")?,
        backtrace: throwable_own_field_index(shared, "backtrace")?,
        depth: throwable_own_field_index(shared, "depth")?,
        npe_state: if plan.npe_fill_override {
            Some(npe_extended_message_slots(shared, class_id)?.1)
        } else {
            None
        },
    })
}

/// Resolve compact byte offsets for `slots` in a single `with_class_layout_versioned` lookup.
pub(super) fn resolve_direct_compact_layout(
    domain: u32,
    class_id: ClassId,
    num_fields: u32,
    slots: &DirectBuildSlots,
) -> Option<DirectCompactLayout> {
    cratonvm_types::compact_object_total_size(domain, class_id.as_u32(), num_fields as usize)?;
    cratonvm_types::with_class_layout_versioned(class_id.as_u32(), num_fields, |layout| {
        // Keep small contiguous backing invariant (G1 non-humongous bound).
        if layout.total_size > 1024 {
            return None;
        }
        let ref_slot = |idx: usize| -> Option<CompactSlot> {
            let disp = layout.field_disp(idx)?;
            let storage = layout.field_storage(idx)?;
            if !storage.is_reference() {
                return None;
            }
            Some(CompactSlot {
                offset: disp,
                storage,
            })
        };
        let int_slot = |idx: usize| -> Option<CompactSlot> {
            let disp = layout.field_disp(idx)?;
            let storage = layout.field_storage(idx)?;
            if !matches!(storage, cratonvm_types::FieldStorageKind::Int) {
                return None;
            }
            Some(CompactSlot {
                offset: disp,
                storage,
            })
        };
        let npe_state = match slots.npe_state {
            Some(idx) => Some(int_slot(idx)?),
            None => None,
        };
        Some(DirectCompactLayout {
            num_fields,
            total_size: layout.total_size,
            cause: ref_slot(slots.cause)?,
            stack_trace: ref_slot(slots.stack_trace)?,
            suppressed: ref_slot(slots.suppressed)?,
            detail_message: ref_slot(slots.detail_message)?,
            backtrace: ref_slot(slots.backtrace)?,
            depth: int_slot(slots.depth)?,
            npe_state,
        })
    })
    .flatten()
}

/// Look up `(slots, compact)` in the 8-entry thread-local cache.
#[inline]
pub(super) fn lookup_direct_throw_cache(
    shared: &SharedVm,
    class_id: ClassId,
    with_message: bool,
    class_name: &str,
) -> Option<ResolvedDirectThrow> {
    let redef_stamp = cratonvm_classloading::class_redefinition_count();
    let layout_gen = cratonvm_types::layout_generation();
    let layout_epoch = cratonvm_types::layout_replace_epoch();
    FAST_THROW_SITES.with(|tls| {
        let tls = tls.try_borrow().ok()?;
        tls.direct_throw_cache.lookup(
            shared.vm_identity,
            class_id.as_u32(),
            with_message,
            class_name,
            redef_stamp,
            shared.jit.class_layout_domain,
            layout_gen,
            layout_epoch,
        )
    })
}

/// Store `(slots, compact)` in the 8-entry thread-local cache.
pub(super) fn store_direct_throw_cache(
    shared: &SharedVm,
    class_id: ClassId,
    with_message: bool,
    class_name: Arc<str>,
    resolved: ResolvedDirectThrow,
) {
    let redef_stamp = cratonvm_classloading::class_redefinition_count();
    let layout_gen = cratonvm_types::layout_generation();
    let layout_epoch = cratonvm_types::layout_replace_epoch();
    FAST_THROW_SITES.with(|tls| {
        if let Ok(mut tls) = tls.try_borrow_mut() {
            tls.direct_throw_cache.insert(DirectThrowCacheEntry {
                vm_identity: shared.vm_identity,
                class_id: class_id.as_u32(),
                with_message,
                redef_stamp,
                layout_domain: shared.jit.class_layout_domain,
                layout_gen,
                layout_epoch,
                class_name,
                resolved,
            });
        }
    });
}

/// Every per-throw precondition of a direct build of `class_id` (already
/// known initialized) with or without a message: used by
/// [`super::try_build_implicit_throwable_directly`].
pub(super) fn direct_build_slots(
    shared: &SharedVm,
    class_id: ClassId,
    with_message: bool,
) -> Option<DirectBuildSlots> {
    if direct_build_observable(shared) {
        return None;
    }
    let plan = cached_direct_build_plan(shared, class_id, with_message)?;
    if !matches!(
        crate::vm::get_static_shared(shared, plan.throwable_class, plan.jfr_tracing_static),
        Value::Int(0)
    ) {
        return None;
    }
    let slots = resolve_direct_build_slots_for_plan(shared, class_id, plan)?;
    if !matches!(
        crate::vm::get_static_shared(shared, plan.throwable_class, plan.unassigned_stack_static),
        Value::Object(Some(_))
    ) || !matches!(
        crate::vm::get_static_shared(
            shared,
            plan.throwable_class,
            plan.suppressed_sentinel_static,
        ),
        Value::Object(Some(_))
    ) {
        return None;
    }
    Some(slots)
}

/// Check if direct compact writes + exact write barriers are sound right now
/// for `obj` (no active SATB marking, no ZGC colored stores, no debug watches,
/// and `obj` actually carries a compact header of `class_id`).
#[inline]
fn can_direct_compact_write_now(
    heap: &crate::memory::VmHeap,
    obj: ObjectRef,
    class_id: ClassId,
    compact: &DirectCompactLayout,
) -> bool {
    let flags = cratonvm_gc::gc_flags();
    if flags.dbg_badref
        || flags.dbg_deadref_store
        || flags.dbg_stale_objref
        || crate::runtime::env_cache::any_field_diag()
        || crate::runtime::env_cache::dbg_field_watch()
    {
        return false;
    }
    if heap.is_zgc() {
        if heap.zgc_load_barrier_colored_stores()
            || heap.zgc_mark_active()
            || cratonvm_gc::zgc::punned_store_watch_armed()
        {
            return false;
        }
    } else if heap.is_g1() {
        if heap.g1_satb_queue().is_some_and(|q| q.is_active()) {
            return false;
        }
    } else if heap.is_generational() && heap.gen_satb_active() {
        return false;
    }
    let header = heap.get_header(obj);
    cratonvm_types::is_compact_object(header)
        && header.kind() != cratonvm_gc::heap::ObjectKind::Array
        && header.class_id == class_id
        && header.num_slots() == compact.num_fields
        && compact.total_size <= 1024
}

#[inline]
unsafe fn write_compact_slot(obj: ObjectRef, slot: CompactSlot, value: Value) {
    // SAFETY: `slot` was resolved from the live compact layout verified on `obj`'s header.
    let ptr = unsafe { obj.as_ptr().add(slot.offset as usize) };
    // SAFETY: `ptr` is naturally aligned for `slot.storage` within `obj`.
    unsafe { cratonvm_types::write_compact_field(ptr, slot.storage, value, Ordering::Relaxed) };
}

/// Constructor chain stores + stack capture on the throwable pinned at
/// `thread.native_pin_roots[pin_base]`, using pre-resolved `unassigned` and
/// `sentinel` values and direct compact writes + exact write barriers when safe.
#[allow(clippy::too_many_arguments)]
pub(super) fn fill_throwable_directly_with_statics(
    shared: &SharedVm,
    thread: &mut JvmThread,
    slots: &DirectBuildSlots,
    compact: Option<DirectCompactLayout>,
    class_id: ClassId,
    pin_base: usize,
    message_pin: Option<usize>,
    unassigned: Value,
    sentinel: Value,
) {
    use cratonvm_native_api::registry::{NativeExceptionAccess, NativeHeapAccess};

    let heap = &shared.mem.heap;
    let obj = thread.native_pin_roots[pin_base];
    let cause_val = Value::Object(Some(obj));

    if let Some(c) = compact.filter(|c| can_direct_compact_write_now(heap, obj, class_id, c)) {
        // SAFETY: `can_direct_compact_write_now` verified `obj` is a live compact
        // instance of `class_id`, and `c` holds its validated compact offsets.
        unsafe {
            write_compact_slot(obj, c.cause, cause_val);
            write_compact_slot(obj, c.stack_trace, unassigned);
            write_compact_slot(obj, c.suppressed, sentinel);
            if let Some(npe_slot) = c.npe_state {
                write_compact_slot(obj, npe_slot, Value::Int(1));
            }
        }
        if heap.is_zgc() {
            heap.zgc_note_ref_store(obj.as_ptr() as usize);
        } else {
            heap.write_barrier(obj, cause_val);
            heap.write_barrier(obj, unassigned);
            heap.write_barrier(obj, sentinel);
        }
    } else {
        heap.set_field(obj, slots.cause, cause_val);
        heap.set_field(obj, slots.stack_trace, unassigned);
        heap.set_field(obj, slots.suppressed, sentinel);
        if let Some(slot) = slots.npe_state {
            heap.set_field(obj, slot, Value::Int(1));
        }
    }

    // `fillInStackTrace(0)`: the native's capture and its two stores.
    let depth = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        ctx.capture_throwable_stack_trace(obj)
    };
    let obj = thread.native_pin_roots[pin_base];
    let depth = i32::try_from(depth).unwrap_or(i32::MAX);
    let filled = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        ctx.finish_throwable_fill(obj, depth)
    };
    let obj = thread.native_pin_roots[pin_base];
    let msg_val = message_pin.map(|pin| Value::Object(Some(thread.native_pin_roots[pin])));

    // Re-check after capture: capture may trigger GC and activate concurrent marking.
    if let Some(c) = compact.filter(|c| can_direct_compact_write_now(heap, obj, class_id, c)) {
        let backtrace_val = Value::Object(Some(obj));
        // SAFETY: `can_direct_compact_write_now` re-verified `obj` post-capture.
        unsafe {
            if filled.is_none() {
                write_compact_slot(obj, c.backtrace, backtrace_val);
                write_compact_slot(obj, c.depth, Value::Int(depth));
            }
            if let Some(mv) = msg_val {
                write_compact_slot(obj, c.detail_message, mv);
            }
        }
        if heap.is_zgc() {
            if filled.is_none() || msg_val.is_some() {
                heap.zgc_note_ref_store(obj.as_ptr() as usize);
            }
        } else {
            if filled.is_none() {
                heap.write_barrier(obj, backtrace_val);
            }
            if let Some(mv) = msg_val {
                heap.write_barrier(obj, mv);
            }
        }
    } else {
        if filled.is_none() {
            heap.set_field(obj, slots.backtrace, Value::Object(Some(obj)));
            heap.set_field(obj, slots.depth, Value::Int(depth));
        }
        if let Some(mv) = msg_val {
            heap.set_field(obj, slots.detail_message, mv);
        }
    }
}

/// The constructor chain's effect on the throwable pinned at
/// `thread.native_pin_roots[pin_base]` (used by
/// [`super::try_build_implicit_throwable_directly`], where allocation intervened
/// after `direct_build_slots` and statics must be re-read).
pub(super) fn fill_throwable_directly(
    shared: &SharedVm,
    thread: &mut JvmThread,
    slots: &DirectBuildSlots,
    pin_base: usize,
    message_pin: Option<usize>,
) {
    let unassigned = crate::vm::get_static_shared(
        shared,
        slots.plan.throwable_class,
        slots.plan.unassigned_stack_static,
    );
    let sentinel = crate::vm::get_static_shared(
        shared,
        slots.plan.throwable_class,
        slots.plan.suppressed_sentinel_static,
    );
    let obj = thread.native_pin_roots[pin_base];
    let header = shared.mem.heap.get_header(obj);
    let class_id = header.class_id;
    let compact = resolve_direct_compact_layout(
        shared.jit.class_layout_domain,
        class_id,
        header.num_slots(),
        slots,
    );
    fill_throwable_directly_with_statics(
        shared,
        thread,
        slots,
        compact,
        class_id,
        pin_base,
        message_pin,
        unassigned,
        sentinel,
    );
}
