# Undocumented blanket `org/junit/` ban (+3 siblings) — CLOSED 2026-07-27: root-caused to a JIT code-buffer overflow panic, fixed, all four bans removed

**Status: CLOSED.** All four bans are gone from `vm/src/jit/skip_list.rs`. What
they were hiding turned out not to be a miscompile at all, but a missing
bail-out in the IR backend — now fixed, with its own regression witness.

The original filing and the 2026-07-26 investigation are preserved verbatim
below the closure section, because their reasoning is what made this answer
findable. In particular, the standing caution that a Hibernate-only sample
could not rule out an Elasticsearch-specific trigger was exactly right.

---

## Closure summary (2026-07-27)

### What the bans were hiding: `org/junit/internal/MethodSorter`

Lifting the four bans and running a real Elasticsearch corpus aborted **58 of
60** test classes. Every abort was the same panic at the same code offset:

```
thread 'SUITE-…' panicked at jit/src/ir_lower.rs:2740:18:
call-exc JE patch in-bounds: PatchFailed { kind: "i32", offset: 4382 }
fatal runtime error: failed to initiate panic, error 5, aborting
```

Bisection narrowed it to one package, then one class:

| JIT-eligible scope | result |
|---|---|
| all four packages | rc=134, abort |
| `org/junit/` alone | rc=134, abort |
| `junit/` alone | clean |
| `org/apache/logging/log4j/` alone | clean |
| `com/carrotsearch/randomizedtesting/` alone | clean |
| inside `org/junit/`: `runner`, `runners`, `rules`, `validator`, `experimental`, `internal/{runners,builders,matchers,requests,management}`, and every other `org/junit/internal/*` class | clean |
| **`org/junit/internal/MethodSorter`** | **rc=134, abort** |

Three of the four bans never guarded anything. The fourth guarded one class.

### The actual bug

`ExecutableBuffer::emit` is non-panicking by design: on capacity exhaustion it
sets a sticky `overflowed` flag and DROPS the write. `ir_lower::lower_inner` is
built around that contract — near its end,
`if buf.overflowed() { return None }` discards the artifact and the caller falls
back to the single-pass backend.

Thirteen rel32 patch sites in `jit/src/ir_lower.rs` used
`.expect("… patch in-bounds")`. Once the buffer overflowed, recorded patch
offsets no longer addressed their placeholder bytes, `try_patch_i32` returned
`Err` as documented — and the `expect` panicked the compile thread before
`lower_inner` could reach its bail. In a release VM that is not a caught panic;
it is `fatal runtime error: failed to initiate panic` → **SIGABRT of the whole
process**.

This was a missed conversion, not a design question:

- `jit/src/x64.rs` has always used `.ok()` at every equivalent site
  (`// on Err try_patch_i32 set buf.overflowed; compile bails`).
- `ir_lower.rs`'s own `patch_rel32_to_here` already documents the rule:
  *"an `expect` here would turn a recoverable 'fall back to single-pass' into a
  compile-thread panic."*

`MethodSorter` simply lowers to a graph that exceeds `lower_inner`'s buffer
estimate (`nodes*32 + call_nodes*448 + 1024`, floor 4096). Nothing else in the
four banned packages did — which is why Hibernate and Spring Boot never tripped
it, and why the 2026-07-26 Hibernate sample came back byte-for-byte clean.

### The fix

All thirteen sites now use `.ok()`. An overflowed buffer discards the IR
artifact and falls back to single-pass instead of aborting the process.

Regression witness: `no_patch_site_panics_on_an_overflowed_buffer` in
`jit/src/ir_lower.rs`'s test module asserts no patch site in the file uses
`expect`/`unwrap`. A behavioural test cannot reach these emitters without a
graph large enough to overflow `lower_inner`'s own estimate — exactly the
condition that estimate exists to prevent — so the invariant is asserted where
it can actually be checked.

