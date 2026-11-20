use super::*;

fn answer(num_captures: usize, proxy_class_id: u32, descriptor: &str) -> IndyLambdaImplAnswer {
    IndyLambdaImplAnswer {
        proxy_class_id,
        num_captures,
        sam_name: "getAsInt".to_string(),
        sam_descriptor: "()I".to_string(),
        class: "T".to_string(),
        name: "lambda$k$0".to_string(),
        descriptor: descriptor.to_string(),
        class_id: 3,
    }
}

const NON_CAPTURING: &str = "()Ljava/util/function/IntSupplier;";
const CAPTURING: &str = "(I)Ljava/util/function/IntSupplier;";

fn admit(
    descriptor: &str,
    bridge_site: usize,
    captures: Option<&dyn Fn(usize) -> Option<(u32, usize)>>,
    row: Option<IndyLambdaImplAnswer>,
) -> Result<(usize, IndyLambdaImplAnswer), LambdaSiteSkip> {
    let impls = move |_: usize| row.clone();
    admit_lambda_site(descriptor, bridge_site, captures, &impls)
}

#[test]
fn a_non_capturing_site_with_its_impl_row_is_admitted() {
    let row = answer(0, 9, "()I");
    assert_eq!(admit(NON_CAPTURING, 5, None, Some(row.clone())), Ok((0, row)));
}

#[test]
fn a_capturing_site_with_the_matching_fact_is_admitted() {
    let fact = |_: usize| Some((9u32, 1usize));
    let row = answer(1, 9, "(I)I");
    assert_eq!(admit(CAPTURING, 5, Some(&fact), Some(row.clone())), Ok((1, row)));
}

#[test]
fn each_refusal_names_its_reason() {
    let fact = |_: usize| Some((9u32, 1usize));
    let wrong_count = |_: usize| Some((9u32, 2usize));
    let no_class = |_: usize| Some((0u32, 1usize));
    let row = answer(1, 9, "(I)I");
    let cases: [(Result<_, _>, LambdaSiteSkip); 9] = [
        (admit("()I", 5, None, Some(answer(0, 9, "()I"))), LambdaSiteSkip::NotReference),
        (admit(NON_CAPTURING, 0, None, Some(answer(0, 9, "()I"))), LambdaSiteSkip::NoBridgeSite),
        (admit(CAPTURING, 5, None, Some(row.clone())), LambdaSiteSkip::CaptureNewOff),
        (admit(CAPTURING, 5, Some(&wrong_count), Some(row.clone())), LambdaSiteSkip::NoCaptureFact),
        (admit(CAPTURING, 5, Some(&no_class), Some(row.clone())), LambdaSiteSkip::NoCaptureFact),
        (admit(CAPTURING, 5, Some(&fact), None), LambdaSiteSkip::NoImplAnswer),
        (admit(CAPTURING, 5, Some(&fact), Some(answer(2, 9, "(II)I"))), LambdaSiteSkip::ImplCaptureCount),
        (admit(CAPTURING, 5, Some(&fact), Some(answer(1, 8, "(I)I"))), LambdaSiteSkip::ImplProxyClass),
        (admit(CAPTURING, 5, Some(&fact), Some(answer(1, 9, "(J)I"))), LambdaSiteSkip::CaptureTags),
    ];
    for (i, (got, want)) in cases.into_iter().enumerate() {
        assert_eq!(got.err(), Some(want), "case {i}");
    }
}

#[test]
fn retain_noting_records_each_dropped_site_once() {
    let mut sites: HashMap<usize, usize> = [(3, 0), (7, 1), (11, 2)].into_iter().collect();
    let mut skipped = Vec::new();
    retain_noting(&mut sites, &mut skipped, LambdaSiteSkip::CapturingOff, |&c| c == 0);
    let mut kept: Vec<usize> = sites.keys().copied().collect();
    kept.sort_unstable();
    assert_eq!(kept, vec![3]);
    skipped.sort_by_key(|&(pc, _)| pc);
    assert_eq!(
        skipped,
        vec![(7, LambdaSiteSkip::CapturingOff), (11, LambdaSiteSkip::CapturingOff)]
    );
}

