use super::*;

/// `static int f(int[] a)`: `while (i < a.length) { if (a[i] < 0) { i++;
/// continue; } s += a[i]; i++; } return s;` -- header 4, back edges 19
/// (the `continue`) and 31.
fn continue_loop() -> Vec<u8> {
    vec![
        0x03, 0x3c, // 0: iconst_0; istore_1
        0x03, 0x3d, // 2: iconst_0; istore_2
        0x1b, 0x2a, 0xbe, // 4: iload_1; aload_0; arraylength   <- header
        0xa2, 0x00, 0x1b, // 7: if_icmpge -> 34
        0x2a, 0x1b, 0x2e, // 10: aload_0; iload_1; iaload
        0x9c, 0x00, 0x09, // 13: ifge -> 22
        0x84, 0x01, 0x01, // 16: iinc 1, 1
        0xa7, 0xff, 0xf1, // 19: goto -> 4   (continue)
        0x1c, 0x2a, 0x1b, 0x2e, 0x60, 0x3d, // 22: s += a[i]
        0x84, 0x01, 0x01, // 28: iinc 1, 1
        0xa7, 0xff, 0xe5, // 31: goto -> 4
        0x1c, 0xac, // 34: iload_2; ireturn
    ]
}

#[test]
fn a_header_keeps_its_farthest_back_edge_in_first_occurrence_order() {
    assert_eq!(
        loops_by_header(&[(4, 19), (40, 60), (4, 31), (40, 50)]),
        vec![(4, 31), (40, 60)]
    );
    let single = vec![(10, 30), (2, 40)];
    assert_eq!(loops_by_header(&single), single);
    assert!(loops_by_header(&[]).is_empty());
}

/// Round 11 wave 7 (W7-3): the driver's one-decode form answers exactly
/// what two separate landing-predicate calls answer, including the
/// opaque (`jsr`) and handler-entry cases.
#[test]
fn the_shared_edge_scan_answers_like_two_separate_calls() {
    let code = continue_loop();
    let n = code.len();
    let mut loops = detect_loops(&code, n);
    loops.sort_unstable();
    let merged = loops_by_header(&loops);
    let ranges_cases: [&[(usize, usize, usize)]; 2] = [&[], &[(0, 6, 22)]];
    for ranges in ranges_cases {
        let (per_pair, both) = find_preheader_bypassing_loop_headers_per_pair_and_merged(
            &code, n, &loops, &merged, ranges,
        );
        assert_eq!(
            per_pair,
            find_preheader_bypassing_loop_headers(&code, n, &loops, ranges)
        );
        assert_eq!(
            both,
            find_preheader_bypassing_loop_headers(&code, n, &merged, ranges)
        );
    }
    // Per pair, the `continue` still enters `(4, 19)` from outside it;
    // merged, nothing does. A handler reaching into the body from before
    // the header is a bypass in both.
    let (per_pair, both) = find_preheader_bypassing_loop_headers_per_pair_and_merged(
        &code,
        n,
        &loops,
        &merged,
        &[],
    );
    assert!(per_pair.contains(&4));
    assert!(both.is_empty());
    let (_, with_handler) = find_preheader_bypassing_loop_headers_per_pair_and_merged(
        &code,
        n,
        &loops,
        &merged,
        &[(0, 6, 22)],
    );
    assert!(with_handler.contains(&4));
    // Opaque control flow (a `jsr`) marks every header of both lists.
    let mut opaque = code.clone();
    opaque[34] = 0xa8; // jsr (its target bytes are the ireturn's; never run)
    let (a, b) = find_preheader_bypassing_loop_headers_per_pair_and_merged(
        &opaque,
        n,
        &loops,
        &merged,
        &[],
    );
    assert!(a.contains(&4) && b.contains(&4));
    let (e1, e2) =
        find_preheader_bypassing_loop_headers_per_pair_and_merged(&code, n, &[], &[], &[]);
    assert!(e1.is_empty() && e2.is_empty());
}

