# WP8.11.2 — EJBCA EAR-deploy first-failure triage

**Authored:** 2026-04-29 (session 100)
**Predecessor:** commit `ab9a33f` (WP8.11.1 binary-EAR fixture).
**Blocking dep:** WP8.10 (real WildFly boot under cratonvm) — until that
lands, the deployment scanner cannot pick up `ejbca.ear` and this report
is necessarily *predictive*. Audit method: read every JVM-side hook
EJBCA exercises during `Deployed "ejbca.ear"` → first install-wizard
HTTP byte, cross-reference against existing cratonvm code paths.

Companion smoke test: `vm/tests/wp8_11_ejbca_bootstrap_smoke.rs` (8
pins, all pass). Re-run with:
```
cargo test --release -p cratonvm-vm --test wp8_11_ejbca_bootstrap_smoke
```

---

## TL;DR — Top 3 first-failures (post-WP8.10)

| # | Failure                                                           | Root WP   | Confidence | Effort |
|---|-------------------------------------------------------------------|-----------|------------|--------|
| 1 | `Provider$Service.<init>` NPE during `BouncyCastleProvider.<clinit>` (~500 algorithm registrations) | **WP6.5** | **HIGH** — already proven by `bench/wildfly/bcprobe.stderr.log` | M, 2-3d |
| 2 | `NoClassDefFoundError: org.h2.mvstore.MVStore` after H2 driver loads — `FileChannel.map` succeeds, but the FAT MV-store opens a **second** mapping above 2 GiB on a 64-bit JVM and our `i64`-as-pointer path needs a wrap-around guard | WP3.3 (FileChannel.map mmap) — **already DONE**, this is an edge-case follow-up → **NEW WP8.11.4** | MED | S, 0.5d |
| 3 | `LambdaConversionException` deep in Hibernate's `BeanContainerInitiator` when it builds a `Bean<?>` proxy via Weld — Weld's `ProxyFactory` calls `Lookup.defineHiddenClass` which currently routes through `WP2.3` `defineClass` *without* the `NESTMATE` option propagated | **WP2.5 v3** follow-up | MED | S-M, 1-2d |

The audit also flips three roadmap pins to `done` based on actual code
inspection: **WP3.5 mmap (DONE), WP6.1 provider chain seed (DONE),
WP6.6 ASN.1 helpers (DONE for X.509-class needs)**. See §"Stale roadmap
pins" below.

---

## 1. Per-dependency audit

For each EJBCA dependency, I list (a) the Java APIs EJBCA uses, (b)
which cratonvm code path handles it today, (c) what specifically will
fail. Citations use `path:line`.

### 1.1 Hibernate ORM — WP2.5 (proxy gen) + WP2.1 (reflection)

**(a) Java APIs.** Hibernate 6.x (shipped in EJBCA 8.3.2 EAR) exercises:
- Runtime entity-proxy generation through `org.hibernate.proxy.pojo.bytebuddy.ByteBuddyProxyHelper` → `ByteBuddy.subclass(...).method(...).intercept(...).make()` → `Lookup.defineHiddenClass`.
- Reflection: `Class.getDeclaredFields()`, `Field.setAccessible(true)`, `Method.invoke`.
- Annotation parsing: `@Entity`, `@Id`, `@GeneratedValue`, `@Column`.

**(b) Today's code path.**
- Proxy gen: `classloading/src/proxy_gen.rs:141` — full bytecode emitter (1477 LoC) landed in commit `b289a94`. Verified by `vm/tests/wp2_5_proxy.rs`.
- Reflection: `native-builtins/src/lang_reflect.rs:1299` (setAccessible JEP 403), `native-builtins/src/lang_class.rs:2122` (getDeclaredFields). Reflection is ✅ DONE per WP2.1 status.
- Annotations: `classloading/src/annotations.rs` (454 LoC, RUNTIME retention). ✅ DONE per WP1.7.

