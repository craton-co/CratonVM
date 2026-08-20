# W7-51 — the vacuous-test population, measured instead of sampled

Status: **OPEN. Round 5 (2026-08-12, lane C9) asked whether this record can be
closed and the answer is no.** Thirteen tests that cannot fail are still in the
tree, verified by symbol at this commit, and §6.3's proposed detector — the
cheap textual tell the expensive sweep produced as a by-product — has been
**measured for the first time and its recall is 3 of the 9 findings it claims to
name**. Read §7 before working from §6.3 or from any "still open" list here.

Status of the original round: **W6-5's two open residuals are closed
structurally**, and a systematic
sweep of the whole test surface found **67 further findings** plus one
population of 23. Round 1 (`W6-5-vacuous-tests.md`) found six by accident, while
reading source for something else. This round asked the question on purpose.

> **2026-08-12 — read §3's box before working from any list in this file.**
> Three of §3's "recorded and not fixed" entries (`RNioNoFollow`, `RCrypto`,
> `RJdkX509Intercept`) are **repaired**; two entries of §2.5's read-only tail are
> **wrong** in a way worth understanding (a locally-weak assertion whose
> observable reaches the cross-VM diff is not vacuous); and §2.5's "verified
> clean, for the record" list is **not a clearance** — a round-3 sweep found the
> species in nine of the fifteen fixtures it covers.

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
| findings | 6 | **67**, plus a population of 23 fixtures / ~286 call sites |
| proved by MUTATION | 4 | **17** |
| fixed here | 4 | 7 fixtures + 33 tests + 3 gates armed |

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

All three are **proved by mutation**, and `RDataInputFastPull` was measured
side by side against its own repair, which is the cleanest evidence in the lane.
The same one-line defect — a typed read that drops the high byte of
`readShort()`, i.e. exactly the partial fast pull the vector exists to catch —
applied to both versions:

| | exit | filtered output |
| --- | --- | --- |
| pre-repair (`HEAD`) | **rc=0** | `PASS RDataInputFastPull` — **byte-identical to the clean run** |
| post-repair | rc=1 | 24 lines &rarr; 2, and `typed.buf1.acc = -8864145947552760507, want -2285467307761758651` |

The broken implementation passed. That is the whole finding, in one run. `RForeignLayoutCollections` is the sharpest case — it has
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

### 2.6 The tolerance species — a bound on a quantity that has no error term

The first sweep pass reported the tolerance surface as "essentially clean",
because every `abs() <` in scope was `1e-6` or tighter and nothing looked
egregious. That reading was wrong, and it was wrong in an instructive way: **a
tolerance is not judged by its magnitude, it is judged against the error term it
absorbs.** Twenty-two sites here have no error term at all — and the widest of
them, at `0.001`, was nowhere near the `1e-12` the first pass went looking for.

The gaussian test is the case that made it legible. Its comment names the gap it
is sized for — *"the JDK's multiplier goes through `StrictMath.log` (fdlibm)
while this uses the platform libm, and those may differ in the last ulp"* — and
then asserts *"a DIFFERENT sequence cannot come within 1e-12 of this one, so the
tolerance costs the assertion nothing."* Both halves are false, and the gap is
measurable. On Temurin jdk-25.0.3+9, over the three accepted pairs of the exact
`new Random(42)` sequence the test pins:

| measurement | value |
| --- | --- |
| worst disagreement between `StrictMath.log(s)` and `Math.log(s)` | **1.000 ulp** |
| worst induced relative error in the returned value | **1.77e-16** |
| the committed tolerance, `1e-12`, expressed in ulps of `g[0]` | **5,143 ulps** |

`sqrt` is IEEE-exact, so `log` is the only source of divergence and the square
root halves whatever it contributes. The tolerance was **~5,650x wider than the
largest disagreement it names**, and ~9,000x wider than a one-ulp justification.

The deeper error is in what the comment treats as noise. `Random.nextGaussian()`
is **specified** in terms of `StrictMath` — a JVM that computes the multiplier
with the platform libm and lands a few ulps away has diverged from the specified
stream, which is a conformance defect, not measurement scatter. The tolerance
was sized to admit exactly the thing it was written about. It is now four ulps
of the expected value: 5x the measured worst case, 1,100x tighter than what it
replaced, and the failure message now says a drift of a few ulps here is a
finding rather than a reason to widen the bound.

