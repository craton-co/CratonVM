# F23-1 — the JDK surface oracle could not see a private native, and 27.7% of the surface was never in it

**2026-08-13, lane F23.** Fixes the instrument F8 built and F17 caught being
wrong, regenerates all 32 baselines, and re-runs both lanes' off-surface verdicts
against the corrected data.

**This lane may not build or run CratonVM, and did not.** No `cargo` command was
run. Every JDK fact below is `javap`/`java`/`jrt:` on this host (Microsoft build
**25.0.3+9-LTS**) and is quoted. `native-builtins/src/jdk_baseline.rs` was
parse-checked with `rustfmt --edition 2021 --emit stdout` on a scratch copy
(exit 0, zero `error:` lines), which rules out syntax errors and nothing else;
it was not type-checked. Every numeric assertion in it was **predicted** by
running a line-by-line Python transcription of `parse`/`audit`/
`audit_off_surface` against the regenerated baselines — 56 assertions, 0
disagreements — but a prediction is not a test run. Files I own were edited;
everything else is a NOMINATION with exact literal text.

---

## 0. Verdict, including on this lane's own brief

| claim | verdict |
|---|---|
| the baselines are blind to private and package-private members, and that is where most JDK natives live | **CONFIRMED, and larger than reported.** 528 of 1908 rows — **27.7%** — and **10 of the 13** `native` methods (§1, §2) |
| the filter is at `scripts/jdk-baseline/generate.py:175`, keeping members whose flags contain `public` | **WRONG ON BOTH COUNTS, and the second one matters.** `generate.py:175` is the known-answer test's *recount*, and it is correct there. The filter was `JdkBaseline.java:221`/`:237`, and it kept `ACC_PUBLIC` **or `ACC_PROTECTED`** (§1.1). The citation is repeated verbatim in three doc comments in files I do not own — NOM-1 |
| F17: `logLambdaFormInvoker(String)V` is real and F8's CDS count was 2, not 5 | **CONFIRMED. And F17 under-reported its own retraction: it was 2 of 10, and the three artifacts were `logLambdaFormInvoker`, `dumpClassList` AND `dumpDynamicArchive`** — all three `private,static,native` (§3.1) |
| expect wrongly deleted registrations in `cds.rs` / `shared_secrets_bridge.rs` | **NONE FOUND. Zero registrations were wrongly deleted by F17** (§3.2, §3.3). The near-miss was real and F17 caught it by hand with `javap -p`; the fix means the next lane does not have to |
| F8's other off-surface verdicts | **ALL SURVIVE** at any access level: `CDSMetrics`, `sun.misc.VM`, `ClassLoader.getCdsArchivePath`, `getJavaSecurityAccess`, `getJavaUtilJarAccess`, `$Config`, the four `StructuredTaskScope` items, `getConstantPool` (§3.4) |
| one verdict *reversed the other way* | **`ManagementFactoryHelper.<init>()V`** was off the version-1 surface and is a real `private` constructor (§3.5) |

---

## 1. The defect, measured

### 1.1 Where it actually was

`JdkBaseline.java`, version 1, in both the method and the field loop:

```java
int f = m.flags().flagsMask();
if ((f & (ACC_PUBLIC | ACC_PROTECTED)) == 0) {
    continue;
}
```

Two corrections to the received account. First, the filter was in the **Java
oracle**, not in `generate.py`; `generate.py:175`'s `"public" in r.split("\t")[3]`
is the KAT recounting `# public-methods` to compare against a `javap -public`
figure, which is exactly what it should count. Second, **`protected` was already
included**, so the blind spot is package-private + private, not
protected + package-private + private. That distinction changes nothing about the
severity and everything about where to look, and a lane that went to
`generate.py:175` and "fixed" it would have broken the KAT while leaving the
filter in place.

### 1.2 How much of the surface was outside the instrument

Regenerated all 32 baselines with the filter removed. **Zero rows were removed
anywhere** — the change is purely additive, and `git diff` over
`scripts/baselines/` contains no deleted line that is not a header:

