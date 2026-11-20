use super::{self_lock_admits_method, CachedBytecodeMethod};
use std::sync::Arc;

/// A `static synchronized void scan(int[] a)` whose loop reads and drops
/// every `a[i]`: iconst_0; istore_1; L: iload_1; aload_0; arraylength;
/// if_icmpge END; aload_0; iload_1; iaload; pop; iinc 1 1; goto L; END:
/// return.
const SCAN: [u8; 19] = [
    0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x0d, 0x2a, 0x1b, 0x2e, 0x57, 0x84, 0x01, 0x01,
    0xa7, 0xff, 0xf3, 0xb1,
];

fn static_scan() -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("R13w13StaticScan"),
        method_name: Arc::from("scan"),
        method_descriptor: Arc::from("([I)V"),
        source_file: None,
        code: Arc::from(SCAN.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 3,
        num_params: 1,
        is_synchronized: true,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    }
}

fn admitted_with(edits: &[(&str, Option<&str>)], mirror_slot: usize) -> bool {
    let cached = static_scan();
    cratonvm_types::flags::with_thread_overrides(edits, || {
        self_lock_admits_method(&cached, &SCAN, SCAN.len(), mirror_slot)
    })
}

// Round 13 wave 13 (lane syncres, proposal S7-2): a `static synchronized`
// method whose loop indexes an array is admitted exactly when its bounds
// guards can hand the class mirror's hold over -- both hand-over switches
// on -- and the static feature itself is on with a mirror slot.
