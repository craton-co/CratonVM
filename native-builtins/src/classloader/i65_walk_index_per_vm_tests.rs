use super::{memoised_walk_index, WalkIndexCache};
use cratonvm_classloading::ClassPath;
use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
use std::cell::Cell;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

fn fat_jar(path: &Path, nested_entries: &[&str]) {
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    let mut inner = Vec::new();
    {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut inner));
        for name in nested_entries {
            z.start_file(*name, opts).unwrap();
            z.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
        }
        z.finish().unwrap();
    }
    let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    z.start_file("BOOT-INF/lib/dep.jar", opts).unwrap();
    z.write_all(&inner).unwrap();
    z.finish().unwrap();
}

fn new_cache() -> OrderedPlMutex<WalkIndexCache> {
    OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch)
}

/// The walk index's rows (one map per VM since wave 65): without the
/// stamp a row answers as built, even after the fat jar was rewritten
/// (the wave-54 behaviour, the i63 defect); with it the rewritten jar's
/// row is rebuilt and the added class is no longer called absent.
/// Two caches are two VMs: neither sees the other's row.
#[test]
fn a_stamped_row_is_rebuilt_after_the_fat_jar_is_rewritten() {
    let dir = std::env::temp_dir().join("cratonvm_i65_walk_index_per_vm");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("app.jar");
    fat_jar(&jar, &["p/Old.class"]);
    let builds = Cell::new(0u32);
    let build = || {
        builds.set(builds.get() + 1);
        ClassPath::nested_archive_entry_index(&jar, "BOOT-INF/lib/dep.jar").map(Arc::new)
    };
    let key = "nested:/x/app.jar/!BOOT-INF/lib/dep.jar!/";

    let plain_vm = new_cache();
    let stamped_vm = new_cache();
    for (cache, stamped) in [(&plain_vm, false), (&stamped_vm, true)] {
        let row = memoised_walk_index(cache, key, 16, stamped, build).expect("indexed");
        assert!(row.contains("p/Old.class") && !row.contains("p/New.class"));
        // A hit does not rebuild an unchanged jar.
        let again = memoised_walk_index(cache, key, 16, stamped, build).expect("indexed");
        assert!(Arc::ptr_eq(&row, &again));
    }
    assert_eq!(builds.get(), 2, "one build per VM, none on a hit");

    fat_jar(&jar, &["p/Old.class", "p/New.class"]);
    let stale = memoised_walk_index(&plain_vm, key, 16, false, build).expect("the old row");
    assert!(!stale.contains("p/New.class"), "unstamped: the old answer");
    assert_eq!(builds.get(), 2);
    let fresh = memoised_walk_index(&stamped_vm, key, 16, true, build).expect("rebuilt");
    assert!(
        fresh.contains("p/New.class"),
        "stamped: the rewritten jar's names"
    );
    assert_eq!(builds.get(), 3);
    let hit = memoised_walk_index(&stamped_vm, key, 16, true, build).expect("the new row");
    assert!(Arc::ptr_eq(&fresh, &hit));
    assert_eq!(builds.get(), 3);

    // A refusal is memoised (the body runs, which is always right).
    assert!(memoised_walk_index(&stamped_vm, "other", 16, true, || None).is_none());
    assert!(memoised_walk_index(&stamped_vm, "other", 16, true, build).is_none());
    assert_eq!(builds.get(), 3);

    // The bound: a full map is cleared before the next insert.
    let bounded = new_cache();
    for i in 0..3 {
        let _ = memoised_walk_index(&bounded, &format!("k{i}"), 2, false, || None);
    }
    assert!(bounded.lock().len() <= 2);

    let _ = std::fs::remove_dir_all(&dir);
}