```
$ git diff -U0 -- scripts/baselines/ | grep '^-' | grep -v '^---' | grep -v '^-# '
(nothing)
$ git diff -U0 -- scripts/baselines/ | grep '^-# ' | sed 's/\t.*//' | sort | uniq -c
     30 -# jdk-baseline      (the format version, 1 -> 2)
     19 -# rows              (11 of the 30 gained no rows; see below)
```

| | v1 | v2 | delta |
|---|---|---|---|
| rows across 32 baselines | 1380 | 1908 | **+528 (27.7% of the new total)** |
| member rows (METHOD+FIELD) | 779 | 1307 | +528 |
| non-public member rows | 10 | 538 | +528 |
| ...of which `protected` | 10 | 10 | 0 |
| **...of which private or package-private** | **0** | **528** | the whole population |
| **`native` methods visible** | **3** | **13** | +10 |

The `protected` row is why §1.1's correction matters, and it is a correction to
this record's own first draft as well: version 1 kept `ACC_PROTECTED`, so
"public-only" is not what it was, and the ten protected rows it emitted are
unchanged. What it hid was the **private and package-private** population, whose
v1 count is exactly **0** and whose v2 count is **528** — every added row.
"Non-public" is the wrong word for the blind spot and gives the wrong v1 baseline
(10, not 0); the floors in §2.4 are stated against 10 for that reason. The v1
column is `git show HEAD:` over the 30 tracked baselines; the two untracked ones
were written the same day by the same version-1 generator.

The last row is the one that indicts the instrument as an oracle *for this
tree*: these baselines exist to audit **native registrars**, and version 1 could
see three of the thirteen JDK natives declared by the classes it baselines.

### 1.3 Per class

`+meth`/`+fld` are rows that were invisible; `+nat` is the `native` subset of
`+meth`; `-rows` is rows lost, and it is zero everywhere.

| class | v1 rows | v2 rows | +meth | +fld | +nat | −rows |
|---|---|---|---|---|---|---|
| java.lang.Character | 176 | 188 | 8 | 4 | 0 | 0 |
| java.lang.management.ManagementFactory | 28 | 44 | 13 | 3 | 0 | 0 |
| java.lang.management.RuntimeMXBean | 22 | 22 | 0 | 0 | 0 | 0 |
| java.lang.module.ModuleDescriptor | 33 | 58 | 10 | 15 | 0 | 0 |
| **java.lang.Module** | 27 | 89 | 48 | 14 | **5** | 0 |
| java.lang.ModuleLayer | 17 | 38 | 13 | 8 | 0 | 0 |
| java.math.BigInteger | 71 | 213 | 103 | 39 | 0 | 0 |
| java.net.Socket | 60 | 94 | 17 | 17 | 0 | 0 |
| java.security.KeyStore | 34 | 45 | 3 | 8 | 0 | 0 |
| java.util.AbstractCollection | 21 | 22 | 1 | 0 | 0 | 0 |
| java.util.AbstractQueue | 14 | 14 | 0 | 0 | 0 | 0 |
| java.util.Base64$Decoder | 8 | 20 | 5 | 7 | 0 | 0 |
| java.util.Base64$Encoder | 9 | 25 | 5 | 11 | 0 | 0 |
| java.util.Base64 | 10 | 11 | 1 | 0 | 0 | 0 |
| java.util.concurrent.BlockingQueue | 18 | 18 | 0 | 0 | 0 | 0 |
| java.util.concurrent.Flow$Publisher | 4 | 4 | 0 | 0 | 0 | 0 |
| java.util.concurrent.StructuredTaskScope$Configuration | 6 | 6 | 0 | 0 | 0 | 0 |
| java.util.concurrent.StructuredTaskScope$Joiner | 11 | 11 | 0 | 0 | 0 | 0 |
| java.util.concurrent.StructuredTaskScope$Subtask | 8 | 8 | 0 | 0 | 0 | 0 |
| java.util.concurrent.StructuredTaskScope | 13 | 13 | 0 | 0 | 0 | 0 |
| java.util.concurrent.SubmissionPublisher | 27 | 42 | 4 | 11 | 0 | 0 |
| java.util.concurrent.SynchronousQueue | 37 | 46 | 3 | 6 | 0 | 0 |
| javax.crypto.Mac | 23 | 39 | 5 | 11 | 0 | 0 |
| javax.net.ssl.SSLParameters | 34 | 49 | 1 | 14 | 0 | 0 |
| javax.net.ssl.SSLSession | 24 | 24 | 0 | 0 | 0 | 0 |
| javax.net.ssl.SSLSocket | 36 | 36 | 0 | 0 | 0 | 0 |
| jdk.internal.access.SharedSecrets | 68 | 101 | 1 | 32 | 0 | 0 |
| **jdk.internal.misc.CDS** | 16 | 45 | 16 | 13 | **5** | 0 |
| jdk.internal.reflect.ReflectionFactory | 28 | 48 | 13 | 7 | 0 | 0 |
| module-java.base | 455 | 455 | 0 | 0 | 0 | 0 |
| sun.management.ManagementFactoryHelper | 25 | 59 | 11 | 23 | 0 | 0 |
| sun.reflect.ReflectionFactory | 17 | 21 | 2 | 2 | 0 | 0 |
| **TOTAL** | **1380** | **1908** | **+528** | | **+10** | **0** |

