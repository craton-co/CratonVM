use super::*;

#[test]
fn default_init_args_populates_version() {
    let mut args = JavaVMInitArgs {
        version: 0,
        n_options: 7,
        options: std::ptr::null_mut(),
        ignore_unrecognized: 1,
    };
    let rc = JNI_GetDefaultJavaVMInitArgs(&mut args as *mut _ as *mut c_void);
    assert_eq!(rc, JNI_OK);
    assert_eq!(args.version, JNI_VERSION);
    assert_eq!(args.version, 0x0001_0008);
    // Option fields are reset.
    assert_eq!(args.n_options, 0);
    assert!(args.options.is_null());
}

#[test]
fn default_init_args_rejects_unsupported_version() {
    let mut args = JavaVMInitArgs {
        version: 0x0001_0009,
        n_options: 0,
        options: std::ptr::null_mut(),
        ignore_unrecognized: 0,
    };
    let rc = JNI_GetDefaultJavaVMInitArgs(&mut args as *mut _ as *mut c_void);
    assert_eq!(rc, JNI_EVERSION);
    assert_eq!(args.version, JNI_VERSION);
}

#[test]
fn default_init_args_null_is_err() {
    assert_eq!(JNI_GetDefaultJavaVMInitArgs(std::ptr::null_mut()), JNI_ERR);
}

#[test]
fn created_vms_reports_count_with_null_buf() {
    // Before any create (in this test process, no create is invoked here to
    // avoid the heavy bootstrap), the count must be 0 and the call must not
    // touch the null buffer.
    let mut n: JSize = -1;
    let rc = JNI_GetCreatedJavaVMs(std::ptr::null_mut(), 0, &mut n as *mut JSize);
    assert_eq!(rc, JNI_OK);
    // count is 0 unless a sibling test in this binary created a VM first;
    // either way it must be a valid non-negative count that was written.
    assert!(n == 0 || n == 1);
}

#[test]
fn destroy_created_vm_without_vm_is_noop_ok() {
    // The `DestroyJavaVM` teardown is a no-op success when no Invocation-API
    // VM is parked (the default test run never bootstraps one — the flat API
    // does not touch `CREATED_VM`). This also pins idempotent double-destroy.
    assert_eq!(destroy_created_vm(), JNI_OK);
    assert_eq!(destroy_created_vm(), JNI_OK);
}

#[test]
fn mem_size_parsing() {
    assert_eq!(parse_mem_size("256m"), Some(256 * 1024 * 1024));
    assert_eq!(parse_mem_size("1g"), Some(1024 * 1024 * 1024));
    assert_eq!(parse_mem_size("512k"), Some(512 * 1024));
    assert_eq!(parse_mem_size("1048576"), Some(1_048_576));
    assert_eq!(parse_mem_size(""), None);
    assert_eq!(parse_mem_size("abc"), None);
}

#[test]
fn classpath_split_uses_platform_sep() {
    let mut out = Vec::new();
    if cfg!(windows) {
        push_classpath(&mut out, "a.jar;b.jar;;c.jar");
    } else {
        push_classpath(&mut out, "a.jar:b.jar::c.jar");
    }
    assert_eq!(out, vec!["a.jar", "b.jar", "c.jar"]);
}

// ── JDK-mode validation fixtures ──────────────────────────────────────
//
// `config_from_args` now validates that the selected class library really
// exists (see `validate_jdk_mode`). That makes it host-dependent unless
// the test points it at a JDK, so these helpers synthesise a minimal one
// — the same trick `vm/src/config.rs`'s detection tests use. Without them
// the suite would pass or fail depending on whether the machine running it
// happens to have a JDK installed, which is exactly the non-determinism
// this whole change exists to remove.

/// `JAVA_HOME` is process-wide; serialise.
///
/// `CRATONVM_JAVA_HOME` no longer needs this — it is a *declared* flag and
/// is now overridden per-thread through
/// [`cratonvm_types::flags::with_thread_overrides`] — but `JAVA_HOME` is
/// not declared, so it keeps `std::env`'s live-read semantics and still
/// has to be stashed and restored under a lock.
fn jdk_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Set an **undeclared** environment variable for the duration of `f`.
///
/// Declared `CRATONVM_*` flags must not go through here: they are served
/// from the frozen [`cratonvm_types::flags::flags`] snapshot, so mutating
/// `environ` would change nothing that the code under test reads. Use
/// `flags::with_thread_overrides` for those.
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

/// A scratch directory removed on drop. With `jmods = true` it looks
/// enough like a JDK 9+ root for `require_real_jdk`; with `jmods = false`
/// it is a bare directory that `resolve_java_home` accepts but
/// `detect_real_jdk` must reject.
struct ScratchJdk(std::path::PathBuf);

impl ScratchJdk {
    fn new(tag: &str, jmods: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "cratonvm-libjni-jdk-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_dir_all(&root);
        if jmods {
            let dir = root.join("jmods");
            std::fs::create_dir_all(&dir).expect("create scratch jmods dir");
            std::fs::write(dir.join("java.base.jmod"), b"JM\x01\x00")
                .expect("write scratch jmod");
        } else {
            std::fs::create_dir_all(&root).expect("create scratch dir");
        }
        Self(root)
    }
    fn path(&self) -> &str {
        self.0.to_str().expect("utf-8 temp path")
    }
}

impl Drop for ScratchJdk {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `f` with the JDK search pointed at `root`.
///
/// `CRATONVM_JAVA_HOME` is a declared flag, so it is overridden on the
/// [`cratonvm_types::flags`] snapshot for this thread rather than by
/// `set_var`. `resolve_java_home` reads it through
/// `flags::runtime_var`, which means an `environ` write would have been
/// ignored by every test but whichever one happened to run first and latch
/// the snapshot — the defect these fixtures existed to avoid in the first
/// place. `JAVA_HOME` is *not* declared and keeps live-read semantics, so
/// it still goes through `with_env`.
fn with_java_home<R>(root: &str, f: impl FnOnce() -> R) -> R {
    cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_JAVA_HOME", Some(root))], || {
        with_env("JAVA_HOME", None, f)
    })
}

/// Run `f` with the process pointed at a synthesised real JDK.
fn with_fake_jdk<R>(tag: &str, f: impl FnOnce() -> R) -> R {
    let _guard = jdk_env_lock();
    let jdk = ScratchJdk::new(tag, true);
    with_java_home(jdk.path(), f)
}

/// Run `f` with no usable JDK.
///
/// Points `CRATONVM_JAVA_HOME` at an *empty real directory* rather than
/// emptying `PATH`: on Windows `CreateProcess` can still resolve a system
/// `java` with an empty `PATH`, which would make the test flaky on
/// developer machines. Same rationale as
/// `config.rs::launcher_default_never_falls_back_to_synthetic_when_no_jdk`.
fn with_no_jdk<R>(tag: &str, f: impl FnOnce() -> R) -> R {
    let _guard = jdk_env_lock();
    let empty = ScratchJdk::new(tag, false);
    with_java_home(empty.path(), f)
}

// `JavaVMInitArgs::ignoreUnrecognized` is a JNI `jboolean` (`u8`), not a
// `jint`; all three call sites pass an untyped literal, so taking `u8`
// here is exact rather than a lossy conversion at the struct literal.
fn args_with(opts: &mut [JavaVMOption], ignore_unrecognized: u8) -> JavaVMInitArgs {
    JavaVMInitArgs {
        version: JNI_VERSION,
        n_options: opts.len() as JInt,
        options: opts.as_mut_ptr(),
        ignore_unrecognized,
    }
}

#[test]
fn config_from_args_rejects_unsupported_jni_version() {
    let args = JavaVMInitArgs {
        version: 0x0001_0009,
        n_options: 0,
        options: std::ptr::null_mut(),
        ignore_unrecognized: 1,
    };
    let result = unsafe { config_from_args(&args) };
    assert_eq!(
        result.err(),
        Some(InitArgsError::UnsupportedVersion(0x0001_0009))
    );
}

