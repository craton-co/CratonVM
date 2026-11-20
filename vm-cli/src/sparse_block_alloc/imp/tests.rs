    use super::*;

    /// The real mapping path, driven directly (the test binary's global
    /// allocator is this wrapper too, in whatever mode its environment
    /// latched): a mapped block reads zero, is not mimalloc's, keeps its
    /// bytes across a grow and a shrink-back, and is freed.
    #[test]
    fn zr10mt_a_mapped_block_is_zero_survives_realloc_and_is_not_mimallocs() {
        let a = SparseBlockAlloc;
        let size = 3 << 20;
        let layout = Layout::from_size_align(size, 8).unwrap();
        // SAFETY: test-local block, used within its layout.
        unsafe {
            let p = map_block(size);
            assert!(!p.is_null());
            assert!(
                !mi_is_in_heap_region(p as *const c_void),
                "a mapping is not mimalloc's"
            );
            assert_eq!(p as usize % PAGE, 0);
            assert!(std::slice::from_raw_parts(p, size).iter().all(|&b| b == 0));
            *p = 7;
            *p.add(size - 1) = 9;
            BIG_LIVE.fetch_add(size, Ordering::Relaxed);
            // An explicit mode, whatever this binary's environment
            // latched: ownership is by address, so it is sound.
            let (mode, min) = (SparseMode::Zeroed, 1 << 20);
            let q = a.realloc_with(mode, min, p, layout, 2 * size);
            assert!(!q.is_null());
            assert!(!mi_is_in_heap_region(q as *const c_void));
            assert_eq!(*q, 7);
            assert_eq!(*q.add(size - 1), 9);
            let grown = Layout::from_size_align(2 * size, 8).unwrap();
            let r = a.realloc_with(mode, min, q, grown, 16);
            assert!(!r.is_null());
            assert!(
                mi_is_in_heap_region(r as *const c_void),
                "a shrink below the threshold returns to mimalloc"
            );
            assert_eq!(*r, 7);
            a.dealloc_with(mode, min, r, Layout::from_size_align(16, 8).unwrap());
            // And a mapped block's dealloc is an unmap.
            let m = map_block(size);
            assert!(!m.is_null());
            let live = MAPPED_LIVE.load(Ordering::Relaxed);
            assert!(live >= map_len(size));
            a.dealloc_with(mode, min, m, layout);
        }
    }

    /// zr z11/rv9: the linked mimalloc is one whose page map backs the
    /// ownership rule (v3). If this fails, a dependency switched the
    /// `mimalloc` crate to its v2 backend and the latch forces `off`.
    #[test]
    fn zr11rv9_the_linked_mimalloc_backs_the_ownership_rule() {
        // SAFETY: a plain FFI call.
        let v = unsafe { mi_version() };
        assert!(backend_supports_mapping(v), "mimalloc {v} is not v3");
    }

    /// The kill arm never unmaps: under `Off` the page-map test is not
    /// even asked, so a block's owner cannot be misread.
    #[test]
    fn zr10mt_off_never_claims_a_block() {
        // SAFETY: a page-map lookup is not made for `Off`; the pointer is
        // never dereferenced.
        unsafe {
            let fake = 0x1000 as *mut u8;
            assert!(!is_mapped(SparseMode::Off, fake, 1 << 30, 1 << 20));
            assert!(
                !is_mapped(SparseMode::Zeroed, fake, 4096, 1 << 20),
                "under min"
            );
        }
    }

    /// A small block is mimalloc's and the page-map test says so: the
    /// address test cannot mistake one for a mapping.
    #[test]
    fn zr10mt_a_mimalloc_block_is_in_its_heap_region() {
        let a = SparseBlockAlloc;
        let layout = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: test-local block.
        unsafe {
            let p = a.alloc(layout);
            assert!(mi_is_in_heap_region(p as *const c_void));
            // Claim a size over the threshold so the page-map test runs.
            assert!(!is_mapped(SparseMode::Zeroed, p, 1 << 30, 1 << 20));
            a.dealloc(p, layout);
        }
    }