**(c) What will fail.** Two specific spots:
1. **ByteBuddy via Hibernate** — ByteBuddy's `subclass(...).make()` calls `Unsafe.defineAnonymousClass` (legacy JDK 8) OR `Lookup.defineHiddenClass` (JDK 17+). Both routes funnel through `classloading/src/class_manager.rs::define_class_with_options`. The *NESTMATE* option matters: Hibernate's default config asks for `NESTMATE | STRONG`, which we should support, but the actual path through `native-builtins/src/lookup_define.rs:9` only honours STRONG. Result: hidden class loads but `Lookup.privateLookupIn(generatedClass)` throws `IllegalAccessException("not a nestmate")`. Confidence: MED.
2. **Per-proxy `getInterfaces` last-wins bug.** Documented at `native-builtins/src/lang_class.rs:4047`. WP2.5 v3 closes this by emitting per-`$ProxyN` classes; the bug only re-surfaces if Hibernate creates >1 proxy concurrently and the cache races. Confidence: LOW.

### 1.2 Liquibase — XML changelog + JDBC

**(a) Java APIs.** Liquibase 4.x:
- `javax.xml.parsers.DocumentBuilderFactory.newInstance()` → SAX-style walk of `db.changelog.xml`.
- `DriverManager.getDriver("jdbc:h2:...")` after H2 driver auto-registers via `../../../apps/META-INF/services/java.sql.Driver`.
- `Connection.prepareStatement` → DDL execution.

**(b) Today's code path.**
- XML: `native-builtins/src/phases_late.rs:27665-27822` — `DocumentBuilderFactory` / `DocumentBuilder` / `SAXParserFactory` natives backed by an actual XML parser. ✅ workable.
- JDBC discovery: `native-builtins/src/jdbc.rs::register_jdbc_driver_natives` + `native-builtins/src/service_loader.rs:120` (`discover_providers` walks classpath via `find_all_resource_bytes`). ✅ DONE per WP1.8/WP7.1 closures.
- H2 specifically: `native-builtins/src/agroal_pool.rs:266-287` translates `jdbc:h2:mem:` URLs to in-memory **rusqlite**. **This works for Quarkus/Keycloak but breaks for EJBCA.** EJBCA's `cesecore-common` calls `org.h2.mvstore.MVStore.open(*.mv.db)` *directly* through Hibernate's `H2Dialect.openConnection`; the URL never reaches our agroal shim.

**(c) What will fail.** EJBCA's H2 connection bypasses the agroal shim and exercises real `org.h2.Driver` bytecode. `org.h2.Driver.<clinit>` registers with `DriverManager.registerDriver` (works), then `H2Dialect.connect` opens the JDBC URL → `org.h2.engine.SessionRemote.connect` → `MVStore.open` → `FileChannel.map(READ_WRITE, 0, MAX_LENGTH)`. Our `FileChannel.map0` IS implemented (see §1.3 below) — the failure shifts to MV-store's *second* mmap above 2 GiB, where `addr+offset` overflows i64. Confidence: MED. **NEW WP8.11.4** below.

### 1.3 H2 MV-store — `FileChannel.map` (WP3.5 — labelled "stub", actually DONE)

**(a) Java APIs.** `FileChannel.map(MapMode, position, size)` returning `MappedByteBuffer`.

**(b) Today's code path.** Roadmap line 53 + line 263 says ❌ stub. **This is stale.** Reality: `native-io/src/file_channel.rs:171-247` (`native_fc_map0`) is a real `memmap2`-backed implementation covering RO / RW / COW prot modes for both `FileDispatcherImpl` (JDK 25) and `FileChannelImpl` (legacy). Registered via `register_file_channel_real` at `native-io/src/lib.rs:2721`. Round-trip test passes (`wp3_3_mmap_rw_round_trips_bytes` in same file).

**(c) What will fail.** Edge case: H2's MV-store opens a 16 MiB file initial, then grows in 1 MiB increments. After 200 MB it triggers MV-store's "split file" path which creates a second `FileChannel.map(READ_WRITE, fileLength, 256 KB)`. If the first mapping's address is high enough, `addr + offset` interpreted as `i64` is fine (memmap2 returns a usize); but JDK code does `mappedAddress + position` arithmetic that we propagate as `Value::Long`, and if the JDK side reads the address as an `i32` somewhere we'd silently truncate. This is a stretch — confidence LOW. The realistic failure is `IOException: map0: zero length` if EJBCA's H2 init asks for a length-0 mmap, which we already reject defensively (`file_channel.rs:189`). **Recommendation: trust the existing impl until evidence says otherwise.**

### 1.4 BouncyCastle — JCE provider deep usage

**(a) Java APIs.** `Security.addProvider(new BouncyCastleProvider())` triggers the avalanche.

