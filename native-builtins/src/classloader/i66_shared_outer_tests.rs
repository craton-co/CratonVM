use super::{
    memoised_walk_index, shared_outer_archive, shared_outer_index, ArchiveBytesCaches,
};
use cratonvm_classloading::ClassPath;
use std::cell::Cell;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// A fat jar with two stored nested jars; `extra` adds `p/New.class` to
/// the second.
fn fat_jar(path: &Path, extra: bool) {
    fn inner(entries: &[&str]) -> Vec<u8> {
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let mut out = Vec::new();
        {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
            for name in entries {
                z.start_file(*name, opts).unwrap();
                z.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
            }
            z.finish().unwrap();
        }
        out
    }
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    let one = inner(&["o/One.class"]);
    let two = if extra {
        inner(&["p/Old.class", "p/New.class"])
    } else {
        inner(&["p/Old.class"])
    };
    let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    z.start_file("BOOT-INF/lib/one.jar", opts).unwrap();
    z.write_all(&one).unwrap();
    z.start_file("BOOT-INF/lib/two.jar", opts).unwrap();
    z.write_all(&two).unwrap();
    z.finish().unwrap();
}

/// `CRATONVM_NESTED_JAR_SHARED_OUTER` (interpreter round i2 wave 66,
/// lane L5): one VM opens a fat jar once for all its nested jars, reopens
/// it after a rewrite (so a stamped walk row rebuilt from it sees the
/// rewritten jar), forgets a refused open, and two VMs never share an open.
#[test]
fn one_outer_open_serves_every_nested_jar_of_a_vm() {
    let dir = std::env::temp_dir().join("cratonvm_i66_shared_outer");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("app.jar");
    fat_jar(&jar, false);
    let outer_key = jar.to_string_lossy().into_owned();

    let vm = ArchiveBytesCaches::new();
    let first = shared_outer_archive(&vm, &outer_key).expect("opened");
    let second = shared_outer_archive(&vm, &outer_key).expect("held");
    assert!(Arc::ptr_eq(&first, &second), "one open per fat jar per VM");
    let one = ClassPath::nested_archive_entry_index_in(&first, "BOOT-INF/lib/one.jar").unwrap();
    let two = ClassPath::nested_archive_entry_index_in(&first, "BOOT-INF/lib/two.jar").unwrap();
    assert!(one.contains("o/One.class") && !one.contains("p/Old.class"));
    assert!(two.contains("p/Old.class") && !two.contains("p/New.class"));
    let bytes = ClassPath::open_nested_archive_in(&first, "BOOT-INF/lib/two.jar").unwrap();
    assert_eq!(
        bytes.entry_bytes("p/Old.class").as_deref(),
        Some(&b"\xCA\xFE\xBA\xBE"[..])
    );

    let other_vm = ArchiveBytesCaches::new();
    let other = shared_outer_archive(&other_vm, &outer_key).expect("opened");
    assert!(!Arc::ptr_eq(&first, &other), "two VMs, two opens");

    // The stamped walk row over the shared outer: rebuilt after a
    // rewrite, from a reopened outer.
    let builds = Cell::new(0u32);
    let build = || {
        builds.set(builds.get() + 1);
        let outer = shared_outer_archive(&vm, &outer_key)?;
        ClassPath::nested_archive_entry_index_in(&outer, "BOOT-INF/lib/two.jar").map(Arc::new)
    };
    let key = "nested:/x/app.jar/!BOOT-INF/lib/two.jar!/";
    let row = memoised_walk_index(&vm.walk, key, 16, true, build).expect("indexed");
    assert!(!row.contains("p/New.class"));
    drop((first, second, bytes, other));

    fat_jar(&jar, true);
    let fresh = memoised_walk_index(&vm.walk, key, 16, true, build).expect("rebuilt");
    assert!(fresh.contains("p/New.class"), "the rewritten jar's names");
    assert_eq!(builds.get(), 2);
    let reopened = shared_outer_archive(&vm, &outer_key).expect("reopened");
    assert!(reopened.unchanged());
    assert_eq!(
        vm.outer.lock().len(),
        1,
        "the reopened row replaced the old one"
    );

    // A refused open is not remembered, and drops the row.
    drop(reopened);
    let _ = std::fs::remove_file(&jar);
    assert!(shared_outer_archive(&vm, &outer_key).is_none());
    assert_eq!(vm.outer.lock().len(), 0);
    fat_jar(&jar, false);
    assert!(
        shared_outer_archive(&vm, &outer_key).is_some(),
        "found once it exists"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `CRATONVM_NESTED_JAR_OUTER_INDEX` (interpreter round i2 wave 67, lane
/// L5): one VM parses a fat jar's central directory once for all its
/// nested jars and holds no bytes or handle, so the fat jar can be
/// truncated, rewritten and deleted while the parse and the rows built
/// from it are held -- on every host -- and the next build is refused or
/// reparsed, never a crash.
#[test]
fn one_outer_index_serves_every_nested_jar_without_holding_the_file() {
    let dir = std::env::temp_dir().join("cratonvm_i67_outer_index");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("app.jar");
    fat_jar(&jar, false);
    let outer_key = jar.to_string_lossy().into_owned();

    let vm = ArchiveBytesCaches::new();
    let first = shared_outer_index(&vm, &outer_key).expect("parsed");
    let second = shared_outer_index(&vm, &outer_key).expect("held");
    assert!(Arc::ptr_eq(&first, &second), "one parse per fat jar per VM");
    assert!(!first.holds_open_file());
    let one =
        ClassPath::nested_archive_entry_index_in_index(&first, "BOOT-INF/lib/one.jar").unwrap();
    let two =
        ClassPath::nested_archive_entry_index_in_index(&first, "BOOT-INF/lib/two.jar").unwrap();
    assert!(one.contains("o/One.class") && !one.contains("p/Old.class"));
    assert!(two.contains("p/Old.class") && !two.contains("p/New.class"));
    let bytes =
        ClassPath::open_nested_archive_in_index(&first, "BOOT-INF/lib/two.jar").unwrap();
    assert_eq!(
        bytes.entry_bytes("p/Old.class").as_deref(),
        Some(&b"\xCA\xFE\xBA\xBE"[..])
    );

    let other_vm = ArchiveBytesCaches::new();
    let other = shared_outer_index(&other_vm, &outer_key).expect("parsed");
    assert!(!Arc::ptr_eq(&first, &other), "two VMs, two parses");

    // Truncated while everything above is still held (no mapping and no
    // handle, so this succeeds on Windows too): the held parse is
    // refused, and the bytes row still answers from its own copy.
    let full = std::fs::read(&jar).unwrap();
    std::fs::write(&jar, &full[..full.len() / 3]).unwrap();
    assert!(!first.unchanged());
    assert!(ClassPath::open_nested_archive_in_index(&first, "BOOT-INF/lib/one.jar").is_none());
    assert!(
        shared_outer_index(&vm, &outer_key).is_none(),
        "a third of a zip"
    );
    assert_eq!(
        vm.outer_index.lock().len(),
        0,
        "the refused parse dropped the row"
    );
    assert_eq!(
        bytes.entry_bytes("p/Old.class").as_deref(),
        Some(&b"\xCA\xFE\xBA\xBE"[..])
    );
    assert!(!bytes.outer_unchanged());

    // The stamped walk row over the outer index: rebuilt after a
    // rewrite, from a reparsed outer.
    fat_jar(&jar, false);
    let builds = Cell::new(0u32);
    let build = || {
        builds.set(builds.get() + 1);
        let outer = shared_outer_index(&vm, &outer_key)?;
        ClassPath::nested_archive_entry_index_in_index(&outer, "BOOT-INF/lib/two.jar")
            .map(Arc::new)
    };
    let key = "nested:/x/app.jar/!BOOT-INF/lib/two.jar!/";
    let row = memoised_walk_index(&vm.walk, key, 16, true, build).expect("indexed");
    assert!(!row.contains("p/New.class"));
    fat_jar(&jar, true);
    let fresh = memoised_walk_index(&vm.walk, key, 16, true, build).expect("rebuilt");
    assert!(fresh.contains("p/New.class"), "the rewritten jar's names");
    assert_eq!(builds.get(), 2);
    assert_eq!(
        vm.outer_index.lock().len(),
        1,
        "the reparsed row replaced the old one"
    );

    // Deleted while held: no handle blocks it.
    std::fs::remove_file(&jar).unwrap();
    assert!(shared_outer_index(&vm, &outer_key).is_none());
    assert_eq!(vm.outer_index.lock().len(), 0);
    drop((first, second, other, one, two, bytes, row, fresh));

    let _ = std::fs::remove_dir_all(&dir);
}

/// `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE` (interpreter round i2 wave
/// 69, lane L5): each VM has its own pool of kept outer handles; a burst
/// of reads from one VM's directory-only rows keeps (on Unix) one handle
/// of that VM's and none of the other's, every read answers the bytes the
/// unpooled read answers, and dropping the VM's caches closes it.
#[test]
fn a_burst_keeps_one_outer_handle_per_vm() {
    let dir = std::env::temp_dir().join("cratonvm_i69_outer_handles");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("app.jar");
    fat_jar(&jar, false);
    let outer_key = jar.to_string_lossy().into_owned();

    let vm = ArchiveBytesCaches::new();
    let other_vm = ArchiveBytesCaches::new();
    let outer = shared_outer_index(&vm, &outer_key).expect("parsed");
    let one = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/one.jar")
        .unwrap();
    let two = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/two.jar")
        .unwrap();
    assert!(!one.holds_nested_bytes() && !two.holds_nested_bytes());
    let class = Some(b"\xCA\xFE\xBA\xBE".to_vec());
    for _ in 0..3 {
        // Two rows of one fat jar share its handle.
        assert_eq!(
            one.entry_bytes_pooled("o/One.class", Some(&vm.outer_handles)),
            class
        );
        assert_eq!(
            two.entry_bytes_pooled("p/Old.class", Some(&vm.outer_handles)),
            class
        );
        assert_eq!(
            two.entry_bytes_pooled("p/None.class", Some(&vm.outer_handles)),
            None
        );
    }
    assert_eq!(
        one.entry_bytes("o/One.class"),
        class,
        "unpooled: the same bytes"
    );
    let kept = usize::from(cfg!(unix));
    assert_eq!(
        vm.outer_handles.held(),
        kept,
        "one handle for the fat jar (Unix)"
    );
    assert_eq!(
        other_vm.outer_handles.held(),
        0,
        "the other VM keeps nothing"
    );
    assert_eq!(vm.outer_handles.opens() + vm.outer_handles.reuses(), 6);
    if cfg!(unix) {
        assert_eq!(vm.outer_handles.opens(), 2, "two opens start the burst");
    }
    drop((one, two, outer));
    drop(vm);
    // Deleted after the VM's caches are gone: nothing holds it.
    std::fs::remove_file(&jar).unwrap();
    drop(other_vm);
    let _ = std::fs::remove_dir_all(&dir);
}
