# W7-51 — the vacuous-test population, measured instead of sampled

Status: **W6-5's two open residuals are closed structurally**, and a systematic
sweep of the whole test surface found **45 further findings** plus one
population of 23. Round 1 (`W6-5-vacuous-tests.md`) found six by accident, while
reading source for something else. This round asked the question on purpose.

Predecessors: `W6-5-vacuous-tests.md` (the two shapes, and the six),
`L10-rjdkprocess-vector-overassertion.md` (the opposite failure),
`W7-1-treemap-views-and-iterator-remove-contract.md` (the defects `RJdkViews`
gates), `vm/tests/probe_compile_guard.rs`, `vm/tests/common/mod.rs`.

---

## 0. The headline

| | round 1 (W6-5) | round 2 (here) |
| --- | --- | --- |
| how found | by accident, reading for another reason | swept on purpose |
| Rust `#[test]` fns examined | — | **16,376** across 13 crates |
| Java vectors examined | 55, for two shapes | **72 / 72**, executed, 7 mutated |
| findings | 6 | **45**, plus a population of 23 fixtures / ~286 call sites |
| proved by MUTATION | 4 | **14** |
| fixed here | 4 | 7 fixtures + 11 tests + 3 gates armed |

**Round 1's estimate of its own §3.2 was low by half.** It reported eleven
missing fixtures out of twelve referenced. The real numbers are **23 missing out
of 27** — its census matched only the single-line `.join("apps").join(<probe>)`
spelling and missed the multi-line builder form, which is what most of the
harnesses use. Two of the eleven it *did* find were written off as
"self-generates its source — likely OK". Neither does; see §1.1.

---

## 1. Disposition of W6-5's three open items

### 1.1 §3.2 — `apps/` is gitignored and the fixtures are gone. **CLOSED structurally, 7 of 23 rebuilt.**

The mechanism, restated because it is the whole finding: `.gitignore` line 12 is
`apps/`. A probe fixture written there is untracked. It exists for its author,
`git status` never mentions it, `git add -A` never stages it, and it is gone for
everyone else — taking its test's ability to fail with it, silently. That is one
repository-layout defect, not 23 independent test bugs.

**Seven fixtures rebuilt**, each measured on HotSpot 25.0.3.9 before its expected
output was trusted:

| fixture | driving test | what makes it go red |
| --- | --- | --- |
| `apps/chm_basic/ChmScale.java` | `wave2_chm.rs` | re-admitting `Integer.valueOf` / `Integer.<init>` to the JIT — the allocate-then-putfield miscompile that returned `value=0` |
| `apps/atomic_probe/AtomicProbe.java` | `wave4_a_atomic.rs` | a non-atomic `compareAndSet`, a CAS that never publishes, an **equality-based** reference CAS |
| `apps/console_probe/ConsoleProbe.java` | `wave3_console_module.rs` | dropping the `Module.canUse` registration, which NPEs inside `Console.instantiateConsole()` |
| `apps/lm_subclass/LmSubclass.java` | `block_2b_logmanager_factory.rs` | ignoring `-Djava.util.logging.manager`, or instantiating the manager twice, or not using the one it built |
| `apps/methodhandles_probe/MhProbe.java` | `wave2_c_methodhandles.rs` | a missing or mis-typed `findStatic`/`findVirtual` path |
| `apps/selector_probe/SelectorProbe.java` | `wave3_c_selector.rs` | `getLocalAddress()` misreporting the bound address, `select()` never reporting `OP_ACCEPT` |
| `apps/proxy_probe/ProxyProbe.java` | `wp2_5_proxy.rs` | a generator that wires only interface 0, a proxy that inherits an interface default instead of routing to the handler |

Three of these were not merely restored but redesigned away from a vacuous
specification. `AtomicProbe`'s driving doc asked for a failing reference-CAS
witness of `new String("hello")` against a held `"world"` — but an
`equals()`-based CAS also answers `false` there, so the assertion could not tell
identity from equality. It is now `new String("world")` against a held
`"world"`: `equals` but not `==`, which an equality-based CAS gets wrong twice
over. `MhProbe` was rewritten from lambdas to plain `try`/`catch` blocks so a
`LambdaMetafactory` gap takes down one line rather than all seven and gets
misread as a `Lookup` defect. `ChmScale` was checked with
`CRATONVM_DBG_JITC=1` to confirm it actually reaches the OSR back-edge and
per-callee thresholds the miscompile needs — a fixture that merely built a
1000-entry map would have been green forever, which is the trap this record is
about arriving inside its own repair.

