    use super::*;

    /// This thread's own stack, from a live local up: complete, starting
    /// exactly at the probe, and ending at the stack's high limit.
    #[test]
    fn span_of_a_live_stack_is_complete_and_reaches_the_stack_top() {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize);
        }
        let probe: u64 = 0x5eed;
        let rsp = (&probe as *const u64 as usize) & !7;
        let s = unsafe { readable_stack_span(rsp, MAX_TAKEOVER_STACK_SCAN) };
        assert!(s.complete, "a live stack band must be reported whole");
        assert_eq!(s.lo, rsp);
        let (mut low, mut high) = (0usize, 0usize);
        unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
        assert_eq!(
            s.hi, high,
            "the band must end at the stack's own top, not at a region boundary"
        );
        assert_eq!(std::hint::black_box(probe), 0x5eed);
    }

    /// A cap below the band reports INCOMPLETE -- the shape a helper window
    /// deeper than `MAX_STACK_SCAN` has, which must not be pinned.
    #[test]
    fn span_cut_by_its_cap_is_incomplete() {
        let probe: u64 = 1;
        let rsp = (&probe as *const u64 as usize) & !7;
        let s = unsafe { readable_stack_span(rsp, 8) };
        assert!(!s.complete);
        assert_eq!(s.lo, rsp);
        assert_eq!(s.hi, rsp + 8);
        assert_eq!(std::hint::black_box(probe), 1);
    }

    /// `Rsp` in the guard page below the committed stack: the band starts at
    /// the stack proper and is complete. The old `committed_region_end`
    /// answered "nothing readable" here and a frozen peer's whole machine
    /// stack went unscanned.
    #[test]
    fn span_from_the_guard_page_steps_over_it() {
        let probe: u64 = 2;
        let rsp = (&probe as *const u64 as usize) & !7;
        let committed = unsafe { query(rsp) }.expect("VirtualQuery of a live local");
        let committed_lo = committed.base_address as usize;
        let below = committed_lo - 8;
        let Some(guard) = (unsafe { query(below) }) else {
            return;
        };
        if guard.protect & PAGE_GUARD == 0 {
            // Not the shape under test on this thread (no guard page right
            // below the committed stack); nothing to assert.
            return;
        }
        let s = unsafe { readable_stack_span(below, MAX_TAKEOVER_STACK_SCAN) };
        assert!(s.complete);
        assert_eq!(s.lo, committed_lo, "the guard page itself is skipped");
        assert!(s.hi > rsp);
        assert_eq!(std::hint::black_box(probe), 2);
    }
