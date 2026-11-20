use super::*;

#[test]
fn gci_i38ds_a_slot_the_map_names_is_not_reported() {
    assert!(!frame_slot_unnamed_by_map(Some(&[8, 16, 24]), 16));
    assert!(frame_slot_unnamed_by_map(Some(&[8, 16, 24]), 32));
    assert!(frame_slot_unnamed_by_map(Some(&[]), 8));
}

#[test]
fn gci_i38ds_no_resolved_map_names_nothing() {
    assert!(frame_slot_unnamed_by_map(None, 8));
    assert!(frame_slot_unnamed_by_map(None, 0));
}

#[test]
fn gci_i38ds_line_names_object_frame_and_region() {
    let line = unnamed_frame_root_line(
        0x7f00_1234_5678,
        42,
        "P.main",
        160,
        "operand-spill",
        true,
        " reg=rbx mask=0x8",
        " tier=ir sp=7 live_hi=152 in_map=false",
        "rooted",
    );
    assert_eq!(
        line,
        "[frame-root-unnamed] obj=0x7f0012345678 cid=42 method=P.main off=160 \
         region=operand-spill map=resolved reg=rbx mask=0x8 tier=ir sp=7 live_hi=152 \
         in_map=false verdict=rooted"
    );
    let none = unnamed_frame_root_line(
        0x10,
        1,
        "Q.f",
        8,
        "java-local",
        false,
        "",
        "",
        "excluded-map-dead",
    );
    assert_eq!(
        none,
        "[frame-root-unnamed] obj=0x10 cid=1 method=Q.f off=8 region=java-local map=none \
         verdict=excluded-map-dead"
    );
}

/// java locals at offsets below 40, the operand spill at [48, 96), the
/// callee-saved image at [200, 248).
fn layout() -> cratonvm_jit::FrameLayout {
    cratonvm_jit::FrameLayout {
        java_locals_hi: 40,
        spill_lo: 48,
        spill_hi: 96,
        callee_saved_lo: 200,
        callee_saved_hi: 248,
        ..Default::default()
    }
}

#[test]
fn gci_i38ds_map_dead_claim_lets_go_an_unnamed_modelled_word() {
    let l = layout();
    let map: &[i32] = &[8, 16, 48];
    // A java-local home and a live-part operand spill the map does not name.
    assert!(band_map_dead_word_excluded(&l, 24, Some(64), Some(map)));
    assert!(band_map_dead_word_excluded(&l, 56, Some(64), Some(map)));
    // Named by the map: kept.
    assert!(!band_map_dead_word_excluded(&l, 16, Some(64), Some(map)));
    assert!(!band_map_dead_word_excluded(&l, 48, Some(64), Some(map)));
}

#[test]
fn gci_i38ds_map_dead_claim_claims_nothing_outside_its_rule() {
    let l = layout();
    let map: &[i32] = &[8];
    // No resolved map proves nothing.
    assert!(!band_map_dead_word_excluded(&l, 24, Some(64), None));
    // A callee-saved image word keeps its own rule.
    assert!(!band_map_dead_word_excluded(&l, 208, Some(64), Some(map)));
    // An unmodelled slot (between the spill and the image) is not claimed.
    assert!(!band_map_dead_word_excluded(&l, 120, Some(64), Some(map)));
    // A spill word above the live cursor keeps the dead-spill rule.
    assert!(!band_map_dead_word_excluded(&l, 80, Some(64), Some(map)));
}

/// t1 W13: a java-local home either claim proves dead owes nothing,
/// whatever word it holds; a spill word (a derived pointer may live
/// there), a named home and a home with no proof keep their old rules.
#[test]
fn t1w13_a_dead_local_home_owes_neither_root_nor_pin() {
    let l = layout();
    let map: &[i32] = &[8, 16, 48];
    assert!(dead_local_home_owes_nothing(&l, 24, Some(64), Some(map), false), "unnamed home");
    assert!(!dead_local_home_owes_nothing(&l, 16, Some(64), Some(map), false), "named home");
    assert!(!dead_local_home_owes_nothing(&l, 56, Some(64), Some(map), false), "spill word");
    assert!(!dead_local_home_owes_nothing(&l, 24, Some(64), None, false), "no proof");
    assert!(dead_local_home_owes_nothing(&l, 24, Some(64), None, true), "single-pass claim");
    assert!(!dead_local_home_owes_nothing(&l, 56, Some(64), None, true), "claim is homes only");
}

