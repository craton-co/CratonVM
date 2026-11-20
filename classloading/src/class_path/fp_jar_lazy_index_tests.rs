use super::*;
use std::io::Write;

fn write_jar(path: &Path, entries: &[(&str, &[u8], bool)]) {
    let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, data, deflate) in entries {
        let method = if *deflate {
            zip::CompressionMethod::Deflated
        } else {
            zip::CompressionMethod::Stored
        };
        let opts = zip::write::SimpleFileOptions::default().compression_method(method);
        z.start_file(*name, opts).unwrap();
        z.write_all(data).unwrap();
    }
    z.finish().unwrap();
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cratonvm_fpjar_{}_{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The entries a jar opens as, with the index on or off.
fn open(path: &Path, lazy: bool) -> Vec<ClassPathEntry> {
    let (data, stamp) = read_archive_for_classpath_stamped(path).unwrap();
    let mut entries = Vec::new();
    ClassPath::load_jar_data_at_depth_with(
        path,
        data,
        stamp,
        &mut entries,
        99,
        true,
        false,
        lazy,
    );
    entries
}

fn the_jar(
    entries: &[ClassPathEntry],
) -> (&JarArchive, &ArchiveBacking, &FxHashSet<String>, bool) {
    match entries
        .iter()
        .find(|e| matches!(e, ClassPathEntry::JarFile { .. }))
    {
        Some(ClassPathEntry::JarFile {
            archive,
            backing,
            entry_index,
            multi_release,
            ..
        }) => (archive, backing, entry_index, *multi_release),
        _ => panic!("no JarFile entry in {entries:?}"),
    }
}

fn big(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 251) as u8).collect()
}

