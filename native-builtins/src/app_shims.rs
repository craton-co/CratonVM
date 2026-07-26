// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Third-party application-shim registration seams.
//!
//! `register_essential_natives` installs, on **every** boot including
//! `HelloWorld`, several thousand natives that only exist to serve one
//! third-party library each: BouncyCastle, JBoss/WildFly/XNIO, the JCA
//! connection pools, ANTLR, ByteBuddy/Mockito, Hibernate. The
//! [reachability audit] proposed extracting them into
//! `native-appshim-*` crates and described the boundary as a "clean seam,
//! one call site each".
//!
//! This module is that seam, made explicit: each family's registrars are
//! collected behind exactly one `pub fn` so that turning the family off —
//! whether by moving it to another crate or by making the call conditional
//! on the library being on the classpath — is a one-line change at a single
//! site instead of an archaeology exercise across 12 000 lines of
//! `register_essential_natives`.
//!
//! **Nothing here is conditional and nothing is dropped.** These functions
//! run unconditionally from `register_essential_natives`, in the same order
//! and under the same ambient [`NativeKind`] as the call sequence they
//! replaced, so the registered set is bit-identical to before the grouping.
//! Grouping is the safe half of the change; see below for why the other half
//! is not landable yet.
//!
//! [reachability audit]: ../../docs/internal/arch-2026-07-26/native-builtins-reachability.md
//!
//! # Why these are not registered lazily
//!
//! Two independent blockers, both outside this crate:
//!
//! 1. **The registry is immutable after boot.** `NativeMethodRegistry` is
//!    held *by value* (`vm/src/vm/realms/native_realm.rs:16`, "immutable
//!    after construction"), built as a local in `SharedVm::new`
//!    (`vm/src/vm/vm_init.rs:1006`) and moved into the `Arc<SharedVm>` at
//!    `vm/src/vm/vm_init.rs:2542`. `register` needs `&mut`
//!    (`native-api/src/registry.rs:4060`) and no `&mut` exists past that
//!    move. Late registration is not merely unsound today — it is
//!    unreachable.
//!
//! 2. **A miss is memoized as `Bytecode`, permanently.** For a method that
//!    *has* real bytecode and whose shim is meant to override it — which is
//!    what almost every registration below is — `populate_invoke_cache`
//!    probes the registry once and bakes `CachedInvokeTarget::Bytecode`
//!    into `thread.invoke_cache` (`vm/src/runtime/interpreter.rs:32451`).
//!    The three dispatch arms (`interpreter.rs:32625`, `:39811`, `:40656`)
//!    never re-probe, and no invalidation path is keyed on the registry, so
//!    a shim registered after that call site first ran would be ignored for
//!    the life of the thread.
//!
//!    Note what *is* sound, so the next reader does not re-derive it:
//!    [`NativeCallSite`](cratonvm_native_api) memoizes negatives against
//!    `NativeMethodRegistry::generation()` (`native-api/src/registry.rs:4728`)
//!    and self-heals; an `ACC_NATIVE` method with no registered callback
//!    caches nothing at all (`interpreter.rs:32414`) and re-probes every
//!    call; and `ResolvedMethod::native_target`'s `None`
//!    (`classloading/src/resolution.rs:122`) falls through to a live
//!    registry probe at `interpreter.rs:32207`, despite the warning comment
//!    above the field. The gap is specifically the `Bytecode` verdict.
//!
//! # What *is* landable, and what stands in its way
//!
//! Deciding at **registration time** — still inside `SharedVm::new`, before
//! any bytecode has executed — has neither problem: nothing has been
//! memoized yet, so there is nothing to poison. The classpath is fully
//! parsed and indexed ~900 lines earlier (`ClassManager::new`,
//! `vm/src/vm/vm_init.rs:612`), and each JAR carries a decompression-free
//! `entry_index: FxHashSet<String>` (`classloading/src/class_path.rs:338`),
//! so "is this library present?" costs one hash probe per classpath entry
//! and never inflates — it does **not** re-enter the O(num_jars x
//! zip-probes) rescan that caused the JAXB model-building hang
//! (`class_manager.rs:1319`, `synthetic_upgrade_absent`).
//!
//! The obstacle is only plumbing: `register_essential_natives`
//! (`crate::register_essential_natives`) takes `&mut NativeMethodRegistry`
//! and nothing else, so a registrar cannot see the classpath. Making a
//! family conditional needs a signature widening plus a one-line change at
//! `vm/src/vm/vm_init.rs:1509` — files this seam deliberately does not own.
//! [`ShimFamily::witness_resources`] carries the exact probe keys that
//! change would use.
//!
//! # …and why two of the four families still could not be gated
//!
//! The audit's "clean seam" claim does not survive checking *which classes*
//! each family registers into, as opposed to which registrars are reachable.
//! Two of the four families register natives on **JDK-module** classes:
//!
//! | Family | JDK classes it also owns |
//! |---|---|
//! | `JBossWildFlyXnio` | `java/security/AccessControlContext`, `javax/security/auth/Subject`, `javax/security/auth/login/LoginContext` (`java.base`), `javax/naming/InitialContext` (`java.naming`) |
//! | `DataSourcePools` | `javax/sql/DataSource` (`java.sql`) |
//!
//! Gating those on "is WildFly on the classpath?" would silently remove
//! JAAS and JNDI natives from every program that is not WildFly — the
//! `d8092acb` failure mode reached by a different route. It is not
//! hypothetical: the H2 suite's `LoginContext` fix lives in
//! `wildfly_security.rs`, and H2 is not WildFly.
//!
//! [`ShimFamily::jdk_entanglements`] records those classes and
//! [`ShimFamily::is_severable`] reports the verdict, both enforced by the
//! tests below. Splitting the JDK registrations out of
//! `wildfly_security` / `wildfly_naming` / `wildfly_datasources_tx` is the
//! prerequisite for gating those two families; `BouncyCastle` and
//! `AppIntrinsics` need no such work.
//!
//! # Deliberate non-members
//!
//! * The Xerces intrinsics (`crate::xml_xerces`, `lib.rs` `XERCES_*`) are
//!   **not** an application shim. Every class they register is
//!   `com/sun/org/apache/xerces/internal/**` or `jdk/xml/internal/**` — the
//!   JDK's own `java.xml` module, present in every JDK. The audit filed
//!   them under `native-appshim-data`; that classification is wrong and
//!   gating them on a classpath probe would be a no-op at best.
//! * `crate::servlet`'s Jython/ScriptEngine block registers
//!   `javax/script/ScriptEngineManager` (`java.scripting`) alongside the
//!   `org/python/**` shims, so it is not a pure family either.
//! * `crate::jboss_jdkspecific` registers `java/lang/module/Configuration`,
//!   `java/lang/module/ResolvedModule` and
//!   `jdk/internal/module/SystemModuleFinders$SystemModuleReader` — JDK
//!   module-system bridges that merely happen to be motivated by JBoss
//!   Modules. Not severable.