#[test]
fn config_from_args_honors_ignore_unrecognized() {
    let unknown = std::ffi::CString::new("-XX:NoSuchCratonOption").unwrap();
    let mut opts = [JavaVMOption {
        option_string: unknown.as_ptr() as *mut c_char,
        extra_info: std::ptr::null_mut(),
    }];
    let mut args = JavaVMInitArgs {
        version: JNI_VERSION,
        n_options: opts.len() as JInt,
        options: opts.as_mut_ptr(),
        ignore_unrecognized: 0,
    };

    let result = unsafe { config_from_args(&args) };
    assert_eq!(
        result.err(),
        Some(InitArgsError::UnrecognizedOption(
            "-XX:NoSuchCratonOption".to_string()
        ))
    );

    // With `ignoreUnrecognized` the unknown option is dropped — but the
    // config still has to name a class library that exists, so this half
    // runs against a synthesised JDK.
    args.ignore_unrecognized = 1;
    with_fake_jdk("ignore-unrecognized", || {
        assert!(unsafe { config_from_args(&args) }.is_ok());
    });
}

/// Interpreter round i2 wave 59, lane L1: `-XX:±DisableAttachMechanism`
/// is the launcher's option here too (`VmConfig::disable_attach_mechanism`),
/// last one wins, and absent leaves the attach mechanism on.
#[test]
fn config_from_args_reads_disable_attach_mechanism() {
    let parse = |spellings: &[&str]| {
        let owned: Vec<std::ffi::CString> = spellings
            .iter()
            .map(|s| std::ffi::CString::new(*s).unwrap())
            .collect();
        let mut opts: Vec<JavaVMOption> = owned
            .iter()
            .map(|s| JavaVMOption {
                option_string: s.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            })
            .collect();
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };
        with_fake_jdk("disable-attach", || unsafe { config_from_args(&args) })
            .expect("recognized options")
            .disable_attach_mechanism
    };
    assert!(parse(&["-XX:+DisableAttachMechanism"]));
    assert!(!parse(&[
        "-XX:+DisableAttachMechanism",
        "-XX:-DisableAttachMechanism"
    ]));
    assert!(!parse(&["-Dx=y"]));
}

/// t1 wave 13, lane I24W: `-Xverify:none` / `-noverify` /
/// `-Xverify:remote|all` are `JNI_CreateJavaVM` options, as on HotSpot
/// (they were unrecognized). Last one wins.
#[test]
fn t1w13_i24w_config_from_args_reads_xverify() {
    use cratonvm_vm::config::XverifyMode;
    let parse = |spellings: &[&str]| {
        let owned: Vec<std::ffi::CString> = spellings
            .iter()
            .map(|s| std::ffi::CString::new(*s).unwrap())
            .collect();
        let mut opts: Vec<JavaVMOption> = owned
            .iter()
            .map(|s| JavaVMOption {
                option_string: s.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            })
            .collect();
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };
        let cfg = with_fake_jdk("xverify", || unsafe { config_from_args(&args) })
            .expect("recognized options");
        (cfg.skip_verification, cfg.xverify_mode)
    };
    assert_eq!(parse(&["-Xverify:none"]), (true, XverifyMode::None));
    assert_eq!(parse(&["-noverify"]), (true, XverifyMode::None));
    assert_eq!(parse(&["-noverify", "-Xverify:all"]), (false, XverifyMode::All));
    assert_eq!(parse(&["-Xverify:all", "-Xverify:remote"]), (false, XverifyMode::Remote));
    assert!(!parse(&["-Dx=y"]).0);
}

/// Interpreter round i3 wave 79, lane L3: `-Xrs` / `-XX:±ReduceSignalUsage`
/// is the launcher's option here too (`VmConfig::reduce_signal_usage`),
/// last one wins, and absent leaves the signal handlers on.
#[test]
fn config_from_args_reads_reduce_signal_usage() {
    let parse = |spellings: &[&str]| {
        let owned: Vec<std::ffi::CString> = spellings
            .iter()
            .map(|s| std::ffi::CString::new(*s).unwrap())
            .collect();
        let mut opts: Vec<JavaVMOption> = owned
            .iter()
            .map(|s| JavaVMOption {
                option_string: s.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            })
            .collect();
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };
        with_fake_jdk("reduce-signal-usage", || unsafe { config_from_args(&args) })
            .expect("recognized options")
            .reduce_signal_usage
    };
    assert!(parse(&["-Xrs"]));
    assert!(parse(&["-XX:+ReduceSignalUsage"]));
    assert!(!parse(&["-Xrs", "-XX:-ReduceSignalUsage"]));
    assert!(!parse(&["-Dx=y"]));
}

/// Interpreter round i5 wave 101, lane L5
/// (`i100-L5-enable-preview-is-a-process-global-set-only-by-the-launcher`):
/// `--enable-preview` is a recognized `JavaVMOption`, as on HotSpot, and
/// sets this VM's `VmConfig::enable_preview`; absent leaves it off.
#[test]
fn config_from_args_reads_enable_preview() {
    let parse = |spellings: &[&str]| {
        let owned: Vec<std::ffi::CString> = spellings
            .iter()
            .map(|s| std::ffi::CString::new(*s).unwrap())
            .collect();
        let mut opts: Vec<JavaVMOption> = owned
            .iter()
            .map(|s| JavaVMOption {
                option_string: s.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            })
            .collect();
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };
        with_fake_jdk("enable-preview", || unsafe { config_from_args(&args) })
            .expect("recognized options")
            .enable_preview
    };
    assert!(parse(&["--enable-preview"]));
    assert!(!parse(&["-Dx=y"]));
}

/// zt t18/hunt2: `--finalization=enabled|disabled` (JEP 421) is a
/// recognized `JavaVMOption` setting `VmConfig::finalization_enabled`,
/// last one wins, absent leaves it on; another value is refused.
#[test]
fn t18_hunt2_config_from_args_reads_finalization() {
    let parse = |spellings: &[&str]| {
        let owned: Vec<std::ffi::CString> = spellings
            .iter()
            .map(|s| std::ffi::CString::new(*s).unwrap())
            .collect();
        let mut opts: Vec<JavaVMOption> = owned
            .iter()
            .map(|s| JavaVMOption {
                option_string: s.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            })
            .collect();
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };
        with_fake_jdk("finalization", || unsafe { config_from_args(&args) })
            .map(|cfg| cfg.finalization_enabled)
    };
    assert!(parse(&["-Dx=y"]).expect("recognized options"));
    assert!(!parse(&["--finalization=disabled"]).expect("recognized options"));
    assert!(
        parse(&["--finalization=disabled", "--finalization=enabled"])
            .expect("recognized options")
    );
    assert!(parse(&["--finalization=maybe"]).is_err());
}

/// zt t21/tg10: `-XX:±ClassUnloading` and `-Xnoclassgc` are recognized
/// `JavaVMOption`s setting `VmConfig::class_unloading`, last one wins,
/// absent leaves it on (HotSpot's default).
#[test]
fn zt21tg10_config_from_args_reads_class_unloading() {
    let parse = |spellings: &[&str]| {
        let owned: Vec<std::ffi::CString> = spellings
            .iter()
            .map(|s| std::ffi::CString::new(*s).unwrap())
            .collect();
        let mut opts: Vec<JavaVMOption> = owned
            .iter()
            .map(|s| JavaVMOption {
                option_string: s.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            })
            .collect();
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };
        with_fake_jdk("class-unloading", || unsafe { config_from_args(&args) })
            .expect("recognized options")
            .class_unloading
    };
    assert!(parse(&["-Dx=y"]));
    assert!(!parse(&["-XX:-ClassUnloading"]));
    assert!(!parse(&["-Xnoclassgc"]));
    assert!(!parse(&["-XX:+ClassUnloading", "-Xnoclassgc"]));
    assert!(parse(&["-Xnoclassgc", "-XX:+ClassUnloading"]));
}

/// zt t23/hunt4: `-XX:±AlwaysPreTouch` sets `VmConfig::always_pre_touch`
/// (last one wins, absent leaves HotSpot's `false`), and an obsolete
/// `-XX:±ZGenerational` is a recognized option (accepted with HotSpot's
/// warning) even with `ignoreUnrecognized` off.
#[test]
fn zt23hunt4_config_from_args_reads_always_pre_touch_and_obsolete_flags() {
    let parse = |spellings: &[&str]| {
        let owned: Vec<std::ffi::CString> = spellings
            .iter()
            .map(|s| std::ffi::CString::new(*s).unwrap())
            .collect();
        let mut opts: Vec<JavaVMOption> = owned
            .iter()
            .map(|s| JavaVMOption {
                option_string: s.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            })
            .collect();
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };
        with_fake_jdk("always-pre-touch", || unsafe { config_from_args(&args) })
            .expect("recognized options")
            .always_pre_touch
    };
    assert!(!parse(&["-Dx=y"]));
    assert!(parse(&["-XX:+AlwaysPreTouch"]));
    assert!(!parse(&["-XX:+AlwaysPreTouch", "-XX:-AlwaysPreTouch"]));
    assert!(parse(&["-XX:-AlwaysPreTouch", "-XX:+AlwaysPreTouch"]));
    assert!(!parse(&["-XX:+ZGenerational", "-XX:-ZGenerational"]));
}

