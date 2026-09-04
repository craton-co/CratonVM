# E25 / R11 — the seventeenth `Mac` method, and the sweep for every other guard whose population was its own answer

**Date:** 2026-08-13 **Lane:** E25
**Closes:** NOM E20-3 in `E20-R11-INTERSECTION-BLIND-GUARD-20260813.md` §6.
**Status:** FIXED-UNVERIFIED (registration + two guards in the owned file);
sweep findings mostly OPEN, nominated.
**Prov:** HotSpot 25.0.3+9 measured on this host today; every CratonVM "after"
is **PREDICTED**.

**This lane did not build or run CratonVM, and did not run `cargo`.** The
HotSpot transcripts in §1 and §3 were taken on this host, 2026-08-13, on
`openjdk 25.0.3 2026-04-21 LTS, Microsoft-13877124, build 25.0.3+9-LTS`.
Everything else is source read in this working tree.

**Edits applied (owned file only):** `native-builtins/src/phases_late/ssl_security.rs`

| what | where |
|---|---|
| `getInstance(String, Provider)` registered — the 17th public `Mac` method | `register_p68_crypto_mac`, after the `(String,String)` overload |
| `check_provider_ownership` added to the `(String,String)` overload — it checked provider EXISTENCE and never OWNERSHIP | same fn |
| `mac_algorithm_arg_string_typed` — class-discriminated algorithm-argument scan | next to `mac_algorithm_arg` |
| `mac_supported_set_matches_the_advertised_sunjce_services` repopulated from HotSpot's 28 SunJCE `Mac` rows, ratcheted both ways | the `#[cfg(test)]` block |

**§6 carries 13 nominations. NOM E25-1 is BLOCKING: the tree is RED until it
lands.**

**The sweep found 31 instances in `native-builtins/` (§3) and 30 more across
every other crate (§4) — 61 rows. Four of them cannot go red for the property
they are named for, under any input: rows 1, 32, 33, 34.**

---

> **VERIFIED AGAINST A BINARY 2026-09-04.** Status was **FIXED-UNVERIFIED**:
> *"This lane did not build or run CratonVM, and did not run `cargo`."* Both
> have now been done.
>
> **TASK 1 — the seventeenth `Mac` method, and the ownership check.** Measured
> with `probes/MacSurfaceProbe.java` against Temurin 25.0.3+9 on the same host.
> Both CratonVM arms are **byte-identical to the oracle**:
>
> ```text
> Mac.getInstance("HmacSHA256", providerObj)   OK alg=HmacSHA256 provider=SunJCE len=32
> Mac.getInstance("HmacSHA1",   providerObj)   OK alg=HmacSHA1   provider=SunJCE len=20
> Mac.getInstance("NoSuchMac",  providerObj)   NoSuchAlgorithmException … for provider SunJCE
> Mac.getInstance("HmacSHA256", "SUN")         NoSuchAlgorithmException … for provider SUN
> Mac.getInstance("HmacSHA256", "SunJCE")      OK
> Mac.getInstance("HmacSHA256", "Ghost")       NoSuchProviderException: no such provider: Ghost
> HmacSHA256 digest  e04e1ddd…095a395c        (identical to HotSpot, byte for byte)
> ```
>
> The `"SUN"` row is the one the guard was added for: `SUN` **exists** and owns
> no `Mac` algorithm, so EXISTENCE would have answered success and OWNERSHIP
> answers `NoSuchAlgorithmException` naming the provider. `"Ghost"` still gives
> `NoSuchProviderException`, so the two failure modes are distinguished rather
> than merged. And the digest matching proves the SPI is reached, not merely
> resolved.
>
> **NOM E25-1, the BLOCKING nomination, HAS LANDED.** *"The tree is RED until
> this lands"* — it is not red for this reason any more. The row was **flipped,
> not deleted**:
>
> ```text
> native-builtins/src/phases_late.rs:9532-9536
>   ("getInstance", "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;", true, "")
>
> cargo test -p cratonvm-native-builtins every_public_mac_method_is_registered   1 passed
> ```
>
> **`cargo test --workspace`, this record's own named command: 17,974 passed,
> 16 failed.** None of the 16 is this record's: the only file this branch
> changes outside `docs/` and `probes/` is `types/tests/unverified_records.rs`,
> so every one of them is pristine `origin/dev`'s. (One of the 16,
> `cratonvm-gc`'s `a_long_mark_step_admits_a_waiting_writer_before_it_finishes`,
> passes 3/3 when run alone — it is load, not code.)
>
> **§3/§4 rows 32 and 33 are now STALE, and stale in this record's favour.**
> Both were listed as guards that *"cannot go red for the property they are
> named for, under any input"* because a `#[cfg(feature = "synthetic-jdk")]` kept
> them out of the default build. Both cfgs are gone, and `vm/tests/jck_conformance.rs`
> credits this sweep by name for it:
>
> > *"# Two layers, and why the file-level `#[cfg]` came off (2026-08-13) … (E25
> > sweep,"*
>
> ```text
> row 32  jck_conformance.rs                    ran 3 tests in the default build (was: zero)
> row 33  wp7_2_jdbc_core_types_reachable.rs    ran 10 tests (was: zero) -- and one FAILS
> ```
>
> **Row 33's newly-live guard immediately caught a real defect**, which is the
> sweep's whole thesis arriving as data rather than argument:
>
> ```text
> connection_methods_carry_signatures … FAILED
>   connection_methods_have_signatures returned -100 (expected 1) —
>   `Connection.class.getDeclaredMethods()` regressed
> ```
>
> **Row 32 is only HALF repaired, and the record's own words say which half.**
> It described *"two dark layers over a gate"*. The outer layer is gone; the
> inner two are not. `jck_regression_gate` — the thing whose message claims it
> *"enforces the committed baseline"* — is still `#[cfg(feature =
> "synthetic-jdk")]` and is **not** among the three tests that now run, and
> behind that it still opens with `if !class_files_available() { … return; }`
> (`:676`). So the harness compiles in; the gate it is named for still does not
> run in the default build.
>
> **What this does NOT verify.** The sweep is 61 rows; four were re-examined
> here (1, 32, 33, 34) and the other 57 were not. Row 34's test runs and passes,
> which is consistent with this record's claim that it is tautological and is not
> evidence against it — nothing here re-derives that argument. The remaining 12
> nominations in §6 are untouched. §§1 and 3's HotSpot transcripts are the oracle
> and were not re-taken.
>
> **One caution about the command this record names.** The first
> `cargo test --workspace` run returned `cc` link failures across the tree,
> which reads exactly like a broken build. `/data` was at **100%** and the
> linker had died with `Bus error` — a full filesystem hitting a mapped write.
> After freeing space the same command gave the 17,974/16 above. A disk-full
> result and a compile failure are indistinguishable in `cargo`'s output.

## 1. TASK 1 — the seventeenth method, and its contract

### 1.1 `javap`, taken here, not recalled

```
$ javap -public -s javax.crypto.Mac
public class javax.crypto.Mac implements java.lang.Cloneable {
  public final java.lang.String getAlgorithm();                    ()Ljava/lang/String;
  public static final javax.crypto.Mac getInstance(String);        (Ljava/lang/String;)Ljavax/crypto/Mac;
  public static final javax.crypto.Mac getInstance(String,String); (Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Mac;
  public static final javax.crypto.Mac getInstance(String,Provider);
                                     (Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;   <-- the missing one
  public final java.security.Provider getProvider();               ()Ljava/security/Provider;
  public final int getMacLength();                                 ()I
  public final void init(Key);                                     (Ljava/security/Key;)V
  public final void init(Key,AlgorithmParameterSpec);              (Ljava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V
  public final void update(byte);                                  (B)V
  public final void update(byte[]);                                ([B)V
  public final void update(byte[],int,int);                        ([BII)V
  public final void update(java.nio.ByteBuffer);                   (Ljava/nio/ByteBuffer;)V
  public final byte[] doFinal();                                   ()[B
  public final void doFinal(byte[],int);                           ([BI)V
  public final byte[] doFinal(byte[]);                             ([B)[B
  public final void reset();                                       ()V
  public final java.lang.Object clone();                           ()Ljava/lang/Object;
}
```

**17.** Confirmed. `grep -rn 'javax/crypto/Mac' --include=*.rs` returns 3 files
(`phases_late/ssl_security.rs`, `phases_late.rs`, `vm/src/vm/tests.rs`) and none
carried `(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;`.

### 1.2 Why the omission was not inert

The other two overloads allocate a 4-slot synthetic `javax/crypto/Mac` and seed
`mac_state_table`, keyed by identity hash. This one fell through to the **real
JDK body**, which hands back a `Mac` with **no** `mac_state_table` row — while
`init`, `update`, `doFinal`, `getAlgorithm`, `getMacLength`, `reset` and `clone`
on that object are all intercepted by natives that read that row. `entry(id)
.or_default()` then fabricates an empty `MacState` on first `init`, so:

