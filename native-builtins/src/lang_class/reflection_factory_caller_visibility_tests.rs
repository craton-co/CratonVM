use super::is_reflection_internal_frame;

/// `ReflectionFactory.generateConstructor` calls `setAccessible(true)` on
/// the constructor it just generated. If its frame is skipped as
/// "reflection plumbing", the caller walk continues out to the application
/// class, the accessor is judged to be the unnamed module, and
/// `newConstructorForSerialization` throws
/// `InaccessibleObjectException: module java.base does not "opens
/// java.util" to unnamed module` -- where HotSpot succeeds with no
/// `--add-opens`, on both JDK images and in both modes.
#[test]
fn both_reflection_factory_frames_stay_visible_as_the_caller() {
    assert!(
        !is_reflection_internal_frame("jdk/internal/reflect/ReflectionFactory"),
        "the jdk.internal delegate must stay visible; it is the frame that \
         actually calls setAccessible (ReflectionFactory.java:437)"
    );
    assert!(
        !is_reflection_internal_frame("sun/reflect/ReflectionFactory"),
        "the jdk.unsupported entry point must stay visible too, or the walk \
         simply skips to the application class one frame later"
    );
}

/// The controls that make the test above mean something. These are the
/// accessor classes the skip list exists FOR: if they stopped being
/// skipped, every `Method.invoke` would be attributed to reflection
/// plumbing instead of to the real user caller, and the deep-reflection
/// gate would trust user code.
#[test]
fn the_real_reflection_plumbing_is_still_skipped() {
    for plumbing in [
        "jdk/internal/reflect/NativeMethodAccessorImpl",
        "jdk/internal/reflect/DelegatingMethodAccessorImpl",
        "jdk/internal/reflect/GeneratedConstructorAccessor3",
        "java/lang/reflect/Method",
        "java/lang/reflect/Constructor",
        "java/lang/invoke/MethodHandles",
        "java/lang/AccessibleObject",
        "java/lang/Class",
    ] {
        assert!(
            is_reflection_internal_frame(plumbing),
            "{plumbing} must stay skipped -- it really does sit between a \
             caller and a reflection native"
        );
    }
}

/// The exception is spelled as exact class names rather than a package
/// prefix, so the rest of both packages keeps its existing treatment. A
/// prefix entry here would be a fail-open change the moment another
/// accessor-like class lands in either package.
#[test]
fn the_exception_did_not_widen_to_the_whole_package() {
    // A DIFFERENT class in the same two packages is still skipped, which
    // is what "narrow on purpose" means.
    assert!(is_reflection_internal_frame(
        "jdk/internal/reflect/Reflection"
    ));
    assert!(is_reflection_internal_frame(
        "sun/reflect/annotation/AnnotationParser"
    ));
    // ... while the pre-existing MethodUtil carve-out is untouched.
    assert!(!is_reflection_internal_frame("sun/reflect/misc/MethodUtil"));
}