use cratonvm_native_api::NativeMethodRegistry;

/// A third-party library family whose natives `register_essential_natives`
/// installs unconditionally on every boot.
///
/// The metadata on this enum is the input a future conditional call site
/// needs: what to probe for ([`witness_resources`](Self::witness_resources)),
/// what the family owns ([`owned_prefixes`](Self::owned_prefixes)), and what
/// it would take away from everyone else if it were skipped
/// ([`jdk_entanglements`](Self::jdk_entanglements)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShimFamily {
    /// ANTLR 4 (incl. Groovy's shaded copy), ByteBuddy, Mockito, Hibernate.
    AppIntrinsics,
    /// BouncyCastle crypto kernels (registered `Intrinsic`; the
    /// `org/bouncycastle` package is JIT-banned, so these leaves would
    /// otherwise run interpreted).
    BouncyCastle,
    /// Agroal / IronJacamar / Infinispan / Vert.x / WildFly datasources +
    /// Narayana JTA.
    DataSourcePools,
    /// JBoss Modules resource loading, JBoss MSC, WildFly Core / Naming /
    /// Security / Undertow, and the XNIO worker, io-thread, conduit and
    /// async layers.
    JBossWildFlyXnio,
}

impl ShimFamily {
    /// Every family, in the order their registrars run during
    /// `register_essential_natives`.
    pub const ALL: [ShimFamily; 4] = [
        ShimFamily::AppIntrinsics,
        ShimFamily::BouncyCastle,
        ShimFamily::DataSourcePools,
        ShimFamily::JBossWildFlyXnio,
    ];

