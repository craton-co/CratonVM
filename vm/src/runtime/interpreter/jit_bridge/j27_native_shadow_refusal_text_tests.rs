use super::native_shadow_refusal_text;
use cratonvm_native_api::NativeKind;

#[test]
fn the_refusal_names_the_kind_and_the_intrinsic_and_keeps_the_token() {
    let both = native_shadow_refusal_text(Some(NativeKind::Intrinsic), true, true);
    assert_eq!(
        both,
        "native-shadow (kind=Intrinsic; the call is intrinsified at the site (sp, ir))"
    );
    let bare = native_shadow_refusal_text(Some(NativeKind::Bridge), false, false);
    assert_eq!(bare, "native-shadow (kind=Bridge)");
    assert_eq!(
        native_shadow_refusal_text(None, false, true),
        "native-shadow (kind=none; the call is intrinsified at the site (ir))"
    );
    for text in [both, bare] {
        // The rank script's `native-shadow\S*` reads the bare token.
        let token: String = text.chars().take_while(|c| !c.is_whitespace()).collect();
        assert_eq!(token, "native-shadow");
    }
}

#[test]
fn the_real_intrinsic_tables_answer_math_max() {
    assert!(cratonvm_jit::try_resolve_intrinsic("java/lang/Math", "max", "(II)I").is_some());
}
