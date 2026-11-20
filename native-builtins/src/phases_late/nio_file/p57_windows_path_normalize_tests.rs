use super::*;

#[test]
fn separator_runs_collapse_to_one() {
    // MEASURED: Paths.get("a//b").toString() == "a\b" on HotSpot;
    // CratonVM kept both separators before this fix.
    assert_eq!(p57_windows_check_and_normalize("a//b").unwrap(), "a/b");
    assert_eq!(p57_windows_check_and_normalize("a///b").unwrap(), "a/b");
    assert_eq!(
        p57_windows_check_and_normalize("C:\\a\\\\b").unwrap(),
        "C:\\a/b"
    );
}

#[test]
fn trailing_space_before_separator_or_end_refuses() {
    // MEASURED: Paths.get("a ").toString() throws on HotSpot;
    // "InvalidPathException: Trailing char < > at index 1: a ".
    let (input, reason, index) = p57_windows_check_and_normalize("a ").unwrap_err();
    assert_eq!(input, "a ");
    assert_eq!(reason, "Trailing char < >");
    assert_eq!(index, 1);

    // A space before a separator refuses the same way, mid-path.
    let (_, reason, index) = p57_windows_check_and_normalize("a /b").unwrap_err();
    assert_eq!(reason, "Trailing char < >");
    assert_eq!(index, 1);

    // A space that is NOT trailing (followed by more of the same name) is
    // legal — only a space immediately before a separator or at the very
    // end refuses.
    assert_eq!(p57_windows_check_and_normalize("a b").unwrap(), "a b");
}

#[test]
fn illegal_char_message_carries_the_char_and_index() {
    // MEASURED: HotSpot's InvalidPathException reads
    // "Illegal char <*> at index 0: *.txt"; CratonVM's read
    // "Illegal char: *.txt" (no char, no index) before this fix.
    let (input, reason, index) = p57_windows_check_and_normalize("*.txt").unwrap_err();
    assert_eq!(input, "*.txt");
    assert_eq!(reason, "Illegal char <*>");
    assert_eq!(index, 0);
}

#[test]
fn stray_colon_outside_a_drive_prefix_is_an_illegal_char_with_an_index() {
    // Previously a CratonVM-only "Illegal char <:>" with NO index (the
    // 2-arg InvalidPathException constructor). HotSpot's colon refusal is
    // the SAME reservedChars check as any other illegal character, so it
    // carries an index too.
    let (input, reason, index) = p57_windows_check_and_normalize("ping:foo").unwrap_err();
    assert_eq!(input, "ping:foo");
    assert_eq!(reason, "Illegal char <:>");
    assert_eq!(index, 4);
}

#[test]
fn drive_colon_is_still_legal_and_not_reported_as_illegal() {
    assert_eq!(
        p57_windows_check_and_normalize("C:/a/b").unwrap(),
        "C:\\a/b"
    );
    // Drive-relative (no separator after the colon) is legal syntax too.
    assert_eq!(p57_windows_check_and_normalize("C:a/b").unwrap(), "C:a/b");
}

#[test]
fn dot_segments_are_left_alone_at_construction() {
    // `Paths.get` collapses separators but does NOT resolve `.`/`..` —
    // that is `Path.normalize()`'s job, a different method entirely (see
    // `p57_normalize_path`). Collapsing the two would silently change
    // what `Paths.get("a/./b").toString()` answers.
    assert_eq!(p57_windows_check_and_normalize("a/./b").unwrap(), "a/./b");
    assert_eq!(p57_windows_check_and_normalize("a/../b").unwrap(), "a/../b");
}
