use super::caller_frame_never_skipped;

#[test]
fn an_ordinary_caller_is_answered_and_every_skippable_one_is_not() {
    let invoke = "java/lang/reflect/Method";
    assert!(caller_frame_never_skipped(invoke, "app/Main"));
    assert!(caller_frame_never_skipped(invoke, "java/util/HashMap"));
    assert!(caller_frame_never_skipped(
        "java/lang/Class",
        "java/lang/reflect/Proxy"
    ));
    // `Method.invoke` itself, the accessors, the adapter frames.
    assert!(!caller_frame_never_skipped(
        invoke,
        "java/lang/reflect/Method"
    ));
    assert!(!caller_frame_never_skipped(
        invoke,
        "jdk/internal/reflect/DirectMethodHandleAccessor"
    ));
    assert!(!caller_frame_never_skipped(
        invoke,
        "java/lang/invoke/LambdaForm$MH"
    ));
    // A stream-plumbing asker: the whole rule's collapsed-chain skip.
    assert!(!caller_frame_never_skipped(
        "java/util/stream/ReferencePipeline",
        "app/Main"
    ));
    assert!(!caller_frame_never_skipped(
        "java/util/ServiceLoader",
        "app/Main"
    ));
}
