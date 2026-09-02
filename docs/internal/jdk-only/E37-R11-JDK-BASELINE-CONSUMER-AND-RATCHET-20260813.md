# E37 / R11 — the baselines get a reader: a self-testing parser, a four-kind two-way ratchet, and the worked rewrite

**Date:** 2026-08-13 **Lane:** E37
**Closes the residual named in:** `E32-R11-JDK-BASELINE-CAPABILITY-20260813.md` §9
— *"Nothing consumes the baselines yet … this is a capability, not a fix, and
E25's sixty-one rows are all still exactly as red-proof as they were."*
**Status:** parser + ratchet LANDED and **MEASURED** (17/17 tests pass). The
`lib.rs` module declaration and the `phases_late.rs` guard rewrite are
NOMINATIONS — both files are owned by other lanes.

**This lane did not run `cargo` and did not build or run CratonVM.** It did run
`rustc` directly, which is a different thing and worth stating precisely:
`native-builtins/src/jdk_baseline.rs` has **no crate dependencies** — `use
std::collections::BTreeSet;` is its only `use` — so it compiles standalone as
its own test crate, out-of-tree, without touching the workspace `target/`
directory or any other lane's build. That is how every "MEASURED" below was
taken. `rustfmt --check --edition 2021` is clean, which is what the CI
changed-files formatting job runs.

**Edits applied (owned files only):**

| what | where |
|---|---|
| the parser, the four-kind ratchet, and 17 tests | `native-builtins/src/jdk_baseline.rs` (**NEW**, 1,090 lines) |
| the `jdk25-*.tsv` LF pin (NOM E32-1) | `.gitattributes` |
| the baseline-table row and the two rules (NOM E32-2) | `scripts/baselines/README.md` |

**§6 carries three nominations. NOM E37-3 is BLOCKING and is not a code
change: the thirty baselines and the generator are `??` untracked in git.**

---

## 1. The measurement, first

```
$ rustc --edition 2021 --test -o jdkbl_test.exe native-builtins/src/jdk_baseline.rs
$ ./jdkbl_test.exe --test-threads 1
running 17 tests
test tests::a_baseline_regenerated_on_a_later_jdk_is_refused ... ok
test tests::a_deleted_row_is_caught_by_the_rows_header ... ok
test tests::a_duplicated_triage_row_is_refused ... ok
test tests::a_reordered_column_grammar_is_refused ... ok
test tests::a_row_edited_in_place_is_caught_by_the_public_methods_header ... ok
test tests::a_truncated_include_is_refused_rather_than_auditing_nothing ... ok
test tests::an_unexplained_false_row_and_a_noisy_true_row_are_both_refused ... ok
test tests::audit_refuses_a_module_baseline ... ok
test tests::every_baseline_is_pinned_to_this_jdk ... ok
test tests::every_checked_in_baseline_is_wired_in_and_parses ... ok
test tests::kind_four_stale_a_row_naming_a_member_this_jdk_does_not_declare ... ok
test tests::kind_four_would_have_caught_structured_task_scope_config ... ok
test tests::kind_one_uncovered_a_jdk_member_with_no_row ... ok
test tests::kind_three_closed_a_gap_the_record_still_calls_open ... ok
test tests::kind_two_dropped_a_registration_that_is_gone ... ok
test tests::the_module_baseline_carries_the_real_export_denominator ... ok
test tests::the_two_hand_measured_counts_survive_the_rust_parser ... ok

test result: ok. 17 passed; 0 failed; ...; finished in 0.13s
```

Zero warnings from `rustc`. All thirty baselines parse; every header self-test
runs against every one of them.

---

## 2. TASK 1 — the parser, and the two defects in the specification it was written from

NOM E32-3 gave the parser as literal text. It was implemented, and **two things
in it are wrong against the data it reads**. Both were found by running it, not
by reading it, which is the argument for compiling a nomination before landing
it.

### 2.1 The specified `parse` panics on the module baseline

E32-3's final self-test is unconditional:

