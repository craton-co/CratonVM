    use super::*;



    /// r9w2-vm2: the parked-window COPY takes the register file and the
    /// whole readable band `[rsp, end)` into the caller's buffer, and the
    /// post-release CLASSIFY finds a JIT return address in the copy and an
    /// object in a register exactly as the old in-place classifier did.
    /// (No band word is reported as an object here: that would publish a
    /// stack-slot remap for this test's own stack into the process-global
    /// capture, which a concurrently running collection could act on.)
    #[test]
    fn parked_snapshot_copies_then_classifies_off_the_copy() {
        let stack: [u64; 6] = [0, 0x7000_0123, 0xdead_beef, 42, 0, 7];
        let rsp = stack.as_ptr() as usize;
        let regions = [(rsp, rsp + stack.len() * 8)];
        let slot = LinuxSlot::new();
        slot.tid.store(4242, Ordering::Release);
        slot.rsp.store(rsp, Ordering::Release);
        let obj_reg = 0x10_0008usize;
        slot.regs[3].store(obj_reg, Ordering::Release);

        let mut band = vec![0u64; 64];
        let snap = snapshot_parked_slot(&slot, &regions, &mut band);
        assert!(snap.band_complete);
        assert_eq!(snap.tid, 4242);
        assert_eq!(snap.rsp, rsp);
        assert_eq!(snap.band_words, stack.len());
        assert_eq!(&band[..stack.len()], &stack[..]);
        assert_eq!(snap.regs[3], obj_reg);

        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        let is_obj = |a: usize| {
            if a == obj_reg {
                // SAFETY: non-null, 8-aligned; never dereferenced.
                Some(unsafe { ObjectRef::from_raw(a as *mut u8) })
            } else {
                None
            }
        };
        let mut candidates = Vec::new();
        let (has_jit, complete, _) =
            classify_parked_snapshot(&snap, &band, &ranges, &is_obj, &mut candidates, &[]);
        assert!(
            has_jit,
            "the JIT return address in the copied band classifies"
        );
        assert!(complete);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].as_ptr() as usize, obj_reg);
    }

    /// A band too small for the stack is INCOMPLETE. The completeness flag
    /// used to be set unconditionally, so a stack deeper than the band was
    /// pinned as whole and could license a discharge with its outer frames
    /// unread (gc-common w1-c, 2026-09-23).
    #[test]
    fn parked_snapshot_that_fills_its_band_is_incomplete() {
        let stack: [u64; 6] = [1, 2, 3, 4, 5, 6];
        let rsp = stack.as_ptr() as usize;
        let regions = [(rsp, rsp + stack.len() * 8)];
        let slot = LinuxSlot::new();
        slot.tid.store(4243, Ordering::Release);
        slot.rsp.store(rsp, Ordering::Release);
        let mut band = vec![0u64; 4];
        let snap = snapshot_parked_slot(&slot, &regions, &mut band);
        assert_eq!(snap.band_words, 4);
        assert!(
            !snap.band_complete,
            "a band cut short by its buffer is not whole"
        );
        assert_eq!(std::hint::black_box(stack[5]), 6);
    }

    /// The take-over and helper caps are reported, not silently applied.
    #[test]
    fn region_end_says_when_the_cap_set_it() {
        let regions = [(0x1000usize, 0x9000usize)];
        assert_eq!(
            readable_region_end_from_regions(0x1000, &regions, 0x10_0000),
            Some((0x9000, false))
        );
        assert_eq!(
            readable_region_end_from_regions(0x1000, &regions, 0x1000),
            Some((0x2000, true))
        );
        assert_eq!(
            readable_region_end_from_regions(0x9000, &regions, 0x1000),
            None
        );
    }

    /// An `rsp` no readable region contains yields an INCOMPLETE snapshot:
    /// registers are still classified, the window is never pinned.
    #[test]
    fn parked_snapshot_without_a_region_is_incomplete() {
        let slot = LinuxSlot::new();
        slot.rsp.store(0x1000, Ordering::Release);
        let mut band = vec![0u64; 8];
        let snap = snapshot_parked_slot(&slot, &[], &mut band);
        assert!(!snap.band_complete);
        assert_eq!(snap.band_words, 0);
        let mut candidates = Vec::new();
        let (has_jit, complete, _) =
            classify_parked_snapshot(&snap, &band, &[], &|_| None, &mut candidates, &[]);
        assert!(!has_jit);
        assert!(!complete);
    }

    /// gcd d9/c: an INCOMPLETE band is still classified. A JIT return
    /// address among its copied words makes the peer a window, unpinned
    /// (`complete` false), where it used to be no window at all. (No word
    /// is reported as an object, for the reason the first test gives.)
    #[test]
    fn an_incomplete_band_still_classifies_its_copied_words() {
        let stack: [u64; 6] = [1, 0x7000_0123, 3, 4, 5, 6];
        let rsp = stack.as_ptr() as usize;
        let regions = [(rsp, rsp + stack.len() * 8)];
        let slot = LinuxSlot::new();
        slot.tid.store(4244, Ordering::Release);
        slot.rsp.store(rsp, Ordering::Release);
        let mut band = vec![0u64; 4];
        let snap = snapshot_parked_slot(&slot, &regions, &mut band);
        assert!(
            !snap.band_complete,
            "a band cut short by its buffer is not whole"
        );
        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        let mut candidates = Vec::new();
        let (has_jit, complete, _) =
            classify_parked_snapshot(&snap, &band, &ranges, &|_| None, &mut candidates, &[]);
        assert!(
            has_jit,
            "the JIT return address in the copied words classifies"
        );
        assert!(!complete, "and the window stays unpinned");
        assert!(candidates.is_empty());
        assert_eq!(std::hint::black_box(stack[5]), 6);
    }

    /// gcd d9/c: a band that stops at the peer's PUBLISHED stack top is its
    /// whole stack, even when the readable region containing `rsp` runs
    /// past `rsp + MAX_STACK_SCAN` (a maps line that merged the stack with
    /// a readable neighbour). `clamped` used to be judged against the
    /// region's end alone, so such a band read incomplete on every pause.
    #[test]
    fn a_band_ending_at_the_published_top_is_whole_past_a_long_region() {
        struct Unpublish(u32);
        impl Drop for Unpublish {
            fn drop(&mut self) {
                cratonvm_gc::shadow_stack::unpublish_thread_stack_band(self.0);
            }
        }
        // A tid no live thread has: the band table is process-wide.
        const FAKE_TID: u32 = u32::MAX - 0xd9c;
        cratonvm_gc::shadow_stack::publish_thread_stack_band(FAKE_TID);
        let _unpublish = Unpublish(FAKE_TID);
        let Some(own) = cratonvm_gc::shadow_stack::thread_stack_band(FAKE_TID) else {
            return; // this thread's stack cannot be introspected here
        };
        let local = [0u64; 4];
        let rsp = std::hint::black_box(local.as_ptr() as usize) & !7;
        if rsp < own.lo || rsp >= own.hi || own.hi - rsp > MAX_STACK_SCAN {
            return; // the published top would not decide the end
        }
        let regions = [(rsp, rsp.saturating_add(4 * MAX_STACK_SCAN))];
        let slot = LinuxSlot::new();
        slot.tid.store(FAKE_TID, Ordering::Release);
        slot.rsp.store(rsp, Ordering::Release);
        let mut band = vec![0u64; MAX_STACK_SCAN / 8];
        let snap = snapshot_parked_slot(&slot, &regions, &mut band);
        assert!(snap.band_words > 0);
        assert!(
            snap.band_complete,
            "the band ran to the published top, below the cap: it is whole"
        );
        assert_eq!(std::hint::black_box(local[0]), 0);
    }

    /// gcd d10/t (`gcd-d9c-...`, item 1): the take-over reader's band
    /// judgement is the helper reader's. With the peer's stack top
    /// published, a long readable region does not make the cap decide the
    /// end (`scan_slot_with_regions` used to count
    /// `XT_TAKEOVER_STACK_INCOMPLETE` here, on every take-over pause of
    /// such a peer); without a published top it does.
    #[test]
    fn the_takeover_band_is_whole_when_the_published_top_ends_it() {
        struct Unpublish(u32);
        impl Drop for Unpublish {
            fn drop(&mut self) {
                cratonvm_gc::shadow_stack::unpublish_thread_stack_band(self.0);
            }
        }
        // Not the d9/c test's fake tid: the band table is process-wide and
        // the tests run in parallel.
        const FAKE_TID: u32 = u32::MAX - 0xd10;
        const UNPUBLISHED_TID: u32 = u32::MAX - 0xd11;
        cratonvm_gc::shadow_stack::publish_thread_stack_band(FAKE_TID);
        let _unpublish = Unpublish(FAKE_TID);
        let Some(own) = cratonvm_gc::shadow_stack::thread_stack_band(FAKE_TID) else {
            return; // this thread's stack cannot be introspected here
        };
        let local = [0u64; 4];
        let rsp = std::hint::black_box(local.as_ptr() as usize) & !7;
        let cap = MAX_TAKEOVER_STACK_SCAN;
        if rsp < own.lo || rsp >= own.hi || own.hi - rsp > cap {
            return; // the published top would not decide the end
        }
        let regions = [(rsp, rsp.saturating_add(4 * cap))];
        assert_eq!(
            peer_band_bound(FAKE_TID, rsp, &regions, cap),
            Some((own.hi, false)),
            "the published top ended the band below the cap: it is whole"
        );
        assert_eq!(
            peer_band_bound(UNPUBLISHED_TID, rsp, &regions, cap),
            Some((rsp + cap, true)),
            "no published top: the cap decided the end"
        );
        assert_eq!(peer_band_bound(FAKE_TID, rsp, &[], cap), None);
        assert_eq!(std::hint::black_box(local[0]), 0);
    }
