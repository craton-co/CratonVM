    use super::*;

    /// The optimizing tier's shape (`spill_hi = callee_saved_lo =
    /// spill_cap_off`, the register tail to the frame's end) over an 8-word
    /// band: locals at off 8 and 16, an operand spill at 24, the tail at
    /// 32..64.
    fn ir_layout() -> cratonvm_jit::FrameLayout {
        cratonvm_jit::FrameLayout {
            java_locals_hi: 16,
            locals_hi: 16,
            spill_lo: 16,
            spill_hi: 32,
            callee_saved_lo: 32,
            callee_saved_hi: 72,
            frame_size: 64,
            ..Default::default()
        }
    }

    /// Eight Generational-heap objects, one per band word, and the band
    /// itself: `objs[i]` sits at `[rbp - (64 - 8 * i)]`, so `objs[5..8]` are
    /// the words at off 24, 16 and 8 (below the floor) and `objs[0..5]` the
    /// tail at off 64 .. 32.
    fn band(heap: &VmHeap) -> ([usize; 8], Box<[usize; 8]>) {
        use crate::classloading::ClassId;
        let mut objs = [0usize; 8];
        for o in objs.iter_mut() {
            *o = heap.alloc_object(ClassId::new(0), 1).as_ptr() as usize;
        }
        (objs, Box::new(objs))
    }

    fn no_claims() -> FrameClaimFacts<'static> {
        FrameClaimFacts {
            map: ActiveMapFacts::NONE,
            ir_prims: None,
            sp_locals: None,
            live_sp: None,
            deopt_residue: false,
            image_rule: false,
            unnamed_dbg: false,
            map_dead_claim: false,
            dead_locals_no_pin: false,
        }
    }

    fn reference_free_ir_method() -> cratonvm_jit::CompiledMethod {
        let mut cm = cratonvm_jit::CompiledMethod::new(
            cratonvm_jit::ExecutableBuffer::new(64)
                .expect("executable buffer alloc must succeed in tests"),
        );
        cm.frame_layout = ir_layout();
        cm.osr_frame_size = 64;
        cm.used_ir_backend = true;
        cm.frame_holds_no_references = true;
        cm
    }

    fn roots_of(out: &[ObjectRef]) -> Vec<usize> {
        out.iter().map(|o| o.as_ptr() as usize).collect()
    }

    /// The floor is `callee_saved_lo` only when every condition holds; each
    /// one withdrawn alone gives `None` (the whole band, as before).
    #[test]
    fn gci_i23rf_the_floor_needs_every_condition() {
        let l = ir_layout();
        assert_eq!(reference_free_band_floor(true, true, true, true, &l, 64), Some(32));
        assert_eq!(reference_free_band_floor(false, true, true, true, &l, 64), None, "switch off");
        assert_eq!(
            reference_free_band_floor(true, false, true, true, &l, 64),
            None,
            "a frame that may hold a reference"
        );
        assert_eq!(
            reference_free_band_floor(true, true, false, true, &l, 64),
            None,
            "not the optimizing tier's contract"
        );
        assert_eq!(
            reference_free_band_floor(true, true, true, false, &l, 64),
            None,
            "a frame recovered by inspection, not on the registered chain"
        );
        let shallow = cratonvm_jit::FrameLayout {
            callee_saved_shallow: true,
            ..l
        };
        assert_eq!(
            reference_free_band_floor(true, true, true, true, &shallow, 64),
            None,
            "a shallow save area is not a half-line"
        );
        // A framed lambda thunk: the flag, but no layout.
        assert_eq!(
            reference_free_band_floor(
                true,
                true,
                true,
                true,
                &cratonvm_jit::FrameLayout::default(),
                64
            ),
            None,
            "no published floor"
        );
        assert_eq!(
            reference_free_band_floor(true, true, true, true, &l, 24),
            None,
            "a floor past the band is malformed"
        );
        assert_eq!(
            reference_free_band_floor(true, true, true, true, &l, 32),
            Some(32),
            "a floor at the band's end cuts the whole band"
        );
    }

    /// The switch is default on (1eb8ed7865): unset and `=1` read on, `=0` off, and the chain
    /// walk's switch set carries it.
