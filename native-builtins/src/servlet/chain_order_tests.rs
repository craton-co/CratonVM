use super::order_chain_from_leaf;

/// leaf <- i1 <- i2 <- root(self-signed)
fn names(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (subject, issuer): (&[u8], &[u8]) = match der {
        b"leaf" => (b"L", b"I1"),
        b"i1__" => (b"I1", b"I2"),
        b"i2__" => (b"I2", b"RT"),
        b"root" => (b"RT", b"RT"),
        b"xxxx" => (b"XX", b"YY"),
        _ => return None,
    };
    Some((subject.to_vec(), issuer.to_vec()))
}

fn v(items: &[&[u8]]) -> Vec<Vec<u8>> {
    items.iter().map(|b| b.to_vec()).collect()
}

#[test]
fn a_shuffled_pool_is_walked_back_into_a_path() {
    let out = order_chain_from_leaf(b"leaf".to_vec(), v(&[b"i2__", b"root", b"i1__"]), names);
    assert_eq!(out, v(&[b"leaf", b"i1__", b"i2__", b"root"]));
}

/// The order the live store happens to use today must survive unchanged —
/// a walk that only works on shuffled input would be worse than none.
#[test]
fn an_already_ordered_pool_is_left_alone() {
    let out = order_chain_from_leaf(b"leaf".to_vec(), v(&[b"i1__", b"i2__", b"root"]), names);
    assert_eq!(out, v(&[b"leaf", b"i1__", b"i2__", b"root"]));
}

/// A certificate that is not on the path is APPENDED. Dropping it would
/// take a cross-certificate away from `x509_manager::validate_chain`,
/// which builds its own path and may need exactly that one.
#[test]
fn a_certificate_off_the_path_is_kept_at_the_end() {
    let out = order_chain_from_leaf(b"leaf".to_vec(), v(&[b"xxxx", b"i1__"]), names);
    assert_eq!(out, v(&[b"leaf", b"i1__", b"xxxx"]));
}

/// A self-issued certificate ends the path. Following `subject == issuer`
/// one step further would append the root to itself forever.
#[test]
fn a_self_issued_root_terminates_the_walk() {
    let out = order_chain_from_leaf(b"root".to_vec(), v(&[b"root"]), names);
    assert_eq!(out, v(&[b"root", b"root"]));
}

/// An unparseable member cannot stall the walk or vanish.
#[test]
fn an_unparseable_member_is_kept_and_skipped() {
    let out = order_chain_from_leaf(b"leaf".to_vec(), v(&[b"junk", b"i1__"]), names);
    assert_eq!(out, v(&[b"leaf", b"i1__", b"junk"]));
}

/// An empty pool is the one-certificate chain, unchanged.
#[test]
fn an_empty_pool_yields_the_leaf_alone() {
    let out = order_chain_from_leaf(b"leaf".to_vec(), Vec::new(), names);
    assert_eq!(out, v(&[b"leaf"]));
}
