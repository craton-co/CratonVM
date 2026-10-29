//! j27 wave 7 (lane T1LAM3, LAMBDA321-3; book
//! `docs/internal/proposals/jit/tier1/jit-r21-lambda3-proposals.md`): why a
//! lambda `invokedynamic` site of the method did not become an own-site
//! impl-call plan (`CRATONVM_JIT_IR_LAMBDA_IMPL_CALL`).
//!
//! [`admit_lambda_site`] is the per-site decision `ir_lambda_impl_lambdas`
//! makes (moved here unchanged, with a reason for each refusal); the door's
//! later filters drop more through [`retain_noting`], and a site admitted but
//! never planned on is named by [`unplanned`]. Under `CRATONVM_DBG_JITC` (or
//! the IR stage report) the door prints one `[ir] lambda-impl-skip` line per
//! dropped site ([`skip_lines`]). Debug output only: no compile decision
//! changes, so there is no switch.

use std::collections::{HashMap, HashSet};

use crate::compile_request::IndyLambdaImplAnswer;
use crate::{count_param_slots, indy_arg_type_tags, return_type};

/// Why a lambda `invokedynamic` site carries no planned impl call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LambdaSiteSkip {
    /// `resolve_indy_site` gave no descriptor / bridge site.
    Unresolved,
    /// The site's value is no object reference.
    NotReference,
    /// The site has no bridge site.
    NoBridgeSite,
    /// A capturing site, while the builder's inline capture allocation is
    /// off (site traps / capture-new switches) or the VM gave no resolver.
    CaptureNewOff,
    /// A capturing site whose VM `(proxy class, capture count)` fact is
    /// missing, names no class, or another capture count.
    NoCaptureFact,
    /// The VM names no static impl for the site (no answer).
    NoImplAnswer,
    /// The impl row's capture count is not the site's.
    ImplCaptureCount,
    /// The impl row names another proxy class than the capture fact.
    ImplProxyClass,
    /// The site's capture tags are not the impl's leading parameter tags.
    CaptureTags,
    /// The indy-direct plan already answers the site's constant-pool entry.
    IndyDirect,
    /// A capturing site whose impl the optimizing tier cannot splice
    /// (`CRATONVM_JIT_IR_LAMBDA_IMPL_SPLICED_CAPTURES`).
    NoImplSplice,
    /// A capturing site while `CRATONVM_JIT_IR_LAMBDA_IMPL_CAPTURING` is off.
    CapturingOff,
    /// Admitted, but no SAM call of the method was planned on it (receiver
    /// not proven, not the SAM, impl shape, the method itself, a first-call
    /// site, or a capturing site whose splice an earlier attempt dropped).
    NoPlannedCall,
    /// In a spliced body the in-splice graft does not walk (a synchronized
    /// body or one with handler rows).
    SplicedBodyRefused,
}

impl LambdaSiteSkip {
    /// The census word.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Unresolved => "unresolved",
            Self::NotReference => "not-reference",
            Self::NoBridgeSite => "no-bridge-site",
            Self::CaptureNewOff => "capture-new-off",
            Self::NoCaptureFact => "no-capture-fact",
            Self::NoImplAnswer => "no-impl-answer",
            Self::ImplCaptureCount => "impl-capture-count",
            Self::ImplProxyClass => "impl-proxy-class",
            Self::CaptureTags => "capture-tags",
            Self::IndyDirect => "indy-direct",
            Self::NoImplSplice => "no-impl-splice",
            Self::CapturingOff => "capturing-off",
            Self::NoPlannedCall => "no-planned-call",
            Self::SplicedBodyRefused => "spliced-body-refused",
        }
    }
}

/// Whether the own-site plan may stand on a lambda `invokedynamic` whose
/// descriptor is `descriptor` and bridge site `bridge_site`: `(capture
/// count, the VM's impl row)`, or the reason it may not.
///
/// The site must return a reference, have a bridge site, and the VM must
/// name its static impl (`impls`) with the site's capture count and, capture
/// by capture, the site's argument tags as the impl's leading parameter
/// tags. A capturing site also needs `captures` (the builder's inline proxy
/// allocation, `None` when off) to answer the very proxy the impl row names.
pub(crate) fn admit_lambda_site(
    descriptor: &str,
    bridge_site: usize,
    captures: Option<&dyn Fn(usize) -> Option<(u32, usize)>>,
    impls: &dyn Fn(usize) -> Option<IndyLambdaImplAnswer>,
) -> Result<(usize, IndyLambdaImplAnswer), LambdaSiteSkip> {
    let args = count_param_slots(descriptor);
    if return_type(descriptor) != b'L' {
        return Err(LambdaSiteSkip::NotReference);
    }
    if bridge_site == 0 {
        return Err(LambdaSiteSkip::NoBridgeSite);
    }
    let proxy = capture_proxy(args, bridge_site, captures)?;
    let answer = impls(bridge_site).ok_or(LambdaSiteSkip::NoImplAnswer)?;
    if answer.num_captures != args {
        return Err(LambdaSiteSkip::ImplCaptureCount);
    }
    if proxy.is_some_and(|p| p != answer.proxy_class_id) {
        return Err(LambdaSiteSkip::ImplProxyClass);
    }
    if args > 0 && !capture_tags_match(descriptor, &answer.descriptor, args) {
        return Err(LambdaSiteSkip::CaptureTags);
    }
    Ok((args, answer))
}