Confirmed directly: with `BISECT_ONLY=org/junit/internal/MethodSorter` and
`ALLOW_PACKAGES=org/junit/`, the command that used to abort now returns rc=0
with **0 panics and 11 `marking buffer overflowed` warnings** — the overflow
still happens, and is now handled.

### This also fixes 7 pre-existing Elasticsearch aborts on DEFAULT settings

The fix is not merely an enabler for the ban removal. In the pre-fix 60-class ES
run with the bans ACTIVE and no env vars set — i.e. the shipping configuration —
**7 of 60 classes SIGABRTed**, all at the same class of bug (6 at
`ir_lower.rs:638`, the IR safepoint-poll patch; 1 at `:2371`, a codegen patch).
After the fix that run has **0 aborts**: 46 pass, 13 pre-existing failures,
1 timeout.

**Known follow-up, not a correctness issue:** `MethodSorter` therefore never
gets an IR-compiled body; it silently falls back to single-pass. Raising
`lower_inner`'s buffer estimate for this shape is a throughput opportunity, not
a bug.

### Evidence that removing the bans is safe

All on ONE binary (the fixed one), each pair run with both legs CONCURRENT so
they see the same shared-host load, seeded class lists, results normalised to
drop timings.

**Realistic pair** — default vs the four bans lifted:

| suite | sample | result |
|---|---|---|
| Elasticsearch (`es-fixture-ivfknn-slicesdense-closure-20260717`, 2571 classes) | 60, seeded | **byte-for-byte identical, 60/60** |
| Hibernate ORM 8.0 (`hib-suite-runner`, JUnit5 Platform Launcher, 4003 classes) | 160, seeded (2× the 2026-07-26 sample) | **byte-for-byte identical, 160/160** |
| Spring Boot `core/spring-boot` | 40, seeded | **byte-for-byte identical, 40/40** |

**Targeted pair** — the decisive engagement test.
`CRATONVM_JIT_BISECT_ONLY=<the four packages>` + `THRESHOLD=1`, once WITHOUT and
once WITH the packages allowed. The control JIT-compiles *literally nothing*
(the four packages are the only JIT-eligible ones and they are still banned);
the test compiles *only* these four packages, on the first invocation of every
method. Any difference is attributable to JIT-compiling exactly the code these
bans covered — so this is not another shadowed no-op.

- Spring Boot: **40/40 identical.**
- Elasticsearch: **30/30 identical** but for `NodeConnectionsServiceTests`'
  failure COUNT (2 vs 1). Eight repeat runs in the CONTROL config alone gave
  2,2,2,2,1,2,1,1 — the class is flaky in itself with the config held constant,
  and the test config gave the same distribution (2,2,2,1,2,2,2,1).

Two methodological notes worth keeping:

- The Hibernate legs initially showed differences that were all
  `TimeoutException … timed out after 120 seconds`, in BOTH directions (baseline
  failing one class, lifted failing another), under host load average 80+. That
  is the harness's `junit.jupiter.execution.timeout.default`, not a JIT effect.
  Re-running with `-Djunit.jupiter.execution.timeout.default=900s` removed the
  noise entirely and produced the 160/160 identical result above.
- Symmetric timeouts are fine: e.g. `ComposableIndexTemplateTests` hit the
  harness's own 420s per-class cap in both ES legs, and Spring Boot's
  `ConfigurationPropertySourcesTests` hangs identically in every config. Those
  are pre-existing and diff away.

### Residuals closed at the same time