**The eleven files that gained nothing are the finding's own control.** Eight are
interfaces (`Flow$Publisher`, `BlockingQueue`, `RuntimeMXBean`, `SSLSession` and
the four `StructuredTaskScope` types) where every member is implicitly public;
`module-java.base` has no members at all; and `AbstractQueue` and `SSLSocket` are
abstract classes whose only non-public members are `protected` constructors,
which version 1 already emitted. Version 1 was accidentally complete on all
eleven.

This is why the anti-vacuity check is at corpus level and **not** per file: a
per-file "every baseline must contain a non-public member" assertion would have
been red on the nine with no non-public member at all, and I wrote it before
measuring and then had to delete it. `[reach≠defect]` in miniature — the
assertion would have been measuring which classes are interfaces.

### 1.4 The ten natives that were outside every guard

```
java.lang.Module   addExports0(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V   private,static,native
java.lang.Module   addExportsToAll0(Ljava/lang/Module;Ljava/lang/String;)V                private,static,native
java.lang.Module   addExportsToAllUnnamed0(Ljava/lang/Module;Ljava/lang/String;)V         private,static,native
java.lang.Module   addReads0(Ljava/lang/Module;Ljava/lang/Module;)V                       private,static,native
java.lang.Module   defineModule0(Ljava/lang/Module;ZLjava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)V  private,static,native
jdk.internal.misc.CDS  dumpClassList(Ljava/lang/String;)V             private,static,native
jdk.internal.misc.CDS  dumpDynamicArchive(Ljava/lang/String;)V        private,static,native
jdk.internal.misc.CDS  getCDSConfigStatus()I                          private,static,native
jdk.internal.misc.CDS  logLambdaFormInvoker(Ljava/lang/String;)V      private,static,native
jdk.internal.misc.CDS  needsClassInitBarrier0(Ljava/lang/Class;)Z     private,static,native
```

**Negative control, run because a fix should be checked for finding nothing as
well as something:** all five `java.lang.Module` natives *are* registered
(`jboss_jdkspecific.rs:2115/2132/2141/2165`, `lib.rs:12282`) and all five
descriptors match the baseline character for character. The widened oracle's
first new census over that class is clean. It could not have said so before.

---

## 2. The fix, and the decision the brief asked to be stated

### 2.1 Two questions, two populations

Widening the file is easy; widening every *check* built on it would have been the
real mistake, so the split is explicit and is enforced by different accessors:

| question | accessor | consumer |
|---|---|---|
| is this a real, dispatchable member? | `declares`, `declared_surface` | `audit` kind 4, `audit_off_surface` kind 5 |
| must every member of this have a triage row? | `public_surface` | `audit` kind 1 (`UNCOVERED`) |
| which members are natives? | `native_surface` (new) | opt-in, for registrar censuses |

