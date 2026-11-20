use super::IfaceSelectPairs;
use cratonvm_types::ClassId;

#[test]
fn a_site_holds_several_pairs_and_evicts_the_oldest() {
    let c = ClassId::new;
    let mut p = IfaceSelectPairs::single(c(1), c(10));
    assert!(p.contains(c(1), c(10)));
    assert!(!p.contains(c(1), c(11)), "both halves are compared");
    assert!(!p.contains(c(2), c(10)), "both halves are compared");
    p.add(c(2), c(10));
    p.add(c(3), c(10));
    p.add(c(4), c(11));
    for (r, d) in [(1, 10), (2, 10), (3, 10), (4, 11)] {
        assert!(p.contains(c(r), c(d)), "{r} {d}");
    }
    // A repeat adds nothing and evicts nothing.
    p.add(c(2), c(10));
    assert!(p.contains(c(1), c(10)));
    // A fifth pair evicts the oldest (the first), then the next oldest.
    p.add(c(5), c(12));
    assert!(!p.contains(c(1), c(10)));
    assert!(p.contains(c(5), c(12)) && p.contains(c(2), c(10)));
    p.add(c(6), c(12));
    assert!(!p.contains(c(2), c(10)));
    assert!(p.contains(c(3), c(10)) && p.contains(c(4), c(11)) && p.contains(c(6), c(12)));
}
