use super::*;

/// The one-shot arms hand a compiled impl only arguments already in its
/// parameters' representation; every mismatch is a coercion the general
/// path owns, so it must answer `false`.
#[test]
fn value_fits_param_tag_accepts_only_the_verbatim_representation() {
    for tag in *b"IZBCS" {
        assert!(value_fits_param_tag(&Value::Int(7), tag));
        assert!(!value_fits_param_tag(&Value::Long(7), tag));
        assert!(!value_fits_param_tag(&Value::Object(None), tag));
    }
    assert!(value_fits_param_tag(&Value::Long(-1), b'J'));
    assert!(!value_fits_param_tag(&Value::Int(-1), b'J'));
    assert!(value_fits_param_tag(&Value::Float(1.5), b'F'));
    assert!(!value_fits_param_tag(&Value::Double(1.5), b'F'));
    assert!(value_fits_param_tag(&Value::Double(1.5), b'D'));
    assert!(!value_fits_param_tag(&Value::Float(1.5), b'D'));
    assert!(value_fits_param_tag(&Value::Object(None), b'L'));
    assert!(value_fits_param_tag(&Value::Object(None), b'['));
    assert!(!value_fits_param_tag(&Value::Int(0), b'L'));
    assert!(!value_fits_param_tag(&Value::Uninitialized, b'L'));
    assert!(!value_fits_param_tag(&Value::Int(0), b'V'));
}

/// The withdrawn-code sweep, now over three site caches, finds nothing to
/// drop on a thread that has built no site and no impl row.
#[test]
fn empty_caches_release_nothing() {
    let key = (0x16_0002usize, 1u32, 2u32);
    LAMBDA_IMPL_BYTECODE_CACHE.with(|cache| assert!(cache.borrow().get(&key).is_none()));
    assert_eq!(release_withdrawn_lambda_code(), 0);
}
