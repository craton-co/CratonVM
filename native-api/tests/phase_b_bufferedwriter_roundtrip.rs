// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase B (RB.8) round-trip test for `Files.newBufferedWriter` /
//! `Files.newBufferedReader` at the fd-table level.  The native-built-
//! ins implementation of `newBufferedWriter` opens a file via
//! `FileDescriptorTable::open_write`, and `newBufferedReader` reads
//! back via `std::fs::read_to_string`.  This test exercises the
//! underlying I/O contract: bytes written via the fd round-trip
//! through the file system identically.

use cratonvm_native_api::fd_table::FileDescriptorTable;

fn tmp_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "cratonvm_phase_b_{}_{}.txt",
        name,
        std::process::id()
    ));
    p
}

#[test]
fn rb8_utf8_write_then_read_roundtrip() {
    let path = tmp_path("rb8_utf8");
    let path_str = path.to_string_lossy().to_string();

    let table = FileDescriptorTable::new();
    let fd = table.open_write(&path_str, false).expect("open_write");
    // Write UTF-8 text including multi-byte sequences.
    table
        .write_string(fd, "Hello \u{00E9} \u{4E2D} \u{1F600}!")
        .expect("write");
    table.close(fd).expect("close");

    let contents = std::fs::read_to_string(&path_str).expect("read back");
    assert_eq!(contents, "Hello \u{00E9} \u{4E2D} \u{1F600}!");
    let _ = std::fs::remove_file(&path_str);
}

#[test]
fn rb8_append_mode_preserves_prior_contents() {
    let path = tmp_path("rb8_append");
    let path_str = path.to_string_lossy().to_string();

    let table = FileDescriptorTable::new();
    let fd1 = table.open_write(&path_str, false).unwrap();
    table.write_string(fd1, "first line\n").unwrap();
    table.close(fd1).unwrap();

    let fd2 = table.open_write(&path_str, true).unwrap(); // append
    table.write_string(fd2, "second line\n").unwrap();
    table.close(fd2).unwrap();

    let contents = std::fs::read_to_string(&path_str).unwrap();
    assert_eq!(contents, "first line\nsecond line\n");
    let _ = std::fs::remove_file(&path_str);
}

#[test]
fn rb8_readline_crlf_handling() {
    let path = tmp_path("rb8_readline");
    let path_str = path.to_string_lossy().to_string();
    std::fs::write(&path_str, b"line1\r\nline2\nline3\rline4").unwrap();

    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path_str).unwrap();
    assert_eq!(table.read_line(fd).unwrap(), Some("line1".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), Some("line2".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), Some("line3".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), Some("line4".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), None);
    table.close(fd).unwrap();
    let _ = std::fs::remove_file(&path_str);
}

#[test]
fn rb8_readline_utf8_multibyte_in_line() {
    let path = tmp_path("rb8_utf8_line");
    let path_str = path.to_string_lossy().to_string();
    // "©\n中\r\n😀"
    std::fs::write(&path_str, "\u{00A9}\n\u{4E2D}\r\n\u{1F600}".as_bytes()).unwrap();

    let table = FileDescriptorTable::new();
    let fd = table.open_read(&path_str).unwrap();
    assert_eq!(table.read_line(fd).unwrap(), Some("\u{00A9}".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), Some("\u{4E2D}".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), Some("\u{1F600}".to_string()));
    assert_eq!(table.read_line(fd).unwrap(), None);
    table.close(fd).unwrap();
    let _ = std::fs::remove_file(&path_str);
}