1. **`junit/textui/TestRunner.main`** — the one class under the blanket `junit/`
   ban with its own documented crash history (the 2026-05-28 "bc-math-ec JUnit-3
   AllTests SEGV" entry in `is_known_miscompile`). That entry is gated behind
   `callee_saved_gpr_local_homes_enabled()`, which is default-OFF, so it was
   dead code and the blanket ban was the only thing keeping the method
   interpreted. Removing the blanket ban makes `TestRunner.main` JIT-eligible by
   default for the first time. `JUnit3TextUiRunnerProbe.java` drives it one call
   per process with `CRATONVM_JIT_THRESHOLD=1` (so `main` compiles on its single
   invocation) under allocation pressure that forces several minor GCs while the
   JIT'd frame is live — the exact window the original SEGV opened in.
   60 runs baseline + 60 lifted on the pre-fix binary: **120/120 clean, 0
   crashes.** Re-run on the fixed binary at 40+40: 78/80, the two misses being
   the same symmetric 180s harness timeout in each leg under load, no crashes.

2. **JUNIT.1** — its 2026-07-26 removal was recorded as a shadowed no-op because
   `JUnitCoreMainProbe`'s 110-run retest never set
   `CRATONVM_JIT_ALLOW_PACKAGES`. Re-run properly with the shadow lifted: three
   test shapes (trivial pass, heavy-allocation pass, intentional fail) ×
   baseline/lifted × 60 runs on the pre-fix binary and 40 on the fixed one —
   **0 wrong exit codes, 0 crashes**, including the failing shape correctly
   still exiting 1. The comment in `skip_list.rs` is updated from "overclaim" to
   "verified".

3. **TOMCAT-DOHEAD-JUNIT-ITERATOR.1** — was already correctly re-verified
   against the unshadowed condition on 2026-07-26. Its comment and unit test now
   say so in the past tense, and the test asserts JIT-eligibility under BOTH
   policies with no allow-list needed.

### Two narrower bans are now the live gates (deliberately kept)

- **PIC.1**, `org/junit/platform/console/shadow/picocli/` — keeps its own
  documented SEGV evidence. Note that `ALLOW_PACKAGES=org/junit/` also lifts
  PIC.1 (`package_allowed` prefix-matches), so every "lifted" leg measured above
  was a strict SUPERSET of this removal: the shipping default still has PIC.1
  active and is therefore no less conservative than what was measured.
- **`("junit/textui/TestRunner", "main")`** in `is_known_miscompile` — still
  listed, still gated behind the default-off GPR-local-homes flag, now probed
  directly as described above.

### How to reproduce — and the flag trap that cost a round

`CRATONVM_JIT_ALLOW_PACKAGES` **cannot narrow within a banned package**.
`package_allowed(prefix, allow_packages)` tests `prefix.starts_with(entry)` —
the ban's own prefix must start with the entry — so an entry has to be a
BROADER-or-equal prefix than the ban. `CRATONVM_JIT_ALLOW_PACKAGES=org/junit/runner/`
does NOT lift the `org/junit/` ban; that run stays fully interpreted and reports
a false clean. (The same shadowing trap this doc was originally filed about, in
a different disguise.)

To narrow inside a package, combine the two flags — `BISECT_ONLY` is applied
earlier and forces every class NOT matching one of its prefixes to skip the JIT,
while `ALLOW_PACKAGES` lifts the ban itself:

```bash
CRATONVM_JIT_ALLOW_PACKAGES=org/junit/ \
CRATONVM_JIT_BISECT_ONLY=org/junit/internal/MethodSorter \
  <cratonvm> --java-home /home/victor/jdk25 -Dtests.seed=DEADBEEFDEADBEEF \
  -Dtests.asserts=false -Des.path.home=$ES -Djava.awt.headless=true \
  -cp "$(cat /data/tmp/es-full-cp-single-line.txt)" \
  org.junit.runner.JUnitCore org.elasticsearch.cluster.metadata.ClusterNameExpressionResolverTests
```

Harness scripts used for the A/B matrices (Azure build host, not committed):
`/data/tmp/{run_es_ab,run_hib_ab,run_sb_ab,drive_pair,run_probes,flaky_check}.sh`,
seeded lists `/data/tmp/{es-junitban-60,hib-junitban-160,sb-junitban-40}.txt`
regenerable via `/data/tmp/gen_es_sample.py`.

---

## Original filing and 2026-07-26 investigation (preserved verbatim)

**Status: still active, NOT removed, NOT further investigated this session — flagged for a dedicated future session.**

## What was found

While re-testing `TOMCAT-DOHEAD-JUNIT-ITERATOR.1`'s removal, discovered that
`vm/src/jit/skip_list.rs` has a separate, much broader blanket ban on the
**entire `org/junit/` package**:

```rust
if class_name.starts_with("org/junit/") && !package_allowed("org/junit/", allow_packages) {
    return Some(SkipReason::RustJvmTestFixture);
}
```

located a few hundred lines below `should_skip_jit_internal`'s start,
grouped alongside similar blanket bans for `net/bytebuddy/`,
`com/carrotsearch/randomizedtesting/`, and `org/apache/logging/log4j/` —
but unlike every one of those neighbors, **this one has no explanatory
comment of its own** describing why it exists, what miscompile it guards
against, or when it was added.

**It is gated entirely inside `if policy == SkipPolicy::Conservative { ... }`**
(opened well above it) — so it only applies under the default Conservative
policy; `SkipPolicy::Aggressive` bypasses it (and its neighboring blanket
bans) unconditionally, same as the rest of that block.

**It is liftable without a rebuild**: `package_allowed("org/junit/",
allow_packages)` returns true whenever any entry in `allow_packages`
prefix-matches the literal string `"org/junit/"` — i.e.
`CRATONVM_JIT_ALLOW_PACKAGES=org/junit/` (or `CRATONVM_JIT=allow-packages`
in the newer flag spelling from today's JIT rework) lifts it at runtime.

## Where it came from

`git log -S'class_name.starts_with("org/junit/")' -- vm/src/jit/skip_list.rs`
shows exactly one commit ever touched this literal string:
`60ef90d4b`, dated 2026-07-05, titled **"Fix Elasticsearch postings FFM
checksum bridges"** — a large, generically-named commit touching 14 files
across `native-builtins`, `native-collections`, `native-io`, and
`vm/src/jit/skip_list.rs` (56 lines added there). The `org/junit/` ban is
almost certainly incidental collateral in that squash rather than a
deliberately, individually-justified addition — it rode in silently next
to genuinely-documented bans (`net/bytebuddy/`, log4j, randomizedtesting)
without picking up its own rationale comment.

## Why this matters

This ban silently shadows **any narrower, more specific ban on a class
under `org/junit/`** — meaning re-testing such a narrower ban without also
explicitly lifting this blanket one produces a false-clean result: the
probe runs, reports success, but the target method was never actually
JIT-compiled at all (it was still fully interpreted via this blanket
ban the whole time).

This exact trap caught two things this session:

1. **`TOMCAT-DOHEAD-JUNIT-ITERATOR.1`'s first re-test attempt** (this
   session, same day) — initially "confirmed clean" without
   `CRATONVM_JIT_ALLOW_PACKAGES=org/junit/` set, which proved nothing.
   Caught before landing; re-tested properly with the blanket ban also
   lifted (via `check_with`/env var), genuinely confirmed clean, and
   removed correctly this time — see the removal comment in
   `skip_list.rs` and `TomcatDoheadJunitIteratorProbe.java`.
2. **`JUNIT.1`'s earlier removal** (this session, already landed on `dev`
   before this was discovered) — its removal comment claimed
   "`JuintCore.main` is JIT-eligible unconditionally now," which is false:
   `org/junit/runner/JUnitCore` is also caught by this same blanket ban,
   and `JUnitCoreMainProbe.java`'s 110-run retest never set the
   allow-packages env var either. The comment has been corrected in
   `skip_list.rs` to accurately describe this as a safe-but-shadowed
   no-op removal (like `SPRINGBOOT-WITHOUT-JACKSON.2` and `HIB-ANTLR.1`),
   not an independently-verified unconditional fix.