`ProxyProbe` is **mutation-checked**: with `count()` returning 99 and the
default-method case routed to `InvocationHandler.invokeDefault`, it prints
`summary 4/6` and `FAIL` and names both broken rows.

**The durable half is `vm/tests/probe_fixture_census.rs`**, and it matters more
than the seven. It enumerates all 27 fixtures with their candidate paths and
owning harnesses, and fails a plain `cargo test --workspace` — which
`.github/workflows/ci.yml` runs on every push — when:

* a fixture is missing and not in the committed baseline (a fixture referenced
  but never `git add -f`ed shows up here on the first CI clone), **or**
* a fixture is present but *still* in the baseline (restoring one is not
  finished until its row is deleted), **or**
* a `vm/tests/*.rs` that reaches into `apps/` appears in neither table, so the
  silent-skip population cannot grow unrecorded.

The second direction is the load-bearing one. A baseline that only ever records
"known bad" decays into a list nobody re-checks; this one cannot, because
clearing an entry is what makes the test green again — the ratchet shape
`L6-unadjudicated-bridge-ratchet-DONE-20260805.md` established.

Three prior mechanisms all existed and all failed to make this visible:
`common::require_fixture` makes the skip loud, but on captured stderr a green
`cargo test` summary never shows; `probe_compile_guard.rs` requires every
`apps/`-reaching test to route through it, which is a *shape* requirement, not
a presence one; and `CRATONVM_REQUIRE_E2E` promotes it to a panic but was set by
nobody (§1.2). None of the three **fails a default run**. That was the gap.

**Two corrections to W6-5 §3.2.** `scanner_probe` and `xml_probe` were recorded
as "self-generates its source — likely OK". They do not:
`ensure_scanner_probe_compiled` reads `ScannerProbe.java` off disk and skips when
it is absent, and `wave1_d_xml_stax.rs`'s `write_fixture()` writes the XML
*data* to `/tmp/test.xml` (itself a Unix-only path on a suite that runs on
Windows), not the probe source. Both are missing fixtures like the rest.

**Sixteen remain**, each with its reason in the baseline. Two of them —
`bytebuddy_probe` and `h2` — need third-party jars this repository does not
carry and are not reconstructible from the assertions at all. The rest are
multi-nested-type probes (`ConstructorProbe` has 11 nested types across two
harnesses, `MethodInvokeProbe` 6, `ReflectProbe` 5) or, in `aqs_probe`'s case, a
latency shape where a rebuild that failed to reproduce the historical contention
would be green forever — worse than the absence.

**Also fixed in all seven driving harnesses: the stale-`.class` trap.** Each did
`if class_file.exists() { return true; }`, which reuses a compiled fixture
regardless of age. A probe's `SbRunner.class` was once found to be a *month*
older than its `.java`, and a landed change appeared in no log because of it.
The harnesses now recompile when the source's mtime is newer. One useful side
effect: a `.class` present with its `.java` missing now routes through
`require_fixture` instead of silently answering `true`.

### 1.2 §3.3 — `CRATONVM_REQUIRE_E2E` is set by no workflow. **CLOSED, both ends proved.**

A flag no consumer reads and a flag no producer sets are the same defect from
two ends. This repository has produced both — a young-generation pause goal that
moved a number nothing read is the sibling case. This one was the second kind,
and it was worse than useless: `vm/tests/common/mod.rs` claimed in two comments
that *"CI sets it to assert that a green run was a real one"*, so a reader had
positive reason to believe ~60 prerequisite skips were being caught. None were.

**The consumer reads it — proved by construction, not by grep.**
`vm/tests/require_e2e_gate.rs::the_gate_is_honoured_by_the_harness` calls the
real `common::require_fixture` / `require_binary` / `require_jdk` with the
variable unset, set to `0`, set to the empty string, and set to `1`, and asserts
the behaviour differs. It carries its own **positive control** — a fixture that
IS present must still be returned under the gate — without which a
`require_fixture` that panicked unconditionally would satisfy every other
assertion in the test. It is deliberately one test function rather than four:
`set_var` is process-wide and cargo runs a binary's tests on parallel threads,
so splitting it would produce a flake whose green runs proved nothing.

