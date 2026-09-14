# E41 / R11 — the twelve guards converted, and the three class names JDK 25 does not have

**Date:** 2026-08-13 **Lane:** E41
**Closes the residual named in:** `E37-R11-JDK-BASELINE-CONSUMER-AND-RATCHET-20260813.md` §7.1
— *"twelve rows convert with data already checked in … Row 16 is the one to do
next, and to expect red."*
**Status:** the parser extension is LANDED and **MEASURED** (25/25 in the module,
38/38 with the twelve converted bodies). The twelve guard rewrites are
**NOMINATIONS** — every one of them lives in a file another lane owns.

**This lane did not run `cargo` and did not build or run CratonVM.** Every
"MEASURED" below was taken with `rustc --edition 2021 --test` on
`native-builtins/src/jdk_baseline.rs`, which has no crate dependency (`use
std::collections::BTreeSet;` is its only `use`), plus a scratch harness that
`#[path]`-includes that same file and carries the twelve NEW bodies verbatim.
`javap` and a `jrt:/` walk were run on this host against
`openjdk 25.0.3 2026-04-21 LTS, Microsoft-13877124, build 25.0.3+9-LTS`.

**Edits applied (owned files only):**

| what | where |
|---|---|
| `audit_off_surface` — the fifth check, plus `OffSurface` | `native-builtins/src/jdk_baseline.rs` |
| four live re-enactments + four synthetic-input tests for it | same, `mod tests` |
| two new baselines wired in; the anti-vacuity floor 30 → 32 | same |
| `sun.reflect.ReflectionFactory`, `sun.management.ManagementFactoryHelper` | `scripts/jdk-baseline/classes.txt` |
| the two generated baselines | `scripts/baselines/jdk25-sun.*.tsv` (**NEW**) |

`rustfmt --check --edition 2021 native-builtins/src/jdk_baseline.rs` is clean.
`python scripts/jdk-baseline/generate.py --check` reports **32 baselines agree
with this JDK**, and the thirty pre-existing files are byte-identical after a
full `--update`, which is the generator's own determinism check.

---

## 1. The measurement, first

```
$ CARGO_MANIFEST_DIR=native-builtins \
  rustc --edition 2021 --test -o jdkbl_test.exe native-builtins/src/jdk_baseline.rs
$ ./jdkbl_test.exe --test-threads 1
test result: ok. 25 passed; 0 failed;   (was 17 before this lane)

$ rustc --edition 2021 --test -o harness.exe scratchpad/f8/harness.rs
$ ./harness.exe --test-threads 1
test result: ok. 38 passed; 0 failed;   (25 + the 13 converted bodies)
```

Zero `rustc` warnings. The harness is **not** checked in: it exists so the
twelve NEW bodies below are measured rather than predicted. Its registration
oracle is a mechanical extraction of `r.register(CLASS, "name", "desc"` over
the registrar call graph — the same technique E37 used for
`SubmissionPublisher`, generalised, and it independently reproduces E37's count
of **11** `SubmissionPublisher` registrations from a different program.

**The residual that leaves:** the oracle is an extraction, not
`NativeMethodRegistry`. A conditional registration, a feature-gated branch or
an intra-fixture overwrite would break the prediction without breaking the
extraction. `[setup lies]`. It is the same residual E37 named and it is not
closed here; what is new is that it now applies to thirteen classes instead of
one, and that **`audit`'s verdict on the JDK side of every one of them is
independent of it** — a `STALE` row is a fact about the baseline alone.

---

## 2. What `audit` structurally could not see, and the fifth check

E37 built a four-kind ratchet and called kinds 3 and 4 "the ones that get
omitted". Converting twelve guards found a fifth thing, and it is the one that
mattered most in practice.

**`audit` walks two populations: the triage rows and the JDK surface. A
registration in neither is invisible to all four kinds.** Kind 4 fires only
when somebody wrote a row for the thing — and the whole premise of E25 is that
nobody writes rows for what they are not already thinking about. So `audit`
catches a fabricated name in a *hand list* and misses the same fabricated name
in the *registrar*, which is the copy that is loaded into a running VM.

`jdk_baseline::audit_off_surface` closes it from the other end, from
`NativeMethodRegistry::dump_registrations()`:

| # | condition | output |
|---|---|---|
| 5 | a registration the JDK does not declare, with no row | `OFF-SURFACE` / `OFF-SURFACE (descriptor)` |
| 6 | an `expected` row nothing registers any more | `DEAD OFF-SURFACE row` |
| 7 | an `expected` row the JDK *does* declare publicly | `STALE OFF-SURFACE row` |

Kinds 6 and 7 are the standing-permission shape again, one level up: a record
that says *"we knowingly register something the JDK does not have"* has to stop
saying it the moment either half stops being true. `[both halves]`.

**Off-surface is not automatically a defect, and the `reason` field is where
that judgement is written down.** Three shapes recur, and only two of them may
ever be recorded:

1. **a synthetic `<init>()V`** on a class whose real constructor is protected,
   private or absent — this VM allocates synthetic stubs by that key. Recorded.
2. **an inherited member** — `javap -public` on a leaf never lists one, so
   `getObjectName` on `RuntimeMXBean` is declared by `PlatformManagedObject`
   and is genuinely reachable. Recorded; baselining the declaring type moves
   the row onto the surface and kind 7 then forces that.
3. **a name this JDK does not have anywhere**, which is the `$Config` shape.
   **This one is never recorded.** A `reason` on it would be exactly the
   standing permission the ratchet exists to remove, so the converted guard
   goes red and stays red until the registration is fixed or deleted.

That rule is what makes six of the twelve nominations below red on landing.
It is deliberate and it is the finding. `[gate=FR]`.

---

## 3. Three class names, and a fourth spelling, that JDK 25 does not have

E32 found `StructuredTaskScope$Config` because the generator refused to write a
baseline for it. Converting the twelve found three more of the same species —
two class names and, more insidiously, one *method* name spelled with a prefix
the JDK never used. All four are measured, not inferred.

### 3.1 `sun.management.CDSMetrics` — not in the runtime image

A `jrt:/` walk of every root directory on this host:

```
sun/management/CDSMetrics       -> NOT IN IMAGE
sun/reflect/ReflectionFactory   -> PRESENT
jdk/internal/misc/CDS           -> PRESENT
```

`cds.rs::register_cds_natives` makes **six** registrations on
`sun/management/CDSMetrics` and a seventh,
`ManagementFactoryHelper.getCDSMetrics()Lsun/management/CDSMetrics;`, that
returns one. `test_all_registered_methods_findable` lists all seven and finds
all seven, because it transcribed them from the registrar.

There is no baseline to convert that half against and there never will be — the
generator refuses a type that is not in the image, and the missing file **is**
the finding. `classes.txt` now says so in place of the entry, so the next
person to notice the gap reads the answer instead of adding the line.

What *can* be checked is the owner, and it says the same thing from the other
side. `sun.management.ManagementFactoryHelper` is in the image, was baselined
today, declares **22** public methods, and `getCDSMetrics` is not among them at
any access level. A native registered under a name its own owner class does not
declare is a native no bytecode can reach. Test:
`management_factory_helper_does_not_declare_get_cds_metrics`.

### 3.2 `SharedSecrets.getJavaSecurityAccess` — deleted with the Security Manager

`shared_secrets_bridge.rs`'s `FACTORIES` is fifteen rows and
`all_factories_listed` asserts `FACTORIES.len() == 15` — a const against a
literal copied out of that const. `javap -p -s jdk.internal.access.SharedSecrets`
on this host reports 65 public members, and **two of the fifteen name a method
that is not one of them**:

* **`getJavaSecurityAccess`** — `JavaSecurityAccess` went with the Security
  Manager (JEP 486). The three `getJavaSecurity*Access` methods that do exist
  are `Properties`, `Signature` and `Spec`.
* **`getJavaUtilJarAccess`** — the real spelling has never carried the `get`
  prefix. The JDK's method is `javaUtilJarAccess()`; it is right there in the
  same baseline, and this tree invented the other name.

Both are registered on `jdk/internal/access/SharedSecrets` **and** on the
legacy `jdk/internal/misc/SharedSecrets` alias, so four registrations are keyed
on names no real call can produce. `every_factory_returns_access_interface` —
the guard whose entire body is `starts_with("getJava")` and
`ends_with("Access")` — passes both of them. Test:
`kind_four_catches_two_of_the_fifteen_shared_secrets_factories`.

### 3.3 `jdk.internal.misc.CDS` — five of ten, including one right name

Ten registrations on a class that *is* in the image. Five name something JDK 25
does not have:

| registered | JDK 25 declares |
|---|---|
| `isDumpingClassList()Z` | nothing; the near neighbour is `isDumpingStaticArchive()Z` |
| `isSharingEnabled()Z` | nothing; the near neighbour is `isUsingArchive()Z` |
| `logLambdaFormInvoker(Ljava/lang/String;)V` | the same name with **four** `String` parameters |
| `dumpClassList(Ljava/lang/String;)V` | nothing |
| `dumpDynamicArchive(Ljava/lang/String;)V` | nothing |

`logLambdaFormInvoker` is the sharp one and the reason kind 4 was split into
sub-cases: the name matches, so any name-keyed search reports agreement, and
only the descriptor comparison says otherwise. Test:
`kind_four_catches_five_of_the_ten_cds_registrations`.

### 3.4 `StructuredTaskScope` — the outer class, which is real, and still wrong

E32's finding was the nested `$Config`. The outer type is real and the tree
gets four things wrong about it at once — three names JEP 505 did not ship and
one that is declared with a different return type:

* `isShutdown()Z` — JEP 505 shipped `isCancelled()Z`.
* `shutdown()V` — gone; cancellation is the joiner's business now.
* `joinUntil(Ljava/time/Instant;)…` — gone; a deadline is
  `Configuration.withTimeout(Duration)`.
* `join()` — declared, returning `Ljava/lang/Object;`, not
  `Ljava/util/concurrent/StructuredTaskScope;`. The preview API returned the
  scope for chaining; the final one returns the joiner's result.

Plus `$Joiner.policy()I` and `$Subtask.task()Ljava/util/concurrent/Callable;`,
neither of which is a member of anything. `s52_total_registration_count`
asserts none of these: its fourteen triples are seven `$Joiner`, six `$Config`
and exactly one on the outer class, so every row above is outside its
population. Test: `kind_four_catches_the_outer_structured_task_scope_surface`.

---

## 4. The per-row before/after table

"Asserted" is what the guard names today; "denominator" is the JDK's public
surface (public methods **plus** public constructors, the population a native
registrar can key on). "Off" is registrations on the class the JDK does not
declare — the fifth check.

