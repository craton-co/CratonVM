use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::io::Write;
use tempfile::NamedTempFile;
use zip::write::SimpleFileOptions;

fn make_test_jar() -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    let file = tmp.reopen().unwrap();
    let mut zw = zip::ZipWriter::new(file);
    let opts =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zw.start_file("META-INF/MANIFEST.MF", opts).unwrap();
    zw.write_all(b"Manifest-Version: 1.0\r\n\r\n").unwrap();
    zw.start_file("hello.txt", opts).unwrap();
    zw.write_all(b"hello jar world").unwrap();
    zw.finish().unwrap();
    tmp
}

#[test]
#[ignore]
fn diag_bench_open_all_module_jars() {
    // Diagnostic-only, not a real regression test: point
    // CRATONVM_DIAG_JAR_LIST at a newline/semicolon-separated classpath
    // file (e.g. spring-boot-jetty's cratonvm-test-cp.txt) and time how
    // long raw `zip::ZipArchive::new` open + central-directory parse
    // takes across every real jar, to isolate whether that's the TLD-scan
    // slowdown bottleneck independent of the VM/interpreter.
    let list_path = crate::io_flags()
        .diag_jar_list
        .clone()
        .expect("set CRATONVM_DIAG_JAR_LIST to a classpath file path");
    let contents = std::fs::read_to_string(&list_path).unwrap();
    let paths: Vec<&str> = contents
        .split(|c| c == ';' || c == '\n' || c == '\r')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && s.to_ascii_lowercase().ends_with(".jar"))
        .collect();
    eprintln!("diag: {} jar paths", paths.len());

    let start = std::time::Instant::now();
    let mut opened = 0usize;
    let mut failed = 0usize;
    let mut per_jar: Vec<(std::time::Duration, &str)> = Vec::new();
    for p in &paths {
        let t0 = std::time::Instant::now();
        match File::open(p) {
            Ok(f) => match zip::ZipArchive::new(f) {
                Ok(_archive) => {
                    opened += 1;
                    per_jar.push((t0.elapsed(), p));
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("diag: zip parse failed for {p}: {e}");
                }
            },
            Err(e) => {
                failed += 1;
                eprintln!("diag: open failed for {p}: {e}");
            }
        }
    }
    let total = start.elapsed();
    per_jar.sort_by(|a, b| b.0.cmp(&a.0));
    eprintln!(
        "diag: opened={opened} failed={failed} total={total:?} avg={:?}",
        total.checked_div(opened.max(1) as u32).unwrap_or_default()
    );
    eprintln!("diag: top 15 slowest opens:");
    for (dur, p) in per_jar.iter().take(15) {
        eprintln!("diag:   {dur:?}  {p}");
    }

    // Second pass: re-open the SAME jars again (simulating a second
    // TldScanner pass / second server start in the same process) to see
    // whether repeat opens are cheaper (OS file-cache warm) or the same
    // cost every time.
    let start2 = std::time::Instant::now();
    for p in &paths {
        if let Ok(f) = File::open(p) {
            let _ = zip::ZipArchive::new(f);
        }
    }
    eprintln!(
        "diag: second pass (warm cache) total={:?}",
        start2.elapsed()
    );
}

#[test]
fn open_jar_and_list_entries() {
    let tmp = make_test_jar();
    let archive = zip::ZipArchive::new(File::open(tmp.path()).unwrap()).unwrap();
    // 2 entries expected (manifest + hello.txt)
    assert_eq!(archive.len(), 2);
}

#[test]
fn read_manifest_content_from_zip() {
    let tmp = make_test_jar();
    let mut archive = zip::ZipArchive::new(File::open(tmp.path()).unwrap()).unwrap();
    let mut mf = archive.by_name("META-INF/MANIFEST.MF").unwrap();
    let mut s = String::new();
    mf.read_to_string(&mut s).unwrap();
    assert!(s.starts_with("Manifest-Version: 1.0"));
}

#[test]
fn handle_allocator_is_monotonic() {
    let h1 = next_handle();
    let h2 = next_handle();
    assert!(h2 > h1);
}

#[test]
fn cursor_readback_preserves_bytes() {
    let data = b"test-bytes";
    let mut c = Cursor::new(data.to_vec());
    let mut out = Vec::new();
    c.read_to_end(&mut out).unwrap();
    assert_eq!(&out, data);
}

