use super::*;

fn info() -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "p/C",
        method_name: "callee",
        descriptor: "(Ljava/lang/Object;)V",
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

/// The caller's re-run after a hand-off of a FRAMELESS callee trap is as
/// exact as the callee's replay: the trapping stub's own stamp, only when
/// it names this callee; a dropped frame's verdict; nothing else.
#[test]
fn gce_e1f_a_frameless_handoff_is_judged_by_the_callees_own_stamp() {
    let i = info();
    let own = frameless_trap_method_hash(i.class_name, i.method_name, i.descriptor);
    let stub = |exact, method| FramelessTrapSite::Stub {
        compile_id: 9,
        bci: 4,
        exact,
        method,
    };
    assert!(owned_args_frameless_callee_replay_is_exact(
        stub(true, own),
        &i
    ));
    assert!(!owned_args_frameless_callee_replay_is_exact(
        stub(false, own),
        &i
    ));
    assert!(
        !owned_args_frameless_callee_replay_is_exact(stub(true, own ^ 1), &i),
        "another method's stamp says nothing about this callee"
    );
    assert!(
        !owned_args_frameless_callee_replay_is_exact(stub(true, 0), &i),
        "an unparsed label is not trusted here"
    );
    assert!(owned_args_frameless_callee_replay_is_exact(
        FramelessTrapSite::Dropped { exact: true },
        &i
    ));
    assert!(!owned_args_frameless_callee_replay_is_exact(
        FramelessTrapSite::Dropped { exact: false },
        &i
    ));
    for site in [
        FramelessTrapSite::Helper,
        FramelessTrapSite::Unknown,
        FramelessTrapSite::Handoff { exact: true },
    ] {
        assert!(!owned_args_frameless_callee_replay_is_exact(site, &i));
    }
}

/// The record replaces the `Helper` the re-raise wrote, and the next
/// raise replaces it in turn.
#[test]
fn gce_e1f_the_handoff_record_follows_the_reraise() {
    set_jit_deopt_pending();
    note_owned_args_handoff(true);
    let on = true;
    let expect = if on {
        FramelessTrapSite::Handoff { exact: true }
    } else {
        FramelessTrapSite::Helper
    };
    assert_eq!(last_frameless_trap_site(), expect);
    set_jit_deopt_pending();
    assert_eq!(last_frameless_trap_site(), FramelessTrapSite::Helper);
    let _ = take_jit_deopt_pending();
}