## Recommendation for a future session

This ban is a high-value, unexplored target precisely because of its
scope: if it can be safely narrowed or removed, it restores real JIT
eligibility to the **entire JUnit test-running harness** across every
JUnit-based test suite this VM runs (Spring, Hibernate, Tomcat, and
everything else that uses JUnit4/JUnit5 internally) — a much bigger win
than any single narrow ban in this file.

Suggested approach:
1. Run a broad, real JUnit-based test corpus with
   `CRATONVM_JIT_ALLOW_PACKAGES=org/junit/` set — e.g. the Hibernate ORM
   harness at `apps/hibernate-orm-harness/` (JUnit5 Platform Launcher,
   hundreds of real test methods) or a Spring suite — and compare pass/
   fail counts and wall-clock behavior (hangs, OOMs, wrong results)
   against the same run without the env var.
2. If clean, bisect which specific `org/junit/` classes/methods (if any)
   still need a narrower, targeted ban, following this file's established
   pattern (see how `HIB-LONGTAIL.1`, `SPB.1`, etc. narrowed from an
   initial blanket ban to specific classes over time).
3. Do not assume "no regressions in a quick probe" is sufficient — this
   ban's total absence of a rationale comment means nobody currently
   knows what it was protecting against, so a real, broad test corpus
   (not a synthetic probe) is the right bar for removing or narrowing it.