> Not run. This lane could not build. What is proved is that the assertions
> address the real helpers on the real call path; whether they pass is the
> orchestrator's first build. A red there is a finding, not a bad test.

**A producer sets it — proved by mutation.** `.github/workflows/ci.yml` now
exports `CRATONVM_REQUIRE_E2E: 1` on a step that runs three fixture-gated
targets, and `a_workflow_sets_the_gate` reads `.github/workflows/*.yml` and
fails when none of them sets it. Its parser accepts `NAME:` and `NAME=` and
**rejects a mention inside a comment**, because "someone wrote the name down" is
not "someone switched it on" — and demoting a live setter to a comment is the
most likely way this rots. The predicate was mutation-checked against the real
tree, on three copies of `.github/workflows/`:

```
REAL         -> ['ci.yml']
REMOVED      -> []          # the `CRATONVM_REQUIRE_E2E: 1` line deleted
COMMENT_ONLY -> []          # the same line demoted to `# CRATONVM_REQUIRE_E2E: 1 (disabled)`
```

**Why the CI step runs three targets and not `--workspace`.** Sixteen fixtures
are still absent, so setting the variable on the whole run would panic on all
sixteen at once. A permanently-red job is a job nobody reads, which is how the
formatting gate in this same file ended up standing in front of the correctness
gates. The list is a **ratchet**, and its ordering rule is the one W6-5 §3.1 had
to learn the hard way: a target goes on it only *after* someone has run it
against a real CratonVM binary. Scheduling an unverified one manufactures either
a red nobody trusts or a green nobody earned. The three seeded here were run
under a CratonVM binary on 2026-08-12.

The step is not decoration: with the variable set, deleting
`apps/chm_basic/ChmScale.java` fails it, where the plain `Test` step above would
still report `ok` in 0.00 s.

### 1.3 §3.4 — the new vector and registration have never run under CratonVM. **Structurally verified; still never run under CratonVM.**

Two claims, checked separately.

**Scheduled.** `RJdkViews` is in `CORE_CLASSES` in `regression-suite/run.sh`,
deliberately not also in `JDKONLY_CLASSES` (it asserts `--real-jdk` compatibility
behaviour). `run.sh`'s own coverage census independently confirms the tables are
complete: 42 CORE + 28 JDK-only + 2 unregistered = exactly 72 sources, with no
unlisted vector and no list entry whose source is gone.

**Capable of failing — PROVED BY MUTATION, five times.** The vector is green on
HotSpot 25.0.3.9 at 67 checks. Five mutants, one per defect family plus the
negative control, each substituting a deliberately broken implementation of the
JDK behaviour the family is about:

| mutant | what it models | outcome |
| --- | --- | --- |
| `descendingMap()` returns an empty map | family 1, the recorded defect verbatim | `AssertionError: descendingMap size: 0`, rc=1 |
| an iterator that accepts `remove()` before `next()` and eats element 0 | family 2, the recorded defect verbatim | `AssertionError: Iterator.remove() before next() must throw IllegalStateException`, rc=1 |
| `%e`/`%g` print `Double.toString` shapes | family 3 | `AssertionError: format floats: 0.333\|1.2345e3\|1.0E-4`, rc=1 |
| `summaryStatistics()` answers a zeroed struct | family 4 | `AssertionError: summaryStatistics count: 0`, rc=1 |
| a range view that forwards every call, ignoring its bounds | the §1.3 negative control | `AssertionError: headMap.put outside the view's range must throw IllegalArgumentException`, rc=1 |

Each fails at a **named** assertion in milliseconds, never at the suite's 120 s
timeout — which would be indistinguishable from a VM hang. The last row is the
one that matters most: without the negative control, a "view" that ignored its
range entirely would satisfy every write-through assertion in family 1.

Still true, and still the falsifier for this section: **no CratonVM run.**

---

## 2. The sweep — method, and what it found

### 2.1 What was actually examined

