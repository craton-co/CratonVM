use super::{field_link_site_id, field_link_site_matches_local};

/// The per-thread copy answers exactly what the table answers: on the
/// filling call, on a cached repeat, for another text of the same id, for
/// an id the table never issued, and for two ids sharing one line.
#[test]
fn the_local_copy_answers_as_the_table() {
    let a = field_link_site_id("(Lp/I91Cell;)I").expect("interned") as usize;
    let b = field_link_site_id("(Lp/I91Cell;)J").expect("interned") as usize;
    for _ in 0..2 {
        assert!(field_link_site_matches_local(a, "(Lp/I91Cell;)I"));
        assert!(!field_link_site_matches_local(a, "(Lp/I91Cell;)J"));
        assert!(field_link_site_matches_local(b, "(Lp/I91Cell;)J"));
        assert!(!field_link_site_matches_local(b, "(Lp/I91Cell;)I"));
    }
    // A site text from another allocation with the same bytes.
    let owned = String::from("(Lp/I91Cell;)I");
    assert!(field_link_site_matches_local(a, &owned));
    assert!(!field_link_site_matches_local(
        usize::MAX >> 1,
        "(Lp/I91Cell;)I"
    ));
    // Another id on `a`'s line (issued or not) evicts or misses; the text
    // is interned once, so no other id names it, and `a` still answers.
    let c = a + super::FIELD_LINK_SITE_TEXT_LINES;
    assert!(!field_link_site_matches_local(c, "(Lp/I91Cell;)I"));
    assert!(field_link_site_matches_local(a, "(Lp/I91Cell;)I"));
}
