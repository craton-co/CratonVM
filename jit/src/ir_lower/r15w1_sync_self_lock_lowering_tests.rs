use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;

extern "C" fn fake_enter(_vm: i64, obj: i64) -> i64 {
    obj
}

/// The real `jit_monitor_exit` answers `1` on success.

extern "C" fn fake_exit(_vm: i64, _obj: i64) -> i64 {
    1
}

/// Never called: the self-lock's mirror is read from its slot.
extern "C" fn fake_ldc_class(_vm: i64, _holder: i64, _cp: i64) -> i64 {
    0
}

fn helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires the two monitor slots and the class-constant slot.
    let mut h: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.monitor_enter = fake_enter as *const () as usize;
    h.monitor_exit = fake_exit as *const () as usize;
    h.ldc_class_cp = fake_ldc_class as *const () as usize;
    h
}


fn occurrences(cm: &CompiledMethod, word: usize) -> usize {
    let target = word.to_le_bytes();
    cm.code_bytes().windows(8).filter(|w| *w == target).count()
}



/// The static frame shape: local 0 is an ordinary (here `int`) argument,
/// which the instance check would refuse and the static one does not ask.
#[test]
fn a_static_frame_asks_nothing_of_local_zero() {
    let fs = FrameState {
        class_id: 0,
        method_key: String::new(),
        bci: 2,
        locals: vec![FrameValue::StackSlot(-8), FrameValue::StackSlot(-16)],
        stack: Vec::new(),
        monitors: vec![crate::deopt::MonitorInfo {
            object: FrameValue::StackSlotRef(-40),
            lock_depth: 1,
            relock: false,
        }],
        caller: None,
    };
    assert_eq!(self_lock_frame_refusal_for(&fs, false), None);
    assert!(self_lock_frame_refusal_for(&fs, true).is_some());
    let mut none = fs.clone();
    none.monitors.clear();
    assert!(self_lock_frame_refusal_for(&none, false).is_some());
}