```rust
assert_eq!(Some(b.public_methods().len()), declared_public_methods, …);
```

`scripts/baselines/jdk25-module-java.base.tsv` has **no `# public-methods`
header** — it carries `# unqualified-exports 58` instead, its `# columns` is
`kind name target flags`, and its row kinds are `EXPORTS`/`PACKAGE`/`USES`/
`PROVIDES`. So `Some(0) == None` fails and the module baseline cannot be read
at all. E32-3's `include_str!` list happens not to include it, which is why the
defect was invisible: the nomination is correct for exactly the seven files it
names and wrong for the eighth kind of file in the directory. That matters
because the module baseline is the oracle for E25 row 13
(`test_java_base_exports_has_14_entries`, 14 asserted as fact against a real
58).

**As implemented:** the self-test dispatches on `# kind`. A `class` baseline
must carry `# public-methods` and it is recounted from the `METHOD` rows; a
`module` baseline must carry `# unqualified-exports` and it is recounted from
the `EXPORTS` rows with empty flags. An unknown `# kind` panics rather than
falling through to a check that cannot apply.

### 2.2 `audit` on a module baseline reads as a pass

`public_surface()` of a module baseline is **empty** — there are no `METHOD`
rows. So `audit` would report every triage row as `STALE` and, given an empty
triage, report *nothing at all*: a clean green from a guard that examined zero
members. That is E25 row 32's shape (a "regression gate" that returns green in
0.00 s with no corpus) reproduced inside the fix for it.

**As implemented:** `audit` asserts `baseline.kind == "class"` on entry, with a
message pointing at `Baseline::unqualified_exports()`.
`audit_refuses_a_module_baseline` is the test.

### 2.3 What the parser validates, and why each check is a cross-check

Every header is written by `generate.py` (Python, reading class-file bytes);
every count below is recomputed by the parser (Rust, reading the emitted rows).
Two independent implementations must agree, so none of these is a restatement.

| check | catches |
|---|---|
| `# jdk-baseline` == `1` | the row grammar changed under this parser |
| `# columns` == the exact grammar, per kind | **two columns swapped without the version changing** — a descriptor silently read as a name |
| `# java.version` starts with `25.` | a regeneration on JDK 26 silently re-baselining every guard |
| `# rows` == rows found | a row deleted, the file truncated, a hand edit |
| `# public-methods` / `# unqualified-exports` == a recount | a row **edited in place** — the corruption `# rows` cannot see |
| every row has exactly 4 fields | a malformed line; there is no escaping in this format because no field can contain a tab |
| every row kind is legal for the baseline kind | a kind this parser does not know is a row it would silently ignore |
| `!class.is_empty() && !rows.is_empty()` | an `include_str!` that resolved to nothing auditing a population of zero |

**Five planted bypasses**, one per corruption, each asserting that `parse`
panics *and* that it panics with the right diagnosis. E25 §4.4 calls a planted
bypass "what would make this red?" written as executable code, and names it the
only answer to that question that cannot itself go stale. Two files in this
tree had one before today.

### 2.4 The two known-answer counts, carried across a third implementation

`generate.py`'s KAT asserts `javax.crypto.Mac` = 17 and `java.lang.Character` =
96 against `javap` — a different instrument from the class-file reader under
test. `the_two_hand_measured_counts_survive_the_rust_parser` asserts the same
two numbers a third time, from the Rust recount. A header, a Python counter and
a Rust counter now all three have to agree, and the fixed point outside all of
them is still the `javap` transcript in E32 §4.

### 2.5 The dead-row check, both directions

`every_checked_in_baseline_is_wired_in_and_parses` reads
`scripts/baselines/` at test time and compares the listing against `ALL`:

* a `jdk25-*.tsv` on disk that no `include_str!` names → **fail**. That file can
  never go red; `generate.py --check` would keep regenerating it for nobody.
  This is `--check`'s `STALE` classification, seen from the Rust side.
* an `ALL` row naming a file that is gone → fail (in practice a compile error
  first).
