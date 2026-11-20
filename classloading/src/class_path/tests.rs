    use super::*;
    use std::io::Write;

    #[test]
    fn jar_subdirectory_spec_accepts_war_archives() {
        assert_eq!(
            parse_jar_subdir_spec("/apps/test.war!/WEB-INF/classes/"),
            Some(("/apps/test.war".to_string(), "WEB-INF/classes/".to_string()))
        );
    }

    // -----------------------------------------------------------------------
    // Glob parse-once refactor (2026-07-26 classpath-scan audit)
    // -----------------------------------------------------------------------

    /// Hoisting the glob parse out of the per-candidate filter must not change
    /// which names match. Compare the batch API against the per-candidate
    /// predicate it replaced, over globs that exercise `*`, `?`, directory
    /// prefixes, the root, and the reject paths (no wildcard, wildcard in the
    /// prefix, empty pattern, unsafe prefix).
    #[test]
    fn matching_resource_entry_names_agrees_with_per_candidate_predicate() {
        let names: Vec<String> = [
            "java/lang/Object.class",
            "java/lang/String.class",
            "java/lang/invoke/MethodHandle.class",
            "java/util/List.class",
            "Root.class",
            "java/lang/notes.txt",
            "java/lang/",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        for glob in [
            "java/lang/*.class",
            "java/lang/*",
            "java/lang/?bject.class",
            "java/util/*.class",
            "*.class",
            "java/*/List.class",      // wildcard in the prefix → unparseable
            "java/lang/Object.class", // no wildcard → unparseable
            "java/lang/",             // empty pattern → unparseable
            "/java/lang/*",           // unsafe prefix → unparseable
        ] {
            let batch = ClassPath::matching_resource_entry_names(names.iter(), glob);
            let mut expected: Vec<String> = names
                .iter()
                .filter(|n| resource_name_matches_simple_glob(glob, n))
                .cloned()
                .collect();
            expected.sort();
            assert_eq!(batch, expected, "glob {glob:?} changed meaning");
        }
    }

    /// The parsed and unparsed forms must be the same predicate.
    #[test]
    fn parsed_glob_predicate_matches_unparsed_form() {
        let glob = "java/lang/*.class";
        let (prefix, pattern) = simple_resource_glob(glob).expect("glob must parse");
        for candidate in [
            "java/lang/Object.class",
            "java/lang/invoke/MethodHandle.class",
            "java/util/List.class",
            "java/lang/",
            "",
        ] {
            assert_eq!(
                resource_name_matches_parsed_glob(prefix, pattern, candidate),
                resource_name_matches_simple_glob(glob, candidate),
                "candidate {candidate:?} disagreed"
            );
        }
    }

    /// `archive_has_entry` must answer exactly what `find_in_archive` answers,
    /// without paying for the inflate. Uses a Deflated entry so the two paths
    /// genuinely differ in work done.
    #[test]
    fn archive_has_entry_agrees_with_find_in_archive() {
        let dir = std::env::temp_dir().join("cratonvm_archive_has_entry");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let jar_path = dir.join("probe.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("pkg/present.txt", options).unwrap();
            zip.write_all(&b"payload".repeat(512)).unwrap();
            zip.finish().unwrap();
        }
        let backing = read_archive_for_classpath(&jar_path).unwrap();
        let archive: Mutex<SharedArchive> =
            Mutex::new(ZipArchive::new(Cursor::new(backing)).unwrap());

        for name in ["pkg/present.txt", "pkg/absent.txt", "", "pkg/"] {
            assert_eq!(
                ClassPath::archive_has_entry(&archive, name),
                ClassPath::find_in_archive(&archive, name).is_some(),
                "existence verdict for {name:?} diverged"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classpath_new_finds_resource_in_war_subdirectory() {
        let dir = std::env::temp_dir().join("cratonvm_war_subdirectory_classpath");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let war_path = dir.join("test.war");
        {
            let file = fs::File::create(&war_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("WEB-INF/classes/test.txt", options).unwrap();
            zip.write_all(b"test resource").unwrap();
            zip.finish().unwrap();
        }

        let spec = format!("{}!/WEB-INF/classes/", war_path.display());
        let cp = ClassPath::new(&[spec]);
        assert_eq!(
            cp.find_resource("test.txt"),
            Some(b"test resource".to_vec())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classpath_new_expands_manifest_class_path() {
        let dir = std::env::temp_dir().join("cratonvm_manifest_class_path");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dependency = dir.join("dependency.jar");
        let launcher = dir.join("launcher.jar");
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        {
            let file = fs::File::create(&dependency).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("META-INF/services/example.Service", options)
                .unwrap();
            zip.write_all(b"example.ServiceImpl\n").unwrap();
            zip.finish().unwrap();
        }
        {
            let file = fs::File::create(&launcher).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
            zip.write_all(b"Manifest-Version: 1.0\r\nClass-Path: dependency.jar\r\n\r\n")
                .unwrap();
            zip.finish().unwrap();
        }

        let cp = ClassPath::new(&[launcher.to_string_lossy().into_owned()]);
        assert_eq!(cp.entry_count(), 2, "manifest dependency must be added");
        assert_eq!(
            cp.find_resource("META-INF/services/example.Service"),
            Some(b"example.ServiceImpl\n".to_vec())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_class_path_cycle_is_not_reloaded() {
        let dir = std::env::temp_dir().join("cratonvm_manifest_class_path_cycle");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.jar");
        let second = dir.join("second.jar");
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        for (path, dependency) in [(&first, "second.jar"), (&second, "first.jar")] {
            let file = fs::File::create(path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
            zip.write_all(
                format!("Manifest-Version: 1.0\r\nClass-Path: {dependency}\r\n\r\n").as_bytes(),
            )
            .unwrap();
            zip.finish().unwrap();
        }

        let cp = ClassPath::new(&[first.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.entry_count(),
            2,
            "manifest cycle must terminate without duplicates"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_classpath_finds_nothing() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object").is_err());
    }

    #[test]
    fn nonexistent_jar_skipped() {
        let cp = ClassPath::new(&["nonexistent.jar".to_string()]);
        assert!(cp.is_empty());
    }

    #[test]
    fn load_class_from_jar() {
        // Create a temporary JAR file with a fake .class entry
        let dir = std::env::temp_dir().join("cratonvm_test_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("test.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip_writer = zip::ZipWriter::new(file);

        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip_writer
            .start_file("com/example/Hello.class", options)
            .unwrap();
        zip_writer
            .write_all(b"\xCA\xFE\xBA\xBE_fake_class_data")
            .unwrap();
        zip_writer.finish().unwrap();

        // Load from the JAR
        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty());

        let data = cp.find_class("com/example/Hello").unwrap();
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.len() > 4);
        assert!(
            data.is_external(),
            "stored ZIP entries should retain the mapped archive backing"
        );

        // Not found in JAR
        assert!(cp.find_class("com/example/Missing").is_err());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compressed_class_uses_shared_owned_bytes() {
        let dir = std::env::temp_dir().join("cratonvm_test_deflated_jar");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let jar_path = dir.join("test.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip_writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip_writer
                .start_file("com/example/Compressed.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_compressed_class_data")
                .unwrap();
            zip_writer.finish().unwrap();
        }

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let first = cp.find_class("com/example/Compressed").unwrap();
        let second = first.clone();
        assert_eq!(first.as_ref(), second.as_ref());
        assert!(
            !first.is_external() && !second.is_external(),
            "deflated entries should use reference-counted inflated storage"
        );
        assert_eq!(first.as_ref().as_ptr(), second.as_ref().as_ptr());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_format() {
        let cp = ClassPath::new(&[]);
        let debug = format!("{cp:?}");
        assert!(debug.contains("ClassPath"));
    }

    // -- Path traversal protection tests --

    #[test]
    fn path_traversal_double_dot_rejected() {
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("java/lang/../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn path_traversal_absolute_path_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("/etc/passwd").is_err());
    }

    #[test]
    fn path_traversal_backslash_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("\\\\server\\share").is_err());
    }

    #[test]
    fn single_windows_separator_component_rejected_everywhere() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("pkg\\Foo").is_err());
        assert!(cp.find_class_source_path("pkg\\Foo").is_none());
        assert!(cp.find_class_code_source_info("pkg\\Foo").is_none());
        assert!(cp.find_resource("pkg\\config.properties").is_none());
        assert!(cp
            .find_all_resource_bytes("pkg\\config.properties")
            .is_empty());
        assert!(cp
            .find_all_resource_urls("pkg\\config.properties")
            .is_empty());
    }

    #[test]
    fn classpath_metadata_change_detects_length_change() {
        let dir = std::env::temp_dir().join("cratonvm_classpath_mutation_guard");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("guard.jar");

        fs::write(&path, b"before").unwrap();
        let before = fs::metadata(&path).unwrap();
        fs::write(&path, b"after mutation").unwrap();
        let after = fs::metadata(&path).unwrap();

        assert!(classpath_file_metadata_changed(&before, &after));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_traversal_null_byte_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object\0.evil").is_err());
    }

    #[test]
    fn valid_class_name_format_accepted() {
        // Valid names should not be rejected (they'll fail with ClassNotFound
        // because there's no classpath entry, but NOT due to validation)
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("java/lang/Object");
        assert!(
            matches!(result, Err(ClassFileError::ClassNotFound { .. })),
            "expected ClassNotFound, got {result:?}"
        );
    }

    #[test]
    fn find_resource_from_directory() {
        let dir = std::env::temp_dir().join("cratonvm_test_resource_dir");
        let _ = fs::create_dir_all(&dir);
        let resource_path = dir.join("hello.txt");
        fs::write(&resource_path, b"hello world").unwrap();

        let cp = ClassPath::new(&[dir.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("hello.txt")
            .expect("resource should be found");
        assert_eq!(data, b"hello world");

        assert!(cp.find_resource("missing.txt").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_simple_glob_from_directory() {
        let dir = std::env::temp_dir().join("cratonvm_test_resource_dir_glob");
        let _ = fs::remove_dir_all(&dir);
        let web_inf = dir.join("org/springframework/web/context/WEB-INF");
        fs::create_dir_all(&web_inf).unwrap();
        fs::write(web_inf.join("myplaceholder.properties"), b"name=Rod").unwrap();
        fs::write(web_inf.join("myoverride.properties"), b"age=42").unwrap();
        fs::write(web_inf.join("other.properties"), b"ignored=true").unwrap();

        let cp = ClassPath::new(&[dir.to_string_lossy().into_owned()]);
        let pattern = "org/springframework/web/context/WEB-INF/myplace*.properties";
        let data = cp.find_resource(pattern).expect("globbed resource");
        assert_eq!(data, b"name=Rod");

        let bytes = cp.find_all_resource_bytes(pattern);
        assert_eq!(bytes, vec![b"name=Rod".to_vec()]);

        let urls = cp.find_all_resource_urls(pattern);
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0].ends_with("/org/springframework/web/context/WEB-INF/myplaceholder.properties"),
            "unexpected URL for globbed resource: {:?}",
            urls
        );

        assert!(
            cp.find_resource("org/springframework/web/*/WEB-INF/myplace*.properties")
                .is_none(),
            "wildcards in directory segments are intentionally unsupported"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_from_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_resource_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("resources.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip_writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip_writer.start_file("data/words.txt", options).unwrap();
        zip_writer.write_all(b"word1\nword2\nword3").unwrap();
        zip_writer.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_resource("data/words.txt").expect("resource in JAR");
        assert_eq!(data, b"word1\nword2\nword3");

        // Leading slash is stripped
        let data2 = cp
            .find_resource("/data/words.txt")
            .expect("leading slash stripped");
        assert_eq!(data2, b"word1\nword2\nword3");

        assert!(cp.find_resource("data/missing.txt").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    /// The short-circuiting walk must answer EXACTLY what the whole-list walk
    /// answers first — that equivalence is the licence for
    /// `ClassLoader.getResource` to stop at the first hit rather than build
    /// every URL and discard all but element 0.
    ///
    /// Asserted on a classpath where the name hits in MORE than one entry, and
    /// where the first hit is not the last entry: a fixture with a single hit
    /// cannot tell "returns the first" from "returns the only one", and one
    /// where the hit is last cannot tell "stopped early" from "walked
    /// everything".
    #[test]
    fn the_incremental_walk_returns_what_the_whole_list_walk_returns_first() {
        let dir =
            std::env::temp_dir().join(format!("cratonvm_test_first_url_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let first = dir.join("first");
        let second = dir.join("second");
        let empty = dir.join("empty");
        for d in [&first, &second, &empty] {
            fs::create_dir_all(d.join("pkg")).unwrap();
        }
        fs::write(first.join("pkg/R.txt"), b"from-first").unwrap();
        fs::write(second.join("pkg/R.txt"), b"from-second").unwrap();

        // `empty` last so a walk that does not stop early still has an entry
        // left to visit after the answer is known.
        let cp = ClassPath::new(&[
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
            empty.to_string_lossy().into_owned(),
        ]);

        let all = cp.find_all_resource_urls("pkg/R.txt");
        assert_eq!(all.len(), 2, "fixture must hit twice, got {all:?}");
        let (incremental, resume) = cp
            .next_resource_url_from("pkg/R.txt", 0)
            .expect("the incremental walk must find it too");
        assert_eq!(
            Some(&incremental),
            all.first(),
            "the two walks disagree on the FIRST url"
        );
        assert_eq!(resume, 1, "it must stop at the entry that answered");

        // A miss agrees too, and costs nothing to state.
        assert!(cp.find_all_resource_urls("pkg/Absent.txt").is_empty());
        assert!(cp.next_resource_url_from("pkg/Absent.txt", 0).is_none());

        // And the gate: a glob name can match several names inside ONE entry,
        // so it must NOT take the incremental path.
        assert!(!ClassPath::name_supports_incremental_scan("pkg/*.txt"));
        assert!(ClassPath::name_supports_incremental_scan("pkg/R.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_path_traversal_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("../etc/passwd").is_none());
        assert!(cp.find_resource("foo\0bar").is_none());
    }

    // -- Spring Boot fat JAR tests --

    /// Helper: create a Spring Boot fat JAR with:
    /// - META-INF/MANIFEST.MF (with Start-Class)
    /// - BOOT-INF/classes/com/example/App.class
    /// - BOOT-INF/lib/dep.jar (containing org/dep/Util.class)
    /// - org/springframework/boot/loader/JarLauncher.class (launcher at root)
    fn create_fat_jar(jar_path: &Path) {
        let file = fs::File::create(jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // MANIFEST.MF
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(
            b"Manifest-Version: 1.0\r\n\
              Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
              Start-Class: com.example.App\r\n\
              Spring-Boot-Classes: BOOT-INF/classes/\r\n\
              Spring-Boot-Lib: BOOT-INF/lib/\r\n",
        )
        .unwrap();

        // Application class in BOOT-INF/classes/
        zip.start_file("BOOT-INF/classes/com/example/App.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_app_class").unwrap();

        // Resource in BOOT-INF/classes/
        zip.start_file("BOOT-INF/classes/application.properties", opts)
            .unwrap();
        zip.write_all(b"server.port=8080").unwrap();

        // Nested dependency JAR in BOOT-INF/lib/
        let mut dep_jar_buf = Vec::new();
        {
            let cursor = Cursor::new(&mut dep_jar_buf);
            let mut dep_zip = zip::ZipWriter::new(cursor);
            dep_zip.start_file("org/dep/Util.class", opts).unwrap();
            dep_zip.write_all(b"\xCA\xFE\xBA\xBE_util_class").unwrap();
            dep_zip
                .start_file("META-INF/services/org.dep.SPI", opts)
                .unwrap();
            dep_zip.write_all(b"org.dep.SpiImpl").unwrap();
            dep_zip.finish().unwrap();
        }
        zip.start_file("BOOT-INF/lib/dep-1.0.jar", opts).unwrap();
        zip.write_all(&dep_jar_buf).unwrap();

        // Launcher class at JAR root
        zip.start_file("org/springframework/boot/loader/JarLauncher.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_launcher").unwrap();

        zip.finish().unwrap();
    }

    #[test]
    fn fat_jar_detects_spring_boot_structure() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_detect");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);

        // Should have multiple entries: NestedDirectory + NestedJar(s) + outer JarFile
        assert!(
            cp.entry_count() >= 3,
            "expected >=3 entries, got {}",
            cp.entry_count()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_class_from_boot_inf_classes() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_classes");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("com/example/App")
            .expect("should find App.class from BOOT-INF/classes/");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_app_class"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_class_from_nested_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_nested");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("org/dep/Util")
            .expect("should find Util.class from BOOT-INF/lib/dep-1.0.jar");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_util_class"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_launcher_from_root() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_root");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("org/springframework/boot/loader/JarLauncher")
            .expect("should find JarLauncher from JAR root");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_launcher"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_resource_from_boot_inf_classes() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_resource");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("application.properties")
            .expect("should find application.properties from BOOT-INF/classes/");
        assert_eq!(data, b"server.port=8080");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Without emulation (`--real-jdk`'s application class path) a fat JAR
    /// is what HotSpot sees: its root, and nothing under `BOOT-INF/`, neither
    /// as entries of their own nor through the prefixed class lookup.
    #[test]
    fn fat_jar_without_emulation_is_its_root_only() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_no_emulation");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp =
            ClassPath::with_fat_jar_emulation(&[jar_path.to_string_lossy().into_owned()], false);
        assert_eq!(cp.entry_count(), 1, "the outer JAR alone");
        let data = cp
            .find_class("org/springframework/boot/loader/JarLauncher")
            .expect("the root still loads");
        assert!(data.ends_with(b"_launcher"));
        assert!(
            cp.find_class("com/example/App").is_err(),
            "BOOT-INF/classes is Spring's launcher's to load, not the class path's"
        );
        assert!(cp.find_class("org/dep/Util").is_err(), "nor BOOT-INF/lib");
        assert!(cp.find_resource("application.properties").is_none());
        assert!(cp.find_resource("META-INF/services/org.dep.SPI").is_none());
        assert_eq!(
            cp.find_resource("BOOT-INF/classes/application.properties")
                .as_deref(),
            Some(&b"server.port=8080"[..]),
            "the archive's own entries stay readable by their real names"
        );

        let emulated = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(
            emulated.find_class("com/example/App").is_ok(),
            "control: the default class path still emulates"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// `nested_archive_entry_names`: the nested jar's exact central directory
    /// (the walk index of `native-builtins` `classloader::nested_jar_entry_index`),
    /// `None` for an entry that is missing, not an archive, or unsafe.
    #[test]
    fn nested_archive_entry_names_index_one_nested_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_nested_entry_names");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let names = ClassPath::nested_archive_entry_names(&jar_path, "BOOT-INF/lib/dep-1.0.jar")
            .expect("the nested jar is indexed");
        assert!(names.contains("org/dep/Util.class"));
        assert!(names.contains("META-INF/services/org.dep.SPI"));
        assert!(
            !names.contains("com/example/App.class"),
            "the outer jar's entries are not its"
        );
        assert_eq!(names.len(), 2);
        assert!(
            ClassPath::nested_archive_entry_names(&jar_path, "BOOT-INF/lib/none.jar").is_none()
        );
        assert!(
            ClassPath::nested_archive_entry_names(
                &jar_path,
                "BOOT-INF/classes/application.properties"
            )
            .is_none(),
            "not an archive"
        );
        assert!(ClassPath::nested_archive_entry_names(&jar_path, "../x.jar").is_none());
        assert!(ClassPath::nested_archive_entry_names(&dir.join("absent.jar"), "a.jar").is_none());

        // A Multi-Release nested jar: a versioned-only entry is indexed under
        // its unversioned name too (the walk index must never call it absent).
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let mut mr = Vec::new();
        {
            let mut z = zip::ZipWriter::new(Cursor::new(&mut mr));
            z.start_file("META-INF/versions/17/p/V.class", opts)
                .unwrap();
            z.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
            z.start_file("META-INF/versions/x/q.txt", opts).unwrap();
            z.write_all(b"x").unwrap();
            z.finish().unwrap();
        }
        let mr_outer = dir.join("mr.jar");
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&mr_outer).unwrap());
            z.start_file("BOOT-INF/lib/mr.jar", opts).unwrap();
            z.write_all(&mr).unwrap();
            z.finish().unwrap();
        }
        let names =
            ClassPath::nested_archive_entry_names(&mr_outer, "BOOT-INF/lib/mr.jar").unwrap();
        assert!(names.contains("p/V.class"), "{names:?}");
        assert!(names.contains("META-INF/versions/17/p/V.class"));
        assert!(!names.contains("q.txt"), "not a numeric version directory");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `nested_archive_entry_index` (interpreter round i2 wave 65, lane L5):
    /// the same names as `nested_archive_entry_names`, and a stamp that tells
    /// a fat jar rewritten at the same path (the index holds no mapping, so
    /// the file can be rewritten on every host).
    #[test]
    fn nested_archive_entry_index_notices_a_rewritten_outer_jar() {
        fn fat_jar(path: &Path, nested_entries: &[&str]) {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut inner = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut inner));
                for name in nested_entries {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
                }
                z.finish().unwrap();
            }
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/lib/dep.jar", opts).unwrap();
            z.write_all(&inner).unwrap();
            z.finish().unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_nested_entry_index_stamp");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, &["p/Old.class"]);

        let index = ClassPath::nested_archive_entry_index(&jar_path, "BOOT-INF/lib/dep.jar")
            .expect("indexed");
        assert!(index.contains("p/Old.class"));
        assert!(!index.contains("p/New.class"));
        assert_eq!(index.len(), 1);
        assert!(!index.is_empty());
        assert!(index.outer_unchanged());

        // Rewritten at the same path with one more class: a longer file.
        fat_jar(&jar_path, &["p/Old.class", "p/New.class"]);
        assert!(!index.outer_unchanged(), "the rewrite is noticed");
        let rebuilt = ClassPath::nested_archive_entry_index(&jar_path, "BOOT-INF/lib/dep.jar")
            .expect("re-indexed");
        assert!(rebuilt.contains("p/New.class"));
        assert!(rebuilt.outer_unchanged());

        assert!(
            ClassPath::nested_archive_entry_index(&jar_path, "BOOT-INF/lib/none.jar").is_none()
        );
        assert!(ClassPath::nested_archive_entry_index(&dir.join("absent.jar"), "a.jar").is_none());
        assert!(
            ClassPath::nested_archive_entry_index(&dir, "a.jar").is_none(),
            "a directory"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// `OuterArchive` (interpreter round i2 wave 66, lane L5,
    /// `CRATONVM_NESTED_JAR_SHARED_OUTER`): one open of the outer fat jar
    /// answers every nested jar exactly as the per-spec opens do -- the same
    /// walk-index names and the same entry bytes, for a stored and a deflated
    /// nested jar -- carries the outer stamp, and tells a rewrite.
    #[test]
    fn outer_archive_answers_every_nested_jar_as_the_per_spec_opens_do() {
        fn inner(entries: &[(&str, &[u8])]) -> Vec<u8> {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let mut out = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut out));
                for (name, data) in entries {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(data).unwrap();
                }
                z.finish().unwrap();
            }
            out
        }
        fn fat_jar(path: &Path, extra: bool) {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let a = inner(&[
                ("a/A.class", &b"\xCA\xFE\xBA\xBEA"[..]),
                ("a/r.txt", &b"alpha"[..]),
            ]);
            let mut b_entries: Vec<(&str, &[u8])> = vec![("b/B.class", &b"\xCA\xFE\xBA\xBEB"[..])];
            if extra {
                b_entries.push(("b/C.class", &b"\xCA\xFE\xBA\xBEC"[..]));
            }
            let b = inner(&b_entries);
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/classes/app.properties", stored)
                .unwrap();
            z.write_all(b"k=v").unwrap();
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&a).unwrap();
            z.start_file("BOOT-INF/lib/b.jar", deflated).unwrap();
            z.write_all(&b).unwrap();
            z.finish().unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_shared_outer_archive");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);

        let outer = OuterArchive::open(&jar_path).expect("opened");
        assert_eq!(outer.path(), jar_path.as_path());
        assert_eq!(outer.entry_count(), 3);
        assert!(outer.unchanged());
        for (nested, present, absent) in [
            ("BOOT-INF/lib/a.jar", "a/A.class", "b/B.class"),
            ("BOOT-INF/lib/b.jar", "b/B.class", "a/A.class"),
        ] {
            let shared = ClassPath::nested_archive_entry_index_in(&outer, nested).expect("indexed");
            let own = ClassPath::nested_archive_entry_names(&jar_path, nested).expect("indexed");
            assert_eq!(shared.len(), own.len(), "{nested}");
            for name in &own {
                assert!(shared.contains(name), "{nested}: {name}");
            }
            assert!(shared.contains(present));
            assert!(!shared.contains(absent));
            assert!(shared.outer_unchanged());

            let shared_bytes = ClassPath::open_nested_archive_in(&outer, nested).expect("opened");
            let own_bytes =
                ClassPath::open_nested_archive(&jar_path, nested, true).expect("opened");
            assert_eq!(shared_bytes.plain_view(), own_bytes.plain_view());
            assert_eq!(
                shared_bytes.entry_bytes(present),
                own_bytes.entry_bytes(present)
            );
            assert!(
                shared_bytes.entry_bytes(present).is_some(),
                "{nested}: {present}"
            );
            assert!(shared_bytes.outer_unchanged());
        }
        assert!(
            ClassPath::nested_archive_entry_index_in(&outer, "BOOT-INF/lib/none.jar").is_none()
        );
        assert!(
            ClassPath::nested_archive_entry_index_in(&outer, "BOOT-INF/classes/app.properties")
                .is_none(),
            "not an archive"
        );
        assert!(ClassPath::nested_archive_entry_index_in(&outer, "../x.jar").is_none());
        assert!(ClassPath::nested_archive_entry_index_in(&outer, "").is_none());
        assert!(ClassPath::open_nested_archive_in(&outer, "BOOT-INF/lib/none.jar").is_none());
        assert!(OuterArchive::open(&dir.join("absent.jar")).is_none());
        assert!(OuterArchive::open(&dir).is_none(), "a directory");

        // Rewritten at the same path with one more class in b.jar. On
        // Windows the outer holds no mapping (read into memory), so the
        // rewrite succeeds there too.
        let index = ClassPath::nested_archive_entry_index_in(&outer, "BOOT-INF/lib/b.jar").unwrap();
        fat_jar(&jar_path, true);
        assert!(!outer.unchanged(), "the rewrite is noticed");
        assert!(!index.outer_unchanged(), "and by what the outer handed out");
        let reopened = OuterArchive::open(&jar_path).expect("reopened");
        let rebuilt =
            ClassPath::nested_archive_entry_index_in(&reopened, "BOOT-INF/lib/b.jar").unwrap();
        assert!(rebuilt.contains("b/C.class"));
        assert!(rebuilt.outer_unchanged());
        drop((outer, reopened));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `OuterArchiveIndex` (interpreter round i2 wave 67, lane L5,
    /// `CRATONVM_NESTED_JAR_OUTER_INDEX`): the outer central directory held
    /// without the outer bytes answers every nested jar as the per-spec opens
    /// do, holds no open file between asks, and refuses -- never crashes on
    /// -- a fat jar truncated, rewritten, or rewritten in place with the same
    /// length and modification time, then deleted.
    #[test]
    fn outer_archive_index_reads_each_nested_jar_at_the_ask() {
        fn inner(entries: &[(&str, &[u8])]) -> Vec<u8> {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let mut out = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut out));
                for (name, data) in entries {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(data).unwrap();
                }
                z.finish().unwrap();
            }
            out
        }
        fn fat_jar(path: &Path, extra: bool) {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let a = inner(&[
                ("a/A.class", &b"\xCA\xFE\xBA\xBEA"[..]),
                ("a/r.txt", &b"alpha"[..]),
            ]);
            let mut b_entries: Vec<(&str, &[u8])> = vec![("b/B.class", &b"\xCA\xFE\xBA\xBEB"[..])];
            if extra {
                b_entries.push(("b/C.class", &b"\xCA\xFE\xBA\xBEC"[..]));
            }
            let b = inner(&b_entries);
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/classes/app.properties", stored)
                .unwrap();
            z.write_all(b"k=v").unwrap();
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&a).unwrap();
            z.start_file("BOOT-INF/lib/b.jar", deflated).unwrap();
            z.write_all(&b).unwrap();
            z.finish().unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_outer_archive_index");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);

        let outer = OuterArchiveIndex::open(&jar_path).expect("parsed");
        assert_eq!(outer.path(), jar_path.as_path());
        assert_eq!(outer.entry_count(), 3);
        assert!(outer.unchanged());
        assert!(!outer.holds_open_file(), "closed after the parse");
        for (nested, present, absent) in [
            ("BOOT-INF/lib/a.jar", "a/A.class", "b/B.class"),
            ("BOOT-INF/lib/b.jar", "b/B.class", "a/A.class"),
        ] {
            let index =
                ClassPath::nested_archive_entry_index_in_index(&outer, nested).expect("indexed");
            let own = ClassPath::nested_archive_entry_names(&jar_path, nested).expect("indexed");
            assert_eq!(index.len(), own.len(), "{nested}");
            for name in &own {
                assert!(index.contains(name), "{nested}: {name}");
            }
            assert!(index.contains(present));
            assert!(!index.contains(absent));
            assert!(index.outer_unchanged());
            assert!(!outer.holds_open_file(), "closed after the ask");

            let bytes = ClassPath::open_nested_archive_in_index(&outer, nested).expect("opened");
            let own_bytes =
                ClassPath::open_nested_archive(&jar_path, nested, true).expect("opened");
            assert_eq!(bytes.plain_view(), own_bytes.plain_view());
            assert_eq!(bytes.entry_bytes(present), own_bytes.entry_bytes(present));
            assert!(bytes.entry_bytes(present).is_some(), "{nested}: {present}");
            assert!(bytes.outer_unchanged());
            drop(own_bytes);
        }
        assert!(
            ClassPath::nested_archive_entry_index_in_index(&outer, "BOOT-INF/lib/none.jar")
                .is_none()
        );
        assert!(
            ClassPath::nested_archive_entry_index_in_index(
                &outer,
                "BOOT-INF/classes/app.properties"
            )
            .is_none(),
            "not an archive"
        );
        assert!(ClassPath::nested_archive_entry_index_in_index(&outer, "../x.jar").is_none());
        assert!(ClassPath::nested_archive_entry_index_in_index(&outer, "").is_none());
        assert!(ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/none.jar").is_none());
        assert!(OuterArchiveIndex::open(&dir.join("absent.jar")).is_none());
        assert!(OuterArchiveIndex::open(&dir).is_none(), "a directory");
        assert!(!outer.holds_open_file());

        // A row handed out before the file changes keeps its own (owned)
        // bytes: it is still readable after a truncation, and its stamp
        // tells the change.
        let held = ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/a.jar").unwrap();

        // Truncated to half: no handle is held, so this works on every host,
        // and the next ask is refused (stamp), never a signal.
        let full = fs::read(&jar_path).unwrap();
        fs::write(&jar_path, &full[..full.len() / 2]).unwrap();
        assert!(!outer.unchanged(), "the truncation is noticed");
        assert!(
            ClassPath::nested_archive_entry_index_in_index(&outer, "BOOT-INF/lib/a.jar").is_none()
        );
        assert!(ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/b.jar").is_none());
        // Half a fat jar can still parse: a STORED nested jar's own end-of-
        // central-directory record lies in the first half, so the zip reader
        // may find that inner directory. Whatever it finds is not the outer
        // jar the index was built from.
        if let Some(half) = OuterArchiveIndex::open(&jar_path) {
            assert_ne!(
                half.entry_count(),
                outer.entry_count(),
                "half a zip is not the fat jar"
            );
        }
        assert!(!held.outer_unchanged());
        assert_eq!(
            held.entry_bytes("a/r.txt").as_deref(),
            Some(&b"alpha"[..]),
            "owned bytes"
        );

        // Rewritten with one more class in b.jar: a fresh index sees it.
        fat_jar(&jar_path, true);
        let reparsed = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let rebuilt =
            ClassPath::nested_archive_entry_index_in_index(&reparsed, "BOOT-INF/lib/b.jar")
                .unwrap();
        assert!(rebuilt.contains("b/C.class"));
        assert!(rebuilt.outer_unchanged());

        // Rewritten IN PLACE with the same length and modification time (one
        // byte of the stored a.jar flipped): the stamp cannot tell, the
        // outer entry's CRC does.
        let data_start = {
            let mut z = ZipArchive::new(fs::File::open(&jar_path).unwrap()).unwrap();
            let entry = z.by_name("BOOT-INF/lib/a.jar").unwrap();
            entry.data_start().unwrap() as usize
        };
        let mtime = fs::metadata(&jar_path).unwrap().modified().unwrap();
        let mut bytes = fs::read(&jar_path).unwrap();
        bytes[data_start + 8] ^= 0xFF;
        fs::write(&jar_path, &bytes).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&jar_path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        if reparsed.unchanged() {
            assert!(
                ClassPath::open_nested_archive_in_index(&reparsed, "BOOT-INF/lib/a.jar").is_none(),
                "a bad CRC answers None"
            );
        }
        assert!(!reparsed.holds_open_file());

        // Deleted: no handle blocks it (Windows too), and the next ask is
        // refused.
        fs::remove_file(&jar_path).unwrap();
        assert!(!reparsed.unchanged());
        assert!(ClassPath::open_nested_archive_in_index(&reparsed, "BOOT-INF/lib/b.jar").is_none());
        drop((outer, reparsed, held));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `open_nested_archive_directory_in_index` (interpreter round i2 wave
    /// 68, lane L5, `CRATONVM_NESTED_JAR_BYTES_DIRECTORY`): a STORED nested
    /// jar's row keeps only its directory and serves every entry -- small,
    /// stored, deflated, and larger than the read-ahead -- byte for byte as
    /// the owned copy does, with the same index and plain view, holding no
    /// file between asks; a deflated nested jar keeps the copy; a truncated,
    /// rewritten or deleted fat jar answers `None`, never a signal.
    #[test]
    fn a_directory_row_reads_each_entry_from_the_outer_file() {
        let big: Vec<u8> = (0..(3 * NESTED_WINDOW_READ_AHEAD + 17))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let big_deflated: Vec<u8> = (0..(2 * NESTED_WINDOW_READ_AHEAD))
            .map(|i| (i % 7) as u8)
            .collect();
        let inner = |extra: bool| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let mut out = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut out));
                z.add_directory("a/", stored).unwrap();
                z.start_file("a/A.class", stored).unwrap();
                z.write_all(b"\xCA\xFE\xBA\xBEA").unwrap();
                z.start_file("a/r.txt", deflated).unwrap();
                z.write_all(b"alpha alpha alpha").unwrap();
                z.start_file("a/Big.class", stored).unwrap();
                z.write_all(&big).unwrap();
                z.start_file("a/Big2.class", deflated).unwrap();
                z.write_all(&big_deflated).unwrap();
                if extra {
                    z.start_file("a/New.class", stored).unwrap();
                    z.write_all(b"\xCA\xFE\xBA\xBEN").unwrap();
                }
                z.finish().unwrap();
            }
            out
        };
        let fat_jar = |path: &Path, extra: bool| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let a = inner(extra);
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/classes/app.properties", stored)
                .unwrap();
            z.write_all(b"k=v").unwrap();
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&a).unwrap();
            z.start_file("BOOT-INF/lib/d.jar", deflated).unwrap();
            z.write_all(&a).unwrap();
            z.finish().unwrap();
        };
        let dir = std::env::temp_dir().join("cratonvm_test_nested_directory_row");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);

        let outer = OuterArchiveIndex::open(&jar_path).expect("parsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .expect("a directory row");
        assert!(
            !row.holds_nested_bytes(),
            "a stored nested jar: directory only"
        );
        let copy =
            ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/a.jar").expect("copy");
        assert!(copy.holds_nested_bytes());
        assert_eq!(row.plain_view(), copy.plain_view());
        assert_eq!(row.entry_index, copy.entry_index, "the same index");
        for name in [
            "a/A.class",
            "a/r.txt",
            "a/Big.class",
            "a/Big2.class",
            "a/",
            "a/None.class",
            "",
            "a",
        ] {
            assert_eq!(row.entry_bytes(name), copy.entry_bytes(name), "{name}");
        }
        assert_eq!(row.entry_bytes("a/Big.class").as_deref(), Some(&big[..]));
        assert_eq!(
            row.entry_bytes("a/Big2.class").as_deref(),
            Some(&big_deflated[..])
        );
        assert_eq!(
            row.entry_bytes("a/r.txt").as_deref(),
            Some(&b"alpha alpha alpha"[..])
        );
        // Asked twice, and out of order: the read-ahead never serves stale
        // or misplaced bytes.
        assert_eq!(
            row.entry_bytes("a/A.class").as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEA"[..])
        );
        assert_eq!(row.entry_bytes("a/Big.class").as_deref(), Some(&big[..]));
        let crc = ZipArchive::new(Cursor::new(inner(false)))
            .unwrap()
            .by_name("a/A.class")
            .unwrap()
            .crc32();
        assert_eq!(
            row.entry_bytes_matching("a/A.class", crc, 5).as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEA"[..])
        );
        assert!(row.entry_bytes_matching("a/A.class", crc ^ 1, 5).is_none());
        match &row.archive {
            NestedArchiveReader::Window { state, .. } => {
                let state = state.lock();
                assert!(
                    state.file.is_none() && state.buf.is_empty(),
                    "nothing held between asks"
                );
            }
            NestedArchiveReader::Held(_) => unreachable!(),
        }
        assert!(!outer.holds_open_file());

        // A deflated nested jar: the owned copy, as before.
        let deflated =
            ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/d.jar")
                .expect("a copy row");
        assert!(deflated.holds_nested_bytes());
        assert_eq!(
            deflated.entry_bytes("a/Big.class").as_deref(),
            Some(&big[..])
        );
        assert!(
            ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/none.jar")
                .is_none()
        );
        assert!(
            ClassPath::open_nested_archive_directory_in_index(
                &outer,
                "BOOT-INF/classes/app.properties"
            )
            .is_none(),
            "not an archive"
        );
        assert!(ClassPath::open_nested_archive_directory_in_index(&outer, "../x.jar").is_none());

        // Truncated under the held row: refused by the stamp, never a signal;
        // the copy still answers from its own bytes.
        let full = fs::read(&jar_path).unwrap();
        fs::write(&jar_path, &full[..full.len() / 2]).unwrap();
        assert!(!row.outer_unchanged());
        assert!(row.entry_bytes("a/A.class").is_none());
        assert!(copy.entry_bytes("a/A.class").is_some());

        // Rewritten: the held row stays refused (its stamp); a row built
        // from a reparsed outer serves the new entry.
        fat_jar(&jar_path, true);
        assert!(row.entry_bytes("a/A.class").is_none());
        let reparsed = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let fresh =
            ClassPath::open_nested_archive_directory_in_index(&reparsed, "BOOT-INF/lib/a.jar")
                .unwrap();
        assert_eq!(
            fresh.entry_bytes("a/New.class").as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEN"[..])
        );

        // Deleted: no handle blocks it (Windows too).
        fs::remove_file(&jar_path).unwrap();
        assert!(fresh.entry_bytes("a/A.class").is_none());
        drop((outer, reparsed, row, copy, deflated, fresh));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `OuterHandlePool` (interpreter round i2 wave 69, lane L5,
    /// `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`): an isolated read opens and
    /// closes; the second read of a burst keeps its handle and the third
    /// reuses it, serving the same bytes; a file replaced by a rename with the
    /// SAME length and modification time (the row's stamp still matches) is
    /// never read through the old handle; a deleted file is refused; the
    /// reaper closes an idle handle and exits. On Windows the pool keeps
    /// nothing and every read opens afresh.
    #[test]
    fn a_kept_outer_handle_serves_a_burst_and_never_a_replaced_file() {
        let fat_jar = |path: &Path, a_byte: u8| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut inner = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut inner));
                z.start_file("a/A.class", stored).unwrap();
                z.write_all(&[0xCA, 0xFE, 0xBA, 0xBE, a_byte]).unwrap();
                z.start_file("a/B.class", stored).unwrap();
                z.write_all(b"\xCA\xFE\xBA\xBEB").unwrap();
                z.finish().unwrap();
            }
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&inner).unwrap();
            z.finish().unwrap();
        };
        let dir = std::env::temp_dir().join("cratonvm_test_outer_handle_pool");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, b'A');
        let outer = OuterArchiveIndex::open(&jar_path).expect("parsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .expect("a directory row");
        assert!(!row.holds_nested_bytes());

        // A long interval: the burst does not end by itself mid-test.
        let pool = OuterHandlePool::with_idle(std::time::Duration::from_secs(30));
        let a = Some(b"\xCA\xFE\xBA\xBEA".to_vec());
        let b = Some(b"\xCA\xFE\xBA\xBEB".to_vec());
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(
            (pool.opens(), pool.reuses(), pool.held()),
            (1, 0, 0),
            "isolated: closed"
        );
        assert_eq!(row.entry_bytes_pooled("a/B.class", Some(&pool)), b);
        let kept = usize::from(OUTER_HANDLE_REUSE_SUPPORTED);
        assert_eq!(
            (pool.opens(), pool.held()),
            (2, kept),
            "second of a burst: kept (Unix)"
        );
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(row.entry_bytes_pooled("a/B.class", Some(&pool)), b);
        if !OUTER_HANDLE_REUSE_SUPPORTED {
            assert_eq!((pool.opens(), pool.reuses(), pool.held()), (4, 0, 0));
            assert_eq!(pool.reapers_spawned(), 0);
            drop((pool, row, outer));
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!((pool.opens(), pool.reuses(), pool.held()), (2, 2, 1));
        assert_eq!(pool.reapers_spawned(), 1, "one reaper per burst");
        // Without a pool: the same bytes, opened afresh.
        assert_eq!(row.entry_bytes("a/A.class"), a);

        // Replaced by a rename with a file of the same length and the same
        // modification time: the row's stamp still matches, the kept handle
        // reads the OLD inode; the path's inode differs, so the read opens
        // the new file, whose bytes fail the old directory's CRC -> `None`
        // (the body runs, as without the pool).
        let mtime = fs::metadata(&jar_path).unwrap().modified().unwrap();
        let next = dir.join("app.jar.next");
        fat_jar(&next, b'Z');
        fs::File::options()
            .write(true)
            .open(&next)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        fs::rename(&next, &jar_path).unwrap();
        if row.outer_unchanged() {
            assert_eq!(
                row.entry_bytes_pooled("a/A.class", Some(&pool)),
                None,
                "never the old file"
            );
            assert_eq!(
                row.entry_bytes("a/A.class"),
                None,
                "the same answer without the pool"
            );
            assert_eq!(pool.held(), 0, "the old handle was closed");
        }

        // A fresh file and a fresh burst; then deleted: refused, nothing kept.
        fat_jar(&jar_path, b'A');
        let outer = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .unwrap();
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(pool.held(), 1);
        fs::remove_file(&jar_path).unwrap();
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), None);
        assert_eq!(pool.held(), 0);

        // Truncated in place under a kept handle: refused.
        fat_jar(&jar_path, b'A');
        let outer = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .unwrap();
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(pool.held(), 1);
        let full = fs::read(&jar_path).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&jar_path)
            .unwrap()
            .set_len(full.len() as u64 / 2)
            .unwrap();
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), None);
        assert_eq!(pool.held(), 0);
        drop(pool);

        // The reaper closes an idle handle and exits.
        fat_jar(&jar_path, b'A');
        let outer = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .unwrap();
        let short = OuterHandlePool::with_idle(std::time::Duration::from_millis(20));
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&short)), a);
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&short)), a);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while (short.held() > 0 || short.reaper_running()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(short.held(), 0, "closed by the reaper");
        assert!(!short.reaper_running(), "the reaper exited");
        drop((short, row, outer));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `NestedArchiveEntries::pooled_read_checks_outer` (interpreter round i3
    /// wave 71, lane L5, `CRATONVM_NESTED_JAR_POOLED_STAT`): `true` only for a
    /// directory-only row while the pool keeps a handle of its fat jar (Unix);
    /// a row holding its bytes never leaves its `stat` to a read. A fat jar
    /// rewritten under a kept handle is answered `None` by the pooled read
    /// (never stale bytes), after which the pool keeps nothing, the hit
    /// checks the file itself again and finds it changed.
    #[test]
    fn a_pooled_read_checks_the_outer_file_only_while_a_handle_is_kept() {
        let fat_jar = |path: &Path, extra: bool| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut inner = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut inner));
                z.start_file("a/A.class", stored).unwrap();
                z.write_all(b"\xCA\xFE\xBA\xBEA").unwrap();
                if extra {
                    z.start_file("a/New.class", stored).unwrap();
                    z.write_all(b"\xCA\xFE\xBA\xBEN").unwrap();
                }
                z.finish().unwrap();
            }
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&inner).unwrap();
            z.finish().unwrap();
        };
        let dir = std::env::temp_dir().join("cratonvm_test_pooled_read_checks_outer");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);
        let outer = OuterArchiveIndex::open(&jar_path).expect("parsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .expect("a directory row");
        let copy =
            ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/a.jar").expect("a copy");
        assert!(!row.holds_nested_bytes() && copy.holds_nested_bytes());
        let pool = OuterHandlePool::with_idle(std::time::Duration::from_secs(30));
        assert!(!row.pooled_read_checks_outer(&pool), "nothing kept yet");

        let a = Some(b"\xCA\xFE\xBA\xBEA".to_vec());
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(
            row.pooled_read_checks_outer(&pool),
            OUTER_HANDLE_REUSE_SUPPORTED,
            "a kept handle (Unix)"
        );
        assert!(
            !copy.pooled_read_checks_outer(&pool),
            "a row holding its bytes checks the file itself"
        );

        if OUTER_HANDLE_REUSE_SUPPORTED {
            // Rewritten with another length under the kept handle: the hit
            // would skip its `stat`, and the pooled read refuses the file.
            fat_jar(&jar_path, true);
            assert!(
                row.pooled_read_checks_outer(&pool),
                "no system call: still kept"
            );
            assert_eq!(
                row.entry_bytes_pooled("a/A.class", Some(&pool)),
                None,
                "never stale bytes"
            );
            assert_eq!(pool.held(), 0, "a refused read keeps nothing");
            assert!(
                !row.pooled_read_checks_outer(&pool),
                "the next hit checks the file"
            );
            assert!(
                !row.outer_unchanged(),
                "and finds it changed (the row is rebuilt)"
            );
        }
        drop((pool, row, copy, outer));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `ClassPath::outer_archive_for` (interpreter round i2 wave 69, lane L5,
    /// `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER`): a fat jar that is an archive
    /// of the class path (with or without fat-jar emulation) is lent with
    /// its load-time stamp, and the walk and bytes rows built from it answer
    /// what rows built from a fresh open answer; a file that is not on the
    /// class path is not lent, nor (Unix: a mapped file cannot be truncated
    /// on Windows) one rewritten since the class path loaded it.
    #[test]
    fn the_class_path_lends_its_own_parse_of_a_fat_jar() {
        let fat_jar = |path: &Path, extra: bool| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut inner = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut inner));
                z.start_file("a/A.class", stored).unwrap();
                z.write_all(b"\xCA\xFE\xBA\xBEA").unwrap();
                if extra {
                    z.start_file("a/New.class", stored).unwrap();
                    z.write_all(b"\xCA\xFE\xBA\xBEN").unwrap();
                }
                z.finish().unwrap();
            }
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&inner).unwrap();
            z.finish().unwrap();
        };
        let dir = std::env::temp_dir().join("cratonvm_test_class_path_outer_archive");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);
        let spelled = jar_path.to_string_lossy().into_owned();

        for emulation in [false, true] {
            let cp = ClassPath::with_fat_jar_emulation(std::slice::from_ref(&spelled), emulation);
            let lent = cp
                .outer_archive_for(&jar_path)
                .expect("an archive of the class path");
            assert!(lent.unchanged(), "the load-time stamp is the file's");
            let row =
                ClassPath::open_nested_archive_in(&lent, "BOOT-INF/lib/a.jar").expect("a row");
            let fresh =
                ClassPath::open_nested_archive(&jar_path, "BOOT-INF/lib/a.jar", false).unwrap();
            assert_eq!(row.entry_bytes("a/A.class"), fresh.entry_bytes("a/A.class"));
            assert_eq!(
                row.entry_bytes("a/A.class").as_deref(),
                Some(&b"\xCA\xFE\xBA\xBEA"[..])
            );
            let names =
                ClassPath::nested_archive_entry_index_in(&lent, "BOOT-INF/lib/a.jar").unwrap();
            assert!(names.contains("a/A.class") && !names.contains("a/New.class"));
            assert!(names.outer_unchanged());
            assert!(
                cp.outer_archive_for(&dir.join("other.jar")).is_none(),
                "not on the class path"
            );
            drop((lent, row, fresh, names, cp));
        }
        let cp = ClassPath::with_fat_jar_emulation(std::slice::from_ref(&spelled), false);
        assert!(
            ClassPath::new(&[]).outer_archive_for(&jar_path).is_none(),
            "an empty class path"
        );
        if cfg!(unix) {
            // Rewritten with another length since the class path loaded it:
            // never lent; the caller opens the file as it now is.
            fat_jar(&jar_path, true);
            assert!(
                cp.outer_archive_for(&jar_path).is_none(),
                "a rewritten fat jar"
            );
        }
        drop(cp);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `ClassPath::open_jar_archive_lent` (interpreter round i2 wave 70, lane
    /// L5, `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE`): a jar-loader row built
    /// from the class path's own archive answers what a row opened from the
    /// file answers (bytes, CRC/size check, plain view, entry index), carries
    /// the load-time stamp, and a Multi-Release jar's lent row has no plain
    /// view either; on Unix a jar rewritten after the lend is no longer
    /// `outer_unchanged` (the memo then drops the row), and is not lent again.
    #[test]
    fn a_jar_loader_row_over_the_class_path_archive_answers_as_an_opened_one() {
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
        fn crc_and_size(path: &Path, name: &str) -> (u32, u64) {
            let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
            let entry = archive.by_name(name).unwrap();
            (entry.crc32(), entry.size())
        }
        let dir = std::env::temp_dir().join("cratonvm_test_jar_loader_lent_archive");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let plain = dir.join("lib.jar");
        let big: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
        write_jar(
            &plain,
            &[
                ("p/Stored.class", &b"\xCA\xFE\xBA\xBEs"[..], false),
                ("p/Deflated.class", &big[..], true),
            ],
        );
        let mr = dir.join("mr.jar");
        write_jar(
            &mr,
            &[
                ("p/A.class", &b"base"[..], false),
                ("META-INF/versions/11/p/A.class", &b"x"[..], false),
            ],
        );
        let spelled: Vec<String> = [&plain, &mr]
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let cp = ClassPath::new(&spelled);

        let lent = cp
            .outer_archive_for(&plain)
            .expect("an archive of the class path");
        let row = ClassPath::open_jar_archive_lent(lent).expect("a row");
        let opened = ClassPath::open_jar_archive(&plain, false).expect("opened");
        assert!(row.plain_view() && opened.plain_view());
        assert!(row.outer_unchanged(), "the load-time stamp is the file's");
        assert_eq!(row.entry_index, opened.entry_index, "the same index");
        for name in ["p/Stored.class", "p/Deflated.class"] {
            let (crc, size) = crc_and_size(&plain, name);
            assert_eq!(
                row.entry_bytes_matching(name, crc, size),
                opened.entry_bytes_matching(name, crc, size)
            );
            assert!(row.entry_bytes_matching(name, crc, size).is_some());
            assert_eq!(
                row.entry_bytes_matching(name, crc ^ 1, size),
                None,
                "another CRC"
            );
        }
        assert_eq!(row.entry_bytes("p/Deflated.class"), Some(big.clone()));
        assert_eq!(row.entry_bytes("p/Absent.class"), None);

        let mr_row = ClassPath::open_jar_archive_lent(cp.outer_archive_for(&mr).unwrap()).unwrap();
        assert!(
            !mr_row.plain_view(),
            "a Multi-Release jar: no plain view, lent or not"
        );
        assert_eq!(mr_row.entry_bytes("p/A.class"), None);

        if cfg!(unix) {
            // Rewritten with another length after the lend: the row's stamp
            // (the class path's load-time one) no longer matches, and the
            // class path lends it no more; the caller opens the file as it is.
            write_jar(
                &plain,
                &[("p/Stored.class", &b"\xCA\xFE\xBA\xBEsecond"[..], false)],
            );
            assert!(!row.outer_unchanged(), "a rewritten jar");
            assert!(cp.outer_archive_for(&plain).is_none(), "not lent again");
        }
        drop((row, opened, mr_row, cp));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `ClassPath::jar_entry_bytes_matching` (interpreter round i3 wave 71,
    /// lane L5, `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`): the class path's
    /// own entry answers what a jar-loader row opened from the file answers
    /// (bytes on a CRC / size match, `Body` on a mismatch, an absent or
    /// directory name, or a jar without the plain view), `NotHeld` for a jar
    /// that is not on the class path, the same answer for a path spelled
    /// another way (canonical match, remembered), and on Unix `NotHeld` once
    /// the jar is rewritten with another length.
    #[test]
    fn a_class_path_jar_serves_its_entries_without_a_row() {
        use cratonvm_native_api::ClassPathJarBytes as Answer;
        fn write_jar(path: &Path, entries: &[(&str, &[u8], bool)]) {
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            for (name, data, deflate) in entries {
                let method = if *deflate {
                    zip::CompressionMethod::Deflated
                } else {
                    zip::CompressionMethod::Stored
                };
                let opts = zip::write::SimpleFileOptions::default().compression_method(method);
                if name.ends_with('/') {
                    z.add_directory(name.trim_end_matches('/'), opts).unwrap();
                } else {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(data).unwrap();
                }
            }
            z.finish().unwrap();
        }
        fn crc_and_size(path: &Path, name: &str) -> (u32, u64) {
            let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
            let entry = archive.by_name(name).unwrap();
            (entry.crc32(), entry.size())
        }
        let dir = std::env::temp_dir().join("cratonvm_test_jar_entry_bytes_direct");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(dir.join("sub"));
        let plain = dir.join("lib.jar");
        let big: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
        write_jar(
            &plain,
            &[
                ("p/", &b""[..], false),
                ("p/Stored.class", &b"\xCA\xFE\xBA\xBEs"[..], false),
                ("p/Deflated.class", &big[..], true),
            ],
        );
        let mr = dir.join("mr.jar");
        write_jar(
            &mr,
            &[
                ("p/A.class", &b"base"[..], false),
                ("META-INF/versions/11/p/A.class", &b"x"[..], false),
            ],
        );
        let spelled: Vec<String> = [&plain, &mr]
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let cp = ClassPath::new(&spelled);
        let opened = ClassPath::open_jar_archive(&plain, false).expect("opened");

        for name in ["p/Stored.class", "p/Deflated.class"] {
            let (crc, size) = crc_and_size(&plain, name);
            let expected = opened
                .entry_bytes_matching(name, crc, size)
                .expect("a row serves it");
            assert_eq!(
                cp.jar_entry_bytes_matching(&plain, name, crc, size),
                Answer::Bytes(expected)
            );
            assert_eq!(
                cp.jar_entry_bytes_matching(&plain, name, crc ^ 1, size),
                Answer::Body,
                "another CRC"
            );
            assert_eq!(
                cp.jar_entry_bytes_matching(&plain, name, crc, size + 1),
                Answer::Body,
                "another size"
            );
        }
        assert_eq!(
            cp.jar_entry_bytes_matching(&plain, "p/Absent.class", 0, 0),
            Answer::Body
        );
        assert_eq!(
            cp.jar_entry_bytes_matching(&plain, "p/", 0, 0),
            Answer::Body,
            "a directory"
        );
        assert_eq!(cp.jar_entry_bytes_matching(&plain, "", 0, 0), Answer::Body);

        // Another spelling of the same file: matched once canonicalised.
        let other_spelling = dir.join("sub").join("..").join("lib.jar");
        let (crc, size) = crc_and_size(&plain, "p/Stored.class");
        for _ in 0..2 {
            assert_eq!(
                cp.jar_entry_bytes_matching(&other_spelling, "p/Stored.class", crc, size),
                Answer::Bytes(b"\xCA\xFE\xBA\xBEs".to_vec()),
                "the same jar spelled another way"
            );
        }

        // A Multi-Release jar has no plain view: the body, as for its row.
        let (crc, size) = crc_and_size(&mr, "p/A.class");
        assert_eq!(
            cp.jar_entry_bytes_matching(&mr, "p/A.class", crc, size),
            Answer::Body
        );

        // Not on the class path: not held (twice: the absence is remembered).
        let elsewhere = dir.join("elsewhere.jar");
        write_jar(
            &elsewhere,
            &[("p/Stored.class", &b"\xCA\xFE\xBA\xBEs"[..], false)],
        );
        for _ in 0..2 {
            assert_eq!(
                cp.jar_entry_bytes_matching(&elsewhere, "p/Stored.class", crc, size),
                Answer::NotHeld
            );
        }
        assert_eq!(
            ClassPath::new(&[]).jar_entry_bytes_matching(&plain, "p/Stored.class", crc, size),
            Answer::NotHeld,
            "an empty class path"
        );

        if cfg!(unix) {
            // Rewritten with another length since the class path loaded it:
            // not held; the caller's rows open the file as it now is.
            write_jar(
                &plain,
                &[("p/Stored.class", &b"\xCA\xFE\xBA\xBEsecond"[..], false)],
            );
            let (crc, size) = crc_and_size(&plain, "p/Stored.class");
            assert_eq!(
                cp.jar_entry_bytes_matching(&plain, "p/Stored.class", crc, size),
                Answer::NotHeld
            );
        }
        drop((opened, cp));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `OuterFileStamp` (interpreter round i2 wave 68, lane L5): equal for an
    /// unchanged file, different after a rewrite of another length, `None`
    /// for no file or a directory.
    #[test]
    fn outer_file_stamp_tells_a_rewrite() {
        let dir = std::env::temp_dir().join("cratonvm_test_outer_file_stamp");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("f.jar");
        fs::write(&path, b"one").unwrap();
        let first = OuterFileStamp::read(&path).expect("a file");
        assert_eq!(OuterFileStamp::read(&path), Some(first));
        fs::write(&path, b"three").unwrap();
        assert_ne!(OuterFileStamp::read(&path), Some(first));
        fs::remove_file(&path).unwrap();
        assert_eq!(OuterFileStamp::read(&path), None);
        assert_eq!(OuterFileStamp::read(&dir), None, "a directory");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `open_nested_archive` / `NestedArchiveEntries::entry_bytes` (the
    /// nested-jar bytes index, interpreter round i2 wave 63, lane L5): a
    /// stored and a deflated entry read back exactly; directories, absent
    /// names, a name that is also a directory, Multi-Release and signed jars
    /// answer `None`; the outer archive's stamp tells a changed file.
    #[test]
    fn open_nested_archive_serves_plain_entries_only() {
        fn nested(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
            let mut buf = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut buf));
                for (name, data, deflate) in entries {
                    let method = if *deflate {
                        zip::CompressionMethod::Deflated
                    } else {
                        zip::CompressionMethod::Stored
                    };
                    let opts = zip::write::SimpleFileOptions::default().compression_method(method);
                    if name.ends_with('/') {
                        z.add_directory(name.trim_end_matches('/'), opts).unwrap();
                    } else {
                        z.start_file(*name, opts).unwrap();
                        z.write_all(data).unwrap();
                    }
                }
                z.finish().unwrap();
            }
            buf
        }
        let dir = std::env::temp_dir().join("cratonvm_test_open_nested_archive");
        let _ = fs::create_dir_all(&dir);
        let outer = dir.join("outer.jar");
        let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let plain = nested(&[
            ("p/", &b""[..], false),
            ("p/Stored.class", &b"\xCA\xFE\xBA\xBEstored"[..], false),
            ("p/Deflated.class", &big[..], true),
            ("both", &b"file"[..], false),
            ("both/", &b""[..], false),
        ]);
        let mr = nested(&[
            ("p/A.class", &b"base"[..], false),
            ("META-INF/versions/9/p/A.class", &b"nine"[..], false),
        ]);
        let signed = nested(&[
            ("p/A.class", &b"a"[..], false),
            ("meta-inf/signer.sf", &b"x"[..], false),
        ]);
        {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut z = zip::ZipWriter::new(fs::File::create(&outer).unwrap());
            for (name, data) in [
                ("BOOT-INF/lib/plain.jar", &plain),
                ("BOOT-INF/lib/mr.jar", &mr),
                ("BOOT-INF/lib/signed.jar", &signed),
            ] {
                z.start_file(name, stored).unwrap();
                z.write_all(data).unwrap();
            }
            z.finish().unwrap();
        }

        let jar =
            ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/plain.jar", true).expect("opened");
        assert!(jar.plain_view());
        assert!(jar.outer_unchanged());
        assert_eq!(
            jar.entry_bytes("p/Stored.class").as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEstored"[..])
        );
        assert_eq!(jar.entry_bytes("p/Deflated.class"), Some(big.clone()));
        assert!(jar.has_entry("p/"));
        assert_eq!(jar.entry_bytes("p/"), None, "a directory");
        assert_eq!(jar.entry_bytes("p"), None, "only the directory exists");
        assert_eq!(jar.entry_bytes("both"), None, "the name is a directory too");
        assert_eq!(jar.entry_bytes("p/Missing.class"), None);
        assert_eq!(jar.entry_bytes(""), None);

        let mr =
            ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/mr.jar", true).expect("opened");
        assert!(!mr.plain_view(), "Multi-Release");
        assert_eq!(mr.entry_bytes("p/A.class"), None);
        let signed = ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/signed.jar", true)
            .expect("opened");
        assert!(!signed.plain_view(), "a signature file, any case");
        assert_eq!(signed.entry_bytes("p/A.class"), None);

        assert!(ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/none.jar", true).is_none());
        assert!(ClassPath::open_nested_archive(&outer, "../x.jar", true).is_none());
        assert!(ClassPath::open_nested_archive(&dir.join("absent.jar"), "a.jar", true).is_none());

        // The stamp a later ask compares (not by rewriting `outer`: a mapped
        // file cannot be rewritten on every host).
        let other = dir.join("other.bin");
        fs::write(&other, b"one").unwrap();
        let stamp = OuterArchiveStamp::read(&other).expect("a file");
        fs::write(&other, b"longer").unwrap();
        assert_ne!(
            OuterArchiveStamp::read(&other),
            Some(stamp),
            "the length changed"
        );
        assert_eq!(
            OuterArchiveStamp::read(&dir),
            None,
            "a directory is no archive"
        );
        drop(jar);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Without `map`, `open_nested_archive` and `open_jar_archive` keep no
    /// mapping and no handle of the file: a jar an application loader was
    /// handed may be truncated or rewritten in place afterwards, on every
    /// host, and the copy keeps serving what it read (the stamp turns the
    /// caller away). With a mapping that truncation would be `SIGBUS` on
    /// Linux and macOS. Interpreter round i2 wave 64, lane L5 follow-up.
    #[test]
    fn unmapped_archive_opens_survive_a_truncation_of_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "cratonvm_test_unmapped_archive_opens_{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let big: Vec<u8> = (0..9000u32).map(|i| (i % 239) as u8).collect();
        let inner = {
            let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            z.start_file("p/A.class", deflated).unwrap();
            z.write_all(&big).unwrap();
            z.finish().unwrap().into_inner()
        };
        let jar_path = dir.join("app.jar");
        fs::write(&jar_path, &inner).unwrap();
        let outer = dir.join("fat.jar");
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&outer).unwrap());
            for method in [
                zip::CompressionMethod::Stored,
                zip::CompressionMethod::Deflated,
            ] {
                let name = format!("BOOT-INF/lib/{method:?}.jar");
                z.start_file(
                    name,
                    zip::write::SimpleFileOptions::default().compression_method(method),
                )
                .unwrap();
                z.write_all(&inner).unwrap();
            }
            z.finish().unwrap();
        }

        let stored = ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/Stored.jar", false)
            .expect("opened");
        let deflated = ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/Deflated.jar", false)
            .expect("opened");
        let jar = ClassPath::open_jar_archive(&jar_path, false).expect("opened");
        assert!(ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/none.jar", false).is_none());
        for archive in [&stored, &deflated, &jar] {
            assert!(archive.plain_view());
            assert!(archive.outer_unchanged());
            assert_eq!(archive.entry_bytes("p/A.class"), Some(big.clone()));
        }

        // Truncate both files in place (no handle of ours keeps them).
        fs::File::create(&outer).unwrap();
        fs::File::create(&jar_path).unwrap();
        for archive in [&stored, &deflated, &jar] {
            assert!(!archive.outer_unchanged(), "the stamp sees the rewrite");
            assert_eq!(
                archive.entry_bytes("p/A.class"),
                Some(big.clone()),
                "served from memory"
            );
        }
        drop((stored, deflated, jar));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `open_jar_archive` / `NestedArchiveEntries::entry_bytes_matching` (the
    /// jar-loader bytes index, interpreter round i2 wave 64, lane L5): a plain
    /// jar's stored and deflated entries read back when the caller's CRC and
    /// size agree with the central directory, and not otherwise;
    /// Multi-Release and signed jars, directories and absent names answer
    /// `None`; a non-archive is not opened.
    #[test]
    fn open_jar_archive_serves_entries_whose_crc_and_size_match() {
        fn write_jar(path: &Path, entries: &[(&str, &[u8], bool)]) {
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            for (name, data, deflate) in entries {
                let method = if *deflate {
                    zip::CompressionMethod::Deflated
                } else {
                    zip::CompressionMethod::Stored
                };
                let opts = zip::write::SimpleFileOptions::default().compression_method(method);
                if name.ends_with('/') {
                    z.add_directory(name.trim_end_matches('/'), opts).unwrap();
                } else {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(data).unwrap();
                }
            }
            z.finish().unwrap();
        }
        fn crc_and_size(path: &Path, name: &str) -> (u32, u64) {
            let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
            let entry = archive.by_name(name).unwrap();
            (entry.crc32(), entry.size())
        }
        let dir = std::env::temp_dir().join("cratonvm_test_open_jar_archive");
        let _ = fs::create_dir_all(&dir);
        let plain = dir.join("plain.jar");
        let big: Vec<u8> = (0..7000u32).map(|i| (i % 241) as u8).collect();
        write_jar(
            &plain,
            &[
                ("p/", &b""[..], false),
                ("p/Stored.class", &b"\xCA\xFE\xBA\xBEstored"[..], false),
                ("p/Deflated.class", &big[..], true),
            ],
        );
        let mr = dir.join("mr.jar");
        write_jar(
            &mr,
            &[
                ("p/A.class", &b"base"[..], false),
                ("META-INF/versions/11/p/A.class", &b"x"[..], false),
            ],
        );
        let signed = dir.join("signed.jar");
        write_jar(
            &signed,
            &[
                ("p/A.class", &b"a"[..], true),
                ("META-INF/SIGNER.RSA", &b"x"[..], false),
            ],
        );

        let jar = ClassPath::open_jar_archive(&plain, true).expect("opened");
        assert!(jar.plain_view());
        assert!(jar.outer_unchanged());
        let (crc, size) = crc_and_size(&plain, "p/Stored.class");
        assert_eq!(
            jar.entry_bytes_matching("p/Stored.class", crc, size)
                .as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEstored"[..])
        );
        assert_eq!(
            jar.entry_bytes_matching("p/Stored.class", crc ^ 1, size),
            None,
            "another CRC"
        );
        assert_eq!(
            jar.entry_bytes_matching("p/Stored.class", crc, size + 1),
            None,
            "another size"
        );
        let (crc, size) = crc_and_size(&plain, "p/Deflated.class");
        assert_eq!(size, big.len() as u64);
        assert_eq!(
            jar.entry_bytes_matching("p/Deflated.class", crc, size),
            Some(big.clone())
        );
        assert_eq!(jar.entry_bytes("p/Deflated.class"), Some(big.clone()));
        assert_eq!(jar.entry_bytes_matching("p/", 0, 0), None, "a directory");
        assert_eq!(jar.entry_bytes_matching("p/Missing.class", crc, size), None);

        let mr_jar = ClassPath::open_jar_archive(&mr, true).expect("opened");
        assert!(!mr_jar.plain_view(), "Multi-Release");
        let (crc, size) = crc_and_size(&mr, "p/A.class");
        assert_eq!(mr_jar.entry_bytes_matching("p/A.class", crc, size), None);
        let signed_jar = ClassPath::open_jar_archive(&signed, true).expect("opened");
        assert!(!signed_jar.plain_view(), "a signature file");
        let (crc, size) = crc_and_size(&signed, "p/A.class");
        assert_eq!(
            signed_jar.entry_bytes_matching("p/A.class", crc, size),
            None
        );

        let not_zip = dir.join("not.jar");
        fs::write(&not_zip, b"not a zip").unwrap();
        assert!(ClassPath::open_jar_archive(&not_zip, true).is_none());
        assert!(ClassPath::open_jar_archive(&dir.join("absent.jar"), true).is_none());
        assert!(
            ClassPath::open_jar_archive(&dir, true).is_none(),
            "a directory"
        );
        drop(jar);
        drop(mr_jar);
        drop(signed_jar);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_resource_from_nested_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_spi");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("META-INF/services/org.dep.SPI")
            .expect("should find SPI file from nested JAR");
        assert_eq!(data, b"org.dep.SpiImpl");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_class_not_found_returns_error() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_missing");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(cp.find_class("com/example/Missing").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_parsing() {
        let manifest = b"Manifest-Version: 1.0\r\n\
            Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
            Start-Class: com.example.DemoApplication\r\n\
            Spring-Boot-Version: 3.2.0\r\n\
            Spring-Boot-Classes: BOOT-INF/classes/\r\n\
            Spring-Boot-Lib: BOOT-INF/lib/\r\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.main_class.as_deref(),
            Some("org.springframework.boot.loader.JarLauncher")
        );
        assert_eq!(
            info.start_class.as_deref(),
            Some("com.example.DemoApplication")
        );
        assert!(info.is_spring_boot());
    }

    #[test]
    fn manifest_continuation_lines() {
        // MANIFEST.MF allows continuation lines starting with a single space
        let manifest = b"Manifest-Version: 1.0\r\n\
            Main-Class: org.springframework.boot.loader.JarLa\r\n \
            uncher\r\n\
            Start-Class: com.example.App\r\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.main_class.as_deref(),
            Some("org.springframework.boot.loader.JarLauncher")
        );
    }

    #[test]
    fn plain_jar_not_treated_as_fat_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_plain_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("plain.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_foo").unwrap();
        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        // Plain JAR should have exactly 1 entry (JarFile)
        assert_eq!(cp.entry_count(), 1);

        let data = cp.find_class("com/example/Foo").unwrap();
        assert!(data.ends_with(b"_foo"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dynamic_add_path_fat_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_dynamic_fatjar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let mut cp = ClassPath::new(&[]);
        assert!(cp.is_empty());

        cp.add_path(&jar_path.to_string_lossy());
        assert!(!cp.is_empty());

        // Should find classes from nested structure
        assert!(cp.find_class("com/example/App").is_ok());
        assert!(cp.find_class("org/dep/Util").is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // JMOD file tests
    // -----------------------------------------------------------------------

    #[test]
    fn jmod_magic_validation() {
        // A file with wrong magic should fail
        let dir = std::env::temp_dir().join("cratonvm_test_jmod");
        let _ = fs::create_dir_all(&dir);
        let bad_path = dir.join("bad.jmod");
        fs::write(&bad_path, b"NOT_JMOD_DATA").unwrap();
        assert!(ClassPath::load_jmod(&bad_path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A `jmods/<module>.jmod` of a JDK (a `lib` directory beside `jmods`)
    /// names its resources `jrt:/<module>/...`, as the `lib/modules` form and
    /// HotSpot do; a `.jmod` anywhere else keeps its `jar:file:` URL.
    #[test]
    fn jdk_jmod_resources_are_jrt_urls() {
        fn write_jmod(path: &Path) {
            let mut zip_buf = Vec::new();
            {
                let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buf));
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                w.start_file("classes/pkg/res.txt", options).unwrap();
                w.write_all(b"x").unwrap();
                w.finish().unwrap();
            }
            let mut data = Vec::new();
            data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
            data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
            data.push(0x00);
            data.extend_from_slice(&zip_buf);
            fs::write(path, &data).unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_jmod_jrt_url");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home/jmods")).unwrap();
        fs::create_dir_all(dir.join("home/lib")).unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();
        let jdk_jmod = dir.join("home/jmods/java.base.jmod");
        let loose_jmod = dir.join("other/loose.jmod");
        write_jmod(&jdk_jmod);
        write_jmod(&loose_jmod);

        let cp = ClassPath::new(&[jdk_jmod.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.find_all_resource_urls("pkg/res.txt"),
            vec!["jrt:/java.base/pkg/res.txt".to_string()]
        );
        assert_eq!(
            cp.find_all_resource_urls("pkg/*.txt"),
            vec!["jrt:/java.base/pkg/res.txt".to_string()]
        );
        let cp = ClassPath::new(&[loose_jmod.to_string_lossy().into_owned()]);
        let urls = cp.find_all_resource_urls("pkg/res.txt");
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("jar:file:/") && urls[0].ends_with("loose.jmod!/pkg/res.txt"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Interpreter round i3 wave 73, lane L5
    /// (`CRATONVM_PLATFORM_RESOURCE_JRT_WALK`): the runtime-image walk answers
    /// the first `jrt:` URL of the whole-list walk, for a class path that
    /// mixes a directory and a loose `.jmod` holding the same name (neither
    /// counts) with a JDK `jmods/<module>.jmod` (which does), and `None` where
    /// only the non-`jrt:` entries hold it.
    #[test]
    fn i73_the_runtime_image_walk_answers_the_first_jrt_url_of_the_whole_walk() {
        fn write_jmod(path: &Path) {
            let mut zip_buf = Vec::new();
            {
                let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buf));
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                w.start_file("classes/pkg/res.txt", options).unwrap();
                w.write_all(b"x").unwrap();
                w.finish().unwrap();
            }
            let mut data = Vec::new();
            data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
            data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
            data.push(0x00);
            data.extend_from_slice(&zip_buf);
            fs::write(path, &data).unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_i73_jrt_walk");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home/jmods")).unwrap();
        fs::create_dir_all(dir.join("home/lib")).unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();
        fs::create_dir_all(dir.join("classes/pkg")).unwrap();
        fs::write(dir.join("classes/pkg/res.txt"), b"y").unwrap();
        let jdk_jmod = dir.join("home/jmods/java.base.jmod");
        let loose_jmod = dir.join("other/loose.jmod");
        write_jmod(&jdk_jmod);
        write_jmod(&loose_jmod);
        let classes = dir.join("classes");

        let first_jrt = |cp: &ClassPath, name: &str| {
            cp.find_all_resource_urls(name)
                .into_iter()
                .find(|url| url.starts_with("jrt:"))
        };
        let mixed = ClassPath::new(&[
            classes.to_string_lossy().into_owned(),
            loose_jmod.to_string_lossy().into_owned(),
            jdk_jmod.to_string_lossy().into_owned(),
        ]);
        for name in [
            "pkg/res.txt",
            "/pkg/res.txt",
            "pkg/*.txt",
            "pkg/absent.txt",
            "../x",
        ] {
            assert_eq!(
                mixed.first_runtime_image_resource_url(name),
                first_jrt(&mixed, name),
                "{name}"
            );
        }
        assert_eq!(
            mixed
                .first_runtime_image_resource_url("pkg/res.txt")
                .as_deref(),
            Some("jrt:/java.base/pkg/res.txt")
        );
        let no_image = ClassPath::new(&[
            classes.to_string_lossy().into_owned(),
            loose_jmod.to_string_lossy().into_owned(),
        ]);
        assert_eq!(no_image.find_all_resource_urls("pkg/res.txt").len(), 2);
        assert_eq!(
            no_image.first_runtime_image_resource_url("pkg/res.txt"),
            None
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_class_from_synthetic_jmod() {
        // Create a synthetic JMOD file: 4-byte magic + ZIP with classes/ prefix
        let dir = std::env::temp_dir().join("cratonvm_test_jmod2");
        let _ = fs::create_dir_all(&dir);
        let jmod_path = dir.join("test.jmod");

        // Build the ZIP portion in memory
        let mut zip_buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut zip_buf);
            let mut zip_writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip_writer
                .start_file("classes/java/lang/Object.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_fake_object")
                .unwrap();
            zip_writer
                .start_file("classes/java/lang/String.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_fake_string")
                .unwrap();
            zip_writer
                .start_file("classes/module-info.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_module_info")
                .unwrap();
            zip_writer.finish().unwrap();
        }

        // Write JMOD = magic + ZIP data
        let mut jmod_data = Vec::new();
        jmod_data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
        jmod_data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
        jmod_data.push(0x00); // minor version
        jmod_data.extend_from_slice(&zip_buf);
        fs::write(&jmod_path, &jmod_data).unwrap();

        // Load JMOD as classpath entry
        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty());

        // Find classes (the prefix "classes/" should be stripped automatically)
        let obj_bytes = cp.find_class("java/lang/Object").unwrap();
        assert_eq!(&obj_bytes[..4], b"\xCA\xFE\xBA\xBE");

        let str_bytes = cp.find_class("java/lang/String").unwrap();
        assert_eq!(&str_bytes[..4], b"\xCA\xFE\xBA\xBE");

        // Unknown class not found
        assert!(cp.find_class("java/lang/Missing").is_err());

        // list_class_names should find both (but not module-info)
        let names = cp.list_class_names();
        assert!(names.contains(&"java/lang/Object".to_string()));
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(!names.iter().any(|n| n.contains("module-info")));

        // module-info should be found by scan_module_infos
        let modules = cp.scan_module_infos();
        assert_eq!(modules.len(), 1);

        // Resource lookup should work via the classes/ prefix
        let resource = cp.find_resource("java/lang/Object.class");
        assert!(resource.is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    /// A JDK booted from `jmods/` (the Windows Microsoft build) is a runtime
    /// image too: `runtime_image_defines_class` must answer from its entries,
    /// not report "no image" and leave the caller on a name-prefix guess.
    #[test]
    fn runtime_image_defines_class_reads_jmod_entries() {
        let dir = std::env::temp_dir().join("cratonvm_test_jmod_image_defines");
        let _ = fs::create_dir_all(&dir);
        let jmod_path = dir.join("java.base.jmod");
        let mut zip_buf = Vec::new();
        {
            let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buf));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip_writer
                .start_file("classes/com/sun/crypto/provider/SunJCE.class", options)
                .unwrap();
            zip_writer.write_all(b"\xCA\xFE\xBA\xBE_fake").unwrap();
            zip_writer.finish().unwrap();
        }
        let mut jmod_data = Vec::new();
        jmod_data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
        jmod_data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
        jmod_data.push(0x00);
        jmod_data.extend_from_slice(&zip_buf);
        fs::write(&jmod_path, &jmod_data).unwrap();

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.runtime_image_defines_class("com/sun/crypto/provider/SunJCE"),
            Some(true)
        );
        // JDK-shaped, not in the image: an application class.
        assert_eq!(
            cp.runtime_image_defines_class("com/sun/el/ExpressionFactoryImpl"),
            Some(false)
        );
        // No image entry at all: no answer.
        let no_image = ClassPath::new(&[dir.to_string_lossy().into_owned()]);
        assert_eq!(
            no_image.runtime_image_defines_class("com/sun/crypto/provider/SunJCE"),
            None
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_real_jdk_jmod() {
        // Skip this test if no JDK is available
        let java_home = cratonvm_types::flags::runtime_var("JAVA_HOME")
            .ok()
            .or_else(|| {
                let candidate = std::path::PathBuf::from("C:/Program Files/Java/jdk-25");
                if candidate.exists() {
                    Some(candidate.to_string_lossy().into_owned())
                } else {
                    None
                }
            });
        let Some(java_home) = java_home else {
            eprintln!("Skipping load_real_jdk_jmod: no JAVA_HOME set");
            return;
        };
        let jmod_path = PathBuf::from(&java_home)
            .join("jmods")
            .join("java.base.jmod");
        if !jmod_path.exists() {
            eprintln!(
                "Skipping load_real_jdk_jmod: {} not found",
                jmod_path.display()
            );
            return;
        }

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty(), "JMOD classpath should not be empty");

        // Must find java.lang.Object
        let obj_bytes = cp.find_class("java/lang/Object").unwrap();
        assert_eq!(
            &obj_bytes[..4],
            b"\xCA\xFE\xBA\xBE",
            "Object.class should have class file magic"
        );
        assert!(obj_bytes.len() > 100, "Object.class should be non-trivial");

        // Must find java.lang.String
        let str_bytes = cp.find_class("java/lang/String").unwrap();
        assert_eq!(&str_bytes[..4], b"\xCA\xFE\xBA\xBE");
        assert!(str_bytes.len() > 1000, "String.class should be substantial");

        // Must find java.util.HashMap
        let hm_bytes = cp.find_class("java/util/HashMap").unwrap();
        assert_eq!(&hm_bytes[..4], b"\xCA\xFE\xBA\xBE");

        // Class listing should have thousands of classes
        let names = cp.list_class_names();
        assert!(
            names.len() > 1000,
            "java.base should have >1000 classes, got {}",
            names.len()
        );
        assert!(names.contains(&"java/lang/Object".to_string()));
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(names.contains(&"java/util/HashMap".to_string()));
        assert!(names.contains(&"java/util/ArrayList".to_string()));
        assert!(names.contains(&"java/io/InputStream".to_string()));

        // module-info.class should be found
        let modules = cp.scan_module_infos();
        assert_eq!(
            modules.len(),
            1,
            "java.base.jmod should have exactly 1 module-info"
        );
    }

    // --- Phase 80.5: Path Traversal Fix Tests ---

    #[test]
    fn find_class_rejects_dotdot_traversal() {
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("../../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn find_class_rejects_absolute_path() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("/etc/passwd").is_err());
        assert!(cp.find_class("\\Windows\\System32\\cmd").is_err());
    }

    #[test]
    fn find_class_rejects_null_byte() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object\0.evil").is_err());
    }

    #[test]
    fn find_class_rejects_windows_drive() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("C:Windows/System32/cmd").is_err());
    }

    #[test]
    fn find_class_rejects_current_dir_reference() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("./java/lang/Object").is_err());
        assert!(cp.find_class(".\\java\\lang\\Object").is_err());
    }

    #[test]
    fn find_resource_rejects_dotdot_traversal() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("../../../etc/passwd").is_none());
    }

    #[test]
    fn find_resource_rejects_null_byte() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("file\0.txt").is_none());
    }

    #[test]
    fn find_class_canonicalize_with_real_dir() {
        // Create a temp directory with a class file, verify canonicalization works
        let tmp = std::env::temp_dir().join("cratonvm_test_80_5");
        let _ = std::fs::create_dir_all(&tmp);
        let class_dir = tmp.join("com").join("test");
        let _ = std::fs::create_dir_all(&class_dir);
        // Write a minimal (invalid but loadable) class file
        let _ = std::fs::write(class_dir.join("Foo.class"), b"CAFEBABE_fake");
        let mut cp = ClassPath::new(&[]);
        cp.add_path(tmp.to_str().unwrap());
        // Normal lookup should work
        let result = cp.find_class("com/test/Foo");
        assert!(result.is_ok());
        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── JMOD integration tests ────────────────────────────────────────
    // These require a real JDK 9+ installation. Run with:
    //   cargo test -p cratonvm-classloading -- --ignored

    /// Helper: find java.base.jmod on this machine.
    fn find_java_base_jmod() -> Option<PathBuf> {
        // Try JAVA_HOME first
        if let Ok(val) = cratonvm_types::flags::runtime_var("JAVA_HOME") {
            let p = PathBuf::from(&val).join("jmods").join("java.base.jmod");
            if p.exists() {
                return Some(p);
            }
        }
        // Try common Windows paths
        for dir in &[
            r"C:\Program Files\Java",
            r"C:\Program Files\Eclipse Adoptium",
        ] {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let jmod = entry.path().join("jmods").join("java.base.jmod");
                    if jmod.exists() {
                        return Some(jmod);
                    }
                }
            }
        }
        // Try running java to find it
        let output = std::process::Command::new("java")
            .args(["-XshowSettings:properties", "-version"])
            .output()
            .ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("java.home") {
                if let Some(value) = rest.trim().strip_prefix('=') {
                    let jmod = PathBuf::from(value.trim())
                        .join("jmods")
                        .join("java.base.jmod");
                    if jmod.exists() {
                        return Some(jmod);
                    }
                }
            }
        }
        None
    }

    #[test]
    #[ignore] // requires JDK on host
    fn load_jmod_validates_magic_and_version() {
        let jmod_path = find_java_base_jmod();
        assert!(
            jmod_path.is_some(),
            "No JDK found — cannot test JMOD loading"
        );
        let jmod_path = jmod_path.unwrap();

        // Should load successfully
        let entry = ClassPath::load_jmod(&jmod_path);
        assert!(
            entry.is_ok(),
            "Failed to load {}: {:?}",
            jmod_path.display(),
            entry.err()
        );

        // Verify it's a JmodFile variant with a populated class_entry_index
        match entry.unwrap() {
            ClassPathEntry::JmodFile {
                class_entry_index,
                all_entry_names,
                ..
            } => {
                assert!(
                    class_entry_index.len() > 100,
                    "java.base should have >100 classes, got {}",
                    class_entry_index.len()
                );
                assert!(
                    all_entry_names.len() > class_entry_index.len(),
                    "all_entry_names should include non-class entries too"
                );
                // Should contain java.lang.Object
                assert!(
                    class_entry_index.contains("java/lang/Object.class"),
                    "java.base should contain java/lang/Object.class"
                );
                // Should contain java.lang.String
                assert!(
                    class_entry_index.contains("java/lang/String.class"),
                    "java.base should contain java/lang/String.class"
                );
                eprintln!(
                    "java.base.jmod: {} classes, {} total entries",
                    class_entry_index.len(),
                    all_entry_names.len()
                );
            }
            other => panic!("Expected JmodFile, got {other:?}"),
        }
    }

    #[test]
    #[ignore] // requires JDK on host
    fn find_class_from_jmod_returns_valid_classfile() {
        let jmod_path = find_java_base_jmod();
        if jmod_path.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmod_path = jmod_path.unwrap();

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);

        // Find java.lang.Object
        let bytes = cp.find_class("java/lang/Object").unwrap();
        // Verify it starts with the class file magic
        assert_eq!(
            &bytes[..4],
            &[0xCA, 0xFE, 0xBA, 0xBE],
            "java.lang.Object should start with CAFEBABE"
        );
        // Check class file version is reasonable (major >= 45 for any JDK)
        let major = u16::from_be_bytes([bytes[6], bytes[7]]);
        assert!(
            major >= 45 && major <= 80,
            "class file major version {major} out of expected range"
        );
        eprintln!(
            "java.lang.Object: {} bytes, class file version {major}",
            bytes.len()
        );

        // Find java.util.HashMap
        let bytes = cp.find_class("java/util/HashMap").unwrap();
        assert_eq!(&bytes[..4], &[0xCA, 0xFE, 0xBA, 0xBE]);

        // Non-existent class should fail
        assert!(cp.find_class("java/lang/NonExistent").is_err());
    }

    #[test]
    #[ignore] // requires JDK on host
    fn list_jmod_modules_returns_module_names() {
        let jmod_path = find_java_base_jmod();
        if jmod_path.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmod_path = jmod_path.unwrap();

        // Load just java.base
        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        let modules = cp.list_jmod_modules();
        assert_eq!(modules, vec!["java.base"]);

        // Load all jmods from the JDK
        let jmods_dir = jmod_path.parent().unwrap();
        let mut all_jmods: Vec<String> = fs::read_dir(jmods_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jmod"))
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        all_jmods.sort();

        let cp = ClassPath::new(&all_jmods);
        let modules = cp.list_jmod_modules();
        assert!(modules.contains(&"java.base".to_string()));
        assert!(modules.contains(&"java.logging".to_string()));
        assert!(
            modules.len() >= 10,
            "Expected at least 10 modules, got {}",
            modules.len()
        );
        eprintln!(
            "Found {} modules: {:?}",
            modules.len(),
            &modules[..5.min(modules.len())]
        );
    }

    #[test]
    fn load_jmod_rejects_bad_magic() {
        let dir = std::env::temp_dir().join("cratonvm_test_bad_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("bad.jmod");

        // Write a file with wrong magic
        fs::write(&path, b"BAAD\x00\x00\x00\x00").unwrap();
        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("magic"), "Error should mention magic: {err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_jmod_rejects_unsupported_major_version() {
        let dir = std::env::temp_dir().join("cratonvm_test_future_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("future.jmod");

        // Write JMOD header with major version 99 (unsupported)
        // followed by minimal ZIP data (will fail ZIP parse, but version check comes first)
        let mut data = vec![0x4A, 0x4D, 99, 0x00]; // JM + major=99 + minor=0
        data.extend_from_slice(&[0; 100]); // padding
        fs::write(&path, &data).unwrap();

        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("major version"),
            "Error should mention major version: {err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_jmod_rejects_too_small_file() {
        let dir = std::env::temp_dir().join("cratonvm_test_tiny_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("tiny.jmod");

        fs::write(&path, b"JM").unwrap(); // Only 2 bytes
        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("too small"),
            "Error should mention file too small: {err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jmod_class_count_empty_classpath() {
        let cp = ClassPath::new(&[]);
        assert_eq!(cp.jmod_class_count(), 0);
    }

    #[test]
    fn list_jmod_modules_empty_classpath() {
        let cp = ClassPath::new(&[]);
        assert!(cp.list_jmod_modules().is_empty());
    }

    // -- ManifestInfo parsing tests --

    #[test]
    fn manifest_multi_release_true() {
        let data = b"Multi-Release: true\nMain-Class: com.example.Main\n";
        let info = ManifestInfo::parse(data);
        assert!(info.multi_release);
        assert_eq!(info.main_class.as_deref(), Some("com.example.Main"));
    }

    #[test]
    fn manifest_multi_release_false_by_default() {
        let data = b"Main-Class: com.example.Main\n";
        let info = ManifestInfo::parse(data);
        assert!(!info.multi_release);
    }

    #[test]
    fn manifest_multi_release_case_insensitive() {
        let data = b"Multi-Release: TRUE\n";
        let info = ManifestInfo::parse(data);
        assert!(info.multi_release);
    }

    #[test]
    fn manifest_classpath_not_dropped_when_large() {
        let mut entries = Vec::new();
        for i in 0..1200 {
            entries.push(format!("file:/C:/repo/lib/dep{i}.jar"));
        }
        let cp = entries.join(" ");
        let raw = format!("Manifest-Version: 1.0\nClass-Path: {cp}\n");
        let info = ManifestInfo::parse(raw.as_bytes());
        assert!(
            info.class_path.is_some(),
            "large Class-Path must be preserved"
        );
    }

    #[test]
    fn manifest_classpath_file_uri_windows_drive() {
        let data = b"Class-Path: file:/C:/Users/dev/.m2/repository/x/y.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/booter.jar"));
        assert_eq!(cp.len(), 1);
        let norm = cp[0].replace('\\', "/");
        assert_eq!(norm, "C:/Users/dev/.m2/repository/x/y.jar");
    }

    #[test]
    fn manifest_classpath_relative_entries_still_resolve_against_jar_dir() {
        let data = b"Class-Path: lib/a.jar ../shared/b.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/booter.jar"));
        assert_eq!(cp.len(), 2);
        assert!(cp[0].replace('\\', "/").ends_with("/tmp/boot/lib/a.jar"));
        // `..` folded as `new URL(base, token)` folds it (wave 108, L5).
        assert!(cp[1].replace('\\', "/").ends_with("/tmp/shared/b.jar"), "{cp:?}");
    }

    /// Interpreter round i5 wave 106, lane L5: a relative token is a URL, so
    /// its `%XX` escapes are decoded before the join (HotSpot's
    /// `FileURLMapper` opens `lib dir/dep.jar` for `lib%20dir/dep.jar`).
    #[test]
    fn manifest_classpath_relative_entries_are_percent_decoded() {
        let data = b"Class-Path: lib%20dir/dep.jar plain.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/booter.jar"));
        assert_eq!(cp.len(), 2);
        assert!(cp[0].replace('\\', "/").ends_with("/tmp/boot/lib dir/dep.jar"), "{cp:?}");
        assert!(cp[1].replace('\\', "/").ends_with("/tmp/boot/plain.jar"), "{cp:?}");
    }

    /// Interpreter round i5 wave 107, lane L5: JDK 25 `URLClassPath` skips a
    /// whole jar whose `Class-Path` has a token with an unknown URL scheme (a
    /// drive spelling), and ignores a token with a known non-`file` scheme.
    #[test]
    fn manifest_classpath_unknown_scheme_drops_the_jar() {
        for drops in [
            "Class-Path: dep.jar C:/x.jar\n",
            "Class-Path: c:\\x.jar\n",
            "Class-Path: url:Z:/x.jar\n",
            "Class-Path: foo+bar:baz.jar\n",
        ] {
            assert!(ManifestInfo::parse(drops.as_bytes()).class_path_drops_jar(), "{drops}");
        }
        for keeps in [
            "Class-Path: dep.jar lib/a:b.jar 1:x.jar\n",
            "Class-Path: file:/C:/x.jar http://example.invalid/y.jar\n",
            "Class-Path: jar:file:/x.jar!/ mailto:a@b #c:d\n",
            "Manifest-Version: 1.0\n",
        ] {
            assert!(!ManifestInfo::parse(keeps.as_bytes()).class_path_drops_jar(), "{keeps}");
        }
        let info = ManifestInfo::parse(b"Class-Path: http://example.invalid/y.jar dep.jar\n");
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/main.jar"));
        assert_eq!(cp.len(), 1, "{cp:?}");
        assert!(cp[0].replace('\\', "/").ends_with("/tmp/boot/dep.jar"), "{cp:?}");
        assert_eq!(super::manifest_token_protocol("C:/x.jar").as_deref(), Some("c"));
        assert_eq!(super::manifest_token_protocol("lib/a:b.jar"), None);
    }

    /// Interpreter round i5 wave 108, lane L5: JDK 25's lazy application
    /// `URLClassPath` with URL handler provider lookups
    /// (`simulate_url_class_path`), measured on HotSpot 25.0.3 by
    /// `L5W107ClassPathDriveToken`: `main.jar` (`Class-Path: bad.jar
    /// good.jar`), `bad.jar` (`dep.jar C:/nowhere/x.jar`), `good.jar`
    /// (`http://example.invalid/y.jar`) ends with `main.jar` alone and one
    /// circularity `Error`.
    #[test]
    fn url_class_path_provider_lookups_follow_the_jdk() {
        use super::{simulate_url_class_path, ClassPathToken, UrlClassPathModel};
        fn run(
            top: &[&str],
            manifests: &[(&str, Vec<ClassPathToken>)],
        ) -> UrlClassPathModel {
            let top: Vec<PathBuf> = top.iter().map(PathBuf::from).collect();
            let mut tokens_of = |path: &Path| -> Option<Vec<ClassPathToken>> {
                manifests
                    .iter()
                    .find(|(name, _)| Path::new(name) == path)
                    .map(|(_, tokens)| tokens.clone())
            };
            simulate_url_class_path(&top, &mut tokens_of)
        }
        let file = |s: &str| ClassPathToken::File(s.to_string());
        let scheme = |s: &str| ClassPathToken::Scheme(s.to_string());
        let probe = [
            ("main.jar", vec![file("bad.jar"), file("good.jar")]),
            ("bad.jar", vec![file("dep.jar"), scheme("c")]),
            ("dep.jar", vec![]),
            ("good.jar", vec![scheme("http")]),
        ];
        let model = run(&["main.jar"], &probe);
        assert_eq!(model.order, vec![PathBuf::from("main.jar")]);
        assert_eq!(model.circularity_errors, 1);
        // The URLClassLoader rows: no circularity over a loader of its own.
        let model = run(&["bad.jar"], &probe);
        assert_eq!((model.order.len(), model.circularity_errors), (0, 0));
        let model = run(&["good.jar"], &probe);
        assert_eq!(model.order, vec![PathBuf::from("good.jar")]);
        // A known scheme alone: the jar goes AFTER the rest, then its own
        // `Class-Path`.
        let reorder = [
            ("a.jar", vec![scheme("http"), file("x.jar")]),
            ("b.jar", vec![]),
            ("x.jar", vec![]),
        ];
        let model = run(&["a.jar", "b.jar"], &reorder);
        let names: Vec<_> = model.order.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["b.jar", "a.jar", "x.jar"]);
        assert_eq!((model.circularity_errors, model.provider_lookups), (0, 1));
        // An unknown scheme with nothing after it: only the jar is dropped
        // (the wave-107 rule).
        let model = run(&["bad.jar", "good.jar"], &[("bad.jar", vec![scheme("c")]), ("good.jar", vec![])]);
        assert_eq!(model.order, vec![PathBuf::from("good.jar")]);
        assert_eq!(model.circularity_errors, 0);
        // `jar:` and `jrt:` never look a provider up.
        let model = run(&["a.jar"], &[("a.jar", vec![scheme("jar"), scheme("jrt")])]);
        assert_eq!((model.order.len(), model.provider_lookups), (1, 0));
    }

    /// Wave 108, lane L5 (`L5W108ClassPathDotSegments`): `dep.jar`,
    /// `sub/../dep.jar` and `./dep.jar` are one URL to the JDK, so one entry.
    #[test]
    fn manifest_classpath_dot_segments_fold_like_a_url() {
        let info = ManifestInfo::parse(b"Class-Path: dep.jar sub/../dep.jar ./dep.jar ../../../up.jar\n");
        let cp = info.resolve_class_path(Path::new("out/x/main.jar"));
        let cp: Vec<String> = cp.iter().map(|s| s.replace('\\', "/")).collect();
        assert_eq!(cp[0], "out/x/dep.jar");
        assert_eq!(cp[1], "out/x/dep.jar");
        assert_eq!(cp[2], "out/x/dep.jar");
        assert_eq!(cp[3], "../up.jar");
        assert_eq!(
            super::url_path_normalized(PathBuf::from("a/b.jar")),
            PathBuf::from("a/b.jar")
        );
    }

    #[test]
    fn manifest_classpath_continuation_lines_preserve_entries() {
        // 72-column style split: first line ends with a space before the break;
        // continuation line begins with one marker space then the next path.
        let raw = concat!(
            "Manifest-Version: 1.0\n",
            "Class-Path: file:/C:/repo/lib/one.jar \n",
            " file:/C:/repo/lib/two.jar\n",
            "\n"
        );
        let info = ManifestInfo::parse(raw.as_bytes());
        let cp = info.resolve_class_path(Path::new("C:/tmp/booter.jar"));
        assert_eq!(cp.len(), 2, "expected two jars, got {:?}", cp);
        assert!(cp[0].replace('\\', "/").contains("/repo/lib/one.jar"));
        assert!(cp[1].replace('\\', "/").contains("/repo/lib/two.jar"));
    }

    #[test]
    fn manifest_classpath_mid_token_continuation() {
        let raw = concat!(
            "Class-Path: file:/C:/repo/lib/prefix-\n",
            " suffix.jar\n",
            "\n"
        );
        let info = ManifestInfo::parse(raw.as_bytes());
        let cp = info.resolve_class_path(Path::new("C:/tmp/booter.jar"));
        assert_eq!(cp.len(), 1);
        assert!(cp[0].replace('\\', "/").contains("prefix-suffix.jar"));
    }

    /// Optional fixture: copy a Surefire booter `META-INF/MANIFEST.MF` to
    /// `CratonVM/target/booter-extract/MANIFEST.MF` to validate folding on a
    /// real multi-kilobyte Class-Path header.
    #[test]
    fn manifest_parse_optional_surefire_booter_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/booter-extract/META-INF/MANIFEST.MF");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(&path).expect("read fixture");
        let info = ManifestInfo::parse(&data);
        let cp = info.class_path.expect("Class-Path");
        assert!(
            cp.contains("junit-platform-launcher-1.10.1.jar"),
            "merged Class-Path should include junit launcher; len={}",
            cp.len()
        );
        assert!(
            !cp.contains(".././"),
            "malformed .././ segment after fold: {cp:?}"
        );
    }

    // -- Multi-release JAR tests --

    #[test]
    fn multi_release_jar_prefers_versioned_class() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // MANIFEST.MF with Multi-Release
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Base class
        zip.start_file("com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        // Versioned class (Java 11)
        zip.start_file("META-INF/versions/11/com/example/Hello.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v11").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Hello").unwrap();
        // Should get the versioned class (v11), not the base.
        assert_eq!(&data[4..], b"_v11");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_release_jar_highest_version_wins() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar_highest");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr2.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Base class
        zip.start_file("com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        // Java 11 version
        zip.start_file("META-INF/versions/11/com/example/Foo.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v11").unwrap();

        // Java 17 version
        zip.start_file("META-INF/versions/17/com/example/Foo.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v17").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Foo").unwrap();
        // Should get the highest available version (v17), since our VM targets Java 25.
        assert_eq!(&data[4..], b"_v17");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Interpreter round i5 wave 100, lane L5: the versioned view is the
    /// booted library's (`set_multi_release_feature`), clamped to 8..=25.
    #[test]
    fn i100_multi_release_jar_is_read_at_the_booted_feature_version() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar_i100_feature");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr3.jar");
        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();
        for (name, body) in [
            ("p/V.class", &b"\xCA\xFE\xBA\xBE_base"[..]),
            ("META-INF/versions/21/p/V.class", &b"\xCA\xFE\xBA\xBE_v21"[..]),
            ("META-INF/versions/25/p/V.class", &b"\xCA\xFE\xBA\xBE_v25"[..]),
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();

        let at = |feature: Option<u32>| {
            let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
            if let Some(feature) = feature {
                cp.set_multi_release_feature(feature);
            }
            let bytes = cp.find_class("p/V").unwrap();
            (cp.multi_release_feature(), bytes[4..].to_vec())
        };
        assert_eq!(at(None), (25, b"_v25".to_vec()));
        assert_eq!(at(Some(21)), (21, b"_v21".to_vec()));
        assert_eq!(at(Some(17)), (17, b"_base".to_vec()));
        assert_eq!(at(Some(99)), (25, b"_v25".to_vec()));
        // Wave 103: 8 (`JarFile.BASE_VERSION_FEATURE`) serves the base entry only.
        assert_eq!(at(Some(8)), (8, b"_base".to_vec()));
        assert_eq!(at(Some(5)).0, 8);

        // `JarFile.<clinit>`'s `jdk.util.jar.version` / `enableMultiRelease` rule.
        assert_eq!(ClassPath::jar_runtime_version(25, None, None), 25);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("21"), None), 21);
        assert_eq!(ClassPath::jar_runtime_version(21, Some("25"), None), 21);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("7"), None), 8);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("-3"), None), 8);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("x"), None), 25);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("21"), Some("false")), 8);
        assert_eq!(ClassPath::jar_runtime_version(25, None, Some("force")), 25);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Interpreter round i5 wave 103, lane L5: the fast-define gate's name index
    /// counts a versioned class only under a version `JarFile.getEntry` searches
    /// at the class path's feature version (9..=feature, canonical decimal).
    #[test]
    fn i103_fast_absent_gate_honours_the_multi_release_feature() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar_i103_gate");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr_gate.jar");
        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();
        for name in [
            "p/V.class",
            "META-INF/versions/21/q/At21.class",
            "META-INF/versions/25/q/At25.class",
            "META-INF/versions/x/q/Odd.class",
            "META-INF/versions/021/q/Padded.class",
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
        }
        zip.finish().unwrap();

        let absent = |feature: u32, name: &str| {
            let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
            cp.set_multi_release_feature(feature);
            cp.fast_absent_ok(name)
        };
        assert!(!absent(25, "p/V"));
        assert!(!absent(25, "q/At25"));
        assert!(!absent(25, "q/At21"));
        assert!(absent(21, "q/At25"));
        assert!(!absent(21, "q/At21"));
        assert!(absent(17, "q/At21"));
        assert!(absent(25, "q/Odd"));
        assert!(absent(25, "q/Padded"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Interpreter round i5 wave 104, lane L5: JDK 25's `getVersionedEntry`
    /// walks the present versions down to 8 (`BASE_VERSION_FEATURE`), so a
    /// `META-INF/versions/8/` entry is served at any version above 8 and
    /// never at 8 itself; versions below 8 and `META-INF/` names are never
    /// versioned.
    #[test]
    fn i104_multi_release_versions_8_is_served_above_the_base_version() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar_i104_v8");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr_v8.jar");
        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();
        for (name, body) in [
            ("p/V.class", &b"\xCA\xFE\xBA\xBE_base"[..]),
            ("META-INF/versions/8/p/V.class", &b"\xCA\xFE\xBA\xBE_v8"[..]),
            ("p/W.class", &b"\xCA\xFE\xBA\xBE_base"[..]),
            ("META-INF/versions/7/p/W.class", &b"\xCA\xFE\xBA\xBE_v7"[..]),
            ("META-INF/versions/8/q/Only8.class", &b"\xCA\xFE\xBA\xBE_v8"[..]),
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();

        let at = |feature: u32, name: &str| {
            let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
            cp.set_multi_release_feature(feature);
            cp.find_class(name).ok().map(|b| b[4..].to_vec())
        };
        assert_eq!(at(25, "p/V"), Some(b"_v8".to_vec()));
        assert_eq!(at(9, "p/V"), Some(b"_v8".to_vec()));
        assert_eq!(at(8, "p/V"), Some(b"_base".to_vec()));
        assert_eq!(at(25, "p/W"), Some(b"_base".to_vec()));
        assert_eq!(at(25, "q/Only8"), Some(b"_v8".to_vec()));
        assert_eq!(at(8, "q/Only8"), None);

        let absent = |feature: u32, name: &str| {
            let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
            cp.set_multi_release_feature(feature);
            cp.fast_absent_ok(name)
        };
        assert!(!absent(25, "q/Only8"));
        assert!(absent(8, "q/Only8"));

        assert_eq!(versioned_lookup_range("p/V.class", 25), 8..26);
        assert_eq!(versioned_lookup_range("p/V.class", 8), 0..0);
        assert_eq!(versioned_lookup_range("META-INF/services/x", 25), 0..0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_multi_release_jar_ignores_versioned() {
        let dir = std::env::temp_dir().join("cratonvm_test_no_mr");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("no_mr.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // No Multi-Release header
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Main-Class: com.example.Main\n").unwrap();

        zip.start_file("com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        zip.start_file("META-INF/versions/17/com/example/Hello.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v17").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Hello").unwrap();
        // Without Multi-Release flag, should get the base version.
        assert_eq!(&data[4..], b"_base");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_release_jar_falls_back_to_base() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_fallback");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr_fallback.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Only base class (no versioned entry)
        zip.start_file("com/example/Base.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base_only").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Base").unwrap();
        assert_eq!(&data[4..], b"_base_only");

        let _ = fs::remove_dir_all(&dir);
    }

    // =======================================================================
    // NEW-5 — jimage integration
    // =======================================================================

    /// Produce a temp-file path unique to this test run so concurrent
    /// tests don't collide.
    fn jimage_temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cratonvm_jimage_test");
        let _ = fs::create_dir_all(&dir);
        dir.join(format!(
            "{tag}_{}.img",
            std::process::id().wrapping_mul(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0)
            )
        ))
    }

    fn sample_jimage_resources() -> Vec<(
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
                b"\xCA\xFE\xBA\xBE_jimage_string".as_ref(),
            ),
            (
                "java.base",
                "java/util",
                "HashMap",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_hashmap".as_ref(),
            ),
            (
                "java.desktop",
                "javax/swing",
                "JFrame",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_jframe".as_ref(),
            ),
            (
                "java.base",
                "",
                "module-info",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_base_module_info".as_ref(),
            ),
            (
                "java.desktop",
                "",
                "module-info",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_desktop_module_info".as_ref(),
            ),
        ]
    }

    /// Write a synthetic jimage to disk and load it through
    /// `ClassPath::add_jimage`. The resulting classpath must resolve
    /// every class via `find_class`.
    #[test]
    fn jimage_find_class_round_trip() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("find_class");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).expect("add_jimage");

        // `module-info` is scoped per-module and not resolvable via
        // the flat `find_class(name)` API (see `load_jimage`). All
        // other sample classes round-trip exactly.
        for (_module, parent, base, _ext, bytes) in sample_jimage_resources() {
            if parent.is_empty() && base == "module-info" {
                continue;
            }
            let internal = format!("{parent}/{base}");
            let got = cp.find_class(&internal).expect("class present");
            assert_eq!(got.as_ref(), bytes, "bytes for {internal} must round-trip");
        }

        let _ = fs::remove_file(&path);
    }

    /// The `lib/modules` auto-detection in `ClassPath::new` should pick
    /// up a file literally named `modules` without an extension.
    #[test]
    fn jimage_auto_detected_by_filename() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let dir = std::env::temp_dir().join(format!("cratonvm_jimage_auto_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("modules");
        fs::write(&path, &data).unwrap();

        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
        assert!(
            !cp.is_empty(),
            "ClassPath::new should auto-detect a file named `modules`"
        );
        let bytes = cp
            .find_class("java/lang/String")
            .expect("class present via auto-detected jimage");
        assert!(bytes.starts_with(b"\xCA\xFE\xBA\xBE"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `scan_module_infos` should return a module-info entry for every
    /// distinct module in the jimage.
    #[test]
    fn jimage_scan_module_infos_returns_every_module() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("scan_module_infos");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let infos = cp.scan_module_infos();
        assert_eq!(
            infos.len(),
            2,
            "exactly two modules in the sample jimage ({} found)",
            infos.len()
        );
        // Both should start with the class-file magic. The exact bytes
        // reflect what our sample resources wrote.
        for info in &infos {
            assert!(info.starts_with(b"\xCA\xFE\xBA\xBE"));
        }
        let _ = fs::remove_file(&path);
    }

    /// The class-to-module index exposed via `jimage_class_count` must
    /// reflect every `.class` entry in the image (module-infos count).
    #[test]
    fn jimage_class_count_matches_resources() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("class_count");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        // 3 regular classes (module-info entries are not counted
        // because they're scoped per-module, not global).
        assert_eq!(cp.jimage_class_count(), 3);
        let modules = cp.list_jimage_modules();
        assert_eq!(
            modules,
            vec!["java.base".to_string(), "java.desktop".to_string()]
        );

        let _ = fs::remove_file(&path);
    }

    /// zs s6/rs: the per-package index (default) and the per-class map
    /// (`CRATONVM_JIMAGE_PACKAGE_INDEX=0`) answer every class-path question
    /// alike, including for a class absent from a package the image holds, and
    /// the default arm really is the per-package one.
    /// zt t3/fp: with the switch, an image's `/packages` and `/modules`
    /// directory records are neither resources nor modules; real resources
    /// and classes answer alike on both arms; the kill arm (positive control)
    /// still indexes them.
    #[test]
    fn zt3fp_jimage_directory_records_are_not_resources() {
        let mut resources = sample_jimage_resources();
        resources.push(("java.base", "java/lang", "zt3fp", "dat", b"zt3fp_res".as_ref()));
        resources.push(("packages", "java.lang", "java.base", "", b"\0\0\0\0".as_ref()));
        resources.push(("modules", "java.base", "zt3fp", "dir", b"zt3fp_dir".as_ref()));
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("zt3fp_dir_records");
        fs::write(&path, &data).unwrap();
        let open = |skip: bool| {
            let mut cp = ClassPath::new(&[]);
            let entry = ClassPath::load_jimage_with_opts(&path, true, skip).unwrap();
            push_classpath_entry(&mut cp.entries, entry);
            cp
        };
        let (skip, keep) = (open(true), open(false));
        assert!(
            keep.contains_resource("java.lang/java.base"),
            "positive control"
        );
        assert!(keep.list_jimage_modules().contains(&"packages".to_string()));
        assert!(!skip.contains_resource("java.lang/java.base"));
        assert!(skip.find_resource("java.lang/java.base").is_none());
        assert!(!skip.contains_resource("java.base/zt3fp.dir"));
        for m in ["packages", "modules"] {
            assert!(!skip.list_jimage_modules().contains(&m.to_string()), "{m}");
        }
        for res in ["java/lang/zt3fp.dat", "java/lang/String.class"] {
            assert!(skip.contains_resource(res), "{res}");
            assert_eq!(skip.find_resource(res), keep.find_resource(res), "{res}");
        }
        assert_eq!(skip.jimage_class_count(), keep.jimage_class_count());
        let _ = fs::remove_file(&path);
    }

    /// The index `load_jimage_with_opts` built before p5-cp (every component
    /// string read per entry, in walk order): `(class -> module, resource ->
    /// modules, module names)`. The reference for the offset walk.
    fn p5cp_reference_jimage_index(
        reader: &cratonvm_reader::JImageReader,
        skip_dirs: bool,
    ) -> (
        Vec<(String, String)>,
        FxHashMap<String, Vec<String>>,
        Vec<String>,
    ) {
        let mut classes: FxHashMap<String, String> = FxHashMap::default();
        let mut resources: FxHashMap<String, Vec<String>> = FxHashMap::default();
        let mut module_set: std::collections::BTreeSet<String> = Default::default();
        reader
            .for_each_entry_parts(|module, parent, base, extension| {
                let mut full = String::new();
                for (sep, part) in [('/', module), ('/', parent), ('/', base), ('.', extension)] {
                    if !part.is_empty() {
                        full.push(sep);
                        full.push_str(part);
                    }
                }
                let Some(without_lead) = full.strip_prefix('/') else {
                    return;
                };
                let Some((m, r)) = without_lead.split_once('/') else {
                    return;
                };
                if m.is_empty() || r.is_empty() {
                    return;
                }
                if skip_dirs && ClassPath::is_jimage_directory_record_module(m) {
                    return;
                }
                module_set.insert(m.to_string());
                match r.strip_suffix(".class") {
                    // A `module-info` base is never a class, in any package.
                    Some(c) if c == "module-info" || c.ends_with("/module-info") => {}
                    Some(c) => {
                        classes.insert(c.to_string(), m.to_string());
                    }
                    None => resources
                        .entry(r.to_string())
                        .or_default()
                        .push(m.to_string()),
                }
            })
            .unwrap();
        let mut classes: Vec<(String, String)> = classes.into_iter().collect();
        classes.sort();
        (classes, resources, module_set.into_iter().collect())
    }

    /// p5-cp: the offset walk (memoised module, package, `class` and
    /// `module-info` per string offset) builds exactly the index the
    /// string-per-entry walk did, on both index arms and both directory
    /// record arms: same classes and modules, same resources with their
    /// modules IN WALK ORDER (the first one is the one served), same module
    /// list. The image repeats a resource across modules, has an entry
    /// without a module, a class in the unnamed package and directory records.
    #[test]
    fn p5cp_offset_walk_builds_the_string_walks_index() {
        let mut resources = sample_jimage_resources();
        let extra: [(&str, &str, &str, &str, &[u8]); 9] = [
            ("java.base", "java/lang", "p5cp", "dat", b"p5_a"),
            ("java.desktop", "java/lang", "p5cp", "dat", b"p5_b"),
            ("jdk.zipfs", "java/lang", "p5cp", "dat", b"p5_c"),
            ("java.base", "", "Root", "class", b"p5_root"),
            ("java.base", "META-INF", "p5", "", b"p5_noext"),
            ("", "java.base", "Loose", "class", b"p5_loose"),
            ("packages", "java.lang", "java.base", "", b"\0\0\0\0"),
            ("modules", "java.base", "p5", "dir", b"p5_dir"),
            ("java.base", "java/lang", "module-info", "class", b"p5_mi"),
        ];
        resources.extend(extra);
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("p5cp_offset_walk");
        fs::write(&path, &data).unwrap();
        let reader = ClassPath::open_jimage_reader(&path).unwrap();
        for skip in [true, false] {
            let (want_classes, want_resources, want_modules) =
                p5cp_reference_jimage_index(&reader, skip);
            assert!(want_resources["java/lang/p5cp.dat"].len() == 3);
            for by_package in [true, false] {
                let ClassPathEntry::JImageFile {
                    reader: r,
                    class_to_module,
                    resource_to_modules,
                    module_names,
                    ..
                } = ClassPath::load_jimage_with_opts(&path, by_package, skip).unwrap()
                else {
                    panic!("not a jimage entry");
                };
                let arm = format!("skip={skip} by_package={by_package}");
                assert_eq!(resource_to_modules, want_resources, "{arm}");
                assert_eq!(module_names, want_modules, "{arm}");
                assert_eq!(class_to_module.class_count(), want_classes.len(), "{arm}");
                let mut names = class_to_module.class_names(&r);
                names.sort();
                let want_names: Vec<String> = want_classes.iter().map(|c| c.0.clone()).collect();
                assert_eq!(names, want_names, "{arm}");
                for (class, module) in &want_classes {
                    assert_eq!(
                        class_to_module.candidate_module(class),
                        Some(module.as_str()),
                        "{arm} {class}"
                    );
                }
            }
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn zs6rs_jimage_package_index_answers_as_the_class_map() {
        let mut resources = sample_jimage_resources();
        resources.push((
            "java.base",
            "",
            "Unnamed",
            "class",
            b"zs6rs_unnamed".as_ref(),
        ));
        resources.push((
            "java.base",
            "java/lang",
            "module-info",
            "class",
            b"zs6rs_mi".as_ref(),
        ));
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("zs6rs_package_index");
        fs::write(&path, &data).unwrap();
        let open = |by_package: bool| {
            let mut cp = ClassPath::new(&[]);
            let entry = ClassPath::load_jimage_with(&path, by_package).unwrap();
            if let ClassPathEntry::JImageFile {
                class_to_module, ..
            } = &entry
            {
                assert_eq!(
                    matches!(class_to_module, JImageClassIndex::Packages { .. }),
                    by_package
                );
            }
            push_classpath_entry(&mut cp.entries, entry);
            cp
        };
        let (pk, cl) = (open(true), open(false));
        assert_eq!(pk.jimage_class_count(), cl.jimage_class_count());
        let sorted = |cp: &ClassPath| {
            let mut v = cp.list_class_names();
            v.sort();
            v
        };
        assert_eq!(sorted(&pk), sorted(&cl));
        for name in [
            "java/lang/String",
            "java/util/HashMap",
            "javax/swing/JFrame",
            "Unnamed",
            // Absent, in a package the image holds / does not hold.
            "java/lang/Missing",
            "javax/swing/Missing",
            "com/example/Absent",
            "module-info",
            "java/lang/module-info",
        ] {
            assert_eq!(
                pk.runtime_image_defines_class(name),
                cl.runtime_image_defines_class(name),
                "{name}"
            );
            assert_eq!(
                pk.find_class(name).ok().map(|b| b.to_vec()),
                cl.find_class(name).ok().map(|b| b.to_vec()),
                "{name}"
            );
            let res = format!("{name}.class");
            assert_eq!(
                pk.contains_resource(&res),
                cl.contains_resource(&res),
                "{res}"
            );
            assert_eq!(
                pk.find_all_resource_urls(&res),
                cl.find_all_resource_urls(&res),
                "{res}"
            );
        }
        assert_eq!(
            pk.runtime_image_defines_class("java/lang/String"),
            Some(true)
        );
        assert_eq!(
            pk.runtime_image_defines_class("java/lang/Missing"),
            Some(false)
        );
        let _ = fs::remove_file(&path);
    }

    /// zs s6/rs: the real JDK image (`$JAVA_HOME/lib/modules`) indexed both
    /// ways gives the same answer for every class it holds and for misses, and
    /// the default arm is the per-package one (no split package in a JDK).
    /// Ignored by default (needs `JAVA_HOME`); prints the per-arm resident
    /// estimate of the index.
    #[test]
    #[ignore]
    fn zs6rs_real_jdk_image_package_index_matches_the_class_map() {
        let Some(home) = cratonvm_types::flags::runtime_var_os("JAVA_HOME") else {
            eprintln!("zs6rs: JAVA_HOME unset, skipped");
            return;
        };
        let path = std::path::PathBuf::from(home).join("lib").join("modules");
        let pk = ClassPath::load_jimage_with(&path, true).unwrap();
        let cl = ClassPath::load_jimage_with(&path, false).unwrap();
        let (
            ClassPathEntry::JImageFile {
                reader: pr,
                class_to_module: pi,
                ..
            },
            ClassPathEntry::JImageFile {
                class_to_module: ci,
                ..
            },
        ) = (&pk, &cl)
        else {
            panic!("not jimage entries");
        };
        let JImageClassIndex::Packages {
            modules, packages, ..
        } = pi
        else {
            panic!("the JDK image fell back to the per-class map");
        };
        let JImageClassIndex::Classes(map) = ci else {
            panic!("kill arm is not the per-class map");
        };
        assert_eq!(pi.class_count(), map.len());
        let mut names = pi.class_names(pr);
        names.sort();
        let mut want: Vec<String> = map.keys().cloned().collect();
        want.sort();
        assert_eq!(names, want);
        for (class, module) in map {
            assert_eq!(pi.candidate_module(class), Some(module.as_str()), "{class}");
            assert!(pi.contains(pr, class), "{class}");
        }
        for miss in [
            "java/lang/NoSuchClassZs6",
            "com/example/NoSuch",
            "module-info",
            "NoPackage",
        ] {
            assert!(!pi.contains(pr, miss), "{miss}");
            assert!(!ci.contains(pr, miss), "{miss}");
        }
        let key_bytes: usize = map.iter().map(|(k, v)| k.len() + v.len()).sum();
        eprintln!(
            "zs6rs-index classes={} packages={} modules={} class_map_capacity={} \
             class_map_string_bytes={key_bytes}",
            map.len(),
            packages.len(),
            modules.len(),
            map.capacity()
        );
    }

    /// The s6 builder's module numbering (a linear search per entry), kept
    /// as the reference for zs s9/rs2b's name map: `(modules, packages,
    /// classes, split, string compares the linear search made)`.
    fn zs9rs2b_reference_index(
        reader: &cratonvm_reader::JImageReader,
    ) -> (Vec<String>, Vec<(String, u32)>, usize, bool, usize) {
        let mut modules: Vec<String> = Vec::new();
        let mut packages: FxHashMap<String, u32> = FxHashMap::default();
        let (mut last, mut classes, mut split, mut compares) = (None::<u32>, 0, false, 0);
        reader
            .for_each_entry_parts(|module, parent, base, extension| {
                let Some((m, c)) = jimage_class_entry(module, parent, base, extension) else {
                    return;
                };
                if split {
                    return;
                }
                let id = match last {
                    Some(id) if modules[id as usize] == m => id,
                    _ => {
                        let found = modules.iter().position(|x| {
                            compares += 1;
                            *x == m
                        });
                        let id = match found {
                            Some(i) => i as u32,
                            None => {
                                modules.push(m.clone());
                                (modules.len() - 1) as u32
                            }
                        };
                        last = Some(id);
                        id
                    }
                };
                classes += 1;
                let package = jimage_package_of(&c).to_string();
                match packages.get(&package) {
                    Some(&have) if have != id => split = true,
                    Some(_) => {}
                    None => {
                        packages.insert(package, id);
                    }
                }
            })
            .unwrap();
        let mut packages: Vec<(String, u32)> = packages.into_iter().collect();
        packages.sort();
        (modules, packages, classes, split, compares)
    }

    /// The new builder's result in the reference's shape, plus its
    /// `module_lookups`.
    fn zs9rs2b_new_index(
        reader: &cratonvm_reader::JImageReader,
    ) -> (Vec<String>, Vec<(String, u32)>, usize, bool, usize) {
        let mut b = JImageIndexBuilder::new(true, 0);
        reader
            .for_each_entry_parts(|module, parent, base, extension| {
                if let Some((m, c)) = jimage_class_entry(module, parent, base, extension) {
                    b.add(&c, &m);
                }
            })
            .unwrap();
        let lookups = b.module_lookups;
        let (split, modules) = (b.split, b.modules.iter().map(|m| m.to_string()).collect());
        let mut packages: Vec<(String, u32)> = b
            .packages
            .iter()
            .map(|(k, &v)| (k.to_string(), v))
            .collect();
        packages.sort();
        (modules, packages, b.filed, split, lookups)
    }

    /// zs s9/rs2b (rv6 X3): the name-map module numbering gives exactly the
    /// linear search's index (same module order, same package -> module ids,
    /// same counts, same split verdict), on a built image.
    #[test]
    fn zs9rs2b_module_map_builds_the_same_index_as_the_linear_search() {
        let mut resources = sample_jimage_resources();
        resources.push((
            "java.desktop",
            "java/awt",
            "Frame",
            "class",
            b"zs9_f".as_ref(),
        ));
        resources.push(("java.base", "java/util", "List", "class", b"zs9_l".as_ref()));
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("zs9rs2b_same_index");
        fs::write(&path, &data).unwrap();
        let reader = ClassPath::open_jimage_reader(&path).unwrap();
        let want = zs9rs2b_reference_index(&reader);
        let got = zs9rs2b_new_index(&reader);
        assert_eq!(
            (&got.0, &got.1, got.2, got.3),
            (&want.0, &want.1, want.2, want.3)
        );
        let _ = fs::remove_file(&path);
    }

    /// zs s9/rs2b: the same on the real JDK image (ignored; needs
    /// `JAVA_HOME`). Prints what the walk cost each way.
    #[test]
    #[ignore]
    fn zs9rs2b_real_jdk_image_module_map_matches_the_linear_search() {
        let Some(home) = cratonvm_types::flags::runtime_var_os("JAVA_HOME") else {
            eprintln!("zs9rs2b: JAVA_HOME unset, skipped");
            return;
        };
        let path = std::path::PathBuf::from(home).join("lib").join("modules");
        let reader = ClassPath::open_jimage_reader(&path).unwrap();
        let t0 = std::time::Instant::now();
        let want = zs9rs2b_reference_index(&reader);
        let t_ref = t0.elapsed();
        let t1 = std::time::Instant::now();
        let got = zs9rs2b_new_index(&reader);
        let t_new = t1.elapsed();
        assert_eq!(
            (&got.0, &got.1, got.2, got.3),
            (&want.0, &want.1, want.2, want.3)
        );
        assert!(!got.3, "a JDK image has no split package");
        eprintln!(
            "zs9rs2b-walk classes={} modules={} linear_compares={} map_lookups={} \
             ref_walk_us={} new_walk_us={}",
            got.2,
            got.0.len(),
            want.4,
            got.4,
            t_ref.as_micros(),
            t_new.as_micros()
        );
    }

    /// zs s6/rs: a package in two modules keeps the per-class map (last entry
    /// wins, as before).
    #[test]
    fn zs6rs_a_split_package_keeps_the_class_map() {
        let resources = vec![
            ("mod.a", "p", "Alpha", "class", b"zs6rs_a".as_ref()),
            ("mod.b", "p", "Beta", "class", b"zs6rs_b".as_ref()),
            ("mod.a", "q", "Gamma", "class", b"zs6rs_c".as_ref()),
        ];
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("zs6rs_split");
        fs::write(&path, &data).unwrap();
        let entry = ClassPath::load_jimage_with(&path, true).unwrap();
        let ClassPathEntry::JImageFile {
            class_to_module, ..
        } = &entry
        else {
            panic!("not a jimage entry");
        };
        assert!(matches!(class_to_module, JImageClassIndex::Classes(_)));
        assert_eq!(class_to_module.candidate_module("p/Alpha"), Some("mod.a"));
        assert_eq!(class_to_module.candidate_module("p/Beta"), Some("mod.b"));
        assert_eq!(class_to_module.class_count(), 3);
        let _ = fs::remove_file(&path);
    }

    /// `list_class_names` should include every class in the jimage.
    #[test]
    fn jimage_list_class_names_includes_every_entry() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("list_class_names");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let mut names = cp.list_class_names();
        names.sort();
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(names.contains(&"java/util/HashMap".to_string()));
        assert!(names.contains(&"javax/swing/JFrame".to_string()));
        // module-info is deliberately excluded from `class_to_module`
        // because it is scoped per-module rather than globally unique.
        assert!(
            !names.contains(&"module-info".to_string()),
            "module-info must not appear as a regular class name"
        );

        let _ = fs::remove_file(&path);
    }

    /// Missing-class lookups against a jimage classpath must return
    /// ClassNotFound and not leak a panic.
    #[test]
    fn jimage_missing_class_returns_clean_error() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("missing");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let err = cp.find_class("no/such/Class").unwrap_err();
        assert!(matches!(err, ClassFileError::ClassNotFound { .. }));

        let _ = fs::remove_file(&path);
    }

    /// `add_jimage` on a non-existent path must return an error, not
    /// a silent no-op.
    #[test]
    fn jimage_add_nonexistent_returns_error() {
        let mut cp = ClassPath::new(&[]);
        let err = cp
            .add_jimage("/nonexistent/cratonvm/jimage/file")
            .unwrap_err();
        assert!(
            err.contains("open jimage"),
            "error message should mention open failure: {err}"
        );
    }

    /// `add_jimage` on a bogus file (not a valid jimage) must return
    /// an error without corrupting the classpath.
    #[test]
    fn jimage_add_bogus_file_returns_error() {
        let path = jimage_temp_path("bogus");
        fs::write(&path, b"this is not a jimage file").unwrap();

        let mut cp = ClassPath::new(&[]);
        let err = cp.add_jimage(&path).unwrap_err();
        assert!(
            err.contains("open jimage") || err.contains("jimage"),
            "error should reference jimage: {err}"
        );
        assert!(cp.is_empty(), "classpath must be unchanged on failure");

        let _ = fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // T19_H10: ManifestInfo.attributes — generic attribute capture so
    // `Class.getPackage().getImplementationVersion()` and friends can read
    // arbitrary `Implementation-*` / `Specification-*` headers without
    // expanding `ManifestInfo` for every new field.
    // -----------------------------------------------------------------------

    #[test]
    fn t19_h10_manifest_attributes_captures_implementation_fields() {
        // Use \n-only line separators (no continuation prefix) so the
        // parser captures every attribute as its own entry.
        let manifest = b"Manifest-Version: 1.0\n\
Implementation-Title: keycloak-common\n\
Implementation-Version: 26.2.4\n\
Implementation-Vendor: Red Hat, Inc.\n\
Specification-Title: Keycloak Common\n\
Specification-Version: 26.2\n\
Specification-Vendor: Keycloak\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.attributes
                .get("Implementation-Title")
                .map(String::as_str),
            Some("keycloak-common")
        );
        assert_eq!(
            info.attributes
                .get("Implementation-Version")
                .map(String::as_str),
            Some("26.2.4")
        );
        assert_eq!(
            info.attributes
                .get("Implementation-Vendor")
                .map(String::as_str),
            Some("Red Hat, Inc.")
        );
        assert_eq!(
            info.attributes
                .get("Specification-Title")
                .map(String::as_str),
            Some("Keycloak Common")
        );
        assert_eq!(
            info.attributes
                .get("Specification-Version")
                .map(String::as_str),
            Some("26.2")
        );
        assert_eq!(
            info.attributes
                .get("Specification-Vendor")
                .map(String::as_str),
            Some("Keycloak")
        );
    }

    #[test]
    fn t19_h10_manifest_attributes_returns_none_for_missing() {
        let manifest = b"Manifest-Version: 1.0\nMain-Class: foo.Bar\n";
        let info = ManifestInfo::parse(manifest);
        assert!(
            info.attributes.get("Implementation-Version").is_none(),
            "absent attribute must yield None"
        );
        assert_eq!(
            info.attributes.get("Manifest-Version").map(String::as_str),
            Some("1.0")
        );
    }

    #[test]
    fn t19_h10_manifest_attributes_skips_per_entry_sections() {
        // The `\n\n` separator ends the main section. Anything after must
        // not contaminate `info.attributes`.
        let manifest = b"Manifest-Version: 1.0\n\
Implementation-Version: 1.0\n\
\n\
Name: foo/Bar.class\n\
Implementation-Version: 999.999\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.attributes
                .get("Implementation-Version")
                .map(String::as_str),
            Some("1.0"),
            "main-section attribute wins; per-entry section is ignored"
        );
        assert!(
            info.attributes.get("Name").is_none(),
            "per-entry `Name` header must not leak into main attributes"
        );
    }

    #[test]
    fn t19_h10_manifest_attributes_caps_oversized_value() {
        // Build a manifest with one valid line + one 16 KiB-value line.
        // The big line must be silently dropped (returned as None) so a
        // hostile signed jar cannot blow our heap.
        let big = "X".repeat(16 * 1024);
        let raw = format!("Manifest-Version: 1.0\nFoo: {big}\nBar: ok\n");
        let info = ManifestInfo::parse(raw.as_bytes());
        assert!(
            info.attributes.get("Foo").is_none(),
            "oversized value must be dropped"
        );
        assert_eq!(
            info.attributes.get("Bar").map(String::as_str),
            Some("ok"),
            "subsequent attributes must still be captured"
        );
    }

    // ── Classpath wildcard expansion (Elasticsearch / Cassandra parity) ──
    //
    // HotSpot's `java -cp lib/*` expands to every `*.jar` directly under
    // `lib/`. Real-world launchers (Elasticsearch, Cassandra, hand-rolled
    // `bin/foo` shell scripts) rely on this; without it `ServiceLoader`
    // sees zero classpath entries and returns an empty iterator.

    /// Build a temp dir with N JARs (valid zips, each containing a
    /// `META-INF/services/dummy.SPI` so `find_all_resource_urls` has
    /// something to enumerate). Returns the dir.
    fn make_jar_dir(name: &str, jar_names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for jar in jar_names {
            let p = dir.join(jar);
            let f = fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/services/dummy.SPI", opts).unwrap();
            zip.write_all(b"com.example.Provider\n").unwrap();
            zip.finish().unwrap();
        }
        dir
    }

    /// `dir/*` must expand to every `.jar` in the directory.
    #[test]
    fn wildcard_expands_dir_star_to_all_jars() {
        let dir = make_jar_dir("cratonvm_wildcard_dir_star", &["a.jar", "b.jar", "c.jar"]);
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 3, "wildcard should expand to 3 jars");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The launcher-facing expansion API must publish concrete paths too.
    /// In-process javac reads `java.class.path` and cannot resolve a literal
    /// `lib/*` token on its own.
    #[test]
    fn wildcard_publication_expands_to_concrete_sorted_jars() {
        let dir = make_jar_dir(
            "cratonvm_wildcard_property_publication",
            &["z.jar", "a.jar", "m.jar"],
        );
        let pattern = format!("{}/*", dir.to_string_lossy());
        let expanded = ClassPath::expand_classpath_entries(&[pattern]);
        let names: Vec<_> = expanded
            .iter()
            .map(|path| {
                Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["a.jar", "m.jar", "z.jar"]);
        assert!(expanded.iter().all(|path| !path.contains('*')));
        let _ = fs::remove_dir_all(&dir);
    }

    /// All wildcard-expanded jars must be visible to
    /// `find_all_resource_urls` — this is the load-bearing claim for
    /// ServiceLoader. Before the fix this returned an empty Vec because
    /// the literal `lib/*` entry was silently dropped as a non-existent
    /// path.
    #[test]
    fn wildcard_makes_every_jar_searchable_for_services() {
        let dir = make_jar_dir(
            "cratonvm_wildcard_services",
            &["alpha.jar", "beta.jar", "gamma.jar"],
        );
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        let urls = cp.find_all_resource_urls("META-INF/services/dummy.SPI");
        assert_eq!(
            urls.len(),
            3,
            "every wildcard-expanded jar must contribute its META-INF/services entry"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Backslash form `dir\*` (Windows-style) must work identically.
    #[test]
    fn wildcard_accepts_backslash_form() {
        let dir = make_jar_dir("cratonvm_wildcard_backslash", &["x.jar", "y.jar"]);
        let pattern = format!("{}\\*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Non-jar files in the wildcard directory are ignored.
    #[test]
    fn wildcard_skips_non_jar_files() {
        let dir = make_jar_dir("cratonvm_wildcard_skips_non_jar", &["good.jar"]);
        fs::write(dir.join("README.txt"), b"hello").unwrap();
        fs::write(dir.join("native.dll"), [0u8; 8]).unwrap();
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 1, "only `.jar` files must be picked up");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `*.JAR` (upper-case extension) is accepted — Windows is
    /// case-insensitive and real-world launchers occasionally ship
    /// `Some-Lib.JAR`.
    #[test]
    fn wildcard_accepts_uppercase_jar_extension() {
        let dir = make_jar_dir("cratonvm_wildcard_uppercase", &["LIB.JAR"]);
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `add_path` (dynamic loading via URLClassLoader / module loader)
    /// honours the same wildcard so app servers that synthesise `lib/*`
    /// strings at runtime benefit.
    #[test]
    fn wildcard_works_via_add_path() {
        let dir = make_jar_dir("cratonvm_wildcard_add_path", &["one.jar", "two.jar"]);
        let pattern = format!("{}/*", dir.to_string_lossy());
        let mut cp = ClassPath::new(&[]);
        cp.add_path(&pattern);
        assert_eq!(cp.entry_count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Non-wildcard entries pass through unchanged — `foo.jar` must
    /// not be treated as a wildcard pattern (regression guard against
    /// false-positive expansion).
    #[test]
    fn wildcard_passthrough_for_plain_jar() {
        let dir = make_jar_dir("cratonvm_wildcard_passthrough", &["a.jar"]);
        let plain = dir.join("a.jar").to_string_lossy().into_owned();
        let cp = ClassPath::new(&[plain]);
        assert_eq!(cp.entry_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Explicit URLClassLoader entries are archive URLs, not Java launcher
    /// wildcard entries: a valid ZIP must remain searchable even when its
    /// extension is `.par` rather than `.jar`.
    #[test]
    fn explicit_non_jar_archive_is_searchable() {
        let dir = make_jar_dir("cratonvm_packaged_archive_suffix", &["payload.jar"]);
        let jar = dir.join("payload.jar");
        let par = dir.join("payload.par");
        fs::rename(&jar, &par).unwrap();

        let cp = ClassPath::new(&[par.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.entry_count(),
            1,
            "the .par ZIP must be admitted as an archive"
        );
        let urls = cp.find_all_resource_urls("META-INF/services/dummy.SPI");
        assert_eq!(
            urls.len(),
            1,
            "the resource inside the .par must be visible"
        );
        assert!(urls[0].contains("payload.par"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Wildcard pointing at a non-existent directory must silently
    /// expand to nothing (matches `java -cp missing/*` behaviour).
    #[test]
    fn wildcard_missing_dir_yields_empty_classpath() {
        let cp = ClassPath::new(&["definitely_does_not_exist_12345/*".to_string()]);
        assert!(cp.is_empty());
    }

    /// Provider ordering must be deterministic across runs. Two
    /// invocations on the same directory must produce the same URL
    /// order — `ServiceLoader` clients can depend on first-match
    /// semantics.
    #[test]
    fn wildcard_yields_deterministic_jar_order() {
        let dir = make_jar_dir(
            "cratonvm_wildcard_order",
            &["zeta.jar", "alpha.jar", "mu.jar"],
        );
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp1 = ClassPath::new(&[pattern.clone()]);
        let cp2 = ClassPath::new(&[pattern]);
        let urls1 = cp1.find_all_resource_urls("META-INF/services/dummy.SPI");
        let urls2 = cp2.find_all_resource_urls("META-INF/services/dummy.SPI");
        assert_eq!(urls1, urls2, "wildcard expansion order must be stable");
        // Sanity: lexicographic — alpha < mu < zeta.
        assert!(urls1[0].contains("alpha.jar"));
        assert!(urls1[1].contains("mu.jar"));
        assert!(urls1[2].contains("zeta.jar"));
        let _ = fs::remove_dir_all(&dir);
    }

    // ── ES2 multi-jar SPI enumeration (Elasticsearch parity) ───────────
    //
    // Elasticsearch's launcher script passes every jar EXPLICITLY (not
    // via `lib/*` wildcard). Only one of those 60+ jars holds the
    // `META-INF/services/org.elasticsearch.cli.CliToolProvider` SPI
    // descriptor. The other 59 must be walked without producing false
    // positives or short-circuiting the search after the first miss.
    //
    // Regression target: an earlier version of `find_all_resource_urls`
    // could short-circuit on `find_in_archive` returning Err (vs Ok(None)),
    // which would cause the walk to abort silently after the first jar
    // that lacked the entry. The current code returns Ok-or-None for
    // every entry and continues, but a unit test pins the contract.

    /// Build N empty jars + 1 jar that holds the SPI descriptor. All N+1
    /// must be walked by `find_all_resource_urls`, and exactly ONE URL
    /// must come back — the one from the lone jar that has the entry.
    /// Mirrors the Elasticsearch `-cp <60 explicit jars>` shape.
    #[test]
    fn explicit_multi_jar_classpath_finds_lone_spi_provider() {
        let dir = std::env::temp_dir().join("cratonvm_es2_multi_jar_spi");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // 19 empty jars (just an empty MANIFEST.MF) + 1 jar with the SPI.
        let mut cp_entries: Vec<String> = Vec::new();
        for i in 0..19 {
            let p = dir.join(format!("empty-{i}.jar"));
            let f = fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zip.write_all(b"Manifest-Version: 1.0\n").unwrap();
            zip.finish().unwrap();
            cp_entries.push(p.to_string_lossy().into_owned());
        }
        // The lone provider jar — modelled after server-cli-8.15.5.jar.
        let provider = dir.join("server-cli-x.y.z.jar");
        {
            let f = fs::File::create(&provider).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file(
                "META-INF/services/org.elasticsearch.cli.CliToolProvider",
                opts,
            )
            .unwrap();
            zip.write_all(b"org.elasticsearch.server.cli.ServerCliProvider\n")
                .unwrap();
            zip.finish().unwrap();
        }
        cp_entries.push(provider.to_string_lossy().into_owned());

        let cp = ClassPath::new(&cp_entries);
        assert_eq!(
            cp.entry_count(),
            20,
            "every explicitly-listed jar must register as a classpath entry"
        );
        let urls =
            cp.find_all_resource_urls("META-INF/services/org.elasticsearch.cli.CliToolProvider");
        assert_eq!(
            urls.len(),
            1,
            "exactly one jar holds the SPI descriptor; walk must not \
             short-circuit on the first miss. Got urls={urls:?}",
        );
        assert!(
            urls[0].contains("server-cli-x.y.z.jar"),
            "the lone hit must point at the provider jar, got {}",
            urls[0]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Same multi-jar walk with `find_all_resource_bytes` — the Rust
    /// helper path used by service_loader.rs that bypasses URL stream
    /// handling. Must produce exactly one byte block matching the SPI
    /// content.
    #[test]
    fn explicit_multi_jar_classpath_finds_lone_spi_bytes() {
        let dir = std::env::temp_dir().join("cratonvm_es2_multi_jar_spi_bytes");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut cp_entries: Vec<String> = Vec::new();
        for i in 0..9 {
            let p = dir.join(format!("nothing-{i}.jar"));
            let f = fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("README", opts).unwrap();
            zip.write_all(b"nothing here\n").unwrap();
            zip.finish().unwrap();
            cp_entries.push(p.to_string_lossy().into_owned());
        }
        let provider = dir.join("provider.jar");
        {
            let f = fs::File::create(&provider).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/services/foo.Bar", opts).unwrap();
            zip.write_all(b"foo.BarImpl\n").unwrap();
            zip.finish().unwrap();
        }
        cp_entries.push(provider.to_string_lossy().into_owned());

        let cp = ClassPath::new(&cp_entries);
        let bytes = cp.find_all_resource_bytes("META-INF/services/foo.Bar");
        assert_eq!(bytes.len(), 1);
        assert_eq!(bytes[0], b"foo.BarImpl\n");
        let _ = fs::remove_dir_all(&dir);
    }

    // -- T1.8.2 follow-up (MED #32): fat-JAR re-open path no longer panics --

    /// Regression test: a malformed/truncated archive must NOT panic via the
    /// previous `.unwrap()` on the fat-JAR re-open path. Feeding raw garbage
    /// fails the very first `ZipArchive::new` and pushes no entries.
    #[test]
    fn load_jar_data_with_truncated_bytes_does_not_panic() {
        // Eight bytes that are not a valid zip central directory.
        let garbage: Vec<u8> = b"\x00\x01\x02\x03\x04\x05\x06\x07".to_vec();
        let mut entries: Vec<ClassPathEntry> = Vec::new();
        let path = Path::new("truncated.jar");

        // The fix removed `.unwrap()` on re-open; the first open also
        // fails here, exercising the broader no-panic contract.
        ClassPath::load_jar_data(path, garbage, &mut entries);

        assert!(
            entries.is_empty(),
            "expected no entries from malformed archive, got {}",
            entries.len()
        );
    }

    /// Regression test: a fat JAR header that contains a Spring-Boot manifest
    /// but is truncated after the central-directory probe must not panic on
    /// the re-open path either. We construct a fat JAR, mangle the trailing
    /// bytes after the in-memory buffer has been built, and confirm load
    /// proceeds without panicking.
    #[test]
    fn fat_jar_corrupted_after_first_open_does_not_panic() {
        // Build a valid fat JAR in memory.
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(cursor);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zip.write_all(
                b"Manifest-Version: 1.0\r\n\
                  Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
                  Start-Class: com.example.App\r\n\
                  Spring-Boot-Classes: BOOT-INF/classes/\r\n\
                  Spring-Boot-Lib: BOOT-INF/lib/\r\n",
            )
            .unwrap();
            zip.start_file("BOOT-INF/classes/com/example/App.class", opts)
                .unwrap();
            zip.write_all(b"\xCA\xFE\xBA\xBE_app").unwrap();
            zip.finish().unwrap();
        }

        // Sanity: a pristine fat JAR loads fine and pushes >=1 entry.
        let mut ok_entries: Vec<ClassPathEntry> = Vec::new();
        ClassPath::load_jar_data(Path::new("ok.jar"), buf.clone(), &mut ok_entries);
        assert!(
            !ok_entries.is_empty(),
            "pristine fat JAR should push at least the BOOT-INF entries"
        );

        // Truncate the buffer mid-central-directory. The first open in
        // `load_jar_data` may still succeed for some inputs, but in either
        // case the re-open path (formerly `.unwrap()`) must not panic.
        let mut truncated = buf.clone();
        if truncated.len() > 32 {
            truncated.truncate(truncated.len() - 32);
        }
        let mut entries: Vec<ClassPathEntry> = Vec::new();
        ClassPath::load_jar_data(Path::new("corrupt.jar"), truncated, &mut entries);
        // No assertion on entry count: the contract is "does not panic".
    }

    // -- VULN (low) manifest Class-Path hardening: `path_is_within` --
    //
    // These exercise the pure lexical containment helper that backs the
    // opt-in `CRATONVM_HARDEN_MANIFEST_CLASSPATH` policy. The env-gated
    // wiring in `resolve_class_path` is not toggled here (the flag latches
    // in a process-wide `OnceLock`), so we test the decision function
    // directly.

    #[test]
    fn path_is_within_accepts_descendants() {
        assert!(path_is_within(
            Path::new("C:/app/lib/dep.jar"),
            Path::new("C:/app")
        ));
        assert!(path_is_within(
            Path::new("C:/app/lib/sub/dep.jar"),
            Path::new("C:/app/lib")
        ));
        // Identical path is "within" itself.
        assert!(path_is_within(Path::new("C:/app"), Path::new("C:/app")));
    }

    #[test]
    fn path_is_within_rejects_dotdot_escape() {
        // `..` that climbs out of the base directory must be rejected.
        assert!(!path_is_within(
            Path::new("C:/app/../secret/x.jar"),
            Path::new("C:/app")
        ));
        assert!(!path_is_within(
            Path::new("C:/app/lib/../../secret"),
            Path::new("C:/app")
        ));
    }

    #[test]
    fn path_is_within_rejects_unrelated_absolute_root() {
        // A sibling/foreign absolute root (e.g. manifest `file:/etc`) is
        // not a descendant of the JAR directory.
        assert!(!path_is_within(Path::new("C:/etc"), Path::new("C:/app")));
        assert!(!path_is_within(
            Path::new("D:/other/x.jar"),
            Path::new("C:/app")
        ));
    }

    #[test]
    fn path_is_within_dot_dot_normalizes_back_inside() {
        // `lib/../lib2` stays inside `C:/app`.
        assert!(path_is_within(
            Path::new("C:/app/lib/../lib2/dep.jar"),
            Path::new("C:/app")
        ));
    }

    #[test]
    fn manifest_classpath_escape_preserved_by_default() {
        // Regression guard for the DEFAULT (HotSpot-parity) behaviour: with
        // the hardening flag OFF, a manifest Class-Path that escapes the
        // JAR directory MUST still resolve (matching
        // `manifest_classpath_relative_entries_still_resolve_against_jar_dir`).
        // This protects the trusted-manifest contract `decode_manifest_classpath_entry`
        // documents from accidental regression.
        let data = b"Class-Path: ../../shared/b.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/booter.jar"));
        assert_eq!(
            cp.len(),
            1,
            "default policy must keep escaping entries: {cp:?}"
        );
        // Folded lexically as the JDK's URL folds it (wave 108, L5): the
        // escape is kept, only its spelling changes.
        assert!(cp[0].replace('\\', "/").ends_with("C:/shared/b.jar"), "{cp:?}");
    }

    // -----------------------------------------------------------------------
    // Lazy JMOD class serving (2026-07-26 boot-classpath-lazy)
    //
    // `load_jmod` used to inflate every `classes/` entry at load time. It now
    // indexes names only and inflates per lookup. These tests pin the three
    // things that change could plausibly break: the bytes, the fall-through to
    // other classpath entries, and the behaviour on a damaged archive.
    // -----------------------------------------------------------------------

    /// The 4-byte JMOD header that precedes the embedded ZIP.
    const TEST_JMOD_HEADER: [u8; 4] = [0x4A, 0x4D, 0x01, 0x00];

    /// Build a JMOD image in memory: `JM\x01\x00` followed by a ZIP holding
    /// `entries` verbatim (names are used exactly as given, so callers
    /// control whether an entry lands under `classes/`).
    ///
    /// Entries are **Deflated**, not Stored, so the lazy path is exercised
    /// against genuinely compressed data — a Stored entry would be served by
    /// the zero-copy `ArchiveSlice` branch and would never prove that
    /// on-demand inflation produces the right bytes.
    fn build_test_jmod_bytes(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let zip_bytes = zip.finish().unwrap().into_inner();

        let mut out = Vec::with_capacity(TEST_JMOD_HEADER.len() + zip_bytes.len());
        out.extend_from_slice(&TEST_JMOD_HEADER);
        out.extend_from_slice(&zip_bytes);
        out
    }

    fn jmod_temp_path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cratonvm_lazy_jmod_{}_{}", std::process::id(), tag));
        let _ = fs::create_dir_all(&dir);
        dir.join(format!("{tag}.jmod"))
    }

    /// A class body big enough that its deflate stream spans many bytes, so
    /// the corruption test below has payload to damage. Deterministic so a
    /// failure is reproducible.
    fn synthetic_class_bytes(seed: u8, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        let mut x = seed as u32 | 1;
        while out.len() < len {
            // xorshift — pseudo-random so the entry does not compress to
            // nothing, but fully reproducible.
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            out.push((x & 0xFF) as u8);
        }
        out
    }

    /// Equivalence with the eager implementation this replaced: for every
    /// `classes/` entry, `find_class` must return exactly the bytes that
    /// pre-extracting the whole archive would have cached.
    ///
    /// The reference side is computed here by replicating the old eager loop
    /// against the same archive, so this compares the two strategies rather
    /// than comparing the new one against a hand-written expectation.
    #[test]
    fn jmod_lazy_lookup_matches_eager_extraction_byte_for_byte() {
        let entries: Vec<(&str, Vec<u8>)> = vec![
            (
                "classes/java/lang/Object.class",
                synthetic_class_bytes(1, 3000),
            ),
            (
                "classes/java/lang/String.class",
                synthetic_class_bytes(2, 5000),
            ),
            (
                "classes/java/util/List.class",
                synthetic_class_bytes(3, 700),
            ),
            ("classes/module-info.class", synthetic_class_bytes(4, 400)),
            (
                "classes/META-INF/services/example.Service",
                b"example.ServiceImpl\n".to_vec(),
            ),
            // Not under `classes/` — a JMOD's build-time payload. Must not be
            // indexed as a class and must not be counted.
            ("legal/java.base/LICENSE", b"license text".to_vec()),
            ("lib/libjava.so", vec![0u8; 64]),
        ];
        let path = jmod_temp_path("equivalence");
        fs::write(&path, build_test_jmod_bytes(&entries)).unwrap();

        // --- reference: what the old eager pre-extraction would have cached
        let raw = fs::read(&path).unwrap();
        let mut reference: HashMap<String, Vec<u8>> = HashMap::new();
        {
            let mut archive = ZipArchive::new(Cursor::new(raw[4..].to_vec())).unwrap();
            for i in 0..archive.len() {
                let name = archive
                    .by_index_raw(i)
                    .unwrap()
                    .name()
                    .unwrap()
                    .into_owned();
                if let Some(relative) = name.strip_prefix("classes/") {
                    if relative.is_empty() || relative.ends_with('/') {
                        continue;
                    }
                    let mut entry = archive.by_name(&name).unwrap();
                    let mut buf = Vec::new();
                    entry.read_to_end(&mut buf).unwrap();
                    reference.insert(relative.to_string(), buf);
                }
            }
        }
        assert_eq!(reference.len(), 5, "five entries live under classes/");

        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);

        // The index must agree with the eager cache on membership...
        assert_eq!(
            cp.jmod_class_count(),
            reference.len(),
            "index must cover exactly the classes/ subtree"
        );

        // ...and on bytes, for every class the eager cache would have held.
        for name in ["java/lang/Object", "java/lang/String", "java/util/List"] {
            let got = cp.find_class(name).expect("class must resolve");
            let want = reference
                .get(&format!("{name}.class"))
                .expect("reference entry");
            assert_eq!(
                got.as_ref(),
                want.as_slice(),
                "lazy bytes for {name} must equal eagerly-extracted bytes"
            );
        }

        // Non-class resources under `classes/` come back too.
        assert_eq!(
            cp.find_resource("META-INF/services/example.Service"),
            Some(b"example.ServiceImpl\n".to_vec())
        );

        // A second lookup returns the same bytes — inflating on demand must
        // be idempotent, not stateful.
        let first = cp.find_class("java/lang/String").unwrap();
        let second = cp.find_class("java/lang/String").unwrap();
        assert_eq!(first.as_ref(), second.as_ref());

        // Entries outside `classes/` are not classes.
        assert!(cp.find_class("legal/java.base/LICENSE").is_err());
        assert!(cp.find_class("java/lang/Missing").is_err());

        // The code source still points at the JMOD, and answering that
        // question never needs the bytes.
        assert_eq!(
            cp.find_class_source_path("java/lang/Object"),
            Some(path.to_string_lossy().into_owned())
        );
        assert_eq!(cp.find_class_source_path("java/lang/Missing"), None);

        let _ = fs::remove_file(&path);
    }

    /// A JMOD and a jimage on the same classpath: a class only the JMOD has
    /// must still resolve. This is the case that makes the "just prefer
    /// `lib/modules`" alternative unsafe, and it is what the fall-through in
    /// `find_class` exists for.
    #[test]
    fn class_only_in_jmod_resolves_when_jimage_is_also_on_the_path() {
        let jimage_data =
            cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let jimage_path = jimage_temp_path("jmod_fallthrough");
        fs::write(&jimage_path, &jimage_data).unwrap();

        let jmod_only = synthetic_class_bytes(9, 2048);
        let jmod_path = jmod_temp_path("fallthrough");
        fs::write(
            &jmod_path,
            build_test_jmod_bytes(&[("classes/com/example/OnlyInJmod.class", jmod_only.clone())]),
        )
        .unwrap();

        // jimage first, JMOD second — the ordering a `lib/modules`-preferring
        // boot classpath would produce.
        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&jimage_path).expect("add_jimage");
        cp.add_path(&jmod_path.to_string_lossy());
        assert_eq!(cp.entry_count(), 2, "both entries must be on the classpath");

        // The jimage-only class resolves through the jimage reader...
        assert_eq!(
            cp.find_class("java/lang/String").unwrap().as_ref(),
            b"\xCA\xFE\xBA\xBE_jimage_string".as_ref()
        );
        // ...and the JMOD-only class resolves through the lazy JMOD path,
        // after the jimage arm has declined it.
        assert_eq!(
            cp.find_class("com/example/OnlyInJmod").unwrap().as_ref(),
            jmod_only.as_slice()
        );

        let _ = fs::remove_file(&jimage_path);
        let _ = fs::remove_file(&jmod_path);
    }

    /// Malformed and truncated JMODs must produce errors, never panics.
    /// `load_jmod` is reached from `ClassPath::new` on any host-supplied
    /// `JAVA_HOME`, so a damaged install must degrade to "class not found".
    #[test]
    fn malformed_jmod_errors_instead_of_panicking() {
        let good = build_test_jmod_bytes(&[(
            "classes/java/lang/Object.class",
            synthetic_class_bytes(7, 4096),
        )]);

        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("shorter_than_header", vec![0x4A, 0x4D, 0x01]),
            ("bad_magic", {
                let mut v = good.clone();
                v[0] = 0x50;
                v[1] = 0x4B;
                v
            }),
            ("future_major_version", {
                let mut v = good.clone();
                v[2] = 0x02;
                v
            }),
            ("header_only", TEST_JMOD_HEADER.to_vec()),
            ("garbage_after_header", {
                let mut v = TEST_JMOD_HEADER.to_vec();
                v.extend_from_slice(&[0xFFu8; 512]);
                v
            }),
            // Valid header, ZIP truncated mid-way: the central directory is
            // gone, so the archive cannot be opened at all.
            ("truncated_zip", good[..good.len() / 2].to_vec()),
        ];

        for (tag, bytes) in cases {
            let path = jmod_temp_path(tag);
            fs::write(&path, &bytes).unwrap();

            // Direct: `load_jmod` reports an error string.
            assert!(
                ClassPath::load_jmod(&path).is_err(),
                "load_jmod({tag}) must return Err, not succeed or panic"
            );

            // Through the public entry point: the damaged file is skipped and
            // lookups fail cleanly.
            let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
            assert!(
                cp.find_class("java/lang/Object").is_err(),
                "find_class on damaged JMOD ({tag}) must return Err"
            );

            let _ = fs::remove_file(&path);
        }
    }

    /// Corruption that the *eager* loader would have absorbed at load time
    /// (the entry simply never entered the cache) now surfaces at lookup
    /// time. It must surface as `ClassNotFound`, not as a panic and not as
    /// silently truncated class bytes.
    ///
    /// The archive itself stays well-formed — only the last entry's
    /// compressed payload is damaged — so `load_jmod` succeeds and the name
    /// is in the index, which is precisely the new lazy-path-only case.
    #[test]
    fn jmod_with_corrupt_entry_payload_reports_class_not_found() {
        let mut bytes = build_test_jmod_bytes(&[(
            "classes/java/lang/Object.class",
            synthetic_class_bytes(11, 65536),
        )]);

        // Locate the central directory via the end-of-central-directory
        // record (no ZIP comment is written, so it is the final 22 bytes).
        let eocd = bytes.len() - 22;
        assert_eq!(
            &bytes[eocd..eocd + 4],
            &[0x50, 0x4B, 0x05, 0x06],
            "expected EOCD signature at end of synthesized JMOD"
        );
        let cd_offset = u32::from_le_bytes([
            bytes[eocd + 16],
            bytes[eocd + 17],
            bytes[eocd + 18],
            bytes[eocd + 19],
        ]) as usize
            + TEST_JMOD_HEADER.len(); // offsets are relative to the ZIP, not the JMOD

        // Damage 256 bytes of the compressed stream, ending just before the
        // central directory. Well clear of the local file header.
        let end = cd_offset;
        let start = end - 256;
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }

        let path = jmod_temp_path("corrupt_payload");
        fs::write(&path, &bytes).unwrap();

        // The archive still parses: the index is built from the central
        // directory, which we did not touch.
        let entry = ClassPath::load_jmod(&path).expect("central directory is intact");
        match &entry {
            ClassPathEntry::JmodFile {
                class_entry_index, ..
            } => assert!(
                class_entry_index.contains("java/lang/Object.class"),
                "the damaged entry is still indexed"
            ),
            other => panic!("expected JmodFile, got {other:?}"),
        }

        // But serving it fails cleanly rather than panicking or returning
        // a partially-inflated class file.
        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
        match cp.find_class("java/lang/Object") {
            Err(ClassFileError::ClassNotFound { .. }) => {}
            Err(other) => panic!("expected ClassNotFound, got {other:?}"),
            Ok(bytes) => panic!(
                "corrupt entry must not resolve; got {} bytes",
                bytes.as_ref().len()
            ),
        }

        let _ = fs::remove_file(&path);
    }