| crate | `#[test]` fns |
| --- | --- |
| vm | 5,347 (1,525 in `vm/src/vm/tests.rs`, 1,041 in `vm/tests/*.rs`) |
| native-builtins | 4,018 first-party |
| jit | 2,251 |
| gc | 1,551 |
| classloading | 924 |
| types | 555 |
| native-io | 445 |
| reader | 399 |
| native-api | 343 |
| native-awt | 269 |
| native-collections | 215 |
| native-builtins-crypto | 44 |
| native-builtins-security | 15 |
| **total** | **16,376** |

Plus all 72 `regression-suite/src/*.java`, 70 of which were **executed** on
HotSpot 25 with their output re-filtered through `run.sh`'s own `extract()` to
measure how much of each vector reaches the cross-VM diff, and 7 of which were
**mutated**.

The brief's own premise needed correcting on the way: `vm/src/vm/tests.rs` is
**not** dark. `.github/workflows/ci.yml` runs
`cargo test -p cratonvm-vm --lib --features synthetic-jdk`, so all 1,525 of its
tests execute. A *subset* of nine is dark, for a different and worse reason —
§2.4.

### 2.2 The dominant shape: **the test reconstructs the fix inside its own body**

This is the finding of the round, it was not in W6-5's two shapes, and it
accounts for ten of the worst cases. It reads as thorough coverage. The tell is
a comment of the form *"we can't call the native without a `NativeContext`, so
we replicate the check here"* — after which the test asserts against its own
replica and the production function is never called at all.

| finding | file | what it re-implements | reverting the named fix leaves it green |
| --- | --- | --- | --- |
| F1 | `native-builtins/src/securerandom.rs:1813` | the polar transform, over a test-local `secure_uniform` | repoint `SecureRandom.nextGaussian` back at `java.util.Random`'s LCG — the exact named defect |
| F2 | `native-api/src/init_level.rs:162` | `fetch_max` on a fresh local atomic | restore the load-then-store race in `set_init_level` |
| F4 | `native-builtins/src/tzdb.rs:881` | the `GMT+HH:MM` delegation `get_zone_rules` is supposed to perform | delete the fallback branch; `GMT+01:00` resolves to a 0 offset everywhere again |
| F5 | `native-builtins/src/jboss_resource_loader.rs:200` | the `..` path-traversal scan, over a literal path | remove the traversal guard from `native_create_resource_loader` — a security control |
| F6 | `native-builtins/src/ironjacamar_pool.rs:541` | the timeout→`ResourceException` classification, over a hard-coded string | delete the classification from `native_pool_get_connection` |
| F7 | `native-io/src/file_channel.rs:1758` | nothing at all — the body is `assert_eq!(0x7fff_ffff_i32, i32::MAX)` | make `maxDirectTransferSize0` return 0, or unregister it |
| F9 | `native-io/src/zip_real_jar.rs:1459` | round-trips a comment through the third-party `zip` crate | make `native_jarfile_get_comment` return null unconditionally |
| F18-F21 | `native-collections/src/lib.rs:57752…57784` | four tests named for `values_equal` that never call it | rewrite `values_equal` to `true` |
| F22-F24 | `native-builtins/src/serialization.rs:5894…5927` | construct a `RuntimeError` in the test, assert it matches itself | make the not-supported paths return `Ok(None)` |
| — | `native-collections/src/lib.rs:56917` | `rnd_gaussian_pair`, consuming both variates in a loop the test writes | delete the `nextNextGaussian` cache from `native_random_next_gaussian` |

The last row was found independently and is **fixed here**, as the worked
example. Its doc comment names the defect precisely — *"this used to discard the
second and draw a fresh pair every time"* — and the function that did that,
`native_random_next_gaussian`, is not on the test's call path. The replacement
drives the native through a `MockCtx` receiver with the two real
`java/util/Random` slots and pins three things: that the second variate is
cached, that the next call consumes it, and that a cache-served call does not
advance the LCG. Then it pins the rate exactly — after six `nextGaussian()`
calls, `next(32)` must be `1583910553`, measured on Temurin jdk-25.0.3+9. That
last assertion is an `i32`, which is the point: **no tolerance can widen it**.
The sibling's `1e-12` is four orders wider than the fdlibm-vs-libm ulp it is
justified by, and is now not load-bearing for anything.