* anti-vacuity: `>= 30` files, `> 1000` rows in total. A census that measures
  its own reach reads as agreement otherwise — `[reach≠defect]`.

All thirty baselines are wired in, not just the seven a guard reads today,
because the dead-row check is only two-way if `ALL` is meant to be complete.

---

## 3. TASK 2 — the four-kind two-way ratchet, measured against planted defects

The four kinds are E32 §6's table, implemented in `audit`, plus three
consistency kinds the same loop gets for free (`DUPLICATE`, `UNJUSTIFIED` — a
`false` row with no reason, `NOISY` — a `true` row carrying one).

**Kind 4 is split into three sub-cases**, because they need three different
repairs and one message would hide which applies:

| output | means |
|---|---|
| `STALE row (descriptor)` | the name exists on this class; the descriptor does not. A registration keyed on this triple can never be found by a real call |
| `STALE row (not public)` | declared, but not part of the public surface this census covers |
| `STALE row` | not a member at all. The message names the supertypes, because `javap -public` on a leaf class never lists an inherited member |

**Measured** against the real `SubmissionPublisher` baseline and the eleven
registrations *mechanically extracted* from `phases_late/concurrent.rs` (regex
over `r.register(sp, "…", "…"` — `let sp = "java/util/concurrent/SubmissionPublisher";`
is bound exactly twice in that file and to that string both times):

| input | audit output |
|---|---|
| the tree as it stands | **0 problems** |
| `register_p69_submission_publisher` deleted | 3 × `DROPPED` — `closeExceptionally`, `getClosedException`, `getMaxBufferCapacity` |
| the timed `offer` registered, row not updated | 1 × `CLOSED` — *"registered now, but this row still records it as absent … A closed gap recorded as open is standing permission."* |
| one triage row deleted | 1 × `UNCOVERED` — the JDK declares `isSubscribed` and this census has no row for it |

The first line of that table is the one the old guard also passed. The other
three are the ones it could not produce under any input.

---

## 4. Kind 4 and `StructuredTaskScope$Config` — the argument for the whole capability

E32 §8 recorded that the generator **refused a class on its first run**:
`java.util.concurrent.StructuredTaskScope$Config` is not in the runtime image.
`jdk25_concurrency.rs:1524` defines `CLS_CONFIG` as that spelling,
`s52_class_name_config` (`:5103`) asserts it, and `s52_total_registration_count`
(`:5130-5135`) counts six methods on it. The real nested type is
`$Configuration`.

`kind_four_would_have_caught_structured_task_scope_config` re-enacts it against
the real checked-in baseline. The six triples are taken verbatim out of
`jdk25_concurrency.rs`, every one is treated as registered (it is), and they are
audited against `jdk25-java.util.concurrent.StructuredTaskScope$Configuration.tsv`:

```
baseline internal_name = java/util/concurrent/StructuredTaskScope$Configuration
  STALE row (descriptor): …$Configuration.withName(Ljava/lang/String;)L…$Config;
      — this JDK (25.0.3) declares `withName` only as (Ljava/lang/String;)L…$Configuration;
  STALE row (descriptor): …withThreadFactory(…)L…$Config;  [same]
  STALE row (descriptor): …withTimeout(…)L…$Config;        [same]
  STALE row: …$Configuration.<init>()V is not a public member of that class on java.version 25.0.3
  STALE row: …$Configuration.getName()Ljava/lang/String; is not a public member …
  STALE row: …$Configuration.getThreadFactory()Ljava/util/concurrent/ThreadFactory; is not …
  UNCOVERED: the JDK declares …withName(…)L…$Configuration; and this census has no row for it
  UNCOVERED: …withThreadFactory(…)L…$Configuration;
  UNCOVERED: …withTimeout(…)L…$Configuration;
```

**All six registrations are STALE, and three of them carry the sharper
descriptor diagnosis** — a detail E32 §8 did not have, and it is the more
serious half: the fabricated name is not only the class constant, it is baked
into the **return descriptor** of `withName`, `withThreadFactory` and
`withTimeout`. Those three natives are registered under a key no real call can
produce. `getName` and `getThreadFactory` are not members of the JDK type at
all, under any descriptor.

