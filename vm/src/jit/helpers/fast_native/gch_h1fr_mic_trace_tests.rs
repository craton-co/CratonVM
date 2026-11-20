use super::{mic_door_trace_label, JitInvokeInfo};

/// gch h1/fr: the MIC door's trace names its callee as `[DISP_TRACE]` does.
#[test]
fn gch_h1fr_mic_trace_label_matches_the_dispatch_trace() {
    let info = JitInvokeInfo {
        class_name: "Gcd1ArgPinSpecialProbe$Leaf",
        method_name: "dropVirtual",
        descriptor: "([JZ)Z",
        num_jit_args: 3,
        return_type: b'Z',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    };
    assert_eq!(
        mic_door_trace_label(&info),
        "Gcd1ArgPinSpecialProbe$Leaf.dropVirtual([JZ)Z kind=0"
    );
}