/// zt t24/hunt7: on Linux `-XX:±UseTransparentHugePages` sets
/// `VmConfig::use_transparent_huge_pages` (last one wins, absent leaves
/// HotSpot's `false`); elsewhere it is an unrecognized option, as in
/// HotSpot 25, whose flag is declared in `globals_linux.hpp` only.
#[test]
fn zt24hunt7_config_from_args_reads_use_transparent_huge_pages() {
    let parse = |spellings: &[&str]| {
        let owned: Vec<std::ffi::CString> = spellings
            .iter()
            .map(|s| std::ffi::CString::new(*s).unwrap())
            .collect();
        let mut opts: Vec<JavaVMOption> = owned
            .iter()
            .map(|s| JavaVMOption {
                option_string: s.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            })
            .collect();
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };
        with_fake_jdk("use-thp", || unsafe { config_from_args(&args) })
            .map(|cfg| cfg.use_transparent_huge_pages)
    };
    assert_eq!(parse(&["-Dx=y"]).ok(), Some(false));
    if cfg!(target_os = "linux") {
        assert_eq!(parse(&["-XX:+UseTransparentHugePages"]).ok(), Some(true));
        // zt t25/hunt7b: an explicit `-XX:-UseLargePages` turns it off,
        // wherever it stands.
        assert_eq!(
            parse(&["-XX:-UseLargePages", "-XX:+UseTransparentHugePages"]).ok(),
            Some(false)
        );
        assert_eq!(
            parse(&[
                "-XX:+UseTransparentHugePages",
                "-XX:-UseTransparentHugePages"
            ])
            .ok(),
            Some(false)
        );
    } else {
        assert!(parse(&["-XX:+UseTransparentHugePages"]).is_err());
    }
}

/// Interpreter round i4 wave 84, lane L2: `-XX:±ShowCodeDetailsInException\
/// Messages` and `-XX:±ShowHiddenFrames` are recognized options here too
/// (they used to fail `JNI_CreateJavaVM` as unrecognized), last one wins,
/// and absent leaves HotSpot's defaults (on, off).
#[test]
fn config_from_args_reads_the_exception_detail_flags() {
    let parse = |spellings: &[&str]| {
        let owned: Vec<std::ffi::CString> = spellings
            .iter()
            .map(|s| std::ffi::CString::new(*s).unwrap())
            .collect();
        let mut opts: Vec<JavaVMOption> = owned
            .iter()
            .map(|s| JavaVMOption {
                option_string: s.as_ptr() as *mut c_char,
                extra_info: std::ptr::null_mut(),
            })
            .collect();
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: opts.len() as JInt,
            options: opts.as_mut_ptr(),
            ignore_unrecognized: 0,
        };
        let cfg = with_fake_jdk("exc-detail-flags", || unsafe { config_from_args(&args) })
            .expect("recognized options");
        (cfg.show_code_details_in_exception_messages, cfg.show_hidden_frames)
    };
    assert_eq!(parse(&["-Dx=y"]), (true, false));
    // Wave 85: `-XX:±StackTraceInThrowable` is recognized (an unrecognized
    // option would fail the parse above's `expect`).
    assert_eq!(parse(&["-XX:-StackTraceInThrowable"]), (true, false));
    assert_eq!(
        parse(&["-XX:-ShowCodeDetailsInExceptionMessages"]),
        (false, false)
    );
    assert_eq!(
        parse(&[
            "-XX:-ShowCodeDetailsInExceptionMessages",
            "-XX:+ShowCodeDetailsInExceptionMessages",
            "-XX:+ShowHiddenFrames",
        ]),
        (true, true)
    );
    assert_eq!(
        parse(&["-XX:+ShowHiddenFrames", "-XX:-ShowHiddenFrames"]),
        (true, false)
    );
}

// ── JDK-mode validation on the C-ABI entry point ──────────────────────
//
// `jdk-mode-determinism.md` §6.4: this path called
// `VmConfig::with_host_jdk_default()` and performed *no* validation, so an
// embedder could boot into a mode whose backing library is not present and
// only find out much later, as an unexplained `NoClassDefFoundError`.

#[test]
fn config_from_args_defaults_to_real_jdk() {
    with_fake_jdk("default-mode", || {
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: 0,
            options: std::ptr::null_mut(),
            ignore_unrecognized: 1,
        };
        let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
        assert_eq!(cfg.jdk_mode(), cratonvm_vm::config::JdkMode::Real);
        assert_eq!(
            cfg.jdk_mode(),
            cratonvm_vm::config::LAUNCHER_DEFAULT_JDK_MODE,
            "the embedding entry point must agree with the launcher default"
        );
    });
}

#[test]
fn config_from_args_pins_the_validated_java_home() {
    // The point of pinning: boot-classpath discovery inside the VM must
    // resolve the installation that was just validated, not re-run the
    // environment probe and possibly land somewhere else.
    with_fake_jdk("pin-home", || {
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: 0,
            options: std::ptr::null_mut(),
            ignore_unrecognized: 1,
        };
        let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
        assert!(
            cfg.java_home.is_some(),
            "the resolved JDK root must be pinned onto the config"
        );
    });
}

#[test]
fn config_from_args_fails_loudly_when_no_jdk_is_available() {
    with_no_jdk("no-jdk", || {
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: 0,
            options: std::ptr::null_mut(),
            ignore_unrecognized: 1,
        };
        let err = unsafe { config_from_args(&args) }
            .expect_err("real-JDK mode with no JDK must not boot an empty VM");
        match err {
            InitArgsError::JdkModeUnavailable(msg) => {
                assert!(msg.contains("no usable JDK was found"), "{msg}");
                // The message must be actionable, not just a verdict.
                assert!(msg.contains("JAVA_HOME"), "{msg}");
                assert!(msg.contains("--java-home"), "{msg}");
            }
            other => panic!("expected JdkModeUnavailable, got {other:?}"),
        }
    });
}

#[test]
fn config_from_args_rejects_both_mode_flags() {
    let synth = std::ffi::CString::new("--synthetic-jdk").unwrap();
    let real = std::ffi::CString::new("--real-jdk").unwrap();
    let mut opts = [
        JavaVMOption {
            option_string: synth.as_ptr() as *mut c_char,
            extra_info: std::ptr::null_mut(),
        },
        JavaVMOption {
            option_string: real.as_ptr() as *mut c_char,
            extra_info: std::ptr::null_mut(),
        },
    ];
    // `ignoreUnrecognized` must NOT soften this: both flags were
    // recognized, they just contradict each other.
    let args = args_with(&mut opts, 1);
    let err = unsafe { config_from_args(&args) }.expect_err("both flags must be rejected");
    match err {
        InitArgsError::InvalidOption(msg) | InitArgsError::InvalidCompatibility(msg) => {
            assert!(msg.contains("--synthetic-jdk"), "{msg}");
            assert!(
                msg.contains("--compatible") || msg.contains("--real-jdk"),
                "{msg}"
            );
        }
        other => panic!("expected InvalidOption or InvalidCompatibility, got {other:?}"),
    }
}

#[test]
fn config_from_args_honours_the_synthetic_flag_subject_to_the_cargo_feature() {
    let synth = std::ffi::CString::new("--synthetic-jdk").unwrap();
    let mut opts = [JavaVMOption {
        option_string: synth.as_ptr() as *mut c_char,
        extra_info: std::ptr::null_mut(),
    }];
    let args = args_with(&mut opts, 1);
    // Deliberately run with NO usable JDK: synthetic mode must not consult
    // the host at all. The outcome depends only on whether the stubs were
    // compiled in.
    with_no_jdk("synthetic-flag", || {
        let result = unsafe { config_from_args(&args) };
        if cratonvm_vm::config::SYNTHETIC_JDK_COMPILED_IN {
            let cfg = result.expect("synthetic mode is available in this build");
            assert_eq!(cfg.jdk_mode(), cratonvm_vm::config::JdkMode::Synthetic);
        } else {
            match result.expect_err("synthetic mode without the feature must be rejected") {
                InitArgsError::JdkModeUnavailable(msg) => {
                    assert!(msg.contains("synthetic-jdk"), "{msg}");
                    assert!(
                        msg.contains("Cargo feature"),
                        "the error must name the fix: {msg}"
                    );
                }
                other => panic!("expected JdkModeUnavailable, got {other:?}"),
            }
        }
    });
}

