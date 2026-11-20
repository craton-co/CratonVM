//! gch h1/jw (`r17-gc-compiled-chain-walks-stop-at-4096-activations-silently`):
//! the collector's RBP-chain walks take [`GcChainWalkGuard`], which never
//! cuts a well-formed chain and, when it does cut, says so.
use super::*;

/// Walk a synthetic chain the way the GC walks do: every step moves
/// strictly up by `stride` bytes inside `[lo, hi)`. Returns (steps taken,
/// whether the guard cut the walk).
fn walk_chain(
    mut guard: GcChainWalkGuard,
    lo: usize,
    hi: usize,
    stride: usize,
) -> (usize, bool) {
    let mut rbp = lo;
    let mut steps = 0usize;
    loop {
        if !guard.step() {
            return (steps, true);
        }
        steps += 1;
        let parent = rbp + stride;
        if parent <= rbp || parent & 0x7 != 0 || parent >= hi {
            return (steps, false);
        }
        rbp = parent;
    }
}

/// A spent guard answers `false` from its limit on, so a loop that ends on
/// it can tell a cut from the end of the chain.
#[test]
fn gch_jw_gc_chain_walk_guard_cuts_after_its_limit_and_says_so() {
    let mut g = GcChainWalkGuard::with_limit(3);
    assert_eq!(g.limit(), 3);
    assert!(g.step());
    assert!(g.step());
    assert!(g.step());
    assert!(!g.step(), "the fourth step is a cut");
    assert!(!g.step(), "a cut stays a cut");
    let mut none = GcChainWalkGuard::with_limit(0);
    assert!(!none.step());
}

/// The band guard never cuts a monotone chain, however deep: a 10 000
/// activation recursion of minimal (16-byte) frames, and the densest
/// possible chain (8-byte steps), both end on the band, not the guard.
#[test]
fn gch_jw_band_guard_never_cuts_a_chain_deeper_than_4096() {
    let lo = 0x7f00_0000_0000usize;
    for (stride, frames) in [(16usize, 10_000usize), (8, 20_000), (256, 6_000)] {
        let hi = lo + stride * frames;
        let g = GcChainWalkGuard::with_limit(compiled_walk_step_guard(true, lo, hi));
        let (steps, cut) = walk_chain(g, lo, hi, stride);
        assert!(
            !cut,
            "stride {stride}: the band guard cut after {steps} steps"
        );
        assert_eq!(
            steps, frames,
            "stride {stride}: every activation is visited"
        );
        assert!(steps > LEGACY_WALK_ACTIVATION_GUARD);
    }
}

/// `CRATONVM_JIT_COMPILED_WALK_WHOLE_ENTRY=0` keeps the legacy 4096, and
/// a 4097-deep chain is then a CUT (reported), not a chain end.
#[test]
fn gch_jw_legacy_guard_reports_the_cut_of_a_4097_deep_chain() {
    let lo = 0x7f00_0000_0000usize;
    let legacy =
        || GcChainWalkGuard::with_limit(compiled_walk_step_guard(false, lo, lo + 16 * 5_000));
    let (steps, cut) = walk_chain(legacy(), lo, lo + 16 * 5_000, 16);
    assert!(cut);
    assert_eq!(steps, LEGACY_WALK_ACTIVATION_GUARD);
    // A chain that fits ends on the band, uncut, under either guard.
    let (steps, cut) = walk_chain(legacy(), lo, lo + 16 * 4_000, 16);
    assert!(!cut);
    assert_eq!(steps, 4_000);
}

/// The process default (no env override in a unit test) is the band guard.
#[test]
fn gch_jw_for_entry_follows_the_whole_entry_switch() {
    let (lo, hi) = (0x7000_0000usize, 0x7080_0000usize);
    let want = compiled_walk_step_guard(compiled_walk_whole_entry_enabled(), lo, hi);
    assert_eq!(GcChainWalkGuard::for_entry(lo, hi).limit(), want);
}

fn fn_body<'a>(src: &'a str, name: &str) -> &'a str {
    let needle = format!("fn {name}(");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("{name} is defined here"));
    let rest = &src[start..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("{name} has a body"));
    &rest[..end]
}