#[test]
fn gci_i38ds_local_mask_homes_name_the_canonical_homes() {
    let homes: Vec<i32> = local_mask_homes(Some(0b1011)).collect();
    assert_eq!(homes, vec![8, 16, 32]);
    assert_eq!(local_mask_homes(None).count(), 0);
    let top: Vec<i32> = local_mask_homes(Some(1u64 << 63)).collect();
    assert_eq!(top, vec![512]);
}

/// gci i41/rs (residuals page `gci-i39rv-waves-i35-i38-review-residuals`
/// item 3): the two i38/ds switches reach the band scan through
/// [`FrameClaimFacts`], copied from the once-per-chain-walk
/// [`BandClaimSwitches`] in every arm of [`frame_claim_facts_with`].
#[test]
fn gci_i41rs_frame_claim_facts_carry_the_two_i38_switches() {
    for (dbg, claim) in [(false, false), (true, false), (false, true), (true, true)] {
        let sw = BandClaimSwitches {
            unnamed_dbg: dbg,
            map_dead_claim: claim,
            ..BandClaimSwitches::read()
        };
        // No sp-id slot: the "no map" arm.
        let frame: [usize; 4] = [0, 3, 0, 0];
        let rbp = &frame[2] as *const usize as usize;
        let cm = cratonvm_jit::CompiledMethod::new(
            cratonvm_jit::ExecutableBuffer::new(64)
                .expect("executable buffer alloc must succeed in tests"),
        );
        let f = frame_claim_facts(rbp, &cm, sw);
        assert_eq!((f.unnamed_dbg, f.map_dead_claim), (dbg, claim), "no-map arm");
        // A slot naming id 3 and a map that carries it: the mapped arm.
        let mut cm = cratonvm_jit::CompiledMethod::new(
            cratonvm_jit::ExecutableBuffer::new(64)
                .expect("executable buffer alloc must succeed in tests"),
        );
        cm.sp_id_slot_off = 8;
        let mut m = cratonvm_jit::OopMapEntry::new(0);
        m.bytecode_pc = 3;
        cm.push_oop_map(m);
        let f = frame_claim_facts(rbp, &cm, sw);
        assert_eq!(active_maps(rbp, &cm).sp_id, Some(3), "the id resolved");
        assert_eq!((f.unnamed_dbg, f.map_dead_claim), (dbg, claim), "mapped arm");
    }
    // The read itself is the two functions' answers.
    let sw = BandClaimSwitches::read();
    assert_eq!(sw.unnamed_dbg, dbg_frame_root_unnamed());
    assert_eq!(sw.map_dead_claim, band_map_dead_not_roots_enabled());
}

/// gci i41/rs: the per-frame scan reads neither switch itself any more
/// (a source pin: the cost item is a flag lookup per compiled frame per
/// collection), and every arm of the facts constructor copies both.
#[test]
fn gci_i41rs_the_band_scan_reads_no_switch_per_frame() {
    let src = concat!(include_str!("../conservative_roots/mod.rs"), "\n", include_str!("../conservative_roots/jit_frame_chain_entry.rs"), "\n", include_str!("../conservative_roots/gc_outgoing_arg_roots_enabled.rs"), "\n", include_str!("../conservative_roots/refresh_moving_young_coverage_for.rs"), "\n", include_str!("../conservative_roots/warn_cross_thread_jit_gap.rs"), "\n", include_str!("../conservative_roots/scan_one_frame.rs"));
    let body = |name: &str| -> &'static str {
        let needle = format!("fn {name}(");
        let start = src.find(&needle).expect("defined here");
        let rest = &src[start..];
        &rest[..rest.find("\n\u{7d}\n").expect("has a body")]
    };
    let scan = body("scan_one_frame_filtered_from");
    assert!(!scan.contains("dbg_frame_root_unnamed()"));
    assert!(!scan.contains("band_map_dead_not_roots_enabled()"));
    assert!(scan.contains("unnamed_dbg,") && scan.contains("map_dead_claim,"));
    let facts = body("frame_claim_facts_with<'a>");
    assert_eq!(facts.matches("unnamed_dbg: sw.unnamed_dbg,").count(), 3);
    assert_eq!(facts.matches("map_dead_claim: sw.map_dead_claim,").count(), 3);
}
