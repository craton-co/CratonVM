use super::*;

/// A thread that is not blocked, or blocked with no deferred record (its
/// deposit walked the chain eagerly, or it is a nested deposit), names
/// nothing here: its roots are in its snapshot already.
#[test]
fn no_record_or_no_window_names_no_dump_root() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
    let state = crate::threading::jvm_thread::GcBlockState::new();
    assert!(deferred_blocked_jit_chain_roots(&heap, &state).is_empty());
    state
        .in_blocked_region
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(state.deferred_jit_chain.lock().is_none());
    assert!(deferred_blocked_jit_chain_roots(&heap, &state).is_empty());
}

/// The dump's root segment asks for those roots, inside its pause only, and
/// names each with its own thread's serial.
#[test]
fn the_heap_dump_names_the_deferred_chain_roots() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime/hprof.rs");
    let src = std::fs::read_to_string(&path)
        .expect("hprof.rs")
        // A CRLF checkout (core.autocrlf) must not hide the "\n    }\n" end.
        .replace("\r\n", "\n");
    let at = src
        .find("fn deferred_blocked_chain_roots(&self)")
        .expect("the dump's reader");
    let reader = &src[at..at + src[at..].find("\n    }\n").expect("reader end")];
    assert!(reader.contains("if !self.in_pause"));
    assert!(reader.contains("deferred_blocked_jit_chain_roots("));
    assert!(src.contains("for &(tid, obj) in &self.deferred_blocked_chain_roots() {"));
}
