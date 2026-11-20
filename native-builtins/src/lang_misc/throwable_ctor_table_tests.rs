use super::{throwable_ctor_native, THROWABLE_FAMILY_CLASSES};
use cratonvm_classloading::{throwable_ctor_descriptors, THROWABLE_DEFAULT_CTORS};

/// The stub's method table and the native registry are built from ONE list,
/// but they are in different crates and only this gate says so out loud: a
/// descriptor the stub declares with no native here is a method that
/// resolves and then has no body, which is worse than the
/// `NoSuchMethodError` it replaces because it fails later and less clearly.
#[test]
fn every_declared_throwable_ctor_has_a_native() {
    for class in THROWABLE_FAMILY_CLASSES {
        for descriptor in throwable_ctor_descriptors(class) {
            assert!(
                throwable_ctor_native(class, descriptor).is_some(),
                "{class} declares <init>{descriptor} but no native backs it",
            );
        }
    }
}

/// The measured facts this change is built on, kept where a reader can see
/// them fail. Re-derive with `probes/ThrowableCtorCensusProbe.java` after a
/// JDK bump — these are JDK 25 numbers, not invariants.
#[test]
fn the_measured_census_still_describes_the_table() {
    // `AssertionError(String)` is PRIVATE in the JDK, so registering it
    // fabricates a constructor javac will never emit — and `(Object)V`,
    // which javac emits for BOTH `new AssertionError(msg)` and
    // `assert cond : msg`, is the one that must be there.
    let assertion_error = throwable_ctor_descriptors("java/lang/AssertionError");
    assert!(assertion_error.contains(&"(Ljava/lang/Object;)V"));
    assert!(!assertion_error.contains(&"(Ljava/lang/String;)V"));
    assert!(!assertion_error.contains(&"(Ljava/lang/Throwable;)V"));

    // `UncheckedIOException` is the worst in kind: all four blanket
    // descriptors are dead on it, and neither of its two real constructors
    // was registered.
    let unchecked_io = throwable_ctor_descriptors("java/io/UncheckedIOException");
    assert_eq!(
        unchecked_io,
        [
            "(Ljava/io/IOException;)V",
            "(Ljava/lang/String;Ljava/io/IOException;)V"
        ]
    );
    for dead in THROWABLE_DEFAULT_CTORS {
        assert!(
            !unchecked_io.contains(dead),
            "{dead} is dead on UncheckedIOException"
        );
    }

    // `NoClassDefFoundError` has no cause-taking constructor at all — the
    // one in-tree caller that used `(String,Throwable)V` on it was fixed to
    // message-plus-`initCause`, which is what the JDK's own ClassLoader does.
    assert_eq!(
        throwable_ctor_descriptors("java/lang/NoClassDefFoundError"),
        ["()V", "(Ljava/lang/String;)V"]
    );

    // A class this table has never measured still gets the common four:
    // the caller's `is_throwable_like` test is a NAME heuristic and fires
    // for application classes.
    assert_eq!(
        throwable_ctor_descriptors("com/example/TotallyUnknownException"),
        THROWABLE_DEFAULT_CTORS
    );
}
