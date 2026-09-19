# F17-1 — eleven CDS natives for classes JDK 25 does not have, two real natives a public-only baseline could not see, and a factory-name guard that punished the correct spelling

**2026-08-13, lane F17.** Acts on F8's off-surface findings for
`native-builtins/src/cds.rs`, `shared_secrets_bridge.rs`,
`jdk25_concurrency.rs` and `phases_late/concurrent.rs` — the four files this
lane owns. All patches are in the working tree; nothing is committed.

**This lane may not build or run CratonVM, and did not.** Every JDK fact below
is `javap`/`java` on this host (Microsoft build **25.0.3+9-LTS**, `java -version`
→ `OpenJDK Runtime Environment Microsoft-13877124 (build 25.0.3+9-LTS)`) or the
JDK 25 source checkout at `C:\craton\jdk25src`, and is quoted. Every claim about
CratonVM's behaviour — before and after — is **PREDICTED** from source. All four
files were parse-checked (`rustfmt --edition 2021 --emit stdout` on scratch
copies, exit 0, zero `error:` lines), which rules out syntax errors and nothing
else; none was type-checked.

---

## 0. Verdict

| F8 claim | verdict |
|---|---|
| `sun.management.CDSMetrics` is absent from the JDK 25 image; 6 registrations + a `ManagementFactoryHelper.getCDSMetrics` that `ManagementFactoryHelper` does not declare | **CONFIRMED and DELETED** (§1) |
| `jdk.internal.misc.CDS` — 5 of 10 registrations off-surface, one being `logLambdaFormInvoker(String)V` where the JDK declares four String parameters | **PARTLY WRONG. 2 of 10 are off-surface, not 5, and `logLambdaFormInvoker(String)V` IS the real native** — it is `private`, and F8's oracle is public-only. Two REAL natives were missing and the same blind spot is why (§2) |
| `SharedSecrets.getJavaSecurityAccess` removed by JEP 486; `getJavaUtilJarAccess` is really `javaUtilJarAccess`; `every_factory_returns_access_interface` is a guard checking the tree against itself | **CONFIRMED on all three. One deleted, one corrected, the guard replaced** (§3) |
| `StructuredTaskScope` is wrong four ways beyond `$Config`: `isShutdown`, `shutdown`, `joinUntil`, `join()` return type; plus `$Joiner.policy()I` and `$Subtask.task()` | **CONFIRMED as facts, but the file is wrong.** All six are in `jdk25_concurrency.rs`. `phases_late/concurrent.rs`'s JEP 505 registrar is 100% JDK-true, all nine triples (§4) |
| — | **NEW, not on F8's list: three more whole-class / real-class fabrications in `cds.rs`** — `sun/misc/VM` (3 registrations, class absent) and `java/lang/ClassLoader.getCdsArchivePath` (1 registration, class present, member absent) (§1.2) |

Net effect on `cds.rs`: **22 registrations → 12.** Eleven deleted, one renamed,
two added.

---

## 1. `cds.rs` — three fabricated surfaces, deleted

### 1.1 `sun/management/CDSMetrics` and its factory (7 registrations)

```
$ javap sun.management.CDSMetrics                        # 25.0.3+9-LTS
Error: class not found: sun.management.CDSMetrics

$ javap -p sun.management.ManagementFactoryHelper | grep -c CDS
0
```

The second command is the one that matters and it is a different shape from the
first. `ManagementFactoryHelper` **is** in the image and does load; `javap -p`
prints all 22 of its methods and not one mentions CDS. So the failure mode was
never "class missing at the call site" — it was a real, loadable JDK class
carrying a native for a method it does not declare, plus six more on a class
that does not exist at all.

Deleted: `ManagementFactoryHelper.getCDSMetrics()Lsun/management/CDSMetrics;`
and all six `sun/management/CDSMetrics` registrations (`<init>` plus five
accessors), with the bodies `native_get_cds_metrics`,
`native_cds_metrics_init` and the helper `alloc_cds_metrics_obj`.
`ManagementFactoryHelper.<init>()V` is **kept** — the class is real and the
empty body is its real implementation.

