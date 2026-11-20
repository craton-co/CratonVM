use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn sentence_does_not_end_inside_dotted_identifier() {
    let text = "Server namespace has moved to spring.server";
    assert_eq!(
        bi_find_next(text, 0, BI_SENTENCE),
        Some(java_text_len(text))
    );
}

#[test]
fn sentence_ends_before_whitespace_separated_successor() {
    let text = "First sentence. Second sentence.";
    assert_eq!(bi_find_next(text, 0, BI_SENTENCE), Some(16));
}