* **`audit`'s `UNCOVERED` denominator was NOT widened**, deliberately. Widening it
  would demand a triage row for each of the 528 newly visible members before any
  converted guard could go green. The `UNCOVERED` count for the CDS re-enactment
  is 8 before and after; widening would have made it 23.
* **`audit_off_surface`'s reachability test WAS widened.** A native keyed on a
  private JDK method is dispatched by the JDK's own bytecode exactly like a
  public one — `CDS.<clinit>` calls `getCDSConfigStatus()I`, which is
  `private static native`. Testing that against `public_surface()` is what
  reported five real, reachable natives as fabrications.
* **The cost of not widening `UNCOVERED` is stated in the module doc, not left
  as silence:** `audit` still cannot report a *missing* private native. That is
  F17's false negative and it stays open. `native_surface()` is the surgical
  opt-in — 13 methods corpus-wide, not 528 — and converting a registrar guard
  onto it is NOM-4.

### 2.2 An ordering bug in `audit`, which was the false positive's actual mechanism

Kind 4 had three sub-cases and tested them in the wrong order: `descriptors_named(name).is_empty()`
ran **before** the exact-match test. So a member declared privately under a name
that also has a public overload got *"the name is right and the descriptor is
not"* — pointing confidently at the wrong overload. That is
`CDS.logLambdaFormInvoker` exactly: one `private` one-String native, one `public`
four-String wrapper. The exact-match test now runs first. Note that the ordering
bug was invisible under version 1 (the private row was not in the file to match)
and would have become a *new* wrong answer the moment the file widened — a fix
that only removed the filter would have kept producing the same false positive.

### 2.3 Format version 1 → 2

Bumped although no column changed. The row *grammar* is the same; the *population*
is not, and a version-1 parser reading a version-2 file answers `declares()` from
a wider set than it was written against without ever saying so. `parse` refuses
version 1.

### 2.4 The anti-vacuity floor, which is the part a widening breaks

`every_checked_in_baseline_is_wired_in_and_parses` asserted `total_rows > 1_000`
against a 1380-row corpus. At 1908 that floor is cleared by 900 and could not
notice a third of the corpus vanishing — the brief's point, and correct. Two
changes:

* the row floor is re-derived to `>= 1_800` and its message names the number a
  reverted filter produces (1380 — **red**);
* a new test, `the_widened_population_is_present_and_is_not_a_rounding_error`,
  counts the population version 1 could not emit. A version-1 corpus scores
  **10** non-public members — all of them `protected`, the private and
  package-private count being exactly **0** — and **3** natives, against floors
  of 500 and 13. Red by a factor of fifty if the filter comes back, however many
  rows the files have. A count whose floor is a fraction of a moving total is the
  `[gate=FR]` shape; a count whose floor is zero-under-the-defect is not.

Both KATs are now two-instrument as well: `generate.py` keeps its two
`javap -public` counts (17 / 96) and gains three `javap -p` counts
(Mac 22, Character 104, CDS 28), plus a "at least one counted row is neither
public nor protected" clause, because **a KAT taken with `javap -public` agrees
with a generator that drops every private member — and did, for 32 files.**

### 2.5 Mutation checks, both directions

* **known-present private member** — `a_known_private_member_is_visible_and_a_known_absent_one_is_not`
  asserts all five CDS `private,static,native` methods are found by `declares`
  and `declared_surface`, are **not** in `public_surface` (if they were, the two
  populations have been collapsed and `UNCOVERED` just grew by 538), and that
  `flags_of` returns `private,static,native` for each.
* **known-absent member** — the same test asserts `isDumpingClassList()Z` and
  `isSharingEnabled()Z` are absent at every access level, and
  `f23_1_every_cds_registration_is_on_the_jdk25_declared_surface` plants
  `isDumpingClassList` into the live registration list and requires
  `audit_off_surface` to fire on exactly it.
* **the instrument regressing** — `a_version_one_baseline_is_refused_in_both_of_its_shapes`
  builds a mutant that drops CDS's 16 non-public METHOD rows *and rewrites
  `# rows` to match*, i.e. what a reverted generator would actually emit. Its
  `# public-methods` header is correct in both files, which is the whole point:
  the only check that can fire is `# declared-methods`, and the test asserts that
  it is the one that does.

