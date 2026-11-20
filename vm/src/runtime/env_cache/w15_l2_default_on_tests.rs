/// The parse behind the wave-15 default-ON switches
/// (`CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS`, `CRATONVM_JIT_PROFILE_MEMO_WIDE`;
/// `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL` until JIT r22 INLINE22-5):
/// unset is ON, and only `0` / `false` turn it off.
#[test]
fn w15_l2_default_on_parse() {
    assert!(super::default_on_from_env(None));
    assert!(super::default_on_from_env(Some("")));
    assert!(super::default_on_from_env(Some("1")));
    assert!(super::default_on_from_env(Some("true")));
    assert!(!super::default_on_from_env(Some("0")));
    assert!(!super::default_on_from_env(Some("false")));
    assert!(!super::default_on_from_env(Some("FALSE")));
}