**Kind 4 catches this two ways, and the second is stronger.** The blunt way is
the transcript above. The blunt-instrument way is that a guard converted to read
a baseline for `$Config` **does not compile** — `include_str!` cannot resolve a
file the generator refuses to write for a type not in the image. Either way the
name is checked against the runtime image instead of against the code that
invented it. A hand-maintained list cannot report that one of its own names is
not a JDK name; that is the sentence this whole capability exists to make false.

The one-line pin that converts this from luck into policy is on
`Baseline::internal_name`, and every converted guard should carry it:

```rust
assert_eq!(baseline.internal_name(), CLS_CONFIG);
```

Against `jdk25_concurrency.rs`'s `CLS_CONFIG` that line alone is red.

---

## 5. TASK 3 — the worked rewrite, delivered as NOM E37-2 (§6)

`b6_submission_publisher_core_methods_registered` lives in
`native-builtins/src/phases_late.rs:9027`, which this lane does not own, so it
is a nomination with exact literal text. Verified in the working tree today
rather than recalled from E32:

* `register_p60_flow` — `phases_late/concurrent.rs:2510, 2516, 2531, 2552, 2578,
  2582, 2587, 2592` = **8** registrations.
* `register_p69_submission_publisher` — `:5705, 5713, 5769` = **3**.
* `sp_registry()` (`phases_late.rs:9019`) is exactly those two registrars.
* the baseline: 17 public methods + 3 public constructors = **20**.
* the guard as it stands asserts **4**, and neither `closeExceptionally` nor
  `getClosedException` — the two methods p69 exists to add — is among them.

The replacement is measured to `0 problems` (§3). The nominated `TRIAGE` differs
from E32-4's draft in one respect: it adds the `assert_eq!(baseline.internal_name(), sp)`
pin from §4 and a stated-scope paragraph noting that `lib.rs`'s
`register_t31_concurrent_extras` also registers `<init>()V` and `close()V` on
this class and wins at runtime by registration order — both are already `true`
here so the verdict is unchanged, but a method that *only* t31 registers would
read as absent in this fixture. E32-4 did not say that and a reader would
otherwise take the fixture for the runtime set.

---

## 6. NOMINATIONS

### NOM E37-3 — **BLOCKING, and not a code change**: the baselines are untracked

`git status --porcelain -uall scripts/` in this working tree today:

```
?? scripts/baselines/jdk25-java.lang.Character.tsv
… 30 files …
?? scripts/jdk-baseline/JdkBaseline.java
?? scripts/jdk-baseline/classes.txt
?? scripts/jdk-baseline/generate.py
```

`git ls-files scripts/baselines/` returns the eight pre-existing files and none
of the thirty. **They exist only in this shared worktree.** Everything in this
record — every `include_str!` in `jdk_baseline.rs`, NOM E37-1, NOM E37-2 — fails
to compile on any other checkout until:

```
git add scripts/baselines/jdk25-*.tsv scripts/jdk-baseline/
```

This is stated first because it is invisible: the code compiles and the tests
pass *here*, which is exactly the condition under which a missing `git add` gets
missed. `[edit-vs-]`.

Note also that `.gitattributes` (NOM E32-1) must be committed **in the same
change or earlier**, not after: attributes are applied at add/checkout time, so
adding the files first and pinning them second leaves CRLF in the index for
whoever pulls in between.

### NOM E37-1 — `native-builtins/src/lib.rs:4586` — the module declaration

