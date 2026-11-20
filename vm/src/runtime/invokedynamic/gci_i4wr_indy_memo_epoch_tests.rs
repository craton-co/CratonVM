fn production() -> String {
    include_str!("dbg_indy_all.rs").replace("\r\n", "\n")
}

fn body_of(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest.find("\n    }\n").unwrap_or(rest.len());
    rest[..stop].to_string()
}

#[test]
fn gci_i4wr_the_indy_direct_memo_keys_on_the_address_epoch_at_both_ends() {
    let src = production();
    let check = &src[src
        .find("pub unsafe fn jit_indy_linked_direct_leaf(")
        .expect("the check")..];
    let check = &check[..check.find("\n}\n").expect("end of the check")];
    assert!(check.contains("m.epoch != shared.mem.address_epoch()"));
    let fill = body_of(&src, "fn fill_direct(");
    assert!(fill.contains("let epoch = shared.mem.address_epoch();"));
    for b in [check, fill.as_str()] {
        assert!(
            !b.contains(concat!("heap.collection", "_count()")),
            "the memo is back on the collection count"
        );
    }
}
