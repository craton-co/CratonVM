use super::{
    memoised_archive, memoised_archive_with, memoised_walk_index, memoised_walk_index_with,
    ArchiveBytesCache, WalkIndexCache, NESTED_JAR_REFUSAL_RETRY_INTERVAL,
};
use cratonvm_classloading::ClassPath;
use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A plain jar with `entries` (each holding its own name's bytes),
/// stored; used both as a jar and as the one nested jar of a fat jar.
fn jar_bytes(entries: &[&str]) -> Vec<u8> {
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    let mut out = Vec::new();
    {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
        for name in entries {
            z.start_file(*name, opts).unwrap();
            z.write_all(name.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }
    out
}

fn fat_jar(path: &Path, nested_entries: &[&str]) {
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    let inner = jar_bytes(nested_entries);
    let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    z.start_file("BOOT-INF/lib/dep.jar", opts).unwrap();
    z.write_all(&inner).unwrap();
    z.finish().unwrap();
}

fn past_the_interval() {
    std::thread::sleep(
        NESTED_JAR_REFUSAL_RETRY_INTERVAL + std::time::Duration::from_millis(20),
    );
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn walk_cache() -> OrderedPlMutex<WalkIndexCache> {
    OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch)
}

fn bytes_cache() -> OrderedPlMutex<ArchiveBytesCache> {
    OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch)
}

/// `CRATONVM_NESTED_JAR_REFUSAL_STAMP` (interpreter round i2 wave 68,
/// lane L5) on the walk index: a fat jar caught half written is refused,
/// the refusal is kept while the file is unchanged (and for the retry
/// interval, with no stat), and rebuilt once the file is whole again.
/// Without a refusal path the refusal is kept for the VM's life, as
/// before; a key that names no file is refused for good either way.
#[test]
fn a_stamped_walk_refusal_is_rebuilt_once_the_fat_jar_changes() {
    let dir = scratch("cratonvm_i68_walk_refusal_stamp");
    let jar = dir.join("app.jar");
    std::fs::write(&jar, b"not yet a zip").unwrap();
    let builds = Cell::new(0u32);
    let build = || {
        builds.set(builds.get() + 1);
        ClassPath::nested_archive_entry_index(&jar, "BOOT-INF/lib/dep.jar").map(Arc::new)
    };
    let jar_path = || Some(jar.clone());
    let refusal: Option<&dyn Fn() -> Option<PathBuf>> = Some(&jar_path);
    let key = "nested:/x/app.jar/!BOOT-INF/lib/dep.jar!/";

    let stamped_vm = walk_cache();
    let plain_vm = walk_cache();
    assert!(memoised_walk_index_with(&stamped_vm, key, 16, true, refusal, build).is_none());
    assert!(memoised_walk_index(&plain_vm, key, 16, true, build).is_none());
    assert_eq!(builds.get(), 2);
    // Within the interval, and past it with the file unchanged: kept.
    assert!(memoised_walk_index_with(&stamped_vm, key, 16, true, refusal, build).is_none());
    past_the_interval();
    assert!(memoised_walk_index_with(&stamped_vm, key, 16, true, refusal, build).is_none());
    assert_eq!(builds.get(), 2, "an unchanged file is not rebuilt");

    // The fat jar is whole now.
    fat_jar(&jar, &["p/A.class"]);
    let row =
        memoised_walk_index_with(&stamped_vm, key, 16, true, refusal, build).expect("rebuilt");
    assert!(row.contains("p/A.class"));
    assert_eq!(builds.get(), 3);
    let hit =
        memoised_walk_index_with(&stamped_vm, key, 16, true, refusal, build).expect("held");
    assert!(Arc::ptr_eq(&row, &hit));
    assert!(
        memoised_walk_index(&plain_vm, key, 16, true, build).is_none(),
        "unstamped: kept"
    );
    assert_eq!(builds.get(), 3);

    // Truncated under the built row: the stamped walk rebuild is refused,
    // stamped, and rebuilt once the jar is whole again.
    let full = std::fs::read(&jar).unwrap();
    std::fs::write(&jar, &full[..full.len() / 2]).unwrap();
    assert!(memoised_walk_index_with(&stamped_vm, key, 16, true, refusal, build).is_none());
    assert_eq!(builds.get(), 4);
    past_the_interval();
    fat_jar(&jar, &["p/A.class", "p/B.class"]);
    let again =
        memoised_walk_index_with(&stamped_vm, key, 16, true, refusal, build).expect("rebuilt");
    assert!(again.contains("p/B.class"));
    assert_eq!(builds.get(), 5);

    // A key that names no file: refused for good.
    let none = || -> Option<PathBuf> { None };
    let no_file: Option<&dyn Fn() -> Option<PathBuf>> = Some(&none);
    assert!(memoised_walk_index_with(&stamped_vm, "bad", 16, true, no_file, || None).is_none());
    past_the_interval();
    assert!(memoised_walk_index_with(&stamped_vm, "bad", 16, true, no_file, build).is_none());
    assert_eq!(builds.get(), 5);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The bytes indexes under `CRATONVM_NESTED_JAR_REFUSAL_STAMP`: a row
/// whose jar was rewritten is rebuilt (the new bytes), where without it
/// the row turns into a refusal for the VM's life; a deleted jar is
/// refused until it exists again.
#[test]
fn a_stamped_bytes_row_is_rebuilt_after_a_rewrite() {
    let dir = scratch("cratonvm_i68_bytes_refusal_stamp");
    let jar = dir.join("lib.jar");
    std::fs::write(&jar, jar_bytes(&["p/A.class"])).unwrap();
    let builds = Cell::new(0u32);
    let open = || {
        builds.set(builds.get() + 1);
        ClassPath::open_jar_archive(&jar, false).map(Arc::new)
    };
    let jar_path = || Some(jar.clone());
    let refusal: Option<&dyn Fn() -> Option<PathBuf>> = Some(&jar_path);
    let key = jar.to_string_lossy().into_owned();

    let stamped_vm = bytes_cache();
    let plain_vm = bytes_cache();
    let row = memoised_archive_with(&stamped_vm, &key, 16, refusal, open).expect("opened");
    assert_eq!(
        row.entry_bytes("p/A.class").as_deref(),
        Some(&b"p/A.class"[..])
    );
    assert!(memoised_archive(&plain_vm, &key, 16, open).is_some());
    assert_eq!(builds.get(), 2);

    std::fs::write(&jar, jar_bytes(&["p/A.class", "p/B.class"])).unwrap();
    assert!(
        memoised_archive(&plain_vm, &key, 16, open).is_none(),
        "unstamped: refused"
    );
    assert!(
        memoised_archive(&plain_vm, &key, 16, open).is_none(),
        "for good"
    );
    assert_eq!(builds.get(), 2);
    let fresh = memoised_archive_with(&stamped_vm, &key, 16, refusal, open).expect("rebuilt");
    assert_eq!(
        fresh.entry_bytes("p/B.class").as_deref(),
        Some(&b"p/B.class"[..])
    );
    assert_eq!(builds.get(), 3);

    // Deleted: refused (no file, stamp `None`), rebuilt once it exists.
    std::fs::remove_file(&jar).unwrap();
    assert!(memoised_archive_with(&stamped_vm, &key, 16, refusal, open).is_none());
    assert_eq!(builds.get(), 4);
    past_the_interval();
    assert!(memoised_archive_with(&stamped_vm, &key, 16, refusal, open).is_none());
    assert_eq!(builds.get(), 4, "still absent: not rebuilt");
    std::fs::write(&jar, jar_bytes(&["p/C.class"])).unwrap();
    let back = memoised_archive_with(&stamped_vm, &key, 16, refusal, open).expect("rebuilt");
    assert_eq!(
        back.entry_bytes("p/C.class").as_deref(),
        Some(&b"p/C.class"[..])
    );
    assert_eq!(builds.get(), 5);
    drop((row, fresh, back));

    let _ = std::fs::remove_dir_all(&dir);
}
