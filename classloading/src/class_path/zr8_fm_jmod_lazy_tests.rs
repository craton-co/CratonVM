use super::*;
use std::io::Write;

fn jmod_bytes(build: impl FnOnce(&mut zip::ZipWriter<Cursor<Vec<u8>>>)) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    build(&mut zip);
    let zip_bytes = zip.finish().unwrap().into_inner();
    let mut out = vec![0x4A, 0x4D, 0x01, 0x00];
    out.extend_from_slice(&zip_bytes);
    out
}

fn sample() -> Vec<u8> {
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let stored = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    let class: Vec<u8> = (0..4000u32).map(|i| (i * 7 % 251) as u8).collect();
    jmod_bytes(|z| {
        z.start_file("classes/p/A.class", deflated).unwrap();
        z.write_all(&class).unwrap();
        z.start_file("classes/p/res.txt", stored).unwrap();
        z.write_all(b"stored resource bytes").unwrap();
        z.add_directory("classes/p/dir/", stored).unwrap();
        z.start_file("classes/module-info.class", deflated).unwrap();
        z.write_all(b"\xCA\xFE\xBA\xBEmodule").unwrap();
        z.start_file("lib/libx.so", deflated).unwrap();
        z.write_all(&[0u8; 9000]).unwrap();
        z.start_file("conf/x.properties", stored).unwrap();
        z.write_all(b"k=v\n").unwrap();
    })
}

fn write_tmp(tag: &str, bytes: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cratonvm_zr8fm_{}_{tag}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{tag}.jmod"));
    fs::write(&path, bytes).unwrap();
    path
}

struct Arm {
    names: Vec<String>,
    classes: FxHashSet<String>,
    archive: JmodArchive,
    backing: ArchiveBacking,
}

fn open(path: &Path, lazy: bool) -> Arm {
    match ClassPath::load_jmod_with(path, lazy).expect("opens") {
        ClassPathEntry::JmodFile {
            all_entry_names,
            class_entry_index,
            archive,
            backing,
            ..
        } => Arm {
            names: all_entry_names.iter().map(str::to_string).collect(),
            classes: class_entry_index.iter().map(str::to_string).collect(),
            archive,
            backing,
        },
        other => panic!("expected JmodFile, got {other:?}"),
    }
}

/// Every answer the class path asks a JMOD for, for `name`.
fn answers(arm: &Arm, name: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>, bool) {
    (
        ClassPath::jmod_find_shared(&arm.archive, &arm.backing, name).map(|b| b.to_vec()),
        ClassPath::jmod_find_vec(&arm.archive, &arm.backing, name),
        ClassPath::jmod_has_entry(&arm.archive, name),
    )
}

fn assert_arms_agree(path: &Path, extra_names: &[&str]) -> usize {
    let lazy = open(path, true);
    let zip = open(path, false);
    assert!(
        matches!(lazy.archive, JmodArchive::Central(_)),
        "{}: the central-directory index must take this JMOD",
        path.display()
    );
    assert!(matches!(zip.archive, JmodArchive::Zip(_)));
    assert_eq!(lazy.names, zip.names, "{}: entry names", path.display());
    assert_eq!(lazy.classes, zip.classes, "{}: class index", path.display());
    let mut checked = 0;
    let all = lazy.names.iter().map(String::as_str);
    for name in all.chain(extra_names.iter().copied()) {
        let (l, z) = (answers(&lazy, name), answers(&zip, name));
        assert_eq!(l, z, "{}: {name}", path.display());
        checked += 1;
    }
    checked
}

#[test]
fn zr8_fm_both_arms_serve_the_same_entries() {
    let path = write_tmp("same", &sample());
    let n = assert_arms_agree(
        &path,
        &["classes/p/B.class", "classes/p/dir", "", "classes/", "lib/"],
    );
    assert!(n >= 11);
    // Positive control: the entries do resolve (a vacuous `None == None`
    // agreement would also pass the loop above).
    let lazy = open(&path, true);
    let (shared, vec, has) = answers(&lazy, "classes/p/res.txt");
    assert_eq!(shared.as_deref(), Some(&b"stored resource bytes"[..]));
    assert_eq!(vec.as_deref(), Some(&b"stored resource bytes"[..]));
    assert!(has);
    let class = answers(&lazy, "classes/p/A.class").0;
    assert_eq!(class.map(|b| b.len()), Some(4000));
    let _ = fs::remove_file(&path);
}

