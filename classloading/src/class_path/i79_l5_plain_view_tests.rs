use super::*;
use std::io::Write;

/// A stored zip of `entries`, in memory.
fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let mut z = zip::ZipWriter::new(&mut cursor);
        for (name, data) in entries {
            z.start_file(*name, stored).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }
    cursor.into_inner()
}

/// `zip` 2.x's writer refuses a duplicate name, so the duplicate is made
/// by renaming `p/B.class` to `p/A.class` (same length) in the local
/// header and the central directory.
fn with_duplicate() -> Vec<u8> {
    let mut bytes = zip_of(&[("p/A.class", &b"first"[..]), ("p/B.class", &b"other"[..])]);
    let from = b"p/B.class";
    let mut at = 0;
    while let Some(pos) = bytes[at..].windows(from.len()).position(|w| w == from) {
        bytes[at + pos..at + pos + from.len()].copy_from_slice(b"p/A.class");
        at += pos + from.len();
    }
    bytes
}

/// i71-L5 "the plain view's duplicate-name check is always true": the
/// `zip` map keeps one entry per name, so a jar with two records of
/// `p/A.class` has `len() == 1` == its entry index; the record walk sees
/// two, and the plain view is refused. A clean jar keeps it.
#[test]
fn i79_a_duplicated_name_has_no_plain_view() {
    let dup = ZipArchive::new(Cursor::new(ArchiveBacking::from_vec(with_duplicate()))).unwrap();
    assert_eq!(
        dup.len(),
        1,
        "the zip crate's map keeps one record per name"
    );
    assert_eq!(shared_archive_records(&dup), Some(2), "two records on disk");
    let mut indexed = dup.clone();
    let index = ClassPath::build_archive_entry_index(&mut indexed);
    assert_eq!(index.len(), dup.len(), "the old check could not tell");
    assert!(!plain_view_of(
        &index,
        dup.len(),
        shared_archive_records(&dup)
    ));

    let clean = ZipArchive::new(Cursor::new(ArchiveBacking::from_vec(zip_of(&[
        ("p/A.class", &b"first"[..]),
        ("p/B.class", &b"other"[..]),
    ]))))
    .unwrap();
    let mut indexed = clean.clone();
    let index = ClassPath::build_archive_entry_index(&mut indexed);
    assert_eq!(shared_archive_records(&clean), Some(2));
    assert!(plain_view_of(
        &index,
        clean.len(),
        shared_archive_records(&clean)
    ));

    // Through a row: `open_jar_archive` over the files.
    let dir = std::env::temp_dir().join(format!("cratonvm_i79_l5_dup_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let dup_path = dir.join("dup.jar");
    let clean_path = dir.join("clean.jar");
    fs::write(&dup_path, with_duplicate()).unwrap();
    fs::write(
        &clean_path,
        zip_of(&[("p/A.class", &b"first"[..]), ("p/B.class", &b"other"[..])]),
    )
    .unwrap();
    assert!(!ClassPath::open_jar_archive(&dup_path, false)
        .expect("dup row")
        .plain_view());
    assert!(ClassPath::open_jar_archive(&clean_path, false)
        .expect("clean row")
        .plain_view());
    let _ = fs::remove_dir_all(&dir);
}

/// The walk stops at the first non-header and counts at most `limit + 1`.
#[test]
fn i79_the_record_walk_is_bounded() {
    let bytes = zip_of(&[("a", &b"1"[..]), ("b", &b"2"[..]), ("c", &b"3"[..])]);
    let archive = ZipArchive::new(Cursor::new(ArchiveBacking::from_vec(bytes))).unwrap();
    let start = archive.central_directory_start();
    let mut reader = archive.clone().into_inner();
    assert_eq!(count_central_records(&mut reader, start, 10), Some(3));
    assert_eq!(
        count_central_records(&mut reader, start, 1),
        Some(2),
        "limit + 1"
    );
    assert_eq!(
        count_central_records(&mut reader, 0, 10),
        Some(0),
        "a local header is not one"
    );
}