#[test]
fn config_from_args_honours_the_real_flag() {
    let real = std::ffi::CString::new("--real-jdk").unwrap();
    let mut opts = [JavaVMOption {
        option_string: real.as_ptr() as *mut c_char,
        extra_info: std::ptr::null_mut(),
    }];
    let args = args_with(&mut opts, 0);
    with_fake_jdk("real-flag", || {
        let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
        assert_eq!(cfg.jdk_mode(), cratonvm_vm::config::JdkMode::Real);
    });
}

// ── Compatibility mode on the C-ABI entry points ──────────────────────
//
// `docs/book/src/user-guide/real-jdk-mode.md` §6/§10 and `docs/EMBEDDING.md`
// ("Choosing a compatibility mode"). Four resolution rules are deliberate
// and each is pinned below, because each one is a plausible-looking
// "simplification" away from being wrong in a way no compiler catches:
// the default is reached by doing nothing; an unknown ABI integer is
// rejected rather than clamped; an explicit COMPATIBLE plus `--real-jdk`
// is a contradiction rather than a precedence rule; and `--real-jdk` does
// not rewrite the JDK mode.

fn jvm_option(s: &std::ffi::CString) -> JavaVMOption {
    JavaVMOption {
        option_string: s.as_ptr() as *mut c_char,
        extra_info: std::ptr::null_mut(),
    }
}

/// nio21 wave 21 L5, the host embedding check of
/// `docs/feature-designs/embedding-api.md`:
/// a `--real-jdk` VM created through the C API (`bootstrap`, with
/// `CRATONVM_EMBED_LAUNCHER_BOOT` at its default) has HotSpot's
/// `Reference Handler` and `Finalizer` daemons AND the module-declared
/// locale providers (`Locale.getAvailableLocales()` > 100, which the
/// daemons-before-the-layer order loses). Needs a real JDK 25
/// (`JAVA_HOME` / `CRATONVM_JAVA_HOME`), so it is `#[ignore]`d:
/// `cargo test -p libcratonvm --lib w21_l5_embedded -- --ignored`.
#[test]
#[ignore = "host check: needs a real JDK 25"]
fn w21_l5_embedded_jdk_only_boot_has_reference_threads_and_providers() {
    let _guard = jdk_env_lock();
    let jdk_only = CString::new("--real-jdk").unwrap();
    let mut opts = [jvm_option(&jdk_only)];
    let args = args_with(&mut opts, 0);
    let vm = cratonvm_create(&args);
    assert!(
        !vm.is_null(),
        "VM creation failed: {:?}",
        last_error_string()
    );
    let call = |class: &str, method: &str, sig: &str, a: &[CratonValue]| -> CratonValue {
        let (c, m, s) = (
            CString::new(class).unwrap(),
            CString::new(method).unwrap(),
            CString::new(sig).unwrap(),
        );
        cratonvm_invoke_static(
            vm,
            c.as_ptr(),
            m.as_ptr(),
            s.as_ptr(),
            a.as_ptr(),
            a.len() as JInt,
        )
    };
    let text_of = |obj: CratonValue| -> String {
        assert_eq!(obj.tag, craton_tag::OBJECT, "{:?}", last_error_string());
        let shown = call(
            "java/lang/String",
            "valueOf",
            "(Ljava/lang/Object;)Ljava/lang/String;",
            &[obj],
        );
        assert_eq!(shown.tag, craton_tag::OBJECT, "{:?}", last_error_string());
        let p = cratonvm_string_utf8(vm, shown.payload);
        assert!(!p.is_null(), "{:?}", last_error_string());
        // SAFETY: a NUL-terminated string this API allocated.
        let s = unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned();
        cratonvm_free_string(p);
        s
    };
    let threads = call(
        "java/lang/Thread",
        "getAllStackTraces",
        "()Ljava/util/Map;",
        &[],
    );
    let threads = text_of(threads);
    assert!(
        threads.contains("Reference Handler"),
        "no Reference Handler: {threads}"
    );
    assert!(threads.contains("Finalizer"), "no Finalizer: {threads}");
    let locales = call(
        "java/util/Locale",
        "getAvailableLocales",
        "()[Ljava/util/Locale;",
        &[],
    );
    assert_eq!(locales.tag, craton_tag::OBJECT, "{:?}", last_error_string());
    let listed = call(
        "java/util/Arrays",
        "toString",
        "([Ljava/lang/Object;)Ljava/lang/String;",
        &[locales],
    );
    let listed = text_of(listed);
    assert!(
        listed.matches(", ").count() > 100,
        "jdk.localedata providers missing: {} locales",
        listed.matches(", ").count() + 1
    );
    cratonvm_destroy(vm);
}

/// The ABI numbers are published and append-only: a compiled host carries
/// them in its `.text`, so this test failing means every deployed binary
/// that linked an older header is now wrong.
#[test]
fn compatibility_abi_values_are_published_and_append_only() {
    assert_eq!(craton_compatibility::COMPATIBLE, 0);
    assert_eq!(craton_compatibility::JDK_ONLY, 1);
    assert_eq!(
        CRATONVM_COMPATIBILITY_COMPATIBLE,
        craton_compatibility::COMPATIBLE
    );
    assert_eq!(
        CRATONVM_COMPATIBILITY_JDK_ONLY,
        craton_compatibility::JDK_ONLY
    );

    for (abi, mode) in [
        (
            CRATONVM_COMPATIBILITY_COMPATIBLE,
            CompatibilityMode::Compatible,
        ),
        (CRATONVM_COMPATIBILITY_JDK_ONLY, CompatibilityMode::JdkOnly),
    ] {
        assert_eq!(compatibility_mode_from_abi(abi), Ok(mode));
        assert_eq!(compatibility_mode_to_abi(mode), abi);
    }

    // `-1` is `cratonvm_compatibility_mode`'s bad-handle sentinel; it must
    // never become a mode value, which is why the encoding grows upwards.
    assert_ne!(CRATONVM_COMPATIBILITY_COMPATIBLE, -1);
    assert_ne!(CRATONVM_COMPATIBILITY_JDK_ONLY, -1);
}

/// An unknown ABI integer is rejected, **never clamped to COMPATIBLE**: a
/// host compiled against a newer header and run against an older library
/// must be told, not silently handed the loose policy it opted out of.
#[test]
fn unknown_compatibility_mode_is_rejected_not_clamped() {
    for bogus in [2, -1, 7, JInt::MAX, JInt::MIN] {
        match compatibility_mode_from_abi(bogus) {
            Err(InitArgsError::UnknownCompatibilityMode(v)) => assert_eq!(v, bogus),
            other => panic!("expected UnknownCompatibilityMode({bogus}), got {other:?}"),
        }
    }
    // The exact wording is quoted in docs/EMBEDDING.md.
    assert_eq!(
        InitArgsError::UnknownCompatibilityMode(2).to_string(),
        "unknown compatibility mode 2: expected 0 (compatible) or 1 (jdk-only)"
    );
}

/// The default is this crate's base config's — `with_host_jdk_default()`
/// — and it is `Compatible`. It is deliberately **not** the launcher's,
/// which has been `JdkOnly` since 2026-09-20: this constant is ABI, a
/// compiled host does not re-read the header, and a library upgrade must
/// not start refusing work the host never asked to have refused.
#[test]
fn default_compatibility_mode_is_the_embedding_default_not_the_launchers() {
    assert_eq!(
        DEFAULT_COMPATIBILITY_MODE,
        cratonvm_vm::config::EMBEDDED_HOST_JDK_DEFAULT_COMPATIBILITY_MODE
    );
    assert_eq!(DEFAULT_COMPATIBILITY_MODE, CompatibilityMode::Compatible);
    assert_ne!(
        DEFAULT_COMPATIBILITY_MODE,
        cratonvm_vm::config::LAUNCHER_DEFAULT_COMPATIBILITY_MODE,
        "re-aliasing the C ABI default to the launcher's is an ABI break"
    );

    with_fake_jdk("default-compat", || {
        let args = JavaVMInitArgs {
            version: JNI_VERSION,
            n_options: 0,
            options: std::ptr::null_mut(),
            ignore_unrecognized: 1,
        };
        let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::Compatible);
        assert!(!cfg.is_jdk_only());
    });
}

