use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{HandshakeState, JvmThread, ThreadId};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// A frame-trace handshake queued for a thread running compiled code is
/// served at the next helper entry door (no pause requested): it runs
/// once, on that thread, publishes and bumps `root_publish_seq`.
#[test]
fn r23w6_a_helper_entry_serves_the_queued_frame_trace_handshake() {
    if !cratonvm_types::flags().gc.handshake_at_helper_entry {
        return;
    }
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let tid = ThreadId(23_0601);
    let mut thread = Box::new(JvmThread::new(tid, "r23w6-door"));
    shared.threads.thread_registry.register(tid, "r23w6-door", None);
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    let seq = thread.root_publish_seq.load(Ordering::Relaxed);
    let ticket = thread
        .gc_block_state
        .handshakes
        .push(crate::runtime::interpreter::frame_trace_handshake_op());
    assert!(!shared.mem.gc_barrier.stw_requested.load(Ordering::Acquire));
    let scope = set_jit_thread(&mut thread);
    // SAFETY: `vm_ptr` names the live `SharedVm` above and the JIT thread
    // pointer is installed for this OS thread, as compiled code has it.
    unsafe {
        jit_safepoint_flush_satb(vm_ptr);
        // Served once: a second door finds nothing queued.
        jit_safepoint_flush_satb(vm_ptr);
    }
    restore_jit_thread(scope);
    assert_eq!(ticket.state(), HandshakeState::Ran);
    assert!(!thread.gc_block_state.handshakes.is_pending());
    assert_eq!(thread.root_publish_seq.load(Ordering::Relaxed), seq + 1);
}

/// Source pin: the door serves handshakes after the SATB flush, under
/// the switch.
#[test]
fn r23w6_the_satb_door_serves_handshakes_under_the_switch() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = src
        .find("\npub unsafe fn jit_safepoint_flush_satb(")
        .expect("the door");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    let flush = body.find("flush_thread_satb();").expect("the flush");
    let serve = body
        .find("serve_handshakes_at_helper_entry(vm);")
        .expect("the serve");
    assert!(flush < serve);
    let gate_at = src
        .find("\npub unsafe fn serve_handshakes_at_helper_entry(")
        .expect("the server");
    assert!(src[gate_at..].contains("flags().gc.handshake_at_helper_entry"));
}
