// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A loop's internal return must not close its splice before the back edge.
use super::*;
use crate::ir::{IrBuilder, IrInlineSite};

#[test]
#[cfg(target_arch = "x86_64")]
fn a_single_return_loop_splice_keeps_its_latch_and_caller_continuation() {
    // outer(n) = until(n) + 7; until(n) counts from zero until i >= n.
    let code = [
        0x1a, 0xb8, 0, 1, 0x10, 7, 0x60, 0xac, 0x03, 0x3c, 0x1b, 0x1a, 0xa1, 0, 5, 0x1b, 0xac,
        0x84, 1, 1, 0xa7, 0xff, 0xf6,
    ];
    let site = IrInlineSite {
        base: 8,
        code_len: 15,
        num_args: 1,
        max_locals: 2,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: "T.until:(I)I".into(),
        class_id: 0,
    };
    let mut builder = IrBuilder::new(1, 2);
    builder.set_param_types(&[IrType::Int]);
    builder.set_inline_sites(HashMap::from([(1, site)]));
    builder.set_spliced_bodies_pure(true);
    let mut graph = builder
        .build(&code, 8)
        .expect("single-return loop splice builds");
    crate::ir_optimize::optimize(&mut graph);
    let schedule = crate::ir_schedule::schedule(&graph);
    let cm = lower_inner(
        &graph,
        &schedule,
        1,
        2,
        &JitRuntimeHelpers::default(),
        &HashMap::new(),
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &crate::ir::IrInlineFrameSites::default(),
    )
    .expect("loop splice lowers without missing latch inputs");
    for n in [-17, 0, 1, 2, 57, 1000] {
        // SAFETY: this is int outer(int), with no runtime calls or enabled polls.
        let got = unsafe { cm.try_call_with_context(0, &[n]) }.expect("call");
        assert_eq!(got as i32, (n as i32).max(0) + 7, "outer({n})");
    }
}
