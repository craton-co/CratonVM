    use super::*;

    fn fake_obj(addr: usize) -> Option<ObjectRef> {
        // 8-aligned non-null address — the validator contract the real
        // `is_object_address` provides.
        if addr != 0 && addr % 8 == 0 {
            // SAFETY: non-null, 8-aligned; test-only value never dereferenced.
            Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
        } else {
            None
        }
    }

    /// gcd d2/i: a proven blocked monitor peer is credited only at the depth
    /// it published, and one whose depth disagrees goes back to the scan.
    #[test]
    fn proven_blocked_monitor_peers_are_credited_only_at_their_published_depth() {
        let depth_of = |tid: u32| match tid {
            2 => Some(3),
            4 => Some(6),
            _ => None,
        };
        let mut credited = Vec::new();
        let scan = credit_proven_blocked_monitor_peers(
            vec![1],
            &[(2, 3), (4, 5), (7, 1)],
            depth_of,
            |d| credited.push(d),
        );
        assert_eq!(
            credited,
            vec![3],
            "only the peer whose depth matches is credited"
        );
        assert_eq!(
            scan,
            vec![1, 4, 7],
            "the rest are scanned as helper windows"
        );
    }


    /// zt t26/zc5b item 1: the registry spans every published TLAB address
    /// and nothing for a thread that published none.
    #[test]
    fn zt26_zc5b_the_registry_spans_each_published_tlab() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let registry = &shared.threads.thread_registry;
        let before = registry.tlab_pointer_slots().len();
        let tid = crate::ThreadId(26_026);
        registry.register(tid, "zc5b-tlab", None);
        registry.set_tlab_addr(tid, 0x4_0000);
        let after = registry.tlab_pointer_slots();
        assert_eq!(after.len(), before + 1);
        assert!(after.contains(&(0x4_0000, 0x4_0018)));
        registry.clear_tlab_addr(tid);
        assert_eq!(registry.tlab_pointer_slots().len(), before);
    }

    /// gcd d3/m: a band read into the young pin ledger keeps every word in a
    /// young region RAW (a base, an interior word, one-past-end, the low
    /// slack) and nothing else, in order, appended to what is there.
    #[test]
    fn a_band_keeps_every_young_word_raw_and_nothing_else() {
        const BASE: usize = 0x7000_0000;
        const END: usize = 0x7010_0000;
        let range = cratonvm_gc::gc_quiescence::YoungPinRange::from_regions([(BASE, END), (0, 0)]);
        let slack = cratonvm_gc::gc_quiescence::YOUNG_PIN_LOW_SLACK;
        let mut out = vec![1usize];
        collect_young_band_words(
            &range,
            [
                0,
                BASE,
                BASE + 0x1003,
                42,
                END,
                END + 8,
                BASE - slack,
                BASE - slack - 8,
            ],
            &mut out,
        );
        assert_eq!(out, vec![1, BASE, BASE + 0x1003, END, BASE - slack]);
        let empty = cratonvm_gc::gc_quiescence::YoungPinRange::from_regions([(0, 0), (0, 0)]);
        let mut out = Vec::new();
        collect_young_band_words(&empty, [BASE, BASE + 8], &mut out);
        assert!(
            out.is_empty(),
            "no published young region: nothing is young"
        );
    }

    /// The per-pause helper-window verdict (gcd d2/i): an unbound test thread
    /// reads its own fallback ledger, so a refusal recorded here is read back
    /// here and the reset clears it.
    #[test]
    fn the_helper_window_verdict_reads_the_pause_ledger() {
        reset_helper_window_cycle();
        assert!(helper_windows_all_pinned_this_cycle());
        cratonvm_gc::gc_quiescence::set_xt_helper_windows_unpinned(2);
        assert_eq!(helper_windows_unpinned_this_cycle(), 2);
        assert!(!helper_windows_all_pinned_this_cycle());
        reset_helper_window_cycle();
        assert_eq!(helper_windows_unpinned_this_cycle(), 0);
    }

    /// A stack band with a JIT return address is classified as a helper
    /// window and its object-resolving words become candidate roots.
    #[test]
    fn helper_window_classifier_detects_jit_band_and_collects_candidates() {
        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        // Two heap-shaped words; the validator accepts only 0x10_0008.
        let obj = 0x10_0008usize;
        let words = [
            0usize,
            0x7000_0123, // return address into the JIT range
            obj,
            0xdead_beef,
        ];
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &ranges, &is_obj, &mut candidates);
        assert!(
            has_jit,
            "JIT return address on the band must classify as a helper window"
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].as_ptr() as usize, obj);
    }

    /// A pure-native stack (no JIT return address anywhere) is NOT a helper
    /// window — the caller must discard its candidates (no blanket
    /// over-retention for threads that never ran JIT code).
    #[test]
    fn helper_window_classifier_rejects_band_without_jit_frames() {
        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        let obj = 0x10_0008usize;
        let words = [obj, 0x12345usize, 0usize];
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &ranges, &is_obj, &mut candidates);
        assert!(!has_jit);
        // Candidates were still collected — the caller's `has_jit` gate is
        // what discards them.
        assert_eq!(candidates.len(), 1);
    }

    /// A frozen-in-JIT peer's shadow stack is read into the take-over root set.
    ///
    /// The layout is `ShadowStack`'s `#[repr(C)]` prefix (`top`, `end`,
    /// `base`), published under a tid no real thread in the test binary holds.
    /// Before gc-common w1-c the take-over pass never read this window at all,
    /// so the one oop here -- a value an outer compiled frame parked across a
    /// call into a spinning callee -- was in no root set.
    #[test]
    fn taken_over_peer_shadow_window_is_scanned() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE01;
        let obj = 0x10_0010usize;
        let buf: Box<[usize; 4]> = Box::new([0, obj, 0xdead_beef, 0]);
        let base = buf.as_ptr() as usize;
        let end = base + 4 * 8;
        // `top` covers three slots; the fourth is above the stack and must
        // not be read.
        let ss = FakeShadow {
            top: base + 3 * 8,
            end,
            base,
        };
        let ss_addr = &ss as *const FakeShadow as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(TID, ss_addr, base, end);
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut roots = Vec::new();
        let n = scan_taken_over_peer_shadow(TID, &is_obj, &mut roots);
        let enabled = crate::jit::conservative_roots::xt_peer_shadow_scan_enabled();
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        if enabled {
            assert_eq!(n, 1);
            assert_eq!(roots.len(), 1);
            assert_eq!(roots[0].as_ptr() as usize, obj);
        } else {
            assert_eq!(
                n, 0,
                "the kill switch restores the pre-fix take-over root set"
            );
        }
        assert_eq!(std::hint::black_box(ss.top), base + 24);
    }

    /// A published triple that no longer matches the struct it names is
    /// refused rather than read -- the identity check the blocked-peer path
    /// already relied on, shared unchanged by the take-over path.
    #[test]
    fn taken_over_peer_shadow_window_refuses_a_stale_triple() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE02;
        let obj = 0x10_0018usize;
        let buf: Box<[usize; 2]> = Box::new([obj, obj]);
        let base = buf.as_ptr() as usize;
        let ss = FakeShadow {
            top: base + 16,
            end: base + 16,
            base,
        };
        // Published with a DIFFERENT end: the struct moved or was freed.
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(
            TID,
            &ss as *const FakeShadow as usize,
            base,
            base + 64,
        );
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut roots = Vec::new();
        let n = scan_taken_over_peer_shadow(TID, &is_obj, &mut roots);
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        assert_eq!(n, 0);
        assert!(roots.is_empty());
        assert_eq!(std::hint::black_box(ss.top), base + 16);
    }

    /// gc-common w5-g (`ShadowOddLongProbe`): a peer's INDIRECT shadow entry
    /// is resolved only inside the stack band the peer published under the
    /// same tid. Inside it the frame word is a root; an odd primitive outside
    /// it (here: a readable heap word holding an object address) is not read;
    /// and with no band at all the entry is not read and the window is
    /// untrusted rather than claimed covered.
    #[test]
    fn peer_indirect_entries_resolve_only_inside_the_published_stack_band() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE03;
        let obj = 0x10_0020usize;
        let decoy = 0x10_0028usize;
        // The "peer's compiled frame": a word of THIS thread's stack, whose
        // band the test publishes under the fake tid.
        let mut frame = [obj, 0usize];
        let slot = std::hint::black_box(frame.as_mut_ptr()) as usize;
        // A readable word that is NOT stack, holding an object address: what
        // an odd primitive that happens to name mapped memory would reach.
        let heap: Box<[usize; 1]> = Box::new([decoy]);
        let heap_word = std::hint::black_box(heap.as_ptr()) as usize;
        let tag = cratonvm_gc::shadow_stack::ShadowStack::INDIRECT_TAG;
        let buf: Box<[usize; 2]> = Box::new([slot | tag, heap_word | tag]);
        let base = buf.as_ptr() as usize;
        let end = base + 16;
        let ss = FakeShadow {
            top: end,
            end,
            base,
        };
        let ss_addr = &ss as *const FakeShadow as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(TID, ss_addr, base, end);
        let is_obj = |a: usize| {
            if a == obj || a == decoy {
                fake_obj(a)
            } else {
                None
            }
        };

        // No band published: nothing indirect is read, the window is untrusted.
        let mut roots = Vec::new();
        assert!(read_peer_shadow_window(TID, &is_obj, &mut roots).is_none());
        assert!(roots.is_empty());

        // Band published: the stack slot resolves, the heap word does not.
        cratonvm_gc::shadow_stack::publish_thread_stack_band(TID);
        let mut roots = Vec::new();
        let got = read_peer_shadow_window(TID, &is_obj, &mut roots);
        let band_known = cratonvm_gc::shadow_stack::thread_stack_band(TID).is_some();
        cratonvm_gc::shadow_stack::unpublish_thread_stack_band(TID);
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        if band_known {
            assert_eq!(got, Some(2));
            let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
            assert_eq!(
                addrs,
                vec![obj],
                "the decoy behind a non-stack odd word is never read"
            );
        }
        assert_eq!(std::hint::black_box(ss.top), end);
        let _ = std::hint::black_box(&mut frame);
        drop(heap);
    }

    /// rx27 (frame descriptor records): a peer window holding a descriptor
    /// record yields every home inside the published band as a root (and not
    /// the record's own words), and none without a band.
    #[test]
    fn rx27_peer_descriptor_record_homes_are_roots() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE13;
        let obj_a = 0x10_0040usize;
        let obj_b = 0x10_0048usize;
        // The peer's frame: four words of THIS thread's stack; rbp one past.
        let mut frame = [obj_b, 0usize, 0usize, obj_a];
        let lo = std::hint::black_box(frame.as_mut_ptr()) as usize;
        let rbp = lo + 32;
        // Homes rbp-8 (obj_a) and rbp-32 (obj_b).
        let packed = 1usize | (4usize << 16);
        let buf: Box<[usize; 3]> = Box::new([
            rbp | cratonvm_gc::shadow_stack::DESCRIPTOR_TAG,
            cratonvm_gc::shadow_stack::descriptor_header(2) as usize,
            packed,
        ]);
        let base = buf.as_ptr() as usize;
        let end = base + 24;
        let ss = FakeShadow {
            top: end,
            end,
            base,
        };
        let ss_addr = &ss as *const FakeShadow as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(TID, ss_addr, base, end);
        let is_obj = |a: usize| {
            if a == obj_a || a == obj_b {
                fake_obj(a)
            } else {
                None
            }
        };
        // No band: the record's homes are not read; the window is untrusted.
        let mut roots = Vec::new();
        assert_eq!(
            read_peer_shadow_window_ex(TID, &is_obj, &mut roots),
            PeerWindowRead::BandlessIndirect(3)
        );
        assert!(roots.is_empty());
        cratonvm_gc::shadow_stack::publish_thread_stack_band(TID);
        let mut roots = Vec::new();
        let got = read_peer_shadow_window(TID, &is_obj, &mut roots);
        let band_known = cratonvm_gc::shadow_stack::thread_stack_band(TID).is_some();
        cratonvm_gc::shadow_stack::unpublish_thread_stack_band(TID);
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        if band_known {
            assert_eq!(got, Some(3));
            let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
            assert_eq!(addrs, vec![obj_a, obj_b], "both homes, in record order");
        }
        assert_eq!(std::hint::black_box(ss.top), end);
        let _ = std::hint::black_box(&mut frame);
    }

    /// gc-common w6-g: a TAKE-OVER window whose peer published no stack band
    /// (the only case on a target other than Windows/Linux) adds its value
    /// entries, never dereferences an indirect one, and withholds the pause's
    /// complete-pins claim through the pause ledger's stack-incomplete row
    /// (also counted in the process total `XT_TAKEOVER_STACK_INCOMPLETE`),
    /// which `stw_take_over_and_wait` reads into the `TakeoverVerdict`.
    #[test]
    fn a_bandless_takeover_window_keeps_values_and_withholds_complete_pins() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE04;
        let obj = 0x10_0030usize;
        let behind = 0x10_0038usize;
        // A readable word holding an object address, named by an indirect
        // entry: it must not be read without a band.
        let word: Box<[usize; 1]> = Box::new([behind]);
        let word_addr = std::hint::black_box(word.as_ptr()) as usize;
        let tag = cratonvm_gc::shadow_stack::ShadowStack::INDIRECT_TAG;
        let buf: Box<[usize; 2]> = Box::new([obj, word_addr | tag]);
        let base = buf.as_ptr() as usize;
        let end = base + 16;
        let ss = FakeShadow {
            top: end,
            end,
            base,
        };
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(
            TID,
            &ss as *const FakeShadow as usize,
            base,
            end,
        );
        assert!(cratonvm_gc::shadow_stack::thread_stack_band(TID).is_none());
        let is_obj = |a: usize| {
            if a == obj || a == behind {
                fake_obj(a)
            } else {
                None
            }
        };

        let mut roots = Vec::new();
        assert_eq!(
            read_peer_shadow_window_ex(TID, &is_obj, &mut roots),
            PeerWindowRead::BandlessIndirect(2)
        );
        let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(
            addrs,
            vec![obj],
            "the value entry is a root; the indirect one is not read"
        );

        let incomplete_before = XT_TAKEOVER_STACK_INCOMPLETE.load(Ordering::Relaxed);
        let bandless_before = XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed);
        // gce e1/t: the verdict reads THIS pause's ledger row, not the process
        // total, so the note must land in the initiator's bound ledger -- and
        // only there (a second VM's ledger stays untouched).
        let this_vm = std::sync::Arc::new(cratonvm_gc::gc_quiescence::PauseLedger::new());
        let other_vm = std::sync::Arc::new(cratonvm_gc::gc_quiescence::PauseLedger::new());
        let mut roots = Vec::new();
        let n = cratonvm_gc::gc_quiescence::with_pause_ledger(&this_vm, || {
            cratonvm_gc::gc_quiescence::reset_xt_cycle();
            scan_taken_over_peer_shadow(TID, &is_obj, &mut roots)
        });
        let enabled = crate::jit::conservative_roots::xt_peer_shadow_scan_enabled();
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        let row = |l: &std::sync::Arc<cratonvm_gc::gc_quiescence::PauseLedger>| {
            cratonvm_gc::gc_quiescence::with_pause_ledger(
                l,
                cratonvm_gc::gc_quiescence::xt_takeover_stack_incomplete_this_pause,
            )
        };
        if enabled {
            assert_eq!(n, 1);
            assert!(XT_TAKEOVER_STACK_INCOMPLETE.load(Ordering::Relaxed) > incomplete_before);
            assert!(XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed) > bandless_before);
            assert_eq!(row(&this_vm), Some(1), "the initiator's own pause row");
        } else {
            assert_eq!(row(&this_vm), Some(0));
        }
        assert_eq!(row(&other_vm), Some(0), "another VM's pause never sees it");
        assert_eq!(std::hint::black_box(ss.top), end);
        drop(word);
    }

    /// Empty range set never classifies anything (JIT never emitted code).
    #[test]
    fn helper_window_classifier_empty_ranges() {
        let words = [0x7000_0123usize];
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &[], &|_| None, &mut candidates);
        assert!(!has_jit);
        assert!(candidates.is_empty());
    }

    /// gc-common w2-c: the take-over probe resolves a frozen register's DERIVED
    /// pointer -- a misaligned cursor into a `byte[]` and a one-past-the-end
    /// cursor -- to an address INSIDE the array, on Generational, whose
    /// `is_heap_addr` answers "aligned and in a region", not "in an object".
    ///
    /// Before w2-c the probe was exact-base only by default (both cursors ->
    /// `None`, the array in no root set), and with the opt-in it resolved `w`
    /// first, which for a one-past-the-end cursor answers `w` itself: the NEXT
    /// object's (or on G1, possibly the next region's) address.
    #[test]
    fn takeover_probe_resolves_derived_cursors_inside_their_array() {
        use crate::config::{GcAlgorithm, VmConfig};
        if !takeover_interior_enabled() {
            // `CRATONVM_XT_TAKEOVER_INTERIOR=0` in the test environment: the
            // kill switch restores the exact-base probe, nothing to check.
            return;
        }
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let heap = &shared.mem.heap;
        let arr = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            13,
        );
        let base = arr.as_ptr() as usize;
        let size = heap
            .walk_objects()
            .iter()
            .find(|(p, _)| *p as usize == base)
            .map(|&(_, s)| s)
            .expect("a freshly allocated array is walkable");
        assert!(size >= 16, "an array has a header");
        let inside = |w: usize| {
            let r = takeover_word_probe(heap, w)
                .unwrap_or_else(|| panic!("derived pointer {w:#x} resolved to nothing"));
            let a = r.as_ptr() as usize;
            assert!(
                a >= base && a < base + size,
                "derived pointer {w:#x} resolved to {a:#x}, outside [{base:#x}, {:#x})",
                base + size
            );
        };
        // The exact base is still the exact base.
        assert_eq!(
            takeover_word_probe(heap, base).map(|o| o.as_ptr() as usize),
            Some(base)
        );
        // A misaligned cursor into the body.
        inside(base + size - 3);
        // One past the end: the loop finished, the cursor is all that is left.
        // Allocate a neighbour so the common shape is exercised -- bump
        // allocation puts the next object's BASE exactly there, the exact probe
        // accepts it as that object, and only the companion names `arr`.
        let next = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            5,
        );
        let end = base + size;
        let probed = takeover_word_probe(heap, end);
        let companion = takeover_word_companion(heap, end, probed);
        let names_arr = |o: Option<ObjectRef>| {
            o.is_some_and(|o| {
                let a = o.as_ptr() as usize;
                a >= base && a < end
            })
        };
        assert!(
            names_arr(probed) || names_arr(companion),
            "a one-past-the-end cursor {end:#x} must root the array it walked \
             (probe {:?}, companion {:?}, neighbour at {:#x})",
            probed.map(|o| o.as_ptr()),
            companion.map(|o| o.as_ptr()),
            next.as_ptr() as usize
        );
        // A companion is only ever produced for an EXACT-base answer.
        assert!(takeover_word_companion(heap, base + size - 3, None).is_none());
        let _keep = (arr, next);
    }

    /// gc-common w3-g: the same two derived-pointer shapes through the
    /// HELPER-WINDOW probe (`common-w2c-helper-window-probe-misses-the-object-
    /// ending-at-a-cursor`). Before w3-g the pass probed with a bare
    /// `resolve_interior_for_pin`: a misaligned cursor resolved to nothing on
    /// Generational, and a one-past-the-end cursor named only the neighbour.
    #[test]
    fn helper_window_probe_resolves_derived_cursors_inside_their_array() {
        use crate::config::{GcAlgorithm, VmConfig};
        if !helper_window_pin_resolve_enabled() {
            // Kill switch set in the test environment: the pre-2026-09-08
            // probe, and no companion, by design.
            return;
        }
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let heap = &shared.mem.heap;
        let arr = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            13,
        );
        let base = arr.as_ptr() as usize;
        let size = heap
            .walk_objects()
            .iter()
            .find(|(p, _)| *p as usize == base)
            .map(|&(_, s)| s)
            .expect("a freshly allocated array is walkable");
        let end = base + size;
        let names_arr = |o: Option<ObjectRef>| {
            o.is_some_and(|o| {
                let a = o.as_ptr() as usize;
                a >= base && a < end
            })
        };
        // A misaligned cursor into the body resolves inside the array.
        assert!(
            names_arr(helper_window_word_probe(heap, end - 3)),
            "misaligned cursor {:#x} must resolve inside the array",
            end - 3
        );
        let next = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            5,
        );
        let probed = helper_window_word_probe(heap, end);
        let companion = helper_window_word_companion(heap, end, probed);
        assert!(
            names_arr(probed) || names_arr(companion),
            "a one-past-the-end cursor {end:#x} must root the array it walked \
             (probe {:?}, companion {:?}, neighbour at {:#x})",
            probed.map(|o| o.as_ptr()),
            companion.map(|o| o.as_ptr()),
            next.as_ptr() as usize
        );
        // No companion for a word the probe declined.
        assert!(helper_window_word_companion(heap, end - 3, None).is_none());
        let _keep = (arr, next);
    }

    /// gc-common w9-g (`common-c-linux-takeover-signals-every-thread-FIXED-20260929`): the
    /// Linux take-over signals the ROSTER by default, not every task of the
    /// process; the audit's task list replaces it only when armed. Either list
    /// is sorted and de-duplicated (a mounted virtual thread republishes its
    /// carrier's tid), and 0 (unpublished), the collector and an
    /// already-frozen peer are never signalled. Pure, so it runs on every host.
    #[test]
    fn linux_takeover_signals_the_deduplicated_roster_unless_audited() {
        let mut taken = TakenOver::default();
        taken.handles.push(0);
        taken.tids.push(41);
        let roster = [44, 0, 41, 43, 44, 42];
        assert_eq!(
            takeover_signal_candidates(&roster, None, 42, &taken),
            vec![43, 44],
            "roster only: sorted, one signal per OS thread, no 0, no self, no frozen peer"
        );
        // Audited: every task, including the ones off the roster (7 is a JIT
        // compiler worker, say) -- with the same three exclusions.
        let tasks = vec![7, 44, 42, 41, 43, 900];
        assert_eq!(
            takeover_signal_candidates(&roster, Some(tasks), 42, &taken),
            vec![7, 43, 44, 900]
        );
        // An empty roster signals nobody; nothing is invented.
        assert!(takeover_signal_candidates(&[], None, 42, &TakenOver::default()).is_empty());
        // A fresh take-over holds no Linux handler session.
        assert!(!TakenOver::default().holds_session);
        // The peers above are invented: nothing to hand back on drop.
        taken.resumed = true;
    }

    /// JIT round 19 residual (lane eintr19's page): a take-over dropped by an
    /// unwind before any `resume` hands its native claims back, once;
    /// a resumed one does nothing more on drop.
    #[test]
    fn an_unwind_out_of_a_pause_resumes_the_take_over_once() {
        use crate::threading::jvm_thread::{GcBlockState, LAZY_NATIVE_IN, LAZY_NATIVE_OFF};
        let state = std::sync::Arc::new(GcBlockState::new());
        state.enter_lazy_native(0x10_000, 0x20_000);
        assert!(state.try_claim_lazy_native().is_some());
        let held = std::sync::Arc::clone(&state);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut taken = TakenOver::default();
            taken.push_native_claim(4242, held);
            panic!("a collection unwinds before resume");
        }));
        assert!(unwound.is_err());
        assert_eq!(
            state.lazy_native.load(Ordering::Acquire),
            LAZY_NATIVE_IN,
            "the unwind handed the claim back"
        );
        assert!(!state.leave_lazy_native());
        assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
        // Normal path: `resume` releases, the drop inside it adds nothing.
        let mut taken = TakenOver::default();
        assert!(!taken.resumed);
        resume_held(&mut taken);
        assert!(taken.resumed);
        assert!(taken
            .claims_released
            .load(std::sync::atomic::Ordering::Acquire));
    }

    /// JIT round 16 wave 14 (lane ffm3): a peer claimed in C counts as held,
    /// is in the tid set every frozen-peer reader uses, is never signalled,
    /// and is released by `resume`.
    #[test]
    fn a_claimed_peer_is_held_like_a_frozen_one_and_released_by_resume() {
        use crate::threading::jvm_thread::{GcBlockState, LAZY_NATIVE_IN, LAZY_NATIVE_OFF};
        let state = std::sync::Arc::new(GcBlockState::new());
        state.enter_lazy_native(0x10_000, 0x20_000);
        assert!(state.try_claim_lazy_native().is_some());
        let mut taken = TakenOver::default();
        taken.push_native_claim(4242, std::sync::Arc::clone(&state));
        assert_eq!(taken.count(), 1);
        assert_eq!(taken.native_claim_count(), 1);
        assert!(taken.contains(4242));
        assert!(taken.is_native_claim(4242));
        assert!(
            takeover_signal_candidates(&[4242, 4343], None, 1, &taken) == vec![4343],
            "a claimed peer is never signalled"
        );
        resume(taken);
        assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert!(!state.leave_lazy_native());
        assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
    }

    #[test]
    fn a_conservative_native_claim_never_fabricates_a_jit_entry() {
        let src = include_str!("../xt_root_scan.rs");
        let push = &src[src.find("pub(crate) fn push_native_claim(").unwrap()..];
        let push = &push[..push.find("\n    \u{7d}\n").unwrap()];
        assert!(!push.contains("gc_quiescence::enter("));
        let release = &src[src
            .find("fn release_native_claims(taken: &TakenOver)")
            .unwrap()..];
        let release = &release[..release.find("\n\u{7d}\n").unwrap()];
        assert!(!release.contains("gc_quiescence::leave("));
        std::thread::spawn(|| {
            use crate::threading::jvm_thread::{GcBlockState, LAZY_NATIVE_IN};
            let state = std::sync::Arc::new(GcBlockState::new());
            state.enter_lazy_native(0x10_000, 0x20_000);
            assert!(state.try_claim_lazy_native().is_some());
            let mut taken = TakenOver::default();
            taken.push_native_claim(4242, std::sync::Arc::clone(&state));
            assert_eq!(taken.unrewritable_count(), 1);
            resume(taken);
            assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
            assert!(!state.leave_lazy_native());
        })
        .join()
        .unwrap();
    }
    #[test]
    fn native_claim_shadow_requires_the_whole_identity_checked_window() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FD43;
        let obj = 0x10_0010usize;
        let buf = Box::new([obj, 0usize]);
        let base = buf.as_ptr() as usize;
        let mut shadow = FakeShadow {
            top: base + 8,
            end: base + 16,
            base,
        };
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(
            TID,
            &shadow as *const FakeShadow as usize,
            base,
            base + 16,
        );
        let mut roots = Vec::new();
        if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
            assert_eq!(
                scan_native_claim_shadow(TID, &fake_obj, &mut roots),
                Some(1)
            );
            assert_eq!(roots[0].as_ptr() as usize, obj);
            shadow.top = base + 24;
            std::hint::black_box(&shadow);
            assert_eq!(
                scan_native_claim_shadow(TID, &fake_obj, &mut roots),
                None,
                "a corrupt top must not buy a pin credit"
            );
            shadow.top = base + 8;
            shadow.base = base + 8;
            std::hint::black_box(&shadow);
            assert_eq!(
                scan_native_claim_shadow(TID, &fake_obj, &mut roots),
                None,
                "a stale window must not buy a pin credit"
            );
        }
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        assert_eq!(
            scan_native_claim_shadow(TID, &fake_obj, &mut roots),
            None,
            "unpublished is unknown, not empty"
        );
    }

    #[test]
    fn pinned_native_claim_deposits_without_a_frozen_peer_or_fake_jit_entry() {
        use crate::threading::jvm_thread::{GcBlockState, LAZY_NATIVE_IN};
        let state = std::sync::Arc::new(GcBlockState::new());
        state.enter_lazy_native(0x10_000, 0x20_000);
        assert!(state.try_claim_lazy_native().is_some());
        let mut taken = TakenOver::default();
        taken.push_pinned_native_claim(4242, std::sync::Arc::clone(&state));
        assert_eq!(taken.count(), 1);
        assert_eq!(
            taken.unrewritable_count(),
            0,
            "complete deposit is not a frozen peer"
        );
        assert_eq!(taken.pinned_native_claim_count(), 1);
        assert!(state.lazy_native_precise_claim.load(Ordering::Acquire));
        assert!(taken.unrewritable_os_tids().is_empty());
        assert!(takeover_signal_candidates(&[4242, 4343], None, 1, &taken) == vec![4343]);
        // An actual frozen/conservative peer must still refuse the cycle.
        taken.tids.push(4343);
        taken.handles.push(0);
        assert_eq!(taken.unrewritable_count(), 1);
        assert_eq!(taken.unrewritable_os_tids(), vec![4343]);
        taken.tids.pop();
        taken.handles.pop();
        resume(taken);
        assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert!(!state.lazy_native_precise_claim.load(Ordering::Acquire));
        assert!(!state.leave_lazy_native());
    }
    #[test]
    fn a_native_band_must_prove_no_guardless_compiled_frame() {
        let band = [0usize, 0x7000_0123, 0x10_0010, usize::MAX];
        let lo = band.as_ptr() as usize;
        let hi = lo + std::mem::size_of_val(&band);
        let ranges = [(0x7000_0000, 0x7000_4000)];
        // SAFETY: a live local array; invalid bands below are never read.
        unsafe {
            assert_eq!(
                native_claim_band_has_compiled_word(lo, hi, &ranges),
                Some(true),
                "an empty entry chain cannot excuse this compiled return address"
            );
            assert_eq!(
                native_claim_band_has_compiled_word(lo, hi, &[(0x8000_0000, 0x8000_4000)]),
                Some(false)
            );
            assert_eq!(
                native_claim_band_has_compiled_word(lo + 4, hi, &ranges),
                None
            );
            assert_eq!(native_claim_band_has_compiled_word(lo, lo, &ranges), None);
        }
    }

    /// JIT round 16 wave 14 (lane ffm3): the claimed band is read word by
    /// word, every accepted word a root; an unusable band reads nothing.
    #[test]
    fn a_claimed_band_is_scanned_conservatively() {
        let band: [usize; 6] = [0, 0x5008, 7, 0x6010, 0x6011, usize::MAX];
        let lo = band.as_ptr() as usize;
        let hi = lo + std::mem::size_of_val(&band);
        let mut roots = Vec::new();
        // SAFETY: a live local array, read while it is in scope.
        let n = unsafe { scan_native_claim_band(lo, hi, &fake_obj, &mut roots) };
        let got: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(got, vec![0x5008, 0x6010]);
        assert_eq!(n, 2);
        let mut none = Vec::new();
        // SAFETY: a refused band is not read.
        assert_eq!(
            unsafe { scan_native_claim_band(lo + 4, hi, &fake_obj, &mut none) },
            0
        );
        assert!(none.is_empty());
    }

    /// gce e2/t: `CRATONVM_DBG_XT_FORCE_TAKEOVER` declines one pause for at
    /// most `DECLINE_LIMIT` from its first decline, then parks; the next
    /// pause (a new barrier generation) opens a new window. So a forced run
    /// can be late but never hangs.
    #[test]
    fn force_takeover_declines_each_pause_for_a_bounded_window() {
        let t0 = std::time::Instant::now();
        let limit = ForceTakeover::DECLINE_LIMIT;
        let mut f = ForceTakeover::new();
        assert!(f.decline_at(7, t0), "first poll of a pause declines");
        assert!(f.decline_at(7, t0 + limit / 2), "inside the window");
        assert!(!f.decline_at(7, t0 + limit), "the window is closed: park");
        assert!(
            !f.decline_at(7, t0 + limit * 3),
            "and stays closed for that pause"
        );
        assert!(
            f.decline_at(8, t0 + limit * 3),
            "a new pause opens a new window"
        );
        assert!(f.decline_at(8, t0 + limit * 3 + limit / 2));
        assert!(!f.decline_at(8, t0 + limit * 4));
        // A clock that reads earlier than the window's start (another core's
        // `Instant`) is inside the window, not a panic.
        let mut g = ForceTakeover::new();
        assert!(g.decline_at(1, t0 + limit));
        assert!(g.decline_at(1, t0));
    }

    /// gcd d10/t (`common-c`): the opt-in narrowing withholds the signal only
    /// from a thread whose published JIT depth is exactly zero. An unknown
    /// depth (`None`: never published) is signalled, and order is kept.
    #[test]
    fn jit_only_signalling_skips_only_a_published_zero_depth() {
        let depth = |tid: u32| match tid {
            10 => Some(0),
            11 => Some(3),
            12 => None,
            13 => Some(0),
            _ => Some(1),
        };
        let (signal, skipped) = split_by_published_jit_depth(vec![10, 11, 12, 13, 14], depth);
        assert_eq!(signal, vec![11, 12, 14]);
        assert_eq!(skipped, vec![10, 13]);
        let (signal, skipped) = split_by_published_jit_depth(Vec::new(), depth);
        assert!(signal.is_empty() && skipped.is_empty());
        // Bounded per take-over: a fresh one starts counting its passes at 0
        // (`JIT_ONLY_SIGNAL_PASSES` narrowed passes, then the whole roster).
        assert_eq!(TakenOver::default().passes, 0);
    }

    /// JIT round 17 wave 1 (lane eintr): the three shapes the kernel writes to
    /// `/proc/<pid>/task/<tid>/syscall`, and garbage.
    #[test]
    fn eintr17_proc_syscall_lines_parse() {
        assert_eq!(parse_proc_syscall(b"running\n"), PeerSyscall::Running);
        assert_eq!(
            parse_proc_syscall(b"-1 0x7ffc1000 0x7f0011223344\n"),
            PeerSyscall::OffSyscall
        );
        // clock_nanosleep (230) as usleep makes it: six args, sp, pc.
        assert_eq!(
            parse_proc_syscall(b"230 0x0 0x0 0x7ffd5000 0x0 0x0 0x0 0x7ffd4f00 0x7f00aabbccdd\n"),
            PeerSyscall::InSyscall {
                pc: 0x7f00_aabb_ccdd
            }
        );
        let malformed: [&[u8]; 8] = [
            b"",
            b"\n",
            b"sleeping",
            b"230 0x0 0x0",
            b"230 0x0 0x0 0x0 0x0 0x0 0x0 0x1 zz",
            b"230 0x0 0x0 0x0 0x0 0x0 0x0 0x1 0x2 0x3",
            b"-1 0x1",
            b"\xff\xfe",
        ];
        for bad in malformed {
            assert_eq!(parse_proc_syscall(bad), PeerSyscall::Unknown, "{bad:?}");
        }
    }

    /// JIT round 17 wave 1 (lane eintr): only a peer blocked INSIDE a system
    /// call with its `Rip` outside every compiled range goes unsignalled.
    #[test]
    fn eintr17_only_a_peer_in_a_syscall_outside_compiled_code_is_not_signalled() {
        let ranges = [(0x1000usize, 0x2000usize), (0x5000, 0x6000)];
        let skip = |s| takeover_signal_skips_peer_in_syscall(s, &ranges);
        assert!(skip(PeerSyscall::InSyscall { pc: 0x7f00_0000 }));
        assert!(
            skip(PeerSyscall::InSyscall { pc: 0x2000 }),
            "one past a range is outside"
        );
        // A JIT-emitted syscall: the handler would park it, so it is signalled.
        assert!(!skip(PeerSyscall::InSyscall { pc: 0x1800 }));
        assert!(!skip(PeerSyscall::InSyscall { pc: 0x5000 }));
        assert!(!skip(PeerSyscall::InSyscall { pc: 0 }));
        // Running, blocked outside a syscall, unreadable: signalled as before.
        assert!(!skip(PeerSyscall::Running));
        assert!(!skip(PeerSyscall::OffSyscall));
        assert!(!skip(PeerSyscall::Unknown));
        // No compiled code at all: nothing can be in it.
        assert!(takeover_signal_skips_peer_in_syscall(
            PeerSyscall::InSyscall { pc: 0x1800 },
            &[]
        ));
    }

    /// JIT round 17 wave 6 (lane eintr2, `CRATONVM_XT_TAKEOVER_STAT_FALLBACK`):
    /// the state letter follows the LAST `)` of a `stat` line, whatever the
    /// command name holds; only `S` is a skippable sleep, and it is skipped
    /// with or without compiled code.
    #[test]
    fn eintr2_stat_state_letters_parse_and_only_a_sleep_is_skipped() {
        assert_eq!(
            parse_proc_stat_state(b"4242 (java) S 1 4242 0 0 -1\n"),
            Some(b'S')
        );
        assert_eq!(
            parse_proc_stat_state(b"77 (C2 Compiler) R 1 77\n"),
            Some(b'R')
        );
        // A name with spaces and parentheses of its own.
        assert_eq!(parse_proc_stat_state(b"9 (a) b (c)) D 1 9\n"), Some(b'D'));
        // Truncated right after the letter: still read.
        assert_eq!(parse_proc_stat_state(b"9 (x) t"), Some(b't'));
        let malformed: [&[u8]; 6] = [
            b"",
            b"9 (x)",
            b"9 (x) ",
            b"9 (x) SS 1",
            b"9 (x) 1 2",
            b"no paren S",
        ];
        for bad in malformed {
            assert_eq!(parse_proc_stat_state(bad), None, "{bad:?}");
        }
        assert_eq!(
            peer_syscall_from_stat_state(Some(b'S')),
            PeerSyscall::Sleeping
        );
        assert_eq!(
            peer_syscall_from_stat_state(Some(b'R')),
            PeerSyscall::Running
        );
        for other in [
            Some(b'D'),
            Some(b'T'),
            Some(b't'),
            Some(b'Z'),
            Some(b'I'),
            None,
        ] {
            assert_eq!(peer_syscall_from_stat_state(other), PeerSyscall::Unknown);
        }
        let ranges = [(0x1000usize, 0x2000usize)];
        assert!(takeover_signal_skips_peer_in_syscall(
            PeerSyscall::Sleeping,
            &ranges
        ));
        assert!(takeover_signal_skips_peer_in_syscall(
            PeerSyscall::Sleeping,
            &[]
        ));
        // The kernel's `syscall` parser never answers `Sleeping`.
        assert_ne!(parse_proc_syscall(b"S\n"), PeerSyscall::Sleeping);
    }


    /// JIT round 17 wave 6 (lane eintr2): the Linux arm asks `stat` only
    /// after an unreadable `syscall`, behind its switch. Source pin: the read
    /// needs a host without the file.
    #[test]
    fn eintr2_the_stat_fallback_runs_only_after_a_failed_syscall_read() {
        let src = include_str!("../xt_root_scan.rs").replace("\r\n", "\n");
        let linux = src
            .split("// Linux x86-64 implementation (signal rendezvous)\n")
            .nth(1)
            .expect("the Linux arm's banner");
        let body = linux
            .split("fn peer_syscall_state(tid: u32) -> super::PeerSyscall {")
            .nth(1)
            .and_then(|s| s.split("fn peer_blocks_takeover_signal(").next())
            .expect("linux peer_syscall_state");
        let first = body.find("b\"/syscall\\0\"").expect("reads syscall");
        let gate = body
            .find("super::takeover_stat_fallback_enabled()")
            .expect("behind its switch");
        let second = body.find("b\"/stat\\0\"").expect("reads stat");
        assert!(first < gate && gate < second);
    }

    /// JIT round 17 wave 1 (lane eintr): the Linux take-over consults the
    /// kernel before it arms a slot, on every pass, behind its switch; and the
    /// helper-window pass signals each OS thread once. Source pins: the passes
    /// need live peers and signals to run.
    #[test]
    fn eintr17_the_linux_passes_skip_in_syscall_peers_and_dedupe_blocked_tids() {
        let src = include_str!("../xt_root_scan.rs").replace("\r\n", "\n");
        let linux = src
            .split("// Linux x86-64 implementation (signal rendezvous)\n")
            .nth(1)
            .expect("the Linux arm's banner");
        let pass = linux
            .split("pub fn take_over_pass<F>(")
            .nth(1)
            .and_then(|s| s.split("pub fn resume(").next())
            .expect("linux take_over_pass");
        let check = pass
            .find("takeover_signal_skips_peer_in_syscall(peer_syscall_state(tid)")
            .expect("the pass reads the peer's syscall state");
        let arm = pass.find("arm_slot(tid, false)").expect("arms a slot");
        assert!(check < arm, "the kernel is read before any slot is armed");
        assert!(pass.contains("super::takeover_skip_in_syscall_enabled()"));
        let hw = linux
            .split("pub fn helper_window_pass<F>(")
            .nth(1)
            .expect("linux helper_window_pass");
        let dedup = hw.find("blocked_once.dedup();").expect("dedup");
        let first_arm = hw.find("arm_slot(tid, true)").expect("arms a helper slot");
        assert!(dedup < first_arm);
    }

    /// g1ep7 (`CRATONVM_XT_HELPER_PRESIGNAL`): the Linux helper-window pass
    /// signals its peers before it waits on the first one, behind its switch,
    /// and the loop takes a pre-armed slot instead of arming another. Source
    /// pins: the pass needs live blocked peers and signals to run.
    #[test]
    fn g1ep7_the_helper_window_pass_signals_every_peer_before_waiting() {
        let src = include_str!("../xt_root_scan.rs").replace("\r\n", "\n");
        let linux = src
            .split("// Linux x86-64 implementation (signal rendezvous)\n")
            .nth(1)
            .expect("the Linux arm's banner");
        let hw = linux
            .split("pub fn helper_window_pass<F>(")
            .nth(1)
            .and_then(|s| s.split("\n    }\n").next())
            .expect("linux helper_window_pass");
        let gate = hw.find("true").expect("the switch");
        let signal = hw.find("presignalled.push((tid, slot, preread))").expect("the pre-signal");
        let wait = hw.find("wait_for_response_retrying(slot, tid)").expect("the wait");
        assert!(gate < signal && signal < wait, "every peer is signalled before the first wait");
        assert!(hw.contains("let parked = if let Some(early) = early {"));
    }

    /// JIT round 17 wave 2 (lane ffm3, EINTR17-1): the `SigBlk` line of a
    /// task's `status` names the blocked signals, bit `sig - 1`.
    #[test]
    fn ffm3_status_sigblk_lines_parse() {
        let usr2 = 12;
        let blocked = b"Name:\tjava\nSigQ:\t0/63432\nSigPnd:\t0000000000000000\n\
ShdPnd:\t0000000000000000\nSigBlk:\t0000000000000800\nSigIgn:\t0000000000001000\n";
        assert_eq!(parse_status_sigblk(blocked, usr2), Some(true));
        assert_eq!(parse_status_sigblk(blocked, 10), Some(false), "SIGUSR1");
        let open = b"SigPnd:\t0000000000000800\nSigBlk:\t0000000000000000\n";
        assert_eq!(
            parse_status_sigblk(open, usr2),
            Some(false),
            "pending is not blocked"
        );
        assert_eq!(
            parse_status_sigblk(b"SigBlk:\tfffffffe7ffbfeff", usr2),
            Some(true)
        );
        assert_eq!(parse_status_sigblk(b"Name:\tjava\n", usr2), None, "no line");
        assert_eq!(parse_status_sigblk(b"SigBlk:\tzz\n", usr2), None, "not hex");
        assert_eq!(parse_status_sigblk(b"SigBlk:\t\n", usr2), None, "empty");
        assert_eq!(parse_status_sigblk(blocked, 0), None, "no signal 0");
    }

    /// JIT round 17 wave 2 (lane ffm3, EINTR17-1): the Linux take-over reads
    /// the peer's mask after the syscall read and before it arms a slot, and
    /// hands the switch to its wait. Source pins.
    #[test]
    fn ffm3_the_linux_take_over_skips_masked_peers_before_arming() {
        let src = include_str!("../xt_root_scan.rs").replace("\r\n", "\n");
        let linux = src
            .split("// Linux x86-64 implementation (signal rendezvous)\n")
            .nth(1)
            .expect("the Linux arm's banner");
        let pass = linux
            .split("pub fn take_over_pass<F>(")
            .nth(1)
            .and_then(|s| s.split("pub fn resume(").next())
            .expect("linux take_over_pass");
        let syscall = pass
            .find("takeover_signal_skips_peer_in_syscall(peer_syscall_state(tid)")
            .expect("the syscall read");
        let mask = pass
            .find("mask_door && peer_blocks_takeover_signal(tid) == Some(true)")
            .expect("the mask read");
        let arm = pass.find("arm_slot(tid, false)").expect("arms a slot");
        assert!(syscall < mask && mask < arm);
        assert!(pass.contains("wait_for_response_retrying_with(slot, tid, mask_door)"));
        assert!(pass.contains("super::takeover_mask_door_enabled()"));
    }

    /// gcd d10/t (`gcd-d9c-...` item 2): a blocked peer with no band word read
    /// and no JIT address in its registers is UNREADABLE, not native.
    #[test]
    fn a_blocked_peer_without_a_band_is_unreadable_not_native() {
        assert_eq!(blocked_peer_read(true, 0), BlockedPeerRead::Window);
        assert_eq!(blocked_peer_read(true, 12), BlockedPeerRead::Window);
        assert_eq!(blocked_peer_read(false, 12), BlockedPeerRead::NativeOnly);
        assert_eq!(blocked_peer_read(false, 0), BlockedPeerRead::Unreadable);
    }

    /// gcg g3/h (`CRATONVM_XT_HELPER_WINDOW_ROOT_STARTS`): only the entries
    /// from `from` on are judged; the rejected ones are reported in order and
    /// removed, the kept ones keep their order, and the prefix is untouched.
    #[test]
    fn gcg_g3h_root_starts_judge_only_the_helper_window_tail() {
        let addrs = [0x1000usize, 0x2008, 0x3000, 0x3010, 0x4000, 0x4018];
        let mut roots: Vec<ObjectRef> = addrs.iter().filter_map(|&a| fake_obj(a)).collect();
        assert_eq!(roots.len(), addrs.len());
        // "Starts" are the 0x1000-aligned addresses; 0x2008 sits in the prefix.
        let is_start = |a: usize| a & 0xfff == 0;
        let mut dropped = Vec::new();
        let n = retain_root_starts_from(&mut roots, 2, is_start, |a| dropped.push(a));
        assert_eq!(n, 2);
        assert_eq!(dropped, vec![0x3010, 0x4018]);
        let left: Vec<usize> = roots.iter().map(|o| o.as_ptr() as usize).collect();
        assert_eq!(left, vec![0x1000, 0x2008, 0x3000, 0x4000]);
    }

    /// gcg g3/h: an empty or out-of-range tail removes nothing and reports
    /// nothing; a tail with no start is emptied.
    #[test]
    fn gcg_g3h_root_starts_edge_tails() {
        let mut roots: Vec<ObjectRef> = [0x1008usize, 0x2008]
            .iter()
            .filter_map(|&a| fake_obj(a))
            .collect();
        let mut seen = 0usize;
        assert_eq!(
            retain_root_starts_from(&mut roots, 2, |_| false, |_| seen += 1),
            0
        );
        assert_eq!(
            retain_root_starts_from(&mut roots, 9, |_| false, |_| seen += 1),
            0
        );
        assert_eq!(seen, 0);
        assert_eq!(roots.len(), 2);
        assert_eq!(
            retain_root_starts_from(&mut roots, 0, |_| true, |_| seen += 1),
            0
        );
        assert_eq!(roots.len(), 2);
        assert_eq!(
            retain_root_starts_from(&mut roots, 0, |_| false, |_| seen += 1),
            2
        );
        assert_eq!(seen, 2);
        assert!(roots.is_empty());
    }

    /// gcg g3/h: the switch is applied by the helper-window caller AFTER the
    /// pins are taken, and only on Generational -- a source pin, because the
    /// G1 marking roots are also its pin set (`pin_frozen_peer_roots_for_g1`).
    #[test]
    fn gcg_g3h_root_starts_are_generational_only_and_never_touch_pins() {
        let src = include_str!("../xt_root_scan.rs");
        let body = src
            .split("pub fn helper_window_keep_root_starts(")
            .nth(1)
            .expect("helper_window_keep_root_starts");
        let body = &body[..body.find("\n}\n").expect("end of fn")];
        assert!(
            body.contains("if !heap.is_generational()"),
            "Generational only"
        );
        assert!(
            !body.contains("add_xt_cycle_pinned_jit_roots"),
            "the starts filter narrows marking roots, never the pin set"
        );
    }

    /// gcg g5/xt: the windows-only capture switch was deleted on vg4's
    /// measurement (every repaired word came from a pure-native band); no
    /// reader of it may come back, and the recorder records every capture.
    #[test]
    fn gcg_g5xt_pure_native_band_captures_are_always_recorded() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        let key = concat!("CRATON", "VM_XT_HELPER_CAPTURE_WINDOWS_ONLY");
        assert!(
            !prod.contains(key),
            "a reader of the deleted switch is back"
        );
        let body = prod
            .split("fn record_helper_stack_captures(")
            .nth(1)
            .expect("record_helper_stack_captures");
        let body = &body[..body.find("\n}\n").expect("end of fn")];
        assert!(
            !body.contains("windows_only"),
            "no population filter in the recorder"
        );
        assert!(
            body.contains("for &(at, w) in pending"),
            "every capture is recorded"
        );
    }

    /// gcg g4/xt: `record_helper_stack_captures` is the ONLY production
    /// caller of `record_peer_stack_slot` in this file, so neither helper
    /// arm can capture a band word before `has_jit` is known again. A source
    /// pin (LF line endings assumed, as the g3/h pin).
    #[test]
    fn gcg_g4xt_helper_captures_have_one_recording_site() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        let needle = concat!("gc_quiescence::", "record_peer_stack_slot(");
        assert_eq!(prod.matches(needle).count(), 1, "one recording site");
        let body = prod
            .split("fn record_helper_stack_captures(")
            .nth(1)
            .expect("record_helper_stack_captures");
        let body = &body[..body.find("\n}\n").expect("end of fn")];
        assert!(
            body.contains(needle),
            "the one site is the deferred recorder"
        );
    }

    /// gcg g4/xt (`gcg-g3h-...-review-findings` item 4): both Linux arms
    /// record EVERY peer register through `record_peer_reg` before the object
    /// filter, as the Windows arms do -- the call also puts young-region
    /// register words (slack words included) into the young pin ledger. A
    /// source pin over the two Linux functions.
    #[test]
    fn gcg_g4xt_linux_arms_record_every_peer_register() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        for (fn_head, rec) in [
            (
                "fn scan_slot_with_regions<F, G>(",
                concat!("record_peer_reg(", "pair_tid, ri as u8, v)"),
            ),
            (
                "fn classify_parked_snapshot<F>(",
                concat!("record_peer_reg(", "pair_tid_hw, ri_hw as u8, v)"),
            ),
        ] {
            let body = prod.split(fn_head).nth(1).expect(fn_head);
            let at_rec = body.find(rec).expect("register record");
            let at_filter = body.find("is_obj(v)").expect("register filter");
            assert!(
                at_rec < at_filter,
                "{fn_head}: record before the object filter"
            );
        }
    }

    /// gcg g5/xt: the take-over's innermost-frame scope (g1/f) was deleted on
    /// lane jm's g5 verdict; no reader of its switch and no second probe for
    /// a frozen peer's stack may come back. (gci i1/fr's opt-in
    /// `take_over_pass_split` is not that scope: it splits at the TOP of the
    /// innermost compiled segment, giving the Rust frames above it the
    /// cooperative scans' starts rule; every compiled frame of the innermost
    /// segment keeps the full probe.)
    #[test]
    fn gcg_g5xt_takeover_probes_every_stack_word_alike() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        let key = concat!("CRATON", "VM_XT_TAKEOVER_INTERIOR_INNERMOST");
        assert!(
            !prod.contains(key),
            "a reader of the deleted switch is back"
        );
        assert!(
            !prod.contains(concat!("is_obj", "_outer")),
            "a second take-over probe is back"
        );
        assert!(!prod.contains(concat!("fn take_over_pass", "_scoped")));
    }

    /// gch h3/yc: the `[xt-frame]` census walks a frozen peer's RBP chain,
    /// innermost first, and stops at the first return into code that is not
    /// compiled, at a record that does not climb or leaves the band, and at
    /// its frame budget.
    #[test]
    fn gch_h3yc_frozen_frame_chain_follows_the_records_and_stops_where_it_must() {
        // Code: [0x1000, 0x2000) has 0x40-byte frames, [0x3000, 0x4000) 0x80.
        let size_at = |pc: usize| match pc {
            0x1000..=0x1fff => Some(0x40),
            0x3000..=0x3fff => Some(0x80),
            _ => None,
        };
        // Records: 0x7000 -> (0x7100, ret 0x3010) -> (0x7200, ret 0x1020)
        // -> (0x7300, ret 0x9000: not compiled).
        let records = |at: usize| match at {
            0x7000 => Some((0x7100, 0x3010)),
            0x7100 => Some((0x7200, 0x1020)),
            0x7200 => Some((0x7300, 0x9000)),
            _ => None,
        };
        let band = (0x6000, 0x8000);
        let frames = frozen_frame_chain(0x1004, 0x7000, band, 64, size_at, records);
        let got: Vec<(usize, usize, usize, usize)> = frames
            .iter()
            .map(|f| (f.depth, f.rbp, f.pc, f.lo))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, 0x7000, 0x1004, 0x6fc0),
                (1, 0x7100, 0x3010, 0x7080),
                (2, 0x7200, 0x1020, 0x71c0)
            ],
        );
        // A frozen RIP outside compiled code: no frame at all.
        assert!(frozen_frame_chain(0x9000, 0x7000, band, 64, size_at, records).is_empty());
        // The budget.
        assert_eq!(
            frozen_frame_chain(0x1004, 0x7000, band, 2, size_at, records).len(),
            2
        );
        // A record outside the band, misaligned, or not climbing ends the walk.
        assert!(frozen_frame_chain(0x1004, 0x7ff8, band, 64, size_at, records).is_empty());
        assert!(frozen_frame_chain(0x1004, 0x7004, band, 64, size_at, records).is_empty());
        let falling = |at: usize| (at == 0x7000).then_some((0x6f00, 0x1010));
        assert_eq!(
            frozen_frame_chain(0x1004, 0x7000, band, 64, size_at, falling).len(),
            1
        );
    }

    /// gci i1/fr (`gcg-g3h-...` item 1): only a window whose peer published a
    /// depth of exactly zero is demoted; an unknown depth (`None`) and any
    /// positive depth keep it, and a non-window stays one.
    #[test]
    fn gci_i1fr_a_zero_depth_window_is_native_and_an_unknown_depth_is_not() {
        assert!(zero_depth_window_is_native(true, Some(0)));
        assert!(!zero_depth_window_is_native(true, None), "unknown depth keeps the window");
        assert!(!zero_depth_window_is_native(true, Some(1)));
        assert!(!zero_depth_window_is_native(true, Some(7)));
        assert!(!zero_depth_window_is_native(false, Some(0)), "a native band stays native");
        assert!(!zero_depth_window_is_native(false, None));
        // The arm's step: a non-window is never promoted, and a tid that never
        // published a depth keeps its window with the rule applied.
        assert!(!helper_window_after_depth_check(0xFFFF_FF01, false, true));
        for apply in [false, true] {
            assert_eq!(helper_window_after_depth_check(0xFFFF_FF01, true, apply), !jit_entry_publication_on());
        }
    }

    /// gci i20/vh: only a window whose peer's deposit deferred its chain is
    /// demoted; a native band stays native, and a window without a record
    /// keeps its window.
    #[test]
    fn gci_i20vh_a_deferred_chain_window_is_native_and_nothing_else_moves() {
        assert!(deferred_chain_window_is_native(true, true));
        assert!(!deferred_chain_window_is_native(true, false), "no record: the window stays");
        assert!(!deferred_chain_window_is_native(false, true), "a native band stays native");
        assert!(!deferred_chain_window_is_native(false, false));
        assert!(!helper_window_deferred_chain_step(0xFFFF_FD01, true, true));
        assert!(helper_window_deferred_chain_step(0xFFFF_FD01, true, false));
        assert!(!helper_window_deferred_chain_step(0xFFFF_FD01, false, true));
        assert!(!helper_window_deferred_chain_step(0xFFFF_FD01, false, false));
    }

    /// gci i20/vh: a default `TakenOver` demotes nothing; the driver's list is
    /// sorted and deduplicated and answered by membership.
    #[test]
    fn ki3_a_covered_peers_window_is_pinned_but_not_committed() {
        // SAFETY: never dereferenced; addresses only.
        let fake = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
        let mut taken = TakenOver::default();
        let mut roots: Vec<ObjectRef> = Vec::new();
        // Nothing handed in: every window commits, as before.
        let mut c = vec![fake(0x1000), fake(0x2000)];
        taken.commit_window_roots(42, &mut c, &mut roots);
        assert!(c.is_empty());
        assert_eq!(roots.len(), 2);
        assert!(taken.take_pin_only_roots().is_empty());
        taken.set_covered_chain_peers(vec![42, 7, 42]);
        // A covered peer's words go aside, not into the marking roots.
        let mut c = vec![fake(0x3000)];
        taken.commit_window_roots(42, &mut c, &mut roots);
        assert!(c.is_empty());
        assert_eq!(roots.len(), 2);
        // Another peer still commits.
        let mut c = vec![fake(0x4000)];
        taken.commit_window_roots(8, &mut c, &mut roots);
        assert_eq!(roots.len(), 3);
        let aside = taken.take_pin_only_roots();
        assert_eq!(aside.len(), 1);
        assert_eq!(aside[0].as_ptr() as usize, 0x3000);
        assert!(taken.take_pin_only_roots().is_empty(), "taken once");
        // Never a demotion: the deferred-chain roster is separate.
        assert!(!taken.demotes_deferred_chain_window(42));
    }


    #[test]
    fn gci_i20vh_taken_over_answers_the_deferred_chain_peers_it_was_handed() {
        let mut taken = TakenOver::default();
        assert!(!taken.demotes_deferred_chain_window(0));
        assert!(!taken.demotes_deferred_chain_window(42));
        taken.set_deferred_chain_peers(vec![42, 7, 42, 19]);
        assert!(taken.demotes_deferred_chain_window(7));
        assert!(taken.demotes_deferred_chain_window(19));
        assert!(taken.demotes_deferred_chain_window(42));
        assert!(!taken.demotes_deferred_chain_window(8));
        taken.set_deferred_chain_peers(Vec::new());
        assert!(!taken.demotes_deferred_chain_window(42));
        // Nothing was suspended: dropping it resumes nothing.
        taken.resumed = true;
    }

    /// The deferred-chain rule remains enabled after its switch was retired.
    #[test]
    fn gci_i24jp_the_deferred_chain_switch_is_default_on() {
        assert!(helper_window_deferred_chain_native_enabled());
    }

    /// gci i24/jp: the default flip carries its greppable apps-triage marker
    /// on the reader, and the reader is the default-ON form (a source pin; LF
    /// line endings assumed).
    #[test]
    fn gci_i24jp_the_deferred_chain_flip_is_marked_on_its_reader() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        let head = concat!("pub fn helper_window_deferred_chain_native_enabled", "() -> bool {");
        let at = prod.find(head).expect("the reader");
        let body = &prod[at..at + prod[at..].find("\n}\n").expect("the reader's end")];
        assert!(body.contains(concat!("WARNING gci-i24jp-flip", "-20261015")), "{body}");
        assert!(body.trim_end().ends_with("true"), "{body}");
        assert!(!body.contains(concat!("runtime_flag_on", "(")), "{body}");
    }

    /// gci i25/rv (`gci-i25rv-i24-default-path-review-findings-FIXED-20261016` item
    /// 9): the comments that say why the helper-window pass runs no longer
    /// restate the premise "a blocked peer's deposit never scans the JIT band"
    /// (false since `24cf0b19b0`; the i24/jp flip's soundness argument rests on
    /// the opposite), and the argument's point 2 names both families of
    /// blocking entries. Source pins, CRLF-tolerant.
    #[test]
    fn gci_i25rv_the_blocked_deposit_premise_is_stated_as_it_is() {
        let read = |rel: &str| {
            let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
            std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{path}: {e}"))
                .replace("\r\n", "\n")
        };
        let own = read("src/jit/xt_root_scan.rs");
        assert!(!own.contains(concat!(
            "coverage gap (deposit_root_snapshot\n",
            "            // never scans the JIT band)"
        )));
        assert!(own.contains(concat!("entered by a flag-raising deposit in one", " of two families")));
        assert!(own.contains(concat!("`monitor_enter_synchronized_method_contended`", "),")));
        let read_parts = |dir: &str| {
            let path = format!("{}/{dir}", env!("CARGO_MANIFEST_DIR"));
            let mut files: Vec<_> = std::fs::read_dir(path).unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.file_name().and_then(|name| name.to_str())
                    .is_some_and(|name| name == "mod.rs" || (name.starts_with("part_") && name.ends_with(".rs"))))
                .collect();
            files.sort();
            files.iter().map(|path| std::fs::read_to_string(path).unwrap()).collect::<String>().replace("\r\n", "\n")
        };
        let registry = read_parts("src/threading/thread_registry");
        assert!(!registry.contains(concat!(
            "only by `deposit_root_snapshot` — which never scans the JIT band",
            " on"
        )));
        let gc = read_parts("src/runtime/interpreter/gc_and_alloc");
        assert!(!gc.contains(concat!(
            "covered only by its `deposit_root_snapshot`, which\n",
            "    // never scans JIT frames)"
        )));
        assert!(!gc.contains(concat!(
            "`CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`, opt-in,\n",
            "        // G1 only)"
        )));
    }

    /// gci i20/vh: every helper-window depth step (one Windows site, two
    /// Linux sites) goes through the deferred-chain step with the take-over's
    /// answer for the same peer. A source pin (LF line endings assumed).
    #[test]
    fn gci_i20vh_every_helper_depth_step_applies_the_deferred_chain_step() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        let step = concat!("super::helper_window_deferred_chain_step", "(");
        assert_eq!(prod.matches(step).count(), 3, "one Windows site, two Linux sites");
        let answer = concat!("taken.demotes_deferred_chain_window", "(tid)");
        assert_eq!(prod.matches(answer).count(), 6);
        let check = concat!("super::helper_window_after_depth_check", "(");
        for (at, _) in prod.match_indices(step) {
            let depth = prod[..at].rfind(check).expect("a depth check before the step");
            let tail = &prod[at..];
            let call = &tail[..tail.find(");").expect("the step ends")];
            assert!(depth < at && call.contains("after_depth"));
            assert!(call.contains(answer), "the step uses the same peer's answer");
        }
    }

    /// gci i1/fr: both helper-window arms run the depth check -- the Windows
    /// arm once, after its band loop and before the captures are recorded and
    /// the unreadable test; the Linux arm after each `classify_parked_snapshot`
    /// of the pass (the first read and the re-park) -- and the switch is read
    /// once per pass, not per peer. A source pin (LF line endings assumed).
    #[test]
    fn gci_i1fr_both_helper_arms_apply_the_zero_depth_check_before_the_unreadable_test() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        let call = concat!("super::helper_window_after_depth_check", "(");
        assert_eq!(prod.matches(call).count(), 3, "one Windows site, two Linux sites");
        let reader = concat!("super::helper_window_zero_depth_native_enabled", "()");
        assert_eq!(prod.matches(reader).count(), 2, "one read per arm's pass");
        // Windows: the check precedes the recorder and the unreadable test.
        let win = prod
            .split("mod imp {")
            .nth(1)
            .expect("the Windows arm")
            .split("pub fn helper_window_pass<F>(")
            .nth(1)
            .expect("its helper_window_pass");
        let at_check = win.find(call).expect("Windows check");
        let at_record = win.find("super::record_helper_stack_captures(tid, has_jit").expect("rec");
        let at_unreadable = win.find("super::blocked_peer_read(has_jit").expect("unreadable test");
        assert!(at_check < at_record && at_record < at_unreadable);
    }

    /// gci i2/fr (`gci-i1fr-...-review-findings` item 2, and item 1 of
    /// `gci-i2fr-...-review-findings`): the Linux classifier neither prints a
    /// census line nor records its captures; the pass does both through
    /// `ParkedBandTail::finish` after each depth step (first read, then the
    /// re-park) and before the unreadable test, as Windows does. A source pin
    /// (LF line endings assumed).
    #[test]
    fn gci_i2fr_the_linux_helper_census_prints_after_the_depth_step() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        let linux = prod.split("mod imp {").nth(2).expect("the Linux arm");
        let classify = linux
            .split("fn classify_parked_snapshot<F>(")
            .nth(1)
            .expect("the classifier")
            .split("\n    }\n")
            .next()
            .expect("its body");
        assert!(!classify.contains("eprintln!"), "the classifier prints no census");
        let recorder = concat!("record_helper_stack_captures", "(");
        assert!(!classify.contains(recorder), "the classifier records no capture");
        let call = concat!("super::helper_window_after_depth_check", "(");
        let finish = concat!("tail.finish", "(has_jit);");
        let checks: Vec<usize> = linux.match_indices(call).map(|(i, _)| i).collect();
        let finishes: Vec<usize> = linux.match_indices(finish).map(|(i, _)| i).collect();
        assert_eq!(checks.len(), 2, "two Linux depth steps");
        assert_eq!(finishes.len(), 2, "two Linux tails finished");
        assert!(checks[0] < finishes[0] && finishes[0] < checks[1] && checks[1] < finishes[1]);
        let unreadable = linux.find("super::blocked_peer_read(has_jit").expect("unreadable test");
        assert!(finishes[0] < unreadable, "the first tail finishes before the unreadable test");
    }

    /// gci i1/fr (`gcf-f7f-...` kind A): the innermost segment's top is the
    /// `rbp + 16` of the last frame `frozen_frame_chain` finds -- the same
    /// walk, with nothing collected -- and `None` where that walk finds none.
    #[test]
    fn gci_i1fr_the_innermost_segment_top_is_the_frame_chain_s_last_record() {
        let size_at = |pc: usize| match pc {
            0x1000..=0x1fff => Some(0x40),
            0x3000..=0x3fff => Some(0x80),
            _ => None,
        };
        let records = |at: usize| match at {
            0x7000 => Some((0x7100, 0x3010)),
            0x7100 => Some((0x7200, 0x1020)),
            0x7200 => Some((0x7300, 0x9000)),
            _ => None,
        };
        let band = (0x6000, 0x8000);
        for (rip, rbp, max) in [
            (0x1004, 0x7000, 64),
            (0x1004, 0x7000, 2),
            (0x1004, 0x7000, 1),
            (0x9000, 0x7000, 64),
            (0x1004, 0x7ff8, 64),
            (0x1004, 0x7004, 64),
            (0x1004, 0x5000, 64),
        ] {
            let chain = frozen_frame_chain(rip, rbp, band, max, size_at, records);
            let want = chain.last().map(|f| f.rbp + 16);
            assert_eq!(
                takeover_innermost_segment_top(rip, rbp, band, max, size_at, records),
                want,
                "rip={rip:#x} rbp={rbp:#x} max={max}"
            );
        }
        // The fixture's whole chain: three frames, the outermost at 0x7200.
        assert_eq!(
            takeover_innermost_segment_top(0x1004, 0x7000, band, 64, size_at, records),
            Some(0x7210)
        );
        // zwr: one frame (ZGC's split) is the innermost frame's top.
        assert_eq!(
            takeover_innermost_segment_top(0x1004, 0x7000, band, 1, size_at, records),
            Some(0x7010)
        );
        // A frozen `rip` outside compiled code: no segment, the full probe.
        assert_eq!(
            takeover_innermost_segment_top(0x9000, 0x7000, band, 64, size_at, records),
            None
        );
    }

    /// gci i1/fr: `take_over_pass` is `take_over_pass_split` with no second
    /// probe on both OS arms, and both stack loops pick the `above` probe only
    /// at or above the segment top they computed. Source pins (LF assumed).
    #[test]
    fn gci_i1fr_both_take_over_arms_split_only_above_the_innermost_segment() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        let delegate =
            concat!("take_over_pass_split::<F, F>(taken, is_obj, ", "None, roots, live_tids)");
        assert_eq!(prod.matches(delegate).count(), 2, "both arms delegate");
        assert_eq!(
            prod.matches(concat!("Some(from)) if p ", ">= from => above_probe(w)")).count(),
            1,
            "Windows stack loop"
        );
        assert_eq!(
            prod.matches(concat!("Some(from)) if at ", ">= from => above_probe(w)")).count(),
            1,
            "Linux stack loop"
        );
        // Registers are never split: both register loops still call `is_obj`.
        assert_eq!(prod.matches(concat!("super::takeover_innermost_segment_top", "(")).count(), 2);
        let probe = prod
            .split("pub(crate) fn takeover_above_chain_word_probe(")
            .nth(1)
            .expect("the above-chain probe");
        let probe = &probe[..probe.find("\n}\n").expect("end of fn")];
        assert!(probe.contains("if heap.is_g1()"), "G1 keeps the full probe");
        assert!(
            probe.contains("return takeover_word_probe(heap, w);"),
            "G1: the full probe"
        );
        assert!(
            probe.contains("return heap.is_object_address(w);"),
            "ZGC: exact bases"
        );
        assert!(!probe.contains("companion"), "no companion above the segment");
    }

    /// The retired zero-depth rule stays off; the above-chain rule stays on.
    #[test]
    fn gci_i1fr_the_zero_depth_and_above_chain_switch_defaults() {
        assert!(!helper_window_zero_depth_native_enabled());
        assert!(takeover_above_chain_starts_enabled());
    }

    /// zwr: a published floor inside the band ends it there; none, one at or
    /// below `rsp`, or one past the end leaves the whole band.
    #[test]
    fn zwr_a_rust_band_floor_caps_only_inside_the_band() {
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0x5000), 0x5000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0), 0x9000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0x1000), 0x9000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0x0800), 0x9000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0x9000), 0x9000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0xa000), 0x9000);
    }

    /// The ZGC arm of the above-chain rule stays enabled.
    #[test]
    fn zwr_the_zgc_above_chain_switch_is_default_on() {
        assert!(takeover_above_chain_starts_zgc_enabled());
    }

    /// gci i3/tk (`gci-i2yg-refusing-helper-windows-...` fix 2): only a
    /// window whose peer published neither a depth nor a shadow window, while
    /// every JIT entry publishes both, is demoted.
    #[test]
    fn gci_i3tk_an_unpublished_window_is_native_only_when_every_entry_publishes() {
        assert!(unpublished_window_is_native(true, None, false, true));
        assert!(!unpublished_window_is_native(true, None, false, false), "a kill switch: proves nothing");
        assert!(!unpublished_window_is_native(true, None, true, true), "a published window keeps it");
        assert!(!unpublished_window_is_native(true, Some(0), false, true), "zero is the other rule's");
        assert!(!unpublished_window_is_native(true, Some(3), false, true));
        assert!(!unpublished_window_is_native(false, None, false, true), "a native band stays native");
    }

    /// gci i3/tk: the arm step demotes a never-published peer only with the
    /// switch on (and only while every entry publishes), and keeps the window
    /// of a peer that published a shadow window.
    #[test]
    fn gci_i3tk_the_depth_step_applies_the_unpublished_rule_under_its_switch() {
        use cratonvm_types::flags::with_thread_overrides;
        let up = concat!("CRATON", "VM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE");
        const NEVER: u32 = 0xFFFF_FE21;
        const SHADOWED: u32 = 0xFFFF_FE22;
        let dflt = with_thread_overrides(&[(up, None)], || {
            helper_window_after_depth_check(NEVER, true, false)
        });
        assert_eq!(dflt, !jit_entry_publication_on(), "default ON: demoted iff publication is on");
        let on = with_thread_overrides(&[(up, Some("1"))], || {
            helper_window_after_depth_check(NEVER, true, false)
        });
        assert_eq!(on, !jit_entry_publication_on(), "switch on: demoted iff publication is on");
        let a = Box::new([0usize; 4]);
        let base = a.as_ptr() as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(SHADOWED, base, base, base + 32);
        let kept = with_thread_overrides(&[(up, Some("1"))], || {
            helper_window_after_depth_check(SHADOWED, true, false)
        });
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(SHADOWED);
        assert!(kept, "a published shadow window keeps the window");
        let native = with_thread_overrides(&[(up, Some("1"))], || {
            helper_window_after_depth_check(NEVER, false, false)
        });
        assert!(!native, "a native band stays native");
    }

    /// gci i3/tk: both promotion licences are Generational only and need the
    /// derived-pointer probe of their population; the starts-only marking
    /// rules (and, for the take-over, a dead peer shadow scan) refuse.
    #[test]
    fn gci_i3tk_promotion_licences_are_generational_and_need_the_derived_pointer_probes() {
        assert!(takeover_promotes_licence(true, true, true, false, true));
        assert!(!takeover_promotes_licence(false, true, true, false, true), "G1 / ZGC");
        assert!(!takeover_promotes_licence(true, false, true, false, true), "switch off");
        assert!(!takeover_promotes_licence(true, true, false, false, true), "exact-base probe");
        assert!(!takeover_promotes_licence(true, true, true, true, true), "above-chain starts");
        assert!(!takeover_promotes_licence(true, true, true, false, false), "shadow scan killed");
        assert!(helper_refusal_promotes_licence(true, true, true, false));
        assert!(!helper_refusal_promotes_licence(false, true, true, false), "G1 / ZGC");
        assert!(!helper_refusal_promotes_licence(true, false, true, false), "switch off");
        assert!(!helper_refusal_promotes_licence(true, true, false, false), "pin resolve off");
        assert!(!helper_refusal_promotes_licence(true, true, true, true), "root starts");
        assert!(helper_window_refusal_relieved(true, 0, 0));
        assert!(!helper_window_refusal_relieved(false, 0, 0));
        assert!(!helper_window_refusal_relieved(true, 1, 0), "a cut band");
        assert!(!helper_window_refusal_relieved(true, 0, 1), "an unreadable peer");
    }

    /// gci i3/tk: the pause-wide licence. Every fact that leaves a word
    /// unread refuses; both switches off never keep promotion.
    #[test]
    fn gci_i3tk_a_pause_keeps_promotion_only_under_its_whole_licence() {
        let frozen = PausePromoteFacts {
            frozen: 1,
            takeover_licensed: true,
            ..PausePromoteFacts::default()
        };
        assert!(pause_keeps_promotion(&frozen));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { takeover_licensed: false, ..frozen }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { stack_incomplete: true, ..frozen }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts {
            takeover_shadow_untrusted: 1,
            ..frozen
        }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { unclassified: 1, ..frozen }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { native_claims: 1, frozen: 2, ..frozen }));
        // A frozen peer beside a refusing helper window: both licences needed.
        let both = PausePromoteFacts { helper_windows: 2, helper_refusals: 1, ..frozen };
        assert!(!pause_keeps_promotion(&both), "helper not licensed");
        assert!(!pause_keeps_promotion(&PausePromoteFacts { helper_licensed: true, ..both }));
        assert!(pause_keeps_promotion(&PausePromoteFacts {
            helper_licensed: true,
            helper_relieved: true,
            ..both
        }));
        // Helper windows alone.
        let hw = PausePromoteFacts {
            helper_windows: 3,
            helper_refusals: 3,
            helper_licensed: true,
            helper_relieved: true,
            ..PausePromoteFacts::default()
        };
        assert!(pause_keeps_promotion(&hw));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { helper_relieved: false, ..hw }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { helper_licensed: false, ..hw }));
        // An unreadable peer and no window (the pass never relieves it).
        assert!(!pause_keeps_promotion(&PausePromoteFacts {
            helper_windows: 0,
            helper_relieved: false,
            ..hw
        }));
        // Neither switch: never.
        assert!(!pause_keeps_promotion(&PausePromoteFacts {
            frozen: 1,
            helper_windows: 1,
            ..PausePromoteFacts::default()
        }));
    }

    /// The retired promotion switches stay off and the unpublished rule stays on.
    #[test]
    fn gci_i3tk_the_promotion_and_unpublished_switches_are_default_off() {
        use cratonvm_types::flags::with_thread_overrides;
        for (key, read) in [
            (
                concat!("CRATON", "VM_GEN_TAKEOVER_PROMOTES"),
                gen_takeover_promotes_enabled as fn() -> bool,
            ),
            (
                concat!("CRATON", "VM_GEN_HELPER_WINDOW_REFUSAL_PROMOTES"),
                gen_helper_window_refusal_promotes_enabled,
            ),
        ] {
            assert!(!with_thread_overrides(&[(key, None)], read), "{key} default OFF");
            assert!(!with_thread_overrides(&[(key, Some("0"))], read), "{key}=0 OFF");
            assert!(!with_thread_overrides(&[(key, Some("1"))], read), "{key} remains OFF");
        }
        // gci i4: the never-published rule is default ON, `=0` its kill.
        let key = concat!("CRATON", "VM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE");
        let read = helper_window_unpublished_native_enabled;
        assert!(with_thread_overrides(&[(key, None)], read), "{key} default ON");
        assert!(with_thread_overrides(&[(key, Some("0"))], read), "{key} remains ON");
        assert!(with_thread_overrides(&[(key, Some("1"))], read), "{key} remains ON");
    }

    /// gci i3/tk: every per-peer peer-state mark of both OS arms goes through
    /// the two licence-aware helpers, and only they call the un-rewritable
    /// door. A source pin (LF assumed).
    #[test]
    fn gci_i3tk_both_arms_mark_peer_state_through_the_licence_helpers() {
        let src = include_str!("../xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests;").expect("tests module")];
        let frozen = concat!("super::mark_takeover_peer_frozen", "(taken);");
        let refusal = concat!(
            "super::mark_helper_window_refusal",
            "(taken, unpinned_incomplete, unknown_peers);"
        );
        assert_eq!(prod.matches(frozen).count(), 2, "both take-over arms");
        assert_eq!(prod.matches(refusal).count(), 2, "both helper-window arms");
        let door = concat!("mark_moving_young_coverage_incomplete_because", "(reason);");
        assert_eq!(prod.matches(door).count(), 2, "only the two helpers");
        assert_eq!(
            prod.matches(concat!("incomplete_because", "(\n")).count(),
            0,
            "no direct multi-line call left"
        );
    }

    /// gci i5/tk: a licensed take-over beside helper windows that were ALL
    /// pinned keeps promotion without the helper licence when a discharged
    /// helper-only pause would; a refusal, a missing take-over licence or the
    /// pinned fact absent still refuse, and the pinned fact never engages a
    /// pause by itself.
    #[test]
    fn gci_i5tk_a_licensed_takeover_beside_pinned_windows_keeps_promotion() {
        let f = PausePromoteFacts {
            frozen: 2,
            takeover_licensed: true,
            helper_windows: 3,
            ..PausePromoteFacts::default()
        };
        assert!(pause_keeps_promotion_with(&f, true));
        assert!(!pause_keeps_promotion_with(&f, false), "discharge killed or kept un-rewritable");
        assert!(
            !pause_keeps_promotion_with(&PausePromoteFacts { helper_refusals: 1, ..f }, true),
            "a refusing window still needs the helper licence"
        );
        assert!(
            !pause_keeps_promotion_with(&PausePromoteFacts { takeover_licensed: false, ..f }, true),
            "the take-over licence is still needed"
        );
        assert!(
            !pause_keeps_promotion_with(&PausePromoteFacts { frozen: 0, ..f }, true),
            "pinned windows alone engage nothing"
        );
        assert!(!pause_keeps_promotion_with(&PausePromoteFacts { stack_incomplete: true, ..f }, true));
        assert!(!pause_keeps_promotion_with(&PausePromoteFacts { native_claims: 1, ..f }, true));
        // The retired switches leave the pinned-window policy enabled.
        use cratonvm_types::flags::with_thread_overrides;
        let dis = concat!("CRATON", "VM_XT_HELPER_WINDOW_DISCHARGE");
        let keep = concat!("CRATON", "VM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE");
        assert!(with_thread_overrides(&[(dis, None), (keep, None)], || pause_keeps_promotion(&f)));
        assert!(with_thread_overrides(&[(dis, Some("0")), (keep, None)], || {
            pause_keeps_promotion(&f)
        }));
        assert!(with_thread_overrides(&[(dis, None), (keep, Some("1"))], || {
            pause_keeps_promotion(&f)
        }));
    }

    /// gci i12/fs (`gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`): the
    /// Windows roster audit's per-pass line names its own time and the two
    /// counts, in the grammar the vi12-fs rows grep.
    #[test]
    fn gci_i12fs_roster_audit_line_shape() {
        assert_eq!(
            roster_audit_line(28_467, 5_120, 23),
            "[xt-jit-roots] roster audit: audit_us=28467 walked=5120 off_roster=23"
        );
        assert_eq!(
            roster_audit_line(0, 0, 0),
            "[xt-jit-roots] roster audit: audit_us=0 walked=0 off_roster=0"
        );
    }

    /// gci i12/fs: the Windows take-over pass times the audit around the audit
    /// call itself and prints the line only with the scan-debug flag; the audit
    /// answers its two counts. Source pins: the audit needs a live process
    /// thread table.
    #[test]
    fn gci_i12fs_the_windows_pass_times_the_roster_audit_under_dbg_only() {
        let src = include_str!("../xt_root_scan.rs").replace("\r\n", "\n");
        let windows = src
            .split("#[cfg(windows)]\nmod imp {\n")
            .nth(1)
            .and_then(|s| s.split("// Linux x86-64 implementation (signal rendezvous)\n").next())
            .expect("the Windows arm");
        let gate = windows.find("if roster_audit_enabled() {").expect("the audit gate");
        let t0 = windows.find("let audit_t0 = std::time::Instant::now();").expect("timed");
        let call = windows
            .find("let (walked, off_roster) = audit_roster_covers_jit_peers(")
            .expect("the audit call answers its counts");
        let line = windows
            .find("roster_audit_line(audit_t0.elapsed().as_micros(), walked, off_roster)")
            .expect("the line");
        assert!(gate < t0 && t0 < call && call < line);
        let between = &windows[call..line];
        assert!(between.contains("if dbg() {"), "printed only under the scan-debug flag");
        let audit = windows.find("fn audit_roster_covers_jit_peers(").expect("the audit");
        let body = &windows[audit..];
        let ret = body.find(") -> (u32, u32) {").expect("answers (walked, off_roster)");
        let snap = body.find("CreateToolhelp32Snapshot(").expect("the snapshot");
        assert!(ret < snap);
        assert!(body.contains("return (0, 0);"), "no snapshot answers zero counts");
    }