## Update 2026-07-26 (later same day): 80-class real Hibernate sample — clean, no regressions with the blanket ban lifted

Ran an 80-class random sample (seeded, reproducible — `gen_hib_test_list.py`,
list at `/data/tmp/hib-org-junit-test-sample.txt`, drawn from ~4000 real
`*Test.class` files) from the real Hibernate ORM 8.0 harness at
`apps/hibernate-orm-harness/` through `hib-suite-runner/CratonRunner.java`
(JUnit5 Platform Launcher), comparing:

1. **Baseline** (blanket `org/junit/` ban active, current default): all 80
   classes ran to completion. Exactly one real failure
   (`InsertOrderingReferenceSeveralDifferentSubclassTest`,
   `org.opentest4j.AssertionFailedError`) and one aborted
   (`FinalEmbeddableFieldTest`, 5/6 ok); everything else fully passed or
   was cleanly skipped (dialect-gated tests for databases not configured
   on this host, e.g. HANA/Postgres-specific tests).
2. **Ban lifted** (`CRATONVM_JIT_ALLOW_PACKAGES=org/junit/` set, same
   binary, same 80-class list): `diff` of the two runs' `@@RESULT` lines
   (timing stripped) is **byte-for-byte empty** — identical found/started/
   ok/failed/aborted/skipped counts for every one of the 80 classes,
   including the same single pre-existing failure and the same single
   abort.

**This is a genuinely clean result**: JIT-compiling `org/junit/` classes
(actually exercising them, not just a shadowed no-op — this ban gates the
whole package, so lifting it engages real JIT compilation across whatever
`org/junit/` code this test run exercises) produced zero new failures,
hangs, or crashes across a real, diverse 80-class Hibernate test sample.

**Not sufficient to remove the ban outright.** 80 classes is a sample of
roughly 2% of the ~4000 real test classes available in just this one
fixture, and this ban's total lack of a rationale comment (see the main
writeup above) means the ORIGINAL bug it was protecting against is
unknown — a clean sample doesn't prove that original trigger no longer
exists, only that it didn't happen to fire in this particular 80-class
draw. The ban predates even the SPRING-TESTCOMPILER/HIB-STOREDPROC-JIT
family (commit `60ef90d4b`, 2026-07-05) and may have been guarding
against an ES- or Spring-specific JUnit runner interaction this
Hibernate-only sample wouldn't exercise at all.

**Recommendation for a future session:** treat this as positive-but-
partial evidence. Before removing the ban:
1. Run a much larger sample (or the full ~4000-class corpus) through this
   same harness.
