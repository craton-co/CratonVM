//! zs s9/la: HotSpot's preallocated traced pool on ZGC
//! (`CRATONVM_ZGC_OOME_TRACED_POOL`). The first four VM-raised `Java heap
//! space` errors ARE the four objects preallocated at boot, then the
//! traceless singleton; nothing is constructed on the refusing heap.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::vm::Vm;

fn zgc_vm() -> Option<Vm> {
    let vm = Vm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    });
    crate::vm::heap_is_zgc(&vm.shared).then_some(vm)    }

fn raise(vm: &mut Vm) -> ObjectRef {
    create_vm_oome_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/OutOfMemoryError",
        Some("Java heap space"),
    )
    .expect("an OutOfMemoryError")
}

#[test]
fn the_first_four_heap_errors_are_the_boot_pool_then_the_default() {
    let Some(mut vm) = zgc_vm() else {
        return;
    };
    ensure_singleton_oom(&vm.shared, &mut vm.main_thread);
    let pool: Vec<Option<ObjectRef>> = (0..HOTSPOT_PREALLOCATED_OOME_COUNT as usize)
        .map(|i| vm.shared.mem.preallocated_oome.read().traced(i))
        .collect();
    if pool.iter().any(Option::is_none) {
        eprintln!("zs9la: the pool could not be built here (no JDK); skipped");
        return;
    }
    for (i, want) in pool.iter().enumerate() {
        let got = raise(&mut vm);
        assert_eq!(Some(got), *want, "slot {i}: the preallocated error");
    }
    let fifth = raise(&mut vm);
    assert_eq!(
        Some(fifth),
        *vm.shared.mem.singleton_oom.read(),
        "then the default"
    );
    // addr-keyed: test-local identity check, no allocation between collect and use.
    let distinct: std::collections::HashSet<usize> =
        pool.iter().flatten().map(|o| o.as_ptr() as usize).collect();
    assert_eq!(distinct.len(), pool.len(), "four distinct errors");
    eprintln!("zs9la: pool served {} errors, then the default", pool.len());
}


/// Generational: no pool (unchanged).
#[test]
fn other_collectors_build_no_pool() {
    let mut vm = Vm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    ensure_singleton_oom(&vm.shared, &mut vm.main_thread);
    assert_eq!(vm.shared.mem.preallocated_oome.read().traced_len(), 0);
}
