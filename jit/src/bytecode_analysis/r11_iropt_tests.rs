use super::*;

/// `try { … goto C } catch (E e) { B } C:` with `B` running into `C`: the
/// target has a second ordinary predecessor that only an exception
/// reaches, so the `goto` is a join and must stay a barrier.
#[test]
fn a_handler_falling_into_the_goto_target_is_a_join() {
    // 0: nop  1: goto -> 7  4: astore_1 (handler)  5: nop  6: nop  7: return
    let mut code = vec![0x00, 0xa7, 0x00, 0x06, 0x4c, 0x00, 0x00, 0xb1];
    let s = straight_line_gotos(&code, code.len(), &[4]);
    assert!(!s[1], "the handler body runs into pc 7");
    // MUST ACCEPT twin: the handler ends in `athrow`, so only the goto
    // reaches pc 7.
    code[5] = 0x2b; // aload_1
    code[6] = 0xbf; // athrow
    let s = straight_line_gotos(&code, code.len(), &[4]);
    assert!(s[1], "nothing but the goto reaches pc 7");
}
#[test]
fn cyclic_pcs_keep_loop_calls_and_exception_retry_paths() {
    // Pre-loop call at 0, a counted loop at 5..14, tail call at 17.
    let mut code = vec![
        0xb8, 0, 1, 0x03, 0x3b, 0x1a, 0x10, 10, 0xa2, 0, 9, 0x84, 0, 1, 0xa7, 0xff, 0xf7, 0xb8,
        0, 2, 0xb1,
    ];
    let cfg = InsnCfg::build(&code, code.len()).unwrap();
    let repeated = cfg.cyclic_pcs();
    assert!(!repeated.contains(&0));
    assert!(!repeated.contains(&17));
    for pc in [5, 6, 8, 11, 14] {
        assert!(repeated.contains(&pc), "loop pc {pc}");
    }
    // A catch that retries from entry also repeats the pre-loop call.
    code.extend([0x4c, 0xa7, 0xff, 0xea]);
    let cfg = InsnCfg::build_with_handlers(&code, code.len(), &[(5, 17, 21)]).unwrap();
    assert!(cfg.cyclic_pcs().contains(&0));
    assert!(!cfg.cyclic_pcs().contains(&17));
}

#[test]
fn cyclic_pcs_keep_self_loops_and_unreachable_sites_conservative() {
    let code = [0xa7, 0, 0, 0xb8, 0, 1, 0xb1];
    let cfg = InsnCfg::build(&code, code.len()).unwrap();
    let repeated = cfg.cyclic_pcs();
    assert!(repeated.contains(&0));
    assert!(repeated.contains(&3));
}
