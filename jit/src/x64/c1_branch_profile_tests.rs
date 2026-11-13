// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use std::sync::Arc;

fn build(code: &[u8], counters: Option<Arc<crate::profile::BranchCounters>>) -> CompiledMethod {
    build_with_params(code, 1, counters)
}

fn build_with_params(
    code: &[u8],
    params: usize,
    counters: Option<Arc<crate::profile::BranchCounters>>,
) -> CompiledMethod {
    compile_with_request(
        BackendRequest {
            baseline_mode: true,
            verified_max_stack: Some(2),
            c1_branch_counters: counters,
            ..BackendRequest::default()
        },
        Vec::new(),
        code,
        code.len() - 2,
        params,
        params,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("baseline branch fixture compiles")
}

#[test]
fn native_c1_branches_continue_the_methods_actual_observations() {
    let values = [i32::MIN, -7, -1, 0, 1, 7, i32::MAX];
    for op in 0x99..=0x9e {
        // iload_0; if<cond> 6; iconst_1; ireturn; iconst_m1; ireturn.
        let code = [0x1a, op, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
        let bank = Arc::new(crate::profile::BranchCounters::new(8));
        let plain = build(&code, None);
        let profiled = build(&code, Some(bank.clone()));
        let mut taken = 0;
        for x in values {
            let branch = match op {
                0x99 => x == 0,
                0x9a => x != 0,
                0x9b => x < 0,
                0x9c => x >= 0,
                0x9d => x > 0,
                _ => x <= 0,
            };
            let result = if branch { -1 } else { 1 };
            assert_eq!(unsafe { plain.try_call(&[i64::from(x)]) }, Ok(result));
            assert_eq!(unsafe { profiled.try_call(&[i64::from(x)]) }, Ok(result));
            taken += u32::from(branch);
        }
        assert_eq!(
            bank.counts(1),
            (taken, 7 - taken),
            "C1 native execution must record the branch at its bytecode PC"
        );
        for pc in [0, 2, 3, 4, 5, 6, 7] {
            assert_eq!(
                bank.counts(pc),
                (0, 0),
                "only the executed branch PC changes"
            );
        }
    }
}

#[test]
fn native_c1_reference_and_folded_null_branches_record_their_own_pc() {
    for op in [0xa5, 0xa6] {
        let code = [0x2a, 0x2b, op, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
        let bank = Arc::new(crate::profile::BranchCounters::new(9));
        let body = build_with_params(&code, 2, Some(bank.clone()));
        let mut taken = 0;
        for (x, y) in [(0, 0), (0, 2048), (2048, 0), (2048, 2048), (2048, 4096)] {
            let branch = (x == y) == (op == 0xa5);
            assert_eq!(
                unsafe { body.try_call(&[x, y]) },
                Ok(if branch { -1 } else { 1 })
            );
            taken += u32::from(branch);
        }
        assert_eq!(
            bank.counts(2),
            (taken, 5 - taken),
            "reference branch observations"
        );
    }
    for op in [0xc6, 0xc7] {
        for load in [0x2a, 0x01] {
            let code = [load, op, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
            let bank = Arc::new(crate::profile::BranchCounters::new(8));
            let body = build(&code, Some(bank.clone()));
            let mut taken = 0;
            for x in [0, 2048] {
                let branch = ((load == 0x01) || x == 0) == (op == 0xc6);
                assert_eq!(
                    unsafe { body.try_call(&[x]) },
                    Ok(if branch { -1 } else { 1 })
                );
                taken += u32::from(branch);
            }
            assert_eq!(
                bank.counts(1),
                (taken, 2 - taken),
                "including folded null branches"
            );
        }
    }
}

#[test]
fn native_c1_fused_comparisons_record_java_direction_including_nan() {
    let values = [
        f64::NEG_INFINITY,
        -1.5,
        -0.0,
        0.0,
        2.0,
        f64::INFINITY,
        f64::NAN,
    ];
    for cmp in [0x95, 0x96, 0x97, 0x98] {
        let double = cmp >= 0x97;
        let (load0, load1) = if double { (0x26, 0x27) } else { (0x22, 0x23) };
        for op in 0x99..=0x9e {
            let code = [load0, load1, cmp, op, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
            let bank = Arc::new(crate::profile::BranchCounters::new(10));
            let body = build_with_params(&code, 2, Some(bank.clone()));
            let mut taken = 0;
            for a in values {
                for b in values {
                    let (x, y) = if double {
                        (a.to_bits() as i64, b.to_bits() as i64)
                    } else {
                        (
                            i64::from((a as f32).to_bits()),
                            i64::from((b as f32).to_bits()),
                        )
                    };
                    let branch = java_if_r11(op, java_fp_cmp_r11(a, b, cmp == 0x96 || cmp == 0x98));
                    assert_eq!(
                        unsafe { body.try_call(&[x, y]) },
                        Ok(if branch { -1 } else { 1 }),
                        "cmp={cmp:x} if={op:x} a={a} b={b}"
                    );
                    taken += u32::from(branch);
                }
            }
            assert_eq!(
                bank.counts(3),
                (taken, 49 - taken),
                "fused FP branch observations including NaN"
            );
            assert_eq!(
                bank.counts(2),
                (0, 0),
                "the compare PC is not the branch PC"
            );
        }
    }
    for op in 0x99..=0x9e {
        let code = [0x1e, 0x1f, 0x94, op, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
        let bank = Arc::new(crate::profile::BranchCounters::new(10));
        let body = build_with_params(&code, 2, Some(bank.clone()));
        let mut taken = 0;
        for x in [i64::MIN, -1, 0, 1, i64::MAX] {
            for y in [i64::MIN, -1, 0, 1, i64::MAX] {
                let cmp = if x < y {
                    -1
                } else if x > y {
                    1
                } else {
                    0
                };
                let branch = java_if_r11(op, cmp);
                assert_eq!(
                    unsafe { body.try_call(&[x, y]) },
                    Ok(if branch { -1 } else { 1 })
                );
                taken += u32::from(branch);
            }
        }
        assert_eq!(
            bank.counts(3),
            (taken, 25 - taken),
            "fused long branch observations"
        );
    }
}

#[test]
fn native_c1_minmax_records_the_branch_even_when_it_becomes_a_cmov() {
    let code = [0x1a, 0x1b, 0xa2, 0, 7, 0x1a, 0xa7, 0, 4, 0x1b, 0xac, 0, 0];
    let bank = Arc::new(crate::profile::BranchCounters::new(11));
    let body = build_with_params(&code, 2, Some(bank.clone()));
    let mut taken = 0;
    for x in [i32::MIN, -7, 0, 7, i32::MAX] {
        for y in [i32::MIN, -7, 0, 7, i32::MAX] {
            assert_eq!(
                unsafe { body.try_call(&[i64::from(x), i64::from(y)]) },
                Ok(i64::from(x.min(y)))
            );
            taken += u32::from(x >= y);
        }
    }
    assert_eq!(
        bank.counts(2),
        (taken, 25 - taken),
        "CMOV still observes the Java branch"
    );
}

#[test]
fn native_c1_counter_owner_outlives_the_request() {
    let code = [0x1a, 0x99, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
    let bank = Arc::new(crate::profile::BranchCounters::new(8));
    let weak = Arc::downgrade(&bank);
    let body = build(&code, Some(bank));
    assert!(
        weak.upgrade().is_some(),
        "artifact owns the native cell addresses"
    );
    assert_eq!(unsafe { body.try_call(&[0]) }, Ok(-1));
    assert_eq!(weak.upgrade().unwrap().counts(1), (1, 0));
    drop(body);
    assert!(
        weak.upgrade().is_none(),
        "no global owner leaks the counter bank"
    );
}

#[test]
fn native_c1_saturation_has_no_wrapping_window_under_concurrent_writers() {
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
    let code = [0x1a, 0x99, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
    let bank = Arc::new(crate::profile::BranchCounters::new(8));
    for addr in {
        let (t, n) = bank.native_branch_cells(1).unwrap();
        [t, n]
    } {
        unsafe { &*(addr as *const AtomicU32) }.store(u32::MAX - 1, Relaxed);
    }
    let body = Arc::new(build(&code, Some(bank.clone())));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let body = &body;
            let bank = &bank;
            scope.spawn(move || {
                for i in 0..1000 {
                    let x = i & 1;
                    assert_eq!(
                        unsafe { body.try_call(&[x]) },
                        Ok(if x == 0 { -1 } else { 1 })
                    );
                    let (t, n) = bank.counts(1);
                    assert!(
                        t >= u32::MAX - 1 && n >= u32::MAX - 1,
                        "a saturated counter never reads zero"
                    );
                }
            });
        }
    });
    assert_eq!(bank.counts(1), (u32::MAX, u32::MAX));
}

#[test]
fn native_c1_preserves_values_beneath_the_branch_operand() {
    let code = [
        0x1a, 0x59, 0x9b, 0, 6, 0x04, 0x60, 0xac, 0x02, 0x60, 0xac, 0, 0,
    ];
    let bank = Arc::new(crate::profile::BranchCounters::new(11));
    let body = build(&code, Some(bank.clone()));
    for x in [i32::MIN, -7, -1, 0, 1, 7, i32::MAX] {
        let want = if x < 0 {
            x.wrapping_sub(1)
        } else {
            x.wrapping_add(1)
        };
        assert_eq!(
            unsafe { body.try_call(&[i64::from(x)]) },
            Ok(i64::from(want))
        );
    }
    assert_eq!(bank.counts(2), (3, 4));
}

#[test]
fn native_c1_absent_or_out_of_range_bank_leaves_the_same_code() {
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_ENTRY_COUNTER", Some("0"))],
        || {
            let code = [0x1a, 0x99, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
            let plain = build(&code, None);
            let empty = build(
                &code,
                Some(Arc::new(crate::profile::BranchCounters::new(0))),
            );
            assert_eq!(plain._buffer.as_slice(), empty._buffer.as_slice());
            assert_eq!(unsafe { plain.try_call(&[0]) }, Ok(-1));
            assert_eq!(unsafe { empty.try_call(&[1]) }, Ok(1));
        },
    );
}

#[test]
fn native_c1_signed_binary_tests_and_proven_nonnull_edges_record_direction() {
    for op in 0x9f..=0xa4 {
        let code = [0x1a, 0x1b, op, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
        let bank = Arc::new(crate::profile::BranchCounters::new(9));
        let body = build_with_params(&code, 2, Some(bank.clone()));
        let mut taken = 0;
        for x in [i32::MIN, -1, 0, 1, i32::MAX] {
            for y in [i32::MIN, -1, 0, 1, i32::MAX] {
                let cmp = if x < y {
                    -1
                } else if x > y {
                    1
                } else {
                    0
                };
                let branch = java_if_r11(op - 6, cmp);
                assert_eq!(
                    unsafe { body.try_call(&[i64::from(x), i64::from(y)]) },
                    Ok(if branch { -1 } else { 1 })
                );
                taken += u32::from(branch);
            }
        }
        assert_eq!(bank.counts(2), (taken, 25 - taken));
    }
    for op in [0xc6, 0xc7] {
        // The second test's incoming edge has proved local0 nonnull.
        let (fall, taken_value) = if op == 0xc6 {
            (0x04, 0x05)
        } else {
            (0x05, 0x04)
        };
        let code = [
            0x2a,
            0xc6,
            0,
            9,
            0x2a,
            op,
            0,
            7,
            fall,
            0xac,
            0x02,
            0xac,
            taken_value,
            0xac,
            0,
            0,
        ];
        let bank = Arc::new(crate::profile::BranchCounters::new(14));
        let body = build(&code, Some(bank.clone()));
        assert_eq!(unsafe { body.try_call(&[0]) }, Ok(-1));
        assert_eq!(unsafe { body.try_call(&[2048]) }, Ok(1));
        assert_eq!(bank.counts(1), (1, 1));
        assert_eq!(
            bank.counts(5),
            if op == 0xc6 { (0, 1) } else { (1, 0) },
            "the elided nonnull edge remains an observation"
        );
    }
}

#[test]
fn rescued_code_keeps_its_native_branch_cells_until_metadata_is_released() {
    let code = [0x1a, 0x99, 0, 5, 0x04, 0xac, 0x02, 0xac, 0, 0];
    let bank = Arc::new(crate::profile::BranchCounters::new(8));
    let weak = Arc::downgrade(&bank);
    let mut body = build(&code, Some(bank));
    let rescued = crate::RescuedSideTables::take_from(&mut body);
    drop(body);
    assert!(
        weak.upgrade().is_some(),
        "rescued mappings must retain their branch counter bank"
    );
    drop(rescued);
    assert!(
        weak.upgrade().is_none(),
        "the rescue owner releases the bank with its other tables"
    );
}

#[test]
fn native_c1_provenance_and_scope_guards_preserve_the_value_and_frame_floor() {
    let bank = Arc::new(crate::profile::BranchCounters::new(8));
    let mut c = tail11_bare_compiler();
    c.c1_branch_counters = Some(bank.clone());
    c.bci_provenance = Some(vec![0, 7, 1]);
    c.synthetic_guard_span = Some((2, 3));
    // A minimal native frame with the same 48-byte outgoing reserve. This
    // emitter-only fixture has no helper, safepoint or metadata pointer.
    c.buf
        .emit(&[0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x30]);
    let value = 0x1234_5678_abcd_ef42;
    c.emit_mov_imm64(RAX, value);
    c.emit_cmp_r64_r64(RAX, RAX);
    c.emit_c1_branch_profile(1, 0x84);
    c.emit_c1_branch_profile(2, 0x84); // synthetic guard, not a Java observation
    c.inline_scope_stack.push(crate::deopt::FrameState {
        method_key: "Caller.f:()V".to_string(),
        bci: 0,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    });
    c.emit_c1_branch_profile(0, 0x84); // another method's inline scope
    c.buf.emit(&[0x48, 0x89, 0xec, 0x5d, 0xc3]);
    let body = CompiledMethod::try_new(c.buf).expect("native profiling fixture");
    assert_eq!(unsafe { body.try_call(&[]) }, Ok(value));
    assert_eq!(
        bank.counts(7),
        (1, 0),
        "record in original bytecode coordinates"
    );
    for pc in 0..7 {
        assert_eq!(
            bank.counts(pc),
            (0, 0),
            "exclude synthetic and inlined code"
        );
    }
}

#[test]
fn native_c1_back_edges_count_each_executed_direction_without_stack_drift() {
    let code = [
        0x1a, 0x9e, 0, 12, 0x84, 0, 0xff, 0x1a, 0x9d, 0xff, 0xfc, 0x1a, 0xac, 0x03, 0xac, 0, 0,
    ];
    let bank = Arc::new(crate::profile::BranchCounters::new(15));
    let body = build(&code, Some(bank.clone()));
    for n in [0, 1, 5, 17] {
        assert_eq!(unsafe { body.try_call(&[n]) }, Ok(0));
    }
    assert_eq!(bank.counts(1), (1, 3));
    assert_eq!(bank.counts(8), (20, 3));
}

#[test]
fn native_c1_temporary_value_copy_does_not_remain_as_a_conservative_gc_root() {
    let bank = Arc::new(crate::profile::BranchCounters::new(2));
    let mut c = tail11_bare_compiler();
    c.c1_branch_counters = Some(bank.clone());
    c.buf
        .emit(&[0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x30]);
    c.emit_mov_imm64(RAX, 0x1234_5678_abcd_ef40);
    c.emit_cmp_r64_r64(RAX, RAX);
    c.emit_c1_branch_profile(1, 0x84);
    // Return the borrowed outgoing word, which the conservative frame sweep
    // may later visit. A copied reference must not survive the tap there.
    c.buf.emit(&[0x48, 0x8b, 0x04, 0x24]); // MOV RAX,[RSP]
    c.buf.emit(&[0x48, 0x89, 0xec, 0x5d, 0xc3]);
    let body = CompiledMethod::try_new(c.buf).expect("native outgoing-word fixture");
    assert_eq!(
        unsafe { body.try_call(&[]) },
        Ok(0),
        "profiling must erase the temporary reference copy"
    );
    assert_eq!(bank.counts(1), (1, 0));
}
