# `qualfiedTableNaming` package — 2 HANGs in the 2026-07-21 "passed" rerun: one is a known-tradeoff timeout gap (NOT a regression), the other does not reproduce solo (likely host-noise)

> **CORRECTION 2026-07-31 — the "Resolved 2026-07-22" section below is FALSE as of the
> current tree; `DefaultCatalogAndSchemaTest` HANGs again, exactly as this doc's original
> (pre-"Resolved") diagnosis predicts.** The fresh full 4548-class suite run
> `apps/hib-suite-runner/runs/categorize-20260730-225515` (8 shards, binary
> `CratonVM-hib-local-0712-v3` @ `8e8a7b8cd`, `dev` merged with the real ByteBuddy fix)
> reports this exact class `HANG` / `process-died rc=124` again
> (`results.tsv` idx 70, `ms=0`). Solo repro this session confirms the process is
> genuinely, continuously CPU-bound (not stalled) for 5+ minutes straight with zero stdout
> progress past the DDL-bootstrap stage — the identical "clean but slow" signature this
> doc already established, not a new symptom.
>
> **Why:** the "Resolved 2026-07-22" section's claim — that `run-hib.sh` was updated to
> give this one class a 3600-second timeout floor and force `--nojit` — is not true of the
> `run-hib.sh` in this repo today. The current script (`apps/hib-suite-runner/run-hib.sh`)
> applies exactly one flat `TIMEOUT="${TIMEOUT:-300}"` to every class in `run_shard()`
> (`timeout "$TIMEOUT" "$CV_BIN" ...`), with no per-class table, no `--nojit` override, and
> no mention of `DefaultCatalogAndSchemaTest` anywhere in the file (verified by direct
> read and `grep`). `apps/` is wholly gitignored (`.gitignore:12: apps/`), so this script
> carries no commit history in this repo — whatever local copy implemented the
> 2026-07-22 accommodation was never durably captured anywhere tracked, and has since been
> lost (plausibly via one of `apps/`'s own documented silent-truncation / stale-backup-restore
> incidents — see the "Data-loss note" in `docs/known-issues/hibernate/README.md`, where the
> harness driver files were once recovered from a `hib-suite-runner.tar` backup dated weeks
> *before* 2026-07-22).
>
> **This is a harness/tooling regression, not a VM regression.** The underlying correctness
> fix this doc is actually about — the `MutableBigInteger` AIOOBE quarantine (`41cdfdf94`) —
> is unrelated, still intact, and not in question. Full write-up, current evidence, and the
> re-implementation recommendation: `qualfiedtablenaming-runner-timeout-floor-lost-20260731`
> (now retired to
> [`qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md`](qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md)).
>
> **FOLLOW-UP 2026-07-31 (same day, later) — the accommodation is re-implemented, and two
> claims in this doc are now known to be WRONG.** The runner override is back and durable
> (tracked `apps/hib-suite-runner/class-overrides.tsv` + `run-hib.sh`, force-added past the
> `apps/` ignore). Running the class to completion for the first time since then showed:
>
> 1. **"Clean but slow" is false on current `dev`.** The class does not pass in *either*
>    mode with *any* timeout. `--nojit` **SIGSEGVs** at ~20 min (two distinct corrupt-write
>    bugs — one fixed as `HIB-WEAKREF-RECYCLE.1`, one filed as `HIB-MAPRESIZE-STALE.1`);
>    JIT-on **OOMs** at ~41 min on a 49 %-full heap (`HIB-GCOVERHEAD-HALFFULL.1`). The
>    2026-07-21 CPU-time sampling that established "genuinely computing, not parked" was
>    correct as far as it went — it just never ran long enough to watch the class fail.
> 2. **"Force `--nojit` for this class" is INVERTED.** The "Resolved 2026-07-22" section
>    below prescribes `--nojit` as the safe mode. On current `dev` `--nojit` is the
>    *worse* mode: it segfaults, in roughly half the time the JIT lane takes to OOM. The
>    re-implemented override is therefore **timeout-only** — deliberately no `--nojit`.
>    Do not reinstate it from the section below without re-measuring.
>
> HotSpot control, identical class/classpath/`-Xmx1500m`/runner: `found=132 started=132
> ok=132 failed=0` in **119,721 ms**, clean — so the historical `started=123` was a
> CratonVM artifact, not 9 genuinely-skipped tests.

