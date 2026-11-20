//! gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
//! item 1): an agent callback keeps a pointer the poster installed from its
//! own live `&mut`, and gets the registry's address only when none (or
//! another thread's) is installed.
use super::*;

#[test]
fn gci_i11ja_event_context_keeps_the_posters_pointer() {
    // Installed and published at the same address: kept, nothing replaced.
    assert_eq!(event_thread_to_install(Some(0x1000), Some(0x1000)), None);
    // Nothing installed: the registry's address, as before.
    assert_eq!(event_thread_to_install(Some(0x1000), None), Some(0x1000));
    // Another thread's pointer installed: replaced by this thread's.
    assert_eq!(
        event_thread_to_install(Some(0x1000), Some(0x2000)),
        Some(0x1000)
    );
    // This thread not published: install nothing (the poster's, if any, stays).
    assert_eq!(event_thread_to_install(None, Some(0x1000)), None);
    assert_eq!(event_thread_to_install(None, None), None);
}

/// Source pin: `in_event_context` decides through the helper with the raw
/// installed address, and restores only what it replaced.
#[test]
fn gci_i11ja_in_event_context_asks_for_the_installed_address() {
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
    let start = src.find("fn in_event_context(").expect("in_event_context");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n}\n").expect("its end")];
    let ask = body.find("event_thread_to_install(").expect("the decision");
    assert!(body[ask..].contains("jni::installed_jni_thread_addr()"));
    let call = body
        .find("call(jni::get_jni_env(), thread)")
        .expect("the callback");
    let restore = body
        .find("jni::restore_jni_thread(prev)")
        .expect("the restore");
    assert!(ask < call && call < restore);
    assert!(
        body.contains("if let Some(prev) = prev_thread"),
        "restore only what was replaced"
    );
}
