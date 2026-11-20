use super::*;

/// JDK 25 `Pattern.matcher(CharSequence)`:
/// `if (!compiled) { synchronized (this) { if (!compiled) compile(); } }
/// return new Matcher(this, input);` as javac emits it.
const MATCHER: [u8; 44] = [
    0x2a, // 0: aload_0
    0xb4, 0x00, 0x01, // 1: getfield compiled
    0x9a, 0x00, 0x1c, // 4: ifne -> 32
    0x2a, // 7: aload_0
    0x59, // 8: dup
    0x4d, // 9: astore_2
    0xc2, // 10: monitorenter
    0x2a, // 11: aload_0
    0xb4, 0x00, 0x01, // 12: getfield compiled
    0x9a, 0x00, 0x07, // 15: ifne -> 22
    0x2a, // 18: aload_0
    0xb6, 0x00, 0x02, // 19: invokevirtual compile
    0x2c, // 22: aload_2
    0xc3, // 23: monitorexit
    0xa7, 0x00, 0x08, // 24: goto -> 32
    0x4e, // 27: astore_3
    0x2c, // 28: aload_2
    0xc3, // 29: monitorexit
    0x2d, // 30: aload_3
    0xbf, // 31: athrow
    0xbb, 0x00, 0x03, // 32: new Matcher
    0x59, // 35: dup
    0x2a, // 36: aload_0
    0x2b, // 37: aload_1
    0xb7, 0x00, 0x04, // 38: invokespecial Matcher.<init>
    0x4d, // 41: astore_2
    0x2c, // 42: aload_2
    0xb0, // 43: areturn
];
/// Its table: `[11, 24) -> 27 any`, `[27, 30) -> 27 any`.
const MATCHER_ROWS: [(usize, usize, usize); 2] = [(11, 24, 27), (27, 30, 27)];

fn profile(taken: u32, not_taken: u32) -> crate::profile::MethodProfile {
    let mut p = crate::profile::MethodProfile::default();
    p.branches
        .insert(4, crate::profile::BranchCounts { taken, not_taken });
    p
}


#[test]
fn rx27_pattern_matcher_masks_its_whole_compile_guard() {
    let (masked, arm) =
        cold_handler_arm_mask(&MATCHER, 44, &MATCHER_ROWS, 2, 4).expect("an arm");
    assert_eq!(
        arm,
        ColdHandlerArm {
            branch_pc: 4,
            start: 7,
            end: 32
        }
    );
    assert_eq!(masked.len(), MATCHER.len(), "same length");
    assert_eq!(masked[..7], MATCHER[..7], "the guard's test is untouched");
    assert_eq!(masked[32..], MATCHER[32..], "the hot path is untouched");
    assert_eq!(masked[7], 0x01, "aconst_null");
    assert!(masked[8..31].iter().all(|&b| b == 0x00), "nops");
    assert_eq!(masked[31], 0xbf, "athrow");
    assert!(
        !masked.iter().any(|&b| b == 0xc2 || b == 0xc3),
        "no monitor instruction is left for any walk to see"
    );
    // The resolver's cold-throw pass takes it as one arm, so the builder
    // traps its branch.
    assert_eq!(
        cold_throw_arms(&masked, 44),
        Some(vec![ColdThrowArm {
            branch_pc: 4,
            start: 7,
            athrow_pc: 31
        }])
    );
}

#[test]
fn rx27_the_arm_is_cold_only_on_a_profile_that_never_fell_into_it() {
    let (_, arm) = cold_handler_arm_mask(&MATCHER, 44, &MATCHER_ROWS, 2, 4).expect("an arm");
    assert!(splice_cold_handler_arm_unexecuted(
        &arm,
        44,
        &profile(64, 0)
    ));
    assert!(
        !splice_cold_handler_arm_unexecuted(&arm, 44, &profile(64, 1)),
        "a pattern whose first matcher() compiled it ran the arm"
    );
    assert!(
        !splice_cold_handler_arm_unexecuted(&arm, 44, &profile(3, 0)),
        "too few samples prove nothing"
    );
    assert!(
        !splice_cold_handler_arm_unexecuted(
            &arm,
            44,
            &crate::profile::MethodProfile::default()
        ),
        "no observation proves nothing"
    );
}