/// A JDK-written entry carries two *different* 0x5455 payloads: 13 bytes
/// with all three times in the local header, 5 bytes with the modified
/// time alone in the central directory — and both stamp the same flags
/// byte 0x07. `zip_entry_times` reads central records, so it must answer
/// modified-only, matching what HotSpot's `ZipFile` reports. Feeding it
/// the local payload instead is what used to make `getLastAccessTime()`
/// and `getCreationTime()` non-`null` where HotSpot answers `null`.
///
/// Byte layout below is verbatim from a jar written by Temurin 25.0.3+9's
/// `JarOutputStream` (`5554 0d00 07 …` local, `5554 0500 07 …` central).
#[test]
fn central_extended_timestamp_carries_modified_time_only() {
    fn extended_timestamp(payload: &[u8]) -> ZipEntryTimes {
        let mut extra = vec![0x55, 0x54];
        extra.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        extra.extend_from_slice(payload);
        zip_entry_times(&extra, None)
    }

    // Central directory: flags 0x07, but only the modified time follows.
    let mut central = vec![0x07];
    central.extend_from_slice(&1_700_000_001u32.to_le_bytes());
    let times = extended_timestamp(&central);
    assert_eq!(times.modified, Some(1_700_000_001_000));
    assert_eq!(times.access, None, "central record carries no access time");
    assert_eq!(
        times.creation, None,
        "central record carries no creation time"
    );

    // The same parser still reads all three from a full 13-byte payload,
    // which is what a central record looks like when a writer does emit
    // every time there. Nothing in our read path feeds it local bytes.
    let mut full = vec![0x07];
    for seconds in [1_700_000_001u32, 1_700_000_002, 1_700_000_003] {
        full.extend_from_slice(&seconds.to_le_bytes());
    }
    let times = extended_timestamp(&full);
    assert_eq!(times.modified, Some(1_700_000_001_000));
    assert_eq!(times.access, Some(1_700_000_002_000));
    assert_eq!(times.creation, Some(1_700_000_003_000));
}

#[test]
fn central_timestamp_metadata_preserves_all_present_times_and_raw_bytes() {
    use std::io::Write;
    let mut payload = vec![7];
    for seconds in [1_700_000_001u32, 1_700_000_002, 1_700_000_003] {
        payload.extend_from_slice(&seconds.to_le_bytes());
    }
    let mut options = zip::write::FullFileOptions::default();
    options.add_extra_field(0xfafa, &payload, true).unwrap();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file("x", options).unwrap();
    writer.write_all(b"x").unwrap();
    let mut bytes = writer.finish().unwrap().into_inner();
    let offset = bytes
        .windows(4)
        .position(|b| b == [0xfa, 0xfa, 13, 0])
        .unwrap();
    bytes[offset..offset + 2].copy_from_slice(&0x5455u16.to_le_bytes());
    let mut source = Cursor::new(bytes.clone());
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let entry = archive.by_index(0).unwrap();
    let (extra, times) = zip_entry_extra_metadata(&entry, &mut source).unwrap();
    assert_eq!(times.access, Some(1_700_000_002_000));
    assert_eq!(times.creation, Some(1_700_000_003_000));
    let mut expected = vec![0x55, 0x54, 13, 0];
    expected.extend(payload);
    assert_eq!(extra, expected);
}

#[test]
fn extra_timestamps_honor_flags_signed_seconds_and_truncated_optional_fields() {
    let extra = [0x55, 0x54, 5, 0, 2, 0xff, 0xff, 0xff, 0xff];
    let times = zip_entry_times(&extra, Some(42));
    assert_eq!(times.modified, None);
    assert_eq!(times.access, Some(-1_000));
    assert_eq!(times.creation, None);
    assert_eq!(times.dos_time, Some(42));
    let times = zip_entry_times(&extra[..8], Some(42));
    assert_eq!(times.access, None);
    assert_eq!(times.dos_time, Some(42));
}

