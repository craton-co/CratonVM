use super::*;
use crate::ThreadId;

/// Build w4c regression: a publish made while `raise_jit_stack_overflow`
/// has the floor suspended (TLS `0`) must not copy the `0` into the
/// thread's floor word -- nothing would re-arm it afterwards, and every
/// compiled inline-cache check of the thread would pass forever.
#[test]
fn a_publish_under_a_suspended_floor_keeps_the_armed_word() {
    let ok = std::thread::spawn(|| {
        let mut thread = Box::new(JvmThread::new(ThreadId(13), "r13w4-mega3-floor-word"));
        JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(0x1000_0000));
        let scope = set_jit_thread(&mut thread);
        restore_jit_thread(scope);
        assert_eq!(thread.jit_stack_floor, 0x1000_0000, "armed by the publish");
        {
            let _suspended = SuspendedStackFloor::enter();
            // The error's constructor re-enters compiled code.
            let scope = set_jit_thread(&mut thread);
            restore_jit_thread(scope);
        }
        assert_eq!(
            thread.jit_stack_floor, 0x1000_0000,
            "a suspended publish disarmed the floor word"
        );
    })
    .join()
    .is_ok();
    assert!(ok);
}
