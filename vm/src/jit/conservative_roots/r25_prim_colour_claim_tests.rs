use super::*;

/// The optimizing tier's shape over an 8-word band (as the gci i23/rf
/// module's): locals at off 8 and 16, an operand spill at 24, the register
/// tail at 32..64.
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

/// An optimizing-tier body that MAY hold references, with no safepoint-id
/// slot -- so no map ever resolves and the per-map claim abstains, as at
/// the entry poll -- claiming `claim`.
fn ir_method(claim: &[i16]) -> cratonvm_jit::CompiledMethod {
    let mut cm = cratonvm_jit::CompiledMethod::new(
        cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests"),
    );
    cm.frame_layout = ir_layout();
    cm.osr_frame_size = 64;
    cm.used_ir_backend = true;
    cm.frame_holds_no_references = false;
    cm.sp_id_slot_off = 0;
    cm.ir_prim_colour_claim = claim.to_vec().into_boxed_slice();
    cm
}

/// Eight Generational-heap objects, one per band word: `objs[i]` sits at
/// `[rbp - (64 - 8 * i)]`, so off 24 is `objs[5]`, off 16 `objs[6]`, off 8
/// `objs[7]`.
fn band(heap: &VmHeap) -> ([usize; 8], Box<[usize; 8]>) {
    use crate::classloading::ClassId;
    let mut objs = [0usize; 8];
    for o in objs.iter_mut() {
        *o = heap.alloc_object(ClassId::new(0), 1).as_ptr() as usize;
    }
    (objs, Box::new(objs))
}

fn roots_of(out: &[ObjectRef]) -> Vec<usize> {
    out.iter().map(|o| o.as_ptr() as usize).collect()
}

/// The claim is spent only when every condition holds; each one withdrawn
/// alone claims nothing (the whole band, as before).
#[test]
fn r25_the_body_claim_needs_every_condition() {
    let cm = ir_method(&[16, 24]);
    let want: &[i16] = &[16, 24];
    assert_eq!(ir_prim_colour_claim(&cm, true, true), Some(want));
    assert_eq!(
        ir_prim_colour_claim(&cm, false, true),
        None,
        "the per-map claim's switch off"
    );
    assert_eq!(
        ir_prim_colour_claim(&cm, true, false),
        None,
        "a frame recovered by inspection, not on the registered chain"
    );
    let mut sp = ir_method(&[16, 24]);
    sp.used_ir_backend = false;
    assert_eq!(ir_prim_colour_claim(&sp, true, true), None, "a single-pass body");
    let none = ir_method(&[]);
    assert_eq!(ir_prim_colour_claim(&none, true, true), None, "no claim");
    let unsorted = ir_method(&[24, 16]);
    assert_eq!(
        ir_prim_colour_claim(&unsorted, true, true),
        None,
        "not ascending: the binary search could miss a word"
    );
    let dup = ir_method(&[16, 16]);
    assert_eq!(
        ir_prim_colour_claim(&dup, true, true),
        None,
        "a duplicate is a malformed list"
    );
    let zero = ir_method(&[0, 16]);
    assert_eq!(
        ir_prim_colour_claim(&zero, true, true),
        None,
        "[rbp - 0] is the saved RBP, never a colour"
    );
    assert!(ir_prim_colour_claimed(Some(want), 16));
    assert!(ir_prim_colour_claimed(Some(want), 24));
    assert!(!ir_prim_colour_claimed(Some(want), 8));
    assert!(!ir_prim_colour_claimed(Some(want), i64::from(i16::MAX) + 16));
    assert!(!ir_prim_colour_claimed(None, 16));
}

/// JIT round 25 wave 4: the whole-word rule is the per-body claim's alone.
/// The list the caller substitutes (`ir_prim_colour_claim`, the body's own
/// slice) is recognised by identity; an equal list from somewhere else (a
/// map's `non_oop_stack_slots`) is not, nor is any list of a single-pass
/// or claim-less body. The census reads.
#[test]
fn r25w4_only_the_body_claim_itself_ignores_words_whole() {
    let cm = ir_method(&[16, 24]);
    let spent = ir_prim_colour_claim(&cm, true, true).expect("the claim is spent");
    assert!(ir_body_claim_ignores_word(&cm, spent), "the body's own slice");
    let map_list: Vec<i16> = vec![16, 24];
    assert!(
        !ir_body_claim_ignores_word(&cm, &map_list),
        "an equal map list keeps the object-start rule"
    );
    let mut sp = ir_method(&[16, 24]);
    sp.used_ir_backend = false;
    let sp_list: &[i16] = &sp.ir_prim_colour_claim;
    assert!(!ir_body_claim_ignores_word(&sp, sp_list), "a single-pass body");
    let none = ir_method(&[]);
    let none_list: &[i16] = &none.ir_prim_colour_claim;
    assert!(!ir_body_claim_ignores_word(&none, none_list), "no claim");
    let _ = ir_body_claim_unpinned_census();
}