    /// Classpath resources whose presence means this family's library is
    /// actually in play.
    ///
    /// These are resource names, not class names, so they can be answered
    /// by a single `entry_index.contains(..)` probe per classpath entry
    /// (`classloading/src/class_path.rs:338`) with no inflate — the cheap
    /// path documented in the module header. Any one of them matching is
    /// enough; they are alternatives, not a conjunction (a family can be
    /// pulled in by more than one artifact, e.g. ANTLR either directly or
    /// via Groovy's shaded copy).
    pub fn witness_resources(self) -> &'static [&'static str] {
        match self {
            ShimFamily::AppIntrinsics => &[
                "org/antlr/v4/runtime/CommonToken.class",
                "groovyjarjarantlr4/v4/runtime/atn/ATNState.class",
                "net/bytebuddy/description/method/MethodDescription.class",
                "org/mockito/internal/creation/bytebuddy/MockMethodAdvice.class",
                "org/hibernate/spi/NavigablePath.class",
            ],
            ShimFamily::BouncyCastle => &[
                "org/bouncycastle/crypto/engines/AESEngine.class",
                "org/bouncycastle/math/ec/ECPoint.class",
                "org/springframework/security/crypto/bcrypt/BCrypt.class",
            ],
            ShimFamily::DataSourcePools => &[
                "io/agroal/pool/ConnectionPool.class",
                "org/jboss/jca/core/connectionmanager/pool/AbstractPool.class",
                "org/infinispan/manager/DefaultCacheManager.class",
                "io/vertx/core/impl/VertxImpl.class",
                "com/arjuna/ats/jta/TransactionManagerImple.class",
            ],
            ShimFamily::JBossWildFlyXnio => &[
                "org/jboss/msc/service/ServiceName.class",
                "org/jboss/modules/ResourceRootFactory.class",
                "org/xnio/OptionMap.class",
                "io/undertow/Undertow.class",
            ],
        }
    }

    /// Internal-name package prefixes this family registers natives into.
    ///
    /// Every class the family's registrars touch is either under one of
    /// these or listed in [`jdk_entanglements`](Self::jdk_entanglements) —
    /// an invariant the tests enforce, so a registrar that grows a new
    /// out-of-family target fails loudly instead of silently widening the
    /// blast radius of a future gating change.
    pub fn owned_prefixes(self) -> &'static [&'static str] {
        match self {
            ShimFamily::AppIntrinsics => &[
                "org/antlr/v4/",
                "groovyjarjarantlr4/",
                "net/bytebuddy/",
                "org/mockito/",
                "org/hibernate/",
            ],
            ShimFamily::BouncyCastle => &[
                "org/bouncycastle/",
                // Spring Security vendors BCrypt's expensive key schedule
                // under its own package; `register_bc_bcrypt_generator`
                // covers both copies.
                "org/springframework/security/crypto/bcrypt/",
            ],
            ShimFamily::DataSourcePools => &[
                "io/agroal/",
                "io/vertx/",
                "org/infinispan/",
                "org/jboss/jca/",
                "org/jboss/as/connector/",
                "com/arjuna/",
                // Jakarta EE (not JDK): these ship with the app server, not
                // with the JDK, so they are severable along with the family.
                "javax/resource/",
                "javax/transaction/",
            ],
            ShimFamily::JBossWildFlyXnio => &[
                "org/jboss/",
                "org/wildfly/",
                "org/xnio/",
                "io/undertow/",
                // VM-internal accept/poll pumps that exist only to drive the
                // XNIO event loop.
                "cratonvm/xnio/",
            ],
        }
    }

    /// JDK-module classes this family *also* registers natives on.
    ///
    /// Skipping the family would take these away from every program,
    /// including ones that have never heard of the library. A non-empty
    /// list therefore means the family cannot be made conditional until
    /// these registrations are split out into their own registrar.
    ///
    /// Kept exact rather than approximate: the tests assert both that no
    /// unlisted JDK class leaks in *and* that every entry is still really
    /// registered, so this list cannot rot in either direction.
    pub fn jdk_entanglements(self) -> &'static [&'static str] {
        match self {
            ShimFamily::AppIntrinsics | ShimFamily::BouncyCastle => &[],
            // `wildfly_datasources_tx.rs` registers the JDBC `DataSource`
            // surface itself, not merely WildFly's binding of it.
            ShimFamily::DataSourcePools => &["javax/sql/DataSource"],
            // JAAS (`wildfly_security.rs`) and JNDI (`wildfly_naming.rs`).
            // The H2 suite depends on the `LoginContext` natives; see the
            // module header.
            ShimFamily::JBossWildFlyXnio => &[
                "java/security/AccessControlContext",
                "javax/naming/InitialContext",
                "javax/security/auth/Subject",
                "javax/security/auth/login/LoginContext",
            ],
        }
    }

    /// Whether this family can be skipped without removing a native that a
    /// program outside the family might depend on.
    ///
    /// `false` is not a permanent verdict — it means "not until the classes
    /// in [`jdk_entanglements`](Self::jdk_entanglements) move to their own
    /// registrar".
    pub fn is_severable(self) -> bool {
        self.jdk_entanglements().is_empty()
    }

    /// Run this family's registrars. Equivalent to calling the matching
    /// free function directly.
    pub fn register(self, registry: &mut NativeMethodRegistry) {
        match self {
            ShimFamily::AppIntrinsics => register_app_intrinsic_shims(registry),
            ShimFamily::BouncyCastle => register_bouncycastle_shims(registry),
            ShimFamily::DataSourcePools => register_datasource_pool_shims(registry),
            ShimFamily::JBossWildFlyXnio => register_jboss_wildfly_xnio_shims(registry),
        }
    }
}