The Rust `CdsMetrics` struct is **kept**. It is the archive generator's own
bookkeeping type with tests of its own; only the Java class it was projected
into was invented.

### 1.2 Two more that F8 did not list

Found by sweeping the registrar's own class list against `javap` rather than by
following the report:

```
$ javap -p sun.misc.VM
Error: class not found: sun.misc.VM

$ javap -p java.lang.ClassLoader | grep -i -e archive -e cds
  private void resetArchivedStates();
```

* **`sun/misc/VM` — 3 registrations deleted** (`<init>`, `isBooted()Z`,
  `savedProps()Ljava/util/Properties;`), with `native_vm_is_booted`,
  `native_vm_saved_props` and the helper `alloc_properties_obj`.

  **Nothing was added in their place, deliberately.** The JDK-true home is
  `jdk.internal.misc.VM`, and `lib.rs` already owns that class in full —
  `isBooted()Z` is registered at `lib.rs:14894` alongside `initLevel`,
  `getSavedProperty` and the rest. `register()` is last-write-wins, so a second
  body here would have made *which one runs* a function of registrar call order.
  The shapes do not transfer anyway: JDK 25 declares
  `getSavedProperties()Ljava/util/Map;` and
  `getSavedProperty(Ljava/lang/String;)Ljava/lang/String;`, with `savedProps`
  surviving only as a private **field** — so `savedProps()` had no
  descriptor-compatible successor to be corrected into.

* **`java/lang/ClassLoader.getCdsArchivePath()Ljava/lang/String;` — 1
  registration deleted.** That single `resetArchivedStates` hit is the whole
  CDS-adjacent surface of `ClassLoader` in JDK 25. This is the most dangerous of
  the three shapes: a registration on a real, always-loaded class reads as
  legitimate at a glance, and no "class not found" ever fires to contradict it.

### 1.3 Dispatch check, done before deleting

`call_native` panics on an unregistered triple, and a Rust panic is not a Java
throwable — it kills the VM rather than raising something the program can catch.
So the check ran first, not after:

* A tree-wide grep for `sun/misc/VM`, `CDSMetrics` and `getCdsArchivePath`
  finds no caller outside `cds.rs` and its tests. The only surviving mention
  anywhere is a prose comment at `lib.rs:14892`.
* No JDK 25 bytecode can reach any of them: two of the three classes are not in
  the image, and the third does not declare the member.
* And `register_cds_natives` is doubly out of reach of both shipping modes
  regardless — it is called only from `register_synthetic_overrides`
  (`#[cfg(feature = "synthetic-jdk")]`) and only under
  `#[cfg(feature = "experimental-aot")]`.

**PREDICTED: no behaviour change in any mode.** The deletions are of things
nothing could dispatch to.

---

## 2. `jdk.internal.misc.CDS` — where F8's count comes from, and why it is wrong in both directions

This is the finding worth carrying forward, because it is a property of the
instrument rather than of this class.

### 2.1 The oracle is public-only

`scripts/jdk-baseline/generate.py:175` keeps a member only if its flags contain
`public`:

```python
and "public" in r.split("\t")[3].split(","))
```

`scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv` says so in its own header:
`# public-methods 12`, `# rows 16`. But `javap -p jdk.internal.misc.CDS` lists
**23** members. The eleven the baseline cannot see are exactly the
package-private and private ones — **which is the access level most JDK natives
live at.** Auditing a *native* registrar against a public-only surface is
auditing the one population the oracle systematically omits.

This produced a false positive and a false negative on the same class.

### 2.2 The false positive: `logLambdaFormInvoker`

F8 reported the registration `logLambdaFormInvoker(Ljava/lang/String;)V` as
off-surface because the baseline lists a four-String method under that name.
Measured:

```
$ javap -p jdk.internal.misc.CDS | grep logLambdaFormInvoker
  private static native void logLambdaFormInvoker(java.lang.String);
  public static void logLambdaFormInvoker(java.lang.String, java.lang.String,
                                          java.lang.String, java.lang.String);
```

JDK 25 declares **both**. They are an overload pair, not a divergence. The
four-String form is ordinary Java —
`jdk25src/java.base/jdk/internal/misc/CDS.java:142` — whose entire body
concatenates its arguments and calls the one-String native at line 144:

```java
public static void logLambdaFormInvoker(String prefix, String holder, String name, String type) {
    if (isLoggingLambdaFormInvokers()) {
        logLambdaFormInvoker(prefix + " " + holder + " " + name + " " + type);
    }
}
```

So "correcting" the registration to the 4-arg descriptor would have been wrong
twice over: it would shadow real bytecode with a body that silently drops three
arguments, and it would remove the only registration for the one form that has
**no** bytecode to fall back on. **The registration is KEPT**, with this
transcript at the site.

Only **2** of the 10 registrations were genuinely off-surface — no name in
`javap -p`'s 23 matches either:

* `isDumpingClassList()Z` — **DELETED.** No JDK 25 predicate corresponds to it.
  `isDumpingArchive()Z` and `isDumpingStaticArchive()Z` both exist and neither
  means "is a class list being written"; `dumpClassList(String)` is the action,
  and HotSpot gates it on the same `configStatus` word rather than on a
  predicate of its own.
* `isSharingEnabled()Z` — **RENAMED to `isUsingArchive()Z`.** Same descriptor,
  same meaning, JDK-true name; body unchanged. The JDK's own javadoc on
  `isUsingArchive` is *"Is the VM using at least one CDS archive?"*.

### 2.3 The false negative: two real natives nobody registered

The blind spot cuts the other way too, and this half is the more serious one —
a public-only baseline reports a *missing* native as nothing at all.

```
$ javap -p jdk.internal.misc.CDS | grep -e getCDSConfigStatus -e needsClassInitBarrier
  private static native int getCDSConfigStatus();
  public static boolean needsClassInitBarrier(java.lang.Class<?>);
  private static native boolean needsClassInitBarrier0(java.lang.Class<?>);
```

Both are **ADDED** by this lane.

`getCDSConfigStatus()I` is the load-bearing one. `CDS.java:55` is:

```java
private static final int configStatus = getCDSConfigStatus();
```

— a `<clinit>` call. Every one of the class's five public predicates
(`isLoggingLambdaFormInvokers`, `isDumpingArchive`, `isUsingArchive`,
`isDumpingStaticArchive`, `isSingleThreadVM`) reads a field only this native can
fill, so merely *initialising* `CDS` needs it. Returns 0: the bits are
`IS_DUMPING_ARCHIVE | IS_DUMPING_METHOD_HANDLES | IS_DUMPING_STATIC_ARCHIVE |
IS_LOGGING_LAMBDA_FORM_INVOKERS | IS_USING_ARCHIVE` (CDS.java:50-54) and CratonVM
does none of those five things.

`needsClassInitBarrier0(Ljava/lang/Class;)Z` shows the trap in miniature: the
**public** half `needsClassInitBarrier` IS on the baseline, so a name-keyed audit
reads the family as covered — while the method that actually needs a body is the
one ending in `0`. Registering the public half instead would be inert under
`--jdk-only` anyway: it has real bytecode, so §7 step 3 answers `Bytecode`.
Returns `false`, for the same reason as above: the barrier orders initialisation
of classes reached through an archived heap subgraph, and this VM maps none.

**PREDICTED:** in `--jdk-only` / `--real-jdk`, no change (registrar not compiled
in). In a `synthetic-jdk` + `experimental-aot` build, `CDS.<clinit>` gains a body
for the native it calls; previously it would have hit `call_native`'s panic if
anything ever initialised the class there.

---

## 3. `shared_secrets_bridge.rs` — the only one of the four files that is in the `--jdk-only` path

`register_wp1_4_shared_secrets` is called from `lib.rs:10063`, inside
`register_essential_natives_with_shims` (starts `lib.rs:7108`), **not** inside
`register_synthetic_overrides` (starts `lib.rs:21612`). Everything in this
section ships in every mode.

### 3.1 `getJavaSecurityAccess` — DELETED, no equivalent exists

```
$ javap jdk.internal.access.JavaSecurityAccess
Error: class not found: jdk.internal.access.JavaSecurityAccess

$ javap jdk.internal.access.SharedSecrets | grep -c getJavaSecurityAccess
0
```