* `getAlgorithm()` answers `""`;
* `getMacLength()` answers `IllegalStateException: MAC algorithm unavailable: `;
* `doFinal()` computes over an **empty algorithm name** through
  `mac_compute_hmac`, whose match has no default arm.

Which of those a caller hits first is unmeasured, and unmeasured is the point:
the census that claimed to cover this class never listed the method, so no run
has ever asked.

### 1.3 The contract — all rows from HotSpot, none inferred from the two-arg form

`scratchpad/e25/MacOracle.java`, this host, today:

```text
--- 1-arg ---
getInstance("HmacSHA256")            -> OK  algo=HmacSHA256 prov=SunJCE len=32
getInstance("NoSuchMac")             -> NoSuchAlgorithmException: Algorithm NoSuchMac not available
getInstance((String)null)            -> NullPointerException: null algorithm name
getInstance("")                      -> NoSuchAlgorithmException: Algorithm  not available

--- 2-arg (String provider) ---
getInstance("HmacSHA256","SunJCE")   -> OK  prov=SunJCE
getInstance("HmacSHA256","SUN")      -> NoSuchAlgorithmException: no such algorithm: HmacSHA256 for provider SUN
getInstance("HmacSHA256","NoSuchProv") -> NoSuchProviderException: no such provider: NoSuchProv
getInstance("HmacSHA256",(String)null) -> IllegalArgumentException: missing provider
getInstance("HmacSHA256","")         -> IllegalArgumentException: missing provider
getInstance(null,"SunJCE")           -> NullPointerException: null algorithm name
getInstance("NoSuchMac","SunJCE")    -> NoSuchAlgorithmException: no such algorithm: NoSuchMac for provider SunJCE
getInstance("NoSuchMac","NoSuchProv")-> NoSuchProviderException: no such provider: NoSuchProv
getInstance(null,(String)null)       -> NullPointerException: null algorithm name

--- 3rd overload (Provider OBJECT) ---
getInstance("HmacSHA256",SunJCE)     -> OK  prov=SunJCE len=32, getProvider() == the SAME instance (measured `true`)
getInstance("HmacSHA256",SUN)        -> NoSuchAlgorithmException: no such algorithm: HmacSHA256 for provider SUN
getInstance("HmacSHA256",new Provider("MyAnon","1.0","test"){})
                                     -> NoSuchAlgorithmException: no such algorithm: HmacSHA256 for provider MyAnon
getInstance("NoSuchMac",SunJCE)      -> NoSuchAlgorithmException: no such algorithm: NoSuchMac for provider SunJCE
getInstance("",SunJCE)               -> NoSuchAlgorithmException: no such algorithm:  for provider SunJCE
getInstance("HmacSHA256",(Provider)null) -> IllegalArgumentException: missing provider
getInstance(null,SunJCE)             -> NullPointerException: null algorithm name
getInstance(null,(Provider)null)     -> NullPointerException: null algorithm name

--- the three overloads agree byte-for-byte ---
m1=m2=m3=9c196e32dc0175f86f4b1cb89289d6619de6bee699e4c378e68309ed97a1a6ab   (HmacSHA256, key="key", "abc")
providers: m1=SunJCE m2=SunJCE m3=SunJCE
```

Four rows are the whole design, and **none is derivable from the `(String,
String)` overload**:

1. **There is no `NoSuchProviderException` on this overload at all.** The caller
   handed over an instance; there is nothing to look up. `MyAnon` was never
   passed to `Security.addProvider` and still got as far as the algorithm check.
   Reasoning from the two-arg form would have produced a
   `NoSuchProviderException` arm that HotSpot can never reach.
2. **A null provider is `IllegalArgumentException("missing provider")`** — the
   same wording and the same exception as the two-arg form's `null` and `""`.
3. **The null-algorithm check runs FIRST.** `getInstance(null, null)` is NPE, not
   IAE, on both two-argument overloads. So the algorithm is located before the
   provider is examined.
4. **A registered provider that lacks the row and an unregistered provider that
   lacks it answer identically** — `no such algorithm: X for provider <name>`.
   `SUN` is a registered provider with **zero** `Mac` service rows (measured:
   `SUN.getServices()` filtered to type `Mac` is empty).

### 1.4 What was registered

Ordering: `mac_algorithm_arg_string_typed` (NPE) → `check_named_provider_arg`
(IAE on null; documented no-op for a non-`String` argument) →
`check_provider_ownership` (the NSAE) → `mac_algorithm_supported` (backstop).
`ProviderArgWording::Shared` reproduces both messages verbatim.

**`mac_algorithm_arg_string_typed` is not decoration.** The existing
`mac_algorithm_arg` takes the first argument `read_string` succeeds on. On the
`(String, String)` overload that is always the algorithm. On this one it is only
the algorithm while the algorithm is non-null — a `Provider` receiver is not
guaranteed to read back as `None`, which is the hazard
`check_named_provider_arg`'s own comment is built around ("a Provider object
that read back as an empty string would otherwise be reported as a missing
provider"). With a null algorithm the plain scan returns the PROVIDER as the
algorithm name and answers `NoSuchAlgorithmException` where HotSpot answers
`NullPointerException: null algorithm name`. The new scan skips any argument
whose class is known and is not `java/lang/String`, and falls back to
`read_string` only where the context cannot report a class at all — so mock
contexts degrade to exactly the old behaviour, never worse, and the leading
`Value::Object(None)` receiver-placeholder convention still resolves.

### 1.5 The sibling that disagreed with HotSpot — fixed in the same file

The brief asked what the new overload must agree with. It does not agree with
what was there:

**`getInstance(String, String)` checked provider EXISTENCE and never provider
OWNERSHIP.** `check_named_provider_arg` admits any name `find()` knows, and
`SUN` is such a name. So `Mac.getInstance("HmacSHA256", "SUN")` returned a
working HMAC — **PREDICTED**, from source — where HotSpot throws
`NoSuchAlgorithmException: no such algorithm: HmacSHA256 for provider SUN`
(measured). That is the wrong-accept half of the W4-3 species this file
documents at length: not a wrong MAC, but a MAC where the oracle refuses, which
presents only as an interop bug against everyone running a real JDK.

`check_provider_ownership` was added to that overload too. It reads the same
service table `Provider.put` populates, so an application provider registered
via `Security.addProvider` is still owned and still served; and its `Shared`
wording is byte-identical to the measured message.

### 1.6 Residuals, stated so a green run is not read as more than it is

| residual | measured HotSpot | this tree | why not fixed here |
|---|---|---|---|
| `getProvider()` returns the canonical `SunJCE` object, not the caller's instance | `m.getProvider() == provider` is `true` | returns `jce_provider_object(ctx)` | needs a provider field on `MacState`, which is built with an explicit all-fields literal in `phases_late.rs:9247/9264` — a file this lane does not own, whose own comment says to list every field |
| `getInstance(null, "SunJCE")` on the 2-arg overload | `NullPointerException: null algorithm name` | `NoSuchAlgorithmException: Algorithm SunJCE not available` (PREDICTED — the plain scan returns the provider as the algorithm) | the fix is to move that overload onto `mac_algorithm_arg_string_typed` too; `mac_algorithm_arg`'s own doc says changing it "would red a test in a file this lane does not own". NOM E25-9 |
| `mac_normalise` strips `-`, so `HMAC-SHA256` is accepted | `NoSuchAlgorithmException: no such algorithm: HMAC-SHA256 for provider SunJCE` (measured; `hmacsha512/256` and `HMACSHA3-512` DO resolve, so JCA folds case and does not strip `-`) | accepted, and a test **asserts** it must be | over-acceptance, not a wrong MAC; narrowing it needs a run. Now recorded in the test rather than silently frozen — see §2.2 |

---

## 2. TASK 2 — the shape, and what it is

> A completeness guard whose expected set is derived from the actual set is not
> a guard; it is a restatement.

Three sub-shapes were swept for: (A) an expected list transcribed from the code
under test rather than an external oracle; (B) a census that enumerates a
registry and asserts something about that same registry; (C) a comment claiming
exhaustiveness over a hand-maintained list.

The diagnostic question is the third column of every table below: **what input
would make it red today?** Where the answer is "nothing", that is the finding.

### 2.1 The two already-remediated instances, for context

`every_public_mac_method_is_registered` (`phases_late.rs:9578`) and
`phase59_module_vs_essential_natives` (`phases_late.rs:10286`) were both fixed by
lane E20 yesterday. They are the pattern the rest of this table is measured
against, and `phase59`'s two-way ratchet is the structural remedy §5 recommends.