/// ANTLR / ByteBuddy / Mockito / Hibernate bytecode-equivalent intrinsics.
///
/// Called from inside `register_essential_natives`' existing
/// `with_category(Intrinsic, ..)` scope, so this function must not set a
/// category of its own — the ambient `Intrinsic` is the whole point.
pub fn register_app_intrinsic_shims(registry: &mut NativeMethodRegistry) {
    crate::antlr_intrinsics::register_antlr_prediction_context_intrinsics(registry);
    crate::antlr_intrinsics::register_antlr_token_intrinsics(registry);
    crate::test_frameworks::register_bytebuddy_method_token_intrinsics(registry);
    crate::test_frameworks::register_mockito_debugging_intrinsics(registry);
    crate::orm_hibernate::register_hibernate_testing_util_intrinsics(registry);
    crate::orm_hibernate::register_hibernate_models_intrinsics(registry);
}

/// BouncyCastle crypto kernels.
///
/// Every one of these exists for the same reason: the `org/bouncycastle`
/// package is JIT-banned (a value-model collision in its F2m EC path), so
/// the arithmetic leaves would otherwise run interpreted and dominate the
/// crypto suites. The per-registrar rationale is kept inline rather than
/// summarized away — it is the record of *why* each kernel was worth a
/// native port.
pub fn register_bouncycastle_shims(registry: &mut NativeMethodRegistry) {
    // BouncyCastle RSA-keygen small-factor prime pre-screen fast-path (Intrinsic).
    // BC is JIT-banned (value-model collision in its F2m EC path, unrelated), so
    // `Primes.implHasAnySmallFactors` otherwise runs interpreted and dominates
    // RSA key generation. Faithful single-word-mod reimplementation; see the fn doc.
    crate::phases_late::register_bc_primes_small_factors(registry);
    // BouncyCastle binary-field EC uses LongArray for generic F2m arithmetic.
    // Keep the org/bouncycastle JIT ban intact, but run the small polynomial
    // multiply/square/reduce/inverse leaves natively so math-ec and EC crypto
    // regression do not spend minutes in interpreted bit loops.
    crate::phases_late::register_bc_long_array(registry);
    // BouncyCastle generic prime-field EC uses ECFieldElement.Fp bytecode for
    // every point add/double. Keep BC bytecode JIT-banned, but run the field
    // arithmetic leaves with the same limb BigInteger core used by java.math.
    crate::phases_late::register_bc_fp_field_element(registry);
    // The generic prime-field point formulas remain hot in complete EC math
    // tests after the field leaves are native; route just those methods natively.
    crate::phases_late::register_bc_fp_point(registry);
    // Same treatment for generic binary-field ECFieldElement.F2m wrappers: this
    // avoids spending the math-ec suite in interpreted field-element glue around
    // the native LongArray polynomial leaves.
    crate::phases_late::register_bc_f2m_field_element(registry);
    // Generic binary-field ECPoint.F2m add/double is the hot Lambda-projective
    // point layer above the native F2m field-element leaves.
    crate::phases_late::register_bc_f2m_point(registry);
    // Keep the BC package JIT ban in place, but run the high-level Shamir JSF
    // driver loop natively above the native EC point methods.
    crate::phases_late::register_bc_ec_algorithms(registry);
    // Custom SEC binary curves bypass ECFieldElement.F2m and call static
    // SecT*Field kernels directly from point add/double code. Route those
    // polynomial kernels through the same native GF(2^m) engine.
    crate::phases_late::register_bc_sect_field_kernels(registry);
    // The inherited ECPoint.timesPow2 loop otherwise spends complete binary
    // curve tests in interpreted SecT*Point.twice glue around those kernels.
    crate::phases_late::register_bc_sect_point_methods(registry);
    // BouncyCastle AESEngine single-block transform fast-path (Intrinsic). Same
    // JIT-ban rationale: the interpreted T-table AES otherwise dominates AESTest's
    // Monte-Carlo stress. Verbatim FIPS-197-validated port of encrypt/decryptBlock.
    crate::phases_late::register_bc_aes_engine(registry);
    // CBC mode wrapper fast-path for AES-backed MAC/encryption loops. This keeps
    // the BC JIT ban intact while avoiding interpreted CBC bytecode above the
    // already-native AES block transform.
    crate::phases_late::register_bc_cbc_block_cipher(registry);
    // BouncyCastle GOST3412_2015Engine single-block transform fast-path
    // (Intrinsic). Keeps the BC package JIT ban while removing the interpreted
    // block-cipher loop that dominates GOST3412Test CTR stress.
    crate::phases_late::register_bc_gost3412_engine(registry);
    // BouncyCastle SM4Engine single-block transform fast-path for crypto regression.
    crate::phases_late::register_bc_sm4_engine(registry);
    // BouncyCastle XTEAEngine single-block transform fast-path for CipherStreamTest.
    crate::phases_late::register_bc_xtea_engine(registry);
    // BouncyCastle Strings UTF-8 transcode fast-path (Intrinsic) — dominates
    // AESTest.testCounter's growing-string round-trips once AES is native.
    crate::phases_late::register_bc_strings_utf8(registry);
    crate::phases_late::register_bc_arrays_helpers(registry);
    crate::phases_late::register_bc_param_helpers(registry);
    // BouncyCastle byte/int packing helpers used by block ciphers and digests.
    crate::phases_late::register_bc_pack_helpers(registry);
    // BouncyCastle X25519 field multiply fast-path for Ed25519/X25519 regression.
    crate::phases_late::register_bc_x25519_field(registry);
    // BouncyCastle X448 field multiply/square fast-path for Ed448 regression.
    crate::phases_late::register_bc_x448_field(registry);
    // BouncyCastle BLAKE2s compression leaf fast-path for Blake2xs XOF vectors.
    crate::phases_late::register_bc_blake2s_digest(registry);
    // BouncyCastle Keccak absorb/extract/permutation fast-path for CSHAKE/KMAC.
    crate::phases_late::register_bc_keccak_digest(registry);
    // BouncyCastle legacy GOST3411 compression-block fast-path for the
    // million-'a' digest regression under the org/bouncycastle JIT ban.
    crate::phases_late::register_bc_gost3411_digest(registry);
    // BouncyCastle Whirlpool update/compression fast-path for the million-'a'
    // digest regression under the same package JIT ban.
    crate::phases_late::register_bc_whirlpool_digest(registry);
    // BouncyCastle Poly1305 accumulator/finalization fast-path for standalone
    // Poly1305 and ChaCha20-Poly1305 regression vectors under the BC JIT ban.
    crate::phases_late::register_bc_poly1305(registry);
    // BouncyCastle SCrypt SMix/BlockMix fast-path for crypto regression.
    crate::phases_late::register_bc_scrypt_generator(registry);
    // BouncyCastle Argon2 block-round fast-path for crypto regression.
    crate::phases_late::register_bc_argon2_bytes_generator(registry);
    // BouncyCastle PKCS#5 v2 PBKDF2/SHA-1 KDF fast-path for crypto regression.
    crate::phases_late::register_bc_pkcs5s2_parameters_generator(registry);
    // BouncyCastle PKCS#12 SHA-1 KDF fast-path for crypto regression vectors.
    crate::phases_late::register_bc_pkcs12_parameters_generator(registry);
    // BouncyCastle BCrypt expensive key schedule fast-path for crypto regression.
    crate::phases_late::register_bc_bcrypt_generator(registry);
    // BouncyCastle CTR-mode (SICBlockCipher) per-byte loop fast-path (Intrinsic) —
    // the sole remaining hot frame in AESTest.testCounter once AES+Strings are native.
    crate::phases_late::register_bc_sic_ctr(registry);
    // BouncyCastle DigestRandomGenerator synchronized PRNG fast-path
    // (Intrinsic). This keeps the package JIT ban intact while avoiding the
    // million-call interpreted monitor body in DigestRandomNumberTest.
    crate::phases_late::register_bc_digest_random_generator(registry);
    // BouncyCastle ChaCha permutation fast-path (Intrinsic). Same JIT-ban
    // rationale: the interpreted ChaCha core (dozens of Integers.rotateLeft
    // calls per block) dominates the SPHINCS-256 PQC RegressionTest (PRG via
    // ChaChaEngine.chachaCore + hash via Permute.permute/HashFunctions).
    // Verbatim port, RFC 8439- and HotSpot-validated.
    crate::phases_late::register_bc_chacha(registry);
    // BouncyCastle NewHope lattice fast-path (Intrinsic). The NTT
    // (Poly.toNTT/fromNTT) + SHAKE128 sampler (Poly.uniform) dominate the PQC
    // RegressionTest's NewHopeTest once ChaCha is native. Verbatim port of
    // NTT/Reduce + the `sha3` crate's SHAKE128, validated against HotSpot.
    crate::phases_late::register_bc_newhope(registry);
}