OLD (verified unique in the working tree today — the only
`pub(crate) mod test_utils;` in the file, at `:4587`, preceded by its `#[cfg(test)]`
at `:4586`):

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
/// docs/known-issues/jdk-only/E37-R11-JDK-BASELINE-CONSUMER-AND-RATCHET-20260813.md.
#[cfg(test)]
pub(crate) mod jdk_baseline;
```

**PREDICTED effect: compiles and adds 17 passing tests.** The module itself is
measured (§1); what is predicted is only that `cargo test -p cratonvm-native-builtins`
agrees with `rustc --test` on a file that has no crate dependencies. `dead_code`
is already allowed crate-wide (`lib.rs:9`), so the accessors no guard calls yet
do not warn.

### NOM E37-2 — `native-builtins/src/phases_late.rs:9026-9036` — the worked rewrite

OLD (verified byte-exact and unique in the working tree today):

```rust
    #[test]
    fn b6_submission_publisher_core_methods_registered() {
        let r = sp_registry();
        let sp = "java/util/concurrent/SubmissionPublisher";
        assert!(r.find(sp, "submit", "(Ljava/lang/Object;)I").is_some());
        assert!(r
            .find(sp, "subscribe", "(Ljava/util/concurrent/Flow$Subscriber;)V")
            .is_some());
        assert!(r.find(sp, "hasSubscribers", "()Z").is_some());
        assert!(r.find(sp, "getNumberOfSubscribers", "()I").is_some());
    }
