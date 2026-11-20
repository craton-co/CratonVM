use super::lambda_int_to_double_runs_proven;

type Names = Option<(String, String, String)>;

fn names(c: &str, m: &str, d: &str) -> Names {
    Some((c.to_string(), m.to_string(), d.to_string()))
}

/// `f.apply(i)` unboxed to `double`: `aload_1; iload_2; invokestatic #10;
/// invokeinterface #11 1 0; checkcast #12; invokevirtual #13; dreturn`.
const RUN: [u8; 17] = [
    0x2b, 0x1c, 0xb8, 0x00, 0x0a, 0xb9, 0x00, 0x0b, 0x01, 0x00, 0xc0, 0x00, 0x0c, 0xb6, 0x00,
    0x0d, 0xaf,
];

fn invoke(idx: u16) -> Names {
    match idx {
        10 => names("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;"),
        11 => names(
            "java/util/function/Function",
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        13 => names("java/lang/Double", "doubleValue", "()D"),
        _ => None,
    }
}

fn class(idx: u16) -> Option<String> {
    (idx == 12).then(|| "java/lang/Double".to_string())
}

#[test]
fn a_run_whose_four_members_the_pool_proves_arms_the_fold() {
    assert!(lambda_int_to_double_runs_proven(
        &RUN,
        RUN.len(),
        &invoke,
        &class
    ));
}

#[test]
fn a_run_naming_another_member_does_not() {
    // Another boxing overload at the first site.
    let other_box = |idx: u16| match idx {
        10 => names(
            "java/lang/Integer",
            "valueOf",
            "(Ljava/lang/String;)Ljava/lang/Integer;",
        ),
        _ => invoke(idx),
    };
    assert!(!lambda_int_to_double_runs_proven(
        &RUN,
        RUN.len(),
        &other_box,
        &class
    ));
    // Another functional interface.
    let other_sam = |idx: u16| match idx {
        11 => names(
            "java/util/function/IntFunction",
            "apply",
            "(I)Ljava/lang/Object;",
        ),
        _ => invoke(idx),
    };
    assert!(!lambda_int_to_double_runs_proven(
        &RUN,
        RUN.len(),
        &other_sam,
        &class
    ));
    // Another cast.
    let other_cast = |idx: u16| (idx == 12).then(|| "java/lang/Number".to_string());
    assert!(!lambda_int_to_double_runs_proven(
        &RUN,
        RUN.len(),
        &invoke,
        &other_cast
    ));
    // A pool that cannot answer.
    assert!(!lambda_int_to_double_runs_proven(
        &RUN,
        RUN.len(),
        &|_| None,
        &class
    ));
}

#[test]
fn one_unproven_run_disarms_the_whole_method() {
    // The run twice (the first without its `dreturn`, its double popped
    // with `pop2`), the second naming `apply` through pool entry 14.
    let mut code = RUN[..16].to_vec();
    code.push(0x58);
    let second = [
        0x2b, 0x1c, 0xb8, 0x00, 0x0a, 0xb9, 0x00, 0x0e, 0x01, 0x00, 0xc0, 0x00, 0x0c, 0xb6,
        0x00, 0x0d, 0xaf,
    ];
    code.extend_from_slice(&second);
    assert!(!lambda_int_to_double_runs_proven(
        &code,
        code.len(),
        &invoke,
        &class
    ));
    let with_14 = |idx: u16| if idx == 14 { invoke(11) } else { invoke(idx) };
    assert!(lambda_int_to_double_runs_proven(
        &code,
        code.len(),
        &with_14,
        &class
    ));
}

#[test]
fn a_method_with_no_run_is_not_armed_and_operand_bytes_are_not_opcodes() {
    assert!(!lambda_int_to_double_runs_proven(
        &[0xb1],
        1,
        &invoke,
        &class
    ));
    // `bipush 0xb8; pop; nop; invokeinterface #11; checkcast #12;
    // invokevirtual #13; return`: read byte by byte, pc 1 would be an
    // `invokestatic #0x5700` heading the shape. It is `bipush`'s operand.
    let code = [
        0x10, 0xb8, 0x57, 0x00, 0xb9, 0x00, 0x0b, 0x01, 0x00, 0xc0, 0x00, 0x0c, 0xb6, 0x00,
        0x0d, 0xb1,
    ];
    let lenient = |idx: u16| {
        if idx == 0x5700 {
            invoke(10)
        } else {
            invoke(idx)
        }
    };
    assert!(!lambda_int_to_double_runs_proven(
        &code,
        code.len(),
        &lenient,
        &class
    ));
}

/// No jit-crate source names the TDigest class in a string literal any
/// more: the fold, the splice stand-down and the allocation-sink screen
/// all ask `lambda_int_to_double_proven`.
#[test]
fn the_jit_crate_names_no_application_class_for_the_fold() {
    let needle = concat!("\"org/elasticsearch/", "tdigest/");
    for src in [
        include_str!("lib.rs"),
        concat!(
            include_str!("ir/mod.rs"),
            "\n",
            include_str!("ir/node_id.rs"),
            "\n",
            include_str!("ir/safepoints.rs"),
            "\n",
            include_str!("ir/loop_phis.rs"),
            "\n",
            include_str!("ir/set_osr_requested_header.rs"),
            "\n",
            include_str!("ir/splice_op_cannot_raise.rs"),
            "\n",
            include_str!("ir/narrow_int_return.rs"),
            "\n",
            include_str!("ir/is_jdk_throwable_init_candidate.rs"),
            "\n",
            include_str!("ir/attach_sb_virtual_builders.rs"),
            "\n",
            include_str!("ir/field_load_memo_mode.rs"),
            "\n",
            include_str!("ir/ir_splice_store_fence_forward_branch.rs"),
            "\n",
            include_str!("ir/ir_trap_census.rs")
        ),
        concat!(
            include_str!("x64/op_invoke/mod.rs"),
            "\n",
            include_str!("x64/op_invoke/sp_tailcall_keeps_vm_helper_frame.rs"),
            "\n",
            include_str!("x64/op_invoke/sp_byte_array_view_body.rs"),
            "\n",
            include_str!("x64/op_invoke/walk_invoke_instance.rs"),
            "\n",
            include_str!("x64/op_invoke/arraylist_access_prefix.rs"),
            "\n",
            include_str!("x64/op_invoke/indy_site_is_bridged.rs"),
            "\n",
            include_str!("x64/op_invoke/sb_to_string_latin1_body.rs")
        ),
        include_str!("x64/op_object.rs"),
        concat!(
            include_str!("x64/inlining.rs"),
            "\n",
            include_str!("x64/inlining/spliced_sites.rs")
        ),
    ] {
        assert!(!src.contains(needle));
    }
}