/// Through the fused band walk: with the floor, the three words below
    /// `callee_saved_lo` are neither roots nor pins, the register tail is
    /// rooted as before, and the census counts the frame and the words. The
    /// control (no floor) roots all eight.
    #[test]
    fn gci_i23rf_a_floored_frame_roots_only_its_register_tail() {
        use crate::memory::vm_heap::VmHeap;
        use cratonvm_gc::gc_quiescence as q;
        let heap = VmHeap::new(
            crate::memory::vm_heap::GcBackend::Generational,
            16 * 1024 * 1024,
        );
        let (objs, band) = band(&heap);
        let rbp = band.as_ptr() as usize + 64;
        let cm = reference_free_ir_method();
        let before = reference_free_frame_census();
        q::clear_movable_jit_roots();
        q::clear_unrewritable_jit_roots();
        let mut out = Vec::new();
        scan_one_frame_filtered_from(
            rbp,
            64,
            &cm,
            None,
            no_claims(),
            &heap,
            &mut out,
            true,
            0,
            None,
            Some(32),
        );
        let roots = roots_of(&out);
        for (i, o) in objs.iter().enumerate() {
            let off = 64 - 8 * i;
            if off < 32 {
                assert!(!roots.contains(o), "off={off}: below the floor, not a root");
                assert!(
                    !q::is_movable_jit_root(*o) && !q::is_unrewritable_jit_root(*o),
                    "off={off}: nor a pin"
                );
            } else {
                assert!(roots.contains(o), "off={off}: the register tail is still a root");
            }
        }
        let after = reference_free_frame_census();
        assert!(after.0 > before.0, "the frame is counted");
        assert!(after.1 >= before.1 + 3, "the three words below the floor are counted");

        // The control: the same frame without a floor roots every word.
        let mut out = Vec::new();
        scan_one_frame_filtered(rbp, 64, &cm, None, no_claims(), &heap, &mut out, true, 0, None);
        let roots = roots_of(&out);
        for o in objs.iter() {
            assert!(roots.contains(o), "no floor: every band word is a root");
        }
        q::clear_movable_jit_roots();
        q::clear_unrewritable_jit_roots();
        drop(band);
    }

    /// The routing: `scan_one_compiled_frame_with_layout` applies the floor
    /// only with the switch on, only to a reference-free optimizing-tier
    /// frame, and only on the registered chain (`remap_covered`).
    #[test]
    fn gci_i23rf_the_frame_scan_routes_only_a_qualifying_frame_to_the_floor() {
        use crate::memory::vm_heap::VmHeap;
        use cratonvm_gc::gc_quiescence as q;
        let heap = VmHeap::new(
            crate::memory::vm_heap::GcBackend::Generational,
            16 * 1024 * 1024,
        );
        let (objs, band) = band(&heap);
        let rbp = band.as_ptr() as usize + 64;
        let below: Vec<usize> = objs[5..].to_vec();
        // gkr: the dead-locals claim (default ON since gkr) would drop these
        // synthetic band words in every arm; this test is about the floor.
        let on = BandClaimSwitches {
            ref_free_frames: true,
            dead_locals_no_pin: false,
            map_dead_claim: false,
            ..BandClaimSwitches::read()
        };
        let off = BandClaimSwitches {
            ref_free_frames: false,
            ..on
        };
        let scan = |cm: &cratonvm_jit::CompiledMethod, sw: BandClaimSwitches, chain: bool| {
            let mut out = Vec::new();
            assert!(scan_one_compiled_frame_with_layout(
                rbp, cm, &heap, &mut out, chain, 0, sw, None, None
            ));
            roots_of(&out)
        };
        let cm = reference_free_ir_method();
        let roots = scan(&cm, on, true);
        assert!(below.iter().all(|o| !roots.contains(o)), "qualifying frame: floored");
        assert!(roots.contains(&objs[0]), "and its tail scanned");
        let roots = scan(&cm, off, true);
        assert!(below.iter().all(|o| roots.contains(o)), "switch off: whole band");
        let roots = scan(&cm, on, false);
        assert!(below.iter().all(|o| roots.contains(o)), "off the chain: whole band");
        let mut may_hold = reference_free_ir_method();
        may_hold.frame_holds_no_references = false;
        let roots = scan(&may_hold, on, true);
        assert!(below.iter().all(|o| roots.contains(o)), "not reference-free: whole band");
        let mut single_pass = reference_free_ir_method();
        single_pass.used_ir_backend = false;
        let roots = scan(&single_pass, on, true);
        assert!(below.iter().all(|o| roots.contains(o)), "single-pass body: whole band");
        q::clear_movable_jit_roots();
        q::clear_unrewritable_jit_roots();
        drop(band);
    }