#[test]
fn rx27_a_row_outside_the_arm_or_a_jump_into_it_refuses() {
    let mut rows = MATCHER_ROWS.to_vec();
    rows.push((32, 41, 27));
    assert!(
        cold_handler_arm_mask(&MATCHER, 44, &rows, 2, 4).is_err(),
        "a row on the hot path"
    );
    // The outer branch at 4 retargeted to 11, inside the locked region:
    // no branch's fall-through arm holds every row any more.
    let mut entered = MATCHER;
    entered[5..7].copy_from_slice(&7i16.to_be_bytes()); // 4: ifne -> 11
    assert!(cold_handler_arm_mask(&entered, 44, &MATCHER_ROWS, 2, 4).is_err());
}

/// The hot path loops after the guard: a loop root keeps its call.
#[test]
fn rx27_a_loop_callee_refuses() {
    let mut looping = MATCHER[..41].to_vec();
    looping.extend_from_slice(&[0x57, 0xa7, 0xff, 0xf6]); // 41: pop; 42: goto -> 32
    let err = cold_handler_arm_mask(&looping, 45, &MATCHER_ROWS, 2, 4).unwrap_err();
    assert!(err.contains("loop"), "{err}");
}

/// `static int f(int a, boolean c) { int x = a; if (c) { try { g(); }
/// catch (Throwable t) { return x; } } return 0; }`: only the handler
/// reads `x`, so with the rows gone `x` is dead at the branch and a deopt
/// there would resume the handler without it.
#[test]
fn rx27_a_handler_reading_a_local_from_before_the_branch_refuses() {
    let code: [u8; 17] = [
        0x1a, // 0: iload_0
        0x3d, // 1: istore_2
        0x1b, // 2: iload_1
        0x99, 0x00, 0x0c, // 3: ifeq -> 15
        0xb8, 0x00, 0x01, // 6: invokestatic g
        0xa7, 0x00, 0x06, // 9: goto -> 15
        0x4e, // 12: astore_3
        0x1c, // 13: iload_2
        0xac, // 14: ireturn
        0x03, // 15: iconst_0
        0xac, // 16: ireturn
    ];
    let err = cold_handler_arm_mask(&code, 17, &[(6, 9, 12)], 2, 4).unwrap_err();
    assert!(err.contains("handler reads a local"), "{err}");
    // The same body whose handler does not read `x` is admitted.
    let mut quiet = code;
    quiet[13] = 0x03; // iconst_0
    assert!(cold_handler_arm_mask(&quiet, 17, &[(6, 9, 12)], 2, 4).is_ok());
}

/// Caller `aload_0; iload_1; invokestatic A(Ljava/lang/Object;I)I;
/// ireturn` (pcs 0..6), and `A` at 6 as the resolver hands it over after
/// [`cold_handler_arm_mask`]: originally `if (c == 0) { synchronized (o)
/// {} } return 1;`.
fn spliced_build(overrides: &[(&str, Option<&str>)]) -> Option<Graph> {
    const A: [u8; 20] = [
        0x1b, // 0: iload_1
        0x9a, 0x00, 0x11, // 1: ifne -> 18
        0x2a, // 4: aload_0
        0x59, // 5: dup
        0x4d, // 6: astore_2
        0xc2, // 7: monitorenter
        0x2c, // 8: aload_2
        0xc3, // 9: monitorexit
        0xa7, 0x00, 0x08, // 10: goto -> 18
        0x4e, // 13: astore_3
        0x2c, // 14: aload_2
        0xc3, // 15: monitorexit
        0x2d, // 16: aload_3
        0xbf, // 17: athrow
        0x04, // 18: iconst_1
        0xac, // 19: ireturn
    ];
    let (masked, arm) =
        cold_handler_arm_mask(&A, 20, &[(8, 10, 13), (13, 16, 13)], 2, 4).expect("an arm");
    assert_eq!((arm.branch_pc, arm.start, arm.end), (1, 4, 18));
    let mut combined: Vec<u8> = vec![0x2a, 0x1b, 0xb8, 0x00, 0x01, 0xac];
    combined.extend_from_slice(&masked);
    let a = IrInlineSite {
        base: 6,
        code_len: 20,
        num_args: 2,
        max_locals: 4,
        arg_local_slots: vec![0, 1],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: "T.a:(Ljava/lang/Object;I)I".to_string(),
        class_id: 0,
    };
    cratonvm_types::flags::with_thread_overrides(overrides, move || {
        let mut b = IrBuilder::new(2, 2);
        b.set_param_types(&[IrType::Ref, IrType::Int]);
        b.set_inline_sites(HashMap::from([(2usize, a)]));
        b.set_spliced_bodies_pure(true);
        b.graph.scope_method_key = "T.outer:(Ljava/lang/Object;I)I".to_string();
        b.build(&combined, 6)
    })
}