**One structural fact worth stating outright:** across all of
`native-builtins/src/`, **no test reads a checked-in `.txt`/`.json` baseline and
no test invokes `javap`.** `every_public_mac_method_is_registered`'s doc block is
the only place in the crate that records an external `javap` provenance for its
expected set. That is why the same shape recurs 30 times: there was no
established way to do it differently.

### 2.2 Fixed here (owned file)

**`mac_supported_set_matches_the_advertised_sunjce_services`**
(`phases_late/ssl_security.rs`).

Its doc claimed: *"The names `mac_compute_hmac` implements are exactly the
`SunJCE` `Mac` services `jca::provider_chain` seeds."* Both sides of that
"exactly" were internal — the 12 names were transcribed from
`provider_chain.rs:1371`'s seed, which was written alongside the
`mac_compute_hmac` arms. **What made it red:** deleting an arm from
`mac_compute_hmac`. **What did not:** an arm added for a name the seed does not
carry — the direction `provider_chain.rs:1376` itself calls *"the quiet half of
this defect species, because nothing asks for a name nobody publishes"*.

Repopulated from the external oracle:

```java
for (Provider.Service s : Security.getProvider("SunJCE").getServices())
    if (s.getType().equals("Mac")) print(s.getAlgorithm());
```

**28 rows**, measured today, partitioned 12 implemented / 16 refused, with a
`assert_eq!(12 + 16, 28)` pinning the partition and the refused half asserted
`!mac_algorithm_supported`. Implementing `HmacPBESHA256` now goes **red** with a
message saying to move the name across and seed the service row in the same
change. The hyphen divergence in §1.6 is annotated in place with its measured
HotSpot rows, so the assertions below it no longer read as agreement with the
JDK. **PREDICTED: passes as the tree stands.**

**Residual, stated:** the 28 is a dated hand transcript. JDK 26 adding a SunJCE
`Mac` row would not be noticed. The honest close is generation (§5).

---

## 3. The sweep — `native-builtins/`, ranked by how load-bearing the claim is

Line numbers verified in this working tree today. "Red today" = the literal
input that fails the assertion.

### Tier 1 — the name or doc claims completeness; the expectation is internal

| # | guard | claim, verbatim | red today | external oracle it should use | denominator |
|---|---|---|---|---|---|
| 1 | `graalvm_compat.rs:2129` `test_register_total_method_count` | comment: `// 5 ImageInfo + 2 RuntimeReflection + 1 RuntimeSerialization + 1 RuntimeJNIAccess + 1 Platform = 10 methods` | **NOTHING.** The body is one line: `assert!(r.find(IMAGE_INFO, "nonExistent", "()V").is_none());`. No registration change of any kind turns it red. Only re-adding a method literally named `nonExistent` | GraalVM SDK `org.graalvm.nativeimage.*` public surface via `javap` | 10 claimed, 0 enforced |
| 2 | `tls.rs:4485` `test_ssl_parameters_registration_complete` | message: `"Expected all 13 SSLParameters methods registered"`; the comment two lines above says `// Verify we can look up all 12 SSLParameters methods` | deleting one of the exact 13 `r.register(` calls (count 13→12). The `assert_eq!(count, 13)` compares a filtered 13-row array against the literal 13 — the expected number **is** the array length | `javap -public -s javax.net.ssl.SSLParameters` = **31** public members | 13 of 31. The comment/message already disagree with each other — drift, in the file, today |
| 3 | `cds.rs:1321` `test_all_registered_methods_findable` | doc: `/// Verify every registered native can be located in the registry.` — flatly false; it verifies a hand list is findable | deleting one of the listed registrations. `register_cds_natives` makes **22**; a 23rd is invisible | `javap -p -s jdk.internal.misc.CDS` + `sun.management.CDSMetrics` | hand list of 22 registrations |
| 4 | `shared_secrets_bridge.rs:2861` `all_factories_listed` | name | editing `FACTORIES`. `assert_eq!(FACTORIES.len(), 15)` audits a const against a literal transcribed from that const | `javap -p -s jdk.internal.access.SharedSecrets` → **30** `getJava*Access` statics | 15 of 30 |
| 5 | `shared_secrets_bridge.rs:2890` `all_factory_entry_points_registered` | name | deleting one of 15 registrations. The `assert_eq!(expected.len(), 15)` is a literal against a literal in the same expression | as #4 | 15 of 30 |
| 6 | `shared_secrets_bridge.rs:2868` `every_factory_returns_access_interface` | name | typo'ing a name inside `FACTORIES`. Textbook (B): `for … in FACTORIES` asserting `starts_with("getJava")` — a property of the set derived from that set. Cannot detect a missing factory | as #4 | — |
| 7 | `jmx.rs:7010` `test_runtime_mxbean_all_getters_registered` | name `all_getters` | deleting one of the 13 listed registrations | `javap -public -s java.lang.management.RuntimeMXBean` = **17** | 13 of 17 — `getPid`, `getManagementSpecVersion`, `getSystemProperties`, `getLibraryPath` are green on absence |
| 8 | `jmx.rs:7601` `test_management_factory_returns_all_mxbeans` | comment: `this ensures all seven factory methods exist` | deleting one of the 7 | `javap -public -s java.lang.management.ManagementFactory` = **25** | 7 of 25 |
| 9 | `jmx.rs:7653` `test_all_mxbean_classes_have_init` | name `all_mxbean_classes` | removing one of 10 `<init>` registrations | the JMX platform-MXBean set | 10, hand-picked |
| 10 | `tls.rs:4518` `test_key_store_registration_complete` | name; local variable literally named `all_methods` | deleting one listed registration | `javap -public -s java.security.KeyStore` = **30** | ~13 of 30 — `store`, `getEntry`, `setEntry`, `entryInstanceOf`, `isKeyEntry`, the `getInstance` overloads all uncovered |
| 11 | `deprecated_verify.rs:430` `t85_3_cross_check_jdk25_deprecated_all_registered` | name says `cross_check_jdk25` — implies a JDK oracle | deleting a registration for a checklisted triple. **The "JDK 25 checklist" is a hand-written `Vec` at `deprecated_verify.rs:116`, in the same file as the registrars it audits.** Mitigating: `build_deprecated_registry` does not call `register_missing_deprecated_shims`, so it is not a pure tautology | the JDK 25 `@Deprecated(forRemoval=…)` set, extracted from the real modules and checked in as a frozen `.txt` | — |
| 12 | `deprecated_verify.rs:449` `t85_3_checklist_has_at_least_30_entries` | name claims a completeness floor | deleting >6 rows from the same literal it measures. **Nothing external.** The purest (B) in the crate | as #11 | — |

### Tier 2 — frozen counts with no external provenance

| # | guard | red today | oracle | note |
|---|---|---|---|---|
| 13 | `jdk25_language.rs:834` `test_java_base_exports_has_14_entries` | editing `JAVA_BASE_EXPORTS` | `java --describe-module java.base` → **58** `exports` lines | asserts 14 as fact; the real number is 58 |
| 14 | `jmx_openmbean.rs:2515` `t19_m1_composite_type_count_matches_well_known` | editing either const array | the `javax.management.openmbean` `CompositeType` set used by `java.lang.management` | comment says `"Exactly 5 well-known platform-MXBean composite types"` — "exactly" + "well-known" implies a spec; there is none |
| 15 | `graalvm_compat.rs:2388` `test_register_graalvm_compat_total_count` | `assert!(r.len() >= 12)` — deleting ≥1 registration | as #1 | one-way floor; over-registration and JDK-side omissions invisible |
| 16 | `jdk25_concurrency.rs:5113` `s52_total_registration_count` | deleting one of 14 registrations | `javap -public -s java.util.concurrent.StructuredTaskScope$Joiner` / `$Config` (JEP 505) | comment does the arithmetic `Joiner: 7 … Config: 6 … Total new = 14` off the registrar; asserts no count at all |
| 17 | `cds.rs:1299` `test_registration_count_at_least_20` | deleting ≥3 of the 22 registrations | frozen baseline of the 22 triples | name is honest ("at_least"); 1 or 2 deletions are silently green |
| 18 | `jmx.rs:7641` `test_total_registered_jmx_method_count` | `assert!(r.len() > 50)` | frozen baseline | honest `>` |
| 19 | `biginteger_intrinsics.rs:1024` `registers_all_intrinsics` | deleting one of exactly 5 registrations — the registrar makes exactly 5, so the list is a 1:1 transcription | JDK 25 `@IntrinsicCandidate` set on `java.math.BigInteger` (`vmIntrinsics.hpp`) | doc: `// 20) Registration smoke test — every native is wired.` Cannot go red for a 6th intrinsic never registered — the failure the name asserts against |

### Tier 3 — honest smoke tests; recorded so nobody re-derives that they are partial

