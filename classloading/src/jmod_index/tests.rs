use super::*;

/// A minimal ZIP writer: stored entries only, optional extra fields, so
/// the parser's refusals can be driven byte by byte.
fn zip_with(entries: &[(&str, &[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cd = Vec::new();
    for (name, data, extra) in entries {
        let off = out.len() as u32;
        let crc = crc32fast::hash(data);
        out.extend_from_slice(&LFH_SIG.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        cd.extend_from_slice(&CDH_SIG.to_le_bytes());
        cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        // comment length, disk start, internal and external attributes
        cd.extend_from_slice(&[0u8; 10]);
        cd.extend_from_slice(&off.to_le_bytes());
        cd.extend_from_slice(name.as_bytes());
        cd.extend_from_slice(extra);
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&cd);
    out.extend_from_slice(&EOCD_SIG.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn zr8_fm_index_matches_zip_archive_names_and_locations() {
    let bytes = zip_with(&[
        ("classes/a/A.class", b"AAAA", b""),
        ("classes/b/B.class", b"BBBBBB", b""),
        ("lib/x.so", b"xx", b""),
    ]);
    let idx = JmodCentralIndex::parse(&bytes).expect("plain archive parses");
    let names: Vec<String> = idx.names().iter_all().map(str::to_string).collect();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
    let zip_names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index_raw(i).unwrap().name().unwrap().into_owned())
        .collect();
    assert_eq!(names, zip_names);
    assert_eq!(idx.len(), zip.len());
    for n in &zip_names {
        let loc = idx.locate(&bytes, n).expect("present entry locates");
        let e = zip.by_name(n).unwrap();
        assert_eq!(Some(loc.data_start as u64), e.data_start());
        assert_eq!(loc.uncompressed, e.size());
        assert_eq!(loc.compressed as u64, e.compressed_size());
        assert_eq!(loc.crc32, e.crc32());
        assert!(loc.stored);
    }
    assert!(!idx.contains("classes/c/C.class"));
    assert!(idx.locate(&bytes, "classes/c/C.class").is_none());
}

/// The three name copies this type replaced, rebuilt the old way, must
/// answer alike for every probe (zt/fp: one name store).
#[test]
fn fp_one_name_store_answers_like_the_old_three_copies() {
    let raw = [
        "classes/",
        "classes/java/lang/Object.class",
        "classes/java/lang/",
        "classes/module-info.class",
        "classes/java/lang/Object.class",
        "lib/libzip.so",
        "classes",
        "conf/classes/x.class",
        "classes/a/b/C$D.class",
        "legal/LICENSE",
    ];
    let names = JmodNames::build(&raw).expect("fits");
    let all: Vec<&str> = names.iter_all().collect();
    let mut distinct: Vec<&str> = Vec::new();
    for n in raw {
        if !distinct.contains(&n) {
            distinct.push(n);
        }
    }
    assert_eq!(all, distinct, "first position wins, duplicates fold");
    let old_classes: std::collections::BTreeSet<String> = distinct
        .iter()
        .filter_map(|n| n.strip_prefix("classes/"))
        .filter(|r| !r.is_empty() && !r.ends_with('/'))
        .map(str::to_string)
        .collect();
    let new_classes: std::collections::BTreeSet<String> =
        names.iter_classes().map(str::to_string).collect();
    assert_eq!(new_classes, old_classes);
    assert_eq!(names.class_count(), old_classes.len());
    for probe in [
        "classes/",
        "classes/java/lang/Object.class",
        "classes/java/lang/",
        "classes",
        "lib/libzip.so",
        "lib/libzip.s",
        "legal/LICENSE",
        "",
        "java/lang/Object.class",
        "x.class",
    ] {
        assert_eq!(names.slot(probe).is_some(), distinct.contains(&probe), "{probe:?}");
    }
    for probe in [
        "",
        "/",
        "java/lang/",
        "java/lang/Object.class",
        "java/lang/Object.clas",
        "module-info.class",
        "a/b/C$D.class",
        "x.class",
        "lib/libzip.so",
    ] {
        assert_eq!(
            names.contains_class(probe),
            old_classes.contains(probe),
            "{probe:?}"
        );
    }
    // A table with many entries still finds every one of them.
    let many: Vec<String> = (0..5000).map(|i| format!("classes/p{}/C{i}.class", i % 37)).collect();
    let big = JmodNames::build(&many).unwrap();
    for (i, n) in many.iter().enumerate() {
        assert_eq!(big.slot(n), Some(i));
        assert!(big.contains_class(n.strip_prefix("classes/").unwrap()));
    }
    assert!(!big.contains_class("p1/Nope.class"));
    let empty = JmodNames::build::<&str>(&[]).unwrap();
    assert!(empty.slot("a").is_none() && !empty.contains_class("a"));
}

#[test]
fn zr8_fm_duplicate_names_keep_first_position_and_last_record() {
    let bytes = zip_with(&[
        ("dup", b"first", b""),
        ("other", b"o", b""),
        ("dup", b"second!", b""),
    ]);
    let idx = JmodCentralIndex::parse(&bytes).unwrap();
    let names: Vec<String> = idx.names().iter_all().map(str::to_string).collect();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
    let zip_names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index_raw(i).unwrap().name().unwrap().into_owned())
        .collect();
    assert_eq!(names, zip_names);
    assert_eq!(names, vec!["dup".to_string(), "other".to_string()]);
    let loc = idx.locate(&bytes, "dup").unwrap();
    let e = zip.by_name("dup").unwrap();
    assert_eq!(Some(loc.data_start as u64), e.data_start());
    assert_eq!(loc.uncompressed, 7);
}

#[test]
fn zr8_fm_refuses_what_it_does_not_model() {
    // A ZIP64 extra field on one entry: the whole archive goes back.
    let zip64 = zip_with(&[("a", b"a", &[0x01, 0x00, 0x00, 0x00])]);
    assert!(JmodCentralIndex::parse(&zip64).is_none());
    // A Unicode-path extra field.
    let upath = zip_with(&[("a", b"a", &[0x75, 0x70, 0x00, 0x00])]);
    assert!(JmodCentralIndex::parse(&upath).is_none());
    // A malformed extra field (declares more bytes than it has).
    let bad = zip_with(&[("a", b"a", &[0x55, 0x54, 0x09, 0x00, 0x01])]);
    assert!(JmodCentralIndex::parse(&bad).is_none());
    // A plain extended-timestamp field is fine.
    let ts = zip_with(&[("a", b"a", &[0x55, 0x54, 0x01, 0x00, 0x00])]);
    assert!(JmodCentralIndex::parse(&ts).is_some());
    // A non-ASCII name.
    let utf = zip_with(&[("caf\u{e9}", b"a", b"")]);
    assert!(JmodCentralIndex::parse(&utf).is_none());
    // A prefixed archive.
    let mut prefixed = vec![0u8; 7];
    prefixed.extend_from_slice(&zip_with(&[("a", b"a", b"")]));
    assert!(JmodCentralIndex::parse(&prefixed).is_none());
    // Not a ZIP at all.
    assert!(JmodCentralIndex::parse(b"not a zip").is_none());
}

#[test]
fn zr8_fm_a_damaged_local_header_fails_only_its_entry() {
    let mut bytes = zip_with(&[("a", b"aaaa", b""), ("b", b"bbbb", b"")]);
    let idx = JmodCentralIndex::parse(&bytes).unwrap();
    // Break entry "a"'s local header signature.
    bytes[0] ^= 0xFF;
    assert!(idx.contains("a"));
    assert!(idx.locate(&bytes, "a").is_none());
    assert!(idx.locate(&bytes, "b").is_some());
}
