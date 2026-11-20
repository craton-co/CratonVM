use super::*;

#[test]
fn extracts_declared_checksums() {
    let out = "hello\n\
               ##DIFFTEST-CHECKSUM## arith 0x1234\n\
               world\n\
               ##DIFFTEST-CHECKSUM## strings deadbeefdeadbeef\n";
    let sums = extract(out);
    assert_eq!(sums.len(), 2);
    assert_eq!(sums["arith"], "0x1234");
    assert_eq!(sums["strings"], "deadbeefdeadbeef");
}

#[test]
fn a_program_that_declares_nothing_has_no_checksum_channel() {
    assert!(extract("plain output\nno markers here\n").is_empty());
    assert_eq!(render(&Checksums::new()), "<none declared>");
}

#[test]
fn indented_and_malformed_declarations() {
    // Indentation is tolerated; a marker with no name is ignored rather
    // than recorded under an empty key.
    let sums = extract("    ##DIFFTEST-CHECKSUM## indented v\n##DIFFTEST-CHECKSUM##\n");
    assert_eq!(sums.len(), 1);
    assert_eq!(sums["indented"], "v");
}

#[test]
fn a_repeated_name_keeps_the_last_value() {
    let sums = extract("##DIFFTEST-CHECKSUM## n 1\n##DIFFTEST-CHECKSUM## n 2\n");
    assert_eq!(sums["n"], "2");
}

#[test]
fn differing_names_reports_value_and_presence_splits() {
    let a = extract("##DIFFTEST-CHECKSUM## same 1\n##DIFFTEST-CHECKSUM## diff a\n");
    let b = extract(
        "##DIFFTEST-CHECKSUM## same 1\n##DIFFTEST-CHECKSUM## diff b\n\
                     ##DIFFTEST-CHECKSUM## only-b x\n",
    );
    assert_eq!(differing_names(&a, &b), vec!["diff", "only-b"]);
    assert!(differing_names(&a, &a).is_empty());
}

#[test]
fn fnv1a_matches_the_published_vectors() {
    // The reference FNV-1a 64 test vectors, so a seed reimplementing this
    // in Java can check itself against the same numbers.
    assert_eq!(fnv1a64_hex(b""), "cbf29ce484222325");
    assert_eq!(fnv1a64_hex(b"a"), "af63dc4c8601ec8c");
    assert_eq!(fnv1a64_hex(b"foobar"), "85944171f73967e8");
    assert_eq!(digest("a"), fnv1a64_hex(b"a"));
}

#[test]
fn render_is_stable_regardless_of_declaration_order() {
    let a = extract("##DIFFTEST-CHECKSUM## b 2\n##DIFFTEST-CHECKSUM## a 1\n");
    let b = extract("##DIFFTEST-CHECKSUM## a 1\n##DIFFTEST-CHECKSUM## b 2\n");
    assert_eq!(render(&a), render(&b));
    assert_eq!(render(&a), "a=1\nb=2");
}
