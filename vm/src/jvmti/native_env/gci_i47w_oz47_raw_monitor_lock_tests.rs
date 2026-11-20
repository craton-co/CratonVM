//! gci i47w lane oz47 (`vi12-ev.list` T, every row a `[stw-stall]` hang
//! on the i13 and i47z experimental-debug builds): a raw monitor's inner
//! lock is re-taken by a woken waiter inside its GC-safe region and held
//! while that region's end waits for a pause, so a contended entry must
//! itself be GC-safe ([`super::lock_raw_monitor_state`]).

/// Source pin: the helper tries the lock first and blocks GC-safe only
/// when it is contended.
#[test]
fn gci_i47w_oz47_contended_raw_monitor_lock_is_taken_gc_safe() {
    let src = concat!(
        include_str!("../native_env/mod.rs"),
        "\n",
        include_str!("../native_env/has_cap.rs"),
        "\n",
        include_str!("../native_env/jit_chain_len.rs"),
        "\n",
        include_str!("../native_env/read_capabilities.rs")
    )
    .replace("\r\n", "\n");
    let start = src
        .find("fn lock_raw_monitor_state<")
        .expect("lock_raw_monitor_state");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    let fast = body.find("raw.state.try_lock()").expect("the fast path");
    let slow = body
        .find("raw_monitor_block(env, || raw.state.lock())")
        .expect("the GC-safe slow path");
    assert!(fast < slow);
}

/// Source pin: every raw monitor function takes the inner lock through
/// the helper; no plain `raw.state.lock()` is left outside it.
#[test]
fn gci_i47w_oz47_every_raw_monitor_function_uses_the_helper() {
    let src = concat!(
        include_str!("../native_env/mod.rs"),
        "\n",
        include_str!("../native_env/has_cap.rs"),
        "\n",
        include_str!("../native_env/jit_chain_len.rs"),
        "\n",
        include_str!("../native_env/read_capabilities.rs")
    )
    .replace("\r\n", "\n");
    let helper = src
        .find("fn lock_raw_monitor_state<")
        .expect("lock_raw_monitor_state");
    let plain = concat!("raw.state", ".lock();");
    assert!(!src.contains(plain), "a plain lock outside the helper");
    for f in [
        "extern \"C\" fn destroy_raw_monitor(",
        "extern \"C\" fn raw_monitor_enter(",
        "extern \"C\" fn raw_monitor_exit(",
        "extern \"C\" fn raw_monitor_wait(",
        "fn raw_monitor_notify(",
    ] {
        let at = src.find(f).expect(f);
        assert!(at > helper, "{f}");
        let rest = &src[at..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(body.contains("lock_raw_monitor_state(env, raw)"), "{f}");
    }
}