#[test]
fn fp_both_arms_serve_the_same_jar() {
    let dir = temp_dir("same");
    let jar = dir.join("plain.jar");
    let manifest = b"Manifest-Version: 1.0\r\nMulti-Release: true\r\nMain-Class: p.Main\r\n\r\n";
    let compressible = vec![b'a'; 4096];
    let noisy = big(3000);
    write_jar(
        &jar,
        &[
            ("META-INF/MANIFEST.MF", &manifest[..], true),
            ("p/A.class", &compressible, true),
            ("p/B.class", &noisy, false),
            ("p/res.txt", b"hello", false),
            ("META-INF/versions/11/p/A.class", &noisy, true),
            ("META-INF/versions/17/p/A.class", &compressible, false),
            ("META-INF/versions/9/q/Only.class", b"nine", true),
            ("module-info.class", b"mi", false),
            ("dir/", b"", false),
        ],
    );
    let lazy = open(&jar, true);
    let zip = open(&jar, false);
    let (la, lb, lidx, lmr) = the_jar(&lazy);
    let (za, zb, zidx, zmr) = the_jar(&zip);
    assert!(la.central.is_some(), "the index must take this jar");
    assert!(za.central.is_none());
    assert!(
        !la.zip_is_open(),
        "opening the lazy jar must not open the ZipArchive"
    );
    assert_eq!(lidx, zidx, "entry names");
    assert_eq!(lmr, zmr);
    assert!(lmr);
    let mut names: Vec<String> = Vec::new();
    la.each_name(&mut |n| names.push(n.to_string()));
    let mut znames: Vec<String> = Vec::new();
    za.each_name(&mut |n| znames.push(n.to_string()));
    assert_eq!(names, znames, "entry order");
    for name in names
        .iter()
        .map(String::as_str)
        .chain(["p/Missing.class", "", "p/"])
    {
        assert_eq!(la.has_entry(name), za.has_entry(name), "has {name:?}");
        assert_eq!(la.read_vec(name), za.read_vec(name), "vec {name:?}");
        assert_eq!(
            la.read_shared(lb, name).map(|b| b.to_vec()),
            za.read_shared(zb, name).map(|b| b.to_vec()),
            "shared {name:?}"
        );
    }
    assert!(
        !la.zip_is_open(),
        "serving entries must not open the ZipArchive"
    );
    let (lv, zv) = (Mutex::new(None), Mutex::new(None));
    assert_eq!(
        *ClassPath::ensure_versions_cache(la, &lv),
        *ClassPath::ensure_versions_cache(za, &zv)
    );
    for want in ["p/A.class", "p/B.class", "q/Only.class", "p/res.txt"] {
        for max in [8, 9, 11, 17, 25] {
            assert_eq!(
                ClassPath::find_in_multi_release_archive(la, &lv, Some(lidx), want, max),
                ClassPath::find_in_multi_release_archive(za, &zv, Some(zidx), want, max),
                "mr {want} at {max}"
            );
        }
    }
    let (mut lc, mut zc) = (Vec::new(), Vec::new());
    ClassPath::list_archive_classes(la, &mut lc);
    ClassPath::list_archive_classes(za, &mut zc);
    assert_eq!(lc, zc);
    assert!(la.lacks_signer_blocks());
    assert_eq!(la.read_manifest().main_class, za.read_manifest().main_class);
    assert!(!la.zip_is_open());
    // Anything outside the by-name readers opens the ZipArchive on demand
    // and finds the same jar.
    assert_eq!(la.lock().len(), za.lock().len());
    assert!(la.zip_is_open());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fp_a_signed_jar_still_verifies_through_the_zip_archive() {
    let dir = temp_dir("signed");
    let jar = dir.join("signed.jar");
    write_jar(
        &jar,
        &[
            (
                "META-INF/MANIFEST.MF",
                b"Manifest-Version: 1.0\r\n\r\n",
                false,
            ),
            ("META-INF/FOO.SF", b"Signature-Version: 1.0\r\n\r\n", false),
            ("META-INF/FOO.RSA", b"not a real block", false),
            ("p/A.class", b"a", false),
        ],
    );
    let lazy = open(&jar, true);
    let (la, _, _, _) = the_jar(&lazy);
    assert!(la.central.is_some());
    assert!(
        !la.lacks_signer_blocks(),
        "a signer block is there to verify"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fp_a_fat_jar_and_a_jar_the_index_refuses_take_the_old_open() {
    let dir = temp_dir("fallback");
    let fat = dir.join("fat.jar");
    write_jar(
        &fat,
        &[
            (
                "META-INF/MANIFEST.MF",
                b"Manifest-Version: 1.0\r\n\r\n",
                false,
            ),
            ("BOOT-INF/classes/p/A.class", b"a", false),
            ("BOOT-INF/lib/", b"", false),
        ],
    );
    let (lazy, zip) = (open(&fat, true), open(&fat, false));
    assert_eq!(format!("{lazy:?}"), format!("{zip:?}"), "fat jar entries");
    assert!(lazy
        .iter()
        .any(|e| matches!(e, ClassPathEntry::NestedDirectory { .. })));
    for e in &lazy {
        if let ClassPathEntry::JarFile { archive, .. } = e {
            assert!(
                archive.central.is_none(),
                "a fat jar's root is the old open's"
            );
        }
    }
    // A non-ASCII entry name is not modelled: the jar opens the old way.
    let odd = dir.join("odd.jar");
    write_jar(
        &odd,
        &[("p/caf\u{e9}.class", b"a", false), ("p/A.class", b"b", true)],
    );
    let entries = open(&odd, true);
    let (archive, _, idx, _) = the_jar(&entries);
    assert!(archive.central.is_none());
    assert!(idx.contains("p/A.class"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fp_a_class_path_loads_the_jar_without_opening_the_zip_archive() {
    let dir = temp_dir("classpath");
    let jar = dir.join("lib.jar");
    let dep = dir.join("dep.jar");
    write_jar(
        &jar,
        &[
            (
                "META-INF/MANIFEST.MF",
                b"Manifest-Version: 1.0\r\nClass-Path: dep.jar\r\n\r\n",
                false,
            ),
            ("p/A.class", b"\xCA\xFE\xBA\xBEaaaa", true),
        ],
    );
    write_jar(&dep, &[("q/B.class", b"\xCA\xFE\xBA\xBEbbbb", false)]);
    let cp = ClassPath::new(&[jar.to_string_lossy().into_owned()]);
    assert_eq!(
        cp.find_class("p/A").unwrap().as_ref(),
        b"\xCA\xFE\xBA\xBEaaaa"
    );
    assert_eq!(
        cp.find_class("q/B").unwrap().as_ref(),
        b"\xCA\xFE\xBA\xBEbbbb"
    );
    assert!(cp.find_class("p/Missing").is_err());
    let mut jars = 0;
    for entry in &cp.entries {
        if let ClassPathEntry::JarFile { archive, path, .. } = entry {
            jars += 1;
            assert!(archive.central.is_some(), "{}", path.display());
            assert!(!archive.zip_is_open(), "{}", path.display());
        }
    }
    assert_eq!(jars, 2, "the jar and its Class-Path jar");
    let _ = fs::remove_dir_all(&dir);
}

/// The reads the JDK's own `URLClassPath` makes over the VM's class path
/// (jar-loader bytes, code source, resources, the define gate) are
/// answered from the index too.
#[test]
fn fp_the_jar_loader_rows_code_source_and_resources_stay_off_the_zip_archive() {
    let dir = temp_dir("rows");
    let jar = dir.join("rows.jar");
    let data = big(5000);
    write_jar(
        &jar,
        &[
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\n\r\n", true),
            ("p/A.class", b"\xCA\xFE\xBA\xBEaaaa", true),
            ("p/res.txt", &data, true),
            ("p/stored.bin", &data, false),
        ],
    );
    let cp = ClassPath::new(&[jar.to_string_lossy().into_owned()]);
    let zip_open = || {
        cp.entries.iter().any(|e| match e {
            ClassPathEntry::JarFile { archive, .. } => archive.zip_is_open(),
            _ => false,
        })
    };
    let _ = cp.fast_define_ok("p/A");
    assert!(!zip_open(), "the define gate");
    let (url, certs) = cp.find_class_code_source_info("p/A").expect("code source");
    assert!(url.contains("rows.jar") && certs.is_empty());
    assert!(!zip_open(), "code source");
    assert_eq!(cp.find_resource("p/res.txt").as_deref(), Some(&data[..]));
    assert_eq!(cp.find_all_resource_bytes("p/res.txt"), vec![data.clone()]);
    assert_eq!(cp.find_all_resource_urls("p/res.txt").len(), 1);
    assert!(cp.find_resource("p/none.txt").is_none());
    assert!(!zip_open(), "resources");
    for name in ["p/res.txt", "p/stored.bin"] {
        let answer = cp.jar_entry_bytes_matching(
            &jar,
            name,
            crc32fast::hash(&data),
            data.len() as u64,
        );
        match answer {
            cratonvm_native_api::ClassPathJarBytes::Bytes(bytes) => assert_eq!(bytes, data),
            other => panic!("{name}: expected the bytes, got {other:?}"),
        }
    }
    // A wrong CRC-32 or size is the Java body's business, as before.
    assert!(matches!(
        cp.jar_entry_bytes_matching(&jar, "p/res.txt", 1, data.len() as u64),
        cratonvm_native_api::ClassPathJarBytes::Body
    ));
    assert!(matches!(
        cp.jar_entry_bytes_matching(&jar, "p/res.txt", crc32fast::hash(&data), 7),
        cratonvm_native_api::ClassPathJarBytes::Body
    ));
    assert!(!zip_open(), "the jar-loader rows");
    let _ = fs::remove_dir_all(&dir);
}