/// One assertion per GC chain walk (a source-level check: the walks need
/// live compiled frames to run). Each takes the entry band's guard and has
/// no fixed 4096 loop left; the two that STATE coverage turn a cut into
/// an incomplete verdict.
#[test]
fn gch_jw_every_gc_chain_walk_takes_the_band_guard() {
    let src = concat!(include_str!("../conservative_roots/mod.rs"), "\n", include_str!("../conservative_roots/jit_frame_chain_entry.rs"), "\n", include_str!("../conservative_roots/gc_outgoing_arg_roots_enabled.rs"), "\n", include_str!("../conservative_roots/refresh_moving_young_coverage_for.rs"), "\n", include_str!("../conservative_roots/warn_cross_thread_jit_gap.rs"), "\n", include_str!("../conservative_roots/scan_one_frame.rs"));
    for name in [
        "moving_young_unpublished_frame_oop_present",
        "refresh_moving_young_coverage_for_current_thread",
        "young_pin_classify_compiled_frames",
        "remap_active_jit_frames",
        "verify_precise_covers_conservative",
        "scan_precise_frames_and_bands",
        "audit_jit_frames_for_vacated",
        "report_stale_after_remap",
        "remap_register_image_words",
    ] {
        let body = fn_body(src, name);
        assert!(
            body.contains("GcChainWalkGuard::for_entry("),
            "{name} must bound its RBP-chain walk by the entry band"
        );
        let fixed = ["frames < 4096", "guard < 4096", "band_frames >= 4096"];
        for f in fixed {
            assert!(
                !body.contains(f),
                "{name} still has a fixed walk guard: {f}"
            );
        }
    }
    let refresh = fn_body(src, "refresh_moving_young_coverage_for_current_thread");
    let cut = &refresh[refresh
        .find("if !walk.step() {")
        .expect("the refresh acts on a cut")..];
    let cut = &cut[..cut.find("break;").expect("and ends the walk")];
    assert!(cut.contains("UNBOUNDED_FRAME_BAND") && cut.contains("complete = false"));
    let verifier = fn_body(src, "moving_young_unpublished_frame_oop_present");
    let cut = &verifier[verifier
        .find("if !walk.step() {")
        .expect("the verifier acts on a cut")..];
    let cut = &cut[..cut.find("break;").expect("and ends the walk")];
    assert!(cut.contains("unverified = true"));
}

/// gch h1/jw (review): the coverage proof refuses an exact RBP outside its
/// entry's band BEFORE it resolves a method for it or reads its
/// safepoint-id slot (a source pin: the proof needs live compiled frames).
#[test]
fn gch_jw_refresh_refuses_an_out_of_band_exact_rbp_before_reading_it() {
    let src = concat!(include_str!("../conservative_roots/mod.rs"), "\n", include_str!("../conservative_roots/jit_frame_chain_entry.rs"), "\n", include_str!("../conservative_roots/gc_outgoing_arg_roots_enabled.rs"), "\n", include_str!("../conservative_roots/refresh_moving_young_coverage_for.rs"), "\n", include_str!("../conservative_roots/warn_cross_thread_jit_gap.rs"), "\n", include_str!("../conservative_roots/scan_one_frame.rs"));
    let refresh = fn_body(src, "refresh_moving_young_coverage_for_current_thread");
    let check = refresh
        .find("exact_rbp >= entry.entry_sp")
        .expect("the band check");
    let resolve = refresh
        .find("innermost_frame_method(exact_rbp")
        .expect("the innermost resolution");
    assert!(check < resolve, "checked before the first read");
    let refusal = &refresh[check..resolve];
    assert!(refusal.contains("UNBOUNDED_FRAME_BAND") && refusal.contains("continue;"));
}

/// gch h1/jw (review): a frame whose active maps name more than 64
/// distinct slots rewrites a slot two maps share ONCE, even past the 64th.
/// The old fixed 64-entry `seen` array rewrote it twice, and the second
/// lookup re-forwarded it whenever the collector had moved another object
/// onto its new address (a sliding or in-place copy).
#[test]
fn gch_jw_remap_rewrites_a_shared_slot_once_past_64_distinct_slots() {
    let mut words = vec![0usize; 128].into_boxed_slice();
    let offs: Vec<i16> = (0..70i16).map(|i| 16 + 8 * i).collect();
    let shared_off = *offs.last().expect("70 offsets");
    let shared_idx = 128 - usize::try_from(shared_off).expect("positive") / 8;
    // An object moved X -> Y, and another one Y -> Z.
    let (x, y, z) = (0x7100_0000usize, 0x7100_1000usize, 0x7100_2000usize);
    words[127] = 5; // `[rbp - 8]`: the frame's safepoint id
    words[shared_idx] = x;
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit_byte(0xC3);
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.sp_id_slot_off = 8;
    let mut a = cratonvm_jit::OopMapEntry::new(0x10);
    a.bytecode_pc = 5;
    a.frame_slot_offsets = offs.clone();
    let mut b = cratonvm_jit::OopMapEntry::new(0x20);
    b.bytecode_pc = 5;
    b.frame_slot_offsets = vec![shared_off];
    cm.push_oop_map(a);
    cm.push_oop_map(b);
    let mut pm = cratonvm_types::PointerMap::new();
    pm.insert(x, y);
    pm.insert(y, z);
    let lo = words.as_mut_ptr() as usize;
    let rbp = lo + 8 * 128;
    let band = cratonvm_gc::shadow_stack::SlotBand::new(lo, rbp);
    let (found, examined, rewritten) = remap_one_jit_frame(rbp, &cm, &pm, &[], band);
    assert!(found, "the safepoint id selects both maps");
    assert_eq!(examined, 70, "each distinct slot once");
    assert_eq!(rewritten, 1);
    assert_eq!(
        words[shared_idx], y,
        "forwarded once, not on to the other object's new address"
    );
}
