use super::{
    note_self_lock_refused, static_self_lock_recompile_would_self_lock, CachedBytecodeMethod,
};
use std::sync::Arc;

/// `static synchronized int min(int a, int b)`, padded as a cached
/// method's code is (`frame::padded_bytecode`'s two trailing bytes).
const MIN_PADDED: [u8; 11] = [
    0x1a, 0x1b, 0xa2, 0x00, 0x05, 0x1a, 0xac, 0x1b, 0xac, 0x00, 0x00,
];

fn min(is_static: bool) -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(3),
        class_name: Arc::from("R15w4Sync3Min"),
        method_name: Arc::from("min"),
        method_descriptor: Arc::from("(II)I"),
        source_file: None,
        code: Arc::from(MIN_PADDED.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 3,
        num_params: 2,
        is_synchronized: true,
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

/// Round 15 wave 4 (lane sync3, SY2-2): the VM asks for a recompile of a
/// wrapped `static synchronized` body only when the recompile would be
/// offered a self-locking one: a static method, its class's mirror slot,
/// the static self-lock switched on, and no self-lock refusal memoized.
fn would(
    verdicts: &crate::JitVerdictRegistry,
    edits: &[(&str, Option<&str>)],
    m: &CachedBytecodeMethod,
    slot: usize,
) -> bool {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        static_self_lock_recompile_would_self_lock(verdicts, m, slot)
    })
}

