//! gcd d2/j: the fallback throwable of a VM-raised `OutOfMemoryError`.
use super::*;
use crate::config::VmConfig;
use crate::vm::realms::heap_realm::OomeKind;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: non-null, 8-byte aligned, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}


/// A message with its own default gets it; every other message, and a
/// kind with no default built, gets the singleton; nothing before either
/// exists.
#[test]
fn the_message_picks_its_own_default_else_the_singleton() {
    let shared = SharedVm::new(VmConfig::default());
    let limit = OomeKind::ArraySize.message();
    assert_eq!(preallocated_oome_for(&shared, limit), None);
    let singleton = fake(0x7000);
    *shared.mem.singleton_oom.write() = Some(singleton);
    assert_eq!(preallocated_oome_for(&shared, limit), Some(singleton));
    let own = fake(0x8000);
    shared
        .mem
        .preallocated_oome
        .write()
        .set(OomeKind::ArraySize, own);
    assert_eq!(preallocated_oome_for(&shared, limit), Some(own));
    assert_eq!(
        preallocated_oome_for(&shared, "Java heap space"),
        Some(singleton)
    );
    assert_eq!(preallocated_oome_for(&shared, "Metaspace"), Some(singleton));
    // The fakes must not outlive the test inside a VM a collector could scan.
    *shared.mem.singleton_oom.write() = None;
    *shared.mem.preallocated_oome.write() = Default::default();
}