/// Through the frame scan, with no active safepoint id (the entry poll's
/// case, where the per-map claim abstains): the claimed object starts are
/// neither roots nor pins, every other word is a root as before, and the
/// census counts the frame. Each withdrawn condition roots them again.
#[test]
fn r25_the_body_claim_drops_claimed_object_starts_at_an_id_less_poll() {
    use crate::memory::vm_heap::VmHeap;
    use cratonvm_gc::gc_quiescence as q;
    let heap = VmHeap::new(
        crate::memory::vm_heap::GcBackend::Generational,
        16 * 1024 * 1024,
    );
    let (objs, band) = band(&heap);
    let rbp = band.as_ptr() as usize + 64;
    let on = BandClaimSwitches {
        ir_prims: true,
        ..BandClaimSwitches::read()
    };
    let switched_off = BandClaimSwitches {
        ir_prims: false,
        ..on
    };
    let scan = |cm: &cratonvm_jit::CompiledMethod, sw: BandClaimSwitches, chain: bool| {
        let mut out = Vec::new();
        assert!(scan_one_compiled_frame_with_layout(
            rbp, cm, &heap, &mut out, chain, 0, sw, None, None
        ));
        roots_of(&out)
    };
    q::clear_movable_jit_roots();
    q::clear_unrewritable_jit_roots();
    let cm = ir_method(&[16, 24]);
    assert_eq!(
        frame_claim_facts(rbp, &cm, on).ir_prims,
        None,
        "precondition: no id, so the per-map claim abstains"
    );
    let before = ir_prim_colour_claim_census();
    let roots = scan(&cm, on, true);
    for (i, o) in objs.iter().enumerate() {
        let off = 64 - 8 * i;
        if off == 16 || off == 24 {
            assert!(!roots.contains(o), "off={off}: claimed, not a root");
            assert!(
                !q::is_movable_jit_root(*o) && !q::is_unrewritable_jit_root(*o),
                "off={off}: nor a pin"
            );
        } else {
            assert!(roots.contains(o), "off={off}: unclaimed, still a root");
        }
    }
    assert!(ir_prim_colour_claim_census() > before, "the frame is counted");

    let claimed = [objs[5], objs[6]];
    let roots = scan(&cm, switched_off, true);
    assert!(claimed.iter().all(|o| roots.contains(o)), "switch off: rooted");
    let roots = scan(&cm, on, false);
    assert!(claimed.iter().all(|o| roots.contains(o)), "off the chain: rooted");
    let none = ir_method(&[]);
    let roots = scan(&none, on, true);
    assert!(claimed.iter().all(|o| roots.contains(o)), "no claim: rooted");
    let mut single_pass = ir_method(&[16, 24]);
    single_pass.used_ir_backend = false;
    let roots = scan(&single_pass, on, true);
    assert!(claimed.iter().all(|o| roots.contains(o)), "single-pass body: rooted");
    q::clear_movable_jit_roots();
    q::clear_unrewritable_jit_roots();
    drop(band);
}

/// The moving-young verifier does not report a claimed word as an
/// unpublished root: a stale young address the clear no longer zeroes is
/// residue, and the band scan does not root it either.
#[test]
fn r25_the_verifier_does_not_report_a_claimed_word() {
    const BASE: usize = 0x00AB_C000;
    let mut frame = [0usize; 8];
    // Word `i` is `[rbp - (64 - 8 * i)]`: off 24 and off 16.
    frame[5] = BASE + 0x40;
    frame[6] = BASE + 0x80;
    let rbp = frame.as_ptr() as usize + 64;
    let layout = ir_layout();
    // addr-keyed: test-local, always empty -- no word is published and no collection runs.
    let published: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let ask = |claim: Option<&[i16]>| {
        band_has_unpublished_word_with_map(
            rbp,
            64,
            &layout,
            None,
            &published,
            None,
            |w: usize| (BASE..BASE + 0x1000).contains(&w),
            false,
            None,
            claim,
        )
    };
    assert!(ask(None), "control: both words are reported");
    assert!(!ask(Some(&[16, 24])), "both claimed: nothing to report");
    assert!(ask(Some(&[16])), "off 24 unclaimed: still reported");
    assert!(ask(Some(&[24])), "off 16 unclaimed: still reported");
}