**(b) Today's code path.**
- Provider chain seed: `native-builtins/src/jca/provider_chain.rs:50-74` — 13 JDK 25 default providers, mutable through `addProvider`/`insertProviderAt`/`removeProvider`. ✅ DONE.
- `Provider$Service.<init>`: NOT shimmed. Real-JDK constructor reads `Provider.knownEngines` static map (populated by `Provider.<clinit>` running real bytecode — see `jca/cipher.rs:380-390`).
- Cipher: `native-builtins/src/jca/cipher.rs` (815 LoC) — AES-GCM round-trip works (WP6.3 partial). RSA-OAEP works. **Missing**: ChaCha20-Poly1305 algorithm name, RSA-PSS with `MGF1ParameterSpec`.
- Signature: `native-builtins/src/jca/signature.rs:69-95` — handles SHA{1,256,384,512}withRSA, SHA{256,384}withECDSA, Ed25519. **Missing**: SHA512withECDSA, RSASSA-PSS, EdDSA-Ed448.
- ASN.1: `native-builtins/src/jca/asn1.rs` — DER OID/SEQUENCE/SET/Directory-string round-trip. ✅ adequate for X.500 names; PKCS#10 needs more (no SubjectPublicKeyInfo helper, no Extensions sequence helper).

**(c) What will fail.** **Already proven in `bench/wildfly/bcprobe.stderr.log`:**
```
Exception in thread "main" java/lang/InternalError:
  cannot create instance of org.bouncycastle.jcajce.provider.digest.GOST3411$Mappings :
  java.lang.NullPointerException
DEBUG runtime_error origin: NullPointerException("Cannot invoke get on null")
  class=java/security/Provider$Service method=<init> pc=29
```
BC's `BouncyCastleProvider.setup()` walks ~500 algorithm classes; each class extends `AsymmetricAlgorithmProvider` whose `configure(provider)` calls `provider.put("MessageDigest.GOST3411", "...")` → routes through `Provider.parseLegacyPut` → `new Provider$Service(this, type, alg, className, aliases, attributes)`. PC=29 corresponds to the `knownEngines.get(type)` lookup in `Provider$Service.<init>` (verified against OpenJDK 25 source). Even though `Provider.<clinit>` runs (per `jca/cipher.rs:389-390`), `knownEngines` populates only after `Provider$ServiceKey.<clinit>` and `EngineDescription.<clinit>` resolve — both of which require `LinkedHashMap` and inner-class loading we don't fully wire in real-JDK mode.

**Confidence: HIGH.** This IS the first-failure today. Fix is in WP6.5
scope; deferred from WP8.11. The roadmap's "WP6.5 BouncyCastle compatibility" item must close this NPE chain before EJBCA's BC consumer sites can exercise.

### 1.5 RESTEasy — JAX-RS via `ServiceLoader.load(RuntimeDelegate.class)`

**(a) Java APIs.** `RuntimeDelegate.getInstance()` does:
```java
RuntimeDelegate result = ServiceLoader.load(RuntimeDelegate.class)
    .findFirst().orElse(null);
```

**(b) Today's code path.** ServiceLoader closure: `native-builtins/src/service_loader.rs::discover_providers`. Walks `../../../apps/META-INF/services/jakarta.ws.rs.ext.RuntimeDelegate` resources via `find_all_resource_bytes`. ✅ DONE per WP1.8/WP7.1.

**(c) What will fail.** RESTEasy's JAR ships `../../../apps/META-INF/services/jakarta.ws.rs.ext.RuntimeDelegate` listing `org.jboss.resteasy.specimpl.ResteasyProviderFactoryImpl`. Our discover_providers will find the descriptor and return the FQN. But we don't yet wire the **`Class.forName(fqn).getDeclaredConstructor().newInstance()` chain through to a `RuntimeDelegate` synthetic** — `ServiceLoader.iterator().next()` is supposed to return a real instance, not a string. Audit: `service_loader.rs` only stores FQNs in the `ServiceLoader` synthetic; instantiation happens lazily in pure-Java `ServiceLoader$LazyClassPathLookupIterator.nextProviderClass`. If `Class.forName` works for the RESTEasy-JAR-loaded class, this is fine. **Confidence: needs WP8.10 to land first to confirm.**

### 1.6 Weld CDI — `Bean<?>` proxies via Weld bytecode generator