Source run: `apps/hib-suite-runner/runs/run-20260721-175909-passed/on-real/results.tsv`
(shard-3 / shard-4 `raw.log`), binary from worktree `CratonVM-hib-local-0712`
(branch `test/hib-local-0712`, merged with `origin/dev` @ `7aed580f0`), 6
shards, real-JDK, JIT on, harness default `TIMEOUT=300`.

Both classes are in
`org.hibernate.orm.test.boot.database.qualfiedTableNaming` and both are
reported `HANG` / `process-died rc=124` (the wrapper's `timeout 300` killed
the per-class forked process with zero `@@RESULT` ever printed):

| Class | idx | Status |
|---|---|---|
| `DefaultCatalogAndSchemaTest` | 9 (shard-3) | HANG, rc=124 |
| `NamespaceTest` | 9 (shard-4) | HANG, rc=124 |
| `XmlDefinedNamespaceTests` (same package, control) | 9 | PASS, `found=3 ok=3`, `ms=108458` |

## 1. `DefaultCatalogAndSchemaTest` — NOT a regression. Known, accepted timeout-vs-fix tradeoff; the AIOOBE quarantine is intact and doing exactly what it was built to do.

This exact class has an 11-session history in
[hib-misc-residuals-20260716-FIXED.md](../../internal/fixed-suite-bugs/hib-misc-residuals-20260716-FIXED.md):
a real, JIT-only, GC-amplified `ArrayIndexOutOfBoundsException` in real-JDK
`java.math.MutableBigInteger`'s divide/normalization path (`smallToString` →
`BigInteger.toString(35)` → `NamingHelper.hashedName`'s implicit
FK/constraint-name hashing, called ~132 times across this class's
`@ParameterizedClass` matrix). The saga never found the exact faulty
instruction; instead it was **quarantined** on 2026-07-18, commit
`41cdfdf94` ("fix: quarantine MutableBigInteger JIT corruption"): `java/math/MutableBigInteger`
is unconditionally kept interpreted at all three points that could otherwise
compile it —

- `vm/src/jit/skip_list.rs`'s `should_skip_jit_internal` (`SkipReason::BigIntegerArithmetic`, VM static eligibility),
- `jit/src/tiered.rs`'s `is_biginteger_arithmetic_jit_denied` (background tiered-compiler enqueue), and
- `jit/src/lib.rs`'s `try_compile` final gate (direct/manual compile paths).

**Verified still intact, unmodified, on the exact `dev` tip this run's binary
was built from** (`7aed580f0`): `git merge-base --is-ancestor 41cdfdf94
7aed580f0` succeeds, and all three functions/checks above are present
byte-for-byte in the current tree with no narrowing commits in between
(`git log 41cdfdf94..7aed580f0 -- vm/src/jit/skip_list.rs jit/src/tiered.rs
jit/src/lib.rs` shows only unrelated JIT work — MatchOps, HIB-LONGTAIL.1,
OSR-exit dedup, javac-internals bans, etc. — none of it touches the
`MutableBigInteger`/`BigIntegerArithmetic` guard). **This rules out a
regression of the fix itself.**

### Why it hangs anyway

The quarantine trades the crash for correctness at the cost of throughput:
with `MutableBigInteger` forced interpreted, this specific 132-execution
class got measurably *slower*, not faster. The same doc's own historical
measurements, all captured **after** the quarantine was verified fixing the
AIOOBE, already show whole-class times of **620,902–1,083,347 ms** (10.3–18.1
minutes) for a clean `found=132 ok=132 failed=0` run — and that is on top of
this class's independently-confirmed real ~7-20x CratonVM/HotSpot
interpreter-overhead ratio (HotSpot itself: 34.1s flat for the same class).

