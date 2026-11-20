use super::*;

fn target(
    kind: u8,
    entry: Option<(usize, bool)>,
    method: &str,
) -> cratonvm_jit::InlineInvokeTarget {
    cratonvm_jit::InlineInvokeTarget {
        class_name: "java/lang/StringUTF16".to_string(),
        owner_class_id: 0,
        target_class_id: 0,
        method_name: method.to_string(),
        descriptor: "([B[B)I".to_string(),
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: kind,
        declaring_class_id: 1,
        resolved_class_id: 0,
        direct_entry: entry,
    }
}

/// `compare(String,String)`'s four arms: the bound Latin-1 one stays a
/// call, the three that never ran become exits, a nested or already
/// unloaded pc and a callee that has run are left alone.
#[test]
fn only_unbound_never_run_static_rows_become_exits() {
    let targets = vec![
        (34usize, target(3, Some((0x1000, false)), "compareToCI")),
        (43, target(3, None, "compareToCI")),
        (55, target(3, None, "compareToCI_UTF16")),
        (64, target(3, None, "compareToCI_Latin1")),
        (70, target(3, None, "ran")),
        (80, target(0, None, "virtual")),
    ];
    let c = |pc: usize| (pc, ClassId::new(5));
    let candidates = [c(34), c(43), c(55), c(64), c(70), c(80), c(43)];
    let nested = vec![cratonvm_jit::NestedInlineSite {
        callee_pc: 55,
        guard_class_id: 0,
        site: cratonvm_jit::InlineSite::default(),
        bimorphic_second: None,
    }];
    let got =
        never_run_static_exit_pcs(&targets, &nested, &[64], &candidates, |_, m, _| m == "ran");
    assert_eq!(got, vec![43]);
    let got = never_run_static_exit_pcs(&targets, &[], &[], &candidates, |_, m, _| m == "ran");
    assert_eq!(got, vec![43, 55, 64]);
}