/// The proxy class a capturing site's inline allocation builds (`None` for
/// a non-capturing site).
fn capture_proxy(
    args: usize,
    bridge_site: usize,
    captures: Option<&dyn Fn(usize) -> Option<(u32, usize)>>,
) -> Result<Option<u32>, LambdaSiteSkip> {
    if args == 0 {
        return Ok(None);
    }
    let captures = captures.ok_or(LambdaSiteSkip::CaptureNewOff)?;
    match captures(bridge_site) {
        Some((proxy, n)) if proxy != 0 && n == args => Ok(Some(proxy)),
        _ => Err(LambdaSiteSkip::NoCaptureFact),
    }
}

fn capture_tags_match(site_descriptor: &str, impl_descriptor: &str, args: usize) -> bool {
    let site_tags = indy_arg_type_tags(site_descriptor);
    let impl_tags = indy_arg_type_tags(impl_descriptor);
    site_tags.len() == args && impl_tags.len() >= args && site_tags[..] == impl_tags[..args]
}

/// `HashMap::retain` over the admitted sites (`invokedynamic pc -> row`)
/// that records `reason` for every site `keep` drops.
pub(crate) fn retain_noting<V>(
    sites: &mut HashMap<usize, V>,
    skipped: &mut Vec<(usize, LambdaSiteSkip)>,
    reason: LambdaSiteSkip,
    mut keep: impl FnMut(&V) -> bool,
) {
    sites.retain(|&pc, row| {
        let kept = keep(row);
        if !kept {
            skipped.push((pc, reason));
        }
        kept
    });
}

/// The admitted `invokedynamic` pcs no planned SAM call stands on.
pub(crate) fn unplanned(
    admitted: impl Iterator<Item = usize>,
    planned: impl Iterator<Item = usize>,
) -> Vec<(usize, LambdaSiteSkip)> {
    let planned: HashSet<usize> = planned.collect();
    admitted
        .filter(|pc| !planned.contains(pc))
        .map(|pc| (pc, LambdaSiteSkip::NoPlannedCall))
        .collect()
}

/// j27 wave 7 (lane T1LAM3, LAMBDACAP21-2): the lambda `invokedynamic`s of
/// ONE spliced body the in-splice graft (`ir_graft_spliced_lambda_impls`,
/// `CRATONVM_JIT_IR_LAMBDA_IMPL_IN_SPLICE`) may stand on, `indy pc ->
/// (capture count, the VM's impl row)`, with every refused row's reason
/// pushed on `skipped`. The graft's own rule, moved unchanged:
///
/// * a constant-word row (`word_rows`: `(pc, argument entries, return tag,
///   bridge site)`, `InlineSite::ir_indy_rows`) qualifies when it takes no
///   argument, returns a reference, has a bridge site, and the VM names a
///   non-capturing impl for it ([`admit_spliced_word_row`]);
/// * a capture row (`capture_rows`: `(pc, proxy class, capture tags)`,
///   `InlineSite::ir_indy_capture_rows`, its bridge site in `capture_sites`)
///   qualifies under `capturing` when the VM names an impl with that capture
///   count, that proxy class and those leading tags
///   ([`admit_spliced_capture_row`]).
///
/// A capture row admitted after a word row of the same pc replaces it.
pub(crate) fn spliced_body_lambdas(
    word_rows: &[(usize, usize, u8, usize)],
    capture_rows: &[(usize, u32, Vec<u8>)],
    capture_sites: &[(usize, usize)],
    capturing: bool,
    impls: &dyn Fn(usize) -> Option<IndyLambdaImplAnswer>,
    skipped: &mut Vec<(usize, LambdaSiteSkip)>,
) -> HashMap<usize, (usize, IndyLambdaImplAnswer)> {
    let mut lambdas = HashMap::new();
    for &(pc, args, ret, bridge) in word_rows {
        let has_capture_row = capture_rows.iter().any(|(p, _, _)| *p == pc);
        match admit_spliced_word_row(args, ret, bridge, has_capture_row, impls) {
            Ok(answer) => {
                lambdas.insert(pc, (0, answer));
            }
            Err(Some(why)) => skipped.push((pc, why)),
            Err(None) => {}
        }
    }
    for (pc, proxy, tags) in capture_rows {
        let bridge = capture_sites.iter().find(|(p, _)| p == pc).map(|&(_, b)| b);
        match admit_spliced_capture_row(*proxy, tags, bridge, capturing, impls) {
            Ok(answer) => {
                lambdas.insert(*pc, (tags.len(), answer));
            }
            Err(why) => skipped.push((*pc, why)),
        }
    }
    lambdas
}