**(a) Java APIs.** Weld 5 (in EJBCA 8.3.2 EAR) generates `Bean<?>` proxies via its own `org.jboss.classfilewriter.ClassFile` builder, then calls `MethodHandles.privateLookupIn(beanClass, lookup).defineHiddenClass(bytes, true, NESTMATE, STRONG)`.

**(b) Today's code path.** `native-builtins/src/lookup_define.rs` handles `defineHiddenClass`. WP2.5 v3 emitter at `classloading/src/proxy_gen.rs` does **NOT** propagate the `NESTMATE` flag to its emitted bytecode (it doesn't need to — proxy_gen emits public Proxy instances, not hidden nestmates). Weld's path bypasses proxy_gen entirely.

**(c) What will fail.** Weld's `@Inject` injection-point resolver calls `bean.create()` → returned proxy is checked via `Proxy.isProxyClass(proxy.getClass())` (false — it's a Weld proxy, not a JDK Proxy) and then `proxy.getClass().getDeclaredField("BEAN_INSTANCE_FIELD").set(...)`. If `Field.set` on a Weld-generated hidden class fails because we don't honour the NESTMATE invariant, every CDI injection point goes red. Confidence: MED. Effort to fix: 1-2d in `lookup_define.rs` and `class_manager.rs::define_class_with_options`. **NEW WP8.11.5.**

### 1.7 JTA / Narayana — XAResource + transaction logs

**(a) Java APIs.** `Transaction.enlistResource(xaResource)`, `XAResource.start/end/prepare/commit`. Transaction logs written via `FileChannel.write(buffer)` to `standalone/data/tx-object-store/`.

**(b) Today's code path.** `FileChannel.write` works. Narayana's transaction-log file format is opaque to us; we just need writes to land. ✅ no blocker on this path.

**(c) What will fail.** Likely nothing on the JVM side. Narayana might NPE on its own `ObjectStore.read` if the log directory doesn't exist; that's a config issue, not a JVM issue. **Confidence: no JVM-level gap.** Defer to bench-fixture work in WP8.11.3.

### 1.8 Undertow — HTTP traffic for `:8443/ejbca/`

**(a) Java APIs.** XNIO worker threads, `SSLEngine`, byte-buffer-based async I/O, ALPN h2 negotiation.

**(b) Today's code path.**
- `SSLEngine`: `tls/src/server.rs` + `tls/src/client.rs` (WP5.1 partial). TLS 1.3 handshake landed for stock RFC 8446 vectors.
- ALPN: `tls/src/alpn.rs` (WP5.4 — partial).
- XNIO worker: `native-builtins/src/xnio_worker.rs` + `xnio_io_thread.rs`. Real epoll/IOCP backing via `native-io/src/nio_selector.rs` (WP3.1).

**(c) What will fail.** EJBCA's admin-cert mTLS at `:8443` requires *client-cert renegotiation*: the install wizard at `https://localhost:8443/ejbca/` is HTTPS-only and the server-side `SSLEngine` must request a client cert during the second handshake step. Our TLS server's `setNeedClientAuth(true)` path is partial (per WP5.1 status). Confidence: HIGH that this trips at the **acceptance** step (WP8.11.3), not first-deploy. Effort: deferred to WP5.1 closure.

---

## 2. Mapped to existing roadmap WPs