#[test]
fn unplanned_names_only_admitted_sites_without_a_call() {
    let got = unplanned([3usize, 7, 11].into_iter(), [7usize, 7].into_iter());
    let mut pcs: Vec<usize> = got.iter().map(|&(pc, _)| pc).collect();
    pcs.sort_unstable();
    assert_eq!(pcs, vec![3, 11]);
    assert!(got.iter().all(|&(_, why)| why == LambdaSiteSkip::NoPlannedCall));
}

// LAMBDACAP21-2: the in-splice graft's rows.

fn impls_by_bridge(bridge: usize) -> Option<IndyLambdaImplAnswer> {
    match bridge {
        1 => Some(answer(0, 9, "()I")),
        2 => Some(answer(1, 9, "(I)I")),
        3 => Some(answer(1, 9, "(I)I")),
        _ => None,
    }
}

#[test]
fn spliced_body_admits_the_graft_rows_and_names_each_refusal() {
    let word_rows = [
        (10, 0, b'L', 1), // admitted, non-capturing
        (20, 0, b'I', 1), // not-reference
        (30, 0, b'L', 0), // no-bridge-site
        (40, 0, b'L', 7), // no-impl-answer
        (50, 0, b'L', 2), // impl-capture-count (the VM row captures one)
        (60, 1, b'L', 2), // decided by its capture row: no word-row line
        (70, 1, b'L', 2), // capturing, no capture row: capture-new-off
    ];
    let capture_rows = vec![
        (60, 9u32, vec![b'I']), // admitted, capturing
        (80, 8u32, vec![b'I']), // impl-proxy-class
        (90, 9u32, vec![b'J']), // capture-tags
        (95, 9u32, vec![b'I']), // no-bridge-site (no capture site row)
    ];
    let capture_sites = [(60, 2), (80, 3), (90, 3)];
    let mut skipped = Vec::new();
    let lambdas = spliced_body_lambdas(
        &word_rows,
        &capture_rows,
        &capture_sites,
        true,
        &impls_by_bridge,
        &mut skipped,
    );
    let mut admitted: Vec<(usize, usize)> = lambdas.iter().map(|(&pc, r)| (pc, r.0)).collect();
    admitted.sort_unstable();
    assert_eq!(admitted, vec![(10, 0), (60, 1)]);
    skipped.sort_by_key(|&(pc, _)| pc);
    assert_eq!(
        skipped,
        vec![
            (20, LambdaSiteSkip::NotReference),
            (30, LambdaSiteSkip::NoBridgeSite),
            (40, LambdaSiteSkip::NoImplAnswer),
            (50, LambdaSiteSkip::ImplCaptureCount),
            (70, LambdaSiteSkip::CaptureNewOff),
            (80, LambdaSiteSkip::ImplProxyClass),
            (90, LambdaSiteSkip::CaptureTags),
            (95, LambdaSiteSkip::NoBridgeSite),
        ]
    );
}

#[test]
fn spliced_capture_rows_wait_for_the_capturing_switch() {
    let capture_rows = vec![(60, 9u32, vec![b'I'])];
    let mut skipped = Vec::new();
    let lambdas =
        spliced_body_lambdas(&[], &capture_rows, &[(60, 2)], false, &impls_by_bridge, &mut skipped);
    assert!(lambdas.is_empty());
    assert_eq!(skipped, vec![(60, LambdaSiteSkip::CapturingOff)]);
}

#[test]
fn a_refused_body_names_each_lambda_row_once() {
    let rows = refused_body_rows(&[(10, 0, b'L', 1), (60, 1, b'L', 2)], &[(60, 9, vec![b'I'])]);
    assert_eq!(
        rows,
        vec![(10, LambdaSiteSkip::SplicedBodyRefused), (60, LambdaSiteSkip::SplicedBodyRefused)]
    );
}

#[test]
fn skip_lines_are_sorted_by_pc_with_the_census_word() {
    let lines = skip_lines(&[
        (12, LambdaSiteSkip::NoImplAnswer),
        (4, LambdaSiteSkip::IndyDirect),
    ]);
    assert_eq!(lines, vec!["pc=4 reason=indy-direct", "pc=12 reason=no-impl-answer"]);
}
