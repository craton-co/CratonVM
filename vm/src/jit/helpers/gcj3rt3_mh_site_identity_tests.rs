//! gcj j3/rt3 (`gcj-j2jt2-mh-site-memos-keyed-by-a-reusable-jit-invoke-info-address`):
//! the compiled `MethodHandle` site rows answer only for the site that
//! made them, after a retire re-issued their info address.

use super::{JitInvokeInfo, MhSiteIdentity};

fn mh_site(method: &'static str, descriptor: &'static str, declaring: u32) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/invoke/MethodHandle",
        method_name: method,
        descriptor,
        num_jit_args: 2,
        return_type: 0,
        invoke_kind: 0,
        declaring_class_id: declaring,
        owner_class_id: 0,
    }
}

#[test]
fn gcj3rt3_an_equal_retire_epoch_names_the_site_without_reading_it() {
    let a = mh_site("invokeExact", "(LA;)V", 40);
    let row = MhSiteIdentity::at(&a, 5);
    // Another site at the same address cannot exist at epoch 5: the epoch
    // alone answers, and the switch is never asked.
    let b = mh_site("invokeExact", "(LB;)V", 41);
    assert!(row.names(&b, 5, || panic!("the switch is read only on a mismatch")));
}

#[test]
fn gcj3rt3_after_a_retire_only_the_same_content_is_the_same_site() {
    let a = mh_site("invokeExact", "(LA;)V", 40);
    let row = MhSiteIdentity::at(&a, 5);
    assert!(!row.names(&mh_site("invokeExact", "(LB;)V", 40), 6, || true));
    assert!(!row.names(&mh_site("invokeExact", "(LA;)V", 41), 6, || true));
    assert!(!row.names(&mh_site("invoke", "(LA;)V", 40), 6, || true));
    assert_eq!(row.retire_epoch.get(), 5, "a miss does not re-stamp");
    assert!(row.names(&a, 6, || true));
    assert_eq!(row.retire_epoch.get(), 6, "a content match re-stamps");
    assert!(row.names(&mh_site("invokeExact", "(LB;)V", 41), 6, || panic!()));
}

#[test]
fn gcj3rt3_the_kill_switch_answers_as_the_address_only_rows() {
    let row = MhSiteIdentity::at(&mh_site("invokeExact", "(LA;)V", 40), 5);
    assert!(row.names(&mh_site("invoke", "(LB;)V", 41), 6, || false));
    assert_eq!(row.retire_epoch.get(), 5);
}

/// Source pins: both rows are proved before they are believed.
#[test]
fn gcj3rt3_both_mh_rows_check_identity_before_use() {
    let src = crate::jit::helpers::HELPERS_SRC;
    // The needles are split so this test's own text never matches them.
    let after = |anchor: &str, needle: &str| -> usize {
        let at = src.find(anchor).expect("anchor");
        src[at..].find(needle).expect("needle")
    };
    let warm = concat!("let warm = MH_BIND", "_SITES.with(|sites| {");
    let proof = after(warm, concat!("site.identity", ".names(info, retire_epoch"));
    let seen = after(warm, concat!("site_seen", " = true;"));
    assert!(proof < seen, "a row is seen only after it is proved");
    assert!(seen < 600, "both inside the warm path's closure");
    let linked = concat!("let linked = MH_LINKED", "_SITES.with(|s| {");
    let link_proof = concat!("row.names(info, retire_epoch, ", "mh_site_identity_enabled)");
    assert!(after(linked, link_proof) < 200);
    // JIT round 21 wave 19 (lane mhbind2) added the real-shape memo to the tuple.
    let fill = concat!(
        "let (prev_fills, prev_epoch, prev_real_shape) = MH_BIND",
        "_SITES.with("
    );
    assert!(after(fill, concat!(".filter(|s| s.identity", ".names(")) < 200);
}
