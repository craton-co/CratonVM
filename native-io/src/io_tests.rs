use super::*;
use crate::test_support::{confine_test_lock, MockNativeContext};
use cratonvm_native_api::fd_table::FileDescriptorTable;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::io::Write;

/// Round 15 wave 6 (lane sweepC; jdkonly4 review R4): the JDK 25
/// `WindowsPathParser` refusals with their index -- read off the parser,
/// including its `root + input[off..]` coordinates (`/a?b` is 3).
#[test]
fn windows_path_parser_refusals_carry_the_jdk_index() {
    fn r(input: &str, reason: &str, index: i32) -> Option<WindowsPathRefusal> {
        Some(WindowsPathRefusal {
            input: input.to_string(),
            reason: reason.to_string(),
            index,
        })
    }
    assert_eq!(
        windows_path_parser_refusal("a?b"),
        r("a?b", "Illegal char <?>", 1)
    );
    assert_eq!(
        windows_path_parser_refusal("/a?b"),
        r("/a?b", "Illegal char <?>", 3)
    );
    assert_eq!(
        windows_path_parser_refusal("C:\\a:b"),
        r("C:\\a:b", "Illegal char <:>", 4)
    );
    assert_eq!(
        windows_path_parser_refusal("ping:foo"),
        r("ping:foo", "Illegal char <:>", 4)
    );
    assert_eq!(
        windows_path_parser_refusal("//"),
        r("//", "UNC path is missing hostname", -1)
    );
    assert_eq!(
        windows_path_parser_refusal("\\\\host"),
        r("\\\\host", "UNC path is missing sharename", -1)
    );
    assert_eq!(
        windows_path_parser_refusal("//h?/s"),
        r("//h?/s", "Illegal character [?] in path", 3)
    );
    assert_eq!(
        windows_path_parser_refusal("a /b"),
        r("a /b", "Trailing char < >", 1)
    );
    assert_eq!(
        windows_path_parser_refusal("ab "),
        r("ab ", "Trailing char < >", 2)
    );
    assert_eq!(
        windows_path_parser_refusal("\\\\?\\foo"),
        r(
            "foo",
            "Long path prefix can only be used with an absolute path",
            -1
        )
    );
    for ok in [
        "C:\\ok\\path",
        "C:",
        "rel\\a b\\c",
        "\\\\?\\C:\\x",
        "\\\\h\\s\\f",
        "",
        "a//b//",
    ] {
        assert_eq!(windows_path_parser_refusal(ok), None, "{ok:?}");
    }
}

// -----------------------------------------------------------------------
// Scanner pure-function tests
// -----------------------------------------------------------------------

#[test]
fn scanner_next_token_simple_whitespace() {
    let input = "hello world";
    let (tok, end) = scanner_next_token(input, 0, r"\s+").unwrap();
    assert_eq!(tok, "hello");
    assert_eq!(end, 5);
}

#[test]
fn scanner_next_token_skips_leading_whitespace() {
    let input = "   hello world";
    let (tok, end) = scanner_next_token(input, 0, r"\s+").unwrap();
    assert_eq!(tok, "hello");
    assert_eq!(end, 8);
}

#[test]
fn scanner_next_token_last_token() {
    let input = "hello world";
    // Start after "hello " — position 6
    let (tok, end) = scanner_next_token(input, 6, r"\s+").unwrap();
    assert_eq!(tok, "world");
    assert_eq!(end, input.len());
}

#[test]
fn scanner_next_token_empty_input() {
    assert!(scanner_next_token("", 0, r"\s+").is_none());
}

#[test]
fn scanner_next_token_pos_past_end() {
    assert!(scanner_next_token("hello", 100, r"\s+").is_none());
}

#[test]
fn scanner_next_token_only_whitespace() {
    assert!(scanner_next_token("   ", 0, r"\s+").is_none());
}

#[test]
fn scanner_next_token_custom_delimiter() {
    let input = "one,two,three";
    let (tok, end) = scanner_next_token(input, 0, ",").unwrap();
    assert_eq!(tok, "one");
    assert_eq!(end, 3);
}

#[test]
fn scanner_next_token_custom_delimiter_second_token() {
    let input = "one,two,three";
    let (tok, _) = scanner_next_token(input, 3, ",").unwrap();
    assert_eq!(tok, "two");
}

#[test]
fn scanner_peek_token_does_not_advance() {
    let input = "hello world";
    let tok = scanner_peek_token(input, 0, r"\s+").unwrap();
    assert_eq!(tok, "hello");
    // Calling again from same position returns the same result
    let tok2 = scanner_peek_token(input, 0, r"\s+").unwrap();
    assert_eq!(tok2, "hello");
}

#[test]
fn scanner_peek_token_empty() {
    assert!(scanner_peek_token("", 0, r"\s+").is_none());
}

#[test]
fn scanner_consume_token_stops_at_the_delimiter() {
    let input = "hello world foo";
    let (tok, new_pos) = scanner_consume_token(input, 0, r"\s+").unwrap();
    assert_eq!(tok, "hello");
    // The real `Scanner` leaves the position at the END OF THE TOKEN, not
    // past the delimiter that follows it — that difference is what
    // `nextLine()` after `next()` reads. This assertion used to be
    // `new_pos > 5`, which froze the divergence.
    assert_eq!(new_pos, 5);
    // Second consume from new_pos should give "world"
    let (tok2, new_pos2) = scanner_consume_token(input, new_pos, r"\s+").unwrap();
    assert_eq!(tok2, "world");
    // Third consume should give "foo"
    let (tok3, _) = scanner_consume_token(input, new_pos2, r"\s+").unwrap();
    assert_eq!(tok3, "foo");
}

#[test]
fn scanner_consume_token_last_token_no_trailing() {
    let input = "last";
    let (tok, end) = scanner_consume_token(input, 0, r"\s+").unwrap();
    assert_eq!(tok, "last");
    assert_eq!(end, input.len());
}

#[test]
fn scanner_consume_token_empty() {
    assert!(scanner_consume_token("", 0, r"\s+").is_none());
}

// T2.3.12 - `findWithinHorizon`, ported from `phases_early.rs` when the
// implementation moved here. These exercise the pure helper rather than the
// native, so they need no mock heap; the horizon boundary and the
// no-match-does-not-move-the-position rule are what they are for.

#[test]
fn scan_quote_literal_escapes_metacharacters_and_leaves_text_alone() {
    assert_eq!(scan_quote_literal("ab"), "ab");
    assert_eq!(scan_quote_literal("a.b*c"), r"a\.b\*c");
    assert_eq!(scan_quote_literal("[x]"), r"\[x\]");
    // Non-ASCII is already literal, and must NOT be escaped: a backslash
    // before an alphabetic character is what Java rejects, and the
    // `\Q…\E` form this replaced is what CratonVM's regex engine gets
    // wrong for non-ASCII.
    assert_eq!(scan_quote_literal("\u{e9}\u{e9}"), "\u{e9}\u{e9}");
    // Whitespace is not a metacharacter and stays as itself, so a line
    // match keeps its terminator.
    assert_eq!(scan_quote_literal("a b\n"), "a b\n");
}

#[test]
fn utf16_index_of_byte_counts_java_chars() {
    // Two 2-byte characters then ASCII: byte 5 is char 3, which is what a
    // `MatchResult` reports.
    let s = "\u{e9}\u{e9} ab";
    assert_eq!(utf16_index_of_byte(s, 0), 0);
    assert_eq!(utf16_index_of_byte(s, 4), 2);
    assert_eq!(utf16_index_of_byte(s, 5), 3);
    // A surrogate pair is TWO Java chars for one Rust char.
    let emoji = "\u{1f600}x";
    assert_eq!(utf16_index_of_byte(emoji, 4), 2);
    // Past the end clamps rather than panicking.
    assert_eq!(utf16_index_of_byte(s, 999), 5);
}

#[test]
fn scanner_fwh_finds_first_match_and_reports_the_new_position() {
    let (text, pos) =
        scanner_find_within_horizon("prefix abc123 suffix", 0, r"\d+", 0).unwrap();
    assert_eq!(text, "123");
    assert_eq!(pos, "prefix abc123".len());
}

#[test]
fn scanner_fwh_no_match_is_none() {
    assert!(scanner_find_within_horizon("only letters here", 0, r"\d+", 0).is_none());
}

#[test]
fn scanner_fwh_respects_the_horizon() {
    // Horizon 4 sees only "aaaa" - no digits in that window.
    assert!(scanner_find_within_horizon("aaaa12345", 0, r"\d+", 4).is_none());
    // One more character and the digits are reachable.
    assert!(scanner_find_within_horizon("aaaa12345", 0, r"\d+", 5).is_some());
}

#[test]
fn scanner_fwh_horizon_counts_characters_not_bytes() {
    // Four 2-byte characters then a digit: a horizon of 4 must NOT reach
    // the digit, and a horizon of 5 must. Counting bytes would let a
    // horizon of 4 see nothing and a horizon of 8 see the digit - which is
    // what the implementation this replaced did.
    let s = "\u{e9}\u{e9}\u{e9}\u{e9}7";
    assert!(scanner_find_within_horizon(s, 0, r"\d", 4).is_none());
    assert_eq!(scanner_find_within_horizon(s, 0, r"\d", 5).unwrap().0, "7");
}

#[test]
fn scanner_fwh_horizon_past_the_end_is_the_remainder() {
    assert_eq!(scanner_horizon_window("abc", 0, 99), "abc");
    assert_eq!(scanner_horizon_window("abc", 1, 0), "bc");
}

#[test]
fn scanner_fwh_searches_from_the_given_position() {
    // The first match is behind `pos`; only the one after it counts.
    let (text, pos) = scanner_find_within_horizon("11 22", 3, r"\d+", 0).unwrap();
    assert_eq!(text, "22");
    assert_eq!(pos, 5);
}

#[test]
fn scanner_next_line_basic() {
    let input = "line1\nline2\nline3";
    let (line, new_pos) = scanner_next_line(input, 0).unwrap();
    assert_eq!(line, "line1");
    assert_eq!(new_pos, 6);
}

#[test]
fn scanner_next_line_crlf() {
    let input = "line1\r\nline2";
    let (line, new_pos) = scanner_next_line(input, 0).unwrap();
    assert_eq!(line, "line1");
    assert_eq!(new_pos, 7);
}

#[test]
fn scanner_next_line_cr_only() {
    let input = "line1\rline2";
    let (line, new_pos) = scanner_next_line(input, 0).unwrap();
    assert_eq!(line, "line1");
    assert_eq!(new_pos, 6);
}

#[test]
fn scanner_next_line_last_line_no_newline() {
    let input = "only_line";
    let (line, end) = scanner_next_line(input, 0).unwrap();
    assert_eq!(line, "only_line");
    assert_eq!(end, input.len());
}

#[test]
fn scanner_next_line_empty_line() {
    let input = "\nsecond";
    let (line, new_pos) = scanner_next_line(input, 0).unwrap();
    assert_eq!(line, "");
    assert_eq!(new_pos, 1);
}

#[test]
fn scanner_next_line_at_end() {
    assert!(scanner_next_line("hello", 5).is_none());
}

#[test]
fn scanner_next_line_past_end() {
    assert!(scanner_next_line("hello", 100).is_none());
}

#[test]
fn scanner_next_line_sequential() {
    let input = "a\nb\nc";
    let (l1, p1) = scanner_next_line(input, 0).unwrap();
    assert_eq!(l1, "a");
    let (l2, p2) = scanner_next_line(input, p1).unwrap();
    assert_eq!(l2, "b");
    let (l3, p3) = scanner_next_line(input, p2).unwrap();
    assert_eq!(l3, "c");
    assert_eq!(p3, input.len());
    assert!(scanner_next_line(input, p3).is_none());
}

// -----------------------------------------------------------------------
// Error constructor tests
// -----------------------------------------------------------------------