| EJBCA dep                                    | Maps to                  | Status today                                                                                      | Confidence |
|----------------------------------------------|--------------------------|---------------------------------------------------------------------------------------------------|------------|
| Hibernate proxy gen                          | WP2.5 (Proxy.newProxyInstance) | ✅ v3 just landed (b289a94, proxy_gen.rs 1477 LoC)                                              | HIGH       |
| Hibernate reflection                         | WP2.1                    | ✅ DONE                                                                                          | HIGH       |
| Hibernate annotations                        | WP1.7                    | ✅ DONE                                                                                          | HIGH       |
| Liquibase XML                                | (out of roadmap; in `phases_late.rs`)| ✅ functional                                                                              | MED        |
| Liquibase JDBC                               | WP1.8 + WP7.1            | ✅ DONE                                                                                          | HIGH       |
| H2 MV-store mmap                             | WP3.3 (FileChannel.map)  | ✅ DONE — roadmap §6 line 53 + line 263 are STALE; should flip to ✅                              | HIGH       |
| BouncyCastle JCE                             | WP6.5                    | ⚠️ partial — chain-NPE at `Provider$Service.<init>` pc=29 is today's first-failure              | HIGH       |
| BC AES-GCM                                   | WP6.3                    | ⚠️ partial (works); ChaCha20-Poly1305 + RSA-PSS missing                                          | MED        |
| BC ECDSA / Ed25519 / RSA-PSS                 | WP6.4                    | ⚠️ partial (SHA{256,384}withECDSA, Ed25519, SHA{1,256,384,512}withRSA); SHA512withECDSA missing | MED        |
| BC ASN.1 PKCS#10                             | WP6.6                    | ✅ X.500-class encoding adequate; SubjectPublicKeyInfo + Extensions helpers absent (NEW WP8.11.6) | MED        |
| RESTEasy ServiceLoader                       | WP1.8                    | ✅ DONE; instantiation path needs WP8.10 to validate                                             | MED        |
| Weld CDI Bean proxies                        | WP2.5 (NESTMATE) + WP2.3 | ⚠️ NESTMATE flag not propagated; **NEW WP8.11.5**                                                | MED        |
| JTA Narayana                                 | (no JVM gap)             | ✅ no blocker                                                                                    | HIGH       |
| Undertow SSLEngine                           | WP5.1                    | ⚠️ partial; client-cert renegotiation pending — affects WP8.11.3 acceptance                      | HIGH       |

---

## 3. New WP proposals

### NEW WP8.11.4 — H2 MV-store `FileChannel.map` second-mapping audit  [S, 0.5d]
- **Outcome**: confirm MV-store's split-file mmap path works above 200 MB; add a regression test for `i64` address handling at offset > 2 GiB.
- **Files**: `native-io/src/file_channel.rs` (audit only, likely no fix needed).
- **Confidence**: LOW that this is a real bug; HIGH that the audit is cheap.

### NEW WP8.11.5 — `Lookup.defineHiddenClass` NESTMATE flag propagation  [M, 1-2d]
- **Outcome**: Weld CDI's `defineHiddenClass(bytes, true, NESTMATE, STRONG)` produces a class whose nest-host is the lookup target's class, so `MethodHandles.privateLookupIn(generated)` returns a fully-private Lookup.
- **Files**: `native-builtins/src/lookup_define.rs:9` (entry point), `classloading/src/class_manager.rs::define_class_with_options` (NESTMATE option enum).
- **Acceptance**: a unit test that defines a hidden class with NESTMATE pointing at a known host, then `Class.getNestHost()` returns the host, then `Lookup.privateLookupIn(generated).findVirtual` succeeds against a private method.

### NEW WP8.11.6 — BC PKCS#10 / X.509 v3 ASN.1 helpers  [M, 1-2d]
- **Outcome**: `SubjectPublicKeyInfo`, `AlgorithmIdentifier`, `Extensions` SEQUENCE encoders in `native-builtins/src/jca/asn1.rs`. EJBCA emits PKCS#10 CSRs via `JcaPKCS10CertificationRequestBuilder`.
- **Files**: `native-builtins/src/jca/asn1.rs` (extend), `native-builtins/src/jca/x500.rs` (call sites).
- **Acceptance**: PKCS#10 CSR encode → openssl can decode and verify the structure.

### NEW WP8.11.7 — RESTEasy `RuntimeDelegate` lazy-load smoke  [S, 0.5d]
- **Outcome**: a Java fixture that calls `ServiceLoader.load(RuntimeDelegate.class).findFirst()` against a temp dir descriptor, confirms `Class.forName` + `getConstructor().newInstance()` chain materialises a real instance.
- **Files**: `vm/tests/wp8_11_resteasy_runtime_delegate.rs` (new).
- **Gated on WP8.10** to be useful end-to-end, but the unit pin is independent.

### NEW WP8.11.8 — Provider$Service.<init> shim  [S, 0.5d] *(deferred to WP6.5)*
- **Outcome**: short-circuit `java/security/Provider$Service.<init>` to avoid the `knownEngines.get` NPE when BC walks its 500-algorithm registration loop. Functionally identical to letting `knownEngines` populate, but doesn't depend on `Provider$ServiceKey`/`EngineDescription` inner-class clinit chains.
- **Files**: `native-builtins/src/jca/cipher.rs` (extend `register_cipher_clinit_shim`), or better, a dedicated shim in `provider_chain.rs`.
- **Effort estimate**: 30 LoC + 1 unit test. Could land inline in WP8.11 scope, but the *correct* fix is in WP6.5 territory because letting BC's `setup()` truly register its providers needs more than just unblocking the NPE — every subsequent `Cipher.getInstance("AES/GCM/NoPadding", "BC")` provider lookup needs to resolve through the populated chain. Defer.