Two more findings sit one step away from this shape — a test that asserts a
native is *registered* where the defect it names is a **behaviour**:

* **F8**, `vm/tests/wave3_scanner.rs:45`. Its own failure message names the
  defect: *"the pre-S110 no-op stub left `System.in` null."* Its assertion is
  `natives.find("java/lang/System", "initPhase1", "()V").is_some()`. **A no-op
  stub is registered too.** The test cannot tell the fixed state from the broken
  one it describes.
* **F10**, `native-builtins/src/phases_late.rs:8701`. `assert!(cap.map(|c| c > 0).unwrap_or(true))`
  — and `None` means the zip-bomb inflation cap is **disabled**, which is the
  opposite of what the test's own comment claims it checks.

### 2.3 Tests whose name promises a property the body never checks

Nine, each ending in a discarded value or nothing at all:

| finding | file | the give-away |
| --- | --- | --- |
| F11 | `jit/src/lib.rs:26921` `rg9_tiered_manager_exists` | doc promises the hot-counter threshold is asserted; body constructs the manager and reads nothing |
| F12 | `native-api/src/init_level.rs:154` `await_blocks_then_wakes` | body is one call, no assertion, only the already-satisfied fast path |
| F13 | `gc/src/g1.rs:13518` `p89_cross_region_rset_tracking` | *"we just verify no crash"* |
| F14 | `gc/src/g1.rs:13581` `p89_soft_ref_retained_with_free_heap` | ends `let _ = result.stats.soft_refs_cleared;` |
| F15 | `gc/src/heap.rs:2428` `alloc_object_fields_zero_initialized` | three `let _ = heap.get_field(...)` and *"just verify no crash"* |
| F16 | `classloading/src/class_manager.rs:19411` `jvmti_hooks_off_by_default` | the comment explicitly disclaims the assertion the name makes |
| F17 | `jit/src/platform.rs:534` `alloc_zero_returns_some` | `if let Some(ptr) = …` — the failure case is the silent branch |
| F25 | `gc/src/gen_heap.rs:20437`, `:20473` | 20 alloc/collect cycles, then a discarded read |
| F3 | `native-io/src/nio_selector.rs:4385` | the fix is the one-line `0 → i64::MAX` mapping at `:2285`; the test passes `i64::MAX` itself, bypassing it |

F3 belongs here and in §2.2 both: it is a genuine behavioural test of the wrong
entry point. Deleting the mapping restores the 100%-CPU spin it was written for
and the test does not move.

### 2.4 Dark by feature-gate intersection — worse than a weak assertion

**F26.** Nine tests in `vm/src/vm/tests.rs` are double-gated: the file is
`#[cfg(all(test, feature = "synthetic-jdk"))]` and each carries a second gate on
`experimental-debug` (the three `p90_jdwp_*`) or `experimental-aot` (the six
`cds_*`). The two CI steps that could reach them are **disjoint** — one has
`synthetic-jdk` without the experimental features, the other has the
experimental features without `synthetic-jdk`, and the `cargo check
--all-targets` steps split the same way. No job compiles the intersection, so
those nine bodies were never **type-checked**, let alone run. Replacing one with
`panic!()` changes nothing anywhere.

That is the rot mode `.github/workflows/ci.yml` already documents twice in its
own comments, once at the cost of 1,522 tests. **Fixed here**: a dedicated step
compiles and runs the intersection. It is its own step rather than a longer
feature list on the existing one, so that if the intersection does not currently
compile, the failure names itself instead of reading as a regression in the
experimental surface.

**F27.** Five tests in `vm/src/runtime/gpu_residency.rs:122` are gated on
`gpu-offload`, which `ci.yml` excludes ("need hardware") and which
`gpu-selfhosted.yml` does not enable either — that workflow builds with
`gpu-driver`, a different feature, and never runs `cargo test`. The five are
pure in-memory logic with no hardware dependency, so the stated exclusion does
not apply to them. **Recorded, not fixed** — moving them is a placement decision
for someone who owns that surface.

### 2.5 The Java side: a harness filter that deletes the evidence

