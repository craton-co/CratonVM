//! JIT round 21 lane reflect3 (`CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO`):
//! the member record's one-entry agreement memo answers only the exact
//! (site text, layout) it was filed for.
use super::*;

fn member(desc: &str) -> MhCompiledMember {
    MhCompiledMember {
        record: std::rc::Rc::new(LinkToRecord {
            ref_kind: 6,
            class: "p/Q".to_string(),
            owner: Some(ClassId::new(7)),
            name: "m".to_string(),
            desc: desc.to_string(),
            param_count: 1,
            is_proxy: false,
            settled: std::cell::Cell::new(None),
            jit_by_id: std::cell::Cell::new(0),
            jit_leaf: std::cell::Cell::new(0),
            jit_agree: std::cell::RefCell::new(None),
        }),
    }
}

#[test]
fn a_verdict_is_answered_for_its_own_site_and_layout_only() {
    let m = member("(Ljava/lang/Object;I)I");
    let site: &'static str = "(I)I";
    assert_eq!(m.agree_memo(site, 1), None, "empty before the first filing");
    m.set_agree_memo(site, 1, true);
    assert_eq!(m.agree_memo(site, 1), Some(true));
    assert_eq!(
        m.agree_memo(site, 5),
        None,
        "another layout is not this verdict"
    );
    // JIT round 21 lane reflect4: keyed by content. Equal text at another
    // address is the same key (the verdict is a function of the text).
    // LEAK(intentional): test-only; a second `'static` copy of the text.
    let copy: &'static str = Box::leak("(I)I".to_string().into_boxed_str());
    assert_eq!(m.agree_memo(copy, 1), Some(true));
    // Different text of the same length is not the key, wherever it lives.
    let other: &'static str = "(F)F";
    assert_eq!(m.agree_memo(other, 1), None);
    // A prefix of the same text (same address, shorter) is not the key.
    assert_eq!(m.agree_memo(&site[..3], 1), None);
    // The newest filing replaces the entry.
    m.set_agree_memo(other, 1, false);
    assert_eq!(m.agree_memo(other, 1), Some(false));
    assert_eq!(m.agree_memo(site, 1), None);
}