---

## 3. Re-running F8's and F17's verdicts

### 3.1 `jdk.internal.misc.CDS` — F8's five, re-audited: two survive

`kind_four_catches_five_of_the_ten_cds_registrations` is renamed
`kind_four_catches_two_of_the_ten_cds_registrations_not_five` and asserts the
corrected count. Same ten triples, same `audit`, version-2 baseline:

| F8's row | v1 verdict | v2 verdict, `javap -p` |
|---|---|---|
| `isDumpingClassList()Z` | STALE | **STALE — SURVIVES.** No such name at any access level |
| `isSharingEnabled()Z` | STALE | **STALE — SURVIVES.** `isUsingArchive()Z` is the JDK-true spelling |
| `logLambdaFormInvoker(Ljava/lang/String;)V` | STALE (descriptor) | **ARTIFACT.** `private,static,native` |
| `dumpClassList(Ljava/lang/String;)V` | STALE | **ARTIFACT.** `private,static,native` |
| `dumpDynamicArchive(Ljava/lang/String;)V` | STALE | **ARTIFACT.** `private,static,native` |

**F17 under-reported its own retraction.** F17-1 §2.2 names only
`logLambdaFormInvoker` as the false positive and says the other two were among
"F8's list of 5" without stating that they too are real natives. They are; the
`javap -p` transcript F17 quotes contains both lines. F17 kept the registrations,
so nothing was lost — but the *record* left two of the three retractions implicit,
and a reader taking F17's §2.2 at face value would still believe `dumpClassList`
and `dumpDynamicArchive` are fabrications that happen to have been spared.

### 3.2 `cds.rs` — were any registrations wrongly deleted? **No.**

Audited all eleven `jdk/internal/misc/CDS` registrations in the working tree
against the version-2 baseline: `audit_off_surface` returns **empty**. Under the
version-1 rule the same eleven produce **five** OFF-SURFACE reports. Both numbers
are pinned in `f23_1_every_cds_registration_is_on_the_jdk25_declared_surface`.

F17's three whole-class deletions do not depend on the filter at all and all
three re-verify:

```
$ javap -p sun.misc.VM                 -> Error: class not found
$ javap -p sun.management.CDSMetrics   -> Error: class not found
$ javap -p java.lang.ClassLoader | grep -i -e archive -e cds
  private void resetArchivedStates();          # no getCdsArchivePath, at any access level
$ grep -c getCDSMetrics scripts/baselines/jdk25-sun.management.ManagementFactoryHelper.tsv
0                                              # 32 methods now listed, not 22
```

The `ClassLoader` and `ManagementFactoryHelper` checks are the ones that needed
redoing, because those classes *are* in the image and a public-only search is
capable of missing a private member on them. Both were done with `javap -p` by
F17 and both hold against the full member table.

### 3.3 `shared_secrets_bridge.rs` — nothing wrongly deleted

Every member of `jdk.internal.access.SharedSecrets` is `public static`. The
widening added exactly **one method row and 32 field rows**, and
`public_surface()` is still 65. Re-audited:

* `getJavaSecurityAccess()Ljdk/internal/access/JavaSecurityAccess;` — absent at
  **every** access level. Deletion **SURVIVES**.
* `getJavaUtilJarAccess()Ljdk/internal/access/JavaUtilJarAccess;` — absent at
  every access level; `javaUtilJarAccess()…` is present and `public,static`.
  Correction **SURVIVES**.
* `kind_four_catches_two_of_the_fifteen_shared_secrets_factories` still reports
  exactly 2 STALE.

F17's own two replacement tests (`every_factory_is_declared_by_jdk25_shared_secrets`,
`jdk25_baseline_rejects_the_two_spellings_f17_1_removed`) are unaffected — they
call `declares`, whose answer can only widen, and both of the names they must
reject stay rejected.

### 3.4 The rest of F8's list

All re-audited against version 2; all survive, none was a filter artifact.