#[test]
fn a_continue_loop_is_not_bypassable_once_its_back_edges_are_merged() {
    let code = continue_loop();
    let n = code.len();
    let mut loops = detect_loops(&code, n);
    loops.sort_unstable();
    assert_eq!(loops, vec![(4, 19), (4, 31)]);
    // Per pair, the second back edge enters `(4, 19)` from outside it.
    assert!(find_bypassable_loop_headers(&code, n, &loops, &[]).contains(&4));
    let merged = loops_by_header(&loops);
    assert_eq!(merged, vec![(4, 31)]);
    assert!(find_bypassable_loop_headers(&code, n, &merged, &[]).is_empty());
    // And the hoist found over the merged extent covers the whole loop.
    let hoists = find_array_len_hoists_with_rotation(&code, n, &merged, &FxHashMap::default());
    assert_eq!(hoists.len(), 1);
    assert_eq!(hoists[0].loop_header, 4);
    assert_eq!(hoists[0].loop_end, 34);
    assert_eq!(hoists[0].sites, vec![(5, 7)]);
}

/// A write after the `continue` is inside the merged extent, so a local
/// the loop writes only there is not invariant.
#[test]
fn a_write_after_the_continue_is_seen_by_the_merged_extent() {
    let code = continue_loop();
    let n = code.len();
    let merged = loops_by_header(&detect_loops(&code, n));
    let (header, back_edge) = merged[0];
    let loop_end = back_edge + bytecode_analysis::step(&code, back_edge);
    // Local 2 (`s`) is written only at 27, after the `continue` at 19.
    assert_eq!(find_modified_locals(&code, header, 19 + 3) & (1 << 2), 0);
    assert_ne!(find_modified_locals(&code, header, loop_end) & (1 << 2), 0);
}

/// Round 11 wave 9 (`r11w8-mid-driver-bce-over-merged-loops-patch`): the
/// `continue` loop with a PARAMETER bound, `static int f(int[] a, int n)`
/// (locals 0=a, 1=n, 2=i, 3=s) -- header 4, back edges 18 and 30. Over the
/// merged pair the counted-loop BCE proves the header access `a[i]` (11)
/// behind one pre-header guard, and the merged bypass set (the driver's
/// veto for that guard since wave 9) does not refuse the header, where the
/// per-pair set did.
#[test]
fn a_continue_loop_keeps_its_guarded_bce_over_the_merged_pair() {
    let code: Vec<u8> = vec![
        0x03, 0x3d, // 0: iconst_0; istore_2
        0x03, 0x3e, // 2: iconst_0; istore_3
        0x1c, 0x1b, // 4: iload_2; iload_1   <- header
        0xa2, 0x00, 0x1b, // 6: if_icmpge -> 33
        0x2a, 0x1c, 0x2e, // 9: aload_0; iload_2; iaload
        0x9c, 0x00, 0x09, // 12: ifge -> 21
        0x84, 0x02, 0x01, // 15: iinc 2, 1
        0xa7, 0xff, 0xf2, // 18: goto -> 4   (continue)
        0x1d, 0x2a, 0x1c, 0x2e, 0x60, 0x3e, // 21: s += a[i]
        0x84, 0x02, 0x01, // 27: iinc 2, 1
        0xa7, 0xff, 0xe6, // 30: goto -> 4
        0x1d, 0xac, // 33: iload_3; ireturn
    ];
    let n = code.len();
    let mut loops = detect_loops(&code, n);
    loops.sort_unstable();
    assert_eq!(loops, vec![(4, 18), (4, 30)]);
    let merged = loops_by_header(&loops);
    assert_eq!(merged, vec![(4, 30)]);
    let (safe, guards) = analyze_bounds_elimination_with_handlers(&code, n, &merged, Some(&[]));
    assert!(safe.contains(&11), "header access elided, got {safe:?}");
    assert_eq!(guards.len(), 1, "one guard for the one header");
    assert_eq!(
        (
            guards[0].loop_header,
            guards[0].array_local,
            guards[0].bound_local,
            guards[0].iv_local,
            guards[0].covered_pcs.clone(),
        ),
        (4, 0, 1, 2, vec![11]),
    );
    let (per_pair, both) = find_preheader_bypassing_loop_headers_per_pair_and_merged(
        &code,
        n,
        &loops,
        &merged,
        &[],
    );
    assert!(per_pair.contains(&4));
    assert!(!both.contains(&4));
}