/// The whole resolution matrix, without touching the host: doing nothing
/// is compatible, either explicit route is strict, the two agreeing is
/// fine, and only the explicit contradiction fails.
#[test]
fn apply_compatibility_resolution_matrix() {
    let base = VmConfig::with_host_jdk_default();

    let cases = [
        (None, false, CompatibilityMode::Compatible),
        (None, true, CompatibilityMode::JdkOnly),
        (
            Some(CompatibilityMode::Compatible),
            false,
            CompatibilityMode::Compatible,
        ),
        (
            Some(CompatibilityMode::JdkOnly),
            false,
            CompatibilityMode::JdkOnly,
        ),
        (
            Some(CompatibilityMode::JdkOnly),
            true,
            CompatibilityMode::JdkOnly,
        ),
    ];
    for (requested, jdk_only_option, expected) in cases {
        let cfg = apply_compatibility(base.clone(), requested, jdk_only_option, false)
            .unwrap_or_else(|e| panic!("{requested:?} + {jdk_only_option} rejected: {e}"));
        assert_eq!(cfg.compatibility_mode, expected);
        // Never rewritten: the base config's JDK mode survives untouched,
        // so a later `--synthetic-jdk` conflict is still detectable.
        assert_eq!(cfg.jdk_mode(), base.jdk_mode());
    }

    // Explicit COMPATIBLE + `--real-jdk` is a contradiction, not a
    // precedence rule: both are explicit statements of policy, and picking
    // a winner would run a host under a policy neither half asked for.
    match apply_compatibility(base, Some(CompatibilityMode::Compatible), true, false) {
        Err(InitArgsError::InvalidOption(msg)) => {
            assert!(msg.contains("--real-jdk"), "{msg}");
            assert!(msg.contains("contradict"), "{msg}");
            assert!(msg.contains("CRATONVM_COMPATIBILITY_JDK_ONLY"), "{msg}");
        }
        other => panic!("expected InvalidOption contradiction, got {other:?}"),
    }
}

/// The option-string route, spelled exactly as the launcher flag — the
/// only route available to `JNI_CreateJavaVM`, which takes nothing but a
/// `JavaVMInitArgs`.
#[test]
fn jdk_only_option_string_selects_strict_without_rewriting_the_jdk_mode() {
    let jdk_only = std::ffi::CString::new("--real-jdk").unwrap();
    let mut opts = [jvm_option(&jdk_only)];
    // `ignoreUnrecognized = 0`: the flag must be *recognised*, not tolerated.
    let args = args_with(&mut opts, 0);
    with_fake_jdk("jdk-only-option", || {
        let cfg = unsafe { config_from_args(&args) }.expect("fake JDK is usable");
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::JdkOnly);
        assert!(cfg.is_jdk_only());
        // The base config was already real; nothing forced it, so the
        // `--synthetic-jdk` conflict below stays detectable.
        assert_eq!(cfg.jdk_mode(), cratonvm_vm::config::JdkMode::Real);
        assert!(cfg.execution_policy().is_jdk_only());
        assert!(cfg.execution_policy().real_jdk);
    });
}

/// The typed argument and the option string agreeing is not an error.
#[test]
fn explicit_jdk_only_argument_agrees_with_the_option_string() {
    let jdk_only = std::ffi::CString::new("--real-jdk").unwrap();
    let mut opts = [jvm_option(&jdk_only)];
    let args = args_with(&mut opts, 0);
    with_fake_jdk("jdk-only-both", || {
        let cfg = unsafe {
            config_from_args_with_compatibility(&args, Some(CompatibilityMode::JdkOnly))
        }
        .expect("the two explicit statements agree");
        assert_eq!(cfg.compatibility_mode, CompatibilityMode::JdkOnly);
    });
}

/// …and disagreeing is rejected at the entry point, with no host access
/// needed to reach the verdict.
#[test]
fn explicit_compatible_plus_jdk_only_option_is_rejected() {
    let jdk_only = std::ffi::CString::new("--real-jdk").unwrap();
    let mut opts = [jvm_option(&jdk_only)];
    // `ignoreUnrecognized` must NOT soften this: `--real-jdk` was
    // recognised, it just contradicts the typed argument.
    let args = args_with(&mut opts, 1);
    let err = unsafe {
        config_from_args_with_compatibility(&args, Some(CompatibilityMode::Compatible))
    }
    .expect_err("explicit compatible + --real-jdk must be rejected");
    match err {
        InitArgsError::InvalidOption(msg) => assert!(msg.contains("contradict"), "{msg}"),
        other => panic!("expected InvalidOption, got {other:?}"),
    }
}

/// The conflict verdict is `VmConfig::validate_compatibility`'s, carried
/// **byte for byte**. The rule is called here, never re-derived — so the C
/// ABI and the `cratonvm` launcher can never drift into reporting the same
/// conflict in different words.
#[test]
fn jdk_only_plus_synthetic_reports_validate_compatibility_verbatim() {
    let expected = VmConfig::with_host_jdk_default()
        .with_jdk_mode(cratonvm_vm::config::JdkMode::Synthetic)
        .with_compatibility_mode(CompatibilityMode::JdkOnly)
        .validate_compatibility()
        .expect_err("jdk-only + synthetic-jdk is incoherent")
        .to_string();
    assert!(
        expected.starts_with("invalid configuration: "),
        "the flat API's last error is documented as \
         \"<entry_point>: invalid configuration: …\": {expected}"
    );

    let jdk_only = std::ffi::CString::new("--real-jdk").unwrap();
    let synthetic = std::ffi::CString::new("--synthetic-jdk").unwrap();
    let mut opts = [jvm_option(&jdk_only), jvm_option(&synthetic)];
    let args = args_with(&mut opts, 1);
    let err = unsafe { config_from_args(&args) }
        .expect_err("jdk-only + synthetic-jdk must be rejected");
    match &err {
        InitArgsError::InvalidCompatibility(msg) => assert_eq!(*msg, expected),
        other => panic!("expected InvalidCompatibility, got {other:?}"),
    }
    // The `Display` is the message unchanged, so the last-error text a C
    // host reads is exactly `"<entry_point>: " + expected`.
    assert_eq!(err.to_string(), expected);
}

/// Validation order is normative: `--real-jdk --synthetic-jdk` is wrong on
/// a machine with a perfect JDK and equally wrong on one with none, so the
/// verdict must be a property of the *request*. Running the availability
/// check first would send the operator off to install a JDK to fix a flag
/// conflict.
#[test]
fn compatibility_is_validated_before_jdk_availability() {
    let jdk_only = std::ffi::CString::new("--real-jdk").unwrap();
    let synthetic = std::ffi::CString::new("--synthetic-jdk").unwrap();
    let mut opts = [jvm_option(&jdk_only), jvm_option(&synthetic)];
    let args = args_with(&mut opts, 1);
    with_no_jdk("compat-before-jdk", || {
        match unsafe { config_from_args(&args) } {
            Err(InitArgsError::InvalidCompatibility(msg)) => {
                assert!(msg.contains("--real-jdk"), "{msg}");
                assert!(msg.contains("--synthetic-jdk"), "{msg}");
            }
            other => panic!(
                "the incoherent request must be rejected on its own terms, \
                 not blamed on the host: {other:?}"
            ),
        }
    });
}

/// The capability probe needs no VM — that is its whole reason to exist,
/// since the alternative is learning the answer from a failed create.
#[test]
fn compatibility_mode_supported_probes_without_a_vm() {
    assert_eq!(
        cratonvm_compatibility_mode_supported(CRATONVM_COMPATIBILITY_COMPATIBLE),
        1
    );
    assert_eq!(
        cratonvm_compatibility_mode_supported(CRATONVM_COMPATIBILITY_JDK_ONLY),
        1
    );
    // `-1` = "not a CRATONVM_COMPATIBILITY_* value at all"; `0` stays
    // reserved for known-but-unhonourable and is never returned today.
    for bogus in [2, -1, 99] {
        assert_eq!(cratonvm_compatibility_mode_supported(bogus), -1);
    }
}

#[test]
fn compatibility_mode_null_handle_returns_minus_one() {
    clear_last_error();
    assert_eq!(cratonvm_compatibility_mode(std::ptr::null_mut()), -1);
    assert!(last_error_string().is_some());
}

#[test]
fn create_with_compatibility_rejects_an_unknown_mode_before_touching_the_vm() {
    clear_last_error();
    let vm = cratonvm_create_with_compatibility(std::ptr::null(), 2);
    assert!(vm.is_null(), "an unknown mode must not create a VM");
    let msg = last_error_string().expect("last error is set");
    assert!(
        msg.starts_with("cratonvm_create_with_compatibility: "),
        "the last error must name the entry point the host called: {msg}"
    );
    assert!(msg.contains("unknown compatibility mode 2"), "{msg}");
}