/// Regression coverage for `native_jarfile_get_comment`'s data source.
/// The zip's central-directory comment must round-trip through
/// `ZipArchive::comment()` exactly as written — this is the extraction
/// step `native_jarfile_get_comment` relies on before wrapping it as a
/// Java `String`.
#[test]
fn zip_comment_round_trips() {
    let tmp = NamedTempFile::new().unwrap();
    let file = tmp.reopen().unwrap();
    let mut zw = zip::ZipWriter::new(file);
    zw.set_comment("hello from a zip comment").unwrap();
    let opts = SimpleFileOptions::default();
    zw.start_file("a.txt", opts).unwrap();
    zw.write_all(b"x").unwrap();
    zw.finish().unwrap();

    let archive = zip::ZipArchive::new(File::open(tmp.path()).unwrap()).unwrap();
    assert_eq!(archive.comment(), b"hello from a zip comment");
}

#[test]
fn zip_with_no_comment_has_empty_comment() {
    let tmp = make_test_jar();
    let archive = zip::ZipArchive::new(File::open(tmp.path()).unwrap()).unwrap();
    assert!(archive.comment().is_empty());
}

// -----------------------------------------------------------------------
// gc-common w17-c: per-VM keys, weak owners, the sweep and the teardown
// (`common-w16c-zip-archive-identity-tables-keyed-by-bare-hash`).
// -----------------------------------------------------------------------

/// A zip with `n` entries, so two archives answer `size()` differently.
fn make_jar_with_entries(n: usize) -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    let file = tmp.reopen().unwrap();
    let mut zw = zip::ZipWriter::new(file);
    for i in 0..n {
        zw.start_file(format!("e{i}.txt"), SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"x").unwrap();
    }
    zw.finish().unwrap();
    tmp
}

/// Open `jar` on a fresh four-slot receiver of `ctx`, then make the
/// receiver look like a plain `java.util.zip.ZipFile` (no usable handle
/// slot), so `get_jar_handle` has to go through the identity table.
fn open_plain_zip_file(
    ctx: &mut crate::test_support::MockNativeContext,
    jar: &NamedTempFile,
) -> ObjectRef {
    let this = ctx.alloc_object(4);
    open_and_register(ctx, this, jar.path().to_str().expect("utf-8 temp path"))
        .expect("the archive opens");
    ctx.set_field_by_name(this, "jzfile", Value::Long(0));
    ctx.set_field(this, 1, Value::Object(None));
    this
}

fn size_of(ctx: &mut crate::test_support::MockNativeContext, this: ObjectRef) -> Value {
    native_jarfile_size(ctx, &[Value::Object(Some(this))])
        .expect("size answers")
        .expect("size returns a value")
}

/// Two VMs whose plain `ZipFile`s draw the SAME identity hash each read
/// their own archive, and one VM's `close()` leaves the other's row alone.
/// The tables used to be keyed by the bare hash: the second open replaced
/// the first VM's row, so VM A read VM B's entries.
#[test]
fn w17c_two_vms_with_equal_identity_hashes_keep_their_own_archives() {
    let _g = crate::test_support::confine_test_lock().lock();
    let (jar_a, jar_b) = (make_jar_with_entries(2), make_jar_with_entries(3));
    let mut a = crate::test_support::MockNativeContext::new();
    let mut b = crate::test_support::MockNativeContext::new();
    let zip_a = open_plain_zip_file(&mut a, &jar_a);
    let zip_b = open_plain_zip_file(&mut b, &jar_b);
    let hash = a.identity_hash_code(zip_a);
    assert_eq!(
        hash,
        b.identity_hash_code(zip_b),
        "the collision under test"
    );
    assert_ne!(a.vm_identity(), b.vm_identity());

    assert_eq!(
        size_of(&mut a, zip_a),
        Value::Int(2),
        "A reads its own archive"
    );
    assert_eq!(
        size_of(&mut b, zip_b),
        Value::Int(3),
        "B reads its own archive"
    );
    let (addr_a, addr_b) = (zip_a.as_ptr() as usize, zip_b.as_ptr() as usize);
    assert!(identity_is_known_archive_in(a.vm_identity(), hash, addr_a));
    assert!(identity_is_known_archive_in(b.vm_identity(), hash, addr_b));

    native_jarfile_close(&mut b, &[Value::Object(Some(zip_b))]).expect("close");
    assert_eq!(size_of(&mut b, zip_b), Value::Int(0), "B's handle is gone");
    assert_eq!(
        size_of(&mut a, zip_a),
        Value::Int(2),
        "A's row survived B's close"
    );
    assert!(
        identity_is_known_archive_in(b.vm_identity(), hash, addr_b),
        "a closed archive is still a real one"
    );

    assert!(forget_vm_jar_rows(a.vm_identity()) >= 3);
    assert!(!identity_is_known_archive_in(a.vm_identity(), hash, addr_a));
    assert_eq!(
        size_of(&mut a, zip_a),
        Value::Int(0),
        "teardown released A's archive"
    );
    forget_vm_jar_rows(b.vm_identity());
    assert!(!identity_is_known_archive_in(b.vm_identity(), hash, addr_b));
}

