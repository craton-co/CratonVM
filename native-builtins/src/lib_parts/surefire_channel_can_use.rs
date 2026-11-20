#[allow(unused_imports)]
use super::super::*;

#[allow(unused_variables)]
pub fn register_essential_natives_with_shims_b(registry: &mut NativeMethodRegistry, shim_selection: app_shims::ShimSelection) {
    registry.register(
        "java/lang/System",
        "clearProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let key = system_property_check_key(ctx, args)?;
            // Actually REMOVE the key (not soft-clear to ""): after
            // `System.clearProperty`, `System.getProperty` must return null, not
            // "" — keycloak's `${name}` placeholder resolution (and any code that
            // distinguishes unset from empty) depends on the key being absent.
            // Returns the prior value, matching `Hashtable.remove`.
            let result = ctx.remove_system_property(&key);
            // Mirror the removal into the cached singleton's side-table -- see
            // the matching comment in `setProperty` above (SC-web-method-spel RC-A).
            if let Some(props) = crate::lang_system::system_props_singleton(ctx.vm_identity()) {
                crate::properties_sidetable::remove_property_from_sidetable(ctx, props, &key);
                crate::properties_sidetable::remove_property_from_real_map(ctx, props, &key);
            }
            match result {
                Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/wildfly/security/manager/WildFlySecurityManager",
        "getPropertyPrivileged",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_wildfly_security_manager_get_property_privileged, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "setProperties",
        "(Ljava/util/Properties;)V",
        |ctx, args| {
            // The side table FIRST, and the real map only when it is empty.
            // Order matters both ways round: a VM-built `Properties` has a side
            // table and may have no real map, and a `new Properties()` built by
            // real `<init>` has a real map and no side table. Reading the
            // wrong one silently installs an EMPTY property set, which is how
            // `SystemRuntimeObjectSweep`'s `setProperties round trip` row read
            // `null/null/true` under the Phase 3 retirement.
            //
            // `new_singleton` is carried OUT of the snapshot rather than re-read
            // from `args` below (gen r4w2/remap): `snapshot_real_map` re-enters
            // Java, and `args` is the pre-call slice -- after a moving young
            // cycle in the snapshot it names the vacated address, which this
            // native would then install as the `System.getProperties()`
            // singleton. The `&mut` hands back the relocated one.
            let mut new_singleton = match args.first() {
                Some(Value::Object(Some(props))) => Some(*props),
                // `System.setProperties(null)`: HotSpot installs a freshly
                // initialised `Properties` holding the properties the VM
                // booted with (`System.setProperties`: `props = new
                // Properties(); initProperties(props)`), not the previous
                // object and not an empty set (SBR3 `refmisc`).
                _ => crate::lang_system::fresh_boot_properties(ctx),
            };
            // `--compatible` has no real `System.props` to swap: the store is
            // the authority and the singleton is rebuilt from it, so the boot
            // properties go straight into the store.
            let supplied = matches!(args.first(), Some(Value::Object(Some(_))));
            let boot_entries = if new_singleton.is_none() && !supplied {
                crate::lang_system::boot_property_entries(ctx)
            } else {
                Vec::new()
            };
            let entries = match new_singleton.as_mut() {
                Some(props) => {
                    let from_sidetable =
                        crate::properties_sidetable::snapshot_sidetable(ctx, *props);
                    if from_sidetable.is_empty() {
                        crate::properties_sidetable::snapshot_real_map(ctx, props)
                    } else {
                        from_sidetable
                    }
                }
                None => boot_entries,
            };

            // From here on the application owns the property map, so the
            // bootstrap fallback must stop answering for keys it no longer
            // contains -- see [`system_property_fallback`].
            SYSTEM_PROPERTIES_REPLACED.store(true, std::sync::atomic::Ordering::Relaxed);

            // Replace the VM's canonical system-property map, rather than
            // layering the supplied Properties on top of the old map. Keycloak's
            // AbstractConfigurationTest captures a baseline clone and resets via
            // System.setProperties(clone); additive semantics leave stale
            // kc.config.args values visible to later config builds.
            let old_keys: Vec<String> = ctx
                .list_system_properties()
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            for key in old_keys {
                let _ = ctx.remove_system_property(&key);
            }
            for (key, value) in &entries {
                let _ = ctx.set_system_property(key, value);
            }

            if let Some(old) =
                crate::lang_system::replace_system_props_singleton(ctx.vm_identity(), new_singleton)
            {
                crate::properties_sidetable::unmark_system_props(ctx, old);
            }
            if let Some(props) = new_singleton {
                // SBR w6 SYSTEM: `System.getProperty` is the JDK's own body
                // under `--real-jdk` (`props.getProperty(key)`), which reads
                // the `System.props` STATIC. This native used to leave that
                // field on the previous object until the next
                // `System.getProperties()` re-published it, so a
                // `setProperties(p)` followed directly by `getProperty` read
                // the OLD set. Published FIRST, before anything below can
                // allocate. `--compatible` never reads the field (the natives
                // answer from the VM store), so it is not touched there.
                if crate::sysprops_merge::jdk_bodies_own_system_props(&*ctx) {
                    ctx.set_static_field_by_name(
                        "java/lang/System",
                        "props",
                        Value::Object(Some(props)),
                    );
                    // SBR5 sysprops: the merged reader's baseline described the
                    // previous object; the store was just rebuilt from this one.
                    crate::sysprops_merge::rebaseline(ctx.vm_identity());
                }
                crate::properties_sidetable::mark_system_props(ctx, props);
                crate::properties_sidetable::replace_sidetable(ctx, props, &entries);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // The SECOND registration of this triple, and the one that owns the slot.
    // It used to carry its own store-reading body while
    // `lang_system::native_system_line_separator` carried a third answer (the
    // platform default, ignoring the property outright) -- a duplicate pair
    // sitting half-wrong, which is the shape the lane-operations page's
    // `owns_slot` section warns about. One body now, with the measurement on
    // it.
    registry.register(
        "java/lang/System",
        "lineSeparator",
        "()Ljava/lang/String;",
        crate::lang_system::native_system_line_separator, cratonvm_native_api::NativeKind::Bridge,
    );
    // Duration.parse — JDK bytecode uses regex Matcher.start(int) on
    // unmatched groups, which CratonVM's synthetic Matcher rejects with
    // AIOOBE. Stub the parser natively so ISO-8601 inputs like "P3D",
    // "PT5M" round-trip without touching the regex engine.
    registry.register(
        "java/time/Duration",
        "parse",
        "(Ljava/lang/CharSequence;)Ljava/time/Duration;",
        essential_dur_parse, cratonvm_native_api::NativeKind::Bridge,
    );

    // Quarkus LocaleConverter — its convert(String) rejects any tag for
    // which `Locale.forLanguageTag(...).getLanguage()` is empty. In
    // CratonVM's real-JDK mode `Locale.forLanguageTag` doesn't fully
    // populate the Locale (no native CLDR data), so even valid inputs
    // like "en" or the default-expression value "en-" (produced when
    // `${user.country:}` resolves to empty) get rejected with
    // "Unable to resolve locale". Override the converter to construct
    // a Locale via the simpler `new Locale(lang, country)` path so
    // Quarkus's `quarkus.locales` validation passes.
    registry.register(
        "io/quarkus/runtime/configuration/LocaleConverter",
        "convert",
        "(Ljava/lang/String;)Ljava/util/Locale;",
        essential_quarkus_locale_convert, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "io/quarkus/runtime/configuration/LocaleConverter",
        "convert",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        essential_quarkus_locale_convert, cratonvm_native_api::NativeKind::Bridge,
    );

    // CLUSTER ROOT (H4-1, 2026-08-20) — the first note written in this file
    // after contract §8's ban was lifted for wave 2, and it is a "do not start
    // here" rather than a change.
    //
    // This registration is the entry point of the Properties/Hashtable
    // state-ownership cluster `G88-1` §6 stopped on. The object below is a
    // synthetic `Properties` whose inherited `map` ConcurrentHashMap is
    // permanently null, and every `Properties.*` native in
    // `properties_sidetable.rs` exists to make it behave like a Map at all.
    // Retag those `SyntheticStub` and `register_inner`'s `JdkOnly` arm drops
    // them, which reproduces the 2026-07-14 regression on purpose:
    // `InternalError: null property: java.home` from `java.util.Locale
    // .<clinit>`. So the cluster's first move is to make THIS return a real
    // `Properties` — real `<init>`, real `map` — not to move a tag.
    //
    // `G88-1` §6 named contract §8 as the structural blocker on the ground
    // that the larger half of the cluster lives in this file. Measured
    // 2026-08-20: registrations whose TARGET class is `java/util/Properties`
    // or `java/util/Hashtable` in `native-builtins/src/lib.rs` number **zero**
    // — the 67 are in `properties_sidetable.rs` (32), `deprecated_util.rs`,
    // `deprecated_io_util.rs` and `wildfly_naming.rs`. The full table and the
    // rest of the cluster are on `register_properties_sidetable`.
    //
    // 2026-09-09 — THE CLUSTER'S FIRST MOVE, taken under `--real-jdk`. The note
    // above asks for it in these words: *"make THIS return a real `Properties`
    // — real `<init>`, real `map`"*. Strict mode now populates the real
    // `java.util.concurrent.ConcurrentHashMap` in the real `map` field, which is
    // what every JDK 25 `Properties` body reads.
    //
    // The branch is on the REGISTRY's mode and picks a BODY, not a second
    // registration: §1.4's lever is registration, `NativeCallback` is a bare
    // `fn` pointer that captures nothing, and `NativeContext` exposes no policy
    // accessor — deliberately. Same shape as the `Runtime.loadLibrary0` arm in
    // `lang_system.rs`. `--fast-boot` keeps the null `map` it has always had.
    registry.register(
        "java/lang/System",
        "getProperties",
        "()Ljava/util/Properties;",
        if registry.compatibility_mode().is_jdk_only() {
            native_system_get_properties_jdk_only
        } else {
            native_system_get_properties
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Surefire bootstrap: Maven wraps system properties in
    // `org.apache.maven.surefire.booter.PropertiesWrapper` and calls its
    // virtual `getProperty` overloads very early. In some bootstrap runs the
    // wrapper method body cannot be resolved cleanly yet, so provide native
    // fallbacks mirroring `System.getProperty`.
    registry.register(
        "org/apache/maven/surefire/booter/PropertiesWrapper",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_surefire_properties_wrapper_get_property_1, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/PropertiesWrapper",
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_surefire_properties_wrapper_get_property_2, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/PropertiesWrapper",
        "getIntProperty",
        "(Ljava/lang/String;)I",
        native_surefire_properties_wrapper_get_int_property, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/PropertiesWrapper",
        "getBooleanProperty",
        "(Ljava/lang/String;)Z",
        native_surefire_properties_wrapper_get_boolean_property, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/PropertiesWrapper",
        "setAsSystemProperties",
        "()V",
        native_surefire_properties_wrapper_set_as_system_properties, cratonvm_native_api::NativeKind::Bridge,
    );
    // Surefire 3.1.x `LazyLauncher` lazily obtains the real JUnit Platform
    // `Launcher` via `ReflectionUtils.invokeGetter` → `Method.invoke`. When
    // `Method.invoke` mis-boxes a reference return as null, `launcher()`
    // stores a null delegate and `TestPlanScannerFilter.accept` NPEs on
    // `discover`. Prefer the real `launcher()` path, then fall back to
    // `LauncherFactory.create()` (same as the bytecode CNFE fallback).
    registry.register(
        "org/apache/maven/surefire/junitplatform/LazyLauncher",
        "discover",
        "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;",
        native_lazy_launcher_discover, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/junitplatform/TestPlanScannerFilter",
        "accept",
        "(Ljava/lang/Class;)Z",
        native_test_plan_scanner_filter_accept, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/common/junit4/JUnit4Reflector",
        "createDescription",
        "(Ljava/lang/String;)Lorg/junit/runner/Description;",
        native_surefire_junit4_reflector_create_description, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/common/junit4/JUnit4Reflector",
        "createDescription",
        "(Ljava/lang/String;[Ljava/lang/annotation/Annotation;)Lorg/junit/runner/Description;",
        native_surefire_junit4_reflector_create_description, cratonvm_native_api::NativeKind::Bridge,
    );
    // SystemPropertyManager.loadProperties(InputStream) — Surefire reads
    // its provider configuration via `Properties p = new Properties();
    // p.load(stream); for (k : p.stringPropertyNames()) map.put(k,
    // p.getProperty(k));`.  The CHM put/get round-trip through our
    // synthetic interpreter does not always reliably store the entries
    // in the wrapper's `properties` field (CHM-specific layout
    // assumptions our `native_map_put`/`get` don't fully honour).
    // Override the static factory so we drain the InputStream, parse
    // the .properties bytes ourselves, and seed our side-table indexed
    // by the freshly allocated wrapper object — `PropertiesWrapper.
    // getProperty` (above) consults that side-table first.
    registry.register(
        "org/apache/maven/surefire/booter/SystemPropertyManager",
        "loadProperties",
        "(Ljava/io/InputStream;)Lorg/apache/maven/surefire/booter/PropertiesWrapper;",
        native_surefire_system_property_manager_load_properties, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/SystemPropertyManager",
        "setSystemProperties",
        "(Ljava/io/File;)V",
        native_surefire_system_property_manager_set_system_properties, cratonvm_native_api::NativeKind::Bridge,
    );
    // (`ForkedBooter.launchLastDitchDaemonShutdownThread(I)V` used to be
    // registered here too; the identical registration further down in this
    // same function always won, so the duplicate was removed. See it there
    // for the rationale.)
    registry.register(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "lookupDecoderFactory",
        "(Ljava/lang/String;)Lorg/apache/maven/surefire/spi/MasterProcessChannelProcessorFactory;",
        native_surefire_lookup_decoder_factory, cratonvm_native_api::NativeKind::Bridge,
    );
    // Surefire booter opens the fork payload via `new FileInputStream(new
    // File(tmpDir, propsFile))`. JDK `FileInputStream` field layout + our
    // `drain_input_stream` path often yield zero bytes read, leaving the
    // booter with an empty `PropertiesWrapper` and a later NPE. Read the
    // file in Rust and hand back a synthetic `ByteArrayInputStream` that
    // `SystemPropertyManager.loadProperties` drains reliably.
    registry.register(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "createSurefirePropertiesIfFileExists",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/InputStream;",
        native_forkedbooter_create_surefire_properties_if_file_exists, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "run",
        "(Lorg/apache/maven/surefire/booter/ForkedBooter;[Ljava/lang/String;)V",
        native_surefire_forkedbooter_run, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "acknowledgedExit",
        "()V",
        native_surefire_forkedbooter_acknowledged_exit, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "exit",
        "()V",
        native_surefire_forkedbooter_exit1, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "exit",
        "(I)V",
        native_surefire_forkedbooter_exit_code, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "exit1",
        "()V",
        native_surefire_forkedbooter_exit1, cratonvm_native_api::NativeKind::Bridge,
    );
    // Surefire's Java `lookupDecoderFactory()` picks a provider only when
    // `canUse(connectionString)` returns true. On cratonvm bootstrap runs the
    // connection string can be transiently null/partially materialized, which
    // leaves both built-in providers rejected and `channelProcessorFactory`
    // null.
    //
    // That was previously handled by forcing BOTH built-ins to `true`, which
    // also made them accept each other's scheme — two providers claiming the
    // same channel, with only registration order deciding. Answer honestly for
    // a recognised scheme (`pipe://` = legacy, `tcp://` = modern) and keep the
    // accept-anyway behaviour only for the blank/unrecognised case that the
    // workaround actually exists for.
    fn surefire_channel_can_use(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        scheme: &str,
    ) -> MethodCallResult {
        // Instance method: args[0] = this, args[1] = the connection string.
        let config = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let config = config.trim();
        let recognised = config.starts_with("pipe://") || config.starts_with("tcp://");
        let accept = !recognised || config.starts_with(scheme);
        Ok(Some(Value::Int(i32::from(accept))))
    }
    registry.register(
        "org/apache/maven/surefire/booter/spi/LegacyMasterProcessChannelProcessorFactory",
        "canUse",
        "(Ljava/lang/String;)Z",
        |ctx, args| surefire_channel_can_use(ctx, args, "pipe://"), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/maven/surefire/booter/spi/SurefireMasterProcessChannelProcessorFactory",
        "canUse",
        "(Ljava/lang/String;)Z",
        |ctx, args| surefire_channel_can_use(ctx, args, "tcp://"), cratonvm_native_api::NativeKind::Bridge,
    );
    // Surefire diagnostics: ensure bootstrap throwables become visible even
    // when surefire's dump file plumbing is only partially functional.
    registry.register(
        "org/apache/maven/surefire/api/booter/DumpErrorSingleton",
        "dumpException",
        "(Ljava/lang/Throwable;)Ljava/io/File;",
        native_surefire_dump_exception, cratonvm_native_api::NativeKind::Bridge,
    );
    // Surefire command bootstrap uses j.u.c synchronizers in CommandReader
    // initialization paths. Ensure these core concurrency natives are present
    // in minimal real-JDK bootstrap runs before later phase registrars execute.
    //
    // `SyntheticStub`, stated: these are the SAME five callbacks
    // `util_concurrent_ext::register_concurrent_natives` installs, and that
    // registrar states `SyntheticStub` for them. Left on this function's
    // ambient `Bridge` they became a second copy that `--real-jdk` kept —
    // strict mode refuses the stub at the door, so the surviving row was
    // whichever copy was NOT a stub, and `CountDownLatch` went on being served
    // by the synthetic implementation in the mode that exists to remove it.
    // Measured 2026-08-06 by diffing a `--real-jdk` census against a
    // `--fast-boot` one: five triples, plus the two `CyclicBarrier` constructors
    // below. `java.util.concurrent` is pure Java; JDK 25 declares no
    // `ACC_NATIVE` method on either class.
    //
    // Over a real JDK `--compatible` drops these five too since interpreter
    // round i3 wave 71, lane L1 (the `JdkCountDownLatch` row,
    // `CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH`, default on): `register_with_kind`
    // goes through `register`, whose family stub arm refuses a stated
    // `SyntheticStub` on the class exactly as an ambient one.
    let surefire_cdl = "java/util/concurrent/CountDownLatch";
    registry.register(
        surefire_cdl,
        "<init>",
        "(I)V",
        native_cdl_init,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        surefire_cdl,
        "countDown",
        "()V",
        native_cdl_count_down,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        surefire_cdl,
        "await",
        "()V",
        native_cdl_await,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        surefire_cdl,
        "await",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_cdl_await_timeout,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        surefire_cdl,
        "getCount",
        "()J",
        native_cdl_get_count,
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // Keep Semaphore's real JDK constructor and methods when real AQS is in
    // use.  The synthetic representation stores its permit state in an int[]
    // in Semaphore.sync, which is safe only while every Semaphore operation is
    // intercepted.  Libraries such as Oracle UCP call the inherited protected
    // Sync.reducePermits() directly; with a synthetic holder that method sees
    // an int[] instead of Semaphore$Sync and retries its CAS forever.
    let synthetic_aqs = crate::nbflags().synthetic_aqs && !crate::nbflags().real_aqs;
    if synthetic_aqs {
        let surefire_sem = "java/util/concurrent/Semaphore";
        registry.register(surefire_sem, "<init>", "(I)V", native_sem_init, cratonvm_native_api::NativeKind::Bridge);
        registry.register(surefire_sem, "<init>", "(IZ)V", native_sem_init_fair, cratonvm_native_api::NativeKind::Bridge);
        registry.register(surefire_sem, "acquire", "()V", native_sem_acquire, cratonvm_native_api::NativeKind::Bridge);
        registry.register(surefire_sem, "acquire", "(I)V", native_sem_acquire_n, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            surefire_sem,
            "acquireUninterruptibly",
            "()V",
            native_sem_acquire, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            surefire_sem,
            "acquireUninterruptibly",
            "(I)V",
            native_sem_acquire_n, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(surefire_sem, "release", "()V", native_sem_release, cratonvm_native_api::NativeKind::Bridge);
        registry.register(surefire_sem, "release", "(I)V", native_sem_release_n, cratonvm_native_api::NativeKind::Bridge);
        registry.register(surefire_sem, "tryAcquire", "()Z", native_sem_try_acquire, cratonvm_native_api::NativeKind::Bridge);
        registry.register(surefire_sem, "tryAcquire", "(I)Z", native_sem_try_acquire_n, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            surefire_sem,
            "tryAcquire",
            "(JLjava/util/concurrent/TimeUnit;)Z",
            native_sem_try_acquire_timeout, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            surefire_sem,
            "availablePermits",
            "()I",
            native_sem_available_permits, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            surefire_sem,
            "drainPermits",
            "()I",
            native_sem_drain_permits, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(surefire_sem, "isFair", "()Z", native_sem_is_fair, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            surefire_sem,
            "toString",
            "()Ljava/lang/String;",
            native_sem_to_string, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    // Keep CyclicBarrier constructors available this early too; surefire and
    // plugin ecosystems may switch between latch/semaphore/barrier patterns.
    // `SyntheticStub` for the same reason as the latch above.
    //
    // Gated on `synthetic_aqs` for the same reason as Semaphore above, and it
    // matters MORE here: `native_cb_init` parks its int[3] state holder in the
    // receiver's slot 0, which on the real JDK layout is the `lock`
    // ReentrantLock field. Registering only the constructors while `await()`
    // runs real bytecode would hand that bytecode a barrier whose `lock` is an
    // int[] — so these two must live or die with the `await` natives in
    // `util_concurrent_ext::register_concurrent_natives`.
    if synthetic_aqs {
        let surefire_cb = "java/util/concurrent/CyclicBarrier";
        registry.register(
            surefire_cb,
            "<init>",
            "(I)V",
            native_cb_init,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            surefire_cb,
            "<init>",
            "(ILjava/lang/Runnable;)V",
            native_cb_init_action,
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    // If CommandReader.<clinit> still fails, surefire wraps the cause in
    // UnsatisfiedLinkError / ExceptionInInitializerError very early.
    registry.register(
        "java/lang/UnsatisfiedLinkError",
        "<init>",
        "()V",
        native_exception_init_empty, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/UnsatisfiedLinkError",
        "<init>",
        "(Ljava/lang/String;)V",
        native_exception_init_msg, cratonvm_native_api::NativeKind::Bridge,
    );
    // These two OWN their slots -- `--dump-native-registry` says
    // `owns_slot: true` here and `false` for the same descriptors in
    // `lang_misc`'s measured per-class table, so this registrar is the one
    // that decides and the table's rows are inert. They therefore have to
    // carry the class-specific behaviour themselves.
    //
    // `ExceptionInInitializerError()` is `super(); initCause(null)` and
    // `(String)` is `super(s, null)`: both leave `cause` at a REAL null rather
    // than at the `cause == this` sentinel, so a later `initCause` on such an
    // instance is an `IllegalStateException`. The generic bodies below write
    // the sentinel and made it a silent success. MEASURED by
    // `ThrowableFamilySweep.java`; see
    // `lang_misc::native_exc_init_noargs_null_cause` for the disassembly.
    //
    // The `(Throwable)` descriptor has no duplicate here, which is why the
    // table's fix for it took effect and these two did not -- one fix landing
    // for one descriptor and not its neighbour is the signature of a method
    // with more than one registrar.
    registry.register(
        "java/lang/ExceptionInInitializerError",
        "<init>",
        "()V",
        crate::lang_misc::native_exc_init_noargs_null_cause, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/ExceptionInInitializerError",
        "<init>",
        "(Ljava/lang/String;)V",
        crate::lang_misc::native_exc_init_message_null_cause, cratonvm_native_api::NativeKind::Bridge,
    );
    // Surefire abnormal-shutdown path (`ForkedBooter.exit1`) schedules a
    // "last ditch" terminator thread via private method
    // `launchLastDitchDaemonShutdownThread(int)`. In real-JDK mode this path
    // can trip over partially emulated scheduler/thread-factory internals and
    // throw before `System.exit(1)` executes, masking the real test failure
    // with a booter-side NPE.
    //
    // KEEP (deliberate harness hardening, not an unimplemented stub): keep the
    // explicit process exit semantics and skip the auxiliary terminator thread;
    // if `System.exit` is ignored, surefire already treats that as a fork
    // failure. Suppressing the watchdog only costs us a forced kill we do not
    // need, and it stops a booter-side NPE from masking the real test failure.
    registry.register(
        "org/apache/maven/surefire/booter/ForkedBooter",
        "launchLastDitchDaemonShutdownThread",
        "(I)V",
        native_noop, cratonvm_native_api::NativeKind::Bridge,
    );
    // T15: Properties.getProperty overrides — delegate to the VM's
    // system-properties store regardless of the Properties object's state.
    // This lets real-JDK bootstrap code (e.g. `File.<clinit>` calling
    // `System.getProperties().getProperty("file.separator")`) work even
    // before `System.props` is populated.
    //
    // T19_K3_PROPS_LOAD_OVERRIDE: KC26 KeycloakMain.<clinit> reads
    // `Version.VERSION` which is set by `Version.<clinit>` from
    // `Class.getResourceAsStream("/keycloak-version.properties")` →
    // `Properties.load(InputStream)` → `getProperty("version")`.  Our
    // resource lookup serves the bytes correctly; the real JDK's
    // `Properties.load` then runs through `LineReader.readLine` and
    // `loadConvert` and calls `Properties.put(k, v)` on the synthetic
    // Properties object.  The bytecode runs but the underlying Hashtable
    // table-array allocation is incomplete (we observed `Properties.put`
    // firing but `Properties.get` returning null on the same key),
    // probably due to our synthetic-allocated Hashtable having the
    // wrong field shape vs. the real-JDK Hashtable that `put` writes to.
    //
    // Rather than chase the Hashtable layout drift, we register a side-
    // table indexed by the Properties object's identity hash and override
    // `Properties.load(InputStream)`, `Properties.setProperty`,
    // `Properties.put`, `Properties.get`, and the two `Properties.getProperty`
    // overloads to read/write that side-table.  The implementation is in
    // `properties_sidetable.rs` to keep the registration here lean.
    crate::properties_sidetable::register_properties_sidetable(registry);

    // Spring Boot apps (eureka-server, letsgo-main, sportme) hang in
    // `jdk/internal/loader/AbstractClassLoaderValue.putIfAbsent` (pc=29):
    // the method drives `ConcurrentHashMap.putIfAbsent` whose internal CAS
    // loop livelocks under our Unsafe field-offset emulation.  Bypass the
    // CHM path entirely with a side-table keyed by (ClassLoader, this).
    // See `classloader_value_sidetable.rs`.
    crate::classloader_value_sidetable::register_classloader_value_sidetable(registry);

    // NOTE: `Properties.remove(Object)` is now registered by
    // `register_properties_sidetable` as a side-table-aware native that
    // returns the previous value (or null) — see properties_sidetable.rs.
    // Earlier this was an always-null stub here, which broke any caller
    // that legitimately removed an entry it had just `put` (e.g. H2's
    // `ConnectionInfo.removeProperty("USER", "")` left USER in the
    // property map, so every H2 JDBC connect failed with
    // "Unsupported connection setting USER" when Engine.openSession
    // iterated the keys).

    // JDK 25 additional System natives. One function per stream serves the
    // `*0` native and its public wrapper (see `native_system_set_stream`).
    registry.register(
        "java/lang/System",
        "setIn0",
        "(Ljava/io/InputStream;)V",
        native_system_set_in,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "setOut0",
        "(Ljava/io/PrintStream;)V",
        native_system_set_out,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "setErr0",
        "(Ljava/io/PrintStream;)V",
        native_system_set_err,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "setIn",
        "(Ljava/io/InputStream;)V",
        native_system_set_in,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "setOut",
        "(Ljava/io/PrintStream;)V",
        native_system_set_out,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "setErr",
        "(Ljava/io/PrintStream;)V",
        native_system_set_err,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "mapLibraryName",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_system_map_library_name,
        NativeKind::Bridge,
    );

    // System.getenv — needed in both synthetic and real-JDK modes because
    // the real JDK bytecode for getenv() calls ProcessEnvironment which
    // requires native interop we don't support. Our native override
    // delegates directly to cratonvm_types::flags::runtime_var.
    registry.register(
        "java/lang/System",
        "getenv",
        "(Ljava/lang/String;)Ljava/lang/String;",
        lang_system::native_system_getenv, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "getenv",
        "()Ljava/util/Map;",
        lang_system::native_system_getenv_all, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/lang/System", "gc", "()V", |ctx, _args| {
        crate::hooks::jfr_force_gc_as_system_gc(ctx);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // T14: initPhase1/2/3 — only override in synthetic-jdk mode.
    // In real-JDK mode the actual JDK bytecode runs, supported by the
    // T14 SystemProps$Raw / FileDescriptor / Win32ErrorMode natives
    // registered by system_bootstrap::register_t14_system_bootstrap().
    //
    // The comment above states the intent; the `#[cfg(feature =
    // "synthetic-jdk")]` alone does not enforce it — found 2026-09-17,
    // same family as `InputStreamReader`/`CharArrayReader`/etc:
    // `native_system_init_phase1` unconditionally writes `System.out`/`err`
    // static fields, stamps a Charset onto them, and force-initializes the
    // `System` class — real VM-bootstrap side effects that the real
    // `initPhase1` bytecode (backed by the T14 natives this comment names)
    // already performs correctly. A `--features synthetic-jdk` binary run
    // against a real JDK would run BOTH: this native first (registered
    // over the real method, so it runs INSTEAD, not in addition), doing a
    // synthetic-layout bootstrap that the T14 natives were never designed
    // to follow.
    #[cfg(feature = "synthetic-jdk")]
    if !registry.drops_real_layout_synthetic() {
        registry.register(
            "java/lang/System",
            "initPhase1",
            "()V",
            lang_system::native_system_init_phase1, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/lang/System",
            "initPhase2",
            "(ZZ)I",
            lang_system::native_system_init_phase2, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/lang/System",
            "initPhase3",
            "()V",
            lang_system::native_system_init_phase3, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // RKC16N.8 — KC16 boot stubs (real-JDK mode, JDK 25):
    //   - `Class.desiredAssertionStatus()Z` returns false (assertions disabled,
    //     mirroring `-ea` not being on). The pre-existing
    //     `desiredAssertionStatus0(Class)Z` native already returns false; this
    //     adds the public `()Z` form that real JDK 25 calls during boot.
    //   - `System.initPhase1()V` no-op fallback. The real JDK bytecode would
    //     normally run, but on KC16 boot the method resolution does not see
    //     the bytecode (synthetic class shape). The vm-cli explicit invocation
    //     path (vm-cli/src/main.rs around line 766) treats a no-op as "fell
    //     through to synthetic streams" — downstream init levels still bump.
    // Not over a real JDK under `CRATONVM_REFLECTION_JDK_BODIES`
    // (`lang_class::reflection_jdk_bodies`): JDK 25's body asks the loader's
    // assertion table, then the `desiredAssertionStatus0` native.
    if !lang_class::reflection_jdk_bodies(registry) {
        registry.register(
            "java/lang/Class",
            "desiredAssertionStatus",
            "()Z",
            native_assertion_status, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    // S110 — initPhase1 wires up System.out/err synthetic streams *and*
    // System.in (a FileInputStream-shaped object whose slot 0 holds the
    // stdin fd id 0). Without the System.in install, `new Scanner(System.in)`
    // sees a null stream and `nextLine()` / `nextInt()` immediately throw
    // NoSuchElementException. The full init also drops `lineSeparator` into
    // place so `String.format("%n")` matches HotSpot.
    registry.register(
        "java/lang/System",
        "initPhase1",
        "()V",
        if registry.compatibility_mode().is_jdk_only() {
            native_system_init_phase1_jdk_only
        } else {
            native_system_init_phase1_real_selected
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.lang.String (native methods + overrides) ---
    // JDK 9+: String.intern() is the only ACC_NATIVE method in java.lang.String.
    registry.register(
        "java/lang/String",
        "intern",
        "()Ljava/lang/String;",
        lang_string::native_string_intern,
        NativeKind::Bridge,
    );
    // The `AbstractStringBuilder` half of DF05 — see the
    // `(Ljava/lang/StringBuilder;)V` sibling in `deprecated_util.rs` for the
    // full story and the measurement. `Intrinsic` for the same load-bearing
    // reason: this VM's builders are `char[]`-backed and the real ctor's
    // `Arrays.copyOfRange` over a `byte[]` reads them one byte at a time.
    //
    // JIT round 16 (lane strings, STR16-2): that premise holds for the
    // SYNTHETIC image only. Under `--real-jdk` on a real image every builder
    // is the JDK's own compact `byte[]` one, maintained by JDK bytecode, so
    // `CRATONVM_STRING_BUILDER_CTOR_BYTECODE=1` leaves this triple
    // unregistered there and the real constructor runs — see
    // `lang_string::string_builder_ctor_yields_to_bytecode`.
    if !lang_string::string_builder_ctor_yields_to_bytecode(
        registry.compatibility_mode(),
        registry.real_jdk(),
        lang_string::sb_bodies_aligned(registry),
        lang_string::string_builder_ctor_bytecode_flag(),
    ) {
        registry.register(
            "java/lang/String",
            "<init>",
            "(Ljava/lang/AbstractStringBuilder;Ljava/lang/Void;)V",
            lang_string::native_string_init_abstract_string_builder,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
    }
    // String.valueOf and Integer.toString overrides: the JDK bytecode path uses
    // Unsafe.putByte for byte-level array access which doesn't map to our
    // slot-based heap model (Unsafe offsets are raw byte offsets in HotSpot).
    //
    // STR16-7 (round 16 wave 7, lane strings): this `Bridge` row (the ambient
    // `set_category(Bridge)` at the top of this function) was the one
    // registrar of the three the switch did not gate. With the switch on,
    // `lang_math::register_wrapper_natives` (called further down this
    // function) skipped its `Intrinsic` re-registration, so THIS row survived
    // into the `--real-jdk` registry: `String.valueOf(I)` stayed a native, now
    // a `Bridge`, i.e. the opposite of what the switch promises. See
    // `lang_math::strict_int_to_string_bytecode`.
    if !lang_math::strict_int_to_string_bytecode(registry) {
        registry.register(
            "java/lang/String",
            "valueOf",
            "(I)Ljava/lang/String;",
            lang_string::native_string_value_of_int, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    registry.register(
        "java/lang/String",
        "valueOf",
        "(J)Ljava/lang/String;",
        lang_string::native_string_value_of_long, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(Z)Ljava/lang/String;",
        lang_string::native_string_value_of_boolean, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(D)Ljava/lang/String;",
        lang_string::native_string_value_of_double, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(F)Ljava/lang/String;",
        lang_string::native_string_value_of_float, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(C)Ljava/lang/String;",
        lang_string::native_string_value_of_char, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        lang_string::native_string_value_of_object, cratonvm_native_api::NativeKind::Bridge,
    );
    // STR16-7: see `lang_math::strict_int_to_string_bytecode`.
    if !lang_math::strict_int_to_string_bytecode(registry) {
        registry.register(
            "java/lang/Integer",
            "toString",
            "(I)Ljava/lang/String;",
            native_integer_to_string_compat, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    // `Integer.toString(int, int)` is registered ~200 lines below, in this same
    // function. `register` is last-write-wins, so the copy that used to sit
    // here could never be dispatched — and it was the WRONG one: it handled
    // only radix 2/8/10/16 (silently answering base 10 for the other 32 legal
    // radices) and formatted negatives with `format!("{:x}")` on a signed
    // `i32`, which prints the two's-complement bit pattern
    // (`Integer.toString(-1, 16)` -> the JDK says "-1", that body said
    // "ffffffff"). Deleted rather than reordered: reordering leaves two bodies
    // to drift.
    //
    // COUNT THE REGISTRATIONS BEFORE BELIEVING EITHER BODY IS LIVE. There were
    // THREE, not two, and neither of the ones in this file is the one that
    // runs. `lang_math::register_wrapper_natives` is called further down THIS
    // SAME function (search `register_wrapper_natives(registry)`), and it
    // registers `Integer.toString(II)` and `Long.toString(JI)` again — so
    // `lang_math::native_integer_to_string_radix` /
    // `native_long_to_string_radix` win in essential/real-JDK mode, and win
    // again in synthetic mode via `register_synthetic_overrides`. The bodies
    // below are shadowed. They are kept correct (and unit-tested, see
    // `radix_to_string_tests`) so that they are not a landmine if the
    // registration order ever changes, but a fix aimed at the observable
    // behaviour of `Integer.toString(int, int)` has to land in `lang_math.rs`.
    // The four census-visible rows on the box classes were retired here
    // (`Integer.valueOf(String)`, `Integer.toOctalString`,
    // `Integer.toBinaryString`, `Long.toHexString`) -- real JDK 25 bytecode
    // serves all four. Everything still registered on `Integer`/`Long` is
    // `NativeKind::Intrinsic` and census-exempt, so those four were the block.
    //
    // Do NOT extend this to the classes: `RJdkReflBox` asserts reflective
    // boxing IDENTITY with `==`, and the intrinsic `valueOf(I)` is what backs
    // the cache it checks. Retiring `Integer` or `Long` wholesale reddens it.
    // See `WORKER-3-NOTE-7`.
    registry.register(
        "java/lang/Integer",
        "toHexString",
        "(I)Ljava/lang/String;",
        native_integer_to_hex_string_compat, cratonvm_native_api::NativeKind::Bridge,
    );
    // Long.toString overrides (same Unsafe.putByte issue)
    // STR16-7: see `lang_math::strict_int_to_string_bytecode`.
    if !lang_math::strict_int_to_string_bytecode(registry) {
        registry.register(
            "java/lang/Long",
            "toString",
            "(J)Ljava/lang/String;",
            native_long_to_string_compat, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    // Double.toString(D) / Float.toString(F) overrides — JDK 25's
    // `jdk/internal/math/DoubleToDecimal` / `FloatToDecimal` bytecode
    // mis-renders values in our interpreter (Spark
    // `SparkSubmitArguments.loadEnvironmentArguments` fails parsing
    // `spark.driver.memoryOverheadFactor` default 0.1 as
    // `0.?9999999E-18`). Route through `format_double` / `format_float`
    // which produce Java-spec strings ("0.1", "Infinity", "NaN").
    registry.register(
        "java/lang/Double",
        "toString",
        "(D)Ljava/lang/String;",
        native_double_to_string_compat, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Float",
        "toString",
        "(F)Ljava/lang/String;",
        |ctx, args| {
            let v = match args.first() {
                Some(Value::Float(f)) => *f,
                _ => 0.0,
            };
            Ok(Some(Value::Object(Some(
                ctx.create_string(&format_float(v)),
            ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Round 63 (peaceful-sammet) — Long.toString(long,int) and Integer.toString(int,int)
    // overrides for real-JDK mode. SnakeYAML 2.0 SafeConstructor.<clinit> calls
    // Long.toString(Long.MAX_VALUE, radix) for radix in {2,8,10,16}; the JDK
    // bytecode path throws ArrayIndexOutOfBoundsException in our interpreter
    // (the root cause has not been pinpointed, but bypassing via a native
    // implementation unblocks the SafeConstructor clinit so ua_parser.Parser.<init>
    // and DeviceRepresentationProviderFactoryImpl.<clinit> succeed, advancing
    // Keycloak past the YAML init failure).
    registry.register(
        "java/lang/Long",
        "toString",
        "(JI)Ljava/lang/String;",
        |ctx, args| {
            let val = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let radix = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 10,
            };
            let text = java_long_to_string_radix(val, radix);
            Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Integer",
        "toString",
        "(II)Ljava/lang/String;",
        |ctx, args| {
            let val = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let radix = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 10,
            };
            let text = java_int_to_string_radix(val, radix);
            Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Object.toString override — JDK bytecode uses string concat (invokedynamic)
    // which depends on Unsafe byte-level array writes we don't support.
    registry.register(
        "java/lang/Object",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(obj))) => *obj,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Synthetic generic-reflection stubs (GenericArrayType / ParameterizedType
            // / WildcardType — built by `generics::type_sig_to_java`, whose class IS
            // the bare `java.lang.reflect` interface with no bytecode `toString`)
            // resolve `toString` up to this `Object.toString` native and would leak
            // `GenericArrayType@hash`. Emit the canonical `getTypeName()` rendering
            // instead, e.g. `java.util.List<java.lang.String>[]` (Spring
            // `SerializableTypeWrapperTests.genericArrayType()` asserts exactly this).
            if ctx.heap_kind_of(this) != cratonvm_types::ObjectKind::Array {
                let cid = ctx.class_id_of_object(this);
                let is_reflect_type = matches!(
                    ctx.class_name_arc_of_id(cid).as_deref(),
                    Some("java/lang/reflect/GenericArrayType")
                        | Some("java/lang/reflect/ParameterizedType")
                        | Some("java/lang/reflect/WildcardType")
                );
                if is_reflect_type {
                    let s = crate::phases_late::render_type_name(ctx, &Value::Object(Some(this)));
                    // Round 13 wave 9: a fresh String, not the never-pruned pool.
                    let s = crate::lang_math::fresh_computed_string(ctx, &s);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            // Replicate: getClass().getName() + "@" + Integer.toHexString(hashCode())
            //
            // ARRAY receivers MUST render the array-class name ([Ljava.lang.Class;
            // / [I / [[I …), not the header's class id: heap arrays store the
            // COMPONENT class id (and ClassId(0) for primitive arrays), so the
            // naive class_id_of read printed "java.lang.Class@46" where HotSpot
            // prints "[Ljava.lang.Class;@hex". Every default-toString consumer
            // (string concat, String.valueOf, StringBuilder.append, println,
            // String.format %s, collection rendering) dispatches into this native
            // for array receivers (arrays resolve methods on java/lang/Object and
            // native-override priority shadows the real bytecode), so this single
            // formatter caused the SB-04 "scalar Class" misdiagnosis.
            let mut this = this;
            let dot_name = if ctx.heap_kind_of(this) == cratonvm_types::ObjectKind::Array {
                crate::lang_class::array_descriptor_for(ctx, this).class_text(&*ctx)
            } else {
                let mut class_id = ctx.class_id_of_object(this);
                // JIT round 17 wave 8 (lane streams4): the name `getClass()`
                // reports, not the stamp. A synthetic carrier stamped with an
                // interface (`"a\nb".lines()` under `--compatible` is a
                // `java/util/stream/Stream`) printed `java.util.stream.Stream@..`
                // where HotSpot prints `java.util.stream.ReferencePipeline$Head@..`.
                // `native_object_to_string` (the other `Object.toString`
                // registrar) already asks `getclass_display_class_id`; this one,
                // which wins on the `--compatible` boot, did not.
                // `CRATONVM_OBJECT_TOSTRING_DISPLAY_CLASS=0` keeps the stamp.
                // The display lookup can run Java (a collection's `size()`), so
                // `this` is pinned across it for the `hashCode()` call below.
                {
                    let this_pin = ctx.pin_native_root(this);
                    let display = getclass_display_class_id(ctx, class_id, this);
                    this = ctx.read_native_pin(this_pin, this);
                    ctx.unpin_native_roots(this_pin);
                    if let Some(display) = display {
                        class_id = display;
                    }
                }
                // A LAMBDA receiver has no entry in the class store -- its
                // identity lives in `lambda_proxies`, keyed by an id from the
                // reserved `>= 0x8000_0000` range -- so `class_name_of_id`
                // answers `None` here. `unwrap_or_default()` then rendered the
                // EMPTY string and the whole class name vanished, silently:
                //
                //     HotSpot:  Probe$$Lambda/0x0000000086040210@7344699f
                //     CratonVM: @4af
                //
                // and `Class.getName()` was right the whole time, which is what
                // made it read as a formatting quirk rather than a missing case.
                // Anything that prints a lambda -- string concat, `String.valueOf`,
                // `println`, `%s`, a collection's own `toString` -- got a nameless
                // object; Spring's `DefaultRetryPolicy.toString()` is how it
                // surfaced (`core.retry.RetryPolicyTests.predicatesCombined`).
                //
                // `lambda_proxy_class_name` is the SAME helper the reflection
                // name natives use, so `toString` and `getName` cannot drift.
                // Its result is already dotted and carries a `/0x<id>` tail that
                // the `/`->`.` rewrite below would corrupt, so it is used verbatim.
                match crate::lang_class::lambda_proxy_class_name(ctx, class_id) {
                    Some(lambda_name) => lambda_name,
                    // A hidden class's `/0x<hex>` tail is part of its name
                    // (`dotted_binary_name`, as `Class.getName()` answers):
                    // the JDK-spun lambda proxy (`JavaLangAccess.defineClass`,
                    // wave 51) printed `Host$$Lambda.0x...@hash` under
                    // `--compatible`.
                    None => crate::lang_class::dotted_binary_name(
                        &ctx.class_name_of_id(class_id).unwrap_or_default(),
                    ),
                }
            };
            // JDK `Object.toString` is `getName() + "@" + Integer.toHexString(hashCode())`
            // where `hashCode()` is a VIRTUAL call. A receiver that overrides
            // `hashCode()` (in bytecode) must have THAT value rendered here, not the
            // raw identity hash — otherwise a value-hashed object whose default
            // `toString` leaks into an equals/cache key varies spuriously across runs.
            // This broke real-CGLIB cross-context proxy-class caching: Spring's
            // `AnnotationTransactionAttributeSource` (hashCode = annotationParsers
            // .hashCode()) is embedded via `toString()` in the CGLIB `AdvisorKeyEntry`
            // (EnableTransactionManagementTests.cglibProxyClassIsCachedAcrossApplicationContexts).
            // Objects that do NOT override hashCode dispatch to the Object.hashCode
            // native (identity), so behaviour is unchanged for them (incl. arrays).
            let hash = match ctx.invoke_virtual(this, "hashCode", "()I", &[]) {
                Ok(Some(Value::Int(h))) => h,
                _ => ctx.identity_hash_code(this),
            };
            let hex = format!("{:x}", hash as u32);
            let result = format!("{}@{}", dot_name, hex);
            // Round 13 wave 9 (lane bigdec2): one distinct String per object
            // printed went into the never-pruned intern pool; HotSpot returns
            // a fresh one (`CRATONVM_COMPUTED_STRINGS_UNINTERNED`).
            Ok(Some(Value::Object(Some(
                crate::lang_math::fresh_computed_string(ctx, &result),
            ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Object.equals — reference identity for ordinary objects (matching the
    // default JDK contract), PLUS structural equality for the synthetic
    // generic-reflection stubs (GenericArrayType/ParameterizedType/WildcardType).
    // Those stubs have no bytecode `equals`, so the default identity comparison
    // wrongly reports a deserialized `GenericArrayType` as != its source
    // (spring-bug-08 SerializableTypeWrapperTests.genericArrayType(): the
    // toString matches but `isEqualTo` fails). Compare by canonical
    // `getTypeName()` rendering — two reflection types are equal iff their
    // rendered type names are equal. Real-JDK mode otherwise has no Object.equals
    // native (it runs the identity bytecode); registering one here is behaviour-
    // identical for non-reflection receivers.
    registry.register(
        "java/lang/Object",
        "equals",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                Some(Value::Object(None)) | None => return Ok(Some(Value::Int(0))),
                _ => return Ok(Some(Value::Int(0))),
            };
            let other = args.get(1).copied().unwrap_or(Value::Object(None));
            // Identity fast path (the default Object.equals contract).
            if let Value::Object(Some(bo)) = other {
                if this.as_ptr() == bo.as_ptr() {
                    return Ok(Some(Value::Int(1)));
                }
            }
            let cid = ctx.class_id_of_object(this);
            let is_reflect = matches!(
                ctx.class_name_arc_of_id(cid).as_deref(),
                Some("java/lang/reflect/GenericArrayType")
                    | Some("java/lang/reflect/ParameterizedType")
                    | Some("java/lang/reflect/TypeVariable")
                    | Some("java/lang/reflect/WildcardType")
            );
            if is_reflect {
                // Structural: equal iff the other is a non-null reference whose
                // canonical type-name rendering matches this one's.
                if matches!(other, Value::Object(Some(_))) {
                    let ra = crate::phases_late::render_type_name(ctx, &Value::Object(Some(this)));
                    let rb = crate::phases_late::render_type_name(ctx, &other);
                    return Ok(Some(Value::Int(if ra == rb { 1 } else { 0 })));
                }
                return Ok(Some(Value::Int(0)));
            }
            // Default: reference identity (the refs are known non-identical here).
            Ok(Some(Value::Int(0)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- jdk/internal/util/Preconditions overrides ---
    // These honour the `BiFunction` exception formatter every four-argument
    // overload takes, which is the whole reason those overloads exist:
    // `String` passes `SIOOBE_FORMATTER` and is entitled to a
    // `StringIndexOutOfBoundsException`, `Objects.check*` passes `null` and is
    // entitled to a plain `IndexOutOfBoundsException`. Throwing
    // `ArrayIndexOutOfBoundsException` for both — as this block used to —
    // breaks `catch` in two different directions. `preconditions.rs` has the
    // full contract, including why it must NOT suppress `<clinit>`.
    preconditions::register(registry);

    // --- jdk/internal/util/ByteArray — a REVIEWED `Intrinsic` (SBR w1, 2026-09-28) ---
    //
    // # Why an `Intrinsic` may win over real bytecode here (AGENTS.md)
    //
    // AGENTS.md: concrete Java bytecode beats a registered native, "only a
    // reviewed `Intrinsic` may win". An `Intrinsic` is the one exception
    // because it gives the SAME answer as the bytecode, faster. These twenty
    // bodies were a `Bridge` that gave DIFFERENT answers, and SBR wave 1 retired
    // them (`RETIRED_SHADOW_SBR_BYTES_TRIPLES`, measured against HotSpot 25 with
    // `SbrW1Bytes.java`): no bounds check, no null check, and
    // `setDouble`/`setFloat` stored raw NaN bits. The real bodies — a
    // `byteArrayViewVarHandle` access behind a Java frame — then cost
    // 1.6-2.6x on `DataInputStream`/`DataOutputStream` and 1.15-1.7x on an
    // `ObjectStream` round trip (`SbrW1BytesBench.java`). So the
    // bodies below are rewritten to the JDK's own semantics, line by line, and
    // tagged `Intrinsic` so they keep winning at every door:
    //
    // * **Bounds** — the multi-byte accessors are `VarHandle` views, whose check
    //   is `Preconditions.checkIndex(offset, array.length - (width - 1),
    //   AIOOBE_FORMATTER)`: `ArrayIndexOutOfBoundsException("Index <offset> out
    //   of bounds for length <array.length - width + 1>")`, the length NOT
    //   clamped (HotSpot prints `length -1` for an 8-byte read of a 6-byte
    //   array). `getBoolean`/`setBoolean` are a plain `array[offset]`, so their
    //   length is the array's own.
    // * **Null** — checked BEFORE the bounds (the view reads `ba.length` first).
    //   The message is HotSpot's helpful-NPE text for the JDK's own bytecode:
    //   `Cannot read the array length because "ba" is null` for the views,
    //   `Cannot load from / store to byte/boolean array because "array" is null`
    //   for the boolean pair.
    // * **NaN** — `setDouble`/`setFloat` store `doubleToLongBits` /
    //   `floatToIntBits`, collapsing every NaN to the canonical one; the `*Raw`
    //   pair and every getter move the bits unchanged.
    // * **Order** — big-endian, as `ByteArray` is (`ByteArrayLittleEndian` has no
    //   native and runs its own bytecode).
    //
    // `SbrW1Bytes.java` diffs every one of these against HotSpot,
    // messages included, cold and in a hot loop. Stated explicitly with
    // `with_category(Intrinsic)` rather than inherited, and restored on exit.
    {
        let ba = "jdk/internal/util/ByteArray";
        const NPE_VIEW: &str = "Cannot read the array length because \"ba\" is null";

        /// `(array, offset)` for a `width`-byte view access, or the exception
        /// the JDK's `VarHandle` view throws. `width == 1` is the boolean pair's
        /// plain array access.
        fn ba_target(
            ctx: &dyn NativeContext,
            args: &[Value],
            width: usize,
            store: bool,
        ) -> Result<(ObjectRef, usize), MethodCallFailed> {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    let message = match (width, store) {
                        (1, false) => {
                            "Cannot load from byte/boolean array because \"array\" is null"
                        }
                        (1, true) => "Cannot store to byte/boolean array because \"array\" is null",
                        _ => NPE_VIEW,
                    };
                    return Err(RuntimeError::NullPointerException {
                        message: Some(message.to_string()),
                    }
                    .into());
                }
            };
            let off = match args.get(1) {
                Some(Value::Int(i)) => *i,
                _ => 0,
            };
            let limit = ctx.array_length(arr) as i64 - (width as i64 - 1);
            if off < 0 || i64::from(off) >= limit {
                return Err(RuntimeError::aioobe(off, limit as i32).into());
            }
            Ok((arr, off as usize))
        }
        /// Big-endian read of `N` bytes as an unsigned 64-bit pattern.
        fn ba_get<const N: usize>(
            ctx: &dyn NativeContext,
            args: &[Value],
        ) -> Result<u64, MethodCallFailed> {
            let (arr, off) = ba_target(ctx, args, N, false)?;
            let mut buf = [0u8; N];
            ctx.read_byte_array_into(arr, off, &mut buf);
            Ok(buf.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
        }
        /// Big-endian write of the low `N` bytes of `raw`.
        fn ba_set<const N: usize>(
            ctx: &mut dyn NativeContext,
            args: &[Value],
            raw: u64,
        ) -> MethodCallResult {
            let (arr, off) = ba_target(&*ctx, args, N, true)?;
            let mut buf = [0u8; N];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (raw >> (8 * (N - 1 - i))) as u8;
            }
            ctx.write_byte_array_from(arr, off, &buf);
            Ok(None)
        }
        fn arg_int(args: &[Value]) -> i32 {
            match args.get(2) {
                Some(Value::Int(i)) => *i,
                _ => 0,
            }
        }
        fn arg_long(args: &[Value]) -> i64 {
            match args.get(2) {
                Some(Value::Long(l)) => *l,
                _ => 0,
            }
        }
        fn arg_float(args: &[Value]) -> f32 {
            match args.get(2) {
                Some(Value::Float(f)) => *f,
                _ => 0.0,
            }
        }
        fn arg_double(args: &[Value]) -> f64 {
            match args.get(2) {
                Some(Value::Double(d)) => *d,
                _ => 0.0,
            }
        }
        /// `Float.floatToIntBits`: every NaN is `0x7fc00000`.
        fn float_to_int_bits(f: f32) -> u32 {
            if f.is_nan() {
                0x7fc0_0000
            } else {
                f.to_bits()
            }
        }
        /// `Double.doubleToLongBits`: every NaN is `0x7ff8000000000000`.
        fn double_to_long_bits(d: f64) -> u64 {
            if d.is_nan() {
                0x7ff8_0000_0000_0000
            } else {
                d.to_bits()
            }
        }

        registry.register(ba, "getBoolean", "([BI)Z", |ctx, args| {
            let v = ba_get::<1>(ctx, args)?;
            Ok(Some(Value::Int(i32::from(v != 0))))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "getChar", "([BI)C", |ctx, args| {
            Ok(Some(Value::Int(ba_get::<2>(ctx, args)? as u16 as i32)))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "getShort", "([BI)S", |ctx, args| {
            Ok(Some(Value::Int(
                ba_get::<2>(ctx, args)? as u16 as i16 as i32
            )))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "getUnsignedShort", "([BI)I", |ctx, args| {
            Ok(Some(Value::Int(ba_get::<2>(ctx, args)? as u16 as i32)))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "getInt", "([BI)I", |ctx, args| {
            Ok(Some(Value::Int(ba_get::<4>(ctx, args)? as u32 as i32)))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        // `getFloat` is `Float.intBitsToFloat(INT.get(..))` and `getFloatRaw` is
        // `FLOAT.get(..)`: both move the bits unchanged on a read.
        registry.register(ba, "getFloat", "([BI)F", |ctx, args| {
            Ok(Some(Value::Float(f32::from_bits(
                ba_get::<4>(ctx, args)? as u32
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "getFloatRaw", "([BI)F", |ctx, args| {
            Ok(Some(Value::Float(f32::from_bits(
                ba_get::<4>(ctx, args)? as u32
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "getLong", "([BI)J", |ctx, args| {
            Ok(Some(Value::Long(ba_get::<8>(ctx, args)? as i64)))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "getDouble", "([BI)D", |ctx, args| {
            Ok(Some(Value::Double(f64::from_bits(ba_get::<8>(ctx, args)?))))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "getDoubleRaw", "([BI)D", |ctx, args| {
            Ok(Some(Value::Double(f64::from_bits(ba_get::<8>(ctx, args)?))))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "setBoolean", "([BIZ)V", |ctx, args| {
            ba_set::<1>(ctx, args, u64::from(arg_int(args) != 0))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "setChar", "([BIC)V", |ctx, args| {
            ba_set::<2>(ctx, args, u64::from(arg_int(args) as u16))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "setShort", "([BIS)V", |ctx, args| {
            ba_set::<2>(ctx, args, u64::from(arg_int(args) as u16))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        // `setUnsignedShort(array, offset, int value)` is `SHORT.set(.., (short) value)`:
        // the low sixteen bits, whatever the int holds above them.
        registry.register(ba, "setUnsignedShort", "([BII)V", |ctx, args| {
            ba_set::<2>(ctx, args, u64::from(arg_int(args) as u16))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "setInt", "([BII)V", |ctx, args| {
            ba_set::<4>(ctx, args, u64::from(arg_int(args) as u32))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "setFloat", "([BIF)V", |ctx, args| {
            ba_set::<4>(ctx, args, u64::from(float_to_int_bits(arg_float(args))))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "setFloatRaw", "([BIF)V", |ctx, args| {
            ba_set::<4>(
                ctx,
                args,
                u64::from(crate::lang_math::java_float_to_int_bits(arg_float(args))),
            )
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "setLong", "([BIJ)V", |ctx, args| {
            ba_set::<8>(ctx, args, arg_long(args) as u64)
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "setDouble", "([BID)V", |ctx, args| {
            ba_set::<8>(ctx, args, double_to_long_bits(arg_double(args)))
        }, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(ba, "setDoubleRaw", "([BID)V", |ctx, args| {
            ba_set::<8>(ctx, args, arg_double(args).to_bits())
        }, cratonvm_native_api::NativeKind::Intrinsic);
    };

    // --- java.lang.Class (native methods) ---
    // JNI symbol binding only — see java/lang/Object.registerNatives above.
    registry.register(
        "java/lang/Class",
        "registerNatives",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getPrimitiveClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_class_get_primitive_class,
        NativeKind::Bridge,
    );

    // WildFly livelock fix — override the three `java.lang.Class$Atomic`
    // CAS helpers so they bypass the broken Unsafe.objectFieldOffset
    // lookup for Class.{reflectionData,annotationType,annotationData}
    // and operate on a Rust-side per-mirror table instead. Without
    // these overrides, `Class.privateGetDeclaredFields` livelocks on
    // bootstrap (observed under WildFlySecurityManager.<clinit>).
    registry.register(
        "java/lang/Class$Atomic",
        "casReflectionData",
        "(Ljava/lang/Class;Ljava/lang/ref/SoftReference;Ljava/lang/ref/SoftReference;)Z",
        native_class_atomic_cas_reflection_data, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class$Atomic",
        "casAnnotationType",
        "(Ljava/lang/Class;Lsun/reflect/annotation/AnnotationType;Lsun/reflect/annotation/AnnotationType;)Z",
        native_class_atomic_cas_annotation_type, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class$Atomic",
        "casAnnotationData",
        "(Ljava/lang/Class;Ljava/lang/Class$AnnotationData;Ljava/lang/Class$AnnotationData;)Z",
        native_class_atomic_cas_annotation_data, cratonvm_native_api::NativeKind::Bridge,
    );

    // T20.RD (WildFly/SportMe Class$ReflectionData livelock fix) —
    // The cas-side-store fix above (`casReflectionData`) keeps CAS
    // progress consistent, but it is INVISIBLE to the direct
    // `getfield Class.reflectionData` performed by `Class.reflectionData()`
    // and `Class.newReflectionData()` (slot 11 on the mirror, populated
    // by `make_class_mirror` to `null`). After the first CAS succeeds
    // the side store holds a SoftReference, but the heap slot stays
    // null; the second call therefore re-enters `newReflectionData()`
    // whose `while(true)` loop CASes with `expected=null` against a
    // side-store value of `Some(prevSoftRef)`, the comparison fails
    // forever, and `re-read this.reflectionData` returns null again
    // → livelock at `Class$ReflectionData.<init>` / `newReflectionData`
    // pc=0..9 (60s watchdog dump on WildFly bootstrap and SportMe).
    //
    // Cleanest fix: replace `Class.newReflectionData(SoftReference, int)`
    // with a native that allocates the `Class$ReflectionData`, sets its
    // `redefinedCount`, wraps it in a fresh `SoftReference`, writes the
    // SoftReference into slot 11 of the receiver mirror AND into the
    // cas side store so both the direct getfield and any future
    // `casReflectionData` see the same value. Also register the
    // `Class$ReflectionData.<init>(I)V` constructor as a no-op-ish
    // native that just assigns `redefinedCount` (the JDK constructor
    // body is `this.redefinedCount = redefinedCount;`) — required so
    // that any caller that allocates a ReflectionData directly (the
    // bytecode at `newReflectionData` pc=5) does not run through
    // class-init / JIT paths that may themselves be hung.
    registry.register(
        "java/lang/Class$ReflectionData",
        "<init>",
        "(I)V",
        native_class_reflection_data_init, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "newReflectionData",
        "(Ljava/lang/ref/SoftReference;I)Ljava/lang/Class$ReflectionData;",
        native_class_new_reflection_data, cratonvm_native_api::NativeKind::Bridge,
    );
    // Override the getter too so the read path consults the GC-safe cas side
    // store instead of the dangling heap slot 11 (see
    // `native_class_reflection_data`). Without this, the slot-11 SoftReference is
    // reclaimed by a young GC (untracked old→young edge) and the reflection path
    // faults on an all-zero `SoftReference` (Tomcat DoHead start/stop flood).
    registry.register(
        "java/lang/Class",
        "reflectionData",
        "()Ljava/lang/Class$ReflectionData;",
        native_class_reflection_data, cratonvm_native_api::NativeKind::Bridge,
    );

    // T14/T15: Override Class.getResourceAsStream / getResource so that
    // real-JDK bytecode doesn't depend on the full Module subsystem during
    // bootstrap.  The real JDK's versions call getModule().isNamed() first,
    // but our Class mirrors have no populated module field, which would
    // cause an NPE before we even reach the ClassLoader delegation.  Both
    // overrides simply delegate to the class's ClassLoader (or the system
    // loader if none), which is what the non-module fallback does anyway.
    //
    // Merge of dev into interpreter round i5 (2026-10-16): gci i4/jd made these two
    // reviewed `Intrinsic`s under `--compatible` (`java.sql.Connection.class.getResourceAsStream`
    // read null on warm calls: the JDK body asked `BootLoader` for `java.sql`). On this tree the
    // JDK body already answers it (waves 95-100, lane L3: `BootLoader` / `BuiltinClassLoader`
    // `findResource*` are `--compatible` Intrinsics over the image; `GciJm2ImageReadProbe 3`
    // SAME on the pre-merge build), and the natives miss the body's module rules, so seven L3
    // resource probes (`L3W100ModuleInfoByModule`, `L3W96PlatformModuleResource` ...) went DIFF.
    // The body keeps winning in both modes, as before.
    registry.register(
        "java/lang/Class",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        lang_class::native_class_get_resource_as_stream, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        lang_class::native_class_get_resource, cratonvm_native_api::NativeKind::Bridge,
    );
    // Class.getModule returns a non-null synthetic Module mirror.  Older
    // versions returned null, which broke any downstream `Module.isNamed()`,
    // `Module.canUse(svc)`, or `Module.canRead(other)` callsite in JDK
    // bytecode that immediately dereferences the returned module without
    // a null check (e.g. `ServiceLoader.checkCaller` does
    // `caller.getModule().canUse(svc)` directly, NPE'ing inside
    // `Console.<clinit>` and tripping ByteBuddy/CGLib bootstrap).
    //
    // Returning a Module whose `name` is `module_name_of_class` (on the
    // synthetic shape: slot 0, with the layer slot 1 unset; on the real
    // layout: the `name` field, slot 0 being `layer`) is safe because:
    //   * `Module.canUse(Class)` is overridden as a native that always
    //     returns true on a non-null receiver (see this file's
    //     S109 Wave3 registration).
    //   * `Module.isNamed()` consults field 0 — non-null name yields
    //     true for named modules, false for the unnamed module.
    //   * Every other `Module.*` Java method that bytecode reaches
    //     during boot is either overridden in `phases_late.rs::register_p59_module`
    //     (synthetic mode) or guarded by `canUse`/`canRead` first.
    //
    // This intentionally mirrors the synthetic-mode override at
    // `phases_late.rs::register_p59_module` so real-JDK and synthetic-jdk
    // boot paths see the same Module shape — see vm/tests/wave3_console_module.rs.
    // REVIEWED `Intrinsic`, 2026-09-10, and the review is
    // `ClassModuleSweep.java`.
    //
    // This is a §1.4 shadow by the letter and NOT one in substance, which is
    // the case §1.4's reviewed-`Intrinsic` exception exists for.
    // `Class.getModule()` is not `ACC_NATIVE` -- its body is `return module;`
    // -- so a native in front of it shadows real bytecode and the contract's
    // remedy is to yield. **That remedy cannot work here.**
    // `java.lang.Class.module` is `private transient Module` and NO JAVA CODE
    // WRITES IT: a real JVM populates it at class-definition time through
    // `Module.defineModule0`. Yielding returns null, and a null Module is what
    // 12 of the 108 remaining failures under
    // `CRATONVM_ENFORCE_NATIVE_SHADOW=all` were -- a null `module`,
    // `callerModule` or `thisModule` one or two frames later, through
    // `ClassLoader.postDefineClass` -> `NamedPackage.<init>`.
    //
    // The tag was `Bridge` with `kind_stated: false` -- ambient, never chosen
    // (see the module header on `NativeKind` being ambient). So this states a
    // decision rather than overturning one.
    //
    // An `Intrinsic` claims SEMANTICS-PRESERVING, and that claim is earned
    // rather than asserted. `ClassModuleSweep` is 32 rows against HotSpot
    // 25.0.3+9 -- module names for `java.base`, a platform module, the unnamed
    // module, primitives, arrays of both, nested/anonymous/lambda classes;
    // Module identity WITHIN one VM, which the JDK depends on because `Module`
    // does not override `equals`; and `isNamed`/`getName`/`getClassLoader`/
    // `getDescriptor`/`isOpen`/`isExported`/`canRead`/`getLayer`.
    //
    //     31 of 32 rows byte-identical to HotSpot.
    //
    // The ONE deviation is recorded and NOT fixed here:
    //
    //     32 layer of unnamed is null    HotSpot true, this VM false
    //
    // `Module.getLayer()` on the UNNAMED module should be null and is not. That
    // is a defect in `Module.getLayer`, not in `getModule`, and tagging this
    // triple does not freeze it -- the sweep is checked in, so the row goes red
    // the day it is fixed or the day this answer drifts.
    //
    // What this does NOT license: the rest of the `java/lang/Module` surface
    // stays `Bridge`. The claim here is about ONE triple whose backing field no
    // Java code can fill.
    /// `CRATONVM_MODULE_MIRROR_LAYER_SLOT_GUARD` (default on, both modes): the
    /// Module mirror `Class.getModule()` builds carries its name in slot 0 only
    /// on the synthetic `java/lang/Module` shape, never in the real layout's
    /// `layer` slot (round 13 wave 11, lane misc11). `0` restores the wave-10
    /// dual write.
    fn module_mirror_layer_slot_guard_enabled() -> bool {
        static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        match SLOT.load() {
            1 => false,
            2 => true,
            _ => {
                let on = true;
                SLOT.publish(if on { 2 } else { 1 });
                on
            }
        }
    }
    registry.register(
        "java/lang/Class",
        "getModule",
        "()Ljava/lang/Module;",
        |ctx, args| {
            if let Some(Value::Object(Some(mirror))) = args.first() {
                if let Some(slot) = class_module_slot(ctx) {
                    if let Value::Object(Some(m)) = ctx.get_field(*mirror, slot) {
                        return Ok(Some(Value::Object(Some(m))));
                    }
                }
            }
            // Resolve the module name of the class the receiver mirror
            // *represents* so we can return the canonical (cached) Module for
            // that module. The JDK compares Modules by identity (`Module` does
            // not override `equals`), so every class in a module MUST observe
            // the SAME Module object. Allocating a fresh Module per call broke
            // `Throwable.validateSuppressedExceptionsList`
            // (`Object.class.getModule() == deserList.getClass().getModule()`),
            // which throws `StreamCorruptedException("List implementation not in
            // base module.")` on any ObjectInputStream round-trip of an object
            // holding a java.util List — HIB-CV-29.
            //
            // CRITICAL: the receiver is a `java.lang.Class` *mirror*; its own
            // object class is always `java/lang/Class` (java.base). We must read
            // the module of the class it REFLECTS via `class_id_from_mirror`
            // (the mirror→ClassId reverse map), NOT `class_id_of_object` (which
            // would return `java/lang/Class` and report every class as java.base
            // — the original defect). Fall back to the object class only if the
            // reverse lookup misses (defensive; e.g. primitive mirrors).
            let module_name: Option<String> =
                if let Some(Value::Object(Some(mirror))) = args.first() {
                    let class_id = ctx
                        .class_id_from_mirror(*mirror)
                        .unwrap_or_else(|| ctx.class_id_of_object(*mirror));
                    // A lambda proxy is a hidden class defined into its host's
                    // module and package (HotSpot: `Comparator$$Lambda`
                    // answers `java.base`). The VM mints it without a module
                    // label, so it answered the unnamed module, which made
                    // `ResourceBundle.getBundle` called from `ResourcesMgr`'s
                    // `ResourceBundle::getBundle` method reference fail with
                    // "unnamed module can't load sun.security.util.resources".
                    let class_id = ctx.lambda_proxy_host_id(class_id).unwrap_or(class_id);
                    ctx.module_name_of_class(class_id)
                } else {
                    None
                };

            // A modular jar reached through `-cp` is an UNNAMED-module citizen:
            // a real JVM ignores its `module-info` outright, and `getModule()`
            // answers the loader's unnamed module. This VM's registry scans the
            // application class path for `module-info.class` and registers what
            // it finds (so readability checks pass for e.g. `org.jboss.logging`),
            // which made this accessor report `ch.qos.logback.classic` for a
            // class-path class.
            //
            // MEASURED 2026-08-28, `probes/L7ModuleProbe`, both modes:
            //
            //   ch.qos.logback.classic.spi.LogbackServiceProvider
            //     HotSpot   isNamed=false  name=null
            //     CratonVM  isNamed=true   name=ch.qos.logback.classic
            //
            // It is not a labelling nicety. `ServiceLoader`'s classpath lookup
            // iterator does `if (clazz.getModule().isNamed()) continue;` — a
            // SILENT skip, no error and no report row — so every classpath SPI
            // provider disappeared. Under `--real-jdk`, where the ServiceLoader
            // synthetic stubs are refused and the real bytecode runs, that made
            // SLF4J bind `NOPLoggerFactory` and every Spring Boot application
            // die in `LogbackLoggingSystem.beforeInitialize`.
            //
            // Same rule, same predicate, as `populate_boot_layer_modules` and
            // `ModuleRegistry::providers_for_service`; this is its fourth door.
            // A `--module-path` module is re-registered `automatic = false` by
            // `vm_init` immediately after `ClassManager::new`, so it is NOT
            // class-path-only and is unaffected — which is what
            // `regression-suite/src/RJdkModule.java` pins.
            let module_name = module_name.filter(|n| !ctx.module_is_class_path_only(n));

            // Round 13 wave 9 (lane proxy6): a generated proxy of all-public
            // interfaces lives in its loader's dynamic module `jdk.proxyN`
            // (`Proxy.ProxyBuilder.getDynamicModule`), not in the unnamed
            // module. Only this answer changes: linkage and access control
            // still treat the class as an unnamed-module citizen.
            // `CRATONVM_PROXY_DYNAMIC_MODULE=0` answers the unnamed module.
            let proxy_module = if module_name.is_none() {
                match args.first() {
                    Some(Value::Object(Some(mirror))) => {
                        ctx.class_id_from_mirror(*mirror).and_then(|cid| {
                            crate::reflect_annotations::proxy_dynamic_module_of(&*ctx, cid)
                                .map(|(module, package)| (cid, module, package))
                        })
                    }
                    _ => None,
                }
            } else {
                None
            };
            // zs s5/nd5: a proxy class of a USER loader answers the dynamic
            // module its loader's `ProxyBuilder` defined -- the JDK's own
            // `Module`, recorded per loader by `Module.defineModule0`
            // (`layer_module_of_class`) -- as HotSpot does (one object:
            // `proxyClass.getModule() == ProxyBuilder`'s module). This native
            // minted a look-alike instead and cached it BY NAME in the VM's
            // permanent module table (`cache_module_mirror` below, a strong
            // root): through its `loader` field every such loader, its classes
            // and their mirrors stayed alive for the life of the VM, ~6.5 KB
            // per `Proxy.getProxyClass(new ClassLoader(){}, ..)`
            // (`Zs3Nd3UpcallMints proxy`: the heap filled after ~690 calls).
            // `CRATONVM_PROXY_MODULE_PER_LOADER=0` restores the by-name mint.
            let proxy_user_loader = match (&proxy_module, args.first().copied()) {
                (Some(_), Some(Value::Object(Some(_)))) if proxy_module_per_loader_enabled() => {
                    match crate::lang_class::native_class_get_class_loader(ctx, args) {
                        Ok(Some(Value::Object(Some(loader))))
                            if classloader::is_user_defined_loader(ctx, loader) =>
                        {
                            Some(loader)
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            if let (Some(loader), Some(Value::Object(Some(mirror)))) =
                (proxy_user_loader, args.first().copied())
            {
                if ctx.has_layer_module_packages() {
                    if let Some(m) =
                        crate::jboss_jdkspecific::layer_module_of_class(ctx, loader, mirror)
                    {
                        return Ok(Some(Value::Object(Some(m))));
                    }
                }
            }
            // The loader's namespace, taken now (the loader is not read after
            // the allocations below).
            let proxy_user_ns =
                proxy_user_loader.map(|loader| classloader::loader_namespace_id(ctx, loader));
            let module_name = match &proxy_module {
                Some((_, module, _)) => Some(module.clone()),
                None => module_name,
            };

            // Unnamed module (everything on the application classpath): route
            // through the shared builder so this mirror is identical to the one
            // `ClassLoader.getUnnamedModule()` and the `java.lang.Package`
            // builders hand out, AND carries a non-null `loader` field so real
            // `Module.getClassLoader()` bytecode answers the AppClassLoader like
            // HotSpot instead of null. See `lang_class::canonical_unnamed_module`.
            if module_name.is_none() {
                // ... but the unnamed module is PER-LOADER in HotSpot, and
                // caller-sensitive JDK APIs derive a search ClassLoader from
                // it (`ResourceBundle.getBundle(String)` above all). Route a
                // class defined by a user loader to THAT loader's unnamed
                // module; built-in loaders keep the single canonical one.
                // Reuse the authoritative `Class.getClassLoader()` resolution
                // rather than a second, narrower side-table probe -- the two
                // must never disagree about which loader owns a class.
                let loader = match crate::lang_class::native_class_get_class_loader(ctx, args) {
                    Ok(Some(Value::Object(o))) => o,
                    _ => None,
                };
                // `--real-jdk`: a class a user-defined loader defined in a
                // package a module of a non-boot `ModuleLayer` holds for that
                // loader is a member of THAT module (HotSpot files the package
                // at `Module.defineModule0`; interpreter round i1 wave 43, lane
                // L5). One flag load for every other class.
                if ctx.has_layer_module_packages() {
                    if let (Some(loader), Some(Value::Object(Some(mirror)))) =
                        (loader, args.first().copied())
                    {
                        if let Some(m) =
                            crate::jboss_jdkspecific::layer_module_of_class(ctx, loader, mirror)
                        {
                            return Ok(Some(Value::Object(Some(m))));
                        }
                    }
                }
                let m = crate::lang_class::unnamed_module_for_loader(ctx, loader)?;
                return Ok(Some(Value::Object(Some(m))));
            }

            // SBR2 wave 7 (module), `--real-jdk`: the JDK's own
            // `Module.canRead`/`isExported`/`isOpen` read the `reads`/
            // `exportedPackages`/`openPackages` tables, which the boot layer's
            // population fills for every registered module (qualified targets
            // need every mirror). So the first mirror of a REGISTERED module asks
            // for the layer, whose population caches them all; the cache hit
            // below then answers. A failed build changes nothing.
            if ctx.is_jdk_only()
                && proxy_module.is_none()
                && module_name
                    .as_deref()
                    .is_some_and(|n| ctx.module_is_registered(n))
                && ctx.get_cached_module_mirror(module_name.as_deref()).is_none()
                // Not from inside `ClassLoaders.<clinit>`: the layer would be
                // built against a look-alike platform loader (page
                // `internal/fixed-bugs/i3-init-finalizer-before-phase1-drops-module-service-providers-FIXED-20261009.md`).
                && !classloader::builtin_loader_adoption_in_progress()
            {
                let _ = jboss_jdkspecific::build_boot_layer(ctx);
            }
            // Canonical-cache hit: hand back the existing Module mirror so
            // identity comparisons across classes in the same module succeed.
            if let Some(cached) = ctx.get_cached_module_mirror(module_name.as_deref()) {
                return Ok(Some(Value::Object(Some(cached))));
            }

            // 5, not 2: `class_manager.rs:15407` declares the synthetic
            // `java/lang/Module` as `instance_fields(5)` and
            // `jboss_jdkspecific.rs:214` allocates `MODULE_FIELD_COUNT = 5`.
            // The object was 5 slots wide either way (`num_fields.max(real)`);
            // asking 2 only fired `report_layout_alias(.., 2, 5)` on every
            // uncached call. The synthetic-mode twin of this triple
            // (`phases_late/reflect_invoke.rs`) now asks 5 as well — see
            // docs/feature-designs/native-dispatch-architecture.md §1.2.
            let m_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Module", 5)?;
            // GC-safety: `create_string` below allocates (String + char[]) and
            // can trigger a moving GC. `m_obj` lives only in this Rust local —
            // not a GC root — so without pinning it would be relocated/reclaimed
            // and returned STALE, resolving to a reused slot (observed as
            // getModule() handing back a String → `String.isNamed()`
            // NoSuchMethodError during WildFly ResourceBundle.checkNamedModule).
            // Pin across the allocation and read the forwarded ref back. Same
            // bug class as reference_classloader_gc_root_gap.
            let pin = ctx.pin_native_root(m_obj);
            let module_name_val = module_name
                .as_deref()
                .map(|name| Value::Object(Some(ctx.create_string(name))))
                .unwrap_or(Value::Object(None));
            let m_obj = ctx.read_native_pin(pin, m_obj);
            // Dual-write the name. Field index 0 is the synthetic Module
            // contract (the `register_p59_module` getName/isNamed/toString natives
            // read slot 0). But real `java.lang.Module.isNamed()`/`getName()`
            // bytecode reads the named `name` field (slot 1 in the real layout —
            // slot 0 is `layer`); without also writing it by name, real
            // `isNamed()` reads the unset `name` field and reports a named
            // platform module (e.g. java.base) as UNNAMED. `set_field_by_name`
            // no-ops when the field is absent (synthetic shape), so this is safe
            // in both modes.
            //
            // Round 13 wave 10 (lane misc10): NOT for the dynamic proxy module on
            // the real layout, where slot 0 IS `layer`: the name String parked
            // there made `getLayer()` answer it (non-null; HotSpot: null).
            //
            // Round 13 wave 11 (lane misc11): nor for any other module on the
            // real layout. The String in `layer` made `getLayer()` of every named
            // mirror built here answer a `String` typed as `ModuleLayer`, and
            // `build_module` / `native_module_get_layer` never replaced it (they
            // seed only a null `layer`). Slot 0 is written only on the synthetic
            // shape, which has no `layer` field and is the one p59's natives and
            // the readers' slot-0 fallbacks read; every reader of the real shape
            // asks the `name` field first. `CRATONVM_MODULE_MIRROR_LAYER_SLOT_GUARD=0`
            // restores the wave-10 rule (dual write for all but the proxy module).
            let layer_slot = ctx.resolve_field_index("java/lang/Module", "layer");
            let write_slot0 = if module_mirror_layer_slot_guard_enabled() {
                layer_slot.is_none()
            } else {
                !(proxy_module.is_some()
                    && module_name.as_deref().is_some_and(
                        crate::reflect_annotations::proxy_module_answers_from_descriptor,
                    )
                    && layer_slot == Some(0))
            };
            if write_slot0 {
                ctx.set_field(m_obj, 0, module_name_val);
            }
            ctx.set_field_by_name(m_obj, "name", module_name_val);
            if let Some(name) = module_name.as_deref() {
                // A dynamic proxy module is not in the registry, so the
                // registry-built descriptor would be the all-empty shape; build
                // `getDynamicModule`'s instead (round 13 wave 10, lane misc10;
                // `CRATONVM_PROXY_MODULE_DESCRIPTOR`).
                let proxy_desc = if proxy_module.is_some() {
                    crate::reflect_annotations::proxy_dynamic_module_descriptor(ctx, name)
                } else {
                    None
                };
                let desc = match proxy_desc {
                    Some(desc) => desc,
                    None => build_synthetic_module_descriptor(ctx, name)?,
                };
                let m_obj = ctx.read_native_pin(pin, m_obj);
                ctx.set_field_by_name(m_obj, "descriptor", Value::Object(Some(desc)));
            }
            // `Module.getClassLoader()` is real bytecode `getfield loader`, and
            // the JDK keeps a module's answer in step with the answer every
            // class in it gives: `java.sql.Connection.getClassLoader()` and
            // `java.sql`'s own are BOTH the platform loader. A boot module's is
            // `null`, which an unwritten field already reports, so only the
            // platform side needs a write here.
            if let Some(module_name) = module_name.as_deref() {
                if let Some(platform) =
                    crate::classloader::platform_loader_for_module(ctx, module_name)
                {
                    let m_obj = ctx.read_native_pin(pin, m_obj);
                    ctx.set_field_by_name(m_obj, "loader", Value::Object(Some(platform)));
                }
                // SBR wave 5 (reflmeta): the real `enableNativeAccess` field,
                // as `ModuleBootstrap.boot` leaves it for this module (see
                // `lang_class::module_starts_with_native_access`), so the real
                // `Reflection.ensureNativeAccess` treats a JDK module's
                // restricted call as HotSpot does. `--real-jdk` only.
                if lang_class::module_starts_with_native_access(ctx, module_name) {
                    let m_obj = ctx.read_native_pin(pin, m_obj);
                    ctx.set_field_by_name(m_obj, "enableNativeAccess", Value::Int(1));
                }
            }
            // The dynamic proxy module is defined to the proxy's own loader,
            // exports `jdk.proxyN` unconditionally and opens it and the
            // non-exported `com.sun.proxy.jdk.proxyN` to `java.base` only
            // (`getDynamicModule`), whichever of the two the class is in. The
            // mirror is re-read from the class id: the allocations above can
            // move it.
            if let Some((proxy_cid, module, _package)) = proxy_module.as_ref() {
                let mirror_now = ctx.get_class_mirror(*proxy_cid);
                let loader = match crate::lang_class::native_class_get_class_loader(
                    ctx,
                    &[Value::Object(Some(mirror_now))],
                ) {
                    Ok(Some(Value::Object(o))) => o,
                    _ => None,
                };
                let m_obj = ctx.read_native_pin(pin, m_obj);
                ctx.set_field_by_name(m_obj, "loader", Value::Object(loader));
                let exported = module.replace('.', "/");
                let non_exported = format!("com/sun/proxy/{exported}");
                ctx.module_add_exports(module, &exported, "");
                ctx.module_add_opens(module, &exported, "java.base");
                ctx.module_add_opens(module, &non_exported, "java.base");
                // SBR2 wave 7 (module), `--real-jdk`: the same edges in the JDK's
                // own tables, which the retired `canRead`/`isExported`/`isOpen`
                // bodies read (this mirror is not made by `defineModules`).
                //
                // JIT round 20, lane dispatch: and `--compatible` over a real
                // JDK with the `JdkModuleAccess` row (SBR round 5, on by
                // default), which drops the same `Module` natives, so the JDK's
                // own `canRead` runs there too. Gated on `is_jdk_only()` alone,
                // the proxy module had no read edge to its interfaces' module
                // under `--compatible`: `InvocationHandler.invokeDefault` ->
                // `Proxy.defaultMethodHandle` -> `findSpecial` failed
                // `checkSymbolicClass` ("symbolic reference class is not
                // accessible: interface X, from class jdk.proxy2.$Proxy3")
                // where HotSpot calls the default method
                // (`R20DispProxyAnnotation`, `proxy-sum`). `aligned` answers
                // `true` under `--real-jdk`, so that mode is unchanged.
                if ctx.aligned(cratonvm_types::HotSpotAlignment::JdkModuleAccess) {
                    let mirror_now = ctx.get_class_mirror(*proxy_cid);
                    let m_obj = ctx.read_native_pin(pin, m_obj);
                    let non_exported_dotted = format!("com.sun.proxy.{module}");
                    jboss_jdkspecific::fill_proxy_module_tables(
                        ctx,
                        m_obj,
                        module,
                        &non_exported_dotted,
                        mirror_now,
                    );
                }
            }
            let m_obj = ctx.read_native_pin(pin, m_obj);
            ctx.unpin_native_roots(pin);
            // Publish as the canonical mirror for this module name so future
            // getModule() calls (and the JDK's identity comparisons) see the
            // same instance. The VM registers it as a permanent GC root.
            //
            // Keep named synthetic Modules' real `descriptor` field non-null.
            // Public access checks are still native-backed, but JDK private
            // helpers and module-definition code read `this.descriptor`
            // directly before calling ModuleDescriptor accessors.
            // zs s5/nd5: a user loader's proxy module minted here (no
            // `defineModule0` record) is filed per loader -- for its package --
            // where the lookup above finds it again and where it lives as long
            // as the loader (`record_layer_module_packages`), never in the
            // by-name table.
            match (&proxy_module, proxy_user_ns) {
                (Some((_, _, package)), Some(ns)) => {
                    ctx.record_layer_module_packages(ns, &[package.clone()], m_obj);
                    if let Some(Value::Object(Some(mirror))) = args.first() {
                        if let Some(slot) = class_module_slot(ctx) {
                            ctx.set_field(*mirror, slot, Value::Object(Some(m_obj)));
                        }
                    }
                    return Ok(Some(Value::Object(Some(m_obj))));
                }
                _ => ctx.cache_module_mirror(module_name.as_deref(), m_obj),
            }
            if let Some(Value::Object(Some(mirror))) = args.first() {
                if let Some(slot) = class_module_slot(ctx) {
                    ctx.set_field(*mirror, slot, Value::Object(Some(m_obj)));
                }
            }
            Ok(Some(Value::Object(Some(m_obj))))
        },
        NativeKind::Intrinsic,
    );

    // `java.lang.Module` access checks — registry-backed exports/opens modeling.
    //
    // `getModule()` above returns named Module mirrors (so `getName()`/identity
    // match the real JDK) but does NOT populate the Java-side `descriptor` field,
    // so the real `Module.isExported`/`isOpen` bytecode dereferences a null
    // `descriptor` and NPEs in every reflective access check (Groovy
    // `CachedClass.getMethods` → `ReflectionUtils.checkCanSetAccessible` →
    // `Java9.checkAccessible` → `Module.implIsExportedOrOpen`, and Hibernate's
    // `JdbcTypeNameMapper.<clinit>` reflecting over `java.sql.Types`). These
    // natives are force-listed over the real bytecode (see
    // `force_native_over_real_jdk_bytecode`) so the NPE never fires.
    //
    // Crucially they answer from the boot `ModuleRegistry`, which carries the
    // accurate per-module exports/opens parsed from each module-info (java.base
    // exports `java.lang`/`java.util`/… to all but NOT `jdk.internal.*`/`sun.*`;
    // application-classpath jars register as automatic modules that export every
    // package). A blanket-`true` answer also removes the NPE but regresses
    // ByteBuddy's `JavaDispatcher`, which queries
    // `java.base.isExported("jdk.internal.…")` and needs the real-JDK `false` to
    // select its fallback strategy. The receiver's module name lives at synthetic
    // slot 0 (dual-written by `getModule()` above); an unnamed module (null name →
    // empty string) exports and opens everything, matching the JDK's
    // `if (!isNamed()) return true;` short-circuit. The Java API uses dot-format
    // package names; the registry is keyed on slash-format.
    fn module_name_of_mirror(ctx: &dyn NativeContext, m: ObjectRef) -> String {
        // Real-bytecode `Module.isNamed()`/`getName()` read the real `name`
        // field, which is what actually determines named-ness for a Module
        // built through any path other than this file's `Class.getModule()`
        // override (e.g. one that dual-writes only the real field, not the
        // legacy synthetic slot 0). Check it FIRST so isExported/isOpen/
        // getDescriptor agree with `isNamed()` — otherwise a Module whose
        // slot 0 was never populated reads back as unnamed here even though
        // `isNamed()` (and everyone else) sees it as named, which silently
        // over-permits isExported/isOpen and NPEs getDescriptor's callers
        // (observed via Elasticsearch's `ProviderLocator.checkUses`, whose
        // `caller.getModule()` mirror had a null slot 0 but a populated real
        // `name` field).
        if let Value::Object(Some(s)) = ctx.get_field_by_name(m, "name") {
            return ctx.read_string(s).unwrap_or_default();
        }
        match ctx.get_field(m, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(), // unnamed module
        }
    }
    fn module_pkg_arg(ctx: &dyn NativeContext, args: &[Value]) -> Option<String> {
        match args.get(1) {
            Some(Value::Object(Some(s))) => {
                Some(ctx.read_string(*s).unwrap_or_default().replace('.', "/"))
            }
            _ => None,
        }
    }
    /// JDK 25 `Module.implIsExportedOrOpen`, which all four `isExported` /
    /// `isOpen` forms run: a NAMED module answers `false` for a package it
    /// does not contain, whatever else holds -- the "exported/open to self"
    /// rule and the open/automatic-module rule both test
    /// `descriptor.packages().contains(pn)`, and a declared or reflective
    /// export can only name one of its own packages. The registry answered
    /// `true` there (`java.base.isExported("no.such.pkg", java.base)`, any
    /// package of an open module). Interpreter round i5 wave 106, lane L5;
    /// probe `L5W106ModuleExportsForeignPackage`. Lenient (`false` = not
    /// refused) when the registry records no package of the module at all.
    /// The owner lookup answers the common case (a class's own package) with
    /// one map read; only a foreign package reaches the package scan.
    fn named_module_lacks_package(ctx: &dyn NativeContext, module_name: &str, pkg: &str) -> bool {
        if module_name.is_empty() {
            return false;
        }
        if ctx.module_for_package(pkg).as_deref() == Some(module_name) {
            return false;
        }
        let packages = ctx.module_packages(module_name);
        !packages.is_empty() && !packages.iter().any(|p| p == pkg)
    }
    /// For a TARGET module the registry cannot name (an unnamed module: a
    /// loader's own): the JDK's record of a reflective export or
    /// opening, `Module.isReflectivelyExportedOrOpened(pn, other, open)`, which
    /// reads the `ReflectionData.exports` map `implAddExportsOrOpens` fills
    /// (`Instrumentation.redefineModule`, `Module.addOpens`). The registry is
    /// keyed by module NAME, so an opening to one loader's unnamed module was
    /// invisible here and `isOpen` answered `false` where HotSpot answers
    /// `true` (interpreter round i1 wave 31,
    /// `interpreter-L6-module-opens-to-an-unnamed-module-and-java-lang-lookup-defines-FIXED-20261011`,
    /// item 1). Asked only after the registry said no; a failure of the call
    /// keeps that answer. Both modes since wave 47 (lane L6).
    fn module_reflectively_exported_or_open(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        to_module: &str,
        open: bool,
    ) -> bool {
        if !to_module.is_empty() {
            return false;
        }
        let (Some(Value::Object(Some(this))), Some(pn), Some(other @ Value::Object(Some(_)))) = (
            args.first().copied(),
            args.get(1).copied(),
            args.get(2).copied(),
        ) else {
            return false;
        };
        // The JDK's method is `isReflectivelyExportedOrOpened` (a private
        // instance method, so invoked on its exact class). Until wave 43
        // (lane L5) this asked `isReflectivelyExportedOrOpen`, a name no JDK
        // declares (`javap -p java.lang.Module`, JDK 25.0.3): the call always
        // failed, and this fallback always kept the registry's `false`.
        matches!(
            ctx.invoke(
                "java/lang/Module",
                "isReflectivelyExportedOrOpened",
                "(Ljava/lang/String;Ljava/lang/Module;Z)Z",
                &[Value::Object(Some(this)), pn, other, Value::Int(open as i32)],
            ),
            Ok(Some(Value::Int(v))) if v != 0
        )
    }
    /// For an unnamed TARGET module (`to_module` empty; both modes since wave
    /// 47): the
    /// registry's edge to that module by its loader
    /// (`phases_late::unnamed_module_edge_target`), which
    /// `Instrumentation.redefineModule` / `Module.addOpens` to a loader's
    /// unnamed module now records (interpreter round i1 wave 46, lane L5). An
    /// opening is an export too, as the JDK's `implIsExportedOrOpen` answers.
    fn edge_to_unnamed_of_loader(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        module_name: &str,
        pkg: &str,
        to_module: &str,
        open: bool,
    ) -> bool {
        if !to_module.is_empty() {
            return false;
        }
        let Some(Value::Object(Some(other))) = args.get(2).copied() else {
            return false;
        };
        let Some(target) = crate::phases_late::unnamed_module_edge_target(ctx, other) else {
            return false;
        };
        ctx.is_package_open_to(module_name, pkg, &target)
            || (!open && ctx.is_package_exported_to(module_name, pkg, &target))
    }
    /// `--real-jdk`: a named module of a user `ModuleLayer` answers from the
    /// JDK's own `implIsExportedOrOpen` (its fields and `ReflectionData`); the
    /// name-keyed registry knows no such module
    /// (`jboss_jdkspecific::is_user_layer_module`; interpreter round i1 wave
    /// 43, lane L5). `None`: not such a receiver, or a null argument the
    /// registry path answers as before.
    fn user_layer_exported_or_open(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        qualified: bool,
        open: bool,
    ) -> Option<MethodCallResult> {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return None;
        };
        if !crate::jboss_jdkspecific::is_user_layer_module(ctx, this) {
            return None;
        }
        let pn @ Value::Object(Some(_)) = args.get(1).copied()? else {
            return None;
        };
        let other = if qualified {
            match args.get(2).copied() {
                Some(Value::Object(Some(o))) => Some(o),
                _ => return None,
            }
        } else {
            None
        };
        crate::jboss_jdkspecific::user_layer_module_is_exported_or_open(ctx, this, pn, other, open)
    }
    /// `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` (default off; interpreter
    /// round i6 wave 122, lane L5,
    /// `i45-L5-proposal-one-module-access-question-by-identity`): a NAMED
    /// name-map receiver asked about a TARGET module of a user `ModuleLayer`
    /// (`isExported(pn, other)` / `isOpen(pn, other)`) answers by the target's
    /// identity: a descriptor's qualified `exports p to m` reaches the boot
    /// layer's `m` only, never a layer module that shares the name (the JDK
    /// resolves the target in the module's own configuration). The name-keyed
    /// `is_package_exported_to` admitted it. `None`: switch off, an unnamed
    /// receiver, a target that is not such a module, or no registry; the
    /// caller answers as before. Probe `L5W122LayerTargetQualifiedExport`.
    fn layer_target_by_identity(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        module_name: &str,
        pkg: &str,
        open: bool,
    ) -> Option<bool> {
        if !cratonvm_types::flags().loader.module_access_export_one_question
            || module_name.is_empty()
        {
            return None;
        }
        let Some(Value::Object(Some(other))) = args.get(2).copied() else {
            return None;
        };
        if !crate::jboss_jdkspecific::is_user_layer_module(ctx, other) {
            return None;
        }
        // Wave 125: the target's loader namespace too, so a run-time edge
        // recorded to another layer's (or the boot layer's) module of the
        // same name does not reach `other`.
        let target_ns =
            crate::jboss_jdkspecific::user_layer_module_identity(ctx, other).map(|(ns, _)| ns);
        let target = module_name_of_mirror(ctx, other);
        let answer = ctx.is_package_exported_or_open_to_layer_module(
            module_name,
            pkg,
            &target,
            target_ns,
            open,
        )?;
        // A registry `no` still asks the JDK's own record of a run-time edge
        // (`ReflectionData.exports`, keyed by the `Module` pair, so by
        // identity): `implAddOpens(pn, other)` tells the VM `addExports0` only,
        // so the registry holds no opens edge for it, and HotSpot's
        // `isOpen(pn, other)` answers `true` from that record (wave 125).
        Some(answer || module_reflectively_exported_or_open(ctx, args, "", open))
    }
    // Module.isExported(String) — exported unqualified (to EVERYONE).
    registry.register(
        "java/lang/Module",
        "isExported",
        "(Ljava/lang/String;)Z",
        |ctx, args| {
            if let Some(answer) = user_layer_exported_or_open(ctx, args, false, false) {
                return answer;
            }
            let this = obj_arg(args, 0)?;
            let module_name = module_name_of_mirror(ctx, this);
            let Some(pkg) = module_pkg_arg(ctx, args) else {
                // `Objects.requireNonNull(pn)` in the JDK body: a bare NPE, not `false`.
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            };
            if named_module_lacks_package(ctx, &module_name, &pkg) {
                return Ok(Some(Value::Int(0)));
            }
            let ok = ctx.is_package_exported_unqualified(&module_name, &pkg);
            Ok(Some(Value::Int(ok as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Module.isExported(String, Module) — exported to the given module.
    registry.register(
        "java/lang/Module",
        "isExported",
        "(Ljava/lang/String;Ljava/lang/Module;)Z",
        |ctx, args| {
            if let Some(answer) = user_layer_exported_or_open(ctx, args, true, false) {
                return answer;
            }
            let this = obj_arg(args, 0)?;
            let module_name = module_name_of_mirror(ctx, this);
            let Some(pkg) = module_pkg_arg(ctx, args) else {
                // `Objects.requireNonNull(pn)` in the JDK body: a bare NPE, not `false`.
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            };
            // `Objects.requireNonNull(other)` of the JDK body, after the
            // package's: a null target is a bare NPE, not `false` (an
            // unnamed module is a `Module` object, never null).
            if module_arg_is_null(args, 2) {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            }
            let to_module = match args.get(2) {
                Some(Value::Object(Some(o))) => module_name_of_mirror(ctx, *o),
                _ => String::new(),
            };
            if named_module_lacks_package(ctx, &module_name, &pkg) {
                return Ok(Some(Value::Int(0)));
            }
            if let Some(ok) = layer_target_by_identity(ctx, args, &module_name, &pkg, false) {
                return Ok(Some(Value::Int(ok as i32)));
            }
            let ok = ctx.is_package_exported_to(&module_name, &pkg, &to_module)
                || edge_to_unnamed_of_loader(ctx, args, &module_name, &pkg, &to_module, false)
                || module_reflectively_exported_or_open(ctx, args, &to_module, false);
            Ok(Some(Value::Int(ok as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Module.isOpen(String) — opened unqualified (to EVERYONE).
    registry.register(
        "java/lang/Module",
        "isOpen",
        "(Ljava/lang/String;)Z",
        |ctx, args| {
            if let Some(answer) = user_layer_exported_or_open(ctx, args, false, true) {
                return answer;
            }
            let this = obj_arg(args, 0)?;
            let module_name = module_name_of_mirror(ctx, this);
            let Some(pkg) = module_pkg_arg(ctx, args) else {
                // `Objects.requireNonNull(pn)` in the JDK body: a bare NPE, not `false`.
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            };
            if named_module_lacks_package(ctx, &module_name, &pkg) {
                return Ok(Some(Value::Int(0)));
            }
            let ok = ctx.is_package_open_unqualified(&module_name, &pkg);
            Ok(Some(Value::Int(ok as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Module.isOpen(String, Module) — opened to the given module.
    registry.register(
        "java/lang/Module",
        "isOpen",
        "(Ljava/lang/String;Ljava/lang/Module;)Z",
        |ctx, args| {
            if let Some(answer) = user_layer_exported_or_open(ctx, args, true, true) {
                return answer;
            }
            let this = obj_arg(args, 0)?;
            let module_name = module_name_of_mirror(ctx, this);
            let Some(pkg) = module_pkg_arg(ctx, args) else {
                // `Objects.requireNonNull(pn)` in the JDK body: a bare NPE, not `false`.
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            };
            // `Objects.requireNonNull(other)` of the JDK body, after the
            // package's: a null target is a bare NPE, not `false` (an
            // unnamed module is a `Module` object, never null).
            if module_arg_is_null(args, 2) {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            }
            let to_module = match args.get(2) {
                Some(Value::Object(Some(o))) => module_name_of_mirror(ctx, *o),
                _ => String::new(),
            };
            if named_module_lacks_package(ctx, &module_name, &pkg) {
                return Ok(Some(Value::Int(0)));
            }
            if let Some(ok) = layer_target_by_identity(ctx, args, &module_name, &pkg, true) {
                return Ok(Some(Value::Int(ok as i32)));
            }
            let ok = ctx.is_package_open_to(&module_name, &pkg, &to_module)
                || edge_to_unnamed_of_loader(ctx, args, &module_name, &pkg, &to_module, true)
                || module_reflectively_exported_or_open(ctx, args, &to_module, true);
            Ok(Some(Value::Int(ok as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Module.getDescriptor() → ModuleDescriptor
    //
    // Real HotSpot: `descriptor` is null iff the module is unnamed — a named
    // module is always constructed with a non-null descriptor. `getModule()`
    // above returns a named Module mirror with a NULL `descriptor` field (see
    // its comment above), so the real bytecode's `return this.descriptor;`
    // NPEs any caller that only calls `getDescriptor()` after checking
    // `isNamed()` — e.g. Elasticsearch's `ProviderLocator.checkUses`:
    // `caller.isNamed() && caller.getDescriptor().uses()...`, which breaks
    // `XContentProvider$Holder`'s static init and cascades into thousands of
    // Elasticsearch suite failures via `NoClassDefFoundError`. Force-listed
    // in `force_native_over_real_jdk_bytecode` alongside isExported/isOpen
    // above. Answers from the same boot `ModuleRegistry` (accurate `uses`
    // parsed from each module-info), dual-written onto the real `name`/`uses`
    // fields (via `set_field_by_name`) so any *other*, non-forced
    // `ModuleDescriptor` accessor called on this same instance still sees
    // correct data.
    registry.register(
        "java/lang/Module",
        "getDescriptor",
        "()Ljava/lang/module/ModuleDescriptor;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(desc)) = ctx.get_field_by_name(this, "descriptor") {
                return Ok(Some(Value::Object(Some(desc))));
            }
            let module_name = module_name_of_mirror(ctx, this);
            if module_name.is_empty() {
                // Unnamed module: matches real Module.getDescriptor()'s null.
                return Ok(Some(Value::Object(None)));
            }
            let desc = build_synthetic_module_descriptor(ctx, &module_name)?;
            ctx.set_field_by_name(this, "descriptor", Value::Object(Some(desc)));
            Ok(Some(Value::Object(Some(desc))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `static native void addReads0(Module from, Module to)` — the VM-sync hook
    // behind `Module.implAddReads` (reached via `Instrumentation.redefineModule`).
    // Pure native (no JDK bytecode), so it MUST be registered or the call NSME/
    // UnsatisfiedLinkErrors. Mockito's inline mock maker drives it from
    // `InlineBytecodeGenerator.assureCanReadMockito`: it makes `java.base` read
    // the (unnamed) Mockito module so the redefined bootstrap classes can see the
    // injected `MockMethodDispatcher`. Java-side `implAddReads` already updates
    // the heap `reads` set; here we mirror the edge into the boot `ModuleRegistry`
    // so a later registry-backed `canRead` agrees. `from`/`to` may be the unnamed
    // module (null name → empty string), which `module_add_reads` accepts. Without
    // this the whole inline-mock path aborts before `retransformClasses` even runs.
    registry.register(
        "java/lang/Module",
        "addReads0",
        "(Ljava/lang/Module;Ljava/lang/Module;)V",
        |ctx, args| {
            // `--real-jdk`: a layer module's read is recorded by its identity
            // (interpreter round i1 wave 45, lane L5).
            if let Some(Value::Object(Some(from))) = args.first() {
                let to = match args.get(1) {
                    Some(Value::Object(Some(m))) => Some(*m),
                    _ => None,
                };
                if crate::jboss_jdkspecific::layer_module_add_read(ctx, *from, to) {
                    return Ok(None);
                }
            }
            let from = match args.first() {
                Some(Value::Object(Some(m))) => module_name_of_mirror(ctx, *m),
                _ => String::new(),
            };
            let to = match args.get(1) {
                Some(Value::Object(Some(m))) => module_name_of_mirror(ctx, *m),
                _ => String::new(),
            };
            ctx.module_add_reads(&from, &to);
            Ok(None)
        },
        NativeKind::Bridge,
    );

    registry.register(
        "java/lang/Class",
        "desiredAssertionStatus0",
        "(Ljava/lang/Class;)Z",
        native_assertion_status,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "forName0",
        "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;",
        lang_class::native_class_for_name_defining,
        NativeKind::Bridge,
    );
    // Public-name `Class.forName` overloads. In stock OpenJDK these are
    // Java methods that forward to `forName0`; in synthetic-jdk mode the
    // bridge in `register_synthetic_overrides` registers the same set.
    // Promote them to essential so default-built cratonvm (no
    // `synthetic-jdk` feature) still resolves them — needed by
    // `bench/wave2-4` instrument probe's `RetransformAgent.premain` and
    // generally by any agent / DI framework that calls `Class.forName(...)`.
    registry.register(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        native_class_for_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
        lang_class::native_class_for_name_3, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
        lang_class::native_class_for_name_module, cratonvm_native_api::NativeKind::Bridge,
    );
    // Class.descriptorString(): JDK 25's bytecode reads the private
    // `componentType` field directly for array classes ("[" + componentType
    // .descriptorString()). CratonVM array mirrors leave that field null (the
    // value is computed lazily by the getComponentType native), so the real
    // bytecode NPEs on any array class — which breaks ObjectStreamClass's
    // serial-field computation (ObjectStreamField.<init> -> descriptorString)
    // for any serializable class with an array field. Bind the faithful native
    // (descriptor derived from the class name) in the essential path so the
    // real-JDK CLI build (which never calls register_synthetic_overrides) uses
    // it instead of the NPE-prone bytecode.
    //
    // Lane classshape's CLASSSHAPE19-1 (2026-10-07): that premise is stale (the
    // populator writes `componentType` for every array mirror), but the body
    // stays out under `--compatible` on cost: it builds the string with a
    // concat and a `replace`, 1.3-1.6 us against this native's 160-290 ns
    // (`R19ClassComponentCensus descriptor-string`). `--real-jdk` already lets
    // the body answer (`RETIRED_SHADOW` row in `native-api/src/retired_shadow.rs`).
    registry.register(
        "java/lang/Class",
        "descriptorString",
        "()Ljava/lang/String;",
        lang_class::native_class_descriptor_string, cratonvm_native_api::NativeKind::Bridge,
    );
    // isInterface/isPrimitive are bytecode in JDK 25 (`modifiers`/`primitive`
    // getfields) but `ACC_NATIVE` on 17 and 21, and HotSpot intrinsifies both.
    // REVIEWED `Intrinsic`, stated (SBR W6 classrefl, `SbrW6Classrefl` rows
    // `shape *`): the VM's `modifiers` field is the class file's raw
    // `access_flags` (no InnerClasses substitution, no `ACC_SUPER` mask, primitives
    // 0) and a stand-in mirror's `primitive` flag is name-derived, so the JDK body
    // reads fields this VM does not keep HotSpot-true; these natives answer from
    // the class table and were diffed against HotSpot 25 on 60+ receiver shapes
    // (0 rows differ). Retiring them would also break JDK 17/21, where the image
    // declares them native.
    //
    // JIT round 19 wave 1, lane classshape (BUF18-6): since interpreter round i1
    // wave 52 the populator writes HotSpot's `modifier_flags` (and `primitive`,
    // `componentType`) into every mirror, so the review note above is stale for
    // a JDK 25 image. With `CRATONVM_CLASS_SHAPE_JDK_BODY` on (perf, default ON
    // since the round 19 A/B; `=0` restores the three natives;
    // `lang_class::class_shape_jdk_body`) none of the three is registered and
    // JDK 25's field-reading bodies answer -- real class bytes, which the IR
    // tier splices instead of crossing a native per call.
    if !lang_class::class_shape_jdk_body(registry) {
        registry.register(
            "java/lang/Class",
            "isInterface",
            "()Z",
            native_class_is_interface,
            NativeKind::Intrinsic,
        );
        registry.register(
            "java/lang/Class",
            "isPrimitive",
            "()Z",
            native_class_is_primitive,
            NativeKind::Intrinsic,
        );
        // SBR5 lane ncall: LEAF (audited): isArray reads the mirror class id/name only.
        registry.with_leaf(true, |r| {
            r.register("java/lang/Class", "isArray", "()Z", native_class_is_array, cratonvm_native_api::NativeKind::Bridge);
        });
    }
    registry.register("java/lang/Class", "isEnum", "()Z", native_class_is_enum, cratonvm_native_api::NativeKind::Bridge);
    // JDK 25 native methods needed for real Class bootstrap:
    registry.with_leaf(true, |r| {
        r.register(
            "java/lang/Class",
            "isInstance",
            "(Ljava/lang/Object;)Z",
            lang_class::native_class_is_instance,
            NativeKind::Bridge,
        );
    });
    // WALK12-3 (`CRATONVM_CLASS_IS_ASSIGNABLE_FROM_LEAF`, default ON; JIT round
    // 20 wave 11, lane pNat): LEAF (audited) like `isInstance` above -- the
    // resolved-pair answer skips the funnel, every other call declines to it
    // and runs `native_class_is_assignable_from` there unchanged
    // (`lang_class::native_class_is_assignable_from_leaf`). `=0` registers the
    // old callback, non-leaf. The kind stays `Bridge`, stated.
    // `CRATONVM_CLASS_IS_ASSIGNABLE_FROM_LEAF_FULL_BODY` (default ON) picks the
    // leaf callback that runs the whole native for array / primitive /
    // unresolved pairs instead of declining them (`=0`: the declining one).
    if lang_class::class_is_assignable_from_leaf_enabled() {
        let leaf_callback: cratonvm_native_api::NativeCallback =
            if lang_class::class_is_assignable_from_leaf_full_body_enabled() {
                lang_class::native_class_is_assignable_from_leaf
            } else {
                lang_class::native_class_is_assignable_from_leaf_declining
            };
        registry.with_leaf(true, |r| {
            r.register(
                "java/lang/Class",
                "isAssignableFrom",
                "(Ljava/lang/Class;)Z",
                leaf_callback,
                NativeKind::Bridge,
            );
        });
    } else {
        registry.register(
            "java/lang/Class",
            "isAssignableFrom",
            "(Ljava/lang/Class;)Z",
            lang_class::native_class_is_assignable_from,
            NativeKind::Bridge,
        );
    }
    registry.register(
        "java/lang/Class",
        "cast",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        lang_class::native_class_cast, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "asSubclass",
        "(Ljava/lang/Class;)Ljava/lang/Class;",
        lang_class::native_class_as_subclass, cratonvm_native_api::NativeKind::Bridge,
    );
    // W4B: LEAF with a decline (`native_class_get_superclass_leaf`); the kind stays `Bridge`.
    registry.with_leaf(true, |r| {
        r.register(
            "java/lang/Class",
            "getSuperclass",
            "()Ljava/lang/Class;",
            lang_class::native_class_get_superclass_leaf,
            NativeKind::Bridge,
        );
    });
    registry.register(
        "java/lang/Class",
        "getInterfaces0",
        "()[Ljava/lang/Class;",
        lang_class::native_class_get_interfaces,
        NativeKind::Bridge,
    );
    // Not over a real JDK under `CRATONVM_CLASS_MODIFIERS_JDK_BODY`
    // (`lang_class::class_modifiers_jdk_body`; interpreter round i1 wave 52,
    // lane L3): the mirror's `modifiers` field carries HotSpot's value, which
    // JDK 25's body returns.
    if !lang_class::class_modifiers_jdk_body(registry) {
        registry.register(
            "java/lang/Class",
            "getModifiers",
            "()I",
            lang_class::native_class_get_modifiers,
            NativeKind::Intrinsic,
        );
    }
    // Class.getClassLoader() — JDK 25 bytecode is just `getfield classLoader;
    // areturn`, which always returns null because we never populate that
    // field on Class mirrors.  Override with a native that returns the
    // singleton app loader for non-bootstrap classes (and null for
    // bootstrap classes per JVM spec §5.3).  Without this, e.g.
    // commons-logging's `LogFactory.<clinit>` does
    // `LogFactory.class.getClassLoader().loadClass(...)` and NPEs on
    // the second invokevirtual.  See `lang_class.rs::native_class_get_class_loader`.
    // `--compatible` never adopts the JDK's built-in loaders, so it never fills `Class.classLoader` for
    // an application class: the JDK body (`return classLoader;`) answers null, and with real bytes
    // authoritative over a registered `Bridge` that null reached every caller-sensitive `Class.forName(String)`
    // (`ClassLoader.getClassLoader(caller)` -> `caller.getClassLoader0()`), so `Class.forName("Main")`
    // threw `ClassNotFoundException` for any application class (the Spring Boot suite, every class). The
    // native is the answer there, so it is a reviewed `Intrinsic` under `--compatible`; `--real-jdk`
    // keeps its registration as before (its mirrors carry the loader).
    if registry.compatibility_mode().is_jdk_only() {
        // An `Intrinsic` in `--real-jdk` too: the mirrors do NOT all carry the loader. This VM
        // defines every image class in its boot namespace and records `Class.classLoader` only for
        // a class the Java loaders asked it to define, so the JDK body (`return classLoader;`)
        // answered null for `jdk.compiler` / `jdk.jshell` classes the VM had already loaded
        // (`Class.forName("com.sun.source.tree.Tree").getClassLoader()` is the app loader on HotSpot):
        // Spring Boot's configuration processor lost every field default value through
        // `JavaCompilerFieldValuesParser`, 11 classes. The native applies the image-module rule
        // (`builtin_loader_for_image_class`) and was the winning registration until the FFM merge.
        registry.register(
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
            lang_class::native_class_get_class_loader,
            NativeKind::Intrinsic,
        );
    } else {
        registry.register(
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
            lang_class::native_class_get_class_loader,
            NativeKind::Intrinsic,
        );
    }
    // `getClassLoader0()` is the package-private cousin invoked from
    // `ClassLoader.getClassLoader(Class<?>)` and other JDK internals;
    // same semantics so route to the same native.
    if registry.compatibility_mode().is_jdk_only() {
        registry.register(
            "java/lang/Class",
            "getClassLoader0",
            "()Ljava/lang/ClassLoader;",
            lang_class::native_class_get_class_loader, cratonvm_native_api::NativeKind::Bridge,
        );
    } else {
        registry.register(
            "java/lang/Class",
            "getClassLoader0",
            "()Ljava/lang/ClassLoader;",
            lang_class::native_class_get_class_loader,
            NativeKind::Intrinsic,
        );
    }
    // NEW-8: Class.isHidden() — consult the real hidden flag set by
    // `Lookup.defineHiddenClass`. The previous stub always returned 0
    // which broke JEP 371 class-identity checks.
    registry.register(
        "java/lang/Class",
        "isHidden",
        "()Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let hidden = match lang_class::mirror_class_id(ctx, this) {
                // Lambda proxies (id >= 0x8000_0000) are JVM hidden classes in HotSpot
                // (JDK 15+). Reporting them as hidden makes `Class.getCanonicalName()`
                // return null (matching HotSpot) instead of leaking the synthesized
                // lambda name. bug-06 fam5 #1.
                Some(cid)
                    if cid.as_u32() >= 0x8000_0000 && ctx.lambda_proxy_host(cid).is_some() =>
                {
                    true
                }
                Some(cid) => ctx.is_class_hidden(cid),
                None => false,
            };
            Ok(Some(Value::Int(if hidden { 1 } else { 0 })))
        },
        NativeKind::Bridge,
    );
    // Round 63: org.jboss.staxmapper.IntVersion.toString() — WildFly uses
    // IntStream.of(segments).limit(n).mapToObj(Integer::toString).collect(joining("."))
    // to format its version. Our IntStream/mapToObj chain returns null on the
    // limit() path, leading to "Cannot invoke collect on null" inside
    // VersionedNamespace.createURN -> StandaloneXmlSchemas.<init>. Bypass the
    // stream pipeline entirely by reading the int[] `segments` field directly
    // and joining with dots. Matches IntVersion's no-arg contract: include all
    // non-trailing-zero segments (already stripped at construction time).
    registry.register(
        "org/jboss/staxmapper/IntVersion",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let segments = match ctx.get_field_by_name(this, "segments") {
                Value::Object(Some(arr)) => arr,
                _ => {
                    let s = ctx.create_string("0");
                    return Ok(Some(Value::Object(Some(s))));
                }
            };
            let n = ctx.array_length(segments);
            let mut parts: Vec<String> = Vec::with_capacity(n);
            for i in 0..n {
                match ctx.get_array_element(segments, i) {
                    Value::Int(v) => parts.push(v.to_string()),
                    _ => parts.push("0".to_string()),
                }
            }
            let joined = if parts.is_empty() {
                "0".to_string()
            } else {
                parts.join(".")
            };
            let s = ctx.create_string(&joined);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Also override the (int) overload that limits how many segments are emitted.
    registry.register(
        "org/jboss/staxmapper/IntVersion",
        "toString",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let limit = match args.get(1) {
                Some(Value::Int(v)) => *v as usize,
                _ => 0,
            };
            let segments = match ctx.get_field_by_name(this, "segments") {
                Value::Object(Some(arr)) => arr,
                _ => {
                    let s = ctx.create_string("0");
                    return Ok(Some(Value::Object(Some(s))));
                }
            };
            let n = ctx.array_length(segments);
            let take = n.min(limit.max(0));
            // If limit > n, IntVersion pads the remaining slots with `segment(i)` (=0).
            let total = if limit > n { limit } else { take };
            let mut parts: Vec<String> = Vec::with_capacity(total);
            for i in 0..total {
                if i < n {
                    match ctx.get_array_element(segments, i) {
                        Value::Int(v) => parts.push(v.to_string()),
                        _ => parts.push("0".to_string()),
                    }
                } else {
                    parts.push("0".to_string());
                }
            }
            let joined = if parts.is_empty() {
                "0".to_string()
            } else {
                parts.join(".")
            };
            let s = ctx.create_string(&joined);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Class.hasRealParameterData() — package-private method on java.lang.Class
    // in newer JDKs, used by reflection (e.g. Spring/Method.getParameters) to
    // check whether MethodParameters attribute data is available. Spring Boot
    // 4.0.6 (demo) and 2.0.3 (sportme) throw NoSuchMethodError without it.
    //
    // W3: answer from the class file instead of hard-coding "no parameter
    // data". A constant `false` makes every `-parameters`-compiled class look
    // unannotated, so Spring's constructor/`@RequestParam` name binding
    // silently degrades to `arg0`, `arg1`, … . Report true as soon as any
    // declared method actually carries a `MethodParameters` attribute.
    registry.register(
        "java/lang/Class",
        "hasRealParameterData",
        "()Z",
        |ctx, args| {
            let Some(Value::Object(Some(mirror))) = args.first() else {
                return Ok(Some(Value::Int(0)));
            };
            let Some(class_id) = ctx.class_id_from_mirror(*mirror) else {
                return Ok(Some(Value::Int(0)));
            };
            let methods = ctx.declared_methods(class_id);
            let mut has_data = false;
            for m in &methods {
                if !ctx
                    .method_parameters(class_id, &m.name, &m.descriptor)
                    .is_empty()
                {
                    has_data = true;
                    break;
                }
            }
            Ok(Some(Value::Int(i32::from(has_data))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP (application-level workaround with a documented root cause, not an
    // unimplemented native — see below).
    // Round 63: Keycloak start-dev re-augmentation no-op.
    //
    // `start-dev` invokes `AbstractAutoBuildCommand.runReAugmentation` which
    // calls `Build.runCommand()` to rebuild production config in-process via
    // a child picocli CommandLine. That nested rebuild path goes through
    // `Picocli.updateSpecHelpAndUnmatched`, which recursively removes the
    // `-h/--help` short option from every subcommand spec before re-adding
    // a Keycloak-specific help mixin. The recursion re-enters the same spec
    // a second time after the first pass has already added the new `-h`
    // option, and picocli's `CommandSpec.addOption` throws
    // `DuplicateOptionAnnotationsException("Option name '-h' is used by both
    // option --help and field AutoHelpMixin.helpRequested")`. That bubbles
    // up as `CommandLine.ExecutionException("Failed to update server
    // configuration.")` from `Build.runCommand` (pc=120/134), aborting
    // start-dev before the dev-mode server can boot.
    //
    // Re-augmentation is a production optimization (precompute the runtime
    // image of `kc.sh build`). Dev-mode (`start-dev`) does not require it —
    // Quarkus's dev runtime re-resolves configuration on demand. Stubbing
    // `Build.runCommand` to a no-op lets `runReAugmentation` return cleanly
    // and `start-dev` proceeds to Quarkus bootstrap.
    registry.register(
        "org/keycloak/quarkus/runtime/cli/command/Build",
        "runCommand",
        "()V",
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "initClassName",
        "()Ljava/lang/String;",
        lang_class::native_class_get_name,
        NativeKind::Bridge,
    );
    // getName() is a Java method that caches via initClassName(). Override with
    // native since JDK's Class field layout differs from our mirror layout.
    //
    // REVIEWED `Intrinsic`, 2026-09-10 — §1.4's remedy for this row makes it
    // WORSE, which is the case the contract's exception exists for.
    //
    // `getName` is not `ACC_NATIVE`, so by the letter it is a §1.4 shadow over
    // real bytecode. That bytecode is
    //
    //     String name = this.name;  return name != null ? name : initClassName();
    //
    // and `java.lang.Class.name` in this VM is an OVERLAY: the mirror allocator
    // (`vm/src/vm/vm_object.rs`, the `slots.name` store) writes the VM's
    // INTERNAL name there, and `lang_class::mirror_class_name_strict` and its
    // callers read it back in that form deliberately — that strict reader is
    // the fix for ByteBuddy's hierarchy walker, which the reverse map's Object
    // aliasing broke. So yielding hands every caller `java/lang/Object`.
    //
    // Worse than a null, because nothing throws. MEASURED with
    // `ClassNameSweep.java`, 85 rows, one binary, three arms:
    //
    // ```text
    //                                                  rows differing, of 85
    //   HotSpot vs --real-jdk unarmed                     0
    //   HotSpot vs CRATONVM_ENFORCE_NATIVE_SHADOW=all     9   <- before this tag
    //   HotSpot vs the same arm, after the tag            1
    // ```
    //
    // and the nine are not cosmetic: `Class.forName(X.class.getName())` throws
    // `ClassNotFoundException` for every reference type, `Class.toString()`
    // renders `class java/lang/Object`, and the invariant a binary name never
    // contains `/` fails. It is why `ServiceLoader.checkCaller` reported
    // *"module java.base does not declare `uses`"*.
    //
    // Those rows are also the reason a dial arm alone could not have found
    // this: rows reached through a METHOD REFERENCE (`c::getName`) kept the
    // native and answered correctly, while the same call written inline in a
    // lambda body yielded and answered `java/lang/Object`. Rows 1-5 of the
    // sweep — `getName`/`getTypeName`/`getCanonicalName`/`getSimpleName`/
    // `getPackageName` on `Object`, all method references — MATCH under the
    // dial. One tag removes the split by making the row exempt at every door.
    //
    // The one row the tag does not repair is recorded, not frozen:
    // `Class.forName(Nested.class.getName())` still throws
    // `ClassNotFoundException` under the dial while the same round-trip works
    // for `java.lang.Object` and `String[]`. That is a `Class.forName` defect
    // on a nested application class — `getName` now returns the name HotSpot
    // returns — and the sweep is checked in, so it goes red the day it is
    // fixed or the day this answer drifts.
    //
    // TAGGING THIS ROW TURNED `registrar_drift.rs` RED, and the red was the
    // gate's, not the tag's: its enclosing-fn sweep required the byte after
    // `register` to be `(`, so `register_with_kind(` was invisible to it and
    // every tagged triple's SHIPPING registration vanished from the scan. The
    // same run named `java/lang/Class.getModule`, tagged the day before — that
    // gate had been red on `dev` since. Both are fixed there, not worked
    // around here.
    //
    // An `Intrinsic` is exempt at every dispatch door and exempt from the
    // shadow census by construction. That is a real cost — the row leaves the
    // population the dial can ask about — and it is earned here by the numbers
    // above, not by "yielding breaks it".
    //
    // One distinction from the superseded L0 copy of this comment, kept because
    // it is the §1.5 boundary and it is easy to lose: `initClassName` sits
    // directly above and stays `Bridge` DELIBERATELY. That one IS `ACC_NATIVE`
    // in the image, so §1.5 governs it and `Bridge` is the correct tag; this
    // row is ordinary bytecode, which is why it needed §1.4's reviewed
    // exception instead. The two rows look interchangeable and are not.
    //
    // JIT round 20 wave 18, lane xWalk (`CRATONVM_CLASS_GET_NAME_MEMO`, default
    // ON, `=0` registers the line below as before): the same reviewed
    // `Intrinsic`, claimed LEAF through `lang_class::native_class_get_name_leaf`
    // -- a class's answer is memoized after its first call (the same interned
    // `String`, HotSpot's `c.getName() == c.getName()`), later calls answer it
    // without the funnel, anything else declines to the funnel and runs
    // `native_class_get_name` there unchanged.
    if lang_class::class_get_name_memo_enabled() {
        registry.with_leaf(true, |r| {
            r.register(
                "java/lang/Class",
                "getName",
                "()Ljava/lang/String;",
                lang_class::native_class_get_name_leaf,
                NativeKind::Intrinsic,
            );
        });
    } else {
        registry.register(
            "java/lang/Class",
            "getName",
            "()Ljava/lang/String;",
            lang_class::native_class_get_name,
            NativeKind::Intrinsic,
        );
    }
    registry.register(
        "java/lang/Class",
        "forPrimitiveName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        lang_class::native_class_get_primitive_class, cratonvm_native_api::NativeKind::Bridge,
    );
    // Synthetic bootstrap may still stub `java/lang/Class` when no real JDK
    // classfile is available. Keep the public name accessors declared by the
    // stub wired to the same native implementations used elsewhere.
    registry.register(
        "java/lang/Class",
        "getSimpleName",
        "()Ljava/lang/String;",
        lang_class::native_class_get_simple_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getCanonicalName",
        "()Ljava/lang/String;",
        lang_class::native_class_get_canonical_name, cratonvm_native_api::NativeKind::Bridge,
    );
    // SBR round 5 (lane walker): a reviewed Intrinsic running the JDK body's own
    // steps (`native_class_get_type_name_exact`), not a second derivation of the name.
    registry.register(
        "java/lang/Class",
        "getTypeName",
        "()Ljava/lang/String;",
        lang_class::native_class_get_type_name_exact,
        NativeKind::Intrinsic,
    );
    // JDK 25 reads the mirror's `componentType` field for both
    // (CLASSSHAPE19-1, `lang_class::class_component_jdk_body`): no native over
    // a JDK 25 image unless `CRATONVM_CLASS_COMPONENT_JDK_BODY=0`.
    if !lang_class::class_component_jdk_body(registry) {
        registry.register(
            "java/lang/Class",
            "getComponentType",
            "()Ljava/lang/Class;",
            lang_class::native_class_get_component_type, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/lang/Class",
            "componentType",
            "()Ljava/lang/Class;",
            lang_class::native_class_get_component_type, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    // Round 63: Class.arrayType() — bypass JDK's `Array.newInstance(this, 0).getClass()`
    // chain, which leaked nulls into Spring's GenericConversionService$Converters.
    // getClassHierarchy when invoked on the superclass / interfaces of an array
    // argument (e.g. `Object`, `Cloneable`, `Serializable`). Direct mirror lookup
    // matches what `Object.getClass()` produces for an actual array instance.
    registry.register(
        "java/lang/Class",
        "arrayType",
        "()Ljava/lang/Class;",
        lang_class::native_class_array_type, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaringClass0",
        "()Ljava/lang/Class;",
        lang_class::native_class_get_declaring_class,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getSimpleBinaryName0",
        "()Ljava/lang/String;",
        lang_class::native_class_get_simple_binary_name,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getEnclosingMethod0",
        "()[Ljava/lang/Object;",
        lang_class::native_class_get_enclosing_method,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getGenericSignature0",
        "()Ljava/lang/String;",
        lang_class::native_class_get_generic_signature,
        NativeKind::Bridge,
    );
    // GENS-1 (real-JDK): override Class.getGenericInterfaces() and
    // getGenericSuperclass() with our native implementations. The JDK's
    // own bytecode for these methods drives the SignatureParser →
    // Reifier pipeline (sun.reflect.generics.*), and on classes such as
    // `ArrayList` the parsed `path` list contains a null entry that
    // causes `Reifier.visitClassTypeSignature` to NPE on
    // `new StringBuilder(sc.getName())`. The crash surfaces as
    // "Cannot invoke getName on null" during Spring Boot's
    // `ApplicationConversionService.<clinit>` (and is reproducible with a
    // 5-line program that just touches `ArrayList.class.getGenericInterfaces()`).
    // Our natives parse the Signature attribute via
    // `cratonvm_reader::signature` and build the synthetic
    // `ParameterizedType` / `Class` mirror tree directly, side-stepping
    // the broken JDK parse path entirely. The matching `check_override`
    // entry in `vm_exec.rs` lets these natives take precedence over the
    // non-`ACC_NATIVE` JDK bytecode.
    registry.register(
        "java/lang/Class",
        "getTypeParameters",
        "()[Ljava/lang/reflect/TypeVariable;",
        lang_class::native_class_get_type_parameters, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getGenericInterfaces",
        "()[Ljava/lang/reflect/Type;",
        lang_class::native_class_get_generic_interfaces, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getGenericSuperclass",
        "()Ljava/lang/reflect/Type;",
        lang_class::native_class_get_generic_superclass, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getRawAnnotations",
        "()[B",
        lang_class::native_class_get_raw_annotations,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getRawTypeAnnotations",
        "()[B",
        lang_class::native_class_get_raw_type_annotations,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getConstantPool",
        "()Ljdk/internal/reflect/ConstantPool;",
        lang_class::native_class_get_constant_pool,
        NativeKind::Bridge,
    );
    // SBR wave 5 (reflmeta): the genuine `native` methods of
    // `jdk.internal.reflect.ConstantPool` (ACC_NATIVE on JDK 17/21/25, no
    // bytecode to yield to -- so `Bridge`, stated), which this VM never
    // registered: any real `AnnotationParser` / `TypeAnnotationParser` run
    // ended in `UnsatisfiedLinkError`. They serve the pool of the holder whose
    // mirror `Class.getConstantPool` put in `constantPoolOop`; the member
    // annotation bytes the reflection mints now carry index into it. Only the
    // entry kinds annotation parsing reads (plus size and tag) -- the
    // class/member/name-and-type resolvers stay unregistered, as before.
    for (name, desc, f) in [
        (
            "getSize0",
            "(Ljava/lang/Object;)I",
            lang_class::native_constant_pool_get_size as NativeCallback,
        ),
        (
            "getUTF8At0",
            "(Ljava/lang/Object;I)Ljava/lang/String;",
            lang_class::native_constant_pool_get_utf8_at as NativeCallback,
        ),
        (
            "getStringAt0",
            "(Ljava/lang/Object;I)Ljava/lang/String;",
            lang_class::native_constant_pool_get_string_at as NativeCallback,
        ),
        (
            "getIntAt0",
            "(Ljava/lang/Object;I)I",
            lang_class::native_constant_pool_get_int_at as NativeCallback,
        ),
        (
            "getLongAt0",
            "(Ljava/lang/Object;I)J",
            lang_class::native_constant_pool_get_long_at as NativeCallback,
        ),
        (
            "getFloatAt0",
            "(Ljava/lang/Object;I)F",
            lang_class::native_constant_pool_get_float_at as NativeCallback,
        ),
        (
            "getDoubleAt0",
            "(Ljava/lang/Object;I)D",
            lang_class::native_constant_pool_get_double_at as NativeCallback,
        ),
        (
            "getTagAt0",
            "(Ljava/lang/Object;I)B",
            lang_class::native_constant_pool_get_tag_at as NativeCallback,
        ),
        // Interpreter round i5 wave 106, lane L3 (`L3W106ConstantPoolRefs`):
        // the class and reference readers, `UnsatisfiedLinkError` until now
        // for a library reading a lambda's pool (TypeTools' shape). The
        // member resolvers follow (wave 107).
        (
            "getClassAt0",
            "(Ljava/lang/Object;I)Ljava/lang/Class;",
            lang_class::native_constant_pool_get_class_at as NativeCallback,
        ),
        (
            "getClassAtIfLoaded0",
            "(Ljava/lang/Object;I)Ljava/lang/Class;",
            lang_class::native_constant_pool_get_class_at_if_loaded as NativeCallback,
        ),
        (
            "getClassRefIndexAt0",
            "(Ljava/lang/Object;I)I",
            lang_class::native_constant_pool_get_class_ref_index_at as NativeCallback,
        ),
        (
            "getNameAndTypeRefIndexAt0",
            "(Ljava/lang/Object;I)I",
            lang_class::native_constant_pool_get_name_and_type_ref_index_at as NativeCallback,
        ),
        (
            "getMemberRefInfoAt0",
            "(Ljava/lang/Object;I)[Ljava/lang/String;",
            lang_class::native_constant_pool_get_member_ref_info_at as NativeCallback,
        ),
        (
            "getNameAndTypeRefInfoAt0",
            "(Ljava/lang/Object;I)[Ljava/lang/String;",
            lang_class::native_constant_pool_get_name_and_type_ref_info_at as NativeCallback,
        ),
        // Interpreter round i5 wave 107, lane L3 (`L3W107ConstantPoolMembers`;
        // `i106-L3-constant-pool-member-resolvers-unregistered`): the member
        // resolvers, genuine ACC_NATIVE (`Bridge`, stated below like the rest),
        // answering `Method` / `Constructor` / `Field` as jvm.cpp's
        // `get_method_at_helper` / `get_field_at_helper` do.
        (
            "getMethodAt0",
            "(Ljava/lang/Object;I)Ljava/lang/reflect/Member;",
            lang_class::native_constant_pool_get_method_at as NativeCallback,
        ),
        (
            "getMethodAtIfLoaded0",
            "(Ljava/lang/Object;I)Ljava/lang/reflect/Member;",
            lang_class::native_constant_pool_get_method_at_if_loaded as NativeCallback,
        ),
        (
            "getFieldAt0",
            "(Ljava/lang/Object;I)Ljava/lang/reflect/Field;",
            lang_class::native_constant_pool_get_field_at as NativeCallback,
        ),
        (
            "getFieldAtIfLoaded0",
            "(Ljava/lang/Object;I)Ljava/lang/reflect/Field;",
            lang_class::native_constant_pool_get_field_at_if_loaded as NativeCallback,
        ),
    ] {
        registry.register(
            "jdk/internal/reflect/ConstantPool",
            name,
            desc,
            f,
            NativeKind::Bridge,
        );
    }
    registry.register(
        "java/lang/Class",
        "getDeclaredFields0",
        "(Z)[Ljava/lang/reflect/Field;",
        lang_class::native_class_get_declared_fields,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredFields",
        "()[Ljava/lang/reflect/Field;",
        lang_class::native_class_get_declared_fields, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredField",
        "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
        lang_class::native_class_get_declared_field, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredMethods0",
        "(Z)[Ljava/lang/reflect/Method;",
        lang_class::native_class_get_declared_methods,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredMethods",
        "()[Ljava/lang/reflect/Method;",
        lang_class::native_class_get_declared_methods, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredConstructors0",
        "(Z)[Ljava/lang/reflect/Constructor;",
        lang_class::native_class_get_declared_constructors,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredConstructors",
        "()[Ljava/lang/reflect/Constructor;",
        lang_class::native_class_get_declared_constructors, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        lang_class::native_class_get_declared_method, cratonvm_native_api::NativeKind::Bridge,
    );
    // C32: override JDK25 bytecode for getDeclaredConstructor so PD0$5's
    // probe returns a Constructor whose fields we fully populate (our
    // copyConstructor path doesn't re-initialise `root`/accessor and
    // would yield a half-built Constructor that NPEs on setAccessible).
    registry.register(
        "java/lang/Class",
        "getDeclaredConstructor",
        "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
        lang_class::native_class_get_declared_constructor, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getFields",
        "()[Ljava/lang/reflect/Field;",
        lang_class::native_class_get_fields, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getField",
        "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
        lang_class::native_class_get_field, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getMethods",
        "()[Ljava/lang/reflect/Method;",
        lang_class::native_class_get_methods, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        lang_class::native_class_get_method, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getConstructors",
        "()[Ljava/lang/reflect/Constructor;",
        lang_class::native_class_get_constructors, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getConstructor",
        "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
        lang_class::native_class_get_constructor, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredClasses0",
        "()[Ljava/lang/Class;",
        lang_class::native_class_get_declared_classes,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getRecordComponents0",
        "()[Ljava/lang/reflect/RecordComponent;",
        lang_class::native_class_get_record_components,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "isRecord0",
        "()Z",
        lang_class::native_class_is_record,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getNestHost0",
        "()Ljava/lang/Class;",
        lang_class::native_class_get_nest_host,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getNestMembers0",
        "()[Ljava/lang/Class;",
        lang_class::native_class_get_nest_members,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getPermittedSubclasses0",
        "()[Ljava/lang/Class;",
        lang_class::native_class_get_permitted_subclasses,
        NativeKind::Bridge,
    );
    // WP2.1-class-modern — `Class.getAnnotatedSuperclass()` /
    // `getAnnotatedInterfaces()`. Real-JDK impl is pure Java but reads
    // `getRawTypeAnnotations()` (a native we already provide) and parses
    // it into `AnnotatedType` instances; in synthetic-JDK mode the Java
    // pipeline isn't available, so we surface a minimal-but-non-null
    // synthetic `AnnotatedType` (see `make_annotated_type`). Registered
    // unconditionally so frameworks that probe the methods (ByteBuddy,
    // Hibernate ORM record/sealed scanners) get a non-throwing surface
    // in either mode.
    registry.register(
        "java/lang/Class",
        "getAnnotatedSuperclass",
        "()Ljava/lang/reflect/AnnotatedType;",
        lang_class::native_class_get_annotated_superclass, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Class",
        "getAnnotatedInterfaces",
        "()[Ljava/lang/reflect/AnnotatedType;",
        lang_class::native_class_get_annotated_interfaces, cratonvm_native_api::NativeKind::Bridge,
    );
    // TYPE_USE annotation surface for executables/parameters. The real-JDK
    // `getAnnotatedReturnType()` / `Parameter.getAnnotatedType()` parse the
    // `RuntimeVisibleTypeAnnotations` attribute via `getTypeAnnotationBytes0()`
    // (stubbed to null) + `jdk.internal.reflect.ConstantPool` (whose accessor
    // API we don't expose), so JSpecify `@Nullable`/`@NonNull` were invisible.
    // Intercept the public methods and build AnnotatedTypes carrying the parsed
    // top-level (empty type_path) type annotations, mirroring how the public
    // declared/parameter annotation methods are intercepted (see C24 above).
    // `Intrinsic`, not the ambient default and not `Bridge`: this is the only
    // door to a METHOD's return-type annotations, because the real
    // `getAnnotatedReturnType` is concrete bytecode that reads
    // `getTypeAnnotationBytes0()`, which this VM answers with null (there is no
    // constant-pool accessor to decode the attribute against). As a
    // `SyntheticStub` it was dropped under `--real-jdk`, and as a `Bridge` the
    // real body still outranked it (real class bytes beat a Bridge; only an
    // Intrinsic wins), so every `@Nullable`/`@NonNull` (JSpecify) on a return
    // type read as unspecified -- MEASURED: `NullnessTests`
    // `jspecifyNullableReturnType` and four siblings.
    registry.register(
        "java/lang/reflect/Method",
        "getAnnotatedReturnType",
        "()Ljava/lang/reflect/AnnotatedType;",
        lang_class::native_method_get_annotated_return_type,
        NativeKind::Intrinsic,
    );
    registry.register(
        "java/lang/reflect/Parameter",
        "getAnnotatedType",
        "()Ljava/lang/reflect/AnnotatedType;",
        lang_class::native_parameter_get_annotated_type, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getAnnotatedType",
        "()Ljava/lang/reflect/AnnotatedType;",
        lang_class::native_field_get_annotated_type, cratonvm_native_api::NativeKind::Bridge,
    );
    // Native dispatch keys on the concrete reflective class (see how
    // `getParameterAnnotations` is registered on Method/Constructor, not the
    // shared Executable), so register on both rather than Executable.
    registry.register(
        "java/lang/reflect/Method",
        "getAnnotatedParameterTypes",
        "()[Ljava/lang/reflect/AnnotatedType;",
        lang_class::native_executable_get_annotated_parameter_types, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getAnnotatedParameterTypes",
        "()[Ljava/lang/reflect/AnnotatedType;",
        lang_class::native_executable_get_annotated_parameter_types, cratonvm_native_api::NativeKind::Bridge,
    );
    // Accessor overrides for the `AnnotatedTypeBaseImpl` instances we build —
    // they stash an `Annotation[]` in the `annotations` field (the real impl
    // uses a `Map`), so the real `getDeclaredAnnotations()` (`map.values()`) /
    // `getAnnotation()` (`Class.cast(map.get(...))`) bytecode can't read it.
    // These read the stashed array; for real-bytecode-built instances (Map in
    // the field) they return empty, matching the null-type-annotation-bytes
    // reality. `isAnnotationPresent()` is the inherited default → getAnnotation.
    registry.register(
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        lang_class::native_annotated_type_get_declared_annotations, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        lang_class::native_annotated_type_get_declared_annotations, cratonvm_native_api::NativeKind::Bridge,
    );
}
