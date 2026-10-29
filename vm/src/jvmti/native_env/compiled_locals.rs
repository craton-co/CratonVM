// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane JVMTIFRAME;
//! `docs/known-issues/jit/j27-jvmti-compiled-frames-answer-opaque-20261028.md`):
//! `GetLocalInt` / `Long` / `Float` / `Double` / `Object` and
//! `GetLocalInstance` of a compiled activation, and of an interpreter frame
//! whose body runs compiled (an OSR'd loop), from the activation's home slots
//! (`interpreter::compiled_frame_locals`), where the table answered
//! `JVMTI_ERROR_OPAQUE_FRAME`. HotSpot 25 answers both from the compiled
//! frame's debug info. Only on the thread that owns the frame (the current
//! thread, or a suspended one parked for the debugger running the read
//! itself); a write (`SetLocal*`) still answers `OPAQUE_FRAME`, because the
//! home slot is store-only (stage 3 of
//! `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`).
//! Under `CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS` (default OFF).

use super::{
    declared_in_code, global_ref_of, local_read_of, Declared, FrameInfo, LocalKind, LocalOp,
    LocalRead, ERR_INVALID_SLOT, ERR_OPAQUE_FRAME, ERR_TYPE_MISMATCH,
};
use crate::classloading::ClassId;
use crate::native::jni::{self, JInt};
use crate::threading::jvm_thread::JvmThread;
use crate::runtime::interpreter::compiled_frame_locals::{own_home_values, HomeRow};
use crate::types::Value;
use crate::vm::SharedVm;

/// What a home read of `op` asks: the kind and the slot, and whether the
/// method's `LocalVariableTable` is consulted (`GetLocalInstance` reads slot
/// 0 without it, as the interpreter frame's path does).
fn asked(op: LocalOp) -> Result<(LocalKind, JInt, bool), JInt> {
    match op {
        LocalOp::Read(kind, slot) => Ok((kind, slot, true)),
        LocalOp::Instance => Ok((LocalKind::Object, 0, false)),
        // The frame must leave for the interpreter before a write can stick.
        LocalOp::Write(..) => Err(ERR_OPAQUE_FRAME),
    }
}

/// What the row's method table says of the slot a home read asks
/// ([`declared_for`]), and `redefined_at`, the stamp of its class's last
/// redefinition (`RedefinitionHistory::latest_redefinition`; `None`: never
/// redefined).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RowTable {
    declared: Declared,
    redefined_at: Option<u64>,
}

impl RowTable {
    /// The table a read of an activation compiled at `stamp` is checked
    /// against: none for a body compiled before its class's last
    /// redefinition, or one with no stamp once the class was redefined (j27
    /// wave 8, lane W8REV). Such an activation runs the obsolete bytecode,
    /// which the class's current table may not describe, and the
    /// interpreter path reads an obsolete frame table-less too
    /// (`declared_local`'s `runs_obsolete_method`).
    fn for_activation(self, stamp: Option<u64>) -> Declared {
        match self.redefined_at {
            Some(latest) if stamp.map_or(true, |s| s < latest) => Declared::NoTable,
            _ => self.declared,
        }
    }
}

/// [`RowTable`] for the slot `op` reads of the method the row `info` names,
/// at the row's location. It locks the class manager, so a caller asks it
/// before it enters a foreign-JNI entry.
pub(super) fn declared_for(shared: &SharedVm, info: &FrameInfo, op: LocalOp) -> RowTable {
    let none = RowTable { declared: Declared::NoTable, redefined_at: None };
    let LocalOp::Read(_, slot) = op else {
        return none;
    };
    let (Ok(slot), Ok(bci)) = (u16::try_from(slot), usize::try_from(info.location)) else {
        return none;
    };
    if info.method == 0 {
        return none;
    }
    // Cast: the id's halves, as `with_method` decodes them.
    let class_id = ClassId::new((info.method >> 32) as u32);
    let index = (info.method & 0xFFFF) as usize;
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return none;
    };
    let redefined_at = cm.redefinition_history(class_id).map(|h| h.latest_redefinition());
    let declared = match class.methods.get(index).and_then(|m| m.code()) {
        Some(code) => declared_in_code(class, code, bci, slot),
        None => Declared::NoTable,
    };
    RowTable { declared, redefined_at }
}

