#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_types::ObjectRef;

fn u16s(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

const PATTERNS: [&str; 16] = [
    "[a-z]+\\d*", "o", "\\d+", ",", "\\s+", "a|ab", "(\\w+)=(\\w+)", "[^a-c]", "x*", "",
    "a.c", "(a)(b)?", "\\B", "ab$", "[0-9]{2,3}", "(?i)abc",
];
const SUBJECTS: [&str; 8] =
    ["abc123", "hello world 12345", "a=1 b=22", "", "zzz,y,,x", "ab", "a\nb", "xxaxx"];
const REPLACEMENTS: [&str; 5] = ["#", "", "<$0>", "$2-$1", "\\$x"];

/// The reused-locations replace walks and answers exactly as the
/// per-match `Captures` one, empty matches and groups included.
#[test]
fn replace_units_answers_what_replace_with_answers() {
    for p in PATTERNS {
        let Some(x) = java_regex_exact_subset(&u16s(p)) else {
            continue;
        };
        let Some(re) = exact_std_regex(&x.rust) else {
            continue;
        };
        for s in SUBJECTS {
            for r in REPLACEMENTS {
                for first_only in [false, true] {
                    assert_eq!(
                        exact_regex_replace_units(&x, &re, &u16s(s), s, &u16s(r), first_only),
                        exact_regex_replace_with(&x, &re, s, &u16s(r), first_only),
                        "{p:?} {s:?} {r:?} first_only={first_only}"
                    );
                }
            }
        }
    }
}

fn answer(ctx: &mut MockNativeContext, r: Option<MethodCallResult>) -> String {
    let text = |ctx: &MockNativeContext, o: ObjectRef| {
        String::from_utf16_lossy(&read_string_chars(ctx, o))
    };
    match r {
        None => "declined".to_string(),
        Some(Ok(Some(Value::Int(v)))) => format!("int {v}"),
        Some(Ok(Some(Value::Object(Some(o))))) => {
            if ctx.object_is_array(o) {
                let n = ctx.array_length(o);
                let parts: Vec<String> = (0..n)
                    .map(|i| match ctx.get_array_element(o, i) {
                        Value::Object(Some(e)) => text(&*ctx, e),
                        other => format!("{other:?}"),
                    })
                    .collect();
                format!("array {parts:?}")
            } else {
                format!("string {:?}", text(&*ctx, o))
            }
        }
        Some(other) => format!("{other:?}"),
    }
}

/// `CRATONVM_STRING_REGEX_KNOWN_STRING_READ`: the scratch path's reader
/// gives the units `decode_string_chars` gives, on both arms, and clears
/// what the buffer held before.
#[test]
fn known_string_read_matches_decode() {
    let key = concat!("CRATON", "VM_STRING_REGEX_KNOWN_STRING_READ");
    for s in ["", "abc123", "caf\u{e9} 7", "a\nb", "[a-z]+\\d*"] {
        for arm in [None, Some("0")] {
            cratonvm_types::flags::with_thread_overrides(&[(key, arm)], || {
                let mut ctx = mock_ctx();
                let obj: ObjectRef = ctx.create_string_uninterned(s);
                let mut want = Vec::new();
                decode_string_chars(&ctx, obj, &mut want);
                let mut got = vec![0x41u16; 3];
                read_regex_string_units(&ctx, obj, &mut got);
                assert_eq!(got, want, "{s:?} {arm:?}");
            });
        }
    }
}

/// `string_regex_thin` (the JIT thin helpers' door) answers exactly the
/// exact path's successful answers and declines everything else.
#[test]
fn thin_door_is_the_exact_paths_success_only() {
    let ops = [
        (StringRegexThinOp::Matches, StringRegexOp::Matches),
        (StringRegexThinOp::ReplaceAll, StringRegexOp::ReplaceAll),
        (StringRegexThinOp::ReplaceFirst, StringRegexOp::ReplaceFirst),
    ];
    for p in PATTERNS.iter().copied().chain(["[", "(?<n>a)"]) {
        for s in SUBJECTS.iter().copied().chain(["caf\u{e9}"]) {
            for (thin, op) in ops {
                let mut ctx = mock_ctx();
                // Computed text: uninterned (`r13_misc10_create_string_ratchet`).
                let this: ObjectRef = ctx.create_string_uninterned(s);
                let regex = ctx.create_string_uninterned(p);
                let repl = ctx.create_string("[$0]");
                let args = [
                    Value::Object(Some(this)),
                    Value::Object(Some(regex)),
                    Value::Object(Some(repl)),
                ];
                let want = match string_regex_exact(&mut ctx, &args, op) {
                    Some(Ok(Some(v))) => answer(&mut ctx, Some(Ok(Some(v)))),
                    _ => "declined".to_string(),
                };
                let got = string_regex_thin(&mut ctx, &args, thin).map(|v| Ok(Some(v)));
                assert_eq!(answer(&mut ctx, got), want, "{p:?} {s:?} {thin:?}");
            }
        }
    }
}

/// `CRATONVM_STRING_REGEX_SCRATCH`: every op answers the same with the
/// scratch buffers as with the allocating memo path (`=0`), asked twice so
/// the second call reuses the buffers the first one left.
#[test]
fn scratch_path_answers_what_the_allocating_path_answers() {
    let key = concat!("CRATON", "VM_STRING_REGEX_SCRATCH");
    let ops = [
        StringRegexOp::Matches,
        StringRegexOp::ReplaceAll,
        StringRegexOp::ReplaceFirst,
        StringRegexOp::Split(0),
        StringRegexOp::Split(-1),
        StringRegexOp::Split(2),
    ];
    for p in PATTERNS {
        for s in SUBJECTS.iter().copied().chain(["caf\u{e9} 12"]) {
            for op in ops {
                let run = |scratch: Option<&str>| {
                    cratonvm_types::flags::with_thread_overrides(&[(key, scratch)], || {
                        let mut ctx = mock_ctx();
                        let this: ObjectRef = ctx.create_string_uninterned(s);
                        let regex = ctx.create_string_uninterned(p);
                        let repl = ctx.create_string("<$0>");
                        let args = [
                            Value::Object(Some(this)),
                            Value::Object(Some(regex)),
                            Value::Object(Some(repl)),
                        ];
                        let first = string_regex_exact(&mut ctx, &args, op);
                        let first = answer(&mut ctx, first);
                        let again = string_regex_exact(&mut ctx, &args, op);
                        let again = answer(&mut ctx, again);
                        assert_eq!(first, again, "{p:?} {s:?} repeat");
                        first
                    })
                };
                assert_eq!(run(None), run(Some("0")), "{p:?} {s:?}");
            }
        }
    }
}
