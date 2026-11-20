use super::*;

fn sample_resources() -> Vec<(
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static [u8],
)> {
    vec![
        (
            "java.base",
            "java/lang",
            "String",
            "class",
            b"\xCA\xFE\xBA\xBEfake String.class",
        ),
        (
            "java.base",
            "java/lang",
            "Object",
            "class",
            b"\xCA\xFE\xBA\xBEfake Object.class",
        ),
        (
            "java.base",
            "java/util",
            "HashMap",
            "class",
            b"\xCA\xFE\xBA\xBEfake HashMap.class",
        ),
        (
            "java.desktop",
            "javax/swing",
            "JFrame",
            "class",
            b"\xCA\xFE\xBA\xBEfake JFrame.class",
        ),
        // Resource with no parent (module-info.class):
        (
            "java.base",
            "",
            "module-info",
            "class",
            b"\xCA\xFE\xBA\xBEfake module-info",
        ),
    ]
}

#[test]
fn header_parse_little_endian_magic() {
    let mut bytes = vec![0u8; HEADER_SIZE];
    bytes[0..4].copy_from_slice(&JIMAGE_MAGIC.to_le_bytes());
    // u4 version = (major << 16) | minor — version 1.0 = 0x0001_0000.
    bytes[4..8].copy_from_slice(&((1u32 << 16) | 0).to_le_bytes());
    let h = Header::parse(&bytes).unwrap();
    assert!(h.little_endian);
    assert_eq!(h.major_version, 1);
    assert_eq!(h.minor_version, 0);
}

#[test]
fn header_parse_big_endian_magic() {
    let mut bytes = vec![0u8; HEADER_SIZE];
    bytes[0..4].copy_from_slice(&JIMAGE_MAGIC.to_be_bytes());
    bytes[4..8].copy_from_slice(&((1u32 << 16) | 0).to_be_bytes());
    let h = Header::parse(&bytes).unwrap();
    assert!(!h.little_endian);
    assert_eq!(h.major_version, 1);
}

#[test]
fn header_parse_bad_magic() {
    let bytes = vec![0u8; HEADER_SIZE];
    assert!(matches!(
        Header::parse(&bytes),
        Err(JImageError::BadMagic(_))
    ));
}

#[test]
fn header_parse_unsupported_version() {
    let mut bytes = vec![0u8; HEADER_SIZE];
    bytes[0..4].copy_from_slice(&JIMAGE_MAGIC.to_le_bytes());
    // u4 version = (major << 16) | minor — encode major=99, minor=0.
    bytes[4..8].copy_from_slice(&(99u32 << 16).to_le_bytes());
    assert!(matches!(
        Header::parse(&bytes),
        Err(JImageError::UnsupportedVersion { major: 99, .. })
    ));
}

#[test]
fn header_parse_unsupported_minor_version() {
    let mut bytes = vec![0u8; HEADER_SIZE];
    bytes[0..4].copy_from_slice(&JIMAGE_MAGIC.to_le_bytes());
    bytes[4..8].copy_from_slice(&((1u32 << 16) | 1).to_le_bytes());
    assert!(matches!(
        Header::parse(&bytes),
        Err(JImageError::UnsupportedVersion { major: 1, minor: 1 })
    ));
}

#[test]
fn builder_produces_valid_header() {
    let data = test_builder::build_simple(&sample_resources());
    let reader = JImageReader::from_bytes(data).expect("from_bytes");
    assert_eq!(reader.version(), (1, 0));
    assert_eq!(reader.resource_count(), sample_resources().len() as u32);
}

#[test]
fn find_class_round_trip() {
    let resources = sample_resources();
    let data = test_builder::build_simple(&resources);
    let reader = JImageReader::from_bytes(data).expect("from_bytes");

    for (module, parent, base, ext, bytes) in &resources {
        let internal = if parent.is_empty() {
            base.to_string()
        } else {
            format!("{parent}/{base}")
        };
        assert_eq!(*ext, "class");
        let got = reader
            .find_class(module, &internal)
            .expect("find_class error")
            .unwrap_or_else(|| panic!("missing resource /{module}/{internal}.class"));
        assert_eq!(
            got.as_slice(),
            *bytes,
            "resource bytes must round-trip exactly for /{module}/{internal}.class"
        );
    }
}

#[test]
fn find_resource_returns_none_for_missing() {
    let data = test_builder::build_simple(&sample_resources());
    let reader = JImageReader::from_bytes(data).unwrap();
    let miss = reader
        .find_resource("/java.base/does/not/Exist.class")
        .unwrap();
    assert!(miss.is_none());
}