// -- Layer 2 flat C API ------------------------------------------------
//
// These exercise the FFI surface without the heavy VM bootstrap where
// possible (null-handle handling, last-error round-trip, value
// marshalling). One opt-in round-trip test that actually creates a VM is
// gated on `--cfg flat_api_live_vm` so the default `cargo test` stays
// fast and JDK-independent (mirroring the increment-1 convention of not
// booting a VM in the default unit run).

use std::ffi::CString;

/// Read the calling thread's last-error C string back into a Rust `String`
/// (helper for assertions). Returns `None` when no error is pending.
fn last_error_string() -> Option<String> {
    let p = cratonvm_last_error(std::ptr::null_mut());
    if p.is_null() {
        None
    } else {
        // SAFETY: `p` is the library-owned C string for this thread.
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
}

#[test]
fn craton_value_round_trips_primitive_tags() {
    // int / long / float / double survive a `to_value → from_value` round
    // trip with identical bit content. These tags are independent of the
    // VM (no handle-table access), so the conversion is exercised through a
    // never-dereferenced `&SharedVm` (the primitive arms ignore it). The
    // OBJECT tag, which *does* go through the per-VM handle table, is
    // covered end-to-end by `flat_api_live_round_trip`.
    let cases = [
        CratonValue {
            tag: craton_tag::INT,
            payload: (-42i32) as u32 as u64,
        },
        CratonValue {
            tag: craton_tag::LONG,
            payload: 0x0123_4567_89ab_cdef,
        },
        CratonValue {
            tag: craton_tag::FLOAT,
            payload: 1.5f32.to_bits() as u64,
        },
        CratonValue {
            tag: craton_tag::DOUBLE,
            payload: (-2.25f64).to_bits(),
        },
    ];
    // SAFETY: the primitive `to_value`/`from_value` arms never read through
    // this reference; a dangling pointer is only formed, never dereferenced.
    let shared: &SharedVm = unsafe { &*std::ptr::NonNull::<SharedVm>::dangling().as_ptr() };
    for c in cases {
        let v = c.to_value(shared).expect("primitive tag should decode");
        let back = CratonValue::from_value(shared, v);
        assert_eq!(back.tag, c.tag, "tag changed for {:?}", c.tag);
        assert_eq!(back.payload, c.payload, "payload changed for tag {}", c.tag);
    }
}

#[test]
fn craton_value_rejects_unknown_input_tags() {
    clear_last_error();
    // SAFETY: the unknown-tag path rejects before reading through `shared`.
    let shared: &SharedVm = unsafe { &*std::ptr::NonNull::<SharedVm>::dangling().as_ptr() };
    let bad = [CratonValue {
        tag: 99,
        payload: 0,
    }];
    assert!(decode_craton_args("test_api", shared, &bad).is_none());
    let err = last_error_string().expect("invalid tag should set last error");
    assert!(
        err.contains("unknown CratonValue tag 99"),
        "unexpected error text: {err}"
    );
}

#[test]
fn craton_value_rejects_stale_object_tokens() {
    clear_last_error();
    // SAFETY: this path only uses the address of `shared` as a table key.
    let shared: &SharedVm = unsafe { &*std::ptr::NonNull::<SharedVm>::dangling().as_ptr() };
    let stale = CratonValue {
        tag: craton_tag::OBJECT,
        payload: 0xfeed,
    };
    assert!(decode_craton_value("test_api", shared, stale).is_none());
    let err = last_error_string().expect("stale object token should set last error");
    assert!(
        err.contains("stale or unknown CratonRef object token 0xfeed"),
        "unexpected error text: {err}"
    );
}

#[test]
fn object_handle_dedup_uses_gc_updated_global_refs() {
    let shared = SharedVm::new(VmConfig::default());
    // SAFETY: these aligned fake refs are only stored in JniGlobalRefs and
    // compared by address; the test never dereferences them as heap objects.
    let old = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
    let moved = unsafe { ObjectRef::from_raw(0x2000 as *mut u8) };
    let reused_old_address = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };

    let first = register_handle(&shared, Some(old));
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(old.as_ptr() as usize, moved.as_ptr() as usize);
    shared
        .natives
        .jni_global_refs
        .lock()
        .update_after_gc(&pointer_map);

    let after_move = register_handle(&shared, Some(moved));
    assert_eq!(
        after_move, first,
        "moved object should keep its existing token"
    );

    let new_at_old_address = register_handle(&shared, Some(reused_old_address));
    assert_ne!(
        new_at_old_address, first,
        "a new object at the stale pre-GC address must not inherit the token"
    );

    drop_handle_table(&shared);
}

#[test]
fn release_handle_balances_deduped_object_tokens() {
    let shared = SharedVm::new(VmConfig::default());
    // SAFETY: this aligned fake ref is stored in JniGlobalRefs and compared
    // by address; the test never dereferences it as a heap object.
    let obj = unsafe { ObjectRef::from_raw(0x3000 as *mut u8) };

    let first = register_handle(&shared, Some(obj));
    let second = register_handle(&shared, Some(obj));
    assert_eq!(second, first, "same object should dedup to one token");

    assert!(release_handle(&shared, first));
    assert!(
        resolve_handle(&shared, first).is_some(),
        "one release should leave the deduped token live"
    );

    assert!(release_handle(&shared, second));
    assert!(
        resolve_handle(&shared, first).is_none(),
        "balanced releases should remove the token"
    );
    assert!(
        !release_handle(&shared, first),
        "fully released tokens must be rejected as stale"
    );

    drop_handle_table(&shared);
}

#[test]
fn null_object_handle_resolves_to_null_without_vm() {
    // The reserved null token (`0`) must short-circuit before any handle
    // table or `SharedVm` access, so a dangling reference is safe here.
    // SAFETY: `resolve_handle` returns on `h == 0` before reading `shared`.
    let shared: &SharedVm = unsafe { &*std::ptr::NonNull::<SharedVm>::dangling().as_ptr() };
    assert!(resolve_handle(shared, 0).is_none());
    // `register_handle(None)` likewise returns the null token without access.
    assert_eq!(register_handle(shared, None), 0);
    assert!(release_handle(shared, 0));
}

#[test]
fn null_handle_sets_last_error_and_returns_err() {
    clear_last_error();
    // load_class with a null VM handle → JNI_ERR + last error.
    let name = CString::new("java/lang/Object").unwrap();
    let mut out: CratonClass = u64::MAX;
    let rc = cratonvm_load_class(std::ptr::null_mut(), name.as_ptr(), &mut out);
    assert_eq!(rc, JNI_ERR);
    assert_eq!(out, u64::MAX, "out_class must be untouched on error");
    let err = last_error_string().expect("last error should be set for null handle");
    assert!(err.contains("null"), "unexpected error text: {err}");
}

#[test]
fn invoke_static_null_handle_returns_error_value() {
    clear_last_error();
    let class = CString::new("java/lang/System").unwrap();
    let method = CString::new("gc").unwrap();
    let sig = CString::new("()V").unwrap();
    let r = cratonvm_invoke_static(
        std::ptr::null_mut(),
        class.as_ptr(),
        method.as_ptr(),
        sig.as_ptr(),
        std::ptr::null(),
        0,
    );
    assert_eq!(r.tag, craton_tag::ERROR);
    assert!(last_error_string().is_some());
}

#[test]
fn new_string_null_handle_returns_null_and_sets_error() {
    clear_last_error();
    let s = CString::new("hello").unwrap();
    let h = cratonvm_new_string(std::ptr::null_mut(), s.as_ptr());
    assert_eq!(h, 0);
    assert!(last_error_string().is_some());
}

#[test]
fn string_utf8_null_handle_returns_null_and_sets_error() {
    clear_last_error();
    let p = cratonvm_string_utf8(std::ptr::null_mut(), 0);
    assert!(p.is_null());
    assert!(last_error_string().is_some());
}

#[test]
fn free_string_null_is_noop() {
    // Must not panic / segfault.
    cratonvm_free_string(std::ptr::null_mut());
}

#[test]
fn invoke_virtual_null_handle_returns_error_value() {
    clear_last_error();
    let method = CString::new("length").unwrap();
    let sig = CString::new("()I").unwrap();
    let r = cratonvm_invoke_virtual(
        std::ptr::null_mut(),
        0,
        method.as_ptr(),
        sig.as_ptr(),
        std::ptr::null(),
        0,
    );
    assert_eq!(r.tag, craton_tag::ERROR);
    assert!(last_error_string().is_some());
}