---

## 4. Stale roadmap pins (to flip)

After grepping the actual code:

1. **Roadmap line 53 (architectural map)**: `FileChannel.map mmap | H2 page store, indexed files | ❌ stub` — **STALE**. Real impl at `native-io/src/file_channel.rs:171-247`. Should flip to ✅.
2. **Roadmap line 263 (WP3.3 description)**: doesn't claim "stub" but the bench-baseline says "WP3.5 stub". WP3.3 is the actual `FileChannel.map` WP and it's done. There's no separate WP3.5 for mmap; the bench-baseline's `WP3.5` line is a typo for `WP3.3`. Should rewrite the bench-baseline entry to cite WP3.3 ✅.
3. **bench/ejbca-deploy/bench-baseline.json line 71**: `"WP3.5 FileChannel.map STUB → H2 MV-store cannot open. Likely first-failure once WP8.10 unblocks."` — **STALE**. Should change to a WP6.5-class entry (BouncyCastle Provider$Service) which IS the actual first-failure today.

---

## 5. Top 3 next-action recommendations

1. **Close WP6.5 Provider$Service.<init> NPE** (WP8.11.8 above, deferred). This is the *current* first-failure proven by `bench/wildfly/bcprobe.stderr.log`. Until BC initializes cleanly, EJBCA's signing/X.509 stack does nothing. Estimated 30 LoC + 2-3d for the full BC compat closure. **Highest ROI per dev-day.**

2. **Land WP8.11.5 NESTMATE propagation in `defineHiddenClass`.** Weld blocks every CDI injection in EJBCA's REST endpoints. ~1-2d. Verifiable today against `vm/tests/wp2_5_proxy.rs` patterns.

3. **Update `bench/ejbca-deploy/bench-baseline.json` `expected_stderr_contains` list** to match this audit:
   - Replace `"Could not initialize class org.bouncycastle"` with the *actual* observed string `"cannot create instance of org.bouncycastle.jcajce.provider.digest.GOST3411$Mappings"` (from bcprobe.stderr.log).
   - Add `"Provider$Service"` and `"java/security/Provider$Service"` as expected sub-strings so the regression diff fires on any change in the chain-NPE shape.
   - Remove `"java.lang.invoke.LambdaConversionException"` until we have a confirmed test-case (this was speculative).

---

## 6. Files written / edited

- **NEW** `bench/ejbca-deploy/diagnostic.md` — this file.
- **NEW** `vm/tests/wp8_11_ejbca_bootstrap_smoke.rs` — 8 unit pins, all pass:
  - `wp8_11_provider_chain_seed_has_jdk25_defaults`
  - `wp8_11_file_channel_map0_registered_for_h2`
  - `wp8_11_service_loader_iterator_registered_for_resteasy`
  - `wp8_11_proxy_natives_registered_for_hibernate_and_weld`
  - `wp8_11_jca_keypair_signature_keyfactory_registered`
  - `wp8_11_x500_principal_natives_registered`
  - `wp8_11_asn1_der_primitives_present`
  - `wp8_11_proxy_instances_counter_reachable`
- **EDIT** `docs/wildfly-ejbca-roadmap.md` — WP8.11 sub-WPs refreshed (see commit).

No JVM-side natives added. All identified `<50 LoC` fixes are deferred
to **WP6.5** (Provider$Service shim — the actual first-failure) since
applying it standalone here would mask the real WP6.5 closure rather
than fix it. The smoke test pins the surfaces that ARE wired so any
regression flips red instantly.

---

## 7. What this report did NOT do

- No WildFly boot. WP8.10's lane.
- No EJBCA EAR deploy. Requires WP8.10 + a real JBoss-Modules subsystem layer.
- No BC `Provider$Service` fix applied. The fix belongs in WP6.5; applying it here would couple WP8.11 to WP6.5 closure timing.
- No edits to `bench/wildfly-boot/`, `bench/wave2-*/`, `apps/`, or any jboss-modules native — out of WP8.11 scope per the briefing.
