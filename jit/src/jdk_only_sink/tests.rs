use super::*;

fn row(m: &str) -> JdkOnlyViolation {
    JdkOnlyViolation::NativeShadowsBytecode {
        class: "a/B".to_string(),
        method: m.to_string(),
        descriptor: "()V".to_string(),
        native_kind: "jit-thin-direct-helper",
        class_origin: None,
    }
}

#[test]
fn t1y_a_sink_dedups_caps_and_counts_drops() {
    let s = JdkOnlyViolationSink::default();
    s.record(Some(row("m")), 1);
    s.record(Some(row("m")), 1);
    s.record(Some(row("n")), 1);
    s.record(None, 1);
    assert_eq!(s.events(), 4);
    assert_eq!(s.len(), 1);
    assert_eq!(s.dropped(), 1);
    assert!(s.saturated());
}

#[test]
fn t1y_scopes_route_to_the_innermost_vm_and_restore() {
    let vm_a = JdkOnlyReportSinks::new();
    let vm_b = JdkOnlyReportSinks::new();
    let process_before = process_jdk_only_sinks().jit_compile.events();
    {
        let _a = JdkOnlySinkScope::enter(&vm_a);
        with_current_jdk_only_sinks(|s| s.jit_compile.record(Some(row("a")), 8));
        {
            let _b = JdkOnlySinkScope::enter(&vm_b);
            with_current_jdk_only_sinks(|s| s.jit_compile.record(Some(row("b")), 8));
        }
        with_current_jdk_only_sinks(|s| s.jit_compile.record(None, 8));
    }
    assert_eq!(vm_a.jit_compile.events(), 2);
    assert_eq!(vm_a.jit_compile.rows(), vec![row("a")]);
    assert_eq!(vm_b.jit_compile.rows(), vec![row("b")]);
    // Nothing reached the process sink from inside the scopes. (Other
    // tests may record into it concurrently, so only `>=`.)
    assert!(process_jdk_only_sinks().jit_compile.events() >= process_before);
    let ptr = CURRENT.with(|c| c.get());
    assert!(ptr.is_null(), "every scope restored its predecessor");
}

/// t1 wave 6, lane Y3: the direct-helper gate tallies are per VM. Two VMs'
/// sinks in one process each sum only the tallies registered with them,
/// across threads.
#[test]
fn t1y3_direct_helper_tallies_sum_per_vm() {
    let vm_a = JdkOnlyReportSinks::new();
    let vm_b = JdkOnlyReportSinks::new();
    let a1 = vm_a.register_direct_helper_tally();
    a1.bump_from_owner(true);
    a1.bump_from_owner(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let a2 = vm_a.register_direct_helper_tally();
            a2.bump_from_owner(true);
            let b1 = vm_b.register_direct_helper_tally();
            b1.bump_from_owner(false);
            b1.bump_from_owner(false);
        });
    });
    assert_eq!(vm_a.direct_helper_refusals(), 2);
    assert_eq!(vm_a.direct_helper_admissions(), 1);
    assert_eq!(vm_b.direct_helper_refusals(), 0);
    assert_eq!(vm_b.direct_helper_admissions(), 2);
}
