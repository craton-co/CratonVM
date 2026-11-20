use super::*;

/// Suspended while held (nested holds included), restored exactly once by
/// the outermost, and an unset floor is left unset.
#[test]
fn the_floor_is_suspended_while_building_and_restored_after() {
    let ok = std::thread::spawn(|| {
        let get = || JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get());
        {
            let _unset = SuspendedStackFloor::enter();
            assert_eq!(get(), usize::MAX, "an unset floor stays unset");
        }
        JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(0x1000_0000));
        {
            let _outer = SuspendedStackFloor::enter();
            assert_eq!(get(), 0);
            {
                let _inner = SuspendedStackFloor::enter();
                assert_eq!(get(), 0);
            }
            assert_eq!(get(), 0, "the nested hold does not restore early");
        }
        assert_eq!(get(), 0x1000_0000);
    })
    .join()
    .is_ok();
    assert!(ok);
}
