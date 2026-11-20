#[allow(unused_imports)]
use super::*;

impl ClassManager {
    /// Create a new class manager with the three built-in class loaders.
    ///
    /// All classpath entries are scanned for `module-info.class` files during
    /// construction so that the module registry is populated before any class
    /// loading begins (N4: boot module loading baseline).
    pub fn new(
        boot_classpath: &[String],
        ext_classpath: &[String],
        app_classpath: &[String],
    ) -> Self {
        Self::with_app_fat_jar_emulation(boot_classpath, ext_classpath, app_classpath, true)
    }

    /// [`ClassManager::new`], choosing whether the APPLICATION class path
    /// emulates web and Spring Boot archives (see `ClassPath`'s
    /// `fat_jar_emulation`). `vm_init` passes `false` under `--real-jdk`, so a
    /// fat JAR there is HotSpot's: its root, with the application inside it
    /// loaded by Spring Boot's own launcher.
    pub fn with_app_fat_jar_emulation(
        boot_classpath: &[String],
        ext_classpath: &[String],
        app_classpath: &[String],
        app_fat_jar_emulation: bool,
    ) -> Self {
        let (bootstrap, extension, application) = if cratonvm_types::image_layout::capturing() {
            // Boot-image capture runs single-threaded before P1 so worker thread
            // stacks/arenas do not leak into imaged heap/class metadata.
            (
                BootstrapClassFinder::new(boot_classpath),
                ExtensionClassFinder::new(ext_classpath),
                ApplicationClassFinder::with_fat_jar_emulation(app_classpath, app_fat_jar_emulation),
            )
        } else {
            std::thread::scope(|s| {
                let app_handle = s.spawn(|| {
                    ApplicationClassFinder::with_fat_jar_emulation(app_classpath, app_fat_jar_emulation)
                });
                let bootstrap = BootstrapClassFinder::new(boot_classpath);
                let extension = ExtensionClassFinder::new(ext_classpath);
                let application = app_handle.join().expect("app classpath thread panicked");
                (bootstrap, extension, application)
            })
        };

        let mut module_registry = ModuleRegistry::new();

        // Eagerly scan all classpath entries for module-info.class files
        // (N1: module graph resolution + N4: boot module loading).
        // Opt-out safety net: `CRATONVM_BOOT_MODULE_REGISTRY=0` skips the eager
        // boot/ext/app module-info registration, leaving the registry empty (the
        // historic behaviour, in which the module-info parser was broken and
        // registered nothing — see `descriptor_from_module_attribute`). That
        // restores the fully-permissive classpath-only mode where every
        // `module_registry.is_empty()`-gated access check short-circuits to
        // "allow", in case populating real module metadata regresses an app that
        // relied on CratonVM not enforcing JPMS. Default-on: register modules so
        // service-provider discovery (`ServiceLoader` via module `provides`,
        // e.g. ToolProvider.getSystemJavaCompiler) and module labelling match
        // the real JDK.
        let register_modules = loader_flags().boot_module_registry;
        if register_modules {
            // `automatic = true` for the application class path: those jars are
            // on the class path (not a module path), so the real JDK puts them in
            // the unnamed module. We keep their descriptors for service discovery
            // and labelling but grant automatic-module access semantics so JPMS
            // readability/exports are not enforced between app jars (which would
            // otherwise break e.g. org.jboss.logging → org.apache.logging.log4j).
            // Bootstrap/extension are the genuine platform modules — keep them
            // strict.
            for (class_path, automatic) in [
                (bootstrap.class_path(), false),
                (extension.class_path(), false),
                (application.class_path(), true),
            ] {
                for bytes in class_path.scan_module_infos() {
                    Self::try_register_module_info(&mut module_registry, &bytes, automatic);
                }
            }
            // Ensure package-to-module mappings from the jimage are populated
            // into the registry so that all JDK packages are known even when
            // fast-boot module scan deferred individual module-info parses.
            bootstrap.class_path().populate_module_registry_packages(&mut module_registry);
            // A jlinked image keeps `java.base`'s recorded hashes in the
            // generated `SystemModules$all`, not in the `ModuleHashes`
            // attribute `try_register_module_info` reads: without them
            // `StackTraceElement.HashedModules` is empty and the real
            // `computeFormat` prints `java.base@25.0.4/...` where HotSpot
            // prints `java.base/...` (SBR3 `refmisc`).
            if module_registry.recorded_hash_names("java.base").is_none() {
                if let Ok(class_bytes) = bootstrap
                    .class_path()
                    .find_class("jdk/internal/module/SystemModules$all")
                {
                    if let Some(names) =
                        crate::module::system_modules_java_base_hash_names(&class_bytes)
                    {
                        module_registry.set_recorded_hash_names("java.base", names);
                    }
                }
            }
        }

        if !module_registry.is_empty() {
            module_registry.build_readability_graph();
            debug!(
                modules = module_registry.len(),
                "Module registry populated from classpath scan"
            );
        }

        Self {
            class_store: ClassStore::new(),
            bootstrap,
            extension,
            application,
            loaded_classes: hashbrown::HashMap::with_capacity_and_hasher(256, Default::default()),
            unbound_defines: FxHashMap::default(),
            name_definitions: FxHashMap::default(),
            virtual_components: FxHashMap::default(),
            pending_user_arrays: std::sync::Mutex::new(Vec::new()),
            module_registry,
            class_bytes_cache: FxHashMap::with_capacity_and_hasher(128, Default::default()),
            class_type_annotations: FxHashMap::default(),
            hidden_lookup_class_names: FxHashMap::default(),
            empty_record_classes: FxHashSet::default(),
            class_bytes_cache_fifo: std::collections::VecDeque::with_capacity(128),
            class_bytes_cache_size: 0,
            class_bytes_cache_cap: DEFAULT_CLASS_BYTES_CACHE_CAP,
            class_bytes_base_digest: FxHashMap::with_capacity_and_hasher(128, Default::default()),
            class_bytes_pinned: FxHashSet::default(),
            class_bytes_raw_agent_bases: FxHashSet::default(),
            class_bytes_pinned_size: 0,
            class_bytes_pinned_cap: DEFAULT_PINNED_CLASS_BASES_CAP,
            class_bytes_pinned_overflowed: false,
            class_bytes_refetched_released: 0,
            pre_verified_redefinitions: parking_lot::Mutex::new(FxHashMap::default()),
            metaspace_charges: FxHashMap::default(),
            metaspace_charged_bytes: 0,
            max_metaspace_bytes: None,
            metaspace_arena_tally: parking_lot::Mutex::new(MetaspaceArenaTally::default()),
            cds_class_cache: FxHashMap::with_capacity_and_hasher(64, Default::default()),
            loading_guard: FxHashSet::default(),
            loader_constraints: parking_lot::Mutex::new(
                crate::loader_constraints::LoaderConstraints::new(),
            ),
            synthetic_upgrade_absent: FxHashSet::default(),
            bootstrap_appended_classes: FxHashSet::default(),
            user_loader_parents: Arc::new(crate::loaders::UserLoaderParents::new()),
            bootstrap_append_seen: Arc::new(AtomicBool::new(false)),
            bootstrap_append_packages: FxHashMap::default(),
            vtable_descriptors: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            shadowed_signature_vtables: FxHashSet::default(),
            shadow_validation_per_signature: true,
            vtable_hidden_receiver_package: true,
            // zs s2/fp2: on by default (`=0` off).
            vtable_share_slots: true,
            skip_bytecode_verification: FxHashSet::default(),
            verifier_link_loads: FxHashMap::default(),
            type_maps_at_link: true,
            real_boot_classes_latch: true,
            real_boot_classes_latched: std::sync::atomic::AtomicBool::new(false),
            // Off here (interpreter round i4 wave 90, lane L5); --compatible boots turn it
            // on in `vm_init` (compat-fast-boot, `=0` keeps it off), hence the row's `0` off word.
            define_boot_maps_at_link: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DEFINE_BOOT_MAPS_AT_LINK",
            ),
            skip_boot_verification: false,
            // On by default since the interpreter round i4 wave-88 triage (`=0` off).
            define_no_class_path_code_source: true,
            // On by default since the interpreter round i4 wave-94 triage (`=0` off).
            define_stub_probe_by_name: true,
            // WARNING(i6-user-flip): CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL default ON by user decision 2026-10-19; =0 restores the old path
            define_builtin_constraint_refusal: true,
            vm_fallback_threads: std::collections::HashSet::new(),
            define_builtin_constraint_fallback_pin: true,
            vm_fallback_classes: FxHashSet::default(),
            // JIT round 21 wave 17 (lane finalize-reinit): on by default (`=0` off).
            finalizer_empty_is_none: true,
            // JIT round 21 wave 22, lane hidden-resid: on by default (`=0` off).
            stub_upgrade_finalizer_to_subclasses: true,
            stub_upgrade_finalizer_flips: Vec::new(),
            type_maps_pending: FxHashMap::default(),
            user_pass3_pending: FxHashSet::default(),
            hidden_name_counter: 0,
            redefine_generations: RwLock::new(FxHashMap::with_capacity_and_hasher(
                8,
                Default::default(),
            )),
            hidden_classes: HiddenClassBits::new(),
            class_init_bits: ClassInitBits::new(),
            redefinition_histories: FxHashMap::default(),
            redefinition_log: RedefinitionLog::new_shared(),
            user_loaders: FxHashSet::with_capacity_and_hasher(4, Default::default()),
            init_states: parking_lot::RwLock::new(FxHashMap::with_capacity_and_hasher(
                256,
                Default::default(),
            )),
            // Permissive by default. Strict mode is a launcher decision
            // (`--real-jdk`), never inferred from a build feature or a stray
            // env var — see contract §6.
            compatibility_mode: CompatibilityMode::Compatible,
            hotspot_alignments: cratonvm_types::HotSpotAlignments::defaults(),
            module_path_roots: FxHashMap::default(),
            jfr_typed_event_configuration: false,
            preview_enabled: false,
            finalization_enabled: true,
            empty_finalizer_not_finalizable: true,
            alignment_census: std::sync::Arc::new(cratonvm_types::AlignmentCensus::new()),
            // `-Xverify:remote`, HotSpot's default. `vm_init` raises it to
            // `all` when the launcher was given `-Xverify:all`.
            strict_verification: false,
            // `vm_init` sets it under `-Xverify:none` / `-noverify`.
            skip_remote_verification: false,
            // `vm_init` sets it under `-Xverify:none` / `-noverify`.
            noverify_relaxes_format_checks: false,
            noverify_relaxes_structural_checks: false,
            verify_all_checks_boot_format: true,
            origin_violations: Vec::new(),
            origin_violations_seen: FxHashSet::default(),
            origin_requesters: FxHashMap::default(),
            violation_sink: None,
            jdk_only_refusals: FxHashMap::default(),
            // Unbound: `ClassManager::new` runs before the owning `SharedVm`
            // exists, so it has no `vm_identity` to record yet. `vm_init` calls
            // `bind_vm_id` as soon as it does.
            metadata_realm: MetadataRealm::new(),
            pending_transformed_classes: FxHashMap::default(),
            pending_native_shadow_cedes: FxHashSet::default(),
            app_class_path_defines: (0, 0),
            class_trace: None,
            sys_class_path_bytes: 0,
            unloaded_class_bytes: 0,
            app_workarounds: Vec::new(),
            app_stub_layouts: FxHashMap::default(),
        }
    }

    /// Parse a `module-info.class` byte array and register the contained
    /// module descriptor in `registry`.  Silently ignores parse failures.
    pub(in crate::class_manager) fn try_register_module_info(registry: &mut ModuleRegistry, bytes: &[u8], automatic: bool) {
        let diag_mp = loader_flags().dbg_modprov;
        let mut class_file = match cratonvm_reader::read_class(bytes) {
            Ok(cf) => cf,
            Err(e) => {
                debug!("Failed to parse module-info.class: {e}");
                return;
            }
        };

        // Eagerly decode the (small) module-info attribute table once so the
        // `Attribute`-typed helpers below (`descriptor_from_module_attribute`,
        // `packages_from_module_packages_attribute`) can pattern-match. The
        // module-info attribute list is tiny — one `Module` + maybe a
        // `ModulePackages` + `SourceFile` — so eager decode is the safest
        // option (and matches the prior behaviour from before lazy storage).
        if let Err(e) = force_decode_all(&mut class_file.attributes, &class_file.constant_pool) {
            debug!("Failed to decode module-info attributes: {e}");
            return;
        }

        // Find the Module attribute
        let module_desc = class_file.attributes.iter().find_map(|a| {
            a.as_decoded()
                .and_then(|d| descriptor_from_module_attribute(d, &class_file.constant_pool))
        });

        let mut desc = match module_desc {
            Some(d) => d,
            None => return,
        };
        // A `module-info.class` found on the application class path describes a
        // jar the real JDK would treat as part of the unnamed module. Give it
        // automatic-module access semantics (read/export/open all) so classpath
        // apps are not subjected to JPMS encapsulation between their own jars
        // (e.g. org.jboss.logging → org.apache.logging.log4j).
        desc.automatic = automatic;

        // Find ModulePackages attribute for package-to-module mapping
        let packages: Vec<String> = class_file
            .attributes
            .iter()
            .find_map(|a| {
                a.as_decoded().and_then(|d| {
                    packages_from_module_packages_attribute(d, &class_file.constant_pool)
                })
            })
            .unwrap_or_default();

        debug!(
            module = %desc.name,
            packages = packages.len(),
            requires = desc.requires.len(),
            exports = desc.exports.len(),
            "Registered module"
        );

        let recorded =
            crate::module::module_hashes_names(&class_file.attributes, &class_file.constant_pool);
        let module_name = desc.name.clone();
        registry.register(desc, packages);
        if let Some(names) = recorded {
            registry.set_recorded_hash_names(&module_name, names);
        }
    }

    // ---------------------------------------------------------------------
    // Real-JDK mode: policy, provenance and the census (contract §5)
    // ---------------------------------------------------------------------

    /// Install the compatibility policy for this manager.
    ///
    /// Call once at VM init, **before** any class is loaded — `vm_init`
    /// propagates `VmConfig::compatibility_mode` here and into the native
    /// registry in the same pass. Flipping it mid-run is supported (the test
    /// suite does it to isolate a delta) but only affects classes fabricated
    /// after the call; classes already in the store keep the origin they were
    /// created with.
    pub fn set_compatibility_mode(&mut self, mode: CompatibilityMode) {
        self.compatibility_mode = mode;
        // JVMS §5.4.5 overriding by run-time package (defining loader AND
        // name) in both modes: the owner turned it on for `--compatible` too
        // (2026-09-27). `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0` puts either
        // mode back on the name-only rule.
        let _ = mode;
        let by_loader =
            true;
        self.class_store.set_override_packages_by_loader(by_loader);
    }

    /// The compatibility policy in force for this manager.
    pub fn compatibility_mode(&self) -> CompatibilityMode {
        self.compatibility_mode
    }

    /// Install the VM's `--compatible` HotSpot-alignment rows (the native
    /// Instrument boot-loader JFR events against the typed `EventConfiguration` field
    /// (JDK 17/21's `EventInstrumentation`), not JDK 25's `Object`-typed one.
    pub fn set_jfr_typed_event_configuration(&mut self, typed: bool) {
        self.jfr_typed_event_configuration = typed;
    }

    /// Set this VM's `--enable-preview` bit (see the field).
    pub fn set_preview_enabled(&mut self, enabled: bool) {
        self.preview_enabled = enabled;
    }

    /// This VM's `--enable-preview` bit (see the field).
    pub fn preview_enabled(&self) -> bool {
        self.preview_enabled
    }

    /// Set this VM's `--finalization=` bit (see the field). Called by
    /// `vm_init` before the first class is defined.
    pub fn set_finalization_enabled(&mut self, enabled: bool) {
        self.finalization_enabled = enabled;
    }

    /// This VM's `--finalization=` bit (see the field).
    pub fn finalization_enabled(&self) -> bool {
        self.finalization_enabled
    }

    /// `Class::has_finalizer` for a class being defined, HotSpot 25's
    /// `ClassFileParser` rule (zt t18/hunt2): never while finalization is
    /// disabled (`--finalization=disabled`); otherwise the class declares a
    /// `finalize()V` or its superclass has a finalizer -- unless the class's own
    /// `finalize()V` is the empty `return` body, which HotSpot records as
    /// `_has_empty_finalizer` and then leaves `has_finalizer` unset
    /// (`CRATONVM_EMPTY_FINALIZER_NOT_FINALIZABLE`, default ON; `=0` counts an
    /// empty body as a finalizer, as before).
    ///
    /// dev merge (zt round x JIT round 21 wave 17): dev implemented the same
    /// empty-finalizer rule as [`class_has_finalizer_by_hotspot_rule`] under
    /// `CRATONVM_FINALIZER_EMPTY_IS_NONE`. Both doors now share that one rule;
    /// either kill switch (`=0`) restores the old "any declaration" answer
    /// ([`Self::empty_finalizer_is_none`]).
    pub(in crate::class_manager) fn computed_has_finalizer(&self, class: &Class, parent_has: bool) -> bool {
        self.finalization_enabled
            && class_has_finalizer_by_hotspot_rule(
                class,
                parent_has,
                self.empty_finalizer_is_none(),
            )
    }

    /// Is an empty `finalize()V` no finalizer? Both rounds' switches must be
    /// on: `CRATONVM_EMPTY_FINALIZER_NOT_FINALIZABLE` (zt t18/hunt2) and
    /// `CRATONVM_FINALIZER_EMPTY_IS_NONE` (JIT round 21 wave 17), each default
    /// ON with `=0` as its kill.
    #[inline]
    pub(in crate::class_manager) fn empty_finalizer_is_none(&self) -> bool {
        self.empty_finalizer_not_finalizable && self.finalizer_empty_is_none
    }

    /// This VM's user-loader delegation parents. The VM hands the same `Arc`
    /// to its loader-identity tables (`SharedVm::new`), where the natives that
    /// see a loader's Java `parent` field record the links without taking the
    /// class-manager lock. Interpreter round i5 wave 103, lane L5.
    pub fn user_loader_parents(&self) -> &Arc<crate::loaders::UserLoaderParents> {
        &self.user_loader_parents
    }

    /// Serve this VM's multi-release jars (boot, extension and application
    /// class paths, the last including its `add_path` jars) at the booted
    /// class library's feature version (`ClassPath::set_multi_release_feature`;
    /// `vm_init`, from the image's `release` file, before the first class is
    /// loaded). Interpreter round i5 wave 100, lane L5
    /// (`i100-L5-multi-release-jars-read-at-version-25-whatever-jdk-boots`).
    pub fn set_multi_release_feature(&self, feature: u32) {
        self.bootstrap.class_path().set_multi_release_feature(feature);
        self.extension.class_path().set_multi_release_feature(feature);
        self.application.class_path().set_multi_release_feature(feature);
    }

    /// registry's set; `vm_init`, with [`Self::set_compatibility_mode`]).
    pub fn set_hotspot_alignments(&mut self, set: cratonvm_types::HotSpotAlignments) {
        self.hotspot_alignments = set;
    }

    /// Record resolved `--module-path` module `module`'s roots on the
    /// application class path (patch paths first, then the module root).
    /// `vm_init`, once per resolved module.
    pub fn set_module_path_roots(&mut self, module: &str, roots: Vec<std::path::PathBuf>) {
        self.module_path_roots.insert(module.to_string(), roots);
    }

    /// Install the VM's application-workaround table (from `NativeMethodRegistry`).
    pub fn set_app_workarounds(&mut self, entries: Vec<cratonvm_native_api::AppWorkaroundEntry>) {
        self.app_workarounds = entries;
    }

    /// Install third-party synthetic stub field layouts (from `NativeMethodRegistry`).
    pub fn set_app_stub_layouts(
        &mut self,
        layouts: FxHashMap<&'static str, &'static [cratonvm_native_api::SyntheticStubField]>,
    ) {
        self.app_stub_layouts = layouts;
    }

    /// Resolve synthetic stub fields for `name`, checking registered third-party
    /// layouts and stub-layout aliases before `synthetic_stub_fields`.
    pub(in crate::class_manager) fn synthetic_stub_fields_for(
        &self,
        name: &str,
    ) -> Vec<cratonvm_reader::field::ClassFileField> {
        if let Some(fields) = self.app_stub_layouts.get(name) {
            return fields
                .iter()
                .map(|f| cratonvm_reader::field::ClassFileField {
                    access_flags: if f.is_public_static_final {
                        FieldAccessFlags::PUBLIC
                            | FieldAccessFlags::STATIC
                            | FieldAccessFlags::FINAL
                    } else {
                        FieldAccessFlags::empty()
                    },
                    name: cratonvm_types::intern_arc(f.name),
                    descriptor: cratonvm_types::intern_arc(f.descriptor),
                    attributes: vec![],
                })
                .collect();
        }
        if let Some(alias) = self.app_workarounds.iter().find(|e| {
            e.kind == cratonvm_native_api::AppWorkaroundKind::StubLayoutAlias
                && e.class_pattern == name
        }) {
            return synthetic_stub_fields(alias.aux);
        }
        synthetic_stub_fields(name)
    }

    /// Read the installed application-workaround table.
    #[inline]
    pub fn app_workarounds(&self) -> &[cratonvm_native_api::AppWorkaroundEntry] {
        &self.app_workarounds
    }

    /// Whether `name` matches an `EnterpriseStubPackagePrefix` workaround.
    #[inline]
    pub(in crate::class_manager) fn is_enterprise_stub_prefix(&self, name: &str) -> bool {
        self.app_workarounds.iter().any(|e| {
            e.kind == cratonvm_native_api::AppWorkaroundKind::EnterpriseStubPackagePrefix
                && name.starts_with(e.class_pattern)
        })
    }

    /// Whether `name` is a JDK class, array, or enterprise stub prefix.
    #[inline]
    fn is_jdk_or_enterprise_stub(&self, name: &str) -> bool {
        is_jdk_class(name) || self.is_enterprise_stub_prefix(name)
    }

    /// HotSpot's `ModuleReader.find(name)` URL of resolved `--module-path`
    /// module `module`: the first of its roots (patch paths, then the module
    /// root) that holds `name`, as the application class path spells that
    /// entry's URL. `None` when `module` is not a module-path module,
    /// `Some(None)` when no root of it holds `name`; never another module's
    /// copy of the name.
    ///
    /// Interpreter round i4 wave 98, lane L3
    /// (`interpreter-i95-L3-compatible-builtin-loader-find-resource-by-module-is-null-FIXED-20261130.md`):
    /// `--compatible` fills no `BuiltinClassLoader.nameToModule`, so
    /// `Class.getResource` of a named module-path module's class answered
    /// null.
    pub fn module_path_resource_url(&self, module: &str, name: &str) -> Option<Option<String>> {
        let roots = self.module_path_roots.get(module)?;
        let app = self.application.class_path();
        Some(
            roots
                .iter()
                .find_map(|root| app.resource_urls_in_root(root, name).into_iter().next()),
        )
    }

    /// The bytes sibling of [`Self::module_path_resource_url`], for
    /// `findResourceAsStream(mn, name)`.
    pub fn module_path_resource_bytes(&self, module: &str, name: &str) -> Option<Option<Vec<u8>>> {
        let roots = self.module_path_roots.get(module)?;
        let app = self.application.class_path();
        Some(
            roots
                .iter()
                .find_map(|root| app.resource_bytes_in_root(root, name)),
        )
    }

    /// Does this manager behave as HotSpot for `a`: `--real-jdk`, or
    /// `--compatible` with the row set (`cratonvm_types::hotspot_alignment`).
    #[inline]
    pub fn aligned(&self, a: cratonvm_types::HotSpotAlignment) -> bool {
        self.hotspot_alignments.aligned(self.compatibility_mode, a)
    }

    /// Share the VM's alignment census (`vm_init`, with
    /// [`Self::set_hotspot_alignments`]).
    pub fn set_alignment_census(
        &mut self,
        census: std::sync::Arc<cratonvm_types::AlignmentCensus>,
    ) {
        self.alignment_census = census;
    }

    /// Count one answer of row `a` that differs from its row-off route's
    /// (`cratonvm_types::AlignmentCensus`). Cold.
    #[cold]
    pub fn note_alignment_divergence(&self, a: cratonvm_types::HotSpotAlignment) {
        self.alignment_census.note(a);
    }

    /// Install `-Xverify:all` — verify EVERY class strictly, including the
    /// bootstrap image and classes from user-defined loaders.
    ///
    /// Call once at VM init before any class is loaded, exactly like
    /// [`Self::set_compatibility_mode`]; `vm_init` propagates
    /// `VmConfig::xverify_mode` here. Per-manager rather than process-global so
    /// two VMs in one process can run under different verification policies.
    ///
    /// Three things change when this is on, all of them "stop taking a
    /// shortcut":
    ///
    /// * `class_is_bootstrap_trusted` stops earning the lenient branch-target
    ///   path, so the JDK image is checked against the spec-literal rule
    ///   (`bytecode_verifier::verify_bytecode_strict`, which until now had no
    ///   production caller at all despite being documented as this flag's entry
    ///   point);
    /// * `defer_loader_sensitive_pass3` stops deferring the Pass-3 type-state
    ///   verdict for user-loader classes — i.e. for every Spring / Tomcat / H2
    ///   application class;
    /// * `vm_util::verifier_skip_eligible` stops skipping link-time Pass 2 for
    ///   bootstrap classes.
    pub fn set_strict_verification(&mut self, strict: bool) {
        self.strict_verification = strict;
    }

    /// Whether `-Xverify:all` is in force for this manager.
    pub fn strict_verification(&self) -> bool {
        self.strict_verification
    }

    /// Install `-Xverify:none` / `-noverify` for the DEFINE-time verifier
    /// (t1 wave 9, lane I24V; `vm_init` passes `VmConfig::skip_verification`
    /// under `CRATONVM_NOVERIFY_SKIPS_DEFINE_PASS3`, default ON, `=0` kill).
    ///
    /// HotSpot 25 still honours `-Xverify:none` (with a deprecation warning):
    /// `BytecodeVerificationRemote` goes off, so no class of a non-boot loader
    /// gets the type-state verifier. The link step here already skipped it
    /// (`vm_util`, `shared.config.skip_verification`), but the define of an
    /// application- or platform-loader class (the class path, and
    /// `Lookup.defineClass` / `ClassLoader.defineClass` into those loaders) ran
    /// the full Pass 3 regardless, so unverifiable-but-runnable bytecode raised
    /// `VerifyError` where HotSpot runs it (probe
    /// `T1W8I24FinalizableCtorRetry.java`). With this set, such a
    /// define keeps Pass 2 and the structural bytecode scan (the decoded
    /// interpreter relies on its bounds) and drops only the type-state verdict;
    /// the type maps are harvested as for a deferred user-loader class.
    /// Bootstrap classes, redefinitions and `-Xverify:all` are untouched.
    pub fn set_skip_remote_verification(&mut self, skip: bool) {
        self.skip_remote_verification = skip;
    }

    /// Whether [`Self::set_skip_remote_verification`] is in force.
    pub fn skip_remote_verification(&self) -> bool {
        self.skip_remote_verification
    }

    /// Install `-Xverify:none` / `-noverify` for the define-time FORMAT
    /// legality checks (t1 wave 10, lane I24W; `vm_init` passes
    /// `VmConfig::skip_verification` under
    /// `CRATONVM_NOVERIFY_RELAXES_FORMAT_CHECKS`, default ON, `=0` kill).
    ///
    /// HotSpot's `ClassFileParser` sets `_need_verify` from
    /// `Verifier::should_verify_for`, which reads `BytecodeVerificationRemote`
    /// for every non-boot loader; `-Xverify:none` turns that off (and
    /// `relax_format_check_for` answers "relax"), so
    /// `verify_legal_class_modifiers`, `verify_legal_field_modifiers`,
    /// `verify_legal_method_modifiers` and `verify_legal_field_name` /
    /// `verify_legal_method_name` all return at once. With this set, a define
    /// by a non-boot loader skips exactly those: the class, field and method
    /// modifier legality, and the field / method name legality (declarations,
    /// constant-pool member references and local variable tables). The
    /// structural `_need_verify` checks are relaxed separately, by
    /// [`Self::set_noverify_relaxes_structural_checks`] (t1 wave 13; see
    /// `docs/feature-designs/class-file-parser-hardening.md`).
    /// The checks HotSpot makes in every loader (`ACC_MODULE`, a non-static
    /// `<clinit>`, an interface `<init>`, an interface's superclass) are
    /// unaffected. `-Xverify:all` wins.
    pub fn set_noverify_relaxes_format_checks(&mut self, relax: bool) {
        self.noverify_relaxes_format_checks = relax;
    }

    /// Whether [`Self::set_noverify_relaxes_format_checks`] is in force.
    pub fn noverify_relaxes_format_checks(&self) -> bool {
        self.noverify_relaxes_format_checks
    }

    /// Install `-Xverify:none` / `-noverify` for the STRUCTURAL define-time
    /// format checks as well (t1 wave 13, lane I24W; `vm_init` passes
    /// `VmConfig::skip_verification` under
    /// `CRATONVM_NOVERIFY_RELAXES_STRUCTURAL_CHECKS`, default ON, `=0` kill).
    /// Needs [`Self::set_noverify_relaxes_format_checks`]; `-Xverify:all` wins.
    ///
    /// HotSpot's `_need_verify` also gates most of `ClassFileParser`'s
    /// structural checks. Measured on HotSpot 25.0.4.1 (probe
    /// `T1W13I24WStructuralFormat.java`, 48 rows), a non-boot
    /// define then also skips the class-name legality, an array `this_class`,
    /// the constant pool's `Fieldref` / `Methodref` / `NameAndType` index checks
    /// and its whole format pass, duplicate interfaces / fields / methods, the
    /// method descriptors and the field descriptors that still name a layout
    /// type, the `ConstantValue` type, `Arguments can't fit into locals`, every
    /// `LocalVariableTable` / `LocalVariableTypeTable` rule, the `Code`
    /// presence rules, and the `InnerClasses` both-outer-and-inner, duplicate
    /// and circularity verdicts (the table is kept as written). With this set
    /// the define skips the same. KEPT, as HotSpot keeps them: the
    /// `MethodHandle` / `MethodType` / `Dynamic` index checks, `ACC_MODULE` and
    /// module constants, the `LineNumberTable` pc, `Exceptions`, every
    /// repeated-attribute rule, the class attributes and the `InnerClasses`
    /// index types; also kept, because HotSpot crashes without them: a field
    /// descriptor with no layout type (`Q`) and an `Exceptions` entry that is
    /// not a class.
    ///
    /// The consumers tolerate the relaxed shapes: members are kept in
    /// declaration order and looked up by first match (a duplicate is
    /// unreachable by name, as on HotSpot), descriptors are parsed on use and
    /// refuse there, the debugger's variable tables are bounds-checked, and the
    /// `InnerClasses` readers make one step at a time.
    pub fn set_noverify_relaxes_structural_checks(&mut self, relax: bool) {
        self.noverify_relaxes_structural_checks = relax;
    }

    /// Install whether `-Xverify:all` extends the define-time format checks
    /// to the boot and platform loaders (t1 wave 13, lane I24W;
    /// `CRATONVM_VERIFY_ALL_CHECKS_BOOT_FORMAT`, default ON, `=0` kill).
    /// HotSpot's `-Xverify:all` sets `BytecodeVerificationLocal`, so
    /// `_need_verify` holds for boot classes and `relax_format_check_for`
    /// relaxes no loader: a malformed class on `-Xbootclasspath/a:` is refused.
    pub fn set_verify_all_checks_boot_format(&mut self, on: bool) {
        self.verify_all_checks_boot_format = on;
    }

    /// `(legality, structural)`: does a define relax the `-Xverify:none`
    /// modifier / member-name legality checks, and the structural ones?
    /// `-Xverify:all` relaxes neither.
    pub fn noverify_relaxation(&self) -> (bool, bool) {
        let legality = self.noverify_relaxes_format_checks && !self.strict_verification;
        (legality, legality && self.noverify_relaxes_structural_checks)
    }

    /// The `--dump-class-origins` census: one row per class currently in the
    /// store.
    ///
    /// Rows are emitted in `ClassId` order, which is load order, so two dumps
    /// of an unchanged manager are byte-identical — the census is diffed
    /// between runs and between OS legs in CI, and a non-deterministic order
    /// would make that gate flap.
    pub fn dump_class_origins(&self) -> Vec<ClassOriginEntry> {
        self.class_store
            .iter()
            .map(|c| ClassOriginEntry {
                // Superclass first, then declared interfaces, each resolved
                // back to a name through the same store. An id that does not
                // resolve is dropped: it can only mean the row was taken
                // mid-definition, and a census that guesses is worse than one
                // that is short.
                supertypes: c
                    .superclass
                    .into_iter()
                    .chain(c.interfaces.iter().copied())
                    .filter_map(|id| self.class_store.get(id).map(|s| s.name.to_string()))
                    .collect(),
                name: c.name.to_string(),
                origin: c.origin.as_str().to_string(),
                reason: c.origin.reason().map(|r| r.to_string()),
                // Two provenances, in one string, in the order they become
                // known:
                //
                // * the **Rust call site**, from `#[track_caller]` on the
                //   `ensure_*` → `fabricate_class` →
                //   `admit_compatibility_class` chain. Always available, and
                //   for a `--real-jdk` boot it is the answer for nearly the
                //   whole population: compatibility classes come overwhelmingly
                //   from natives asking for an allocation shape, not from
                //   constant-pool resolution.
                // * the **Java frame** — the method that ran the `new` /
                //   `checkcast` / `Class.forName` — which `ClassManager` cannot
                //   see (it is called with a bare name) and which the VM's
                //   class-load choke point supplies afterwards through
                //   [`Self::attach_origin_requester`]. Rendered as
                //   `"owner/Class.method(Desc) via <rust site>"`.
                //
                // `None` here means the class produced no violation at all —
                // i.e. nothing to attribute — which is the common case and the
                // whole point of a census: 400-odd honest rows and a handful
                // that name who is responsible.
                requested_by: self.origin_requesters.get(&*c.name).cloned(),
                real_bytes_found: c.origin.has_real_bytes(),
                loader_id: c.loader_id.to_native_id(),
            })
            .collect()
    }

    /// Class-origin violations recorded this run, in first-observation order.
    ///
    /// Non-empty in `Compatible` mode too: wave 1 is measurement, not deletion
    /// (contract §10), so a permissive run still reports everything a
    /// `--real-jdk` run would have refused.
    pub fn origin_violations(&self) -> &[JdkOnlyViolation] {
        &self.origin_violations
    }

    /// How many class-origin violations have been recorded so far.
    ///
    /// A `Vec::len`, taken before a load so
    /// [`Self::attach_origin_requester`] can tell whether that load recorded
    /// anything. Keeping this separate from `origin_violations()` means the
    /// caller does not borrow the whole slice just to compare a length.
    pub fn origin_violation_count(&self) -> usize {
        self.origin_violations.len()
    }

    /// Name the Java frame responsible for every violation recorded at or
    /// after `from`.
    ///
    /// Called by the VM's class-load choke point with the count it took
    /// immediately before the load. **Returns without allocating when the load
    /// recorded nothing**, which is every load but the handful that fabricate
    /// — that is why the requester is passed as three borrowed `&str`s rather
    /// than a formatted `String`: the common path must not pay for a format
    /// that is then thrown away.
    ///
    /// A violation that already names a requester is left alone, so the
    /// *first* frame to demand a class keeps the attribution even if a nested
    /// load re-enters. Note that `admit_compatibility_class` dedupes by class
    /// name, so only the first requester of a given class is ever recorded at
    /// all; "which call sites depend on this fabrication" needs a different
    /// instrument.
    pub fn attach_origin_requester(
        &mut self,
        from: usize,
        owner: &str,
        method: &str,
        descriptor: &str,
    ) {
        if from >= self.origin_violations.len() {
            return;
        }
        for violation in &mut self.origin_violations[from..] {
            if let JdkOnlyViolation::CompatibilityClassRequested {
                class,
                requester: slot,
                ..
            } = violation
            {
                // Layered on top of the Rust call site
                // `admit_compatibility_class` already recorded, giving
                // `"org/foo/Bar.baz(Desc) via classloading/…:7289"`. Both
                // halves answer different questions and both are worth
                // keeping: the Java frame says which application code depends
                // on the fabrication, the Rust site says which VM code
                // performed it — and the latter is the work list the
                // `ensure_synthetic_class` migration needs.
                let combined = match slot.as_deref() {
                    Some(site) => format!("{owner}.{method}{descriptor} via {site}"),
                    None => format!("{owner}.{method}{descriptor}"),
                };
                self.origin_requesters
                    .insert(class.clone(), combined.clone());
                *slot = Some(combined);
            }
        }
    }

    /// Install a live sink for class-origin violations (contract §9's
    /// *"log every violation as it happens"*).
    ///
    /// Call once, right after `Vm::new`, before the launcher's first drain.
    /// Violations recorded during `Vm::new` itself predate the sink and are
    /// still picked up by that first drain; everything after it arrives live.
    ///
    /// Per VM, never process-global (contract §2).
    pub fn set_violation_sink(&mut self, sink: Arc<dyn Fn(&JdkOnlyViolation) + Send + Sync>) {
        self.violation_sink = Some(sink);
    }

    /// Whether a live sink is installed.
    ///
    /// The launcher's periodic drain uses this to advance its watermark
    /// *without* re-printing: with a sink installed, every violation past the
    /// first drain has already been reported at the instant it happened, and
    /// printing it again at shutdown would double-report it.
    pub fn has_violation_sink(&self) -> bool {
        self.violation_sink.is_some()
    }

    /// Decide whether a compatibility class may be fabricated, recording the
    /// request either way.
    ///
    /// This is the single policy predicate for class fabrication. It is called
    /// from exactly two choke points — [`Self::create_synthetic_stub`] (which
    /// covers the whole `load_class` fabrication chain) and
    /// [`Self::fabricate_class`] (which covers the `ensure_*_synthetic_class`
    /// family) — so there is no third place a stub can be minted without the
    /// policy seeing it.
    ///
    /// Recording happens in **both** modes; only `JdkOnly` returns `Err`.
    ///
    /// The error is `ClassNotFoundException` rather than a bespoke
    /// `VmError::JdkOnly`: the three sibling branches of the same `load_class`
    /// match arm (`$$`-generated names, `package-info`, the reflective-probe
    /// gate) already return exactly this, so every downstream caller is
    /// written to handle it, and constant-pool resolution converts it to
    /// `NoClassDefFoundError` unchanged. The structured
    /// [`JdkOnlyViolation`] carries the detail an operator needs; the thrown
    /// exception carries what the *program* needs.
    ///
    /// # Provenance
    ///
    /// `#[track_caller]`, threaded through `fabricate_class` and the three
    /// `ensure_*` entry points, so the recorded `requester` names the **Rust
    /// call site** that asked for the fabrication.
    ///
    /// That is not a consolation prize for the missing Java frame — it is the
    /// answer for most of the population. A `--real-jdk` boot's compatibility
    /// classes come overwhelmingly from natives calling `ensure_synthetic_class`
    /// directly for an allocation shape, not from constant-pool resolution, and
    /// those have a Rust caller and no Java frame at all. The Java frame, when
    /// one exists, is added on top by [`Self::attach_origin_requester`].
    ///
    /// Costs one `format!` per *distinct class name*, inside the dedupe branch —
    /// not per request, which is thousands of times more often.
    #[track_caller]
    pub(in crate::class_manager) fn admit_compatibility_class(&mut self, name: &str, reason: &str) -> Result<(), VmError> {
        // Deduped by class name: a single missing class is requested over and
        // over (constant-pool resolution, `Class.forName` probes, every
        // allocation of a stub-backed container), and an undeduped list would
        // be thousands of rows of a handful of names.
        if self.origin_violations_seen.insert(name.to_string()) {
            let site = core::panic::Location::caller();
            self.origin_requesters
                .insert(name.to_string(), format!("{}:{}", site.file(), site.line()));
            self.origin_violations
                .push(JdkOnlyViolation::CompatibilityClassRequested {
                    class: name.to_string(),
                    // Every fabrication path in this file hard-codes
                    // `ClassLoaderId::Bootstrap` for the class it mints (and
                    // debug-asserts it), so the initiating loader is not in
                    // question here.
                    initiating_loader: Some("bootstrap".to_string()),
                    // The Rust call site. A Java frame, when there is one, is
                    // added on top by `attach_origin_requester` once the load
                    // unwinds to the VM choke point that can see it.
                    requester: Some(format!("{}:{}", site.file(), site.line())),
                    reason: reason.to_string(),
                    // Constant by construction: this row fires while a
                    // `ClassOrigin::CompatibilityStub` is being minted, so the
                    // origin is that tag and `reason` above is the part that
                    // varies. Spelled out rather than left for the reader to
                    // infer from the kind — §1.7's class-origin field is a
                    // column, and a column a consumer has to derive from
                    // another column is not one.
                    class_origin: Some(
                        crate::class_origin::ClassOrigin::compatibility_stub(reason)
                            .as_str()
                            .to_string(),
                    ),
                });
            // Live trace (contract §9). Dispatched inside the dedupe branch so
            // the sink sees exactly the violations the report will list — one
            // per distinct class — and not the thousands of repeat requests
            // the dedupe exists to swallow.
            //
            // The requester is not attached yet: it arrives a moment later,
            // from the choke point above this call, once the load unwinds back
            // to a frame. A live line therefore names the class and the reason
            // but not the caller; the shutdown census has both. Reporting late
            // enough to have the requester would defeat the point of the sink.
            if let Some(sink) = self.violation_sink.clone() {
                if let Some(violation) = self.origin_violations.last() {
                    sink(violation);
                }
            }
            if self.compatibility_mode.is_jdk_only() {
                debug!(
                    class = name,
                    reason, "--real-jdk: refusing to fabricate a compatibility class"
                );
            }
        }
        if self.compatibility_mode.is_jdk_only() {
            return Err(VmError::ClassFile(ClassFileError::ClassNotFound {
                class_name: name.to_string(),
            }));
        }
        Ok(())
    }

    /// Classify a class being defined from **real bytes** by
    /// [`Self::define_class_shared_with_options`].
    ///
    /// Precedence is most-specific-first:
    ///
    /// 1. `options.origin` — an explicit statement from a generator that knows
    ///    why it is emitting these bytes always beats a heuristic.
    /// 2. The generated-name families. A `$$Lambda` / `$ProxyN` /
    ///    `Generated*Accessor*` is reported as what generated it even when it
    ///    is also a hidden class, because "hidden" is a *mechanism* and the
    ///    census wants the *producer*: `--dump-class-origins` has to be able to
    ///    separate lambda spinning from proxy generation from reflection
    ///    accessor emission, and collapsing all three into `hidden-class` would
    ///    make that impossible.
    /// 3. `options.hidden` — a hidden class from some other generator.
    /// 4. The defining loader.
    ///
    /// None of these can be a [`ClassOrigin::CompatibilityStub`]: by the time
    /// this runs, real class bytes have been parsed. That is the invariant that
    /// makes the census meaningful — a `compatibility-stub` row can only have
    /// come from one of the two fabrication paths.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::class_manager) fn classify_defined_origin(
        &self,
        stored_name: &str,
        loader_id: ClassLoaderId,
        options: &DefineClassOptions,
        module_name: Option<&str>,
        code_source_url: Option<&str>,
        nest_host: Option<&str>,
        interface_ids: &[ClassId],
    ) -> ClassOrigin {
        if let Some(origin) = options.origin.clone() {
            return origin;
        }

        // The nest host is the closest thing to a "who generated me" pointer
        // the class file carries for these types: `LambdaMetafactory` and the
        // reflection accessor emitters make the generated class a nestmate of
        // the class whose behaviour it implements.
        let host = nest_host.and_then(|h| self.get_loaded_class_id(h));

        if is_generated_lambda_name(stored_name) {
            return ClassOrigin::GeneratedLambda { host };
        }
        if is_generated_proxy_name(stored_name) {
            // The interface set is the proxy's whole identity — a `$ProxyN`
            // exists only to implement it — so the census records the resolved
            // `ClassId`s rather than a name list.
            return ClassOrigin::GeneratedProxy {
                interfaces: Arc::from(interface_ids.to_vec()),
            };
        }
        if is_reflection_accessor_name(stored_name) {
            return ClassOrigin::ReflectionAccessor { host };
        }
        if options.hidden {
            return ClassOrigin::HiddenClass { host };
        }

        // `CodeSource::url` is the classpath entry / jar URL these bytes came
        // from. A runtime-defined class has the synthesised
        // `file:/runtime-defined/<name>.class` URL, which is still the most
        // useful thing to print; only a class with no code source at all falls
        // back to its own name.
        let source: Arc<str> = code_source_url
            .map(Arc::<str>::from)
            .unwrap_or_else(|| Arc::<str>::from(stored_name));
        match loader_id {
            ClassLoaderId::Bootstrap => ClassOrigin::BootImage {
                // The JPMS attribution already computed above (module-info
                // package map, falling back to the class file's own `Module`
                // attribute) — not a second, divergent guess.
                module: module_name.map(Arc::<str>::from),
                source,
            },
            // The extension finder is part of the application-visible class
            // path from the census's point of view: neither is the boot image,
            // and both mean "real bytes found by a built-in finder".
            ClassLoaderId::Extension | ClassLoaderId::Application => {
                ClassOrigin::ApplicationClassPath { source }
            }
            ClassLoaderId::UserDefined(_) => ClassOrigin::UserDefined {
                loader: loader_id,
                source: Some(source),
            },
        }
    }

    /// Loader-aware class lookup by internal name.
    ///
    /// **Round 4 audit fix (CRIT):** previously this consulted a separate
    /// `name_to_id: FxHashMap<u64, ClassId>` keyed only by the raw FNV-1a
    /// digest of `name`. Two distinct class names that collide under
    /// FNV-1a (rare but realistic on adversarial input) returned the
    /// wrong `ClassId`, and the shadow map had no `ClassLoaderId`
    /// component so different loaders defining the same name silently
    /// stomped each other. The fix routes lookups through the
    /// authoritative `loaded_classes` map (keyed by
    /// `(ClassLoaderId, Arc<str>)`), which performs full name equality
    /// and is loader-aware. The three built-in loaders are probed in
    /// delegation order (Bootstrap → Extension → Application); custom
    /// loaders are then linearly scanned (rare path — only relevant once
    /// `URLClassLoader`-style user loaders are wired up).
    /// Loader-faithful variant of [`Self::get_loaded_class_id`]: resolves
    /// `name` as the given requesting loader would, mirroring
    /// `ClassStoreHierarchy::lookup`. A user-defined loader that defines its
    /// OWN copy of `name` (loader-aware gate on) resolves to that copy, not
    /// to a same-named class from the built-in chain or an unrelated loader.
    /// Use this whenever a requesting-class context exists; the bare
    /// name-only lookup returns an arbitrary copy when names collide across
    /// loaders.
    pub fn get_loaded_class_id_for_requester(
        &self,
        name: &str,
        requesting_loader: ClassLoaderId,
    ) -> Option<ClassId> {
        match requesting_loader {
            ClassLoaderId::Bootstrap | ClassLoaderId::Extension | ClassLoaderId::Application => {
                for loader_id in BUILTIN_LOADER_DELEGATION_CHAIN {
                    if let Some(id) = loaded_classes_probe(&self.loaded_classes, *loader_id, name) {
                        return Some(id);
                    }
                    // A built-in loader never delegates DOWN to its children.
                    if *loader_id == requesting_loader {
                        break;
                    }
                }
                None
            }
            ClassLoaderId::UserDefined(_) => {
                // An overriding user loader's own definition wins (JVMS
                // §5.4.3 initiating-loader semantics) — same order as
                // `ClassStoreHierarchy::lookup`.
                if loader_aware_resolution() {
                    if let Some(id) =
                        loaded_classes_probe(&self.loaded_classes, requesting_loader, name)
                    {
                        return Some(id);
                    }
                }
                // Same ordering as `loaded_class_for_requesting_loader`: a
                // registered delegation ancestor is closer than the built-in
                // chain and must be consulted before it.
                if let Some(id) =
                    loaded_class_via_parent_chain(
                        &self.loaded_classes,
                        &self.user_loader_parents,
                        requesting_loader,
                        name,
                    )
                {
                    return Some(id);
                }
                for loader_id in BUILTIN_LOADER_DELEGATION_CHAIN {
                    if let Some(id) = loaded_classes_probe(&self.loaded_classes, *loader_id, name) {
                        return Some(id);
                    }
                }
                loaded_classes_probe(&self.loaded_classes, requesting_loader, name)
            }
        }
    }

    pub fn get_loaded_class_id(&self, name: &str) -> Option<ClassId> {
        // C34 audit fix (HIGH): zero-allocation probe via
        // `loaded_classes_probe` (hashbrown `raw_entry`). Previously this
        // minted a fresh `Arc<str>` per probe (round-9 CRIT-2 carry) —
        // Spring cold start hits this >100k times, so the per-call
        // allocation was visible on `perf top`.
        //
        // Round 9 audit fix (HIGH #6): iterate over the canonical
        // `BUILTIN_LOADER_DELEGATION_CHAIN` constant rather than re-inlining
        // the (Bootstrap, Extension, Application) array.
        for loader_id in BUILTIN_LOADER_DELEGATION_CHAIN {
            if let Some(id) = loaded_classes_probe(&self.loaded_classes, *loader_id, name) {
                return Some(id);
            }
        }
        // Round 5 audit fix (HIGH): custom-loader fallback now probes the
        // small `user_loaders` set by exact key instead of linearly
        // walking every entry in `loaded_classes`. Same fix as
        // `find_class_by_name`. Empty-set early-out keeps the common
        // "no user loaders" case at zero extra work.
        //
        // context.groovy fix: `user_loaders` is an unordered `FxHashSet`, so
        // "return the first match" was really "return an ARBITRARY match" --
        // unsound whenever more than one user-defined loader has its OWN
        // distinct class registered under the same simple name. This is the
        // common case for Apache Groovy: `GroovyShell.evaluate` compiles each
        // script through a fresh `GroovyClassLoader$InnerLoader`, and every
        // closure literal in a script is named positionally
        // (`<Script>$_run_closure1`, `$_run_closure2`, ...), so two different
        // scripts loaded in the same process routinely produce two DIFFERENT
        // classes sharing the identical name. Blindly returning whichever
        // loader's copy the hash-set happened to visit first silently
        // collapsed every later script's closures onto the first script's
        // compiled bytecode (no exception -- the class "resolved" fine, just
        // to the wrong loader's copy), e.g. Spring's
        // `GroovyBeanDefinitionReader` registering zero of the beans the
        // current script actually declared. A name that is genuinely
        // ambiguous across loaders has no single correct answer here (this
        // function takes no loader/caller context to disambiguate with), so
        // when more than one user loader owns a same-named class we return
        // `None` -- ambiguous is a miss, not a guess -- letting the caller fall
        // through to a loader-specific resolution path (e.g.
        // `drive_defining_loader_load`) instead of silently picking one. The
        // common single-custom-loader case (Tomcat/Hibernate/WildFly: one
        // relevant webapp/session loader) is unaffected -- this only changes
        // behavior when 2+ user loaders actually collide on the same name.
        if !self.user_loaders.is_empty() {
            let mut found: Option<ClassId> = None;
            for loader_id in &self.user_loaders {
                if let Some(id) = loaded_classes_probe(&self.loaded_classes, *loader_id, name) {
                    if let Some(prev) = found {
                        if prev != id {
                            // Ambiguous: two different loaders each define
                            // their own distinct class under this name.
                            return None;
                        }
                    } else {
                        found = Some(id);
                    }
                }
            }
            if let Some(id) = found {
                return Some(id);
            }
        }
        None
    }

    /// True when a previous classpath scan for `name` failed and the
    /// classpath has not been extended since. Callers use this to skip the
    /// (expensive, full-classpath) synthetic-stub upgrade rescan. See
    /// [`Self::synthetic_upgrade_absent`].
    #[inline]
    pub(in crate::class_manager) fn synthetic_upgrade_known_absent(&self, name: &str) -> bool {
        self.synthetic_upgrade_absent.contains(name)
    }

    /// [`Self::synthetic_upgrade_known_absent`] for a caller outside this
    /// crate: has a classpath scan for `name` already failed, with no
    /// classpath extension since?
    ///
    /// For the VM's `NativeContext::try_ensure_synthetic_class`, which tries a
    /// real load before asking [`Self::try_ensure_synthetic_class`] and so
    /// re-scanned every classpath entry on every call. Under `--real-jdk` a
    /// refused name is never registered, so nothing else stops the rescan:
    /// netty's `Http2ConnectionRoundtripTest.stressTest` asked for
    /// `cratonvm/stream/LazyOp` 12,125 times and paid 1.22 million `statx`
    /// misses for it, each batch under the class manager's write lock. See
    /// `docs/feature-designs/class-resolution-and-caching.md`.
    #[inline]
    pub fn synthetic_class_bytes_known_absent(&self, name: &str) -> bool {
        self.synthetic_upgrade_known_absent(name)
    }

    /// Record that the real `.class` for `name` is absent from every current
    /// classpath, so future upgrade attempts can short-circuit. Bounded with
    /// clear-on-full so a pathological probe set (many distinct absent names)
    /// cannot grow the set without limit — losing the memo just reverts to
    /// the (correct, slower) rescan behaviour.
    pub(in crate::class_manager) fn note_synthetic_upgrade_absent(&mut self, name: &str) {
        const SYNTHETIC_ABSENT_CACHE_CAP: usize = 8192;
        if self.synthetic_upgrade_absent.len() >= SYNTHETIC_ABSENT_CACHE_CAP {
            self.synthetic_upgrade_absent.clear();
        }
        self.synthetic_upgrade_absent.insert(name.to_string());
    }

    /// Access flags for a freshly-minted synthetic stub class.
    ///
    /// Synthetic stubs default to `ACC_PUBLIC | ACC_SUPER` (0x0021) — the right
    /// guess for the public shim types the bootstrap mints (`PrintStream`, …).
    /// But some synthetic names shadow JDK-internal *nested* classes that do not
    /// exist on the classpath under that name (e.g. CratonVM's collection
    /// iterators are minted as `java/util/HashMap$KeyItr`, whereas the real JDK
    /// class is the package-private `java.util.HashMap$KeyIterator`). Reflecting
    /// on such a stub via `Class.getModifiers()` returned `public`, breaking
    /// callers that gate on the declaring class's accessibility — e.g. Spring's
    /// `ClassUtils.getInterfaceMethodIfPossible` / `isCacheSafe`, which use
    /// `Modifier.isPublic(clazz.getModifiers())` to decide method visibility
    /// (`ClassUtilsTests` asserts `hashMap.keySet().iterator().getClass()` is
    /// NOT public). Collection iterators are categorically non-public in the
    /// JDK; map their flags to the faithful real-JDK values here so reflection
    /// (and everything downstream of it) agrees with HotSpot.
    pub(in crate::class_manager) fn synthetic_stub_access_flags(name: &str) -> u16 {
        const ACC_PUBLIC_SUPER: u16 = 0x0021;
        const ACC_FINAL: u16 = 0x0010; // real KeyIterator/ValueIterator/…
        if Self::is_synthetic_collection_iterator(name) {
            // Real-JDK collection iterators (KeyIterator/ValueIterator/…) are
            // `final` and package-private.
            ACC_FINAL
        } else {
            ACC_PUBLIC_SUPER
        }
    }

    /// True for the fake names CratonVM mints for snapshot-backed collection
    /// iterators that have no real `.class` under that name (the real JDK class
    /// is e.g. `java.util.HashMap$KeyIterator`, package-private + `final`,
    /// `implements java.util.Iterator`). Drives both their access flags
    /// ([`synthetic_stub_access_flags`]) and the `Iterator` interface +
    /// `hasNext`/`next`/`remove` method entries injected for reflection.
    pub(in crate::class_manager) fn is_synthetic_collection_iterator(name: &str) -> bool {
        matches!(
            name,
            // HashMap/LinkedHashMap key/value/entry iterators.
            "java/util/HashMap$KeyItr"
                | "java/util/HashMap$ValItr"
                | "java/util/HashMap$EntryItr"
                // TreeMap key/value/entry iterators.
                | "java/util/TreeMap$KeyItr"
                | "java/util/TreeMap$ValItr"
                | "java/util/TreeMap$EntryItr"
        )
    }

    /// The `InnerClasses` entry a synthetic stub should carry, derived from its
    /// binary name.
    ///
    /// A synthetic stub is minted from a name alone — there is no class file to
    /// read an `InnerClasses` attribute from — so every JDK member class
    /// modelled as a stub reported the TOP-LEVEL answer to the whole reflection
    /// family that consults that attribute. `java.util.Map$Entry` had simple
    /// name `"Map$Entry"` (not `"Entry"`), canonical name
    /// `"java.util.Map$Entry"` (not `"java.util.Map.Entry"`), a null
    /// `getDeclaringClass()`/`getEnclosingClass()`, and `isMemberClass() ==
    /// false`. Same for `java.util.HashMap$Node`,
    /// `java.util.Collections$UnmodifiableList`, and every other stubbed
    /// nested class.
    ///
    /// Synthesize the entry javac would have emitted, but ONLY for the
    /// unambiguous member-class shape `Outer$Member`, where `Member` is a Java
    /// identifier not starting with a digit. Everything else keeps the honest
    /// top-level answer:
    ///
    ///   * a leading digit marks an anonymous (`Outer$1`) or local
    ///     (`Outer$1Loc`) class, whose real entry javac writes with a zeroed
    ///     `outer_class_info_index` (and, for anonymous, `inner_name_index`).
    ///     That is not reconstructible from the name, and guessing "member"
    ///     would be worse than reporting top-level.
    ///   * `$$` is the generated-proxy convention (Spring CGLIB
    ///     `$$SpringCGLIB$$`, ByteBuddy, EasyMock `$$$EasyMock$`, and lambda
    ///     hidden classes `Host$$Lambda/0x…`). Those are genuine TOP-LEVEL
    ///     classes whose literal binary name happens to contain `$`, and
    ///     `lang_class::simple_class_name` deliberately does not split them —
    ///     see the EasyMock `isAClassMock` note there.
    ///   * `com/sun/proxy/$Proxy0` has nothing before the `$` in its last
    ///     segment, so it is top-level too.
    ///
    /// The entry's `access_flags` are the stub's own
    /// ([`synthetic_stub_access_flags`]) rather than a guess. That keeps
    /// `Class.getModifiers()` — which prefers the `InnerClasses` flags over the
    /// class's own for a nested class (JVMS §4.7.6) — returning exactly what it
    /// did before this entry existed. In particular no `ACC_STATIC` is
    /// invented: whether a nested class is static is not derivable from its
    /// name (`HashMap$Node` is static, `HashMap$KeyIterator` is not).
    pub(in crate::class_manager) fn synthetic_inner_classes(name: &str) -> Vec<InnerClassEntry> {
        // Arrays have no InnerClasses attribute; `$$` is the generated-proxy
        // convention, never a javac nested class.
        if name.starts_with('[') || name.contains("$$") {
            return Vec::new();
        }
        let Some(split) = name.rfind('$') else {
            return Vec::new();
        };
        let (outer_class, inner_name) = (&name[..split], &name[split + 1..]);
        // `Outer` must actually name something: reject `$Foo` and
        // `com/sun/proxy/$Proxy0`, where the `$` opens the last name segment.
        if outer_class.is_empty() || outer_class.ends_with('/') {
            return Vec::new();
        }
        // `Member` must be a Java identifier that does not start with a digit.
        // (Splitting on the LAST `$` means it cannot itself contain one.)
        let mut chars = inner_name.chars();
        let is_member_name = match chars.next() {
            Some(first) => {
                (first.is_alphabetic() || first == '_')
                    && chars.all(|c| c.is_alphanumeric() || c == '_')
            }
            None => false,
        };
        if !is_member_name {
            return Vec::new();
        }
        vec![InnerClassEntry {
            inner_class: ThinArcStr::from(name),
            outer_class: ThinArcStr::from(outer_class),
            inner_name: ThinArcStr::from(inner_name),
            access_flags: Self::synthetic_stub_access_flags(name),
        }]
    }

    /// Register a minimal synthetic class with the given name and field count —
    /// the **compatibility stand-in** entry point, and the only one.
    ///
    /// If a class with this name is already loaded, returns its existing
    /// ClassId. Otherwise allocates a new ClassId, creates a minimal
    /// `Class` struct, and registers it in the class store.
    ///
    /// Prefer real `.class` files for application-visible types; see `docs/feature-designs/native-dispatch-architecture.md`.
    ///
    /// Under [`CompatibilityMode::JdkOnly`] this refuses to fabricate and
    /// returns `ClassNotFoundException`, recording a
    /// [`JdkOnlyViolation::CompatibilityClassRequested`]. Under
    /// [`CompatibilityMode::Compatible`] it fabricates, exactly as the VM
    /// always has.
    ///
    /// # The infallible twin is GONE — Real-JDK wave 2, step 3, 2026-08-10
    ///
    /// `ensure_synthetic_class` returned a bare `ClassId`, passed `enforce:
    /// false`, and so **recorded** the `CompatibilityClassRequested` violation
    /// and then fabricated anyway. A `--real-jdk` run therefore reported a
    /// violation while continuing in the exact state contract §5 forbids, and
    /// no signature in the chain could say otherwise. Wave 1 measured here and
    /// enforced only at the other end (`load_class` → `create_synthetic_stub`);
    /// wave 2 migrated the call sites and the three allocation funnels
    /// (`native-collections::alloc_synthetic`, `native-io::alloc_synthetic`,
    /// `native-builtins::alloc_concurrent_synthetic`, ~2,300 callers between
    /// them); step 3 deleted the entry point.
    ///
    /// Each surviving caller took one of three shapes, and the shape is the
    /// decision — a grep of the call name never was:
    ///
    ///   1. **A legitimately-generated VM class** (a lambda, a proxy or its
    ///      `Proxy$Instance` superclass, a reflection accessor, an array
    ///      shape): [`Self::ensure_generated_class`] with the matching
    ///      [`ClassOrigin`]. Never refused, in either mode, per contract §1
    ///      item 6. Natives reach it as
    ///      `NativeContext::ensure_vm_internal_class`.
    ///   2. **A stand-in for a class whose real bytes should have been found**:
    ///      this method, with the refusal propagated up the native's own error
    ///      path. `native_api::refusal_to_java_failure` turns it into the
    ///      catchable `NoClassDefFoundError` §5 asks for rather than the
    ///      uncatchable `MethodCallFailed::InternalError` a bare `?` produces.
    ///   3. **A caller with no error channel at all** — the VM bootstrap block,
    ///      a JNI entry point, a `-> Option<..>` helper. Those absorb the
    ///      refusal at a site that says so and warns, naming the class:
    ///      `vm_init::ensure_bootstrap_compat_class` and
    ///      `jni::jni_class_or_refuse` are the two shapes.
    ///
    /// # Behaviour when the name is ambiguous
    ///
    /// A name that two or more **distinct** classes already carry (see
    /// [`Self::classify_loaded_name`]) is refused in **both** modes, and that
    /// is a correctness gate rather than a policy one. Fabricating for such a
    /// name is the second defect the deleted twin carried: the stub lands under
    /// `(Bootstrap, name)`, the bootstrap loader is probed first, and the stub
    /// therefore outranks *every* real class that made the name ambiguous.
    /// A caller that genuinely has no error channel and must allocate something
    /// can ask [`Self::ambiguity_stand_in`] for a distinctly-named,
    /// correctly-sized `cratonvm/synthetic/AmbiguousName$…` class: the heap
    /// stays well-formed, the real classes keep resolving, and any `checkcast`
    /// / `instanceof` / method lookup against the *requested* name fails
    /// loudly, naming the stand-in.
    ///
    /// # Two distinct refusals
    ///
    /// 1. **Policy** — `--real-jdk` forbids compatibility stand-ins at all.
    ///    `ClassFileError::ClassNotFound`. Only under `JdkOnly`.
    /// 2. **Identity** — the name is already carried by two or more distinct
    ///    classes, so a stand-in filed under `(Bootstrap, name)` would shadow
    ///    all of them. `LinkageError::IncompatibleClassChangeError`, in **both**
    ///    modes. See [`Self::classify_loaded_name`] and
    ///    `docs/feature-designs/synthetic-class-fallibility.md`.
    ///
    /// The two are deliberately different error shapes: a caller retrying with
    /// an initiating loader (the right response to 2) must not confuse it with
    /// a policy refusal it cannot retry out of.
    ///
    /// # Every refusal is reported, not just the boot block's
    ///
    /// `vm_init::ensure_bootstrap_compat_class` `warn!`s when one of its three
    /// call sites is refused, and until 2026-08-12 that was the only refusal an
    /// operator saw. Measured that day under `--real-jdk --explain-jdk-only`,
    /// the split was not "some of the blockers are named" but a clean line:
    /// **every boot-time refusal was reported and every runtime refusal was
    /// silent.** Thirteen boot classes named
    /// (`java/util/Enumeration$Impl`, `java/util/Comparator$Native`, eleven
    /// `cratonvm/internal/Unmodifiable*`); nothing at all for the runtime
    /// refusals that were each killing an application path —
    /// `…/atomic/AtomicReferenceFieldUpdater$RustJvmImpl` (all of `java.sql`),
    /// `cratonvm/internal/UnmodifiableMap` (`System.getenv`, and through it
    /// Spring), `javax/net/ssl/SSLSocketOutputStream` (all HTTPS),
    /// `java/util/function/Consumer$AndThen`,
    /// `java/lang/foreign/DowncallHandle` (all FFM).
    ///
    /// The refusal IS the diagnosis for whatever fails next: a native asks for
    /// a receiver, is correctly refused, and the application dies far away with
    /// a `NoClassDefFoundError` naming something else. Emitting it here — the
    /// choke point every caller shares, boot and runtime alike — is what makes
    /// the failure self-diagnosing, and it does not depend on the violation-log
    /// bookkeeping the `--real-jdk-report`/`--trace-jdk-only` path uses, so a
    /// gap in that plumbing cannot swallow it.
    ///
    /// See [`Self::warn_fabrication_refused`] for the rate limit and for what
    /// the line can and cannot say about the caller.
    #[track_caller]
    pub fn try_ensure_synthetic_class(
        &mut self,
        name: &str,
        num_fields: usize,
    ) -> Result<ClassId, VmError> {
        // Captured before the call, not inside the error arm: `Location` is
        // the *caller's*, and taking it here keeps it identical to the one
        // `admit_compatibility_class` records as the violation's `requester`,
        // so the live line and the report name the same site.
        let site = core::panic::Location::caller();
        let fabricated =
            self.fabricate_class(name, num_fields, fabricated_origin_for_name(name), true);
        if let Err(err) = &fabricated {
            self.warn_fabrication_refused(name, num_fields, site, err);
        }
        fabricated
    }

    /// One `warn!` per distinct `(refused class, requesting site)` pair, then
    /// rate-limited on the repeats.
    ///
    /// # What the line names, and why that is the caller
    ///
    /// The `#[track_caller]` chain is unbroken from the native down to here —
    /// `NativeContext::try_ensure_synthetic_class` (the trait default and the
    /// `vm_exec` impl), `native-collections`/`native-io`/`native-builtins`'
    /// `refused_class` funnels, and
    /// [`Self::try_ensure_synthetic_class`] itself are each `#[track_caller]`
    /// — so `site` is the native's own `file:line`, not the funnel's. That is
    /// the half of the attribution this record needs: a bare class name says
    /// what was refused, and the requester says *who asked*, which is the
    /// native to fix.
    ///
    /// The class is named in full and nothing screens on a prefix. Three of
    /// the five runtime refusals measured on 2026-08-12 are in JDK package
    /// namespaces (`java/util/concurrent/atomic/…$RustJvmImpl`,
    /// `javax/net/ssl/SSLSocketOutputStream`,
    /// `java/lang/foreign/DowncallHandle`), so a `cratonvm/internal/*` screen
    /// — which the roadmap's definition of done still uses — would have missed
    /// the majority of them.
    ///
    /// What it cannot name is the **Java** frame. That arrives later, from
    /// `attach_origin_requester`, once the load unwinds to a VM choke point
    /// that can see a frame; waiting for it would defeat the point of
    /// reporting at the instant of refusal. `--real-jdk-report` /
    /// `--explain-jdk-only` carry both halves, and this line says so.
    ///
    /// # The rate limit, and where its state lives
    ///
    /// Keyed on the pair, counted in [`Self::jdk_only_refusals`] (a manager
    /// field — VM-scoped, never process-global; see that field's doc for why
    /// the key is the pair and not the class).
    ///
    /// The schedule is `n < 8 || n.is_power_of_two()`, which is the in-tree
    /// precedent (`gc::gc_quiescence::record_moving_young_coverage_fallback`,
    /// `gc::autobox::observe_primitive_into_reference_field`) and is chosen for
    /// the same reason: **the first occurrence is always visible**, a genuinely
    /// hot refusal cannot flood the log, and the `occurrence` field still shows
    /// it is hot. That last part matters here — "refused once during boot
    /// wiring" and "refused on every call of a live application path" are
    /// different defects, and a strict once-per-pair warn cannot tell them
    /// apart while nothing consumes the counters.
    ///
    /// Cost on the suppressed path is one `String` key and a hash. The refusal
    /// path already allocates a `String` per refusal for the
    /// `ClassNotFound { class_name }` it is about to return, and it ends in a
    /// Java throwable, so this does not put an allocation anywhere that did not
    /// have one. The `format!` for the message stays inside the emit branch.
    ///
    /// # Both refusals, deliberately
    ///
    /// `fabricate_class` has two (see this type's `try_ensure_synthetic_class`
    /// doc): the `--real-jdk` **policy** refusal, and the **identity** refusal
    /// for an ambiguous name, which fires in `Compatible` mode too. Both are
    /// warned. The identity refusal is rare, is a correctness event rather
    /// than a policy one, and was previously visible only to a caller that
    /// chose to log it; `error = %err` distinguishes the two without this
    /// function having to re-classify the name.
    pub(in crate::class_manager) fn warn_fabrication_refused(
        &mut self,
        name: &str,
        num_fields: usize,
        site: &'static core::panic::Location<'static>,
        err: &VmError,
    ) {
        let occurrence = {
            let counter = self
                .jdk_only_refusals
                .entry((name.to_string(), site.file(), site.line()))
                .or_insert(0);
            *counter += 1;
            *counter
        };
        if occurrence >= 8 && !occurrence.is_power_of_two() {
            return;
        }
        let requested_by = format!("{}:{}", site.file(), site.line());
        tracing::warn!(
            class = name,
            fields = num_fields,
            requested_by = requested_by.as_str(),
            occurrence,
            error = %err,
            "refusing to fabricate a compatibility stand-in for this class. It is NOT \
             registered and the requester above did not get its receiver, so the failure \
             will surface later, at that caller's own call site and usually naming a \
             different class. One line per (class, requester); repeats are rate-limited \
             and `occurrence` counts them. --real-jdk-report and --explain-jdk-only list \
             every refusal with its requester, including the Java frame when there is one."
        );
    }

    /// `(class name, requester file, requester line) -> refusal count`.
    ///
    /// The census view of what the rate-limited `warn!` above reports. A
    /// caller that wants "which refusals are on a hot path", or wants the full
    /// set in one place at shutdown rather than interleaved through the run,
    /// reads this rather than the log.
    ///
    /// Deliberately **not** named `jdk_only_refusal_counts`: `SharedVm` already
    /// has a method by that name returning `JdkOnlyRefusalCounts`, which is the
    /// JIT/dispatch event tally and a different thing entirely.
    pub fn compatibility_refusal_counts(&self) -> &FxHashMap<RefusalSite, u64> {
        &self.jdk_only_refusals
    }

    /// Register a VM-generated class with an explicit, legitimate provenance.
    ///
    /// This is the other half of the §5 API boundary: `ensure_synthetic_class`
    /// / `try_ensure_synthetic_class` mint *compatibility stand-ins* (the one
    /// thing `--real-jdk` forbids), while this mints the classes a conforming
    /// JVM creates without any class file — array-adjacent shapes, lambda and
    /// proxy implementation classes, reflection accessors, and the VM's own
    /// internal allocation shapes. Those are legal in both modes (contract §1
    /// item 6), so **this method never refuses and never records a
    /// violation**, whatever the compatibility mode.
    ///
    /// `origin` must not be a [`ClassOrigin::CompatibilityStub`]; that is what
    /// the other two entry points are for. Passing one anyway is a caller bug
    /// and is caught by a debug assertion rather than silently laundering a
    /// stub past the policy.
    ///
    /// # Why there is no `.expect` here any more
    ///
    /// Both of [`Self::fabricate_class`]'s refusals are gated on
    /// `origin.is_compatibility_stub()`, which the debug assertion above says
    /// is false for every caller — so in a debug build this cannot fail. In a
    /// **release** build the assertion is compiled out, and a caller that did
    /// pass a stub origin would reach the ambiguity gate and get an `Err`;
    /// `.expect`ing it would abort the VM for what is a recoverable naming
    /// conflict. The stand-in keeps the failure local to the one allocation.
    #[track_caller]
    pub fn ensure_generated_class(
        &mut self,
        name: &str,
        num_fields: usize,
        origin: ClassOrigin,
    ) -> ClassId {
        debug_assert!(
            !origin.is_compatibility_stub(),
            "ensure_generated_class is the NON-compatibility entry point; \
             {name} was passed a CompatibilityStub origin — use \
             try_ensure_synthetic_class instead so the policy can see it",
        );
        let fabricated = self.fabricate_class(name, num_fields, origin, false);
        match fabricated {
            Ok(id) => id,
            Err(_) => self.ambiguity_stand_in(name, num_fields),
        }
    }

    /// The class handed back when a compatibility fabrication was refused and
    /// the caller's signature has no way to say so.
    ///
    /// # The contract
    ///
    /// This is a **miss, not a guess**. It is emphatically *not* a stub for
    /// `name`:
    ///
    /// * it is registered under `cratonvm/synthetic/AmbiguousName$<mangled
    ///   name>$<fields>`, a name in the VM's own reserved namespace, so it can
    ///   never be reached by a resolution of `name` and can never outrank the
    ///   classes that made `name` ambiguous;
    /// * it declares exactly `num_fields` slots, so an object allocated
    ///   against it has a layout the GC's `get_field` bounds guard accepts —
    ///   the alternative (`ClassId::new(0)`, `java/lang/Object`, zero fields)
    ///   produces an undersized object that faults on the *first field write*,
    ///   somewhere unrelated to the cause;
    /// * it carries [`ClassOrigin::VmInternal`], because that is what it is: a
    ///   VM bookkeeping shape. It is not a compatibility stand-in for anyone's
    ///   class, so it must not be counted as one in the `--real-jdk` census.
    ///
    /// The `num_fields` suffix keeps two different requested shapes for the
    /// same ambiguous name from colliding on one undersized stand-in.
    ///
    /// # What the caller sees
    ///
    /// A valid `ClassId` that fails every identity question about `name`:
    /// `checkcast`/`instanceof` to `name` is false, a virtual call resolves to
    /// nothing but `java/lang/Object`'s natives, and the class name in any
    /// resulting `ClassCastException` / `NoSuchMethodError` message contains
    /// the requested name — which is how this is diagnosed in the field.
    pub(in crate::class_manager) fn ambiguity_stand_in(&mut self, name: &str, num_fields: usize) -> ClassId {
        let mangled: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let stand_in = format!("cratonvm/synthetic/AmbiguousName${mangled}${num_fields}");
        debug!(
            requested = name,
            stand_in = stand_in.as_str(),
            "refusing to fabricate a stand-in under an ambiguous class name"
        );
        // `VmInternal` is not a compatibility stub, so neither of
        // `fabricate_class`'s two refusals can fire for it and this recursion
        // is exactly one level deep. `unwrap_or` rather than `expect` all the
        // same: a future third refusal must not turn this into an abort, and
        // `ClassId::new(0)` is the same last-resort the `NativeContext` default
        // has always used.
        self.fabricate_class(&stand_in, num_fields, ClassOrigin::VmInternal, false)
            .unwrap_or(ClassId::new(0))
    }

    /// Shared body of the three `ensure_*_class` entry points.
    ///
    /// `enforce` decides what happens when `origin` is a
    /// [`ClassOrigin::CompatibilityStub`] and the manager is in
    /// [`CompatibilityMode::JdkOnly`]: `true` returns the
    /// `ClassNotFoundException` the contract asks for, `false` records the
    /// violation and fabricates anyway. Either way the violation is recorded,
    /// and either way `Compatible` mode fabricates — wave 1 is measurement
    /// (contract §10).
    ///
    /// The policy check is deliberately placed at the *mint* point, past both
    /// early returns. A name that is already loaded, or whose real bytes turn
    /// up on the classpath, is not a fabrication and must not appear in the
    /// census: those two cases are the overwhelming majority of calls (every
    /// allocation of a stub-backed container re-enters here), and counting
    /// them would bury the handful of names that actually have no bytes.
    ///
    // JDK-ONLY-WAVE2, CLOSED 2026-08-06: every class minted here is labelled
    // with whatever `origin` the caller passed. `ensure_synthetic_class` used to
    // pass `CompatibilityStub` unconditionally, which over-reported two of its
    // callers. Both are now classified by name in
    // [`fabricated_origin_for_name`], so the caller no longer decides:
    //   * `cratonvm/synthetic/AnonymousObject$N` — the untyped allocation
    //     shape behind every `HashMap`/`LinkedHashMap` node and friends
    //     (`alloc_concurrent_synthetic` in `native-builtins`). **DONE
    //     (2026-08-04)** — the minting site in `vm_exec::heap_alloc_object`
    //     calls [`Self::ensure_generated_class`] with `VmInternal`. Safe to flip
    //     because the class is inert at every read site: no native is registered
    //     on it, neither real-protected-stub allow-list names it, and the
    //     `Proxy$Instance` / collection-iterator special cases below key on the
    //     *name*, not the origin.
    //   * `java/lang/reflect/Proxy$Instance` — the VM's own supertype for
    //     every generated `$ProxyN` (`proxy_gen` / the `Proxy` natives). A
    //     generation artefact, not a stand-in for absent bytes. **DONE
    //     (2026-08-06)**, as `VmInternal`: `GeneratedProxy` was the wrong
    //     prescription (it carries `interfaces: Arc<[ClassId]>`, which the
    //     shared *supertype* has no value for) and `is_generated_proxy_name`
    //     already says in terms that this name "must NOT be counted as a
    //     generated proxy".
    //
    // What made the second one hard: unlike `AnonymousObject$N` it has a
    // NATIVE-flagged `<init>` from `synthetic_stub_ctor_methods` and native
    // registrations of its own, so it genuinely needs the dispatch branches
    // that used to be selected by the derived `is_synthetic_stub` bool —
    // flipping the origin alone would have moved it off three of them. The
    // prerequisite was splitting that bool's two meanings; those sites now ask
    // [`Class::dispatch_lacks_class_file`](crate::Class::dispatch_lacks_class_file),
    // which answers the dispatch question directly and gives the same answer for
    // this class before and after. See
    // `jdk-only-wave2-vm-internal-classes-mislabelled-RETIRED-20260806.md`.
    #[track_caller]
    pub(in crate::class_manager) fn fabricate_class(
        &mut self,
        name: &str,
        num_fields: usize,
        origin: ClassOrigin,
        enforce: bool,
    ) -> Result<ClassId, VmError> {
        let synthetic_access_flags = Self::synthetic_stub_access_flags(name);
        if let Some(id) = self.get_loaded_class_id(name) {
            // Already loaded — but it might be a synthetic stub created by
            // an earlier `ensure_synthetic_class` call with an undersized
            // `num_fields`. If a real `.class` file is now reachable on the
            // classpath, upgrade the stub in place so its `num_total_fields`
            // (and hence every subsequently-allocated object) reflects the
            // real field layout. Without this, an object allocated against
            // the stub's tiny layout drops every `putfield` past the stub's
            // slot count — the WildFly `WFLYCTL0002` boot failure and the
            // `PrintStream` charset-NPE (commit cf1b478) are both instances
            // of this bug class.
            let is_synthetic = self
                .class_store
                .get(id)
                .map(|c| c.origin.is_compatibility_stub())
                .unwrap_or(false);
            // HIB-DEV-03: skip the full-classpath upgrade rescan once we've
            // learned the real `.class` is absent — otherwise every
            // allocation of this stub (e.g. a `HashMap` node's
            // `cratonvm/synthetic/AnonymousObject$N`) re-scans every JAR.
            if is_synthetic && !name.starts_with('[') && !self.synthetic_upgrade_known_absent(name)
            {
                match self.find_class_bytes_delegated(name) {
                    Ok((bytes, loader_id)) => {
                        // zt t20/pl9: the real define of a class-path class.
                        self.note_class_path_define(name, loader_id, bytes.len());
                        if let Err(e) = self.upgrade_synthetic_class(id, name, bytes, loader_id) {
                            tracing::debug!(
                                class = name,
                                "ensure_synthetic_class: real-class upgrade failed: {e:?}"
                            );
                        }
                    }
                    Err(_) => self.note_synthetic_upgrade_absent(name),
                }
            }
            return Ok(id);
        }
        // ── Identity gate (classloading-identity-audit, Open 2) ───────────────
        //
        // `get_loaded_class_id` just answered `None`, and that single answer
        // carries two opposite facts: *nobody* has this name (fabricate away),
        // or **several distinct classes do** and it has no context-free answer
        // (it returns `None` for exactly that case — "ambiguous is a miss, not
        // a guess"). Everything below this line assumes the first reading. On
        // the second, both of the remaining paths are wrong:
        //
        //   * the `load_class` retry would define a *third* copy of the name;
        //   * the fabrication would file an empty stub under
        //     `(Bootstrap, name)`, and since the built-in chain is probed
        //     first, that stub then outranks **both** real classes for every
        //     subsequent by-name resolution. Symptom in the field:
        //     `NoSuchMethodError` / `<init> … has no Code attribute` on a class
        //     that is demonstrably loaded. This is the Groovy
        //     `$_run_closureN` shape — two `GroovyClassLoader$InnerLoader`s,
        //     one name, two genuinely different classes.
        //
        // So refuse, and say which of the two it is. The gate is scoped to
        // compatibility stand-ins on purpose: `ensure_generated_class` is
        // minting a *new* VM class (a lambda, a proxy, an allocation shape)
        // under a name it just constructed, which is a different question and
        // one this lane deliberately does not answer.
        //
        // NOT covered, and tracked in
        // `docs/feature-designs/synthetic-class-fallibility.md`: when a built-in
        // loader *also* has a class under the name, `get_loaded_class_id`
        // returns that copy from the early return above and never reaches this
        // gate. That is the pre-existing built-in-first delegation answer every
        // context-free lookup in this file gives; closing it means migrating
        // the early return itself to `classify_loaded_name`, which changes the
        // answer for callers that are relying on it today.
        if origin.is_compatibility_stub() {
            if let NameResolution::Ambiguous { definitions } = self.classify_loaded_name(name) {
                return Err(ambiguous_stand_in_refused(name, definitions));
            }
        }
        // Not loaded yet: prefer the real `.class` file over a possibly
        // undersized synthetic stub. `ensure_synthetic_class` is frequently
        // called with a hand-picked `num_fields` (often 1) that predates the
        // real JDK/app class being on the classpath; allocating objects
        // against that stub layout silently truncates field writes. Try a
        // real classpath load first — but only when the bytes are actually
        // present, so pure synthetic-jdk mode (no real boot/app jars) skips
        // straight to the synthetic stub below with no behaviour change.
        //
        // `load_class` is the authoritative loader: it handles the
        // circular-load guard, registers the name in `loaded_classes`, and
        // computes the real field layout. If it succeeds we MUST return its
        // id — falling through would mint a second `Class` for an
        // already-registered name and mis-key `loaded_classes`. When the
        // bytes are genuinely absent (`find_class_bytes_delegated` errs) we
        // skip this entirely and build the requested-size synthetic stub
        // below, exactly as before.
        if !name.starts_with('[') && !self.synthetic_upgrade_known_absent(name) {
            if self.find_class_bytes_delegated(name).is_ok() {
                match self.load_class(name) {
                    Ok(id) => return Ok(id),
                    Err(e) => {
                        tracing::debug!(
                            class = name,
                            "ensure_synthetic_class: real-class load failed, \
                             falling back to synthetic stub: {e:?}"
                        );
                    }
                }
            } else {
                // No real `.class` on the classpath — memoize so the stub we
                // create below isn't re-probed on every future allocation.
                self.note_synthetic_upgrade_absent(name);
            }
        }
        // Real-JDK choke point #2 (contract §5). Everything above this line
        // either returned an already-loaded class or loaded real bytes; from
        // here on we are inventing a class that has none. `create_synthetic_stub`
        // is the other choke point, covering the `load_class` chain.
        if origin.is_compatibility_stub() {
            if let Err(e) = self.admit_compatibility_class(name, origin.reason().unwrap_or("")) {
                if enforce {
                    return Err(e);
                }
            }
        }
        // `CRATONVM_DBG_STUB_BT=<substring>` -- same hook as the one in
        // `load_class`'s synthetic-stub fallback, for the OTHER way a stub
        // gets minted. Without it a stub fabricated through this entry point
        // is indistinguishable from a real class at the Java level.
        if let Some(want) = dbg_stub_bt_filter() {
            if !want.is_empty() && name.contains(want) {
                let bt = std::backtrace::Backtrace::force_capture();
                eprintln!("[DBG_STUB_BT] ensure_synthetic_class stub for {name}\n{bt}");
            }
        }
        // Every synthetic stub object IS-A `java.lang.Object`, so its
        // superclass must be `java/lang/Object` (not `None`). Without this
        // link, method dispatch on a synthetic-stub receiver — e.g. a
        // `cratonvm/synthetic/AnonymousObject$N` allocated by a native that
        // passed `ClassId(0)` with N fields — walks an empty superclass chain
        // and never reaches the natives registered on `java/lang/Object`
        // (`clone`, `equals`, `hashCode`, `toString`, `getClass`, `wait`,
        // `notify`, …). The result is a spurious `NoSuchMethodError:
        // cratonvm/synthetic/AnonymousObject$4.clone()`. Resolving Object's id
        // here (it is always loaded before any synthetic object is allocated)
        // makes those inherited Object methods reachable. Arrays and Object
        // itself keep `superclass = None`.
        let synthetic_superclass = if name == "java/lang/Object" || name.starts_with('[') {
            None
        } else if name == "javax/net/ssl/SSLSocketOutputStream" {
            // FIX (netty-https-client-trust residual): this synthetic class
            // stands in for the NEW-13 client socket's OutputStream
            // (phases_late.rs's alloc_concurrent_synthetic call), but with no
            // special case here it got the blanket java/lang/Object
            // superclass below — so any caller storing the result in an
            // OutputStream-typed local/field (javac emits a checkcast when
            // the compile-time and declared types differ) failed with
            // ClassCastException: SSLSocketOutputStream cannot be cast to
            // java.io.OutputStream. Give it its real ancestor, mirroring the
            // Proxy$Instance special case above.
            self.get_loaded_class_id("java/io/OutputStream")
                .or_else(|| self.get_loaded_class_id("java/lang/Object"))
        } else if name == "javax/net/ssl/SSLSocketInputStream" {
            // Same reasoning as SSLSocketOutputStream above, InputStream side.
            self.get_loaded_class_id("java/io/InputStream")
                .or_else(|| self.get_loaded_class_id("java/lang/Object"))
        } else if name == "cratonvm/synthetic/Process" {
            // Same reasoning again, for the object `ProcessBuilder.start()`
            // hands back (`native-io`'s `spawn_and_wrap`).
            //
            // With the blanket `java/lang/Object` superclass below, the VM
            // contradicted itself about this class: `Process.class
            // .isAssignableFrom(p.getClass())` answered `true` — the subtype
            // machinery knows the relation — while walking
            // `p.getClass().getSuperclass()` never reached `java.lang.Process`.
            // Measured against HotSpot 25:
            //
            //   HotSpot   ProcessImpl -> Process -> Object    consistent
            //   CratonVM  synthetic/Process -> Object         NOT consistent
            //
            // `instanceof`, casts, `isInstance`, `isAssignableFrom`, `List.of`
            // and array stores all agreed with HotSpot; only the hand-walked
            // hierarchy disagreed, which is precisely the shape serialization
            // frameworks, DI containers, matchers and mock frameworks use to
            // decide assignability. Naming the real ancestor makes the two
            // answers agree.
            //
            // `java.lang.Process` is abstract, which costs nothing here: this
            // class is concrete and is allocated directly by a native, never
            // through `new`, so no instantiability check consults the super.
            //
            // The natives keep winning for this receiver — dispatch probes the
            // registry from the receiver's OWN class name first, and both
            // `cratonvm/synthetic/Process` and `java/lang/Process` are
            // registered — so inheriting `Process`'s concrete bytecode does not
            // change which body runs. See `process.rs::is_vm_process`, which
            // keys on this exact name.
            self.get_loaded_class_id("java/lang/Process")
                .or_else(|| self.get_loaded_class_id("java/lang/Object"))
        } else if name == "cratonvm/synthetic/ProcessPipeInputStream" {
            // Reaches generic java/io/InputStream methods (e.g. transferTo,
            // readAllBytes, readNBytes) naturally via receiver-driven dispatch.
            self.get_loaded_class_id("java/io/InputStream")
                .or_else(|| self.load_class("java/io/InputStream").ok())
                .or_else(|| self.get_loaded_class_id("java/lang/Object"))
        } else if name == "cratonvm/synthetic/ProcessPipeOutputStream" {
            // Reaches generic java/io/OutputStream methods naturally via
            // receiver-driven dispatch.
            self.get_loaded_class_id("java/io/OutputStream")
                .or_else(|| self.load_class("java/io/OutputStream").ok())
                .or_else(|| self.get_loaded_class_id("java/lang/Object"))
        } else if matches!(
            name,
            "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl"
                | "java/util/concurrent/atomic/AtomicLongFieldUpdater$RustJvmImpl"
                | "java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl"
        ) {
            // MEASURED 2026-08-27 (`probes/RustJvmImplReach.java`), compatible
            // mode against HotSpot 25.0.3+9:
            //
            //   HotSpot   ...$AtomicIntegerFieldUpdaterImpl -> AtomicIntegerFieldUpdater
            //   CratonVM  ...$RustJvmImpl                   -> java.lang.Object
            //
            //   u instanceof AtomicIntegerFieldUpdater   HotSpot true, CratonVM FALSE
            //   u.updateAndGet(box, x -> x + 1)          HotSpot ok,   CratonVM NoSuchMethodError
            //
            // `jdk_superclass` ALREADY maps all three of these names to their
            // abstract base, and its own comment says why: "the cast only
            // succeeds if the returned object's class chain reaches the
            // abstract base". **This mint path never asks that table**, so the
            // mapping was dead code and the premise it states was false. The
            // sibling `jdk_interfaces(name)` call thirty lines below IS
            // consulted generally, which is the asymmetry that hid it.
            //
            // Consequence, and it is not cosmetic: the base declares
            // `updateAndGet`, `getAndUpdate`, `accumulateAndGet` and
            // `getAndAccumulate` as ordinary `public final` bytecode written in
            // terms of `get`/`compareAndSet` -- both of which ARE registered on
            // the impl. With no link to the base, all four resolve to nothing
            // and any caller dies. `probes/AtomicUpdaterSweep.java` runs 87/87
            // clean under `--real-jdk` (which never mints this class and uses
            // the real JDK impl) and DIES in compatible mode at the first
            // `updateAndGet`.
            //
            // Named explicitly rather than by consulting `jdk_superclass` here:
            // that table covers a large slice of the JDK, and routing every
            // fabricated stub through it would change superclasses, field
            // layouts, `instanceof` and catch-matching for classes this change
            // never measured. The general divergence is recorded separately;
            // this arm fixes the three that were measured, in the same idiom as
            // the four special cases above it.
            let base = name
                .strip_suffix("$RustJvmImpl")
                .expect("matched arm ends in $RustJvmImpl")
                .to_string();
            // LOAD it, do not merely look it up. The first cut of this fix used
            // `get_loaded_class_id` like the four cases above, and it changed
            // nothing: at the moment `newUpdater` mints the impl, the abstract
            // base is NOT yet a loaded class, so the lookup missed and the
            // `or_else` quietly restored `java/lang/Object` -- the very default
            // this arm exists to override.
            //
            // Why it is not loaded despite `newUpdater` being a static method ON
            // it: `newUpdater` is a REGISTERED NATIVE, so the invokestatic
            // resolves through the native registry without the class ever being
            // loaded from the image. The four cases above happen to name classes
            // (`java/io/OutputStream`, `java/lang/Process`) that boot long
            // before any of them is minted, which is why the weaker idiom has
            // held there and why copying it here was a silent no-op.
            //
            // `load_class` is what the sibling synthetic-stub path already uses
            // for exactly this (`create_synthetic_stub`'s
            // `Some(self.load_class(parent)?)`). An error falls back to
            // `java/lang/Object` rather than failing the mint: a fabricated
            // object with a worse superclass is still better than no object.
            self.get_loaded_class_id(&base)
                .or_else(|| self.load_class(&base).ok())
                .or_else(|| self.get_loaded_class_id("java/lang/Object"))
        } else {
            self.get_loaded_class_id("java/lang/Object")
        };
        // spring-bug-08: the synthetic `Proxy$Instance` super of every
        // generated `$ProxyN` must mirror the real `java.lang.reflect.Proxy`
        // closely enough for `ObjectOutputStream`/`ObjectInputStream` to
        // round-trip a JDK dynamic proxy. Two facts of the real `Proxy`:
        //   (a) `java.lang.reflect.Proxy implements java.io.Serializable`, so
        //       EVERY proxy is serializable; without this, real-OOS
        //       `lookup(superclass, all=false)` returns null for `Proxy$Instance`
        //       (not Serializable) → the proxy's `superDesc` is written as
        //       `TC_NULL` and the handler is dropped.
        //   (b) `Proxy` declares `protected InvocationHandler h;` — the single
        //       serialized field. Declaring `h` here (slot 0, the same slot the
        //       proxy natives already use for the handler) puts the handler in
        //       the serialized field set so it is restored on read.
        // Slots 1 (interfaces `Class[]`) and 2 (identity-hash) stay native-only
        // (accessed by raw index, undeclared) so they are not serialized.
        let iface_names = jdk_interfaces(name).to_vec();
        let (mut synthetic_interfaces, synthetic_fields): (
            Vec<ClassId>,
            Vec<cratonvm_reader::field::ClassFileField>,
        ) = if name == "java/lang/reflect/Proxy$Instance" {
            let ifaces = self
                .get_loaded_class_id("java/io/Serializable")
                .into_iter()
                .collect();
            let fields = vec![cratonvm_reader::field::ClassFileField {
                access_flags: FieldAccessFlags::empty(),
                name: cratonvm_types::intern_arc("h"),
                descriptor: cratonvm_types::intern_arc("Ljava/lang/reflect/InvocationHandler;"),
                attributes: vec![],
            }];
            (ifaces, fields)
        } else if Self::is_synthetic_collection_iterator(name) {
            // CratonVM's snapshot-backed collection iterators (minted under fake
            // names like `java/util/HashMap$KeyItr`) must report
            // `implements java.util.Iterator` so reflection matches the real JDK
            // iterators they stand in for — Spring's `ClassUtils
            // .getInterfaceMethodIfPossible` walks `targetClass.getInterfaces()`
            // to late-bind `iterator.hasNext()` to `Iterator.hasNext`
            // (ClassUtilsTests). The matching `hasNext`/`next`/`remove` method
            // entries are declared by `synthetic_stub_ctor_methods`.
            let ifaces = self
                .get_loaded_class_id("java/util/Iterator")
                .into_iter()
                .collect();
            (ifaces, vec![])
        } else {
            (vec![], vec![])
        };
        let id = self.class_store.next_id();
        let mut class = Class {
            id,
            loader_id: ClassLoaderId::Bootstrap,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: cratonvm_reader::class_file_version::ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: cratonvm_reader::constant_pool::ConstantPool::new(vec![
                cratonvm_reader::constant_pool::ConstantPoolEntry::Tombstone,
            ]),
            access_flags: cratonvm_reader::class_access_flags::ClassAccessFlags::from_bits_truncate(
                synthetic_access_flags,
            ),
            superclass: synthetic_superclass,
            interfaces: synthetic_interfaces.clone(),
            fields: synthetic_fields,
            methods: synthetic_stub_ctor_methods(name),
            first_field_index: 0,
            num_total_fields: num_fields,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Self::synthetic_inner_classes(name),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            // Overwritten (together with `is_synthetic_stub`) by the
            // `set_origin` call below — the only writer of either field.
            origin: ClassOrigin::default(),
            has_finalizer: false,
            signature: None,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        };
        // For `ensure_synthetic_class` this is a `CompatibilityStub`, so the
        // derived bool lands on `true` exactly as the previous literal did.
        // Not yet stored: the in-place provenance epoch stays put (wave 94).
        class.set_origin_of_unstored_class(origin);
        self.class_store.add(class);
        self.register_class_name(ClassLoaderId::Bootstrap, name, id);

        // The class is registered now, so resolving its curated synthetic
        // interfaces cannot recursively create the same class. This mirrors the
        // other synthetic-stub path and keeps checkcast/instanceof honest for
        // native-only helper classes such as Function$Identity.
        for iface_name in iface_names {
            if let Ok(iface_id) = self.load_class(iface_name) {
                if !synthetic_interfaces.contains(&iface_id) {
                    synthetic_interfaces.push(iface_id);
                }
            }
        }
        // Round 13 wave 9 (lane proxy6): the annotation carrier is the
        // `InvocationHandler` of every annotation `$ProxyN` (HotSpot's
        // `AnnotationInvocationHandler`), so `Proxy.getInvocationHandler(ann)
        // instanceof InvocationHandler` must hold. Calls on the carrier are
        // routed by name before any method lookup, and CHA binds only behind a
        // receiver guard, so the new implementor edge changes no dispatch.
        // `CRATONVM_ANNOTATION_CARRIER_IS_HANDLER=0` keeps `Annotation` alone.
        if is_vm_annotation_carrier_name(name)
        {
            if let Ok(iface_id) = self.load_class("java/lang/reflect/InvocationHandler") {
                if !synthetic_interfaces.contains(&iface_id) {
                    synthetic_interfaces.push(iface_id);
                }
            }
        }
        // Through `set_interfaces`, not a raw `class.interfaces =` write: the
        // store's direct-implementor index has to see these edges, or
        // class-hierarchy analysis cannot see any `invokeinterface` site whose
        // receiver is one of these stubs.
        if !synthetic_interfaces.is_empty() {
            self.class_store.set_interfaces(id, synthetic_interfaces);
        }

        Ok(id)
    }

    /// Check if the boot classpath has real JDK class files (not just empty).
    ///
    /// Returns true if `java/lang/Object.class` can be found on the bootstrap classpath.
    ///
    /// JIT round 23 wave 3 (proxy-define page; `CRATONVM_REAL_BOOT_CLASSES_LATCH`,
    /// default ON, `=0` kill): a `true` answer is latched per manager. It
    /// read `Object`'s bytes from the image on every call -- every
    /// `would_fabricate_synthetic_stub` asks it, and the link-time verifier-load
    /// resolution asks that per name (2.7 % of the proxy row's main thread). The
    /// boot class path only ever grows (`-Xbootclasspath/a`, an agent's
    /// `appendToBootstrapClassLoaderSearch`), so a found `Object` stays found; a
    /// `false` is asked again every time, as before.
    pub fn has_real_boot_classes(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.real_boot_classes_latched.load(Ordering::Relaxed) {
            return true;
        }
        let found = self.bootstrap.find_class_bytes("java/lang/Object").is_ok();
        if found && self.real_boot_classes_latch {
            self.real_boot_classes_latched.store(true, Ordering::Relaxed);
        }
        found
    }

    /// Whether the runtime image defines `internal` -- the classes the
    /// bootstrap and platform loaders can define, every module of the image
    /// being one of theirs. `None` when the boot classpath carries no image
    /// (a synthetic-JDK build), so the caller keeps its own answer.
    pub fn runtime_image_defines_class(&self, internal: &str) -> Option<bool> {
        let image = self
            .bootstrap
            .class_path()
            .runtime_image_defines_class(internal);
        // A class a `--patch-module` of an IMAGE module adds (not one it
        // replaces: the image answers those) is that module's, so its loader's
        // -- `java.base=<dir>` holding `java/lang/PatchedHello.class` is a
        // bootstrap class on HotSpot (`ModulePatcher` adds the patch to the
        // module's reader). The image index alone answered "not the image's",
        // and every gate built on this answer (`findBootstrapClass` under
        // `--real-jdk`, the `RuntimeImageNames` loader gates in both modes)
        // refused it before the boot class path -- which the patch paths head
        // -- was searched (interpreter round i4 wave 90 follow-up, lane L5;
        // probe `L5W87ModuleOptions`, row `patch-java-base`).
        if image == Some(false) && self.image_module_patch_adds_class(internal) {
            return Some(true);
        }
        image
    }

    /// Whether a `--patch-module` path of an image module holds
    /// `internal.class` (see [`Self::runtime_image_defines_class`]). One empty
    /// check without the option.
    pub(in crate::class_manager) fn image_module_patch_adds_class(&self, internal: &str) -> bool {
        let patches = self.module_registry.patches();
        if patches.is_empty() {
            return false;
        }
        let relative = format!("{internal}.class");
        let mut jar_patch = false;
        for (module, paths) in patches {
            if !crate::module::is_platform_module_name(module) {
                continue;
            }
            for path in paths {
                let root = std::path::Path::new(path);
                if root.is_dir() {
                    if root.join(&relative).is_file() {
                        return true;
                    }
                } else if root.is_file() {
                    jar_patch = true;
                }
            }
        }
        // A patch JAR heads the boot class path (`vm_init`): the image said
        // no, so a boot-path hit is a patch's (or an upgrade / appended
        // entry's, which the boot loader defines too).
        jar_patch && self.bootstrap.find_class_bytes(internal).is_ok()
    }

    /// List all JMOD module names on the boot classpath.
    ///
    /// Returns module names extracted from JMOD file stems
    /// (e.g. `java.base.jmod` → `"java.base"`).
    pub fn list_boot_modules(&self) -> Vec<String> {
        self.bootstrap.class_path().list_jmod_modules()
    }

    /// Return the total number of classes available in boot classpath JMODs.
    pub fn boot_jmod_class_count(&self) -> usize {
        self.bootstrap.class_path().jmod_class_count()
    }

    /// Pre-load essential bootstrap classes from the boot classpath.
    ///
    /// This must be called early in VM initialization (before any Java code runs)
    /// when real JDK class files are available. It loads the core classes that
    /// everything else depends on: Object, Serializable, Comparable, CharSequence,
    /// String, Class, System, and Throwable.
    ///
    /// Returns the number of classes successfully loaded, or 0 if no real JDK.
    pub fn bootstrap_core_classes(&mut self) -> usize {
        if !self.has_real_boot_classes() {
            return 0;
        }

        // Order matters: Object first (no superclass), then interfaces, then classes
        // that depend on them. Each load_class call recursively loads dependencies.
        let core_classes = [
            "java/lang/Object",
            "java/io/Serializable",
            "java/lang/Comparable",
            "java/lang/CharSequence",
            "java/lang/constant/Constable",
            "java/lang/constant/ConstantDesc",
            "java/lang/String",
            "java/lang/Class",
            "java/lang/Cloneable",
            "java/lang/Number",
            "java/lang/Integer",
            "java/lang/Long",
            "java/lang/Boolean",
            "java/lang/Byte",
            "java/lang/Short",
            "java/lang/Character",
            "java/lang/Float",
            "java/lang/Double",
            "java/lang/Throwable",
            "java/lang/Exception",
            "java/lang/RuntimeException",
            "java/lang/Error",
            "java/lang/System",
            "java/lang/Thread",
            "java/lang/Iterable",
            "java/util/Iterator",
        ];

        let minimal_extras = [
            "java/lang/ClassLoader",
            "java/lang/ref/Reference",
            "java/util/HashMap",
            "java/util/HashMap$Node",
            "java/util/HashMap$KeySet",
            "java/util/HashMap$EntrySet",
            "java/util/HashMap$Values",
        ];
        if std::env::var_os("CRATONVM_EAGER_BOOTSTRAP_ALL").is_none() {
            let mut loaded = 0usize;
            for name in core_classes.iter().chain(minimal_extras.iter()) {
                if self.load_class(name).is_ok() {
                    loaded += 1;
                }
            }
            return loaded;
        }

        // Extended classes — loaded after core to provide common stdlib bytecode
        let extended_classes = [
            "java/lang/Math",
            "java/lang/StrictMath",
            "java/lang/StringBuilder",
            "java/lang/StringBuffer",
            "java/lang/AbstractStringBuilder",
            "java/lang/Enum",
            "java/lang/Void",
            "java/lang/StackTraceElement",
            "java/lang/ClassLoader",
            "java/lang/ref/Reference",
            "java/lang/ref/WeakReference",
            "java/lang/ref/SoftReference",
            "java/lang/ref/PhantomReference",
            "java/lang/ref/ReferenceQueue",
            "java/lang/reflect/AccessibleObject",
            "java/lang/reflect/Member",
            "java/lang/reflect/Field",
            "java/lang/reflect/Method",
            "java/lang/reflect/Constructor",
            "java/util/Collection",
            "java/util/List",
            "java/util/Set",
            "java/util/Map",
            "java/util/AbstractCollection",
            "java/util/AbstractList",
            "java/util/AbstractSet",
            "java/util/AbstractMap",
            "java/util/ArrayList",
            "java/util/HashMap",
            "java/util/HashSet",
            "java/util/LinkedList",
            "java/util/Collections",
            "java/util/Arrays",
            "java/util/Objects",
            "java/util/Optional",
            "java/io/InputStream",
            "java/io/OutputStream",
            "java/io/PrintStream",
            "java/io/Closeable",
            "java/io/Flushable",
            "java/lang/AutoCloseable",
        ];

        // Tier 3: Exception hierarchy — needed for catch handlers in real bytecode
        let exception_classes = BOOTSTRAP_EXCEPTION_CLASSES;

        // Tier 4: Collections, concurrency, and functional interfaces
        let collections_classes = [
            "java/util/LinkedHashMap",
            "java/util/TreeMap",
            "java/util/TreeSet",
            "java/util/LinkedHashSet",
            "java/util/Properties",
            "java/util/Hashtable",
            "java/util/Vector",
            "java/util/Stack",
            "java/util/EnumMap",
            "java/util/EnumSet",
            "java/util/IdentityHashMap",
            "java/util/WeakHashMap",
            "java/util/ArrayDeque",
            "java/util/PriorityQueue",
            "java/util/BitSet",
            "java/util/StringJoiner",
            "java/util/concurrent/ConcurrentHashMap",
            "java/util/concurrent/CopyOnWriteArrayList",
            "java/util/concurrent/CopyOnWriteArraySet",
            "java/util/concurrent/atomic/AtomicInteger",
            "java/util/concurrent/atomic/AtomicLong",
            "java/util/concurrent/atomic/AtomicBoolean",
            "java/util/concurrent/atomic/AtomicReference",
            "java/util/concurrent/locks/ReentrantLock",
            "java/util/concurrent/locks/ReentrantReadWriteLock",
            "java/util/concurrent/CompletableFuture",
            "java/util/concurrent/CyclicBarrier",
            "java/util/concurrent/ForkJoinPool",
            "java/util/concurrent/ForkJoinTask",
            "java/util/concurrent/ExecutorService",
            "java/util/concurrent/ThreadPoolExecutor",
            "java/util/concurrent/Executors",
            "java/util/concurrent/locks/AbstractQueuedSynchronizer",
            "java/util/concurrent/locks/LockSupport",
            "java/util/concurrent/locks/StampedLock",
            "java/util/concurrent/locks/Lock",
            "java/util/concurrent/locks/Condition",
            "java/util/concurrent/locks/ReadWriteLock",
            "java/util/concurrent/atomic/AtomicIntegerArray",
            "java/util/concurrent/atomic/AtomicLongArray",
            "java/util/concurrent/atomic/AtomicReferenceArray",
            "java/util/concurrent/atomic/AtomicStampedReference",
            "java/util/concurrent/atomic/AtomicMarkableReference",
            "java/util/concurrent/atomic/LongAdder",
            "java/util/concurrent/atomic/DoubleAdder",
            "java/util/concurrent/atomic/LongAccumulator",
            "java/util/concurrent/atomic/DoubleAccumulator",
            "java/util/concurrent/Phaser",
        ];

        // Tier 4b: Inner-class views of core collections.
        // These are not loaded automatically because their outer class's
        // <clinit> doesn't reference them; they're allocated on-demand by
        // bytecode like `HashMap.keySet()` which calls `new KeySet()`. If
        // they're not bootstrapped, the views land with cid=0 and
        // class_id_of() reports `java/lang/Object`, breaking virtual
        // dispatch on `iterator()`, `size()`, `contains()`, etc.
        // (S111r8 — fixes Spring Boot fat-jar boot which iterates env-var
        // keysets via the URLClassLoader path.)
        let view_classes = [
            // HashMap views and iterators
            "java/util/HashMap$Node",
            "java/util/HashMap$TreeNode",
            "java/util/HashMap$KeySet",
            "java/util/HashMap$Values",
            "java/util/HashMap$EntrySet",
            "java/util/HashMap$HashIterator",
            "java/util/HashMap$KeyIterator",
            "java/util/HashMap$ValueIterator",
            "java/util/HashMap$EntryIterator",
            "java/util/HashMap$KeySpliterator",
            "java/util/HashMap$ValueSpliterator",
            "java/util/HashMap$EntrySpliterator",
            // LinkedHashMap views and iterators
            "java/util/LinkedHashMap$Entry",
            "java/util/LinkedHashMap$LinkedKeySet",
            "java/util/LinkedHashMap$LinkedValues",
            "java/util/LinkedHashMap$LinkedEntrySet",
            "java/util/LinkedHashMap$LinkedHashIterator",
            "java/util/LinkedHashMap$LinkedKeyIterator",
            "java/util/LinkedHashMap$LinkedValueIterator",
            "java/util/LinkedHashMap$LinkedEntryIterator",
            // ConcurrentHashMap views and iterators
            "java/util/concurrent/ConcurrentHashMap$Node",
            "java/util/concurrent/ConcurrentHashMap$TreeNode",
            "java/util/concurrent/ConcurrentHashMap$TreeBin",
            "java/util/concurrent/ConcurrentHashMap$KeySetView",
            "java/util/concurrent/ConcurrentHashMap$ValuesView",
            "java/util/concurrent/ConcurrentHashMap$EntrySetView",
            "java/util/concurrent/ConcurrentHashMap$Traverser",
            "java/util/concurrent/ConcurrentHashMap$BaseIterator",
            "java/util/concurrent/ConcurrentHashMap$KeyIterator",
            "java/util/concurrent/ConcurrentHashMap$ValueIterator",
            "java/util/concurrent/ConcurrentHashMap$EntryIterator",
            // TreeMap views and iterators
            "java/util/TreeMap$Entry",
            "java/util/TreeMap$KeySet",
            "java/util/TreeMap$Values",
            "java/util/TreeMap$EntrySet",
            "java/util/TreeMap$NavigableSubMap",
            "java/util/TreeMap$AscendingSubMap",
            "java/util/TreeMap$DescendingSubMap",
            "java/util/TreeMap$PrivateEntryIterator",
            "java/util/TreeMap$EntryIterator",
            "java/util/TreeMap$KeyIterator",
            "java/util/TreeMap$ValueIterator",
            "java/util/TreeMap$DescendingKeyIterator",
            // ArrayList iterator
            "java/util/ArrayList$Itr",
            "java/util/ArrayList$ListItr",
            "java/util/ArrayList$SubList",
            // LinkedList iterator
            "java/util/LinkedList$Node",
            "java/util/LinkedList$ListItr",
            "java/util/LinkedList$DescendingIterator",
            // HashSet/LinkedHashSet/TreeSet share Map's views internally
            // but Hashtable has its own.
            "java/util/Hashtable$Entry",
            "java/util/Hashtable$KeySet",
            "java/util/Hashtable$ValueCollection",
            "java/util/Hashtable$EntrySet",
            "java/util/Hashtable$Enumerator",
        ];

        // Tier 5: Functional interfaces and streams
        let functional_classes = [
            "java/util/function/Function",
            "java/util/function/Consumer",
            "java/util/function/Supplier",
            "java/util/function/Predicate",
            "java/util/function/BiFunction",
            "java/util/function/BiConsumer",
            "java/util/function/BiPredicate",
            "java/util/function/UnaryOperator",
            "java/util/function/BinaryOperator",
            "java/util/function/IntFunction",
            "java/util/function/LongFunction",
            "java/util/function/DoubleFunction",
            "java/util/function/IntConsumer",
            "java/util/function/LongConsumer",
            "java/util/function/DoubleConsumer",
            "java/util/function/IntSupplier",
            "java/util/function/LongSupplier",
            "java/util/function/DoubleSupplier",
            "java/util/function/IntPredicate",
            "java/util/function/LongPredicate",
            "java/util/function/DoublePredicate",
            "java/util/function/IntUnaryOperator",
            "java/util/function/LongUnaryOperator",
            "java/util/function/DoubleUnaryOperator",
            "java/util/function/IntBinaryOperator",
            "java/util/function/LongBinaryOperator",
            "java/util/function/DoubleBinaryOperator",
            "java/util/function/ToIntFunction",
            "java/util/function/ToLongFunction",
            "java/util/function/ToDoubleFunction",
            "java/util/stream/Stream",
            "java/util/stream/IntStream",
            "java/util/stream/LongStream",
            "java/util/stream/DoubleStream",
            "java/util/stream/Collectors",
            "java/util/stream/Collector",
            "java/util/stream/BaseStream",
            "java/util/stream/StreamSupport",
        ];

        // Tier 6: I/O and NIO
        let io_classes = [
            "java/io/BufferedInputStream",
            "java/io/BufferedOutputStream",
            "java/io/BufferedReader",
            "java/io/BufferedWriter",
            "java/io/InputStreamReader",
            "java/io/OutputStreamWriter",
            "java/io/FileInputStream",
            "java/io/FileOutputStream",
            "java/io/FileReader",
            "java/io/FileWriter",
            "java/io/Reader",
            "java/io/Writer",
            "java/io/File",
            "java/io/DataInputStream",
            "java/io/DataOutputStream",
            "java/io/ObjectInputStream",
            "java/io/ObjectOutputStream",
            "java/io/ByteArrayInputStream",
            "java/io/ByteArrayOutputStream",
            "java/io/StringReader",
            "java/io/StringWriter",
            "java/io/PrintWriter",
            "java/io/FilterInputStream",
            "java/io/FilterOutputStream",
            "java/nio/ByteBuffer",
            "java/nio/CharBuffer",
            "java/nio/Buffer",
            "java/nio/charset/Charset",
            "java/nio/charset/CharsetDecoder",
            "java/nio/charset/CharsetEncoder",
            "java/nio/charset/CodingErrorAction",
            "java/nio/charset/StandardCharsets",
            "java/nio/file/Path",
            "java/nio/file/Paths",
            "java/nio/file/Files",
        ];

        // Tier 7: Internal VM support classes
        let internal_classes = [
            "jdk/internal/misc/Unsafe",
            "jdk/internal/misc/VM",
            "jdk/internal/misc/Signal",
            "jdk/internal/misc/SharedSecrets",
            "jdk/internal/access/SharedSecrets",
            "sun/misc/Unsafe",
            "java/lang/invoke/MethodHandle",
            "java/lang/invoke/MethodHandles",
            "java/lang/invoke/MethodType",
            "java/lang/invoke/CallSite",
            "java/lang/invoke/ConstantCallSite",
            "java/lang/invoke/MutableCallSite",
            "java/lang/invoke/VolatileCallSite",
            "java/lang/invoke/LambdaMetafactory",
            "java/lang/invoke/StringConcatFactory",
            "java/lang/annotation/Annotation",
            "java/lang/annotation/Retention",
            "java/lang/annotation/Target",
            "java/lang/annotation/ElementType",
            "java/lang/annotation/RetentionPolicy",
            "java/lang/annotation/Documented",
            "java/lang/annotation/Inherited",
            "java/util/Locale",
            "java/util/Currency",
            "java/util/Date",
            "java/util/Calendar",
            "java/util/TimeZone",
            "java/util/UUID",
            "java/util/regex/Pattern",
            "java/util/regex/Matcher",
            "java/util/Formatter",
            "java/text/DecimalFormat",
            "java/text/MessageFormat",
            "java/text/NumberFormat",
            "java/text/SimpleDateFormat",
            "java/math/BigInteger",
            "java/math/BigDecimal",
            "java/math/MathContext",
            "java/math/RoundingMode",
            "java/net/URL",
            "java/net/URI",
            "java/security/AccessController",
            "java/security/PrivilegedAction",
            "java/security/Permission",
        ];

        let fast_boot = self.compatibility_mode == CompatibilityMode::Compatible;

        let is_non_essential_compat_boot_class = |name: &str| -> bool {
            matches!(
                name,
                "java/text/DecimalFormat"
                    | "java/text/MessageFormat"
                    | "java/text/NumberFormat"
                    | "java/text/SimpleDateFormat"
                    | "java/math/BigInteger"
                    | "java/math/BigDecimal"
                    | "java/math/MathContext"
                    | "java/math/RoundingMode"
                    | "java/util/Calendar"
                    | "java/util/TimeZone"
                    | "java/util/Currency"
                    | "java/util/UUID"
                    | "java/util/Formatter"
                    | "java/util/concurrent/CompletableFuture"
                    | "java/util/concurrent/CyclicBarrier"
                    | "java/util/concurrent/ForkJoinPool"
                    | "java/util/concurrent/ForkJoinTask"
                    | "java/util/concurrent/ThreadPoolExecutor"
                    | "java/util/concurrent/Executors"
                    | "java/util/concurrent/locks/StampedLock"
                    | "java/util/concurrent/locks/ReentrantReadWriteLock"
                    | "java/util/concurrent/atomic/AtomicIntegerArray"
                    | "java/util/concurrent/atomic/AtomicLongArray"
                    | "java/util/concurrent/atomic/AtomicReferenceArray"
                    | "java/util/concurrent/atomic/AtomicStampedReference"
                    | "java/util/concurrent/atomic/AtomicMarkableReference"
                    | "java/util/concurrent/atomic/LongAdder"
                    | "java/util/concurrent/atomic/DoubleAdder"
                    | "java/util/concurrent/atomic/LongAccumulator"
                    | "java/util/concurrent/atomic/DoubleAccumulator"
                    | "java/util/concurrent/Phaser"
                    | "java/util/concurrent/CopyOnWriteArrayList"
                    | "java/util/concurrent/CopyOnWriteArraySet"
                    | "java/io/ObjectInputStream"
                    | "java/io/ObjectOutputStream"
                    | "java/io/DataInputStream"
                    | "java/io/DataOutputStream"
                    | "java/io/PrintWriter"
                    | "java/io/StringReader"
                    | "java/io/StringWriter"
                    | "java/util/BitSet"
                    | "java/util/PriorityQueue"
                    | "java/util/ArrayDeque"
                    | "java/util/Stack"
                    | "java/util/Vector"
                    | "java/util/Hashtable"
                    | "java/util/EnumMap"
                    | "java/util/EnumSet"
                    | "java/util/Hashtable$Entry"
                    | "java/util/Hashtable$KeySet"
                    | "java/util/Hashtable$ValueCollection"
                    | "java/util/Hashtable$EntrySet"
                    | "java/util/Hashtable$Enumerator"
                    | "java/util/concurrent/ConcurrentHashMap$Traverser"
                    | "java/util/concurrent/ConcurrentHashMap$EntryIterator"
                    | "java/util/TreeMap$DescendingKeyIterator"
                    | "java/util/TreeMap$DescendingSubMap"
                    | "java/util/TreeMap$AscendingSubMap"
                    | "java/util/TreeMap$NavigableSubMap"
                    | "java/util/stream/IntStream"
                    | "java/util/stream/LongStream"
                    | "java/util/stream/DoubleStream"
                    | "java/util/function/DoubleFunction"
                    | "java/util/function/DoubleConsumer"
                    | "java/util/function/DoubleSupplier"
                    | "java/util/function/DoublePredicate"
                    | "java/util/function/DoubleUnaryOperator"
                    | "java/util/function/DoubleBinaryOperator"
            )
        };

        let mut loaded = 0;
        for name in core_classes
            .iter()
            .chain(extended_classes.iter())
            .chain(exception_classes.iter())
            .chain(collections_classes.iter())
            .chain(view_classes.iter())
            .chain(functional_classes.iter())
            .chain(io_classes.iter())
            .chain(internal_classes.iter())
        {
            if fast_boot && is_non_essential_compat_boot_class(name) {
                continue;
            }
            match self.load_class(name) {
                Ok(class_id) => {
                    let is_real = self
                        .class_store
                        .get(class_id)
                        .map(|c| !c.origin.is_compatibility_stub())
                        .unwrap_or(false);
                    if is_real {
                        loaded += 1;
                        debug!(class = name, id = %class_id, "Bootstrap: loaded real class");
                    } else {
                        debug!(class = name, "Bootstrap: loaded as synthetic stub");
                    }
                }
                Err(e) => {
                    debug!(class = name, error = %e, "Bootstrap: failed to load");
                }
            }
        }

        if loaded > 0 {
            // Log summary
            let total = self.loaded_count();
            debug!(
                "Bootstrap complete: {} core classes loaded ({} total classes in ClassStore)",
                loaded, total
            );
        }

        loaded
    }

    /// Load a class by its binary name (e.g. `"java/lang/Object"`).
    ///
    /// Uses the parent delegation model:
    /// 1. Check if already loaded by any loader
    /// 2. Ask bootstrap → extension → application to find the class
    /// 3. Parse, recursively load superclass/interfaces, and register
    /// Loader-faithful fast-path lookup for the requester-less, pure
    /// parent-delegation loaders (`load_class` / `SharedVm::load_class_concurrent`).
    ///
    /// Prefers [`Self::get_loaded_class_id_for_requester`] with `Application`
    /// as the requester, which probes only the built-in delegation chain
    /// (bootstrap -> extension -> application) -- exactly the set of
    /// answers `find_class_bytes_delegated` (the real, from-classpath slow
    /// path these callers fall back to) can itself ever produce.
    ///
    /// Runtime-package-identity bug fix: the bare [`Self::get_loaded_class_id`]
    /// additionally falls back to an arbitrary lone user-defined loader's own
    /// copy of `name` when no built-in loader has defined it (see that fn's
    /// "context.groovy fix" doc comment) -- the right default for genuinely
    /// requester-less reflection-style lookups, but unsound as this
    /// function's PRIMARY answer: a class that legitimately exists on the
    /// real classpath (so `find_class_bytes_delegated` would find it) must
    /// resolve to its own freshly-loaded built-in-loader copy, never to an
    /// unrelated user-defined loader's redefinition that happens to share
    /// the name (JVMS §5.3: defining loader is part of a class's identity).
    /// Concretely: Spring's `TestCompiler` (Application loader) executing
    /// `new TestCompiler$Problems()` byte-code must resolve to the ordinary
    /// Application-classpath `Problems`, never to a *different*
    /// `TestCompiler$Problems` that some earlier, unrelated
    /// `@CompileWithForkedClassLoader` test forked into its own
    /// `DynamicClassLoader` in the same run.
    ///
    /// That said, the lone-user-loader fallback is also the ONLY way
    /// purely in-memory, never-backed-by-a-`.class`-file classes (e.g.
    /// `java.lang.reflect.Proxy`-generated annotation proxies) are ever
    /// re-resolved by name after their first definition -- there is no
    /// classpath scan that could find them. So the fallback is used here
    /// too, but only as a LAST RESORT, gated on `find_class_bytes_delegated`
    /// genuinely failing to find real bytes for `name` -- i.e. only when no
    /// better, classpath-backed answer could possibly exist.
    pub fn resolve_fast_path_class_id(&self, name: &str) -> Option<ClassId> {
        let dbg = loader_flags().dbg_dupclass
            && loader_flags()
                .dbg_dupclass_filter
                .as_deref()
                .is_none_or(|f| name.contains(f));
        if let Some(id) = self.get_loaded_class_id_for_requester(name, ClassLoaderId::Application) {
            if dbg {
                eprintln!(
                    "[DBG_DUPCLASS] builtin-chain hit {:?} (loader={:?}) for {:?}",
                    id,
                    self.class_store.get(id).map(|c| c.loader_id),
                    name,
                );
            }
            return Some(id);
        }
        let Some(candidate) = self.get_loaded_class_id(name) else {
            if dbg {
                eprintln!("[DBG_DUPCLASS] no loaded candidate at all for {name:?}");
                if loader_flags().dbg_dupclass_bt {
                    eprintln!(
                        "[DBG_DUPCLASS_BT] {name}\n{}",
                        std::backtrace::Backtrace::force_capture()
                    );
                }
            }
            return None;
        };
        let is_user_loader_answer = matches!(
            self.class_store.get(candidate).map(|c| c.loader_id),
            Some(ClassLoaderId::UserDefined(_))
        );
        if dbg {
            eprintln!(
                "[DBG_DUPCLASS] fallback candidate {:?} (loader={:?}, is_user={is_user_loader_answer}) for {:?}; delegated_bytes_found={}",
                candidate,
                self.class_store.get(candidate).map(|c| c.loader_id),
                name,
                self.find_class_bytes_delegated(name).is_ok(),
            );
        }
        if is_user_loader_answer && self.find_class_bytes_delegated(name).is_err() {
            return Some(candidate);
        }
        if dbg {
            eprintln!(
                "[DBG_DUPCLASS] rejecting existing UserDefined-loader candidate {:?} (loader={:?}) for {:?} -- delegation chain also has it, so a SEPARATE ClassId will be created under Application",
                candidate,
                self.class_store.get(candidate).map(|c| c.loader_id),
                name,
            );
            if loader_flags().dbg_dupclass_bt {
                let bt = std::backtrace::Backtrace::force_capture();
                eprintln!("[DBG_DUPCLASS_BT] {name}\n{bt}");            }        }
        None
    }

    /// Would [`Self::load_class`] answer `name` with a *fabricated synthetic
    /// stub* rather than a real class?
    ///
    /// A synthetic stub has no `Code` on any method: it exists only so that
    /// WildFly/Quarkus-style bytecode can LINK against enterprise classes that
    /// are genuinely absent. Answering with one when the class merely lives
    /// behind a custom `ClassLoader` (invisible to this process's own `-cp`)
    /// is a silent mis-resolution -- and a destructive one, because the stub is
    /// registered globally under `Application`, so the real loader can never
    /// define its own copy afterwards.
    ///
    /// Callers that DO have a loader context (see
    /// `NativeContextImpl::load_class`) use this to consult that loader
    /// *before* the fabrication happens. Mirrors `load_class`'s own fallback
    /// branches exactly, so a `true` answer is precise, not a guess.
    pub fn would_fabricate_synthetic_stub(&self, name: &str) -> bool {
        if name.starts_with('[') {
            return false;
        }
        if !self.is_jdk_or_enterprise_stub(name) {
            return false;
        }
        if name.contains("$$") || is_jboss_logging_locale_lookup(name) {
            return false;
        }
        // Must mirror `load_class`'s `package-info` gate below, or callers that
        // pre-check this predicate would believe a stub is coming when the load
        // will in fact raise ClassNotFound.
        if name == "package-info" || name.ends_with("/package-info") {
            return false;
        }
        if is_standard_jdk_namespace(name)
            && self.has_real_boot_classes()
            && !is_native_backed_jdk_stub(name)
        {
            return false;
        }
        if self.resolve_fast_path_class_id(name).is_some() {
            return false;
        }
        self.find_class_bytes_delegated(name).is_err()
    }

    /// Whether [`Self::load_class`] has anything to answer `name` (a non-array
    /// name) with other than a fabricated compatibility stub: a class the
    /// fast path already resolves, a staged transformed class file, or bytes
    /// on the built-in delegation chain. Read-only (no define, no stub, no
    /// heap object); `false` means the global route can only fail.
    ///
    /// The `--real-jdk` class-resolution door asks it after an
    /// application-parented loader whose `loadClass` is CratonVM's
    /// base-delegation native threw a `ClassNotFoundException`: when the
    /// global route cannot answer either, the resolution fails whatever
    /// happens next, and the loader's exception is HotSpot's cause for it
    /// (interpreter round i1 wave 41, lane L5; probe
    /// `tools/probes/interp/L5/L5W41AppParentMissCause.java`). Cold: once
    /// per failing entry (the failure is recorded against it).
    pub fn global_route_may_answer(&self, name: &str) -> bool {
        self.resolve_fast_path_class_id(name).is_some()
            || self.pending_transformed_classes.contains_key(name)
            || self.find_class_bytes_delegated(name).is_ok()
    }

    pub fn load_class(&mut self, name: &str) -> Result<ClassId, VmError> {
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::ClassLoad);
        if loader_flags().dbg_loadclass && name.contains("GroupsMetadata") {
            let bt = std::backtrace::Backtrace::force_capture();
            eprintln!(
                "[DBG_LOADCLASS] load_class({name}) already_loaded={:?}\n{bt}",
                self.get_loaded_class_id(name)
            );
        }
        // RKC16N.3: Reference- and primitive-array classes (`[X`) are
        // *synthesised* by the VM directly from the resolved component
        // class — JVMS §5.3.3 explicitly says no class file is consulted.
        // Short-circuit before any I/O so that
        // `Class.forName("[Ljava/util/HashMap;")` succeeds without
        // scanning JMOD/classpath and without producing the
        // "synthetic stub" warning.
        //
        // This entry point carries no requesting loader, so it asks as the
        // bootstrap loader would. See `load_array_class_for_loader` for the
        // loader-faithful entry point and for why that keeps this arm's
        // observable behaviour unchanged.
        if name.starts_with('[') {
            return self.load_array_class_for_loader(name, ClassLoaderId::Bootstrap);
        }
        // Fast path: loader-aware lookup via loaded_classes. See
        // `resolve_fast_path_class_id`'s doc comment for the runtime-
        // package-identity bug this guards against.
        if let Some(id) = self.resolve_fast_path_class_id(name) {
            // If the class is a synthetic stub (no methods, no bytecode), try
            // to upgrade it to a real class from the classpath. This handles
            // the case where wrapper types like java/lang/Boolean are created
            // as synthetic stubs during early bootstrap but later need their
            // real bytecode methods (e.g. parseBoolean).
            let is_synthetic = self
                .class_store
                .get(id)
                .map(|c| c.origin.is_compatibility_stub())
                .unwrap_or(false);
            // HIB-DEV-03: skip the full-classpath upgrade rescan once the real
            // `.class` is known absent (re-armed on classpath extension) so a
            // repeatedly-loaded synthetic stub doesn't re-scan every JAR.
            if is_synthetic && !name.starts_with('[') && !self.synthetic_upgrade_known_absent(name)
            {
                match self.find_class_bytes_delegated(name) {
                    Ok((bytes, loader_id)) => {
                        // zt t20/pl9: the real define of a class-path class.
                        self.note_class_path_define(name, loader_id, bytes.len());
                        match self.upgrade_synthetic_class(id, name, bytes, loader_id) {
                            Ok(()) => {
                                tracing::debug!(
                                    class = name,
                                    "Upgraded synthetic stub to real class"
                                );
                            }
                            Err(e) => {
                                tracing::debug!(
                                    class = name,
                                    "Failed to upgrade synthetic stub: {e:?}"
                                );
                            }
                        }
                    }
                    Err(_) => self.note_synthetic_upgrade_absent(name),
                }
            }
            return Ok(id);
        }

        // Circular dependency guard: if this class is already being loaded
        // by a recursive call (e.g. A extends B extends A), reject it.
        // JVMS §5.3.5 steps 3-4 name the error: `ClassCircularityError`.
        if self.loading_guard.contains(name) {
            return Err(VmError::Linkage(LinkageError::ClassCircularityError {
                class_name: name.to_string(),
            }));
        }

        debug!(class = name, "Loading class (parent delegation)");

        // A `java.lang.instrument` agent already rewrote this class file, one
        // level up, off the lock (see `pending_transformed_classes`). Those
        // bytes replace what parent delegation would have read — that IS the
        // transform landing — and everything downstream (parse, class-bytes
        // cache, ClassLoad/ClassPrepare) sees only the final definition, so the
        // agent's view and the VM's view cannot diverge.
        if let Some((bytes, loader_id, retransform_base)) =
            self.pending_transformed_classes.remove(name)
        {
            // A retransform-capable transformer changed the class file: the
            // cached bytes (the retransformation base) are the ones it was
            // handed, not the definition (see `pending_transformed_classes`).
            // Otherwise the base is the staged (transformed) file itself.
            // Either way the class path cannot give it back, so it is pinned
            // out of the FIFO's reach (interpreter round i1 wave 28, lane L3;
            // `class_bytes_pinned`).
            let raw_agent_base = retransform_base.is_some();
            let base = retransform_base.unwrap_or_else(|| bytes.clone());
            // zt t20/pl9: HotSpot counts the define's own length, read before
            // the transformers run; the base is the nearest figure kept.
            self.note_class_path_define(name, loader_id, base.len());
            let cede = self.pending_native_shadow_cedes.remove(name);
            let defined = self.define_class_shared_with_options(
                name,
                bytes,
                loader_id,
                DefineClassOptions::default(),
            );
            if let Ok(id) = &defined {
                if raw_agent_base { self.insert_raw_agent_base(*id, base); }
                else { self.insert_class_bytes_pinned(*id, base); }
                if cede {
                    self.cede_native_shadows_to_woven_bytecode(*id);
                }
            }
            return defined;
        }

        // Parent delegation: try bootstrap → extension → application
        //
        // zt pl4 item 1: `sun.cls.sysClassLoadTime` times a boot class's read
        // and parse, HotSpot's `PerfTraceTime` around `ClassLoader::load_class`
        // (kept only when the boot loader found the class outside the CDS
        // cache; it ends when this arm returns).
        let mut sys_load =
            Some(cratonvm_types::perf_class_trace::SysClassLoadTime::begin(self.class_trace.clone()));
        let found = self.find_class_bytes_delegated(name);
        if !matches!(found, Ok((_, ClassLoaderId::Bootstrap)))
            || (!self.cds_class_cache.is_empty() && self.cds_class_cache.contains_key(name))
            || self.image_class_java_loader(name, ClassLoaderId::Bootstrap).is_some()
        {
            if let Some(timer) = sys_load.take() {
                timer.discard();
            }
        }
        match found {
            Ok((bytes, loader_id)) => {
                // zt t20/pl9: counted before the parse, as HotSpot's
                // `jvm_define_class_common` counts a define that then fails.
                self.note_class_path_define(name, loader_id, bytes.len());
                // zt pl4 item 1: an application-loader class-path class is
                // the `CLASS_LOAD` event's answer when the VM asked for it
                // (HotSpot's upcall to the loader's `loadClass`; a Java
                // `loadClass` is no event), and its define the
                // `DEFINE_CLASS` event the upcall's `defineClass` makes
                // (`defineAppClassTime`), nested in it.
                // A platform- or tool-module image class is the platform /
                // application loader's define too (`image_class_java_loader`),
                // reached through a Java delegation, not the VM's upcall.
                let app = loader_id == ClassLoaderId::Application;
                let java_define = app
                    || loader_id == ClassLoaderId::Extension
                    || self.image_class_java_loader(name, loader_id).is_some();
                let _class_load = (app
                    && !cratonvm_types::perf_class_trace::java_initiated_load())
                .then(|| {
                    cratonvm_types::perf_class_trace::ClassTraceTime::begin(
                        self.class_trace.clone(),
                        cratonvm_types::perf_class_trace::ClassTraceEvent::ClassLoad,
                    )
                });
                let _define = java_define.then(|| {
                    cratonvm_types::perf_class_trace::ClassTraceTime::begin(
                        self.class_trace.clone(),
                        cratonvm_types::perf_class_trace::ClassTraceEvent::DefineClass,
                    )
                });
                // Parse and register with the loader that found it
                let refetchable = (loader_id == ClassLoaderId::Bootstrap
                    && loader_flags().boot_class_bytes_refetch)
                    .then(|| bytes.clone());
                let defined = self.define_class_shared_with_options(
                    name,
                    bytes,
                    loader_id,
                    DefineClassOptions::default(),
                );
                // zr z11/md: the boot loader's own class path gives these
                // bytes back on demand (`defined_class_bytes`), as HotSpot
                // keeps no class file for them.
                if let (Ok(id), Some(found)) = (&defined, refetchable) {
                    self.release_refetchable_class_bytes(*id, &found);
                }
                defined
            }
            Err(_) if is_jboss_logging_locale_lookup(name) => {
                // S-trinity #3: JBoss Logging i18n probes locale-specific
                // implementation classes (`_$logger_<locale>` /
                // `_$bundle_<locale>`) inside a try/catch
                // (ClassNotFoundException) and falls back to the locale-less
                // `_$logger` / `_$bundle` when the probe fails. Our
                // `is_jdk_class("org/jboss/...")` returns true for these
                // names, so without this branch we would synthesize a stub
                // — and `create_synthetic_stub`'s heuristic flags any
                // name containing `$` as an interface, which then fails
                // `Class.asSubclass(ServerLogger.class)` with a CCE that
                // escapes the JBoss-Logging CNFE catch.
                Err(VmError::ClassFile(ClassFileError::ClassNotFound {
                    class_name: name.to_string(),
                }))
            }
            Err(_) if self.is_jdk_or_enterprise_stub(name) => {
                // In real-JDK mode the boot jimage/jmods are authoritative for
                // the standard JDK namespaces (java/, javax/, sun/, jdk/,
                // com/sun/). A delegated miss there means the class genuinely
                // does not exist, so we must surface ClassNotFoundException
                // instead of fabricating a stub — otherwise `Class.forName`
                // existence probes wrongly succeed (e.g. jakarta.el.ImportHandler
                // resolving the simple name `ArrayList` against both java.util.*
                // and java.net.* sees a bogus `java.net.ArrayList` and reports a
                // spurious import conflict). Stubs are still created for array
                // classes, the enterprise app prefixes that legitimately use the
                // stub mechanism even with a real JDK, and all of synthetic-JDK
                // mode. Internal synthetic helpers (`java/util/Enumeration$Impl`,
                // …) reach the class store via `ensure_synthetic_class`, which
                // registers directly when this load fails — so the CNFE here
                // does not break them.
                if is_standard_jdk_namespace(name)
                    && self.has_real_boot_classes()
                    && !is_native_backed_jdk_stub(name)
                {
                    return Err(VmError::ClassFile(ClassFileError::ClassNotFound {
                        class_name: name.to_string(),
                    }));
                }
                // BUG-06 — a reflective `Class.forName` / `ClassUtils.isPresent`
                // existence probe must report an enterprise-framework class that
                // is not on the classpath as ABSENT, exactly as HotSpot does.
                // The synthetic stub fabricated below exists only to satisfy
                // WildFly/Quarkus *bytecode* linkage; letting it satisfy a
                // reflective probe is a false positive. Spring's
                // ReactiveAdapterRegistry probes `io.smallrye.mutiny.Multi` to
                // decide whether to register its MutinyRegistrar, whose
                // <clinit> then dies on the incomplete stub — an
                // ExceptionInInitializerError that cascades across ~20
                // reactive-messaging / RSocket test classes. Only the reflective
                // path is gated (the probe flag is set by the Class.forName
                // native); genuine constant-pool resolution still gets its stub.
                if self.is_enterprise_stub_prefix(name) && cratonvm_types::reflective_probe::active() {
                    return Err(VmError::ClassFile(ClassFileError::ClassNotFound {
                        class_name: name.to_string(),
                    }));
                }
                // A name containing "$$" is the universal marker JVM bytecode
                // generators use for a runtime-synthesized implementation that
                // is never shipped as a `.class` file — SmallRye Config's
                // `@ConfigMapping` `<Iface>$$CMImpl` (io.smallrye.config.
                // ConfigMappingLoader + io.smallrye.common.classloader.
                // ClassDefiner), CGLIB's `$$EnhancerBy...$$`/`$$FastClassBy...$$`,
                // ByteBuddy, Mockito's `$MockitoMock$`, etc. These generators all
                // use the same idiom: try `ClassLoader.loadClass(generatedName)`
                // first (a cheap check for an already-generated class earlier in
                // the same run), catch `ClassNotFoundException`, and only then
                // generate + `Lookup.defineClass`/`Unsafe.defineClass` the real
                // bytecode. Fabricating an enterprise-prefix synthetic stub here
                // (as below, for genuinely-missing-jar classes) hands that probe
                // a bogus non-null `Class` instead of the CNFE it needs to ever
                // reach the generation step — the real class is never produced,
                // and any later reference to the same name is permanently stuck
                // on the wrong stub. Unlike the reflective-probe gate above, this
                // must fire unconditionally: the generators call plain
                // `ClassLoader.loadClass`, which never sets that flag. A real,
                // non-generated class name essentially never contains "$$", so
                // this can't misclassify a genuine missing-jar case.
                if name.contains("$$") {
                    return Err(VmError::ClassFile(ClassFileError::ClassNotFound {
                        class_name: name.to_string(),
                    }));
                }
                // `package-info` is a compiler-generated annotation carrier that
                // exists ONLY when the package actually declares annotations.
                // `java.lang.Package.getPackageInfo()` probes for it with
                // `Class.forName(pkg + ".package-info", false, loader)` inside a
                // `catch (ClassNotFoundException)`, so an absent one is the
                // normal case, not a missing jar. Fabricating a stub answers
                // that probe with a bogus non-null `Class` (HotSpot throws), and
                // it accounted for most of the enterprise-prefix stub fallbacks
                // on the Keycloak 26.6.1 boot (`io/quarkus/arc/impl/package-info`,
                // `io/agroal/narayana/package-info`, four `org/infinispan/**`).
                if name == "package-info" || name.ends_with("/package-info") {
                    return Err(VmError::ClassFile(ClassFileError::ClassNotFound {
                        class_name: name.to_string(),
                    }));
                }
                // JDK class not found as a .class file — create a synthetic stub.
                // Our VM handles JDK classes natively, so we just need a minimal
                // entry in the ClassStore for the type system to work.
                debug!(
                    class = name,
                    "Falling back to synthetic stub — class not found in any classpath"
                );
                if trace_stub_fallback() {
                    eprintln!(
                        "[cratonvm] stub fallback: {name} — not found on any classpath entry (enterprise-prefix stub; add the missing jar)"
                    );
                }
                // `CRATONVM_DBG_STUB_BT=<substring>` -- Rust backtrace at the
                // point a synthetic stub is fabricated for a matching name.
                // The stub itself is silent until something tries to *run* it
                // (`VerifyError: ... must have Code attribute`), by which
                // point the resolver that asked for it is long gone from the
                // stack -- this names it.
                if let Some(want) = dbg_stub_bt_filter() {
                    if !want.is_empty() && name.contains(want) {
                        let bt = std::backtrace::Backtrace::force_capture();
                        eprintln!("[DBG_STUB_BT] synthetic stub for {name}\n{bt}");
                    }
                }
                self.create_synthetic_stub(name)
            }
            Err(e) => Err(e),
        }
    }

    /// Adjudicate a whole registry's worth of `(class, name, descriptor)`
    /// triples against the bytes on the class path.
    ///
    /// ## Why this exists next to the census's `real_declaring_method`
    ///
    /// That column answers from the **loaded** class store, so it is a
    /// measurement of the run: `loaded: false` means "this workload never
    /// touched the class". That is the honest answer to the question it asks,
    /// and it is the wrong instrument for adjudicating ~8,000 registrations,
    /// because the ones most in need of a verdict are precisely the ones no
    /// single workload exercises. `docs/known-issues/jdk-only/`'s sixteen
    /// `JDK-ONLY-CLASSIFY: unknown — needs census` verdicts are all blocked on
    /// this distinction.
    ///
    /// ## Why it does not just load the classes
    ///
    /// Force-loading every registered name through [`Self::load_class`] would
    /// **fabricate a synthetic stub for every name the image lacks** (see
    /// `would_fabricate_synthetic_stub`), which is both a large perturbation of
    /// the thing being measured and, under `JdkOnly`, several hundred recorded
    /// violations manufactured by the measurement itself. This reads bytes and
    /// parses them; nothing is defined, no `ClassId` is allocated, no
    /// `<clinit>` runs, and `&self` makes that structural rather than a promise.
    ///
    /// Parses each distinct class **once**, not once per registration — the
    /// registry has thousands of rows over roughly a thousand classes. A class
    /// whose bytes are present but unparseable is reported as
    /// `image_has_class: true` with everything else `false`, which is
    /// deliberately indistinguishable from "declares no such method": both mean
    /// "the image does not give this registration an `ACC_NATIVE` target", and
    /// that is the only question this answers.
    pub fn adjudicate_natives_against_image(
        &self,
        triples: &[(String, String, String)],
    ) -> Vec<ImageMethodVerdict> {
        let mut parsed: FxHashMap<String, Option<ImageClassShape>> = FxHashMap::default();

        triples
            .iter()
            .map(|(class, name, descriptor)| {
                let Some(shape) = self.image_class_shape(&mut parsed, class) else {
                    return ImageMethodVerdict::absent();
                };
                if let Some((acc_native, has_code)) = shape.find(name, descriptor) {
                    return ImageMethodVerdict {
                        image_has_class: true,
                        declared: true,
                        acc_native,
                        has_code,
                        inherited_from: None,
                        inherited_acc_native: false,
                        inherited_has_code: false,
                        inherited_abstract: false,
                    };
                }
                // Not declared here. Ask the hierarchy, in the order both JVMS
                // §5.4.3.3 and CratonVM's dispatch use: superclass chain, then
                // superinterfaces.
                //
                // EXCEPT for the two initialization methods, which are not
                // inherited and must not be resolved up the hierarchy. JVMS
                // §6.5 `invokespecial`: "if the resolved method is an instance
                // initialization method, and the class in which it is declared
                // is not the class symbolically referenced by the instruction,
                // a `NoSuchMethodError` is thrown" — so a `<init>` found on a
                // SUPERTYPE is not a target this registration could ever yield
                // to. `<clinit>` is never resolved by name at all.
                //
                // This is not a nicety: `inherited_has_code` is what puts a row
                // in the `--real-jdk` campaign's bucket B, i.e. in the
                // population a retirement wave may refuse. MEASURED on JDK 25,
                // 2026-09-10: **31 registrations** carried an inherited `<init>`
                // verdict, 23 of them live `Bridge` rows in the goal
                // population, and 17 of those inherited `java/lang/Object`'s
                // no-arg constructor — which initialises nothing. Retiring one
                // would have refused a native that populates a VM-minted
                // receiver's fields and yielded to a body that writes none of
                // them: a silent field-init loss on
                // `java/lang/management/*MXBean`, `HttpURLConnection`,
                // `SSLEngineImpl` and `MBeanServer`. They are bucket F —
                // "class present, method absent" — and always were.
                let inherited = if name == "<init>" || name == "<clinit>" {
                    None
                } else {
                    self.resolve_in_image_hierarchy(&mut parsed, class, name, descriptor)
                };
                match inherited {
                    Some((declarer, acc_native, has_code)) => ImageMethodVerdict {
                        image_has_class: true,
                        declared: false,
                        acc_native: false,
                        has_code: false,
                        inherited_acc_native: acc_native,
                        inherited_has_code: has_code,
                        inherited_abstract: !acc_native && !has_code,
                        inherited_from: Some(declarer),
                    },
                    None => ImageMethodVerdict {
                        image_has_class: true,
                        declared: false,
                        acc_native: false,
                        has_code: false,
                        inherited_from: None,
                        inherited_acc_native: false,
                        inherited_has_code: false,
                        inherited_abstract: false,
                    },
                }
            })
            .collect()
    }

    /// Parse `class`'s image bytes once and memoise the shape the adjudication
    /// needs. `None` means the image has no bytes for the name; a parse failure
    /// yields a shape with no methods and no supertypes, which is deliberately
    /// indistinguishable from "declares nothing and extends nothing" — both mean
    /// "the image gives this registration no target", the only question asked.
    pub(in crate::class_manager) fn image_class_shape<'a>(
        &self,
        parsed: &'a mut FxHashMap<String, Option<ImageClassShape>>,
        class: &str,
    ) -> Option<&'a ImageClassShape> {
        use std::collections::hash_map::Entry;
        let entry = match parsed.entry(class.to_string()) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => {
                let decoded = self
                    .find_class_bytes_delegated(class)
                    .ok()
                    .map(
                        |(bytes, _)| match cratonvm_reader::class_reader::read_class(&bytes) {
                            Ok(cf) => ImageClassShape {
                                methods: cf
                                    .methods
                                    .iter()
                                    .map(|m| {
                                        (
                                            (m.name.clone(), m.descriptor.clone()),
                                            (m.is_native(), !m.is_native() && !m.is_abstract()),
                                        )
                                    })
                                    .collect(),
                                super_class: cf.super_class.clone(),
                                interfaces: cf.interfaces.clone(),
                            },
                            Err(_) => ImageClassShape::default(),
                        },
                    );
                v.insert(decoded)
            }
        };
        entry.as_ref()
    }

    /// Find the SUPERTYPE declaration of `(name, descriptor)` for a class that
    /// does not declare it itself. Returns `(declaring class, acc_native,
    /// has_code)`.
    ///
    /// Superclasses first and interfaces after, breadth-first, which is JVMS
    /// §5.4.3.3's order and also the order a receiver-driven dispatch would
    /// reach them. Bounded by [`IMAGE_HIERARCHY_MAX_DEPTH`] *and* by a visited
    /// set, so neither a cycle nor a diamond can make this quadratic.
    pub(in crate::class_manager) fn resolve_in_image_hierarchy(
        &self,
        parsed: &mut FxHashMap<String, Option<ImageClassShape>>,
        class: &str,
        name: &str,
        descriptor: &str,
    ) -> Option<(Arc<str>, bool, bool)> {
        // Superclass chain first: a concrete override on a superclass wins over
        // an abstract interface declaration, which is what dispatch does too.
        let mut visited: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
        visited.insert(class.to_string());
        let mut interface_queue: Vec<Arc<str>> = self
            .image_class_shape(parsed, class)
            .map(|s| s.interfaces.clone())
            .unwrap_or_default();

        let mut current: Option<Arc<str>> = self
            .image_class_shape(parsed, class)
            .and_then(|s| s.super_class.clone());
        let mut depth = 0usize;
        while let Some(sup) = current {
            depth += 1;
            if depth > IMAGE_HIERARCHY_MAX_DEPTH || !visited.insert(sup.to_string()) {
                break;
            }
            let Some(shape) = self.image_class_shape(parsed, &sup) else {
                break;
            };
            let found = shape.find(name, descriptor);
            let next = shape.super_class.clone();
            interface_queue.extend(shape.interfaces.iter().cloned());
            if let Some((acc_native, has_code)) = found {
                return Some((sup, acc_native, has_code));
            }
            current = next;
        }

        // Then superinterfaces, breadth-first.
        let mut head = 0usize;
        while head < interface_queue.len() && head < IMAGE_HIERARCHY_MAX_DEPTH * 64 {
            let iface = interface_queue[head].clone();
            head += 1;
            if !visited.insert(iface.to_string()) {
                continue;
            }
            let Some(shape) = self.image_class_shape(parsed, &iface) else {
                continue;
            };
            let found = shape.find(name, descriptor);
            interface_queue.extend(shape.interfaces.iter().cloned());
            if let Some((acc_native, has_code)) = found {
                return Some((iface, acc_native, has_code));            }        }
        None
    }

    /// Find class bytes using parent delegation.
    /// Locate `name`'s class file through the built-in delegation chain
    /// (bootstrap → extension → application) **without defining it**.
    ///
    /// The `java.lang.instrument` load-time transform needs the raw bytes and
    /// the loader that would own them before it can offer them to an agent, and
    /// it must do that with no class-manager lock held (see
    /// [`Self::pending_transformed_classes`]). This is that read-only half of
    /// [`Self::load_class`], exposed for exactly that caller; it takes `&self`,
    /// so it can run under the read lock.
    pub fn find_class_bytes_for_transform(
        &self,
        name: &str,
    ) -> Result<(Vec<u8>, ClassLoaderId), VmError> {
        let (bytes, loader_id) = self.find_class_bytes_delegated(name)?;
        Ok((bytes.to_vec(), loader_id))
    }

    /// Stage agent-transformed bytes for the next load of `name`.
    ///
    /// See [`Self::pending_transformed_classes`]. The caller has already run the
    /// transformer chain; this hands the result to the load that will ask for
    /// it. A second stage for the same name overwrites the first — the last
    /// transform wins, which is the only ordering a single load can observe.
    /// `retransform_base`: the class file before the first retransform-capable
    /// transformer changed it, when one did (`None`: `bytes` is the base).
    pub fn stage_transformed_class(
        &mut self,
        name: &str,
        bytes: Vec<u8>,
        loader_id: ClassLoaderId,
        retransform_base: Option<Vec<u8>>,
    ) {
        self.pending_transformed_classes.insert(
            name.to_string(),
            (bytes.into(), loader_id, retransform_base.map(Into::into)),
        );
    }

    /// Drop a staged entry without defining it. Used when the transform path
    /// bails after staging (an aborted load must not leave bytes behind for an
    /// unrelated later load of the same name to pick up).
    pub fn discard_staged_transformed_class(&mut self, name: &str) {
        self.pending_transformed_classes.remove(name);
        self.pending_native_shadow_cedes.remove(name);
    }

    /// Mark the class staged under `name` ([`Self::stage_transformed_class`])
    /// to cede its registered natives to its woven bytecode once the staged
    /// bytes define it ([`Self::cede_native_shadows_to_woven_bytecode`]).
    /// Interpreter round i1 wave 45, lane L3.
    pub fn stage_native_shadow_cede(&mut self, name: &str) {
        self.pending_native_shadow_cedes.insert(name.to_string());
    }

    /// A class an agent's LOAD-time transformer rewrote runs the woven
    /// bytecode on HotSpot, whatever the VM registers for its methods; this
    /// VM's dispatch doors cede a registered native to bytecode only for a
    /// class they see redefined (`class_redefine_generation > 0`, the
    /// Mockito inline-mock rule, with its reviewed immune lists). Give such a
    /// class that state: its generation goes from 0 to 1 and the
    /// [`any_class_redefined`] latch is raised, exactly as the class's first
    /// redefinition would, before any call reached it (the caller defined it
    /// under this manager's write lock and calls this in the same hold). No
    /// redefinition history is recorded and no counter a redefinition moves
    /// otherwise (`class_redefinition_count`, the metadata realm) moves: the
    /// class's bytecode never changed after it was defined.
    ///
    /// Only for a class the native registry may name (the caller checks):
    /// the latch moves every generation-gated door to its slow path. Before
    /// interpreter round i1 wave 45 (lane L3) a registered bridge kept
    /// shadowing the transformed class's bytecode, so an agent's wrapper of
    /// `java.util.zip.Adler32.update(II)I` never ran
    /// (`L3W44NativeMethodPrefix.java`, row
    /// `wrapper-ran`).
    pub fn cede_native_shadows_to_woven_bytecode(&self, class_id: ClassId) {
        let counter = self.class_redefine_generation_handle(class_id);
        let _ = counter.compare_exchange(0, 1, Ordering::Release, Ordering::Relaxed);
        ANY_CLASS_REDEFINED.store(true, Ordering::Release);
    }

    /// zt t20/pl9 (t16 defect D6): count a class-path define for the
    /// application loader (`loader_id` [`ClassLoaderId::Application`]; any
    /// other loader is not counted here) of `len` class-file bytes, before its
    /// parse, as HotSpot's `jvm_define_class_common` counts. The boot and
    /// platform arms are not counted: HotSpot's boot loader defines without
    /// `JVM_DefineClass`, and its platform loader's defines count only for a
    /// class outside the CDS archive (a residual on the perf-counter page).
    ///
    /// zt pl4 item 9 (D6 residual): the platform loader's defines count too
    /// (`Extension`, and the platform-module image classes the caller maps
    /// to it), as HotSpot's `jvm_define_class_common` counts every
    /// `defineClass` of a class outside the CDS archive, and its default
    /// archive holds no platform class.
    pub(in crate::class_manager) fn note_app_class_path_define(&mut self, loader_id: ClassLoaderId, len: usize) {
        if matches!(loader_id, ClassLoaderId::Application | ClassLoaderId::Extension) {
            self.app_class_path_defines.0 += 1;
            // Widening: usize -> u64 (value preserved).
            self.app_class_path_defines.1 += len as u64;
        }
    }

    /// zt t20/pl9: `(defines, bytes)` [`Self::note_app_class_path_define`]
    /// counted, for `sun.cls.defineAppClasses` / `appClassBytes`.
    pub fn app_class_path_defines(&self) -> (u64, u64) {
        self.app_class_path_defines
    }

    /// zt pl4 item 1: hand in the VM's class-trace figures (see the field).
    pub fn set_class_trace(
        &mut self,
        counters: Option<std::sync::Arc<cratonvm_types::perf_class_trace::ClassTraceCounters>>,
    ) {
        self.class_trace = counters;
    }

    /// zt t29/cb: a define of `name` by `loader_id` from `len` class-file
    /// bytes the built-in delegation chain found: the application loader's
    /// count ([`Self::note_app_class_path_define`]) and the boot loader's
    /// ([`Self::note_sys_class_path_bytes`]).
    pub(in crate::class_manager) fn note_class_path_define(&mut self, name: &str, loader_id: ClassLoaderId, len: usize) {
        // zt pl4 item 9 D6 residual: a class of a platform or tool module of
        // the runtime image is HotSpot's platform / application loader's
        // define (`JVM_DefineClass`, never restored from the default CDS
        // archive: the JDK's `lib/classlist` lists `java.base` classes only),
        // not a boot read.
        let loader_id = match self.image_class_java_loader(name, loader_id) {
            Some(java_loader) => java_loader,
            None => loader_id,
        };
        self.note_app_class_path_define(loader_id, len);
        self.note_sys_class_path_bytes(name, loader_id, len);
    }

    /// zt pl4 item 9 D6 residual (`CRATONVM_PERF_DEFINE_COUNTS_PLATFORM`,
    /// default ON, `=0` kill): the built-in loader HotSpot defines the
    /// boot-namespace class `name` with when it belongs to a platform or tool
    /// module of the runtime image (`ModuleRegistry::image_module_builtin_loader`);
    /// `None` for any other class, a non-boot `loader_id`, or the switch off.
    pub(in crate::class_manager) fn image_class_java_loader(&self, name: &str, loader_id: ClassLoaderId) -> Option<ClassLoaderId> {
        if loader_id != ClassLoaderId::Bootstrap {
            return None;
        }
        let module = self
            .module_registry
            .module_for_package(crate::module::package_of(name))?;
        let java_loader = self.module_registry.image_module_builtin_loader(module)?;
        true
            .then_some(java_loader)    }

    /// zt t29/cb (perf-counter page item 1, `sysClassBytes`): count `len`
    /// bytes when the boot loader (`loader_id` [`ClassLoaderId::Bootstrap`])
    /// defines `name` from bytes its search read, wherever they came from
    /// (the image, an appended or a patch entry), unless the CDS cache served
    /// them. HotSpot 25 adds every class file its boot loader reads outside
    /// the CDS archive: the jimage's (measured on 25.0.3, probe
    /// `Zt29CbClassBytes sysbytes`: 86717 bytes by `main` with the default
    /// archive, 4.7 MB with `-Xshare:off`) as well as a
    /// `-Xbootclasspath/a` directory's (the delta of one appended load is
    /// its file's length). The perf-counter page's reading ("not the
    /// jimage") was wrong; this VM keeps no HotSpot archive, so its figure is
    /// `-Xshare:off`'s kind.
    pub(in crate::class_manager) fn note_sys_class_path_bytes(&mut self, name: &str, loader_id: ClassLoaderId, len: usize) {
        if loader_id != ClassLoaderId::Bootstrap
            || (!self.cds_class_cache.is_empty() && self.cds_class_cache.contains_key(name))
        {
            return;
        }
        // Widening: usize -> u64 (value preserved).
        self.sys_class_path_bytes = self.sys_class_path_bytes.saturating_add(len as u64);
    }

    /// zt t29/cb: the bytes [`Self::note_sys_class_path_bytes`] counted, for
    /// `sun.cls.sysClassBytes`.
    pub fn sys_class_path_bytes(&self) -> u64 {
        self.sys_class_path_bytes
    }

    /// zt t29/cb: add `bytes`, the model bytes of classes an unload
    /// transaction removed, to [`Self::unloaded_class_bytes`]. Called under
    /// the same write lock as the removal, so a reader that sizes the live
    /// classes under the read lock sees both figures of one moment.
    pub fn note_unloaded_class_bytes(&mut self, bytes: u64) {
        self.unloaded_class_bytes = self.unloaded_class_bytes.saturating_add(bytes);
    }

    /// zt t29/cb: the model bytes of every class unloaded so far, for
    /// `sun.cls.unloadedBytes` (and `loadedBytes = live + unloaded`).
    pub fn unloaded_class_bytes(&self) -> u64 {
        self.unloaded_class_bytes
    }

    pub(in crate::class_manager) fn find_class_bytes_delegated(
        &self,
        name: &str,
    ) -> Result<(SharedBytes, ClassLoaderId), VmError> {
        // A class of an image module the boot configuration does not resolve
        // (an incubator module without `--add-modules`) is found by no loader
        // on HotSpot (interpreter round i4 wave 87, lane L5; probe
        // `L5W86ModuleLayers`, `incubator class`).
        if self.module_registry.class_outside_boot_configuration(name) {
            return Err(VmError::ClassFile(ClassFileError::ClassNotFound {
                class_name: name.to_string(),
            }));
        }
        // CDS archive check — fastest path
        if let Some(bytes) = self.cds_class_cache.get(name) {
            return Ok((bytes.clone().into(), ClassLoaderId::Bootstrap));
        }
        // Bootstrap first
        if let Ok(bytes) = self.bootstrap.find_class_bytes(name) {
            return Ok((bytes, ClassLoaderId::Bootstrap));
        }
        // Extension second
        if let Ok(bytes) = self.extension.find_class_bytes(name) {
            return Ok((bytes, ClassLoaderId::Extension));
        }
        // Application last
        if let Ok(bytes) = self.application.find_class_bytes(name) {
            return Ok((bytes, ClassLoaderId::Application));
        }

        Err(VmError::ClassFile(ClassFileError::ClassNotFound {
            class_name: name.to_string(),
        }))
    }

    /// Parse class bytes and register with the given loader identity.
    ///
    /// This is the core method for loading a class from raw bytecode.
    /// Used both internally (from `load_class`) and externally (from
    /// `ClassLoader.defineClass(byte[])` via `NativeContext`).
    pub fn define_class(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: ClassLoaderId,
    ) -> Result<ClassId, VmError> {
        self.define_class_with_options(name, bytes, loader_id, DefineClassOptions::default())
    }

    /// The synthetic slot FLOOR, applied to a class being defined from real
    /// class-file bytes.
    ///
    /// Both places that define such a class — [`Self::define_class_with_options`]
    /// and the stub-upgrade path — call this rather than open-coding the
    /// `max`, so the floor and the exemptions below it cannot drift apart. A
    /// FABRICATED stub never reaches here: `synthesize_stub_class` takes its
    /// fields straight from `synthetic_stub_fields` and consults no floor, so
    /// nothing this function does can change synthetic-JDK mode.
    ///
    /// The floor is the parent's already-floored total plus the class's own
    /// fabricated instance fields, and it pads a real class up to the width
    /// native code was written against. `java.net.InetSocketAddress` is the
    /// case it exists for: one declared instance field (`holder`), and a
    /// native `<init>` that writes raw synthetic indices on the REAL class.
    ///
    /// For [`FLOOR_EXEMPT_CLASSES`] it is skipped. Their fabricated table is a
    /// layout for the STUB and a fiction for the real class, and applying it
    /// pads the class out of the compact layout entirely — a padded slot has
    /// no descriptor, so `ClassStore::build_compact_layout` refuses the class
    /// outright and every one of its slots falls back to the legacy uniform
    /// 16-byte tagged cell. `java.util.Properties` paid 544 bytes empty for
    /// ten fields that pack into ~190 that way; `HashSet` paid 128 for one
    /// reference.
    ///
    /// For every other class, `CRATONVM_SYNTHETIC_FLOOR_STUB_EXTENT` (default
    /// ON since wave 17; `=0` turns it off) caps the floor at the stub's own
    /// extent
    /// ([`synthetic_floor_capped_at_stub_extent`]): the slots the define-time
    /// sum adds by counting a wider REAL parent twice are dropped, every
    /// absolute index of the fabricated layout is kept. `InetSocketAddress`
    /// is unchanged by it (its stub parent and its real parent are both
    /// field-less), and the GC-owned `Reference` family is never touched.
    pub(in crate::class_manager) fn apply_synthetic_floor(
        &self,
        name: &str,
        superclass_id: Option<ClassId>,
        num_total_fields: usize,
    ) -> usize {
        if floor_is_synthetic_only(name) {
            // Report a JDK whose shape has moved out from under the screen
            // that cleared this class. The exemption was measured against a
            // specific real layout; a changed one moves every absolute index
            // the natives were checked at, and the failure mode is the silent
            // one (`set_field` DROPS an out-of-range write).
            if let Some((expected, actual)) =
                floor_exempt_real_extent_disagreement(name, num_total_fields)
            {
                static REPORTED: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                if REPORTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 16 {
                    eprintln!(
                        "[layout] {name}: floor-exempt class declares {actual} instance \
                         fields, not the {expected} its exemption in \
                         FLOOR_EXEMPT_CLASSES was screened against. Re-run the screen \
                         before trusting this class's raw-slot natives."
                    );
                }
            }
            return num_total_fields;
        }
        let real_parent_fields = match superclass_id {
            Some(super_id) => self
                .class_store
                .get(super_id)
                .map_or(0, |c| c.num_total_fields),
            None => 0,
        };
        let floor = synthetic_floor_over_real_parent(name, real_parent_fields);
        if floor <= num_total_fields {
            return num_total_fields;
        }
        // JIT round 16 wave 13 (lane fieldsite2), OP16-1: the data-driven
        // screen. Behind `CRATONVM_SYNTHETIC_FLOOR_STUB_EXTENT` (default ON
        // since wave 17, `=0` restores the uncapped floor), and read only when
        // the floor would pad, so an unpadded class costs nothing. A [`FLOOR_EXEMPT_CLASSES`] row keeps its own switch (an
        // `ArrayList$ListItr` row that was measured and left OFF stays OFF),
        // and a class whose REAL chain reaches `java/lang/ref/Reference` is
        // the GC round's to screen (reference processing reads those objects):
        // it is capped too since zt t34
        // (`CRATONVM_SYNTHETIC_FLOOR_REFERENCE_STUB_EXTENT`, default ON; `=0`
        // keeps its old floor).
        if !floor_exempt_row(name)
            && (true
                || !self.real_chain_reaches_reference(name, superclass_id))
        {
            let capped = synthetic_floor_capped_at_stub_extent(name, real_parent_fields);
            let total = num_total_fields.max(capped);
            if loader_flags().dbg_layout && total != floor {
                eprintln!(
                    "[layout] {name}: synthetic floor capped at the stub extent: \
                     {floor} -> {total} (real {num_total_fields}, real parent \
                     {real_parent_fields}, stub extent {})",
                    synthetic_stub_total_field_count(name)
                );
            }
            return total;
        }
        floor
    }

    /// Whether `name`, or a class on the loaded superclass chain from
    /// `superclass_id`, is `java/lang/ref/Reference`.
    ///
    /// Not a name allow-list: it is the one ownership boundary the
    /// stub-extent screen must not cross. Every `Reference` subclass
    /// (`WeakReference`, `SoftReference`, `PhantomReference`,
    /// `WeakHashMap$Entry`, `jdk/internal/ref/*Cleanable`, ...) is read by the
    /// collector's reference processing, which the GC round screens, not a
    /// JIT lane. The walk is bounded so a malformed chain cannot hang a
    /// define.
    pub(in crate::class_manager) fn real_chain_reaches_reference(&self, name: &str, superclass_id: Option<ClassId>) -> bool {
        if name == REFERENCE_ROOT {
            return true;
        }
        let mut cur = superclass_id;
        for _ in 0..256 {
            let Some(id) = cur else { return false };
            let Some(class) = self.class_store.get(id) else {
                return false;
            };
            if &*class.name == REFERENCE_ROOT {
                return true;
            }
            cur = class.superclass;        }
        false
    }

    /// NEW-8: extended define_class entry point used by
    /// `MethodHandles.Lookup.defineHiddenClass`. Accepts an
    /// [`DefineClassOptions`] that lets the caller override the stored
    /// class name (so hidden classes can be registered under a unique
    /// mangled name even when their class file's `this_class` entry
    /// collides with an existing class) and mark the new class as
    /// hidden in one atomic operation.
    pub fn define_class_with_options(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: ClassLoaderId,
        options: DefineClassOptions,
    ) -> Result<ClassId, VmError> {
        self.define_class_shared_with_options(name, bytes.to_vec().into(), loader_id, options)
    }

    /// [`Self::define_class_with_options`] over a class file the caller has
    /// already parsed from these exact `bytes`
    /// (`cratonvm_reader::read_class_arc(bytes.clone())`): the define skips
    /// its own parse and uses `class_file` in its place, and caches `bytes`
    /// (the allocation the parse's lazy attributes share) as the class's file
    /// instead of a fresh copy; everything else -- the header checks, the
    /// attribute decode, the name check, linking, verification -- is the
    /// same. The `defineClass1` / `defineClass2` natives' supertype pre-pass
    /// parse, under `CRATONVM_DEFINE_ONE_PARSE` (interpreter round i3 wave 72,
    /// lane L5). A `class_file` that is not the parse of `bytes` is a caller
    /// bug: the class would be defined from it while `bytes` are cached as
    /// its file.
    pub fn define_class_with_options_preparsed(
        &mut self,
        name: &str,
        bytes: Arc<[u8]>,
        loader_id: ClassLoaderId,
        options: DefineClassOptions,
        class_file: cratonvm_reader::ClassFile,
    ) -> Result<ClassId, VmError> {
        self.define_class_shared_parsed(name, bytes.into(), loader_id, options, Some(class_file))
    }

    /// The JVMS §5.3.4 constraint table, for a recorder that holds only the
    /// class-manager READ guard (`--real-jdk` member resolution,
    /// `vm/src/runtime/resolve/loader_constraints.rs`) and for the
    /// initiating-loader check. A short critical section; never held across a
    /// Java call.
    pub fn loader_constraint_table(
        &self,
    ) -> parking_lot::MutexGuard<'_, crate::loader_constraints::LoaderConstraints> {
        self.loader_constraints.lock()
    }

    /// HotSpot's `loader_name_and_id` text for `loader`: the three built-in
    /// loaders by name, a user-defined loader by the label its recorder
    /// stored (`CratonVM`'s id when none was), with the parent clause
    /// `class_in_module_of_loader` prints for a user-defined loader.
    pub(in crate::class_manager) fn loader_constraint_label(
        constraints: &crate::loader_constraints::LoaderConstraints,
        loader: ClassLoaderId,
    ) -> (String, Option<String>) {
        match loader {
            ClassLoaderId::Bootstrap => ("'bootstrap'".to_string(), None),
            ClassLoaderId::Extension => ("'platform'".to_string(), None),
            ClassLoaderId::Application => ("'app'".to_string(), None),
            ClassLoaderId::UserDefined(_) => match constraints.label(loader.to_native_id()) {
                Some(label) => (label.name_and_id.clone(), label.parent.clone()),
                None => (format!("{loader}"), None),
            },
        }
    }

    /// HotSpot 25's `InstanceKlass::has_as_permitted_subclass` for the sealed
    /// supertype `sealed` and the class being defined (`this_class`, internal
    /// name, defined by `loader`, `this_public`): `None` when permitted, else
    /// the `IncompatibleClassChangeError` message, in its three checks' order
    /// and words (measured on Temurin 25.0.3, probe `L5W84FinalOverride`):
    ///
    /// * `Failed same module check: subclass <C> is in module '<m>' with
    ///   loader <l>, and sealed class <S> is in module '<m>' with loader <l>`;
    /// * `Failed same package check: non-public subclass <C> is in package
    ///   '<p>' with classloader <l>, and sealed class <S> is in package '<p>'
    ///   with classloader <l>` (internal package names, `unnamed` for none);
    /// * `Failed listed permitted subclass check: class <C> is not a permitted
    ///   subclass of <S>`.
    ///
    /// The module check is approximated by the defining loaders, compared only
    /// when a user-defined loader is on either side: CratonVM files some
    /// null-loader JDK-internal defines under the application id, so the
    /// built-in ids are not told apart, and two named modules of one loader
    /// are not told apart either. The class being defined is named in an
    /// unnamed module. Interpreter round i4 wave 84, lane L5.
    /// HotSpot's `Klass::class_in_module_of_loader` for `class`: `<C> is in
    /// <module> of loader <loader>` (`module <name>` or `unnamed module`; the
    /// loader as [`Self::loader_constraint_label`] names it, a user loader by
    /// its recorded `nameAndId` or as `user-defined(<id>)`, which the define
    /// natives rewrite). Measured on JDK 25.0.3: `(java.lang.Object is in
    /// module java.base of loader 'bootstrap')`, `(M$AppPlain is in unnamed
    /// module of loader 'app')`, `(java.sql.Date is in module java.sql of
    /// loader 'platform')` (the boot-tagged platform class is named by
    /// `access_control::message_loader_of`).
    /// `None` with `CRATONVM_SUPERTYPE_SHAPE_MODULE_CLAUSE=0`. Interpreter
    /// round i7 wave 141, lane L5.
    pub(in crate::class_manager) fn class_in_module_of_loader(&self, class: &Class) -> Option<String> {
        // As `loader_constraint_class_origin` (the platform loader for a
        // platform module's class, HotSpot's `@version`), without the parent
        // clause: this message is `class_in_module_of_loader(false, false)`.
        let module = crate::access_control::layer_module_name_of(class, &self.module_registry)
            .map(str::to_string)
            .or_else(|| class.module_name.clone())
            .or_else(|| match &class.origin {
                ClassOrigin::BootImage { module: Some(m), .. } => Some(m.to_string()),
                _ => None,
            })
            .filter(|m| !m.is_empty())
            .map_or_else(
                || "unnamed module".to_string(),
                |m| {
                    format!(
                        "module {}",
                        crate::access_control::module_with_version(
                            &m,
                            crate::access_control::message_module_version(
                                class,
                                &self.module_registry
                            )
                        )
                    )
                },
            );
        let loader = {
            let constraints = self.loader_constraints.lock();
            Self::loader_constraint_label(
                &constraints,
                crate::access_control::message_loader_of(class, Some(&self.module_registry)),
            )
            .0
        };
        Some(format!(
            "{} is in {module} of loader {loader}",
            class.name.replace('/', ".")
        ))
    }

    pub(in crate::class_manager) fn sealed_refusal(
        &self,
        sealed: &Class,
        this_class: &str,
        loader: ClassLoaderId,
        this_public: bool,
    ) -> Option<String> {
        let user = |id: ClassLoaderId| matches!(id, ClassLoaderId::UserDefined(_));
        let label = |id: ClassLoaderId| {
            let constraints = self.loader_constraints.lock();
            Self::loader_constraint_label(&constraints, id).0
        };
        let sub = this_class.replace('/', ".");
        let sup = sealed.name.replace('/', ".");
        let same_module =
            sealed.loader_id == loader || (!user(sealed.loader_id) && !user(loader));
        if !same_module {
            let sealed_module = match crate::access_control::layer_module_name_of(
                sealed,
                &self.module_registry,
            )
            .or(sealed.module_name.as_deref())
            {
                Some(m) if !m.is_empty() => m.to_string(),
                _ => "unnamed module".to_string(),
            };
            return Some(format!(
                "Failed same module check: subclass {sub} is in module 'unnamed module' with \
                 loader {}, and sealed class {sup} is in module '{sealed_module}' with loader {}",
                label(loader),
                label(sealed.loader_id)
            ));
        }
        let package = |name: &str| -> String {
            match package_of_internal(name) {
                "" => "unnamed".to_string(),
                p => p.to_string(),
            }
        };
        if !this_public && package_of_internal(&sealed.name) != package_of_internal(this_class) {
            return Some(format!(
                "Failed same package check: non-public subclass {sub} is in package '{}' with \
                 classloader {}, and sealed class {sup} is in package '{}' with classloader {}",
                package(this_class),
                label(loader),
                package(&sealed.name),
                label(sealed.loader_id)
            ));
        }
        if sealed
            .permitted_subclasses
            .iter()
            .any(|p| p.as_str() == this_class)
        {
            return None;
        }
        Some(format!(
            "Failed listed permitted subclass check: class {sub} is not a permitted subclass of \
             {sup}"
        ))
    }

    /// `loader`'s HotSpot `loader_name_and_id` text: the built-in names, a user
    /// loader's stored label, or `user-defined(<id>)` when it has none -- which
    /// the define natives replace with the loader's `nameAndId`
    /// (`native-builtins` `classloader_real::name_user_loaders_in`).
    pub(in crate::class_manager) fn loader_message_text(&self, loader: ClassLoaderId) -> String {
        let constraints = self.loader_constraints.lock();
        Self::loader_constraint_label(&constraints, loader).0
    }

    /// The refusals HotSpot's `ClassFileParser` raises once a class's
    /// supertypes are resolved, in its order, for the class `class` being
    /// defined (built, not registered):
    ///
    /// 1. `check_super_class_access` on the superclass: the sealed check
    ///    ([`Self::sealed_refusal`], `IncompatibleClassChangeError`), then the
    ///    access check (`IllegalAccessError`, [`Self::super_access_refusal`]);
    /// 2. `check_super_interface_access` on each direct superinterface, LAST
    ///    declared first (HotSpot walks `local_interfaces` backwards), the same
    ///    two checks;
    /// 3. `check_final_method_override` ([`final_method_override_violation`]).
    ///
    /// `trusted` (a `skip_verification` define) skips the override check; the
    /// boot loader's classes skip the access and override checks.
    /// The access check is the `SupertypeShapeChecks` alignment row's (on by
    /// default in both modes; `CRATONVM_COMPAT_ALIGN=-supertype-shape-checks`
    /// withdraws it under `--compatible`). Interpreter round i4 waves 84-85,
    /// lane L5.
    pub(in crate::class_manager) fn supertype_link_refusal(&self, class: &Class, trusted: bool) -> Option<LinkageError> {
        let this_public = class.access_flags.contains(ClassAccessFlags::PUBLIC);
        let checked = !trusted && class.loader_id != ClassLoaderId::Bootstrap;
        // HotSpot admits every access from a subclass of
        // `jdk.internal.reflect.SerializationConstructorAccessorImpl`; the
        // JDK's generated accessors live in that package and are defined
        // through a throwaway loader, so the package is left alone here.
        //
        // Not withheld from a `trusted` define: `Lookup.defineClass`'s
        // `--compatible` route sets `skip_verification` for arbitrary user
        // bytes, and HotSpot asks this of every class.
        let access_checked = class.loader_id != ClassLoaderId::Bootstrap
            && package_of_internal(&class.name) != "jdk/internal/reflect"
            && self.aligned(cratonvm_types::HotSpotAlignment::SupertypeShapeChecks);
        let supertypes = class
            .superclass
            .map(|id| (id, false))
            .into_iter()
            .chain(class.interfaces.iter().rev().map(|&id| (id, true)));
        for (id, interface) in supertypes {
            let Some(sup) = self.class_store.get(id) else {
                continue;
            };
            if sup.is_sealed() {
                if let Some(message) =
                    self.sealed_refusal(sup, &class.name, class.loader_id, this_public)
                {
                    return Some(LinkageError::IncompatibleClassChangeError { message });
                }
            }
            if access_checked {
                if let Some(message) = self.super_access_refusal(class, sup, interface) {
                    return Some(LinkageError::IllegalAccessError { message });
                }
            }
        }
        if checked {
            if let Some(message) = final_method_override_violation(
                &class.name,
                class.loader_id,
                &class.methods,
                class.superclass,
                &self.class_store,
            ) {
                return Some(LinkageError::IncompatibleClassChangeError { message });            }        }
        None
    }

    /// `Reflection::verify_class_access(class, sup)` and HotSpot's message
    /// when it fails (`None` when `class` may access `sup`):
    ///
    /// * same run-time package: allowed;
    /// * `sup` not public (`OTHER_PROBLEM`):
    ///   [`crate::access_control::super_access_denied_message`];
    /// * `sup` public but its package not exported to `class`'s module
    ///   (`TYPE_NOT_EXPORTED`, or a layer module's `MODULE_NOT_READABLE`):
    ///   `superclass access check failed: <verify_class_access_msg>` or
    ///   `superinterface check failed: <...>`. HotSpot names an unnamed
    ///   module `unnamed module @0x<hash>`; this VM says `unnamed module`.
    ///
    /// A supertype without real class bytes (a compatibility stub) is not
    /// judged: its flags are fabricated.
    pub(in crate::class_manager) fn super_access_refusal(&self, class: &Class, sup: &Class, interface: bool) -> Option<String> {
        if !sup.origin.has_real_bytes()
            || crate::access_control::same_runtime_package(class, sup)
        {
            return None;
        }
        if !sup.is_public() {
            let name_user_loader = |c: &Class| Some(self.loader_message_text(c.loader_id));
            return Some(crate::access_control::super_access_denied_message(
                class,
                sup,
                interface,
                Some(&self.module_registry),
                &name_user_loader,
            ));
        }
        let denial = crate::access_control::class_export_denial(class, sup, &self.module_registry)?;
        Some(format!(
            "{} check failed: {}",
            if interface {
                "superinterface"
            } else {
                "superclass access"
            },
            denial.message
        ))
    }

    /// HotSpot's `Klass::class_in_module_of_loader(false, true)` for
    /// `class_id`, from the stored loader labels: `p.X is in unnamed module of
    /// loader 'a' @1b6d3586, parent loader 'b' @4554617c` (measured on JDK
    /// 25; the parent clause for a user-defined loader only, `'bootstrap'` for
    /// a null parent).
    pub fn loader_constraint_class_origin(
        &self,
        constraints: &crate::loader_constraints::LoaderConstraints,
        class_id: ClassId,
    ) -> Option<String> {
        let class = self.get_class(class_id)?;
        // A platform module's class is the platform loader's on HotSpot
        // (`access_control::message_loader_of`; interpreter round i1 wave 48,
        // lane L5).
        let (loader, parent) = Self::loader_constraint_label(
            constraints,
            crate::access_control::message_loader_of(class, Some(&self.module_registry)),
        );
        // A class of a non-boot layer's module (`--real-jdk`) is in that
        // module, which its `module_name` does not record.
        let module = match crate::access_control::layer_module_name_of(class, &self.module_registry)
            .or(class.module_name.as_deref())
        {
            // HotSpot's `@version` (interpreter round i4 wave 99, lane L5).
            Some(m) if !m.is_empty() => format!(
                "module {}",
                crate::access_control::module_with_version(
                    m,
                    crate::access_control::message_module_version(class, &self.module_registry)
                )
            ),
            _ => "unnamed module".to_string(),
        };
        let parent = parent
            .map(|p| format!(", parent loader {p}"))
            .unwrap_or_default();
        Some(format!(
            "{} is in {module} of loader {loader}{parent}",
            class.name.replace('/', ".")
        ))
    }

    /// Mark (`on`) or unmark the CURRENT thread's defines as VM fallbacks
    /// ([`Self::vm_fallback_threads`]). The VM sets it around the flat-store
    /// load it makes after a user loader refused, under a short write guard
    /// of its own, and clears it after (interpreter round i6 wave 118, lane L5).
    ///
    /// Answers whether this call changed the thread's mark (wave 119, lane L9
    /// review): a nested fallback load on the same thread finds it set already
    /// (`false`), and must then not clear it, or the outer load's remaining
    /// defines would lose the exemption.
    pub fn set_vm_fallback_defines(&mut self, on: bool) -> bool {
        let me = std::thread::current().id();
        if on {
            self.vm_fallback_threads.insert(me)
        } else {
            self.vm_fallback_threads.remove(&me)
        }
    }

    /// Is the current thread inside a VM fallback load
    /// ([`Self::set_vm_fallback_defines`])? One `is_empty` test outside one.
    pub(in crate::class_manager) fn on_vm_fallback_thread(&self) -> bool {
        !self.vm_fallback_threads.is_empty()
            && self.vm_fallback_threads.contains(&std::thread::current().id())
    }

    /// Is the define of a class by `loader` with `options` on this thread a
    /// VM fallback define to remember in [`Self::vm_fallback_classes`]? Only
    /// a built-in loader's, only while the built-in refusal and its
    /// fallback-pin exemption are both on (interpreter round i6 wave 133, lane
    /// L5c). Read once at the refusal point of the define, before its
    /// supertypes load.
    pub(in crate::class_manager) fn records_vm_fallback_copy(&self, loader: ClassLoaderId, options: &DefineClassOptions) -> bool {
        self.define_builtin_constraint_refusal
            && self.define_builtin_constraint_fallback_pin
            && !matches!(loader, ClassLoaderId::UserDefined(_))
            && (options.vm_fallback || self.on_vm_fallback_thread())
    }

    /// The `LinkageError` message when `loader` defining `name` would break a
    /// JVMS §5.3.4 constraint: HotSpot's `SystemDictionary::check_constraints`
    /// wording, measured on JDK 25 (probe `L5W37LoaderConstraintPending`):
    ///
    /// `loader constraint violation: loader 'b' @6d06d69c wants to load class
    /// p.Shared. A different class with the same name was previously loaded by
    /// 'a' @7852e922. (p.Shared is in unnamed module of loader 'a' @7852e922,
    /// parent loader 'b' @6d06d69c)`
    ///
    /// `None` when no constraint pins `(name, loader)` (always while the table
    /// is empty). The class being defined is new, so a pin is a violation; it
    /// is counted in the table.
    pub(in crate::class_manager) fn loader_constraint_define_refusal(
        &mut self,
        name: &str,
        loader: ClassLoaderId,
        is_interface: bool,
    ) -> Option<String> {
        let loader_key = loader.to_native_id();
        let pinned = self
            .loader_constraints
            .get_mut()
            .pinned_for(name, loader_key)?;
        // A built-in loader's define of a name a compatibility stub holds is
        // the stub's upgrade (below in the define), not a new class the loader
        // asked for: admitted and counted as before (wave 117).
        if !matches!(loader, ClassLoaderId::UserDefined(_))
            && self
                .get_loaded_class_id(name)
                .and_then(|id| self.class_store.get(id))
                .is_some_and(|c| c.origin.is_compatibility_stub())
        {
            return None;
        }
        // A built-in loader's define against a pin that names a VM fallback
        // copy (`CRATONVM_DEFINE_BUILTIN_CONSTRAINT_FALLBACK_PIN`, wave 133,
        // lane L5c; hazard 1 of the `i118-L9` review): HotSpot never made that
        // copy, so this define is the name's only class there and succeeds.
        // Admitted; the pin after registration counts the broken constraint
        // and traces it as `(counted, admitted)`, as before wave 117. A user
        // loader's define keeps its wave-37 refusal.
        if !matches!(loader, ClassLoaderId::UserDefined(_))
            && !self.vm_fallback_classes.is_empty()
            && self.vm_fallback_classes.contains(&ClassId::new(pinned))
        {
            if cratonvm_types::flags().loader.dbg_access {
                eprintln!(
                    "[ACCESS-DBG] LOADER-CONSTRAINT fallback pin: built-in define of {} by {loader} admitted (the pin names VM fallback copy {pinned})",
                    name.replace('/', ".")
                );
            }
            return None;
        }
        let existing_id = ClassId::new(pinned);
        let existing = self.get_class(existing_id)?;
        let existing_kind = if existing.is_interface() {
            "interface"
        } else {
            "class"
        };
        let registry = Some(&self.module_registry);
        let existing_loader = crate::access_control::message_loader_of(existing, registry);
        let loader_named = crate::access_control::message_loader_for_name(loader, name, registry);
        let constraints = self.loader_constraints.lock();
        let (me, _) = Self::loader_constraint_label(&constraints, loader_named);
        let (previous, _) = Self::loader_constraint_label(&constraints, existing_loader);
        let origin = self.loader_constraint_class_origin(&constraints, existing_id)?;
        let kind = if is_interface { "interface" } else { "class" };
        let external = name.replace('/', ".");
        // zt t21/df9 (define page §9): a pin to a define whose `addClass`
        // upcall threw (`withdraw_unbound_define`; the class is nameless).
        // HotSpot's `find_constrained_klass` answers only a `loaded` class,
        // and that one never got there, so `check_constraints` prints the
        // other arm: the NEW class's module clause alone. The new class is
        // not built yet: the withdrawn one's clause when the same loader
        // defines (same name, same loader, same module), else the defining
        // loader's unnamed module.
        let message = if self.class_store.is_nameless(existing_id) {
            let clause = if existing_loader == loader_named {
                origin
            } else {
                let (_, parent) = Self::loader_constraint_label(&constraints, loader_named);
                let parent = parent
                    .map(|p| format!(", parent loader {p}"))
                    .unwrap_or_default();
                format!("{external} is in unnamed module of loader {me}{parent}")
            };
            format!(
                "loader constraint violation: loader {me} wants to load {kind} {external}. ({clause})"
            )
        } else {
            format!(
                "loader constraint violation: loader {me} wants to load {kind} {external}. \
                 A different {existing_kind} with the same name was previously loaded by \
                 {previous}. ({origin})"
            )
        };
        drop(constraints);
        self.loader_constraints
            .get_mut()
            .note_define_refusal(name, pinned);
        if cratonvm_types::flags().loader.dbg_access {
            eprintln!("[ACCESS-DBG] LOADER-CONSTRAINT DENY at define: {message}");
        }
        Some(message)
    }

    /// Define a class while retaining the class-path or archive backing that
    /// supplied its bytes. Public byte-slice callers still enter through
    /// `define_class_with_options`; class-path loads use this path to avoid a
    /// second allocation and copy.
    pub(in crate::class_manager) fn define_class_shared_with_options(
        &mut self,
        name: &str,
        bytes: SharedBytes,
        loader_id: ClassLoaderId,
        options: DefineClassOptions,
    ) -> Result<ClassId, VmError> {
        self.define_class_shared_parsed(name, bytes, loader_id, options, None)
    }

}
