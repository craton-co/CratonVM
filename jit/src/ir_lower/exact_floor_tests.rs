// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::IrBuilder;

#[derive(Default)]
struct GuardState {
    calls: usize,
    receiver: i64,
    answer: i64,
    body_calls: usize,
}

extern "C" fn guard(ctx: *mut GuardState, receiver: i64) -> i64 {
    // SAFETY: this fixture passes its live local state as the hidden context.
    let s = unsafe { &mut *ctx };
    s.calls += 1;
    s.receiver = receiver;
    s.answer
}

extern "C" fn callee(ctx: *mut GuardState, x: i64) -> i64 {
    let s = unsafe { &mut *ctx };
    s.body_calls += 1;
    7000 + x
}

extern "C" fn dispatch(ctx: *mut GuardState, _info: usize, args: *const i64, _n: usize) -> i64 {
    let s = unsafe { &mut *ctx };
    s.body_calls += 1;
    5000 + unsafe { *args }
}

struct MirrorRestore(usize);
impl Drop for MirrorRestore {
    fn drop(&mut self) {
        crate::x64::publish_jit_thread_mirror(self.0);
    }
}

extern "C" fn native_floor() -> i64 {
    0
}

fn body(constructor: bool, floor_word: bool, cell: usize) -> CompiledMethod {
    let info = Box::new(JitInvokeInfo {
        class_name: "fixture/Guard",
        method_name: if constructor { "<init>" } else { "cycle" },
        descriptor: if constructor { "()V" } else { "(I)I" },
        num_jit_args: 1,
        return_type: if constructor { b'V' } else { b'I' },
        invoke_kind: if constructor { 1 } else { 3 },
        declaring_class_id: 0,
        owner_class_id: 0,
    });
    let info_ptr = &*info as *const JitInvokeInfo as usize;
    let (code, len) = if constructor {
        (vec![0x2a, 0xb7, 0, 2, 0x10, 77, 0xac, 0, 0], 7)
    } else {
        (vec![0x1a, 0xb8, 0, 2, 0xac, 0, 0], 5)
    };
    let mut builder = IrBuilder::new(1, 1);
    builder.set_param_types(&[if constructor {
        IrType::Ref
    } else {
        IrType::Int
    }]);
    builder.set_invoke_info(HashMap::from([(1, (info_ptr, 1, info.return_type))]));
    let graph = builder.build(&code, len).unwrap();
    let schedule = crate::ir_schedule::schedule(&graph);
    let mut helpers = JitRuntimeHelpers::default();
    helpers.invoke_dispatch = dispatch as *const () as usize;
    helpers.self_call_stack_guard = guard as *const () as usize;
    helpers.native_stack_floor_fn = native_floor as *const () as usize;
    helpers.jit_stack_floor_offset_in_thread = if floor_word { 8 } else { 0 };
    let direct = if constructor {
        HashMap::from([(1, (callee as *const () as usize, true))])
    } else {
        HashMap::new()
    };
    let cells = if constructor {
        HashMap::new()
    } else {
        HashMap::from([(1, cell)])
    };
    let mut cm = lower_inner_with_cycle_cells(
        &crate::DirectHelperTable::EMPTY,
        &graph,
        &schedule,
        1,
        1,
        &helpers,
        &HashMap::new(),
        &[],
        None,
        &direct,
        &HashMap::new(),
        &HashMap::new(),
        &crate::ir::IrInlineFrameSites::default(),
        &cells,
        &std::collections::HashSet::new(),
    )
    .expect("guarded call lowers");
    cm._jit_invoke_infos.push(info);
    cm
}

#[test]
fn constructor_and_cycle_guards_use_the_exact_published_floor() {
    let Some(old) = crate::x64::jit_thread_mirror_read() else {
        return;
    };
    let _restore = MirrorRestore(old);
    for constructor in [false, true] {
        let mut cell = [callee as *const () as usize as u64, 0, 0, 0];
        for floor_word in [false, true] {
            crate::x64::publish_jit_thread_mirror(0);
            let cm = body(constructor, floor_word, cell.as_mut_ptr() as usize);
            for mirror in [0, 1, 2] {
                // absent, enough room, at/below the floor
                let thread_words = [0_usize, if mirror == 2 { usize::MAX } else { 0 }];
                let ptr = if mirror == 0 {
                    0
                } else {
                    thread_words.as_ptr() as usize
                };
                crate::x64::publish_jit_thread_mirror(ptr);
                for trip in [false, true] {
                    let mut s = GuardState {
                        answer: if trip { i64::MIN } else { 0 },
                        ..Default::default()
                    };
                    let guarded = !floor_word || mirror != 1;
                    let expected = if guarded && trip {
                        i64::MIN
                    } else if constructor {
                        77
                    } else {
                        7003
                    };
                    let arg = if constructor { 0x1234_5678 } else { 3 };
                    // SAFETY: the only native calls use the bounded state above;
                    // fake TLS storage stays live and is restored on every exit.
                    let got = unsafe { cm.try_call_with_context(&mut s as *mut _ as i64, &[arg]) }
                        .unwrap();
                    assert_eq!(
                        got, expected,
                        "ctor={constructor} word={floor_word} mirror={mirror} trip={trip}"
                    );
                    assert_eq!(
                        s.calls,
                        usize::from(guarded),
                        "a proven floor skips the helper, never samples: ctor={constructor} word={floor_word} mirror={mirror} trip={trip}"
                    );
                    assert_eq!(s.body_calls, usize::from(!(guarded && trip)));
                    if guarded {
                        assert_eq!(s.receiver, if constructor { arg } else { 0 });
                    }
                }
                crate::x64::publish_jit_thread_mirror(0);
            }
        }
    }
}