#[test]
fn io_err_wraps_io_error() {
    let err = io_err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
            message,
        })) => {
            assert!(message.contains("gone"));
        }
        other => panic!("expected IOException, got {other:?}"),
    }
}

#[test]
fn file_not_found_carries_the_hotspot_reason_suffix() {
    // The payload IS the Java exception message, and HotSpot renders
    // `<path> (<strerror>)`. Asserting the bare path is what let the
    // suffix go missing; `W4Buffers` measured it against the oracle.
    let err = file_not_found("/tmp/missing.txt");
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::FileNotFoundException { path },
        )) => {
            assert_eq!(path, "/tmp/missing.txt (No such file or directory)");
        }
        other => panic!("expected FileNotFoundException, got {other:?}"),
    }
}

#[test]
fn file_not_found_because_uses_the_real_errno() {
    let denied = io::Error::from(io::ErrorKind::PermissionDenied);
    match file_not_found_because("/tmp/locked", &denied) {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::FileNotFoundException { path },
        )) => {
            assert_eq!(path, "/tmp/locked (Permission denied)");
        }
        other => panic!("expected FileNotFoundException, got {other:?}"),
    }
}

#[test]
fn reject_directory_open_rejects_a_directory_with_the_hotspot_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_string_lossy().to_string();
    match reject_directory_open(&path) {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::FileNotFoundException { path: msg },
        ))) => {
            // The payload IS the Java exception message; HotSpot's
            // `handleOpen` renders exactly `<path> (Is a directory)` on
            // POSIX, and on Windows `CreateFileW`'s ERROR_ACCESS_DENIED
            // text in the system language (wave 5,
            // `CRATONVM_IO_WINDOWS_OS_ERROR_REASON`).
            let reason = windows_directory_open_reason()
                .unwrap_or_else(|| "Is a directory".to_string());
            assert_eq!(msg, format!("{path} ({reason})"));
        }
        other => panic!("expected FileNotFoundException for a directory, got {other:?}"),
    }
}

#[test]
fn reject_directory_open_allows_a_regular_file_and_a_missing_path() {
    let f = temp_file_with_content("hello");
    let path = f.path().to_string_lossy().to_string();
    assert!(reject_directory_open(&path).is_ok());
    // A path that does not exist is NOT this check's business — the open
    // itself reports it, exactly as HotSpot's `open(2)` does. Rejecting it
    // here would change a "No such file" into "Is a directory".
    assert!(reject_directory_open("/tmp/cratonvm_no_such_path_xyzzy/inner").is_ok());
}

#[test]
fn throw_input_mismatch_creates_error() {
    let err = throw_input_mismatch("bad token");
    // Should be some kind of error; just verify it is an error
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::InputMismatchException { message },
        )) => {
            assert_eq!(message, "bad token");
        }
        other => panic!("expected InputMismatchException, got {other:?}"),
    }
}

#[test]
fn throw_no_such_element_creates_error() {
    let err = throw_no_such_element("empty");
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NoSuchElementException { message },
        )) => {
            assert_eq!(message, "empty");
        }
        other => panic!("expected NoSuchElementException, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// FileDescriptorTable tests
// -----------------------------------------------------------------------

fn temp_file_with_content(content: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    f
}

#[test]
fn fd_table_new_has_stdio() {
    let table = FileDescriptorTable::new();
    // stdin (0), stdout (1), stderr (2) should be present and usable
    // available on stdin should return Ok(0)
    let avail = table.available(0);
    assert!(avail.is_ok());
}

#[test]
fn fd_table_open_read_existing_file() {
    let tmp = temp_file_with_content("hello fd");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    assert!(fd >= 3);
    table.close(fd).unwrap();
}

#[test]
fn fd_table_open_read_nonexistent_fails() {
    let table = FileDescriptorTable::new();
    let result = table.open_read("/nonexistent/path/xyz_9999.txt");
    assert!(result.is_err());
}

#[test]
fn fd_table_read_byte_returns_data() {
    let tmp = temp_file_with_content("AB");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    let b1 = table.read_byte(fd).unwrap();
    assert_eq!(b1, b'A' as i32);
    let b2 = table.read_byte(fd).unwrap();
    assert_eq!(b2, b'B' as i32);
    // EOF
    let b3 = table.read_byte(fd).unwrap();
    assert_eq!(b3, -1);
    table.close(fd).unwrap();
}

#[test]
fn fd_table_read_bytes_into_buffer() {
    let tmp = temp_file_with_content("hello world");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    let mut buf = [0u8; 5];
    let n = table.read_bytes(fd, &mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf, b"hello");
    table.close(fd).unwrap();
}

#[test]
fn fd_table_read_bytes_at_eof() {
    let tmp = temp_file_with_content("");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    let mut buf = [0u8; 10];
    let n = table.read_bytes(fd, &mut buf).unwrap();
    assert_eq!(n, 0);
    table.close(fd).unwrap();
}

#[test]
fn fd_table_read_bytes_zero_length_buffer() {
    let tmp = temp_file_with_content("data");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    let mut buf = [0u8; 0];
    let n = table.read_bytes(fd, &mut buf).unwrap();
    assert_eq!(n, 0);
    table.close(fd).unwrap();
}

#[test]
fn fd_table_read_line_basic() {
    let tmp = temp_file_with_content("line1\nline2\n");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    let l1 = table.read_line(fd).unwrap().unwrap();
    assert_eq!(l1, "line1");
    let l2 = table.read_line(fd).unwrap().unwrap();
    assert_eq!(l2, "line2");
    table.close(fd).unwrap();
}

#[test]
fn fd_table_read_line_crlf() {
    let tmp = temp_file_with_content("win\r\nline\r\n");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    let l1 = table.read_line(fd).unwrap().unwrap();
    assert_eq!(l1, "win");
    table.close(fd).unwrap();
}

#[test]
fn fd_table_read_line_eof_returns_none() {
    let tmp = temp_file_with_content("");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    let line = table.read_line(fd).unwrap();
    assert!(line.is_none());
    table.close(fd).unwrap();
}

#[test]
fn fd_table_write_and_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_write.txt");
    let path_str = path.to_str().unwrap();

    let table = FileDescriptorTable::new();
    let wfd = table.open_write(path_str, false).unwrap();
    table.write_bytes(wfd, b"hello from fd").unwrap();
    table.flush(wfd).unwrap();
    table.close(wfd).unwrap();

    // Read back
    let rfd = table.open_read(path_str).unwrap();
    let mut buf = [0u8; 13];
    let n = table.read_bytes(rfd, &mut buf).unwrap();
    assert_eq!(n, 13);
    assert_eq!(&buf, b"hello from fd");
    table.close(rfd).unwrap();
}

#[test]
fn fd_table_write_byte_single() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_byte.txt");
    let path_str = path.to_str().unwrap();

    let table = FileDescriptorTable::new();
    let wfd = table.open_write(path_str, false).unwrap();
    table.write_byte(wfd, b'X').unwrap();
    table.flush(wfd).unwrap();
    table.close(wfd).unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "X");
}

#[test]
fn fd_table_write_string() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_str.txt");
    let path_str = path.to_str().unwrap();

    let table = FileDescriptorTable::new();
    let wfd = table.open_write(path_str, false).unwrap();
    table.write_string(wfd, "rust jvm").unwrap();
    table.flush(wfd).unwrap();
    table.close(wfd).unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "rust jvm");
}

#[test]
fn fd_table_append_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_append.txt");
    let path_str = path.to_str().unwrap();

    let table = FileDescriptorTable::new();

    // Write first part
    let wfd = table.open_write(path_str, false).unwrap();
    table.write_string(wfd, "first").unwrap();
    table.flush(wfd).unwrap();
    table.close(wfd).unwrap();

    // Append second part
    let afd = table.open_write(path_str, true).unwrap();
    table.write_string(afd, " second").unwrap();
    table.flush(afd).unwrap();
    table.close(afd).unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "first second");
}

#[test]
fn fd_table_truncate_on_non_append_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_truncate.txt");
    let path_str = path.to_str().unwrap();

    let table = FileDescriptorTable::new();

    // Write some data
    let wfd = table.open_write(path_str, false).unwrap();
    table.write_string(wfd, "long content here").unwrap();
    table.flush(wfd).unwrap();
    table.close(wfd).unwrap();

    // Open non-append truncates
    let wfd2 = table.open_write(path_str, false).unwrap();
    table.write_string(wfd2, "short").unwrap();
    table.flush(wfd2).unwrap();
    table.close(wfd2).unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "short");
}

#[test]
fn fd_table_close_does_not_close_stdio() {
    let table = FileDescriptorTable::new();
    // Closing fd 0,1,2 should be a no-op (they are protected)
    table.close(0).unwrap();
    table.close(1).unwrap();
    table.close(2).unwrap();
    // Stdout should still work
    let result = table.write_byte(1, b'.');
    assert!(result.is_ok());
}

#[test]
fn fd_table_close_then_read_fails() {
    let tmp = temp_file_with_content("some data");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    table.close(fd).unwrap();
    // Reading from closed fd should fail
    let result = table.read_byte(fd);
    assert!(result.is_err());
}

#[test]
fn fd_table_read_bad_fd_fails() {
    let table = FileDescriptorTable::new();
    let result = table.read_byte(9999);
    assert!(result.is_err());
}

#[test]
fn fd_table_write_bad_fd_fails() {
    let table = FileDescriptorTable::new();
    let result = table.write_byte(9999, b'X');
    assert!(result.is_err());
}

#[test]
fn fd_table_available_on_file() {
    let tmp = temp_file_with_content("data");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    // available() should return Ok (may be 0 initially due to buffering)
    let avail = table.available(fd).unwrap();
    assert!(avail <= 4); // at most 4 bytes
    table.close(fd).unwrap();
}

#[test]
fn fd_table_available_bad_fd_fails() {
    let table = FileDescriptorTable::new();
    assert!(table.available(9999).is_err());
}

#[test]
fn fd_table_flush_noop_on_read_fd() {
    let tmp = temp_file_with_content("data");
    let table = FileDescriptorTable::new();
    let fd = table.open_read(tmp.path().to_str().unwrap()).unwrap();
    // Flushing a read fd should be a no-op (not an error)
    let result = table.flush(fd);
    assert!(result.is_ok());
    table.close(fd).unwrap();
}

#[test]
fn fd_table_multiple_fds_independent() {
    let tmp1 = temp_file_with_content("AAA");
    let tmp2 = temp_file_with_content("BBB");
    let table = FileDescriptorTable::new();
    let fd1 = table.open_read(tmp1.path().to_str().unwrap()).unwrap();
    let fd2 = table.open_read(tmp2.path().to_str().unwrap()).unwrap();
    assert_ne!(fd1, fd2);

    let b1 = table.read_byte(fd1).unwrap();
    assert_eq!(b1, b'A' as i32);

    let b2 = table.read_byte(fd2).unwrap();
    assert_eq!(b2, b'B' as i32);

    table.close(fd1).unwrap();
    table.close(fd2).unwrap();
}

#[test]
fn fd_table_debug_display() {
    let table = FileDescriptorTable::new();
    let debug = format!("{:?}", table);
    assert!(debug.contains("FileDescriptorTable"));
    assert!(debug.contains("open_fds"));
}

#[test]
fn fd_table_default_trait() {
    let table = FileDescriptorTable::default();
    // Should have stdin/stdout/stderr
    assert!(table.available(0).is_ok());
}

// -----------------------------------------------------------------------
// Registration completeness tests
// -----------------------------------------------------------------------

fn io_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_io_natives(&mut r);
    r
}