2. Also test against a real Spring or Elasticsearch JUnit-based suite
   (different frameworks/JUnit usage patterns than Hibernate's), since
   the original trigger's nature is unknown and may be framework-specific.
3. If both come back clean, this becomes a strong case for removal —
   restoring JIT eligibility to the entire JUnit test-running harness
   across every suite this VM runs would be one of the highest-leverage
   single changes available in this whole campaign.

Raw logs (not committed, too large/verbose): `/data/tmp/hib-baseline-run.log`,
`/data/tmp/hib-allowjunit-run.log` on the Azure build host. Test list:
`/data/tmp/hib-org-junit-test-sample.txt` (regenerate via the seeded
`gen_hib_test_list.py` script for full reproducibility if needed).

## Update 2026-07-26 (later same day): three more undocumented siblings found, same origin commit, same clean result

While investigating this ban, found it is NOT alone: three neighboring
blanket bans share the exact same shape — no rationale comment, gated
Conservative-only, and (confirmed via `git log -S'<pattern>' --
vm/src/jit/skip_list.rs` run separately for each) all four patterns
trace to the SAME single commit, `60ef90d4b`:

```rust
if class_name.starts_with("com/carrotsearch/randomizedtesting/") && !package_allowed(...) { ... }
if class_name.starts_with("org/apache/logging/log4j/") && !package_allowed(...) { ... }
if class_name.starts_with("org/junit/") && !package_allowed("org/junit/", allow_packages) { ... }
if class_name.starts_with("junit/") && !package_allowed("junit/", allow_packages) { ... }
```

Given the commit's own subject ("Fix Elasticsearch postings FFM checksum
bridges") and that `randomizedtesting`/`log4j`/`junit` are all central to
Elasticsearch's own test framework, this is almost certainly one
incidental defensive group added together to keep ES's test harness
fully interpreted during that historical investigation — never
individually justified or revisited since.

**Re-ran the same 80-class Hibernate sample with all four lifted
together** (`CRATONVM_JIT_ALLOW_PACKAGES='org/junit/,junit/,org/apache/logging/log4j/,com/carrotsearch/randomizedtesting/'`):
`diff` against baseline is again byte-for-byte empty across all 80
classes. Same conclusion as the `org/junit/`-alone test: positive but
not sufficient evidence to remove, since a Hibernate-only sample can't
rule out the group's likely actual origin (Elasticsearch-specific).

**Updated recommendation:** the most direct next step is testing against
the real Elasticsearch fixture at
`/data/data/es-fixture-ivfknn-slicesdense-closure-20260717/` (2555
compiled test classes) with the same env var — this directly tests the
"these bans exist because of ES" hypothesis rather than a
Hibernate-based proxy for it.

## Update 2026-07-26 (final note on the ES-specific test attempt): inconclusive due to host contention, stopped

Two attempts to run the 4-ban-lifted config against the real 18-class ES
sample (baseline: `Tests run: 389, Failures: 93`, completed successfully)
failed to reach completion:

- First attempt: killed by a 600s timeout partway through (5/18 classes
  done).
- Second attempt (25-minute/1500s budget): progressed only ~1 minute of
  test execution per ~20 minutes of wall-clock wait, indicating severe
  host contention from other concurrent sessions on this shared Azure
  build host (this host has had multiple sessions running heavy
  Rust/JVM workloads throughout this whole investigation) rather than a
  hang, crash, or regression specific to the bans under test. Stopped
  manually after it became clear it would not complete in a reasonable
  window.

**No conclusion can be drawn from either ES attempt** — this is a
resource/scheduling limitation of the shared host at this specific time,
not evidence about the bans themselves.

**Final status: NOT removed.** The only decisive evidence remains the
Hibernate ORM sample (80/80 real classes, byte-for-byte identical results
with all 4 bans lifted, both individually and together). This is
positive and real, but per this doc's own standing caution, a
Hibernate-only sample cannot rule out an ES-specific original trigger
given the group's likely origin (commit 60ef90d4b, "Fix Elasticsearch
postings FFM checksum bridges"). The 4 bans stay in place.

**Recommendation for a future session:** retry the ES-specific comparison
when the host is less contended, or use a smaller/faster ES class subset
(3-5 fast unit-style classes rather than this 18-class mixed sample,
which includes several slow, heavily-parameterized classes like
`TextFieldMapperTests` and `FloatFieldBlockLoaderTests`) to get a
decisive answer within a shorter, more reliable window.
