//! gc-common round 2026-09-23, wave 8, lane G: `-XX:±DisableExplicitGC` and
//! `-XX:±ExplicitGCInvokesConcurrent` at the `System.gc()` door
//! (`common-w7g-explicit-gc-flags-are-silently-ignored`). The end-to-end
//! check (JMX counts, a weak reference) is `ExplicitGcFlagsProbe.java`.

use super::*;
use crate::config::{ExplicitGcPolicy, GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

fn vm(gc_algorithm: GcAlgorithm, explicit_gc: ExplicitGcPolicy) -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm,
        explicit_gc,
        ..VmConfig::default()
    })
}

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

/// Same helper as `w7g_door_tests::body`.
fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// The whole rule: `Disabled` ignores the request; `Concurrent` collects
/// as `Full` does (see `explicit_gc_action`'s doc for why).
#[test]
fn the_explicit_gc_rule_per_policy() {
    assert_eq!(
        explicit_gc_action(ExplicitGcPolicy::Full),
        ExplicitGcAction::Collect
    );
    assert_eq!(
        explicit_gc_action(ExplicitGcPolicy::Disabled),
        ExplicitGcAction::Ignore
    );
    assert_eq!(
        explicit_gc_action(ExplicitGcPolicy::Concurrent),
        ExplicitGcAction::Collect
    );
}

/// `-XX:+DisableExplicitGC`: `System.gc()` runs no collection and counts
/// nothing, while the metaspace refusal's collection (not an explicit GC)
/// still collects on the same VM.
#[test]
fn a_disabled_explicit_gc_collects_nothing_but_the_metadata_door_still_does() {
    let shared = vm(GcAlgorithm::Generational, ExplicitGcPolicy::Disabled);
    let mut thread = JvmThread::new(ThreadId(0), "w8g-disabled");
    let sys0 = system_gc_collections(&shared).load(Ordering::Relaxed);
    let cycles0 = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    for _ in 0..3 {
        force_gc_from_native(&shared, &mut thread);
    }
    assert_eq!(system_gc_collections(&shared).load(Ordering::Relaxed), sys0);
    assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), cycles0);
    force_gc_for_class_metadata(&shared, &mut thread);
    assert_eq!(
        system_gc_collections(&shared).load(Ordering::Relaxed),
        sys0 + 1
    );
    assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > cycles0);
}

/// The policy is per VM: a `Disabled` VM beside a default one does not
/// disable the default one's `System.gc()`.
#[test]
fn the_policy_is_per_vm() {
    let off = vm(GcAlgorithm::Generational, ExplicitGcPolicy::Disabled);
    let on = vm(GcAlgorithm::Generational, ExplicitGcPolicy::Full);
    let mut off_thread = JvmThread::new(ThreadId(0), "w8g-per-vm-off");
    let mut on_thread = JvmThread::new(ThreadId(0), "w8g-per-vm-on");
    let off0 = system_gc_collections(&off).load(Ordering::Relaxed);
    let on0 = system_gc_collections(&on).load(Ordering::Relaxed);
    force_gc_from_native(&off, &mut off_thread);
    force_gc_from_native(&on, &mut on_thread);
    assert_eq!(system_gc_collections(&off).load(Ordering::Relaxed), off0);
    assert_eq!(system_gc_collections(&on).load(Ordering::Relaxed), on0 + 1);
}

/// `-XX:+ExplicitGCInvokesConcurrent` collects as the default does.
#[test]
fn concurrent_collects_as_full_does() {
    let shared = vm(GcAlgorithm::Generational, ExplicitGcPolicy::Concurrent);
    let mut thread = JvmThread::new(ThreadId(0), "w8g-concurrent-gen");
    let sys0 = system_gc_collections(&shared).load(Ordering::Relaxed);
    force_gc_from_native(&shared, &mut thread);
    assert_eq!(
        system_gc_collections(&shared).load(Ordering::Relaxed),
        sys0 + 1
    );
}

/// The door's shape: the policy is read per VM, `Disabled` returns before
/// any drain and before `jdk.SystemGC`, and the metaspace door never reads
/// the policy at all.
#[test]
fn disabled_returns_before_any_drain_and_the_metadata_door_ignores_the_policy() {
    let src = source();
    // `System.gc()` passes the VM's own policy into the shared door; the
    // VM-internal door (`force_gc_for_vm`) passes `Full` and is not
    // explicit (handoff-w8g-vm-internal-collection-uses-the-explicit-door).
    // gci i23/pk: through the `jdk.SystemGC` stack-carrying entry (`0`
    // = no stack); the VM-internal door carries none.
    let entry = body(&src, "pub fn force_gc_from_native(");
    assert!(entry.contains("force_gc_from_native_with_jfr_stack(shared, thread, 0)"));
    let with_stack = body(&src, "pub fn force_gc_from_native_with_jfr_stack(");
    assert!(with_stack
        .contains("full_gc_door(shared, thread, shared.config.explicit_gc, true, jfr_stack)"));
    let internal = body(&src, "pub fn force_gc_for_vm(");
    assert!(internal.contains("ExplicitGcPolicy::Full, false, 0)"));
    let door = body(&src, "fn full_gc_door(");
    let dispatch = door
        .find("explicit_gc_action(policy)")
        .expect("the door reads the per-VM policy");
    let ignore = door
        .find("ExplicitGcAction::Ignore => return")
        .expect("ignore arm");
    let first_drain = door.find("run_finalizers_forced(").expect("drains");
    assert!(
        dispatch < ignore && ignore < first_drain,
        "Disabled returns before any drain"
    );
    // `jdk.SystemGC` is committed only for a request that is not ignored.
    let event = door
        .find("emit_system_gc_jfr_event(")
        .expect("jdk.SystemGC");
    assert!(door[..event].contains("if explicit && action != ExplicitGcAction::Ignore"));
    let meta = body(&src, "pub fn force_gc_for_class_metadata(");
    assert!(!meta.contains("explicit_gc"));
}

/// Under `Disabled` on G1 the door starts no marking cycle either.
#[test]
fn a_disabled_explicit_gc_on_g1_opens_no_cycle() {
    let shared = vm(GcAlgorithm::G1, ExplicitGcPolicy::Disabled);
    let mut thread = JvmThread::new(ThreadId(0), "w8g-disabled-g1");
    let cycles0 = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    force_gc_from_native(&shared, &mut thread);
    assert!(!shared.mem.heap.g1_is_marking_active());
    assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), cycles0);
}
