//! gci i16/sn (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
//! finding 6, `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`): the C
//! functions behind `live_phase_vm` stop a suspended caller in native at
//! their entry, as the JNIEnv functions do (`jni::ForeignJniEntry`; the
//! behaviour is `jni.rs`'s
//! `gci_i16sn_a_suspended_thread_in_native_stops_at_its_next_jni_entry`).

/// Source pin: the hold is the prologue's first statement, before the env
/// is read.
#[test]
fn gci_i16sn_live_phase_vm_holds_a_suspended_caller_first() {
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
    let start = src.find("fn live_phase_vm(").expect("live_phase_vm");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    let hold = body
        .find("jni::hold_in_native_while_suspended();")
        .expect("the hold");
    let env = body.find("let env = live_env(env)?;").expect("the env");
    assert!(hold < env, "held before anything is read");
}
