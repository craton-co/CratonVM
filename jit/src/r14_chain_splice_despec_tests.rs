use super::ir_splice_site_withdrawn;
use crate::deopt::{speculation_id, DeoptReason, DespecRegistry, SPECULATION_ID_ANY};

const KEY: &str = "T.walk:()I";

#[test]
fn a_call_site_wildcard_withdraws_the_splice() {
    let registry = DespecRegistry::new();
    assert!(!ir_splice_site_withdrawn(Some(&registry), KEY, 7));
    registry.insert(KEY, 7, SPECULATION_ID_ANY);
    assert!(ir_splice_site_withdrawn(Some(&registry), KEY, 7));
    // Another site, another method and no registry keep splicing.
    assert!(!ir_splice_site_withdrawn(Some(&registry), KEY, 12));
    assert!(!ir_splice_site_withdrawn(Some(&registry), "T.other:()I", 7));
    assert!(!ir_splice_site_withdrawn(None, KEY, 7));
}

#[test]
fn a_named_withdrawal_at_the_invoke_keeps_the_splice() {
    // The receiver guard's / trap miss's own `(bci, UncommonTrap)` entry is
    // read by its own consumer; it must not drop the whole splice.
    let registry = DespecRegistry::new();
    registry.insert(KEY, 7, speculation_id(7, DeoptReason::UncommonTrap));
    assert!(!ir_splice_site_withdrawn(Some(&registry), KEY, 7));
}

fn site(base: usize, code_len: usize, key: &str, class_id: u32) -> crate::ir::IrInlineSite {
    crate::ir::IrInlineSite {
        base,
        code_len,
        num_args: 1,
        max_locals: 2,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: true,
        method_key: key.to_string(),
        class_id,
    }
}

/// R14DP-5: each spliced body's own bytes, padded, one row per key; a key
/// two sites resolved differently, and a site with no class id, keep none.
#[test]
fn splice_scope_sources_keep_each_body_once_and_drop_disagreeing_keys() {
    use std::collections::HashMap;
    // Caller code [0..4), then three bodies.
    let combined: Vec<u8> = vec![
        0xb6, 0, 1, 0xac, // caller
        0x2a, 0xb4, 0, 2, 0xac, // A.get at 4..9
        0x2a, 0xb4, 0, 2, 0xac, // A.get again (same bytes) at 9..14
        0x03, 0xac, // B.zero at 14..16
        0x04, 0xac, // B.zero with other bytes at 16..18
        0, 0,
    ];
    let mut sites = HashMap::new();
    sites.insert(0, site(4, 5, "A.get:()I", 11));
    sites.insert(100, site(9, 5, "A.get:()I", 11));
    sites.insert(101, site(14, 2, "B.zero:()I", 12));
    sites.insert(102, site(16, 2, "B.zero:()I", 12));
    sites.insert(103, site(14, 2, "C.none:()I", 0));
    let none = HashMap::new();
    let no_tables = HashMap::new();
    let rows = super::splice_scope_sources(&sites, &combined, &none, &no_tables);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].method_key, "A.get:()I");
    assert_eq!(rows[0].class_id, 11);
    assert_eq!(&*rows[0].code, &[0x2a, 0xb4, 0, 2, 0xac, 0, 0][..]);
    assert_eq!(rows[0].max_locals, 2);
    assert!(!rows[0].is_static);
    // A body past the buffer is skipped, not sliced.
    let mut out_of_range = HashMap::new();
    out_of_range.insert(0, site(18, 9, "D.x:()I", 13));
    assert!(
        super::splice_scope_sources(&out_of_range, &combined, &none, &no_tables).is_empty()
    );
}

/// Round rx27 (lane matcher): the row a redefined chain scope is rebuilt
/// from is the bytecode the interpreter would have run. A body the
/// resolver masked (a cold throw arm: `aconst_null; nop..; athrow`) keeps
/// its ORIGINAL bytes, not the masked copy (resumed at its branch, the copy
/// would throw `NullPointerException`); a body whose original carried an
/// exception table (fence strip, handler splice, cold handler arm) keeps its
/// original bytes AND that table, so the frame the VM rebuilds from the row
/// catches its own exception (rx27 wave 3 item 3).
#[test]
fn rx27_splice_scope_sources_resume_the_original_not_the_masked_copy() {
    use std::collections::HashMap;
    // `if (c == 0) throw new X(); return 1;` (receiver in slot 0, `c` in 1).
    let original: Vec<u8> = vec![
        0x1b, 0x9a, 0x00, 0x08, 0xbb, 0x00, 0x03, 0x59, 0xbf, 0x04, 0xac,
    ];
    let masked: Vec<u8> = vec![
        0x1b, 0x9a, 0x00, 0x08, 0x01, 0x00, 0x00, 0x00, 0xbf, 0x04, 0xac,
    ];
    let mut combined: Vec<u8> = vec![0xb6, 0, 1, 0xac];
    combined.extend_from_slice(&masked); // A.f at 4..15
    combined.extend_from_slice(&[0x03, 0xac]); // B.h at 15..17 (masked handlers)
    combined.extend_from_slice(&[0, 0]);
    let mut sites = HashMap::new();
    sites.insert(0, site(4, 11, "A.f:(I)I", 11));
    sites.insert(100, site(15, 2, "B.h:()I", 12));
    let originals = HashMap::from([(4usize, original.clone()), (15usize, vec![0x03, 0xac])]);
    let with_tables = HashMap::from([(15usize, vec![(0usize, 1usize, 1usize, 7u16)])]);
    let rows = super::splice_scope_sources(&sites, &combined, &originals, &with_tables);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].method_key, "A.f:(I)I");
    let mut want = original;
    want.extend_from_slice(&[0, 0]);
    assert_eq!(&*rows[0].code, &want[..], "the unmasked body, padded");
    assert!(rows[0].exception_table.is_empty());
    assert_eq!(rows[1].method_key, "B.h:()I");
    assert_eq!(&*rows[1].code, &[0x03, 0xac, 0, 0][..]);
    assert_eq!(rows[1].exception_table, vec![(0, 1, 1, 7)], "its own table, catch type kept");
    // A table with no unmasked copy to pair it with is left out.
    let no_originals = HashMap::new();
    let rows = super::splice_scope_sources(&sites, &combined, &no_originals, &with_tables);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].method_key, "A.f:(I)I");
}