#[test]
fn invoke_virtual_null_receiver_with_live_vm_unneeded_is_error() {
    // Even with a (here null) VM handle, a 0 receiver must be rejected
    // before any VM access — the null-VM branch fires first, but this
    // pins that a 0 receiver is never dereferenced.
    clear_last_error();
    let method = CString::new("length").unwrap();
    let sig = CString::new("()I").unwrap();
    let r = cratonvm_invoke_virtual(
        std::ptr::null_mut(),
        0,
        method.as_ptr(),
        sig.as_ptr(),
        std::ptr::null(),
        0,
    );
    assert_eq!(r.tag, craton_tag::ERROR);
}

#[test]
fn object_class_null_handle_returns_err() {
    clear_last_error();
    let mut out: CratonClass = u64::MAX;
    let rc = cratonvm_object_class(std::ptr::null_mut(), 0, &mut out);
    assert_eq!(rc, JNI_ERR);
    assert_eq!(out, u64::MAX, "out_class must be untouched on error");
    assert!(last_error_string().is_some());
}

#[test]
fn class_name_null_handle_returns_null_and_sets_error() {
    clear_last_error();
    let p = cratonvm_class_name(std::ptr::null_mut(), 0);
    assert!(p.is_null());
    assert!(last_error_string().is_some());
}

#[test]
fn field_count_null_handle_returns_minus_one() {
    clear_last_error();
    assert_eq!(cratonvm_field_count(std::ptr::null_mut(), 0), -1);
    assert!(last_error_string().is_some());
}

#[test]
fn get_field_null_handle_returns_error_value() {
    clear_last_error();
    let r = cratonvm_get_field(std::ptr::null_mut(), 0, 0);
    assert_eq!(r.tag, craton_tag::ERROR);
    assert!(last_error_string().is_some());
}

#[test]
fn field_index_null_handle_returns_err() {
    clear_last_error();
    let name = CString::new("h").unwrap();
    let mut out: JInt = -999;
    let rc = cratonvm_field_index(std::ptr::null_mut(), 0, name.as_ptr(), &mut out);
    assert_eq!(rc, JNI_ERR);
    assert_eq!(out, -999, "out_index must be untouched on error");
    assert!(last_error_string().is_some());
}

#[test]
fn field_index_desc_null_handle_returns_err() {
    clear_last_error();
    let name = CString::new("h").unwrap();
    let desc = CString::new("I").unwrap();
    let mut out: JInt = -999;
    // Both with and without a descriptor, a null VM handle must short-circuit
    // to JNI_ERR before any VM access, leaving out_index untouched.
    let rc = cratonvm_field_index_desc(
        std::ptr::null_mut(),
        0,
        name.as_ptr(),
        desc.as_ptr(),
        &mut out,
    );
    assert_eq!(rc, JNI_ERR);
    assert_eq!(out, -999, "out_index must be untouched on error");
    assert!(last_error_string().is_some());
    // Null descriptor (name-only) path, same null-VM rejection.
    let rc2 = cratonvm_field_index_desc(
        std::ptr::null_mut(),
        0,
        name.as_ptr(),
        std::ptr::null(),
        &mut out,
    );
    assert_eq!(rc2, JNI_ERR);
}

#[test]
fn get_field_by_name_null_handle_returns_error_value() {
    clear_last_error();
    let name = CString::new("h").unwrap();
    let r = cratonvm_get_field_by_name(std::ptr::null_mut(), 0, name.as_ptr());
    assert_eq!(r.tag, craton_tag::ERROR);
    assert!(last_error_string().is_some());
}

#[test]
fn set_field_null_handle_returns_err() {
    clear_last_error();
    let rc = cratonvm_set_field(
        std::ptr::null_mut(),
        0,
        0,
        CratonValue {
            tag: craton_tag::INT,
            payload: 1,
        },
    );
    assert_eq!(rc, JNI_ERR);
    assert!(last_error_string().is_some());
}

#[test]
fn set_field_by_name_null_handle_returns_err() {
    clear_last_error();
    let name = CString::new("h").unwrap();
    let rc = cratonvm_set_field_by_name(
        std::ptr::null_mut(),
        0,
        name.as_ptr(),
        CratonValue {
            tag: craton_tag::INT,
            payload: 1,
        },
    );
    assert_eq!(rc, JNI_ERR);
    assert!(last_error_string().is_some());
}

#[test]
fn last_error_clear_round_trip() {
    set_last_error("boom");
    assert_eq!(last_error_string().as_deref(), Some("boom"));
    cratonvm_clear_error(std::ptr::null_mut());
    assert!(last_error_string().is_none());
}

#[test]
fn destroy_null_is_noop() {
    // Must not panic / segfault.
    cratonvm_destroy(std::ptr::null_mut());
}

#[test]
fn release_ref_null_handle_returns_err() {
    clear_last_error();
    assert_eq!(cratonvm_release_ref(std::ptr::null_mut(), 0), JNI_ERR);
    assert!(last_error_string().is_some());
}

#[cfg(not(flat_api_live_vm))]
#[test]
fn flat_vm_state_allows_only_one_active_surface() {
    {
        let mut state = FLAT_VM_STATE.lock().expect("flat state lock");
        *state = None;
    }

    {
        let _creating = begin_flat_vm_create().expect("first flat create should claim");
        assert!(
            begin_flat_vm_create().is_err(),
            "overlapping flat create must be rejected"
        );
        assert!(
            flat_vm_blocks_invocation_create(),
            "Invocation API create must be blocked while flat create is in flight"
        );
    }
    assert!(
        !flat_vm_blocks_invocation_create(),
        "dropped in-flight flat create should release the process slot"
    );

    let mut active = begin_flat_vm_create().expect("flat create should claim again");
    active.activate(0xabc0).expect("activation should succeed");
    assert!(
        begin_flat_vm_create().is_err(),
        "active flat VM must block a second flat create"
    );
    assert!(
        flat_vm_blocks_invocation_create(),
        "active flat VM must block Invocation API create"
    );

    release_flat_vm(0xdef0);
    assert!(
        flat_vm_blocks_invocation_create(),
        "wrong VM key must not release the active slot"
    );
    release_flat_vm(0xabc0);
    assert!(
        !flat_vm_blocks_invocation_create(),
        "destroying the active flat VM should release the process slot"
    );
}

#[test]
fn borrow_guard_rejects_reentrant_same_handle() {
    // A fabricated (never-dereferenced) handle address: the guard only keys
    // on the pointer value, never reads through it.
    let fake = 0xdead_beef_usize as *mut CratonVm;
    let g1 = BorrowGuard::acquire(fake).expect("first acquire must succeed");
    // A second acquire on the same handle (the re-entrant case) is rejected.
    assert!(
        BorrowGuard::acquire(fake).is_none(),
        "re-entrant acquire on the same handle must be rejected"
    );
    // A different handle is independent.
    let other = 0xfeed_face_usize as *mut CratonVm;
    let g2 = BorrowGuard::acquire(other).expect("distinct handle must acquire");
    drop(g2);
    // After the first guard drops, the handle is free again.
    drop(g1);
    let g3 = BorrowGuard::acquire(fake).expect("handle must be re-acquirable after release");
    drop(g3);
}

