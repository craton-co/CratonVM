use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const PATTERN_KEY: &str = "java.util.logging.FileHandler.pattern";

/// Forgets the listed VMs' logging rows however the test ends.
struct ForgetVms(&'static [usize]);
impl Drop for ForgetVms {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_logging_state(vm);
        }
    }
}

/// Removes a scratch file however the test ends.
struct RemoveFile(std::path::PathBuf);
impl Drop for RemoveFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn get_property(ctx: &mut crate::test_utils::MockNativeContext, key: &str) -> Option<String> {
    let key = ctx.create_string(key);
    match native_get_property(ctx, &[Value::Object(None), Value::Object(Some(key))]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

#[test]
fn read_configuration_in_one_vm_leaves_another_vms_properties_alone() {
    const VM_A: usize = 0xB13_0001;
    const VM_B: usize = 0xB13_0002;
    let _forget = ForgetVms(&[VM_A, VM_B]);
    let mut a = mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = mock_ctx();
    b.set_vm_identity(VM_B);

    apply_jul_config_entries(&mut a, &[(PATTERN_KEY.to_string(), "a.log".to_string())])
        .unwrap();
    // VM B's configuration names no `FileHandler`. Its `readConfiguration`
    // used to clear the one process-wide table, VM A's pattern with it.
    apply_jul_config_entries(&mut b, &[("w13b.other".to_string(), "b".to_string())]).unwrap();

    assert_eq!(
        parsed_log_properties(VM_A)
            .lock()
            .unwrap()
            .get(PATTERN_KEY)
            .map(String::as_str),
        Some("a.log"),
        "VM B's readConfiguration must not clear VM A's FileHandler.pattern"
    );
    assert_eq!(get_property(&mut a, PATTERN_KEY).as_deref(), Some("a.log"));
    assert_eq!(
        get_property(&mut a, "w13b.other"),
        None,
        "A must not see B's keys"
    );
    assert_eq!(
        get_property(&mut b, PATTERN_KEY),
        None,
        "B must not see A's keys"
    );
    assert_eq!(get_property(&mut b, "w13b.other").as_deref(), Some("b"));
}

#[test]
fn every_vm_reads_its_own_config_file() {
    const VM_A: usize = 0xB13_0011;
    const VM_B: usize = 0xB13_0012;
    let _forget = ForgetVms(&[VM_A, VM_B]);
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let file_a = RemoveFile(dir.join(format!("cratonvm-w13b-{pid}-a.properties")));
    let file_b = RemoveFile(dir.join(format!("cratonvm-w13b-{pid}-b.properties")));
    std::fs::write(&file_a.0, b"w13b.file.key=from-a\n").unwrap();
    std::fs::write(&file_b.0, b"w13b.file.key=from-b\n").unwrap();

    let mut a = mock_ctx();
    a.set_vm_identity(VM_A);
    a.set_system_property(
        "java.util.logging.config.file",
        file_a.0.to_str().expect("utf-8 temp path"),
    );
    let mut b = mock_ctx();
    b.set_vm_identity(VM_B);
    b.set_system_property(
        "java.util.logging.config.file",
        file_b.0.to_str().expect("utf-8 temp path"),
    );

    assert_eq!(
        get_property(&mut a, "w13b.file.key").as_deref(),
        Some("from-a")
    );
    // The latch was one process-wide `OnceLock`: after VM A's read, VM B
    // never read its own file and answered null.
    assert_eq!(
        get_property(&mut b, "w13b.file.key").as_deref(),
        Some("from-b")
    );
    assert!(*config_file_properties_loaded(VM_A).lock().unwrap());
    assert!(*config_file_properties_loaded(VM_B).lock().unwrap());
}

#[test]
fn a_log_record_burst_in_one_vm_keeps_another_vms_payloads() {
    const VM_A: usize = 0xB13_0021;
    const VM_B: usize = 0xB13_0022;
    let _forget = ForgetVms(&[VM_A, VM_B]);
    let kept = next_log_record_id();
    remember_log_record_message(VM_A, kept, "in flight in A".to_string());
    for _ in 0..=LOG_RECORD_MESSAGES_CAP {
        remember_log_record_message(VM_B, next_log_record_id(), String::new());
    }
    assert_eq!(
        log_record_messages(VM_A)
            .lock()
            .unwrap()
            .get(&kept)
            .map(String::as_str),
        Some("in flight in A"),
        "VM B's burst must not evict VM A's payload"
    );
    assert_eq!(
        log_record_messages(VM_B).lock().unwrap().len(),
        LOG_RECORD_MESSAGES_CAP
    );
}

#[test]
fn teardown_forgets_one_vms_logging_rows_in_both_modules() {
    const VM_A: usize = 0xB13_0031;
    const VM_B: usize = 0xB13_0032;
    let _forget = ForgetVms(&[VM_A, VM_B]);
    let mut a = mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = mock_ctx();
    b.set_vm_identity(VM_B);

    let handler_a = a.fresh_object_ref();
    let handler_b = b.fresh_object_ref();
    for (ctx, vm, handler, text) in [
        (&mut a, VM_A, handler_a, "a.log"),
        (&mut b, VM_B, handler_b, "b.log"),
    ] {
        parsed_log_properties(vm)
            .lock()
            .unwrap()
            .insert(PATTERN_KEY.to_string(), text.to_string());
        logger_explicit_levels(vm)
            .lock()
            .unwrap()
            .insert("w13b.logger".to_string(), 900);
        crate::jul_file_handler_set_filename(&mut *ctx, handler, Some(text.to_string()));
        assert_eq!(
            crate::jul_file_handler_filename(&mut *ctx, handler).as_deref(),
            Some(text)
        );
    }

    forget_vm_logging_state(VM_A);

    assert!(parsed_log_properties(VM_A).lock().unwrap().is_empty());
    assert!(logger_explicit_levels(VM_A).lock().unwrap().is_empty());
    assert_eq!(
        crate::jul_file_handler_filename(&mut a, handler_a),
        None,
        "the logging_shims tables are forgotten too (their slots register the same hook)"
    );
    assert_eq!(
        parsed_log_properties(VM_B)
            .lock()
            .unwrap()
            .get(PATTERN_KEY)
            .map(String::as_str),
        Some("b.log"),
        "another VM's rows survive"
    );
    assert_eq!(
        logger_explicit_levels(VM_B)
            .lock()
            .unwrap()
            .get("w13b.logger"),
        Some(&900)
    );
    assert_eq!(
        crate::jul_file_handler_filename(&mut b, handler_b).as_deref(),
        Some("b.log")
    );
    // Idempotent.
    forget_vm_logging_state(VM_A);
}