The largest Java finding is not in any vector. `regression-suite/run.sh:218`:

```sh
extract() { sed 's/\x1b\[[0-9;]*m//g' | grep -aE '^(PASS|CK) ' ; }
```

The cross-VM diff sees **only** lines beginning `PASS ` or `CK `. Three
scheduled vectors print their entire evidence on lines with other prefixes, and
none of the three has a check counter on its `PASS` line — so what survives the
filter is a **constant string**, and the vector reduces to "did `main` reach its
last statement".

| vector | lines printed | lines reaching the diff |
| --- | --- | --- |
| `RForeignLayoutCollections` | 43 | **1** |
| `RDataInputFastPull` | 19 | **1** |
| `RClassUnloadSweep` | 2 | **1** |

All three are **proved by mutation**: a mutant reproducing the defect the vector
was written for changes up to 29 of the observables and leaves the filtered
output byte-identical. `RForeignLayoutCollections` is the sharpest case — it has
zero `check()` calls, and its `safe()` helper turns a thrown `Throwable` into an
`"EXC:…"` string that also lands on a deleted line, so the success path and the
total-failure path produce identical filtered stdout. Its header claims
*"Measured, all 42 lines match HotSpot"*; that measurement was done by hand
once, and the scheduled gate does not repeat it.

`RClassUnloadSweep` is a two-line file whose own comment says *"the suite's
HotSpot diff is the assertion"* — about the exact line the filter deletes. A
CratonVM that never unloads a class at all passes it.

The method was validated with a **positive control**: the same mutation
technique applied to `RJitGc`, which does put its observable on a `CK` line,
changes the filtered output (`mega=437281994736` → `437029049632`) and would go
red. The instrument discriminates.

Three more Java findings, all **proved by mutation**:

* **`RNioNoFollow`** — a `Files.createSymbolicLink` failure prints
  `CK RNioNoFollow symlinks=unavailable`, prints `PASS`, and returns, skipping
  **all 27 checks**. Windows denies symlink creation by default and `run.sh`
  says the suite is usually run from Git Bash on Windows, so on the primary
  platform this vector asserts nothing. Both VMs print the same bail-out line,
  so the diff agrees and the gate is green. The guard is also over-broad: six
  later checks (NOFOLLOW on a regular file, APPEND, CREATE_NEW refusal,
  shorter-write truncation, a 4 MiB round trip, `Files.write(Iterable)`) need no
  symlink at all and are lost with the rest.
* **`RCrypto`** — four of its seven checks are round-trips whose "expected"
  value is produced by the same implementation under test. A JCA provider
  installed at position 1 whose `engineVerify` returns `true` unconditionally
  and whose AES-GCM is the identity function with no tag produces
  **byte-identical** output. `RChaCha20Cipher.java:34` states the principle in
  this very repository — *"a round-trip test cannot catch any of that"* — and
  `RJdkSecurity.java:187-204` implements it correctly. `RCrypto` has no such
  arm.
* **`RJdkX509Intercept:167`** — the negative control is written in the comment
  (*"and must NOT verify against a different one"*) and never in the code. A
  delegating `X509Certificate` subclass whose `verify` is a no-op — i.e. a VM
  that accepts any certificate under any key — produces identical output.

And by reading: `RJdkServices:197` accepts `"none"` where the defect drops the
`ServiceConfigurationError` cause; `RForNameGcStress:135` and
`ROverlaySystemGcStress:193` publish a `churn=(n > 0)` boolean that is `true` on
any VM that executes a loop at all;
`scripts/jdk-only-strict-probes.sh:246` lets a failed agent-jar or JNI build
print `absent` in all three arms, where the arms then agree and the gate passes
with a WARNING — and a WARNING in a green build is how a silently-lost section
stays lost.