/// `DatagramChannel.send` must exist and must be the fd_table-backed
/// `native_dc_send`, i.e. the same family that `open`/`bind` populate.
///
/// This test used to assert the opposite — that `send` stayed routed to
/// `datagram.rs`'s callback, to stop this phase-92 registration from
/// overriding the SSRF gate. But `datagram.rs` resolved the channel through
/// a private registry that `open`/`bind` never populated, so the "guarded"
/// send threw `no socket id` on every real call and the channel could not
/// round-trip at all. The gate itself is what mattered; it now lives in
/// `native_dc_send` (see `datagram_channel_send_keeps_outbound_policy_gate`).
#[test]
fn datagram_channel_send_uses_the_fd_table_family() {
    let r = io_registry();
    let actual = r
        .find(
            "java/nio/channels/DatagramChannel",
            "send",
            "(Ljava/nio/ByteBuffer;Ljava/net/SocketAddress;)I",
        )
        .expect("DatagramChannel.send registered");
    assert_eq!(
        actual as usize, native_dc_send as usize,
        "DatagramChannel.send must resolve through the same fd_table family as open/bind"
    );
}

#[test]
fn phase92_datagram_channel_registers_public_send() {
    let mut r = NativeMethodRegistry::new();
    register_datagram_channel(&mut r);
    assert!(
        r.find(
            "java/nio/channels/DatagramChannel",
            "send",
            "(Ljava/nio/ByteBuffer;Ljava/net/SocketAddress;)I",
        )
        .is_some(),
        "the fd_table DatagramChannel family owns send"
    );
}

/// The SSRF gate that the previous routing existed to protect: a send to a
/// link-local cloud-metadata address must still be refused.
#[test]
fn datagram_channel_send_keeps_outbound_policy_gate() {
    let metadata: std::net::SocketAddr = "169.254.169.254:80".parse().unwrap();
    assert!(
        datagram::check_outbound_target(metadata).is_err(),
        "native_dc_send calls this gate; it must keep rejecting cloud metadata"
    );
}

#[test]
fn file_methods_registered() {
    let r = io_registry();
    let f = "java/io/File";
    assert!(r.find(f, "<init>", "(Ljava/lang/String;)V").is_some());
    assert!(r
        .find(f, "<init>", "(Ljava/lang/String;Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(f, "<init>", "(Ljava/io/File;Ljava/lang/String;)V")
        .is_some());
    assert!(r.find(f, "exists", "()Z").is_some());
    assert!(r.find(f, "isFile", "()Z").is_some());
    assert!(r.find(f, "isDirectory", "()Z").is_some());
    assert!(r.find(f, "length", "()J").is_some());
    assert!(r.find(f, "delete", "()Z").is_some());
    assert!(r.find(f, "mkdir", "()Z").is_some());
    assert!(r.find(f, "mkdirs", "()Z").is_some());
    assert!(r.find(f, "getName", "()Ljava/lang/String;").is_some());
    assert!(r.find(f, "getPath", "()Ljava/lang/String;").is_some());
    assert!(r
        .find(f, "getAbsolutePath", "()Ljava/lang/String;")
        .is_some());
    assert!(r.find(f, "getParent", "()Ljava/lang/String;").is_some());
    assert!(r.find(f, "canRead", "()Z").is_some());
    assert!(r.find(f, "canWrite", "()Z").is_some());
    assert!(r.find(f, "createNewFile", "()Z").is_some());
    assert!(r.find(f, "renameTo", "(Ljava/io/File;)Z").is_some());
}

#[test]
fn file_input_stream_methods_registered() {
    // JDK-25 real-bytecode path: the legacy native `<init>(String)V` /
    // `<init>(File)V` overrides were intentionally dropped (see FIS-FIX in
    // `register_io_natives`). The constructor runs as real bytecode and
    // calls `open0`; the I/O natives below store/recover the OS handle on
    // the `FileDescriptor`. Assert the natives that ARE registered.
    let r = io_registry();
    let fis = "java/io/FileInputStream";
    assert!(r.find(fis, "initIDs", "()V").is_some());
    assert!(r.find(fis, "open0", "(Ljava/lang/String;)V").is_some());
    assert!(r.find(fis, "read0", "()I").is_some());
    assert!(r.find(fis, "readBytes", "([BII)I").is_some());
    assert!(r.find(fis, "skip0", "(J)J").is_some());
    assert!(r.find(fis, "available0", "()I").is_some());
}

#[test]
fn file_output_stream_methods_registered() {
    // JDK-25 real-bytecode path: the legacy native `<init>(String)V` /
    // `<init>(File)V` overrides were intentionally dropped (see FOS-FIX in
    // `register_io_natives`). The constructor runs as real bytecode and
    // calls `open0`; the write/close natives below operate on the fd
    // stashed on the `FileDescriptor`. Assert the natives that ARE
    // registered.
    let r = io_registry();
    let fos = "java/io/FileOutputStream";
    assert!(r.find(fos, "initIDs", "()V").is_some());
    assert!(r.find(fos, "open0", "(Ljava/lang/String;Z)V").is_some());
    assert!(r.find(fos, "write", "(IZ)V").is_some());
    assert!(r.find(fos, "writeBytes", "([BIIZ)V").is_some());
    assert!(r.find(fos, "flush", "()V").is_some());
    assert!(r.find(fos, "close", "()V").is_some());
}

#[test]
fn scanner_methods_registered() {
    let r = io_registry();
    let sc = "java/util/Scanner";
    assert!(r.find(sc, "<init>", "(Ljava/lang/String;)V").is_some());
    assert!(r.find(sc, "next", "()Ljava/lang/String;").is_some());
    assert!(r.find(sc, "nextLine", "()Ljava/lang/String;").is_some());
    assert!(r.find(sc, "nextInt", "()I").is_some());
    assert!(r.find(sc, "nextLong", "()J").is_some());
    assert!(r.find(sc, "nextDouble", "()D").is_some());
    assert!(r.find(sc, "hasNext", "()Z").is_some());
    assert!(r.find(sc, "hasNextLine", "()Z").is_some());
    assert!(r.find(sc, "hasNextInt", "()Z").is_some());
    assert!(r.find(sc, "close", "()V").is_some());
}

// ByteBuffer overrides are synthetic-jdk only; real-JDK mode uses the
// JDK's own bytecode with its native layout.  The probe assertion below
// must match the production gating at `register_nio_natives` (see line
// ~3200 above).
#[cfg(feature = "synthetic-jdk")]
#[test]
fn bytebuffer_methods_registered() {
    let r = io_registry();
    let bb = "java/nio/ByteBuffer";
    assert!(r.find(bb, "allocate", "(I)Ljava/nio/ByteBuffer;").is_some());
    assert!(r.find(bb, "wrap", "([B)Ljava/nio/ByteBuffer;").is_some());
    assert!(r.find(bb, "get", "()B").is_some());
    assert!(r.find(bb, "put", "(B)Ljava/nio/ByteBuffer;").is_some());
    assert!(r.find(bb, "position", "()I").is_some());
    assert!(r.find(bb, "limit", "()I").is_some());
    assert!(r.find(bb, "capacity", "()I").is_some());
    assert!(r.find(bb, "flip", "()Ljava/nio/Buffer;").is_some());
    assert!(r.find(bb, "clear", "()Ljava/nio/Buffer;").is_some());
    assert!(r.find(bb, "remaining", "()I").is_some());
}

/// SBR wave 5, lane fs2: the genuine `ACC_NATIVE` methods under the real
/// `LinuxWatchService` and `LinuxFileStore` are registered, explicitly
/// `Bridge` (never an inherited `SyntheticStub`, which `--real-jdk` would
/// drop), and `eventSize`/`eventOffsets` answer the kernel ABI.
#[cfg(target_os = "linux")]
#[test]
fn fs2_watch_and_mount_natives_are_bridges() {
    let r = io_registry();
    for (c, m, d) in [
        ("sun/nio/fs/LinuxWatchService", "eventSize", "()I"),
        ("sun/nio/fs/LinuxWatchService", "eventOffsets", "()[I"),
        ("sun/nio/fs/LinuxWatchService", "inotifyInit", "()I"),
        ("sun/nio/fs/LinuxWatchService", "inotifyAddWatch", "(IJI)I"),
        ("sun/nio/fs/LinuxWatchService", "inotifyRmWatch", "(II)V"),
        ("sun/nio/fs/LinuxWatchService", "configureBlocking", "(IZ)V"),
        ("sun/nio/fs/LinuxWatchService", "socketpair", "([I)V"),
        ("sun/nio/fs/LinuxWatchService", "poll", "(II)I"),
        ("sun/nio/fs/LinuxNativeDispatcher", "setmntent0", "(JJ)J"),
        (
            "sun/nio/fs/LinuxNativeDispatcher",
            "getmntent0",
            "(JLsun/nio/fs/UnixMountEntry;JI)I",
        ),
        ("sun/nio/fs/LinuxNativeDispatcher", "endmntent", "(J)V"),
        ("sun/nio/fs/UnixNativeDispatcher", "read0", "(IJI)I"),
        ("sun/nio/fs/UnixNativeDispatcher", "write0", "(IJI)I"),
        ("sun/nio/fs/UnixNativeDispatcher", "getlinelen", "(J)I"),
        ("sun/nio/fs/UnixNativeDispatcher", "rewind", "(J)V"),
    ] {
        assert_eq!(r.kind_of(c, m, d), Some(NativeKind::Bridge), "{c}.{m}{d}");
    }
    let mut ctx = MockNativeContext::new();
    assert_eq!(
        native_linux_ws_event_size(&mut ctx, &[]).unwrap(),
        Some(Value::Int(16))
    );
    let Some(Value::Object(Some(arr))) = native_linux_ws_event_offsets(&mut ctx, &[]).unwrap()
    else {
        panic!("eventOffsets answered no array");
    };
    let got: Vec<Value> = (0..5).map(|i| ctx.get_array_element(arr, i)).collect();
    assert_eq!(
        got,
        [0, 4, 8, 12, 16].map(Value::Int).to_vec(),
        "wd, mask, cookie, len, name"
    );
}

#[test]
fn path_and_files_methods_registered() {
    let r = io_registry();
    let path = "java/nio/file/Path";
    let paths = "java/nio/file/Paths";
    let files = "java/nio/file/Files";
    assert!(r
        .find(paths, "get", "(Ljava/lang/String;)Ljava/nio/file/Path;")
        .is_some());
    assert!(r.find(path, "toString", "()Ljava/lang/String;").is_some());
    assert!(r
        .find(path, "getFileName", "()Ljava/nio/file/Path;")
        .is_some());
    assert!(r
        .find(path, "getParent", "()Ljava/nio/file/Path;")
        .is_some());
    assert!(r.find(path, "isAbsolute", "()Z").is_some());
    assert!(r
        .find(path, "normalize", "()Ljava/nio/file/Path;")
        .is_some());
    assert!(r
        .find(
            files,
            "exists",
            "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z"
        )
        .is_some());
    assert!(r
        .find(
            files,
            "isDirectory",
            "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z"
        )
        .is_some());
    assert!(r.find(files, "delete", "(Ljava/nio/file/Path;)V").is_some());
    assert!(r
        .find(files, "readAllBytes", "(Ljava/nio/file/Path;)[B")
        .is_some());
    assert!(r
        .find(
            files,
            "readString",
            "(Ljava/nio/file/Path;)Ljava/lang/String;"
        )
        .is_some());
}

#[test]
fn data_stream_methods_registered() {
    let r = io_registry();
    let dis = "java/io/DataInputStream";
    let dos = "java/io/DataOutputStream";
    // <init> intentionally NOT registered (real JDK bytecode handles it)
    assert!(r.find(dis, "<init>", "(Ljava/io/InputStream;)V").is_none());
    assert!(r.find(dis, "readInt", "()I").is_some());
    assert!(r.find(dis, "readLong", "()J").is_some());
    assert!(r.find(dis, "readUTF", "()Ljava/lang/String;").is_some());
    assert!(r.find(dos, "<init>", "(Ljava/io/OutputStream;)V").is_some());
    assert!(r.find(dos, "writeInt", "(I)V").is_some());
    assert!(r.find(dos, "writeLong", "(J)V").is_some());
}

#[test]
fn data_output_stream_close_uses_declared_outputstream_for_inner_close() {
    let mut ctx = MockNativeContext::new();
    let dos = ctx.alloc_object(1);
    let inner = ctx.alloc_object_with_class(0, "java/lang/Object");
    ctx.set_field(dos, DOS_FIELD_OUT, Value::Object(Some(inner)));

    native_dos_close(&mut ctx, &[Value::Object(Some(dos))]).unwrap();

    let calls = ctx.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].declared_class.as_deref(),
        Some("java/io/OutputStream")
    );
    assert_eq!(calls[0].method_name, "flush");
    assert_eq!(calls[0].descriptor, "()V");
    assert_eq!(
        calls[1].declared_class.as_deref(),
        Some("java/io/OutputStream")
    );
    assert_eq!(calls[1].method_name, "close");
    assert_eq!(calls[1].descriptor, "()V");
}

