use super::{compile_java_regex, AsciiRun, AsciiRunSearch};

fn parse(pattern: &str) -> Option<AsciiRun> {
    AsciiRun::parse(&pattern.encode_utf16().collect::<Vec<_>>(), 0)
}

#[test]
fn declines_context_flags_and_ambiguous_repetitions() {
    for pattern in [
        "", "a", "a*", "a++", "a+?", "^a+", "a+$", "(a)+", "(a+)(b+)", "(?i)a+", "a.*b|a",
        "[a&&b]+", "[^a]+", "[\\d]+", "[z-a]+", "é+",
    ] {
        assert!(parse(pattern).is_none(), "{pattern}");
    }
    assert!(AsciiRun::parse(&[b'a' as u16, b'+' as u16], 2).is_none());
}

#[test]
fn long_searches_and_long_matches_require_the_full_engine() {
    let run = parse(r"(\d+)").unwrap();
    assert_eq!(run.search(&vec![b'x'; 256], 0), AsciiRunSearch::NotFound);
    assert_eq!(run.search(&vec![b'x'; 257], 0), AsciiRunSearch::NeedsEngine);
    assert_eq!(
        run.search(&vec![b'1'; 256], 0),
        AsciiRunSearch::Found(0, 256)
    );
    assert_eq!(run.search(&vec![b'1'; 257], 0), AsciiRunSearch::NeedsEngine);
    let mut text = vec![b'x'; 256];
    text.extend_from_slice(b"123 ");
    assert_eq!(run.search(&text, 0), AsciiRunSearch::NeedsEngine);
    assert_eq!(run.search(&text, 256), AsciiRunSearch::Found(256, 259));
}

#[test]
fn bounded_results_equal_the_independent_regex_engine() {
    for pattern in [
        r"\d+",
        r"(\d+)",
        r"(?:\d+)",
        r"\w+",
        r"(\s+)",
        "[a-z]+",
        "([A-Za-z0-9_]+)",
        "[A-z]+",
        "[0-9]+",
        "a+",
    ] {
        let run = parse(pattern).unwrap();
        let reference = compile_java_regex(pattern, 0).unwrap();
        for text in [
            "",
            "1 22 333",
            "abc_123 XYz",
            "\t\n\r\u{b}\u{c} ",
            "é𐐀١a12_b",
            "[\\]^_`a Z",
        ] {
            for start in 0..=text.len() {
                if !text.is_char_boundary(start) {
                    continue;
                }
                let mut ranges = Vec::new();
                let count = reference
                    .visit_capture_ranges_at(text, start, |_, range| ranges.push(range));
                match run.search(text.as_bytes(), start) {
                    AsciiRunSearch::Found(a, b) => {
                        assert_eq!(count, Some(run.captures), "{pattern} {text:?} {start}");
                        assert!(ranges.iter().all(|range| *range == Some((a, b))));
                    }
                    AsciiRunSearch::NotFound => {
                        assert!(count.is_none(), "{pattern} {text:?} {start}")
                    }
                    AsciiRunSearch::NeedsEngine => panic!("short subject exceeded the bound"),
                }
            }
        }
    }
}
