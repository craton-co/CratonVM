use super::{clear_lower_bail, last_lower_bail};
use super::{ir_entry_mode_exits_admitted, lower_inner_with_mode_exit_polls};
use super::{IrPollModeExits, PollExitBytecode};
use crate::deopt::{DeoptReason, FrameValue};
use crate::ir::{IrBuilder, IrInlineFrameSites, IrType};
use crate::{CompiledMethod, JitRuntimeHelpers};
use std::collections::{HashMap, HashSet};

/// The stop-the-world byte, always raised: every poll takes its slow path.
static FLAG: u8 = 1;

/// `HelperFnSafepointSlowPath`; these tests only lower, never run.
extern "C" fn no_verdict(_flag_addr: i64, _body_id: i64) -> i64 {
    0
}

extern "C" fn fake_monitor(_vm: i64, obj: i64) -> i64 {
    obj
}

extern "C" fn fake_monitor_exit(_vm: i64, _obj: i64) -> i64 {
    1
}

/// `static int f(Object o) { synchronized (o) { } int i = 0;
/// while (i < 1000) i++; return i; }` as javac emits it, the block's
/// catch-all handler (9..=13) included: header at 16, outside the block.
/// `max_locals` 3. (Wave 21 rebuilt both fixtures from javac's shape: the
/// wave-19 hand-built ones had no handler and an int-typed lock object,
/// and did not lower on the build host.)
pub(super) const LOCK_THEN_LOOP: [u8; 33] = [
    0x2a, // 0:  aload_0
    0x59, // 1:  dup
    0x4c, // 2:  astore_1
    0xc2, // 3:  monitorenter
    0x2b, // 4:  aload_1
    0xc3, // 5:  monitorexit
    0xa7, 0x00, 0x08, // 6:  goto +8 -> 14
    0x4d, // 9:  astore_2            <- handler, any
    0x2b, // 10: aload_1
    0xc3, // 11: monitorexit
    0x2c, // 12: aload_2
    0xbf, // 13: athrow
    0x03, // 14: iconst_0
    0x3c, // 15: istore_1
    0x1b, // 16: iload_1             <- loop header
    0x11, 0x03, 0xe8, // 17: sipush 1000
    0xa2, 0x00, 0x09, // 20: if_icmpge +9 -> 29
    0x84, 0x01, 0x01, // 23: iinc 1, 1
    0xa7, 0xff, 0xf6, // 26: goto -10 -> 16
    0x1b, // 29: iload_1
    0xac, // 30: ireturn
    0, 0,
];
/// `[4, 6) -> 9` and `[9, 12) -> 9`, both catch-all.
const LOCK_THEN_LOOP_HANDLERS: [(usize, usize, usize); 2] = [(4, 6, 9), (9, 12, 9)];

/// `static int g(Object o) { int i = 0; synchronized (o) {
/// while (i < 1000) i++; } return i; }`: the loop INSIDE the block,
/// header at 6, where the lock is held. `max_locals` 4.
const LOOP_IN_LOCK: [u8; 33] = [
    0x03, // 0:  iconst_0
    0x3c, // 1:  istore_1
    0x2a, // 2:  aload_0
    0x59, // 3:  dup
    0x4d, // 4:  astore_2
    0xc2, // 5:  monitorenter
    0x1b, // 6:  iload_1             <- loop header
    0x11, 0x03, 0xe8, // 7:  sipush 1000
    0xa2, 0x00, 0x09, // 10: if_icmpge +9 -> 19
    0x84, 0x01, 0x01, // 13: iinc 1, 1
    0xa7, 0xff, 0xf6, // 16: goto -10 -> 6
    0x2c, // 19: aload_2
    0xc3, // 20: monitorexit
    0xa7, 0x00, 0x08, // 21: goto +8 -> 29
    0x4e, // 24: astore_3            <- handler, any
    0x2c, // 25: aload_2
    0xc3, // 26: monitorexit
    0x2d, // 27: aload_3
    0xbf, // 28: athrow
    0x1b, // 29: iload_1
    0xac, // 30: ireturn
    0, 0,
];
/// `[6, 21) -> 24` and `[24, 27) -> 24`, both catch-all.
const LOOP_IN_LOCK_HANDLERS: [(usize, usize, usize); 2] = [(6, 21, 24), (24, 27, 24)];


fn poll_exits(cm: &CompiledMethod) -> Vec<&crate::deopt::DeoptimizationPoint> {
    cm._deopt_point_boxes
        .iter()
        .filter(|p| p.reason == DeoptReason::OsrExit)
        .map(|p| &**p)
        .collect()
}

/// Both shapes hold a monitor: the door admits them exactly when the
/// resume arm the vouch serves is on. Wave 22: as an `ACC_SYNCHRONIZED`
/// method's too, on the same arm (the IR door's own vouch).
#[test]
fn a_method_with_a_synchronized_block_is_admitted_on_the_vouching_arm() {
    for code in [&LOCK_THEN_LOOP, &LOOP_IN_LOCK] {
        assert!(crate::bytecode_holds_monitor(code, code.len() - 2));
        assert_eq!(
            ir_entry_mode_exits_admitted(code, code.len() - 2, false),
            super::IR_MONITOR_BLOCK_MODE_EXITS_ENABLED && crate::deopt_real_enabled()
        );
        assert_eq!(
            ir_entry_mode_exits_admitted(code, code.len() - 2, true),
            super::IR_SYNCHRONIZED_METHOD_MODE_EXITS_ENABLED && crate::deopt_real_enabled()
        );
    }
}