JEP 486 removed the Security Manager and took the interface with it, so there is
no differently-spelled equivalent to correct into — the whole *type* is gone, not
just the getter. `ls jdk25src/java.base/jdk/internal/access/ | grep -i security`
returns `JavaSecurityPropertiesAccess`, `JavaSecuritySignatureAccess`,
`JavaSecuritySpecAccess`, `JavaxSecurityAccess` and no `JavaSecurityAccess`.

**The near-miss to not fall for:**
`getJavaxSecurityAccess()Ljdk/internal/access/JavaxSecurityAccess;` **is** on the
JDK 25 surface, one character away in a name-keyed search. It is a different
interface (`javax.security.auth.Subject` plumbing), not a rename.

`register_java_security_access` — two natives on `java/security/AccessController$1`
— is **KEPT**, as a decision. That owner class is fabricated too
(`javap java.security.AccessController$1` → class not found; JDK 25's
`AccessController` has no anonymous inner classes left), but its *name* is pinned
by two files this lane does not own: `native-api/src/no_image_receiver.rs:143`
lists it and `native-builtins/tests/stub_ratchet.rs:325` asserts on that listing.
Deleting here without moving those in the same commit would redden a ratchet for
a change with no behavioural effect. **Nominated in §6.**

### 3.2 `getJavaUtilJarAccess` → `javaUtilJarAccess` — corrected

```
$ javap jdk.internal.access.SharedSecrets | grep JarAccess
  public static ...JavaUtilJarAccess javaUtilJarAccess();
  public static void setJavaUtilJarAccess(...JavaUtilJarAccess);
```

The **setter** has the `get`-symmetric prefix and the getter never has; that
asymmetry is almost certainly where the invented spelling came from. Descriptor
and owner class are unchanged, so this is a one-token repair.

Corroboration that it was never reached:
`scripts/baselines/jdk-only-dead-everywhere.tsv:192` already records
`jdk/internal/access/SharedSecrets getJavaUtilJarAccess … method-nowhere`.

### 3.3 The guard that could not fail — and that punished the correct spelling

`every_factory_returns_access_interface`, in full as it stood:

```rust
for (method, ret, _) in FACTORIES {
    assert!(method.starts_with("getJava"));
    assert!(method.ends_with("Access"));
    assert!(ret.contains("Access;"));
}
```

Every name in `FACTORIES` was *written* to that shape, so the test checked the
list against itself. It had no way to tell a JDK-true getter from an invented
one and passed on both defects above for as long as they were listed.

The sharper point, and the reason this is worth a section rather than a line:
**the guard actively opposed the fix.** `javaUtilJarAccess` has no `get` prefix,
so repairing the table would have reddened the very test that was supposed to
protect it — a shape that trains the next reader to revert the repair.

Replaced by `every_factory_is_declared_by_jdk25_shared_secrets`, which checks
each `(method, descriptor)` against `jdk_baseline::SHARED_SECRETS` — the JDK 25
class surface walked out of the runtime image — and, on failure, prints
`descriptors_named(method)` so "you spelled the descriptor wrong" is
distinguishable from "this member does not exist".

**Why a public-only baseline is a sound oracle *here* but not in §2:** every
member of `SharedSecrets` is `public static`. Its 65 baseline rows are its whole
surface, and `javap` (no `-p`) returns the same set. The filter that hides CDS's
natives removes nothing here.

Added alongside it: `jdk25_baseline_rejects_the_two_spellings_f17_1_removed`, a
mutation check. Without it the replacement would be one silent `parse()` change
away from being as vacuous as the test it replaced — a `Baseline` that parsed to
zero rows would make `declares` false for everything, and a loop over an empty
`FACTORIES` would still pass. It pins both directions: the two removed spellings
must be rejected, and `javaUtilJarAccess` must be accepted.

### 3.4 Two counts that look like one and are not

`all_factories_listed` 15 → **14**. `owner_classes_are_unique` derives from
`FACTORIES.len()`, so it needed no edit.