/// JDBC connection pools, the embedded cache, the Vert.x/Netty event loop
/// and the WildFly datasources + Narayana JTA glue.
///
/// The two `real_*` guards are opt-in escapes that let the *real* library
/// bytecode run instead of the shim; they are preserved verbatim, so this
/// function registers exactly what the inline sequence did.
pub fn register_datasource_pool_shims(registry: &mut NativeMethodRegistry) {
    // T19.8: Agroal (Quarkus) + IronJacamar (WildFly) JDBC pool natives.
    // real-cdi-bean-container (Keycloak Gap 8): under `CRATONVM_REAL_AGROAL`
    // the shim is suppressed so the real `io.agroal.pool.*` container bytecode
    // runs over the real `org.h2.Driver`. The shim's interface-typed config
    // object otherwise collides with the real `DataSourceProvider` bytecode
    // (AbstractMethodError on `dataSourceImplementation()`). See `real_agroal`.
    if !crate::real_agroal() {
        crate::agroal_pool::register_agroal_natives(registry);
    }
    crate::ironjacamar_pool::register_ironjacamar_natives(registry);
    // T19.10: Infinispan local-mode cache (DefaultCacheManager + Cache).
    crate::infinispan_local::register_infinispan_natives(registry);
    // T19.6: Vert.x / Netty NioEventLoop affinity scheduler natives.
    // Under CRATONVM_REAL_VERTX the synthetic loop is suppressed so the real Netty
    // NioEventLoop.run() drives Selector.select() over our selector (see real_vertx).
    if !crate::real_vertx() {
        crate::vertx_eventloop::register_vertx_eventloop_natives(registry);
    }
    // T19.2.e: WildFly Datasources subsystem + Narayana JTA glue.
    crate::wildfly_datasources_tx::register_wildfly_datasources_tx_natives(registry);
}

