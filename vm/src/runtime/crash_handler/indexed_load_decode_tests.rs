use super::decode_indexed_load;

/// The real thing: `hs_err_pid3512`'s own registers and fault address.
/// If this stops matching, the decoder has stopped being able to fire on
/// the crash it was written for.
#[test]
fn decodes_the_json_xml_segv_jump_table_load() {
    let regs = [
        ("rax", 0x0000_0000_EAF8_2DA0u64),
        ("rcx", 0x0000_008D_769D_2C88),
        ("rbx", 0x5B),
        ("rbp", 6),
        ("r10", 0x0000_7FF6_35D7_9014),
        ("r11", 0),
    ];
    let line = decode_indexed_load(&regs, 0x0000_7FF9_E1B8_4694).expect("must decode");
    assert!(line.contains("[r10+rax*4]"), "got: {line}");
}

#[test]
fn declines_when_nothing_explains_the_address() {
    let regs = [("rax", 1u64), ("rbx", 2), ("rcx", 3)];
    assert!(decode_indexed_load(&regs, 0xDEAD_BEEF).is_none());
}

/// A zero index would make every base "explain" the address.
/// `current_thread_name` is called from panic hooks and signal handlers,
/// so it must stay callable on a thread whose thread-local data has
/// already been destroyed. `std::thread::current()` is not — it panics
/// there, and a panic from a TLS destructor aborts the whole process, so
/// a regression in this helper fails this test unmissably rather than
/// subtly.
///
/// The ordering matters and is set up deliberately: destructors registered
/// through `__cxa_thread_atexit_impl` run in reverse registration order, so
/// the probe touches its own `thread_local!` FIRST and `std::thread::
/// current()` SECOND. That makes std's `CURRENT` handle the first thing
/// torn down and the probe's `Drop` the last — i.e. the probe runs in
/// exactly the state that produced `thread/current.rs:315:9`.
#[test]
fn current_thread_name_survives_tls_teardown() {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::Mutex;

    static RAN: AtomicBool = AtomicBool::new(false);
    static SEEN: Mutex<Option<String>> = Mutex::new(None);

    struct Probe;
    impl Drop for Probe {
        fn drop(&mut self) {
            let name = super::current_thread_name();
            *SEEN.lock().unwrap() = name;
            RAN.store(true, AtomicOrdering::SeqCst);
        }
    }
    thread_local! {
        static PROBE: Probe = const { Probe };
    }

    std::thread::Builder::new()
        // <= 15 bytes: Linux stores only TASK_COMM_LEN - 1, and this test
        // asserts on the exact string.
        .name("tlsprobe".to_string())
        .spawn(|| {
            PROBE.with(|_| ());
            let _ = std::thread::current().name().map(String::from);
        })
        .unwrap()
        .join()
        .unwrap();

    assert!(
        RAN.load(AtomicOrdering::SeqCst),
        "the TLS destructor never ran"
    );
    #[cfg(any(unix, windows))]
    assert_eq!(
        SEEN.lock().unwrap().as_deref(),
        Some("tlsprobe"),
        "the OS thread name should still be readable after TLS teardown"
    );
}

#[test]
fn zero_index_and_zero_base_are_not_explanations() {
    let regs = [("rax", 0u64), ("r10", 0x1000)];
    assert!(decode_indexed_load(&regs, 0x1000).is_none());
}
