/// The gate over the no-VM registry, which no test here puts a cycle in.
fn admitted(
    helpers: &crate::JitRuntimeHelpers,
    direct_helpers: &crate::DirectHelperTable,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    ret: u8,
) -> bool {
    super::single_pass_sync_direct_site_admitted(
        crate::process_jit_verdicts(),
        helpers,
        direct_helpers,
        class_name,
        method_name,
        descriptor,
        ret,
    )
}

fn tables(
    ldc_class_cp: usize,
    dispatch_threw: usize,
    enter: usize,
    exit: usize,
) -> (crate::JitRuntimeHelpers, crate::DirectHelperTable) {
    let mut helpers = crate::JitRuntimeHelpers::default();
    helpers.ldc_class_cp = ldc_class_cp;
    helpers.dispatch_threw = dispatch_threw;
    let direct = crate::DirectHelperTable {
        monitor_enter: enter,
        monitor_exit: exit,
        ..crate::DirectHelperTable::EMPTY
    };
    (helpers, direct)
}

/// Each helper the backend arm emits is required: without it the site
/// keeps the dispatch helper, whatever the flags say.
#[test]
fn a_missing_helper_refuses_the_site() {
    for (ldc, enter, exit) in [(0, 3, 4), (1, 0, 4), (1, 3, 0)] {
        let (h, d) = tables(ldc, 2, enter, exit);
        assert!(!admitted(&h, &d, "r11w14/SpsyncGate", "inc", "()V", b'V'));
    }
    // A wide result needs the `dispatch_threw` peek.
    let (h, d) = tables(1, 0, 3, 4);
    assert!(!admitted(&h, &d, "r11w14/SpsyncGate", "get", "()J", b'J'));
    assert!(!admitted(&h, &d, "r11w14/SpsyncGate", "bad", "()X", b'X'));
}

/// With every helper wired, the site is admitted exactly when the three
/// flags are on (their defaults).
#[test]
fn a_fully_wired_site_is_admitted_under_the_default_flags() {
    let on = crate::direct_jit_callee_calls_enabled()
        && super::sync_direct_enabled()
        && crate::x64::merged_call_sentinel_enabled();
    let (h, d) = tables(1, 2, 3, 4);
    assert_eq!(
        admitted(&h, &d, "r11w14/SpsyncGate", "inc", "()V", b'V'),
        on
    );
    assert_eq!(
        admitted(&h, &d, "r11w14/SpsyncGate", "get", "()J", b'J'),
        on
    );
    assert_eq!(
        admitted(
            &h,
            &d,
            "r11w14/SpsyncGate",
            "obj",
            "()Ljava/lang/Object;",
            b'L'
        ),
        on
    );
}
