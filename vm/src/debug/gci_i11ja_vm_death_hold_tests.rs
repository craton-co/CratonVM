//! gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
//! item 2): the `VMDeath` hold opens no region of its own on a thread the
//! exit native already made GC-safe.
use super::*;
use crate::threading::jvm_thread::{JvmThread, ThreadId};

#[test]
fn gci_i11ja_vm_death_hold_reads_the_blocked_flag_only() {
    let mut thread = JvmThread::new(ThreadId(3), "dying");
    let ptr = &mut thread as *mut JvmThread;
    assert!(!dying_thread_already_gc_safe(ptr), "a running thread");
    thread
        .gc_block_state
        .in_blocked_region
        .store(true, std::sync::atomic::Ordering::Release);
    let ptr = &mut thread as *mut JvmThread;
    assert!(dying_thread_already_gc_safe(ptr), "inside the exit native's region");
}

/// Source pin: the already-blocked arm comes first and only waits.
#[test]
fn gci_i11ja_vm_death_hold_skips_its_region_when_blocked() {
    let src = include_str!("c_class_hooks_switch.rs").replace("\r\n", "\n");
    let start = src
        .find("\npub(in crate::debug) fn hold_for_vm_death(")
        .expect("hold_for_vm_death");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n}\n").expect("its end")];
    let skip = body
        .find("Some(thread) if dying_thread_already_gc_safe(thread) => wait(),")
        .expect("the blocked arm");
    let region = body.find("begin_blocking_region(&mut ctx)").expect("the own region");
    assert!(skip < region);
}