| E25 row | guard | asserted | denominator | off | verdict on landing |
|---|---|---|---|---|---|
| 2 | `tls.rs` `test_ssl_parameters_registration_complete` | 13 | **31** | 0 | GREEN |
| 3 | `cds.rs` `test_all_registered_methods_findable` (CDS half) | 10 of 22 | **13** | **5** | **RED — 5** |
| 4-6 | `shared_secrets_bridge.rs` ×3 | 15 | **65** | **2** | **RED — 2** |
| 7 | `jmx.rs` `test_runtime_mxbean_all_getters_registered` | 13 | **17** | 2 | GREEN |
| 8 | `jmx.rs` `test_management_factory_returns_all_mxbeans` | 7 | **16** | **2** | **RED — 1** |
| 10 | `tls.rs` `test_key_store_registration_complete` | 12 | **30** | 1 | GREEN |
| 13 | `jdk25_language.rs` `test_java_base_exports_has_14_entries` | 14 | **58** | n/a | GREEN |
| 16 | `jdk25_concurrency.rs` `s52_total_registration_count` | 14 | **8 + 8 + 3 + (no such class)** | **6** | **RED — 6, and does not compile** |
| 20 | `phases_late.rs` `b6_submission_publisher_…` | 4 | **20** | 0 | GREEN (E37's, verified) |
| 21 | `phases_late.rs` `sq_real_rendezvous_methods_registered` | 5 | **25** | 0 | GREEN |
| 22 | `lib.rs` `essential_path_does_not_override_…` | 4 | **25 + 14** | **1** | **RED — 1** |

Two corrections to E37 §7.1's own table, both from measurement:

* Row 8's denominator is **16**, not 25. `javap -public
  java.lang.management.ManagementFactory | grep -c '('` on this host today is
  **16**, and the class file agrees: 16 public methods, no public constructor.
  E25 row 8 and E37 §7.1 both say 25 and neither shows the transcript; this is
  a correction to both, taken from the instrument rather than reasoned about.
  It is the smaller finding of the two — the guard asserted 7 either way.
* Rows 4-6's denominator is **65**, not 30. Thirty is the `getJava*Access`
  getter family alone; the class a native registrar can key on is the whole
  public surface, and the setters are just as registrable as the getters.

### 4.1 One sentence per row: the input that now makes it fail

Where this sentence is hard to write, the row is not converted. All twelve are
below.

| row | the input that makes it red |
|---|---|
| 2 | Registering `SSLParameters.setServerNames(Ljava/util/List;)V` — one of the 18 members nobody has triaged — without adding its row, or flipping any of the 18 `false` rows by implementing it and leaving the record saying "absent". |
| 3 | Deleting `CDS.isDumpingArchive()Z` from the registrar (`DROPPED`), or renaming `isDumpingClassList` to the real `isDumpingStaticArchive` without deleting its off-surface row (`DEAD OFF-SURFACE row`). It is red **today** for the five names JDK 25 does not have. |
| 4-6 | Adding a sixteenth `FACTORIES` row whose method name is not a `SharedSecrets` member — the exact failure `every_factory_returns_access_interface`'s `starts_with("getJava")` admits. It is red **today** for `getJavaSecurityAccess` and `getJavaUtilJarAccess`. |
| 7 | Deleting `register_runtime_mxbean`'s `getSystemProperties()Ljava/util/Map;` registration, which the old thirteen-row list never named; or implementing `getPid()J` and leaving its `false` row (`CLOSED`). |
| 8 | Implementing `ManagementFactory.getPlatformMBeanServer()` and leaving its `false` row — the deliberate absence the old comment explained in prose and asserted nowhere. It is red **today** for `loadNativeLib()V`. |
| 10 | Deleting `KeyStore.store(Ljava/io/OutputStream;[C)V` or `isKeyEntry` — both registered, neither named by the old twelve-row `all_methods`. |
| 13 | Adding a fifteenth `JAVA_BASE_EXPORTS` entry that `java.base` does not export unqualified — e.g. `sun/nio/ch`, or a typo'd `java/util/streams`. |
| 16 | Nothing, ever, in its present form: `CLS_CONFIG` names a type that is not in the runtime image, so the converted guard **does not compile** — `include_str!` cannot resolve a baseline the generator refuses to write. Once respelled `$Configuration`, six registrations move to a class that declares three of them. |
| 20 | Deleting `register_p69_submission_publisher`, which drops `closeExceptionally` and `getClosedException` — the two methods p69 exists to add and the old four-row body named neither. |
| 21 | Deleting `SynchronousQueue.drainTo(Ljava/util/Collection;)I` or `remainingCapacity()I`; twelve of the seventeen registrations were unasserted. |
| 22 | Registering **any** of the 39 `ReflectionFactory` members on the essential path, not the four the old body happened to name. It is red **today** for `getConstantPool`. |

---

## 5. NOMINATIONS

### NOM E41-0 — **BLOCKING** — `native-builtins/src/lib.rs` — the module declaration

Unchanged from **NOM E37-1**, which has **not landed**: `grep -n jdk_baseline
native-builtins/src/lib.rs` returns nothing in the working tree today.
`native-builtins/src/jdk_baseline.rs` is committed and is **not compiled by
anything**. Every nomination below depends on it, and until it lands the module
is a file that can never go red — which is the property this whole capability
exists to remove, reproduced one level up. `[reach≠defect]`.

OLD (verified unique — the only `pub(crate) mod test_utils;` in the file):

```rust
#[cfg(test)]
pub(crate) mod test_utils;
```

NEW:

```rust
#[cfg(test)]
pub(crate) mod test_utils;

/// Reads `scripts/baselines/jdk25-*.tsv`. Test-only: the baselines are an
/// oracle for guards, never an input to the VM.
///
/// No crate dependency — the format is TSV precisely so that a `#[cfg(test)]`
/// consumer needs nothing but `str::split`. See
/// docs/known-issues/jdk-only/E37-R11-JDK-BASELINE-CONSUMER-AND-RATCHET-20260813.md
/// and docs/known-issues/jdk-only/E41-R11-TWELVE-GUARDS-CONVERTED-20260813.md.
#[cfg(test)]
pub(crate) mod jdk_baseline;
```

**PREDICTED: compiles and adds 25 passing tests.** The module is measured under
`rustc --test` (§1); what is predicted is only that `cargo test -p
cratonvm-native-builtins` agrees with `rustc` on a file with no crate
dependencies. `dead_code` is allowed crate-wide (`lib.rs:9`), so the accessors
no guard calls yet do not warn.

### NOM E41-1 — rows 4-6 — `SharedSecrets`, 15 of **65**, and two names that are not members

**File:** `native-builtins/src/shared_secrets_bridge.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/shared_secrets_bridge.rs:2889-2907` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

E37 §7.1 called this "the largest silent gap" and E25 NOM E25-7 declined to write the list because it needed a per-accessor judgement. The judgement below is deliberately uniform and honest — `"Not registered here; not measured."` — because this lane could not run the VM to find out which of the fifty-two absences a real caller reaches. The value is not that the decision has been made; it is that the decision is now **recorded and enforced in both directions**.

The two `STALE` rows are not a judgement call. They are measurement.

`all_factories_listed` and `every_factory_returns_access_interface` (`native-builtins/src/shared_secrets_bridge.rs:2860-2865`, `native-builtins/src/shared_secrets_bridge.rs:2867-2874`) should be **deleted** in the same change: the first asserts a const against a literal copied out of it, and the second asserts a property of the set derived from that set. Both are subsumed here.

OLD:

```rust
    #[test]
    fn all_factory_entry_points_registered() {
        // Instantiate a fresh registry and apply the bridge.
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        for (method, ret, _) in FACTORIES {
            let desc = format!("(){}", ret);
            assert!(
                r.find("jdk/internal/access/SharedSecrets", method, &desc)
                    .is_some(),
                "factory {method} not registered (descriptor {desc})"
            );
            assert!(
                r.find("jdk/internal/misc/SharedSecrets", method, &desc)
                    .is_some(),
                "legacy-package factory {method} not registered"
            );
        }
    }
```

NEW:

```rust
    /// The population is **the JDK's**, not `FACTORIES`'.
    ///
    /// `scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv`, generated
    /// from `jrt:/modules/java.base/jdk/internal/access/SharedSecrets.class` on
    /// openjdk 25.0.3+9: **64 public methods plus one public constructor = 65**
    /// members. `FACTORIES` (`shared_secrets_bridge.rs:61`) carries **15**, and
    /// the body this replaced asserted that those 15 resolve — a list against
    /// itself. E25 rows 4-6 quote "15 of 30"; 30 is the `getJava*Access` getter
    /// family alone, and a native registrar can key on any of the 65.
    ///
    /// **Two of the fifteen name a method this JDK does not declare** and both
    /// come back `STALE`:
    ///
    /// * `getJavaSecurityAccess` — `JavaSecurityAccess` went with the Security
    ///   Manager (JEP 486). The surviving `getJavaSecurity*Access` methods are
    ///   `Properties`, `Signature` and `Spec`.
    /// * `getJavaUtilJarAccess` — the JDK's method is `javaUtilJarAccess()`, with
    ///   no `get` prefix. It is in this same baseline.
    ///
    /// `every_factory_returns_access_interface` passed both, because its whole
    /// body was `starts_with("getJava")` and `ends_with("Access")`.
    ///
    /// **Scope, stated:** `register_factories` registers each factory on
    /// `jdk/internal/access/SharedSecrets` AND on the legacy
    /// `jdk/internal/misc/SharedSecrets`. This fixture audits the first; the
    /// second is asserted method-for-method below it, against the same TRIAGE, so
    /// a factory added to one package and not the other is red.
    #[test]
    fn jdk_surface_sharedsecrets_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        let cls = "jdk/internal/access/SharedSecrets";
        let baseline = jdk_baseline::parse(jdk_baseline::SHARED_SECRETS);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("<init>", "()V", false,
             "Not registered here; not measured."),
            ("getJavaAWTAccess", "()Ljdk/internal/access/JavaAWTAccess;", false,
             "Not registered here; not measured."),
            ("getJavaAWTFontAccess", "()Ljdk/internal/access/JavaAWTFontAccess;", false,
             "Not registered here; not measured."),
            ("getJavaBeansAccess", "()Ljdk/internal/access/JavaBeansAccess;", false,
             "Not registered here; not measured."),
            ("getJavaIOAccess", "()Ljdk/internal/access/JavaIOAccess;", true, ""),
            ("getJavaIOFileDescriptorAccess", "()Ljdk/internal/access/JavaIOFileDescriptorAccess;", true, ""),
            ("getJavaIORandomAccessFileAccess", "()Ljdk/internal/access/JavaIORandomAccessFileAccess;", true, ""),
            ("getJavaLangAccess", "()Ljdk/internal/access/JavaLangAccess;", true, ""),
            ("getJavaLangInvokeAccess", "()Ljdk/internal/access/JavaLangInvokeAccess;", true, ""),
            ("getJavaLangModuleAccess", "()Ljdk/internal/access/JavaLangModuleAccess;", false,
             "Not registered here; not measured."),
            ("getJavaLangRefAccess", "()Ljdk/internal/access/JavaLangRefAccess;", true, ""),
            ("getJavaLangReflectAccess", "()Ljdk/internal/access/JavaLangReflectAccess;", true, ""),
            ("getJavaNetHttpCookieAccess", "()Ljdk/internal/access/JavaNetHttpCookieAccess;", true, ""),
            ("getJavaNetInetAddressAccess", "()Ljdk/internal/access/JavaNetInetAddressAccess;", true, ""),
            ("getJavaNetURLAccess", "()Ljdk/internal/access/JavaNetURLAccess;", false,
             "Not registered here; not measured."),
            ("getJavaNetUriAccess", "()Ljdk/internal/access/JavaNetUriAccess;", true, ""),
            ("getJavaNioAccess", "()Ljdk/internal/access/JavaNioAccess;", true, ""),
            ("getJavaObjectInputFilterAccess", "()Ljdk/internal/access/JavaObjectInputFilterAccess;", false,
             "Not registered here; not measured."),
            ("getJavaObjectInputStreamAccess", "()Ljdk/internal/access/JavaObjectInputStreamAccess;", false,
             "Not registered here; not measured."),
            ("getJavaObjectInputStreamReadString", "()Ljdk/internal/access/JavaObjectInputStreamReadString;", false,
             "Not registered here; not measured."),
            ("getJavaObjectStreamReflectionAccess", "()Ljdk/internal/access/JavaObjectStreamReflectionAccess;", false,
             "Not registered here; not measured."),
            ("getJavaSecurityPropertiesAccess", "()Ljdk/internal/access/JavaSecurityPropertiesAccess;", false,
             "Not registered here; not measured."),
            ("getJavaSecuritySignatureAccess", "()Ljdk/internal/access/JavaSecuritySignatureAccess;", false,
             "Not registered here; not measured."),
            ("getJavaSecuritySpecAccess", "()Ljdk/internal/access/JavaSecuritySpecAccess;", false,
             "Not registered here; not measured."),
            ("getJavaUtilCollectionAccess", "()Ljdk/internal/access/JavaUtilCollectionAccess;", false,
             "Not registered here; not measured."),
            ("getJavaUtilConcurrentFJPAccess", "()Ljdk/internal/access/JavaUtilConcurrentFJPAccess;", false,
             "Not registered here; not measured."),
            ("getJavaUtilConcurrentTLRAccess", "()Ljdk/internal/access/JavaUtilConcurrentTLRAccess;", false,
             "Not registered here; not measured."),
            ("getJavaUtilResourceBundleAccess", "()Ljdk/internal/access/JavaUtilResourceBundleAccess;", true, ""),
            ("getJavaUtilZipFileAccess", "()Ljdk/internal/access/JavaUtilZipFileAccess;", true, ""),
            ("getJavaxCryptoSealedObjectAccess", "()Ljdk/internal/access/JavaxCryptoSealedObjectAccess;", false,
             "Not registered here; not measured."),
            ("getJavaxCryptoSpecAccess", "()Ljdk/internal/access/JavaxCryptoSpecAccess;", false,
             "Not registered here; not measured."),
            ("getJavaxSecurityAccess", "()Ljdk/internal/access/JavaxSecurityAccess;", false,
             "Not registered here; not measured."),
            ("javaUtilJarAccess", "()Ljdk/internal/access/JavaUtilJarAccess;", false,
             "Not registered here; not measured."),
            ("setJavaAWTAccess", "(Ljdk/internal/access/JavaAWTAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaAWTFontAccess", "(Ljdk/internal/access/JavaAWTFontAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaBeansAccess", "(Ljdk/internal/access/JavaBeansAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaIOAccess", "(Ljdk/internal/access/JavaIOAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaIOFileDescriptorAccess", "(Ljdk/internal/access/JavaIOFileDescriptorAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaIORandomAccessFileAccess", "(Ljdk/internal/access/JavaIORandomAccessFileAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaLangAccess", "(Ljdk/internal/access/JavaLangAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaLangInvokeAccess", "(Ljdk/internal/access/JavaLangInvokeAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaLangModuleAccess", "(Ljdk/internal/access/JavaLangModuleAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaLangRefAccess", "(Ljdk/internal/access/JavaLangRefAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaLangReflectAccess", "(Ljdk/internal/access/JavaLangReflectAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaNetHttpCookieAccess", "(Ljdk/internal/access/JavaNetHttpCookieAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaNetInetAddressAccess", "(Ljdk/internal/access/JavaNetInetAddressAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaNetURLAccess", "(Ljdk/internal/access/JavaNetURLAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaNetUriAccess", "(Ljdk/internal/access/JavaNetUriAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaNioAccess", "(Ljdk/internal/access/JavaNioAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaObjectInputFilterAccess", "(Ljdk/internal/access/JavaObjectInputFilterAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaObjectInputStreamAccess", "(Ljdk/internal/access/JavaObjectInputStreamAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaObjectInputStreamReadString", "(Ljdk/internal/access/JavaObjectInputStreamReadString;)V", false,
             "Not registered here; not measured."),
            ("setJavaObjectStreamReflectionAccess", "(Ljdk/internal/access/JavaObjectStreamReflectionAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaSecurityPropertiesAccess", "(Ljdk/internal/access/JavaSecurityPropertiesAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaSecuritySignatureAccess", "(Ljdk/internal/access/JavaSecuritySignatureAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaSecuritySpecAccess", "(Ljdk/internal/access/JavaSecuritySpecAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaUtilCollectionAccess", "(Ljdk/internal/access/JavaUtilCollectionAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaUtilConcurrentFJPAccess", "(Ljdk/internal/access/JavaUtilConcurrentFJPAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaUtilConcurrentTLRAccess", "(Ljdk/internal/access/JavaUtilConcurrentTLRAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaUtilJarAccess", "(Ljdk/internal/access/JavaUtilJarAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaUtilResourceBundleAccess", "(Ljdk/internal/access/JavaUtilResourceBundleAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaUtilZipFileAccess", "(Ljdk/internal/access/JavaUtilZipFileAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaxCryptoSealedObjectAccess", "(Ljdk/internal/access/JavaxCryptoSealedObjectAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaxCryptoSpecAccess", "(Ljdk/internal/access/JavaxCryptoSpecAccess;)V", false,
             "Not registered here; not measured."),
            ("setJavaxSecurityAccess", "(Ljdk/internal/access/JavaxSecurityAccess;)V", false,
             "Not registered here; not measured."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "SharedSecrets's registered surface disagrees with \
             scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "SharedSecrets: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **RED**, with two `STALE row` lines — §3.2. That is the ratchet working.**

**What makes it red:** It is red today for `getJavaSecurityAccess` and `getJavaUtilJarAccess`; after those are fixed, adding a sixteenth `FACTORIES` row for a method `SharedSecrets` does not declare goes red on the next run.

### NOM E41-2 — row 3 — `jdk.internal.misc.CDS`, and five registrations no call can reach

**File:** `native-builtins/src/cds.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/cds.rs:1320-1396` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

E37 §7.1 called row 3 "half convertible". This is that half. The other half cannot be converted at all and §3.1 says why: `sun.management.CDSMetrics` is not in the JDK 25 image, so its six registrations plus `ManagementFactoryHelper.getCDSMetrics()` have no oracle and never will. `sun.management.ManagementFactoryHelper` was baselined today instead, and it declares no `getCDSMetrics`. NOM E41-16 adds that check.

The doc line NOM E25-6 nominated (*"Verify every registered native can be located"* → *"Verify a hand-maintained list…"*) is superseded: after this rewrite the first wording is true for `jdk/internal/misc/CDS` and the guard no longer needs the apology.

OLD:

```rust
    #[test]
    fn test_all_registered_methods_findable() {
        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);

        let expected: &[(&str, &str, &str)] = &[
            ("sun/management/ManagementFactoryHelper", "<init>", "()V"),
            (
                "sun/management/ManagementFactoryHelper",
                "getCDSMetrics",
                "()Lsun/management/CDSMetrics;",
            ),
            ("sun/management/CDSMetrics", "<init>", "()V"),
            (
                "sun/management/CDSMetrics",
                "getTotalClassesInArchive",
                "()I",
            ),
            (
                "sun/management/CDSMetrics",
                "getClassesLoadedFromArchive",
                "()I",
            ),
            ("sun/management/CDSMetrics", "getArchiveSizeBytes", "()J"),
            ("sun/management/CDSMetrics", "getArchiveLoadTimeMs", "()J"),
            (
                "sun/management/CDSMetrics",
                "getArchivePath",
                "()Ljava/lang/String;",
            ),
            (
                "java/lang/ClassLoader",
                "getCdsArchivePath",
                "()Ljava/lang/String;",
            ),
            ("sun/misc/VM", "<init>", "()V"),
            ("sun/misc/VM", "isBooted", "()Z"),
            ("sun/misc/VM", "savedProps", "()Ljava/util/Properties;"),
            ("jdk/internal/misc/CDS", "<init>", "()V"),
            ("jdk/internal/misc/CDS", "isDumpingClassList", "()Z"),
            ("jdk/internal/misc/CDS", "isDumpingArchive", "()Z"),
            ("jdk/internal/misc/CDS", "isSharingEnabled", "()Z"),
            (
                "jdk/internal/misc/CDS",
                "initializeFromArchive",
                "(Ljava/lang/Class;)V",
            ),
            ("jdk/internal/misc/CDS", "getRandomSeedForDumping", "()J"),
            (
                "jdk/internal/misc/CDS",
                "logLambdaFormInvoker",
                "(Ljava/lang/String;)V",
            ),
            (
                "jdk/internal/misc/CDS",
                "defineArchivedModules",
                "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
            ),
            (
                "jdk/internal/misc/CDS",
                "dumpClassList",
                "(Ljava/lang/String;)V",
            ),
            (
                "jdk/internal/misc/CDS",
                "dumpDynamicArchive",
                "(Ljava/lang/String;)V",
            ),
        ];

        for (cls, name, desc) in expected {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing registration for {cls}.{name}{desc}"
            );
        }
    }
```

NEW:

```rust
    /// The population is **the JDK's**, for the one class in this registrar
    /// that JDK 25 actually has.
    ///
    /// `scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv`: 12 public methods plus
    /// a public no-arg constructor = **13**. `register_cds_natives` makes ten
    /// registrations on this class, and the body this replaced listed all ten and
    /// found all ten — because it transcribed them from the registrar. Its doc
    /// said "Verify every registered native can be located in the registry", which
    /// was false in the direction that mattered: it could not report that five of
    /// the ten name something JDK 25 does not declare.
    ///
    /// The five are in `OFF_SURFACE` below **with no reason, deliberately** — they
    /// are not there. A `reason` on a fabricated name is standing permission, so
    /// this test is RED until each registration is either respelled or deleted:
    ///
    /// * `isDumpingClassList()Z` → the JDK has `isDumpingStaticArchive()Z`.
    /// * `isSharingEnabled()Z` → the JDK has `isUsingArchive()Z`.
    /// * `logLambdaFormInvoker(Ljava/lang/String;)V` → the JDK declares that NAME
    ///   with FOUR `String` parameters. A name-keyed search reports agreement.
    /// * `dumpClassList(Ljava/lang/String;)V`, `dumpDynamicArchive(…)V` → neither
    ///   is a member at any access level.
    ///
    /// **Scope, stated:** `register_cds_natives` also registers on
    /// `sun/management/CDSMetrics` (six), `sun/management/ManagementFactoryHelper`
    /// (two), `sun/misc/VM` (three) and `java/lang/ClassLoader` (one). Those are
    /// not audited here. `sun.management.CDSMetrics` is **not in the JDK 25
    /// runtime image** — a `jrt:/` walk finds no such class — so it has no
    /// baseline and cannot have one; see E41 §3.1 and `classes.txt`.
    #[test]
    fn jdk_surface_cds_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        let cls = "jdk/internal/misc/CDS";
        let baseline = jdk_baseline::parse(jdk_baseline::CDS);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("<init>", "()V", true, ""),
            ("defineArchivedModules", "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V", true, ""),
            ("getRandomSeedForDumping", "()J", true, ""),
            ("initializeFromArchive", "(Ljava/lang/Class;)V", true, ""),
            ("isDumpingArchive", "()Z", true, ""),
            ("isDumpingStaticArchive", "()Z", false,
             "Not registered here; not measured."),
            ("isLoggingLambdaFormInvokers", "()Z", false,
             "Not registered here; not measured."),
            ("isSingleThreadVM", "()Z", false,
             "Not registered here; not measured."),
            ("isUsingArchive", "()Z", false,
             "Not registered here; not measured."),
            ("keepAlive", "(Ljava/lang/Object;)V", false,
             "Not registered here; not measured."),
            ("logLambdaFormInvoker", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V", false,
             "Not registered here; not measured."),
            ("logSpeciesType", "(Ljava/lang/String;Ljava/lang/String;)V", false,
             "Not registered here; not measured."),
            ("needsClassInitBarrier", "(Ljava/lang/Class;)Z", false,
             "Not registered here; not measured."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "CDS's registered surface disagrees with \
             scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "CDS: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **RED**, with five `OFF-SURFACE` lines, one of them the sharper `(descriptor)` form — §3.3.**

**What makes it red:** It is red today for the five names JDK 25 does not have; afterwards, deleting `isDumpingArchive()Z` from the registrar goes `DROPPED`, and renaming `isDumpingClassList` to the real `isDumpingStaticArchive` without deleting its record goes `DEAD OFF-SURFACE row`.

### NOM E41-3 — row 8 — `ManagementFactory`, 7 of **16**, and `loadNativeLib`

**File:** `native-builtins/src/jmx.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/jmx.rs:7600-7638` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

The old body's comment says *"this ensures all seven factory methods exist"* and the word doing the work is *all*. The class has 16 public methods. The most valuable row this conversion adds is `getPlatformMBeanServer`, which the old comment goes out of its way to explain is deliberately absent — and then asserts nothing about, so nothing would notice if somebody registered it and broke interface dispatch on the returned receiver, which is the failure the comment describes.

OLD:

```rust
    #[test]
    fn test_management_factory_returns_all_mxbeans() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/ManagementFactory";
        // `getPlatformMBeanServer` is intentionally NOT in this list — the
        // real JDK bytecode supplies a concrete `JmxMBeanServer`, and a
        // synthetic-stub native here would break interface dispatch on the
        // returned receiver (see KAFKA-MBEAN note above).
        let factory_methods = [
            "getRuntimeMXBean",
            "getMemoryMXBean",
            "getThreadMXBean",
            "getClassLoadingMXBean",
            "getOperatingSystemMXBean",
            "getCompilationMXBean",
            "getGarbageCollectorMXBeans",
        ];
        for method in &factory_methods {
            // Just check the method name is registered (any descriptor)
            // We already checked specific descriptors above; this ensures
            // all seven factory methods exist.
            let found = r.find(
                cls,
                method,
                match *method {
                    "getRuntimeMXBean" => "()Ljava/lang/management/RuntimeMXBean;",
                    "getMemoryMXBean" => "()Ljava/lang/management/MemoryMXBean;",
                    "getThreadMXBean" => "()Ljava/lang/management/ThreadMXBean;",
                    "getClassLoadingMXBean" => "()Ljava/lang/management/ClassLoadingMXBean;",
                    "getOperatingSystemMXBean" => "()Ljava/lang/management/OperatingSystemMXBean;",
                    "getCompilationMXBean" => "()Ljava/lang/management/CompilationMXBean;",
                    "getGarbageCollectorMXBeans" => "()Ljava/util/List;",
                    _ => unreachable!(),
                },
            );
            assert!(found.is_some(), "Missing factory method: {}", method);
        }
    }
```

NEW:

```rust
    /// The population is **the JDK's**, not this registrar's.
    ///
    /// `scripts/baselines/jdk25-java.lang.management.ManagementFactory.tsv`: **16**
    /// public methods and no public constructor. The body this replaced named
    /// seven and its comment said "this ensures all seven factory methods exist".
    ///
    /// The row worth reading first is `getPlatformMBeanServer`. The old comment
    /// explained at length why it is deliberately NOT registered — the real JDK
    /// bytecode supplies a concrete `JmxMBeanServer` and a synthetic stub here
    /// would break interface dispatch on the returned receiver — and then asserted
    /// nothing, so the explanation could not have stopped anyone. It is now a
    /// `false` row carrying that reason, and registering it goes `CLOSED`.
    ///
    /// `loadNativeLib()V` is in `OFF_SURFACE` below **with no reason**: JDK 25
    /// declares no such method on this class at any access level, so no bytecode
    /// can dispatch to it. This test is RED until that registration is deleted or
    /// somebody names the caller that needs it.
    #[test]
    fn jdk_surface_managementfactory_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/ManagementFactory";
        let baseline = jdk_baseline::parse(jdk_baseline::MANAGEMENT_FACTORY);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("getClassLoadingMXBean", "()Ljava/lang/management/ClassLoadingMXBean;", true, ""),
            ("getCompilationMXBean", "()Ljava/lang/management/CompilationMXBean;", true, ""),
            ("getGarbageCollectorMXBeans", "()Ljava/util/List;", true, ""),
            ("getMemoryMXBean", "()Ljava/lang/management/MemoryMXBean;", true, ""),
            ("getMemoryManagerMXBeans", "()Ljava/util/List;", false,
             "Not registered here; not measured."),
            ("getMemoryPoolMXBeans", "()Ljava/util/List;", false,
             "Not registered here; not measured."),
            ("getOperatingSystemMXBean", "()Ljava/lang/management/OperatingSystemMXBean;", true, ""),
            ("getPlatformMBeanServer", "()Ljavax/management/MBeanServer;", false,
             "Not registered here; not measured."),
            ("getPlatformMXBean", "(Ljava/lang/Class;)Ljava/lang/management/PlatformManagedObject;", true, ""),
            ("getPlatformMXBean", "(Ljavax/management/MBeanServerConnection;Ljava/lang/Class;)Ljava/lang/management/PlatformManagedObject;", false,
             "Not registered here; not measured."),
            ("getPlatformMXBeans", "(Ljava/lang/Class;)Ljava/util/List;", true, ""),
            ("getPlatformMXBeans", "(Ljavax/management/MBeanServerConnection;Ljava/lang/Class;)Ljava/util/List;", false,
             "Not registered here; not measured."),
            ("getPlatformManagementInterfaces", "()Ljava/util/Set;", false,
             "Not registered here; not measured."),
            ("getRuntimeMXBean", "()Ljava/lang/management/RuntimeMXBean;", true, ""),
            ("getThreadMXBean", "()Ljava/lang/management/ThreadMXBean;", true, ""),
            ("newPlatformMXBeanProxy", "(Ljavax/management/MBeanServerConnection;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;", false,
             "Not registered here; not measured."),
        ];
        #[rustfmt::skip]
        const OFF_SURFACE: &[jdk_baseline::OffSurface] = &[
            ("<init>", "()V",
             "SYNTHETIC <init>. ManagementFactory's constructor is private, \
              so `()V` is not on the public surface; no bytecode can reach \
              this registration."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "ManagementFactory's registered surface disagrees with \
             scripts/baselines/jdk25-java.lang.management.ManagementFactory.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, OFF_SURFACE);
        assert!(
            off.is_empty(),
            "ManagementFactory: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **RED**, with one `OFF-SURFACE` line for `loadNativeLib()V`.**

**What makes it red:** Implementing `getPlatformMBeanServer()` and leaving its `false` row goes `CLOSED` — the deliberate absence the old comment explained in prose and asserted nowhere. It is red today for `loadNativeLib()V`.

### NOM E41-4 — row 22 — `ReflectionFactory`, 4 of **25 + 14**, and `getConstantPool`

**File:** `native-builtins/src/lib.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/lib.rs:4956-4992` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

The guard's own doc says *"Those 18 methods"*; the class has 25 public members and its `jdk.unsupported` twin has 14 more. **The two are not one surface** — `newOptionalDataExceptionForSerialization` is `()Ljava/lang/reflect/Constructor;` on one and `(Z)Ljava/io/OptionalDataException;` on the other — so a guard reading one baseline for both would report a false `STALE`. `sun.reflect.ReflectionFactory` was baselined today; NOM E41-5 is its half, and the essential path registers **nothing** on it, which is exactly what the guard claims and had only ever checked for two triples.

OLD:

```rust
    #[test]
    fn essential_path_does_not_override_reflection_factory_serialization() {
        let mut registry = NativeMethodRegistry::new();
        register_essential_natives(&mut registry);

        for (class, method, descriptor) in [
            (
                "sun/reflect/ReflectionFactory",
                "getReflectionFactory",
                "()Lsun/reflect/ReflectionFactory;",
            ),
            (
                "sun/reflect/ReflectionFactory",
                "readObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            ),
            (
                "jdk/internal/reflect/ReflectionFactory",
                "newConstructorForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            ),
            (
                "jdk/internal/reflect/ReflectionFactory",
                "hasStaticInitializerForSerialization",
                "(Ljava/lang/Class;)Z",
            ),
        ] {
            assert!(
                registry.find(class, method, descriptor).is_none(),
                "{class}.{method}{descriptor} is registered on the real-JDK \
                 essential path; that path must run the JDK's own bytecode for \
                 the ReflectionFactory serialization surface. If this override \
                 is genuinely needed, it must be added ungated — see the \
                 comment in register_essential_natives_with_shims.",
            );
        }
    }
```

NEW:

```rust
    /// The population is **the JDK's**, and the claim is a NEGATIVE one.
    ///
    /// The real-JDK essential path must not override the `ReflectionFactory`
    /// serialization surface: those methods are ordinary `java.base` /
    /// `jdk.unsupported` *bytecode*, and a differential probe over everything they
    /// cover is byte-identical to HotSpot JDK 25 with the overrides absent.
    ///
    /// The body this replaced named **four** triples, two on each of the two
    /// classes, and its own doc said "Those 18 methods".
    /// `scripts/baselines/jdk25-jdk.internal.reflect.ReflectionFactory.tsv` says
    /// **25**, and `jdk25-sun.reflect.ReflectionFactory.tsv` says another **14**.
    /// The other 35 could be overridden silently.
    ///
    /// Five ARE registered here and are recorded `true`: `copyField`, `copyMethod`,
    /// `copyConstructor` (identity forwarders) and
    /// `getExecutableSharedParameterTypes` / `getExecutableTypeAnnotationBytes`,
    /// whose registrations carry long comments about the JBoss Modules and
    /// ByteBuddy bootstrap paths that need them. None is part of the serialization
    /// surface, which is why the old four-triple body and this one agree on the
    /// verdict and disagree on everything else.
    ///
    /// `getConstantPool` is in `OFF_SURFACE` below **with no reason**: neither
    /// `ReflectionFactory` declares it on JDK 25. It is declared by
    /// `jdk.internal.access.JavaLangAccess` and reached through
    /// `SharedSecrets.getJavaLangAccess()`. Its registration's comment argues at
    /// length about what it should RETURN; nothing had ever checked that anything
    /// can call it. RED until it is moved or deleted.
    ///
    /// **Scope, stated:** this is `register_essential_natives`, i.e.
    /// `register_essential_natives_with_shims(…, ShimSelection::ALL)`. A
    /// registration behind a narrower shim selection is out of this fixture's
    /// reach and would read as absent.
    #[test]
    fn jdk_surface_reflectionfactory_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_essential_natives(&mut r);
        let cls = "jdk/internal/reflect/ReflectionFactory";
        let baseline = jdk_baseline::parse(jdk_baseline::REFLECTION_FACTORY);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("copyConstructor", "(Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;", true, ""),
            ("copyField", "(Ljava/lang/reflect/Field;)Ljava/lang/reflect/Field;", true, ""),
            ("copyMethod", "(Ljava/lang/reflect/Method;)Ljava/lang/reflect/Method;", true, ""),
            ("defaultReadObjectForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("defaultWriteObjectForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("getExecutableSharedParameterTypes", "(Ljava/lang/reflect/Executable;)[Ljava/lang/Class;", true, ""),
            ("getExecutableTypeAnnotationBytes", "(Ljava/lang/reflect/Executable;)[B", true, ""),
            ("getReflectionFactory", "()Ljdk/internal/reflect/ReflectionFactory;", false,
             "Not registered here; not measured."),
            ("hasStaticInitializerForSerialization", "(Ljava/lang/Class;)Z", false,
             "Not registered here; not measured."),
            ("leafCopyMethod", "(Ljava/lang/reflect/Method;)Ljava/lang/reflect/Method;", false,
             "Not registered here; not measured."),
            ("newConstructorAccessor", "(Ljava/lang/reflect/Constructor;)Ljdk/internal/reflect/ConstructorAccessor;", false,
             "Not registered here; not measured."),
            ("newConstructorForExternalization", "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;", false,
             "Not registered here; not measured."),
            ("newConstructorForSerialization", "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;", false,
             "Not registered here; not measured."),
            ("newConstructorForSerialization", "(Ljava/lang/Class;Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;", false,
             "Not registered here; not measured."),
            ("newFieldAccessor", "(Ljava/lang/reflect/Field;Z)Ljdk/internal/reflect/FieldAccessor;", false,
             "Not registered here; not measured."),
            ("newInstance", "(Ljava/lang/reflect/Constructor;[Ljava/lang/Object;Ljava/lang/Class;)Ljava/lang/Object;", false,
             "Not registered here; not measured."),
            ("newMethodAccessor", "(Ljava/lang/reflect/Method;Z)Ljdk/internal/reflect/MethodAccessor;", false,
             "Not registered here; not measured."),
            ("newOptionalDataExceptionForSerialization", "()Ljava/lang/reflect/Constructor;", false,
             "Not registered here; not measured."),
            ("parseAccessFlags", "(ILjava/lang/reflect/AccessFlag$Location;Ljava/lang/Class;)Ljava/util/Set;", false,
             "Not registered here; not measured."),
            ("readObjectForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("readObjectNoDataForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("readResolveForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("serialPersistentFields", "(Ljava/lang/Class;)[Ljava/io/ObjectStreamField;", false,
             "Not registered here; not measured."),
            ("writeObjectForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("writeReplaceForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "ReflectionFactory's registered surface disagrees with \
             scripts/baselines/jdk25-jdk.internal.reflect.ReflectionFactory.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "ReflectionFactory: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **RED**, with one `OFF-SURFACE` line for `getConstantPool`.**

**What makes it red:** Registering any of the 39 `ReflectionFactory` members on the essential path — not the four the old body happened to name. It is red today for `getConstantPool(Ljava/lang/Class;)Ljdk/internal/reflect/ConstantPool;`.

### NOM E41-5 — row 22, second class — `sun.reflect.ReflectionFactory`, 0 of **14**

**File:** `native-builtins/src/lib.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/lib.rs:4956-4992` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

This is the same nomination as E41-4, for the other class, and it is listed separately because it is a **second `#[test]`**: one fixture, two baselines, two `internal_name()` pins. Merging them would need a class parameter on `audit` that `jdk_baseline` deliberately does not have — the caller owns the class constant, so that the pin is a real assertion and not a lookup.

Apply E41-4 and E41-5 together; the OLD text is the same block and is replaced once by both bodies.

OLD:

```rust
    #[test]
    fn essential_path_does_not_override_reflection_factory_serialization() {
        let mut registry = NativeMethodRegistry::new();
        register_essential_natives(&mut registry);

        for (class, method, descriptor) in [
            (
                "sun/reflect/ReflectionFactory",
                "getReflectionFactory",
                "()Lsun/reflect/ReflectionFactory;",
            ),
            (
                "sun/reflect/ReflectionFactory",
                "readObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            ),
            (
                "jdk/internal/reflect/ReflectionFactory",
                "newConstructorForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            ),
            (
                "jdk/internal/reflect/ReflectionFactory",
                "hasStaticInitializerForSerialization",
                "(Ljava/lang/Class;)Z",
            ),
        ] {
            assert!(
                registry.find(class, method, descriptor).is_none(),
                "{class}.{method}{descriptor} is registered on the real-JDK \
                 essential path; that path must run the JDK's own bytecode for \
                 the ReflectionFactory serialization surface. If this override \
                 is genuinely needed, it must be added ungated — see the \
                 comment in register_essential_natives_with_shims.",
            );
        }
    }
```

NEW:

```rust
    /// The `jdk.unsupported` twin of the class above, and a different surface.
    ///
    /// `scripts/baselines/jdk25-sun.reflect.ReflectionFactory.tsv`: **14** public
    /// methods. It is a thin delegate over `jdk.internal.reflect.ReflectionFactory`
    /// and the two disagree on more than membership —
    /// `newOptionalDataExceptionForSerialization` is `(Z)Ljava/io/
    /// OptionalDataException;` here and `()Ljava/lang/reflect/Constructor;` there.
    /// A guard that read one baseline for both would report a false `STALE`.
    ///
    /// Every row is `false`: the essential path registers nothing on this class,
    /// which is what the guard has always claimed and had only ever checked for
    /// `getReflectionFactory` and `readObjectForSerialization`.
    #[test]
    fn jdk_surface_reflectionfactory_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_essential_natives(&mut r);
        let cls = "sun/reflect/ReflectionFactory";
        let baseline = jdk_baseline::parse(jdk_baseline::SUN_REFLECTION_FACTORY);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("defaultReadObjectForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("defaultWriteObjectForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("getReflectionFactory", "()Lsun/reflect/ReflectionFactory;", false,
             "Not registered here; not measured."),
            ("hasStaticInitializerForSerialization", "(Ljava/lang/Class;)Z", false,
             "Not registered here; not measured."),
            ("newConstructorForExternalization", "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;", false,
             "Not registered here; not measured."),
            ("newConstructorForSerialization", "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;", false,
             "Not registered here; not measured."),
            ("newConstructorForSerialization", "(Ljava/lang/Class;Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;", false,
             "Not registered here; not measured."),
            ("newOptionalDataExceptionForSerialization", "(Z)Ljava/io/OptionalDataException;", false,
             "Not registered here; not measured."),
            ("readObjectForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("readObjectNoDataForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("readResolveForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("serialPersistentFields", "(Ljava/lang/Class;)[Ljava/io/ObjectStreamField;", false,
             "Not registered here; not measured."),
            ("writeObjectForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
            ("writeReplaceForSerialization", "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;", false,
             "Not registered here; not measured."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "ReflectionFactory's registered surface disagrees with \
             scripts/baselines/jdk25-sun.reflect.ReflectionFactory.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "ReflectionFactory: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **GREEN**. All fourteen rows are `false`; the essential path registers nothing on this class.**

**What makes it red:** Registering any of the fourteen — `getReflectionFactory` and `readObjectForSerialization` were the only two the old body named, and the other twelve were free.

### NOM E41-6 — row 16 — `StructuredTaskScope`, the outer class

**File:** `native-builtins/src/jdk25_concurrency.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/jdk25_concurrency.rs:5112-5146` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

**This is the row E37 said to expect red, and it is redder than expected.** E37 predicted the `$Config` half. The outer class is real, is baselined, and is wrong in four more places — see §3.4.

`s52_total_registration_count` covers **fourteen** triples across three classes and this is one of them (`open(Joiner)`). Its OLD text is replaced once by E41-6, E41-7 and E41-8 together, and the `$Config` sixth is discussed under E41-9.

OLD:

```rust
    #[test]
    fn s52_total_registration_count() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        // Verify all new S52 registrations:
        // Joiner: 7 (4 factories + onComplete + result + policy)
        // open(Joiner): 1
        // Config: 6 (init + withName + withThreadFactory + withTimeout + getName + getThreadFactory)
        // Total new = 14
        let s52_methods = [
            (CLS_JOINER, "allSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "anySuccessfulResultOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "awaitAllSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "awaitAll", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "onComplete", "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z"),
            (CLS_JOINER, "result", "()Ljava/lang/Object;"),
            (CLS_JOINER, "policy", "()I"),
            (CLS_TASK_SCOPE, "open", "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;"),
            (CLS_CONFIG, "<init>", "()V"),
            (CLS_CONFIG, "withName", "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "withThreadFactory", "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "withTimeout", "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "getName", "()Ljava/lang/String;"),
            (CLS_CONFIG, "getThreadFactory", "()Ljava/util/concurrent/ThreadFactory;"),
        ];
        for (cls, name, desc) in &s52_methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing S52: {}.{}{}",
                cls,
                name,
                desc
            );
        }
    }
```

NEW:

```rust
    /// The population is **the JDK's**, for the outer JEP 505 type.
    ///
    /// `scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope.tsv`:
    /// **8** public members, and no constructors — JEP 505 made this an interface.
    /// The body this replaced asserted exactly ONE triple on this class
    /// (`open(Joiner)`) out of the twelve it registers.
    ///
    /// Four registrations are in `OFF_SURFACE` below **with no reason**, and this
    /// test is RED until each is respelled or deleted:
    ///
    /// * `isShutdown()Z` — the shipped method is `isCancelled()Z`.
    /// * `shutdown()V` — gone; cancellation moved onto the `Joiner`.
    /// * `joinUntil(Ljava/time/Instant;)…` — gone; a deadline is
    ///   `Configuration.withTimeout(Duration)`.
    /// * `join()` — declared, returning `Ljava/lang/Object;` (the joiner's result),
    ///   not the scope. The preview API returned the scope for chaining, and that
    ///   preview descriptor is what this tree still registers.
    ///
    /// The three synthetic `<init>` registrations and `toString` ARE recorded, with
    /// reasons: an interface declares no constructors and this VM allocates its
    /// synthetic scope by that key, and `toString` is `java.lang.Object`'s.
    #[test]
    fn jdk_surface_structuredtaskscope_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let cls = "java/util/concurrent/StructuredTaskScope";
        let baseline = jdk_baseline::parse(jdk_baseline::STRUCTURED_TASK_SCOPE);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("close", "()V", true, ""),
            ("fork", "(Ljava/lang/Runnable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;", false,
             "Not registered here; not measured."),
            ("fork", "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;", true, ""),
            ("isCancelled", "()Z", false,
             "Not registered here; not measured."),
            ("join", "()Ljava/lang/Object;", false,
             "Not registered here; not measured."),
            ("open", "()Ljava/util/concurrent/StructuredTaskScope;", true, ""),
            ("open", "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;", true, ""),
            ("open", "(Ljava/util/concurrent/StructuredTaskScope$Joiner;Ljava/util/function/Function;)Ljava/util/concurrent/StructuredTaskScope;", false,
             "Not registered here; not measured."),
        ];
        #[rustfmt::skip]
        const OFF_SURFACE: &[jdk_baseline::OffSurface] = &[
            ("<init>", "()V",
             "SYNTHETIC <init> on an INTERFACE. JEP 505 made \
              StructuredTaskScope an interface; it declares no \
              constructors."),
            ("<init>", "(Ljava/lang/String;)V",
             "As `<init>()V`, and additionally a shape the preview API \
              never had."),
            ("<init>", "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
             "As `<init>()V`. The final API configures name and thread \
              factory through `Configuration`."),
            ("toString", "()Ljava/lang/String;",
             "INHERITED from java.lang.Object; StructuredTaskScope does not \
              declare it."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "StructuredTaskScope's registered surface disagrees with \
             scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, OFF_SURFACE);
        assert!(
            off.is_empty(),
            "StructuredTaskScope: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **RED**, with four `OFF-SURFACE` lines, one of them the `(descriptor)` form — §3.4.**

**What makes it red:** It is red today for `isShutdown`, `shutdown`, `joinUntil` and `join`'s return type; afterwards, registering `fork(Ljava/lang/Runnable;)…` and leaving its `false` row goes `CLOSED`.

### NOM E41-7 — row 16 — `StructuredTaskScope$Joiner`, 7 registered, `policy()I` invented

**File:** `native-builtins/src/jdk25_concurrency.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/jdk25_concurrency.rs:5112-5146` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

The old comment does the arithmetic — *"Joiner: 7 (4 factories + onComplete + result + policy)"* — off the registrar. Six of those seven are real. `policy` is this tree's own int-tagged discriminator (`JOINER_POLICY_ALL_SUCCESSFUL` and friends), registered under a JDK class name where nothing in the JDK can call it. The two members the JDK does declare and this VM does not register, `allUntil` and `onFork`, were outside the old population entirely.

OLD:

```rust
    #[test]
    fn s52_total_registration_count() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        // Verify all new S52 registrations:
        // Joiner: 7 (4 factories + onComplete + result + policy)
        // open(Joiner): 1
        // Config: 6 (init + withName + withThreadFactory + withTimeout + getName + getThreadFactory)
        // Total new = 14
        let s52_methods = [
            (CLS_JOINER, "allSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "anySuccessfulResultOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "awaitAllSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "awaitAll", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "onComplete", "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z"),
            (CLS_JOINER, "result", "()Ljava/lang/Object;"),
            (CLS_JOINER, "policy", "()I"),
            (CLS_TASK_SCOPE, "open", "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;"),
            (CLS_CONFIG, "<init>", "()V"),
            (CLS_CONFIG, "withName", "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "withThreadFactory", "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "withTimeout", "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "getName", "()Ljava/lang/String;"),
            (CLS_CONFIG, "getThreadFactory", "()Ljava/util/concurrent/ThreadFactory;"),
        ];
        for (cls, name, desc) in &s52_methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing S52: {}.{}{}",
                cls,
                name,
                desc
            );
        }
    }
```

NEW:

```rust
    /// The population is **the JDK's**, not the "Joiner: 7" comment's.
    ///
    /// `scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Joiner.tsv`:
    /// **8** public members. The body this replaced asserted seven, counted off the
    /// registrar, and could not name the two it was missing — `allUntil` and
    /// `onFork`, both now `false` rows with a reason.
    ///
    /// `policy()I` is in `OFF_SURFACE` below **with no reason**: it is this tree's
    /// own int-tagged joiner discriminator (`JOINER_POLICY_*`), registered under a
    /// JDK class name. Nothing in the JDK declares it and no JDK bytecode can call
    /// it. If a VM-internal discriminator is needed, it belongs on a
    /// `cratonvm/…` class, not on `java.util.concurrent`. RED until then.
    #[test]
    fn jdk_surface_structuredtaskscope_joiner_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let cls = "java/util/concurrent/StructuredTaskScope$Joiner";
        let baseline = jdk_baseline::parse(jdk_baseline::STRUCTURED_TASK_SCOPE_JOINER);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("allSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;", true, ""),
            ("allUntil", "(Ljava/util/function/Predicate;)Ljava/util/concurrent/StructuredTaskScope$Joiner;", false,
             "Not registered here; not measured."),
            ("anySuccessfulResultOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;", true, ""),
            ("awaitAll", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;", true, ""),
            ("awaitAllSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;", true, ""),
            ("onComplete", "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z", true, ""),
            ("onFork", "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z", false,
             "Not registered here; not measured."),
            ("result", "()Ljava/lang/Object;", true, ""),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "StructuredTaskScope$Joiner's registered surface disagrees with \
             scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Joiner.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "StructuredTaskScope$Joiner: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **RED**, with one `OFF-SURFACE` line for `policy()I`.**

**What makes it red:** Implementing `Joiner.allUntil(Ljava/util/function/Predicate;)…` or `onFork` and leaving the `false` row goes `CLOSED`. It is red today for `policy()I`.

### NOM E41-8 — row 16 — `StructuredTaskScope$Subtask`, 3 of 3, plus `task()`

**File:** `native-builtins/src/jdk25_concurrency.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/jdk25_concurrency.rs:5112-5146` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

The good news row of the four: all three JDK members are registered, which `s52_total_registration_count` never asserted at all — `CLS_SUBTASK` is not in its fourteen. The fourth registration is not a member.

OLD:

```rust
    #[test]
    fn s52_total_registration_count() {
        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        // Verify all new S52 registrations:
        // Joiner: 7 (4 factories + onComplete + result + policy)
        // open(Joiner): 1
        // Config: 6 (init + withName + withThreadFactory + withTimeout + getName + getThreadFactory)
        // Total new = 14
        let s52_methods = [
            (CLS_JOINER, "allSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "anySuccessfulResultOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "awaitAllSuccessfulOrThrow", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "awaitAll", "()Ljava/util/concurrent/StructuredTaskScope$Joiner;"),
            (CLS_JOINER, "onComplete", "(Ljava/util/concurrent/StructuredTaskScope$Subtask;)Z"),
            (CLS_JOINER, "result", "()Ljava/lang/Object;"),
            (CLS_JOINER, "policy", "()I"),
            (CLS_TASK_SCOPE, "open", "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;"),
            (CLS_CONFIG, "<init>", "()V"),
            (CLS_CONFIG, "withName", "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "withThreadFactory", "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "withTimeout", "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Config;"),
            (CLS_CONFIG, "getName", "()Ljava/lang/String;"),
            (CLS_CONFIG, "getThreadFactory", "()Ljava/util/concurrent/ThreadFactory;"),
        ];
        for (cls, name, desc) in &s52_methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing S52: {}.{}{}",
                cls,
                name,
                desc
            );
        }
    }
```

NEW:

```rust
    /// The population is **the JDK's**.
    ///
    /// `scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Subtask.tsv`:
    /// **3** public members, all three registered. `s52_total_registration_count`
    /// asserted none of them — `CLS_SUBTASK` is not among its fourteen triples, so
    /// deleting all three was silent.
    ///
    /// `task()Ljava/util/concurrent/Callable;` is in `OFF_SURFACE` below **with no
    /// reason**: `Subtask` declares `get`, `exception` and `state`, and nothing
    /// else. RED until it is deleted.
    #[test]
    fn jdk_surface_structuredtaskscope_subtask_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_jdk25_concurrency_natives(&mut r);
        let cls = "java/util/concurrent/StructuredTaskScope$Subtask";
        let baseline = jdk_baseline::parse(jdk_baseline::STRUCTURED_TASK_SCOPE_SUBTASK);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("exception", "()Ljava/lang/Throwable;", true, ""),
            ("get", "()Ljava/lang/Object;", true, ""),
            ("state", "()Ljava/util/concurrent/StructuredTaskScope$Subtask$State;", true, ""),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "StructuredTaskScope$Subtask's registered surface disagrees with \
             scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Subtask.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "StructuredTaskScope$Subtask: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **RED**, with one `OFF-SURFACE` line for `task()`.**

**What makes it red:** Deleting the `get`, `exception` or `state` registration goes `DROPPED`. It is red today for `task()Ljava/util/concurrent/Callable;`.

### NOM E41-9 — row 7 — `RuntimeMXBean`, 13 of **17**

**File:** `native-builtins/src/jmx.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/jmx.rs:7009-7037` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

E25 row 7 says *"`getPid`, `getManagementSpecVersion`, `getSystemProperties`, `getLibraryPath` are green on absence"*. Measurement corrects that: **three of those four are registered**, and were simply not asserted. Only `getPid` is genuinely absent. That is the difference between reading a guard's list and reading the registrar.

This nomination also demonstrates the second legitimate off-surface shape: `getObjectName` is registered on this class by `register_platform_managed_object_names`, is not declared by `RuntimeMXBean`, and is perfectly reachable — `PlatformManagedObject` declares it. Baselining that interface would move the row onto the surface, and kind 7 (`STALE OFF-SURFACE row`) is what would force the record to follow.

OLD:

```rust
    #[test]
    fn test_runtime_mxbean_all_getters_registered() {
        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/RuntimeMXBean";
        let methods = [
            ("getName", "()Ljava/lang/String;"),
            ("getVmName", "()Ljava/lang/String;"),
            ("getVmVersion", "()Ljava/lang/String;"),
            ("getVmVendor", "()Ljava/lang/String;"),
            ("getSpecName", "()Ljava/lang/String;"),
            ("getSpecVersion", "()Ljava/lang/String;"),
            ("getSpecVendor", "()Ljava/lang/String;"),
            ("getStartTime", "()J"),
            ("getUptime", "()J"),
            ("getInputArguments", "()Ljava/util/List;"),
            ("getClassPath", "()Ljava/lang/String;"),
            ("getBootClassPath", "()Ljava/lang/String;"),
            ("isBootClassPathSupported", "()Z"),
        ];
        for (name, desc) in &methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing RuntimeMXBean.{}{}",
                name,
                desc
            );
        }
    }
```

NEW:

```rust
    /// The population is **the JDK's**, not this registrar's.
    ///
    /// `scripts/baselines/jdk25-java.lang.management.RuntimeMXBean.tsv`: **17**
    /// public methods, no constructors (it is an interface). The body this replaced
    /// named 13 and was called `all_getters`.
    ///
    /// Three of the four E25 row 7 listed as uncovered — `getManagementSpecVersion`,
    /// `getLibraryPath`, `getSystemProperties` — turn out to be registered and
    /// merely unasserted. `getPid()J` is the one genuine absence and is the single
    /// `false` row below.
    ///
    /// `OFF_SURFACE` carries the two registrations this class does not declare, and
    /// they are the two LEGITIMATE shapes, not defects:
    ///
    /// * `<init>()V` on an interface — how the 10-field synthetic bean is
    ///   allocated.
    /// * `getObjectName()Ljavax/management/ObjectName;` — declared by
    ///   `PlatformManagedObject`, which `javap -public` on this leaf never lists.
    ///   Baseline `java.lang.management.PlatformManagedObject` and this row becomes
    ///   a `STALE OFF-SURFACE row` telling you to move it into TRIAGE.
    #[test]
    fn jdk_surface_runtimemxbean_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_jmx_natives(&mut r);
        let cls = "java/lang/management/RuntimeMXBean";
        let baseline = jdk_baseline::parse(jdk_baseline::RUNTIME_MXBEAN);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("getBootClassPath", "()Ljava/lang/String;", true, ""),
            ("getClassPath", "()Ljava/lang/String;", true, ""),
            ("getInputArguments", "()Ljava/util/List;", true, ""),
            ("getLibraryPath", "()Ljava/lang/String;", true, ""),
            ("getManagementSpecVersion", "()Ljava/lang/String;", true, ""),
            ("getName", "()Ljava/lang/String;", true, ""),
            ("getPid", "()J", false,
             "Not registered here; not measured."),
            ("getSpecName", "()Ljava/lang/String;", true, ""),
            ("getSpecVendor", "()Ljava/lang/String;", true, ""),
            ("getSpecVersion", "()Ljava/lang/String;", true, ""),
            ("getStartTime", "()J", true, ""),
            ("getSystemProperties", "()Ljava/util/Map;", true, ""),
            ("getUptime", "()J", true, ""),
            ("getVmName", "()Ljava/lang/String;", true, ""),
            ("getVmVendor", "()Ljava/lang/String;", true, ""),
            ("getVmVersion", "()Ljava/lang/String;", true, ""),
            ("isBootClassPathSupported", "()Z", true, ""),
        ];
        #[rustfmt::skip]
        const OFF_SURFACE: &[jdk_baseline::OffSurface] = &[
            ("<init>", "()V",
             "SYNTHETIC <init> on an INTERFACE. RuntimeMXBean declares no \
              constructors at all; this is how the 10-field synthetic bean \
              is allocated."),
            ("getObjectName", "()Ljavax/management/ObjectName;",
             "INHERITED, not absent. `PlatformManagedObject` declares it \
              and `javap -public` on the leaf never lists an inherited \
              member. Baseline java.lang.management.PlatformManagedObject \
              to move this row onto the surface."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "RuntimeMXBean's registered surface disagrees with \
             scripts/baselines/jdk25-java.lang.management.RuntimeMXBean.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, OFF_SURFACE);
        assert!(
            off.is_empty(),
            "RuntimeMXBean: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **GREEN**, with 16 of the 17 `true` and `getPid()J` the only `false`.**

**What makes it red:** Deleting `register_runtime_mxbean`'s `getSystemProperties()Ljava/util/Map;` registration, which the old thirteen-row list never named; or implementing `getPid()J` and leaving its `false` row.

### NOM E41-10 — row 2 — `SSLParameters`, 13 of **31**

**File:** `native-builtins/src/tls.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/tls.rs:4797-4828` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

The old body's comment says *"all 12"*, its message says *"all 13"*, and its array has 13 rows — a disagreement sitting in the file, which NOM E25-5 proposed to fix with a comment. This replaces the mechanism instead. The `assert_eq!(count, 13)` compared a filtered 13-row array against the literal 13, so it could only fail when one of those exact 13 registrations was deleted.

**Scope, stated:** this fixture is `register_tls_natives` alone. `t27_tls.rs::register_alpn_on_parameters` registers `setApplicationProtocols` on this class too and wins in a full build by registration order; both copies are `true` here, so the verdict is unchanged, but a method that ONLY t27 registers would read as absent.

OLD:

```rust
    #[test]
    fn test_ssl_parameters_registration_complete() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLParameters";
        // Verify we can look up all 12 SSLParameters methods
        let count = [
            ("<init>", "()V"),
            ("getProtocols", "()[Ljava/lang/String;"),
            ("setProtocols", "([Ljava/lang/String;)V"),
            ("getCipherSuites", "()[Ljava/lang/String;"),
            ("setCipherSuites", "([Ljava/lang/String;)V"),
            ("getApplicationProtocols", "()[Ljava/lang/String;"),
            ("setApplicationProtocols", "([Ljava/lang/String;)V"),
            ("getEndpointIdentificationAlgorithm", "()Ljava/lang/String;"),
            (
                "setEndpointIdentificationAlgorithm",
                "(Ljava/lang/String;)V",
            ),
            ("getNeedClientAuth", "()Z"),
            ("setNeedClientAuth", "(Z)V"),
            ("getWantClientAuth", "()Z"),
            ("setWantClientAuth", "(Z)V"),
        ]
        .iter()
        .filter(|(name, desc)| r.find(cls, name, desc).is_some())
        .count();
        assert_eq!(
            count, 13,
            "Expected all 13 SSLParameters methods registered"
        );
    }
```

NEW:

```rust
    /// The population is **the JDK's**, not this registrar's.
    ///
    /// `scripts/baselines/jdk25-javax.net.ssl.SSLParameters.tsv`, generated from
    /// `jrt:/modules/java.base/javax/net/ssl/SSLParameters.class` on openjdk
    /// 25.0.3+9: **28 public methods plus 3 public constructors = 31** members.
    /// `register_tls_natives` makes **13** of them.
    ///
    /// The body this replaced filtered a 13-row array and asserted the count was
    /// 13 — the expected number WAS the array length — under a comment saying
    /// "all 12" and a message saying "all 13". The 18 members it never mentioned
    /// (`getServerNames`, `setSNIMatchers`, `getAlgorithmConstraints`,
    /// `setUseCipherSuitesOrder`, the two extra constructors, …) are now `false`
    /// rows: still absent, no longer silent.
    ///
    /// **Scope, stated:** `t27_tls.rs::register_alpn_on_parameters` registers
    /// `setApplicationProtocols` on this class as well and wins at runtime by
    /// registration order. It is already `true` here, so the verdict does not
    /// change — but a method only t27 registers would read as absent in this
    /// fixture.
    #[test]
    fn jdk_surface_sslparameters_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "javax/net/ssl/SSLParameters";
        let baseline = jdk_baseline::parse(jdk_baseline::SSL_PARAMETERS);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("<init>", "()V", true, ""),
            ("<init>", "([Ljava/lang/String;)V", false,
             "Not registered here; not measured."),
            ("<init>", "([Ljava/lang/String;[Ljava/lang/String;)V", false,
             "Not registered here; not measured."),
            ("getAlgorithmConstraints", "()Ljava/security/AlgorithmConstraints;", false,
             "Not registered here; not measured."),
            ("getApplicationProtocols", "()[Ljava/lang/String;", true, ""),
            ("getCipherSuites", "()[Ljava/lang/String;", true, ""),
            ("getEnableRetransmissions", "()Z", false,
             "Not registered here; not measured."),
            ("getEndpointIdentificationAlgorithm", "()Ljava/lang/String;", true, ""),
            ("getMaximumPacketSize", "()I", false,
             "Not registered here; not measured."),
            ("getNamedGroups", "()[Ljava/lang/String;", false,
             "Not registered here; not measured."),
            ("getNeedClientAuth", "()Z", true, ""),
            ("getProtocols", "()[Ljava/lang/String;", true, ""),
            ("getSNIMatchers", "()Ljava/util/Collection;", false,
             "Not registered here; not measured."),
            ("getServerNames", "()Ljava/util/List;", false,
             "Not registered here; not measured."),
            ("getSignatureSchemes", "()[Ljava/lang/String;", false,
             "Not registered here; not measured."),
            ("getUseCipherSuitesOrder", "()Z", false,
             "Not registered here; not measured."),
            ("getWantClientAuth", "()Z", true, ""),
            ("setAlgorithmConstraints", "(Ljava/security/AlgorithmConstraints;)V", false,
             "Not registered here; not measured."),
            ("setApplicationProtocols", "([Ljava/lang/String;)V", true, ""),
            ("setCipherSuites", "([Ljava/lang/String;)V", true, ""),
            ("setEnableRetransmissions", "(Z)V", false,
             "Not registered here; not measured."),
            ("setEndpointIdentificationAlgorithm", "(Ljava/lang/String;)V", true, ""),
            ("setMaximumPacketSize", "(I)V", false,
             "Not registered here; not measured."),
            ("setNamedGroups", "([Ljava/lang/String;)V", false,
             "Not registered here; not measured."),
            ("setNeedClientAuth", "(Z)V", true, ""),
            ("setProtocols", "([Ljava/lang/String;)V", true, ""),
            ("setSNIMatchers", "(Ljava/util/Collection;)V", false,
             "Not registered here; not measured."),
            ("setServerNames", "(Ljava/util/List;)V", false,
             "Not registered here; not measured."),
            ("setSignatureSchemes", "([Ljava/lang/String;)V", false,
             "Not registered here; not measured."),
            ("setUseCipherSuitesOrder", "(Z)V", false,
             "Not registered here; not measured."),
            ("setWantClientAuth", "(Z)V", true, ""),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "SSLParameters's registered surface disagrees with \
             scripts/baselines/jdk25-javax.net.ssl.SSLParameters.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "SSLParameters: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **GREEN**. 13 `true`, 18 `false`, 0 off-surface.**

**What makes it red:** Registering `setServerNames(Ljava/util/List;)V` — one of the 18 untriaged members — without adding its row, or implementing any of the 18 and leaving the record saying "absent".

### NOM E41-11 — row 10 — `KeyStore`, 12 of **30**

**File:** `native-builtins/src/tls.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/tls.rs:4830-4869` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

The local variable in the old body is literally named `all_methods` and holds twelve of thirty. Five registrations it does not name — `getType`, `getCertificateChain`, `store`, `isCertificateEntry`, `isKeyEntry` — are made by the same registrar, so five deletions were silent.

`<init>()V` is the first legitimate off-surface shape and is recorded: JDK 25's only constructor is `protected KeyStore(KeyStoreSpi, Provider, String)`, so `()V` is not a member at any access level, and this VM allocates its synthetic `KeyStore` by that key.

OLD:

```rust
    #[test]
    fn test_key_store_registration_complete() {
        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "java/security/KeyStore";
        let all_methods = [
            ("<init>", "()V"),
            (
                "getInstance",
                "(Ljava/lang/String;)Ljava/security/KeyStore;",
            ),
            ("getDefaultType", "()Ljava/lang/String;"),
            ("load", "(Ljava/io/InputStream;[C)V"),
            (
                "getCertificate",
                "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
            ),
            ("getKey", "(Ljava/lang/String;[C)Ljava/security/Key;"),
            ("containsAlias", "(Ljava/lang/String;)Z"),
            ("aliases", "()Ljava/util/Enumeration;"),
            ("size", "()I"),
            (
                "setCertificateEntry",
                "(Ljava/lang/String;Ljava/security/cert/Certificate;)V",
            ),
            (
                "setKeyEntry",
                "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V",
            ),
            ("deleteEntry", "(Ljava/lang/String;)V"),
        ];
        for (name, desc) in &all_methods {
            assert!(
                r.find(cls, name, desc).is_some(),
                "Missing KeyStore.{}{}",
                name,
                desc
            );
        }
    }
```

NEW:

```rust
    /// The population is **the JDK's**, not this registrar's.
    ///
    /// `scripts/baselines/jdk25-java.security.KeyStore.tsv`: **30** public members.
    /// `register_tls_natives` makes **16**; the body this replaced named twelve, in
    /// a local called `all_methods`. `store`, `getType`, `getCertificateChain`,
    /// `isCertificateEntry` and `isKeyEntry` are all registered and were all
    /// unasserted, so deleting any of them was silent.
    ///
    /// `<init>()V` is in `OFF_SURFACE` with a reason: JDK 25's only constructor is
    /// `protected KeyStore(KeyStoreSpi, Provider, String)`, so `()V` is not a
    /// member at any access level. This VM allocates its synthetic KeyStore by that
    /// key; real bytecode arrives through `getInstance`, which IS registered.
    #[test]
    fn jdk_surface_keystore_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_tls_natives(&mut r);
        let cls = "java/security/KeyStore";
        let baseline = jdk_baseline::parse(jdk_baseline::KEY_STORE);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("aliases", "()Ljava/util/Enumeration;", true, ""),
            ("containsAlias", "(Ljava/lang/String;)Z", true, ""),
            ("deleteEntry", "(Ljava/lang/String;)V", true, ""),
            ("entryInstanceOf", "(Ljava/lang/String;Ljava/lang/Class;)Z", false,
             "Not registered here; not measured."),
            ("getAttributes", "(Ljava/lang/String;)Ljava/util/Set;", false,
             "Not registered here; not measured."),
            ("getCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;", true, ""),
            ("getCertificateAlias", "(Ljava/security/cert/Certificate;)Ljava/lang/String;", false,
             "Not registered here; not measured."),
            ("getCertificateChain", "(Ljava/lang/String;)[Ljava/security/cert/Certificate;", true, ""),
            ("getCreationDate", "(Ljava/lang/String;)Ljava/util/Date;", false,
             "Not registered here; not measured."),
            ("getDefaultType", "()Ljava/lang/String;", true, ""),
            ("getEntry", "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;", false,
             "Not registered here; not measured."),
            ("getInstance", "(Ljava/io/File;Ljava/security/KeyStore$LoadStoreParameter;)Ljava/security/KeyStore;", false,
             "Not registered here; not measured."),
            ("getInstance", "(Ljava/io/File;[C)Ljava/security/KeyStore;", false,
             "Not registered here; not measured."),
            ("getInstance", "(Ljava/lang/String;)Ljava/security/KeyStore;", true, ""),
            ("getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/KeyStore;", false,
             "Not registered here; not measured."),
            ("getInstance", "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/KeyStore;", false,
             "Not registered here; not measured."),
            ("getKey", "(Ljava/lang/String;[C)Ljava/security/Key;", true, ""),
            ("getProvider", "()Ljava/security/Provider;", false,
             "Not registered here; not measured."),
            ("getType", "()Ljava/lang/String;", true, ""),
            ("isCertificateEntry", "(Ljava/lang/String;)Z", true, ""),
            ("isKeyEntry", "(Ljava/lang/String;)Z", true, ""),
            ("load", "(Ljava/io/InputStream;[C)V", true, ""),
            ("load", "(Ljava/security/KeyStore$LoadStoreParameter;)V", false,
             "Not registered here; not measured."),
            ("setCertificateEntry", "(Ljava/lang/String;Ljava/security/cert/Certificate;)V", true, ""),
            ("setEntry", "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V", false,
             "Not registered here; not measured."),
            ("setKeyEntry", "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V", true, ""),
            ("setKeyEntry", "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V", false,
             "Not registered here; not measured."),
            ("size", "()I", true, ""),
            ("store", "(Ljava/io/OutputStream;[C)V", true, ""),
            ("store", "(Ljava/security/KeyStore$LoadStoreParameter;)V", false,
             "Not registered here; not measured."),
        ];
        #[rustfmt::skip]
        const OFF_SURFACE: &[jdk_baseline::OffSurface] = &[
            ("<init>", "()V",
             "SYNTHETIC <init>. JDK 25's only constructor is `protected \
              KeyStore(KeyStoreSpi,Provider,String)`, so `()V` is not a \
              member at any access level; this VM allocates its synthetic \
              KeyStore by that key. Real bytecode reaches a KeyStore \
              through `getInstance`, which IS registered."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "KeyStore's registered surface disagrees with \
             scripts/baselines/jdk25-java.security.KeyStore.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, OFF_SURFACE);
        assert!(
            off.is_empty(),
            "KeyStore: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **GREEN**. 16 `true`, 14 `false`, one recorded off-surface `<init>()V`.**

**What makes it red:** Deleting `KeyStore.store(Ljava/io/OutputStream;[C)V` or `isKeyEntry` — both registered, neither named by the old twelve-row `all_methods`.

### NOM E41-12 — row 21 — `SynchronousQueue`, 5 of **25**

**File:** `native-builtins/src/phases_late.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/phases_late.rs:9769-9791` — line numbers drift under
concurrent lanes, so the text is the anchor, not the number.

`register_p58_synchronous_queue` makes seventeen registrations and the body this replaced asserted five. The eight remaining JDK members are `containsAll`, the `drainTo(Collection,int)` overload, `remove(Object)`, `removeAll`, `retainAll`, `spliterator`, `toArray(Object[])` and `toString` — every one of them **declared on this class**, which is why they are in the baseline at all and why the eight `false` rows are not noise. `remove(Object)` is the one worth a second look: `SynchronousQueue` overrides it to always answer `false`, and this VM does not register it, so the answer comes from wherever the receiver's real bytecode lands.

OLD:

```rust
    #[test]
    fn sq_real_rendezvous_methods_registered() {
        let mut r = NativeMethodRegistry::new();
        register_p58_synchronous_queue(&mut r);
        let sq = "java/util/concurrent/SynchronousQueue";
        assert!(r.find(sq, "put", "(Ljava/lang/Object;)V").is_some());
        assert!(r.find(sq, "take", "()Ljava/lang/Object;").is_some());
        assert!(r.find(sq, "offer", "(Ljava/lang/Object;)Z").is_some());
        assert!(r
            .find(
                sq,
                "offer",
                "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z"
            )
            .is_some());
        assert!(r
            .find(
                sq,
                "poll",
                "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;"
            )
            .is_some());
    }
```

NEW:

```rust
    /// The population is **the JDK's**, not this registrar's.
    ///
    /// `scripts/baselines/jdk25-java.util.concurrent.SynchronousQueue.tsv`: **23
    /// public methods plus 2 public constructors = 25** members.
    /// `register_p58_synchronous_queue` makes **17**; the body this replaced named
    /// five. Both constructors, `poll()`, `peek`, `size`, `isEmpty`, `contains`,
    /// `iterator`, `toArray`, `clear`, `remainingCapacity` and `drainTo` were all
    /// registered and all unasserted.
    ///
    /// **Scope, stated:** `AbstractQueue` and `BlockingQueue` are baselined
    /// separately (`classes.txt` says why). A member this class inherits rather
    /// than declares is not in this population, and `javap -public` on the leaf is
    /// the reason: it never lists an inherited member.
    #[test]
    fn jdk_surface_synchronousqueue_is_triaged() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_p58_synchronous_queue(&mut r);
        let cls = "java/util/concurrent/SynchronousQueue";
        let baseline = jdk_baseline::parse(jdk_baseline::SYNCHRONOUS_QUEUE);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name (E41 §3).
        assert_eq!(baseline.internal_name(), cls);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            ("<init>", "()V", true, ""),
            ("<init>", "(Z)V", true, ""),
            ("clear", "()V", true, ""),
            ("contains", "(Ljava/lang/Object;)Z", true, ""),
            ("containsAll", "(Ljava/util/Collection;)Z", false,
             "Not registered here; not measured."),
            ("drainTo", "(Ljava/util/Collection;)I", true, ""),
            ("drainTo", "(Ljava/util/Collection;I)I", false,
             "Not registered here; not measured."),
            ("isEmpty", "()Z", true, ""),
            ("iterator", "()Ljava/util/Iterator;", true, ""),
            ("offer", "(Ljava/lang/Object;)Z", true, ""),
            ("offer", "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z", true, ""),
            ("peek", "()Ljava/lang/Object;", true, ""),
            ("poll", "()Ljava/lang/Object;", true, ""),
            ("poll", "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;", true, ""),
            ("put", "(Ljava/lang/Object;)V", true, ""),
            ("remainingCapacity", "()I", true, ""),
            ("remove", "(Ljava/lang/Object;)Z", false,
             "Not registered here; not measured."),
            ("removeAll", "(Ljava/util/Collection;)Z", false,
             "Not registered here; not measured."),
            ("retainAll", "(Ljava/util/Collection;)Z", false,
             "Not registered here; not measured."),
            ("size", "()I", true, ""),
            ("spliterator", "()Ljava/util/Spliterator;", false,
             "Not registered here; not measured."),
            ("take", "()Ljava/lang/Object;", true, ""),
            ("toArray", "()[Ljava/lang/Object;", true, ""),
            ("toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;", false,
             "Not registered here; not measured."),
            ("toString", "()Ljava/lang/String;", false,
             "Not registered here; not measured."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(cls, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "SynchronousQueue's registered surface disagrees with \
             scripts/baselines/jdk25-java.util.concurrent.SynchronousQueue.tsv ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );

        // The FIFTH check, which `audit` structurally cannot make: every row
        // the REGISTRY holds for this class, audited against the JDK. `audit`
        // walks the triage rows and the JDK surface; a registration in
        // neither is invisible to all four of its kinds.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "SynchronousQueue: {} registration(s) key on something JDK 25 does not \
             declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: **GREEN**. 17 `true`, 8 `false`, 0 off-surface.**

**What makes it red:** Deleting `drainTo(Ljava/util/Collection;)I` or `remainingCapacity()I` — twelve of the seventeen registrations were unasserted, including both constructors and the whole `Collection` surface.

### NOM E41-13 — row 13 — `java.base`, 14 of **58**, and the module path end to end

**File:** `native-builtins/src/jdk25_language.rs` (**not owned by this lane**). OLD verified byte-exact and
**unique** in the working tree today, at `native-builtins/src/jdk25_language.rs:833-836`.

E37 §7.1 flagged this row specially: its oracle is the MODULE baseline, and
`jdk25-module-java.base.tsv` has **no `# public-methods` header** — it carries
`# unqualified-exports 58`, a different `# columns` grammar and
`EXPORTS`/`PACKAGE`/`USES`/`PROVIDES` row kinds. That is exactly the file shape
that made E32-3's specified parser panic before E37 fixed it. **Measured today:
the module path works end to end** — `parse` dispatches its recount on
`# kind`, `unqualified_exports_internal()` returns 58 internal-form packages,
and the body below is green under `rustc --test` (§1, 26th test).

The two sibling tests `test_java_base_exports_contains_stream` and
`test_java_base_exports_contains_function` become redundant and should go in
the same change: a spot check for two members of a set is subsumed by a
membership audit of all fourteen against the module.

OLD:

```rust
    #[test]
    fn test_java_base_exports_has_14_entries() {
        assert_eq!(JAVA_BASE_EXPORTS.len(), 14);
    }
```

NEW:

```rust
    /// The population is the JDK MODULE DESCRIPTOR's, not this file's.
    ///
    /// `scripts/baselines/jdk25-module-java.base.tsv`, generated from
    /// `java.base`'s own `module-info` on openjdk 25.0.3+9: **58** unqualified
    /// `exports`. `JAVA_BASE_EXPORTS` (`jdk25_language.rs:32`) carries **14**,
    /// and the body this replaced asserted `14` as a fact. It is a fact about
    /// the array eight hundred lines above it, not about java.base.
    ///
    /// This is the MODULE half of the baseline capability and it needs
    /// different accessors: `jdk_baseline::audit` refuses a `# kind module`
    /// baseline outright, because a module has no public method surface and
    /// every triage row would come back STALE — a clean green from a guard that
    /// examined nothing, which is E25 row 32's shape.
    /// `unqualified_exports_internal()` does the binary→internal spelling
    /// (`java.util.stream` → `java/util/stream`) that every package constant in
    /// this crate uses.
    ///
    /// Two directions, both live:
    ///
    /// * **kind 4** — every entry must be a real unqualified export. A typo
    ///   (`java/util/streams`), or a package java.base exports only
    ///   qualified-to-a-named-module (`sun/nio/ch`), fails here. All fourteen
    ///   pass today, and nothing had ever checked that.
    /// * **kind 1** — the **44** java.base exports this resolver does NOT carry
    ///   are counted rather than omitted. `import module java.base;` does not
    ///   resolve `java/nio/file`, `java/lang/ref`, `java/util/regex`,
    ///   `java/lang/annotation` or 40 others in this VM. That is a real defect
    ///   this guard now records; closing it is a resolver change and needs a
    ///   run, so the number is frozen with instructions for moving it in either
    ///   direction.
    #[test]
    fn java_base_exports_are_a_stated_subset_of_the_real_module() {
        let baseline = jdk_baseline::parse(jdk_baseline::MODULE_JAVA_BASE);
        assert_eq!(baseline.kind, "module");
        let real = baseline.unqualified_exports_internal();
        assert_eq!(
            real.len(),
            58,
            "`java --describe-module java.base` reports 58 unqualified exports on \
             openjdk 25.0.3+9; this baseline says {}.",
            real.len()
        );

        // Kind 4 — a package this VM claims java.base exports, and it does not.
        let invented: Vec<&&str> = JAVA_BASE_EXPORTS
            .iter()
            .filter(|p| !real.iter().any(|r| r == *p))
            .collect();
        assert!(
            invented.is_empty(),
            "JAVA_BASE_EXPORTS names {} package(s) java.base does not export \
             unqualified on java.version {}: {:?}. A resolver that admits a package \
             the module does not export resolves an import HotSpot refuses.",
            invented.len(),
            baseline.java_version,
            invented
        );

        // Kind 1 — the absences, recorded rather than omitted.
        let missing: Vec<&String> = real
            .iter()
            .filter(|r| !JAVA_BASE_EXPORTS.contains(&r.as_str()))
            .collect();
        assert_eq!(
            missing.len(),
            44,
            "`import module java.base;` does not resolve these packages in this VM. \
             44 is the count on openjdk 25.0.3+9 as of E41. If it went DOWN, delete \
             rows here in the same change that adds them to JAVA_BASE_EXPORTS; if it \
             went UP, java.base grew an export and this resolver did not follow. \
             Missing: {:?}",
            missing
        );
        assert_eq!(JAVA_BASE_EXPORTS.len() + missing.len(), real.len());
    }
```

**PREDICTED: GREEN.** Measured under `rustc --test` against the real baseline
and a verbatim copy of `JAVA_BASE_EXPORTS`; the only thing not measured is that
the constant is still spelled that way when the change lands.

**What makes it red:** Adding a fifteenth `JAVA_BASE_EXPORTS` entry that
`java.base` does not export unqualified — a typo like `java/util/streams`, or
`sun/nio/ch`, which java.base exports only to named modules.


### NOM E41-14 — row 16, the `$Config` half — **the guard cannot be written until the source is fixed**

**File:** `native-builtins/src/jdk25_concurrency.rs` (**not owned by this
lane**). This is not a test rewrite. It is the source change the test rewrite
depends on, and it is the reason NOM E41-6/7/8 are only three quarters of row
16.

`CLS_CONFIG` is defined as `"java/util/concurrent/StructuredTaskScope$Config"`,
`s52_class_name_config` asserts that spelling, and
`s52_total_registration_count` counts six methods on it. **There is no such
type in the JDK 25 runtime image.** Kind 4 catches this two ways and the second
is the stronger: a guard converted to read a baseline for `$Config` **does not
compile**, because `include_str!` cannot resolve a file the generator refuses
to write. There is no way to write a green version of that test, and that is
the correct outcome.

Audited against the real `$Configuration` baseline, all six registrations come
back `STALE` and three of them carry the sharper descriptor diagnosis — the
fabricated name is baked into the return types of `withName`,
`withThreadFactory` and `withTimeout` as well. `getName` and `getThreadFactory`
are not members of the JDK type under any descriptor.
`jdk_baseline::tests::kind_four_would_have_caught_structured_task_scope_config`
is that transcript, measured, against the checked-in baseline.

OLD (verified unique):

```rust
    #[test]
    fn s52_class_name_config() {
        assert_eq!(
            CLS_CONFIG,
            "java/util/concurrent/StructuredTaskScope$Config"
        );
    }
```

NEW — **delete this test** and respell the constant it guards:

```rust
    // `s52_class_name_config` deleted 2026-08-13 (E41). It asserted a string
    // literal against a string literal, and the literal was wrong:
    // `java/util/concurrent/StructuredTaskScope$Config` is not a type in the
    // JDK 25 runtime image. The check that replaces it is
    // `assert_eq!(baseline.internal_name(), CLS_CONFIG)` inside the converted
    // registration census, where the name is checked against
    // `jrt:/modules/java.base/...` instead of against the code that invented
    // it. See docs/known-issues/jdk-only/
    // E41-R11-TWELVE-GUARDS-CONVERTED-20260813.md §3 and E32-R11 §8.
```

and, at `CLS_CONFIG`'s definition:

```rust
const CLS_CONFIG: &str = "java/util/concurrent/StructuredTaskScope$Configuration";
```

**PREDICTED: this changes what the VM registers, not just what a test asserts,
and it needs a run.** Three of the six methods (`withName`,
`withThreadFactory`, `withTimeout`) exist on `$Configuration` but only with
`$Configuration` in their return descriptor, so their descriptors must be
respelled in the same change or they move from one unreachable key to another.
The other three (`<init>()V`, `getName`, `getThreadFactory`) are not members of
`$Configuration` at all and have nowhere to go: `$Configuration` is an
interface with exactly three methods. **Do not make this change to get a test
green.** It is a behaviour change on a JEP 505 surface and belongs to a lane
that can run the VM.

**What makes it red:** nothing, ever, in its present form — which is the
finding. `s52_class_name_config` compares `CLS_CONFIG` to a copy of itself, so
no JDK, no registration and no rename can falsify it. That is E25's shape (B)
at its purest, on a name that is already wrong.


### NOM E41-15 — row 20 — E37's `SubmissionPublisher` rewrite, independently verified, plus the fifth check

**File:** `native-builtins/src/phases_late.rs` (**not owned by this lane**).
NOM **E37-2** already carries the full rewrite and has **not landed** — the OLD
body is still in the tree today at
`native-builtins/src/phases_late.rs:9454-9464` (the line numbers moved from E37's `:9027` under a
concurrent lane; the text is unchanged and unique).

This lane re-derived the registration set with a **different program** — a
mechanical extraction over the registrar call graph rather than E37's
hand-plus-regex read — and independently reproduces:

* **11** registrations on `java/util/concurrent/SubmissionPublisher` from
  `sp_registry()`;
* **20** public members in the baseline (17 methods + 3 constructors);
* all 11 on the public surface, so **0 off-surface** — the only one of the
  thirteen classes audited today with nothing to record.

NOM E37-2's TRIAGE table is measured to `0 problems` here as well (§1, harness
test `row_20_submissionpublisher`). **Land E37-2 as written.**

One addition is nominated on top of it: the fifth check. Append to E37-2's NEW
body, immediately before the closing brace:

```rust
        // The FIFTH check (E41 §2): every row the REGISTRY holds for this
        // class, audited against the JDK. `audit` walks the triage and the JDK
        // surface, so a registration in neither is invisible to all four of its
        // kinds. `SubmissionPublisher` is the one class of the thirteen audited
        // on 2026-08-13 with nothing to record — the empty third argument is
        // the assertion, not an omission.
        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == sp)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "SubmissionPublisher: {} registration(s) key on something JDK 25 \
             does not declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
```

**PREDICTED: GREEN**, and measured as such against the extracted registration
set.

**What makes it red:** Deleting `register_p69_submission_publisher`, which
drops `closeExceptionally` and `getClosedException` — the two methods p69
exists to add, and the two the old four-row body named neither of.


### NOM E41-16 — `native-builtins/src/cds.rs` — the owner class says `getCDSMetrics` does not exist

**File:** `native-builtins/src/cds.rs` (**not owned by this lane**). This is a
**new test**, not a rewrite, and it is the only reachable half of §3.1:
`sun.management.CDSMetrics` is not in the JDK 25 image, so its six
registrations have no oracle. Its declared owner does.

Add next to the converted `test_all_registered_methods_findable` (NOM E41-2):

```rust
    /// `register_cds_natives` registers
    /// `getCDSMetrics()Lsun/management/CDSMetrics;` on
    /// `sun/management/ManagementFactoryHelper`, and six more natives on
    /// `sun/management/CDSMetrics` itself.
    ///
    /// **`sun.management.CDSMetrics` is not in the JDK 25 runtime image.** A
    /// `jrt:/` walk of every root finds no such class file — the same refusal
    /// the generator gave for `StructuredTaskScope$Config` (E32-R11 §8). So
    /// there is no baseline for it, there cannot be one, and the six
    /// registrations on it cannot be audited from this side at all.
    ///
    /// What CAN be audited is the owner, and it says the same thing from the
    /// other end. `sun.management.ManagementFactoryHelper` IS in the image, was
    /// baselined on 2026-08-13, declares 22 public methods, and `getCDSMetrics`
    /// is not among them at any access level. A native registered under a name
    /// its own owner class does not declare is a native no bytecode can reach.
    ///
    /// See docs/known-issues/jdk-only/E41-R11-TWELVE-GUARDS-CONVERTED-20260813.md
    /// §3.1 and the `classes.txt` stanza that records why CDSMetrics is absent.
    #[test]
    fn the_cds_metrics_owner_does_not_declare_get_cds_metrics() {
        use crate::jdk_baseline;

        let mut r = NativeMethodRegistry::new();
        register_cds_natives(&mut r);
        let cls = "sun/management/ManagementFactoryHelper";
        let baseline = jdk_baseline::parse(jdk_baseline::SUN_MANAGEMENT_FACTORY_HELPER);
        assert_eq!(baseline.internal_name(), cls);
        assert_eq!(baseline.public_methods().len(), 22);

        let live: Vec<(&str, &str)> = r
            .dump_registrations()
            .into_iter()
            .filter(|(c, _, _, _)| *c == cls)
            .map(|(_, n, d, _)| (n, d))
            .collect();
        // Both rows are deliberately UNRECORDED. `getCDSMetrics` is not a
        // member at any access level, and `ManagementFactoryHelper` declares
        // NO public constructor either (`# public-methods 22`, zero public
        // `<init>` rows), so the synthetic `<init>()V` is off-surface too. A
        // `reason` on a fabricated name is standing permission, which is the
        // thing this ratchet exists to remove.
        let off = jdk_baseline::audit_off_surface(&baseline, &live, &[]);
        assert!(
            off.is_empty(),
            "sun/management/ManagementFactoryHelper: {} registration(s) key on \
             something JDK 25 does not declare:\n  {}",
            off.len(),
            off.join("\n  ")
        );
    }
```

**PREDICTED: RED**, naming `getCDSMetrics()Lsun/management/CDSMetrics;` and
the synthetic `<init>()V` alongside it. The `getCDSMetrics` half is measured —
`jdk_baseline::tests::management_factory_helper_does_not_declare_get_cds_metrics`
produces exactly one `OFF-SURFACE` line for it against the real baseline. The
`<init>()V` half is read off the same baseline (`# public-methods 22`, zero
public `<init>` rows) and is not separately measured through the registry.

**What makes it red:** it is red today. After the CDSMetrics registrations are
removed or moved to a `cratonvm/…` class, it goes red again if anything
re-registers a name `ManagementFactoryHelper` does not declare.


---

## 6. Residuals, stated so a green run is not read as more than it is

* **NOM E41-0 is BLOCKING and is E37's, unlanded.** `jdk_baseline.rs` is
  committed and compiled by nothing: `grep -n jdk_baseline
  native-builtins/src/lib.rs` returns nothing today. Twenty-five passing tests
  in a module no build includes is a file that can never go red. Everything
  else here waits on one `mod` declaration.
* **The registration oracle is an extraction, not `NativeMethodRegistry`.** A
  conditional registration, a feature-gated branch, or a triple overwritten
  inside a fixture would break the prediction without breaking the extraction.
  `[setup lies]`. The extractor resolves `let cls = "…"`, file-scope `const`s,
  and `for cls in ["a", "b"] {` loops, and reports zero unresolved class
  arguments on every registrar audited here — but "resolves everything it saw"
  is a statement about its reach, not its correctness. `[reach≠defect]`.
* **The JDK side does not depend on that.** A `STALE` or `OFF-SURFACE
  (descriptor)` verdict is a fact about the baseline and the triple, and every
  finding in §3 holds however the registrations are enumerated. What the
  extraction could get wrong is a `DROPPED`/`CLOSED`, i.e. a direction, not a
  membership.
* **Six of the twelve nominations are RED on landing, deliberately.** Rows 3,
  4-6, 8, 16 and 22. §2's rule is the reason: a fabricated name never gets a
  `reason` field, because a reason is permission. Landing them means fixing
  thirteen registrations across five files or accepting a red tree, and that is
  a decision for the owning lanes, not a defect in the guards.
* **Every `false` row says "Not registered here; not measured."** That is the
  honest state and not a placeholder. Each is a decision someone has to make
  with a running VM: unimplemented, or deliberately out of scope. The ratchet's
  value is that the decision is now recorded and enforced in both directions,
  not that it has been made. The rows worth looking at first are
  `ManagementFactory.getPlatformMBeanServer` (deliberate, explained in prose by
  the old comment, asserted by nothing) and `RuntimeMXBean.getPid`.
* **`audit_off_surface`'s inherited-member shape is a promise, not a check.**
  A row that says "inherited from `PlatformManagedObject`" is taken on trust
  until that type is baselined. Baselining it converts the row into a `STALE
  OFF-SURFACE row` and forces it into TRIAGE — which is the mechanism working,
  but nothing schedules it. `java.lang.management.PlatformManagedObject` is one
  `classes.txt` line away.
* **`sun.management.CDSMetrics` has no oracle and never will.** Six
  registrations plus a seventh returning it are outside every instrument this
  capability has. The `classes.txt` stanza records why, so the next person does
  not add the line and get a refusal with no explanation.
* **Row 8's and rows 4-6's denominators in E37 §7.1 were wrong** (25 → 16, 30 →
  65) and were corrected here by measurement. Both errors came from reading
  `javap` output line counts rather than the class file. This record's own
  numbers are recounts by the Rust parser of files the Python generator wrote
  from `java.lang.classfile`, which is three implementations; the fixed point
  outside all of them is still the `javap` transcript.
* **`generate.py --check` is still not wired into CI** (E32 §9, E37 §8,
  unchanged). Thirty-two baselines now agree with this JDK on this host. Nothing
  regenerates them on a schedule, so a JDK bump is visible only to
  `parse`'s version pin, and only once somebody regenerates.
* **The `<init>()V` off-surface rows are a design decision, not a discovery.**
  Five of the thirteen classes carry one. They are recorded because this VM
  genuinely allocates synthetic stubs by that key, and recording them means a
  `<init>` registration that STOPS being needed shows up as a `DEAD OFF-SURFACE
  row`. If a later lane decides synthetic constructors should be keyed
  differently, all five rows go red at once — which is the intended blast
  radius.
