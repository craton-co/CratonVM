use super::*;

fn line(start: usize, len: usize, name: &str) -> String {
    let mut out = Vec::new();
    write_line(&mut out, start, len, name).expect("Vec writes cannot fail");
    String::from_utf8(out).expect("utf-8")
}

#[test]
fn a_line_is_lowercase_hex_start_size_then_the_name() {
    assert_eq!(
        line(0x7f00_dead_b000, 0x1a0, "java/lang/String.hashCode()I [c1]"),
        "7f00deadb000 1a0 java/lang/String.hashCode()I [c1]\n"
    );
}

#[test]
fn spaces_survive_and_line_breaks_do_not() {
    assert_eq!(line(0x10, 0x20, "a b\nc\r\nd"), "10 20 a bcd\n");
}

#[test]
fn exactly_one_newline_per_record() {
    let text = line(1, 2, "x\n\n\ny");
    assert_eq!(text.matches('\n').count(), 1);
    assert!(text.ends_with('\n'));
}

fn render(lines: &Lines) -> String {
    let mut out = Vec::new();
    write_all_lines(&mut out, lines).expect("Vec writes cannot fail");
    String::from_utf8(out).expect("utf-8")
}

#[test]
fn a_region_that_reuses_freed_code_replaces_the_lines_it_overlaps() {
    let mut lines = Lines::new();
    assert!(!admit(&mut lines, 0x1000, 0x2760, "A.append [c1]"));
    assert!(!admit(&mut lines, 0x3760, 0x67, "not-entrant-stub A.append"));
    assert!(!admit(&mut lines, 0x8000, 0x100, "B.far [c2]"));
    // A later body placed inside the freed range of the first one.
    assert!(admit(&mut lines, 0x2000, 0x2930, "C.copyOfRange [c2]"));
    assert_eq!(
        render(&lines),
        "2000 2930 C.copyOfRange [c2]\n8000 100 B.far [c2]\n",
        "both the freed body and its stub are gone; the untouched line stays"
    );
}

#[test]
fn disjoint_regions_only_append() {
    let mut lines = Lines::new();
    assert!(!admit(&mut lines, 0x2000, 0x100, "x"));
    // Adjacent on both sides, not overlapping.
    assert!(!admit(&mut lines, 0x1f00, 0x100, "before"));
    assert!(!admit(&mut lines, 0x2100, 0x10, "after"));
    assert_eq!(render(&lines), "1f00 100 before\n2000 100 x\n2100 10 after\n");
}

#[test]
fn the_map_is_named_after_the_pid() {
    let path = map_path(4242);
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("perf-4242.map")
    );
    #[cfg(not(windows))]
    assert_eq!(path, std::path::PathBuf::from("/tmp/perf-4242.map"));
}