`apps/hib-suite-runner/run-hib.sh` applies a flat, per-class
`TIMEOUT=300` (5 minutes) via `timeout "$TIMEOUT" "$CV_BIN" ...` regardless
of class. A class whose own well-documented, already-accepted "clean"
runtime floor is 2-4x the wrapper's timeout will **always** be killed and
reported `HANG`, whether or not anything is actually wrong. This is exactly
what happened here: `--stack-dump-on-timeout`/`process-died rc=124` gives no
diagnostic signal because there's nothing stuck to catch a stack of.

### Solo verification this session

Reproduced with the exact `test/hib-local-0712` binary
(`--java-home "Eclipse Adoptium jdk-25.0.3.9-hotspot"`, `--Xmx 1500m`,
`@common.args`, `-Dcraton.batch=1`, single-class list, no extra flags):

- The process is genuinely computing, not parked: two `Get-CimInstance
  Win32_Process` CPU-time samples ~23s apart showed **17.0s → 40.0s** of
  accumulated CPU time (essentially 1 CPU-core's worth of continuous work
  across the sampled wall-clock window) while deep inside the class's normal
  session-factory/DDL bootstrap sequence (per stdout, well past the
  `MockDatabase`/H2-cleaner setup stage, running real entity-persister and
  schema-export work matching `DefaultCatalogAndSchemaTest.produceModel()`).
- This matches — rather than contradicts — the doc's own established
  "clean but slow" signature; no stall, no repeating stack, no zero-progress
  window.

### Conclusion

**Not a regression of the BigInteger AIOOBE.** The fix (`41cdfdf94`) is
present and unmodified. This is a **known, already-implicitly-accepted
tradeoff of that fix** (forced-interpreter execution of a divide-heavy,
132-execution parameterized class) colliding with the suite runner's
one-size-fits-all 300s per-class timeout, which nothing has reconciled yet.
`README.md`'s own prior entry for this class
("the 132-test class passes under both normal JIT and `--nojit`") was
written from earlier sessions' longer-timeout / solo harnesses — it was
never re-validated against this repo's *default* 300s harness timeout, which
is simply too short for this specific, already-known-slow class.

**Recommendation:** either bump this one class's effective timeout (e.g. a
per-class timeout override/allowlist in `run-hib.sh`, or rerun this class
specifically with `--timeout 1400`), or accept it as a standing `HANG`
entry in `passed.txt`'s bookkeeping with a comment explaining why. No code
change is warranted — the underlying correctness fix is doing its job.

## 2. `NamespaceTest` — does NOT reproduce solo; most likely host-contention noise, not a CratonVM defect

`NamespaceTest` is unrelated to the BigInteger saga: 83 lines, one trivial
`@Test` (`testPhysicalNameSchemaAndCatalog`), a single `Mockito.mock(Database.class)`,
no `SessionFactory`, no DDL, no divide-heavy code anywhere in its call path.
There is no plausible legitimate reason this test needs anywhere near 300s.

Raw-log analysis of the actual hang (`shard-4/raw.log`): after `@@BEGIN 9`
and the (shared, cross-class) `H2DatabaseCleaner` setup completing cleanly
(`Dropping schema objects: END` / `Committing: END` both logged), the process
prints **nothing else at all** for almost exactly 300s before the wrapper's
`timeout` kills it (`rc=124`) — no partial output, no repeating pattern, just
silence timed to the wrapper's own kill window.

