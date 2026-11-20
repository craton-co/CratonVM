#[allow(unused_imports)]
use super::*;


impl SharedVm {
    /// Create a new SharedVm from a VmConfig.
    pub fn new(mut config: VmConfig) -> Self {
        // ── Put the layout-replacement epoch counter next to the code that
        //    guards on it ─────────────────────────────────────────────────
        //
        // Deliberately the FIRST statement in the constructor, and the reason
        // is an ordering requirement rather than taste.
        // `install_layout_epoch_cell` fails closed once the counter has been
        // read, bumped or baked even once — because compiled code bakes the
        // ADDRESS and `register_class_layout` bumps whatever cell the
        // `LazyLock` resolved to, so swapping later would leave compiled
        // guards reading a word nobody increments. Every registration, every
        // JIT compile and every diagnostic that could force that resolution is
        // downstream of here.
        //
        // A `false` costs the short encoding and nothing else, which is why
        // this is not a panic. What it does NOT have is a test at this level:
        // a `cargo test` process builds many VMs and only the first could
        // install, so an assertion here would pass or fail by test order.
        // `types/tests/layout_epoch_cell.rs` pins the install and
        // `layout_epoch_cell_too_late.rs` pins the refusal, each in a process
        // of its own; what pins THIS call site is
        // `CRATONVM_DBG_EPOCH_CELL=1`, which prints the origin on any real
        // boot and reads `Heap` instead of `CodeAdjacent` the moment this
        // statement stops being first.
        //
        // The cell comes from the code cache's own allocator, which is also
        // what keeps "exactly one strategy owns placement" true:
        // `alloc_code_adjacent_cell` returns `None` while
        // `CRATONVM_JIT_CODE_NEAR_GLOBALS` is set, so that strategy and this
        // one can never both be engaged. See
        // `internal/performance/c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md`,
        // "Route 4".
        if let Some(cell) = cratonvm_jit::platform::alloc_code_adjacent_cell() {
            // SAFETY: `alloc_code_adjacent_cell` hands back a freshly carved,
            // zero-filled, 64-byte-aligned cell that no other reference names
            // and that is never unmapped or recycled — which is exactly
            // `install_layout_epoch_cell`'s contract, 4-byte alignment
            // included.
            unsafe {
                cratonvm_types::field_layout::install_layout_epoch_cell(cell.cast::<u32>());
            }
        }
        // Latch the choice HERE rather than leaving it to whoever reads the
        // counter first. Without this the decision point drifts with unrelated
        // boot order, and a regression in it would show up as a silently
        // longer encoding on some runs and not others — the failure mode this
        // whole area keeps producing.
        let epoch_cell_origin = cratonvm_types::field_layout::layout_epoch_cell_origin();
        if None::<std::ffi::OsString>.is_some() {
            let (addr, _) = cratonvm_types::field_layout::layout_replace_epoch_guard();
            eprintln!("[epoch-cell] origin={epoch_cell_origin:?} addr={addr:p}");
        }
        // t1 lane R (`t1-D-proposal-gate-bytes-in-rip-reach-of-the-code-cache`):
        // the SATB pre-gate byte gets a code-adjacent copy for the same reason
        // as the epoch counter above -- the original is a `static` in the
        // executable image, out of disp32 reach of every compiled gate. Before
        // any heap publishes its plan and before any compile, so this VM's
        // compiled reference stores bake the copy's address. One per process
        // (first install wins; a later VM sees it installed and allocates
        // nothing). `None` from the allocator, or `CRATONVM_JIT_SATB_GATE_CELL=0`,
        // leaves the static -- the long encoding, nothing else.
        if cratonvm_gc::jit_satb_pre_gate_addr()
            == &cratonvm_gc::JIT_REF_STORE_GATES.pre_active as *const _ as usize
        {
            if let Some(cell) = cratonvm_jit::platform::alloc_code_adjacent_cell() {
                // SAFETY: a fresh, zero-filled, 64-byte-aligned cell that no
                // other reference names and that is never unmapped -- a valid
                // `&'static AtomicU8`.
                let cell: &'static std::sync::atomic::AtomicU8 =
                    unsafe { &*cell.cast::<std::sync::atomic::AtomicU8>() };
                cratonvm_gc::install_jit_satb_pre_gate_cell(cell);            }        }
        if None::<std::ffi::OsString>.is_some() {
            eprintln!(
                "[satb-gate-cell] addr={:#x} static={:#x}",
                cratonvm_gc::jit_satb_pre_gate_addr(),
                &cratonvm_gc::JIT_REF_STORE_GATES.pre_active as *const _ as usize
            );
        }
        // zt t26/x21: this VM's creation, the origin of the default-shape
        // `[<uptime>s]` decoration of its start-up `[warning][pagesize]` line
        // (below) -- HotSpot's `uptime` counts from `os::init` inside
        // `JNI_CreateJavaVM`, which this constructor is.
        let vm_creation_t0 = std::time::Instant::now();

        apply_container_default_heap(&mut config);

        // Phase 2 Fast-Boot JIT Tuning: deferring compilation helps startup on
        // small hosts, but wastes parallelism on larger ones. Preserve an explicit opt-in.
        let compat_fast_boot_jit_flag =
            true;
        let compat_fast_boot_jit = fast_boot_jit_tuning_on(
            compat_fast_boot_jit_flag,
            None::<std::ffi::OsString>.is_some(),
            std::thread::available_parallelism().ok().map(|n| n.get()),
        );
        if config.compatibility_mode == CompatibilityMode::Compatible && compat_fast_boot_jit {
            let explicit_thresh = cratonvm_types::flags::runtime_var("CRATONVM_JIT_THRESHOLD").is_ok();
            if !explicit_thresh && config.jit_invocation_threshold == 200 {
                config.jit_invocation_threshold = 200;
            }
        }

        // ── Strict-mode boot precondition (real-jdk-mode.md §1.1, §8) ──────
        //
        // Deliberately the *second* statement in the constructor: before any
        // heap allocation, before the crash-handler cells are published, and
        // before `ClassManager::new` has read a byte of any classpath. A
        // `--real-jdk` run with no real JDK image cannot produce a meaningful
        // VM, so it must not produce a half-built one either — failing here
        // keeps the error a configuration error rather than a class-loading
        // mystery 3,000 classes later.
        //
        // `SharedVm::new` is infallible by signature and has several hundred
        // call sites, so this surfaces as a panic — the same mechanism
        // `reject_startup_jvmti_agents_when_disabled` (just above) already
        // uses for the same class of "the config asks for something this
        // build/host cannot do" failure. Changing the signature to
        // `Result` is a separate, much larger refactor.
        if let Err(e) = require_jdk_image_for_jdk_only(&config) {
            panic!("{e}");
        }

        // ── Publish the crash-report facts that only the config knows ──────
        //
        // The launcher prints the JDK mode on its own paths (`-version`, panic
        // hook, fatal-error arm), but the hardware-fault handler bypasses all
        // of them, and the embedding entry points (`libcratonvm`,
        // `cratonvm-embed`) have no launcher at all. `SharedVm::new` is the one
        // place every boot passes through *and* the config is in scope, so the
        // publication happens here, before anything can fault.
        //
        // Both cells are one-shot and lock-free; see the `VM diagnostic
        // snapshot` section in `runtime::crash_handler` and
        // `jdk-mode-determinism.md` §6.1.
        crate::runtime::crash_handler::publish_jdk_mode(
            config.jdk_mode(),
            config.java_home.as_deref(),
        );
        crate::runtime::crash_handler::publish_gc_algorithm(match config.gc_algorithm {
            crate::config::GcAlgorithm::Generational => "generational",
            crate::config::GcAlgorithm::G1 => "g1",
            #[cfg(feature = "zgc")]
            crate::config::GcAlgorithm::Zgc => "zgc",
        });

        // zt t26/x21 (`zgc-zt23hunt4-transparent-huge-pages-flag-ignored-*`,
        // host rows `h7b_T1` / `h7b_T4` / `h7b_T8*`): HotSpot 25 ZGC's
        // large-page decision is taken here, at VM creation, and its
        // `[warning][pagesize]` line printed now. HotSpot prints it from
        // `os::init_2` (`os::large_page_init`) and ZGC's heap reservation,
        // before a class is loaded (`[0.00Xs]`); printed at the heap's
        // construction, after the boot class path ingestion, it read
        // `[0.2xxs]`. The decision needs only the configuration and the
        // kernel's modes; the heap applies it once built (below). Under
        // `-Xlog` the line waits for the logger, which routes and decorates
        // it (`log_pagesize_warning`) and whose clock starts with it.
        // zt t26/x21 (hunt9 F3): on G1 / Generational, `os::large_page_init`'s
        // own lines (`os_large_page_warning_at_vm_creation`), as HotSpot
        // prints them under every collector.
        #[cfg(feature = "zgc")]
        let zgc_large_pages_decided = zgc_large_pages_at_vm_creation(&config);
        #[cfg(feature = "zgc")]
        let pagesize_warning = zgc_large_pages_decided
            .and_then(|decided| decided.warning)
            .or_else(|| os_large_page_warning_at_vm_creation(&config));
        #[cfg(not(feature = "zgc"))]
        let pagesize_warning = os_large_page_warning_at_vm_creation(&config);
        let mut pagesize_warning_for_the_logger: Option<&'static str> = None;
        if let Some(text) = pagesize_warning {
            if config.xlog_spec.is_some() {
                pagesize_warning_for_the_logger = Some(text);
            } else {
                use std::io::Write as _;
                let mut out = std::io::stdout().lock();
                let _ = writeln!(
                    out,
                    "{}",
                    crate::runtime::unified_logging::pagesize_warning_default_line(
                        vm_creation_t0.elapsed().as_secs_f64(),
                        text,
                    )
                );
                let _ = out.flush();
            }
        }
        // zt t27/lg1 (`zgc-zt12tg5-*` item 5, `zgc-zt13x10-review-residuals-*`
        // item 23i; host rows `tg8_s2h`, `tg10_h3`): ZGC's Linux facts -- the
        // address-space limit and `max_map_count` -- read once here, for the
        // `gc,init` block's reservation / backing lines and for HotSpot's
        // `max_map_count` warning. Without `-Xlog` the warning prints now, in
        // HotSpot's default shape (heap initialisation, before any class
        // loads); under `-Xlog` the logger prints it inside the ZGC block
        // (`log_gc_startup`).
        #[cfg(feature = "zgc")]
        let zgc_linux_backing = if config.gc_algorithm == crate::config::GcAlgorithm::Zgc {
            crate::runtime::unified_logging::zgc_linux_backing_facts()
        } else {
            None
        };
        #[cfg(not(feature = "zgc"))]
        let zgc_linux_backing: Option<crate::runtime::unified_logging::ZgcLinuxBacking> = None;
        // zki: a `[warning]` line stdout printed before the heap's `[error]`
        // lines widens their level column (HotSpot's default decorations).
        #[allow(unused_mut)]
        let mut stdout_warned_before_heap =
            pagesize_warning.is_some() && config.xlog_spec.is_none();
        if config.xlog_spec.is_none() {
            let lines = crate::runtime::unified_logging::zgc_max_map_count_warning(
                u64::try_from(config.max_heap_size).unwrap_or(u64::MAX),
                zgc_linux_backing.as_ref(),
            );
            stdout_warned_before_heap |= !lines.is_empty();
            if !lines.is_empty() {
                use std::io::Write as _;
                // HotSpot pads the tags column to the widest tag set its
                // stdout printed: `pagesize` when that warning came first.
                let width = if pagesize_warning.is_some() { 8 } else { 2 };
                let uptime = vm_creation_t0.elapsed().as_secs_f64();
                let mut out = std::io::stdout().lock();
                for text in &lines {
                    let _ = writeln!(
                        out,
                        "{}",
                        crate::runtime::unified_logging::gc_warning_default_line(
                            uptime, width, text,
                        )
                    );
                }
                let _ = out.flush();
            }
        }

        // Compact tagless field layouts are a VM-wide contract. Generational,
        // G1, and ZGC now share the same allocation predicate, field encoding,
        // oop-map scan, and relocation fixup; no collector-specific opt-out is
        // permitted because it would let JIT-baked offsets disagree with heap
        // storage.

        #[cfg(feature = "experimental-debug")]
        let mut jvmti_env = crate::jvmti::create_jvmti_env();
        #[cfg(feature = "experimental-debug")]
        load_startup_jvmti_agents(&config, &mut jvmti_env);
        #[cfg(not(feature = "experimental-debug"))]
        reject_startup_jvmti_agents_when_disabled(&config);
        // Interpreter round i3 wave 72, lane L6: the debug state is built
        // here so the VM can keep its suspension hint's handle beside its lock
        // (`DebugRealm::suspension_hint`).
        #[cfg(feature = "experimental-debug")]
        let initial_debug_state = crate::debug::DebugState::new();

        // Register the application's own directories as trusted file-I/O
        // sandbox roots. native-io confines file operations to the process
        // CWD by default; that is far too strict for a real JVM, which is
        // routinely pointed at an application installed elsewhere on disk
        // (e.g. WildFly launched from a worktree but reading `standalone.xml`
        // out of its `-Djboss.home.dir` tree). Every directory registered
        // here was supplied explicitly on the command line — the classpath
        // / `--jar`, `--java-home`, and `-D*.home` / `-D*.dir` properties —
        // and is therefore trusted. Symlink-escape and `..`-traversal
        // protection is preserved: native-io still canonicalizes and
        // containment-checks against this set.
        {
            // Classpath entries: register the entry itself if it is a
            // directory, and its parent directory either way (so sibling
            // resources next to an application jar resolve).
            for entry in &config.classpath {
                let p = std::path::Path::new(entry);
                cratonvm_native_io::add_sandbox_root(p);
                if let Some(parent) = p.parent() {
                    if !parent.as_os_str().is_empty() {
                        cratonvm_native_io::add_sandbox_root(parent);
                    }
                }
            }
            if let Some(jh) = &config.java_home {
                cratonvm_native_io::add_sandbox_root(jh);
            }
            // `-Djboss.home.dir=...`, `-Dcatalina.home=...`,
            // `-Duser.dir=...` and similar location properties name an
            // application root directory the program will read from.
            for (k, v) in &config.system_properties {
                let lk = k.to_ascii_lowercase();
                if (lk.ends_with(".home")
                    || lk.ends_with(".dir")
                    || lk.ends_with(".home.dir")
                    || lk.ends_with(".base"))
                    && std::path::Path::new(v).is_dir()
                {
                    cratonvm_native_io::add_sandbox_root(v);
                }
            }
        }

        // Auto-discover boot/ext classpath when not explicitly provided.
        //
        // When `use_synthetic_jdk` is true (default for tests), skip discovery
        // to avoid accidentally loading the entire JDK and slowing down tests.
        // When `use_synthetic_jdk` is false, always attempt discovery from:
        //   1. Explicit `java_home` in config
        //   2. `JAVA_HOME` environment variable
        //   3. `java` on PATH (auto-detected via `java -XshowSettings:properties`)
        let boot_cp = if config.boot_classpath.is_empty() && !config.use_synthetic_jdk {
            let discovered = discover_boot_classpath(config.java_home.as_deref());
            // Boot classpath discovered
            if !discovered.is_empty() {
                tracing::info!(
                    "Auto-discovered {} boot classpath entries ({} jmods)",
                    discovered.len(),
                    discovered.iter().filter(|p| p.ends_with(".jmod")).count()
                );
            }
            discovered
        } else if config.boot_classpath.is_empty() && config.java_home.is_some() {
            // Synthetic JDK mode but explicit java_home: discover for potential future use
            discover_boot_classpath(config.java_home.as_deref())
        } else {
            config.boot_classpath.clone()
        };
        // `JarFile.runtimeVersion()` for this VM: the booted library's feature
        // version (unknown or synthetic: the newest this VM serves), capped by
        // `-Djdk.util.jar.version` and turned off by
        // `-Djdk.util.jar.enableMultiRelease=false`, as `JarFile.<clinit>`
        // does with the command-line values (`ClassPath::jar_runtime_version`;
        // interpreter round i5 waves 100 and 103, lane L5). Multi-release jars
        // are read at it on the class path (`set_multi_release_feature`
        // below) and, since wave 104, on the module path (`ModulePath`'s
        // `releaseVersion` is the same `JarFile.runtimeVersion()`; the
        // module-path scans read them at 25 before). `jar_feature_given`: any
        // of the three inputs was known (else the class path keeps its default).
        let (jar_runtime_feature, jar_feature_given) = {
            let booted = (!config.use_synthetic_jdk)
                .then(|| config.java_home.as_deref().and_then(jdk_feature_from_release_file))
                .flatten();
            let cmdline_property = |key: &str| {
                config
                    .system_properties
                    .iter()
                    .rev()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
            };
            let jar_version = cmdline_property("jdk.util.jar.version");
            let enable_multi_release = cmdline_property("jdk.util.jar.enableMultiRelease");
            (
                cratonvm_classloading::ClassPath::jar_runtime_version(
                    booted.unwrap_or(crate::classloading::module::MODULE_PATH_JAR_FEATURE),
                    jar_version,
                    enable_multi_release,
                ),
                booted.is_some() || jar_version.is_some() || enable_multi_release.is_some(),
            )
        };
        // `--upgrade-module-path` and `--patch-module` (interpreter round i4
        // wave 87, lane L5). This VM defines every image class through the boot
        // class path, so an upgraded image module's root and a patched image
        // module's patch paths go IN FRONT of it: their classes and resources
        // are found before the image's. An upgrade-path module the image does
        // not hold is an ordinary module-path module; a patch of a module-path
        // module goes in front of that module's root (below).
        let upgrade_modules = crate::classloading::module::scan_module_path_at(
            &config.upgrade_module_path,
            jar_runtime_feature,
        );
        let is_image_module = |name: &str| crate::classloading::module::is_platform_module_name(name);
        let mut boot_front: Vec<String> = Vec::new();
        for (module, paths) in &config.patch_modules {
            if is_image_module(module) {
                boot_front.extend(paths.iter().cloned());
            }
        }
        boot_front.extend(
            upgrade_modules
                .iter()
                .filter(|m| is_image_module(&m.descriptor.name))
                .map(|m| m.root.clone()),
        );
        let boot_cp: Vec<String> = if boot_front.is_empty() {
            boot_cp
        } else {
            boot_front.into_iter().chain(boot_cp).collect()
        };
        let mut module_path_entries = config.module_path.clone();
        module_path_entries.extend(
            upgrade_modules
                .iter()
                .filter(|m| !is_image_module(&m.descriptor.name))
                .map(|m| m.root.clone()),
        );
        let ext_cp = if config.ext_classpath.is_empty() && !config.use_synthetic_jdk {
            discover_ext_classpath(config.java_home.as_deref())
        } else if config.ext_classpath.is_empty() && config.java_home.is_some() {
            discover_ext_classpath(config.java_home.as_deref())
        } else {
            config.ext_classpath.clone()
        };

        // ── Boot-phase timing ──────────────────────────────────────────────
        //
        // Startup is a headline JVM metric and it had no instrumentation at
        // all: the only way to find out where `SharedVm::new` spent its time
        // was to add `Instant::now()` by hand and rebuild. The three phases
        // below are the ones measurement showed actually matter (see
        // `startup-and-diagnostics.md` §2):
        //
        //   1. classpath ingestion — `ClassManager::new` constructs the
        //      bootstrap/extension/application `ClassPath`s, and `load_jmod`
        //      *eagerly inflates every `classes/` entry of every `.jmod` into
        //      an in-memory map*. `discover_boot_classpath` puts ALL of
        //      `$JAVA_HOME/jmods` on the boot classpath, so on a stock JDK 25
        //      this is ~28,000 class entries / ~136 MB of decompressed
        //      bytecode before a single Java class is loaded. This dominates.
        //   2. `bootstrap_core_classes` — ~323 named classes plus their
        //      recursive supertypes, parsed and linked before `main`.
        //   3. native registration — the `register_*` cascade.
        //
        // Emitted through `tracing` (the same channel the surrounding
        // boot-classpath log already uses), so it costs one `Instant::now()`
        // per phase and needs no new flag to be useful under `-verbose`/
        // `RUST_LOG`. Keep these three spans intact: a regression here is
        // otherwise invisible until someone notices the VM "feels slow".
        let __boot_t0 = std::time::Instant::now();
        // JPMS `--module-path` / `--add-modules`. Both were parsed by the
        // launcher into `VmConfig` (vm-cli/src/main.rs:3428/3449) and then read
        // by NOBODY — the same "parsed then ignored" shape already recorded for
        // `--add-opens`. Resolve the selected modules here, put each root on the
        // APPLICATION search path (HotSpot also defines module-path classes to
        // the application loader), and re-register the descriptors as EXPLICIT
        // modules: `ClassManager::new`'s app-class-path scan would otherwise
        // stamp them `automatic = true`, which short-circuits every exports /
        // opens check to "allow" and defeats the point of a module path.
        // Empty `module_path` => empty vec, no filesystem probing: a plain `-cp`
        // launch pays nothing.
        let resolved_modules = crate::classloading::module::resolve_module_path_at(
            &module_path_entries,
            &config.add_modules,
            jar_runtime_feature,
        );
        // The `--compatible` rows aligned with HotSpot
        // (`cratonvm_types::hotspot_alignment`), read ONCE for this VM (the
        // defaults, each row's legacy variable (`legacy_key`), then
        // `CRATONVM_COMPAT_ALIGN`) and handed to the class manager, the
        // native registry and the class realm's bools below (interpreter
        // round i1 wave 50, lane L5). Read here, before the class manager is
        // built, because the `UnflattenedAppClassPath` row decides the
        // application class path it is built over (wave 52, lane L5).
        let mut hotspot_alignments = cratonvm_types::HotSpotAlignments::from_env();
        // Graduated rows (`HotSpotAlignment::graduated`, 2026-10-11:
        // `jdk-fork-join-pool`, `jdk-streams`) are in force over a real JDK
        // whatever the set says: their reverts brought back stand-ins that
        // lost parallel-stream results and minted the StackWalker carrier. A
        // `--compatible` run that asked for one gets one stderr line, so a
        // host A/B does not read an unchanged run as "the revert did nothing".
        if !config.use_synthetic_jdk {
            let ignored = hotspot_alignments.graduated_rows_turned_off();
            if !ignored.is_empty() && !config.is_jdk_only() {
                eprintln!(
                    "[cratonvm] {} off, but over a real JDK these rows graduated \
                     (2026-10-11); the JDK's own bodies run",
                    ignored.join(", ")
                );
            }
            hotspot_alignments = hotspot_alignments.with_graduated_rows();
        }
        // HotSpot's application class path: `--real-jdk`, or `--compatible`
        // over a real JDK with the `UnflattenedAppClassPath` row (wave 52,
        // lane L5) -- and the real `URLClassLoader` bodies it needs (a Spring
        // Boot jar's `jar:nested:` URLs are readable only through them), so
        // the `JdkUrlClassLoaders` row too
        // (`NativeMethodRegistry::jdk_url_class_loaders`).
        let hotspot_app_class_path = config.is_jdk_only()
            || (!config.use_synthetic_jdk
                && hotspot_alignments
                    .contains(cratonvm_types::HotSpotAlignment::JdkUrlClassLoaders)
                && hotspot_alignments
                    .contains(cratonvm_types::HotSpotAlignment::UnflattenedAppClassPath));
        // `--real-jdk` hands the class manager the class path HotSpot's own
        // app loader holds (`jdk_only_app_class_path_entry`), so code sources
        // and resource URLs name absolute paths. `java.class.path` keeps the
        // user's spelling either way.
        let mut app_cp: Vec<String> = if hotspot_app_class_path {
            config
                .classpath
                .iter()
                .map(|entry| jdk_only_app_class_path_entry(entry))
                .collect()
        } else {
            config.classpath.clone()
        };
        // A patched module-path module's patch paths go in front of the module
        // roots (`--patch-module`, wave 87, lane L5).
        for m in &resolved_modules {
            if let Some((_, paths)) = config
                .patch_modules
                .iter()
                .find(|(name, _)| *name == m.descriptor.name)
            {
                app_cp.extend(paths.iter().cloned());
            }
        }
        app_cp.extend(resolved_modules.iter().map(|m| m.root.clone()));
        // `--real-jdk` (and the `UnflattenedAppClassPath` row): no fat-JAR or
        // web-archive emulation on the application class path. A Spring Boot
        // jar is then HotSpot's: its root, whose `JarLauncher` builds the real
        // `LaunchedClassLoader`.
        let mut class_manager = ClassManager::with_app_fat_jar_emulation(
            &boot_cp,
            &ext_cp,
            &app_cp,
            !hotspot_app_class_path,
        );
        if !resolved_modules.is_empty() {
            for m in &resolved_modules {
                class_manager
                    .module_registry
                    .register(m.descriptor.clone(), m.packages.clone());
                // A module-path module: its version shows in HotSpot's
                // module clauses, and an automatic one reads every module
                // (interpreter round i4 wave 99, lane L5).
                class_manager
                    .module_registry
                    .note_module_path_module(&m.descriptor.name, m.automatic);
                // The module's roots on the application class path, in the
                // order `app_cp` put them (patch paths, then the root), for
                // its `ModuleReader.find` (`Class.getResource` of a named
                // module-path module's class under `--compatible`;
                // interpreter round i4 wave 98, lane L3).
                let mut roots: Vec<std::path::PathBuf> = config
                    .patch_modules
                    .iter()
                    .filter(|(name, _)| *name == m.descriptor.name)
                    .flat_map(|(_, paths)| paths.iter().map(std::path::PathBuf::from))
                    .collect();
                roots.push(std::path::PathBuf::from(&m.root));
                class_manager.set_module_path_roots(&m.descriptor.name, roots);
            }
            class_manager.module_registry.build_readability_graph();
            tracing::info!(
                "module path: resolved {} module(s) from {} entry/entries: {:?}",
                resolved_modules.len(),
                config.module_path.len(),
                resolved_modules
                    .iter()
                    .map(|m| m.descriptor.name.as_str())
                    .collect::<Vec<_>>(),
            );
        }
        // ── Class-origin policy, installed before the first question ───────
        //
        // This is the *very next statement* after construction on purpose, and
        // the position is load-bearing rather than stylistic: everything below
        // that can mint a class — `application_contains_resource`'s probe,
        // `bootstrap_core_classes`, `ensure_synthetic_class`, and every
        // `load_class` — must find the policy already installed. A manager that
        // answers even one class request before `set_compatibility_mode` would
        // fabricate that class under `Compatible` rules and then be told the
        // rules were strict, i.e. exactly the un-auditable state
        // `--real-jdk` exists to make impossible (real-jdk-mode.md §5, §8).
        //
        // Read straight off the config: there is NO process global for this
        // (§2). Two VMs in one process may run under different policies, and
        // this repo has already paid for process-global native state leaking
        // across VM instances more than once.
        class_manager.set_compatibility_mode(config.compatibility_mode);
        // The rows read above, before the class manager was built.
        class_manager.set_hotspot_alignments(hotspot_alignments);
        // The application class path as JDK 25's lazy `URLClassPath` over
        // `java.class.path` ends with when a manifest `Class-Path` token makes
        // `URL` look a stream handler provider up (interpreter round i5 wave
        // 108, lane L5; probe `L5W107ClassPathDriveToken`): its jars in the
        // JDK's order, the ones lost to the provider-circularity `Error` gone.
        // `java.class.path` is the launcher jar alone under `-jar` (the jars
        // its manifest names are the JDK's to find), else the `-cp` entries
        // (`app_cp` before the module-path roots).
        if !config.use_synthetic_jdk {
            let java_class_path: Option<Vec<String>> = match config.launcher_jar.as_deref() {
                Some(jar) => app_cp
                    .first()
                    .filter(|first| {
                        std::path::Path::new(first.as_str()).file_name()
                            == std::path::Path::new(jar).file_name()
                    })
                    .map(|first| vec![first.clone()]),
                None => Some(app_cp[..config.classpath.len().min(app_cp.len())].to_vec()),
            };
            if let Some(java_class_path) = java_class_path {
                class_manager.apply_application_url_class_path_model(&java_class_path);
            }
        }
        // SBR5 lane jfr: JDK 17/21 instrument boot-loader events against the typed field.
        class_manager.set_jfr_typed_event_configuration(
            !config.use_synthetic_jdk
                && config
                    .java_home
                    .as_deref()
                    .and_then(jdk_feature_from_release_file)
                    .is_some_and(|f| f < 22),
        );
        // `--enable-preview`, this VM's own (interpreter round i5 wave 101,
        // lane L5; a process global set only by the launcher before): every
        // define checks a preview class file against it.
        class_manager.set_preview_enabled(config.enable_preview);
        // zt t18/hunt2: `--finalization=disabled` (JEP 421), before the first
        // define: HotSpot gives no class `has_finalizer` while it is off.
        class_manager.set_finalization_enabled(config.finalization_enabled);
        // Multi-release jars at the booted library's feature version, as
        // HotSpot's `JarFile.runtimeVersion()` (interpreter round i5 wave 100,
        // lane L5); unknown or synthetic: the newest this VM serves.
        // Wave 103: capped by `-Djdk.util.jar.version` and turned off by
        // `-Djdk.util.jar.enableMultiRelease=false`, as `JarFile.<clinit>` does
        // with the command-line values (`ClassPath::jar_runtime_version`).
        // Wave 104: computed once above (`jar_runtime_feature`), shared with
        // the module-path scans.
        {
            if jar_feature_given {
                class_manager.set_multi_release_feature(jar_runtime_feature);
            }
        }
        // The rows' divergence counts, one per VM, shared by the class manager
        // and the class realm (`ClassRealm::alignment_census`; wave 51, lane
        // L5).
        let alignment_census = std::sync::Arc::new(cratonvm_types::AlignmentCensus::new());
        class_manager.set_alignment_census(std::sync::Arc::clone(&alignment_census));
        // `-Xverify:all`. Same placement rule as the line above: before any
        // class is loaded, so no class escapes the policy it was started under.
        // `XverifyMode::None` is carried by `skip_verification` (which the
        // dispatcher in `vm_util` reads) and `Remote` is the default, so `All`
        // is the only mode this line has to say anything about.
        class_manager
            .set_strict_verification(config.xverify_mode == crate::config::XverifyMode::All);
        // `-Xverify:none` / `-noverify` reaches the DEFINE-time verifier too
        // (t1 wave 9, lane I24V; `ClassManager::set_skip_remote_verification`):
        // HotSpot 25 runs no type-state verifier for a non-boot class then.
        // `CRATONVM_NOVERIFY_SKIPS_DEFINE_PASS3=0` restores the full define-time
        // Pass 3 for application / platform classes.
        class_manager.set_skip_remote_verification(
            config.skip_verification
                && true,
        );
        // ... and the define-time modifier / member-name legality checks
        // (t1 wave 10, lane I24W; `ClassManager::set_noverify_relaxes_format_checks`):
        // HotSpot's `ClassFileParser::_need_verify` is false for every loader
        // then. `CRATONVM_NOVERIFY_RELAXES_FORMAT_CHECKS=0` keeps them.
        class_manager.set_noverify_relaxes_format_checks(
            config.skip_verification
                && true,
        );
        // ... and the structural ones (t1 wave 13, lane I24W;
        // `ClassManager::set_noverify_relaxes_structural_checks`).
        // `CRATONVM_NOVERIFY_RELAXES_STRUCTURAL_CHECKS=0` keeps them.
        class_manager.set_noverify_relaxes_structural_checks(
            config.skip_verification
                && true,
        );
        // `-Xverify:all` judges boot and platform classes' format too (same
        // lane; `ClassManager::set_verify_all_checks_boot_format`).
        // `CRATONVM_VERIFY_ALL_CHECKS_BOOT_FORMAT=0` keeps them relaxed.
        class_manager.set_verify_all_checks_boot_format(
            true,
        );
        // Phase 1 Fast-Boot Specialization: In --compatible mode, defer Pass 3
        // verification type-map walks for trusted bootstrap classes from define
        // time to link time (`CRATONVM_DEFINE_BOOT_MAPS_AT_LINK=0` disables).
        class_manager.set_define_boot_maps_at_link(true);
        // Match HotSpot `-Xverify:remote` (default): trusted bootstrap classes
        // keep Pass 2 structure checks and skip Pass 3 bytecode verification
        // and eager type-map walks unless `-Xverify:all` was requested.
        if config.xverify_mode != crate::config::XverifyMode::All {
            class_manager.set_skip_boot_verification(true);
        }
        let native_shim_selection =
            cratonvm_native_builtins::app_shims::ShimSelection::from_resource_probe(|resource| {
                class_manager.application_contains_resource(resource)
            });
        let vm_identity =
            vm_identity_for_layout_domain(class_manager.class_store().layout_domain());
        let __native_reg_fn = {
            let config = config.clone();
            move || {
                let __boot_t2 = std::time::Instant::now();
                let config = &config;
        cratonvm_native_builtins_jfr::install_hooks();
        cratonvm_native_builtins_jmx::install_hooks();
        let mut native_methods = NativeMethodRegistry::new();

        // ── Capability policy, installed before ANY `register_*` pass ───────
        //
        // ORDERING IS LOAD-BEARING, in both directions:
        //
        //  * `set_capabilities` must precede the `register_*` passes because
        //    `NativeMethodRegistry::register` is itself gated
        //    (`Capability::NativeRegister`), and because `register()` is what
        //    populates `sensitive_slots` — the precomputed
        //    `slot -> CapabilityKind` map that makes the dispatch-side gate an
        //    integer lookup. A policy installed after the ~3,100 boot
        //    registrations would leave every one of them unclassified, so the
        //    dispatch gate would be permanently blind even under `Enforce`.
        //  * `install_capabilities` publishes the SAME `Arc` into the process
        //    -wide `VmId -> CapabilitySet` index, which is how a native holding
        //    only a `&dyn NativeContext` reaches this VM's policy
        //    (`CapabilityCheck::check_capability`). One `Arc`, so the registry
        //    gate, the ~35 per-call-site gates in `native-builtins`/`native-io`
        //    and the dispatch gate all write to ONE audit log.
        //    It is done here, at the earliest possible point, so no boot-time
        //    native can run before the policy is visible — that window is
        //    exactly what `capability_gate`'s per-thread raw-memory memo would
        //    otherwise latch a stale `None` into.
        //
        // The default is `CapabilityMode::Permissive`: `from_env` reads
        // `CRATONVM_CAPABILITY_MODE` / `CRATONVM_CAPABILITY_GRANTS`, and with
        // neither set the result allows everything and merely counts it. So the
        // default configuration is behaviour-identical to no policy at all,
        // while `capability_audit(VmId)` can now actually report something.
        //
        // Per-VM, never a process global: two `SharedVm`s in one process get two
        // `CapabilitySet`s under two `VmId`s, which is the cross-VM policy
        // interference `native-api/src/capability.rs` exists to remove.
        let capabilities = Arc::new(cratonvm_native_api::CapabilitySet::from_env(
            cratonvm_native_api::VmId::from_raw(vm_identity),
        ));
        if capabilities.mode() != cratonvm_native_api::CapabilityMode::Permissive {
            native_methods.set_capabilities(Arc::clone(&capabilities));
        }
        // `None` — a fresh `vm_identity` can never displace an existing entry.
        let _displaced = cratonvm_native_api::install_capabilities(Arc::clone(&capabilities));

        // ── Native policy, installed before ANY `register_*` pass ──────────
        //
        // One line after construction and one line *above* the
        // `#[cfg(feature = "synthetic-jdk")]` fork, so it is structurally
        // impossible to add a registration pass to either arm that runs
        // unpoliced: there is no reachable point between `new()` and the first
        // `register_*` where the mode is not yet set. (Placing it inside the
        // arms would have meant two call sites and a standing invitation for
        // the next `register_*` to be inserted above one of them.)
        //
        // Under `JdkOnly` the registry refuses `NativeKind::SyntheticStub`
        // registrations outright and records a `SyntheticNativeRegistered`
        // violation with the registration site, which is what turns the
        // ~157-stub inventory from an assertion into evidence
        // (real-jdk-mode.md §4, §8, §10).
        //
        // Config-sourced, not a process global — see the `ClassManager` note
        // above for why (§2).
        native_methods.set_compatibility_mode(config.compatibility_mode);
        // The same rows the class manager got above, before any `register_*`
        // (the `ParallelLoaders` row gates registration).
        native_methods.set_hotspot_alignments(hotspot_alignments);
        // Which CLASS LIBRARY the registrars below are populating for — a
        // different question from the compatibility policy above, and one three
        // of them need. Set here, ahead of every `register_*` arm, because it
        // gates registration: see `NativeMethodRegistry::real_jdk`.
        native_methods.set_real_jdk(!config.use_synthetic_jdk);
        // Whether this VM compiles at all: the `MethodType.makeImpl` intern
        // memo (`CRATONVM_MH_METHOD_TYPE_INTERN_MEMO`) registers only without
        // a JIT (interpreter round i3 wave 75, lane L4).
        native_methods.set_jit_enabled(config.jit_enabled);
        // …and which image: JDK 17/21 still declare a few methods `native`
        // that JDK 25 gives bodies (`Class.getModifiers`, `getSigners`), and
        // the registrars that yield those to bytecode must not do so there.
        let jdk_feature = if config.use_synthetic_jdk {
            None
        } else {
            config
                .java_home
                .as_deref()
                .and_then(jdk_feature_from_release_file)
        };
        native_methods.set_jdk_feature(jdk_feature);
        // JDK-driven virtual threads (`VmConfig::jdk_continuations`): only a
        // real image with `jdk.internal.vm.Continuation` (21 or newer; an
        // unknown version answers as the newest, as `real_jdk_older_than`).
        let jdk_continuations = config.jdk_continuations
            && !config.use_synthetic_jdk
            && jdk_feature.map_or(true, |f| f >= 21);
        native_methods.set_jdk_continuations(jdk_continuations);
        // Interpreter round i4 wave 95, lane L6
        // (`CRATONVM_VTHREAD_RETIRE_MANAGER_ENTRIES`, default on): how many
        // terminated JDK continuations' entries the virtual-thread manager
        // keeps (`VirtualThreadManager::with_retired_jdk_entry_cap`).
        let retired_jdk_entry_cap = config
            .vthread_retire_manager_entries
            .then_some(crate::threading::virtual_threads::RETIRED_JDK_ENTRY_CAP);
        // Whether the type-annotation byte natives answer the class file's
        // bytes (`lang_class::native_member_type_annotation_bytes`): gates
        // `CRATONVM_JDK_TYPE_ANNOTATIONS` (interpreter round i2 wave 57, lane L3).
        native_methods.set_type_annotation_bytes_real(
            config.is_jdk_only() || config.compatible_reflect_type_impls,
        );
        #[cfg(feature = "synthetic-jdk")]
        {
            if config.use_synthetic_jdk {
                // Synthetic mode: register all ~5,200 Rust stubs for full JDK API coverage
                register_builtins(&mut native_methods);
                register_io_natives(&mut native_methods);
                register_collections_natives(&mut native_methods);
                // LinkedBlockingQueue.drainTo(Collection, int) - needed by SLF4J/Spring
                // Register here to ensure it's available even when class is loaded from JAR
                // [Bridge] real drainTo impl over the synthetic LBQ layout.
                                                native_methods.register(
                    "java/util/concurrent/LinkedBlockingQueue",
                    "drainTo",
                    "(Ljava/util/Collection;I)I",
                    |ctx, args| {
                        let this = match args.first() {
                            Some(cratonvm_types::Value::Object(Some(o))) => *o,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        let coll = match args.get(1) {
                            Some(cratonvm_types::Value::Object(Some(c))) => *c,
                            _ => return Ok(Some(cratonvm_types::Value::Int(0))),
                        };
                        let max_elements = match args.get(2) {
                            Some(cratonvm_types::Value::Int(n)) => *n,
                            _ => i32::MAX,
                        };
                        // gen r4w3/rooting: each `add` upcall is Java (a GC point);
                        // pin `this` / `coll` / `arr` and re-read them.
                        let this_pin = ctx.pin_native_root(this);
                        let coll_pin = ctx.pin_native_root(coll);
                        // Ensure monitor is initialized before entering
                        ctx.monitor_enter(this);
                        let this = ctx.read_native_pin(this_pin, this);
                        let size = match ctx.get_field(this, 1) {
                            cratonvm_types::Value::Int(n) => n,
                            _ => 0,
                        };
                        let arr = match ctx.get_field(this, 0) {
                            cratonvm_types::Value::Object(Some(a)) => a,
                            _ => {
                                ctx.monitor_exit(this);
                                ctx.unpin_native_roots(this_pin);
                                return Ok(Some(cratonvm_types::Value::Int(0)));
                            }
                        };
                        let arr_pin = ctx.pin_native_root(arr);
                        let to_drain = size.min(max_elements);
                        for i in 0..to_drain as usize {
                            let arr = ctx.read_native_pin(arr_pin, arr);
                            let coll = ctx.read_native_pin(coll_pin, coll);
                            let elem = ctx.get_array_element(arr, i);
                            if let Err(e) =
                                ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])
                            {
                                ctx.unpin_native_roots(this_pin);
                                return Err(e);
                            }
                        }
                        let this = ctx.read_native_pin(this_pin, this);
                        ctx.set_field(this, 1, cratonvm_types::Value::Int(size - to_drain));
                        if let Err(e) = ctx.monitor_notify_all(this) {
                            ctx.unpin_native_roots(this_pin);
                            return Err(e);
                        }
                        let this = ctx.read_native_pin(this_pin, this);
                        ctx.unpin_native_roots(this_pin);
                        ctx.monitor_exit(this);
                        Ok(Some(cratonvm_types::Value::Int(to_drain)))
                    }, cratonvm_native_api::NativeKind::Bridge,
                );
                                // The legacy monitor-backed `ReentrantLock` / `Lock` /
                // `Condition` / `Semaphore` natives.
                //
                // Real AQS became the default because the synthetic
                // lock/condition bridge deadlocks the blocking-queue producer/
                // consumer pattern, so `register_concurrent_natives` skips them
                // unless `CRATONVM_SYNTHETIC_AQS=1`. That reasoning is entirely
                // about real-JDK mode: it needs `AbstractQueuedSynchronizer`
                // bytecode to defer to. Synthetic mode has none, so the skip
                // left `new Semaphore(3)` and `new ReentrantLock()` raising
                // `UnsatisfiedLinkError` — 14 `JucComplete` corpus tests,
                // invisible for as long as the corpus was dark. Runtime-gated
                // on `use_synthetic_jdk`, not on the Cargo feature, so a
                // feature-enabled binary running real-JDK mode is unaffected.
                cratonvm_native_builtins::util_concurrent_ext::register_synthetic_aqs_natives(
                    &mut native_methods,
                );
                // Same shape, same reason, one class further on: the
                // `CyclicBarrier` natives are gated on `CRATONVM_SYNTHETIC_AQS`
                // inside `register_concurrent_natives` because the default
                // real-JDK build should run the real class. Synthetic mode has
                // no real class — `CyclicBarrier` is a 3-field compatibility
                // stub with no method bodies — so that gate left `new
                // CyclicBarrier(2)` at `NoSuchMethodError: <init>(I)V` and all
                // four `JucComplete` barrier fixtures red, with the TCK table
                // still listing them as passing. Runtime-gated on
                // `use_synthetic_jdk` (this arm), not on the flag and not on
                // the Cargo feature, so a feature-enabled binary running
                // real-JDK mode is unaffected.
                cratonvm_native_builtins::util_concurrent_ext::register_cyclic_barrier_natives(
                    &mut native_methods,
                );
            } else {
                // Real-JDK mode: register essential natives only. Do NOT use
                // register_builtins — synthetic overrides assume synthetic field
                // layouts and corrupt real JDK objects.
                //
                // `register_collections_natives` (called below) is shared with
                // synthetic mode and bundles the synthetic `java/util/StringJoiner`
                // natives (fake 5-field layout). On a real StringJoiner those
                // corrupt the object — `add` no-ops, so e.g. Spring
                // `UriComponentsBuilder.pathSegment` drops the URL path segment.
                // Tell the registry to drop StringJoiner registrations so the real,
                // self-contained bytecode runs instead.
                native_methods.set_drop_real_layout_synthetic(true);
                // NOTE: unconditionally dropping ALL SyntheticStub-tagged
                // natives in real-JDK mode was tried (dev d8092acb,
                // 2026-07-14) and reverted the same day: several register_*
                // clusters that are tagged SyntheticStub are actually needed
                // as permanent bridges in BOTH modes (no working real-bytecode
                // fallback exists), not just as fake-JDK approximations.
                // Confirmed regressions: the entire java.lang.management/JMX
                // native surface (native-builtins/src/jmx.rs -- WildFly's
                // very first ManagementFactory.getPlatformMBeanServer() call
                // NPEs deep inside real javax.management bytecode,
                // ObjectName._ca_array null, with the stub dropped) and
                // java.util.function.Function$Identity (a VM-internal
                // synthetic stand-in for the real lambda-based
                // Function.identity(), which has no real bytecode to fall
                // back to at all -- UnsatisfiedLinkError). The
                // `drop_synthetic_stubs` field's own doc comment already
                // documented the safe, original design: opt-in only, via
                // `CRATONVM_NO_STUBS`, "because some apps currently limp on
                // these fakes and dropping them surfaces real gaps as clear
                // errors." Leave it opt-in; do not force it on here. See
                // wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md's
                // 2026-07-14 addendum for the WildFly-boot regression this
                // caused and how it was found (git bisect).
                //
                // 2026-07-31, `--real-jdk` (docs/book/src/user-guide/real-jdk-mode.md):
                // strict mode does NOT re-litigate the 2026-07-14 revert above,
                // and nothing here changes. The two are different questions.
                // What the 2026-07-14 bisect actually proved is that a subset
                // of the `SyntheticStub`-tagged clusters is **mis-tagged**:
                // JMX and `Function$Identity` are permanent bridges with no
                // real-bytecode fallback, which by the contract's own taxonomy
                // (§1's terminology table) makes them `NativeKind::Bridge`, not
                // stubs. The fix is to retag them at the source, one subsystem
                // per PR, in the reclassification wave (§8 explicitly keeps
                // `native-builtins/src/lib.rs` out of scope this wave) — not to
                // flip a global drop switch again and rediscover the same
                // WildFly boot failure. Until that wave lands, `--real-jdk`'s
                // enforcement is measurement-only for this population (§10):
                // the registry records the refusal and the census names the
                // registration site, so the retagging work starts from evidence
                // instead of from another bisect.
                cratonvm_native_builtins::register_essential_natives_with_shims(
                    &mut native_methods,
                    native_shim_selection,
                );
                // Register concurrent natives (ReentrantLock, etc.) needed by real JDK classes
                // like LinkedBlockingQueue which use ReentrantLock for synchronization
                cratonvm_native_builtins::register_concurrent_natives(&mut native_methods);
                // MUST follow `register_concurrent_natives`: that call registers
                // the old constant `ForkJoinPool.awaitQuiescence` -> true, and
                // registration is last-write-wins. The real one polls this
                // crate's async worker pool, which `native-collections` cannot
                // see.
                cratonvm_native_builtins::register_forkjoin_quiescence(&mut native_methods);
                cratonvm_native_builtins::register_stamped_lock_natives(&mut native_methods);
                // java.util.logging.FileHandler's natives are registered
                // (as part of register_p61_logging) only under
                // register_synthetic_overrides, which is
                // #[cfg(feature = "synthetic-jdk")]-gated and never runs in
                // real-JDK mode -- so Spring Boot's logging-file.properties
                // (handlers=java.util.logging.FileHandler,...), which only
                // ever exercises real-JDK mode, silently fell through to the
                // real FileHandler() bytecode instead (which throws
                // NoSuchFileException trying to actually lock a real log
                // file). Register just the FileHandler natives directly here.
                // See filehandler-noarg-ctor-handler-field-layout-gap-FIXED.md.
                cratonvm_native_builtins::phases_late::register_p61_file_handler(
                    &mut native_methods,
                );
                // `URLClassLoader.close()` likewise: the synthetic-only versions
                // are written for the synthetic carrier's slots and cannot run
                // against a real `java.net.URLClassLoader`.
                cratonvm_native_builtins::servlet::register_url_classloader_close_bridge(
                    &mut native_methods,
                );

                // NOTE: no `LinkedBlockingQueue.drainTo` / `BlockingQueue.drainTo`
                // stand-ins here (interpreter round i3 wave 79, lane L1;
                // `i74-L1-real-jdk-lbq-drain-to-stand-in-reads-the-synthetic-layout`).
                // The three inline copies that sat here read the SYNTHETIC
                // two-slot layout (slot 0 an element array, slot 1 an `int`
                // size); on a real JDK 25 `LinkedBlockingQueue` slot 0 is
                // `capacity` and slot 1 the `count` `AtomicInteger`, so they
                // drained nothing and answered 0 (SLF4J's `replayEvents` dropped
                // its substitute-logger events), and on the interface they read
                // any other queue's slots the same way. The JDK bodies need
                // only `ReentrantLock` / `Condition`, which run as the JDK's.
                // The synthetic-library copies (`use_synthetic_jdk` arm above,
                // `letsgo_compat.rs`) stay.
                // [Bridge] scope for the inline `AtomicBoolean.<init>` below;
                // restored to __prev_bridge just before register_io_natives.
                                                // Do not override ScheduledThreadPoolExecutor constructors in
                // real-JDK mode. The real constructors initialize the inherited
                // ThreadPoolExecutor state (`ctl`, `workQueue`, `mainLock`,
                // `workers`) coherently. A two-slot synthetic poke here leaves
                // `getQueue()` null and breaks Tomcat ContainerBase startup.
                // CopyOnWriteArrayList addIfAbsent/contains/bulkRemove are registered
                // by `register_collections_natives` with real-JDK field resolution.
                // Do not register a synthetic two-slot layout here — it corrupts
                // JDK instances (real `array` stays null → bulkRemove NPE).
                native_methods.register(
                    "java/util/concurrent/atomic/AtomicBoolean",
                    "<init>",
                    "(Z)V",
                    |ctx, args| {
                        let this = match args.first() {
                            Some(cratonvm_types::Value::Object(Some(o))) => *o,
                            _ => return Ok(None),
                        };
                        let v = match args.get(1) {
                            Some(cratonvm_types::Value::Int(n)) if *n != 0 => 1,
                            _ => 0,
                        };
                        ctx.set_field(this, 0, cratonvm_types::Value::Int(v));
                        Ok(None)
                    }, cratonvm_native_api::NativeKind::Bridge,
                );
                                register_io_natives(&mut native_methods);
                // The phase bundles are synthetic-only, but SmallRye calls
                // ProcessHandle.current().info() in real-JDK mode as well.
                cratonvm_native_builtins::phases_late::register_p60_process_handle(
                    &mut native_methods,
                );
                // 2026-08-13 (lane F30) ARM-DRIFT FIX. `register_classvalue_natives` was
                // called only by the *other* real-JDK arm (the
                // `#[cfg(not(feature = "synthetic-jdk"))]` block below). Both arms are
                // real-JDK mode and neither one calls `register_synthetic_overrides` —
                // the registrar this registration otherwise lives behind (via
                // `register_p67_misc`) is `#[cfg(feature = "synthetic-jdk")]` AND is only
                // ever called from `register_builtins`, i.e. from the SYNTHETIC-mode arm
                // above. So a `--features synthetic-jdk` binary running real-JDK mode or
                // `--real-jdk` had no `java.lang.ClassValue` natives at all.
                // Position matches the shipping arm (io -> process-handle -> classvalue);
                // this registry is last-write-wins, so position is semantics.
                cratonvm_native_builtins::phases_late::register_classvalue_natives(
                    &mut native_methods,
                );
                // ╔══ LAST-WRITE-WINS BOUNDARY — do not reorder ═════════════╗
                //
                // Everything from here to the end of this arm is ordered
                // *semantically*, not stylistically. `NativeMethodRegistry`
                // registration is last-write-wins, and this arm deliberately
                // re-registers implementations that `register_collections_natives`
                // is known to clobber (Properties side-table below; `Random`/
                // `SecureRandom` and StringJoiner in the sibling arm). Each
                // re-registration below already carries the incident that
                // produced it. Moving, sorting or deduplicating these calls
                // silently reinstates the clobbered version — every one of
                // these comments is a bug that was found in production, not a
                // preference.
                //
                // Untangling this into an explicit precedence table is
                // deliberately deferred (real-jdk-mode.md §8): this wave is
                // measurement, not deletion. The schema-2 native census
                // (`dump_native_census_json`) now records `overwrote` and
                // `registered_by` per slot, so the wave that does the
                // untangling has evidence — which registration actually won,
                // and which one it displaced — instead of having to re-derive
                // the order by bisecting this file again.
                //
                // ╚══════════════════════════════════════════════════════════╝
                //
                // Mixed real-JDK mode still routes many collection call sites
                // through synthetic wrappers; register collection natives so
                // ArrayList/Iterator/Map operations don't fail linkage.
                register_collections_natives(&mut native_methods);
                // 2026-08-13 (lane F30) ARM-DRIFT FIX. Must follow
                // `register_collections_natives`, and must exist in BOTH real-JDK arms —
                // it existed only in the shipping arm below. In real-JDK mode
                // `java/util/Random` field 0 is the `AtomicLong seed` REFERENCE, not a
                // long, so the aliases `register_collections_natives` re-registers read 0
                // and every `nextInt/nextLong/nextDouble/...` on a seeded `Random`
                // returned 0. The `securerandom` handlers keep the seed in an
                // identity-hash-keyed side table, so they are layout-independent and must
                // win. Absent here, a `--features synthetic-jdk` binary running real-JDK
                // mode or `--real-jdk` produced all-zero `Random` output.
                cratonvm_native_builtins::securerandom::register_random_and_securerandom_natives(
                    &mut native_methods,
                );
                // Re-register the side-table-backed Properties natives AFTER
                // `register_collections_natives` (see real-JDK arm below for
                // rationale) — Surefire's BooterDeserializer needs the
                // side-table round-trip to retrieve forkNumber et al.
                cratonvm_native_builtins::properties_sidetable::register_properties_sidetable(
                    &mut native_methods,
                );
                // T12: Register JDK 25 Unsafe natives (addressSize0, fences, etc.)
                cratonvm_native_builtins::unsafe_jdk25::register_t12_unsafe_natives(
                    &mut native_methods,
                );
                // T14: Register System bootstrap natives (SystemProps$Raw, FileDescriptor, etc.)
                cratonvm_native_builtins::system_bootstrap::register_t14_system_bootstrap(
                    &mut native_methods,
                );
                // C4: Register jdk.internal.loader.BootLoader natives so its
                // <clinit> completes and BootLoader.INSTANCE is non-null;
                // downstream URLClassPath.<clinit> stops NPE'ing and
                // ClassLoader.getResources can walk the boot packages.
                cratonvm_native_builtins::boot_loader::register_boot_loader_natives(
                    &mut native_methods,
                );
                // KC26: Register NIO Path/FileSystem natives — needed because Path is an interface
                // in the real JDK (abstract methods have no Code attribute) and our synthetic
                // Path objects need native dispatch.
                cratonvm_native_builtins::phases_late::register_phase57_nio_file(
                    &mut native_methods,
                );
                // 2026-08-13 (lane F30) ARM-DRIFT FIX. The 2026-08-07 note just below
                // writes the shipping arm's order out as
                // "nio_file -> file -> jar -> bulk -> zip-output" and then omitted `file`
                // itself. The real-JDK `java.io.File` constructor runs `FileSystem.
                // normalize` bytecode this interpreter does not execute cleanly (no
                // `WinNTFileSystem.normalize` override), which is why the shipping arm
                // routes `File` constructors and metadata accessors through
                // `register_phase57_file`. Paired with the `check_override` allow-list
                // entry for `java/io/File`.
                cratonvm_native_builtins::phases_late::register_phase57_file(&mut native_methods);
                // 2026-08-07: the jar/zip bridge, which this arm was missing.
                //
                // This is a REAL-JDK arm, so it has to register what the shipping
                // real-JDK arm below registers — the two arms differ only in what
                // the feature COMPILED IN, never in which class library is loaded.
                // These three were registered in that arm and not in this one, so
                // a `--features synthetic-jdk` binary run `--fast-boot` fell back to
                // `native-io`'s `zip_real_jar` surface for `JarFile`.
                //
                // That surface builds its entry objects with `alloc_zip_entry`
                // unconditionally, so `JarFile.entries()`, `getJarEntry()` and
                // `getEntry()` all handed back a bare `java.util.zip.ZipEntry`.
                // `JarFile.entries()` is declared `Enumeration<JarEntry>`, so the
                // implicit checkcast the compiler emits at the call site threw
                // `ClassCastException: java.util.zip.ZipEntry cannot be cast to
                // java.util.jar.JarEntry` — measured by `regression-suite`'s
                // `RFileTimes`, and it would hit any caller that iterates a jar.
                // `register_p59_jar` is force-listed in `native_override.rs` for
                // exactly these methods, so registering it here makes it win, and
                // it answers `java.util.jar.JarEntry` like the shipping build.
                //
                // Kept in the shipping arm's order (nio_file → file → jar → bulk
                // → zip-output) because this registry is last-write-wins.
                cratonvm_native_builtins::phases_late::register_p59_jar(&mut native_methods);
                cratonvm_native_builtins::phases_late::register_p59_bulk_stream_transfer(
                    &mut native_methods,
                );
                cratonvm_native_builtins::phases_late::register_p59_zip_output_primitives(
                    &mut native_methods,
                );
                // 2026-08-13 (lane F30) ARM-DRIFT FIX. `register_spring_boot_logback_apply`
                // is an empty no-op today (`native-builtins/src/logging_shims.rs`, emptied
                // by the 2026-07-24 logging-bootstrap batch) — but its own doc comment says
                // it is "registered unconditionally in real-JDK mode by `vm_init.rs`",
                // which was false for this arm. Called here so the two real-JDK arms have
                // identical registrar SEQUENCES and the witness test at the bottom of this
                // file can assert that with no exception list to rot. If the body is ever
                // repopulated, both arms already receive it.
                cratonvm_native_builtins::register_spring_boot_logback_apply(&mut native_methods);
                // KC26: Register URL codec (URLDecoder/URLEncoder) natives — the real JDK
                // bytecode depends on internal sun.net classes we don't support.
                //
                // Call `register_url_codec` directly rather than the whole-module
                // `register_deprecated_io_util_natives` — that function ALSO
                // re-registers this module's Date constructors/getters/setters
                // (and everything else in the module), which is a genuine
                // duplicate registration of `java/util/Date`'s deprecated
                // multi-arg constructors: `deprecated_util.rs` registers its own
                // (Julian/Gregorian-cutover-aware, default-timezone-aware) version
                // of the same natives earlier in boot, and `NativeMethodRegistry`'s
                // registration table is last-write-wins, so this second call was
                // silently clobbering the correct implementation back to the
                // naive proleptic-Gregorian one with no timezone offset at all.
                // See `bug-h2-suite-residual-fail-triage-FIXED.md`'s
                // `TestPreparedStatement.testDate8` writeup.
                cratonvm_native_builtins::deprecated_io_util::register_url_codec(
                    &mut native_methods,
                );
                // KC26: Register Charset/StandardCharsets natives
                cratonvm_native_builtins::register_charset_natives_pub(&mut native_methods);
                // KC26: Register Charset coder natives
                cratonvm_native_builtins::phases_late::register_p58_charset_coder(
                    &mut native_methods,
                );
                // Phase B (RB.1/RB.2): real charset transcoding (overrides the
                // no-op stubs registered by register_p58_charset_coder above).
                cratonvm_native_builtins::charset::register_real_charset_natives(
                    &mut native_methods,
                );
                // KC26: Register reflection/signal/unsafe-deprecated natives
                // (needed for getCallerClass, StackWalker, etc.)
                cratonvm_native_builtins::deprecated_internal::register_deprecated_internal_natives(
                    &mut native_methods,
                );
                // KC26: ArraysSupport vectorized intrinsics (vectorizedMismatch, vectorizedHashCode)
                cratonvm_native_builtins::phases_early::register_arrays_support_natives(
                    &mut native_methods,
                );
                // KC26: StringLatin1 compareTo/getChar — native overrides to bypass
                // bytecode loop bugs in the interpreter's baload/if_icmpge interaction.
                cratonvm_native_builtins::phases_early::register_string_latin1_natives(
                    &mut native_methods,
                );
                // KC26: RunnerClassLoader.close() — the real bytecode NPEs on null
                // KC26: ClassLoader constructors — the real JDK ClassLoader.<init>
                // is extremely complex (creates ArrayList, ProtectionDomain,
                // NativeLibraries, Module, etc.) and fails on missing internals.
                // Register simplified constructors that just store the parent field.
                cratonvm_native_builtins::classloader_real::register_classloader_real_natives(
                    &mut native_methods,
                );
                // KC26: MethodType factories + MethodHandle basics — needed because
                // the real JDK bytecode for these depends on deep JDK internals
                // (MethodHandleNatives, DirectMethodHandle) we don't support.
                cratonvm_native_builtins::lang_invoke::register_phase54_method_handle(
                    &mut native_methods,
                );
                // KC26: MethodHandles.lookup() + Lookup.findStatic/findVirtual/etc.
                // Phase 63 overrides phase 54's stub find* with real implementations
                // that resolve class/method/descriptor for proper MH dispatch.
                cratonvm_native_builtins::lang_invoke::register_p63_method_handles_lookup(
                    &mut native_methods,
                );
                // KC26: MethodHandle.invoke/invokeExact — signature-polymorphic
                // dispatch that reads the target class/method/descriptor from
                // our synthetic 5-field MethodHandle and invokes the target.
                cratonvm_native_builtins::lang_invoke::register_t4_method_handle_invoke(
                    &mut native_methods,
                );
                // C5: Lookup.unreflect/unreflectGetter/unreflectSetter/
                // unreflectConstructor/permuteArguments/guardWithTest overrides.
                // Without these, real-JDK Lookup.unreflectGetter runs Java
                // bytecode that relies on MethodHandleNatives.init populating
                // the MemberName, and our Field's JDK layout must match —
                // which we fix in lang_class.rs::create_field_object.
                cratonvm_native_builtins::lang_invoke::register_t28_method_handle_completeness(
                    &mut native_methods,
                );
                // Round 85: LambdaMetafactory.metafactory/altMetafactory natives.
                // log4j ServiceLoaderUtil.callServiceLoader calls
                // LambdaMetafactory.metafactory directly (not via invokedynamic).
                // The real-JDK implementation drives InnerClassLambdaMetafactory →
                // java.lang.classfile API, which trips StackMapGenerator with
                // "Bad CP index: 0" inside our incomplete classfile shim.
                // Intercept with a native stub that returns null so the JDK path
                // is bypassed entirely (callers tolerate the missing call site).
                cratonvm_native_builtins::lang_invoke::register_p68_invoke_extras(
                    &mut native_methods,
                );
                // RA.8 + WP1.8: Real ServiceLoader.load/iterator that walks
                // META-INF/services. Registration + classpath-scan bootstrap
                // are kept together in `init_service_loader_bootstrap` so
                // Wave 2 (module-scoped extension) only has one call site
                // to extend.
                init_service_loader_bootstrap(&mut native_methods);
                // WP2.5: java.lang.reflect.Proxy natives. The bytecode
                // path goes through real-JDK `Proxy.newProxyInstance`,
                // which calls into `defineClass` which we don't fully
                // support yet (WP2.3); registering the natives here
                // intercepts the synthetic Proxy$Instance allocation
                // path that the dispatcher in interpreter.rs already
                // recognizes.
                cratonvm_native_builtins::register_reflect_proxy_natives(&mut native_methods);
                // WP2.4-A: java.lang.instrument runtime natives —
                // sun.instrument.InstrumentationImpl + the in-process
                // cratonvm.Instrument bridge used by the
                // apps/instrument_probe smoke fixture.
                crate::runtime::instrument::register_instrumentation_natives(&mut native_methods);
                // In-process self-attach (com.sun.tools.attach.VirtualMachine)
                // so runtime-attach agents (Mockito inline mock maker, JaCoCo)
                // can load themselves without `-javaagent:`.
                crate::runtime::instrument::register_self_attach_natives(&mut native_methods);
                // RKC16N.10: sun.management.VMManagementImpl natives —
                // ManagementFactory.<clinit> instantiates VMManagementImpl
                // whose <clinit> calls native helpers; without these the
                // JBoss Modules boot path (Keycloak) trips on an
                // UnsatisfiedLinkError. Lives outside register_jmx_natives
                // (which is synthetic-only) so the real-JDK path picks it up.
                cratonvm_native_builtins_jmx::jmx::register_vm_management_impl(&mut native_methods);
                // W7-50 (2026-08-12): `register_management_factory_platform_server_stub`
                // used to be called here. This is the REAL-JDK arm — the
                // `else` of `if config.use_synthetic_jdk` above — and the
                // default build's real-JDK arm below refuses this exact call
                // under JMX-CLUSTER-20260720, with a measured rationale
                // (`queryNames(null, null)` returning 0 entries because every
                // platform MXBean was being zeroed out). The feature build
                // never got that revert, so the two builds' real-JDK arms
                // disagreed on the one registration that decides whether
                // `ManagementFactory.getPlatformMBeanServer()` returns a real
                // `com.sun.jmx.mbeanserver.JmxMBeanServer` or an empty
                // synthetic `javax/management/MBeanServer`.
                //
                // With the synthetic server as the receiver, `MBeanServer`'s
                // interface natives in `native-builtins/src/jmx.rs` answer
                // instead of real bytecode, and they bind BY NAME: getAttribute
                // (jmx.rs:6677) probes `getAttribute(String)Object`, then
                // `get<Cap>()Ljava/lang/Object;` and `is<Cap>()Z` with
                // HARD-CODED return descriptors. `RJdkJmx$Counter.getValue()`
                // returns `int`, so every probe missed and the three misses
                // surfaced as three `NoSuchMethodError`s naming methods the
                // fixture's own nested class never declared. Removing the call
                // puts the real introspection back on the path and makes this
                // arm identical to the default build's.
                //
                // The bind-by-name dispatch itself is NOT fixed here: it is
                // still the only implementation synthetic mode has, and this
                // vector cannot measure it. Recorded in
                // W7-50-synthetic-jdk-strict-six.md as a live latent defect.
                // Surefire ForkedBooter: ManagementFactory.getRuntimeMXBean() and
                // friends. The real-JDK bytecode delegates to
                // `getPlatformMXBean(Class)` which throws "X is not a platform
                // management interface" because the PlatformMBeanProvider SPI
                // is not wired up. Register the synthetic-bean shortcut natives
                // so the public factory methods short-circuit to
                // alloc_runtime_mxbean / alloc_thread_mxbean / etc., letting
                // ForkedBooter.isDebugging() / dumpHelp() succeed and the
                // forked test JVM continue past constructor.
                #[cfg(feature = "management")]
                cratonvm_native_builtins_jmx::jmx::register_jmx_natives(&mut native_methods);
                // W7-50 (2026-08-12): `register_mbean_server_factory_synthetic`
                // used to be called here, under a comment reading
                // "Synthetic-Real-JDK ... Must NOT be called from the real-JDK
                // branch below — ... it broke `getPlatformMBeanServer()`
                // interface dispatch when it leaked into real mode." The
                // comment was right and its own call site was the leak: this
                // is the real-JDK arm. "The branch below" was read as the
                // `#[cfg(not(feature = "synthetic-jdk"))]` block, but the
                // relevant fork is `if config.use_synthetic_jdk`, and these
                // lines are in its `else`.
                //
                // Removing it costs synthetic mode nothing: vm_init's two
                // call sites for this function were BOTH in this arm, so the
                // synthetic arm never received it. Its only other caller is a
                // unit test (jmx.rs:7172).
                // RKC16N.11: pre-register the rest of the sun.management.*
                // native surface so future Keycloak-boot iterations don't
                // trip on missing-native errors as JMM init walks deeper.
                // All return reasonable defaults (zeros / empty arrays /
                // -1 for "metric unavailable"); JBoss only iterates these
                // MXBeans for diagnostic display, not control flow.
                cratonvm_native_builtins_jmx::jmx::register_thread_impl(&mut native_methods);
                cratonvm_native_builtins_jmx::jmx::register_class_loading_impl(&mut native_methods);
                cratonvm_native_builtins_jmx::jmx::register_garbage_collector_impl(&mut native_methods);
                // Wave 1 / Task A: per-pool / per-manager MXBean natives
                // so ManagementFactory.getXxxMXBeans() returns at least
                // one usable bean per type (not just empty arrays).
                cratonvm_native_builtins_jmx::jmx::register_memory_pool_impl(&mut native_methods);
                cratonvm_native_builtins_jmx::jmx::register_memory_manager_impl(&mut native_methods);
                cratonvm_native_builtins_jmx::jmx::register_operating_system_impl(&mut native_methods);
                cratonvm_native_builtins_jmx::jmx::register_hotspot_diagnostic(&mut native_methods);
                cratonvm_native_builtins_jmx::jmx::register_flag_impl(&mut native_methods);
                // Spring Boot 2.x fat-jars: SLF4J 1.7's MDC.<clinit> /
                // LoggerFactory.<clinit> call StaticMDCBinder.getSingleton()
                // / StaticLoggerBinder.getSingleton() which only resolve
                // when an SLF4J impl JAR (slf4j-log4j12, logback-classic,
                // slf4j-simple, ...) is on the runtime classpath. Boot's
                // nested BOOT-INF/lib/ visibility makes those JARs invisible
                // to our class loader, so the static call site raises
                // NoSuchMethodError → JIT linkage error and the boot dies.
                // Register synthetic singletons + a no-op BasicMDCAdapter so
                // <clinit> completes; the existing MDC / Logger natives
                // already cover the actual API surface.
                cratonvm_native_builtins::register_slf4j_binder_stubs_pub(&mut native_methods);
                // ApplicationStartup / StartupStep: `spring_startup_bootstrap` in essentials.
                tracing::info!(
                    "Real JDK mode: {} native methods registered",
                    native_methods.len()
                );
            }
        }
        #[cfg(not(feature = "synthetic-jdk"))]
        {
            // Real-JDK mode (the default `cratonvm-cli` build): drop the synthetic
            // `java/util/StringJoiner` natives that `register_collections_natives`
            // (called below) bundles in — their fake 5-field layout corrupts the
            // real 7-field object (`add` no-ops, so e.g. Spring
            // `UriComponentsBuilder.pathSegment` drops the URL path segment). The
            // real, self-contained StringJoiner bytecode runs instead. Must be set
            // BEFORE any `register_*` pass here. (The synthetic-jdk-feature build
            // sets the same flag in its real-JDK arm above.)
            //
            // Gated on `config.use_synthetic_jdk`, NOT on the Cargo feature —
            // the same correction `register_synthetic_aqs_natives` and
            // `register_cyclic_barrier_natives` already carry in the sibling
            // arm above, for the same reason and with the same failure shape.
            //
            // This block is `cfg(not(feature = "synthetic-jdk"))`, which is a
            // statement about the BUILD, and the flag it sets is a statement
            // about the RUN. They are not the same question.
            // `VmConfig::default()` selects `EMBEDDED_DEFAULT_JDK_MODE` =
            // `JdkMode::Synthetic` (config.rs), so every in-tree test and every
            // embedder that does not ask for a JDK boots synthetic mode in THIS
            // build too: boot-classpath discovery is skipped a few hundred lines
            // above (`!config.use_synthetic_jdk` guards it), and `java/lang/
            // String` is a minted 5-method carrier with no `Code` anywhere.
            //
            // Setting the flag there deleted the only implementation of
            // `String.length()` / `isEmpty()` / `charAt()` / `equals()` the
            // binary has — `register_essential_natives_with_shims` registers
            // them a few lines below and `register` then dropped them — so
            // `"abcdef".length()` raised `NoSuchMethodError` in a default-build
            // `Vm::new(VmConfig::new())`. The flag's own rationale ("a fake
            // 5-field layout corrupts the real 7-field object") presupposes a
            // real object; in synthetic mode there is none to protect, and the
            // drop is pure loss. Found by `wp7_2_jdbc_core_types_reachable::
            // connection_methods_carry_signatures`, whose fixture is the first
            // in-tree caller to reach `String.length()` on this path.
            if !config.use_synthetic_jdk {
                native_methods.set_drop_real_layout_synthetic(true);
            }
            // set_drop_synthetic_stubs(true) intentionally NOT called here.
            // See the matching real-JDK arm above for why (dev d8092acb
            // regression + revert, 2026-07-14): several SyntheticStub-tagged
            // register_* clusters (JMX, Function$Identity) are permanent
            // bridges needed in real mode too, not fake-Real-JDK shadows.
            //
            // 2026-07-31, `--real-jdk` (docs/book/src/user-guide/real-jdk-mode.md):
            // unchanged here too, and for the same reason as the sibling arm —
            // `drop_synthetic_stubs` stays opt-in (`CRATONVM_NO_STUBS`) in
            // BOTH arms. `--real-jdk` is a stricter, *recorded* superset of
            // that switch, not a second way to turn it on globally: the
            // mis-tagged permanent bridges (JMX, `Function$Identity`) get
            // reclassified to `NativeKind::Bridge` at their registration sites
            // in their own wave (§8), and the schema-2 census exists to tell
            // that wave which ones they are.
            cratonvm_native_builtins::register_essential_natives_with_shims(
                &mut native_methods,
                native_shim_selection,
            );
            // cratonvm-cli default features omit `synthetic-jdk`; the rich
            // registration block only lives under `cfg(feature = "synthetic-jdk")`
            // above. Real-JDK apps still need ReentrantLock / Condition / LBQ
            // drainTo natives (SLF4J replayEvents, Spring thread pools).
            cratonvm_native_builtins::register_concurrent_natives(&mut native_methods);
            // MUST follow `register_concurrent_natives`: that call registers
            // the old constant `ForkJoinPool.awaitQuiescence` -> true, and
            // registration is last-write-wins. The real one polls this
            // crate's async worker pool, which `native-collections` cannot
            // see.
            cratonvm_native_builtins::register_forkjoin_quiescence(&mut native_methods);
            cratonvm_native_builtins::register_stamped_lock_natives(&mut native_methods);
            // java.util.logging.FileHandler's natives are registered
            // (as part of register_p61_logging) only under
            // register_synthetic_overrides, which is
            // #[cfg(feature = "synthetic-jdk")]-gated and never runs in
            // real-JDK mode -- so Spring Boot's logging-file.properties
            // (handlers=java.util.logging.FileHandler,...), which only
            // ever exercises real-JDK mode, silently fell through to the
            // real FileHandler() bytecode instead (which throws
            // NoSuchFileException trying to actually lock a real log
            // file). Register just the FileHandler natives directly here.
            // See filehandler-noarg-ctor-handler-field-layout-gap-FIXED.md.
            cratonvm_native_builtins::phases_late::register_p61_file_handler(&mut native_methods);
            // See the twin above.
            cratonvm_native_builtins::servlet::register_url_classloader_close_bridge(
                &mut native_methods,
            );

            // NOTE: no `LinkedBlockingQueue.drainTo` / `BlockingQueue.drainTo`
            // stand-ins here either (interpreter round i3 wave 79, lane L1): the
            // three copies (`real_jdk_lbq_drain_to_bounded` and an inline
            // unbounded one) read the synthetic two-slot layout off a real
            // JDK 25 queue, drained nothing and answered 0. See the twin note
            // in the `synthetic-jdk` build's real-JDK arm above.
            // [Bridge] scope for the inline `AtomicBoolean.<init>` below.
            // Restored to __prev_bridge just before register_io_natives below.
                                    // NOTE: do NOT register a synthetic ScheduledThreadPoolExecutor.<init>
            // here. The synthetic 2-field poke (slots 0/1) corrupts the real STPE
            // layout (ctl/workQueue) and leaves mainLock null, so the inherited
            // real ThreadPoolExecutor.shutdownNow() NPEs on mainLock.lock() in
            // TomcatBaseTest.tearDown. The real STPE constructor bytecode runs
            // correctly on CratonVM once the synthetic STPE natives are gone (the
            // native-collections copy is now gated behind synthetic-jdk). See
            // 11-stpe-mainlock-npe-teardown-regression.md.
            // NOTE: do NOT register `CopyOnWriteArrayList.addIfAbsent` here
            // either. An inline copy over the SYNTHETIC two-slot layout (slot 0
            // the array, slot 1 an int size) sat here until interpreter round i3
            // wave 73: on a real JDK 25 COWAL slot 0 is `lock` and slot 1 is
            // `array`, so it would have replaced `lock` with an `Object[]`. It
            // never answered (last-write-wins: `register_collections_natives`
            // below installs `native-collections` `native_cowal_add_if_absent`,
            // which resolves `lock` / `array` by name, and the `JdkCopyOnWrite`
            // row drops both). Deleted with
            // `i72-L1-vm-init-real-jdk-arm-registers-a-synthetic-layout-cowal-add-if-absent-20261105`.
            native_methods.register(
                "java/util/concurrent/atomic/AtomicBoolean",
                "<init>",
                "(Z)V",
                |ctx, args| {
                    let this = match args.first() {
                        Some(cratonvm_types::Value::Object(Some(o))) => *o,
                        _ => return Ok(None),
                    };
                    let v = match args.get(1) {
                        Some(cratonvm_types::Value::Int(n)) if *n != 0 => 1,
                        _ => 0,
                    };
                    ctx.set_field(this, 0, cratonvm_types::Value::Int(v));
                    Ok(None)
                }, cratonvm_native_api::NativeKind::Bridge,
            );
                        register_io_natives(&mut native_methods);
            // Keep the default CLI's real-JDK registration in sync with the
            // synthetic-feature build above.
            cratonvm_native_builtins::phases_late::register_p60_process_handle(&mut native_methods);
            // `java.lang.ClassValue#get`/`#remove` (see `phases_late.rs`'s
            // `register_classvalue_natives`) — needed here explicitly because
            // this real-JDK-mode branch does NOT call
            // `register_synthetic_overrides` (which is where this
            // registration otherwise lives, via `register_p67_misc`), same
            // "keep in sync" reasoning as `register_p60_process_handle` above.
            cratonvm_native_builtins::phases_late::register_classvalue_natives(&mut native_methods);
            // ╔══ LAST-WRITE-WINS BOUNDARY — do not reorder ═════════════════╗
            //
            // Same contract as the sibling arm above. Registration is
            // last-write-wins, and the calls below exist *because*
            // `register_collections_natives` overwrites earlier, correct
            // implementations: `securerandom` (collections re-registers every
            // `java/util/Random` method against a synthetic 2-field layout, so
            // a seeded `Random` returned all zeroes) and
            // `properties_sidetable` (collections re-registers `Properties`
            // against the legacy HashMap layout, breaking Surefire's
            // load → stringPropertyNames → getProperty round-trip). Both
            // incidents are written up inline below. Reordering, sorting or
            // "cleaning up" this sequence reinstates the broken versions.
            //
            // Untangling is deferred by real-jdk-mode.md §8. The schema-2
            // native census now carries `overwrote` + `registered_by` per
            // slot, which is the evidence a later wave needs to replace this
            // ordering with an explicit precedence rule.
            //
            // ╚══════════════════════════════════════════════════════════════╝
            register_collections_natives(&mut native_methods);
            // Re-register the side-table-backed `java.util.Random` /
            // `SecureRandom` natives AFTER `register_collections_natives`:
            // that earlier call (native-collections `register_random_natives`)
            // re-registers every `java/util/Random` method with a SYNTHETIC
            // 2-field layout that reads the LCG seed from instance field 0.
            // In real-JDK mode field 0 is the `AtomicLong seed` reference, not
            // a long, so that version reads 0 and every `nextInt/nextLong/
            // nextDouble/...` returns 0 (a seeded `Random` produced all-zero
            // output). The `securerandom` module's handlers are layout-
            // independent (seed in an identity-hash-keyed side table) and
            // spec-exact, so they must win — same "re-register after
            // collections clobbers essentials" pattern as Properties below.
            cratonvm_native_builtins::securerandom::register_random_and_securerandom_natives(
                &mut native_methods,
            );
            // Re-register the side-table-backed Properties natives AFTER
            // `register_collections_natives` because that earlier call
            // re-registers `Properties.load`, `getProperty`, `setProperty`,
            // `put`, `get`, `stringPropertyNames` etc. with the legacy
            // HashMap-layout natives, overwriting the side-table-backed
            // implementations from `register_essential_natives`.  The
            // side-table is required for Surefire's
            // `SystemPropertyManager.loadProperties(InputStream)` round-trip
            // (load → stringPropertyNames → getProperty) so the forked JVM
            // sees `forkNumber`, `reportsDirectory`, `shutdown`, etc.
            cratonvm_native_builtins::properties_sidetable::register_properties_sidetable(
                &mut native_methods,
            );
            cratonvm_native_builtins::unsafe_jdk25::register_t12_unsafe_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::system_bootstrap::register_t14_system_bootstrap(
                &mut native_methods,
            );
            // C4: BootLoader natives (see comment at first registration site above).
            cratonvm_native_builtins::boot_loader::register_boot_loader_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::phases_late::register_phase57_nio_file(&mut native_methods);
            // Spring Boot 3.2 fat-jar launcher needs File.<init>(String) to
            // normalise URI-style `/<drive>:/...` paths so the round-trip
            // `URL.toURI().getSchemeSpecificPart() -> new File(...)` lands on
            // an existing path. The real-JDK File constructor invokes
            // FileSystem.normalize via bytecode that doesn't run cleanly in
            // our interpreter (no `WinNTFileSystem.normalize` native
            // override), so route File constructors and metadata accessors
            // through our Rust natives in `register_phase57_file`. Paired
            // with the `check_override` allow-list entry for `java/io/File`.
            cratonvm_native_builtins::phases_late::register_phase57_file(&mut native_methods);
            // Spring Boot 3.2: JarFileArchive.<init> opens the fat-jar via
            // `new JarFile(File)` and immediately calls `jarFile.stream()`
            // / `jarFile.getManifest()` to walk `BOOT-INF/lib/*.jar`. The
            // real-JDK ZipFile bytecode reaches into native primitives we
            // don't wire up, so route JarFile constructors and accessors
            // through `register_p59_jar` (which uses the `zip` crate to
            // open the archive directly). Paired with the `check_override`
            // allow-list entry for `java/util/jar/JarFile`.
            cratonvm_native_builtins::phases_late::register_p59_jar(&mut native_methods);
            // The loader's ZIP64 size-limit fixture streams six GiB through
            // `InputStream.transferTo` (via Spring's `StreamUtils.copy`).
            // Register the bulk-transfer bridge on this real-JDK path as
            // well as the full synthetic registration path.
            cratonvm_native_builtins::phases_late::register_p59_bulk_stream_transfer(
                &mut native_methods,
            );
            cratonvm_native_builtins::phases_late::register_p59_zip_output_primitives(
                &mut native_methods,
            );
            // SB3-LOGBACK: Spring Boot 3.2's DefaultLogbackConfiguration.apply
            // NPEs on its first monitorenter against a synthetic LoggerContext.
            // Register a no-op native override so the boot path skips logback's
            // default configuration (logs fall back to JVM stderr). Paired with
            // the `check_override` allow-list entry in `vm_exec.rs`.
            cratonvm_native_builtins::register_spring_boot_logback_apply(&mut native_methods);
            // Spring Boot 3 fat-jar launcher: `Launcher.createClassLoader`
            // calls `urls.toArray(new URL[0])` on the 67-element URL list
            // returned by `JarFileArchive.getClassPathUrls`. The real-JDK
            // bytecode for `ArrayList.toArray(T[])` (and the inherited
            // `AbstractCollection.toArray(T[])`) takes the
            // `Arrays.copyOf(elementData, size, a.getClass())` path which
            // NPEs in our VM because the array-component-type metadata
            // path on `Object.getClass()` for an array receiver is
            // incomplete. Register a real-JDK-aware native that reads
            // `elementData` / `size` by name (so it works against the
            // real ArrayList field layout, not the synthetic 2-field
            // stub). Paired with the `check_override` allow-list entry
            // for `java/util/ArrayList` / `java/util/AbstractCollection`
            // in `vm_exec.rs`. Synthetic-jdk mode registers the same
            // native via `register_collections_natives`; this branch
            // covers the real-JDK path which never calls that bulk
            // registration.
            fn real_jdk_to_array_typed(
                ctx: &mut dyn cratonvm_native_api::NativeContext,
                args: &[cratonvm_types::Value],
            ) -> cratonvm_types::error::MethodCallResult {
                use cratonvm_types::Value;
                // `Collection.toArray(T[])` reads `a.length` before anything
                // else, so a null template is an NPE whatever the receiver
                // holds. MEASURED: no-throw (apps/probes/ArrayListShadowSweep 135).
                if matches!(args.get(1), Some(Value::Object(None))) {
                    // HotSpot's helpful message for `a.length` (MEASURED,
                    // `R15Compat5NullArgMore` row 4; JIT round 15 wave 5,
                    // lane compat6); `=0` keeps the bare NPE.
                    let message = true
                    .then(|| "Cannot read the array length because \"a\" is null".to_string());
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message,
                    }
                    .into());
                }
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let template = args.get(1).copied().unwrap_or(Value::Object(None));
                // Read fields by name so the same native works for
                // ArrayList, Vector, CopyOnWriteArrayList, etc.
                let data = match ctx.get_field_by_name(this, "elementData") {
                    Value::Object(Some(arr)) => Some(arr),
                    _ => None,
                };
                // S111r32 — when the receiver lacks `elementData` (e.g.
                // EnumSet, HashSet, TreeSet, IdentityHashMap.values()),
                // we MUST NOT take the ArrayList shortcut: it would read
                // `size`=0 and silently return an empty array, dropping
                // the real elements. Iterate via the receiver's own
                // `iterator()` instead — same shape as
                // `AbstractCollection.toArray(T[])` in the JDK.
                if data.is_none() {
                    // Iterator-based copy using virtual dispatch on the
                    // receiver's actual class.
                    //
                    // Every call below goes through `invoke_virtual`, which
                    // dispatches on the RECEIVER. The by-name `ctx.invoke(
                    // <class>, ...)` this used to call resolves against the
                    // named class's own bytecode and never consults
                    // `should_force_registered_native_over_bytecode` — the gate
                    // that exists precisely because a CratonVM-minted
                    // `HashMap$KeyIterator` / `LinkedHashMap$LinkedKeyIterator`
                    // carries its snapshot PAST the fields the real
                    // `HashIterator` bytecode walks (`key_itr_base`), so the
                    // real `hasNext()` reads an unset `next` field and answers
                    // `false` on the FIRST element. The loop then broke at
                    // i = 0 and every slot of the freshly allocated result
                    // stayed null while `size()` had already fixed the length —
                    // a right-length, all-null array, which is the single
                    // hardest shape for a caller to notice.
                    //
                    // That is the whole of the Jetty embedded-JSP failure:
                    // `ClassMatcher extends AbstractSet<String>` with
                    // `iterator()` = `_entries.keySet().iterator()`, so
                    // `ClassMatcher.getPatterns()` (`toArray(new String[size])`)
                    // handed Jetty `[null]`. An all-null pattern set makes
                    // `IncludeExcludeSet` answer "empty", and an EMPTY
                    // hidden-class matcher matches EVERYTHING
                    // (`ClassMatcher.combine`: empty patterns fall through to
                    // the empty location set, whose `test` is vacuously true).
                    // `WebAppClassLoader.loadClass` therefore discarded the
                    // `org.apache.jasper.servlet.JspServlet` its parent had just
                    // resolved, as "hidden", and threw
                    // `ClassNotFoundException` from line 540 with no cause —
                    // surfacing as `UnavailableException: Class loading error
                    // for holder jsp==...JspServlet`.
                    //
                    // GC-safety (DOM17 stale-canary backtrace, 2026-07-15):
                    // every `ctx.invoke` below can run a moving GC, and the
                    // pre-fix loop re-used `this`, the template array, the
                    // target array, and the iterator raw across those windows
                    // — the WildFly Host Controller tripped
                    // CRATONVM_DBG_STALE_OBJREF passing the stale iterator to
                    // `next()`. Pin each and re-read through the pin before
                    // every post-window use.
                    let pin_base = ctx.pin_native_root(this);
                    let template_pin = match template {
                        Value::Object(Some(arr)) => Some((ctx.pin_native_root(arr), arr)),
                        _ => None,
                    };
                    let result = (|| -> cratonvm_types::error::MethodCallResult {
                        let size_v = ctx.invoke_virtual(this, "size", "()I", &[])?;
                        let size = match size_v {
                            Some(Value::Int(n)) => n.max(0) as usize,
                            _ => 0,
                        };
                        let target = match template_pin {
                            // Template too small: `Collection.toArray(T[])` must
                            // return a NEW array of the template's RUNTIME type,
                            // not a bare `Object[]`. An array's heap header stores
                            // its component class id, so `class_id_of_object(arr)`
                            // IS the component id `new_ref_array` wants —
                            // preserving multi-dimensional element types
                            // (`Value[][]` for H2 SortOrder.sort).
                            Some((h, orig)) => {
                                let arr = ctx.read_native_pin(h, orig);
                                if ctx.array_length(arr) >= size {
                                    arr
                                } else {
                                    let comp = ctx.class_id_of_object(arr);
                                    ctx.new_ref_array(comp, size)
                                }
                            }
                            _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, size),
                        };
                        let target_pin = ctx.pin_native_root(target);
                        let cur_this = ctx.read_native_pin(pin_base, this);
                        let it_v = ctx.invoke_virtual(
                            cur_this,
                            "iterator",
                            "()Ljava/util/Iterator;",
                            &[],
                        )?;
                        let it = match it_v {
                            Some(Value::Object(Some(o))) => o,
                            _ => {
                                let target = ctx.read_native_pin(target_pin, target);
                                return Ok(Some(Value::Object(Some(target))));
                            }
                        };
                        let it_pin = ctx.pin_native_root(it);
                        for i in 0..size {
                            let cur_it = ctx.read_native_pin(it_pin, it);
                            let has = ctx.invoke_virtual(cur_it, "hasNext", "()Z", &[])?;
                            if !matches!(has, Some(Value::Int(1))) {
                                break;
                            }
                            let cur_it = ctx.read_native_pin(it_pin, it);
                            let nxt =
                                ctx.invoke_virtual(cur_it, "next", "()Ljava/lang/Object;", &[])?;
                            let v = nxt.unwrap_or(Value::Object(None));
                            let cur_target = ctx.read_native_pin(target_pin, target);
                            // `AbstractCollection.toArray(T[])` stores through
                            // `aastore`, so a narrowing element is an
                            // `ArrayStoreException` naming the VALUE's class --
                            // not arraycopy's sentence, which is the other
                            // route's. MEASURED: a `HashSet<Object>` holding an
                            // Integer, `toArray(new String[0])` ->
                            // `ArrayStoreException: java.lang.Integer`.
                            if let Some(e) = cratonvm_native_api::array_store::reject_unstorable(
                                ctx,
                                cur_target,
                                v,
                                cratonvm_native_api::array_store::StoreRoute::Aastore,
                            ) {
                                return Err(e);
                            }
                            ctx.set_array_element(cur_target, i, v);
                        }
                        let target = ctx.read_native_pin(target_pin, target);
                        let target_len = ctx.array_length(target);
                        if target_len > size {
                            ctx.set_array_element(target, size, Value::Object(None));
                        }
                        Ok(Some(Value::Object(Some(target))))
                    })();
                    ctx.unpin_native_roots(pin_base);
                    return result;
                }
                // ArrayList-shaped: use `size` field directly.
                let size = match ctx.get_field_by_name(this, "size") {
                    Value::Int(s) => s.max(0) as usize,
                    _ => 0,
                };
                // GC-safety: allocating `target` below can move `elementData`
                // — pin it BEFORE the allocation and re-read after.
                let d_pin = data.map(|d| (ctx.pin_native_root(d), d));
                let target = match template {
                    Value::Object(Some(arr)) if ctx.array_length(arr) >= size => arr,
                    // Template too small: allocate a NEW array of the template's
                    // runtime component type (see the iterator-path comment above),
                    // not a bare `Object[]`. Fixes the H2 `SortOrder.sort`
                    // `rows.toArray(new Value[0][])` CCE (`Object -> [[Value`).
                    Value::Object(Some(arr)) => {
                        let comp = ctx.class_id_of_object(arr);
                        ctx.new_ref_array(comp, size)
                    }
                    _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, size),
                };
                if let Some((h, orig)) = d_pin {
                    let d = ctx.read_native_pin(h, orig);
                    let d_len = ctx.array_length(d);
                    let copy = size.min(d_len);
                    for i in 0..copy {
                        let v = ctx.get_array_element(d, i);
                        // `ArrayList.toArray(T[])` is a NARROWING copy through
                        // `Arrays.copyOf` / `System.arraycopy`, so arraycopy's
                        // wording, with the source named `java.lang.Object[]`
                        // because `elementData` is one whatever the list's
                        // element type. This native wrote every element
                        // unchecked, so compatible mode produced a `String[]`
                        // holding an `Integer`.
                        if let Some(e) = cratonvm_native_api::array_store::reject_unstorable(
                            ctx,
                            target,
                            v,
                            cratonvm_native_api::array_store::StoreRoute::Arraycopy {
                                source_component: "java/lang/Object",
                            },
                        ) {
                            ctx.unpin_native_roots(h);
                            return Err(e);
                        }
                        ctx.set_array_element(target, i, v);
                    }
                    ctx.unpin_native_roots(h);
                }
                let target_len = ctx.array_length(target);
                if target_len > size {
                    ctx.set_array_element(target, size, Value::Object(None));
                }
                Ok(Some(Value::Object(Some(target))))
            }
            // [Bridge] real-JDK-aware toArray(T[]) over the real ArrayList/
            // AbstractCollection field layout (reads elementData/size by name,
            // iterator fallback). Restored to __prev_toarray after.
                                    native_methods.register(
                "java/util/ArrayList",
                "toArray",
                "([Ljava/lang/Object;)[Ljava/lang/Object;",
                real_jdk_to_array_typed, cratonvm_native_api::NativeKind::Bridge,
            );
            native_methods.register(
                "java/util/AbstractCollection",
                "toArray",
                "([Ljava/lang/Object;)[Ljava/lang/Object;",
                real_jdk_to_array_typed, cratonvm_native_api::NativeKind::Bridge,
            );
                        // See the KC26 URL-codec registration comment above: only
            // `register_url_codec` is wanted here, not the whole
            // `deprecated_io_util` module (which would re-clobber
            // `deprecated_util.rs`'s correct `java/util/Date` natives).
            cratonvm_native_builtins::deprecated_io_util::register_url_codec(&mut native_methods);
            cratonvm_native_builtins::register_charset_natives_pub(&mut native_methods);
            cratonvm_native_builtins::phases_late::register_p58_charset_coder(&mut native_methods);
            cratonvm_native_builtins::charset::register_real_charset_natives(&mut native_methods);
            cratonvm_native_builtins::deprecated_internal::register_deprecated_internal_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::phases_early::register_arrays_support_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::phases_early::register_string_latin1_natives(
                &mut native_methods,
            );
            // Real close() (see `quarkus_runner_class_loader_close`): closes each
            // distinct ClassLoadingResource once and skips the null map values
            // the real bytecode NPEs on. `SyntheticStub` so `CRATONVM_NO_STUBS`
            cratonvm_native_builtins::classloader_real::register_classloader_real_natives(
                &mut native_methods,
            );
            cratonvm_native_builtins::lang_invoke::register_phase54_method_handle(
                &mut native_methods,
            );
            cratonvm_native_builtins::lang_invoke::register_p63_method_handles_lookup(
                &mut native_methods,
            );
            cratonvm_native_builtins::lang_invoke::register_t4_method_handle_invoke(
                &mut native_methods,
            );
            // C5: See the synthetic-jdk branch above for rationale.
            cratonvm_native_builtins::lang_invoke::register_t28_method_handle_completeness(
                &mut native_methods,
            );
            // Round 85: LambdaMetafactory.metafactory/altMetafactory natives.
            // log4j ServiceLoaderUtil.callServiceLoader (and similar code paths
            // in WildFly's PropertiesUtil bootstrap) invokes
            // LambdaMetafactory.metafactory directly (not via invokedynamic).
            // The real-JDK implementation drives InnerClassLambdaMetafactory →
            // java.lang.classfile API, which trips our incomplete classfile
            // shim with "Bad CP index: 0" inside StackMapGenerator. Intercept
            // with a native stub that returns null so the JDK path is bypassed
            // entirely; the surrounding code tolerates a null call site.
            cratonvm_native_builtins::lang_invoke::register_p68_invoke_extras(&mut native_methods);
            // RA.8 + WP1.8: ServiceLoader bootstrap — see
            // `init_service_loader_bootstrap` doc for the rationale around
            // keeping registration + classpath seeding in a single entry.
            init_service_loader_bootstrap(&mut native_methods);
            // WP2.5: java.lang.reflect.Proxy natives. See companion
            // call in the `feature = "synthetic-jdk"` branch above.
            cratonvm_native_builtins::register_reflect_proxy_natives(&mut native_methods);
            // WP2.4-A: java.lang.instrument runtime natives and the
            // in-process Attach API are VM bridges, not synthetic stubs.
            // The default real-JDK build drops synthetic registrations, so
            // tag this pair explicitly or JDK InstrumentationImpl native
            // methods resolve as missing before any javaagent premain runs.
                                    crate::runtime::instrument::register_instrumentation_natives(&mut native_methods);
            // In-process self-attach (com.sun.tools.attach.VirtualMachine) so
            // runtime-attach agents (Mockito inline mock maker, JaCoCo) can
            // load themselves without `-javaagent:`. See companion call in the
            // `feature = "synthetic-jdk"` branch above.
            crate::runtime::instrument::register_self_attach_natives(&mut native_methods);
                        // RKC16N.10: VMManagementImpl natives. See companion call
            // in the `feature = "synthetic-jdk"` branch above.
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_vm_management_impl(&mut native_methods);
            // JMX-CLUSTER-20260720: do NOT register
            // `register_management_factory_platform_server_stub` here. It was
            // added 2026-07-14 as a Bridge (always-wins) override that
            // permanently shadows real bytecode for
            // `ManagementFactory.getPlatformMBeanServer()` with an empty
            // synthetic `MBeanServer` — directly undoing the KAFKA-MBEAN fix
            // in `register_management_factory` (`native-builtins/src/jmx.rs`),
            // which deliberately leaves this method unregistered so real
            // `MBeanServerFactory.createMBeanServer()` bytecode constructs a
            // genuine `JmxMBeanServer` with a real `registerMBean` Code
            // attribute. The 07-14 override's rationale was a real,
            // then-uninvestigated NPE deep in `ObjectName.
            // getCanonicalKeyPropertyListString()` (`_ca_array` null on the
            // synthetic 1-field ObjectName model) that aborted the platform
            // MXBean registration loop — that NPE (and its sibling in
            // `getSerializedNameString()`/`_kp_array`, hit when an
            // ObjectName is actually serialized over jmxmp) is now fixed by
            // the `RKC-ObjectName-01/02/03` natives in `jmx.rs`, so the
            // workaround is obsolete: it was left in permanently and never
            // reverted, silently zeroing out EVERY platform MXBean
            // (Memory/Threading/ClassLoading/...) registered on
            // `getPlatformMBeanServer()` in real-JDK mode ever since —
            // confirmed via `MBeanServer.queryNames(null, null)` returning
            // 0 entries. Leaving this call out restores the original
            // KAFKA-MBEAN behavior: real bytecode runs end-to-end.
            //
            // Surefire ForkedBooter: ManagementFactory.getRuntimeMXBean() and
            // friends. See companion call in the `feature = "synthetic-jdk"`
            // branch above for the rationale (real-JDK bytecode delegates
            // to `getPlatformMXBean(Class)` which fails with
            // `IllegalArgumentException: ... is not a platform management
            // interface`, killing the forked test JVM constructor).
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_jmx_natives(&mut native_methods);
            // RKC16N.11: rest of the sun.management.* native surface.
            // See companion calls in the `feature = "synthetic-jdk"`
            // branch above for the full rationale.
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_thread_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_class_loading_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_garbage_collector_impl(&mut native_methods);
            // Wave 1 / Task A: per-pool / per-manager MXBean natives.
            // See companion call in the synthetic-jdk branch above.
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_memory_pool_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_memory_manager_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_operating_system_impl(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_hotspot_diagnostic(&mut native_methods);
            #[cfg(feature = "management")]
            cratonvm_native_builtins_jmx::jmx::register_flag_impl(&mut native_methods);
            // SLF4J 1.7 binder stubs — see companion call in the synthetic-jdk
            // branch above for the rationale (Spring Boot 2.x fat-jar
            // <clinit> survival).
            cratonvm_native_builtins::register_slf4j_binder_stubs_pub(&mut native_methods);
            // Spring ApplicationStartup: see `spring_startup_bootstrap` in essentials.
            tracing::info!(
                "Real JDK mode: {} native methods registered",
                native_methods.len()
            );
        }
        // T7: Register AWT/Swing/Java2D native methods for desktop support.
        // Gated behind the (default-on) `awt` feature: `cratonvm-native-awt`
        // sets `publish = false`, so it is an optional dependency. The default
        // build enables `awt` and registers these natives exactly as before.
        #[cfg(feature = "awt")]
        {
            cratonvm_native_awt::register_awt_natives(&mut native_methods);
            tracing::info!(
                "AWT/Swing native methods registered (total: {})",
                native_methods.len()
            );
        }
        // After every registrar: `--compatible`'s loader and thread carriers are
        // reviewed `Intrinsic`s whichever site installed them (a JDK body there
        // reads state that mode never fills). `--real-jdk` returns at once.
        if !config.use_synthetic_jdk {
            cratonvm_native_builtins::compat_carriers::classify_compat_carriers(
                &mut native_methods,
            );
        }
                (
                    native_methods,
                    capabilities,
                    jdk_feature,
                    jdk_continuations,
                    retired_jdk_entry_cap,
                    __boot_t2.elapsed(),
                )
            }
        };
        let __native_reg_handle = if crate::runtime::deferred_starts::DeferredStarts::holding() {
            Err(__native_reg_fn())
        } else {
            Ok(std::thread::spawn(__native_reg_fn))
        };
        let __boot_classpath_elapsed = __boot_t0.elapsed();
        tracing::info!(
            "boot phase 1/3 classpath ingestion: {:?} ({} boot entries, {} ext, {} app)",
            __boot_classpath_elapsed,
            boot_cp.len(),
            ext_cp.len(),
            config.classpath.len(),
        );

        // If module registry is empty after scan but we have a real JDK,
        // manually register java.base as a fallback (Session 14).
        if class_manager.module_registry.is_empty() && !config.use_synthetic_jdk {
            use crate::classloading::module::ModuleDescriptor;
            let java_base = ModuleDescriptor {
                name: "java.base".to_string(),
                version: None,
                requires: vec![],
                exports: vec![],
                opens: vec![],
                uses: vec![],
                provides: vec![],
                is_open: false,
                automatic: false,
                main_class: None,
            };
            let packages = vec![
                "java/lang".to_string(),
                "java/lang/annotation".to_string(),
                "java/lang/invoke".to_string(),
                "java/lang/ref".to_string(),
                "java/lang/reflect".to_string(),
                "java/util".to_string(),
                "java/util/concurrent".to_string(),
                "java/util/concurrent/atomic".to_string(),
                "java/util/concurrent/locks".to_string(),
                "java/util/function".to_string(),
                "java/util/stream".to_string(),
                "java/io".to_string(),
                "java/math".to_string(),
                "java/net".to_string(),
                "java/nio".to_string(),
                "java/security".to_string(),
                "java/time".to_string(),
                "jdk/internal/misc".to_string(),
                "jdk/internal/util".to_string(),
                "sun/misc".to_string(),
            ];
            // JIT round 20 lane superAcc: the fallback exports what the real
            // `java.base` exports of these packages (all but `jdk/internal/*`).
            // With no exports the JPMS export checks (`CONSTANT_Class`, and the
            // define-time supertype check, which judges every application
            // class's `java/lang/Object`) would refuse every class-path class.
            let mut java_base = java_base;
            java_base.exports = packages
                .iter()
                .filter(|p| !p.starts_with("jdk/internal/"))
                .map(|p| crate::classloading::module::ModuleExportsEntry {
                    package_name: p.clone(),
                    ..Default::default()
                })
                .collect();
            class_manager.module_registry.register(java_base, packages);
            class_manager.module_registry.build_readability_graph();
            tracing::info!("Manually registered java.base module (JMOD scan fallback)");
        }

        // Apply CLI module overrides (Phase B).
        for (reader, target) in &config.add_reads {
            class_manager.module_registry.add_reads(reader, target);
        }
        // Recorded as command-line edges: their target is a boot-layer module
        // by identity (`DynamicExport::command_line`, wave 124 lane L5).
        for (module, pkg, target) in &config.add_exports {
            class_manager
                .module_registry
                .add_command_line_exports(module, pkg, target);
        }
        for (module, pkg, target) in &config.add_opens {
            class_manager
                .module_registry
                .add_command_line_opens(module, pkg, target);
        }
        // Rebuild graph if any dynamic edges were added.
        if !config.add_reads.is_empty() {
            class_manager.module_registry.build_readability_graph();
        }
        // The boot configuration's system modules (JEP 261 default roots,
        // `--add-modules`, `requires`, binding): the rest stay out of
        // `ModuleLayer.boot()` (interpreter round i4 wave 86, lane L5).
        if !config.use_synthetic_jdk {
            class_manager
                .module_registry
                .settle_boot_configuration(&config.add_modules, &config.limit_modules);
            // `--patch-module` (wave 87, lane L5): recorded for the JDK's own
            // module references of non-boot modules (`ModulePatcher`), and
            // HotSpot's warning for a module the configuration does not hold.
            class_manager
                .module_registry
                .set_patches(config.patch_modules.clone());
            for (module, _) in &config.patch_modules {
                if class_manager.module_registry.get(module).is_none()
                    || class_manager
                        .module_registry
                        .is_outside_boot_configuration(module)
                {
                    eprintln!("WARNING: Unknown module: {module} specified to --patch-module");
                }
            }
            // `ModuleBootstrap.boot`'s warning for a resolved incubator
            // module, on stderr before the program runs.
            let incubating = class_manager.module_registry.resolved_incubator_modules();
            if !incubating.is_empty() {
                eprintln!("WARNING: Using incubator modules: {}", incubating.join(", "));
            }
        }

        // If a real JDK is on the boot classpath, pre-load core classes so their
        // bytecode methods take priority over native registrations (Phase 33).
        let __boot_t1 = std::time::Instant::now();
        let bootstrapped = class_manager.bootstrap_core_classes();
        let __boot_core_classes_elapsed = __boot_t1.elapsed();
        if bootstrapped > 0 {
            tracing::info!(
                "JDK bootstrap: {} core classes loaded from boot classpath",
                bootstrapped
            );
        }
        // Reports the *total* loaded count, not just the named list: each of
        // the ~323 names in `bootstrap_core_classes` recursively drags in its
        // supertypes and interfaces, so "how many classes are loaded before
        // main" is the second number, not the first.
        tracing::info!(
            "boot phase 2/3 core-class bootstrap: {:?} ({} named classes resolved to real \
             bytecode, {} classes in the ClassStore)",
            __boot_core_classes_elapsed,
            bootstrapped,
            class_manager.loaded_count(),
        );

        // C25: Pre-register synthetic stub classes used by native-builtins so
        // that allocations via `alloc_concurrent_synthetic` carry a valid
        // ClassId.  Without this, `ensure_class_initialized` for these names
        // fails (the classes don't exist on the real classpath), the fallback
        // uses ClassId(0), and downstream `invokevirtual`/`invokeinterface`
        // dispatch can't find the receiver's class — so it falls back to the
        // constant-pool class (e.g. `java/util/Enumeration` or
        // `java/util/Iterator`) where our natives are NOT registered, yielding
        // either NoSuchMethodError or "abstract method has no Code attribute".
        // Registering as synthetic stubs (with empty `methods`) routes
        // dispatch through the native registry on `Enumeration$Impl` instead,
        // where `hasMoreElements`/`nextElement`/`hasNext`/`next` are bound.
        //
        // Fallible since 2026-08-05 (Real-JDK wave 2, lane L7): under
        // `--real-jdk` the class is not created and the wiring below is
        // skipped. Since 2026-08-12 the request is not even made in that mode
        // — `ensure_bootstrap_compat_class` carries the measurement showing
        // the refusal was a constant and the natives on the resulting class
        // are all dropped anyway, so asking bought 13 census rows and no
        // information. `None` here means the same thing it always did: this
        // wiring did not happen.
        let enum_impl_id =
            ensure_bootstrap_compat_class(&mut class_manager, "java/util/Enumeration$Impl", 5);
        // Wire up the synthetic `Enumeration$Impl` so that real-JDK code which
        // does `Enumeration<URL> e = classLoader.getResources(...)` (e.g.
        // `org.apache.commons.logging.LogFactory.getResources`) can perform
        // the implicit checkcast to `java/util/Enumeration` without throwing
        // a `ClassCastException`.  We also set `java/lang/Object` as the
        // superclass so virtual methods like `getClass()` resolve through the
        // standard chain.
        let object_id = class_manager
            .load_class("java/lang/Object")
            .expect("java/lang/Object must be loadable");
        // Preload `java/lang/AssertionError` so the JIT's `new`-site resolver
        // (`resolve_jit_new_site` → `find_class_by_name`) can resolve it. The
        // `assert` statement compiles to `... new AssertionError ...; athrow`,
        // which is DEAD code when assertions are disabled (the default), so the
        // class is otherwise never loaded. With it unloaded, the resolver returns
        // None and the JIT bails compilation of the ENTIRE method — and `assert`
        // is pervasive in libraries. The canonical victim is ANTLR's
        // `ParserATNSimulator.closure` (and the whole ATN-simulation cluster:
        // `getEpsilonTarget`, `ATNConfigSet.add`, `PredictionContext.join`,
        // `SingletonPredictionContext.getReturnState`, …), every one of which has
        // asserts → none JIT-compile → Groovy script parsing runs the interpreter
        // ~2500× slower than HotSpot and times out (Spring Boot buildSrc
        // `SpringRepositoriesExtensionTests` hang). Loading it once here (a tiny
        // standard class) lets all assert-bearing methods compile; the dead
        // assert path still deopts harmlessly if ever reached.
        let _ = class_manager.load_class("java/lang/AssertionError");
        let enumeration_id = class_manager
            .load_class("java/util/Enumeration")
            .expect("java/util/Enumeration must be loadable");
        // Spring Boot 2.3+ `ExecutableArchiveLauncher.getClassPathArchivesIterator`
        // returns `Iterator<Archive>`. Launcher bytecode uses `invokeinterface`
        // `java/util/Iterator.hasNext/next` on our synthetic `Enumeration$Impl`.
        // Without declaring `Iterator`, `invoke_on_class_shared` does not retarget
        // from the interface to the concrete receiver and `next()` never runs our
        // native — the wrong value survives to `checkcast Archive` (CCE).
        let iterator_id = class_manager
            .load_class("java/util/Iterator")
            .expect("java/util/Iterator must be loadable");
        // Through `set_superclass`, not a raw `cls.superclass =` write: the
        // store's direct-subclass index has to see the new edge, or
        // `recompute_subclass_layouts` goes blind to this class. The interface
        // list goes through `set_interfaces` for the mirror-image reason: the
        // store's direct-IMPLEMENTOR index is what class-hierarchy analysis
        // walks (`ClassStore::subtypes_of`), and a raw `cls.interfaces.push`
        // would leave `Enumeration`/`Iterator` looking as though this class did
        // not implement them.
        if let Some(enum_impl_id) = enum_impl_id {
            class_manager.set_superclass(enum_impl_id, Some(object_id));
            let ifaces = class_manager
                .get_class(enum_impl_id)
                .map(|cls| cls.interfaces.clone());
            if let Some(mut ifaces) = ifaces {
                for add in [enumeration_id, iterator_id] {
                    if !ifaces.contains(&add) {
                        ifaces.push(add);
                    }
                }
                class_manager.set_interfaces(enum_impl_id, ifaces);
            }
        }

        // Same treatment for `Comparator$Native`: real-JDK code that does
        // `Stream.sorted(comparator)` (e.g. Spring's `ConfigurationClassParser`)
        // performs an implicit checkcast to `java/util/Comparator`. Our
        // synthetic class must declare `Object` as superclass and
        // `java/util/Comparator` as an implemented interface for the cast to
        // succeed.
        let cmp_native_id =
            ensure_bootstrap_compat_class(&mut class_manager, "java/util/Comparator$Native", 3);
        let comparator_id = class_manager
            .load_class("java/util/Comparator")
            .expect("java/util/Comparator must be loadable");
        if let Some(cmp_native_id) = cmp_native_id {
            class_manager.set_superclass(cmp_native_id, Some(object_id));
            // `set_interfaces` rather than a raw push, as above: the
            // direct-implementor index CHA walks is only maintained there.
            let ifaces = class_manager
                .get_class(cmp_native_id)
                .map(|cls| cls.interfaces.clone());
            if let Some(mut ifaces) = ifaces {
                if !ifaces.contains(&comparator_id) {
                    ifaces.push(comparator_id);
                    class_manager.set_interfaces(cmp_native_id, ifaces);
                }
            }
        }

        // Unmodifiable collection-view wrappers (native-collections):
        // `Collections.unmodifiableList/Set/Map/Collection` and
        // `List.of` / `Set.of` / `Map.of` allocate these synthetic classes,
        // each with one field holding the backing collection. Native read
        // methods delegate to the backing object; native mutators throw
        // `UnsupportedOperationException`. They MUST declare `Object` as
        // superclass and the matching `java/util/*` interface so the
        // implicit checkcast at the API boundary (and `instanceof`) succeed
        // and `invokeinterface` retargets to the concrete receiver where the
        // natives are bound.
        {
            let collection_id = class_manager
                .load_class("java/util/Collection")
                .expect("java/util/Collection must be loadable");
            let list_id = class_manager
                .load_class("java/util/List")
                .expect("java/util/List must be loadable");
            let set_id = class_manager
                .load_class("java/util/Set")
                .expect("java/util/Set must be loadable");
            // Sorted/navigable unmodifiable-set APIs use separate internal
            // stamps from plain `unmodifiableSet`, so a normal set wrapper
            // does not accidentally satisfy `SortedSet` and invite callers to
            // invoke `comparator()` on a LinkedHashSet backing. The sorted and
            // navigable stamps both declare the relevant interfaces so caller
            // checkcasts still match the real JDK wrapper surfaces.
            let sorted_set_id = class_manager
                .load_class("java/util/SortedSet")
                .expect("java/util/SortedSet must be loadable");
            let navigable_set_id = class_manager
                .load_class("java/util/NavigableSet")
                .expect("java/util/NavigableSet must be loadable");
            let map_id = class_manager
                .load_class("java/util/Map")
                .expect("java/util/Map must be loadable");
            let list_iterator_id = class_manager
                .load_class("java/util/ListIterator")
                .expect("java/util/ListIterator must be loadable");
            // The real-JDK `Collections$Unmodifiable*` views (and the
            // `ImmutableCollections$*` family backing `List.of`/`Map.of`) all
            // implement `java.io.Serializable`, so application code that
            // serializes a collection containing one (e.g. Spring's `MimeType`,
            // whose `parameters` field is `Collections.unmodifiableMap(...)`)
            // round-trips. Declare it on the synthetic stamp so `instanceof
            // Serializable` and the object-serialization natives agree; the
            // wrappers carry no real bytecode fields, so the serialization
            // native emits an explicit (backing, marker) record for them.
            let serializable_id = class_manager
                .load_class("java/io/Serializable")
                .expect("java/io/Serializable must be loadable");
            // `entrySet()`'s Set view and its Map.Entry elements — see
            // native-collections' `UNMOD_ENTRY_SET_CLASS`/`UNMOD_ENTRY_ITR_CLASS`/
            // `UNMOD_MAP_ENTRY_CLASS`. Same checkcast/instanceof requirement as
            // every other synthetic wrapper below: `Map.replaceAll`'s default
            // body does `Map.Entry<K,V> entry : entrySet()` (an implicit
            // checkcast to `Map$Entry` on each `Iterator.next()` result), so the
            // wrapped entry must declare that interface or the cast throws
            // ClassCastException.
            let map_entry_id = class_manager
                .load_class("java/util/Map$Entry")
                .expect("java/util/Map$Entry must be loadable");
            // (synthetic class name, list of interface ClassIds it implements)
            let unmod_specs: [(&str, &[ClassId]); 11] = [
                (
                    "cratonvm/internal/UnmodifiableCollection",
                    &[collection_id, serializable_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableList",
                    &[list_id, collection_id, serializable_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableSet",
                    &[set_id, collection_id, serializable_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableSortedSet",
                    &[set_id, sorted_set_id, collection_id, serializable_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableNavigableSet",
                    &[
                        set_id,
                        sorted_set_id,
                        navigable_set_id,
                        collection_id,
                        serializable_id,
                    ],
                ),
                (
                    "cratonvm/internal/UnmodifiableMap",
                    &[map_id, serializable_id],
                ),
                ("cratonvm/internal/UnmodifiableItr", &[iterator_id]),
                (
                    "cratonvm/internal/UnmodifiableListItr",
                    &[list_iterator_id, iterator_id],
                ),
                (
                    "cratonvm/internal/UnmodifiableEntrySet",
                    &[set_id, collection_id, serializable_id],
                ),
                ("cratonvm/internal/UnmodifiableEntryItr", &[iterator_id]),
                ("cratonvm/internal/UnmodifiableMapEntry", &[map_entry_id]),
            ];
            for (name, ifaces) in unmod_specs {
                // These eleven are the largest group of compatibility classes
                // on a strict boot and the most tempting to reclassify as
                // `VmInternal` — no class file exists under
                // `cratonvm/internal/UnmodifiableList`, which is the
                // `VmInternal` shape. They stay `CompatibilityStub` on
                // purpose: they stand in for `java.util.Collections$Unmodifiable*`
                // and friends, whose real bytecode is not running, and that is
                // a compatibility substitution whatever the stand-in is named.
                // Reclassifying them would silence the violation, keep
                // fabricating, and make the zero-stub census read green while
                // the substitution continued.
                //
                // That argument is about `Compatible` mode, which is the only
                // mode that now reaches the fabrication: since 2026-08-12
                // `ensure_bootstrap_compat_class` returns `None` under
                // `--real-jdk` without asking, so strict mode neither
                // fabricates these nor records them. The distinction the
                // paragraph above protects is unchanged — a `VmInternal`
                // reclassification would still be a lie, and it would still be
                // a lie in the mode where the substitution actually happens.
                let Some(cid) = ensure_bootstrap_compat_class(&mut class_manager, name, 1) else {
                    continue;
                };
                class_manager.set_superclass(cid, Some(object_id));
                // `set_interfaces` rather than a raw push: only that call keeps
                // the store's direct-implementor index — the one
                // `ClassStore::subtypes_of` and therefore class-hierarchy
                // analysis walk — in step with the field.
                let existing = class_manager
                    .get_class(cid)
                    .map(|cls| cls.interfaces.clone());
                if let Some(mut existing) = existing {
                    for iface in ifaces {
                        if !existing.contains(iface) {
                            existing.push(*iface);
                        }
                    }
                    class_manager.set_interfaces(cid, existing);
                }
            }
        }

        let gc_backend = match config.gc_algorithm {
            crate::config::GcAlgorithm::Generational => GcBackend::Generational,
            crate::config::GcAlgorithm::G1 => GcBackend::G1,
            #[cfg(feature = "zgc")]
            crate::config::GcAlgorithm::Zgc => GcBackend::Zgc,
        };
        let g1_overrides = G1ConfigOverrides {
            region_size: config.g1_region_size,
            ihop_percent: config.g1_ihop_percent,
            max_gc_pause_ms: config.g1_max_gc_pause_ms,
            string_dedup: config.g1_string_dedup,
            parallel_gc_threads: config.g1_parallel_gc_threads,
            mixed_gc_live_threshold_percent: config.g1_mixed_gc_live_threshold_percent,
            heap_waste_percent: config.g1_heap_waste_percent,
            // zki (page `zgc-zt23hunt4-*` item 4): HotSpot's `Explicit`
            // state backs the ZGC heap by hugetlbfs pages, after the code
            // cache's share of the pool.
            #[cfg(feature = "zgc")]
            zgc_explicit_large_pages: zgc_large_pages_decided
                .filter(|decided| decided.explicit)
                .map(|_| cratonvm_gc::zgc::ZgcExplicitPages {
                    code_heap_bytes: usize::try_from(
                        crate::config::hotspot_flags::code_cache_large_page_bytes(&config),
                    )
                    .unwrap_or(0),
                    // HotSpot probes uncommit only when `ZUncommit` is on and
                    // the minimum heap is below the maximum.
                    uncommit_probe: config.zgc_uncommit
                        && crate::runtime::unified_logging::hotspot_min_heap_size(
                            config.min_heap_size,
                            config.initial_heap_size.min(config.max_heap_size),
                        ) < config.max_heap_size,
                }),
            #[cfg(not(feature = "zgc"))]
            zgc_explicit_large_pages: None,
        };
        // `-Xms` is NOT a G1 override any more (2026-09-20). It used to ride on
        // `G1ConfigOverrides::initial_heap_size`, and the struct's name is why
        // only the G1 arm ever read it: on Generational and on ZGC — the
        // DEFAULT — the flag was accepted and dropped with no diagnostic. It is
        // now a parameter every arm sees, and `XmsDisposition` is each arm's
        // answer. The warning for a dropped `-Xms` lives in the launcher
        // (`vm-cli`), which is the only layer that can tell an explicit flag
        // from `VmConfig::initial_heap_size`'s 16 MiB default; the disposition
        // is bound here so the value is not silently discarded a second time.
        // `-Xmn` / `NewSize` / `MaxNewSize` / `NewRatio` ride as a PARAMETER for
        // the same reason `-Xms` does (gen r4w2/alloc2): a default request is
        // byte-for-byte `new_with_heap_sizing`, and only the Generational arm
        // honours an explicit one.
        // zs s2/ob: HotSpot's `MinHeapSize`, for `ZGC Young Generation`'s JMX
        // `init` (taken here: `config` moves before the realm is built).
        let zgc_jmx_min_capacity = crate::runtime::unified_logging::hotspot_min_heap_size(
            config.min_heap_size,
            config.initial_heap_size.min(config.max_heap_size),
        ) as u64;
        // zt t30/nh: the ZGC beans' code cache layout (`CRATONVM_JMX_CODE_CACHE_HOTSPOT_LAYOUT`).
        let zgc_code_cache_layout =
            crate::config::hotspot_flags::code_cache_hotspot_layout_enabled()
                .then(|| crate::config::hotspot_flags::code_cache_layout(&config));
        // gci i47w/xl47 (`gci-i45rv-waves-i41-i44-review-findings-FIXED-20261016`
        // item 2's residual): the G1YoungGenSizer warnings an `-Xlog` run
        // prints through the logger once it is installed (below), with its
        // stdout output's decorators; empty otherwise.
        let mut g1_sizer_warnings_for_the_logger: Vec<String> = Vec::new();
        let (mut heap, _xms_disposition) = VmHeap::new_with_heap_young_and_survivor_sizing(
            gc_backend,
            config.max_heap_size,
            Some(config.initial_heap_size),
            &config.young_gen_sizing,
            // gcj yg3-residuals R3: the Serial geometry sizes its semis for
            // the ratio `set_survivor_ratio` applies below.
            config.survivor_ratio,
            g1_overrides,
        );
        // gci i43/gy (`gci-i42ta-g1-ignores-xmn-newsize-maxnewsize-newratio-FIXED-20261016`):
        // G1 honours the young sizing as HotSpot 25's `G1YoungGenSizer` does
        // (the min / max young length; a fixed one for `-Xmn` or `NewRatio`),
        // per VM, before the first allocation, and prints HotSpot's
        // `[warning][gc,ergo]` lines for it. A default request is a no-op;
        // `CRATONVM_G1_YOUNG_GEN_SIZER=0` restores "ignored under G1".
        // gc-known-issues retire (`CRATONVM_GEN_HOTSPOT_YOUNG_ERGO`): the
        // texts the launcher resolved for the collector HotSpot would run
        // (Serial's `GenArguments`, or G1's sizer on a non-G1 heap) go out the
        // same way, first.
        let launcher_ergo_warnings = config.hotspot_gc_ergo_warnings.clone();
        let g1_heap = heap.is_g1();
        if g1_heap || !launcher_ergo_warnings.is_empty() {
            let mut warnings = launcher_ergo_warnings;
            if heap.is_g1() {
                warnings.extend(heap.g1_set_young_gen_sizing(&config.young_gen_sizing));
                // Lane lf's hunk H-gy1: `-XX:SurvivorRatio` (validated by the
                // launcher; `None` when not typed) sizes G1's maximum survivor
                // regions, `ceil(young target / SurvivorRatio)`, as on HotSpot.
                heap.g1_set_survivor_ratio(config.survivor_ratio);
            }
            let lines = g1_young_sizer_warning_lines(
                config.xlog_spec.as_deref(),
                __boot_t0.elapsed().as_secs_f64(),
                &warnings,
            );
            // gci i47w/xl47: under `-Xlog`, HotSpot decorates them with the
            // stdout output's decorators (an `-Xlog:...:stdout:<decorators>`
            // option re-decorates the default output) and pads the lines
            // after them to their widths: they wait for the logger, which
            // exists only below. Printed there before any startup line.
            if g1_sizer_warnings_wait_for_the_logger(config.xlog_spec.as_deref())
                && !lines.is_empty()
            {
                g1_sizer_warnings_for_the_logger = warnings;
            } else if !lines.is_empty() {
                use std::io::Write as _;
                let mut out = std::io::stdout().lock();
                for line in &lines {
                    let _ = writeln!(out, "{line}");
                }
                let _ = out.flush();
            }
        }
        // zt t23/hunt4: whether `-XX:+AlwaysPreTouch` is in force on this
        // heap, for the `gc,init` block's `Pre-touch:` line below (ZGC here;
        // G1 and Generational below, zt t29/g1).
        #[allow(unused_mut)]
        let mut pre_touch_in_force = false;
        // zt t24/hunt7: ZGC's `Large Page Support:` value, when this heap
        // decided one (`None` prints `Disabled`). zki (item 5(c)): G1's and
        // Generational's, decided at VM creation.
        #[allow(unused_mut)]
        let mut zgc_large_pages_line: Option<&'static str> =
            gc_init_large_pages_line_at_vm_creation(&config);
        // zki (item 5(c)): HotSpot advises a G1 / Serial heap `MADV_HUGEPAGE`
        // under `-XX:+UseTransparentHugePages` (the state above).
        if zgc_large_pages_line == Some("Enabled (Transparent)") {
            let _ = heap.advise_transparent_huge_pages_non_zgc();
        }
        // zki (page item 4): `[error][gc]` lines the logger prints after the
        // ZGC `gc,init` block, and whether VM creation then fails.
        #[allow(unused_mut)]
        let mut zgc_startup_errors: Vec<String> = Vec::new();
        #[allow(unused_mut)]
        let mut zgc_startup_failed = false;
        if heap.is_zgc() {
            // t1 wave 3 lane Q (`t1-R-zgc-gate-block-out-of-rip-reach`,
            // `CRATONVM_ZGC_JIT_GATE_CELL`, default ON, `=0` kills): this heap's
            // JIT gate block gets a code-adjacent copy, as lane R's SATB byte
            // above, so the fused reference-store gate takes its RIP form.
            // Per heap (one 64-byte cell per ZGC heap, never unmapped), before
            // the first compile and before any thread can write a gate. `None`
            // from the allocator leaves the Box: the long encoding, nothing else.
            {
                if let Some(cell) = cratonvm_jit::platform::alloc_code_adjacent_cell() {
                    // SAFETY: a fresh, zero-filled, 64-byte-aligned cell that
                    // no other reference names and that is never unmapped, at
                    // least `size_of::<JitRefStoreGates>()` (5 bytes of
                    // atomics, align 1) -- a valid all-zero `&'static` block.
                    let cell: &'static cratonvm_gc::JitRefStoreGates =
                        unsafe { &*cell.cast::<cratonvm_gc::JitRefStoreGates>() };
                    heap.zgc_place_jit_ref_store_gates(cell);                }            }
            if None::<std::ffi::OsString>.is_some() {
                eprintln!("[zgc-gate-cell] pre={:#x}", heap.zgc_jit_ref_store_plan().unwrap_or((0, 0, 0, 0)).0);            }
            // zki: HotSpot disables uncommit when its probe on explicit
            // hugetlbfs pages is refused.
            heap.zgc_set_uncommit_options(
                config.zgc_uncommit && !heap.zgc_explicit_uncommit_unsupported(),
                config.zgc_uncommit_delay_secs,
            );
            // zr z3/fp: uncommit down to HotSpot's `MinHeapSize` (the figure
            // the `[gc,init]` block prints as `Min Capacity`), not to the
            // startup commit (`CRATONVM_ZGC_UNCOMMIT_TO_MIN_HEAP=0` keeps it).
            heap.zgc_set_uncommit_floor(crate::runtime::unified_logging::hotspot_min_heap_size(
                config.min_heap_size,
                config.initial_heap_size.min(config.max_heap_size),
            ));
            // zt t24/hunt7 (`zgc-zt23hunt4-transparent-huge-pages-flag-ignored-*`,
            // `CRATONVM_ZGC_TRANSPARENT_HUGE_PAGES`): HotSpot 25 ZGC's
            // large-page decision from `-XX:+UseTransparentHugePages` and the
            // kernel's THP modes, before the startup commit is touched (the
            // pre-touch below): the heap is advised `MADV_HUGEPAGE` in the
            // `Transparent` state, `os::large_page_init`'s warning prints when
            // the kernel has no THP, and the `gc,init` block states the
            // decision. The sysfs files are read only for the flag or an
            // `-Xlog` that may print the block. Linux only.
            // zt t26/x21: decided (and its warning printed or held for the
            // logger) at VM creation, `zgc_large_pages_at_vm_creation`.
            if let Some(decided) = zgc_large_pages_decided {
                if decided.advise {
                    let _ = heap.zgc_advise_transparent_huge_pages();
                }
                zgc_large_pages_line = Some(decided.line);
                // zki (page item 4): the kernel could not back the heap by
                // hugetlbfs pages (no `MADV_POPULATE_WRITE`): small pages, as
                // `CRATONVM_LARGE_PAGES_EXPLICIT=0` says.
                if decided.explicit && !heap.zgc_explicit_huge_pages() {
                    zgc_large_pages_line = Some("Disabled");
                }
            }
            // zki (page item 4): HotSpot's startup commit on explicit
            // hugetlbfs pages -- a `[error][gc] Failed to commit memory (Not
            // enough space)` line per refusal and, when the initial heap
            // could not be committed whole, `Failed to allocate initial Java
            // heap` and a failed VM creation. Without `-Xlog` the lines print
            // now in HotSpot's default shape; under `-Xlog` after the `gc,init`
            // block, through the logger (below).
            if let Some((refusals, short)) = heap.zgc_explicit_initial_commit() {
                let mut texts: Vec<String> = vec![
                    crate::runtime::unified_logging::ZGC_COMMIT_FAILED_ERROR.to_string();
                    refusals
                ];
                if short {
                    texts.push(
                        crate::runtime::unified_logging::zgc_initial_heap_failure_text(
                            u64::try_from(config.initial_heap_size.min(config.max_heap_size))
                                .unwrap_or(u64::MAX),
                        ),
                    );
                }
                if config.xlog_spec.is_some() {
                    zgc_startup_errors = texts;
                    zgc_startup_failed = short;
                } else {
                    use std::io::Write as _;
                    let uptime = vm_creation_t0.elapsed().as_secs_f64();
                    let mut out = std::io::stdout().lock();
                    for text in &texts {
                        let _ = writeln!(
                            out,
                            "{}",
                            crate::runtime::unified_logging::gc_error_default_line(
                                uptime,
                                if stdout_warned_before_heap { 7 } else { 5 },
                                if pagesize_warning.is_some() { 8 } else { 2 },
                                text,
                            )
                        );
                    }
                    let _ = out.flush();
                    drop(out);
                    if short {
                        std::panic::panic_any(crate::error::VmCreationError {
                            detail: texts.last().cloned().unwrap_or_default(),
                        });
                    }
                }
            }
            // zt t23/hunt4 (`zgc-zt22hunt3-always-pre-touch-ignored-*`):
            // `-XX:+AlwaysPreTouch`, per VM, before the first allocation: the
            // startup commit is touched now, later commits as they are made
            // (`CRATONVM_ZGC_ALWAYS_PRE_TOUCH=0`: accepted and ignored).
            pre_touch_in_force = heap.zgc_set_always_pre_touch(config.always_pre_touch);
            // zr z4/un: the uncommitter is started below, once `vm_identity`
            // exists (its `jdk.ZUncommit` events are keyed by it).
            // zr z1/tr: `-XX:SoftMaxHeapSize`, per VM (never above `-Xmx`).
            if let Some(soft) = config.zgc_soft_max_heap_size {
                heap.zgc_set_soft_max_heap_bytes(soft);
            } else if let Some(soft) = config.zgc_ergonomic_soft_max_heap_size {
                // zt t28/sm: HotSpot's ergonomic 90 % (launcher-decided).
                heap.zgc_set_ergonomic_soft_max_heap_bytes(soft);
            }
            // zt t6/tc (zt1tp item 4): `-XX:-ZProactive` and
            // `-XX:ZAllocationSpikeTolerance`, per VM (they were dropped).
            heap.zgc_set_director_proactive(config.zgc_proactive);
            if let Some(tolerance) = config.zgc_allocation_spike_tolerance {
                heap.zgc_set_director_allocation_spike_tolerance(tolerance);
            }
            // zt t7/tg (zt1tp item 3): `-XX:ZCollectionInterval`, per VM. The
            // tick thread that serves it starts once this VM is published
            // (`start_zgc_director_tick`, below `set_process_vm`).
            heap.zgc_set_collection_interval(
                config
                    .zgc_collection_interval_secs
                    .and_then(cratonvm_gc::zgc::timer::interval_from_secs),
            );
            // zt t27/gn5 (gn page item 14): `-XX:ZCollectionIntervalMinor`,
            // per VM; acts only in the generational mode, and not under
            // `CRATONVM_ZGC_GEN_MINOR_TIMER=0`. The same tick serves it.
            heap.zgc_set_collection_interval_minor(
                config
                    .zgc_collection_interval_minor_secs
                    .and_then(cratonvm_gc::zgc::timer::interval_from_secs),
            );
            // zt t10/tg3: `-XX:+ZCollectionIntervalOnly`, per VM (default
            // ON since zt t14/tg6; `CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY=0`
            // parses and ignores it, as before).
            heap.zgc_set_collection_interval_only(config.zgc_collection_interval_only);
            // zr z3/mt: `-XX:ParallelGCThreads` / `-XX:ConcGCThreads` /
            // `-XX:±UseDynamicNumberOfGCThreads`, per VM. Inert on ZGC before
            // (`zgc-zr1mk-gc-thread-flags-ignored-FIXED-20261012.md`).
            heap.zgc_set_gc_worker_threads(
                config.zgc_parallel_gc_threads,
                config.zgc_conc_gc_threads,
                config.zgc_dynamic_gc_threads,
            );
            // zr z2/cp: the infallible allocation arms raise a catchable
            // OutOfMemoryError where a converting handler is up the stack,
            // instead of aborting (`CRATONVM_ZGC_INFALLIBLE_OOME_UNWIND`).
            heap.zgc_set_exhaustion_unwind(crate::runtime::native_oom::infallible_exhaustion_unwind);
        }
        // Bind the heap to THIS VM's compact-layout domain, here rather than
        // later: the comment below is the reason — no object has been allocated
        // and no class layout registered yet, so from this point on every
        // allocation and every registration agree on who owns a `class_id`.
        //
        // `class_id` is a per-`ClassStore` index (`ClassId::new(classes.len())`)
        // and the layout registry is process-global, so without this a second
        // VM allocates its objects against the first VM's layouts and every
        // field access on them is decoded with the wrong storage kinds and
        // offsets.
        heap.set_layout_domain(class_manager.class_store().layout_domain());
        // gen r4w6/young6: the HotSpot tenuring flags, per VM, before the
        // first collection. A default config (nothing typed) is a no-op, and
        // only the Generational backend honours a non-default one.
        heap.set_tenuring_config(&config.tenuring);
        // gci i45/sc (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`):
        // `-XX:SurvivorRatio` (validated by the launcher, gci i43/lf; `None`
        // when not typed) sizes the Generational heap's Serial eden, survivor
        // reserve and tenuring target as on HotSpot Serial, per VM, before the
        // first allocation. `None` / 8 are no-ops; `CRATONVM_GEN_SURVIVOR_RATIO_FLAG=0`
        // restores the constant 8. (G1's survivor regions read it above.)
        heap.gen_set_survivor_ratio(config.survivor_ratio, &config.young_gen_sizing);
        // zt t29/g1 (G1 carry page item 10a, row `h4_P5_g1`): `-XX:+AlwaysPreTouch`
        // on G1 and Generational, per VM, before the first allocation: the
        // startup commit is touched now (G1: later region commits as they are
        // made) and `gc,init` says `Pre-touch: Enabled`, as HotSpot 25 G1 /
        // Serial (`CRATONVM_GC_ALWAYS_PRE_TOUCH_ALL=0`: accepted and ignored).
        // A no-op on ZGC, whose heap applied it above.
        if !pre_touch_in_force {
            pre_touch_in_force = heap.set_always_pre_touch_non_zgc(config.always_pre_touch);
        }
        // gci i45/sc: the TLAB flags hand-off -- `VmConfig::tlab_flags` (gci
        // i43/lf) as the per-VM `TlabSizing` the refill site reads
        // (`HeapRealm::tlab_sizing`). Nothing typed, ZGC, or
        // `CRATONVM_TLAB_HOTSPOT_FLAGS=0`: the old constants.
        let tlab_sizing = pinned_copy_tlab_sizing(&config, gci_i45_sc_tlab_sizing(&config));
        // gcj j3/yg3 (`CRATONVM_GEN_TYPED_MIN_TLAB_FLOOR`, default ON, `=0`
        // kills): the Generational heap's own smallest-useful-TLAB floor (the
        // smallest-whole refill, the tail sink) follows the typed
        // `-XX:MinTLABSize` the refill request already honours. Untyped: 8 KiB,
        // the old constant.
        heap.gen_set_min_tlab_floor(tlab_sizing.min_size());
        // gcg g3/x: `-XX:MinHeapFreeRatio` / `-XX:MaxHeapFreeRatio`.
        heap.set_heap_free_ratios(config.min_heap_free_ratio, config.max_heap_free_ratio);
        let bootstrap_phase = BootstrapPhase::<Allocated>::begin();

        // --- Compressed oops -------------------------------------------------
        //
        // Opt-in (`-XX:+UseCompressedOops`, or `CRATONVM_COMPRESSED_OOPS=1` for
        // A/B runs); OFF by default. This is the ONE point where the narrow-oop
        // base/shift is fixed: the heap's backing stores now exist, so their
        // addresses are known, and no object has been allocated and no class
        // layout registered yet — both of which must observe a stable reference
        // width for the whole process.
        //
        // TWO gates, and they were one by accident.
        //
        // The geometry half is no longer collector-specific: every backend now
        // publishes its reserved spans into `cratonvm_gc::heap_geometry`, and
        // `enable_for_live_heap` derives the narrow-oop window from that rather
        // than from `gen_heap::JIT_REGION_BOUNDS` -- a table whose emptiness
        // under G1 is load-bearing for something else entirely (defect G1-2,
        // inline reference stores), so asking it "where is the heap" answered
        // "nowhere" for two of the three collectors.
        //
        // The AUDIT half is real and stays: only the generational collector's
        // scan and relocation paths have been checked for 4-byte reference
        // slots. That is a statement about work not yet done, and it belongs
        // here, in the open, where an operator can read it and a future change
        // can move it one backend at a time -- not buried in which global table
        // a helper function happened to read.
        let want_compressed_oops = config.use_compressed_oops
            || matches!(
                Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
                Ok("1") | Ok("true")
            );
        // Every VM, not only one that asked (gc-common w8-f): the width is
        // process-wide (`cratonvm_types::narrow_oop`), so a VM that cannot run
        // at the width an earlier VM fixed must not be created, and a VM that
        // asks after a 64-bit VM exists must not flip the process under it.
        // The spans are THIS heap's own (the process-global `heap_geometry`
        // table can hold another VM's), and the audit gate (only the
        // generational collector has been checked for 4-byte slots) is an
        // input, not a pre-filter. Reported on stderr, not just through
        // `tracing`: a silent fallback to 64-bit references would look
        // identical to a successful run apart from the footprint.
        {
            use cratonvm_gc::compressed_oops::{admit_heap, NarrowOopAdmission, NarrowOopRequest};
            match admit_heap(
                heap.reserved_address_envelope(),
                NarrowOopRequest {
                    wants_narrow: want_compressed_oops,
                    backend_audited: gc_backend == GcBackend::Generational,
                },
            ) {
                NarrowOopAdmission::Wide => {}
                NarrowOopAdmission::WideDeclined(why) => {
                    eprintln!(
                        "[cratonvm] compressed oops requested but unusable ({why}) \
                         - running with 64-bit references"
                    );
                }
                NarrowOopAdmission::Narrow {
                    base,
                    shift,
                    fixed_by_this_vm,
                } => {
                    // The bootstrap class set was loaded and laid out BEFORE
                    // this point, with 8-byte reference fields, while the
                    // accessors are now going to read 4-byte narrow slots.
                    // Re-lay every loaded class out at the narrow width. This
                    // is the one moment it is safe: the heap was created a few
                    // lines above, so not a single instance of any of those
                    // classes exists yet. (A VM joining a window an earlier VM
                    // fixed was laid out narrow already; the recompute is then
                    // a no-op in effect.)
                    let relaid = class_manager.recompute_all_compact_layouts();
                    eprintln!(
                        "[cratonvm] compressed oops ON: HeapBased base={base:#x} \
                         shift={shift}{} (reference fields and array elements are 4 \
                         bytes; {relaid} class layouts re-laid out)",
                        if fixed_by_this_vm {
                            ""
                        } else {
                            ", joined an earlier VM's window"
                        }
                    );
                }
                // Same mechanism as `require_jdk_image_for_jdk_only` above:
                // `Vm::new` is infallible by signature, and creating this VM
                // would corrupt its heap.
                NarrowOopAdmission::Refuse(why) => {
                    panic!("[cratonvm] cannot create this VM: {why}")
                }
            }
        }

        // --- ZGC relocation --------------------------------------------------
        //
        // Beside the compressed-oops gate above because it is the same shape: a
        // GC capability the JIT has not been taught about, decided once, here,
        // while the heap is young enough for the answer to be honoured.
        //
        // `RELOCATION_REQUESTED` stands in for the eventual relocation switch.
        // Nothing requests relocation today — see `zgc_relocation_permitted` —
        // so this is inert, which is the point: the gate is placed AHEAD of the
        // capability. Whoever wires the switch replaces this constant and
        // inherits the refusal rather than having to remember it.
        #[cfg(feature = "zgc")]
        if gc_backend == GcBackend::Zgc {
            const RELOCATION_REQUESTED: bool = false;
            let _zgc_relocation = zgc_relocation_permitted(RELOCATION_REQUESTED);
        }

        // bug-h2-largeblob-direct-memory-oom fix — resolve the process-wide
        // direct-buffer accounting cap (java.nio.Bits.reserveMemory's ceiling)
        // the same way real JDK resolves `-XX:MaxDirectMemorySize`: an
        // explicit flag value if the launcher passed one, otherwise `-Xmx`.
        // Previously native_io::direct_buffer hardcoded a 256 MiB cap
        // regardless of `-Xmx`, so a `-Xmx 1g` H2 MVStore workload with a
        // genuine ~250 MiB direct-buffer working set (chunk writer thread)
        // threw OutOfMemoryError at a ceiling HotSpot doesn't impose at the
        // same heap size. See
        // bug-h2-largeblob-direct-memory-oom.md.
        let direct_memory_cap = config
            .max_direct_memory_size
            .unwrap_or(config.max_heap_size);
        cratonvm_native_io::direct_buffer::configure_max_direct_memory(direct_memory_cap as i64);

        // fork6 GC_STRESS fix — wire the SATB write barrier to the concurrent
        // old-gen cycle. `enable_concurrent_gc` previously had NO production
        // caller: the heap's `concurrent_gc_state` stayed `None`, so
        // `satb_barrier` was a hard no-op in every run, and each
        // `maybe_concurrent_gc` cycle's marker drained its own private,
        // never-written queue — the concurrent mark ran against live mutators
        // with no write barrier at all, and the sweep freed old-gen objects
        // whose only reference moved mid-cycle. These handles are shared with
        // every cycle's marker via `ConcurrentMarker::with_shared`.
        let concurrent_satb = std::sync::Arc::new(cratonvm_gc::SatbQueue::new());
        let concurrent_gc_state = std::sync::Arc::new(cratonvm_gc::ConcurrentGcState::new());
        heap.enable_concurrent_gc(concurrent_satb.clone(), concurrent_gc_state.clone());

        // Reset the classloader side-tables that are still process-wide.
        //
        // NOTE what is deliberately NOT here any more: the built-in loader
        // singletons and the `System.getenv()`/`getProperties()` singletons.
        // Clearing those from `Vm::new` assumed VMs are created strictly in
        // sequence; a Rust test binary runs `#[test]`s on several threads, so
        // this call was wiping cells that a *concurrently live* VM was using —
        // after which the two VMs traded heap objects and the reader segfaulted.
        // They are keyed by `vm_identity` now (a fresh VM starts with no row)
        // and dropped in `release_vm_native_state`.
        cratonvm_native_builtins::classloader::reset_loader_singletons();

        let __boot_t2 = std::time::Instant::now();
        let bootstrap_phase = bootstrap_phase
            .classes_ready(
                class_manager.loaded_count(),
                class_manager
                    .get_loaded_class_id("java/lang/Object")
                    .is_some(),
            )
            .expect("bootstrap ClassesReady invariants");

        // This VM's process-unique identity is taken HERE rather than in the
        // `Self { .. }` literal at the end of this function, for one reason: the
        // capability policy is keyed on it (`VmId`), and the policy has to exist
        // before the first `register_*` call. Derived from the class store's
        // layout domain since interpreter round i1 wave 21, so the store, the
        // heap, the realm and the hooks name this VM by one token
        // ([`vm_identity_for_layout_domain`]).

        // Name this VM in its class manager's metadata realm, so the scoped
        // vtable hooks (installed below) route this VM's installs into its own
        // `VtableManager` (interpreter round i1 wave 19). Classes defined
        // before this line mint unbound handles, which the adapter drops; the
        // catch-up replay installs them. A fresh manager cannot already be
        // bound to another VM.
        if let Err(other) =
            class_manager.bind_vm_id(cratonvm_native_api::VmId::from_raw(vm_identity))
        {
            tracing::error!("class manager of VM {vm_identity} was already bound to {other}");
        }
        // zr z4/un: HotSpot's ZUncommitter -- a per-heap thread that gives back
        // memory unused for `ZUncommitDelay` without a collection
        // (`CRATONVM_ZGC_UNCOMMITTER=0` kill). Its `[info][gc,heap]` line goes
        // through `-Xlog`, and each pass that released memory is a
        // `jdk.ZUncommit` on both JFR backends (zr z4/jf's event). Starts
        // nothing under `-XX:-ZUncommit` or `-Xms == -Xmx`.
        //
        // Phase 5 WP3: both threads go through this VM's deferred starts, so a
        // boot image's capture run holds them back until after the restore
        // (a normal run starts them here, on this call, as before).
        let deferred_starts = crate::runtime::deferred_starts::DeferredStarts::new();
        #[cfg(feature = "zgc")]
        if heap.is_zgc() {
            let domain = class_manager.class_store().layout_domain();
            if let Some(starter) = heap.zgc_uncommitter_starter(
                Some(std::sync::Arc::new(|line: &str| {
                    crate::runtime::unified_logging::log_unified(
                        &[crate::runtime::unified_logging::LogTag::GcHeap],
                        crate::runtime::unified_logging::LogLevel::Info,
                        line,
                    );
                })),
                Some(std::sync::Arc::new(
                    move |bytes: usize, start_ns: u64, dur_ns: u64| {
                        zgc_uncommit_jfr(vm_identity, domain, bytes, start_ns, dur_ns);
                    },
                )),
            ) {
                deferred_starts.start_or_defer("ZUncommitter", starter);
            }
            if let Some(starter) = heap.zgc_prefaulter_starter() {
                deferred_starts.start_or_defer("ZPrefaulter", starter);
            }
        }

        // `-Xbootclasspath/a:` (round 13 wave 10, lane misc10;
        // `r13w9-proxy6-xbootclasspath-append-replaces-the-boot-image-FIXED-20260928.md`).
        // HotSpot 9+ keeps the appended entries in the private property
        // `jdk.boot.class.path.append` (`ClassLoaders.<clinit>` reads it for the
        // boot loader's resource search) and searches them AFTER the runtime
        // image; the launcher now maps `-Xbootclasspath/a:` onto that property
        // instead of replacing the boot class path. Append the entries to the
        // bootstrap search the way `Instrumentation.appendToBootstrapClassLoaderSearch`
        // does, which also files the appended-jar census on this VM's class
        // manager. `CRATONVM_XBOOTCLASSPATH_APPEND=0` restores the old launcher
        // mapping (a replace) and skips this.
        {
            if let Some((_, append)) = config
                .system_properties
                .iter()
                .rev()
                .find(|(k, _)| k == "jdk.boot.class.path.append")
            {
                let entries = crate::config::VmConfig::parse_classpath(append);
                if !entries.is_empty() {
                    tracing::info!("-Xbootclasspath/a: {} entries appended", entries.len());
                    class_manager.extend_bootstrap_classpath(&entries);
                }
            }
        }

        // Arm the compiled-tier GPU input-residency barrier, before any
        // class is loaded and so before any method can be compiled. A
        // method compiled ahead of this would carry no barrier and could
        // stale a cache entry created later, which is the whole reason
        // this sits at the top of VM construction rather than beside the
        // offload registry it belongs to (that one is built lazily, on
        // first dispatch, long after the first compile).
        //
        // Only under `--gpu`, and only on x86_64: `gpu_barrier` encodes
        // x64, and an unarmed barrier is what keeps `offload_jit_gate`
        // refusing array writers on every other target, exactly as it
        // did before. `CRATONVM_JIT_GPU_ARRAY_BARRIER=0` is the kill
        // switch and restores that behaviour here too.
        //
        // RT-7 -- the RESULT is kept, on this VM, and it is the whole per-VM
        // half of the barrier: `array_writer_policy` reads this field rather
        // than the process-wide `gpu_barrier::is_armed()`. `arm` no longer
        // refuses a late arming (a VM created after another had compiled
        // code), so here it answers `true`; the proof that this is safe is at
        // `gpu_barrier::arm`, and it depends on THIS function building its own
        // heap -- a constructor that shared a heap between VM identities would
        // be the change that re-opens it.
        #[allow(unused_mut)]
        let mut gpu_array_barrier_armed = false;
        #[cfg(all(feature = "gpu-offload", target_arch = "x86_64"))]
        if config.gpu_offload_enabled
            && Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .ok()
                .as_deref()
                != Some("0")
        {
            gpu_array_barrier_armed = cratonvm_jit::gpu_barrier::arm(
                crate::runtime::offload::input_cache::filter_addr(),
                crate::runtime::offload::input_cache::dirty_addr(),
            );
        }

        let (
            mut native_methods,
            capabilities,
            jdk_feature,
            jdk_continuations,
            retired_jdk_entry_cap,
            __boot_phase3_elapsed,
        ) = match __native_reg_handle {
            Ok(h) => h.join().expect("native registration thread panicked"),
            Err(res) => res,
        };
        class_manager.set_app_workarounds(native_methods.app_workarounds().to_vec());
        class_manager.set_app_stub_layouts(native_methods.app_stub_layouts().clone());
        // Build system properties from platform defaults + user overrides.
        //
        // WP1.11 (2026-04-24) — System property fidelity: populate the full
        // 40-key HotSpot 25 baseline. WildFly's launcher and every
        // standard-library class reads this set; a missing key causes
        // unpredictable fallback paths (often an NPE deep inside
        // Locale/Charset/ClassLoader static init). Keys are split into
        // three tiers, each documented with its JDK origin:
        //
        //  1. Invariant keys — fixed by the VM spec or this build's
        //     identity (java.vm.name, java.vm.vendor, java.specification.*,
        //     java.class.version).
        //  2. Platform-derived keys — read from OS / std::env at startup
        //     (os.name, os.arch, os.version, user.name, user.home, user.dir,
        //     java.io.tmpdir, file.separator, path.separator, line.separator).
        //  3. Env/config-derived keys — read from VmConfig + environment
        //     variables (java.home, java.class.path, java.library.path).
        let mut sys_props = HashMap::new();

        // ---- Tier 1: invariant keys ----
        // java.version family — reported as JDK 25 to match class-file
        // feature support (class-version 69).
        sys_props.insert("java.version".to_string(), "25.0.1".to_string());
        sys_props.insert("java.runtime.version".to_string(), "25.0.1+8".to_string());
        // `java.vm.{name,vendor,version,info}`, `java.vm.specification.*` and
        // `jdk.debug`: one copy, which the C JVMTI table's OnLoad answer
        // reads too (interpreter round i2 wave 61, lane L1). Interpreter
        // round i6 wave 122, lane L1: inserted with the rest of the VM's own
        // list, as one block in tier 3 ([`vm_property_list`]).
        // HotSpot puts its execution-mode summary here ("mixed mode, sharing",
        // "interpreted mode", …) and tools print it verbatim after the VM
        // name. `--real-jdk` appends its own token so a captured run, a
        // support bundle or a `-version` paste says which substitution policy
        // produced it, without anyone having to find the flags line.
        //
        // The list is built by [`vm_info_mode_list`], which the launcher's
        // `-version` banner also calls — the property and the banner used to
        // spell the policy two different ways, and one function is the only
        // way that stays fixed. Under `Compatible` the value stays *exactly*
        // `"mixed mode"` (real-jdk-mode.md §10). (Inserted with the identity
        // keys above, `vm_identity_properties`.)
        sys_props.insert(
            "java.runtime.name".to_string(),
            "Java(TM) SE Runtime Environment".to_string(),
        );
        sys_props.insert("java.vendor".to_string(), "CratonVM".to_string());
        sys_props.insert(
            "java.vendor.url".to_string(),
            "https://cratonvm.invalid/".to_string(),
        );
        sys_props.insert(
            "java.vendor.url.bug".to_string(),
            "https://cratonvm.invalid/bugs".to_string(),
        );
        sys_props.insert("java.vendor.version".to_string(), "25.0.1".to_string());
        // C32: Netty's PlatformDependent0$5 branches on Java version to
        // decide whether DirectByteBuffer(long,long) or (long,int) exists.
        // Must expose a JDK25 spec so the (long,long) path is selected.
        sys_props.insert("java.specification.version".to_string(), "25".to_string());
        sys_props.insert(
            "java.specification.vendor".to_string(),
            "Oracle Corporation".to_string(),
        );
        sys_props.insert(
            "java.specification.name".to_string(),
            "Java Platform API Specification".to_string(),
        );
        // class-file major version for JDK 25 = 69 (45 + feature 24? → JDK 25 = 69).
        sys_props.insert("java.class.version".to_string(), "69.0".to_string());

        // Encodings. **Read the next three paragraphs before adding a key here.**
        //
        // The comment this replaces read "Encodings — JDK 18+ pinned to UTF-8
        // for stdout/stderr/file/native", and it was FALSE for three of the
        // four keys it named. JEP 400 pinned `file.encoding` and nothing else:
        // `jdk/internal/util/SystemProps` (JDK 25 `lib/src.zip`, read on the
        // audit host) ASSIGNS `file.encoding = "UTF-8"`, then DERIVES
        // `native.encoding` from the platform and the three stream keys from
        // `native.encoding`. A written-down premise that is wrong is how this
        // survived unexamined; see
        // docs/feature-designs/real-jdk-mode.md.
        //
        // THE OTHER TABLES — there are three, not two.
        // `native-builtins/src/system_bootstrap.rs` holds a second copy of
        // these six keys in `vmProperties()` and a third, partial copy in
        // `platformProperties()`. Which one wins depends on the mode:
        //   * built-in (no real JDK) — THIS table is the only one; the other
        //     two are never called.
        //   * real-JDK — `SystemProps.initProperties` seeds from
        //     `vmProperties()`, then `put`s `native.encoding` and
        //     `sun.jnu.encoding` unconditionally from `platformProperties()`.
        //     So for those two the PLATFORM table wins outright, and for the
        //     remaining four the `vmProperties()` copy wins by being present
        //     before the `putIfAbsent`s that would otherwise derive them.
        // Both of `system_bootstrap.rs`'s copies read this map back through
        // `NativeContext::get_system_property`, so the derivation below is the
        // single source of truth in every mode and the three tables cannot
        // disagree.
        //
        // THE STREAM KEYS ARE DERIVED HERE TOO, since 2026-09-01. That is the
        // half the page above staged separately and recommended (§9, option A
        // with C as its kill switch): `stdout.encoding` decides the `Charset`
        // `install_charset` stamps on `System.out`, and therefore the BYTES —
        // under `LANG=C` HotSpot writes `?` for a character US-ASCII cannot
        // map and this VM wrote the UTF-8 sequence, ten rows of a 273-assertion
        // differential. `CRATONVM_STDOUT_ENCODING=UTF-8` restores the old
        // behaviour in one binary.
        //
        // `sun.jnu.encoding` is NOT derived, deliberately: it decides how FILE
        // NAMES are encoded, so moving it changes class loading rather than
        // printing. Different blast radius, different change.
        //
        // `file.encoding` is the one key the old comment got right: JEP 400
        // does pin it, unconditionally, and `COMPAT` is the only escape.
        let native_encoding = derive_native_encoding();
        sys_props.insert("file.encoding".to_string(), "UTF-8".to_string());
        sys_props.insert("native.encoding".to_string(), native_encoding.clone());
        sys_props.insert("sun.jnu.encoding".to_string(), native_encoding.clone());
        sys_props.insert(
            "stdout.encoding".to_string(),
            cratonvm_native_api::os_encoding::stream_encoding(
                cratonvm_native_api::os_encoding::StdStream::Out,
            )
            .to_string(),
        );
        sys_props.insert(
            "stderr.encoding".to_string(),
            cratonvm_native_api::os_encoding::stream_encoding(
                cratonvm_native_api::os_encoding::StdStream::Err,
            )
            .to_string(),
        );
        // Session 108 (Cluster D v2 — Console.<clinit> companion fix):
        // `java/io/Console.<clinit>` calls
        // `Charset.forName(System.getProperty("stdin.encoding"), UTF_8)`. The
        // 2-arg `forName` only catches `IllegalCharsetNameException`, NOT the
        // `IllegalArgumentException("Null charset name")` that
        // `Charset.lookup(null)` throws — so a null property here trips the
        // Console.<clinit> swallow we observed in Session 108. The
        // `stdin.encoding` key was missing from the bootstrap seed (only
        // `stdout.encoding` / `stderr.encoding` were pinned). It must stay
        // non-null; it need not be UTF-8, and on HotSpot it is the console's
        // own code page — `cp866` on the 1251 Windows host measured above,
        // where `stdout.encoding` was the redirected `Cp1251`.
        sys_props.insert(
            "stdin.encoding".to_string(),
            cratonvm_native_api::os_encoding::stream_encoding(
                cratonvm_native_api::os_encoding::StdStream::In,
            )
            .to_string(),
        );

        // Force BufferedInputStream/etc. to use synchronized blocks instead
        // of InternalLock/ReentrantLock. This avoids potential issues with
        // our ReentrantLock native when nested inside deep I/O call chains.
        sys_props.insert("jdk.io.useMonitors".to_string(), "true".to_string());

        // NOT seeded here: `sun.misc.unsafe.memory.access`.
        //
        // It used to be pinned to `allow` unconditionally, as a workaround for
        // JEP 498: under the JDK 25 default (`warn`) the first legacy
        // `sun.misc.Unsafe` memory access drops into
        // `Unsafe.beforeMemoryAccessSlow()`, which walks the stack and
        // dereferences `frames.get(1)`; CratonVM's StackWalker was reported to
        // return fewer than two frames for some native/JIT-spliced chains, and
        // that surfaced in jctools' `MpscUnboundedArrayQueue` (netty's
        // per-`NioEventLoop` task queue) as "failed to create a child event
        // loop". Pinning `allow` makes `beforeMemoryAccess()` return at its
        // first check and bypasses the warning machinery.
        //
        // The cost was much larger than the fix. HotSpot sets this property
        // ONLY for `--sun-misc-unsafe-memory-access=<mode>`; a default JDK 25
        // run leaves it unset. netty 4.2 keys its entire Unsafe-vs-FFM
        // decision on exactly that (`PlatformDependent0.explicitNoUnsafeCause0`
        // disables Unsafe on Java 25+ unless the property is set), so pinning
        // it put netty — and every other Unsafe-aware library — on a different
        // code path than a stock JDK 25 run, and made every "CratonVM vs
        // HotSpot" netty comparison a comparison of two different code paths.
        //
        // The property is now the user's to set: `vm-cli` rewrites
        // `--sun-misc-unsafe-memory-access=<mode>` to the `-D` form and nothing
        // else writes it, so a default run answers `null` exactly as HotSpot
        // does. The `sun/misc/Unsafe` post-clinit repair in `vm_util.rs` reads
        // the same property and falls back to `WARN` — the JDK's own default —
        // rather than to a forced `ALLOW`.

        // HotSpot leaves jdk.attach.allowAttachSelf absent. All supported
        // platforms now provide an external attach target, so ByteBuddy can
        // use its child attacher without altering Java-visible defaults.
        // Retain the per-VM rollback switch; explicit -D values are applied below.
        if !config.disable_attach_mechanism && false {
            sys_props.insert("jdk.attach.allowAttachSelf".to_string(), "true".to_string());
        }
        // Weld CDI: default the bootstrap deployer to single-threaded (NONE) so
        // Weld runs its real `SimpleBeanDeployer` bytecode instead of the
        // concurrent `ConcurrentBeanDeployer`, which submits bean-discovery
        // tasks to `ForkJoinPool.commonPool().invokeAll(...)`. CratonVM's
        // synthetic ForkJoinPool can't service that (RejectedExecutionException
        // / hang). Single-threaded deployment is functionally identical (same
        // beans, no parallelism). App `-D` overrides this (config loop applies
        // over these defaults). HIB-CV-20.
        //
        // The real ForkJoinPool path is the default, so Weld's concurrent
        // `COMMON` deployer works (and clears the WELD-001301 the legacy
        // single-threaded path hits). Only seed NONE when the explicit
        // `CRATONVM_SYNTHETIC_FORKJOINPOOL` compatibility opt-out is active;
        // that surface cannot service commonPool().invokeAll safely.
        if !cratonvm_types::flags::flags().natives.real_forkjoinpool {
            sys_props.insert(
                "org.jboss.weld.executor.threadPoolType".to_string(),
                "NONE".to_string(),
            );
        }

        // Keys HotSpot 25 publishes that this table did not, found 2026-08-04
        // by diffing `System.getProperties()` against a HotSpot 25 control (45
        // keys against 48). Each is read by real library code, not only by
        // `-XshowSettings`:
        //
        // * `sun.cpu.endian` — Netty, Chronicle and several serialization
        //   libraries branch on it, and code that finds it absent typically
        //   assumes big-endian, which is wrong on every machine this runs on.
        // * `sun.io.unicode.encoding` — read by `java.io.ObjectStreamClass` and
        //   by the older text codecs.
        // * `java.version.date`, `jdk.debug`, `sun.management.compiler`,
        //   `java.vm.compressedOopsMode` — informational, but they appear in
        //   crash reports, `RuntimeMXBean` dumps and support bundles, and their
        //   absence is what makes a CratonVM dump obviously not a JVM dump.
        //
        // THIS is the table that reaches `System.getProperties()` in real-JDK
        // mode. `native-builtins/src/system_bootstrap.rs::native_vm_properties`
        // has its own, overlapping list which is NOT the source here — the tell
        // is `java.vm.name`, which that one sets to "CratonVM" and this one to
        // "cratonvm", and a real-JDK run reports the lower-case spelling. Add a
        // key to both or you will add it to neither.
        //
        // `sun.java.command` and `sun.java.launcher` are deliberately absent:
        // only the launcher knows them, and `vm-cli` supplies them through
        // `config.system_properties`.
        sys_props.insert(
            "sun.cpu.endian".to_string(),
            if cfg!(target_endian = "big") {
                "big".to_string()
            } else {
                "little".to_string()
            },
        );
        sys_props.insert(
            "sun.io.unicode.encoding".to_string(),
            "UnicodeLittle".to_string(),
        );
        sys_props.insert("java.version.date".to_string(), "2025-10-21".to_string());
        // (`jdk.debug`: with the identity keys, `vm_identity_properties`.)
        // zt t27/lg1 (`CRATONVM_VM_IDENTITY_HOTSPOT`, default ON, `=0` kill):
        // HotSpot 25's value for its tiered compilers (what
        // `CompilationMXBean.getName()` reads), with the VM's other identity
        // keys (`vm_identity_properties`); `=0`: this VM's own name.
        // Interpreter round i7 wave 138, lane L1b: absent under `-Xint` with
        // the JIT off, as HotSpot 25 leaves it unset there (MEASURED: `null`
        // under `-Xint`); the JDK's `VMManagementImpl.getCompilerName` reads
        // it, so `ManagementFactory.getCompilationMXBean()` then answers
        // `null` as on HotSpot. `system_bootstrap::native_vm_properties`
        // reads this map back.
        if !vm_info_interpreted(&config) {
            sys_props.insert(
                "sun.management.compiler".to_string(),
                if cratonvm_types::vm_properties::vm_identity_hotspot_enabled() {
                    "HotSpot 64-Bit Tiered Compilers".to_string()
                } else {
                    "CratonVM JIT".to_string()
                },
            );
        }
        // (`java.vm.compressedOopsMode`: the VM's list, [`vm_property_list`].)

        // ---- Tier 2: platform-derived keys ----
        sys_props.insert("os.name".to_string(), canonical_os_name());
        sys_props.insert("os.arch".to_string(), canonical_os_arch());
        sys_props.insert("os.version".to_string(), canonical_os_version());
        // HotSpot always publishes the data model (pointer width in bits).
        // Some JDK internals branch on it — e.g. `jdk.internal.jimage`'s
        // `BasicImageReader` derives `IS_64_BIT`/`MAP_ALL` from it and, when
        // it defaults to "32", opens a FileChannel over the run-time image
        // instead of using the whole-image map. Without this key the module
        // system takes a degraded path. Derive it from the target pointer
        // width so it is correct on every arch.
        sys_props.insert(
            "sun.arch.data.model".to_string(),
            (std::mem::size_of::<usize>() * 8).to_string(),
        );
        sys_props.insert(
            "file.separator".to_string(),
            std::path::MAIN_SEPARATOR.to_string(),
        );
        sys_props.insert(
            "path.separator".to_string(),
            if cfg!(windows) { ";" } else { ":" }.to_string(),
        );
        sys_props.insert(
            "line.separator".to_string(),
            if cfg!(windows) { "\r\n" } else { "\n" }.to_string(),
        );
        if let Ok(dir) = std::env::current_dir() {
            sys_props.insert("user.dir".to_string(), dir.to_string_lossy().into_owned());
        } else {
            sys_props.insert("user.dir".to_string(), ".".to_string());
        }
        let user_home = cratonvm_types::flags::runtime_var("HOME")
            .or_else(|_| cratonvm_types::flags::runtime_var("USERPROFILE"))
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "C:\\".to_string()
                } else {
                    "/".to_string()
                }
            });
        sys_props.insert("user.home".to_string(), user_home);
        sys_props.insert(
            "user.name".to_string(),
            cratonvm_types::flags::runtime_var("USER")
                .or_else(|_| cratonvm_types::flags::runtime_var("USERNAME"))
                .unwrap_or_else(|_| "unknown".to_string()),
        );
        // HotSpot's derivation, not `std::env::temp_dir()`: HotSpot on Linux
        // ignores `$TMPDIR`. See `cratonvm_native_api::os_tmpdir`.
        sys_props.insert(
            "java.io.tmpdir".to_string(),
            cratonvm_native_api::os_tmpdir::java_io_tmpdir(),
        );

        // Locale — W7-67. Read the HOST's two locales (Windows: UI language +
        // regional format; Unix: LC_MESSAGES + LC_CTYPE, each resolved through
        // LC_ALL ▸ category ▸ LANG) and publish the `user.*` family the way
        // `jdk.internal.util.SystemProps.fillI18nProps` does. This used to
        // parse `$LANG` only, so every Windows host — where `$LANG` is unset —
        // reported `en_US` regardless of the machine's actual settings.
        //
        // `config.system_properties` (the `-D` flags) is passed in so a
        // command-line `-Duser.language=…` suppresses the derived `.format`
        // overlay, matching the JDK; it is re-applied over `sys_props` below,
        // which is what makes the `-D` value itself win.
        //
        // r16 locale lane (b): on Linux the four keys now come from
        // `java_props_md.c`'s `ParseLocale` reproduced exactly
        // (`parse_locale_java_props_md`), including its NULLs: a subtag the
        // locale name does not carry is ABSENT, as `fillI18nProps` leaves it,
        // not `""`. `LANG=C.UTF-8` is `en` with no country there -- only the
        // exact names `C` / `POSIX` become `en_US` -- and this used to answer
        // `en_US`. `CRATONVM_HOST_LOCALE_PARSELOCALE=0` restores the
        // `derive_host_locale` arm on Linux too.
        //
        // Merged with SBR wave 4 LOCALE2 (`derive_host_locale`): a name glibc
        // cannot load fails HotSpot's `setlocale(LC_ALL, "")` and leaves every
        // category in `C`, which `ParseLocale` reports as `en_US`
        // (`posix_env_locales_loadable`).
        if linux_parselocale_enabled() {
            let (display, format) = if posix_env_locales_loadable() {
                (
                    posix_locale_category("LC_MESSAGES"),
                    posix_locale_category("LC_CTYPE"),
                )
            } else {
                ("C".to_string(), "C".to_string())
            };
            let display = parse_locale_java_props_md(&display);
            let format = parse_locale_java_props_md(&format);
            for (base, display, format) in [
                (
                    "user.language",
                    Some(display.language.as_str()),
                    Some(format.language.as_str()),
                ),
                (
                    "user.script",
                    display.script.as_deref(),
                    format.script.as_deref(),
                ),
                (
                    "user.country",
                    display.country.as_deref(),
                    format.country.as_deref(),
                ),
                (
                    "user.variant",
                    display.variant.as_deref(),
                    format.variant.as_deref(),
                ),
            ] {
                fill_i18n_props_platform(
                    &mut sys_props,
                    &config.system_properties,
                    base,
                    display,
                    format,
                );
            }
        } else {
            let host_locale = derive_host_locale();
            for (base, display, format) in [
                (
                    "user.language",
                    &host_locale.display.language,
                    &host_locale.format.language,
                ),
                (
                    "user.script",
                    &host_locale.display.script,
                    &host_locale.format.script,
                ),
                (
                    "user.country",
                    &host_locale.display.country,
                    &host_locale.format.country,
                ),
                (
                    "user.variant",
                    &host_locale.display.variant,
                    &host_locale.format.variant,
                ),
            ] {
                fill_i18n_props(
                    &mut sys_props,
                    &config.system_properties,
                    base,
                    display,
                    format,
                );
            }
        }
        // user.timezone is normally set by the JDK's TimeZone.getDefault()
        // during initPhase1 — pre-populate with TZ env or empty string so
        // the key is at least present.
        sys_props.insert(
            "user.timezone".to_string(),
            cratonvm_types::flags::runtime_var("TZ").unwrap_or_default(),
        );

        // ---- Tier 3: env/config-derived keys ----
        // java.home — same resolution order as boot classpath discovery
        // (`resolve_java_home_public`): explicit config, CRATONVM_JAVA_HOME,
        // JAVA_HOME, then PATH. Avoid reading JAVA_HOME alone here — that
        // skipped CRATONVM_JAVA_HOME and could disagree with boot discovery.
        let java_home_val = initial_java_home(&config);
        // Interpreter round i6 wave 122, lane L1 (i61-L1 stage 3): the VM's
        // own property list, its thirteen keys as one block -- the identity
        // keys (`VM_IDENTITY`), `java.vm.info`, the four paths and
        // `java.vm.compressedOopsMode` -- apart from the platform's keys
        // around it. No other line of this table writes any of them; the
        // `-D` loop below still overrides, as before.
        for (key, value) in vm_property_list(&config, &java_home_val) {
            sys_props.insert(key.to_string(), value);
        }
        // `java.version` and `java.runtime.version` describe the CLASS LIBRARY,
        // not this VM -- `java.vm.name` is where CratonVM names itself
        // (`java.vm.version` follows the image too since wave 138, L1b:
        // `vm_identity_properties`). Seeding them as constants
        // meant a run against a 25.0.4+7 image reported 25.0.1, which is what
        // every version-sniffing library reads:
        //
        //   java.version                   HotSpot 25.0.4      was 25.0.1
        //   Runtime.version().update()             4           was 1
        //   Runtime.version().toString()   25.0.4+7-LTS        was 25.0.1+8
        //
        // `Runtime.version()` parses `java.runtime.version` (see
        // `lang_system::runtime_version_parts`), so one wrong constant was five
        // wrong rows. The image states both in its own `release` file; the
        // hardcoded values stay as the fallback for a synthetic-JDK run, which
        // has no image to ask. MEASURED by
        // `SystemRuntimeObjectSweep.java`.
        if let Some((version, runtime_version)) = jdk_image_release_versions(&java_home_val) {
            sys_props.insert("java.version".to_string(), version);
            sys_props.insert("java.runtime.version".to_string(), runtime_version);
        }
        // Interpreter round i7 wave 136, lane L1b: `java.version.date` is the
        // class library's too (`VersionProps.VERSION_DATE`), and was the 25.0.1
        // constant above for every image. MEASURED: Temurin 25.0.3+9 answers
        // `2026-04-21`, its `release` file's `JAVA_VERSION_DATE`.
        if let Some(date) = jdk_image_release_version_date(&java_home_val) {
            sys_props.insert("java.version.date".to_string(), date);
        }

        // java.class.path — match the HotSpot contract
        // ([`initial_java_class_path`]; in the VM's list above).
        // HotSpot publishes `--module-path` as `jdk.module.path` (the JDK's own
        // `ModuleBootstrap` and `ModuleFinder` users read it back).
        if !config.module_path.is_empty() {
            let sep = if cfg!(windows) { ";" } else { ":" };
            sys_props.insert("jdk.module.path".to_string(), config.module_path.join(sep));
        }
        // zt t12/df4: the `java` launcher turns `--illegal-native-access=<mode>`
        // into `-Djdk.module.illegal.native.access=<mode>`, which the JDK's
        // `ModuleBootstrap` reads (and removes) for `Module.ensureNativeAccess`.
        // CratonVM's launcher set only its own gate
        // (`panama::NativeAccessState`, per VM since zt t18/df7), which serves the VM's `Linker`
        // bridges; a restricted method whose JDK body runs
        // (`MemorySegment.reinterpret`, `System.loadLibrary`'s JNI binding)
        // asked the JDK, which saw no property and warned under `deny` (HotSpot
        // throws `IllegalCallerException`) and under `allow` (HotSpot is
        // silent). `warn` is the JDK's default and needs no property.
        // `CRATONVM_FFM_ILLEGAL_NATIVE_ACCESS_PROPERTY=0` restores the old
        // boot. A user `-Djdk.module.illegal.native.access` still wins (applied
        // below). Probe `Zt12Df4UpcallWarning deny-reinterpret`.
        if let Some(mode) = illegal_native_access_property_value(config.native_access.illegal) {
            sys_props.insert(
                "jdk.module.illegal.native.access".to_string(),
                mode.to_string(),
            );
        }

        // java.library.path and sun.boot.library.path
        // ([`initial_java_library_path`], [`initial_sun_boot_library_path`];
        // in the VM's list above).

        // Apply user overrides from config — always wins over platform defaults.
        //
        // Except `jdk.boot.class.path.append`, the launcher's carrier for
        // `-Xbootclasspath/a:` (round 13 wave 10, lane misc10): the bootstrap
        // search was already extended from it above, and HotSpot keeps it out
        // of `System.getProperties()` (`System.createProperties` drops it; only
        // `VM.getSavedProperty` sees it, for a boot loader resource search this
        // VM serves natively). `CRATONVM_XBOOTCLASSPATH_APPEND=0` publishes it.
        let hide_boot_append =
            true;
        for (k, v) in &config.system_properties {
            if hide_boot_append && k == "jdk.boot.class.path.append" {
                continue;
            }
            sys_props.insert(k.clone(), command_line_property_value(&java_home_val, k, v));
        }

        // … with exactly ONE exception, and it is specified rather than
        // chosen. `java.lang.System`'s property table says of
        // `native.encoding`: "setting this system property on the command line
        // has no effect". The JDK enforces that by `put`ting the platform value
        // AFTER the command-line map has been built
        // (`SystemProps.initProperties`), which is what this line mirrors. It
        // is also what real-JDK mode already did here, because
        // `platformProperties()` overrides the `-D` map there — so re-asserting
        // it is what makes the two modes agree instead of disagreeing only
        // when someone passes a flag the spec says is inert.
        //
        // Note this re-asserts the DERIVED value, not a constant: a
        // `CRATONVM_NATIVE_ENCODING=…` pin is applied inside
        // `derive_native_encoding` and therefore survives this line, which is
        // what keeps the kill switch usable.
        sys_props.insert("native.encoding".to_string(), native_encoding);

        // `-XX:SoftRefLRUPolicyMSPerMB` (gen r5w2/conc6): read here, before
        // `config` moves into the `SharedVm`, like `verbose_gc` below.
        let soft_ref_lru_ms_per_mb = config.soft_ref_lru_policy_ms_per_mb;
        // Wire GC logging from config
        let verbose_gc_at_launch = config.verbose_gc;
        if verbose_gc_at_launch {
            heap.enable_gc_logging();
        }

        // WP8.5 — wire HotSpot-style unified logging (`-Xlog:...`).
        // The full JEP 158/271 parser lives in
        // `vm/src/runtime/unified_logging.rs`; the wiring step is to
        // initialize the global logger at VM startup so subsequent
        // `log_unified()` / `gc_info()` calls in GC and class-load
        // hot paths actually emit. A failed parse is logged through
        // tracing but does not abort startup — HotSpot's behaviour for
        // a malformed `-Xlog` spec is to print a warning and continue.
        if let Some(spec) = config.xlog_spec.as_deref() {
            if let Err(e) = crate::runtime::unified_logging::init_unified_logging(spec) {
                tracing::warn!("invalid -Xlog spec {:?}: {}", spec, e);
            }
            // zt t26/x21: the start-up `[warning][pagesize]` line held at VM
            // creation, first (HotSpot prints it from `os::init_2`, before the
            // heap exists), routed and decorated by the logger; with no
            // logger (a spec that did not parse), HotSpot's default shape.
            if let Some(text) = pagesize_warning_for_the_logger.take() {
                if !crate::runtime::unified_logging::log_pagesize_warning(text) {
                    use std::io::Write as _;
                    let mut out = std::io::stdout().lock();
                    let _ = writeln!(
                        out,
                        "{}",
                        crate::runtime::unified_logging::pagesize_warning_default_line(
                            vm_creation_t0.elapsed().as_secs_f64(),
                            text,
                        )
                    );
                    let _ = out.flush();
                }
            }
            // gci i47w/xl47: the G1YoungGenSizer warnings held above, first on
            // stdout (HotSpot logs them while the heap's policy is built,
            // before `Using G1`), decorated as the logger's stdout output; with
            // no logger (a spec that did not parse), HotSpot's default shape.
            for text in std::mem::take(&mut g1_sizer_warnings_for_the_logger) {
                if !crate::runtime::unified_logging::log_on_default_stdout(
                    &[crate::runtime::unified_logging::LogTag::GcErgo],
                    crate::runtime::unified_logging::LogLevel::Warning,
                    &text,
                ) {
                    use std::io::Write as _;
                    let mut out = std::io::stdout().lock();
                    for line in g1_young_sizer_warning_lines(
                        None,
                        __boot_t0.elapsed().as_secs_f64(),
                        std::slice::from_ref(&text),
                    ) {
                        let _ = writeln!(out, "{line}");
                    }
                    let _ = out.flush();
                }
            }
            // gen r5w3/obs7: HotSpot's startup lines — `[gc] Using Serial`
            // and the `gc,init` block — on the backend whose beans and
            // `-Xlog:gc` lines are Serial's. gcd d10/o retirement
            // (2026-10-04): and `Using G1` / `Using The Z Garbage Collector`
            // with their `gc,init` blocks on G1 and ZGC, which printed none
            // (a GC-log parser such as GCeasy picks its grammar from the
            // `Using` line). Once per VM, after the logger exists.
            {
                use crate::runtime::unified_logging::{GC_NAME_G1, GC_NAME_SERIAL, GC_NAME_ZGC};
                let (collector, region_size) = (
                    heap.collector_log_name(),
                    heap.region_census().map(|c| c.region_size),
                );
                let online = std::thread::available_parallelism().map_or(1, |n| n.get());
                // gch h1/ob (g4yg item 8): HotSpot's `total` is `os::processor_count()`
                // (configured CPUs), its `available` `os::active_processor_count()`
                // (affinity / quota, which `available_parallelism` already is).
                let configured = configured_cpu_count().unwrap_or(online).max(online);
                crate::runtime::unified_logging::log_gc_startup_with_errors(
                    &crate::runtime::unified_logging::GcInitFacts {
                        collector,
                        // gcg g1/c: HotSpot prints `VM_Version::vm_release()`, the image's
                        // build string, which is `java.runtime.version` here (read from
                        // `<java.home>/release`; the constant only on a synthetic JDK).
                        // (`java.vm.version` is the same string since wave 138.)
                        version: sys_props
                            .get("java.runtime.version")
                            .map_or("", String::as_str),
                        cpus_total: configured,
                        cpus_available: online,
                        memory_bytes: crate::runtime::container::physical_ram_bytes(),
                        // gcg g3/x: HotSpot's `MinHeapSize` (`-Xms` /
                        // `-XX:MinHeapSize`, else `min(8M, initial)`); it
                        // printed the initial size.
                        // The initial size never above the maximum (the 16 MiB
                        // library default is not clamped under `-Xmx8m`).
                        heap_min: crate::runtime::unified_logging::hotspot_min_heap_size(
                            config.min_heap_size,
                            config.initial_heap_size.min(config.max_heap_size),
                        ),
                        heap_initial: config.initial_heap_size.min(config.max_heap_size),
                        heap_max: config.max_heap_size,
                        // gcg g3/x: this process's narrow-oop mode, fixed by
                        // the admission above (`Disabled` by default).
                        compressed_oops: crate::runtime::unified_logging::compressed_oops_mode(
                            cratonvm_types::narrow_oop::narrow_oops_enabled(),
                            cratonvm_types::narrow_oop::narrow_base(),
                            cratonvm_types::narrow_oop::narrow_shift(),
                        ),
                        region_size,
                        // zr z1/tr: ZGC's soft maximum and uncommit policy,
                        // as the heap applied them (`None` off ZGC).
                        soft_max: heap.soft_max_capacity(),
                        zgc_uncommit: heap
                            .soft_max_capacity()
                            .map(|_| (config.zgc_uncommit, config.zgc_uncommit_delay_secs)),
                        // zr z3/mt: `Runtime Workers:` -- an explicit
                        // `-XX:ParallelGCThreads` as the ZGC heap applied it.
                        zgc_runtime_workers: zgc_runtime_workers_fact(&heap),
                        // zki: HotSpot's refused uncommit probe.
                        zgc_uncommit_unsupported: zgc_uncommit_unsupported_fact(&heap),
                        // zt t23/hunt4: `Pre-touch: Enabled` under an
                        // `-XX:+AlwaysPreTouch` the heap applied (ZGC; G1 and
                        // Generational since zt t29/g1).
                        pre_touch: pre_touch_in_force,
                        // zt t24/hunt7: ZGC's large-page decision above.
                        large_pages: zgc_large_pages_line,
                        g1_parallel_workers: if heap.is_g1() { heap.parallel_worker_count() } else { None },
                        // zt t31/x10: `Periodic GC: Enabled` and its interval.
                        g1_periodic_interval_ms: crate::config::hotspot_flags::g1_periodic_gc_interval_arg(&config),
                        // zt t27/lg1: ZGC's Linux facts read at VM creation
                        // (`None` off ZGC / off Linux).
                        zgc_linux_backing: if collector == GC_NAME_ZGC {
                            zgc_linux_backing
                        } else {
                            None
                        },
                    },
                    &zgc_startup_errors,
                );
                // zki (page item 4): HotSpot's failed VM creation after its
                // `Failed to allocate initial Java heap` error.
                if zgc_startup_failed {
                    std::panic::panic_any(crate::error::VmCreationError {
                        detail: zgc_startup_errors.last().cloned().unwrap_or_default(),
                    });
                }
            }
            // gcj j1/st item 3: HotSpot creates its Symbol and String tables
            // after the heap (`universe_init`), each logging its start size.
            {
                use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
                use crate::memory::string_table::{hotspot_start_size_text, hotspot_stringtable_start_size};
                log_unified(
                    &[LogTag::SymbolTable],
                    LogLevel::Trace,
                    &hotspot_start_size_text(crate::runtime::serviceability::SYMBOLTABLE_BUCKETS),
                );
                log_unified(
                    &[LogTag::StringTable],
                    LogLevel::Trace,
                    &hotspot_start_size_text(hotspot_stringtable_start_size(config.string_table_size)),
                );
            }
        } else {
            // gci i30/ob (i28-dg review item 4): no `-Xlog`, no logger -- not
            // an earlier VM's in this process.
            crate::runtime::unified_logging::reset_unified_logging();
        }

        // Initialize AOT runtime if enabled
        #[cfg(feature = "experimental-aot")]
        {
            use crate::config::AotMode;
            let (training, production) = match config.aot_mode {
                AotMode::Training => (true, false),
                AotMode::Production => (false, true),
                AotMode::Off => (false, false),
            };
            crate::native::builtins::aot::init_aot_runtime(
                training,
                production,
                config.aot_cache_input.as_deref(),
                config.aot_cache_output.as_deref(),
            );
        }

        // AOT and CDS are explicitly compiled experiments and are no longer in
        // the default feature set. A build that cannot honour `-XX:AOTMode` /
        // `-XX:SharedArchiveFile` must say so: silently ignoring the request is
        // exactly the "capability reads as landed but never runs" failure mode
        // ARCHITECTURE.md's flag-default checklist exists to prevent.
        #[cfg(not(feature = "experimental-aot"))]
        {
            if !matches!(config.aot_mode, crate::config::AotMode::Off) {
                tracing::warn!(
                    "AOT cache requested (-XX:AOTMode) but this build was compiled \
                     without --features experimental-aot; the request is ignored"
                );
            }
            if !matches!(config.cds_mode, crate::config::CdsMode::Off) {
                tracing::warn!(
                    "CDS requested (-XX:SharedArchiveFile / -Xshare) but this build \
                     was compiled without --features experimental-aot; the request \
                     is ignored"
                );
            }
        }

        // Load CDS archive if configured
        #[cfg(feature = "experimental-aot")]
        if matches!(
            config.cds_mode,
            crate::config::CdsMode::On | crate::config::CdsMode::Auto
        ) {
            if let Some(ref archive_path) = config.shared_archive_file {
                let mut loader =
                    cratonvm_native_builtins::cds::CdsArchiveLoader::new(archive_path.clone());
                if loader.try_load() {
                    let n = loader.classes_loaded();
                    tracing::info!("CDS: loaded {} classes from {}", n, archive_path);
                    // Transfer cached class bytes into ClassManager for fast lookup.
                    // Convert the loader's std HashMap into the ClassManager's FxHashMap
                    // (T10.9.B — CDS cache is internal-keyed so Fx is safe).
                    class_manager.cds_class_cache =
                        loader.drain_class_cache().into_iter().collect();
                    // Set system property so native handlers know CDS is active
                    sys_props.insert(
                        "jdk.internal.vm.cds.enabled".to_string(),
                        "true".to_string(),
                    );
                } else if matches!(config.cds_mode, crate::config::CdsMode::On) {
                    tracing::error!(
                        "CDS: archive not found or invalid at {} (mode=on, failing)",
                        archive_path
                    );
                }
            }
        }

        // `-Xshare:dump`: this run WILL write an archive at shutdown (see
        // `SharedVm::dump_cds_archive`). Publish that as a system property so
        // `jdk/internal/misc/CDS.isDumpingArchive0()` can report the real mode
        // instead of a hard-coded `false`; mirrors the
        // `jdk.internal.vm.cds.enabled` handshake above.
        if matches!(config.cds_mode, crate::config::CdsMode::Dump) {
            sys_props.insert(
                "jdk.internal.vm.cds.dumping".to_string(),
                "true".to_string(),
            );
        }

        // Build the GPU offload cache registry. With the feature off,
        // this block does not exist. The registry itself is empty until
        // the first `get_or_create` call constructs a per-device
        // `OffloadCache`, so this is effectively free at startup.
        #[cfg(feature = "gpu-offload")]
        let offload_registry =
            std::sync::Arc::new(crate::runtime::offload::OffloadCacheRegistry::new());

        // The real-JDK platform-server bridge needs its interface methods.
        #[cfg(feature = "management")]
        cratonvm_native_builtins_jmx::jmx::register_mbean_server(&mut native_methods);

        // Phase 3 closes here — `register_mbean_server` above is the LAST
        // `register_*` call before the registry is moved into `Self`, so
        // `native_methods.len()` is the true boot registration count for
        // whichever arm of the two mode `cfg` blocks was compiled in. That
        // count is the honest answer to "is real-JDK mode really ~300
        // natives?" (it is not — see
        // `startup-and-diagnostics.md` §2.5).
        // Note the registry pre-sizes four maps to 4,096 entries
        // unconditionally (`NativeMethodRegistry::new`), i.e. independently of
        // mode; that is one bounded allocation, not per-native work.
        tracing::info!(
            "boot phase 3/3 native registration: {:?} ({} natives registered)",
            __boot_phase3_elapsed,
            native_methods.len(),
        );
        let bootstrap_phase = bootstrap_phase
            .natives_ready(native_methods.len())
            .expect("bootstrap NativesReady invariants");

        // The registration pass just pushed one `native-register:<class>.<method>`
        // row into the audit log per registered native — ~3,100 in real-JDK
        // mode, ~5,200 with `synthetic-jdk`. Under the default `Permissive` mode
        // that is pure noise with a sharp edge: `MAX_AUDIT_ENTRIES` is 4,096, so
        // in synthetic mode the map is FULL before `main()` runs and every
        // subsequent file/socket/spawn use — the only thing the report exists to
        // collect — is silently dropped, with `truncated` set. The same rows are
        // already recorded, with provenance, by the registry's own census
        // (`registrations` / `provenance` / `census()`), so nothing is lost.
        //
        // Cleared ONLY under `Permissive`. `Audit` and `Enforce` keep every row:
        // in those modes an operator deliberately asked for the detail, and an
        // `Enforce` run's *denied* registrations are exactly what they need to
        // see. (Those modes still hit the 4,096 cap in synthetic mode — that is
        // a `native-api` sizing question, reported separately, not something to
        // paper over here.)
        if capabilities.mode() == cratonvm_native_api::CapabilityMode::Permissive {
            native_methods.set_capabilities(Arc::clone(&capabilities));
        }

        // One per-VM code-state epoch, shared by the realm that advances it
        // and the tiered manager that stamps with it (`JitRealm::code_state_epoch`).
        let code_state_epoch = crate::vm::realms::JitRealm::new_code_state_epoch();
        // And one redefine epoch, which both verdict registries share
        // (`JitRealm::redefine_epoch`, interpreter round i1 wave 17).
        let redefine_epoch = crate::vm::realms::JitRealm::new_redefine_epoch();
        // The owner token the JIT invalidate hook names this VM by
        // (`JitRealm::class_layout_domain`); read before `class_manager`
        // moves into its lock.
        let class_layout_domain = class_manager.class_store().layout_domain();
        // nio21 wave 5 (L1): the resolution maps judge redefinitions by this VM's log.
        let vm_redefinition_log = class_manager.redefinition_log();
        // The same epochs are the runtime de-speculation verdicts'.
        let runtime_despec =
            cratonvm_jit::compile_gate::RuntimeDespecRegistry::new_registered_with_vm_epochs(
                Arc::clone(&code_state_epoch),
                Arc::clone(&redefine_epoch),
            );
        // The tiered manager forgets an unloaded or redefined class's runtime
        // verdicts in this VM's registry only (interpreter round i1 wave 18).
        let tiered_manager = {
            use crate::jit::tiered::TieredCompilationManager as Tiered;
            let mgr = Tiered::with_env_policy_and_vm_epochs(
                Arc::clone(&code_state_epoch),
                Arc::clone(&redefine_epoch),
                Arc::clone(&runtime_despec),
            );
            if config.compatibility_mode == CompatibilityMode::Compatible && compat_fast_boot_jit {
                let mut policy = mgr.policy();
                let explicit_c1 = false;
                if !explicit_c1 {
                    policy.c1_threshold = 2000;
                }
                let explicit_c2 = false;
                if !explicit_c2 {
                    policy.c2_threshold = 50_000;
                }
                let explicit_c2_min = false;
                if !explicit_c2_min {
                    policy.c2_min_invocations = 5_000;
                }
                mgr.set_policy(policy);
            }
            mgr
        };
        // The resolved class-entry table's switch (`ClassRealm::
        // resolved_entry_table`; interpreter round i1 wave 47, lane L5), read
        // once per VM. On by default; `CRATONVM_RESOLVED_ENTRY_TABLE=0` turns
        // it off (the wave-46 armed record and constant-pool scan).
        let resolved_entry_table =
            true;
        // `ClassRealm::builtin_find_loaded_initiated_only` (zt t22/df10, define
        // page §9c), read once per VM, both modes. On by default with the
        // table above (its rows are the record); `=0` restores the ancestor
        // answer.
        let builtin_find_loaded_initiated_only = resolved_entry_table;
        // `ClassRealm::builtin_image_class_rows` (zt df4 §9c), with the rule
        // above; `=0` keeps every image class the bootstrap loader's.
        let builtin_image_class_rows = builtin_find_loaded_initiated_only;
        // `ClassRealm::method_type_vm_types` (interpreter round i1 wave 48,
        // lane L5), read once per VM. On by default; `=0` turns it off.
        let method_type_vm_types =
            true;
        // `ClassRealm::initiating_records_on` (interpreter round i1 wave 48,
        // lane L5): `--real-jdk` always; `--compatible` unless
        // `CRATONVM_COMPAT_INITIATING_RECORDS=0`.
        let initiating_records_on = hotspot_alignments.aligned(
            config.compatibility_mode,
            cratonvm_types::HotSpotAlignment::InitiatingRecords,
        );
        // The `--compatible` halves of three loader behaviours `--real-jdk`
        // has had since waves 28-38 (interpreter round i1 wave 49, lane L5;
        // the i47-L5 `--compatible` gates page, items 6, 1 and 3), each read
        // once per VM: `ClassRealm::loader_constraints_on`,
        // `loader_throws_on`, `global_name_ask_on`.
        // Since wave 50 (lane L5) they are rows of the one alignment table
        // read above; the variables still answer as its legacy keys.
        let aligned = |a| hotspot_alignments.aligned(config.compatibility_mode, a);
        let loader_constraints_on = aligned(cratonvm_types::HotSpotAlignment::LoaderConstraints);
        let loader_throws_on = aligned(cratonvm_types::HotSpotAlignment::LoaderThrows);
        let global_name_ask_on = aligned(cratonvm_types::HotSpotAlignment::GlobalNameAsk);
        // `ClassRealm::hotspot_aligned`: the effective set (every row under
        // `--real-jdk`).
        let hotspot_aligned = if config.is_jdk_only() {
            cratonvm_types::HotSpotAlignments::all()
        } else {
            hotspot_alignments
        };
        // SBR3 `ref`: read before `config` moves into the VM. Discovery at
        // allocation is ON wherever the `java/lang/ref` constructors run as the
        // JDK's bytecode: `--real-jdk`, and `--compatible` over a real JDK with
        // the `JdkReferences` row (nio20, lane L4), which drops the constructor
        // stand-ins that were the processor's only other source.
        let ref_constructors_are_bytecode = config.is_jdk_only()
            || (!config.use_synthetic_jdk
                && aligned(cratonvm_types::HotSpotAlignment::JdkReferences));
        // Built here, not in the realm literal, so the processor below can be
        // handed the same striped note buffer (interpreter round i7 wave 143,
        // lane L6, `CRATONVM_REF_NOTES_PER_THREAD`, default ON): attached
        // before any allocation, so no note can miss the processor.
        let ref_discovery_state =
            crate::memory::ref_discovery::RefDiscoveryState::from_flags(ref_constructors_are_bytecode);
        let ref_note_stripes = ref_discovery_state.note_stripes().map(Arc::clone);
        // `ClassRealm::record_replaces_memo` (wave 49, lane L5): `--real-jdk`
        // only.
        let record_replaces_memo = config.is_jdk_only();
        // `ClassRealm::record_behind_memo` (wave 52, lane L5): `--compatible`
        // with the `RecordBehindMemo` row, over records that exist.
        let record_behind_memo = !config.is_jdk_only()
            && initiating_records_on
            && hotspot_alignments.contains(cratonvm_types::HotSpotAlignment::RecordBehindMemo);
        // `ClassRealm::load_class_asks_receiver_only` (interpreter round i2
        // wave 53, lane L5), read once per VM, both modes. On by default;
        // `=0` restores the caller-loader drive in the base `loadClass`
        // native's flat-store step.
        let load_class_asks_receiver_only = true;
        // `ClassRealm::class_path_package_at_link` (interpreter round i2
        // wave 62, lane L5), read once per VM. On by default (the lane's
        // switch-on commit); `=0` restores the lazy first-door build alone.
        let class_path_package_at_link =
            true;
        // `ClassRealm::unlinked_doors_check_package` (interpreter round i4
        // wave 94, lane L5), read once per VM. On by default (HotSpot parity);
        // `=0` leaves the step to the link and `Class.forName`.
        let unlinked_doors_check_package = true;
        // `ClassRealm::link_state_one_write` (interpreter round i4 wave 91,
        // lane L5), read once per VM. Default off (`runtime_flag_on`).
        let link_state_one_write =
            cratonvm_types::flags::runtime_flag_on("CRATONVM_LINK_STATE_ONE_WRITE");
        // `ClassRealm::classloader_null_name_intercepts` (interpreter round i2
        // wave 55, lane L5), read once per VM, both modes: the intercepts
        // retire over a real JDK only. On by default (lane L5's switch-on
        // commit); `CRATONVM_RETIRE_CLASSLOADER_NULL_NAME_INTERCEPTS=0`
        // restores them.
        let classloader_null_name_intercepts = config.use_synthetic_jdk;
        // `ClassRealm::indy_other_shapes_to_jdk` (interpreter round i1 wave
        // 48, lane L4), read once per VM. On by default;
        // `CRATONVM_INDY_OTHER_SHAPES_TO_JDK=0` turns it off.
        let indy_other_shapes_to_jdk =
            true;
        // `ClassRealm::lmf_reflective_to_jdk` (interpreter round i1 wave 49,
        // lane L4), read once per VM.
        // Phase 3 Fast-Boot Specialization: In --compatible mode, bypass InnerClassLambdaMetafactory
        // bytecode class generation and promote direct native lambda trampoline bridges.
        let lmf_reflective_to_jdk = true;
        // `ClassRealm::vh_exact_site_memo` (interpreter round i1 wave 50,
        // lane L4), read once per VM. On by default; `=0` turns it off.
        let vh_exact_site_memo = true;
        // `ClassRealm::indy_link_checks` (interpreter round i1 wave 49, lane
        // L4), read once per VM. On by default under --real-jdk.
        // Phase 3 Fast-Boot Specialization: In --compatible mode, use lightweight native
        // call site linkage by default (=0 of CRATONVM_INDY_CHECKS_EVERY_MODE).
        let check_family = |a| hotspot_alignments.aligned(config.compatibility_mode, a);
        let indy_link_checks = if config.compatibility_mode == CompatibilityMode::Compatible {
            // A mode-scoped default (gcj j3/x2 guard, `flag_default_matches_reader`):
            // ON as the row says on --real-jdk (`check_family`), opt-in (`=1`)
            // here under --compatible.
            cratonvm_types::flags::runtime_flag_default_on_if(
                "CRATONVM_INDY_CHECKS_EVERY_MODE",
                config.compatibility_mode != CompatibilityMode::Compatible,
            )
        } else {
            check_family(cratonvm_types::HotSpotAlignment::IndyLinkChecks)
        };
        // `ClassRealm::selection_checks` (interpreter round i1 wave 50, lane
        // L4), read once per VM. On by default.
        let selection_checks = check_family(cratonvm_types::HotSpotAlignment::SelectionChecks);
        // `ClassRealm::mh_combinator_checks` and `ldc_handle_access_checks`
        // (interpreter round i1 wave 50, lane L4), read once per VM. On by
        // default.
        let mh_combinator_checks =
            check_family(cratonvm_types::HotSpotAlignment::MhCombinatorChecks);
        let ldc_handle_access_checks =
            check_family(cratonvm_types::HotSpotAlignment::LdcHandleAccessChecks);
        // `CRATONVM_INTERP_PRECISE_OOP_MAPS` (interpreter round i6 wave 121,
        // lane L7): carried by the VM's type-map store, which every mapped
        // frame scan and remap already receives, so both read one per-VM
        // answer (`TypeMapStore::precise_scan`). Read here, before `config`
        // moves into the VM.
        let precise_oop_maps = config.interp_precise_oop_maps;
        // gcj j1/st: `-XX:StringTableSize`, for the String table's ledger below.
        let string_table_size = config.string_table_size;
        // j27 wave 9 (W9VER): `-XX:-CompactStrings`, read before `config` moves.
        let compact_strings_policy = config.compact_strings_enabled();

        // zt t18/df7: the launcher's native-access settings, for the VM's own
        // state (`loader_identity` below; `config` moves into the VM first).
        let native_access_config = config.native_access.clone();
        // zt t21/tg10: this VM's `-XX:±ClassUnloading` / `-Xnoclassgc`
        // (`VmConfig::class_unloading`), for `loader_identity` below.
        // `CRATONVM_CLASS_UNLOADING_OFF_KEEPS_CLASSES=0` is the kill: the
        // option is accepted and ignored again (classes unload).
        let class_unloading = effective_class_unloading(&config);
        let vm = Self {
            // Allocated at the top of this function, before the `register_*`
            // pass, because the capability policy filed under this id has to be
            // in the registry before it accepts its first registration.
            vm_identity,
            config,
            ffm_lazy_precise_claim_enabled: true,
            finalizer_ancestor_walk_past_finalizable: true,
            finalizer_ancestor_record_reachable: true,
            finalizer_bar_refusal_epoch: false,
            jni_reinit_witness_window: std::sync::atomic::AtomicU32::new(0),
            dbg_base_body_entry_filter:
                crate::runtime::interpreter::gci_i11jd_witness::filter_from_env(),
            lambda_switches: crate::runtime::interpreter::lambda::LambdaSwitches::from_flags(),
            deopt_chain_run_from_outermost: true,
            jit_ffm_direct_door: true,
            jit_ffm_door_trim: true,
            mh_compiled_link_unbound: true,
            dbg_g1_gray_prov: false,
            jni_switches: crate::native::jni::JniSwitches::from_flags(),
            zgc_multiarray_tlab: true,
            side_row_ephemeron_engaged: if gc_backend == GcBackend::Generational {
                true
            } else {
                gc_backend == GcBackend::Zgc
            },
            jit_varhandle_array_element_fast:
                crate::jit::helpers::varhandle_array_element_fast_from_flags(),
            jit_thin_helper_asks_resolver: true,
            jit_compat_bind_asks_registry: true,
            jit_compat_bind_asks_resolver: true,
            hw_float_cas: true,
            hw_subword_cas: true,
            hw_static_cas: true,
            hw_ref_cas_slot_loop: true,
            // R23-AB-FLIP: default ON since the JIT r23 triage A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
            jit_will_run_asks_resolver: true,
            jit_forced_seal_asks_resolver: false,
            jit_static_owner_loads_first: true,
            jit_native_site_owner_by_id: true,
            jit_static_owner_load_refused: parking_lot::Mutex::new(std::collections::HashSet::new()),
            // The class manager's user-loader parent links, shared so the
            // natives record them without the class-manager lock (interpreter
            // round i5 wave 103, lane L5; a process global before). Before
            // `class_manager` moves into its lock below.
            // zt t18/df7: and the VM's native-access state, installed from its
            // configuration (`panama::NativeAccessState`; process globals
            // before).
            loader_identity: {
                let tables =
                    cratonvm_native_builtins::classloader::LoaderIdentityTables::with_user_loader_parents(
                        vm_identity,
                        std::sync::Arc::clone(class_manager.user_loader_parents()),
                    );
                tables.native_access().install(&native_access_config);
                tables.set_class_unloading(class_unloading);
                tables
            },
            next_loader_id: std::sync::atomic::AtomicU32::new(
                cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED,
            ),
            #[cfg(feature = "gpu-offload")]
            offload_registry,
            // RT-7. Taken at the arming site near the top of this function, so
            // it describes the state of the barrier BEFORE this VM could
            // compile anything — which is the only moment at which the
            // question has an answer.
            gpu_array_barrier_armed,
            classes: crate::vm::realms::ClassRealm {
                stack_trace_source_files: parking_lot::RwLock::new(FxHashMap::default()),
                // Before `class_manager` moves into its lock (fields are
                // evaluated in the order written).
                published_supers: class_manager.class_store.published_supers(),
                class_store_next_id: class_manager.class_store.next_id_cell(),
                type_maps: {
                    let store = class_manager.class_store.type_map_store();
                    store.set_precise_scan(precise_oop_maps);
                    store
                },
                hidden_classes: class_manager.hidden_class_bits(),
                bootstrap_append_seen: class_manager.bootstrap_append_seen(),
                class_init_bits: class_manager.class_init_bits(),
                cha_witness: Default::default(),
                redefinition_log: class_manager.redefinition_log(),
                installed_retransform_files: parking_lot::Mutex::new(FxHashMap::default()),
                class_manager: OrderedPlRwLock::new(class_manager, LockLevel::ClassManager),
                anon_class_cache: std::array::from_fn(|_| AtomicU32::new(0)),
                statics: RwLock::new(FxHashMap::default()),
                statics_index: crate::vm::realms::class_realm::StaticsIndex::new(),
                static_varhandle_holders: RwLock::new(FxHashSet::default()),
                static_varhandle_fields: RwLock::new(FxHashMap::default()),
                static_final_array_lengths: RwLock::new(FxHashMap::default()),
                static_final_ref_classes: RwLock::new(FxHashMap::default()),
                frozen: crate::runtime::frozen_pages::FrozenVmState::default(),
                ffm_layout_facts: RwLock::new(FxHashMap::default()),
                // u32::MAX = "java/lang/System not prepared yet"; a real
                // ClassId can never be u32::MAX (see AUTOBOX_CLASS_ID's
                // reserved-range note in `types`).
                system_class_id: AtomicU32::new(u32::MAX),
                boolean_class_id: AtomicU32::new(u32::MAX),
                resolution_cache: RwLock::new(ResolutionCache::with_redefinition_log(
                    std::sync::Arc::clone(&vm_redefinition_log),
                )),
                resolution_failure_recorded: std::sync::atomic::AtomicBool::new(false),
                member_access_refusals: std::sync::atomic::AtomicU64::new(0),
                member_access_unverified: std::sync::atomic::AtomicU64::new(0),
                loader_constraint_violations: std::sync::atomic::AtomicU64::new(0),
                nest_host_claims: parking_lot::Mutex::new(FxHashMap::default()),
                nest_host_unsettled_reads: std::sync::atomic::AtomicU64::new(0),
                receiver_owner_unresolved: std::sync::atomic::AtomicU64::new(0),
                loader_throws_propagated: std::sync::atomic::AtomicU64::new(0),
                resolution_race_adoptions: std::sync::atomic::AtomicU64::new(0),
                loader_reentry_circularities: std::sync::atomic::AtomicU64::new(0),
                loader_reentry_reasks: std::sync::atomic::AtomicU64::new(0),
                load_class_caller_drives_skipped: std::sync::atomic::AtomicU64::new(0),
                // LEVEL: `Scratch` (L0): one insert per drive under
                // `CRATONVM_DBG=access`, taken with no other lock held.
                caller_drive_census: cratonvm_types::lock_order::OrderedPlMutex::new(
                    std::collections::HashMap::new(),
                    cratonvm_types::lock_order::LockLevel::Scratch,
                ),
                // LEVEL: `Scratch` (L0), as `caller_drive_census`.
                loader_request_census: cratonvm_types::lock_order::OrderedPlMutex::new(
                    std::collections::HashMap::new(),
                    cratonvm_types::lock_order::LockLevel::Scratch,
                ),
                nested_jar_walk_skipped: std::sync::atomic::AtomicU64::new(0),
                nested_jar_walk_asked: std::sync::atomic::AtomicU64::new(0),
                nested_jar_bytes_served: std::sync::atomic::AtomicU64::new(0),
                nested_jar_bytes_body: std::sync::atomic::AtomicU64::new(0),
                jar_loader_bytes_served: std::sync::atomic::AtomicU64::new(0),
                jar_loader_bytes_body: std::sync::atomic::AtomicU64::new(0),
                nested_jar_row_events: Default::default(),
                loader_nulls_propagated: std::sync::atomic::AtomicU64::new(0),
                loader_census_reported: std::sync::atomic::AtomicBool::new(false),
                member_access_census_reported: std::sync::atomic::AtomicBool::new(false),
                // Round 8 audit fix (CRIT #2): reflective lookup cache.
                link_resolver: LinkResolver::new(),
                vtable_manager: std::sync::Arc::new(parking_lot::RwLock::new(
                    crate::runtime::vtable::VtableManager::new(),
                )),
                shared_resolution:
                    crate::runtime::lockfree_resolve::SharedResolutionState::with_redefinition_log(
                        std::sync::Arc::clone(&vm_redefinition_log),
                    ),
                class_locks: RwLock::new(FxHashMap::default()),
                class_mirrors: RwLock::new(FxHashMap::default()),
                class_mirrors_reverse: RwLock::new(FxHashMap::default()),
                class_mirror_id_table: cratonvm_native_builtins::mirror_id_memo::MirrorIdTable::new(
                    cratonvm_native_builtins::mirror_id_memo::table_enabled_from_env(),
                ),
                getclass_mirror_words: crate::vm::realms::class_realm::GetClassMirrorWords::new(),
                class_meta_words: crate::vm::realms::class_realm::ClassMetaWords::new(),
                mirror_fast_table: crate::vm::realms::class_realm::MirrorFastTable::new(),
                initiating_resolution_cache: RwLock::new(FxHashMap::default()),
                initiating_records: RwLock::new(FxHashMap::default()),
                initiating_record_find_hits: std::sync::atomic::AtomicU64::new(0),
                initiating_record_define_refusals: std::sync::atomic::AtomicU64::new(0),
                jdk_names_initiated: RwLock::new(FxHashMap::default()),
                jdk_names_initiated_armed: std::sync::atomic::AtomicBool::new(resolved_entry_table),
                resolved_class_entries: RwLock::new(FxHashMap::default()),
                resolved_entry_table,
                builtin_find_loaded_initiated_only,
                builtin_image_class_rows,
                builtin_find_loaded_withheld: std::sync::atomic::AtomicU64::new(0),
                builtin_find_loaded_served_by_row: std::sync::atomic::AtomicU64::new(0),
                builtin_find_loaded_served_by_supertype: std::sync::atomic::AtomicU64::new(0),
                builtin_initiated_rows_filed: std::sync::atomic::AtomicU64::new(0),
                method_type_vm_types,
                initiating_records_on,
                loader_constraints_on,
                hotspot_aligned,
                alignment_census,
                loader_throws_on,
                global_name_ask_on,
                record_replaces_memo,
                record_behind_memo,
                load_class_asks_receiver_only,
                class_path_package_at_link,
                unlinked_doors_check_package,
                link_state_one_write,
                class_path_packages_checked: parking_lot::Mutex::new(
                    rustc_hash::FxHashSet::default(),
                ),
                class_path_packages_refused: parking_lot::Mutex::new(
                    rustc_hash::FxHashSet::default(),
                ),
                // Interpreter round i7 wave 137, lane L5 (HotSpot parity,
                // default on, `=0` off): read once per VM.
                builtin_loader_add_class: true,
                builtin_loader_classes_recorded: parking_lot::Mutex::new(
                    rustc_hash::FxHashSet::default(),
                ),
                builtin_loader_records_pending: parking_lot::Mutex::new(Vec::new()),
                builtin_loader_records_pending_any: std::sync::atomic::AtomicBool::new(false),
                classloader_null_name_intercepts,
                initiating_memo_only_answers: std::sync::atomic::AtomicU64::new(0),
                global_name_record_census: Default::default(),
                indy_other_shapes_to_jdk,
                lmf_reflective_to_jdk,
                vh_exact_site_memo,
                indy_link_checks,
                selection_checks,
                unsafe_byte_field_offsets: true,
                mh_combinator_checks,
                ldc_handle_access_checks,
                loader_global_name_transparency: RwLock::new(FxHashMap::default()),
                loader_link_verdicts: RwLock::new(FxHashMap::default()),
                lambda_proxies: RwLock::new(FxHashMap::default()),
                lambda_proxy_hit_bits: crate::vm::realms::class_realm::LambdaProxyHitBits::from_env(
                ),
                // JIT round 21 wave 25, lane rawwrite: on by default (`=0` off).
                raw_write_settles_finalizer_flips: true,
                lambda_native_infos: parking_lot::Mutex::new(FxHashMap::default()),
                proxy_method_cache: RwLock::new(FxHashMap::default()),
                lambda_impl_owner_memo: RwLock::new(FxHashMap::default()),
                linked_virtual_sites: RwLock::new(FxHashMap::default()),
                linked_invokevirtual_ame_text: true,
                lambda_proxy_hosts: RwLock::new(FxHashMap::default()),
                next_lambda_id: AtomicU32::new(
                    crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE,
                ),
                annotation_proxy_cid: AtomicU32::new(u32::MAX),
                annotation_proxy_absent_epoch: std::sync::atomic::AtomicU64::new(u64::MAX),
                class_layout_domain,
                ann_proxy_latch: crate::vm::realms::class_realm::ann_proxy_latch_from_env(),
                primitive_mirrors: RwLock::new(FxHashMap::default()),
                module_mirrors: RwLock::new(FxHashMap::default()),
                layer_module_packages: RwLock::new(FxHashMap::default()),
                layer_module_packages_any: std::sync::atomic::AtomicBool::new(false),
                layer_module_pins: parking_lot::Mutex::new(Vec::new()),
                layer_module_prune_at: AtomicUsize::new(64),
                cached_string_num_fields: AtomicUsize::new(0),
                compact_strings: std::sync::atomic::AtomicBool::new(false),
                compact_strings_policy,
                compact_strings_off_flag: std::sync::atomic::AtomicBool::new(false),
                cached_class_mirror_num_fields: AtomicUsize::new(0),
                class_mirror_layout: std::sync::OnceLock::new(),
                class_init_waiters: parking_lot::Mutex::new(FxHashMap::default()),
                class_init_errors: parking_lot::Mutex::new(FxHashMap::default()),
                class_loading_locks: parking_lot::Mutex::new(FxHashMap::default()),
                // T10.9.E — lazy per-(ClassId, slot_index) field descriptor cache.
                field_descriptor_cache: parking_lot::RwLock::new(
                    crate::runtime::fx_collections::FxHashMap::default(),
                ),
                // WP0.2 — ObjectStreamClass.lookup(cls) cache.
                osc_cache: crate::runtime::serialization::OscCache::new(),
                // JIT round 16 (lane mh, MH3-2): bound `MethodType`s.
                bound_method_types: cratonvm_native_api::method_type_table::MethodTypeTable::new(),
            },
            mem: crate::vm::realms::HeapRealm {
                diagnostics: Default::default(),
                helper_panics: Default::default(),
                ref_processor: OrderedPlMutex::new(
                    {
                        let mut processor = cratonvm_gc::ReferenceProcessor::new_for_backend(
                            soft_ref_lru_ms_per_mb,
                            soft_lru_takes_hotspot_inputs_by_default(gc_backend),
                            gc_backend == GcBackend::Zgc,
                        );
                        // Wave 143 (L6): the realm's striped notes, if built.
                        if let Some(stripes) = ref_note_stripes {
                            processor.attach_allocation_note_stripes(stripes);
                        }
                        heap.zgc_attach_persistent_skip_bits(&mut processor);
                        #[cfg(feature = "gpu-offload")]
                        heap.gpu_coord().set_owner_vm_id(vm_identity as u64);
                        processor
                    },
                    LockLevel::RefProcessor,
                ),
                heap,
                concurrent_satb,
                concurrent_gc_state,

                operand_stack_pool: crate::runtime::alloc_fastpath::VecPool::new(64),
                tag_pool: crate::runtime::alloc_fastpath::VecPool::new(64),
                string_pool: RwLock::new(FxHashMap::default()),
                // gcg g2/w: both switches default on (=0 kills each) (`memory::string_table`).
                string_table: {
                    let table = crate::memory::string_table::StringTableState::from_flags();
                    // gcj j1/st: `-XX:StringTableSize`'s start size.
                    table.set_hotspot_ledger(table.hotspot_ledger_on(), string_table_size);
                    table
                },
                // gcg g4/rs: opt-in (`memory::ref_discovery`); SBR3 `ref`: on
                // wherever the `java/lang/ref` constructors are bytecode
                // (`--real-jdk`, or the `JdkReferences` row).
                ref_discovery: ref_discovery_state,
                singleton_oom: RwLock::new(None),
                // gcd d2/j: the per-message preallocated OOMEs (opt-in).
                preallocated_oome: RwLock::new(
                    crate::vm::realms::heap_realm::PreallocatedOome::default(),
                ),
                var_handle_roots: RwLock::new(
                    crate::vm::realms::heap_realm::VarHandleRoots::default(),
                ),
                gc_barrier: GcBarrier::new(),
                finalizer_thread: cratonvm_gc::reference::FinalizerThread::new(),
                cleaner_thread: cratonvm_gc::reference::CleanerThread::new(),
                gc_requested: std::sync::atomic::AtomicBool::new(false),
                native_array_gc_requested: std::sync::atomic::AtomicBool::new(false),
                zgc_side_row_cycle_pins: parking_lot::Mutex::new(Default::default()),
                // T19.3.G1 — allocation-storm observability counters.
                tlab_refill_count: std::sync::atomic::AtomicU64::new(0),
                gc_cycle_count: std::sync::atomic::AtomicU64::new(0),
                gc_pause_ns_total: std::sync::atomic::AtomicU64::new(0),
                // gc-common w7-g: `System.gc()` coalescing (per VM).
                system_gc_collections: std::sync::atomic::AtomicU64::new(0),
                // gc-common w7-f: ZGC's collector beans and pools.
                zgc_gc_beans: {
                    let beans = cratonvm_gc::gc_metrics::BackendGcBeans::new(
                        cratonvm_gc::gc_metrics::BackendBeanShape::Zgc,
                    );
                    // zs s2/ob: `ZGC Young Generation`'s `init` is HotSpot's
                    // `MinHeapSize` (`CRATONVM_ZGC_HOTSPOT_POOL_INIT`), the
                    // figure the `[gc,init]` block prints as `Min Capacity`.
                    beans.set_zgc_min_capacity(zgc_jmx_min_capacity);
                    // zt t30/nh: the code pools a `GcInfo` names follow the
                    // VM's code cache layout (`CRATONVM_JMX_CODE_CACHE_HOTSPOT_LAYOUT`).
                    if let Some(layout) = zgc_code_cache_layout {
                        beans.set_code_cache_layout(layout);
                    }
                    std::sync::Arc::new(beans)
                },
                gc_unproductive_streak: std::sync::atomic::AtomicU32::new(0),
                // gc-common w18-g: `-verbose:gc` as it is now (`setVerbose`).
                verbose_gc: std::sync::atomic::AtomicBool::new(verbose_gc_at_launch),
                bytes_allocated_total: std::sync::atomic::AtomicU64::new(0),
                // gc-common w6-c: the per-VM allocation state that replaced
                // `tlab::PROCESS_ALLOCATED_BYTES` (as the ThreadMXBean source)
                // and the four wedge statics of `gc_and_alloc.rs`.
                thread_allocated_total: std::sync::atomic::AtomicU64::new(0),
                tlab_wedge: crate::vm::realms::heap_realm::TlabWedgeState::default(),
                // gci i45/sc: the TLAB flags, per VM (`gci_i45_sc_tlab_sizing`).
                tlab_sizing,
                // gen r4w5/thrash5: the GC-overhead limit's progress marks.
                gc_progress: crate::vm::realms::heap_realm::GcProgressMarks::default(),
                // gcd d2/j: the allocation ladder's futile-young backoff and
                // `jit_alloc_oom` counter.
                alloc_ladder: crate::vm::realms::heap_realm::AllocLadderState::default(),
                // zr z1/br: the ZGC mark start's take-over cover latch.
                zgc_satb_window: crate::vm::realms::heap_realm::ZgcSatbWindowState::default(),
                // zt t24/x20: values `setVMOption` wrote to modelled
                // manageable flags (none at boot).
                managed_flags: crate::vm::realms::heap_realm::ManagedHotspotFlags::default(),
                // zt t31/x10: the heap dumps' shared file sequence.
                heap_dumps: crate::vm::realms::heap_realm::HeapDumpSequence::default(),
                // zt t31/x10: G1's periodic-collection service (not started).
                g1_periodic: crate::vm::realms::heap_realm::G1PeriodicState::default(),
                // zt t31/x10: ZGC's `gc,mmu` pause history (empty).
                zgc_mmu: parking_lot::Mutex::new(crate::vm::realms::heap_realm::ZStatMmu::default()),
            },

            natives: crate::vm::realms::NativeRealm {
                native_methods,

                fd_table: FileDescriptorTable::new(),
                native_memory: parking_lot::Mutex::new(crate::native::ffi::NativeMemoryTable::new()),
                native_libraries: parking_lot::Mutex::new(Vec::new()),
                native_library_loaders: parking_lot::Mutex::new(Vec::new()),
                jni_lazy_linked_methods: parking_lot::RwLock::new(std::collections::HashMap::new()),
                upcall_table: parking_lot::Mutex::new(crate::native::ffi::UpcallTable::new()),
                jni_global_refs: parking_lot::Mutex::new(crate::native::jni::JniGlobalRefs::new()),
                ldc_slots: parking_lot::Mutex::new(rustc_hash::FxHashMap::default()),
                jni_native_methods: crate::native::jni::JniNativeMethodTable::default(),
                // gce e1/j: see the field's doc (unique per VM, never 0-based).
                jni_native_generation: std::sync::atomic::AtomicU64::new(
                    (vm_identity as u64) << 32,
                ),
                jni_phase_census: crate::native::jni::JniPhaseCensus::from_env(),
                jni_bridge_invocations: cratonvm_native_api::registry::ShardedCounter::new(),
                matcher_leaf_admission: std::array::from_fn(|_| {
                    std::sync::atomic::AtomicU64::new(0)
                }),
                netty_tcnative_real: std::sync::atomic::AtomicBool::new(false),
                var_handle_exact_slot: std::sync::atomic::AtomicU32::new(u32::MAX),
                box_value_of_intrinsic: std::array::from_fn(|_| std::sync::atomic::AtomicU8::new(0)),
                exact_return_wrapper_ids: std::array::from_fn(|_| {
                    std::sync::atomic::AtomicU32::new(u32::MAX)
                }),
                native_sync_facts: Default::default(),
                hotspot_object_sizes: true,
                hotspot_size_memo: parking_lot::Mutex::new(rustc_hash::FxHashMap::default()),
                // nio21 wave 9 L1: the interpreter's native call-site memos (per VM).
                interp_call_sites: crate::vm::realms::native_realm::InterpreterNativeCallSites::new(
                ),
                // t1 wave 4 (lane Y2): the resolver's `--real-jdk-report` sink (per VM).
                native_shadow_sink: crate::vm::NativeShadowSink::new(),
            },

            threads: crate::vm::realms::ThreadRealm {
                throwable_stacks: (0..crate::vm::realms::thread_realm::THROWABLE_STACK_SHARDS)
                    .map(|_| RwLock::new(Default::default()))
                    .collect(),
                throwable_layout_cache: crate::vm::realms::thread_realm::ThrowableLayoutCache::new(
                ),
                thread_layout_cache: crate::vm::realms::thread_realm::ThreadLayoutCache::new(),
                monitors: MonitorTable::new(),
                main_thread_group: RwLock::new(None),
                main_thread_group_init: parking_lot::Mutex::new(MainThreadGroupInit::Idle),
                thread_registry: ThreadRegistry::new(),

                virtual_scheduler: crate::threading::VirtualThreadScheduler::new_default(),
                virtual_thread_manager: Arc::new(
                    crate::threading::VirtualThreadManager::with_default_parallelism()
                        .with_jdk_continuations(jdk_continuations)
                        // gci i11/vt item 4 (default ON since 2026-10-17, `=0`
                        // kills; read once per VM): forget terminated JDK
                        // continuations' records.
                        .with_reap_terminated(true)
                        .with_retired_jdk_entry_cap(retired_jdk_entry_cap),
                ),
            },
            system_out: RwLock::new(None),
            system_err: RwLock::new(None),
            system_in: RwLock::new(None),
            system_stream_written: Default::default(),
            system_properties: RwLock::new(sys_props),
            self_arc: RwLock::new(None),
            deferred_starts,
            debug: crate::vm::realms::DebugRealm {
                oom_dump_written: std::sync::atomic::AtomicBool::new(false),
                missing_natives_log: parking_lot::Mutex::new(Vec::new()),
                flight_recorder: parking_lot::Mutex::new(cratonvm_jfr::create_flight_recorder()),
                jfr_startup_claimed: std::sync::atomic::AtomicBool::new(false),
                jfr_dump_on_exit: parking_lot::Mutex::new(None),
                jfr_java_recording: parking_lot::Mutex::new(None),
                jfr_java_recording_running: std::sync::atomic::AtomicBool::new(false),
                jfr_java_output: parking_lot::Mutex::new(None),
                jfr_gc_reference_counts: parking_lot::Mutex::new(None),
                jcmd_processor: parking_lot::Mutex::new(None),
                perf_data_file: parking_lot::Mutex::new(None),
                perf_foreign_views: parking_lot::Mutex::new(Vec::new()),
                class_trace: std::sync::OnceLock::new(),
                // Before `debug_state`, which moves the state it is taken from.
                #[cfg(feature = "experimental-debug")]
                suspension_hint: initial_debug_state.suspension_hint_handle(),
                #[cfg(feature = "experimental-debug")]
                debug_state: parking_lot::Mutex::new(initial_debug_state),
                // Wave 54 (lane L1): the gates read the older JVMTI
                // manager's enabled events (the D14 bridge's listener gate);
                // the handle is taken before the env moves into its lock.
                #[cfg(feature = "experimental-debug")]
                debugger_gates: crate::debug::DebuggerGates::with_legacy_jvmti_events(
                    jvmti_env.event_manager.enabled_mask_handle(),
                ),
                #[cfg(feature = "experimental-debug")]
                jvmti_env: parking_lot::Mutex::new(jvmti_env),
                // gci i12/ev: no deferred `ClassLoad` yet.
                #[cfg(feature = "experimental-debug")]
                deferred_class_loads_pending: std::sync::atomic::AtomicBool::new(false),
                #[cfg(feature = "experimental-debug")]
                lambda_proxy_rows: parking_lot::Mutex::new(
                    crate::runtime::fx_collections::FxHashMap::default(),
                ),
                #[cfg(feature = "experimental-debug")]
                breakpoints_active: std::sync::atomic::AtomicBool::new(false),
                #[cfg(feature = "experimental-debug")]
                debug_event_tx: std::sync::Mutex::new(None),
                #[cfg(feature = "experimental-debug")]
                debug_event_rx: std::sync::Mutex::new(None),
                #[cfg(feature = "experimental-debug")]
                jdwp_server_wake: std::sync::Mutex::new(None),
                #[cfg(feature = "experimental-debug")]
                jdwp_server_wake_pending: std::sync::atomic::AtomicBool::new(false),
                #[cfg(feature = "experimental-debug")]
                used_unlinked_classes: crate::debug::UsedUnlinkedClasses::new(),
                diagnostic_counters: crate::runtime::diagnostics::DiagnosticCounters::new(),
                swallow_counter: std::sync::atomic::AtomicU64::new(0),
                stack_dump_requested: std::sync::atomic::AtomicBool::new(false),
                stack_dump_ack_count: std::sync::atomic::AtomicU32::new(0),
                stack_dump_acked_tids: parking_lot::Mutex::new(Vec::new()),
                stack_sample_mode: std::sync::atomic::AtomicBool::new(false),
            },
            jit: crate::vm::realms::JitRealm {
                // Tier proposal W3-1 (round 12 wave 4): every withdrawal from
                // this cache demotes the methods it leaves without a body, in
                // this VM's manager only. Before `tiered_manager` moves in
                // below (fields are evaluated in the order written).
                jit_cache: {
                    let cache = JitCache::new();
                    cache.install_withdrawal_sink(tiered_manager.withdrawal_sink());
                    cache
                },
                profile_store: ProfileStore::new(),
                inline_gates: std::sync::OnceLock::new(),
                jit_skip_set: Default::default(),
                jit_gate_pass: parking_lot::RwLock::new(FxHashMap::default()),
                finalizer_bar_refusals: parking_lot::RwLock::new(FxHashMap::default()),
                execute_entry_records: parking_lot::RwLock::new(FxHashMap::default()),
                execute_reentry_records: parking_lot::RwLock::new(FxHashMap::default()),
                execute_method_index: parking_lot::RwLock::new(FxHashMap::default()),
                // wire-tiered-manager Step 6: honor the CRATONVM_TIER_* threshold
                // overrides (c1/c2/osr/c2_min/enabled). Identical to the default
                // policy when the environment is unset. Stamps with this VM's
                // code-state epoch, not the process one (interpreter round i1
                // wave 14); built above, beside that epoch.
                tiered_manager,
                deopt_log: parking_lot::Mutex::new(crate::jit::deopt::DeoptimizationLog::new()),
                despec_registry: Arc::new(crate::jit::deopt::DespecRegistry::new()),
                // M7 — this VM's runtime de-speculation verdicts. Both
                // constructors weakly register the registry so the two
                // VM-blind free functions in `compile_gate` (the class
                // unload / redefine forget, which the tiered manager drives)
                // can still reach it. Its flush epoch is this VM's code-state
                // epoch (interpreter round i1 wave 15); built above.
                runtime_despec,
                method_epochs: parking_lot::RwLock::new(FxHashMap::default()),
                method_epoch_overflow: std::sync::atomic::AtomicU64::new(0),
                // JIT slow-path allocation: per-class init recipe cache.
                jit_alloc_class_cache: crate::jit::alloc_class_cache::JitAllocClassCache::new(),
                bridge: crate::runtime::interpreter::JitBridgeState::default(),
                code_state_epoch,
                redefine_epoch,
                class_layout_domain,
                supersede_epoch: crate::vm::realms::JitRealm::new_supersede_epoch(),
                // nio21 wave 7 L3: the `Unsafe` field-cell table and the
                // compareAndExchange rows' policy memos (per VM).
                unsafe_inline: crate::vm::realms::jit_realm::UnsafeInlineCells::new(),
                // nio21 wave 8 L2: the thin-helper gate memos, the Matcher
                // leaf memos and the bypass-marking latch (per VM).
                direct_helper_gates: crate::vm::realms::jit_realm::DirectHelperGateSites::new(),
                // nio21 wave 11 L5: the `static final VarHandle` plans the
                // constant-handle inline reads (per VM).
                vh_const_plans: crate::vm::realms::jit_realm::VhConstPlans::new(),
                vh_static_cells: crate::jit::vh_static_cells::VhStaticCells::new(),
                // nio21 wave 12 L2: the JIT-readable primary-supers displays
                // the constant-handle inline's subclass checks read (per VM).
                supers_displays: crate::vm::realms::jit_realm::SupersDisplays::new(),
                // JIT r21 wave 1, lane reflect (REFLECT21-4): the reflective
                // links' class-id memo (per VM).
                reflect_link_ids: crate::vm::realms::jit_realm::ReflectLinkIds::default(),

                // JIT round 21 lane hash2: the compiled `Object.hashCode()`
                // identity arm's per-class verdict bytes (per VM).
                identity_hash_classes: crate::vm::realms::jit_realm::IdentityHashClassBytes::new(),
                // JIT round 21 lane hashlock (HASH2-21-1): the compiled
                // `Object.equals(Object)` identity arm's verdict bytes (per VM).
                identity_equals_classes: crate::vm::realms::jit_realm::IdentityHashClassBytes::new(
                ),
            },
            // WP1.3 — bootstrap init-level state machine. Starts at 0
            // ("VM construction in progress"); `Vm::new` bumps it to 1
            // after `SharedVm` is wrapped in `Arc` and the main thread
            // is registered.
            init_level: AtomicI32::new(0),
            init_level_waiters: Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new())),
            boot_finished_latch: std::sync::atomic::AtomicBool::new(false),
        };

        // JIT round 21 wave 19 (item 6 of `r21-review-w18-findings-FIXED-20261020.md`):
        // the reference processor follows this VM's
        // `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`, which also arms the
        // interpreter's `return`-arm gate bit, so the two can never disagree.
        vm.mem
            .ref_processor
            .lock()
            .set_reregister_at_init_return(vm.config.finalizer_reregister_at_init_return);

        // wire-tiered-manager Step 4 (PGO handoff C1 → C2): turn on interpreter
        // profile recording when the opt-in gate is set. This MUST happen here, at
        // VM init (before any method frame executes), not lazily at the JIT
        // threshold — the dispatch loop captures `is_profiling_enabled()` once per
        // frame entry, so a method already warming up would never record. With the
        // gate set, the interpreted ("C1"/warmup) phase populates
        // `profile_store` (branch bias, receiver types, loop trips); the optimizing
        // C2 compile then consumes it. Default-OFF: `enable_profiling` is never
        // called, every `record_*` short-circuits, and behaviour is unchanged.
        // `CRATONVM_TIER_PGO` — the original opt-in, and still the one that
        // turns recording on for the whole process.
        //
        // `CRATONVM_TIER_PGO_ALWAYS` is the same switch under a name that says
        // what it is FOR. Branch recording is now one relaxed `fetch_add` into a
        // per-method array found through a per-thread handle cache
        // (`record_branch_for_frame`), not a fingerprint + shard lock + slot
        // mutex + map entry, so the argument that kept it off — "a GLOBAL cost
        // paid for a LOCAL benefit" — is the thing that changed and the thing
        // this flag exists to re-measure. If it prices out, the C2 window
        // (`arm_branch_profiling_for_c2`) can go, and with it the situation
        // where the optimizing tier's scheduler reads an empty `branch_counts`
        // at essentially every compile.
        //
        // Default OFF: this is a measurement lever until somebody takes the
        // measurement, not a default flipped on an argument.
        if crate::runtime::env_cache::tier_pgo()
            || None::<std::ffi::OsString>.is_some()
        {
            crate::jit::profile::enable_profiling(true);
        }
        // Receiver-type and call-site recording (default ON) feeds the JIT
        // only: speculative inlining, the MIC pre-population and the tiered
        // manager. Under `--nojit` / `CRATONVM_DISABLE_JIT` nothing reads it,
        // and it is not free — the virtual door's `door_record_receiver` takes
        // a `ReceiverRecorder` mutex on every warm virtual call, and the
        // general `invokestatic` / `invokespecial` paths do a profile-store
        // shard lookup per call (interpreter round i1 wave 23, lane L4;
        // docs/feature-designs/jit-ir-optimizations.md). The
        // two consumers that exist without a compiler keep it on: a profile
        // save (`CRATONVM_JIT_PROFILE_SAVE`, written in `Drop for Vm`) and an
        // AOT training run (`-XX:AOTMode=record`, synced into the training
        // recorder at shutdown). HotSpot's `-Xint` likewise records no
        // `MethodData`. The master gate (`CRATONVM_TIER_PGO`) is unaffected:
        // `is_receiver_profiling_enabled` still answers it.
        let receiver_profile_consumer = !crate::runtime::env_cache::disable_jit()
            || cratonvm_jit::profile_store::save_path_from_env().is_some()
            || matches!(vm.config.aot_mode, crate::config::AotMode::Training);
        // Both runtime modes need receiver and call-site evidence throughout
        // warmup. A startup bypass here lasts for the entire VM lifetime and
        // starves hot FFM loops of the shapes required by the optimizing tier.
        // Fast-boot compilation thresholds may defer compilation, but must not
        // disable its profile input. Keep explicit opt-outs and no-consumer
        // suppression identical in both modes.
        let receivers_wanted =
            crate::runtime::env_cache::tier_pgo_receivers() && receiver_profile_consumer;
        if crate::jit::profile::receivers_per_vm_enabled() {
            // JIT round 21 wave 4, lane ic
            // (`r21-receiver-profiling-gate-is-per-process`): this VM's answer
            // is its own store's, and the process gate only ever goes ON ("some
            // VM records"), so a VM with no consumer no longer switches
            // recording off for the others. The invoke doors ask both
            // (`dispatch_virtual::receiver_profiling_on`). The master gate
            // still records receivers on its own, whatever this store says
            // (`receivers_recording_answer`; JIT round 21 wave 7, lane ic).
            vm.jit.profile_store.set_receivers_on(receivers_wanted);
            if receivers_wanted {
                crate::jit::profile::enable_receiver_profiling(true);
            }
        } else {
            crate::jit::profile::enable_receiver_profiling(receivers_wanted);
        }

        // ── Replay a previous run's profile, if one was named ─────────
        //
        // `CRATONVM_JIT_PROFILE_LOAD=<path>`, default unset. Here and not
        // earlier because the store must exist and profiling must already be
        // configured; here and not later because the seeds are only useful
        // before the first method gets hot.
        //
        // The resolver is `find_unique_class_by_name`, deliberately NOT the
        // deprecated loader-blind `find_class_by_name`. A class is `(name,
        // defining loader)` and the file stores only the name, so a name two
        // loaders define is ambiguous and must resolve to `None` — replay
        // simply does less under per-module loaders rather than seeding the
        // wrong class's shape.
        //
        // Never fails: a corrupt, truncated or missing file is a `warn!`/`info!`
        // and the VM continues with an empty profile. A profile is an
        // optimization hint, and **a bad profile must never be able to change a
        // program's answer** — refusing to start is a way of changing it.
        {
            let resolver = |name: &str| -> Option<u32> {
                vm.classes
                    .class_manager
                    .try_read()
                    .and_then(|cm| cm.find_unique_class_by_name(name))
                    .map(|cid| cid.as_u32())
            };
            // Format version 2: every record is checked against the live
            // method's bytecode length before it seeds. `Busy` parks it; the
            // definition sweep revisits every class from slot 0.
            let code_len = |class_id: u32, name: &str, descriptor: &str| match vm
                .classes
                .class_manager
                .try_read()
            {
                Some(cm) => crate::runtime::interpreter::jit_bridge::live_method_code_len(
                    &cm, class_id, name, descriptor,
                ),
                None => cratonvm_jit::profile_store::LiveCode::Busy,
            };
            cratonvm_jit::profile_store::load_if_configured(
                &vm.jit.profile_store,
                &resolver,
                &code_len,
            );
        }

        // Post-construction: pre-initialize critical static fields for core
        // classes when running with real JDK bytecode.  This must happen after
        // the struct is fully built because the helpers need &SharedVm.
        if !vm.config.use_synthetic_jdk {
            crate::vm::vm_object::pre_init_string_statics(&vm);
            if let Err(error) = crate::vm::vm_object::pre_init_class_statics(&vm) {
                if crate::runtime::native_oom::unwind_ok() {
                    crate::runtime::native_oom::raise(error.length, error.what);
                }
                std::panic::panic_any(crate::error::VmInitializationError {
                    detail: "java.lang.OutOfMemoryError: Java heap space".into(),
                });
            }
            crate::vm::vm_object::pre_init_wrapper_type_fields(&vm);

            // Session 10: Validate native method coverage at bootstrap time.
            // Scans all loaded non-synthetic classes for ACC_NATIVE methods and
            // checks each has a registered Rust implementation. Logs a report.
            if tracing::enabled!(tracing::Level::WARN) || tracing::enabled!(tracing::Level::INFO) {
                let report = crate::vm::vm_object::validate_native_coverage(&vm);
                report.log_report();
                if !report.missing.is_empty() {
                    tracing::warn!(
                        "[NativeBridge] {} ACC_NATIVE methods have no Rust implementation; \
                         they will throw UnsatisfiedLinkError if called",
                        report.missing.len()
                    );
                }
            }
        }

        // T6.3.1: install this VM's JVMTI event manager, and fire VMInit once
        // all core subsystems are up.
        //
        // Per-VM, not process-wide: a single global manager sent VM B's events
        // to VM A's callbacks and dropped VM B's own manager on the floor.
        crate::runtime::jvmti::install_manager_for_vm(
            vm.vm_identity,
            std::sync::Arc::new(crate::runtime::jvmti::JvmtiEventManager::new_for_vm(
                vm.vm_identity,
            )),
        );
        // AND the unattributed row, which is NOT redundant. Delivery sites
        // — GCStart, GCFinish; ClassPrepare moved to this VM's own row in
        // interpreter round i1 wave 17 (`fire_class_prepare_for_vm`) and
        // ClassLoad in wave 22 (`fire_class_load_for_vm`, which still falls
        // back to this row while a VM has none of its own) —
        // still resolve through `global_manager()`, which is an EXACT lookup
        // of row 0 with no fallback. Installing only the per-VM row above left
        // row 0 uninhabited in production, so every one of those events
        // resolved `None` and was silently dropped. The per-VM lane's note
        // that "their events land on the unattributed manager, which is
        // exactly where they land today" was true only while this call site
        // still populated row 0 — so it has to keep populating it until those
        // sites are attributed. The test that should have caught this installs
        // the row itself first, and so passes vacuously.
        crate::runtime::jvmti::install_global_manager(std::sync::Arc::new(
            crate::runtime::jvmti::JvmtiEventManager::new(),
        ));

        // Bridge the classloading crate's JVMTI ClassLoad hook to the runtime
        // JVMTI manager. `classloading` cannot depend on `vm`, so it exposes
        // a `fn(u32, &str, u64, u32)` hook registry; we install a small adapter
        // that forwards to the runtime fire_* free function. ClassPrepare has
        // no hook since interpreter round i1 wave 17: the VM posts it where
        // it prepares a class (`vm_util::link_claimed_class`,
        // `runtime::jvmti::fire_class_prepare_for_vm`).
        cratonvm_classloading::install_class_load_hook(class_load_adapter);

        // CRATONVM_DBG_ROOT_SOURCE: let the collector ask the root inventory
        // "who handed me this address?".
        //
        // The registry lives in this crate and the question is asked in the
        // GC crate, which cannot call back the other way -- so the GC owns a
        // doorway and we install the lookup through it, exactly as the
        // quiescence flag is arranged. Installed unconditionally; the lookup
        // itself short-circuits to `None` unless the flag is on, so this costs
        // one `OnceLock` write per VM.
        cratonvm_gc::gc_quiescence::install_root_source_hook(
            crate::memory::native_roots::root_source_of,
        );

        // T10.5 — register the class loader's vtable-install hook so each
        // class-link emits its descriptor vec into `shared.classes.vtable_manager`.
        //
        // Two things have to be registered here, in this order:
        //   1. This VM's `VtableManager`, under its `vm_identity` (the
        //      identity its class manager's metadata realm was bound to
        //      above), so the adapter has something to write into.
        //   2. The VM-scoped `vtable_install_adapter_scoped` itself, which
        //      the class loader invokes from inside its write lock on
        //      `ClassManager` with a `ClassHandle` naming the firing VM.
        //
        // Scoped since interpreter round i1 wave 19: the legacy `fn(u32, ..)`
        // hook wrote every VM's vtables into the FIRST VM's manager
        // (`GLOBAL_VTABLE_MANAGER`, per-vm-state V1), under colliding
        // `ClassId`s, and left every later VM's own manager empty.
        //
        // Any classes that were bootstrapped BEFORE this registration
        // (e.g. `java/lang/Object` loaded during `ClassManager::new`) will
        // be lazily replayed into the manager by the explicit catch-up
        // loop right below.
        crate::runtime::vtable::register_vm_vtable_manager(
            vm.vm_identity,
            &vm.classes.vtable_manager,
        );
        cratonvm_classloading::install_vtable_install_hook_scoped(
            crate::runtime::vtable::vtable_install_adapter_scoped,
        );
        // T10.9.A — also register the override hook so each subclass's
        // link-time slot-override fires `invalidate_for_override` on the
        // super-class vtable. Closes the CHA loop for cached invoke
        // entries and JIT leaf-class assumptions.
        cratonvm_classloading::install_vtable_override_hook_scoped(
            crate::runtime::vtable::vtable_override_adapter_scoped,
        );

        // Round 4 audit fix (CRIT) — register the ResolutionCache
        // invalidation hook so the JVMTI `RedefineClasses` path
        // (classloading::ClassManager::redefine_class step 9) drops every
        // cached field/method/call-site/condy resolution that refers to
        // (or was resolved into) the redefined class.
        //
        // Order: this must be installed BEFORE this VM is added to the
        // `RESOLUTION_INVALIDATE_VMS` registry below in `Vm::new` (where
        // `self_arc` is set), but installing the hook here is fine — the
        // adapter iterates the live-VM registry, which is empty until the
        // first VM is registered, so it falls through to a no-op.
        cratonvm_classloading::install_resolution_invalidate_hook(resolution_invalidate_adapter);

        // Give the GC crate a way to turn a `ClassId` into a name for its
        // failure-path reports. Same bridge, same reason: the gc crate cannot
        // name a `Class`. Without it the ZGC fragmentation report can only say
        // `class_id=418`, and the second run needed to decode that is a
        // different process with a different heap layout — so the answer does
        // not carry over. See `cratonvm_gc::collector::set_class_namer`.
        cratonvm_gc::collector::set_class_namer(class_name_adapter);
        // The owner-scoped twin, for a report whose heap knows its layout
        // domain (interpreter round i1 wave 23, lane L5).
        cratonvm_gc::collector::set_class_namer_in_domain(class_name_in_domain_adapter);

        // Found while investigating the guarded-inline-getfield SIGSEGV
        // cluster (that SIGSEGV's actual cause was a separate, already-fixed
        // bug — see `jit_invalidate_adapter`'s doc comment): `install_jit_invalidate_hook`
        // itself dates back further (it already backs `redefine_class`'s Step
        // 8 `fire_jit_invalidate_hook` call) but had no installer anywhere in
        // the VM, so that call was always a silent no-op. Wire it up so BOTH
        // `redefine_class` and the synthetic-stub-upgrade path
        // (`upgrade_synthetic_class` / `recompute_subclass_layouts`, which —
        // unlike JEP 109 redefine — really can change field layout) actually
        // evict stale JIT-compiled code.
        cratonvm_classloading::install_jit_invalidate_hook(jit_invalidate_adapter);

        // Catch-up pass: replay every class already in the ClassManager's
        // `vtable_descriptors` into this VM's own manager, so classes loaded
        // during ClassManager bootstrap (before the hook was live, or, in a
        // later VM of the process, before this VM's manager was registered)
        // end up in the VM's VtableManager too.
        {
            let cm = vm.classes.class_manager.read();
            // `slot_count()`, not `len()` — the latter is the live count and
            // under-runs the id space as soon as anything has been unloaded
            // (see `ClassStore::slot_count`). `vtable_descriptors_of` returns
            // `None` for tombstoned ids, so the extra slots cost nothing.
            let store_len = cm.class_store.slot_count() as u32;
            for cid in 0..store_len {
                let cid = crate::classloading::ClassId::new(cid);
                if let Some(entries) = cm.vtable_descriptors_of(cid) {
                    crate::runtime::vtable::install_vtable_descriptors(
                        &vm.classes.vtable_manager,
                        cid.as_u32(),
                        entries.to_vec(),
                    );
                }
            }
        }

        // Bridge the GC crate's hooks too. Every backend fires the pair once
        // per collection: G1 and ZGC from `VmHeap::collect_garbage` /
        // `collect_garbage_with_finalizers` (the dispatcher), Generational from
        // its own two entry points (gen r4/plumbing) — both since 2026-09-23.
        // Before that only the unselectable semi-space `Heap`'s `gc::collect`
        // fired them, so no agent ever saw a GarbageCollectionStart/Finish.
        // Zero-cost when no agent is attached.
        fn gc_start_adapter() {
            crate::runtime::jvmti::fire_gc_start();
            // Interpreter round i2 wave 63, lane L1: the C JVMTI table's
            // `GarbageCollectionStart`; one relaxed load when no env listens.
            #[cfg(feature = "experimental-debug")]
            crate::jvmti::native_env::post_gc(true);
        }
        fn gc_finish_adapter() {
            crate::runtime::jvmti::fire_gc_finish();
            // Wave 63: `GarbageCollectionFinish`, and the `ObjectFree` sweep.
            #[cfg(feature = "experimental-debug")]
            crate::jvmti::native_env::post_gc(false);
        }
        cratonvm_gc::install_gc_start_hook(gc_start_adapter);
        cratonvm_gc::install_gc_finish_hook(gc_finish_adapter);

        // Install the class-info resolver used by the heap's
        // layout-mismatch diagnostics. When `gen_heap::{get,set}_field`
        // drops an out-of-bounds access it logs the raw `ClassId`; this
        // hook lets it also print the class NAME and the class's REAL
        // declared field count, turning `ClassId(275)` into an
        // actionable `org/jboss/.../Foo (real layout has 7 fields)`.
        cratonvm_gc::install_class_info_hook(class_info_adapter);
        cratonvm_gc::install_class_info_in_domain_hook(class_info_in_domain_adapter);

        // Round 4 audit fix (CRIT) — publish the global Weak<SharedVm>
        // handle used by `resolution_invalidate_adapter`. Done at the
        // very end of `SharedVm::new` so the Arc returned by the caller
        // wrapping us already exists; if we ran before that, the
        // upgrade would always fail. The Arc-wrap step happens in
        // `Vm::new` (vm.rs) when `self_arc` is set — `set_global_shared_vm_for_hooks`
        // is idempotent, so calling it again from there is harmless and
        // covers the case where this constructor runs as part of a
        // larger flow that hasn't yet Arc-wrapped us.
        //
        // Until the handle is set, `fire_resolution_invalidate_hook`
        // still fires the adapter, but the adapter's
        // `Weak::upgrade()` returns None and the call is a no-op —
        // perfectly safe for unit tests that construct a SharedVm
        // outside an Arc.
        // `VMInit` on this VM's manager (attributed: a row-0 agent used to
        // see it for every VM in the process) is sent when the VM enters the
        // live phase, `set_init_level(3)` (interpreter round i1 wave 15).
        // Sent from here it reached nobody: the manager was created a few
        // lines up, and the agent bridge is installed by `Vm::new`.

        // Boot-cost summary. `SharedVm::new` is only part of startup — the
        // launcher still has to run `System.initPhase1/2/3` and load the
        // application's own classes after this returns — but it is the part
        // that is fixed cost for every run, so a regression in it is a
        // regression for every workload. The three phase lines above break the
        // total down; this line is what a `RUST_LOG=info` run can be grepped
        // for to compare two builds.
        let typed_boot_elapsed = bootstrap_phase
            .runtime_ready(
                vm.classes.class_manager.read().loaded_count() > 0
                    && vm.natives.native_methods.len() > 0,
            )
            .expect("bootstrap RuntimeReady invariants")
            .finish();
        tracing::info!(
            "boot: SharedVm::new total {:?} (phase 1 classpath {:?}, phase 2 core classes {:?})",
            typed_boot_elapsed,
            __boot_classpath_elapsed,
            __boot_core_classes_elapsed,
        );

        vm
    }
}