| verdict | still true at any access level? |
|---|---|
| `StructuredTaskScope$Config` is not a JDK type | yes — the generator still refuses to emit a baseline |
| the six `$Config` triples are STALE (3 by descriptor) | yes — 6 and 3, unchanged (that file gained 0 rows) |
| `StructuredTaskScope` outer: `isShutdown`, `shutdown`, `joinUntil`, `join()` return type | yes — 4 STALE, unchanged |
| the two `ReflectionFactory` types are different surfaces (25 vs 14 public) | yes — unchanged |
| `getConstantPool(Class)ConstantPool` is on neither | yes — absent at every access level on both |
| `java.base` has 58 unqualified exports, not 14 | yes — module baselines are untouched by this change |

### 3.5 The one verdict that reversed the *other* way

`cds.rs` registers `sun/management/ManagementFactoryHelper.<init>()V`. Version 1
listed **no** `<init>` for that class, so the registration was off the surface and
readable as a fabrication. Version 2 lists it:

```
METHOD	<init>	()V	private
```

`javap -p sun.management.ManagementFactoryHelper` → `private sun.management.ManagementFactoryHelper();`.
The `cds.rs` comment at the site already says "in the JDK this class is `final`
with a private constructor" and keeps the registration — a correct decision made
from source rather than from the oracle, which is what one has to do when the
oracle cannot see private members. `audit_off_surface` is now silent on it.

This is the general shape, not a one-off: **every synthetic-`<init>` exemption in
the tree written against a version-1 baseline is now potentially a kind-7b
`STALE OFF-SURFACE row (declared, not public)`.** Kind 7b exists to make each
one announce itself, and its message says which of the two things to delete —
the row, not the registration.

---

## 4. NOMINATIONS

Exact literal text. Nothing applied; none of these files is mine.

### NOM-1 — three doc comments cite a filter location that was never right and is now gone