| # | guard | asserted / true denominator | red today |
|---|---|---|---|
| 20 | `phases_late.rs:8989` `b6_submission_publisher_core_methods_registered` | **4 of 11** `SubmissionPublisher` registrations (`register_p60_flow` 8 + `register_p69_submission_publisher` 3); `javap` says 20 public members | deleting `submit`, `subscribe`, `hasSubscribers` or `getNumberOfSubscribers`. **`closeExceptionally` and `getClosedException` — the two methods `register_p69_submission_publisher` exists to add — are asserted by nothing** |
| 21 | `phases_late.rs:9304` `sq_real_rendezvous_methods_registered` | **5 of 17** `SynchronousQueue` registrations; `javap` says 25 public members | deleting one of `put`, `take`, `offer(Object)Z`, `offer(Object,J,TimeUnit)Z`, `poll(J,TimeUnit)`. Unasserted: both `<init>`, `poll()`, `peek`, `size`, `isEmpty`, `contains`, `iterator`, `toArray`, `clear`, `remainingCapacity`, `drainTo` |
| 22 | `lib.rs:4957` `essential_path_does_not_override_reflection_factory_serialization` | **4 of 18** — the doc itself says `"Those 18 methods"` | registering one of those 4 exact triples on the essential path; the other 14 `ReflectionFactory` methods can be overridden silently. Oracle: `javap -p -s jdk.internal.reflect.ReflectionFactory` |
| 23 | `lib.rs:4916` `default_build_does_not_register_synthetic_object_stream_natives` | 2 hand rows | registering one of those exact 2 |
| 24 | `util_concurrent_ext.rs:8308` `default_real_aqs_does_not_override_semaphore` | 5 hand `Semaphore` rows | registering one of those 5. Also early-`return`s on a flag check, so under `synthetic_aqs && !real_aqs` it goes green **without asserting anything** — `[flags ltch]` |
| 25 | `jdk25_concurrency.rs:4494` `w7_18_jep505_surface_is_not_shadowed_here` | hand negative list | re-registering one listed triple here. A *new* JEP 505 method registered in both places is invisible. Should compute the intersection the way `phase59_module_vs_essential_natives` does |
| 26 | `classloader.rs:13156` `every_by_name_entry_point_resolves_one_name_to_one_slot` | 4 hand `(class, field)` pairs | reintroducing a skew for one of those 4; a 5th special-cased class is invisible |
| 27 | `apps_h2.rs:3133` `h2_long_data_type_binary_search_is_registered` | 10 hand rows, message `"must be registered"` | deleting any of the 10. Oracle: `javap` on the pinned `h2-*.jar` |
| 28 | `jmx_openmbean.rs:2219`/`2228` | `for (c,m,d) in MAPPING_OVERLAY` — overlay asserted against registries built from that overlay | flipping the `with(…, bool)` gate. Cannot detect a method missing from `MAPPING_OVERLAY` |
| 29 | `atomic_updater.rs:2071` `t19_h5_register_natives_lists_every_factory` | 5 hand rows | deleting one of the 5. Its **sibling** `t19_h5_jdk_only_registers_nothing_from_this_module` (`:2126`) is the good pattern and says so: it asserts `strict.is_empty()` *"so a future registration added inside register_arfu/register_aifu/register_alfu is covered without anyone remembering to extend the list"* |
| 30 | `jca/provider_chain.rs:4529` `every_advertised_sunjce_mac_is_computable` | doc claims: *"This reads the advertised set out of the registry rather than restating it, so the list cannot be updated on one side only."* **True of the first loop, false of the second.** The reverse-direction loop at `:4563` hand-lists **6** names while `mac_compute_hmac` implements **12** | deleting a SunJCE seed row for one of `HmacMD5/SHA1/224/256/384/512`. **NOT red** for dropping the seed row of `HmacSHA512/224`, `HmacSHA512/256`, `HmacSHA3-224/-256/-384/-512` — the six added on 2026-08-12, i.e. the six the comment at `:1382` says were "Added alongside the matching `mac_compute_hmac` arms". NOM E25-2 |
| 31 | `jca/provider_chain.rs:4594` `every_keygenerator_the_engine_implements_is_advertised` | 13 hand names vs `keygen_default_bits` (`phases_early.rs:14545`), whose arms name **15** | deleting a seed row for one of the 13. **NOT red** for `TripleDES` or `RC4`, both of which `keygen_default_bits` implements and neither of which is listed. Both resolve on HotSpot (`KeyGenerator.getInstance("TripleDES")`/`("RC4")` → `SunJCE`, measured). NOM E25-3 |

### 3.1 Counter-example — the shape done right, in this tree

`jdk25_concurrency.rs:5301` `every_named_model_slot_is_at_its_real_jdk_index`
freezes `assert_eq!(m.len(), 8)` **but** compares each named slot against
`REAL_THREAD_PREFIX`, a table derived from a real JDK image — and its doc records
the real bug that found (`contextClassLoader` at index 5 where the real class has
`holder`). An external table plus a frozen count is a guard. A frozen count alone
is a restatement. Also `atomic_updater.rs:2126` (row 29), which enumerates the
registry instead of a list, on purpose, and explains why.

---

## 4. The sweep — every other crate

Scope: `reader types native-api native-collections native-io native-awt jit-api
jit jit-cuda cuda-bridge classloading craton-gpu gc vm vm-cli jfr libcratonvm
cratonvm-embed difftest` + `fuzz test-infra tools regression-suite probes`.
Vendored trees excluded.

### 4.1 Tier 1 — the gate cannot go red at all in the build CI runs

| # | guard | claim, verbatim | red today |
|---|---|---|---|
| 32 | `vm/tests/jck_conformance.rs` — file gate `:4`, `BASELINE_FLOORS` `:692`, `jck_regression_gate` `:762` | `//! NEW-16: JCK-style java.base conformance harness.` and the assert text `"The regression gate enforces the committed baseline in gaps/jdk-regression-baseline.md"` | **NOTHING, in the default build.** Line 4 is `#![cfg(feature = "synthetic-jdk")]`, so the entire conformance harness compiles out of `cargo test --workspace`. Even under `--features synthetic-jdk`, `jck_regression_gate` opens with `if !class_files_available() { eprintln!("Skipping …"); return; }` — green in 0.00 s with no corpus. Two dark layers over a gate named "regression gate". `CORPUS` and `BASELINE_FLOORS` are additionally hand tables |
| 33 | `vm/tests/wp7_2_jdbc_core_types_reachable.rs:162` `each_jdbc_core_type_has_registered_natives` | `"Each of the 6 JDBC core types must have at least one well-known public API method registered"` | **NOTHING in the default build** — `#[cfg(feature = "synthetic-jdk")]` at `:161`. When compiled: 6 hand-picked anchor triples. `[synjdk mod rots]` |
| 34 | `difftest/src/opcorpus.rs:1138` `there_is_an_entry_for_every_named_opcode_and_nothing_else` | `"one entry per JVMS mnemonic"` | **NOTHING for the property it names.** `generate_all()` is `(0..=0xc9).filter_map(matrix::opcode_name)`; the test then asserts `p.mnemonic == matrix::opcode_name(p.opcode)` — the corpus and the assertion read the same array — plus `len() == 202`, which is already a compile-time fact of `const NAMES: [&str; 202]`. A misspelled mnemonic (`invokedyanmic`), a swapped `dup_x2`/`dup2_x1`, or a wrong opcode→name mapping is invisible. Oracle: JVMS §6.5, or `javap -c` over a class exercising each opcode, frozen as a `.tsv` |
| 35 | `reader/tests/mutation_harness.rs:346` `mutation_coverage_totals_are_exact` + the four sweeps | module header: `"an *exhaustive, deterministic*"` corpus; doc: `"Pins the mutation arithmetic so the number quoted in docs/feature-designs/class-file-parser-hardening.md cannot silently drift"` | changing the seed class or an extremes array — because the test *recomputes* `truncations + byte_subs + u16_subs + u32_subs` **from the same constants the sweeps use** and compares to `218/1090/651/860/2819`. It is an arithmetic identity, not a coverage measurement: `assert_eq!(byte_subs, 1_090)` restates `len * SUBSTITUTES.len()` two lines after computing it. Each sweep body is `let _ = drive_no_panic(&mutant, &what); count += 1;`, so the sweeps assert **only** "no panic" — which is exactly what their names say (`…_never_panic_and_never_exhaust_memory`), and is not the defect here |