`representative_method_registered_per_owner` asserts `expected.len() == 15` and
**still passes**, because its list is hand-written over the `register_*_access`
calls, not over `FACTORIES`. The two lists have never had the same membership and
now do not have the same length either. Measured against the current tree: owners
probed there with no `FACTORIES` entry are `java/io/ObjectInputStream$1`
(deliberately not intercepted) and now `java/security/AccessController$1`; the
factory owner not probed there is `java/io/FileDescriptor$1`. A comment at the
site says so, because re-deriving either count from the other is how they drift.

---

## 4. `StructuredTaskScope` — right facts, wrong file

### 4.1 `phases_late/concurrent.rs` is clean, and that needed checking

F8's item 4 makes `StructuredTaskScope` sound broken, and this lane owns the file
holding the JEP 505 registrar, so the natural move is to start editing there.
Don't. All nine triples in `register_p67_structured_task_scope_j25` were checked
one at a time and every one is JDK-true, descriptor included:

```
$ javap java.util.concurrent.StructuredTaskScope
  public abstract R join() throws InterruptedException;    -> ()Ljava/lang/Object;
  public abstract boolean isCancelled();
  public abstract <U extends T> Subtask<U> fork(Runnable);
  public static <T,R> StructuredTaskScope<T,R> open(Joiner, Function);
$ javap -p java.util.concurrent.StructuredTaskScope\$Joiner
  public static <T> Joiner<...> allUntil(Predicate<...>);
  public default boolean onFork(Subtask<? extends T>);
$ javap -p java.util.concurrent.StructuredTaskScope\$Configuration
  withName / withThreadFactory / withTimeout
```

`StructuredTaskScope` and its two nested types are interfaces whose every member
is public, so the frozen baselines are a complete oracle here and they agree.
**No change made; the verification is recorded at the site** so the next lane
does not re-run `javap` — a doc-only edit.

### 4.2 `jdk25_concurrency.rs` — seven off-surface triples, documented not deleted

All six of F8's defects, plus one it did not name, are here. Measured:

