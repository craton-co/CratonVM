use super::*;

fn takeover_signal_blocked() -> bool {
    // SAFETY: a query of this thread's mask into a zeroed set.
    unsafe {
        let mut now: libc::sigset_t = std::mem::zeroed();
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut now);
        libc::sigismember(&now, TAKEOVER_SIGNAL) == 1
    }
}

#[test]
fn the_mask_guards_nest_and_restore() {
    assert!(!takeover_signal_blocked(), "a test thread starts unmasked");
    {
        let _off = TakeoverSignalMask::block(false);
        assert!(!takeover_signal_blocked(), "the switch off changes nothing");
    }
    {
        let _call = TakeoverSignalMask::block(true);
        assert!(takeover_signal_blocked());
        {
            let _upcall = TakeoverSignalMask::unblock(true);
            assert!(!takeover_signal_blocked());
        }
        assert!(takeover_signal_blocked(), "C resumes masked");
    }
    assert!(
        !takeover_signal_blocked(),
        "the call's guard restores the mask"
    );
}