// Opt-in live-VM round trip: create → new_string → load_class →
// invoke_static → destroy. Gated off the default run because it performs
// the full VM bootstrap (and may require a host JDK on the classpath).
// Run with: `cargo test -p libcratonvm --lib -- --ignored` is NOT enough
// (it still compiles into the default binary); instead build with
// `RUSTFLAGS="--cfg flat_api_live_vm"`.
#[cfg(flat_api_live_vm)]
#[test]
fn flat_api_live_round_trip() {
    clear_last_error();
    let vm = cratonvm_create(std::ptr::null());
    assert!(!vm.is_null(), "create failed: {:?}", last_error_string());

    // load a bootstrap class.
    let name = CString::new("java/lang/System").unwrap();
    let mut cls: CratonClass = u64::MAX;
    assert_eq!(
        cratonvm_load_class(vm, name.as_ptr(), &mut cls),
        JNI_OK,
        "load_class failed: {:?}",
        last_error_string()
    );

    // create a string handle.
    let text = CString::new("embed").unwrap();
    let s = cratonvm_new_string(vm, text.as_ptr());
    assert_ne!(s, 0, "new_string failed: {:?}", last_error_string());

    // read the string handle back to UTF-8 and confirm the round trip.
    let back = cratonvm_string_utf8(vm, s);
    assert!(
        !back.is_null(),
        "string_utf8 failed: {:?}",
        last_error_string()
    );
    // SAFETY: `back` is a live caller-owned C string from cratonvm_string_utf8.
    let back_str = unsafe { CStr::from_ptr(back) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(back_str, "embed");
    cratonvm_free_string(back);

    // virtual dispatch: "embed".length() == 5 (descriptor excludes receiver).
    let len_m = CString::new("length").unwrap();
    let len_sig = CString::new("()I").unwrap();
    let len =
        cratonvm_invoke_virtual(vm, s, len_m.as_ptr(), len_sig.as_ptr(), std::ptr::null(), 0);
    assert_eq!(
        len.tag,
        craton_tag::INT,
        "length() failed: {:?}",
        last_error_string()
    );
    assert_eq!(len.payload as u32 as i32, 5);

    // object inspection: the string's runtime class is java/lang/String.
    let mut scls: CratonClass = u64::MAX;
    assert_eq!(
        cratonvm_object_class(vm, s, &mut scls),
        JNI_OK,
        "object_class failed: {:?}",
        last_error_string()
    );
    assert_ne!(scls, u64::MAX);
    let cname = cratonvm_class_name(vm, scls);
    assert!(
        !cname.is_null(),
        "class_name failed: {:?}",
        last_error_string()
    );
    // SAFETY: caller-owned C string from cratonvm_class_name.
    let cname_str = unsafe { CStr::from_ptr(cname) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(cname_str, "java/lang/String");
    cratonvm_free_string(cname);

    // field read-back: a String has a non-negative field count and slot 0
    // reads without error (the byte[]/coder layout is JDK-version-specific,
    // so we only assert the read-back path is sound, not a specific value).
    let fc = cratonvm_field_count(vm, s);
    assert!(fc >= 0, "field_count failed: {:?}", last_error_string());
    if fc > 0 {
        let f0 = cratonvm_get_field(vm, s, 0);
        assert_ne!(
            f0.tag,
            craton_tag::ERROR,
            "get_field(0) failed: {:?}",
            last_error_string()
        );
    }
    // out-of-range field index is a clean error, not a crash.
    let oob = cratonvm_get_field(vm, s, fc);
    assert_eq!(oob.tag, craton_tag::ERROR);

    // field-by-name resolution + read-back + write-back round trip on
    // String.hash (the non-final cached-hashCode int field present in the
    // JDK String layout).
    let hash_name = CString::new("hash").unwrap();
    let mut hash_idx: JInt = -1;
    assert_eq!(
        cratonvm_field_index(vm, scls, hash_name.as_ptr(), &mut hash_idx),
        JNI_OK,
        "field_index(String.hash) failed: {:?}",
        last_error_string()
    );
    assert!(hash_idx >= 0);
    // descriptor-aware resolution: the correct descriptor ("I") resolves to
    // the SAME slot as the name-only resolve; a wrong descriptor ("J") finds
    // no matching field; a null descriptor is name-only (same slot again).
    {
        let desc_i = CString::new("I").unwrap();
        let desc_j = CString::new("J").unwrap();
        let mut idx_i: JInt = -1;
        assert_eq!(
            cratonvm_field_index_desc(
                vm,
                scls,
                hash_name.as_ptr(),
                desc_i.as_ptr(),
                &mut idx_i
            ),
            JNI_OK,
            "field_index_desc(String.hash, I) failed: {:?}",
            last_error_string()
        );
        assert_eq!(
            idx_i, hash_idx,
            "descriptor-matched slot must equal name-only slot"
        );
        let mut idx_j: JInt = -1;
        assert_eq!(
            cratonvm_field_index_desc(
                vm,
                scls,
                hash_name.as_ptr(),
                desc_j.as_ptr(),
                &mut idx_j
            ),
            JNI_ERR,
            "field_index_desc(String.hash, J) should not match an int field"
        );
        let mut idx_n: JInt = -1;
        assert_eq!(
            cratonvm_field_index_desc(
                vm,
                scls,
                hash_name.as_ptr(),
                std::ptr::null(),
                &mut idx_n
            ),
            JNI_OK
        );
        assert_eq!(
            idx_n, hash_idx,
            "null descriptor must be name-only resolution"
        );
    }
    // name-based read agrees with index-based read at the resolved slot.
    let by_name = cratonvm_get_field_by_name(vm, s, hash_name.as_ptr());
    let by_index = cratonvm_get_field(vm, s, hash_idx);
    assert_eq!(by_name.tag, craton_tag::INT);
    assert_eq!(by_name.payload, by_index.payload);
    // write-back by name, then read it back (round trip).
    assert_eq!(
        cratonvm_set_field_by_name(
            vm,
            s,
            hash_name.as_ptr(),
            CratonValue {
                tag: craton_tag::INT,
                payload: 0x4d2
            }
        ),
        JNI_OK,
        "set_field_by_name failed: {:?}",
        last_error_string()
    );
    let after = cratonvm_get_field_by_name(vm, s, hash_name.as_ptr());
    assert_eq!(after.payload as u32 as i32, 0x4d2);
    // an unknown field name is a clean error, not a crash.
    let bogus = CString::new("no_such_field_xyz").unwrap();
    let mut bogus_idx: JInt = -1;
    assert_eq!(
        cratonvm_field_index(vm, scls, bogus.as_ptr(), &mut bogus_idx),
        JNI_ERR
    );
    assert!(last_error_string().is_some());

    // invoke a void static (System.gc) with no args.
    let m = CString::new("gc").unwrap();
    let sig = CString::new("()V").unwrap();
    let r = cratonvm_invoke_static(
        vm,
        name.as_ptr(),
        m.as_ptr(),
        sig.as_ptr(),
        std::ptr::null(),
        0,
    );
    assert_ne!(
        r.tag,
        craton_tag::ERROR,
        "invoke failed: {:?}",
        last_error_string()
    );

    // a bogus class name sets the last error.
    let bad = CString::new("no/such/Class").unwrap();
    let mut bad_cls: CratonClass = 0;
    assert_eq!(cratonvm_load_class(vm, bad.as_ptr(), &mut bad_cls), JNI_ERR);
    assert!(last_error_string().is_some());

    assert_eq!(
        cratonvm_release_ref(vm, s),
        JNI_OK,
        "release_ref failed: {:?}",
        last_error_string()
    );

    cratonvm_destroy(vm);
}

// -- Foreign-thread attach concurrent-GC soak (opt-in) -----------------
//
// End-to-end validation of the foreign-thread-attach work
// (`foreign-thread-attach.md` §6): `JNI_CreateJavaVM` once, then K host
// (non-VM-created) OS threads each `AttachCurrentThread`, loop a static
// method that allocates churny garbage while a moving young-gen GC fires
// under a small heap, then `DetachCurrentThread`.
//
// Pass criteria: no UAF / SIGSEGV, no `wait_for_all` hang (a watchdog
// aborts on deadlock), the process stays alive, every worker's calls
// executed (proving real registration, not the env-only gate-off path),
// and `alive_count` returns to the post-create baseline.
//
// Opt-in (JDK-dependent + slow + creates the process-global VM, so it must
// not share a binary with the other live-VM test). Build with the cfg and
// run with the gate on:
//   $env:CRATONVM_FOREIGN_ATTACH=1
//   $env:RUSTFLAGS="--cfg foreign_attach_soak"
//   cargo test -p libcratonvm --release foreign_attach_concurrent_gc_soak -- --nocapture
/// Debug helper: symbolize comma-separated `exe+0x<RVA>` addresses from a
/// prior crash, against THIS (same) binary. Run immediately after a crash
/// run (no rebuild) so the RVAs still map.
#[cfg(foreign_attach_soak)]
#[test]
fn dbg_symbolize_rvas() {
    let spec = Err::<String, std::env::VarError>(std::env::VarError::NotPresent).unwrap_or_default();
    let rvas: Vec<usize> = spec
        .split(',')
        .map(|s| s.trim().trim_start_matches("0x"))
        .filter(|s| !s.is_empty())
        .filter_map(|s| usize::from_str_radix(s, 16).ok())
        .collect();
    for (rva, name) in cratonvm_vm::runtime::crash_handler::symbolize_rvas(&rvas) {
        eprintln!(
            "0x{rva:x} => {}",
            name.unwrap_or_else(|| "<unresolved>".to_string())
        );        }    }