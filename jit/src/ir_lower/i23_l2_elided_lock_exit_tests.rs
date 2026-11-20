use super::{clear_lower_bail, last_lower_bail, lower_inner_with_mode_exit_polls};
use super::{IrPollModeExits, PollExitBytecode};
use crate::deopt::{DeoptReason, FrameValue};
use crate::ir::{IrBuilder, IrInlineFrameSites, IrType};
use crate::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

extern "C" fn no_verdict(_flag_addr: i64, _body_id: i64) -> i64 {
    0
}

extern "C" fn fake_monitor(_vm: i64, obj: i64) -> i64 {
    obj
}

extern "C" fn fake_monitor_exit(_vm: i64, _obj: i64) -> i64 {
    1
}

/// `static int h(Object o) { int i = 0; synchronized (o) {
/// synchronized (o) { while (i < 1000) i++; } } return i; }`, without
/// the catch-all handlers (the nesting is what is under test): header at
/// 6, inside both regions. `max_locals` 2.
const LOOP_IN_NESTED_LOCK: [u8; 27] = [
    0x03, // 0:  iconst_0
    0x3c, // 1:  istore_1
    0x2a, // 2:  aload_0
    0xc2, // 3:  monitorenter
    0x2a, // 4:  aload_0
    0xc2, // 5:  monitorenter
    0x1b, // 6:  iload_1             <- loop header
    0x11, 0x03, 0xe8, // 7:  sipush 1000
    0xa2, 0x00, 0x09, // 10: if_icmpge +9 -> 19
    0x84, 0x01, 0x01, // 13: iinc 1, 1
    0xa7, 0xff, 0xf6, // 16: goto -10 -> 6
    0x2a, // 19: aload_0
    0xc3, // 20: monitorexit
    0x2a, // 21: aload_0
    0xc3, // 22: monitorexit
    0x1b, // 23: iload_1
    0xac, // 24: ireturn
    0, 0,
];