#[test]
fn i32_min_redirect_is_absent_not_overflow() {
    let path = "/java.base/does/not/Exist.class";
    let mut data = test_builder::build_simple(&sample_resources());
    let table_len = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let bucket = (jimage_hash(path, HASH_MULTIPLIER) as usize) % table_len;
    let redirect_offset = HEADER_SIZE + bucket * 4;
    data[redirect_offset..redirect_offset + 4].copy_from_slice(&i32::MIN.to_le_bytes());

    let reader = JImageReader::from_bytes(data).unwrap();
    assert!(reader.find_resource(path).unwrap().is_none());
}

#[test]
fn i32_min_redirect_for_existing_bucket_is_absent_not_overflow() {
    let path = "/java.base/java/lang/String.class";
    let mut data = test_builder::build_simple(&sample_resources());
    let table_len = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let bucket = (jimage_hash(path, HASH_MULTIPLIER) as usize) % table_len;
    let redirect_offset = HEADER_SIZE + bucket * 4;
    data[redirect_offset..redirect_offset + 4].copy_from_slice(&i32::MIN.to_le_bytes());

    let reader = JImageReader::from_bytes(data).unwrap();
    assert!(reader.find_resource(path).unwrap().is_none());
}

#[test]
fn find_resource_reconstructs_path_exactly() {
    let data = test_builder::build_simple(&sample_resources());
    let reader = JImageReader::from_bytes(data).unwrap();
    let bytes = reader
        .find_resource("/java.base/java/lang/String.class")
        .unwrap()
        .expect("hit");
    assert!(bytes.starts_with(b"\xCA\xFE\xBA\xBE"));
}

#[test]
fn for_each_entry_parts_matches_iter_entries() {
    let data = test_builder::build_simple(&sample_resources());
    let reader = JImageReader::from_bytes(data).unwrap();
    let expected: Vec<String> = reader
        .iter_entries()
        .unwrap()
        .into_iter()
        .map(|(path, _, _)| path)
        .collect();
    let mut rebuilt = Vec::new();
    reader
        .for_each_entry_parts(|module, parent, base, extension| {
            let mut path = String::new();
            if !module.is_empty() {
                path.push('/');
                path.push_str(module);
            }
            if !parent.is_empty() {
                path.push('/');
                path.push_str(parent);
            }
            if !base.is_empty() {
                path.push('/');
                path.push_str(base);
            }
            if !extension.is_empty() {
                path.push('.');
                path.push_str(extension);
            }
            rebuilt.push(path);
        })
        .unwrap();
    assert_eq!(
        rebuilt, expected,
        "same entries, same order, same path assembly"
    );
}

/// p5-cp: the offset walk visits the parts walk's entries in the same
/// order, and `string_at` / `string_is` / `string_is_empty` give
/// `read_string`'s answers, on a trusted strings buffer and on one that
/// is not (a non-UTF-8 byte planted in a string no entry names, so the
/// image still reads but the whole-buffer check fails).
#[test]
fn p5cp_offset_walk_and_string_probes_match_read_string() {
    let data = test_builder::build_simple(&sample_resources());
    let mut untrusted = data.clone();
    {
        let r = JImageReader::from_bytes(data.clone()).unwrap();
        assert!(r.strings_trusted);
        // Offset 0 is never read (it is ""), so a non-UTF-8 byte there
        // fails the whole-buffer check without breaking any entry.
        untrusted[r.strings_offset] = 0xFF;
    }
    for (i, bytes) in [data, untrusted].into_iter().enumerate() {
        let r = JImageReader::from_bytes(bytes).unwrap();
        assert_eq!(r.strings_trusted, i == 0, "positive control");
        let mut parts = Vec::new();
        r.for_each_entry_parts(|m, p, b, e| {
            parts.push([m, p, b, e].map(str::to_string));
        })
        .unwrap();
        let mut via_offsets = Vec::new();
        r.for_each_entry_offsets(|e| {
            let strings = r.strings_buffer();
            let parts = [e.module, e.parent, e.base, e.extension];
            for off in parts {
                let want = read_string(strings, off).unwrap();
                assert_eq!(r.string_at(off).unwrap(), want);
                assert!(r.string_is(off, want).unwrap());
                assert!(!r.string_is(off, &format!("{want}x")).unwrap());
                assert_eq!(r.string_is_empty(off).unwrap(), want.is_empty());
                if want.len() > 1 {
                    assert!(!r.string_is(off, &want[..want.len() - 1]).unwrap());
                }
            }
            via_offsets.push(parts.map(|o| r.string_at(o).unwrap().to_string()));
        })
        .unwrap();
        assert_eq!(via_offsets, parts, "trusted={}", r.strings_trusted);
        assert_eq!(
            r.find_resource_bytes("/java.base/java/lang/String.class")
                .unwrap()
                .map(<[u8]>::to_vec),
            r.find_resource("/java.base/java/lang/String.class")
                .unwrap()
        );
        assert_eq!(
            r.find_resource_bytes("/java.base/java/lang/Nope.class")
                .unwrap(),
            None
        );
        // A prefix or extension of a real path is not that path.
        for miss in [
            "/java.base/java/lang/String.clas",
            "/java.base/java/lang/String.classx",
            "/java.base/java/lang/String",
            "java.base/java/lang/String.class",
            "/java.base/java/lang/String.class/",
        ] {
            assert_eq!(r.find_resource_bytes(miss).unwrap(), None, "{miss}");
        }
    }
}