/// JBoss Modules, MSC, WildFly Core/Naming/Security/Undertow and the four
/// XNIO layers.
///
/// This is the largest single family (~48 kLoC of module source) and the
/// one a `HelloWorld` has the least use for — but see
/// [`ShimFamily::jdk_entanglements`]: it currently also owns the JAAS and
/// JNDI natives, so it is not severable as-is.
pub fn register_jboss_wildfly_xnio_shims(registry: &mut NativeMethodRegistry) {
    // --- RA.4 / RA.5: JBoss Modules `module.xml` parser + ResourceRootFactory.
    // We bypass the MXParser path (which relies on NIO CharBuffer internals
    // the VM is still stabilizing) by parsing module.xml in Rust and calling
    // back into ModuleSpec$Builder / DependencySpec / ResourceLoaders via
    // `ctx.invoke`.
    registry.register(
        "org/jboss/modules/xml/ModuleXmlParser",
        "parseModuleXml",
        "(Lorg/jboss/modules/xml/ModuleXmlParser$ResourceRootFactory;Lorg/jboss/modules/ModuleLoader;Ljava/lang/String;Ljava/io/File;Ljava/io/File;)Lorg/jboss/modules/ModuleSpec;",
        crate::jboss_module_xml::native_parse_module_xml,
    );
    crate::jboss_resource_loader::register_resource_loader_natives(registry);

    // T19.1: JBoss MSC (Modular Service Container) — the async service
    // orchestrator WildFly / Keycloak 16 enters right after Main.main().
    // Registers ServiceName / ServiceContainer / ServiceController /
    // StartContext natives and spins up the msc-worker thread pool
    // lazily on first `ServiceContainer.create()` call.
    crate::jboss_msc::register_jboss_msc_natives(registry);

    // T19.2.a: WildFly Core kernel — Deployment + Threads + Logging.
    // Runs right after MSC so DeploymentUnit / EnhancedQueueExecutor /
    // LogManager + Logger / ControlledProcessState natives are in place
    // before naming / security / undertow register their services.
    crate::wildfly_core::register_wildfly_core_natives(registry);

    // T19.2.b: WildFly Naming (JNDI) — InitialContext + ServiceBasedNamingStore
    // + ContextNames natives. Runs after the Core kernel so the MSC container
    // and DeploymentUnit layout exist before any subsystem calls `bind()`.
    // Enforces the Log4Shell-class JNDI-injection allowlist at the native
    // boundary: only `java:`, `java:jboss/`, `java:comp/`, `java:global/`,
    // `java:app/`, `java:module/` absolute names are accepted.
    crate::wildfly_naming::register_wildfly_naming_natives(registry);

    // T19.2.c: WildFly Security — JAAS LoginContext / LoginModule chain,
    // Subject / Principal sets, SecurityDomainService / ApplicationPolicy
    // lookup, and SecurityIdentity.runAs. Bootstrap internal-auth path for
    // Keycloak 16; real identity still comes from KC's own realm.
    crate::wildfly_security::register_wildfly_security_natives(registry);

    // T19.2.d: WildFly Undertow — HTTP server builder, HttpHandler dispatch,
    // HeaderMap / HttpString / Sender surface, UndertowService and
    // ListenerService lifecycle. HTTP/1.1 request head parser with size caps
    // (8 KiB line / 32 KiB headers / 100 headers / 10 MiB body), CRLF-injection
    // rejection on response headers, Host-header validation, and panic-safe
    // handler dispatch (500 on panic instead of process crash). The accept
    // loop that will drive live request handling is deferred to T19.7 (XNIO
    // worker); here we expose `dispatch_handler` / `accept_backoff` as the
    // integration points.
    crate::wildfly_undertow::register_undertow_natives(registry);

    // T19.7.b: JBoss XNIO worker — top-level XnioWorker with bounded I/O
    // + task thread pools (default 4 + 16, clamped at 128 + 1024), round-
    // robin I/O-thread dispatch, panic-safe task submission, and graceful
    // shutdown/awaitTermination semantics. Provides the scaffolding
    // T19.7.c (io_thread event loop), T19.7.d (conduits), and T19.7.e
    // (OptionMap builder) plug into.
    crate::xnio_worker::register_xnio_worker_natives(registry);

    // T19.7.c: XnioIoThread / NioIoThread / XnioExecutor$Key natives —
    // execute(), executeAfter(), executeAtTime(), currentThread(),
    // Key.remove(). Event loop uses a BinaryHeap<Reverse<ScheduledTask>>
    // min-heap for timers + Mutex<VecDeque<IoTask>> for immediate tasks;
    // cross-thread execute wakes the Selector via a coalesced AtomicBool.
    crate::xnio_io_thread::register_xnio_io_thread_natives(registry);

    // T19.7.d: XNIO Conduit stream channels — read/write through
    // ConduitStreamSourceChannel / ConduitStreamSinkChannel, ChannelListener
    // dispatch (panic-safe), resume/suspend semantics, half-close via
    // shutdownReads/Writes, and flush reporting based on a buffered-bytes
    // counter. Listener callbacks are dispatched through `invoke_virtual`
    // wrapped in `catch_unwind` so a buggy listener can't crash the loop.
    crate::xnio_conduits::register_xnio_conduits_natives(registry);

    // T19.7.e: XNIO OptionMap + IoFuture + Options + XnioExecutor primitives.
    // OptionMap is immutable-after-build (Arc<HashMap>-backed); IoFuture has
    // an AtomicU8 monotonic state machine (WAITING → DONE/CANCELLED/FAILED)
    // with Condvar-wake on completion and panic-isolated notifier dispatch.
    // Populates Options.<WORKER_IO_THREADS, BACKLOG, TCP_NODELAY, …> on first
    // access so xnio_worker can read defaults.
    crate::xnio_async::register_xnio_async_natives(registry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_native_api::NativeKind;

    /// Internal-name prefixes that belong to the JDK, not to any
    /// application. A family registering into one of these cannot be
    /// skipped without taking the native away from unrelated programs.
    const JDK_PREFIXES: [&str; 5] = ["java/", "javax/", "jdk/", "sun/", "com/sun/"];

    /// `javax/` is shared: `javax.naming` / `javax.sql` / `javax.script` are
    /// JDK modules, while `javax.transaction` / `javax.resource` are Jakarta
    /// EE artifacts that ship with the app server. Only the JDK ones matter
    /// for severability.
    const JAKARTA_NOT_JDK: [&str; 2] = ["javax/transaction/", "javax/resource/"];

    fn is_jdk_class(class: &str) -> bool {
        if JAKARTA_NOT_JDK.iter().any(|p| class.starts_with(p)) {
            return false;
        }
        JDK_PREFIXES.iter().any(|p| class.starts_with(p))
    }

    /// A registry that registers what the registrars ask for, regardless of
    /// the ambient environment.
    ///
    /// `NativeMethodRegistry::new()` latches `CRATONVM_NO_STUBS`
    /// (`native-api/src/registry.rs:3979`), and under it `register()` silently
    /// drops every `SyntheticStub` registration — which includes the JNDI
    /// entanglement, since `wildfly_naming` never sets a category and so
    /// inherits the registry's `SyntheticStub` default
    /// (`registry.rs:3879`). Pinning it off keeps these tests measuring the
    /// registrars rather than the shell they happen to run in.
    fn probe_registry() -> NativeMethodRegistry {
        let mut registry = NativeMethodRegistry::new();
        registry.set_drop_synthetic_stubs(false);
        registry
    }

    fn registered_classes(family: ShimFamily) -> Vec<String> {
        let mut registry = probe_registry();
        family.register(&mut registry);
        let mut classes: Vec<String> = registry
            .dump_registrations()
            .into_iter()
            .map(|(class, _, _, _)| class.to_string())
            .collect();
        classes.sort();
        classes.dedup();
        classes
    }

    #[test]
    fn every_family_registers_something() {
        for family in ShimFamily::ALL {
            assert!(
                !registered_classes(family).is_empty(),
                "{family:?} registered nothing — the seam has drifted away from \
                 its registrars"
            );
        }
    }

    /// The invariant that makes the seam meaningful: a family owns its own
    /// packages and nothing else, apart from the JDK classes it explicitly
    /// declares. If a registrar grows a new out-of-family target this fails
    /// with the class name, instead of silently widening what a future
    /// conditional call site would remove.
    #[test]
    fn families_register_only_into_owned_prefixes_or_declared_entanglements() {
        for family in ShimFamily::ALL {
            let owned = family.owned_prefixes();
            let declared = family.jdk_entanglements();
            for class in registered_classes(family) {
                let in_family = owned.iter().any(|p| class.starts_with(p));
                let declared_jdk = declared.contains(&class.as_str());
                assert!(
                    in_family || declared_jdk,
                    "{family:?} registers `{class}`, which is neither under one of \
                     its owned prefixes {owned:?} nor a declared JDK entanglement. \
                     Either add the prefix (if the family genuinely grew) or add the \
                     class to jdk_entanglements() (if it is a JDK bridge that blocks \
                     conditional registration)."
                );
            }
        }
    }

    /// The list must not rot in the other direction either: a stale
    /// entanglement entry would make a family look unseverable forever.
    #[test]
    fn declared_entanglements_are_all_actually_registered() {
        for family in ShimFamily::ALL {
            let classes = registered_classes(family);
            for declared in family.jdk_entanglements() {
                assert!(
                    classes.iter().any(|c| c == declared),
                    "{family:?} declares `{declared}` as a JDK entanglement but no \
                     longer registers it — drop it from jdk_entanglements() so the \
                     family can be gated."
                );
            }
        }
    }

    /// Pins the actual finding: two of the four families are blocked, and
    /// the reason is JDK-namespace registrations, not the `NativeKind` tag.
    /// If a later change splits JAAS/JNDI out of `wildfly_security` /
    /// `wildfly_naming`, this test is the one that says "you may now gate
    /// this family".
    #[test]
    fn severability_matches_measured_jdk_entanglement() {
        for family in ShimFamily::ALL {
            let has_jdk_class = registered_classes(family).iter().any(|c| is_jdk_class(c));
            assert_eq!(
                family.is_severable(),
                !has_jdk_class,
                "{family:?}: is_severable() disagrees with what it actually registers"
            );
        }
        assert!(ShimFamily::AppIntrinsics.is_severable());
        assert!(ShimFamily::BouncyCastle.is_severable());
        assert!(!ShimFamily::DataSourcePools.is_severable());
        assert!(!ShimFamily::JBossWildFlyXnio.is_severable());
    }

    /// `NativeKind` is ambient (`current_category` persists across
    /// `set_category`), which is precisely what made the `d8092acb` tag
    /// audit unsound. Grouping the call sequence must not disturb it: every
    /// family's registrars save and restore the category, so the ambient
    /// value a caller had before is the value it has after.
    #[test]
    fn grouping_does_not_leak_an_ambient_category() {
        for seed in [
            NativeKind::SyntheticStub,
            NativeKind::Bridge,
            NativeKind::Intrinsic,
        ] {
            for family in ShimFamily::ALL {
                let mut registry = probe_registry();
                registry.set_category(seed);
                family.register(&mut registry);
                assert_eq!(
                    registry.current_category(),
                    seed,
                    "{family:?} leaked an ambient category (seeded {seed:?})"
                );
            }
        }
    }

    /// A witness is only useful if it names a class the family actually
    /// serves, so a classpath probe for it means what the gating change
    /// would assume it means.
    #[test]
    fn witness_resources_are_class_files_under_an_owned_prefix() {
        for family in ShimFamily::ALL {
            let witnesses = family.witness_resources();
            assert!(
                !witnesses.is_empty(),
                "{family:?} has no classpath witness, so it could never be probed for"
            );
            for witness in witnesses {
                assert!(
                    witness.ends_with(".class"),
                    "{family:?} witness `{witness}` is not a .class resource name; \
                     entry_index holds exact archive entry names"
                );
                assert!(
                    !witness.starts_with('/'),
                    "{family:?} witness `{witness}` must be a bare internal name"
                );
                assert!(
                    family
                        .owned_prefixes()
                        .iter()
                        .any(|p| witness.starts_with(p)),
                    "{family:?} witness `{witness}` is outside the family's own \
                     packages, so its presence would not imply the family is in use"
                );
            }
        }
    }

    /// Guards the classification correction in the module header: the
    /// Xerces intrinsics are `java.xml`, so they are not a candidate family
    /// and must never be added to `ShimFamily`.
    #[test]
    fn jdk_bundled_xerces_is_not_a_shim_family() {
        assert!(
            is_jdk_class("com/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet"),
            "the JDK-bundled Xerces must classify as a JDK class"
        );
        assert!(is_jdk_class("jdk/xml/internal/XMLLimitAnalyzer"));
        for family in ShimFamily::ALL {
            assert!(
                !family
                    .owned_prefixes()
                    .iter()
                    .any(|p| p.starts_with("com/sun/") || p.starts_with("jdk/")),
                "{family:?} claims a JDK-internal package as its own"
            );
        }
    }

    #[test]
    fn jakarta_ee_packages_are_not_treated_as_jdk() {
        assert!(!is_jdk_class("javax/transaction/TransactionManager"));
        assert!(!is_jdk_class("javax/resource/spi/ManagedConnection"));
        assert!(is_jdk_class("javax/sql/DataSource"));
        assert!(is_jdk_class("javax/naming/InitialContext"));
        assert!(is_jdk_class("java/security/AccessControlContext"));
    }
}
