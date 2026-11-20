use super::string_line_ranges;

fn lines(text: &[u16]) -> Vec<Vec<u16>> {
    string_line_ranges(text)
        .into_iter()
        .map(|(s, e)| text[s..e].to_vec())
        .collect()
}

fn u(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

#[test]
fn terminators_and_the_trailing_line() {
    assert!(lines(&u("")).is_empty());
    assert_eq!(lines(&u("\n")), vec![u("")]);
    assert_eq!(lines(&u("a\n")), vec![u("a")]);
    assert_eq!(lines(&u("a\r\n\r\nb")), vec![u("a"), u(""), u("b")]);
    assert_eq!(lines(&u("a\rb\nc")), vec![u("a"), u("b"), u("c")]);
    assert_eq!(lines(&u("a\n\rb")), vec![u("a"), u(""), u("b")]);
}

/// The defect the units path fixes: a lone surrogate survives the cut.
#[test]
fn an_unpaired_surrogate_is_kept() {
    let text = [0x0078, 0xD800, 0x000A, 0x0079];
    assert_eq!(lines(&text), vec![vec![0x0078, 0xD800], vec![0x0079]]);
}