* `<init>()V`, `<init>(String)V`, `<init>(String,ThreadFactory)V` — `javap`
  reports `public interface java.util.concurrent.StructuredTaskScope<T,R> extends
  AutoCloseable`. JEP 505 made it an **interface**; interfaces have no
  constructors. Three triples. *(Not on F8's list.)*
* `isShutdown()Z`, `shutdown()V` — gone. `isCancelled()Z` replaces the predicate;
  the mutator has no replacement, because cancellation is the `Joiner`'s decision
  via `onComplete`.
* `joinUntil(Ljava/time/Instant;)…` — gone; the deadline moved onto
  `Configuration.withTimeout(Duration)`.
* `join()Ljava/util/concurrent/StructuredTaskScope;` — **name real, descriptor
  not.** JEP 505 changed the return type to `R`, so javac emits
  `()Ljava/lang/Object;`. This is precisely the shape a name-keyed search reports
  as agreement.
* `$Subtask.task()Ljava/util/concurrent/Callable;` — `javap -p …$Subtask` lists
  exactly `state()`, `get()`, `exception()`.

**NOT DELETED.** W7-18 already made an explicit, measured decision on the
adjacent `ShutdownOn*`/`$Config`/`Joiner.policy` block, on a two-part test, and
the test comes out the same way here — but the *second* part fails for a
different reason, so it was re-checked rather than assumed:

* *Cannot move either shipping mode?* **TRUE.**
  `register_jdk25_concurrency_natives` is reached only from
  `register_synthetic_overrides`, `#[cfg(feature = "synthetic-jdk")]`, not a
  default feature of `cratonvm-vm` or `cratonvm-cli`.
* *Is the deletion checkable?* **FALSE without a run.** Unlike
  `Joiner.policy()I` — pinned by `r.find` at two sites — these seven triples are
  **not** pinned by any `r.find` in this file. What binds them is the other
  direction: `native_sts_join_until` and `native_subtask_task` have direct-call
  unit tests (`p82_join_until_*` and the `$Subtask` test above them). Deleting
  the registrations alone leaves those bodies reachable only from `#[cfg(test)]`
  code, i.e. `dead_code` warnings in a release build. Registrations, bodies and
  tests must go together — and the one mode that could observe the result,
  `--synthetic-jdk`, has never been executed.

A JDK-ONLY-NOTE carrying the measurements above is added at the block, explicitly
extending W7-18's note (which scopes itself to "every registration between here
and the `Joiner` block below" and so left this block untriaged). The deletion is
**nominated** in §6.

**The coupling to read before touching either file:** all three STS registrars
sit inside `register_synthetic_overrides`; `register_phase67_natives`
(`lib.rs:24121`, reaching the JEP 505 registrar) runs **before**
`register_jdk25_concurrency_natives` (`lib.rs:24300`), and `register()` is
last-write-wins. Adding any JEP 505 triple to `jdk25_concurrency.rs` silently
replaces a correct body with a JDK-21-shaped one, with no warning and no
duplicate-registration row. `w7_18_jep505_surface_is_not_shadowed_here` is the
ratchet that catches it.

---

## 5. Tests expected to flip, and in which direction

All **PREDICTED**; nothing was built or run.

| test | file | direction |
|---|---|---|
| `every_factory_returns_access_interface` | shared_secrets_bridge.rs | **removed** — replaced by `every_factory_is_declared_by_jdk25_shared_secrets` (new, expected GREEN) and `jdk25_baseline_rejects_the_two_spellings_f17_1_removed` (new, expected GREEN) |
| `all_factories_listed` | shared_secrets_bridge.rs | edited 15 → 14; GREEN either way, would have gone RED unedited |
| `owner_classes_are_unique`, `all_factory_entry_points_registered`, `representative_method_registered_per_owner` | shared_secrets_bridge.rs | unchanged, expected GREEN (first two derive from `FACTORIES`; the third is hand-written and `register_java_security_access` was kept) |
| `test_cds_metrics_accessors_registered`, `test_classloader_cds_archive_path_registered`, `test_sun_misc_vm_registered` | cds.rs | **removed** — each asserted `is_some()` on a triple whose defect was that it existed, so the only way to fail them was to fix the bug. Replaced by `f17_1_registrar_mints_nothing_absent_from_the_jdk25_image`, which asserts the inverse |
| `test_management_factory_helper_registration` | cds.rs | `getCDSMetrics` half dropped; expected GREEN |
| `test_jdk_internal_cds_all_methods_registered` | cds.rs | list edited (−2, +3); expected GREEN, would have gone RED unedited |
| `test_registration_count_at_least_20` | cds.rs | **replaced** by `f17_1_registration_count_is_exact` (`== 12`). The old `>=` would have gone RED at 12 — but the floor was never the point: a one-sided `>=` cannot notice a fabricated class being *added back*, which is the regression this file has already had once |
| `test_all_registered_methods_findable` | cds.rs | list rewritten to the 12 survivors **and made two-sided** (`expected.len() == r.len()`). `is_some()` over a hand-written list only ever proves that list is a *subset* of what is registered — structurally incapable of noticing an extra registration, which is exactly how eleven fabricated triples sat here being asserted-present |
| `test_is_dumping_class_list_returns_zero`, `test_vm_is_booted_returns_one` | cds.rs | **removed** with their natives |
| `test_is_sharing_enabled_returns_zero` | cds.rs | renamed `test_is_using_archive_returns_zero`; same body, expected GREEN |
| `f17_1_cds_config_status_is_zero`, `f17_1_needs_class_init_barrier_is_false` | cds.rs | new, expected GREEN |
| every test in `jdk25_concurrency.rs` and `phases_late/concurrent.rs` | — | **unchanged.** Both edits there are comments only |

Worth naming as its own finding: **four `cds.rs` tests and one
`shared_secrets_bridge.rs` test were pinning the fabrication, not merely tolerating
it.** A test that asserts `is_some()` on a fabricated triple makes fixing the bug
the only way to turn it red. That is the same failure mode as the tautological
guard in §3.3 arriving by a different route, and both live in files whose test
suites otherwise look thorough.

---

## 6. NOMINATIONS

These touch files this lane does not own. Exact literal text; nothing applied.

### N1 — `native-api/src/no_image_receiver.rs` + `native-builtins/tests/stub_ratchet.rs` + `native-builtins/src/shared_secrets_bridge.rs`, one atomic change

Retire `java/security/AccessController$1` entirely. It is a fabricated owner
class (`javap java.security.AccessController$1` → `class not found`) whose only
factory this lane already deleted. Requires, together:

1. `native-api/src/no_image_receiver.rs` — delete line 143:
   OLD: `    "java/security/AccessController$1",`
   NEW: *(line removed)*
2. `native-builtins/tests/stub_ratchet.rs:325` — update the assertion/comment
   that names it.
3. `native-builtins/src/shared_secrets_bridge.rs` — delete
   `register_java_security_access` and its two bodies, and its call at
   `register_wp1_4_shared_secrets`; drop the `java/security/AccessController$1`
   row from `representative_method_registered_per_owner`'s `expected` and change
   `assert_eq!(expected.len(), 15)` to `14`.

Do **not** do (1) without (3): `register()` would still mint natives on a name
`no_image_receiver` no longer tags.

### N2 — `vm/src/runtime/shared_secrets.rs`

`SharedSecretsInterface` is the canonical mapping this bridge mirrors, and it
still carries the two spellings F17-1 corrected. Three edits:

* line 131: OLD `            Self::JavaSecurity => "jdk/internal/access/JavaSecurityAccess",` — delete with the `JavaSecurity` variant, its `owner_class` arm (line 160), its `factory_method` arm (line 182), its doc comment, and its entry in `all()`.
* line 183: OLD `            Self::JavaUtilJar => "getJavaUtilJarAccess",`
  NEW: `            Self::JavaUtilJar => "javaUtilJarAccess",`

**Also worth a look while there:** the module doc claims *"a compile-time
`#[test]` in the vm crate asserts the two lists stay in sync."* They are already
out of sync and were before this lane touched anything — `SharedSecretsInterface::all()`
contains `JavaObjectInputStream` and not `JavaIOFileDescriptor`, while
`FACTORIES` had the opposite. Both were length 15, which is presumably how it
went unnoticed. Whatever that test checks, it is not membership.

### N3 — `jdk25_concurrency.rs` (this lane's file, but needs a build)

Delete the seven off-surface `StructuredTaskScope`/`$Subtask` triples of §4.2
together with `native_sts_join_until`, `native_subtask_task`, the three `<init>`
bodies, `native_sts_shutdown`, `native_sts_is_shutdown`, and the direct-call
tests `p82_join_until_past_deadline`,
`p82_join_until_past_deadline_honours_nanos`,
`p82_join_until_future_deadline_succeeds` and the `$Subtask.task` test. Needs one
`cargo test -p cratonvm-native-builtins` and one `--synthetic-jdk` gate run to be
checkable at all; the JDK-ONLY-NOTE now at the site is the handover.

---

## 7. Deliberately left undone

* **`jdk25_concurrency.rs` deletions** — §4.2. Documented and nominated instead.
* **`register_java_security_access`** — §3.1. Blocked on two files this lane does
  not own; nominated as N1.
* **`scripts/baselines/jdk-only-*.tsv`** — four rows now describe registrations
  that no longer exist (`jdk-only-dead-everywhere.tsv:192`,
  `jdk-only-gated-never-delete.tsv:160-161`,
  `jdk-only-kind-map-25-linux.tsv:9233-9234, 9489-9490`). These are **frozen
  measurements**, and this lane cannot re-measure. Editing them by hand would
  make a census claim about a tree nobody censused — the failure mode
  `jdk-only-kind-map-25-linux.tsv`'s own header warns about. They should be
  re-frozen from a run.
* **`scripts/jdk-baseline/generate.py`'s public-only filter** — §2.1 is a defect
  in the instrument, not just in this class's audit: it is structurally blind to
  the access level most JDK natives live at. Fixing it (a `--all-access` mode, or
  a second flags column) would re-baseline 20+ frozen TSVs and belongs to whoever
  owns that script. **This is the single highest-value follow-up in this record**
  — §2.3 found two missing real natives in the first class anybody looked at with
  `javap -p` instead.
