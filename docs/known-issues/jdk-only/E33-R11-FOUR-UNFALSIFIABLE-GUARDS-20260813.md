# E33 / R11 — the four guards that could not go red, repaired and mutation-checked

**Date:** 2026-08-13 **Lane:** E33
**Closes:** rows 1, 32, 33 and 34 of
`docs/known-issues/jdk-only/E25-R11-GUARD-POPULATION-SWEEP-20260813.md` §3–§4 —
the four guards E25 found that **cannot go red for the property they are named
for, under any input**. Also closes NOM E25-4 and NOM E25-10.
**Status:** FIXED-UNVERIFIED-BY-CARGO. Every repair is mutation-checked by
executable transcript (§6), but **this lane did not run `cargo` and did not run
the CratonVM binary** — a release build was compiling throughout. All edits are
confined to `#[cfg(test)]` modules and `tests/` targets; §7 nominates the three
things that need a non-test change.

> **VERIFIED AGAINST A BINARY 2026-09-02.** The status says "this lane did not
> run `cargo`". It has now been run, on a build from this tree, and **all four
> guards were run BY NAME** so that a rename or deletion could not hide behind a
> green file:
>
> ```text
> 1  graalvm_compat::tests::test_register_total_method_count            1 passed, 4215 filtered
> 2  vm/tests/jck_conformance.rs                                        3 passed
> 3  wp7_2 ::each_jdbc_core_type_has_registered_natives                 1 passed,    9 filtered
> 4  opcorpus::tests::there_is_an_entry_for_every_named_opcode_...      1 passed,  181 filtered
> ```
>
> **Guard 2 is the one that proves the repair.** §"the committed baseline
> document" records that `cargo test -p cratonvm-vm --test jck_conformance
> -- --nocapture` **"ran zero tests, and had since the `#![cfg]` was added"** —
> a guard that was green because it executed nothing. Today the same command on
> default features runs **3**. That is the defect this record is named for, and
> it is visible only in the COUNT: the `ok` looked identical before and after.
>
> **A red in guard 3's file is NOT this record's.** `cargo test -p cratonvm-vm
> --test wp7_2_jdbc_core_types_reachable` fails today — but on
> `connection_methods_carry_signatures`, which returns the "probe did not run"
> sentinel `-100` and drives `Connection.class.getDeclaredMethods()` through
> `synthetic_jdk_method_decls`. Guard 3 is
> `each_jdbc_core_type_has_registered_natives`, a different test, and it passes.
> Judging this record by the FILE would have held it open for a defect that is
> not its subject; that regression is tracked separately and is not this lane's.
>
> Both failure modes in one record, which is worth stating plainly: a green that
> meant nothing because the binary ran no tests, and a red that meant something
> else because it was a different test in the same file. Neither is visible from
> a pass/fail line alone.
>
> Unchanged: §6's mutation transcripts remain executable-transcript evidence, not
> `cargo` runs of the mutants. This note says the four guards run and pass today;
> it does not re-run the nine mutations.

**Prov:** GraalVM SDK 25.0.2 and the JDK 25 opcode tables were **measured on
this host today** (§2.1, §5.1). Everything else is source read in this working
tree. Rust behaviour is asserted by standalone `rustc` harnesses that lift the
new assertion bodies verbatim, not by `cargo test`.

| # | guard | what it asserted before | what makes it red now |
|---|---|---|---|
| 1 | `native-builtins/src/graalvm_compat.rs::test_register_total_method_count` | the absence of a method named `nonExistent` that nothing has ever registered | deleting, adding or re-descriptoring **any** of the 15 registrations in `register_graalvm_compat_natives` |
| 2 | `vm/tests/jck_conformance.rs` | nothing, in any executing configuration | adding, removing or duplicating a `CORPUS` row; editing `BASELINE_FLOORS`; editing the committed baseline document; and — with `CRATONVM_REQUIRE_E2E=1` — running the gate with no compiled corpus |
| 3 | `vm/tests/wp7_2_jdbc_core_types_reachable.rs::each_jdbc_core_type_has_registered_natives` | nothing, in any executing configuration | dropping any of the five JDBC anchors `register_p68_jdbc` puts on the real-JDK path, **or** pulling `DriverManager.registerDriver` onto it |
| 4 | `difftest/src/opcorpus.rs::there_is_an_entry_for_every_named_opcode_and_nothing_else` | that `matrix::opcode_name` equals itself | any disagreement between `matrix::NAMES` and the JDK 25 opcode table — a misspelling, a swap, a deletion |