```

NEW (`rustfmt --check --edition 2021` clean at this indentation):

```rust
    /// The population is **the JDK's**, not this registrar's.
    ///
    /// `scripts/baselines/jdk25-java.util.concurrent.SubmissionPublisher.tsv`,
    /// generated by `scripts/jdk-baseline/generate.py` from
    /// `jrt:/modules/java.base/java/util/concurrent/SubmissionPublisher.class`
    /// on openjdk 25.0.3+9: 17 public methods plus 3 public constructors = 20
    /// members. `register_p60_flow` (`phases_late/concurrent.rs:2510`, `:2516`,
    /// `:2531`, `:2552`, `:2578`, `:2582`, `:2587`, `:2592`) makes 8 of them and
    /// `register_p69_submission_publisher` (`:5705`, `:5713`, `:5769`) makes 3.
    /// The body this replaced named **four**, and neither of the two methods
    /// p69 exists to add (`closeExceptionally`, `getClosedException`) was among
    /// them — so it could not have gone red if `register_p69_submission_publisher`
    /// had been deleted outright
    /// (`docs/known-issues/jdk-only/E25-R11-GUARD-POPULATION-SWEEP-20260813.md`
    /// row 20).
    ///
    /// Every row below is one of the JDK's twenty, and `jdk_baseline::audit`
    /// fails four ways: a JDK member with no row (`UNCOVERED`), a `true` row
    /// that is not registered (`DROPPED`), a `false` row that IS registered
    /// (`CLOSED`), and a row naming something this JDK does not declare
    /// (`STALE`). The third and fourth are the ones a hand list never has, and
    /// they are the two E20 found already broken.
    ///
    /// **Scope, stated:** `sp_registry()` is `register_p60_flow` +
    /// `register_p69_submission_publisher` and nothing else. `lib.rs`'s
    /// `register_t31_concurrent_extras` also registers `<init>()V` and
    /// `close()V` on this class and wins at runtime by registration order
    /// (`concurrent.rs:2505`, `:2546`); both are already `true` here, so the
    /// verdict does not change — but a method that ONLY t31 registers would
    /// read as absent in this fixture.
    #[test]
    fn b6_submission_publisher_surface_is_triaged_against_the_jdk() {
        use crate::jdk_baseline;

        let r = sp_registry();
        let sp = "java/util/concurrent/SubmissionPublisher";
        let baseline = jdk_baseline::parse(jdk_baseline::SUBMISSION_PUBLISHER);
        // The class name is checked against the runtime image rather than
        // against the constant beside it. That is the line that catches a
        // fabricated class name — `jdk25_concurrency.rs`'s
        // `StructuredTaskScope$Config`, a nested type JDK 25 does not have, is
        // the in-tree example (E32-R11 §8).
        assert_eq!(baseline.internal_name(), sp);

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            // --- register_p60_flow (8) ---
            ("<init>", "()V", true, ""),
            ("submit", "(Ljava/lang/Object;)I", true, ""),
            ("offer", "(Ljava/lang/Object;Ljava/util/function/BiPredicate;)I", true, ""),
            ("close", "()V", true, ""),
            ("isClosed", "()Z", true, ""),
            ("hasSubscribers", "()Z", true, ""),
            ("getNumberOfSubscribers", "()I", true, ""),
            ("subscribe", "(Ljava/util/concurrent/Flow$Subscriber;)V", true, ""),
            // --- register_p69_submission_publisher (3) ---
            // These two are the reason p69 exists and were asserted by NOTHING
            // until this rewrite.
            ("closeExceptionally", "(Ljava/lang/Throwable;)V", true, ""),
            ("getClosedException", "()Ljava/lang/Throwable;", true, ""),
            ("getMaxBufferCapacity", "()I", true, ""),
            // --- the nine the JDK declares and this VM does not register ---
            // UNTRIAGED, deliberately: each `false` below states what is known
            // and does not guess at intent. A reason that says "not measured"
            // is worth more than a confident one that is wrong, and the lane
            // that wrote these rows could not run the VM to find out which of
            // them a real SubmissionPublisher user reaches.
            ("<init>", "(Ljava/util/concurrent/Executor;I)V", false,
             "Only the no-arg constructor is registered; concurrent.rs:5701's own \
              comment says so. A caller using the Executor form gets the real JDK \
              body and a publisher with no side-table row, which is the shape E25 \
              §1.2 describes for Mac."),
            ("<init>", "(Ljava/util/concurrent/Executor;ILjava/util/function/BiConsumer;)V", false,
             "As the two-arg constructor above."),
            ("offer", "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;Ljava/util/function/BiPredicate;)I", false,
             "The TIMED offer. The untimed overload IS registered, so a caller \
              that adds a timeout crosses from a native to the real JDK body \
              mid-API. Unmeasured."),
            ("consume", "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;", false,
             "Not registered; not measured."),
            ("estimateMaximumLag", "()I", false, "Not registered; not measured."),
            ("estimateMinimumDemand", "()J", false, "Not registered; not measured."),
            ("getExecutor", "()Ljava/util/concurrent/Executor;", false,
             "Not registered; no executor is modelled by the registered <init>()V."),
            ("getSubscribers", "()Ljava/util/List;", false,
             "Not registered, although `getNumberOfSubscribers` IS and reads the \
              same field-0 subscriber list. The two answers can disagree."),
            ("isSubscribed", "(Ljava/util/concurrent/Flow$Subscriber;)Z", false,
             "Not registered; same list as `getSubscribers`."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(sp, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "SubmissionPublisher's registered surface disagrees with \
             scripts/baselines/jdk25-java.util.concurrent.SubmissionPublisher.tsv \
             ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );
    }
```

**PREDICTED effect: passes as the tree stands** — and the prediction is now
narrow, because the `audit` half is measured (§3, `0 problems`). The single
remaining assumption is that `NativeMethodRegistry::find` answers `Some` for
exactly the eleven `(name, descriptor)` pairs the regex found, i.e. that no
registration in those two functions is conditional and none is overwritten
inside `sp_registry()`. If it goes red, the failure text names the exact triple
**and the direction**, and the first thing to check is whether a registration
moved between p60 and p69 rather than whether the baseline is wrong.

---

## 7. Which of E25's 61 rows convert mechanically, and which need a different oracle

Asked plainly, because the honest answer is smaller than "sixty-one" and larger
than "one".

### 7.1 Convertible **today**, with a baseline already checked in — 12 rows

`audit` + an `include_str!` + a per-row `expect_registered` judgement is the
whole change. No new tooling.

| E25 row | guard | baseline | denominator it gains |
|---|---|---|---|
| 2 | `tls.rs:4485` `test_ssl_parameters_registration_complete` | `SSLParameters` | 13 of **31** |
| 4, 5, 6 | `shared_secrets_bridge.rs:2861/2890/2868` | `SharedSecrets` | 15 of **30** `getJava*Access` |
| 7 | `jmx.rs:7010` `test_runtime_mxbean_all_getters_registered` | `RuntimeMXBean` | 13 of **17** |
| 8 | `jmx.rs:7601` `test_management_factory_returns_all_mxbeans` | `ManagementFactory` | 7 of **25** |
| 10 | `tls.rs:4518` `test_key_store_registration_complete` | `KeyStore` | ~13 of **30** |
| 13 | `jdk25_language.rs:834` `test_java_base_exports_has_14_entries` | `module-java.base` | 14 of **58** (map binary→internal; `Baseline::unqualified_exports_internal()` does it) |
| 16 | `jdk25_concurrency.rs:5113` `s52_total_registration_count` | `StructuredTaskScope{,$Joiner,$Subtask,$Configuration}` | **goes RED, correctly** — §4 |
| 20 | `phases_late.rs:9027` — **done**, NOM E37-2 | `SubmissionPublisher` | 4 of 11 of **20** |
| 21 | `phases_late.rs:9304` `sq_real_rendezvous_methods_registered` | `SynchronousQueue` | 5 of 17 of **25** |
| 22 | `lib.rs:4957` `…does_not_override_reflection_factory_serialization` | `ReflectionFactory` | 4 of 18 of **25** |

Row 3 (`cds.rs:1321`) is **half** convertible: `jdk.internal.misc.CDS` is
baselined, `sun.management.CDSMetrics` is not — one line in `classes.txt` closes
it.

**Row 16 is the one to do next, and to expect red.** It is the only row in this
table whose conversion changes a verdict rather than a denominator: the class it
audits does not exist. Row 4-6 (`SharedSecrets`, 15 of 30) is the largest
*silent* gap and the one E32 §7 nominated as clearest.

### 7.2 Convertible after **one line in `classes.txt`** — no new tooling

The class is in the runtime image; nobody has asked the generator for it yet.
`generate.py --update` writes it and §7.1 applies.

* row 9 `test_all_mxbean_classes_have_init` — the ten platform MXBean interfaces.
* row 14 `t19_m1_composite_type_count_matches_well_known` — the
  `javax.management.openmbean` types.
* row 25 `w7_18_jep505_surface_is_not_shadowed_here` — already baselined; needs
  the intersection shape, not new data.
* row 26 `classloader.rs:13156`, row 50 `wp8_10_7_throwable_subclass_getmessage.rs`
  — ordinary `java.base` classes.
* **row 37 `classloading/src/shadow_layout.rs:948`** is the most valuable of
  these and the generator already emits what it needs: `FIELD` rows, in
  declaration order, from the class file. That guard's doc claims *"Spelled out
  so a JDK upgrade falsifies it"* over a hard-coded literal — E25 calls that the
  most expensive shape in the sweep, a comment describing a mechanism the test
  does not have. Baselining `java/lang/reflect/Method` et al. gives it the
  mechanism. **This is outside `native-builtins/` and outside E25's stated
  scope for the capability; recording it because the data format already
  supports it.**

### 7.3 Need a **different oracle**, and the generator would need a new mode — 8 rows

| rows | what is actually needed |
|---|---|
| 1, 15 | GraalVM SDK `org.graalvm.nativeimage.*` — a `--jar` mode; `JdkBaseline` reads `jrt:/` only |
| 27 | the pinned `h2-*.jar` — same `--jar` mode |
| 11, 12 | the JDK 25 `@Deprecated(forRemoval)` set — a whole-image annotation walk |
| 19 | `@IntrinsicCandidate` on `BigInteger` — the *surface* is baselined; "which five are intrinsics" is an annotation question a surface dump does not answer |
| 30, 31 | the `SunJCE` `Mac` / `KeyGenerator` service sets — `Security.getProvider("SunJCE").getServices()`, a provider dump, not a class surface. E25 §2.2 took it by hand and named the residual |
| 46 | JFR's `jdk.*` event set — `jfr metadata` / `metadata.xml` |
| 34, 53, 54 | JVMS §6.5 opcode names — `javap -c` over a corpus, frozen as a `.tsv`. The format here would serve; the emitter would not |

### 7.4 A JDK baseline is the **wrong instrument** — roughly 30 rows

This is the part worth saying out loud, because "we have baselines now" invites
the assumption that all 61 rows are one edit away. They are not. Rows 23, 24,
28, 29, 32, 33, 35, 36, 38–45, 47–49, 51, 52, 55–61 audit **in-tree**
populations: enum variants, allowlists, registry knobs, flag inventories, mode
tables. There is no JDK class to read. Their fix is already written twice in
this tree and E25 §4.4 names it:
`types/tests/flag_declaration_guard.rs::the_allowlist_has_no_dead_rows` (`:330`)
and `vm/src/runtime/resolve/guard.rs::the_allowlist_has_no_dead_rows` (`:622`),
plus `jit/src/x64/single_pass_only.rs:327`'s exhaustive-`ordinal()` pattern for
rows 41, 42, 44, 45. Copy those; a `.tsv` of a JDK class does nothing for them.

Rows 1, 32, 33, 34 additionally **cannot go red for the property they are named
for, under any input** — E25 §Header. For those the baseline is not even the
second problem.

**Summary:** of 61 rows, **12** convert with data already checked in, **~7**
convert after a `classes.txt` line, **~8** need a new generator mode or a
different instrument entirely, and **~30** are not JDK-surface questions at all.

---

## 8. Residuals, stated so a green run is not read as more than it is

* **The 30 baselines and the generator are untracked (NOM E37-3).** Everything
  here compiles and passes in this working tree and nowhere else. This is the
  residual most likely to be missed, because the symptom is absence.
* **`jdk_baseline.rs` is measured by `rustc`, not by `cargo test`.** The module
  has no crate dependencies, so the two should agree; "should agree" is not
  "measured". The predicted part of NOM E37-1 is exactly that gap and nothing
  more.
* **NOM E37-2 is measured against a *regex extraction* of the registrations,
  not against `NativeMethodRegistry`.** The regex found 11 and E32 read 11 by
  hand, from the same source, so the two agree — but neither ran the registrar.
  A conditional registration or an intra-fixture overwrite would break the
  prediction and not the extraction. `[setup lies]`.
* **Nine `false` rows in NOM E37-2 say "not measured", and that is the honest
  state, not a placeholder.** Each is a decision someone has to make with a
  running VM: unimplemented, or deliberately out of scope. The ratchet's value
  is that the decision is now *recorded and enforced in both directions*, not
  that it has been made. `getSubscribers` is the one worth looking at first: it
  is absent while `getNumberOfSubscribers` is present and both read the same
  field-0 list, so the two can disagree.
* **The ratchet enforces a *record*, not correctness.** A row that says `false`
  with a confident wrong reason passes. What it cannot do any more is be
  *silently* wrong in either direction, which is the specific decay E20 found.
* **`--check` is still not wired into CI** (E32 §9, unchanged by this lane). A
  JDK bump is now visible to `jdk_baseline::parse`'s version pin the moment a
  baseline is regenerated — but nothing regenerates them on a schedule.
* **`kind_four_would_have_caught_structured_task_scope_config` is a
  re-enactment, not a guard on `jdk25_concurrency.rs`.** It proves the
  mechanism against real data; it does not make `s52_class_name_config` red.
  That conversion is E25 row 16 and belongs to the owning lane (§7.1).
* **The `.gitattributes` pin is not yet observable.** The jdk25-*.tsv are LF in
  the working tree today only because the generator just wrote them.
  `git ls-files --eol scripts/baselines/` shows `i/lf w/crlf` for every
  unpinned neighbour in that directory and `i/lf w/lf attr/text eol=lf` for the
  one file the pre-existing stanza pins — so the flip would happen at the first
  clone, not now. The pin is applied ahead of the symptom on purpose.