/// Damaged payloads: a deflated entry's stream and a stored entry's bytes
/// (whose CRC then fails). Both arms refuse the same reads -- the stored
/// range view has never checked the CRC, the owned read always has.
#[test]
fn zr8_fm_both_arms_refuse_the_same_damaged_entries() {
    let mut bytes = sample();
    let find = |bytes: &[u8], needle: &[u8]| {
        let mut windows = bytes.windows(needle.len());
        windows.position(|w| w == needle).unwrap()
    };
    // The stored resource's payload follows its local header's name.
    let at = find(&bytes, b"stored resource bytes");
    bytes[at] ^= 0x20;
    // The deflated class's payload: just past its local header.
    let name = b"classes/p/A.class";
    let header = find(&bytes, name) - 30;
    let data = header + 30 + name.len() + 4;
    for b in &mut bytes[data..data + 40] {
        *b ^= 0x5A;
    }
    let path = write_tmp("damaged", &bytes);
    assert_arms_agree(&path, &[]);
    let lazy = open(&path, true);
    let (shared, vec, _) = answers(&lazy, "classes/p/res.txt");
    assert_eq!(shared.as_deref(), Some(&b"Stored resource bytes"[..]));
    assert_eq!(vec, None, "the CRC check refuses the owned read");
    assert_eq!(answers(&lazy, "classes/p/A.class").1, None);
    let _ = fs::remove_file(&path);
}

/// The real JDK's JMODs, every entry of every file, both arms. Ignored by
/// default like the other JDK-dependent JMOD tests; run with
/// `--ignored zr8_fm` on a JDK that ships `jmods/`.
#[test]
#[ignore]
fn zr8_fm_every_real_jdk_jmod_serves_the_same_bytes_both_ways() {
    let Ok(home) = cratonvm_types::flags::runtime_var("JAVA_HOME") else {
        eprintln!("JAVA_HOME unset: skipped");
        return;
    };
    let dir = PathBuf::from(home).join("jmods");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("JAVA_HOME/jmods")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jmod"))
        .collect();
    files.sort();
    assert!(!files.is_empty());
    let mut entries = 0;
    for f in &files {
        entries += assert_arms_agree(f, &["classes/no/such/Thing.class"]);
    }
    eprintln!("zr8fm: {} jmods, {entries} entries agree", files.len());
}

/// This process's resident set in KiB: `VmRSS` on Linux, the working set
/// on Windows, `None` elsewhere.
fn resident_kib() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        return line.split_whitespace().nth(1)?.parse().ok();
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct Counters {
            cb: u32,
            page_fault_count: u32,
            peak_working_set_size: usize,
            working_set_size: usize,
            quota_peak_paged_pool_usage: usize,
            quota_paged_pool_usage: usize,
            quota_peak_non_paged_pool_usage: usize,
            quota_non_paged_pool_usage: usize,
            pagefile_usage: usize,
            peak_pagefile_usage: usize,
        }
        extern "system" {
            fn GetCurrentProcess() -> isize;
            fn K32GetProcessMemoryInfo(p: isize, counters: *mut Counters, cb: u32) -> i32;
        }
        let mut c = Counters {
            cb: std::mem::size_of::<Counters>() as u32,
            ..Default::default()
        };
        // SAFETY: a correctly sized PROCESS_MEMORY_COUNTERS for the
        // current-process pseudo handle.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        return (ok != 0).then_some(c.working_set_size as u64 / 1024);
    }
    #[allow(unreachable_code)]
    None
}

/// The resident cost of OPENING the real JDK's 70 JMODs (and serving
/// `java/lang/Object.class`), each arm measured over its own baseline:
/// the row that says what `CRATONVM_JMOD_LAZY_INDEX` buys. Ignored by
/// default; `--ignored zr8_fm_jmod_open_resident --nocapture`.
#[test]
#[ignore]
fn zr8_fm_jmod_open_resident_cost_per_arm() {
    let Ok(home) = cratonvm_types::flags::runtime_var("JAVA_HOME") else {
        eprintln!("JAVA_HOME unset: skipped");
        return;
    };
    let mut files: Vec<PathBuf> = fs::read_dir(PathBuf::from(home).join("jmods"))
        .expect("JAVA_HOME/jmods")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jmod"))
        .collect();
    files.sort();
    for lazy in [true, false] {
        let before = resident_kib();
        let arms: Vec<Arm> = files.iter().map(|f| open(f, lazy)).collect();
        let base = arms
            .iter()
            .zip(&files)
            .find(|(_, f)| f.file_name().is_some_and(|n| n == "java.base.jmod"))
            .map(|(a, _)| a)
            .expect("java.base.jmod");
        let object = ClassPath::jmod_find_shared(
            &base.archive,
            &base.backing,
            "classes/java/lang/Object.class",
        );
        assert!(object.is_some(), "positive control: Object.class serves");
        let after = resident_kib();
        let delta = match (before, after) {
            (Some(b), Some(a)) => a as i64 - b as i64,
            _ => -1,
        };
        eprintln!(
            "zr8fm-jmod-open lazy={} jmods={} resident_before_kib={} resident_after_kib={} delta_kib={delta}",
            u8::from(lazy),
            arms.len(),
            before.map_or(-1, |v| v as i64),
            after.map_or(-1, |v| v as i64),
        );
        drop(arms);
    }
}
