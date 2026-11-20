use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::vm::Vm;

/// Poll `cond` every millisecond for up to `secs` seconds.
fn eventually(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    cond()
}

fn generational_vm(service: bool) -> Vm {
    let cfg = VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    };
    let edit: &[(&str, Option<&str>)] = if service {
        &[("CRATONVM_GEN_CONC_SERVICE_THREAD", Some("1"))]
    } else {
        &[("CRATONVM_GEN_CONC_SERVICE_THREAD", None)]
    };
    cratonvm_types::flags::with_thread_overrides(edit, || Vm::new(cfg))
}

/// gen r4w6/concsvc6 — with the flag, `Vm::new` starts the service: it
/// attaches to the heap's concurrent state, idles GC-blocked (so a pause
/// requested now does not wait for it), and a mutator's due cycle is
/// handed to it; `Drop for Vm` stops it, and it detaches and is marked
/// dead in the registry.
#[test]
fn the_service_thread_starts_with_a_generational_vm_and_stops_with_it() {
    let vm = generational_vm(true);
    let shared = Arc::clone(&vm.shared);
    let state = Arc::clone(&shared.mem.concurrent_gc_state);
    assert!(
        eventually(30, || state.service_attached()),
        "the service thread attached"
    );
    assert!(!state.on_service_thread(), "the test thread is a mutator");
    let line = state.driver_census_line();
    assert!(line.contains(" concdrv_service_attached=true "), "{line}");
    // A second service on the same VM is refused and exits cleanly (the
    // not-attached exit path).
    let second = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_GEN_CONC_SERVICE_THREAD", Some("1"))],
        || spawn_gen_concurrent_service_thread(&shared),
    )
    .expect("a second thread is spawned");
    assert!(
        eventually(30, || !shared.threads.thread_registry.is_alive(second)),
        "the refused service marked itself dead"
    );
    assert!(state.service_attached(), "the first service is unaffected");

    drop(vm);
    assert!(
        eventually(30, || !state.service_attached()),
        "shutdown detached the service"
    );
    assert!(
        state.attach_service_thread().is_none(),
        "the shutdown is permanent"
    );
}

/// gcd d1/c (`gengc-r5w5-conc9-the-service-thread-can-outlive-vm-teardown`):
/// the teardown stop waits, inside a blocked region, until the service
/// has detached, and leaves the dropping thread a counted mutator again.
/// Without a service it returns at once.
#[test]
fn gcd_d1c_the_teardown_stop_waits_for_the_service_to_detach() {
    let mut vm = generational_vm(true);
    let state = Arc::clone(&vm.shared.mem.concurrent_gc_state);
    assert!(
        eventually(30, || state.service_attached()),
        "the service thread attached"
    );
    let shared = Arc::clone(&vm.shared);
    let started = std::time::Instant::now();
    stop_gen_concurrent_service_thread(&shared, &mut vm.main_thread);
    assert!(state.service_shutting_down());
    assert!(
        !state.service_attached() || started.elapsed() >= SERVICE_STOP_WAIT,
        "returned before the service detached and before the bound"
    );
    assert!(
        !vm.main_thread
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire),
        "the dropping thread left the blocked region"
    );
    drop(shared);
    drop(vm);

    let plain = generational_vm(false);
    let shared = Arc::clone(&plain.shared);
    let mut plain = plain;
    stop_gen_concurrent_service_thread(&shared, &mut plain.main_thread);
    assert!(!shared.mem.concurrent_gc_state.service_attached());
}

/// Without the flag (the default) no service is started and every path
/// stays inline.
#[test]
fn no_service_thread_without_the_flag() {
    let vm = generational_vm(false);
    assert!(!start_gen_concurrent_service_thread(&vm.shared));
    assert!(!vm.shared.mem.concurrent_gc_state.service_attached());
    assert!(vm
        .shared
        .mem
        .concurrent_gc_state
        .driver_census_line()
        .contains(" concdrv_service_attached=false "));
}
