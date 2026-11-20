use super::*;
use crate::test_utils::mock_ctx;

fn ncdfe(name: &str) -> MethodCallFailed {
    LinkageError::NoClassDefFoundError {
        class_name: name.to_string(),
    }
    .into()
}

fn ncdfe_name(failed: &MethodCallFailed) -> Option<&str> {
    match failed {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
            LinkageError::NoClassDefFoundError { class_name },
        )) => Some(class_name.as_str()),
        _ => None,
    }
}

/// No recorded miss, or a miss of another supertype: the define's error
/// is handed back unchanged (no allocation, no new throwable).
#[test]
fn only_the_missed_supertypes_error_gets_a_cause() {
    let mut ctx = mock_ctx();
    let none = SupertypeAnswers::default();
    let back = supertype_miss_with_cause(&mut ctx, ncdfe("p/Gone"), &none);
    assert_eq!(ncdfe_name(&back), Some("p/Gone"));

    let other = SupertypeAnswers {
        miss_cause: Some((
            "p/Other".to_string(),
            "java/lang/ClassNotFoundException".to_string(),
            Some("p.Other".to_string()),
        )),
        ..SupertypeAnswers::default()
    };
    let back = supertype_miss_with_cause(&mut ctx, ncdfe("p/Gone"), &other);
    assert_eq!(ncdfe_name(&back), Some("p/Gone"));
    // Not a NoClassDefFoundError at all.
    let cfe: MethodCallFailed = LinkageError::ClassCircularityError {
        class_name: "p/Other".to_string(),
    }
    .into();
    let back = supertype_miss_with_cause(&mut ctx, cfe, &other);
    assert!(ncdfe_name(&back).is_none());
}

/// JIT round 22 wave 1, lane review
/// (`CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE_ORIGINAL`): the define natives'
/// answers keep the loader's own object under a global root, hand it out
/// as the cause, and release it; any other user of the answers roots
/// nothing.
#[test]
fn the_loaders_own_exception_is_kept_and_released() {
    use cratonvm_native_api::NativeHeapAccess;
    let mut ctx = mock_ctx();
    let thrown = ctx.create_string("stands in for the loader's ClassNotFoundException");
    let mut kept = SupertypeAnswers {
        keep_miss_object: true,
        ..SupertypeAnswers::default()
    };
    kept.miss_root = ctx.add_global_root(thrown);
    assert_eq!(kept.miss_object(&ctx), Some(thrown));
    kept.release_miss_root(&mut ctx);
    assert_eq!(kept.miss_root, 0);
    assert_eq!(kept.miss_object(&ctx), None, "released");
    kept.release_miss_root(&mut ctx); // idempotent

    let plain = SupertypeAnswers::default();
    assert!(!plain.keep_miss_object, "Lookup.defineClass's answers root nothing");
    assert_eq!(plain.miss_object(&ctx), None);
}
