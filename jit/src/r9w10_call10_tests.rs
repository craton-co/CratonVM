use super::*;

fn method(is_static: bool, is_synchronized: bool, name: &str) -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(0x0010_2001),
        class_name: Arc::from("r9w10/P"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from("(I)I"),
        source_file: None,
        code: Arc::from(&[0xac_u8, 0x00, 0x00][..]),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 2,
        num_params: 1,
        is_synchronized,
        is_static,
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