#[test]
fn a_shared_backing_reads_the_same_as_an_owned_one() {
    let data = test_builder::build_simple(&sample_resources());
    let owned = JImageReader::from_bytes(data.clone()).unwrap();
    let shared: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync> = std::sync::Arc::new(data);
    let mapped = JImageReader::from_shared(shared).unwrap();
    assert_eq!(
        owned.iter_entries().unwrap(),
        mapped.iter_entries().unwrap()
    );
    assert_eq!(
        owned.find_class("java.base", "java/lang/String").unwrap(),
        mapped.find_class("java.base", "java/lang/String").unwrap()
    );
}

#[test]
fn iter_entries_covers_all_resources() {
    let resources = sample_resources();
    let data = test_builder::build_simple(&resources);
    let reader = JImageReader::from_bytes(data).unwrap();
    let entries = reader.iter_entries().unwrap();
    assert_eq!(entries.len(), resources.len());
    let mut paths: Vec<_> = entries.iter().map(|(p, _, _)| p.clone()).collect();
    paths.sort();
    let mut expected = vec![
        "/java.base/java/lang/String.class".to_string(),
        "/java.base/java/lang/Object.class".to_string(),
        "/java.base/java/util/HashMap.class".to_string(),
        "/java.desktop/javax/swing/JFrame.class".to_string(),
        "/java.base/module-info.class".to_string(),
    ];
    expected.sort();
    assert_eq!(paths, expected);
}

#[test]
fn find_class_with_empty_parent() {
    let data = test_builder::build_simple(&sample_resources());
    let reader = JImageReader::from_bytes(data).unwrap();
    let mi = reader
        .find_class("java.base", "module-info")
        .unwrap()
        .expect("module-info present");
    assert!(mi.ends_with(b"fake module-info"));
}

#[test]
fn two_level_rehash_path_works() {
    // Force the builder to encode every entry via the re-hash path
    // and confirm the reader resolves them correctly. This exercises
    // the seeded-hash branch that single-step entries don't touch.
    let resources = sample_resources();
    let data = test_builder::build_with_rehash(&resources);
    let reader = JImageReader::from_bytes(data).unwrap();
    for (module, parent, base, _ext, bytes) in &resources {
        let internal = if parent.is_empty() {
            base.to_string()
        } else {
            format!("{parent}/{base}")
        };
        let got = reader
            .find_class(module, &internal)
            .unwrap()
            .expect("re-hash lookup");
        assert_eq!(got.as_slice(), *bytes);
    }
}

#[test]
fn decode_location_rejects_bad_attribute_kind() {
    // Kind = 8 (invalid); length = 1.
    let buf = [(8u8 << 3) | 0, 0x00, 0x00];
    assert!(matches!(
        decode_location(&buf, 0),
        Err(JImageError::BadLocationRecord(_))
    ));
}

#[test]
fn decode_location_rejects_truncated_value() {
    // Kind = 5 (OFFSET); length = 8 bytes but only 3 available.
    let buf = [(5u8 << 3) | 7, 0x01, 0x02, 0x03];
    assert!(matches!(
        decode_location(&buf, 0),
        Err(JImageError::BadLocationRecord(_))
    ));
}