**Nine more sites of the same species**, each a value read back out of storage or
a pure bit reinterpretation. There is no arithmetic anywhere on the path, so the
round trip is exact or the slot corrupted it — and a tolerance can only hide the
second case:

| file | what it round-trips | what the tolerance admitted |
| --- | --- | --- |
| `gc/src/heap.rs:3115` | a `Value::Double` reinterpretation | ~5,100 ulps of drift in a value that has been through no arithmetic |
| `vm/tests/t10_9_e_descriptor_aware.rs:156` | the same, in a test literally named `reinterprets_bits` | ~5,100 ulps |
| `vm/src/runtime/value_stack.rs:2672`, `:2664` | `push_double`/`pop_double`, `push_float`/`pop_float` | a slot that narrowed the double to `f32` and back |
| `types/src/compact_value.rs:2260`, `:2232`, `:3043` | the NaN-box round trip | **a box that silently truncated 40 bits of mantissa.** `CompactValue::double` stores `v.to_bits()` verbatim, so the test that proves the box is lossless permitted it to be lossy |
| `gc/src/zgc/page.rs:2241`, `:2303` | `live_ratio()` of 512/512 and 512/8192, both exact in binary | the first test's own comment says *"no threshold short of 1.0 admits it"*, which the tolerance quietly contradicted |

A widened grep (`abs() <`, `EPSILON`, `epsilon` across all first-party crates,
not just the `1e-12` sites the first pass looked at) found **twelve more of the
same round-trip shape**, and the worst of them is in
`vm/src/runtime/jvmti.rs:4352-4378`:

```rust
assert_eq!(env.get_local_int(1, 0, 0).unwrap(), 42);          // exact
assert_eq!(env.get_local_long(1, 0, 1).unwrap(), 123456789);  // exact
assert!((env.get_local_float(1, 0, 2).unwrap() - 3.14).abs() < 0.001);       // 0.03%
assert!((env.get_local_double(1, 0, 3).unwrap() - 2.718281828).abs() < 0.0001);
assert_eq!(env.get_local_object(1, 0, 4).unwrap(), Some(0xDEAD)); // exact
```

Five slots, one round trip, one `insert`-then-read. The int, long and object
slots are asserted exactly; the float and double slots — the same operation on
the same map — get a window three to four orders of magnitude wide, enough to
pass a slot that stored the float as fixed point. Nobody chose that; it is what
happens when a float comparison is written by reflex.

The rest: `vm/src/runtime/value_stack.rs:1879-1880`/`:2408-2409`
(`push_*`/`pop_*`), `vm/src/runtime/frame.rs:2802-2803` (`get_local`, again with
`assert_eq!` neighbours), and `vm/src/native/jni.rs:10522`/`:10537`, which assert
a **varargs ABI round trip** — where any difference at all is a marshalling
defect — with an `f64::EPSILON` window.

All 22 are fixed, compared via `to_bits()` so the assertion is integral and does
not depend on `clippy::float_cmp` policy.

**Checked and left alone**, because a sweep that only reports hits is not a
measurement: `gc/src/zgc/census.rs:2229` (shares summing to 1 — genuine
accumulated floating error, tolerance justified),
`vm/src/bin/bench_hotspot_compare.rs:802` (a geomean ratio), and the two
arithmetic round-trips at `types/src/compact_value.rs:2851`/`:2856` — those
results are exactly representable and the operations are IEEE-exact, but an
operation *is* on the path, so tightening them is a smaller and less clear-cut
win.

### 2.7 The second gate nothing was setting

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

**Fixed:** the three filtered vectors of §2.5 —
`RForeignLayoutCollections` (0 &rarr; 42 observables reaching the diff, plus
local assertions against measured values, mutation-checked with two mutants),
`RDataInputFastPull` (1 &rarr; 22, mutation-checked side by side against its own
pre-repair version) and `RClassUnloadSweep` (kept deliberately diff-only, with
the reason recorded in the file); the two `native-api/src/init_level.rs` tests;
the 7 fixtures and the census ratchet (§1.1); the
`CRATONVM_REQUIRE_E2E` producer, the consumer test and the workflow guard
(§1.2); the gaussian test (§2.2) and all 22 over-wide tolerances (§2.6);
`STRICT_COVERAGE` (§2.7); the
`synthetic-jdk` × experimental CI step (§2.4); the stale-`.class` trap in seven
harnesses; and the Rust and Java repairs listed in the commit log for this
branch.