/// One constant-word row of a spliced body ([`spliced_body_lambdas`]).
/// `Err(None)`: a row with arguments that a capture row of the same pc
/// decides, so nothing is said about it here.
pub(crate) fn admit_spliced_word_row(
    args: usize,
    ret: u8,
    bridge: usize,
    has_capture_row: bool,
    impls: &dyn Fn(usize) -> Option<IndyLambdaImplAnswer>,
) -> Result<IndyLambdaImplAnswer, Option<LambdaSiteSkip>> {
    if ret != b'L' {
        return Err(Some(LambdaSiteSkip::NotReference));
    }
    if args != 0 {
        return Err((!has_capture_row).then_some(LambdaSiteSkip::CaptureNewOff));
    }
    if bridge == 0 {
        return Err(Some(LambdaSiteSkip::NoBridgeSite));
    }
    let answer = impls(bridge).ok_or(Some(LambdaSiteSkip::NoImplAnswer))?;
    if answer.num_captures != 0 {
        return Err(Some(LambdaSiteSkip::ImplCaptureCount));
    }
    Ok(answer)
}

/// One capture row of a spliced body ([`spliced_body_lambdas`]); `bridge`
/// is the row's bridge site, `None` when the body has no site for it.
pub(crate) fn admit_spliced_capture_row(
    proxy: u32,
    tags: &[u8],
    bridge: Option<usize>,
    capturing: bool,
    impls: &dyn Fn(usize) -> Option<IndyLambdaImplAnswer>,
) -> Result<IndyLambdaImplAnswer, LambdaSiteSkip> {
    if !capturing {
        return Err(LambdaSiteSkip::CapturingOff);
    }
    if tags.is_empty() {
        return Err(LambdaSiteSkip::CaptureTags);
    }
    let bridge = bridge.filter(|&b| b != 0).ok_or(LambdaSiteSkip::NoBridgeSite)?;
    let answer = impls(bridge).ok_or(LambdaSiteSkip::NoImplAnswer)?;
    if answer.num_captures != tags.len() {
        return Err(LambdaSiteSkip::ImplCaptureCount);
    }
    if answer.proxy_class_id != proxy {
        return Err(LambdaSiteSkip::ImplProxyClass);
    }
    let impl_tags = indy_arg_type_tags(&answer.descriptor);
    if impl_tags.len() < tags.len() || impl_tags[..tags.len()] != tags[..] {
        return Err(LambdaSiteSkip::CaptureTags);
    }
    Ok(answer)
}

/// Every lambda row of a spliced body the graft refuses whole
/// ([`LambdaSiteSkip::SplicedBodyRefused`]).
pub(crate) fn refused_body_rows(
    word_rows: &[(usize, usize, u8, usize)],
    capture_rows: &[(usize, u32, Vec<u8>)],
) -> Vec<(usize, LambdaSiteSkip)> {
    let mut pcs: Vec<usize> = word_rows.iter().map(|r| r.0).collect();
    pcs.extend(capture_rows.iter().map(|r| r.0));
    pcs.sort_unstable();
    pcs.dedup();
    pcs.into_iter()
        .map(|pc| (pc, LambdaSiteSkip::SplicedBodyRefused))
        .collect()
}

/// The in-splice census: under `CRATONVM_DBG_JITC` (or the IR stage report)
/// one `[ir] lambda-impl-skip-in-splice <compiling method> body <spliced
/// body> pc=<body pc> reason=<word>` line per refused row.
pub(crate) fn print_spliced_skips(
    own: (&str, &str, &str),
    body: (&str, &str, &str),
    skipped: &[(usize, LambdaSiteSkip)],
) {
    if skipped.is_empty() || !(crate::ir_stage_reporting() || crate::x64::dbg_jitc_enabled()) {
        return;
    }
    for line in skip_lines(skipped) {
        eprintln!(
            "[ir] lambda-impl-skip-in-splice {}.{}{} body {}.{}{} {line}",
            own.0, own.1, own.2, body.0, body.1, body.2,
        );
    }
}

/// One `pc=<pc> reason=<word>` census tail per dropped site, by pc.
pub(crate) fn skip_lines(skipped: &[(usize, LambdaSiteSkip)]) -> Vec<String> {
    let mut rows = skipped.to_vec();
    rows.sort_by_key(|&(pc, _)| pc);
    rows.iter()
        .map(|&(pc, why)| format!("pc={pc} reason={}", why.reason()))
        .collect()
}

#[cfg(test)]
mod tests {
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
}
