use super::*;
use crate::test_utils::MockNativeContext;

const VM_D: usize = 0xF2_5A01;
const VM_E: usize = 0xF2_5A02;

struct Forget;
impl Drop for Forget {
    fn drop(&mut self) {
        forget_vm_signal_handlers(VM_D);
        forget_vm_signal_handlers(VM_E);
    }
}

fn handle0(ctx: &mut MockNativeContext, sig: i32, new_h: i64) -> i64 {
    match native_signal_handle0(ctx, &[Value::Int(sig), Value::Long(new_h)]) {
        Ok(Some(Value::Long(v))) => v,
        other => panic!("handle0 answered {other:?}"),
    }
}

#[test]
fn handle0_answers_the_previous_disposition_per_vm_and_refuses_vm_signals() {
    let _forget = Forget;
    let mut d = MockNativeContext::new();
    d.set_vm_identity(VM_D);
    let mut e = MockNativeContext::new();
    e.set_vm_identity(VM_E);
    // USR1 (10): the first answer is the process's OS disposition, whatever
    // it is; after that the table answers.
    assert_eq!(handle0(&mut d, 10, 2), signal_os_disposition(10));
    assert_eq!(
        handle0(&mut d, 10, 0),
        2,
        "a re-registration sees the Java handler"
    );
    assert_eq!(handle0(&mut d, 10, 2), 0);
    // Another VM's table is its own.
    assert_eq!(handle0(&mut e, 10, 1), signal_os_disposition(10));
    assert_eq!(handle0(&mut d, 10, 2), 2);
    // HotSpot 25 refuses these (`Signal already used by VM or OS`):
    // QUIT, ILL, FPE, SEGV on POSIX; FPE and BREAK on Windows (wave 100).
    let reserved: &[i32] = if cfg!(windows) {
        &[8, 21]
    } else {
        &[3, 4, 8, 11]
    };
    for &sig in reserved {
        assert_eq!(handle0(&mut d, sig, 2), -1, "signal {sig}");
    }
}

#[test]
fn shutdown_ignores_are_sticky_and_xrs_registration_is_per_vm() {
    let mut d = MockNativeContext::new();
    d.set_vm_identity(0xF2_5A03);
    let mut e = MockNativeContext::new();
    e.set_vm_identity(0xF2_5A04);
    e.reduce_signal_usage = true;
    // Windows has no HUP, and its `JVM_RegisterSignal` does not keep an
    // ignore: the second registration replaces it (wave 100, lane L1).
    let shutdown: &[i32] = if cfg!(windows) { &[2, 15] } else { &[1, 2, 15] };
    let after_reinstall = if cfg!(windows) {
        HostSignalDisposition::Java
    } else {
        HostSignalDisposition::Ignore
    };
    for &sig in shutdown {
        handle0(&mut d, sig, 1);
        assert_eq!(handle0(&mut d, sig, 2), 1);
        assert_eq!(host_signal_disposition(&d, sig), after_reinstall);
        assert_eq!(handle0(&mut e, sig, 1), -1);
        assert_eq!(
            host_signal_disposition(&e, sig),
            HostSignalDisposition::Shutdown
        );
    }
    assert_ne!(handle0(&mut e, 10, 2), -1);
    assert_eq!(host_signal_disposition(&e, 10), HostSignalDisposition::Java);
    forget_vm_signal_handlers(0xF2_5A03);
    forget_vm_signal_handlers(0xF2_5A04);
}

#[test]
fn raise0_without_a_java_handler_runs_nothing() {
    let _forget = Forget;
    let mut d = MockNativeContext::new();
    d.set_vm_identity(VM_D);
    // No row, then SIG_IGN: neither is the Java handler, so no `dispatch`
    // is invoked.
    assert!(matches!(
        native_signal_raise0(&mut d, &[Value::Int(14)]),
        Ok(None)
    ));
    handle0(&mut d, 14, 1);
    assert!(matches!(
        native_signal_raise0(&mut d, &[Value::Int(14)]),
        Ok(None)
    ));
}
