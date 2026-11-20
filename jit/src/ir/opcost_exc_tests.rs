// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Unit tests for `OpCostRows.throwCatch` / `throwNew` IR local-throw event
//! guards and JDK `Throwable.<init>` dispatch routing.

use super::*;
use std::collections::HashMap;
use std::sync::atomic::AtomicU8;

static FLAG_EX: AtomicU8 = AtomicU8::new(0);
static FLAG_CATCH: AtomicU8 = AtomicU8::new(0);

fn event_addrs() -> [usize; 2] {
    [
        std::ptr::from_ref(&FLAG_EX) as usize,
        std::ptr::from_ref(&FLAG_CATCH) as usize,
    ]
}

#[test]
fn opcost_throw_catch_hoists_local_throw_event_guards_to_entry() {
    // static int throwCatch(int n) {
    //   int s = 0;
    //   for (int i = 0; i < n; i++) {
    //     try { if (i >= 0) throw EX; } catch (RuntimeException e) { s++; }
    //   }
    //   return s;
    // }
    #[rustfmt::skip]
    let code: [u8; 29] = [
        0x03,             // 0: iconst_0
        0x3c,             // 1: istore_1 (s)
        0x03,             // 2: iconst_0
        0x3d,             // 3: istore_2 (i)
        0x1c,             // 4: iload_2
        0x1a,             // 5: iload_0 (n)
        0xa2, 0x00, 0x15, // 6: if_icmpge 27
        0x1c,             // 9: iload_2
        0x9b, 0x00, 0x0b, // 10: iflt 21
        0xb2, 0x00, 0x01, // 13: getstatic #1 (EX)
        0xbf,             // 16: athrow
        0x4e,             // 17: astore_3 (handler)
        0x84, 0x01, 0x01, // 18: iinc 1, 1
        0x84, 0x02, 0x01, // 21: iinc 2, 1
        0xa7, 0xff, 0xec, // 24: goto 4
        0x1b,             // 27: iload_1
        0xac,             // 28: ireturn
    ];

    let addrs = event_addrs();
    let mut b = IrBuilder::new(1, 4);
    b.set_static_field_info(HashMap::from([(
        13usize,
        (9u32, 0u32 as usize, b'L', false),
    )]));
    b.set_static_final_exact_classes(HashMap::from([((9u32, 0u32), 10u32)]));
    b.set_local_throw_rows(vec![IrLocalThrowRow {
        start_pc: 13,
        end_pc: 17,
        handler_pc: 17,
        catch: IrCatchClass::Class(10),
    }]);
    b.set_exception_event_guard(Some(addrs));

    let g = b.build(&code, code.len()).expect("throwCatch IR builds");
    assert!(
        !g.nodes.iter().any(|n| n.op == Op::Throw),
        "local throw event guard must use Op::Guard, not Op::Throw"
    );

    let event_guards: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| {
            matches!(n.op, Op::Guard { .. })
                && n.input_opt(1)
                    .and_then(|tid| g.node_opt(tid))
                    .is_some_and(|tn| {
                        matches!(
                            tn.op,
                            Op::ClassInitTest {
                                state_addr,
                                initialized: 0
                            } if addrs.contains(&state_addr)
                        )
                    })
        })
        .collect();
    assert_eq!(
        event_guards.len(),
        2,
        "expected exactly 2 hoisted entry guards, got {}",
        event_guards.len()
    );
    for guard in event_guards {
        assert_eq!(guard.op, Op::Guard { bci: 0 });
        assert_eq!(guard.input_opt(0), Some(1));
    }
}

#[test]
fn opcost_effectful_throw_keeps_per_site_guard_without_if_throw() {
    // Loop with `new #1; athrow; catch`: has Op::New, so event guards stay
    // at athrow (bci 12) as direct Op::Guard nodes without Op::If/Op::Throw.
    #[rustfmt::skip]
    let code: [u8; 25] = [
        0x03,             // 0: iconst_0
        0x3c,             // 1: istore_1 (s)
        0x03,             // 2: iconst_0
        0x3d,             // 3: istore_2 (i)
        0x1c,             // 4: iload_2
        0x1a,             // 5: iload_0 (n)
        0xa2, 0x00, 0x11, // 6: if_icmpge 23
        0xbb, 0x00, 0x01, // 9: new #1
        0xbf,             // 12: athrow
        0x4e,             // 13: astore_3 (handler)
        0x84, 0x01, 0x01, // 14: iinc 1, 1
        0x84, 0x02, 0x01, // 17: iinc 2, 1
        0xa7, 0xff, 0xf0, // 20: goto 4
        0x1b,             // 23: iload_1
        0xac,             // 24: ireturn
    ];

    let addrs = event_addrs();
    let mut b = IrBuilder::new(1, 4);
    b.set_new_info(
        HashMap::from([(9usize, (10u32, 0usize))]),
        Default::default(),
    );
    b.set_local_throw_rows(vec![IrLocalThrowRow {
        start_pc: 9,
        end_pc: 13,
        handler_pc: 13,
        catch: IrCatchClass::Class(10),
    }]);
    b.set_exception_event_guard(Some(addrs));

    let g = b.build(&code, code.len()).expect("new+athrow IR builds");
    assert!(
        !g.nodes.iter().any(|n| n.op == Op::Throw),
        "per-site event guard must use Op::Guard, not Op::Throw"
    );

    let site_guards: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| {
            matches!(n.op, Op::Guard { bci: 12 })
                && n.input_opt(1)
                    .and_then(|tid| g.node_opt(tid))
                    .is_some_and(|tn| {
                        matches!(
                            tn.op,
                            Op::ClassInitTest {
                                state_addr,
                                initialized: 0
                            } if addrs.contains(&state_addr)
                        )
                    })
        })
        .collect();
    assert_eq!(site_guards.len(), 2);
}

#[test]
fn jdk_throwable_init_candidate_filter() {
    assert!(is_jdk_throwable_init_candidate(
        "java/lang/IllegalStateException",
        "<init>",
        "()V"
    ));
    assert!(is_jdk_throwable_init_candidate(
        "java/lang/RuntimeException",
        "<init>",
        "(Ljava/lang/String;)V"
    ));
    assert!(is_jdk_throwable_init_candidate(
        "java/lang/Throwable",
        "<init>",
        "()V"
    ));
    assert!(is_jdk_throwable_init_candidate(
        "java/lang/AssertionError",
        "<init>",
        "()V"
    ));
    assert!(!is_jdk_throwable_init_candidate(
        "java/lang/Object",
        "<init>",
        "()V"
    ));
    assert!(!is_jdk_throwable_init_candidate(
        "java/lang/StringBuilder",
        "<init>",
        "()V"
    ));
    assert!(!is_jdk_throwable_init_candidate(
        "com/example/MyException",
        "<init>",
        "()V"
    ));
    assert!(!is_jdk_throwable_init_candidate(
        "java/lang/IllegalStateException",
        "<init>",
        "(Ljava/lang/Throwable;)V"
    ));
}