A note on the repairs themselves: **a constant assertion was written and then
deleted during the `RClassUnloadSweep` fix** — a `rounds.capped=(12 > 0)` line,
which cannot fail. W6-5 §3.4 records the same thing happening while *that*
record was open. Three occurrences now, in three separate lanes, all by authors
actively working on this exact defect class. The reflex is strong enough that
noticing it requires deliberately asking "what would make this line red?" of
every line, including the ones being added to fix the problem.

**Recorded and not fixed**, with the reason in each case: ~~`RNioNoFollow`,
`RCrypto` and `RJdkX509Intercept` from §2.5 (all three specified in full there,
with their mutation evidence, but the repairs are larger than the mechanical
`CK`-prefix ones and were not reached);~~ the 16 remaining fixtures (§1.1); F27's `gpu-offload` tests (a placement decision for that
surface's owner); the `jdk-only-strict-probes.sh` absent-arm agreement; and the
`RForNameGcStress` / `ROverlaySystemGcStress` constant booleans, which are low
severity because the rest of each `CK` line does discriminate; and the two
arithmetic tolerances named at the end of §2.6.

> **2026-08-12 — THIS LIST WAS STALE ON ITS FIRST THREE ENTRIES, and the
> staleness was live long enough to be handed out as pending work.**
> `RNioNoFollow`, `RCrypto` and `RJdkX509Intercept` were all repaired in
> `2969f44be` / `173f38606` and are specified with fresh side-by-side mutation
> evidence in `W7-60-harness-extract-blindness.md` §3 and §4. Verified against
> the tree rather than taken on the commit message: `RNioNoFollow` carries
> `symlinkArms()` / `plainFileArms()` and 39 `check()` sites where the
> `symlinks=unavailable` bail-out used to skip all 27; `RCrypto` carries the
> AES-256-GCM known answer, GCM specification test case 16, nine refusals and
> three signature negatives at 51 `check()` sites; `RJdkX509Intercept` carries
> both negatives — a wrong key derived from the certificate's own SPKI and a
> signature-tampered certificate — at 27. **Do not re-fix them.** This record was
> edited *after* the repairs landed and the sentence was not updated, which is
> the same bookkeeping failure `W6-9`'s §8 heading caused twice; the cost is
> identical, an agent-run spent re-deriving finished work.
>
> **Two entries of §2.5's read-only tail are also wrong, in the instructive
> direction — an assertion that is weak LOCALLY is not vacuous if its observable
> reaches the cross-VM diff.**
>
> * **`RJdkServices:197.** The `kind.equals("ClassNotFoundException") ||
>   kind.equals("none")` disjunction really does accept the defect locally. But
>   `kind` is printed at `RJdkServices:204` as `CK RJdkServices
>   badProviderCause=<kind>`, a `CK` line, so a CratonVM that drops the cause
>   answers `none` where the oracle answers something else and the diff goes red.
>   The residual is real but narrower than recorded: it is vacuous only on a host
>   with no HotSpot, where `run.sh` skips the diff. Tightening it further would
>   need the JDK's own behaviour measured first — modern `ServiceLoader.fail`
>   does not obviously attach a cause — so pinning `ClassNotFoundException`
>   without that measurement risks a red on the ORACLE, which is worse than the
>   loose disjunction.
> * **`RJdkLambdas`'s `bridgeCount >= 1`** is the same shape and is left for the
>   same reason plus one more: the exact bridge count on `StringMapper` is a
>   javac artefact, so pinning it pins the compiler, not the VM. It is on the
>   `CK RJdkLambdas bridges=` line and the diff judges it.
>
> **Still open from this record, unchanged:** the 16 fixtures, F27's placement,
> `jdk-only-strict-probes.sh`, the two `churn=(n > 0)` booleans, and the two
> arithmetic tolerances.
>
> **Round 3 (2026-08-12)** re-asked this record's question of fifteen fixtures it
> had marked sound — including `RJdkModule`, `RJdkProxy` and `RJdkSecurity`,
> which §2.5's "verified clean, for the record" paragraph names explicitly. Nine
> carried the species. The paragraph's method was `!= null`-majority plus
> empty-`catch` plus helper-delegation, and it cannot see the shape that actually
> dominates here: **a single load-bearing assertion that is satisfied by the
> wrong answer, sitting among many that are not.** `RJdkModule` at 155 checks had
> `check(!d.isAutomatic())` against a hardcoded `false` (W2-3, unfixable from the
> fixture); `RJdkFieldModule` asserted `Field.get(null) != null` for `System.out`
> where a fabricated stream passes and identity does not; `RJdkRecords` asserted
> a record accessor's result `!= null` where this VM's recorded failure mode is a
> boxed `0`; `RJdkProxy` asserted `s.getClass() != null`, which cannot fail on
> any VM, to stand for "getClass is not routed to the handler". The count is not
> the instrument; **majority is not the instrument either.**

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

The tolerance family in §2.6 is the same lesson in miniature, and it caught this
sweep out once before it was corrected. The first pass judged tolerances by
their magnitude — `1e-12` looks tight — and pronounced the surface clean. The
right question is not "is this bound small" but **"what error term is this bound
absorbing, and how large is that term?"** Asked that way, `1e-12` on a value
whose only error source is one ulp of `log` is 5,650x too wide, and `1e-12` on a
value read straight back out of a storage slot is infinitely too wide, because
that value has no error term at all. In both cases the bound's own comment
named the thing it was sized to admit. **A tolerance that names its
justification is not thereby justified — the justification has to be measured,
and the measurement takes about ten minutes.**

---

## 6. Round 4 (2026-08-12, doc-only lane) — §3's disposition ledger is incomplete, and twelve of the F-series are still in the tree

**Read this before working from §2.2, §2.3 or §3.**

This round re-derived §2.2's and §2.3's findings **against the working tree**,
by symbol rather than by line number (every line number in those two sections has
drifted, some by 150 lines). It ran no cargo and changed no Rust. The method was
`grep` for the named symbol plus a read of the whole test body, which is a
**verification** for "the assertion is still the one recorded" and only a
**reading** for "reverting the named fix leaves it green" — the mutation was not
executed here, exactly as §4.3 says of the original round.

### 6.1 The bookkeeping defect

§3 has two lists — **Fixed** and **Recorded and not fixed** — and the 27
F-numbered findings of §2.2/§2.3 appear in **neither**, except F2/F12
(`init_level.rs`) and F10. A reader who trusts §3, as the campaign's own
convention says to, concludes that the F-series was disposed of. It was not.
This is the same failure §3's own 2026-08-12 box condemns in its first three
entries, one level up: there the list was stale, here the list is *silent*, and
silence reads as "handled" just as reliably.

Measured disposition of the 27:

| state | count | which |
| --- | --- | --- |
| **fixed** | 6 | F2, F10, F12, F15, F17 (renamed `alloc_zero_is_refused` — *the old name was the lie*), the gaussian worked example |
| **STILL IN THE TREE, verbatim** | 12 | F1, F3, F5, F6, F7, F8, F9, F11, F13, F14, F16, F18–F21, F22–F24, F25 (counting the F18–F21 and F22–F24 clusters as one each) |
| not re-derived here | the rest | F4 (`tzdb.rs`; the cited `:881` is now production code and the test was not located by symbol — treat as unadjudicated, not as clean) |

### 6.2 The twelve, re-anchored — **this is the list of tests that cannot fail**

Line numbers are as of this commit and will drift again; the **symbol** is the
durable anchor.

| # | symbol / file | why it cannot fail | the mutation that should break it and does not |
| --- | --- | --- | --- |
| F1 | `test_secure_gaussian_uses_csprng_and_is_finite`, `native-builtins/src/securerandom.rs:1916` | the body declares its own `secure_uniform` closure and runs the polar transform locally; `native_secure_random_next_gaussian` is never called | repoint `SecureRandom.nextGaussian` at `java.util.Random`'s LCG — the named defect |
| F3 | `nio_selector_indefinite_block_path_honors_wakeup`, `native-io/src/nio_selector.rs:4510` | the fix is the `0 → i64::MAX` mapping inside `selector_select_native`; the test calls the low-level `selector_select(id, i64::MAX)` and never crosses the mapping | delete the mapping — the 100%-CPU `select(0)` spin returns, the test does not move |
| F5 | `path_rejects_parent_segment`, `native-builtins/src/jboss_resource_loader.rs:200` | the body iterates a literal `Path` looking for `..`; its own comment says *"we duplicate the check here"* | remove the traversal guard from `native_create_resource_loader` — **a security control** |
| F6 | the timeout arm at `native-builtins/src/ironjacamar_pool.rs:573` | the test **re-writes the classification `if`** over a hard-coded string and asserts its own result | delete the classification from `native_pool_get_connection` |
| F7 | `wp3_6_max_direct_transfer_size_is_int_max`, `native-io/src/file_channel.rs:1899` | the entire body is `assert_eq!(0x7fff_ffff_i32, i32::MAX)` | make `maxDirectTransferSize0` return 0, or unregister it |
| F8 | `system_init_phase1_is_registered_as_native`, `vm/tests/wave3_scanner.rs:44` | asserts `find(...).is_some()`; **a no-op stub is registered too**, and its own failure message names a *behaviour* (*"the pre-S110 no-op stub left `System.in` null"*) | restore the no-op stub — registration is unchanged, the test is green, `System.in` is null |
| F9 | `zip_comment_round_trips`, `native-io/src/zip_real_jar.rs:1459` | round-trips a comment through the third-party `zip` crate; the native is not on the call path | make `native_jarfile_get_comment` return null unconditionally |
| F11 | `rg9_tiered_manager_exists`, `jit/src/lib.rs:26921` | body is one `let _manager = …` line; the doc promises the hot-counter threshold is asserted | change the default policy's threshold to anything |
| F13 | `p89_cross_region_rset_tracking`, `gc/src/g1.rs:~13606` | ends on the comment *"we just verify no crash"* — no assertion | make `write_barrier` record nothing |
| F14 | `p89_soft_ref_retained_with_free_heap`, `gc/src/g1.rs:~13668` | ends `let _ = result.stats.soft_refs_cleared;` | clear every soft ref regardless of heap pressure |
| F16 | `jvmti_hooks_off_by_default`, `classloading/src/class_manager.rs:20224` | body is two `fire_*` calls; the comment **explicitly disclaims** the assertion the name makes (*"may be either true … or false"*) | fire the hooks when none are installed |
| F18–F21 | `values_equal_null_null` / `_ints` / `_long` / `_mixed_types`, `native-collections/src/lib.rs:59044–59083` | **worse than recorded.** They do not merely fail to call `values_equal` — each asserts a `matches!` pattern against a `Value` literal it constructed two lines earlier, e.g. `matches!((&Value::Object(None), &Value::Object(None)), (Value::Object(None), Value::Object(None)))`. These are tautologies over constants | rewrite `values_equal` to `true` — or delete it — all four stay green |
| F22–F24 | `test_serialization_not_supported_returns_err` and neighbours, `native-builtins/src/serialization.rs:5910` | constructs a `RuntimeError::UnsupportedOperationException` in the test and asserts it `matches!` itself; the comment states the method (*"we cannot call it directly without a `NativeContext`, but we can verify the error type by constructing the same `RuntimeError`"*) | make the not-supported paths return `Ok(None)` |
| F25 | the two loops at `gc/src/gen_heap.rs:20497`, `:20548` | 20 alloc/collect cycles, then `for … { let _ = heap.get_field(*obj_ref, 0); }` under the comment *"should be readable without panicking"* | return a garbage `Value` from every surviving slot |

Also still open and unchanged from §3's list, re-verified: the two `churn=(n > 0)`
booleans (`RForNameGcStress.java:135`, `ROverlaySystemGcStress.java:193`, both
verbatim), the two arithmetic tolerances (`types/src/compact_value.rs:2852`,
`:2857`), F27's `gpu-offload` placement (`vm/src/runtime/gpu_residency.rs:8`
onward, still the only gate), and the `jdk-only-strict-probes.sh` absent-arm
agreement — **whose source comment now argues the shape is correct**
(*"the agent section will report absent in EVERY arm, so the arms still agree
and the gate stays honest"*, `scripts/jdk-only-strict-probes.sh:~252`). It is
not honest: three arms that all failed to build agree with each other, and
agreement between three broken instruments is this record's entire subject. The
residual is now harder to close than when it was merely unnoticed, because a
reader has to disagree with a comment first.

**Round 3's four Java findings ARE repaired**, verified in the tree rather than
taken from the commit log: `RJdkRecords.java:113` now carries an explicit
*"NOT `!= null`"* comment, `RJdkFieldModule.java:186` asserts **IDENTITY**,
`RJdkModule.java:99` cross-checks `isAutomatic()` against
`modifiers().contains(AUTOMATIC)` beside the hardcoded-`false` note, and no
`getClass() != null` remains in any of the 72 vectors.

### 6.3 The sweep of 16,376 missed instances of its own named tell

§2.2 names the give-away precisely — a comment of the form *"we can't call the
native without a `NativeContext`, so we replicate the check here"*. Grepping
**that sentence** across the eight first-party crates (excluding `vendor/`)
returns 16 hits in ~10 seconds. It names every one of F1, F5, F6, F7, F9,
F18–F21 and F22–F24 — and at least one the sweep did not report:

* **`t10_9_a_adapter_preserves_empty_dispatch`, `vm/src/runtime/vtable.rs:2031`.**
  The doc comment states the property under test: *"when the install adapter
  sees a `VtableSlotDescriptor` with `dispatch: None`, the resulting
  `VtableEntry` has `resolved_method: None`"*. The body constructs the
  descriptor, then asserts `desc.dispatch.is_none()` and
  `desc.method_index == 3` — **the two fields it set two lines above**.
  `vtable_install_adapter` is never called, and its comment says why: *"We can't
  call `vtable_install_adapter` without a global manager; simulate the
  conversion manually."* **Mutation that should break it and does not:** make
  the adapter populate `resolved_method: Some(..)` for a `dispatch: None`
  descriptor — i.e. dispatch an abstract method to a body.

The lesson is not that the sweep was careless; 16,376 is a lot of tests. It is
that **once a species has a textual tell, the tell is a cheaper and more complete
instrument than the sweep that discovered it**, and it was never run. This is
W7-60's lesson (subtract the filter from the oracle) arriving from the other
direction: the expensive census produced a cheap detector as a by-product and
nobody consumed it.

### 6.4 Is this record's evidence scheduled?

Partly, and the split matters.

* The **Rust** F-series is reachable by `cargo test --workspace`, which
  `.github/workflows/ci.yml` runs. Those tests execute — they simply cannot go
  red. Scheduling was never the problem here; discrimination is.
* The **Java** side is scheduled: all 72 vectors go through
  `regression-suite/run.sh`, `STRICT_COVERAGE: 1` is set at `ci.yml:1350` and
  `CRATONVM_REQUIRE_E2E: 1` at `:211` — §1.2's and §2.7's producers, both
  verified present.
* **`probes/` is scheduled by essentially nothing, and the number is worse than
  "not by `run.sh`".** The string `probes` appears **zero** times in
  `regression-suite/run.sh`. The one scheduled consumer is
  `scripts/jdk-only-strict-probes.sh` (`ci.yml:315` and `:1404`), and its
  `PROBE_LIST` default is exactly three names —
  `JdkOnlyCensusLoadProbe JdkOnlyBreadthProbe JdkOnlyPlatformProbe` — plus
  `JdkOnlyProbeAgent`. There are **449 `.java` files in `probes/`**. So ~1% of
  the probe corpus is scheduled, and **no suite run at any `SUITE=` value can
  discharge a finding whose only witness is a probe** — which is the standing
  caveat for W7-33 and W7-36, whose entire evidence base is
  `probes/ShadowDifferentialProbe.java`. That file is named by no `.sh` and no
  `.yml` in the tree.

### 6.5 What round 4 did NOT do

No cargo, no Rust, no mutation executed. Every row in §6.2 is a **reading** of a
test body against the production symbol it names — strong for the twelve where
the production function is provably absent from the call path (that is a
structural fact, not a judgement), and weaker for F13/F14/F16/F25, where the
claim is only that **no assertion exists**. That one needs no mutation: a test
with no assertion cannot fail, and F13, F14, F16 and F25 have none.

---

## 7. Round 5 (2026-08-12, lane C9) — can this record be closed? No, and the reason is one level up again

**Method.** Read-only. Every §6.2 symbol re-derived against the working tree by
`git grep -l "fn <symbol>"` — symbol, not line number, since §6 already
established that every line number in §2.2/§2.3 has drifted. Then the thing
§6.3 asked for and nobody did: **the detector was executed.** No cargo, and no
Rust changes to any file this lane does not own.

### 7.1 The thirteen are still there — verified, not assumed

All twelve symbols of §6.2, plus §6.3's `vtable.rs` addition, resolve at this
commit, each in the file §6 names:

```
test_secure_gaussian_uses_csprng_and_is_finite       native-builtins/src/securerandom.rs
nio_selector_indefinite_block_path_honors_wakeup     native-io/src/nio_selector.rs
path_rejects_parent_segment                          native-builtins/src/jboss_resource_loader.rs
wp3_6_max_direct_transfer_size_is_int_max            native-io/src/file_channel.rs
system_init_phase1_is_registered_as_native           vm/tests/wave3_scanner.rs
zip_comment_round_trips                              native-io/src/zip_real_jar.rs
rg9_tiered_manager_exists                            jit/src/lib.rs
p89_cross_region_rset_tracking                       gc/src/g1.rs
p89_soft_ref_retained_with_free_heap                 gc/src/g1.rs
jvmti_hooks_off_by_default                           classloading/src/class_manager.rs
values_equal_null_null / _ints / _long / _mixed_types  native-collections/src/lib.rs
test_serialization_not_supported_returns_err         native-builtins/src/serialization.rs
t10_9_a_adapter_preserves_empty_dispatch             vm/src/runtime/vtable.rs
```

`alloc_zero_returns_some` resolves nowhere and `alloc_zero_is_refused` resolves
in `jit/src/platform.rs` — §6.1's "renamed, *the old name was the lie*" row is
confirmed. F27's `gpu-offload` gate, both `churn=(n > 0)` booleans
(`RForNameGcStress.java:135`, `ROverlaySystemGcStress.java:193`), the two
arithmetic tolerances (`types/src/compact_value.rs:2852`, `:2857`) and the
`jdk-only-strict-probes.sh` absent-arm agreement (`:376`, `:402`) are all
verbatim. **Nothing on this record's open list has moved.**

### 7.2 The finding: §6.3's detector was never run, and it does not work

§6.3's whole point is that the expensive census produced a cheap detector as a
by-product and nobody consumed it. It then states that detector's recall as a
fact:

> Grepping **that sentence** across the eight first-party crates (excluding
> `vendor/`) returns 16 hits in ~10 seconds. It names every one of F1, F5, F6,
> F7, F9, F18–F21 and F22–F24 — and at least one the sweep did not report
> [`t10_9_a_adapter_preserves_empty_dispatch`].

Executed here for the first time. The hit COUNT reproduces — a widened form of
the tell returns 16 hits across the first-party crates in about ten seconds.
**What those hits NAME does not.** The exact phrase occurs in four files:

| file | finding it names |
| --- | --- |
| `native-builtins/src/securerandom.rs:1815` | F1 |
| `native-builtins/src/serialization.rs:5913` | F22–F24 |
| `native-io/src/file_channel.rs:1825` | F7 |
| `native-builtins/src/xnio_io_thread.rs:1458` | not an F-finding |

and in **none** of `jboss_resource_loader.rs` (F5), `ironjacamar_pool.rs` (F6),
`zip_real_jar.rs` (F9), `native-collections/src/lib.rs` (F18–F21),
`nio_selector.rs` (F3), `wave3_scanner.rs` (F8), `jit/src/lib.rs` (F11),
`g1.rs` (F13/F14), `class_manager.rs` (F16), `gen_heap.rs` (F25) — **or
`vtable.rs`, the one case §6.3 offers as its own demonstration.** Measured
recall: **3 of the 9 findings claimed, and 0 of the 2 it is demonstrated on.**

**Why, and this is the durable lesson.** The two sentences the detector keys on
are gone from the tree:

```
$ git grep -c "we duplicate the check here"      -- '*.rs'    ->  (no hits)
$ git grep -c "simulate the conversion manually" -- '*.rs'    ->  (no hits)
```

Both were replaced by the round-3/4 annotation pass. `jboss_resource_loader.rs`
now opens its test with `**THIS TEST CANNOT FAIL, and it is standing in for a
SECURITY control.**`, and `vtable.rs` with `**THIS TEST CANNOT FAIL, and its
name overstates what it covers.**` — strictly better prose, written by someone
fixing this exact defect, which **destroyed the detector's recall as a side
effect.** A detector keyed on prose is invalidated by improving the prose.

So §6.3's conclusion survives and its instrument does not: the census did
produce a cheap detector, nobody ran it, and by the time anyone did, the repair
pass had disarmed it. **A detector whose recall is asserted rather than measured
is the same species this record is about — a check that reads as good news
because nothing ever made it speak.** That is the third occurrence inside this
record's own repairs (§3's deleted `rounds.capped=(12 > 0)`, W6-5 §3.4, here).

### 7.3 The repair: a stable marker plus a positive control

Prose is the wrong key. Two sites have already independently converged on a
literal, stable one — `THIS TEST CANNOT FAIL` — and it is greppable, but it is
on **2 of the 13**:

```
$ git grep -c "THIS TEST CANNOT FAIL" -- '*.rs' ':!*/vendor/*'
native-builtins/src/jboss_resource_loader.rs:1
vm/src/runtime/vtable.rs:1
```

**NOMINATION (this lane owns neither file).** Adopt that literal as the campaign
marker, put it on all thirteen, and gate it — with the gate's **positive control
built in**, which is the half this campaign keeps omitting:

* `vm/tests/vacuous_marker_census.rs`, reached by the `cargo test --workspace`
  step `.github/workflows/ci.yml` already runs. It carries the thirteen
  `(symbol, file)` pairs of §7.1 and asserts, for each, that the file contains
  both the symbol and the marker.
* **The positive control, which is the entire point:** the census must also
  assert `found >= 13`. A gate written only as "no unmarked vacuous test exists"
  passes identically when the search matched nothing at all — a wrong path, a
  renamed crate, a `ripgrep` that is not installed, a stray `|| true`. All three
  of this campaign's calibrated instances of that failure — the CI gate whose
  `|| true` made "clean tree" and "search failed" identical, the mutation test
  written against a `ripgrep` branch on a host with no `ripgrep`, and the
  `ArrayDeque` negative control that drove only the ends of the deque and so
  never called the method under test — are one defect: **the machinery's own
  silence read as a pass.** An assertion with a positive floor cannot be
  satisfied by silence.
* Clearing a row is what makes the test green again — the ratchet shape §1.1
  established and the reason its `probe_fixture_census.rs` works. A marker
  removed because the test was genuinely repaired is a **deletion from the
  table, in the same commit**, or the gate stays red.

This does not repair the thirteen. It makes their number honest and makes a
silent failure of the accounting loud — the strictly smaller claim this record
is entitled to make without a build.

### 7.4 Disposition — why W7-51 cannot be closed

| blocker | state at this commit | measured how |
| --- | --- | --- |
| the thirteen tests that cannot fail | **all present, verbatim** | symbol grep, §7.1 |
| §6.3's detector | **recall 3/9, disarmed by the annotation pass** | executed, §7.2 |
| 11 of the 13 carry no greppable marker | **unmarked** | §7.3 |
| the 16 missing `apps/` fixtures (§1.1) | unchanged | §1.1's own baseline |
| F27 `gpu-offload` placement | unchanged | `gpu_residency.rs:8` |
| `jdk-only-strict-probes.sh` absent-arm agreement | unchanged, **and still argued for in its own comments** | `:376`, `:402` |
| the two `churn=(n > 0)` booleans | verbatim | §7.1 |
| the two arithmetic tolerances | verbatim | §7.1 |

**Closing this record requires a build, and this lane could not run one.** Every
row above is a grep or a reading against the tree; none is a mutation, and
§4.3's caveat still governs. The honest position: W7-51 is a correct and
unusually well-evidenced description of a population that has now been
re-measured four times and repaired twice, and **both repairs were
documentation.** What it is still missing is the thing it names in its own §5 —
*a vacuous test is not found by reading tests; it is found by breaking the code
they claim to cover* — applied to the thirteen, one `cargo test` at a time.

**One thing round 5 asks the next lane NOT to do:** do not re-derive §6.2. It
has now been verified against the tree twice, by symbol, and both passes found
the same list unchanged. A third reading buys nothing. Land §7.3's gate, then
break one of the thirteen and watch it stay green — that is the only evidence
this record does not already have.