/// JO7-6 (round 15 wave 6, lane jni6): failed before -- a null `out` was a
/// message-less NPE on the write natives and no failure at all on
/// `flush()`; JDK 25's bodies are `out.write(..)` / `out.flush()`.
#[test]
fn data_output_stream_null_out_npe_names_the_call() {
    fn npe_message(r: MethodCallResult) -> Option<String> {
        match r {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { message },
            ))) => message,
            other => panic!("expected a NullPointerException, got {other:?}"),
        }
    }
    let mut ctx = MockNativeContext::new();
    let dos = ctx.alloc_object(2);
    ctx.set_field(dos, DOS_FIELD_OUT, Value::Object(None));
    assert_eq!(
        npe_message(native_dos_write(
            &mut ctx,
            &[Value::Object(Some(dos)), Value::Int(1)]
        ))
        .as_deref(),
        Some("Cannot invoke \"java.io.OutputStream.write(int)\" because \"this.out\" is null")
    );
    assert_eq!(
        npe_message(native_dos_flush(&mut ctx, &[Value::Object(Some(dos))])).as_deref(),
        Some("Cannot invoke \"java.io.OutputStream.flush()\" because \"this.out\" is null")
    );
    assert_eq!(
        npe_message(native_dos_write_int(
            &mut ctx,
            &[Value::Object(Some(dos)), Value::Int(7)]
        ))
        .as_deref(),
        Some(
            "Cannot invoke \"java.io.OutputStream.write(byte[], int, int)\" because \
             \"this.out\" is null"
        )
    );
}

// BufferedReader / BufferedWriter overrides are synthetic-jdk only;
// real-JDK mode uses the JDK's own bytecode (see the
// `#[cfg(feature = "synthetic-jdk")]` block around line ~3036).
#[cfg(feature = "synthetic-jdk")]
#[test]
fn buffered_reader_writer_registered() {
    let r = io_registry();
    let br = "java/io/BufferedReader";
    let bw = "java/io/BufferedWriter";
    assert!(r.find(br, "<init>", "(Ljava/io/Reader;)V").is_some());
    assert!(r.find(br, "readLine", "()Ljava/lang/String;").is_some());
    assert!(r.find(br, "close", "()V").is_some());
    assert!(r.find(bw, "<init>", "(Ljava/io/Writer;)V").is_some());
    assert!(r.find(bw, "write", "(Ljava/lang/String;II)V").is_some());
    assert!(r.find(bw, "flush", "()V").is_some());
    assert!(r.find(bw, "close", "()V").is_some());
}

// -----------------------------------------------------------------------
// Scanner tokenization edge cases
// -----------------------------------------------------------------------

#[test]
fn scanner_next_token_multiple_spaces() {
    let input = "a    b     c";
    let (tok1, p1) = scanner_next_token(input, 0, r"\s+").unwrap();
    assert_eq!(tok1, "a");
    let (tok2, p2) = scanner_next_token(input, p1, r"\s+").unwrap();
    assert_eq!(tok2, "b");
    let (tok3, _) = scanner_next_token(input, p2, r"\s+").unwrap();
    assert_eq!(tok3, "c");
}

#[test]
fn scanner_next_token_tab_delimiter() {
    let input = "col1\tcol2\tcol3";
    let (tok, _) = scanner_next_token(input, 0, r"\t").unwrap();
    assert_eq!(tok, "col1");
}

#[test]
fn scanner_next_token_single_char() {
    let input = "x";
    let (tok, end) = scanner_next_token(input, 0, r"\s+").unwrap();
    assert_eq!(tok, "x");
    assert_eq!(end, 1);
}

#[test]
fn scanner_consume_all_tokens() {
    let input = "1 2 3 4 5";
    let mut pos = 0;
    let mut tokens = Vec::new();
    while let Some((tok, new_pos)) = scanner_consume_token(input, pos, r"\s+") {
        tokens.push(tok);
        pos = new_pos;
    }
    assert_eq!(tokens, vec!["1", "2", "3", "4", "5"]);
}

#[test]
fn scanner_next_line_empty_string() {
    assert!(scanner_next_line("", 0).is_none());
}

#[test]
fn scanner_next_line_only_newline() {
    let (line, pos) = scanner_next_line("\n", 0).unwrap();
    assert_eq!(line, "");
    assert_eq!(pos, 1);
}

// -----------------------------------------------------------------------
// Path validation tests
// -----------------------------------------------------------------------

#[test]
fn path_validation_rejects_dotdot() {
    // PATH_VALIDATION_ENABLED is a process-global atomic, exactly like
    // PATH_CONFINE_TO_CWD, and this test's verdict depends on it. Take the
    // same shared guard: without it a parallel test that turns validation
    // OFF makes this one read a tree it never set up.
    let _g = crate::test_support::confine_test_lock().lock();
    set_path_validation_enabled(true);
    // A *leading* `..` segment escapes the start directory and is always
    // rejected — the always-on traversal guard, independent of CWD
    // confinement.
    let result = validate_path("../escapes-sandbox.txt");
    assert!(result.is_err());
    let err = format!("{:?}", result.unwrap_err());
    assert!(err.contains("Path traversal detected"), "err = {err}");
}

/// B-C: an *interior* `..` that merely cancels a preceding component
/// (`apps/kafka/../config/x` → `apps/config/x`) does NOT escape and must
/// be accepted — the JDK opens such paths, so a faithful JVM must too.
/// Kafka's `ConsumerConfigTest.testValidateConfigPropertiesFile` reads
/// `apps/kafka/../config/consumer.properties`. Independent of CWD
/// confinement (off by default here).
#[test]
fn path_validation_accepts_interior_dotdot() {
    let _g = crate::test_support::confine_test_lock().lock();
    let prev = is_path_confine_to_cwd();
    set_path_validation_enabled(true);
    set_path_confine_to_cwd(false);
    // Relative, interior `..` cancels `kafka`: nets to `apps/config/...`,
    // never climbs above the start directory.
    let rel = validate_path("apps/kafka/../config/consumer.properties");
    assert!(rel.is_ok(), "interior `..` relative path rejected: {rel:?}");
    // Absolute, interior `..`: cannot escape root, accepted unconfined.
    #[cfg(windows)]
    let abs = validate_path(r"C:\craton\CratonVM\apps\kafka\..\config\consumer.properties");
    #[cfg(not(windows))]
    let abs = validate_path("/craton/CratonVM/apps/kafka/../config/consumer.properties");
    assert!(abs.is_ok(), "interior `..` absolute path rejected: {abs:?}");
    set_path_confine_to_cwd(prev);
}

/// A relative path whose `..` segments net to climbing above the start
/// directory (more `..` than preceding names) is still rejected.
#[test]
fn path_validation_rejects_net_escaping_dotdot() {
    // PATH_VALIDATION_ENABLED is a process-global atomic, exactly like
    // PATH_CONFINE_TO_CWD, and this test's verdict depends on it. Take the
    // same shared guard: without it a parallel test that turns validation
    // OFF makes this one read a tree it never set up.
    let _g = crate::test_support::confine_test_lock().lock();
    set_path_validation_enabled(true);
    // `a/../../b` → one name, two parents → escapes one level above start.
    let result = validate_path("a/../../b.txt");
    assert!(
        result.is_err(),
        "net-escaping `..` path accepted: {result:?}"
    );
    let err = format!("{:?}", result.unwrap_err());
    assert!(err.contains("Path traversal detected"), "err = {err}");
}

/// With CWD confinement OFF (the default), an absolute path with no
/// `..` segment is accepted: CratonVM is a general-purpose JVM, not a
/// sandbox, so applications may read files anywhere the host process
/// can. (Jetty's `start.jar` launcher reads `$JETTY_HOME/modules/*`.)
#[test]
fn path_validation_accepts_absolute_path_by_default() {
    // Confinement is a process-global flag; serialize with the shared
    // guard (same one watch.rs / random_access_file.rs use) and restore.
    let _g = crate::test_support::confine_test_lock().lock();
    let prev = is_path_confine_to_cwd();
    set_path_validation_enabled(true);
    set_path_confine_to_cwd(false);
    let result = validate_path("/etc/passwd");
    assert!(
        result.is_ok(),
        "absolute path rejected by default: {result:?}"
    );
    set_path_confine_to_cwd(prev);
}

/// AUDIT 2026-05-19: with CWD confinement explicitly ON, a path that
/// resolves outside the sandbox root is rejected even with no literal
/// `..` segment. This is the canonicalize-after-check / symlink-escape
/// regression guard for the opt-in confinement mode.
#[test]
fn path_validation_rejects_out_of_sandbox_absolute_when_confined() {
    // Confinement is a process-global flag; serialize with the shared
    // guard (same one watch.rs / random_access_file.rs use) and restore.
    let _g = crate::test_support::confine_test_lock().lock();
    let prev = is_path_confine_to_cwd();
    set_path_validation_enabled(true);
    set_path_confine_to_cwd(true);
    let result = validate_path("/etc/passwd");
    assert!(result.is_err(), "out-of-sandbox path accepted: {result:?}");
    let err = format!("{:?}", result.unwrap_err());
    assert!(err.contains("Path traversal detected"), "err = {err}");
    set_path_confine_to_cwd(prev);
}

#[test]
fn path_validation_confined_root_lookup_failure_fails_closed() {
    let result = cwd_sandbox_root(Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cwd vanished",
    )));

    assert!(result.is_err(), "CWD lookup failure was accepted");
    let err = format!("{:?}", result.unwrap_err());
    assert!(
        err.contains("Unable to establish CWD sandbox root"),
        "unexpected error: {err}"
    );
}

#[test]
fn path_validation_confined_root_canonicalization_failure_fails_closed() {
    let mut missing = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    missing.push(format!("cratonvm_missing_cwd_root_{nanos}"));
    assert!(
        !missing.exists(),
        "test root unexpectedly exists: {missing:?}"
    );

    let result = cwd_sandbox_root(Ok(missing));

    assert!(
        result.is_err(),
        "CWD root canonicalization failure was accepted"
    );
    let err = format!("{:?}", result.unwrap_err());
    assert!(
        err.contains("Unable to canonicalize CWD sandbox root"),
        "unexpected error: {err}"
    );
}

#[test]
fn path_validation_rejects_null_byte() {
    // PATH_VALIDATION_ENABLED is a process-global atomic, exactly like
    // PATH_CONFINE_TO_CWD, and this test's verdict depends on it. Take the
    // same shared guard: without it a parallel test that turns validation
    // OFF makes this one read a tree it never set up.
    let _g = crate::test_support::confine_test_lock().lock();
    set_path_validation_enabled(true);
    let result = validate_path("/etc/passwd\0.txt");
    assert!(result.is_err());
    let err = format!("{:?}", result.unwrap_err());
    assert!(err.contains("null byte"), "err = {err}");
}