**`native-builtins/src/cds.rs`, around line 730** (in `native_cds_get_config_status`'s doc):

OLD:
```
/// WHY IT WAS INVISIBLE. `scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv`
/// does not list it. That is not staleness — `generate.py:175` keeps only rows
/// whose flags contain `public`, so a baseline structurally cannot see a
/// `private static native`, which is the exact access level most JDK natives
/// live at. Auditing a native registrar against a public-only surface therefore
```
NEW:
```
/// WHY IT WAS INVISIBLE. `scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv`
/// did not list it until F23-1 (2026-08-13). That was not staleness — the
/// generator's oracle (`JdkBaseline.java:221`, format version 1) emitted a
/// member only if ACC_PUBLIC or ACC_PROTECTED was set, so a baseline
/// structurally could not see a `private static native`, which is the exact
/// access level most JDK natives live at. It lists this method now, as
/// `private,static,native`. Auditing a native registrar against a public-only
/// surface therefore
```

**`native-builtins/src/cds.rs`, around line 1307** (in `test_jdk_internal_cds_all_methods_registered`):

OLD:
```
        // F17-1 (2026-08-13): every entry below is checked against `javap -p
        // jdk.internal.misc.CDS`, i.e. ALL access levels — not against
        // `scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv`, which
        // `generate.py:175` filters to `public` and which therefore lists none
        // of the natives this registrar exists to supply. `isDumpingClassList`
```
NEW:
```
        // F17-1 (2026-08-13): every entry below is checked against `javap -p
        // jdk.internal.misc.CDS`, i.e. ALL access levels. F23-1 made
        // `scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv` agree: it now
        // lists all 28 members including the five `private,static,native` ones,
        // and `jdk_baseline::f23_1_every_cds_registration_is_on_the_jdk25_\
        // declared_surface` audits this exact list against it. `isDumpingClassList`
```

**`native-builtins/src/shared_secrets_bridge.rs`, around line 2948**:

OLD:
```
    /// SOUNDNESS OF THIS PARTICULAR ORACLE, because it is not sound everywhere:
    /// the generator keeps only rows whose flags contain `public`
    /// (`generate.py:175`), so a baseline is blind to package-private, private
    /// and `private static native` members. That blindness is why the same
    /// baseline mis-reports `jdk.internal.misc.CDS.logLambdaFormInvoker` (see
    /// `cds.rs`). It does not bite here: every member of `SharedSecrets` is
    /// `public static`, so its 65 baseline rows are its whole surface, and
    /// `javap jdk.internal.access.SharedSecrets` returns the same set.
```
NEW:
```
    /// SOUNDNESS OF THIS PARTICULAR ORACLE. Until F23-1 (2026-08-13) the
    /// baselines carried only public and protected members, so they were blind
    /// to the access level most JDK natives live at — which is why the same
    /// baseline mis-reported `jdk.internal.misc.CDS.logLambdaFormInvoker` (see
    /// `cds.rs`). It never bit here: every member of `SharedSecrets` is
    /// `public static`, so `javap` and `javap -p` return the same set and this
    /// guard's answer is unchanged by the widening. The baseline is now the
    /// whole class — 65 public methods+ctor, plus 32 private static fields —
    /// and `declares()` means what it says.
```

### NOM-2 — `docs/known-issues/jdk-only/E41-R11-TWELVE-GUARDS-CONVERTED-20260813.md:185`

OLD: `kind_four_catches_five_of_the_ten_cds_registrations`.
NEW: `kind_four_catches_two_of_the_ten_cds_registrations_not_five` (renamed by
F23-1; the "five" was three real natives plus two real fabrications — see
F23-1 §3.1).

### NOM-3 — `docs/known-issues/jdk-only/F17-1-cds-sharedsecrets-fabrications-20260813.md` §2.2

F17-1 names only `logLambdaFormInvoker` as the false positive. Add
`dumpClassList(Ljava/lang/String;)V` and `dumpDynamicArchive(Ljava/lang/String;)V`
to the same paragraph: both are `private,static,native` in JDK 25 and both were on
F8's five, so the retraction is three registrations, not one. F17 kept all three,
so this is a record correction with no code consequence — but it is the difference
between "one instrument bug produced one wrong item" and "three of five".

Also §7's last bullet ("**`scripts/jdk-baseline/generate.py`'s public-only
filter** … belongs to whoever owns that script") can be marked **DONE — F23-1**,
with the note that the filter was in `JdkBaseline.java`, not `generate.py`.

### NOM-4 — convert a registrar guard onto `native_surface()`

`audit` still cannot report a *missing* private native (§2.1), which is F17's
false negative and the reason `getCDSConfigStatus` sat unregistered. The data now
exists: `Baseline::native_surface()` is 8 methods on `CDS` and 5 on
`java.lang.Module`, 13 corpus-wide. A two-way census of a registrar against that
list is cheap and would have found `getCDSConfigStatus` and
`needsClassInitBarrier0` without anyone running `javap -p` by hand. Needs a
`cargo test -p cratonvm-native-builtins` this lane may not run.

---

## 5. Deliberately left undone

* **Nothing was built, type-checked or executed.** `rustfmt` on a copy is a parse
  check, not a compile. The 56 numeric assertions in `jdk_baseline.rs` were
  predicted by a Python transcription of the same logic run against the real
  files; a transcription can be faithful and still not be the program.
  `[proxy oracle]` — validate it as a proxy before quoting the zero.
* **The 528 newly visible members are not audited against anything.** This lane
  made them *visible*; no guard yet reads them except the CDS re-enactments. The
  natives among them are the population worth censusing first (NOM-4).
* **`scripts/baselines/jdk-only-*.tsv`** — the frozen run-measured censuses. F17
  listed four rows there that describe registrations that no longer exist; this
  lane cannot re-measure them either, and hand-editing a census is the failure
  mode those files' own headers warn about. Still open.
* **The `# declared-fields` header has no consumer.** It is cross-checked by
  `parse` and nothing else reads field rows. 245 of the 528 new rows are fields (283 are methods).
* **No other lane's off-surface list was re-run** beyond F8's and F17's. Any
  guard elsewhere in the crate that concluded "the JDK does not declare this"
  from a version-1 baseline is subject to the same reversal, and the tell is a
  registration keyed on a name ending in `0` or one that `javap` (no `-p`) does
  not print.
