/// The re-plan drops each declining splice once, keeping what an earlier
/// W5-2 rebuild already dropped.
#[test]
fn j27_spliceffm2_the_replan_skips_each_declined_splice_once() {
    let mut skip = vec![40];
    super::ffm_splice_rebuild_skip(&mut skip, vec![10, 40, 70]);
    assert_eq!(skip, vec![40, 10, 70]);
}
