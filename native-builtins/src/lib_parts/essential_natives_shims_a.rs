#[allow(unused_imports)]
use super::super::*;

#[allow(unused_variables)]
pub fn register_essential_natives_with_shims_a(registry: &mut NativeMethodRegistry, shim_selection: app_shims::ShimSelection) {

    // JDK-module registrations stay unconditional except for the synthetic
    // InitialContext implementation.  A real JDK must execute its own JNDI
    // provider-selection bytecode so application `jndi.properties` resources
    // and user-supplied InitialContextFactory implementations are honored.
    // The compact in-memory naming implementation has a synthetic field layout
    // and is therefore valid only when the synthetic JDK feature is enabled.
    crate::wildfly_datasources_tx::register_jdk_datasource_natives(registry);
    #[cfg(feature = "synthetic-jdk")]
    crate::wildfly_naming::register_jdk_naming_natives(registry);
    crate::wildfly_security::register_jdk_security_natives(registry);

    // Integration-test harness support. These classes are not part of the JDK,
    // but test VMs use real-JDK mode and still need the print capture natives.
    register_test_harness_natives(registry);
    crate::hooks::jfr_register_jfr_natives(registry);
    crate::tls::register_conscrypt_native_bridges(registry);
    register_ecj_problem_overrides(registry);
    registry.register(
        "org/apache/jasper/servlet/JspServlet",
        "handleMissingResource",
        "(Ljakarta/servlet/http/HttpServletRequest;Ljakarta/servlet/http/HttpServletResponse;Ljava/lang/String;)V",
        native_jsp_servlet_handle_missing_resource, cratonvm_native_api::NativeKind::Bridge,
    );

    // DELIBERATELY NOT REGISTERED HERE: the `ReflectionFactory` serialization
    // bridge (`register_reflection_factory_serialization` — 18 overrides across
    // `sun/reflect/ReflectionFactory` and
    // `jdk/internal/reflect/ReflectionFactory`). Real-JDK mode runs the JDK's
    // own bytecode for all of it, which a differential probe shows is
    // byte-identical to HotSpot 25 both with and without the overrides.
    // Asserted by
    // `essential_path_does_not_override_reflection_factory_serialization`.
    //
    // A call to that registrar lived here from 364c469c4d (2026-07-09, WildFly
    // managed-server startup), commented as a real-JDK-mode bridge for JBoss
    // Marshalling — while feature-gated on `experimental-serialization` the
    // whole time. That feature is default-off for `cratonvm-vm` and
    // `cratonvm-cli`, and was then on for `libcratonvm` (dropped there in
    // interpreter wave 99); Cargo unifies features across a workspace resolve,
    // so the call was compiled OUT of every `-p cratonvm-cli` build (what every
    // suite runner uses, WildFly included) and IN for a `--workspace` build
    // (CI). Deleted rather than ungated so the two builds converge on the
    // JDK's own bytecode; ungating would have converged them on Rust overrides
    // no test asks for.
    //
    // If a JBoss Marshalling case ever does need one of these, add it back
    // WITHOUT a feature gate and with a test that fails when it is missing — a
    // gated registration on this path is invisible to every suite run. See
    // fixed-suite-bugs/serialization/ for the retired report.
    //
    // The registrar is still used by the synthetic path
    // (`register_serialization_natives`), where there is no JDK bytecode to run.

    registry.register(
        "com/fasterxml/jackson/core/StreamReadConstraints$Builder",
        "maxNameLength",
        "(I)Lcom/fasterxml/jackson/core/StreamReadConstraints$Builder;",
        native_jackson_stream_read_constraints_max_name_length, cratonvm_native_api::NativeKind::Bridge,
    );

    {
        register_spring_codec_intrinsics(registry);
    };
    // Reactor Netty schedules much of its connection work through shaded
    // JCTools MPSC queues. (The `Mono.just` bridge that used to ride here was
    // loader-blind and is deleted; see `messaging_shims.rs`.)
    register_queue_bridge_natives(registry);

    // Spring messaging.simp/STOMP integration uses Reactor Netty and embedded
    // ActiveMQ paths whose real-JDK bytecode depends on JVM internals that are
    // not fully modeled here. Register narrow bridge overrides for the known
    // native/locking/linkage gaps while leaving Spring's unit-test-sensitive
    // STOMP session classes on their Java implementations.
    register_spring_messaging_bridges(registry);

    // JDK 25 VectorSupport declares these three ACC_NATIVE methods in
    // java.base. Keep them in the real-JDK essential path; the broader
    // incubator Vector API shims remain synthetic-only overrides.
    crate::vector_api::register_vector_support_natives(registry);
    // The `VectorSupport` operations HotSpot intrinsifies. MUST stay in this
    // registrar, not a `phases_late` one: `register_synthetic_overrides` does
    // not run in real-JDK mode, which is exactly where these are needed.
    crate::vector_support_intrinsics::register_vector_support_intrinsics(registry);
    crate::lang_system::register_runtime_natives(registry);

    // Synthetic-stream `spliterator()` natives — synthetic stream objects
    // (stamped with the bare `java/util/stream/*Stream` interface) are produced
    // in real-JDK mode (e.g. `OptionalInt.stream()`), and real JDK stream code
    // (`IntStream.concat` → `a.spliterator()`) then calls `spliterator()` on
    // them. Without this the call falls to the abstract interface method →
    // `AbstractMethodError` (swallowed by JUnit's launcher → every parameterized
    // /factory test reported EMPTY). See `register_synthetic_stream_spliterators`.
    crate::phases_late::register_synthetic_stream_spliterators(registry);

    // RBIGDEC.1 — BigInteger / BigDecimal arithmetic + toString overrides.
    //
    // BigInteger.<clinit> can fail in real-JDK mode (intrinsic fallback,
    // SunJCE side-effects, etc.), causing the BigDecimal cascade NPE on
    // `Cannot read field 'signum' because the object is null`.  The
    // post-clinit fixup in `vm/src/vm/vm_util.rs` populates the static
    // constants but cannot patch the rest of BigInteger/BigDecimal's
    // partially-initialized internal tables (POW10, BIG_TEN_POWERS, etc.)
    // which the JDK's `add` / `multiply` bytecode dereferences.
    //
    // We register Rust-side arithmetic + `toString` overrides that read the
    // operand decimal value via `bi_read` / `bd_read` (which now understand
    // the real-JDK `signum`+`mag` and `intCompact`+`scale` layouts via
    // `NativeContext::resolve_field_index`) and produce a fresh result that
    // round-trips through `bi_alloc` / `bd_alloc`.  These overrides run
    // BEFORE the JDK bytecode (native dispatch takes priority — see
    // `try_stackless_invoke`), so the partially-init'd JDK internals are
    // never touched at runtime.
    register_biginteger_arithmetic_overrides(registry);
    register_bigdecimal_arithmetic_overrides(registry);
    // App-shim seam: ANTLR / ByteBuddy / Mockito / Hibernate. See
    // `crate::app_shims` — grouped so this family can be moved or made
    // conditional at one site. Still registered unconditionally, under the
    // same ambient `Intrinsic` category as before.
    if shim_selection.includes(crate::app_shims::ShimFamily::AppIntrinsics) {
        {
            crate::app_shims::register_app_intrinsic_shims(registry);
        };
    }
    // SBR-02 / bug-03: fast regex, and the one `java/lang/String` family that
    // still wins over real-JDK bytecode. The real `String.replaceAll` /
    // `replaceFirst` / `matches` bodies run `Pattern.compile(...).matcher(...)`
    // through the interpreted `java.util.regex` engine, which is 30–600× slower
    // than HotSpot (every Matcher step crosses the VM→native String-accessor
    // boundary — see wildfly-suite-bugs/bug-03). These fast Rust
    // `regex`/`fancy-regex` natives are normally registered only by
    // `register_synthetic_overrides` (compiled out in real-JDK mode), so they
    // are absent here by default and the real bytecode runs (the real-Java
    // default). Read directly (native-builtins cannot depend on vm::env_cache)
    // with the SAME default-ON / opt-out (`=0`/`false`) semantics as
    // `env_cache::native_string_regex`; registration runs once at VM init so the
    // lookup cost is negligible.
    //
    // # `register_with_kind(.., Intrinsic)`, and why it is load-bearing
    //
    // Every other `java/lang/String` `Bridge` is DROPPED in real-JDK mode by
    // `NativeMethodRegistry::register` (contract §1.4 — real class bytes are
    // authoritative), which is where the three deleted forced-native `String`
    // lists went. These four survive that drop on their KIND, which is exactly
    // §1.4's reviewed exception and needs no name list at any dispatch site.
    // Registering them `Bridge` — including by omission, since the ambient
    // category here is `Bridge` — silently deletes the SBR-02 fix.
    //
    // Stated at the site rather than inherited from an enclosing
    // `set_category`, so the census's `kind_stated` column records that
    // somebody adjudicated these four rather than that they inherited whatever
    // the call chain left behind. These are its first callers.
    //
    // Reviewed against HotSpot with `probes/StringPolicyMatrixProbe` (392
    // cases) and measured with `probes/StringRegexCostProbe`: the four are
    // ~2× faster than HotSpot on the `PluginXmlParser.format()` shape they
    // were written for, with identical digests. The review is what makes them
    // `Intrinsic`; being fast is not sufficient, and the review had to be
    // *repaired* first — see `lang_string.rs`, where these four now raise
    // `PatternSyntaxException` / NPE / `IndexOutOfBoundsException` where the
    // JDK does instead of falling back to a literal replacement and returning
    // a plausible wrong answer.
    let native_string_regex_enabled = crate::nbflags().native_string_regex;
    if native_string_regex_enabled {
        registry.register(
            "java/lang/String",
            "replaceAll",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            native_string_replace_all,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
        registry.register(
            "java/lang/String",
            "replaceFirst",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            native_string_replace_first,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
        registry.register(
            "java/lang/String",
            "matches",
            "(Ljava/lang/String;)Z",
            native_string_matches,
            cratonvm_native_api::NativeKind::Intrinsic,
        );
        // SBR-02 secondary finding: the `replace(CharSequence,CharSequence)`
        // overload is LITERAL all-occurrences replacement (NOT regex), but its
        // real-JDK body still runs interpreted per-char scans + StringBuilder
        // allocations (~13× slower than HotSpot under CratonVM — it dominated
        // the `PluginXmlParser.format` chain of 8 chained `replace` calls).
        // `native_string_replace_charseq` is byte-identical to Rust
        // `str::replace` (literal, non-overlapping, empty-target inserts at
        // every position — same as Java) so there's no regex-semantics risk;
        // gated together with the regex natives. NOTE: the `(char,char)`
        // overload has its own native, which is a `Bridge` and is therefore
        // dropped in real-JDK mode with the rest.
        // With a JIT, compiled JDK bytecode (`String.replace` -> `StringLatin1.replace`)
        // runs 6-7x faster than crossing the native funnel per call; keep the
        // `Intrinsic` for `--nojit` (`string-replace-charseq`, matching `regionMatches`).
        const REPLACE_CS_ROW: &str = "string-replace-charseq";
        if registry.interpreted_intrinsic_registers_row(REPLACE_CS_ROW) {
            registry.register(
                "java/lang/String",
                "replace",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;",
                native_string_replace_charseq,
                cratonvm_native_api::NativeKind::Intrinsic,
            );
            registry.mark_body_compilable_intrinsic_row(
                REPLACE_CS_ROW,
                "java/lang/String",
                "replace",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;",
            );
        }
    }
    // `CRATONVM_NATIVE_MATCHER_FIND`: real-JDK-layout `Matcher.find()`/
    // `find(int)`/`start`/`end`/`group` fast path — see the "java.util.regex.
    // Matcher — real-JDK-layout `find()`/`find(int)` fast path" module banner
    // further down this file (`native_matcher_find_realjdk`) for the full
    // design. Unlike `CRATONVM_NATIVE_STRING_REGEX` above, this accelerates
    // the explicit `Pattern.compile(...).matcher(...)` + `while (m.find())`
    // idiom, not just the `String` convenience methods. DEFAULT-ON (opt-out
    // `=0`/`false`) after a 141-case parity battery confirmed byte-identical
    // results to HotSpot; read directly (native-builtins cannot depend on
    // vm::env_cache) with the SAME semantics as
    // `env_cache::native_matcher_find`.
    let native_matcher_find_enabled = crate::nbflags().native_matcher_find;
    if native_matcher_find_enabled {
        // `NativeKind::Intrinsic`: required so `NativeMethodRegistry::register`'s
        // `keep_real_matcher_find_fastpath` exception (registry.rs) applies —
        // otherwise the unconditional real-JDK-mode Pattern/Matcher drop
        // (aimed at the legacy synthetic-layout bridge) would silently
        // swallow this registration too, since that drop keys on class name
        // alone. MUST be `Intrinsic`, not `Bridge`: the legacy synthetic
        // `Matcher.find`/`find(int)` this drop targets is itself registered
        // under `Bridge` in the real-JDK build (inherited from an ambient
        // `set_category(Bridge)` far above its own registration site) — using
        // `Bridge` here too would make the registry.rs exception match BOTH
        // registrations, silently un-dropping the legacy synthetic-layout
        // bridge alongside this one and corrupting every real `Matcher`
        // object via its raw slot-index field writes. See the long comment
        // on `keep_real_matcher_find_fastpath` in registry.rs for the full
        // story (this was a real regression caught during validation, not a
        // hypothetical).
        {
            // The warmed search touches primitive fields/arrays only. Cold,
            // unsupported and JDK end-flag paths decline before side effects.
            registry.with_leaf(true, |registry| {
                registry.register(
                    "java/util/regex/Matcher",
                    "find",
                    "()Z",
                    native_matcher_find_realjdk, cratonvm_native_api::NativeKind::Intrinsic,
                );
            });
            registry.register(
                "java/util/regex/Matcher",
                "find",
                "(I)Z",
                native_matcher_find_at_realjdk, cratonvm_native_api::NativeKind::Intrinsic,
            );
            // `start`/`end`/`group` read-only accessors over the state
            // `find`/`find(int)` already populate correctly — see the "---
            // `start()`/`end()`/..." banner above their implementations.
            registry.register(
                "java/util/regex/Matcher",
                "start",
                "()I",
                native_matcher_start_realjdk, cratonvm_native_api::NativeKind::Intrinsic,
            );
            registry.register(
                "java/util/regex/Matcher",
                "end",
                "()I",
                native_matcher_end_realjdk, cratonvm_native_api::NativeKind::Intrinsic,
            );
            registry.register(
                "java/util/regex/Matcher",
                "start",
                "(I)I",
                native_matcher_start_idx_realjdk, cratonvm_native_api::NativeKind::Intrinsic,
            );
            registry.register(
                "java/util/regex/Matcher",
                "end",
                "(I)I",
                native_matcher_end_idx_realjdk, cratonvm_native_api::NativeKind::Intrinsic,
            );
            registry.register(
                "java/util/regex/Matcher",
                "group",
                "()Ljava/lang/String;",
                native_matcher_group_realjdk, cratonvm_native_api::NativeKind::Intrinsic,
            );
            registry.register(
                "java/util/regex/Matcher",
                "group",
                "(I)Ljava/lang/String;",
                native_matcher_group_idx_realjdk, cratonvm_native_api::NativeKind::Intrinsic,
            );
        };
    }
    {
        registry.register(
            "java/nio/ByteBuffer",
            "allocate",
            "(I)Ljava/nio/ByteBuffer;",
            native_heap_bytebuffer_allocate, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            "java/nio/ByteBuffer",
            "allocateDirect",
            "(I)Ljava/nio/ByteBuffer;",
            native_heap_bytebuffer_allocate, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            "java/nio/charset/CodingErrorAction",
            "<clinit>",
            "()V",
            native_coding_error_action_clinit, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            "java/nio/charset/CodingErrorAction",
            "toString",
            "()Ljava/lang/String;",
            native_coding_error_action_to_string, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    };
    // Real-JDK mode still allocates synthetic `java/nio/ByteBuffer` objects for
    // `ByteBuffer.allocate*`; those receivers are stamped with the abstract
    // ByteBuffer class, so abstract instance methods such as `put(int, byte)`
    // need the S2 native surface in the essential registry too. Keep this to
    // ByteBuffer + ByteOrder rather than enabling the full synthetic NIO stack.
    servlet::register_s2_bytebuffer_essentials(registry);
    registry.register(
        "java/util/logging/Level",
        "<clinit>",
        "()V",
        native_level_clinit, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/logging/Level",
        "<init>",
        "(Ljava/lang/String;I)V",
        native_level_init, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/logging/Level",
        "<init>",
        "(Ljava/lang/String;ILjava/lang/String;)V",
        native_level_init, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/logging/Level",
        "getName",
        "()Ljava/lang/String;",
        native_level_get_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/logging/Level",
        "intValue",
        "()I",
        native_level_int_value, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/logging/Level",
        "toString",
        "()Ljava/lang/String;",
        native_level_get_name, cratonvm_native_api::NativeKind::Bridge,
    );

    // The fake-JDK WildFly launcher can resolve java.util.concurrent locks as
    // synthetic stubs before any real AQS bytecode is available. Tag these as
    // SyntheticStub and keep the dispatcher honest. In default real-JDK mode,
    // however, these registrations must not exist: several dispatch paths can
    // see a registered native before they have enough class context to prefer
    // real AQS bytecode, which lets the legacy monitor-backed ReentrantLock /
    // Condition bridge steal LinkedBlockingQueue's real lock operations and hang
    // executor shutdown. Keep the legacy surface opt-in and aligned with
    // `register_concurrent_natives` below.
    let synthetic_aqs = crate::nbflags().synthetic_aqs && !crate::nbflags().real_aqs;
    if synthetic_aqs {
        {
            let rl = "java/util/concurrent/locks/ReentrantLock";
            registry.register(rl, "<init>", "()V", native_rl_init, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(rl, "<init>", "(Z)V", native_rl_init_fair, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(rl, "lock", "()V", native_rl_lock, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(rl, "lockInterruptibly", "()V", native_rl_lock, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(rl, "unlock", "()V", native_rl_unlock, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(rl, "tryLock", "()Z", native_rl_try_lock, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(
                rl,
                "tryLock",
                "(JLjava/util/concurrent/TimeUnit;)Z",
                native_rl_try_lock_timeout, cratonvm_native_api::NativeKind::SyntheticStub,
            );
            registry.register(rl, "isLocked", "()Z", native_rl_is_locked, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(
                rl,
                "isHeldByCurrentThread",
                "()Z",
                native_rl_is_held_by_current_thread, cratonvm_native_api::NativeKind::SyntheticStub,
            );
            registry.register(rl, "getHoldCount", "()I", native_rl_get_hold_count, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(rl, "isFair", "()Z", native_rl_is_fair, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(
                rl,
                "newCondition",
                "()Ljava/util/concurrent/locks/Condition;",
                native_rl_new_condition, cratonvm_native_api::NativeKind::SyntheticStub,
            );
            registry.register(rl, "toString", "()Ljava/lang/String;", native_rl_to_string, cratonvm_native_api::NativeKind::SyntheticStub);

            let lock = "java/util/concurrent/locks/Lock";
            registry.register(lock, "lock", "()V", native_rl_lock, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(lock, "unlock", "()V", native_rl_unlock, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(lock, "tryLock", "()Z", native_rl_try_lock, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(
                lock,
                "newCondition",
                "()Ljava/util/concurrent/locks/Condition;",
                native_rl_new_condition, cratonvm_native_api::NativeKind::SyntheticStub,
            );

            let cond = "java/util/concurrent/locks/Condition";
            registry.register(cond, "await", "()V", native_cond_await, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(cond, "awaitUninterruptibly", "()V", native_cond_await, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(
                cond,
                "await",
                "(JLjava/util/concurrent/TimeUnit;)Z",
                native_cond_await_timeout, cratonvm_native_api::NativeKind::SyntheticStub,
            );
            registry.register(cond, "awaitNanos", "(J)J", native_cond_await_nanos, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(
                cond,
                "awaitUntil",
                "(Ljava/util/Date;)Z",
                native_cond_await_until, cratonvm_native_api::NativeKind::SyntheticStub,
            );
            registry.register(cond, "signal", "()V", native_cond_signal, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(cond, "signalAll", "()V", native_cond_signal_all, cratonvm_native_api::NativeKind::SyntheticStub);
        };
    }

    // `StandardLocation.computeIsModuleOrientedLocation` is a tiny javac helper
    // whose JDK 25 body is `Pattern.compile("\\bMODULE\\b").matcher(name)
    // .matches()`. CratonVM's real regex/JIT path can stall there while javac
    // scans file-manager locations, so keep this package-private helper on the
    // same native surface as the regex bridge and answer the equivalent result
    // directly (`matches`, not `find`, means only the exact token matches).
    registry.register(
        "javax/tools/StandardLocation",
        "computeIsModuleOrientedLocation",
        "(Ljava/lang/String;)Z",
        native_standard_location_compute_is_module_oriented_location, cratonvm_native_api::NativeKind::Bridge,
    );

    // The javac file-manager Bridges below (and the four `RelativePath` rows
    // further down) are not registered under `--compatible` over a real JDK
    // while the `JdkJavacFileManager` HotSpot-alignment row is in force
    // (default on; `retired_shadow::COMPAT_JAVAC_FILE_MANAGER_TRIPLES`):
    // `docs/feature-designs/native-dispatch-architecture.md`.
    registry.register(
        "com/sun/tools/javac/file/JavacFileManager",
        "checkNotModuleOrientedLocation",
        "(Ljavax/tools/JavaFileManager$Location;)V",
        native_javac_file_manager_check_not_module_oriented_location, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "com/sun/tools/javac/file/JavacFileManager",
        "list",
        "(Ljavax/tools/JavaFileManager$Location;Ljava/lang/String;Ljava/util/Set;Z)Ljava/lang/Iterable;",
        native_javac_file_manager_list, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "com/sun/tools/javac/file/JavacFileManager",
        "inferBinaryName",
        "(Ljavax/tools/JavaFileManager$Location;Ljavax/tools/JavaFileObject;)Ljava/lang/String;",
        native_javac_file_manager_infer_binary_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "com/sun/tools/javac/file/JavacFileManager$PathAndContainer",
        "compareTo",
        "(Ljava/lang/Object;)I",
        native_javac_path_and_container_compare_to, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "com/sun/tools/javac/util/StringNameTable$NameImpl",
        "hashCode",
        "()I",
        native_javac_string_name_hash_code, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Name.equals` is on javac's compiler lookup hot path. Preserve the JDK
    // identity/table semantics in native code so package and class resolution
    // does not repeatedly interpret this small virtual dispatch.
    registry.register(
        "com/sun/tools/javac/util/Name",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_javac_shared_name_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "com/sun/tools/javac/util/SharedNameTable$NameImpl",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_javac_shared_name_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "com/sun/tools/javac/util/StringNameTable$NameImpl",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_javac_shared_name_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/springframework/core/test/tools/CompileWithForkedClassLoaderExtension",
        "isUsingForkedClassPathLoader",
        "(Lorg/junit/jupiter/api/extension/ExtensionContext;)Z",
        native_spring_is_using_forked_class_path_loader, cratonvm_native_api::NativeKind::Bridge,
    );

    registry.register(
        "com/sun/tools/javac/file/RelativePath",
        "hashCode",
        "()I",
        native_javac_relative_path_hash_code, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "com/sun/tools/javac/file/RelativePath",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_javac_relative_path_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "com/sun/tools/javac/file/RelativePath",
        "compareTo",
        "(Lcom/sun/tools/javac/file/RelativePath;)I",
        native_javac_relative_path_compare_to, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "com/sun/tools/javac/file/RelativePath",
        "getPath",
        "()Ljava/lang/String;",
        native_javac_relative_path_get_path, cratonvm_native_api::NativeKind::Bridge,
    );

    // JBoss Modules' Java-version gate can reach regex while Pattern/Matcher are
    // still synthetic stubs. Real-JDK mode drops these legacy layout natives via
    // `NativeMethodRegistry::set_drop_real_layout_synthetic`.
            // JIT r23 wave 4, lane L5 (`CRATONVM_REGEX_PATTERN_COMPILE_BYTECODE`,
    // opt-in): a real-JDK registry without the compile cache registers no
    // `Pattern.compile` row, so dispatch reaches the JDK body; see
    // `regex_matcher::pattern_compile_yields_to_bytecode`.
    if !crate::regex_matcher::pattern_compile_yields_to_bytecode(registry) {
        registry.register(
            "java/util/regex/Pattern",
            "compile",
            "(Ljava/lang/String;)Ljava/util/regex/Pattern;",
            native_pattern_compile, cratonvm_native_api::NativeKind::Intrinsic,
        );
        registry.register(
            "java/util/regex/Pattern",
            "compile",
            "(Ljava/lang/String;I)Ljava/util/regex/Pattern;",
            native_pattern_compile_flags, cratonvm_native_api::NativeKind::Intrinsic,
        );
    }
        registry.register(
        "java/util/regex/Pattern",
        "matcher",
        "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
        native_pattern_matcher, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/regex/Matcher",
        "find",
        "()Z",
        native_matcher_find, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/regex/Matcher",
        "find",
        "(I)Z",
        native_matcher_find_at, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/regex/Matcher",
        "matches",
        "()Z",
        native_matcher_matches, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/regex/Matcher",
        "group",
        "()Ljava/lang/String;",
        native_matcher_group, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/regex/Matcher",
        "group",
        "(I)Ljava/lang/String;",
        native_matcher_group_idx, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/regex/Matcher",
        "start",
        "()I",
        native_matcher_start, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/regex/Matcher",
        "start",
        "(I)I",
        native_matcher_start_idx, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/util/regex/Matcher", "end", "()I", native_matcher_end, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/util/regex/Matcher",
        "end",
        "(I)I",
        native_matcher_end_idx, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/regex/Matcher",
        "replaceAll",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_matcher_replace_all, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/regex/Matcher",
        "replaceFirst",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_matcher_replace_first, cratonvm_native_api::NativeKind::Bridge,
    );

    // bug-26 (kafka SCRAM): `javax.crypto.Mac` (getInstance/init/update/doFinal)
    // was only registered inside `register_synthetic_overrides`, which real-JDK
    // mode never calls — so `Mac.getInstance("HmacSHA256")` fell through to the
    // real JDK bytecode and threw `NoSuchAlgorithmException: Algorithm HmacSHA256
    // not available` (the real provider chain has no working HMAC MacSpi). The
    // synthetic Mac native computes a real, RFC-4231-correct HMAC over the
    // key+data accumulated via init/update, so promote it to the universal
    // essential path (matching how Cipher/MessageDigest are wired). Fixes the
    // SCRAM Formatter/Messages/CredentialUtils/SaslServer suite (0-pass → pass).
    crate::phases_late::register_p68_crypto_mac(registry);
    // bug-27 (kafka FileLog/RemoteLog/UnalignedFile record reads): only the
    // *abstract* `java/nio/channels/FileChannel.truncate` was overridden, so on a
    // real `sun.nio.ch.FileChannelImpl` (built by the reconcile-with-real path)
    // `FileRecords.truncateTo(...)` runs the JDK `FileChannelImpl.truncate`
    // bytecode, whose position bookkeeping issues a *negative* seek →
    // `IOException: seek0: …before start of file (os error 131)`. Override the
    // concrete subclass so truncate clamps the new length ≥ 0, truncates via the
    // fd table, and only moves the file pointer back when it sits past the new end
    // — never seeking < 0. fd is read from `this.fd` (a real FileDescriptor whose
    // `fd`/`handle` is the fd-table id, exactly how the read/write paths resolve it).
    registry.register(
        "sun/nio/ch/FileChannelImpl",
        "truncate",
        "(J)Ljava/nio/channels/FileChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Real bytecode (sun/nio/ch/FileChannelImpl.truncate) checks the
            // writable field and throws NonWritableChannelException before
            // touching the fd -- this concrete-class override bypasses that
            // bytecode entirely (see bug-27 comment above), so it must
            // replicate the check itself or a channel opened FileChannel.open(
            // path, READ) silently truncates the real file instead of
            // rejecting the call (
            // bug-h2-files-setposixfilepermissions-FIXED.md residual).
            //
            // FAIL CLOSED, and read by RESOLVED SLOT. The earlier form was
            // `!matches!(ctx.get_field_by_name(this, "writable"), Value::Int(0))`,
            // which asks the wrong question: production `get_field_by_name`
            // answers `Value::Object(None)` for a name it cannot resolve
            // (vm_exec.rs:10613-10623; trait contract at
            // native-api/src/registry.rs:2351-2354), and `Object(None)` is not
            // `Int(0)`, so an unresolvable `writable` made the guard read TRUE
            // and truncate the file. Measured NOT reachable today -- real JDK 25
            // declares `private final boolean writable` on FileChannelImpl
            // itself (javap), and both construction paths assign it
            // (native-io/src/file_channel.rs:487 for the hand-built channel,
            // the real `FileChannelImpl.open` bytecode for the reconcile-with-
            // real path at phases_late/nio_file.rs:5725) -- so this is a
            // hardening, not a live-bug fix. It is worth doing anyway: the old
            // form's safety was an accident of the field happening to resolve,
            // and it failed CLOSED under MockNativeContext (whose absent answer
            // is `Int(0)`), so any unit test of it read as protective while
            // production took the opposite arm.
            //
            // `int_field_strict` returns `Some` only when the name resolves to
            // an in-range slot on the receiver's own class AND the descriptor-
            // decoded value is an `Int` -- `boolean`/`Z` decodes as `Int(0|1)`.
            // Absent, out-of-range, or wrong-tag therefore all deny the write.
            let writable =
                matches!(field_read::int_field_strict(ctx, this, "writable"), Some(v) if v != 0);
            if !writable {
                return Err(throw_non_writable_channel_exception(ctx));
            }
            let fd_id: Option<u32> = match ctx.get_field_by_name(this, "fd") {
                Value::Object(Some(fd_obj)) => match ctx.get_field_by_name(fd_obj, "fd") {
                    Value::Int(v) if v >= 0 => Some(v as u32),
                    _ => match ctx.get_field_by_name(fd_obj, "handle") {
                        Value::Long(v) if v >= 0 => Some(v as u32),
                        _ => None,
                    },
                },
                _ => None,
            };
            let new_len_signed = match args.get(1) {
                Some(Value::Long(v)) => *v,
                Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()),
                _ => 0,
            };
            // "@throws IllegalArgumentException If the new size is negative"
            // (`FileChannel.truncate(long)`). The abstract
            // `java/nio/channels/FileChannel` copy of this method has carried
            // the check since it was found there; this CONCRETE copy — the one
            // a real `sun.nio.ch.FileChannelImpl` receiver actually reaches,
            // and therefore the one `Files.newByteChannel(...).truncate(-1)`
            // runs — still had the `.max(0)` clamp. MEASURED after the other
            // copy was fixed: HotSpot `IllegalArgumentException: Negative
            // size`, this VM no-throw. The half-fixed duplicate pair, exactly
            // as the campaign's `owns_slot` note predicts.
            //
            // The clamp is the most destructive shape a refusal can take here:
            // `truncate(-1)` — what an off-by-one length computation produces —
            // became `truncate(0)`, i.e. it DELETED the file's contents and
            // answered as if that had been asked for.
            if new_len_signed < 0 {
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        // HotSpot's message is the bare literal, no value
                        // echoed (`apps/probes/L4Diag`, measured against
                        // HotSpot 25: this VM said `Negative size: -1`).
                        // `--real-jdk` gets this for free by yielding to real
                        // bytecode (`RETIRED_SHADOW_PHASE2_TRIPLES`); this is
                        // the same parity fix for the default/compatible mode,
                        // which still runs this native.
                        message: "Negative size".to_string(),
                    }
                    .into(),
                );
            }
            let new_len = new_len_signed as u64;
            if let Some(fd) = fd_id {
                // Real FileChannel.truncate() contract: a no-op when the
                // requested size is >= the current file size (only ever
                // shrinks). The original bug-27 code called rw_set_length
                // unconditionally, which *grows* (zero-extends) the file
                // when truncate() is called with a larger size -- caught by
                // TestFileSystem.testRandomAccess, which runs the identical
                // op sequence against a real RandomAccessFile oracle and
                // expects truncate(9800) on a 570-byte file to be a no-op.
                let cur_size = ctx.fd_table().file_size(fd).unwrap_or(0);
                if new_len < cur_size {
                    ctx.fd_table().rw_set_length(fd, new_len).map_err(|e| {
                        cratonvm_types::error::RuntimeError::IOException {
                            message: e.to_string(),
                        }
                    })?;
                }
                let cur_pos = ctx
                    .fd_table()
                    .rw_seek(fd, std::io::SeekFrom::Current(0))
                    .unwrap_or(0);
                if cur_pos > new_len {
                    let _ = ctx
                        .fd_table()
                        .rw_seek(fd, std::io::SeekFrom::Start(new_len));
                }
            }
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `Long.parseLong(String)J` / `Integer.parseInt(String)I` — in real-JDK
    // mode, these fall through to JDK bytecode whose loop multiplies-and-adds
    // digit-by-digit (`result = result * 10 + digit`). Bounds-check arithmetic
    // for values above ~2^54 trips a wrong overflow path in our interpreter
    // (Long.parseLong("35747322042253312") returned 1970324836974592 = 7·2^48,
    // dropping the high 0x78 bits of the 0x7F top byte; Long.parseLong(
    // "9151314442816847872") returned 2114440025050447872, similar pattern).
    // Cascading impact: ASN1RelativeOID.writeField takes the long branch for
    // arc components of length ≤ 18 chars and encodes the wrong value, so the
    // BC ASN.1 OID/RelativeOID round-trip tests on the canonical wide-arc OID
    // `1.1.127.32512.…35747322042253312.9151314442816847872` decode back to
    // …1970324836974592… instead. Route through our Rust `text.parse::<i64>()`
    // so the parse is unconditionally correct.
    if !crate::lang_math::wrapper_box_natives_yield_to_bytecode(registry) {
        registry.with_leaf(true, |registry| {
            registry.register(
                "java/lang/Long",
                "parseLong",
                "(Ljava/lang/String;)J",
                crate::lang_math::native_long_parse_long, cratonvm_native_api::NativeKind::Bridge,
            );
        });
    }
    registry.register(
        "java/lang/Integer",
        "parseInt",
        "(Ljava/lang/String;)I",
        crate::lang_math::native_integer_parse_int, cratonvm_native_api::NativeKind::Bridge,
    );
    // The "extras" pack — `gcd`, `shiftLeft/Right`, `and/or/xor/not`,
    // `testBit`, `bitLength/bitCount`, `toByteArray`, `<init>([B)V`,
    // `<init>(I[B)V`, `modPow`, `modInverse`, `intValueExact`,
    // `longValueExact`, `isProbablePrime`. Previously only registered in
    // synthetic-JDK mode via `register_builtins`. In real-JDK mode the
    // JDK bytecode bodies for these methods depend on intrinsics / DRBG
    // plumbing we don't fully model — `isProbablePrime` returns false for
    // every prime ≥ ~63 bits (Miller-Rabin path), which surfaces as
    // BC's `ECCurve.Fp.<init>` throwing "Fp q value not prime" for SM2
    // and X9 curves. Our string-decimal helpers (sweep landed alongside
    // this commit, follow-up to 958baae) preserve full precision.
    crate::phases_late::register_p71_biginteger_extras(registry);
    // App-shim seam: BouncyCastle. The ~30 crypto-kernel registrars that used
    // to be inlined here now live behind one call in `crate::app_shims`, with
    // their per-kernel rationale moved alongside them. Order, ambient
    // category and the registered set are unchanged; the family is still
    // registered on every boot, including for programs that never load BC.
    if shim_selection.includes(crate::app_shims::ShimFamily::BouncyCastle) {
        crate::app_shims::register_bouncycastle_shims(registry);
    }

    // Spring Boot loader in real-JDK mode can resolve Pattern natives through
    // synthetic-stub dispatch paths before/without usable JDK bytecode
    // linkage. Keep regex natives in essentials too (not only synthetic
    // overrides) so Pattern.compile(String,int) is always reachable.
    register_regex_natives(registry);
    // Same rationale for java.net URL/URI entrypoints used by Spring Boot
    // launcher initialization (`URL.<init>(String)` and friends).
    register_net_natives(registry);

    // NIO2 asynchronous channels (AsynchronousSocketChannel / -ServerSocketChannel
    // / -ChannelGroup / -FileChannel). These were only registered via
    // `register_synthetic_overrides` (synthetic-JDK mode); in real-JDK mode the
    // abstract `AsynchronousSocketChannel.open(...)` returns a synthetic instance
    // whose `connect`/`read`/`write` resolved to the abstract (no-Code) methods —
    // `AbstractMethodError` in the Tomcat WebSocket client
    // (`WsWebSocketContainer.connectToServerRecursive`). Wire them into the
    // universal essential path so the synthetic impl backs the abstract class in
    // both modes. (No real IOCP/`sun.nio.ch` async stack exists in CratonVM, so
    // the synthetic implementation is the only backing for these abstract
    // classes.)
    crate::phases_late::register_p67_async_channels(registry);

    // Surefire's forked JVM calls `ClassLoader.setDefaultAssertionStatus` on the
    // context loader before the JDK static `assertionLock` is assigned; the real
    // bytecode does `synchronized (assertionLock)` and NPEs. Full
    // `register_classloader_natives` (which installs the Rust no-op) runs only in
    // synthetic-JDK mode — real-JDK mode uses `register_essential_natives` only.
    // We do the assignment the bytecode would have done (`defaultAssertionStatus
    // = enabled`) directly on the receiver, minus the `synchronized
    // (assertionLock)` that NPEs, so a later `ClassLoader.clearAssertionStatus`
    // / reflective read observes the value instead of a silently dropped write.
    //
    // Under `CRATONVM_REFLECTION_JDK_BODIES` (`lang_class::reflection_jdk_bodies`,
    // interpreter round i1 wave 51, lane L3) the JDK body's other step runs
    // too: `if (classAssertionStatus == null) initializeJavaAssertionMaps();`.
    // JDK 25's `Class.desiredAssertionStatus()` (which then runs) asks the
    // loader's table only once those maps exist, so without the step a
    // `setDefaultAssertionStatus(true)` alone was never honoured
    // (`L3W51ReflectionBridgesYield`, row `loader-default-on`).
    registry.register(
        "java/lang/ClassLoader",
        "setDefaultAssertionStatus",
        "(Z)V",
        if lang_class::reflection_jdk_bodies(registry) {
            native_cl_set_default_assertion_status_with_maps
        } else {
            native_cl_set_default_assertion_status_field
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // RKC16N.6 RECON-STUB (Session 94): layout-neutral `java/lang/String`
    // surface for real-JDK mode. Real-JDK mode bytecode resolution is
    // failing for these basic String methods during JDK class clinits on
    // KC16; the natives are registered today but only inside
    // `register_synthetic_overrides` which real-JDK mode never calls.
    // Register a universal set here using `read_string` so we do not
    // depend on the synthetic vs JDK-25 byte[]+coder field layout.
    // Drop these when RKC16N.6 lands a proper fix (move the originals
    // into essentials, or fix bytecode dispatch).
    // `String.charAt(i)` indexes by UTF-16 CODE UNIT. The previous
    // `read_string().chars().collect()` indexed by code POINT, so for a string
    // containing a surrogate pair `charAt(0)` returned the combined code point
    // (e.g. 0x10401) instead of the high surrogate (0xD801), and indices past
    // the first supplementary char were all off. Delegate to the layout-aware
    // code-unit accessor (reads the compact `value: byte[]` + coder).
    registry.register(
        "java/lang/String",
        "charAt",
        "(I)C",
        crate::lang_string::native_string_char_at, cratonvm_native_api::NativeKind::Bridge,
    );
    // LEAF: `native_string_length` -> `string_char_count` is three heap reads —
    // field 0 (`value`), that array's length, and field 1 (`coder`) — with no
    // allocation, no safepoint and no throw. See
    // `NativeMethodRegistry::set_leaf`. Measured at 1405 ns from compiled code
    // against HotSpot's 0.2 ns (`probes/NativeShapeProbe.java`); `String.length`
    // is one of the most-called methods in any Java program.
    registry.set_leaf(true);
    registry.register(
        "java/lang/String",
        "length",
        "()I",
        // `String.length()` returns the UTF-16 CODE-UNIT count, not the
        // code-POINT count. The previous `read_string().chars().count()` decoded
        // to Rust `char`s (Unicode scalar values), so a surrogate pair counted
        // as 1 instead of 2 — `new String(new char[]{0xD801,0xDC01}).length()`
        // returned 1, and `charAt`/`codePointCount`/UTF-8 surrogate encoding
        // all broke downstream. Delegate to the layout-aware code-unit counter
        // (reads the compact `value: byte[]` length / coder).
        crate::lang_string::native_string_length, cratonvm_native_api::NativeKind::Bridge,
    );
    // NOT the `isEmpty` below: it goes through `ctx.read_string`, which decodes
    // the whole string into a fresh Rust `String`. That is not a Java-heap
    // allocation, so it does not break the leaf contract outright — but it is
    // O(n) work behind a predicate, and marking it leaf would advertise a
    // cheapness it does not have. Left on the funnel until someone rewrites it
    // against `string_char_count` and measures.
    registry.set_leaf(false);
    registry.register("java/lang/String", "isEmpty", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(1))),
        };
        let s = ctx.read_string(this).unwrap_or_default();
        Ok(Some(Value::Int(if s.is_empty() { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/String",
        "equals",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            if this.as_ptr() == other.as_ptr() {
                return Ok(Some(Value::Int(1)));
            }
            let a = ctx.read_string(this).unwrap_or_default();
            let b = match ctx.read_string(other) {
                Some(s) => s,
                None => return Ok(Some(Value::Int(0))),
            };
            Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // All four overloads share `lang_string::native_string_to_{lower,upper}_case`,
    // which honours the `Locale` argument (`args[1]`, absent here for the no-arg
    // forms → the default locale). Inlining the mapping here is what made
    // `"TITLE".toLowerCase(Locale.forLanguageTag("tr"))` return `"title"` instead
    // of HotSpot's `"tıtle"`.
    registry.register(
        "java/lang/String",
        "toLowerCase",
        "()Ljava/lang/String;",
        lang_string::native_string_to_lower_case, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "toUpperCase",
        "()Ljava/lang/String;",
        lang_string::native_string_to_upper_case_uncached, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "toLowerCase",
        "(Ljava/util/Locale;)Ljava/lang/String;",
        lang_string::native_string_to_lower_case, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "toUpperCase",
        "(Ljava/util/Locale;)Ljava/lang/String;",
        lang_string::native_string_to_upper_case_uncached, cratonvm_native_api::NativeKind::Bridge,
    );
    // String.hashCode — the layout-aware CACHING implementation (reads and
    // writes the JDK `hash` field) rather than recomputing from scratch on
    // every call. The previous inline closure here re-decoded the char array
    // and re-ran the fold every time, with NO caching, so real-JDK
    // String-keyed hashing was ~1950x slower than HotSpot (which caches in
    // `String.hash`): a 5M-call microbench took 17.6s vs HotSpot's 9ms, and it
    // dominated the Xerces XSD model build (`XSElementDecl.hashCode` /
    // `CMStateSet.hashCode` were ~100% of self-time).
    //
    // # `Intrinsic`, and why this one is not a performance argument
    //
    // Every other `java/lang/String` `Bridge` is dropped in real-JDK mode by
    // `NativeMethodRegistry::register` (contract §1.4). This one is stated
    // `Intrinsic` so it survives, and the reason is CORRECTNESS, not speed:
    // the real `String.hashCode()` bytecode is **wrong** here for any string
    // whose backing array is UTF-16.
    //
    // Measured with `probes/StringUtf16HashProbe`, which computes the JLS
    // formula in plain Java over the receiver's own `charAt` and compares:
    //
    //   "ΣΟΣ"   hashCode() = 62956255   JLS = 924359
    //
    // The object is not corrupt — `length()`, `charAt` and `equals` on it all
    // agree with HotSpot. The bytecode path reads the first `length()` BYTES
    // of the backing array, each sign-extended to a `char`, instead of the
    // `length()` UTF-16 code units: `(char) value[i]` where it needs
    // `getChar(value, i)`. Solving the observed hashes for their input
    // sequence gives that exact reading for all four probe strings, including
    // the sign extension (`0xA3` hashed as `0xFFA3`).
    //
    // So dropping this registration replaces a correct answer with a wrong one
    // for every non-Latin-1 `String` key in the VM, which is not an edge case:
    // it is every `HashMap<String,_>` with a non-ASCII key. The underlying
    // `StringUTF16` defect is filed separately — see
    // `string-utf16-hashcode-reads-bytes-not-code-units-FIXED-20260805.md`
    // — and when it is fixed this registration should be re-measured and
    // probably deleted, because at that point it becomes a pure perf
    // optimisation again and has to argue for itself on those terms.
    // NOT `register_with_kind(.., Intrinsic)`, deliberately: this stays a
    // `Bridge`, so the real-JDK drop takes it and the real bytecode runs.
    //
    // It was promoted to `Intrinsic` on 2026-08-05 for CORRECTNESS, because
    // `String.hashCode()` on a UTF-16 string was wrong — `ArraysSupport.
    // vectorizedHashCode` read one byte per char under `T_CHAR` instead of
    // pairing them. That defect is FIXED (`phases_early.rs`), so the
    // registration has to argue on performance again, which is what it was
    // originally written for.
    //
    // It loses that argument. Measured A-B-B-A interleaved, three rounds,
    // `probes/StringHashCostProbe`, identical digests (medians, ms):
    //
    //           cold-latin1  cold-utf16  warm  map-utf16
    //   native       89         135        5      110
    //   bytecode     95         149        2      111
    //
    // Slightly faster on the first hash of a distinct string, 2-4x SLOWER on
    // the cached read, and a wash on the realistic `HashMap<String,_>`
    // workload. Both sides cache in the same `String.hash` field, so the warm
    // gap is `safe_native_call` overhead on what should be one field read.
    //
    // §1.4's default is the bytecode, and a review that comes back "wash"
    // does not license shadowing it. (Serial measurement said the native was
    // slower on cold too; interleaving reversed that, which is why the arms
    // are interleaved.) Kept registered because synthetic-jdk mode still needs
    // it -- there the drop does not apply.
    //
    // Those numbers were asked to be re-checked against a real workload on the
    // Linux host before this was final. Done, same day, on two dev-tip binaries
    // differing only in this registration's kind: in-VM `javac` over 60 classes
    // whose constants are UTF-16 `HashMap` keys, A-B-B-A, two rounds —
    //
    //   native    97408  95020  95599  87269 ms
    //   bytecode  98888  86560  90650  87245 ms
    //
    // — fully overlapping, on a corpus picked to maximise the effect. The
    // microbenchmark above also reproduced independently (medians: cold 100 vs
    // 108 latin1, 148 vs 150 utf16; warm 6/7 vs 3/2; map 120 vs 121). So the
    // native does not come back.
    //
    // A first, NON-interleaved pass of that javac A/B reported 49.8 s against
    // 71.0 s — a clean 1.43x, every native round below every bytecode round —
    // and it did not survive A-B-B-A on a loaded host. That is the third perf
    // claim in this feature to fail re-measurement and the second where the
    // ordering of the runs decided the answer. On this host an A/B that is not
    // A-B-B-A interleaved is not a measurement.
    registry.register(
        "java/lang/String",
        "hashCode",
        "()I",
        native_string_hash_code, cratonvm_native_api::NativeKind::Bridge,
    );
    register_xerces_cmstateset_intrinsics(registry);
    register_xerces_xml_parser_intrinsics(registry);
    register_liquibase_checksum_intrinsics(registry);
    // E27-1 N2c: these four were hand-rolled closures over
    // `s.chars().enumerate()`, which yields a CODE POINT index, not a UTF-16
    // one — `"x\u{10437}yz".indexOf('z')` answered 3 where HotSpot answers 4,
    // off by one for every supplementary character before the hit, and `-1`
    // for every lone surrogate (`char::from_u32` rejects them). They were
    // copies six through NINE of a rule E18-1 had already put in exactly one
    // place, `lang_string.rs`'s `code_point_needle`.
    //
    // Rewired rather than deleted. Deleting needs them to be unreachable in
    // EVERY configuration, and they are only provably dead in two of the
    // three: real-JDK mode drops them (they are `Bridge`-kind on
    // `java/lang/String`, and `registry.rs`'s adjudication drops all but
    // `intern`), and a `synthetic-jdk` build has
    // `register_synthetic_overrides` re-register the same four later and win.
    // The third — feature off, drop-real-layout off — has neither mechanism.
    // Pointing them at the canonical natives is correct in all three and
    // needs no such proof.
    registry.register(
        "java/lang/String",
        "indexOf",
        "(I)I",
        native_string_index_of, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "lastIndexOf",
        "(I)I",
        native_string_last_index_of_char, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "lastIndexOf",
        "(II)I",
        lang_string::native_string_last_index_of_from, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "lastIndexOf",
        "(Ljava/lang/String;)I",
        // JIT r22 wave 11, lane intrinsics: the fifth of E27-1's closures,
        // missed by that rewiring. `rfind` + `chars().count()` answered a CODE
        // POINT index (`"\u{10437}ab".lastIndexOf("ab")` gave 1, HotSpot 2),
        // decoded lone surrogates as U+FFFD, and answered -1 for a null needle
        // where HotSpot throws. The canonical native fixed all three already;
        // the kind is unchanged (the ambient one of this block, as before).
        lang_string::native_string_last_index_of_str, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "indexOf",
        "(II)I",
        lang_string::native_string_index_of_from, cratonvm_native_api::NativeKind::Bridge,
    );

    // KEEP (W3), with the risk bounded rather than hand-waved.
    // RKC16N.9: org/jboss/modules/Module.<clinit> PC 154 invokes
    // JDKSpecific.addInternalPackages(list). JDKSpecific.<clinit> uses heavy
    // MethodHandle reflection and half-fails on cratonvm, so its static `hack`
    // field is null and the inner call NPEs — the class cannot run its own
    // body here at all.
    // The argument is jboss-modules' list of package prefixes that a
    // ModuleClassLoader must delegate to the platform rather than resolve
    // itself, and `Module.<clinit>` has ALREADY seeded it with the base
    // `java./javax./sun./com.sun.` set before this call; this native only
    // drops the *extra*, JDK-version-specific internal prefixes. Skipping them
    // costs delegation fidelity for `jdk.internal.*`-style names, never the
    // base JDK surface. The other `JDKSpecific` boot natives that CratonVM
    // does implement live in `native-builtins/src/jboss_jdkspecific.rs`; if a
    // ground-truth list for this method ever becomes available, that is where
    // the real implementation belongs.
    registry.register(
        "org/jboss/modules/JDKSpecific",
        "addInternalPackages",
        "(Ljava/util/List;)V",
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge,
    );

    // RKC16N-RECON: throwable ctors that take a String message. Real JDK has
    // bytecode for these; the dispatcher historically failed to find them
    // (same root-cause as String — see RKC16N.7), so we shadow `<init>`.
    //
    // STACK-TRACE CAPTURE FIX: route these throwable ctors through the canonical
    // capturing natives (`native_exc_init_message` / `native_exc_init_noargs`)
    // instead of a local closure that only stashed the message in raw slot 0.
    // The old closure had two bugs: (1) it never called fillInStackTrace, so
    // `getStackTrace()` came back empty for these subclasses; (2) it wrote the
    // String into slot 0, which in the real-JDK Throwable layout is `backtrace`
    // — clobbering the marker `getOurStackTrace()` keys on. The shared natives
    // write `detailMessage` by name, seed the `cause = this` sentinel, and
    // capture the trace.
    //
    // E43 (2026-08-13): this list held fourteen classes. Thirteen of them —
    // ClassCastException, IllegalArgumentException, IllegalStateException,
    // UnsupportedOperationException, NullPointerException,
    // IndexOutOfBoundsException, ArrayIndexOutOfBoundsException,
    // StringIndexOutOfBoundsException, NumberFormatException,
    // SecurityException, NoSuchMethodError, NoSuchFieldError and VerifyError —
    // are rows of the JDK-derived table in
    // `lang_misc::register_throwable_subclass_natives`, which every one of them
    // lists with BOTH `()V` and `(Ljava/lang/String;)V`, bound to these exact
    // two bodies. That registrar is called from THIS function, further down
    // (search `register_throwable_subclass_natives`), with no early return in
    // between, and `register` is last-write-wins — so all 26 of those
    // registrations were overwritten by an identical pair a few hundred lines
    // later, every boot, in both modes. Deleting them is a provable no-op and
    // it is 26 fewer shadowed rows for the duplicate-registration census.
    //
    // `java/lang/IllegalThreadStateException` is the one name that is NOT in
    // that table, so it is the one that has to stay. MEASURED on JDK 25.0.3+9:
    // it declares `()V` and `(Ljava/lang/String;)V` and both are public, so
    // this pair satisfies the same rule the table is built from — a descriptor
    // belongs iff the real class declares it and it is public (or was already
    // registered). It is not registered anywhere else.
    {
        let cls = &"java/lang/IllegalThreadStateException";
        let cls_static: &'static str = Box::leak(cls.to_string().into_boxed_str());
        registry.register(
            cls_static,
            "<init>",
            "(Ljava/lang/String;)V",
            crate::lang_misc::native_exc_init_message, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            cls_static,
            "<init>",
            "()V",
            crate::lang_misc::native_exc_init_noargs, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    // `java/lang/RuntimePermission` is NOT a Throwable — its `(String)` ctor
    // stores the permission *name* in field 0 (the real `Permission.name`
    // slot), and it has no `detailMessage`/`backtrace`/stack trace. Keep its
    // dedicated layout-neutral closure (must not route through the Throwable
    // capture path, which would no-op the name write and pointlessly record a
    // trace under the permission's identity hash).
    let permission_init = |ctx: &mut dyn NativeContext, args: &[Value]| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Some(Value::Object(Some(name))) = args.get(1) {
            ctx.set_field_by_name(this, "name", Value::Object(Some(*name)));
            ctx.set_field(this, 0, Value::Object(Some(*name)));
        }
        Ok(None)
    };
    let permission_get_name = |ctx: &mut dyn NativeContext, args: &[Value]| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        match ctx.get_field_by_name(this, "name") {
            Value::Object(Some(name)) => Ok(Some(Value::Object(Some(name)))),
            _ => Ok(Some(ctx.get_field(this, 0))),
        }
    };
    // `SyntheticStub`, not the ambient category, since 2026-08-29. These
    // closures were written in `b448f2039` when this VM FABRICATED the
    // permission classes — `synthetic_stub_fields` gave them a single field and
    // `synthetic_stub_methods` gave them these very constructors, so writing
    // `name` WAS the whole implementation. The real classes are loaded now,
    // with their real bodies, and the closures went on shadowing them.
    //
    // What that cost, measured against HotSpot: `permission_init` writes `name`
    // and returns, so `BasicPermission.<init>`'s `init(name)` and
    // `PropertyPermission.<init>`'s `init(getMask(actions))` never ran. Every
    // field those two set stayed at its default —
    //
    //     new PropertyPermission("a.b.*", "read,write")
    //       HotSpot   mask=3  path="a.b.*"  getActions()="read,write"
    //       CratonVM  mask=0  path=null     getActions()=""
    //
    // — so `implies()` NPEd on a null `that.path`, `new PropertyPermission(k,
    // "bogus")` was accepted where the JDK throws, and a serialization round
    // trip died in `readObject`'s `init(getMask(actions))` with "invalid
    // actions mask", which is how this was found. Neither `init` nor `getMask`
    // is broken: called reflectively both answer exactly as HotSpot does. Only
    // the constructors that should call them were being replaced.
    //
    // Tagging the family `SyntheticStub` and allow-listing it in
    // `real_protected_stub_class` hands every one of these back to the real
    // body whenever the real body is loaded — the same route
    // `ThreadPoolExecutor.execute` and `java/util/Objects` already take. On a
    // synthetic image `synthetic_stub_yields_with_cm` answers false ("loaded
    // class is itself a synthetic stub") and these keep serving, which is the
    // configuration they were written for.
    //
    // `register_with_kind`, not the `with_category` scope alone, on each call
    // below: before this loop was consolidated (`b448f2039`) and then tagged
    // `SyntheticStub` at the scope level, `java/util/logging/LoggingPermission`
    // reached its `SyntheticStub` kind by inheriting an ambient `Bridge`
    // category and tripping `register`'s own retired-shadow re-tag (its four
    // triples are listed in `retired_shadow.rs`), which also records
    // `kind_stated`. Scoping the category here instead reaches the same kind
    // through a path that never sets that bit, and the four LoggingPermission
    // rows silently lost their "this was adjudicated" bookkeeping with no
    // change in behaviour (`stated_shadows_bytecode`/kind-map baselines,
    // 2026-09-17 investigation). Stating it explicitly on every call — not
    // just the previously-adjudicated four — keeps the whole family stated
    // regardless of which path a future refactor reaches this kind through.
    {
        for permission_cls in [
            "java/security/Permission",
            "java/security/BasicPermission",
            "java/lang/RuntimePermission",
            "java/util/PropertyPermission",
            "java/util/logging/LoggingPermission",
        ] {
            registry.register(
                permission_cls,
                "<init>",
                "(Ljava/lang/String;)V",
                permission_init,
                cratonvm_native_api::NativeKind::SyntheticStub,
            );
            registry.register(
                permission_cls,
                "<init>",
                "(Ljava/lang/String;Ljava/lang/String;)V",
                permission_init,
                cratonvm_native_api::NativeKind::SyntheticStub,
            );
            registry.register(
                permission_cls,
                "getName",
                "()Ljava/lang/String;",
                permission_get_name,
                cratonvm_native_api::NativeKind::SyntheticStub,
            );
            // KEEP: `java.security.Permission` and its subclasses declare NO
            // no-arg constructor in the real JDK, so this triple is reachable
            // only for receivers CratonVM synthesizes itself — and those have
            // nothing to initialise (the `name` slot is written by the (String)
            // ctor above). The yield predicate leaves it alone on its own
            // terms: it finds no such method on the real class.
            registry.register(
                permission_cls,
                "<init>",
                "()V",
                |_ctx, _args| Ok(None),
                cratonvm_native_api::NativeKind::SyntheticStub,
            );
        }
    };
    {
        let c = "java/util/concurrent/atomic/AtomicBoolean";
        registry.register(c, "<init>", "()V", native_ab_init_default, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(c, "<init>", "(Z)V", native_ab_init_value, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(c, "get", "()Z", native_ab_get, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(c, "set", "(Z)V", native_ab_set, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(c, "lazySet", "(Z)V", native_ab_set, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(c, "compareAndSet", "(ZZ)Z", native_ab_cas, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(c, "getAndSet", "(Z)Z", native_ab_get_and_set, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(c, "toString", "()Ljava/lang/String;", native_ab_to_string, cratonvm_native_api::NativeKind::SyntheticStub);
    };
    registry.register(
        "java/lang/String",
        "indexOf",
        "(Ljava/lang/String;)I",
        // JIT r22 wave 11, lane intrinsics: was a closure answering a
        // code-POINT index (and -1 for a null needle); the canonical native
        // answers UTF-16 units and raises HotSpot's NPE. Kind unchanged.
        lang_string::native_string_index_of_str, cratonvm_native_api::NativeKind::Bridge,
    );
    // These two were inline closures that called `ctx.read_string(this)`
    // (decode the ENTIRE parent String) followed by `s.chars().collect()`
    // (ANOTHER full-string pass into a `Vec<char>`) on every single call,
    // regardless of how small the requested substring range was -- O(parent
    // length) instead of O(substring length) per call. This is the real,
    // active registration for `java/lang/String.substring` in real-JDK mode
    // (register_essential_natives; NOT the lang_string.rs `native_string_substring`
    // this file also defines, which is only reachable in
    // `register_synthetic_overrides`, `#[cfg(feature = "synthetic-jdk")]`-gated
    // and therefore never compiled into the default build -- a dead end this
    // investigation went down first). Root-caused via a `while
    // (m.find()) { m.group(N); }`-shaped user benchmark that turned out to
    // reduce to plain `String.substring()` on a large parent string scaling
    // O(n^2); see substring-large-parent-quadratic-allocation-FIXED.md.
    // `native_string_substring`/`native_string_substring_one` already have a
    // "read only the requested range, don't materialize the whole String
    // first" fast path (peeks at the backing array length + slices directly)
    // -- delegate to them instead of duplicating (and re-introducing) the bug.
    registry.register(
        "java/lang/String",
        "substring",
        "(II)Ljava/lang/String;",
        native_string_substring, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "substring",
        "(I)Ljava/lang/String;",
        lang_string::native_string_substring_one, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "startsWith",
        "(Ljava/lang/String;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let prefix = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let s = ctx.read_string(this).unwrap_or_default();
            let p = ctx.read_string(prefix).unwrap_or_default();
            Ok(Some(Value::Int(if s.starts_with(&p) { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "endsWith",
        "(Ljava/lang/String;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let suffix = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let s = ctx.read_string(this).unwrap_or_default();
            let p = ctx.read_string(suffix).unwrap_or_default();
            Ok(Some(Value::Int(if s.ends_with(&p) { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "replace",
        "(CC)Ljava/lang/String;",
        native_string_replace, cratonvm_native_api::NativeKind::Bridge,
    );
    // WP8.10.9: register String.contains(CharSequence) so synthetic-jdk
    // mode no longer NSME's when WildFly/JBoss code does
    // `name.contains("Module")`. The descriptor in OpenJDK 25 is
    // `(Ljava/lang/CharSequence;)Z` — in practice every observed callsite
    // passes a String literal, so we route through `read_string` (which
    // returns `Some` for any string-shaped object). For non-String
    // CharSequence subtypes (StringBuilder, CharBuffer) `read_string`
    // also works because both expose char[] at slot 0 in synthetic-jdk
    // mode. Empty-needle is an explicit `true` per the JDK spec.
    registry.register(
        "java/lang/String",
        "contains",
        "(Ljava/lang/CharSequence;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let needle = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let s = ctx.read_string(this).unwrap_or_default();
            let n = ctx.read_string(needle).unwrap_or_default();
            if n.is_empty() {
                return Ok(Some(Value::Int(1)));
            }
            Ok(Some(Value::Int(if s.contains(&n) { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "join",
        "(Ljava/lang/CharSequence;[Ljava/lang/CharSequence;)Ljava/lang/String;",
        native_string_join, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/String",
        "join",
        "(Ljava/lang/CharSequence;Ljava/lang/Iterable;)Ljava/lang/String;",
        native_string_join_iterable, cratonvm_native_api::NativeKind::Bridge,
    );
    // WP8.10.9: also register the offset variant of startsWith — the
    // single-arg `startsWith(String)` is already registered above, but
    // bytecode that does `name.startsWith("Module", 4)` would still NSME
    // without this entry.
    registry.register(
        "java/lang/String",
        "startsWith",
        "(Ljava/lang/String;I)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let prefix = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let off = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let s = ctx.read_string(this).unwrap_or_default();
            let p = ctx.read_string(prefix).unwrap_or_default();
            if off < 0 {
                return Ok(Some(Value::Int(0)));
            }
            let off = off as usize;
            let chars: Vec<char> = s.chars().collect();
            if off > chars.len() {
                return Ok(Some(Value::Int(0)));
            }
            let tail: String = chars[off..].iter().collect();
            Ok(Some(Value::Int(if tail.starts_with(&p) { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // WP8.10.7 — Throwable.getMessage / printStackTrace / getLocalizedMessage /
    // toString / getCause registered on synthetic-stub Throwable subclasses
    // (NoClassDefFoundError, ClassNotFoundException, NoSuchMethodError, …).
    // Without this, jboss-modules / WildFly catch-block code paths trip a
    // secondary NoSuchMethodError when calling `t.getMessage()` on a caught
    // synthetic-stub Throwable subclass — see bench/wildfly-boot/diagnostic.md
    // §WP8.10.7. Field layout verified against the `instance_fields(2)` arm
    // in classloading/src/class_manager.rs::synthetic_stub_fields:3373+.
    // Wired here in `register_essential_natives` (universal) so the natives
    // are reachable in BOTH synthetic-jdk and real-JDK feature configurations.
    crate::lang_misc::register_throwable_subclass_natives(registry);

    // java/io/InputStreamReader: removed the synthetic <init>/read([CII)I/close
    // overrides here. read([CII)I) did a dumb byte-for-byte passthrough --
    // chars[i] = stream.read() & 0xff -- ignoring the charset entirely. Real
    // JDK 25 InputStreamReader.read(char[],int,int) is real bytecode delegating to
    // sd.read(...) (a sun.nio.cs.StreamDecoder built via
    // StreamDecoder.forInputStreamReader(...), already correctly registered with
    // full charset support in native-io/src/stream_decoder.rs). This synthetic
    // block unconditionally shadowed that real bytecode for EVERY InputStreamReader
    // (not just genuinely-synthetic-stub ones) because the interpreter invokevirtual
    // vtable fast path (execute_invokevirtual_vtable_fast in interpreter.rs) treats
    // "any native registered for this (class, method, descriptor)" as an
    // authoritative shadow -- it does not consult the NativeKind::SyntheticStub
    // category the way vm_exec.rs::invoke_or_native real_protected_stub check does,
    // so tagging this SyntheticStub alone (tried first) did not help; only removing
    // it does. Broke every multi-byte decode through InputStreamReader, most
    // visibly UTF-16BE/LE (each 2-byte code unit decoded as two separate
    // Latin-1-ish chars).
    {
        registry.register(
            "java/util/jar/Attributes",
            "<init>",
            "()V",
            native_attrs_init, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/util/jar/Attributes",
            "<init>",
            "(I)V",
            native_attrs_init, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/util/jar/Attributes",
            "putValue",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            native_attrs_put_value, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/util/jar/Attributes",
            "getValue",
            "(Ljava/lang/String;)Ljava/lang/String;",
            native_attrs_get_value, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/util/jar/Attributes",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_attrs_get_object, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/util/jar/Attributes",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_attrs_put_object, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register("java/util/jar/Attributes", "size", "()I", native_attrs_size, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            "java/util/jar/Attributes",
            "entrySet",
            "()Ljava/util/Set;",
            native_attrs_entry_set, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/util/jar/Attributes$Name",
            "<init>",
            "(Ljava/lang/String;)V",
            native_attrs_name_init, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/util/jar/Attributes$Name",
            "toString",
            "()Ljava/lang/String;",
            native_attrs_name_to_string, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/util/jar/Attributes$Name",
            "equals",
            "(Ljava/lang/Object;)Z",
            native_attrs_name_equals, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/util/jar/Attributes$Name",
            "hashCode",
            "()I",
            native_attrs_name_hash_code, cratonvm_native_api::NativeKind::Bridge,
        );
        // `Attributes$Name.MANIFEST_VERSION` / `MAIN_CLASS` were registered
        // here with the FIELD descriptor and are gone (round 13 wave 13, lane
        // shadow5, SH4-2): no door can dispatch a field-shaped key, and they
        // were the losing half of a duplicate. `jar_manifest.rs`'s winning
        // half went too, with its two mint sites and their
        // `unconstructed_carrier_gate` baseline row (round 14 wave 2, lane
        // shadow).
        // Real-JDK BufferedReader owns its internal Reader/buffer fields.
    } // end #[cfg(feature = "synthetic-jdk")] synthetic Reader ctor shadows
      // Also wire the full R3 bundle in essential mode so readLine/lines/close
      // are available during early Maven/Surefire bootstrap.
    register_r3_resource_loading(registry);
    servlet::register_spring_mock_response_natives(registry);
    servlet::register_script_engine_manager_natives(registry);
    servlet::register_jython_thread_state_natives(registry);
    servlet::register_jython_pyobject_natives(registry);
    servlet::register_jython_imp_natives(registry);

    // S109 Wave3 — `java/lang/Module.canUse(Class)`. The real bytecode reads
    // `this.descriptor` (a real-Module field that does not exist on our
    // synthetic 2-field Module shape) and dereferences it, producing a
    // NullPointer deep inside `ServiceLoader.checkCaller(Class, Class)`
    // during `Console.instantiateConsole()` static-init. The override
    // returns true (every module is treated as permissively
    // `uses`-declared); null-receiver returns false; null service-class
    // throws NPE per spec.
    //
    // This registration alone does NOT protect real-JDK mode: real bytecode
    // wins over a registered native unless the triple is also listed in
    // `interpreter.rs::force_native_over_real_jdk_bytecode`, and `canUse` was
    // missing from that list until the WildFly Host Controller
    // `org.jboss.as.jmx` module-load NPE investigation added it alongside
    // `addUses` below (see that function's doc comment for the full trace:
    // `DeferredExtensionContext.load()` -> JDK-internal module helper code ->
    // `Module.canUse`/`addUses` on a named Module mirror with a null
    // `descriptor` field -> `NullPointerException: ... "this.descriptor" is
    // null`). Synthetic-jdk mode has no competing real bytecode, so this
    // native always took effect there — see vm/tests/wave3_console_module.rs.
    // KNOWN OVER-APPROXIMATION, measured and left standing 2026-08-28.
    //
    //   java.base.canUse(Runnable.class)
    //     HotSpot   false      CratonVM  true
    //
    // (`probes/ClassShadowSweep.java`, identical in both modes.) `canUse` is
    // true only when the module DECLARES `uses` for that service; an unnamed
    // module uses everything, a named one uses what its descriptor says. This
    // native answers true for any non-null service on any receiver.
    //
    // Not corrected here, and the reason is the comment above: this native
    // exists precisely because a named Module mirror can carry a NULL
    // `descriptor` field, which is the one thing a faithful implementation
    // would have to read. Consulting it means calling back into Java
    // (`getDescriptor().uses().contains(..)`) from inside the native written to
    // avoid touching that field — re-entrancy for one row, on the path
    // `ServiceLoader.checkCaller` takes during `Console.<clinit>`.
    //
    // What it costs meanwhile: an application on a NAMED module asking whether
    // it may use a service gets `true` where the reference VM says no, and then
    // loads a service its module descriptor did not declare. Application code
    // on the unnamed module — which is most code on this VM — is unaffected,
    // because an unnamed module genuinely can use anything.
    registry.register(
        "java/lang/Module",
        "canUse",
        "(Ljava/lang/Class;)Z",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first().copied() else {
                return Ok(Some(Value::Int(0)));
            };
            let Some(Value::Object(Some(service))) = args.get(1).copied() else {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            };
            // `canUse` is true iff the module DECLARES `uses` for that service.
            // An UNNAMED module uses everything; a NAMED one uses exactly what
            // its descriptor says.
            //
            // MEASURED (`probes/ClassShadowSweep.java`), both modes:
            //   java.base.canUse(Runnable.class)   HotSpot false   CratonVM true
            //
            // This used to answer `true` for any non-null service on any
            // receiver, and the note here said a faithful version would have to
            // call back into Java for `getDescriptor().uses()` -- the one thing
            // this native exists to avoid, because a named Module mirror can
            // carry a NULL `descriptor` field. That was wrong: the VM's own
            // module registry already holds the `uses` set, reachable with no
            // Java call and no re-entrancy, via `ctx.module_uses`.
            //
            // An unregistered or descriptor-less module yields an EMPTY uses
            // list, which would answer false for everything -- so the unnamed
            // case is decided first, and a named module with no registry entry
            // keeps the permissive answer rather than newly refusing work that
            // used to succeed.
            let module_name = crate::phases_late::read_module_name(ctx, this);
            if module_name.is_empty() {
                return Ok(Some(Value::Int(1)));
            }
            if !ctx.module_is_registered(&module_name) {
                return Ok(Some(Value::Int(1)));
            }
            let service_name = crate::lang_class::mirror_class_name(ctx, service)
                .unwrap_or_default()
                .replace('/', ".");
            let uses = ctx.module_uses(&module_name);
            let declared = uses.iter().any(|u| u.replace('/', ".") == service_name);
            return Ok(Some(Value::Int(i32::from(declared))));
            #[allow(unreachable_code)]
            {
                Ok(Some(Value::Int(1)))
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // `Module.getDescriptor()` is registered ~3,500 lines below, in this same
    // function — the `Module.getResourceAsStream` shape exactly (instance 3 of
    // the duplicate-registration species: same file, same function, thousands
    // of lines apart). `register` is last-write-wins, so the copy that used to
    // sit here never ran despite its comment claiming it was "the registration
    // path used by the CLI"; the CLI reached the later one.
    //
    // It was also the weaker body: it fabricated a descriptor for EVERY
    // module, including the unnamed one, where real `Module.getDescriptor()`
    // answers null; and it re-built the descriptor on every call instead of
    // memoising it onto the mirror's `descriptor` field, so two calls on one
    // module handed back two different `ModuleDescriptor` objects. Deleted
    // rather than reordered — the survivor covers this case.

    // `java/lang/Module.addUses(Class)` — companion to `canUse` above, same
    // null-descriptor gap (real bytecode reads `this.descriptor` to decide
    // whether the module already implicitly "uses" everything as an
    // automatic module before recording an explicit service-use edge).
    // CratonVM does not enforce `uses`/`provides` reachability beyond what
    // `canUse`/`getDescriptor` already answer permissively, so this is a
    // pure identity passthrough: validate the service-class argument (per
    // spec, NPEs on null) and hand back the receiver unchanged for
    // chaining. Force-listed alongside `canUse` in
    // `interpreter.rs::force_native_over_real_jdk_bytecode`.
    registry.register(
        "java/lang/Module",
        "addUses",
        "(Ljava/lang/Class;)Ljava/lang/Module;",
        |_ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(m))) => *m,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("Module.addUses: receiver is null".into()),
                    }
                    .into())
                }
            };
            match args.get(1) {
                Some(Value::Object(Some(_))) => Ok(Some(Value::Object(Some(this)))),
                _ => Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("Module.addUses: service class is null".into()),
                }
                .into()),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // `java/lang/Module.canRead(Module)` — same essential-vs-synthetic-jdk
    // coverage gap as `canUse` immediately above: `register_p59_module`
    // (native-builtins/src/phases_late.rs) ALSO registers this triple, but
    // that function is only reachable via the `synthetic-jdk`-feature-gated
    // `register_synthetic_overrides` — dead code in the default `cratonvm-cli`
    // build. Unlike `canUse`/`getDescriptor`, real bytecode doesn't NPE here;
    // it silently returns the wrong boolean (confirmed: `m.canRead(javaBase)`
    // returned `false` via real bytecode vs. `true` on real HotSpot — every
    // module implicitly reads `java.base`). Register the shared
    // `native_module_can_read` fn (phases_late.rs) here too so the correct,
    // readability-graph-backed answer is reachable in the default build.
    registry.register(
        "java/lang/Module",
        "canRead",
        "(Ljava/lang/Module;)Z",
        crate::phases_late::native_module_can_read, cratonvm_native_api::NativeKind::Bridge,
    );

    // `Module.addExports(String, Module)` / `addOpens(String, Module)` — same
    // essential-vs-synthetic-jdk gap, but worse than `canRead`: real bytecode
    // (`implAddExportsOrOpens`, Module.java) directly reads
    // `this.descriptor.isOpen()` (a field, not the overridden `getDescriptor()`
    // accessor), and that field is never populated — so a direct call NPEs
    // ("Cannot invoke ... because \"this.descriptor\" is null") instead of
    // silently returning a wrong answer. Register the shared
    // `native_module_add_exports`/`native_module_add_opens` fns
    // (phases_late.rs) here too so the mutation reaches the boot
    // `ModuleRegistry` without ever touching the null field.
    registry.register(
        "java/lang/Module",
        "addExports",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;",
        crate::phases_late::native_module_add_exports, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Module",
        "addOpens",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;",
        crate::phases_late::native_module_add_opens, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Module",
        "implAddExports",
        "(Ljava/lang/String;)V",
        crate::phases_late::native_module_impl_add_exports_all, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Module",
        "implAddExports",
        "(Ljava/lang/String;Ljava/lang/Module;)V",
        crate::phases_late::native_module_impl_add_exports_to_module, cratonvm_native_api::NativeKind::Bridge,
    );
    // NOT `native_module_impl_add_exports_all`: "to all unnamed modules" is a
    // qualified edge on HotSpot (`Module.isExported(pkg)` stays false after
    // it), and the `…_all` fn records the unqualified one. See
    // `native_module_impl_add_exports_to_all_unnamed`.
    registry.register(
        "java/lang/Module",
        "implAddExportsToAllUnnamed",
        "(Ljava/lang/String;)V",
        crate::phases_late::native_module_impl_add_exports_to_all_unnamed, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Module",
        "implAddExportsNoSync",
        "(Ljava/lang/String;)V",
        crate::phases_late::native_module_impl_add_exports_all, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Module",
        "implAddExportsNoSync",
        "(Ljava/lang/String;Ljava/lang/Module;)V",
        crate::phases_late::native_module_impl_add_exports_to_module, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Module",
        "implAddOpens",
        "(Ljava/lang/String;)V",
        crate::phases_late::native_module_impl_add_opens_all, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Module",
        "implAddOpens",
        "(Ljava/lang/String;Ljava/lang/Module;)V",
        crate::phases_late::native_module_impl_add_opens_to_module, cratonvm_native_api::NativeKind::Bridge,
    );
    // Qualified to the unnamed module, same as `implAddExportsToAllUnnamed`
    // above — not `native_module_impl_add_opens_all`.
    registry.register(
        "java/lang/Module",
        "implAddOpensToAllUnnamed",
        "(Ljava/lang/String;)V",
        crate::phases_late::native_module_impl_add_opens_to_all_unnamed, cratonvm_native_api::NativeKind::Bridge,
    );

    registry.register(
        "java/lang/String",
        "trim",
        "()Ljava/lang/String;",
        // JIT r22 wave 11, lane intrinsics: was a closure through
        // `read_string` (lone surrogates became U+FFFD) that always built a
        // new String; the canonical native is the one the later row uses.
        lang_string::native_string_trim, cratonvm_native_api::NativeKind::Bridge,
    );

    // Session 108 (Cluster D v2 — fix #2): java/lang/String.indexOf(String,int)
    // The JDK's URLClassPath(String, boolean) constructor uses
    // `cp.indexOf(File.pathSeparator, i)` to walk the classpath. The real-JDK
    // bytecode for this method funnels into `String.checkIndex` →
    // `Preconditions.checkIndex` and—if any internal layout/coder mismatch
    // occurs in real-JDK mode—throws an unhelpful, message-less
    // `StringIndexOutOfBoundsException` from inside `ClassLoaders.<clinit>`.
    // That cascade then surfaces as the Cluster-D-v2 silent-swallow we
    // observed in Session 107 (`exc=java/lang/StringIndexOutOfBoundsException`
    // with no detail). Registering an explicit, layout-neutral native here
    // routes the call through `read_string` and `find` over `&str`, which
    // never throws and matches OpenJDK semantics exactly:
    //   * fromIndex < 0 is clamped to 0 (per JDK spec)
    //   * fromIndex >= length returns -1
    //   * empty needle returns clamp(fromIndex, 0, length)
    //   * otherwise returns the char-index of the first occurrence at or
    //     after fromIndex, or -1.
    registry.register(
        "java/lang/String",
        "indexOf",
        "(Ljava/lang/String;I)I",
        // JIT r22 wave 11, lane intrinsics: this was a closure over `chars()`
        // (code-POINT indices and offsets, lone surrogates as U+FFFD, `-1`
        // for a null needle where HotSpot throws); the canonical native
        // (`lang_string.rs`, bug nb-lang-string / E18) answers UTF-16 units.
        // The kind is unchanged (this block's ambient one).
        lang_string::native_string_index_of_str_from, cratonvm_native_api::NativeKind::Bridge,
    );

    // Session 108 (Cluster D v2 — fix #1): java/io/Console.istty()Z native.
    // The real-JDK `java.io.Console.<clinit>` calls `istty()` to detect a
    // controlling terminal. CratonVM has no JNI binding for this (and we are
    // never a TTY anyway — stdin is always non-interactive in our embedding),
    // so the missing native dispatch in `vm_exec` raises an
    // `UnsatisfiedLinkError` that gets swallowed at <clinit>. Returning
    // `false` mirrors what HotSpot reports when stdin is redirected, which
    // is the correct answer for any non-interactive embedding (CLI tools,
    // CI smoke tests, container entrypoints). The Console object then
    // initialises in its "no-tty" mode and downstream callers (e.g.
    // `System.console()`) gracefully return null without further error.
    // W3: answer from the host instead of hard-coding "never a tty". HotSpot's
    // `Java_java_io_Console_istty` is `isatty(stdin) && isatty(stdout)`; the
    // std `IsTerminal` impl is the same syscall (`isatty` / `GetConsoleMode`).
    // Redirected/piped/daemon launches still get `false` — but an interactive
    // launch now gets the truthful answer.

    // JDK 25 residual (classutils-forname-platform-loader-false-positive.md):
    // JDK 25's `java.io.Console` no longer has `istty()Z` at all -- it was
    // replaced by `private static native int ttyStatus()`, called once from
    // `<clinit>` and stashed in a static `ttyStatus` field that
    // `isStdinTty()`/`isStdoutTty()`/`isStderrTty()` later mask against
    // `TTY_STDIN_MASK`/`TTY_STDOUT_MASK`/`TTY_STDERR_MASK`. Without this
    // native, `<clinit>` throws `UnsatisfiedLinkError`, which normally gets
    // swallowed as an ExceptionInInitializerError the first time anything
    // touches `Console` -- but Mockito's `InlineBytecodeGenerator` calls
    // `Class.forName` on the target class before mocking it precisely so it
    // can surface a clean `MockitoException` instead of a confusing NCDFE,
    // and that surfaces the swallowed link error as
    // "Mockito cannot mock this class: class java.io.Console", misdiagnosed
    // in the known-issue doc above as an unfixable Mockito/Console
    // limitation. Returning 0 (no bit set) mirrors `istty()`'s `false`: every
    // stream reports non-tty, matching HotSpot's behavior for CratonVM's
    // always-non-interactive embedding, and lets `DefaultLogbackConfigurationTests
    // .consoleLogCharsetShouldUseConsoleCharsetIfConsoleAvailable` (which mocks
    // `Console` directly, forcing real `<clinit>` to run) initialize cleanly.
    // W3: same treatment as `istty()` above — report the real per-stream state.
    // Bit layout matches `java.io.Console`: 0b001 stdin, 0b010 stdout,
    // 0b100 stderr (TTY_STDIN_MASK / TTY_STDOUT_MASK / TTY_STDERR_MASK).
    registry.register(
        "java/io/Console",
        "ttyStatus",
        "()I",
        |_ctx, _args| {
            use std::io::IsTerminal;
            let mut status = 0i32;
            if std::io::stdin().is_terminal() {
                status |= 0b001;
            }
            if std::io::stdout().is_terminal() {
                status |= 0b010;
            }
            if std::io::stderr().is_terminal() {
                status |= 0b100;
            }
            Ok(Some(Value::Int(status)))
        },
        NativeKind::Bridge,
    );

    // nontty-console: companion to `Console.istty()` for the modern
    // (JDK 22+) console path. In JDK 25 the only two natives across the
    // entire `System.console()` chain are:
    //   1. `java/io/Console.istty()Z`                       (above)
    //   2. `jdk/internal/io/JdkConsoleImpl.echo(Z)Z`        (here)
    //
    // `Console.instantiateConsole()` reads the `istty` field (seeded by the
    // native above): when it is `false` — our default for every redirected /
    // piped / daemon stdout — the `if (istty && cons == null)` arm that would
    // `new JdkConsoleImpl(...)` is SKIPPED, so `System.console()` returns
    // `null` exactly like HotSpot on a non-TTY. That is the common path and it
    // works with `istty()` alone.
    //
    // `echo(boolean)` is only reached when a `JdkConsoleImpl` actually exists
    // (i.e. `istty == true`) and the program calls `readLine`/`readPassword`,
    // which toggle terminal echo around the read. If `istty()` ever reports
    // `true` for an embedding (an interactive launch, or a future change that
    // wires real TTY detection) WITHOUT this native, the `echo()` call hits the
    // missing-native path in `vm_exec` mid-read — an `UnsatisfiedLinkError`
    // raised deep inside `JdkConsoleImpl.readline()` that is NOT at a
    // <clinit>/swallow boundary, manifesting as the picocli/JLine/Boot-logger
    // "probe System.console() then crash/hang" symptom (the daemon-gating bug).
    //
    // The OpenJDK Windows/Unix native flips the OS echo flag via a console
    // ioctl and returns the PREVIOUS echo state. CratonVM has no JNI binding
    // and must never block on a controlling-terminal ioctl, so we model a
    // non-interactive terminal: echo is treated as already in the requested
    // state, the call is a pure no-op, and we return the requested flag (`on`).
    // This is non-blocking, never throws (the `throws IOException` is unused),
    // and keeps `readLine`/`readPassword` from faulting if a Console is ever
    // instantiated. Args: [0] = boolean `on` (this is a STATIC native, so there
    // is no receiver in args).
    registry.register(
        "jdk/internal/io/JdkConsoleImpl",
        "echo",
        "(Z)Z",
        |_ctx, args| {
            let on = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            // Return the requested state (== "previous state was already this"):
            // a faithful no-op for a non-interactive terminal that never
            // touches a real tty ioctl.
            Ok(Some(Value::Int(on)))
        },
        NativeKind::Bridge,
    );

    registry.register(
        "java/lang/String",
        "toString",
        "()Ljava/lang/String;",
        |_ctx, args| match args.first() {
            Some(Value::Object(Some(o))) => Ok(Some(Value::Object(Some(*o)))),
            _ => Ok(Some(Value::Object(None))),
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Real-JDK-mode Inflater/Deflater natives backed by flate2. Registered
    // unconditionally; in synthetic-jdk mode the phase71 overrides run after
    // register_synthetic_overrides and supersede these.
    zip_real::register_zip_real_natives(registry);

    // Kafka third-party compression codecs: snappy-java block natives + zstd-jni
    // streaming natives (see compression_native.rs). Registered in the same
    // Bridge category as the zip_real natives above.
    compression_native::register_compression_natives(registry);

    // java.util.zip.CRC32C — Java-method overrides for the Castagnoli CRC-32C.
    // CRC32C has NO native methods in JDK 25 (pure-Java, Unsafe-backed hot
    // loop); these overrides give a bit-exact CRC-32C and a stable differential
    // oracle for a future JIT CRC32C.update intrinsic. See zip_crc32c.rs.
    zip_crc32c::register_crc32c_natives(registry);

    // NOTE: apps_h2 per-method Thread.getState/getPriority/isDaemon
    // overrides were removed once `current_thread_object` began
    // populating `Thread.holder:FieldHolder` on VM-created Thread
    // objects.  Real-JDK bytecode now resolves `this.holder.threadStatus`
    // through the constructed FieldHolder instead of NPE-ing.
    // The module is kept (unregistered) so the code is available if a
    // regression reintroduces the holder-null path.
    let _ = apps_h2::register_apps_h2_overrides;

    // C42: H2 TableFilter.prepare() NPE workaround — ensures
    // `this.index` is non-null before it is dereferenced.
    apps_h2::register_h2_table_filter_prepare(registry);
    // Hibernate FunctionTests reaches H2's parser cursor path during schema
    // create/drop. These bytecode-equivalent parser/token intrinsics keep that
    // path from spending most of the watchdog window in tiny interpreted
    // accessors.
    apps_h2::register_h2_parser_fastpaths(registry);

    // App-shim seam: JDBC connection pools (Agroal / IronJacamar), the
    // Infinispan local cache, the Vert.x event loop and the WildFly
    // datasources + Narayana JTA glue. Grouped into `crate::app_shims`; the
    // `CRATONVM_REAL_AGROAL` / `CRATONVM_REAL_VERTX` escapes moved with it, so
    // the registered set is unchanged. NOTE: this family also registers
    // `javax/sql/DataSource`, so it is not severable as-is — see
    // `ShimFamily::jdk_entanglements`.
    if shim_selection.includes(crate::app_shims::ShimFamily::DataSourcePools) {
        crate::app_shims::register_datasource_pool_shims(registry);
    }
    // T19.H2: StackWalker boot-time getInstance variants + getCallerClass.
    stack_walker::register_stack_walker_boot(registry);
    // WildFly's security manager can hit StackWalker.walk(Function) before the
    // late StackWalker phase; use the same implementation earlier.
    crate::phases_late::register_p59_stackwalker(registry);
    // Logging bootstrap can see synthetic LogRecord/EnumMap before the normal
    // phase-50/54 registrations are installed. Only `<init>` is registered
    // here — see `register_enum_map_init_native`'s doc comment for why the
    // rest of `register_enum_map_natives` must stay synthetic-mode-only.
    crate::phases_early::register_enum_map_init_native(registry);
    crate::phases_early::register_phase54_logging_extras(registry);
    // JBoss Modules can resolve ThreadLocal as a synthetic real-JDK stub during
    // early module bootstrap, but its withInitial body lives in phase 50.
    crate::phases_early::register_thread_local_natives(registry);
    // Domain process-controller boot can resolve InetSocketAddress through a
    // synthetic stub before phase 52; expose the holder-shaped constructors
    // and accessors in essentials as well.
    crate::phases_early::register_phase52_inet_socket_address(registry);
    // ThreadGroup is normally late-phase, but process-controller bootstrap
    // builds a JBossThreadFactory before those late registrations are enough.
    crate::phases_late::register_p71_thread_extras(registry);
    // ProcessController.addProcess encodes auth material through
    // Base64.getEncoder() while running under JBoss Modules; expose the existing
    // Base64 intrinsics in essentials instead of waiting for the full phase table.
    register_base64_natives(registry);
    registry.register("java/util/Arrays", "equals", "([B[B)Z", |ctx, args| {
        let a_ref = match args.first() {
            Some(Value::Object(o)) => *o,
            _ => None,
        };
        let b_ref = match args.get(1) {
            Some(Value::Object(o)) => *o,
            _ => None,
        };
        if a_ref == b_ref {
            return Ok(Some(Value::Int(1)));
        }
        let (Some(a), Some(b)) = (a_ref, b_ref) else {
            return Ok(Some(Value::Int(0)));
        };
        let len = ctx.array_length(a);
        if len != ctx.array_length(b) {
            return Ok(Some(Value::Int(0)));
        }
        for i in 0..len {
            let av = match ctx.get_array_element(a, i) {
                Value::Int(v) => v as i8,
                Value::Long(v) => v as i8,
                _ => 0,
            };
            let bv = match ctx.get_array_element(b, i) {
                Value::Int(v) => v as i8,
                Value::Long(v) => v as i8,
                _ => 0,
            };
            if av != bv {
                return Ok(Some(Value::Int(0)));
            }
        }
        Ok(Some(Value::Int(1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register("java/util/Arrays", "hashCode", "([B)I", |ctx, args| {
        let arr = match args.first() {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        let mut hash = 1i32;
        for i in 0..len {
            let elem_hash = match ctx.get_array_element(arr, i) {
                Value::Int(v) => v as i8 as i32,
                Value::Long(v) => v as i8 as i32,
                _ => 0,
            };
            hash = hash.wrapping_mul(31).wrapping_add(elem_hash);        }
        Ok(Some(Value::Int(hash)))    }, cratonvm_native_api::NativeKind::Bridge);
    // DIAGNOSTIC (temporary, CRATONVM_TRACE_ARRAYS_HASHCODE-gated):
    // `createLayoutFromConfigClass` (Thymeleaf/Groovy MetaClass hang, see
    // thymeleaf-groovy-layoutdialect-metaclass-introspection-hang-FIXED.md)
    // hangs permanently inside real bytecode `jdk.internal.util.ArraysSupport
    // .hashCode(Object[], int, int, int)`, called from `Arrays.hashCode` on
    // a `ParameterizedTypeImpl`'s `actualTypeArguments`. This native
    // override reads the SAME array via `ctx.array_length()`/`get_array_element`
    // (bypassing whatever real-bytecode interpretation issue may be at
    // fault) so the trace print reveals the array's ACTUAL length at the
    // moment of the hang — if that's small and sane, the bug is an
    // interpreter defect in the specific unrolled/vectorized-adjacent
    // bytecode shape, not data corruption; if it's huge/garbage, the bug is
    // upstream in how the array was built or GC-relocated. Left registered
    // unconditionally (not gated on the trace flag) because if it resolves
    // the hang outright, that's evidence this IS the fix, not just a probe.
    registry.register(
        "java/util/Arrays",
        "hashCode",
        "([Ljava/lang/Object;)I",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let arr_pin = ctx.pin_native_root(arr);
            let len = ctx.array_length(ctx.read_native_pin(arr_pin, arr));
            let trace = crate::nbflags().trace_arrays_hashcode;
            if trace {
                let mut desc = String::new();
                for i in 0..len {
                    if i > 0 {
                        desc.push_str(", ");
                    }
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    match ctx.get_array_element(arr, i) {
                        Value::Object(None) => desc.push_str("null"),
                        Value::Object(Some(o)) => {
                            let cid = ctx.class_id_of_object(o);
                            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
                            let ident = ctx.identity_hash_code(o);
                            // If it's a TypeVariable (real or our synthetic
                            // interface-typed stand-in), also show its `name`
                            // field so distinct-identity-but-same-conceptual-
                            // variable recurrence is visible directly, not just
                            // inferred from the identity hash changing. Our
                            // synthetic stand-ins (`alloc_concurrent_synthetic`
                            // on the bare `java/lang/reflect/TypeVariable`
                            // INTERFACE) have no real field-name table --
                            // `get_field_by_name` can't resolve "name" on
                            // them -- so read field 0 POSITIONALLY (the
                            // documented layout: 0=name, 1=bounds,
                            // 2=genericDeclaration) as well as the by-name
                            // path (covers real TypeVariableImpl objects,
                            // whose real field is NOT named "name" either --
                            // print raw field-0 content either way for a
                            // real repr fallback).
                            let tv_name = if cname.contains("TypeVariable") {
                                match ctx.get_field_by_name(o, "name") {
                                    Value::Object(Some(s)) => ctx.read_string(s),
                                    _ => match ctx.get_field(o, 0) {
                                        Value::Object(Some(s)) => ctx.read_string(s),
                                        _ => None,
                                    },
                                }
                            } else {
                                None
                            };
                            // Also show the genericDeclaration (field 2 for
                            // our synthetic layout) so we can tell which
                            // class/method declared this variable.
                            let decl_desc = if cname.contains("TypeVariable") {
                                match ctx.get_field(o, 2) {
                                    Value::Object(Some(d)) => {
                                        let dcid = ctx.class_id_of_object(d);
                                        let dcname = ctx.class_name_of_id(dcid).unwrap_or_default();
                                        // getName() on a Class/Method/Constructor
                                        // mirror reveals the actual declaration.
                                        match ctx.invoke_virtual(
                                            d,
                                            "getName",
                                            "()Ljava/lang/String;",
                                            &[],
                                        ) {
                                            Ok(Some(Value::Object(Some(s)))) => {
                                                ctx.read_string(s).map(|n| format!("{dcname}:{n}"))
                                            }
                                            _ => Some(dcname),
                                        }
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            match (tv_name, decl_desc) {
                                (Some(n), Some(d)) => desc
                                    .push_str(&format!("{cname}@{ident:x}(name={n:?},decl={d})")),
                                (Some(n), None) => {
                                    desc.push_str(&format!("{cname}@{ident:x}(name={n:?})"))
                                }
                                (None, Some(d)) => {
                                    desc.push_str(&format!("{cname}@{ident:x}(decl={d})"))
                                }
                                (None, None) => desc.push_str(&format!("{cname}@{ident:x}")),
                            }
                        }
                        _ => desc.push_str("?"),
                    }
                }
                eprintln!("ARRAYS-HASHCODE-TRACE: Object[] hashCode len={len} elems=[{desc}]");
            }
            let mut hash = 1i32;
            for i in 0..len {
                let arr = ctx.read_native_pin(arr_pin, arr);
                let elem_hash = match ctx.get_array_element(arr, i) {
                    Value::Object(None) => 0,
                    Value::Object(Some(o)) => match ctx.invoke_virtual(o, "hashCode", "()I", &[]) {
                        Ok(Some(Value::Int(h))) => h,
                        _ => 0,
                    },
                    _ => 0,
                };
                hash = hash.wrapping_mul(31).wrapping_add(elem_hash);
            }
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Int(hash)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // The process controller starts the Host Controller via ProcessBuilder
    // before the regular phase-57 table is visible in this module path.
    crate::phases_late::register_phase57_process(registry);
    registry.register("java/io/FileDescriptor", "<init>", "()V", |ctx, args| {
        // The real `FileDescriptor()` body is `fd = -1; handle = -1;` — NOT
        // empty. A no-op left both slots at their zero-initialized value, and
        // fd == 0 is stdin: a freshly constructed FileDescriptor reported
        // `valid() == true` and every fd-indexed native (close0/sync0/
        // getHandle) that fell back to reading the `fd` field addressed
        // standard input. Write the real sentinels instead. `handle` only
        // exists on the Windows JDK's FileDescriptor; `set_field_by_name` is
        // a no-op when the field is absent, so this is safe on both.
        let this = obj_arg(args, 0)?;
        ctx.set_field_by_name(this, "fd", Value::Int(-1));
        ctx.set_field_by_name(this, "handle", Value::Long(-1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // `SyntheticStub`, stated, for the same reason as the `FilterOutputStream`
    // constructor above: the shim assigns `fd` and stops, while the real
    // constructor also initializes `private final Object closeLock = new
    // Object()`, and `FileInputStream.close()` / `FileOutputStream.close()`
    // open with `synchronized (closeLock)`. Any stream built through this shim
    // therefore throws NPE — not IOException — on its first close.
    //
    // Reached from `ProcessImpl`'s `ProcessPipeInputStream(fd)`, whose
    // `close()` chains down to `FileInputStream.close`; that was the second
    // null `closeLock` in the same `Process.destroy()`, and it is the same
    // defect on the input side.
    //
    // Compatible mode keeps the shim, where the receiver may carry the
    // synthetic 1-slot layout the `set_field(this, 0, ..)` writes through.
    // Strict mode drops it and runs the real constructor, which additionally
    // sets `path`/`append` and calls `fd.attach(this)`; the fd-table id still
    // arrives through the `FileDescriptor`'s own `fd` field, which is where
    // `fis_get_fd`/`fos_get_fd` look first.
    for stream_class in ["java/io/FileInputStream", "java/io/FileOutputStream"] {
        registry.register(
            stream_class,
            "<init>",
            "(Ljava/io/FileDescriptor;)V",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(None),
                };
                let fd = args.get(1).copied().unwrap_or(Value::Object(None));
                ctx.set_field_by_name(this, "fd", fd);
                ctx.set_field(this, 0, fd);
                // Interpreter round i3 wave 80, lane L6: the real
                // constructor's `closeLock`, whose absence is the NPE this
                // note describes (`ProcessImpl` under
                // `CRATONVM_COMPAT_JDK_PROCESS`, measured on the host).
                cratonvm_native_io::backfill_close_lock(ctx, this);
                Ok(None)
            },
            cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
    // Retired under `--real-jdk` (re-tagged `SyntheticStub` by the registry:
    // `RETIRED_SHADOW_OUTPUT_STREAM_WRITE_ALL_TRIPLES`, round 14 wave 2); the
    // real body is the same `write(b, 0, b.length)`.
    registry.register(
        "java/io/OutputStream",
        "write",
        "([B)V",
        native_output_stream_write_all, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/FilterInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let input = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "in", input);
            ctx.set_field(this, 0, input);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/io/FilterInputStream", "skip", "(J)J", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(object))) => *object,
            _ => return Ok(Some(Value::Long(0))),
        };
        let requested = args.get(1).and_then(|value| value.as_long()).unwrap_or(0);
        Ok(Some(Value::Long(filter_input_stream_skip(
            ctx, this, requested,
        )?)))
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/io/FilterOutputStream",
        "write",
        "([B)V",
        native_output_stream_write_all, cratonvm_native_api::NativeKind::Bridge,
    );
    // Synthetic-JDK builds only. In real-JDK mode this OutputStreamWriter
    // surface (added 2026-07-09 by the WildFly process-controller bootstrap
    // batch, b448f2039) shadowed the real OSW bytecode at every dispatch
    // site (WP0.1 native-override-priority) and REGRESSED the StreamEncoder
    // commit-threshold fix (1773d3df2, dohead-streamencoder-eager-flush-commit-threshold-FIXED.md):
    // `write_bytes_from_output_stream_writer` encodes every `write()` call
    // straight to the wrapped stream — one underlying `write([BII)` per
    // Writer call instead of real StreamEncoder's 512-byte batches — which
    // broke `NoBodyOutputStream.checkCommit`'s byte-count commit threshold
    // again (Tomcat `TestHttpServletDoHead*`: the 8 useLegacy+useWriter+FULL
    // params fail `expected:<2> but was:<3>` / GET has content-length while
    // HEAD goes chunked). It also encodes with `String::into_bytes()` — i.e.
    // hard-coded UTF-8, ignoring the writer's charset — and its `<init>`
    // native writes the OutputStream into raw slot 0 (real layout:
    // `Writer.writeBuffer`) while never creating the `se` StreamEncoder the
    // real bytecode needs, breaking the CharsetEncoder-ctor delegation
    // workaround above too. Real-JDK mode runs the real OSW bytecode →
    // `sun.nio.cs.StreamEncoder` shim (native-io/src/stream_encoder.rs),
    // which was validated byte-for-byte against HotSpot's flush granularity.
    // Same gating precedent as the BufferedInputStream block below.
    //
    // 2026-08-12 (W7-50): every word of the paragraph above is about real-JDK
    // MODE, but `cfg!(feature = "synthetic-jdk")` tests the BUILD. This
    // function is reached from BOTH arms of `vm_init` -- directly at the
    // real-JDK arm, and via `register_builtins` -> `register_essential_natives`
    // at the synthetic arm -- so in a `--features synthetic-jdk` binary the
    // cfg was true in real-JDK mode too and this surface shadowed the real OSW
    // bytecode exactly as described. The `(OutputStream, Charset)` descriptor
    // below is registered ONLY here, and it is the one JDK 25's `PrintStream`
    // ctor calls for `charOut`, so this block alone was enough to leave
    // `PrintStream.textOut` unusable and `checkError()` true (`RJdkHello`).
    // `drops_real_layout_synthetic()` is the mode-accurate guard: false in the
    // synthetic arm, true in both real-JDK arms, in both builds.
    if cfg!(feature = "synthetic-jdk") && !registry.drops_real_layout_synthetic() {
        for descriptor in [
            "(Ljava/io/OutputStream;)V",
            "(Ljava/io/OutputStream;Ljava/nio/charset/Charset;)V",
            "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        ] {
            registry.register(
                "java/io/OutputStreamWriter",
                "<init>",
                descriptor,
                native_output_stream_writer_init, cratonvm_native_api::NativeKind::Bridge,
            );
        }
        registry.register(
            "java/io/OutputStreamWriter",
            "write",
            "(I)V",
            native_output_stream_writer_write_int, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/OutputStreamWriter",
            "write",
            "([CII)V",
            native_output_stream_writer_write_chars, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/OutputStreamWriter",
            "write",
            "(Ljava/lang/String;)V",
            native_output_stream_writer_write_string, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/OutputStreamWriter",
            "write",
            "(Ljava/lang/String;II)V",
            native_output_stream_writer_write_string_range, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/OutputStreamWriter",
            "flush",
            "()V",
            native_output_stream_writer_flush, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/OutputStreamWriter",
            "close",
            "()V",
            native_output_stream_writer_close, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // Real JDK BufferedInputStream has a layout and close protocol that the
    // old synthetic bridge cannot emulate safely.  In particular, dispatching
    // its `skip`/`ensureOpen` path through this bridge leaves `buf` looking
    // closed while Jandex indexes a class stream.  Keep the bridge only for
    // synthetic-JDK builds; real-JDK execution must use the class bytecode.
    //
    // 2026-08-12 (W7-50): "real-JDK execution" is a MODE, and the cfg tests
    // the BUILD -- same wrong guard as the OutputStreamWriter block above,
    // which cites this one as its precedent. Corrected together so the
    // precedent is the right shape for the next reader. Not implicated in any
    // of the six W7-50 vectors; fixed because leaving one of two identical
    // wrong guards in one function is how a family gets re-opened.
    if cfg!(feature = "synthetic-jdk") && !registry.drops_real_layout_synthetic() {
        registry.register(
            "java/io/BufferedInputStream",
            "<init>",
            "(Ljava/io/InputStream;)V",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(None),
                };
                let input = args.get(1).copied().unwrap_or(Value::Object(None));
                buffered_input_stream_init(ctx, this, input);
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/BufferedInputStream",
            "<init>",
            "(Ljava/io/InputStream;I)V",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(None),
                };
                let input = args.get(1).copied().unwrap_or(Value::Object(None));
                buffered_input_stream_init(ctx, this, input);
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register("java/io/BufferedInputStream", "read", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(-1))),
            };
            Ok(Some(Value::Int(buffered_input_stream_read_one(ctx, this)?)))
        }, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            "java/io/BufferedInputStream",
            "read",
            "([BII)I",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(-1))),
                };
                let arr = match args.get(1) {
                    Some(Value::Object(Some(arr))) => *arr,
                    _ => return Ok(Some(Value::Int(-1))),
                };
                let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
                let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
                if len == 0 {
                    return Ok(Some(Value::Int(0)));
                }
                let arr_len = ctx.array_length(arr);
                if off >= arr_len {
                    return Ok(Some(Value::Int(-1)));
                }
                let limit = len.min(arr_len - off);
                // With no active mark/replay state, BufferedInputStream may pass a
                // large read straight to its delegate. This is the common Jandex
                // class-file path and avoids turning every bulk read into thousands
                // of single-byte virtual calls.
                if buffered_input_stream_can_delegate_bulk(ctx, this) {
                    if let Some(input) = buffered_input_stream_input(ctx, this) {
                        return Ok(ctx
                            .invoke_virtual(
                                input,
                                "read",
                                "([BII)I",
                                &[
                                    Value::Object(Some(arr)),
                                    Value::Int(off as i32),
                                    Value::Int(limit as i32),
                                ],
                            )?
                            .or(Some(Value::Int(-1))));
                    }
                }
                let mut read = 0usize;
                // GC-safety: `buffered_input_stream_read_one` dispatches the
                // delegate's `read()` -- real bytecode -- once per byte, and
                // both the stream and the destination array are carried in
                // from outside the loop.
                let this_pin = ctx.pin_native_root(this);
                let arr_pin = ctx.pin_native_root(arr);
                for i in 0..limit {
                    let this = ctx.read_native_pin(this_pin, this);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    let b = buffered_input_stream_read_one(ctx, this)?;
                    if b < 0 {
                        break;
                    }
                    ctx.set_array_element(arr, off + i, Value::Int(b as i8 as i32));
                    read += 1;
                }
                Ok(Some(Value::Int(if read == 0 { -1 } else { read as i32 })))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/BufferedInputStream",
            "mark",
            "(I)V",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(None),
                };
                let limit = args.get(1).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
                buffered_input_stream_mark(ctx, this, limit);
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/BufferedInputStream",
            "reset",
            "()V",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Err(buffered_input_stream_invalid_mark()),
                };
                if !buffered_input_stream_rewind(ctx, this) {
                    return Err(buffered_input_stream_invalid_mark());                }
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        // KEEP: correct constant. `BufferedInputStream.markSupported()` is
        // `return true;` in the JDK, and the mark/reset natives registered
        // just above implement it for real via the side table.
        registry.register(
            "java/io/BufferedInputStream",
            "markSupported",
            "()Z",
            |_ctx, _args| Ok(Some(Value::Int(1))), cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/BufferedInputStream",
            "available",
            "()I",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let input = match ctx.get_field_by_name(this, "in") {
                    Value::Object(Some(o)) => Some(o),
                    _ => match ctx.get_field(this, 0) {
                        Value::Object(Some(o)) => Some(o),
                        _ => None,
                    },
                };
                match input {
                    Some(input) => Ok(ctx
                        .invoke_virtual(input, "available", "()I", &[])?
                        .or(Some(Value::Int(0)))),
                    None => Ok(Some(Value::Int(0))),
                }
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            "java/io/BufferedInputStream",
            "close",
            "()V",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(None),
                };
                let input = match ctx.get_field_by_name(this, "in") {
                    Value::Object(Some(o)) => Some(o),
                    _ => match ctx.get_field(this, 0) {
                        Value::Object(Some(o)) => Some(o),
                        _ => None,
                    },
                };
                if let Some(input) = input {
                    let _ = ctx.invoke_virtual(input, "close", "()V", &[])?;                }
                Ok(None)
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    registry.register(
        "org/apache/catalina/connector/Response",
        "toAbsolute",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_response_to_absolute, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/tomcat/util/buf/CharChunk",
        "toString",
        "()Ljava/lang/String;",
        native_char_chunk_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/tomcat/util/buf/CharChunk",
        "equals",
        "(Ljava/lang/String;)Z",
        native_char_chunk_equals_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/tomcat/util/buf/CharChunk",
        "equalsIgnoreCase",
        "(Ljava/lang/String;)Z",
        native_char_chunk_equals_ignore_case, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/tomcat/util/buf/CharChunk",
        "startsWith",
        "(Ljava/lang/String;)Z",
        native_char_chunk_starts_with, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/tomcat/util/buf/CharChunk",
        "startsWithIgnoreCase",
        "(Ljava/lang/String;I)Z",
        native_char_chunk_starts_with_ignore_case, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/tomcat/util/buf/MessageBytes",
        "toChars",
        "()V",
        native_message_bytes_to_chars, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/apache/tomcat/util/buf/MessageBytes",
        "toString",
        "()Ljava/lang/String;",
        native_message_bytes_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
    // Tomcat request-mapping shadows. Opt out with
    // `CRATONVM_TOMCAT_MAPPER_NATIVES=0` to run Tomcat's own `Mapper` bytecode
    // instead — the shadows re-implement real, present Java code, so the
    // bytecode path is the reference, and being able to A/B them is how
    // `TestMapperPerformance` (an ABSOLUTE 5 s budget for 10^6 `map()` calls)
    // gets attributed to the right side of the boundary. See known-issue
    // tomcat/04.
    if mapper_natives_enabled() {
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "find",
        "([Lorg/apache/catalina/mapper/Mapper$MapElement;Lorg/apache/tomcat/util/buf/CharChunk;)I",
        native_mapper_find_chunk, cratonvm_native_api::NativeKind::Bridge,
    );
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "find",
        "([Lorg/apache/catalina/mapper/Mapper$MapElement;Lorg/apache/tomcat/util/buf/CharChunk;II)I",
        native_mapper_find_chunk_range, cratonvm_native_api::NativeKind::Bridge,
    );
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "findIgnoreCase",
        "([Lorg/apache/catalina/mapper/Mapper$MapElement;Lorg/apache/tomcat/util/buf/CharChunk;)I",
        native_mapper_find_ignore_case, cratonvm_native_api::NativeKind::Bridge,
    );
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "findIgnoreCase",
        "([Lorg/apache/catalina/mapper/Mapper$MapElement;Lorg/apache/tomcat/util/buf/CharChunk;II)I",
        native_mapper_find_ignore_case_range, cratonvm_native_api::NativeKind::Bridge,
    );
        registry.register(
            "org/apache/catalina/mapper/Mapper",
            "find",
            "([Lorg/apache/catalina/mapper/Mapper$MapElement;Ljava/lang/String;)I",
            native_mapper_find_string, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "exactFind",
        "([Lorg/apache/catalina/mapper/Mapper$MapElement;Lorg/apache/tomcat/util/buf/CharChunk;)Lorg/apache/catalina/mapper/Mapper$MapElement;",
        native_mapper_exact_find_chunk, cratonvm_native_api::NativeKind::Bridge,
    );
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "exactFindIgnoreCase",
        "([Lorg/apache/catalina/mapper/Mapper$MapElement;Lorg/apache/tomcat/util/buf/CharChunk;)Lorg/apache/catalina/mapper/Mapper$MapElement;",
        native_mapper_exact_find_ignore_case, cratonvm_native_api::NativeKind::Bridge,
    );
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "exactFind",
        "([Lorg/apache/catalina/mapper/Mapper$MapElement;Ljava/lang/String;)Lorg/apache/catalina/mapper/Mapper$MapElement;",
        native_mapper_exact_find_string, cratonvm_native_api::NativeKind::Bridge,
    );
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "map",
        "(Lorg/apache/tomcat/util/buf/MessageBytes;Lorg/apache/tomcat/util/buf/MessageBytes;Ljava/lang/String;Lorg/apache/catalina/mapper/MappingData;)V",
        native_mapper_map, cratonvm_native_api::NativeKind::Bridge,
    );
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "internalMap",
        "(Lorg/apache/tomcat/util/buf/CharChunk;Lorg/apache/tomcat/util/buf/CharChunk;Ljava/lang/String;Lorg/apache/catalina/mapper/MappingData;)V",
        native_mapper_internal_map, cratonvm_native_api::NativeKind::Bridge,
    );
        registry.register(
        "org/apache/catalina/mapper/Mapper",
        "internalMapWildcardWrapper",
        "([Lorg/apache/catalina/mapper/Mapper$MappedWrapper;ILorg/apache/tomcat/util/buf/CharChunk;Lorg/apache/catalina/mapper/MappingData;)V",
        native_mapper_internal_map_wildcard_wrapper, cratonvm_native_api::NativeKind::Bridge,
    );
    } // end mapper_natives_enabled()
      // `SyntheticStub`, stated. This shim sets `out` and nothing else, but the
      // real constructor also initializes `private final Object closeLock = new
      // Object()` — and `FilterOutputStream.close()` begins with
      // `synchronized (closeLock)`. Every subclass built through this shim
      // therefore throws NPE on its first `close()`; the JDK's own
      // `ProcessImpl.destroy` is one such caller, and its
      // `catch (IOException ignored)` does not catch it. See the matching note on
      // the `BufferedOutputStream` constructors in `native-io`.
      //
      // Strict mode drops it and runs the two-line real constructor. Compatible
      // mode, where the receiver may have the synthetic 1-slot layout this writes
      // through `set_field(this, 0, ..)`, keeps it.
    registry.register(
        "java/io/FilterOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let output = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "out", output);
            ctx.set_field(this, 0, output);
            // Interpreter round i3 wave 80, lane L6: the `closeLock` the note
            // above is about (`cratonvm_native_io::backfill_close_lock`).
            cratonvm_native_io::backfill_close_lock(ctx, this);
            Ok(None)
        },
        cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        "java/io/FilterOutputStream",
        "write",
        "(I)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let output = match ctx.get_field_by_name(this, "out") {
                Value::Object(Some(o)) => Some(o),
                _ => match ctx.get_field(this, 0) {
                    Value::Object(Some(o)) => Some(o),
                    _ => None,
                },
            };
            // `new FilterOutputStream(null)` is legal -- the constructor stores
            // whatever it is given -- and the FIRST write is where the JDK
            // fails, on `out.write(b)`. `if let Some(..)` turned that into a
            // no-op, so a stream with nowhere to write accepted every byte and
            // reported success.
            //
            // MEASURED with `L4TailSweep2.java` (`new
            // FilterOutputStream(null).write(1)`).
            let Some(output) = output else {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            };
            {
                let _ = ctx.invoke_virtual(
                    output,
                    "write",
                    "(I)V",
                    &[args.get(1).copied().unwrap_or(Value::Int(0))],
                )?;            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/io/FilterOutputStream",
        "write",
        "([BII)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            // TWO SILENT NO-OPS, both measured with
            // `L4TailSweep2.java`:
            //
            //   fos.write((byte[]) null, 0, 1)      HotSpot NPE, this VM nothing
            //   new FilterOutputStream(null).write  HotSpot NPE, this VM nothing
            //
            // `FilterOutputStream.write(byte[],int,int)` is a loop of
            // `out.write(b[off + i])`, so a null buffer is an NPE on the array
            // read and a null `out` is an NPE on the call. Doing nothing
            // instead is a write that vanishes -- the caller closes the stream
            // and believes it holds the bytes.
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            }
            let output = match ctx.get_field_by_name(this, "out") {
                Value::Object(Some(o)) => Some(o),
                _ => match ctx.get_field(this, 0) {
                    Value::Object(Some(o)) => Some(o),
                    _ => None,
                },
            };
            let Some(output) = output else {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            };
            {
                let _ = ctx.invoke_virtual(
                    output,
                    "write",
                    "([BII)V",
                    &[
                        args.get(1).copied().unwrap_or(Value::Object(None)),
                        args.get(2).copied().unwrap_or(Value::Int(0)),
                        args.get(3).copied().unwrap_or(Value::Int(0)),
                    ],
                )?;            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("java/io/FilterOutputStream", "flush", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let output = match ctx.get_field_by_name(this, "out") {
            Value::Object(Some(o)) => Some(o),
            _ => match ctx.get_field(this, 0) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            },
        };
        if let Some(output) = output {
            // `invoke_virtual` (the generic native-context dispatcher) can
            // resolve a dynamically-generated subclass (e.g. a Mockito
            // ByteBuddy mock of `OutputStream`) to the WRONG declaring
            // method's bytecode without erroring -- it silently no-ops
            // instead of running the mock's own overridden `flush()`, so
            // Mockito's interaction recorder never sees the call
            // (`StreamUtils.nonClosing(mock).flush()` -> `ordered.verify
            // (source).flush()` failed with "wanted but not invoked" even
            // though this native ran and returned success). `invoke_virtual
            // _bytecode_only` walks the RECEIVER's actual class hierarchy to
            // find where the method is truly declared and executes that
            // bytecode directly, matching what an ordinary `invokevirtual`
            // instruction would do.
            let _ = ctx.invoke_virtual_bytecode_only(output, "flush", "()V", &[])?;        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    registry.register("java/io/FilterOutputStream", "close", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let output = match ctx.get_field_by_name(this, "out") {
            Value::Object(Some(o)) => Some(o),
            _ => match ctx.get_field(this, 0) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            },
        };
        if let Some(output) = output {
            // See the identical fix + rationale on the sibling `flush()`
            // native above.
            let _ = ctx.invoke_virtual_bytecode_only(output, "close", "()V", &[])?;        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // Same process-controller path stores ServerSocketFactory.getDefault() in
    // the ProtocolServer configuration before the later network phase.
    crate::phases_early::register_phase52_server_socket_factory(registry);
    // Domain process-controller also constructs its read executor before the
    // normal concurrent phase; register the Executors factory bridge early.
    register_executor_natives(registry);
    crate::phases_early::register_scheduled_executor_natives(registry);
    crate::phases_early::register_qname_natives(registry);

    // WP1.9: StackStreamFactory.AbstractStackWalker.{callStackWalk,
    // fetchStackFrames} + StackFrameInfo accessors (getByteCodeIndex,
    // getDeclaringClass, etc.).
    lang_stackwalker::register_lang_stackwalker(registry);
    // T19.H2: JBoss Modules JDKSpecific — ModuleLayer.boot()/findModule +
    // Module.getName/getPackages/getLayer with pre-populated JDK packages.
    jboss_jdkspecific::register_jboss_jdkspecific(registry);
    // T19.H4: JBoss Modules LocalModuleLoader + Module + ModuleClassLoader
    // natives. Combined with the post-clinit fixup in
    // vm/src/vm/vm_util.rs that populates
    // DefaultBootModuleLoaderHolder.INSTANCE, this closes the KC16
    // "Cannot invoke loadModule on null" NPE by letting
    // Main.loadModule("org.jboss.as.standalone") resolve against
    // -mp <path>/module.xml on disk.
    jboss_module_loader::register_jboss_module_loader(registry);
    // T19.H2: SharedSecrets JavaLangAccess shim — returns a non-null
    // System$1-shaped object whose currentCarrierThread() forwards to
    // the current thread. Fixes "Cannot invoke currentCarrierThread on null"
    // on KC16 boot after ConcurrentHashMap / Lookup clinit B6-swallows.
    register_t19_h2_shared_secrets_shim(registry);
    // FFM real-JDK native-library load path: jdk.internal.loader
    // .RawNativeLibraries.load0/unload0 + NativeLibrary.findEntry0. These back
    // `java.lang.foreign.SymbolLookup.libraryLookup` when the JDK's own bytecode
    // runs (real-JDK mode), letting FFM bindings like Tomcat's openssl_h load
    // libssl/libcrypto. Registered here (the always-compiled essential path)
    // rather than in the synthetic-only `register_pe_panama`. See the fn doc.
    crate::panama::register_pe_raw_native_libraries(registry);
    // FFM real-JDK SymbolLookup bridge. `Linker.defaultLookup()` is supplied
    // by the late real-JDK shim, but it returns a real `SymbolLookup` interface
    // object whose `find(String)` declaration is abstract. Keep the Panama
    // `find`/lookup natives in the essential registry so off-heap users such as
    // Infinispan can resolve symbols instead of falling into AbstractMethodError.
    crate::panama::register_pe_symbol_lookup(registry);
    // FFM real-JDK Linker option compatibility. Elasticsearch native access
    // compiled against newer JDKs calls Linker.Option.critical(boolean),
    // which is absent from the Java 21 runtime used by this suite.
    crate::panama::register_pe_linker_options(registry);
    // FFM upcall stubs (round 12 wave 7, lane upcall): `Linker.upcallStub` on
    // the interface-stamped linker `nativeLinker()` hands out, whose JDK
    // declaration is abstract. States its own kind (`Bridge`).
    crate::panama_upcall::register_panama_upcall(registry);
    crate::ffm_fallback_linker::register_ffm_fallback_linker(registry);
    // FFM real-JDK layout/runtime shims. JDK 25's vector/foreign bootstrap
    // reaches `java.lang.foreign.ValueLayout$Of*` interface methods whose real
    // declarations are abstract/covariant. Register the phase-67 foreign-memory
    // shims here too (not only in synthetic phase 67) so the real-JDK dispatch
    // force routes have concrete native targets.
    crate::phases_late::register_p67_foreign_memory(registry);
    // Keep the full MemorySegment bridge in the always-on real-JDK registry.
    // Phase 67 covers layout/linker bootstrapping; this adds heap array
    // segments and indexed access used by Elasticsearch native vectors.
    crate::panama::register_pe_memory_segment(registry);
    // WP1.4: SharedSecrets.getJavaXxxAccess() factories for the
    // JDK Access interfaces plus the per-interface method natives
    // (currentCarrierThread, doIntersectionPrivilege, copyMethod,
    // parseCookie, …).  Registered after the T19.H2 shim so the
    // WP1.4 widened method set overrides the T19.H2 minimum.
    shared_secrets_bridge::register_wp1_4_shared_secrets(registry);

    // S111r12 SB3 follow-on: post-banner Spring Boot 3 boot reaches
    // `PathMatchingResourcePatternResolver.<clinit>` which calls
    // `SystemModuleFinders.ofSystem()` → `SystemModules$all.moduleDescriptors()`
    // → `jdk.internal.module.Builder.newExports(...)`.  `Builder.newExports`
    // (Builder.java:99) delegates to the static field
    // `JLMA = SharedSecrets.getJavaLangModuleAccess()` which we never wired
    // up — so JLMA is null and `JLMA.newExports(...)` NPEs.
    //
    // Fix: register native overrides for the five `Builder.new*` static
    // factories (`newExports` x2, `newOpens` x2, `newProvides`, `newRequires`
    // x2, `newVersion`) so the JLMA dereference is bypassed.  Each native
    // allocates a synthetic instance of the corresponding `ModuleDescriptor$*`
    // inner class with the original constructor arguments stashed in
    // recognisable named fields (`source`, `targets`, `mods`, `name`,
    // `version`, `service`, `providers`).  Spring's `<clinit>` only requires
    // the descriptor walk to NOT throw; it does not deeply inspect the
    // returned objects, so synthetic stand-ins are sufficient.
    //
    // The fields are written via `set_field_by_name` which is layout-tolerant
    // (slot-resolved at runtime), so the same code path works whether the
    // real-JDK ModuleDescriptor inner classes are loaded with their full
    // private fields or our synthetic minimum-field allocation is in play.
    register_module_builder_overrides(registry);
    // T19.H2: Lookup clinit dependency natives — Reflection.registerFieldsToFilter,
    // ClassFileDumper.getInstance, Set.of(Obj,…) overloads up to 8-arg.
    register_t19_h2_lookup_clinit_deps(registry);
    // T19_H13_BIGINTEGER_INTRINSICS — register HotSpot-equivalent native
    // overrides for `java.math.BigInteger`'s `@IntrinsicCandidate` methods
    // (implSquareToLen, shiftLeftImplWorker, shiftRightImplWorker,
    // implMulAdd, mulAdd). Without these, KC16 boot pegs main thread inside
    // `BigInteger.<clinit>` / SunJCE 2048-bit parameter validation for
    // tens of seconds, missing the watchdog safepoint.
    // census-tag: these are @IntrinsicCandidate fast-paths → Intrinsic
    // (nested sub-tag inside this Bridge-tagged wrapper).
    {
        biginteger_intrinsics::register_biginteger_intrinsics(registry);
    };
    // SBR wave 5 (lane cryptoeng): the SUN/SunJCE engines' own
    // `@IntrinsicCandidate` kernels, so the real JCA engine bytecode runs at
    // native speed. The registrar states `NativeKind::Intrinsic` itself and
    // restores the ambient category on exit.
    crypto_intrinsics::register_jdk_crypto_intrinsics(registry);

    // SunEC P-256 Montgomery field multiply/square. The pure-Java
    // MontgomeryIntegerPolynomialP256.{mult,square} is the dominant cost of EC
    // keygen/sign/verify (a one-time generator-table precompute calls it
    // ~10^5–10^6 times); HotSpot intrinsifies the same field arithmetic. The
    // Rust impl is byte-identical to the JDK (decode→Montgomery-mult→canonical
    // re-encode; verified vs JDK 25 limb vectors in sunec_intpoly::tests).
    {
        sunec_intpoly::register_sunec_intpoly_intrinsics(registry);
    };

    // Coarse native EC scalar-multiply that bypasses the one-time ~14 s
    // generator-table precompute. Default-ON whenever EC is routed real
    // (`route_ec_to_real`); `CRATONVM_NATIVE_EC_MULTIPLY` force-enables, and the
    // `CRATONVM_SYNTHETIC_EC=1` kill-switch leaves it inert. P-256 only — other
    // curves get a clear error (they were already unsupported in default mode).
    {
        sunec_point::register_sunec_point_intrinsics(registry);
    };

    // WP4.2: java.util.concurrent.CompletableFuture executor support.
    // Real JDK CompletableFuture.{supplyAsync, runAsync, thenApplyAsync,
    // thenAcceptAsync, thenRunAsync, thenComposeAsync, applyToEitherAsync,
    // …} all funnel through `executor.execute(Runnable)`. The default
    // executor is `ASYNC_POOL = ForkJoinPool.asyncCommonPool()`. Our
    // ForkJoinPool worker threads do not run Java bytecode (NativeContext
    // is not Send), so without an override the *Async chain leaves a
    // `CompletableFuture$Signaller` in the result slot, and `.get()`
    // produces a ClassCastException when the user casts to the value type.
    //
    // Eager-inline policy: invoke `runnable.run()` on the calling thread.
    // This loses true parallelism but preserves the user-observable
    // contract that `.get()` returns the final stage's value. Mirrors
    // the existing eager-inline policy used for ForkJoinTask.fork/invoke
    // in `phases_early::register_forkjoin_natives`.
    //
    // Registered in real-JDK essentials so it is in the registry
    // BEFORE CompletableFuture's <clinit> runs and resolves
    // ASYNC_POOL via `ForkJoinPool.asyncCommonPool()`. The dispatch
    // override in `vm/src/vm/vm_exec.rs` (search "WP4.2") forces the
    // native to win over the real JDK bytecode for both
    // `(Ljava/lang/Runnable;)V` and `(Ljava/util/concurrent/ForkJoinTask;)V`.

    // WP4.2: CompletableFuture.completeValue — manual CAS on the result slot.
    registry.register(
        "java/util/concurrent/CompletableFuture",
        "completeValue",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = match args.first().copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            let raw_val = args.get(1).copied().unwrap_or(Value::Object(None));
            // gc-common w20-c: `ensure_class_initialized` below can run
            // `<clinit>` (a GC point), and `this` is written after it.
            let this_pin = ctx.pin_native_root(this);
            let val = if matches!(raw_val, Value::Object(None)) {
                // Real-JDK CompletableFuture uses a null result slot to mean
                // "not completed yet".  A normally completed null value must be
                // represented by the private CompletableFuture.NIL AltResult
                // sentinel; otherwise waitingGet()/join() will spin/park
                // forever after completeValue(null).
                let cf_id =
                    ctx.ensure_class_initialized("java/util/concurrent/CompletableFuture")?;
                ctx.static_field_index_by_name(cf_id, "NIL")
                    .map(|idx| ctx.get_static_field(cf_id, idx))
                    .filter(|v| matches!(v, Value::Object(Some(_))))
                    .unwrap_or(raw_val)
            } else {
                raw_val
            };
            let this: ObjectRef = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            // Interpreter round i3 wave 80, lane L1: a real compare-and-set,
            // as the JDK body's `RESULT.compareAndSet(this, null, v)`. This was
            // a plain read then a plain store, so a racing `complete(v)` (whose
            // stand-in CASes) that had answered `true` could be overwritten by
            // `completeAsync`'s `AsyncSupply.run` landing here, and two
            // completers could both win; the store was not ordered against a
            // waiter's `stack` push either. `native_cf_complete` was fixed the
            // same way in nio20 wave 5. Probe `L1W80CompletableFutureBodies`
            // (`complete-race`).
            let won = ctx.compare_and_swap_field(this, 0, Value::Object(None), val);
            Ok(Some(Value::Int(i32::from(won))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    registry.register(
        "java/util/concurrent/ForkJoinPool",
        "execute",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            phases_early::fjp_reject_submission(ctx, args, 1)?;
            let runnable = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(None),
            };
            // Bug D: run on a real daemon thread, not inline, so blocking tasks
            // (start-gate latches, CompletableFuture.get) don't deadlock the
            // submitter. See `spawn_runnable_on_real_thread`.
            spawn_runnable_on_real_thread(ctx, runnable)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/concurrent/ForkJoinPool",
        "execute",
        "(Ljava/util/concurrent/ForkJoinTask;)V",
        |ctx, args| {
            phases_early::fjp_reject_submission(ctx, args, 1)?;
            let task = match args.get(1).copied() {
                Some(Value::Object(Some(r))) => r,
                _ => return Ok(None),
            };
            // Run the task through the shared side-table path rather than a
            // bare `exec()` invoke. The bare invoke never RECORDED the task
            // as done, so the matching `join()` saw `done == false` and ran
            // the body a SECOND time — a matrix probe against the host JDK
            // caught `execute(t); t.join()` executing `compute()` twice
            // (`ran=2` where HotSpot reports `ran=1`). Silent double
            // execution is a correctness bug for any non-idempotent task.
            //
            // `execute` is void and must not raise the task's exception at
            // the submitter (it surfaces at the matching `join()`/`get()`),
            // which is exactly `fjp_compute_for_submit`'s policy.
            let _ = phases_early::fjp_compute_for_submit(ctx, task)?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // WP4.3: Real-JDK essentials registration of ForkJoinTask /
    // RecursiveTask / RecursiveAction / ForkJoinPool natives. The real
    // JDK bytecode for `fork()` / `join()` / `invoke()` uses Unsafe CAS
    // on `ForkJoinTask.status` which interacts poorly with our heap-side
    // primitive layout (status read as Value::Object(None) → CAS retry
    // forever). Override with the same eager-inline + side-table policy
    // used in `phases_early::register_forkjoin_natives` so the FjpProbe
    // (1M long[]→sum, threshold 1000, depth 10) terminates with the
    // correct sum.
    //
    // The done+result side-table lives in `phases_early`; we re-import
    // its public API via `phases_early::{fjp_state_get, fjp_state_set_done}`
    // so the same map is shared between synthetic and real-JDK paths.
    phases_early::register_real_jdk_forkjoin_essentials(registry);

    // T19_K3_FJP_FACTORY_ESSENTIALS: ForkJoinPool.commonPool() / getFactory()
    // / getCommonPoolParallelism() — KC26's
    // `KeycloakMain.ensureForkJoinPoolThreadFactoryHasBeenSetToQuarkus`
    // does:
    //   sf = System.getProperty("java.util.concurrent.ForkJoinPool.common.threadFactory")
    //   cls = ForkJoinPool.commonPool().getFactory().getClass().getName()
    //   if (!cls.equals(sf)) throw RuntimeException
    // The real JDK bytecode for `commonPool()` reads a static `common`
    // field populated by ForkJoinPool's static initializer, which uses
    // Unsafe-backed AtomicReferenceFieldUpdater plumbing we don't fully
    // model.  Both fields read back as null/uninitialized, so
    // `getFactory().getClass()` NPEs.  We override both methods so they
    // return a synthetic ForkJoinPool whose `factory` field is an
    // instance of the class named by the system property (allowlisted
    // via `is_safe_factory_class_name`).  The corresponding override
    // gate in `vm/src/vm/vm_exec.rs` (search T19_K3_FJP_NATIVE_OVERRIDE)
    // bumps the override priority above the real JDK bytecode.
    crate::phases_late::register_t19_k3_forkjoinpool_common(registry);

    // --- java.lang.Object (native methods) ---
    // Object.<init>() — intentional no-op: Object has no fields to
    // initialize. The `_with_this` form documents that we receive a
    // `this` argument and deliberately ignore it. (NEW-6)
    registry.register("java/lang/Object", "<init>", "()V", native_noop_with_this, cratonvm_native_api::NativeKind::Bridge);
    // `registerNatives()` only binds JNI symbols into a method table HotSpot
    // builds at class-init time. CratonVM resolves natives through this
    // registry instead, so there is nothing to bind: a no-op is the correct
    // implementation, not a stub. (Same for every other `registerNatives`
    // below.)
    registry.register("java/lang/Object", "registerNatives", "()V", native_noop, cratonvm_native_api::NativeKind::Bridge);
    // JIT round 21, lane native (RD16-3; `CRATONVM_JIT_IDENTITY_HASH_LEAF`,
    // default ON): LEAF (audited) through `native_object_hash_code_leaf`, which
    // declines to `native_object_hash_code` on the full funnel for every case
    // it does not answer from the header. `=0`: the old callback, non-leaf.
    // The kind stays `Bridge`, stated.
    if identity_hash_leaf_enabled() {
        registry.with_leaf(true, |r| {
            r.register(
                "java/lang/Object",
                "hashCode",
                "()I",
                native_object_hash_code_leaf,
                NativeKind::Bridge,
            );
        });
    } else {
        registry.register(
            "java/lang/Object",
            "hashCode",
            "()I",
            native_object_hash_code,
            NativeKind::Bridge,
        );
    }
    registry.register(
        "java/lang/Object",
        "getClass",
        "()Ljava/lang/Class;",
        native_object_get_class,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Object",
        "clone",
        "()Ljava/lang/Object;",
        native_object_clone_reclaiming,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Object",
        "notify",
        "()V",
        native_object_notify,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Object",
        "notifyAll",
        "()V",
        native_object_notify_all,
        NativeKind::Bridge,
    );
    registry.register("java/lang/Object", "wait", "()V", native_object_wait, cratonvm_native_api::NativeKind::Bridge);
    // BUG FIX: `wait(J)` / `wait0(J)` must honor the millisecond timeout.
    // These were wired to `native_object_wait` (which passes `None` = wait
    // forever), so `Object.wait(100)` blocked indefinitely with no notify —
    // which made every timed-wait framework hang: Timer/ScheduledExecutor
    // threads `queue.wait(delay)` and never wake, so their tasks never fire.
    // Route to `native_object_wait_timeout`, which reads the millis arg.
    registry.register(
        "java/lang/Object",
        "wait",
        "(J)V",
        native_object_wait_timeout, cratonvm_native_api::NativeKind::Bridge,
    );
    // JDK 19+: wait(long) is a Java method that delegates to wait0(long),
    // which is the actual private native method.
    registry.register(
        "java/lang/Object",
        "wait0",
        "(J)V",
        native_object_wait_timeout,
        NativeKind::Bridge,
    );
    registry.register("java/lang/Object", "finalize", "()V", |_ctx, _args| {
        // Object.finalize() is deprecated for removal since JDK 9 and was removed from
        // Object's method table in JDK 18+. Our GC does not invoke finalizers — objects
        // are reclaimed when unreachable, with no opportunity for cleanup hooks. User
        // code should use try-with-resources / Cleaner / PhantomReference instead.
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- java.lang.System (native methods) ---
    // JNI symbol binding only — see java/lang/Object.registerNatives above.
    registry.register(
        "java/lang/System",
        "registerNatives",
        "()V",
        native_noop,
        NativeKind::Bridge,
    );
    // LEAF: both clock reads are `_ctx`-free — a `SystemTime::now()` /
    // `Instant::elapsed()` and an integer cast. Nothing to pin, nothing that
    // can allocate, safepoint or throw. See `NativeMethodRegistry::set_leaf`.
    // Measured at 604 ns from compiled code against HotSpot's 32 ns
    // (`probes/NativeShapeProbe.java`), essentially all of it the funnel — and
    // `System.nanoTime` is what every timeout, scheduler and benchmark in the
    // JDK calls.
    registry.set_leaf(true);
    registry.register(
        "java/lang/System",
        "currentTimeMillis",
        "()J",
        native_system_current_time_millis,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "nanoTime",
        "()J",
        native_system_nano_time,
        NativeKind::Bridge,
    );
    registry.set_leaf(false);
    registry.register(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        native_system_arraycopy,
        NativeKind::Bridge,
    );
    // JIT round 21, lane native (RD16-3; `CRATONVM_JIT_IDENTITY_HASH_LEAF`,
    // default ON): LEAF (audited) through
    // `native_system_identity_hash_code_leaf`, which declines to the full
    // funnel for a hash displaced into a monitor. `=0`: non-leaf as before.
    if identity_hash_leaf_enabled() {
        registry.with_leaf(true, |r| {
            r.register(
                "java/lang/System",
                "identityHashCode",
                "(Ljava/lang/Object;)I",
                native_system_identity_hash_code_leaf,
                NativeKind::Bridge,
            );
        });
    } else {
        registry.register(
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
            native_system_identity_hash_code,
            NativeKind::Bridge,
        );
    }
    registry.register("java/lang/System", "exit", "(I)V", native_system_exit, cratonvm_native_api::NativeKind::Bridge);
    // W3: derive the answer from the host instead of hard-coding null, so this
    // agrees with the now-truthful `Console.istty()`/`ttyStatus()` above.
    // No controlling terminal -> null, which is the spec'd "no console"
    // answer. On an interactive launch, hand back whatever the JDK's own
    // Console bootstrap already published in `java.io.Console.cons`; we do
    // NOT force <clinit> here, because the spec lets us answer null and a
    // throwing `System.console()` is worse than a null one.
    registry.register(
        "java/lang/System",
        "console",
        "()Ljava/io/Console;",
        |ctx, _args| {
            use std::io::IsTerminal;
            if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
                return Ok(Some(Value::Object(None)));
            }
            let Some(class_id) = ctx.class_id_by_name("java/io/Console") else {
                return Ok(Some(Value::Object(None)));
            };
            let Some(index) = ctx.static_field_index_by_name(class_id, "cons") else {
                return Ok(Some(Value::Object(None)));
            };
            match ctx.get_static_field(class_id, index) {
                Value::Object(Some(console)) => Ok(Some(Value::Object(Some(console)))),
                _ => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // T14: Real-JDK-safe System.getProperty/setProperty/clearProperty.
    // Delegate to the VM's system-properties store so these work before
    // initPhase1 populates System.props (or when it fails and falls back
    // to synthetic streams).  These override the JDK bytecode that
    // dereferences the (possibly-null) System.props field.
    registry.register(
        "java/lang/System",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            // `System.getProperty(null)` throws NPE; it does not answer null.
            // Answering null turned a caller's programming error into a silent
            // "property not set", which is the failure this whole override
            // exists to avoid on the OTHER side. MEASURED against HotSpot
            // 25.0.3+9, `probes/IoSystemSweep.java`, both modes.
            //
            // The EMPTY key is the other half of the same JDK method, added
            // 2026-08-29 -- see [`system_property_check_key`], which all four
            // entry points now share so they cannot drift apart again.
            let key = system_property_check_key(ctx, args)?;
            match system_property_read(ctx, &key) {
                Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            // The two-argument form runs the SAME `checkKey`: a null or empty
            // key is a throw, not a fall back to the default. Answering the
            // default was the more misleading of the two, because it looks
            // exactly like a correctly-handled missing property.
            let key = system_property_check_key(ctx, args)?;
            match system_property_read(ctx, &key) {
                Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
                None => Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None)))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/System",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let key = system_property_check_key(ctx, args)?;
            // `Properties.setProperty` -> `Hashtable.put`, whose null-value NPE
            // carries NO message. MEASURED: `System.setProperty("k", null)` is
            // `NullPointerException` with a null message on HotSpot and was a
            // silent `null` return here.
            let val_obj = match args.get(1) {
                Some(Value::Object(Some(v))) => *v,
                _ => {
                    return Err(RuntimeError::NullPointerException { message: None }.into());
                }
            };
            let val = ctx.read_string(val_obj).unwrap_or_default();
            let old = ctx.set_system_property(&key, &val);
            // Keep the cached `System.getProperties()` singleton's side-table in
            // sync (SC-web-method-spel RC-A): a caller that already holds a
            // reference to that singleton (e.g. Spring's `systemProperties` bean,
            // registered once at ApplicationContext refresh time) never calls
            // `System.getProperties()` again, so the resync-on-call logic in the
            // `getProperties` native never re-fires. Without this, a later
            // `System.setProperty(...)` is invisible through that stale
            // reference -- SpEL's `#{systemProperties.foo}` (routed through
            // `MapAccessor.canRead` -> `Properties.containsKey`) then reports the
            // property as absent even though `System.getProperty("foo")` sees it.
            if let Some(props) = crate::lang_system::system_props_singleton(ctx.vm_identity()) {
                crate::properties_sidetable::store_property_in_sidetable(ctx, props, &key, &val);
                // And the REAL map, which is the store once the `Properties`
                // natives are retired. Inert while the field is null, so
                // `--fast-boot` is untouched.
                crate::properties_sidetable::store_property_in_real_map(ctx, props, &key, &val);
            }
            match old {
                Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}