/// Local `slot` of `kind` among a home read's `values`, or the error, in
/// `checked_local`'s order (HotSpot 25.0.3's, measured for interpreter
/// frames): `INVALID_SLOT` outside the locals (a wide value's second slot
/// included); `TYPE_MISMATCH` when the slot holds a reference and `kind` is
/// not `Object`, or the reverse; then, with a table (`declared`),
/// `INVALID_SLOT` for a slot no entry covers and `TYPE_MISMATCH` for another
/// declared kind. A never-written slot reads as `int` 0, as an interpreter
/// frame's does (`DebuggerHomeLocal::Undefined`).
pub(super) fn checked_home_value(
    values: &[Value],
    declared: Option<Declared>,
    slot: JInt,
    kind: LocalKind,
) -> Result<Value, JInt> {
    let at = usize::try_from(slot).map_err(|_| ERR_INVALID_SLOT)?;
    if at + usize::from(kind.wide()) >= values.len() {
        return Err(ERR_INVALID_SLOT);
    }
    let value = values[at];
    if matches!(value, Value::Object(_)) != (kind == LocalKind::Object) {
        return Err(ERR_TYPE_MISMATCH);
    }
    match declared {
        Some(Declared::NotLive) => return Err(ERR_INVALID_SLOT),
        Some(Declared::Kind(k)) if k != kind => return Err(ERR_TYPE_MISMATCH),
        _ => {}
    }
    Ok(match value {
        Value::Uninitialized => Value::Int(0),
        v => v,
    })
}

/// `op` on the home row `home` of the CALLING thread, whose interpreter
/// frames are `frames`, with `table` from [`declared_for`]: the kind and
/// the value, or the error (`OPAQUE_FRAME` when the homes cannot be read).
/// A reference answered is an address valid until this thread's next
/// safepoint: the caller makes its handle first (inside its foreign-JNI
/// entry, or parked between two polls).
pub(super) fn home_op(
    frames: &[crate::runtime::frame::Frame],
    home: &HomeRow,
    op: LocalOp,
    table: RowTable,
) -> Result<(LocalKind, Value), JInt> {
    let (kind, slot, with_table) = asked(op)?;
    let homes = own_home_values(frames, home).ok_or(ERR_OPAQUE_FRAME)?;
    let declared = table.for_activation(homes.compile_cp_stamp);
    let value = checked_home_value(&homes.values, with_table.then_some(declared), slot, kind)?;
    Ok((kind, value))
}

/// [`home_op`] (a read) for the calling thread `current`, its own live
/// `JvmThread`, answered as the caller's local reference for an object: the
/// homes are read and the handle made inside the foreign-thread entry, so no
/// collection moves the reference meanwhile.
pub(super) fn current_home_read(
    shared: &SharedVm,
    current: *mut JvmThread,
    info: &FrameInfo,
    home: &HomeRow,
    op: LocalOp,
) -> Result<LocalRead, JInt> {
    let declared = declared_for(shared, info, op);
    let _fx = jni::ForeignJniEntry::enter();
    // SAFETY: the calling thread's own `JvmThread`, inside this call: its
    // frames do not change while they are read.
    let frames: &[crate::runtime::frame::Frame] = unsafe { &(*current).frames };
    let (kind, value) = home_op(frames, home, op, declared)?;
    local_read_of(kind, value, |obj| {
        let prev = jni::replace_jni_context(shared);
        let handle = obj.map_or(0, jni::new_local_handle);
        jni::restore_jni_context(prev);
        handle
    })
}

/// [`home_op`] (a read) on `thread`, parked for the debugger and running
/// this itself between two polls (`local_op_on_parked`): an object comes
/// back as a JNI global reference, as every parked read answers one.
pub(super) fn parked_home_read(
    shared: &SharedVm,
    thread: &JvmThread,
    info: &FrameInfo,
    home: &HomeRow,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    let declared = declared_for(shared, info, op);
    let (kind, value) = home_op(&thread.frames, home, op, declared)?;
    local_read_of(kind, value, |obj| global_ref_of(shared, obj)).map(Some)
}

#[cfg(test)]
mod j27_jvmtiframe_checked_home_value_tests {
    //! j27 wave 5 (lane JVMTIFRAME): the codes a compiled row's local read
    //! answers, in the interpreter frame's order.
    use super::*;

    /// A static `mid(int x, long w, String s)` at a call: x, w (two slots),
    /// s (null), and an unassigned local.
    fn values() -> Vec<Value> {
        vec![
            Value::Int(7),
            Value::Long(1 << 40),
            Value::Uninitialized,
            Value::Object(None),
            Value::Uninitialized,
        ]
    }

