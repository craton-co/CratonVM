use super::*;
use std::io::Write;

fn write_jar(dir_tag: &str, manifest: Option<&str>) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cratonvm_w6_attrs_{dir_tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("probe.jar");
    let file = std::fs::File::create(&jar).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    if let Some(m) = manifest {
        zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
        zip.write_all(m.as_bytes()).unwrap();
    }
    zip.start_file("p/one.txt", options).unwrap();
    zip.write_all(b"one").unwrap();
    zip.finish().unwrap();
    jar
}

/// The section a real `JarFileEntry.getAttributes()` would answer with.
#[test]
fn a_per_entry_section_is_found_by_the_entrys_own_name() {
    let jar = write_jar(
        "section",
        Some(
            "Manifest-Version: 1.0\r\nMain-Class: p.Main\r\n\r\n\
             Name: p/one.txt\r\nSection-Key: Section-Value\r\n\r\n",
        ),
    );
    let sections = jar_manifest_sections_cached(jar.to_str().unwrap()).unwrap();
    let one = sections
        .get("p/one.txt")
        .expect("the manifest declares a section for this entry");
    assert!(
        one.iter()
            .any(|(k, v)| k == "Section-Key" && v == "Section-Value"),
        "section pairs were {one:?}"
    );
    // The MAIN attributes are not part of an entry's `attr`:
    // `Manifest.getAttributes(name)` is `entries.get(name)` and nothing
    // more, so a probe reading `Main-Class` off an entry reads null on
    // HotSpot too.
    assert!(
        !one.iter().any(|(k, _)| k == "Main-Class"),
        "main attributes leaked into the entry section: {one:?}"
    );
    assert!(
        sections.get("p/two.txt").is_none(),
        "an entry with no section must not acquire one"
    );
}

/// A jar with no manifest answers the EMPTY map rather than `None`, which
/// is what makes the negative cacheable.
#[test]
fn a_jar_with_no_manifest_caches_an_empty_section_map() {
    let jar = write_jar("nomanifest", None);
    let sections = jar_manifest_sections_cached(jar.to_str().unwrap())
        .expect("a jar with no manifest still answers");
    assert!(sections.is_empty(), "sections were {sections:?}");
    // Twice, because the second call is the one that would re-read the
    // archive if the negative were not cached.
    let again = jar_manifest_sections_cached(jar.to_str().unwrap()).unwrap();
    assert!(again.is_empty());
}

/// An empty path is the closed-`JarFile` shape (`close()` clears slot 0),
/// and must not be treated as a jar.
#[test]
fn an_empty_path_is_not_a_jar() {
    assert!(jar_manifest_sections_cached("").is_none());
}

/// The sections cache's `LockLevel::Scratch` claim is actually EVALUATED,
/// and the miss path does not hold its guard across the read it needs.
///
/// Modelled on `memo_lock_is_order_checked_and_not_re_entered_on_a_miss`
/// further down this file, because it is the same claim about the same
/// kind of lock and the sibling has already been bitten once by the shape
/// this asserts the absence of.
///
/// * `enforcement_active()` is unconditionally true in DEBUG builds, which
///   is what makes the three tests above live checks of the ordering claim
///   rather than runs with the checker asleep. Asserting it here means a
///   future change that makes debug enforcement conditional turns this
///   module from silently vacuous into loudly red. `cfg`-gated because in
///   RELEASE enforcement is off unless `CRATONVM_LOCK_ORDER_CHECK` is set
///   and `cargo test --release` runs this test too.
/// * A MISS locks, drops, reads the manifest through
///   `jar_entry_bytes_cached` — which takes two more locks — and then locks
///   again to publish. `jar_path_mtime` takes the `Scratch` mtime memo
///   before any of that. `Scratch` is the floor, so a thread holding it may
///   take nothing else *including another `Scratch`*, and `parking_lot` is
///   not reentrant: a guard left alive in an `if let` scrutinee is a HANG
///   rather than a failure. Miss, hit, revalidate, hit — the whole cycle,
///   under the checker.
#[test]
fn sections_lock_is_order_checked_and_not_held_across_the_read() {
    #[cfg(debug_assertions)]
    assert!(
        cratonvm_types::lock_order::enforcement_active(),
        "debug builds must enforce lock order, or this module's ordering \
         claim is never checked by anything"
    );

    let jar = write_jar(
        "lockorder",
        Some("Manifest-Version: 1.0\r\n\r\nName: p/one.txt\r\nK: V\r\n\r\n"),
    );
    let path = jar.to_str().unwrap().to_string();
    let miss = jar_manifest_sections_cached(&path).expect("miss");
    let hit = jar_manifest_sections_cached(&path).expect("hit");
    assert_eq!(miss.len(), hit.len());
    assert!(miss.contains_key("p/one.txt"), "sections were {miss:?}");
    // Re-probe the mtime (the memo lock again, still outside ours) and
    // come back through the cache.
    jar_cache_revalidate(&path);
    let after = jar_manifest_sections_cached(&path).expect("post-revalidate");
    assert_eq!(miss.len(), after.len());
}