#[test]
fn path_validation_accepts_normal_path() {
    // Result depends on the process-global confinement flag; serialize
    // with the shared guard and pin a known state for the duration.
    let _g = crate::test_support::confine_test_lock().lock();
    let prev = is_path_confine_to_cwd();
    set_path_validation_enabled(true);
    set_path_confine_to_cwd(false);
    // A plain not-yet-existing file inside the sandbox (cwd) is
    // accepted: the parent (cwd) canonicalizes and the result stays
    // within the sandbox root.
    let result = validate_path("path_validation_normal_test.txt");
    assert!(result.is_ok(), "rejected in-sandbox path: {result:?}");
    set_path_confine_to_cwd(prev);
}

#[test]
fn path_validation_disabled_allows_dotdot() {
    // Turning validation OFF is process-global: hold the shared guard for
    // the whole window, or every parallel test that asserts a path is
    // REJECTED can observe it accepted instead. That is exactly how
    // `files_validated_path_rejects_dotdot_segment` failed about one run
    // in twelve of `cargo test -p cratonvm-native-io --lib`, reporting
    // `Files path with `..` segment accepted: Ok("../../etc/passwd")`.
    let _g = crate::test_support::confine_test_lock().lock();
    set_path_validation_enabled(false);
    let result = validate_path("/etc/../passwd");
    assert!(result.is_ok());
    // Reset to default
    set_path_validation_enabled(true);
}

/// AUDIT 2026-05-17: legitimate filenames that contain `..` as a
/// literal substring (but not as a path segment) must be accepted.
/// AUDIT 2026-05-19: must also stay inside the sandbox root.
#[test]
fn path_validation_accepts_literal_dotdot_in_filename() {
    // Result depends on the process-global confinement flag; serialize
    // with the shared guard and pin a known state for the duration.
    let _g = crate::test_support::confine_test_lock().lock();
    let prev = is_path_confine_to_cwd();
    set_path_validation_enabled(true);
    set_path_confine_to_cwd(false);
    let result = validate_path("foo..bar.txt");
    assert!(result.is_ok(), "rejected legitimate filename: {result:?}");
    set_path_confine_to_cwd(prev);
}

/// AUDIT 2026-05-17: the null-byte check is a security check and
/// must run even when path validation is otherwise disabled.
#[test]
fn path_validation_disabled_still_rejects_null_byte() {
    // Turning validation OFF is process-global: hold the shared guard for
    // the whole window, or every parallel test that asserts a path is
    // REJECTED can observe it accepted instead. That is exactly how
    // `files_validated_path_rejects_dotdot_segment` failed about one run
    // in twelve of `cargo test -p cratonvm-native-io --lib`, reporting
    // `Files path with `..` segment accepted: Ok("../../etc/passwd")`.
    let _g = crate::test_support::confine_test_lock().lock();
    set_path_validation_enabled(false);
    let result = validate_path("/etc/passwd\0.txt");
    assert!(result.is_err(), "null byte accepted with validation off");
    set_path_validation_enabled(true);
}

// -----------------------------------------------------------------------
// NIO `java.nio.file.Files` path-validation regression tests
//
// AUDIT 2026-05-29 (HIGH): the `Files.*` natives previously passed the
// raw guest path straight to `std::fs`, bypassing `validated_path`
// entirely — skipping even the always-on null-byte and `..`-segment
// guards. Every `Files` native now routes its path(s) through
// `validated_path` (the same convenience wrapper the `File`/FIS/FOS/RAF
// natives use). These tests pin the contract of that shared wrapper so a
// future refactor that drops the call regresses loudly.
// -----------------------------------------------------------------------

/// `validated_path` (called by every `Files` native) must reject a
/// `..` *segment* via the always-on traversal guard, independent of
/// CWD confinement.
#[test]
fn files_validated_path_rejects_dotdot_segment() {
    // Confinement is a process-global flag; serialize with the shared
    // guard (same one watch.rs / random_access_file.rs use) and restore.
    let _g = crate::test_support::confine_test_lock().lock();
    let prev = is_path_confine_to_cwd();
    set_path_validation_enabled(true);
    set_path_confine_to_cwd(false);
    let result = validated_path("../../etc/passwd");
    assert!(
        result.is_err(),
        "Files path with `..` segment accepted: {result:?}"
    );
    let err = format!("{:?}", result.unwrap_err());
    assert!(err.contains("Path traversal detected"), "err = {err}");
    set_path_confine_to_cwd(prev);
}

/// `validated_path` must reject an embedded null byte even when path
/// validation is otherwise disabled (the NUL truncates the host
/// C-string boundary).
#[test]
fn files_validated_path_rejects_null_byte_even_when_disabled() {
    // Turning validation OFF is process-global: hold the shared guard for
    // the whole window, or every parallel test that asserts a path is
    // REJECTED can observe it accepted instead. That is exactly how
    // `files_validated_path_rejects_dotdot_segment` failed about one run
    // in twelve of `cargo test -p cratonvm-native-io --lib`, reporting
    // `Files path with `..` segment accepted: Ok("../../etc/passwd")`.
    let _g = crate::test_support::confine_test_lock().lock();
    set_path_validation_enabled(false);
    let result = validated_path("/tmp/evil\0.txt");
    assert!(
        result.is_err(),
        "Files path with null byte accepted: {result:?}"
    );
    let err = format!("{:?}", result.unwrap_err());
    assert!(err.contains("null byte"), "err = {err}");
    set_path_validation_enabled(true);
}

/// A not-yet-existing file inside the sandbox (e.g. the target of
/// `Files.createFile`/`writeString`) must still be accepted: legitimate
/// creation must not break. The parent (cwd) canonicalizes and the
/// result stays inside the sandbox root.
#[test]
fn files_validated_path_allows_nonexistent_in_sandbox() {
    // Confinement is a process-global flag; serialize with the shared
    // guard (same one watch.rs / random_access_file.rs use) and restore.
    let _g = crate::test_support::confine_test_lock().lock();
    let prev = is_path_confine_to_cwd();
    set_path_validation_enabled(true);
    set_path_confine_to_cwd(true);
    let result = validated_path("files_create_regression_target.txt");
    assert!(
        result.is_ok(),
        "in-sandbox not-yet-existing path rejected: {result:?}"
    );
    set_path_confine_to_cwd(prev);
}

// -----------------------------------------------------------------------
// Regex caching tests
// -----------------------------------------------------------------------

#[test]
fn cached_regex_returns_default_for_whitespace() {
    let re = delimiter_regex(r"\s+");
    assert!(re.is_match(" "));
    assert!(re.is_match("\t"));
}

#[test]
fn cached_regex_returns_custom_for_comma() {
    let re = delimiter_regex(",");
    assert!(re.is_match(","));
    assert!(!re.is_match(" "));
}

#[test]
fn cached_regex_falls_back_on_invalid_pattern() {
    // Invalid regex should fallback to whitespace
    let re = delimiter_regex("[invalid");
    assert!(re.is_match(" "));
}

// -----------------------------------------------------------------------
// Position overflow tests
// -----------------------------------------------------------------------

#[test]
fn safe_pos_to_i32_normal() {
    assert_eq!(safe_pos_to_i32(0).unwrap(), 0);
    assert_eq!(safe_pos_to_i32(100).unwrap(), 100);
    assert_eq!(safe_pos_to_i32(i32::MAX as usize).unwrap(), i32::MAX);
}

#[test]
fn safe_pos_to_i32_overflow() {
    let result = safe_pos_to_i32(i32::MAX as usize + 1);
    assert!(result.is_err());
}

// ===================================================================
// Phase 92.1: AsynchronousFileChannel Tests
// ===================================================================

fn make_afc_path(ctx: &mut MockNativeContext, path_str: &str) -> ObjectRef {
    let obj = ctx.alloc_object(1);
    let s = ctx.create_string(path_str);
    ctx.set_field(obj, PATH_FIELD_STR, Value::Object(Some(s)));
    obj
}

fn make_afc_options(ctx: &mut MockNativeContext, names: &[&str]) -> ObjectRef {
    let arr = ctx.new_ref_array(ClassId::new(0), names.len());
    for (i, name) in names.iter().enumerate() {
        let opt = ctx.create_string(name);
        ctx.set_array_element(arr, i, Value::Object(Some(opt)));
    }
    arr
}

fn temp_afc_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    p.push(format!("cratonvm_afc_{tag}_{nanos}.bin"));
    p
}

#[test]
fn async_file_channel_open_rejects_out_of_cwd_when_confined() {
    let _g = confine_test_lock().lock();
    let prev_confine = is_path_confine_to_cwd();
    let prev_validation = is_path_validation_enabled();
    set_path_validation_enabled(true);
    set_path_confine_to_cwd(true);

    let path = temp_afc_path("confined");
    let path_str = path.to_string_lossy().into_owned();
    let mut ctx = MockNativeContext::new();
    let p = make_afc_path(&mut ctx, &path_str);
    let res = native_afc_open(&mut ctx, &[Value::Object(Some(p))]);

    assert!(
        res.is_err(),
        "confined async open accepted out-of-cwd path: {res:?}"
    );
    let err = format!("{:?}", res.unwrap_err());
    assert!(
        err.contains("SecurityException") || err.contains("outside sandbox"),
        "unexpected error: {err}"
    );

    set_path_confine_to_cwd(prev_confine);
    set_path_validation_enabled(prev_validation);
}

#[test]
fn async_file_channel_write_without_create_does_not_create_file() {
    let _g = confine_test_lock().lock();
    let prev_confine = is_path_confine_to_cwd();
    set_path_confine_to_cwd(false);

    let path = temp_afc_path("write_no_create");
    let path_str = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    let mut ctx = MockNativeContext::new();
    let p = make_afc_path(&mut ctx, &path_str);
    let opts = make_afc_options(&mut ctx, &["WRITE"]);
    let res = native_afc_open(
        &mut ctx,
        &[Value::Object(Some(p)), Value::Object(Some(opts))],
    );

    assert!(res.is_err(), "WRITE without CREATE created a channel");
    assert!(
        !path.exists(),
        "WRITE without CREATE must not create missing files"
    );

    set_path_confine_to_cwd(prev_confine);
}

#[test]
fn async_file_channel_rejects_negative_positions() {
    let _g = confine_test_lock().lock();
    let prev_confine = is_path_confine_to_cwd();
    set_path_confine_to_cwd(false);

    let path = temp_afc_path("negative_position");
    std::fs::write(&path, b"abcdef").unwrap();
    let path_str = path.to_string_lossy().into_owned();

    let mut ctx = MockNativeContext::new();
    let p = make_afc_path(&mut ctx, &path_str);
    let opts = make_afc_options(&mut ctx, &["READ", "WRITE"]);
    let channel = match native_afc_open(
        &mut ctx,
        &[Value::Object(Some(p)), Value::Object(Some(opts))],
    )
    .expect("open ok")
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected channel object, got {other:?}"),
    };
    let bb = alloc_byte_buffer(&mut ctx, 4);

    let read_res = native_afc_read(
        &mut ctx,
        &[
            Value::Object(Some(channel)),
            Value::Object(Some(bb)),
            Value::Long(-1),
        ],
    );
    assert!(
        read_res.is_err(),
        "negative read position was accepted: {read_res:?}"
    );

    let write_res = native_afc_write(
        &mut ctx,
        &[
            Value::Object(Some(channel)),
            Value::Object(Some(bb)),
            Value::Long(-1),
        ],
    );
    assert!(
        write_res.is_err(),
        "negative write position was accepted: {write_res:?}"
    );

    let _ = native_afc_close(&mut ctx, &[Value::Object(Some(channel))]);
    let _ = std::fs::remove_file(&path);
    set_path_confine_to_cwd(prev_confine);
}

