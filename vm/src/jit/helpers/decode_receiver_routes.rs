use super::*;
use crate::config::VmConfig;
use std::sync::Arc;

pub static PROBE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "p/Probe",
    method_name: "m",
    descriptor: "()I",
    num_jit_args: 1,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

#[test]
pub fn a_resolved_receiver_decodes_to_what_the_raw_word_decodes_to() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let raw = obj.as_ptr() as i64;

    let mut resolved = JitDecodedArgs::new();
    let mut unresolved = JitDecodedArgs::new();
    // SAFETY: `[raw]` holds a live heap address from `alloc_object`.
    unsafe {
        decode_dispatch_values_into(
            &shared,
            &PROBE_INFO,
            &[raw],
            Some(Some(obj)),
            &mut resolved,
        );
        decode_dispatch_values_into(&shared, &PROBE_INFO, &[raw], None, &mut unresolved);
    }
    assert_eq!(resolved.len(), 1);
    assert_eq!(&resolved[..], &unresolved[..]);
    assert!(matches!(resolved[0], Value::Object(Some(o)) if o.as_ptr() == obj.as_ptr()));
}

/// The buffer is reused across calls, so a longer previous call must not
/// leave arguments behind for a shorter one.
#[test]
pub fn a_reused_buffer_does_not_leak_the_previous_calls_arguments() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    pub static TWO_INT_INFO: JitInvokeInfo = JitInvokeInfo {
        class_name: "p/Probe",
        method_name: "m",
        descriptor: "(II)I",
        num_jit_args: 3,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    };
    let mut buf = JitDecodedArgs::new();
    // SAFETY: slot 0 is a live heap address; the rest are plain ints.
    unsafe {
        decode_dispatch_values_into(
            &shared,
            &TWO_INT_INFO,
            &[obj.as_ptr() as i64, 7, 9],
            Some(Some(obj)),
            &mut buf,
        );
        assert_eq!(buf.len(), 3);
        decode_dispatch_values_into(
            &shared,
            &PROBE_INFO,
            &[obj.as_ptr() as i64],
            Some(Some(obj)),
            &mut buf,
        );
    }
    assert_eq!(
        buf.len(),
        1,
        "the reused buffer still holds the previous call's arguments"
    );
}