**None of the four was deleted.** Each had a real property behind it; in three
cases the property was not the one the body was checking, and in the fourth the
body checked the property against itself.

---

## 1. Guard 1 — `test_register_total_method_count`

### 1.1 What it was

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

The name promises a count. The comment computes one. The body asserts that a
method named `nonExistent` is absent — and nothing has ever registered a method
by that name, so the assertion is true of every possible state of the registrar.
**Deleting all 15 `r.register(...)` calls leaves it green** (transcript, §6.1
mutation C).

The arithmetic was wrong as well. `register_graalvm_compat_natives` makes
**15** registrations, not 10: the tally omitted the three `ImageSingletons`
methods, the `hosted/Feature` row and the CratonVM `MetadataAgent` extension.

### 1.2 What "10 methods" was for

The property is *the registrar registers exactly this surface* — the
`org.graalvm.nativeimage` compatibility stubs an application compiled for
Substrate VM will call. Two failure modes: a registration silently lost, and a
registration silently added.

### 1.3 What it is now

A **two-sided frozen set** — the shape this tree already blesses for exactly
this failure (`vm/tests/wp8_10_9_string_contains_native.rs:266`
`the_surviving_string_registration_set_is_exactly_this`, written because "both
passed while the drop was silently deleting four registrations nobody had
thought to name"). Every row must be registered; every registration must be a
row. A changed descriptor trips both halves.

Plus two anchors that are **not** transcribed from the code under test:

* `no_graalvm_substrate_stub_reaches_the_essential_path` reads a registry built
  by `register_essential_natives` — a registrar this module does not call — and
  asserts none of the 15 triples is on it. `register_graalvm_compat_natives` has
  exactly one call site (`lib.rs`, inside `register_synthetic_overrides`), and
  `ImageInfo.inImageCode()` answering on a real JDK would tell an application it
  is inside a native image when it is not. That is the `register_p68_xml`
  failure mode with a different class name. It carries an anti-vacuity floor
  (`essential.len() > 100`).
* an `Sdk` verdict column, measured against the real GraalVM SDK — §2.

## 2. The GraalVM SDK measurement, and the five divergences it found

### 2.1 Provenance

A GraalVM SDK **is** present on this host, contrary to this lane's first
assumption:

```
$ ls /c/craton/tornadovm/tornadovm-4.0.1-jdk25-ptx/share/java/tornado/
nativeimage-25.0.2.jar   graal-sdk-25.0.2.jar   word-25.0.2.jar   ...

$ javap -public -s -cp nativeimage-25.0.2.jar org.graalvm.nativeimage.ImageInfo
$ javap -public -s -cp nativeimage-25.0.2.jar org.graalvm.nativeimage.ImageSingletons
$ javap -public -s -cp nativeimage-25.0.2.jar org.graalvm.nativeimage.Platform
$ javap -public -s -cp nativeimage-25.0.2.jar org.graalvm.nativeimage.hosted.Feature
$ javap -public -s -cp nativeimage-25.0.2.jar org.graalvm.nativeimage.hosted.RuntimeReflection
$ javap -public -s -cp nativeimage-25.0.2.jar org.graalvm.nativeimage.hosted.RuntimeSerialization
$ javap -public -s -cp nativeimage-25.0.2.jar org.graalvm.nativeimage.hosted.RuntimeJNIAccess
```

`javap` from `openjdk 25.0.3 2026-04-21 LTS, Microsoft-13877124, build
25.0.3+9-LTS`, taken 2026-08-13.

### 2.2 Result — 9 of 15 exact, 5 wrong, 1 extension

| CratonVM registration | GraalVM SDK 25.0.2 |
|---|---|
| `org/graalvm/nativeimage/ImageInfo` — `inImageCode`, `inImageBuildtimeCode`, `inImageRuntimeCode`, `isExecutable`, `isSharedLibrary`, all `()Z` | exact, all five |
| `org/graalvm/nativeimage/ImageSingletons` — `contains(Ljava/lang/Class;)Z`, `lookup(Ljava/lang/Class;)Ljava/lang/Object;`, `add(Ljava/lang/Class;Ljava/lang/Object;)V` | exact, all three |
| `org/graalvm/nativeimage/Platform.includedIn(Ljava/lang/Class;)Z` | exact |
| `org/graalvm/nativeimage/RuntimeReflection.register(Ljava/lang/Class;)V` | **no such class.** The SDK class is `org/graalvm/nativeimage/**hosted**/RuntimeReflection`, and the method is **varargs**: `register(Class<?>...)` → `([Ljava/lang/Class;)V` |
| `.../RuntimeReflection.registerForReflectiveInstantiation(Ljava/lang/Class;)V` | same: `hosted`, and `([Ljava/lang/Class;)V` |
| `org/graalvm/nativeimage/RuntimeSerialization.register(Ljava/lang/Class;)V` | same: `.../hosted/RuntimeSerialization`, `([Ljava/lang/Class;)V` |
| `org/graalvm/nativeimage/RuntimeJNIAccess.register(Ljava/lang/Class;)V` | same: `.../hosted/RuntimeJNIAccess`, `([Ljava/lang/Class;)V` |
| `org/graalvm/nativeimage/hosted/Feature.register(Ljava/lang/Class;)V` | **fabricated.** `Feature` is an interface of 17 `default` lifecycle methods (`beforeAnalysis`, `duringSetup`, …) and declares no `register` of any shape |
| `cratonvm/graalvm/MetadataAgent.dumpConfigs(Ljava/lang/String;)I` | CratonVM extension, correctly absent |

Four of the five are the same mistake twice over — the wrong package **and** the
scalar form of a varargs method — so no application calling
`RuntimeReflection.register(Foo.class)` reaches any of these shims. Fixing them
is a registrar change, i.e. non-test code: **NOM E33-1**.

The guard records them as a named, ratcheted, **expiring** exemption
(`MEASURED_SDK_DIVERGENCES`) rather than asserting them away. It fails three
ways: a sixth divergence appears; a divergence disappears and its row is not
reclassified `Sdk::Exact`; or a `Divergent` row's registration comes to equal
exactly what the SDK declares, which means the code was fixed and the row was
not. That last arm is the one that stops an exemption from becoming standing
permission — E20 found 3 of 6 `already_triaged` rows had already rotted that way.

---

## 3. Guard 2 — `vm/tests/jck_conformance.rs`

### 3.1 Two dark layers, and a third thing nobody had checked

Line 4 was `#![cfg(feature = "synthetic-jdk")]`. **No CI job executes
`vm/tests/*` under that feature.** Read `.github/workflows/ci.yml`:

* the `synthetic-jdk` job runs `cargo check --all-targets --features
  synthetic-jdk` (compile only for integration targets) and then
  `cargo test -p cratonvm-native-builtins --lib` and
  `cargo test -p cratonvm-vm --lib --features synthetic-jdk` — **`--lib` scopes
  only**;
* the blocking `cargo test --workspace` job (line 173) uses default features,
  where this file compiled to an empty crate.

So a test named `jck_regression_gate`, whose own failure text says it "enforces
the committed baseline", existed in no executing configuration. Behind that, it
opened with `if !class_files_available() { eprintln!("Skipping…"); return; }`,
which in cargo's output is `test jck_regression_gate ... ok` in 0.00 s.

The committed baseline document itself records the third layer without knowing
it. `internal/gaps/jdk-regression-baseline.md` §"How to run" says:

```bash
# Default features (honest baseline — no synthetic JDK stubs):
cargo test -p cratonvm-vm --test jck_conformance -- --nocapture
```

That command ran **zero tests**, and had since the `#![cfg]` was added.

### 3.2 Split by what it needs, not by cargo feature

* `CORPUS` and `BASELINE_FLOORS` need no VM and no `.class` files. The checks
  over them are **ungated** and run in `cargo test --workspace`.
* The corpus run needs a synthetic-JDK VM and a `javac` corpus. It stays behind
  `#[cfg(feature = "synthetic-jdk")]` — the original comment's reason is a good
  one — applied **per item** rather than as a file-level `#![cfg]`, so removing
  the darkness did not cost the separation.

### 3.3 The four things that can now fail

1. `every_category_population_matches_the_corpus` — `BASELINE_FLOORS` grew a
   third column, the category's corpus population. It was a trailing `// 4/5`
   comment before, which asserted nothing and had already rotted. Two-sided on
   the category set as well: a `CORPUS` category with no floor row is a
   population nothing floors, and a floor row with no corpus tests is a
   permanent no-op (`tallies.get(cat).unwrap_or_default()`). Anti-vacuity floor:
   `CORPUS.len() > 400`.
2. `the_corpus_lists_no_test_twice` — a duplicate inflates a category's
   population and its pass count together, so the floor still passes while the
   corpus covers less than it claims.
3. `the_committed_baseline_document_and_this_table_agree` — the one check whose
   expectation is a file **outside this crate** that a person edits by hand. It
   parses the markdown table and compares Floor and Total per category, plus the
   TOTAL row against `CORPUS.len()`.
4. the skip is now loud, and `CRATONVM_REQUIRE_E2E` (this suite's existing
   switch, `vm/tests/common/mod.rs::REQUIRE_VAR`) promotes it to a panic. The
   spelling and the `0`/empty-means-unset semantics were read out of that file,
   not assumed.

### 3.4 What the document check found on its first run — four live drifts

The document was last updated 2026-04-16. Since then:

| | this file | the document | why |
|---|---|---|---|
| `Io` floor | 18 | 20 | lowered here with the reason in a trailing comment only ("slight variance") |
| `Lang` total | 110 | 109 | the corpus grew by 1 |
| `Util` total | 45 | 43 | the corpus grew by 2 |
| corpus size | 424 | 421 | the same +3 |

Fixing them means editing a document this lane does not own (**NOM E33-3**), so
they are recorded in `BASELINE_DOC_DRIFT` — a four-row exemption in which every
row states **both** numbers. A row dies the moment the disagreement stops being
exactly what it says: if the code is fixed, if the document is fixed, or if the
disagreement changes shape. Both directions are in the transcript (§6.3
mutations C and D).

### 3.5 The baseline document may be missing, and that is a failure

`baseline_document()` searches the internal tree's `gaps/` directory first and
the repo-root `gaps/` second, and **panics** if neither exists, naming both
paths. (It is the one place in the tree that still spells the internal prefix in
full, because it is a filesystem probe rather than a citation — see the
`NOT_A_CITATION` row in `types/tests/doc_citation_paths.rs`.) `docs/internal` is being removed from
history by a separate effort; if it goes, this fails, and that is the correct
signal — a regression gate whose committed baseline was deleted is not a gate.
The panic text says exactly that and tells the reader what to do. **NOM E33-4**
flags it for whoever performs the removal.

---

## 4. Guard 3 — `each_jdbc_core_type_has_registered_natives`

### 4.1 The `#[cfg]` was dark *and* stale

Same darkness as §3.1. But its stated reason was also false in this tree. The
comment inside the test read:

> The registry-only `register_essential_natives` does not include phase68.

`native-builtins/src/lib.rs`, inside `register_essential_natives_with_shims`,
calls `crate::phases_late::jdbc::register_p68_jdbc(registry)` **directly**, with
a comment explaining why: the registrations are on `java/sql/*` INTERFACES,
which do not intercept a driver's implementation class. Five of the six WP7.2
anchors are therefore live on the real-JDK path — the path an application with a
real JDBC driver actually takes — and the test that was supposed to prove them
was compiled out of it.

The sixth is different, and the difference is the guard. `DriverManager` is a
CONCRETE class, so a native on it intercepts; `register_p68_jdbc_driver_manager`
was split out of `register_p68_jdbc` for exactly that reason, and its own doc
records what happened when it was not: `DriverManager.getConnection(url)` handed
back a rusqlite connection for every URL, shadowing whatever driver the
application had registered.

### 4.2 Split by registrar

`ANCHOR_NATIVES` grew a `JdbcPath` column — `EssentialAndSynthetic` (5 rows) or
`SyntheticOnly` (1 row). The ungated test asserts the five are **present** on the
real-JDK registry and the one is **absent** from it, with an anti-vacuity floor.
A gated companion asserts all six under `--features synthetic-jdk`, where
`java.sql.*` has no bytecode at all. Lookups use `kind_of` (exact triple) rather
than `find`, which also matches through the registry's descriptor-compatibility
rewriting and would let a near-miss answer for a row that is really gone.

The two sibling guards in the same file, `each_jdbc_core_type_has_multiple_anchor_natives`
(16 anchors) and `statement_subtype_alias_chain_resolves`, carried the same dark
`#[cfg]` and were un-gated the same way. All 16 wide anchors were verified
present in `register_p68_jdbc` by enumerating its 111 `r.register(` calls, and
`alias_class` physically copies registrations at call time
(`native-api/src/registry.rs:7517`), so the `Statement` →
`PreparedStatement` → `CallableStatement` chain exists on whichever path called
the registrar.

**The remaining residual is stated, not hidden:** the `…_under_synthetic_jdk`
companion is still compiled only by `cargo check`. That is **NOM E33-2**, and it
is a CI-configuration change, not a test change.

---

## 5. Guard 4 — `there_is_an_entry_for_every_named_opcode_and_nothing_else`

`generate_all` is `(0..=0xc9).filter_map(matrix::opcode_name)`. The test then
asserted `p.mnemonic == matrix::opcode_name(p.opcode)`. The corpus and the
assertion are the same expression evaluated twice: a misspelling, a swapped
`dup_x2`/`dup2_x1`, or any wrong opcode-to-name mapping is copied into the
corpus and then agreed with. The `len() == 202` half was already a compile-time
fact of `const NAMES: [&str; 202]`.

### 5.1 The external oracle — two of them, cross-checked

1. **`jdk.hotspot.agent/sun/jvm/hotspot/interpreter/Bytecodes.java`** from
   `C:\craton\jdk25src` — every `def(_x, "x", …)` row joined to its
   `public static final int _x = N;`. **202 rows** with value ≤ `0xc9`, in
   lowercase JVMS spelling.
2. **`java.lang.classfile.Opcode`** (JEP 484), enumerated by running
   `Opcode.values()` on `openjdk 25.0.3 … build 25.0.3+9-LTS` on this host,
   skipping `isWide()` pseudo-opcodes, keyed by `bytecode()`, lowercased.
   **201 rows.**

The two agree on all 201 they share. Source 2 has no `0xc4 wide` because the
ClassFile API models `wide` as a modifier on the widened opcode rather than an
opcode of its own; source 1 has it, JVMS §6.5 has it, and this crate's decoder
needs it, so the frozen table keeps it. Both agree with `matrix::NAMES` exactly
as it stands today — the table was correct; the guard was not.

Source 1's join is now `JVMS_MNEMONICS`, a `#[rustfmt::skip]` const in the test
module with the commands and JDK build string in its doc comment. Two tests read
it: a new `the_matrix_opcode_table_is_the_jvms_opcode_table` compares the whole
`u8` domain (so a name appearing where the JVMS has a reserved opcode is caught
too), and the repaired corpus test takes both its expected population and each
entry's expected mnemonic from it.

---

## 6. Mutation transcripts

No `cargo` was run. Each transcript is a standalone `rustc` build of a harness
that **lifts the new assertion bodies (or their exact logic) out of the working
tree** and feeds them tables parsed out of the real source files. Mutations are
applied to copies of the real sources, never to the tree.
Scripts: `…/scratchpad/e33/gen_{graal,op,wp72,jck}_harness.py`.

### 6.1 Guard 1 — `graalvm_compat.rs`

Pristine:

```
$ python gen_graal_harness.py native-builtins/src/graalvm_compat.rs
frozen rows: 15  registrations: 15  measured divergences: 5
$ ./gh_pristine.exe
PASS: 15 rows, two-sided + 5 measured SDK divergences
```

**A — delete one registration** (`ImageSingletons.lookup`):

```
register_graalvm_compat_natives no longer matches its frozen set.
  REGISTRATION LOST: ["org/graalvm/nativeimage/ImageSingletons::lookup(Ljava/lang/Class;)Ljava/lang/Object;"]
  UNDECLARED REGISTRATION: []
```

**B — add an undeclared registration** (`ImageInfo.inImageCodeNEW()Z`):

```
  REGISTRATION LOST: []
  UNDECLARED REGISTRATION: ["org/graalvm/nativeimage/ImageInfo::inImageCodeNEW()Z"]
```

**C — delete EVERY registration in the module.** This is the case the old body
could not see:

```
  REGISTRATION LOST: ["cratonvm/graalvm/MetadataAgent::dumpConfigs(Ljava/lang/String;)I",
   "org/graalvm/nativeimage/ImageInfo::inImageBuildtimeCode()Z", … 15 rows …]
  UNDECLARED REGISTRATION: []
```

**D — half-fix the `RuntimeSerialization` class** (package corrected, descriptor
still scalar), leaving the row marked `Divergent`:

```
assertion `left == right` failed: the set of registrations that disagree with GraalVM SDK 25.0.2 changed
  left:  [… "org/graalvm/nativeimage/hosted/RuntimeSerialization::register(Ljava/lang/Class;)V"]
  right: [… "org/graalvm/nativeimage/RuntimeSerialization::register(Ljava/lang/Class;)V"]
```

**E — fully fix `RuntimeJNIAccess` to the SDK triple** and update the frozen row
but not its verdict — the exemption-expiry arm:

```
rows marked Sdk::Divergent that now register exactly what the SDK declares:
    org/graalvm/nativeimage/hosted/RuntimeJNIAccess::register([Ljava/lang/Class;)V
```

### 6.2 Guard 4 — `opcorpus.rs`

Pristine: `PASS: 202 opcodes agree with the external JDK 25 table`.

**A — misspell one mnemonic** in `matrix::NAMES` (`invokedynamic` →
`invokedyanmic`), the exact example E25 named:

```
matrix::opcode_name disagrees with the JDK 25 opcode table:
  0xba: matrix=Some("invokedyanmic") JVMS=Some("invokedynamic")
```

**B — swap `dup_x2` and `dup2_x1`**, the other example E25 named:

```
  0x5b: matrix=Some("dup2_x1") JVMS=Some("dup_x2")
  0x5d: matrix=Some("dup_x2") JVMS=Some("dup2_x1")
```

**C — delete `wide` from the table** (and shrink the array length):

```
  0xc4: matrix=Some("multianewarray") JVMS=Some("wide")
  0xc5: matrix=Some("ifnull")         JVMS=Some("multianewarray")
  …
  0xc9: matrix=None                   JVMS=Some("jsr_w")
```

### 6.3 Guard 2 — `jck_conformance.rs`

The harness lifts `baseline_rows` and all three new test bodies **verbatim** and
reads the real committed markdown.

```
$ ./jck_pristine.exe
PASS: 424 corpus entries, 19 floor rows, 4 drift rows
```

**A — a 425th corpus entry in `Lang`:**

```
BASELINE_FLOORS and CORPUS have drifted apart:
  Lang: BASELINE_FLOORS says 110 tests, CORPUS has 111
```

and, in the document check:

```
this file and the committed baseline document disagree, and the disagreement is not in BASELINE_DOC_DRIFT:
  TOTAL.total: this file says 425, …/jdk-regression-baseline.md says 421
```

**B — duplicate an existing corpus entry:**

```
CORPUS lists the same (class, method) more than once. …
  left: 424
 right: 425
```

**C — someone raises the `Io` floor to the document's 20 and forgets the drift
row** (exemption expiry, code side):

```
BASELINE_DOC_DRIFT has rows that no longer describe a real disagreement:
  Io.floor: BASELINE_DOC_DRIFT still excuses "18 here vs 20 in the document", but that is no longer the disagreement
```

**D — the *document* is corrected (`Lang` total 109 → 110) and the drift row is
not** (exemption expiry, document side):

```
  Lang.total: BASELINE_DOC_DRIFT still excuses "110 here vs 109 in the document", but that is no longer the disagreement
```

**E — drop the `Sql` row from `BASELINE_FLOORS`:**

```
  Sql: CORPUS has tests in this category and BASELINE_FLOORS has no row, so its pass count is floored by nothing
```

**F — the committed baseline document is missing** (`baseline_document()` lifted
verbatim, compiled against a tree with no `docs/internal`):

```
the committed JCK baseline document was not found. Searched:
  …/vm/../…/gaps/jdk-regression-baseline.md      <- the internal tree
  …/vm/../gaps/jdk-regression-baseline.md
```

### 6.4 Guard 3 — `wp7_2_jdbc_core_types_reachable.rs`

`ESSENTIAL` is `register_p68_jdbc`'s registrations plus its three `alias_class`
expansions, parsed from `native-builtins/src/phases_late/jdbc.rs`.

```
$ ./wp72_pristine.exe
PASS: 151 essential registrations, 6 anchors, two-sided
```

**A — drop `Connection.createStatement` from `register_p68_jdbc`:**

```
WP7.2 acceptance: missing:
  java.sql.Connection -> java/sql/Connection::createStatement()Ljava/sql/Statement;
```

**B — pull the `DriverManager` natives onto the essential path** (the exact
regression `register_p68_jdbc_driver_manager` was split out to prevent):

```
A synthetic-only JDBC native reached the real-JDK path:
  java.sql.Driver -> java/sql/DriverManager::registerDriver(Ljava/sql/Driver;)V
```

---

## 7. NOMINATIONS

### NOM E33-1 — `native-builtins/src/graalvm_compat.rs::register_graalvm_compat_natives` — five registrations no GraalVM application can reach

Non-test change. §2.2 has the measurement. Four rows are registered on classes
that do not exist in the GraalVM SDK (`org.graalvm.nativeimage.RuntimeReflection`
and friends live in `org.graalvm.nativeimage.hosted`) **and** with the scalar
descriptor of a varargs method. One row, `hosted/Feature.register`, is
fabricated outright.

OLD (four sites; `RuntimeReflection.register` shown):

```rust
    r.register(
        "org/graalvm/nativeimage/RuntimeReflection",
        "register",
        "(Ljava/lang/Class;)V",
        graalvm_runtime_reflection_register,
    );
```

NEW:

```rust
    r.register(
        "org/graalvm/nativeimage/hosted/RuntimeReflection",
        "register",
        "([Ljava/lang/Class;)V",
        graalvm_runtime_reflection_register,
    );
```

with the same `hosted/` + `([Ljava/lang/Class;)V` correction for
`registerForReflectiveInstantiation`, `RuntimeSerialization.register` and
`RuntimeJNIAccess.register`. **The callback bodies must change too** — the
argument is now a `Class[]`, not a `Class`, so each shim has to iterate. That is
why this is nominated rather than done: it is a behaviour change, not a rename.

`hosted/Feature.register` should be **deleted**, not moved: `Feature` declares
no such method. Whatever called it was calling something GraalVM does not have.

Whoever lands it must, in the same edit, update
`GRAALVM_COMPAT_REGISTRATIONS`'s rows and delete the corresponding entries from
`MEASURED_SDK_DIVERGENCES` — `the_graalvm_sdk_divergences_are_the_five_that_were_measured`
fails loudly if they do only one of the two (§6.1 mutations D and E).

**Follow-up, cheap:** `scripts/jdk-baseline/generate.py` (added by E32/E37) now
writes `scripts/baselines/jdk25-*.tsv` from the running image, and
`native-builtins/src/jdk_baseline.rs::ALL` is `read_dir`-cross-checked so an
unread baseline fails the build. The `Sdk` column above is the last hand
transcript in this file; it wants a
`scripts/baselines/graalsdk25-org.graalvm.nativeimage.tsv` written by a
`--classpath <jar>` arm of that generator. The SDK is a jar, not `jrt:`, so
`jdk_baseline.rs::parse`'s `# java.version` pin needs a sibling key.

### NOM E33-2 — `.github/workflows/ci.yml` — no job EXECUTES `vm/tests/*` under `--features synthetic-jdk`

Non-test change. The `synthetic-jdk` job's own comment explains that
`--all-targets` was chosen deliberately because "the lib/bin scope alone would
NOT have caught the test-module drift". It compiles integration targets and runs
`--lib` scopes. So every `#[cfg(feature = "synthetic-jdk")]` test in
`vm/tests/*.rs` is type-checked and never run — the darkness E25 rows 32 and 33
named. This lane moved four guards out from behind it; the following remain, and
they are the ones that genuinely cannot leave:

* `vm/tests/jck_conformance.rs` — `jck_full_corpus_runs`, `jck_regression_gate`
* `vm/tests/wp7_2_jdbc_core_types_reachable.rs` —
  `each_jdbc_core_type_has_registered_natives_under_synthetic_jdk`

NEW step, after the existing `Test vm (synthetic-jdk)`:

```yaml
      # Integration targets under the feature. `cargo check --all-targets`
      # above proves they COMPILE; nothing proved they run, so every
      # `#[cfg(feature = "synthetic-jdk")]` test in `vm/tests/*.rs` was
      # type-checked and never executed. E25 rows 32/33.
      - name: Test vm integration targets (synthetic-jdk)
        run: cargo test -p cratonvm-vm --tests --features synthetic-jdk
```

Expect this to be **loud on its first run** — 1,000+ integration tests that no
job has ever executed in this configuration. Land it advisory first and read the
list before promoting it to blocking; `[gates=stale]`.

### NOM E33-3 — `internal/gaps/jdk-regression-baseline.md` — four stale numbers and a command that runs nothing

Doc change, not owned by this lane.

1. `| Io | 20 | 37 | 54% |` → `| Io | 18 | 37 | 49% |` — the code lowered this
   floor and the document was not updated.
2. `| Lang | 34 | 109 | 31% |` → `| Lang | 34 | 110 | 31% |`
3. `| Util | 3 | 43 | 7% |` → `| Util | 3 | 45 | 7% |`
4. `| **TOTAL** | **109** | **421** | **26%** |` → `**424**`, and
   `**421 tests** across 19 categories` → `**424 tests**`

Then delete the four rows of `BASELINE_DOC_DRIFT` in
`vm/tests/jck_conformance.rs` in the **same** commit — the test fails until you
do (§6.3 mutation D), which is the point.

Also §"How to run" recommends
`cargo test -p cratonvm-vm --test jck_conformance -- --nocapture` as the "honest
baseline"; before today that ran zero tests, and it now runs the three table
checks and nothing else. Say so, and point the corpus run at
`--features synthetic-jdk`.

### NOM E33-4 — the `docs/internal` removal will turn `the_committed_baseline_document_and_this_table_agree` red

Whoever removes `docs/internal` from history must move
`jdk-regression-baseline.md` to a surviving path and update the two
candidates in `baseline_document()` (`vm/tests/jck_conformance.rs`). Deleting it
outright is also a legitimate choice — but then `jck_regression_gate`'s failure
message, which names the document as the thing it enforces, has to go too. The
panic text says this.

### NOM E33-5 — `difftest/src/opcorpus.rs` is not rustfmt-clean, in NON-test code, and the fmt gate runs on changed files

`.github/workflows/ci.yml`'s `Formatting (changed files)` job runs
`rustfmt --check --edition 2021` on every changed `.rs`. `difftest/src/opcorpus.rs`
has **five** pre-existing diffs, all above the `#[cfg(test)]` boundary at line
1127, so any lane that touches this file — including this one — trips the gate
through no fault of its own. This lane made the other three files it touched
rustfmt-clean (including the pre-existing diffs inside the test modules it owns)
but cannot fix these five without editing code `cargo build --release` compiles.

Sites, with rustfmt's own output:

`:182`
```rust
                Recipe::Unreachable(reason) => (OpcodeSupport::UnreachableFromSource(reason), None),
```

`:214`
```rust
                OpcodeSupport::UnreachableFromSource(reason) | OpcodeSupport::Skipped(reason) => {
                    GeneratorStatus::Unreachable {
                        reason: reason.to_string(),
                    }
                }
```

`:408`
```rust
            &helper(
                "i4",
                "int a, int b, int c, int d",
                "",
                "s += a + b + c + d;",
            ),
```

`:659`
```rust
        0xac => emit(
            &typed_return("int", "int", "s += p + k;"),
            "mix(retint(i));",
        ),
```

`:885`
```rust
        helper(
            "ls02",
            "long a, long b",
            "",
            "a = k;\nb = k + 1;\ns += a + b;"
        ),
```

Apply with `rustfmt --edition 2021 difftest/src/opcorpus.rs` once no build is in
flight. Do **not** reach for `cargo fmt --all`: this workspace is not
rustfmt-clean and has not been for a long time — the CI job checks changed files
for that reason.

---

## 8. Residuals, stated so a green run is not read as more than it is

* **Nothing here was run under `cargo`.** All four guards' new test bodies were
  additionally lifted **verbatim** into standalone `rustc` builds against
  stubbed registries and passed there
  (`{graal,op,wp72}_verbatim.rs`, plus the jck harness which lifts its three
  bodies and `baseline_rows` verbatim), so they type-check — including the
  match-ergonomics and `&String`/`&&str` comparisons, which is where a body that
  has never been compiled usually breaks. What §6 proves is that the assertions
  are *falsifiable* and do not fire on the current tree. It does not prove that
  `register_essential_natives` behaves as the source reads: the tables the
  harnesses use were parsed out of the registrars, not produced by running them.
  The first real `cargo test --workspace` is the check that remains.
* **Two hand tables remain**, both with commands in their doc comments and both
  nominated for the `scripts/baselines/` treatment: `JVMS_MNEMONICS` (§5.1) and
  the `Sdk` column (§2.1). A hand transcript's residual is always "the next
  version changes it"; only regeneration in CI removes it.
* **`test_register_graalvm_compat_total_count`** (`graalvm_compat.rs`, E25 row
  15) is untouched. Its `assert!(r.len() >= 12)` is now subsumed by the exact set
  in `test_register_total_method_count`, but a one-way floor next to a two-sided
  set is confusing rather than wrong; a later lane should delete it.
* **`opcorpus.rs:1152`** (E25 row 54) is untouched: `assert_eq!(generated, 202 -
  5, "five opcodes are unreachable from Java source")` still asserts a bare
  literal, and *which* five and *why* is a `javac` question answered by a number.
  Naming them is a small change in the same test module and was left out of this
  lane deliberately — it is a different property from the four assigned.
* **The `Io` floor of 18 vs 20** is recorded as document drift, but the trailing
  comment ("18-20/37 … slight variance") admits the pass count is not
  deterministic. A floor that has to absorb run-to-run variance is a different
  problem from a stale document, and this lane did not investigate it.

## 9. The lesson, in one line

Three of these four guards named a real property and then asserted something
else; the fourth asserted its own input. In all four cases the give-away was in
the file already — a comment doing arithmetic no assertion used, a doc naming a
document nothing read, a `#[cfg]` whose stated reason was false, and a corpus
built by the function the assertion called. `[gate=FR]`, `[both halves]`,
`[cfg≠guard]`, `[freeze=lock]`.
