use super::*;

#[test]
fn names_carry_the_tier_suffix() {
    assert_eq!(
        method_name("java/lang/String", "hashCode", "()I", CodeTier::C1),
        "java/lang/String.hashCode()I [c1]"
    );
    assert_eq!(
        method_name("java/lang/String", "hashCode", "()I", CodeTier::C2),
        "java/lang/String.hashCode()I [c2]"
    );
    assert_eq!(
        method_name("a/B", "loop", "([I)J", CodeTier::Osr(Some(17))),
        "a/B.loop([I)J [osr@17]"
    );
    assert_eq!(
        method_name("a/B", "loop", "([I)J", CodeTier::Osr(None)),
        "a/B.loop([I)J [osr]"
    );
    assert_eq!(
        with_tier_suffix("lambda-adapter->0x10", CodeTier::Stub("lambda-adapter")),
        "lambda-adapter->0x10 [stub:lambda-adapter]"
    );
}

#[test]
fn sanitizing_strips_line_breaks_and_nul_but_keeps_spaces() {
    assert_eq!(sanitize_name("a\nb\r\nc\0d e"), "abcd e");
}

#[test]
fn flag_words() {
    assert!(flag_is_on(Ok("1".into())));
    assert!(flag_is_on(Ok(" TRUE ".into())));
    assert!(flag_is_on(Ok("yes".into())));
    assert!(!flag_is_on(Ok("0".into())));
    assert!(!flag_is_on(Ok("".into())));
    assert!(!flag_is_on(Err::<String, std::env::VarError>(std::env::VarError::NotPresent)));    }