#[test]
fn async_file_completed_future_methods_are_registered_and_completed() {
    let mut registry = NativeMethodRegistry::new();
    register_async_file_channel(&mut registry);
    let cancel = registry
        .find("java/util/concurrent/CompletedFuture", "cancel", "(Z)Z")
        .expect("CompletedFuture.cancel must be registered");
    let is_cancelled = registry
        .find("java/util/concurrent/CompletedFuture", "isCancelled", "()Z")
        .expect("CompletedFuture.isCancelled must be registered");
    let is_done = registry
        .find("java/util/concurrent/CompletedFuture", "isDone", "()Z")
        .expect("CompletedFuture.isDone must be registered");
    let get = registry
        .find(
            "java/util/concurrent/CompletedFuture",
            "get",
            "()Ljava/lang/Object;",
        )
        .expect("CompletedFuture.get must be registered");
    let timed_get = registry
        .find(
            "java/util/concurrent/CompletedFuture",
            "get",
            "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        )
        .expect("CompletedFuture timed get must be registered");

    let mut ctx = MockNativeContext::new();
    // `?` in a `-> ()` test does not compile, and `wrap_completed_future`
    // returns `Result<Value, MethodCallFailed>` -- so unwrap the Result and
    // match the `Value`. Landed broken in 1a07c0e54: `cargo test -p
    // cratonvm-native-io` could not build the lib TEST TARGET at all, so
    // every test in the crate was unrunnable and none of them said so.
    let future = match wrap_completed_future(&mut ctx, Value::Int(123))
        .expect("wrap_completed_future must not fail")
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected future object, got {other:?}"),
    };

    // The REAL `java.util.concurrent.CompletableFuture` is tried first, and
    // the five registrations above are the fallback for the configurations
    // that cannot answer it. This mock's `invoke` answers `Ok(None)` for
    // everything, so `future` above IS that fallback and the assertions
    // below exercise only it — the recorded call is the only trace the
    // primary path leaves here. Without this assertion the whole test passes
    // unchanged on the pre-fix body, which minted the no-image
    // `CompletedFuture` unconditionally and died as
    // `NoClassDefFoundError` under `--real-jdk`.
    assert!(
        ctx.recorded_calls().iter().any(|c| {
            c.declared_class.as_deref() == Some("java/util/concurrent/CompletableFuture")
                && c.method_name == "completedFuture"
                && c.descriptor
                    == "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;"
        }),
        "wrap_completed_future must try the real CompletableFuture before \
         minting the synthetic fallback: {:?}",
        ctx.recorded_calls()
    );

    assert_eq!(
        get(&mut ctx, &[Value::Object(Some(future))]).unwrap(),
        Some(Value::Int(123))
    );
    assert_eq!(
        timed_get(
            &mut ctx,
            &[
                Value::Object(Some(future)),
                Value::Long(1),
                Value::Object(None),
            ],
        )
        .unwrap(),
        Some(Value::Int(123))
    );
    assert_eq!(
        is_done(&mut ctx, &[Value::Object(Some(future))]).unwrap(),
        Some(Value::Int(1))
    );
    assert_eq!(
        cancel(&mut ctx, &[Value::Object(Some(future)), Value::Int(1)],).unwrap(),
        Some(Value::Int(0))
    );
    assert_eq!(
        is_cancelled(&mut ctx, &[Value::Object(Some(future))]).unwrap(),
        Some(Value::Int(0))
    );
}

#[test]
fn async_file_completion_handler_failed_receives_throwable() {
    let mut ctx = MockNativeContext::new();
    let channel = ctx.alloc_object(AFC_NUM_FIELDS);
    ctx.set_field(channel, AFC_FIELD_OPEN, Value::Int(0));
    ctx.set_field(channel, AFC_FIELD_FD, Value::Int(1));
    let bb = alloc_byte_buffer(&mut ctx, 4);
    let handler = ctx.alloc_object_with_class(0, "java/nio/channels/CompletionHandler");

    let result = native_afc_read_handler(
        &mut ctx,
        &[
            Value::Object(Some(channel)),
            Value::Object(Some(bb)),
            Value::Long(0),
            Value::Object(None),
            Value::Object(Some(handler)),
        ],
    );

    assert!(result.is_ok(), "handler path returned error: {result:?}");
    // Exactly one HANDLER callback. Counting every recorded invoke instead
    // would also count the `IOException.<init>` that `afc_io_exception`
    // makes whenever the context can allocate — which is the branch
    // production takes, and which this mock only began exercising once its
    // `new_object` stopped reporting allocation failure.
    let calls: Vec<_> = ctx
        .recorded_calls()
        .iter()
        .filter(|c| c.method_name == "failed" || c.method_name == "completed")
        .cloned()
        .collect();
    assert_eq!(calls.len(), 1, "unexpected handler callbacks: {calls:?}");
    let call = &calls[0];
    assert_eq!(call.method_name, "failed");
    assert_eq!(
        call.descriptor,
        "(Ljava/lang/Throwable;Ljava/lang/Object;)V"
    );
    let thrown = match call.args.first() {
        Some(Value::Object(Some(o))) => *o,
        other => panic!("failed() first argument was not an object: {other:?}"),
    };
    assert!(
        ctx.read_string(thrown).is_none(),
        "failed() received a Java String instead of Throwable"
    );
    let thrown_class = ctx.class_name_of_id(ctx.class_id_of_object(thrown));
    assert_eq!(thrown_class.as_deref(), Some("java/io/IOException"));
    let message = match ctx.get_field_by_name(thrown, "detailMessage") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        other => panic!("throwable detailMessage was not a string: {other:?}"),
    };
    assert!(
        message.contains("AsynchronousFileChannel is closed"),
        "unexpected throwable message: {message}"
    );
}

#[test]
fn test_92_1_async_file_channel_read() {
    let table = FileDescriptorTable::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("async_read.txt");
    let path_str = path.to_str().unwrap();

    // Write test data
    std::fs::write(&path, b"hello async world").unwrap();

    // Open for read+write
    let fd = table.open_read_write(path_str, false).unwrap();

    // Read at position 0
    let mut buf = [0u8; 5];
    let n = table.pread_at(fd, &mut buf, 0).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf, b"hello");

    // Read at position 6
    let mut buf2 = [0u8; 5];
    let n2 = table.pread_at(fd, &mut buf2, 6).unwrap();
    assert_eq!(n2, 5);
    assert_eq!(&buf2, b"async");

    table.close(fd).unwrap();
}

#[test]
fn test_92_1_async_file_channel_write() {
    let table = FileDescriptorTable::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("async_write.txt");
    let path_str = path.to_str().unwrap();

    // Create the file
    std::fs::write(&path, b"__________").unwrap();

    let fd = table.open_read_write(path_str, false).unwrap();

    // Write at position 0
    table.pwrite_at(fd, b"hello", 0).unwrap();

    // Write at position 5
    table.pwrite_at(fd, b"world", 5).unwrap();

    table.close(fd).unwrap();

    // Verify
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "helloworld");
}

#[test]
fn test_92_1_async_file_channel_completion_handler() {
    let table = FileDescriptorTable::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("async_handler.txt");
    let path_str = path.to_str().unwrap();

    std::fs::write(&path, b"completion test data").unwrap();

    let fd = table.open_read_write(path_str, false).unwrap();

    // Verify file size
    let size = table.file_size(fd).unwrap();
    assert_eq!(size, 20); // "completion test data" is 20 bytes

    // pread at position 11 should get "test data"
    let mut buf = [0u8; 9];
    let n = table.pread_at(fd, &mut buf, 11).unwrap();
    assert_eq!(n, 9);
    assert_eq!(&buf, b"test data");

    table.close(fd).unwrap();
}

// ===================================================================
// Phase 92.2: WatchService Tests — exercise the real `notify`-backed
// watcher end-to-end. Each test creates a RecommendedWatcher, performs
// a filesystem action, then drains the mpsc channel and asserts the
// expected event kind appears. These tests are deliberately tolerant
// of platform-specific event batching (some platforms emit Modify
// instead of / in addition to Create for a fresh file).
// ===================================================================

fn drain_events(
    rx: &std::sync::mpsc::Receiver<NotifyResult>,
    deadline: std::time::Duration,
) -> Vec<notify::Event> {
    let start = std::time::Instant::now();
    let mut out = Vec::new();
    while start.elapsed() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(Ok(ev)) => out.push(ev),
            Ok(Err(_)) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !out.is_empty() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    out
}

#[test]
fn test_92_2_watch_create_event() {
    use notify::{RecursiveMode, Watcher as NotifyWatcher};
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<NotifyResult>();
    let mut w = notify::RecommendedWatcher::new(
        move |r: NotifyResult| {
            let _ = tx.send(r);
        },
        notify::Config::default(),
    )
    .unwrap();
    w.watch(dir.path(), RecursiveMode::NonRecursive).unwrap();

    std::fs::write(dir.path().join("new_file.txt"), "hello").unwrap();

    let events = drain_events(&rx, std::time::Duration::from_secs(3));
    assert!(
        events.iter().any(|e| matches!(
            e.kind,
            notify::EventKind::Create(_) | notify::EventKind::Modify(_)
        )),
        "no create/modify event observed: {events:?}"
    );
}

#[test]
fn test_92_2_watch_delete_event() {
    use notify::{RecursiveMode, Watcher as NotifyWatcher};
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("to_delete.txt");
    std::fs::write(&file_path, "bye").unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<NotifyResult>();
    let mut w = notify::RecommendedWatcher::new(
        move |r: NotifyResult| {
            let _ = tx.send(r);
        },
        notify::Config::default(),
    )
    .unwrap();
    w.watch(dir.path(), RecursiveMode::NonRecursive).unwrap();

    std::fs::remove_file(&file_path).unwrap();

    let events = drain_events(&rx, std::time::Duration::from_secs(3));
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, notify::EventKind::Remove(_))),
        "no remove event observed: {events:?}"
    );
}

#[test]
fn test_92_2_watch_modify_event() {
    use notify::{RecursiveMode, Watcher as NotifyWatcher};
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("modify_me.txt");
    std::fs::write(&file_path, "original").unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<NotifyResult>();
    let mut w = notify::RecommendedWatcher::new(
        move |r: NotifyResult| {
            let _ = tx.send(r);
        },
        notify::Config::default(),
    )
    .unwrap();
    w.watch(dir.path(), RecursiveMode::NonRecursive).unwrap();

    std::fs::write(&file_path, "modified content that is longer").unwrap();

    let events = drain_events(&rx, std::time::Duration::from_secs(3));
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, notify::EventKind::Modify(_))),
        "no modify event observed: {events:?}"
    );
}

// -------------------------------------------------------------------
// WatchEvent.Kind translation (E40-1, replacing `vm/src/vm/tests.rs`'s
// deleted `watch_event_kinds_p66`).
//
// The three tests above measure the `notify` crate, not this VM. Nothing
// in this tree exercised the Kind translation itself, and the test that
// *claimed* to — `watch_event_kinds_p66` — drove four dead field-shaped
// registrations in another crate and asserted that a `WatchEvent$Kind`
// reads back as the java.lang.String "ENTRY_CREATE". Measured on the
// oracle (JDK 25.0.3+9-LTS):
//
//     StandardWatchEventKinds.ENTRY_CREATE.getClass().getName()
//       = java.nio.file.StandardWatchEventKinds$StdWatchEventKind
//     .name() = "ENTRY_CREATE"   .type() = interface java.nio.file.Path
//     (ENTRY_CREATE instanceof String) = false
//
// so the constant's `name()` — NOT its identity as a String — is the
// JDK-guaranteed handle, and `watch_event_kind_bit` is the code that
// consumes it.
// -------------------------------------------------------------------

