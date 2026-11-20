use super::{compile_gate::CompileDoor, ir_self_lock_candidate, CachedBytecodeMethod};
use std::sync::Arc;

/// `static synchronized int min(int a, int b)`: iload_0; iload_1;
/// if_icmpge +5; iload_0; ireturn; iload_1; ireturn.
const MIN: [u8; 9] = [0x1a, 0x1b, 0xa2, 0x00, 0x05, 0x1a, 0xac, 0x1b, 0xac];

fn static_min() -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("R15w3StaticMin"),
        method_name: Arc::from("min"),
        method_descriptor: Arc::from("(II)I"),
        source_file: None,
        code: Arc::from(MIN.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 2,
        num_params: 2,
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

fn candidate(edits: &[(&str, Option<&str>)], mirror_slot: usize, door: CompileDoor) -> bool {
    let cached = static_min();
    let verdicts = crate::JitVerdictRegistry::default();
    cratonvm_types::flags::with_thread_overrides(edits, || {
        ir_self_lock_candidate(&verdicts, &cached, &MIN, MIN.len(), door, mirror_slot)
    })
}

// Round 15 wave 3 (lane sync2, S15-3): a `static synchronized` method is
// an optimizing self-lock candidate only under both IR switches, with its
// class's mirror slot, on a door other than OSR.