**A correction this record owes its own subject.** The first draft of row 35, from
a delegated sweep, read *"**No positive control.** a parser that returned `Err` on
every single mutant … passes all four families"*. **That is false**, and it was
checked rather than relayed. `mutation_harness.rs:205`
`baseline_class_parses_and_fully_decodes` is a positive control, under a section
header that states the reason in one line: *"Family 0 — the baseline must be
valid, or every other test is vacuous."* The real residual is narrower and still
worth recording: nothing asserts that the 2,819 mutants are **non-degenerate** —
if every mutant were rejected at the magic-number check, all four families and
the coverage census stay green, and the corpus would be measuring one branch
2,819 times. Oracle: assert a stated split (some mutants `Ok`, some `Err`, and a
floor on how deep into the parser the `Err`s occur). This record's entire subject
is instruments reporting their own blind spots as clean results, and a sweep
report is an instrument. `[setup lies]`, `[verify m]`.

### 4.2 Tier 2 — the guard's hole is precisely the omission it exists to catch

| # | guard | claim | red today |
|---|---|---|---|
| 36 | `vm/tests/tier1_tests.rs:2277` `t9c_synthetic_field_tables_cover_their_factories` | `"every native slot past the declared width is silently DISCARDED on a bytecode-new instance"` | a table arm NARROWER than its factory. **Not red:** an arm deleted entirely — `:2319` is `if declared == 0 { continue; }`, so `declared`→0 while the factory still asks for N is skipped. **This one is `[premise=guard]`, not a plain hole**, and the second sweep's first reading of it was too harsh: the skip carries a stated reason (`"a name with no arm has no synthesized form for new to size"`), and if that premise holds the skip is correct. The finding is that **the premise is never asserted** — nothing checks that a zero-declaring class is genuinely un-synthesized rather than a class whose arm was deleted while a factory still allocates it. A guard scoped by a stated premise is only as good as the premise, and this one is load-bearing for the guard's whole zero-case. Fix: assert the premise, not the count |
| 37 | `classloading/src/shadow_layout.rs:948` `rotated_models_now_name_fields_at_their_real_indices` | `"The real JDK 21–25 declaration order for each class whose model was rotated. **Spelled out so a JDK upgrade falsifies it**"` | this VM's `synthetic_stub_field_model` changing. **The claim is false:** a hard-coded literal cannot be falsified by a JDK upgrade. If JDK 26 reorders `java/lang/reflect/Method`, the test stays green while the model is silently wrong — the one event the sentence promises it catches. Asserted as a PREFIX, so a model that stops short also passes. Oracle: `javap -p` / a `jrt:/java.base` walk at test time, keyed by the image's `java.version` |
| 38 | `jit-api/src/lib.rs:2557`/`:2597` `jit_runtime_helpers_validate_rejects_each_required_null`, `…all_required_null_reports_every_name` | `"The test exists precisely to ensure no required field is silently uncovered."` | adding/removing a `RequiredPtr`. **Not red:** classifying a genuinely-mandatory new helper as `OptionalPtr` — the count stays 42, the null check never runs, and a null call target is a jump to address 0 in JIT'd code (`[wild jum]`). The six ad-hoc `required.contains(&"…")` pins at `:2616-2647` are a patch over exactly this. Mitigation is real though: `helper_fields!` + a `const _: () = assert!(NUM_FIELDS == size_of::<JitRuntimeHelpers>()/size_of::<usize>())` makes an absent struct field a compile error. Oracle: cross-check against the slots `vm/src/jit/helpers.rs::build_helpers` actually writes |
| 39 | `difftest/src/normalize.rs:626` `every_rule_neutralizes_its_own_probe` | `"{} states no meaningful risk — every rule has one"` | a rule disagreeing with its **own declared** `probe_expected`, a duplicate id, an empty field. **Not red:** a probe chosen so it cannot fail (`probe == probe_expected`), an over-broad rule whose real over-normalization no probe exercises, or a needed rule that does not exist. Partly redeemed by `no_rule_touches_another_rules_probe` (`:643`), which is a genuine cross-check. Oracle: real captured HotSpot/CratonVM stdout pairs from the seed corpus |
| 40 | `jit/src/x64/flag_and_header_contracts.rs:766` `header_offset_emission_site_inventory_matches_the_doc` | `"the inventory counts below are the tripwire that says \"the doc's site list is stale\""` | adding/removing a `HEADER_SIZE as u8` in one of the **18 hand-listed** `include_str!` sources. **Not red:** a brand-new `x64/*.rs` submodule full of header-offset sites — it is not in `backend_sources()`. The doc comment at `:805` (`"scans only x64.rs"`) is already stale against the code. Fix: `read_dir` the directory instead of transcribing the file list |

### 4.3 Tier 3 — enum/registry tables where the population is a hand list

| # | guard | red today | what escapes |
|---|---|---|---|
| 41 | `classloading/tests/jdk_only_class_origin.rs:76` `every_origin_category_is_distinguishable` — doc: `"**Every legitimate way a class can come into existence has its own origin, and they are all distinguishable.**"` | a collision among the 10 listed | an 11th `ClassOrigin` variant with a duplicate tag — `every_origin()` is a hand `vec![…]` |
| 42 | `difftest/src/runner.rs:1118` `all_lists_every_mode_exactly_once` | deleting a row from `Mode::all()` | `Mode::Foo` added with `label()`/`from_label()` arms but omitted from `all()`: count stays 15, and the mode becomes invisible to **every** `Mode::all()`-driven audit in the crate |
| 43 | `difftest/src/runner.rs:968` `all_mode_knobs_cover_every_override` — `"The clear-list must be a superset of every mode's set-keys"` | a mode setting a knob absent from `ALL_MODE_KNOBS` | a **stale extra** in `ALL_MODE_KNOBS` — no reverse `difference`, so a knob no mode sets is cleared forever with no signal. `[bound=2nd]` |
| 44 | `difftest/src/ledger.rs:804` `every_channel_has_a_unique_stable_label` — `"the labels … are a compatibility surface"` | a collision among the 8 listed | a 9th `Channel` omitted from `all()`, shipping with a duplicate or empty label on a wire format |
| 45 | `cuda-bridge/src/critical.rs:1966` `every_release_cause_and_relocation_policy_has_a_distinct_label` | a collision among the 4 hand-listed | a 5th `ReleaseCause` |
| 46 | `jfr/src/builtin.rs:3910` `test_builtin_exact_count_23` | adding/removing any event — `assert_eq!(registry.len(), 47); // 28 original + 19 new`. **The test name still says 23** | any divergence from the JDK's real `jdk.*` event set. Oracle: `jfr metadata` / `metadata.xml` frozen as a baseline |
| 47 | `jfr/src/jdk_only.rs:1138` `every_counter_name_is_the_planned_spelling` | renaming a counter | "planned" names no artefact. A counter that exists in the emitter but was never added to `COUNTER_NAMES`. Its sibling `enabling_flags_are_real_contract_flags` (`:1508`) parses the design doc at test time — **that** is the good pattern, in the same file |
| 48 | `vm-cli/src/main.rs:7640` `jdk_only_flag_surface_matches_the_contract` — `"The long help names every one of them."` | removing one of the 7 from clap or `LONG_ABOUT` | an 8th `--jdk-only-*` flag omitted from `LONG_ABOUT` **and** from `VALUE_TAKING_OPTS` — which is exactly the bug its own `:7675` comment warns about (`"their path argument is mistaken for the main class"`). Oracle: enumerate clap's `Command::get_arguments()` |
| 49 | `classloading/src/class_manager.rs:15465` `native_constant_surface_raw_slot_layout_audit` — `"A hand-written manifest catches these eight and only these eight."` (the comment says eight; the table has **nine** rows) | shortening one of the 9 | the ~40 other classes in the same `match` |
| 50 | `vm/tests/wp8_10_7_throwable_subclass_getmessage.rs:232` — doc says `"every Throwable subclass we promised to register"`, the code comment two lines down says `"Subset of WP8.10.7 promise"` | the loop dropping one of the 9 listed | any other `Throwable` subclass — the "future reordering" the doc says it guards |
| 51 | `classloading/src/shadow_layout.rs:913` `every_safe_claim_names_a_real_site` | emptying the table below 6, a blank field, a `site` without `.rs` | a `site` naming a file that does not exist; a well-formed but WRONG descriptor; a missing claim |
| 52 | `difftest/src/matrix.rs:1567` `the_full_execution_path_list_drives_every_axis` | removing a mode while an axis exists | message hard-codes `"must drive all seven axes"` while `execution_paths()`'s doc says `"The six execution-path modes"` and returns 7 — the two disagree in the tree today |
| 53 | `difftest/src/matrix.rs:1263` `opcode_names_cover_the_whole_jvms_range` | 5 spot-checked mnemonics + 2 reserved boundaries | the other 197 |
| 54 | `difftest/src/opcorpus.rs:1152` | a 6th opcode becoming `Recipe::Unreachable` | `assert_eq!(generated, 202 - 5, "five opcodes are unreachable from Java source")` — the 5 is a bare literal; *which* five and *why* is a `javac` question asserted from a number |
| 55 | `jit-api/src/lib.rs:1860` `test_helpers_all_fields_distinct` | two of the **31** hand-listed sharing an address | the other ~19 fields of a ~50-field struct. `all_fields()` already exists in the same file and is not used here |
| 56 | `vm/tests/tier1_tests.rs:1540` `t9b_inline_constant_native_census` (`CEILING = 327`) | a 328th constant native | **no lower bound at all** — the population can shrink to zero and the gate stays green. Unusually honest doc (`"read this number as \"approximately this many\""`), and the ceiling was raised as drift with the cause unattributed (`"I could not attribute the +1 to a commit"`) |
| 57 | `vm/tests/probe_compile_guard.rs:99`/`:124` (`FIXTURE_ALLOWED`, `ALLOWED`) — `"Keep this list short and justified — an entry here is a test that can go quiet."` | a new `vm/tests/*.rs` reaching into `apps/` without `require_fixture` | **a row for a deleted or renamed test file** — no dead-row check, so the exemption outlives the exception. `[dupwork]` / E20 §2.1's decay mode, again. Two files in this same tree get it right and can be copied verbatim: `types/tests/flag_declaration_guard.rs::the_allowlist_has_no_dead_rows:330` and `vm/src/runtime/resolve/guard.rs::the_allowlist_has_no_dead_rows:622` |
| 58 | `vm/tests/probe_fixture_census.rs:287` `NOT_A_FIXTURE` | — | same dead-row gap as #57. Its sibling `:330` is one of the best guards in the tree (two-sided: a restored fixture still baselined **fails**); the residual is that `FIXTURES` detection is 5 literal needles, so a test reaching `apps/` via `vm/tests/common/mod.rs` is invisible to both censuses |
| 59 | `vm/tests/no_test_only_public_api.rs:93` `BASELINE_OFFENDERS = 322` | a 323rd offender; `<2000` declarations scanned | the six named A7 items masked by cross-crate identifier collisions — known-dead, known-invisible, permanently. `MEMBERS` (`:107`) is a hand copy of `Cargo.toml`'s `[workspace] members`, so a crate missing from it biases the count. Doc is honest: `"It under-reports; it does not over-report."` |
| 60 | `types/tests/flag_surface.rs:40`/`:63` | any `INVENTORY`/fixture divergence | **listed as a counter-example, not a defect** — `flag-surface.txt` is a real checked-in frozen baseline. Only the hard-coded `15` and the `> 400` floor are code-derived, and its sibling states the residual better than this record can: *"Both sides are files a person edits, so it only notices a declaration that went missing — never a read site that was never declared."* |
| 61 | `gc/tests/zgc_module_integration.rs:1640` | a duplicate/failed round-trip | **weak** — `ZgcPhase::ALL` is length-typed by `ZGC_PHASE_COUNT`, so a variant omitted from `ALL` is a compile error. Residual: a phase in `ALL` the collector never emits. Doc says `"PINS … as an external contract"` but nothing external reads it |