    #[test]
    fn a_home_read_answers_the_value_of_its_kind() {
        let v = values();
        assert_eq!(checked_home_value(&v, None, 0, LocalKind::Int), Ok(Value::Int(7)));
        assert_eq!(checked_home_value(&v, None, 1, LocalKind::Long), Ok(Value::Long(1 << 40)));
        assert_eq!(checked_home_value(&v, None, 3, LocalKind::Object), Ok(Value::Object(None)));
        // Never written: int 0, as an interpreter frame's slot.
        assert_eq!(checked_home_value(&v, None, 4, LocalKind::Int), Ok(Value::Int(0)));
    }

    #[test]
    fn a_slot_outside_the_locals_is_invalid_before_any_type_check() {
        let v = values();
        assert_eq!(checked_home_value(&v, None, -1, LocalKind::Int), Err(ERR_INVALID_SLOT));
        assert_eq!(checked_home_value(&v, None, 5, LocalKind::Object), Err(ERR_INVALID_SLOT));
        // A long at the last slot has no second one.
        assert_eq!(checked_home_value(&v, None, 4, LocalKind::Long), Err(ERR_INVALID_SLOT));
    }

    #[test]
    fn a_reference_read_as_a_primitive_and_the_reverse_mismatch() {
        let v = values();
        assert_eq!(checked_home_value(&v, None, 3, LocalKind::Int), Err(ERR_TYPE_MISMATCH));
        assert_eq!(checked_home_value(&v, None, 0, LocalKind::Object), Err(ERR_TYPE_MISMATCH));
        assert_eq!(checked_home_value(&v, None, 4, LocalKind::Object), Err(ERR_TYPE_MISMATCH));
    }

    #[test]
    fn the_table_decides_after_the_tag() {
        let v = values();
        let not_live = Some(Declared::NotLive);
        assert_eq!(checked_home_value(&v, not_live, 0, LocalKind::Int), Err(ERR_INVALID_SLOT));
        let as_float = Some(Declared::Kind(LocalKind::Float));
        assert_eq!(checked_home_value(&v, as_float, 0, LocalKind::Int), Err(ERR_TYPE_MISMATCH));
        let as_int = Some(Declared::Kind(LocalKind::Int));
        assert_eq!(checked_home_value(&v, as_int, 0, LocalKind::Int), Ok(Value::Int(7)));
        // The tag check comes first: a reference slot read as int is a
        // mismatch even where the table says nothing covers it.
        assert_eq!(checked_home_value(&v, not_live, 3, LocalKind::Int), Err(ERR_TYPE_MISMATCH));
    }

    #[test]
    fn a_write_is_not_served_and_the_instance_skips_the_table() {
        assert_eq!(
            asked(LocalOp::Write(LocalKind::Int, 0, super::super::LocalRead::Int(1))).err(),
            Some(ERR_OPAQUE_FRAME)
        );
        assert_eq!(asked(LocalOp::Instance), Ok((LocalKind::Object, 0, false)));
        assert_eq!(asked(LocalOp::Read(LocalKind::Long, 2)), Ok((LocalKind::Long, 2, true)));
    }

    /// j27 wave 8 (lane W8REV): a body compiled before its class's last
    /// redefinition is read without the current table (the interpreter path's
    /// rule for an obsolete frame); a current body keeps the table's answer.
    #[test]
    fn j27_w8rev_an_obsolete_compiled_body_is_read_without_the_table() {
        let not_live = Declared::NotLive;
        let never = RowTable { declared: not_live, redefined_at: None };
        assert_eq!(never.for_activation(Some(0)), not_live);
        assert_eq!(never.for_activation(None), not_live);
        let redefined = RowTable { declared: not_live, redefined_at: Some(5) };
        // Compiled at or after the last redefinition: the current bytecode.
        assert_eq!(redefined.for_activation(Some(5)), not_live);
        assert_eq!(redefined.for_activation(Some(9)), not_live);
        // Compiled before it, or no stamp to tell: table-less.
        assert_eq!(redefined.for_activation(Some(4)), Declared::NoTable);
        assert_eq!(redefined.for_activation(None), Declared::NoTable);
        // End to end: the stale body's slot reads its value where the current
        // table would have answered INVALID_SLOT.
        let v = values();
        let stale = redefined.for_activation(Some(4));
        assert_eq!(checked_home_value(&v, Some(stale), 0, LocalKind::Int), Ok(Value::Int(7)));
        let current = redefined.for_activation(Some(5));
        assert_eq!(
            checked_home_value(&v, Some(current), 0, LocalKind::Int),
            Err(ERR_INVALID_SLOT)
        );
    }
}