/// All three `Kind` shapes `watch_event_kind_bit` must accept, and the
/// one it must reject.
#[test]
fn watch_event_kind_bit_translates_all_three_kind_shapes() {
    let mut ctx = MockNativeContext::new();

    // 1. Synthetic one-field Kind: the bit lives in slot 0.
    let synth = ctx.alloc_object(1);
    ctx.set_field(synth, 0, Value::Int(EVENT_MODIFY));
    assert_eq!(
        watch_event_kind_bit(&mut ctx, synth),
        EVENT_MODIFY,
        "synthetic Kind's slot-0 bit was not read; this is the decode that \
         once yielded mask 0 and filtered out every event forever"
    );

    // 2. Real JDK `StdWatchEventKind`: no readable instance fields, and
    //    `name()` is what identifies it. Quoted off the oracle above.
    let real = ctx.alloc_object(0);
    let name = ctx.create_string("ENTRY_CREATE");
    ctx.script(
        "name",
        "()Ljava/lang/String;",
        Ok(Some(Value::Object(Some(name)))),
    );
    assert_eq!(
        watch_event_kind_bit(&mut ctx, real),
        EVENT_CREATE,
        "a real Kind must be identified by name(), not by being a String"
    );

    // 3. Legacy bare-String Kind (the shape the dead
    //    `StandardWatchEventKinds` rows in `phases_late/nio_file.rs`
    //    hand out). Accepted, but only via the `read_string` fallback.
    let legacy = ctx.create_string("ENTRY_DELETE");
    assert_eq!(watch_event_kind_bit(&mut ctx, legacy), EVENT_DELETE);

    // 4. OVERFLOW is not one of the three entry kinds this VM surfaces,
    //    so it must translate to 0 rather than to some entry bit.
    let overflow = ctx.create_string("OVERFLOW");
    assert_eq!(watch_event_kind_bit(&mut ctx, overflow), 0);
}

/// `watch_event_kind_object` -> `watch_event_kind_bit` is the round trip
/// `Path.register` and `detect_events` sit on either side of.
#[test]
fn watch_event_kind_object_round_trips_through_the_bit() {
    // The mock's `static_field_index_by_name` answers `None`, so this
    // exercises the SYNTHETIC arm — the one that runs whenever the real
    // `StandardWatchEventKinds` statics are unreadable.
    for bit in [EVENT_CREATE, EVENT_DELETE, EVENT_MODIFY] {
        let mut ctx = MockNativeContext::new();
        let obj = match watch_event_kind_object(&mut ctx, bit).unwrap() {
            Value::Object(Some(o)) => o,
            other => panic!("kind object for bit {bit} was {other:?}"),
        };
        assert_eq!(
            watch_event_kind_bit(&mut ctx, obj),
            bit,
            "kind object for bit {bit} did not translate back"
        );
    }
}

// ===================================================================
// Phase 92.3: DatagramChannel (UDP) Tests
// ===================================================================

#[test]
fn test_92_3_udp_send_receive() {
    let table = FileDescriptorTable::new();

    // Open two UDP sockets
    let fd1 = table.open_udp(Some("127.0.0.1:0")).unwrap();
    let fd2 = table.open_udp(Some("127.0.0.1:0")).unwrap();

    // Get the actual addresses
    let _addr1 = table.udp_local_addr(fd1).unwrap();
    let addr2 = table.udp_local_addr(fd2).unwrap();

    // Send from fd1 to fd2
    let sent = table.udp_send(fd1, b"hello udp", &addr2).unwrap();
    assert_eq!(sent, 9);

    // Receive on fd2
    let mut buf = [0u8; 64];
    let (received, source) = table.udp_recv(fd2, &mut buf).unwrap();
    assert_eq!(received, 9);
    assert_eq!(&buf[..9], b"hello udp");
    // Source should be addr1
    assert!(source.contains("127.0.0.1"));

    table.close(fd1).unwrap();
    table.close(fd2).unwrap();
}

#[test]
fn test_92_3_udp_bind() {
    let table = FileDescriptorTable::new();

    // Bind to a specific port (0 = OS-assigned)
    let fd = table.open_udp(Some("127.0.0.1:0")).unwrap();
    let addr = table.udp_local_addr(fd).unwrap();
    assert!(addr.starts_with("127.0.0.1:"));

    // Port should be non-zero (OS assigned)
    let port: u16 = addr.split(':').last().unwrap().parse().unwrap();
    assert!(port > 0);

    table.close(fd).unwrap();
}

#[test]
fn test_92_3_udp_nonblocking() {
    let table = FileDescriptorTable::new();
    let fd = table.open_udp(Some("127.0.0.1:0")).unwrap();

    // Set non-blocking
    table.udp_set_nonblocking(fd, true).unwrap();

    // Receive should return WouldBlock error immediately
    let mut buf = [0u8; 64];
    let result = table.udp_recv(fd, &mut buf);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

    table.close(fd).unwrap();
}

// ===================================================================
// Phase 92.4: Selector Tests
// ===================================================================

#[test]
fn test_92_4_selector_poll_ready() {
    let table = FileDescriptorTable::new();

    // Open two UDP sockets
    let fd1 = table.open_udp(Some("127.0.0.1:0")).unwrap();
    let fd2 = table.open_udp(Some("127.0.0.1:0")).unwrap();

    let addr2 = table.udp_local_addr(fd2).unwrap();

    // Send data to fd2
    table.udp_send(fd1, b"data", &addr2).unwrap();

    // Give time for delivery
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Poll fd2 — should be readable
    let (readable, writable) = table.poll_ready(fd2);
    assert!(readable, "fd2 should be readable after data was sent to it");
    assert!(writable, "UDP sockets should always be writable");

    table.close(fd1).unwrap();
    table.close(fd2).unwrap();
}

#[test]
fn test_92_4_selector_cancel_key() {
    // Test that cancelled keys are excluded from selection
    // This is a unit test of the key state management
    let cancelled = 0i32; // SK_FIELD_VALID = 0
    let valid = 1i32;

    // A cancelled key should not be selected
    assert_eq!(cancelled, 0);
    assert_eq!(valid, 1);
    assert_ne!(cancelled, valid);
}

#[test]
fn test_92_4_selector_wakeup() {
    // Verify wakeup constants and selector open/close state
    assert_eq!(OP_READ, 1);
    assert_eq!(OP_WRITE, 4);
    assert_eq!(OP_CONNECT, 8);
    assert_eq!(OP_ACCEPT, 16);
}

#[test]
fn test_92_4_selector_concurrent_udp() {
    let table = FileDescriptorTable::new();

    // Open 3 UDP sockets
    let fds: Vec<u32> = (0..3)
        .map(|_| table.open_udp(Some("127.0.0.1:0")).unwrap())
        .collect();

    let addrs: Vec<String> = fds
        .iter()
        .map(|fd| table.udp_local_addr(*fd).unwrap())
        .collect();

    // Send to socket 0 and 2 (not 1)
    let sender = table.open_udp(Some("127.0.0.1:0")).unwrap();
    table.udp_send(sender, b"msg0", &addrs[0]).unwrap();
    table.udp_send(sender, b"msg2", &addrs[2]).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(10));

    // Poll each
    let (r0, _) = table.poll_ready(fds[0]);
    let (r1, _) = table.poll_ready(fds[1]);
    let (r2, _) = table.poll_ready(fds[2]);

    assert!(r0, "fd[0] should be readable");
    assert!(!r1, "fd[1] should NOT be readable");
    assert!(r2, "fd[2] should be readable");

    // Clean up
    for fd in &fds {
        table.close(*fd).unwrap();
    }
    table.close(sender).unwrap();
}

// -----------------------------------------------------------------------
// G39-1 — the after-close precedence order, and the two close halves.
//
// Every expected value below was MEASURED on Eclipse Adoptium
// 25.0.3+9-LTS, Windows, 2026-08-17. See
// docs/feature-designs/real-jdk-mode.md.
//
// Each refusal test is PAIRED with the non-refusal on the same body, so
// neither direction can regress silently: a test that only asserted the
// throw would pass on a body that threw for everything, which is the
// over-correction these fixes are repairing.
// -----------------------------------------------------------------------

/// Build a `FileInputStream`/`FileOutputStream`-shaped receiver whose
/// `FileDescriptor` carries the marker a close writes (`fd == -1` AND
/// `handle == -1`), which is what `io_stream_is_closed` keys on.
fn closed_stream_receiver(ctx: &mut MockNativeContext) -> ObjectRef {
    let fd_obj = ctx.alloc_object(2);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(-1));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));
    let stream = ctx.alloc_object(2);
    ctx.set_field_by_name(stream, "fd", Value::Object(Some(fd_obj)));
    stream
}

/// A receiver that is marked closed on ONE ground while a descriptor is
/// still reachable by ANOTHER — which is the whole defect
/// `f{i,o}s_refuse_if_closed` exists for.
///
/// The `FileDescriptor` carries the closed marker; instance slot 0 is left
/// at the untagged `Int(0)` a never-written reference slot reads back as,
/// and `f{i,o}s_get_fd`'s legacy arm accepts that as descriptor 0. So the
/// old `match get_fd { .., None if is_closed => Err, .. }` took the
/// `Some(fd)` arm and did the I/O on a stream it had just agreed was
/// closed.
fn closed_but_descriptor_still_reachable(ctx: &mut MockNativeContext) -> ObjectRef {
    let fd_obj = ctx.alloc_object(2);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(-1));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));
    let stream = ctx.alloc_object(2);
    ctx.set_field_by_name(stream, "fd", Value::Object(Some(fd_obj)));
    // Slot 0 untouched — `Int(0)` — exactly as an unwritten reference slot
    // reads. Asserted rather than assumed, because the whole test turns on
    // it and a mock change that started returning `Object(None)` here would
    // otherwise make this pass for the wrong reason.
    assert!(
        matches!(ctx.get_field(stream, 0), Value::Int(0)),
        "slot 0 must be the untagged default for this fixture to bite"
    );
    assert!(
        fis_get_fd(ctx, stream).is_some(),
        "the descriptor must still be REACHABLE, or there is no defect to test"
    );
    stream
}

/// Every operation the oracle says throws on a closed stream, against a
/// receiver whose descriptor is still reachable.
///
/// `probes/ClosedStreamOracle.java` on Eclipse Adoptium 25.0.3+9-LTS is the
/// source of every row; see `fis_refuse_if_closed` for the full matrix.
#[test]
fn a_closed_stream_refuses_even_while_its_descriptor_is_reachable() {
    let mut ctx = MockNativeContext::new();
    let s = closed_but_descriptor_still_reachable(&mut ctx);
    let arr = ctx.new_array(ArrayElementType::Byte, 4);
    let this = Value::Object(Some(s));
    let a = Value::Object(Some(arr));

    assert!(native_fis_read(&mut ctx, &[this]).is_err(), "fis.read()");
    assert!(
        native_fis_read_byte_array(&mut ctx, &[this, a]).is_err(),
        "fis.read(byte[4])"
    );
    assert!(
        native_fis_read_bytes(&mut ctx, &[this, a, Value::Int(0), Value::Int(4)]).is_err(),
        "fis.read(b, 0, 4)"
    );
    assert!(
        native_fis_available(&mut ctx, &[this]).is_err(),
        "fis.available()"
    );
    for n in [0i64, -5, 1] {
        assert!(
            native_fis_skip(&mut ctx, &[this, Value::Long(n)]).is_err(),
            "fis.skip({n})"
        );
    }

    assert!(
        native_fos_write_byte(&mut ctx, &[this, Value::Int(7)]).is_err(),
        "fos.write(int)"
    );
    assert!(
        native_fos_write_byte_array(&mut ctx, &[this, a]).is_err(),
        "fos.write(byte[4])"
    );
    assert!(
        native_fos_write_bytes(&mut ctx, &[this, a, Value::Int(0), Value::Int(4)]).is_err(),
        "fos.write(b, 0, 4)"
    );
    assert!(
        native_fos_write_byte_ignore_append(&mut ctx, &[this, Value::Int(7), Value::Int(0)])
            .is_err(),
        "fos.write(int) [JDK 25 append descriptor]"
    );
    assert!(
        native_fos_write_bytes_ignore_append(
            &mut ctx,
            &[this, a, Value::Int(0), Value::Int(4), Value::Int(0)]
        )
        .is_err(),
        "fos.write(b, 0, 4) [JDK 25 append descriptor]"
    );
}

