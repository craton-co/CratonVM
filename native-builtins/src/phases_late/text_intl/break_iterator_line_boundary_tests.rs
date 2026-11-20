use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const HELP_TEXT: &str =
    "specified, --user is used, and the env variable KC_CLI_PASSWORD is not defined";
const PICOCLI_BREAK_TEXT: &str =
    "specified, \u{00ff}\u{00ff}user is used, and the env variable KC_CLI_PASSWORD is not defined";

fn collect_line_boundaries(text: &str) -> Vec<usize> {
    let text_len = java_text_len(text);
    let mut pos = 0;
    let mut out = Vec::new();
    while let Some(next) = bi_find_next(text, pos, BI_LINE) {
        assert!(next > pos, "line boundary did not advance from {pos}");
        out.push(next);
        if next == text_len {
            break;
        }
        pos = next;
    }
    out
}

#[test]
fn line_next_boundaries_match_hotspot_for_cli_help_text() {
    assert_eq!(
        collect_line_boundaries(HELP_TEXT),
        vec![11, 13, 18, 21, 27, 31, 35, 39, 48, 64, 67, 71, 78]
    );
}

#[test]
fn line_boundaries_do_not_split_env_var_after_whitespace() {
    assert_eq!(bi_find_next(HELP_TEXT, 47, BI_LINE), Some(48));
    assert_eq!(bi_find_next(HELP_TEXT, 48, BI_LINE), Some(64));
    assert_eq!(bi_find_prev(HELP_TEXT, 48, BI_LINE), Some(39));
    assert_eq!(bi_find_prev(HELP_TEXT, 49, BI_LINE), Some(48));
}

#[test]
fn line_boundaries_treat_hyphen_runs_as_one_separator() {
    assert_eq!(bi_find_next(HELP_TEXT, 11, BI_LINE), Some(13));
    assert_eq!(bi_find_prev(HELP_TEXT, 13, BI_LINE), Some(11));
    assert_eq!(bi_find_prev(HELP_TEXT, 14, BI_LINE), Some(13));
}

#[test]
fn line_boundaries_return_java_offsets_for_non_ascii_replacement_text() {
    assert_eq!(
        collect_line_boundaries(PICOCLI_BREAK_TEXT),
        vec![11, 18, 21, 27, 31, 35, 39, 48, 64, 67, 71, 78]
    );
    assert_eq!(bi_find_next(PICOCLI_BREAK_TEXT, 39, BI_LINE), Some(48));
    assert_eq!(bi_find_next(PICOCLI_BREAK_TEXT, 48, BI_LINE), Some(64));
    assert_eq!(bi_find_prev(PICOCLI_BREAK_TEXT, 48, BI_LINE), Some(39));
    assert_eq!(bi_find_prev(PICOCLI_BREAK_TEXT, 49, BI_LINE), Some(48));
}