**Verified clean, for the record**, because a sweep that only reports hits is not
a measurement: list hygiene in `run.sh` is currently perfect (no unlisted
vector, no list entry without a source, a timeout yields rc=124 and FAILs, a
typo'd `ONLY=` goes red via `ClassNotFound`); no empty `catch` block exists in
any of the 72; all four non-throwing assertion helpers delegate to a `check()`
that throws; and `RJdkSecurity`, `RChaCha20Cipher`, `RJdkPhaser`, `RJdkProxy`,
`RJdkHandles`, `RFieldSiteCache`, `RExecutorShutdown`, `RJdkStampedStamps`,
`RFileTimes`, `RCanAccessRules` and `RJdkModule` were each checked and are
sound.

Similarly on the Rust side: `assert_eq!(x, x)` has **zero** first-party
instances (all 27 hits are in `native-builtins/vendor/rustls-cbc`), as do
`assert!(true)` and `assert!(n >= 0)` on an unsigned type; and the tolerance
shape the brief expected to find is essentially clean — every `abs() <` in scope
is `1e-6` or tighter apart from one appropriate `< 10.0` on a projected growth
percentage and one inside the already-condemned F1.

### 2.6 The second gate nothing was setting

`run.sh:37-41` documents `STRICT_COVERAGE=1` — which makes an unscheduled vector
a failure rather than a warning — and says *"CI should set it"*. No workflow did.
So the next vector to land in no class list would have printed a warning inside
a passing build, which is precisely how `RJdkPhaser` (240 checks) arrived inert.
The same producer/consumer defect as §1.2, in a second place, found by the same
question.

**Fixed here**, and armed on measurement rather than hope: the census is exactly
42 + 28 + 2 = 72 at this commit with nothing unlisted, so switching it on is
green today and red the moment it should be.

---

## 3. What is fixed here, and what is only recorded

**Fixed:** the 7 fixtures and the census ratchet (§1.1); the
`CRATONVM_REQUIRE_E2E` producer, the consumer test and the workflow guard
(§1.2); the gaussian test (§2.2); `STRICT_COVERAGE` (§2.6); the
`synthetic-jdk` × experimental CI step (§2.4); the stale-`.class` trap in seven
harnesses; and the Rust and Java repairs listed in the commit log for this
branch.

**Recorded and not fixed**, with the reason in each case: the 16 remaining
fixtures (§1.1); F27's `gpu-offload` tests (a placement decision for that
surface's owner); the `jdk-only-strict-probes.sh` absent-arm agreement; and the
`RForNameGcStress` / `ROverlaySystemGcStress` constant booleans, which are low
severity because the rest of each `CK` line does discriminate.

**Nothing was deleted or weakened to resolve a finding.** Where a test could not
be made meaningful, it is named here with why.

---

## 4. What would falsify this lane

Four things, in descending order of likelihood.

1. **The restored fixtures fail under CratonVM.** Seven tests that reported `ok`
   in 0.00 s now execute a real probe. Three were run under a CratonVM binary
   and passed; four were verified only on HotSpot. A red on any of them is a
   *finding* — the coverage was absent, and this is what its absence was hiding
   — but it will arrive as a newly-red test rather than a newly-green one, and
   the orchestrator should expect that. The same caveat W6-5 §5 raised about
   `FjpProbe`, now seven times over.
2. **`STRICT_COVERAGE` or the intersection step is red on the first build.** The
   coverage census was measured clean at this commit, so a red there means
   something landed between the measurement and the run. The intersection step
   is the riskier of the two: if those nine double-gated bodies have rotted
   while nothing compiled them, this is the build that says so — which is the
   step's entire purpose, but it will look like this lane broke something.
3. **The Rust findings are proved by READING, not by mutation.** This lane could
   not build. For the §2.2 family the reading is structural rather than a
   judgement call — the mutation target is provably not on the test's call path,
   which is a stronger statement than "this assertion looks weak" — but it is
   not a measurement and is not presented as one. The Java findings, the
   `RJdkViews` verification and the workflow-guard predicate **are** mutation
   measurements, 14 in total.
4. **The `require_e2e_gate` and census tests have never been compiled.** They
   are written against the real helpers and the real tree, but a type error in
   either is a broken build, not a broken test.

---

## 5. The one-line lesson

Round 1 found six by accident and estimated the population at eleven. Asking the
question deliberately — with mutation as the instrument, and with the harness
itself in scope, not just the tests — found 45 and a population of 23. **A
vacuous test is not found by reading tests; it is found by breaking the code
they claim to cover.** The three vectors in §2.5 had been read many times by
people who wrote careful headers about what they measured. What nobody had done
was run them against a broken VM and check that anything changed.
