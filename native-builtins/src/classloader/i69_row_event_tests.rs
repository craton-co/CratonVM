use super::{
    memoised_archive_noted, memoised_walk_index_noted, ArchiveBytesCache, WalkIndexCache,
};
use cratonvm_classloading::ClassPath;
use cratonvm_native_api::NestedJarRowEvent as E;
use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

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

/// The LOADER census's nested-jar row events (interpreter round i2 wave
/// 69, lane L5): a miss that builds, a hit (no event), a changed row
/// rebuilt, a rebuild that refused, a refusal kept (no retry within the
/// interval) -- for the bytes rows and the walk rows alike; a changed
/// row under the old rule (no refusal path) is a refusal, then kept.
#[test]
fn row_events_are_reported_in_order() {
    let dir = std::env::temp_dir().join("cratonvm_i69_row_events");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("lib.jar");
    std::fs::write(&jar, jar_bytes(&["p/A.class"])).unwrap();
    let events: RefCell<Vec<E>> = RefCell::new(Vec::new());
    let note = |event: E| events.borrow_mut().push(event);
    let jar_path = || Some(jar.clone());
    let refusal: Option<&dyn Fn() -> Option<PathBuf>> = Some(&jar_path);
    let key = jar.to_string_lossy().into_owned();
    let open = || ClassPath::open_jar_archive(&jar, false).map(Arc::new);
    let bytes: OrderedPlMutex<ArchiveBytesCache> =
        OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch);

    assert!(memoised_archive_noted(&bytes, &key, 16, refusal, &note, open).is_some());
    assert!(memoised_archive_noted(&bytes, &key, 16, refusal, &note, open).is_some());
    std::fs::write(&jar, jar_bytes(&["p/A.class", "p/B.class"])).unwrap();
    assert!(memoised_archive_noted(&bytes, &key, 16, refusal, &note, open).is_some());
    std::fs::write(&jar, b"not a zip").unwrap();
    assert!(memoised_archive_noted(&bytes, &key, 16, refusal, &note, open).is_none());
    assert!(memoised_archive_noted(&bytes, &key, 16, refusal, &note, open).is_none());
    assert_eq!(
        events.borrow_mut().split_off(0),
        vec![E::Built, E::Rebuilt, E::Rebuilt, E::Refused, E::RefusalKept]
    );

    // The old rule: a changed bytes row becomes a refusal for good.
    std::fs::write(&jar, jar_bytes(&["p/A.class"])).unwrap();
    let plain: OrderedPlMutex<ArchiveBytesCache> =
        OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch);
    assert!(memoised_archive_noted(&plain, &key, 16, None, &note, open).is_some());
    std::fs::write(&jar, jar_bytes(&["p/A.class", "p/C.class"])).unwrap();
    assert!(memoised_archive_noted(&plain, &key, 16, None, &note, open).is_none());
    assert!(memoised_archive_noted(&plain, &key, 16, None, &note, open).is_none());
    assert_eq!(
        events.borrow_mut().split_off(0),
        vec![E::Built, E::Refused, E::RefusalKept]
    );

    // The walk rows: a refused build, kept.
    let walk: OrderedPlMutex<WalkIndexCache> =
        OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch);
    let build =
        || ClassPath::nested_archive_entry_index(&jar, "BOOT-INF/lib/none.jar").map(Arc::new);
    assert!(memoised_walk_index_noted(&walk, "w", 16, true, refusal, &note, build).is_none());
    assert!(memoised_walk_index_noted(&walk, "w", 16, true, refusal, &note, build).is_none());
    assert_eq!(
        events.borrow_mut().split_off(0),
        vec![E::Refused, E::RefusalKept]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