**Solo reproduction this session:** ran the identical class alone (same
binary/flags/classpath as above) — completed cleanly in **9,566 ms**,
`found=1 ok=1 failed=0`, with only the same benign
`org.junit.platform.commons.JUnitException: Failed to close extension
context` `@@FAIL` line other passing classes in this same run also show
(e.g. `BatchedMultiTableDynamicStatementTests`, `XmlDefinedNamespaceTests`) —
i.e. not a new or distinguishing symptom.

**Host-load context:** at the time of this solo repro, `Get-CimInstance
Win32_Process -Filter "Name='cratonvm.exe'"` showed **5 other** concurrent
CratonVM processes on this same shared Windows host, from unrelated sessions
(an ASTParser probe, a Spring Boot OAuth2 resource-server test, a
`SingleMethodRunner` probe, etc.) — confirming genuine multi-tenant
contention was present. The original full-suite run (6 parallel shards, each
forking a fresh JVM per class) is an even heavier version of the same
contention pattern, and `DefaultCatalogAndSchemaTest`'s own `@@BEGIN 9` in
shard-3 landed within ~90 seconds of `NamespaceTest`'s `@@BEGIN 9` in
shard-4 — i.e. both hangs cluster in the same narrow wall-clock window
across shards, consistent with a shared host-load spike rather than two
independent per-class defects.

This matches the cautionary precedent in
[hib-delayedcdisupporttest-weld-bootstrap-hang-NOT-A-BUG.md](../../internal/hib-delayedcdisupporttest-weld-bootstrap-hang-NOT-A-BUG.md):
an apparent hang that, on isolated solo reproduction, turned out to be
shared-host contention rather than a genuine defect.

### Conclusion

**Not confirmed as a CratonVM bug.** One clean, fast (9.6s) solo
reproduction against zero repeats of the symptom is not proof of absence,
but combined with (a) the test's trivial nature, (b) confirmed real
concurrent host load at the time, and (c) the temporal clustering with the
unrelated, independently-explained `DefaultCatalogAndSchemaTest` hang in a
sibling shard, the balance of evidence points at host noise, not a genuine
hang. **Not written up as a bug** beyond this entry; if this reproduces
again on a quiet host (low `Get-CimInstance Win32_Process` cratonvm.exe
count, low `wmic cpu` load), that would upgrade this from "likely noise" to
"needs investigation," ideally with `--stack-dump-on-timeout 60` armed so a
live capture is possible if it recurs.

## Housekeeping

`README.md`'s existing line for
`hib-misc-residuals-20260716-FIXED.md` ("the 132-test class passes under
both normal JIT and `--nojit`") remains accurate for the *correctness*
claim (no AIOOBE) but is now cross-referenced from this doc for the
*throughput-vs-harness-timeout* caveat, since a reader could otherwise
mistake this run's `HANG` status for a correctness regression.

## Resolved 2026-07-22

> **This section is no longer true — see the correction banner at the top of this doc
> (2026-07-31). The `run-hib.sh` change described immediately below does not exist in the
> current `apps/hib-suite-runner/run-hib.sh`.** Retained verbatim as historical record of
> what was believed fixed and how; do not treat the runner behavior described here as
> current.

The runner now gives `DefaultCatalogAndSchemaTest` a finite 3600-second
class-level floor instead of applying the unsuitable five-minute suite default.
On the isolated Windows validation host the exact real-JDK run completed with
`found=132 started=123 ok=123 failed=0` in 1,804,813 ms under `--nojit`.
`NamespaceTest` also completed in 2,115 ms with `found=1 ok=1 failed=0`.

The extended JIT control exposed a separate real correctness residual rather
than a timeout: `found=132 started=122 ok=121 failed=1`, with an
`UnknownEntityTypeException`. Its stderr contained guarded field-store drops
against objects with class id 0 (`java/lang/Object`), demonstrating JIT-only
metadata corruption. The runner therefore forces `--nojit` for this one
class even when its enclosing suite lane requests JIT. This is a narrow
fail-closed quarantine, preserving JIT for every other class and preventing
false PASS/HANG accounting or silently incorrect ORM metadata.