### 4.4 Counter-examples — the shape done right elsewhere in this tree

Do not "fix" these; copy them.

* `types/tests/flag_declaration_guard.rs` — source-witness with **all four** parts:
  an allowlist dead-row check (`:330`), a **scanner self-test on synthetic input**
  (`:356`), an anti-vacuity floor (`sources.len() > 100`), and stated blind spots.
* `vm/src/runtime/resolve/guard.rs` — dead-row check (`:622`), a **planted-bypass
  test** (`a_planted_bypass_is_caught_and_the_allowlist_is_tolerated`, `:866`), and
  `the_scanner_counts_calls_and_not_prose` (`:773`). A planted bypass is the
  answer to "what would make it red" written as executable code.
* `vm/tests/wp8_10_9_string_contains_native.rs:266`
  `the_surviving_string_registration_set_is_exactly_this` — a two-sided frozen set,
  built because *"Both passed while the drop was silently deleting four
  registrations nobody had thought to name."* That is this record's finding,
  already written down once, in one file.
* `jit/src/x64/single_pass_only.rs:327` `all_variants_are_registered` — ties `ALL`'s
  index to an exhaustive `ordinal()` match and checks for gaps, so a new variant
  cannot be silently exempt. The correct fix for rows 41, 42, 44, 45.
* `types/tests/doc_citation_paths.rs` — uses `git ls-files` as the definition of
  "in the repo", with three anti-vacuity floors.
* `native-builtins/src/atomic_updater.rs:2126` and
  `jdk25_concurrency.rs:5301` — see §3.1.

---

## 5. How to regenerate these from the JDK, cheaply

Two lanes generated tables this way today and verified them by parsing the
literals back out of the working tree. The recipe, for any of rows 2, 4, 7, 8,
10, 13, 19, 20, 21, 22:

1. **Emit** — `javap -public -s <class>` (or `java --describe-module`, or a
   `jrt:` walk via `FileSystems.getFileSystem(URI.create("jrt:/"))` for a whole
   package) into a `#[rustfmt::skip]` `const EXPECTED: &[(&str, &str)]` with a
   header comment carrying the exact command, the JDK build string
   (`openjdk 25.0.3 … build 25.0.3+9-LTS`) and the date. This is what
   `every_public_mac_method_is_registered` and §2.2 now do by hand; the only
   change is that a script writes it.
2. **Ratchet both ways.** Each row carries an `expect_registered: bool`. A row
   that flips **either** direction fails. `phase59_module_vs_essential_natives`
   (`phases_late.rs:10286`) is the worked example — four failure kinds, including
   "MOVE the row" and "DELETE the row". A one-way list cannot notice a row going
   stale, and E20 found 3 of 6 `already_triaged` rows had already rotted.
3. **Verify the generator by parsing its output back.** Re-read the emitted
   `const` out of the working tree and diff it against a fresh `javap` run. That
   is the step that distinguishes a generated table from a hand table that
   claims to be generated.
4. **Re-run on JDK bump.** The residual every hand transcript carries — §2.2's
   28, `every_public_mac_method_is_registered`'s 17 — is "JDK 26 adds one". Step
   1 in CI removes it; nothing else does.

---

## 6. NOMINATIONS

### NOM E25-1 — `native-builtins/src/phases_late.rs:9590-9604` — **BLOCKING**

`every_public_mac_method_is_registered` carries `getInstance(String, Provider)`
as an explicit `false` row whose assertion is `assert!(!registered, …)`. That
overload **is registered now**. The guard is doing exactly what E20 built it to
do — a closed gap must stop being recorded — so **the tree is RED until this
lands.** Owned by the `phases_late.rs` lane.

OLD (verified unique in the working tree today):

```rust
            (
                "getInstance",
                "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;",
                false,
                "NOT REGISTERED, and never triaged: this overload was missing from \
                 this census's own population until 2026-08-13, so no run has ever \
                 asked what it does. The other two `getInstance` overloads build a \
                 4-slot synthetic `javax/crypto/Mac` and seed `mac_state_table`; \
                 this one falls through to the real JDK body, which returns a Mac \
                 that has no `mac_state_table` row, and every subsequent \
                 `init`/`update`/`doFinal` on it IS intercepted by a native that \
                 expects one. Nominated for registration in \
                 phases_late/ssl_security.rs (a file this lane does not own) — see \
                 docs/known-issues/jdk-only/E20-R11-INTERSECTION-BLIND-GUARD-20260813.md.",
            ),
```

NEW:

```rust
            (
                "getInstance",
                "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;",
                true,
                "",
            ),
```

**PREDICTED effect:** `every_public_mac_method_is_registered` returns to green
with all 17 rows `true`. The doc comment above it (`:9560-9576`) still describes
the seventeenth as unregistered; updating that prose is part of the same change,
and the sentence that must survive is the one that says the population is
`javap`'s and not the registry's.

### NOM E25-2 — `native-builtins/src/jca/provider_chain.rs:4563` — the reverse ratchet covers 6 of 12

Row 30. The loop's hand list is half the implemented set, so six SunJCE `Mac`
seed rows can be deleted with nothing going red.

OLD:

```rust
        for implemented in [
            "HmacMD5",
            "HmacSHA1",
            "HmacSHA224",
            "HmacSHA256",
            "HmacSHA384",
            "HmacSHA512",
        ] {
```

NEW:

```rust
        // All TWELVE names `phases_late::ssl_security::mac_compute_hmac` has an
        // arm for, not the six this list carried from 2026-08-12 until E25.
        // The six that were missing are exactly the six added that day
        // (`provider_chain.rs:1382`: "Added alongside the matching
        // `mac_compute_hmac` arms, never ahead of them") — so the list was
        // written before the arms and never revisited, and dropping any of their
        // seed rows was silent. `ssl_security::mac_supported_set_matches_the_
        // advertised_sunjce_services` holds the other end against HotSpot
        // 25.0.3+9's own 28-row `SunJCE.getServices()` transcript.
        for implemented in [
            "HmacMD5",
            "HmacSHA1",
            "HmacSHA224",
            "HmacSHA256",
            "HmacSHA384",
            "HmacSHA512",
            "HmacSHA512/224",
            "HmacSHA512/256",
            "HmacSHA3-224",
            "HmacSHA3-256",
            "HmacSHA3-384",
            "HmacSHA3-512",
        ] {
```

**PREDICTED effect:** passes — all twelve are seeded at `provider_chain.rs:1371`
today. Deleting any one seed row now fails.

### NOM E25-3 — `native-builtins/src/jca/provider_chain.rs:4597` — `TripleDES` and `RC4` implemented, unlisted

Row 31. `keygen_default_bits` (`phases_early.rs:14548`, `:14551`) has arms for
`TRIPLEDES` and `RC4`; the ratchet lists neither. HotSpot measured today:
`KeyGenerator.getInstance("TripleDES")` and `("RC4")` both resolve to `SunJCE`.

OLD:

```rust
        for implemented in [
            "AES", "ARCFOUR", "Blowfish", "ChaCha20", "DES", "DESede", "HmacMD5", "HmacSHA1",
            "HmacSHA224", "HmacSHA256", "HmacSHA384", "HmacSHA512", "RC2",
        ] {
```

NEW:

```rust
        // `TripleDES` and `RC4` join on 2026-08-13 (E25): `keygen_default_bits`
        // (`phases_early.rs:14548`, `:14551`) folds them onto the `DESEDE` and
        // `ARCFOUR` arms, so the engine generates keys for both names, and
        // HotSpot 25.0.3+9 resolves both to SunJCE (measured). Being folded onto
        // another arm is not the same as being the same NAME: a caller writes
        // the name, and `Security.getAlgorithms("KeyGenerator")` publishes the
        // name.
        for implemented in [
            "AES", "ARCFOUR", "Blowfish", "ChaCha20", "DES", "DESede", "HmacMD5", "HmacSHA1",
            "HmacSHA224", "HmacSHA256", "HmacSHA384", "HmacSHA512", "RC2", "RC4", "TripleDES",
        ] {
```

**PREDICTED effect:** this one may go **RED**, and that is the finding.
`provider_chain.rs:1371`-style seeding for `KeyGenerator` must be read before
landing: if `TripleDES`/`RC4` have no service row, the assertion fires and the
fix is to add the rows — which is the defect the test exists for, surfacing for
the first time. Do not delete the names to make it pass.

### NOM E25-4 — `native-builtins/src/graalvm_compat.rs:2129` — a guard that asserts nothing

Row 1, and the loudest finding in the sweep: **no input makes it red.**

OLD:

```rust
    #[test]
    fn test_register_total_method_count() {
        let r = make_registry();
        // 5 ImageInfo + 2 RuntimeReflection + 1 RuntimeSerialization
        // + 1 RuntimeJNIAccess + 1 Platform = 10 methods
        // Verify a few are not present to confirm no over-registration
        assert!(r.find(IMAGE_INFO, "nonExistent", "()V").is_none());
    }
```

NEW:

```rust
    /// The name says "total method count" and the comment does the arithmetic;
    /// until 2026-08-13 the body asserted neither. The only assertion was that a
    /// method literally named `nonExistent` is absent, so no registration change
    /// of any kind could turn this test red — including deleting every
    /// registration in the module. Renamed intent, plus the count it claimed.
    #[test]
    fn test_register_total_method_count() {
        let r = make_registry();
        // 5 ImageInfo + 2 RuntimeReflection + 1 RuntimeSerialization
        // + 1 RuntimeJNIAccess + 1 Platform = 10 methods
        assert_eq!(
            r.len(),
            10,
            "the registrar's method count changed. If you ADDED a native, update \
             this number and the tally above it in the same edit; if it DROPPED, \
             a registration was lost. This assertion replaces a body that could \
             not fail."
        );
        // Retained: a name nothing registers must stay unregistered, which is
        // the over-registration half.
        assert!(r.find(IMAGE_INFO, "nonExistent", "()V").is_none());
    }
```

**PREDICTED effect:** unknown until run — `r.len()` is asserted here for the
first time, and 10 is the comment's arithmetic, not a measurement. If it fails,
the number in the failure text is the true count and the comment was wrong; fix
the comment, not the assertion. Note `test_register_graalvm_compat_total_count`
(`:2388`) asserts `r.len() >= 12` on a *different* registry, so the two numbers
are not comparable.

### NOM E25-5 — `native-builtins/src/tls.rs:4485` — the comment and the message disagree, in the file, today

Row 2. `// Verify we can look up all 12 SSLParameters methods` sits three lines
above `"Expected all 13 SSLParameters methods registered"` over a 13-row array.
`javap -public -s javax.net.ssl.SSLParameters` reports **31** public members.

OLD:

```rust
        // Verify we can look up all 12 SSLParameters methods
```

NEW:

```rust
        // NOT "all" — 13 of the 31 public members `javap -public -s
        // javax.net.ssl.SSLParameters` reports on openjdk 25.0.3+9. The
        // remaining 18 (`getServerNames`, `setSNIMatchers`,
        // `getAlgorithmConstraints`, `setUseCipherSuitesOrder`, …) are
        // unregistered and unasserted, so this test cannot notice one going
        // missing. The count below is the length of the array below it, which
        // makes `assert_eq!(count, 13)` a restatement: it fails only when one of
        // these exact 13 registrations is deleted. Regenerate the population
        // from `javap` with a per-row `expect_registered` flag to make it a
        // census — see docs/known-issues/jdk-only/
        // E25-R11-GUARD-POPULATION-SWEEP-20260813.md §5.
        // (The comment previously said "all 12" over a 13-row array asserting 13.)
```

**PREDICTED effect:** none — comment only. Renaming the test from
`..._registration_complete` to `..._core_registrations_present` is the honest
follow-up but touches call sites, so it is left to the owning lane.

### NOM E25-6 — `native-builtins/src/cds.rs:1319` — a doc that states the opposite of the body

Row 3.

OLD:

```rust
    /// Verify every registered native can be located in the registry.
```

NEW:

```rust
    /// Verify a hand-maintained list of natives can be located in the registry.
    ///
    /// It does NOT verify "every registered native", which is what this line
    /// said until 2026-08-13: the population is the `expected` array below,
    /// transcribed from `register_cds_natives`, which makes 22 `r.register(`
    /// calls. A 23rd registration is invisible to it, and so is any
    /// `jdk.internal.misc.CDS` / `sun.management.CDSMetrics` method this VM
    /// never registered — which is the failure the old wording claimed to cover.
    /// The external oracle is `javap -p -s jdk.internal.misc.CDS` on the pinned
    /// JDK 25 image; see E25-R11-GUARD-POPULATION-SWEEP-20260813.md §5.
```

**PREDICTED effect:** none — comment only.

### NOM E25-7 — `native-builtins/src/shared_secrets_bridge.rs:2861` — 15 of 30

Rows 4-6. `javap -p -s jdk.internal.access.SharedSecrets` on this host reports
**30** `getJava*Access` statics; `FACTORIES` carries 15, and
`assert_eq!(FACTORIES.len(), 15)` compares that const against a literal copied
out of it. No replacement text is offered, deliberately: closing this means
deciding, for each of the 15 absent accessors, whether it is unimplemented or
deliberately out of scope — and that is the same judgement call E20 declined to
make for `SubmissionPublisher`. The oracle command and the number 30 are the
contribution; the list is the owning lane's to write, with `expect_registered`
flags per §5 step 2 so the absences are recorded rather than omitted.

### NOM E25-8 — `native-builtins/src/util_concurrent_ext.rs:8308` — a guard that can no-op silently

Row 24. `default_real_aqs_does_not_override_semaphore` early-`return`s on a flag
check, so in the `synthetic_aqs && !real_aqs` configuration it passes without
executing a single assertion — `[flags ltch]`, and the same shape as a green run
that measured nothing. The minimal fix is to make the skipped configuration
loud rather than silent:

OLD (the early return; anchor to be confirmed by the owning lane against the
exact flag expression in the working tree):

```rust
            return;
```

NEW:

```rust
            // Do not return silently: a configuration in which this test asserts
            // NOTHING must say so, or a green run reads as coverage it does not
            // have. `[2cfgs]` — verify both feature configs, not just the one
            // you are working on.
            eprintln!(
                "default_real_aqs_does_not_override_semaphore: SKIPPED — the \
                 synthetic-AQS configuration does not exercise the default path. \
                 This test asserted nothing in this configuration."
            );
            return;
```

**PREDICTED effect:** none on pass/fail; the skipped configuration becomes
visible in test output. The stronger fix — running the assertions under both
configurations — needs a build.

### NOM E25-9 — `native-builtins/src/phases_late/ssl_security.rs` — deferred in this lane's OWN file

The `(String, String)` overload still uses `mac_algorithm_arg`, so
`Mac.getInstance(null, "SunJCE")` answers `NoSuchAlgorithmException: Algorithm
SunJCE not available` (PREDICTED) where HotSpot answers `NullPointerException:
null algorithm name` (measured). The fix is one identifier — swap in
`mac_algorithm_arg_string_typed` — and it is **not** applied here because
`mac_algorithm_arg`'s own doc records that `vm/src/vm/tests.rs::crypto_mac_basics_p68`
calls these natives under a receiver-placeholder convention, and this lane
cannot run that test. The new helper is written to degrade to the old behaviour
when the context cannot report a class, so the change is believed safe; "believed
safe" is not "measured", and a wrong exception type on a null-algorithm call is
too small a prize for an unmeasured change to a path SCRAM logins run through.
Land it behind a run.

### NOM E25-10 — `vm/tests/jck_conformance.rs:763` — a "regression gate" that skips silently

Row 32. Two dark layers: `#![cfg(feature = "synthetic-jdk")]` at `:4` removes the
harness from the default build entirely, and the gate then skips on a missing
corpus. The `#[cfg]` is deliberate and documented and is **not** nominated —
`[cfg≠guard]`, and the owning lane decided that. The silent skip is.

OLD (`:763`; verified unique — the sibling at `:722` has different text):

```rust
    if !class_files_available() {
        eprintln!("Skipping jck_regression_gate: .class files not available");
        return;
    }
```

NEW:

```rust
    if !class_files_available() {
        // A gate named "regression gate" that returns green in 0.00 s with no
        // corpus is indistinguishable, in CI output, from one that ran and
        // passed. `CRATONVM_REQUIRE_E2E` is the in-tree switch that turns a
        // skipped fixture into a failure (`vm/tests/common/mod.rs`
        // `require_fixture`); honour it here so at least one configuration
        // cannot go green by having no inputs.
        if std::env::var("CRATONVM_REQUIRE_E2E").is_ok() {
            panic!(
                "jck_regression_gate: .class files not available, and \
                 CRATONVM_REQUIRE_E2E is set. This gate enforces the baseline in \
                 gaps/jdk-regression-baseline.md; with no corpus it enforces \
                 nothing."
            );
        }
        eprintln!(
            "Skipping jck_regression_gate: .class files not available — \
             THIS GATE ASSERTED NOTHING. Set CRATONVM_REQUIRE_E2E to make this \
             a failure."
        );
        return;
    }
```

**PREDICTED effect:** no change to any current run (nothing sets
`CRATONVM_REQUIRE_E2E` in the default path); the skip becomes visible in test
output, and a CI job can opt into failing on it. Verify the env-var spelling
against `vm/tests/common/mod.rs` before landing — this lane read the mechanism's
name, not its implementation.

### NOM E25-11 — `classloading/src/shadow_layout.rs:948` — a claim its mechanism cannot keep

Row 37. Comment only; the assertion is left alone.

OLD:

```rust
    /// rotated. Spelled out so a JDK upgrade falsifies it, and asserted as a
```

NEW:

```rust
    /// rotated. **A JDK upgrade does NOT falsify this** — that is what this line
    /// claimed until 2026-08-13, and a hard-coded literal cannot be falsified by
    /// a change in the image it was copied from. If JDK 26 reorders
    /// `java/lang/reflect/Method`, this test stays green while the model is
    /// silently wrong, which is precisely the event the old wording promised it
    /// caught. What it DOES catch is this VM's own model drifting away from the
    /// JDK 21-25 order recorded here. Closing the gap means reading the order
    /// from the image at test time (`javap -p`, or a `jrt:/java.base` walk keyed
    /// by the running image's `java.version`) — see
    /// docs/known-issues/jdk-only/E25-R11-GUARD-POPULATION-SWEEP-20260813.md §5.
    /// Asserted as a
```

**PREDICTED effect:** none — comment only.

### NOM E25-12 — `vm/tests/probe_compile_guard.rs` — two allowlists with no dead-row check

Row 57. `ALLOWED` (`:124`) and `FIXTURE_ALLOWED` (`:99`) only ever grow; a row
naming a deleted or renamed test file survives forever and pre-exempts any future
file that reuses the name. That is E20 §2.1's decay mode — 3 of 6
`already_triaged` rows had already rotted the first time anyone looked — in a
file whose own comment says *"an entry here is a test that can go quiet."*

No replacement text is offered because the fix is a **verbatim copy** of a test
that already exists twice in this tree and is written for exactly this:
`types/tests/flag_declaration_guard.rs::the_allowlist_has_no_dead_rows` (`:330`)
and `vm/src/runtime/resolve/guard.rs::the_allowlist_has_no_dead_rows` (`:622`).
Both assert that every allowlist row still names a file that exists **and** still
trips the needle it was exempted from. Copy one, twice, once per list.

**PREDICTED effect:** may go **RED**, and that is the finding — the same way E20's
`already_triaged` did. A row that fails is a fixed exemption still being granted.

### NOM E25-13 — the rest of §3 and §4, table-only

Rows 7-10, 13-19, 20-31, 38-56, 58-59, 61 carry no replacement literal. Each is
one of two things and the table says which: a hand list that needs a generated
population (§5), or an honest smoke test whose only defect was being read as more
than it claims. Neither is a one-line edit, and offering a literal for a list
whose correct membership needs a `javap` walk plus a per-row
"unimplemented or deliberately out of scope?" judgement would be the same species
of guess this record is about. The oracle command and the true denominator are
given for each, which is the part that was missing.

---

## 7. The lesson

E20 wrote: *"A guard's population is a claim, and it is usually the least
examined line in the file."* This sweep says something narrower and worse.

**The population is not usually examined because there was no way to examine
it.** Across `native-builtins/src/` — 30 guards of this shape — **not one test
reads a checked-in baseline file, and not one invokes `javap`.** Every expected
set in the crate was typed by a person reading the code beside it. That is not
thirty independent lapses; it is one missing capability, thirty times.
`[refusal=no cap]`.

The tell held everywhere it was checked. `test_register_total_method_count` is
named for a count, carries a comment computing that count, and asserts a single
negative — **nothing** makes it red. `test_ssl_parameters_registration_complete`
says "all 12" and "all 13" three lines apart, over 13 of 31. `cds.rs`'s
`/// Verify every registered native can be located` verifies a hand list.
`t85_3_checklist_has_at_least_30_entries` measures a literal against a literal.
In each case the gate was green and nobody could say what would make it red —
and in each case the answer was one `javap` away.

**And the shape survives its own fix.** `every_advertised_sunjce_mac_is_computable`
was written *because* two lists had drifted, its doc explains that it "reads the
advertised set out of the registry rather than restating it", and its second
loop restates six of twelve. A guard built to catch drift drifted, in the half
its own doc did not describe. `[both halves]` — memoize, or ratchet, both halves
of a gate, not just the one that was failing when you wrote it.

**Outside `native-builtins/` the capability exists and the discipline is
uneven.** `flag_declaration_guard.rs` and `resolve/guard.rs` have all four parts
of a real source-witness — a dead-row check, a scanner self-test on synthetic
input, an anti-vacuity floor, and a **planted bypass**. A planted bypass is
"what would make this red?" written as executable code, and it is the only
answer to that question that cannot itself go stale. Two files have one. Sixty
guards do not. `probe_compile_guard.rs` sits three directories from a
`the_allowlist_has_no_dead_rows` it could have copied verbatim, and its
allowlists have grown one-way ever since.

**The most expensive rows are the ones that name a real oracle and then do not
use it.** `rotated_models_now_name_fields_at_their_real_indices` says *"Spelled
out so a JDK upgrade falsifies it"* over a hard-coded literal — the sentence
describes the mechanism the test would need and the test does not have it, so a
reader who checks the comment is *more* misled than one who reads only the code.
`jck_conformance.rs` calls itself a regression gate enforcing a committed
baseline, and does not exist in the build CI runs. `test_register_total_method_count`
computes its count in a comment and asserts a single unrelated negative. In each
case the prose is the specification and nothing holds the code to it.

**Finally, a sweep is an instrument too.** Two rows in this record arrived from
delegated passes with confident wrong readings — "no positive control" for a
harness whose §0 is a positive control, and "a plain hole" for a skip that
carries a stated premise. Both were checked against the source before landing,
and both corrections are in §4.1 and row 36 rather than quietly dropped, because
a record that silently discards its instrument's misses is doing the thing it
was written to name. `[verify m]`, `[setup lies]`.