/// The sweep: a moved receiver's rows follow it, a dead unclosed
/// receiver's rows go and its archive is released (and its path pruned
/// from the shared cache), and another VM's rows are never judged.
#[test]
fn w17c_sweep_follows_a_moved_archive_and_releases_a_dead_one() {
    let _g = crate::test_support::confine_test_lock().lock();
    let (jar_moved, jar_dead, jar_foreign) = (
        make_jar_with_entries(1),
        make_jar_with_entries(2),
        make_jar_with_entries(3),
    );
    let mut ctx = crate::test_support::MockNativeContext::new();
    let mut other = crate::test_support::MockNativeContext::new();
    let moved = open_plain_zip_file(&mut ctx, &jar_moved);
    let dead = open_plain_zip_file(&mut ctx, &jar_dead);
    let foreign = open_plain_zip_file(&mut other, &jar_foreign);
    let vm = ctx.vm_identity();
    let moved_handle = get_jar_handle(&mut ctx, moved);
    let dead_handle = get_jar_handle(&mut ctx, dead);
    let foreign_handle = get_jar_handle(&mut other, foreign);
    assert!(moved_handle != 0 && dead_handle != 0 && foreign_handle != 0);
    let dead_path = jar_table()
        .lock()
        .get(&dead_handle)
        .map(|state| state.path.clone())
        .expect("the dead receiver's state");
    let owner_of = |handle: i64| jar_table().lock().get(&handle).map(|s| s.owner);

    // A real move keeps the identity hash (the mock's is the low 32 bits
    // of the address); the rows are re-filed at the new address
    // (gc-common w29-c).
    let to = moved.as_ptr() as usize + (1usize << 32);
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(moved.as_ptr() as usize, to);
    assert_eq!(
        gc_sweep_jar_rows(vm, &map, &|_| false),
        3,
        "the dead receiver's identity row, known-archive row and JarState"
    );
    assert_eq!(owner_of(moved_handle), Some(to), "re-addressed, kept");
    assert_eq!(owner_of(dead_handle), None, "released");
    assert!(
        shared_archive_cache().lock().get(&dead_path).is_none(),
        "the released archive's path is pruned"
    );
    assert_eq!(
        owner_of(foreign_handle),
        Some(foreign.as_ptr() as usize),
        "another VM's row is untouched"
    );
    let dead_key = crate::io_side_row_key(&ctx, dead);
    assert!(!identity_is_known_archive_in(
        dead_key.0, dead_key.1, dead_key.2
    ));
    assert!(!identity_handle_table().lock().contains_key(&dead_key));
    let moved_key = crate::io_row_key(crate::io_side_key(&ctx, moved), to);
    assert_eq!(
        identity_handle_table()
            .lock()
            .get(&moved_key)
            .map(|r| r.owner),
        Some(to)
    );
    assert!(identity_is_known_archive_in(vm, moved_key.1, to));
    assert!(
        !identity_is_known_archive_in(vm, moved_key.1, moved.as_ptr() as usize),
        "re-filed, not copied"
    );

    // The next collection finds the moved receiver alive where it now is.
    let none = cratonvm_types::PointerMap::new();
    assert_eq!(gc_sweep_jar_rows(vm, &none, &|addr| addr == to), 0);
    assert_eq!(owner_of(moved_handle), Some(to));

    assert_eq!(
        forget_vm_jar_rows(vm),
        3,
        "teardown drops the survivor's rows"
    );
    assert_eq!(owner_of(moved_handle), None);
    assert_eq!(forget_vm_jar_rows(other.vm_identity()), 3);
}