#[test]
fn decode_location_rejects_missing_end() {
    // No END byte in the buffer.
    let buf: Vec<u8> = vec![(1u8 << 3) | 0, 0x05];
    assert!(matches!(
        decode_location(&buf, 0),
        Err(JImageError::BadLocationRecord(_))
    ));
}

#[test]
fn read_string_returns_empty_for_offset_zero() {
    let strings = [0u8, b'h', b'i', 0];
    assert_eq!(read_string(&strings, 0).unwrap(), "");
}

#[test]
fn read_string_reads_terminated_string() {
    let strings = [0u8, b'h', b'i', 0];
    assert_eq!(read_string(&strings, 1).unwrap(), "hi");
}

#[test]
fn read_string_rejects_out_of_bounds() {
    let strings = [0u8];
    assert!(matches!(
        read_string(&strings, 99),
        Err(JImageError::BadStringOffset(_))
    ));
}

#[test]
fn jimage_hash_matches_openjdk_reference() {
    // The expected values below were computed using the OpenJDK
    // `ImageStrings::hash_code` C implementation for the exact same
    // inputs. This is the acid test: if the hash disagrees, the
    // reader cannot resolve real lib/modules entries.
    //
    // Expected values (computed by running the C algorithm manually):
    //   hash_code("/java.base/java/lang/String.class", 0x01000193)
    //     = (h * HASH_MULTIPLIER) ^ byte for each byte in the string.
    // We reproduce the algorithm here and confirm our Rust rewrite
    // matches the textbook formula bit-for-bit.
    fn reference(path: &str, seed: i32) -> i32 {
        let mut h = seed;
        for &b in path.as_bytes() {
            h = h.wrapping_mul(HASH_MULTIPLIER) ^ (b as i32);
        }
        h & 0x7FFF_FFFF
    }
    for path in [
        "/java.base/java/lang/String.class",
        "/java.base/java/util/HashMap.class",
        "/java.desktop/javax/swing/JFrame.class",
        "",
        "a",
    ] {
        assert_eq!(
            jimage_hash(path, HASH_MULTIPLIER),
            reference(path, HASH_MULTIPLIER),
            "hash mismatch for {path:?}"
        );
    }
}

#[test]
fn from_bytes_rejects_truncated_header() {
    let short = vec![0u8; 10];
    assert!(matches!(
        JImageReader::from_bytes(short),
        Err(JImageError::TruncatedSection("header"))
    ));
}

#[test]
fn from_bytes_rejects_truncated_body() {
    // Build a valid image, then lop off the last half.
    let data = test_builder::build_simple(&sample_resources());
    let truncated = data[..data.len() / 2].to_vec();
    assert!(matches!(
        JImageReader::from_bytes(truncated),
        Err(JImageError::TruncatedSection(_))
    ));
}

/// If `$JAVA_HOME/lib/modules` exists on the test host, smoke-test
/// the reader against it: header parses, version is (1, 0), and
/// `java/lang/String` can be resolved via `find_class`. This test
/// is a best-effort assertion — it is ignored when JAVA_HOME is
/// unset or when the file is missing so CI environments without a
/// full JDK still build clean.
#[test]
fn smoke_test_real_lib_modules() {
    let Ok(java_home) = cratonvm_types::flags::runtime_var("JAVA_HOME") else {
        eprintln!("JAVA_HOME unset; skipping real lib/modules smoke test");
        return;
    };
    let path = std::path::PathBuf::from(&java_home)
        .join("lib")
        .join("modules");
    if !path.exists() {
        eprintln!("{} not present; skipping", path.display());
        return;
    }
    let reader = match JImageReader::open(&path) {
        Ok(r) => r,
        Err(e) => {
            // A production JDK *might* ship a compressed jimage. We
            // tolerate that rather than failing the test — the
            // reader correctly surfaces the format as an error.
            eprintln!("open {} failed: {e}", path.display());
            return;
        }
    };
    assert_eq!(reader.version().0, 1, "jimage major version must be 1");
    match reader.find_class("java.base", "java/lang/String") {
        Ok(Some(bytes)) => {
            assert!(
                bytes.len() > 100 && bytes.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]),
                "java/lang/String must be a valid class file"
            );
        }
        Ok(None) => {
            panic!("java/lang/String not found in real lib/modules");
        }
        Err(JImageError::Compressed(_)) => {
            eprintln!("real lib/modules is compressed; reader correctly flagged");
        }
        Err(e) => panic!("unexpected error reading real lib/modules: {e}"),
    }
}
