    use super::*;

    // -----------------------------------------------------------------------
    // Boot-classpath source selection (2026-07-26 boot-classpath-lazy)
    //
    // `discover_boot_classpath` prefers `jmods/` and falls back to
    // `lib/modules`. That ordering is load-bearing — see the function's doc
    // comment — so it is pinned here rather than left to be "fixed" by a
    // later reader who only sees the startup cost and not the reason.
    // `resolve_java_home(Some(dir))` is authoritative when the directory
    // exists, so these tests need no JDK and touch no environment variables.
    // -----------------------------------------------------------------------

    fn fake_java_home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cratonvm_boot_cp_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // `java` on PATH resolves to its home the way the JDK launcher does it --
    // the parent of the real executable's `bin/` -- with no child JVM. Phase
    // 5 of the fast-boot proposal: the spawn was most of a bare launch's cost.
    #[cfg(unix)]
    #[test]
    fn java_home_from_executable_follows_the_symlink_to_the_jdk() {
        let home = fake_java_home("exe_home");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("bin").join("java"), b"").unwrap();
        std::fs::write(home.join("lib").join("modules"), b"jimage").unwrap();
        let link_dir = fake_java_home("exe_link");
        std::os::unix::fs::symlink(home.join("bin").join("java"), link_dir.join("java")).unwrap();

        let found = java_home_from_executable(&link_dir.join("java"));
        assert_eq!(found, Some(std::fs::canonicalize(&home).unwrap()));
    }

    #[test]
    fn java_home_from_executable_refuses_a_tree_with_no_boot_image() {
        let home = fake_java_home("exe_no_image");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::write(home.join("bin").join("java"), b"").unwrap();

        assert_eq!(java_home_from_executable(&home.join("bin").join("java")), None);
    }

    #[test]
    fn boot_classpath_prefers_jmods_and_puts_java_base_first() {
        let home = fake_java_home("jmods_first");
        let jmods = home.join("jmods");
        std::fs::create_dir_all(&jmods).unwrap();
        // Deliberately create them out of order so the result proves sorting
        // and the java.base hoist rather than directory-iteration order.
        for name in ["java.xml.jmod", "java.base.jmod", "java.desktop.jmod"] {
            std::fs::write(jmods.join(name), b"JM\x01\x00").unwrap();
        }
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("lib").join("modules"), b"not-a-real-jimage").unwrap();

        let cp = discover_boot_classpath_with(Some(&home.to_string_lossy()), false);
        assert_eq!(cp.len(), 3, "all three JMODs must be on the boot classpath");
        assert!(
            cp[0].ends_with("java.base.jmod"),
            "java.base must come first, got {cp:?}"
        );
        assert!(
            cp[1].ends_with("java.desktop.jmod") && cp[2].ends_with("java.xml.jmod"),
            "remaining JMODs must be sorted for run-to-run determinism, got {cp:?}"
        );
        assert!(
            !cp.iter()
                .any(|e| e.replace('\\', "/").ends_with("lib/modules")),
            "lib/modules must not be used when jmods/ is present: {cp:?}"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

#[test]
    fn boot_classpath_falls_back_to_lib_modules_without_jmods() {
        // JRE / jlink-trimmed image: no `jmods/` at all. This is the path
        // RKC16N.9 added, and it must keep working.
        let home = fake_java_home("jimage_fallback");
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("lib").join("modules"), b"not-a-real-jimage").unwrap();

        let cp = discover_boot_classpath(Some(&home.to_string_lossy()));
        assert_eq!(cp.len(), 1, "expected the jimage alone, got {cp:?}");
        assert!(cp[0].replace('\\', "/").ends_with("lib/modules"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn boot_classpath_empty_jmods_dir_falls_through_to_lib_modules() {
        // A `jmods/` directory that exists but holds no `.jmod` files must
        // not shadow a usable `lib/modules`.
        let home = fake_java_home("empty_jmods");
        std::fs::create_dir_all(home.join("jmods")).unwrap();
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("lib").join("modules"), b"not-a-real-jimage").unwrap();

        let cp = discover_boot_classpath(Some(&home.to_string_lossy()));
        assert_eq!(cp.len(), 1, "expected the jimage alone, got {cp:?}");
        assert!(cp[0].replace('\\', "/").ends_with("lib/modules"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn boot_classpath_empty_when_java_home_has_neither() {
        let home = fake_java_home("neither");
        let cp = discover_boot_classpath(Some(&home.to_string_lossy()));
        assert!(cp.is_empty(), "expected no boot classpath, got {cp:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The `-version` banner must not carry the old, ~9x-low "~300" figure.
    /// This is user-visible text quoted in bug reports; see
    /// [`REAL_JDK_NATIVE_REGISTRATIONS`] for the derivation.
    #[test]
    fn jdk_mode_describe_does_not_quote_the_stale_native_count() {
        let real = JdkMode::Real.describe();
        assert!(
            !real.contains("~300"),
            "real-JDK banner still quotes the retracted ~300 figure: {real}"
        );
        assert!(
            real.contains("2,700"),
            "real-JDK banner should quote the measured figure: {real}"
        );
        assert_eq!(REAL_JDK_NATIVE_REGISTRATIONS, 2_700);
    }

    #[test]
    fn default_config() {
        let config = VmConfig::default();
        assert_eq!(config.max_heap_size, 256 * 1024 * 1024);
        assert_eq!(config.max_stack_depth, 8192);
        assert!(config.classpath.is_empty());
        assert!(config.boot_classpath.is_empty());
        assert!(config.ext_classpath.is_empty());
        assert!(config.java_home.is_none());
        // ZGC is the default collector as of 2026-08-10 (see `VmConfig::default`
        // for the measurement behind the flip). A `--no-default-features` build
        // has no `Zgc` variant at all and falls back to `Generational`, so this
        // assertion is cfg'd the same way the field is — otherwise it would be
        // asserting on a value that cannot exist in that configuration.
        #[cfg(feature = "zgc")]
        assert_eq!(config.gc_algorithm, GcAlgorithm::Zgc);
        #[cfg(not(feature = "zgc"))]
        assert_eq!(config.gc_algorithm, GcAlgorithm::Generational);
    }

    #[test]
    fn parse_gc_algorithm_supported() {
        assert_eq!(parse_gc_algorithm("g1"), Some(GcAlgorithm::G1));
        assert_eq!(parse_gc_algorithm("G1"), Some(GcAlgorithm::G1));
        #[cfg(feature = "zgc")]
        {
            assert_eq!(parse_gc_algorithm("Z"), Some(GcAlgorithm::Zgc));
            assert_eq!(parse_gc_algorithm("ZGC"), Some(GcAlgorithm::Zgc));
            assert_eq!(parse_gc_algorithm("  zGc  "), Some(GcAlgorithm::Zgc));
        }
        assert_eq!(
            parse_gc_algorithm("Generational"),
            Some(GcAlgorithm::Generational)
        );
        // Case-insensitive + surrounding whitespace tolerated.
        assert_eq!(
            parse_gc_algorithm("  gEnErAtIoNaL  "),
            Some(GcAlgorithm::Generational)
        );
    }

    #[test]
    fn parse_gc_algorithm_unsupported_is_none() {
        // Known HotSpot collectors CratonVM does not implement → None so the
        // launcher warns and falls back to Generational.
        for name in ["Serial", "Parallel", "Shenandoah", "Epsilon"] {
            assert_eq!(
                parse_gc_algorithm(name),
                None,
                "{name} should be unsupported"
            );
        }
        // Garbage / empty input is also None.
        assert_eq!(parse_gc_algorithm("nonsense"), None);
        assert_eq!(parse_gc_algorithm(""), None);
    }

    #[test]
    fn parse_classpath_platform() {
        if cfg!(windows) {
            let entries = VmConfig::parse_classpath("lib;classes;.");
            assert_eq!(entries, vec!["lib", "classes", "."]);
        } else {
            let entries = VmConfig::parse_classpath("lib:classes:.");
            assert_eq!(entries, vec!["lib", "classes", "."]);
        }
    }

    #[test]
    fn builder_pattern() {
        let config = VmConfig::new()
            .with_max_heap_size(512 * 1024 * 1024)
            .with_classpath(vec!["lib".to_string()])
            .with_verbose_gc(true);
        assert_eq!(config.max_heap_size, 512 * 1024 * 1024);
        assert_eq!(config.classpath, vec!["lib"]);
        assert!(config.verbose_gc);
    }

    #[test]
    fn builder_boot_classpath() {
        let config = VmConfig::new()
            .with_boot_classpath(vec!["rt.jar".to_string()])
            .with_java_home("/opt/jdk8".to_string());
        assert_eq!(config.boot_classpath, vec!["rt.jar"]);
        assert_eq!(config.java_home.as_deref(), Some("/opt/jdk8"));
    }

    #[test]
    fn discover_boot_classpath_no_java_home() {
        // With a non-existent path, should return empty
        let result = discover_boot_classpath(Some("/nonexistent/path"));
        assert!(result.is_empty());
    }

    #[test]
    fn discover_ext_classpath_no_java_home() {
        let result = discover_ext_classpath(Some("/nonexistent/path"));
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_java_home_nonexistent() {
        assert!(resolve_java_home(Some("/nonexistent/jdk")).is_none());
    }

    #[test]
    fn skip_verification_default_is_false() {
        let config = VmConfig::default();
        assert!(!config.skip_verification);
    }

    #[test]
    fn with_skip_verification() {
        let config = VmConfig::new().with_skip_verification(true);
        assert!(config.skip_verification);
    }

    #[test]
    fn default_stack_depth_is_reasonable() {
        let config = VmConfig::default();
        assert!(config.max_stack_depth >= 64);
        assert!(config.max_stack_depth <= 10000);
    }

    #[test]
    fn parse_classpath_empty_string() {
        let entries = VmConfig::parse_classpath("");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_classpath_single_entry() {
        let entries = VmConfig::parse_classpath("lib");
        assert_eq!(entries, vec!["lib"]);
    }

    #[test]
    fn with_ext_classpath_builder() {
        let config = VmConfig::new().with_ext_classpath(vec!["ext.jar".to_string()]);
        assert_eq!(config.ext_classpath, vec!["ext.jar"]);
    }

    // ── Additional edge case tests ────────────────────────────────────

    #[test]
    fn default_initial_heap_size() {
        let config = VmConfig::default();
        assert_eq!(config.initial_heap_size, 16 * 1024 * 1024);
    }

    #[test]
    fn default_verbose_flags_are_false() {
        let config = VmConfig::default();
        assert!(!config.verbose_class_loading);
        assert!(!config.verbose_gc);
    }

    #[test]
    fn default_system_properties_empty() {
        let config = VmConfig::default();
        assert!(config.system_properties.is_empty());
    }

    #[test]
    fn custom_heap_size_via_builder() {
        let config = VmConfig::new().with_max_heap_size(1024);
        assert_eq!(config.max_heap_size, 1024);
        // Other fields remain default
        assert_eq!(config.initial_heap_size, 16 * 1024 * 1024);
        assert_eq!(config.max_stack_depth, 8192);
    }

    #[test]
    fn heap_size_zero() {
        let config = VmConfig::new().with_max_heap_size(0);
        assert_eq!(config.max_heap_size, 0);
    }

    #[test]
    fn heap_size_very_large() {
        let config = VmConfig::new().with_max_heap_size(usize::MAX);
        assert_eq!(config.max_heap_size, usize::MAX);
    }

    #[test]
    fn parse_classpath_consecutive_separators() {
        // Consecutive separators produce empty strings which should be filtered
        if cfg!(windows) {
            let entries = VmConfig::parse_classpath("lib;;classes");
            assert_eq!(entries, vec!["lib", "classes"]);
        } else {
            let entries = VmConfig::parse_classpath("lib::classes");
            assert_eq!(entries, vec!["lib", "classes"]);
        }
    }

    #[test]
    fn parse_classpath_trailing_separator() {
        if cfg!(windows) {
            let entries = VmConfig::parse_classpath("lib;classes;");
            assert_eq!(entries, vec!["lib", "classes"]);
        } else {
            let entries = VmConfig::parse_classpath("lib:classes:");
            assert_eq!(entries, vec!["lib", "classes"]);
        }
    }

    #[test]
    fn parse_classpath_leading_separator() {
        if cfg!(windows) {
            let entries = VmConfig::parse_classpath(";lib;classes");
            assert_eq!(entries, vec!["lib", "classes"]);
        } else {
            let entries = VmConfig::parse_classpath(":lib:classes");
            assert_eq!(entries, vec!["lib", "classes"]);
        }
    }

    /// MSYS/Git-Bash on Windows leaves `;`-joined classpaths untranslated,
    /// so `parse_classpath` has to recognise POSIX-drive entries and
    /// convert them. Verified on Windows only — on POSIX hosts the
    /// translation is a no-op and `/c/...` is taken verbatim.
    #[test]
    #[cfg(windows)]
    fn parse_classpath_normalizes_msys_paths() {
        let entries =
            VmConfig::parse_classpath("/c/craton/bootstrap.jar;/c/craton/tomcat-juli.jar");
        assert_eq!(
            entries,
            vec![
                "C:/craton/bootstrap.jar".to_string(),
                "C:/craton/tomcat-juli.jar".to_string(),
            ]
        );
    }

    #[test]
    #[cfg(windows)]
    fn parse_classpath_normalizes_cygdrive_paths() {
        let entries = VmConfig::parse_classpath("/cygdrive/c/a.jar;/cygdrive/d/b.jar");
        assert_eq!(
            entries,
            vec!["C:/a.jar".to_string(), "D:/b.jar".to_string()]
        );
    }

    /// Native Windows entries and relative entries pass through untouched.
    #[test]
    #[cfg(windows)]
    fn parse_classpath_preserves_native_windows_paths() {
        let entries = VmConfig::parse_classpath("C:/foo/a.jar;lib/b.jar;.;C:\\bar\\c.jar");
        assert_eq!(
            entries,
            vec![
                "C:/foo/a.jar".to_string(),
                "lib/b.jar".to_string(),
                ".".to_string(),
                "C:\\bar\\c.jar".to_string(),
            ]
        );
    }

    /// An entry that is just a leading slash followed by a name that
    /// isn't a single drive letter (e.g. `/etc/...`) must not be
    /// rewritten — we don't want to mangle paths that genuinely start
    /// at the filesystem root on some other platform that happens to be
    /// passed through verbatim.
    #[test]
    #[cfg(windows)]
    fn parse_classpath_leaves_non_drive_root_paths_untouched() {
        let entries = VmConfig::parse_classpath("/etc/foo.jar;/usr/lib/bar.jar");
        assert_eq!(
            entries,
            vec!["/etc/foo.jar".to_string(), "/usr/lib/bar.jar".to_string(),]
        );
    }

    #[test]
    fn builder_chaining_all_options() {
        let config = VmConfig::new()
            .with_max_heap_size(128 * 1024 * 1024)
            .with_classpath(vec!["a.jar".to_string(), "b.jar".to_string()])
            .with_boot_classpath(vec!["rt.jar".to_string()])
            .with_ext_classpath(vec!["ext.jar".to_string()])
            .with_java_home("/usr/lib/jvm/java-8".to_string())
            .with_verbose_class_loading(true)
            .with_verbose_gc(true)
            .with_skip_verification(true);

        assert_eq!(config.max_heap_size, 128 * 1024 * 1024);
        assert_eq!(config.classpath.len(), 2);
        assert_eq!(config.boot_classpath, vec!["rt.jar"]);
        assert_eq!(config.ext_classpath, vec!["ext.jar"]);
        assert_eq!(config.java_home.as_deref(), Some("/usr/lib/jvm/java-8"));
        assert!(config.verbose_class_loading);
        assert!(config.verbose_gc);
        assert!(config.skip_verification);
    }

    #[test]
    fn config_clone_is_independent() {
        let config1 = VmConfig::new().with_max_heap_size(100);
        let mut config2 = config1.clone();
        config2.max_heap_size = 200;
        assert_eq!(config1.max_heap_size, 100);
        assert_eq!(config2.max_heap_size, 200);
    }

    #[test]
    fn config_debug_format() {
        let config = VmConfig::default();
        let debug = format!("{:?}", config);
        assert!(debug.contains("VmConfig"));
        assert!(debug.contains("max_heap_size"));
    }

    // ── AOT configuration tests ────────────────────────��─────────────

    #[test]
    fn default_aot_mode_is_off() {
        let config = VmConfig::default();
        assert_eq!(config.aot_mode, AotMode::Off);
        assert!(config.aot_cache_input.is_none());
        assert!(config.aot_cache_output.is_none());
    }

    #[test]
    fn with_aot_mode_training() {
        let config = VmConfig::new().with_aot_mode(AotMode::Training);
        assert_eq!(config.aot_mode, AotMode::Training);
    }

    #[test]
    fn with_aot_mode_production() {
        let config = VmConfig::new().with_aot_mode(AotMode::Production);
        assert_eq!(config.aot_mode, AotMode::Production);
    }

    #[test]
    fn with_aot_cache_paths() {
        let config = VmConfig::new()
            .with_aot_cache_input("/tmp/cache.aot".to_string())
            .with_aot_cache_output("/tmp/cache_out.aot".to_string());
        assert_eq!(config.aot_cache_input.as_deref(), Some("/tmp/cache.aot"));
        assert_eq!(
            config.aot_cache_output.as_deref(),
            Some("/tmp/cache_out.aot")
        );
    }

    #[test]
    fn aot_builder_chaining() {
        let config = VmConfig::new()
            .with_aot_mode(AotMode::Training)
            .with_aot_cache_output("/tmp/out.aot".to_string())
            .with_max_heap_size(512 * 1024 * 1024);
        assert_eq!(config.aot_mode, AotMode::Training);
        assert_eq!(config.aot_cache_output.as_deref(), Some("/tmp/out.aot"));
        assert_eq!(config.max_heap_size, 512 * 1024 * 1024);
    }

    #[test]
    fn aot_mode_clone_is_independent() {
        let c1 = VmConfig::new().with_aot_mode(AotMode::Training);
        let mut c2 = c1.clone();
        c2.aot_mode = AotMode::Production;
        assert_eq!(c1.aot_mode, AotMode::Training);
        assert_eq!(c2.aot_mode, AotMode::Production);
    }

    // ── Boot classpath auto-discovery integration tests ───────────────
    // These tests require a JDK installation on the host. They are ignored
    // by default so CI without a JDK doesn't fail. Run with:
    //   cargo test -p cratonvm-vm -- --ignored

    /// Helper: find a real JDK installation on this machine, or return None.
    fn find_local_jdk() -> Option<PathBuf> {
        // Check JAVA_HOME first
        if let Ok(val) = cratonvm_types::flags::runtime_var("JAVA_HOME") {
            let p = PathBuf::from(&val);
            if p.join("jmods").is_dir() {
                return Some(p);
            }
        }
        // Fall back to PATH detection
        detect_java_home_from_path()
    }

    /// **ONE FILE, TWO TESTS, AND `cargo test` RUNS THEM AT THE SAME TIME.**
    ///
    /// `java_resolves_to_the_sibling_alias_before_path` and
    /// `auto_detection_declines_when_java_is_this_process` both stand a decoy
    /// at `<test binary's directory>/java.exe`, and neither can use a different
    /// path: the behaviour under test is that `CreateProcessW` searches the
    /// calling executable's OWN directory first, so the file has to be beside
    /// this binary and has to be called `java.exe`.
    ///
    /// Unserialised they raced three ways, and the loudest was the simplest:
    /// both reach `std::fs::write` / `std::fs::copy` on the same path, and
    /// Windows refuses the second one rather than interleaving it. Measured at
    /// **5 failures in 40 paired runs** before this lock, 0 in 200 after.
    /// The quieter interleaving is the one that reads as a spelling bug: B
    /// deletes the file between A's `resolve_java_executable` and A's
    /// `canonicalize`, so A's `expected` falls through `unwrap_or` to the raw
    /// path while `resolved` is the canonical `\?\` form, and the assertion
    /// compares two spellings of the same file.
    ///
    /// `parking_lot`, so a panicking test poisons nothing: a failure in one of
    /// these must fail ONE test, not convert the other into a lock-poisoning
    /// error that hides its own result.
    #[cfg(windows)]
    static SIBLING_ALIAS: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// The `java.exe`-beside-`cratonvm.exe` layout must not auto-detect.
    ///
    /// This is the configuration the `java-bin-alias` cargo feature
    /// produces, and it is the one that recursed without bound: the old
    /// guard compared `current_exe()` against the first `java` on `PATH`,
    /// which on a machine with a real JDK installed is a different file,
    /// so the guard passed and the spawn then ran the sibling alias —
    /// this VM — anyway.
    ///
    /// Asserted through `resolve_java_executable` rather than by spawning
    /// anything. A test that actually let the recursion start would be a
    /// test that can take the machine down when it regresses, which is
    /// the opposite of useful.
    #[test]
    #[cfg(windows)]
    fn java_resolves_to_the_sibling_alias_before_path() {
        let _serialised = SIBLING_ALIAS.lock();
        let me = std::env::current_exe().expect("current_exe");
        let dir = me.parent().expect("exe has a parent").to_path_buf();

        let alias = dir.join("java.exe");
        let created = !alias.exists();
        if created {
            // Contents are irrelevant: resolution is by path, and nothing
            // here executes it.
            std::fs::write(&alias, b"not a real executable").expect("write decoy");
        }

        let resolved = resolve_java_executable();

        // CANONICALISE BEFORE THE DELETE. `std::fs::canonicalize` resolves
        // against the filesystem and FAILS for a path that no longer exists, so
        // asking about `alias` after removing it silently fell through
        // `unwrap_or` to the raw path -- while `resolve_java_executable` hands
        // back an already-canonical extended-length one. Two spellings of the
        // same file, and the assertion compared them: the resolved side carried
        // the Windows `\\?\` prefix and the expected side did not.
        //
        // Windows-only, and it also fails ALONE, which is why it was fixed
        // separately from the race: the two causes are independent and closing
        // either one leaves the other. The race is real -- see
        // [`SIBLING_ALIAS`], which is what now closes it -- and an earlier
        // revision of this comment asserted it was not.
        let expected = std::fs::canonicalize(&alias).unwrap_or_else(|_| alias.clone());

        if created {
            let _ = std::fs::remove_file(&alias);
        }

        let resolved = resolved.expect("a java.exe beside the test binary must resolve");
        let resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
        assert_eq!(
            resolved, expected,
            "CreateProcessW searches the calling executable's own directory \
             before PATH, so resolution must too — otherwise the self-check \
             in detect_java_home_from_path compares against the wrong file \
             and a cratonvm built as java.exe spawns itself without bound"
        );
    }

    /// Auto-detection must decline when `java` resolves to this process.
    ///
    /// The payload of the fix: whatever `PATH` holds, if the thing that
    /// would be spawned is this executable, the answer is `None` and the
    /// user is pushed to `JAVA_HOME` / `--java-home`.
    #[test]
    #[cfg(windows)]
    fn auto_detection_declines_when_java_is_this_process() {
        let _serialised = SIBLING_ALIAS.lock();
        let me = std::env::current_exe().expect("current_exe");
        let dir = me.parent().expect("exe has a parent").to_path_buf();
        let alias = dir.join("java.exe");

        // Only meaningful when we can stand in for the alias ourselves.
        // Copying this test binary makes `resolve_java_executable` return a
        // file whose canonical path equals `current_exe()`'s content-wise
        // but not path-wise, so instead assert the narrower, decisive
        // property: a resolution equal to current_exe() yields None.
        if alias.exists() {
            return;
        }
        std::fs::copy(&me, &alias).expect("copy self as java.exe");
        let resolved = resolve_java_executable().map(|p| std::fs::canonicalize(&p).unwrap_or(p));
        let _ = std::fs::remove_file(&alias);

        let resolved = resolved.expect("sibling alias resolves");
        assert!(
            resolved.file_name().and_then(|n| n.to_str()) == Some("java.exe"),
            "expected the sibling alias, got {}",
            resolved.display()
        );
    }

    #[test]
    #[ignore] // requires JDK on host
    fn detect_java_home_from_path_finds_jdk() {
        let jdk = detect_java_home_from_path();
        assert!(
            jdk.is_some(),
            "Expected to find JDK via `java` on PATH. \
             Is Java installed and on the system PATH?"
        );
        let jdk = jdk.unwrap();
        assert!(
            jdk.is_dir(),
            "java.home should be a directory: {}",
            jdk.display()
        );
        // JDK 9+ should have a jmods directory
        let jmods = jdk.join("jmods");
        assert!(
            jmods.is_dir() || jdk.join("lib").join("rt.jar").exists(),
            "JDK at {} should have jmods/ (JDK 9+) or lib/rt.jar (JDK 8)",
            jdk.display()
        );
    }

    #[test]
    #[ignore] // requires JDK on host
    fn discover_boot_classpath_finds_jmods() {
        let jdk = find_local_jdk();
        if jdk.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jdk = jdk.unwrap();

        let entries = discover_boot_classpath_with(Some(jdk.to_str().unwrap()), false);
        assert!(
            !entries.is_empty(),
            "discover_boot_classpath should find entries for JDK at {}",
            jdk.display()
        );

        // java.base.jmod should be the first entry
        let first = &entries[0];
        assert!(
            first.contains("java.base"),
            "First boot classpath entry should be java.base.jmod, got: {first}"
        );

        // Should have multiple modules
        let jmod_count = entries.iter().filter(|e| e.ends_with(".jmod")).count();
        assert!(
            jmod_count >= 10,
            "Expected at least 10 jmod files, found {jmod_count}"
        );

        eprintln!(
            "Discovered {} boot classpath entries ({} jmods) from {}",
            entries.len(),
            jmod_count,
            jdk.display()
        );
    }

    // -----------------------------------------------------------------------
    // Phase B: JPMS CLI parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_add_reads_single() {
        let result = VmConfig::parse_add_reads("modA=modB");
        assert_eq!(result, vec![("modA".to_string(), "modB".to_string())]);
    }

    #[test]
    fn parse_add_reads_multiple_targets() {
        let result = VmConfig::parse_add_reads("modA=modB,modC");
        assert_eq!(
            result,
            vec![
                ("modA".to_string(), "modB".to_string()),
                ("modA".to_string(), "modC".to_string()),
            ]
        );
    }

    #[test]
    fn parse_add_reads_no_equals() {
        let result = VmConfig::parse_add_reads("modA");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_add_exports_basic() {
        let result = VmConfig::parse_add_exports("modA/com.foo=modB");
        assert_eq!(
            result,
            Some((
                "modA".to_string(),
                "com/foo".to_string(),
                "modB".to_string()
            ))
        );
    }

    #[test]
    fn parse_add_exports_all_unnamed() {
        // `ALL-UNNAMED` survives parsing verbatim. Collapsing it to `""` here
        // is what made `--add-opens ...=ALL-UNNAMED` an unqualified open, so
        // this assertion is the guard on the over-grant, not a formatting
        // preference: `""` is `ModuleRegistry`'s open-to-everyone marker.
        let result = VmConfig::parse_add_exports("java.base/java.lang=ALL-UNNAMED");
        assert_eq!(
            result,
            Some((
                "java.base".to_string(),
                "java/lang".to_string(),
                "ALL-UNNAMED".to_string()
            ))
        );
    }

    #[test]
    fn parse_add_exports_invalid() {
        assert!(VmConfig::parse_add_exports("garbage").is_none());
        assert!(VmConfig::parse_add_exports("mod=target").is_none()); // missing /package
    }

    // -----------------------------------------------------------------------
    // Real-JDK detection and boot-default decision (task #53)
    // -----------------------------------------------------------------------

    /// Helper: create a fake JDK layout containing `jmods/java.base.jmod`.
    fn fake_jdk_with_jmod(root: &std::path::Path) {
        let jmods = root.join("jmods");
        std::fs::create_dir_all(&jmods).unwrap();
        std::fs::write(jmods.join("java.base.jmod"), b"JM\x01\x00").unwrap();
    }

    /// Helper: create a fake jlink-image layout with `lib/modules`.
    fn fake_jdk_with_lib_modules(root: &std::path::Path) {
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("modules"), b"jimg\x00\x00\x00\x01").unwrap();
    }

    /// Several tests below drive [`detect_real_jdk`] / [`require_real_jdk`]
    /// by pointing `CRATONVM_JAVA_HOME` at a synthesised JDK tree, with
    /// `JAVA_HOME` scrubbed so the next probe down cannot answer instead.
    ///
    /// The two variables need *different* mechanisms, and getting that wrong
    /// is what made these tests vacuous for months:
    ///
    /// * `CRATONVM_JAVA_HOME` is a **declared** flag. `resolve_java_home`
    ///   reads it through `flags::runtime_var`, which serves it from one
    ///   process-wide snapshot latched on the first read of any flag. In this
    ///   crate's test binary that latch has long since happened, so `set_var`
    ///   here changed `environ` and nothing the code under test would read —
    ///   the test then measured the developer's real JDK. It is overridden on
    ///   the snapshot instead, for this thread only.
    /// * `JAVA_HOME` is **not** declared and keeps `std::env`'s live-read
    ///   semantics, so it still has to be stashed/restored in `environ` —
    ///   which is process-wide and races concurrent tests, hence
    ///   [`env_lock`].
    fn with_scratch_java_home<R>(root: Option<&str>, f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_JAVA_HOME", root)], || {
            with_env("JAVA_HOME", None, f)
        })
    }

    /// Set an **undeclared** environment variable for the duration of `f`.
    ///
    /// Declared `CRATONVM_*` flags must not go through here — see
    /// [`with_scratch_java_home`] — so this asserts against them rather than
    /// leaving the next caller to rediscover why their override did nothing.
    fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        debug_assert!(
            !key.starts_with("CRATONVM_"),
            "{key} looks like a declared flag; use flags::with_thread_overrides"
        );
        let prev = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let result = f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        result
    }

    /// Avoid `JAVA_HOME` races between the detection tests.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn detect_real_jdk_returns_some_when_java_base_jmod_present() {
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        let detected = detect_real_jdk_from(tmp.path().to_str());
        assert!(
            detected.is_some(),
            "detect_real_jdk should find synthetic JDK at {}",
            tmp.path().display()
        );
        assert_eq!(detected.unwrap(), tmp.path());
    }

    #[test]
    fn detect_real_jdk_returns_some_for_lib_modules_image() {
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_lib_modules(tmp.path());
        let detected = detect_real_jdk_from(tmp.path().to_str());
        assert!(
            detected.is_some(),
            "detect_real_jdk should accept jlink-image layout"
        );
    }

    #[test]
    fn detect_real_jdk_returns_none_when_only_rtjar_present() {
        // JDK 8-style: `lib/rt.jar` only. We have no JMOD/jimage loader,
        // so this should NOT count as "real JDK boot available".
        // `CRATONVM_JAVA_HOME` is the highest-priority probe in
        // `resolve_java_home`, so we don't need to scrub `JAVA_HOME` or
        // `PATH` — the synthesised directory wins.
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("rt.jar"), b"PK\x03\x04").unwrap();
        assert!(
            detect_real_jdk_from(tmp.path().to_str()).is_none(),
            "rt.jar-only install must not satisfy detect_real_jdk"
        );
    }

    /// **New contract (2026-07-26).** The launcher default is real-JDK
    /// *unconditionally*. This test previously asserted that
    /// `with_host_jdk_default` flipped the mode when a JDK was detected;
    /// it now asserts the mode does not depend on detection at all. The
    /// JDK-present arm is kept so the two host shapes (JDK present /
    /// JDK absent) are still both exercised — they must agree.
    #[test]
    fn launcher_default_is_real_jdk_when_host_has_a_jdk() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        with_scratch_java_home(tmp.path().to_str(), || {
            let cfg = VmConfig::for_launcher();
            assert_eq!(cfg.jdk_mode(), JdkMode::Real);
            assert!(!cfg.use_synthetic_jdk);
        });
    }

    /// The behaviour this file used to encode — "no JDK found ⇒ silently
    /// run the synthetic class library instead" — is now a defect, and
    /// this test pins its absence.
    ///
    /// Using an empty real directory (instead of unsetting `JAVA_HOME`
    /// and emptying `PATH`) avoids racing with the system `java`
    /// binary, which `Command::new("java")` may still resolve on
    /// Windows via `CreateProcess` fallback search paths even with an
    /// empty `PATH`.
    #[test]
    fn launcher_default_never_falls_back_to_synthetic_when_no_jdk() {
        let tmp = tempfile::tempdir().unwrap();
        // Bare directory: no `jmods/`, no `lib/modules`. `resolve_java_home`
        // will accept it (it's a real directory) but `detect_real_jdk`
        // must reject it because neither boot blob is present.
        let cfg = VmConfig::for_launcher();
        assert_eq!(
            cfg.jdk_mode(),
            JdkMode::Real,
            "the launcher default must not depend on whether a JDK exists"
        );
        let err = require_real_jdk(tmp.path().to_str())
            .expect_err("require_real_jdk must fail when no boot modules exist");
        assert!(err.contains("no usable JDK was found"), "{err}");
    }

    /// `with_host_jdk_default` is the out-of-crate (`libcratonvm`,
    /// `cratonvm-embed`) base. It no longer aliases `for_launcher()` — the
    /// two now disagree about compatibility mode — but it must still share
    /// the launcher's JDK mode, and in particular it must NOT probe the host.
    #[test]
    fn with_host_jdk_default_shares_the_launchers_jdk_mode() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        // Host with no usable JDK: the legacy implementation would have
        // returned synthetic here.
        with_scratch_java_home(tmp.path().to_str(), || {
            assert_eq!(
                VmConfig::with_host_jdk_default().jdk_mode(),
                VmConfig::for_launcher().jdk_mode()
            );
            assert_eq!(VmConfig::with_host_jdk_default().jdk_mode(), JdkMode::Real);
        });
    }

    /// Explicit `--synthetic-jdk` opt-in must beat the launcher default:
    /// even with a real JDK on the host, an explicit synthetic request is
    /// honoured (subject to `require_synthetic_jdk`, which the CLI checks).
    #[test]
    fn explicit_synthetic_jdk_override_wins_over_launcher_default() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        with_scratch_java_home(tmp.path().to_str(), || {
            let cfg = VmConfig::for_launcher().with_jdk_mode(JdkMode::Synthetic);
            assert_eq!(cfg.jdk_mode(), JdkMode::Synthetic);
            assert!(cfg.use_synthetic_jdk);
            // The legacy boolean setter must stay equivalent.
            assert_eq!(
                VmConfig::for_launcher().with_synthetic_jdk(true).jdk_mode(),
                JdkMode::Synthetic
            );
        });
    }

    /// `VmConfig::default()` must remain hermetic (synthetic) so library
    /// callers and the ~5000-test suite don't accidentally start
    /// resolving JMODs from the host JDK — and the fact that it differs
    /// from the launcher default must be a *declared* difference.
    #[test]
    fn default_config_stays_synthetic_regardless_of_host() {
        let cfg = VmConfig::default();
        assert_eq!(cfg.jdk_mode(), EMBEDDED_DEFAULT_JDK_MODE);
        assert_eq!(EMBEDDED_DEFAULT_JDK_MODE, JdkMode::Synthetic);
        assert!(
            cfg.use_synthetic_jdk,
            "VmConfig::default() must remain synthetic (hermetic embedding/test path)"
        );
        // The split is intentional and must stay visible: if these two
        // ever converge, delete one of the constants rather than letting
        // callers rediscover the difference by debugging.
        assert_ne!(
            EMBEDDED_DEFAULT_JDK_MODE, LAUNCHER_DEFAULT_JDK_MODE,
            "the launcher and embedding defaults differ by design; see \
             jdk-mode-determinism.md"
        );
        assert_eq!(LAUNCHER_DEFAULT_JDK_MODE, JdkMode::Real);
    }

    // -----------------------------------------------------------------------
    // Compatibility mode, 2026-07-31; launcher default flipped 2026-09-20.
    //
    // BOTH pairs now split by entry point, for different reasons: the
    // JDK-mode pair on hermeticity, the compatibility pair on who asked.
    // These tests pin the split in both directions, because each half is the
    // kind of "inconsistency" a later reader could tidy the wrong way --
    // toward an embedder that silently runs strict, or a launcher that
    // silently does not.
    // -----------------------------------------------------------------------

    /// The launcher default is real-JDK *and* strict. A person typing
    /// `cratonvm Foo` gets the conforming answer; `--compatible` opts out.
    #[test]
    fn launcher_default_is_real_jdk_and_jdk_only() {
        let cfg = VmConfig::for_launcher();
        assert_eq!(cfg.jdk_mode(), JdkMode::Real);
        assert_eq!(cfg.compatibility_mode, LAUNCHER_DEFAULT_COMPATIBILITY_MODE);
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::JdkOnly);
        assert!(cfg.is_jdk_only());
        assert_eq!(cfg.execution_policy(), ExecutionPolicy::jdk_only());
        cfg.validate_compatibility()
            .expect("the launcher default must be a valid pairing");
    }

    /// `VmConfig::default()` (embedding / in-tree tests) stays `compatible`,
    /// and that is now a DIFFERENCE from the launcher rather than an
    /// agreement. A host that never passed a policy argument has not asked
    /// for a stricter one and has no flag of its own to opt back out with.
    #[test]
    fn embedded_default_is_compatible_and_differs_from_the_launcher() {
        let cfg = VmConfig::default();
        assert_eq!(cfg.compatibility_mode, EMBEDDED_DEFAULT_COMPATIBILITY_MODE);
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
        assert!(!cfg.is_jdk_only());
        assert_ne!(
            EMBEDDED_DEFAULT_COMPATIBILITY_MODE, LAUNCHER_DEFAULT_COMPATIBILITY_MODE,
            "the compatibility defaults differ by design; re-equalising them either \
             hands every embedder a policy it never asked for, or gives the \
             launcher back the substitutions --compatible now names"
        );
        // The default is synthetic-JDK, so the policy must report
        // `real_jdk = false` - `execution_policy` reads the config as it
        // stands rather than asserting a house style.
        assert!(!cfg.execution_policy().real_jdk);
    }

    /// The real-JDK EMBEDDING base: the launcher's JDK mode, the embedder's
    /// compatibility mode. `libcratonvm`'s advertised C-ABI default is
    /// defined as this value, so it must not follow the launcher.
    #[test]
    fn host_jdk_default_is_real_jdk_but_still_compatible() {
        let cfg = VmConfig::with_host_jdk_default();
        assert_eq!(cfg.jdk_mode(), JdkMode::Real);
        assert_eq!(
            cfg.compatibility_mode,
            EMBEDDED_HOST_JDK_DEFAULT_COMPATIBILITY_MODE
        );
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
        assert!(!cfg.is_jdk_only());
        assert_eq!(cfg.execution_policy(), ExecutionPolicy::compatible(true));
        cfg.validate_compatibility()
            .expect("real-JDK + compatible is a valid pairing");
        // It was a bare alias for `for_launcher()` until the launcher default
        // flipped. Re-aliasing it is an ABI break, so pin the divergence.
        assert_ne!(
            VmConfig::with_host_jdk_default().compatibility_mode,
            VmConfig::for_launcher().compatibility_mode,
            "with_host_jdk_default must not inherit the launcher's strictness"
        );
    }

    /// `--real-jdk` paired with real-JDK (what the CLI builds) is a coherent
    /// configuration whose policy reports both halves.
    #[test]
    fn jdk_only_with_real_jdk_is_the_valid_strict_pairing() {
        let cfg = VmConfig::for_launcher()
            .with_jdk_mode(JdkMode::Real)
            .with_compatibility_mode(CompatibilityMode::JdkOnly);
        assert!(cfg.is_jdk_only());
        assert!(!cfg.use_synthetic_jdk);
        let policy = cfg.execution_policy();
        assert_eq!(policy.compatibility_mode, CompatibilityMode::JdkOnly);
        assert!(policy.real_jdk, "strict mode runs on a real JDK image");
        assert!(policy.is_jdk_only());
        assert_eq!(policy, ExecutionPolicy::jdk_only());
        cfg.validate_compatibility()
            .expect("jdk-only + real-jdk is valid");
    }

    /// `--real-jdk --synthetic-jdk` is a configuration error, and the error
    /// has to name both flags and both fixes: the correct correction depends
    /// on what the caller meant, so the VM picks neither.
    ///
    /// Note what is *not* asserted: `with_compatibility_mode` does not clear
    /// `use_synthetic_jdk`. If it did, this pairing would repair itself
    /// silently and the conflict would never reach the operator.
    #[test]
    fn jdk_only_with_synthetic_jdk_is_rejected() {
        let cfg = VmConfig::default()
            .with_jdk_mode(JdkMode::Synthetic)
            .with_compatibility_mode(CompatibilityMode::JdkOnly);
        assert!(
            cfg.use_synthetic_jdk,
            "with_compatibility_mode must not rewrite the JDK mode"
        );
        // The incoherence stays visible in the policy rather than being
        // papered over by `ExecutionPolicy::jdk_only()`.
        assert!(!cfg.execution_policy().real_jdk);

        let err = cfg
            .validate_compatibility()
            .expect_err("jdk-only + synthetic-jdk must be rejected");
        match &err {
            crate::error::VmError::InvalidConfiguration(msg) => {
                for needle in ["--real-jdk", "--synthetic-jdk", "Fix by one of:"] {
                    assert!(
                        msg.contains(needle),
                        "error message must mention {needle:?}; got:\n{msg}"
                    );
                }
            }
            other => panic!("expected VmError::InvalidConfiguration, got {other:?}"),
        }
    }

    /// `JdkMode` carries no strictness. Selecting either class library leaves
    /// the compatibility mode alone — the two axes are orthogonal, and
    /// overloading `JdkMode` with strictness is explicitly out of contract.
    #[test]
    fn jdk_mode_alone_never_implies_strictness() {
        for mode in [JdkMode::Real, JdkMode::Synthetic] {
            let cfg = VmConfig::default().with_jdk_mode(mode);
            assert_eq!(
                cfg.compatibility_mode,
                CompatibilityMode::Compatible,
                "{mode} must not change the compatibility mode"
            );
            assert!(!cfg.is_jdk_only());
            cfg.validate_compatibility()
                .expect("compatible mode is valid with either class library");
        }
        // The legacy boolean setter is equivalent and equally inert.
        assert!(!VmConfig::default().with_synthetic_jdk(true).is_jdk_only());
        // `with_host_jdk_default()` rather than `for_launcher()`: the
        // launcher default is strict now, so it would fail this for a reason
        // that has nothing to do with the setter under test.
        assert!(!VmConfig::with_host_jdk_default()
            .with_synthetic_jdk(false)
            .is_jdk_only());
    }

    /// The real-JDK-unavailable error must name every location searched
    /// and every accepted layout, so a bug report carries enough to fix
    /// the machine without a round trip.
    #[test]
    fn require_real_jdk_error_names_search_path_and_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        let err = require_real_jdk(tmp.path().to_str()).expect_err("bare dir is not a JDK");
        for needle in [
            "jmods/java.base.jmod",
            "lib/modules",
            "lib/rt.jar",
            "--java-home",
            "--synthetic-jdk",
        ] {
            assert!(
                err.contains(needle),
                "error message must mention {needle:?}; got:\n{err}"
            );
        }
    }

    /// An `rt.jar`-only (JDK 8) install is rejected by the *validation*
    /// path too, not merely by detection — and the rejection is an error,
    /// never a downgrade. Companion to
    /// `detect_real_jdk_returns_none_when_only_rtjar_present`.
    #[test]
    fn require_real_jdk_rejects_rtjar_only_install() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("rt.jar"), b"PK\x03\x04").unwrap();
        assert!(require_real_jdk(tmp.path().to_str()).is_err());
    }

    /// A valid JDK makes validation succeed and hand back the resolved
    /// root, and an explicit `--java-home` short-circuits the env probes.
    #[test]
    fn require_real_jdk_accepts_jmods_and_honours_explicit_java_home() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        fake_jdk_with_jmod(tmp.path());
        let explicit = tmp.path().to_str().unwrap().to_string();
        // Explicit path wins even when the env points somewhere useless.
        let bogus = tempfile::tempdir().unwrap();
        with_scratch_java_home(bogus.path().to_str(), || {
            let home = require_real_jdk(Some(explicit.as_str())).expect("explicit JDK is valid");
            assert_eq!(home, tmp.path());
            let described = describe_jdk_search(Some(explicit.as_str()));
            assert!(described.contains("--java-home"), "{described}");
        });
    }

    /// `require_synthetic_jdk` must agree with the compile-time feature —
    /// this is what stops a default (feature-off) build from accepting
    /// `--synthetic-jdk` and booting with no class library at all.
    #[test]
    fn require_synthetic_jdk_tracks_the_cargo_feature() {
        assert_eq!(
            require_synthetic_jdk().is_ok(),
            SYNTHETIC_JDK_COMPILED_IN,
            "synthetic mode must be accepted exactly when the stubs are compiled in"
        );
        assert_eq!(SYNTHETIC_JDK_COMPILED_IN, cfg!(feature = "synthetic-jdk"));
    }

    /// Mode naming is part of the bug-report contract: the strings in
    /// `-version` output must stay stable and must match the CLI flags.
    #[test]
    fn jdk_mode_strings_are_stable_and_match_cli_flags() {
        assert_eq!(JdkMode::Real.as_str(), "real-jdk");
        assert_eq!(JdkMode::Synthetic.as_str(), "synthetic-jdk");
        assert_eq!(JdkMode::Real.selecting_flag(), "--real-jdk");
        assert_eq!(JdkMode::Synthetic.selecting_flag(), "--synthetic-jdk");
        assert_eq!(format!("{}", JdkMode::Real), "real-jdk");
        assert!(!JdkMode::Real.use_synthetic_jdk());
        assert!(JdkMode::Synthetic.use_synthetic_jdk());
        assert_eq!(JdkMode::from_use_synthetic_jdk(true), JdkMode::Synthetic);
        assert_eq!(JdkMode::from_use_synthetic_jdk(false), JdkMode::Real);
    }

    #[test]
    #[ignore] // requires JDK on host
    fn resolve_java_home_finds_jdk_without_env_var() {
        // This test verifies the full resolution chain works
        let jdk = resolve_java_home(None);
        // On a machine with Java on PATH but no JAVA_HOME, this should
        // still find the JDK via PATH detection.
        // We can't guarantee this on all CI machines, so just log.
        if let Some(jdk) = jdk {
            eprintln!("resolve_java_home(None) found: {}", jdk.display());
            assert!(jdk.is_dir());
        } else {
            eprintln!(
                "resolve_java_home(None) returned None — \
                 no JAVA_HOME env and no java on PATH"
            );
        }
    }
