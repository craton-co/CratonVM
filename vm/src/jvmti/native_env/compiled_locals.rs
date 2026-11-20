// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane JVMTIFRAME;
//! `docs/feature-designs/native-dispatch-architecture.md`):
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
mod j27_jvmtiframe_checked_home_value_tests;
