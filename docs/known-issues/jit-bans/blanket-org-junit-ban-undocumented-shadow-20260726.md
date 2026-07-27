# Undocumented blanket `org/junit/` ban — a new, unexplored finding (not yet investigated)

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
