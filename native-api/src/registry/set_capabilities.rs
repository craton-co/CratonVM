#[allow(unused_imports)]
use super::*;

impl NativeMethodRegistry {
    pub fn new() -> Self {
        // ~8,800 native methods are registered at boot; size the maps
        // up front so `register()` does not repeatedly rehash/grow.
        const BOOT_REGISTRATION_HINT: usize = 16384;
        Self {
            slots: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            // Each shard is index-parallel with `slots` and sized the same way.
            // The boot pushes may still reallocate once the registration count
            // passes the hint.
            slot_invocations: std::array::from_fn(|_| Vec::with_capacity(BOOT_REGISTRATION_HINT)),
            // Same index-parallel discipline and the same sizing hint; see the
            // field doc for what the bit means.
            slot_invocations_incomplete: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            slot_by_key: FxHashMap::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            classes_with_natives: FxHashSet::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            registry_epoch: NEXT_REGISTRY_EPOCH
                .fetch_add(REGISTRY_EPOCH_STRIDE, std::sync::atomic::Ordering::Relaxed),
            mutation_epoch: 0,
            netty_tcnative_muted: std::sync::atomic::AtomicBool::new(false),
            registrations: RegistrationLog::with_capacity(BOOT_REGISTRATION_HINT),
            provenance: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            by_method_desc: FxHashMap::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            method_owner_index: std::sync::RwLock::new(None),
            categories: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            current_leaf: false,
            retagging_retired_shadow: false,
            admitting_family_intrinsic: false,
            // Read once at construction. `CRATONVM_NO_STUBS` (any non-empty
            // value) enables strict mode: synthetic-stub registrations are
            // dropped so calls hit real bytecode or a clear error.
            allow_synthetic_net_sockets: false,
            drop_synthetic_stubs: cratonvm_types::flags::runtime_var_os("CRATONVM_NO_STUBS")
                .is_some_and(|v| !v.is_empty()),
            // Real-JDK policy is a CLI/init decision, never an env var: it must
            // be identical for every registry in a process-hosted multi-VM run
            // only if the caller says so. Default `Compatible` keeps today's
            // behaviour byte-for-byte (§1, "--real-jdk (default) keeps today's
            // behaviour exactly").
            compatibility_mode: CompatibilityMode::Compatible,
            real_jdk: false,
            jdk_feature: None,
            jit_enabled: true,
            jdk_continuations: false,
            retired_rows_mask_ancestor_bridges: true,
            ancestor_walk_helper: true,
            ancestor_redefine_rule: true,
            ancestor_walk_stub_yield: true,
            redefine_owner_scoped_eviction: true,
            ancestor_redefine_bodyless_keep: true,
            jit_site_cache_owner_redefine: true,
            redefine_inherited_owner: true,
            jit_native_lean_door: true,
            class_hash_memo: true,
            dbg_native_door_split: false,
            native_door_split: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            jit_native_site_door_lean_funnel: false,
            jit_native_walk_once: true,
            dbg_native_site_door_census: false,
            jit_native_site_door_leaf_kinds: true,
            jit_native_site_door_two_way: true,
            jit_native_fresh_result: true,
            jit_native_site_refusal_by_receiver: false,
            jit_leaf_probe_validated_args: false,
            native_site_door_census: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            hotspot_alignments: cratonvm_types::HotSpotAlignments::from_env(),
            compat_family_in_force: false,
            image_app_module_loaders: true,
            per_name_loader_locks: true,
            delegates_to_url_parent: true,
            resolver_absence_shape: true,
            bais_jdk_bodies: true,
            jdk_inet_socket_address_bodies: true,
            // Default ON since interpreter round i7 wave 144 (A/B w142lto).
            strict_family_bridges: true,
            nested_jar_walk_index:true,
            nested_jar_walk_index_stamp: true,
            nested_jar_shared_outer: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_NESTED_JAR_SHARED_OUTER",
            ),
            nested_jar_outer_index: true,
            nested_jar_refusal_stamp: true,
            nested_jar_bytes_directory: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_NESTED_JAR_BYTES_DIRECTORY",
            ),
            nested_jar_outer_handle_reuse: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE",
            ),
            nested_jar_pooled_stat: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_NESTED_JAR_POOLED_STAT",
            ),
            nested_jar_class_path_outer: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_NESTED_JAR_CLASS_PATH_OUTER",
            ),
            nested_jar_bytes_index: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_NESTED_JAR_BYTES_INDEX",
            ),
            jar_loader_bytes_index: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JAR_LOADER_BYTES_INDEX",
            ),
            jar_loader_class_path_archive: true,
            jar_loader_class_path_direct: true,
            // On by default since the interpreter round i4 wave-88 triage (`=0` off).
            define_one_parse: true,
            // The exact rule of the process `OnceLock` this replaced: off
            // only for the value `0`.
            real_builtin_loaders: cratonvm_types::flags::runtime_var("CRATONVM_REAL_BUILTIN_LOADERS")
                .map(|v| v != "0")
                .unwrap_or(true),
            define_supertype_dictionary: true,
            define_records_supertype_initiation: true,
            loader_kind_memo: true,
            clv_sub_fast_key: cratonvm_types::flags::runtime_flag_on("CRATONVM_CLV_SUB_FAST_KEY"),
            // Interpreter round i4 wave 86 triage: on by default (`=0` off);
            // the G1-cleanup epoch and the ZGC pre-slide verdicts were fixed
            // in wave 84.
            loader_namespace_index: true,
            platform_resource_jrt_walk: true,
            inflater_reuse_buffers: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_INFLATER_REUSE_BUFFERS",
            ),
            inflater_input_window: true,
            inflater_raw_adler_zero: true,
            // Default ON since interpreter round i5 w100 triage (4 threads
            // 5x, 8 threads 9x faster per inflate; one thread unchanged); `=0` off.
            inflater_sharded_table: true,
            arrays_mismatch_bulk: true,
            zip_utils_intrinsics: true,
            zip_cen_accessor_intrinsics: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS",
            ),
            zip_coder_compare_intrinsic: true,
            jit_zip_header_loads: true,
            zip_entry_miss_intrinsic: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_ZIP_ENTRY_MISS_INTRINSIC",
            ),
            zip_entry_hit_intrinsic: true,
            intrinsic_body_compiles: false,
            intrinsic_body_compiles_rows: read_intrinsic_body_compiles_rows(
                None::<String>
                    .as_deref(),
            ),
            body_compilable_intrinsics: Vec::new(),
            reentry_only_bodies: Vec::new(),
            reentry_only_bodies_own_switch: Vec::new(),
            zip_source_slots: std::sync::OnceLock::new(),
            record_boolean_components: std::sync::OnceLock::new(),
            vh_real_varform: std::sync::OnceLock::new(),
            hidden_frames_observed: std::sync::atomic::AtomicBool::new(false),
            reference_layout_memo: true,
            reference_layout: std::sync::OnceLock::new(),
            process_tables: std::sync::OnceLock::new(),
            archive_bytes_cache: std::sync::OnceLock::new(),
            compat_url_loader_defined_packages: true,
            string_literal_identity: true,
            builtin_loader_package_sync: true,
            walk_carrier_retired: true,
            compatible_jdk_reifier: true,
            jdk_type_annotations: true,
            jdk_declaration_annotations: true,
            reflect_class_access: true,
            inner_classes_agreement: true,
            jdk_reflective_invoke: true,
            jdk_only_field_doors: true,
            stack_trace_element_of_intrinsic: true,
            reflect_invoke_plan: true,
            reflect_door_caches: std::sync::OnceLock::new(),
            field_door_ident_memo: true,
            stack_trace_origin_record: true,
            stack_trace_origin_records: std::sync::OnceLock::new(),
            stack_trace_fill_name_from_text: true,
            stack_trace_element_to_string_intrinsic: true,
            stack_trace_element_of_frame_intrinsic: true,
            compat_stack_trace_keeps_array: true,
            compat_sfi_element_jdk_body: false,
            stack_trace_fill_switches: StackTraceFillSwitches::from_env(),
            reentrant_lock_uncontended_fast: true,
            reentrant_lock_fast_decline_all: false,
            reentrant_lock_fast_wide: true,
            reflect_invoke_settled: true,
            reflect_ctor_plan_per_vm: true,
            reflect_door_word_batch: true,
            reflect_door_phase_stats: false,
            jit_reflect_door_direct: true,
            reflect_invoke_plan_facts: true,
            reflect_export_gate_disabled: cratonvm_types::flags::runtime_var(
                "CRATONVM_REFLECT_NO_EXPORT_GATE",
            )
            .is_ok(),
            type_annotation_bytes_real: false,
            resource_streams_open_url: true,
            defined_class_resources_defining_loader: true,
            boot_append_package_table: true,
            jdk_built_packages: true,
            impl_jars_es_loader_only: true,
            refused: Vec::new(),
            drop_real_layout_synthetic: false,
            // PERF: deferred native-ring name index. Sized like the other boot
            // maps so the ~3,100 boot inserts don't rehash/grow.
            name_index: FxHashMap::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            class_facts: FxHashMap::with_capacity_and_hasher(2048, Default::default()),
            // No policy until an embedder installs one: today's behaviour
            // exactly. See `capabilities` field doc.
            capabilities: None,
            sensitive_slots: FxHashMap::default(),
            app_workarounds: Vec::new(),
            app_stub_layouts: FxHashMap::default(),
        }
    }

    /// Install this VM's capability policy.
    ///
    /// Call **once at VM init, before the `register_*` population pass** — this
    /// gates `register()`, and it is what populates the dispatch-side
    /// classification map, so a policy installed afterwards leaves the already-
    /// registered natives unclassified. `&mut self` is the enforcement:
    /// dispatch only ever holds `&`, so a running native cannot swap the
    /// policy out from under the gate.
    ///
    /// Pass the same `Arc` to
    /// [`install_capabilities`](crate::capability::install_capabilities) so
    /// per-call-site gates reached through `NativeContext` share one audit log
    /// with the registry.
    pub fn set_capabilities(&mut self, caps: Arc<crate::capability::CapabilitySet>) {
        self.capabilities = Some(caps);
    }

    /// This VM's capability policy, if one was installed.
    #[inline]
    pub fn capabilities(&self) -> Option<&Arc<crate::capability::CapabilitySet>> {
        self.capabilities.as_ref()
    }

    /// The capability a registered native definitionally exercises, or `None`
    /// for the ~3,100 that exercise none (and for every registration accepted
    /// before a policy was installed).
    #[inline]
    pub fn capability_of_id(
        &self,
        id: NativeMethodId,
    ) -> Option<crate::capability::CapabilityKind> {
        self.sensitive_slots.get(&(id.index() as u32)).copied()
    }

    /// Dispatch-side capability gate: check the capability class of the native
    /// behind `id` before invoking it.
    ///
    /// This is the coarse safety net under the per-call-site gates. It can only
    /// report [`Scope::Any`](crate::capability::Scope::Any) — at this point the
    /// arguments have not been decoded, so there is no path or host to name —
    /// which means an `Enforce` deployment must hold the unscoped grant (e.g.
    /// `process-spawn:*`) for the class of native to dispatch at all, and the
    /// per-call-site gate then applies the scoped decision. Its value is
    /// coverage: it fires for every native in
    /// [`classify_native`](crate::capability::classify_native), including ones
    /// whose implementation has no gate of its own yet.
    ///
    /// With no policy installed this is one `Option` discriminant test.
    #[inline]
    #[track_caller]
    pub fn check_dispatch_capability(
        &self,
        id: NativeMethodId,
    ) -> Result<(), crate::capability::CapabilityDenied> {
        let Some(caps) = self.capabilities.as_ref() else {
            return Ok(());
        };
        let Some(kind) = self.capability_of_id(id) else {
            return Ok(());
        };
        caps.check(crate::capability::Capability::of(
            kind,
            crate::capability::Scope::Any,
        ))
    }

    /// Override the strict no-stubs mode programmatically (e.g. for tests or a
    /// CLI flag), independent of the `CRATONVM_NO_STUBS` env var. Call before
    /// the `register_*` population pass. See [`drop_synthetic_stubs`](Self).
    pub fn set_drop_synthetic_stubs(&mut self, drop: bool) {
        self.drop_synthetic_stubs = drop;
    }

    /// Whether strict no-stubs mode is active (synthetic-stub registrations are
    /// being dropped).
    pub fn drops_synthetic_stubs(&self) -> bool {
        self.drop_synthetic_stubs
    }

    /// Set the VM-scoped strict policy
    /// (`docs/book/src/user-guide/real-jdk-mode.md` §4). Call **once at VM init,
    /// before the `register_*` population pass** — this gates `register()`, so
    /// flipping it afterwards leaves whatever was already accepted in place and
    /// produces a registry that is neither honestly compatible nor honestly
    /// strict. `&mut self` is the enforcement: dispatch only ever holds `&`.
    ///
    /// Does not touch `drop_synthetic_stubs`; the `CRATONVM_NO_STUBS` env
    /// reading in `new()` is independent and still applies.
    pub fn set_compatibility_mode(&mut self, mode: CompatibilityMode) {
        self.compatibility_mode = mode;
    }

    /// Register an application/library workaround entry in this VM's table.
    pub fn register_app_workaround(&mut self, entry: AppWorkaroundEntry) {
        self.app_workarounds.push(entry);
    }

    /// All registered application-workaround entries for this VM.
    #[inline]
    pub fn app_workarounds(&self) -> &[AppWorkaroundEntry] {
        &self.app_workarounds
    }

    /// Register synthetic stub field layout for a third-party class.
    pub fn register_synthetic_stub_layout(
        &mut self,
        class_name: &'static str,
        fields: &'static [SyntheticStubField],
    ) {
        self.app_stub_layouts.insert(class_name, fields);
    }

    /// All registered third-party synthetic stub field layouts.
    #[inline]
    pub fn app_stub_layouts(&self) -> &FxHashMap<&'static str, &'static [SyntheticStubField]> {
        &self.app_stub_layouts
    }

    /// Whether an exact `(kind, class_name, method_name, descriptor)`
    /// application workaround is registered (wildcards `""` on entry
    /// `method_name`/`descriptor` match any).
    #[inline]
    pub fn has_app_workaround(
        &self,
        kind: AppWorkaroundKind,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        self.app_workarounds.iter().any(|e| {
            e.kind == kind
                && e.class_pattern == class_name
                && (e.method_name.is_empty() || e.method_name == method_name)
                && (e.descriptor.is_empty() || e.descriptor == descriptor)
        })
    }

    /// Whether any workaround of `kind` has `class_pattern` as a prefix of
    /// `class_name`.
    #[inline]
    pub fn has_app_workaround_prefix(&self, kind: AppWorkaroundKind, class_name: &str) -> bool {
        self.app_workarounds
            .iter()
            .any(|e| e.kind == kind && class_name.starts_with(e.class_pattern))
    }

    /// Return the first matching `aux` string for `(kind, class_name)` where
    /// `class_pattern` equals `class_name` (exact) or is a prefix if `aux` is
    /// non-empty.
    #[inline]
    pub fn find_app_workaround_aux(
        &self,
        kind: AppWorkaroundKind,
        class_name: &str,
    ) -> Option<&'static str> {
        self.app_workarounds
            .iter()
            .find(|e| e.kind == kind && e.class_pattern == class_name)
            .map(|e| e.aux)
    }

    /// Return the `aux` string for an exact `(kind, class_name, method_name,
    /// descriptor)` application workaround (wildcards `""` on entry
    /// `method_name`/`descriptor` match any).
    #[inline]
    pub fn find_app_workaround_with_aux(
        &self,
        kind: AppWorkaroundKind,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<&'static str> {
        self.app_workarounds
            .iter()
            .find(|e| {
                e.kind == kind
                    && e.class_pattern == class_name
                    && (e.method_name.is_empty() || e.method_name == method_name)
                    && (e.descriptor.is_empty() || e.descriptor == descriptor)
            })
            .map(|e| e.aux)
    }

    /// Return the first registered `class_pattern` for `kind`.
    #[inline]
    pub fn first_app_workaround_class(&self, kind: AppWorkaroundKind) -> Option<&'static str> {
        self.app_workarounds
            .iter()
            .find(|e| e.kind == kind)
            .map(|e| e.class_pattern)
    }

    /// The VM-scoped strict policy in force. One field load — callers on
    /// dispatch-adjacent paths may read it per call.
    #[inline]
    pub fn compatibility_mode(&self) -> CompatibilityMode {
        self.compatibility_mode
    }

    /// `--real-jdk`: does a superclass walk that passed through `class_name`
    /// (a class that does not declare `(method_name, descriptor)`) have to
    /// ignore every `Bridge` it finds further up?
    ///
    /// Yes when `(class_name, method_name, descriptor)` is a retired shadow
    /// row. Its retirement says "the real bytecode is authoritative for this
    /// class", and strict mode refused the row -- but the dispatch doors that
    /// walk ancestors for a registered native (`invoke_or_native`, the
    /// virtual-invoke cache populate, the vtable shadow probe, the stackless
    /// step-1 walk, the JIT site cache) then found a live `Bridge` on an
    /// ancestor and ran it for every receiver of the retired class
    /// (`r13w13-shadow5-ancestor-bridges-over-retired-rows`, 19 pairs, e.g.
    /// `TreeSet.toArray()` served by `AbstractCollection`'s native). Only
    /// `Bridge` is masked: a reviewed `Intrinsic` on the ancestor still wins,
    /// as AGENTS.md's "real class bytes win" allows.
    ///
    /// Driven by the retirement tables, not by class names, so a family
    /// retired later is covered without touching dispatch. `Compatible` mode
    /// answers `false` after one field compare (the retired rows are still
    /// registered there, and the walk never reaches an ancestor for them).
    /// Kill switch: `CRATONVM_RETIRED_SHADOW_MASKS_ANCESTOR_BRIDGES=0`; one
    /// row at a time: `CRATONVM_UNRETIRE_NATIVE_SHADOW` (an unretired row is
    /// not retired here either). Round 14 wave 2, lane shadow.
    #[inline]
    pub fn retired_row_masks_ancestor_bridges(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        // `Compatible` pays one field compare unless an adopted family
        // (`retired_shadow::COMPAT_FAMILIES`) is in force; then the mask holds for
        // that family's retired rows alone, exactly as `JdkOnly` holds it for all
        // of them (SBR round 4, lane compat).
        let strict = self.compatibility_mode == CompatibilityMode::JdkOnly;
        (strict || self.compat_family_in_force)
            && self.retired_rows_mask_ancestor_bridges
            && crate::retired_shadow::triple_is_retired_shadow(class_name, method_name, descriptor)
            && (strict
                || crate::retired_shadow::compat_family_of_class(class_name).is_some_and(|f| {
                    self.hotspot_alignments.contains(f.row)
                        && self.compat_row_prerequisites_hold(f.row)
                }))
    }

    /// Do the converted dispatch doors walk ancestors for a registered native
    /// through the VM's one shared walk (`runtime::interpreter::ancestor_native`,
    /// SH14-1) rather than their own copy? The two answer the same by
    /// construction; `CRATONVM_ANCESTOR_WALK_HELPER=0` restores the copies so
    /// a divergence can be bisected to the helper. Read once at construction:
    /// the stackless walk asks on the interpreter's invoke path. Round 15
    /// wave 2, lane jdkonly.
    #[inline]
    pub fn ancestor_walk_helper(&self) -> bool {
        self.ancestor_walk_helper
    }

    /// Do the doors on the shared walk ([`Self::ancestor_walk_helper`]) judge
    /// a JVMTI-redefined ancestor by the one rule
    /// (`runtime::interpreter::redefined_ancestor_guard`), instead of the
    /// guard each door carried before (none at all for `invoke_or_native` and
    /// the JIT site cache; `break` for the populate walk)?
    /// `CRATONVM_ANCESTOR_REDEFINE_RULE=0` restores each door's own guard on
    /// the helper, so the rule and the helper can be bisected apart. Round 15
    /// wave 3, lane walk
    /// (`r15w2-jdkonly-ancestor-walk-redefine-guard-drift-FIXED-20260930.md`).
    #[inline]
    pub fn ancestor_redefine_rule(&self) -> bool {
        self.ancestor_redefine_rule
    }

    /// Do the doors on the shared walk that run what they find (the stackless
    /// step 1, the populate, `invoke_or_native`) refuse an ANCESTOR's
    /// `SyntheticStub` that yields to that ancestor's real bytecode
    /// (`runtime::interpreter::ancestor_stub_yields`), as the vtable probe, the
    /// JIT site cache and the warm revalidation already do? Before, they asked
    /// the yield of the call's own class, which registers nothing, so a
    /// subclass receiver ran the stub cold and the real bytecode warm.
    /// `CRATONVM_ANCESTOR_WALK_STUB_YIELD=0` restores that. Round 15 wave 3,
    /// lane walk.
    #[inline]
    pub fn ancestor_walk_stub_yield(&self) -> bool {
        self.ancestor_walk_stub_yield
    }

    /// Does the interpreter's cached `VirtualNative` / receiver-guarded
    /// `Intrinsic` hit judge a JVMTI redefinition by the class that OWNS the
    /// native (or declares the intrinsified method) and that class's cache-path
    /// immunity, as the populate does (WK-2)? Otherwise it asks, as before,
    /// whether ANY class from the receiver up to `java.lang.Object` was
    /// redefined and the immunity of the constant-pool class, which after one
    /// Mockito inline mock (it retransforms `Object`) evicts every such entry
    /// on every hit. `CRATONVM_REDEFINE_OWNER_SCOPED_EVICT=0` restores that.
    /// Round 15 wave 4, lane walk2
    /// (`r15w3-walk-virtual-native-hit-eviction-asks-the-hierarchy-FIXED-20260930.md`).
    #[inline]
    pub fn redefine_owner_scoped_eviction(&self) -> bool {
        self.redefine_owner_scoped_eviction
    }

    /// Does the one redefine rule of the ancestor walks
    /// (`runtime::interpreter::redefined_ancestor_guard`) keep a redefined
    /// ancestor's native for a method that ancestor declares with NO body
    /// (`ACC_NATIVE`, or no `Code`), which no agent can weave? Otherwise the
    /// native is skipped and the walk ends on that bodyless declaration as
    /// `Bytecode`, which made the JIT site cache refuse e.g. every
    /// `java.lang.Object` native site once Mockito had retransformed `Object`.
    /// `CRATONVM_ANCESTOR_REDEFINE_BODYLESS_KEEP=0` restores that. Round 15
    /// wave 4, lane walk2.
    #[inline]
    pub fn ancestor_redefine_bodyless_keep(&self) -> bool {
        self.ancestor_redefine_bodyless_keep
    }

    /// Is a native registered on a class `R` for a method `R` does NOT
    /// declare (it inherits `A.m`) judged by the redefinition of the
    /// DECLARING class `A` (`runtime::interpreter::owner_redefinition`, and
    /// the populate / vtable probe `receiver_redefined` terms), as
    /// `invoke_or_native`'s probe and HotSpot judge it? A weave of `A.m`
    /// alone then runs `A`'s woven body for every `R` receiver; before, those
    /// doors read `R`'s (or the receiver's) generation and kept `R`'s native
    /// over the advice. Requires [`Self::ancestor_redefine_rule`].
    /// `CRATONVM_REDEFINE_INHERITED_OWNER=0` restores the old reading. Round
    /// 16, lane redefine (S1 of `r15w6-sweepB-review-findings-CLOSED-20260930.md`,
    /// `r16-redefine-an-inherited-registration-skips-the-declarations-weave-FIXED-20261001.md`).
    #[inline]
    pub fn redefine_inherited_owner(&self) -> bool {
        self.redefine_inherited_owner && self.ancestor_redefine_rule
    }

    /// Does the JIT native site cache judge a JVMTI redefinition by the ONE
    /// class that can stale an entry (its owner, recorded at fill with its
    /// generation, re-checked once per redefine epoch), and apply the
    /// publishing door's redefine rule to the receiver's own native at fill,
    /// instead of declining every hit whose receiver or constant-pool class
    /// was EVER redefined? The permanent tests made one Mockito inline mock of
    /// a JDK collection send every compiled native call typed by a class in
    /// its hierarchy through the `invoke_or_native` funnel for the rest of the
    /// process. Requires [`Self::ancestor_redefine_rule`] (the owner re-check
    /// is that rule's). Default ON since round 16 (lane redefine): measured
    /// with the `R16RedefAgent` harness (`R16RedefineNatives`), the permanent
    /// tests made five compiled native sites 2-6x slower after a same-bytes
    /// retransform of their owners (`Object.hashCode`, `identityHashCode`,
    /// `getClass`, `Thread.holdsLock`), the owner scope keeps them at their
    /// pre-redefinition cost (interleaved medians, Windows box); `CRATONVM_JIT_SITE_CACHE_OWNER_REDEFINE=0` is
    /// the kill switch. Round 15 wave 4, lane walk3
    /// (`r15w4-walk2-jit-site-cache-declines-forever-after-a-redefinition-FIXED-20261001.md`).
    #[inline]
    pub fn jit_site_cache_owner_redefine(&self) -> bool {
        self.jit_site_cache_owner_redefine && self.ancestor_redefine_rule
    }

    /// May a compiled call that hits a warm JIT native-site-cache entry for a
    /// plain non-leaf native take the lean funnel
    /// (`vm_exec::safe_native_call_jit_lean`): the funnel minus the work the
    /// JIT door has already done or that cannot apply to it (the argument
    /// re-forward, the interpreter-only native-array GC hook)?
    /// `CRATONVM_JIT_NATIVE_LEAN_DOOR=1`, default OFF until measured. JIT
    /// round 17 wave 2, lane funnel
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    #[inline]
    pub fn jit_native_lean_door(&self) -> bool {
        self.jit_native_lean_door
    }

    /// Is the native-door cycle census on (`CRATONVM_DBG_NATIVE_DOOR_SPLIT=1`)?
    /// The JIT native-site door then times each crossing it serves through a
    /// funnel in three spans and reports them through
    /// [`Self::note_native_door_split`]. JIT round 17 wave 2, lane funnel.
    #[inline]
    pub fn dbg_native_door_split(&self) -> bool {
        self.dbg_native_door_split
    }

    /// Add one crossing to the native-door census (`front`: door entry to the
    /// funnel call; `funnel`: the funnel, the native included; `tail`: the
    /// funnel's return to the door's), in the caller's cycle unit, and print
    /// the per-crossing averages every 2^18 crossings:
    /// `[native-door-split] crossings=.. lean=.. front=.. funnel=.. tail=..`
    /// (cycles per crossing). Called only while
    /// [`Self::dbg_native_door_split`] is on.
    #[cold]
    pub fn note_native_door_split(&self, lean: bool, front: u64, funnel: u64, tail: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let c = &self.native_door_split;
        let n = c[0].fetch_add(1, Relaxed) + 1;
        if lean {
            c[1].fetch_add(1, Relaxed);
        }
        c[2].fetch_add(front, Relaxed);
        c[3].fetch_add(funnel, Relaxed);
        c[4].fetch_add(tail, Relaxed);
        if n & ((1 << 18) - 1) == 0 {
            eprintln!(
                "[native-door-split] crossings={n} lean={} front={} funnel={} tail={} (cycles per crossing)",
                c[1].load(Relaxed),
                c[2].load(Relaxed) / n,
                c[3].load(Relaxed) / n,
                c[4].load(Relaxed) / n,
            );
        }
    }

    /// Does the SBR5 lean native door (`jit::helpers::try_lean_native_door`,
    /// the per-site table probed at the top of both JIT dispatch helpers) send
    /// a non-leaf hit through the lean funnel (`vm_exec::safe_native_call_jit_lean`:
    /// no argument re-forward, no interpreter-only native-array GC hook)
    /// instead of `safe_native_call_prevalidated_objects`?
    /// `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAN_FUNNEL=1`, default OFF until
    /// measured. The door validates every reference itself and runs no GC
    /// point before the call, which is the lean funnel's precondition. JIT
    /// round 19 wave 2, lane funnel2 (FUNNEL17-5 residual,
    /// `docs/feature-designs/jit-ir-optimizations.md`).
    #[inline]
    pub fn jit_native_site_door_lean_funnel(&self) -> bool {
        self.jit_native_site_door_lean_funnel
    }

    /// Do the two JIT dispatch helpers' slow routes walk each reference
    /// argument ONCE (`CRATONVM_JIT_NATIVE_WALK_ONCE=1`, default OFF)? The
    /// forwarding pass then records which argument words it validated as live
    /// bases, and the native-site door reuses that answer instead of a second
    /// heap-membership walk of the receiver and of each object parameter.
    /// JIT round 19 wave 2, lane funnel2 (FUNNEL17-2).
    #[inline]
    pub fn jit_native_walk_once(&self) -> bool {
        self.jit_native_walk_once
    }

    /// Does the SBR5 lean native door file the site-cache LEAF KINDS too
    /// (`CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS=1`, default OFF)? The
    /// `ArrayList.get`/`size`, `ConcurrentHashMap.get`, `Integer.hashCode`/
    /// `equals`, `Object.getClass` and `Reference.refersTo` sites are then
    /// served by their no-wrapper probes from the per-thread table instead of
    /// the full helper's site-cache route. JIT round 19 wave 3, lane funnel3
    /// (FUNNEL2-19-2, `docs/feature-designs/jit-ir-optimizations.md`).
    #[inline]
    pub fn jit_native_site_door_leaf_kinds(&self) -> bool {
        self.jit_native_site_door_leaf_kinds
    }

    /// Is the SBR5 lean native door's table 2-way set-associative, keyed by
    /// call site AND receiver class, with a per-(site, receiver) site-cache
    /// memo behind it (`CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY=1`, default
    /// OFF; see the field above)? Two sites sharing a slot, or one site alternating between two
    /// receiver classes that resolve to two natives, then keep both records.
    /// JIT round 19 wave 3, lane funnel3 (FUNNEL2-19-1).
    #[inline]
    pub fn jit_native_site_door_two_way(&self) -> bool {
        self.jit_native_site_door_two_way
    }

    /// Do the JIT native doors skip the funnel's entry walk of the object
    /// result of a native proven to return a fresh, live base
    /// (`CRATONVM_JIT_NATIVE_FRESH_RESULT=1`, default OFF;
    /// `cratonvm_native_builtins::is_fresh_object_result_native`)? The
    /// forwarding read stays. JIT round 19 wave 3, lane funnel3 (FUNNEL2-19-3).
    #[inline]
    pub fn jit_native_fresh_result(&self) -> bool {
        self.jit_native_fresh_result
    }

    /// Does the site-cache door cache a virtual/interface site's refusal for
    /// the refusing RECEIVER CLASS only
    /// (`CRATONVM_JIT_NATIVE_SITE_REFUSAL_BY_RECEIVER=1`, default OFF)? A site
    /// whose first receiver resolved to bytecode then still serves a later
    /// receiver class whose method is a registered native (`equals` over
    /// `String` and `Integer` keys); a second refusing class makes the refusal
    /// site-wide again. JIT round 19 wave 6, lane mega6 (FUNNEL3-19-1,
    /// `docs/feature-designs/jit-ir-optimizations.md`).
    #[inline]
    pub fn jit_native_site_refusal_by_receiver(&self) -> bool {
        self.jit_native_site_refusal_by_receiver
    }

    /// Do the JIT leaf-kind probes take the reference argument a native door
    /// already validated instead of walking it again
    /// (`CRATONVM_JIT_LEAF_PROBE_VALIDATED_ARGS=1`, default on)? Applies to
    /// the `ChmGet` / `IntWrapperEquals` / `ReferenceRefersTo` kinds, on the
    /// lean door (its own walk) and on the site-cache door (the forwarding
    /// pass's mask under `CRATONVM_JIT_NATIVE_WALK_ONCE`). JIT round 19 wave
    /// 6, lane mega6 (FUNNEL3-19-2).
    #[inline]
    pub fn jit_leaf_probe_validated_args(&self) -> bool {
        self.jit_leaf_probe_validated_args
    }

    /// Is the SBR5 lean-door census on (`CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS=1`)?
    /// JIT round 19 wave 2, lane funnel2.
    #[inline]
    pub fn dbg_native_site_door_census(&self) -> bool {
        self.dbg_native_site_door_census
    }

    /// Add `add` to one lean-door census row (`1` for a counted event, a cycle
    /// span for the `Cycles*` rows). Called only while
    /// [`Self::dbg_native_site_door_census`] is on.
    #[cold]
    pub fn note_native_site_door(&self, event: NativeSiteDoorEvent, add: u64) {
        self.native_site_door_census[event as usize]
            .fetch_add(add, std::sync::atomic::Ordering::Relaxed);
    }

    /// The lean-door census as one line, or `None` while it is off. Printed at
    /// exit (`SharedVm::report_native_funnel_censuses_at_exit`).
    pub fn native_site_door_census_line(&self) -> Option<String> {
        if !self.dbg_native_site_door_census {
            return None;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let c = |e: NativeSiteDoorEvent| self.native_site_door_census[e as usize].load(Relaxed);
        let timed = c(NativeSiteDoorEvent::HitLeaf)
            + c(NativeSiteDoorEvent::HitFunnel)
            + c(NativeSiteDoorEvent::HitLeanFunnel);
        // FUNNEL2-19-2 (lane funnel3): a leaf kind's probe hit is served but
        // not timed, so the cycle sums divide by the callback crossings only.
        let served = timed + c(NativeSiteDoorEvent::HitLeafKind);
        let per = |e: NativeSiteDoorEvent| c(e).checked_div(timed).unwrap_or(0);
        Some(format!(
            "[native-site-door] probes={} served={} (leaf={} funnel={} lean_funnel={} \
             leaf_kind={}) leaf_kind_fallback={} second_way={} fresh_result={} \
             miss_empty={} miss_other_site={} decline_guard={} decline_shape={} \
             decline_receiver={} decline_capability={} | cycles per timed crossing: \
             front={} call={} tail={}",
            c(NativeSiteDoorEvent::Probe),
            served,
            c(NativeSiteDoorEvent::HitLeaf),
            c(NativeSiteDoorEvent::HitFunnel),
            c(NativeSiteDoorEvent::HitLeanFunnel),
            c(NativeSiteDoorEvent::HitLeafKind),
            c(NativeSiteDoorEvent::LeafKindFallback),
            c(NativeSiteDoorEvent::HitSecondWay),
            c(NativeSiteDoorEvent::FreshResult),
            c(NativeSiteDoorEvent::MissEmpty),
            c(NativeSiteDoorEvent::MissOtherSite),
            c(NativeSiteDoorEvent::DeclineGuard),
            c(NativeSiteDoorEvent::DeclineShape),
            c(NativeSiteDoorEvent::DeclineReceiver),
            c(NativeSiteDoorEvent::DeclineCapability),
            per(NativeSiteDoorEvent::CyclesFront),
            per(NativeSiteDoorEvent::CyclesCall),
            per(NativeSiteDoorEvent::CyclesTail),
        ))
    }

    /// Is the JDK's parallel-capable loader model in force for this VM: the
    /// `JdkOnly` policy, or `Compatible` over a real JDK with
    /// `CRATONVM_COMPAT_PARALLEL_LOADERS` (see the field). One field compare
    /// or two.
    #[inline]
    pub fn compat_parallel_capable_loaders(&self) -> bool {
        self.compatibility_mode == CompatibilityMode::JdkOnly
            || (self
                .hotspot_alignments
                .contains(cratonvm_types::HotSpotAlignment::ParallelLoaders)
                && self.real_jdk)
    }

    /// Is a `java.net.URL`'s application-provided `URLStreamHandler` the
    /// authority for its bytes, as on HotSpot: the `JdkOnly` policy, or
    /// `Compatible` with `CRATONVM_COMPAT_URL_APP_HANDLER` (see the field).
    /// One field compare or two.
    #[inline]
    pub fn url_app_handler_is_authoritative(&self) -> bool {
        self.aligned(cratonvm_types::HotSpotAlignment::UrlAppHandler)
    }

    /// Do VM-raised messages name a class's module and loader as HotSpot
    /// does -- the raise site's class ids for a `ClassCastException`'s
    /// operands, a user-defined loader by its `nameAndId`: the `JdkOnly`
    /// policy, or `Compatible` with `CRATONVM_COMPAT_MODULE_MESSAGES` (see the
    /// field). One field compare or two.
    #[inline]
    pub fn messages_name_user_loaders(&self) -> bool {
        self.aligned(cratonvm_types::HotSpotAlignment::ModuleMessages)
    }

    /// Does the `URLClassLoader` family run the JDK's own bodies: the
    /// `JdkOnly` policy, or `Compatible` over a real JDK with the
    /// `JdkUrlClassLoaders` row (interpreter round i1 wave 52, lane L5). The
    /// real JDK is required because the synthetic class library has no
    /// `URLClassPath` bytecode for the natives to fall to. One field compare
    /// or three.
    ///
    /// Windows too since wave 52's follow-up: `--compatible`'s `java.io.File`
    /// kept the `\C:\dir\` spelling the JDK's `URLClassPath$FileLoader` builds
    /// from a `file:` URL, so the row was off there until the normalisation
    /// matched `WinNTFileSystem.normalizePrefix`
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    #[inline]
    pub fn jdk_url_class_loaders(&self) -> bool {
        self.compatibility_mode == CompatibilityMode::JdkOnly
            || (self.real_jdk
                && self
                    .hotspot_alignments
                    .contains(cratonvm_types::HotSpotAlignment::JdkUrlClassLoaders))
    }

    /// Does the `java.util.jar` / `java.util.zip` archive family run the
    /// JDK's own bodies: `JdkOnly` (the 2026-09-24 `JarFile` retirement), or
    /// `Compatible` over a real JDK with the `JdkJarFileBodies` row
    /// (interpreter round i2 wave 58, lane L5)? The registrars of the natives
    /// that only existed in front of the compact `JarFile` (Spring Boot's
    /// nested-jar and launcher stand-ins, `ZipFile.close`'s `Intrinsic`) ask
    /// it; the family's own `RETIRED_SHADOW_*` rows are dropped by
    /// [`Self::register`] (`retired_shadow::is_jar_file_family_shadow`).
    pub fn jdk_jar_file_bodies(&self) -> bool {
        self.compatibility_mode == CompatibilityMode::JdkOnly
            || (self.real_jdk
                && self
                    .hotspot_alignments
                    .contains(cratonvm_types::HotSpotAlignment::JdkJarFileBodies))
    }

    /// Does this VM behave as HotSpot for `a`: the `JdkOnly` policy, or
    /// `Compatible` with the row in [`Self::hotspot_alignments`] (see the
    /// field). One compare and one bit test.
    #[inline]
    pub fn aligned(&self, a: cratonvm_types::HotSpotAlignment) -> bool {
        // A family row (`retired_shadow::COMPAT_FAMILIES`) moves a registration
        // to the JDK's own bytecode, which exists only over a real JDK; on the
        // synthetic class library `Compatible` keeps every stand-in, so the row
        // is not in force there whatever the set says (SBR round 4, lane compat).
        self.hotspot_alignments.aligned(self.compatibility_mode, a)
            && (self.compatibility_mode == CompatibilityMode::JdkOnly
                || self.real_jdk
                || !a.needs_real_jdk())
    }

    /// `CRATONVM_IMAGE_APP_MODULE_LOADERS` (see the field).
    pub fn image_app_module_loaders(&self) -> bool {
        self.image_app_module_loaders
    }

    /// `CRATONVM_NESTED_JAR_WALK_INDEX` where the `URLClassLoader` family runs
    /// the JDK's bodies (see the field): the `URLClassPath$Loader` walk
    /// intrinsics are registered.
    pub fn nested_jar_walk_index(&self) -> bool {
        self.nested_jar_walk_index && self.jdk_url_class_loaders()
    }

    /// `CRATONVM_NESTED_JAR_WALK_INDEX_STAMP` where the walk index is
    /// registered (see the field): the stamped walk intrinsics are.
    pub fn nested_jar_walk_index_stamp(&self) -> bool {
        self.nested_jar_walk_index_stamp && self.nested_jar_walk_index()
    }

    /// `CRATONVM_NESTED_JAR_SHARED_OUTER` where the walk index or the
    /// nested-jar bytes index is registered (see the field).
    pub fn nested_jar_shared_outer(&self) -> bool {
        self.nested_jar_shared_outer
            && (self.nested_jar_walk_index() || self.nested_jar_bytes_index())
    }

    /// `CRATONVM_NESTED_JAR_OUTER_INDEX` where the walk index or the
    /// nested-jar bytes index is registered (see the field).
    pub fn nested_jar_outer_index(&self) -> bool {
        self.nested_jar_outer_index
            && (self.nested_jar_walk_index() || self.nested_jar_bytes_index())
    }

    /// `CRATONVM_NESTED_JAR_REFUSAL_STAMP` where the walk index or either
    /// bytes index is registered (see the field).
    pub fn nested_jar_refusal_stamp(&self) -> bool {
        self.nested_jar_refusal_stamp
            && (self.nested_jar_walk_index()
                || self.nested_jar_bytes_index()
                || self.jar_loader_bytes_index())
    }

    /// `CRATONVM_NESTED_JAR_BYTES_DIRECTORY` where the nested-jar bytes index
    /// builds its rows from the outer index (see the field).
    pub fn nested_jar_bytes_directory(&self) -> bool {
        self.nested_jar_bytes_directory
            && self.nested_jar_bytes_index()
            && self.nested_jar_outer_index()
    }

    /// `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE` where the bytes index builds
    /// directory-only rows (see the field).
    pub fn nested_jar_outer_handle_reuse(&self) -> bool {
        self.nested_jar_outer_handle_reuse && self.nested_jar_bytes_directory()
    }

    /// `CRATONVM_NESTED_JAR_POOLED_STAT` where outer handles are reused (see
    /// the field).
    pub fn nested_jar_pooled_stat(&self) -> bool {
        self.nested_jar_pooled_stat && self.nested_jar_outer_handle_reuse()
    }

    /// `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER` where the walk index or the
    /// nested-jar bytes index is registered (see the field).
    pub fn nested_jar_class_path_outer(&self) -> bool {
        self.nested_jar_class_path_outer
            && (self.nested_jar_walk_index() || self.nested_jar_bytes_index())
    }

    /// `CRATONVM_NESTED_JAR_BYTES_INDEX` where the `URLClassLoader` family
    /// runs the JDK's bodies (see the field): the `Resource.getBytes`
    /// intrinsic is registered.
    pub fn nested_jar_bytes_index(&self) -> bool {
        self.nested_jar_bytes_index && self.jdk_url_class_loaders()
    }

    /// `CRATONVM_JAR_LOADER_BYTES_INDEX` where the `URLClassLoader` family
    /// runs the JDK's bodies (see the field): the `Resource.getBytes`
    /// intrinsic is registered with its `URLClassPath$JarLoader` half.
    pub fn jar_loader_bytes_index(&self) -> bool {
        self.jar_loader_bytes_index && self.jdk_url_class_loaders()
    }

    /// `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE` where the jar-loader bytes
    /// index is registered (see the field).
    pub fn jar_loader_class_path_archive(&self) -> bool {
        self.jar_loader_class_path_archive && self.jar_loader_bytes_index()
    }

    /// `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT` where the jar-loader bytes
    /// index is registered (see the field).
    pub fn jar_loader_class_path_direct(&self) -> bool {
        self.jar_loader_class_path_direct && self.jar_loader_bytes_index()
    }

    /// `CRATONVM_DEFINE_ONE_PARSE` (see the field).
    pub fn define_one_parse(&self) -> bool {
        self.define_one_parse
    }

    /// `CRATONVM_REAL_BUILTIN_LOADERS` (see the field; the `--real-jdk`
    /// condition is the caller's).
    pub fn real_builtin_loaders(&self) -> bool {
        self.real_builtin_loaders
    }

    /// `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY` (see the field).
    pub fn define_supertype_dictionary(&self) -> bool {
        self.define_supertype_dictionary
    }

    /// `CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION` (see the field).
    pub fn define_records_supertype_initiation(&self) -> bool {
        self.define_records_supertype_initiation
    }

    /// `CRATONVM_LOADER_KIND_MEMO` (see the field), either mode.
    pub fn loader_kind_memo(&self) -> bool {
        self.loader_kind_memo
    }

    /// Override the env-read [`Self::loader_kind_memo`] switch. Call before
    /// the VM runs Java.
    pub fn set_loader_kind_memo(&mut self, on: bool) {
        self.loader_kind_memo = on;
    }

    /// `CRATONVM_CLV_SUB_FAST_KEY` (see the field), either mode.
    pub fn clv_sub_fast_key(&self) -> bool {
        self.clv_sub_fast_key
    }

    /// Override the env-read [`Self::clv_sub_fast_key`] switch.
    pub fn set_clv_sub_fast_key(&mut self, on: bool) {
        self.clv_sub_fast_key = on;
    }

    /// `CRATONVM_LOADER_NAMESPACE_INDEX` (see the field), either mode.
    pub fn loader_namespace_index(&self) -> bool {
        self.loader_namespace_index
    }

    /// Override the env-read [`Self::loader_namespace_index`] switch. Call
    /// before the VM runs Java.
    pub fn set_loader_namespace_index(&mut self, on: bool) {
        self.loader_namespace_index = on;
    }

    /// `CRATONVM_PLATFORM_RESOURCE_JRT_WALK` (see the field).
    pub fn platform_resource_jrt_walk(&self) -> bool {
        self.platform_resource_jrt_walk
    }

    /// `CRATONVM_INFLATER_REUSE_BUFFERS` (see the field).
    pub fn inflater_reuse_buffers(&self) -> bool {
        self.inflater_reuse_buffers
    }

    /// `CRATONVM_INFLATER_INPUT_WINDOW` (see the field).
    pub fn inflater_input_window(&self) -> bool {
        self.inflater_input_window
    }

    /// `CRATONVM_INFLATER_RAW_ADLER_ZERO` (see the field).
    pub fn inflater_raw_adler_zero(&self) -> bool {
        self.inflater_raw_adler_zero
    }

    /// `CRATONVM_INFLATER_SHARDED_TABLE` (see the field).
    pub fn inflater_sharded_table(&self) -> bool {
        self.inflater_sharded_table
    }

    /// `CRATONVM_ARRAYS_MISMATCH_BULK` (see the field).
    pub fn arrays_mismatch_bulk(&self) -> bool {
        self.arrays_mismatch_bulk
    }

    /// `CRATONVM_ZIP_UTILS_INTRINSICS` (see the field).
    pub fn zip_utils_intrinsics(&self) -> bool {
        self.zip_utils_intrinsics
    }

    /// `CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS` (see the field).
    pub fn zip_cen_accessor_intrinsics(&self) -> bool {
        self.zip_cen_accessor_intrinsics
    }

    /// `CRATONVM_ZIP_CODER_COMPARE_INTRINSIC` (see the field).
    pub fn zip_coder_compare_intrinsic(&self) -> bool {
        self.zip_coder_compare_intrinsic
    }

    /// `CRATONVM_JIT_ZIP_HEADER_LOADS` (see the field).
    pub fn jit_zip_header_loads(&self) -> bool {
        self.jit_zip_header_loads
    }

    /// Do the `ZipUtils` field-read intrinsics (`get16` / `get32` and the CEN
    /// / LOC accessors) yield to the JDK bytecode in this VM? Only with
    /// `CRATONVM_JIT_ZIP_HEADER_LOADS` on AND a JIT (see the field).
    pub fn zip_header_reads_yield_to_jit(&self) -> bool {
        self.jit_zip_header_loads && self.jit_enabled
    }

    /// `CRATONVM_ZIP_ENTRY_MISS_INTRINSIC` (see the field).
    pub fn zip_entry_miss_intrinsic(&self) -> bool {
        self.zip_entry_miss_intrinsic
    }

    /// `CRATONVM_ZIP_ENTRY_HIT_INTRINSIC` (see the field).
    pub fn zip_entry_hit_intrinsic(&self) -> bool {
        self.zip_entry_hit_intrinsic
    }

    /// Override the env-read [`Self::zip_entry_hit_intrinsic`] switch. Call
    /// before the `register_*` passes: the switch decides a registration.
    pub fn set_zip_entry_hit_intrinsic(&mut self, on: bool) {
        self.zip_entry_hit_intrinsic = on;
    }

    /// `CRATONVM_INTRINSIC_BODY_COMPILES` (see the field).
    pub fn intrinsic_body_compiles(&self) -> bool {
        self.intrinsic_body_compiles
    }

    /// Override the env-read [`Self::intrinsic_body_compiles`] switch. Call
    /// before the `register_*` passes: the switch decides registrations.
    pub fn set_intrinsic_body_compiles(&mut self, on: bool) {
        self.intrinsic_body_compiles = on;
    }

    /// Does a "worth it interpreted" `Intrinsic` -- one that loses to the
    /// compiled JDK body under a JIT, and so was registered only without one
    /// -- register in this VM? Without a JIT always; with one only under
    /// [`Self::intrinsic_body_compiles`], where its registrar must also call
    /// [`Self::mark_body_compilable_intrinsic`] so the body still compiles.
    pub fn interpreted_intrinsic_registers(&self) -> bool {
        !self.jit_enabled || self.intrinsic_body_compiles
    }

    /// Mark the `Intrinsic` row `(class, method, descriptor)` as one whose JDK
    /// body the JIT may still compile and bind compiled callers to
    /// ([`Self::is_body_compilable_intrinsic`]). Nothing unless
    /// [`Self::intrinsic_body_compiles`]. The row's kind is the caller's: this
    /// changes no dispatch decision of the interpreter, which keeps running
    /// the intrinsic at an interpreted call.
    pub fn mark_body_compilable_intrinsic(
        &mut self,
        class: &'static str,
        method: &'static str,
        descriptor: &'static str,
    ) {
        if self.intrinsic_body_compiles
            && !self
                .body_compilable_intrinsics
                .iter()
                .any(|&(c, m, d)| c == class && m == method && d == descriptor)
        {
            self.body_compilable_intrinsics
                .push((class, method, descriptor));
        }
    }

    /// Name `(class, method, descriptor)` as an `Intrinsic` whose native
    /// re-enters the JDK's own bytecode (`invoke_*_bytecode_only`), so the VM
    /// may publish that body re-entry-only ([`Self::reentry_only_bodies`]).
    /// Changes no dispatch decision and no row's kind. Idempotent.
    /// Interpreter round i6 wave 121, lane L2.
    pub fn mark_reentry_only_body(
        &mut self,
        class: &'static str,
        method: &'static str,
        descriptor: &'static str,
    ) {
        if !self
            .reentry_only_bodies
            .iter()
            .any(|&(c, m, d)| c == class && m == method && d == descriptor)
        {
            self.reentry_only_bodies.push((class, method, descriptor));
        }
    }

    /// The triples named by [`Self::mark_reentry_only_body`], in marking
    /// order. Read once at VM init.
    pub fn reentry_only_bodies(&self) -> &[(&'static str, &'static str, &'static str)] {
        &self.reentry_only_bodies
    }

    /// [`Self::mark_reentry_only_body`], plus: the VM publishes this row
    /// re-entry-only whatever the cache-wide
    /// `CRATONVM_JIT_REENTRY_ONLY_PUBLICATION` says
    /// ([`Self::reentry_only_bodies_own_switch`]). The REGISTRAR asks the
    /// row's own switch before calling this. Changes no dispatch decision and
    /// no row's kind. Idempotent. JIT round 23 wave 8h, lane W8-L1.
    pub fn mark_reentry_only_body_on_its_own_switch(
        &mut self,
        class: &'static str,
        method: &'static str,
        descriptor: &'static str,
    ) {
        self.mark_reentry_only_body(class, method, descriptor);
        if !self
            .reentry_only_bodies_own_switch
            .iter()
            .any(|&(c, m, d)| c == class && m == method && d == descriptor)
        {
            self.reentry_only_bodies_own_switch
                .push((class, method, descriptor));
        }
    }

    /// The triples named by [`Self::mark_reentry_only_body_on_its_own_switch`].
    /// Read once at VM init.
    pub fn reentry_only_bodies_own_switch(&self) -> &[(&'static str, &'static str, &'static str)] {
        &self.reentry_only_bodies_own_switch
    }

    /// Does the row named `row` (a token of
    /// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`) take the body-compiles path:
    /// the global [`Self::intrinsic_body_compiles`], or the row listed?
    pub fn intrinsic_body_compiles_row(&self, row: &str) -> bool {
        self.intrinsic_body_compiles || self.intrinsic_body_compiles_rows.iter().any(|r| r == row)
    }

    /// Override the env-read `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS` list
    /// (same parse). Call before the `register_*` passes.
    pub fn set_intrinsic_body_compiles_rows(&mut self, rows: &str) {
        self.intrinsic_body_compiles_rows = parse_intrinsic_body_compiles_rows(rows);
    }

    /// [`Self::interpreted_intrinsic_registers`] for one row: no JIT, or
    /// [`Self::intrinsic_body_compiles_row`]. Its registrar marks with
    /// [`Self::mark_body_compilable_intrinsic_row`].
    pub fn interpreted_intrinsic_registers_row(&self, row: &str) -> bool {
        !self.jit_enabled || self.intrinsic_body_compiles_row(row)
    }

    /// [`Self::mark_body_compilable_intrinsic`] for one row: kept when
    /// [`Self::intrinsic_body_compiles_row`] admits `row`.
    pub fn mark_body_compilable_intrinsic_row(
        &mut self,
        row: &str,
        class: &'static str,
        method: &'static str,
        descriptor: &'static str,
    ) {
        if self.intrinsic_body_compiles_row(row)
            && !self
                .body_compilable_intrinsics
                .iter()
                .any(|&(c, m, d)| c == class && m == method && d == descriptor)
        {
            self.body_compilable_intrinsics
                .push((class, method, descriptor));
        }
    }

    /// Is `(class, method, descriptor)` a marked body-compilable `Intrinsic`
    /// ([`Self::mark_body_compilable_intrinsic`])? `false` without
    /// [`Self::intrinsic_body_compiles`] or a row of
    /// `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`. Asked by the compile gates only.
    pub fn is_body_compilable_intrinsic(
        &self,
        class: &str,
        method: &str,
        descriptor: &str,
    ) -> bool {
        (self.intrinsic_body_compiles || !self.intrinsic_body_compiles_rows.is_empty())
            && self
                .body_compilable_intrinsics
                .iter()
                .any(|&(c, m, d)| c == class && m == method && d == descriptor)
    }

    /// This VM's slot for the entry-miss intrinsic's `ZipFile$Source` field
    /// slots (see the field).
    pub fn zip_source_slot_cache(
        &self,
    ) -> &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> {
        &self.zip_source_slots
    }

    /// This VM's slot for the record `boolean`-component memo (see the field).
    pub fn record_boolean_components_slot(
        &self,
    ) -> &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> {
        &self.record_boolean_components
    }

    /// This VM's slot for the `CRATONVM_VH_REAL_VARFORM` record (see the field).
    pub fn vh_real_varform_slot(
        &self,
    ) -> &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> {
        &self.vh_real_varform
    }

    /// Record that a hidden-frame-showing walk ran in this VM (see the field).
    pub fn note_hidden_frames_observed(&self) {
        if !self.hidden_frames_observed.load(std::sync::atomic::Ordering::Relaxed) {
            self.hidden_frames_observed
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Has a hidden-frame-showing walk run in this VM? (See the field.)
    pub fn hidden_frames_observed(&self) -> bool {
        self.hidden_frames_observed
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn hidden_frames_observed_addr(&self) -> usize {
        &self.hidden_frames_observed as *const std::sync::atomic::AtomicBool as usize
    }

    /// `CRATONVM_REFERENCE_LAYOUT_MEMO` (see the field), either mode.
    pub fn reference_layout_memo(&self) -> bool {
        self.reference_layout_memo
    }

    /// Override the env-read [`Self::reference_layout_memo`] switch. Call
    /// before the `register_*` passes: the switch decides a registration.
    pub fn set_reference_layout_memo(&mut self, on: bool) {
        self.reference_layout_memo = on;
    }

    /// This VM's slot for the `java.lang.ref` layout memo (see the field).
    pub fn reference_layout_slot(
        &self,
    ) -> &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> {
        &self.reference_layout
    }

    /// This VM's slot for `native-io`'s process tables (see the field).
    pub fn process_tables_slot(
        &self,
    ) -> &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> {
        &self.process_tables
    }

    /// This VM's slot for the `Resource.getBytes` intrinsics' archive caches
    /// (see the field).
    pub fn archive_bytes_cache_slot(
        &self,
    ) -> &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> {
        &self.archive_bytes_cache
    }

    /// `CRATONVM_COMPAT_URL_LOADER_DEFINED_PACKAGES` (see the field).
    pub fn compat_url_loader_defined_packages(&self) -> bool {
        self.compat_url_loader_defined_packages
    }

    /// `CRATONVM_STRING_LITERAL_IDENTITY` (see the field).
    pub fn string_literal_identity(&self) -> bool {
        self.string_literal_identity
    }

    /// `CRATONVM_BUILTIN_LOADER_PACKAGE_SYNC` over a real JDK (see the field).
    pub fn builtin_loader_package_sync(&self) -> bool {
        self.builtin_loader_package_sync && self.real_jdk
    }

    /// `CRATONVM_RETIRE_WALK_CARRIER` over a real JDK (see the field): the
    /// synthetic-JDK VM, whose walks have no JDK frame classes to build,
    /// keeps the carrier.
    pub fn walk_carrier_retired(&self) -> bool {
        self.walk_carrier_retired && self.real_jdk
    }

    /// `CRATONVM_COMPATIBLE_JDK_REIFIER` under `Compatible` over a real JDK
    /// (see the field): the generic-signature natives are not registered.
    /// `JdkOnly` never registers them anyway (ambient stubs), and the
    /// synthetic-JDK VM, which has no JDK repository classes, keeps them.
    pub fn compatible_jdk_reifier(&self) -> bool {
        self.compatible_jdk_reifier
            && self.real_jdk
            && self.compatibility_mode == CompatibilityMode::Compatible
    }

    /// `CRATONVM_JDK_TYPE_ANNOTATIONS` over a real JDK that answers the
    /// type-annotation bytes (see the field), either mode.
    pub fn jdk_type_annotations(&self) -> bool {
        self.jdk_type_annotations && self.real_jdk && self.type_annotation_bytes_real
    }

    /// `CRATONVM_JDK_DECLARATION_ANNOTATIONS` under `Compatible`, over a JDK
    /// whose `Class.getRawAnnotations` answers the class file's bytes (which
    /// [`Self::jdk_type_annotations`] implies): see the field. `JdkOnly` runs
    /// the JDK's parser anyway.
    pub fn jdk_declaration_annotations(&self) -> bool {
        self.jdk_declaration_annotations
            && self.compatibility_mode == CompatibilityMode::Compatible
            && self.jdk_type_annotations()
    }

    /// `CRATONVM_REFLECT_CLASS_ACCESS` (see the field), either mode.
    pub fn reflect_class_access(&self) -> bool {
        self.reflect_class_access
    }

    /// `CRATONVM_INNER_CLASSES_AGREEMENT` (see the field), either mode.
    pub fn inner_classes_agreement(&self) -> bool {
        self.inner_classes_agreement
    }

    /// `CRATONVM_JDK_REFLECTIVE_INVOKE` over a real JDK, either mode (see the
    /// field). `--compatible` too: its natives skipped the accessor's `asType`
    /// adapter frame and the direct handle's customization, which a
    /// `SHOW_HIDDEN_FRAMES` walk sees
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    pub fn jdk_reflective_invoke(&self) -> bool {
        self.jdk_reflective_invoke && self.real_jdk
    }

    /// `CRATONVM_FIELD_DOORS_INTRINSIC` under `JdkOnly` over a real JDK (see
    /// the field). `Compatible` never reads it: its `Field` doors are already
    /// served by the same natives, as `Bridge`s.
    pub fn jdk_only_field_doors(&self) -> bool {
        self.jdk_only_field_doors
            && self.real_jdk
            && self.compatibility_mode == CompatibilityMode::JdkOnly
    }

    /// Override the env-read [`Self::jdk_only_field_doors`] switch (tests, and
    /// an embedder that configures the registry itself). Call before the
    /// `register_*` passes: the switch decides a registration KIND.
    pub fn set_jdk_only_field_doors(&mut self, on: bool) {
        self.jdk_only_field_doors = on;
    }

    /// `CRATONVM_STACK_TRACE_ELEMENT_OF_INTRINSIC` under `JdkOnly` over a
    /// real JDK (see the field). `Compatible` never reads it: its
    /// `Throwable.getStackTrace()` is served whole by a native and does not
    /// reach `StackTraceElement.of`.
    pub fn stack_trace_element_of_intrinsic(&self) -> bool {
        self.stack_trace_element_of_intrinsic
            && self.real_jdk
            && self.compatibility_mode == CompatibilityMode::JdkOnly
    }

    /// Override the env-read [`Self::stack_trace_element_of_intrinsic`]
    /// switch (tests, and an embedder that configures the registry itself).
    /// Call before the `register_*` passes: the switch decides a registration.
    pub fn set_stack_trace_element_of_intrinsic(&mut self, on: bool) {
        self.stack_trace_element_of_intrinsic = on;
    }

    /// `CRATONVM_REFLECT_INVOKE_PLAN` (see the field), either mode.
    pub fn reflect_invoke_plan(&self) -> bool {
        self.reflect_invoke_plan
    }

    /// Override the env-read [`Self::reflect_invoke_plan`] switch (tests, and
    /// an embedder that configures the registry itself).
    pub fn set_reflect_invoke_plan(&mut self, on: bool) {
        self.reflect_invoke_plan = on;
    }

    /// This VM's slot for the served reflective doors' caches (see the
    /// field).
    pub fn reflect_door_cache_slot(
        &self,
    ) -> &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> {
        &self.reflect_door_caches
    }

    /// `CRATONVM_FIELD_DOOR_IDENT_MEMO` (see the field), either mode.
    pub fn field_door_ident_memo(&self) -> bool {
        self.field_door_ident_memo
    }

    /// Override the env-read [`Self::field_door_ident_memo`] switch.
    pub fn set_field_door_ident_memo(&mut self, on: bool) {
        self.field_door_ident_memo = on;
    }

    /// `CRATONVM_STACK_TRACE_ORIGIN_RECORD` (see the field), either mode.
    pub fn stack_trace_origin_record(&self) -> bool {
        self.stack_trace_origin_record
    }

    /// Override the env-read [`Self::stack_trace_origin_record`] switch.
    pub fn set_stack_trace_origin_record(&mut self, on: bool) {
        self.stack_trace_origin_record = on;
    }

    /// This VM's slot for the frame-origin records (see the field).
    pub fn stack_trace_origin_record_slot(
        &self,
    ) -> &std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>> {
        &self.stack_trace_origin_records
    }

    /// `CRATONVM_STACK_TRACE_FILL_NAME_FROM_TEXT` (see the field), either mode.
    pub fn stack_trace_fill_name_from_text(&self) -> bool {
        self.stack_trace_fill_name_from_text
    }

    /// Override the env-read [`Self::stack_trace_fill_name_from_text`] switch.
    pub fn set_stack_trace_fill_name_from_text(&mut self, on: bool) {
        self.stack_trace_fill_name_from_text = on;
    }

    /// `CRATONVM_STACK_TRACE_ELEMENT_TO_STRING_INTRINSIC` under `JdkOnly`
    /// over a real JDK (see the field). `Compatible` never reads it.
    pub fn stack_trace_element_to_string_intrinsic(&self) -> bool {
        self.stack_trace_element_to_string_intrinsic
            && self.real_jdk
            && self.compatibility_mode == CompatibilityMode::JdkOnly
    }

    /// Override the env-read [`Self::stack_trace_element_to_string_intrinsic`]
    /// switch. Call before the `register_*` passes: the switch decides a
    /// registration.
    pub fn set_stack_trace_element_to_string_intrinsic(&mut self, on: bool) {
        self.stack_trace_element_to_string_intrinsic = on;
    }

    /// `CRATONVM_STACK_TRACE_ELEMENT_OF_FRAME_INTRINSIC` under `JdkOnly`
    /// over a real JDK (see the field). `Compatible` reads it only with
    /// [`Self::compat_sfi_element_jdk_body`] on: otherwise its
    /// `StackFrameInfo.toStackTraceElement()` is served by a native and does
    /// not reach `StackTraceElement.of`.
    pub fn stack_trace_element_of_frame_intrinsic(&self) -> bool {
        self.stack_trace_element_of_frame_intrinsic
            && self.real_jdk
            && (self.compatibility_mode == CompatibilityMode::JdkOnly
                || self.compat_sfi_element_jdk_body())
    }

    /// `CRATONVM_COMPAT_SFI_ELEMENT_JDK_BODY` under `Compatible` over a real
    /// JDK (see the field): `register_lang_stackwalker` leaves
    /// `StackFrameInfo.toStackTraceElement()` to its JDK body. `JdkOnly`
    /// never reads it (the native is a retired shadow there already).
    pub fn compat_sfi_element_jdk_body(&self) -> bool {
        self.compat_sfi_element_jdk_body
            && self.real_jdk
            && self.compatibility_mode == CompatibilityMode::Compatible
    }

    /// Override the env-read [`Self::compat_sfi_element_jdk_body`] switch.
    /// Call before the `register_*` passes: the switch decides a
    /// registration.
    pub fn set_compat_sfi_element_jdk_body(&mut self, on: bool) {
        self.compat_sfi_element_jdk_body = on;
    }

    /// Override the env-read [`Self::stack_trace_element_of_frame_intrinsic`]
    /// switch. Call before the `register_*` passes: the switch decides a
    /// registration.
    pub fn set_stack_trace_element_of_frame_intrinsic(&mut self, on: bool) {
        self.stack_trace_element_of_frame_intrinsic = on;
    }

    /// `CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY` (see the field), either
    /// mode: only the native `Throwable.getStackTrace()` reads it, and only
    /// `--compatible` dispatches that native.
    pub fn compat_stack_trace_keeps_array(&self) -> bool {
        self.compat_stack_trace_keeps_array
    }

    /// Override the env-read [`Self::compat_stack_trace_keeps_array`] switch.
    pub fn set_compat_stack_trace_keeps_array(&mut self, on: bool) {
        self.compat_stack_trace_keeps_array = on;
    }

    /// The `StackTraceElement` fill's behaviour switches (see the field),
    /// either mode.
    pub fn stack_trace_fill_switches(&self) -> StackTraceFillSwitches {
        self.stack_trace_fill_switches
    }

    /// Override the env-read [`Self::stack_trace_fill_switches`].
    pub fn set_stack_trace_fill_switches(&mut self, switches: StackTraceFillSwitches) {
        self.stack_trace_fill_switches = switches;
    }

    /// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST` over a real JDK (see the
    /// field), either mode. The registrar adds its own JDK-version and
    /// real-AQS conditions.
    pub fn reentrant_lock_uncontended_fast(&self) -> bool {
        self.reentrant_lock_uncontended_fast && self.real_jdk
    }

    /// Override the env-read [`Self::reentrant_lock_uncontended_fast`]
    /// switch. Call before the `register_*` passes: the switch decides a
    /// registration.
    pub fn set_reentrant_lock_uncontended_fast(&mut self, on: bool) {
        self.reentrant_lock_uncontended_fast = on;
    }

    /// `CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL` (see the field): the
    /// diagnostic that makes the two `ReentrantLock` intrinsics decline every
    /// call. Meaningful only with [`Self::reentrant_lock_uncontended_fast`].
    pub fn reentrant_lock_fast_decline_all(&self) -> bool {
        self.reentrant_lock_fast_decline_all
    }

    /// Override the env-read [`Self::reentrant_lock_fast_decline_all`]
    /// switch (before the `register_*` passes).
    pub fn set_reentrant_lock_fast_decline_all(&mut self, on: bool) {
        self.reentrant_lock_fast_decline_all = on;
    }

    /// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE` (see the field): the
    /// `lockInterruptibly()` / `tryLock()` rows of the `ReentrantLock` fast
    /// path. Meaningful only with [`Self::reentrant_lock_uncontended_fast`].
    pub fn reentrant_lock_fast_wide(&self) -> bool {
        self.reentrant_lock_fast_wide
    }

    /// Override the env-read [`Self::reentrant_lock_fast_wide`] switch
    /// (before the `register_*` passes: it decides two registrations).
    pub fn set_reentrant_lock_fast_wide(&mut self, on: bool) {
        self.reentrant_lock_fast_wide = on;
    }

    /// `CRATONVM_REFLECT_INVOKE_SETTLED` (see the field), either mode.
    pub fn reflect_invoke_settled(&self) -> bool {
        self.reflect_invoke_settled
    }

    /// Override the env-read [`Self::reflect_invoke_settled`] switch.
    pub fn set_reflect_invoke_settled(&mut self, on: bool) {
        self.reflect_invoke_settled = on;
    }

    /// `CRATONVM_REFLECT_CTOR_PLAN_PER_VM` (see the field), either mode.
    pub fn reflect_ctor_plan_per_vm(&self) -> bool {
        self.reflect_ctor_plan_per_vm
    }

    /// Override the env-read [`Self::reflect_ctor_plan_per_vm`] switch.
    pub fn set_reflect_ctor_plan_per_vm(&mut self, on: bool) {
        self.reflect_ctor_plan_per_vm = on;
    }

    /// `CRATONVM_REFLECT_DOOR_WORD_BATCH` (see the field), either mode.
    pub fn reflect_door_word_batch(&self) -> bool {
        self.reflect_door_word_batch
    }

    /// Override the env-read [`Self::reflect_door_word_batch`] switch.
    pub fn set_reflect_door_word_batch(&mut self, on: bool) {
        self.reflect_door_word_batch = on;
    }

    /// `CRATONVM_REFLECT_DOOR_PHASE_STATS` (see the field), either mode.
    pub fn reflect_door_phase_stats(&self) -> bool {
        self.reflect_door_phase_stats
    }

    /// Override the env-read [`Self::reflect_door_phase_stats`] switch.
    pub fn set_reflect_door_phase_stats(&mut self, on: bool) {
        self.reflect_door_phase_stats = on;
    }

    /// `CRATONVM_JIT_REFLECT_DOOR_DIRECT` (see the field), either mode.
    #[inline]
    pub fn jit_reflect_door_direct(&self) -> bool {
        self.jit_reflect_door_direct
    }

    /// Override the env-read [`Self::jit_reflect_door_direct`] switch.
    pub fn set_jit_reflect_door_direct(&mut self, on: bool) {
        self.jit_reflect_door_direct = on;
    }

    /// `CRATONVM_REFLECT_INVOKE_PLAN_FACTS` (see the field), either mode.
    pub fn reflect_invoke_plan_facts(&self) -> bool {
        self.reflect_invoke_plan_facts
    }

    /// Override the env-read [`Self::reflect_invoke_plan_facts`] switch.
    pub fn set_reflect_invoke_plan_facts(&mut self, on: bool) {
        self.reflect_invoke_plan_facts = on;
    }

    /// `CRATONVM_REFLECT_NO_EXPORT_GATE` is set (see the field).
    pub fn reflect_export_gate_disabled(&self) -> bool {
        self.reflect_export_gate_disabled
    }

    /// The kind the `Field` value doors are registered under: a reviewed
    /// `Intrinsic` when [`Self::jdk_only_field_doors`] holds; `SyntheticStub`
    /// under `--real-jdk` over a real JDK with the switch off (`=0`), so the
    /// strict boot refuses them and the JDK's `Field` bytecode runs, exactly
    /// what their row in `RETIRED_SHADOW_SBR2_CALLER_TRIPLES` did until wave
    /// 78 took them out of that table (record
    /// `docs/feature-designs/native-dispatch-architecture.md`); else
    /// `ambient` (the registrar's own category, `Bridge` for the essential
    /// natives).
    pub fn field_door_kind(&self, ambient: NativeKind) -> NativeKind {
        if self.jdk_only_field_doors() {
            NativeKind::Intrinsic
        } else if self.real_jdk && self.compatibility_mode == CompatibilityMode::JdkOnly {
            NativeKind::SyntheticStub
        } else {
            ambient
        }
    }

    /// Whether the type-annotation byte natives answer the class file's bytes
    /// (see the field). Call before the `register_*` passes.
    pub fn set_type_annotation_bytes_real(&mut self, real: bool) {
        self.type_annotation_bytes_real = real;
    }

    /// `CRATONVM_RESOURCE_STREAMS_OPEN_URL` where resource URLs are the JDK's
    /// own (see the field).
    pub fn resource_streams_open_url(&self) -> bool {
        self.resource_streams_open_url && self.jdk_url_class_loaders()
    }

    /// `CRATONVM_BOOT_APPEND_PACKAGE_TABLE` (see the field).
    pub fn boot_append_package_table(&self) -> bool {
        self.boot_append_package_table
    }

    /// `CRATONVM_JDK_BUILT_PACKAGES` where the archive family is the JDK's
    /// ([`Self::jdk_jar_file_bodies`]: `--real-jdk`, or `--compatible` over a
    /// real JDK with the `JdkJarFileBodies` row), see the field. Without the
    /// row, `--compatible`'s `Manifest(InputStream)` stand-in parses the
    /// manifest (it drops a value's trailing spaces, measured, wave 58
    /// `L5W58JdkBuiltPackages` `specTitle=[ padded]`), so the old path, which
    /// keeps them, stays.
    pub fn jdk_built_packages(&self) -> bool {
        self.jdk_built_packages && self.real_jdk && self.jdk_jar_file_bodies()
    }
    /// `CRATONVM_DEFINED_CLASS_RESOURCES_DEFINING_LOADER` (see the field).
    pub fn defined_class_resources_defining_loader(&self) -> bool {
        self.defined_class_resources_defining_loader
    }
    /// `CRATONVM_IMPL_JARS_ES_LOADER_ONLY` (see the field).
    pub fn impl_jars_es_loader_only(&self) -> bool {
        self.impl_jars_es_loader_only
    }

    /// `CRATONVM_LOADER_PER_NAME_LOCK` where the JDK's parallel-capable
    /// loader model is in force (see the field).
    pub fn per_name_loader_locks(&self) -> bool {
        self.per_name_loader_locks && self.compat_parallel_capable_loaders()
    }

    /// `CRATONVM_CL_DELEGATES_TO_URL_PARENT` (see the field).
    pub fn delegates_to_url_parent(&self) -> bool {
        self.delegates_to_url_parent
    }

    /// `CRATONVM_RESOLVER_ABSENCE_SHAPE` (see the field).
    pub fn resolver_absence_shape(&self) -> bool {
        self.resolver_absence_shape
    }

    /// The `--compatible` rows in force (see the field).
    pub fn hotspot_alignments(&self) -> cratonvm_types::HotSpotAlignments {
        self.hotspot_alignments
    }

    /// Install the VM's rows (`vm_init`, before the first `register_*`: the
    /// `ParallelLoaders` row gates registration), replacing the set read at
    /// construction so the class manager, the class realm and this registry
    /// answer from one read.
    pub fn set_hotspot_alignments(&mut self, set: cratonvm_types::HotSpotAlignments) {
        self.hotspot_alignments = set;
        self.refresh_compat_family_in_force();
    }

    /// Declare that this registry is being populated for a REAL JDK class
    /// library. See [`Self::real_jdk`]; call once, before the `register_*`
    /// pass.
    pub fn set_real_jdk(&mut self, real_jdk: bool) {
        self.real_jdk = real_jdk;
        self.refresh_compat_family_in_force();
    }

    /// Does the VM have what a `--compatible` alignment row needs before
    /// [`Self::register`] may drop that row's registrations? Every row but one
    /// needs nothing beyond its bit and a real JDK. `JdkObjectIdentityBodies`
    /// (`Object.equals` / `toString`; nio20 wave 2, L5) needs the VM to build
    /// the JDK's generic-reflection `*Impl` classes instead of its
    /// interface-classed carriers, the only receivers the stand-ins answered
    /// otherwise than the JDK's bodies -- which is
    /// [`Self::set_type_annotation_bytes_real`]'s input (`--real-jdk`, or
    /// `CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`), set before the registrars
    /// run. Cold: registration only.
    pub(in crate::registry) fn compat_row_prerequisites_hold(&self, row: cratonvm_types::HotSpotAlignment) -> bool {
        match row {
            cratonvm_types::HotSpotAlignment::JdkObjectIdentityBodies => {
                self.type_annotation_bytes_real
            }
            // nio20 wave 4 (L4): the JDK's `HttpClientImpl` drives a real
            // `Selector` over real `SocketChannel`s; over the VM's channel
            // carriers (`-jdk-nio-channels`) it would be the half-family the
            // NIOREAL record warns against, so the client stays native there.
            // nio20 wave 5 (L1): the same for the JDK's `ServerImpl`, which
            // drives a real `Selector` over a real `ServerSocketChannel`.
            cratonvm_types::HotSpotAlignment::JdkHttpClient
            | cratonvm_types::HotSpotAlignment::JdkHttpServer => self
                .hotspot_alignments
                .contains(cratonvm_types::HotSpotAlignment::JdkNioChannels),
            _ => true,
        }
    }

    pub(in crate::registry) fn refresh_compat_family_in_force(&mut self) {
        self.compat_family_in_force = self.real_jdk
            && crate::retired_shadow::COMPAT_FAMILIES
                .iter()
                .any(|f| self.hotspot_alignments.contains(f.row));
    }

    /// Is a real JDK class library in use? Read by registrars deciding whether
    /// a synthetic-library stand-in is worth registering at all.
    #[inline]
    pub fn real_jdk(&self) -> bool {
        self.real_jdk
    }

    /// Declare whether this VM runs a JIT (see the field); call once, before
    /// the `register_*` pass.
    pub fn set_jit_enabled(&mut self, on: bool) {
        self.jit_enabled = on;
    }

    /// Does this VM run a JIT? `true` unless VM init said otherwise.
    #[inline]
    pub fn jit_enabled(&self) -> bool {
        self.jit_enabled
    }

    /// Declare whether the JDK drives virtual threads (see the field); call
    /// once, before the `register_*` pass.
    pub fn set_jdk_continuations(&mut self, on: bool) {
        self.jdk_continuations = on;
    }

    /// Does the JDK drive virtual threads? `false` unless VM init said so.
    #[inline]
    pub fn jdk_continuations(&self) -> bool {
        self.jdk_continuations
    }

    /// Record the real image's feature version (see the field); call once,
    /// before the `register_*` pass, like [`Self::set_real_jdk`].
    pub fn set_jdk_feature(&mut self, feature: Option<u32>) {
        self.jdk_feature = feature;
    }

    /// The real image's feature version [`Self::set_jdk_feature`] recorded
    /// (`None`: unknown or synthetic).
    #[inline]
    pub fn jdk_feature(&self) -> Option<u32> {
        self.jdk_feature
    }

    /// Is the class library a real JDK image OLDER than `feature`? `false`
    /// when the version is unknown, so a registry nobody configures keeps
    /// the newest image's choices.
    #[inline]
    pub fn real_jdk_older_than(&self, feature: u32) -> bool {
        self.real_jdk && self.jdk_feature.is_some_and(|f| f < feature)
    }

    /// Is the class library a real JDK image KNOWN to be `feature` or newer?
    /// `false` when the version is unknown -- the complement of
    /// [`Self::real_jdk_older_than`] for a registrar whose stand-in exists only
    /// for images that lack a method, and must yield to the image's own
    /// bytecode where it has it.
    #[inline]
    pub fn real_jdk_at_least(&self, feature: u32) -> bool {
        self.real_jdk && self.jdk_feature.is_some_and(|f| f >= feature)
    }

    /// Registrations refused because of `JdkOnly`, in registration order.
    /// Always empty under `Compatible`.
    pub fn refused_registrations(&self) -> &[JdkOnlyViolation] {
        &self.refused
    }

    /// The refusals that did **not** retire their method — every
    /// [`JdkOnlyViolation::SyntheticNativeRegistered`] whose triple was already
    /// owned, so an earlier native survived and still serves in strict mode.
    ///
    /// Returns `(class, method, descriptor, survivor)`. This is the species
    /// `registrar_drift.rs` is structurally blind to (it compares a
    /// synthetic-only pass against a shipping one, and this is two SHIPPING
    /// passes) and that `duplicate_registration_gate.rs` names as its blind
    /// spot 3 (a dropped registration leaves no census row at all).
    pub fn refusals_that_left_a_survivor(&self) -> Vec<(&str, &str, &str, &str)> {
        self.refused
            .iter()
            .filter_map(|v| match v {
                JdkOnlyViolation::SyntheticNativeRegistered {
                    class,
                    method,
                    descriptor,
                    survivor: Some(survivor),
                    ..
                } => Some((
                    class.as_str(),
                    method.as_str(),
                    descriptor.as_str(),
                    survivor.as_str(),
                )),
                _ => None,
            })
            .collect()
    }

    /// `"<kind>@<file>:<line>"` of the registration that currently owns this
    /// triple, or `None` when nothing does.
    ///
    /// Called only from the `JdkOnly` refusal arm, which is bounded by the
    /// number of refusals rather than by the ~3,100 registrations, so the
    /// `format!` is affordable there for the same reason the site string is.
    pub(in crate::registry) fn surviving_owner(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<String> {
        let class_state = self.class_prefilter(class_name)?;
        let idx = self.slot_index_from_state(class_state, class_name, method_name, descriptor)?;
        let slot = self.slots.get(idx as usize)?;
        let site = self
            .provenance
            .get(slot.reg_index as usize)
            .map(|p| format!("{}:{}", p.site.file(), p.site.line()))
            .unwrap_or_else(|| "<no provenance>".to_string());
        Some(format!("{}@{}", slot.kind.as_str(), site))
    }

    /// Enable real-JDK-mode dropping of synthetic natives whose hardcoded
    /// field-slot layout corrupts the real JDK object (see
    /// [`drop_real_layout_synthetic`](Self)). Call before the `register_*`
    /// population pass in real-JDK mode.
    pub fn set_drop_real_layout_synthetic(&mut self, drop: bool) {
        self.drop_real_layout_synthetic = drop;
    }

    /// Whether this registry is being populated for a REAL-JDK arm.
    ///
    /// Read this at a registration SITE when a cluster has no working
    /// real-bytecode fallback to drop to and so cannot be expressed as a rule
    /// in `register` — the synthetic `FileInputStream`/`FileOutputStream`
    /// `<init>` block in `native-io` is the case this exists for. A
    /// `#[cfg(feature = "synthetic-jdk")]` guard is NOT equivalent and must
    /// not be used for this: the Cargo feature decides what is COMPILED, the
    /// launcher flag decides which CLASS LIBRARY loads, and a feature-enabled
    /// binary run `--real-jdk` satisfies the cfg while facing real JDK
    /// classes.
    pub fn drops_real_layout_synthetic(&self) -> bool {
        self.drop_real_layout_synthetic
    }

    /// Set the category applied to all subsequent `register()` calls until
    /// changed again. Prefer [`with_category`](Self::with_category) for a
    /// scoped set/restore.
    ///
    /// Takes `impl Into<Option<NativeKind>>` so both halves of the
    /// save/restore idiom this codebase uses in 564 places —
    /// `set_category(NativeKind::Bridge)` and `set_category(__prev_cat)` —
    /// compile unchanged while the restore now also restores the *absence* of
    /// a choice. See [`Self::current_category`].

    /// The category currently applied to new registrations, or `None` if no
    /// scope covers them. Feed it straight back to [`Self::set_category`] to
    /// restore around a nested registrar.

    /// The kind a registration made right now would carry: the scope's, or the
    /// conservative `SyntheticStub` when none covers it.
    ///
    /// The ONE place that default lives. Its doc comment's promise — "anything
    /// an author forgets to tag stays visible to the audit and gateable, never
    /// silently trusted" — is only kept if something can see the forgetting,
    /// which is what `category_chosen_log` and the census's `kind_chosen` are
    /// for.

    /// [`ClassFacts`] for `class_name`, computed on first sight.
    #[inline]
    pub(in crate::registry) fn class_facts(&mut self, class_name: &str) -> ClassFacts {
        if let Some(f) = self.class_facts.get(class_name) {
            return *f;
        }
        let f = ClassFacts::of(class_name);
        self.class_facts.insert(class_name.into(), f);
        f
    }

    /// Declare that subsequent `register()` calls install **leaf** natives.
    ///
    /// # The contract a leaf callback must satisfy
    ///
    /// A leaf native may be invoked through `safe_native_call_leaf` — the
    /// funnel with its GC bookkeeping removed — so all four of these must hold
    /// for the registered body, on **every** path including its error returns:
    ///
    ///  1. **It cannot allocate on the Java heap.** No `alloc_object`,
    ///     `new_array`, `new_ref_array`, no string interning, no boxing.
    ///  2. **It cannot safepoint or block.** No `begin_blocking_region`, no
    ///     monitor acquisition, no lock on a structure another thread holds
    ///     across a safepoint, no `park`, no re-entry into Java.
    ///  3. **It cannot initiate or observe a collection.** No `maybe_gc_*`, and
    ///     no retained raw `ObjectRef` across anything that could move the heap
    ///     (which follows from 1 and 2).
    ///  4. **It cannot throw a JNI-pending exception.** Returning
    ///     `Err(MethodCallFailed)` is fine — the caller routes that exactly as
    ///     the funnel does — but nothing may go through
    ///     `jni::set_pending_exception`, because the leaf path does not drain
    ///     that slot.
    ///
    /// What is left is a field read, a field write, an atomic RMW, or pure
    /// arithmetic: the bodies for which the funnel's ~180-330 ns of pinning,
    /// STW probing, thread-state transitions and unwind bookkeeping is the
    /// entire cost of the call. Measured on `probes/NativeShapeProbe.java`;
    /// see `native-call-funnel-is-the-per-call-floor-RETIRED-20260805.md`.
    ///
    /// The claim rides on the **callback**, not on the triple: it is captured
    /// into the slot by `register()` alongside the category, so a later phase
    /// that re-registers the same triple with a different body demotes it back
    /// to the full funnel unless that phase opts in as well. That ordering
    /// property is why this is a scoped ambient flag and not a
    /// `mark_leaf(class, method, descriptor)` post-pass: a post-pass keyed by
    /// triple would keep claiming leafness for whatever body happened to win
    /// the slot last.
    ///
    /// Prefer [`with_leaf`](Self::with_leaf) for a scoped set/restore.
    pub fn set_leaf(&mut self, leaf: bool) {
        self.current_leaf = leaf;
    }

    /// The leaf claim currently applied to new registrations.
    pub fn current_leaf(&self) -> bool {
        self.current_leaf
    }

    /// Run `f` with the leaf claim set, restoring the previous value after.
    /// See [`set_leaf`](Self::set_leaf) for the contract `f`'s registrations
    /// are asserting.
    pub fn with_leaf(&mut self, leaf: bool, f: impl FnOnce(&mut Self)) {
        let prev = self.current_leaf;
        self.current_leaf = leaf;
        f(self);
        self.current_leaf = prev;
    }

    /// [`Self::register`], with the kind stated **at the registration site**
    /// instead of inherited from whatever `set_category` the enclosing
    /// registrar last ran.
    ///
    /// This is the migration target for
    /// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up.
    /// `register` takes four arguments, none of them a kind; the kind comes
    /// from a mutable field on the registry that some *ancestor* frame set. The
    /// consequence runs in both directions and is silent at the point of the
    /// mistake:
    ///
    /// * one `set_category(Bridge)` line at the head of
    ///   `register_collections_natives` tags **1,195 registrations**, not one
    ///   of which targets an `ACC_NATIVE` method;
    /// * and a permanent bridge that inherits `SyntheticStub` is *dropped* by
    ///   the arms below under `CRATONVM_NO_STUBS` / `JdkOnly`, which is the
    ///   2026-07-14 `java.util.Properties` regression that surfaced minutes
    ///   later as `InternalError: null property: java.home`.
    ///
    /// A registration made through this entry point records
    /// [`NativeCensusEntry::kind_stated`], so the census can finally separate
    /// *"someone adjudicated this"* from *"this inherited the default"* —
    /// which is the evidence the 157-entry reclassification was blocked on.
    ///
    /// `#[track_caller]` on both this and `register` so provenance still points
    /// at the registrar, not at this line.
    /// Open (`true`) or close a scope in which a stated [`NativeKind::Intrinsic`]
    /// is kept by `Compatible` even when its
    /// triple belongs to an adopted HotSpot-alignment family
    /// (`retired_shadow::compat_row_of_shadow`), whose drop otherwise refuses
    /// every registration of the triple whatever its kind. Under the family the
    /// JDK's body is what runs, exactly as under `JdkOnly`; an `Intrinsic`
    /// registered in the scope is a reviewed memo in front of that body, which
    /// `JdkOnly` already keeps (`resolve_dispatch` step 2), so the two modes then
    /// agree. Any other kind is dropped as before. Returns the previous value:
    /// restore it on exit. Interpreter round i5 wave 109, lane L4: the `Lookup`
    /// finder mint memos under `CRATONVM_MH_COMPAT_FINDER_MINT`.
    pub fn set_admitting_family_intrinsic(&mut self, on: bool) -> bool {
        std::mem::replace(&mut self.admitting_family_intrinsic, on)
    }

    /// Re-state the kind of the registration that currently answers this EXACT
    /// triple, keeping its callback: a review that classifies a registration
    /// after the fact, without knowing which of several last-write-wins sites
    /// installed it. Returns whether such a registration existed; a triple with
    /// no exact registration is left alone (no descriptor-quirk fallback, which
    /// would copy a neighbour's callback onto a new triple).
    #[track_caller]
    pub fn restate_kind(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        kind: NativeKind,
    ) -> bool {
        if self.kind_of(class_name, method_name, descriptor).is_none() {
            return false;
        }
        let Some(callback) = self
            .resolve_id(class_name, method_name, descriptor)
            .and_then(|id| self.callback_of(id))
        else {
            return false;
        };
        self.register(class_name, method_name, descriptor, callback, kind);
        true
    }

    /// The category a native was registered under, or `None` if no native is
    /// registered for this exact triple. O(1).
    #[inline]
    pub fn kind_of(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeKind> {
        self.slot_for_exact(class_name, method_name, descriptor)
            .map(|slot| slot.kind)
    }

    /// Every class a native for `(method_name, descriptor)` is registered on.
    ///
    /// The registry is keyed on the EXACT class name, and every lookup path
    /// asks it that way — `find`, `resolve_id`, the JIT site cache. This is the
    /// one question they cannot answer: *given a method, which receivers would
    /// take a native?* The JIT's `final`-method devirtualiser needs exactly
    /// that, because it decides at COMPILE time, with no receiver in hand, to
    /// bind a site straight to the classfile body — and a native registered on
    /// a SUBCLASS of the declaring class shadows that body for every receiver
    /// of the subclass. See `invoke::invokevirtual_site_final_owner`, whose
    /// screen this serves, and the netty
    /// `channeloutboundbuffer-close-ordering` page for the two defects that
    /// escaped through the gap.
    ///
    /// Cold path: called once per candidate call site while compiling, never
    /// per execution. The index behind it is built on first use and rebuilt if
    /// `registrations` has grown since (registration is an init-time activity,
    /// so in an ordinary run it is built exactly once, and the length check is
    /// what keeps a late `register()` — the tests do this — from being invisible
    /// to a cached answer).
    #[must_use]
    pub fn owner_classes_for_method(&self, method_name: &str, descriptor: &str) -> Vec<Box<str>> {
        type Index = std::collections::HashMap<(Box<str>, Box<str>), Vec<Box<str>>>;
        let mut guard = self
            .method_owner_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale = match &*guard {
            Some((built_len, _)) => *built_len != self.registrations.len(),
            None => true,
        };
        if stale {
            let mut index: Index = std::collections::HashMap::new();
            for (class, method, desc) in self.registrations.iter() {
                index
                    .entry((method.into(), desc.into()))
                    .or_default()
                    .push(class.into());
            }
            *guard = Some((self.registrations.len(), index));
        }
        let Some((_, index)) = &*guard else {
            return Vec::new();
        };
        index
            .get(&(Box::from(method_name), Box::from(descriptor)))
            .cloned()
            .unwrap_or_default()
    }

    /// Snapshot of every registration as `(class, method, descriptor, kind)`,
    /// for the `--dump-native-registry` census. Order follows registration
    /// order; callers sort for diff-stable output.
    #[inline]
    pub fn registration_count(&self) -> usize {
        self.registrations.len()
    }

    #[inline]
    pub fn registrations_since(
        &self,
        since: usize,
    ) -> impl Iterator<Item = (&str, &str, &str, NativeKind)> {
        self.registrations
            .iter()
            .skip(since)
            .zip(self.categories.iter().skip(since))
            .map(|((c, m, d), k)| (c, m, d, *k))
    }

    pub fn dump_registrations(&self) -> Vec<(&str, &str, &str, NativeKind)> {
        self.registrations
            .iter()
            .zip(self.categories.iter())
            .map(|((c, m, d), k)| (c, m, d, *k))
            .collect()
    }

    /// Schema-v2 census (`docs/book/src/user-guide/real-jdk-mode.md` §4): every
    /// accepted registration with its provenance and dispatch count. Order
    /// follows registration order; callers sort for diff-stable output.
    ///
    /// This is the slow, cold, allocating counterpart to
    /// [`dump_registrations`](Self::dump_registrations) — one `String` per
    /// field per row, ~3,100 rows. It runs at most once per VM, from a CLI dump
    /// or the CI gate. Nothing on any hot path calls it.
    ///
    /// **Superseded rows.** A triple registered twice yields two rows, in
    /// registration order, but only ONE slot (re-registration updates in
    /// place). The one-pass `reg_index -> slot` reverse map below decides which
    /// row owns the counter: the current owner reports the triple's full count,
    /// every superseded row reports `0`. Splitting the count between them is
    /// impossible — the counter never knew about the handover — and attributing
    /// it to both would double the gate's `synthetic_stub_invocations` total.
    pub fn census(&self) -> Vec<NativeCensusEntry> {
        // Reverse of `NativeSlot::reg_index`. One pass over `slots`
        // (<= registrations.len()), then O(1) per row — not a scan per row.
        let mut owner_slot: Vec<Option<u32>> = vec![None; self.registrations.len()];
        for (slot_idx, slot) in self.slots.iter().enumerate() {
            if let Some(cell) = owner_slot.get_mut(slot.reg_index as usize) {
                *cell = Some(slot_idx as u32);
            }
        }
        self.registrations
            .iter()
            .enumerate()
            .map(|(reg_index, (class, name, descriptor))| {
                let prov = self.provenance.get(reg_index);
                let owning_slot = owner_slot.get(reg_index).copied().flatten();
                let invocations = owning_slot
                    .and_then(|slot_idx| self.invocation_total(slot_idx as usize))
                    .unwrap_or(0);
                NativeCensusEntry {
                    class: class.to_string(),
                    name: name.to_string(),
                    descriptor: descriptor.to_string(),
                    // `categories` is index-parallel with `registrations`; the
                    // fallback is unreachable and picks the conservative kind
                    // (matching `current_category`'s default) so a hypothetical
                    // desync can only ever over-report stubs, never hide one.
                    kind: self
                        .categories
                        .get(reg_index)
                        .copied()
                        .unwrap_or(NativeKind::SyntheticStub),
                    // The one and only place a provenance string is formatted.
                    registered_by: prov.map(|p| format!("{}:{}", p.site.file(), p.site.line())),
                    overwrote: prov.and_then(|p| p.overwrote),
                    invocations,
                    // Read through the same `owner_slot` reverse index as the
                    // count itself, so the flag always describes the slot the
                    // number came from. A superseded row reports `0`
                    // invocations and therefore `true`: a floor of zero on a
                    // row that can never be dispatched is exact, and claiming
                    // it might be higher would invent a doubt.
                    invocations_complete: owning_slot
                        .and_then(|slot_idx| {
                            self.slot_invocations_incomplete.get(slot_idx as usize)
                        })
                        .map(|f| !f.load(std::sync::atomic::Ordering::Relaxed))
                        .unwrap_or(true),
                    kind_stated: true,
                    kind_chosen: true,
                    // The same `owner_slot` reverse index the invocation count
                    // above is read through — a superseded registration has no
                    // entry in it, which is precisely what "owns no slot" means.
                    owns_slot: owning_slot.is_some(),
                }
            })
            .collect()
    }

    /// Every registration in this registry that a LATER `register*` of the
    /// identical triple displaced — the losers of last-write-wins.
    ///
    /// See [`shadowed_registrations_in`] for what this is for and why it is the
    /// only mechanical way to find this defect species. Cold: it censuses.
    pub fn shadowed_registrations(&self) -> Vec<ShadowedRegistration> {
        shadowed_registrations_in(&self.census())
    }

    /// Permit the synthetic `java.net.Socket` / `ServerSocket` natives to be
    /// registered even under `CRATONVM_REAL_NET_SOCKETS`. See
    /// [`Self::allow_synthetic_net_sockets`]. Test-registry use only.
    pub fn allow_synthetic_net_sockets(&mut self, allow: bool) {
        self.allow_synthetic_net_sockets = allow;
    }

    /// Register a native method implementation.
    ///
    /// `#[track_caller]` (2026-07-31, Real-JDK §4): the ~40 `register_*`
    /// functions across `native-builtins` / `native-collections` / … are the
    /// only thing that knows where a stub is declared, and after boot that
    /// information was gone. The attribute is a caller-ABI change with no
    /// signature change — every call site keeps compiling — and costs the
    /// caller two words of `&'static Location` at the call, not a `format!`.
    /// The string is built only if [`census`](Self::census) or the Real-JDK
    /// refusal path actually needs it.
    ///
    /// # The one kind decision made here rather than at the site
    ///
    /// A `Bridge` whose receiver class no supported JDK image declares cannot
    /// bind to an `ACC_NATIVE` method — §1.5's whole definition — so it is
    /// re-tagged [`NativeKind::SyntheticStub`] before any of the policy arms
    /// below run, and reports `kind_stated`, because a measurement adjudicated
    /// it. See [`crate::no_image_receiver`] for the measurement, the six images
    /// it was taken against, and why the reviewed VM services are excluded.
    ///
    /// Ordering is load-bearing: the re-tag has to happen before the `JdkOnly`
    /// arm, or strict mode keeps admitting exactly the rows the re-tag exists to
    /// exclude.
    #[track_caller]
    pub fn register(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        callback: NativeCallback,
        kind: NativeKind,
    ) {
        // Which loader this native's by-name lookups use is decided by what it is
        // registered for; see `crate::caller_loader`.
        crate::caller_loader::note_registration(class_name, callback as usize);
        // The class-only predicates below, answered once per class (see
        // `ClassFacts`): same answers, read where they were always asked.
        let facts = self.class_facts(class_name);
        // `Compatible` over a real JDK: a `RETIRED_SHADOW_*` family that mode
        // has adopted as a HotSpot-alignment row is not registered at all when
        // its row is in the set -- by any registrar, whatever its kind -- so
        // the JDK's bodies run, as under `JdkOnly`. One index
        // (`retired_shadow::compat_row_of_shadow`; interpreter round i2 wave
        // 53, lane L5, the i52-L5 proposal's step 1) for what were one arm per
        // row:
        //
        // * `ParallelLoaders` (wave 48, lane L5): the two shadows that keep
        //   `ClassLoader$ParallelLoaders` empty (three registrars carry
        //   `registerAsParallelCapable`);
        // * `JdkUnmodifiableViews` (wave 51, lane L5; stage 4 of
        //   `docs/feature-designs/native-dispatch-architecture.md`):
        //   `Collections.unmodifiable*` and the immutable collections'
        //   serialization carrier (five registrars, some handing back the
        //   MUTABLE argument);
        // * `JdkUrlClassLoaders` (wave 52, lane L5; item 6 of
        //   `docs/feature-designs/native-dispatch-architecture.md`):
        //   the 26 `URLClassLoader` / `URLClassPath` stand-ins;
        // * `JdkHashCollections` (JIT round 16, hcb, a `COMPAT_FAMILIES`
        //   entry): `HashMap`, `LinkedHashMap`, `HashSet`, `LinkedHashSet` and
        //   their views;
        // * `JdkFileBodies` (wave 53, lane L5): `java.io.File` and the
        //   platform `FileSystem` wrappers;
        // * `JdkJarFileBodies` (wave 58, lane L5): `JarFile`, the `ZipFile`
        //   rows and the jar value classes (`is_jar_file_family_shadow`);
        // * `JdkStringBuilderBodies` / `JdkArraysBodies` (strings round,
        //   `str16`; `COMPAT_FAMILIES` entries): the two builder classes, and
        //   `java.util.Arrays`.
        if self.compatibility_mode == CompatibilityMode::Compatible && self.real_jdk {
            // `Compatible` over a real JDK: `ClassLoader`'s three instance resource
            // lookups and the fast-boot startup bridges are reviewed `Intrinsic`s in that
            // mode, whichever registrar makes them, so cold ancestor walks and warm
            // `resolve_dispatch` doors run the native callback. `JdkOnly` keeps the `Bridge`.
            if kind == NativeKind::Bridge
                && (crate::retired_shadow::is_compat_loader_resource_intrinsic(
                    class_name,
                    method_name,
                    descriptor,
                ) || crate::retired_shadow::is_compat_fast_boot_intrinsic(
                    class_name,
                    method_name,
                    descriptor,
                ))
            {
                self.register_inner(
                    class_name,
                    method_name,
                    descriptor,
                    callback,
                    NativeKind::Intrinsic,
                );
                return;
            }
            // Interpreter round i5 wave 109, lane L4: a stated `Intrinsic`
            // registered inside a `set_admitting_family_intrinsic` scope stays,
            // as it does under `JdkOnly` (a reviewed memo in front of the
            // JDK's own body).
            let family_intrinsic =
                self.admitting_family_intrinsic && kind == NativeKind::Intrinsic;
            if let Some(row) = crate::retired_shadow::compat_row_of_shadow_with(
                class_name,
                method_name,
                descriptor,
                facts.family,
                facts.retired_prefix,
            )
            .filter(|_| !family_intrinsic)
            {
                if self.hotspot_alignments.contains(row) && self.compat_row_prerequisites_hold(row)
                {
                    return;
                }
            }
            // JIT round 20 (nio20, lane L4): the NIO interrupt hook
            // `System$1.blockedOn` belongs to two families at once, so it has
            // its own arm (`retired_shadow::is_nio_blocker_hook`): dropped when
            // either family whose channels are the JDK's own is in force.
            if (self
                .hotspot_alignments
                .contains(cratonvm_types::HotSpotAlignment::JdkPipe)
                || self
                    .hotspot_alignments
                    .contains(cratonvm_types::HotSpotAlignment::JdkNioChannels))
                && crate::retired_shadow::is_nio_blocker_hook(class_name, method_name, descriptor)
            {
                return;
            }
            // The adopted family's companion stand-ins (SBR round 4, lane
            // compat; `retired_shadow::CompatFamily`): `--real-jdk`'s stub
            // refusal, scoped to the family's classes. Only a registration that
            // is a `SyntheticStub` here -- a `Bridge` or `Intrinsic` of the same
            // class stays, as it does under `--real-jdk`.
            if kind == NativeKind::SyntheticStub {
                if let Some(row) = facts.stub_row {
                    if self.hotspot_alignments.contains(row)
                        && self.compat_row_prerequisites_hold(row)
                    {
                        return;
                    }
                }
            }
            // JDK-driven virtual threads (`jdk_continuations`): `Thread`'s
            // stand-ins answer from the VM's view of a platform thread, and a
            // `java.lang.VirtualThread` is not one -- it has no `holder`, its
            // state and liveness are its own fields (`state`, `alive()`), its
            // `sleep` and `yield` unmount it (`VirtualThread.sleepNanos`,
            // `tryYield`), its `join` waits on its termination latch. The
            // JDK's bodies run instead, as under `--real-jdk` (where concrete
            // bytecode beats every such registration): only `Thread`'s own
            // `native` methods ([`JDK_THREAD_NATIVE_METHODS`]) and its
            // constructors (whose stand-ins carry the inheritable thread-local
            // capture) stay registered on `java/lang/Thread`.
            if self.jdk_continuations
                && class_name == "java/lang/Thread"
                && method_name != "<init>"
                && !JDK_THREAD_NATIVE_METHODS.contains(&method_name)
            {
                return;
            }
            // SBR round 5, lane compat2, row `JdkStreams`: under `--real-jdk`
            // concrete bytecode beats every `Bridge` on the stream pipeline and
            // on the methods that hand one out (`resolve_dispatch` step 3), so
            // the whole stream surface -- `java/util/stream/*` and every
            // method answering a `Stream` / `IntStream` / `LongStream` /
            // `DoubleStream` -- is the JDK's own `ReferencePipeline`, split over
            // the common `ForkJoinPool` when parallel. `--compatible` lets the
            // natives win, so a stream is the VM's synthetic carrier and a
            // parallel one runs on the caller. With the row on, no such
            // registration is made: the carrier is never minted and the JDK's
            // bodies are the only ones left (an `Intrinsic` stays, as it does
            // under `--real-jdk`).
            if kind != NativeKind::Intrinsic
                && self.hotspot_alignments.contains(cratonvm_types::HotSpotAlignment::JdkStreams)
                && crate::retired_shadow::is_stream_surface(class_name, method_name, descriptor)
                // A carrier class with no JDK counterpart has no bytecode to
                // yield to (`MemorySegmentImpl.elements`): it keeps its native,
                // as under `--real-jdk`.
                && !class_name.starts_with("cratonvm/")
                && !facts.no_image
            {
                return;
            }
        }
        // `JdkOnly` over a real JDK: a `Bridge` over a method the JDK 25 image
        // gives `Code` that no strict door can admit (concrete bytecode beats
        // a `Bridge`, `resolve_dispatch` step 3) and whose receivers there are
        // JDK-built, reviewed per triple in
        // `retired_shadow::is_jdk_only_yielded_bridge` (JIT round 20 wave 17,
        // lane bridge: the bridge ratchet's strict stated-shadow row). Not
        // registered, so the strict registry does not claim a `Bridge` it
        // never dispatches; `Compatible` is untouched.
        if self.compatibility_mode == CompatibilityMode::JdkOnly
            && self.real_jdk
            && kind == NativeKind::Bridge
            && crate::retired_shadow::is_jdk_only_yielded_bridge(
                class_name,
                method_name,
                descriptor,
            )
        {
            if None::<std::ffi::OsString>.is_some() {
                eprintln!("[JDK-ONLY-YIELDED-BRIDGE] {class_name}.{method_name}{descriptor}");            }
            return;
        }
        // `CRATONVM_COMPATIBLE_JDK_REIFIER` (see the field): the JDK's own
        // generic-reflection bodies run, so their natives are not registered
        // by any registrar (three register the `Class` rows); nor are the
        // natives over the JDK's own type classes
        // (`sun.reflect.generics.reflectiveObjects.*`, the `RETIRED_SHADOW`
        // REFLECT family `JdkOnly` drops), whose eager bound reification
        // resolved a missing bound loader-blind where the JDK's lazy one
        // raises `TypeNotPresentException`.
        if self.compatible_jdk_reifier()
            && (class_name.starts_with("sun/reflect/generics/reflectiveObjects/")
                || GENERIC_REIFIER_NATIVES
                    .iter()
                    .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor))
        {
            return;
        }
        // `CRATONVM_JDK_TYPE_ANNOTATIONS` (see the field): the JDK's
        // `TypeAnnotationParser` builds every `AnnotatedType`; the natives that
        // build or read the VM's own `AnnotatedType` shapes go together (one
        // left would hand the JDK's accessors an `Annotation[]` where they
        // read a `Map`). Either mode: `JdkOnly` keeps the reviewed
        // `getAnnotatedReturnType` `Intrinsic` otherwise.
        if self.jdk_type_annotations()
            && TYPE_ANNOTATION_NATIVES
                .iter()
                .any(|&(c, m, d)| c == class_name && m == method_name && d == descriptor)
        {
            return;
        }
        // `CRATONVM_JDK_DECLARATION_ANNOTATIONS` (see the field): the JDK's
        // `AnnotationParser` answers every declaration annotation of `Class`
        // and the members; every registrar's natives for those getters go
        // (interpreter round i2 wave 58, lane L3). In fast-boot mode under
        // `Compatible`, `getParameterAnnotations` stays native for rapid scanning.
        if self.jdk_declaration_annotations()
            && is_declaration_annotation_native(class_name, method_name)
            && method_name != "getParameterAnnotations"
        {
            return;
        }
        // `CRATONVM_JDK_REFLECTIVE_INVOKE` (see the field): the JDK's
        // `Method.invoke` / `Constructor.newInstance` bodies run; every
        // registrar's native for the two goes (interpreter round i2 wave 59,
        // lane L3). The accessors' genuine natives (`invoke0`, `newInstance0`)
        // stay.
        if self.jdk_reflective_invoke()
            && ((class_name == "java/lang/reflect/Method"
                && method_name == "invoke"
                && descriptor == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;")
                || (class_name == "java/lang/reflect/Constructor"
                    && method_name == "newInstance"
                    && descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;"))
        {
            return;
        }
        // `CRATONVM_BAIS_JDK_BODIES` over a real JDK, either mode (see the
        // field): the JDK's `ByteArrayInputStream` bodies run. Except
        // `close()V` (interpreter round i1 wave 52, lane L5): the JDK's body is
        // empty, and `native-io`'s is the same no-op plus the
        // `BaisEvent::Close` observation by which closing an `https` response
        // stream recycles its connection (`http_url_connection.rs`
        // `huc_live_bais_event`; `RSslLiveSession`'s `drainTrap`, which failed
        // 4 rows with `close` dropped too). MEASURED, HotSpot 25.0.3: CLOSING
        // the drained body is the recycle instant -- the same vector with
        // `in.close()` removed leaves every connection accessor answering on
        // HotSpot -- so the JDK's `read` bodies, which dispatch no
        // `BaisEvent::Eof`, are HotSpot's behaviour there, and `close` is the
        // one observation that must stay.
        if self.bais_jdk_bodies
            && self.real_jdk
            && class_name == "java/io/ByteArrayInputStream"
            && method_name != "<init>"
            && !(method_name == "close" && descriptor == "()V")
        {
            return;
        }
        // `CRATONVM_JDK_INET_SOCKET_ADDRESS_BODIES` (see the field): under
        // `JdkOnly` over a real JDK the JDK's `InetSocketAddress` bodies run,
        // every registrar's copy and every kind (they are `Intrinsic`s, so the
        // retirement tables, which re-tag `Bridge`s only, cannot carry them).
        // `SocketAddress.toString()` goes too, in EITHER mode: the JDK class
        // declares no `toString`, so the registration answered for every
        // subclass that does not override it (`"SocketAddress"`, or
        // `host:port` read out of whatever three slots it has), where HotSpot
        // runs `Object.toString`. `--compatible`'s `InetSocketAddress` keeps
        // its own `toString` registration, so nothing it minted needs this one.
        if self.jdk_inet_socket_address_bodies
            && self.real_jdk
            && ((self.compatibility_mode == CompatibilityMode::JdkOnly
                && class_name == "java/net/InetSocketAddress")
                || (class_name == "java/net/SocketAddress"
                    && method_name == "toString"
                    && descriptor == "()Ljava/lang/String;"))
        {
            return;
        }
        // `CRATONVM_STRICT_FAMILY_BRIDGES` (see the field; default ON since i7 wave 144):
        // under `JdkOnly` over a real JDK, a family's reviewed stand-in over JDK
        // `Code` is not registered, as `--compatible` does not register it with
        // the family row. An `Intrinsic` stays (it may win, step 2).
        if self.strict_family_bridges
            && self.compatibility_mode == CompatibilityMode::JdkOnly
            && self.real_jdk
            && kind != NativeKind::Intrinsic
            && crate::retired_shadow::is_compat_family_bridge_with(
                class_name,
                method_name,
                descriptor,
                facts.family,
            )
        {
            return;
        }
        // Two measured, centrally-applied kind decisions, in one arm because
        // they have the same shape and the same reason: a fact about the JDK
        // image that no registration site can know, adjudicated once.
        //
        //  * the receiver class no supported image declares (§1.5 has no target
        //    to bind to), and
        //  * a triple RETIRED as a §1.4 shadow, subsystem by subsystem, each
        //    against a strict-corpus measurement. See `crate::retired_shadow`.
        if kind == NativeKind::Bridge {
            // …unless this image still declares the triple `native`: the
            // retirement was measured where it has a body, and here there is
            // no bytecode to yield to.
            let retired = crate::retired_shadow::triple_is_retired_shadow_given_prefix(
                || facts.retired_prefix,
                class_name,
                method_name,
                descriptor,
            ) && !crate::retired_shadow::retired_triple_is_native_before(
                class_name,
                method_name,
                descriptor,
            )
            .is_some_and(|first_body| self.real_jdk_older_than(first_body));
            if retired || facts.no_image {
                self.retagging_retired_shadow = retired;
                self.register_inner(class_name, method_name, descriptor, callback, NativeKind::SyntheticStub);
                self.retagging_retired_shadow = false;
                return;
            }
        }
        if kind == NativeKind::Intrinsic
            && crate::unretire::audit_yields_intrinsic(class_name, method_name)
        {
            // Diagnostic only (`CRATONVM_AUDIT_YIELD_INTRINSIC`): demote this
            // reviewed Intrinsic to a dropped stub so the real bytecode runs.
            self.register_inner(class_name, method_name, descriptor, callback, NativeKind::SyntheticStub);
            return;
        }
        self.register_inner(class_name, method_name, descriptor, callback, kind);
    }

    /// [`register`](Self::register)'s body. Split out only so the re-tag above
    /// can set/restore around it: this function has a dozen early returns, and
    /// restoring at each of them is the shape that eventually misses one.
    #[track_caller]
    pub(in crate::registry) fn register_inner(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        callback: NativeCallback,
        kind: NativeKind,
    ) {
        // Real-JDK strict policy (docs/book/src/user-guide/real-jdk-mode.md §4,
        // wave 1, 2026-07-31). Deliberately placed BEFORE the
        // `drop_synthetic_stubs` arm below rather than folded into it: the two
        // answer different questions and must stay separable. `CRATONVM_NO_STUBS`
        // is an operator's blunt "make the whole SyntheticStub bucket vanish"
        // switch and is silent by design; `JdkOnly` is a *policy* that has to
        // produce attributed, structured evidence (§10 is "measurement, not
        // deletion"). Merging them would mean either the env var suddenly starts
        // allocating a violation per drop, or JdkOnly loses its provenance —
        // and the arm below is load-bearing for a documented real-JDK bootstrap
        // regression (java.home), so it is left byte-for-byte alone.
        //
        // Compatible mode pays exactly one field load plus a discriminant
        // compare here, and the `allowed_in` call is short-circuited away.
        if self.compatibility_mode == CompatibilityMode::JdkOnly
            && !kind.allowed_in(CompatibilityMode::JdkOnly)
        {
            // Reuse the existing `CRATONVM_DBG_DROPPED_STUBS` switch (added
            // 2026-07-14 for the mis-tagged-category bisection) with a distinct
            // tag, so an operator debugging "why did this native disappear" sets
            // ONE env var and sees both mechanisms. The tags differ because the
            // remedies differ: `[DROPPED-STUB]` means "unset CRATONVM_NO_STUBS",
            // `[JDK-ONLY-REFUSED]` means "this stub is a real Real-JDK blocker,
            // go implement or re-tag it".
            if None::<std::ffi::OsString>.is_some() {
                eprintln!("[JDK-ONLY-REFUSED] {class_name}.{method_name}{descriptor}");            }
            // This is the one place a provenance string is built eagerly, and it
            // is bounded by the number of refusals (which the gate drives to
            // zero), not by the ~3,100 registrations.
            let site = core::panic::Location::caller();
            // The refusal is only a RETIREMENT when nothing already owns the
            // triple. See `JdkOnlyViolation::SyntheticNativeRegistered`'s
            // `survivor` doc: an earlier registration survives the refusal and
            // keeps serving, so strict mode runs that older native rather than
            // the real bytecode the policy was asking for.
            let survivor = self.surviving_owner(class_name, method_name, descriptor);
            self.refused
                .push(JdkOnlyViolation::SyntheticNativeRegistered {
                    class: class_name.to_string(),
                    method: method_name.to_string(),
                    descriptor: descriptor.to_string(),
                    registered_by: Some(format!("{}:{}", site.file(), site.line())),
                    survivor,
                });
            // Return WITHOUT inserting: nothing is pushed to `registrations` /
            // `categories` / `provenance` / `slots`, so the refused triple never
            // appears in the census and `generation()` does not move.
            return;
        }
        // Strict no-stubs mode: drop synthetic-stub registrations entirely so
        // the call falls through to real bytecode or a clear error instead of a
        // fake. Bridges and intrinsics are always registered. (See the
        // `drop_synthetic_stubs` field doc.)
        if self.drop_synthetic_stubs && kind == NativeKind::SyntheticStub {
            // CRATONVM_DBG_DROPPED_STUBS=1: list every registration this mode
            // silently drops. Added 2026-07-14 while chasing a real-JDK-mode
            // bootstrap regression (`InternalError: null property: java.home`)
            // that traced back to a whole register_* function's worth of
            // permanent bridges (java.util.Properties' side-table natives)
            // being mis-tagged SyntheticStub by inheriting the wrong ambient
            // category at one of its call sites — this made the drop visible
            // in seconds instead of a multi-round bisection. Cheap/no-op when
            // unset; kept as a permanent diagnostic for the next occurrence.
            if None::<std::ffi::OsString>.is_some() {
                eprintln!("[DROPPED-STUB] {class_name}.{method_name}{descriptor}");            }
            return;
        }
        // NIO-SERVER-SOCKET (route 1): when `CRATONVM_REAL_NET_SOCKETS` is set,
        // drop EVERY synthetic native registered on `java/net/Socket` /
        // `java/net/ServerSocket` so the real JDK bytecode runs and drives the
        // real `sun/nio/ch/Net` path (native-io::net). A single registered
        // native shadows the class's bytecode at every interpreter dispatch
        // site (WP0.1 native-override-priority), and the synthetic surface is
        // registered from ~6 different functions (phases_early phase53,
        // phases_late p72, net_phase_e re1/re2, socket_channel, …) — filtering
        // here catches them all in one place. See `reference_server_socket_gap`.
        if real_net_sockets_enabled()
            && !self.allow_synthetic_net_sockets
            && (class_name == "java/net/Socket"
                || class_name == "java/net/ServerSocket"
                // DoHead third root cause (2026-07-13): the WildFly bootstrap
                // batch (be6055605) added synthetic `javax/net/SocketFactory`
                // getDefault/createSocket natives (phases_early.rs phase52)
                // that hand out a natively-built `java/net/Socket`. Under
                // CRATONVM_REAL_NET_SOCKETS every java/net/Socket native is
                // dropped (above), so REAL Socket bytecode consumes that
                // object and reads its uninitialized/clobbered real fields:
                // NPE `"socketLock" is null`, bogus `SocketException: Socket
                // is closed` (the port int lands on `state` and can satisfy
                // the CLOSED bit), `NoSuchMethodError:
                // java/lang/String.setOption` (the host String lands on
                // `impl`) — the Tomcat `TestHttpServletDoHead*`
                // testDoHeadHttp2 144/144 cluster
                // (`Http2TestBase.openClientConnection` →
                // `Socket.setSoTimeout`). Drop the factory natives too so
                // real `SocketFactory`/`DefaultSocketFactory` bytecode
                // constructs sockets through the real `Socket` constructors.
                // NOT `javax/net/ServerSocketFactory` — its
                // createServerSocket natives delegate to real constructors
                // and are layout-correct. (Root-cause analysis shared with
                // the concurrent dohead-third-cause session; landed here to
                // complete the DoHead family fix.)
                || class_name == "javax/net/SocketFactory"
                // A legacy SSLSocketFactory stub returns a two-slot Socket.
                // Keep P68's later Bridge registrations: they perform TLS and
                // produce a layout-correct SSLSocket. A `Bridge` re-tagged as a
                // retired shadow (SBR wave 5 lane JSSE: `getDefault`, the
                // no-arg `createSocket`) is one of those, not a legacy stub:
                // keep it too, so `--compatible` still dispatches it and only
                // `--real-jdk`'s stub refusal (above) drops it -- as the two
                // ForkJoin arms below do.
                || (class_name == "javax/net/ssl/SSLSocketFactory"
                    && kind == NativeKind::SyntheticStub
                    && !self.retagging_retired_shadow))
        {
            return;
        }
        // REAL-FORKJOINPOOL (opt-in): drop synthetic ForkJoinPool natives so the
        // real JDK pool bytecode runs (real init + workers, real
        // invokeAll/submit). See `real_forkjoinpool_enabled`.
        //
        // EXCEPTION — keep the synthetic eager-inline `execute`: under the real
        // pool, work submitted via `execute()` runs on a worker thread, and
        // CratonVM's cross-worker memory ordering doesn't reliably publish an
        // object-reference field (e.g. `CompletableFuture.result`) written by
        // one worker to a dependent task on another worker — so
        // `CompletableFuture.*Async` (which schedules every stage via
        // `execute`) reads a stale-null upstream result. Keeping `execute`
        // eager-inline (run on the caller) makes those stages run caller-side,
        // avoiding the cross-worker read, so CF keeps working WHILE the real
        // pool services Weld's `invokeAll`. `ForkJoinWorkerThread` natives are
        // also kept (the real pool needs them).
        //
        // Keep a small Bridge-only pool surface that real-JDK mode cannot
        // safely execute as bytecode under GC stress. These entries are not
        // synthetic layout shims; they are VM policy bridges registered by the
        // real-JDK native path and explicitly forced by the interpreter.
        //
        // `retagging_retired_shadow`: a `Bridge` that `register` has just
        // re-tagged as a retired §1.4 shadow (SBR wave 4 lane FJP) is KEPT
        // here, as the `SyntheticStub` it now is — so `--compatible` still
        // dispatches it and `--real-jdk` drops it as a stub. Without this the
        // re-tag would delete the native in `--compatible` too.
        let keep_real_forkjoinpool_bridge =
            (kind == NativeKind::Bridge || self.retagging_retired_shadow)
                && class_name == "java/util/concurrent/ForkJoinPool"
                && matches!(
                (method_name, descriptor),
                ("commonPool", "()Ljava/util/concurrent/ForkJoinPool;")
                    | (
                        "getFactory",
                        "()Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;",
                    )
                    | ("getParallelism", "()I")
                    | ("getCommonPoolParallelism", "()I")
                    | ("invoke", "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;")
                    | (
                        "submit",
                        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
                    )
                    | (
                        "externalSubmit",
                        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
                    )
                    // submit(Callable)/submit(Runnable)/submit(Runnable, T): left off
                    // the original allow-list, so real bytecode ran them against a pool
                    // whose commonPool() shortcut never populates queues/runState/mode —
                    // RejectedExecutionException at submissionQueue() (RealFjp.java).
                    | (
                        "submit",
                        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/ForkJoinTask;",
                    )
                    | (
                        "submit",
                        "(Ljava/lang/Runnable;)Ljava/util/concurrent/ForkJoinTask;",
                    )
                    | (
                        "submit",
                        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/ForkJoinTask;",
                    )
                    // `awaitQuiescence` was in the interpreter's
                    // `is_forkjoin_native_override` list but NOT here, so the
                    // registration was dropped and the interpreter's "force the
                    // native" had no native to force: the call fell through to
                    // real bytecode and answered "quiescent" immediately while
                    // an `execute(Runnable)` daemon thread was still running.
                    // A `--dump-native-registry` census (no awaitQuiescence
                    // row) plus a probe observing `execute(slow);
                    // awaitQuiescence()` with ran=0 where HotSpot reports
                    // ran=1 is what surfaced it. The two lists must agree
                    // entry-for-entry — a name present in only one of them is
                    // silently inert.
                    | ("awaitQuiescence", "(JLjava/util/concurrent/TimeUnit;)Z")
                    // 2026-09-11, lane 5 residual §9.1 — and it is the very
                    // failure the paragraph above describes, sitting in this
                    // file the whole time. `is_forkjoin_native_override`
                    // ALREADY lists `execute` under BOTH descriptors; this
                    // list had neither and nothing registered either, so the
                    // interpreter was told to force a native that did not
                    // exist and the call fell through to the concrete JDK
                    // bytecode. `pool.execute(task)` then put the task in a
                    // real `WorkQueue` for a real worker to run through the
                    // JDK's own `doExec()`, which never touches the side
                    // table — so the caller's `join()`/`get()`, which reads
                    // it, saw `done == false` and ran the body a SECOND time,
                    // on a second thread. `L5FjDouble.java`
                    // measures it per shape.
                    //
                    // Only the `ForkJoinTask` descriptor is added.
                    // `execute(Runnable)V` stays off this list ON PURPOSE and
                    // its override entry stays inert: a `Runnable` hands the
                    // caller no task to join, so there is nothing to double,
                    // and real bytecode puts it on a real worker thread —
                    // which is CLOSER to HotSpot than this pool's
                    // borrow-the-caller model, not further. Retiring that
                    // asymmetry means moving the whole pool off the side
                    // table, which is the next wave's item and not this one's.
                    | ("execute", "(Ljava/util/concurrent/ForkJoinTask;)V")
                    // BULK SUBMISSION — see the matching block in
                    // `is_forkjoin_native_override`. `invokeAll(Collection)` is
                    // the overload Weld's `ConcurrentBeanDeployer` calls and was
                    // on neither list, which failed the entire hibernate
                    // `org.hibernate.orm.test.cdi.*` cluster. `invokeAny` was
                    // uncovered too and failed SILENTLY (ran the callables,
                    // returned null); `lazySubmit` threw like invokeAll.
                    | ("invokeAll", "(Ljava/util/Collection;)Ljava/util/List;")
                    | (
                        "invokeAll",
                        "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/util/List;",
                    )
                    | (
                        "invokeAllUninterruptibly",
                        "(Ljava/util/Collection;)Ljava/util/List;",
                    )
                    | ("invokeAny", "(Ljava/util/Collection;)Ljava/lang/Object;")
                    | (
                        "invokeAny",
                        "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
                    )
                    | (
                        "lazySubmit",
                        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
                    )
            );
        if real_forkjoinpool_enabled()
            && class_name == "java/util/concurrent/ForkJoinPool"
            && method_name != "execute"
            && !keep_real_forkjoinpool_bridge
        {
            return;
        }
        // REAL-FORKJOINPOOL (opt-in): drop ordinary synthetic FJP task-family
        // natives so real JDK task bytecode can remain coherent with real pool
        // bootstrap state. The exception is the Bridge subset below: those
        // fork/join/get/result helpers are a deliberate VM policy surface used
        // by the real-FJP GC-stress lane. They share one side-table with the
        // pool Bridge methods above, and that side-table is scanned/remapped by
        // GC; letting `ForkJoinTask.fork()` fall through to bytecode reaches the
        // real WorkQueue/CAS path and reopens the residual timeout/corruption
        // face.
        // `retagging_retired_shadow`: see `keep_real_forkjoinpool_bridge`.
        let keep_real_forkjointask_bridge = (kind == NativeKind::Bridge
            || self.retagging_retired_shadow)
            && matches!(
                class_name,
                "java/util/concurrent/ForkJoinTask"
                    | "java/util/concurrent/RecursiveTask"
                    | "java/util/concurrent/RecursiveAction"
            )
            && matches!(
                (method_name, descriptor),
                ("fork", "()Ljava/util/concurrent/ForkJoinTask;")
                    | ("join", "()Ljava/lang/Object;")
                    | ("invoke", "()Ljava/lang/Object;")
                    | ("get", "()Ljava/lang/Object;")
                    | (
                        "get",
                        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;"
                    )
                    | ("getRawResult", "()Ljava/lang/Object;")
                    | ("setRawResult", "(Ljava/lang/Object;)V")
                    | ("isDone", "()Z")
                    | ("isCompletedNormally", "()Z")
                    // `(status & ABNORMAL) != 0` over the real `status` field,
                    // which no Bridge here ever writes — completions live in
                    // the `fjp_state` side table. Real bytecode therefore
                    // answered `false` for a task that had just raised
                    // ExecutionException (RJdkForkJoin.java:259). Must stay in
                    // step with `is_forkjoin_native_override`.
                    | ("isCompletedAbnormally", "()Z")
                    | ("isCancelled", "()Z")
                    | ("cancel", "(Z)Z")
                    | ("complete", "(Ljava/lang/Object;)V")
                    // Reads the throwable the side table records for an
                    // abnormally completed task, so it cannot disagree with
                    // join()/get() about whether the task failed.
                    | ("getException", "()Ljava/lang/Throwable;")
                    // W6-9: the WRITER for that record. Unregistered, real
                    // bytecode CASed the real `aux`/`status`, which nothing
                    // here reads — the task stayed `done == false` and the next
                    // `join()` ran its body and returned a value. Must stay in
                    // step with `is_forkjoin_native_override`.
                    | ("completeExceptionally", "(Ljava/lang/Throwable;)V")
                    // W6-9 §7.2: the only method on the class that CLEARS a
                    // status bit. Unregistered, real bytecode reset the real
                    // `status`/`aux`, which nothing here reads — the side-table
                    // completion survived and the next `join()` replayed the
                    // stale result. Must stay in step with
                    // `is_forkjoin_native_override`.
                    | ("reinitialize", "()V")
                    // L12: the STATIC `invokeAll` overloads. JDK 25's
                    // `invokeAll(t1, t2)` runs one task inline and then blocks
                    // in `awaitDone` for the FORKED sibling — which the lazy
                    // `fork()` above never schedules and no worker thread
                    // exists to run. RJdkForkJoin hung there forever.
                    | (
                        "invokeAll",
                        "(Ljava/util/concurrent/ForkJoinTask;Ljava/util/concurrent/ForkJoinTask;)V",
                    )
                    | ("invokeAll", "([Ljava/util/concurrent/ForkJoinTask;)V")
                    | ("invokeAll", "(Ljava/util/Collection;)Ljava/util/Collection;")
                    // W6-7: the `quietly*` family. `quietlyJoin()` is
                    // `if (status >= 0) awaitDone(false, 0L);` and
                    // `quietlyInvoke()` is `doExec(); ... awaitDone(...)` over
                    // the real `status` field, which no Bridge here ever writes
                    // — completions live in the `fjp_state` side table. With no
                    // worker threads that is a HANG, not a null. Must stay in
                    // step with `is_forkjoin_native_override`, entry for entry.
                    | ("quietlyJoin", "()V")
                    | ("quietlyInvoke", "()V")
                    | ("quietlyComplete", "()V")
                    | ("quietlyJoin", "(JLjava/util/concurrent/TimeUnit;)Z")
                    | ("quietlyJoinUninterruptibly", "(JLjava/util/concurrent/TimeUnit;)Z")
                    | ("quietlyJoinPoolInvokeAllTask", "(J)V")
                    // The two STATIC accessors that answer "which pool am I
                    // in". Registered by `phases_early.rs`, backed by
                    // `FJP_POOL_STACK`; unregistered they run real bytecode
                    // that asks `Thread.currentThread() instanceof
                    // ForkJoinWorkerThread`, which is false on this VM even
                    // inside `pool.invoke` because the pool runs its tasks
                    // INLINE. Adding a registration without adding it to THIS
                    // list is silent: the filter below drops any Bridge on
                    // these three classes that the list does not name, so the
                    // native is registered and then thrown away. Measured that
                    // way first. Must stay in step with
                    // `is_forkjoin_native_override`.
                    | ("inForkJoinPool", "()Z")
                    | ("getPool", "()Ljava/util/concurrent/ForkJoinPool;")
            );
        if real_forkjoinpool_enabled()
            && matches!(
                class_name,
                "java/util/concurrent/ForkJoinTask"
                    | "java/util/concurrent/RecursiveTask"
                    | "java/util/concurrent/RecursiveAction"
                    | "java/util/concurrent/CountedCompleter"
            )
            && !keep_real_forkjointask_bridge
        {
            return;
        }
        // Real-JDK mode: drop synthetic `java/io/StringReader` natives. The
        // fake surface uses the historical content/pos/length slot layout, but
        // JDK 25 StringReader wraps a final `Reader r`; the fake constructor
        // leaves that delegate null and real `mark()`/`read()` immediately NPE.
        if self.drop_real_layout_synthetic && class_name == "java/io/StringReader" {
            return;
        }
        // Real-JDK mode: drop synthetic `java/io/InputStreamReader` natives —
        // the same defect as `StringReader` above, one class over, found
        // 2026-09-17 when it took out the whole hibernate-reactive suite
        // (188/188 classes crashed before a single test ran).
        //
        // The synthetic surface parks the wrapped `InputStream` in slot 0 and
        // reads it back from there. On a real JDK 25 `InputStreamReader` slot 0
        // is `Reader.lock` — so the `<init>` shim clobbers the monitor object
        // and, decisively, never creates the real `sd` (`sun.nio.cs.StreamDecoder`)
        // that the genuine bytecode builds via `StreamDecoder.forInputStreamReader`.
        // The shim registers `read()I` but NOT `read([CII)I`, so the bulk read —
        // which is the one `BufferedReader.fill` calls, i.e. every `readLine()`
        // in the image, including `ServiceLoader`'s own provider-file parse —
        // keeps running real bytecode and dereferences that null `sd`:
        // `NullPointerException: Cannot invoke "sun.nio.cs.StreamDecoder.read(char[], int, int)"
        // because "this.sd" is null`.
        //
        // The registrar in `native-builtins/src/servlet.rs` now carries the
        // site-level `drops_real_layout_synthetic()` guard its `native-io` twin
        // grew on 2026-08-12 (W7-50). This entry is the mechanism-level half:
        // the cfg is a COMPILE-time question and this is the LOAD-time one, so
        // a future registrar that repeats the mistake is refused here rather
        // than shipping the same outage again.
        if self.drop_real_layout_synthetic && class_name == "java/io/InputStreamReader" {
            return;
        }
        // Real-JDK mode: drop synthetic `java/io/PipedInputStream`/
        // `java/io/PipedOutputStream` natives. These were written for a
        // legacy hardcoded 4-slot BufferedInputStream/BufferedOutputStream-
        // style layout (in/buf/pos/count) and reused verbatim for the piped
        // streams, including writing/reading those slot indices directly on
        // whatever object is passed in. On a real JDK 25 PipedOutputStream
        // (single field: `sink`, a connected PipedInputStream), the "out"
        // slot these natives resolve/hardcode to index 0 lands on `sink`
        // instead of a delegate OutputStream, so flush()/write()/close()
        // then invoke_virtual "write"/"flush" on the connected
        // PipedInputStream itself -- which declares neither -- producing a
        // NoSuchMethodError naming PipedInputStream for a completely
        // unrelated method. See
        // bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md
        // (H2's TestLob/TestLobApi/TestSQLXML/TestUpdatableResultSet/
        // TestResultSet, which all use real connected Piped stream pairs).
        // Real JDK PipedInputStream/PipedOutputStream bytecode is
        // self-contained (synchronized circular buffer, wait/notifyAll,
        // Thread identity checks -- no missing native dependency), so drop
        // the synthetic surface and let it run, same as StringReader/
        // EnumSet/Pattern/Matcher above.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/io/PipedInputStream" | "java/io/PipedOutputStream"
            )
        {
            return;
        }
        // Real-JDK mode: drop every `java/util/EnumSet` native, including the
        // SyntheticStub-tagged fallback surface. The real JDK factories are
        // self-contained once `Class.getEnumConstantsShared` works, and they
        // allocate concrete RegularEnumSet/JumboEnumSet receivers. Keeping even
        // the fallback `noneOf`/`of` natives in real mode manufactures an
        // abstract EnumSet object with the synthetic two-field layout; later
        // virtual calls then either skip the synthetic methods or read the wrong
        // layout, producing `size() == 0` and `iterator() == null` for non-JDK
        // enums such as Log4j's StandardLevel and Jakarta DispatcherType.
        if self.drop_real_layout_synthetic && class_name == "java/util/EnumSet" {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/security/Permissions` +
        // `java/security/PermissionCollection` natives (`add`, `setReadOnly`,
        // `isReadOnly`). The fake `add` stores the permission into the single
        // `allPermission` field slot instead of the real JDK
        // `permsMap` + per-class `PermissionCollection` structure. On a real
        // JDK `Permissions` object that leaves `permsMap` empty, so `implies`
        // limps via the `allPermission` fallback (true only for the
        // last-added permission's own class) while `elements()`/`size()`
        // iterate the never-populated `permsMap` and return EMPTY. WildFly's
        // Elytron builds a permission-set verifier by COPYING permissions via
        // `Permissions.elements()`
        // (`PermissionMapperDefinitions.createPermissions`): the copy yields
        // nothing, the `$local` identity never receives `LoginPermission`, and
        // JBOSS-LOCAL-USER management authentication is rejected
        // (`ServerRejected` — the entire WildFly integration suite blocker).
        // The real JDK `Permissions`/`PermissionCollection` bytecode is
        // self-contained and correct, so drop the synthetic surface and let it
        // run. The synthetic permissive collection built by
        // `security_manager::build_permissive_collection` seeds its slots
        // directly (not via native `add`) and does not depend on these natives.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/security/Permissions" | "java/security/PermissionCollection"
            )
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/io/StringWriter` surface.
        // It models the writer as `char[] buf` (slot 0) + `int count` (slot 1).
        // The real JDK layout is the inherited `Writer.lock` (slot 0, Object)
        // and `StringWriter.buf` (slot 1, StringBuffer), which the JDK holds
        // EQUAL to each other. The synthetic state squats two real
        // reference-typed fields, so a SUBCLASS reading the inherited
        // `this.lock` saw an `int[]` where the JDK guarantees the StringBuffer
        // — the assertion `regression-suite`'s `RSerial` fails on. A moving GC
        // tracing a `char[]` parked in that mislabelled slot is the same hazard.
        //
        // The registration already sits behind `#[cfg(feature = "synthetic-jdk")]`
        // with the comment "let the real bytecode run by default", but a Cargo
        // feature only decides what is COMPILED: a feature-enabled binary RUN
        // in real-JDK mode still registered it over the real class. This is the
        // runtime half of that same intent. It is the only production
        // registration site for the class (the others are `#[cfg(test)]`), so
        // dropping it here reproduces the default build exactly, and the real
        // StringWriter bytecode is self-contained — it just delegates to a real
        // StringBuffer.
        if self.drop_real_layout_synthetic && class_name == "java/io/StringWriter" {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/io/BufferedReader.read`/
        // `read([CII)I` and the synthetic `java/io/BufferedWriter` six-method
        // surface, both registered by
        // `native-builtins/src/phases_late/nio_file.rs`. Found 2026-09-17
        // alongside `InputStreamReader` above — same family, same missing
        // guard, both classes seen carrying real bytecode in the probe run
        // that found this.
        //
        // `BufferedReader.read()I`/`read([CII)I`: when the object was not
        // built by the (now-retired) `Files.newBufferedReader` side-table
        // path, these delegate straight to the wrapped `Reader` at slot 0,
        // BYPASSING `cb`/`nChars`/`nextChar` and the mark()/reset()
        // bookkeeping that real `BufferedReader.fill` does. MEASURED: H2's
        // RUNSCRIPT/CSV/INIT BOM-skip does `mark(1); read(); reset()`, and the
        // native `read()` consumed a character the real `reset()` then
        // rewound past — dropping the first character of every script
        // (`"create table"` parsed as `"reate table"`). This is not the
        // InputStreamReader defect's failure mode (that one clobbers a field);
        // this one runs on the RIGHT field (slot 0 is genuinely the wrapped
        // Reader on a real `BufferedReader` in this VM's layout) but skips
        // real semantics the field exists to support.
        //
        // `BufferedWriter`'s six (`write(String,II)`, `write(I)`,
        // `write([CII)`, `newLine`, `flush`, `close`): the delegating arm
        // (reached whenever slot 0 holds a real `Writer` rather than a
        // synthetic fd) forwards straight to that `Writer`, never touching
        // `cb`/`nextChar`. MEASURED (`probes/L4StreamTailSweep.java`, row
        // `writer buffered`): `new BufferedWriter(sw, 4).write("ab")` reached
        // the delegate immediately, where HotSpot holds it in the buffer.
        // Real `BufferedWriter`/`BufferedReader` bytecode is self-contained
        // once the wrapped stream is real (which it is by default — the
        // fd-backed synthetic paths behind these are already retired from
        // shipping builds), so drop the synthetic surface and let it run.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/io/BufferedReader" | "java/io/BufferedWriter"
            )
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/nio/CharBuffer.wrap`
        // factories (`wrap([C)` and `wrap(CharSequence)`,
        // `native-builtins/src/phases_late/charset_buffers.rs`,
        // `register_p62_char_buffer`).
        //
        // Same shape as the `java/io/StringWriter` entry just above: the
        // registration already sits behind `#[cfg(feature = "synthetic-jdk")]`
        // with a comment saying these are "DELIBERATELY not registered in
        // real-JDK mode", but a Cargo feature only decides what is COMPILED —
        // a feature-enabled binary RUN in real-JDK mode still registered both
        // over the real `java.nio.CharBuffer.wrap` bytecode. This is the
        // runtime half of that same intent, now paired with the matching
        // site-level `drops_real_layout_synthetic()` guard added there.
        //
        // `wrap(CharSequence)` is the concrete defect, not merely a
        // theoretical one: on HotSpot `CharBuffer.wrap(csq)` returns a
        // `StringCharBuffer` that HOLDS the `CharSequence` reference directly
        // — `isReadOnly() == true`, `hasArray() == false`, and a later
        // mutation of a mutable `csq` (e.g. a `StringBuilder`) is visible
        // through the buffer. The synthetic native instead COPIES the
        // sequence's chars into a fresh `char[]` and hands back a mutable
        // `HeapCharBuffer` (`isReadOnly() == false`, `hasArray() == true`) —
        // silently wrong rather than a crash: `put()` succeeds where HotSpot
        // throws `ReadOnlyBufferException`, and the buffer is frozen at
        // wrap-time instead of tracking the live sequence.
        //
        // It also feeds the CHARSET family below directly: Tomcat's
        // `MessageBytes.toBytes` is `encoder.encode(CharBuffer.wrap(charChunk))`,
        // so a wrongly-mutable, wrongly-copied `CharBuffer` would have flowed
        // straight into the real `CharsetEncoder` that entry restores. The
        // real `CharBuffer.wrap` bytecode is self-contained (it just builds a
        // real `HeapCharBuffer`/`StringCharBuffer` via the 3-arg overloads,
        // which are not intercepted anywhere in this file), so dropping the
        // synthetic surface is the whole fix.
        if self.drop_real_layout_synthetic
            && class_name == "java/nio/CharBuffer"
            && method_name == "wrap"
        {
            return;
        }
        // Real-JDK mode: drop the synthetic CHARSET family.
        //
        // These natives fabricate their objects with the ABSTRACT class as the
        // runtime class — `alloc_concurrent_synthetic("java/nio/charset/Charset", …)`
        // and the same for `CharsetEncoder`. On HotSpot `StandardCharsets.UTF_8`
        // is a `sun.nio.cs.UTF_8` and its encoder a `sun.nio.cs.UTF_8$Encoder`;
        // here both were instances of the abstract classes themselves. Any
        // method with no native to intercept it then resolves to an abstract
        // declaration, and the ones that WERE intercepted read a synthetic
        // 3-slot layout (`charset`/`averageBytesPerChar`/`maxBytesPerChar` at
        // field indices 0/1/2) that a real coder does not have.
        //
        // The visible result was silent and wrong rather than an error: EVERY
        // `String.getBytes` overload — no-arg, `(String)`, `(Charset)`, for
        // UTF-8, ASCII and ISO-8859-1 alike — returned a correctly-SIZED,
        // ZERO-FILLED array, and every decode round-trip failed with it.
        //
        // That is what made `regression-suite`'s `RCrypto` look like a crypto
        // defect. It is not: the SHA-256 was computed correctly over the wrong
        // input. `"abc".getBytes("UTF-8")` handed it three zero bytes, and
        // sha256(00 00 00) is exactly the 709e80c8… digest the suite reported.
        // `RStrings` failed the same way on its UTF-8 round-trip, so the two
        // classes were one defect.
        //
        // All four names have to go together, and the order in which they were
        // added is the evidence for that: dropping the coders alone moves the
        // failure from zeros to `AbstractMethodError: CharsetEncoder.encodeLoop
        // has no Code attribute`, because the Charset handing out the encoder
        // is still a fabricated abstract instance; adding `Charset` fixes the
        // no-arg and named overloads but leaves `getBytes(StandardCharsets.UTF_8)`
        // failing on `Charset.newEncoder`, because the STANDARD CHARSET OBJECT
        // is fabricated by its own registrations. With all four dropped the real
        // `sun.nio.cs` classes are constructed and every overload matches
        // HotSpot.
        //
        // Verified not to touch the shipping build: the default `cratonvm-cli`
        // build measures the identical 30-passed/1-failed with and without this
        // rule (the one failure, `RSocketChannelInterrupt`, is dev's own and
        // predates it). Synthetic-jdk MODE is byte-identical too —
        // `drop_real_layout_synthetic` is only ever set in a real-JDK arm.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/nio/charset/Charset"
                    | "java/nio/charset/StandardCharsets"
                    | "java/nio/charset/CharsetEncoder"
                    | "java/nio/charset/CharsetDecoder"
            )
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `FileChannel.open` FACTORY.
        //
        // `native_fc_open` (native-io) is the THIRD producer of an abstract
        // `java/nio/channels/FileChannel` instance, and the one the suite's
        // `RChannelInterrupt` actually reaches. The other two —
        // `FileSystemProvider.newFileChannel`'s fallback and
        // `RandomAccessFile.getChannel` — were audited first and are not on
        // this path; that audit is why this took two rounds to find.
        //
        // It does `alloc_object(FileChannel, 2)` and writes an fd id and a
        // position into slots 0/1. So `FileChannel.open(p, WRITE).getClass()`
        // was `java.nio.channels.FileChannel` itself where HotSpot 25 answers
        // `sun.nio.ch.FileChannelImpl`, and every method with no native to
        // intercept it resolved to an ABSTRACT declaration:
        // `AbstractMethodError: FileChannel.write(Ljava/nio/ByteBuffer;J)I has
        // no Code attribute`. Only the no-position `write(ByteBuffer)` had a
        // native at all. The same object also has no `interruptor`, which is
        // the field `AbstractInterruptibleChannel.begin()` dereferences — the
        // very defect `RChannelInterrupt` was written for.
        //
        // It is also wrong in a quieter way: the implementation is documented
        // "simplified" and calls `open_read`, IGNORING the `OpenOption[]`
        // entirely. `FileChannel.open(p, WRITE)` handed back a READ-ONLY fd.
        //
        // Dropping it is the whole fix because the real path is already built
        // and already forced: `FileChannel.open` bytecode calls
        // `FileSystemProvider.newFileChannel`, which is force-listed in
        // `native_override.rs` and routed to the base-class registration, and
        // that shim's RECONCILE-WITH-REAL block constructs a genuine
        // `sun.nio.ch.FileChannelImpl` via its 7-arg `open`. Instrumenting that
        // block previously produced NO output on this path — because
        // `native_fc_open` intercepted the call before the provider was ever
        // consulted. Its synthetic fallback stays in place, so a host where the
        // real construction fails keeps exactly today's behaviour.
        //
        // Scoped to `open` BY NAME. The instance natives on this class
        // (`read`/`write`/`position`/`size`/`close`) still serve that fallback
        // object; they do not intercept a real `FileChannelImpl`, whose own
        // declarations win because native dispatch keys on the resolved
        // method's declaring class.
        if self.drop_real_layout_synthetic
            && class_name == "java/nio/channels/FileChannel"
            && method_name == "open"
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/io/CharArrayReader` natives
        // (found 2026-09-17,
        // synthetic-jdk-feature-shadows-real-classes-in-real-jdk-mode-20260917.md,
        // same family as `StringReader`/`InputStreamReader` above). The
        // native-io registrar's own RDR-MIGRATION comment already documents
        // the mismatch: the synthetic layout is `buf`/`pos`/`count` at slots
        // 0/1/2, but the real JDK 25 `CharArrayReader` inherits `Reader.lock`
        // at slot 0 — so `<init>` clobbers the monitor object with the
        // caller's `char[]` instead of setting it up, and only `read()I` was
        // ever implemented (not the bulk `read([CII)I` a real
        // `BufferedReader.fill` calls). The registrar's site-level guard was
        // cfg-only until this pass; this is the mechanism-level half. The
        // real `CharArrayReader` bytecode is self-contained (no missing
        // native primitive), so drop the synthetic surface and let it run,
        // same as `StringReader`/`InputStreamReader` above.
        if self.drop_real_layout_synthetic && class_name == "java/io/CharArrayReader" {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/io/CharArrayWriter` natives.
        // Same 2026-09-17 sweep as `CharArrayReader` above. The native-io
        // registrar's own comment already names the concrete failure: the
        // synthetic 2-field carrier (`buf`/`count` at slots 0/1) collides
        // with the real JDK 25 layout, which is inherited `Writer.lock`
        // (slot 0) + `CharArrayWriter.buf` (slot 1) + `count` (slot 2) — so
        // the synthetic `<init>` never sets the inherited `lock`, and real
        // `write(String)` NPEs on `synchronized (lock)`. This is what turned
        // every JSP into an empty (HTTP 200, 0-byte body) servlet via
        // Jasper's `JspReader`. The real `CharArrayWriter` bytecode is
        // self-contained (its constructor chains through `Writer()`, which
        // sets `lock = this`), so drop the synthetic surface and let it run.
        if self.drop_real_layout_synthetic && class_name == "java/io/CharArrayWriter" {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/io/LineNumberReader`
        // natives. Same 2026-09-17 sweep. `LineNumberReader` extends
        // `BufferedReader`, and the synthetic 4-field layout
        // (`in`/`lineNumber`/`pos`/`content` at slots 0-3) treats slot 0 as
        // the wrapped Reader, but on a real object in this Reader chain slot
        // 0 is inherited `Reader.lock` — so the synthetic `<init>` clobbers
        // the monitor instead of recording the wrapped Reader, and the real
        // `BufferedReader`/`Reader` bytecode built on top of that never sees
        // a delegate. The real `LineNumberReader` bytecode builds on real
        // `BufferedReader`, which is itself real by the time this runs, so
        // drop the synthetic surface and let it run.
        if self.drop_real_layout_synthetic && class_name == "java/io/LineNumberReader" {
            return;
        }
        // Real-JDK mode: drop the synthetic base-class `java/io/Reader.read`
        // natives (both `()I` and `(Ljava/nio/CharBuffer;)I` — scoped by
        // method name, not the full class, because `Reader.close()` is a
        // separate, universally-registered entry that must stay). Same
        // 2026-09-17 sweep. `native_sr_read` (backing `read()I`) looks up
        // `this` in a per-instance side table that only `StringReader.<init>`
        // populates; registered on the BASE class it intercepts every real
        // Reader subclass that inherits the concrete `read()I` body instead
        // of overriding it (FileReader, CharArrayReader, BufferedReader,
        // ...), and on any of those the lookup misses and throws "Stream
        // closed" instead of running the real, correct, concrete bytecode.
        // `read(CharBuffer)` is not itself unsafe (it mirrors the real
        // default via `invoke_virtual`), but it is registered in the same
        // guarded block as `read()I` in native-io, so it is dropped alongside
        // it here too — real `Reader.read(CharBuffer)` produces the identical
        // result, so there is no behavior to lose.
        if self.drop_real_layout_synthetic
            && class_name == "java/io/Reader"
            && method_name == "read"
        {
            return;
        }
        // Real-JDK mode: drop the synthetic base-class `java/io/Writer.write`
        // native (scoped by method name for the same reason as `Reader.read`
        // above — other `Writer`-family entries are unaffected). Same
        // 2026-09-17 sweep. `native_sw_write_int` (backing `write(I)V`) reads
        // and writes `SW_FIELD_BUF`/`SW_FIELD_COUNT` — `StringWriter`'s own
        // `char[]`/`count` slots. Registered on the BASE class it intercepts
        // every real Writer subclass that inherits the concrete `write(I)V`
        // body (BufferedWriter, OutputStreamWriter, PrintWriter, FileWriter,
        // CharArrayWriter, ...), where those slots are a different field of a
        // different type — `set_array_element`ing into whatever object sits
        // there corrupts it instead of writing the character. The real
        // `Writer.write(I)V` bytecode delegates to the subclass's
        // `write([CII)V`, so it runs correctly without a native.
        if self.drop_real_layout_synthetic
            && class_name == "java/io/Writer"
            && method_name == "write"
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/nio/channels/FileLock`
        // instance natives (`<init>`/`position`/`size`/`isShared`/`isValid`/
        // `release`/`close` — distinct from `FileChannel.open` above, which
        // is the ABSTRACT `FileChannel` factory, not this class). Same
        // 2026-09-17 sweep; the native-io registrar's own header comment
        // already names the concrete failure: cratonvm's native-override
        // priority makes these SHADOW the concrete `sun/nio/ch/FileLockImpl`
        // methods, so `<init>` runs on the real `FileLockImpl` and writes its
        // in-process token into slot 5 — out of bounds on the real 5-field
        // layout the class comment measured via `javap` — and `release` only
        // clears that in-process token, never reaching the real
        // `FileChannelImpl.release` -> `FileLockTable.remove` path. A closed
        // lock then lingers in the JDK's per-file `FileLockTable`, and the
        // next `tryLock` on the same file throws `OverlappingFileLockException`
        // (H2 `SingleFileStore` reconnect / close+reopen). Real-JDK mode must
        // use the genuine `FileLockImpl` bytecode together with the
        // lock0/release0/FileKey natives that keep the `FileLockTable`
        // consistent, so drop this synthetic surface entirely.
        if self.drop_real_layout_synthetic && class_name == "java/nio/channels/FileLock" {
            return;
        }
        // Real-JDK mode: drop the synthetic `FileChannel.lock`/`tryLock`
        // NO-ARG natives (scoped by method name — the other `FileChannel`
        // entries, `open` above and `map`/`force`/`truncate`/`unmap0` etc.,
        // are unaffected). Same 2026-09-17 sweep, paired with the
        // `FileLock` entry above: these build a SYNTHETIC `FileLock` object
        // via the same synthetic FileLock natives that entry drops, so
        // leaving them registered here would hand back an object whose
        // instance natives no longer exist. Real-JDK mode uses the real,
        // final `FileChannel.lock()`/`tryLock()`, which delegate to the 3-arg
        // `FileChannelImpl` path and a genuine `FileLockImpl`.
        if self.drop_real_layout_synthetic
            && class_name == "java/nio/channels/FileChannel"
            && matches!(method_name, "lock" | "tryLock")
        {
            return;
        }
        // Real-JDK mode: drop the synthetic blocking/concurrent QUEUE family.
        // Every one of these is registered with the native-collections
        // four-slot layout (array/head/size/capacity), which no real JDK class
        // in the family has: `LinkedBlockingQueue` is
        // head/last/count/putLock/takeLock/notEmpty/notFull/capacity,
        // `ArrayBlockingQueue` is items/takeIndex/putIndex/count/lock/notEmpty/
        // notFull, and the `ConcurrentLinked*` pair is a CAS-linked node chain
        // with no lock fields at all. The synthetic `<init>` never assigns the
        // real ones, so the real bytecode NPEs on them the moment it runs.
        //
        // Only the Deque was dropped here originally, which left the other four
        // half-native: `offer` returned `true` into the side layout while
        // `size()` read 0 and the real `poll(timeout)`/`take()` bytecode NPE'd
        // on a null `takeLock`. That is silent, and it is load-bearing —
        // `ThreadPoolExecutor`'s work queue IS a `LinkedBlockingQueue`, so
        // `execute()` queued every task past `corePoolSize` into a store no
        // worker could see. Exactly `corePoolSize` tasks ran, the rest were
        // lost with no exception, and the pool never reached TERMINATED
        // (`newSingleThreadExecutor` resolved 1 of 6 futures). The real bodies
        // are self-contained — ReentrantLock/Condition for the blocking pair,
        // CAS for the ConcurrentLinked pair — so dropping the surface is all
        // that is needed.
        //
        // This only ever bit a binary built with the `synthetic-jdk` Cargo
        // feature and RUN in real-JDK mode: the default `cratonvm-cli` build
        // does not compile these registrations at all. That is precisely the
        // configuration the vm test gate and the regression suite use, so the
        // damage was to measurement rather than to shipped behaviour — seven
        // regression-suite classes were red for this reason alone.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/util/concurrent/LinkedBlockingDeque"
                    | "java/util/concurrent/LinkedBlockingQueue"
                    | "java/util/concurrent/ArrayBlockingQueue"
                    | "java/util/concurrent/ConcurrentLinkedQueue"
                    | "java/util/concurrent/ConcurrentLinkedDeque"
                    | "java/util/concurrent/BlockingQueue"
            )
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/lang/System`
        // `initPhase1`/`initPhase2`/`initPhase3` overrides
        // (`native-builtins/src/lib.rs`, `register_essential_natives_with_shims`).
        // The site's own comment already documents the intent — "only
        // override in synthetic-jdk mode. In real-JDK mode the actual JDK
        // bytecode runs, supported by the T14 ... natives" — but the
        // `#[cfg(feature = "synthetic-jdk")]` guarding it answers a
        // compile-time question, not a runtime one. `native_system_init_phase1`
        // unconditionally writes `System.out`/`err` static fields, stamps a
        // Charset onto them, and force-initializes the `System` class — real
        // VM-bootstrap side effects that the real `initPhase1` bytecode,
        // backed by the T14 `SystemProps$Raw`/`FileDescriptor`/
        // `Win32ErrorMode` natives named in that comment, already performs.
        // A `--features synthetic-jdk` binary run against a real JDK
        // registered this over the real method, so it ran INSTEAD of, not
        // alongside, the real bootstrap the T14 natives were built to
        // support. This entry is the mechanism-level half; the site-level
        // half is the matching `drops_real_layout_synthetic()` guard added
        // there.
        if self.drop_real_layout_synthetic
            && class_name == "java/lang/System"
            && matches!(method_name, "initPhase1" | "initPhase2" | "initPhase3")
        {
            return;
        }
        // Real-JDK mode: drop the synthetic JMX MXBean type-mapping overlay —
        // `ConvertingMethod.from`, `MXBeanMappingFactory`/
        // `DefaultMXBeanMappingFactory` `.mappingForType`/`.makeMapping`, and
        // `MXBeanMapping.toOpenValue`/`.fromOpenValue` (`native-builtins/src/
        // jmx_openmbean.rs`, `register_jmx_openmbean_natives_with`'s
        // `synthetic_mapping` arm).
        //
        // That file's own header already measured (2026-08-11, against JDK 25)
        // that leaving the REAL `com.sun.jmx.mbeanserver` bytecode alone is
        // strictly better than this overlay: with the primary fix (the
        // unconditional `MBeanIntrospector.getMethods` Object-method filter,
        // NOT part of this list) removing the `Class` -> `AnnotatedType[]`
        // self-reference at the source, the real `DefaultMXBeanMappingFactory`
        // recursion terminates and every platform-MXBean attribute matches
        // HotSpot exactly — `MemoryUsage` becomes a real `CompositeDataSupport`
        // instead of the overlay's raw `java.lang.management.MemoryUsage`
        // typed as `SimpleType.STRING`, `SystemProperties` becomes a real
        // `TabularDataSupport`, and a genuinely self-referential MXBean type is
        // rejected with HotSpot's own `NotCompliantMBeanException` instead of
        // being silently accepted as a String. That measurement is why
        // `real_mxbean_mapping_enabled()` already makes this overlay OFF by
        // default on a real-JDK run of the default build.
        //
        // The gate at that call site used to be `cfg!(feature =
        // "synthetic-jdk")` alone, which asks what was COMPILED rather than
        // which CLASS LIBRARY is LOADED: a `--features synthetic-jdk` binary
        // run against a real JDK satisfied the cfg and installed the overlay
        // over real, loadable bytecode anyway. The site now also reads
        // `drops_real_layout_synthetic()`; this entry is the mechanism-level
        // half, same relationship `InputStreamReader`/`StringWriter` have to
        // their own site-level guards above.
        //
        // Also confirmed NOT self-inflicted by a missing native: none of
        // `ConvertingMethod`/`MXBeanMapping`/`MXBeanMappingFactory`/
        // `DefaultMXBeanMappingFactory` is in `classloading::class_manager`'s
        // synthetic-field-count fallback table for any reason other than "the
        // real JDK class file can't be loaded" (see that table's own
        // System$1/System$2 and FileSystemException entries) — the fallback
        // is a last resort, not evidence the real class needs one.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "com/sun/jmx/mbeanserver/ConvertingMethod"
                    | "com/sun/jmx/mbeanserver/MXBeanMappingFactory"
                    | "com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory"
                    | "com/sun/jmx/mbeanserver/MXBeanMapping"
            )
        {
            return;
        }
        // Real-JDK mode: drop the synthetic ScheduledThreadPoolExecutor surface.
        // The old constructors only wrote two synthetic slots, leaving real JDK
        // ThreadPoolExecutor fields such as `workQueue` null; Tomcat's
        // ContainerBase then failed in scheduleWithFixedDelay -> delayedExecute.
        // Let the real STPE constructors and scheduling bytecode initialize the
        // inherited executor state coherently.
        //
        // RETIRED 2026-09-17 (lane-concurrency-forkjoin-scheduler-20260917.md
        // item 2): the 3-arg constructor and getCorePoolSize()I used to be
        // kept here as real-layout Bridges for Spring's
        // ThreadPoolTaskScheduler anonymous subclass. A keep arm here cannot
        // coexist with a `RETIRED_SHADOW_L5_TRIPLES` entry for the same two
        // triples (the `Bridge -> SyntheticStub` re-tag in
        // `NativeMethodRegistry::register` runs before this predicate is ever
        // consulted, so a table entry drops the native in real-JDK mode
        // regardless of what this arm says) -- see that table's own comment.
        // Both triples are now in the table instead; this whole class drops
        // unconditionally like its neighbours above. Measured before landing:
        // `L5StpeKeepArm.java` (7/7, both modes, unchanged) and
        // the real-JDK Spring `scheduling.concurrent.*` cluster this arm's
        // own comment named (10 classes, 200 test methods, 189 passed/0
        // failed/11 pre-existing skips, byte-identical against the tree with
        // the keep arm still in place) -- see the FIXED write-up.
        if self.drop_real_layout_synthetic
            && class_name == "java/util/concurrent/ScheduledThreadPoolExecutor"
        {
            return;
        }
        // Real-JDK mode: drop the `Executors` POOL FACTORIES so the real
        // `java.util.concurrent.Executors` bytecode builds every executor.
        //
        // JDK-ONLY-WAVE2 L10 (`L10-blocker-threadpool-init-DONE-20260806.md`,
        // `docs/feature-designs/native-dispatch-architecture.md` P1). The scheduled pair has been
        // dropped here since the Tomcat `ContainerBase` fix; the three plain-pool
        // factories were the ones still fabricating. What they did was subtler
        // than a two-slot stub and is worth stating, because the obvious reading
        // of the code says they were already fine:
        //
        //   `alloc_concurrent_synthetic(.., "java/util/concurrent/ThreadPoolExecutor", 2)`
        //   followed by `initialize_real_thread_pool_executor`, which
        //   `invoke_special`s the real `ThreadPoolExecutor.<init>`.
        //
        // On the happy path that does produce a genuinely real executor — which
        // is why the fixed/cached transcripts already matched HotSpot. But it
        // keeps TWO fallbacks (`stpe_legacy_slot_init`, taken when the
        // `BlockingQueue` or the `TimeUnit` constant cannot be built) that write
        // the historical two-slot shape onto a real-layout object and return it
        // as if construction had succeeded. A fallback that silently hands back a
        // half-constructed executor is exactly the state the eight
        // `ThreadPoolExecutor.execute` receiver-shape dispatch sites exist to
        // detect, so leaving it in place would keep the predicate they consult
        // conditionally true — and "conditionally true" is what blocks deleting
        // them (L11).
        //
        // And one of the three was observably wrong, not just fragile:
        // `newSingleThreadExecutor()` returned a bare `ThreadPoolExecutor`, where
        // the real JDK returns `Executors$AutoShutdownDelegatedExecutorService`
        // wrapping one. Every `instanceof ThreadPoolExecutor` a caller writes
        // flipped, and the pool was reconfigurable when the JDK guarantees it is
        // not. Measured against HotSpot 25 by `probes/L10ThreadPoolInitProbe`:
        // `single.class` and `single.isTpe` were the ONLY two divergent lines out
        // of 62 in both `--real-jdk` and `--real-jdk`.
        //
        // Dropping is the whole fix, and it is a registration-time decision on
        // purpose — the L11/item-3 precedent
        // (`forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md`)
        // is that a policy expressed at dispatch has to be restated once per
        // dispatch path, while a policy expressed here is invisible to all of
        // them at once. With no native, `Executors.newFixedThreadPool` et al. run
        // their own bytecode — `new ThreadPoolExecutor(...)`, or for the single-
        // thread case the delegating wrapper — so a factory-built executor is
        // real by CONSTRUCTION rather than by a fallback that happened not to be
        // taken. `newSingleThreadExecutor(ThreadFactory)` was never registered
        // and has always run that real path, which is what made this safe to do
        // rather than merely desirable.
        //
        // Scoped to the pool factories by NAME. `Executors.callable`,
        // `defaultThreadFactory`, `privilegedThreadFactory` and the
        // `unconfigurable*` wrappers are not fabrications and keep whatever
        // registration they have; and the synthetic-JDK build never sets this
        // flag, so its own two-slot executor model is untouched.
        if self.drop_real_layout_synthetic
            && class_name == "java/util/concurrent/Executors"
            && matches!(
                method_name,
                "newScheduledThreadPool"
                    | "newSingleThreadScheduledExecutor"
                    | "newFixedThreadPool"
                    | "newCachedThreadPool"
                    | "newSingleThreadExecutor"
            )
        {
            return;
        }
        // Real-JDK mode: drop the two `identity()` FACTORIES so `java.base`'s
        // own bytecode runs and the VM spins a real lambda proxy.
        //
        // JDK-ONLY-WAVE2 L18 (`docs/feature-designs/native-dispatch-architecture.md`).
        // `Function.identity()` and `UnaryOperator.identity()` are each a single
        // `invokedynamic` returning `t -> t` (verified with `javap -p -c` against
        // the JDK 25 image), so the class HotSpot answers with is a generated
        // `$$Lambda` hidden class: `isSynthetic()` true, name containing
        // `$$Lambda`. CratonVM intercepted both with a native returning a
        // hand-made `java/util/function/Function$Identity` stand-in — an
        // ordinary named class that fails all three predicates
        // (`RJdkLambdas.java:61,62,66`). `--real-jdk` already drops these
        // (`SyntheticStub`) and passes that block; this makes real-JDK mode
        // agree. The `invokedynamic` opcode is short-circuited in
        // `vm/src/runtime/invokedynamic.rs` and never calls the
        // `LambdaMetafactory` natives, so the proxy-spinning machinery is the
        // same code in both modes and the strict arm's result transfers.
        //
        // Only the FACTORIES. `Function$Identity.{apply,andThen,compose}` keep
        // their registrations and simply become unreachable — dropping the
        // instance methods while one of the two factory copies survived is
        // exactly the 2026-07-14 `d8092acb` regression
        // (`UnsatisfiedLinkError: Function$Identity.andThen` on WildFly boot).
        // Dropping by class+method here covers BOTH factory copies
        // (`native-builtins/src/lib.rs`'s `register_function_identity_natives`
        // and `native-builtins/src/phases_late/streams.rs`) at once, so that
        // split cannot recur. And the synthetic-JDK build never sets this flag,
        // so its stand-in — which has no real bytecode to fall back to — is
        // untouched.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/util/function/Function" | "java/util/function/UnaryOperator"
            )
            && method_name == "identity"
        {
            return;
        }
        // Real-JDK mode: drop legacy regex natives. They were written for the
        // old synthetic two-field Pattern / six-field Matcher layout; on real
        // OpenJDK objects they corrupt slots and bypass constructors, so later
        // Pattern/Matcher bytecode observes impossible state (for example a
        // Matcher whose `locals` field is not an int[]). Let the JDK regex
        // bytecode own both object construction and matching in real mode.
        //
        // EXCEPTION (`CRATONVM_NATIVE_MATCHER_FIND`) — keep the real-JDK-layout
        // `Matcher.find()`/`find(int)` fast path. Unlike the legacy natives
        // this drop exists to suppress, that fast path never assumes a
        // synthetic field layout: it resolves every field by name against
        // whatever real OpenJDK object is actually there (see
        // `native-builtins/src/lib.rs`'s `native_matcher_find_realjdk`), so
        // it does not corrupt anything the way the old slot-index bridge did
        // — a correct, same-answer-just-faster fast path is exactly what
        // `NativeKind::Intrinsic` means per this enum's own doc comment.
        //
        // Registered under `NativeKind::Intrinsic` SPECIFICALLY BECAUSE the
        // legacy synthetic-layout `Matcher.find`/`find(int)` registrations
        // this drop targets run under `NativeKind::Bridge` in the real-JDK
        // build (inherited from a persistent `set_category(Bridge)` far
        // above their registration site) — an earlier version of this
        // exception keyed on `Bridge` and, because of that inherited
        // category, ALSO accidentally un-dropped the legacy bridge, which
        // then corrupted every real `Matcher` via its raw synthetic slot
        // indices (symptom: `Matcher.start()` throwing after a second
        // `find()`, reproduced even with `CRATONVM_NATIVE_MATCHER_FIND`
        // unset). `Intrinsic` is registered nowhere else in this file for
        // `java/util/regex/Pattern`/`Matcher` under `drop_real_layout_synthetic`
        // (confirmed: the only other `register_regex_natives()` call site
        // that runs under `Intrinsic` is `register_synthetic_overrides`,
        // which only executes when `drop_real_layout_synthetic` is unset in
        // the first place, so the outer `if` below short-circuits before this
        // exception is even consulted there) — category-matching is
        // otherwise inherently fragile (any future `set_category` reshuffle
        // upstream of either registration site can silently reintroduce this
        // exact collision), so treat `Intrinsic` here as load-bearing: do not
        // change this registration's category without re-auditing every
        // `set_category`/`with_category` call between both `register_regex_natives`
        // call sites and the top of `register_essential_natives`.
        let keep_real_matcher_find_fastpath = kind == NativeKind::Intrinsic
            && class_name == "java/util/regex/Matcher"
            && matches!(
                (method_name, descriptor),
                ("find", "()Z")
                    | ("find", "(I)Z")
                    | ("start", "()I")
                    | ("start", "(I)I")
                    | ("end", "()I")
                    | ("end", "(I)I")
                    | ("group", "()Ljava/lang/String;")
                    | ("group", "(I)Ljava/lang/String;")
                    // nio21 wave 11 L2: settle the fast path's pending
                    // `search(from)` in JDK bytecode, so hitEnd / requireEnd
                    // read what java.util.regex would have left there.
                    | ("hitEnd", "()Z")
                    | ("requireEnd", "()Z")
            );
        // Pattern is immutable after its constructor finishes.  The two
        // static factories below may therefore return a VM-rooted, fully
        // constructed real-JDK Pattern from a bounded cache; unlike the old
        // synthetic regex bridge they never fabricate or partially initialize
        // a Pattern/Matcher layout.
        let keep_real_pattern_compile_cache = kind == NativeKind::Intrinsic
            && class_name == "java/util/regex/Pattern"
            && method_name == "compile"
            && matches!(
                descriptor,
                "(Ljava/lang/String;)Ljava/util/regex/Pattern;"
                    | "(Ljava/lang/String;I)Ljava/util/regex/Pattern;"
            );
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/util/regex/Pattern" | "java/util/regex/Matcher"
            )
            && !keep_real_matcher_find_fastpath
            && !keep_real_pattern_compile_cache
        {
            return;
        }
        // Real-JDK mode: drop bridge/intrinsic `java/util/StringJoiner` natives
        // with hardcoded synthetic layouts so the real 7-field-layout bytecode
        // runs. Keep SyntheticStub-tagged fallbacks registered: dispatch skips
        // them for a loaded real StringJoiner, but synthetic fallback classes
        // still need a small native surface.
        if self.drop_real_layout_synthetic
            && class_name == "java/util/StringJoiner"
            && kind != NativeKind::SyntheticStub
        {
            return;
        }
        // Real-JDK mode: `java/lang/String`'s real class bytes are
        // authoritative (contract §1.4), so a `Bridge` registered on it does
        // not survive into a real-JDK registry at all.
        //
        // # This is where the forced-native `String` policy went
        //
        // FOUR hard-coded lists used to decide this at DISPATCH time — a
        // 21-name descriptor-blind arm in `check_override`, a 12-shape
        // inverted whitelist in `force_native_over_real_jdk_bytecode`, a JIT
        // direct bind for `toLowerCase(Locale)`, and
        // `is_jdk_string_charset_name_constructor_override`, which the record
        // that catalogued the first three had missed because it is a
        // differently-named predicate rather than a `String` list. They
        // disagreed with each other, so which implementation of
        // `String.equals`/`hashCode`/`substring` ran depended on how many times
        // the call site had executed. All four are deleted.
        //
        // They are not deleted because they looked redundant. They were
        // MEASURED inert first: a binary with both interpreter lists removed
        // produced a byte-identical 392-case `String` transcript in both modes
        // and identical invocation counts on all 38 exercised `String` registry
        // slots. Neither list ever decided anything, because
        // `resolve_step1_native` (`try_stackless_invoke` step 1) resolves the
        // triple and dispatches whatever it finds before either list runs, and
        // it has no list of its own. Registration was always the real gate;
        // this is that gate, stated once, for every dispatch path.
        //
        // # Why the rule is a KIND and not another method list
        //
        // Adjudicated against the JDK 25 image, 79 of the 80 registrations on
        // `java/lang/String` target a method the image declares with a `Code`
        // attribute — §1.4's `NativeShadowsBytecode` — and exactly one,
        // `intern()`, is genuinely `ACC_NATIVE` and therefore a legitimate
        // §1.5 bridge. So "drop the bridges, keep `intern`" is the adjudication
        // itself rather than a list that has to be kept in step with one.
        //
        // `Intrinsic` survives on purpose: §1.4's reviewed exception. The
        // `CRATONVM_NATIVE_STRING_REGEX` family registers under it
        // (`register_with_kind`, so `kind_stated` is true and the census can
        // see somebody decided), which is what a same-answer-just-faster
        // native is supposed to look like — it wins on kind, with no name
        // anywhere. `SyntheticStub` survives too: it never dispatches under
        // `--real-jdk`, and in compatible mode it is the fallback for a
        // synthetic `String` that has no real class bytes behind it.
        //
        // Anything promoted into this exception needs the evidence, not the
        // intent: `probes/StringPolicyMatrixProbe` diffed against HotSpot, and
        // a measurement that the native is actually faster. The five h2-bnf
        // shapes were argued to be "trivially equivalent to the real bytecode
        // for every input" and were wrong about that for unpaired surrogates,
        // for every out-of-range index's exception message, and for `null`.
        if self.drop_real_layout_synthetic
            && class_name == "java/lang/String"
            && matches!(kind, NativeKind::Bridge | NativeKind::SyntheticStub)
            && !(method_name == "intern" && descriptor == "()Ljava/lang/String;")
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/lang/ref/Cleaner`/
        // `Cleaner$Cleanable` natives (`create()`, `register(Object,Runnable)`,
        // `Cleanable.clean()`). These were meant only as a fallback for when
        // real class bytes are unavailable (see this block's own comment at
        // the registration site, phases_late.rs::register_p68_cleaner: "real
        // Cleaner bytecode still wins whenever the real class is loaded") --
        // but `create()` is a STATIC factory method, and static dispatch has
        // no per-instance real-vs-synthetic safety net the way concrete
        // instance methods do, so the native unconditionally wins there and
        // allocates a bare Cleaner with its real `impl` field left null.
        // `register(Object,Runnable)` (an instance method) then correctly
        // prefers real bytecode -- which calls `PhantomCleanable.<init>` ->
        // `CleanerImpl.getCleanerImpl(this)` -> reads the null `impl` field
        // and NPEs ("Cannot read field \"queue\" because the return value of
        // ... getCleanerImpl(...) is null"), first seen booting a WildFly
        // Host Controller (`ServiceContainer$Factory.create()` calls
        // `Cleaner.create()` then `.register(...)`). Same half-real-object
        // bug class as the ThreadPoolExecutor/Executors-factory NPEs above --
        // drop the synthetic surface entirely so real bytecode constructs and
        // wires up the Cleaner end-to-end (matches this file's own stated
        // intent, just enforced from the registration side since dispatch
        // does not enforce it uniformly for static factory methods).
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/lang/ref/Cleaner" | "java/lang/ref/Cleaner$Cleanable"
            )
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/lang/ref/ReferenceQueue`
        // CONSTRUCTOR so the real one runs. The synthetic ctor only writes the
        // two-slot (head, size) shape this file's natives use; a real JDK 25
        // `ReferenceQueue` additionally declares `private final Lock lock`,
        // which the real `enqueue`/`poll`/`remove` bytecode synchronizes on.
        // Leaving it null made every real `ReferenceQueue.enqueue()` throw
        // `NullPointerException: Cannot enter synchronized block because
        // "this.lock" is null` — reached from `Reference.enqueue()`, whose
        // native already (correctly) delegates to the real `enqueue` bytecode
        // for a real-layout Reference. Symptom: Spring's
        // `ConcurrentReferenceHashMap` failing to purge stale entries, which
        // aborts `AbstractApplicationContext.resetCommonCaches()` and hence
        // any context refresh that has to be cancelled
        // (`scripting.{bsh,config,groovy}.*` in the Spring suite).
        //
        // Only the constructor is dropped. `poll`/`remove` stay native: the
        // GC's reference processor enqueues by writing the queue's head slot
        // directly (`vm/src/runtime/interpreter.rs`) and never notifies the
        // real `lock`, so real blocking `remove()` bytecode would wait forever.
        if self.drop_real_layout_synthetic
            && class_name == "java/lang/ref/ReferenceQueue"
            && method_name == "<init>"
        {
            return;
        }
        // NOTE: real-JDK mode used to drop EVERY native registered directly on
        // `java/util/concurrent/ThreadPoolExecutor` here (submit/execute/
        // shutdown included), on the theory that only genuinely-real
        // `ThreadPoolExecutor` instances carry that class name. That's false:
        // `Executors.newSingleThreadExecutor()`/`newFixedThreadPool()`/
        // `newCachedThreadPool()` (registered on `Executors` below) also stamp
        // their synthetic 2-field return object with this exact class name
        // (see `alloc_concurrent_synthetic` call sites in
        // `register_executor_natives`), so a class-name-keyed drop can't tell
        // the two apart — it silently starved the synthetic objects' own
        // `execute()`/`submit()`/`shutdown()` overrides too, sending them
        // straight to real JDK bytecode that dereferences an uninitialized
        // `ctl`/`mainLock` field and NPEs
        // (`threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`,
        // `threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor-FIXED.md`).
        // A prior narrower fix (merged separately, same day) exempted only
        // `execute(Runnable)` from this drop and pushed the real-vs-synthetic
        // distinction into the interpreter's dispatch layer instead
        // (`force_native_over_real_jdk_bytecode` / `intercept_force_registered_native`
        // in vm/src/runtime/interpreter.rs, plus matching checks in
        // vm/src/vm/vm_exec.rs) — those checks are still in place and harmless,
        // but they don't cover every dispatch path (`try_stackless_invoke`'s own
        // direct native lookup isn't one of the patched call sites), so a real
        // receiver could still reach `native_es_execute` and — in that fix —
        // degrade to synchronous inline execution. This drop is now removed
        // entirely (not just for `execute`), and the real-vs-synthetic
        // distinction happens per-*instance* inside
        // `native_es_execute`/`native_es_submit_*`/the `shutdown` closures
        // (`executor_has_real_workers`), which forward a genuinely-real
        // receiver to real bytecode via
        // `NativeContext::invoke_virtual_bytecode_only` — regardless of which
        // dispatch path reached the native, and preserving true async
        // semantics for a real pool's `execute()` (not just synchronous
        // fallback).
        // CAPABILITY GATE (`NativeRegister`). Deliberately the LAST drop arm:
        // every arm above answers "should this native exist at all in this
        // build", which is a compatibility question; this one answers "is this
        // VM permitted to install native code for this triple", which is a
        // security question, and it must see exactly the set of registrations
        // that would otherwise be accepted.
        //
        // Under the default (no policy installed) this is one `Option`
        // discriminant test per registration and nothing else — the ~3,100
        // boot registrations pay a predicted not-taken branch.
        //
        // A refusal RETURNS WITHOUT INSERTING, like the Real-JDK arm: nothing
        // reaches `registrations` / `categories` / `provenance` / `slots`, so
        // the refused triple never appears in the census and `generation()`
        // does not move. The denial is recorded in the capability audit log,
        // which is where an operator looks for it.
        if let Some(caps) = self.capabilities.as_ref() {
            // `caps.check(Capability::native_register(class_name, method_name))`,
            // without allocating the scope name per registration.
            if caps.check_native_register(class_name, method_name).is_err() {
                return;
            }
        }
        // `native_method_hash(class_name, ..)`, with the class half read from
        // the per-class memo (`ClassFacts::class_hash` is `native_class_hash`).
        let class_hash = self.class_facts(class_name).class_hash;
        let key = native_method_hash_from(class_hash, method_name, descriptor);
        // With 128-bit composite keys, collisions on our keyspace are
        // vanishingly unlikely. We keep a cheap `debug_assert!` as
        // defense-in-depth: if `register()` ever overwrites an existing
        // entry, the most common cause is legitimate re-registration of
        // the same triple (e.g. by `alias_class` running twice). A true
        // hash collision would only be flagged if it produced a
        // pre-existing key for a triple we have NOT seen before.
        //
        // Duplicate-registration / collision detection MUST use a stable
        // key — the `(class, name, descriptor)` triple — not `fn` pointer
        // equality. Rust `fn` pointer comparison is unreliable: the
        // compiler may merge identical functions or duplicate them across
        // codegen units, so comparing the prior callback produces no
        // meaningful result (and triggers the
        // `unpredictable_function_pointer_comparisons` lint). If the key is
        // already occupied it is a legitimate re-registration iff the very
        // same triple appears in `registrations`; otherwise it is a true
        // 128-bit hash collision.
        //
        // NOTE (memoization work, 2026-07-26): this remains a `debug_assert`
        // — the *registration-time* check — but it is no longer the only line
        // of defense. `slot_index_for_key` now re-verifies the full triple on every
        // lookup, so in a release build a true collision degrades to "the
        // loser's triple resolves to `None`" rather than "the loser's triple
        // silently dispatches the winner's callback". See the `registrations`
        // field doc for the `class_manager::name_to_id` precedent.
        let prior_slot = self.slot_by_key.get(&key).copied();
        debug_assert!(
            prior_slot.is_none()
                || self
                    .registrations
                    .iter()
                    .any(|(c, m, d)| c == class_name && m == method_name && d == descriptor),
            "NativeMethodRegistry 128-bit hash collision for {class_name}.{method_name}{descriptor} (key={key:?})"
        );
        // Index of this registration's triple in `registrations`, used below
        // to back the deferred native-ring name map without a second copy of
        // the parts (see the `name_index` field doc), and by every slot to
        // name-verify a digest hit.
        let reg_index = self.registrations.len();
        self.registrations.push(class_name, method_name, descriptor);
        // Arm the negative-lookup prefilter. Kept adjacent to the
        // `registrations` push — the one statement pair that must never drift
        // apart, because a class in `registrations` but absent here would make
        // `slot_for_exact` answer `None` for a native that IS registered.
        self.classes_with_natives.insert(class_hash);
        self.categories.push(kind);
        let overwrote = prior_slot.and_then(|idx| self.slots.get(idx as usize).map(|s| s.kind));
        self.provenance.push(RegistrationProvenance {
            site: core::panic::Location::caller(),
            overwrote,
        });
        let category = kind;
        // The leaf claim is a property of the CALLBACK being registered, not of
        // the triple: it travels with `current_leaf` exactly like `category`
        // does, so a later phase that re-registers the same triple with a
        // different body silently demotes it to the funnel unless that phase
        // opts in too. See `set_leaf`.
        let leaf = self.current_leaf;
        match prior_slot {
            Some(idx) => {
                if let Some(slot) = self.slots.get_mut(idx as usize) {
                    slot.callback = callback;
                    slot.kind = category;
                    slot.reg_index = reg_index as u32;
                    slot.leaf = leaf;
                    self.mutation_epoch = self.mutation_epoch.wrapping_add(1);
                }
            }
            None => {
                let idx = self.slots.len() as u32;
                self.slots.push(NativeSlot {
                    callback,
                    kind: category,
                    reg_index: reg_index as u32,
                    leaf,
                });
                // The ONLY place `slot_invocations` grows — it must stay
                // index-parallel with `slots`, and `slots` only ever grows in
                // this arm (the `Some` arm updates in place, which is what keeps
                // an already-issued `NativeMethodId` valid). Keeping the two
                // pushes adjacent is the same discipline the
                // `registrations`/`classes_with_natives` pair documents above.
                for shard in self.slot_invocations.iter_mut() {
                    shard.push(std::sync::atomic::AtomicU64::new(0));
                }
                // Index-parallel with `slots` for the same reason and by the
                // same rule: this is the only arm that grows the slot table,
                // so it is the only arm that may grow either sidecar. A slot
                // starts out claiming a complete count; only a bypassing
                // dispatch path clears that claim, via
                // `mark_invocations_incomplete`.
                self.slot_invocations_incomplete
                    .push(std::sync::atomic::AtomicBool::new(false));
                self.slot_by_key.insert(key, idx);
            }
        }
        // CAPABILITY: classify the slot once, here, so the dispatch-side gate
        // (`check_dispatch_capability`) is an integer map lookup rather than a
        // per-invocation string match. Populated only when a policy is
        // installed — with none there is nothing to gate, and the map stays
        // empty and unallocated.
        //
        // Kept adjacent to the slot publication above because it is keyed by
        // the slot index, and re-registration of a triple UPDATES the slot in
        // place: recomputing here keeps the classification tracking whichever
        // triple currently owns the slot.
        if self.capabilities.is_some() {
            let slot_index = match prior_slot {
                Some(idx) => idx,
                None => (self.slots.len() - 1) as u32,
            };
            match crate::capability::classify_native(class_name, method_name) {
                Some(kind) => {
                    self.sensitive_slots.insert(slot_index, kind);
                }
                None => {
                    // A re-registration may have replaced a sensitive triple
                    // with a non-sensitive one; drop the stale classification
                    // rather than leaving the slot gated for the wrong reason.
                    self.sensitive_slots.remove(&slot_index);
                }
            }
        }
        // AUDIT 2026-05-17 (Fix 5): also populate the class-agnostic
        // (method, descriptor) index used by `find_by_method_descriptor`.
        // Reuse `native_method_hash` with an empty class string so the
        // key is independent of the registering class.
        //
        // Collision semantics: when two different classes register the
        // same `(method, descriptor)` pair, the FIRST registration wins
        // and is kept. `entry().or_insert()` (not `insert()`) guarantees
        // a later registration never silently overwrites an earlier one,
        // which is what `find_by_method_descriptor`'s "first match"
        // contract requires.
        let md_key = native_method_hash("", method_name, descriptor);
        self.by_method_desc.entry(md_key).or_insert(callback);
        // Native-call ring buffer: associate this callback pointer with its
        // human-readable `class.method desc` name so a watchdog dump can resolve
        // raw `fn` pointers back to method names.
        //
        // WF32-fix (history): the name map used to be populated UNCONDITIONALLY
        // and eagerly here, with a per-`register()` `format!` allocation **plus**
        // a cross-module `name_map()` `Mutex` round-trip. The reason it wasn't
        // gated behind `native_ring::is_enabled()` was correctness: boot
        // registers ~3,100 natives *before* the watchdog arms recording, so a
        // naive skip-while-disabled lost every name and the dump showed only
        // useless `<unknown cb@0x...>` pointers.
        //
        // PERF FIX: the eager work is now deferred. The default (ring disabled)
        // path records ONLY a cheap `cb_ptr -> reg_index` entry — no `format!`
        // heap allocation, no cross-module lock — and the actual name strings
        // are materialized lazily by `flush_native_ring_names()` if/when a
        // diagnostic is actually requested (the ring is enabled). Names are NOT
        // lost: the full triple is already persisted in `registrations[reg_index]`
        // (pushed unconditionally above), so `flush_native_ring_names()` can
        // rebuild every name on demand. This preserves the WF32 guarantee while
        // removing the per-boot-registration `format!` + `Mutex` cost.
        //
        // If recording is ALREADY enabled at register time (rare — e.g. a native
        // registered after the watchdog armed), populate the ring's name map
        // eagerly so a dump that races registration still resolves the name. The
        // cheap index is recorded in both cases so a later `flush_*` is complete.
        // First-write-wins to mirror the prior `register_name`/`or_insert_with`.
        self.name_index
            .entry(callback as usize)
            .or_insert(reg_index);
        if crate::native_ring::is_enabled() {
            let triple = format!("{class_name}.{method_name}{descriptor}");
            crate::native_ring::register_name(callback as usize, &triple);
        }
    }

    /// Materialize the deferred native-call-ring name map: walk the
    /// `cb_ptr -> registration-index` index built cheaply at `register()` time
    /// and publish each `fn-ptr → "class.method desc"` mapping into
    /// `native_ring`'s name map (idempotent / first-write-wins).
    ///
    /// PERF (native-ring lazy name map): this is the lazy counterpart to the
    /// deferral in `register()`. Boot registration no longer pays a `format!`
    /// allocation + `Mutex` lock per native for a diagnostic that is off by
    /// default; instead, the watchdog (or whoever arms the ring) calls this
    /// ONCE when recording is actually turned on, paying the ~3,100 `format!`
    /// allocations only then. Calling it is cheap to repeat: `register_name`
    /// uses `or_insert_with`, so already-published names are left untouched.
    ///
    /// CROSS-FILE FOLLOW-UP (out of this file's scope): the ring arm site —
    /// where `native_ring::enable(true)` is called (per native_ring.rs docs:
    /// `vm-cli/src/main.rs`, on `--stack-dump-on-timeout=N>0` /
    /// `CRATONVM_ENABLE_NATIVE_RING=1`) — should call this on the global
    /// `NativeMethodRegistry` immediately after `enable(true)` so a subsequent
    /// dump resolves names. Until that wiring lands, names registered while the
    /// ring was disabled resolve as `<unknown cb@0x...>` in the dump (same
    /// failure mode the WF32-fix originally addressed, but now opt-in and only
    /// when the wiring is absent — semantics of registration are unchanged).
    pub fn flush_native_ring_names(&self) {
        for (&cb_ptr, &idx) in &self.name_index {
            if let Some((c, m, d)) = self.registrations.get(idx) {
                let triple = format!("{c}.{m}{d}");
                crate::native_ring::register_name(cb_ptr, &triple);
            }
        }
    }

    /// Combined `find` + `kind_of`: computes the 128-bit
    /// `(class, method, descriptor)` hash once and looks up both the
    /// callback and its category from it, instead of the two independent
    /// hashes (one full byte-walk each) `invoke_or_native`'s
    /// synthetic-stub check used to pay on every native dispatch --
    /// `find(...)` to get the callback, then immediately `kind_of(...)`
    /// with the identical three strings to classify it. A gdb sampling
    /// profile of a hung-looking H2 `TestFileSystem.testConcurrent` run
    /// (two real threads, heavy native-call volume) caught both live
    /// threads inside `hash_byte_pair`/`native_method_hash` disproportionately
    /// often, which is this exact redundant second pass. Only covers the
    /// fast exact-hash path (mirroring `find`'s own fast path); falls back
    /// to the slow `find`+`kind_of` pair on a miss so descriptor-quirk
    /// rewriting keeps working unchanged.
    #[inline]
    pub fn find_with_kind(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<(NativeCallback, NativeKind)> {
        self.find_with_kind_and_id(class_name, method_name, descriptor)
            .map(|(callback, kind, _)| (callback, kind))
    }

    /// [`find_with_kind`](Self::find_with_kind), plus the slot's
    /// [`NativeMethodId`](crate::NativeMethodId) when the exact-hash path
    /// answered (`None` on the descriptor-quirk path, whose kind is not the
    /// slot's either — see below). Same single hash, same answer: a caller that
    /// needs a per-slot fact (the by-name doors' synchronized-method facts)
    /// gets the id without hashing the triple a second time.
    #[inline]
    pub fn find_with_kind_and_id(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<(NativeCallback, NativeKind, Option<NativeMethodId>)> {
        lookup_census::probe(lookup_census::FIND_WITH_KIND);
        // One prefilter for both halves — see `find`.
        let class_state = self.class_prefilter(class_name)?;
        if let Some(idx) =
            self.slot_index_from_state(class_state, class_name, method_name, descriptor)
        {
            if let Some(slot) = self.slots.get(idx as usize) {
                return Some((
                    slot.callback,
                    slot.kind,
                    Some(NativeMethodId::from_u32(idx)),
                ));
            }
        }
        // Cold descriptor-quirk path. Semantics deliberately preserved from the
        // pre-memoization implementation: the kind is looked up with the
        // ORIGINAL (un-rewritten) descriptor, so it misses and falls back to
        // `Bridge`. The slot now carries its true kind and we could report it
        // exactly, but that would change which natives the real-JDK
        // `SyntheticStub` drop applies to on quirky descriptors — a dispatch
        // semantics change, out of scope for a perf change. Left as-is,
        // deliberately.
        //
        // MERGE NOTE (dev @ 6495a191c): dev's concurrent edit here was a pure
        // rustfmt reflow of the `methods` / `category_by_key` probe that this
        // change deletes outright. Its formatting of the `kind_of` chain is
        // carried over; the two-map probe itself is gone.
        let cb = self.find_with_descriptor_quirks(class_name, method_name, descriptor)?;
        let kind = self
            .kind_of(class_name, method_name, descriptor)
            .unwrap_or(NativeKind::Bridge);
        Some((cb, kind, None))
    }

    /// Number of distinct native slots ever allocated. Changes **only** when a
    /// registration introduces a triple the registry has not seen before;
    /// re-registering an existing triple updates its slot in place and leaves
    /// this unchanged.
    ///
    /// This is the invalidation signal for [`NativeCallSite`](crate::NativeCallSite):
    /// a memo — including a memoized *negative* ("no native for this triple") —
    /// is valid exactly as long as the generation it was taken at still holds.
    /// It costs one `u32` load to check, and it is what makes call-site
    /// memoization correct during boot and across the lazy `register_*` passes,
    /// not merely "after the registry stops changing".
    ///
    /// The value is `registry_epoch + slots.len()`, not `slots.len()` alone, so
    /// it also distinguishes *which* registry a memo was taken against — see the
    /// [`registry_epoch`](Self) field doc. Never `0`, so a `NativeCallSite` can
    /// keep using an all-zero word as its "never resolved" sentinel.
    ///
    /// The absolute value is an opaque token: compare it for equality, never
    /// treat it as a count.
    #[inline]
    pub fn generation(&self) -> u32 {
        self.registry_epoch.wrapping_add(self.slots.len() as u32)
    }

    #[inline]
    pub fn mutation_epoch(&self) -> u32 {
        self.registry_epoch
            .wrapping_add(self.slots.len() as u32)
            .wrapping_add(self.mutation_epoch)
    }

    /// Resolve a triple to a stable [`NativeMethodId`](crate::NativeMethodId)
    /// that a call site can cache and redeem later with
    /// [`callback_of`](Self::callback_of) — an array index instead of three
    /// string hashes plus a map probe.
    ///
    /// Semantics are identical to [`find`](Self::find), descriptor-quirk
    /// fallback included: `resolve_id(..).and_then(|id| reg.callback_of(id))`
    /// always equals `find(..)`.
    #[inline]
    pub fn resolve_id(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        // PERF (2026-08-13, netty `io.netty.buffer` throughput). This is the
        // lookup `try_stackless_invoke`'s step 1 goes through — the one on the
        // interpreter's EVERY-invoke path — and it was the only entry point
        // that never consulted the class prefilter. It hashed the full triple,
        // missed, and then entered the descriptor-quirk path, for classes like
        // `io/netty/buffer/AdaptiveByteBuf` that register nothing at all. A
        // flat profile of `AdaptiveByteBufAllocatorTest` put `slot_for_exact`
        // at 8.2% and `resolve_id_with_descriptor_quirks` — the `#[cold]` arm —
        // at another 1.8%, the largest family in the run.
        //
        // Answering from the prefix state also removes the second walk over the
        // class name that `native_method_hash` did.
        lookup_census::probe(lookup_census::RESOLVE_ID);
        let class_state = self.class_prefilter(class_name)?;
        let key = native_method_hash_from(class_state, method_name, descriptor);
        if let Some(idx) = self.slot_index_for_key(key, class_name, method_name, descriptor) {
            return Some(NativeMethodId::from_u32(idx));
        }
        self.resolve_id_with_descriptor_quirks(class_name, method_name, descriptor)
    }

    /// As [`resolve_id`](Self::resolve_id), but with the 128-bit digest already
    /// computed (see [`NativeMethodKey`](crate::NativeMethodKey)) so constant
    /// strings known at class-link time are not re-hashed on every lookup.
    ///
    /// The three strings are still required and still checked: the digest
    /// narrows the search, the names decide the answer. Passing a `key` that
    /// does not correspond to the strings simply misses — it can never return
    /// some other class's native.
    #[inline]
    pub fn resolve_id_by_key(
        &self,
        key: NativeMethodKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        lookup_census::probe(lookup_census::RESOLVE_ID_BY_KEY);
        if let Some(idx) =
            self.slot_index_for_key(key.as_pair(), class_name, method_name, descriptor)
        {
            return Some(NativeMethodId::from_u32(idx));
        }
        // No prefilter here on purpose: the caller already saved the digest,
        // so a class hash would be NEW work on this path, and the quirk arm
        // below opens with its own cheap descriptor precheck.
        self.resolve_id_with_descriptor_quirks(class_name, method_name, descriptor)
    }

    /// Redeem a handle: the callback for `id`, or `None` if the handle does not
    /// belong to this registry. O(1), no hashing, no string comparison — this is
    /// the whole point of the mechanism.
    #[inline]
    pub fn callback_of(&self, id: NativeMethodId) -> Option<NativeCallback> {
        self.slots.get(id.index()).map(|slot| slot.callback)
    }

    /// Every triple whose current registration runs the callback at address
    /// `callback`, as `(class, method, descriptor)`, in slot order. A scan of
    /// the slot table, for cold readers only (interpreter round i2 wave 61,
    /// lane L6: the C JVMTI table's `GetThreadState` asks whether the native
    /// another thread runs stands in for a JDK `native` method).
    pub fn triples_running(&self, callback: usize) -> Vec<(&str, &str, &str)> {
        self.slots
            .iter()
            .filter(|slot| slot.callback as usize == callback) // Cast: fn address
            .filter_map(|slot| self.registrations.get(slot.reg_index as usize))
            .collect()
    }

    /// The category the native behind `id` was registered under.
    #[inline]
    pub fn kind_of_id(&self, id: NativeMethodId) -> Option<NativeKind> {
        self.slots.get(id.index()).map(|slot| slot.kind)
    }

    /// Whether the native behind `id` was registered as a **leaf** — i.e. may
    /// be dispatched through `safe_native_call_leaf` rather than the full
    /// funnel. See [`set_leaf`](Self::set_leaf) for the contract.
    ///
    /// `false` for an unknown handle, which is the safe answer: an unrecognised
    /// id takes the funnel.
    #[inline]
    pub fn is_leaf_id(&self, id: NativeMethodId) -> bool {
        self.slots.get(id.index()).is_some_and(|slot| slot.leaf)
    }

    /// Every registered triple currently claiming leaf status, as
    /// `(class, method, descriptor)`. Cold — for the census and for the test
    /// that pins the leaf set, never for dispatch.
    pub fn leaf_registrations(&self) -> Vec<(&str, &str, &str)> {
        self.slots
            .iter()
            .filter(|slot| slot.leaf)
            .filter_map(|slot| self.registrations.get(slot.reg_index as usize))
            .collect()
    }

    /// Count one dispatch of `id`. Called by every dispatch path **that still
    /// resolves this triple by name or id** immediately before invoking it
    /// (`docs/book/src/user-guide/real-jdk-mode.md` §4).
    ///
    /// # What this counter does NOT see — read before believing a number
    ///
    /// The counter is attached to the *resolution*, not to the call. Every
    /// optimisation this VM has added to the native path since consists of
    /// removing the resolution from the hot path, and each one took the
    /// counter with it. Two families were isolated and causally confirmed on
    /// 2026-08-17 against the release binary built from `783685c34`
    /// (`docs/feature-designs/native-dispatch-architecture.md`):
    ///
    /// * **The interpreter's intrinsic table.** Once an invoke cache installs
    ///   a `CachedInvokeTarget::Intrinsic`, the site holds a raw `fn` pointer
    ///   and this registry is never consulted again. MEASURED: 100,000
    ///   `Math.abs` calls report **1**, and the identical run under
    ///   `CRATONVM_DISABLE_INTRINSICS=1` reports **100,000**. Arm-independent
    ///   — it is just as blind under `--nojit`.
    /// * **The JIT's thin direct-call helpers** (`jit_hashmap_put_direct` and
    ///   its siblings). The compiler emits a direct `CALL` to a VM function
    ///   that open-codes the native's semantics with no `NativeMethodId` in
    ///   scope. MEASURED: 100,000 `HashMap.get` calls report **1,873** in the
    ///   JIT arm and **100,001** under `--nojit`, and the JIT figure does not
    ///   move with the workload size or with `CRATONVM_JIT_THRESHOLD` — it is
    ///   frozen at the count reached before the enclosing loop was compiled.
    ///
    /// So `invocations` is a **lower bound on Java-level calls**, and an exact
    /// count of *registry-resolved dispatches*. A zero is not evidence a body
    /// is dead; a small number is not evidence a body is cold. There is one
    /// configuration in which it is exact for everything measured —
    /// `--nojit` with `CRATONVM_DISABLE_INTRINSICS=1` — and that is the
    /// configuration to take a census in.
    ///
    /// A path that knowingly bypasses this counter must say so once, cold, via
    /// [`mark_invocations_incomplete`](Self::mark_invocations_incomplete), so
    /// the census can label the row instead of the reader having to know this
    /// doc comment exists.
    ///
    /// # Cost
    ///
    /// One bounds-checked index into `slot_invocations` and one **relaxed**
    /// `fetch_add`. MEASURED (`rustc -O`, 12 interleaved rounds, medians, a
    /// 12,011-entry counter vector matching this branch's registration count):
    /// **+9.2 ns/call** against a 1.25 ns bounds-checked-index baseline on a
    /// single hot slot, **+8.4 ns/call** spread over 64 slots. Against the
    /// ~141 ns Rust native-call boundary (`G20-1` §5) that is ~6% and
    /// affordable, which is why it sits here. Against a thin direct-call
    /// helper, whose entire reason to exist is to be cheaper than that
    /// boundary, it is not — which is why those helpers are expected to set
    /// the incomplete bit rather than pay this.
    ///
    /// No allocation, no lock, no hashing, no string comparison —
    /// the caller already holds the `NativeMethodId`, so there is nothing left
    /// to resolve. `&self`, because every dispatch path holds only `&` on the
    /// registry; that is why the counter is an atomic and not a `u64`.
    ///
    /// `Relaxed` is correct and deliberate: the census is a *tally*, read once
    /// at the end of the run, and no other memory is published through it.
    /// Anything stronger would put a fence on the native-dispatch path to buy
    /// an ordering nobody reads.
    ///
    /// An out-of-range `id` — a handle from a different registry, the shape
    /// `callback_of`/`kind_of_id` already answer `None` for — is **ignored**,
    /// never a panic. Dispatch must not be able to abort the VM on a stale
    /// memo.
    ///
    /// The counter is this thread's shard of the slot (see the
    /// `slot_invocations` field). A thread picks its shard once and caches it
    /// in a thread-local, so the cost is one thread-local read on top of the
    /// index and the `fetch_add`. The shards exist to remove cross-thread
    /// cache-line traffic on hot slots that many threads dispatch. They do not
    /// change the cost of an uncontended dispatch.
    #[inline]
    pub fn record_invocation(&self, id: NativeMethodId) {
        if let Some(counter) = self
            .slot_invocations
            .get(invocation_shard())
            .and_then(|shard| shard.get(id.index()))
        {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The sum of `index`'s counter over every shard, or `None` if `index` is
    /// not a slot of this registry. Exact once the dispatching threads are
    /// quiescent. While they run, it is a relaxed snapshot, as the single
    /// counter was.
    pub(in crate::registry) fn invocation_total(&self, index: usize) -> Option<u64> {
        let mut total = 0u64;
        for shard in &self.slot_invocations {
            let count = shard.get(index)?.load(std::sync::atomic::Ordering::Relaxed);
            total = total.wrapping_add(count);
        }
        Some(total)
    }

    /// Dispatches recorded against `id` this run, or `None` if the handle does
    /// not belong to this registry. O(1).
    ///
    /// A **lower bound** on Java-level calls whenever
    /// [`invocations_complete`](Self::invocations_complete) answers
    /// `Some(false)` — see [`record_invocation`](Self::record_invocation) for
    /// the two measured bypass families and for the one configuration in which
    /// this number is exact.
    #[inline]
    pub fn invocations_of_id(&self, id: NativeMethodId) -> Option<u64> {
        self.invocation_total(id.index())
    }

}