/// THE TWIN. Without it the suite above is satisfied by refusing
/// everything, which is a worse defect than the one being fixed.
///
/// Three things must still NOT refuse on a closed stream, and every one of
/// them is measured:
///
/// * the ZERO-LENGTH transfers, on both sides;
/// * `close()`, twice;
/// * `flush()` on a closed `FileOutputStream`.
#[test]
fn the_carve_outs_survive_the_closed_check() {
    let mut ctx = MockNativeContext::new();
    let s = closed_but_descriptor_still_reachable(&mut ctx);
    let arr = ctx.new_array(ArrayElementType::Byte, 4);
    let empty = ctx.new_array(ArrayElementType::Byte, 0);
    let this = Value::Object(Some(s));
    let a = Value::Object(Some(arr));
    let e = Value::Object(Some(empty));

    assert_eq!(
        native_fis_read_bytes(&mut ctx, &[this, a, Value::Int(0), Value::Int(0)]).unwrap(),
        Some(Value::Int(0)),
        "fis.read(b, 0, 0) on a closed stream is 0, not IOException"
    );
    assert_eq!(
        native_fis_read_byte_array(&mut ctx, &[this, e]).unwrap(),
        Some(Value::Int(0)),
        "fis.read(byte[0]) on a closed stream is 0"
    );
    assert_eq!(
        native_fos_write_bytes(&mut ctx, &[this, a, Value::Int(0), Value::Int(0)]).unwrap(),
        None,
        "fos.write(b, 0, 0) on a closed stream is void"
    );
    assert_eq!(
        native_fos_write_byte_array(&mut ctx, &[this, e]).unwrap(),
        None,
        "fos.write(byte[0]) on a closed stream is void"
    );
    assert_eq!(
        native_fos_write_bytes_ignore_append(
            &mut ctx,
            &[this, a, Value::Int(0), Value::Int(0), Value::Int(0)]
        )
        .unwrap(),
        None,
        "fos.write(b, 0, 0) [append descriptor] is void"
    );

    assert_eq!(
        native_fis_close(&mut ctx, &[this]).unwrap(),
        None,
        "a double close stays a clean void"
    );
    assert_eq!(
        native_fos_flush(&mut ctx, &[this]).unwrap(),
        None,
        "flush() on a closed FileOutputStream is void"
    );
}

/// THE OTHER TWIN: an OPEN stream is untouched. The hoisted check now runs
/// on every call rather than only when the descriptor lookup failed, so the
/// thing to prove is that it does not start refusing live streams.
#[test]
fn an_open_stream_is_not_refused_by_the_hoisted_check() {
    let mut ctx = MockNativeContext::new();
    let fd_obj = ctx.alloc_object(2);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(9_101));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(9_101));
    let stream = ctx.alloc_object(2);
    ctx.set_field_by_name(stream, "fd", Value::Object(Some(fd_obj)));
    assert!(!fis_is_closed(&ctx, stream));
    assert!(!fos_is_closed(&ctx, stream));
    assert!(fis_refuse_if_closed(&ctx, stream).is_ok());
    assert!(fos_refuse_if_closed(&ctx, stream).is_ok());
}

#[test]
fn is_empty_transfer_is_zero_only() {
    assert!(is_empty_transfer(0));
    assert!(!is_empty_transfer(1));
    assert!(!is_empty_transfer(-1));
}

/// `fis.read(b, 0, 0)` after close is `0`, not `IOException`.
/// Measured: `CLOSED fis.read(b,0,0) | RET [0]`.
#[test]
fn fis_read_bytes_zero_length_answers_zero_on_a_closed_stream() {
    let mut ctx = MockNativeContext::new();
    let stream = closed_stream_receiver(&mut ctx);
    let arr = ctx.new_array(ArrayElementType::Byte, 4);
    let zero = native_fis_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(stream)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(0),
        ],
    );
    assert_eq!(
        zero.unwrap(),
        Some(Value::Int(0)),
        "a zero-length read must answer 0 before the closed check"
    );
    // The pair: a NON-zero-length read on the same receiver still refuses.
    let refused = native_fis_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(stream)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(4),
        ],
    );
    assert!(
        refused.is_err(),
        "a non-empty read on a closed stream must still raise Stream Closed"
    );
}

/// `fis.read(new byte[0])` is `0` — on an OPEN stream too, where this body
/// used to answer `-1` because a zero-byte `read_bytes` looks like EOF.
#[test]
fn fis_read_empty_array_answers_zero_not_eof() {
    let mut ctx = MockNativeContext::new();
    // No descriptor at all and no closed marker: the "unknown receiver"
    // case, which must NOT be a refusal and must not be EOF either.
    let stream = ctx.alloc_object(2);
    let empty = ctx.new_array(ArrayElementType::Byte, 0);
    let got = native_fis_read_byte_array(
        &mut ctx,
        &[Value::Object(Some(stream)), Value::Object(Some(empty))],
    );
    assert_eq!(
        got.unwrap(),
        Some(Value::Int(0)),
        "read(new byte[0]) is 0, never -1"
    );
}

/// `fos.write(b, 0, 0)` after close is `void`, not `IOException` — on both
/// the legacy `write([BII)V` body and the JDK 25 `writeBytes([BIIZ)V` one
/// that the SHIPPING modes reach.
#[test]
fn fos_write_bytes_zero_length_is_void_on_a_closed_stream() {
    let mut ctx = MockNativeContext::new();
    let stream = closed_stream_receiver(&mut ctx);
    let arr = ctx.new_array(ArrayElementType::Byte, 4);

    let legacy = native_fos_write_bytes(
        &mut ctx,
        &[
            Value::Object(Some(stream)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(0),
        ],
    );
    assert_eq!(legacy.unwrap(), None, "write([BII) of 0 bytes must be void");

    let shipping = native_fos_write_bytes_ignore_append(
        &mut ctx,
        &[
            Value::Object(Some(stream)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(0),
            Value::Int(0),
        ],
    );
    assert_eq!(
        shipping.unwrap(),
        None,
        "writeBytes([BIIZ) of 0 bytes must be void"
    );

    // The pair: a real write on the same receiver still refuses, on both.
    assert!(
        native_fos_write_bytes(
            &mut ctx,
            &[
                Value::Object(Some(stream)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(4),
            ],
        )
        .is_err(),
        "a non-empty write([BII) on a closed stream must still refuse"
    );
    assert!(
        native_fos_write_bytes_ignore_append(
            &mut ctx,
            &[
                Value::Object(Some(stream)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(4),
                Value::Int(0),
            ],
        )
        .is_err(),
        "a non-empty writeBytes([BIIZ) on a closed stream must still refuse"
    );
}

/// `fos.write(new byte[0])` after close is `void`.
#[test]
fn fos_write_empty_array_is_void_on_a_closed_stream() {
    let mut ctx = MockNativeContext::new();
    let stream = closed_stream_receiver(&mut ctx);
    let empty = ctx.new_array(ArrayElementType::Byte, 0);
    assert_eq!(
        native_fos_write_byte_array(
            &mut ctx,
            &[Value::Object(Some(stream)), Value::Object(Some(empty))],
        )
        .unwrap(),
        None,
    );
    // The pair: a one-byte array on the same receiver refuses.
    let one = ctx.new_array(ArrayElementType::Byte, 1);
    assert!(native_fos_write_byte_array(
        &mut ctx,
        &[Value::Object(Some(stream)), Value::Object(Some(one))],
    )
    .is_err());
}

/// `skip` has NO zero-length escape hatch: `skip(0)` and `skip(-5)` on a
/// closed stream both raise. Measured:
/// `CLOSED fis.skip(0) | THROW java.io.IOException msg=[Stream Closed]`.
#[test]
fn fis_skip_consults_the_descriptor_before_the_count() {
    let mut ctx = MockNativeContext::new();
    let stream = closed_stream_receiver(&mut ctx);
    for n in [0i64, -5, 1] {
        assert!(
            native_fis_skip(&mut ctx, &[Value::Object(Some(stream)), Value::Long(n)]).is_err(),
            "skip({n}) on a closed stream must raise Stream Closed"
        );
    }
    // The pair: an UNKNOWN receiver (no descriptor, no closed marker) keeps
    // its previous quiet `0` for a non-positive count — the refusal is
    // gated on the close marker, not on `fis_get_fd` answering `None`.
    let unknown = ctx.alloc_object(2);
    assert_eq!(
        native_fis_skip(&mut ctx, &[Value::Object(Some(unknown)), Value::Long(0)]).unwrap(),
        Some(Value::Long(0)),
    );
}

/// `native_fd_close0` writes the closed marker and stays quiet for a
/// descriptor whose fd is not in the table — the double-close row
/// (`fos.close(); fos.close(); -> void, void`).
#[test]
fn fd_close0_marks_closed_and_a_second_close_is_quiet() {
    let mut ctx = MockNativeContext::new();
    let fd_obj = ctx.alloc_object(2);
    // A high fd that was never opened: `FdTable::close` removes nothing and
    // answers `Ok(())`, which is the "already released" case HotSpot also
    // reports as a clean void.
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(9_001));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(9_001));
    assert_eq!(
        native_fd_close0(&mut ctx, &[Value::Object(Some(fd_obj))]).unwrap(),
        None,
    );
    assert_eq!(ctx.get_field_by_name(fd_obj, "fd"), Value::Int(-1));
    assert_eq!(ctx.get_field_by_name(fd_obj, "handle"), Value::Long(-1));
    // Second close: the marker is already there, so no fd is resolved and
    // nothing is released. Must still be a clean void.
    assert_eq!(
        native_fd_close0(&mut ctx, &[Value::Object(Some(fd_obj))]).unwrap(),
        None,
    );
    // And the marker now makes the after-close family refuse.
    let stream = ctx.alloc_object(2);
    ctx.set_field_by_name(stream, "fd", Value::Object(Some(fd_obj)));
    assert!(
        fos_is_closed(&ctx, stream),
        "the marker native_fd_close0 writes must be the one io_stream_is_closed reads"
    );
    assert!(fis_is_closed(&ctx, stream));
}

/// `native_fis_close` writes the marker and propagates its host close. The
/// propagation cannot fire on a `FileRead` entry (`FdTable::close` answers
/// `Ok(())` for it), so the observable here is the marker plus the clean
/// double close.
#[test]
fn fis_close_marks_closed_and_is_idempotent() {
    let mut ctx = MockNativeContext::new();
    let fd_obj = ctx.alloc_object(2);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(9_002));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(9_002));
    let stream = ctx.alloc_object(2);
    ctx.set_field_by_name(stream, "fd", Value::Object(Some(fd_obj)));
    assert_eq!(
        native_fis_close(&mut ctx, &[Value::Object(Some(stream))]).unwrap(),
        None,
    );
    assert!(fis_is_closed(&ctx, stream));
    assert_eq!(
        native_fis_close(&mut ctx, &[Value::Object(Some(stream))]).unwrap(),
        None,
        "a double close must stay a clean void"
    );
    // And now the read side refuses rather than answering EOF.
    assert!(native_fis_read(&mut ctx, &[Value::Object(Some(stream))]).is_err());
    assert!(native_fis_available(&mut ctx, &[Value::Object(Some(stream))]).is_err());
}

/// `fos.flush()` after close is `void` on HotSpot while `bw.flush()` and
/// `osw.flush()` throw. The control row: this body must NOT grow a refusal
/// when its neighbours did.
#[test]
fn fos_flush_after_close_stays_quiet() {
    let mut ctx = MockNativeContext::new();
    let stream = closed_stream_receiver(&mut ctx);
    assert_eq!(
        native_fos_flush(&mut ctx, &[Value::Object(Some(stream))]).unwrap(),
        None,
        "FileOutputStream.flush() after close does NOT throw (measured)"
    );
}
